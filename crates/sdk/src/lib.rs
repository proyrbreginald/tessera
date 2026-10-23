//! 业务 capsule 的稳定 SDK 门面：`Capsule` 契约与静态组合根。
//!
//! 本 crate 是纯逻辑层：不依赖 RTIC/寄存器，业务 capsule 可以在主机上复用同一套
//! 契约。组合根由产品固件在编译期提供。

#![no_std]
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

/// capsule 返回的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsuleError {
    /// 依赖尚未就绪（稍后重试）。
    NotReady,
    /// 输入/状态被拒绝。
    Rejected,
}

/// capsule 在组合根中的执行阶段。
///
/// 阶段由 capsule 自己声明，不由注册顺序决定：终端输入必须先于后台维护，
/// 健康聚合必须看到本轮维护结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsulePhase {
    /// 输入处理，优先级最高。
    Input,
    /// 后台维护 / 存储推进。
    Maintenance,
    /// 健康聚合，最后执行。
    Health,
}

impl CapsulePhase {
    /// 固定执行顺序。
    pub const ALL: &'static [Self] = &[Self::Input, Self::Maintenance, Self::Health];
}

/// 注册失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// 容量已满。
    Full,
    /// 同名 capsule 已存在。
    DuplicateName,
}

/// 单次推进的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsuleStep {
    /// 本轮没有工作。
    Idle,
    /// 本轮做了工作（组合根应避免立即睡眠）。
    Worked,
    /// 本轮失败（仍计入 `failed`，由组合根决定如何处置）。
    Failed,
    /// 本轮有未完成工作，但主动让出；组合根不应把它当成 `Worked` 忙等。
    Deferred,
}

/// capsule 本轮是否做了工作。
impl CapsuleStep {
    /// 是否应当避免睡眠。
    #[must_use]
    pub const fn worked(self) -> bool {
        matches!(self, Self::Worked)
    }
}

/// 单个 capsule 连续失败多少次后隔离。
const MAX_STEP_FAILURES: u32 = 3;

/// 单阶段推进报告。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepReport {
    /// 是否做过工作。
    pub worked: bool,
    /// 本阶段失败个数。
    pub failed: usize,
}

/// 业务 capsule 契约。
///
/// capsule 是被动状态机：由组合根在固定上下文调用，不注册中断、不阻塞。
pub trait Capsule<C> {
    /// 稳定名字（诊断/健康上报用）。
    fn name(&self) -> &'static str;

    /// 执行阶段；默认后台维护。
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Maintenance
    }

    /// 初始化；失败时组合根必须把它标记为不可用，而不是假装成功。
    ///
    /// # Errors
    ///
    /// `NotReady` 可重试，`Rejected` 表示配置/状态不可接受。
    fn init(&mut self) -> Result<(), CapsuleError>;

    /// 周期推进；必须快速返回，并报告本轮是否做了工作。
    fn step(&mut self, ctx: &mut C) -> CapsuleStep;
}

/// 静态组合根：固定容量的 capsule 引用表。
pub struct CapsuleSet<'a, C, const N: usize> {
    slots: [Option<Slot<'a, C>>; N],
    len: usize,
    init_failures: usize,
    step_failures: usize,
    register_errors: usize,
}

struct Slot<'a, C> {
    capsule: &'a mut dyn Capsule<C>,
    phase: CapsulePhase,
    init_failed: bool,
    step_failures: u32,
    quarantined: bool,
}

