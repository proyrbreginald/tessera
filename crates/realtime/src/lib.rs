//! `realtime` —— 实时性的**可测量底座**（纯逻辑，主机可测）。
//!
//! # 为什么要这个 crate
//!
//! 这个项目的实时性论证原本只活在**纸面**上：[`primitives::budget`] 里写着
//! "各 ISR 最坏执行时间 20 µs / 15 µs / 10 µs"，同一行还老实标着
//! **🟡 预算值，需 DWT 实测替换** —— 也就是说，那些数字**从来没有被测过**。
//!
//! 纸面预算最危险的失效方式不是"算错了"，而是**没人知道它已经错了**：
//! 有人在某条路径里加了一次全盘扫描（本项目真发生过：终端 `fs` 命令第一版在关中断里
//! 调 `stats()`，主机实测要读 28.3 MB 介质、真机折算 0.5–2 s），
//! 而构建、测试、门禁**全都照样通过**。
//!
//! 所以这里做三件事（**平台需求 RT-1 / RT-2 / TS-5**）：
//!
//! 1. **span（区间）注册表**：每个要被观测的区间有名字、有预算、有一个稳定下标；
//! 2. **统计容器**：[`SpanStats`] 记次数/上次/最大/累计/超预算次数，[`RtMetrics`] 收全部 span，
//!    并提供**机器判据** [`RtMetrics::check`]（超预算即 `Err`）与**人类可读报告**；
//! 3. **嵌套语义**：关中断是可以嵌套的（`with_fs` 里再进 `console` 的入队临界区），
//!    必须**只记最外层**，否则同一个窗口会被重复计数、把最长时长算错。
//!
//! # 与硬约束的关系是**编译期**钉住的
//!
//! 最紧的物理约束是"115200 8N1 的串行总线每字节约 86.8 µs"：关中断超过它就可能吃掉一个字节。
//! [`BUDGET_IRQ_OFF_MICROS`] 因此取 80 µs（留余量），并用 `const_assert!` 把
//! "预算 < 字节间隔"这条关系钉在编译期 —— 谁改坏了，构建就失败。
//!
//! # 这个 crate 不碰硬件
//!
//! 它**一个寄存器都不读**：谁去读 DWT 周期计数器、什么时候读，都在 `board-support`。
//! 这里只负责"拿到一串周期数之后怎么算、怎么判、怎么报"—— 而那恰好是最容易算错的部分
//! （历史上栈涂抹就是在这种纯算术里漏了一个字，见 `memory-layout`）。
//!
//! # 口径提醒（别把统计读成它没有的含义）
//!
//! - 单位是 **CPU 周期**（`CYCCNT`），换算成时间用 [`primitives::budget::CPU_HZ`]；
//! - **测量本身有开销**（进出各读写一次周期计数器），因此报告里的数字比"真值"略大一点点；
//! - `max` 是**自复位以来**的最大值，不是"最近一次"。要看当前状态就复位后再跑（[`RtMetrics::reset`]）。

#![no_std]
// 纯算术：本 crate 存在的意义之一就是能被主机上的门禁与测试反复跑。
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_arithmetic,
    clippy::wildcard_imports
)]
// 测试里放宽：断言失败就该直接 panic。
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )
)]

use core::fmt;
use core::num::NonZeroU32;

use primitives::budget;
use static_assertions::const_assert;

/// 关中断时长的预算（微秒）。
///
/// **依据**：串行总线在 115200 8N1 下每字节约 86.8 µs（见
/// [`budget::SERIAL_BYTE_INTERVAL_NANOS`]）。关中断超过这个间隔就可能吃掉一个到达的字节，
/// 因此预算必须**小于**它；这里取 80 µs，留 ~8% 余量（还要算上进中断本身的延迟）。
///
/// 这条关系不是注释，是断言 —— 见下方 `const_assert!`。
pub const BUDGET_IRQ_OFF_MICROS: u64 = 80;

/// 1 ms 时基中断（`PIT`）的预算（微秒）。取自 [`budget::ISR_DEADLINE_TICK_MICROS`]。
///
/// 注意：它必须显著小于 [`budget::TICK_PERIOD_MICROS`]，否则中断会挤在一起。
pub const BUDGET_ISR_DEADLINE_TICK_MICROS: u64 = budget::ISR_DEADLINE_TICK_MICROS;

/// 控制台接收中断（`LPUART1`）的预算（微秒）。
///
/// **比别的 ISR 更紧的理由**：这块 LPUART 的接收 FIFO **只有 1 字节深**，
/// 中断本身必须在下一个字节到达（≈86.8 µs）之前把 FIFO 里的字节搬走。
pub const BUDGET_ISR_CONSOLE_RX_MICROS: u64 = 20;

/// 一轮终端输入处理的预算（微秒）。
///
/// 它不是硬实时约束（终端在最低优先级），但**是软实时的总闸门**：一轮处理超过一个
/// 时基周期，就说明有命令在做重活（扫介质、长循环、大块格式化），该拆就拆。
pub const BUDGET_SHELL_INPUT_MICROS: u64 = budget::TICK_PERIOD_MICROS;

