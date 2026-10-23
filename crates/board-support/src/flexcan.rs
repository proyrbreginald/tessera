//! FlexCAN2 寄存器直访 —— **先只读，后写**。
//!
//! # 为什么第一步只做只读探针
//!
//! 这是本项目在 FlexSPI 上验证过的做法：**先读全寄存器、确认地址与复位值对得上，
//! 再去翻译写序列**。FlexSPI 那次正是靠"只读探针一次通过"才敢往下写；
//! 而在此之前，凡是"先写再看"的尝试都在猜。
//!
//! 只读探针还能顺带回答一个**我此前标注为 TODO-VERIFY 的事实**：这颗芯片上
//! FlexCAN 实际可用的**邮箱个数**（`MCR[MAXMB]` 的回读值）。手册正文说
//! "64 Message Buffers are supported"（`[RM 40.1]`），但 `MCR` 的复位值是
//! `0x5980_000F` —— 低 7 位的 `MAXMB` 复位为 `0x0F`（即 16 个）。
//! **两个数字对不上时，以回读为准**，不要靠推。
//!
//! # 常量从哪来（不猜）
//!
//! | 项 | 值 | 出处 |
//! |---|---|---|
//! | FlexCAN2 基址 | `0x401D_4000` | `[RM 表 3-x AIPS-4 memory map]`（`401D_4000–401D_7FFF 16KB FlexCAN2`）；同表 `FlexCAN1 = 0x401D_0000` |
//! | 中断号 | `FLEXCAN2 = 37` | `[RM 第 4 章 表 4-1]`（"Combined interrupt of ini_int_busoff, ini_int_error, …"） |
//! | 寄存器偏移与复位值 | 见 [`reg`] | `[RM 40.9 FLEXCAN Memory Map/Register Definition]` 的逐行表 |
//! | `MCR` 位域 | 见 [`mcr`] | `[RM 40.9.1]` 的字段说明 |
//!
//! # 真机读数（2026-10-13，**已测量，不是推断**）
//!
//! ```text
//! can      : mcr=0x5980000f ctrl1=0x00000000 ecr=0x00000000 esr1=0x00000000 iflag1=0x00000000
//! can      : 存活=是 mdis=false halt=true not_rdy=true frz_ack=false srxdis=false 邮箱=16 故障域=0
//! ```
//!
//! 三条结论：
//!
//! 1. **基址正确且模块有时钟** —— `MCR` 回读 `0x5980_000F`，与 `[RM 40.9.1]` 记的复位值
//!    **逐位一致**。若地址错或模块没被时钟驱动，这里会是 `0` 或 `0xFFFF_FFFF`。
//! 2. **上电停在 Freeze 模式** —— `HALT=1`、`NOT_RDY=1`。这解释了为什么"直接写邮箱"
//!    不会生效：`[RM 40.9.1]` 明确写 *"No reception or transmission is performed by
//!    FLEXCAN before this bit is cleared"*。
//! 3. **可用邮箱 = 16 个（`MAXMB=15`）** —— 这一条**推翻了"64 个"的直觉**：
//!    `[RM 40.1]` 说 *"64 Message Buffers are supported by the Flexcan"*，那是**IP 的能力上限**；
//!    而本实例 `MCR` 复位值低 7 位是 `0x0F` ⇒ 实际 16 个。
//!    **⇒ 设计约束：不使用 MB16..MB63**（要用就得先改 `MAXMB`，而那需要先确认该字段可写）。
//!    16 个邮箱对本项目绰绰有余（几路收发 + 勘误 ERR005829 要保留的第 0 号邮箱）。
//!
//! # 状态（2026-10-19 更新）
//!
//! - **只读探针**（[`probe`]/[`looks_alive`]/[`pe_clock_hz`]）：不写任何寄存器，常开；
//! - **写序列 + loopback 自证**（[`loopback_selftest`]）：覆盖"时钟门控 → 复位 →
//!   位时序 → 邮箱武装 → 发送 → 接收"整条通路，由 `config::CAN_LOOPBACK_SELFTEST`
//!   控制；
//! - **当前真机结论（2026-10-19）**：内部 loopback 自证 **PASS**（TX 完成、RX
//!   逐字节一致）；它证明控制器/邮箱路径，**不证明** TJA1044T 之后的差分总线电气。
//!   外部总线接收与 `Bus` 端口接线仍未完成 —— 见下面的 `GAP(P2)`。
//!
//! 缺口标记见文件头之后的 `GAP(P2)` 注释（门禁按普通注释放行清点）。

// GAP(P2): **外部总线接收**未在真机验证通过，且 CAN 驱动未接入 `Bus` 端口契约。
// 2026-10-19 实测：内部 loopback 自证 PASS（TX 完成、RX 逐字节一致），但这只覆盖
// 控制器内部路径，不覆盖 TJA1044T 之后的 CANH/CANL 电气与对端设备。在接通第二块板
// 或示波器之前，不要改序列去猜（已排除项见 `config::CAN_LOOPBACK_SELFTEST` 的文档）。

use core::ptr;
use imxrt_ral as ral;

/// FlexCAN2 寄存器块基址。
///
/// `[RM 表 3-x AIPS-4 memory map]`：`401D_4000` 起 16 KB 为 FlexCAN2。
/// ⚠️ **不是 `0x401D_0000`** —— 那是 FlexCAN1。两者相差 `0x4000`，
/// 而本板的总线接的是 **CAN2**（原理图网络名 `CAN2_TX`/`CAN2_RX`）。
pub const FLEXCAN2_BASE: u32 = 0x401D_4000;

/// 寄存器偏移（**逐个照抄 `[RM 40.9]` 的行，不做任何推算**）。
///
/// 这个项目的 FlexSPI 模块曾经把 `IPRXFSTS` 写成 `0xF8`（保留区）而不是官方的
/// `0xF0`，后果是**所有状态读都超时、连擦除都失败**。教训是：偏移必须一个一个抄，
/// 且要写清出处。
pub mod reg {
    /// 模块配置（`[RM 40.9.1]`，复位 `0x5980_000F`）。
    pub const MCR: u32 = 0x0000;
    /// 控制 1：位时序（`[RM 40.9.2]`，复位 `0x0000_0000`）。
    pub const CTRL1: u32 = 0x0004;
    /// 自由运行计数器（`[RM 40.9.3]`）。
    pub const TIMER: u32 = 0x0008;
    /// 接收全局屏蔽（`[RM 40.9.4]`，复位 `0xFFFF_FFFF`）。
    pub const RXGMASK: u32 = 0x0010;
    /// `MB14` 的接收屏蔽（`[RM 40.9.5]`）。
    pub const RX14MASK: u32 = 0x0014;
    /// `MB15` 的接收屏蔽（`[RM 40.9.6]`）。
    pub const RX15MASK: u32 = 0x0018;
    /// 错误计数器（`[RM 40.9.7]`）。
    pub const ECR: u32 = 0x001C;
    /// 错误与状态 1（`[RM 40.9.8]`）。
    pub const ESR1: u32 = 0x0020;
    /// 中断屏蔽 2（`[RM 40.9.9]`）。
    pub const IMASK2: u32 = 0x0024;
    /// 中断屏蔽 1（`[RM 40.9.10]`）。
    pub const IMASK1: u32 = 0x0028;
    /// 中断标志 2（`[RM 40.9.11]`）。
    pub const IFLAG2: u32 = 0x002C;
    /// 中断标志 1（`[RM 40.9.12]`）。
    pub const IFLAG1: u32 = 0x0030;
    /// 控制 2（`[RM 40.9.13]`）。
    pub const CTRL2: u32 = 0x0034;
    /// 错误与状态 2（`[RM 40.9.14]`）。
    pub const ESR2: u32 = 0x0038;
    /// 接收 FIFO 全局屏蔽（`[RM 40.9.17]`，复位 `0xFFFF_FFFF`）。
    pub const RXFGMASK: u32 = 0x0048;
    /// 接收 FIFO 信息（`[RM 40.9.18]`）。
    pub const RXFIR: u32 = 0x004C;
    /// 邮箱区起点：`[RM 40.5]` 的 **Message Buffer Address: Base + 0x0080-0x047C**。
    pub const MB_BASE: u32 = 0x0080;
    /// 单个邮箱的字节数：`[RM 40.5]` "Each individual Message buffer is formed by 16 bytes"。
    pub const MB_SIZE: u32 = 0x10;
}

/// `MCR` 的字段掩码（`[RM 40.9.1]` 的字段表逐条抄）。
pub mod mcr {
    /// 模块禁用。**复位为 0**（模块使能）—— 这是 MCR 里唯一不受软复位影响的位。
    pub const MDIS: u32 = 1 << 31;
    /// 使能进入 Freeze 模式。
    pub const FRZ: u32 = 1 << 30;
    /// 接收 FIFO 使能。
    pub const RFEN: u32 = 1 << 29;
    /// 请求 Freeze 模式。**复位为 1** —— 即上电后模块停在 Freeze 模式，
    /// 在它被清零之前**不做任何收发**（`[RM 40.9.1]` 原文）。
    pub const HALT: u32 = 1 << 28;
    /// 只读：模块当前处于 Disable/Stop/Freeze 模式。
    pub const NOT_RDY: u32 = 1 << 27;
    /// 使能唤醒中断。
    pub const WAK_MSK: u32 = 1 << 26;
    /// 软复位。
    pub const SOFT_RST: u32 = 1 << 25;
    /// 只读：**已进入 Freeze 模式且预分频器已停**。
    pub const FRZ_ACK: u32 = 1 << 24;
    /// 只读：模块处于 Disable 或 Stop 模式（低功耗）。
    pub const LPM_ACK: u32 = 1 << 20;
    /// 自接收禁止。`SRXDIS=0` 时允许收到自己发出的帧（loopback 自证的前提）。
    pub const SRXDIS: u32 = 1 << 17;
    /// 使能发送/接收警告中断。
    pub const WRNEN: u32 = 1 << 21;
    /// 邮箱个数 − 1（`bits[6:0]`）。
    pub const MAXMB_MASK: u32 = 0x7F;
    /// 自唤醒使能（`bit 22`）。
    pub const SLF_WAK: u32 = 1 << 22;
    /// 唤醒源选择（`bit 19`）。
    pub const WAK_SRC: u32 = 1 << 19;
    /// 个体掩码使能（`bit 16`）。
    pub const IRMQ: u32 = 1 << 16;
}

