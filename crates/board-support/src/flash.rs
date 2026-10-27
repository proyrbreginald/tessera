//! 外部串行 NOR Flash 与启动配置块（FCB）。
//!
//! i.MX RT1021 **没有内部程序存储器**：上电后片上 Boot ROM 先从 FlexSPI1 外部
//! NOR 的偏移 0 读一个配置块（FCB），用它把 FlexSPI 控制器配到能取指的程度，
//! 再去读映像向量表（IVT）。FCB 不对，芯片永远到不了 `main`。
//!
//! # 实装器件（✅ 有依据）
//!
//! Winbond **`W25Q256JVEQ`**：256 Mbit = 32 MiB、3 V、quad-SPI 串行 NOR。
//! 原理图 `U2`（8 脚 SOIC，`SPI_SS0`/`SPI_CLK`/`SPI_DATA0..3`，供电 `FLASH_3V3`）。
//!
//! 下列参数**不是猜的**：取自**本板既有固件**的已知正确配置
//! （`bsp/board/imx_nor_flash/nor/flexspi_romapi.c`，那份固件在这块板上跑过现场），
//! 读/写/擦序列逐项相同，唯一改动是按实装器件填真实容量。
//!
//! # 32 MiB 全部可寻址：靠专用 4 字节操作码，不靠模式切换
//!
//! 器件上电是 3 字节地址模式（只能寻址 16 MiB）。本文件选用"专用 4 字节操作码"
//! （`0xEC`/`0x34`/`0x21`），而不是发 `EN4B(0xB7)` 把器件整体切到 4 字节模式 ——
//! 后者会把器件留在非上电默认状态，一旦序列没生效就等于变砖。详见 [`SEQ_READ`] 附近的对照表。
//!
//! # ⚠️ dummy 周期数与串行时钟是**绑定**的
//!
//! `0x06`（6 个 dummy 周期）只在 100 MHz 下正确。`W25Q256JV` 在 `0xEC`
//! （四线读、4 字节地址）下允许 133 MHz，但更快的时钟可能需要**更多** dummy 周期。只改 [`SERIAL_CLOCK`] 而
//! 不重算 dummy，会读到垃圾数据、表现为**启动即崩溃**。
//! **两者要么一起改，要么都别动。**
//!
//! 这条约束不是靠注释守住的：`cargo xtask gate image` 会从链接好的 ELF 里
//! 按数据手册偏移读出 FCB 的 tag/版本/容量/读序列并逐项断言，
//! 因此任何回归都在**构建期**失败，而不是在板子上失败。

use imxrt_boot_gen::{
    Imxrt,
    flexspi::{self, opcodes::sdr::*, *},
    serial_flash::nor,
};

/// i.MX RT 家族标识。
///
/// 用 `Imxrt1020` 而非具体型号：`imxrt-boot-gen` 按**家族**建模，
/// 与 `imxrt-hal` 的 feature 命名一致（只有 RAL 才按 `imxrt1021` 精确型号）。
const CHIP: Imxrt = Imxrt::Imxrt1020;

/// 实装 Flash 容量（字节）。32 MiB。
///
/// 与 `firmware/build.rs` 里的 `FLASH_SIZE` 必须一致：FCB 里报大了会让
/// Boot ROM 读到阵列之外，报小了则浪费空间。由 build.rs 生成的断言绑死。
pub const SIZE_BYTES: u32 = 32 * 1024 * 1024;

/// FlexSPI 串行时钟（器件允许 133 MHz，但 dummy 按 100 MHz 配置）。
pub const SERIAL_CLOCK: SerialClockOption = SerialClockOption::MHz100;

