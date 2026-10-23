//! FlexSPI 控制器的**寄存器与位域**（照抄官方源码，不猜）。
//!
//! # 为什么这个模块存在
//!
//! 这颗芯片在 `imxrt-ral 0.6.2` 里**没有 FlexSPI 块**（1015/1061 才有），
//! 所以寄存器访问只能靠裸地址 + 位域。裸地址本身不可怕，**可怕的是凭记忆写位域** ——
//! 所以这里把每一个偏移与掩码**逐条照抄**，并在注释里写明出处：
//!
//! - 偏移：CMSIS 设备头 `MIMXRT1021.h` 的 `FLEXSPI_Type` 结构（注释里自带 offset）
//! - 位域：CMSIS 设备头里的 `FLEXSPI_*_MASK` / `*_SHIFT` 宏
//! - 时序：官方驱动 `fsl_flexspi.c` 的 `FLEXSPI_TransferBlocking`（IP 命令 9 步）
//!
//! 本模块**只放常量与纯函数**（可主机测试）；真正的 `volatile` 读写留在 BSP，
//! 与 `mpu-regions`（编码/解码在纯逻辑、寄存器写在 BSP）同一分工。
//!
//! # 这条路的收益：**错误可见**
//!
//! 走 Boot ROM API 时只能看到"返回成功"，而 `INTR` 里的错误位被 ROM 吞掉了 ——
//! 于是"编程返回成功却一个字节都没写"这种故障**没有出口**（几天都只能靠排除法）。
//! 寄存器级路径每次传输都**读 `INTR` 并解码**，静默失败在这里不可能发生。

/// 寄存器偏移（`MIMXRT1021.h` 里 `FLEXSPI_Type` 各字段的 offset 注释）。
pub mod offset {
    /// Module Control Register 0。
    pub const MCR0: usize = 0x000;
    /// Module Control Register 1。
    pub const MCR1: usize = 0x004;
    /// Module Control Register 2。
    pub const MCR2: usize = 0x008;
    /// AHB Bus Control Register。
    pub const AHBCR: usize = 0x00C;
    /// 中断**使能**。
    pub const INTEN: usize = 0x010;
    /// 中断**标志**（错误位的出口）。
    pub const INTR: usize = 0x014;
    /// LUT 解锁键。
    pub const LUTKEY: usize = 0x018;
    /// LUT 控制（写保护）。
    pub const LUTCR: usize = 0x01C;
    /// Flash Control Register 0 基址（每端口 +4）。
    pub const FLSHCR0: usize = 0x060;
    /// Flash Control Register 1 基址（每端口 +4）。
    pub const FLSHCR1: usize = 0x070;
    /// Flash Control Register 2 基址（每端口 +4）—— **清序列指针在它的 bit31**。
    pub const FLSHCR2: usize = 0x080;
    /// IP Control Register 0（命令地址）。
    pub const IPCR0: usize = 0x0A0;
    /// IP Control Register 1（数据量 + 序列号）。
    pub const IPCR1: usize = 0x0A4;
    /// IP Command Register（写 1 触发）。
    pub const IPCMD: usize = 0x0B0;
    /// IP RX FIFO 控制。
    pub const IPRXFCR: usize = 0x0B8;
    /// IP TX FIFO 控制。
    pub const IPTXFCR: usize = 0x0BC;
    /// Status Register 0（含总线空闲位）。
    pub const STS0: usize = 0x0E0;
    /// IP TX FIFO 状态（低 8 位是已填充字数）。
    pub const IPTXFSTS: usize = 0x0F4;
    /// IP RX FIFO 状态（低 8 位是已填充单位数，**每单位 8 字节**）。
    ///
    /// 出处：SDK `devices/RT/RT1020/periph/PERI_FLEXSPI.h:149`
    /// —— `__I uint32_t IPRXFSTS; /**< offset: 0xF0 */`，紧邻 `IPTXFSTS`（`0xF4`，同文件 `:150`）。
    /// ⚠️ 我一度把它写成 `0xF8`（**那是保留区**），于是"等 RX FIFO 有数据"永远等不到 ⇒
    /// 状态读全部超时、擦除也一起失败。偏移必须逐条从 SDK 的结构体注释抄，不能按邻近推。
    pub const IPRXFSTS: usize = 0x0F0;
    /// IP RX FIFO 数据寄存器基址（每字 +4，共 32 个字）。
    pub const RFDR: usize = 0x100;
    /// Flash Control Register 4（`FLSHCR4.WMOPT1` = AHB 写突发的对齐选项）。
    pub const FLSHCR4: usize = 0x094;
    /// DLL Control Register 0 基址（每端口 +4）。
    pub const DLLCR: usize = 0x0C0;
    /// IP TX FIFO 数据寄存器基址（每字 +4，共 32 个字）。
    ///
    /// 注意它与 `RFDR`（`0x100`）**不在同一段**：官方头里 `TFDR[32]` 在 `0x180`。
    pub const TFDR: usize = 0x180;
    /// TX/RX FIFO 的字数（由 CMSIS 里 `TFDR[32]`/`RFDR[32]` 的数组长度给出）。
    pub const FIFO_WORDS: usize = 32;
    /// Lookup Table 基址（每项 +4，共 64 项 = 256 字节）。
    pub const LUT: usize = 0x200;
    /// LUT 项数。
    pub const LUT_ENTRIES: usize = 64;
    /// LUT 里每个序列占几项。
    pub const LUT_ENTRIES_PER_SEQUENCE: usize = 4;