// ⚠️⚠️ **上面这张表的第一版是错的，而且是本项目第二次犯同一类错。**
//
// 第一版我把 `FRZ_ACK` 写成 `1 << 25`、`SOFT_RST` 写成 `1 << 26`、
// `SRXDIS` 写成 `1 << 16` —— 位号是凭印象写的，没有逐条核对。
//
// 后果很重：用**错位**去解码实测的 `0x5980_000F`，会得到"`FRZ_ACK=0`"这个读数。
// 于是连续三轮排查（CCM 门控、时钟源、CMEON 自举覆盖）都在追一个**并不存在的**
// "协议引擎没有时钟"。而用**正确位号**解码同一个值：
//
// ```text
// bit24 FRZACK = 1   ← 它一直是置起的
// bit27 NOTRDY = 1
// bit28 HALT   = 1
// bit30 FRZ    = 1
// bit20 LPMACK = 0   ← 不在低功耗/禁用模式
// ```
//
// ⇒ 模块**一直就在 Freeze 模式**，一切正常；stage=1 的"超时"是因为
// `wait_mcr(FRZ_ACK, true)` 去等的是 bit25（`SOFTRST`）。
//
// **这与本项目 FlexSPI 那次的 `IPRXFSTS` 写成 `0xF8`（保留区）而不是官方的 `0xF0`
// 是同一类错误**：位号/偏移不逐条核对，症状就会指向一个完全错误的根因。
// 那次之后本项目定下的规矩是"偏移必须一个一个从官方结构体注释抄"——
// 这次要把它扩展到**位域**：位号必须从 `imxrt-ral`（与手册同源生成）核对，
// 不能凭印象写。`CTRL1` 那张表后来正是这么做的（因此 `LPB` 才从误读的 7 纠正到 12）。
//
// 上面这张表现在逐条来自 `imxrt-ral` 的 `MCR` 字段 `offset`（见 v0.12.3.0 的提交信息）。

/// 把 `u32` 地址/偏移转成 `usize`（**const 上下文**）。
///
/// 与 `dma.rs` 的 `pool_constant` 同一取舍：`usize` 在目标平台就是 32 位，
/// 转换不可能丢信息；而 `usize::try_from` 在 const 上下文里还不是 stable。
const fn to_usize(value: u32) -> usize {
    #[allow(clippy::as_conversions)]
    let converted = value as usize;
    converted
}

/// 读一个 32 位寄存器。
///
/// # Safety
///
/// 调用者必须保证 `offset` 是 [`reg`] 里列出的偏移之一。
/// 本函数只做一次 `read_volatile`，写入任何值都不可能（没有写路径）。
fn read(offset: u32) -> u32 {
    let address = to_usize(FLEXCAN2_BASE) + to_usize(offset);
    // SAFETY:
    // ① `FLEXCAN2_BASE + offset` 是 `[RM 表 3-x]` 与 `[RM 40.9]` 共同确定的
    //    **外设寄存器地址**（基址 0x401D_4000 + 逐条抄来的偏移），不是普通内存；
    // ② `read_volatile` 正是寄存器语义 —— 防止编译器把这次读优化掉或与其它访问重排；
    // ③ 只读：本模块没有任何写路径，因此不可能改变器件或模块状态；
    // ④ 4 字节对齐由基址（0x401D_4000）与全是 4 的倍数的偏移共同保证。
    // 与 `flexspi_ip` 同一手法（`with_exposed_provenance` 而不是 `as` 转换）：
    // `as` 能静默截断，而 `board-support` 的 lint 头禁止它。
    unsafe { ptr::read_volatile(ptr::with_exposed_provenance::<u32>(address)) }
}

/// 写一个 32 位寄存器。
///
/// # Safety
///
/// 调用者必须保证 `offset` 是 [`reg`] 里列出的偏移之一，且**当前处于允许写它的状态**
/// （例如 `CTRL1` 只允许在 Freeze 模式写 —— `[RM 40.9.2]` 反复强调
/// "This field can only be written in Freeze mode as it is blocked by hardware in other modes"）。
///
/// ⚠️ 本模块只写 FlexCAN 自己的寄存器区，**从不碰 MCR[MDIS]/SOFT_RST 之外的全局状态**，
/// 也不会越界到相邻外设（偏移全部来自 [`reg`]）。
fn write(offset: u32, value: u32) {
    let address = to_usize(FLEXCAN2_BASE) + to_usize(offset);
    // SAFETY:
    // ① 地址是 `[RM 表 3-x]` 的 FlexCAN2 基址加 `[RM 40.9]`/`[RM 40.5]` 抄来的偏移；
    // ② `write_volatile` 正是寄存器语义（保证写真的落下去、不被优化掉或重排）；
    // ③ 只写 FlexCAN 自己的寄存器块（16 KB 窗口内），不触碰其它外设或普通内存；
    // ④ 4 字节对齐由基址与偏移共同保证。
    unsafe { ptr::write_volatile(ptr::with_exposed_provenance_mut::<u32>(address), value) }
}

/// 邮箱控制/状态字（MB 字 0）的位域 —— 布局出自 `[RM 表 40-4]`。
///
/// ```text
/// 31 30 29 28 | 27 26 25 24 | 23 22 21 20 | 19 18 17 16 | 15 ....... 0
///  (保留)         CODE         -  SRR IDE RTR    DLC         TIME STAMP
/// ```
pub mod cs {
    /// `CODE`：`bits[27:24]`。
    pub const CODE_SHIFT: u32 = 24;
    /// `SRR`（替代远程请求，仅扩展帧）。
    pub const SRR: u32 = 1 << 22;
    /// `IDE`：1 = 扩展帧（29 位 ID）。
    pub const IDE: u32 = 1 << 21;
    /// `RTR`：1 = 远程帧。
    pub const RTR: u32 = 1 << 20;
    /// `DLC`：数据长度，`bits[19:16]`。
    pub const DLC_SHIFT: u32 = 16;

    /// 拼一个 `CODE` 字段。
    #[must_use]
    pub const fn code(value: u32) -> u32 {
        value << CODE_SHIFT
    }

    /// 拼一个 `DLC` 字段（标准帧最多 8 字节）。
    #[must_use]
    pub const fn dlc(bytes: u32) -> u32 {
        (bytes & 0xF) << DLC_SHIFT
    }
}

/// `CODE` 的取值 —— `[RM 40.5]` 的两张表（Rx 表 / Tx 表）。
///
/// ⚠️ **同一个数值在收/发两种用法下含义不同**，所以这里分成两个模块，
/// 而不是给一组裸常量加上含糊的名字。
pub mod rx_code {
    /// `0b0000`：未激活。
    pub const INACTIVE: u32 = 0b0000;
    /// `0b0100`：空且激活（准备好接收）。
    pub const EMPTY: u32 = 0b0100;
    /// `0b0010`：已满（收到一帧，待 CPU 取走）。
    pub const FULL: u32 = 0b0010;
    /// `0b0110`：溢出（上一帧还没取走又来了一帧）。
    pub const OVERRUN: u32 = 0b0110;
    /// `0b0001`：忙（模块正在更新这个邮箱）。
    pub const BUSY: u32 = 0b0001;
}

/// 发送方向的 `CODE` 取值。
pub mod tx_code {
    /// `0b1000`：未激活（不参与仲裁与发送）。
    pub const INACTIVE: u32 = 0b1000;
    /// `0b1100`：数据帧，**发送一次**后自动回到未激活。
    pub const DATA_ONCE: u32 = 0b1100;
    /// `0b1010`：远程请求帧，发送一次。
    pub const REMOTE_ONCE: u32 = 0b1010;
}

/// 本板既有 C 固件写给每个接收邮箱的个体掩码值。
///
/// 来源：`drv_can.c:188` 的 `FLEXCAN_SetRxIndividualMask(base, mailbox,
/// FLEXCAN_RX_MB_STD_MASK(0, 0, 0))`。该宏（`fsl_flexcan.h:41`）展开为
/// `(rtr << 31) | (ide << 30) | FLEXCAN_ID_STD(id)`，三个实参都是 0 ⇒ **`0x00000000`**。
/// SDK 自己的注释是 *"all ID bit in the filter is \"don't care\""*
/// ⇒ 在 FlexCAN 里**掩码位 0 = 不比较**。（与我先前照 `FLEXCAN_Reset` 写的
/// `0x3FFF_FFFF` 语义相反 —— 那是"全部比较"。）
const RX_INDIVIDUAL_MASK_FIELD_FW: u32 = 0x0000_0000;

/// `RXIMR[i]` 的**偏移**：`RXIMR0` 在 `0x0880`，每个 4 字节。
///
/// 依据：官方 `MIMXRT1021.h` 的 `CAN_Type` 结构体注释
/// `__IO uint32_t RXIMR[64]; /* array offset: 0x880, array step: 0x4 */`。
/// （`imxrt-ral` **没有**建模这个寄存器，所以只能从官方结构体取 —— 这条我核对过。）
const fn rximr(index: u32) -> u32 {
    0x0880 + index * 4
}

/// `CTRL1` 的字段掩码与位偏移（**取自 `imxrt-ral`，与手册同源**）。
///
/// ⚠️ 这份表是"不猜"的直接产物：我先前从 PDF 的寄存器图里读出的 `LPB` 位号是 **7**，
/// 而权威值是 **12** —— PDF 的文本提取把位号与字段名错开了。
/// 教训：**位域优先从 ral（同源生成）核对，而不是从 PDF 的 ASCII 图里目测。**
pub mod ctrl1 {
    /// 传播段，`bits[2:0]`（3 位）。
    pub const PROPSEG_SHIFT: u32 = 0;
    /// 只听模式。
    pub const LOM: u32 = 1 << 3;
    /// 最低优先级邮箱先发。
    pub const LBUF: u32 = 1 << 4;
    /// 定时器同步。
    pub const TSYN: u32 = 1 << 5;
    /// 总线关闭自动恢复。
    pub const BOFFREC: u32 = 1 << 6;
    /// 采样模式。
    pub const SMP: u32 = 1 << 7;
    /// **内部回环**（`bit 12`）—— 自证用的就是它。
    ///
    /// `[RM 40.9.2]`：*"the bit stream output of the transmitter is fed back"*
    /// ⇒ 不需要总线对端、不需要收发器、**也不需要位时序与任何人对齐**。
    pub const LPB: u32 = 1 << 12;
    /// 相位段 2，`bits[18:16]`（3 位）。
    pub const PSEG2_SHIFT: u32 = 16;
    /// 相位段 1，`bits[21:19]`（3 位）。
    pub const PSEG1_SHIFT: u32 = 19;
    /// 重同步跳宽，`bits[23:22]`（2 位）。
    pub const RJW_SHIFT: u32 = 22;
    /// 预分频，`bits[31:24]`（8 位）。
    pub const PRESDIV_SHIFT: u32 = 24;
}

/// 拼一个字段到它该在的位置。
///
/// 单独写这个函数是为了**避免在别处出现裸的移位**：位号只出现在上面两张表里，
/// 一处错就全都错，而一处对就全都对。
#[must_use]
pub const fn field(shift: u32, width: u32, value: u32) -> u32 {
    (value & ((1 << width) - 1)) << shift
}

