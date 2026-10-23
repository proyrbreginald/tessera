//! 可注入掉电的仿真存储介质。
//!
//! # 掉电模型
//!
//! 真实 NOR Flash 在编程过程中掉电，会留下**已写入的字节 + 未写入的字节**
//! （未写入部分保持擦除态 0xFF）。本仿真忠实模拟这一点：
//!
//! - [`MemMedium::power_cut_after`] 设定"还能再编程多少字节"；
//! - 预算耗尽时，`program` **写入能写的部分**然后返回错误。
//!
//! 这样 [`ports::double_bank::DoubleBankStore`] 的原子性策略就能在主机上
//! 被**任意时刻的掉电**反复验证 —— 这是"契约无法用类型表达"那一类性质最有力的
//! 替代验证手段。
//!
//! # 擦除的悲观假设
//!
//! `erase` **总是整扇区生效**，即使预算已耗尽。理由：掉电也可能发生在擦除刚开始
//! 或已完成的时刻，把擦除视为"一定成功"是对旧值最不利的假设。策略若在这种假设下
//! 仍然安全，那么在所有真实情况下都安全。

use ports::raw::RawMedium;
use ports::store::StoreError;

/// 内存介质（初始为擦除态 0xFF）。
#[derive(Debug, Clone)]
pub struct MemMedium<const N: usize> {
    bytes: [u8; N],
    program_budget: Option<usize>,
    erase_calls: usize,
    program_calls: usize,
}

impl<const N: usize> Default for MemMedium<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> MemMedium<N> {
    /// 全擦除态。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0xFF; N],
            program_budget: None,
            erase_calls: 0,
            program_calls: 0,
        }
    }

    /// 设定"还能再编程多少字节"，模拟即将掉电。
    pub fn power_cut_after(&mut self, bytes: usize) {
        self.program_budget = Some(bytes);
    }

    /// 取消掉电注入。
    pub fn restore_power(&mut self) {
        self.program_budget = None;
    }

    /// 擦除次数（可用于断言"写入确实走了轮换"）。
    #[must_use]
    pub const fn erase_calls(&self) -> usize {
        self.erase_calls
    }

    /// 编程调用次数。
    #[must_use]
    pub const fn program_calls(&self) -> usize {
        self.program_calls
    }

    /// 直接看原始字节（测试诊断用）。
    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.bytes
    }
}

impl<const N: usize> RawMedium for MemMedium<N> {
    const CAPACITY: usize = N;
    /// 仿真扇区取 256 字节：与真实 NOR 的 4 KiB 相比更小，
    /// 这样测试用的槽位可以小到便于穷举断电点。
    const SECTOR: usize = 256;

    fn read(&mut self, offset: usize, out: &mut [u8]) -> Result<usize, StoreError> {
        if offset >= N {
            return Err(StoreError::Driver);
        }
        let available = self.bytes.get(offset..).ok_or(StoreError::Driver)?;
        let n = core::cmp::min(out.len(), available.len());
        if let (Some(dst), Some(src)) = (out.get_mut(..n), available.get(..n)) {
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = *s;
            }
        }
        Ok(n)
    }

    fn erase(&mut self, offset: usize, len: usize) -> Result<(), StoreError> {
        let end = offset.checked_add(len).ok_or(StoreError::Driver)?;
        if end > N {
            return Err(StoreError::Driver);
        }
        if !offset.is_multiple_of(Self::SECTOR) || !len.is_multiple_of(Self::SECTOR) {
            return Err(StoreError::Driver);
        }
        let region = self.bytes.get_mut(offset..end).ok_or(StoreError::Driver)?;
        for byte in region.iter_mut() {
            *byte = 0xFF;
        }
        self.erase_calls = self.erase_calls.saturating_add(1);
        Ok(())
    }

    fn program(&mut self, offset: usize, data: &[u8]) -> Result<(), StoreError> {
        self.program_calls = self.program_calls.saturating_add(1);
        let allowed = self.program_budget.unwrap_or(usize::MAX);
        let n = core::cmp::min(allowed, data.len());
        let end = offset.checked_add(n).ok_or(StoreError::Driver)?;
        if end > N {
            return Err(StoreError::Driver);
        }
        if let (Some(dst), Some(src)) = (self.bytes.get_mut(offset..end), data.get(..n)) {
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = *s;
            }
        }
        self.program_budget = Some(allowed.saturating_sub(n));
        if n < data.len() {
            // 掉电发生在这次编程中途：已写入的部分留下，其余不写。
            return Err(StoreError::Driver);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erase_resets_to_all_ones() {
        let mut medium = MemMedium::<512>::new();
        medium.program(0, &[0x00; 16]).unwrap();
        medium.erase(0, 256).unwrap();
        let mut out = [0u8; 16];
        assert_eq!(medium.read(0, &mut out), Ok(16));
        assert_eq!(out, [0xFF; 16]);
    }

    #[test]
    fn erase_rejects_misalignment() {
        let mut medium = MemMedium::<512>::new();
        assert_eq!(medium.erase(1, 256), Err(StoreError::Driver));
        assert_eq!(medium.erase(0, 100), Err(StoreError::Driver));
    }

    #[test]
    fn power_cut_leaves_a_partial_write() {
        let mut medium = MemMedium::<512>::new();
        medium.power_cut_after(4);
        assert_eq!(
            medium.program(0, &[1, 2, 3, 4, 5, 6]),
            Err(StoreError::Driver)
        );
        let mut out = [0u8; 8];
        assert_eq!(medium.read(0, &mut out), Ok(8));
        assert_eq!(
            &out[..6],
            &[1, 2, 3, 4, 0xFF, 0xFF],
            "已写入的留下，其余保持擦除态"
        );
    }

    #[test]
    fn erase_is_pessimistically_assumed_to_succeed() {
        let mut medium = MemMedium::<512>::new();
        medium.program(0, &[0xAA; 8]).unwrap();
        medium.power_cut_after(0);
        // 预算为 0，但擦除仍被认为成功（对旧值最不利的假设）。
        assert_eq!(medium.erase(0, 256), Ok(()));
        let mut out = [0u8; 4];
        medium.read(0, &mut out).unwrap();
        assert_eq!(out, [0xFF; 4]);
    }
}
