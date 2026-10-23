//! v3 文件系统的 BSP 门面：全局实例、属主借用与写队列。
//!
//! 这里不再保留 v2 的路径记录格式；所有调用都落到 [`filesystem::path_fs::PathFs`]
//! 与 [`filesystem::volume::Volume`]。

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use filesystem::path_fs::{FacadeError, PathFs};
use filesystem::volume::{Volume, VolumeError};
use filesystem::write_queue::{Queue, WriteRequest};
pub use filesystem::{FileInfo, FileKind, FsError, FsStats};

use crate::config;
use crate::fs_volume::FlashMedium;

pub use filesystem::write_queue::{
    Kind as WriteKind, MAX_DATA as WRITE_MAX_DATA, MAX_NAME as WRITE_MAX_NAME,
};

/// 文件系统分区块数（64 KiB 块）。
pub const BLOCKS: usize = config::flash_offset(flash_layout::FILESYSTEM_SIZE / flash_layout::BLOCK);
/// v3 数据块数（最后一块为索引块）。
pub const DATA_BLOCKS: usize = BLOCKS - 1;
/// 文件表容量。
pub const MAX_FILES: usize = 16;

/// v3 路径门面实例（BSP 包装，保留终端所需的便捷方法）。
pub struct FlashFs {
    inner: PathFs<FlashMedium, MAX_FILES, 32>,
}

impl FlashFs {
    fn new(inner: PathFs<FlashMedium, MAX_FILES, 32>) -> Self {
        Self { inner }
    }

    /// 索引统计视图。
    pub fn stats_index_only(&mut self) -> FsStats {
        make_stats(self, false)
    }

    /// 统计视图。
    pub fn stats(&mut self) -> FsStats {
        make_stats(self, true)
    }

    /// 显式 GC。
    pub fn gc(&mut self) -> Result<u32, FsError> {
        self.inner
            .volume_mut()
            .gc_dead_blocks()
            .map_err(|_| FsError::Driver)
    }

    /// 删除文件/空目录项。
    pub fn remove(&mut self, path: &str) -> Result<u32, FsError> {
        self.inner.unlink(path.as_bytes()).map_err(facade_error)?;
        Ok(1)
    }

    /// 列目录。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn list_dir(
        &mut self,
        dir: &str,
        callback: &mut dyn FnMut(&str, Option<u32>),
    ) -> Result<(), FsError> {
        let parent = self
            .inner
            .volume()
            .resolve(dir.as_bytes())
            .ok_or(FsError::NotFound)?;
        let mut entries = [None; 32];
        let mut count = 0usize;
        for entry in self.inner.volume().namespace().dir_entries() {
            if entry.parent != parent {
                continue;
            }
            if let Some(slot) = entries.get_mut(count) {
                *slot = Some(entry);
                count += 1;
            }
        }
        for entry in entries.iter().take(count).flatten() {
            let name = entry
                .name
                .get(..usize::from(entry.name_len))
                .ok_or(FsError::Corrupt)?;
            let kind = self
                .inner
                .volume()
                .namespace()
                .inode_kind(entry.child)
                .ok_or(FsError::Corrupt)?;
            let size = if kind == filesystem::namespace::InodeKind::Directory {
                None
            } else {
                Some(
                    u32::try_from(
                        self.inner
                            .volume_mut()
                            .inode_size(entry.child)
                            .map_err(|_| FsError::Driver)?,
                    )
                    .unwrap_or(u32::MAX),
                )
            };
            callback(core::str::from_utf8(name).unwrap_or(""), size);
        }
        Ok(())
    }

    /// 元数据。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn stat(&mut self, path: &str) -> Result<FileInfo, FsError> {
        let (kind, _) = self.inner.stat(path.as_bytes()).ok_or(FsError::NotFound)?;
        let size = self
            .inner
            .file_size(path.as_bytes())
            .map_err(facade_error)?;
        Ok(FileInfo {
            kind: if kind == filesystem::namespace::InodeKind::Log {
                FileKind::Log
            } else {
                FileKind::Data
            },
            size: u32::try_from(size).unwrap_or(u32::MAX),
        })
    }

    /// 区间读取。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn read_range(
        &mut self,
        path: &str,
        offset: u32,
        out: &mut [u8],
    ) -> Result<usize, FsError> {
        self.inner
            .read_file(
                path.as_bytes(),
                usize::try_from(offset).unwrap_or(usize::MAX),
                out,
            )
            .map_err(facade_error)
    }

    fn exists(&self, path: &[u8]) -> bool {
        self.inner.exists(path)
    }
    fn create_file(&mut self, path: &[u8]) -> Result<(), FsError> {
        self.inner.create_file(path).map_err(facade_error)
    }
    fn create_log(&mut self, path: &[u8]) -> Result<(), FsError> {
        self.inner.create_log(path).map_err(facade_error)
    }
    fn write_file(&mut self, path: &[u8], data: &[u8]) -> Result<(), FsError> {
        self.inner.write_file(path, data).map_err(facade_error)
    }
    fn append_log(&mut self, path: &[u8], data: &[u8]) -> Result<(), FsError> {
        self.inner.append_log(path, data).map_err(facade_error)
    }
    fn unlink(&mut self, path: &[u8]) -> Result<(), FsError> {
        self.inner.unlink(path).map_err(facade_error)
    }
    fn volume_mut(&mut self) -> &mut Volume<FlashMedium, MAX_FILES, 32> {
        self.inner.volume_mut()
    }
}

