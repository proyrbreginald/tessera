//! 启动头：直接复用现有固件的前 8 KiB（FCB + IVT + boot data）。
//!
//! 内容来自 `VERSION=0.1.1.0` 的 Tessera 固件（`imxrt-rt` 生成），
//! 已包含：W25Q256JVEQ 32 MiB / 4 字节地址相位的 FCB，以及 entry=0x6000_2000 的 IVT。
//! Tock 的 kernel 同样从 0x6000_2000 开始，因此这 8 KiB 可以原样复用。
pub static BOOT_HDR: [u8; 8192] = *include_bytes!("../boot_hdr.bin");
