//! 双份 + 序号 + 校验和的**掉电原子**存储策略。
//!
//! # 这是本项目最重要的一段可测逻辑（**平台需求 TS-2**）
//!
//! `Store::write_atomic` 的契约是"任意时刻掉电，读回来要么完整旧值、要么完整新值"。
//! 这条性质**无法用类型表达**，早期设计因此把它列为"契约靠实现者维持、只能靠
//! 真机断电测试验证"的薄弱点。
//!
//! 本模块把它拆开：
//!
//! - **策略**（这里）是纯逻辑：在 [`RawMedium`] 上做双份轮换、序号比较、CRC 校验。
//!   它可以在主机上被任意时刻的"断电"注入反复验证 —— 快、可复现、可穷举。
//! - **介质**（[`RawMedium`] 实现）才是不可测的部分，且它的接口已经小到只剩
//!   读/擦/写三个动作。
//!
//! # 记录格式（槽位内）
//!
//! ```text
//! 偏移  长度  内容
//! 0     4     魔数 0x424E_4B31（"BNK1"，小端）
//! 4     4     序号（小端，单调递增，回绕只是理论问题：4 字节足以）
//! 8     2     负载长度（小端）
//! 10    N     负载
//! 10+N  2     CRC-16/MODBUS（覆盖前 10+N 字节，小端）
//! ```
//!
//! # 写入顺序为什么是安全的
//!
//! 1. 选定"另一份"（序号较小的那份）作为目标；
//! 2. 擦除它 —— 此刻另一份完好，读路径仍能返回旧值；
//! 3. 写入新记录（序号 = 已见最大序号 + 1）；
//! 4. **回读校验**。失败即报错，另一份仍是旧值。
//!
//! 因此任意一步掉电后：要么旧份仍有效（返回旧值），要么新份完整（返回新值）。
//! 唯一需要介质保证的是"擦除后读到的是未通过 CRC 的内容"—— 对 NOR Flash 成立
//! （全 0xFF 的槽位魔数不匹配）。
//!
//! # 序号回绕
//!
//! 序号用 `u32` 且按**回绕安全**方式比较（差值小于半个空间即认为更大），
//! 与 `primitives::time::Timepoint` 同一套推理。4 字节 @ 每秒一次写入
//! 需要 136 年才回绕一次，但这属于"免费的正确性"。

use primitives::crc::{INIT, crc16_update};

use crate::raw::RawMedium;
use crate::store::{Region, Store, StoreError};

/// 槽位内的魔数：`"BNK1"` 小端。
pub const MAGIC: u32 = 0x424E_4B31;

/// 记录头长度：魔数 + 序号 + 长度。
pub const HEADER_LEN: usize = 10;

/// 记录尾长度：CRC。
pub const TRAILER_LEN: usize = 2;

/// 每份副本需要的额外开销。
pub const OVERHEAD: usize = HEADER_LEN + TRAILER_LEN;

/// 流式校验用的块大小。取 32 字节是为了让栈占用与缓存行对齐无关紧要。
const CHUNK: usize = 32;

/// 双份存储。
///
/// `SLOT` 是单份副本的槽位大小，由布局决定（见 `docs/02-架构/内存与Flash布局.md`）。
#[derive(Debug)]
pub struct DoubleBankStore<M, const SLOT: usize> {
    medium: M,
    /// 本存储区在介质上的起始偏移。
    ///
    /// 必须显式给出：外部 Flash 的前 1 MiB 属于启动头、boot data 与应用程序，
    /// 存储区从 0 开始会把它们覆盖掉 —— 那是一个"能编译、能烧录、设备变砖"的缺陷。
    base: usize,
}

/// 某个区域的两份副本在介质上的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlotPair {
    first: usize,
    second: usize,
}

impl<M: RawMedium, const SLOT: usize> DoubleBankStore<M, SLOT> {
    /// 编译期约束：槽位必须放得下记录头尾，且必须是扇区的整数倍。
    ///
    /// 这两条一旦不成立，运行期会以"擦除越界"或"写入跨扇区"的形式失败，
    /// 而这属于**可以在编译期排除**的失效，因此放在这里。
    const _LAYOUT_CHECK: () = {
        assert!(SLOT > OVERHEAD, "槽位放不下记录头尾");
        assert!(
            SLOT.is_multiple_of(M::SECTOR),
            "槽位必须是擦除扇区的整数倍，否则擦除会波及相邻记录"
        );
    };

