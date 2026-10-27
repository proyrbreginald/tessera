//! **Boot ROM 的 NOR 擦写驱动** —— 薄适配层。
//!
//! # 为什么用 ROM API，而不是自己写 IP 命令
//!
//! 1. `imxrt-ral 0.6.2` 的 `src/blocks/imxrt1021/` **没有 flexspi.rs**（1015/1061 才有）
//!    ⇒ FlexSPI 控制器寄存器在这颗芯片上**没有类型化封装**，手写 `IPCR0/IPCR1/IPCMD/INTR`
//!    位域只能靠手册逐位写 —— 而那正是本项目禁止的"猜"；
//! 2. ROM 里那份驱动**已经在现场跑过**：本板既有 C 固件用的就是它
//!    （`winshare/test/master-control-board-a-06/bsp/board/imx_nor_flash/nor/flexspi_romapi.c`）；
//! 3. 它自带 WIP 轮询与超时，且用的 LUT 与 FCB 一致（我们是同一个配置）。
//!
//! # 配置不重新构造：直接复用 FCB
//!
//! `flexspi_nor_config_t` 的字段（`memConfig` + `pageSize` + `sectorSize` +
//! `ipcmdSerialClkFreq` + `blockSize`，见 SDK 的 `fsl_romapi.h`）**就是 Flash 偏移 0 处
//! FCB 的布局**。因此我们把 `XIP_BASE`（`0x6000_0000`）直接当配置指针传进去：
//!
//! - ROM 用的配置与 Boot ROM 自己加载的**是同一块数据**，不可能不一致；
//! - Rust 侧**不需要**复刻那个大结构（少抄一个字段就少一个"写进错误地址"的机会）；
//! - 这块 FCB 的内容已由 `cargo xtask gate image` 从链接产物逐项断言
//!   （tag/版本/容量/LUT 三个序列的 4 字节相位）。
//!
//! # ABI 来源（逐字来自 SDK，不是记忆）
//!
//! ```c
//! // fsl_romapi.c:61
//! #define g_bootloaderTree ((bootloader_api_entry_t *)*(uint32_t *)0x0020001cU)
//! // 树：runBootloader(+0) version(+4) copyright(+8) reserved0(+12) flexSpiNorDriver(+16)
//! // 驱动：version(+0) init(+4) program(+8) reserved0(+12) erase(+16) …
//! ```
//!
//! 下面的两个 `repr(C)` 结构**按原顺序逐字段复刻**，并用 `const` 断言钉住尺寸，
//! 这样"顺序抄错"会变成编译期错误。

use ports::store::StoreError;

/// ROM 引导 API 树指针的存放地址（`fsl_romapi.c:61`）。
const API_TREE_POINTER_ADDR: usize = 0x0020_001C;

/// SDK 的成功码（`kStatus_Success`，`fsl_common.h`）。
const STATUS_SUCCESS: i32 = 0;

/// FlexSPI NOR 实例号（本板只用 FlexSPI1）。
const INSTANCE: u32 = 0;

/// ROM 的 NOR 驱动接口表（`fsl_romapi.c` 里 `flexspi_nor_driver_interface_t` 的复刻）。
///
/// ⚠️ **字段顺序不可改动**（SDK 原文写着 "The order of existing fields must not be changed"）。
/// 只声明到 `erase` 为止也**不行**：尺寸断言会比较整个结构。
#[repr(C)]
struct NorDriverInterface {
    version: u32,
    init: Option<unsafe extern "C" fn(u32, *mut core::ffi::c_void) -> i32>,
    program: Option<unsafe extern "C" fn(u32, *mut core::ffi::c_void, u32, *const u32) -> i32>,
    reserved0: u32,
    erase: Option<unsafe extern "C" fn(u32, *mut core::ffi::c_void, u32, u32) -> i32>,
    reserved1: u32,
    clear_cache: Option<unsafe extern "C" fn(u32)>,
    xfer: Option<unsafe extern "C" fn(u32, *mut core::ffi::c_void) -> i32>,
    update_lut: Option<unsafe extern "C" fn(u32, u32, *const u32, u32) -> i32>,
    reserved2: u32,
}

/// ROM 引导 API 树（`bootloader_api_entry_t` 的复刻，只保留取驱动所需的字段）。
#[repr(C)]
struct ApiTree {
    run_bootloader: Option<unsafe extern "C" fn(*mut core::ffi::c_void)>,
    version: u32,
    copyright: *const u8,
    reserved0: u32,
    flex_spi_nor_driver: *const NorDriverInterface,
}

// 尺寸/偏移断言：抄错顺序或漏掉 `reserved` 会当场编译失败。
// （32 位目标：10 个 4 字节字段 ⇒ 40 字节；5 个字段 ⇒ 20 字节。）
const _: () = assert!(core::mem::size_of::<NorDriverInterface>() == 40);
const _: () = assert!(core::mem::size_of::<ApiTree>() == 20);
const _: () = assert!(core::mem::offset_of!(ApiTree, flex_spi_nor_driver) == 16);
const _: () = assert!(core::mem::offset_of!(NorDriverInterface, program) == 8);
const _: () = assert!(core::mem::offset_of!(NorDriverInterface, erase) == 16);

