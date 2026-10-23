//! 半双工总线端口 + 类型状态机。
//!
//! # 契约
//!
//! | 方法 | 语义 |
//! |------|------|
//! | [`Bus::send`] | 把整帧写出去。返回即表示**已完全送出**（含方向切换回接收）。 |
//! | [`Bus::recv`] | 收一帧。`deadline` 之前必须返回，且**不得**越过它继续等待。 |
//!
//! `recv` 的 `deadline` 语义很重要：超时必须由**驱动层**实现，不能靠"上层用
//! 时钟轮询"。原因是最紧的实时约束在字节级（串行总线 115200 8N1 的字节间隔
//! 86.8 µs），只有驱动层才能在正确的时机放弃并交还控制权。
//!
//! # 错误分类
//!
//! [`BusError`] 的变体刻意对应**可区分的现场故障**，这样错误计数才有诊断价值：
//! 大量 `Timeout` 指向对端不响应；大量 `Framing` 指向电气/终端电阻；`Overrun`
//! 指向 ISR 预算被破坏。

use primitives::time::Timepoint;

/// 总线错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusError {
    /// 在 `deadline` 之前没有收到完整应答。
    Timeout,
    /// 收到字节但帧不合法（噪声、波特率不符、终端电阻缺失）。
    Framing,
    /// 接收溢出：本机来不及取走字节。
    ///
    /// 出现这个错误意味着**实时预算已被破坏**（见 `primitives::budget`），
    /// 应当触发降级而不是简单重试。
    Overrun,
    /// 驱动层自身故障（时钟未使能、引脚配置错、硬件不存在）。
    Driver,
    /// 该功能尚未实现（里程碑未到）。
    ///
    /// 刻意保留这个变体而不是 `todo!()`：未实现必须是**可观测的运行时事实**，
    /// 而不是一次 panic（**平台需求 MT-3**）。`cargo xtask check-gaps` 会清点
    /// 所有未实现点。
    NotImplemented,
}

/// 半双工（请求-应答）总线。
pub trait Bus {
    /// 送出一整帧。
    ///
    /// # Errors
    ///
    /// 驱动层故障返回 [`BusError::Driver`]；功能未实现返回 [`BusError::NotImplemented`]。
    fn send(&mut self, frame: &[u8]) -> Result<(), BusError>;

    /// 收一帧，最多等到 `deadline`。
    ///
    /// 返回实际收到的字节数。
    ///
    /// # Errors
    ///
    /// 超时 [`BusError::Timeout`]、帧错误 [`BusError::Framing`]、溢出 [`BusError::Overrun`]。
    fn recv(&mut self, out: &mut [u8], deadline: Timepoint) -> Result<usize, BusError>;
}

/// 发送结果：失败时把**链路与错误一起**交回，调用方必须显式处置。
pub type SendOutcome<B> = Result<Link<B, AwaitingResponse>, (Link<B, Idle>, BusError)>;

/// 接收结果：无论成败都回到 [`Idle`]，因此链路可继续使用。
pub type RecvOutcome<B> = Result<(Link<B, Idle>, usize), (Link<B, Idle>, BusError)>;

/// 状态标记：可以发送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle;
/// 状态标记：已发出请求，只等应答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingResponse;

/// 有序半双工链路。
///
/// 顺序由类型保证：
///
/// ```text
/// Link<B, Idle> ──send()──▶ Link<B, AwaitingResponse> ──recv()──▶ Link<B, Idle>
/// ```
///
/// 在 `Idle` 上调 `recv`、或在 `AwaitingResponse` 上调 `send`，都是编译错误。
/// 错误路径同样回到 `Idle`，因此不会出现"卡在中间状态、后续帧全乱"的情况。
#[derive(Debug)]
pub struct Link<B, S> {
    bus: B,
    _state: core::marker::PhantomData<S>,
}

impl<B: Bus> Link<B, Idle> {
    /// 由一条总线构造空闲链路。
    pub const fn new(bus: B) -> Self {
        Self {
            bus,
            _state: core::marker::PhantomData,
        }
    }

    /// 只读访问底层总线（诊断用）。
    pub const fn bus(&self) -> &B {
        &self.bus
    }

