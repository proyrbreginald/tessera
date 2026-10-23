//! 文件系统 v3 的**记录编解码**（[ADR-0009](../../../docs/adr/0009-文件系统v3-inode目录硬链接.md) 第二层）。
//!
//! 这是纯逻辑层：把 v3 的块内记录编码成字节、从字节解码，并做 CRC/边界校验。
//! 它不碰块设备，也不决定记录放在哪个块；持久化/扫描层稍后建立在它之上。
//!
//! # 记录头（24 B）
//!
//! ```text
//! 0  crc32        u32
//! 4  flags        u8
//! 5  name_len     u8
//! 6  reserved     u16
//! 8  inode        u32
//! 12 version      u32
//! 16 chunk_index  u16
//! 18 chunk_total  u16
//! 20 data_len     u16
//! 22 reserved     u16
//! ```
//!
//! `crc32` 覆盖 `[4, 24)` + 名字 + 数据；因此长度/名字/inode 都在校验范围内。
//! 目录项记录用 `FLAG_DIRENT`，`inode` 是父目录，名字是目录项名，数据是子 inode
//! （4 B 小端）+ 子类型（1 B）。

use crate::MAX_NAME_LEN;
use primitives::crc::{CRC32_INIT, crc32_finish, crc32_update};

/// v3 记录头长度。
pub const VOLUME_RECORD_HEADER_LEN: usize = 24;

/// 记录标志。
pub const FLAG_DATA: u8 = 0b0000_0001;
/// 只追加日志。
pub const FLAG_LOG: u8 = 0b0000_0010;
/// 墓碑。
pub const FLAG_TOMBSTONE: u8 = 0b0000_0100;
/// 目录项。
pub const FLAG_DIRENT: u8 = 0b0000_1000;
/// rename 意图。
pub const FLAG_RENAME_INTENT: u8 = 0b0001_0000;
/// rename 提交。
pub const FLAG_RENAME_COMMIT: u8 = 0b0010_0000;
const FLAG_KNOWN: u8 =
    FLAG_DATA | FLAG_LOG | FLAG_TOMBSTONE | FLAG_DIRENT | FLAG_RENAME_INTENT | FLAG_RENAME_COMMIT;

/// v3 记录头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeRecordHeader {
    /// 记录 CRC-32（覆盖头 `[4,24)`、名字、数据）。
    pub crc32: u32,
    /// 标志位。
    pub flags: u8,
    /// 名字长度（目录项/兼容记录使用；0 = 无名字）。
    pub name_len: u8,
    /// 内容归属 inode；目录项记录里是父目录 inode。
    pub inode: u32,
    /// 版本号（全局单调）。
    pub version: u32,
    /// 分块序号。
    pub chunk_index: u16,
    /// 分块总数。
    pub chunk_total: u16,
    /// 数据长度。
    pub data_len: u16,
}

/// 编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCodecError {
    /// 输入/输出缓冲不足。
    BufferTooSmall,
    /// 魔数/结构字段非法（未知标志、非法名字长度等）。
    Corrupt,
    /// CRC 不匹配。
    BadCrc,
}

impl VolumeRecordHeader {
    /// 编码为 24 字节。
    #[must_use]
    pub fn encode(&self) -> [u8; VOLUME_RECORD_HEADER_LEN] {
        let mut raw = [0u8; VOLUME_RECORD_HEADER_LEN];
        raw[0..4].copy_from_slice(&self.crc32.to_le_bytes());
        raw[4] = self.flags;
        raw[5] = self.name_len;
        raw[8..12].copy_from_slice(&self.inode.to_le_bytes());
        raw[12..16].copy_from_slice(&self.version.to_le_bytes());
        raw[16..18].copy_from_slice(&self.chunk_index.to_le_bytes());
        raw[18..20].copy_from_slice(&self.chunk_total.to_le_bytes());
        raw[20..22].copy_from_slice(&self.data_len.to_le_bytes());
        raw
    }

