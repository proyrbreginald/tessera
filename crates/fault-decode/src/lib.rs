//! `fault-decode` —— **复位原因**与**故障现场**的解码
//! （纯逻辑，主机可测；**平台需求 DB-1**）。
//!
//! # 为什么这个 crate 值得存在
//!
//! 2026-10-12 的只读取证审计给出了本底座最刺眼的一条结论：
//!
//! > **`HardFault` 的处理程序在链接产物里就是一个空死循环**（`b .`），
//! > 全仓零个 `#[exception]`，`SRC_SRSR` 全仓零命中 ⇒ 设备挂掉之后
//! > **既不知道"上一把是怎么挂的"，也留不下任何现场**：没有 `CFSR`/`HFSR`、
//! > 没有 `PC`/`LR`/`SP`、没有崩溃前的日志。现场只剩"串口安静"。
//!
//! 而"解释不了故障"这件事，对本项目是**硬伤**：现场故障恰恰没有调试器（这是
//! [`ADR-0004`](../../docs/adr/0004-日志方案不使用defmt.md) 立论的出发点）。
//!
//! 更关键的是：**这类解码逻辑天生是纯算术**。位域怎么翻、栈帧怎么切、
//! "哪些位组合是自相矛盾"—— 这些判断出错的方式全都是纯逻辑的错，而它们
//! 完全可以在主机上 `cargo test` 反复跑。留在 BSP 里就只能"上机碰运气"。
//!
//! # 三个部分
//!
//! 1. [`ResetCause`]：`SRC_SRSR` 的位 → "上次为什么复位"。位定义取自
//!    `imxrt-ral` 的 `blocks/imxrt1015/src.rs`（本芯片族同一份块定义），不是我按印象写的。
//! 2. [`FaultStatus`]：`CFSR`/`HFSR` 的位 → "哪一类故障、根因是什么"。位定义来自
//!    ARMv7-M 架构手册（`CFSR` = `MMFSR` | `BFSR` | `UFSR`）。
//! 3. [`FaultFrame`] + [`CrashRecord`]：硬件入栈帧怎么切（**含 FPU 扩展帧**，Cortex-M7
//!    有 FPU，这一点最容易切错），以及"留在 RAM 里、跨暖复位存活"的崩溃记录怎么编解码。
//!
//! # 一条纪律：**不知道就说不出来，别编**
//!
//! [`ResetCause::describe`] 对"一个位都没置"的情况输出 `未知`，而不是猜一个"上电复位"。
//! 同理 [`FaultStatus::is_empty`] 为真时，调用方应当说"没有任何故障状态位"——
//! 那通常意味着**故障发生在使能这些 fault 之前**（例如 `SHCSR` 还没配好就崩了）。

#![no_std]
// 纯位运算与算术：本 crate 存在的意义之一就是能被主机测试反复跑。
#![cfg_attr(not(target_os = "none"), forbid(unsafe_code))]
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
// 测试里放宽：断言失败就该直接 panic。
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )
)]

use core::fmt;

/// `SRC_SRSR` 的位定义（来自 `imxrt-ral` 的 `blocks/imxrt1015/src.rs`：`pub mod SRSR`）。
///
/// 每一位都是"上一次复位的一个可能来源"，**可以同时置位**（例如掉电 + 上电）。
pub mod srsr {
    /// 上电复位（IPP）。
    pub const IPP_RESET_B: u32 = 1 << 0;
    /// CPU lockup 触发的系统复位请求（内核进入 LOCKUP 状态）。
    pub const LOCKUP_SYSRESETREQ: u32 = 1 << 1;
    /// CSU 复位。
    pub const CSU_RESET_B: u32 = 1 << 2;
    /// IPP 用户复位。
    pub const IPP_USER_RESET_B: u32 = 1 << 3;
    /// 看门狗复位（WDOG1/2）。
    pub const WDOG_RST_B: u32 = 1 << 4;
    /// JTAG 复位。
    pub const JTAG_RST_B: u32 = 1 << 5;
    /// JTAG 软件复位。
    pub const JTAG_SW_RST: u32 = 1 << 6;
    /// 看门狗 3 复位。
    pub const WDOG3_RST_B: u32 = 1 << 7;
    /// 温度传感器复位。
    pub const TEMPSENSE_RST_B: u32 = 1 << 8;
    /// 本模块认识的位（其余位保留）。
    pub const KNOWN: u32 = IPP_RESET_B
        | LOCKUP_SYSRESETREQ
        | CSU_RESET_B
        | IPP_USER_RESET_B
        | WDOG_RST_B
        | JTAG_RST_B
        | JTAG_SW_RST
        | WDOG3_RST_B
        | TEMPSENSE_RST_B;
}

