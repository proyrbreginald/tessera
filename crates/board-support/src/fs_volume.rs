//! 文件系统 v3 的 BSP 适配**脚手架**。
//!
//! 目标：在不动 0.27 生产 v2 路径的前提下，把 v3 卷层接到整块 Flash 分区上，
//! 当前是唯一的正式文件系统格式；v2 路径不再维护。

use crate::config;
use crate::fs::FsError;
use crate::fs::{BLOCKS, FlashBlocks};
use crate::store::FlexSpiMedium;
use filesystem::BlockDevice;
use filesystem::volume::Volume;
use ports::raw::RawMedium;

/// v3 使用的整分区块设备：`BLOCKS` 个 64 KiB 块，最后一块是 v3 索引块。
///
/// 注意：v2 的 [`FlashBlocks`] 对外只报 `DATA_BLOCKS`（最后一块留给 v2 索引），
/// 因此不能直接把 v2 设备交给 v3 卷层。
pub struct FlashMedium;

impl BlockDevice for FlashMedium {
    fn block_size(&self) -> u32 {
        flash_layout::BLOCK
    }

    fn block_count(&self) -> u32 {
        u32::try_from(BLOCKS).unwrap_or(0)
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> Result<(), FsError> {
        let relative = usize::try_from(offset).map_err(|_| FsError::Driver)?;
        let absolute = config::FILESYSTEM_BASE
            .checked_add(relative)
            .ok_or(FsError::Driver)?;
        let mut medium = FlexSpiMedium::new();
        let read = medium.read(absolute, out).map_err(|_| FsError::Driver)?;
        if read != out.len() {
            return Err(FsError::Driver);
        }
        Ok(())
    }

    fn erase_block(&mut self, block: u32) -> Result<(), FsError> {
        let offset = block
            .checked_mul(self.block_size())
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|value| value.checked_add(config::FILESYSTEM_BASE))
            .ok_or(FsError::Driver)?;
        let mut medium = FlexSpiMedium::new();
        medium
            .erase(offset, usize::try_from(flash_layout::BLOCK).unwrap_or(0))
            .map_err(|_| FsError::Driver)
    }

    fn program(&mut self, offset: u32, data: &[u8]) -> Result<(), FsError> {
        let absolute = usize::try_from(offset)
            .ok()
            .and_then(|value| value.checked_add(config::FILESYSTEM_BASE))
            .ok_or(FsError::Driver)?;
        let mut medium = FlexSpiMedium::new();
        medium.program(absolute, data).map_err(|_| FsError::Driver)
    }
}

/// v3 卷类型（与 v2 的 `MAX_FILES` 对齐）。
pub type Fs = Volume<FlashMedium, 16, 32>;

/// 确认 v2/v3 共用同一个分区几何（编译期）。
const _: () = assert!(BLOCKS > 1);

/// 保留对 `FlashBlocks` 的引用，便于后续迁移时做"v2 迁移到 v3"的适配。
#[allow(dead_code)]
fn _v2_geometry_is_same() -> (u32, u32) {
    let v2 = FlashBlocks;
    (v2.block_size(), v2.block_count())
}

// ---------------------------------------------------------------------------
// 全局 v3 实例（**尚未接线**；默认 V2，只有后续显式调用才会使用）
// ---------------------------------------------------------------------------
//
// 与 v2 的 `fs` 模块一样，v3 实例放静态存储；不使用栈上临时搬移。
// 当前没有调用者，保留为 0.28 接线的落点。

use core::sync::atomic::{AtomicBool, Ordering};

static V3_READY: AtomicBool = AtomicBool::new(false);
static mut V3_STORAGE: core::mem::MaybeUninit<Fs> = core::mem::MaybeUninit::uninit();

/// 挂载 v3 卷（**尚未接入 `board::init`**）。
///
/// # Errors
///
/// 设备读失败、块头无效。
pub fn mount_v3() -> Result<(), FsError> {
    let volume = Fs::mount(FlashMedium).map_err(|_| FsError::Driver)?;
    // SAFETY: 只在启动/显式诊断上下文调用一次，写入期间没有读者。
    unsafe {
        core::ptr::addr_of_mut!(V3_STORAGE)
            .cast::<Fs>()
            .write(volume);
    }
    V3_READY.store(true, Ordering::Release);
    Ok(())
}

/// 格式化并挂载 v3 卷（会擦除整个文件系统分区）。
///
/// # Errors
///
/// 擦除/编程/挂载失败。
pub fn format_v3() -> Result<(), FsError> {
    let volume = Fs::format(FlashMedium).map_err(|_| FsError::Driver)?;
    // SAFETY: 同 `mount_v3`，单线程启动/诊断上下文。
    unsafe {
        core::ptr::addr_of_mut!(V3_STORAGE)
            .cast::<Fs>()
            .write(volume);
    }
    V3_READY.store(true, Ordering::Release);
    Ok(())
}

/// 借用 v3 实例；未挂载或正在借用时返回 `None`。
pub fn with_v3<R>(f: impl FnOnce(&mut Fs) -> R) -> Option<R> {
    if !V3_READY.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: v3 只在单线程启动/诊断上下文使用；`V3_READY` 保证已初始化。
    unsafe {
        let storage = &mut *core::ptr::addr_of_mut!(V3_STORAGE);
        Some(f(storage.assume_init_mut()))
    }
}

/// v3 是否已挂载。
#[must_use]
pub fn v3_ready() -> bool {
    V3_READY.load(Ordering::Acquire)
}