    /// 从 24 字节解码；只做结构检查，CRC 由 [`decode_record`] 校验。
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < VOLUME_RECORD_HEADER_LEN {
            return None;
        }
        let flags = *raw.get(4)?;
        if flags == 0 || flags & !FLAG_KNOWN != 0 {
            return None;
        }
        let name_len = usize::from(*raw.get(5)?);
        if name_len > MAX_NAME_LEN {
            return None;
        }
        let inode = u32::from_le_bytes(raw.get(8..12)?.try_into().ok()?);
        if inode == 0 {
            return None;
        }
        let version = u32::from_le_bytes(raw.get(12..16)?.try_into().ok()?);
        let chunk_index = u16::from_le_bytes(raw.get(16..18)?.try_into().ok()?);
        let chunk_total = u16::from_le_bytes(raw.get(18..20)?.try_into().ok()?);
        let data_len = u16::from_le_bytes(raw.get(20..22)?.try_into().ok()?);
        if chunk_total != 0 && chunk_index >= chunk_total {
            return None;
        }
        Some(Self {
            crc32: u32::from_le_bytes(raw.get(0..4)?.try_into().ok()?),
            flags,
            name_len: u8::try_from(name_len).ok()?,
            inode,
            version,
            chunk_index,
            chunk_total,
            data_len,
        })
    }
}

/// 解码后的记录视图（借用输入缓冲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeRecordView<'a> {
    /// 记录头。
    pub header: VolumeRecordHeader,
    /// 名字字节（长度 = `header.name_len`）。
    pub name: &'a [u8],
    /// 数据字节（长度 = `header.data_len`）。
    pub data: &'a [u8],
}

/// 计算 v3 记录 CRC。
#[must_use]
pub fn record_crc(header: &VolumeRecordHeader, name: &[u8], data: &[u8]) -> u32 {
    let raw = header.encode();
    let mut crc = crc32_update(CRC32_INIT, raw.get(4..24).unwrap_or(&[]));
    crc = crc32_update(crc, name);
    crc32_finish(crc32_update(crc, data))
}

/// 编码一条 v3 记录到 `out`，返回总长度。
///
/// # Errors
///
/// 名字/数据超长或输出缓冲不足。
#[allow(clippy::too_many_arguments)] // 记录字段本来就是一组扁平编码参数，收结构体只会多一层搬移
pub fn encode_record(
    flags: u8,
    inode: u32,
    version: u32,
    chunk_index: u16,
    chunk_total: u16,
    name: &[u8],
    data: &[u8],
    out: &mut [u8],
) -> Result<usize, RecordCodecError> {
    if flags == 0 || flags & !FLAG_KNOWN != 0 || inode == 0 || name.len() > MAX_NAME_LEN {
        return Err(RecordCodecError::Corrupt);
    }
    let name_len = u8::try_from(name.len()).map_err(|_| RecordCodecError::Corrupt)?;
    let data_len = u16::try_from(data.len()).map_err(|_| RecordCodecError::Corrupt)?;
    let total = VOLUME_RECORD_HEADER_LEN
        .checked_add(name.len())
        .and_then(|value| value.checked_add(data.len()))
        .ok_or(RecordCodecError::Corrupt)?;
    if out.len() < total {
        return Err(RecordCodecError::BufferTooSmall);
    }
    let mut header = VolumeRecordHeader {
        crc32: 0,
        flags,
        name_len,
        inode,
        version,
        chunk_index,
        chunk_total,
        data_len,
    };
    header.crc32 = record_crc(&header, name, data);
    let encoded = header.encode();
    out.get_mut(..VOLUME_RECORD_HEADER_LEN)
        .ok_or(RecordCodecError::BufferTooSmall)?
        .copy_from_slice(&encoded);
    out.get_mut(VOLUME_RECORD_HEADER_LEN..VOLUME_RECORD_HEADER_LEN + name.len())
        .ok_or(RecordCodecError::BufferTooSmall)?
        .copy_from_slice(name);
    let data_at = VOLUME_RECORD_HEADER_LEN + name.len();
    out.get_mut(data_at..data_at + data.len())
        .ok_or(RecordCodecError::BufferTooSmall)?
        .copy_from_slice(data);
    Ok(total)
}