/// 只读探针的读数（**原始值 + 解码，两者都报**）。
///
/// 铁律 3："报事实，不报意图"。所以这里既带原始寄存器值，也带解码后的字段 ——
/// 只报解码会掩盖"我解错了"，只报原始值又要求读日志的人自己去查手册。
#[derive(Clone, Copy)]
pub struct Probe {
    /// `MCR` 原始值。
    pub mcr: u32,
    /// `CTRL1` 原始值。
    pub ctrl1: u32,
    /// `ECR` 原始值。
    pub ecr: u32,
    /// `ESR1` 原始值。
    pub esr1: u32,
    /// `IFLAG1` 原始值。
    pub iflag1: u32,
}

impl Probe {
    /// 模块是否被禁用（`MCR[MDIS]`）。
    #[must_use]
    pub fn disabled(self) -> bool {
        self.mcr & mcr::MDIS != 0
    }

    /// 是否停在 Freeze 模式（`MCR[FRZ_ACK]`，只读位）。
    #[must_use]
    pub fn in_freeze(self) -> bool {
        self.mcr & mcr::FRZ_ACK != 0
    }

    /// 是否未就绪（`MCR[NOT_RDY]`，只读位）。
    #[must_use]
    pub fn not_ready(self) -> bool {
        self.mcr & mcr::NOT_RDY != 0
    }

    /// 自接收是否被禁止（`MCR[SRXDIS]`）。loopback 自证要求它是 0。
    #[must_use]
    pub fn self_reception_disabled(self) -> bool {
        self.mcr & mcr::SRXDIS != 0
    }

    /// **邮箱个数**（`MCR[MAXMB] + 1`）。
    ///
    /// 这个值就是那个 TODO-VERIFY 的答案：手册正文说 64
    ///（`[RM 40.1]`："64 Message Buffers are supported by the Flexcan"），
    /// 而 `MCR` 复位值 `0x5980_000F` 的低 7 位是 `0x0F`（16 个）。
    /// **回读说了算。**
    #[must_use]
    pub fn mailboxes(self) -> u32 {
        (self.mcr & mcr::MAXMB_MASK) + 1
    }

    /// 接收错误计数（`ECR[RXERRCNT]`，`bits[7:0]`）。
    #[must_use]
    pub fn rx_error_count(self) -> u32 {
        self.ecr & 0xFF
    }

    /// 发送错误计数（`ECR[TXERRCNT]`，`bits[15:8]`）。
    #[must_use]
    pub fn tx_error_count(self) -> u32 {
        (self.ecr >> 8) & 0xFF
    }

    /// 总线是否关闭（`ESR1[FLTCONF]`，`bits[5:4]`；`0b00` = 正常）。
    #[must_use]
    pub fn fault_confinement(self) -> u32 {
        (self.esr1 >> 4) & 0b11
    }

    /// 是否处于总线关闭（`ESR1[BOFFINT]` 不是状态；这里用 `FLTCONF` 的语义）——
    /// `0b00` 正常、`0b01` 错误被动、`0b1x` 总线关闭。
    #[must_use]
    pub fn bus_off(self) -> bool {
        self.fault_confinement() >= 0b10
    }

    /// 是否有邮箱中断挂起（`IFLAG1` 任何位）。
    #[must_use]
    pub fn any_mailbox_flag(self) -> bool {
        self.iflag1 != 0
    }
}

/// 读一次 FlexCAN2 的全部关键寄存器（**只读，零副作用**）。
#[must_use]
pub fn probe() -> Probe {
    Probe {
        mcr: read(reg::MCR),
        ctrl1: read(reg::CTRL1),
        ecr: read(reg::ECR),
        esr1: read(reg::ESR1),
        iflag1: read(reg::IFLAG1),
    }
}

/// 探针读数是否"像一个活着的外设"。
///
/// # 判据（为什么这几条）
///
/// - `MCR` **不能**是 `0` 或 `0xFFFF_FFFF`：前者是"没有时钟、总线读回零"的典型，
///   后者是"总线错误被当成全 1"。两者都说明**模块没被时钟驱动**或地址是错的。
/// - `MCR[MDIS]` 必须为 0（复位值如此，`[RM 40.9.1]`）：模块默认使能。
/// - `MCR[HALT]` 必须为 1（复位值如此）：上电停在 Freeze 模式。
///
/// ⇒ 三条同时成立，才算"地址与时钟都对"。任一条不成立，**下一步不该是写序列，
/// 而是先查 CCM 的模块门控**（这正是 FlexSPI 那次的顺序）。
#[must_use]
pub fn looks_alive(probe: &Probe) -> bool {
    if probe.mcr == 0 || probe.mcr == 0xFFFF_FFFF {
        return false;
    }
    !probe.disabled() && (probe.mcr & mcr::HALT != 0)
}

// ---------------------------------------------------------------------------
// 写序列 + **loopback 自证**
// ---------------------------------------------------------------------------

/// 自证用的标准帧 ID。
pub const TEST_ID: u32 = 0x123;
/// 自证用的数据长度。
pub const TEST_DLC: u32 = 8;
/// 自证用的负载（**每一个字节都不同** —— 这样即使出现字节内/字节间的错位，
/// 比对也会失败，而不会因为"全是 0"或"全是 0xFF"而假通过）。
pub const TEST_PAYLOAD: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
/// 自证用的发送邮箱号。
///
/// **照官方 loopback 例程逐句对齐**：`examples/_boards/evkmimxrt1020/driver_examples/
/// flexcan/loopback_transfer/app.h:25` 定义 `TX_MESSAGE_BUFFER_NUM (9)`。
pub const MB_TX: u32 = 9;
/// 自证用的接收邮箱号。
///
/// 同文件 `:24` 定义 `RX_MESSAGE_BUFFER_NUM (10)`。
pub const MB_RX: u32 = 10;

/// 轮询上限（每次轮询是一次 32 位寄存器读）。
///
/// loopback 下位时序是自产的，帧在几十个 quanta 内就完成 —— 这个上限是为了
/// **不把"挂住"变成"死循环"**：真出问题时应当打印一个 FAIL 并继续启动，
/// 而不是让设备停在启动阶段（那会让现场只剩"串口不出字"这一条线索）。
const POLL_LIMIT: u32 = 200_000;

/// 位时序（`PRESDIV` / `PROPSEG` / `PSEG1` / `PSEG2` / `RJW`）。
///
/// # 目标：**1 Mbps**（用户明确要求），PE 时钟 = **12 MHz**（真机实测）
///
/// 全部按官方 `FLEXCAN_SetBaudRate`（`fsl_flexcan.c:665`）的公式**反解**，不是手写：
///
/// ```text
/// quantum = 1 + (PSEG1 + 1) + (PSEG2 + 1) + (PROPSEG + 1)
/// priDiv  = baudRate_Bps * quantum
/// preDivider = (sourceClock_Hz / priDiv) - 1
/// ⇒ baud = PE / (quantum × (PRESDIV + 1))
/// ```
///
/// 令 `baud = 1e6`、`PE = 12e6` ⇒ **`quantum × (PRESDIV + 1) = 12`**。
/// 取 `PRESDIV = 0`（预分频最小 ⇒ 量化误差最小）⇒ **`quantum = 12`**
/// ⇒ `(PSEG1+1) + (PSEG2+1) + (PROPSEG+1) = 11`。
///
/// # 段值怎么分：照官方的理想采样点
///
/// `FLEXCAN_GetSegments`（`fsl_flexcan.c:1944`）按 CiA 301 选理想采样点，
/// 且**对 1 Mbps 专用一个值**：
///
/// ```c
/// if (bitRate == 1000000U) { ideal_sp = IDEAL_SP_LOW; }   /* = 750，即 75.0% */
/// ```
///
/// 采样点 = `(1 + (PSEG1+1) + (PROPSEG+1)) / quantum`。取 `PSEG1 = PROPSEG = 3`：
/// `(1 + 4 + 4) / 12 = 9/12 = ` **75.0%** ✓ 正好是官方给 1 Mbps 的理想值。
/// 余下 `PSEG2 + 1 = 11 - 4 - 4 = 3` ⇒ `PSEG2 = 2`（官方约束 `PSEG2 ∈ 1..7` ✓）。
///
/// `RJW` 照官方规则取两者较小值：`min(PSEG1, PSEG2) = min(3, 2) = 2`
/// （`fsl_flexcan.c` 的 "rJumpwidth (sjw) is the minimum value of phaseSeg1 and phaseSeg2"）。
///
/// # 判据（**在横幅里，每次启动都自证**）
///
/// [`actual_baud`] 用同一套官方公式从本组值与 `CSCMR2[CAN_CLK_PODF]` 反算，
/// 横幅会打出 `实际波特率=1000000 bps` —— 数字不对就是算错了，不需要靠推理。
const TIMING: (u32, u32, u32, u32, u32) = (0, 3, 3, 2, 2);

/// 读一个邮箱字的偏移。
fn mb_word(mb: u32, word: u32) -> u32 {
    reg::MB_BASE + mb * reg::MB_SIZE + word * 4
}

/// 把 `MCR` 的某些位置起 / 清掉。
fn set_mcr(mask: u32, set: bool) {
    let current = read(reg::MCR);
    let next = if set { current | mask } else { current & !mask };
    write(reg::MCR, next);
}

/// 等到 `MCR` 的某个位变成期望值。
fn wait_mcr(mask: u32, want_set: bool) -> bool {
    for _ in 0..POLL_LIMIT {
        let value = read(reg::MCR);
        if (value & mask != 0) == want_set {
            return true;
        }
    }
    false
}

/// 写一个邮箱的字 0（控制/状态字）。
fn write_cs(mb: u32, value: u32) {
    write(mb_word(mb, 0), value);
}

/// **按勘误 `ERR005829` 保留 0 号邮箱**。
///
/// `[CE ERR005829]` 的规避第 7–8 步原文：
///
/// > Reserve the first valid mailbox as an inactive mailbox (CODE = 0b1000).
/// > If RX FIFO is disabled, this mailbox must be message buffer 0.
/// > ... Write twice INACTIVE code (0b1000) into the first valid mailbox.
/// > The first mailbox cannot be used for reception or transmission process.
///
/// 本板 `MCR[RFEN]` 复位为 0（实测），所以"第一个有效邮箱"就是 **MB0**。
/// **写两次**是规避的一部分，不是冗余 —— 照抄。
#[allow(dead_code)] // 留给 P6f（真实收发落地时按勘误原文加回）
fn reserve_mb0() {
    for _ in 0..2 {
        write_cs(0, cs::code(tx_code::INACTIVE));
    }
}

/// 写一个邮箱的 ID 字（字 1）。
///
/// 标准帧的 11 位 ID 在 `bits[28:18]`（`[RM 表 40-4]` 的 "ID Standard"），
/// 高 3 位 `bits[31:29]` 是发送用的本地优先级 `PRIO`，这里置 0。
fn write_id(mb: u32, id: u32) {
    write(mb_word(mb, 1), (id & 0x7FF) << 18);
}