/// 上电 → 中断使能之间的预算（微秒）。
///
/// **刻意定得很宽（50 ms）**：2026-10-12 实测这一段约 **3.25 s**（SDRAM 全片自检 3.0 s +
/// 250 ms 忙等）。2026-10-19 把 SDRAM 自检搬到 `idle` 之后，真机实测已降到 **≈8 ms**
/// （见 `rt` 的 `boot-init` 行）。预算暂时保持 50 ms：它是"搬完长活之后应该达到的量级"，
/// 留出余量比贴着 8 ms 收紧更安全。
pub const BUDGET_BOOT_INIT_MICROS: u64 = 50_000;

/// 一整轮 `idle` 的预算（微秒）。
///
/// 一个时基周期（1 ms）：超了说明软实时的推进节拍会抖。它不是硬实时约束
/// （ISR 仍可抢占），但它是"上报与心跳是否还能按节拍走"的闸门。
pub const BUDGET_IDLE_STEP_MICROS: u64 = budget::TICK_PERIOD_MICROS;

/// SDRAM 上电自检的预算（微秒）—— 4 s。
///
/// 为什么给得这么宽：它量的是"32 MiB 写遍+读遍+保持检查"的体检耗时（真机实测约 3.3 s），
/// 不是在追求实时性。**真正的约束不是它多快，而是它不在关中断里** —— 所以它同时被
/// 归入"启动期"（`is_startup()`），不参与运行期预算判定。
pub const BUDGET_BOOT_POST_MICROS: u64 = 4_000_000;

/// 存储访问临界区的预算（微秒）。
///
/// 与 [`BUDGET_IRQ_OFF_MICROS`] 同值不同义：它单独统计"取全局文件系统实例"这条路径，
/// 便于把"存储访问"从"所有关中断"里拆出来看（历史上正是这条路径出过 0.5–2 s 的窗口）。
pub const BUDGET_STORAGE_CRITICAL_MICROS: u64 = BUDGET_IRQ_OFF_MICROS;

// 关中断预算必须**严格小于**一个串行总线字节间隔 —— 这是本 crate 存在的最硬理由。
const_assert!(BUDGET_IRQ_OFF_MICROS * 1_000 < budget::SERIAL_BYTE_INTERVAL_NANOS);
// 时基 ISR 的预算必须远小于时基周期，否则中断会自我堆叠。
const_assert!(BUDGET_ISR_DEADLINE_TICK_MICROS * 2 < budget::TICK_PERIOD_MICROS);

/// 被观测的区间。
///
/// # 顺序即契约
///
/// 枚举顺序与 [`SPANS`] 的表顺序、[`RtMetrics`] 内部的槽位顺序**必须一致**；
/// 已有变体不得插入中间（会静默改变既有读数的含义），新增只能追加在末尾。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    /// 关中断区间（只记最外层，见 [`RtMetrics::irq_off_enter`]）。
    IrqOff,
    /// `PIT` 时基中断服务程序（1 ms 节拍）。
    IsrDeadlineTick,
    /// 控制台接收中断服务程序（`LPUART1`）。
    IsrConsoleRx,
    /// 空闲任务里处理一轮终端输入（**命令分发就在这一段里**）。
    ///
    /// 为什么单独测它：本项目真发生过"一条命令在关中断里做全盘扫描"
    /// （终端 `fs` 命令第一版，实测折算真机 0.5–2 s）。关中断那部分由
    /// [`Span::IrqOff`] / [`Span::StorageCritical`] 抓，而"命令本身把空闲任务占住多久"
    /// 由这一项抓 —— 两者合起来才能定位"是命令慢，还是关中断长"。
    ShellInput,
    /// 存储访问临界区（取全局文件系统实例那条路径）。
    StorageCritical,
    /// **SDRAM 上电自检**（全片写遍 + 读遍 + 250 ms 保持检查）。
    ///
    /// # 为什么它有自己的预算，而且属于"启动期"
    ///
    /// 自检要跑遍 32 MiB（真机约 3.3 s）—— 它**必须**在一处允许长时间运行的地方做。
    /// 原先它在 `#[init]` 里：RTIC 用 `interrupt::disable()` 把整个 `init` 括住，
    /// 于是这 3.3 s 变成**全系统最长的关中断窗口**（详见 [`Span::BootInit`] 的说明）。
    /// 现在它搬到 `idle` 的**循环之前** —— 那里中断已经使能，这段长活变成
    /// **可被中断打断**，关中断窗口从秒级回到毫秒级。
    ///
    /// 预算给得宽（4 s）：它是"内存带宽+器件正确性"的体检，不是在抢实时性；
    /// 真正的约束是"它**不在关中断里**"，那一条由它出现在 `idle` 而不是 `init` 保证。
    BootPost,
    /// **空闲任务的一整轮**（从循环顶部到本轮结束）。
    ///
    /// # 为什么它与 [`Span::ShellInput`] 都要留
    ///
    /// `ShellInput` 只覆盖"处理终端输入"那一段（命令分发在里面），而这一项覆盖整轮：
    /// 终端输入 + 健康聚合 + 服务推进 + 到点上报。两者的差值就是"除了终端之外，
    /// 一轮 idle 还要花多久" —— 少了任何一项都回答不了"某条命令把空闲任务占住多久"
    /// 与"一轮本身有多长"这两个不同的问题（2026-10-12 审计指出：我一度把
    /// `IdleStep` 换成了 `ShellInput`，结果整轮指标丢了）。
    ///
    /// # 为什么它用 RAII 而不是闭包
    ///
    /// "一整轮"是**块的边界**，不是一个表达式。用 guard（构造时取一次计数、`Drop` 时结算）
    /// 才能表达"无论从哪个出口离开这个块都会结算"。当前 idle 循环**没有提前出口**
    /// （`continue`/`break`/`return` 都没有），因此读数是精确的；将来若有人加了提前出口，
    /// 那次记录会变成"部分轮"—— 这是 RAII 语义的诚实代价，已在此写明。
    IdleStep,
    /// **上电到中断使能之间的整段**（RTIC 用 `interrupt::disable()` 把整个 `#[init]` 括住）。
    ///
    /// # 为什么必须有这一项
    ///
    /// 2026-10-12 的实时性审计算出一件很不好看的事：**全系统最长的关中断窗口就是这一段**
    /// （2026-10-12 实测 SDRAM 全片自检 3.0 s + 250 ms 保持性忙等 ≈ 3.25 s），而它**恰好不被
    /// 任何仪表覆盖** —— 因为它不由 `critical_section::with` 造成，是 RTIC 生成代码造的。
    /// 这一项专门把它量出来。2026-10-19 把 SDRAM 自检搬到 `idle` 后，真机实测已降到 **≈8 ms**
    /// （`rt` 的 `boot-init` 行），当前在 50 ms 预算内；若再次搬入长活，它会重新变红。
    BootInit,
}

