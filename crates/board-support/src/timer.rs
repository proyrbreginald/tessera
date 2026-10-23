//! 单调时基：PIT0+PIT1 链成 64 位自由计数器，PIT2 出 1 ms 心跳。
//!
//! # 为什么用 PIT 而不是 GPT
//!
//! 需要同时满足两件事：**64 位**单调计数与**1 ms 中断**。
//!
//! - 32 位计数器在 24 MHz 下每 179 秒回绕一次。虽然本项目的比较逻辑是回绕安全的
//!   （见 `primitives::time`），但设备要求连续运行 ≥30 天，让绝对时间受 179 秒
//!   周期约束会给诊断带来不必要的麻烦。
//! - PIT 有 4 个通道：`Chan0`+`Chan1` 链成 64 位自由计数器，`Chan2` 独立做 1 ms 心跳。
//!   同一份硬件同时给出"长时基"与"周期中断"。
//!
//! # ⚠️ 硬约束：`hal::pit::Pit::new()` 只允许在 [`init`] 里出现一次
//!
//! `imxrt-hal` 的 `Pit::new()` 会把**四个通道的 `TCTRL` 全部清零**
//! （源码注释原文："Reset all PIT channels … Set them to a known, good state"）。
//!
//! 2026-10-09 真机实测踩到的坑：早期版本为保持"零大小句柄、每次读寄存器"的写法，
//! 在 `PitClock::ticks()`（**读时间**）里也调用 `Pit::new()` —— 于是**每次读时间都把
//! 定时器自己关掉**。表现（逐条实测）：
//!
//! - `TCTRL0/TCTRL1/TCTRL2` 恒为 0：调试器写进去的使能位 0.5 秒后又被清回 0；
//! - `CVAL0` 停在 `LDVAL0` 一动不动，`LTMR64H/L` 不推进；
//! - 时间恒定 → 每秒心跳的条件永不成立 → 串口**只有横幅、之后永久静默**；
//! - 1 ms 中断从未产生 → `deadline_tick` 任务与健康计数全是死的。
//!
//! 因此：读路径一律**只用 RAL 读寄存器**，绝不再构造 `Pit`。
//!
//! # 第二个同源缺陷：链式要链的是 `Chan1`
//!
//! `enable_chaining` 对 `Chan0` 直接返回 `Err(CannotChainError)`
//! （链语义是"**该**通道在前一个通道到期时减一"，所以低位不能成链）。
//! 早期代码链了 `Chan0` 并把返回值丢掉 → **64 位链从未建立**，且没有任何报错。
//! 现在改为链 `Chan1`、两个通道都使能，并**不吞返回值**。
//!
//! # 为什么不把时基对象交给服务层持有
//!
//! [`PitClock`] 是零大小类型，每次 `now()` 直接读寄存器：时基的**所有权**留在中断
//! 任务侧（它需要清中断标志），服务层只需一个 `Clock` 实现。

use core::sync::atomic::{AtomicU32, Ordering};
use imxrt_hal as hal;
use imxrt_ral as ral;
use ports::clock::Clock;
use primitives::time::Timepoint;

use crate::config;

/// 本次 boot 的 64 位时基原点（PIT 原始刻度）。
///
/// # 为什么需要它（2026-10-19 真机实测）
///
/// `Pit::new()` 只把各通道 `TCTRL` 清零，**不保证**把 64 位链的 `CVAL`/高字复位到
/// 上电初值。调试器软复位（`SRC_SRSR=0x40`）后 PIT 继续计数，于是 `init()` 重新
/// 配置通道之后，`LTMR64H/L` 读到的仍是从"上一次上电"起算的刻度 —— 真机表现为
/// **每次 reset 后 `uptime` 都从 `00:03:02.318` 开始**（连续 3 次可重复），
/// 而不是从 0 开始。跨暖复位的崩溃记录/黑匣子正好依赖这个原点。
///
/// 修法不依赖"猜 HAL/器件会不会复位 CVAL"：在 `init()` 末尾把**当前原始刻度**
/// 记下来，`now()` 用 `raw - origin`。这样无论硬件是否复位链，固件视角的 uptime
/// 都从本次 boot 的 0 开始。
///
/// 用两个 `AtomicU32` 而不是 `AtomicU64`：`thumbv7em-none-eabihf` 没有 64 位
/// 原子指令（`rustc --print cfg` 只有 8/16/32/ptr），`AtomicU64` 在该目标上不可用。
static BOOT_TICKS_HI: AtomicU32 = AtomicU32::new(0);
static BOOT_TICKS_LO: AtomicU32 = AtomicU32::new(0);

/// 把当前原始刻度记为"本次 boot 的原点"。
fn set_boot_origin() {
    let raw = PitClock.ticks();
    let hi = u32::try_from(raw >> 32).unwrap_or(0);
    let lo = u32::try_from(raw & 0xFFFF_FFFF).unwrap_or(0);
    BOOT_TICKS_HI.store(hi, Ordering::Release);
    BOOT_TICKS_LO.store(lo, Ordering::Release);
}