/// 取 ROM 的 NOR 驱动表；取不到（指针为空或树为空）返回 `None`。
///
/// **不猜**：空指针一律当"不可用"，绝不去调用一个零地址。
fn nor_driver() -> Option<&'static NorDriverInterface> {
    // 三处解引用放在**同一个** `unsafe` 块里：它们共享同一个前提（"这两张表在 ROM 里，
    // 且我们已经判过指针非空"），拆成三块只会让 SAFETY 注释重复三遍。
    //
    // SAFETY:
    // ① `0x0020_001C` 是 NXP 文档化的引导 API 树指针位置，SDK 自己的宏
    //    （`fsl_romapi.c:61`）就是同一个表达式；它在 ROM 常量区，读取不会写任何内存；
    // ② 树指针为 0 或驱动指针为 0 时我们**直接返回 `None`**，因此不会解引用空指针；
    // ③ 两个结构的字段顺序逐字来自 SDK 原文（`flexspi_nor_driver_interface_t` /
    //    `bootloader_api_entry_t`），并有 `const` 尺寸/偏移断言兜住"抄错顺序"；
    // ④ 全程只读 ROM，不写。
    unsafe {
        let tree_pointer = core::ptr::read_volatile(core::ptr::with_exposed_provenance::<u32>(
            API_TREE_POINTER_ADDR,
        ));
        let tree_address = usize::try_from(tree_pointer).ok()?;
        if tree_address == 0 {
            return None;
        }
        let tree = &*core::ptr::with_exposed_provenance::<ApiTree>(tree_address);
        let driver = tree.flex_spi_nor_driver;
        if driver.is_null() {
            return None;
        }
        Some(&*driver)
    }
}

/// **8 字节对齐**的配置缓冲。
///
/// # 为什么要显式对齐（2026-10-13 真机故障的处置）
///
/// ROM 的 NOR 驱动把 `config` 当 `flexspi_nor_config_t *` 用，里面全是 `uint32_t` 字段，
/// 并且会**自己写**这个结构（运行期状态）。而 `[u8; 512]` 的对齐只有 **1** ——
/// 编译器没有义务把它放在 4 字节边界上，于是"结构体字段访问"就可能变成非对齐访问。
///
/// 真机观测到的正是它：打开实验开关后 ROM 在**写 LUT** 时 `UsageFault`（非对齐），
/// 故障帧 `R2=0x5af05af0`（LUT 解锁键）、`R1=0x402a8210`（FlexSPI `LUT[4]`）。
/// 对齐到 8（本结构里最宽字段是 `uint32_t`，4 就够；取 8 留一档裕量）之后，
/// 这一整类"编译器把数组摆歪了"的可能性被排除掉，而不是靠运气。
#[repr(align(8))]
pub struct AlignedConfig(pub [u8; CONFIG_BYTES]);

/// 配置缓冲的对齐（终端/日志可据此如实上报；`xtask` 与测试都引用它）。
pub const CONFIG_ALIGN: usize = core::mem::align_of::<AlignedConfig>();

/// RAM 里的配置副本长度。
///
/// i.MXRT 的 Flash Configuration Block 是 512 字节；这里拷同样的前缀。
/// **不必复刻结构版式**（那正是最容易抄错的地方）：ROM 只读它需要的字段，
/// 多拷的字节无害。
pub const CONFIG_BYTES: usize = 512;

// ---------------------------------------------------------------------------
// 逐字段构造 `flexspi_nor_config_t`（只在开关打开时生效；默认仍走 FCB）
// ---------------------------------------------------------------------------

/// 字段偏移（**逐条来自 SDK 头文件的 `[0x0xx-0x0xx]` 注释**，一个都不猜）。
///
/// 来源：`tmp/SDK_26_09_00_MIMXRT1021xxxxx/mcuxsdk/devices/RT/RT1020/MIMXRT1021/
/// drivers/romapi/fsl_romapi.h` 的 `flexspi_mem_config_t`（第 220-270 行）
/// 与 `flexspi_nor_config_t`（第 271-291 行）。
///
/// ⚠️ 这里的每一个数字都对应 SDK 注释里的一段区间，`const` 断言把区间接续关系钉死 ——
/// "抄错偏移"在本项目造成过两次复位循环，所以它必须在**编译期**就失败。
mod field {
    /// `tag` `[0x000-0x003]`，固定值 `0x42464346`（`"FCFB"`）。
    pub(super) const TAG: usize = 0x000;
    /// `version` `[0x004-0x007]`。
    pub(super) const VERSION: usize = 0x004;
    /// `readSampleClkSrc` `[0x00c-0x00c]`。
    pub(super) const READ_SAMPLE_CLK_SRC: usize = 0x00C;
    /// `csHoldTime` `[0x00d-0x00d]`。
    pub(super) const CS_HOLD_TIME: usize = 0x00D;
    /// `csSetupTime` `[0x00e-0x00e]`。
    pub(super) const CS_SETUP_TIME: usize = 0x00E;
    /// `columnAddressWidth` `[0x00f-0x00f]`。
    pub(super) const COLUMN_ADDRESS_WIDTH: usize = 0x00F;
    /// `controllerMiscOption` `[0x040-0x043]`。
    pub(super) const CONTROLLER_MISC_OPTION: usize = 0x040;
    /// `deviceType` `[0x044-0x044]`。
    pub(super) const DEVICE_TYPE: usize = 0x044;
    /// `sflashPadType` `[0x045-0x045]`。
    pub(super) const SFLASH_PAD_TYPE: usize = 0x045;
    /// `serialClkFreq` `[0x046-0x046]`。
    pub(super) const SERIAL_CLK_FREQ: usize = 0x046;
    /// `sflashA1Size` `[0x050-0x053]`。
    pub(super) const SFLASH_A1_SIZE: usize = 0x050;
    /// `lookupTable` `[0x080-0x17f]`（每条序列 4 字）。
    pub(super) const LOOKUP_TABLE: usize = 0x080;
    /// `pageSize`（NOR 段首字段）。
    pub(super) const PAGE_SIZE: usize = 0x1C0;
    /// `sectorSize`。
    pub(super) const SECTOR_SIZE: usize = 0x1C4;
    /// `ipcmdSerialClkFreq`。
    pub(super) const IPCMD_SERIAL_CLK_FREQ: usize = 0x1C8;
    /// `blockSize`。
    pub(super) const BLOCK_SIZE: usize = 0x1D0;
}

