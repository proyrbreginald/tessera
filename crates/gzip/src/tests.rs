//! `gzip` 的主机测试。
//!
//! # 两个层次，缺一不可
//!
//! 1. **与真正的 gzip 互操作**：下面这些向量是用**系统的 `gzip`**（Python 的
//!    `gzip` 模块，即 zlib）现压出来的，逐字节嵌进来。只测"自己压、自己解"
//!    是不够的 —— 那只能证明**两个都错也自洽**。与 zlib 互操作才证明格式对。
//! 2. **损坏输入不许 panic**：日志回读发生在故障之后，介质上的字节可能任意。
//!
//! # 向量怎么来的（可复现）
//!
//! ```python
//! import gzip, io
//! buf = io.BytesIO()
//! with gzip.GzipFile(filename=name or '', mode='wb', fileobj=buf,
//!                    compresslevel=9, mtime=0) as f:
//!     f.write(data)
//! ```
//!
//! ⚠️ `mtime=0` 是为了确定性；`filename` 非空时会写出 **FNAME** 可选字段
//! （见 `WITH_NAME` 那条）—— 解码器必须能跳过它，否则"PC 上压的日志传回设备"就解不开。

// 本 crate 是 `no_std`；测试跑在主机上，因此显式把 `std`/`alloc` 拉进来。
// （不用 `#[cfg(test)] extern crate std;`：测试模块本身就在 `cfg(test)` 下。）
extern crate alloc;
extern crate std;

use alloc::{format, vec, vec::Vec};

use crate::{GzipError, HEADER_LEN, TRAILER_LEN, compress, decompress, new_compressor};
use miniz_oxide::inflate::core::DecompressorOxide;

/// 解压一个向量并断言结果等于 `expected`。
fn 应解出(blob: &[u8], expected: &[u8]) {
    let mut state = DecompressorOxide::new();
    // 输出缓冲区给足：expected 的长度 + 一些余量（刻意**恰好**测另一条路径）。
    let mut out = vec![0u8; expected.len()];
    let outcome = decompress(blob, &mut out, &mut state).expect("真 gzip 产生的流必须解得开");
    assert_eq!(outcome.produced, expected.len(), "解出的长度不对");
    assert_eq!(outcome.consumed, blob.len(), "应当正好吃完整条流");
    assert_eq!(&out[..outcome.produced], expected);
}

/// `b''`（0 字节 → 20 字节 gzip）
const EMPTY: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `b'abc'`（3 字节 → 23 字节 gzip）
const ABC: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 75, 76, 74, 6, 0, 194, 65, 36, 53, 3, 0, 0, 0,
];

/// `b'sensor=3 dir=1 rssi=-67'`（23 字节 → 43 字节 gzip）
const LINE: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 43, 78, 205, 43, 206, 47, 178, 53, 86, 72, 201, 44, 178, 53,
    84, 40, 42, 46, 206, 180, 213, 53, 51, 7, 0, 253, 219, 119, 95, 23, 0, 0, 0,
];

/// `b'heartbeat tick=0\nheartbeat t'...`（710 字节 → 133 字节 gzip）
const REP: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 101, 210, 49, 14, 131, 64, 20, 3, 209, 158, 83, 112, 132,
    216, 14, 33, 91, 228, 48, 27, 132, 4, 74, 135, 246, 254, 74, 205, 159, 118, 170, 39, 203, 199,
    222, 175, 241, 221, 251, 152, 199, 185, 253, 62, 143, 233, 184, 7, 213, 224, 26, 82, 195, 179,
    134, 165, 134, 87, 13, 107, 13, 239, 26, 26, 96, 164, 194, 42, 96, 5, 173, 192, 21, 188, 2, 88,
    16, 11, 100, 193, 108, 152, 205, 125, 97, 54, 204, 134, 217, 48, 27, 102, 195, 108, 152, 13,
    115, 96, 14, 204, 225, 41, 96, 14, 204, 129, 57, 48, 7, 230, 192, 156, 54, 253, 1, 206, 100,
    123, 9, 198, 2, 0, 0,
];

