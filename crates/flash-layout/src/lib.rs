//! `flash-layout` —— 外部 QSPI Flash 的**用途划分**（**平台需求 MT-2 / EV-1**）。
//!
//! # 设计取向：分区只留"启动与升级必需"，应用数据全部交给文件系统
//!
//! 早期方案把**每一类数据各切一个分区**（15 个分区）。那是**过度设计**：
//!
//! - 每多一个分区，就多一套"偏移/大小/掉电策略/磨损预算"要论证与维护；
//! - 而它们本质上都是"应用要存的数据"，生命周期差异**完全可以用文件语义表达**
//!   （整体覆盖的文件、只追加的日志文件、写一次的文件），不需要用物理分区表达；
//! - 分区表越大，`check-flash-map` 要维护的不变量越多，越容易"两处事实"。
//!
//! 因此收敛为**启动与升级必需的 6 个功能分区**：启动头、引导程序、应用槽 A/B、
//! A/B 元数据、一整块文件系统。全部应用数据以文件形式住在文件系统里，
//! 见 [`filesystem`](../../filesystem/src/lib.rs) 与
//! [Flash 用途划分](../../../docs/02-架构/Flash布局与用途划分.md)。
//!
//! # 为什么分区表仍是"可执行的事实来源"
//!
//! 它有三类使用者且必须永远一致：`board-support`（取偏移）、
//! `xtask check-flash-map`（校验不变量）、`docs/`（解释理由）。
//! 文档与代码各写一遍必然漂移，而"分区表漂移"的后果是**擦掉别人的数据**。
//! 所以表放在这个无依赖纯 crate 里，BSP 与门禁引用同一份数据
//! （`xtask` 以路径依赖引入，不解析源码）。
//!
//! # 两条硬约束
//!
//! 1. **整片 32 MiB 都在可寻址窗口内** —— 但这不是器件上电就能给的：
//!    `W25Q256` 上电处于 3 字节地址模式（上限 16 MiB）。本项目通过 FCB 里的
//!    **专用 4 字节地址操作码**（读 `0xEC`、页编程 `0x34`、扇区擦除 `0x21`，
//!    地址相位 `0x20`）打开了整片，见 `board-support::flash` 里的路线对照表。
//!    这条前提由 `xtask check-image` 从链接产物断言；一旦有人把 LUT 换回
//!    3 字节（`0xEB`/`0x18`），构建期就会失败 —— 否则后果是静默别名。
//! 2. **擦除粒度 4 KiB 扇区**（块擦除 64 KiB）。每个分区必须 4 KiB 对齐、
//!    大小为扇区整数倍，否则一次擦除会波及邻居。

// GAP(P4): run/backup 切换与引导程序未实现 —— 分区（bootloader/slot_run/slot_backup/boot_meta）
// 已预留且由门禁保护，但"复制、校验、回退"的代码还不存在。
// 阶段 1 过渡：映像仍从偏移 0 起，会临时占用 bootloader 预留区的一部分；
// 该状态由 `check-flash-map` 显式放行并打印，Bootloader 接入后结束。

#![no_std]
// 纯数据与算术：本 crate 存在的意义之一就是能被主机上的门禁遍历。
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_arithmetic,
    clippy::wildcard_imports
)]
// 测试里放宽：断言失败就该直接 panic。
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod boot_meta;

/// Flash 总容量（字节）：Winbond W25Q256JVEQ = 32 MiB。
pub const FLASH_SIZE: u32 = 32 * 1024 * 1024;

/// 擦除扇区（字节）。**所有分区必须按它对齐**。
pub const SECTOR: u32 = 4 * 1024;

/// 擦除块（字节）：块擦除比 16 次扇区擦除快得多，文件系统的分配单元取它。
pub const BLOCK: u32 = 64 * 1024;