/// `readSampleClkSrc` = `kFLEXSPIReadSampleClk_LoopbackInternally`（SDK 枚举值 0）。
const READ_SAMPLE_CLK_LOOPBACK_INTERNAL: u8 = 0;
/// `deviceType` = `kFLEXSPIDeviceType_SerialNOR`（SDK 枚举值 1）。
const DEVICE_TYPE_SERIAL_NOR: u8 = 1;
/// `sflashPadType` = `kSerialFlash_4Pads`（SDK 枚举值 4）。
const SFLASH_PAD_4: u8 = 4;
/// `serialClkFreq` / `ipcmdSerialClkFreq` = `kFLEXSPISerialClk_100MHz`（SDK 枚举值 6）。
///
/// **本板取 100 MHz**（依据：`crates/board-support/src/flash.rs:52` 的 `SerialClockOption::MHz100`，
/// 而 dummy `0x06` 也只对该频率正确 —— `AGENTS.md` §5）。
/// 既有参考实现写的是 133 MHz，那是**另一块板/另一套时钟树**的值，不能照抄到本板 ✗。
const SERIAL_CLK_100MHZ: u8 = 6;
/// `controllerMiscOption` = `1 << kFLEXSPIMiscOffset_SafeConfigFreqEnable`（位 4）。
///
/// SDK `fsl_romapi.h:191`：`kFLEXSPIMiscOffset_SafeConfigFreqEnable = 4U` ⇒ 掩码 `0x10`。
/// 既有参考实现用的 `FSL_ROM_FLEXSPI_BITMASK(kFLEXSPIMiscOffset_SafeConfigFreqEnable)` 也是这一位。
const MISC_SAFE_CONFIG_FREQ: u32 = 1 << 4;
/// `csHoldTime` / `csSetupTime` 默认值 3（SDK 注释：`default value: 3`）。
const CS_TIME_DEFAULT: u8 = 3;
/// FCB 版本（与本项目产物里 FCB 的版本字段一致：`0x5601_0400` = `V` + 1.4.0）。
const CFG_BLOCK_VERSION: u32 = 0x5601_0400;

/// 把 `[u8; CONFIG_BYTES]` 当作小端 32 位字段表来写。
///
/// 用 `get_mut(..)` 而不是裸指针：越界（= 偏移抄错）会**在运行期返回 `None`**
/// 而不是静默写到别处 —— 这条比"省一次判空"重要得多。
fn put_u32(config: &mut AlignedConfig, offset: usize, value: u32) -> bool {
    match config.0.get_mut(offset..offset.saturating_add(4)) {
        Some(slot) => {
            slot.copy_from_slice(&value.to_le_bytes());
            true
        }
        None => false,
    }
}

/// 把一条 LUT 序列写进 `lookupTable`（序列 `index` 占 4 字；**剩余字显式清零**）。
///
/// "剩余字必须清零"的依据：官方约定**零字即 STOP**（opcode 0 = STOP），
/// 而我们要给 ROM 的 LUT 会按索引被整条拷进控制器的 LUT1 —— 槽里留着垃圾
/// 就会让序列跑进垃圾（本项目实测过一次 `INTR.IPCMDERR`）。
fn put_lut_seq(config: &mut AlignedConfig, index: usize, words: &[u32]) -> bool {
    let Some(base) = nor_flash::flexspi::lut_word(u8::try_from(index).unwrap_or(u8::MAX)) else {
        return false;
    };
    for slot in 0..nor_flash::flexspi::offset::LUT_WORDS_PER_SEQUENCE {
        let offset = field::LOOKUP_TABLE + (base + slot) * 4;
        let word = words.get(slot).copied().unwrap_or(0);
        if !put_u32(config, offset, word) {
            return false;
        }
    }
    true
}