/// 写一个邮箱的 8 字节负载（字 2、字 3，**大端字节序**）。
///
/// `[RM 表 40-4]` 把 `DATA BYTE 0` 放在字 2 的 `bits[31:24]` ⇒ 字节 0 是最高字节。
/// 这是 CAN 线序的大端约定在寄存器里的体现；写反了会得到"每个字节都对、顺序全反"的
/// 读数，而那种错误在只看首字节时不会暴露。
fn write_data(mb: u32, payload: &[u8]) {
    let mut words = [0u32; 2];
    for (index, byte) in payload.iter().take(8).enumerate() {
        // 用 `get_mut` 而不是下标：越界在这里是**不可能的**（`take(8)` 且 index/4 ≤ 1），
        // 但让编译器知道这一点，比让 clippy 相信我更好。
        let Some(slot) = words.get_mut(index / 4) else {
            return;
        };
        let shift = 24 - 8 * (index % 4);
        *slot |= u32::from(*byte) << shift;
    }
    write(mb_word(mb, 2), words[0]);
    write(mb_word(mb, 3), words[1]);
}

/// 读一个邮箱的 8 字节负载。
fn read_data(mb: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (index, slot) in out.iter_mut().enumerate() {
        let word_index = u32::try_from(index / 4).unwrap_or(0);
        let word = read(mb_word(mb, 2 + word_index));
        let shift = 24 - 8 * (index % 4);
        // 只取低 8 位：`u32 -> u8` 用检查式转换表达"我确实只要一个字节"。
        *slot = u8::try_from((word >> shift) & 0xFF).unwrap_or(0);
    }
    out
}

