//! `mpu-regions` —— MPU 区域表与 `RBAR`/`RASR` 的**纯逻辑**编解码
//! （**平台需求 SF-1**）。
//!
//! # 为什么把"区域表 + 编码"从 BSP 搬到这里
//!
//! 原来的 `crates/board-support/src/mpu.rs` 把三件事混在一个文件里：区域表、
//! 寄存器编码、以及**真正写寄存器**的那几行 `unsafe`。最后一件只能在真机上验证，
//! 而前两件恰恰是最容易错、也最该被机器反复检查的部分 —— 它们**曾经真的错过**。
//!
//! # 那个真 bug：`RBAR` 的区域号写错了位
//!
//! 第一版把区域号编码成 `index << 4`（塞进 bits[7:4]）。ARMv7-M 的 `RBAR` 却是：
//!
//! | 位 | 含义 |
//! |---|---|
//! | `[31:5]` | 基址（低 5 位隐含为 0） |
//! | `[4]` | `VALID`：写 1 才让"该区域号对应的那一组寄存器"真正落盘 |
//! | `[3:0]` | **区域号**（0..=7） |
//!
//! 后果有两层，第二层比第一层更隐蔽：
//!
//! 1. 8 次写全部落进 **region0**（`index << 4` 的 bits[3:0] 恒为 0），
//!    于是"配置了 8 个区域"实际上只配了 1 个；
//! 2. 从 `index >= 2` 起，`index << 4` 溢出 nibble、**变成地址位**（bit5 起），
//!    于是连那唯一生效的 region0 的**基址也被改掉**。
//!
//! 当时是**启动横幅回读硬件寄存器**发现的（region0 显示的是 region7 的属性、
//! 其余全 0）—— 那要烧写、接线、看串口才知道。同样的错误在主机上只要一条
//! `assert_eq!(rbar_index(rbar(i, base)), i)` 就会当场失败。
//!
//! 所以本 crate 的核心主张是：**把"枚举→位域→寄存器字"这一步做成被测函数**
//! （[`rbar`]/[`rasr`] 与其逆 [`rbar_index`]/[`rbar_base`]/[`rasr_size`]/[`rasr_access`]），
//! 让编码错误在 `cargo test` 里失败，而不是在目标现场失败。
//!
//! # 这份区域表从哪来
//!
//! 不是自己推的：取自**本板既有固件的已知正确配置**
//! （`winshare/test/master-control-board-a-06/bsp/board/board.c` 的 `BOARD_ConfigMPU`，
//! 那份固件在这块板上跑过现场）。相对它的改动只有一处，由**本板事实**决定：
//! Flash 区域 8 MB → **32 MB**（实装 W25Q256JVEQ 是 32 MiB）。
//!
//! # 三条不是"风格"而是安全/性能关键的约束
//!
//! 1. **Flash 必须只读**（[`AccessPermission::ReadOnly`]，AP=6）。它同时是"可缓存"
//!    （XIP 读得快）与"只读"（MPU 拒绝写入）。可缓存 + 可写意味着脏行会被
//!    **回写进 Flash** —— 那等于在没有任何命令序列的情况下改写器件内容。
//! 2. **SDRAM 覆盖层必须是编号最大的区域**。重叠时**高编号胜出**，而 SDRAM 的
//!    32 MiB 写回区压在同基址的 1 GB「背景区」（默认不可缓存）之上；一旦它的
//!    编号不再是最大，背景区会把它盖回"不可缓存"，性能收益**静默消失**。
//! 3. **基址必须按自身大小对齐**。这是 MPU 的硬性要求，违反它得到的不是"部分
//!    生效"而是**未定义行为**（区域覆盖的地址范围不可预测）。
//!
//! 这三条连同"大小是 2 的幂且 >= 32 字节""区域数 <= 8"一起，被**两处**强制：
//! 编译期断言（固件构建就失败）与 [`validate`]（主机测试、门禁、BSP 复用同一份判据）。
//!
//! # 不做什么
//!
//! 这里**一个寄存器都不写**：`MPU->RBAR` 的写入、`dsb`、cache 使能序列留在
//! `board-support`（它们需要 `unsafe` 与真机验证）。本 crate 无依赖、无分配、
//! 无 `unsafe`，因此能在主机上 `cargo test` 反复跑。

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

// ---------------------------------------------------------------------------
// 位域常量：与 ARMv7-M 手册的编号逐位对应（下面每个 `_SHIFT` 都是"从第几位开始"）
// ---------------------------------------------------------------------------

/// Cortex-M7（ARMv7-M）只有 8 个 MPU 区域，`RBAR`/`RASR` 各 8 个。
pub const MAX_REGIONS: usize = 8;

/// FlexSPI XIP 映射基址（本板 Flash 的物理起点）。
///
/// [`validate`] 用它来**认出** Flash 区域 —— 判据是物理基址而不是名字：
/// 名字是文案，基址是事实。
pub const FLASH_BASE: u32 = memory_layout::FLASH_XIP.base;

/// 外部 SDRAM 基址（W9825G6KH-6I，32 MB）。
pub const SDRAM_BASE: u32 = memory_layout::SDRAM.base;

/// `RBAR` 的 `VALID` 位：置 1 后，`RBAR` 里写的区域号所指的那组寄存器才生效。
pub const RBAR_VALID: u32 = 1 << 4;

/// `RBAR` 的区域号掩码（bits[3:0]）。**本 crate 的头号回归对象。**
pub const RBAR_REGION_MASK: u32 = 0xF;

/// `RBAR` 的地址掩码（bits[31:5]）：低 5 位不是地址（bit4 是 `VALID`）。
pub const RBAR_ADDRESS_MASK: u32 = !0x1F;

/// 区域最小字节数：2^5。ARMv7-M 规定 `SIZE` 字段小于 4 是**未定义编码**。
pub const MIN_REGION_SIZE: u32 = 32;

/// `RASR.SIZE` 允许的最小值（对应 [`MIN_REGION_SIZE`]）。
pub const MIN_SIZE_FIELD: u32 = 4;

/// `RASR.SIZE` 允许的最大值：`SIZE = 31` 表示 **4 GiB**。
///
/// 注意它**不在**本 crate 可编码的范围内：`size: u32` 装不下 2^32，
/// 所以能表达的最大区域是 2 GiB（`SIZE = 30`）。见 [`rasr_size`] 的返回值约定。
pub const MAX_SIZE_FIELD: u32 = 31;

