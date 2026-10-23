//! `gzip` —— gzip 容器（[RFC 1952]）+ DEFLATE（[RFC 1951]）。
//!
//! 纯逻辑、`no_std`、**零分配**。压缩与解压都走"调用方给缓冲区与状态"的入口，
//! 于是"这一步耗多少内存"是**静态可知**的（与 ADR-0003 的口径一致）。
//!
//! [RFC 1952]: https://www.rfc-editor.org/rfc/rfc1952
//! [RFC 1951]: https://www.rfc-editor.org/rfc/rfc1951
//!
//! # 压缩侧：靠**大块静态池**解决（不是通用堆）
//!
//! **解压是真正零分配的**（`inflate::core::decompress` 把输出写进调用方给的缓冲区），
//! 但 **`miniz_oxide` 的压缩器不是**：它的 `deflate` 模块整体被
//! `#[cfg(feature = "with-alloc")]` 门控，而 `CompressorOxide` 的内部状态
//! 是若干 `Box`：
//!
//! | 字段 | 大小 |
//! |---|---|
//! | `dict: Box<[u8; 33026]>` | 33 KB |
//! | `next: Box<[u16; 32768]>` | 64 KB |
//! | `hash: Box<[u16; 32768]>` | 64 KB |
//! | 代码缓冲（`LZ_CODE_BUF_SIZE`） | 64 KB |
//! | `HuffmanOxide`（3 张表） | ~4 KB |
//! | **合计** | **≈ 230 KB** |
//!
//! DTCM(64 KiB)/OCRAM(128 KiB)/ITCM(64 KiB) 都装不下 ⇒ 只能落 **SDRAM**。
//! 而本项目的分配池（`block-pool` + `board-support/src/alloc_pool.rs`）是**固定块**池
//! ——一次 `alloc` 只给一个块，**撑不住 230 KB 的单次分配**。
//!
//! ⇒ 出路**不是**通用堆，也**不是**自己写 DEFLATE，而是：
//! **把本工程的分配器扩成两个固定块静态池** ——
//! 小块 1 KiB × 2048（原有）+ **大块 64 KiB × 32**（2 MiB，SDRAM，新增）。
//! 压缩器的 5 次分配分别落进大块池的 5 个块：
//!
//! | 分配 | 需要 | 占块 |
//! |---|---|---|
//! | `dict` | 33 KB | 1 × 64 KiB |
//! | `next` | 64 KB | 1 × 64 KiB |
//! | `hash` | 64 KB | 1 × 64 KiB |
//! | 代码缓冲 | 64 KB | 1 × 64 KiB |
//! | `HuffmanOxide` | ~4 KB | 1 × 64 KiB |
//! | **合计** | **≈ 230 KB** | **5 × 64 KiB = 320 KiB** |
//!
//! 大块池共 32 块（2 MiB）⇒ 余量充足。三条论证**一条都没丢**：
//!
//! - **无碎片**：固定块、不合并、不切分、不跨池拼块；
//! - **静态可知**：两段区域都是常量地址 + 常量大小，编译期断言钉在 SDRAM 窗口内且不重叠；
//! - **失败是一个值**：`alloc` 返回空指针，**绝不 panic**。
//!
//! 代价是**块内浪费**（33 KB 的请求占掉 64 KiB）—— 在 32 MiB 的 SDRAM 上这个代价划算，
//! 换来的是论证的简单。
//!
//! # 为什么用 gzip 而不是自己写压缩
//!
//! 本项目的口径是"不重复实现已验证过的东西"，而 DEFLATE 是**几十年的公共标准**：
//! Huffman 树构建、位流读写、LZ77 窗口与匹配、动态/固定码表切换 —— 自己写一遍
//! 既要花大量时间，又要承担"只在特定数据上出错"的风险。
//! 因此这里接 [`miniz_oxide`]（`flate2` 的后端，纯 Rust、`no_std`）。
//!
//! ⚠️ 顺带记一条排查结论：产品族里**已有**一个 `GzFile` 实现，
//! 但它**不是纯 Rust** —— 它是 zlib 的 C 接口 FFI（`gzopen`/`gzread`/`gzwrite`/`gzclose`），
//! 与本项目"100% Rust、`no_std`"的口径不符，**不能照搬**。
//!
//! # 为什么是 gzip 容器而不是裸 DEFLATE
//!
//! 落盘的日志要能**在 PC 上直接看**：`gunzip` / `7-zip` / `zcat` 都能解标准 `.gz`。
//! 裸 DEFLATE 省 18 字节/流，但换来的代价是"任何消费方都得自带解压工具"。
//! 对"现场取回日志给人分析"这个主用途，容器本身就是价值。
//!
//! # 容器格式（逐字段）
//!
//! ```text
//! ┌──────────────── 10 字节头 ────────────────┬── DEFLATE 数据 ──┬──── 8 字节尾 ────┐
//! │ 1F 8B 08 FLG MTIME(4) XFL OS              │ RFC 1951         │ CRC32(4) ISIZE(4) │
//! └───────────────────────────────────────────┴──────────────────┴───────────────────┘
//! ```
//!
//! 本 crate **写**的头是固定的十个字节 `1F 8B 08 00 00 00 00 00 00 FF`：
//!
//! | 字段 | 值 | 理由 |
//! |---|---|---|
//! | `ID1 ID2` | `1F 8B` | 魔数 |
//! | `CM` | `08` | DEFLATE |
//! | `FLG` | `00` | 不用 FEXTRA/FNAME/FCOMMENT/FHCRC |
//! | `MTIME` | `0` | **确定性输出**：同一输入必然得到同一字节序列（测试的判据靠它） |
//! | `XFL` | `0` | 不声明压缩级别（不同实现的最快/最优提示，给了反而误导） |
//! | `OS` | `FF` | "未知"（本固件不是 Unix/Windows 中的任何一个） |
//!
//! 读的时候**支持** `FLG` 的四个可选字段（别的工具产生的 `.gz` 会带 `FNAME`），
//! 否则"PC 上压的日志传回设备"就解不开。见 [`decompress`]。

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::float_arithmetic
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation
    )
)]

