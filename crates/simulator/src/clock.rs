//! 手动时钟：时间由测试推进。
//!
//! 有了它，"超时""抖动""回绕"都变成确定性的、可复现的输入，
//! 而不是"跑一会儿看看会不会出问题"。
//!
//! # 为什么内部是 `&Cell<Timepoint>` 而不是自己持有 `Timepoint`
//!
//! 服务层**拥有**它的时钟（这是分层要求：时间从边界注入）。如果 `ManualClock`
//! 自己持有值，测试把时钟交给服务之后就再也推不动时间了 —— 而"推进时间"恰恰是
//! 所有超时场景的前提。
//!
//! 用共享的 `Cell` 让两边看同一份时间：服务通过 `Clock::now()` 读，
//! 测试通过 `advance`/`set` 写。`Cell` 而非 `RefCell`：读写都是单次拷贝，不产生借用。

use core::cell::Cell;

use ports::clock::Clock;
use primitives::time::{Duration, Timepoint};

/// 手动推进的单调时钟。
#[derive(Debug, Clone, Copy)]
pub struct ManualClock<'a> {
    now: &'a Cell<Timepoint>,
}

impl<'a> ManualClock<'a> {
    /// 绑定到一块共享的时间单元。
    #[must_use]
    pub const fn new(now: &'a Cell<Timepoint>) -> Self {
        Self { now }
    }

    /// 前进一段时间，返回新的时间点。
    pub fn advance(&self, delta: Duration) -> Timepoint {
        let next = self.now.get().wrapping_add(delta);
        self.now.set(next);
        next
    }

    /// 直接跳到某个时间点（构造"已经跑了很久/接近回绕"的场景）。
    pub fn set(&self, at: Timepoint) {
        self.now.set(at);
    }
}

impl Clock for ManualClock<'_> {
    fn now(&self) -> Timepoint {
        self.now.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_moves_monotonically() {
        let cell = Cell::new(Timepoint::ZERO);
        let clock = ManualClock::new(&cell);
        let t1 = clock.advance(Duration::from_millis(1));
        let t2 = clock.advance(Duration::from_millis(2));
        assert!(t2.is_after(t1));
        assert_eq!(t2.elapsed_since(t1), Duration::from_millis(2));
    }

    #[test]
    fn clones_share_the_same_time_source() {
        // 这正是"服务持有时钟、测试仍能推进时间"所依赖的性质。
        let cell = Cell::new(Timepoint::ZERO);
        let clock = ManualClock::new(&cell);
        let handed_to_service = clock;
        clock.advance(Duration::from_millis(5));
        assert_eq!(handed_to_service.now(), clock.now());
    }

    #[test]
    fn can_be_positioned_near_wraparound() {
        let cell = Cell::new(Timepoint::from_micros(u64::MAX - 100));
        let clock = ManualClock::new(&cell);
        let before = clock.now();
        clock.advance(Duration::from_micros(1_000));
        assert!(clock.now().is_after(before));
        assert!(clock.now().as_micros() < before.as_micros(), "必须确实回绕");
    }
}
