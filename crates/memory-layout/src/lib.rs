//! `memory-layout` —— 片内内存的**布局事实**与**栈余量测量的纯逻辑**
//! （**平台需求 PF-3 / SF-2**）。
//!
//! # 为什么要抽出来
//!
//! 抽的是两件"同一件事被写了两遍"的事：
//!
//! 1. **片内内存布局**（ITCM / DTCM / OCRAM / Flash XIP / SDRAM 的基址与大小）
//!    散落在 `board-support/src/config.rs`、`firmware/build.rs`、`xtask check-size`
//!    与 `docs/` 里 —— 每多一处，就多一条漂移路径。这里让**数字只有一个来源**。
//! 2. **栈余量测量**（涂抹 + 扫描）原本写在 `board-support/src/stacks.rs` 里。
//!    它之所以留在 BSP，是因为它直接读写内存、要关中断；但它**错的方式恰恰是纯逻辑的错**
//!    （下面那个 bug 与硬件一点关系都没有）。把算法搬到无 `unsafe`、无依赖的
//!    环境里，就能在主机上 `cargo test` 反复跑。
//!
//! 抽出来之后剩下什么给 BSP：读 SP、`write_volatile` / `read_volatile`、临界区。
//! 那三件事确实只能在真机上验证；本 crate **一个真实地址都不碰**。
//!
//! # 那个真 bug：涂抹漏掉了栈底的那个字
//!
//! 涂抹（stack painting）的思路是"把空闲栈涂成指纹，跑一段之后看指纹被吃掉多少"。
//! 第一版的涂抹循环把下界写成了 `STACK_BOTTOM + 4` 而不是 `STACK_BOTTOM`，于是
//! **栈底那一个字从来没有被涂成指纹**。后果是一条**恒定**的假警报：
//!
//! 1. 扫描是**自栈底向上**找第一个不是指纹的字；
//! 2. 栈底那个字是上电遗留的旧内容，本来就不是指纹；
//! 3. 扫描在第一个字就"命中"，返回"整段栈都被用过"；
//! 4. 真机横幅于是永远打印 `峰值 = 整段栈 / 余量 0 B`（实测过一次 32768 B / 0 B）。
//!
//! 危害不是"数字错了"，而是**这个数字不再随负载变化**：余量是不是 0 与程序用了多少栈
//! 完全无关，于是"栈够不够"这个问题被彻底变成了噪声 —— 而栈曾被打穿过三次。
//!
//! 这个错误在主机上一秒可测：构造"涂 `[1, len)`"的场景，断言 [`high_water_bytes`]
//! 会报整段栈；再断言 [`paint_prefix`] 的正确用法（涂 `[0, n)`）不会。
//! 见测试 `missing_the_bottom_word_reports_the_whole_stack_as_used`。
//!
//! # 两个坐标约定（读错就会把方向搞反）
//!
//! - 切片 **`words[0]` 是栈底**（最低地址），最后一个元素是栈顶。栈从栈顶向下增长，
//!   因此"用掉的字"永远是**尾部连续的一段**。
//! - [`high_water_bytes`] 返回的是**从栈顶算起被用到过的字节数**，不是从栈底算起的偏移。
//!   两个极端：**全是指纹 → 0**（一个字都没用过）；**栈底那个字都不是指纹 → 整段栈**
//!   （用量已触底）。
//!
//! # 不做什么
//!
//! - 不读写任何真实地址，不带 `unsafe`，不做动态分配；
//! - 不依赖本项目的任何其它 crate（连 `mpu-regions` 也不依赖：布局常量在这里是
//!   **数据**，在那里是 MPU 属性；两处的一致性由 BSP 的编译期断言绑住）。
//!
//! # 每个数字的证据链
//!
//! | 常量 | 值 | 依据 |
//! |---|---|---|
//! | [`ITCM`] / [`DTCM`] / [`OCRAM`] | `0x0000_0000` 64 KiB、`0x2000_0000` 64 KiB、`0x2020_0000` 128 KiB | FlexRAM 8 bank × 32 KB 的默认（全零熔丝）划分，与 `imxrt-rt` 的 `Family::Imxrt1020` 链接脚本一致；`docs/硬件与板级事实.md` |
//! | [`FLASH_XIP`] | `0x6000_0000` 32 MiB | 实装 `W25Q256JVEQ`；`docs/硬件与板级事实.md` |
//! | [`SDRAM`] | `0x8000_0000` 32 MiB | 实装 `W9825G6KH-6I`（SEMC CS0）；`docs/硬件与板级事实.md` |
//! | [`STACK_SIZE`] | 48 KiB | 被真机打穿三次（8 / 16 / 32 KiB）之后的**实测**值；唯一真源是 `crates/firmware/build.rs` 的 `STACK_BYTES` |
//! | [`ITCM_CODE_BUDGET_BYTES`] | 64 KiB | `docs/架构与实现.md` §3.2：`.text` 落在 ITCM 且不超 64 KiB（按用户要求取消 8 KiB 预留） |

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
// 地址区间
// ---------------------------------------------------------------------------

/// 一个字（`u32`）的字节数。
///
/// 栈涂抹按**字**操作，而"用了多少字节"按字节报告，因此每一处换算都要用它 ——
/// 而不是在若干地方各写一个 `4`（那是"同一件事写两遍"的最小形态）。
pub const WORD_BYTES: usize = core::mem::size_of::<u32>();

/// 一段连续的地址区间：`base` 起、`size` 字节。
///
/// 刻意**不带名字**：名字是文案，地址与大小才是事实。名字只出现在 [`REGIONS`]
/// 这张带名字的清单里，供报错与文档使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// 起始地址（字节）。
    pub base: u32,
    /// 长度（字节）。
    pub size: u32,
}

impl Range {
    /// 结束地址（**不含**）。
    ///
    /// 只有"已确认完整落在 32 位地址空间内"的区间才能安全调用它 ——
    /// [`Range::fits_in_address_space`] 与下面的编译期断言给出这个保证。
    #[must_use]
    pub const fn end(&self) -> u32 {
        self.base + self.size
    }

    /// 地址是否落在本区间内（含起点、不含终点）。
    ///
    /// 写成 `address - self.base < self.size` 而不是 `address < self.base + self.size`：
    /// 后者在 `base + size` 越过 `u32::MAX` 时会**回绕**（本工作区开启
    /// `overflow-checks`，于是变成 panic）；前者先判 `address >= base` 保证减法不下溢，
    /// 对"上界贴着 `u32::MAX`"的区间也照样正确。
    ///
    /// 空区间（`size == 0`）不含任何地址。
    #[must_use]
    pub const fn contains(&self, address: u32) -> bool {
        address >= self.base && address - self.base < self.size
    }

    /// 两段区间是否有公共字节。
    ///
    /// 空区间不含任何字节，因此与谁也不重叠 —— 这条不是学究气：
    /// "零长度的保留区"如果被判为重叠，会逼着人去放宽一条**本该硬**的不变量。
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.size != 0 && other.size != 0 && self.base < other.end() && other.base < self.end()
    }

    /// 结束地址是否没有越过 32 位地址空间上界（即 `base + size` 不回绕）。
    #[must_use]
    pub const fn fits_in_address_space(&self) -> bool {
        self.base.checked_add(self.size).is_some()
    }

    /// 大小是否为 2 的幂**且**基址按该大小对齐。
    ///
    /// 两条一起判：布局常量（如 MPU 区域、链接脚本的段）通用要求是"基址按自身大小对齐"，
    /// 而这条要求只有在大小是 2 的幂时才谈得上。
    #[must_use]
    pub const fn is_aligned(&self) -> bool {
        self.size.is_power_of_two() && self.base.is_multiple_of(self.size)
    }
}

// ---------------------------------------------------------------------------
// 片内 / 外部内存布局：既是给 BSP 用的数据，也是编译期断言与主机测试的输入
// ---------------------------------------------------------------------------

/// 片内 ITCM：64 KiB，全部给代码（`.text`）。
///
/// 取指零等待，因此代码放这里；代价是它只有 64 KiB，而**栈与数据挤不进来**
/// —— 这正是 [`ITCM_CODE_BUDGET_BYTES`] 存在的理由。
pub const ITCM: Range = Range {
    base: 0x0000_0000,
    size: 64 * 1024,
};