/// `"底座 通用 文本 压缩测试"`（33 字节 → 56 字节 gzip）
const UTF8: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 1, 33, 0, 222, 255, 229, 186, 149, 229, 186, 167, 32, 233,
    128, 154, 231, 148, 168, 32, 230, 150, 135, 230, 156, 172, 32, 229, 142, 139, 231, 188, 169,
    230, 181, 139, 232, 175, 149, 107, 246, 160, 120, 33, 0, 0, 0,
];

/// `b'named payload'`（13 字节 → 46 字节 gzip，带 FNAME='original.log'）
const WITH_NAME: &[u8] = &[
    31, 139, 8, 8, 0, 0, 0, 0, 2, 255, 111, 114, 105, 103, 105, 110, 97, 108, 46, 108, 111, 103, 0,
    203, 75, 204, 77, 77, 81, 40, 72, 172, 204, 201, 79, 76, 1, 0, 149, 163, 161, 195, 13, 0, 0, 0,
];

/// 每条向量：与系统 gzip 的产物逐字节对齐，解出来必须与原明文相同。
#[test]
fn 与系统_gzip_互操作() {
    应解出(EMPTY, b"");
    应解出(ABC, b"abc");
    应解出(LINE, b"sensor=3 dir=1 rssi=-67");
    应解出(UTF8, "底座 通用 文本 压缩测试".as_bytes());
    应解出(WITH_NAME, b"named payload");

    let rep: Vec<u8> = (0..40)
        .flat_map(|i| format!("heartbeat tick={i}\n").into_bytes())
        .collect();
    应解出(REP, &rep);
}

/// **多成员串联**（`cat a.gz b.gz > c.gz`）：只解第一个成员，
/// 并如实报出"消耗到哪"，把"要不要继续解"留给调用方。
#[test]
fn 多成员只解第一个且如实报告消耗量() {
    let mut joined = Vec::new();
    joined.extend_from_slice(ABC);
    joined.extend_from_slice(LINE);

    let mut state = DecompressorOxide::new();
    let mut out = [0u8; 64];
    let first = decompress(&joined, &mut out, &mut state).expect("第一个成员必须解得开");
    assert_eq!(&out[..first.produced], b"abc");
    assert_eq!(first.consumed, ABC.len(), "消耗量应当正好是第一个成员");

    // 从消耗处接着解第二个成员。
    let second = decompress(&joined[first.consumed..], &mut out, &mut state).expect("第二个成员");
    assert_eq!(&out[..second.produced], b"sensor=3 dir=1 rssi=-67");
}

/// 头/尾的边界都要被看见，而不是"解出来就算对"。
#[test]
fn 头部与尾部的错误各自可分辨() {
    let mut state = DecompressorOxide::new();
    let mut out = [0u8; 64];

    // 魔数不对。
    let mut bad = ABC.to_vec();
    bad[0] = 0x00;
    assert_eq!(
        decompress(&bad, &mut out, &mut state),
        Err(GzipError::NotGzip)
    );

    // 压缩方法不是 DEFLATE。
    let mut bad = ABC.to_vec();
    bad[2] = 0x09;
    assert_eq!(
        decompress(&bad, &mut out, &mut state),
        Err(GzipError::NotGzip)
    );

    // 连固定头都没读满。
    assert_eq!(
        decompress(&ABC[..HEADER_LEN - 1], &mut out, &mut state),
        Err(GzipError::Truncated)
    );

    // 缺尾（截掉最后 1 字节）。
    assert_eq!(
        decompress(&ABC[..ABC.len() - 1], &mut out, &mut state),
        Err(GzipError::Truncated)
    );
}

