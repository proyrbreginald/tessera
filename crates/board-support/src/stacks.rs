//! 栈余量**测量**（而不是估算）—— **薄适配层**。
//!
//! # 为什么需要它
//!
//! 这个项目的栈被真机打穿过三次，而且症状都是"串口一行都不出"：
//!
//! | 时间 | 栈 | 现象 | 根因 |
//! |---|---|---|---|
//! | 2026-10-09 | 8 KiB | HardFault，SP 到 `0x1FFFE7F0` | `init` 同时放业务服务实例(~4 KiB) 与启动装载缓冲(~3.5 KiB) |
//! | 2026-10-10 | 16 KiB | HardFault，`CFSR=0x400`(STKERR)、SP 到 `0x1FFFF838` | `fs::mount()` 的 ~7 KiB 实例 + 搬进静态的副本同时在栈上 |
//! | 2026-10-10（扩到 446 块后） | 32 KiB | 串口无输出 + SWD 连不上（lockup） | 实例涨到 ~12 KiB，峰值逼近 32 KiB |
//!
//! 三次都是"猜一个够大的数字"，三次都不够。**数字必须来自测量**：
//! 启动早期把未使用的栈区涂成固定图案，之后看图案被吃掉多少 —— 那就是峰值用量。
//!
//! # 这一层与 [`memory_layout`] 的分工
//!
//! **纯逻辑（涂抹范围、扫描判据、余量换算、阈值判定）在纯逻辑 crate
//! [`memory_layout`] 里**，它有主机测试，其中一条专门钉住本项目踩过的假象：
//! 涂抹时漏掉栈底那一个字 → 扫描时第一个字就不是指纹 → **永远报"整段栈都用过"**。
//! 本模块只负责"把真实栈区变成切片"这件必须碰指针的事。
//!
//! # 怎么用
//!
//! 启动最早期调用 [`paint`]（越早越准：它只能标记"此后"的用量），
//! 在启动完成后（例如 idle 里打印横幅时）调用 [`high_water_bytes`]。
//! 它给出的是"自栈底起被用到过多少字节"，因此
//! `STACK_SIZE - high_water` 就是**最坏情况下仍然剩余的余量**。

/// 栈底（最低地址）。由链接脚本决定：`imxrt-rt` 把栈放在 DTCM 最底部。
pub const STACK_BOTTOM: usize = memory_layout::STACK_BOTTOM;

/// 栈大小。**必须与 `firmware/build.rs` 的 `stack_size` 一致** ——
/// 由 `firmware` 里的编译期断言绑住，不一致就编译失败；
/// `xtask check-memory` 再从链接产物里核实一次。
pub const STACK_SIZE: usize = memory_layout::STACK_SIZE;

/// 栈顶（不含）。中断入栈从这里往下增长。
pub const STACK_TOP: usize = memory_layout::STACK_TOP;

/// 把**当前未使用**的栈区涂成指纹。
///
/// # 为什么关中断
///
/// 涂抹会写"当前 SP 以下"的地址。如果此刻来一个中断，硬件会把异常帧压进
/// 同一段栈里，而我们正把它当空闲区涂掉 —— 那会破坏中断帧。因此整个过程
/// （读 SP + 涂抹）必须在关中断的临界区里完成。
pub fn paint() {
    crate::rt::measured_cs(|_cs| {
        // 32 位目标上 u32→usize 无损；用 `let Ok(..) else` 而不是 `as`：
        // 万一将来目标换成 64 位，这里会**安全地什么都不做**，而不是涂错地址。
        let Ok(sp) = usize::try_from(cortex_m::register::msp::read()) else {
            return;
        };
        // 留 64 字节安全边距：不碰当前函数帧里可能被写到的位置。
        let upper = (sp.saturating_sub(64)) & !3;
        if upper <= STACK_BOTTOM {
            return;
        }
        // 涂抹范围自**栈底**起算（含栈底那一个字）—— 判据在 `memory_layout` 里，
        // 那里有一条测试专门说明"少涂一个字"会造成什么假象。
        let painted = (upper - STACK_BOTTOM) / 4;
        // SAFETY: `[STACK_BOTTOM, upper)` 全部落在链接脚本划给栈的 DTCM 区域内，
        // 且整体位于当前栈指针之下 —— 除了下面这段代码自己，没有别人在读它。
        // 关中断保证中断帧不会落进这段区间；切片只在本次调用内存活。
        let words = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::with_exposed_provenance_mut::<u32>(STACK_BOTTOM),
                painted,
            )
        };
        memory_layout::paint_prefix(words, painted, memory_layout::PAINT);
    });
}

/// 自栈底起被用到过的字节数（即峰值用量）。
///
/// 返回 `STACK_SIZE` 表示整段栈都被动过（应当视为"余量耗尽"）。
#[must_use]
pub fn high_water_bytes() -> usize {
    let words = STACK_SIZE / 4;
    // SAFETY: 这段地址属于本项目的栈区（起点与长度都来自 `memory-layout` 的
    // 常量，`xtask check-memory` 会核实链接产物确实把 `.stack` 放在这里）。
    // 只读，因此不需要关中断；用不可变切片避免制造别名。
    let stack = unsafe {
        core::slice::from_raw_parts(
            core::ptr::with_exposed_provenance::<u32>(STACK_BOTTOM),
            words,
        )
    };
    memory_layout::high_water_bytes(stack, memory_layout::PAINT)
}

/// 剩余余量（字节）。
#[must_use]
pub fn headroom_bytes() -> usize {
    STACK_SIZE.saturating_sub(high_water_bytes())
}

/// 清零 `.dtcm` 段。
///
/// 该段是 `NOLOAD`，启动时不会被加载器复制；其中的静态量必须在这里归零后才能使用。
///
/// # Safety
///
/// 只能在启动早期、任何 `.dtcm` 对象被使用之前调用一次。
pub unsafe fn zero_dtcm() {
    unsafe extern "C" {
        static mut __sdtcm: u8;
        static mut __edtcm: u8;
    }
    // SAFETY: 两个符号由链接脚本定义，区间是 `.dtcm` 段；启动期独占，尚未使用。
    let start = core::ptr::addr_of_mut!(__sdtcm).cast::<u8>();
    let end = core::ptr::addr_of_mut!(__edtcm).cast::<u8>();
    let len = end.addr().saturating_sub(start.addr());
    if len > 0 {
        // SAFETY: 区间来自链接脚本，长度按两端地址计算；对象尚未初始化。
        unsafe { core::ptr::write_bytes(start, 0, len) };
    }
}
