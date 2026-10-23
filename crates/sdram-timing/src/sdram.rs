//! SDRAM 器件规格与 **SEMC 时序换算**：把数据手册的纳秒参数换成寄存器值。
//!
//! # 这里是整个 SDRAM 支持里最容易出错的部分
//!
//! 真正的风险不是"往寄存器写值"这个动作，而是**写进去的数值对不对**：
//! tRP/tRCD/tRAS/tRFC 各换算成几个时钟周期、模式寄存器该是什么、
//! 刷新定时器装多少、容量字段怎么编码。这些都与硬件无关，
//! 因此全部放在这里由主机测试钉死；`board-support` 只负责把算好的
//! 32 位值写进对应寄存器。
//!
//! # 编码规则的来源（不是猜的）
//!
//! 本板（PJ0007_TF2）既有 C/RT-Thread 固件里带着 NXP MCUXpresso SDK 的
//! `fsl_semc.c` / `MIMXRT1021.h`，其中 `SEMC_ConfigureSDRAM` 是**这块板验证过**的
//! 实现。本模块的编码规则逐条对照它：
//!
//! | 字段 | 规则 | 依据 |
//! |---|---|---|
//! | `SDRAMCR0[PS]` | 8 位→0、16 位→1 | `kSEMC_PortSize8Bit=0` / `16Bit=1` |
//! | `SDRAMCR0[BL]` | `log2(burst_len)` | `BurstLen1=0 / 2=1 / 4=2 / 8=3` |
//! | `SDRAMCR0[COL]` | `12 - 列地址位数` | `12bit=0 / 11=1 / 10=2 / 9=3` |
//! | `SDRAMCR0[CL]` | **等于** CAS 延迟（2→2、3→3；1→0） | 头文件注释 `0b10..2 / 0b11..3` |
//! | `SDRAMCR1/2/3` 定时字段 | **周期数 − 1** | `SEMC_ConvertTiming` 末尾 `clockCycles - 1` |
//! | `BR0[MS]` | `log2(容量字节) - 12` | `SEMC_CovertMemorySize`：`size_kbytes/8` 取 log2 |
//! | 模式寄存器 | `BL字段 \| (CAS延迟 << 4)` | `Modeset` 的写数据（`MODESETCAL_OFFSET=4`） |
//!
//! 一处**刻意的偏离**：`SEMC_ConvertTiming` 先把时钟按 MHz 取整
//! （`clkSrc_Hz /= 1000000`，62.5 MHz 会变成 62），再用皮秒反复相加。
//! 本模块改用 `ceil(ns × clk / 1e9)` 的整数运算：等价且没有 MHz 取整误差。

/// SDRAM 器件规格。全部是数据手册参数与结构参数，不含任何寄存器编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdramSpec {
    /// 总容量（字节）。
    pub size_bytes: u32,
    /// 数据位宽（16 表示 16 位）。
    pub data_width_bits: u8,
    /// 列地址位数（W9825G6KH 为 9）。
    pub column_bits: u8,
    /// 行地址位数（13 = A0..A12）。
    pub row_bits: u8,
    /// bank 位数（2 = BA0..BA1）。
    pub bank_bits: u8,
    /// CAS 延迟（周期数）。
    pub cas_latency: u8,
    /// 突发长度（1/2/4/8）。
    pub burst_len: u8,
    /// PRECHARGE 到 ACT/Refresh 等待（tRP）。
    pub trp_ns: u32,
    /// ACT 到读/写等待（tRCD）。
    pub trcd_ns: u32,
    /// 刷新恢复时间（tRFC 一类）。
    pub trfc_ns: u32,
    /// 写恢复时间（tWR）。
    pub twr_ns: u32,
    /// CKE 关闭最短时间。
    pub tcke_ns: u32,
    /// ACT 到 PRECHARGE 最短时间（tRAS）。
    pub tras_ns: u32,
    /// 自刷新恢复（tXSR）。
    pub tsrr_ns: u32,
    /// 刷新到刷新等待（tRC）。
    pub trc_ns: u32,
    /// ACT 到 ACT 等待。
    pub tact2act_ns: u32,
    /// 每行的刷新间隔（64 ms / 8192 行 = 7812 ns）。
    pub refresh_row_ns: u32,
    /// 刷新"紧急"阈值（本板取与每行间隔相同）。
    pub refresh_urgent_ns: u32,
    /// 预分频周期，以**时钟周期数**表示（SDK 用 160）。
    pub prescale_cycles: u16,
    /// 刷新突发长度（1 表示每次刷新 1 个 bank 行）。
    pub refresh_burst_len: u8,
}

