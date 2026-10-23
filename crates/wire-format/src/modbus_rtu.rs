//! Modbus-RTU 帧结构。
//!
//! ```text
//! ┌─────────┬──────────┬───────────────┬──────────────┐
//! │ 地址(1) │ 功能码(1)│ 数据(0..=252) │ CRC-16(2, LE)│
//! └─────────┴──────────┴───────────────┴──────────────┘
//! ```
//!
//! 结构本身是**标准化的**（Modbus over Serial Line V1.02），因此这里可以确定地实现；
//! 而"功能码与数据的含义"由应用层解释 —— 本层只做结构校验，不解码设备语义。
//!
//! # 校验与拒绝策略
//!
//! [`ModbusRtu::decode`] 在**返回帧之前**完成全部结构校验（长度、CRC、地址）。
//! 这样上层拿到的 [`Frame`] 一定是结构合法的 —— 把"校验有没有做"从调用方的
//! 记忆问题变成类型保证。

use primitives::crc::crc16;

use crate::error::ProtoError;
use crate::frame::FrameCodec;

/// 最大 ADU 长度（Modbus 规范：RS-485 上 256 字节）。
pub const MAX_ADU: usize = 256;

/// 最小 ADU 长度：地址 + 功能码 + CRC。
pub const MIN_ADU: usize = 4;

/// 最大 PDU 数据长度。
pub const MAX_PDU_DATA: usize = MAX_ADU - MIN_ADU;

/// 广播地址。
pub const BROADCAST: u8 = 0;

/// Modbus 功能码（不透明 newtype）。
///
/// 本层**不解释**功能码的含义：标准功能码与厂商自定义码都从这里过，
/// "哪个码对应什么操作"属于应用/设备规范。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionCode(u8);

impl FunctionCode {
    /// 从原始字节构造。
    #[must_use]
    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// 原始字节。
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// 已通过结构校验的一帧（借入接收缓冲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    address: u8,
    function: FunctionCode,
    data: &'a [u8],
}

impl<'a> Frame<'a> {
    /// 从机地址。
    #[must_use]
    pub const fn address(&self) -> u8 {
        self.address
    }

    /// 功能码（异常响应的原始功能码，不含 0x80 位）。
    #[must_use]
    pub const fn function(&self) -> FunctionCode {
        self.function
    }

    /// 数据负载。
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    // 刻意**没有** `is_exception()`/`exception_code()`：
    // 异常响应在 `decode` 里已经被转成 `ProtoError::ExceptionResponse`，
    // 因此拿到 `Frame` 就意味着"这是一帧正常的响应"。
    // 让类型表达这个不变量，好过让每个调用点都记得去检查 `0x80` 位。
}

/// Modbus-RTU 编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModbusRtu {
    /// 期望的从机地址（`None` 表示接受任意地址）。
    expected_address: Option<u8>,
}

impl ModbusRtu {
    /// 接受任意地址。
    #[must_use]
    pub const fn any_address() -> Self {
        Self {
            expected_address: None,
        }
    }

    /// 只接受指定从机地址（广播也接受）。
    #[must_use]
    pub const fn for_address(address: u8) -> Self {
        Self {
            expected_address: Some(address),
        }
    }
}

impl Default for ModbusRtu {
    fn default() -> Self {
        Self::any_address()
    }
}