/// "上次为什么复位"。
///
/// 构造自 `SRC_SRSR` 的原始值；解码是纯位运算，因此可主机测试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetCause {
    raw: u32,
}

impl ResetCause {
    /// 从 `SRC_SRSR` 读到的原始值构造。
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// 原始值（便于日志里原样打出，避免"解码错了就全错"）。
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.raw
    }

    /// 上电复位（掉电或首次上电）。
    #[must_use]
    pub const fn is_power_on(self) -> bool {
        self.raw & srsr::IPP_RESET_B != 0
    }

    /// 由 **CPU lockup** 触发的复位 —— 通常意味着跑了非法指令或访问了坏地址后无人处理。
    #[must_use]
    pub const fn is_lockup(self) -> bool {
        self.raw & srsr::LOCKUP_SYSRESETREQ != 0
    }

    /// 看门狗（WDOG1/2 或 WDOG3）复位。
    #[must_use]
    pub const fn is_watchdog(self) -> bool {
        self.raw & (srsr::WDOG_RST_B | srsr::WDOG3_RST_B) != 0
    }

    /// 调试器（JTAG）导致的复位：`JTAG_RST_B` 或 JTAG 软件复位。
    ///
    /// **为什么必须单独分辨**：台架调试时最常见的就是这一类复位 ——
    /// 把它当成"现场故障"会让排障方向完全跑偏。
    #[must_use]
    pub const fn is_debugger(self) -> bool {
        self.raw & (srsr::JTAG_RST_B | srsr::JTAG_SW_RST) != 0
    }

    /// 有没有任何一个**已知**位被置上。
    ///
    /// 为假时说明读到的值里没有可解释的复位来源（可能是读得太早、或寄存器行为与预期不符）——
    /// 此时应当说"未知"，不要猜。
    #[must_use]
    pub const fn is_known(self) -> bool {
        self.raw & srsr::KNOWN != 0
    }

    /// 应当写回 `SRC_SRSR` 的**写 1 清零**掩码（清掉本次已上报的位）。
    ///
    /// 只清已知位：保留位留给下次判断，避免把将来新增的位静默抹掉。
    #[must_use]
    pub const fn clear_mask(self) -> u32 {
        self.raw & srsr::KNOWN
    }

    /// 一行人话（进日志/横幅用）。**多个来源同时置位时会全部列出**。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn describe(self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "0x{:08x}", self.raw)?;
        if !self.is_known() {
            return write!(out, "（未知：没有任何已知复位来源位）");
        }
        write!(out, " [")?;
        let mut first = true;
        for (flag, name) in [
            (srsr::IPP_RESET_B, "上电"),
            (srsr::LOCKUP_SYSRESETREQ, "CPU-LOCKUP"),
            (srsr::WDOG_RST_B, "看门狗"),
            (srsr::WDOG3_RST_B, "看门狗3"),
            (srsr::JTAG_RST_B, "调试器"),
            (srsr::JTAG_SW_RST, "调试器软复位"),
            (srsr::CSU_RESET_B, "CSU"),
            (srsr::IPP_USER_RESET_B, "用户复位"),
            (srsr::TEMPSENSE_RST_B, "温度传感器"),
        ] {
            if self.raw & flag != 0 {
                if !first {
                    write!(out, "|")?;
                }
                write!(out, "{name}")?;
                first = false;
            }
        }
        write!(out, "]")
    }
}

