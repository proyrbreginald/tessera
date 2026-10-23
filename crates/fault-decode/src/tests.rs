//! `fault-decode` 的测试。
//!
//! 这些测试钉住的都是"**看起来对、实际把人带向错误方向**"的地方：
//! 把无效地址当成出错地址、把 FPU 寄存器当成 `PC`、把上电垃圾当成上次的崩溃记录、
//! 以及"一个位都没置"时不肯说"未知"而是猜一个原因。

extern crate std;

use std::{format, string::String};

use super::*;

/// 位定义必须与 `imxrt-ral` 的 `blocks/imxrt1015/src.rs` 一致（这里把偏移逐一钉住）。
#[test]
fn srsr_位偏移与_ral_的定义一致() {
    assert_eq!(srsr::IPP_RESET_B, 1 << 0);
    assert_eq!(srsr::LOCKUP_SYSRESETREQ, 1 << 1);
    assert_eq!(srsr::WDOG_RST_B, 1 << 4);
    assert_eq!(srsr::WDOG3_RST_B, 1 << 7);
    assert_eq!(srsr::JTAG_SW_RST, 1 << 6);
    // 已知位集合必须覆盖上面每一个。
    for bit in [
        srsr::IPP_RESET_B,
        srsr::LOCKUP_SYSRESETREQ,
        srsr::WDOG_RST_B,
        srsr::WDOG3_RST_B,
        srsr::JTAG_RST_B,
        srsr::JTAG_SW_RST,
    ] {
        assert_ne!(srsr::KNOWN & bit, 0, "KNOWN 漏了位 {bit:#x}");
    }
}

/// 单一来源的识别。
#[test]
fn 复位原因逐位识别() {
    let power_on = ResetCause::from_raw(srsr::IPP_RESET_B);
    assert!(power_on.is_power_on() && power_on.is_known());
    assert!(!power_on.is_watchdog() && !power_on.is_lockup() && !power_on.is_debugger());

    let lockup = ResetCause::from_raw(srsr::LOCKUP_SYSRESETREQ);
    assert!(lockup.is_lockup() && !lockup.is_power_on());

    for raw in [srsr::WDOG_RST_B, srsr::WDOG3_RST_B] {
        assert!(
            ResetCause::from_raw(raw).is_watchdog(),
            "{raw:#x} 应识别为看门狗"
        );
    }
    for raw in [srsr::JTAG_RST_B, srsr::JTAG_SW_RST] {
        assert!(
            ResetCause::from_raw(raw).is_debugger(),
            "{raw:#x} 应识别为调试器复位（否则排障方向会跑偏）"
        );
    }
}

/// **一个位都没有时不许猜**：说"未知"，不说"上电复位"。
#[test]
fn 没有已知位时如实说未知() {
    let unknown = ResetCause::from_raw(0);
    assert!(!unknown.is_known());
    assert!(!unknown.is_power_on());

    let mut text = String::new();
    unknown.describe(&mut text).unwrap();
    assert!(text.contains("未知"), "应当明说未知：{text}");
    assert!(!text.contains("上电"), "不得猜成上电：{text}");
    assert_eq!(unknown.clear_mask(), 0, "没有已知位就没有要清的位");
}

/// 多个来源同时置位要**全部列出**（掉电 + 看门狗是常见的组合）。
#[test]
fn 多个复位来源同时列出() {
    let raw = srsr::IPP_RESET_B | srsr::WDOG_RST_B;
    let cause = ResetCause::from_raw(raw);
    assert!(cause.is_power_on() && cause.is_watchdog());

    let mut text = String::new();
    cause.describe(&mut text).unwrap();
    assert!(text.contains("上电") && text.contains("看门狗"), "{text}");
    assert!(
        text.contains(&format!("0x{raw:08x}")),
        "原始值要原样打出：{text}"
    );
}

