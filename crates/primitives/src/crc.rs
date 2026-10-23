//! CRC-16/MODBUS。
//!
//! 参数是标准化的，不是"我们选的"：多项式 `0x8005` 反射实现（`0xA001`）、
//! 初值 `0xFFFF`、输入/输出均反射、无最终异或。标准校验值：
//! `"123456789"` → `0x4B37`（见测试）。
//!
//! # 为什么查表而不是逐位算
//!
//! 逐位实现要 8 次移位/字节，在 ISR 预算里不可接受；256 项查表只需
//! 一次异或 + 两次查表。表的生成放在 `const` 上下文里，因此它既是编译期
//! 常量（不占 RAM），又不需要手工维护一张魔数表（避免抄错）。

/// 生成 256 项 CRC 表（编译期求值）。
///
/// 这里显式放行两条 lint，理由是它们在本函数里**不可能**造成失效：
///
/// - `indexing_slicing`：目标是定长数组 `[u16; 256]`，而 `index` 由 `while index < 256`
///   约束，下标越界在数学上不成立；
/// - `cast_possible_truncation` / `as_conversions`：`index < 256` 因此 `as u16`
///   不可能截断 —— 而这正是 `const fn` 里无法使用 `u16::try_from` 的地方。
#[allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
const fn build_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut value = index as u16;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xA001
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// 查表（`const` 求值，落在 `.rodata`）。
static TABLE: [u16; 256] = build_table();

/// CRC 初值。
pub const INIT: u16 = 0xFFFF;

/// 分块更新 CRC。
///
/// 存储层需要校验 4 KiB 的槽位，而栈只有 8 KiB —— 因此必须支持**流式**校验：
/// 每次喂 32 字节，不需要把整块内容放进内存。协议层则直接用一次性版本。
#[must_use]
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for byte in data {
        let index = usize::from((crc ^ u16::from(*byte)) & 0x00FF);
        crc = (crc >> 8) ^ TABLE.get(index).copied().unwrap_or(0);
    }
    crc
}

/// 计算 Modbus CRC-16。
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(INIT, data)
}

/// 按 Modbus 线上顺序写入 CRC（低字节在前）。
///
/// 返回写入的字节数；空间不足时返回 0（不写半个 CRC）。
pub fn write_crc(data: &[u8], out: &mut [u8]) -> usize {
    let crc = crc16(data);
    let low = u8::try_from(crc & 0x00FF).unwrap_or(0);
    let high = u8::try_from(crc >> 8).unwrap_or(0);
    if let Some(slot) = out.get_mut(..2) {
        if let Some(first) = slot.get_mut(0) {
            *first = low;
        }
        if let Some(second) = slot.get_mut(1) {
            *second = high;
        }
        return 2;
    }
    0
}

// ---------------------------------------------------------------------------
// CRC-32/ISO-HDLC（`CRC-32`，即以太网 / zlib / gzip / zip 用的那个）
// ---------------------------------------------------------------------------
//
// # 为什么它在本 crate 而不是在用户手里
//
// 它原先住在 `filesystem/src/crc32.rs`（记录级校验）。现在 **gzip 容器也要它**
// （RFC 1952 的尾部就是 CRC-32 + ISIZE）—— 两处各写一份正是本项目
// "不重复实现已验证过的东西"的反面。放到共享的纯逻辑 crate，两边都从这里取。
//
// 参数是**标准化的，不是我们选的**：多项式 `0x04C1_1DB7` 的反射形式
// `0xEDB8_8320`、初值 `0xFFFF_FFFF`、输入/输出均反射、结果异或 `0xFFFF_FFFF`。
// 标准校验向量：`"123456789"` → `0xCBF4_3926`。

/// CRC-32 初值。
pub const CRC32_INIT: u32 = 0xFFFF_FFFF;

/// CRC-32 结果异或值。
const CRC32_XOR_OUT: u32 = 0xFFFF_FFFF;