/// `CFSR`（Configurable Fault Status Register）与配套地址寄存器的解码。
///
/// `CFSR` 由三段拼成（ARMv7-M）：低字节 `MMFSR`、次字节 `BFSR`、高半字 `UFSR`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FaultStatus {
    /// `CFSR`：可配置故障的状态位。
    pub cfsr: u32,
    /// `HFSR`：硬故障状态位。
    pub hfsr: u32,
    /// `MMFAR`：触发 MemManage 的地址（有效位为 `MMFSR.MMARVALID`）。
    pub mmfar: u32,
    /// `BFAR`：触发 BusFault 的地址（有效位为 `BFSR.BFARVALID`）。
    pub bfar: u32,
}

/// `CFSR`/`HFSR` 的位定义（ARMv7-M）。
pub mod fault_bits {
    // ---- MMFSR（CFSR[7:0]）----
    /// 指令取指被 MPU/权限拒绝。
    pub const MMFSR_IACCVIOL: u32 = 1 << 0;
    /// 数据访问被 MPU/权限拒绝。
    pub const MMFSR_DACCVIOL: u32 = 1 << 1;
    /// 非特权访问被拒。
    pub const MMFSR_MUNSTKERR: u32 = 1 << 3;
    /// 出栈时出错。
    pub const MMFSR_MSTKERR: u32 = 1 << 4;
    /// 浮点惰性保存出错。
    pub const MMFSR_MLSPERR: u32 = 1 << 5;
    /// `MMFAR` 有效。
    pub const MMFSR_MMARVALID: u32 = 1 << 7;

    // ---- BFSR（CFSR[15:8]）----
    /// 总线错误：取指。
    pub const BFSR_IBUSERR: u32 = 1 << 8;
    /// 总线错误：精确的数据访问。
    pub const BFSR_PRECISERR: u32 = 1 << 9;
    /// 总线错误：不精确的数据访问（**最难查的一类**：出错点与触发点不在一起）。
    pub const BFSR_IMPRECISERR: u32 = 1 << 10;
    /// 入栈时总线错误。
    pub const BFSR_STKERR: u32 = 1 << 11;
    /// 出栈时总线错误。
    pub const BFSR_UNSTKERR: u32 = 1 << 12;
    /// 浮点惰性保存总线错误。
    pub const BFSR_LSPERR: u32 = 1 << 13;
    /// `BFAR` 有效。
    pub const BFSR_BFARVALID: u32 = 1 << 15;

    // ---- UFSR（CFSR[31:16]）----
    /// 除零。
    pub const UFSR_DIVBYZERO: u32 = 1 << 25;
    /// 非对齐访问。
    pub const UFSR_UNALIGNED: u32 = 1 << 24;
    /// 协处理器访问被拒。
    pub const UFSR_NOCP: u32 = 1 << 19;
    /// 非法指令。
    pub const UFSR_INVSTATE: u32 = 1 << 18;
    /// 非法状态（例如 EPSR.T 被清掉后执行）。
    pub const UFSR_UNDEFINSTR: u32 = 1 << 16;

    // ---- HFSR ----
    /// 硬故障由"使能的故障升级"而来（**最常见的 HardFault 原因**）。
    pub const HFSR_FORCED: u32 = 1 << 30;
    /// 取向量表时出错。
    pub const HFSR_VECTTBL: u32 = 1 << 1;
    /// 调试事件触发。
    pub const HFSR_DEBUGEVT: u32 = 1 << 31;
}

