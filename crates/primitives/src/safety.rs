//! 健康门控与安全状态机（通用平台逻辑，不含任何具体设备的语义）。
//!
//! 对应平台需求（见 `docs/01-需求/需求规格.md`）：
//!
//! | 需求 | 机制 | 本模块提供 |
//! |------|------|-----------|
//! | SF-3 链路不可信不得继续驱动输出 | 失联优先的"与"判据，且 `Safe` 不自动恢复 | [`SafetySupervisor::outputs_permitted_with_link`] |
//! | SF-4 任务级健康门控 | 全部必需健康位到齐才允许喂狗 | [`all_healthy`] / [`HealthBits`] |
//! | SF-5 连续复位闭环 | 连续多次复位后进入只跑诊断的安全状态 | [`SafetySupervisor::on_boot`] |
//!
//! # 状态迁移
//!
//! ```text
//! Normal ──degrade()──▶ Degraded ──enter_safe()──▶ Safe
//!    ▲                     │                        │
//!    └── on_selfcheck(true) ×RECOVERY_STREAK ───────┘
//! ```
//!
//! `Safe` **不参与自动恢复**：只有重新上电或显式维护动作才离开。原因是
//! "安全状态被自动解除"本身就是一类危险失效——现场必须能解释为什么会进去。

/// 每个硬实时任务上报自己的健康位。全部必需位到齐才允许喂狗。
///
/// 位号由**应用层**约定：这里只预置平台自身的两个位；应用可以自行追加
/// 高位（例如每路总线一个接收位），并把"必需集合"作为常量传给
/// [`all_healthy`]。
pub mod health {
    /// 1 ms 截止时刻推进任务。
    pub const DEADLINE_TICK: u32 = 1 << 0;
    /// 空闲（主循环）任务。
    pub const IDLE: u32 = 1 << 1;
}

/// 健康位是否到齐（纯函数，便于在 ISR 与主机测试中共用）。
///
/// `required` 由应用给出："哪些位必须到齐"是应用的设计决定，
/// 平台只提供判定机制（**平台需求 SF-4**）。
#[must_use]
pub const fn all_healthy(reported: u32, required: u32) -> bool {
    reported & required == required
}

/// 任务级健康位的累积器。
///
/// 目标端会用 `AtomicU32` 实现同一策略（ISR 里只 `fetch_or`，空闲任务里
/// `swap(0)` 后判定）；本类型提供可在主机上穷举验证的策略实现，
/// 让"门控逻辑本身是对的"这件事与硬件无关。
#[derive(Debug, Clone, Copy, Default)]
pub struct HealthBits {
    reported: u32,
}

impl HealthBits {
    /// 空。
    #[must_use]
    pub const fn new() -> Self {
        Self { reported: 0 }
    }

    /// 上报一个任务的健康位。
    pub fn report(&mut self, bits: u32) {
        self.reported |= bits;
    }

    /// 当前累积值。
    #[must_use]
    pub const fn current(&self) -> u32 {
        self.reported
    }

    /// 是否到齐（按给定必需集合）。
    #[must_use]
    pub const fn all_healthy(&self, required: u32) -> bool {
        all_healthy(self.reported, required)
    }

    /// 读出并清零（喂狗的原子动作）。
    pub fn read_and_clear(&mut self) -> u32 {
        let value = self.reported;
        self.reported = 0;
        value
    }
}

/// 降级原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DegradeReason {
    /// 总线错误率超过阈值。
    BusErrorRate,
    /// 存储校验失败但可回退到安全默认。
    StoreChecksum,
    /// 截止时刻被错过。
    DeadlineMissed,
    /// 实测 ISR 执行时间超预算。
    IsrBudgetExceeded,
    /// 协议层出现异常报文（CRC 连续失败、越界字段等）。
    ProtocolAnomaly,
}

/// 进入安全状态的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SafeReason {
    /// 与外部链路的失联超过安全时限。
    LinkLost,
    /// 存储双份均损坏，无法获得可信配置。
    StoreCorrupt,
    /// 连续多次复位，怀疑存在不可恢复的失效。
    RepeatedResets,
}