use primitives::crc::{CRC32_INIT, crc32_finish, crc32_update};

#[cfg(test)]
mod tests;

#[cfg(feature = "compress")]
pub use miniz_oxide::deflate::core::CompressorOxide;
pub use miniz_oxide::inflate::core::DecompressorOxide;

/// gzip 固定头的字节数。
pub const HEADER_LEN: usize = 10;

/// gzip 固定尾的字节数（CRC-32 + ISIZE）。
pub const TRAILER_LEN: usize = 8;

/// 本 crate 写出的固定头（十个字节，逐字段依据见模块文档）。
const HEADER: [u8; HEADER_LEN] = [
    0x1F, 0x8B, // ID1 ID2：魔数
    0x08, // CM = DEFLATE
    0x00, // FLG = 无额外字段
    0x00, 0x00, 0x00, 0x00, // MTIME = 0（确定性输出）
    0x00, // XFL
    0xFF, // OS = 未知
];

/// 压缩/解压失败的原因。
///
/// **刻意分得比"成功/失败"细**：日志回读发生在故障之后，介质上的字节可能任意，
/// 而"是头不对、还是数据坏、还是长度对不上"直接决定下一步该查什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GzipError {
    /// 输出缓冲区放不下（**不截断、不假装成功**）。
    OutputTooSmall,
    /// 输入不足：连固定头或尾都没读完。
    Truncated,
    /// 魔数不是 `1F 8B`，或压缩方法不是 DEFLATE。
    NotGzip,
    /// 头部字段非法（例如 `FLG` 声明的可选字段越过了输入末尾）。
    BadHeader,
    /// DEFLATE 数据坏了（[`miniz_oxide`] 判定）。
    BadDeflate,
    /// 尾部的 CRC-32 与解出来的数据不符 —— **内容被改过或介质损坏**。
    CrcMismatch,
    /// 尾部的 ISIZE 与解出来的长度不符（超出 `2^32` 的部分无法校验，见文档）。
    SizeMismatch,
    /// **该方向尚未实现**。目前没有路径返回它，保留是为了让"未实现"有一个显式的值
    /// 可表达（本项目铁律 6：未实现不是错误，但假装完成是）。
    NotImplemented,
}