impl FaultStatus {
    /// 有没有任何状态位被置上。
    ///
    /// **为真时的含义要读准**：可能确实是"没有故障"（处理器正常退出），
    /// 也可能是"故障发生时这些 fault 还没被使能"（`SHCSR` 未配）——
    /// 后者会让根因信息永久丢失，因此固件必须**早**配 `SHCSR`。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.cfsr == 0 && self.hfsr == 0
    }

    /// `MMFAR` 是否可信（只有 `MMARVALID` 置位时才有意义）。
    #[must_use]
    pub const fn mmfar_valid(self) -> bool {
        self.cfsr & fault_bits::MMFSR_MMARVALID != 0
    }

    /// `BFAR` 是否可信。
    #[must_use]
    pub const fn bfar_valid(self) -> bool {
        self.cfsr & fault_bits::BFSR_BFARVALID != 0
    }

    /// 是否由"使能的故障升级"而来（HardFault 的典型成因）。
    #[must_use]
    pub const fn is_escalated(self) -> bool {
        self.hfsr & fault_bits::HFSR_FORCED != 0
    }

    /// 逐条把人话写进 `out`（一条一行，方便直接进日志）。
    ///
    /// **不可信的信息不打印**：`MMFAR`/`BFAR` 的值只在对应 VALID 位置位时才输出 ——
    /// 否则会把一个随机残留值当成"出错地址"，把人带向错误方向。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn describe(self, out: &mut dyn fmt::Write) -> fmt::Result {
        writeln!(out, "CFSR=0x{:08x} HFSR=0x{:08x}", self.cfsr, self.hfsr)?;
        for (flag, text) in [
            (
                fault_bits::MMFSR_IACCVIOL,
                "MemManage：取指被拒（MPU/权限）",
            ),
            (
                fault_bits::MMFSR_DACCVIOL,
                "MemManage：数据访问被拒（MPU/权限）",
            ),
            (fault_bits::MMFSR_MUNSTKERR, "MemManage：出栈出错"),
            (
                fault_bits::MMFSR_MSTKERR,
                "MemManage：入栈出错（**栈有问题**）",
            ),
            (fault_bits::MMFSR_MLSPERR, "MemManage：浮点惰性保存出错"),
            (fault_bits::BFSR_IBUSERR, "BusFault：取指总线错误"),
            (fault_bits::BFSR_PRECISERR, "BusFault：精确的数据访问错误"),
            (
                fault_bits::BFSR_IMPRECISERR,
                "BusFault：**不精确**的数据访问错误（出错点与触发点不在一起）",
            ),
            (
                fault_bits::BFSR_STKERR,
                "BusFault：入栈总线错误（**栈有问题**）",
            ),
            (fault_bits::BFSR_UNSTKERR, "BusFault：出栈总线错误"),
            (fault_bits::BFSR_LSPERR, "BusFault：浮点惰性保存总线错误"),
            (fault_bits::UFSR_UNDEFINSTR, "UsageFault：非法指令"),
            (fault_bits::UFSR_INVSTATE, "UsageFault：非法状态（EPSR.T）"),
            (fault_bits::UFSR_NOCP, "UsageFault：协处理器访问被拒"),
            (fault_bits::UFSR_UNALIGNED, "UsageFault：非对齐访问"),
            (fault_bits::UFSR_DIVBYZERO, "UsageFault：除零"),
        ] {
            if self.cfsr & flag != 0 {
                writeln!(out, "  - {text}")?;
            }
        }
        if self.hfsr & fault_bits::HFSR_FORCED != 0 {
            writeln!(out, "  - HFSR：由使能的故障升级而来（看上面 CFSR）")?;
        }
        if self.hfsr & fault_bits::HFSR_VECTTBL != 0 {
            writeln!(out, "  - HFSR：取向量表出错（**中断向量表本身有问题**）")?;
        }
        if self.hfsr & fault_bits::HFSR_DEBUGEVT != 0 {
            writeln!(out, "  - HFSR：调试事件")?;
        }
        if self.mmfar_valid() {
            writeln!(out, "  MMFAR=0x{:08x}（有效）", self.mmfar)?;
        } else if self.cfsr & 0xff != 0 {
            writeln!(out, "  MMFAR 无效（MemManage 状态位有置位但没有有效地址）")?;
        }
        if self.bfar_valid() {
            writeln!(out, "  BFAR =0x{:08x}（有效）", self.bfar)?;
        } else if self.cfsr & 0xff00 != 0 {
            writeln!(out, "  BFAR 无效（BusFault 状态位有置位但没有有效地址）")?;
        }
        Ok(())
    }
}

