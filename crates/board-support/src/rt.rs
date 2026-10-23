//! 实时性仪表的 BSP 侧：**DWT 周期计数器** + 测量包裹。
//!
//! # 分工
//!
//! - [`realtime`]（纯逻辑）：拿着周期数怎么算、怎么判、怎么报；
//! - 本模块：**去哪儿拿周期数**、以及"把测量塞进临界区/ISR"这件硬件相关的事。
//!
//! # 为什么用 DWT 而不是定时器
//!
//! Cortex-M7 的 `DWT->CYCCNT` 是核心私有的周期计数器：**读它就是一条寄存器读**，
//! 不占外设、不产生中断、不需要配置时钟树。测"关中断窗口"这种几十到几千周期的量，
//! 用 1 ms 的 PIT 去量等于用米尺量头发丝。
//!
//! # 零新增 `unsafe`
//!
//! `cortex_m::peripheral::DWT::cycle_count()` 是**安全的关联函数**（`unsafe` 在其内部），
//! 因此本模块不需要持有 DWT 句柄、也不需要裸指针读写 —— 这在 `unsafe` 预算只减不增的
//! 约束下很重要。
//!
//! # 一个刻意的取舍：**没有计数器就不记录**
//!
//! `DWT` 在 `init` 里才打开（在那之前 `cycle_counter_enabled()` 为假）。此时本模块
//! **跳过记录**，而不是记一个 0 —— 0 会被读成"很快"，而事实是"还没开始测"。
//! 这与 `realtime` 里"没有样本时余量为 `None`"是同一条纪律。
//!
//! # 测量本身的开销
//!
//! [`measured_cs`] 在临界区**内部**读两次周期计数器：这样测到的是"真正的关中断时长"
//! （不含开/关中断自身的延迟），代价是每次测量多几十个周期 —— 相对 80 µs 的预算
//! 可以忽略，而且它是**偏保守**的（把测量开销算进了被测对象）。

use core::cell::RefCell;
use core::fmt;

use cortex_m::peripheral::{DCB, DWT};
use critical_section::{CriticalSection, Mutex};
use realtime::{Measurement, RtMetrics, Span};

/// 全局实测统计。零分配、`const` 初始化，因此可以直接做 `static`。
#[unsafe(link_section = ".dtcm")]
static RT: Mutex<RefCell<RtMetrics>> = Mutex::new(RefCell::new(RtMetrics::new()));

/// 打开周期计数器（在 `init` 里调用一次）。
///
/// 先 `DCB::enable_trace()` 再 `DWT::enable_cycle_counter()`：前者打开跟踪单元的
/// 总开关（`DEMCR.TRCENA`），少了它 `CYCCNT` 不会走。两个都是安全 API。
pub fn install(dcb: &mut DCB, dwt: &mut DWT) {
    dcb.enable_trace();
    dwt.enable_cycle_counter();
    // **装完立刻回读**：把"写进去了没有"与"后来被谁清了"分开。
    // 只读一次"现在"的值无法区分这两种失败（真机上就卡在这里）。
    let (ctrl, demcr) = read_dwt_regs();
    INSTALL_CTRL.store(ctrl, core::sync::atomic::Ordering::Relaxed);
    INSTALL_DEMCR.store(demcr, core::sync::atomic::Ordering::Relaxed);
}