/// 片内 DTCM：64 KiB，栈（48 KiB，最底部）与 Cortex-M 向量表（紧随栈顶）。
///
/// 单周期访问、不经总线仲裁，因此放"最热且必须最快"的东西。
pub const DTCM: Range = Range {
    base: 0x2000_0000,
    size: 64 * 1024,
};

/// 片内 OCRAM：128 KiB，`.rodata` / `.data` / `.bss` / `.uninit`。
///
/// 注意它从 `0x2020_0000` 而不是 `0x2000_0000` 起：FlexRAM 共 8 bank × 32 KB，
/// 而 **Boot ROM 需要一个 OCRAM bank**，于是前一个 bank 的位置被跳过。
/// 把 OCRAM 写成"紧接 DTCM 之后"是一个会立刻错 2 MiB 的直觉。
///
/// ⚠️ 这 128 KiB 里**顶部 32 KiB 划给了 [`DMA_POOL`]**（非缓存），
/// 可供链接器自由分配的只有 [`OCRAM_CACHEABLE`]。
pub const OCRAM: Range = Range {
    base: 0x2020_0000,
    size: 128 * 1024,
};

/// **非缓存 DMA 池**：OCRAM 顶部 32 KiB，**固定地址**，供 DMA 引擎的描述符与缓冲使用。
///
/// # 为什么必须非缓存（而不是事后做 clean/invalidate）
///
/// 三个 DMA 引擎都要访问内存，而它们的描述符/缓冲**不在缓存一致性域内**：
///
/// | 引擎 | 依据 |
/// |---|---|
/// | 核内 EHCI（USB OTG1） | `[RM 38.1]` *"Embedded DMA controller in each core"* |
/// | ENET uDMA | `[RM 37.1]` *"A unified DMA (uDMA), internal to the ENET module"* |
/// | eDMA（CAN/串口） | `[RM 第6章]` |
///
/// 而 I/D-Cache 各 **16 KB**（cache line 32 B）。若描述符落在可缓存区，
/// CPU 写的描述符可能还停在缓存里，DMA 看到的是**旧内容** —— 这类缺陷的症状是
/// "偶尔传错一帧"，比本轮刚踩过的"写后读回是旧值"更难查。
///
/// ⇒ 选择**让它们在地址上就不可缓存**（而不是每次传输前手工维护）。理由：
/// ① 维护点少一处就少一个漏掉的机会；② 规避勘误 `ERR011572`（Cortex-M7 的
/// Write-Through 存储/加载可能返回错数据 —— 非缓存区不涉及该路径）。
///
/// # 对齐（硬约束，来自手册而不是经验）
///
/// - **4 KB** —— USB dQH 指针与周期帧表是"4 K 页对齐"（`[RM 38.5]`：
///   *"31-12 This is a 4 K aligned pointer to physical memory"*）⇒ 池基址取 4 KB 对齐；
/// - **32 B** —— USB dTD 必须 32 字节对齐（`[RM 38.5]`），且 32 B 正是 cache line 大小
///   ⇒ 池内各子区一律按 32 B 对齐划分；
/// - ENET 的 RxBD/TxBD 具体对齐要求见 `[RM 37.5]`（**TODO-VERIFY**）；
///   按 32 B 划分不会比它更松。
///
/// # 与 MPU 的关系（**这是让它真正非缓存的唯一机制**）
///
/// 本窗口**不单独占用 MPU 区域**（8 个区域已全部占满）：把 `region6`（OCRAM）
/// 缩到 [`OCRAM_CACHEABLE`] 的大小之后，本窗口自然落回 `region3`
/// （`0x0000_0000` 起 1 GB 的**非缓存兜底**）—— 即"高编号区域优先覆盖低编号"的直接应用。
/// ⚠️ **为什么是 64 KB 而不是"刚够用的 32 KB"**：ARMv7-M 的 MPU 区域大小由
/// `RASR.SIZE` 编码，**必须是 ≥ 32 的 2 的幂** —— 第一次我按 96 KB 划分，
/// `mpu-regions` 的编译期断言当场拦下（"MPU 区域大小必须是 >= 32 的 2 的幂"）。
/// OCRAM 128 KB 只能二分为 64 KB + 64 KB，没有"32 KB 池 + 96 KB 数据"这种切法。
/// ⇒ 于是这个窗口的**全部 64 KB** 都归池，且它同时是
/// [`OCRAM_CACHEABLE`] 之外唯一合法的二分结果（不留"属性未定义"的夹缝）。
pub const DMA_POOL: Range = Range {
    base: 0x2021_0000,
    size: 64 * 1024,
};

/// OCRAM 中**可缓存**的部分（64 KiB）：`.rodata`/`.data`/`.bss`/`.uninit`。
///
/// ⚠️ **这是本项目新的紧约束**：现有四段合计约 50 KB（`.uninit` 末端实测 `0x2020_CA38`），
/// 因此**只剩约 14 KB 余量**给后续的协议状态机/队列等 `.bss` 增长。
/// 新增的 DMA 描述符与缓冲应一律放 [`DMA_POOL`]（那里有 64 KB），
/// 而**大块数据**应放 SDRAM（有 32 MiB，且已有固定块静态池）。
pub const OCRAM_CACHEABLE: Range = Range {
    base: OCRAM.base,
    size: OCRAM.size / 2,
};

// 池必须**紧贴 OCRAM 顶部**：否则 OCRAM 里会出现一段既不属于可缓存区、
// 也不属于池的"夹缝"，而那段夹缝的缓存属性取决于它落在哪个 MPU 区域 ——
// 一个没人会去核对的隐式属性。
const _: () = assert!(
    OCRAM_CACHEABLE.base + OCRAM_CACHEABLE.size == DMA_POOL.base
        && DMA_POOL.base + DMA_POOL.size == OCRAM.base + OCRAM.size
);
// 池基址必须 4 KB 对齐（USB dQH / 周期帧表要求，`[RM 38.5]`）。
const _: () = assert!(DMA_POOL.base.is_multiple_of(4096));
// 区域大小必须是 2 的幂（ARMv7-M `RASR.SIZE` 的编码要求）——
// 这条断言是"96 KB 方案"被编译期拦下的那次教训的固化。
const _: () = assert!(OCRAM_CACHEABLE.size.is_power_of_two() && DMA_POOL.size.is_power_of_two());
// 池大小必须是 32 B（cache line）的整数倍，否则最后一个 cache line 跨出池边界。
const _: () = assert!(DMA_POOL.size.is_multiple_of(32));

/// 外部 QSPI NOR 的 XIP 映射窗口：`0x6000_0000` 起 32 MiB。
///
/// 这是 FlexSPI1 的固定映射地址，不是可以商量的布局选择。整片可用（而不是只有
/// 前 16 MiB）的前提是 FCB 的 LUT 使用**专用 4 字节地址操作码**；越界访问不报错、
/// 会静默别名到低 16 MiB，因此这条前提由 `xtask check-image` 在构建期断言。
pub const FLASH_XIP: Range = Range {
    base: 0x6000_0000,
    size: 32 * 1024 * 1024,
};

/// 外部 SDRAM：`0x8000_0000` 起 32 MiB（SEMC CS0）。
///
/// 目前**没有业务负载**：流式访问没有缓存收益，要放就放大缓冲与有复用的工作集。
pub const SDRAM: Range = Range {
    base: 0x8000_0000,
    size: 32 * 1024 * 1024,
};

/// 全部片内 / 外部区域的清单：**测试与门禁遍历它**，而不是各写一遍五个名字。
///
/// 名字只用于报错与文档；判据一律基于 `base` / `size`（地址是事实，名字是文案）。
pub const REGIONS: &[(&str, Range)] = &[
    ("itcm", ITCM),
    ("dtcm", DTCM),
    ("ocram", OCRAM),
    ("flash_xip", FLASH_XIP),
    ("sdram", SDRAM),
];

// ---------------------------------------------------------------------------
// 栈：大小与两个端点
// ---------------------------------------------------------------------------