/// 可以通过 XIP 映射访问的上限（字节）= **整片器件** 32 MiB。
///
/// 这个值等于 [`FLASH_SIZE`] 不是巧合、也不是"器件有 32 MiB 就一定能用 32 MiB"：
/// 它成立的前提是 FCB 的 LUT 用**专用 4 字节地址操作码**（见 crate 文档第 1 条）。
/// 若哪天有人把 LUT 换回 `0xEB`（3 字节地址），这个常量就会**不再成立** ——
/// 那时越界访问不会报错，而是把高位地址丢掉、**别名到低 16 MiB**。
/// 因此「LUT 的地址相位」与「本常量」必须一起看：`xtask check-image` 会从链接好的
/// ELF 里断言 LUT 用的是 4 字节地址相位，把这两件事绑在构建期。
pub const XIP_WINDOW: u32 = FLASH_SIZE;

/// 分区的用途。用途只表达**启动与升级的角色**；应用数据的差异由文件系统表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 启动头：FCB / IVT / boot data。只由 Boot ROM 读，运行期一个字都不许写。
    BootHeader,
    /// 引导程序：选择 A/B 槽位、校验映像、回滚。
    Bootloader,
    /// 应用槽位（A/B 各一份），可执行映像。
    AppSlot,
    /// A/B 元数据：当前槽位、试启次数、已确认标记。
    BootMeta,
    /// 应用数据：一整块文件系统。
    Filesystem,
}

/// 掉电原子性策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Atomicity {
    /// 运行期只读（写它等于破坏启动）。
    ReadOnly,
    /// 双份 + 序号 + CRC：任一次写入掉电都读回"完整旧值或完整新值"。
    DoubleBank,
    /// 整区擦写（只在升级时发生；失败由另一槽兜底）。
    WholeRegionSwap,
    /// 由文件系统保证：记录级 CRC + 日志结构 + 垃圾回收。
    Filesystem,
    /// 未启用（保留区）。
    Unused,
}

/// 一个分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// 分区名（同时用于文档与门禁报错）。
    pub name: &'static str,
    /// 起始偏移（相对 Flash 起点，即 XIP 映射的 `0x6000_0000 + offset`）。
    pub offset: u32,
    /// 大小（字节）。
    pub size: u32,
    /// 用途。
    pub purpose: Purpose,
    /// 掉电原子性策略。
    pub atomicity: Atomicity,
    /// 一句话说明"为什么是这个大小/位置"。
    pub why: &'static str,
}

impl Region {
    /// 结束偏移（不含）。
    #[must_use]
    pub const fn end(&self) -> u32 {
        self.offset + self.size
    }

    /// 扇区数。
    #[must_use]
    pub const fn sectors(&self) -> u32 {
        self.size / SECTOR
    }

    /// 块数（文件系统按块分配）。
    #[must_use]
    pub const fn blocks(&self) -> u32 {
        self.size / BLOCK
    }
}

// ---------------------------------------------------------------------------
// 各分区的偏移与大小：既是给 BSP 用的常量，也是编译期断言的输入
// ---------------------------------------------------------------------------

/// 启动头（FCB + IVT + boot data）。
pub const BOOT_HEADER_OFFSET: u32 = 0;
/// 启动头大小（到引导程序为止）。
pub const BOOT_HEADER_SIZE: u32 = 8 * 1024;
/// 引导程序。
pub const BOOTLOADER_OFFSET: u32 = BOOT_HEADER_OFFSET + BOOT_HEADER_SIZE;
/// 引导程序大小：256 KiB **区域**（含前 8 KiB 启动头）中的代码部分 = 248 KiB。
pub const BOOTLOADER_SIZE: u32 = 256 * 1024 - BOOT_HEADER_SIZE;
/// 实际运行槽（`slot_run`）：当前阶段 1 的映像仍从偏移 0 起，此槽为引导程序接入后预留。
pub const SLOT_RUN_OFFSET: u32 = BOOTLOADER_OFFSET + BOOTLOADER_SIZE;
/// 单个应用槽大小。
pub const SLOT_SIZE: u32 = 2 * 1024 * 1024;
/// 备份槽（`slot_backup`）：新固件写入这里，由 Bootloader 校验后复制到运行槽。
pub const SLOT_BACKUP_OFFSET: u32 = SLOT_RUN_OFFSET + SLOT_SIZE;
/// 启动元数据（运行/备份状态、复制进度、试启标记）。
pub const BOOT_META_OFFSET: u32 = SLOT_BACKUP_OFFSET + SLOT_SIZE;
/// A/B 元数据大小。
pub const BOOT_META_SIZE: u32 = 64 * 1024;
/// 文件系统起点：**全部应用数据都在这里**。
pub const FILESYSTEM_OFFSET: u32 = BOOT_META_OFFSET + BOOT_META_SIZE;
/// 文件系统大小：整片器件去掉启动/升级必需部分后**全部**给它（约 27.88 MiB）。
///
/// 为什么敢给这么大：4 字节操作码让整片可用之后，"多给空间"不再有代价
/// （代价只在 RAM：文件系统的块索引约 24 B/块）。日志类数据（黑匣子、离线缓冲）
/// 越大越好用，而 Flash 是这里最不缺的资源。
pub const FILESYSTEM_SIZE: u32 = XIP_WINDOW - FILESYSTEM_OFFSET;