    /// 在介质起始处构造（只为测试与"介质就是整个存储区"的场合使用）。
    pub const fn new(medium: M) -> Self {
        Self { medium, base: 0 }
    }

    /// 在介质的指定偏移处构造。
    pub const fn at(medium: M, base: usize) -> Self {
        Self { medium, base }
    }

    /// 本存储区的介质起始偏移。
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// 只读访问介质。
    pub const fn medium(&self) -> &M {
        &self.medium
    }

    /// 可变访问介质（供测试注入故障）。
    pub fn medium_mut(&mut self) -> &mut M {
        &mut self.medium
    }

    /// 计算某区域的两份槽位偏移。
    fn slots_of(&self, region: Region) -> SlotPair {
        let SlotPair { first, second } = Self::region_offsets(region);
        SlotPair {
            first: self.base.saturating_add(first),
            second: self.base.saturating_add(second),
        }
    }

    /// 区域在存储区内的相对偏移。
    fn region_offsets(region: Region) -> SlotPair {
        let index = match region {
            Region::Config => 0,
            Region::Dataset => 1,
            Region::BlackBox => 2,
        };
        let base = index * 2 * SLOT;
        SlotPair {
            first: base,
            second: base + SLOT,
        }
    }

    /// 读取一份槽位的 `(序号, 负载长度)`；损坏时返回 `None`。
    fn read_header(&mut self, base: usize) -> Option<(u32, usize)> {
        let mut header = [0u8; HEADER_LEN];
        let n = self.medium.read(base, &mut header).ok()?;
        if n < HEADER_LEN {
            return None;
        }
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if magic != MAGIC {
            return None;
        }
        let sequence = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let length = usize::from(u16::from_le_bytes([header[8], header[9]]));
        if length.saturating_add(OVERHEAD) > SLOT {
            return None;
        }
        Some((sequence, length))
    }

    /// 读取并**完整校验**一份槽位，返回 `(序号, 负载长度)`；任何一步不成立即 `None`。
    fn read_valid(&mut self, base: usize) -> Option<(u32, usize)> {
        let (sequence, length) = self.read_header(base)?;
        self.verify_and_copy(base, length, None)
            .map(|_| (sequence, length))
    }

    /// 校验一份槽位：流式计算 CRC，可选同时把负载拷出。返回负载长度。
    fn verify_and_copy(
        &mut self,
        base: usize,
        length: usize,
        mut out: Option<&mut [u8]>,
    ) -> Option<usize> {
        let mut chunk = [0u8; CHUNK];
        let mut crc = INIT;
        let total = length.saturating_add(HEADER_LEN);

        // 头部参与 CRC。
        let head_len = core::cmp::min(HEADER_LEN, total);
        let head = chunk.get_mut(..head_len)?;
        let n = self.medium.read(base, head).ok()?;
        if n < head_len {
            return None;
        }
        crc = crc16_update(crc, head);

        // 负载参与 CRC，并按需拷贝。
        let mut done = HEADER_LEN;
        let mut copied = 0usize;
        while done < total {
            let want = core::cmp::min(CHUNK, total.saturating_sub(done));
            let buf = chunk.get_mut(..want)?;
            let n = self.medium.read(base.saturating_add(done), buf).ok()?;
            if n < want {
                return None;
            }
            crc = crc16_update(crc, buf);
            if let Some(destination) = out.as_deref_mut() {
                let take = core::cmp::min(want, destination.len().saturating_sub(copied));
                if let (Some(dst), Some(src)) = (
                    destination.get_mut(copied..copied.saturating_add(take)),
                    buf.get(..take),
                ) {
                    for (d, s) in dst.iter_mut().zip(src.iter()) {
                        *d = *s;
                    }
                }
                copied = copied.saturating_add(take);
            }
            done = done.saturating_add(want);
        }

        // 尾部 CRC 参与比较。
        let mut trailer = [0u8; TRAILER_LEN];
        let n = self
            .medium
            .read(base.saturating_add(total), &mut trailer)
            .ok()?;
        if n < TRAILER_LEN {
            return None;
        }
        let stored = u16::from_le_bytes(trailer);
        if stored != crc {
            return None;
        }
        Some(length)
    }

