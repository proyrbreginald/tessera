//! SDRAM 静态池 + `GlobalAlloc`：**让"动态分配"与"内存静态可知"同时成立**。
//!
//! # 为什么这样做（以及为什么它不推翻安全论证）
//!
//! [`ADR-0003`](../../../docs/adr/0003-不使用alloc与async.md) 否决 `alloc` 的理由是
//! "内存用量静态可知、不存在耗尽/碎片导致的非确定性"。这个实现把这个理由**保留**下来：
//!
//! - **内存静态可知**：池就是 SDRAM 里常量地址 + 常量大小的**两段**区域
//!   （小块池 [`memory_layout::POOL_BASE`] / 大块池 [`memory_layout::BIG_POOL_BASE`]，
//!   并有编译期断言把两段都钉在 SDRAM 窗口内、且**首尾相接不重叠**）。
//!   不存在"堆能长到多大"这个问题。
//! - **无碎片**：两个池**各自**都是**固定块大小**（1 KiB / 128 KiB）⇒
//!   不可能出现"总空闲够、但没有连续块"的状态。**不做跨池拼块、不做块内切分。**
//! - **失败是一个值**：`alloc` 返回空指针（**绝不 panic**）；`dealloc` 遇到重复释放
//!   或越界时**不改位图**、只记一笔统计 —— 策略部分在纯逻辑 crate
//!   [`block_pool`] 里，主机上有测试。
//!
//! # 契约（必须让调用方知道，否则会踩坑）
//!
//! - 请求**按大小分流**，分流规则是确定的：
//!
//!   | 请求 | 落点 | 对齐要求 |
//!   |---|---|---|
//!   | `≤ 1 KiB` | 小块池（1 KiB × 2048 块） | `≤ 8` |
//!   | `≤ 128 KiB` | **大块池（128 KiB × 32 块）** | `≤ 128 KiB`（块起点天然满足） |
//!   | 更大 | **拒绝**（返回空指针） | — |
//!
//!   超过 128 KiB 一律失败：不去"偷偷拿两块拼一个大的"——那会把无碎片这条论证破坏掉。
//! - **需要超过 64 KiB 的缓冲仍然要声明静态区**（或直接落在 SDRAM 的空闲一段上），
//!   那样尺寸在链接期确定、可被门禁断言。
//! - `dealloc` 按**指针落在哪个区间**回流（不按 `layout.size()` 猜）：两个池区间
//!   首尾相接且不重叠，由编译期断言保证。
//! - 池的元数据（位图）在 `.bss`，不是 SDRAM；SDRAM **不参与 C 启动的零初始化**，
//!   所以我们从不依赖池内存的初值（`alloc` 不做隐式清零；需要清零用 `alloc_zeroed`）。
//!
//! # 谁在用
//!
//! 目前只有终端（`alloc` 的 `Vec`/`String` 便于把诊断信息排版成人能读的样子）。
//! 控制路径（总线收发与状态机）**一律不使用**它 —— 那些路径的缓冲区都是
//! 编译期定容的 `heapless` 容器。

// 本文件会做**地址与常量宽度**的转换（`u32` 常量地址 ↔ `usize`、块大小 ↔ `usize`）。
// 逐处放行会把文件淹没在注解里，因此在文件级放行这两条 lint，并在每次使用时写明
// 为什么无损（常量值远小于 `usize` 上界；地址常量本就是平台地址宽度）。
// 其余 BSP 文件仍受工作区级 `deny(as_conversions)` 约束。
#![allow(clippy::as_conversions, clippy::cast_possible_truncation)]

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use block_pool::BlockPool;
use memory_layout::{
    BIG_POOL_BASE, BIG_POOL_BLOCK_BYTES, BIG_POOL_BLOCKS, BIG_POOL_BYTES, POOL_BASE,
    POOL_BLOCK_BYTES, POOL_BLOCKS, POOL_BYTES,
};

/// 块位图字数：每字 32 块。`2048 / 32 = 64`。
///
/// 放行 `as`：`POOL_BLOCKS` 是常量 2048，收窄到 `usize` 在任何平台上都不可能截断。
const WORDS: usize = (POOL_BLOCKS / 32) as usize;

/// 池的位图（元数据在 `.bss`，不进 SDRAM）。
static POOL: critical_section::Mutex<core::cell::RefCell<BlockPool<WORDS>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(BlockPool::new()));

/// 大块池的位图字数：每字 32 块。`32 / 32 = 1`。
///
/// ⚠️ `block_pool::BlockPool` 的位图是**字粒度**，所以块数必须是 32 的倍数 ——
/// 这条由 `memory-layout` 的编译期断言钉住（块数取少了会得到一个"字里存在不存在的块"）。
const BIG_WORDS: usize = (BIG_POOL_BLOCKS / 32) as usize;

