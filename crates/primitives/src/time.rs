//! 单调时间：回绕安全的比较与"到期"判定（**平台需求 RT-4**）。
//!
//! # 为什么不用 `core::time::Duration` + 裸 `u64`
//!
//! 现场时基来自硬件计数器，**一定会回绕**。一旦回绕，"到期"判定写成
//! `now >= deadline` 就会在最坏情况下失效——而且只在连续运行若干天/若干次回绕后
//! 出现，属于最难复现的一类缺陷。
//!
//! 本模块的处理方式是把正确性抬进类型：
//!
//! - [`Timepoint`] **刻意不实现 `PartialOrd`/`Ord`**。于是 `now < deadline`
//!   是编译错误，唯一可用的判定是 [`Timepoint::has_reached`]（回绕安全）。
//! - 时间差与加法一律用 `wrapping_*`，因为"回绕"是设计的一部分而不是异常。
//!
//! 本模块内的算术**全部**显式回绕，因此单独开启 `arithmetic_side_effects` 门禁：
//! 任何一处普通的 `+`/`-` 都会是编译失败，而不是一次静默溢出。

// 本模块的算术必须全部显式（wrapping_/saturating_），普通运算符直接拒绝。
#![deny(clippy::arithmetic_side_effects)]

/// 一毫秒的微秒数。
pub const MICROS_PER_MILLI: u64 = 1_000;
/// 一秒的微秒数。
pub const MICROS_PER_SECOND: u64 = 1_000_000;

/// 一段时间长度，单位微秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Duration(u64);

impl Duration {
    /// 零长度。
    pub const ZERO: Self = Self(0);

    /// 由微秒构造。
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// 由毫秒构造（溢出饱和）。
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(MICROS_PER_MILLI))
    }

    /// 由秒构造（溢出饱和）。
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(MICROS_PER_SECOND))
    }

    /// 微秒数。
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// 毫秒数（向下取整）。
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0.wrapping_div(MICROS_PER_MILLI)
    }

    /// 是否为零。
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// 饱和相减。
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

/// 单调时间点，单位微秒，**允许回绕**。
///
/// 单位微秒的理由：最紧的实时约束是串行总线 115200 8N1 的字节间隔 86.8 µs，微秒分辨率足够；
/// 而 u64 微秒可表示约 58 万年，远超两次回绕之间的运行时长。
///
/// ⚠️ 本类型**没有** `PartialOrd`：请用 [`Timepoint::is_after`] 与
/// [`Timepoint::has_reached`]。派生顺序比较在回绕后会给出错误答案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Timepoint(u64);

impl Timepoint {
    /// 数值空间的一半。判定"先后"时，差值落在半个空间内即认为"在后"。
    const HALF_SPACE: u64 = u64::MAX >> 1;

    /// 零点。
    pub const ZERO: Self = Self(0);

    /// 由微秒构造。
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// 微秒数。
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// 回绕安全的加法。
    #[must_use]
    pub const fn wrapping_add(self, delta: Duration) -> Self {
        Self(self.0.wrapping_add(delta.as_micros()))
    }

    /// `self` 是否**严格晚于** `other`（回绕安全）。
    ///
    /// 注意零差值必须判为 `false`：相等既不是"之后"也不是"之前"。
    /// 早期版本漏了这一条，导致 `has_reached(deadline)` 在 `now == deadline`
    /// 时返回 false —— 到期判定恰好卡在边界上失败。
    #[must_use]
    pub const fn is_after(self, other: Self) -> bool {
        let delta = self.0.wrapping_sub(other.0);
        delta != 0 && delta < Self::HALF_SPACE
    }

    /// 自 `earlier` 起经过的时间（回绕安全）。
    #[must_use]
    pub const fn elapsed_since(self, earlier: Self) -> Duration {
        Duration::from_micros(self.0.wrapping_sub(earlier.0))
    }

    /// 是否已到（或已过）`deadline`（回绕安全，**含相等**）。
    ///
    /// 这是本项目**唯一**允许的到期判定方式。
    /// 实现直接比较"自 deadline 起的回绕差值"，因此 `self == deadline` 判为已到。
    #[must_use]
    pub const fn has_reached(self, deadline: Self) -> bool {
        self.0.wrapping_sub(deadline.0) < Self::HALF_SPACE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_conversions_saturate_instead_of_wrapping() {
        assert_eq!(Duration::from_millis(3).as_micros(), 3_000);
        assert_eq!(Duration::from_secs(2).as_micros(), 2_000_000);
        assert_eq!(Duration::from_millis(1_500).as_millis(), 1_500);
        assert!(Duration::ZERO.is_zero());
        assert_eq!(u64::MAX, Duration::from_millis(u64::MAX).as_micros());
        assert_eq!(
            Duration::from_micros(100).saturating_sub(Duration::from_micros(300)),
            Duration::ZERO
        );
    }

    #[test]
    fn deadline_uses_wrap_safe_comparison() {
        let now = Timepoint::from_micros(1_000);
        let deadline = now.wrapping_add(Duration::from_millis(10));
        assert!(!now.has_reached(deadline));
        assert!(deadline.has_reached(deadline));
        let later = deadline.wrapping_add(Duration::from_micros(1));
        assert!(later.has_reached(deadline));
    }

    #[test]
    fn wrapping_counter_does_not_break_deadline_logic() {
        // 计数器在 deadline 之前回绕：这是"用 < 判到期"会失效的场景。
        let almost_wrapped = Timepoint::from_micros(u64::MAX - 500);
        let deadline = almost_wrapped.wrapping_add(Duration::from_micros(1_000));
        assert!(
            deadline.as_micros() < almost_wrapped.as_micros(),
            "必须确实回绕过"
        );
        assert!(!almost_wrapped.has_reached(deadline));
        let after_wrap = deadline.wrapping_add(Duration::from_micros(1));
        assert!(after_wrap.has_reached(deadline));
        assert!(after_wrap.is_after(almost_wrapped));
        // deadline 相对 almost_wrapped 是 +1000 µs，再 +1 µs，合计 1001 µs。
        assert_eq!(
            after_wrap.elapsed_since(almost_wrapped),
            Duration::from_micros(1_001)
        );
    }

    #[test]
    fn elapsed_is_wrap_safe() {
        let a = Timepoint::from_micros(u64::MAX - 10);
        let b = a.wrapping_add(Duration::from_micros(100));
        assert_eq!(b.elapsed_since(a), Duration::from_micros(100));
    }
}