/// **在 RAM 里逐字段构造** ROM 的 `flexspi_nor_config_t`。
///
/// # 为什么不再从 FCB 拷
///
/// 原来那条路是"把 Flash 偏移 0 的 FCB 读进 RAM，再补两个尾部字段"。
/// 它的前提是"FCB 的版式就是 `flexspi_nor_config_t`"。逐字段核对之后，
/// 这个前提**在本例里基本成立**（产物 FCB 的 tag/版本/时钟/容量/LUT 都与 SDK 表一致），
/// 但有一个**结构性**的不同：FCB 是给 Boot ROM **引导**用的，它的尾部三个字段
/// （`pageSize`/`sectorSize`/`blockSize`/`ipcmdSerialClkFreq`）在产物里并不完整；
/// 而 IP 命令路径**需要**它们。与其在一个"大体对、局部空"的结构上打补丁，
/// 不如按 SDK 注释**逐字段显式构造** —— 每个字段的存在与取值都能被逐条审阅。
///
/// # 取值来源（逐项）
///
/// - 结构性与时序字段：既有参考实现 `FLEXSPI_NorFlash_GetConfig`
///   （`winshare/test/master-control-board-a-06/bsp/board/imx_nor_flash/nor/flexspi_romapi.c:60`），
///   那份固件**在这块板上跑过现场**；
/// - LUT：`crates/nor-flash/src/flexspi.rs` 的 `lut_words`（= 既有参考实现的 4 字节地址分支，
///   已逐字核对；也用 `xtask check-image` 从链接产物断言过）；
/// - 时钟 100 MHz 与容量 32 MiB：本板**实测**（见上面对应常量的注释）。
fn constructed_config() -> Option<AlignedConfig> {
    use nor_flash::flexspi::{lut_words, seq};

    let mut config = AlignedConfig([0u8; CONFIG_BYTES]);

    // ---- 头部（[0x000-0x00f]）----
    put_u32(&mut config, field::TAG, TAG_VALUE).then_some(())?;
    put_u32(&mut config, field::VERSION, CFG_BLOCK_VERSION).then_some(())?;
    *config.0.get_mut(field::READ_SAMPLE_CLK_SRC)? = READ_SAMPLE_CLK_LOOPBACK_INTERNAL;
    *config.0.get_mut(field::CS_HOLD_TIME)? = CS_TIME_DEFAULT;
    *config.0.get_mut(field::CS_SETUP_TIME)? = CS_TIME_DEFAULT;
    // `columnAddressWidth`：串行 NOR 不用（既有参考实现写 0），保持 0。

    // ---- 器件模式配置（[0x010-0x03f]）----
    // 全部保持 0：本板**不使用** EN4B 之类的模式切换（`flash.rs` 的路线 B：
    // 专用 4 字节操作码），因此 `deviceModeCfgEnable` / `configCmdEnable` 都是 0。
    // 直接后果：ROM 的 `need_safe_freq` 判据（`bl_flexspi.c` 的
    // `flexspi_init`：`(deviceModeCfgEnable || configCmdEnable) && misc 位`）为**假** ⇒
    // ROM 不会把 IP 命令降到 30 MHz 安全频率，而是用 `serialClkFreq`。

    // ---- 控制器选项与器件类型（[0x040-0x047]）----
    put_u32(
        &mut config,
        field::CONTROLLER_MISC_OPTION,
        MISC_SAFE_CONFIG_FREQ,
    )
    .then_some(())?;
    *config.0.get_mut(field::DEVICE_TYPE)? = DEVICE_TYPE_SERIAL_NOR;
    *config.0.get_mut(field::SFLASH_PAD_TYPE)? = SFLASH_PAD_4;
    *config.0.get_mut(field::SERIAL_CLK_FREQ)? = SERIAL_CLK_100MHZ;
    // `lutCustomSeqEnable` [0x047] 保持 0：我们不用自定义序列索引（既有参考实现也是 0）。

    // ---- 容量（[0x050-0x05f]）----
    // 只连了 A1 一片；`sflashA2Size`/`B1`/`B2` 保持 0（"0 = 该端口没有器件"）。
    put_u32(&mut config, field::SFLASH_A1_SIZE, BASE_FLASH_SIZE).then_some(())?;

    // ---- 查找表（[0x080-0x17f]）----
    // **序列索引是 ROM 的约定**，必须与 `fsl_romapi.h` 的 `NOR_CMD_LUT_SEQ_IDX_*` 一致：
    // 读 0 / 读状态 1 / 写使能 3 / 扇区擦除 5 / 页编程 9
    //（已在 `nor_flash::flexspi::seq` 里注明出处）。
    put_lut_seq(&mut config, usize::from(seq::READ), &lut_words::READ).then_some(())?;
    put_lut_seq(
        &mut config,
        usize::from(seq::READ_STATUS),
        &lut_words::READ_STATUS,
    )
    .then_some(())?;
    put_lut_seq(
        &mut config,
        usize::from(seq::WRITE_ENABLE),
        &lut_words::WRITE_ENABLE,
    )
    .then_some(())?;
    put_lut_seq(
        &mut config,
        usize::from(seq::ERASE_SECTOR),
        &lut_words::ERASE_SECTOR,
    )
    .then_some(())?;
    put_lut_seq(
        &mut config,
        usize::from(seq::PAGE_PROGRAM),
        &lut_words::PAGE_PROGRAM,
    )
    .then_some(())?;

    // ---- NOR 段（紧跟 `memConfig`，其长度由 SDK 注释的最后一个区间给出）----
    // `reserved4[4]` 结束在 `[0x1b0-0x1bf]` ⇒ `memConfig` 长 0x1C0 字节，
    // 于是 `pageSize` 落在 0x1C0（与产物 FCB 的实测偏移一致）。
    put_u32(&mut config, field::PAGE_SIZE, PAGE_BYTES).then_some(())?;
    put_u32(&mut config, field::SECTOR_SIZE, SECTOR_BYTES).then_some(())?;
    *config.0.get_mut(field::IPCMD_SERIAL_CLK_FREQ)? = SERIAL_CLK_100MHZ;
    put_u32(&mut config, field::BLOCK_SIZE, BLOCK_BYTES).then_some(())?;

    Some(config)
}

