//! 文件系统 v3 的**持久化索引快照编解码**（ADR-0009）。
//!
//! 索引只缓存命名空间：inode 表 + 目录项表。块视图/内容记录仍由卷层扫描；
//! 索引损坏时调用方回退全盘扫描。

use crate::MAX_NAME_LEN;
use crate::namespace::{InodeKind, Namespace};
use primitives::crc::{CRC32_INIT, crc32_finish, crc32_update};

/// 索引魔数 `"V3IX"`。
pub const VOLUME_INDEX_MAGIC: u32 = u32::from_le_bytes(*b"V3IX");
/// 索引版式。
pub const VOLUME_INDEX_VERSION: u8 = 1;
/// 头部长度。
pub const VOLUME_INDEX_HEADER_LEN: usize = 24;
/// 每条 inode 编码长度。
const INODE_LEN: usize = 7;
/// 每条目录项编码长度。
const ENTRY_LEN: usize = 40;

/// 索引编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexError {
    /// 输出缓冲不足。
    BufferTooSmall,
    /// 结构非法。
    Corrupt,
    /// CRC 不匹配。
    BadCrc,
    /// 容量不足。
    Full,
}

/// 编码命名空间快照，返回总长度。
///
/// # Errors
///
/// 缓冲不足或条目过多。
pub fn encode_index<const MAX_INODES: usize, const MAX_ENTRIES: usize>(
    namespace: &Namespace<MAX_INODES, MAX_ENTRIES>,
    out: &mut [u8],
) -> Result<usize, IndexError> {
    encode_index_state(namespace, 1, 0, 16, out)
}

/// 编码命名空间 + 卷挂载状态（version / current_block / cursor）。
///
/// # Errors
///
/// 缓冲不足或条目过多。
pub fn encode_index_state<const MAX_INODES: usize, const MAX_ENTRIES: usize>(
    namespace: &Namespace<MAX_INODES, MAX_ENTRIES>,
    version: u32,
    current_block: u32,
    cursor: u32,
    out: &mut [u8],
) -> Result<usize, IndexError> {
    let inode_count = namespace.inodes().count();
    let entry_count = namespace.dir_entries().count();
    let total = VOLUME_INDEX_HEADER_LEN
        .checked_add(inode_count.checked_mul(INODE_LEN).ok_or(IndexError::Full)?)
        .and_then(|value| value.checked_add(entry_count.checked_mul(ENTRY_LEN)?))
        .and_then(|value| value.checked_add(4))
        .ok_or(IndexError::Full)?;
    if out.len() < total {
        return Err(IndexError::BufferTooSmall);
    }
    out.get_mut(..4)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(&VOLUME_INDEX_MAGIC.to_le_bytes());
    *out.get_mut(4).ok_or(IndexError::BufferTooSmall)? = VOLUME_INDEX_VERSION;
    *out.get_mut(5).ok_or(IndexError::BufferTooSmall)? = 1;
    out.get_mut(6..8)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(
            &u16::try_from(inode_count)
                .map_err(|_| IndexError::Full)?
                .to_le_bytes(),
        );
    out.get_mut(8..10)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(
            &u16::try_from(entry_count)
                .map_err(|_| IndexError::Full)?
                .to_le_bytes(),
        );
    out.get_mut(10..14)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(&version.to_le_bytes());
    out.get_mut(14..18)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(&current_block.to_le_bytes());
    out.get_mut(18..22)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(&cursor.to_le_bytes());
    let mut at = VOLUME_INDEX_HEADER_LEN;
    for (inode, kind, links) in namespace.inodes() {
        out.get_mut(at..at + 4)
            .ok_or(IndexError::BufferTooSmall)?
            .copy_from_slice(&inode.to_le_bytes());
        *out.get_mut(at + 4).ok_or(IndexError::BufferTooSmall)? = kind_code(kind);
        out.get_mut(at + 5..at + 7)
            .ok_or(IndexError::BufferTooSmall)?
            .copy_from_slice(&links.to_le_bytes());
        at += INODE_LEN;
    }
    for entry in namespace.dir_entries() {
        out.get_mut(at..at + 4)
            .ok_or(IndexError::BufferTooSmall)?
            .copy_from_slice(&entry.parent.to_le_bytes());
        out.get_mut(at + 4..at + 8)
            .ok_or(IndexError::BufferTooSmall)?
            .copy_from_slice(&entry.child.to_le_bytes());
        *out.get_mut(at + 8).ok_or(IndexError::BufferTooSmall)? = entry.name_len;
        out.get_mut(at + 9..at + 9 + usize::from(entry.name_len))
            .ok_or(IndexError::BufferTooSmall)?
            .copy_from_slice(
                entry
                    .name
                    .get(..usize::from(entry.name_len))
                    .ok_or(IndexError::Corrupt)?,
            );
        at += ENTRY_LEN;
    }
    let crc = crc32_finish(crc32_update(
        CRC32_INIT,
        out.get(..at).ok_or(IndexError::BufferTooSmall)?,
    ));
    out.get_mut(at..at + 4)
        .ok_or(IndexError::BufferTooSmall)?
        .copy_from_slice(&crc.to_le_bytes());
    Ok(at + 4)
}