// 栈的几何先在 **`u32`**（地址空间的宽度）口径上定义，再派生出对外的 `usize` 常量。
//
// 为什么要分两层：
//
// - **对外必须是 `usize`**：BSP 侧要拿这三个值去做切片长度与指针偏移，而
//   `u32 → usize` 的转换不是 const（`TryFrom` 与 `From` 都尚未 const 化），
//   BSP 又禁止 `as` 转换 —— 于是"唯一干净写法"是这里直接给出 `usize`，
//   BSP 只需 `pub const STACK_SIZE: usize = memory_layout::STACK_SIZE;`；
// - **对内留在 `u32`**：[`Range`] 的宽度就是 32 位地址（MPU 区域、链接脚本、
//   寄存器都按 32 位地址说话），于是下面的编译期断言全部在同一口径里比较，
//   一次转换都不需要 —— 断言本身也就不会引入它要防的那类错误。
//
// 两条约束把两层钉在一起：`STACK_BOTTOM` 必须等于 [`DTCM`]`.base`、
// `STACK_BOTTOM + STACK_SIZE` 必须等于 `STACK_TOP`（都在下面的 `const _` 里）。
const STACK_SIZE_U32: u32 = 48 * 1024;
const STACK_BOTTOM_U32: u32 = DTCM.base;
const STACK_TOP_U32: u32 = STACK_BOTTOM_U32 + STACK_SIZE_U32;

/// 栈大小（字节）：48 KiB。
///
/// ⚠️ **必须与 `crates/firmware/build.rs` 的 `STACK_BYTES` 一致**。那个值既用于
/// 生成链接脚本（`imxrt-rt` 的 `.stack`），也用于生成固件里的编译期断言；不一致会得到
/// "余量算在一个不存在的区间上"的镜像。`board-support` 目前自己那份 `STACK_SIZE`
/// 也由那条断言绑住 —— 三处一起改才算改，所以本 crate 只是把**同一个数字**搬到了
/// 纯逻辑层。
///
/// 48 KiB 不是估的：8 / 16 / 32 KiB 都被真机打穿过（症状都是"串口一行都不出"），
/// 48 KiB 配栈涂抹测量后实测峰值 29,860 B、余量 39%（2026-10-13 真机抓包）。缩栈之前先有测量。
///
/// 类型是 `usize`（"用于切片与指针算术的尺寸"）：48 KiB 在两个方向都放得下，
/// 而 `u32 → usize` 的转换在 const 上下文里还不可用。这里的 `as` 是**放宽**而不是
/// 截断（32 位目标同宽、64 位宿主加宽），由 `word_bytes_conversions_are_lossless`
/// 与编译期断言共同兜住。
#[allow(clippy::as_conversions)]
pub const STACK_SIZE: usize = STACK_SIZE_U32 as usize;

/// 栈底（最低地址，**含**）：`0x2000_0000`，即 DTCM 的最底部。
///
/// `imxrt-rt` 生成的链接脚本把 `.stack` 放在 DTCM 最底部、向量表紧随其后
/// （`cargo xtask check-image` 从链接产物断言初始 SP = `0x2000_C000`）。
/// 这个值等于 [`DTCM`]`.base` 是**事实**，不是方便：栈底就是 DTCM 的下界 ——
/// 那条事实由下面的编译期断言（在 `u32` 口径上）钉住。
#[allow(clippy::as_conversions)]
pub const STACK_BOTTOM: usize = STACK_BOTTOM_U32 as usize;

/// 栈顶（**不含**）：初始 SP 就指向这里，中断入栈从这里往下增长。
#[allow(clippy::as_conversions)]
pub const STACK_TOP: usize = STACK_TOP_U32 as usize;

// ---------------------------------------------------------------------------
// 栈涂抹：纯逻辑（真正的读写内存留在 BSP）
// ---------------------------------------------------------------------------

/// 涂抹图案（指纹）。
///
/// `0` 与 `0xFF` 都可能**自然出现**在栈上（BSS 清零、未初始化区、被当作字节缓冲的栈），
/// 用它们当指纹会把"真的被写过"与"本来就是 0"混为一谈。`0xA5A5_A5A5` 是一条
/// 典型数据里几乎不会出现的位型；即便如此，它也只是**概率**上的判据：
/// 函数只回答"这个字是否等于图案"，不回答"这段字节是不是合法数据"。
pub const PAINT: u32 = 0xA5A5_A5A5;

/// 把 `words` 的前 `painted` 个字涂成 `pattern`。
///
/// # 语义
///
/// - `words[0]` 是**栈底**，最后一个元素是栈顶；涂抹**从栈底开始**，因此涂的是
///   索引 `[0, painted)`。这是"涂当前未使用的栈区"在纯逻辑里的对应物：
///   真实调用方把 `painted` 取成"从栈底到当前 SP 之间的字数"。
/// - `painted` 大于切片长度时**涂满为止**，不 panic（饱和，而不是回绕或截断索引）；
///   `painted == 0` 时什么都不涂。
///
/// # 为什么这个函数值得被测
///
/// 少涂一个字（尤其栈底那一个字）**不会**产生任何编译错误、不会 panic、
/// 也不会在别的测试里露头 —— 它只会让 [`high_water_bytes`] 从此恒定报"整段栈"。
/// 当年就是在真机横幅上看到的。所以这里刻意提供 `painted` 参数，
/// 让"漏涂"与"涂过量"都能在主机上被构造出来。
pub fn paint_prefix(words: &mut [u32], painted: usize, pattern: u32) {
    // `take` 天然处理 `painted > words.len()`：取到切片尽头就停。
    for word in words.iter_mut().take(painted) {
        *word = pattern;
    }
}

/// 自栈底向上找第一个**不是** `pattern` 的字，返回**从栈顶算起被用到过的字节数**。
///
/// # 两个极端（这条函数的语义边界）
///
/// | 情形 | 返回 |
/// |---|---|
/// | 全部仍是指纹（一个字都没被动过） | `0` |
/// | `words[0]` 就不是指纹（或全部被动过） | `words.len() * `[`WORD_BYTES`]，即**整段栈** |
/// | 前 `k` 个字仍是指纹，第 `k` 个（下标 `k`）起被动过 | `(words.len() - k) * `[`WORD_BYTES`] |
///
/// 为什么"栈底那个字不是指纹"就等于"整段栈都被用过"：栈从栈顶向下增长，
/// `words[0]` 是**最深**的那个字，它被动过意味着用量已经触底。这条语义本身是对的；
/// 变成假警报的唯一原因是**涂抹漏了它**（见 crate 文档）。
///
/// # 保守性（偏哪边错）
///
/// 判据是"是否等于指纹"，因此自由区里任何一个**杂字**（被别的东西写过、或指纹本身
/// 恰好被当成数据用过）都会把从它到栈顶的全部字节算成占用。两个方向的偏差都**偏保守**：
/// 高估用量、低估余量。宁可误报"余量不足"，也不要漏报 —— 漏报的代价是 HardFault。
#[must_use]
pub fn high_water_bytes(words: &[u32], pattern: u32) -> usize {
    for (index, word) in words.iter().enumerate() {
        if *word != pattern {
            // `index < words.len()`，减法不会下溢；饱和乘法只是为了让"理论上不可能"
            // 的超长切片也不 panic（宁可报得保守，不要因为报告本身崩掉）。
            return words.len().saturating_sub(index).saturating_mul(WORD_BYTES);
        }
    }
    0
}

/// 剩余余量（字节）：切片总字节数减去 [`high_water_bytes`]。
///
/// 与 `high_water_bytes` **互补**：两者之和恒等于 `words.len() * `[`WORD_BYTES`]。
/// 返回 0 表示"没有余量"（可能真的用满了，也可能是涂抹漏字的假象 ——
/// 两者在这一层看不出来，区别在 [`paint_prefix`] 调得对不对）。
#[must_use]
pub fn headroom_bytes(words: &[u32], pattern: u32) -> usize {
    words
        .len()
        .saturating_mul(WORD_BYTES)
        .saturating_sub(high_water_bytes(words, pattern))
}

// ---------------------------------------------------------------------------
// 预算：把"数字"集中成事实来源
// ---------------------------------------------------------------------------