/// 解码一条完整 v3 记录（头 + 名字 + 数据）并校验 CRC。
///
/// # Errors
///
/// 输入不足、结构非法、CRC 不匹配。
pub fn decode_record(data: &[u8]) -> Result<VolumeRecordView<'_>, RecordCodecError> {
    let header = VolumeRecordHeader::decode(data).ok_or(RecordCodecError::Corrupt)?;
    let name_start = VOLUME_RECORD_HEADER_LEN;
    let name_end = name_start
        .checked_add(usize::from(header.name_len))
        .ok_or(RecordCodecError::Corrupt)?;
    let data_end = name_end
        .checked_add(usize::from(header.data_len))
        .ok_or(RecordCodecError::BufferTooSmall)?;
    let name = data
        .get(name_start..name_end)
        .ok_or(RecordCodecError::BufferTooSmall)?;
    let payload = data
        .get(name_end..data_end)
        .ok_or(RecordCodecError::BufferTooSmall)?;
    if record_crc(&header, name, payload) != header.crc32 {
        return Err(RecordCodecError::BadCrc);
    }
    Ok(VolumeRecordView {
        header,
        name,
        data: payload,
    })
}

/// 目录项数据：子 inode + 子类型。
///
/// # Errors
///
/// 数据长度不是 5 或子类型非法。
pub fn decode_dirent_data(data: &[u8]) -> Result<(u32, u8), RecordCodecError> {
    if data.len() != 5 {
        return Err(RecordCodecError::Corrupt);
    }
    let inode = u32::from_le_bytes(
        data.get(..4)
            .ok_or(RecordCodecError::Corrupt)?
            .try_into()
            .map_err(|_| RecordCodecError::Corrupt)?,
    );
    let kind = *data.get(4).ok_or(RecordCodecError::Corrupt)?;
    if kind > 2 {
        return Err(RecordCodecError::Corrupt);
    }
    Ok((inode, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 记录往返与_crc() {
        let mut buf = [0u8; 128];
        let len = encode_record(FLAG_DATA, 7, 9, 0, 1, b"file.txt", b"hello", &mut buf).unwrap();
        let record = decode_record(&buf[..len]).unwrap();
        assert_eq!(record.header.inode, 7);
        assert_eq!(record.header.flags, FLAG_DATA);
        assert_eq!(record.name, b"file.txt");
        assert_eq!(record.data, b"hello");
    }

    #[test]
    fn 目录项记录可区分父子() {
        let mut data = [0u8; 5];
        data[..4].copy_from_slice(&42u32.to_le_bytes());
        data[4] = 2;
        let mut buf = [0u8; 64];
        let len = encode_record(FLAG_DIRENT, 1, 1, 0, 1, b"child", &data, &mut buf).unwrap();
        let record = decode_record(&buf[..len]).unwrap();
        assert_eq!(record.header.inode, 1);
        assert_eq!(decode_dirent_data(record.data).unwrap(), (42, 2));
    }

    #[test]
    fn 损坏_crc_被拒绝() {
        let mut buf = [0u8; 128];
        let len = encode_record(FLAG_DATA, 7, 9, 0, 1, b"file", b"x", &mut buf).unwrap();
        if let Some(byte) = buf.get_mut(VOLUME_RECORD_HEADER_LEN + 1) {
            *byte ^= 0xFF;
        }
        assert_eq!(decode_record(&buf[..len]), Err(RecordCodecError::BadCrc));
    }

    #[test]
    fn 未知标志与坏名字长度被拒绝() {
        let mut buf = [0u8; 64];
        assert_eq!(
            encode_record(0, 1, 1, 0, 1, b"x", b"", &mut buf),
            Err(RecordCodecError::Corrupt)
        );
        assert_eq!(
            encode_record(FLAG_DATA, 1, 1, 0, 1, b"", b"", &mut buf),
            Ok(VOLUME_RECORD_HEADER_LEN)
        );
        let too_long = [b'x'; MAX_NAME_LEN + 1];
        assert_eq!(
            encode_record(FLAG_DATA, 1, 1, 0, 1, &too_long, b"", &mut buf),
            Err(RecordCodecError::Corrupt)
        );
    }
    #[test]
    fn 记录长度与目录项坏数据被拒绝() {
        let mut buf = [0u8; 64];
        assert_eq!(
            encode_record(FLAG_DATA, 1, 1, 0, 1, b"x", b"", &mut []),
            Err(RecordCodecError::BufferTooSmall)
        );
        assert_eq!(decode_record(&[]), Err(RecordCodecError::Corrupt));
        assert_eq!(decode_dirent_data(&[]), Err(RecordCodecError::Corrupt));
        let mut data = [0u8; 5];
        data[4] = 9;
        assert_eq!(decode_dirent_data(&data), Err(RecordCodecError::Corrupt));
        let _ = &mut buf;
    }
}
