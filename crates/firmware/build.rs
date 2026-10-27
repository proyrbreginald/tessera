//! 构建期配置：内存布局、启动头、版本号与**启动横幅**注入，以及**跨文件一致性**检查。
//!
//! `imxrt-rt` 的构建期 API 按这里描述的内存映射生成链接脚本（`imxrt-link.x`），
//! 并在映像作为可启动 flash 映像构建时注入启动头（FCB 槽位 + IVT + boot data）。
//!
//! 本文件里每个数值都能追到 `docs/硬件与板级事实.md` 的证据链。
//! 两个"文本真源"在仓库根：`VERSION`（版本号）与 `banner.txt`（启动横幅）。

use std::env;
use std::fs;
use std::path::PathBuf;

/// 启动 Flash 容量（字节）：Winbond `W25Q256JVEQ`，256 Mbit = 32 MiB。
///
/// ⚠️ 必须与 `board-support` 的 `config::FLASH_SIZE` 一致。两者含义不同：
/// 这里决定**链接脚本**里 FLASH 区域的长度，那里决定 **Boot ROM 认为器件多大**。
/// 不一致会得到"能编译、能烧录、但启动不了"的镜像，因此由本文件生成 `const`
/// 断言把二者绑死 —— 让它在编译期失败，而不是在板子上失败。
const FLASH_SIZE: usize = 32 * 1024 * 1024;

/// 栈大小（字节）：既用于生成链接脚本，也生成给固件做编译期断言。
///
/// # 为什么这个数字必然是两份
///
/// 构建脚本**先于**所有 crate 编译，拿不到 `memory-layout::STACK_SIZE`
/// （那会造成循环依赖）。所以"值"在这里，"含义"在
/// [`memory_layout`](../memory-layout/src/lib.rs) 里。两者由**两道**检查绑死：
/// 本文件生成的编译期断言（不一致就编译失败）+ `cargo xtask check-memory`
/// （再从链接产物的 `.stack` 段核实一次大小）。
const STACK_BYTES: usize = 48 * 1024;

/// 横幅行数上限。
///
/// 横幅在**启动突发**里输出，而控制台 TX 队列是按实测突发（约 3.7 KB）定容的 8 KiB。
/// 过长的横幅会挤占余量、增加 `txdrop` 风险，所以在构建期就把它挡下来 ——
/// 快速失败比"开机丢几行"好。
const MAX_BANNER_LINES: usize = 64;

/// 横幅字节数上限（理由同上）。
const MAX_BANNER_BYTES: usize = 4096;