impl Span {
    /// 全部 span，顺序与枚举声明一致。
    pub const ALL: &'static [Span] = &[
        Span::IrqOff,
        Span::IsrDeadlineTick,
        Span::IsrConsoleRx,
        Span::ShellInput,
        Span::StorageCritical,
        Span::BootInit,
        Span::IdleStep,
        Span::BootPost,
    ];

    /// 槽位下标。
    ///
    /// 显式 `match` 而不是 `as usize`：crate 禁止 `as` 转换，而且显式映射能在
    /// 改动枚举时**编译失败**（漏一个分支编译器会拦），比"默默错位"安全得多。
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Span::IrqOff => 0,
            Span::IsrDeadlineTick => 1,
            Span::IsrConsoleRx => 2,
            Span::ShellInput => 3,
            Span::StorageCritical => 4,
            Span::BootInit => 5,
            Span::IdleStep => 6,
            Span::BootPost => 7,
        }
    }

    /// 人类可读名字（出现在报告与终端里，改动等于改对外口径）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Span::IrqOff => "irq-off",
            Span::IsrDeadlineTick => "isr-pit",
            Span::IsrConsoleRx => "isr-lpuart1",
            Span::ShellInput => "shell-input",
            Span::StorageCritical => "storage-crit",
            Span::BootInit => "boot-init",
            Span::IdleStep => "idle-step",
            Span::BootPost => "boot-post",
        }
    }

    /// 预算（微秒）。`0` 表示**不设预算**（只观测、不判定）。
    #[must_use]
    pub const fn budget_micros(self) -> u64 {
        match self {
            Span::IrqOff => BUDGET_IRQ_OFF_MICROS,
            Span::IsrDeadlineTick => BUDGET_ISR_DEADLINE_TICK_MICROS,
            Span::IsrConsoleRx => BUDGET_ISR_CONSOLE_RX_MICROS,
            Span::ShellInput => BUDGET_SHELL_INPUT_MICROS,
            Span::StorageCritical => BUDGET_STORAGE_CRITICAL_MICROS,
            Span::BootInit => BUDGET_BOOT_INIT_MICROS,
            Span::IdleStep => BUDGET_IDLE_STEP_MICROS,
            Span::BootPost => BUDGET_BOOT_POST_MICROS,
        }
    }

    /// 这一项是否属于**启动期**（而不是运行期）。
    ///
    /// 启动期与运行期的判据必须分开，理由不是"启动期可以慢"，而是**不能让一条已知会红的
    /// 判据污染运行期结论**：审计针对这类设计明确警告过 ——
    /// "否则门禁一上线就是红的，然后大家就会开始无视它"。
    /// 所以 [`crate::RtMetrics::check`] 只看运行期，启动期用
    /// [`crate::RtMetrics::startup_violations`] 单独报，并一直显示在表里。
    #[must_use]
    pub const fn is_startup(self) -> bool {
        matches!(self, Span::BootInit | Span::BootPost)
    }

    /// 预算（CPU 周期）。`0` 表示不设预算。
    #[must_use]
    pub const fn budget_cycles(self) -> u64 {
        let micros = self.budget_micros();
        if micros == 0 {
            0
        } else {
            budget::cycles_for_micros(micros)
        }
    }
}

/// [`Span`] 的数量（编译期常量，用于静态数组）。
pub const SPAN_COUNT: usize = 8;

