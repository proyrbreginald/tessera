//! 原始可擦写介质：**唯一**无法在主机上测试的那一层。
//!
//! 这个 trait 的全部意义是**把不可测的部分压到最小**。上层
//! （[`crate::double_bank`]）负责"掉电原子性"的策略，那部分是纯逻辑、可在主机上
//! 用断电注入穷举；本 trait 只描述"读一个扇区、擦一个扇区、写一段字节"。
//!
//! 于是"存储会不会半写"这个问题被拆成两半：
//!
//! | 问题 | 验证手段 |
//! |------|----------|
//! | 策略对不对（双份、序号、校验和、选哪份） | 主机穷举 + 断电注入（快、可复现） |
//! | 介质行为对不对（擦写时序、掉电时字节状态） | HIL 真机断电 ≥1000 次（慢、不可替代） |
//!
//! 拆开之后，HIL 只需要回答后一个问题，而不是两个。

use crate::store::StoreError;

/// 擦除粒度与编程约束由实现者负责；调用方只按字节偏移操作。
pub trait RawMedium {
    /// 介质总容量（字节）。
    const CAPACITY: usize;
    /// 可擦除单元（扇区）大小。
    const SECTOR: usize;

    /// 读取一段字节，返回实际读到的长度。
    ///
    /// # Errors
    ///
    /// 偏移越界或驱动故障。
    fn read(&mut self, offset: usize, out: &mut [u8]) -> Result<usize, StoreError>;

    /// 擦除 `[offset, offset + len)`。
    ///
    /// # Errors
    ///
    /// 未对齐到 [`RawMedium::SECTOR`]、越界或驱动故障。
    fn erase(&mut self, offset: usize, len: usize) -> Result<(), StoreError>;

    /// 写入 `data`（介质须已擦除）。实现**不得**做读-改-写，否则原子性分析失效。
    ///
    /// # Errors
    ///
    /// 跨扇区、越界或驱动故障。
    fn program(&mut self, offset: usize, data: &[u8]) -> Result<(), StoreError>;
}