/// 本次 boot 的原点（原始刻度）。
#[must_use]
pub fn boot_origin_ticks() -> u64 {
    let hi = BOOT_TICKS_HI.load(Ordering::Acquire);
    let lo = BOOT_TICKS_LO.load(Ordering::Acquire);
    (u64::from(hi) << 32) | u64::from(lo)
}

/// 取 PIT 句柄 —— **本模块唯一的硬件入口**。
///
/// 收敛成一处之后，`unsafe` 的**理由只需要论证一次**（这也是 unsafe.budget 的意图：
/// 让"新增加一处硬件别名"成为必须解释的动作）。风险本身不在"拿到句柄"，而在
/// "构造 `hal::pit::Pit`" —— `Pit::new()` 会复位所有通道，因此它在本模块只允许
/// 出现在 [`init`] 里一次。
fn pit() -> ral::pit::PIT {
    // SAFETY: `instance()` 返回指向固定寄存器基址的句柄；PIT 是单实例外设，
    // 本模块不再构造持有所有权的 `Pit`，只读写寄存器与幂等标志位。
    unsafe { ral::pit::PIT::instance() }
}

/// 初始化时基：PERCLK 取晶振、分频 1；`Chan0/Chan1` 链成 64 位；`Chan2` 装载 1 ms。
///
/// `Chan2` 在这里**只装载、不使能计数**：使能交给 [`PitClock::start_tick`]，
/// 由空闲任务在 RTIC 资源就绪之后调用。中断线一旦使能，ISR 就可能在任何时刻触发；
/// 若它早于 RTIC 把任务局部资源放到位，处理函数会访问未初始化的 `cx.local`。
/// 这类窗口极短、只在特定时刻复现，因此用"先装载、后使能"从结构上消除。
///
/// # Safety
///
/// 全程只调用一次，且此后 PIT 的配置不再被其它代码修改。
pub unsafe fn init() {
    // SAFETY: 见函数的 `# Safety` 段：CCM 的独占性由调用方（`board::init`）保证。
    let mut ccm = unsafe { ral::ccm::CCM::instance() };
    hal::ccm::clock_gate::pit().set(&mut ccm, hal::ccm::clock_gate::ON);

    // PERCLK 取 24 MHz 晶振：与晶振同源，因此时基的准确性等价于晶振的准确性，
    // 这正是"用 uptime 检验时钟树"这条判据成立的原因。
    hal::ccm::perclk_clk::set_selection(&mut ccm, hal::ccm::perclk_clk::Selection::Oscillator);
    hal::ccm::perclk_clk::set_divider(&mut ccm, 1);

    // 本模块唯一一次构造 `Pit`（它会复位所有通道，见模块文档）。
    // SAFETY: 全项目只在这里构造 `Pit`，且由 `board::init` 保证只调用一次。
    let mut pit = hal::pit::Pit::new(unsafe { ral::pit::PIT::instance() });

    // --- 64 位自由计数器：Chan0 计数、Chan1 链接到 Chan0 ---
    pit.set_load_timer_value(hal::pit::Channel::Chan0, u32::MAX);
    pit.set_load_timer_value(hal::pit::Channel::Chan1, u32::MAX);
    // 必须链 **Chan1**（低位 Chan0 会被 HAL 拒绝）。不吞返回值。
    // 链式失败**不允许静默**：退化到 32 位后每 179 秒回绕一次，安全截止判定会失真，
    // 而这种失真在正常运行中几乎不可见。快速失败比带病运行好。
    assert!(
        pit.enable_chaining(hal::pit::Channel::Chan1).is_ok(),
        "PIT Chan1 无法链接到 Chan0：64 位时基不成立，拒绝以降级时基运行"
    );
    pit.enable(hal::pit::Channel::Chan0);
    pit.enable(hal::pit::Channel::Chan1);

    // **把当前原始刻度记为本次 boot 的原点**（见 `BOOT_TICKS_HI` 的说明）。
    // 必须在链使能之后、任何 `now()` 之前：`board::init` 紧接着就会用 `now()` 记阶段耗时。
    set_boot_origin();

    // --- 1 ms 心跳：只装载 + 开中断使能，计数使能留给 start_tick ---
    pit.set_load_timer_value(hal::pit::Channel::Chan2, config::TICK_TICKS);
    pit.set_interrupt_enable(hal::pit::Channel::Chan2, true);
}

/// 零大小的时基句柄（只读寄存器，不持有 `Pit`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PitClock;

