//! 文件系统对上层暴露的稳定类型（v3 生产用）。

/// 文件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// 整体覆盖的普通文件。
    Data,
    /// 只追加的日志文件。
    Log,
}

/// `stat` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// 种类。
    pub kind: FileKind,
    /// 当前版本字节数。
    pub size: u32,
}

/// 文件系统统计快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    /// 块大小。
    pub block_size: u32,
    /// 块总数。
    pub block_count: u32,
    /// 空闲块数。
    pub free_blocks: u32,
    /// 已用块数。
    pub used_blocks: u32,
    /// 有效字节数（未扫描时为 0）。
    pub live_bytes: u32,
    /// 可回收字节数（未扫描时为 0）。
    pub dead_bytes: u32,
    /// 文件数。
    pub file_count: u32,
    /// 名字表溢出数。
    pub table_overflow: u32,
    /// 最小擦除次数。
    pub erase_min: u32,
    /// 最大擦除次数。
    pub erase_max: u32,
    /// 擦除次数总和。
    pub erase_total: u32,
    /// 显式 GC 次数。
    pub gc_explicit_runs: u32,
    /// 自动 GC 次数。
    pub gc_auto_runs: u32,
    /// 是否做过介质扫描。
    pub scanned: bool,
    /// 索引是否脏。
    pub index_dirty: bool,
}