/// **大块池**的位图。与小块池完全独立：各自固定块、各自记账、互不借还。
static BIG_POOL: critical_section::Mutex<core::cell::RefCell<BlockPool<BIG_WORDS>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(BlockPool::new()));

/// 因请求过大/对齐不足而**拒绝**的次数（与"池满"分开计：两者的处置完全不同）。
static REJECTED_LAYOUT: AtomicU32 = AtomicU32::new(0);
/// 高位地址：分配出去的最大字节数（用于终端显示"用过多大"）。
static PEAK_USED_BYTES: AtomicU32 = AtomicU32::new(0);
/// 大块池上因"请求过大/对齐不足"而拒绝的次数。
static BIG_REJECTED_LAYOUT: AtomicU32 = AtomicU32::new(0);
/// 大块池的高水位（字节）。
static BIG_PEAK_USED_BYTES: AtomicU32 = AtomicU32::new(0);

/// 池基址指针。
fn base() -> *mut u8 {
    // 由**常量地址**构造指针：用 `without_provenance_mut` 而不是 `as` 转换，
    // 语义更准（这不是从某个已有指针派生的地址，而是我们自己的静态区域）。
    core::ptr::without_provenance_mut(POOL_BASE as usize)
}

/// 池的分配器单例（`#[global_allocator]` 指向它）。
pub struct SdramPool;

/// 池的当前状态（终端 `mem` 命令打印）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// 池总字节数。
    pub total_bytes: u32,
    /// 块大小。
    pub block_bytes: u32,
    /// 总块数。
    pub blocks: u32,
    /// 当前占用块数。
    pub used_blocks: u32,
    /// 历史最高同时占用块数。
    pub high_water_blocks: u32,
    /// 因池满而失败的分配次数。
    pub exhaustion_failures: u32,
    /// 因"请求过大/对齐不足"而拒绝的次数。
    pub rejected_layout: u32,
    /// 被拒绝的释放次数（重复释放 + 越界）。
    pub rejected_releases: u32,
    /// 当前是否全空闲。
    pub empty: bool,

    // ---- 大块池（128 KiB × 32 块）：给"尺寸固定、生命周期长"的大缓冲用 ----
    /// 大块池块大小。
    pub big_block_bytes: u32,
    /// 大块池总块数。
    pub big_blocks: u32,
    /// 大块池当前占用块数。
    pub big_used_blocks: u32,
    /// 大块池历史最高同时占用块数。
    pub big_high_water_blocks: u32,
    /// 大块池上因"请求过大/对齐不足"而拒绝的次数。
    pub big_rejected_layout: u32,
    /// 大块池被拒绝的释放次数（重复释放 + 越界）。
    pub big_rejected_releases: u32,
}

/// 读一份池状态（在临界区里读位图，保证与 `alloc`/`dealloc` 不撕裂）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
#[must_use]
pub fn stats() -> PoolStats {
    let (used, high_water, exhaust, rejected_releases, empty) = crate::rt::measured_cs(|cs| {
        let pool = POOL.borrow(cs).borrow();
        (
            pool.used_count(),
            pool.high_water(),
            pool.exhaustion_failures(),
            pool.rejected_releases(),
            pool.is_empty(),
        )
    });
    let (big_used, big_high_water, big_rejected_releases) = crate::rt::measured_cs(|cs| {
        let pool = BIG_POOL.borrow(cs).borrow();
        (
            pool.used_count(),
            pool.high_water(),
            pool.rejected_releases(),
        )
    });
    PoolStats {
        total_bytes: POOL_BYTES,
        block_bytes: POOL_BLOCK_BYTES,
        blocks: POOL_BLOCKS,
        used_blocks: used,
        high_water_blocks: high_water,
        exhaustion_failures: exhaust,
        rejected_layout: REJECTED_LAYOUT.load(Ordering::Relaxed),
        rejected_releases,
        empty,
        big_block_bytes: BIG_POOL_BLOCK_BYTES,
        big_blocks: BIG_POOL_BLOCKS,
        big_used_blocks: big_used,
        big_high_water_blocks: big_high_water,
        big_rejected_layout: BIG_REJECTED_LAYOUT.load(Ordering::Relaxed),
        big_rejected_releases,
    }
}

impl PoolStats {
    /// 已用字节数。
    #[must_use]
    pub const fn used_bytes(&self) -> u32 {
        self.used_blocks * self.block_bytes
    }

