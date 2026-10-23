//! Cortex-M7 **数据缓存维护**（按地址范围失效）—— 器件内容改变后让 XIP 读重新一致。
//!
//! # 为什么必须自己写（而不是用现成 API）
//!
//! 已在本机 registry 源码里核实：**`cortex-m 0.7.9` 与 `imxrt-hal 0.6` 都没有
//! D-Cache 维护 API**（`grep -rn "invalidate_dcache|clean_dcache|DCIMVAC"` 无结果）。
//! 而 XIP 映射区是**可缓存**的（横幅 `flash rd : … cache ON`）：器件内容一改，
//! 缓存里的旧行就是脏的，紧接着的读会拿到旧内容 —— 表现为"写了没生效"。
//!
//! # 真机定位（2026-10-13，一次决定性对照）
//!
//! 把介质层的整页编程从 ROM 路径切到**寄存器路径**之后，**两条自证同时失效**：
//!
//! ```text
//! flash : 自证(跨页+非对齐) FAIL bad=300        ← 写入范围第一个字节仍是擦除态
//! flash : reg 写路径自证 FAIL detail=100        ← 第 0 字节就不符
//! ```
//!
//! 决定性对照：把 [`crate::config::MPU_AND_CACHE_ENABLED`] 置 `false`（D-Cache 关）后
//! **两条自证全部 PASS**（`自证(跨页+非对齐) PASS bad=0`、`reg 写路径自证 PASS detail=0`）
//! ⇒ **写入路径本身完全正确，唯一障碍是缓存一致性**。
//!
//! ⚠️ **ROM 的 `clear_cache` 不够**：SDK 里它是 `flexspi_clear_cache` → `flexspi_swreset`，
//! 清的是**控制器侧**的 AHB RX 缓冲/预取（那是勘误 ERR011207 的另一半），
//! **不是 Cortex-M7 的 L1 D-Cache**。两者都要做，且各自负责不同的失效对象。
//!
//! # 与勘误的关系
//!
//! `[CE ERR011207]`：`AHBCR[PREFETCHEN]=1` 且非缓存时，"写后立刻从 AHB 读同地址"
//! 可能返回错数据 —— 那一条讲的是**控制器预取**（由 `flexspi_swreset` 处理）。
//! 本模块处理的是**内核 L1 D-Cache**，是同一现象的另一半来源。两条都要覆盖。

/// D-Cache 行大小（Cortex-M7 = 32 字节；与 `CCSIDR` 报告的行大小一致）。
pub const CACHE_LINE_BYTES: usize = 32;

/// `SCB.DCIMVAC`（按 MVA 失效 D-Cache 到 PoC）。
///
/// 地址来自 ARMv7-M 架构参考手册的 SCB 寄存器摘要表
/// （`0xE000_EF50` DCCMVAC、`0xE000_EF58` DCCIMVAC、**`0xE000_EF5C` DCIMVAC**）。
/// ⚠️ 这是**架构寄存器**，不是芯片外设寄存器，因此不受 MPU 区域属性影响。
const SCB_DCIMVAC: u32 = 0xE000_EF5C;