    /// 序号比较（回绕安全）。
    fn is_newer(candidate: u32, reference: u32) -> bool {
        candidate.wrapping_sub(reference) < (u32::MAX >> 1)
    }
}

impl<M: RawMedium, const SLOT: usize> Store for DoubleBankStore<M, SLOT> {
    fn read(&mut self, region: Region, out: &mut [u8]) -> Result<usize, StoreError> {
        let slots = self.slots_of(region);

        // ⚠️ 关键：必须先**完整校验**两份，再在"有效的那些"里挑最新的。
        //
        // 一个诱人的错误写法是"先按序号挑出最新的，再校验它"——那样一旦最新那份
        // 正好是掉电半写的（序号已写入、负载尚未），读路径就会返回错误，
        // 把仍然完好的旧值也一起丢掉。这正是断电注入测试要抓的那类缺陷。
        let valid_first = self.read_valid(slots.first);
        let valid_second = self.read_valid(slots.second);

        let chosen = match (valid_first, valid_second) {
            (Some((seq_a, len_a)), Some((seq_b, len_b))) => {
                if Self::is_newer(seq_a, seq_b) {
                    Some((slots.first, len_a))
                } else {
                    Some((slots.second, len_b))
                }
            }
            (Some((_, len_a)), None) => Some((slots.first, len_a)),
            (None, Some((_, len_b))) => Some((slots.second, len_b)),
            (None, None) => {
                // 两份都不可用：区分"从未写过"与"写过但损坏"。
                // 这个区分对现场诊断有价值（前者是正常首启，后者是数据损坏告警）。
                let ever_written = self.read_header(slots.first).is_some()
                    || self.read_header(slots.second).is_some();
                return Err(if ever_written {
                    StoreError::Checksum
                } else {
                    StoreError::Empty
                });
            }
        };

        match chosen {
            Some((base, length)) => match self.verify_and_copy(base, length, Some(out)) {
                Some(len) => Ok(len),
                None => Err(StoreError::Checksum),
            },
            None => Err(StoreError::Empty),
        }
    }

    fn write_atomic(&mut self, region: Region, data: &[u8]) -> Result<(), StoreError> {
        if data.len().saturating_add(OVERHEAD) > SLOT {
            return Err(StoreError::TooLarge);
        }
        let slots = self.slots_of(region);

        // 选定目标：序号较小的那一份（缺失视为"最旧"）。
        let first_header = self.read_header(slots.first);
        let second_header = self.read_header(slots.second);
        let (target, next_sequence) = match (first_header, second_header) {
            (Some((seq_a, _)), Some((seq_b, _))) => {
                if Self::is_newer(seq_a, seq_b) {
                    (slots.second, seq_a.wrapping_add(1))
                } else {
                    (slots.first, seq_b.wrapping_add(1))
                }
            }
            (Some((seq_a, _)), None) => (slots.second, seq_a.wrapping_add(1)),
            (None, Some((seq_b, _))) => (slots.first, seq_b.wrapping_add(1)),
            (None, None) => (slots.first, 1),
        };

        // 1) 擦除目标槽位。此刻另一份仍然完好。
        self.medium.erase(target, SLOT)?;

        // 2) 写入头部 + 负载 + CRC（流式，不整块进内存）。
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&next_sequence.to_le_bytes());
        // 长度已经过 `data.len() + OVERHEAD <= SLOT` 检查，必然放得进 u16。
        let length = u16::try_from(data.len()).map_err(|_| StoreError::TooLarge)?;
        header[8..10].copy_from_slice(&length.to_le_bytes());
        self.medium.program(target, &header)?;

        let mut crc = crc16_update(INIT, &header);
        let mut offset = HEADER_LEN;
        for block in data.chunks(CHUNK) {
            self.medium.program(target.saturating_add(offset), block)?;
            crc = crc16_update(crc, block);
            offset = offset.saturating_add(block.len());
        }
        self.medium
            .program(target.saturating_add(offset), &crc.to_le_bytes())?;

        // 3) 回读校验：这一步把"编程没生效""地址算错"这类缺陷挡在返回之前。
        let verified = self
            .read_header(target)
            .and_then(|(seq, len)| {
                if seq != next_sequence {
                    return None;
                }
                self.verify_and_copy(target, len, None).map(|_| ())
            })
            .is_some();
        if verified {
            Ok(())
        } else {
            Err(StoreError::Driver)
        }
    }
}
