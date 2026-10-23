//! `nor-flash` —— 外部 QSPI NOR 的**纪律**（纯逻辑，主机可测；**平台需求 SF-6**）。
//!
//! # 为什么值得一个 crate
//!
//! FlexSPI 的擦写驱动要写不少寄存器（IP 命令触发、LUT 索引、WIP 轮询、超时），
//! 但**真正会出错、而且出错后果最严重的那部分不是寄存器**，而是**算术与范围校验**：
//!
//! - 擦除必须落在 **4 KiB 扇区**边界上（否则器件行为未定义）；
//! - 页编程**不能跨 256 B 页**（跨页写会回卷到页首，静默破坏该页开头）；
//! - 写入必须落在**可写窗口**内，且**绝不碰启动头**（写坏它只能靠 BOOT_MODE 引脚救）；
//! - 4 字节地址命令（擦 `0x21`/写 `0x34`/读 `0xEC`）**必须**配 4 字节地址相位（`0x20`）——
//!   配错会静默写到低 16 MiB 的同名偏移（"擦日志区"擦掉启动头）。
//!
//! 这些判断全是纯算术，且**每一条都曾在嵌入式项目里造成过灾难**。把它们放在这里，
//! 意味着驱动层只剩下"照做"，而"做对"这件事由 `cargo test` 在主机上反复验证 ——
//! 与 [`sdram_timing`](../sdram-timing/src/lib.rs)（SDRAM 的算术）是同一条分工。
//!
//! # 未做的事（明确留痕）
//!
//! 本 crate **不碰寄存器**，也不含器件时序参数（擦除/编程耗时）—— 那些必须以**手册**为准
//! 在驱动层写常量，并标注来源。这里只提供几何与判据。

#![cfg_attr(not(test), no_std)]
// 纯算术与范围校验：这个 crate 存在的意义就是能被主机测试反复跑。
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
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )
)]

#[cfg(test)]
mod tests;

pub mod flexspi;

mod geometry;
mod ops;
mod range;

pub use geometry::{ADDRESSABLE_BYTES, DEVICE_BYTES, PAGE_BYTES, SECTOR_BYTES};
pub use ops::{ADDRESS_PHASE_4B, AddressMode, Opcode, byte_address_phase};
pub use range::{FlashError, check_erase, check_program, pages_in, sectors_in};
pub mod selftest;