/// 一次测量的周期数 —— **保证非零**。
///
/// # 为什么值得一个 newtype（不是洁癖，是硬件教出来的）
///
/// 2026-10-12 真机上踩到：DWT 没在计时，于是每次测量都拿到"0 周期"，而统计照样把它记下来 ——
/// 结果 `count > 0` 而 `max == 0`，"这项**没被观测**"被显示成"这项**快得测不出来**"，
/// 心跳还据此报 `rt=ok`。
///
/// 根因是"0 周期"这个值同时承载了两种完全不同的含义。修法不是在每个调用点记得判断，
/// 而是**让这个值无法被构造**：零周期 ⇒ [`Cycles::new`] 返回 `None` ⇒ 调用方只能选择
/// "跳过这次测量"，没有别的路可走。这就是 Rust 里最省心的那类修复：
/// **把不变式放进类型，而不是放进注释与纪律**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cycles(NonZeroU32);

impl Cycles {
    /// 从原始周期数构造；**零返回 `None`**（零周期不是一次有效测量）。
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// 取回原始周期数（必定 ≥ 1）。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// `CYCCNT` 回绕保护阈值（周期）。
///
/// Cortex-M7 的 `CYCCNT` 是 32 位 @396 MHz ⇒ **10.85 s 回绕一圈**。若某次窗口超过
/// [这个阈值]（约 10.1 s，留 0.75 s 余量），就认为"很可能绕过了一圈"，
/// 此时 `end - start` 的结果**不可信**（可能很小）。
///
/// 判据是"宁可当成极长，也绝不当成极快" —— 见 [`SpanStats::record_wrap`]。
pub const WRAP_GUARD_CYCLES: u32 = 0xF000_0000;

/// 一次测量的判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measurement {
    /// 有效窗口（非零且未触发回绕保护）。
    Valid(Cycles),
    /// 疑似跨越 `CYCCNT` 回绕：真实长度不可知，但**至少** [`WRAP_GUARD_CYCLES`]。
    SuspectedWrap,
}

impl Measurement {
    /// 由 `end.wrapping_sub(start)` 的差值判定。
    ///
    /// `None` = 零周期（本次没有测到，调用方应当**跳过**而不是记一条 0）。
    #[must_use]
    pub const fn from_delta(delta: u32) -> Option<Self> {
        if delta == 0 {
            return None;
        }
        if delta >= WRAP_GUARD_CYCLES {
            return Some(Self::SuspectedWrap);
        }
        match Cycles::new(delta) {
            Some(cycles) => Some(Self::Valid(cycles)),
            None => None,
        }
    }
}

/// 直方图桶数：按 `floor(log2(cycles))` 分桶（32 位计数器最多 32 桶）。
pub const BUCKET_COUNT: usize = 32;

/// 一个 span 的实测统计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanStats {
    /// 记录了多少次。
    ///
    /// **`u64` 而不是 `u32`**（2026-10-12 审计）：`measured_cs` 挂在"每字节 TX 入队"与
    /// `idle` 每轮的 `rx_pop` 上，实测约 **5.6×10⁵ 次/秒** ⇒ `u32` 会在 **1.3–2 小时**
    /// 内顶满，然后 `saturating_add` **静默停住** —— 数字看起来还在，其实早就不动了。
    /// 对一个"要连续跑 30 天"的设备，这种静默饱和是最坏的一类失效。
    pub count: u64,
    /// 最近一次的周期数。
    pub last_cycles: u32,
    /// 自上次复位以来最大的周期数。
    pub max_cycles: u32,
    /// 累计周期数（用于算平均；`u64` 避免长时间运行后溢出）。
    pub total_cycles: u64,
    /// 超过预算的次数（预算为 0 时恒为 0）。同样用 `u64`，理由见 [`SpanStats::count`]。
    pub overruns: u64,
    /// 窗口疑似**跨越了 `CYCCNT` 回绕**（32 位 @396 MHz ⇒ 10.85 s）的次数。
    ///
    /// 真机上超过这个长度的窗口会把 `end - start` 变成一个**很小**的值 ——
    /// "极长"被显示成"极快"。因此这种窗口单独计数，并把 `max` 抬高到
    /// [`WRAP_GUARD_CYCLES`] 这个**下界**（它至少有这么长），同时计入超预算。
    pub wraps: u64,
    /// 预算（周期）。`0` = 不设预算。
    pub budget_cycles: u64,
    /// 按 `floor(log2(cycles))` 分桶的直方图（用于近似 p50/p99）。
    ///
    /// 每桶计数用 `u32` 饱和：对 p99 只需要"累计占比"，即使极端长时间运行后
    /// 某桶饱和，也不会让百分位**偏小**（饱和只让累计更早到达目标，给出的是更保守
    /// 的上界）。精确的 `count`/`max` 仍是 `u64`/`u32`，不受影响。
    pub buckets: [u32; BUCKET_COUNT],
}

impl SpanStats {
    /// 建立一个带预算的空统计。
    #[must_use]
    pub const fn new(budget_cycles: u64) -> Self {
        Self {
            count: 0,
            last_cycles: 0,
            max_cycles: 0,
            total_cycles: 0,
            overruns: 0,
            wraps: 0,
            budget_cycles,
            buckets: [0; BUCKET_COUNT],
        }
    }