/// **失效**覆盖 `[start, start + len)` 的 D-Cache 行。
///
/// # 为什么是"失效"而不是"清"
///
/// 场景是"**器件**内容变了，CPU 缓存里的副本过期"。我们要的是**丢弃** CPU 的旧副本，
/// 下一次读直接从器件取 —— 这正是 `DCIMVAC`（invalidate）的语义。
/// 若用 `DCCMVAC`（clean：把 CPU 副本写回内存），对"外部器件"根本没有意义，
/// 因为器件不在缓存一致性域内。
///
/// # 边界
///
/// 按行对齐向外扩：起点向下取整、终点向上取整。多失效几行无害（只是后续多读一次），
/// 少失效一行就会留下脏行 —— 所以宁可扩、不可缩。
///
/// D-Cache 关闭时这些写是**架构允许的空操作**（维护操作在缓存关闭时无副作用），
/// 因此本函数无需判断缓存是否使能。
pub fn invalidate_dcache_range(start: usize, len: usize) {
    if len == 0 {
        return;
    }
    let mask = CACHE_LINE_BYTES - 1;
    let begin = start & !mask;
    let Some(tail) = start.checked_add(len) else {
        return;
    };
    let end = tail.saturating_add(mask) & !mask;

    let mut at = begin;
    while at < end {
        let Ok(mva) = u32::try_from(at) else {
            return;
        };
        // SAFETY:
        // ① `SCB_DCIMVAC` 是 ARMv7-M **架构定义**的 D-Cache 维护寄存器
        //    （`0xE000_EF5C`，见 ARMv7-M ARM 的 SCB 摘要表）—— 不是外设寄存器，
        //    不读任何设备状态、不产生外部副作用；
        // ② 该指令只**失效** `mva` 所在的 cache line，不改内存内容、不改外设；
        // ③ `mva` 由 `usize -> u32` 的**检查式**转换得到（失败即返回，绝不截断）；
        // ④ 循环每次按行递增，`end - begin` 有界（调用方传的是页/块长度），必然终止；
        // ⑤ `nostack`：不需要栈；`preserves_flags`：不改条件标志。
        unsafe {
            core::arch::asm!(
                "str {v}, [{r}]",
                v = in(reg) mva,
                r = in(reg) SCB_DCIMVAC,
                options(nostack, preserves_flags)
            );
        }
        at += CACHE_LINE_BYTES;
    }
    // 保证所有失效操作在后续访存之前完成（ARMv7-M 要求维护操作后跟 DSB）。
    cortex_m::asm::dsb();
}

/// **把**指定范围的 D-Cache 行写回内存（clean，`DCCMVAC`）。
///
/// 与 [`invalidate_dcache_range`] 的分工：
/// - **clean**：内存内容过时（CPU 有更新的副本）⇒ 把副本写回；
/// - **invalidate**：器件内容过时（CPU 有旧副本）⇒ 丢弃副本、下次从器件读。
///
/// # 为什么需要它
///
/// `.uninit` 段（OCRAM，可缓存）里的跨暖复位记录（崩溃记录、连续复位计数）
/// 在写完之后如果只停在 D-Cache，**复位时可能没有写回 SRAM**；下次启动读到的
/// 还是旧值。2026-10-19 真机校准抓到了这个现象：心跳里 `rst=0`（缓存值），
/// 复位后 boot 报告却仍是旧计数。写记录后 clean 一次即可。
///
/// # 实现
///
/// 用 `cortex_m` 0.7.9 自带的 [`cortex_m::peripheral::SCB::clean_dcache_by_address`]
/// （它内部已经处理 DSB 与行对齐）；这里仅负责拿到 `SCB` 单例。`SCB` 是内核私有
/// 单例，本函数只调用缓存维护方法、不读写任何外设状态。
pub fn clean_dcache_range(start: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: `Peripherals::steal()` 构造的是**未检查**的外设集合；本函数只取其中
    // `SCB` 字段调用 `clean_dcache_by_address` —— 该操作只发出 DCCMVAC 维护操作
    // （写回本核 D-Cache 行到 PoC），不改内存语义、不改外设、不产生中断。
    // 本函数在单核上、短生命周期调用，不与其他外设使用者并发。
    let mut peripherals = unsafe { cortex_m::peripheral::Peripherals::steal() };
    // ⚠️ `clean_dcache_by_address` 的 `num_lines` 是按**传入的 size 从对齐后的起点**
    // 计算的；如果 `start` 未对齐，末尾那一行会漏掉。2026-10-19 真机实测：
    // `.uninit` 的 CRASH 从 0x...d59c 开始、48 B，末尾 8 B 落在第三行 0x...d5c0；
    // 原调用只清了前两行，于是崩溃记录的 `uptime`/`fault_streak`/checksum 丢失，
    // 复位后 `last crash: none`。这里先把起点对齐、把长度补上起点偏移。
    let mask = CACHE_LINE_BYTES - 1;
    let aligned_start = start & !mask;
    let aligned_len = len.saturating_add(start & mask);
    peripherals
        .SCB
        .clean_dcache_by_address(aligned_start, aligned_len);
}
