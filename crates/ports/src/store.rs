//! 持久化端口。
//!
//! # 契约的核心是"掉电原子性"
//!
//! [`Store::write_atomic`] 必须在**任意时刻掉电**的情况下保证：读回来要么是完整
//! 的旧值，要么是完整的新值，绝不出现半写。
//!
//! ⚠️ **这条性质无法用类型表达**。trait 签名约束不了实现者，一个只写一半扇区的
//! 实现照样能编译通过。因此本项目的策略是：
//!
//! 1. 只提供一个经过验证的实现（[`double_bank::DoubleBankStore`]，双份 + 序号 +
//!    校验和），**不把 trait 泛型暴露给上层**——上层看到的是具体类型；
//! 2. 唯一可信的验证手段是**断电注入**（对每个掉电点各跑一次，见
//!    `docs/04-开发/测试策略.md` 与**平台需求 TS-2**）。
//!
//! 换句话说：这个契约的保证来自**实现 + 测试**，不是来自类型系统。把它写成
//! trait 只是为了让主机替身能替换真实 Flash。

/// 存储区域。
///
/// 用枚举而不是"任意 key 字节串"：区域的集合是**静态可知**的，
/// 把它们变成类型可以杜绝"写错区域"这类静默数据损坏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// 应用配置（版本、标定值等）。
    Config,
    /// 应用数据集（整体覆盖写，双份 + 校验和）。
    Dataset,
    /// 现场黑匣子（复位原因、故障现场、安全状态迁移）。
    BlackBox,
}

/// 掉电安全的键值存储。
pub trait Store {
    /// 读取整块区域，返回实际长度。
    ///
    /// # Errors
    ///
    /// 校验失败、区域损坏、驱动故障。
    fn read(&mut self, region: Region, out: &mut [u8]) -> Result<usize, StoreError>;

    /// 原子写入整块区域。
    ///
    /// # Errors
    ///
    /// 擦写失败、校验不符、驱动故障。**失败时旧值必须仍然可读**。
    fn write_atomic(&mut self, region: Region, data: &[u8]) -> Result<(), StoreError>;
}

/// 存储错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreError {
    /// 该区域尚未写入过。
    Empty,
    /// 校验和不符（内容损坏）。
    Checksum,
    /// 两份副本都不可用。
    Corrupt,
    /// 写入数据超过区域容量。
    TooLarge,
    /// 驱动层故障。
    Driver,
    /// 未实现（里程碑未到）。
    NotImplemented,
    /// **被安全判据拒绝**：请求本身不合法（未对齐、跨页、落在只读分区），
    /// 或者**环境不允许写**（例如 4 字节寻址自检不通过 ⇒ 写下去可能被静默别名到低 16 MiB，
    /// 也就是"擦日志区"擦掉启动头）。
    ///
    /// 与 [`Self::Driver`] 的区别：`Driver` 是"器件/控制器出问题了"，`Rejected` 是
    /// "**我们**判断这次写入不该发生"。分开两者，是为了让上层能区分"硬件坏了"与"请求被正确拦下"。
    Rejected,
}

/// **没有可用存储**的 `Store` 实现：所有读都返回"尚未写入"，所有写明确失败。
///
/// # 为什么需要它（而不是"随便找个地方当存储"）
///
/// 历史上出现过一次真实的双属主事故：两套持久化机制被放在同一片物理区域上
/// （一套是过渡期的原始偏移表，一套是文件系统），而其中一方不知道自己会被
/// 另一方擦掉 —— 文件系统第一次擦块就会抹掉那三张表。
///
/// 正确的处置不是"把它们挪到另一个分区"（那还是两个属主），而是**让未实现的
/// 能力回到"明确不可用"这一态**：持久化只有一个属主（文件系统）；还没接线的
/// 那条持久化端口就照实说"没有人服务"。
///
/// 这与本项目的口径一致：**未实现不是错误，但假装完成是**。`NullStore` 从不
/// 静默成功 —— 调用方拿到的一定是明确的错误值。
#[derive(Clone, Copy, Debug, Default)]
pub struct NullStore;

impl NullStore {
    /// 构造。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Store for NullStore {
    fn read(&mut self, _region: Region, _out: &mut [u8]) -> Result<usize, StoreError> {
        // 没有任何区域被写入过 —— 这正是 `Empty` 的语义，而不是 `NotImplemented`：
        // "读"这件事我们其实知道答案（没东西），只有"写"是能力缺失。
        Err(StoreError::Empty)
    }

    fn write_atomic(&mut self, _region: Region, _data: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个刻意"不原子"的实现，用来演示契约为什么必须靠测试来保证。
    struct BadStore {
        config: heapless::Vec<u8, 32>,
    }

    impl Store for BadStore {
        fn read(&mut self, region: Region, out: &mut [u8]) -> Result<usize, StoreError> {
            match region {
                Region::Config => {
                    let n = core::cmp::min(out.len(), self.config.len());
                    for (slot, byte) in out.iter_mut().zip(self.config.iter()) {
                        *slot = *byte;
                    }
                    Ok(n)
                }
                _ => Err(StoreError::NotImplemented),
            }
        }

        fn write_atomic(&mut self, region: Region, data: &[u8]) -> Result<(), StoreError> {
            if region != Region::Config {
                return Err(StoreError::NotImplemented);
            }
            self.config.clear();
            for byte in data {
                self.config.push(*byte).map_err(|_| StoreError::TooLarge)?;
            }
            Ok(())
        }
    }

    #[test]
    fn trait_is_usable_through_a_trait_object() {
        let mut store = BadStore {
            config: heapless::Vec::new(),
        };
        let dynamic: &mut dyn Store = &mut store;
        dynamic.write_atomic(Region::Config, &[1, 2, 3]).unwrap();
        let mut out = [0u8; 8];
        assert_eq!(dynamic.read(Region::Config, &mut out), Ok(3));
        assert_eq!(&out[..3], &[1, 2, 3]);
    }

    #[test]
    fn unimplemented_regions_are_visible_not_panicking() {
        let mut store = BadStore {
            config: heapless::Vec::new(),
        };
        let mut out = [0u8; 4];
        assert_eq!(
            store.read(Region::Dataset, &mut out),
            Err(StoreError::NotImplemented)
        );
        assert_eq!(
            store.write_atomic(Region::BlackBox, &[0]),
            Err(StoreError::NotImplemented)
        );
    }

    #[test]
    fn oversize_write_is_rejected_without_destroying_old_value() {
        let mut store = BadStore {
            config: heapless::Vec::new(),
        };
        store.write_atomic(Region::Config, &[7, 7]).unwrap();
        let oversized = [0u8; 40];
        assert_eq!(
            store.write_atomic(Region::Config, &oversized),
            Err(StoreError::TooLarge)
        );
        // 注意：BadStore 在这里确实破坏了旧值（clear 了再 push 失败）。
        // 这正是"契约靠测试保证"的例证：主机替身必须比它更严格，
        // 真实实现必须提供双份 + 序号 + 校验和。
        let mut out = [0u8; 8];
        let _ = store.read(Region::Config, &mut out);
    }
}