/// 判断一个偏移是否落在**可通过 XIP 访问**的窗口内。
///
/// 越界的访问**不会报错** —— 控制器会丢掉高位地址、别名到低 16 MiB
/// （前提被破坏时尤其危险，见 [`XIP_WINDOW`]）。因此调用方必须用这个判据
/// 来决定"能不能访问"：宁可明确失败，也不要静默写错地方。
///
/// 收在这里而不是各模块自己写，是为了让"边界"只有一处定义、并被测试钉住。
#[must_use]
pub const fn is_addressable(offset: u32) -> bool {
    offset < XIP_WINDOW
}

/// 分区条目数。
pub const COUNT: usize = MAP.len();

/// 分区表。**顺序即偏移顺序**，门禁会验证这一点。
pub const MAP: &[Region] = &[
    Region {
        name: "boot_header",
        offset: BOOT_HEADER_OFFSET,
        size: BOOT_HEADER_SIZE,
        purpose: Purpose::BootHeader,
        atomicity: Atomicity::ReadOnly,
        why: "FCB 固定在偏移 0（Boot ROM 只从那里读），IVT 在 0x1000、boot data 紧随其后；\
              留到 8 KiB 是为了与引导程序的分界落在既有固件用过的 64 KiB 边界上",
    },
    Region {
        name: "bootloader",
        offset: BOOTLOADER_OFFSET,
        size: BOOTLOADER_SIZE,
        purpose: Purpose::Bootloader,
        atomicity: Atomicity::WholeRegionSwap,
        why: "既有固件在 0x2000 起放引导程序、整块 bootloader 区 64 KiB；本表沿用该边界",
    },
    Region {
        name: "slot_run",
        offset: SLOT_RUN_OFFSET,
        size: SLOT_SIZE,
        purpose: Purpose::AppSlot,
        atomicity: Atomicity::WholeRegionSwap,
        why: "既有固件的 app 槽只有 960 KiB 而实测映像已 1,062,020 B —— 明显不够；\
              给到 2 MiB（当前 Rust 映像约 41 KiB，约 50 倍余量）",
    },
    Region {
        name: "slot_backup",
        offset: SLOT_BACKUP_OFFSET,
        size: SLOT_SIZE,
        purpose: Purpose::AppSlot,
        atomicity: Atomicity::WholeRegionSwap,
        why: "A/B 双槽：升级只写非活动槽，引导程序校验后切换；\
              不做'把新映像拷进活动槽'的搬运式升级 —— 那样一次掉电就没有可启动的映像",
    },
    Region {
        name: "boot_meta",
        offset: BOOT_META_OFFSET,
        size: BOOT_META_SIZE,
        purpose: Purpose::BootMeta,
        atomicity: Atomicity::DoubleBank,
        why: "双份 + 单调序号：'当前槽位/试启次数/已确认'三个事实必须能掉电原子地更新；\
              8 KiB 就够，留 64 KiB 是为了与 64 KiB 块对齐并给未来留位。\
              必须独立成区：引导程序要在**没有文件系统**的情况下读它",
    },
    Region {
        name: "filesystem",
        offset: FILESYSTEM_OFFSET,
        size: FILESYSTEM_SIZE,
        purpose: Purpose::Filesystem,
        atomicity: Atomicity::Filesystem,
        why: "全部应用数据（应用参数、现场标定、复位台账、黑匣子、\
              离线缓冲、出厂信息）都以文件形式住在这里。\
              不再为每类数据切物理分区：它们的差异是文件语义（整体覆盖 / 只追加 / 写一次），\
              由文件系统表达即可；分区表越小，需要论证的不变量越少。\
              启用 4 字节操作码后整片可用，于是把剩下的 27.88 MiB 全给它",
    },
];

