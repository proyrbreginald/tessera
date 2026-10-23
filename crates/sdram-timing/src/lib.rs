//! `sdram-timing` —— SDRAM 的**规格、时序换算与内存测试算法**，全部是纯逻辑。
//!
//! # 为什么把这些从 BSP 里拆出来
//!
//! SEMC + SDRAM 的初始化是"一次写错就整块内存不可用"的代码，而且真正的风险不在
//! 寄存器写入这个动作，而在**数值**：tRP/tRCD/tRAS/tRFC 换算成多少个时钟周期、
//! 模式寄存器该写什么、刷新定时器该装多少。这些数值：
//!
//! - 完全由数据手册参数与 SEMC 时钟决定，**不需要硬件就能算**；
//! - 算错了在主机上就能被测出来（边界、取整方向、+1/-1 规则）；
//! - 一旦错了，在真机上表现为"内存随机出错"，是最难查的一类故障。
//!
//! 因此本 crate 承担数值与算法，`board-support` 只承担"把算好的值写进寄存器"
//! 这一层薄薄的、可逐行审阅的动作。
//!
//! # 内存测试算法也放在这里
//!
//! 地址线粘连、数据线固定、行/列译码错误……这些故障模式可以用**标准测试图样**
//! 检出。算法本身与硬件无关，因此放在纯逻辑层：主机上用一个"会故意出错的内存替身"
//! 就能验证算法**真的能抓到错**，而不是只验证"跑完没报错"。

#![cfg_attr(not(test), no_std)]
// 本层只有算术与算法，完全可主机测试。
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

pub mod memtest;
pub mod sdram;
