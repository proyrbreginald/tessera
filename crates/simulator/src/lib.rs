//! `simulator` —— 主机侧仿真替身与场景驱动（**平台需求 TS-1**）。
//!
//! # 为什么这是"一等公民"而不是测试附件
//!
//! Boot ROM、外部 Flash 时序、真实中断延迟这三样**无法**在主机上复现，
//! 而 i.MX RT1021 在 QEMU/Renode 里的支持并不完整。于是 CI 与"真实运行"之间
//! 天然存在一道鸿沟 —— 如果不主动压缩它，就会出现"一个破坏启动流程的改动
//! 要等到夜间 HIL 才发现"。
//!
//! 本 crate 的作用就是把这道鸿沟压缩到最小：凡是**不依赖电气与真实时序**的部分
//! （协议解析、状态机、存储策略、调度决策），都能在这里被确定性驱动。
//! 目标是让"必须靠真机才能发现的问题"只剩电气与真实时序两类。
//!
//! # 组件
//!
//! | 组件 | 替身对象 | 关键能力 |
//! |------|----------|----------|
//! | [`ManualClock`] | `ports::clock::Clock` | 时间由测试推进，可跳变、可回绕 |
//! | [`MockBus`] | `ports::bus::Bus` | 脚本化应答，可注入超时/帧错误/丢应答 |
//! | [`MemMedium`] | `ports::raw::RawMedium` | **可注入任意时刻掉电**，且保留半写结果 |
//! | [`CapturingLog`] | `ports::log::LogSink` | 断言"该报的错报了、不该丢的没丢" |
//!
//! # 与目标端的关系
//!
//! 本 crate 是 `no_std` 且禁止 `unsafe`：它可以在任何目标上编译（包括固件目标），
//! 因此同一套替身既服务于主机测试，也可以在固件上作为"自检模式"的后端
//! （例如出厂自检时不接真实总线，直接跑脚本化场景）。

#![cfg_attr(not(test), no_std)]
// 替身代码同样不允许 unsafe：如果替身本身有 UB，主机测试给出的绿灯毫无意义。
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
pub mod log;
pub mod store;

pub use bus::{MockBus, Response};
pub use clock::ManualClock;
pub use log::CapturingLog;
pub use store::MemMedium;