/// 栈余量阈值（百分比）：低于它就算不达标。
///
/// 30% 是 `docs/架构与实现.md` §3.2 建议的门禁值，也是
/// `docs/构建与验证.md` §1.6 的判据（"栈高水位：栈填充 + 检查，余量 ≥ 30%"）。
/// 当前实测峰值 29,860 B / 48 KiB，余量 39%（2026-10-13；bench 基线记 34%）。
pub const MIN_STACK_HEADROOM_PERCENT: u32 = 30;

/// ITCM 里代码之外的预留：**0**（按用户要求取消 8 KiB 预留，ITCM 全量可放代码）。
///
/// 仍保留这个常量作为"预算推导"的唯一来源；若将来重新引入预留，只改这里。
pub const ITCM_CODE_RESERVE_BYTES: u32 = 0;

/// 代码允许占用的 ITCM 上限：**64 KiB**（ITCM 全量）。
///
/// 不是另外拍的数：它是上面两条事实相减的结果（编译期断言钉住这一点）。
pub const ITCM_CODE_BUDGET_BYTES: u32 = ITCM.size - ITCM_CODE_RESERVE_BYTES;

/// 余量是否达标：`peak_bytes` 用掉之后，剩余比例是否 >= `min_percent`%。
///
/// # 语义与边界（每条都有测试）
///
/// - **恰好等于阈值算达标**（`>=`，不是 `>`）：`total = 1000, peak = 700, min = 30`
///   → 余量 300 B = 30.0% → `true`。阈值是"允许的最坏情况"，不是"必须更好"。
/// - `peak_bytes > total_bytes` 按余量 0 处理（饱和）：这只会发生在"两个数字来自
///   不同来源"时，此时必须报**不达标**，而不是让减法回绕成一个巨大的余量。
/// - `total_bytes == 0` 且 `min_percent > 0` → **不达标**：0 字节的栈不是"余量充足"，
///   而是配置错误。
/// - `min_percent == 0` → 恒达标（包括 `total_bytes == 0`）：要求"余量 ≥ 0%"是空条件。
/// - 比较用整数：`余量 * 100 >= 总量 * min_percent`，**不用浮点** —— 这个 crate 禁掉
///   浮点算术，而百分比比较本来也不需要它（`f32` 在这里只会引入舍入方向的争论）。
/// - 两处乘法都可能溢出（极端入参）：溢出时判为**不达标**，与"减法回绕"同一个立场：
///   报告本身出错时，宁可报红灯。
#[must_use]
pub fn headroom_ok(peak_bytes: usize, total_bytes: usize, min_percent: u32) -> bool {
    if min_percent == 0 {
        return true;
    }
    if total_bytes == 0 {
        return false;
    }
    let Ok(total) = u64::try_from(total_bytes) else {
        return false;
    };
    let Ok(peak) = u64::try_from(peak_bytes) else {
        return false;
    };
    let headroom = total.saturating_sub(peak);
    match (
        headroom.checked_mul(100),
        total.checked_mul(u64::from(min_percent)),
    ) {
        (Some(lhs), Some(rhs)) => lhs >= rhs,
        _ => false,
    }
}

// 编译期不变量：布局或栈一旦改坏，**构建就失败**，而不是等到真机上一行都不出。
//
// 就地允许 `indexing_slicing`：在 **const 上下文**里，越界索引是**编译错误**，
// 而 clippy 那条 lint 担心的是运行期 panic —— 在这里不成立。
// 这里刻意只写 `while` 循环：const 上下文里没有迭代器。
#[allow(clippy::indexing_slicing)]
const _: () = {
    // 1) 栈的几何。`STACK_TOP` 必须**恰好**是栈底 + 大小：这个值就是初始 SP，
    //    `xtask check-image` 从链接产物断言它是 `0x2000_C000`；差一字节都说明
    //    "测量的区间"与"链接脚本给的区间"不是同一段。
    //
    //    这一段刻意用 `*_U32`（地址口径）：对外的 `usize` 常量只是它的派生，
    //    几何关系在这里判一次就够，而且判据里不会有任何类型转换。
    assert!(
        STACK_TOP_U32 == STACK_BOTTOM_U32 + STACK_SIZE_U32,
        "STACK_TOP 必须是 STACK_BOTTOM + STACK_SIZE"
    );
    assert!(STACK_SIZE_U32 > 0, "栈大小不能为零");
    // 栈按**字**涂抹与扫描：一个字 4 字节（这条断言把"按 4 字节处理"钉住），
    // 因此大小必须能被 4 整除 —— 否则末尾会剩下一段永远扫不到的字节。
    // 这里用字面量 4 而不是 `WORD_BYTES`：`u32::try_from` 在 const 上下文里还不可用，
    // 而 `as` 转换在本 crate 是要逐处说明的（上面那条断言已经让两者等价）。
    assert!(WORD_BYTES == 4, "一个字按 4 字节处理");
    assert!(
        STACK_SIZE_U32.is_multiple_of(4),
        "栈大小必须是 4 的倍数（涂抹与扫描都按字进行）"
    );
    assert!(
        STACK_SIZE_U32 >= 32 * 1024,
        "栈不得小于 32 KiB（真机打穿过 8/16/32 KiB）"
    );

    // 2) 栈整段落在 DTCM 内。栈顶是**不含**的一端，因此 `<=` 就够了；
    //    向量表紧随栈顶之后（632 B），也在 DTCM 里 —— 由下面的"区域不重叠"和
    //    `STACK_TOP <= DTCM.end()` 一起保证不会越出 DTCM。
    assert!(STACK_BOTTOM_U32 >= DTCM.base, "栈底不能低于 DTCM 下界");
    assert!(
        STACK_TOP_U32 <= DTCM.base + DTCM.size,
        "栈顶不能越过 DTCM 上界"
    );

    // 3) 对外 `usize` 常量的无损失性由 `public_stack_constants_widen_without_loss`
    //    在主机上证明（`usize::try_from` 在 const 上下文里还不可用，而本 crate 里
    //    每一处 `as` 都要逐处说明理由，因此不在这里再抄一遍同样的转换）。

    // 3) 每个区域：名字非空、大小是 2 的幂、基址按自身大小对齐、完整落在 32 位地址空间内。
    let mut i = 0;
    while i < REGIONS.len() {
        let (name, region) = REGIONS[i];
        assert!(!name.is_empty(), "区域必须写明名字（报错与文档都读它）");
        assert!(region.size > 0, "区域大小不能为零");
        assert!(region.size.is_power_of_two(), "区域大小应当是 2 的幂");
        assert!(
            region.base % region.size == 0,
            "区域基址必须按其自身大小对齐"
        );
        // 先判不回绕，下面的 `end()`（用 `+`）才安全。
        assert!(
            region.base <= u32::MAX - region.size,
            "区域必须完整落在 32 位地址空间内"
        );
        i += 1;
    }

    // 4) 区域两两不重叠。重叠不会报错，只会让"这块内存归谁"变成没人说得清的事。
    let mut a = 0;
    while a < REGIONS.len() {
        let mut b = a + 1;
        while b < REGIONS.len() {
            let (_, left) = REGIONS[a];
            let (_, right) = REGIONS[b];
            assert!(
                !left.overlaps(&right),
                "片内/外部区域之间不得重叠（重叠时归属无人说得清）"
            );
            b += 1;
        }
        a += 1;
    }

    // 5) 预算来自事实，而不是另拍一个数。
    assert!(
        ITCM_CODE_BUDGET_BYTES + ITCM_CODE_RESERVE_BYTES == ITCM.size,
        "ITCM 代码预算 + 保留余量必须恰好等于 ITCM 大小"
    );
    assert!(
        ITCM_CODE_BUDGET_BYTES <= ITCM.size,
        "代码预算不得超过 ITCM 大小"
    );
    assert!(
        MIN_STACK_HEADROOM_PERCENT < 100,
        "余量阈值必须小于 100%（等于 100% 就是永远不达标）"
    );
};

/// SDRAM 基址（`sdram-timing` 的 SEMC CS0 窗口起点）。
pub const SDRAM_BASE: u32 = 0x8000_0000;
/// SDRAM 容量（W9825G6KH-6I，32 MiB）。
pub const SDRAM_SIZE: u32 = 32 * 1024 * 1024;