/// 硬件入栈的异常帧（`R0 R1 R2 R3 R12 LR PC xPSR`）。
///
/// # 为什么单独建模"基本帧 / 扩展帧"
///
/// Cortex-M7 **有 FPU**：若故障发生时浮点上下文是活跃的，硬件会额外压入
/// `S0..S15 + FPSCR + 保留字`（共 104 字节）。切错帧的后果是**把 FPU 寄存器当成
/// `PC`/`LR` 打印出来** —— 那比不打更糟：它给人一个看起来很像地址的假现场。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultFrame {
    /// `R0`
    pub r0: u32,
    /// `R1`
    pub r1: u32,
    /// `R2`
    pub r2: u32,
    /// `R3`
    pub r3: u32,
    /// `R12`
    pub r12: u32,
    /// `LR`（返回地址）
    pub lr: u32,
    /// `PC`（**故障指令地址**，最有价值的一个字）
    pub pc: u32,
    /// `xPSR`
    pub xpsr: u32,
    /// 故障发生时的栈指针。
    ///
    /// **必须记下来**：只打 `PC` 而不知道 `SP`，现场就少一半 ——
    /// 栈指针是判断"是不是栈溢出/栈被写坏"的唯一入口。
    pub sp: u32,
}

impl FaultFrame {
    /// 基本帧的字数（8 个字 = 32 字节）。
    pub const BASIC_WORDS: usize = 8;

    /// 扩展帧（含 FPU 上下文）的字数：8 + 18 = 26 个字（32 + 72 字节 = 104 字节）。
    pub const EXTENDED_WORDS: usize = 26;

    /// 从硬件入栈帧切出基本帧。`sp` 是帧所在的地址（即故障时的栈指针）。
    ///
    /// 返回 `None` 表示 `stack` 太短 —— **宁可报"取不到现场"，也不要用越界读出来的
    /// 垃圾值冒充现场**。
    #[must_use]
    pub fn from_stack(stack: &[u32], sp: u32) -> Option<Self> {
        let words = stack.get(..Self::BASIC_WORDS)?;
        Some(Self {
            r0: *words.first()?,
            r1: *words.get(1)?,
            r2: *words.get(2)?,
            r3: *words.get(3)?,
            r12: *words.get(4)?,
            lr: *words.get(5)?,
            pc: *words.get(6)?,
            xpsr: *words.get(7)?,
            sp,
        })
    }

    /// 该帧对应的**栈指针**：`stack` 的起始地址即故障发生时的 SP。
    ///
    /// 单独给一个函数是为了让调用者不容易忘：只打 `PC` 而不知道 `SP`，现场就少一半。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn describe(self, out: &mut dyn fmt::Write, extended: bool) -> fmt::Result {
        write!(
            out,
            "PC=0x{:08x} LR=0x{:08x} SP=0x{:08x} xPSR=0x{:08x}{}",
            self.pc,
            self.lr,
            self.sp,
            self.xpsr,
            if extended {
                "（含 FPU 扩展帧）"
            } else {
                ""
            }
        )?;
        write!(
            out,
            " R0=0x{:08x} R1=0x{:08x} R2=0x{:08x} R3=0x{:08x} R12=0x{:08x}",
            self.r0, self.r1, self.r2, self.r3, self.r12
        )
    }
}

/// 崩溃记录：**跨暖复位**留在 RAM 里的一小段结构化信息。
///
/// 冷启动读它，就能自报"上一次是为什么挂的"。字段刻意少而硬：
/// 复位原因、故障状态、故障帧、崩溃时的运行时长、以及一个用于判断
/// "这段 RAM 到底是上次写的、还是上电垃圾"的校验和。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrashRecord {
    /// 复位原因（`SRC_SRSR` 原始值）。
    pub reset_cause: u32,
    /// `CFSR`
    pub cfsr: u32,
    /// `HFSR`
    pub hfsr: u32,
    /// `MMFAR`
    pub mmfar: u32,
    /// `BFAR`
    pub bfar: u32,
    /// 故障指令地址（`0` 表示没有帧）。
    pub pc: u32,
    /// 返回地址。
    pub lr: u32,
    /// 故障时的栈指针。
    pub sp: u32,
    /// 崩溃时的运行时长（毫秒；`0` 表示未知）。
    pub uptime_millis: u32,
    /// **连续**第几次故障（1 = 本故障之前一切正常）。
    ///
    /// 用途：决定"故障后复位"还是"停机等人"。反复故障时若无条件复位，设备就变成
    /// 复位循环 —— 而本板的外部看门狗由 I2C 扩展器控制、固件目前既不能开也不能关，
    /// 没有任何东西能把它从循环里拉出来。
    pub fault_streak: u32,
}

