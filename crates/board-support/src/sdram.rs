//! SDRAM：Winbond **W9825G6KH-6I**（32 MB）挂在 SEMC CS0，映射到 `0x8000_0000`。
//!
//! # 为什么这一段必须自己写
//!
//! `imxrt-hal` 0.6 **没有** SEMC 模块，`imxrt-iomuxc` 也**没有** SEMC 的焊盘复用
//! 数据（只有 `lpuart`/`lpspi`/`lpi2c`/`flexcan`/`sai`/`usdhc`/`flexio`）。
//! 因此焊盘复用、时钟门控、SEMC 寄存器与 IP 命令序列全部由本模块直写 RAL。
//!
//! # 为什么先算再写
//!
//! 时序数值（多少个时钟周期）全部由 [`sdram_timing::sdram`] 在主机上算好并被
//! 单元测试钉住；本模块只做"读时钟 → 换算 → 写寄存器 → 发 IP 命令"。
//! 这样，一旦内存测试失败，"是数值错了"与"是寄存器写错了"可以分开排查。
//!
//! # 移植来源（不是照文档猜的）
//!
//! 本板既有 C/RT-Thread 固件带着 NXP MCUXpresso SDK（`fsl_semc.c` +
//! `drv_sdram.c` + `sdram_port.h`），那是**这块板验证过**的 SDRAM 初始化。
//! 本模块逐条对应它的 `SEMC_Init` + `SEMC_ConfigureSDRAM`：
//! 先把所有 `BR[]` 清零，`MCR` 软复位并等自清，再配置 `BMCR`/`BR`/`SDRAMCR*`，
//! 然后依次发送 `Prechargeall` → `AutoRefresh` ×2 → `Modeset`，最后置 `SDRAMCR3[REN]`。
//!
//! # ⚠️ 绝不能改 PLL
//!
//! SEMC 的时钟源取"替代时钟"= `PLL2_PFD2`（即系统 PLL 的 PFD2）。**固件正在从
//! FlexSPI 取指**，而 FlexSPI 的时钟同样来自 PLL2 —— 动 PLL 会立刻让取指失败、
//! 芯片当场跑飞。因此这里只做两件事：**读出** PLL 现值 → 用 `SEMC_PODF` 分频，
//! 并把换算出的实际频率报给上层（横幅会打印它，便于自证）。

use imxrt_ral as ral;
use sdram_timing::sdram::{SdramSpec, SemcTiming, TimingError, pfd_hz, pll_sys_hz, semc_podf_for};

use crate::config;

/// SEMC IP 命令码（与 SDK `semc_ipcmd_sdram_t` 一致）。
const CMD_MODESET: u32 = 0xA;
const CMD_AUTOREFRESH: u32 = 0xC;
const CMD_PRECHARGEALL: u32 = 0xF;
/// `IPCMD[KEY]` 的魔术值：不写它命令不会执行。
const IPCMD_KEY: u32 = 0xA55A << 16;

/// `INTR` 位。
const INTR_IPCMDDONE: u32 = 1 << 0;
const INTR_IPCMDERR: u32 = 1 << 1;

/// 初始化失败的原因。每一种都对应一个可区分的处置动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdramError {
    /// PLL2_PFD2 的分数分频字段非法（说明时钟树不是预期状态）。
    PfdInvalid,
    /// 换算时序失败（参数超出 SEMC 可编码范围）。
    Timing(TimingError),
    /// IP 命令超时：SEMC 没在预期时间内完成命令。
    IpCommandTimeout,
    /// IP 命令报错（`INTR[IPCMDERR]`）。
    IpCommandError,
    /// 初始化后的冒烟读写不一致：寄存器写完了但内存不通。
    SmokeTestFailed,
    /// CCM 里的 SEMC 分频写入被拒绝（读回不一致）。
    ///
    /// 这条判据是 2026-10-10 的真机教训换来的：当时 PLL 倍频算错，
    /// 分频算成 0，SEMC 跑在 500 MHz（远超规格），于是**整块寄存器读回全 0、
    /// IP 命令必然超时**。若当时写后就读回校验，一次就能定位到 CCM。
    ClockWriteRejected,
    /// SEMC 寄存器写不进去（块没有时钟或未使能）。
    SemcNotResponding,
    /// 算出的 SEMC 时钟超出合理区间。
    ClockOutOfRange,
}

