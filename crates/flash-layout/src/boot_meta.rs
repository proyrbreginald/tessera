//! A/B 启动元数据的纯逻辑编解码（P4 引导程序的第一个可测部分）。
//!
//! 元数据区是双份 + 单调序号：引导程序在**没有文件系统**的情况下必须能选出
//! 最近一次完整写入的一份。这里只定义字节格式与选择规则，不碰 Flash。
#![allow(clippy::indexing_slicing)]

/// 元数据魔数 `"BTM2"`。
pub const BOOT_META_MAGIC: u32 = u32::from_le_bytes(*b"BTM2");
/// 当前版式。
pub const BOOT_META_VERSION: u8 = 2;
/// 单份编码长度。
pub const BOOT_META_LEN: usize = 24;

/// 活动槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootSlot {
    /// 运行槽。
    Run,
    /// 备份槽。
    Backup,
}

/// 一份启动元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootMeta {
    /// 单调序号（越大越新）。
    pub seq: u32,
    /// 当前活动槽。
    pub active: BootSlot,
    /// 映像长度。
    pub image_len: u32,
    /// 映像 CRC-32。
    pub image_crc: u32,
    /// 该映像是否已被确认（防止试启回滚后无限重试）。
    pub confirmed: bool,
}

/// 编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMetaError {
    /// 输入不足。
    BufferTooSmall,
    /// 魔数/版式/字段非法。
    Corrupt,
    /// CRC 不匹配。
    BadCrc,
}

impl BootMeta {
    /// 编码为 24 字节。
    #[must_use]
    pub fn encode(self) -> [u8; BOOT_META_LEN] {
        let mut raw = [0u8; BOOT_META_LEN];
        raw[0..4].copy_from_slice(&BOOT_META_MAGIC.to_le_bytes());
        raw[4] = BOOT_META_VERSION;
        raw[5] = match self.active {
            BootSlot::Run => 0,
            BootSlot::Backup => 1,
        };
        raw[6] = u8::from(self.confirmed);
        raw[8..12].copy_from_slice(&self.seq.to_le_bytes());
        raw[12..16].copy_from_slice(&self.image_len.to_le_bytes());
        raw[16..20].copy_from_slice(&self.image_crc.to_le_bytes());
        let crc = crc32(raw.get(..20).unwrap_or(&[]));
        raw[20..24].copy_from_slice(&crc.to_le_bytes());
        raw
    }

    /// 解码并校验。
    ///
    /// # Errors
    ///
    /// 结构/版式/CRC 非法。
    pub fn decode(raw: &[u8]) -> Result<Self, BootMetaError> {
        if raw.len() < BOOT_META_LEN {
            return Err(BootMetaError::BufferTooSmall);
        }
        if u32::from_le_bytes(
            raw.get(0..4)
                .ok_or(BootMetaError::Corrupt)?
                .try_into()
                .map_err(|_| BootMetaError::Corrupt)?,
        ) != BOOT_META_MAGIC
            || *raw.get(4).ok_or(BootMetaError::Corrupt)? != BOOT_META_VERSION
        {
            return Err(BootMetaError::Corrupt);
        }
        let active = match *raw.get(5).ok_or(BootMetaError::Corrupt)? {
            0 => BootSlot::Run,
            1 => BootSlot::Backup,
            _ => return Err(BootMetaError::Corrupt),
        };
        let confirmed = *raw.get(6).ok_or(BootMetaError::Corrupt)? != 0;
        let seq = u32::from_le_bytes(
            raw.get(8..12)
                .ok_or(BootMetaError::Corrupt)?
                .try_into()
                .map_err(|_| BootMetaError::Corrupt)?,
        );
        let image_len = u32::from_le_bytes(
            raw.get(12..16)
                .ok_or(BootMetaError::Corrupt)?
                .try_into()
                .map_err(|_| BootMetaError::Corrupt)?,
        );
        let image_crc = u32::from_le_bytes(
            raw.get(16..20)
                .ok_or(BootMetaError::Corrupt)?
                .try_into()
                .map_err(|_| BootMetaError::Corrupt)?,
        );
        let stored = u32::from_le_bytes(
            raw.get(20..24)
                .ok_or(BootMetaError::Corrupt)?
                .try_into()
                .map_err(|_| BootMetaError::Corrupt)?,
        );
        if crc32(raw.get(..20).ok_or(BootMetaError::Corrupt)?) != stored {
            return Err(BootMetaError::BadCrc);
        }
        Ok(Self {
            seq,
            active,
            image_len,
            image_crc,
            confirmed,
        })
    }
}