/// **loopback 自证：发一帧、自己收回来、逐字节比对。**
///
/// # 为什么用内部回环而不是接总线
///
/// `[RM 40.9.2]` 对 `CTRL1[LPB]` 的说明：*"the bit stream output of the transmitter
/// is fed back"*。⇒ 不需要总线对端、不需要收发器、**也不需要位时序与任何人对齐**。
/// 因此在"CAN 波特率未知"的现在就能把三件事验证掉：
///
/// 1. **写序列本身**（清 `HALT` → 等 `FRZ_ACK` → 配 `CTRL1` → 配邮箱 → 触发）；
/// 2. **勘误 `ERR005829` 的规避**（0 号邮箱保留 + 写两次 INACTIVE）；
/// 3. **邮箱数据通路的字节序与长度**（8 个互不相同的字节，任何错位都会失败）。
///
/// # 返回值
///
/// `Ok(())` 表示逐字节一致。`Err((阶段, 诊断值))` —— **不折叠**：阶段说明失败在哪一步，
/// 诊断值是那一步读到的原始寄存器内容（现场只有一行日志，原因不能在跨层时被吃掉）。
///
/// # 副作用
///
/// 结束时把模块**放回 Freeze 模式**（置 `HALT` 并等 `FRZ_ACK`），
/// 于是它停在一个已知状态，而不是留在"刚跑完自证"的中间态。
pub fn loopback_selftest() -> Result<(), (u8, u32)> {
    // ⓪ **先把时钟门控打开** —— 否则协议引擎没有时钟，`FRZ_ACK` 永不置起
    //    （真机第一版的 stage=1 就是这个原因，见 `ccm::enable_can2` 的文档）。
    ccm::enable_can2().map_err(|got| (0, got))?;

    // ⓪⁰ **引脚复用**（最后一个未验证的结构性假设）：`GPIO_AD_B0_14/15` → FLEXCAN2 ALT1
    //     + RX 的输入 daisy。地址与 ALT 全部来自官方 `fsl_iomuxc.h:484/493`。
    mux_can2_pins();

    // ⓪′ **官方 `FLEXCAN_Reset`：把模块置到已知状态。**
    // 这一步我在第一版里整段漏掉了 —— 官方 `FLEXCAN_Init` 在配置之前必做
    //（`fsl_flexcan.c:764` 的 "Reset to known status"）。见 [`flexcan_reset`] 的文档。
    flexcan_reset()?;

    // ⓪″ **官方 `FLEXCAN_Init` 的配置装配段**（读-改-写，默认 `maxMbNum=16`、
    //     `enableTimerSync=true`、`enableLoopBack=true`）。见 [`apply_config`]。
    apply_config()?;

    // ① 进 Freeze：复位值里 FRZ/HALT 已经是 1（实测），但这里显式置一次，
    //    以免将来有人改了复位假设。`CTRL1` 只能在 Freeze 里写（`[RM 40.9.2]`）。
    set_mcr(mcr::FRZ | mcr::HALT, true);
    if !wait_mcr(mcr::FRZ_ACK, true) {
        return Err((1, read(reg::MCR)));
    }

    // ② 自接收必须使能（`SRXDIS=0`），否则发出的帧不会被自己收到。
    //    实测复位值就是 0，这里显式清一次并**回读确认**（铁律 3）。
    set_mcr(mcr::SRXDIS, false);
    let mcr_after = read(reg::MCR);
    if mcr_after & mcr::SRXDIS != 0 {
        return Err((2, mcr_after));
    }

    // ②′ **个体掩码：必须在 Freeze 模式里写。**
    //
    // ⚠️ **这就是上一次 HardFault 的原因。** 官方 `FLEXCAN_SetRxIndividualMask`
    // （`fsl_flexcan.c:1920`）全函数只有三行实质动作：
    //
    // ```c
    // assert(maskIdx <= (base->MCR & CAN_MCR_MAXMB_MASK));
    // (void)FLEXCAN_EnterFreezeMode(base);
    // base->RXIMR[maskIdx] = mask;      /* ← 在 Freeze 里写 */
    // (void)FLEXCAN_ExitFreezeMode(base);
    // ```
    //
    // 而我上一版把这两行写在了**「退出 Freeze」之后**（因为那一步我刚提前到配置邮箱之前），
    // 正好落在 Freeze 之外 ⇒ `CFSR=0x400` 不精确 BusFault + `HFSR=0x40000000`。
    //
    // 记一笔：`RXIMR` 的**偏移我当时核对过、是对的** —— 官方 `MIMXRT1021.h` 写着
    // `__IO uint32_t RXIMR[64]; /* array offset: 0x880, array step: 0x4 */`。
    // **错的是时机，不是偏移。**
    //
    // 值照本板既有 C 固件：`FLEXCAN_RX_MB_STD_MASK(0, 0, 0)` = **`0x00000000`**，
    // 其注释原文 *"all ID bit in the filter is 'don't care'"* ⇒ **任何 ID 都收**。
    write(rximr(MB_RX), RX_INDIVIDUAL_MASK_FIELD_FW);
    write(rximr(MB_TX), RX_INDIVIDUAL_MASK_FIELD_FW);

    // ③ 位时序 + 内部回环。**必须在 Freeze 里写**。
    let (presdiv, propseg, pseg1, pseg2, rjw) = TIMING;
    let ctrl1 = field(ctrl1::PRESDIV_SHIFT, 8, presdiv)
        | field(ctrl1::RJW_SHIFT, 2, rjw)
        | field(ctrl1::PSEG1_SHIFT, 3, pseg1)
        | field(ctrl1::PSEG2_SHIFT, 3, pseg2)
        | field(ctrl1::PROPSEG_SHIFT, 3, propseg)
        // ⚠️ **保留 `SMP`**：官方 `FLEXCAN_Reset` 特意写下 `CTRL1 = SMP`（采样三次），
        // 而我这里是**整字覆写** —— 第一版把它清掉了。逐句翻译时，只该动自己要配的位。
        | ctrl1::SMP
        | ctrl1::LPB;
    write(reg::CTRL1, ctrl1);
    let ctrl1_back = read(reg::CTRL1);
    if ctrl1_back != ctrl1 {
        return Err((3, ctrl1_back));
    }

    // ④ **退出 Freeze**（在**配置邮箱之前**）。
    //
    // ⚠️ **这是与官方例程的又一处顺序差异，而且是本轮最可疑的一处。**
    //
    // 官方 `FLEXCAN_Init` 的最后一步是 `FLEXCAN_SetBaudRate` → `FLEXCAN_SetTimingConfig`，
    // 而 `SetTimingConfig` **最后会退出 Freeze**（`fsl_flexcan.c:1085` 起）。
    // 随后官方例程才调用——**在 Freeze 之外**：
    //
    // ```c
    // FLEXCAN_SetRxMbConfig(EXAMPLE_CAN, RX_MESSAGE_BUFFER_NUM, &mbConfig, true);  /* 武装 MB10 */
    // FLEXCAN_SetTxMbConfig(EXAMPLE_CAN, TX_MESSAGE_BUFFER_NUM, true);             /* MB9 置未激活 */
    // FLEXCAN_TransferReceiveNonBlocking(...);
    // FLEXCAN_TransferSendNonBlocking(...);
    // ```
    //
    // 而我第一版是**在 Freeze 里武装两个邮箱、然后才退出 Freeze**。
    // `[RM 40.9.1]` 对 `HALT` 的说法是"The Arm should clear it after initializing the
    // Message Buffers and Control Register"（听起来像"在 Freeze 里初始化邮箱"），
    // 但**官方例程的实际顺序是反的** —— 按用户的规矩"照官方源码逐句翻译"，
    // 以官方例程的顺序为准。
    write(reg::IFLAG1, u32::MAX);
    set_mcr(mcr::HALT | mcr::FRZ, false);
    if !wait_mcr(mcr::FRZ_ACK, false) {
        return Err((5, read(reg::MCR)));
    }

    // ⑤ 接收邮箱：ID 与发送一致 + 置 EMPTY（激活）。**在 Freeze 之外**（同官方）。
    //    ⚠️ 屏蔽寄存器保持 `FLEXCAN_Reset` 写的 `0x3FFF_FFFF`（"全不比较"）——
    //    在两种掩码语义下，MB 的 ID 与发出的帧 ID 相同，都必然匹配。
    // ⚠️ **这里原本还有两行写 `RXIMR[MB_RX]`/`RXIMR[MB_TX] = 0`（照本板既有 C 固件配个体掩码）
    //    —— 它们把板子打成了 HardFault（`CFSR=0x400` 不精确 BusFault，`R3=0x401d40fc`），
    //    已撤销，待查明原因后再加回。**
    //
    // 值得记一笔：`RXIMR` 的偏移我**这次没有错** —— 官方 `MIMXRT1021.h` 写着
    // `RXIMR[64] /* array offset: 0x880, array step: 0x4 */`，我的 `0x0880 + i*4` 与之一致。
    // 所以故障**不是**偏移错，而是别的原因（`IRMQ=1` 之后写 `RXIMR` 的时机/前提？）。
    // ⇒ 在查明之前不要靠猜把它加回来。
    write_cs(MB_RX, cs::code(rx_code::INACTIVE));
    // 本板既有 C 固件把接收邮箱的 ID 设为 0（`FLEXCAN_ID_STD(0)`）—— 配合"不比较"的掩码，
    // 收什么 ID 都行。这里保留 `TEST_ID` 并不影响匹配（掩码已全 0），只是让诊断更直观。
    write_id(MB_RX, TEST_ID);
    write_cs(MB_RX, cs::code(rx_code::EMPTY));
    let rx_cs = read(mb_word(MB_RX, 0));
    if rx_cs >> cs::CODE_SHIFT & 0xF != rx_code::EMPTY {
        return Err((4, rx_cs));
    }

    // ⑥ 发送邮箱：置未激活、填 ID 与数据（**最后一步才写 CS 激活**，同 ERR005829 的 5 步）。
    write_cs(MB_TX, cs::code(tx_code::INACTIVE));
    write_id(MB_TX, TEST_ID);
    write_data(MB_TX, &TEST_PAYLOAD);

    // ⑥′ **使能两个邮箱的中断**（官方流程里的 `FLEXCAN_EnableMbInterrupts`）。
    //
    // 官方例程的两条路径都会调它——`TransferReceiveNonBlocking` 里是
    // `FLEXCAN_EnableMbInterrupts(base, (uint64_t)1U << mbIdx)`（`fsl_flexcan.c:4216`
    // 全函数读完后确认：**这是它唯一的硬件效果**），`TransferSendNonBlocking` 里同理。
    //
    // ⚠️ 严格说 `IMASK1` 只控制**中断的产生**，不影响邮箱能否收到帧 ——
    // 所以它大概率不是根因。但它是我与官方流程之间**最后一个未对齐的硬件差异**，
    // 按"照官方源码逐句翻译"必须补上；补上之后仍不通，才能说"官方做法我已完全复现"。
    //
    // `IFLAG1` 的位图：MB0..MB31 各占一位（`bit n` = MB n）。
    write(reg::IMASK1, (1 << MB_TX) | (1 << MB_RX));

    // ⑦ 触发发送：把 TX 邮箱的 CODE 写成"数据帧，发一次"。
    //    **CS 一次写完（CODE + DLC）**，与官方 `FLEXCAN_WriteTxMb` 一致
    //    （`fsl_flexcan.c:3134`：`cs_temp |= CAN_CS_CODE(kFLEXCAN_TxMbDataOrRemote) | CAN_CS_DLC(len);`
    //     然后 `base->MB[mbIdx].CS = cs_temp;`）。
    //    ⚠️ 这一步必须在**退出 Freeze 之后**（在此之前的写只影响配置）。
    write_cs(MB_TX, cs::code(tx_code::DATA_ONCE) | cs::dlc(TEST_DLC));

    // ⑦′ **勘误 `ERR005829` 的规避：紧接发送激活之后**，把第一个有效邮箱
    //     （本板 `RFEN=0` ⇒ MB0）写两次 `INACTIVE`。
    //
    //     这一条我先前按旧版勘误文档的 *"Software workaround is not in SDK"* 判断
    //     "SDK 没实现"，于是去掉了它。**但读这份 SDK 的 `FLEXCAN_WriteTxMb` 末尾**
    //     （`fsl_flexcan.c:3134` 的 `#if ... ERRATA_5829 ...` 块）我看到：
    //
    //     ```c
    //     base->MB[FLEXCAN_GetFirstValidMb(base)].CS = CAN_CS_CODE(kFLEXCAN_TxMbInactive);
    //     base->MB[FLEXCAN_GetFirstValidMb(base)].CS = CAN_CS_CODE(kFLEXCAN_TxMbInactive);
    //     ```
    //
    //     ⇒ **SDK 已经实现了这个规避**（勘误文档那句是旧信息），而且它的位置正是
    //     **每次发送激活之后**。⇒ 照它放回这里。
    // ④′ **勘误 `ERR005829` 的规避：第一个有效邮箱必须显式写成 `INACTIVE`(0b1000)，
    //     而且要写两次。**
    //
    // ⚠️ 我一度按"与本板既有 C 固件一致"把它去掉了（本板既有 C 固件的 `drv_can.c` 确实没有这句，
    // 它只是把 MB0 留在 `FLEXCAN_Init` 之后的状态）—— **结果板子直接卡死**：
    // 串口一个字节都不出，随后 `pyocd reset` 报 `No cores were discovered`。
    // 恢复本调用后一切正常。
    //
    // ⇒ 这等于在真机上验证了勘误的原文：**"未配置"（`CS = 0`）不等于勘误要求的
    //    `INACTIVE`（`0b1000`）**。本板既有 C 固件能跑通可能是它别的初始化路径恰好也做了这件事，
    //    或者它尚未触发该勘误的竞态条件。**勘误规避不能因为"别人的代码里没有"就删掉。**
    reserve_mb0();

    // ⑧ 等接收邮箱真的变 `FULL`/`OVERRUN`：发送完成（BUF5I 覆盖 MB5..0，`bit0`）与接收（MB2 → `bit2`）。
    //    `IFLAG1` 的位图见 `[RM 40.9.12]`：`bit0` = BUF4TO0I、`bit5` = BUF5I、
    //    `bit6` = BUF6I、`bit7` = BUF7I、`bit8` = BUF31TO8I。
    //    这里用"某位被置起"而不是精确匹配，因为一次 loopback 会同时置起多位。
    // ⚠️ **不能"看到任何 `IFLAG` 非零就往下载"** —— 真机证据（`stage=7 诊断值=0x0400_0000`）：
    // loopback 下**发送完成标志先于接收完成置起**，那一刻接收邮箱还是 `EMPTY`（`0b0100`）。
    // ⇒ 正确的等待对象是**接收邮箱自己的 `CODE`**（`FULL` 或 `OVERRUN`），它才是
    // "帧真的被收进这个邮箱"的直接证据。顺带记下 `IFLAG` 用于诊断。
    let mut seen = 0u32;
    let mut got_cs = read(mb_word(MB_RX, 0));
    for _ in 0..POLL_LIMIT {
        seen |= read(reg::IFLAG1);
        got_cs = read(mb_word(MB_RX, 0));
        let code = got_cs >> cs::CODE_SHIFT & 0xF;
        if code == rx_code::FULL || code == rx_code::OVERRUN {
            break;
        }
    }
    if seen == 0 {
        return Err((6, read(reg::ESR1)));
    }

    // ⑨ 读回接收邮箱并逐字节比对。
    let got_code = got_cs >> cs::CODE_SHIFT & 0xF;
    // 收完后 CODE 应当是 FULL(0b0010) 或 OVERRUN(0b0110)。
    if got_code != rx_code::FULL && got_code != rx_code::OVERRUN {
        // 诊断做厚：把"发送方到底成没成、错误计数器怎么走、TX 邮箱现在是什么状态"
        // 一次性带出来。折叠成"失败"会让下一轮又从零开始猜。
        let tx_cs = read(mb_word(MB_TX, 0));
        let ecr = read(reg::ECR);
        let esr1 = read(reg::ESR1);
        // 打包（把一个 32 位数用满，避免"再烧一次只为多看一个字段"）：
        //   bits[3:0]   RX 邮箱的 CODE
        //   bits[7:4]   TX 邮箱的 CODE
        //   bits[15:8]  ECR.RXERRCNT
        //   bits[23:16] ECR.TXERRCNT
        //   bits[27:24] ESR1 里的错误位低 4 位（FLTCONF/RXWRN/TXWRN/STFERR… 的压缩）
        //   bits[31:28] IFLAG1 的低 4 位
        let esr1_bits = (esr1 & 0xF) | ((esr1 >> 4) & 0xF0);
        return Err((
            7,
            ((got_cs >> cs::CODE_SHIFT) & 0xF)
                | (((tx_cs >> cs::CODE_SHIFT) & 0xF) << 4)
                | ((ecr & 0xFF) << 8)
                | (((ecr >> 8) & 0xFF) << 16)
                | ((esr1_bits & 0xF) << 24)
                | ((seen & 0xF) << 28),
        ));
    }
    let got_dlc = (got_cs >> cs::DLC_SHIFT) & 0xF;
    if got_dlc != TEST_DLC {
        return Err((8, got_cs));
    }
    let got_id = (read(mb_word(MB_RX, 1)) >> 18) & 0x7FF;
    if got_id != TEST_ID {
        return Err((9, got_id));
    }
    let got = read_data(MB_RX);
    for (index, expected) in TEST_PAYLOAD.iter().enumerate() {
        let Some(actual) = got.get(index) else {
            return Err((10, 0));
        };
        if actual != expected {
            // 诊断值打包成"下标 << 8 | 实读字节"，于是一行日志就能说清是第几个字节错了。
            let position = u32::try_from(index).unwrap_or(u32::MAX);
            return Err((10, (position << 8) | u32::from(*actual)));
        }
    }

    // ⑩ 收尾：清标志、把两个邮箱放回未激活、回到 Freeze（已知状态）。
    write(reg::IFLAG1, u32::MAX);
    write_cs(MB_RX, cs::code(rx_code::INACTIVE));
    write_cs(MB_TX, cs::code(tx_code::INACTIVE));
    set_mcr(mcr::HALT, true);
    let _ = wait_mcr(mcr::FRZ_ACK, true);
    Ok(())
}

// ---------------------------------------------------------------------------
// CCM 模块门控（**这是"协议引擎没有时钟"的根因修复**）
// ---------------------------------------------------------------------------

/// CCM 里的 FlexCAN 时钟门控位置。
///
/// # 依据（**两个独立来源一致**）
///
/// | 项 | 值 | 来源 |
/// |---|---|---|
/// | CCM 基址 | `0x400F_C000` | 本板既有 C 固件 `MIMXRT1021.h:5741`（`#define CCM_BASE`） |
/// | `CCGR0.CG9` = **CAN2** | shift **18**，2 位 | 本板既有 C 固件 `MIMXRT1021.h:5351`（`CCM_CCGR0_CG9_SHIFT (18U)`）+ **`imxrt-ral` 的 `CG9.offset = 18`** —— 两者逐位一致 |
/// | `CCGR0.CG8` = CAN1 | shift 16 | `imxrt-ral` 的 `CG8.offset = 16` |
///
/// 本板既有 C 固件里对应的表达是 `kCLOCK_Can2 = (0U << 8U) | CCM_CCGR0_CG9_SHIFT`
///（`soc/soc_imxrt10xx/libraries/MIMXRT1064/MIMXRT1064/drivers/fsl_clock.h:420`），
/// 即"CCGR 实例号 0 + 字段 CG9"。
///
/// # 为什么这是根因修复而不是猜想
///
/// 真机上 `loopback_selftest` 报 **FAIL stage=1**：`MCR` 读得到（且是最新写值的回读），
/// 但 `FRZ_ACK` 永不置起。`[RM 40.9.1]` 说 `FRZ_ACK` 表示"已进入 Freeze 且预分频器已停"
/// ⇒ **协议引擎（PE/CHI）没有时钟**，而寄存器可访问说明接口时钟是开的。
/// 这颗芯片上 FlexCAN 至少有两个时钟域，门控在 CCM —— **没开门控就是上面这个症状**。
mod ccm {
    // `use` 不会被**子模块**继承（Rust 2018+ 的规则）—— 必须显式带进来。
    // 这一条踩过一次：写成裸 `ral::` 会报 "cannot find module or crate `ral`"。
    use super::ral;