/// `size: u32` 能表达的 `SIZE` 上界：`30`，即 2 GiB。
///
/// 它比 [`MAX_SIZE_FIELD`] 小 1，正是"4 GiB 在 `u32` 里装不下"这条事实的落点：
/// 解码时遇到 `SIZE = 31` 必须报"给不出大小"，而不是把一个移位的产物当成大小。
pub const MAX_ENCODABLE_SIZE_FIELD: u32 = MAX_SIZE_FIELD - 1;

/// `TEX` 是 3 位字段，取值 0..=7；超出会**串到保留位/相邻字段**上（大值甚至压到 `AP`）。
pub const MAX_TEX: u8 = 7;

// 位域布局常量：**公开**，因为 BSP 的回读打印（启动横幅那一行 `regionN:`）
// 与门禁要用与编码**同一组**位定义 —— 各自抄一遍数字，迟早有一处漂移。
/// `RASR` 的 `XN` 位（1 = 禁止取指）。本表全部为 0：任何区域都允许取指。
pub const XN_SHIFT: u32 = 28;
/// `RASR` 的 `AP` 位域起点（bits[26:24]）。
pub const AP_SHIFT: u32 = 24;
/// `RASR` 的 `TEX` 位域起点（bits[21:19]）。
pub const TEX_SHIFT: u32 = 19;
/// `RASR` 的 `S`（shareable）位。
pub const S_SHIFT: u32 = 18;
/// `RASR` 的 `C`（cacheable）位。
pub const C_SHIFT: u32 = 17;
/// `RASR` 的 `B`（bufferable）位。
pub const B_SHIFT: u32 = 16;
/// `RASR` 的 `SIZE` 位域起点（bits[5:1]）。
pub const SIZE_SHIFT: u32 = 1;
/// `RASR` 的 `SIZE` 位域掩码（5 位）。
pub const SIZE_MASK: u32 = 0x1F;
/// `RASR` 的 `AP` 位域掩码（3 位）。
pub const AP_MASK: u32 = 0x7;
/// `RASR` 的 `TEX` 位域掩码（3 位）。
pub const TEX_MASK: u32 = 0x7;
/// `RASR.ENABLE`：区域使能位。
pub const ENABLE: u32 = 1;

/// MPU 区域的访问权限（`RASR.AP` 的两种取值）。
///
/// 只列出本板用得到的两种：`AP` 还有"仅特权可读写/只读"等组合，本板不用，
/// 因为本固件**没有**非特权代码 —— 多列几种只会让"哪个才是 Flash 的只读"
/// 变得需要查表。真要用时再加，并同时补上对应的编码测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPermission {
    /// 特权与非特权都可读写（`AP = 3`，即 `0b011`）。
    ReadWrite,
    /// 特权与非特权都只读（`AP = 6`，即 `0b110`）。
    ReadOnly,
}

impl AccessPermission {
    /// 编码成 `RASR.AP` 的三位值。
    #[must_use]
    pub const fn ap(self) -> u32 {
        match self {
            Self::ReadWrite => 3,
            Self::ReadOnly => 6,
        }
    }

    /// 从 `RASR.AP` 的三位值解码；其余取值本 crate 不表达。
    ///
    /// 返回 `Option` 而不是"兜底成 `ReadWrite`"：回读时读到陌生的 `AP` 必须
    /// **看得出来**，而不是被静默翻译成"可读写"（那正是"报事实"变成假事实的路径）。
    #[must_use]
    pub const fn from_ap(ap: u32) -> Option<Self> {
        match ap {
            3 => Some(Self::ReadWrite),
            6 => Some(Self::ReadOnly),
            _ => None,
        }
    }
}

/// 一个 MPU 区域：**用字节数与布尔量表达意图**，编码成寄存器字是下面几个函数的活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// 基址（字节，必须按 `size` 对齐）。
    pub base: u32,
    /// 大小（字节，必须是 >= 32 的 2 的幂）。
    pub size: u32,
    /// 访问权限。
    pub access: AccessPermission,
    /// `TEX` 字段（0..=7）。
    pub tex: u8,
    /// 是否共享（`S`）。
    pub shareable: bool,
    /// 可否缓存（`C`）。
    pub cacheable: bool,
    /// 可否缓冲（`B`）。
    pub bufferable: bool,
    /// 一句话说明这片区域是什么、以及为什么这样配（启动横幅与文档都读它）。
    pub name: &'static str,
}

// ---------------------------------------------------------------------------
// 区域表：既是给 BSP 用的数据，也是编译期断言与主机测试的输入
// ---------------------------------------------------------------------------

/// 区域表。**表内下标即区域号**，且编号升序 —— 重叠靠"高编号胜出"生效。
pub const REGIONS: &[Region] = &[
    Region {
        base: 0xC000_0000,
        size: 512 * 1024 * 1024,
        access: AccessPermission::ReadWrite,
        tex: 2,
        shareable: false,
        cacheable: false,
        bufferable: false,
        name: "未使用的高位地址空间（挡住投机预取）",
    },
    Region {
        base: 0x8000_0000,
        size: 1024 * 1024 * 1024,
        access: AccessPermission::ReadWrite,
        tex: 2,
        shareable: false,
        cacheable: false,
        bufferable: false,
        name: "SDRAM 背景（默认不可缓存）",
    },
    Region {
        base: 0x6000_0000,
        size: 32 * 1024 * 1024,
        access: AccessPermission::ReadOnly,
        tex: 0,
        shareable: false,
        cacheable: true,
        bufferable: true,
        name: "FlexSPI XIP Flash（32 MiB，只读+可缓存）",
    },
    Region {
        base: 0x0000_0000,
        size: 1024 * 1024 * 1024,
        access: AccessPermission::ReadWrite,
        tex: 2,
        shareable: false,
        cacheable: false,
        bufferable: false,
        name: "片内 RAM 与外设的背景区（默认不可缓存）",
    },
    Region {
        base: memory_layout::ITCM.base,
        size: memory_layout::ITCM.size,
        access: AccessPermission::ReadWrite,
        tex: 0,
        shareable: false,
        cacheable: true,
        bufferable: true,
        name: "ITCM（64 KB，代码）",
    },
    Region {
        base: memory_layout::DTCM.base,
        size: memory_layout::DTCM.size,
        access: AccessPermission::ReadWrite,
        tex: 0,
        shareable: false,
        cacheable: true,
        bufferable: true,
        name: "DTCM（64 KB，栈与向量表）",
    },
    Region {
        // ⚠️ 用 **`OCRAM_CACHEABLE`（96 KB）而不是整块 128 KB** ——
        // 这不是省事，而是让 `DMA_POOL`（顶部 32 KB）**真正变成非缓存**的唯一机制：
        // 本区域不再覆盖那 32 KB，于是它落回 `region3`（1 GB 非缓存兜底）。
        // 若这里写回 128 KB，池就会重新变成可缓存，而 DMA 会读到停在缓存里的旧描述符。
        base: memory_layout::OCRAM_CACHEABLE.base,
        size: memory_layout::OCRAM_CACHEABLE.size,
        access: AccessPermission::ReadWrite,
        tex: 0,
        shareable: false,
        cacheable: true,
        bufferable: true,
        name: "OCRAM 可缓存区（64 KB，.rodata/.data/.bss；顶部 64 KB 留给 DMA 池）",
    },
    Region {
        // 基址/大小取自布局事实（`memory-layout`）：整个工程只有一处写 32 MiB。
        base: memory_layout::SDRAM.base,
        size: memory_layout::SDRAM.size,
        access: AccessPermission::ReadWrite,
        tex: 0,
        shareable: true, // 与既有固件一致：SDRAM 标为共享
        cacheable: true,
        bufferable: true,
        name: "外部 SDRAM（32 MB，写回+写分配）",
    },
];

