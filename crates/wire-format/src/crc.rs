//! CRC-16/MODBUS —— 实现已上移到 `primitives`。
//!
//! 原因是它同时被**协议层**（帧校验）与**存储层**（槽位校验）使用，
//! 放在任何一边都会让另一边产生反向依赖。这里保留再导出，
//! 使协议层的使用者仍然写 `wire_format::crc::crc16(...)`。

pub use primitives::crc::{INIT, crc16, crc16_update, write_crc};