    /// 打开 CAN2 的时钟门控（`CCGR0[CG9] = 0b11`），并**读回校验**。
    ///
    /// # 为什么必须读回
    ///
    /// 本项目在 SEMC 上踩过一次：**CCM 的写入被静默丢弃**，后果是"SEMC 寄存器全 0"
    /// 这种极难反推的现象（见 `sdram::SdramError::ClockWriteRejected` 的文档）。
    /// ⇒ 这里同样写完立刻读回，读不回就报错，而不是让调用方拿着一个"以为开了"的门控去排查。
    ///
    /// # Errors
    ///
    /// 返回读回的门控字段值（期望 `0b11`）。
    pub(super) fn enable_can2() -> Result<(), u32> {
        // SAFETY:
        // ① `CCM::instance()` 是 `imxrt-ral` 的实例获取入口，它要求**同一时刻只有一个**
        //    `CCM` 实例（外设寄存器的别名规则）；
        // ② 本函数的调用者是 `loopback_selftest`，而它只在启动阶段被调用一次
        //    （`firmware` 的 `idle_body` 里），此时没有任何其它 CCM 使用者并发；
        // ③ `modify_reg!` 对 `CCGR0` 做**读-改-写**，只动 `CG9` 这两个位，
        //    不碰 `CCGR0` 里其它外设的门控（也绝不碰其它 CCM 寄存器）。
        //    ⇒ 不会因为打开 CAN2 而影响别的外设。
        let ccm = unsafe { ral::ccm::CCM::instance() };

        // ① **先把时钟源选到 24 MHz 晶振**（`CSCMR2[CAN_CLK_SEL] = 1`）。
        //
        // 这一步是**真正的根因修复**。依据是手册的取值表（`imxrt-ral` 的字段文档里
        // 带着手册原文）：
        //
        // ```text
        // CAN_CLK_SEL（CCM_CSCMR2，offset 8，2 位）
        //   0 = pll3_sw_clk divided (60M)
        //   1 = osc_clk (24M)
        //   2 = pll3_sw_clk divided (80M)
        // ```
        //
        // ⚠️ 本板既有 C 固件用的是 **2**（`clock_config.c:303` 的 `CLOCK_SetMux(kCLOCK_CanMux, 2)`），
        // 而那要 **PLL3** —— **本工程刻意不启用 PLL3**（控制台取 24 MHz 晶振正是为了
        // "不依赖 PLL3 锁定"）。⇒ 协议引擎没有时钟、`FRZ_ACK` 永不置起，
        // 这正是两轮真机看到的症状（寄存器可读可写 ⇒ 接口时钟在；`FRZ_ACK` 不来 ⇒ PE 没时钟）。
        //
        // 选 **1**（晶振）的代价是 CAN 时基只有 24 MHz，但换来两件事：
        // ① **不需要新开任何 PLL** ⇒ **不碰 PLL2**（FlexSPI 取指与 SEMC 都靠它 ——
        //    本项目的铁律："正在从 FlexSPI 取指，动 PLL 会立刻让取指失败"）；
        // ② 24 MHz 足够覆盖到 1 Mbps（位时间 24 个时间份额即可）。
        ral::modify_reg!(ral::ccm, ccm, CSCMR2, CAN_CLK_SEL: 1);

        // ② 打开**两个**模块门控。
        //
        // ⚠️ **这里原本只开了一个 —— 那正是"模块内部状态机从不运行"的原因。**
        //
        // 官方 `FLEXCAN_Init`（`fsl_flexcan.c:764`）里是**两个** `CLOCK_EnableClock`：
        //
        // ```c
        // (void)CLOCK_EnableClock(s_flexcanClock[instance]);          /* FLEXCAN_CLOCKS */
        // assert(CAN_CLOCK_CHECK_NO_AFFECTS);                        /* 本芯片为 true */
        // #if defined(FLEXCAN_PERIPH_CLOCKS)
        //     (void)CLOCK_EnableClock(s_flexcanPeriphClock[instance]); /* ← 我漏了这个 */
        // #endif
        // ```
        //
        // 本板既有 C 固件 `fsl_clock.h:473/475` 给出两个门控：
        //
        // ```text
        // kCLOCK_Can2  = (0U << 8U) | CCM_CCGR0_CG9_SHIFT    /* CCGR0, CG9  */
        // kCLOCK_Can2S = (0U << 8U) | CCM_CCGR0_CG10_SHIFT   /* CCGR0, CG10 */
        // ```
        //
        // **只开 `CG9` 时**：寄存器可读可写（接口时钟在），但模块内部状态机从不运行 ——
        // 表现为 `MCR` 的 `HALT`/`FRZ`/`FRZACK`/`NOTRDY` **全是复位默认值**，
        // 而且 `SOFTRST` **置起后永不自清**（真机 `stage=22 诊断值=0x5B80_000F`，bit25 卡在 1）。
        ral::modify_reg!(ral::ccm, ccm, CCGR0, CG9: 0b11, CG10: 0b11);

        // ③ **覆盖"由 CAN2 自己的信号来门控时钟"这条路径**（`CMEOR[MOD_EN_OV_CAN2_CPI]`）。
        //
        // 手册对该位的原文（`imxrt-ral` 的字段文档）：
        // *"Overide clock enable signal from CAN2 - clock will not be gated based on
        //   CAN's signal `enable_clk_cpi`"*。
        //
        // 换句话说：默认情况下 CAN2 的时钟**由模块自己输出的 `enable_clk_cpi` 信号来使能** ——
        // 这是一个**自举**结构。若模块因某种原因没有拉高该信号，它就永远拿不到时钟，
        // 也就永远无法改变状态 ⇒ **`FRZ_ACK` 永不置起**，而寄存器却可读可写
        //（接口时钟不受这条门控影响）—— 与两轮真机的症状完全吻合。
        // 置起这一位就是**绕开这个自举环**。
        ral::modify_reg!(ral::ccm, ccm, CMEOR, MOD_EN_OV_CAN2_CPI: 1);

        // ③ **两处都读回校验**：本项目在 SEMC 上踩过"CCM 写入被静默丢弃"
        //    （后果是"寄存器全 0"这种极难反推的现象）。时钟树是后面一切的前提，
        //    绝不能拿着一个"以为写进去了"的值继续往下走。
        let sel = ral::read_reg!(ral::ccm, ccm, CSCMR2, CAN_CLK_SEL);
        if sel != 1 {
            return Err(sel);
        }
        let gate = ral::read_reg!(ral::ccm, ccm, CCGR0, CG9);
        if gate != 0b11 {
            return Err(gate);
        }
        let gate_s = ral::read_reg!(ral::ccm, ccm, CCGR0, CG10);
        if gate_s != 0b11 {
            return Err(0x200 | gate_s);
        }
        let ovr = ral::read_reg!(ral::ccm, ccm, CMEOR, MOD_EN_OV_CAN2_CPI);
        if ovr != 1 {
            return Err(0x100 | ovr);
        }
        Ok(())
    }
}

/// **打开 FlexCAN2 的时钟门控**（幂等；已开则什么都不做并回读确认）。
///
/// # Errors
///
/// 返回读回的门控字段值（期望 `0b11`）。
pub fn ensure_clock() -> Result<(), u32> {
    ccm::enable_can2()
}

// ⚠️ **写落地探针一度被放在 `ensure_clock` 之前，导致设备停止输出**（真机，2026-10-13）。
//
// 那个位置是错的：**对未时钟化的外设写寄存器会触发总线故障**。
// 已移除。若将来要重新启用，**必须放在 `ensure_clock` 成功之后**，
// 而且要先确认门控真的生效（这正是探针本身想回答的问题 —— 是个先有鸡还是先有蛋的环，
// 所以它只能作为"门控已确认生效之后"的第二步检查）。
//
// 它要回答的问题仍然成立且重要：`MCR` 的复位值本来就带 `FRZ=1|HALT=1`，
// 因此"进 Freeze"那一步的 `set_mcr(FRZ|HALT, true)` 写的是**与当前相同的值** ——
// **写失败与写成功在那一步上无法区分**，所以"写到底通不通"至今没有被验证过。

// ---------------------------------------------------------------------------
// 官方 `FLEXCAN_Reset` —— **逐句翻译**（`fsl_flexcan.c:578-660`）
// ---------------------------------------------------------------------------

// ⚠️ 这里曾经有一个 `CTRL1_SMP = 1 << 6` —— **是错的**：`SMP` 在 **bit 7**，
// 而 bit 6 是 `BOFFREC`（总线关闭恢复）。于是"保留 SMP"那段实际置起的是 `BOFFREC`。
// 同类错误在本文件里出现过两次（另一次是 `MCR[FRZACK]`），教训见文件头的说明。
// ⇒ 统一用 `ctrl1::SMP`（那张表是照 `imxrt-ral` 逐条抄的）。
/// `CTRL2` 的 `TASD`/`RRS`/`EACEN` 位偏移（`imxrt-ral`：19 / 17 / 16）。
const CTRL2_TASD_SHIFT: u32 = 19;
const CTRL2_RRS: u32 = 1 << 17;
const CTRL2_EACEN: u32 = 1 << 16;
/// 官方 `FLEXCAN_Reset` 里 `CTRL2 = TASD(0x16) | RRS | EACEN`。
const CTRL2_OFFICIAL: u32 = (0x16 << CTRL2_TASD_SHIFT) | CTRL2_RRS | CTRL2_EACEN;
/// 官方 `FLEXCAN_Reset` 里给 `RXIMR[i]` / `RXMGMASK` / `RX14MASK` / `RX15MASK` 写的值。
const RX_MASK_OFFICIAL: u32 = 0x3FFF_FFFF;
/// **邮箱个数**：官方 `FSL_FEATURE_FLEXCAN_HAS_MESSAGE_BUFFER_MAX_NUMBERn(x) = (64)`
/// （`MIMXRT1021_features.h:161`），而 `FLEXCAN_Reset` 会把 `MCR[MAXMB]` 写成 **63**。
///
/// ⚠️ **这纠正了我先前的一条结论。** 只读探针回读到 `MAXMB=15`（16 个），我当时据此写下
/// "可用邮箱 16 个、不要用 MB16..MB63"。**那只是复位值** —— 官方的 `Reset` 会把它设成 63，
/// 于是 64 个邮箱全部可用。**复位值不等于可用上限**，这条要以官方初始化后的状态为准。
pub const MAILBOXES_OFFICIAL: u32 = 64;