/// 区域条目数。
pub const COUNT: usize = REGIONS.len();

/// 上电配置完成后**应当**处于的状态。
///
/// 单独做成常量而不是散在断言里：它是"我们期望什么"的唯一定义，
/// 固件自检、文档、测试都引用它。
pub const EXPECTED_ICACHE: bool = true;

/// 期望的 D-cache 状态。
pub const EXPECTED_DCACHE: bool = true;

/// 期望的 MPU 状态。
pub const EXPECTED_MPU: bool = true;

/// 核对读回的 MPU/cache 状态是否等于期望。
///
/// # 为什么需要它
///
/// 启动横幅已经把状态**回读并打印**了（"报事实"），但打印不等于判定：
/// 属性没生效时，横幅上那行会安静地显示 `OFF`，而没有东西会失败。
/// 这里把判定做成纯函数，于是"什么算不对"在主机上是可穷举测试的。
///
/// 返回 `Err` 时给的是**中文原因**，直接可以进日志。
///
/// # 为什么 Flash 只读不能只靠这里
///
/// 本函数只看 cache/MPU 的**使能位**；"Flash 区域是不是真的只读"由
/// [`REGIONS`] 的编译期断言（`Flash 区域必须只读（AP=6）`）与启动时的**区域回读**共同保证。
/// 两件事分开，是因为前者是"总开关"，后者是"每个区域的属性"。
pub fn check_state(icache: bool, dcache: bool, mpu_enabled: bool) -> Result<(), &'static str> {
    if !mpu_enabled {
        return Err("MPU 未启用：区域属性不生效，缓存行为退化为默认内存映射");
    }
    if !icache {
        return Err("I-cache 未启用：取指吞吐退化为逐次外部/片内访问");
    }
    if !dcache {
        return Err("D-cache 未启用：数据访问没有缓存，实测 Flash 读吞吐低约 11%");
    }
    Ok(())
}

// 编译期不变量：区域表一旦改坏，**固件构建就失败**，而不是等到真机上属性不对。
//
// 就地允许 `indexing_slicing`：在 **const 上下文**里，越界索引是**编译错误**，
// 而 clippy 那条 lint 担心的是运行期 panic —— 在这里不成立。
// 这里刻意只写 `while` 循环：const 上下文里没有迭代器。
//
// 覆盖面与 `validate` 有意**重叠而不相等**：`validate` 还能检查"任意一张传进来的表"
// （含测试里的坏表），const 断言只能看这一张表 —— 但它连"固件能不能编出来"都管，
// 这是 `validate` 做不到的。
#[allow(clippy::indexing_slicing)]
const _: () = {
    assert!(!REGIONS.is_empty(), "区域表不能为空");
    assert!(REGIONS.len() <= MAX_REGIONS, "MPU 只有 8 个区域");

    let mut i = 0;
    while i < REGIONS.len() {
        let region = REGIONS[i];
        // 大小必须是 >= 32 的 2 的幂（`size_field` 的合法输入）。
        assert!(
            region.size >= MIN_REGION_SIZE && region.size.is_power_of_two(),
            "MPU 区域大小必须是 >= 32 的 2 的幂"
        );
        // 基址必须按自身大小对齐（MPU 的硬性要求）。
        assert!(
            region.base.is_multiple_of(region.size),
            "MPU 区域基址未按大小对齐"
        );
        // TEX 只有 3 位：越界会串到保留位/相邻字段上（大值甚至压到 AP）。
        assert!(region.tex <= MAX_TEX, "TEX 只有 3 位");
        // 说明不能留白：启动横幅与文档都靠它解释"这区域是干什么的"。
        assert!(!region.name.is_empty(), "区域必须写明中文说明");
        i += 1;
    }

    // Flash 区域必须**只读**且**可缓存**：
    // 可缓存 + 可写意味着脏行会被回写进 Flash，那等于在没有任何命令序列的情况下
    // 改写器件内容；不可缓存则白丢 XIP 的读性能 —— 启用 MPU + 缓存后真机实测
    // Flash 读吞吐 46.6 → 51.8 MiB/s（见 `unsafe.budget` 里 34→35 那段记录；
    // 这个数字是"MPU 区域属性 + I/D cache 整体启用"的合计效果，不是单独一处的功劳）。
    assert!(REGIONS[2].base == FLASH_BASE, "region2 应当是 Flash");
    assert!(
        matches!(REGIONS[2].access, AccessPermission::ReadOnly),
        "Flash 区域必须只读（AP=6）"
    );
    assert!(REGIONS[2].cacheable, "Flash 区域应当可缓存");

    // SDRAM 覆盖层必须是编号最大的区域：重叠时高编号胜出，
    // 它要盖住前面那个同基址的 1 GB 背景区（否则性能收益静默消失）。
    assert!(
        REGIONS[REGIONS.len() - 1].base == SDRAM_BASE,
        "SDRAM 必须是编号最大的区域"
    );

    // 同一基址上，编号更高的区域必须是**更小**的覆盖层。
    let mut a = 0;
    while a < REGIONS.len() {
        let mut b = a + 1;
        while b < REGIONS.len() {
            if REGIONS[a].base == REGIONS[b].base {
                assert!(
                    REGIONS[b].size < REGIONS[a].size,
                    "同一基址上，编号更高的区域必须更小"
                );
            }
            b += 1;
        }
        a += 1;
    }
};

