//! `shell` —— 串口终端：行编辑、ANSI 渲染与命令分发，全部是纯逻辑
//! （**平台需求 DB-4**）。
//!
//! # 为什么终端也要做成纯逻辑层
//!
//! 终端是"人机接口"，但它同时是**现场排障的主要工具**：设备装到现场（机柜/设备间）之后，能用的
//! 只有一根串口。因此它必须可靠——而"可靠"在这里的含义是：
//!
//! - 转义序列解析（方向键、Home/End/Delete）必须有确定行为，不能靠猜终端；
//! - 行编辑与历史记录的边界（插到行首/行尾、历史回绕、超长行截断）必须可测；
//! - 命令解析与输出格式必须能在主机上逐字节断言。
//!
//! 把这些放进 `#[no_std]` 的纯 crate，就能用主机测试覆盖上面的每一条；
//! `firmware` 只负责"把串口收到的字节喂进来、把要输出的字节发出去"。
//!
//! # 与 Fedora 终端的关系
//!
//! 提示符形如 `[root@tessera ~]# `，彩色区分用户/主机/路径与错误；
//! 支持方向键、Home/End、Ctrl-A/E/U/K/L/C、Tab 补全、上下翻历史。
//! 不追求与 bash 兼容，追求"**看起来熟悉、行为可预测**"。

#![cfg_attr(not(test), no_std)]
// 纯逻辑：转义序列解析与命令分发都应当能主机测试。
#![cfg_attr(not(target_os = "none"), forbid(unsafe_code))]
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

pub mod ansi;
pub mod commands;
pub mod cwd;
pub mod editor;
pub mod shell;

pub use cwd::Cwd;
pub use shell::{InstrumentHealth, MemoryReport};