/// 安全状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyState {
    /// 正常。
    Normal,
    /// 降级：功能受限但继续服务，并持续上报原因。
    Degraded(DegradeReason),
    /// 安全状态：撤除有风险的输出，只保留可观测性。
    Safe(SafeReason),
}

impl SafetyState {
    /// 是否已进入 `Safe`（供固件/上层快速判断；`Safe` 不自动恢复）。
    #[must_use]
    pub const fn is_safe(self) -> bool {
        matches!(self, Self::Safe(_))
    }
}

/// 连续多少次自检通过才允许回到 `Normal`。
pub const RECOVERY_STREAK: u8 = 3;

/// 连续多少次复位后进入安全状态。
pub const MAX_CONSECUTIVE_RESETS: u8 = 3;

/// 安全监督器。
#[derive(Debug, Clone)]
pub struct SafetySupervisor {
    state: SafetyState,
    recovery_streak: u8,
    consecutive_resets: u8,
}

impl Default for SafetySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl SafetySupervisor {
    /// 初始为 `Normal`。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: SafetyState::Normal,
            recovery_streak: 0,
            consecutive_resets: 0,
        }
    }

    /// 当前状态。
    #[must_use]
    pub const fn state(&self) -> SafetyState {
        self.state
    }

    /// 复位计账。返回 `Some(reason)` 表示本次上电应直接进入安全状态。
    ///
    /// 台账记录由跨暖复位的崩溃记录承担（见 `fault-decode` 与 BSP 的 `.uninit` 区）；
    /// 本函数只做判定。**平台需求 SF-5。**
    pub fn on_boot(&mut self) -> Option<SafeReason> {
        self.consecutive_resets = self.consecutive_resets.saturating_add(1);
        if self.consecutive_resets >= MAX_CONSECUTIVE_RESETS {
            self.state = SafetyState::Safe(SafeReason::RepeatedResets);
            return Some(SafeReason::RepeatedResets);
        }
        None
    }

    /// 清空复位计数（例如运行满一定时长后由空闲任务调用）。
    pub fn clear_reset_streak(&mut self) {
        self.consecutive_resets = 0;
    }

    /// 降级。**不会**把 `Safe` 拉回 `Degraded`。
    pub fn degrade(&mut self, reason: DegradeReason) -> SafetyState {
        if !matches!(self.state, SafetyState::Safe(_)) {
            self.state = SafetyState::Degraded(reason);
            self.recovery_streak = 0;
        }
        self.state
    }

    /// 进入安全状态。这是单向的。
    pub fn enter_safe(&mut self, reason: SafeReason) -> SafetyState {
        self.state = SafetyState::Safe(reason);
        self.recovery_streak = 0;
        self.state
    }

    /// 自检结果。连续 [`RECOVERY_STREAK`] 次通过才从 `Degraded` 回到 `Normal`。
    pub fn on_selfcheck(&mut self, passed: bool) -> SafetyState {
        match self.state {
            SafetyState::Safe(_) => {}
            SafetyState::Degraded(reason) => {
                if passed {
                    self.recovery_streak = self.recovery_streak.saturating_add(1);
                    if self.recovery_streak >= RECOVERY_STREAK {
                        self.state = SafetyState::Normal;
                        self.recovery_streak = 0;
                    }
                } else {
                    self.recovery_streak = 0;
                    self.state = SafetyState::Degraded(reason);
                }
            }
            SafetyState::Normal => {
                if !passed {
                    self.state = SafetyState::Degraded(DegradeReason::StoreChecksum);
                }
            }
        }
        self.state
    }

    /// **SF-3**：是否允许驱动安全相关的输出。
    ///
    /// 判定是"与"关系，且**失联优先**：只要外部链路不可信，就不允许驱动输出。
    #[must_use]
    pub const fn outputs_permitted_with_link(&self, link_alive: bool) -> bool {
        link_alive && !matches!(self.state, SafetyState::Safe(_))
    }

    /// 是否还允许驱动安全相关输出（供上层快速判断）。
    #[must_use]
    pub const fn outputs_permitted(&self) -> bool {
        !matches!(self.state, SafetyState::Safe(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 平台需求 SF-4：必需位到齐才算健康。
    #[test]
    fn sf_4_all_healthy_requires_every_required_bit() {
        let required = health::DEADLINE_TICK | health::IDLE;
        assert!(!all_healthy(0, required));
        assert!(!all_healthy(health::DEADLINE_TICK, required));
        assert!(all_healthy(required, required));
        assert!(
            all_healthy(required | 0xF000_0000, required),
            "多余位不影响判定"
        );
    }

    #[test]
    fn health_bits_read_and_clear_is_atomic_in_effect() {
        let required = health::DEADLINE_TICK | health::IDLE;
        let mut bits = HealthBits::new();
        bits.report(health::DEADLINE_TICK);
        assert!(!bits.all_healthy(required));
        bits.report(health::IDLE);
        assert!(bits.all_healthy(required));
        assert_eq!(bits.read_and_clear(), required);
        assert_eq!(bits.current(), 0);
        assert!(
            !bits.all_healthy(required),
            "清零后必须重新累积，否则挂死检测失效"
        );
    }

    /// 平台需求 SF-3：失联优先，即使状态机处于 Normal 也不允许输出。
    #[test]
    fn sf_3_outputs_are_refused_when_link_is_lost() {
        let sup = SafetySupervisor::new();
        assert_eq!(sup.state(), SafetyState::Normal);
        assert!(sup.outputs_permitted_with_link(true));
        assert!(
            !sup.outputs_permitted_with_link(false),
            "SF-3：失联即拒绝输出"
        );
    }

    #[test]
    fn safe_state_refuses_outputs_even_with_live_link() {
        let mut sup = SafetySupervisor::new();
        sup.enter_safe(SafeReason::LinkLost);
        assert!(!sup.outputs_permitted_with_link(true));
        assert!(!sup.outputs_permitted());
    }

    #[test]
    fn degrade_then_recover_requires_consecutive_successes() {
        let mut sup = SafetySupervisor::new();
        sup.degrade(DegradeReason::BusErrorRate);
        assert_eq!(
            sup.state(),
            SafetyState::Degraded(DegradeReason::BusErrorRate)
        );

        // 中间夹一次失败会清零连击。
        assert_eq!(
            sup.on_selfcheck(true),
            SafetyState::Degraded(DegradeReason::BusErrorRate)
        );
        assert_eq!(
            sup.on_selfcheck(false),
            SafetyState::Degraded(DegradeReason::BusErrorRate)
        );

        for _ in 0..(RECOVERY_STREAK - 1) {
            assert!(matches!(sup.on_selfcheck(true), SafetyState::Degraded(_)));
        }
        assert_eq!(sup.on_selfcheck(true), SafetyState::Normal);
    }

    #[test]
    fn safe_state_is_not_left_automatically() {
        let mut sup = SafetySupervisor::new();
        sup.enter_safe(SafeReason::StoreCorrupt);
        for _ in 0..10 {
            sup.on_selfcheck(true);
        }
        assert!(matches!(sup.state(), SafetyState::Safe(_)));
    }

    #[test]
    fn degrade_cannot_downgrade_safe() {
        let mut sup = SafetySupervisor::new();
        sup.enter_safe(SafeReason::LinkLost);
        assert_eq!(
            sup.degrade(DegradeReason::DeadlineMissed),
            SafetyState::Safe(SafeReason::LinkLost)
        );
    }

    /// 平台需求 SF-5：连续复位进入安全状态且不自动恢复。
    #[test]
    fn sf_5_repeated_resets_eventually_force_safe_state() {
        let mut sup = SafetySupervisor::new();
        for _ in 0..(MAX_CONSECUTIVE_RESETS - 1) {
            assert_eq!(sup.on_boot(), None);
        }
        assert_eq!(sup.on_boot(), Some(SafeReason::RepeatedResets));
        assert_eq!(sup.state(), SafetyState::Safe(SafeReason::RepeatedResets));
    }

    #[test]
    fn a_long_healthy_run_clears_the_reset_streak() {
        let mut sup = SafetySupervisor::new();
        assert_eq!(sup.on_boot(), None);
        assert_eq!(sup.on_boot(), None);
        sup.clear_reset_streak();
        assert_eq!(sup.on_boot(), None, "清零后不应累计到安全状态");
    }
}