/// 分配池在 SDRAM 中的基址。
///
/// 取 **16 MiB 偏移**而不是紧贴起点：起点那一段留给"未来可能的大块连续缓冲"
/// （例如 OTA 暂存、帧缓冲），而 16 MiB 之后仍有 16 MiB 空闲 —— 两侧都有余地。
/// 这个选择是**静态可知**的：基址与大小都是常量，写错会在编译期被下面的断言拦住。
pub const POOL_BASE: u32 = SDRAM_BASE + 16 * 1024 * 1024;
/// 分配池总字节数（2 MiB = 2048 块 × 1 KiB）。
pub const POOL_BYTES: u32 = 2 * 1024 * 1024;
/// 分配池块大小（1 KiB）。
pub const POOL_BLOCK_BYTES: u32 = 1024;
/// 分配池块数。
pub const POOL_BLOCKS: u32 = POOL_BYTES / POOL_BLOCK_BYTES;

/// **大块池**在 SDRAM 中的基址（紧接在小块池之后）。
///
/// # 为什么要有第二个池
///
/// 小块池是 **1 KiB 块**，而有些状态天然就是"几十 KB、尺寸固定、生命周期与固件同长"
/// —— 典型例子是 gzip 压缩器的状态（`miniz_oxide` 的 `CompressorOxide` 内部是若干
/// `Box`，合计约 230 KB：33 KB 字典 + 64 KB `next` + 64 KB `hash` + 64 KB 代码缓冲
/// + ~4 KB Huffman 表）。
///
/// 让小块池去"拼一个大块"会直接破坏无碎片那条论证（见 [`POOL_BASE`] 的说明），
/// 所以正确做法是**再开一个固定块池**，只是块更大：
///
/// - **仍然无碎片**：固定块、不合并、不切分；
/// - **仍然静态可知**：基址与大小是常量，下面的断言把它钉在 SDRAM 窗口内；
/// - **仍然"失败是一个值"**：`alloc` 返回空指针，绝不 panic。
///
/// 代价是"块内浪费"（33 KB 的请求占掉一整块 128 KiB）—— 在 32 MiB 的 SDRAM 上
/// 这个代价是划算的，换来的是论证的简单。
pub const BIG_POOL_BASE: u32 = POOL_BASE + POOL_BYTES;
/// 大块池总字节数（4 MiB = 32 块 × 128 KiB）。
pub const BIG_POOL_BYTES: u32 = 4 * 1024 * 1024;
/// 大块池块大小（128 KiB）。
///
/// gzip 的 `CompressorOxide` 实测有一次约 85,196 B 的分配，64 KiB 块装不下；
/// 128 KiB 仍是 2 的幂，任意 `align ≤ 128 KiB` 的请求都能被块起点满足。
pub const BIG_POOL_BLOCK_BYTES: u32 = 128 * 1024;
/// 大块池块数。
pub const BIG_POOL_BLOCKS: u32 = BIG_POOL_BYTES / BIG_POOL_BLOCK_BYTES;

// 两个池**首尾相接且不重叠** —— 分配器靠"指针落在哪个区间"来分流 `dealloc`，
// 区间重叠会让它归还到错误的池。这条必须在编译期成立。
const _: () = assert!(BIG_POOL_BASE == POOL_BASE + POOL_BYTES);
const _: () = assert!(BIG_POOL_BASE + BIG_POOL_BYTES <= SDRAM_BASE + SDRAM_SIZE);
const _: () = assert!(BIG_POOL_BLOCK_BYTES.is_power_of_two());
const _: () = assert!(BIG_POOL_BYTES.is_multiple_of(BIG_POOL_BLOCK_BYTES));
// `block_pool::BlockPool` 的位图是**字粒度**（每字 32 块）⇒ 块数必须是 32 的倍数，
// 否则会有一个字里存在"不存在的块"（策略层会拒绝，但那是运行期的事，这里拦在编译期）。
const _: () = assert!(BIG_POOL_BLOCKS.is_multiple_of(32));
// 块起点必须按块大小对齐，否则"任意 align ≤ 块大小"的保证不成立。
const _: () = assert!(BIG_POOL_BASE.is_multiple_of(BIG_POOL_BLOCK_BYTES));

// 池必须**完整落在** SDRAM 窗口内 —— 否则 GlobalAlloc 会去写别的地址空间，
// 而那种错的后果是随机的内存破坏,不是"分配失败"。这条必须在编译期成立。
const _: () = assert!(POOL_BASE >= SDRAM_BASE);
const _: () = assert!(POOL_BASE + POOL_BYTES <= SDRAM_BASE + SDRAM_SIZE);
// 块大小必须是 2 的幂、且整除池容量（否则最后一块不完整）。
const _: () = assert!(POOL_BLOCK_BYTES.is_power_of_two());
const _: () = assert!(POOL_BYTES.is_multiple_of(POOL_BLOCK_BYTES));

#[cfg(test)]
mod tests {
    use super::*;

    /// 栈大小对应的**字数**（对外的 `usize` 口径）。
    const STACK_WORDS: usize = STACK_SIZE / WORD_BYTES;

    /// 栈区间（**`u32` 地址口径**，供 [`Range`] 与地址断言使用）。
    ///
    /// 对外的 [`STACK_BOTTOM`] / [`STACK_TOP`] 是 `usize`；凡是要落进 `Range`
    /// （宽度是 32 位地址）的地方，就回到这一层 —— 与编译期断言同一口径。
    const STACK: Range = Range {
        base: STACK_BOTTOM_U32,
        size: STACK_SIZE_U32,
    };

    /// [`WORD_BYTES`] 的 `u32` 形态：测试里做地址算术用。
    ///
    /// 需要它是 `const`（`u32::try_from` 在 const 上下文里还不可用），因此就地允许
    /// 这一次 `as`；`WORD_BYTES == 4` 已由编译期断言钉住，转换不可能有损 ——
    /// 无损失这件事另有 `word_bytes_conversions_are_lossless` 一条测试兜住。
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    const WORD_U32: u32 = WORD_BYTES as u32;

    /// 模拟"程序用掉了最上面的 `used` 个字"：把它们改成**不像指纹**的内容。
    ///
    /// 真实栈上这些字会被函数帧与局部变量覆盖成任意值；这里用一个固定的非指纹值
    /// 代表"被用过"。刻意不用 0：0 是 BSS 清零后栈底残留的典型内容，
    /// 与"被用过"要能区分开。
    fn use_top_words(words: &mut [u32], used: usize) {
        let start = words.len() - used;
        for word in &mut words[start..] {
            *word = 0x1234_5678;
        }
    }

    /// **刻意复原**当年那个涂抹写法：从 `words[1]` 起涂，`words[0]` 永远涂不到。
    ///
    /// 它只存在于测试里，用途是证明下面那条回归测试**真的能抓住它** ——
    /// 否则"回归测试"只是与实现一起写出来的同义反复。
    fn paint_prefix_skipping_bottom(words: &mut [u32], pattern: u32) {
        for word in words.iter_mut().skip(1) {
            *word = pattern;
        }
    }

    // ------------------------------------------------------ 回归：漏涂栈底那个字

    #[test]
    fn missing_the_bottom_word_reports_the_whole_stack_as_used() {
        const LEN: usize = 64;
        const USED: usize = 8;
        const TOTAL: usize = LEN * WORD_BYTES;

        // ① 当年那个 bug：涂 [1, LEN)，栈底那个字保持"上电遗留的旧内容"。
        let mut buggy = [0u32; LEN];
        paint_prefix_skipping_bottom(&mut buggy, PAINT);
        // 一个字都还没被"用"的时候，假象就已经存在了：
        let mut untouched = [0u32; LEN];
        paint_prefix_skipping_bottom(&mut untouched, PAINT);
        assert_eq!(
            high_water_bytes(&untouched, PAINT),
            TOTAL,
            "漏涂栈底 → 空栈也报整段栈"
        );
        assert_eq!(headroom_bytes(&untouched, PAINT), 0);
        // 用到 8 个字之后还是"整段栈 / 余量 0"：警告完全失效（与用量无关）。
        use_top_words(&mut buggy, USED);
        assert_eq!(high_water_bytes(&buggy, PAINT), TOTAL);
        assert_eq!(headroom_bytes(&buggy, PAINT), 0);
        assert!(
            !headroom_ok(
                high_water_bytes(&buggy, PAINT),
                TOTAL,
                MIN_STACK_HEADROOM_PERCENT
            ),
            "漏涂栈底会让余量门禁**永远**报不达标（此时真实余量是 87.5%）"
        );

        // ② 正确用法（涂 [0, LEN)）在同样用量下报出真实值 —— 这一半是本测试的
        //    "牙"：把 `paint_prefix` 改成漏掉 `words[0]`，它立刻失败。
        let mut good = [0u32; LEN];
        paint_prefix(&mut good, LEN, PAINT);
        use_top_words(&mut good, USED);
        assert_eq!(high_water_bytes(&good, PAINT), USED * WORD_BYTES);
        assert_eq!(
            headroom_bytes(&good, PAINT),
            TOTAL - USED * WORD_BYTES,
            "正确涂抹下，余量必须随用量变化"
        );
        assert!(headroom_ok(
            high_water_bytes(&good, PAINT),
            TOTAL,
            MIN_STACK_HEADROOM_PERCENT
        ));
    }