/// 生成 256 项查表（编译期求值，落在 `.rodata`）。
///
/// 逐位实现要 8 次移位/字节；查表只需一次异或 + 两次查表。表格在 `const`
/// 上下文里生成，既不占 RAM，也不需要手工维护魔数表
/// （手抄的表迟早会抄错一个数，而 CRC 表抄错的表现是"偶发校验失败"）。
#[allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::as_conversions
)]
const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xEDB8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

/// CRC-32 查表（`const` 求值）。
static CRC32_TABLE: [u32; 256] = build_crc32_table();

/// 分块更新 CRC-32。
///
/// 必须支持**流式**校验：记录可以长到 64 KiB，而固件栈很小 ——
/// 每次喂一小段，不需要把整块放进内存（"无分配"约束下的必然选择）。
#[must_use]
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for byte in data {
        let index = usize::try_from((crc ^ u32::from(*byte)) & 0x00FF).unwrap_or(0);
        crc = (crc >> 8) ^ CRC32_TABLE.get(index).copied().unwrap_or(0);
    }
    crc
}

/// CRC-32 收尾（异或输出），得到最终校验和。
#[must_use]
pub const fn crc32_finish(crc: u32) -> u32 {
    crc ^ CRC32_XOR_OUT
}

/// 一次算完整段数据的 CRC-32。
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    crc32_finish(crc32_update(CRC32_INIT, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_check_value() {
        // CRC-16/MODBUS 的标准校验向量。
        assert_eq!(crc16(b"123456789"), 0x4B37);
    }

    #[test]
    fn known_modbus_rtu_frames() {
        // 经典 Modbus 例帧（从机 0x01，读保持寄存器 0x0000 起 2 个）。
        // 期望值由**逐位参考实现**独立算出（见 `table_matches_bitwise_reference`），
        // 不是从本实现里抄来的 —— 否则测试只能证明"实现与自身一致"。
        let request = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(crc16(&request), 0x0BC4);
        let request2 = [0x11u8, 0x04, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(crc16(&request2), 0x9A62);
    }

    #[test]
    fn crc_is_written_low_byte_first() {
        let data = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x02];
        let mut out = [0u8; 2];
        assert_eq!(write_crc(&data, &mut out), 2);
        assert_eq!(out, [0xC4, 0x0B], "线上顺序是低字节在前（CRC 值 0x0BC4）");
    }

    #[test]
    fn single_bit_flip_always_changes_the_crc() {
        // CRC 的检错能力：任何单比特翻转都必须被检出。
        let base = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x02];
        let reference = crc16(&base);
        for byte_index in 0..base.len() {
            for bit in 0..8 {
                let mut mutated = base;
                if let Some(target) = mutated.get_mut(byte_index) {
                    *target ^= 1 << bit;
                }
                assert_ne!(
                    crc16(&mutated),
                    reference,
                    "字节 {byte_index} 第 {bit} 位翻转未被检出"
                );
            }
        }
    }

    #[test]
    fn table_matches_bitwise_reference() {
        // 用逐位算法作为独立参考实现，交叉验证查表结果。
        fn bitwise(data: &[u8]) -> u16 {
            let mut crc = 0xFFFFu16;
            for byte in data {
                crc ^= u16::from(*byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xA001
                    } else {
                        crc >> 1
                    };
                }
            }
            crc
        }
        let mut rng = crate::prng::Rng::new(0xC0FF_EE00);
        for len in 0..64usize {
            let mut buf = [0u8; 64];
            let slice = buf.get_mut(..len).unwrap();
            rng.fill(slice);
            assert_eq!(crc16(slice), bitwise(slice), "长度 {len} 不一致");
        }
    }

    #[test]
    fn empty_input_returns_the_initial_value() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }

    #[test]
    fn streaming_matches_one_shot_for_every_split_point() {
        let mut rng = crate::prng::Rng::new(0x51F7_0000);
        let mut data = [0u8; 96];
        rng.fill(&mut data);
        let reference = crc16(&data);
        for split in 0..=data.len() {
            let first = data.get(..split).unwrap();
            let second = data.get(split..).unwrap();
            let streamed = crc16_update(crc16_update(INIT, first), second);
            assert_eq!(streamed, reference, "在 {split} 处切分结果不一致");
        }
    }
}