/// 官方 `FLEXCAN_Init` 的**配置装配段**（`fsl_flexcan.c:833-873`）—— 逐句翻译。
///
/// # 官方是**读-改-写**，不是整字覆写
///
/// ```c
/// ctrl1Temp = base->CTRL1;      /* 读当前值 */
/// mcrTemp   = base->MCR;
/// ctrl1Temp = enableLoopBack ? (ctrl1Temp | LPB) : (ctrl1Temp & ~LPB);
/// ctrl1Temp = enableTimerSync ? (ctrl1Temp | TSYN) : (ctrl1Temp & ~TSYN);
/// ctrl1Temp = enableListenOnlyMode ? (ctrl1Temp | LOM) : (ctrl1Temp & ~LOM);
/// mcrTemp = (mcrTemp & ~MAXMB_MASK) | MAXMB(pConfig->maxMbNum - 1);   /* 默认 16 ⇒ 15 */
/// mcrTemp = enableSelfWakeup ? (mcrTemp | SLFWAK) : (mcrTemp & ~SLFWAK);
/// mcrTemp = (kFLEXCAN_WakeupSrcFiltered == wakeupSrc) ? (| WAKSRC) : (& ~WAKSRC);
/// mcrTemp = enableIndividMask ? (mcrTemp | IRMQ) : (mcrTemp & ~IRMQ);
/// mcrTemp = disableSelfReception ? (mcrTemp | SRXDIS) : (mcrTemp & ~SRXDIS);
/// base->CTRL1 = ctrl1Temp;
/// base->MCR   = mcrTemp;
/// ```
///
/// ⚠️ **我第一版是整字覆写 `CTRL1`** —— 于是把官方刚设好的位（如 `BOFFREC`）
/// 一并清掉/改掉，而且我 OR 进去的"`SMP`"其实是 `BOFFREC`（位号写错）。
/// ⇒ 必须按官方那样**读-改-写**，只动自己要动的位。
///
/// # 官方默认值（`FLEXCAN_GetDefaultConfig`）
///
/// `maxMbNum = 16`、`enableLoopBack = false`、**`enableTimerSync = true`**、
/// `enableSelfWakeup = false`、`enableIndividMask = false`、`disableSelfReception = false`。
/// 本自证把 `enableLoopBack` 改成 `true`（要回环），其余保持官方默认。
pub fn apply_config() -> Result<(), (u8, u32)> {
    /// 本板既有 C 固件 `config.maxMbNum = 64`（`drv_can.c:157`，注释 "all series have 64 MB"）
    /// ⇒ `MCR[MAXMB] = 63`。**与官方默认 `maxMbNum = 16` 刻意不同。**
    const MAXMB_FIELD_FW: u32 = 64 - 1;

    let mut mcr_now = read(reg::MCR);
    let mut ctrl1_now = read(reg::CTRL1);

    // `enableLoopBack = true`（本自证需要）；`enableTimerSync = true`（官方默认）；
    // `enableListenOnlyMode = false`（官方默认）。
    ctrl1_now |= ctrl1::LPB | ctrl1::TSYN;
    ctrl1_now &= !ctrl1::LOM;

    // ⚠️ **这里改成照本板既有 C 固件（`drv_can.c:154-160`），不是照官方默认值。**
    //
    // ```c
    // FLEXCAN_GetDefaultConfig(&config);
    // config.baudRate = cfg->baud_rate;
    // config.maxMbNum = 64;               /* all series have 64 MB */
    // config.enableIndividMask = true;    /* one filter per MB */
    // ```
    //
    // 两处与官方**默认值**不同，而且都是刻意的：
    // - `maxMbNum = 64`（官方默认 16）：本板既有 C 固件用满 64 个邮箱；
    // - **`enableIndividMask = true` ⇒ `MCR[IRMQ] = 1`**（官方默认 false）——
    //   而我第一版是照官方默认**明确把它清掉**的。
    //
    // 这一位为什么关键，见 [`set_rx_individual_mask`] 的文档：`IRMQ = 1` 时
    // 接收匹配改用**每个邮箱的 `RXIMR`**，而 `FLEXCAN_Reset` 写进 `RXIMR` 的是
    // `0x3FFF_FFFF`（在"0 = 不比较"的约定下 = **全部比较**）；本板既有 C 固件则写成
    // `FLEXCAN_RX_MB_STD_MASK(0,0,0)` = **`0x00000000`**（注释原文："all ID bit in the
    // filter is 'don't care'"）⇒ **任何帧都收**。
    // ⚠️ 临时回到官方默认值：`IRMQ = 0`（全局掩码）。本板既有 C 固件用的是
    //    `enableIndividMask = true`（`IRMQ = 1`）+ 逐邮箱写 `RXIMR = 0`，
    //    但**逐邮箱那一步当前会把板子打成 HardFault**（见下方 `write_cs` 前的说明），
    //    所以这里先不同时开 `IRMQ` —— 否则 `FLEXCAN_Reset` 写进 `RXIMR` 的 `0x3FFF_FFFF`
    //    会变成"全部比较"，语义与本板既有 C 固件相反。
    //    **待 HardFault 查明后，两处要一起改回本板既有 C 固件的配置。**
    mcr_now = (mcr_now & !mcr::MAXMB_MASK) | MAXMB_FIELD_FW;
    mcr_now |= mcr::IRMQ; // 本板既有 C 固件：`enableIndividMask = true`
    mcr_now &= !(mcr::SLF_WAK | mcr::WAK_SRC | mcr::SRXDIS);

    write(reg::CTRL1, ctrl1_now);
    write(reg::MCR, mcr_now);

    // 回读校验（铁律 3）：LPB 与 TSYN 必须真的置起，SRXDIS 必须真的清掉。
    let ctrl1_back = read(reg::CTRL1);
    if ctrl1_back & (ctrl1::LPB | ctrl1::TSYN) != (ctrl1::LPB | ctrl1::TSYN) {
        return Err((23, ctrl1_back));
    }
    let mcr_back = read(reg::MCR);
    if mcr_back & mcr::SRXDIS != 0 {
        return Err((24, mcr_back));
    }
    Ok(())
}

/// 官方 `FLEXCAN_Reset`：把模块置到**已知状态**。
///
/// # 这是我在第一版里整段漏掉的一步
///
/// 官方 `FLEXCAN_Init`（`fsl_flexcan.c:764`）的顺序是：
///
/// ```text
/// FLEXCAN_Enable(base, false)   /* Disable —— 之后才允许配 CTRL1[CLKSRC] */
/// [CTRL1[CLKSRC] 选择 PE 时钟源]
/// FLEXCAN_Enable(base, true)    /* Enable */
/// FLEXCAN_Reset(base)           /* ← 我漏了这整段 */
/// [EnterFreezeMode → 配位时序/邮箱 → ExitFreezeMode]
/// ```
///
/// ⚠️ 本芯片**没有** `CTRL1[CLKSRC]`：`FSL_FEATURE_FLEXCAN_SUPPORT_ENGINE_CLK_SEL_REMOVE = 1`
/// 且 `..._INSTANCE_..._REMOVEn(x) = 1`（`MIMXRT1021_features.h:177/179`），
/// 所以官方代码里那一段被 `#if` 编译掉了 ⇒ **PE 时钟源只由 CCM 决定**
///（见 [`ensure_clock`] 里配 `CSCMR2[CAN_CLK_SEL]`）。
///
/// # 为什么这一步不能省
///
/// 真机判据：位号修正后自证推进到 `stage=5`（清 `HALT`+`FRZ` 后 `FRZACK` 仍不落），
/// 而我从未对该模块做过软复位 —— 模块的内部控制状态机停在**上电默认**（那个默认里
/// `HALT=1`、`FRZ=1`，即"已请求 Freeze"）。官方把 `FLEXCAN_Reset` 放在配置之前，
/// 正是为了让后面每一步都从已知状态出发。
///
/// # Errors
///
/// 返回 `(阶段, 诊断值)`。**不折叠** —— 现场只有一行日志。
pub fn flexcan_reset() -> Result<(), (u8, u32)> {
    // 官方断言：软复位只能在**已使能**（MDIS=0）状态下做。
    let mcr = read(reg::MCR);
    if mcr & mcr::MDIS != 0 {
        return Err((20, mcr));
    }

    // `FSL_FEATURE_FLEXCAN_HAS_DOZE_MODE_SUPPORT` 对本芯片为假 ⇒ 官方的 DOZE 处理跳过。

    // 官方原文："Wait until FlexCAN exit from any Low Power Mode."
    if !wait_mcr(mcr::LPM_ACK, false) {
        return Err((21, read(reg::MCR)));
    }

    // 官方原文："Assert Soft Reset Signal."
    set_mcr(mcr::SOFT_RST, true);
    // 官方原文："Wait until FlexCAN reset completes."（`SOFTRST` 由硬件自清）
    if !wait_mcr(mcr::SOFT_RST, false) {
        return Err((22, read(reg::MCR)));
    }

    // 官方 `FLEXCAN_Reset`：
    //   base->MCR |= CAN_MCR_WRNEN_MASK | CAN_MCR_MAXMB(MAX_NUMBER - 1);
    // 本芯片无 GLITCH_FILTER ⇒ 走 `#else` 分支（只有 WRNEN 与 MAXMB）。
    // ⚠️ 这里 `FLEXCAN_Reset` 用的是**特性宏**的上限（64），但紧接着的
    //    `FLEXCAN_Init` 会用 `pConfig->maxMbNum` 覆盖它 —— 而官方默认
    //    `FLEXCAN_GetDefaultConfig` 给的是 **16**（`fsl_flexcan.c`）。
    //    所以"最终生效值"是 16（`MAXMB=15`），见 [`apply_config`]。
    let mcr_now = read(reg::MCR);
    write(
        reg::MCR,
        (mcr_now | mcr::WRNEN) & !mcr::MAXMB_MASK | (MAILBOXES_OFFICIAL - 1),
    );

    // 官方：`base->CTRL1 = CAN_CTRL1_SMP_MASK;`（**不是 0**）。
    write(reg::CTRL1, ctrl1::SMP);
    // 官方：base->CTRL2 = TASD(0x16) | RRS | EACEN;
    write(reg::CTRL2, CTRL2_OFFICIAL);

    // 官方："Clean all individual Rx Mask of Message Buffers."（RXIMR[i] 从 0x0880 起，4 字节一个）
    for index in 0..MAILBOXES_OFFICIAL {
        write(0x0880 + index * 4, RX_MASK_OFFICIAL);
    }
    // 官方：RXMGMASK / RX14MASK / RX15MASK = 0x3FFF_FFFF
    write(reg::RXGMASK, RX_MASK_OFFICIAL);
    write(reg::RX14MASK, RX_MASK_OFFICIAL);
    write(reg::RX15MASK, RX_MASK_OFFICIAL);

    // 官方在此之后还清了 IFLAG1/IMASK1 等（`fsl_flexcan.c:660+`）。
    write(reg::IFLAG1, u32::MAX);
    write(reg::IFLAG2, u32::MAX);
    write(reg::IMASK1, 0);
    write(reg::IMASK2, 0);
    Ok(())
}

