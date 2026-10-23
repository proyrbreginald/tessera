//! **非缓存 DMA 池**的访问与自证。
//!
//! 池的地址、大小与"为什么必须非缓存"见 [`memory_layout::DMA_POOL`] 的文档；
//! 让它在 MPU 上真正不可缓存的机制见 [`mpu_regions`] 的 `region6`（缩到 64 KB，
//! 于是池这 64 KB 落回 `region3` 的 1 GB 非缓存兜底区域）。
//!
//! # 这个模块存在的意义：**把"属性对不对"变成可复现的读数**
//!
//! MPU 区域的属性是"配置了但看不见"的典型：写错一个位，程序照样跑，
//! 只是 DMA 偶尔读到旧描述符。所以这里给出一条**不需要 DMA 参与**的自证 ——
//! 用"脏行能否在失效后存活"来区分可缓存与非缓存（原理见 [`probe_non_cacheable`]）。

use memory_layout::DMA_POOL;

/// 池的基址（`memory_layout` 已用编译期断言保证它 4 KB 对齐）。
///
/// `DMA_POOL.base` 是 `u32`（地址在本芯片上必然是 32 位），这里用**检查式**转换
/// 而不是 `as`：`board-support` 的 lint 头禁止 `as`（它能静默截断）。
/// `unwrap_or` 的兜底值取 0 —— 若转换失败，自证会当场失败，而不是去碰一个错地址。
pub const POOL_BASE: usize = pool_constant(DMA_POOL.base);

/// 池的字节数。
pub const POOL_BYTES: usize = pool_constant(DMA_POOL.size);

/// 把池的 `u32` 地址/长度转成 `usize`（**const 上下文**）。
///
/// 为什么这里允许 `as`：`usize` 在目标平台（`thumbv7em-none-eabihf`）就是 32 位，
/// 宽窄相同 ⇒ 转换不可能丢信息；而 `usize::try_from` 在 const 上下文里**还不是 stable**
/// （`TryFrom` 尚未 const 稳定化，本工具链实测报错 `cannot call ... in constants`）。
/// 这与 `board-support/src/config.rs` 的 `flash_offset()` 是同一取舍、同一理由。
const fn pool_constant(value: u32) -> usize {
    #[allow(clippy::as_conversions)]
    let converted = value as usize;
    converted
}

/// **自证：池所在区域确实是非缓存的。**
///
/// # 判据（为什么这条能区分"可缓存"与"非缓存"）
///
/// 做法是"写两个值 + 失效 + 读回"：
///
/// 1. 先往池头写一个**已知的旧值**（`0xAAAA_AAAA`）；
/// 2. 再改写成一个**新值**（`0x5555_5555`）；
/// 3. **失效**覆盖池头的 D-Cache 行；
/// 4. 读回。
///
/// - 若区域**非缓存**：两次写都直达 RAM，失效对 RAM 无影响 ⇒ 读回是**新值** ✓；
/// - 若区域**可缓存（写回）**：新值还在脏行里；第 3 步把脏行**丢弃** ⇒
///   RAM 里仍是旧值 ⇒ 读回是**旧值**（或 RAM 上电后的任意内容），**不是**新值 ✗。
///
/// ⇒ 读回等于新值 **当且仅当**该区域非缓存。这条判据不依赖 DMA、不依赖器件，
/// 因此每次启动都能跑，且结果只有两种。
///
/// # 副作用
///
/// 只碰池头的前 32 字节，跑完清零（池是共享资源，绝不留脏数据）。
/// 启动阶段池还没有属主，因此这是安全的调用时机。
#[must_use]
pub fn probe_non_cacheable() -> bool {
    /// 探测的字节数 = 一个 cache line（32 B = 8 个字）。
    const WORDS: usize = 8;
    const OLD: u32 = 0xAAAA_AAAA;
    const NEW: u32 = 0x5555_5555;

    // 创建裸指针本身是安全操作；解引用才需要 unsafe（见下面唯一的块）。
    let head = core::ptr::with_exposed_provenance_mut::<u32>(POOL_BASE);

    // SAFETY:
    // ① **地址合法**：`POOL_BASE` 是 `memory_layout::DMA_POOL` 的基址，一个**编译期常量**，
    //    已由该 crate 的 `const` 断言保证它落在 OCRAM 内、4 KB 对齐、大小是 32 的倍数；
    // ② **归属唯一**：池在启动阶段（`board::init` 之后、任何 DMA 引擎被初始化之前）
    //    没有任何属主 —— 本函数是它在启动期的唯一访问者；调用点见 `report_dma_pool`；
    // ③ **不越界**：只访问 `head[0..8]`，共 32 字节，远小于 `POOL_BYTES`；
    // ④ **对齐**：`u32` 访问要求 4 字节对齐，而基址是 4 KB 对齐的；
    // ⑤ `read_volatile`/`write_volatile` 正是"这块内存会被外人（DMA）看到"的语义，
    //    不能用普通读写 —— 普通读写会被优化掉或与缓存维护重排。
    unsafe {
        for index in 0..WORDS {
            core::ptr::write_volatile(head.add(index), OLD);
        }
        for index in 0..WORDS {
            core::ptr::write_volatile(head.add(index), NEW);
        }
        // 失效覆盖池头的 D-Cache 行：若是可缓存区域，这一步会丢弃刚写的脏行。
        crate::cache::invalidate_dcache_range(POOL_BASE, WORDS * 4);

        let mut survived = true;
        for index in 0..WORDS {
            if core::ptr::read_volatile(head.add(index)) != NEW {
                survived = false;
            }
        }

        // 收尾：清零，别在共享缓冲里留探测图案。
        for index in 0..WORDS {
            core::ptr::write_volatile(head.add(index), 0);
        }
        crate::cache::invalidate_dcache_range(POOL_BASE, WORDS * 4);

        survived
    }
}
