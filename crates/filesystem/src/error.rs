//! 文件系统的错误类型。
//!
//! # 为什么把"数据损坏"和"驱动故障"分开
//!
//! 这两类失败对调用方的含义完全不同：
//!
//! - [`FsError::Corrupt`] 表示**介质内容不自洽**（校验不过、结构非法）。它是
//!   "数据出了事"，现场要记录、要上报、要留给诊断；但文件系统本身仍然可用，
//!   只是丢掉了受影响的那部分。
//! - [`FsError::Driver`] 表示**读写动作本身失败**（QSPI 超时、DMA 错误）。
//!   介质内容多半没坏，重试有意义。
//!
//! 把它们合成一个 `Io` 会让上层无法决定"该重试还是该报警"，而这两种反应
//! 在现场是相反的。同理，容量类错误（[`FsError::NoSpace`]、
//! [`FsError::TooManyFiles`]）与用法类错误（[`FsError::BadName`]、
//! [`FsError::AlreadyExists`]）也各自独立：前者的正确反应是回收/换策略，
//! 后者是**调用方写错了**，重试永远不会成功。

/// 文件系统错误。
///
/// 刻意不做成 `#[non_exhaustive]` 之外的抽象层：嵌入式侧的错误处理是
/// "逐个变体决定反应"，多一层映射只会多一处漏掉的分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsError {
    /// 文件名非法：为空、超过 31 字节、含 `/`、或含非可打印 ASCII。
    BadName,
    /// 文件不存在（从未写过，或已被 [`crate::FileSystem::remove`] 删除）。
    NotFound,
    /// 文件已存在（[`crate::FileSystem::create_file`] 的独占语义）。
    AlreadyExists,
    /// 文件条目已用满（[`crate::MAX_FILES`] 个），且没有可回收的墓碑条目。
    TooManyFiles,
    /// 空间不足：没有可写的块，且垃圾回收也无法腾出。
    NoSpace,
    /// 提供的缓冲区装不下要读的内容。
    ///
    /// 刻意**不**做"读一半"：截断的读取是"静默数据损坏"的经典来源，
    /// 调用方应当拿得到"这份内容有 N 字节"这个事实。
    BufferTooSmall,
    /// 单条记录/单个文件超过可表示范围（例如分块数超过 256）。
    FileTooLarge,
    /// 日志的下一个分块序号会超过 `u16`。
    LogFull,
    /// 设备几何不合法（块太小、块数为零、块太大）。
    BadGeometry,
    /// 块数超过 `MAX_BLOCKS`（内存索引放不下）。
    TooManyBlocks,
    /// 介质内容不自洽：校验不过或结构非法。
    Corrupt,
    /// 驱动层故障（读写擦失败）。
    Driver,
}
