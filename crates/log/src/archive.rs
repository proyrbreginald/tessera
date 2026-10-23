//! 归档窗口的 generation 索引（纯逻辑，主机可测）。
//!
//! # 为什么需要它
//!
//! 旧实现用三个固定文件名（`archive.gz` / `archive.1.gz` / `archive.2.gz`）滚动：
//! 每次轮转都要把 `archive.1 → archive.2`、`archive.gz → archive.1` 整文件复制一遍。
//! 一次 8 KiB 的活动日志轮转，实际写放大到约 20 KiB 以上；归档越大，放大越严重。
//!
//! 新实现让**每个归档窗口有自己的 generation 文件名**：
//! `/log/archive.<8 位十六进制>.gz`。轮转只写一个新文件，然后删除最旧的一个窗口，
//! **不再复制任何归档字节**。窗口的先后顺序由 generation 决定，而不是由文件名
//! 的复制顺序决定。
//!
//! 一个小索引（本模块的 [`Index`]）持久化"下一个 generation 是多少、当前保留哪些窗口、
//! 每个窗口的压缩/源字节数与 CRC-32"。索引只是**缓存**：真正的窗口仍以文件为准，
//! 启动时可以用目录扫描把索引重建出来，因此索引丢失不会导致归档丢失。
//!
//! # 为什么索引用固定 64 字节
//!
//! - 一次 `read_range` 的上限是 64 字节，索引正好一次读完；
//! - 3 个窗口 × 16 字节 + 12 字节头 + 4 字节 CRC = 64 字节；
//! - 小到可以直接投递进 192 字节的写队列，不需要分块。
//!
//! # 索引格式（全部小端）
//!
//! ```text
//! 0..4   magic = "LIDX"
//! 4      version = 1
//! 5      count（0..=3）
//! 6..8   保留（必须为 0）
//! 8..12  next_generation（下一个要分配的 generation，必须 > 最后一个窗口）
//! 12..   最多 3 条窗口，每条 16 字节：
//!        generation u32 | gzip_bytes u32 | source_bytes u32 | crc32 u32
//! 60..64 前面 60 字节的 CRC-32
//! ```
//!
//! `crc32` 是**归档文件（压缩后字节）**的 CRC-32，不是解压后内容的 CRC。
//! 这样校验时不需要先解压就能判断"文件是不是索引里记录的那一份"；解压后的
//! gzip 尾部 CRC 由 [`gzip::decompress`] 自己校验。

use primitives::crc::{CRC32_INIT, crc32, crc32_update};

/// 最多保留的归档窗口数。
pub const MAX_WINDOWS: usize = 3;

/// 归档路径的固定字节数：`/log/archive.` + 8 位十六进制 + `.gz`。
pub const PATH_LEN: usize = 24;

/// 持久化索引的固定字节数。
pub const INDEX_LEN: usize = 64;

const MAGIC: [u8; 4] = *b"LIDX";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 12;
const ENTRY_LEN: usize = 16;
const INDEX_CRC_AT: usize = INDEX_LEN - 4;

/// 一个归档窗口的元数据。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Window {
    /// 窗口代数；0 是非法值（0 留给"没有窗口"）。
    pub generation: u32,
    /// 压缩后的文件字节数。
    pub gzip_bytes: u32,
    /// 压缩前的源字节数；从旧版归档迁移来的窗口未知时为 0。
    pub source_bytes: u32,
    /// 压缩后文件字节的 CRC-32。
    pub crc32: u32,
}

impl Window {
    /// 构造一个窗口。合法性由 [`Index::push`] / [`Index::from_windows`] 检查。
    #[must_use]
    pub const fn new(generation: u32, gzip_bytes: u32, source_bytes: u32, crc32: u32) -> Self {
        Self {
            generation,
            gzip_bytes,
            source_bytes,
            crc32,
        }
    }
}

/// 窗口操作的错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveError {
    /// generation 为 0、大小为 0，或 generation 没有严格递增。
    BadWindow,
}

