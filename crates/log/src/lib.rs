//! `log` —— 面向"现场没有调试器"的**文本日志**核心
//! （纯逻辑，主机可测；平台需求 DB-3 / P3）。
//!
//! 本 crate 只保留当前生产链路的两块纯逻辑资产：
//!
//! - [`text_log::TextLog`]：按行 FIFO 的文本环（只持有状态，不持有缓冲区）；
//! - [`archive`]：gzip generation 窗口的 64 B 索引。
//!
//! # 为什么不再保留二进制日志资产
//!
//! 0.25.x 之前还有一套"16 B 定长记录 → Ring → 转置 → LZ4 → 批次"的二进制链路。
//! 它在真机上从未成为生产路径：实际落地的是文本日志（`/log/blackbox.log`），
//! 二进制链路的读回/终端命令始终没有闭环。两套格式并存只会让"日志到底以谁为准"
//! 变模糊，因此按 0.26 的收敛决定，二进制记录、环、字符串、LZ4、转置、批次与
//! `persist` 模块已全部删除。
//!
//! 需要结构化日志时，正确方向是为文本行增加稳定的字段标签（或在 `log` 之上另建
//! 纯逻辑编解码层），而不是把一套未接线的旧格式继续留在仓库里。
//!
//! # 当前生产链路
//!
//! ```text
//! log_event()
//!   → TextLog（64 KiB SDRAM 环，按行 FIFO）
//!   → 写队列 Append
//!   → /log/blackbox.log
//!   → gzip generation 窗口 + /log/archive.idx
//! ```
//!
//! 级别过滤、`log on/off/level/dump/prev` 等终端行为在固件层实现；本 crate 只保证
//! 文本环与归档索引的纯逻辑可主机测试。

#![no_std]
// 纯逻辑：本 crate 存在的意义之一就是能被主机穷举测试。
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
// 测试里放宽：断言失败就该直接 panic。
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

pub mod archive;
pub mod level;
pub mod text_log;

pub use level::Level;
pub use text_log::{TextLog, TextStats};
