//! 默认产品组合根：把平台注入的基础 capsule 装配成 [`sdk::Product`]。
//!
//! # 为什么需要这个 crate
//!
//! 组合根是"产品包含哪些 capsule、按什么顺序注册、容量多大"的**唯一决定处**。
//! 它如果留在 `firmware` 里，业务方想换一组 capsule 就必须改底座源码；把它外提成
//! 独立 crate 后，`firmware` 只负责实现 [`Platform`] 并把服务实例借出来。
//!
//! # 依赖方向（循环依赖的解）
//!
//! ```text
//! firmware ──▶ product ──▶ sdk ──▶ ports
//! ```
//!
//! 这个 crate **只依赖 `sdk`**：不依赖 `firmware`、不依赖 `board-support`、不认识任何
//! 具体服务类型。平台通过 [`Platform`] 注入 `&mut dyn Capsule<C>`，`C` 对组合根是
//! 不透明泛型参数 —— 因此不会出现 `firmware → product → firmware` 的循环依赖。
//!
//! # 扩展边界（诚实口径）
//!
//! 本 crate 只外提**组合策略**：集合、顺序、容量、产品名。它**不**解决以下三件事，
//! 因此"仓库外业务 capsule 只需换一个依赖就能接入"在 S1 **不成立**：
//!
//! 1. **所有权**：`sdk::Product` 借用 capsule（`&'a mut dyn Capsule<C>`），不拥有它。
//!    业务 capsule 实例必须由**调用方**持有并活过组合根；在本 crate 的函数里
//!    构造局部实例再 `register` 会因为生命周期不足而编译失败（E0597）。
//!    当前唯一可行的组合点是持有 idle 局部变量的那一层。
//! 2. **能力**：`sdk::Capsule<C>` 的 `C` 仍是泛型参数，`sdk` 也没有能力句柄层；
//!    仓库外 capsule 只能写 `impl<C> Capsule<C>` 这种**无能力**实现。
//! 3. **基础 capsule 的实现**：六个服务仍物理位于 `firmware`（要经 `board-support` 接触硬件）。
//!
//! 上面 1/2 属于 S2（上下文能力对象化），3 属于 S3（基础 capsule 外提）。
//! 见 `docs/02-架构/产品组合根外提设计.md` 与 ADR-0010。
//!
//! S1 真正可做的扩展：替换默认产品的注册顺序/容量，或在自己的产品 crate 里
//! 用 [`build`] 组装平台六槽并追加**由调用方持有**的 capsule。

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

use sdk::{Capsule, Product, RegisterError};

/// 默认产品名（诊断与健康上报用）。
pub const PRODUCT_NAME: &str = "tessera";

/// 平台必须提供的基础 capsule 数量。
///
/// 与 [`PlatformServices`] 的字段数一一对应：改这个常量就要同时改结构体，
/// 编译器会强制两处一致（少一个字段 `build` 就编译不过）。
pub const BASE_CAPSULES: usize = 6;

/// 预留给业务 capsule 的槽位数。
///
/// 容量是产品决定：默认产品给业务留 2 个槽。业务产品可以不用 [`CAPSULE_SLOTS`]
/// 而自己给 `build` 传更大的 `N`。
pub const BUSINESS_SLOTS: usize = 2;

/// 默认产品的总容量（基础 + 业务预留）。
pub const CAPSULE_SLOTS: usize = BASE_CAPSULES + BUSINESS_SLOTS;