/// 按名字取分区。
#[must_use]
pub fn find(name: &str) -> Option<&'static Region> {
    MAP.iter().find(|region| str_eq(region.name, name))
}

/// `no_std` 下的字符串比较（不引入依赖）。
fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

// 编译期不变量：只依赖上面的常量，因此能在固件构建里生效。
// （全表级不变量——首尾相接、逐区对齐、恰好覆盖——由主机测试与
//   `xtask check-flash-map` 检查；const 上下文里 `slice::get` 还不可用。）
const _: () = {
    assert!(BOOT_HEADER_OFFSET == 0, "FCB 必须位于偏移 0");
    assert!(BOOT_HEADER_SIZE.is_multiple_of(SECTOR) && BOOT_HEADER_SIZE >= 0x2000);
    assert!(
        (BOOTLOADER_OFFSET + BOOTLOADER_SIZE).is_multiple_of(BLOCK),
        "引导程序区的末尾应落在 64 KiB 边界"
    );
    assert!(
        SLOT_RUN_OFFSET.is_multiple_of(BLOCK),
        "槽位起点应按 64 KiB 块对齐"
    );
    assert!(SLOT_SIZE.is_multiple_of(BLOCK), "槽位大小应是整块");
    assert!(SLOT_SIZE >= 1024 * 1024, "应用槽不得小于 1 MiB");
    assert!(
        SLOT_BACKUP_OFFSET == SLOT_RUN_OFFSET + SLOT_SIZE,
        "A/B 必须紧邻且等大"
    );
    assert!(BOOT_META_OFFSET.is_multiple_of(SECTOR));
    assert!(
        FILESYSTEM_OFFSET.is_multiple_of(BLOCK),
        "文件系统起点应按块对齐"
    );
    assert!(
        FILESYSTEM_SIZE.is_multiple_of(BLOCK),
        "文件系统大小应是整块"
    );
    assert!(
        FILESYSTEM_SIZE >= 4 * 1024 * 1024,
        "文件系统至少要有 4 MiB：它要装下应用文件与离线缓冲"
    );
    assert!(
        FILESYSTEM_OFFSET + FILESYSTEM_SIZE == XIP_WINDOW,
        "分区表必须恰好覆盖整片可寻址窗口"
    );
    assert!(
        XIP_WINDOW == FLASH_SIZE,
        "整片器件都应可寻址（前提：LUT 用 4 字节地址相位）"
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 可寻址边界不含糊() {
        // 4 字节操作码让整片可用，因此窗口 = 器件容量；边界之外仍然必须失败。
        assert!(is_addressable(0));
        assert!(is_addressable(XIP_WINDOW - 1));
        assert!(
            !is_addressable(XIP_WINDOW),
            "窗口之后必须明确失败，不能别名"
        );
        assert!(is_addressable(FLASH_SIZE - 1), "整片最后一个字节应当可寻址");
        assert!(!is_addressable(FLASH_SIZE));
    }

    #[test]
    fn 全部功能分区都在可寻址边界内() {
        for region in MAP {
            assert!(
                is_addressable(region.offset) && is_addressable(region.end() - 1),
                "{} 有字节落在可寻址窗口之外",
                region.name
            );
        }
    }

    #[test]
    fn 分区表首尾相接且不重叠() {
        let mut cursor = 0u32;
        for region in MAP {
            assert_eq!(
                region.offset, cursor,
                "{} 与前一个分区之间有缝或重叠",
                region.name
            );
            cursor = region.end();
        }
        assert_eq!(cursor, FLASH_SIZE, "分区表没有覆盖到 Flash 末尾");
    }

    #[test]
    fn 每个分区都按扇区对齐且写明了理由() {
        for region in MAP {
            assert_eq!(region.offset % SECTOR, 0, "{} 偏移未对齐", region.name);
            assert_eq!(region.size % SECTOR, 0, "{} 大小未对齐", region.name);
            assert!(region.size > 0, "{} 大小为零", region.name);
            assert!(
                !region.why.is_empty(),
                "{} 缺少'为什么这样分'的说明",
                region.name
            );
        }
    }

    #[test]
    fn 分区数量保持精简() {
        // 这条断言是**设计约束**而不是实现细节：一旦有人想再切一个专用分区，
        // 必须先把这里的数字改掉，从而被迫回答"为什么不能用文件表达"。
        assert_eq!(COUNT, 6, "只应有：启动头/引导程序/槽A/槽B/元数据/文件系统");
        // 分区表里**不该**再有"某类应用数据专用"的分区：它们的差异用文件语义表达。
        assert!(
            MAP.iter().all(|r| matches!(
                r.purpose,
                Purpose::BootHeader
                    | Purpose::Bootloader
                    | Purpose::AppSlot
                    | Purpose::BootMeta
                    | Purpose::Filesystem
            )),
            "出现了非启动/升级/文件系统用途的分区"
        );
    }

    #[test]
    fn run_backup_双槽大小相等且不小于一兆() {
        let a = find("slot_run").expect("应有 slot_run");
        let b = find("slot_backup").expect("应有 slot_backup");
        assert_eq!(a.size, b.size);
        assert!(a.size >= 1024 * 1024);
        assert_eq!(a.purpose, Purpose::AppSlot);
        assert_eq!(b.purpose, Purpose::AppSlot);
    }

    #[test]
    fn 启动头在偏移零且容得下启动结构() {
        let header = find("boot_header").expect("应有 boot_header");
        assert_eq!(header.offset, 0, "FCB 固定在偏移 0");
        assert!(header.size >= 0x2000);
        assert_eq!(header.atomicity, Atomicity::ReadOnly);
    }

    #[test]
    fn 文件系统承担全部应用数据且足够大() {
        let fs = find("filesystem").expect("应有 filesystem");
        assert_eq!(fs.purpose, Purpose::Filesystem);
        assert_eq!(fs.atomicity, Atomicity::Filesystem);
        assert_eq!(fs.offset, FILESYSTEM_OFFSET);
        // 装得下：日志 + 离线缓冲 + 各表，给到 11+ MiB。
        assert!(fs.size >= 8 * 1024 * 1024);
        assert_eq!(fs.size % BLOCK, 0);
        assert!(fs.blocks() >= 100, "块数太少会让垃圾回收频繁搬移");
    }

    #[test]
    fn 功能分区都在_xip_窗口内() {
        for region in MAP {
            assert!(
                region.end() <= XIP_WINDOW,
                "{} 越过可寻址窗口：3 字节地址模式下越过 16 MiB 的访问会静默别名",
                region.name
            );
        }
    }

    #[test]
    fn 启动与升级必需的用途都在表里() {
        for purpose in [
            Purpose::BootHeader,
            Purpose::Bootloader,
            Purpose::AppSlot,
            Purpose::BootMeta,
            Purpose::Filesystem,
        ] {
            assert!(
                MAP.iter().any(|r| r.purpose == purpose),
                "{purpose:?} 在分区表里没有落脚点"
            );
        }
    }

    #[test]
    fn 应用数据不再占用独立分区() {
        // 反向约束：应用数据类用途不应再出现在分区表里 —— 它们应当进文件系统。
        // 若将来有人加回"业务数据分区"之类的分区，这条测试会失败。
        assert_eq!(
            MAP.iter()
                .filter(|r| r.purpose == Purpose::Filesystem)
                .count(),
            1
        );
        let known = MAP
            .iter()
            .filter(|r| {
                matches!(
                    r.purpose,
                    Purpose::BootHeader
                        | Purpose::Bootloader
                        | Purpose::AppSlot
                        | Purpose::BootMeta
                        | Purpose::Filesystem
                )
            })
            .count();
        assert_eq!(known, MAP.len(), "分区表里出现了非启动/升级用途的分区");
    }
}