/// generation 窗口索引。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Index {
    next_generation: u32,
    count: u8,
    windows: [Window; MAX_WINDOWS],
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// 空索引，下一个 generation 从 1 开始。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_generation: 1,
            count: 0,
            windows: [Window::new(0, 0, 0, 0); MAX_WINDOWS],
        }
    }

    /// 下一个要分配的 generation。
    #[must_use]
    pub const fn next_generation(&self) -> u32 {
        self.next_generation
    }

    /// 当前窗口数。
    #[must_use]
    pub fn count(&self) -> usize {
        usize::from(self.count)
    }

    /// 是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 按 generation 从旧到新排列的窗口。
    #[must_use]
    pub fn windows(&self) -> &[Window] {
        self.windows.get(..usize::from(self.count)).unwrap_or(&[])
    }

    /// 是否包含某个 generation。
    #[must_use]
    pub fn contains(&self, generation: u32) -> bool {
        self.windows()
            .iter()
            .any(|window| window.generation == generation)
    }

    /// 追加一个窗口；满了就丢最旧并把它返回给调用方删除。
    ///
    /// # Errors
    ///
    /// generation 为 0、大小为 0、或没有严格大于当前最后一个窗口。
    pub fn push(&mut self, window: Window) -> Result<Option<Window>, ArchiveError> {
        if window.generation == 0 || window.gzip_bytes == 0 {
            return Err(ArchiveError::BadWindow);
        }
        if let Some(last) = self.windows().last()
            && window.generation <= last.generation
        {
            return Err(ArchiveError::BadWindow);
        }
        let mut evicted = None;
        if usize::from(self.count) < MAX_WINDOWS {
            let at = usize::from(self.count);
            if let Some(slot) = self.windows.get_mut(at) {
                *slot = window;
            }
            self.count = self.count.saturating_add(1);
        } else {
            evicted = self.windows.first().copied();
            for at in 1..MAX_WINDOWS {
                if let Some(value) = self.windows.get(at).copied()
                    && let Some(slot) = self.windows.get_mut(at - 1)
                {
                    *slot = value;
                }
            }
            if let Some(slot) = self.windows.get_mut(MAX_WINDOWS - 1) {
                *slot = window;
            }
        }
        self.next_generation = self
            .next_generation
            .max(window.generation.saturating_add(1));
        Ok(evicted)
    }

    /// 从"旧到新"的窗口列表构造索引；`next_generation` 必须大于最后一个窗口。
    ///
    /// 返回 `None` 表示列表非法（超过容量、generation 不递增、或 next 不大于最后一项）。
    #[must_use]
    pub fn from_windows(next_generation: u32, windows: &[Window]) -> Option<Self> {
        if windows.len() > MAX_WINDOWS || next_generation == 0 {
            return None;
        }
        let mut index = Self::new();
        for window in windows {
            index.push(*window).ok()?;
        }
        if let Some(last) = index.windows().last()
            && next_generation <= last.generation
        {
            return None;
        }
        index.next_generation = next_generation;
        Some(index)
    }

    /// 选最新的 `newest` 个窗口（仍按从旧到新返回）。
    ///
    /// `newest == 0` 返回空；`newest` 超过窗口数时返回全部。
    #[must_use]
    pub fn select_newest(&self, newest: usize) -> &[Window] {
        if newest == 0 {
            return &[];
        }
        let count = usize::from(self.count);
        let take = newest.min(count);
        let start = count - take;
        self.windows.get(start..count).unwrap_or(&[])
    }

    /// 校验索引自身的一致性。
    #[must_use]
    pub fn validate(&self) -> bool {
        if usize::from(self.count) > MAX_WINDOWS || self.next_generation == 0 {
            return false;
        }
        let mut previous = 0u32;
        for window in self.windows() {
            if window.generation == 0 || window.generation <= previous || window.gzip_bytes == 0 {
                return false;
            }
            previous = window.generation;
        }
        if let Some(last) = self.windows().last() {
            self.next_generation > last.generation
        } else {
            true
        }
    }

    /// 编码成固定 64 字节；缓冲区不足或索引非法时返回 `None`。
    #[must_use]
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if !self.validate() {
            return None;
        }
        let bytes = out.get_mut(..INDEX_LEN)?;
        bytes.fill(0);
        bytes.get_mut(0..4)?.copy_from_slice(&MAGIC);
        *bytes.get_mut(4)? = VERSION;
        *bytes.get_mut(5)? = self.count;
        put_u32(bytes, 8, self.next_generation)?;
        for (at, window) in self.windows().iter().enumerate() {
            let base = HEADER_LEN.checked_add(at.checked_mul(ENTRY_LEN)?)?;
            put_u32(bytes, base, window.generation)?;
            put_u32(bytes, base.checked_add(4)?, window.gzip_bytes)?;
            put_u32(bytes, base.checked_add(8)?, window.source_bytes)?;
            put_u32(bytes, base.checked_add(12)?, window.crc32)?;
        }
        let checksum = crc32(bytes.get(..INDEX_CRC_AT)?);
        put_u32(bytes, INDEX_CRC_AT, checksum)?;
        Some(INDEX_LEN)
    }

    /// 解码固定 64 字节；魔数、版本、CRC 或字段一致性任一不满足即返回 `None`。
    #[must_use]
    pub fn decode(input: &[u8]) -> Option<Self> {
        let bytes = input.get(..INDEX_LEN)?;
        if bytes.get(0..4)? != MAGIC.as_slice() {
            return None;
        }
        if *bytes.get(4)? != VERSION || *bytes.get(6)? != 0 || *bytes.get(7)? != 0 {
            return None;
        }
        let expected = read_u32(bytes, INDEX_CRC_AT)?;
        if crc32(bytes.get(..INDEX_CRC_AT)?) != expected {
            return None;
        }
        let count = usize::from(*bytes.get(5)?);
        if count > MAX_WINDOWS {
            return None;
        }
        let next_generation = read_u32(bytes, 8)?;
        if next_generation == 0 {
            return None;
        }
        let mut index = Self::new();
        index.count = u8::try_from(count).ok()?;
        for at in 0..count {
            let base = HEADER_LEN.checked_add(at.checked_mul(ENTRY_LEN)?)?;
            let window = Window::new(
                read_u32(bytes, base)?,
                read_u32(bytes, base.checked_add(4)?)?,
                read_u32(bytes, base.checked_add(8)?)?,
                read_u32(bytes, base.checked_add(12)?)?,
            );
            if let Some(slot) = index.windows.get_mut(at) {
                *slot = window;
            }
        }
        index.next_generation = next_generation;
        if !index.validate() {
            return None;
        }
        Some(index)
    }
}