static FS_PTR: AtomicUsize = AtomicUsize::new(0);
static mut FS_STORAGE: core::mem::MaybeUninit<FlashFs> = core::mem::MaybeUninit::uninit();
static FS_BUSY: AtomicBool = AtomicBool::new(false);
static INDEX_LOADED: AtomicBool = AtomicBool::new(false);

const QUEUE_CAPACITY: usize = 8;
static QUEUE: critical_section::Mutex<core::cell::RefCell<Queue<QUEUE_CAPACITY>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(Queue::new()));

/// 安装已挂载的 v3 实例。
pub fn install(fs: FlashFs) {
    // SAFETY: 单线程启动路径写入一次；写完才发布指针。
    let pointer = unsafe {
        let storage = core::ptr::addr_of_mut!(FS_STORAGE);
        storage.cast::<FlashFs>().write(fs);
        #[allow(clippy::as_conversions)]
        let address = storage.cast::<FlashFs>() as usize;
        address
    };
    FS_PTR.store(pointer, Ordering::Release);
}

/// 属主上下文借用。
pub fn with_fs_owner<R>(f: impl FnOnce(&mut FlashFs) -> R) -> Option<R> {
    if FS_BUSY.swap(true, Ordering::AcqRel) {
        return None;
    }
    let pointer = FS_PTR.load(Ordering::Acquire);
    if pointer == 0 {
        FS_BUSY.store(false, Ordering::Release);
        return None;
    }
    // SAFETY: 指针由 install 发布；FS_BUSY 保证独占。
    let fs = unsafe { &mut *core::ptr::with_exposed_provenance_mut::<FlashFs>(pointer) };
    let result = f(fs);
    FS_BUSY.store(false, Ordering::Release);
    Some(result)
}

/// 与属主借用相同（读路径可被中断抢占）。
pub fn with_fs_read<R>(f: impl FnOnce(&mut FlashFs) -> R) -> Option<R> {
    with_fs_owner(f)
}

/// 受限只读视图（关中断窗口内只允许索引统计）。
pub struct FsAccess<'a> {
    fs: &'a mut FlashFs,
}

impl FsAccess<'_> {
    /// 索引统计。
    #[must_use]
    pub fn stats_index_only(&mut self) -> FsStats {
        self.fs.stats_index_only()
    }

    /// 列目录。
    pub fn list_dir(
        &mut self,
        dir: &str,
        callback: &mut dyn FnMut(&str, Option<u32>),
    ) -> Result<(), FsError> {
        self.fs.list_dir(dir, callback)
    }

    /// 文件元数据。
    pub fn stat(&mut self, path: &str) -> Result<FileInfo, FsError> {
        self.fs.stat(path)
    }

    /// 区间读取。
    pub fn read_range(
        &mut self,
        path: &str,
        offset: u32,
        out: &mut [u8],
    ) -> Result<usize, FsError> {
        self.fs.read_range(path, offset, out)
    }
}