impl SdramSpec {
    /// 本板实装器件：Winbond **W9825G6KH-6I**（TSSOP-54，`U20`）。
    ///
    /// 参数取自三处并互相印证：
    /// - 原理图 p13 实装型号与既有固件 `sdram_port.h` 的板级参数；
    /// - W9825G6KH-6 数据手册（4 bank × 4M × 16 bit = 256 Mb = **32 MB**，
    ///   13 行 / 9 列 / 2 bank 位，CL3 @166 MHz，tRP/tRCD 18 ns、tRAS 42 ns、tRC 60 ns）；
    /// - 既有固件 `drv_sdram.c`（`SDRAM_SIZE = 0x8000` KB = 32 MB）。
    ///
    /// 顺带解决一处资料矛盾：早期文档在"32 MB / 64 MB"之间摇摆，
    /// 现以**已上机运行的既有固件参数**为准 —— 32 MB。
    pub const W9825G6KH_6I: Self = Self {
        size_bytes: 32 * 1024 * 1024,
        data_width_bits: 16,
        column_bits: 9,
        row_bits: 13,
        bank_bits: 2,
        cas_latency: 3,
        burst_len: 1,
        trp_ns: 18,
        trcd_ns: 18,
        trfc_ns: 127, // 既有固件写的是 (60 + 67)
        twr_ns: 12,
        tcke_ns: 42,
        tras_ns: 42,
        tsrr_ns: 67,
        trc_ns: 60,
        tact2act_ns: 60,
        refresh_row_ns: 64 * 1_000_000 / 8192, // 7812 ns
        refresh_urgent_ns: 64 * 1_000_000 / 8192,
        prescale_cycles: 160,
        refresh_burst_len: 1,
    };

    /// 容量（KB），用于 `BR0[MS]` 编码。
    #[must_use]
    pub const fn size_kbytes(&self) -> u32 {
        self.size_bytes / 1024
    }

    /// 出线位数（行 + 列 + bank + 位宽换算）推出的容量，用于**校验规格自洽**。
    #[must_use]
    pub fn derived_size_bytes(&self) -> u64 {
        let bytes_per_word = u64::from(self.data_width_bits) / 8;
        (1u64 << self.row_bits)
            * (1u64 << self.column_bits)
            * (1u64 << self.bank_bits)
            * bytes_per_word
    }
}

/// 换算失败的原因。全部是"参数本身不可能成立"，而不是运行时状态问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingError {
    /// 时钟为 0，无法换算。
    ClockZero,
    /// 时钟太慢：换算出的周期数超出字段宽度。
    ClockTooSlow,
    /// 容量小于 4 KB（SEMC 可编码的最小值）。
    SizeTooSmall,
    /// 容量超过 4 GB（SEMC 可编码的最大值）。
    SizeTooLarge,
    /// 数据位宽不是 8/16。
    UnsupportedWidth,
    /// 列地址位数不在 8..=12。
    UnsupportedColumnBits,
    /// CAS 延迟不在 1..=4。
    UnsupportedLatency,
    /// 突发长度不是 1/2/4/8。
    UnsupportedBurst,
}