    /// 占用率（百分比，整数）。
    #[must_use]
    pub const fn used_percent(&self) -> u32 {
        if self.blocks == 0 {
            return 0;
        }
        (self.used_blocks * 100) / self.blocks
    }
}

/// 由块号算出块地址。
fn block_address(index: block_pool::BlockIndex) -> *mut u8 {
    let offset = index.get() * POOL_BLOCK_BYTES as usize;
    // SAFETY: `index` 来自池策略，必然 `< POOL_BLOCKS` ⇒ 偏移落在池内；
    // 池本身已被 4 条编译期断言钉在 SDRAM 窗口内。这里只做地址运算，不解引用。
    unsafe { base().add(offset) }
}

/// 由块号算出**大块池**里的块地址。
///
/// 用 `wrapping_add` 而不是 `add`：`add` 是 `unsafe`（它要求结果留在同一分配对象内），
/// 而 `wrapping_add` 是**安全方法**。这里的偏移由池策略给出、必然落在池内，
/// 但"必然"不需要靠 `unsafe` 来表达 —— 少一个 `unsafe` 块就少一条要论证的东西，
/// 而 `unsafe.budget` 的预算是**只减不增**的。
fn big_block_address(index: block_pool::BlockIndex) -> *mut u8 {
    let offset = index.get() * BIG_POOL_BLOCK_BYTES as usize;
    big_base().wrapping_add(offset)
}

/// 大块池基址指针。
fn big_base() -> *mut u8 {
    core::ptr::without_provenance_mut(BIG_POOL_BASE as usize)
}

/// 更新"峰值字节数"（只增不减；用 CAS 循环而不是读-改-写，避免与中断里的分配互相覆盖）。
fn raise_peak(peak_cell: &AtomicU32, candidate: u32) {
    let mut peak = peak_cell.load(Ordering::Relaxed);
    while candidate > peak {
        match peak_cell.compare_exchange_weak(peak, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => peak = actual,
        }
    }
}

// SAFETY: `GlobalAlloc` 的契约要求实现满足"返回的块与本分配器其它块的区域不重叠、
// 且 `dealloc` 收到的指针必来自本分配器的 `alloc`"。本实现满足：
// - **两个池互不重叠**（由 `memory-layout` 的编译期断言 `BIG_POOL_BASE == POOL_BASE + POOL_BYTES`
//   钉住），块来自各自的固定池位图（`BlockPool` 保证一个块只被发出一次，
//   `take`/`release` 都在临界区里，主机测试覆盖了重复释放与越界）；
// - 请求超过池的块大小或对齐要求时**返回空指针**，不做任何拼块/切分 ⇒ 不存在交错区域；
// - `dealloc` 按**指针落在哪个区间**回流（不按 `layout.size()` 猜），
//   反推不出块号（或重复归还）时由池策略拒绝并计数，
//   不 panic、不改位图（这一点比"遵守契约的 UB"更强：我们把 UB 变成了可观测的错误）。
unsafe impl GlobalAlloc for SdramPool {
    /// # Safety
    ///
    /// 由 `GlobalAlloc` 的契约约束：调用方必须保证 `layout.size() != 0`（标准库的
    /// `alloc` 包装已经保证这一点）。本实现自身不依赖任何调用方不变量 ——
    /// 请求过大/对齐不足时返回空指针，**不会** panic。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();

