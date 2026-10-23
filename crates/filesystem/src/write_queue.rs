//! 写请求的**投递队列**：把"谁想写"与"谁去写"分开。
//!
//! # 为什么需要它（一个会打穿硬实时的陷阱）
//!
//! `board_support::fs::with_fs` 是 `critical_section::with` —— **整个闭包在关中断下执行**。
//! 对"读"合适（XIP 拷贝，微秒级），但对"写"**绝对不行**：
//!
//! - 擦一个 64 KiB 块真机量级是**几十毫秒**，页编程一页也是百微秒量级；
//! - 而串行总线的字节间隔 **86.8 µs**、关中断预算 **80 µs**。
//!
//! 所以写入必须由**持有文件系统实例所有权的那个任务**在可被打断的上下文里推进。
//! 生产者（终端命令、日志搬运层、配置变更）只往这个队列里**投递请求**：
//! 投递本身是"拷贝几十字节"，可以在任意上下文（含关中断）里完成。
//!
//! # 满策略与日志环**故意相反**
//!
//! 日志环满了丢**最旧**（崩溃现场最需要最近的事）；**写请求不能这么干** ——
//! 静默丢掉一次写入，等于对着调用方说谎（它以为写好了）。所以这里：
//!
//! - 满了 ⇒ **拒绝**新请求，返回 `false`，并把 `rejected` 计数加一；
//! - **绝不覆盖**队列里已有的请求。
//!
//! 调用方拿到 `false` 之后可以重试、可以上报、可以退化成"放弃了并记一笔" ——
//! 但**必须由它决定**，而不是由队列偷偷决定。

/// 单个请求的数据上限。
///
/// 取 512 是为了让文本日志一次 append 能带 512 B，减少"每条记录只有
/// 192 B 数据 + 固定头"的写放大；同时 submit 的关中断拷贝保持在 80 µs 预算内
/// （1024 B 版本在真机上实测 irq-off 109 µs，超预算）。超长写入由调用方拆投。
pub const MAX_DATA: usize = 192;

/// 文件名字节上限（`config::files` 里的名字都很短）。
pub const MAX_NAME: usize = 24;

/// 请求类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 覆盖写入（不存在则创建）。
    Write,
    /// 追加。
    Append,
    /// 删除。
    Remove,
}

impl Kind {
    /// 名字（终端/日志用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Append => "append",
            Self::Remove => "remove",
        }
    }
}

/// 一次写请求。
///
/// `Debug` 是手写的（不 derive）：`[u8; 192]` 的默认 Debug 会把整块数据打出来，
/// 而日志/断言里我们只关心类型、名字与长度。
#[derive(Clone, Copy)]
pub struct WriteRequest {
    kind: Kind,
    name: [u8; MAX_NAME],
    name_len: u8,
    data: [u8; MAX_DATA],
    data_len: u16,
}

impl core::fmt::Debug for WriteRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WriteRequest")
            .field("kind", &self.kind)
            .field("name", &self.name())
            .field("data_len", &self.data().len())
            .finish()
    }
}

impl WriteRequest {
    /// 构造；名字或数据超长返回 `None`（**不截断** —— 截断的写入是数据损坏）。
    #[must_use]
    pub fn new(kind: Kind, name: &str, data: &[u8]) -> Option<Self> {
        if name.len() > MAX_NAME || data.len() > MAX_DATA {
            return None;
        }
        let mut request = Self {
            kind,
            name: [0; MAX_NAME],
            name_len: u8::try_from(name.len()).ok()?,
            data: [0; MAX_DATA],
            data_len: u16::try_from(data.len()).ok()?,
        };
        request
            .name
            .get_mut(..name.len())?
            .copy_from_slice(name.as_bytes());
        request.data.get_mut(..data.len())?.copy_from_slice(data);
        Some(request)
    }

    /// 类型。
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// 文件名。
    #[must_use]
    pub fn name(&self) -> &str {
        // 构造时保证是合法 UTF-8 且长度正确；取不到就返回空串（不 panic）。
        self.name
            .get(..usize::from(self.name_len))
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    /// 数据。
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data.get(..usize::from(self.data_len)).unwrap_or(&[])
    }
}

