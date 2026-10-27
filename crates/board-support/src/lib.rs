//! `board-support` —— 板级支持层。
//!
//! # 这是**唯一**允许接触寄存器的 crate
//!
//! 理由不是洁癖，而是可测试性：真实的寄存器写入、中断时序与电气连接无法在主机上
//! 验证。既然无法消除这一层，就让它尽可能薄，并把它的边界做成编译期事实 ——
//! 只有本 crate 的依赖闭包里会出现 `imxrt-*`，`cargo xtask gate layering` 强制这一点。
//!
//! # unsafe 预算
//!
//! 本 crate **允许** `unsafe`，但每一处都必须：
//!
//! 1. 紧邻一条 `// SAFETY:` 说明（`cargo xtask gate unsafe` 强制配平）；
//! 2. 计入 `unsafe.budget`（只允许减少或经显式批准增加）。
//!
//! 当前的 unsafe 集中在两处：外设 `instance()` 的获取（一次性、由 `board::init`
//! 保证唯一）与外设寄存器写入（HAL 的 `Pad::new`/`configure`）。业务逻辑里一处都没有。
//!
//! # 与 `imxrt-hal` 的关系
//!
//! HAL 0.6 覆盖了 CCM/LPUART/GPT/PIT/GPIO/IOMUXC/DMA/SAI/ADC 等，但**没有** FlexCAN
//! 与 FlexSPI 驱动，而 i.MX RT1021 的 CAN 引脚复用数据在 `imxrt-iomuxc` 里也缺失
//! （只有 1060 有实现）。因此：
//!
//! | 外设 | 来源 | 状态 |
//! |------|------|------|
//! | 时钟树 / 控制台 / 时基 | `imxrt-hal` | 已实现 |
//! | FlexCAN | **本 crate 自研**（RAL 寄存器直写） | 探针 + loopback 自证；总线收发待对端验证 |
//! | FlexSPI 擦写 | **本 crate 自研**（寄存器级 + ROM API） | 已实现（自证曾全过；扩展用例待复测） |

#![no_std]
// 本层允许 unsafe，但要求 `unsafe fn` 内部也显式标注 —— 避免"整个函数是 unsafe
// 于是内部随便写"的老习惯。
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_arithmetic
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod alloc_pool;
pub mod board;
pub mod cache;
pub mod config;
pub mod console;
pub mod dma;
pub mod fault;
pub mod flash;
pub mod flexcan;
pub mod flexspi_ip;
pub mod fs;
pub mod fs_volume;
pub mod mpu;
pub mod ocotp;
pub mod rom_flash;
pub mod rt;
pub mod sdram;
pub mod stacks;
pub mod store;
pub mod timer;