    /// 一条序列在 LUT 里占的**字数**（= [`LUT_ENTRIES_PER_SEQUENCE`]）。
    ///
    /// 要点：**零字等于 `STOP`**（opcode 0 = STOP），所以一条 4 字槽里未被显式写到的字
    /// 必须是 0，否则序列会**跑进残留数据**（实测：残留字非零 ⇒ `INTR.IPCMDERR`）。
    pub const LUT_WORDS_PER_SEQUENCE: usize = LUT_ENTRIES_PER_SEQUENCE;
}

/// 位域掩码与移位（CMSIS 头里的 `FLEXSPI_*_MASK` / `*_SHIFT`）。
pub mod bits {
    /// `MCR0.MDIS`（1 = 模块关闭）。
    pub const MCR0_MDIS: u32 = 0x0000_0002;
    /// `INTR.IPCMDDONE`（命令完成）。
    pub const INTR_IPCMDDONE: u32 = 0x0000_0001;
    /// `INTR.IPCMDGE`（IP 命令授予错误）。
    pub const INTR_IPCMDGE: u32 = 0x0000_0002;
    /// `INTR.IPCMDERR`（IP 命令错误）。
    pub const INTR_IPCMDERR: u32 = 0x0000_0008;
    /// `INTR.SCKSTOPBYWR`（bit9）：**写相位 SCLK 停摆**（TX FIFO 空了）。
    ///
    /// 出处：`devices/RT/RT1020/periph/PERI_FLEXSPI.h` 的
    /// `FLEXSPI_INTR_SCKSTOPBYWR_MASK = 0x200U`，官方注释
    /// "SCLK is stopped during command sequence because Async TX FIFO empty"。
    ///
    /// ⚠️ 这是"页编程写不进去"最直接的官方出口，而本项目的 `Intr` 之前**只解码 bit0/1/3**
    /// ⇒ 它从来没有被上报过（本项目历史上真机抓到过一次 `SCKSTOPBYWR`，却因为没有解码器
    /// 而没进入任何判据）。
    pub const INTR_SCKSTOPBYWR: u32 = 0x0000_0200;
    /// `INTR.SEQTIMEOUT`（bit11）：序列执行超时。
    pub const INTR_SEQTIMEOUT: u32 = 0x0000_0800;
    /// `INTR.IPTXWE`（bit6）：TX FIFO 水位标志（**写 1 清除**）。
    ///
    /// ⚠️ 官方 `FLEXSPI_WriteBlocking` 的 Phase 2 **等的是这个标志**，
    /// 不是轮询 `IPTXFSTS.FILL`。而且**触发前**要先清它（官方 Phase 1 末尾就清），
    /// 否则等待会立刻通过、节奏全乱。
    pub const INTR_IPTXWE: u32 = 0x0000_0040;
    /// `INTR.IPRXWA`（bit5）：RX FIFO 水位标志（写 1 清除）。
    pub const INTR_IPRXWA: u32 = 0x0000_0020;
    /// `FLSHCR2.CLRINSTRPTR`（bit31，清序列指针）。
    pub const FLSHCR2_CLRINSTRPTR: u32 = 0x8000_0000;
    /// `FLSHCR2.ARDSEQID` 的移位（**AHB 读**用哪条 LUT 序列）。
    ///
    /// 出处：CMSIS `MIMXRT1021.h` 的 `FLEXSPI_FLSHCR2_ARDSEQID_SHIFT`。
    pub const FLSHCR2_ARDSEQID_SHIFT: u32 = 0;
    /// `FLSHCR2.ARDSEQID` 的掩码（4 位 ⇒ 序列 0-15）。
    pub const FLSHCR2_ARDSEQID_MASK: u32 = 0x0000_000F;
    /// `FLSHCR2.AWRSEQID` 的移位（**AHB 写**用哪条 LUT 序列）。
    pub const FLSHCR2_AWRSEQID_SHIFT: u32 = 8;
    /// `FLSHCR2.AWRSEQID` 的掩码。
    pub const FLSHCR2_AWRSEQID_MASK: u32 = 0x0000_0F00;
    /// `FLSHCR2.ARDSEQNUM` 的移位（读序列条数减一）。
    pub const FLSHCR2_ARDSEQNUM_SHIFT: u32 = 16;
    /// `FLSHCR2.ARDSEQNUM` 的掩码。
    pub const FLSHCR2_ARDSEQNUM_MASK: u32 = 0x0007_0000;
    /// `FLSHCR2.AWRSEQNUM` 的移位（写序列条数减一）。
    pub const FLSHCR2_AWRSEQNUM_SHIFT: u32 = 24;
    /// `FLSHCR2.AWRSEQNUM` 的掩码。
    pub const FLSHCR2_AWRSEQNUM_MASK: u32 = 0x0700_0000;
    /// `IPCR1.IDATSZ`（低 16 位：数据字节数）。
    pub const IPCR1_IDATSZ_SHIFT: u32 = 0;
    /// `IPCR1.IDATSZ` 掩码。
    pub const IPCR1_IDATSZ_MASK: u32 = 0x0000_FFFF;
    /// `IPCR1.ISEQID`（bit16 起：LUT 序列号）。
    pub const IPCR1_ISEQID_SHIFT: u32 = 16;
    /// `IPCR1.ISEQID` 掩码。
    pub const IPCR1_ISEQID_MASK: u32 = 0x000F_0000;
    /// `IPCR1.ISEQNUM`（bit24 起：序列条数 **减一**）。
    pub const IPCR1_ISEQNUM_SHIFT: u32 = 24;
    /// `IPCR1.ISEQNUM` 掩码。
    pub const IPCR1_ISEQNUM_MASK: u32 = 0x0700_0000;
    /// `IPCMD.TRG`（写 1 触发）。
    pub const IPCMD_TRG: u32 = 0x0000_0001;
    /// `IPRXFCR.CLRIPRXF`（清 RX FIFO）。
    pub const IPRXFCR_CLRIPRXF: u32 = 0x0000_0001;
    /// `IPTXFCR.CLRIPTXF`（清 TX FIFO）。
    pub const IPTXFCR_CLRIPTXF: u32 = 0x0000_0001;
    /// `IPTXFSTS.FILL`（低 8 位：TX FIFO 已填字数）。
    pub const IPTXFSTS_FILL_MASK: u32 = 0x0000_00FF;
    /// `IPTXFCR.TXWMRK`（水位，**以字为单位**）。
    pub const IPTXFCR_TXWMRK_MASK: u32 = 0x0000_003C;
    /// `IPTXFCR.TXWMRK` 的移位。
    pub const IPTXFCR_TXWMRK_SHIFT: u32 = 2;
    /// 打包 `IPTXFCR.TXWMRK` 字段（官方宏 `FLEXSPI_IPTXFCR_TXWMRK(x)`：`(x << 2) & 0x3C`）。
    #[must_use]
    pub const fn txfcr_txwmrk(value: u32) -> u32 {
        (value << 2) & 0x0000_003C
    }