/// 平台向组合根注入的基础服务视图。
///
/// 所有服务都以 `&mut dyn Capsule<C>` 出现：组合根只关心"它是哪个阶段、叫什么名字"，
/// **不关心它的具体类型**，因此不需要依赖任何实现 crate。
pub struct PlatformServices<'a, C> {
    /// 终端输入（`CapsulePhase::Input`）。
    pub terminal: &'a mut dyn Capsule<C>,
    /// 文件系统写队列推进（`CapsulePhase::Maintenance`）。
    pub fs: &'a mut dyn Capsule<C>,
    /// 文本日志落盘（`CapsulePhase::Maintenance`）。
    pub log: &'a mut dyn Capsule<C>,
    /// 活动日志轮转（`CapsulePhase::Maintenance`）。
    pub rotate: &'a mut dyn Capsule<C>,
    /// 健康聚合（`CapsulePhase::Health`）。
    pub health: &'a mut dyn Capsule<C>,
    /// 后台维护队列（`CapsulePhase::Maintenance`）。
    pub maintenance: &'a mut dyn Capsule<C>,
}

/// 平台（core kernel）向产品组合根提供服务的契约。
///
/// 实现者只需把已构造好的服务实例借出来；**不要**在这里做初始化或 I/O ——
/// 组合根会在注册后立即调用 `init_all`。
pub trait Platform {
    /// 单次 `step` 的上下文类型。
    ///
    /// 组合根不解释它，也不需要它是公开类型；它只被原样传回 capsule。
    type Context;

    /// 借出基础服务。一次产品构建只调用一次。
    fn services(&mut self) -> PlatformServices<'_, Self::Context>;
}

/// 组合根构建失败。
///
/// `#[non_exhaustive]`：S3 之后可能出现新的失败原因（例如基础 capsule 外提后的构造失败），
/// 提前给下游留出演化空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildError {
    /// 基础 capsule 注册失败：容量不足或同名重复。
    ///
    /// **不得**把它当成"少几个 capsule 也能跑"：组合根必须把它当启动失败上报。
    Register(RegisterError),
}

/// 按固定顺序装配默认产品（产品名 [`PRODUCT_NAME`]）。
///
/// 等价于 [`build_named`]`(PRODUCT_NAME, platform)`。
///
/// # Errors
///
/// 容量不足或出现同名 capsule 时返回 [`BuildError::Register`]。
pub fn build<'a, P, const N: usize>(
    platform: &'a mut P,
) -> Result<Product<'a, P::Context, N>, BuildError>
where
    P: Platform,
{
    build_named(PRODUCT_NAME, platform)
}