/// 把 generation 格式化成固定长度的绝对路径。
///
/// 返回的 `&str` 借用自 `out`；`out` 必须是 [`PATH_LEN`] 字节。
#[must_use]
pub fn format_path(generation: u32, out: &mut [u8; PATH_LEN]) -> Option<&str> {
    if generation == 0 {
        return None;
    }
    out.fill(0);
    let prefix = b"/log/archive.";
    out.get_mut(..prefix.len())?.copy_from_slice(prefix);
    for at in 0..8 {
        let shift = (7 - at) * 4;
        let nibble = u8::try_from((generation >> shift) & 0x0F).ok()?;
        let digit = match nibble {
            0..=9 => b'0'.saturating_add(nibble),
            _ => b'a'.saturating_add(nibble.saturating_sub(10)),
        };
        *out.get_mut(prefix.len().checked_add(at)?)? = digit;
    }
    let suffix = b".gz";
    let suffix_at = PATH_LEN.checked_sub(suffix.len())?;
    out.get_mut(suffix_at..)?.copy_from_slice(suffix);
    core::str::from_utf8(out).ok()
}

/// 从绝对路径或目录项 basename 解析 generation。
///
/// 只接受 `/log/archive.<8 位十六进制>.gz` 与 `archive.<8 位十六进制>.gz`；
/// 旧版 `archive.gz` / `archive.1.gz` 不会被误认成 generation（它们是迁移输入）。
#[must_use]
pub fn parse_path(name: &str) -> Option<u32> {
    let name = name.strip_prefix("/log/").unwrap_or(name);
    let rest = name.strip_prefix("archive.")?;
    let digits = rest.strip_suffix(".gz")?;
    if digits.len() != 8 {
        return None;
    }
    let mut value = 0u32;
    for byte in digits.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    if value == 0 { None } else { Some(value) }
}

/// 计算压缩后文件的 CRC-32（索引用这个口径）。
#[must_use]
pub fn file_crc32(bytes: &[u8]) -> u32 {
    crc32(bytes)
}

/// 流式 CRC-32 的初值（供调用方把解压后的多个输出块拼起来校验）。
#[must_use]
pub const fn stream_crc_init() -> u32 {
    CRC32_INIT
}

/// 流式 CRC-32 的更新入口。
#[must_use]
pub fn stream_crc_update(crc: u32, bytes: &[u8]) -> u32 {
    crc32_update(crc, bytes)
}