impl Default for CrashRecord {
    fn default() -> Self {
        Self::empty()
    }
}

impl CrashRecord {
    /// 记录格式的魔数（"FaultRecord" 的首字母 + 版本，便于一眼看出是什么）。
    pub const MAGIC: u32 = 0x4641_0001;

    /// 空记录（校验和必然不匹配，因此不会被误认为有效）。
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            reset_cause: 0,
            cfsr: 0,
            hfsr: 0,
            mmfar: 0,
            bfar: 0,
            pc: 0,
            lr: 0,
            sp: 0,
            uptime_millis: 0,
            fault_streak: 0,
        }
    }

    /// 有没有值得上报的故障现场（`PC` 非 0 即认为有帧）。
    #[must_use]
    pub const fn has_frame(&self) -> bool {
        self.pc != 0
    }

    /// 编码成固定长度的字数组（`[magic, 8 个字段, checksum]`）。
    #[must_use]
    pub fn encode(&self) -> [u32; Self::WORDS] {
        let mut words = [
            Self::MAGIC,
            self.reset_cause,
            self.cfsr,
            self.hfsr,
            self.mmfar,
            self.bfar,
            self.pc,
            self.lr,
            self.sp,
            self.uptime_millis,
            self.fault_streak,
            0,
        ];
        let checksum = checksum(&words[..Self::WORDS - 1]);
        if let Some(slot) = words.get_mut(Self::WORDS - 1) {
            *slot = checksum;
        }
        words
    }

    /// 从字数组解码。`None` = 魔数不对或校验和不匹配（**这块 RAM 不是我们写的**）。
    ///
    /// 用 FNV-1a 而不是 CRC：这里要回答的问题只是"这段 RAM 是上次我们写的那份、
    /// 还是上电后的随机内容"，不是抗攻击的完整性校验。选它的理由是**无需依赖**
    /// 且实现只有几行 —— 将来要换成 `filesystem` 的 CRC-32 也不影响接口。
    #[must_use]
    pub fn decode(words: &[u32]) -> Option<Self> {
        let all = words.get(..Self::WORDS)?;
        if *all.first()? != Self::MAGIC {
            return None;
        }
        let expected = *all.get(Self::WORDS - 1)?;
        if checksum(all.get(..Self::WORDS - 1)?) != expected {
            return None;
        }
        Some(Self {
            reset_cause: *all.get(1)?,
            cfsr: *all.get(2)?,
            hfsr: *all.get(3)?,
            mmfar: *all.get(4)?,
            bfar: *all.get(5)?,
            pc: *all.get(6)?,
            lr: *all.get(7)?,
            sp: *all.get(8)?,
            uptime_millis: *all.get(9)?,
            fault_streak: *all.get(10)?,
        })
    }

    /// 记录占多少个字。
    pub const WORDS: usize = 12;

    /// 一行人话（冷启动上报用）。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn describe(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "复位=")?;
        ResetCause::from_raw(self.reset_cause).describe(out)?;
        if self.has_frame() {
            write!(
                out,
                " 故障：PC=0x{:08x} LR=0x{:08x} SP=0x{:08x} CFSR=0x{:08x} HFSR=0x{:08x}",
                self.pc, self.lr, self.sp, self.cfsr, self.hfsr
            )?;
            if self.uptime_millis != 0 {
                write!(out, " 崩溃于 {} ms", self.uptime_millis)?;
            }
            if self.fault_streak > 1 {
                write!(out, "（连续第 {} 次）", self.fault_streak)?;
            }
        } else {
            write!(out, " 无故障帧（可能是干净复位）")?;
        }
        Ok(())
    }
}

/// FNV-1a 32 位校验和（见 [`CrashRecord::decode`] 里关于选型的说明）。
#[must_use]
pub fn checksum(words: &[u32]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for word in words {
        for byte in word.to_le_bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    hash
}

#[cfg(test)]
mod tests;