/// `board::init` 之后可用的 SDRAM 句柄。
///
/// 句柄本身不持有资源（SDRAM 就是一段地址区间），只记录基址与容量，
/// 访问通过 volatile 读写完成 —— 编译器不能把它优化掉或缓存。
#[derive(Debug, Clone, Copy)]
pub struct Sdram {
    base: *mut u32,
    len_bytes: u32,
}

// SAFETY: SDRAM 是普通外部内存，任意线程/中断读写都不会破坏 Rust 的内存模型；
// 本类型只提供按字访问，不做别名假设（内部用 volatile）。
unsafe impl Send for Sdram {}
// SAFETY: 同上。
unsafe impl Sync for Sdram {}

impl Sdram {
    /// 容量（字节）。
    #[must_use]
    pub const fn len_bytes(&self) -> u32 {
        self.len_bytes
    }

    /// 基址。
    #[must_use]
    pub fn base_address(&self) -> usize {
        self.base.expose_provenance()
    }

    /// 读一个字（偏移以字节计）。
    ///
    /// # Panics
    ///
    /// 偏移超出容量或未按 4 字节对齐时 panic —— 这是**编程错误**，
    /// 而不是运行时故障，越界读会在总线上产生难以定位的挂起。
    #[must_use]
    pub fn read32(&self, offset: u32) -> u32 {
        assert!(
            offset.is_multiple_of(4) && offset < self.len_bytes,
            "SDRAM 读越界或未对齐"
        );
        let words = offset / 4;
        // SAFETY: 上面已断言 offset 在容量内且对齐；base 是映射到 0x8000_0000 的
        // SDRAM 基址（由 SEMC 初始化保证）。volatile 防止优化与缓存。
        unsafe {
            core::ptr::read_volatile(self.base.wrapping_add(usize::try_from(words).unwrap_or(0)))
        }
    }

    /// 写一个字（偏移以字节计）。
    ///
    /// # Panics
    ///
    /// 同 [`Sdram::read32`]。
    pub fn write32(&mut self, offset: u32, value: u32) {
        assert!(
            offset.is_multiple_of(4) && offset < self.len_bytes,
            "SDRAM 写越界或未对齐"
        );
        let words = offset / 4;
        // SAFETY: 同 read32；写外部内存不会破坏 Rust 的内存模型。
        unsafe {
            core::ptr::write_volatile(
                self.base.wrapping_add(usize::try_from(words).unwrap_or(0)),
                value,
            )
        };
    }
}

/// 需要复用为 SEMC 功能的 EMC 焊盘编号。
///
/// 清单来源有两处且互相印证：
/// 1. **RAL 的 SVD 复用表**：每个焊盘 ALT0 的功能名（如 `GPIO_EMC_09 → SEMC_WE`）；
/// 2. **原理图网络清单**：`SEMC_A0..A12`、`BA0/1`、`CAS/CKE/CLK/CS0`、`D0..D15`、
///    `DM0/DM1`、`RAS/WE`。
///
/// 因此这里**跳过** `EMC_28`(DQS)、`EMC_40`(CSX0)、`EMC_41`(READY)：SDR SDRAM
/// 不需要 DQS，片选走的是 `CS0`，而 `READY` 只有异步器件才用。
const SEMC_PADS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7,  // D0..D7
    8,  // DM0
    9,  // WE
    10, // CAS
    11, // RAS
    12, // CS0
    13, // BA0
    14, // BA1
    15, // A10（precharge-all 需要它）
    16, 17, 18, 19, 20, 21, 22, 23, 24, // A0..A8
    25, // A9
    26, // A11
    27, // A12
    29, // CKE
    30, // CLK
    31, // DM1
    32, 33, 34, 35, 36, 37, 38, 39, // D8..D15
];