/// **CRC-32 不符必须被抓住** —— 这是"介质损坏"的最后一道闸门。
///
/// 改的是 DEFLATE 数据中的一个字节（头尾都不动），于是长度仍然对得上，
/// 只有 CRC 能发现内容被改过。
#[test]
fn 内容被篡改时_crc_会抓住() {
    let mut state = DecompressorOxide::new();
    // 输出缓冲要**够大**：缓冲不够会先报 `OutputTooSmall`，那条路径另有测试覆盖，
    // 这里要测的是"内容被改了"，别让缓冲尺寸把结论盖掉。
    let mut out = [0u8; 2048];
    // `REP` 的载荷较长，中间改一个字节不会破坏 Huffman 结构。
    let mut bad = REP.to_vec();
    let mid = HEADER_LEN + (REP.len() - HEADER_LEN - TRAILER_LEN) / 2;
    bad[mid] ^= 0x01;
    // 要么 DEFLATE 直接报坏，要么解出来但 CRC 对不上 —— 两者都算"抓住了"，
    // 唯独不允许"解出来还报告成功"。
    match decompress(&bad, &mut out, &mut state) {
        Err(GzipError::BadDeflate | GzipError::CrcMismatch | GzipError::SizeMismatch) => {}
        other => panic!("篡改必须被抓住，实际 {other:?}"),
    }
}

/// 任意前缀截断都**不许 panic**（掉电撕裂的就是完整流的前缀）。
#[test]
fn 任意前缀截断都不许_panic() {
    for cut in 0..REP.len() {
        let mut state = DecompressorOxide::new();
        let mut out = [0u8; 2048];
        // 截断只会报错；唯一不允许的是崩溃。
        let _ = decompress(&REP[..cut], &mut out, &mut state);
    }
}

/// 逐字节篡改都**不许 panic**。
#[test]
fn 任意单字节篡改都不许_panic() {
    for at in 0..REP.len() {
        for delta in [1u8, 0x80, 0xFF] {
            let mut bad = REP.to_vec();
            if let Some(slot) = bad.get_mut(at) {
                *slot = slot.wrapping_add(delta);
            }
            let mut state = DecompressorOxide::new();
            let mut out = [0u8; 2048];
            let _ = decompress(&bad, &mut out, &mut state);
        }
    }
}

/// 输出缓冲区不够时**报 `OutputTooSmall`**，而不是静默截断出一段"看起来能用"的数据。
#[test]
fn 输出缓冲不够会明确报错() {
    let mut state = DecompressorOxide::new();
    let mut out = [0u8; 2]; // `ABC` 要 3 字节
    assert_eq!(
        decompress(ABC, &mut out, &mut state),
        Err(GzipError::OutputTooSmall)
    );
}

// ---------------------------------------------------------------------------
// 压缩侧
// ---------------------------------------------------------------------------

