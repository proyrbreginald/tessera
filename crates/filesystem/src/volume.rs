//! 文件系统 v3 的**最小卷层**：格式、挂载、目录项持久化。
//!
//! 这一层把 `v3_block` + `v3_record` + `v3` 命名空间接到 `BlockDevice` 上。
//! 当前只持久化目录/inode/硬链接/rename，**不包含文件内容读写、GC 或索引**；
//! 那些是后续步骤。它用于先把 v3 的挂载重建与掉电前顺序跑通。

use crate::MAX_NAME_LEN;
use crate::device::BlockDevice;
use crate::namespace::{InodeKind, Namespace, NamespaceError};
use crate::volume_block::{VOLUME_BLOCK_HEADER_LEN, VolumeBlockHeader, VolumeBlockState};
use crate::volume_index::{decode_index_state, encode_index_state};
use crate::volume_record::{
    FLAG_DATA, FLAG_DIRENT, FLAG_RENAME_COMMIT, FLAG_RENAME_INTENT, FLAG_TOMBSTONE,
    RecordCodecError, VolumeRecordHeader, decode_dirent_data, decode_record, encode_record,
};

/// 卷层 GC 的最大块数（固定数组，避免堆）。
const MAX_BLOCKS: usize = 512;

/// 卷错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeError {
    /// 设备驱动错误。
    Driver,
    /// 记录编解码失败。
    Record(RecordCodecError),
    /// 命名空间错误。
    Namespace(NamespaceError),
    /// 空间不足。
    Full,
    /// 记录大于单块可用空间。
    TooLarge,
    /// 文件/inode 不存在。
    NotFound,
    /// 输出缓冲不足。
    BufferTooSmall,
    /// 记录/分块不一致。
    Corrupt,
}

impl From<RecordCodecError> for VolumeError {
    fn from(value: RecordCodecError) -> Self {
        Self::Record(value)
    }
}

impl From<NamespaceError> for VolumeError {
    fn from(value: NamespaceError) -> Self {
        Self::Namespace(value)
    }
}

/// 内容记录单块大小（字节）。
const DATA_CHUNK: usize = 128;
/// 单个 inode 最多分块数。
const MAX_DATA_CHUNKS: usize = 512;
/// 单 inode 最大内容（字节）。
const MAX_DATA_BYTES: usize = DATA_CHUNK * MAX_DATA_CHUNKS;

/// 最小 v3 卷。
pub struct Volume<D: BlockDevice, const MAX_INODES: usize, const MAX_ENTRIES: usize> {
    dev: D,
    namespace: Namespace<MAX_INODES, MAX_ENTRIES>,
    block_size: u32,
    block_count: u32,
    current_block: u32,
    cursor: u32,
    version: u32,
    index_clean: bool,
    free: [bool; MAX_BLOCKS],
    started: [bool; MAX_BLOCKS],
    compacting: bool,
    compact_active: bool,
    compact_cursor: u32,
    compact_source: [bool; MAX_BLOCKS],
}