    #[test]
    fn the_old_painting_loop_is_wrong_even_on_an_untouched_stack() {
        // 把"假象与用量无关"单独钉一条：同一段内存，用量从 0 变到整段，
        // 错误实现给出的答案**恒为整段**（这正是它作为告警彻底失效的原因）。
        const LEN: usize = 32;
        let mut reported = [0usize; LEN + 1];
        for (used, slot) in reported.iter_mut().enumerate() {
            let mut words = [0u32; LEN];
            paint_prefix_skipping_bottom(&mut words, PAINT);
            use_top_words(&mut words, used);
            *slot = high_water_bytes(&words, PAINT);
        }
        for (used, value) in reported.iter().enumerate() {
            assert_eq!(
                *value,
                LEN * WORD_BYTES,
                "用量 {used} 个字时仍报整段栈：这个数字不携带任何信息"
            );
        }
        // 对照：正确实现下，报出的值与用量是**一一对应**的。
        for used in 0..=LEN {
            let mut words = [0u32; LEN];
            paint_prefix(&mut words, LEN, PAINT);
            use_top_words(&mut words, used);
            assert_eq!(high_water_bytes(&words, PAINT), used * WORD_BYTES);
        }
    }

    // -------------------------------------------------------------- 涂抹与往返

    #[test]
    fn fully_painted_words_report_zero_high_water() {
        const LEN: usize = 16;
        let mut words = [0xFFFF_FFFFu32; LEN];
        paint_prefix(&mut words, LEN, PAINT);
        assert_eq!(
            high_water_bytes(&words, PAINT),
            0,
            "全是指纹 → 一个字都没用过"
        );
        assert_eq!(headroom_bytes(&words, PAINT), LEN * WORD_BYTES);
    }

    #[test]
    fn unpainted_words_report_the_whole_buffer() {
        const LEN: usize = 8;
        // 一个都不涂：整段都不等于指纹 → 报"整段都用过"（语义明确，不是错误）。
        let words = [0u32; LEN];
        assert_eq!(high_water_bytes(&words, PAINT), LEN * WORD_BYTES);
        assert_eq!(headroom_bytes(&words, PAINT), 0);
    }

    #[test]
    fn using_the_top_k_words_reports_k_words() {
        const LEN: usize = 32;
        for used in 0..=LEN {
            let mut words = [0u32; LEN];
            paint_prefix(&mut words, LEN, PAINT);
            use_top_words(&mut words, used);
            assert_eq!(
                high_water_bytes(&words, PAINT),
                used * WORD_BYTES,
                "用掉最上面 {used} 个字"
            );
            assert_eq!(
                headroom_bytes(&words, PAINT),
                (LEN - used) * WORD_BYTES,
                "用掉最上面 {used} 个字后的余量"
            );
        }
    }

    #[test]
    fn corrupting_the_first_free_word_reports_that_word_and_everything_above() {
        const LEN: usize = 16;
        const PAINTED: usize = 5;
        let mut words = [0u32; LEN];
        // 只涂了底部 5 个字（等价于"涂到某个位置为止"）。
        paint_prefix(&mut words, PAINTED, PAINT);
        for word in &words[..PAINTED] {
            assert_eq!(*word, PAINT, "底部 5 个字仍是指纹");
        }
        // 改掉下标 = PAINTED 的那个字（下标 0 起算第 PAINTED 个）：
        // 它以及它上面的一切都算"用到过"。
        words[PAINTED] = 0xDEAD_BEEF;
        assert_eq!(
            high_water_bytes(&words, PAINT),
            (LEN - PAINTED) * WORD_BYTES
        );
        assert_eq!(headroom_bytes(&words, PAINT), PAINTED * WORD_BYTES);
    }

    #[test]
    fn paint_prefix_does_not_touch_anything_past_the_prefix() {
        const LEN: usize = 8;
        const PAINTED: usize = 3;
        let mut words = [0xFFFF_FFFFu32; LEN];
        paint_prefix(&mut words, PAINTED, PAINT);
        assert_eq!(&words[..PAINTED], &[PAINT; PAINTED]);
        for word in &words[PAINTED..] {
            assert_eq!(*word, 0xFFFF_FFFF, "前缀之后的字不该被涂");
        }
        // 由此产生的语义：被涂的前缀算"未使用"，其余算"已使用"。
        assert_eq!(
            high_water_bytes(&words, PAINT),
            (LEN - PAINTED) * WORD_BYTES
        );
    }

    #[test]
    fn paint_prefix_with_zero_paints_nothing() {
        const LEN: usize = 4;
        let mut words = [7u32; LEN];
        paint_prefix(&mut words, 0, PAINT);
        assert_eq!(words, [7u32; LEN], "涂 0 个字 = 一个字都不动");
        assert_eq!(high_water_bytes(&words, PAINT), LEN * WORD_BYTES);
    }

    #[test]
    fn paint_prefix_saturates_when_asked_for_more_words_than_exist() {
        const LEN: usize = 4;
        let mut words = [0u32; LEN];
        // 大于长度：涂满为止，**不 panic**（饱和语义，与"截断索引"划清界限）。
        paint_prefix(&mut words, LEN + 1, PAINT);
        assert_eq!(words, [PAINT; LEN]);
        paint_prefix(&mut words, usize::MAX, PAINT);
        assert_eq!(words, [PAINT; LEN]);
        assert_eq!(high_water_bytes(&words, PAINT), 0);
    }

    #[test]
    fn paint_prefix_honours_an_arbitrary_pattern() {
        const LEN: usize = 4;
        let mut words = [0u32; LEN];
        paint_prefix(&mut words, LEN, 0x1234_5678);
        assert!(words.iter().all(|word| *word == 0x1234_5678));
        assert_eq!(high_water_bytes(&words, 0x1234_5678), 0);
        // 同一个缓冲区，换一个图案去扫就是完全不同的结论：图案是参数，不是全局状态。
        assert_eq!(high_water_bytes(&words, PAINT), LEN * WORD_BYTES);
    }

    // ------------------------------------------------------------------ 边界

    #[test]
    fn empty_slice_has_no_bytes_at_all() {
        let words: [u32; 0] = [];
        assert_eq!(
            high_water_bytes(&words, PAINT),
            0,
            "空切片既没有空闲也没有占用"
        );
        assert_eq!(headroom_bytes(&words, PAINT), 0);
        let mut mutable: [u32; 0] = [];
        paint_prefix(&mut mutable, 0, PAINT);
        paint_prefix(&mut mutable, 7, PAINT);
        assert_eq!(mutable.len(), 0, "空切片涂抹是空操作");
    }

    #[test]
    fn one_word_slice_is_either_free_or_used() {
        let mut free = [PAINT; 1];
        assert_eq!(high_water_bytes(&free, PAINT), 0);
        assert_eq!(headroom_bytes(&free, PAINT), WORD_BYTES);
        // 栈底那一个字被动过，就意味着"最深"的位置已经被用到 → 整段栈。
        free[0] = 0;
        assert_eq!(high_water_bytes(&free, PAINT), WORD_BYTES);
        assert_eq!(headroom_bytes(&free, PAINT), 0);
    }