impl<'a, C, const N: usize> CapsuleSet<'a, C, N> {
    /// 空组合根。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            len: 0,
            init_failures: 0,
            step_failures: 0,
            register_errors: 0,
        }
    }

    /// 注册一个 capsule；容量不足或同名重复返回错误。
    ///
    /// # Errors
    ///
    /// `Full` 或 `DuplicateName`；失败也计入 [`Self::register_errors`]。
    pub fn register(&mut self, capsule: &'a mut dyn Capsule<C>) -> Result<(), RegisterError> {
        if self.len >= N {
            self.register_errors = self.register_errors.saturating_add(1);
            return Err(RegisterError::Full);
        }
        let name = capsule.name();
        for existing in self.slots.iter().take(self.len).flatten() {
            if existing.capsule.name() == name {
                self.register_errors = self.register_errors.saturating_add(1);
                return Err(RegisterError::DuplicateName);
            }
        }
        let phase = capsule.phase();
        if let Some(slot) = self.slots.get_mut(self.len) {
            *slot = Some(Slot {
                capsule,
                phase,
                init_failed: false,
                step_failures: 0,
                quarantined: false,
            });
            self.len += 1;
            return Ok(());
        }
        self.register_errors = self.register_errors.saturating_add(1);
        Err(RegisterError::Full)
    }

    /// 初始化全部 capsule；返回失败个数。
    ///
    /// 初始化失败的 capsule 会被标记并**跳过后续 step**，不会每轮重复调用。
    pub fn init_all(&mut self) -> usize {
        let mut failed = 0usize;
        for slot in self.slots.iter_mut().take(self.len).flatten() {
            if slot.init_failed {
                failed += 1;
                continue;
            }
            if slot.capsule.init().is_err() {
                slot.init_failed = true;
                failed += 1;
            }
        }
        self.init_failures = failed;
        failed
    }

    /// 推进指定阶段；只调用该阶段、且初始化成功的 capsule。
    pub fn step_phase(&mut self, phase: CapsulePhase, ctx: &mut C) -> StepReport {
        let mut report = StepReport::default();
        for slot in self.slots.iter_mut().take(self.len).flatten() {
            if slot.init_failed || slot.quarantined || slot.phase != phase {
                continue;
            }
            match slot.capsule.step(ctx) {
                CapsuleStep::Worked => report.worked = true,
                CapsuleStep::Deferred => {}
                CapsuleStep::Failed => {
                    report.failed = report.failed.saturating_add(1);
                    slot.step_failures = slot.step_failures.saturating_add(1);
                    if slot.step_failures >= MAX_STEP_FAILURES {
                        slot.quarantined = true;
                    }
                }
                CapsuleStep::Idle => {}
            }
        }
        self.step_failures = self.step_failures.saturating_add(report.failed);
        report
    }

    /// 按 Input → Maintenance → Health 顺序推进全部 phase。
    pub fn step_all(&mut self, ctx: &mut C) -> StepReport {
        let mut total = StepReport::default();
        for phase in CapsulePhase::ALL {
            let report = self.step_phase(*phase, ctx);
            total.worked |= report.worked;
            total.failed = total.failed.saturating_add(report.failed);
        }
        total
    }

    /// 最近一次初始化失败数。
    #[must_use]
    pub const fn init_failures(&self) -> usize {
        self.init_failures
    }

    /// 累计 step 失败数。
    #[must_use]
    pub const fn step_failures(&self) -> usize {
        self.step_failures
    }

    /// 已被隔离的 capsule 数（连续失败达到阈值）。
    #[must_use]
    pub fn quarantined(&self) -> usize {
        self.slots
            .iter()
            .take(self.len)
            .flatten()
            .filter(|slot| slot.quarantined)
            .count()
    }

    /// 注册失败累计数。
    #[must_use]
    pub const fn register_errors(&self) -> usize {
        self.register_errors
    }

    /// 已注册数量。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 单个 capsule 的累计失败数。
    #[must_use]
    pub fn capsule_failures(&self, name: &str) -> Option<u32> {
        self.slots
            .iter()
            .take(self.len)
            .flatten()
            .find(|slot| slot.capsule.name() == name)
            .map(|slot| slot.step_failures)
    }
}

impl<C, const N: usize> Default for CapsuleSet<'_, C, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// 产品组合根：给一个固定名称的 capsule 集合，SDK 侧的产品模板。
pub struct Product<'a, C, const N: usize> {
    name: &'static str,
    capsules: CapsuleSet<'a, C, N>,
}

