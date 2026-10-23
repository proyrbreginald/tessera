//! `primitives` —— 平台原语层：与硬件、与任何具体设备都无关的纯逻辑。
//!
//! # 为什么单独成 crate
//!
//! 真实的寄存器写入、中断时序与电气连接**无法**用主机测试替代。既然如此，
//! 就让不可测的部分尽可能薄，把可测的部分做大：
//!
//! ```text
//! primitives    平台原语：时间、实时预算、健康门控、格式化、随机数、校验
//!                  ← 唯一依赖是 heapless（固定容量容器）
//! ```
//!
//! 本 crate **不含**任何设备/协议字节，也不含芯片寄存器（那些在下游 crate）。
//! 原因不是洁癖，而是变更速率：外设与协议随项目变化，平台原语不该跟着动。
//!
//! # 类型纪律（每条都消除一类缺陷）
//!
//! | 机制 | 消除的缺陷类别 |
//! |------|----------------|
//! | [`time::Timepoint`] 不实现 `PartialOrd` | "用 `<` 判到期"在计数器回绕后失效 |
//! | [`budget`] 的 `const_assert` | 实时预算被悄悄改坏（**平台需求 RT-1**） |
//! | [`safety`] 的健康位与安全状态机 | 任务失联后仍继续驱动输出（**平台需求 SF-3/SF-4**） |
//! | [`crc`] 的双算法（CRC-16/CRC-32） | "同一个校验各写一份"导致的漂移 |
//! | `#![forbid(unsafe_code)]` | 本层不存在 UB 类缺陷（编译期事实，**平台需求 MT-4**） |
//!
//! # 可测性契约
//!
//! 本层所有状态机都是**纯函数式**的：时间由调用方以 [`time::Timepoint`] 传入，
//! 不读寄存器、不取当前时间、不做 I/O。因此任意时序都可在主机上重放，
//! 包括回绕、抖动、超时与乱序（**平台需求 TS-1/TS-3**）。

#![cfg_attr(not(test), no_std)]
// 本层不允许出现 unsafe：能被主机穷举测试，也最能被穷举验证。
// 让"这里没有 UB"成为编译期事实，而不是靠人工评审。
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
// 测试里允许 unwrap/assert：测试代码的失败方式就是 panic，这没问题。
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

pub mod budget;
pub mod crc;
pub mod format;
pub mod prng;
pub mod safety;
pub mod time;