/// 写 1 清零只清**已知**位：保留位不能被静默抹掉。
#[test]
fn 清零掩码只覆盖已知位() {
    let raw = 0xFFFF_FFFF;
    let cause = ResetCause::from_raw(raw);
    assert_eq!(cause.clear_mask(), srsr::KNOWN);
    assert_eq!(cause.clear_mask() & !srsr::KNOWN, 0);
}

/// `MMFAR`/`BFAR` 只在 VALID 位置位时才可信 —— **不可信就不打印**。
#[test]
fn 无效的出错地址不打印() {
    // 有 MemManage 状态位，但 MMARVALID 没置 ⇒ 地址不可信。
    let status = FaultStatus {
        cfsr: fault_bits::MMFSR_DACCVIOL,
        hfsr: 0,
        mmfar: 0xDEAD_BEEF,
        bfar: 0,
    };
    assert!(!status.mmfar_valid());
    let mut text = String::new();
    status.describe(&mut text).unwrap();
    assert!(!text.contains("DEADBEEF"), "不可信的地址不得打印：{text}");
    assert!(text.contains("MMFAR 无效"), "{text}");

    // 置上 VALID 之后才打印。
    let valid = FaultStatus {
        cfsr: fault_bits::MMFSR_DACCVIOL | fault_bits::MMFSR_MMARVALID,
        ..status
    };
    assert!(valid.mmfar_valid());
    let mut text = String::new();
    valid.describe(&mut text).unwrap();
    assert!(
        text.contains("deadbeef") || text.contains("DEADBEEF"),
        "{text}"
    );
}

/// BusFault 的"不精确"必须显眼提示（它是最难查的一类）。
#[test]
fn 不精确总线错误有显眼提示() {
    let status = FaultStatus {
        cfsr: fault_bits::BFSR_IMPRECISERR,
        hfsr: fault_bits::HFSR_FORCED,
        mmfar: 0,
        bfar: 0,
    };
    assert!(status.is_escalated(), "FORCED 表示由使能故障升级而来");
    let mut text = String::new();
    status.describe(&mut text).unwrap();
    assert!(text.contains("不精确"), "{text}");
    assert!(text.contains("升级"), "{text}");
}

/// 空状态要能被识别（含义：要么真没故障，要么 fault 还没使能）。
#[test]
fn 空故障状态可识别() {
    let empty = FaultStatus::default();
    assert!(empty.is_empty());
    assert!(!empty.is_escalated());
    assert!(!empty.mmfar_valid() && !empty.bfar_valid());
}

/// 栈帧切分：恰好 8 个字能切出；少一个字就**报取不到**，不许越界读垃圾。
#[test]
fn 栈帧切分与帧长常量() {
    let stack = [
        0x1111_1111u32,
        0x2222_2222,
        0x3333_3333,
        0x4444_4444,
        0x5555_5555,
        0x6666_6666,
        0xAAAA_0000,
        0xBBBB_BBBB,
    ];
    let frame = FaultFrame::from_stack(&stack, 0x2000_1000).expect("8 个字应当能切出基本帧");
    assert_eq!(frame.r0, 0x1111_1111);
    assert_eq!(frame.lr, 0x6666_6666);
    assert_eq!(frame.pc, 0xAAAA_0000);
    assert_eq!(frame.xpsr, 0xBBBB_BBBB);
    assert_eq!(frame.sp, 0x2000_1000, "帧必须自带 SP");

    assert!(
        FaultFrame::from_stack(&stack[..7], 0x2000_1000).is_none(),
        "少一个字必须报 None"
    );

    // 帧长是**硬件事实**：基本 32 字节、含 FPU 扩展 104 字节。
    assert_eq!(FaultFrame::BASIC_WORDS * 4, 32);
    assert_eq!(FaultFrame::EXTENDED_WORDS * 4, 104);
}