/// 压得回去：**各种形状**的输入都要往返一致，且解出的长度与内容逐字节相同。
#[test]
fn 压缩往返一致() {
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"a".to_vec(),
        b"abc".to_vec(),
        b"sensor=3 dir=1 rssi=-67".to_vec(),
        (0..40)
            .flat_map(|i| format!("heartbeat tick={i}\n").into_bytes())
            .collect(),
        "底座 通用 文本 压缩测试".as_bytes().to_vec(),
        // 不可压缩：必须仍然往返（要么压不动、要么略微膨胀，但**不能出错**）。
        (0..4096u32)
            .map(|v| (v.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect(),
    ];
    let mut state = new_compressor();
    for input in cases {
        let mut packed = vec![0u8; crate::compress_bound(input.len())];
        let written = compress(&input, &mut packed, &mut state).expect("按上界开缓冲，必然放得下");

        // 头必须是我们写的那十个字节（确定性输出）。
        assert_eq!(
            packed.get(..HEADER_LEN),
            Some(&crate::HEADER[..]),
            "gzip 头不对"
        );

        let mut back = vec![0u8; input.len()];
        let mut inflate_state = DecompressorOxide::new();
        let outcome = decompress(&packed[..written], &mut back, &mut inflate_state)
            .expect("自己压的必须解得开");
        assert_eq!(outcome.produced, input.len(), "解出的长度不对");
        assert_eq!(back, input, "往返内容不一致（{} 字节）", input.len());
    }
}

/// 压缩是**确定性**的：同一输入两次必须得到同一字节序列。
///
/// 判据价值：掉电重启后重压同一段日志，产物应当可复现 —— 否则"内容相同"这种
/// 比对（去重、校验）就不成立。确定性来自恒为 0 的 `MTIME`。
#[test]
fn 压缩输出是确定性的() {
    let input = b"sensor=3 dir=1 rssi=-67\n".repeat(20);
    let mut a = vec![0u8; crate::compress_bound(input.len())];
    let mut b = vec![0u8; crate::compress_bound(input.len())];
    let mut state = new_compressor();
    let n = compress(&input, &mut a, &mut state).expect("放得下");
    let m = compress(&input, &mut b, &mut state).expect("放得下");
    assert_eq!(n, m);
    assert_eq!(
        a.get(..n),
        b.get(..m),
        "同一输入的两次压缩结果必须逐字节相同"
    );
}

/// 输出缓冲不够时**报 `OutputTooSmall`**，不截断、不假装成功。
#[test]
fn 压缩输出太小会报错() {
    let input = b"sensor=3 dir=1 rssi=-67".repeat(64);
    let mut tiny = [0u8; 12]; // 比头+尾还小
    let mut state = new_compressor();
    assert_eq!(
        compress(&input, &mut tiny, &mut state),
        Err(GzipError::OutputTooSmall)
    );
}

/// 尾部必须与 RFC 1952 一致：CRC-32 与 ISIZE 都要能被**独立算出来**对上。
#[test]
fn 尾部字段可独立验算() {
    use primitives::crc::{CRC32_INIT, crc32_finish, crc32_update};

    let input = b"hello gzip";
    let mut packed = vec![0u8; crate::compress_bound(input.len())];
    let mut state = new_compressor();
    let n = compress(input, &mut packed, &mut state).expect("放得下");

    let tail = packed.get(n - TRAILER_LEN..n).expect("尾部在");
    let crc = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
    let isize = u32::from_le_bytes([tail[4], tail[5], tail[6], tail[7]]);
    assert_eq!(crc, crc32_finish(crc32_update(CRC32_INIT, input)));
    assert_eq!(
        isize,
        u32::try_from(input.len()).expect("测试输入远小于 4 GiB")
    );
}

/// **与真正的 `gzip` 双向互操作**（`#[ignore]`：需要系统有 `gzip` 命令）。
///
/// 这是压缩侧**最强的判据**：把自己的产物交给系统的 `gzip -d` 去解，
/// 逐字节对上才算格式真的对。自压自解只能证明"两个都错也自洽"。
#[test]
#[ignore = "需要系统安装 gzip 命令；用 `--ignored` 跑"]
fn 与系统_gzip_双向互操作() {
    use std::io::Write as _;

    let input: Vec<u8> = (0..2000u32)
        .flat_map(|i| format!("heartbeat tick={i} sensor={}\n", i % 8).into_bytes())
        .collect();

    // ① 我们压 → 系统解。
    let mut packed = vec![0u8; crate::compress_bound(input.len())];
    let mut state = new_compressor();
    let n = compress(&input, &mut packed, &mut state).expect("放得下");

    let dir = std::env::temp_dir().join("gzip-interop");
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let gz = dir.join("ours.gz");
    let mut file = std::fs::File::create(&gz).expect("建文件");
    file.write_all(packed.get(..n).expect("写前 n 字节"))
        .expect("写");
    drop(file);

    let output = std::process::Command::new("gzip")
        .arg("-d")
        .arg("-f")
        .arg(&gz)
        .output()
        .expect("系统应当能执行 gzip");
    assert!(output.status.success(), "gzip -d 失败：{:?}", output);
    let decoded = std::fs::read(gz.with_extension("")).expect("读回");
    assert_eq!(decoded, input, "系统 gzip 解出来的内容与我们压进去的不一致");

    // ② 系统压 → 我们解（解压侧的互操作已在 `与系统_gzip_互操作` 覆盖，
    //    这里额外确认**同一段数据**走完整条链路）。
    let plain = dir.join("theirs.txt");
    std::fs::write(&plain, &input).expect("写明文");
    let status = std::process::Command::new("gzip")
        .arg("-9")
        .arg("-n")
        .arg("-f")
        .arg(&plain)
        .status()
        .expect("系统应当能执行 gzip");
    assert!(status.success(), "gzip 压缩失败");
    let theirs = std::fs::read(dir.join("theirs.txt.gz")).expect("读回 .gz");
    let mut back = vec![0u8; input.len()];
    let mut inflate_state = DecompressorOxide::new();
    let outcome = decompress(&theirs, &mut back, &mut inflate_state).expect("解得开");
    assert_eq!(outcome.produced, input.len());
    assert_eq!(back, input);
}

/// **gzip 压缩率实测**（`#[ignore]`：产出数字供报告引用，不是回归判据）。
///
/// 三种真实形状各测一遍，并给出"每 KiB 原始数据的压缩后字节数"：
/// ① 定长心跳日志文本（这是**新日志系统**要落的形状：原始文本）；
/// ② 本板真机控制台抓包（`tmp/boot_*.log`，存在才测）；
/// ③ 伪随机（最坏情形 —— 确认压不动时**不会显著膨胀**）。
#[test]
#[ignore = "主机侧标定：产出数字供报告引用"]
fn gzip_压缩率实测报告() {
    let mut state = new_compressor();

    let mut report = |label: &str, input: &[u8]| {
        let mut packed = vec![0u8; crate::compress_bound(input.len())];
        let n = compress(input, &mut packed, &mut state).expect("按上界开缓冲");
        // 整数口径（本 crate 拒绝浮点）：压缩率用千分数表示。
        let gain_permille = (input.len() * 1000).checked_div(n).unwrap_or(0);
        let saved_permille = if input.is_empty() {
            0
        } else {
            1000u64.saturating_sub((n as u64) * 1000 / input.len() as u64)
        };
        std::println!(
            "{label}: {} B → {} B，压缩率 {}.{:02}×，省 {}.{:01}%（{:.1} B/KiB 输入）",
            input.len(),
            n,
            gain_permille / 1000,
            (gain_permille % 1000) / 10,
            saved_permille / 10,
            saved_permille % 10,
            (n as f64) * 1024.0 / (input.len().max(1) as f64)
        );
    };

    // ① 心跳日志文本：新日志系统落盘的形状。
    let plain: Vec<u8> = (0..4096u32)
        .flat_map(|i| {
            format!(
                "12.{:03} INFO  heartbeat: tick={} bus=ok sensor={} rssi={}\n",
                i % 1000,
                i,
                i % 32,
                -67 + (i % 5) as i32
            )
            .into_bytes()
        })
        .collect();
    report("心跳日志文本 4096 行", &plain);

    // ② 真机抓包（可选）。
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crate 在 workspace 下");
    let capture = root.join("tmp/boot_1mbps.log");
    match std::fs::read(&capture) {
        Ok(bytes) if !bytes.is_empty() => report("真机抓包 boot_1mbps.log", &bytes),
        _ => std::println!("真机抓包：{} 不可读，跳过", capture.display()),
    }

    // ③ 不可压缩：确认**不会显著膨胀**。
    //
    // ⚠️ 这里第一版写的是 `(v * 1664525 + 1013904223) >> 24` —— 那是 **v 的周期函数**，
    // 不是伪随机：实测"压缩率 30.8×"，等于用一条几乎全同的序列去测最坏情形，
    // **结论完全反了**。改用 xorshift32（每步混入 3 次移位/异或），才是真的不可压。
    let mut prng: u32 = 0x1234_5678;
    let random: Vec<u8> = (0..64 * 1024)
        .map(|_| {
            prng ^= prng << 13;
            prng ^= prng >> 17;
            prng ^= prng << 5;
            (prng >> 24) as u8
        })
        .collect();
    report("伪随机 64 KiB（最坏情形）", &random);
}