// 偏移表的**接续关系**在编译期钉死：把"抄错偏移"变成编译失败而不是复位循环。
const _: () = {
    // 头部各字节字段紧密相邻（SDK：0x00c/0x00d/0x00e/0x00f）。
    assert!(field::CS_HOLD_TIME == field::READ_SAMPLE_CLK_SRC + 1);
    assert!(field::CS_SETUP_TIME == field::CS_HOLD_TIME + 1);
    assert!(field::COLUMN_ADDRESS_WIDTH == field::CS_SETUP_TIME + 1);
    // 器件类型三连（SDK：0x044/0x045/0x046/0x047）。
    assert!(field::SFLASH_PAD_TYPE == field::DEVICE_TYPE + 1);
    assert!(field::SERIAL_CLK_FREQ == field::SFLASH_PAD_TYPE + 1);
    // 容量四连（SDK：0x050/0x054/0x058/0x05c）。
    assert!(field::SFLASH_A1_SIZE + 4 == 0x054);
    // `memConfig` 长度：尾部保留区结束在 0x1bf ⇒ NOR 段从 0x1c0 开始。
    assert!(field::PAGE_SIZE == 0x1C0);
    assert!(field::SECTOR_SIZE == field::PAGE_SIZE + 4);
    // NOR 段尾部：4 字节时钟 + 4 个 u8 + 4 字节块大小（SDK 字段顺序）。
    assert!(field::IPCMD_SERIAL_CLK_FREQ == field::SECTOR_SIZE + 4);
    assert!(field::BLOCK_SIZE == field::IPCMD_SERIAL_CLK_FREQ + 8);
    // 整段必须落在我们拷贝的缓冲之内。
    assert!(field::BLOCK_SIZE + 4 <= CONFIG_BYTES);
};

/// FCB 魔数（`"FCFB"` 小端）。
const TAG_VALUE: u32 = 0x4246_4346;
/// 本板实装容量：32 MiB（`nor_flash::SIZE_BYTES` 的同一事实，此处按 u32 复用）。
const BASE_FLASH_SIZE: u32 = 32 * 1024 * 1024;
/// 页大小 256 B（`nor_flash::PAGE_BYTES`）。
const PAGE_BYTES: u32 = nor_flash::PAGE_BYTES;
/// 扇区大小 4 KiB（`nor_flash::SECTOR_BYTES`）。
const SECTOR_BYTES: u32 = nor_flash::SECTOR_BYTES;
/// 块大小 64 KiB（分区表的 `BLOCK` 是唯一来源）。
const BLOCK_BYTES: u32 = flash_layout::BLOCK;

/// 把 FCB **拷进 RAM**，返回可交给 ROM 的配置缓冲。
///
/// # 为什么必须拷（真机自证抓出来的）
///
/// SDK 里 `program`/`erase` 的形参注释是
/// **"config: A pointer to the storage for the driver runtime state"** ——
/// 也就是说 **ROM 会往这个结构里写运行期状态**。
///
/// 我原先直接把 `XIP_BASE`（Flash 里的 FCB）当配置指针传进去，理由是"单一事实来源"。
/// 结果是：驱动往**只读的 XIP 映射区**写状态 ⇒ 状态是垃圾 ⇒
/// **`program` 返回成功，但一个字节都没写进去**（自证读回 `head_got=ffffffff` = 擦除态）。
///
/// 修法保持"内容单一来源"的好处，同时把**存放位置**改对：
/// **内容从 FCB 拷来，结构活在 RAM**。
fn config_in_ram() -> Option<AlignedConfig> {
    if super::config::FLASH_USE_CONSTRUCTED_CONFIG {
        // 另一条实验路径（默认关闭；打开后真机在 ROM 的 `erase` 里非对齐故障，
        // 见 `config::FLASH_USE_CONSTRUCTED_CONFIG` 的文档）。
        return constructed_config();
    }
    // ⚠️ **2026-10-13 真机证据：默认只走 FCB 拷贝。**
    //
    // 逐字段构造的那份配置（[`constructed_config`]）与 FCB 拷贝在**内容上几乎逐字节相同**
    // （tag/版本/时钟/容量/LUT/页/扇区全一致），差别只有两处：
    // ① `controllerMiscOption` 位 4（0x10 vs 0x00）；② `ipcmdSerialClkFreq` / `blockSize`
    // 两个尾部字段（构造版 6 / 65536，FCB 原始版 0 / 0）。
    //
    // 而真机打开开关后，**ROM 的 `erase` 在 stage=52（进入 ROM 调用）当场 UsageFault 非对齐**，
    // 故障帧里 `R2=0x5af05af0`（= LUT 解锁键 `FLEXSPI_LUT_KEY_VAL`）、
    // `R1=0x402a8210`（= FlexSPI `LUT[4]` 寄存器）⇒ ROM 在**写 LUT 时**非对齐访问。
    // 同一时刻走 FCB 拷贝则一切正常（前一轮真机已证）。
    //
    // 结论：**构造配置这条路本身还没走通，不能拿它做"编程能不能写"的判据** ——
    // 必须先把它自己的故障查清（下一步：逐字段二分，先只改 ① 再只改 ②）。
    // 在那之前，实验路径统一用 FCB 拷贝，好让"编程写不进去"这个主问题只有一个变量。
    fcb_copy_config()
}