    /// 记录一次测量；返回本次是否**超预算**。
    ///
    /// 参数是 [`Cycles`]（非零）而不是裸 `u32`：**"0 周期样本"在类型上就构造不出来**，
    /// 因此"没测到"永远不会被记成"测到 0"（真机踩过的坑，见 [`Cycles`] 的文档）。
    ///
    /// 预算为 0 时不判定（永远返回 `false`）—— 这是"只观测不判定"的显式表达，
    /// 不是"忘记设预算"的遮羞布：没设预算的 span 会在报告中显示为 `-`。
    pub fn record(&mut self, cycles: Cycles) -> bool {
        let cycles = cycles.get();
        self.count = self.count.saturating_add(1);
        self.last_cycles = cycles;
        if cycles > self.max_cycles {
            self.max_cycles = cycles;
        }
        self.total_cycles = self.total_cycles.saturating_add(u64::from(cycles));
        let over = self.budget_cycles != 0 && u64::from(cycles) > self.budget_cycles;
        if over {
            self.overruns = self.overruns.saturating_add(1);
        }
        self.note_bucket(cycles);
        over
    }

    /// 把一次样本计入直方图。
    fn note_bucket(&mut self, cycles: u32) {
        if cycles == 0 {
            return;
        }
        let bits = u32::BITS - cycles.leading_zeros();
        let index = usize::try_from(bits.saturating_sub(1)).unwrap_or(0);
        if let Some(slot) = self.buckets.get_mut(index) {
            *slot = slot.saturating_add(1);
        }
    }

    /// 近似百分位：返回该百分位样本所在桶的**上界**（周期）。
    ///
    /// 例：p99 返回 `2^(b+1)-1`，即"99% 的样本 ≤ 这个值"。桶是 2 的幂，
    /// 所以这是一个**保守上界**，不是精确分位数；精确值需要保存全部样本，
    /// 与本项目"零分配"的约束冲突。
    #[must_use]
    pub fn percentile_upper_cycles(&self, percent: u8) -> Option<u32> {
        if self.count == 0 {
            return None;
        }
        let percent = u64::from(percent.clamp(1, 100));
        let target = self.count.saturating_mul(percent).saturating_add(99) / 100;
        let mut accumulated = 0u64;
        for (index, count) in self.buckets.iter().enumerate() {
            accumulated = accumulated.saturating_add(u64::from(*count));
            if accumulated >= target {
                return Some(bucket_upper_bound(index));
            }
        }
        Some(u32::MAX)
    }

    /// 近似 p99 的上界（周期）。
    #[must_use]
    pub fn p99_upper_cycles(&self) -> Option<u32> {
        self.percentile_upper_cycles(99)
    }

    /// 记录一次**疑似跨越 `CYCCNT` 回绕**的窗口。
    ///
    /// 语义：这个窗口**至少**有 [`WRAP_GUARD_CYCLES`] 那么长（真实长度不可知，因为计数器
    /// 已经绕过一圈）。因此：
    /// - `max_cycles` 被抬到该下界（**绝不让它看起来很短**）；
    /// - `wraps` 与 `overruns` 都自增 ⇒ 一定会被判为超预算并出现在明细里。
    ///
    /// 这正是审计指出的缺口：`wrapping_sub` 对 >10.85 s 的窗口会静默给出小值，
    /// 而"看起来很快"比"报错"危险得多。
    pub fn record_wrap(&mut self) {
        self.count = self.count.saturating_add(1);
        self.wraps = self.wraps.saturating_add(1);
        self.overruns = self.overruns.saturating_add(1);
        self.last_cycles = WRAP_GUARD_CYCLES;
        if WRAP_GUARD_CYCLES > self.max_cycles {
            self.max_cycles = WRAP_GUARD_CYCLES;
        }
        self.total_cycles = self
            .total_cycles
            .saturating_add(u64::from(WRAP_GUARD_CYCLES));
        self.note_bucket(WRAP_GUARD_CYCLES);
    }

    /// 平均值（周期）。没记录过时返回 `None`（**不要**返回 0：0 会被读成"很快"）。
    #[must_use]
    pub fn mean_cycles(&self) -> Option<u64> {
        // 用 `checked_div` 而不是"先判零再除"：既满足 clippy 的 `manual_checked_division`，
        // 也把"计数为 0 就没有均值"这件事表达成一次判定而不是两次。
        self.total_cycles
            .checked_div(self.count)
            .filter(|_| self.count > 0)
    }

    /// 最大值的预算余量（百分比，100 = 刚好用满，>100 不可能——超预算时返回 `None`）。
    ///
    /// 用整数算术（crate 禁止浮点）：`headroom = (budget - max) * 100 / budget`。
    #[must_use]
    pub fn headroom_percent(&self) -> Option<u64> {
        // ⚠️ 这一条是测试逼出来的：没有样本时若照公式算（max=0）会得到 **100% 余量**，
        // 于是"还没测过"被显示成"余量充足" —— 正是本项目最忌讳的"把没测的当成测了"。
        if self.count == 0 || self.budget_cycles == 0 {
            return None;
        }
        let max = u64::from(self.max_cycles);
        if max > self.budget_cycles {
            return None;
        }
        Some((self.budget_cycles - max) * 100 / self.budget_cycles)
    }