impl PitClock {
    /// 原始计数值（24 MHz 刻度），由 `LTMR64H/LTMR64L` 组成 64 位。
    ///
    /// # 为什么要重读一次高 32 位
    ///
    /// 读 `LTMR64H` 与 `LTMR64L` 不是原子操作：两次读之间低位可能正好回绕，
    /// 于是"高 32 位"就过期了。RM 给出的做法是先读 H、再读 L，若 L 恰好等于
    /// 通道 0 的装载值（说明刚到过回绕边界）就整体重读一次。
    #[must_use]
    pub fn ticks(&self) -> u64 {
        // 特意**不用** `hal::pit::Pit::new`：它会复位所有通道（见模块文档）。
        let pit = pit();
        let mut high = ral::read_reg!(ral::pit, pit, LTMR64H);
        let mut low = ral::read_reg!(ral::pit, pit, LTMR64L);
        let load0 = ral::read_reg!(ral::pit::timer, &pit.TIMER[0], LDVAL);
        if low == load0 {
            high = ral::read_reg!(ral::pit, pit, LTMR64H);
            low = ral::read_reg!(ral::pit, pit, LTMR64L);
        }
        let lifetime = (u64::from(high) << 32) | u64::from(low);
        // ⚠️ `LTMR64H/L` 是**递减**计数器：链式通道装着 0xFFFF_FFFE 各自向下数。
        // 直接把它当"已过去的刻度"会得到**递减**的时间（并让 `elapsed_since`
        // 的回绕相减恒为巨值）。因此这里翻成递增：已过刻度 = 起始值 - 当前值。
        // 2026-10-09 真机实测：未翻转时 `uptime` 每秒**减少** 1 µs 且恒定在
        // 768614.336s（= 2^64/24MHz），每秒心跳只是"相等→减一"跳变的副产物。
        u64::MAX.wrapping_sub(lifetime)
    }

    /// 固件视角下的时基配置（用于把横幅从"声称"变成"证据"）：
    /// `(LDVAL0, TCTRL0, LDVAL1, TCTRL1, LDVAL2, TCTRL2)`。
    #[must_use]
    pub fn config_snapshot(&self) -> [u32; 6] {
        let pit = pit();
        [
            ral::read_reg!(ral::pit::timer, &pit.TIMER[0], LDVAL),
            ral::read_reg!(ral::pit::timer, &pit.TIMER[0], TCTRL),
            ral::read_reg!(ral::pit::timer, &pit.TIMER[1], LDVAL),
            ral::read_reg!(ral::pit::timer, &pit.TIMER[1], TCTRL),
            ral::read_reg!(ral::pit::timer, &pit.TIMER[2], LDVAL),
            ral::read_reg!(ral::pit::timer, &pit.TIMER[2], TCTRL),
        ]
    }

    /// 从**本次 boot** 起算的原始刻度（已减去 `init()` 记录的原点）。
    ///
    /// 与 [`Self::ticks`] 的区别：`ticks()` 是"PIT 链从上次硬件清零起算"的原始值，
    /// 暖复位后它可能不是 0；本函数保证固件视角从 0 开始。
    #[must_use]
    pub fn ticks_since_boot(&self) -> u64 {
        self.ticks().saturating_sub(boot_origin_ticks())
    }

    /// 启动 1 ms 心跳中断。
    ///
    /// **必须在 RTIC 的任务资源就绪之后调用**（当前由空闲任务的第一条语句调用）。
    pub fn start_tick(&self) {
        // 同样刻意不用 `Pit::new`（那会清掉 Chan0/Chan1 的自由计数）。
        let pit = pit();
        ral::modify_reg!(ral::pit::timer, &pit.TIMER[2], TCTRL, TEN: 1);
    }
}

impl Clock for PitClock {
    fn now(&self) -> Timepoint {
        // 刻度 → 微秒：除以**每微秒的刻度数**（24），不是除以 TIMEBASE_HZ。
        // 写成 `ticks / TIMEBASE_HZ` 会把 1 秒折算成 1 微秒 —— 时间慢 100 万倍。
        //
        // 用 `ticks_since_boot()` 而不是 `ticks()`：暖复位后 PIT 链可能没有清零，
        // 直接用原始值会让 uptime 从上次运行的时间开始（2026-10-19 真机实测）。
        Timepoint::from_micros(self.ticks_since_boot() / u64::from(config::TICKS_PER_MICRO))
    }
}

/// 中断任务持有的心跳句柄：只负责清标志。
///
/// 零大小 —— 不再持有 `Pit`，因为构造 `Pit` 需要 `Pit::new`，那会复位所有通道
/// （早期版本正是在这里第二次复位了 `Chan0/Chan1`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TickSource;

impl TickSource {
    /// 取得心跳句柄。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 中断标志当前是否置位（写清之前读，用于区分"真到期"与"伪进入"）。
    #[must_use]
    pub fn is_elapsed(&self) -> bool {
        let pit = pit();
        ral::read_reg!(ral::pit::timer, &pit.TIMER[2], TFLG, TIF == 1)
    }

    /// 清中断标志。**必须是中断处理的第一件事**，否则会立刻重入。
    pub fn clear(&self) {
        let pit = pit();
        ral::write_reg!(ral::pit::timer, &pit.TIMER[2], TFLG, TIF: 1);
    }
}
