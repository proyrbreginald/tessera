//! 确定性伪随机数与属性测试骨架。
//!
//! # 为什么不用 `proptest`
//!
//! 属性测试要有价值，前提是**可复现**：失败的用例必须能用同一个种子重放。
//! `proptest` 做得更全面，但它不在本机的离线依赖集里；更重要的是，本项目
//! 需要的"随机"只是**穷举的补充**——协议字段与状态机的时间轴都是小字符集，
//! 确定性 PRNG 足够，且没有任何第三方依赖进入"被验证代码"的构建闭包。
//!
//! 这个取舍本身是安全论证的一部分：验证工具越少，工具链鉴定面越小。
//!
//! # 用法
//!
//! ```ignore
//! let mut runner = PropRunner::new(0xE1E7_0A11);
//! runner.for_each(1_000, |rng| {
//!     let input = rng.next_u32();
//!     // ... 用 input 驱动被测逻辑，返回 Err 表示发现反例
//!     Ok(())
//! });
//! ```

/// xorshift64* 状态。
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// 以种子构造（种子为 0 时会被替换，避免退化为全零序列）。
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// 下一个 64 位值。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 下一个 32 位值。
    pub fn next_u32(&mut self) -> u32 {
        u32::try_from(self.next_u64() >> 32).unwrap_or(0)
    }

    /// `[0, bound)` 内的值。`bound == 0` 时返回 0。
    pub fn next_below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return 0;
        }
        self.next_u32() % bound
    }

    /// 随机填满一个缓冲区。
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            for (slot, byte) in chunk.iter_mut().zip(bytes.iter()) {
                *slot = *byte;
            }
        }
    }
}

/// 属性测试运行器：固定种子、记录反例种子。
#[derive(Debug, Clone)]
pub struct PropRunner {
    seed: u64,
    rng: Rng,
}

impl PropRunner {
    /// 以种子构造。
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            seed,
            rng: Rng::new(seed),
        }
    }

    /// 运行 `cases` 次。`check` 返回 `Err(reason)` 表示发现反例。
    ///
    /// 失败时返回的错误信息带**种子与用例序号**，便于单次复现。
    ///
    /// # Errors
    ///
    /// 把 `check` 的失败原因连同复现信息一起返回。
    pub fn for_each<F>(&mut self, cases: u32, mut check: F) -> Result<(), PropFailure>
    where
        F: FnMut(&mut Rng) -> Result<(), &'static str>,
    {
        for case in 0..cases {
            if let Err(reason) = check(&mut self.rng) {
                return Err(PropFailure {
                    reason,
                    case,
                    seed: self.seed,
                });
            }
        }
        Ok(())
    }
}

/// 属性测试失败信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropFailure {
    /// 失败原因。
    pub reason: &'static str,
    /// 第几个用例（0 起）。
    pub case: u32,
    /// 复现用的种子。
    pub seed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_deterministic_for_a_given_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let differ = (0..8).any(|_| a.next_u64() != b.next_u64());
        assert!(differ);
    }

    #[test]
    fn zero_seed_is_replaced() {
        let mut rng = Rng::new(0);
        let first = rng.next_u64();
        assert_ne!(first, 0);
    }

    #[test]
    fn next_below_respects_bound() {
        let mut rng = Rng::new(7);
        assert_eq!(rng.next_below(0), 0);
        for _ in 0..1_000 {
            assert!(rng.next_below(17) < 17);
        }
    }

    #[test]
    fn fill_covers_the_whole_buffer_including_the_tail() {
        let mut rng = Rng::new(9);
        let mut buf = [0u8; 13];
        rng.fill(&mut buf);
        assert!(buf.iter().any(|b| *b != 0));
        let mut again = [0u8; 13];
        Rng::new(9).fill(&mut again);
        assert_eq!(buf, again);
    }

    #[test]
    fn failure_reports_reproducible_context() {
        let mut runner = PropRunner::new(0xDEAD_BEEF);
        let failure = runner
            .for_each(100, |rng| {
                if rng.next_below(4) == 3 {
                    Err("命中")
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(failure.seed, 0xDEAD_BEEF);
        assert!(failure.case < 100);
        assert_eq!(failure.reason, "命中");
    }
}