    /// 是否曾经超预算。
    #[must_use]
    pub const fn over_budget(&self) -> bool {
        self.overruns > 0
    }
}

/// 桶索引对应的上界（周期）：桶 `b` 覆盖 `[2^b, 2^(b+1)-1]`。
fn bucket_upper_bound(index: usize) -> u32 {
    if index >= 31 {
        u32::MAX
    } else {
        (1u32 << (index + 1)).saturating_sub(1)
    }
}

/// 全部 span 的实测容器。
///
/// 零分配、可放在 `static`（`new()` 是 `const fn`）；本身**不做同步** ——
/// 谁负责关中断/加锁是调用方的事（BSP 把它放在 `critical_section::Mutex` 里）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtMetrics {
    slots: [SpanStats; SPAN_COUNT],
    /// 关中断嵌套深度。只有 0 → 1 的那次进入会真正计时。
    irq_depth: u32,
}

impl Default for RtMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl RtMetrics {
    /// 按 [`Span`] 的预算建立空容器。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [
                SpanStats::new(Span::IrqOff.budget_cycles()),
                SpanStats::new(Span::IsrDeadlineTick.budget_cycles()),
                SpanStats::new(Span::IsrConsoleRx.budget_cycles()),
                SpanStats::new(Span::ShellInput.budget_cycles()),
                SpanStats::new(Span::StorageCritical.budget_cycles()),
                SpanStats::new(Span::BootInit.budget_cycles()),
                SpanStats::new(Span::IdleStep.budget_cycles()),
                SpanStats::new(Span::BootPost.budget_cycles()),
            ],
            irq_depth: 0,
        }
    }

    /// 复位所有统计（保留预算）。**不影响**关中断嵌套深度 —— 在临界区里复位统计
    /// 会把当前这次未结束的测量算错，因此深度不清零。
    pub fn reset(&mut self) {
        for (slot, span) in self.slots.iter_mut().zip(Span::ALL) {
            *slot = SpanStats::new(span.budget_cycles());
        }
    }

    /// 记录一次测量；返回本次是否超预算。
    ///
    /// 对 [`Span::IrqOff`] 请**不要**直接调用本方法 —— 用
    /// [`enter_irq_off`](Self::enter_irq_off) / [`exit_irq_off`](Self::exit_irq_off)，
    /// 否则嵌套的临界区会被重复计入。
    /// 记录一次测量判定结果（含"疑似回绕"）。
    pub fn record_measurement(&mut self, span: Span, measurement: Measurement) -> bool {
        match measurement {
            Measurement::Valid(cycles) => self.record(span, cycles),
            Measurement::SuspectedWrap => match self.slots.get_mut(span.index()) {
                Some(slot) => {
                    slot.record_wrap();
                    true
                }
                None => false,
            },
        }
    }

    /// 记录一次**有效**测量（非零周期，由 [`Cycles`] 保证）；返回本次是否超预算。
    ///
    /// 需要处理"疑似回绕"时用 [`record_measurement`](Self::record_measurement)。
    pub fn record(&mut self, span: Span, cycles: Cycles) -> bool {
        match self.slots.get_mut(span.index()) {
            Some(slot) => slot.record(cycles),
            // 不可达：`Span::index()` 与 `SPAN_COUNT` 的一致性由
            // `span_索引与槽位数一致` 这条测试钉住。
            None => false,
        }
    }

    /// 进入关中断区间。返回 `true` 表示**这是最外层**（调用方应当读一次周期计数器）。
    pub fn enter_irq_off(&mut self) -> bool {
        let outermost = self.irq_depth == 0;
        self.irq_depth = self.irq_depth.saturating_add(1);
        outermost
    }

    /// 退出关中断区间。`cycles` 是本次窗口的周期数（只有最外层会被采纳）。
    ///
    /// 返回本次是否超预算。深度为 0 时（多退了一次）返回 `false` 且**不**记录 ——
    /// 配对错误由测试与代码审查负责，这里不制造假数据。
    pub fn exit_irq_off(&mut self, cycles: Cycles) -> bool {
        if self.irq_depth == 0 {
            return false;
        }
        self.irq_depth = self.irq_depth.saturating_sub(1);
        if self.irq_depth == 0 {
            return self.record(Span::IrqOff, cycles);
        }
        false
    }

    /// 退出关中断区间，但窗口**疑似跨越 `CYCCNT` 回绕**（真实长度不可知）。
    ///
    /// 只对最外层生效，语义与 [`exit_irq_off`](Self::exit_irq_off) 一致；
    /// 记的是 [`SpanStats::record_wrap`] 而不是一个不可信的小值。
    pub fn exit_irq_off_wrapped(&mut self) -> bool {
        if self.irq_depth == 0 {
            return false;
        }
        self.irq_depth = self.irq_depth.saturating_sub(1);
        if self.irq_depth == 0 {
            if let Some(slot) = self.slots.get_mut(Span::IrqOff.index()) {
                slot.record_wrap();
            }
            return true;
        }
        false
    }

    /// 把所有嵌套深度清零（panic 路径：配对被中断了，必须显式复位，否则仪表静默失效）。
    pub fn abandon_all(&mut self) {
        self.irq_depth = 0;
    }

    /// 放弃当前关中断窗口（计数器不可用）：只把深度退回去，**不记录任何样本**。
    ///
    /// 与 [`exit_irq_off`](Self::exit_irq_off) 的区别是"没测到"与"测到 0"：
    /// 后者会污染统计（`count > 0` 而 `max == 0`），并让 [`check`](Self::check) 在
    /// **一次都没测过**的情况下报"全部在预算内"。
    pub fn abandon_irq_off(&mut self) {
        if self.irq_depth > 0 {
            self.irq_depth -= 1;
        }
    }

    /// 有没有**任何**样本（用于把"没在测"与"都合格"分开）。
    ///
    /// ⚠️ 这条区分的实战理由（2026-10-12 真机）：DWT 没启用时全部测量被跳过，
    /// 而心跳仍打 `rt=ok` —— **一次都没测过**被显示成"预算成立"。
    #[must_use]
    pub fn has_samples(&self) -> bool {
        self.slots.iter().any(|slot| slot.count > 0)
    }

    /// 当前关中断嵌套深度（诊断用；正常在临界区外应为 0）。
    #[must_use]
    pub const fn irq_depth(&self) -> u32 {
        self.irq_depth
    }

    /// 取某个 span 的统计副本。
    #[must_use]
    pub fn stats(&self, span: Span) -> SpanStats {
        match self.slots.get(span.index()) {
            Some(slot) => *slot,
            None => SpanStats::new(0),
        }
    }

    /// 超预算的 span 列表（机器判据用）。
    ///
    /// 返回 `Err(violations)` 时，`violations` 里每项是 `(span, 实测最大微秒, 预算微秒)`。
    /// 这里**返回数据而不是字符串**：让调用方（固件/门禁）自己决定怎么呈现，
    /// 也让门禁能对具体数字做判断，而不是匹配文本。
    pub fn violations(&self) -> heapless_violations::Violations {
        self.violations_filtered(|_| true)
    }

    /// 只报**运行期**的超预算项（启动期除外）。
    pub fn runtime_violations(&self) -> heapless_violations::Violations {
        self.violations_filtered(|span| !span.is_startup())
    }

    /// 只报**启动期**的超预算项。
    pub fn startup_violations(&self) -> heapless_violations::Violations {
        self.violations_filtered(Span::is_startup)
    }

    fn violations_filtered(&self, keep: impl Fn(Span) -> bool) -> heapless_violations::Violations {
        let mut out = heapless_violations::Violations::new();
        for span in Span::ALL.iter().copied().filter(|span| keep(*span)) {
            let stats = self.stats(span);
            if stats.over_budget() {
                out.push(heapless_violations::Violation {
                    span,
                    max_micros: micros_from_cycles(u64::from(stats.max_cycles)),
                    budget_micros: span.budget_micros(),
                    overruns: stats.overruns,
                    wraps: stats.wraps,
                });
            }
        }
        out
    }

    /// 机器判据：有 span 超预算就返回一份**轻量摘要**。
    ///
    /// 摘要只带"违反了几项 + 第一项明细"：`check()` 会被放在启动横幅与门禁判定的热路径上，
    /// 让它背着整个 128 字节清单走（clippy 的 `result_large_err` 正是这个意思）没有必要。
    /// 要看全部明细就用 [`violations`](Self::violations)。
    pub fn check(&self) -> Result<(), heapless_violations::ViolationSummary> {
        // ⚠️ 只看**运行期**：`boot-init` 是"故意超标"的（它把结构性盲区变成可见数字，
        // 见 `BUDGET_BOOT_INIT_MICROS`），让它污染运行期结论会让 `rt=` 永远红、
        // 然后被现场无视。启动期用 `startup_violations()` 单独看。
        let violations = self.runtime_violations();
        match violations.iter().next() {
            None => Ok(()),
            Some(first) => Err(heapless_violations::ViolationSummary {
                count: violations.len(),
                first: *first,
            }),
        }
    }

    /// 是否有任何 span 超预算（廉价版，供启动横幅判一句"实时预算：OK/违反"）。
    #[must_use]
    pub fn all_within_budget(&self) -> bool {
        self.runtime_violations().is_empty()
    }

    /// 把报告写进任意 `core::fmt::Write`（固件传控制台，测试传 `String`）。
    ///
    /// 格式刻意保持**每行一个 span**、列固定，便于人扫读，也便于门禁按列解析。
    pub fn report(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        writeln!(
            out,
            "span            count     last      max     mean      p99   budget  余量"
        )?;
        for span in Span::ALL.iter().copied() {
            let stats = self.stats(span);
            write!(out, "{:<14}", span.name())?;
            write!(out, "{:>7}", stats.count)?;
            // ⚠️ 没有样本时 `last`/`max` 也必须是 `-`：它们的真实值是 0 周期，
            // 但打出来的 `0` 会被读成"耗时为零"——与 `mean`/余量列的取舍必须一致
            // （同一个坑：`headroom_percent` 曾把"还没测过"算成"余量 100%"）。
            if stats.count == 0 {
                write!(out, "{:>9}", "-")?;
                write!(out, "{:>9}", "-")?;
            } else {
                write!(
                    out,
                    "{:>9}",
                    micros_from_cycles(u64::from(stats.last_cycles))
                )?;
                write!(
                    out,
                    "{:>9}",
                    micros_from_cycles(u64::from(stats.max_cycles))
                )?;
            }
            match stats.mean_cycles() {
                Some(mean) => write!(out, "{:>9}", micros_from_cycles(mean))?,
                None => write!(out, "{:>9}", "-")?,
            }
            match stats.p99_upper_cycles() {
                Some(p99) => write!(out, "{:>8}", micros_from_cycles(u64::from(p99)))?,
                None => write!(out, "{:>8}", "-")?,
            }
            if stats.budget_cycles == 0 {
                write!(out, "{:>9}", "-")?;
            } else {
                write!(out, "{:>9}", span.budget_micros())?;
            }
            match stats.headroom_percent() {
                Some(percent) => writeln!(out, "{:>6}%", percent)?,
                None if stats.count == 0 => writeln!(out, "{:>7}", "-")?,
                // 超预算：把超出量直接写出来，不要只写一个"%"
                None => writeln!(
                    out,
                    "{:>6}%",
                    micros_from_cycles(u64::from(stats.max_cycles)).saturating_mul(100)
                        / span.budget_micros()
                )?,
            }
        }
        Ok(())
    }
}