/// 关中断只读访问。
pub fn with_fs<R>(f: impl FnOnce(&mut FsAccess<'_>) -> R) -> Option<R> {
    with_fs_owner(|fs| f(&mut FsAccess { fs }))
}

/// v3 是否已安装。
#[must_use]
pub fn is_installed() -> bool {
    FS_PTR.load(Ordering::Acquire) != 0
}

/// 挂载 v3 卷。
///
/// # Errors
///
/// 设备读失败或块头无效。
pub fn mount() -> Result<FlashFs, FsError> {
    // 没有 v2 兼容数据：空片/旧格式都直接格式化成 v3。
    let volume = match Volume::mount(FlashMedium) {
        Ok(volume) => {
            INDEX_LOADED.store(true, Ordering::Release);
            volume
        }
        Err(_) => {
            INDEX_LOADED.store(false, Ordering::Release);
            Volume::format(FlashMedium).map_err(|_| FsError::Driver)?
        }
    };
    INDEX_LOADED.store(volume.index_loaded(), Ordering::Release);
    let mut path_fs = PathFs::new(volume);
    // v3 没有隐含目录；/log 是日志归档的父目录，挂载时确保存在。
    let _ = path_fs.mkdir(b"/log");
    // 只有全盘扫描/空片格式化后才写一次索引，索引挂载路径不再重写。
    if !INDEX_LOADED.load(Ordering::Acquire) {
        let _ = path_fs.volume_mut().checkpoint();
    }
    Ok(FlashFs::new(path_fs))
}

/// 本次是否从索引快照挂载（v3 目前由 Volume 内部判断，这里只给诊断用）。
#[must_use]
pub fn index_loaded() -> bool {
    INDEX_LOADED.load(Ordering::Acquire)
}

/// 写 checkpoint。
#[must_use]
pub fn checkpoint_index() -> bool {
    with_fs_owner(|fs| fs.volume_mut().checkpoint().is_ok()).unwrap_or(false)
}

/// 当前空闲块数（属主上下文；取不到返回 0）。
#[must_use]
pub fn free_blocks() -> u32 {
    with_fs_owner(|fs| fs.inner.volume().block_usage().0).unwrap_or(0)
}

/// 写路径为 GC 保留的空闲块数。
#[must_use]
pub fn reserved_blocks() -> u32 {
    with_fs_owner(|fs| fs.inner.volume().reserved_blocks()).unwrap_or(0)
}

/// 分片回收死块；返回 `(reclaimed, next)`。
pub fn gc_dead_step(start: u32, max_blocks: u32) -> Option<Result<(u32, u32), FsError>> {
    with_fs_owner(|fs| {
        fs.inner
            .volume_mut()
            .gc_dead_blocks_step(start, max_blocks)
            .map_err(|_| FsError::Driver)
    })
}

/// 分片搬移压缩：返回 `(复制记录数, 是否完成)`。
pub fn compact_step(max_blocks: u32) -> Option<Result<(u32, bool), FsError>> {
    with_fs_owner(|fs| {
        fs.inner
            .volume_mut()
            .compact_step(max_blocks)
            .map_err(|error| match error {
                VolumeError::Full => FsError::NoSpace,
                _ => FsError::Driver,
            })
    })
}

/// 数据块总数。
#[must_use]
pub fn data_blocks() -> u32 {
    with_fs_owner(|fs| fs.inner.volume().data_block_count()).unwrap_or(0)
}

/// 创建目录。
/// 创建目录。
#[must_use]
pub fn mkdir(path: &str) -> bool {
    with_fs_owner(|fs| fs.inner.mkdir(path.as_bytes()).is_ok()).unwrap_or(false)
}

/// 删除空目录。
#[must_use]
pub fn rmdir(path: &str) -> bool {
    with_fs_owner(|fs| fs.inner.rmdir(path.as_bytes()).is_ok()).unwrap_or(false)
}

/// 改名。
#[must_use]
pub fn rename(old: &str, new: &str) -> bool {
    with_fs_owner(|fs| fs.inner.rename(old.as_bytes(), new.as_bytes()).is_ok()).unwrap_or(false)
}

/// 创建硬链接。
#[must_use]
pub fn link(existing: &str, new: &str) -> bool {
    with_fs_owner(|fs| fs.inner.link(existing.as_bytes(), new.as_bytes()).is_ok()).unwrap_or(false)
}

/// 删除文件/空目录项。
#[must_use]
pub fn unlink(path: &str) -> bool {
    with_fs_owner(|fs| fs.inner.unlink(path.as_bytes()).is_ok()).unwrap_or(false)
}

/// 队列统计。
/// 队列统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueStats {
    /// 待处理。
    pub pending: usize,
    /// 已投递。
    pub submitted: u64,
    /// 已执行。
    pub executed: u64,
    /// 失败。
    pub failed: u64,
    /// 拒绝。
    pub rejected: u64,
    /// 超长。
    pub too_long: u64,
}

/// 投递一条写请求。
pub fn submit(kind: WriteKind, name: &str, data: &[u8]) -> bool {
    crate::rt::measured_cs(|cs| QUEUE.borrow(cs).borrow_mut().submit_parts(kind, name, data))
}

