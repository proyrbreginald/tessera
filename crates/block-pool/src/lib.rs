//! `block-pool` —— 固定块静态池的**策略**（纯逻辑，主机可测；**平台需求 PF-2**）。
//!
//! # 为什么把"分配策略"和"内存本身"拆开
//!
//! 2026-10-12 的决策：在 SDRAM 上放一个**静态池**并用它实现 `GlobalAlloc`。
//! 这个方案能不能守住"内存静态可知"这条安全论证，取决于两件事分开做：
//!
//! - **内存本身**（哪一段地址、多大）→ 留在 BSP，链接期确定，可被门禁断言；
//! - **分配策略**（哪块空、哪块忙、高水位、耗尽）→ 放在这里，纯逻辑、零 unsafe、
//!   主机上 `cargo test` 反复跑。
//!
//! 这与 [`mpu-regions`](../mpu-regions/src/lib.rs) 的分工是同一条理由：
//! **位域/索引这类算术错，应当在 `cargo test` 里失败，而不是在目标现场失败。**
//!
//! # 为什么值得一个 crate
//!
//! `GlobalAlloc` 的实现本身只有几十行 `unsafe`（拿地址、还地址），真正会出错的是
//! **策略**：重复释放、越界释放、耗尽后有没有被记下来、高水位是不是真的高水位。
//! 这些全是纯逻辑，且**恰恰是"上线后才会暴露"的那类错**——把它们放到主机上测掉。
//!
//! # 契约（决定了它为什么"安全"）
//!
//! - 固定块大小 ⇒ **无碎片**（不存在"总空闲够但没有连续块"的状态）；
//! - 分配失败是**一个值**（`None` / [`PoolError`]），不是 panic；
//! - 所有集合尺寸都是编译期常量 ⇒ 内存用量静态可知，这条论证**不因引入池而失效**。
#![cfg_attr(not(test), no_std)]
// 纯位运算与算术：本 crate 存在的意义之一就是能被主机测试反复跑。
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
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )
)]
// 测试里放宽：断言失败就该直接 panic。

#[cfg(test)]
extern crate std;

use core::fmt;

/// 块号（`u16` ⇒ 单池最多 65535 块）。
///
/// 用 newtype 而不是裸 `usize`：块号与"第几个字节""第几次访问"在同一段代码里
/// 经常同时出现，裸整数混用是这类代码最典型的错法（真机上表现为"释放了别人的块"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockIndex(u16);

impl BlockIndex {
    /// 由裸索引构造；超出 `u16` 范围返回 `None`（**不截断**）。
    ///
    /// 显式比较而不是 `as` 截断：`as` 会静默截断，而"块号被截断"正是这里最不能容忍的错
    /// （会释放别人的块）。越界在这里变成 `None`，由调用方当作一次拒绝计数。
    ///
    /// 不用 `u16::try_from` 是因为它在 `const fn` 里尚不可用（`TryFrom` 未 const 稳定）。
    // 放行 `as`：这里的两个转换都是**可证无损**的 —— `u16::MAX as usize` 是放宽方向，
    // 而 `raw as u16` 只在前一行已确认 `raw <= u16::MAX` 之后执行。
    #[must_use]
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    pub const fn new(raw: usize) -> Option<Self> {
        if raw > u16::MAX as usize {
            return None;
        }
        // 到这里 `raw <= u16::MAX`，截断不可能发生。
        let narrowed = raw as u16;
        Some(Self(narrowed))
    }

    /// 取回裸索引。
    // 放行 `as`：`u16 -> usize` 是放宽方向，任何目标平台上都不丢信息。
    #[must_use]
    #[allow(clippy::as_conversions)]
    pub const fn get(self) -> usize {
        // `u16 -> usize` 是**放宽**方向，任何目标平台上都不会丢信息。
        self.0 as usize
    }
}

impl fmt::Display for BlockIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// 池操作失败的原因。
///
/// 全部是**可判定的值**，没有"未定义"这一档：调用方必须处理，且处理方式是明确的
/// （多数情况是"记一笔统计 + 返回 NULL / 报错"，而不是 panic）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolError {
    /// 释放一个本来就在空闲的块 —— 典型原因是**重复释放**（double free）。
    AlreadyFree,
    /// 块号超出本池范围。
    OutOfRange,
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFree => write!(f, "该块本来就空闲（重复释放？）"),
            Self::OutOfRange => write!(f, "块号超出本池范围"),
        }
    }
}

/// 固定块池的位图策略：`WORDS` 个 `u32` ⇒ `WORDS * 32` 块。
///
/// 位为 `1` = **已占用**（与"空闲位图"相反，是为了让"全零 = 全空闲"这一初始状态
/// 在内存里更直观：上电后池是空的，不需要任何初始化循环）。
pub struct BlockPool<const WORDS: usize> {
    used: [u32; WORDS],
    /// 曾经同时占用的最大块数（高水位）—— 它是"池到底够不够"的唯一实测依据。
    high_water: u32,
    /// 因池满而分配失败的次数（**不许静默**：上层要把它报到终端）。
    exhaustion_failures: u32,
    /// 被拒绝的释放次数（重复释放 / 越界）—— 同样要可观测。
    rejected_releases: u32,
}