    #[test]
    fn headroom_and_high_water_are_complementary() {
        const LEN: usize = 24;
        for painted in 0..=LEN {
            let mut words = [9u32; LEN];
            paint_prefix(&mut words, painted, PAINT);
            let peak = high_water_bytes(&words, PAINT);
            let headroom = headroom_bytes(&words, PAINT);
            assert_eq!(
                peak + headroom,
                LEN * WORD_BYTES,
                "峰值 + 余量必须恒等于总字节数（painted={painted}）"
            );
            assert_eq!(headroom, painted * WORD_BYTES);
        }
    }

    #[test]
    fn a_stray_word_inside_the_free_region_is_reported_conservatively() {
        // 自由区里出现杂字（被别的东西写过、或指纹恰好被当数据用过）：
        // 报告从**那个字**到栈顶的全部字节 —— 高估用量、低估余量，偏保守。
        const LEN: usize = 8;
        let mut words = [PAINT; LEN];
        words[2] = 0xAB;
        assert_eq!(high_water_bytes(&words, PAINT), (LEN - 2) * WORD_BYTES);
        assert!(
            high_water_bytes(&words, PAINT) > 2 * WORD_BYTES,
            "杂字之上的字都算占用（宁可误报余量不足，也不要漏报）"
        );
    }

    // ------------------------------------------------------------ 余量门禁

    #[test]
    fn headroom_ok_accepts_exactly_the_threshold() {
        // 1000 B 里用掉 700 B：余量恰好 30.0% —— 阈值是"允许的最坏情况"。
        assert!(headroom_ok(700, 1000, 30));
        // 整数上恰好等于：1000 里用掉 250，余量 75%。
        assert!(headroom_ok(250, 1000, 75));
    }

    #[test]
    fn headroom_ok_rejects_just_below_the_threshold() {
        // 1000 B 里用掉 701 B：余量 29.9%。
        assert!(!headroom_ok(701, 1000, 30));
        assert!(!headroom_ok(999, 1000, 30));
        assert!(!headroom_ok(1000, 1000, 30), "用满 → 余量 0%");
    }

    #[test]
    fn headroom_ok_accepts_far_above_the_threshold() {
        assert!(headroom_ok(1, 1000, 30));
        assert!(headroom_ok(0, 1000, 30), "一个字都没用 → 余量 100%");
        assert!(headroom_ok(0, 1, 100 - 1), "1 字节里一个都没用 ≈ 100%");
    }

    #[test]
    fn headroom_ok_treats_an_oversized_peak_as_zero_headroom() {
        // 峰值大于总量只可能来自"两个数字来源不同"，此时必须报不达标，
        // 而不是让饱和减法给出一个"看起来还有余量"的结论。
        assert!(!headroom_ok(1001, 1000, 30));
        assert!(!headroom_ok(usize::MAX, 1000, 30));
    }

    #[test]
    fn headroom_ok_with_a_zero_percent_threshold_is_always_satisfied() {
        // 要求"余量 ≥ 0%"是空条件 —— 包括 0 字节的栈（退化情形）。
        assert!(headroom_ok(1000, 1000, 0));
        assert!(headroom_ok(9999, 1000, 0));
        assert!(headroom_ok(0, 0, 0));
    }

    #[test]
    fn headroom_ok_rejects_a_zero_size_stack() {
        // 0 字节的栈不是"余量充足"，而是配置错误。
        assert!(!headroom_ok(0, 0, 30));
        assert!(!headroom_ok(0, 0, 1));
    }

    #[test]
    fn stack_words_matches_stack_size() {
        // 测试里用 `as` 换来一个 const 数组长度；这条测试保证那次转换是**无损**的，
        // 否则后面"按真实栈大小复现测量"的那条测试就测了个别的东西。
        assert_eq!(STACK_WORDS * WORD_BYTES, STACK_SIZE);
    }

    #[test]
    fn headroom_ok_agrees_with_the_measured_peak() {
        let total = STACK_SIZE;
        // 真机实测（启动横幅 `stack :` 行，2026-10-13 抓包）：峰值 29,860 B / 48 KiB → 余量 39%。
        let measured_peak = 29_860;
        assert!(
            headroom_ok(measured_peak, total, MIN_STACK_HEADROOM_PERCENT),
            "实测余量 39% 必须达标"
        );
        // 当年那个 bug 的假象：峰值 = 整段栈 → 余量 0% → 门禁报不达标。
        assert!(!headroom_ok(total, total, MIN_STACK_HEADROOM_PERCENT));
    }

    #[test]
    fn stack_sized_buffer_reproduces_the_measured_peak() {
        // 用与真机同尺寸的缓冲区走一遍完整流程：涂满 → 用掉 29,860 B 的**向上取整字数**
        // → 报出的峰值必须与用量一致，余量必须 > 30%。
        let total = STACK_SIZE;
        let peak_words = 29_860_usize.div_ceil(WORD_BYTES);
        let mut words = [0u32; STACK_WORDS];
        paint_prefix(&mut words, STACK_WORDS, PAINT);
        use_top_words(&mut words, peak_words);

        let peak = high_water_bytes(&words, PAINT);
        assert_eq!(peak, peak_words * WORD_BYTES);
        assert!(peak >= 29_860 && peak - 29_860 < WORD_BYTES, "向上取整到字");
        assert_eq!(headroom_bytes(&words, PAINT), total - peak);
        assert!(headroom_ok(peak, total, MIN_STACK_HEADROOM_PERCENT));
    }

    // -------------------------------------------------------------- 布局常量

    #[test]
    fn layout_matches_the_documented_board_map() {
        // 期望值直接照 `docs/硬件与板级事实.md` 与 `docs/硬件与板级事实.md`
        // 抄写，不调用本 crate 的其它函数来"互相印证"。
        assert_eq!(ITCM.base, 0x0000_0000);
        assert_eq!(ITCM.size, 64 * 1024);
        assert_eq!(DTCM.base, 0x2000_0000);
        assert_eq!(DTCM.size, 64 * 1024);
        assert_eq!(
            OCRAM.base, 0x2020_0000,
            "Boot ROM 需要一个 OCRAM bank，故不从 0x2000_0000 起"
        );
        assert_eq!(OCRAM.size, 128 * 1024);
        assert_eq!(FLASH_XIP.base, 0x6000_0000);
        assert_eq!(FLASH_XIP.size, 32 * 1024 * 1024);
        assert_eq!(SDRAM.base, 0x8000_0000);
        assert_eq!(SDRAM.size, 32 * 1024 * 1024);
        // FlexRAM 的账要平：ITCM + DTCM + OCRAM = 256 KB = 8 bank × 32 KB。
        assert_eq!(ITCM.size + DTCM.size + OCRAM.size, 256 * 1024);
    }

    #[test]
    fn layout_regions_do_not_overlap() {
        for (index, (name, region)) in REGIONS.iter().enumerate() {
            for (other_index, (other_name, other)) in REGIONS.iter().enumerate() {
                if index == other_index {
                    continue;
                }
                assert!(
                    !region.overlaps(other),
                    "{name} 与 {other_name} 重叠：归属无人说得清"
                );
            }
        }
    }

    #[test]
    fn layout_regions_fit_in_the_thirty_two_bit_address_space() {
        for (name, region) in REGIONS {
            assert!(region.fits_in_address_space(), "{name} 的结束地址回绕了");
            let end = region.end();
            assert!(end > region.base, "{name} 的结束地址必须大于起始地址");
            assert!(
                u64::from(end) <= u64::from(u32::MAX) + 1,
                "{name} 越过 32 位上界"
            );
            // 边界语义：最后一个字节在内，第一个字节之后的位置不在内。
            assert!(region.contains(region.base), "{name} 不含自己的起点");
            assert!(region.contains(end - 1), "{name} 不含自己的最后一个字节");
            assert!(!region.contains(end), "{name} 不该含结束地址（右开区间）");
        }
        // 32 位地址空间自身的两条边界（与具体区域无关，防止上面的断言被"全都宽松"化）。
        assert!(
            Range {
                base: 0,
                size: u32::MAX
            }
            .fits_in_address_space()
        );
        assert!(
            !Range {
                base: 1,
                size: u32::MAX
            }
            .fits_in_address_space()
        );
    }

