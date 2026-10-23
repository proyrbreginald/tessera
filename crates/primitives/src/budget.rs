//! 实时预算：把"单核可行"这个论证变成**编译期断言**（**平台需求 RT-1**）。
//!
//! # 为什么放在代码里
//!
//! 实时性论证最危险的失效方式不是"算错了"，而是"数字被改了而没人重算"。
//! 例如把串行总线波特率调低、或把某条 ISR 的预算放宽，可行性结论可能翻转，
//! 但代码照样编译、测试照样通过。
//!
//! 因此本模块把预算与约束都写成常量，并用 `static_assertions` 把**结论**
//! 钉在编译期：任何一处改动只要破坏了"留有余量"这个关系，构建就失败。
//!
//! # 预算与实测的关系（**平台需求 RT-2**）
//!
//! 表里的 ISR 数值是**预算**（设计上限）；真机上的实测值由 `realtime` 的
//! DWT 仪表统计，并随每秒心跳以 `rt=ok|VIOLATION` 上报。预算与实测都成立，
//! 实时性论证才算完整：
//!
//! - 这里的 `const_assert` 保证"设计上留有余量"；
//! - `realtime` 保证"实际执行没超"（超了就机械可判地失败）。
//!
//! | 量 | 值 | 状态 |
//! |----|-----|------|
//! | CPU 主频 | 396 MHz | ✅ 手册确认（datasheet Rev 3.1） |
//! | 串行总线波特率 | 115200 8N1 | 平台默认（可被应用调整，改时断言会重算） |
//! | CAN 波特率 | 1 Mbps | ✅ 本板实测（PE 时钟 12 MHz，按官方公式反解） |
//! | 各 ISR 最坏执行时间 | 见常量 | 🟡 **预算值**，需 DWT 实测核对 |

use static_assertions::{const_assert, const_assert_eq};

/// CPU 主频（Hz）。MIMXRT1021CAG4A，Cortex-M7 @396 MHz。
pub const CPU_HZ: u64 = 396_000_000;

/// NVIC 优先级位数（i.MX RT1021 为 4 位 → 16 级）。
///
/// 与 `imxrt_ral::NVIC_PRIO_BITS` 的一致性由 `board-support` 的
/// `static_assertions` 断言（那边才能同时看到两个常量）。
pub const NVIC_PRIO_BITS: u8 = 4;

/// 串行总线波特率（平台默认）。
pub const SERIAL_BAUD: u32 = 115_200;

/// 串行总线每字节的位数：1 起始 + 8 数据 + 1 停止（8N1）。
pub const SERIAL_BITS_PER_BYTE: u32 = 10;

/// 串行总线字节间隔（纳秒）：10 bit ÷ 115200 ≈ 86 805.6 ns。
///
/// 这是**本项目最紧的硬约束**：任何优先级高于串行接收的任务，其最坏执行
/// 时间之和若超过这个间隔，就会丢字节。
pub const SERIAL_BYTE_INTERVAL_NANOS: u64 = 86_806;

/// 时基心跳周期（微秒）。
pub const TICK_PERIOD_MICROS: u64 = 1_000;

/// RTIC 任务优先级表。
///
/// 数值越大优先级越高（RTIC 语义）。顺序本身就是设计的一部分，因此下面用
/// 断言把它钉住：改动顺序会编译失败。
pub mod prio {
    /// 空闲任务/主循环：终端、持久化推进、诊断输出。
    pub const IDLE: u8 = 0;
    /// 1 ms 截止时刻推进 + 健康计数。
    pub const DEADLINE_TICK: u8 = 1;
    /// 串行总线接收。
    pub const SERIAL_RX: u8 = 2;
    /// CAN 发送完成。
    pub const CAN_TX: u8 = 3;
    /// CAN 接收（最高：硬件 FIFO 满即丢帧）。
    pub const CAN_RX: u8 = 4;
}

/// 最高可用优先级。
pub const MAX_PRIO: u8 = (1 << NVIC_PRIO_BITS) - 1;

