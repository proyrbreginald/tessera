//! 可注入掉电的块设备替身。
//!
//! # 掉电模型
//!
//! 真实的 NOR Flash 在编程过程中掉电，留下的是"已写入的字节 + 未写入的字节"
//! （未写入部分保持擦除态 `0xFF`），而且**只能把 1 写成 0**。本替身三条都忠实模拟：
//!
//! - [`MockDevice::power_cut_after`] 设定"还能再编程多少字节"；预算耗尽时
//!   `program` 写入能写的部分然后返回错误；
//! - `erase_block` **总是整块生效**，即使预算已耗尽 —— 掉电也可能正好发生在擦除
//!   刚开始或已完成的时刻，把擦除当作"一定成功"是对旧值最不利的假设。
//!   策略若在这种假设下仍然安全，那么在所有真实情况下都安全。
//! - 对非擦除区域编程按真实 NOR 的行为**按位与**，并记下
//!   [`MockDevice::nor_violations`]：文件系统本该永不覆盖已写字节，
//!   这个计数不为 0 就说明"跳过撕裂记录"那套推理被破坏了。

#![allow(dead_code)]

use crate::device::BlockDevice;
use crate::error::FsError;

/// 内存块设备。
#[derive(Debug, Clone)]
pub(crate) struct MockDevice<const N: usize> {
    bytes: [u8; N],
    block_size: u32,
    block_count: u32,
    /// 还能再编程多少字节（`None` = 不限）。
    budget: Option<usize>,
    program_bytes: usize,
    erase_calls: usize,
    nor_violations: u32,
    /// 第一次"对已写字节编程"的绝对偏移（诊断用）。
    first_violation: Option<u32>,
}

impl<const N: usize> MockDevice<N> {
    /// 全擦除态的新介质。
    pub(crate) fn new(block_size: u32, block_count: u32) -> Self {
        assert!(
            block_size.saturating_mul(block_count) <= u32::try_from(N).unwrap_or(0),
            "替身容量不够：{block_size} × {block_count} > {N}"
        );
        Self {
            bytes: [0xFF; N],
            block_size,
            block_count,
            budget: None,
            program_bytes: 0,
            erase_calls: 0,
            nor_violations: 0,
            first_violation: None,
        }
    }

    /// 用一份字节快照重建介质（掉电测试用它把介质回放到某个时刻）。
    pub(crate) fn from_bytes(bytes: [u8; N], block_size: u32, block_count: u32) -> Self {
        let mut device = Self::new(block_size, block_count);
        device.bytes = bytes;
        device
    }

    /// 设定"还能再编程多少字节"，模拟即将掉电。
    pub(crate) fn power_cut_after(&mut self, bytes: usize) {
        self.budget = Some(bytes);
    }

    /// 取消掉电注入。
    pub(crate) fn restore_power(&mut self) {
        self.budget = None;
    }

    /// 累计编程字节数（用来知道"这次操作一共会写多少字节"，从而穷举掉电点）。
    pub(crate) fn program_bytes(&self) -> usize {
        self.program_bytes
    }

    /// 擦除调用次数。
    pub(crate) fn erase_calls(&self) -> usize {
        self.erase_calls
    }

    /// 对非擦除区域编程的次数。文件系统正常工作时必须恒为 0。
    pub(crate) fn nor_violations(&self) -> u32 {
        self.nor_violations
    }

    /// 第一次违规的绝对偏移（诊断用）。
    pub(crate) fn first_violation_offset(&self) -> Option<u32> {
        self.first_violation
    }

    /// 原始字节（测试断言与损坏注入用）。
    pub(crate) fn raw(&self) -> &[u8; N] {
        &self.bytes
    }

    /// 可变原始字节（损坏注入用）。
    pub(crate) fn raw_mut(&mut self) -> &mut [u8; N] {
        &mut self.bytes
    }
}

impl<const N: usize> BlockDevice for MockDevice<N> {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn block_count(&self) -> u32 {
        self.block_count
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> Result<(), FsError> {
        let start = usize::try_from(offset).map_err(|_| FsError::Driver)?;
        let end = start.checked_add(out.len()).ok_or(FsError::Driver)?;
        let source = self.bytes.get(start..end).ok_or(FsError::Driver)?;
        out.copy_from_slice(source);
        Ok(())
    }

    fn erase_block(&mut self, index: u32) -> Result<(), FsError> {
        if index >= self.block_count {
            return Err(FsError::Driver);
        }
        let start = index.checked_mul(self.block_size).ok_or(FsError::Driver)?;
        let end = start.checked_add(self.block_size).ok_or(FsError::Driver)?;
        let start = usize::try_from(start).map_err(|_| FsError::Driver)?;
        let end = usize::try_from(end).map_err(|_| FsError::Driver)?;
        let block = self.bytes.get_mut(start..end).ok_or(FsError::Driver)?;
        for byte in block.iter_mut() {
            *byte = 0xFF;
        }
        self.erase_calls = self.erase_calls.saturating_add(1);
        Ok(())
    }

    fn program(&mut self, offset: u32, data: &[u8]) -> Result<(), FsError> {
        let start = usize::try_from(offset).map_err(|_| FsError::Driver)?;
        let end = start.checked_add(data.len()).ok_or(FsError::Driver)?;
        if end > N {
            return Err(FsError::Driver);
        }
        let allowed = self.budget.unwrap_or(usize::MAX);
        let count = allowed.min(data.len());
        let target = self
            .bytes
            .get_mut(start..start.saturating_add(count))
            .ok_or(FsError::Driver)?;
        let source = data.get(..count).ok_or(FsError::Driver)?;
        let mut violations = 0u32;
        for (index, (slot, byte)) in target.iter_mut().zip(source.iter()).enumerate() {
            if *slot & *byte != *byte {
                violations = violations.saturating_add(1);
                if self.first_violation.is_none() {
                    self.first_violation = u32::try_from(start.saturating_add(index)).ok();
                }
            }
            *slot &= *byte;
        }
        self.nor_violations = self.nor_violations.saturating_add(violations);
        self.program_bytes = self.program_bytes.saturating_add(count);
        self.budget = Some(allowed.saturating_sub(count));
        if count < data.len() {
            // 掉电发生在这次编程中途：已写入的部分留下，其余不写。
            return Err(FsError::Driver);
        }
        Ok(())
    }
}