impl<const WORDS: usize> Default for BlockPool<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> BlockPool<WORDS> {
    /// 本池的块总数。
    pub const BLOCKS: usize = WORDS * 32;

    /// 全新池：**全空闲**（全零位图，无需初始化循环）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            used: [0; WORDS],
            high_water: 0,
            exhaustion_failures: 0,
            rejected_releases: 0,
        }
    }

    /// 取一个空闲块（**首个**空闲，O(位图字数)）。
    ///
    /// 返回 `None` 表示池满，同时把这次失败计入 [`Self::exhaustion_failures`]。
    // 放行 `as`：块号上界是 `WORDS * 32`，`WORDS` 由调用方给常量（本工程用 64），
    // 远小于 `u32::MAX`；这里 `bit: u32 -> usize` 是放宽方向。
    #[allow(clippy::as_conversions)]
    pub fn take(&mut self) -> Option<BlockIndex> {
        for (word_index, word) in self.used.iter_mut().enumerate() {
            if *word == u32::MAX {
                continue;
            }
            // `!*word` 的最低位就是首个空闲块。
            let bit = (!*word).trailing_zeros();
            *word |= 1u32 << bit;
            let index = word_index * 32 + bit as usize;
            let used_now = self.used_count();
            if used_now > self.high_water {
                self.high_water = used_now;
            }
            return BlockIndex::new(index);
        }
        self.exhaustion_failures = self.exhaustion_failures.saturating_add(1);
        None
    }

    /// 归还一个块。
    ///
    /// 重复释放与越界都**不会**改动位图，只记一笔 [`Self::rejected_releases`] 并返回原因。
    /// 这一条是本 crate 存在的主要理由：`GlobalAlloc::dealloc` 的契约要求"重复释放是 UB"，
    /// 我们在**池这一层**把它变成可判定的错误，从而不让它退化成静默的内存损坏。
    pub fn release(&mut self, index: BlockIndex) -> Result<(), PoolError> {
        if index.get() >= Self::BLOCKS {
            self.rejected_releases = self.rejected_releases.saturating_add(1);
            return Err(PoolError::OutOfRange);
        }
        let word_index = index.get() / 32;
        let bit = index.get() % 32;
        let Some(word) = self.used.get_mut(word_index) else {
            self.rejected_releases = self.rejected_releases.saturating_add(1);
            return Err(PoolError::OutOfRange);
        };
        let mask = 1u32 << bit;
        if *word & mask == 0 {
            self.rejected_releases = self.rejected_releases.saturating_add(1);
            return Err(PoolError::AlreadyFree);
        }
        *word &= !mask;
        Ok(())
    }

    /// 某块当前是否被占用（越界视为"占用"，即**不可分配**，保守取安全方向）。
    #[must_use]
    pub fn is_used(&self, index: BlockIndex) -> bool {
        let (word_index, bit) = (index.get() / 32, index.get() % 32);
        match self.used.get(word_index) {
            Some(word) => word & (1u32 << bit) != 0,
            None => true,
        }
    }

    /// 当前占用块数。
    #[must_use]
    pub fn used_count(&self) -> u32 {
        self.used.iter().map(|word| word.count_ones()).sum()
    }

    /// 当前空闲块数。
    #[must_use]
    // 放行 `as`：`BLOCKS` 与 `used_count()` 都不超过 `WORDS * 32`（本工程 2048），
    // 远小于 `u32::MAX`；在 64 位主机上这个收窄也不可能截断。
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    pub fn free_count(&self) -> u32 {
        Self::BLOCKS as u32 - self.used_count()
    }

    /// 历史最高同时占用块数。
    #[must_use]
    pub const fn high_water(&self) -> u32 {
        self.high_water
    }

    /// 因池满而失败的分配次数。
    #[must_use]
    pub const fn exhaustion_failures(&self) -> u32 {
        self.exhaustion_failures
    }

    /// 被拒绝的释放次数（重复释放 + 越界）。
    #[must_use]
    pub const fn rejected_releases(&self) -> u32 {
        self.rejected_releases
    }

    /// 清零**各类计数**（高水位、耗尽、被拒），**不动位图**。
    ///
    /// # 用途与边界
    ///
    /// 唯一用途是启动自检结束时调用：自检**故意**发一个超一块的请求来验证"拒绝"这条契约，
    /// 而那次拒绝属于"验证手段"，不是"运行期异常" —— 留着它会让终端 `mem` 永远报
    /// `请求被拒 1 次`，把真正的异常淹掉。
    ///
    /// 位图**不能**清：它是"哪些块在用"的唯一记录，清零等于把在用块凭空丢弃（后续
    /// `dealloc` 会变成"释放一个本来就空闲的块"）。
    pub fn reset_counters(&mut self) {
        self.high_water = 0;
        self.exhaustion_failures = 0;
        self.rejected_releases = 0;
    }

    /// 全部块都空闲吗（诊断用：应当只在"没有长期泄漏"的设备上看到 `true`）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.used.iter().all(|word| *word == 0)
    }
}

#[cfg(test)]
mod tests;
