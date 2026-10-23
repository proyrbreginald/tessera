//! 脚本化总线替身。
//!
//! 每一次 `recv` 依序消费一个 [`Response`]，因此测试可以精确表达
//! "第 1 次超时、第 2 次帧错误、第 3 次成功"这类现场时序。

use heapless::Vec;
use ports::bus::{Bus, BusError};
use primitives::time::Timepoint;

/// 一次接收的预设结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// 正常应答这些字节。
    Bytes(Vec<u8, 64>),
    /// 超时（对端不响应）。
    Timeout,
    /// 帧错误（噪声、波特率不符）。
    Framing,
    /// 接收溢出（实时预算被破坏）。
    Overrun,
}

impl Response {
    /// 由切片构造应答。
    #[must_use]
    pub fn reply(bytes: &[u8]) -> Self {
        let mut buffer = Vec::new();
        for byte in bytes {
            // 超过 64 字节的脚本应答会被截断；测试里出现这种情况说明用例写错了。
            if buffer.push(*byte).is_err() {
                break;
            }
        }
        Self::Bytes(buffer)
    }
}

/// 脚本化半双工总线。
#[derive(Debug, Clone)]
pub struct MockBus<const N: usize> {
    script: Vec<Response, N>,
    cursor: usize,
    /// 已送出的帧（按顺序）。
    sent: Vec<Vec<u8, 64>, N>,
    /// `send` 是否失败（模拟驱动故障）。
    send_faults: usize,
}

impl<const N: usize> Default for MockBus<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> MockBus<N> {
    /// 空脚本。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            script: Vec::new(),
            cursor: 0,
            sent: Vec::new(),
            send_faults: 0,
        }
    }

    /// 追加一步应答。
    pub fn push(&mut self, response: Response) {
        let _ = self.script.push(response);
    }

    /// 让接下来 `times` 次 `send` 失败。
    pub fn fail_next_sends(&mut self, times: usize) {
        self.send_faults = times;
    }

    /// 已送出的第 `index` 帧。
    #[must_use]
    pub fn sent(&self, index: usize) -> Option<&[u8]> {
        self.sent.get(index).map(|frame| frame.as_slice())
    }

    /// 已送出的帧数。
    #[must_use]
    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }
}

impl<const N: usize> Bus for MockBus<N> {
    fn send(&mut self, frame: &[u8]) -> Result<(), BusError> {
        if self.send_faults > 0 {
            self.send_faults = self.send_faults.saturating_sub(1);
            return Err(BusError::Driver);
        }
        let mut stored = Vec::new();
        for byte in frame {
            if stored.push(*byte).is_err() {
                return Err(BusError::Driver);
            }
        }
        self.sent.push(stored).map_err(|_| BusError::Driver)
    }

    fn recv(&mut self, out: &mut [u8], _deadline: Timepoint) -> Result<usize, BusError> {
        let response = self.script.get(self.cursor).cloned();
        self.cursor = self.cursor.saturating_add(1);
        match response {
            None => Err(BusError::Timeout),
            Some(Response::Timeout) => Err(BusError::Timeout),
            Some(Response::Framing) => Err(BusError::Framing),
            Some(Response::Overrun) => Err(BusError::Overrun),
            Some(Response::Bytes(bytes)) => {
                if bytes.len() > out.len() {
                    return Err(BusError::Overrun);
                }
                for (slot, byte) in out.iter_mut().zip(bytes.iter()) {
                    *slot = *byte;
                }
                Ok(bytes.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ports::bus::{Idle, Link};
    use primitives::time::Duration;

    fn deadline() -> Timepoint {
        Timepoint::from_micros(1_000).wrapping_add(Duration::from_millis(10))
    }

    #[test]
    fn scripted_sequence_drives_retries() {
        let mut bus = MockBus::<8>::new();
        bus.push(Response::Timeout);
        bus.push(Response::Framing);
        bus.push(Response::reply(&[0x01, 0x02]));
        let mut link = Link::<_, Idle>::new(bus);
        let mut out = [0u8; 8];
        assert_eq!(link.transact(&[0xAA], &mut out, deadline(), 5), Ok(2));
        assert_eq!(link.bus().sent(0), Some(&[0xAAu8][..]));
        assert_eq!(link.bus().sent_count(), 3, "三次尝试各送出一帧");
    }

    #[test]
    fn exhausting_the_script_times_out() {
        let mut link = Link::<_, Idle>::new(MockBus::<4>::new());
        let mut out = [0u8; 8];
        assert_eq!(
            link.transact(&[1], &mut out, deadline(), 2),
            Err(BusError::Timeout)
        );
    }

    #[test]
    fn oversized_response_is_an_overrun_not_a_silent_truncation() {
        let mut bus = MockBus::<4>::new();
        bus.push(Response::reply(&[0u8; 16]));
        let mut link = Link::<_, Idle>::new(bus);
        let mut out = [0u8; 4];
        assert_eq!(
            link.transact(&[1], &mut out, deadline(), 1),
            Err(BusError::Overrun)
        );
    }

    #[test]
    fn send_faults_are_injectable() {
        let mut bus = MockBus::<4>::new();
        bus.fail_next_sends(1);
        bus.push(Response::reply(&[9]));
        let mut link = Link::<_, Idle>::new(bus);
        let mut out = [0u8; 4];
        assert_eq!(link.transact(&[1], &mut out, deadline(), 2), Ok(1));
    }
}