// ---------------------------------------------------------------------------
// 编码：意图 → 寄存器字
// ---------------------------------------------------------------------------

/// `RASR.SIZE` 字段：`log2(size) - 1`。
///
/// 返回 `None` 表示这个字节数**不能**出现在 `SIZE` 里：
///
/// - `< 32`（`SIZE < 4` 按 ARMv7-M 是未定义编码）—— 包括 `0`；
/// - 不是 2 的幂；
/// - 大于 2 GiB（`u32` 能表示的最大 2 的幂是 `1 << 31`）。
///
/// # 关于"4 GiB 上限"
///
/// ARMv7-M 的 `SIZE = 31` 表示 **4 GiB**，但那个大小**在 `u32` 里根本表达不出来**
/// （2^32 回绕成 0）。所以本函数能接受的最大区域是 2 GiB（`SIZE = 30`），而
/// "传 4 GiB"这件事要么编译不过、要么在调用点就已经回绕成 `0` 并被这里拒绝 ——
/// **不会**被静默当成"一个合法的巨大区域"。这条边界有测试钉住。
#[must_use]
pub const fn size_field(size: u32) -> Option<u32> {
    if size < MIN_REGION_SIZE || !size.is_power_of_two() {
        return None;
    }
    // 走到这里 size >= 32，故 trailing_zeros >= 5，减一不会下溢。
    Some(size.trailing_zeros() - 1)
}

/// 把区域号与基址编码成 `RBAR`。
///
/// ```text
/// bits[31:5] = 基址（低 5 位被清零）
/// bit[4]     = VALID = 1
/// bits[3:0]  = 区域号
/// ```
///
/// **区域号在低 4 位**，这是整个 crate 最贵的一条：写成 `region << 4` 会让
/// 8 次写全落进 region0，并且从 region2 起把地址位也一起改掉。
/// 这条性质由 `rbar_region_index_stays_in_low_nibble` 等测试钉死。
///
/// 区域号超过 7 时只取低 4 位（硬件本来就只认 4 位）；调用方应当先用
/// [`MAX_REGIONS`] 约束自己，而不是依赖这里的截断。
#[must_use]
pub const fn rbar(region_index: u32, base: u32) -> u32 {
    (base & RBAR_ADDRESS_MASK) | (region_index & RBAR_REGION_MASK) | RBAR_VALID
}

/// 把区域编码成 `RASR`。
///
/// ```text
/// bit[28]   = XN        = 0（本表所有区域都允许取指）
/// bits[26:24] = AP     = 3 可读写 / 6 只读
/// bits[21:19] = TEX
/// bit[18]   = S
/// bit[17]   = C
/// bit[16]   = B
/// bits[5:1] = SIZE     = log2(字节数) - 1
/// bit[0]    = ENABLE   = 1
/// ```
///
/// 大小非法时返回 **0**（即 `ENABLE = 0`，区域不使能），而不是编出一个"看起来
/// 有效"的区域：编码失败必须是**关掉**，不能是**另开一片**。表级不变量另有编译期
/// 断言与 [`validate`] 兜住，所以正常路径不会走到这里。
///
/// 之所以不是 `const fn`：它要把 `tex: u8` 宽化进 `u32`，而 `u32::from(u8)` 目前
/// 还不是 const（`From` 的 const 化未稳定），本 crate 又禁止 `as` 转换。
/// 表级断言因此直接检查字段，不经过 `rasr`。
#[must_use]
pub fn rasr(region: &Region) -> u32 {
    let size = match size_field(region.size) {
        Some(field) => field,
        None => return 0,
    };
    (region.access.ap() << AP_SHIFT)
        | (u32::from(region.tex) << TEX_SHIFT)
        | (u32::from(region.shareable) << S_SHIFT)
        | (u32::from(region.cacheable) << C_SHIFT)
        | (u32::from(region.bufferable) << B_SHIFT)
        | (size << SIZE_SHIFT)
        | ENABLE
}

// ---------------------------------------------------------------------------
// 解码：寄存器字 → 意图（启动横幅"报事实"的那条路径）
// ---------------------------------------------------------------------------

/// 从 `RBAR` 解出区域号（bits[3:0]）。
#[must_use]
pub const fn rbar_index(rbar: u32) -> u32 {
    rbar & RBAR_REGION_MASK
}

/// 从 `RBAR` 解出基址（bits[31:5]，低 5 位不是地址）。
#[must_use]
pub const fn rbar_base(rbar: u32) -> u32 {
    rbar & RBAR_ADDRESS_MASK
}

/// 从 `RASR` 解出区域大小（字节）。
///
/// 返回 **0** 表示这个编码**给不出可用大小**，两种情形：
///
/// - `SIZE < 4`：按 ARMv7-M 是未定义编码（区域最小 32 字节）；
/// - `SIZE = 31`：那是 4 GiB，`u32` 装不下（见 [`size_field`]）。
///
/// 用 0 当"给不出"的哨兵是安全的：0 在任何一处都不是合法的区域大小，
/// 因此调用方不会把哨兵误当成"一个大小为 0 的区域"。这里**不能**直接
/// `1u32 << (SIZE + 1)` —— `SIZE = 31` 会移位溢出（本工作区 `overflow-checks = true`，
/// 于是固件会在回读一个损坏的 `RASR` 时 panic，而不是把它报出来）。
#[must_use]
pub const fn rasr_size(rasr: u32) -> u32 {
    let field = (rasr >> SIZE_SHIFT) & SIZE_MASK;
    if field < MIN_SIZE_FIELD || field > MAX_ENCODABLE_SIZE_FIELD {
        return 0;
    }
    1u32 << (field + 1)
}