/// 帧描述必须同时给出 `PC`/`LR`/`SP`（只给 PC 现场就少一半）。
#[test]
fn 帧描述含_pc_lr_sp() {
    let frame = FaultFrame::from_stack(&[1, 2, 3, 4, 5, 0x6666, 0x7777, 8], 0x2000_1234).unwrap();
    let mut text = String::new();
    frame.describe(&mut text, true).unwrap();
    assert!(text.contains("PC=0x00007777"), "{text}");
    assert!(text.contains("LR=0x00006666"), "{text}");
    assert!(text.contains("SP=0x20001234"), "{text}");
    assert!(text.contains("FPU 扩展帧"), "要说明帧是否含 FPU：{text}");
}

/// 崩溃记录往返编解码。
#[test]
fn 崩溃记录往返() {
    let record = CrashRecord {
        reset_cause: srsr::WDOG_RST_B,
        cfsr: fault_bits::BFSR_PRECISERR,
        hfsr: fault_bits::HFSR_FORCED,
        mmfar: 0,
        bfar: 0x6000_1234,
        pc: 0x0000_8C20,
        lr: 0x0000_1235,
        sp: 0x2000_C000,
        uptime_millis: 12345,
        fault_streak: 2,
    };
    let words = record.encode();
    assert_eq!(words.len(), CrashRecord::WORDS);
    assert_eq!(CrashRecord::decode(&words), Some(record));

    let mut text = String::new();
    record.describe(&mut text).unwrap();
    assert!(text.contains("看门狗"), "{text}");
    assert!(text.contains("PC=0x00008c20"), "{text}");
    assert!(text.contains("12345 ms"), "{text}");
    assert!(text.contains("连续第 2 次"), "{text}");
}

/// 上电垃圾不能被当成"上次的崩溃记录"：魔数不对或校验和不匹配都要拒绝。
#[test]
fn 垃圾内容不会被当成崩溃记录() {
    // 全 0（未初始化 RAM 的常见形态）。
    assert_eq!(CrashRecord::decode(&[0u32; CrashRecord::WORDS]), None);
    // 全 0xFF。
    assert_eq!(
        CrashRecord::decode(&[0xFFFF_FFFFu32; CrashRecord::WORDS]),
        None
    );
    // 但 `empty()` 经 `encode()` 之后是**合法记录**：它的含义是"我们确实写过一条，
    // 内容是：复位原因没有已知位、也没有故障帧"。这与"上电垃圾"必须区分开 ——
    // 前者可以说"干净复位"，后者只能说"这段 RAM 不是我们写的"。
    let written_empty = CrashRecord::decode(&CrashRecord::empty().encode());
    assert_eq!(written_empty, Some(CrashRecord::empty()));
    assert!(!written_empty.unwrap_or_default().has_frame());

    // 魔数对、但有一个字段被改坏 ⇒ 校验和抓住它。
    let mut words = CrashRecord {
        reset_cause: srsr::IPP_RESET_B,
        ..CrashRecord::empty()
    }
    .encode();
    words[6] ^= 0x0000_0100;
    assert_eq!(
        CrashRecord::decode(&words),
        None,
        "改坏字段必须被校验和发现"
    );

    // 太短的输入也不能 panic。
    assert_eq!(CrashRecord::decode(&words[..3]), None);
}

/// 校验和的基本性质：同输入同结果、改一个字节就变。
#[test]
fn 校验和的基本性质() {
    let a = checksum(&[1, 2, 3]);
    assert_eq!(a, checksum(&[1, 2, 3]));
    assert_ne!(a, checksum(&[1, 2, 4]));
    assert_ne!(checksum(&[]), checksum(&[0]));
}

/// `has_frame`：没有帧就不要假装有（干净复位与崩溃必须可区分）。
#[test]
fn 有无故障帧可区分() {
    assert!(!CrashRecord::empty().has_frame());
    let with_frame = CrashRecord {
        pc: 0x1234,
        ..CrashRecord::empty()
    };
    assert!(with_frame.has_frame());

    let mut text = String::new();
    CrashRecord::empty().describe(&mut text).unwrap();
    assert!(text.contains("无故障帧"), "{text}");
}