/// 产品运行状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductStatus {
    /// 产品名。
    pub name: &'static str,
    /// 已注册 capsule 数。
    pub registered: usize,
    /// 初始化失败数。
    pub init_failures: usize,
    /// 累计 step 失败数。
    pub step_failures: usize,
    /// 已隔离 capsule 数。
    pub quarantined: usize,
    /// 注册失败数。
    pub register_errors: usize,
}

impl<'a, C, const N: usize> Product<'a, C, N> {
    /// 新建空产品。
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            capsules: CapsuleSet::new(),
        }
    }

    /// 注册 capsule。
    ///
    /// # Errors
    ///
    /// 容量不足或同名重复。
    pub fn register(&mut self, capsule: &'a mut dyn Capsule<C>) -> Result<(), RegisterError> {
        self.capsules.register(capsule)
    }

    /// 初始化全部 capsule。
    pub fn init_all(&mut self) -> usize {
        self.capsules.init_all()
    }

    /// 按 phase 推进。
    pub fn step_phase(&mut self, phase: CapsulePhase, ctx: &mut C) -> StepReport {
        self.capsules.step_phase(phase, ctx)
    }

    /// 按全部 phase 推进。
    pub fn step_all(&mut self, ctx: &mut C) -> StepReport {
        self.capsules.step_all(ctx)
    }

    /// 注册失败累计数。
    #[must_use]
    pub const fn register_errors(&self) -> usize {
        self.capsules.register_errors()
    }

    /// 已隔离 capsule 数。
    #[must_use]
    pub fn quarantined(&self) -> usize {
        self.capsules.quarantined()
    }

    /// 状态快照。
    #[must_use]
    pub fn status(&self) -> ProductStatus {
        ProductStatus {
            name: self.name,
            registered: self.capsules.len(),
            init_failures: self.capsules.init_failures(),
            step_failures: self.capsules.step_failures(),
            quarantined: self.capsules.quarantined(),
            register_errors: self.capsules.register_errors(),
        }
    }

    /// 已注册数量。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.capsules.len()
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.capsules.is_empty()
    }

    /// 已注册 capsule 的名字表（按注册顺序；诊断/启动自报用）。
    #[must_use]
    pub fn names(&self) -> [Option<&'static str>; N] {
        names(&self.capsules)
    }
}

/// capsule 名字表（诊断用，零分配）。
pub fn names<C, const N: usize>(set: &CapsuleSet<'_, C, N>) -> [Option<&'static str>; N] {
    let mut names = [None; N];
    for (index, slot) in set.slots.iter().take(set.len).enumerate() {
        if let Some(slot) = slot
            && let Some(out) = names.get_mut(index)
        {
            *out = Some(slot.capsule.name());
        }
    }
    names
}