/// 有界请求队列（单生产者或临界区保护下多生产者 + **单消费者**＝属主任务）。
#[derive(Clone, Copy, Debug)]
pub struct Queue<const N: usize> {
    slots: [Option<WriteRequest>; N],
    head: usize,
    len: usize,
    submitted: u64,
    executed: u64,
    failed: u64,
    rejected: u64,
    too_long: u64,
}

impl<const N: usize> Default for Queue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Queue<N> {
    /// 空队列。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [None; N],
            head: 0,
            len: 0,
            submitted: 0,
            executed: 0,
            failed: 0,
            rejected: 0,
            too_long: 0,
        }
    }

    /// 待处理条数。
    #[must_use]
    pub const fn pending(&self) -> usize {
        self.len
    }

    /// 是否还有空间。
    #[must_use]
    pub const fn has_room(&self) -> bool {
        self.len < N
    }

    /// 累计投递成功条数。
    #[must_use]
    pub const fn submitted(&self) -> u64 {
        self.submitted
    }

    /// 累计执行成功条数。
    #[must_use]
    pub const fn executed(&self) -> u64 {
        self.executed
    }

    /// 累计执行失败条数。
    #[must_use]
    pub const fn failed(&self) -> u64 {
        self.failed
    }

    /// 因队列满**被拒绝**的条数（调用方必须能看到这个数字）。
    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    /// 因超长在构造阶段就被拒的条数。
    #[must_use]
    pub const fn too_long(&self) -> u64 {
        self.too_long
    }

    /// 投递一条；队列满返回 `false`（**不覆盖已有请求**，见模块文档）。
    pub fn submit(&mut self, request: WriteRequest) -> bool {
        if self.len >= N {
            self.rejected = self.rejected.saturating_add(1);
            return false;
        }
        let tail = (self.head + self.len) % N;
        if let Some(slot) = self.slots.get_mut(tail) {
            *slot = Some(request);
        }
        self.len += 1;
        self.submitted = self.submitted.saturating_add(1);
        true
    }

    /// 投递一条"按名字与数据构造"的请求；构造失败（超长）也计入统计并返回 `false`。
    pub fn submit_parts(&mut self, kind: Kind, name: &str, data: &[u8]) -> bool {
        match WriteRequest::new(kind, name, data) {
            Some(request) => self.submit(request),
            None => {
                self.too_long = self.too_long.saturating_add(1);
                false
            }
        }
    }

    /// 属主侧取出一条（FIFO）。
    pub fn take(&mut self) -> Option<WriteRequest> {
        if self.len == 0 {
            return None;
        }
        let request = self.slots.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % N;
        self.len -= 1;
        request
    }

    /// 属主侧回报执行结果（成功/失败分别计数）。
    pub fn complete(&mut self, ok: bool) {
        if ok {
            self.executed = self.executed.saturating_add(1);
        } else {
            self.failed = self.failed.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 队列投递与取出() {
        let mut queue = Queue::<2>::new();
        assert!(queue.submit_parts(Kind::Write, "a", b"1"));
        assert_eq!(queue.pending(), 1);
        let request = queue.take().unwrap();
        assert_eq!(request.name(), "a");
        assert_eq!(request.data(), b"1");
        queue.complete(true);

        assert_eq!(queue.submitted(), 1);
        assert_eq!(queue.executed(), 1);
    }

    #[test]
    fn 队列满拒绝与超长计数() {
        let mut queue = Queue::<1>::new();
        assert!(queue.submit_parts(Kind::Write, "a", b"1"));
        assert!(!queue.submit_parts(Kind::Write, "b", b"2"));
        assert_eq!(queue.rejected(), 1);
        assert!(!queue.submit_parts(Kind::Write, "a", &[0u8; MAX_DATA + 1]));
        assert_eq!(queue.too_long(), 1);
    }

    #[test]
    fn 队列空与失败计数() {
        let mut queue = Queue::<2>::new();
        assert!(queue.take().is_none());
        assert!(queue.submit_parts(Kind::Remove, "a", b""));
        queue.take();
        queue.complete(false);

        assert_eq!(queue.failed(), 1);
        assert_eq!(queue.pending(), 0);
    }
}