/// `SW_MUX_CTL_PAD_GPIO_EMC_00` 的地址（此后每个焊盘 +4）。
///
/// 数值取自 `imxrt-iomuxc` 的焊盘常量（`GPIO_EMC_00_MUX_ADDR`），
/// 并用 `GPIO_EMC_39`（`0x401F_80B0`）验证了步距确实是 4。
const EMC_MUX_BASE: usize = 0x401F_8014;
/// `SW_PAD_CTL_PAD_GPIO_EMC_00` 的地址（此后每个焊盘 +4）。
const EMC_PAD_BASE: usize = 0x401F_8188;

/// 焊盘电气配置字：`SRE=1`（快沿）、`DSE=0b111`（R0/7）、`SPEED=0b11`（200 MHz），
/// 上下拉与 keeper 全关。
///
/// 直接沿用本板 **Boot ROM DCD**（`evkmimxrt1020_sdram_ini_dcd.c`）里给 EMC 焊盘
/// 写的同一个值 `0xE1`：SDRAM 总线由控制器持续驱动，内部上下拉只会增加功耗与反射。
const EMC_PAD_CTRL: u32 = 0x0000_00E1;

/// 把 [`SEMC_PADS`] 里的焊盘复用为 ALT0 并写入电气配置。
///
/// 用"一张表 + 一次 `unsafe`"而不是每个焊盘一个类型化调用，是**刻意**的：
/// 39 次类型化调用会带来 39 个 `unsafe` 块，把 unsafe 预算从 19 顶到 58 ——
/// 那种"预算"就不再有任何约束力了。集中成一处之后，只需要论证一次。
fn configure_pads() {
    // SAFETY: 循环体只写 IOMUXC 的 SW_MUX_CTL_PAD_GPIO_EMC_nn 与
    // SW_PAD_CTL_PAD_GPIO_EMC_nn 两个寄存器；这些焊盘在本板上专用于 SEMC，
    // 不与其它外设共享；本函数由 `init` 调用一次，不并发。
    unsafe {
        for &index in SEMC_PADS {
            let offset = usize::from(index) * 4;
            core::ptr::write_volatile(
                core::ptr::with_exposed_provenance_mut::<u32>(EMC_MUX_BASE + offset),
                0,
            );
            core::ptr::write_volatile(
                core::ptr::with_exposed_provenance_mut::<u32>(EMC_PAD_BASE + offset),
                EMC_PAD_CTRL,
            );
        }
    }
}

/// 读出 SEMC 时钟源（PLL2_PFD2）并选一个不超过上限的后分频。
///
/// 返回 `(SEMC 时钟 Hz, SEMC_PODF 字段值)`。
fn read_semc_clock() -> Result<(u32, u32), SdramError> {
    // SAFETY: 只读 CCM_ANALOG 里的 PLL/PFD 状态；这些寄存器由 Boot ROM 配置，
    // 本函数不写它们（改 PLL 会让 FlexSPI 取指失败，见模块文档）。
    let ana = unsafe { ral::ccm_analog::CCM_ANALOG::instance() };
    let div_select = ral::read_reg!(ral::ccm_analog, ana, PLL_SYS, DIV_SELECT);
    let frac = ral::read_reg!(ral::ccm_analog, ana, PFD_528, PFD2_FRAC);
    let pll_hz = pll_sys_hz(config::XTAL_HZ, div_select);
    let pfd_hz = pfd_hz(pll_hz, u8::try_from(frac).unwrap_or(0)).ok_or(SdramError::PfdInvalid)?;
    let (div, field) =
        semc_podf_for(pfd_hz, config::SEMC_CLOCK_MAX_HZ).ok_or(SdramError::PfdInvalid)?;
    Ok((pfd_hz / div, field))
}