impl ModbusRtu {
    /// 解码并**完整校验**一帧。
    ///
    /// # Errors
    ///
    /// - [`ProtoError::TooShort`] / [`ProtoError::TooLong`] 长度不合法；
    /// - [`ProtoError::Crc`] 校验失败；
    /// - [`ProtoError::AddressMismatch`] 地址不符；
    /// - [`ProtoError::ExceptionResponse`] 对端返回异常（这是**有效**报文，
    ///   因此单独一个变体，便于上层把它当成"对端明确拒绝"而不是"通信故障"）。
    pub fn decode<'a>(&self, buffer: &'a [u8]) -> Result<Frame<'a>, ProtoError> {
        if buffer.len() < MIN_ADU {
            return Err(ProtoError::TooShort {
                got: buffer.len(),
                min: MIN_ADU,
            });
        }
        if buffer.len() > MAX_ADU {
            return Err(ProtoError::TooLong {
                got: buffer.len(),
                max: MAX_ADU,
            });
        }

        let body_len = buffer.len().saturating_sub(2);
        let body = buffer.get(..body_len).ok_or(ProtoError::TooShort {
            got: buffer.len(),
            min: MIN_ADU,
        })?;
        let trailer = buffer.get(body_len..).ok_or(ProtoError::TooShort {
            got: buffer.len(),
            min: MIN_ADU,
        })?;
        let computed = crc16(body);
        let expected = u16::from_le_bytes([
            *trailer.first().ok_or(ProtoError::TooShort {
                got: body_len,
                min: MIN_ADU,
            })?,
            *trailer.get(1).ok_or(ProtoError::TooShort {
                got: body_len,
                min: MIN_ADU,
            })?,
        ]);
        if computed != expected {
            return Err(ProtoError::Crc { expected, computed });
        }

        let address = *body.first().ok_or(ProtoError::TooShort {
            got: 0,
            min: MIN_ADU,
        })?;
        if let Some(expected_address) = self.expected_address
            && address != expected_address
            && address != BROADCAST
        {
            return Err(ProtoError::AddressMismatch {
                expected: expected_address,
                got: address,
            });
        }
        let raw_function = *body.get(1).ok_or(ProtoError::TooShort {
            got: 1,
            min: MIN_ADU,
        })?;
        let data = body.get(2..).unwrap_or(&[]);

        // 异常响应也要能被上层识别成"有效报文"，因此单独返回它的语义。
        if raw_function & 0x80 != 0 {
            let code = data.first().copied().unwrap_or(0);
            return Err(ProtoError::ExceptionResponse {
                function: raw_function & 0x7F,
                code,
            });
        }

        Ok(Frame {
            address,
            function: FunctionCode::new(raw_function),
            data,
        })
    }

    /// 编码一帧，返回写入字节数。
    ///
    /// # Errors
    ///
    /// 负载过长 [`ProtoError::TooLong`] 或输出缓冲不足 [`ProtoError::BufferTooSmall`]。
    pub fn encode(
        &self,
        address: u8,
        function: FunctionCode,
        data: &[u8],
        out: &mut [u8],
    ) -> Result<usize, ProtoError> {
        if data.len() > MAX_PDU_DATA {
            return Err(ProtoError::TooLong {
                got: data.len(),
                max: MAX_PDU_DATA,
            });
        }
        let need = data.len().saturating_add(MIN_ADU);
        if out.len() < need {
            return Err(ProtoError::BufferTooSmall {
                need,
                have: out.len(),
            });
        }
        if let Some(slot) = out.get_mut(..need) {
            if let Some(address_slot) = slot.get_mut(0) {
                *address_slot = address;
            }
            if let Some(function_slot) = slot.get_mut(1) {
                *function_slot = function.get();
            }
            if let (Some(dst), Some(src)) = (
                slot.get_mut(2..2usize.saturating_add(data.len())),
                data.get(..),
            ) {
                dst.copy_from_slice(src);
            }
        }
        // 先把 CRC 算进局部变量，再一次性写入尾部：
        // 这样不会同时可变借用与不可变借用 `out`。
        let crc = crc16(out.get(..need.saturating_sub(2)).unwrap_or(&[]));
        let trailer = crc.to_le_bytes();
        if let Some(slot) = out.get_mut(need.saturating_sub(2)..need) {
            slot.copy_from_slice(&trailer);
        }
        Ok(need)
    }
}

impl FrameCodec for ModbusRtu {
    type Frame<'a> = Frame<'a>;

