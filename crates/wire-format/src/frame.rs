//! 帧编解码的抽象。
//!
//! 之所以要 trait 而不是直接用具体实现：**帧结构是稳定的，设备语义是可变的**。
//! 换一条线路协议（或同一协议的不同设备档位）时，只替换 `FrameCodec` 的实现，
//! 上层代码不动。Modbus-RTU 是本仓库自带的实现（[`crate::ModbusRtu`]）。

use crate::error::ProtoError;

/// 零拷贝帧编解码。
///
/// # 生命周期约定
///
/// `decode` 返回的帧**借入**输入缓冲，因此：
///
/// - 解码不复制、不分配；
/// - 缓冲区被复用后旧帧无法再使用 —— 这是借用检查器保证的，不是纪律。
pub trait FrameCodec {
    /// 解码后的帧类型。
    type Frame<'a>;

    /// 解码一帧。
    ///
    /// # Errors
    ///
    /// 长度、CRC、地址、异常响应等错误。
    fn decode<'a>(&self, buffer: &'a [u8]) -> Result<Self::Frame<'a>, ProtoError>;

    /// 编码一帧到 `out`，返回写入字节数。
    ///
    /// # Errors
    ///
    /// 负载过长或输出缓冲不足。
    fn encode(&self, frame: &Self::Frame<'_>, out: &mut [u8]) -> Result<usize, ProtoError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个最小实现，用来验证 trait 的形状是可用且 object-safe 的（`Frame` 除外）。
    struct Echo;

    impl FrameCodec for Echo {
        type Frame<'a> = &'a [u8];

        fn decode<'a>(&self, buffer: &'a [u8]) -> Result<Self::Frame<'a>, ProtoError> {
            if buffer.is_empty() {
                return Err(ProtoError::TooShort { got: 0, min: 1 });
            }
            Ok(buffer)
        }

        fn encode(&self, frame: &Self::Frame<'_>, out: &mut [u8]) -> Result<usize, ProtoError> {
            if out.len() < frame.len() {
                return Err(ProtoError::BufferTooSmall {
                    need: frame.len(),
                    have: out.len(),
                });
            }
            if let (Some(dst), Some(src)) = (out.get_mut(..frame.len()), frame.get(..)) {
                dst.copy_from_slice(src);
            }
            Ok(frame.len())
        }
    }

    #[test]
    fn decode_borrows_the_input_buffer() {
        let codec = Echo;
        let buffer = [1u8, 2, 3];
        let frame = codec.decode(&buffer).unwrap();
        assert_eq!(frame, &buffer[..]);
        assert_eq!(
            codec.decode(&[]),
            Err(ProtoError::TooShort { got: 0, min: 1 })
        );
    }

    #[test]
    fn encode_reports_buffer_shortage() {
        let codec = Echo;
        let source = [1u8, 2, 3, 4];
        let mut out = [0u8; 2];
        assert_eq!(
            codec.encode(&&source[..], &mut out),
            Err(ProtoError::BufferTooSmall { need: 4, have: 2 })
        );
    }
}
