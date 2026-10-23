//! 擦写范围的校验：**这是本 crate 存在的主要理由**。
//!
//! 三种错各有各的灾难后果：
//! - 擦除不对齐 ⇒ 器件行为未定义；
//! - 页编程跨页 ⇒ 数据**回卷到页首**，静默破坏该页开头（"写进去的值不是我想写的"）；
//! - 越界（含碰到启动头/只读分区）⇒ 写坏启动头就只能靠 BOOT_MODE 引脚救。

use crate::geometry::{PAGE_BYTES, SECTOR_BYTES};
use flash_layout::{MAP, Purpose, Region, XIP_WINDOW};

/// 校验失败的原因（全部是**可判定的值**，没有"未定义"这一档）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlashError {
    /// 起始偏移不是扇区/页边界的整数倍。
    NotAligned,
    /// 长度为 0。
    Empty,
    /// 超出可寻址窗口或分区边界。
    OutOfRange,
    /// 请求落在**只读**分区（例如启动头）里。
    ReadOnly,
    /// 页编程请求超过一页（**不能跨页**）。
    SpansPage,
}

impl FlashError {
    /// 给人看的一句话（终端/日志用）。
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NotAligned => "起始偏移未对齐（擦除 4 KiB / 编程 256 B）",
            Self::Empty => "长度为 0",
            Self::OutOfRange => "超出可寻址窗口或分区边界",
            Self::ReadOnly => "落在只读分区（例如启动头）——绝不允许写",
            Self::SpansPage => "页编程跨页（器件会回卷到页首，静默破坏该页开头）",
        }
    }
}

/// 某段范围落在哪个分区（用于"只读/可写"判定）。
fn partition_of(offset: u32) -> Option<&'static Region> {
    MAP.iter()
        .find(|region| offset >= region.offset && offset < region.offset + region.size)
}

/// 校验一次**扇区擦除**请求：`offset` 必须 4 KiB 对齐，范围必须完整落在**同一个可写分区**内。
///
/// # Errors
///
/// 见 [`FlashError`]。
pub fn check_erase(offset: u32, len: u32) -> Result<(), FlashError> {
    if len == 0 {
        return Err(FlashError::Empty);
    }
    if !offset.is_multiple_of(SECTOR_BYTES) || !len.is_multiple_of(SECTOR_BYTES) {
        return Err(FlashError::NotAligned);
    }
    check_writable(offset, len)
}

/// 校验一次**页编程**请求：范围必须落在**同一页**内且不越界。
///
/// 页内偏移可以不是 0（允许写在页中间），但**不能跨页** —— 器件跨页会回卷。
///
/// # Errors
///
/// 见 [`FlashError`]。
pub fn check_program(offset: u32, len: u32) -> Result<(), FlashError> {
    if len == 0 {
        return Err(FlashError::Empty);
    }
    if len > PAGE_BYTES {
        return Err(FlashError::SpansPage);
    }
    // 跨页判据：起点所在页的页首 + 页大小 ≥ 起点 + 长度。
    let page_start = offset - (offset % PAGE_BYTES);
    let page_end = page_start.saturating_add(PAGE_BYTES);
    if offset.saturating_add(len) > page_end {
        return Err(FlashError::SpansPage);
    }
    check_writable(offset, len)
}

/// 公共的可写性校验：在可寻址窗口内、落在**一个**可写分区内、且不越出该分区。
fn check_writable(offset: u32, len: u32) -> Result<(), FlashError> {
    let end = offset.checked_add(len).ok_or(FlashError::OutOfRange)?;
    if end > XIP_WINDOW {
        return Err(FlashError::OutOfRange);
    }
    // 起点与**终点前一个字节**必须同属一个分区：否则这次擦写横跨两个用途不同的区域，
    // 哪怕每个字节都在"某个分区内"也不能放过。
    let Some(start_region) = partition_of(offset) else {
        return Err(FlashError::OutOfRange);
    };
    let Some(end_region) = partition_of(end - 1) else {
        return Err(FlashError::OutOfRange);
    };
    if start_region.offset != end_region.offset {
        return Err(FlashError::OutOfRange);
    }
    if is_app_writable(start_region.purpose) {
        Ok(())
    } else {
        Err(FlashError::ReadOnly)
    }
}

/// 应用（运行期固件）是否**允许**写这个分区。
///
/// # 为什么是显式白名单而不是"除了只读都可写"
///
/// 这个 match **故意不写 `_` 分支**：`Purpose` 将来新增一个分区时，编译器会强迫编写者
/// 在这里表态"它可不可写"。反过来（用"默认可写"）意味着**新增分区默认获得写权限** ——
/// 对一个"写坏启动头就只能靠引脚救"的系统来说，这个默认值方向是错的。
const fn is_app_writable(purpose: Purpose) -> bool {
    match purpose {
        // 应用数据：一整块文件系统，唯一的常规写入目标。
        Purpose::Filesystem => true,
        // A/B 元数据：升级流程要写（当前槽位/试启次数/已确认标记）。
        Purpose::BootMeta => true,
        // 以下一律**不允许**：
        // - 启动头：Boot ROM 读，运行期一个字都不许写；
        // - 引导程序：只由升级流程按 A/B 规则写非活动槽；
        // - 应用槽位：同上。
        Purpose::BootHeader | Purpose::Bootloader | Purpose::AppSlot => false,
    }
}

/// 一段范围覆盖多少个擦除扇区。
///
/// # Errors
///
/// 长度为 0（没有意义，也防止"0 个扇区"被当成成功）。
pub fn sectors_in(len: u32) -> Result<u32, FlashError> {
    if len == 0 || !len.is_multiple_of(SECTOR_BYTES) {
        return Err(FlashError::NotAligned);
    }
    Ok(len / SECTOR_BYTES)
}

/// 一段范围覆盖多少个编程页。
///
/// # Errors
///
/// 长度为 0。
pub fn pages_in(len: u32) -> Result<u32, FlashError> {
    if len == 0 {
        return Err(FlashError::Empty);
    }
    Ok(len.div_ceil(PAGE_BYTES))
}