/// SEMC 里 SDRAM 相关寄存器的**完整 32 位值**。
///
/// 直接给出寄存器值（而不是逐字段返回）有两个理由：
/// 1. BSP 侧只剩"把值写进寄存器"，减少出错面；
/// 2. 这些值能在主机上被单元测试逐个钉住（含跨字段的位重叠）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemcTiming {
    /// `BR0`：有效位 + 容量编码 + 基址。
    pub br0: u32,
    /// `SDRAMCR0`：端口宽度 / 突发长度 / 列地址位数 / CAS 延迟。
    pub sdramcr0: u32,
    /// `SDRAMCR1`：tRP / tRCD / tRFC / tWR / tCKE / tRAS。
    pub sdramcr1: u32,
    /// `SDRAMCR2`：tXSR / tRC / ACT2ACT / 空闲超时。
    pub sdramcr2: u32,
    /// `SDRAMCR3`：刷新突发长度 / 预分频 / 刷新定时 / 紧急阈值（**不含** `REN`）。
    pub sdramcr3: u32,
    /// 模式寄存器的写入值（`Modeset` 命令的数据）。
    pub mode_register: u32,
    /// `IPCR1` 的取值（IP 命令数据宽度）。
    pub ipcr1: u32,
    /// 实际使用的 SEMC 时钟（Hz），供上层打印自证。
    pub semc_clock_hz: u32,
}

// ---- 字段布局（与 imxrt-ral 的 SVD 一致，逐条注明偏移）----------------------
const BR_VLD: u32 = 1 << 0;
const BR_MS_SHIFT: u32 = 1;
const BR_BA_SHIFT: u32 = 12;
/// `BR0[BA]` 只保留基址高 20 位（低 12 位按 4 KB 对齐丢弃）。
const BR_BA_MASK: u32 = 0x000F_FFFF;

const SDRAMCR0_PS_SHIFT: u32 = 0;
const SDRAMCR0_BL_SHIFT: u32 = 4;
const SDRAMCR0_COL_SHIFT: u32 = 8;
const SDRAMCR0_CL_SHIFT: u32 = 10;

const SDRAMCR1_PRE2ACT_SHIFT: u32 = 0;
const SDRAMCR1_ACT2RW_SHIFT: u32 = 4;
const SDRAMCR1_RFRC_SHIFT: u32 = 8;
const SDRAMCR1_WRC_SHIFT: u32 = 13;
const SDRAMCR1_CKEOFF_SHIFT: u32 = 16;
const SDRAMCR1_ACT2PRE_SHIFT: u32 = 20;

const SDRAMCR2_SRRC_SHIFT: u32 = 0;
const SDRAMCR2_REF2REF_SHIFT: u32 = 8;
const SDRAMCR2_ACT2ACT_SHIFT: u32 = 16;

const SDRAMCR3_REBL_SHIFT: u32 = 1;
const SDRAMCR3_PRESCALE_SHIFT: u32 = 8;
const SDRAMCR3_RT_SHIFT: u32 = 16;
const SDRAMCR3_UT_SHIFT: u32 = 24;

/// 模式寄存器里 CAS 延迟的位偏移（SDK 的 `SEMC_SDRAM_MODESETCAL_OFFSET`）。
const MODE_REG_CAS_SHIFT: u32 = 4;
/// IP 命令的 `IPCR1`：与既有固件一致（0x2）。
const IPCR1_VALUE: u32 = 0x2;

/// 把纳秒时长换算成 SEMC 的定时字段值（**周期数 − 1**，向上取整）。
fn cycles_minus_one(ns: u32, clk_hz: u32) -> Option<u32> {
    if clk_hz == 0 {
        return None;
    }
    let scaled = u64::from(ns) * u64::from(clk_hz);
    let cycles = u32::try_from(scaled.div_ceil(1_000_000_000)).ok()?;
    Some(cycles.saturating_sub(1))
}

/// 检查值能否放进给定宽度的字段。
fn field(value: u32, width: u32) -> Result<u32, TimingError> {
    if value < (1u32 << width) {
        Ok(value)
    } else {
        Err(TimingError::ClockTooSlow)
    }
}

const fn bit_field(value: u32, shift: u32, width: u32) -> u32 {
    (value & ((1u32 << width) - 1)) << shift
}