/// 按固定顺序装配具名产品。
///
/// 注册顺序：`terminal → fs → log → rotate → health → maintenance`。
/// 跨阶段顺序由 [`sdk::CapsulePhase`] 决定，不依赖注册顺序；这里固定同阶段内的顺序，
/// 让"终端输入优先于后台维护"成为可读的产品决定。
///
/// # 调用方必须在注册后立刻初始化
///
/// 本函数**只注册、不调用 `init_all`**。`sdk::CapsuleSet` 在没调用过 `init_all` 时
/// 会把所有 capsule 当成"已初始化"并直接 `step`；因此调用方忽略初始化错误
/// 不会得到报错，只会让 capsule 在未初始化状态下运行。
///
/// # Errors
///
/// 容量不足或出现同名 capsule 时返回 [`BuildError::Register`]；
/// 失败时不返回半成品。
pub fn build_named<'a, P, const N: usize>(
    name: &'static str,
    platform: &'a mut P,
) -> Result<Product<'a, P::Context, N>, BuildError>
where
    P: Platform,
{
    let PlatformServices {
        terminal,
        fs,
        log,
        rotate,
        health,
        maintenance,
    } = platform.services();
    // 数组长度由 `BASE_CAPSULES` 锁死：漏写/多写一个服务都是编译错误。
    let services: [&'a mut dyn Capsule<P::Context>; BASE_CAPSULES] =
        [terminal, fs, log, rotate, health, maintenance];
    let mut product = Product::new(name);
    for capsule in services {
        product.register(capsule).map_err(BuildError::Register)?;
    }
    Ok(product)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{CapsuleError, CapsulePhase, CapsuleStep};
    use std::cell::RefCell;
    use std::vec::Vec;

    /// 记录自己何时被推进的假 capsule。
    struct Fake<'a> {
        name: &'static str,
        phase: CapsulePhase,
        trace: &'a RefCell<Vec<&'static str>>,
    }

    impl<C> Capsule<C> for Fake<'_> {
        fn name(&self) -> &'static str {
            self.name
        }

        fn phase(&self) -> CapsulePhase {
            self.phase
        }

        fn init(&mut self) -> Result<(), CapsuleError> {
            Ok(())
        }

        fn step(&mut self, _ctx: &mut C) -> CapsuleStep {
            self.trace.borrow_mut().push(self.name);
            CapsuleStep::Worked
        }
    }

    struct FakePlatform<'a> {
        terminal: Fake<'a>,
        fs: Fake<'a>,
        log: Fake<'a>,
        rotate: Fake<'a>,
        health: Fake<'a>,
        maintenance: Fake<'a>,
    }

    impl<'a> FakePlatform<'a> {
        fn new(trace: &'a RefCell<Vec<&'static str>>) -> Self {
            Self {
                terminal: Fake {
                    name: "terminal",
                    phase: CapsulePhase::Input,
                    trace,
                },
                fs: Fake {
                    name: "fs",
                    phase: CapsulePhase::Maintenance,
                    trace,
                },
                log: Fake {
                    name: "log",
                    phase: CapsulePhase::Maintenance,
                    trace,
                },
                rotate: Fake {
                    name: "log-rotate",
                    phase: CapsulePhase::Maintenance,
                    trace,
                },
                health: Fake {
                    name: "health",
                    phase: CapsulePhase::Health,
                    trace,
                },
                maintenance: Fake {
                    name: "maintenance",
                    phase: CapsulePhase::Maintenance,
                    trace,
                },
            }
        }
    }

    impl Platform for FakePlatform<'_> {
        type Context = ();

        fn services(&mut self) -> PlatformServices<'_, Self::Context> {
            PlatformServices {
                terminal: &mut self.terminal,
                fs: &mut self.fs,
                log: &mut self.log,
                rotate: &mut self.rotate,
                health: &mut self.health,
                maintenance: &mut self.maintenance,
            }
        }
    }

    #[test]
    fn 默认产品按阶段与固定顺序装配六个基础_capsule() {
        let trace = RefCell::new(Vec::new());
        let mut platform = FakePlatform::new(&trace);
        let built = build::<_, CAPSULE_SLOTS>(&mut platform);
        assert!(built.is_ok(), "默认容量必须装得下六个基础 capsule");
        let Ok(mut product) = built else {
            return;
        };
        assert_eq!(product.len(), BASE_CAPSULES);
        assert!(!product.is_empty());
        assert_eq!(product.init_all(), 0);
        let report = product.step_all(&mut ());
        assert!(report.worked);
        assert_eq!(report.failed, 0);
        // Input 先于 Maintenance，Health 最后；同阶段内按注册顺序。
        assert_eq!(
            trace.borrow().as_slice(),
            [
                "terminal",
                "fs",
                "log",
                "log-rotate",
                "maintenance",
                "health"
            ]
        );
    }

    #[test]
    fn 容量不足时构建失败而不是静默少注册() {
        let trace = RefCell::new(Vec::new());
        let mut platform = FakePlatform::new(&trace);
        // 5 个槽装不下 6 个基础 capsule。
        let result = build::<_, 5>(&mut platform);
        assert!(matches!(
            result,
            Err(BuildError::Register(RegisterError::Full))
        ));
    }

    #[test]
    fn 预留给业务的槽位可以被外层产品继续注册() {
        let trace = RefCell::new(Vec::new());
        let mut platform = FakePlatform::new(&trace);
        let built = build::<_, CAPSULE_SLOTS>(&mut platform);
        assert!(built.is_ok(), "默认容量必须装得下六个基础 capsule");
        let Ok(mut product) = built else {
            return;
        };
        assert_eq!(product.len(), BASE_CAPSULES);
        let mut business = Fake {
            name: "business",
            phase: CapsulePhase::Maintenance,
            trace: &trace,
        };
        assert!(product.register(&mut business).is_ok());
        assert_eq!(product.len(), BASE_CAPSULES + 1);
        let report = product.step_all(&mut ());
        assert!(report.worked);
        assert!(trace.borrow().contains(&"business"));
    }

    #[test]
    fn 容量边界与业务槽数量由产品决定() {
        let trace = RefCell::new(Vec::new());
        let mut platform = FakePlatform::new(&trace);
        let Ok(mut product) = build::<_, CAPSULE_SLOTS>(&mut platform) else {
            return;
        };
        // 恰好还剩 BUSINESS_SLOTS 个空位；装满后第 BUSINESS_SLOTS+1 个必须失败。
        let mut extras: [Fake<'_>; BUSINESS_SLOTS] = core::array::from_fn(|index| Fake {
            name: if index == 0 {
                "business-0"
            } else {
                "business-1"
            },
            phase: CapsulePhase::Maintenance,
            trace: &trace,
        });
        for extra in &mut extras {
            assert!(product.register(extra).is_ok());
        }
        assert_eq!(product.len(), CAPSULE_SLOTS);
        let mut one_more = Fake {
            name: "business-2",
            phase: CapsulePhase::Maintenance,
            trace: &trace,
        };
        assert!(matches!(
            product.register(&mut one_more),
            Err(RegisterError::Full)
        ));
    }

    #[test]
    fn 可以给产品起自己的名字() {
        let trace = RefCell::new(Vec::new());
        let mut platform = FakePlatform::new(&trace);
        let result = build_named::<_, CAPSULE_SLOTS>("acme", &mut platform);
        assert!(result.is_ok());
        let Ok(product) = result else {
            return;
        };
        assert_eq!(product.status().name, "acme");
        assert_eq!(product.names().len(), CAPSULE_SLOTS);
    }

    #[test]
    fn 基础服务重名会被拒绝() {
        // 六个槽必须是六个独立实例；把其中两个命名为同一个名字来触发重名。
        struct DuplicatePlatform<'a> {
            terminal: Fake<'a>,
            fs: Fake<'a>,
            log: Fake<'a>,
            rotate: Fake<'a>,
            health: Fake<'a>,
            maintenance: Fake<'a>,
        }

        impl Platform for DuplicatePlatform<'_> {
            type Context = ();

            fn services(&mut self) -> PlatformServices<'_, Self::Context> {
                PlatformServices {
                    terminal: &mut self.terminal,
                    fs: &mut self.fs,
                    log: &mut self.log,
                    rotate: &mut self.rotate,
                    health: &mut self.health,
                    maintenance: &mut self.maintenance,
                }
            }
        }

        let trace = RefCell::new(Vec::new());
        let mut platform = DuplicatePlatform {
            terminal: Fake {
                name: "dup",
                phase: CapsulePhase::Input,
                trace: &trace,
            },
            fs: Fake {
                name: "dup",
                phase: CapsulePhase::Maintenance,
                trace: &trace,
            },
            log: Fake {
                name: "log",
                phase: CapsulePhase::Maintenance,
                trace: &trace,
            },
            rotate: Fake {
                name: "log-rotate",
                phase: CapsulePhase::Maintenance,
                trace: &trace,
            },
            health: Fake {
                name: "health",
                phase: CapsulePhase::Health,
                trace: &trace,
            },
            maintenance: Fake {
                name: "maintenance",
                phase: CapsulePhase::Maintenance,
                trace: &trace,
            },
        };
        let result = build::<_, CAPSULE_SLOTS>(&mut platform);
        assert!(matches!(
            result,
            Err(BuildError::Register(RegisterError::DuplicateName))
        ));
    }
}