/// 旧路径：把 Flash 偏移 0 的 FCB 拷进 RAM，并补齐 ROM 的 IP 命令路径要用的尾部字段。
///
/// 保留它是**为了不改变默认启动行为**：实验配置只在
/// [`super::config::FLASH_SELFTEST_AT_BOOT`] 打开时生效 —— 配置一旦写错，
/// 后果是复位循环（本项目为此付出过两次真机代价）。
fn fcb_copy_config() -> Option<AlignedConfig> {
    use ports::raw::RawMedium;

    let mut config = AlignedConfig([0u8; CONFIG_BYTES]);
    let mut medium = super::store::FlexSpiMedium::new();
    // 读的是 Flash 里的 FCB（Boot ROM 自己加载的那块），拷贝进 RAM。
    // ⚠️ 这里要的是 **Flash 内偏移**（FCB 在偏移 0），不是 XIP 绝对地址 ——
    // 第一版把 `XIP_BASE`（0x6000_0000）当偏移传了进去，介质层的映射判据直接拒绝 ⇒
    // 表现为"三个用例瞬间失败（erase4k=4us）"。自证当场把它抓了出来。
    let read = RawMedium::read(&mut medium, 0, &mut config.0).ok()?;
    if read != CONFIG_BYTES {
        return None;
    }

    // ⚠️ **补上 FCB 里为空、但 ROM 的 IP 命令路径要用的两个字段**。
    //
    // 这是从**产物里读出来的**事实（`firmware.bin` 的 FCB 尾部）：
    //   @0x1C0 = 0x0000_0100  pageSize   = 256   ✓ 有
    //   @0x1C4 = 0x0000_1000  sectorSize = 4096  ✓ 有
    //   @0x1C8 = 0            ipcmdSerialClkFreq = 0  ✗ **空**
    //   @0x1D0 = 0            blockSize = 0           ✗ **空**
    //
    // 而既有参考实现（`FLEXSPI_NorFlash_GetConfig`）**显式设过这两个**：
    // `ipcmdSerialClkFreq = 133 MHz`、`blockSize = 65536`。
    // Boot ROM 引导不需要它们（所以 FCB 里是 0），但 **IP 命令路径需要** ——
    // `ipcmdSerialClkFreq = 0` 就意味着"IP 命令时钟选择子无效"，
    // 与"命令返回成功却什么都不发生"的现象吻合。
    //
    // 偏移取自 SDK 头文件里 `flexspi_nor_config_t` 的字段顺序（不猜）：
    //   memConfig(448) + pageSize(u32) + sectorSize(u32) + ipcmdSerialClkFreq(u8)
    //   + isUniformBlockSize + isDataOrderSwapped + reserved0[1]
    //   + serialNorType + needExitNoCmdMode + halfClkForNonReadCmd + needRestoreNoCmdMode
    //   + **blockSize(u32)**
    //
    // 时钟选择子取 **100 MHz**（=6，见 SDK 的 `kFLEXSPISerialClk_100MHz`）——
    // **不是**既有参考实现的 133 MHz：本板的 FlexSPI 跑 100 MHz，dummy `0x06` 也只对该频率正确。
    const OFF_PAGE_SIZE: usize = 0x1C0;
    const OFF_SECTOR_SIZE: usize = 0x1C4;
    const OFF_IPCMD_CLK: usize = 0x1C8;
    const OFF_BLOCK_SIZE: usize = 0x1D0;
    /// `kFLEXSPISerialClk_100MHz`（SDK `fsl_romapi.h`）。
    const SERIAL_CLK_100MHZ: u8 = 6;

    // 前两个字段 FCB 里是对的，这里顺便**核对**（不对就拒绝，而不是带着可疑配置往下走）。
    let page_size = u32::from_le_bytes(
        config
            .0
            .get(OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4)?
            .try_into()
            .ok()?,
    );
    let sector_size = u32::from_le_bytes(
        config
            .0
            .get(OFF_SECTOR_SIZE..OFF_SECTOR_SIZE + 4)?
            .try_into()
            .ok()?,
    );
    if page_size != 256 || sector_size != 4096 {
        return None;
    }
    *config.0.get_mut(OFF_IPCMD_CLK)? = SERIAL_CLK_100MHZ;
    // `flash_layout::BLOCK` 本身就是 u32（块大小 64 KiB）——无需转换。
    let block = flash_layout::BLOCK;
    config
        .0
        .get_mut(OFF_BLOCK_SIZE..OFF_BLOCK_SIZE + 4)?
        .copy_from_slice(&block.to_le_bytes());

    Some(config)
}