    /// 可变访问底层总线。
    ///
    /// 用途仅限**诊断与测试**（例如注入脚本化应答、读取驱动计数器）。
    /// 它不改变类型状态：`Idle`/`AwaitingResponse` 的顺序约束仍在。
    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.bus
    }

    /// 送出请求。失败时把链路与错误一起交回，调用方必须显式处置。
    ///
    /// # Errors
    ///
    /// 返回 `(链路, 错误)`：链路回到可发送状态，错误不被吞掉。
    pub fn send(self, frame: &[u8]) -> SendOutcome<B> {
        let mut bus = self.bus;
        match bus.send(frame) {
            Ok(()) => Ok(Link {
                bus,
                _state: core::marker::PhantomData,
            }),
            Err(error) => Err((
                Link {
                    bus,
                    _state: core::marker::PhantomData,
                },
                error,
            )),
        }
    }

    /// 有界重试的一次完整事务。
    ///
    /// 这是给"调用方不关心顺序、只要一次结果"的场景准备的高层入口：它在**函数内部**
    /// 严格遵循 send→recv 顺序，并把 `attempts` 限制在编译期可见的常量上。
    /// 需要精确控制顺序的场合请直接用 [`Link::send`]/[`Link::recv`]。
    ///
    /// # Errors
    ///
    /// 全部尝试失败后返回**最后一次**的错误（保留最接近真相的原因）。
    /// `attempts == 0` 视为 1 次，避免"零次尝试也算成功"这种静默。
    pub fn transact(
        &mut self,
        request: &[u8],
        response: &mut [u8],
        deadline: Timepoint,
        attempts: u8,
    ) -> Result<usize, BusError> {
        let tries = if attempts == 0 { 1 } else { attempts };
        let mut last = BusError::Timeout;
        for _ in 0..tries {
            if let Err(error) = self.bus.send(request) {
                last = error;
                continue;
            }
            match self.bus.recv(response, deadline) {
                Ok(len) => return Ok(len),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

impl<B: Bus> Link<B, AwaitingResponse> {
    /// 等待应答。无论成功失败都回到 `Idle`。
    ///
    /// # Errors
    ///
    /// 返回 `(链路, 错误)`，链路可用于下一次事务。
    pub fn recv(self, out: &mut [u8], deadline: Timepoint) -> RecvOutcome<B> {
        let mut bus = self.bus;
        let outcome = bus.recv(out, deadline);
        let link = Link {
            bus,
            _state: core::marker::PhantomData,
        };
        match outcome {
            Ok(len) => Ok((link, len)),
            Err(error) => Err((link, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitives::time::Duration;

    /// 记录调用的假总线。
    #[derive(Debug, Default)]
    struct Recorder {
        sent: heapless::Vec<u8, 16>,
        reply: heapless::Vec<u8, 8>,
        fail_send_times: u8,
    }

    impl Bus for Recorder {
        fn send(&mut self, frame: &[u8]) -> Result<(), BusError> {
            if self.fail_send_times > 0 {
                self.fail_send_times -= 1;
                return Err(BusError::Driver);
            }
            for byte in frame {
                self.sent.push(*byte).ok();
            }
            Ok(())
        }

        fn recv(&mut self, out: &mut [u8], _deadline: Timepoint) -> Result<usize, BusError> {
            if self.reply.is_empty() {
                return Err(BusError::Timeout);
            }
            let n = core::cmp::min(out.len(), self.reply.len());
            for (slot, byte) in out.iter_mut().zip(self.reply.iter()) {
                *slot = *byte;
            }
            Ok(n)
        }
    }

    fn deadline() -> Timepoint {
        Timepoint::from_micros(1_000).wrapping_add(Duration::from_millis(50))
    }

    #[test]
    fn happy_path_returns_to_idle_and_keeps_the_data() {
        let mut recorder = Recorder::default();
        recorder.reply.push(0xAA).unwrap();
        recorder.reply.push(0x55).unwrap();
        let link = Link::new(recorder);

        let awaiting = link.send(&[1, 2, 3]).unwrap();
        let mut response = [0u8; 8];
        let (link, len) = awaiting.recv(&mut response, deadline()).unwrap();
        assert_eq!(len, 2);
        assert_eq!(&response[..2], &[0xAA, 0x55]);
        assert_eq!(link.bus().sent.as_slice(), &[1, 2, 3]);
        // 回到 Idle：还能继续发。
        let _ = link.send(&[9]).unwrap();
    }

    #[test]
    fn send_failure_returns_a_reusable_idle_link() {
        let link = Link::new(Recorder {
            fail_send_times: 1,
            ..Recorder::default()
        });
        let (link, error) = link.send(&[1]).unwrap_err();
        assert_eq!(error, BusError::Driver);
        // 错误后链路仍然可用 —— 这正是"不卡在中间状态"的意思。
        let _ = link.send(&[2]).unwrap();
    }

    #[test]
    fn recv_timeout_returns_a_reusable_idle_link() {
        let recorder = Recorder::default();
        let link = Link::new(recorder);
        let awaiting = link.send(&[1]).unwrap();
        let mut out = [0u8; 4];
        let (link, error) = awaiting.recv(&mut out, deadline()).unwrap_err();
        assert_eq!(error, BusError::Timeout);
        let _ = link.send(&[2]).unwrap();
    }

    #[test]
    fn transact_retries_up_to_attempts_and_reports_last_error() {
        let mut recorder = Recorder {
            fail_send_times: 2,
            ..Recorder::default()
        };
        recorder.reply.push(0x11).unwrap();
        let mut link = Link::new(recorder);
        let mut out = [0u8; 4];
        // 前两次 send 失败，第三次成功。
        assert_eq!(link.transact(&[1], &mut out, deadline(), 3), Ok(1));

        let mut link = Link::new(Recorder::default());
        assert_eq!(
            link.transact(&[1], &mut out, deadline(), 2),
            Err(BusError::Timeout),
            "全部失败时返回最后一次错误"
        );
    }

    #[test]
    fn zero_attempts_still_tries_once() {
        let mut recorder = Recorder::default();
        recorder.reply.push(7).unwrap();
        let mut link = Link::new(recorder);
        let mut out = [0u8; 4];
        assert_eq!(link.transact(&[1], &mut out, deadline(), 0), Ok(1));
    }
}