// ---------------------------------------------------------------------------
// SEMC 寄存器访问：**刻意不用 `imxrt-ral` 的字段名**
// ---------------------------------------------------------------------------
//
// 2026-10-10 真机实测发现：`imxrt-ral` 0.6.2 的 SEMC 寄存器映射与硬件**不符** ——
// 它在 `BR8` 之后少算了一个保留字，于是整块寄存器名字整体前移 4 字节：
//
// | 寄存器 | 硬件真实偏移 | RAL 认为的偏移 |
// |---|---|---|
// | `INTEN` | `0x38` | `0x34` |
// | `INTR` | `0x3C` | `0x38` |
// | `SDRAMCR0` | `0x40` | `0x3C` |
// | `SDRAMCR1` | `0x44` | `0x40` |
// | `IPCR0` | `0x90` | `0x60` |
// | `IPCMD` | `0x9C` | `0x6C` |
//
// 两条独立证据：
// 1. NXP MCUXpresso 头文件 `MIMXRT1021.h` 里 `SEMC_Type` 每个字段都带 `offset:` 注释；
// 2. 真机标记写入定位：往 RAL 名 `SDRAMCR1`(0x40) 写 `0x22222222`，回读发现它落在
//    **硬件的 `SDRAMCR0`**；而 RAL 名 `SDRAMCR0`(0x3C) 写不进去（读回 0）。
//
// 后果非常隐蔽：`ip_command` 里读 RAL 名 `INTR`(0x38) 实际读到的是硬件 `INTEN`，
// 它的 bit0 恰为 1，于是**每条 IP 命令都"立刻完成"却根本没执行**，
// 而 `SDRAMCR*` 全写进了相邻寄存器 —— 表现出来就是"初始化成功但内存读全 0"。
//
// 因此这里改用显式偏移。数值取自上述头文件注释。

/// SEMC 基址。
const SEMC_BASE: usize = 0x402F_0000;
const SEMC_MCR: usize = 0x00;
const SEMC_BMCR0: usize = 0x08;
const SEMC_BMCR1: usize = 0x0C;
const SEMC_BR0: usize = 0x10;
const SEMC_BR8: usize = 0x30;
const SEMC_INTR: usize = 0x3C;
const SEMC_SDRAMCR0: usize = 0x40;
const SEMC_SDRAMCR1: usize = 0x44;
const SEMC_SDRAMCR2: usize = 0x48;
const SEMC_SDRAMCR3: usize = 0x4C;
const SEMC_IPCR0: usize = 0x90;
const SEMC_IPCR1: usize = 0x94;
const SEMC_IPCR2: usize = 0x98;
const SEMC_IPCMD: usize = 0x9C;
const SEMC_IPTXDAT: usize = 0xA0;

/// 读 SEMC 寄存器。
fn semc_read(offset: usize) -> u32 {
    // SAFETY: 偏移取自 NXP 头文件的 SEMC 寄存器表；SEMC 由本模块独占，
    // 且这些访问发生在初始化期（无并发）。
    unsafe {
        core::ptr::read_volatile(core::ptr::with_exposed_provenance::<u32>(
            SEMC_BASE + offset,
        ))
    }
}

/// 写 SEMC 寄存器。
fn semc_write(offset: usize, value: u32) {
    // SAFETY: 同 `semc_read`。
    unsafe {
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(SEMC_BASE + offset),
            value,
        )
    };
}

/// 读改写：只在 `mask` 覆盖的位上生效。
fn semc_modify(offset: usize, mask: u32, value: u32) {
    let current = semc_read(offset);
    semc_write(offset, (current & !mask) | (value & mask));
}

/// 发一条 SEMC IP 命令并等它完成。
fn ip_command(command: u32, address: u32, data: Option<u32>) -> Result<(), SdramError> {
    // 清完成标志（写 1 清零）——必须在写 IPCMD **之前**。
    semc_write(SEMC_INTR, INTR_IPCMDDONE);
    semc_write(SEMC_IPCR0, address);
    if let Some(value) = data {
        semc_write(SEMC_IPTXDAT, value);
    }
    // KEY 必须写，否则命令不执行；低位是命令码。
    semc_write(SEMC_IPCMD, IPCMD_KEY | command);

    // 有界等待：现场宁可明确失败，也不要在这里死等（会掩盖真正的原因）。
    for _ in 0..config::SEMC_IPCOMMAND_TIMEOUT_LOOPS {
        let intr = semc_read(SEMC_INTR);
        if intr & INTR_IPCMDDONE != 0 {
            return Ok(());
        }
        if intr & INTR_IPCMDERR != 0 {
            return Err(SdramError::IpCommandError);
        }
    }
    Err(SdramError::IpCommandTimeout)
}