        // 小块池：1 KiB 块，块起点按 1 KiB 对齐 ⇒ 任意 align ≤ 8 都满足
        // （不变量：`POOL_BASE` 是 16 MiB 偏移，天然 1 KiB 对齐）。
        if size <= POOL_BLOCK_BYTES as usize && align <= 8 {
            return small_alloc();
        }
        // 大块池：64 KiB 块，块起点按 64 KiB 对齐 ⇒ 任意 align ≤ 64 KiB 都满足。
        if size <= BIG_POOL_BLOCK_BYTES as usize && align <= BIG_POOL_BLOCK_BYTES as usize {
            return big_alloc();
        }
        // 连大块池都装不下 —— **拒绝**，不拼块。
        REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
        ptr::null_mut()
    }

    /// # Safety
    ///
    /// 由 `GlobalAlloc` 的契约约束：`pointer` 必须来自**本**分配器的 `alloc`，
    /// 且 `layout` 与当初分配时一致。本实现进一步**不依赖**这条保证：落在两个池区间
    /// 之外的指针、反推不出块号的指针、或归还本来就空闲的块，都只记一笔统计并返回，
    /// **不改位图、不 panic**。
    ///
    /// ⚠️ 这里**刻意不看 `_layout`**：分流按**地址区间**做。按 `layout.size()` 猜会在
    /// "调用方传了不一致的 layout"时把大块池的块还给小块池 —— 那是内存破坏，
    /// 而不是一次可观测的拒绝。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        if pointer.is_null() {
            REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let address = pointer as usize;
        let small_base = POOL_BASE as usize;
        if address >= small_base && address < small_base + POOL_BYTES as usize {
            small_release(pointer);
            return;
        }
        let big_base_addr = BIG_POOL_BASE as usize;
        if address >= big_base_addr && address < big_base_addr + BIG_POOL_BYTES as usize {
            big_release(pointer);
            return;
        }
        REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
    }

    /// # Safety
    ///
    /// 同 [`Self::alloc`]：由 `GlobalAlloc` 的契约约束调用方，本实现不额外要求不变量。
    /// 清零范围严格是 `layout.size()` —— `alloc_zeroed` 的契约只要求"返回的块已清零"，
    /// 把整块（1 KiB / 128 KiB）都清掉是**多余**的工作，在 64 KiB 块上尤其浪费。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: 走同一个 `alloc`；成功时把**请求的那一段**清零。
        let address = unsafe { self.alloc(layout) };
        if !address.is_null() {
            // SAFETY: `alloc` 成功 ⇒ 至少 `layout.size()` 字节归调用方所有。
            unsafe { ptr::write_bytes(address, 0, layout.size()) };
        }
        address
    }
}

/// 小块池分配。
fn small_alloc() -> *mut u8 {
    let Some(index) = crate::rt::measured_cs(|cs| POOL.borrow(cs).borrow_mut().take()) else {
        return ptr::null_mut();
    };
    let address = block_address(index);
    let used_bytes =
        crate::rt::measured_cs(|cs| POOL.borrow(cs).borrow().used_count() * POOL_BLOCK_BYTES);
    raise_peak(&PEAK_USED_BYTES, used_bytes);
    address
}

/// 小块池归还。
fn small_release(pointer: *mut u8) {
    let offset = (pointer as usize).wrapping_sub(POOL_BASE as usize);
    if !offset.is_multiple_of(POOL_BLOCK_BYTES as usize) {
        REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let block = offset / POOL_BLOCK_BYTES as usize;
    let Some(index) = block_pool::BlockIndex::new(block) else {
        REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let _ = crate::rt::measured_cs(|cs| POOL.borrow(cs).borrow_mut().release(index));
}

/// 大块池分配。
fn big_alloc() -> *mut u8 {
    let Some(index) = crate::rt::measured_cs(|cs| BIG_POOL.borrow(cs).borrow_mut().take()) else {
        // 大块池满 —— 与"请求过大"分开：这里的处置是"等别的持有者释放"，不是"改请求"。
        return ptr::null_mut();
    };
    let address = big_block_address(index);
    let used_bytes = crate::rt::measured_cs(|cs| {
        BIG_POOL.borrow(cs).borrow().used_count() * BIG_POOL_BLOCK_BYTES
    });
    raise_peak(&BIG_PEAK_USED_BYTES, used_bytes);
    address
}

/// 大块池归还。
fn big_release(pointer: *mut u8) {
    let offset = (pointer as usize).wrapping_sub(BIG_POOL_BASE as usize);
    if !offset.is_multiple_of(BIG_POOL_BLOCK_BYTES as usize) {
        BIG_REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let block = offset / BIG_POOL_BLOCK_BYTES as usize;
    let Some(index) = block_pool::BlockIndex::new(block) else {
        BIG_REJECTED_LAYOUT.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let _ = crate::rt::measured_cs(|cs| BIG_POOL.borrow(cs).borrow_mut().release(index));
}

/// 清零各类**计数**（自检结束后调用；位图不动，见 `BlockPool::reset_counters`）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
pub fn reset_counters() {
    crate::rt::measured_cs(|cs| POOL.borrow(cs).borrow_mut().reset_counters());
    crate::rt::measured_cs(|cs| BIG_POOL.borrow(cs).borrow_mut().reset_counters());
    REJECTED_LAYOUT.store(0, Ordering::Relaxed);
    BIG_REJECTED_LAYOUT.store(0, Ordering::Relaxed);
    PEAK_USED_BYTES.store(0, Ordering::Relaxed);
    BIG_PEAK_USED_BYTES.store(0, Ordering::Relaxed);
}

/// 池当前是否全空闲（诊断用）。
#[must_use]
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
pub fn is_empty() -> bool {
    let snapshot = stats();
    snapshot.empty && snapshot.big_used_blocks == 0
}
