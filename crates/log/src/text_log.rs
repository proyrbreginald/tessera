//! 纯文本、字节流日志环：**不按行限制、不转置、不编码**。
//!
//! 环里只存原始文本字节；满了丢**最旧的行**（不是丢半个字节）。落盘就是
//! `fs.append("/log/blackbox.log", bytes)`，读回直接 `cat`。
//!
//! 长内容（例如把文件内容写进日志）由调用方走批量路径：批量写入直接 append
//! 活动文件，不经过事件环；环只服务短事件，因此不需要"每行 128 B"这种上限。

/// 文本环的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStats {
    /// 当前字节数。
    pub bytes: usize,
    /// 因环满丢掉的**行**数。
    pub dropped_lines: u64,
    /// 因单行超过环容量而截断的字节数（只影响病态超长事件；正常走批量路径）。
    pub truncated_bytes: u64,
}

/// 线性文本环的**纯状态**：不持有缓冲区引用。
///
/// 缓冲区由调用方单独持有并作为参数传入。这样 `TextLog` 可以安全地放进静态
/// `Mutex<RefCell<Option<TextLog>>>`，不把 `&'static mut [u8]` 写进静态，
/// 避免真机上出现的 HardFault STKERR。
pub struct TextLog {
    start: usize,
    len: usize,
    dropped_lines: u64,
    truncated_bytes: u64,
}

impl Default for TextLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TextLog {
    /// 建立空环。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            start: 0,
            len: 0,
            dropped_lines: 0,
            truncated_bytes: 0,
        }
    }

    /// 当前字节数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 统计。
    #[must_use]
    pub const fn stats(&self) -> TextStats {
        TextStats {
            bytes: self.len,
            dropped_lines: self.dropped_lines,
            truncated_bytes: self.truncated_bytes,
        }
    }

    /// 丢弃最旧的一行（直到下一个 `\n`），只移动读指针，不搬缓冲区。
    fn drop_oldest_line(&mut self, buffer: &mut [u8]) {
        let cap = buffer.len();
        if cap == 0 || self.len == 0 {
            return;
        }
        let mut position = 0usize;
        while position < self.len {
            let index = (self.start + position) % cap;
            if buffer.get(index).copied() == Some(b'\n') {
                let drop = position.saturating_add(1);
                self.start = (self.start + drop) % cap;
                self.len = self.len.saturating_sub(drop);
                self.dropped_lines = self.dropped_lines.saturating_add(1);
                return;
            }
            position += 1;
        }
        // 没有完整行：整段丢弃，避免死循环。
        self.start = 0;
        self.len = 0;
    }

    /// 写一行（自动补 `\n`）。单行超过环容量时只保留尾部容量并计截断。
    pub fn push_line(&mut self, buffer: &mut [u8], line: &[u8]) {
        let cap = buffer.len();
        if cap == 0 {
            return;
        }
        let add_newline = line.last() != Some(&b'\n');
        let mut bytes = line;
        let mut needed = bytes.len().saturating_add(usize::from(add_newline));
        if needed > cap {
            let keep = cap.saturating_sub(usize::from(add_newline));
            self.truncated_bytes = self.truncated_bytes.saturating_add(
                u64::try_from(bytes.len().saturating_sub(keep)).unwrap_or(u64::MAX),
            );
            bytes = bytes.get(bytes.len().saturating_sub(keep)..).unwrap_or(&[]);
            needed = bytes.len().saturating_add(usize::from(add_newline));
            self.start = 0;
            self.len = 0;
        }
        while self.len.saturating_add(needed) > cap {
            self.drop_oldest_line(buffer);
        }
        for byte in bytes {
            let index = (self.start + self.len) % cap;
            if let Some(slot) = buffer.get_mut(index) {
                *slot = *byte;
                self.len = self.len.saturating_add(1);
            }
        }
        if add_newline {
            let index = (self.start + self.len) % cap;
            if let Some(slot) = buffer.get_mut(index) {
                *slot = b'\n';
                self.len = self.len.saturating_add(1);
            }
        }
    }

    /// 只查看队首最多 `out.len()` 字节，不改变环状态。
    pub fn peek_into(&self, buffer: &[u8], out: &mut [u8]) -> usize {
        let cap = buffer.len();
        let take = self.len.min(out.len());
        if cap == 0 {
            return 0;
        }
        for index in 0..take {
            let source = (self.start + index) % cap;
            if let (Some(byte), Some(dst)) = (buffer.get(source), out.get_mut(index)) {
                *dst = *byte;
            }
        }
        take
    }

    /// 取出最多 `out.len()` 字节（保持 FIFO），返回取出的字节数。
    pub fn drain_into(&mut self, buffer: &mut [u8], out: &mut [u8]) -> usize {
        let cap = buffer.len();
        let take = self.len.min(out.len());
        if cap == 0 {
            return 0;
        }
        for index in 0..take {
            let source = (self.start + index) % cap;
            if let (Some(byte), Some(dst)) = (buffer.get(source), out.get_mut(index)) {
                *dst = *byte;
            }
        }
        self.start = (self.start + take) % cap;
        self.len = self.len.saturating_sub(take);
        take
    }
}

#[cfg(test)]
mod tests {
    use super::TextLog;

    #[test]
    fn push_peek_and_drain_preserve_order() {
        let mut log = TextLog::new();
        let mut buffer = [0u8; 32];
        let mut out = [0u8; 32];
        log.push_line(&mut buffer, b"one");
        log.push_line(&mut buffer, b"two");
        assert_eq!(log.len(), 8);
        assert_eq!(log.peek_into(&buffer, &mut out), 8);
        assert_eq!(&out[..8], b"one\ntwo\n");
        assert_eq!(log.drain_into(&mut buffer, &mut out), 8);
        assert_eq!(&out[..8], b"one\ntwo\n");
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn full_ring_drops_oldest_lines_and_counts() {
        let mut log = TextLog::new();
        let mut buffer = [0u8; 8];
        log.push_line(&mut buffer, b"aaaa");
        log.push_line(&mut buffer, b"bbbb");
        let stats = log.stats();
        assert_eq!(stats.dropped_lines, 1);
        assert_eq!(stats.bytes, 5);
        assert_eq!(log.peek_into(&buffer, &mut [0u8; 8]), 5);
    }

    #[test]
    fn oversized_line_is_truncated_not_split() {
        let mut log = TextLog::new();
        let mut buffer = [0u8; 8];
        log.push_line(&mut buffer, b"0123456789abcdef");
        let stats = log.stats();
        assert_eq!(stats.bytes, 8);
        assert!(stats.truncated_bytes > 0);
        let mut out = [0u8; 8];
        assert_eq!(log.peek_into(&buffer, &mut out), 8);
        assert_eq!(&out, b"9abcdef\n");
    }
}