    /// `IPTXFCR.TXWMRK` 的**满容量值**（4 位字段全 1）。
    ///
    /// 字段定义："Watermark level is (TXWMRK+1)*64 Bits"（`PERI_FLEXSPI.h` 的
    /// `FLEXSPI_IPTXFCR_TXWMRK`）⇒ `TXWMRK=15` = 16 字 = 64 字节 = FIFO 全深。
    /// 真机四组对照实测：`TXWMRK=0` 时一次只能推进 1 个字，`=15` 时能推进 16 个字。
    pub const IPTXFCR_TXWMRK_MAX: u32 = 0x0000_003C;
    /// `IPRXFCR.RXWMRK`（水位，单位 8 字节；官方 `FLEXSPI_Init` 写 `rxWatermark/8-1`）。
    pub const IPRXFCR_RXWMRK_MASK: u32 = 0x0000_003C;
    /// `IPRXFCR.RXWMRK` 的移位。
    pub const IPRXFCR_RXWMRK_SHIFT: u32 = 2;
    /// `IPRXFSTS.FILL`（低 8 位）。
    ///
    /// ⚠️ **单位是"字"而不是"字节"**：官方 `FLEXSPI_ReadBlocking` 的判据是
    /// `size > (((IPRXFSTS & FILL_MASK) >> SHIFT) * 8U)` —— 乘 8 说明一个"单位"是 8 字节。
    /// 而 `IPTXFSTS.FILL` 的单位是**字**（官方 `WriteBlocking` 直接与 `txFifoSize`（字数）比较）。
    /// 两个 `FILL` 名字相同、单位不同，是本项目最容易写错的一处（照抄官方判据即可）。
    pub const IPRXFSTS_FILL_MASK: u32 = 0x0000_00FF;
    /// `IPRXFSTS.FILL` 的移位。
    pub const IPRXFSTS_FILL_SHIFT: u32 = 0;
}

