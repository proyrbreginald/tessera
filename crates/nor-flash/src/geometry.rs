//! 器件几何（**常量，且与器件手册一致**）。

/// 擦除扇区大小：4 KiB。
pub const SECTOR_BYTES: u32 = 4 * 1024;

/// 页编程页大小：256 B。
pub const PAGE_BYTES: u32 = 256;

/// 器件容量：32 MiB（W25Q256JVEQ）。
pub const DEVICE_BYTES: u32 = 32 * 1024 * 1024;

/// **可寻址窗口**：16 MiB。
///
/// ⚠️ 这不是笔误：器件上电处于 **3 字节地址模式**，只有前 16 MiB 能通过 XIP 访问。
/// 越过 16 MiB 的访问**不会报错**，而是丢掉高位地址、**别名到低 16 MiB** ——
/// 于是"擦除日志区"可能实际擦掉启动头。本项目用**专用 4 字节操作码**（见 [`super::Opcode`]）
/// 而不是全局 `EN4B(0xB7)`，就是为了不把器件留在非上电默认状态。
pub const ADDRESSABLE_BYTES: u32 = 16 * 1024 * 1024;

// 几何自洽性：这些断言把"改错一个常量"变成**编译期**错误。
const _: () = assert!(SECTOR_BYTES.is_power_of_two());
const _: () = assert!(PAGE_BYTES.is_power_of_two());
const _: () = assert!(
    SECTOR_BYTES.is_multiple_of(PAGE_BYTES),
    "一个扇区必须由整数个页组成"
);
const _: () = assert!(
    DEVICE_BYTES.is_multiple_of(SECTOR_BYTES),
    "容量必须是整数个扇区"
);
const _: () = assert!(ADDRESSABLE_BYTES <= DEVICE_BYTES);