impl SemcTiming {
    /// 由器件规格、SEMC 时钟与基址算出全部寄存器值。
    ///
    /// # Errors
    ///
    /// 参数超出 SEMC 可编码范围时返回 [`TimingError`]（而不是静默截断）。
    pub fn compute(
        spec: &SdramSpec,
        semc_clock_hz: u32,
        base_address: u32,
    ) -> Result<Self, TimingError> {
        if semc_clock_hz == 0 {
            return Err(TimingError::ClockZero);
        }

        // --- SDRAMCR0 ---
        let ps = match spec.data_width_bits {
            8 => 0,
            16 => 1,
            _ => return Err(TimingError::UnsupportedWidth),
        };
        let bl = match spec.burst_len {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            _ => return Err(TimingError::UnsupportedBurst),
        };
        if !(8..=12).contains(&spec.column_bits) {
            return Err(TimingError::UnsupportedColumnBits);
        }
        let col = 12 - u32::from(spec.column_bits);
        // CL 字段直接等于延迟值；延迟 1 时字段为 0。
        let cl = match spec.cas_latency {
            1 => 0,
            2 => 2,
            3 => 3,
            4 => 4,
            _ => return Err(TimingError::UnsupportedLatency),
        };
        let sdramcr0 = bit_field(ps, SDRAMCR0_PS_SHIFT, 1)
            | bit_field(bl, SDRAMCR0_BL_SHIFT, 3)
            | bit_field(col, SDRAMCR0_COL_SHIFT, 2)
            | bit_field(cl, SDRAMCR0_CL_SHIFT, 2);

        // --- BR0：容量编码 + 基址 ---
        let kbytes = spec.size_kbytes();
        if kbytes < 4 {
            return Err(TimingError::SizeTooSmall);
        }
        // MS = log2(容量字节) - 12，用 size_kbytes / 8 的位数表达（不用浮点）。
        let half_kbytes = kbytes / 8;
        let ms = 32 - half_kbytes.leading_zeros();
        // BA 字段占 bit12..31，值是"地址高 20 位"放在 bit12 起：
        // 等价于"地址按 4 KB 对齐后整体写入"。
        let br0 = BR_VLD
            | (ms << BR_MS_SHIFT)
            | (((base_address >> BR_BA_SHIFT) & BR_BA_MASK) << BR_BA_SHIFT);

        // --- SDRAMCR1 ---
        let pre2act = field(ns_field(spec.trp_ns, semc_clock_hz)?, 4)?;
        let act2rw = field(ns_field(spec.trcd_ns, semc_clock_hz)?, 4)?;
        let rfrc = field(ns_field(spec.trfc_ns, semc_clock_hz)?, 5)?;
        let wrc = field(ns_field(spec.twr_ns, semc_clock_hz)?, 3)?;
        let ckeoff = field(ns_field(spec.tcke_ns, semc_clock_hz)?, 4)?;
        let act2pre = field(ns_field(spec.tras_ns, semc_clock_hz)?, 4)?;
        let sdramcr1 = (pre2act << SDRAMCR1_PRE2ACT_SHIFT)
            | (act2rw << SDRAMCR1_ACT2RW_SHIFT)
            | (rfrc << SDRAMCR1_RFRC_SHIFT)
            | (wrc << SDRAMCR1_WRC_SHIFT)
            | (ckeoff << SDRAMCR1_CKEOFF_SHIFT)
            | (act2pre << SDRAMCR1_ACT2PRE_SHIFT);

        // --- SDRAMCR2 ---
        let srrc = field(ns_field(spec.tsrr_ns, semc_clock_hz)?, 8)?;
        let ref2ref = field(ns_field(spec.trc_ns, semc_clock_hz)?, 8)?;
        let act2act = field(ns_field(spec.tact2act_ns, semc_clock_hz)?, 8)?;
        // 空闲超时：既有固件未设置（0），这里保持一致。
        let sdramcr2 = (srrc << SDRAMCR2_SRRC_SHIFT)
            | (ref2ref << SDRAMCR2_REF2REF_SHIFT)
            | (act2act << SDRAMCR2_ACT2ACT_SHIFT);

        // --- SDRAMCR3：预分频与刷新定时 ---
        let tclk_ps = 1_000_000_000_000u64.div_ceil(u64::from(semc_clock_hz));
        let prescale_period_ns = (u64::from(spec.prescale_cycles) * tclk_ps).div_ceil(1000);
        if prescale_period_ns == 0 {
            return Err(TimingError::ClockZero);
        }
        // prescale 字段 = 预分频周期 / 16 / tclk。因为预分频周期本来就定义为
        // `prescale_cycles` 个时钟周期，这里直接除以 16 就是**精确**结果；
        // 若绕一圈用"整数纳秒"再除回来（SDK 的写法），取整误差会让 10 变成 9。
        let prescale = u32::from(spec.prescale_cycles) / 16;
        let refresh = u32::try_from(u64::from(spec.refresh_row_ns) / prescale_period_ns)
            .map_err(|_| TimingError::ClockTooSlow)?;
        let urgent = u32::try_from(u64::from(spec.refresh_urgent_ns) / prescale_period_ns)
            .map_err(|_| TimingError::ClockTooSlow)?;
        if refresh == 0 {
            // 刷新间隔小于预分频周期：定时器无法表达。
            return Err(TimingError::ClockTooSlow);
        }
        let rebl = field(u32::from(spec.refresh_burst_len.saturating_sub(1)), 3)?;
        let sdramcr3 = (rebl << SDRAMCR3_REBL_SHIFT)
            | (field(prescale, 8)? << SDRAMCR3_PRESCALE_SHIFT)
            | (field(refresh - 1, 8)? << SDRAMCR3_RT_SHIFT)
            | (field(urgent, 8)? << SDRAMCR3_UT_SHIFT);

        // --- 模式寄存器 ---
        let mode_register = bl | (u32::from(spec.cas_latency) << MODE_REG_CAS_SHIFT);

        Ok(Self {
            br0,
            sdramcr0,
            sdramcr1,
            sdramcr2,
            sdramcr3,
            mode_register,
            ipcr1: IPCR1_VALUE,
            semc_clock_hz,
        })
    }
}

