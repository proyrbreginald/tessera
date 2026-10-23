//! 协议层错误。
//!
//! 每个变体都对应一类**可区分的现场故障**，这样错误计数才能直接指导排障：
//!
//! | 变体 | 现场含义 |
//! |------|----------|
//! | [`ProtoError::TooShort`] | 帧被截断（波特率/接线/干扰） |
//! | [`ProtoError::TooLong`] | 长度域不可信或缓冲太小 |
//! | [`ProtoError::Crc`] | 位翻转（电气干扰、接地、终端电阻） |
//! | [`ProtoError::AddressMismatch`] | 总线上有别的从机 / 地址配置错 |
//! | [`ProtoError::ExceptionResponse`] | 对端明确报错（这是**有效**报文） |
//! | [`ProtoError::BufferTooSmall`] | 我方缓冲不足（编码侧） |

/// 协议层错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtoError {
    /// 输入短于最小帧长。
    TooShort {
        /// 实际长度。
        got: usize,
        /// 期望的最小长度。
        min: usize,
    },
    /// 输入长于允许的最大帧长。
    TooLong {
        /// 实际长度。
        got: usize,
        /// 允许的最大长度。
        max: usize,
    },
    /// CRC 校验失败。
    Crc {
        /// 报文携带的 CRC。
        expected: u16,
        /// 本地计算的 CRC。
        computed: u16,
    },
    /// 从机地址不匹配。
    AddressMismatch {
        /// 期望地址。
        expected: u8,
        /// 实际地址。
        got: u8,
    },
    /// 对端返回异常响应（功能码最高位置 1）。
    ExceptionResponse {
        /// 原始功能码。
        function: u8,
        /// 异常码。
        code: u8,
    },
    /// 输出缓冲太小。
    BufferTooSmall {
        /// 需要多少字节。
        need: usize,
        /// 缓冲有多少字节。
        have: usize,
    },
}