// ---------------------------------------------------------------------------
// 4 字节地址：用**专用操作码**，而不是发 EN4B 切模式
// ---------------------------------------------------------------------------
//
// `W25Q256` 上电处于 3 字节地址模式（上限 16 MiB）。要让整片 32 MiB 可用，
// 有两条路：
//
// | 路线 | 做法 | 代价 |
// |---|---|---|
// | A. 全局 EN4B 模式切换 | FCB 的 device mode 序列发 `0xB7`，器件整体切到 4 字节地址模式 | 器件被留在"非上电默认"的状态：一旦序列没执行成功而 LUT 已是 4 字节，Boot ROM 读不到映像 → 变砖，只能靠 BOOT_MODE 引脚 + 串口下载器恢复 |
// | **B. 专用 4 字节操作码（本文件采用）** | 读用 `0xEC`、页编程用 `0x34`、扇区擦除用 `0x21`、块擦除用 `0xDC`，地址相位一律 32 位 | 什么都不留在器件里：器件始终是上电默认的 3 字节模式，而每条命令**自带** 4 字节地址 |
//
// 选 B 的依据不是推理，而是**本板既有固件的已知正确配置**
// （`winshare/test/master-control-board-a-06/bsp/board/imx_nor_flash/nor/flexspi_romapi.c`，
// 那份固件已在这块板上跑过现场）：
//
// ```c
// /* Fast Read Quad I/O */
// // 0xEC - Fast Read Quad I/O with 4-Byte Address, 0x20 - 32 bit address
// LUT[READ]     = SEQ(CMD_SDR 1PAD 0xEC, RADDR_SDR 4PAD 0x20) , SEQ(DUMMY 4PAD 0x06, READ 4PAD 0x04)
// LUT[PAGEPROG] = SEQ(CMD_SDR 1PAD 0x34, RADDR_SDR 1PAD 0x20) , SEQ(WRITE 4PAD 0x04, STOP)
// LUT[ERASE]    = SEQ(CMD_SDR 1PAD 0x21, RADDR_SDR 1PAD 0x20)
// ```
//
// 另一条关键收益：**启动路径的风险不增加**。Boot ROM 读 FCB（偏移 0）与 IVT 用的是
// 它自己的简单读序列，器件始终处于上电默认状态，因此"FCB 写得不对"的后果仍然只是
// "不启动"（可以用 BOOT_MODE 引脚 + 串口下载器救回来），而不是"器件被留在错误模式"。

/// 读：`0xEC` = Fast Read Quad I/O **with 4-Byte Address**。
///
/// 不能再用 `0xEB`：那是 3 字节地址版本，只能寻址前 16 MiB。
const FAST_READ_QUAD_IO_4B: u8 = 0xEC;

/// 页编程：`0x34` = Quad Input Page Program **with 4-Byte Address**。
///
/// 数据相位是四线（与操作码名字对应）；`0x02`/`0x32` 都是 3 字节地址版本，不能用。
const PAGE_PROGRAM_QUAD_4B: u8 = 0x34;

/// 扇区擦除（4 KiB）：`0x21` = Sector Erase **with 4-Byte Address**。
const ERASE_SECTOR_4B: u8 = 0x21;

/// 32 位地址相位（4 字节）。3 字节是 `0x18`。
const ADDR_32BIT: u8 = 0x20;

/// XIP 读序列：命令 1 线（`0xEC`）→ 地址 4 线 **32 位** → dummy 4 线 6 周期 → 读 4 线。
const SEQ_READ: Sequence = SequenceBuilder::new()
    .instr(Instr::new(CMD, Pads::One, FAST_READ_QUAD_IO_4B))
    .instr(Instr::new(RADDR, Pads::Four, ADDR_32BIT))
    .instr(Instr::new(DUMMY, Pads::Four, 0x06))
    .instr(Instr::new(READ, Pads::Four, 0x04))
    .build();

// 编程/擦除序列**不参与 XIP 取指**，但必须放进 FCB：FCB 是 Boot ROM 载入 LUT 的
// 唯一来源，而运行期改写 LUT 有风险（LUT 是共享资源，改写过程中若发生取指就会读到
// 半成品序列）。把全部序列一次性交给 Boot ROM 载入、运行期只按索引取用，最安全。

/// 读状态寄存器 1（`0x05`）。轮询 WIP 位判断命令是否完成。
const SEQ_READ_STATUS: Sequence = SequenceBuilder::new()
    .instr(Instr::new(CMD, Pads::One, 0x05))
    .instr(Instr::new(READ, Pads::One, 0x01))
    .build();