/// **让数据缓存与器件内容重新一致**。
///
/// # 这一步不是可选的（真机自证抓出来的）
///
/// XIP 映射区是**可缓存**的（横幅里 `flash rd : … cache ON` 就是它）。于是写入路径上
/// 有两个必须处理的后果：
///
/// 1. **编程后回读可能命中缓存** —— 读到的是旧内容，看起来"写失败"；
/// 2. 更危险的是**介质层的读-改-写**：它先读当前页（可能命中缓存拿到旧内容）、
///    拼接、再整页编程 ⇒ **会把陈旧内容写回器件**，静默破坏同页其它字节。
///
/// ROM 的 NOR 驱动接口里专门有一个 `clear_cache`（本文件的结构体在 `+24` 已经声明它），
/// 存在的理由就是这个。**擦除与编程之后各调一次**，不做"缓存大概不会命中"的假设 ——
/// 上一版自证正是因此报 FAIL（`bad` 落在写入范围的第一个字节上）。
fn clear_cache(driver: &NorDriverInterface) {
    let Some(clear) = driver.clear_cache else {
        return;
    };
    // SAFETY: ROM 提供的缓存维护例程，参数是本板唯一的 FlexSPI 实例号；
    // 它只做缓存维护（清/失效），不改任何我们拥有的内存结构。
    unsafe { clear(INSTANCE) };
}

/// **让 XIP 读与器件内容重新一致**（转调 ROM 的 `clear_cache`）。
///
/// # 为什么必须由寄存器写路径调用（2026-10-13 真机定位）
///
/// XIP 映射区**可缓存**（横幅 `flash rd : … cache ON`）。器件内容一旦被改写，
/// 缓存里的旧行就是脏的：紧接着的读会拿到**旧内容**，看起来像"写了没生效"。
///
/// 真机判据：把 `store.rs` 的整页编程从 ROM 路径切到寄存器路径之后，
/// **两条自证同时失效** —— 页对齐自证从 `detail=0` 变成 `detail=100`
///（第 0 字节就不符）、跨页自证 `失配 @200 got=ffffffffffffffff`。
/// 而**擦除仍走 ROM（内部调 `clear_cache`）**，所以擦除之后立刻读回是准的 ——
/// 这条对比把根因锁死在"寄存器写路径缺缓存维护"上。
///
/// ⚠️ 写路径的维护必须与擦除路径**用同一个机制**：两套机制各自漂移，
/// 就会变成"擦除看得见、编程看不见"这种只在特定顺序下暴露的缺陷。
pub fn clear_flash_cache() {
    if let Some(driver) = nor_driver() {
        clear_cache(driver);
    }
}

/// ROM 驱动是否可用（终端/日志可据此如实报告）。
#[must_use]
pub fn available() -> bool {
    nor_driver().is_some()
}

/// 本次运行交给 ROM 的配置来自哪里（**如实上报**，不是期望值）。
///
/// 判据与 [`config_in_ram`] 同源：`FLASH_USE_CONSTRUCTED_CONFIG` 打开 ⇒ 逐字段构造的
/// 配置；关闭 ⇒ 从 FCB 拷的那份。自证报告里带上它，才能把"同一段代码在两种配置下的
/// 行为"区分开 —— 否则"这次为什么不一样"只能靠猜。
///
/// ⚠️ 这两个开关**不共用一个判据**（历史 bug：本函数曾看错开关，只因两者默认值恰好
/// 一致而没暴露）。`FLASH_SELFTEST_AT_BOOT` 管"跑不跑自证"，
/// `FLASH_USE_CONSTRUCTED_CONFIG` 管"用哪份配置"。
#[must_use]
pub const fn using_constructed_config() -> bool {
    super::config::FLASH_USE_CONSTRUCTED_CONFIG
}

