//! 文件系统 v3 的**块头编解码**（ADR-0009）。
//!
//! 与 v2 的块头保持 16 字节几何，但 `format_version` 固定为 2；v3 挂载遇到 v1
//! 块头必须判无效，避免把旧记录按新格式解释。

use primitives::crc::{INIT as CRC16_INIT, crc16_update};

/// v3 块头魔数 `"ELFS"`。
pub const VOLUME_BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"ELFS");
/// v3 块头版式版本。
pub const VOLUME_BLOCK_FORMAT_VERSION: u8 = 2;
/// v3 块头长度。
pub const VOLUME_BLOCK_HEADER_LEN: usize = 16;

/// 块状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeBlockState {
    /// 可继续追加。
    Open,
    /// 已封。
    Frozen,
}

/// v3 块头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeBlockHeader {
    /// 块序号。
    pub seq: u32,
    /// 块状态。
    pub state: VolumeBlockState,
    /// 擦除次数。
    pub erase_count: u32,
}

/// 编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockHeaderError {
    /// 输入长度不足。
    BufferTooSmall,
    /// 魔数/版式/结构非法。
    Corrupt,
    /// CRC 不匹配。
    BadCrc,
}

impl VolumeBlockHeader {
    /// 编码为 16 字节。
    #[must_use]
    pub fn encode(self) -> [u8; VOLUME_BLOCK_HEADER_LEN] {
        let mut raw = [0xFFu8; VOLUME_BLOCK_HEADER_LEN];
        raw[0..4].copy_from_slice(&VOLUME_BLOCK_MAGIC.to_le_bytes());
        raw[4..8].copy_from_slice(&self.seq.to_le_bytes());
        raw[8] = match self.state {
            VolumeBlockState::Open => 0xFF,
            VolumeBlockState::Frozen => 0x00,
        };
        raw[9] = VOLUME_BLOCK_FORMAT_VERSION;
        raw[10..14].copy_from_slice(&self.erase_count.to_le_bytes());
        let crc = crc16_update(
            crc16_update(CRC16_INIT, raw.get(0..8).unwrap_or(&[])),
            raw.get(9..14).unwrap_or(&[]),
        );
        raw[14..16].copy_from_slice(&crc.to_le_bytes());
        raw
    }

    /// 从 16 字节解码并校验魔数、版式与 CRC。
    pub fn decode(raw: &[u8]) -> Result<Self, BlockHeaderError> {
        if raw.len() < VOLUME_BLOCK_HEADER_LEN {
            return Err(BlockHeaderError::BufferTooSmall);
        }
        if u32::from_le_bytes(
            raw.get(0..4)
                .ok_or(BlockHeaderError::Corrupt)?
                .try_into()
                .map_err(|_| BlockHeaderError::Corrupt)?,
        ) != VOLUME_BLOCK_MAGIC
        {
            return Err(BlockHeaderError::Corrupt);
        }
        if *raw.get(9).ok_or(BlockHeaderError::Corrupt)? != VOLUME_BLOCK_FORMAT_VERSION {
            return Err(BlockHeaderError::Corrupt);
        }
        let state = match *raw.get(8).ok_or(BlockHeaderError::Corrupt)? {
            0xFF => VolumeBlockState::Open,
            _ => VolumeBlockState::Frozen,
        };
        let seq = u32::from_le_bytes(
            raw.get(4..8)
                .ok_or(BlockHeaderError::Corrupt)?
                .try_into()
                .map_err(|_| BlockHeaderError::Corrupt)?,
        );
        let erase_count = u32::from_le_bytes(
            raw.get(10..14)
                .ok_or(BlockHeaderError::Corrupt)?
                .try_into()
                .map_err(|_| BlockHeaderError::Corrupt)?,
        );
        let stored = u16::from_le_bytes(
            raw.get(14..16)
                .ok_or(BlockHeaderError::Corrupt)?
                .try_into()
                .map_err(|_| BlockHeaderError::Corrupt)?,
        );
        let crc = crc16_update(
            crc16_update(CRC16_INIT, raw.get(0..8).ok_or(BlockHeaderError::Corrupt)?),
            raw.get(9..14).ok_or(BlockHeaderError::Corrupt)?,
        );
        if crc != stored {
            return Err(BlockHeaderError::BadCrc);
        }
        Ok(Self {
            seq,
            state,
            erase_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_块头往返() {
        let header = VolumeBlockHeader {
            seq: 7,
            state: VolumeBlockState::Open,
            erase_count: 3,
        };
        assert_eq!(VolumeBlockHeader::decode(&header.encode()), Ok(header));
    }

    #[test]
    fn v1_块头被拒绝() {
        let header = VolumeBlockHeader {
            seq: 7,
            state: VolumeBlockState::Open,
            erase_count: 3,
        };
        let mut raw = header.encode();
        raw[9] = 1;
        assert_eq!(
            VolumeBlockHeader::decode(&raw),
            Err(BlockHeaderError::Corrupt)
        );
    }

    #[test]
    fn 坏_crc_被拒绝() {
        let header = VolumeBlockHeader {
            seq: 7,
            state: VolumeBlockState::Open,
            erase_count: 3,
        };
        let mut raw = header.encode();
        if let Some(byte) = raw.get_mut(0) {
            *byte ^= 0xFF;
        }
        assert_eq!(
            VolumeBlockHeader::decode(&raw),
            Err(BlockHeaderError::Corrupt)
        );
    }
    #[test]
    fn 块状态与短输入() {
        let frozen = VolumeBlockHeader {
            seq: 1,
            state: VolumeBlockState::Frozen,
            erase_count: 2,
        };
        assert_eq!(VolumeBlockHeader::decode(&frozen.encode()), Ok(frozen));
        assert_eq!(
            VolumeBlockHeader::decode(&[]),
            Err(BlockHeaderError::BufferTooSmall)
        );
    }
}
