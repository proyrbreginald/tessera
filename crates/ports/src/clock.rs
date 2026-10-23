//! 单调时钟端口。
//!
//! 领域逻辑**不读时钟**：时间一律由调用方以 [`Timepoint`] 传入。这个约束让所有
//! 状态机都能在主机上被确定性驱动（含回绕、抖动、超时）。本 trait 只服务于
//! "边界层需要知道现在几点"的少数位置：调度器、超时判定、日志时间戳。

use primitives::time::Timepoint;

/// 单调时间源。
///
/// 实现者必须保证**单调不减**且**允许回绕**（硬件计数器即如此）；
/// 因此调用方必须用 `has_reached`/`is_after` 而不是 `<`。
pub trait Clock {
    /// 当前时间点。
    fn now(&self) -> Timepoint;
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitives::time::Duration;

    struct Fake(u64);

    impl Clock for Fake {
        fn now(&self) -> Timepoint {
            Timepoint::from_micros(self.0)
        }
    }

    #[test]
    fn trait_is_object_safe_and_composable() {
        // trait 必须是 object-safe 的：BSP 要把它作为 &dyn Clock 传进上层。
        let fake = Fake(1_000);
        let clock: &dyn Clock = &fake;
        assert_eq!(clock.now().as_micros(), 1_000);
        let later = clock.now().wrapping_add(Duration::from_millis(1));
        assert!(later.is_after(clock.now()));
    }
}