/// 写使能（`0x06`）。每次编程/擦除之前都必须发，否则命令被器件忽略。
const SEQ_WRITE_ENABLE: Sequence = SequenceBuilder::new()
    .instr(Instr::new(CMD, Pads::One, 0x06))
    .build();

/// 页编程（`0x34`，四线数据输入，4 字节地址）。
///
/// 与既有固件一致：写操作速率不如读关键（只发生在升级/配置变更时），
/// 但**操作码必须与地址宽度配套**，所以这里不能沿用旧的 `0x02`。
const SEQ_PAGE_PROGRAM: Sequence = SequenceBuilder::new()
    .instr(Instr::new(CMD, Pads::One, PAGE_PROGRAM_QUAD_4B))
    .instr(Instr::new(RADDR, Pads::One, ADDR_32BIT))
    .instr(Instr::new(WRITE, Pads::Four, 0x04))
    .build();

/// 扇区擦除（`0x21`，单线，4 KiB，4 字节地址）。
const SEQ_ERASE_SECTOR: Sequence = SequenceBuilder::new()
    .instr(Instr::new(CMD, Pads::One, ERASE_SECTOR_4B))
    .instr(Instr::new(RADDR, Pads::One, ADDR_32BIT))
    .build();

/// FlexSPI 查找表。
///
/// 索引 0 的读序列**正在被 XIP 取指使用**，任何时候都不得覆盖。
const LOOKUP_TABLE: LookupTable = LookupTable::new()
    .command(Command::Read, SEQ_READ)
    .command(Command::ReadStatus, SEQ_READ_STATUS)
    .command(Command::WriteEnable, SEQ_WRITE_ENABLE)
    .command(Command::EraseSector, SEQ_ERASE_SECTOR)
    .command(Command::PageProgram, SEQ_PAGE_PROGRAM);

/// LUT 索引：必须与 `imxrt-boot-gen` 的 `Command` 枚举一致。
/// `xtask check-image` 会从链接好的 ELF 里断言这些内容，不一致会在 CI 被抓到。
pub mod lut_index {
    /// XIP 读（`0xEC` quad-I/O，4 字节地址）。
    pub const READ: u8 = 0;
    /// 读状态寄存器。
    pub const READ_STATUS: u8 = 1;
    /// 写使能。
    pub const WRITE_ENABLE: u8 = 3;
    /// 扇区擦除。
    pub const ERASE_SECTOR: u8 = 5;
    /// 页编程。
    pub const PAGE_PROGRAM: u8 = 9;
}

/// 4 字节寻址**运行期自检**：确认越过 16 MiB 的读没有被别名。
///
/// # 为什么需要它
///
/// "整片 32 MiB 可寻址"完全依赖 LUT 用的是 4 字节地址相位。而这个前提一旦被破坏
/// （例如有人把操作码换回 `0xEB`/`0x18`），**不会报错**：控制器丢掉高位地址、
/// 把 `XIP + 16 MiB + n` 当成 `XIP + n`。后果是"擦日志区"可能实际擦掉启动头。
///
/// # 判据
///
/// 映像的 IVT 魔数 `0x4020_00D1` 固定在偏移 `0x1000`。若发生别名，那么读
/// `XIP + 16 MiB + 0x1000` 会读到同一个魔数。于是：
///
/// - 低位（`XIP + 0x1000`）**必须**是 IVT 魔数 —— 否则说明我们对布局的理解已经错了；
/// - 高位（`XIP + 16 MiB + 0x1000`）**必须不是** IVT 魔数 —— 否则就是别名。
///
/// IVT 在映像里的偏移。
pub const IVT_OFFSET: usize = 0x1000;
/// IVT 魔数（`xtask check-image` 也从链接好的 ELF 里断言它）。
pub const IVT_MAGIC: u32 = 0x4020_00D1;
/// 3 字节地址模式的上限。
const THREE_BYTE_LIMIT: usize = 16 * 1024 * 1024;