/// 装完即刻回读到的 `(DWT_CTRL, DEMCR)` —— 用于判断使能是否真的落地。
static INSTALL_CTRL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static INSTALL_DEMCR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 取"装完即刻回读"的值。
#[must_use]
pub fn install_readback() -> (u32, u32) {
    (
        INSTALL_CTRL.load(core::sync::atomic::Ordering::Relaxed),
        INSTALL_DEMCR.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// 分段打点：把某一阶段的 `(DWT_CTRL, DEMCR)` 记下来（用于定位"谁清了调试域"）。
pub fn probe_stage(stage: usize) {
    let (ctrl, demcr) = read_dwt_regs();
    if let Some(slot) = STAGE_CTRL.get(stage) {
        slot.store(ctrl, core::sync::atomic::Ordering::Relaxed);
    }
    if let Some(slot) = STAGE_DEMCR.get(stage) {
        slot.store(demcr, core::sync::atomic::Ordering::Relaxed);
    }
}

/// 分段回读结果（`N` 个阶段）。
pub const STAGE_COUNT: usize = 12;

/// 取某个阶段回读到的 `(DWT_CTRL, DEMCR)`。
#[must_use]
pub fn stage_readback(stage: usize) -> (u32, u32) {
    match (STAGE_CTRL.get(stage), STAGE_DEMCR.get(stage)) {
        (Some(ctrl), Some(demcr)) => (
            ctrl.load(core::sync::atomic::Ordering::Relaxed),
            demcr.load(core::sync::atomic::Ordering::Relaxed),
        ),
        _ => (0, 0),
    }
}

static STAGE_CTRL: [core::sync::atomic::AtomicU32; STAGE_COUNT] =
    [const { core::sync::atomic::AtomicU32::new(0) }; STAGE_COUNT];
static STAGE_DEMCR: [core::sync::atomic::AtomicU32; STAGE_COUNT] =
    [const { core::sync::atomic::AtomicU32::new(0) }; STAGE_COUNT];

/// 只读回读 `(DWT_CTRL, DEMCR)`。
fn read_dwt_regs() -> (u32, u32) {
    // SAFETY: 同 `dwt_state()` —— 读两张只读的架构常量地址寄存器，无副作用。
    unsafe {
        (
            core::ptr::read_volatile(DWT_CTRL_ADDR),
            core::ptr::read_volatile(DEMCR_ADDR),
        )
    }
}

/// 用 **RAII** 计一段"块"的耗时：构造时读一次周期计数器，`Drop` 时结算。
///
/// 与 [`measured`] 的分工：`measured` 适合**表达式**（`measured(Span::X, || ...)`），
/// 而"一整轮循环体"是**块** —— 用 guard 才能表达"无论从哪个出口离开这个块都会结算"。
///
/// ```ignore
/// loop {
///     let _step = board_support::rt::scope(Span::IdleStep);   // 本轮开始
///     // ... 本轮的全部工作 ...
/// }                                                          // 到这里 Drop ⇒ 结算
/// ```
///
/// 早退（`continue`）会让这次记录变成"部分轮"—— 这是 RAII 的诚实代价，不是缺陷；
/// 判据是"上限"，提前退出只会让它更小，不会掩盖真正的长轮。
pub struct Scope {
    span: Span,
    start: Option<u32>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        let Some(end) = cycles() else { return };
        if let Some(measurement) = Measurement::from_delta(end.wrapping_sub(start)) {
            critical_section::with(|cs| {
                RT.borrow(cs)
                    .borrow_mut()
                    .record_measurement(self.span, measurement);
            });
        }
    }
}

/// 开始计一段"块"的耗时（返回的 guard 在 `Drop` 时结算）。见 [`Scope`]。
#[must_use]
pub fn scope(span: Span) -> Scope {
    Scope {
        span,
        start: cycles(),
    }
}

/// **放弃所有未闭合的关中断窗口**（panic 处理器调用）。
///
/// # 为什么必须有它
///
/// `enter_irq_off`/`exit_irq_off` 是直配对的。若某段临界区里 panic，`irq_depth` 会停在
/// 非 0 —— 此后**所有**样本都会被 `exit_irq_off` 当作"内层窗口"静默丢弃，
/// 仪表看起来还在打表，其实**一条都不再记**（审计点名的缺口 F）。
/// panic 处理器先调它清深度，仪表至少不会假装还在工作。
pub fn abandon_all_irq_offs() {
    critical_section::with(|cs| RT.borrow(cs).borrow_mut().abandon_all());
}

/// 若周期计数器已停（被外部因素清掉），**重新使能**并返回 `true`；本来就正常则返回 `false`。
///
/// # 为什么需要"自动重装"
///
/// 2026-10-12 真机实测：`DEMCR.TRCENA` 与 `DWT_CTRL.CYCCNTENA` 会在
/// `memtest::pattern_test` 那一次 32 MiB 写遍+读遍期间**被清成 0**（根因未解释，
/// 见 AGENTS.md 的坑清单）。当时的处置是"`init` 末尾重新使能一次"—— 那是**一处**，
/// 而这类清除**可能在运行期再次发生**。若只靠那一次，仪表会在无人察觉的情况下停摆，
/// 并且（更糟）继续用**陈旧样本**报 `rt=ok`。
///
/// 所以把处置做成**每次心跳都检查一次**：
/// - 正常：两次寄存器读（PPB 读，代价可忽略）；
/// - 异常：写回两个使能位，并把这次重装**计数上报**（心跳里的 `dwt=`），
///   于是"运行期到底会不会再发生"变成一个**实测数字**，而不是推测。
///
/// 只读+只在异常时写使能位，不改任何别的位。
#[must_use]
pub fn rearm_if_stopped() -> bool {
    let (ctrl, demcr) = read_dwt_regs();
    let cyccntena = ctrl & 1 != 0;
    let trcena = demcr & (1 << 24) != 0;
    if cyccntena && trcena {
        return false;
    }
    // SAFETY: 只写两张架构常量地址（DWT_CTRL / DEMCR）的**使能位**：
    // DWT_CTRL.bit0 = CYCCNTENA、DEMCR.bit24 = TRCENA，其余位保持原值。
    // 这两个寄存器是内核私有寄存器，写它们不影响任何外设；本函数只在计数器**已停**
    // 时才写，属于"把本来该开的东西重新打开"，没有新的状态语义。
    unsafe {
        core::ptr::write_volatile(DWT_CTRL_ADDR.cast_mut(), ctrl | 1);
        core::ptr::write_volatile(DEMCR_ADDR.cast_mut(), demcr | (1 << 24));
    }
    true
}

/// 是否已在计时（横幅据此区分"没超预算"与"还没开始测"）。（**报事实，不报意图**）。
///
/// 返回 `(DWT_CTRL, DEMCR, CYCCNT 是否在走)`：
/// - `CTRL` 的 bit0 是 `CYCCNTENA`；
/// - `DEMCR` 的 bit24 是 `TRCENA`（跟踪总开关，少了它计数器不走）；
/// - 第三个元素：连读两次 `CYCCNT` 看它是否变化 —— 只看使能位还不够，
///   "使能着但没在走"是另一种失败（真机上就吃过"以为在测、其实全是 0"的亏）。
#[must_use]
pub fn dwt_state() -> (u32, u32, bool) {
    let ctrl = DWT::cycle_count();
    let _ = ctrl;
    // SAFETY: 读内核私有的只读调试寄存器（DWT_CTRL / DEMCR）。地址是 ARMv7-M 架构常量，
    // 读操作无副作用；cortex-m 没有暴露这两张寄存器的公开读取器（只暴露了 CYCCNT）。
    let (ctrl_reg, demcr) = read_dwt_regs();
    let first = DWT::cycle_count();
    let second = DWT::cycle_count();
    (ctrl_reg, demcr, second != first || first != 0)
}

/// `DWT_CTRL` 地址（`CYCCNTENA` 在 bit0）。
const DWT_CTRL_ADDR: *const u32 = core::ptr::null::<u32>().wrapping_add(0xE000_1000 / 4);
/// `DEMCR` 地址（`TRCENA` 在 bit24）。
const DEMCR_ADDR: *const u32 = core::ptr::null::<u32>().wrapping_add(0xE000_EDFC / 4);

/// 是否已在计时（横幅据此区分"没超预算"与"还没开始测"）。
#[must_use]
pub fn is_installed() -> bool {
    DWT::cycle_counter_enabled()
}

/// `init` 开始时的周期计数（上电窗口的起点）。
///
/// 用 `AtomicU32` 而不是静态可变：`init` 里写、`idle` 里读，两边都在单核上、顺序明确，
/// 原子量足够且不需要临界区（本模块自己就是"要不要关中断"的定义者，不该在它里面再关）。
static BOOT_START_CYCLES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 记下"上电窗口"的起点（在 `init` 最早处、`install()` 之后调用）。
pub fn mark_boot_start() {
    if let Some(cycles) = cycles() {
        BOOT_START_CYCLES.store(cycles, core::sync::atomic::Ordering::Relaxed);
    }
}

/// 结算"上电窗口"：`init` 结束 → 中断真正被使能之间的整段。
///
/// # 为什么必须有人结算它
///
/// 这段窗口由 RTIC 生成的 `pre_init`（`interrupt::disable()`）与 `post_init`
/// （`interrupt::enable()`）夹出来，**仓库源码里没有对应的 `critical_section::with`** ——
/// 所以 `measured_cs` 与 `check-critical` 都看不见它，而它恰恰是全系统最长的关中断窗口
/// （2026-10-12 实测 ≈3.25 s；2026-10-19 把 SDRAM 自检搬到 `idle` 后实测 ≈8 ms）。
///
/// 在 `idle` 的第一句调用它：那一刻中断已经使能（`post_init` 已跑完），
/// 于是 `now - BOOT_START_CYCLES` 就是"上电到中断使能"的真实长度。
pub fn finish_boot_span() {
    let start = BOOT_START_CYCLES.load(core::sync::atomic::Ordering::Relaxed);
    let Some(end) = cycles() else {
        return;
    };
    if start == 0 {
        // 没标过起点（例如主机测试或 DWT 没开）⇒ **不记录**，而不是记一个假的 0。
        return;
    }
    // 与 `measured_cs` 同一取舍：0 周期 = 没测到，**不记**（`Cycles` 在类型上就禁止了）。
    if let Some(measurement) = Measurement::from_delta(end.wrapping_sub(start)) {
        critical_section::with(|cs| {
            RT.borrow(cs)
                .borrow_mut()
                .record_measurement(Span::BootInit, measurement);
        });
    }
}

/// 读一次周期计数器；计数器没开时返回 `None`。
#[inline]
fn cycles() -> Option<u32> {
    if DWT::cycle_counter_enabled() {
        Some(DWT::cycle_count())
    } else {
        None
    }
}

/// 在**关中断**下执行 `f`，并测量这段关中断窗口（只记最外层）。
///
/// 这是本项目**唯一**允许进入临界区的入口：`check-critical` 门禁会拒绝其它文件里
/// 出现的裸 `critical_section::with` —— 因为"没被测量的临界区"就是"没人知道有多长的
/// 关中断窗口"，而本项目已经在这上面栽过一次（终端 `fs` 命令曾在关中断里做全盘扫描）。
pub fn measured_cs<R>(f: impl FnOnce(CriticalSection<'_>) -> R) -> R {
    critical_section::with(|cs| {
        let start = cycles();
        // 进入/退出只维护嵌套深度：内层窗口不单独记录（否则最长时长会被内层小窗口顶掉）。
        RT.borrow(cs).borrow_mut().enter_irq_off();
        let out = f(cs);
        // 两个条件都要：计数器可用 **且** 这次窗口真的走了至少 1 个周期。
        // 后者用 `Cycles::new` 表达 —— 零周期不是一次测量，直接跳过（类型上也没法记）。
        if let (Some(start), Some(end)) = (start, cycles()) {
            match Measurement::from_delta(end.wrapping_sub(start)) {
                Some(Measurement::Valid(elapsed)) => {
                    RT.borrow(cs).borrow_mut().exit_irq_off(elapsed);
                }
                // 疑似跨越 CYCCNT 回绕：真实长度不可知，但**一定极长** ——
                // 记成"疑似回绕"（并把 max 抬到下界）而不是让 wrapping_sub 的小值
                // 把它伪装成"很快"。
                Some(Measurement::SuspectedWrap) => {
                    RT.borrow(cs).borrow_mut().exit_irq_off_wrapped();
                }
                None => RT.borrow(cs).borrow_mut().abandon_irq_off(),
            }
        } else {
            // 没计数器：**只把嵌套深度退回去，绝不记录样本**。
            // ⚠️ 2026-10-12 真机修正：原实现这里写的是 `exit_irq_off(0)`，那会记下一条
            // "0 周期"的样本 —— 于是 `count > 0` 而 `max == 0`，"这项没被观测"被记成
            // "这项快得测不出来"，而且 `rt=` 还会据此报 `ok`。与 `realtime` 里
            // "没有样本时余量为 `None`"、报告里没样本打 `-` 是同一条纪律。
            RT.borrow(cs).borrow_mut().abandon_irq_off();
        }
        out
    })
}

/// 测量一段**不改变中断状态**的代码（ISR 主体、idle 单步等）。
///
/// 与 [`measured_cs`] 的区别：这里进出各读一次计数器（各在一个极短的临界区里），
/// 中间执行 `f` 时中断状态不受影响。
pub fn measured<R>(span: Span, f: impl FnOnce() -> R) -> R {
    let start = critical_section::with(|_cs| cycles());
    let out = f();
    if let (Some(start), Some(end)) = (start, critical_section::with(|_cs| cycles())) {
        // 与 `measured_cs` 同一取舍：0 周期 = 没测到，跳过而不是记成"极快"。
        if let Some(measurement) = Measurement::from_delta(end.wrapping_sub(start)) {
            critical_section::with(|cs| {
                RT.borrow(cs)
                    .borrow_mut()
                    .record_measurement(span, measurement);
            });
        }
    }
    out
}

/// 取一份统计快照（`RtMetrics` 是 `Copy`，取走即可，不持有借用）。
#[must_use]
pub fn snapshot() -> RtMetrics {
    critical_section::with(|cs| *RT.borrow(cs).borrow())
}

/// 只取**一个 span** 的统计副本。
///
/// 启动报告只需要 `boot-init` 一项；用 [`snapshot`] 会把整个 `RtMetrics`（含
/// 直方图，约 1.5 KiB）拷到栈上。2026-10-19 实测这会把栈余量从 33% 压到 31% ——
/// 接近 30% 硬下限。单项入口只拷一个 `SpanStats`，不制造这个峰值。
#[must_use]
pub fn stats(span: Span) -> realtime::SpanStats {
    critical_section::with(|cs| RT.borrow(cs).borrow().stats(span))
}

/// 清空统计（保留预算）。用于"复位后重新观察一段"。
pub fn reset() {
    critical_section::with(|cs| RT.borrow(cs).borrow_mut().reset());
}

/// 全部 span 是否都在预算内（廉价判据，供横幅/门禁用一句结论）。
#[must_use]
pub fn all_within_budget() -> bool {
    snapshot().all_within_budget()
}

/// 把报告写进任意输出（固件传控制台，终端也可以调）。
pub fn report(out: &mut dyn fmt::Write) -> fmt::Result {
    snapshot().report(out)
}