/// 一次解压的读数。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// 消耗掉的输入字节数（含头、DEFLATE 数据与尾）。
    ///
    /// 一个 `.gz` 文件允许由**多个成员**串联而成（`cat a.gz b.gz > c.gz`）。
    /// 本函数只解**第一个**成员，把"还剩多少"交给调用方决定 ——
    /// 于是"要不要继续解"是一个显式选择，而不是被库偷偷吃掉。
    pub consumed: usize,
    /// 解出的字节数。
    pub produced: usize,
}

/// 压缩后长度的**上界**（gzip 头 + DEFLATE 最坏膨胀 + 尾）。
///
/// DEFLATE 的最坏情形是"存进 Huffman 块"：每 65535 字节多出 5 字节块头，
/// 再加每个字节最多 9 位（非压缩块按 8 位对齐存放，实际不会有 9 位）。
/// 这里取**宽松但确定**的界：`n + n/100 + n/65535*5 + 64`。
/// 调用方按它开缓冲区就**不可能**遇到 [`GzipError::OutputTooSmall`]。
#[must_use]
pub const fn compress_bound(input_len: usize) -> usize {
    input_len + input_len / 100 + (input_len / 65_535) * 5 + 64
}

/// 按 gzip 需要的口径造一个压缩器状态。
///
/// **`DataFormat::Raw`**：gzip 的载荷是**裸 DEFLATE**，不带 zlib 的 2 字节包装
/// （zlib 的包装会多出 `78 9C` 与 Adler-32，那不是 gzip 的格式）。
/// 压缩级别取 `CompressionLevel::BestCompression` —— 落盘场景下 CPU 是富余的，
/// 而 Flash 的写入量与磨损是稀缺的。
///
/// ⚠️ 这个状态约 320 KiB（落在**大块静态池**里，见模块文档）。
/// 调用方应当**只造一次并长期持有**：反复造/扔会持续占用大块池的块。
#[must_use]
#[cfg(feature = "compress")]
pub fn new_compressor() -> CompressorOxide {
    CompressorOxide::with_format_and_level(
        miniz_oxide::DataFormat::Raw,
        miniz_oxide::deflate::CompressionLevel::BestCompression,
    )
}