fn main() {
    // 1) 内存映射与启动头。
    //
    // ⚠️ 栈大小是**实测**定的，三次都被真机打穿过：
    //
    // - 2026-10-09：8 KiB 不够 —— `init` 要同时把 ~4 KiB 的装载缓冲与
    //   ~3.5 KiB 的启动工作区放上栈，SP 溢出到 `0x1FFFE7F0`，HardFault，
    //   串口一行都出不来。改成 16 KiB。
    // - 2026-10-10（同日第二次）：32 KiB 在"启用 4 字节寻址、文件系统扩到 446 块"之后
    //   只剩不到 1 KB 余量 —— 真机表现是启动后**串口长时间无输出 + SWD 也连不上**
    //   （栈被用穿后进入 lockup）。峰值来自两处叠加：`board::init` 局部构造的
    //   文件系统实例（446 块 + 8 文件 ≈ 12 KiB）与它被搬进静态存储时的目标副本。
    //   改成 48 KiB。
    // - 2026-10-10：16 KiB 被文件系统打穿 —— `fs::mount()` 返回的实例约 **7 KiB**
    //   （190 块 + 16 文件的常驻索引），而它还要再被搬进 BSP 的静态存储，
    //   于是"临时量 + 目标副本"同时存在。真机取证：`CFSR=0x400`（STKERR，入栈失败）、
    //   `SP=0x1FFFF838`（已越过 DTCM 下界 `0x20000000`）、`HFSR=0x40000000`（FORCED）。
    //   改成 32 KiB。
    //
    // DTCM 共 64 KiB，`imxrt-rt` 把栈放在最底部（`0x20000000` 起）、向量表紧随其后，
    // 其余大部分本来是空的 —— 因此这里增大栈几乎不占别的资源。
    // 若将来要缩小，先看 `docs/构建与验证.md` 里"栈余量"那条：它要求先有**测量**，
    // 而不是先有数字。
    let mut runtime =
        imxrt_rt::RuntimeBuilder::from_flexspi(imxrt_rt::Family::Imxrt1020, FLASH_SIZE);
    // `.rodata` 直接留在 Flash（XIP 只读），不再复制到 OCRAM。
    // 常量表/字符串访问频率低，用 XIP 换取片上 SRAM 空间是划算的。
    runtime.rodata(imxrt_rt::Memory::Flash);
    runtime
        .stack_size(STACK_BYTES)
        .build()
        .expect("生成 i.MX RT1021 链接脚本失败");

    // 2) 跨文件一致性断言（生成到 OUT_DIR，由 main.rs include!）。
    emit_flash_size_assertion();

    // 3) 版本号注入：`VERSION` 文件是唯一真源。
    // 4) 启动横幅注入：仓库根 `banner.txt` 是唯一真源（改文件即改横幅，不必改代码）。
    let version = read_version();
    emit_version(&version);
    emit_banner(&version);

    // 把 miniz 的压缩/解压代码整体放进 `.xip`：日志归档冷路径，
    // 不参与 Flash 擦写。直接改本 crate 生成的 imxrt-link.x，不额外引入
    // 第二个完整脚本（否则 MEMORY 会重复定义）。
    let _ = patch_linker_script();

    let script = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"))
        .join("sdram-log.x");
    println!("cargo:rustc-link-arg=-T{}", script.display());
    println!("cargo:rerun-if-changed=sdram-log.x");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../VERSION");
    println!("cargo:rerun-if-changed=../../banner.txt");
}

/// 找到 imxrt-rt 生成的链接脚本，把 miniz 输入 section 加进 `.xip`。
///
/// 依赖库无法逐个函数加 `link_section`；这里在构建期生成一份完整脚本副本，
/// 让 `.xip` 的输出 section 先捕获压缩库代码，再交给原脚本的其余规则。
fn patch_linker_script() -> Option<PathBuf> {
    // `RuntimeBuilder::build()` 已把生成的脚本写到固件自己的 OUT_DIR。
    let out = PathBuf::from(env::var("OUT_DIR").ok()?);
    let source_path = out.join("imxrt-link.x");
    let source = fs::read_to_string(&source_path).ok()?;
    let needle = "*(.xip .xip.*);";
    let patched = source.replace(
        needle,
        "*(.xip .xip.*);\n    *(.text.*miniz_oxide*deflate*);\n    *(.text.*miniz_oxide*inflate*);",
    );
    if patched == source {
        return None;
    }
    fs::write(&source_path, patched).ok()?;
    Some(source_path)
}

/// 生成把 `build.rs`、`board-support` 两处容量常量绑在一起的编译期断言。
///
/// 刻意不用 `as` 转换：两个常量都按自己的类型比较，避免"截断后恰好相等"的假通过。
fn emit_flash_size_assertion() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    fs::write(
        out_dir.join("consistency.rs"),
        format!(
            "// 由 build.rs 生成，请勿手改。\n\
             // 链接脚本的 FLASH 长度（本文件）与 BSP 报告的器件容量必须一致。\n\
             const _: () = assert!(board_support::config::FLASH_SIZE == {FLASH_SIZE});\n\
             const _: () = assert!(board_support::flash::SIZE_BYTES == {FLASH_SIZE}u32);\n\
             // BSP 的存储槽位必须与擦除扇区对齐，否则擦除会波及相邻记录。\n\
             // 时基装载值不能为 0，否则 PIT 不会产生中断。\n\
             const _: () = assert!(board_support::config::TICK_TICKS > 0);\n\
             // 链接脚本给的栈大小必须与 BSP 的栈测量模块一致，\n\
             // 否则「余量」会被算在一个不存在的区间上。\n\
             const _: () = assert!(board_support::stacks::STACK_SIZE == {STACK_BYTES});\n"
        ),
    )
    .expect("写 consistency.rs 失败");
}

