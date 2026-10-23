//! 给 RTIC 用的设备描述（PAC 垫片）。
//!
//! # 为什么需要这个垫片
//!
//! `#[rtic::app(device = ...)]` 对 device 的要求并非"一个完整的 PAC"，而是**恰好三样**：
//!
//! | RTIC 的要求 | `imxrt-ral` 是否提供 |
//! |-------------|---------------------|
//! | `Peripherals` 类型，带 `unsafe fn steal()` | ❌ 不提供 |
//! | 名为 `interrupt` 的**类型别名**（枚举变体经它可见） | ⚠️ 提供了，但叫 `imxrt_ral::interrupt`（`pub type interrupt = Interrupt`） |
//! | `NVIC_PRIO_BITS` | ✅ `imxrt_ral::NVIC_PRIO_BITS` |
//!
//! `imxrt-ral` 的用法是**按外设各自 `instance()`**，本来就没有"整包外设"的概念，
//! 所以这里补一个空壳。
//!
//! ⚠️ 两个踩过的坑，写在这里以免重犯：
//!
//! 1. `pub mod interrupt { pub use imxrt_ral::Interrupt; }` **不行**：
//!    RTIC 会去找 `interrupt::PIT` 这样的路径，而模块里的枚举名不构成该路径，
//!    于是报出 `you_must_enable_the_rt_feature_for_the_pac_in_your_cargo_toml::interrupt`
//!    （这条提示来自 `rtic-macros` 内部，指向的是"Cargo feature 没开"，
//!    但真正的原因是路径形状不对）。正确写法是**再导出类型别名**：
//!    `pub(crate) use imxrt_ral::interrupt;`。
//! 2. 光开 `imxrt-ral/rt` 还不够：`imxrt-rt` 的 `device` feature 也必须开，
//!    否则生成的链接脚本不会 `INCLUDE device.x`，中断弱别名不生效，
//!    链接期会报一堆 `undefined symbol: LPUART2`。

// `Interrupt` 本身不必再导出：RTIC 通过下面的 `interrupt` 类型别名访问枚举变体。
// 可见性取 `pub(crate)`：本 crate 是二进制，没有外部使用者，
// 而 `unreachable_pub` 门禁会拒绝对外暴露无人可用的 API。
pub(crate) use imxrt_ral::NVIC_PRIO_BITS;

/// 空壳外设集合。
///
/// `imxrt-ral` 没有"整包外设"的概念（按外设分别 `instance()`），因此不需要字段。
pub(crate) struct Peripherals;

impl Peripherals {
    /// RTIC 在 `init` 里调用它。
    ///
    /// # Safety
    ///
    /// 调用者必须保证只调用一次，且此后再也不通过其它途径获取同样的外设。
    /// 本类型是零字段空壳，因此实际上不产生别名风险；保留 `unsafe` 是为了与 PAC
    /// 的约定一致，也让将来若真的加入字段时不会静默放松。
    pub(crate) unsafe fn steal() -> Self {
        Self
    }
}

/// RTIC 以小写模块名查找中断枚举，且要求它是**类型别名式的再导出**。
pub(crate) use imxrt_ral::interrupt;