/// 两次读，每次启动都跑。这是**产物级事实**：它检查的是控制器此刻的真实行为，
/// 而不是源码里的意图。
#[must_use]
pub fn four_byte_addressing_ok() -> bool {
    use ports::raw::RawMedium;

    let mut medium = crate::store::FlexSpiMedium::new();
    let mut buffer = [0u8; 4];

    let low = medium
        .read(IVT_OFFSET, &mut buffer)
        .ok()
        .filter(|read| *read == buffer.len())
        .map(|_| u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]));
    if low != Some(IVT_MAGIC) {
        return false; // 连低位都不对：我们对映像布局的理解有误
    }

    let high = medium
        .read(THREE_BYTE_LIMIT + IVT_OFFSET, &mut buffer)
        .ok()
        .filter(|read| *read == buffer.len())
        .map(|_| u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]));
    // 高位读到 IVT 魔数 = 地址被丢掉了高位 = 别名 = 上半区不可信。
    high != Some(IVT_MAGIC)
}

/// 512 字节 FlexSPI 配置块。
///
/// `read_sample_clk_src` 取**内部回环**，与既有固件的
/// `kFLEXSPIReadSampleClk_LoopbackInternally` 一致：本板是 1-1-4 的 quad-SPI 器件，
/// 没有独立的 FlexSPI DQS 网络，采样应当走控制器内部回环。
/// （此前用的是 `LoopbackFromDQSPad`，虽然实测也能启动，但那是"用了一个本板不存在的
/// 信号"—— 与 SEMC 那次 `DQSMD=1` 是同一类假设，趁这次改动一并对齐到已知正确值。）
const MEMORY_CONFIG: flexspi::ConfigurationBlock = flexspi::ConfigurationBlock::new(LOOKUP_TABLE)
    .version(Version::new(1, 4, 0))
    .read_sample_clk_src(ReadSampleClockSource::InternalLoopback)
    .cs_hold_time(0x03)
    .cs_setup_time(0x03)
    .column_address_width(ColumnAddressWidth::OtherDevices)
    .device_mode_configuration(DeviceModeConfiguration::Disabled)
    .wait_time_cfg_commands(WaitTimeConfigurationCommands::disable())
    .flash_size(SerialFlashRegion::A1, SIZE_BYTES)
    .serial_clk_freq(CHIP.serial_clock_frequency(SERIAL_CLOCK))
    .serial_flash_pad_type(FlashPadType::Quad);

/// 附加在配置块之后的 64 字节串行 NOR 参数尾巴。
///
/// 页 256 B、扇区 4 KB，都是 Boot ROM 解析映像所需。
/// 器件另有 64 KB 统一擦除块，但 Boot ROM 只**读**映像，块大小不属于启动配置。
const SERIAL_NOR_PARAMS: nor::ConfigurationBlock =
    nor::ConfigurationBlock::new(CHIP, MEMORY_CONFIG)
        .page_size(256)
        .sector_size(4096)
        .ip_cmd_serial_clk_freq(None);

/// 符号名是**承重**的：`imxrt-rt` 生成的链接脚本用
/// `EXTERN(FLEXSPI_CONFIGURATION_BLOCK)` 把它拉进 `.fcb` 段。
///
/// 改名、或去掉 `#[unsafe(no_mangle)]`，链接器就没有理由保留这个静态量，
/// FCB 槽位会被填 0，得到一个**能编译却永不启动**的镜像。
/// `xtask check-image` 断言偏移 0 处的 FCB tag 就是为了兜住这一点。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".fcb")]
pub static FLEXSPI_CONFIGURATION_BLOCK: nor::ConfigurationBlock = SERIAL_NOR_PARAMS;

// 这里刻意**不放单元测试**：FCB 只能在裸机目标上构造（依赖 `imxrt-boot-gen`），
// 而裸机目标是 `no_std`、没有测试框架；主机目标又不编译本模块。
// 写一组永不执行的测试只会给出假信心。
//
// 因此 FCB 的**字节级校验由 `cargo xtask gate image` 承担**：它直接从链接好的
// ELF 里按数据手册偏移读出 tag、版本、容量与读序列并断言。那是**产物级**证据，
// 比在源码里断言结构体更强 —— 它同时证明这些值真的落到了镜像里。