/// 系统 PLL（`CCM_ANALOG_PLL_SYS`，528 MHz PLL）输出频率：`Fout = Fref × (20 + 2×DIV_SELECT)`。
///
/// ⚠️ 这个编码被真机教训过两次，务必看清：
///
/// - `DIV_SELECT = 0` → ×20；`DIV_SELECT = 1` → **×22**（RM 对该字段只有这两个合法值）。
///   本板 24 MHz 晶振、Boot ROM 写入 `DIV_SELECT = 1` → **528 MHz**。
/// - 若把字段当"索引 +20"（`20 + v`）会算成 504 MHz；
///   若当成裸倍频值（`v`）会算成 24 MHz —— 实测后者会让 SEMC 后分频取成 0，
///   于是 SEMC 跑在 500 MHz（远超规格），**整块寄存器读回全 0、IP 命令必定超时**。
///
/// 交叉校验：528 MHz 的 PFD2（`FRAC=19` → ×18/19）≈ 500.2 MHz，
/// 除以 8 得 **62.5 MHz** —— 与既有 C 固件 `clock_config.c` 里记的
/// `SEMC_CLK_ROOT.outFreq = 62.5 MHz`、`SEMC_PODF.scale = 8` 完全一致。
#[must_use]
pub const fn pll_sys_hz(fref_hz: u32, div_select: u32) -> u32 {
    fref_hz * (20 + 2 * div_select)
}

/// 分数分频器（PFD）输出：`PFD = PLL × 18 / FRAC`，`FRAC` 合法范围 18..=35。
///
/// 中间量用 `u64`：528 MHz × 18 已经超出 `u32`。
#[must_use]
pub fn pfd_hz(pll_hz: u32, frac: u8) -> Option<u32> {
    if !(18..=35).contains(&frac) {
        return None;
    }
    let hz = u64::from(pll_hz) * 18 / u64::from(frac);
    u32::try_from(hz).ok()
}

