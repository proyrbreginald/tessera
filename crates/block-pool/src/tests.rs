//! `block-pool` 的主机测试。
//!
//! 重点覆盖**"上线后才会暴露"的那几类错**：重复释放、越界释放、耗尽后的统计、
//! 高水位是否真的记到过峰值。这些都是纯逻辑，没有理由留到真机上碰运气。

use crate::{BlockIndex, BlockPool, PoolError};

/// 取一个就归还一个：池始终回到全空闲。
#[test]
fn 取用与归还后回到全空闲() {
    let mut pool = BlockPool::<1>::new();
    assert!(pool.is_empty());
    assert_eq!(pool.free_count(), 32);

    for _ in 0..5 {
        let index = pool.take().expect("池未满，必须给得出块");
        assert!(pool.is_used(index));
        assert_eq!(pool.release(index), Ok(()));
        assert!(!pool.is_used(index));
    }
    assert!(pool.is_empty(), "取还配平后必须回到全空闲");
    assert_eq!(pool.free_count(), 32);
}

/// 块号从 0 开始、逐块递增：换算法（比如换成扫描字）时这条会立刻发现。
#[test]
fn 分配按块号递增() {
    let mut pool = BlockPool::<1>::new();
    let mut taken = [0usize; 3];
    for slot in &mut taken {
        *slot = pool.take().expect("未满").get();
    }
    assert_eq!(taken, [0, 1, 2]);
}

/// 耗尽：返回 `None`（**不是** panic），并且这次失败被记下来。
#[test]
fn 耗尽返回_none_并计数() {
    let mut pool = BlockPool::<1>::new();
    let mut taken = 0;
    while pool.take().is_some() {
        taken += 1;
    }
    assert_eq!(taken, 32, "恰好 32 块");
    assert_eq!(pool.free_count(), 0);
    assert_eq!(pool.exhaustion_failures(), 1, "失败的分配必须被计数");

    // 再要两次：计数累加，仍然不 panic。
    assert!(pool.take().is_none());
    assert!(pool.take().is_none());
    assert_eq!(pool.exhaustion_failures(), 3);
}

/// **重复释放**：不改位图、记一笔、返回明确原因。
///
/// 这是本 crate 存在的主要理由 —— `GlobalAlloc::dealloc` 的契约里"重复释放是 UB"，
/// 我们在池这一层把它变成可判定的错误，而不是静默的内存损坏。
#[test]
fn 重复释放被拒绝且不改动位图() {
    let mut pool = BlockPool::<1>::new();
    let index = pool.take().expect("未满");
    assert_eq!(pool.release(index), Ok(()));
    assert_eq!(pool.release(index), Err(PoolError::AlreadyFree));
    assert_eq!(pool.rejected_releases(), 1);
    assert!(pool.is_empty(), "被拒绝的释放不得动位图");
    assert_eq!(pool.free_count(), 32);
}

/// 越界释放：拒绝并计数（`is_used` 对越界保守返回 `true` ⇒ 永不会被分配出去）。
#[test]
fn 越界释放被拒绝() {
    let mut pool = BlockPool::<1>::new();
    let beyond = BlockIndex::new(32).expect("32 在 u16 范围内");
    assert_eq!(pool.release(beyond), Err(PoolError::OutOfRange));
    assert_eq!(pool.rejected_releases(), 1);
    assert!(pool.is_used(beyond), "越界块必须被视为不可分配");
}

/// 高水位：记的是**峰值**，取还之后不回落。
#[test]
fn 高水位只增不减() {
    let mut pool = BlockPool::<1>::new();
    let first = pool.take().expect("未满");
    let second = pool.take().expect("未满");
    assert_eq!(pool.high_water(), 2);
    assert_eq!(pool.release(first), Ok(()));
    assert_eq!(pool.high_water(), 2, "归还后高水位不得回落");
    let _ = second;
}

/// 跨字边界：`WORDS=2`（64 块）时第 32、33 块必须落在第二个字里。
#[test]
fn 跨字边界() {
    let mut pool = BlockPool::<2>::new();
    assert_eq!(BlockPool::<2>::BLOCKS, 64);
    for _ in 0..32 {
        let _ = pool.take().expect("未满");
    }
    assert_eq!(pool.free_count(), 32);
    let crossing = pool.take().expect("第二个字还有空位");
    assert_eq!(crossing.get(), 32, "第 33 块应当是 32 号");
    assert!(pool.is_used(crossing));
}

/// 块号 newtype 不截断：超过 `u16::MAX` 返回 `None`。
#[test]
fn 块号构造不截断() {
    assert!(BlockIndex::new(0).is_some());
    assert_eq!(BlockIndex::new(7).map(BlockIndex::get), Some(7));
    assert!(BlockIndex::new(u16::MAX as usize).is_some());
    assert!(
        BlockIndex::new(u16::MAX as usize + 1).is_none(),
        "必须拒绝而不是截断"
    );
}

/// 错误与块号都要能打印（终端上要看得见）。
#[test]
fn 显示实现可用() {
    let text = alloc_format(&BlockIndex::new(3).expect("合法块号"));
    assert_eq!(text, "#3");
}

/// 测试里用 `format!`：本 crate 是纯逻辑、跑在主机上（有 std），不受固件零分配约束。
fn alloc_format(value: &dyn core::fmt::Display) -> String {
    std::format!("{value}")
}

/// `reset_counters` 只清计数，**不动位图** —— 位图是在用块的唯一记录，
/// 清它等于把在用块凭空丢弃（后续 `release` 会变成"释放一个本来就空闲的块"）。
#[test]
fn 清零计数不动位图() {
    let mut pool = BlockPool::<1>::new();
    let held = pool.take().expect("未满");
    let _ = pool.take().expect("未满");
    assert_eq!(pool.high_water(), 2);
    // 制造两类计数。
    let beyond = BlockIndex::new(999).expect("在 u16 内");
    assert_eq!(pool.release(beyond), Err(PoolError::OutOfRange));
    pool.reset_counters();

    assert_eq!(pool.high_water(), 0, "计数要清零");
    assert_eq!(pool.rejected_releases(), 0, "计数要清零");
    // 位图不动：在读的块还在读，归还仍然正常。
    assert!(pool.is_used(held));
    assert_eq!(pool.used_count(), 2, "位图不得被清");
    assert_eq!(pool.release(held), Ok(()));
    assert_eq!(pool.used_count(), 1);
}