/// 从 `RASR` 解出 `AP`（bits[26:24]，原始三位值；用 [`AccessPermission::from_ap`] 转成类型）。
#[must_use]
pub const fn rasr_access(rasr: u32) -> u32 {
    (rasr >> AP_SHIFT) & AP_MASK
}

/// 从 `RASR` 解出 `(TEX, S, C, B)`。
#[must_use]
pub const fn rasr_tex_shareable_cacheable_bufferable(rasr: u32) -> (u32, bool, bool, bool) {
    (
        (rasr >> TEX_SHIFT) & TEX_MASK,
        (rasr >> S_SHIFT) & 1 != 0,
        (rasr >> C_SHIFT) & 1 != 0,
        (rasr >> B_SHIFT) & 1 != 0,
    )
}

// ---------------------------------------------------------------------------
// 校验：同一份判据给测试、门禁与 BSP 复用
// ---------------------------------------------------------------------------

/// 检查一张区域表是否**可以**写进 MPU。返回 `Err` 时给出人能读懂的中文原因。
///
/// 覆盖的不变量（每条都有对应测试，含"改坏就必须报错"的负例）：
///
/// 1. 表非空（空表 = 什么都没配，那不是一张表）；
/// 2. 条目数不超过 [`MAX_REGIONS`]（硬件只有 8 组寄存器）；
/// 3. 每个区域的大小是 >= 32 的 2 的幂（`RASR.SIZE` 能表达）；
/// 4. 每个区域的基址按**自身大小**对齐；
/// 5. `TEX` 在 3 位范围内（越界会串到保留位/相邻字段）；
/// 6. 每个区域都有中文说明（说明留白 = 下次没人敢改这张表）；
/// 7. [`FLASH_BASE`] 上的区域只读且可缓存（见 crate 文档第 1 条）；
/// 8. 编号最大的区域必须覆盖 [`SDRAM_BASE`]（见 crate 文档第 2 条）；
/// 9. 同一基址上，编号更高的区域必须**更小**（否则背景盖住前景）。
///
/// 之所以 `validate` 与编译期断言并存：const 断言只能看 [`REGIONS`] 这一张表，
/// 而这里能看**任意**一张表 —— 测试里的坏表、将来外部传入的表都走同一条判据。
pub fn validate(regions: &[Region]) -> Result<(), &'static str> {
    if regions.is_empty() {
        return Err("区域表为空：至少要配置一个区域");
    }
    if regions.len() > MAX_REGIONS {
        return Err("MPU 只有 8 个区域，表比硬件长");
    }

    for region in regions {
        // 先判大小：这一步同时排除了 size = 0，下面的取模才不会除零。
        // **顺序不能换**（把取模提到前面，size = 0 会先 panic）。
        if size_field(region.size).is_none() {
            return Err("区域大小必须是 >= 32 且不超过 4 GiB 的 2 的幂");
        }
        if region.base % region.size != 0 {
            return Err("区域基址必须按其自身大小对齐");
        }
        if region.tex > MAX_TEX {
            return Err("TEX 只有 3 位（0..=7），越界会串到保留位/相邻字段");
        }
        if region.name.is_empty() {
            return Err("区域必须写明中文说明（启动横幅与文档都读它）");
        }
        if region.base == FLASH_BASE {
            if region.access != AccessPermission::ReadOnly {
                return Err("Flash 区域必须只读：可缓存 + 可写会让脏行回写进 Flash");
            }
            if !region.cacheable {
                return Err("Flash 区域应当可缓存：否则 XIP 读取白丢缓存收益");
            }
        }
    }

    match regions.last() {
        Some(last) if last.base == SDRAM_BASE => {}
        _ => return Err("SDRAM 必须是编号最大的区域（重叠靠高编号胜出）"),
    }

    for (index, region) in regions.iter().enumerate() {
        for later in regions.iter().skip(index + 1) {
            if region.base == later.base && later.size >= region.size {
                return Err("同一基址上，编号更高的区域必须更小（否则背景会盖住前景）");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_state_accepts_the_expected_state() {
        assert_eq!(
            check_state(EXPECTED_ICACHE, EXPECTED_DCACHE, EXPECTED_MPU),
            Ok(())
        );
    }

    #[test]
    fn check_state_rejects_a_disabled_mpu_first() {
        // MPU 关是**最先**要报的：它影响的是正确性（属性不生效），不只是速度。
        let reason = check_state(true, true, false).expect_err("MPU 关必须报错");
        assert!(reason.contains("MPU"), "原因里必须点明是 MPU：{reason}");
    }

    #[test]
    fn check_state_rejects_a_disabled_icache() {
        let reason = check_state(false, true, true).expect_err("I-cache 关必须报错");
        assert!(
            reason.contains("I-cache"),
            "原因里必须点明是 I-cache：{reason}"
        );
    }

    #[test]
    fn check_state_rejects_a_disabled_dcache() {
        let reason = check_state(true, false, true).expect_err("D-cache 关必须报错");
        assert!(
            reason.contains("D-cache"),
            "原因里必须点明是 D-cache：{reason}"
        );
    }

    #[test]
    fn check_state_reports_every_combination_that_is_not_all_on() {
        // 穷举三位：只有"全开"是 Ok，其余 7 种都必须给出原因（且原因非空）。
        for bits in 0u8..8 {
            let icache = bits & 1 != 0;
            let dcache = bits & 2 != 0;
            let mpu = bits & 4 != 0;
            let result = check_state(icache, dcache, mpu);
            if icache && dcache && mpu {
                assert_eq!(result, Ok(()), "全开必须是 Ok");
            } else {
                let reason = result.expect_err("非全开必须报错");
                assert!(!reason.is_empty(), "原因不能是空串");
            }
        }
    }

    use super::*;

    /// 造一个"最普通"的区域：TEX0 / S0 / C1 / B1 / 可读写。
    /// 各负例测试只改它的一两个字段，让"到底哪一条被违反"一目了然。
    const fn normal(base: u32, size: u32) -> Region {
        Region {
            base,
            size,
            access: AccessPermission::ReadWrite,
            tex: 0,
            shareable: false,
            cacheable: true,
            bufferable: true,
            name: "测试区域",
        }
    }

    /// 断言这张表被拒绝，**且报错原因点到了 `needle`**。
    ///
    /// 只断言 `is_err()` 是不够的：那会让"因为别的原因报错"也算通过。
    fn rejected(regions: &[Region], needle: &str) {
        match validate(regions) {
            Ok(()) => panic!("这张表本应被拒绝，报错原因应含「{needle}」"),
            Err(reason) => assert!(
                reason.contains(needle),
                "拒绝理由不对：期望含「{needle}」，实得「{reason}」"
            ),
        }
    }

    /// 把 [`REGIONS`] 复制成可改的定长表（无分配；`Region` 是 `Copy`）。
    fn table() -> [Region; MAX_REGIONS] {
        core::array::from_fn(|index| REGIONS[index])
    }

    /// 合法的区域号范围 `0..8`（`usize → u32` 走 `try_from`：本 crate 禁止 `as`）。
    fn indices() -> core::ops::Range<u32> {
        0..u32::try_from(MAX_REGIONS).unwrap()
    }

    // ---------------------------------------------------------- 区域号回归

    #[test]
    fn rbar_region_index_stays_in_low_nibble() {
        // 这条就是"当年那个 bug"的回归测试：区域号必须在 bits[3:0]，
        // VALID 在 bit4，地址位只由基址决定。
        for index in indices() {
            for base in [0u32, 0x6000_0000, 0x8000_0000, 0x2020_0000, 0xC000_0000] {
                let encoded = rbar(index, base);
                assert_eq!(
                    encoded & RBAR_REGION_MASK,
                    index,
                    "区域号必须在 RBAR 的低 4 位（index={index}, base={base:#010x}）"
                );
                assert_ne!(encoded & RBAR_VALID, 0, "VALID 位必须置起（index={index}）");
                assert_eq!(
                    encoded & RBAR_ADDRESS_MASK,
                    base & RBAR_ADDRESS_MASK,
                    "地址位必须与传入基址一致、不被区域号污染（index={index}）"
                );
            }
        }
    }

    #[test]
    fn old_encoding_would_have_failed_that_regression_test() {
        // **刻意复原**旧写法：区域号被 `<< 4` 放进 bits[7:4]，与 VALID 同处一个 nibble。
        // 这个函数只存在于测试里，用途是证明上面那条回归测试**真的能抓住它** ——
        // 否则"回归测试"只是和实现一起写出来的同义反复。
        fn old_buggy_rbar(region_index: u32, base: u32) -> u32 {
            (base & RBAR_ADDRESS_MASK) | (region_index << 4) | RBAR_VALID
        }

        for index in indices() {
            let base = 0x6000_0000;
            let buggy = old_buggy_rbar(index, base);
            // ① 低 4 位恒为 0 → 8 次写全部落进 region0，其余区域全关。
            assert_eq!(
                rbar_index(buggy),
                0,
                "旧写法的区域号永远解不出来（index={index}）"
            );
            // ② 从 index >= 2 起，`index << 4` 溢出 nibble、变成地址位 → 基址被改掉。
            if index >= 2 {
                assert_ne!(
                    rbar_base(buggy),
                    base & RBAR_ADDRESS_MASK,
                    "旧写法把区域号串进了地址位（index={index}）"
                );
            }
            // ③ region0 是唯一"碰巧正确"的一个，其余全部与正确编码不同。
            if index == 0 {
                assert_eq!(buggy, rbar(index, base), "region0 的旧写法碰巧是对的");
            } else {
                assert_ne!(buggy, rbar(index, base), "index={index} 的旧编码是错的");
            }
        }
    }

    // ---------------------------------------------------------------- RBAR

    #[test]
    fn rbar_round_trips_region_and_base() {
        for (index, region) in REGIONS.iter().enumerate() {
            let index = u32::try_from(index).unwrap();
            let encoded = rbar(index, region.base);
            assert_eq!(rbar_index(encoded), index);
            assert_eq!(rbar_base(encoded), region.base);
        }
    }

    #[test]
    fn rbar_clears_low_five_bits_of_base() {
        // 低 5 位不是地址：即使传进来的基址没对齐，也不该出现在编码结果里
        // （bit4 让给 VALID，bits[3:0] 让给区域号）。
        for index in indices() {
            let encoded = rbar(index, 0x2000_001F);
            assert_eq!(rbar_base(encoded), 0x2000_0000);
            assert_eq!(rbar_index(encoded), index);
        }
    }

    #[test]
    fn rbar_uses_only_the_low_four_bits_of_region_index() {
        // 区域号字段只有 4 位：越界的输入只保留低 4 位，**不会串进地址位**。
        // 注意 8..=15 会被原样编码（`8 & 0xF == 8`）—— 那是硬件里不存在的区域号，
        // 拦住它是 [`MAX_REGIONS`] 与 `validate` 的职责，不是这里截断的职责。
        assert_eq!(rbar_index(rbar(16, 0x2000_0000)), 0);
        assert_eq!(rbar_index(rbar(0x13, 0x2000_0000)), 3);
        assert_eq!(rbar_index(rbar(u32::MAX, 0x2000_0000)), RBAR_REGION_MASK);
        for index in [16u32, 0x13, u32::MAX] {
            assert_eq!(
                rbar_base(rbar(index, 0x2000_0000)),
                0x2000_0000,
                "越界的区域号不该污染地址位（index={index}）"
            );
        }
    }

    // ---------------------------------------------------------------- RASR

    #[test]
    fn rasr_round_trips_size() {
        for region in REGIONS {
            assert_eq!(
                rasr_size(rasr(region)),
                region.size,
                "{} 的大小回读不一致",
                region.name
            );
        }
    }

    #[test]
    fn rasr_round_trips_access_permission() {
        for region in REGIONS {
            let decoded = rasr_access(rasr(region));
            assert_eq!(
                decoded,
                region.access.ap(),
                "{} 的 AP 回读不一致",
                region.name
            );
            assert_eq!(
                AccessPermission::from_ap(decoded),
                Some(region.access),
                "{} 的 AP 应能解回同一个权限",
                region.name
            );
        }
    }

    #[test]
    fn rasr_round_trips_tex_shareable_cacheable_bufferable() {
        for region in REGIONS {
            let (tex, s, c, b) = rasr_tex_shareable_cacheable_bufferable(rasr(region));
            assert_eq!(tex, u32::from(region.tex));
            assert_eq!(s, region.shareable);
            assert_eq!(c, region.cacheable);
            assert_eq!(b, region.bufferable);
        }
    }

    #[test]
    fn rasr_enables_region_and_allows_fetch() {
        for region in REGIONS {
            let encoded = rasr(region);
            assert_eq!(encoded & ENABLE, 1, "{} 必须使能", region.name);
            assert_eq!((encoded >> XN_SHIFT) & 1, 0, "{} 必须允许取指", region.name);
        }
    }

    #[test]
    fn rasr_bit_layout_matches_manual() {
        // 逐位钉住 region7（TEX0 / S1 / C1 / B1 / 32 MB / AP3）的期望字。
        // 这条测试的价值在于：它不调用 `rasr` 之外任何本 crate 的函数来"互相印证"，
        // 期望值是直接照手册算出来的常量。
        let region = REGIONS[7];
        let size = 24; // log2(32 MiB) - 1
        // TEX = 0 与 XN = 0 都贡献 0，所以期望值里不出现它们。
        let expected = (3 << AP_SHIFT)
            | (1 << S_SHIFT)
            | (1 << C_SHIFT)
            | (1 << B_SHIFT)
            | (size << SIZE_SHIFT)
            | ENABLE;
        assert_eq!(rasr(&region), expected);
        assert_eq!(rasr(&region), 0x0307_0031);
    }

    #[test]
    fn rasr_returns_zero_for_invalid_size() {
        // 编码失败必须表现为"区域不使能"，不能表现为"另开一片"。
        assert_eq!(rasr(&normal(0x2000_0000, 0)), 0);
        assert_eq!(rasr(&normal(0x2000_0000, 48)), 0);
        assert_eq!(rasr(&normal(0x2000_0000, 16)), 0);
    }

    #[test]
    fn rasr_size_returns_zero_for_undefined_encodings() {
        // SIZE < 4：ARMv7-M 未定义（区域最小 32 字节）。
        assert_eq!(rasr_size(0), 0);
        assert_eq!(rasr_size(3 << 1), 0);
        // SIZE = 4 是最小合法值：32 字节。
        assert_eq!(rasr_size(4 << 1), 32);
        // SIZE = 31 是 4 GiB —— u32 装不下，因此按要求返回 0（而不是移位溢出 panic）。
        assert_eq!(rasr_size(31 << 1), 0);
        // SIZE = 30 是能表达的最大区域：2 GiB。
        assert_eq!(rasr_size(30 << 1), 1 << 31);
    }

    // ----------------------------------------------------------- size_field

    #[test]
    fn size_field_accepts_powers_of_two_from_32_bytes() {
        assert_eq!(size_field(32), Some(4));
        assert_eq!(size_field(64), Some(5));
        assert_eq!(size_field(1024), Some(9));
        assert_eq!(size_field(64 * 1024), Some(15));
        assert_eq!(size_field(128 * 1024), Some(16));
        assert_eq!(size_field(32 * 1024 * 1024), Some(24));
        assert_eq!(size_field(512 * 1024 * 1024), Some(28));
        assert_eq!(size_field(1024 * 1024 * 1024), Some(29));
        assert_eq!(size_field(1 << 31), Some(30), "2 GiB 是能表达的最大区域");
    }

    #[test]
    fn size_field_rejects_zero_one_and_below_32() {
        // 0 特别要紧：它是"4 GiB 回绕"之后的样子，绝不能解成一个合法区域。
        assert_eq!(size_field(0), None);
        assert_eq!(size_field(1), None);
        assert_eq!(size_field(2), None);
        assert_eq!(size_field(16), None);
        assert_eq!(size_field(31), None, "31 既太小也不是 2 的幂");
    }

    #[test]
    fn size_field_rejects_non_power_of_two() {
        assert_eq!(size_field(33), None);
        assert_eq!(size_field(48), None);
        assert_eq!(size_field(1000), None);
        assert_eq!(size_field(6 * 1024 * 1024), None);
        assert_eq!(size_field(u32::MAX), None);
    }

    #[test]
    fn size_field_rejects_four_gib() {
        // 4 GiB 在 `u32` 里装不下：写成字面量要么编译不过，要么在调用点回绕成 0。
        // 两种形态都必须被拒绝，而不是被当成"一个巨大的合法区域"。
        assert_eq!(size_field(0), None, "4 GiB 回绕成 0 之后必须被拒绝");
        assert_eq!(size_field(0xFFFF_F000), None, "接近 4 GiB 的值也必须被拒绝");
        // 顺带把上界写清楚：能表达的最大区域是 2 GiB。
        assert_eq!(size_field(1 << 31), Some(30));
    }

    #[test]
    fn size_field_and_rasr_size_are_inverses() {
        // 遍历全部**可表达**的大小（32 字节 .. 2 GiB）。
        // 上界写成显式的 break：`size <<= 1` 越过 2 GiB 会回绕成 0，
        // 那会让循环拿 0 去 unwrap 一个 None（这条一度真的写错过）。
        let mut size = MIN_REGION_SIZE;
        loop {
            let field = size_field(size).unwrap();
            assert_eq!(rasr_size(field << SIZE_SHIFT), size, "size={size}");
            if size == 1 << 31 {
                break;
            }
            size <<= 1;
        }
    }

    // ------------------------------------------------------------- 权限编码

    #[test]
    fn access_permission_encodes_three_and_six() {
        assert_eq!(AccessPermission::ReadWrite.ap(), 3);
        assert_eq!(AccessPermission::ReadOnly.ap(), 6);
        assert_eq!(
            AccessPermission::from_ap(3),
            Some(AccessPermission::ReadWrite)
        );
        assert_eq!(
            AccessPermission::from_ap(6),
            Some(AccessPermission::ReadOnly)
        );
        // 其余取值必须"看得出来"，不能被兜底成可读写。
        for ap in 0..=7u32 {
            if ap != 3 && ap != 6 {
                assert_eq!(AccessPermission::from_ap(ap), None, "AP={ap} 不该被接受");
            }
        }
    }

    // ------------------------------------------------------------- 区域表

    #[test]
    fn regions_table_matches_board_config() {
        assert_eq!(COUNT, MAX_REGIONS, "本板应当配满 8 个区域");
        let expected = [
            (
                0xC000_0000u32,
                512 * 1024 * 1024u32,
                2u8,
                false,
                false,
                false,
            ),
            (0x8000_0000, 1024 * 1024 * 1024, 2, false, false, false),
            (0x6000_0000, 32 * 1024 * 1024, 0, false, true, true),
            (0x0000_0000, 1024 * 1024 * 1024, 2, false, false, false),
            (0x0000_0000, 64 * 1024, 0, false, true, true),
            (0x2000_0000, 64 * 1024, 0, false, true, true),
            (0x2020_0000, 64 * 1024, 0, false, true, true),
            (0x8000_0000, 32 * 1024 * 1024, 0, true, true, true),
        ];
        for (index, (base, size, tex, s, c, b)) in expected.iter().enumerate() {
            let region = REGIONS[index];
            assert_eq!(region.base, *base, "region{index} 基址");
            assert_eq!(region.size, *size, "region{index} 大小");
            assert_eq!(region.tex, *tex, "region{index} TEX");
            assert_eq!(region.shareable, *s, "region{index} S");
            assert_eq!(region.cacheable, *c, "region{index} C");
            assert_eq!(region.bufferable, *b, "region{index} B");
            assert!(!region.name.is_empty(), "region{index} 缺少说明");
        }
    }

    #[test]
    fn flash_region_is_32_mib_and_read_only() {
        // 本板实装 W25Q256JVEQ = 32 MiB（既有固件写的是 8 MB，是本 crate 唯一改动的值）。
        let flash = REGIONS[2];
        assert_eq!(flash.base, FLASH_BASE);
        assert_eq!(flash.size, 32 * 1024 * 1024, "Flash 必须是 32 MiB");
        assert_eq!(flash.access, AccessPermission::ReadOnly);
        assert!(flash.cacheable, "Flash 只读但必须可缓存");
    }

    #[test]
    fn sdram_overlay_is_the_last_region() {
        let last = REGIONS[COUNT - 1];
        assert_eq!(last.base, SDRAM_BASE);
        assert_eq!(last.size, 32 * 1024 * 1024);
        assert!(last.shareable, "与既有固件一致：SDRAM 标为共享");
        assert!(last.cacheable && last.bufferable, "SDRAM 应写回 + 写分配");
        // 同基址的 1 GB 背景区必须排在它**前面**（低编号输给高编号）。
        assert!(REGIONS[1].base == SDRAM_BASE && REGIONS[1].size > last.size);
    }

    #[test]
    fn regions_table_passes_validate() {
        // 编译期断言与 `validate` 是两份判据（一份管构建、一份管任意表），
        // 它们对这张表必须给出同一个结论。
        assert_eq!(validate(REGIONS), Ok(()));
    }

    // ------------------------------------------------------------- 负例校验

    #[test]
    fn validate_rejects_empty_table() {
        rejected(&[], "为空");
    }

    #[test]
    fn validate_rejects_more_than_eight_regions() {
        let nine: [Region; MAX_REGIONS + 1] = core::array::from_fn(|index| {
            let step = u32::try_from(index).unwrap();
            normal(0x1000_0000 * (step + 1), 64 * 1024)
        });
        rejected(&nine, "8 个区域");
    }

    #[test]
    fn validate_rejects_misaligned_base() {
        // 64 KiB 的区域落在 4 KiB 边界上：MPU 会得到不可预测的覆盖范围。
        rejected(
            &[
                normal(0x1000, 64 * 1024),
                normal(SDRAM_BASE, 32 * 1024 * 1024),
            ],
            "对齐",
        );
        // 反过来，按自身大小对齐就是合法的（同一张表的"好版本"）。
        assert_eq!(
            validate(&[
                normal(0x1_0000, 64 * 1024),
                normal(SDRAM_BASE, 32 * 1024 * 1024)
            ]),
            Ok(())
        );
    }

    #[test]
    fn validate_rejects_bad_sizes() {
        rejected(
            &[normal(0, 16), normal(SDRAM_BASE, 32 * 1024 * 1024)],
            "2 的幂",
        );
        rejected(
            &[normal(0, 48), normal(SDRAM_BASE, 32 * 1024 * 1024)],
            "2 的幂",
        );
        // size = 0 必须走"大小非法"，而不是先在取模那一步除零 panic。
        rejected(
            &[normal(0, 0), normal(SDRAM_BASE, 32 * 1024 * 1024)],
            "2 的幂",
        );
    }

    #[test]
    fn validate_rejects_writable_flash() {
        // 「改坏就失败」：把 Flash 改成可写（可缓存 + 可写 = 脏行会回写进 Flash）。
        let mut bad = table();
        bad[2].access = AccessPermission::ReadWrite;
        rejected(&bad, "Flash");
    }

    #[test]
    fn validate_rejects_uncacheable_flash() {
        let mut bad = table();
        bad[2].cacheable = false;
        rejected(&bad, "可缓存");
    }

    #[test]
    fn validate_rejects_sdram_that_is_not_last() {
        // SDRAM 覆盖层不在最高编号：同基址的 1 GB 背景区会把它盖回"不可缓存"。
        rejected(
            &[
                normal(SDRAM_BASE, 32 * 1024 * 1024),
                normal(0x1000_0000, 64 * 1024),
            ],
            "SDRAM",
        );
    }

    #[test]
    fn validate_rejects_bigger_overlay_on_same_base() {
        // 同基址上"后写的反而更大"：那是背景盖住前景，与编号语义相反。
        rejected(
            &[
                normal(SDRAM_BASE, 32 * 1024 * 1024),
                normal(SDRAM_BASE, 1024 * 1024 * 1024),
            ],
            "更小",
        );
    }

    #[test]
    fn validate_rejects_tex_out_of_range() {
        let mut bad = table();
        bad[2].tex = 8; // 3 位字段装不下 8，会串到 bit22
        rejected(&bad, "TEX");
    }

    #[test]
    fn validate_rejects_unnamed_region() {
        let mut bad = table();
        bad[5].name = "";
        rejected(&bad, "说明");
    }

    #[test]
    fn validate_accepts_the_good_version_of_each_bad_table() {
        // 反向对照：上面每张坏表都只改了一处，改回去就必须通过 ——
        // 否则"拒绝"可能来自与本次改动无关的另一条规则。
        let mut good = table();
        good[2].access = AccessPermission::ReadOnly;
        good[2].cacheable = true;
        good[2].tex = 0;
        assert_eq!(validate(&good), Ok(()));

        // 只含 SDRAM 一张表也是合法的（最少配置）。
        assert_eq!(validate(&[normal(SDRAM_BASE, 32 * 1024 * 1024)]), Ok(()));
    }
}