/// 从两份元数据中选出有效且序号最大的一份。
#[must_use]
pub fn select(a: &[u8], b: &[u8]) -> Option<BootMeta> {
    match (BootMeta::decode(a), BootMeta::decode(b)) {
        (Ok(a), Ok(b)) => Some(if a.seq >= b.seq { a } else { b }),
        (Ok(a), Err(_)) => Some(a),
        (Err(_), Ok(b)) => Some(b),
        (Err(_), Err(_)) => None,
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        let mut bit = 0;
        while bit < 8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            bit += 1;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seq: u32) -> BootMeta {
        BootMeta {
            seq,
            active: BootSlot::Backup,
            image_len: 1234,
            image_crc: 0xAABB_CCDD,
            confirmed: true,
        }
    }

    #[test]
    fn 元数据往返() {
        assert_eq!(BootMeta::decode(&sample(7).encode()), Ok(sample(7)));
    }

    #[test]
    fn 坏_crc_被拒绝() {
        let mut raw = sample(7).encode();
        *raw.get_mut(8).unwrap() ^= 0xFF;
        assert!(matches!(BootMeta::decode(&raw), Err(BootMetaError::BadCrc)));
    }

    #[test]
    fn 选择序号最大的有效副本() {
        let a = sample(3).encode();
        let b = sample(4).encode();
        assert_eq!(select(&a, &b), Some(sample(4)));
        assert_eq!(select(&a, &[0u8; BOOT_META_LEN]), Some(sample(3)));
        assert_eq!(select(&[0u8; BOOT_META_LEN], &[0u8; BOOT_META_LEN]), None);
    }
}

/// 每个元数据 Bank 的保留大小：一个 4 KiB 扇区。
pub const META_BANK_SIZE: u32 = crate::SECTOR;
/// 双 Bank 在元数据分区内的偏移。
pub const META_BANK_OFFSETS: [u32; 2] = [0, META_BANK_SIZE];

/// 从元数据分区的原始字节中选出最新有效的一份。
///
/// `region` 必须至少覆盖两个 Bank；不足时返回 `None`。
#[must_use]
pub fn select_from_region(region: &[u8]) -> Option<BootMeta> {
    let first = usize::try_from(META_BANK_OFFSETS[0]).ok()?;
    let second = usize::try_from(META_BANK_OFFSETS[1]).ok()?;
    let size = usize::try_from(META_BANK_SIZE).ok()?;
    let a = region.get(first..first.checked_add(size)?)?;
    let b = region.get(second..second.checked_add(size)?)?;
    select(a, b)
}

/// 下一份元数据的序号；饱和而不是回绕（回绕会让旧映像看起来更新）。
#[must_use]
pub const fn next_seq(meta: &BootMeta) -> u32 {
    meta.seq.saturating_add(1)
}

#[cfg(test)]
mod bank_tests {
    use super::*;

    #[test]
    fn 从双_bank_分区选新副本() {
        let mut region = [0u8; 8192];
        let a = BootMeta {
            seq: 1,
            active: BootSlot::Run,
            image_len: 10,
            image_crc: 1,
            confirmed: true,
        }
        .encode();
        let b = BootMeta {
            seq: 2,
            active: BootSlot::Backup,
            image_len: 20,
            image_crc: 2,
            confirmed: false,
        }
        .encode();
        if let Some(slot) = region.get_mut(..BOOT_META_LEN) {
            slot.copy_from_slice(&a);
        }
        if let Some(slot) = region.get_mut(4096..4096 + BOOT_META_LEN) {
            slot.copy_from_slice(&b);
        }
        assert_eq!(select_from_region(&region).map(|meta| meta.seq), Some(2));
        assert_eq!(next_seq(&BootMeta::decode(&a).unwrap()), 2);
        assert!(select_from_region(&[]).is_none());
    }
}