/// 周期 → 微秒（向下取整）。
#[must_use]
pub fn cycles_to_micros(cycles: Cycles) -> u64 {
    micros_from_cycles(u64::from(cycles.get()))
}

/// 周期 → 微秒（向下取整），`u64` 入口。
#[must_use]
pub const fn micros_from_cycles(cycles: u64) -> u64 {
    // CPU_HZ 是 u64；先乘后除，避免先除造成精度损失。
    cycles.saturating_mul(1_000_000) / budget::CPU_HZ
}

/// 超预算清单（固定容量，零分配）。
pub mod heapless_violations {
    use super::Span;

    /// 最多同时报告多少个超预算的 span（等于 span 总数，够用且无需动态分配）。
    pub const CAPACITY: usize = super::SPAN_COUNT;

    /// 一项预算违反。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Violation {
        /// 哪个 span。
        pub span: Span,
        /// 实测最大值（微秒）。
        pub max_micros: u64,
        /// 预算（微秒）。
        pub budget_micros: u64,
        /// 超预算发生了多少次。
        pub overruns: u64,
        /// 其中有多少次疑似跨越了 `CYCCNT` 回绕（真实长度不可知，但一定极长）。
        pub wraps: u64,
    }

    /// 违反摘要：`check()` 的错误分支（轻量、`Copy`）。
    ///
    /// `first` 是**按 span 顺序**的第一项（不是"最严重的那项"—— 那需要额外定义"严重"，
    /// 而这里只需要一个能立刻打进日志的样本）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ViolationSummary {
        /// 一共几项超预算。
        pub count: usize,
        /// 第一项明细。
        pub first: Violation,
    }

    impl core::fmt::Display for ViolationSummary {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(
                f,
                "{} 项超预算，例如 {}：实测 {} µs > 预算 {} µs（{} 次）",
                self.count,
                self.first.span.name(),
                self.first.max_micros,
                self.first.budget_micros,
                self.first.overruns
            )
        }
    }

    /// 固定容量的违反清单。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Violations {
        items: [Option<Violation>; CAPACITY],
        len: usize,
    }

    impl Default for Violations {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Violations {
        /// 空清单。
        #[must_use]
        pub const fn new() -> Self {
            Self {
                items: [None; CAPACITY],
                len: 0,
            }
        }

        /// 追加一项（超出容量则丢弃 —— 容量等于 span 总数，正常不可能溢出）。
        pub fn push(&mut self, violation: Violation) {
            if let Some(slot) = self.items.get_mut(self.len) {
                *slot = Some(violation);
                self.len += 1;
            }
        }

        /// 条目数。
        #[must_use]
        pub const fn len(&self) -> usize {
            self.len
        }

        /// 是否为空。
        #[must_use]
        pub const fn is_empty(&self) -> bool {
            self.len == 0
        }

        /// 迭代全部条目。
        pub fn iter(&self) -> impl Iterator<Item = &Violation> {
            self.items.iter().flatten()
        }
    }

    impl core::fmt::Display for Violations {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            for (index, violation) in self.iter().enumerate() {
                if index > 0 {
                    write!(f, "; ")?;
                }
                write!(
                    f,
                    "{} 超预算：实测 {} µs > 预算 {} µs（{} 次",
                    violation.span.name(),
                    violation.max_micros,
                    violation.budget_micros,
                    violation.overruns
                )?;
                if violation.wraps > 0 {
                    write!(
                        f,
                        "，其中 {} 次疑似 CYCCNT 回绕（真实长度不可知，≥10.1 s）",
                        violation.wraps
                    )?;
                }
                write!(f, "）")?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