/// 把 `input` 压缩成 gzip 流写进 `out`，返回写入的字节数。
///
/// `state` 由调用方提供并长期持有（见 [`new_compressor`]）—— 它约 320 KiB，
/// 落在大块静态池里，于是"压缩要多少内存"仍然是**静态可知**的一件事。
///
/// 输出是**确定性**的：同一 `input` 必然产生同一字节序列（`MTIME` 恒为 0）。
///
/// # Errors
///
/// 输出放不下（[`GzipError::OutputTooSmall`]）、或 DEFLATE 内部失败。
#[cfg(feature = "compress")]
pub fn compress(
    input: &[u8],
    out: &mut [u8],
    state: &mut CompressorOxide,
) -> Result<usize, GzipError> {
    if out.len() < HEADER_LEN + TRAILER_LEN {
        return Err(GzipError::OutputTooSmall);
    }
    let Some(head) = out.get_mut(..HEADER_LEN) else {
        return Err(GzipError::OutputTooSmall);
    };
    head.copy_from_slice(&HEADER);

    // ⚠️ **每次压缩都要把状态重置成"新流"** —— 与解压侧同一个坑（那边是
    // `DecompressorOxide::init()`）。`CompressorOxide` 里留着上一条流的 LZ 字典、
    // Huffman 表与位缓冲；不复位就压第二条流，第二次会报 `BadDeflate`。
    // **这个 bug 只有"同一个状态用两次"的测试才抓得到**（第一版每条用例都新建状态，
    // 于是完全看不到）。调用方长期持有一个状态是**预期用法**，所以复位必须在库里做。
    state.reset();

    // DEFLATE 数据从第 10 字节开始。回调式输出（`compress_to_output`）是
    // `miniz_oxide` 的入口：它不自己缓冲，而是把"该写哪些字节"回调出来 ——
    // 这一点与解压侧一样，是"输出缓冲区由调用方给"的口径。
    let mut at = HEADER_LEN;
    let (status, _consumed) = miniz_oxide::deflate::core::compress_to_output(
        state,
        input,
        miniz_oxide::deflate::core::TDEFLFlush::Finish,
        |chunk| {
            let Some(end) = at.checked_add(chunk.len()) else {
                return false;
            };
            let Some(slot) = out.get_mut(at..end) else {
                return false;
            };
            slot.copy_from_slice(chunk);
            at = end;
            true
        },
    );
    if status != miniz_oxide::deflate::core::TDEFLStatus::Done {
        // `PutBufFailed` = 输出放不下（回调返回了 false）；其余是内部错误。
        return Err(
            if status == miniz_oxide::deflate::core::TDEFLStatus::PutBufFailed {
                GzipError::OutputTooSmall
            } else {
                GzipError::BadDeflate
            },
        );
    }

    // 尾部：CRC-32（输入）与 ISIZE（输入长度对 2^32 取模），均小端。
    let crc = crc32_finish(crc32_update(CRC32_INIT, input));
    let produced_wide = u64::try_from(input.len()).unwrap_or(u64::MAX);
    let isize = u32::try_from(produced_wide & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    let tail_end = at
        .checked_add(TRAILER_LEN)
        .ok_or(GzipError::OutputTooSmall)?;
    let Some(tail) = out.get_mut(at..tail_end) else {
        return Err(GzipError::OutputTooSmall);
    };
    // ⚠️ `tail` 是 8 字节（CRC + ISIZE），要**分段**写：整段 `copy_from_slice`
    // 会因为长度不等而 panic（这里第一版就踩了，被往返测试当场抓住）。
    tail.get_mut(0..4)
        .ok_or(GzipError::OutputTooSmall)?
        .copy_from_slice(&crc.to_le_bytes());
    tail.get_mut(4..8)
        .ok_or(GzipError::OutputTooSmall)?
        .copy_from_slice(&isize.to_le_bytes());

    Ok(tail_end)
}

/// 解压 `input` 里的**第一个** gzip 成员到 `out`，返回消耗与产出的字节数。
///
/// # 为什么不是"解到输入耗尽为止"
///
/// `.gz` 允许**多成员串联**（`cat a.gz b.gz > c.gz`）。把"继续解下一个成员"
/// 做成调用方的显式选择，比库自己猜要诚实 —— 返回的 [`Outcome::consumed`]
/// 就是"下一个成员从哪开始"。
///
/// # 可选字段都要跳过
///
/// 别的工具产生的 `.gz` 常常带 `FNAME`（原文件名）。读的时候必须按 `FLG`
/// 跳过 `FEXTRA`/`FNAME`/`FCOMMENT`/`FHCRC`，否则"PC 上压的日志传回设备"就解不开。
/// 本 crate 自己**只写** `FLG = 0`，但**读**要能吃下别人写的。
///
/// # Errors
///
/// 见 [`GzipError`]。**CRC-32 与 ISIZE 都会校验** ——
/// "解出来了"不等于"解对了"，这一层是介质损坏的最后一道闸门。
pub fn decompress(
    input: &[u8],
    out: &mut [u8],
    state: &mut DecompressorOxide,
) -> Result<Outcome, GzipError> {
    let header = input.get(..HEADER_LEN).ok_or(GzipError::Truncated)?;
    if header.get(0..2) != Some(&HEADER[0..2]) || header.get(2) != Some(&HEADER[2]) {
        return Err(GzipError::NotGzip);
    }
    let Some(&flags) = header.get(3) else {
        return Err(GzipError::Truncated);
    };

    // 逐个跳过可选字段，得到 DEFLATE 数据的起点。
    let mut at = HEADER_LEN;
    if flags & 0x04 != 0 {
        // FEXTRA：2 字节长度 + 该长度的数据。
        let len_bytes = input.get(at..at + 2).ok_or(GzipError::BadHeader)?;
        let xlen = usize::from(u16::from_le_bytes([
            *len_bytes.first().ok_or(GzipError::BadHeader)?,
            *len_bytes.get(1).ok_or(GzipError::BadHeader)?,
        ]));
        at = at
            .checked_add(2)
            .and_then(|v| v.checked_add(xlen))
            .ok_or(GzipError::BadHeader)?;
    }
    // FNAME(0x08) / FCOMMENT(0x10)：以 NUL 结尾的字符串。
    for mask in [0x08u8, 0x10] {
        if flags & mask != 0 {
            loop {
                let Some(&byte) = input.get(at) else {
                    return Err(GzipError::BadHeader);
                };
                at += 1;
                if byte == 0 {
                    break;
                }
            }
        }
    }
    if flags & 0x02 != 0 {
        // FHCRC：2 字节头校验和（本实现不校验它，只跳过）。
        at = at.checked_add(2).ok_or(GzipError::BadHeader)?;
    }

    let deflate = input.get(at..).ok_or(GzipError::BadHeader)?;

    // ⚠️ **每次解压都要把状态重置成"从头开始"**。
    //
    // gzip 的每个成员都是一条**独立的 DEFLATE 流**，而 `DecompressorOxide` 是**有状态**的
    // （窗口、位缓冲、Huffman 表都留在里面）。不复位就接着解下一个成员，会拿上一条流的
    // 残留去解释新流 —— 真机上表现为 `CrcMismatch` 或 `BadDeflate`，而且**只在多成员时出现**。
    // 这个 bug 正是被 `多成员只解第一个且如实报告消耗量` 那条测试抓出来的。
    state.init();

    let (status, consumed_in, produced) = miniz_oxide::inflate::core::decompress(
        state,
        deflate,
        out,
        0,
        // ⚠️ **必须**声明"输出缓冲是线性（不环绕）的缓冲**"。
        //
        // `miniz_oxide::inflate::core::decompress` 的默认口径是把 `out` 当作**环形**
        // 缓冲（为流式解压设计）：它会把写指针绕回去。线性解压不声明这一位，
        // 解出来的东西会被写花 —— 实测症状是**除空流外全部 `BadDeflate`**
        // （空流因为一个字节都不用写而"碰巧通过"，所以这个坑很容易被漏掉）。
        //
        // 同时也**不设** `TINFL_FLAG_PARSE_ZLIB_HEADER`：gzip 里的载荷是**裸 DEFLATE**，
        // 没有 zlib 的 2 字节头；设了它会把 DEFLATE 的头两个字节吃掉。
        miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    // `HasMoreOutput` = 输出缓冲区装满了（调用方该换更大的）；
    // 其余非 `Done` 的状态都意味着数据坏了。
    if status != miniz_oxide::inflate::TINFLStatus::Done {
        return Err(
            if status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
                GzipError::OutputTooSmall
            } else {
                GzipError::BadDeflate
            },
        );
    }

    // 尾部：CRC-32 与 ISIZE。
    let tail_start = at.checked_add(consumed_in).ok_or(GzipError::Truncated)?;
    let tail = input
        .get(tail_start..tail_start + TRAILER_LEN)
        .ok_or(GzipError::Truncated)?;
    let expect_crc = u32::from_le_bytes([
        *tail.first().ok_or(GzipError::Truncated)?,
        *tail.get(1).ok_or(GzipError::Truncated)?,
        *tail.get(2).ok_or(GzipError::Truncated)?,
        *tail.get(3).ok_or(GzipError::Truncated)?,
    ]);
    let expect_size = u32::from_le_bytes([
        *tail.get(4).ok_or(GzipError::Truncated)?,
        *tail.get(5).ok_or(GzipError::Truncated)?,
        *tail.get(6).ok_or(GzipError::Truncated)?,
        *tail.get(7).ok_or(GzipError::Truncated)?,
    ]);

    let Some(plain) = out.get(..produced) else {
        return Err(GzipError::OutputTooSmall);
    };
    if crc32_finish(crc32_update(CRC32_INIT, plain)) != expect_crc {
        return Err(GzipError::CrcMismatch);
    }
    // ISIZE 是**原始长度对 2^32 取模**。用 `u64` 做掩码再收窄回 `u32`：
    // 直接写 `1 << 32` 在 32 位目标上是算术溢出（编译期拒绝），
    // 而 `produced as u32` 会被 `clippy::as_conversions` 拦下。
    let produced_wide = u64::try_from(produced).unwrap_or(u64::MAX);
    let size_expected = u32::try_from(produced_wide & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    if size_expected != expect_size {
        return Err(GzipError::SizeMismatch);
    }

    Ok(Outcome {
        consumed: tail_start + TRAILER_LEN,
        produced,
    })
}

// 解压状态的大小是**静态可知**的一部分：调用方据此决定把它放哪
// （`.bss` 还是 SDRAM）。断言住量级，避免换 miniz 版本时悄悄涨到装不下。
static_assertions::const_assert!(core::mem::size_of::<DecompressorOxide>() <= 16 * 1024);
