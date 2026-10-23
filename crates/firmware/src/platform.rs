//! 平台注入：把 firmware 持有的服务实例交给产品组合根。
//!
//! 本模块是 `firmware` 与 [`product`] 之间**唯一**的接缝：
//!
//! - `firmware` 知道怎么构造服务（它们要经 `board-support` 接触硬件）；
//! - `product` 知道"产品包含哪些 capsule、按什么顺序注册"；
//! - 两者只通过 `product::Platform` 的 `&mut dyn sdk::Capsule<C>` 交接，
//!   因此 `product` 不需要依赖 `firmware`（否则就是循环依赖）。
//!
//! 这里只做搬运，不做判断：注册顺序、容量、产品名全部由 `product` 决定。

use crate::capsules::{
    FsService, HealthService, LogService, MaintenanceService, RotateService, TerminalService,
    TerminalStepContext,
};

/// 平台服务实例的**独占借用**视图。
///
/// 生命周期就是 `idle` 的局部变量：实例先构造、再借用给产品组合根，
/// 组合根随 `idle` 一起结束。
pub(crate) struct PlatformImpl<'a> {
    pub(crate) terminal: &'a mut TerminalService,
    pub(crate) fs: &'a mut FsService,
    pub(crate) log: &'a mut LogService,
    pub(crate) rotate: &'a mut RotateService,
    pub(crate) health: &'a mut HealthService,
    pub(crate) maintenance: &'a mut MaintenanceService,
}

impl product::Platform for PlatformImpl<'_> {
    type Context = TerminalStepContext;

    fn services(&mut self) -> product::PlatformServices<'_, Self::Context> {
        product::PlatformServices {
            terminal: &mut *self.terminal,
            fs: &mut *self.fs,
            log: &mut *self.log,
            rotate: &mut *self.rotate,
            health: &mut *self.health,
            maintenance: &mut *self.maintenance,
        }
    }
}