/// LUT 序列里的**操作码**（官方 `fsl_flexspi.h` 的 `_flexspi_command` 枚举，SDR 部分）。
pub mod opcode {
    /// 停止执行、释放片选。
    pub const STOP: u32 = 0x00;
    /// 发送命令码。
    pub const SDR: u32 = 0x01;
    /// 发送行地址。
    pub const RADDR: u32 = 0x02;
    /// 发送编程数据。
    pub const WRITE: u32 = 0x08;
    /// 接收读数据。
    pub const READ: u32 = 0x09;
    /// 数据线放开（dummy 周期）。
    pub const DUMMY: u32 = 0x0C;
}

/// LUT 序列里的**数据线根数**（官方 `fsl_flexspi.h` 的 `kFLEXSPI_*PAD`）。
pub mod pad {
    /// 1 根。
    pub const ONE: u32 = 0x00;
    /// 2 根。
    pub const TWO: u32 = 0x01;
    /// 4 根。
    pub const FOUR: u32 = 0x02;
    /// 8 根。
    pub const EIGHT: u32 = 0x03;
}

/// 组装一个 LUT 字（**逐位照抄**官方 `FSL_ROM_FLEXSPI_LUT_SEQ`）。
///
/// 官方宏（`devices/RT/RT1020/MIMXRT1021/drivers/romapi/fsl_romapi.h:55`）：
/// ```c
/// (FLEXSPI_LUT_OPERAND0(op0) | FLEXSPI_LUT_NUM_PADS0(pad0) | FLEXSPI_LUT_OPCODE0(cmd0)
///  | FLEXSPI_LUT_OPERAND1(op1) | FLEXSPI_LUT_NUM_PADS1(pad1) | FLEXSPI_LUT_OPCODE1(cmd1))
/// ```
/// 各字段移位取自 CMSIS：`OPERAND0=0`、`NUM_PADS0=8`、`OPCODE0=10`、
/// `OPERAND1=16`、`NUM_PADS1=24`、`OPCODE1=26`。
#[must_use]
pub const fn lut_seq(cmd0: u32, pad0: u32, op0: u32, cmd1: u32, pad1: u32, op1: u32) -> u32 {
    (op0 & 0xFF)
        | ((pad0 & 0x3) << 8)
        | ((cmd0 & 0x3F) << 10)
        | ((op1 & 0xFF) << 16)
        | ((pad1 & 0x3) << 24)
        | ((cmd1 & 0x3F) << 26)
}

