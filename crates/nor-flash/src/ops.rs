//! 操作码与**地址相位**：这两者必须成对，配错会静默损坏数据。

/// 4 字节地址模式下，命令之后必须先发 **4 字节地址**。
///
/// 这个常量存在的意义只有一条：让"用了 4 字节操作码却按 3 字节发地址"这件事
/// 有一个**能被测试与门禁引用**的名字，而不是散落在驱动代码里的字面量。
pub const ADDRESS_PHASE_4B: u8 = 0x20;

/// 地址模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddressMode {
    /// 3 字节地址（上电默认；只能寻址前 16 MiB）。
    Three,
    /// 4 字节地址（本项目用它越过 16 MiB）。
    Four,
}

impl AddressMode {
    /// 该模式所需的**地址相位字节**（LUT 里写进相位字段的值）。
    #[must_use]
    pub const fn phase(self) -> u8 {
        match self {
            // 3 字节：相位 0 表示"不发地址"是错的！这里是"命令本身带 3 字节地址"
            // 的 LUT 编码习惯：本项目 LUT 用 0 表示无额外相位，用 0x20 表示 4 字节地址。
            Self::Three => 0x00,
            Self::Four => ADDRESS_PHASE_4B,
        }
    }
}

/// 本项目用到的操作码（**全部是 4 字节地址变体**，除非注明）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Opcode {
    /// 写使能（无地址）。
    WriteEnable = 0x06,
    /// 读状态寄存器 1（无地址；bit0 是 WIP）。
    ReadStatus1 = 0x05,
    /// 4 字节地址**扇区擦除**（4 KiB）。
    SectorErase4B = 0x21,
    /// 4 字节地址**页编程**（≤256 B，不跨页）。
    PageProgram4B = 0x34,
    /// 4 字节地址**读**（XIP 之外的一次性读也用它）。
    Read4B = 0xEC,
}

impl Opcode {
    /// 操作码字节。
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::WriteEnable => 0x06,
            Self::ReadStatus1 => 0x05,
            Self::SectorErase4B => 0x21,
            Self::PageProgram4B => 0x34,
            Self::Read4B => 0xEC,
        }
    }

    /// 该操作码是否需要地址、以及需要几字节。
    ///
    /// # 为什么这个映射必须是**显式**的
    ///
    /// "4 字节操作码配 4 字节相位"是本项目数据安全的命门：配成 3 字节会让高地址被丢掉，
    /// 命令**看起来成功**、实际写到低 16 MiB 的同名偏移。所以这里不用"查表猜"，
    /// 而是逐个列出，并由下面的测试逐条断言一致性。
    #[must_use]
    pub const fn address_mode(self) -> Option<AddressMode> {
        match self {
            Self::WriteEnable | Self::ReadStatus1 => None,
            Self::SectorErase4B | Self::PageProgram4B | Self::Read4B => Some(AddressMode::Four),
        }
    }
}

/// 某个操作码所需的地址相位字节（无地址的操作码返回 `0`）。
#[must_use]
pub const fn byte_address_phase(opcode: Opcode) -> u8 {
    match opcode.address_mode() {
        Some(mode) => mode.phase(),
        None => 0,
    }
}
