//! `wire-format` —— 与硬件无关的线路编解码层。
//!
//! # 职责边界（这条边界是刻意的）
//!
//! | 属于本层 | 不属于本层 |
//! |----------|-----------|
//! | 帧结构、CRC、字节序、长度校验 | 何时发送、发给谁、重试几次 |
//! | 解码结果的合法性 | 功能码/数据的设备语义（那是应用层） |
//!
//! 目前提供 **Modbus-RTU**（Modbus over Serial Line V1.02）的帧结构与
//! 结构校验。功能码是不透明 newtype（[`modbus_rtu::FunctionCode`]）：
//! 本层只保证"这是一帧结构合法的报文"，不解释它想表达什么。
//!
//! # 零拷贝、零分配
//!
//! 解码借入调用方的接收缓冲，编码写出到调用方的发送缓冲：
//!
//! ```text
//! decode(&'a [u8]) -> Result<Frame<'a>, ProtoError>   // 不复制，不分配
//! encode(&Frame<'_>, &mut [u8]) -> Result<usize, ProtoError>
//! ```
//!
//! 这样做的收益不只是省内存：**帧的有效期被绑定到缓冲区**，于是"缓冲区被复用
//! 之后还在用旧帧"这类缺陷变成借用检查器的错误。
//!
//! # 为什么"异常响应"是独立错误变体
//!
//! 功能码最高位置 1（异常响应）是一帧**有效**报文，现场含义是"对端明确拒绝"，
//! 与"总线不通"的处置完全不同（前者要改请求，后者要查电气）。把它与 CRC 错误
//! 混为一类，会让错误计数失去指导排障的能力。

#![cfg_attr(not(test), no_std)]
// 编解码是最容易被模糊测试打的一层，因此它必须能在主机上穷举——
// 也正因如此，这里不允许出现 unsafe。
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

pub mod crc;
pub mod error;
pub mod frame;
pub mod modbus_rtu;

pub use error::ProtoError;
pub use frame::FrameCodec;
pub use modbus_rtu::{FunctionCode, ModbusRtu};