/// 从仓库根目录的 `VERSION` 读取版本号（唯一真源）。
fn read_version() -> String {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"));
    let version_path = manifest_dir.join("../../VERSION");
    let raw = fs::read_to_string(&version_path).expect("读取 VERSION 失败");
    raw.trim().to_string()
}

/// 注入版本号（`BASE_VERSION*` 环境变量）。
///
/// 版本号是**整包升级的门禁**（升级受理要求设备上报的版本与发布记录匹配）。
/// 因此它必须有一个唯一真源，而不是散落在若干 `const` 里。
fn emit_version(version: &str) {
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    let patch = parts.next().unwrap_or("0");

    // VERSION 的格式是构建期的契约：格式错就**构建失败**，而不是运行期解析出 0。
    let parse = |text: &str, what: &str| -> u16 {
        text.parse::<u16>()
            .unwrap_or_else(|_| panic!("VERSION 的{what}部分不是合法数字：{text:?}"))
    };
    let major = parse(major, "主");
    let minor = parse(minor, "次");
    let patch = parse(patch, "修订");

    println!("cargo:rustc-env=BASE_VERSION={version}");
    println!("cargo:rustc-env=BASE_VERSION_MAJOR={major}");
    println!("cargo:rustc-env=BASE_VERSION_MINOR={minor}");
    println!("cargo:rustc-env=BASE_VERSION_PATCH={patch}");
}

/// 注入启动横幅：仓库根 `banner.txt` 是**唯一真源**，改文件即改横幅（无需改代码）。
///
/// # 规则
///
/// - 支持占位符 `{version}`：构建期替换为 `VERSION` 的当前值 —— 于是"横幅可以随意
///   改"与"版本号只有一个真源"这两件事不冲突；
/// - 非空、行数与字节数有上限（见 [`MAX_BANNER_LINES`]/[`MAX_BANNER_BYTES`]）；
/// - 违规一律**构建失败**：横幅是开机的第一眼输出，"为空/过长"应当在构建期发现，
///   而不是等真机上看到一片空白或丢掉后半段。
///
/// 产物写到 `OUT_DIR/banner.txt`，由 `main.rs` 用 `include_str!` 静态包含
/// （因此不占堆、不解析、不改运行期行为）。
fn emit_banner(version: &str) {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"));
    let banner_path = manifest_dir.join("../../banner.txt");
    let raw = fs::read_to_string(&banner_path).unwrap_or_else(|error| {
        panic!(
            "读取 {} 失败：{error} —— 启动横幅的文本文件必须存在于仓库根目录",
            banner_path.display()
        )
    });
    let text = raw.replace("{version}", version);

    assert!(
        !text.trim().is_empty(),
        "banner.txt 为空：横幅不能为空（宁可构建失败，也不要开机一片空白）"
    );
    let lines = text.lines().count();
    assert!(
        lines <= MAX_BANNER_LINES,
        "banner.txt 有 {lines} 行，超过上限 {MAX_BANNER_LINES} 行"
    );
    // 空行在 print_banner 里会被逐行写成真正的空行：图形每行之间多一条空白，
    // 而且 `make bytecheck` 只核对第一行，抓不到这种"看起来像字体问题"的漂移。
    // 这里的首行/行距由 print_banner 自己控制，banner.txt 里出现空行几乎一定是
    // 粘贴时多带了换行 —— 构建期直接拒绝，避免同一版固件因源文件差异输出不同。
    assert!(
        !text.lines().any(|line| line.trim().is_empty()),
        "banner.txt 含空行；图形行之间不允许空行（print_banner 已负责首行前的空行）"
    );
    assert!(
        text.len() <= MAX_BANNER_BYTES,
        "banner.txt 有 {} 字节，超过上限 {MAX_BANNER_BYTES} 字节",
        text.len()
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    fs::write(out_dir.join("banner.txt"), &text).expect("写 banner.txt 失败");
}
