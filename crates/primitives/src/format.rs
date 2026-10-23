//! 无堆分配的输出缓冲。
//!
//! 现场诊断、串口横幅、`defmt` 的替代通道都需要"把数字与字符串拼进一个定长缓冲"。
//! 本模块提供这件事的**唯一**实现，并保证：
//!
//! - 永不 panic：空间不足时**截断并返回实际写入字节数**；
//! - 永不分配：缓冲区由调用方提供；
//! - 与 `core::fmt` 兼容：可用 `write!` 宏格式化。
//!
//! 截断而不是报错，是刻意的：诊断路径上的"格式化失败"不该再引发一次失败。
//! 但截断必须**可见**——因此写入函数都返回实际写入的字节数，调用方可以据此
//! 判断是否被截断（例如输出末尾的 `...`）。

use core::fmt;

/// 定长输出缓冲。
pub struct FixedBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> FixedBuf<'a> {
    /// 包装一个外部缓冲区。
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// 容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 已写入字节数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 剩余空间。
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.capacity().saturating_sub(self.len)
    }

    /// 已写入内容。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// 清空（不擦除内容，只把长度归零）。
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// 追加字节，返回实际追加数（空间不足时截断）。
    pub fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        let n = core::cmp::min(self.remaining(), bytes.len());
        if let (Some(dst), Some(src)) = (self.buf.get_mut(self.len..), bytes.get(..n)) {
            for (slot, byte) in dst.iter_mut().zip(src.iter()) {
                *slot = *byte;
            }
        }
        self.len = self.len.saturating_add(n);
        n
    }

    /// 追加字符串。
    pub fn push_str(&mut self, text: &str) -> usize {
        self.push_bytes(text.as_bytes())
    }

    /// 追加无符号十进制整数，返回实际写入字节数。
    pub fn write_u64(&mut self, value: u64) -> usize {
        let before = self.len;
        // FixedBuf 的 fmt::Write 永不返回 Err，因此这里的 let _ 不会吞掉错误。
        let _ = fmt::Write::write_fmt(self, format_args!("{value}"));
        self.len.saturating_sub(before)
    }

    /// 追加小写十六进制，至少 `min_digits` 位。
    pub fn write_hex_u32(&mut self, value: u32, min_digits: usize) -> usize {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut tmp = [b'0'; 8];
        let mut index = tmp.len();
        let mut rest = value;

        loop {
            index = index.saturating_sub(1);
            let nibble = usize::try_from(rest & 0xF).unwrap_or(0);
            if let Some(slot) = tmp.get_mut(index) {
                *slot = DIGITS.get(nibble).copied().unwrap_or(b'?');
            }
            rest >>= 4;
            if rest == 0 || index == 0 {
                break;
            }
        }

        // 补足最小位数。
        while tmp.len().saturating_sub(index) < min_digits && index > 0 {
            index = index.saturating_sub(1);
            if let Some(slot) = tmp.get_mut(index) {
                *slot = b'0';
            }
        }

        self.push_bytes(tmp.get(index..).unwrap_or(&[]))
    }
}

impl fmt::Write for FixedBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        // 截断不视为错误：诊断输出不应因为缓冲区小而失败。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn push_truncates_and_reports_written_bytes() {
        let mut storage = [0u8; 4];
        let mut buf = FixedBuf::new(&mut storage);
        assert_eq!(buf.push_str("abc"), 3);
        assert_eq!(buf.push_str("defg"), 1, "只应写入剩余空间");
        assert_eq!(buf.as_bytes(), b"abcd");
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.remaining(), 0);
        assert_eq!(buf.push_bytes(b"x"), 0);
    }

    #[test]
    fn integers_are_formatted() {
        let mut storage = [0u8; 32];
        let mut buf = FixedBuf::new(&mut storage);
        buf.write_u64(0);
        buf.push_str(",");
        buf.write_u64(u64::MAX);
        assert_eq!(buf.as_bytes(), b"0,18446744073709551615");
    }

    #[test]
    fn hex_pads_to_minimum_digits() {
        let mut storage = [0u8; 16];
        let mut buf = FixedBuf::new(&mut storage);
        buf.write_hex_u32(0x1F, 4);
        buf.push_str(" ");
        buf.write_hex_u32(0, 2);
        buf.push_str(" ");
        buf.write_hex_u32(0xDEAD_BEEF, 1);
        assert_eq!(buf.as_bytes(), b"001f 00 deadbeef");
    }

    #[test]
    fn fmt_write_never_reports_error_on_truncation() {
        let mut storage = [0u8; 3];
        let mut buf = FixedBuf::new(&mut storage);
        assert!(write!(buf, "{}", 123_456).is_ok());
        assert_eq!(buf.as_bytes(), b"123");
    }

    #[test]
    fn clear_resets_length_but_keeps_capacity() {
        let mut storage = [0u8; 8];
        let mut buf = FixedBuf::new(&mut storage);
        buf.push_str("hello");
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), 8);
        assert_eq!(buf.remaining(), 8);
    }

    #[test]
    fn zero_sized_buffer_is_safe() {
        let mut storage: [u8; 0] = [];
        let mut buf = FixedBuf::new(&mut storage);
        assert_eq!(buf.push_str("abc"), 0);
        assert_eq!(buf.write_u64(42), 0);
        assert!(buf.as_bytes().is_empty());
    }
}