/// 用 ROM 驱动擦除一个 4 KiB 扇区。
///
/// # Errors
///
/// 判据不过 ⇒ [`StoreError::Rejected`]；ROM 不可用 ⇒ [`StoreError::Driver`]；
/// ROM 返回非成功码 ⇒ [`StoreError::Driver`]。
pub fn erase_sector(flash_offset: u32) -> Result<(), StoreError> {
    super::store::check_write_gates_usize(
        usize::try_from(flash_offset).map_err(|_| StoreError::Rejected)?,
        usize::try_from(nor_flash::SECTOR_BYTES).map_err(|_| StoreError::Rejected)?,
        true,
    )?;
    let driver = nor_driver().ok_or(StoreError::Driver)?;
    let erase = driver.erase.ok_or(StoreError::Driver)?;
    let mut config = config_in_ram().ok_or(StoreError::Driver)?;
    // SAFETY: 调用 ROM 的扇区擦除。三项前提都已由上面的闸门保证：
    // ① `instance = 0` 是本板唯一的 FlexSPI；② 配置指针指向 Flash 偏移 0 的 FCB
    // （Boot ROM 自己加载的那块，布局即 `flexspi_nor_config_t`）；
    // ③ `flash_offset` 已过"4 字节寻址自检 + 4 KiB 对齐 + 不跨分区 + 只读拒绝 + XIP 映射"五道判据。
    // ROM 代码会操作 FlexSPI 控制器与器件，但不会动我们的 RAM 结构（其参数都是值/ROM 指针）。
    // 地址语义：ROM 的 NOR 驱动要的是 **Flash 内偏移**，不是 XIP 绝对地址。
    // 这一点是**真机二分出来的**：传 `XIP_BASE + 偏移` 时 ROM **瞬间返回错误**
    // （自证 `erase4k=33µs`、`擦回=FAIL`），传偏移则返回成功。
    let status = unsafe {
        erase(
            INSTANCE,
            config.0.as_mut_ptr().cast::<core::ffi::c_void>(),
            flash_offset,
            4096,
        )
    };
    if status == STATUS_SUCCESS {
        // 擦除改变了器件内容 ⇒ 立刻让缓存与之一致（见 `clear_cache` 的说明）。
        clear_cache(driver);
        // ⚠️ ROM 的 `clear_cache` 清的是 **FlexSPI 控制器侧**的 AHB RX/预取缓冲，
        // **不是 Cortex-M7 L1 D-Cache**（见 `cache.rs` 的说明）。FS 的编程路径是
        // "读整页→改→写整页"的 RMW：若目标页在擦除前被读过，L1 里的旧行会让
        // RMW 读回擦除前的内容。因此这里必须显式失效对应范围的 D-Cache。
        let sector = usize::try_from(nor_flash::SECTOR_BYTES).unwrap_or(4096);
        let erase_base =
            crate::store::XIP_BASE.saturating_add(usize::try_from(flash_offset).unwrap_or(0));
        crate::cache::invalidate_dcache_range(erase_base, sector);
        // 擦除后回读：4 KiB 必须全 0xFF。ROM 返回成功不等于器件真的擦干净
        // （2026-10-19 校准要求"报事实"）；失败按 Driver 错误上抛，绝不静默使用。
        if !readback_is_erased(flash_offset) {
            return Err(StoreError::Driver);
        }
        Ok(())
    } else {
        Err(StoreError::Driver)
    }
}

/// 擦除后回读校验：整扇区必须全 `0xFF`。
///
/// 通过 `FlexSpiMedium::read`（安全的批量 XIP 拷贝）而不是逐字节 `read_volatile`：
/// 后者在本板会让 4 KiB 读慢到不可接受，且需要新增 `unsafe`（预算只减不增）。
fn readback_is_erased(flash_offset: u32) -> bool {
    use ports::raw::RawMedium as _;

    let mut medium = crate::store::FlexSpiMedium::new();
    let mut chunk = [0u8; 256];
    let sector = usize::try_from(nor_flash::SECTOR_BYTES).unwrap_or(4096);
    let base = usize::try_from(flash_offset).unwrap_or(0);
    let mut at = 0usize;
    while at < sector {
        match medium.read(base.saturating_add(at), &mut chunk) {
            Ok(n) if n == chunk.len() => {}
            _ => return false,
        }
        if chunk.iter().any(|byte| *byte != 0xFF) {
            return false;
        }
        at = at.saturating_add(chunk.len());
    }
    true
}

/// 用 ROM 驱动编程**一页**（256 B）。`page` 必须正好一页。
///
/// # Errors
///
/// 判据不过 ⇒ [`StoreError::Rejected`]；ROM 不可用或返回非成功码 ⇒ [`StoreError::Driver`]。
pub fn program_page(flash_offset: u32, page: &[u8; 256]) -> Result<(), StoreError> {
    super::store::check_write_gates_usize(
        usize::try_from(flash_offset).map_err(|_| StoreError::Rejected)?,
        page.len(),
        false,
    )?;
    // ⚠️ **2026-10-13：整页编程改用寄存器级实现**（`flexspi_ip::program_page`），
    // **不再走 ROM 的 `program`**。
    //
    // 判据（真机，同一时刻两条路径对照）：
    // - **寄存器路径**：`reg 写路径自证 PASS`，写后从 XIP 读回与图案逐字节一致；
    // - **ROM 路径**：`program` 返回成功、器件却一个字节都没收到（读回全 `0xFF`）。
    //
    // 也就是说 ROM 那份 `program` 在本板**报成功但不写**，而寄存器路径每次传输都读
    // `INTR` 并解码、失败会**如实返回错误** —— 这正是 `flexspi_ip` 模块存在的理由。
    // 页内分块、写使能、`WEL` 断言、等 WIP 都在 `flexspi_ip::program_page` 里。
    super::flexspi_ip::program_page(flash_offset, page).map_err(|_| StoreError::Driver)
}