/// **写进配置的 LUT**（`memConfig.lookupTable`，偏移 `0x080`，每条序列占 4 字）。
///
/// # 为什么要在软件里写它（而不是靠 FCB）
///
/// 官方示例允许 `memcpy(&norConfig, CUSTOM_FCB, ...)`（宏 `FLASH_USE_CUSTOM_FCB`），
/// 但那要求 **FCB 里真的带着可用的 LUT**。而我们产物里的那段是填充值 ——
/// 这极可能就是 ROM 路径"能启动、能读、不能写"的根源。
/// 所以这里把 5 条序列**显式写死**，不再依赖 FCB。
///
/// # 关于索引：它是**约定**，不是硬件常量
///
/// ROM API 用 读状态 1 / 写使能 3 / 擦除 5 / 页编程 9（`fsl_romapi.h` 的 `NOR_CMD_LUT_SEQ_IDX_*`），
/// 而 SDK 自己的适配层用 读状态 1 / 写使能 2 / 擦除 3 / 页编程 4、6。
/// **走 ROM 时必须与 ROM 一致**；而我们现在**自己驱动控制器**，索引只需
/// **与我们的 `seq` 常量自我一致**。这里沿用 ROM 的约定（0/1/3/5/9），
/// 免得与既有的 `flash.rs::lut_index` 打架。
pub mod lut_words {
    use super::{lut_seq, opcode, pad};

    /// 读：`0xEC` + 4 字节地址相位 `0x20`，然后 6 个 dummy + quad 读。
    pub const READ: [u32; 2] = [
        lut_seq(opcode::SDR, pad::ONE, 0xEC, opcode::RADDR, pad::FOUR, 0x20),
        lut_seq(
            opcode::DUMMY,
            pad::FOUR,
            0x06,
            opcode::READ,
            pad::FOUR,
            0x04,
        ),
    ];
    /// 读状态：`0x05` + 读 1 字节。
    pub const READ_STATUS: [u32; 1] = [lut_seq(
        opcode::SDR,
        pad::ONE,
        0x05,
        opcode::READ,
        pad::ONE,
        0x01,
    )];
    /// 写使能：`0x06` + 停。
    pub const WRITE_ENABLE: [u32; 1] = [lut_seq(
        opcode::SDR,
        pad::ONE,
        0x06,
        opcode::STOP,
        pad::ONE,
        0,
    )];
    /// 扇区擦除：`0x21` + 4 字节地址相位 `0x20`。
    pub const ERASE_SECTOR: [u32; 1] = [lut_seq(
        opcode::SDR,
        pad::ONE,
        0x21,
        opcode::RADDR,
        pad::ONE,
        0x20,
    )];
    /// 页编程：`0x34` + 4 字节地址相位 `0x20`，然后**单线写** + 停。
    ///
    /// 写相位用 **4PAD** —— 依据是**本板既有 C 固件原文**（`bsp/board/imx_nor_flash/nor/flexspi_romapi.c`
    /// 的 4 字节地址分支，在这块板上跑过现场）：
    /// ```c
    /// lookupTable[4*IDX_PAGEPROGRAM + 0] = LUT_SEQ(CMD_SDR,   1PAD, 0x34, RADDR_SDR, 1PAD, 0x20);
    /// lookupTable[4*IDX_PAGEPROGRAM + 1] = LUT_SEQ(WRITE_SDR, 4PAD, 0x04, STOP,      1PAD, 0);
    /// ```
    /// ⚠️ 我一度按官方 SDK 的 single 表把它改成 1PAD —— 那是**错的** ✗（SDK 表里没有 `0x34`，
    /// 不能拿 `0x02` 的语义套过来）。已回退。
    pub const PAGE_PROGRAM: [u32; 2] = [
        lut_seq(opcode::SDR, pad::ONE, 0x34, opcode::RADDR, pad::ONE, 0x20),
        lut_seq(opcode::WRITE, pad::FOUR, 0x04, opcode::STOP, pad::ONE, 0),
    ];
}