/// 业务接入常用门面：只暴露稳定契约，不暴露底座内部模块。
pub mod prelude {
    pub use crate::{
        Capsule, CapsuleError, CapsulePhase, CapsuleSet, CapsuleStep, Product, ProductStatus,
        RegisterError, StepReport,
    };
    pub use ports::{bus::Bus, clock::Clock, log::LogSink, raw::RawMedium, store::Store};
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        init_ok: bool,
        step_ok: bool,
    }

    impl Capsule<()> for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn init(&mut self) -> Result<(), CapsuleError> {
            if self.init_ok {
                Ok(())
            } else {
                Err(CapsuleError::Rejected)
            }
        }
        fn step(&mut self, _ctx: &mut ()) -> CapsuleStep {
            if self.step_ok {
                CapsuleStep::Idle
            } else {
                CapsuleStep::Failed
            }
        }
    }

    #[test]
    fn 静态组合根注册与失败计数() {
        let mut good = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut bad = Fake {
            init_ok: false,
            step_ok: false,
        };
        let mut set = CapsuleSet::<(), 1>::new();
        assert!(set.register(&mut good).is_ok());
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
        assert_eq!(set.init_all(), 0);
        let report = set.step_all(&mut ());
        assert!(!report.worked);
        assert_eq!(report.failed, 0);
        assert!(set.register(&mut bad).is_err());
        assert_eq!(set.register_errors(), 1);
        assert_eq!(names(&set), [Some("fake")]);
    }

    #[test]
    fn 同名_capsule_不能重复注册() {
        let mut one = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut two = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut set = CapsuleSet::<(), 2>::new();
        assert!(set.register(&mut one).is_ok());
        assert!(set.register(&mut two).is_err());
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn 初始化与推进失败都可见() {
        let mut bad = Fake {
            init_ok: false,
            step_ok: false,
        };
        let mut set = CapsuleSet::<(), 2>::new();
        assert!(set.register(&mut bad).is_ok());
        assert_eq!(set.init_all(), 1);
        assert_eq!(set.init_failures(), 1);
        // 初始化失败后不得继续 step，避免每轮重复失败。
        let report = set.step_all(&mut ());
        assert_eq!(set.step_failures(), 0);
        assert!(!report.worked);
        assert_eq!(report.failed, 0);
        assert!(CapsuleSet::<(), 0>::new().is_empty());
    }
    struct Phased {
        name: &'static str,
        phase: CapsulePhase,
        mark: u8,
    }

    impl Capsule<[u8; 3]> for Phased {
        fn name(&self) -> &'static str {
            self.name
        }
        fn phase(&self) -> CapsulePhase {
            self.phase
        }
        fn init(&mut self) -> Result<(), CapsuleError> {
            Ok(())
        }
        fn step(&mut self, ctx: &mut [u8; 3]) -> CapsuleStep {
            let index = match self.phase {
                CapsulePhase::Input => 0,
                CapsulePhase::Maintenance => 1,
                CapsulePhase::Health => 2,
            };
            if let Some(slot) = ctx.get_mut(index) {
                *slot = self.mark;
            }
            CapsuleStep::Worked
        }
    }

    #[test]
    fn 阶段顺序不依赖注册顺序() {
        let mut health = Phased {
            name: "health",
            phase: CapsulePhase::Health,
            mark: b'H',
        };
        let mut input = Phased {
            name: "input",
            phase: CapsulePhase::Input,
            mark: b'I',
        };
        let mut maintenance = Phased {
            name: "maintenance",
            phase: CapsulePhase::Maintenance,
            mark: b'M',
        };
        let mut set = CapsuleSet::<[u8; 3], 3>::new();
        assert!(set.register(&mut health).is_ok());
        assert!(set.register(&mut input).is_ok());
        assert!(set.register(&mut maintenance).is_ok());
        let mut ctx = [0u8; 3];
        let report = set.step_all(&mut ctx);
        assert_eq!(ctx, [b'I', b'M', b'H']);
        assert!(report.worked);
        assert_eq!(report.failed, 0);
    }

    #[test]
    fn 连续失败达到阈值后隔离() {
        let mut bad = Fake {
            init_ok: true,
            step_ok: false,
        };
        let mut set = CapsuleSet::<(), 1>::new();
        assert!(set.register(&mut bad).is_ok());
        assert_eq!(set.init_all(), 0);
        for _ in 0..MAX_STEP_FAILURES {
            let report = set.step_all(&mut ());
            assert_eq!(report.failed, 1);
        }
        assert_eq!(set.quarantined(), 1);
        let report = set.step_all(&mut ());
        assert_eq!(report.failed, 0);
        assert!(!report.worked);
    }

    #[test]
    fn 产品按注册顺序报告_capsule_名字() {
        let mut one = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut two = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut product = Product::<(), 2>::new("t");
        assert!(product.register(&mut one).is_ok());
        assert!(product.register(&mut two).is_err());
        assert_eq!(product.names(), [Some("fake"), None]);
    }

    #[test]
    fn 容量满与重复名都计入注册错误() {
        let mut one = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut two = Fake {
            init_ok: true,
            step_ok: true,
        };
        let mut set = CapsuleSet::<(), 1>::new();
        assert!(set.register(&mut one).is_ok());
        assert_eq!(set.register(&mut two), Err(RegisterError::Full));
        assert_eq!(set.register_errors(), 1);
    }
}