/// 为 SEMC 选一个后分频值：在**不超过** `max_hz` 的前提下取最快的那个。
///
/// 返回 `(分频值, 寄存器字段值)`；字段值 = 分频值 − 1（SEMC_PODF 的编码）。
///
/// 为什么"取不超过上限里最快的"而不是固定值：SEMC 的时钟源是系统 PLL 的 PFD2，
/// 而上电时它由 Boot ROM 为 FlexSPI 配好（我们的代码**绝不能改它**，否则取指会断）。
/// 因此只能"读出现值再分频"，所以需要一个与现值无关的选取规则。
#[must_use]
pub fn semc_podf_for(pfd_hz: u32, max_hz: u32) -> Option<(u32, u32)> {
    if pfd_hz == 0 || max_hz == 0 {
        return None;
    }
    // 分频至少为 1；字段只有 3 位（上限 8）。超过 8 时 SEMC 会被迫跑得比
    // 上限快，由调用方决定是否接受。
    let div = pfd_hz.div_ceil(max_hz).clamp(1, 8);
    Some((div, div - 1))
}

/// 换算一个纳秒参数；时钟为 0 时返回错误。
fn ns_field(ns: u32, clk_hz: u32) -> Result<u32, TimingError> {
    cycles_minus_one(ns, clk_hz).ok_or(TimingError::ClockZero)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: SdramSpec = SdramSpec::W9825G6KH_6I;
    const BASE: u32 = 0x8000_0000;
    const CLK62: u32 = 62_500_000;
    const CLK66: u32 = 66_000_000;

    #[test]
    fn 板级参数与既有固件一致() {
        assert_eq!(SPEC.size_kbytes(), 0x8000, "容量应为 0x8000 KB = 32 MB");
        assert_eq!(SPEC.data_width_bits, 16);
        assert_eq!(SPEC.column_bits, 9);
        assert_eq!(SPEC.cas_latency, 3);
        assert_eq!(SPEC.burst_len, 1);
    }

    #[test]
    fn 规格自洽_出线位数推出的容量等于标称容量() {
        // 2^13 行 × 2^9 列 × 2^2 bank × 2 字节 = 2^25 = 32 MB
        assert_eq!(SPEC.derived_size_bytes(), u64::from(SPEC.size_bytes));
    }

    #[test]
    fn 周期换算按周期数减一并向上取整() {
        assert_eq!(cycles_minus_one(0, 64_000_000), Some(0));
        // 恰好一个周期 → 1-1 = 0
        assert_eq!(cycles_minus_one(15, 64_000_000), Some(0));
        // 略超一个周期 → 向上取整为 2 → 1
        assert_eq!(cycles_minus_one(16, 64_000_000), Some(1));
        assert_eq!(cycles_minus_one(18, 0), None);
    }

    #[test]
    fn 六十二点五兆时钟下的寄存器值() {
        let t = SemcTiming::compute(&SPEC, CLK62, BASE).expect("应能换算");
        // tclk = 16 ns
        // tRP=tRCD=18ns → ceil(18/16)=2 → 1
        // tRFC=127ns → ceil(7.94)=8 → 7；tWR=12ns → 1 → 0
        // tCKE=tRAS=42ns → ceil(2.625)=3 → 2
        assert_eq!((t.sdramcr1 >> SDRAMCR1_PRE2ACT_SHIFT) & 0xF, 1);
        assert_eq!((t.sdramcr1 >> SDRAMCR1_ACT2RW_SHIFT) & 0xF, 1);
        assert_eq!((t.sdramcr1 >> SDRAMCR1_RFRC_SHIFT) & 0x1F, 7);
        assert_eq!((t.sdramcr1 >> SDRAMCR1_WRC_SHIFT) & 0x7, 0);
        assert_eq!((t.sdramcr1 >> SDRAMCR1_CKEOFF_SHIFT) & 0xF, 2);
        assert_eq!((t.sdramcr1 >> SDRAMCR1_ACT2PRE_SHIFT) & 0xF, 2);
        assert_eq!(t.br0 & 1, 1, "VLD 必须置位");
        assert_eq!((t.br0 >> BR_MS_SHIFT) & 0x1F, 13, "32 MB → MS=13");
        assert_eq!(
            (t.br0 >> BR_BA_SHIFT) & BR_BA_MASK,
            BASE >> BR_BA_SHIFT,
            "基址必须写进 BA"
        );
        assert_eq!(t.mode_register, 0x30, "BL=1、CL=3 → 0b0011_0000");
    }

    #[test]
    fn sdramcr0_字段编码() {
        let t = SemcTiming::compute(&SPEC, CLK66, BASE).expect("应能换算");
        assert_eq!((t.sdramcr0 >> SDRAMCR0_PS_SHIFT) & 1, 1, "16 位端口");
        assert_eq!((t.sdramcr0 >> SDRAMCR0_BL_SHIFT) & 0x7, 0, "突发长度 1");
        assert_eq!(
            (t.sdramcr0 >> SDRAMCR0_COL_SHIFT) & 0x3,
            3,
            "9 位列 → 字段 3"
        );
        assert_eq!((t.sdramcr0 >> SDRAMCR0_CL_SHIFT) & 0x3, 3, "CL3 → 字段 3");
    }

    #[test]
    fn 刷新定时器落在合理范围() {
        let t = SemcTiming::compute(&SPEC, CLK66, BASE).expect("应能换算");
        let prescale = (t.sdramcr3 >> SDRAMCR3_PRESCALE_SHIFT) & 0xFF;
        let rt = (t.sdramcr3 >> SDRAMCR3_RT_SHIFT) & 0xFF;
        let ut = (t.sdramcr3 >> SDRAMCR3_UT_SHIFT) & 0xFF;
        let rebl = (t.sdramcr3 >> SDRAMCR3_REBL_SHIFT) & 0x7;
        // 预分频 = 160 周期 / 16 = 10
        assert_eq!(prescale, 10);
        // 预分频周期 ≈ 160 × 15.16 ns ≈ 2425 ns；每行 7812 ns → refresh = 3 → 字段 2
        assert_eq!(rt, 2, "刷新定时 = 周期数 - 1");
        assert_eq!(ut, 3, "紧急阈值与刷新定时不同：SDK 只对 RT 做 -1");
        assert_eq!(rebl, 0, "刷新突发长度 1 → 字段 0");
    }

    #[test]
    fn 刷新间隔_秒表算得出来() {
        // 预分频周期 × (RT+1) 应接近每行刷新间隔：这是"刷新不能太稀"的硬约束。
        let t = SemcTiming::compute(&SPEC, CLK66, BASE).expect("应能换算");
        let prescale = u64::from((t.sdramcr3 >> SDRAMCR3_PRESCALE_SHIFT) & 0xFF);
        let rt = u64::from((t.sdramcr3 >> SDRAMCR3_RT_SHIFT) & 0xFF) + 1;
        let tclk_ps = 1_000_000_000_000u64 / u64::from(CLK66);
        let actual_ns = prescale * 16 * tclk_ps * rt / 1000;
        let want_ns = u64::from(SPEC.refresh_row_ns);
        let diff = actual_ns.abs_diff(want_ns);
        assert!(
            diff * 2 < want_ns,
            "刷新间隔 {actual_ns} ns 与目标 {want_ns} ns 相差过大"
        );
    }

    #[test]
    fn 时钟越快同一时长需要更多周期() {
        // 18 ns 在 50 MHz（20 ns/周期）里只要 1 个周期，在 100 MHz（10 ns/周期）里要 2 个。
        // 字段是"周期数 - 1"，因此**时钟越快字段越大** —— 这正是换时钟必须重算的原因。
        let slow = SemcTiming::compute(&SPEC, 50_000_000, BASE).expect("应能换算");
        let fast = SemcTiming::compute(&SPEC, 100_000_000, BASE).expect("应能换算");
        let slow_trp = (slow.sdramcr1 >> SDRAMCR1_PRE2ACT_SHIFT) & 0xF;
        let fast_trp = (fast.sdramcr1 >> SDRAMCR1_PRE2ACT_SHIFT) & 0xF;
        let slow_trcd = (slow.sdramcr1 >> SDRAMCR1_ACT2RW_SHIFT) & 0xF;
        let fast_trcd = (fast.sdramcr1 >> SDRAMCR1_ACT2RW_SHIFT) & 0xF;
        assert_eq!(slow_trp, 0, "50 MHz：18 ns 不足 1 个周期 → 字段 0");
        assert_eq!(fast_trp, 1, "100 MHz：18 ns 需 2 个周期 → 字段 1");
        assert!(fast_trcd > slow_trcd, "tRCD 同理");
    }

    #[test]
    fn 非法参数被拒绝而不是静默截断() {
        let mut bad = SPEC;
        bad.data_width_bits = 32;
        assert_eq!(
            SemcTiming::compute(&bad, CLK66, BASE),
            Err(TimingError::UnsupportedWidth)
        );
        bad = SPEC;
        bad.burst_len = 3;
        assert_eq!(
            SemcTiming::compute(&bad, CLK66, BASE),
            Err(TimingError::UnsupportedBurst)
        );
        bad = SPEC;
        bad.cas_latency = 7;
        assert_eq!(
            SemcTiming::compute(&bad, CLK66, BASE),
            Err(TimingError::UnsupportedLatency)
        );
        bad = SPEC;
        bad.column_bits = 20;
        assert_eq!(
            SemcTiming::compute(&bad, CLK66, BASE),
            Err(TimingError::UnsupportedColumnBits)
        );
        bad = SPEC;
        bad.size_bytes = 1024;
        assert_eq!(
            SemcTiming::compute(&bad, CLK66, BASE),
            Err(TimingError::SizeTooSmall)
        );
        assert_eq!(
            SemcTiming::compute(&SPEC, 0, BASE),
            Err(TimingError::ClockZero)
        );
    }

    #[test]
    fn 系统pll与pfd换算() {
        // DIV_SELECT=1 → ×22 → 24 MHz × 22 = 528 MHz（本板 Boot ROM 的实际取值）
        assert_eq!(pll_sys_hz(24_000_000, 1), 528_000_000);
        // DIV_SELECT=0 → ×20 → 480 MHz
        assert_eq!(pll_sys_hz(24_000_000, 0), 480_000_000);
        // 反面：把字段当裸倍频值会得到 24 MHz，这个坑在真机上真的踩到过
        assert_ne!(pll_sys_hz(24_000_000, 1), 24_000_000);
        // 528 × 18 / 19 = 500.2 MHz（本板 PFD2_FRAC 实测为 19）
        assert_eq!(pfd_hz(528_000_000, 19), Some(500_210_526));
        // 528 × 18 / 24 = 396 MHz（另一种常见配置）
        assert_eq!(pfd_hz(528_000_000, 24), Some(396_000_000));
        // FRAC 越界要拒绝，而不是算出一个看似合理的数
        assert_eq!(pfd_hz(528_000_000, 17), None);
        assert_eq!(pfd_hz(528_000_000, 36), None);
    }

    #[test]
    fn semc后分频选择不超上限且尽量快() {
        // PFD2 = 500.2 MHz，上限 70 MHz → 8 分频 = 62.5 MHz（字段 7）
        // 这正是既有 C 固件 `clock_config.c` 记录的 SEMC 工况。
        assert_eq!(semc_podf_for(500_210_526, 70_000_000), Some((8, 7)));
        // PFD2 = 396 MHz，上限 70 MHz → 6 分频 = 66 MHz（字段 5）
        assert_eq!(semc_podf_for(396_000_000, 70_000_000), Some((6, 5)));
        // 上限很高时不浪费：1 分频
        assert_eq!(semc_podf_for(396_000_000, 500_000_000), Some((1, 0)));
        // 上限极低时被字段宽度卡在 8 分频
        assert_eq!(semc_podf_for(396_000_000, 1), Some((8, 7)));
        assert_eq!(semc_podf_for(0, 70_000_000), None);
    }

    #[test]
    fn 容量编码覆盖常见规格() {
        // MS = log2(容量字节) - 12：16 MB → 12，32 MB → 13，64 MB → 14
        for (bytes, want_ms) in [(16u32 << 20, 12u32), (32 << 20, 13), (64 << 20, 14)] {
            let mut s = SPEC;
            s.size_bytes = bytes;
            let t = SemcTiming::compute(&s, CLK66, BASE).expect("应能换算");
            assert_eq!((t.br0 >> BR_MS_SHIFT) & 0x1F, want_ms, "容量 {bytes} 字节");
        }
    }
}