/// 解码命名空间快照。
///
/// # Errors
///
/// 结构/CRC/边界非法。
pub fn decode_index<const MAX_INODES: usize, const MAX_ENTRIES: usize>(
    data: &[u8],
) -> Result<Namespace<MAX_INODES, MAX_ENTRIES>, IndexError> {
    decode_index_state(data).map(|(namespace, _, _, _)| namespace)
}

/// 解码命名空间与卷挂载状态。
///
/// # Errors
///
/// 结构/CRC/边界非法。
pub fn decode_index_state<const MAX_INODES: usize, const MAX_ENTRIES: usize>(
    data: &[u8],
) -> Result<(Namespace<MAX_INODES, MAX_ENTRIES>, u32, u32, u32), IndexError> {
    if data.len() < VOLUME_INDEX_HEADER_LEN + 4
        || u32::from_le_bytes(
            data.get(..4)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        ) != VOLUME_INDEX_MAGIC
        || *data.get(4).ok_or(IndexError::Corrupt)? != VOLUME_INDEX_VERSION
    {
        return Err(IndexError::Corrupt);
    }
    let inode_count = usize::from(u16::from_le_bytes(
        data.get(6..8)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    ));
    let entry_count = usize::from(u16::from_le_bytes(
        data.get(8..10)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    ));
    let version = u32::from_le_bytes(
        data.get(10..14)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    );
    let current_block = u32::from_le_bytes(
        data.get(14..18)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    );
    let cursor = u32::from_le_bytes(
        data.get(18..22)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    );
    let total = VOLUME_INDEX_HEADER_LEN
        .checked_add(
            inode_count
                .checked_mul(INODE_LEN)
                .ok_or(IndexError::Corrupt)?,
        )
        .and_then(|value| value.checked_add(entry_count.checked_mul(ENTRY_LEN)?))
        .and_then(|value| value.checked_add(4))
        .ok_or(IndexError::Corrupt)?;
    if data.len() < total {
        return Err(IndexError::BufferTooSmall);
    }
    let body = data.get(..total - 4).ok_or(IndexError::Corrupt)?;
    let stored = u32::from_le_bytes(
        data.get(total - 4..total)
            .ok_or(IndexError::Corrupt)?
            .try_into()
            .map_err(|_| IndexError::Corrupt)?,
    );
    if crc32_finish(crc32_update(CRC32_INIT, body)) != stored {
        return Err(IndexError::BadCrc);
    }
    let mut namespace = Namespace::new();
    namespace.init_root().map_err(|_| IndexError::Full)?;
    let mut at = VOLUME_INDEX_HEADER_LEN;
    for _ in 0..inode_count {
        let inode = u32::from_le_bytes(
            data.get(at..at + 4)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        let kind = decode_kind(*data.get(at + 4).ok_or(IndexError::Corrupt)?)
            .ok_or(IndexError::Corrupt)?;
        let links = u16::from_le_bytes(
            data.get(at + 5..at + 7)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        if inode != 1 {
            namespace
                .restore_inode(inode, kind)
                .map_err(|_| IndexError::Full)?;
        }
        namespace
            .set_links(inode, links)
            .map_err(|_| IndexError::Corrupt)?;
        at += INODE_LEN;
    }
    for _ in 0..entry_count {
        let parent = u32::from_le_bytes(
            data.get(at..at + 4)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        let child = u32::from_le_bytes(
            data.get(at + 4..at + 8)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        let name_len = usize::from(*data.get(at + 8).ok_or(IndexError::Corrupt)?);
        if name_len > MAX_NAME_LEN {
            return Err(IndexError::Corrupt);
        }
        let name = data
            .get(at + 9..at + 9 + name_len)
            .ok_or(IndexError::Corrupt)?;
        let kind = namespace.inode_kind(child).ok_or(IndexError::Corrupt)?;
        namespace
            .restore_dirent(parent, name, child, kind)
            .map_err(|_| IndexError::Full)?;
        at += ENTRY_LEN;
    }
    // 引用计数必须在目录项恢复之后写回：restore_dirent 会递增计数，
    // 最后以快照值覆盖，避免 2×。
    let mut at = VOLUME_INDEX_HEADER_LEN;
    for _ in 0..inode_count {
        let inode = u32::from_le_bytes(
            data.get(at..at + 4)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        let links = u16::from_le_bytes(
            data.get(at + 5..at + 7)
                .ok_or(IndexError::Corrupt)?
                .try_into()
                .map_err(|_| IndexError::Corrupt)?,
        );
        namespace
            .set_links(inode, links)
            .map_err(|_| IndexError::Corrupt)?;
        at += INODE_LEN;
    }
    Ok((namespace, version, current_block, cursor))
}

fn kind_code(kind: InodeKind) -> u8 {
    match kind {
        InodeKind::Data => 0,
        InodeKind::Log => 1,
        InodeKind::Directory => 2,
    }
}

fn decode_kind(code: u8) -> Option<InodeKind> {
    match code {
        0 => Some(InodeKind::Data),
        1 => Some(InodeKind::Log),
        2 => Some(InodeKind::Directory),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace::ROOT_INODE;

    #[test]
    fn 索引快照往返命名空间() {
        let mut ns = Namespace::<16, 32>::new();
        ns.init_root().unwrap();
        let d = ns.mkdir(ROOT_INODE, b"d").unwrap();
        let f = ns.create_file(d, b"f").unwrap();
        ns.link(f, ROOT_INODE, b"g").unwrap();
        let mut buf = [0u8; 1024];
        let len = encode_index(&ns, &mut buf).unwrap();
        let restored = decode_index::<16, 32>(&buf[..len]).unwrap();
        assert_eq!(restored.resolve(b"/g"), Some(f));
        assert_eq!(restored.link_count(f), Some(2));
    }

    #[test]
    fn 索引_crc_损坏被拒绝() {
        let mut ns = Namespace::<8, 8>::new();
        ns.init_root().unwrap();
        let mut buf = [0u8; 512];
        let len = encode_index(&ns, &mut buf).unwrap();
        if let Some(byte) = buf.get_mut(0) {
            *byte ^= 0xFF;
        }
        assert!(matches!(
            decode_index::<8, 8>(&buf[..len]),
            Err(IndexError::Corrupt)
        ));
    }
    #[test]
    fn 索引错误路径() {
        let mut ns = Namespace::<8, 8>::new();
        ns.init_root().unwrap();
        let mut buf = [0u8; 16];
        assert!(matches!(
            encode_index(&ns, &mut buf),
            Err(IndexError::BufferTooSmall)
        ));
        let mut small = Namespace::<1, 1>::new();
        small.init_root().unwrap_err();
        let mut out = [0u8; 512];
        let len = encode_index(&ns, &mut out).unwrap();
        let mut bad = out;
        bad[4] = 9;
        assert!(matches!(
            decode_index::<8, 8>(&bad[..len]),
            Err(IndexError::Corrupt)
        ));
        assert!(matches!(
            decode_index::<8, 8>(&[]),
            Err(IndexError::Corrupt)
        ));
    }
}