/// LUT 序列号（与官方 `fsl_romapi.h` 的 `NOR_CMD_LUT_SEQ_IDX_*` 完全一致）。
///
/// ⚠️ 这几个数字**必须**与 Boot ROM 的约定一致：ROM 按自己的索引去取 LUT 表项。
/// 我们 FCB 里的 LUT 也是按这套索引排的（`cargo xtask check-image` 从链接产物断言其内容）。
pub mod seq {
    /// 读（`0xEC` + 4 字节地址相位）。
    pub const READ: u8 = 0;
    /// 读状态（`0x05`）。
    pub const READ_STATUS: u8 = 1;
    /// 写使能（`0x06`）。
    pub const WRITE_ENABLE: u8 = 3;
    /// 扇区擦除（`0x21` + 4 字节地址相位）。
    pub const ERASE_SECTOR: u8 = 5;
    /// 页编程（`0x34` + 4 字节地址相位）。
    pub const PAGE_PROGRAM: u8 = 9;
}

/// 一次 IP 命令的类型（决定后面是否要搬运 FIFO 数据）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// 只有命令与地址，没有数据（擦除、写使能）。
    CommandOnly,
    /// 向器件写数据（页编程）。
    Write,
    /// 从器件读数据。
    Read,
}

/// `INTR` 的解码结果 —— **错误可见性的落点**。
///
/// ⚠️ 这里踩过一次语义坑（被测试当场抓住）：`IPCMDGE` 里的 **GE 是 "grant error"（授予错误）**，
/// 是一个**错误位**，不是"已授予"。官方 `FLEXSPI_CheckAndClearError` 把
/// `IPCMDGE` 与 `IPCMDERR` **都当错误**检查。把它当成"已授予"会让判定逻辑完全反过来。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Intr {
    /// `INTR` 的**原始值**。
    ///
    /// ⚠️ 必须留着它：`Intr` 只解码了 3 位，而官方 `INTR` 里还有两个**写路径专属**的出口——
    /// `IPRXWA`(bit5)、以及最关键的 **`SCKSTOPBYWR`(bit9，官方定义
    /// "SCLK is stopped during command sequence because Async TX FIFO empty"**,
    /// `PERI_FLEXSPI.h` 的 `FLEXSPI_INTR_SCKSTOPBYWR_MASK = 0x200`)。
    /// 之前只解码 bit0/1/3 ⇒ "写相位 SCLK 停摆"这个出口**从来没被上报过**。
    pub raw: u32,
    /// 命令是否完成（`IPCMDDONE`）。
    pub done: bool,
    /// 命令**授予错误**（`IPCMDGE`，是个错误位）。
    pub grant_error: bool,
    /// 命令错误（`IPCMDERR`）。
    pub cmd_error: bool,
    /// **诊断附注**（与 `INTR` 无关，只用于把"失败时的那几个中间量"带出到打印方）。
    ///
    /// ⚠️ 为什么单列一个字段：我先前把诊断值打包进 `raw` 的**高 24 位**，
    /// 但 `decode` 只保留低 12 位 ⇒ **那些位当场被清掉**，打印出来永远是 0，
    /// 于是"仪表在跑但读数恒为 0"看起来像"仪表没跑"。教训：
    /// **不要让诊断值穿过一个会裁剪它的解码器**。
    pub note: u32,
}

impl Intr {
    /// 解码（纯函数，可主机测试）。
    #[must_use]
    pub const fn decode(raw: u32) -> Self {
        Self {
            raw,
            note: 0,
            done: raw & bits::INTR_IPCMDDONE != 0,
            grant_error: raw & bits::INTR_IPCMDGE != 0,
            cmd_error: raw & bits::INTR_IPCMDERR != 0,
        }
    }