impl<D: BlockDevice, const MAX_INODES: usize, const MAX_ENTRIES: usize>
    Volume<D, MAX_INODES, MAX_ENTRIES>
{
    /// 格式化：擦除全部块并写入 v3 块头。
    ///
    /// # Errors
    ///
    /// 设备擦除/编程失败，或块几何非法。
    pub fn format(mut dev: D) -> Result<Self, VolumeError> {
        let block_size = dev.block_size();
        let block_count = dev.block_count();
        if block_size < 64
            || block_count < 2
            || usize::try_from(block_count).unwrap_or(usize::MAX) > MAX_BLOCKS
        {
            return Err(VolumeError::Driver);
        }
        // 惰性格式化：只擦/写 block 0；其余块在首次追加时再写块头。
        // 全片 443 块预擦会让首次启动多花数十秒，且大部分块根本用不到。
        dev.erase_block(0).map_err(|_| VolumeError::Driver)?;
        dev.erase_block(block_count.saturating_sub(1))
            .map_err(|_| VolumeError::Driver)?;
        let header = VolumeBlockHeader {
            seq: 1,
            state: VolumeBlockState::Open,
            erase_count: 1,
        };
        dev.program(0, &header.encode())
            .map_err(|_| VolumeError::Driver)?;
        let mut started = [false; MAX_BLOCKS];
        started[0] = true;
        let mut namespace = Namespace::new();
        namespace.init_root()?;
        Ok(Self {
            dev,
            namespace,
            block_size,
            block_count,
            current_block: 0,
            cursor: u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16),
            version: 1,
            index_clean: false,
            free: [true; MAX_BLOCKS],
            started,
            compacting: false,
            compact_active: false,
            compact_cursor: 0,
            compact_source: [false; MAX_BLOCKS],
        })
    }

    /// 挂载：扫描所有块里的 v3 记录，重建命名空间。
    ///
    /// # Errors
    ///
    /// 设备读失败或没有任何有效块头。
    pub fn mount(mut dev: D) -> Result<Self, VolumeError> {
        let block_size = dev.block_size();
        let block_count = dev.block_count();
        if block_size < 64 || block_count < 2 {
            return Err(VolumeError::Driver);
        }
        let mut namespace = Namespace::new();
        namespace.init_root()?;
        let mut index_clean = false;
        let mut indexed_state = None;
        let index_block = block_count.saturating_sub(1);
        if let Ok(header) = read_block_header(&mut dev, block_size, index_block)
            && header.state == VolumeBlockState::Open
        {
            let mut buf = [0u8; 4096];
            let base = index_block
                .checked_mul(block_size)
                .and_then(|value| {
                    value.checked_add(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16))
                })
                .ok_or(VolumeError::Driver)?;
            let read_len = usize::try_from(
                block_size.saturating_sub(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16)),
            )
            .unwrap_or(0)
            .min(buf.len());
            if read_len > 0 {
                dev.read(base, buf.get_mut(..read_len).ok_or(VolumeError::Driver)?)
                    .map_err(|_| VolumeError::Driver)?;
                if let Ok((restored, version, block, cursor)) =
                    decode_index_state::<MAX_INODES, MAX_ENTRIES>(
                        buf.get(..read_len).ok_or(VolumeError::Driver)?,
                    )
                {
                    namespace = restored;
                    index_clean = true;
                    indexed_state = Some((version, block, cursor));
                }
            }
        }
        let mut current_block = 0u32;
        let mut cursor = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
        let mut seq = 1u32;
        let mut version = 1u32;
        let mut valid_blocks = 0u32;
        let mut free = [false; MAX_BLOCKS];
        let mut started = [false; MAX_BLOCKS];
        if let Some((version, current_block, cursor)) = indexed_state {
            let free = [false; MAX_BLOCKS];
            let started = [false; MAX_BLOCKS];
            return Ok(Self {
                dev,
                namespace,
                block_size,
                block_count,
                current_block,
                cursor,
                version,
                index_clean: true,
                free,
                started,
                compacting: false,
                compact_active: false,
                compact_cursor: 0,
                compact_source: [false; MAX_BLOCKS],
            });
        }
        let data_blocks = block_count.saturating_sub(1);
        for block in 0..data_blocks {
            let mut raw = [0u8; VOLUME_BLOCK_HEADER_LEN];
            dev.read(
                block.checked_mul(block_size).ok_or(VolumeError::Driver)?,
                &mut raw,
            )
            .map_err(|_| VolumeError::Driver)?;
            let Ok(header) = VolumeBlockHeader::decode(&raw) else {
                if let Some(slot) = free.get_mut(usize::try_from(block).unwrap_or(usize::MAX)) {
                    *slot = true;
                }
                continue;
            };
            valid_blocks = valid_blocks.saturating_add(1);
            if let Some(slot) = started.get_mut(usize::try_from(block).unwrap_or(usize::MAX)) {
                *slot = true;
            }
            seq = seq.max(header.seq.saturating_add(1));
            let mut offset = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            let mut block_used = false;
            loop {
                let absolute = block
                    .checked_mul(block_size)
                    .and_then(|base| base.checked_add(offset))
                    .ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                dev.read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(record) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let total = 24usize
                    .saturating_add(usize::from(record.name_len))
                    .saturating_add(usize::from(record.data_len));
                if offset.saturating_add(u32::try_from(total).unwrap_or(u32::MAX)) > block_size {
                    break;
                }
                let mut record_bytes = [0u8; 512];
                let Some(slot) = record_bytes.get_mut(..total) else {
                    break;
                };
                dev.read(absolute, slot).map_err(|_| VolumeError::Driver)?;
                let Ok(full) = decode_record(slot) else {
                    break;
                };
                block_used = true;
                version = version.max(full.header.version);
                match full.header.flags {
                    FLAG_DIRENT => {
                        let (child, kind) = decode_dirent_data(full.data)?;
                        let Some(kind) = kind_from_code(kind) else {
                            break;
                        };
                        let _ = namespace.restore_dirent(full.header.inode, full.name, child, kind);
                    }
                    FLAG_TOMBSTONE => {
                        let _ = namespace.restore_remove(full.header.inode, full.name);
                    }
                    FLAG_RENAME_COMMIT => {
                        let mut payload = [0u8; 36];
                        let Some(slot) = payload.get_mut(..full.data.len()) else {
                            break;
                        };
                        slot.copy_from_slice(full.data);
                        let Some(new_parent) = read_u32(slot, 0) else {
                            break;
                        };
                        let Some(name_len) = slot.get(4).copied() else {
                            break;
                        };
                        let name_len = usize::from(name_len);
                        let Some(new_name) = slot.get(5..5usize.saturating_add(name_len)) else {
                            break;
                        };
                        if let Some(child) = namespace.lookup(full.header.inode, full.name)
                            && let Some(kind) = namespace.inode_kind(child)
                        {
                            let _ = namespace.restore_remove(full.header.inode, full.name);
                            let _ = namespace.restore_dirent(new_parent, new_name, child, kind);
                        }
                    }
                    _ => {}
                }
                offset = offset.saturating_add(u32::try_from(total).unwrap_or(0));
            }
            if !block_used
                && block != index_block
                && let Some(slot) = free.get_mut(usize::try_from(block).unwrap_or(usize::MAX))
            {
                *slot = true;
            }
            current_block = block;
            cursor = offset;
        }
        if valid_blocks == 0 {
            return Err(VolumeError::Driver);
        }
        Ok(Self {
            dev,
            namespace,
            block_size,
            block_count,
            current_block: current_block.min(block_count.saturating_sub(2)),
            cursor,
            version: version.saturating_add(1),
            index_clean,
            free,
            started,
            compacting: false,
            compact_active: false,
            compact_cursor: 0,
            compact_source: [false; MAX_BLOCKS],
        })
    }

    /// 把命名空间快照写入最后一个保留块。
    ///
    /// # Errors
    ///
    /// 快照过大或设备擦写失败。
    pub fn checkpoint(&mut self) -> Result<(), VolumeError> {
        let mut buf = [0u8; 4096];
        let len = encode_index_state(
            &self.namespace,
            self.version,
            self.current_block,
            self.cursor,
            &mut buf,
        )
        .map_err(|_| VolumeError::TooLarge)?;
        if u32::try_from(len).unwrap_or(u32::MAX)
            > self
                .block_size
                .saturating_sub(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16))
        {
            return Err(VolumeError::TooLarge);
        }
        let block = self.block_count.saturating_sub(1);
        self.dev
            .erase_block(block)
            .map_err(|_| VolumeError::Driver)?;
        let header = VolumeBlockHeader {
            seq: self.version,
            state: VolumeBlockState::Open,
            erase_count: 1,
        };
        let base = block
            .checked_mul(self.block_size)
            .ok_or(VolumeError::Driver)?;
        self.dev
            .program(base, &header.encode())
            .map_err(|_| VolumeError::Driver)?;
        self.dev
            .program(
                base.checked_add(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16))
                    .ok_or(VolumeError::Driver)?,
                buf.get(..len).ok_or(VolumeError::TooLarge)?,
            )
            .map_err(|_| VolumeError::Driver)?;
        self.index_clean = true;
        Ok(())
    }

    /// 活记录搬移式压缩：把仍有效的 DIRENT/DATA/LOG 复制到空闲块，再擦除旧块。
    ///
    /// 需要至少能容纳全部活记录的临时空闲块；空间不足时返回 `Full`，源数据不变。
    ///
    /// # Errors
    ///
    /// 空间不足、设备读写/擦除失败。
    /// 分片搬移压缩：每步最多处理 `max_blocks` 个源块，返回 `(复制记录数, 是否完成)`。
    ///
    /// 与一次性 `compact` 不同，这里逐块复制并擦除源块；调用方按 `true` 判断结束。
    /// 首次调用会自动建立源块快照并检查是否有空闲块。
    pub fn compact_step(&mut self, max_blocks: u32) -> Result<(u32, bool), VolumeError> {
        self.compacting = true;
        let result = self.compact_step_inner(max_blocks);
        self.compacting = false;
        result
    }

    fn compact_step_inner(&mut self, max_blocks: u32) -> Result<(u32, bool), VolumeError> {
        if !self.compact_active {
            if self.block_usage().0 == 0 {
                return Err(VolumeError::Full);
            }
            self.compact_source = self.free;
            self.compact_cursor = 0;
            // 强制下一次追加走 `next_block`，避免把搬移副本写回源块后再被擦除。
            self.current_block = 0;
            self.cursor = self.block_size;
            self.compact_active = true;
        }
        let data_blocks = self.block_count.saturating_sub(1);
        let end = self
            .compact_cursor
            .saturating_add(max_blocks)
            .min(data_blocks);
        let mut copied = 0u32;
        for block in self.compact_cursor..end {
            if self
                .compact_source
                .get(usize::try_from(block).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                let mut record_bytes = [0u8; 512];
                let Some(slot) = record_bytes.get_mut(..len) else {
                    break;
                };
                self.dev
                    .read(absolute, slot)
                    .map_err(|_| VolumeError::Driver)?;
                let Ok(full) = decode_record(slot) else {
                    break;
                };
                let live = match full.header.flags {
                    crate::volume_record::FLAG_DIRENT => self
                        .namespace
                        .lookup(full.header.inode, full.name)
                        .map(|child| {
                            decode_dirent_data(full.data)
                                .map(|(expected, _)| expected == child)
                                .unwrap_or(false)
                        })
                        .unwrap_or(false),
                    FLAG_DATA => self.data_version_is_latest(
                        full.header.inode,
                        full.header.chunk_index,
                        full.header.version,
                        full.header.chunk_total,
                    )?,
                    crate::volume_record::FLAG_LOG => true,
                    _ => false,
                };
                if live {
                    self.append_record(
                        full.header.flags,
                        full.header.inode,
                        full.header.version,
                        full.header.chunk_index,
                        full.header.chunk_total,
                        full.name,
                        full.data,
                    )?;
                    copied = copied.saturating_add(1);
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
            // 当前源块的全部活记录都已复制；现在可以安全擦除它。
            self.dev
                .erase_block(block)
                .map_err(|_| VolumeError::Driver)?;
            if let Some(slot) = self
                .free
                .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
            {
                *slot = true;
            }
            if let Some(slot) = self
                .started
                .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
            {
                *slot = false;
            }
        }
        self.compact_cursor = end;
        let done = end >= data_blocks;
        if done {
            self.compact_active = false;
            self.index_clean = false;
        }
        Ok((copied, done))
    }

    /// 一次性搬移压缩（保留给显式/启动路径；维护队列使用 [`Self::compact_step`]）。
    pub fn compact(&mut self) -> Result<u32, VolumeError> {
        self.compacting = true;
        let result = self.compact_inner();
        self.compacting = false;
        result
    }

    fn compact_inner(&mut self) -> Result<u32, VolumeError> {
        if self.block_usage().0 == 0 {
            // 没有空闲块时搬移式压缩无法启动；先由调用方回收死块。
            return Err(VolumeError::Full);
        }
        self.compacting = true;
        let data_blocks = self.block_count.saturating_sub(1);
        let source = self.free;
        let mut copied = 0u32;
        for block in 0..data_blocks {
            if source
                .get(usize::try_from(block).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                let mut record_bytes = [0u8; 512];
                let Some(slot) = record_bytes.get_mut(..len) else {
                    break;
                };
                self.dev
                    .read(absolute, slot)
                    .map_err(|_| VolumeError::Driver)?;
                let Ok(full) = decode_record(slot) else {
                    break;
                };
                let live = match full.header.flags {
                    crate::volume_record::FLAG_DIRENT => self
                        .namespace
                        .lookup(full.header.inode, full.name)
                        .map(|child| {
                            decode_dirent_data(full.data)
                                .map(|(expected, _)| expected == child)
                                .unwrap_or(false)
                        })
                        .unwrap_or(false),
                    FLAG_DATA => self.data_version_is_latest(
                        full.header.inode,
                        full.header.chunk_index,
                        full.header.version,
                        full.header.chunk_total,
                    )?,
                    crate::volume_record::FLAG_LOG => true,
                    _ => false,
                };
                if live {
                    self.append_record(
                        full.header.flags,
                        full.header.inode,
                        full.header.version,
                        full.header.chunk_index,
                        full.header.chunk_total,
                        full.name,
                        full.data,
                    )?;
                    copied = copied.saturating_add(1);
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
        }
        // 搬移完成后，擦除全部源块。
        for block in 0..data_blocks {
            if source
                .get(usize::try_from(block).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            self.dev
                .erase_block(block)
                .map_err(|_| VolumeError::Driver)?;
            if let Some(started) = self
                .started
                .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
            {
                *started = false;
            }
            if let Some(free) = self
                .free
                .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
            {
                *free = true;
            }
        }
        self.index_clean = false;
        self.compacting = false;
        Ok(copied)
    }

    /// 写路径保留的空闲块数：给 GC 搬移留出临时空间。
    #[must_use]
    pub fn reserved_blocks(&self) -> u32 {
        let data = self.block_count.saturating_sub(1);
        (data / 8).clamp(1, 32)
    }

    /// 本次挂载是否使用了持久化索引快照。
    #[must_use]
    pub const fn index_loaded(&self) -> bool {
        self.index_clean
    }

    /// 数据块使用情况（空闲/已用）。
    #[must_use]
    pub fn block_usage(&self) -> (u32, u32) {
        let data_blocks = usize::try_from(self.block_count.saturating_sub(1)).unwrap_or(0);
        let free = self
            .free
            .iter()
            .take(data_blocks)
            .filter(|free| **free)
            .count();
        let free = u32::try_from(free).unwrap_or(u32::MAX);
        let used = u32::try_from(data_blocks)
            .unwrap_or(u32::MAX)
            .saturating_sub(free);
        (free, used)
    }

    /// 命名空间只读视图。
    /// 命名空间只读视图。
    #[must_use]
    pub const fn namespace(&self) -> &Namespace<MAX_INODES, MAX_ENTRIES> {
        &self.namespace
    }

    /// 设备只读视图。
    #[must_use]
    pub const fn device(&self) -> &D {
        &self.dev
    }

    /// 按 inode 写文件内容。当前实现为单条 DATA 记录（≤512 B 测试上限）。
    ///
    /// # Errors
    ///
    /// inode 不存在、数据过大、空间不足。
    pub fn write_inode(&mut self, inode: u32, data: &[u8]) -> Result<(), VolumeError> {
        if !self.namespace.inode_exists(inode) {
            return Err(VolumeError::NotFound);
        }
        if data.is_empty() || data.len() > MAX_DATA_BYTES {
            return Err(VolumeError::TooLarge);
        }
        let total = data.chunks(DATA_CHUNK).count();
        let version = self.version;
        for (index, chunk) in data.chunks(DATA_CHUNK).enumerate() {
            self.append_record(
                FLAG_DATA,
                inode,
                version,
                u16::try_from(index).map_err(|_| VolumeError::TooLarge)?,
                u16::try_from(total).map_err(|_| VolumeError::TooLarge)?,
                &[],
                chunk,
            )?;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// 按路径流式写入。
    ///
    /// # Errors
    ///
    /// 路径不存在、块数超限、回调/空间错误。
    pub fn write_file_stream<F>(
        &mut self,
        path: &[u8],
        total_chunks: usize,
        fill: F,
    ) -> Result<(), VolumeError>
    where
        F: FnMut(usize, &mut [u8]) -> Result<usize, VolumeError>,
    {
        let inode = self.resolve(path).ok_or(VolumeError::NotFound)?;
        self.write_inode_stream(inode, total_chunks, fill)
    }

    /// 按路径读区间。
    ///
    /// # Errors
    ///
    /// 路径不存在、输出缓冲不足、记录不一致、设备错误。
    pub fn read_file_range(
        &mut self,
        path: &[u8],
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, VolumeError> {
        let inode = self.resolve(path).ok_or(VolumeError::NotFound)?;
        self.read_inode_range(inode, offset, out)
    }

    /// 按 inode 流式写入：调用方逐块填充 128 B 缓冲，不一次性持有整文件。
    ///
    /// `total_chunks` 必须与实际填充块数一致；每块返回 1..=128 字节。
    ///
    /// # Errors
    ///
    /// inode 不存在、块数超限、回调返回非法长度、空间不足。
    pub fn write_inode_stream<F>(
        &mut self,
        inode: u32,
        total_chunks: usize,
        mut fill: F,
    ) -> Result<(), VolumeError>
    where
        F: FnMut(usize, &mut [u8]) -> Result<usize, VolumeError>,
    {
        if !self.namespace.inode_exists(inode) {
            return Err(VolumeError::NotFound);
        }
        if total_chunks == 0 || total_chunks > MAX_DATA_CHUNKS {
            return Err(VolumeError::TooLarge);
        }
        let version = self.version;
        for index in 0..total_chunks {
            let mut chunk = [0u8; DATA_CHUNK];
            let len = fill(index, &mut chunk)?;
            if len == 0 || len > DATA_CHUNK {
                return Err(VolumeError::TooLarge);
            }
            self.append_record(
                FLAG_DATA,
                inode,
                version,
                u16::try_from(index).map_err(|_| VolumeError::TooLarge)?,
                u16::try_from(total_chunks).map_err(|_| VolumeError::TooLarge)?,
                &[],
                chunk.get(..len).ok_or(VolumeError::TooLarge)?,
            )?;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// 按 inode 读最新版本内容。
    ///
    /// # Errors
    ///
    /// inode 不存在、没有内容、输出缓冲不足、设备错误。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn read_inode(&mut self, inode: u32, out: &mut [u8]) -> Result<usize, VolumeError> {
        self.read_inode_range(inode, 0, out)
    }

    /// 日志文件总长度（所有 LOG 记录 data_len 之和）。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn log_size(&mut self, inode: u32) -> Result<usize, VolumeError> {
        let mut total = 0usize;
        for block in 0..self.block_count.saturating_sub(1) {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                if header.flags == crate::volume_record::FLAG_LOG && header.inode == inode {
                    total = total.saturating_add(usize::from(header.data_len));
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
        }
        Ok(total)
    }

    /// 文件当前内容的字节数（按最高 version 的 chunk_total 估算）。
    ///
    /// # Errors
    ///
    /// inode 不存在、记录不一致、设备错误。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn inode_size(&mut self, inode: u32) -> Result<usize, VolumeError> {
        if !self.namespace.inode_exists(inode) {
            return Err(VolumeError::NotFound);
        }
        if self.namespace.inode_kind(inode) == Some(InodeKind::Log) {
            return self.log_size(inode);
        }
        let (version, total) = self.latest_data_version(inode)?;
        if version == 0 || total == 0 {
            return Ok(0);
        }
        let full = total.saturating_sub(1).saturating_mul(DATA_CHUNK);
        let mut last_len = 0usize;
        for block in 0..self.block_count.saturating_sub(1) {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                if header.flags == FLAG_DATA
                    && header.inode == inode
                    && header.version == version
                    && usize::from(header.chunk_index).saturating_add(1) == total
                {
                    last_len = usize::from(header.data_len);
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
        }
        Ok(full.saturating_add(last_len))
    }

    /// 追加一条日志记录（O(1)，不重写旧内容）。
    ///
    /// # Errors
    ///
    /// inode 不存在/不是日志、单条超过 512 B、空间不足。
    pub fn append_log(&mut self, inode: u32, data: &[u8]) -> Result<(), VolumeError> {
        if self.namespace.inode_kind(inode) != Some(InodeKind::Log) {
            return Err(VolumeError::NotFound);
        }
        if data.is_empty() || data.len() > 512 {
            return Err(VolumeError::TooLarge);
        }
        self.append_record(
            crate::volume_record::FLAG_LOG,
            inode,
            self.version,
            0,
            1,
            &[],
            data,
        )?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// 按 inode 读日志区间（所有 LOG 记录按 version 顺序拼接）。
    ///
    /// # Errors
    ///
    /// inode 不存在、设备错误。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn read_log_range(
        &mut self,
        inode: u32,
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, VolumeError> {
        if self.namespace.inode_kind(inode) != Some(InodeKind::Log) {
            return Err(VolumeError::NotFound);
        }
        let mut written = 0usize;
        let mut logical = 0usize;
        for block in 0..self.block_count.saturating_sub(1) {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                if header.flags == crate::volume_record::FLAG_LOG && header.inode == inode {
                    let mut record_bytes = [0u8; 512];
                    let Some(slot) = record_bytes.get_mut(..len) else {
                        break;
                    };
                    self.dev
                        .read(absolute, slot)
                        .map_err(|_| VolumeError::Driver)?;
                    let Ok(full) = decode_record(slot) else { break };
                    let start = logical;
                    let end = logical.saturating_add(full.data.len());
                    let want_start = offset.max(start);
                    let want_end = offset.saturating_add(out.len()).min(end);
                    if want_start < want_end {
                        let source_at = want_start.saturating_sub(start);
                        let dest_at = want_start.saturating_sub(offset);
                        let copy_len = want_end.saturating_sub(want_start);
                        let Some(source) =
                            full.data.get(source_at..source_at.saturating_add(copy_len))
                        else {
                            break;
                        };
                        let Some(dest) = out.get_mut(dest_at..dest_at.saturating_add(copy_len))
                        else {
                            return Err(VolumeError::BufferTooSmall);
                        };
                        dest.copy_from_slice(source);
                        written = written.saturating_add(copy_len);
                    }
                    logical = end;
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
        }
        Ok(written)
    }

    /// 按 inode 流式读取 `[offset, offset+out.len())`，不构造整文件缓冲。
    ///
    /// # Errors
    ///
    /// inode 不存在、输出缓冲不足、记录不一致、设备错误。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn read_inode_range(
        &mut self,
        inode: u32,
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, VolumeError> {
        if !self.namespace.inode_exists(inode) {
            return Err(VolumeError::NotFound);
        }
        if self.namespace.inode_kind(inode) == Some(InodeKind::Log) {
            return self.read_log_range(inode, offset, out);
        }
        let (best_version, chunk_total) = self.latest_data_version(inode)?;
        if best_version == 0 || chunk_total == 0 {
            return Err(VolumeError::NotFound);
        }
        let mut written = 0usize;
        for block in 0..self.block_count.saturating_sub(1) {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let total = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if total > 512
                    || at.saturating_add(u32::try_from(total).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                if header.flags == FLAG_DATA
                    && header.inode == inode
                    && header.version == best_version
                    && header.chunk_total == u16::try_from(chunk_total).unwrap_or(0)
                {
                    let mut record_bytes = [0u8; 512];
                    let Some(slot) = record_bytes.get_mut(..total) else {
                        break;
                    };
                    self.dev
                        .read(absolute, slot)
                        .map_err(|_| VolumeError::Driver)?;
                    let Ok(full) = decode_record(slot) else {
                        break;
                    };
                    let chunk_start =
                        usize::from(full.header.chunk_index).saturating_mul(DATA_CHUNK);
                    let chunk_end = chunk_start.saturating_add(full.data.len());
                    let want_start = offset.max(chunk_start);
                    let want_end = offset.saturating_add(out.len()).min(chunk_end);
                    if want_start < want_end {
                        let source_at = want_start.saturating_sub(chunk_start);
                        let dest_at = want_start.saturating_sub(offset);
                        let len = want_end.saturating_sub(want_start);
                        let Some(source) = full.data.get(source_at..source_at.saturating_add(len))
                        else {
                            break;
                        };
                        let Some(dest) = out.get_mut(dest_at..dest_at.saturating_add(len)) else {
                            return Err(VolumeError::BufferTooSmall);
                        };
                        dest.copy_from_slice(source);
                        written = written.saturating_add(len);
                    }
                }
                at = at.saturating_add(u32::try_from(total).unwrap_or(0));
            }
        }
        Ok(written)
    }

    /// 找该 inode 的最高 DATA version 与 chunk_total。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn latest_data_version(&mut self, inode: u32) -> Result<(u32, usize), VolumeError> {
        let mut best = 0u32;
        let mut total = 0usize;
        for block in 0..self.block_count.saturating_sub(1) {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut at = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(at).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let len = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if len > 512
                    || at.saturating_add(u32::try_from(len).unwrap_or(u32::MAX)) > self.block_size
                {
                    break;
                }
                if header.flags == FLAG_DATA && header.inode == inode && header.version > best {
                    best = header.version;
                    total = usize::from(header.chunk_total);
                }
                at = at.saturating_add(u32::try_from(len).unwrap_or(0));
            }
        }
        Ok((best, total))
    }

    /// 按路径写文件内容。
    ///
    /// # Errors
    ///
    /// 路径不存在、数据过大、空间不足。
    pub fn write_file(&mut self, path: &[u8], data: &[u8]) -> Result<(), VolumeError> {
        let inode = self.resolve(path).ok_or(VolumeError::NotFound)?;
        self.write_inode(inode, data)
    }

    /// 按路径读最新内容。
    ///
    /// # Errors
    ///
    /// 路径不存在、没有内容、输出缓冲不足、设备错误。
    pub fn read_file(&mut self, path: &[u8], out: &mut [u8]) -> Result<usize, VolumeError> {
        let inode = self.resolve(path).ok_or(VolumeError::NotFound)?;
        self.read_inode(inode, out)
    }

    fn next_block(&mut self) -> Result<(), VolumeError> {
        let data_blocks = self.block_count.saturating_sub(1);
        let mut index = 0u32;
        while index < data_blocks {
            if self
                .free
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .copied()
                == Some(true)
            {
                self.current_block = index;
                self.cursor = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
                return Ok(());
            }
            index = index.saturating_add(1);
        }
        self.current_block = self.current_block.saturating_add(1);
        self.cursor = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
        if self.current_block >= data_blocks {
            return Err(VolumeError::Full);
        }
        Ok(())
    }

    /// 判断一条 DATA 记录是否仍是该 inode 对应 chunk 的最新版本。
    fn data_version_is_latest(
        &mut self,
        inode: u32,
        chunk_index: u16,
        version: u32,
        chunk_total: u16,
    ) -> Result<bool, VolumeError> {
        let data_blocks = self.block_count.saturating_sub(1);
        let mut latest = version;
        for block in 0..data_blocks {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut offset = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            loop {
                let absolute = base.checked_add(offset).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let total = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if total > 512
                    || offset.saturating_add(u32::try_from(total).unwrap_or(u32::MAX))
                        > self.block_size
                {
                    break;
                }
                if header.flags == FLAG_DATA
                    && header.inode == inode
                    && header.chunk_index == chunk_index
                    && header.chunk_total == chunk_total
                {
                    latest = latest.max(header.version);
                }
                offset = offset.saturating_add(u32::try_from(total).unwrap_or(0));
            }
        }
        Ok(version >= latest)
    }

    /// 回收**整块没有活 DATA/DIRENT 记录**的块（墓碑、旧版本内容等）。
    ///
    /// 这是 GC 的第一版保守策略：不做活记录搬移，只回收确定已死的块。
    ///
    /// # Errors
    ///
    /// 设备擦写失败。
    pub fn gc_dead_blocks(&mut self) -> Result<u32, VolumeError> {
        let data_blocks = self.block_count.saturating_sub(1);
        let mut reclaimed = 0u32;
        for block in 0..data_blocks {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut offset = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            let mut has_live = false;
            let mut has_record = false;
            loop {
                let absolute = base.checked_add(offset).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let total = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if total > 512
                    || offset.saturating_add(u32::try_from(total).unwrap_or(u32::MAX))
                        > self.block_size
                {
                    break;
                }
                has_record = true;
                let live = header.flags == FLAG_DIRENT
                    || (header.flags == FLAG_DATA
                        && self.data_version_is_latest(
                            header.inode,
                            header.chunk_index,
                            header.version,
                            header.chunk_total,
                        )?);
                if live {
                    has_live = true;
                }
                offset = offset.saturating_add(u32::try_from(total).unwrap_or(0));
            }
            if has_record && !has_live {
                self.dev
                    .erase_block(block)
                    .map_err(|_| VolumeError::Driver)?;
                let header = VolumeBlockHeader {
                    seq: self.version,
                    state: VolumeBlockState::Open,
                    erase_count: 1,
                };
                self.dev
                    .program(base, &header.encode())
                    .map_err(|_| VolumeError::Driver)?;
                if let Some(slot) = self
                    .free
                    .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
                {
                    *slot = true;
                }
                reclaimed = reclaimed.saturating_add(1);
            }
        }
        Ok(reclaimed)
    }

    /// 分片回收死块：从 `start` 开始最多扫描 `max_blocks` 个数据块。
    ///
    /// 返回 `(本轮回收数, 下一个起点)`；调用方按 `next` 推进，直到等于数据块数。
    /// 这是 `fs gc` 的可中断版本：单次只做少量块，避免一次占满整个 idle 轮。
    pub fn gc_dead_blocks_step(
        &mut self,
        start: u32,
        max_blocks: u32,
    ) -> Result<(u32, u32), VolumeError> {
        let data_blocks = self.block_count.saturating_sub(1);
        let end = start.saturating_add(max_blocks).min(data_blocks);
        let mut reclaimed = 0u32;
        for block in start..end {
            let base = block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            let mut offset = u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16);
            let mut has_live = false;
            let mut has_record = false;
            loop {
                let absolute = base.checked_add(offset).ok_or(VolumeError::Driver)?;
                let mut header_raw = [0u8; 24];
                self.dev
                    .read(absolute, &mut header_raw)
                    .map_err(|_| VolumeError::Driver)?;
                if header_raw.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(&header_raw) else {
                    break;
                };
                let total = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                if total > 512
                    || offset.saturating_add(u32::try_from(total).unwrap_or(u32::MAX))
                        > self.block_size
                {
                    break;
                }
                has_record = true;
                let live = header.flags == FLAG_DIRENT
                    || (header.flags == FLAG_DATA
                        && self.data_version_is_latest(
                            header.inode,
                            header.chunk_index,
                            header.version,
                            header.chunk_total,
                        )?);
                if live {
                    has_live = true;
                }
                offset = offset.saturating_add(u32::try_from(total).unwrap_or(0));
            }
            if has_record && !has_live {
                self.dev
                    .erase_block(block)
                    .map_err(|_| VolumeError::Driver)?;
                let header = VolumeBlockHeader {
                    seq: self.version,
                    state: VolumeBlockState::Open,
                    erase_count: 1,
                };
                self.dev
                    .program(base, &header.encode())
                    .map_err(|_| VolumeError::Driver)?;
                if let Some(slot) = self
                    .free
                    .get_mut(usize::try_from(block).unwrap_or(usize::MAX))
                {
                    *slot = true;
                }
                reclaimed = reclaimed.saturating_add(1);
            }
        }
        Ok((reclaimed, end))
    }

    /// 数据块数量（不含保留的索引块）。
    #[must_use]
    pub const fn data_block_count(&self) -> u32 {
        self.block_count.saturating_sub(1)
    }

    /// 第一次写入后把索引 clean 字节从 1 编程为 0；失败回退全盘扫描。
    fn mark_index_dirty(&mut self) -> Result<(), VolumeError> {
        if !self.index_clean {
            return Ok(());
        }
        let block = self.block_count.saturating_sub(1);
        let base = block
            .checked_mul(self.block_size)
            .and_then(|value| {
                value.checked_add(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16))
            })
            .and_then(|value| value.checked_add(5))
            .ok_or(VolumeError::Driver)?;
        self.dev
            .program(base, &[0])
            .map_err(|_| VolumeError::Driver)?;
        self.index_clean = false;
        Ok(())
    }

    /// 在当前块追加大记录；空间不足时换块。
    fn append(
        &mut self,
        flags: u8,
        inode: u32,
        name: &[u8],
        data: &[u8],
    ) -> Result<(), VolumeError> {
        let version = self.version;
        self.append_record(flags, inode, version, 0, 1, name, data)?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // 记录字段本身就是扁平编码参数
    fn append_record(
        &mut self,
        flags: u8,
        inode: u32,
        version: u32,
        chunk_index: u16,
        chunk_total: u16,
        name: &[u8],
        data: &[u8],
    ) -> Result<(), VolumeError> {
        // 只有命名空间变化才让索引失效；DATA/LOG 追加不影响目录树。
        if matches!(
            flags,
            FLAG_DIRENT
                | FLAG_TOMBSTONE
                | crate::volume_record::FLAG_RENAME_INTENT
                | crate::volume_record::FLAG_RENAME_COMMIT
        ) {
            self.mark_index_dirty()?;
        }
        if !self.compacting && self.block_usage().0 <= self.reserved_blocks() {
            return Err(VolumeError::Full);
        }
        let mut raw = [0u8; 512];
        let len = encode_record(
            flags,
            inode,
            version,
            chunk_index,
            chunk_total,
            name,
            data,
            &mut raw,
        )?;
        if u32::try_from(len).unwrap_or(u32::MAX)
            > self
                .block_size
                .saturating_sub(u32::try_from(VOLUME_BLOCK_HEADER_LEN).unwrap_or(16))
        {
            return Err(VolumeError::TooLarge);
        }
        if self
            .cursor
            .saturating_add(u32::try_from(len).unwrap_or(u32::MAX))
            > self.block_size
        {
            self.next_block()?;
        }
        let absolute = self
            .current_block
            .checked_mul(self.block_size)
            .and_then(|base| base.checked_add(self.cursor))
            .ok_or(VolumeError::Driver)?;
        let block_index = usize::try_from(self.current_block).unwrap_or(usize::MAX);
        if self.started.get(block_index).copied() == Some(false) {
            let header = VolumeBlockHeader {
                seq: self.version,
                state: VolumeBlockState::Open,
                erase_count: 1,
            };
            let block_base = self
                .current_block
                .checked_mul(self.block_size)
                .ok_or(VolumeError::Driver)?;
            self.dev
                .program(block_base, &header.encode())
                .map_err(|_| VolumeError::Driver)?;
            if let Some(slot) = self.started.get_mut(block_index) {
                *slot = true;
            }
        }
        self.dev
            .program(absolute, raw.get(..len).ok_or(VolumeError::Driver)?)
            .map_err(|_| VolumeError::Driver)?;
        if let Some(slot) = self
            .free
            .get_mut(usize::try_from(self.current_block).unwrap_or(usize::MAX))
        {
            *slot = false;
        }
        self.cursor = self.cursor.saturating_add(u32::try_from(len).unwrap_or(0));
        self.index_clean = false;
        Ok(())
    }

    /// 创建目录并持久化目录项。
    ///
    /// # Errors
    ///
    /// 父目录/名字错误、空间不足。
    pub fn mkdir(&mut self, parent: u32, name: &[u8]) -> Result<u32, VolumeError> {
        let inode = self.namespace.mkdir(parent, name)?;
        self.append_dirent(parent, name, inode, InodeKind::Directory)?;
        Ok(inode)
    }

    /// 创建普通文件并持久化目录项（当前不含内容）。
    ///
    /// # Errors
    ///
    /// 父目录/名字错误、空间不足。
    pub fn create_file(&mut self, parent: u32, name: &[u8]) -> Result<u32, VolumeError> {
        let inode = self.namespace.create_file(parent, name)?;
        self.append_dirent(parent, name, inode, InodeKind::Data)?;
        Ok(inode)
    }

    /// 创建日志文件并持久化目录项。
    ///
    /// # Errors
    ///
    /// 父目录/名字错误、空间不足。
    pub fn create_log(&mut self, parent: u32, name: &[u8]) -> Result<u32, VolumeError> {
        let inode = self.namespace.create_log(parent, name)?;
        self.append_dirent(parent, name, inode, InodeKind::Log)?;
        Ok(inode)
    }

    /// 增加硬链接。
    ///
    /// # Errors
    ///
    /// inode/父目录/名字错误、空间不足。
    pub fn link(&mut self, inode: u32, parent: u32, name: &[u8]) -> Result<(), VolumeError> {
        self.namespace.link(inode, parent, name)?;
        let kind = self
            .namespace
            .inode_kind(inode)
            .ok_or(VolumeError::Namespace(NamespaceError::NoInode))?;
        self.append_dirent(parent, name, inode, kind)
    }

    /// 列出目录项（只读内存索引，不扫描介质）。
    ///
    /// 回调参数：名字、子 inode、类型。
    pub fn read_dir(
        &self,
        parent: u32,
        callback: &mut dyn FnMut(&[u8], u32, InodeKind),
    ) -> Result<(), VolumeError> {
        if self.namespace.inode_kind(parent) != Some(InodeKind::Directory) {
            return Err(VolumeError::Namespace(NamespaceError::NotDirectory));
        }
        for entry in self.namespace.dir_entries() {
            if entry.parent != parent {
                continue;
            }
            let Some(child) = self.namespace.inode_kind(entry.child) else {
                continue;
            };
            let name = entry
                .name
                .get(..usize::from(entry.name_len))
                .ok_or(VolumeError::Corrupt)?;
            callback(name, entry.child, child);
        }
        Ok(())
    }

    /// 删除空目录（与 unlink 同一持久化路径，额外保证子 inode 是目录）。
    ///
    /// # Errors
    ///
    /// 不是目录、目录非空、空间不足。
    pub fn rmdir(&mut self, parent: u32, name: &[u8]) -> Result<u32, VolumeError> {
        let inode = self
            .namespace
            .lookup(parent, name)
            .ok_or(VolumeError::Namespace(NamespaceError::NotFound))?;
        if self.namespace.inode_kind(inode) != Some(InodeKind::Directory) {
            return Err(VolumeError::Namespace(NamespaceError::NotDirectory));
        }
        self.unlink(parent, name)
    }

    /// 删除目录项。
    ///
    /// # Errors
    ///
    /// 目录项不存在/目录非空、空间不足。
    pub fn unlink(&mut self, parent: u32, name: &[u8]) -> Result<u32, VolumeError> {
        let child = self.namespace.unlink(parent, name)?;
        self.append(FLAG_TOMBSTONE, parent, name, &[])?;
        Ok(child)
    }

    /// 改名：持久化为旧名墓碑 + 新名目录项（顺序记录，掉电恢复见 ADR-0009）。
    ///
    /// # Errors
    ///
    /// 源/目标错误、空间不足。
    pub fn rename(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
    ) -> Result<(), VolumeError> {
        if self.namespace.lookup(old_parent, old_name).is_none() {
            return Err(VolumeError::Namespace(NamespaceError::NotFound));
        }
        let mut payload = [0u8; 36];
        payload
            .get_mut(..4)
            .ok_or(VolumeError::Driver)?
            .copy_from_slice(&new_parent.to_le_bytes());
        if let Some(len) = payload.get_mut(4) {
            *len = u8::try_from(new_name.len())
                .map_err(|_| VolumeError::Namespace(NamespaceError::BadName))?;
        }
        payload
            .get_mut(5..5usize.saturating_add(new_name.len()))
            .ok_or(VolumeError::TooLarge)?
            .copy_from_slice(new_name);
        let payload = payload
            .get(..5usize.saturating_add(new_name.len()))
            .ok_or(VolumeError::TooLarge)?;
        self.append(FLAG_RENAME_INTENT, old_parent, old_name, payload)?;
        self.append(FLAG_RENAME_COMMIT, old_parent, old_name, payload)?;
        self.namespace
            .rename(old_parent, old_name, new_parent, new_name)?;
        Ok(())
    }

    /// 解析绝对路径。
    #[must_use]
    pub fn resolve(&self, path: &[u8]) -> Option<u32> {
        self.namespace.resolve(path)
    }

    fn append_dirent(
        &mut self,
        parent: u32,
        name: &[u8],
        child: u32,
        kind: InodeKind,
    ) -> Result<(), VolumeError> {
        let mut data = [0u8; 5];
        data.get_mut(..4)
            .ok_or(VolumeError::Driver)?
            .copy_from_slice(&child.to_le_bytes());
        if let Some(slot) = data.get_mut(4) {
            *slot = match kind {
                InodeKind::Data => 0,
                InodeKind::Log => 1,
                InodeKind::Directory => 2,
            };
        }
        self.append(FLAG_DIRENT, parent, name, &data)
    }
}

fn read_block_header<D: BlockDevice>(
    dev: &mut D,
    block_size: u32,
    block: u32,
) -> Result<VolumeBlockHeader, VolumeError> {
    let mut raw = [0u8; VOLUME_BLOCK_HEADER_LEN];
    let base = block.checked_mul(block_size).ok_or(VolumeError::Driver)?;
    dev.read(base, &mut raw).map_err(|_| VolumeError::Driver)?;
    VolumeBlockHeader::decode(&raw).map_err(|_| VolumeError::Driver)
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(data.get(at..end)?.try_into().ok()?))
}

fn kind_from_code(code: u8) -> Option<InodeKind> {
    match code {
        0 => Some(InodeKind::Data),
        1 => Some(InodeKind::Log),
        2 => Some(InodeKind::Directory),
        _ => None,
    }
}

/// 目录项名字上限（重新导出，便于测试/上层不依赖 layout）。
pub const MAX_NAME: usize = MAX_NAME_LEN;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockDevice;
    use crate::namespace::ROOT_INODE;

    const BS: u32 = 4096;
    const BC: u32 = 8;
    const CAP: usize = 32768; // 4096 B × 8 块；const 上下文里不能 try_from

    type Dev = MockDevice<CAP>;
    type Vol = Volume<Dev, 16, 32>;

    fn fresh() -> Vol {
        Vol::format(MockDevice::new(BS, BC)).unwrap()
    }

    fn remount(vol: Vol) -> Vol {
        let bytes = *vol.device().raw();
        Vol::mount(MockDevice::from_bytes(bytes, BS, BC)).unwrap()
    }

    #[test]
    fn 目录项挂载后保留() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let _ = vol.create_file(d, b"f").unwrap();
        let vol = remount(vol);
        assert!(vol.resolve(b"/d").is_some());
        assert_eq!(vol.resolve(b"/d").unwrap(), d);
        assert!(vol.resolve(b"/d/f").is_some());
    }

    #[test]
    fn 硬链接与_unlink_跨复位一致() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        vol.link(f, ROOT_INODE, b"g").unwrap();
        let mut vol = remount(vol);
        assert_eq!(vol.resolve(b"/g").unwrap(), f);
        assert_eq!(vol.namespace().link_count(f), Some(2));
        vol.unlink(ROOT_INODE, b"g").unwrap();
        let vol = remount(vol);
        assert_eq!(vol.namespace().link_count(f), Some(1));
        assert!(vol.resolve(b"/d/f").is_some());
    }

    #[test]
    fn 文件内容跨复位保留且取最新版本() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"f").unwrap();
        vol.write_inode(f, b"old").unwrap();
        vol.write_inode(f, b"newer").unwrap();
        let mut vol = remount(vol);
        let mut out = [0u8; 16];
        assert_eq!(vol.read_file(b"/f", &mut out).unwrap(), 5);
        assert_eq!(&out[..5], b"newer");
    }
    #[test]
    fn 大内容分块跨复位() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"big").unwrap();
        let mut input = [0u8; 600];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).unwrap_or(0);
        }
        vol.write_inode(f, &input).unwrap();
        let mut vol = remount(vol);
        let mut out = [0u8; 1024];
        let len = vol.read_file(b"/big", &mut out).unwrap();
        assert_eq!(len, input.len());
        assert_eq!(out.get(..len), Some(&input[..]));
    }

    #[test]
    fn checkpoint_后挂载走索引快照() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let _ = vol.create_file(d, b"f").unwrap();
        vol.checkpoint().unwrap();
        let vol = remount(vol);
        assert!(vol.resolve(b"/d/f").is_some());
    }

    #[test]
    fn read_dir_与_rmdir() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let _ = vol.create_file(d, b"f").unwrap();
        let mut names = [0u8; 8];
        let mut count = 0usize;
        vol.read_dir(d, &mut |name, _, _| {
            if let Some(slot) = names.get_mut(count..count + name.len()) {
                slot.copy_from_slice(name);
            }
            count += 1;
        })
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(names.get(..1), Some(&b"f"[..]));
        assert!(vol.rmdir(ROOT_INODE, b"d").is_err());
        vol.unlink(d, b"f").unwrap();
        assert!(vol.rmdir(ROOT_INODE, b"d").is_ok());
    }

    #[test]
    fn 按路径流式读写() {
        let mut vol = fresh();
        vol.create_file(ROOT_INODE, b"p").unwrap();
        let source = [0x19u8; 300];
        vol.write_file_stream(b"/p", 3, |index, chunk| {
            let start = index.saturating_mul(DATA_CHUNK);
            let end = (start + DATA_CHUNK).min(source.len());
            let len = end.saturating_sub(start);
            chunk
                .get_mut(..len)
                .ok_or(VolumeError::BufferTooSmall)?
                .copy_from_slice(source.get(start..end).ok_or(VolumeError::BufferTooSmall)?);
            Ok(len)
        })
        .unwrap();
        let mut out = [0u8; 64];
        let n = vol.read_file_range(b"/p", 128, &mut out).unwrap();
        assert_eq!(n, 64);
        assert_eq!(out.get(..n), source.get(128..192));
    }

    #[test]
    fn 流式写入不一次性持有整文件() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"stream").unwrap();
        let source = [0x77u8; 600];
        vol.write_inode_stream(f, 5, |index, chunk| {
            let start = index.saturating_mul(DATA_CHUNK);
            let end = (start + DATA_CHUNK).min(source.len());
            let len = end.saturating_sub(start);
            chunk
                .get_mut(..len)
                .ok_or(VolumeError::BufferTooSmall)?
                .copy_from_slice(source.get(start..end).ok_or(VolumeError::BufferTooSmall)?);
            Ok(len)
        })
        .unwrap();
        let mut vol = remount(vol);
        let mut out = [0u8; 1024];
        let len = vol.read_file(b"/stream", &mut out).unwrap();
        assert_eq!(out.get(..len), Some(&source[..]));
    }

    #[test]
    fn 日志追加与读取跨复位() {
        let mut vol = fresh();
        let log = vol.create_log(ROOT_INODE, b"log").unwrap();
        vol.append_log(log, b"alpha").unwrap();
        vol.append_log(log, b"beta").unwrap();
        let mut vol = remount(vol);
        assert_eq!(vol.inode_size(log).unwrap(), 9);
        let mut out = [0u8; 16];
        let n = vol.read_inode(log, &mut out).unwrap();
        assert_eq!(out.get(..n), Some(b"alphabeta".as_slice()));
    }

    #[test]
    fn 稀疏活记录压缩后内容与目录保留() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"keep").unwrap();
        let gone = vol.create_file(ROOT_INODE, b"gone").unwrap();
        vol.write_inode(f, &[0x5A; 300]).unwrap();
        vol.write_inode(gone, &[0x11; 300]).unwrap();
        vol.unlink(ROOT_INODE, b"gone").unwrap();
        let copied = vol.compact().unwrap();
        assert!(copied > 0);
        let mut vol = remount(vol);
        assert!(vol.resolve(b"/keep").is_some());
        assert!(vol.resolve(b"/gone").is_none());
        let mut out = [0u8; 512];
        assert_eq!(vol.read_file(b"/keep", &mut out).unwrap(), 300);
        assert_eq!(out.get(..300), Some(&[0x5A; 300][..]));
    }

    #[test]
    fn 文件大小按最新版本计算() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"size").unwrap();
        let input = [0x42u8; 300];
        vol.write_inode(f, &input).unwrap();
        assert_eq!(vol.inode_size(f).unwrap(), input.len());
    }

    #[test]
    fn 流式区间读取不构造整文件() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"range").unwrap();
        let mut input = [0u8; 600];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).unwrap_or(0);
        }
        vol.write_inode(f, &input).unwrap();
        let mut out = [0u8; 64];
        let n = vol.read_inode_range(f, 100, &mut out).unwrap();
        assert_eq!(n, 64);
        assert_eq!(out.get(..n), input.get(100..164));
    }

    #[test]
    fn 八_kib_内容跨块分块() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"big8").unwrap();
        let mut input = [0u8; 5000];
        for (index, byte) in input.iter_mut().enumerate() {
            *byte = u8::try_from(index % 241).unwrap_or(0);
        }
        vol.write_inode(f, &input).unwrap();
        let mut vol = remount(vol);
        let mut out = [0u8; 8192];
        let len = vol.read_file(b"/big8", &mut out).unwrap();
        assert_eq!(len, input.len());
        assert_eq!(out.get(..len), Some(&input[..]));
    }

    #[test]
    fn gc_回收只有旧版本数据的块() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"f").unwrap();
        let mut old = [0u8; 600];
        let mut new = [0u8; 600];
        old.fill(0x11);
        new.fill(0x22);
        // 用墓碑把文件内容推到独立的块：block0 保留目录项，block1 放旧内容。
        while vol.current_block == 0 {
            vol.append(FLAG_TOMBSTONE, ROOT_INODE, b"fill", &[])
                .unwrap();
        }
        vol.write_inode(f, &old).unwrap();
        while vol.current_block == 1 {
            vol.append(FLAG_TOMBSTONE, ROOT_INODE, b"fill", &[])
                .unwrap();
        }
        vol.write_inode(f, &new).unwrap();
        let reclaimed = vol.gc_dead_blocks().unwrap();
        assert!(reclaimed >= 1);
        let mut out = [0u8; 1024];
        assert_eq!(vol.read_inode(f, &mut out).unwrap(), new.len());
        assert_eq!(out.get(..new.len()), Some(&new[..]));
    }

    #[test]
    fn 掉电截断后挂载不崩溃且前缀自洽() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        let mut content = [0u8; 600];
        content.fill(0x5A);
        vol.write_inode(f, &content).unwrap();
        vol.rename(d, b"f", ROOT_INODE, b"g").unwrap();
        let raw = *vol.device().raw();
        let mut cut = 16usize;
        while cut < raw.len() {
            let mut damaged = raw;
            for byte in damaged.get_mut(cut..).unwrap_or(&mut []) {
                *byte = 0xFF;
            }
            let device = MockDevice::from_bytes(damaged, BS, BC);
            if let Ok(mut mounted) = Vol::mount(device) {
                // 能挂载就必须能读目录；不能出现 half-restored panic。
                let _ = mounted.resolve(b"/g");
                let _ = mounted.resolve(b"/d/f");
                let mut out = [0u8; 1024];
                let _ = mounted.read_inode(f, &mut out);
            }
            cut = cut.saturating_add(64);
        }
    }

    #[test]
    fn 掉电逐记录边界测试() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        let mut content = [0u8; 600];
        content.fill(0x3C);
        vol.write_inode(f, &content).unwrap();
        vol.rename(d, b"f", ROOT_INODE, b"g").unwrap();
        let raw = *vol.device().raw();
        let block_size = usize::try_from(BS).unwrap_or(0);
        let data_blocks = usize::try_from(BC.saturating_sub(1)).unwrap_or(0);
        let mut cuts = [0usize; 128];
        let mut cut_count = 0usize;
        for block in 0..data_blocks {
            let base = block.saturating_mul(block_size);
            let Some(header) = raw.get(base..base + VOLUME_BLOCK_HEADER_LEN) else {
                continue;
            };
            if VolumeBlockHeader::decode(header).is_err() {
                continue;
            }
            let mut offset = VOLUME_BLOCK_HEADER_LEN;
            loop {
                let absolute = base.saturating_add(offset);
                let Some(record_header) = raw.get(absolute..absolute + 24) else {
                    break;
                };
                if record_header.iter().all(|byte| *byte == 0xFF) {
                    break;
                }
                let Some(header) = VolumeRecordHeader::decode(record_header) else {
                    break;
                };
                if let Some(slot) = cuts.get_mut(cut_count) {
                    *slot = absolute;
                    cut_count += 1;
                }
                let total = 24usize
                    .saturating_add(usize::from(header.name_len))
                    .saturating_add(usize::from(header.data_len));
                offset = offset.saturating_add(total);
            }
        }
        for cut in cuts.iter().take(cut_count) {
            let mut damaged = raw;
            for byte in damaged.get_mut(*cut..).unwrap_or(&mut []) {
                *byte = 0xFF;
            }
            if let Ok(mut mounted) = Vol::mount(MockDevice::from_bytes(damaged, BS, BC)) {
                let _ = mounted.resolve(b"/g");
                let mut out = [0u8; 1024];
                let _ = mounted.read_inode(f, &mut out);
            }
        }
    }

    #[test]
    fn gc_回收只有墓碑的块() {
        let mut vol = fresh();
        vol.append(FLAG_TOMBSTONE, 1, b"x", &[]).unwrap();
        assert_eq!(vol.gc_dead_blocks().unwrap(), 1);
    }

    #[test]
    fn compact_分片推进大容量介质() {
        type BigDev = MockDevice<262144>;
        type BigVol = Volume<BigDev, 16, 32>;
        let mut vol = BigVol::format(BigDev::new(BS, 64)).unwrap();
        let f = vol.create_file(ROOT_INODE, b"f").unwrap();
        let mut content = [0u8; 600];
        content.fill(0x3C);
        vol.write_inode(f, &content).unwrap();
        let total = vol.data_block_count();
        let mut done = false;
        while !done {
            let (_, ended) = vol.compact_step(1).unwrap();
            done = ended;
            assert!(vol.compact_cursor <= total);
        }
        let mut out = [0u8; 1024];
        assert_eq!(vol.read_inode(f, &mut out).unwrap(), content.len());
    }

    #[test]
    fn compact_分片推进不破坏文件() {
        let mut vol = fresh();
        let f = vol.create_file(ROOT_INODE, b"f").unwrap();
        let mut content = [0u8; 600];
        content.fill(0x3C);
        vol.write_inode(f, &content).unwrap();
        let total = vol.data_block_count();
        let mut next = 0u32;
        while next < total {
            let (_, done) = vol.compact_step(1).unwrap();
            next = next.saturating_add(1);
            if done {
                break;
            }
        }
        let mut out = [0u8; 1024];
        assert_eq!(vol.read_inode(f, &mut out).unwrap(), content.len());
        assert_eq!(out.get(..content.len()), Some(&content[..]));
    }

    #[test]
    fn gc_分片推进覆盖全部数据块() {
        let mut vol = fresh();
        vol.append(FLAG_TOMBSTONE, 1, b"x", &[]).unwrap();
        let total = vol.data_block_count();
        let mut next = 0u32;
        let mut reclaimed = 0u32;
        while next < total {
            let (count, advanced) = vol.gc_dead_blocks_step(next, 1).unwrap();
            reclaimed = reclaimed.saturating_add(count);
            assert_eq!(advanced, next.saturating_add(1));
            next = advanced;
        }
        assert_eq!(reclaimed, 1);
    }

    #[test]
    fn checkpoint_后写入使索引失效并回退扫描() {
        let mut vol = fresh();
        let _ = vol.mkdir(ROOT_INODE, b"d").unwrap();
        vol.checkpoint().unwrap();
        let f = vol.create_file(ROOT_INODE, b"new").unwrap();
        let vol = remount(vol);
        assert_eq!(vol.resolve(b"/new"), Some(f));
    }

    #[test]
    fn rename_只有_intent_没有_commit_时旧名保留() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        let mut payload = [0u8; 36];
        payload
            .get_mut(..4)
            .unwrap()
            .copy_from_slice(&ROOT_INODE.to_le_bytes());
        payload[4] = 1;
        payload[5] = b'g';
        vol.append(FLAG_RENAME_INTENT, d, b"f", &payload[..6])
            .unwrap();
        let vol = remount(vol);
        assert_eq!(vol.resolve(b"/d/f").unwrap(), f);
        assert!(vol.resolve(b"/g").is_none());
    }

    #[test]
    fn rename_跨复位只改目录项() {
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        vol.rename(d, b"f", ROOT_INODE, b"g").unwrap();
        let vol = remount(vol);
        assert_eq!(vol.resolve(b"/g").unwrap(), f);
        assert!(vol.resolve(b"/d/f").is_none());
    }
    #[test]
    fn 错误路径与容量分支() {
        let mut vol = fresh();
        assert!(vol.write_inode(999, b"x").is_err());
        assert!(vol.append_log(999, b"x").is_err());
        assert!(vol.inode_size(999).is_err());
        let log = vol.create_log(ROOT_INODE, b"l").unwrap();
        assert!(vol.append_log(log, b"").is_err());
        let big = [0u8; 513];
        assert!(vol.append_log(log, &big).is_err());
        let f = vol.create_file(ROOT_INODE, b"f").unwrap();
        assert!(vol.write_inode(f, b"").is_err());
        let mut out = [0u8; 4];
        assert!(vol.read_inode_range(f, 0, &mut out).is_err());
        assert!(vol.write_inode_stream(f, 0, |_, _| Ok(1)).is_err());
        assert!(vol.write_inode_stream(f, 513, |_, _| Ok(1)).is_err());
        assert!(vol.write_inode_stream(f, 1, |_, _| Ok(0)).is_err());
        assert!(vol.write_inode_stream(f, 1, |_, _| Ok(129)).is_err());
        assert!(vol.mkdir(ROOT_INODE, b"f").is_err());
        assert!(vol.create_file(ROOT_INODE, b"f").is_err());
        assert!(vol.link(999, ROOT_INODE, b"x").is_err());
        assert!(vol.unlink(ROOT_INODE, b"missing").is_err());
        assert!(
            vol.rename(ROOT_INODE, b"missing", ROOT_INODE, b"x")
                .is_err()
        );
        assert!(vol.read_dir(f, &mut |_, _, _| {}).is_err());
        assert!(vol.rmdir(ROOT_INODE, b"f").is_err());
        assert!(vol.resolve(b"relative").is_none());
        assert!(vol.block_usage().1 > 0);
        assert!(vol.compact().is_ok());
        assert!(vol.gc_dead_blocks().is_ok());
        assert!(vol.checkpoint().is_ok());
    }
    #[test]
    fn 卷层错误与索引分支() {
        assert!(Volume::<MockDevice<64>, 4, 4>::mount(MockDevice::new(32, 1)).is_err());
        assert!(Volume::<MockDevice<64>, 4, 4>::format(MockDevice::new(32, 1)).is_err());
        let mut vol = fresh();
        let d = vol.mkdir(ROOT_INODE, b"d").unwrap();
        let f = vol.create_file(d, b"f").unwrap();
        vol.write_inode(f, b"x").unwrap();
        let l = vol.create_log(d, b"l").unwrap();
        vol.append_log(l, b"y").unwrap();
        vol.link(f, d, b"g").unwrap();
        vol.rename(d, b"g", d, b"h").unwrap();
        let mut names = 0usize;
        vol.read_dir(d, &mut |_, _, _| names += 1).unwrap();
        assert!(names >= 2);
        vol.checkpoint().unwrap();
        let vol = remount(vol);
        assert!(vol.resolve(b"/d/f").is_some());
        assert_eq!(vol.namespace().entry_count(), 4);
        assert!(vol.namespace().inodes().count() >= 3);
    }
}