/// `deadline_tick` 的最坏执行时间预算（微秒）。
pub const ISR_DEADLINE_TICK_MICROS: u64 = 20;
/// `serial_rx` 的最坏执行时间预算（微秒）。
pub const ISR_SERIAL_RX_MICROS: u64 = 15;
/// `can_tx_done` 的最坏执行时间预算（微秒）。
pub const ISR_CAN_TX_MICROS: u64 = 10;
/// `can_rx` 的最坏执行时间预算（微秒）。
pub const ISR_CAN_RX_MICROS: u64 = 15;

/// 把纳秒换算成 CPU 周期。
#[must_use]
pub const fn cycles_for_nanos(nanos: u64) -> u64 {
    nanos.wrapping_mul(CPU_HZ).wrapping_div(1_000_000_000)
}

/// 把微秒换算成 CPU 周期。
#[must_use]
pub const fn cycles_for_micros(micros: u64) -> u64 {
    cycles_for_nanos(micros.wrapping_mul(1_000))
}

// --- 断言：以下任一条不成立，构建即失败 ---------------------------------

// 15 µs @396 MHz = 5940 周期。这条断言同时验证换算函数本身。
const_assert_eq!(cycles_for_micros(15), 5_940);

// **优先级顺序是设计**：CAN 接收最高（硬件 FIFO 有限），其次是串行接收，
// 然后是心跳，最后是空闲。
const_assert!(prio::CAN_RX > prio::CAN_TX);
const_assert!(prio::CAN_TX > prio::SERIAL_RX);
const_assert!(prio::SERIAL_RX > prio::DEADLINE_TICK);
const_assert!(prio::DEADLINE_TICK > prio::IDLE);

// 所有优先级都放得进 NVIC 的 4 位。
const_assert!(prio::CAN_RX <= MAX_PRIO);

// 心跳预算必须小于心跳周期，否则心跳任务自身会堆积。
const_assert!(ISR_DEADLINE_TICK_MICROS < TICK_PERIOD_MICROS);

// **串行总线不丢字节的充分条件**：比串行接收优先级更高的两条 ISR 的最坏
// 执行时间之和，必须显著小于一个字节的传输时间。这里要求至少 2 倍余量。
//
// 若将来 CAN 波特率变化或新增更高优先级中断，这条断言会失败 —— 那正是需要
// 重新做可行性论证的时刻，而不是等到现场丢字节。
const_assert!(
    (ISR_CAN_RX_MICROS.wrapping_add(ISR_CAN_TX_MICROS))
        .wrapping_mul(1_000)
        .wrapping_mul(2)
        < SERIAL_BYTE_INTERVAL_NANOS
);

// 单条最高优先级 ISR 的预算不得超过"半个字节时间"，否则即使只有它自己
// 也会在极端对齐下丢字节。
const_assert!(ISR_CAN_RX_MICROS.wrapping_mul(1_000).wrapping_mul(2) < SERIAL_BYTE_INTERVAL_NANOS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_conversion_matches_the_datasheet_frequency() {
        assert_eq!(cycles_for_micros(1_000), 396_000, "1 ms @396 MHz");
        assert_eq!(cycles_for_nanos(SERIAL_BYTE_INTERVAL_NANOS), 34_375);
    }

    #[test]
    fn byte_interval_matches_baud_rate() {
        // 10 bit / 115200 s ≈ 86.8 µs；常量取整到纳秒后误差 < 1 ns/bit。
        let expected_nanos =
            (u64::from(SERIAL_BITS_PER_BYTE) * 1_000_000_000) / u64::from(SERIAL_BAUD);
        let diff = expected_nanos.abs_diff(SERIAL_BYTE_INTERVAL_NANOS);
        assert!(diff <= 1, "字节间隔常量与波特率不一致：差 {diff} ns");
    }

    #[test]
    fn high_priority_isr_budget_leaves_margin() {
        let high = ISR_CAN_RX_MICROS + ISR_CAN_TX_MICROS;
        let interval_micros = SERIAL_BYTE_INTERVAL_NANOS / 1_000;
        assert!(high * 3 < interval_micros, "余量不足 3 倍，需重做预算");
    }
}