    /// 是否成功：**完成且两个错误位都没置**。
    ///
    /// 三者缺一不可 —— 只看"函数返回了"（ROM 路径的做法）连 `done` 都看不到，
    /// 这正是先前"编程静默无效、查了几天"的来源。
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.done && !self.grant_error && !self.cmd_error
    }

    /// 给人看的一句话（顺序：先看错误位，再看是否完成）。
    #[must_use]
    pub const fn describe(self) -> &'static str {
        if self.cmd_error {
            "IPCMDERR（命令错误）"
        } else if self.grant_error {
            "IPCMDGE（命令授予错误）"
        } else if self.done {
            "ok"
        } else {
            "未完成（超时或未触发）"
        }
    }
}

/// 打包 `IPCR1`（与官方 `FLEXSPI_TransferBlocking` 的写法逐位一致）。
///
/// 官方原文：
/// ```c
/// configValue  = FLEXSPI_IPCR1_IDATSZ(xfer->dataSize);
/// configValue |= FLEXSPI_IPCR1_ISEQID((uint32_t)xfer->seqIndex)
///             |  FLEXSPI_IPCR1_ISEQNUM((uint32_t)xfer->SeqNumber - 1U);
/// ```
///
/// ⚠️ `seq_count` 在寄存器里存的是**条数减一**（官方就是 `- 1U`），
/// 这里显式减一并把"0 条"当成非法（返回 `None`）。
///
/// 注：这里**不是 `const fn`** —— `u32::from` 在常量上下文里还没稳定，
/// 而"用无损转换而不是 `as`"比"能在 const 里调"重要（`as` 在本 crate 被 deny）。
/// 与 `text.rs` 里 `TextTable::get` 的取舍同一理由。
#[must_use]
pub fn pack_ipcr1(data_size: u32, seq_index: u8, seq_count: u8) -> Option<u32> {
    if seq_count == 0 {
        return None;
    }
    // 用 `u32::from`（放宽方向）而不是 `as`：`u8 -> u32` 是**无损**的，
    // 而 `as` 会让"以后有人把参数改成更宽的类型"变成静默截断。
    let idatsz = data_size & bits::IPCR1_IDATSZ_MASK;
    let iseqid = (u32::from(seq_index) << bits::IPCR1_ISEQID_SHIFT) & bits::IPCR1_ISEQID_MASK;
    let iseqnum =
        (u32::from(seq_count - 1) << bits::IPCR1_ISEQNUM_SHIFT) & bits::IPCR1_ISEQNUM_MASK;
    Some(idatsz | iseqid | iseqnum)
}

/// TX FIFO 的**容量（字数）**。
///
/// 表达式逐字取自官方 `fsl_flexspi.c:818`：
/// ```c
/// uint32_t txFifoSize = ((base->IPTXFCR & FLEXSPI_IPTXFCR_TXWMRK_MASK) >> SHIFT) + 1U;
/// ```
///
/// ⚠️ 注意它是**按字段掩码**算的（`TXWMRK` 只有 4 位 ⇒ `0xF + 1`），
/// **与当前水位值无关** —— 我第一版把"字段当前值 + 1"当容量 ✗，`IPTXFCR=0` 时就得到 1。
///
/// ⚠️⚠️ 更重要的教训：**别拿 `TFDR[32]` 的数组长度当容量** —— 那是寄存器数量，
/// **不是 FIFO 深度**。我按 32 字预灌 ⇒ 超出真实 FIFO ⇒ 传输中途无数据可发 ⇒
/// 真机 `INTR.SCKSTOPBYWR`（写相位 SCLK 停摆）。
#[must_use]
pub fn tx_fifo_words() -> usize {
    usize::try_from((bits::IPTXFCR_TXWMRK_MASK >> bits::IPTXFCR_TXWMRK_SHIFT) + 1).unwrap_or(0)
}

/// LUT 里第 `sequence` 条序列的**字索引**（LUT 是 32 位字数组）。
#[must_use]
pub fn lut_word(sequence: u8) -> Option<usize> {
    let index = usize::from(sequence) * offset::LUT_ENTRIES_PER_SEQUENCE;
    if index < offset::LUT_ENTRIES {
        Some(index)
    } else {
        None
    }
}