    #[test]
    fn layout_regions_are_aligned_to_their_own_size() {
        for (name, region) in REGIONS {
            assert!(region.is_aligned(), "{name} 未按其自身大小对齐");
            assert!(region.size.is_power_of_two(), "{name} 的大小不是 2 的幂");
        }
        // 负例：同一段长度、错开的基址必须被判为未对齐（否则这条测试只是复读实现）。
        assert!(
            !Range {
                base: 0x2000_1000,
                size: 64 * 1024
            }
            .is_aligned()
        );
        assert!(
            !Range {
                base: 0x2000_0000,
                size: 48 * 1024
            }
            .is_aligned()
        );
    }

    #[test]
    fn stack_lives_inside_dtcm() {
        assert_eq!(STACK_BOTTOM_U32, DTCM.base, "栈底就是 DTCM 下界");
        assert!(DTCM.contains(STACK.base), "栈底必须在 DTCM 内");
        assert!(
            DTCM.contains(STACK.end() - WORD_U32),
            "栈的最后一个字在 DTCM 内"
        );
        // 栈顶是栈区间的**右开端点**：它本身是向量表的第一个字节，不属于栈。
        assert!(!STACK.contains(STACK_TOP_U32), "栈顶是右开端点，不属于栈");
        assert!(
            DTCM.contains(STACK_TOP_U32),
            "栈顶（向量表起点）仍在 DTCM 内"
        );
        assert!(STACK_TOP_U32 < DTCM.end(), "栈顶之后还要放得下向量表");
        // 用遍历把"整段栈的每一个字都在 DTCM 里"钉住，而不是只查两个端点。
        let mut address = STACK.base;
        while address < STACK_TOP_U32 {
            assert!(
                DTCM.contains(address),
                "地址 {address:#010x} 落在 DTCM 之外"
            );
            address += WORD_U32;
        }
        assert_eq!(address, STACK_TOP_U32, "栈大小必须是字的整数倍");
    }

    #[test]
    fn stack_geometry_matches_the_linker_script() {
        // `cargo xtask check-image` 从链接产物断言初始 SP = 0x2000_C000。
        // 这里把同一个事实钉在常量上：48 KiB 栈 + DTCM 最底部 = 0x2000_C000。
        // 上面两条测试走**地址口径**（`u32`，与 `Range`/DTCM 同宽）；这一段走
        // **对外口径**（`usize`，BSP 直接引用它们），并证明两层之间无损。
        assert_eq!(STACK_BOTTOM_U32, 0x2000_0000);
        assert_eq!(STACK_SIZE_U32, 48 * 1024);
        assert_eq!(STACK_TOP_U32, 0x2000_C000);
        assert_eq!(STACK_TOP_U32 - STACK_BOTTOM_U32, STACK_SIZE_U32);
        assert_eq!(STACK_SIZE_U32 % WORD_U32, 0);
        assert_eq!(usize::try_from(STACK_BOTTOM_U32).unwrap(), STACK_BOTTOM);
        assert_eq!(usize::try_from(STACK_SIZE_U32).unwrap(), STACK_SIZE);
        assert_eq!(usize::try_from(STACK_TOP_U32).unwrap(), STACK_TOP);
        assert_eq!(STACK_TOP - STACK_BOTTOM, STACK_SIZE);
        assert_eq!(STACK_SIZE, 48 * 1024);
        // 向量表（632 B）也必须落在 DTCM 里。
        let vector_table = Range {
            base: STACK_TOP_U32,
            size: 632,
        };
        assert!(DTCM.contains(vector_table.base));
        assert!(vector_table.end() <= DTCM.end(), "向量表必须落在 DTCM 内");
        assert!(!vector_table.overlaps(&STACK));
    }

    #[test]
    fn itcm_code_budget_derives_from_itcm_size() {
        assert_eq!(ITCM_CODE_RESERVE_BYTES, 0);
        assert_eq!(ITCM_CODE_BUDGET_BYTES, 64 * 1024);
        assert_eq!(
            ITCM_CODE_BUDGET_BYTES + ITCM_CODE_RESERVE_BYTES,
            ITCM.size,
            "预算与保留余量必须恰好铺满 ITCM，不能有第三种来源"
        );
        // 下面三条都是"常量之间的关系"，因此写成 const 块：它们在**编译期**求值，
        // 比运行期断言更早失败（也避免 clippy 把它们看成恒真的运行期断言）。
        const {
            assert!(ITCM_CODE_BUDGET_BYTES <= ITCM.size);
            assert!(34_036 <= ITCM_CODE_BUDGET_BYTES);
        }
    }

    #[test]
    fn headroom_threshold_is_the_documented_one() {
        assert_eq!(MIN_STACK_HEADROOM_PERCENT, 30);
        const { assert!(MIN_STACK_HEADROOM_PERCENT > 0 && MIN_STACK_HEADROOM_PERCENT < 100) };
        // 阈值与真机实测余量（39%，2026-10-13）的关系：实测必须达标，否则门禁第一天就红。
        assert!(headroom_ok(29_860, STACK_SIZE, 30));
    }

    #[test]
    fn word_bytes_conversions_are_lossless() {
        // 测试里两处 `as`（`STACK_WORDS` 与 `WORD_U32`）各自带一个就地 `allow`；
        // 这条测试证明那两次转换真的无损，而不是靠"32 位很常见"的假设：
        // 若哪天 `WORD_BYTES` 不再等于 `WORD_U32`，地址算术就会与字节算术脱节。
        assert_eq!(usize::try_from(WORD_U32).unwrap(), WORD_BYTES);
        assert_eq!(WORD_BYTES, 4);
        assert_eq!(STACK_WORDS, STACK_SIZE / WORD_BYTES);
    }

    #[test]
    fn regions_table_covers_every_named_region_once() {
        const EXPECTED: [&str; 5] = ["itcm", "dtcm", "ocram", "flash_xip", "sdram"];
        assert_eq!(REGIONS.len(), EXPECTED.len());
        for (index, name) in EXPECTED.iter().enumerate() {
            let (actual_name, region) = REGIONS[index];
            assert_eq!(actual_name, *name);
            assert!(!actual_name.is_empty());
            assert!(region.size > 0);
        }
        // 表里的每一项都必须与它的具名常量是同一份数据（不是抄了一遍）。
        assert!(REGIONS.contains(&("itcm", ITCM)));
        assert!(REGIONS.contains(&("dtcm", DTCM)));
        assert!(REGIONS.contains(&("ocram", OCRAM)));
        assert!(REGIONS.contains(&("flash_xip", FLASH_XIP)));
        assert!(REGIONS.contains(&("sdram", SDRAM)));
    }

    #[test]
    fn range_contains_is_half_open() {
        let range = Range {
            base: 0x1000,
            size: 0x100,
        };
        assert!(!range.contains(0x0FFF));
        assert!(range.contains(0x1000));
        assert!(range.contains(0x10FF));
        assert!(!range.contains(0x1100));
        // 空区间：谁也不含。
        let empty = Range {
            base: 0x1000,
            size: 0,
        };
        assert!(!empty.contains(0x1000));
        assert!(!empty.overlaps(&range));
        assert!(!range.overlaps(&empty));
        // 相接（不相交）与相交各一条。
        assert!(!range.overlaps(&Range {
            base: 0x1100,
            size: 0x10
        }));
        assert!(range.overlaps(&Range {
            base: 0x10FF,
            size: 0x10
        }));
    }

    #[test]
    fn range_end_and_word_bytes_are_consistent() {
        assert_eq!(WORD_BYTES, 4);
        assert_eq!(WORD_BYTES, core::mem::size_of::<u32>());
        // 区间口径（`u32`）：`STACK` 的终点就是栈顶。
        assert_eq!(STACK.end(), STACK_TOP_U32);
        assert_eq!(
            STACK.size / WORD_U32,
            u32::try_from(STACK_WORDS).unwrap(),
            "48 KiB / 4 = 12288 个字"
        );
        // 对外口径（`usize`）：三个常量彼此自洽，且与地址口径无损对齐。
        assert_eq!(STACK_TOP, STACK_BOTTOM + STACK_SIZE);
        assert_eq!(usize::try_from(STACK.end()).unwrap(), STACK_TOP);
        assert_eq!(STACK_WORDS, STACK_SIZE / WORD_BYTES);
    }
}