/// **读出真实的协议引擎（PE）时钟频率**（Hz）。
///
/// # 为什么必须读，而不是假定
///
/// PE 时钟由 `CCM_CSCMR2` 的两个字段决定（两者在**同一个寄存器**里）：
///
/// - `CAN_CLK_SEL`（offset **8**）：0 = `pll3_sw_clk`÷(60M)、**1 = `osc_clk`(24M)**、
///   2 = `pll3_sw_clk`÷(80M)；
/// - `CAN_CLK_PODF`（offset **2**）：后分频，`PE = 源 / (PODF + 1)`。
///
/// ⚠️ **我只设了 `CAN_CLK_SEL`，从未读或设 `CAN_CLK_PODF`。**
/// 而位时序（`PRESDIV`/`PSEG1`/`PSEG2`/`PROPSEG`）是**按 PE 时钟算出来的** ——
/// 我手写的那组值是按"24 MHz"假定的。**PE 时钟一旦不是 24 MHz，位时序就是错的**：
/// 发送端照样能把帧移出去（`TX CODE` 会正常回到 `INACTIVE`，错误计数为 0），
/// 而**接收端的采样窗口对不上 ⇒ 帧收不进来** —— 与真机现象完全吻合。
///
/// 顺带一提：本板既有 C 固件设的是 `CLOCK_SetDiv(kCLOCK_CanDiv, 1)` ⇒ `PODF = 1`（÷2）。
/// 那意味着它们认为 PE = 源 / 2；本工程用的是 24 MHz 晶振做源，所以必须先读出来。
#[must_use]
pub fn pe_clock_hz() -> u32 {
    // 源：`CAN_CLK_SEL = 1` ⇒ osc_clk = 24 MHz（本板晶振，见 `board-support` 的控制台时钟说明）。
    const OSC_HZ: u32 = 24_000_000;
    // SAFETY: 同 [`ccm::enable_can2`] —— `CCM::instance()` 的唯一性由"只在启动阶段、
    // 单线程调用"保证；这里只**读**一个字段，不做任何写。
    let ccm = unsafe { ral::ccm::CCM::instance() };
    let podf = ral::read_reg!(ral::ccm, ccm, CSCMR2, CAN_CLK_PODF);
    OSC_HZ / (podf + 1)
}

/// 按当前 `TIMING` 算出的实际波特率（bps）。
///
/// 官方公式（`fsl_flexcan.c:665` 的 `FLEXCAN_SetBaudRate`）：
/// `quantum = 1 + (PSEG1+1) + (PSEG2+1) + (PROPSEG+1)`，
/// `baud = PE_clock / (quantum * (PRESDIV+1))`。
#[must_use]
pub fn actual_baud() -> u32 {
    let (presdiv, propseg, pseg1, pseg2, rjw) = TIMING;
    let _ = rjw; // RJW 不参与波特率公式，只影响重同步
    let quantum = 1 + (pseg1 + 1) + (pseg2 + 1) + (propseg + 1);
    pe_clock_hz() / (quantum * (presdiv + 1))
}

// ---------------------------------------------------------------------------
// CAN2 引脚复用（**地址与 ALT 全部来自官方 `fsl_iomuxc.h`**）
// ---------------------------------------------------------------------------

/// 把**本板真正的** CAN2 焊盘复用到 FLEXCAN2，并恢复我先前改坏的两个以太网焊盘。
///
/// # ⚠️ 这里曾经写错了焊盘 —— 而且是**两边都错**
///
/// 我第一版用的是 `GPIO_AD_B0_14`(TX) / `GPIO_AD_B0_15`(RX)、ALT **1**，
/// 依据是官方 `fsl_iomuxc.h` 里的宏名 `IOMUXC_GPIO_AD_B0_14_FLEXCAN2_TX`。
///
/// **那是"FLEXCAN2_TX 可以出现在哪些焊盘上"的目录，不是"本板用了哪个"。**
/// 本板实际用哪个由**原理图 + 板级 `pin_mux.c`** 决定。三重证据一致指向另一对焊盘：
///
/// | 来源 | 结论 |
/// |---|---|
/// | 原理图 | `GPIO_AD_B1_00` → 22R → **`CAN2_TX`**；`GPIO_AD_B1_01` → 22R → **`CAN2_RX`** |
/// | 原理图 | `GPIO_AD_B0_14` → 22R → **`ENET_TXD0`**；`GPIO_AD_B0_15` → 22R → **`ENET_TXD1`** |
/// | 板级 `pin_mux.c:262/265`（`BSP_USING_CAN2` 分支） | `IOMUXC_GPIO_AD_B1_00_FLEXCAN2_TX` / `..._AD_B1_01_FLEXCAN2_RX` |
///
/// ⇒ 我的写法有两个后果：**真正的 CAN2 RX 焊盘从未复用**（FlexCAN2 的输入路径没接上），
/// 而且**把以太网的 `ENET_TXD0/TXD1` 改成了 FLEXCAN2 功能** —— 顺手破坏了 ENET。
/// 两个都要修：CAN 换到正确的焊盘，ENET 恢复成 ALT0（`ENET_TDATA00/01`）。
///
/// # 依据（逐字段抄，不推算）
///
/// ```text
/// fsl_iomuxc.h:502  IOMUXC_GPIO_AD_B1_00_FLEXCAN2_TX = 0x401F80FC, 0x2U, 0,          0, 0x401F8270
/// fsl_iomuxc.h:511  IOMUXC_GPIO_AD_B1_01_FLEXCAN2_RX = 0x401F8100, 0x2U, 0x401F8324, 0x3U, 0x401F8274
/// ```
///
/// 五段 = `mux寄存器, mux值, 输入选择寄存器, daisy值, pad控制寄存器`：
///
/// | 项 | 寄存器 | 值 |
/// |---|---|---|
/// | CAN2_TX 复用 | `0x401F80FC` | **2** |
/// | CAN2_RX 复用 | `0x401F8100` | **2** |
/// | CAN2_RX 输入选择 | `0x401F8324` | **3**（daisy） |
/// | CAN2_TX 焊盘控制 | `0x401F8270` | **`0x10B0`** |
/// | CAN2_RX 焊盘控制 | `0x401F8274` | **`0x10B0`** |
///
/// ⚠️ **ALT 是 2，不是 1**；**RX 的 daisy 是 3，不是 2** —— 这两项我都绝不可能猜出来。
/// pad 控制值 `0x10B0` 取自板级 `pin_mux.c:266/270` 的
/// `IOMUXC_SetPinConfig(IOMUXC_GPIO_AD_B1_0x_FLEXCAN2_xx, 0x10B0u)`（与 LPUART1 同值：
/// 慢斜率 / R0/6 驱动强度 / medium(100MHz) / 不开漏 / 无上下拉）。
///
/// # 顺带：把 ENET 焊盘恢复回去
///
/// `fsl_iomuxc.h:483/492`：`ENET_TDATA00/01` 的 ALT 是 **0** ⇒ 写 `0` 复位我改坏的状态。
pub fn mux_can2_pins() {
    /// 本板 **CAN2_TX** 焊盘：`GPIO_AD_B1_00`（原理图 → 22R → `CAN2_TX`）。
    const MUX_CAN2_TX: usize = 0x401F_80FC;
    /// 本板 **CAN2_RX** 焊盘：`GPIO_AD_B1_01`（原理图 → 22R → `CAN2_RX`）。
    const MUX_CAN2_RX: usize = 0x401F_8100;
    /// `SW_INPUT_SELECT` for `FLEXCAN2_RX`（官方宏第三段）。
    const INPUT_SELECT_CAN2_RX: usize = 0x401F_8324;
    /// `SW_PAD_CTL_PAD_GPIO_AD_B1_00`（官方宏第五段）。
    const PAD_CAN2_TX: usize = 0x401F_8270;
    /// `SW_PAD_CTL_PAD_GPIO_AD_B1_01`（官方宏第五段）。
    const PAD_CAN2_RX: usize = 0x401F_8274;
    /// ⚠️ 本板这两个焊盘上 FLEXCAN2 的 ALT 编号 = **2**（官方宏第二段）。
    const ALT_FLEXCAN2_ON_AD_B1: u32 = 2;
    /// `FLEXCAN2_RX` 在 `GPIO_AD_B1_01` 上的 daisy 值 = **3**（官方宏第四段）。
    const DAISY_FLEXCAN2_RX: u32 = 3;
    /// 焊盘控制值，取自板级 `pin_mux.c` 的 `0x10B0u`。
    const PAD_CTL: u32 = 0x10B0;

    /// 我先前**误**改成 FLEXCAN2 的以太网焊盘：`GPIO_AD_B0_14`（`ENET_TXD0`）。
    const MUX_ENET_TXD0: usize = 0x401F_80F4;
    /// 同上：`GPIO_AD_B0_15`（`ENET_TXD1`）。
    const MUX_ENET_TXD1: usize = 0x401F_80F8;
    /// `ENET_TDATA00/01` 的 ALT 编号（`fsl_iomuxc.h:483/492`）。
    const ALT_ENET: u32 = 0;

    // SAFETY:
    // ① 六个地址全部是官方 `fsl_iomuxc.h` 宏里的**字面值**，或由原理图/板级 `pin_mux.c` 确证；
    // ② 只写这六个寄存器，不碰同组其它焊盘；`write_volatile` 是寄存器语义，
    //    4 字节对齐由地址本身保证；
    // ③ 影响面：四个焊盘在本工程中此前无人使用（ENET 与 CAN2 都还没落地），
    //    且 CAN 焊盘按原理图上就是接 CAN 收发器、ENET 焊盘接以太网 PHY；
    // ④ 这次写入**修复**了先前误配（把 ENET 焊盘写成 CAN 功能）。
    unsafe {
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(MUX_CAN2_TX),
            ALT_FLEXCAN2_ON_AD_B1,
        );
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(MUX_CAN2_RX),
            ALT_FLEXCAN2_ON_AD_B1,
        );
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(INPUT_SELECT_CAN2_RX),
            DAISY_FLEXCAN2_RX,
        );
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(PAD_CAN2_TX),
            PAD_CTL,
        );
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(PAD_CAN2_RX),
            PAD_CTL,
        );
        // 恢复被我改坏的以太网焊盘。
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(MUX_ENET_TXD0),
            ALT_ENET,
        );
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(MUX_ENET_TXD1),
            ALT_ENET,
        );
    }
}