/// 最近一次成功初始化时算出的 SEMC 时钟（MHz）。
///
/// 用静态量而不是让上层自己算：SEMC 时钟是"读 PLL 再分频"得到的，
/// 上层打印它只需要一个数，不必重复一遍时钟树逻辑。
static SEMC_CLOCK_MHZ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 读回最近一次初始化算出的 SEMC 时钟（MHz）；未初始化为 0。
#[must_use]
pub fn semc_clock_mhz() -> u32 {
    SEMC_CLOCK_MHZ.load(core::sync::atomic::Ordering::Relaxed)
}

/// 初始化 SDRAM。**必须在 `board::init` 里、且在用到任何 SDRAM 之前调用一次。**
///
/// # Safety
///
/// 会改写 IOMUXC/CCM/SEMC 寄存器与 `0x8000_0000` 起始的内存内容；全局只允许调用一次。
pub unsafe fn init() -> Result<(Sdram, SemcTiming), SdramError> {
    configure_pads();

    let (semc_hz, podf_field) = read_semc_clock()?;
    if !(config::SEMC_CLOCK_MIN_HZ..=config::SEMC_CLOCK_MAX_HZ * 4).contains(&semc_hz) {
        // 下限防止"PLL 没开"这类静默错误，上限留 4 倍余量（分频只能取 1..8）。
        return Err(SdramError::ClockOutOfRange);
    }
    let timing = SemcTiming::compute(&SdramSpec::W9825G6KH_6I, semc_hz, config::SDRAM_BASE)
        .map_err(SdramError::Timing)?;
    SEMC_CLOCK_MHZ.store(semc_hz / 1_000_000, core::sync::atomic::Ordering::Relaxed);

    // SAFETY: 只写 CCM 里与 SEMC 有关的三个字段（读-改-写，不动其它字段），
    // 并打开 SEMC 的时钟门控。
    let ccm = unsafe { ral::ccm::CCM::instance() };
    ral::modify_reg!(
        ral::ccm,
        ccm,
        CBCDR,
        SEMC_CLK_SEL: 1,      // 1 = 使用"替代时钟"
        SEMC_ALT_CLK_SEL: 0,  // 0 = PLL2 PFD2
        SEMC_PODF: podf_field
    );
    ral::modify_reg!(ral::ccm, ccm, CCGR3, CG2: 0b11);

    // 写完立刻读回：CCM 的写入被丢弃过一次（见 `ClockWriteRejected` 的说明），
    // 而丢弃的后果是"SEMC 寄存器全 0"这种极难反推的现象。
    let podf_now = ral::read_reg!(ral::ccm, ccm, CBCDR, SEMC_PODF);
    if podf_now != podf_field {
        return Err(SdramError::ClockWriteRejected);
    }

    // --- 0) 先确认 SEMC 块真的在响应 ---
    // 写入标记再读回：块没有时钟时读回是 0（真机实测过）。
    semc_write(SEMC_BR8, 0x5A5A_0000);
    if semc_read(SEMC_BR8) != 0x5A5A_0000 {
        return Err(SdramError::SemcNotResponding);
    }
    semc_write(SEMC_BR8, 0);

    // --- 1) 软复位并等自清 ---
    semc_write(SEMC_MCR, 0x1);
    for _ in 0..config::SEMC_IPCOMMAND_TIMEOUT_LOOPS {
        if semc_read(SEMC_MCR) & 0x1 == 0 {
            break;
        }
    }

    // --- 2) 模块配置：先禁用，配超时与 DQS 模式，最后使能 ---
    //
    // ⚠️ `DQSMD = 0`（内部回环）是**与既有 C 固件的刻意偏离**：
    // C 固件用 `kSEMC_Loopbackdqspad`(1)，但**本板原理图里没有 SEMC_DQS 网络**
    // （13 页网络检索无 `SEMC_DQS`，且未使用的焊盘正是 `EMC_28`）。
    // 真机实测：`DQSMD=1` 时 SDRAM 读回**恒为 0**（写进去读不回）；
    // 改成 0 后同一地址立刻回读到写入值。SDR SDRAM 本来也不需要 DQS。
    /// `MCR[MDIS]`：配置期间禁用模块。
    const MCR_MDIS: u32 = 0x1 << 1;
    /// `MCR[BTO]` 与 `MCR[CTO]` 的位偏移。
    const MCR_BTO_SHIFT: u32 = 24;
    const MCR_CTO_SHIFT: u32 = 16;
    semc_write(
        SEMC_MCR,
        MCR_MDIS
            | (config::SEMC_BUS_TIMEOUT_CYCLES << MCR_BTO_SHIFT)
            | (config::SEMC_CMD_TIMEOUT_CYCLES << MCR_CTO_SHIFT),
    );
    semc_write(SEMC_BMCR0, 0x0010_4085);
    semc_write(SEMC_BMCR1, 0x4060_2485);
    // 使能模块：清 MDIS，保留其余位。
    semc_modify(SEMC_MCR, MCR_MDIS, 0);

    // --- 3) SDRAM 控制器寄存器（偏移见上方说明）---
    // IOCR：A8 的复用选择清 0（片选走 CS0，无需配 CSX0 字段）。
    semc_modify(0x04, 0x7, 0);
    semc_write(SEMC_BR0, timing.br0);
    semc_write(SEMC_SDRAMCR0, timing.sdramcr0);
    semc_write(SEMC_SDRAMCR1, timing.sdramcr1);
    semc_write(SEMC_SDRAMCR2, timing.sdramcr2);
    semc_write(SEMC_SDRAMCR3, timing.sdramcr3);
    semc_write(SEMC_IPCR1, timing.ipcr1);
    semc_write(SEMC_IPCR2, 0);

    // --- 4) IP 命令序列：Precharge all → AutoRefresh ×2 → Modeset → 开刷新 ---
    //
    // 这一段**不能省**：SDRAM 上电后必须走完"预充电 → 刷新 → 写模式寄存器"才进入
    // 确定的工况，而且 `SDRAMCR3[REN]` 不开就**没有自动刷新** —— 64 ms 之后数据开始
    // 丢失，短测试根本测不出来（真机上第一版就是因为漏了这段而"看起来通过"）。
    //
    // 模式寄存器值：`BL 字段 | (CAS 延迟 << 4)`，由 `sdram-timing` 算出并被单测钉住。
    ip_command(CMD_PRECHARGEALL, config::SDRAM_BASE, None)?;
    // 上电后至少要 2 次 AUTO REFRESH（JEDEC 要求）。
    ip_command(CMD_AUTOREFRESH, config::SDRAM_BASE, None)?;
    ip_command(CMD_AUTOREFRESH, config::SDRAM_BASE, None)?;
    ip_command(CMD_MODESET, config::SDRAM_BASE, Some(timing.mode_register))?;
    // 打开自动刷新（REN 在 SDRAMCR3 的 bit0）。
    semc_modify(SEMC_SDRAMCR3, 1, 1);

    // --- 5) 冒烟测试：能写能读才算初始化成功 ---
    let mut sdram = Sdram {
        base: core::ptr::with_exposed_provenance_mut::<u32>(
            usize::try_from(config::SDRAM_BASE).unwrap_or(0),
        ),
        len_bytes: SdramSpec::W9825G6KH_6I.size_bytes,
    };
    for (offset, pattern) in [
        (0u32, 0xA5A5_5A5Au32),
        (4, 0x5A5A_A5A5),
        (0x1000, 0x1234_5678),
        (0x0010_0000, 0xDEAD_BEEF),
    ] {
        sdram.write32(offset, pattern);
        if sdram.read32(offset) != pattern {
            return Err(SdramError::SmokeTestFailed);
        }
    }

    Ok((sdram, timing))
}

impl sdram_timing::memtest::MemoryDevice for Sdram {
    fn len_bytes(&self) -> u32 {
        self.len_bytes
    }

    fn read32(&mut self, offset: u32) -> u32 {
        Sdram::read32(self, offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        Sdram::write32(self, offset, value);
    }
}