/// 队列推进结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// 空。
    Idle,
    /// 全部成功。
    Drained {
        /// 成功条数。
        ok: usize,
    },
    /// 有失败。
    DrainedWithFailures {
        /// 成功条数。
        ok: usize,
        /// 失败条数。
        failed: usize,
    },
    /// 文件系统忙/未安装。
    Unavailable,
}

/// 属主推进一步。
pub fn step() -> StepOutcome {
    let pending = crate::rt::measured_cs(|cs| QUEUE.borrow(cs).borrow().pending());
    if pending == 0 {
        return StepOutcome::Idle;
    }
    let mut ok = 0usize;
    let mut failed = 0usize;
    let Some(()) = with_fs_owner(|fs| {
        loop {
            let request = crate::rt::measured_cs(|cs| QUEUE.borrow(cs).borrow_mut().take());
            let Some(request) = request else {
                break;
            };
            let succeeded = execute(fs, &request);
            crate::rt::measured_cs(|cs| QUEUE.borrow(cs).borrow_mut().complete(succeeded));
            if succeeded {
                ok = ok.saturating_add(1);
            } else {
                failed = failed.saturating_add(1);
            }
        }
    }) else {
        return StepOutcome::Unavailable;
    };
    if failed == 0 {
        StepOutcome::Drained { ok }
    } else {
        StepOutcome::DrainedWithFailures { ok, failed }
    }
}

/// 队列统计快照。
#[must_use]
pub fn queue_stats() -> QueueStats {
    crate::rt::measured_cs(|cs| {
        let queue = QUEUE.borrow(cs).borrow();
        QueueStats {
            pending: queue.pending(),
            submitted: queue.submitted(),
            executed: queue.executed(),
            failed: queue.failed(),
            rejected: queue.rejected(),
            too_long: queue.too_long(),
        }
    })
}

fn execute(fs: &mut FlashFs, request: &WriteRequest) -> bool {
    let path = request.name().as_bytes();
    match request.kind() {
        WriteKind::Write => {
            if !fs.exists(path) && fs.create_file(path).is_err() {
                return false;
            }
            fs.write_file(path, request.data()).is_ok()
        }
        WriteKind::Append => {
            if !fs.exists(path) && fs.create_log(path).is_err() {
                return false;
            }
            fs.append_log(path, request.data()).is_ok()
        }
        WriteKind::Remove => fs.unlink(path).is_ok(),
    }
}

fn facade_error(error: FacadeError) -> FsError {
    match error {
        FacadeError::AbsolutePathRequired | FacadeError::BadPath => FsError::NotFound,
        FacadeError::Volume(filesystem::volume::VolumeError::NotFound) => FsError::NotFound,
        _ => FsError::Driver,
    }
}

fn make_stats(fs: &FlashFs, scanned: bool) -> FsStats {
    let file_count =
        u32::try_from(fs.inner.volume().namespace().dir_entries().count()).unwrap_or(0);
    let (free_blocks, used_blocks) = fs.inner.volume().block_usage();
    FsStats {
        block_size: flash_layout::BLOCK,
        block_count: u32::try_from(DATA_BLOCKS).unwrap_or(0),
        free_blocks,
        used_blocks,
        live_bytes: 0,
        dead_bytes: 0,
        file_count,
        table_overflow: 0,
        erase_min: 0,
        erase_max: 0,
        erase_total: 0,
        gc_explicit_runs: 0,
        gc_auto_runs: 0,
        scanned,
        index_dirty: !INDEX_LOADED.load(Ordering::Acquire),
    }
}

/// 保留 v2 设备类型名，供旧文档/迁移代码引用；不参与 v3 生产路径。
pub struct FlashBlocks;
impl filesystem::BlockDevice for FlashBlocks {
    fn block_size(&self) -> u32 {
        flash_layout::BLOCK
    }
    fn block_count(&self) -> u32 {
        u32::try_from(DATA_BLOCKS).unwrap_or(0)
    }
    fn read(&mut self, _offset: u32, _out: &mut [u8]) -> Result<(), FsError> {
        Err(FsError::Driver)
    }
    fn erase_block(&mut self, _block: u32) -> Result<(), FsError> {
        Err(FsError::Driver)
    }
    fn program(&mut self, _offset: u32, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::Driver)
    }
}

/// 文件系统统计（v3 只给索引视图）。
#[must_use]
pub fn stats() -> Option<FsStats> {
    with_fs_owner(|fs| make_stats(fs, false))
}

const _: () = assert!(DATA_BLOCKS > 0);