    fn decode<'a>(&self, buffer: &'a [u8]) -> Result<Self::Frame<'a>, ProtoError> {
        ModbusRtu::decode(self, buffer)
    }

    fn encode(&self, frame: &Self::Frame<'_>, out: &mut [u8]) -> Result<usize, ProtoError> {
        ModbusRtu::encode(self, frame.address(), frame.function(), frame.data(), out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参考帧：从机 0x11、功能码 0x04、数据 0x00 0x01 0x00 0x01。
    ///
    /// CRC 值 0x9A62，线上低字节在前 → 尾两字节 `62 9A`。
    /// 该值由独立实现核对（见 `primitives::crc` 的测试）。
    const REFERENCE: [u8; 8] = [0x11, 0x04, 0x00, 0x01, 0x00, 0x01, 0x62, 0x9A];

    #[test]
    fn decodes_a_known_frame() {
        let frame = ModbusRtu::for_address(0x11).decode(&REFERENCE).unwrap();
        assert_eq!(frame.address(), 0x11);
        assert_eq!(frame.function(), FunctionCode::new(0x04));
        assert_eq!(frame.data(), &[0x00, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let codec = ModbusRtu::for_address(0x01);
        let mut buffer = [0u8; MAX_ADU];
        let len = codec
            .encode(
                0x01,
                FunctionCode::new(0x10),
                &[0xAA, 0xBB, 0xCC],
                &mut buffer,
            )
            .unwrap();
        assert_eq!(len, 3 + MIN_ADU);
        let frame = codec.decode(&buffer[..len]).unwrap();
        assert_eq!(frame.address(), 0x01);
        assert_eq!(frame.function(), FunctionCode::new(0x10));
        assert_eq!(frame.data(), &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn crc_error_is_detected_for_every_single_bit_flip() {
        let codec = ModbusRtu::any_address();
        for index in 0..REFERENCE.len() {
            for bit in 0..8 {
                let mut mutated = REFERENCE;
                mutated[index] ^= 1 << bit;
                let outcome = codec.decode(&mutated);
                assert!(outcome.is_err(), "字节 {index} 第 {bit} 位翻转后仍被接受");
            }
        }
    }

    #[test]
    fn too_short_and_too_long_are_rejected() {
        let codec = ModbusRtu::any_address();
        assert_eq!(
            codec.decode(&[0x01, 0x03, 0x00]),
            Err(ProtoError::TooShort { got: 3, min: 4 })
        );
        let oversized = [0u8; MAX_ADU + 1];
        assert_eq!(
            codec.decode(&oversized),
            Err(ProtoError::TooLong {
                got: MAX_ADU + 1,
                max: MAX_ADU
            })
        );
    }

    #[test]
    fn address_mismatch_is_rejected_but_broadcast_is_accepted() {
        let codec = ModbusRtu::for_address(0x02);
        assert_eq!(
            codec.decode(&REFERENCE),
            Err(ProtoError::AddressMismatch {
                expected: 0x02,
                got: 0x11
            })
        );
        // 广播地址永远接受。
        let mut broadcast = REFERENCE;
        broadcast[0] = BROADCAST;
        let crc = crc16(&broadcast[..broadcast.len() - 2]);
        broadcast[6] = u8::try_from(crc & 0x00FF).unwrap();
        broadcast[7] = u8::try_from(crc >> 8).unwrap();
        assert!(codec.decode(&broadcast).is_ok());
    }

    #[test]
    fn exception_response_is_reported_distinctly() {
        // 构造异常响应：功能码 0x84、异常码 0x02。
        let codec = ModbusRtu::any_address();
        let mut buffer = [0u8; 16];
        let len = codec
            .encode(0x01, FunctionCode::new(0x84), &[0x02], &mut buffer)
            .unwrap();
        assert_eq!(
            codec.decode(&buffer[..len]),
            Err(ProtoError::ExceptionResponse {
                function: 0x04,
                code: 0x02
            })
        );
    }

    #[test]
    fn maximum_payload_is_accepted_and_one_more_is_not() {
        let codec = ModbusRtu::any_address();
        let payload = [0x5Au8; MAX_PDU_DATA];
        let mut buffer = [0u8; MAX_ADU];
        assert_eq!(
            codec.encode(0x01, FunctionCode::new(0x10), &payload, &mut buffer),
            Ok(MAX_ADU)
        );
        let too_big = [0u8; MAX_PDU_DATA + 1];
        assert_eq!(
            codec.encode(0x01, FunctionCode::new(0x10), &too_big, &mut buffer),
            Err(ProtoError::TooLong {
                got: MAX_PDU_DATA + 1,
                max: MAX_PDU_DATA
            })
        );
    }

    #[test]
    fn frame_codec_trait_path_agrees_with_inherent_methods() {
        let codec = ModbusRtu::for_address(0x11);
        let frame = <ModbusRtu as FrameCodec>::decode(&codec, &REFERENCE).unwrap();
        let mut out = [0u8; MAX_ADU];
        let len = <ModbusRtu as FrameCodec>::encode(&codec, &frame, &mut out).unwrap();
        assert_eq!(&out[..len], &REFERENCE);
    }
}