fn put_u32(out: &mut [u8], at: usize, value: u32) -> Option<()> {
    let end = at.checked_add(4)?;
    out.get_mut(at..end)?.copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn read_u32(input: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let bytes = input.get(at..end)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows() -> [Window; 3] {
        [
            Window::new(1, 100, 200, 0x1111_1111),
            Window::new(2, 300, 600, 0x2222_2222),
            Window::new(3, 500, 1000, 0x3333_3333),
        ]
    }

    #[test]
    fn 索引往返保持全部字段() {
        let index = Index::from_windows(4, &windows()).expect("合法窗口");
        assert_eq!(index.count(), 3);
        assert_eq!(index.next_generation(), 4);
        let mut buffer = [0u8; INDEX_LEN];
        let len = index.encode(&mut buffer).expect("编码");
        assert_eq!(len, INDEX_LEN);
        assert_eq!(Index::decode(&buffer), Some(index));
    }

    #[test]
    fn 空索引也能往返() {
        let index = Index::new();
        let mut buffer = [0u8; INDEX_LEN];
        index.encode(&mut buffer).expect("编码");
        assert_eq!(Index::decode(&buffer), Some(index));
    }

    #[test]
    fn 索引拒绝坏魔数版本和_crc() {
        let index = Index::new();
        let mut buffer = [0u8; INDEX_LEN];
        index.encode(&mut buffer).expect("编码");
        buffer[0] = b'X';
        assert_eq!(Index::decode(&buffer), None);

        let mut buffer = [0u8; INDEX_LEN];
        index.encode(&mut buffer).expect("编码");
        buffer[4] = 9;
        assert_eq!(Index::decode(&buffer), None);

        let mut buffer = [0u8; INDEX_LEN];
        index.encode(&mut buffer).expect("编码");
        buffer[20] ^= 0x5A;
        assert_eq!(Index::decode(&buffer), None);
    }

    #[test]
    fn 索引校验窗口顺序与_下界() {
        let bad = [Window::new(2, 100, 200, 0), Window::new(1, 100, 200, 0)];
        assert_eq!(Index::from_windows(3, &bad), None);
        let duplicate = [Window::new(1, 100, 200, 0), Window::new(1, 100, 200, 0)];
        assert_eq!(Index::from_windows(2, &duplicate), None);
        assert_eq!(Index::from_windows(3, &[Window::new(3, 100, 200, 0)]), None);
        assert_eq!(Index::from_windows(1, &[]), Some(Index::new()));
    }

    #[test]
    fn 窗口满时丢最旧并返回它() {
        let mut index = Index::new();
        for window in windows().iter() {
            assert_eq!(index.push(*window).expect("追加"), None);
        }
        let newer = Window::new(4, 700, 1400, 0x4444_4444);
        let evicted = index.push(newer).expect("追加");
        assert_eq!(evicted, Some(windows()[0]));
        assert_eq!(index.next_generation(), 5);
        assert_eq!(
            index.windows(),
            &[
                windows()[1],
                windows()[2],
                Window::new(4, 700, 1400, 0x4444_4444)
            ]
        );
        assert_eq!(index.select_newest(3)[2].generation, 4);
        assert!(index.contains(4));
        assert!(!index.contains(1));
    }

    #[test]
    fn 选择最新窗口按从旧到新返回() {
        let index = Index::from_windows(4, &windows()).expect("合法窗口");
        assert_eq!(index.select_newest(0), &[]);
        assert_eq!(index.select_newest(1), &windows()[2..]);
        assert_eq!(index.select_newest(2), &windows()[1..]);
        assert_eq!(index.select_newest(9), &windows()[..]);
    }

    #[test]
    fn 路径格式与解析互为逆() {
        for generation in [1u32, 2, 0xABCD_EF01, u32::MAX] {
            let mut path = [0u8; PATH_LEN];
            let text = format_path(generation, &mut path).expect("格式化");
            assert_eq!(text.len(), PATH_LEN);
            assert!(text.starts_with("/log/archive."));
            assert!(text.ends_with(".gz"));
            assert_eq!(parse_path(text), Some(generation));
            assert_eq!(
                parse_path(text.trim_start_matches("/log/")),
                Some(generation)
            );
        }
    }

    #[test]
    fn 路径拒绝旧窗口名与垃圾输入() {
        assert_eq!(format_path(0, &mut [0u8; PATH_LEN]), None);
        assert_eq!(parse_path("/log/archive.gz"), None);
        assert_eq!(parse_path("/log/archive.1.gz"), None);
        assert_eq!(parse_path("/log/archive.0000000.gz"), None);
        assert_eq!(parse_path("/log/archive.00000000.gz"), None);
        assert_eq!(parse_path("/log/archive.0000000g.gz"), None);
        assert_eq!(parse_path("/log/other.00000001.gz"), None);
    }

    #[test]
    fn 文件_crc_与流式_crc_口径一致() {
        let payload = b"archive payload";
        assert_eq!(file_crc32(payload), crc32(payload));
        let split = payload.len() / 2;
        let mut stream = stream_crc_init();
        stream = stream_crc_update(stream, &payload[..split]);
        stream = stream_crc_update(stream, &payload[split..]);
        assert_eq!(primitives::crc::crc32_finish(stream), file_crc32(payload));
    }
}
