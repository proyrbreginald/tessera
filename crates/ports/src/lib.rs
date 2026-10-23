//! `ports` —— 硬件无关的**端口契约**，以及建立在契约之上的类型状态机。
//!
//! # 依赖倒置点（**平台需求 MT-1 / EV-3**）
//!
//! 上层（应用逻辑、服务）只认识本 crate 的 trait；下层（`board-support` 的真实
//! 外设、`simulator` 的主机替身）提供实现。于是同一份应用代码：
//!
//! ```text
//! primitives  平台原语      ← 不知道 Bus/Store 存在
//! ports 契约 + 类型状态机 ← 只依赖 core 的类型
//!        ▲                    ▲
//!   board-support          simulator
//!   （真实外设）           （主机替身，可在 CI 里跑）
//! ```
//!
//! 换硬件只改 BSP 的端口实现，应用代码逐字不动；换到主机测试只换替身。
//!
//! # 为什么类型状态机也放在这里
//!
//! 半双工总线最常见的一类现场故障是"**方向切换顺序错**"：还没发完就切到接收、
//! 或者在应答没到之前又发一帧。这类 bug 只在特定时序下复现，运行时报错等于把
//! 问题推到现场。
//!
//! [`bus::Link`] 把顺序抬进类型：只有 [`bus::Idle`] 状态能发，只有
//! [`bus::AwaitingResponse`] 状态能收。写错顺序是**编译错误**。
//! 它依赖 [`bus::Bus`] trait，所以契约与状态机必须在同一个 crate 里 ——
//! 否则会形成 core ↔ ports 的循环依赖。

#![cfg_attr(not(test), no_std)]
// 本层只有 trait 与不碰硬件的类型状态机，同样应当完全可主机测试。
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
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::float_arithmetic
    )
)]

pub mod bus;
pub mod clock;
pub mod double_bank;
pub mod log;
pub mod raw;
pub mod store;
