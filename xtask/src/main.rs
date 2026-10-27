//! `xtask` —— 把"工程约束"变成**可执行门禁**。
//!
//! # 为什么要有这一层
//!
//! 分层、unsafe 预算、需求追溯、镜像布局、体积上限这些约束，如果只写在文档里，
//! 它们的实际强度等于"下一个人记得检查"。本 crate 把它们变成会失败的构建步骤。
//!
//! # 设计原则
//!
//! 1. **宁可失败，不要静默通过**：解析不出来就是错误，绝不"跳过这一项"。
//! 2. **产物级优先**：能对 ELF 断言的不对源码断言 —— 后者只证明代码写了什么，
//!    前者证明**镜像里真的是什么**。
//! 3. **零重依赖**：只依赖 `object`（真 ELF 解析）。少一个依赖，
//!    就少一份需要论证的工具面（这与功能安全的目标一致）。

use std::collections::BTreeSet;

// `Object` trait 提供 sections()/data() 等入口，必须显式引入。
use object::{Object as _, ObjectSection as _, ObjectSymbol as _};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 纯逻辑 crate：依赖闭包里**不得**出现任何硬件相关 crate。
const PURE_CRATES: &[&str] = &[
    "primitives",
    "wire-format",
    "ports",
    "simulator",
    "sdk",
    "product",
    "sdram-timing",
    "shell",
    "flash-layout",
    "filesystem",
    "mpu-regions",
    "memory-layout",
    "realtime",
    "fault-decode",
    "block-pool",
    "log",
    "gzip",
    "nor-flash",
];

/// 把 [`PURE_CRATES`] 展开成 `cargo` 的 `-p` 参数序列。
///
/// **这是"纯逻辑 crate 有哪些"的唯一来源**：`verify`、`make test`、
/// CI 的 clippy/测试步骤全部由它生成，因此新增一个纯逻辑 crate 只需改上面的列表。
///
/// 2026-10-10 的教训：Makefile 与 CI 曾经**各自硬编码**了 5 个 crate，于是在
/// 纯逻辑层长到 10 个之后，`make test` 仍只跑 144 个用例（门禁口径是 300 个），
/// CI 也漏掉 5 个 crate 的 lint 与测试。同一件事写两遍必然漂移。
fn pure_package_args() -> Vec<String> {
    let mut args = Vec::new();
    for name in PURE_CRATES {
        args.push("-p".to_string());
        args.push((*name).to_string());
    }
    args
}

/// 主机测试：纯逻辑层的全部 crate。
///
/// 跑完把**通过数**落到 `tmp/host-tests.json`：`cargo xtask context` 会把它带出去，
/// 于是"现在的测试规模"不需要重跑一遍（Agent 与脚本都直接读得到）。
fn test_host() -> Result<(), String> {
    let mut args = vec!["test", "--offline", "--target", "x86_64-unknown-linux-gnu"];
    let packages = pure_package_args();
    args.extend(packages.iter().map(String::as_str));
    let output = std::process::Command::new("cargo")
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .map_err(|error| format!("执行 cargo test 失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprint!("{stderr}");
    // 汇总每一行 `test result: ok. N passed`。
    let passed: u64 = stdout
        .lines()
        .filter_map(|line| line.split("test result: ok. ").nth(1))
        .filter_map(|rest| rest.split(" passed").next())
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .sum();
    let summary = format!(
        "{{\"passed\": {passed}, \"recorded\": \"{}\"}}\n",
        today_iso8601()
    );
    let path = workspace_root().join("tmp/host-tests.json");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, summary);
    if !output.status.success() {
        return Err("主机测试未全部通过".to_string());
    }
    println!("[xtask] 主机测试：{passed} 个用例通过（已记入 tmp/host-tests.json）");
    Ok(())
}

/// 主机 lint：纯逻辑层 + 全部测试目标。
fn lint_host() -> Result<(), String> {
    let mut args = vec![
        "clippy",
        "--offline",
        "--release",
        "--target",
        "x86_64-unknown-linux-gnu",
    ];
    let packages = pure_package_args();
    let owned: Vec<String> = packages;
    args.extend(owned.iter().map(String::as_str));
    args.extend(["--all-targets", "--", "-D", "warnings"]);
    run("cargo", &args)
}

/// 固件目标 lint（只能 lint lib/bin：`no_std` 目标没有 `test` crate）。
fn lint_target() -> Result<(), String> {
    run(
        "cargo",
        &["clippy", "--offline", "--release", "--", "-D", "warnings"],
    )
}

/// 打印纯逻辑 crate 列表（一行一个，供脚本/Agent 取用）。
fn pure_crates() -> Result<(), String> {
    for name in PURE_CRATES {
        println!("{name}");
    }
    Ok(())
}

/// 从链接产物里读出一个段的（VMA, 大小）。
struct SectionInfo {
    /// 段名。
    name: String,
    /// 虚拟地址（运行期地址）。
    address: u64,
    /// 段大小（字节）。
    size: u64,
}

/// 读出所有**已分配**段的名字/地址/大小。
///
/// 用 `object` 直接解析 ELF（与 `check-image` 同一条路）：判据必须来自链接产物，
/// 而不是源码里的结构体。
fn read_sections(path: &Path) -> Result<Vec<SectionInfo>, String> {
    let data = fs::read(path).map_err(|error| format!("读取 {path:?} 失败：{error}"))?;
    let file = object::File::parse(&*data).map_err(|error| format!("解析 ELF 失败：{error}"))?;
    let mut sections = Vec::new();
    for section in file.sections() {
        let Ok(name) = section.name() else {
            continue;
        };
        // 只关心真正占地址的段（`.text`/`.stack`/`.vector_table`）。
        let address = section.address();
        let size = section.size();
        if size == 0 {
            continue;
        }
        sections.push(SectionInfo {
            name: name.to_string(),
            address,
            size,
        });
    }
    Ok(sections)
}

/// **关中断只能走测量入口**。
///
/// 为什么这条要成为门禁：一个"没被测量的临界区"就是"没人知道有多长的关中断窗口"。
/// 本项目已经在这上面栽过一次 —— 终端 `fs` 命令第一版在 `with_fs`（关中断）里调用会
/// 全盘扫描的 `stats()`，主机实测读 28.3 MB 介质、真机折算 **0.5–2 s** 关中断；
/// 而当时**构建、测试、门禁全都照样通过**。
///
/// 现在唯一的合法入口是 [`board_support::rt::measured_cs`]：它读 DWT 周期计数器并把
/// 这段窗口记进 `realtime` 的统计。于是"有人偷偷加了一个长临界区"会同时体现在
/// ①这道门禁（裸用被拒）与 ②运行时的 `irq-off` 最大值（超预算就被判定）。
///
/// 允许清单只有测量模块本身（它必须在临界区内读计数器）；测试文件也放行
/// （它们不参与固件构建）。
fn check_critical() -> Result<(), String> {
    const ALLOWED: &[&str] = &["crates/board-support/src/rt.rs"];
    const FORBIDDEN: &[&str] = &[
        "critical_section::with",
        "interrupt::free",
        "interrupt::disable",
        "cortex_m::interrupt::free",
    ];

    let root = workspace_root();
    let mut sources: Vec<(String, String)> = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut sources)?;

    let mut offenders: Vec<String> = Vec::new();
    for (path, text) in &sources {
        if ALLOWED.contains(&path.as_str()) || path.contains("/tests") || path.contains("tests_") {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            // 注释里的说明不算（本文件与门禁手册都会提到这些名字）。
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if FORBIDDEN.iter().any(|needle| line.contains(needle)) {
                offenders.push(format!("{path}:{}  {}", number + 1, trimmed));
            }
        }
    }

    if offenders.is_empty() {
        println!(
            "[xtask] check-critical 通过：关中断只经 `board_support::rt::measured_cs` \
             （允许清单：{}；其余为测试文件）",
            ALLOWED.join(", ")
        );
        return Ok(());
    }
    println!(
        "[xtask] check-critical 失败：以下位置绕过了测量入口（没人知道这个关中断窗口有多长）："
    );
    for line in &offenders {
        println!("  {line}");
    }
    println!(
        "  ↑ 改用 `board_support::rt::measured_cs(|cs| {{ ... }})`：它会计入 `irq-off` 统计，\
         超预算时被 `realtime` 判为违规。"
    );
    Err(format!("{} 处裸用关中断", offenders.len()))
}

/// 公开项调用者清点：列出**在整个仓库里只出现过一次**的 `pub fn` / `pub const`。
///
/// # 为什么值得有这条（它是怎么来的）
///
/// 2026-10-10 我用"手工数引用次数"的办法排查过一遍公开 API，一次就找出三处真问题：
/// `filesystem::stats()` 与 `rescan()`、`FileSystem::block_erase_count()` 等在
/// 固件/BSP/终端里都没有调用者 —— 其中"磨损统计没人接"正是评审里挂了很久的一条
/// 建议（F4）。手工排查有效但会忘；这里把它变成每次都能重跑的清点。
///
/// **刻意不失败**（与 `check-gaps` 同一取舍）：一个公开项没有调用者可能是
/// ① 给上层/测试用的扩展点、② 真的忘了接线。判据是"让它可见"，不是"禁止存在"。
/// 输出带 `文件:行` 指针，便于直接去看为什么。
fn check_api_usage() -> Result<(), String> {
    let root = workspace_root();
    // 1) 收集全部源码（含 xtask：命令本身也可能被工具调用）。
    let mut sources: Vec<(String, String)> = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut sources)?;
    collect_rust_sources(&root.join("xtask/src"), &mut sources)?;
    let all: String = sources
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // 2) 找出公开项定义，并数它们在全仓出现的次数。
    let mut unused: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (path, text) in &sources {
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // 只看纯逻辑层与 BSP 的公开项；`#[cfg(test)]` 里的辅助函数不算。
            // 只看 `pub fn`：`pub const` 多是"声明的硬件事实"（引脚、波特率、容量），
            // 在对应驱动实现之前本来就没有调用者 —— 那是**已知缺口**，不是漏接线，
            // 混在一起会把真正的信号（有行为却没人调）淹掉。
            let Some(rest) = trimmed
                .strip_prefix("pub fn ")
                .or_else(|| trimmed.strip_prefix("pub const fn "))
            else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if name.is_empty() {
                continue;
            }
            total += 1;
            let occurrences = all.matches(name.as_str()).count();
            // 只出现一次 = 只有定义本身（文档注释里提到会 +1，因此阈值取 1）。
            if occurrences <= 1 {
                unused.push(format!("{path}:{}  {name}", number + 1));
            }
        }
    }
    if unused.is_empty() {
        println!("[xtask] check-api-usage：{total} 个公开项都有调用者");
        return Ok(());
    }
    println!(
        "[xtask] check-api-usage：{total} 个公开项里有 {} 个**没有调用者**（刻意不失败，但要看见）：",
        unused.len()
    );
    for item in &unused {
        println!("  {item}");
    }
    println!(
        "  ↑ 判断标准：该项在整仓只出现一次（即只有定义）。可能是给上层留的扩展点，\
         也可能是**忘了接线** —— 后者是真缺口，请接上或按 GAP 留痕。"
    );
    Ok(())
}

/// 递归收集 `.rs` 源码（只用于文本级清点，不做语法解析）。
fn collect_rust_sources(dir: &Path, out: &mut Vec<(String, String)>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_rust_sources(&path, out)?;
            continue;
        }
        if path.extension().map(|ext| ext != "rs").unwrap_or(true) {
            continue;
        }
        let short = path
            .strip_prefix(workspace_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        out.push((short, read(&path)?));
    }
    Ok(())
}

/// 文档门禁：**关键数字必须与代码一致**。
///
/// # 为什么要这条门禁
///
/// 2026-10-10 的一次人工审计在文档里查出 **27 处**与代码/门禁不符的数字与结论
/// （映像大小、栈大小、SP、unsafe 数、测试数、crate 数、串口别名、"已接线/未接线"…）。
/// 人工审计能查一次，但**下一次改动又会漂**。这里的做法是把"会被反复引用的数字"
/// 变成机械断言：每条都从代码/链接产物现算，然后要求**指定的那句话**里就是那个数。
///
/// 每条断言都自带"这条事实住在文档的哪里"，所以失败信息能直接告诉你去改哪一句。
/// 只查**便宜且唯一**的事实（不跑测试、不跑覆盖率），因此可以放进 `verify`。
fn check_docs() -> Result<(), String> {
    let root = workspace_root();

    // ---- 事实：全部现算 ----
    let manifest = read(&root.join("Cargo.toml"))?;
    let members = manifest
        .split("members")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .map(|array| array.matches("\"crates/").count())
        .unwrap_or(0);
    let pure = PURE_CRATES.len();
    let unsafe_budget = read(&root.join("unsafe.budget"))?
        .lines()
        .find_map(|line| line.trim().strip_prefix("blocks="))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or("unsafe.budget 里找不到 blocks=N")?;
    let mut gaps = Vec::new();
    collect_gaps(&root.join("crates"), &mut gaps)?;
    // 静态门禁的条数：**从命令表现算**（`COMMANDS` 里带 `check` 的那些）。
    let static_gates = COMMANDS
        .iter()
        .filter(|command| command.check.is_some())
        .count();

    // ---- 断言：文档里的**那一句话**必须与事实一致 ----
    // (文件, 必须出现的字符串, 这句话的含义)
    let expectations: Vec<(&str, String, &str)> = vec![
        (
            "AGENTS.md",
            format!("**{members} 个成员 crate**"),
            "规模行：成员 crate 数",
        ),
        (
            "AGENTS.md",
            format!("**{pure} 个纯逻辑**"),
            "规模行：可主机测试的纯逻辑 crate 数",
        ),
        (
            "AGENTS.md",
            format!("check-layering` | {pure} 个纯逻辑 crate"),
            "命令表：check-layering 覆盖的纯逻辑 crate 数",
        ),
        (
            "AGENTS.md",
            format!("**{unsafe_budget}/{unsafe_budget}**"),
            "铁律 5：unsafe 预算当前值",
        ),
        (
            "AGENTS.md",
            format!("**{} 项**", gaps.len()),
            "第 7 节：未实现项数量",
        ),
        (
            "docs/00-索引/项目地图.md",
            format!("{members} 个成员 crate"),
            "一页纸总览：成员 crate 数",
        ),
        (
            "docs/00-索引/项目地图.md",
            format!("| 工作区成员 crate | **{members}**"),
            "规模表：工作区成员 crate 行",
        ),
        (
            "docs/00-索引/项目地图.md",
            format!("| 纯逻辑 crate（门禁口径） | **{pure}**"),
            "规模表：纯逻辑 crate 行",
        ),
        (
            "AGENTS.md",
            format!("{static_gates} 项静态门禁"),
            "接手第一小时：静态门禁条数",
        ),
        (
            "docs/00-索引/项目地图.md",
            format!("| 静态门禁子命令 | **{static_gates}**"),
            "规模表：静态门禁条数",
        ),
        (
            "README.md",
            format!("静态门禁        {static_gates} 项子命令"),
            "README 规模段：静态门禁条数",
        ),
    ];

    let mut problems: Vec<String> = Vec::new();
    // 文档可达性：`docs/**/*.md` 每一篇都要被仓库里的某个 `.md` 链接到。
    // 理由：孤儿文档等于不存在 —— 接手者（尤其 Agent）不会去翻目录树，
    // 而这一条**机械可判**，所以不该靠人记得维护索引。
    check_doc_reachability(&root, &mut problems)?;
    // 文档里出现的每条 `cargo xtask X` / `make X` 都必须真实存在。
    check_invocations(&root, &mut problems)?;
    for (file, needle, meaning) in &expectations {
        let document = match read(&root.join(file)) {
            Ok(text) => text,
            Err(error) => {
                problems.push(format!("{file} 读不到（{error}）"));
                continue;
            }
        };
        if !document.contains(needle.as_str()) {
            problems.push(format!(
                "{file} 里找不到「{needle}」（{meaning}）—— 数字变了就要改文档，不要改代码来迁就文档"
            ));
        }
    }

    // 分层文档的依赖表：**既查漏、也查陈旧**。
    //
    // 为什么要有这一条：`docs/02-架构/分层与依赖规则.md` 里那张表长期靠人维护，
    // 2026-10-12 一查发现只列了 11 个纯逻辑 crate —— `realtime` 与 `fault-decode`
    // 两个已经存在好几轮的 crate **根本没被列进去**。而这张表是接手的人理解分层的第一站：
    // 它的错不会让任何测试失败，只会让读它的人形成错误认知。因此把它变成机器判据：
    // 表里的每个 crate 名必须**恰好**等于 `PURE_CRATES` ∪ {两个允许碰寄存器的 crate}。
    {
        let layering_doc = root.join("docs").join("02-架构").join("分层与依赖规则.md");
        let document = read(&layering_doc)
            .map_err(|error| format!("读 {} 失败：{error}", layering_doc.display()))?;
        // 允许出现在该表里、但**不是**纯逻辑的实现层 crate。
        let implementation_crates = ["board-support", "firmware"];
        let mut listed: Vec<String> = Vec::new();
        for line in document.lines() {
            // **不假设前缀**：只认"表格行里第一个反引号包起来的名字"。
            // 硬编码任何前缀都会在下次改名时让这条断言静默失效
            // （一行都匹配不到 ⇒ 全部 crate 都"缺失"⇒ 报错方向相反、更难查）。
            let Some(rest) = line.strip_prefix("| `") else {
                continue;
            };
            let Some(name_end) = rest.find('`') else {
                continue;
            };
            listed.push(rest[..name_end].to_string());
        }
        for expected in PURE_CRATES.iter().chain(implementation_crates.iter()) {
            if !listed.iter().any(|name| name == expected) {
                problems.push(format!(
                    "docs/02-架构/分层与依赖规则.md 的依赖表里没有「{expected}」                     —— 新 crate 必须在这一页里出现（这张表是接手者理解分层的第一站）"
                ));
            }
        }
        for name in &listed {
            let known = PURE_CRATES.contains(&name.as_str())
                || implementation_crates.contains(&name.as_str());
            if !known {
                problems.push(format!(
                    "docs/02-架构/分层与依赖规则.md 的依赖表里列了「{name}」，                     但代码里没有这个 crate —— 陈旧的行比缺行更误导"
                ));
            }
        }
    }

    if !problems.is_empty() {
        return Err(format!("文档数字与代码不符：\n  {}", problems.join("\n  ")));
    }
    println!(
        "[xtask] check-docs 通过：成员 {members}、纯逻辑 {pure}、静态门禁 {static_gates}、\
         unsafe {unsafe_budget}/{unsafe_budget}、缺口 {} —— {} 处文档断言与代码一致",
        gaps.len(),
        expectations.len()
    );
    Ok(())
}

/// 内存放置门禁：断言**链接产物实际放在哪**，与 `memory-layout` 声明的事实一致。
///
/// # 为什么这条门禁值得存在
///
/// `.text` 跑在 ITCM、`.stack` 落在 DTCM、向量表在 DTCM —— 这些是**性能与正确性的前提**，
/// 但在此之前只写在文档与 linker 脚本里：一旦有人加了个大数组或改了链接脚本，
/// 段就会悄悄挪到别处（或者把 ITCM 挤爆），而**没有任何检查会失败**。
/// 这里从 ELF 里读真实地址来判，判据（各区域基址/大小、ITCM 代码预算、栈大小）
/// 全部来自纯逻辑 crate `memory-layout`，不在门禁里另写一份。
/// 每篇文档都必须能从文档图里走到（被某个 `.md` 以链接形式引用）。
///
/// 排除：`docs/README.md` 自身（它是索引入口）、以及由工具生成的文件
/// （生成物的引用关系由生成器负责，例如需求追溯矩阵）。
///
/// 为什么机械可判就一定要判：**孤儿文档等于不存在** —— 接手者（尤其 Agent）不会去
/// 翻目录树，而"记得维护索引"这件事在多次改动后必然失守（本项目已有 27 处漂移的先例）。
fn check_doc_reachability(root: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    let mut docs: Vec<PathBuf> = Vec::new();
    collect_markdown(&root.join("docs"), &mut docs)?;
    for root_file in ["README.md", "AGENTS.md"] {
        let path = root.join(root_file);
        if path.exists() {
            docs.push(path);
        }
    }
    let mut corpus = String::new();
    for path in &docs {
        corpus.push_str(&read(path)?);
        corpus.push('\n');
    }
    const GENERATED: &[&str] = &["需求追溯矩阵.md"];
    for path in &docs {
        let Ok(relative) = path.strip_prefix(root.join("docs")) else {
            continue;
        };
        let relative = relative.display().to_string();
        if relative == "README.md" || GENERATED.iter().any(|name| relative.ends_with(name)) {
            continue;
        }
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        // 链接可能写成「相对 docs/ 的路径」或「同目录下的文件名」。
        // 命中一次即算被引用 —— 语料里不含本文件自身（上面收集的是全部 md 正文，
        // 而正文里通常不会出现自己的文件名），所以不需要更复杂的阈值。
        let reachable = corpus.contains(relative.as_str()) || corpus.contains(file_name.as_str());
        if !reachable {
            problems.push(format!(
                "docs/{relative} 没有被任何文档链接到（孤儿文档）—— 请在 docs/README.md 的索引里登记它"
            ));
        }
    }
    Ok(())
}

/// 递归收集 `.md` 文件。
fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_markdown(&path, out)?;
            continue;
        }
        if path.extension().map(|ext| ext == "md").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// 文档里出现的每条 `cargo xtask X` / `make X` 都必须**真实存在**。
///
/// 为什么值得断言：文档里的命令是给人（和 Agent）**照抄**的。命令改名或删掉之后，
/// 文档不会自己更新 —— 而"照着文档敲、报了 command not found"是最伤信任的一类错误。
/// 反过来它也防止"新命令只活在源码里、文档从不提"。
fn check_invocations(root: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    // 1) 文档语料（与可达性检查同一批文件）。
    let mut docs: Vec<PathBuf> = Vec::new();
    collect_markdown(&root.join("docs"), &mut docs)?;
    for name in ["README.md", "AGENTS.md"] {
        let path = root.join(name);
        if path.exists() {
            docs.push(path);
        }
    }
    // 2) 合法的 `make` 目标：从 Makefile 里抓 `.PHONY:` 与 `^目标:`。
    let makefile = read(&root.join("Makefile"))?;
    let mut targets: Vec<String> = Vec::new();
    for line in makefile.lines() {
        if let Some(rest) = line.strip_prefix(".PHONY:") {
            for name in rest.split_whitespace() {
                targets.push(name.to_string());
            }
        } else if line.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_') {
            if let Some((name, _)) = line.split_once(':') {
                targets.push(name.trim().to_string());
            }
        }
    }

    // 3) 逐行扫描文档，抽出调用并核对。
    for path in &docs {
        let short = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        for (number, line) in read(path)?.lines().enumerate() {
            for (prefix, known) in [
                // 白名单 = 命令表 + 调度器接受但不在表里的别名（`cargo xtask` 不带参数
                // 与 `--help`/`-h` 都打印帮助）。别名写在这里而不是命令表里，是因为
                // 命令表要用于"列出全部子命令"，把 `-h` 列进去只会让人困惑。
                ("cargo xtask ", &{
                    let mut known: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
                    known.extend(["-h", "--help"]);
                    known
                }),
                (
                    "make ",
                    &targets.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
            ] {
                let mut rest = line;
                while let Some(index) = rest.find(prefix) {
                    let tail = &rest[index + prefix.len()..];
                    let token: String = tail
                        .chars()
                        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
                        .collect();
                    rest = &rest[index + prefix.len()..];
                    // `cargo xtask` 后面直接换行/结束 = 打印帮助，合法。
                    // 空 token（`cargo xtask` 后直接结束）与**以 `-` 结尾**的 token
                    // （文档里的通配写法，如「`check-*` 11 项」）都不是命令名，跳过。
                    if token.is_empty() || token.ends_with('-') {
                        continue;
                    }
                    if !known.iter().any(|name| **name == token) {
                        problems.push(format!(
                            "{short}:{} 引用了不存在的命令 `{prefix}{token}` \
                             —— 改文档，或把该命令加进 {}",
                            number + 1,
                            if prefix.starts_with("cargo") {
                                "`COMMANDS` 表"
                            } else {
                                "Makefile"
                            }
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_memory() -> Result<(), String> {
    let elf = firmware_elf()?;
    let sections = read_sections(&elf)?;
    let mut problems: Vec<String> = Vec::new();

    // 1) 代码必须在 ITCM 内，且不超过预算（预算 = ITCM 大小 − 保留余量）。
    let text = sections
        .iter()
        .find(|section| section.name == ".text")
        .ok_or("链接产物里找不到 .text 段")?;
    let text_address = u32::try_from(text.address).map_err(|_| "段地址超出 32 位地址空间")?;
    if !memory_layout::ITCM.contains(text_address) {
        problems.push(format!(
            ".text 的地址 {:#010x} 不在 ITCM（{:#010x}..{:#010x}）内：取指会走慢速通道",
            text.address,
            memory_layout::ITCM.base,
            memory_layout::ITCM.end()
        ));
    }
    if text.size > u64::from(memory_layout::ITCM_CODE_BUDGET_BYTES) {
        problems.push(format!(
            ".text 占 {} 字节，超过 ITCM 代码预算 {} 字节（ITCM {} − 保留 {}）",
            text.size,
            memory_layout::ITCM_CODE_BUDGET_BYTES,
            memory_layout::ITCM.size,
            memory_layout::ITCM_CODE_RESERVE_BYTES
        ));
    }

    // 2) 栈必须在 DTCM 内，且大小与声明的 STACK_SIZE 一致（否则"栈余量"的百分比是假的）。
    let stack = sections
        .iter()
        .find(|section| section.name == ".stack")
        .ok_or("链接产物里找不到 .stack 段")?;
    let stack_address = u32::try_from(stack.address).map_err(|_| "段地址超出 32 位地址空间")?;
    if !memory_layout::DTCM.contains(stack_address) {
        problems.push(format!(
            ".stack 的地址 {:#010x} 不在 DTCM（{:#010x}..{:#010x}）内",
            stack.address,
            memory_layout::DTCM.base,
            memory_layout::DTCM.end()
        ));
    }
    if stack.size != memory_layout::STACK_SIZE as u64 {
        problems.push(format!(
            ".stack 实际 {} 字节，而 `memory-layout` 声明 STACK_SIZE = {} 字节 —— \
             两者不一致时启动横幅报的「栈余量百分比」就没有意义",
            stack.size,
            memory_layout::STACK_SIZE
        ));
    }

    // 3) 向量表必须在 DTCM 内（异常入口要在单周期内存里）。
    if let Some(vectors) = sections
        .iter()
        .find(|section| section.name == ".vector_table")
    {
        let vectors_address =
            u32::try_from(vectors.address).map_err(|_| "段地址超出 32 位地址空间")?;
        if !memory_layout::DTCM.contains(vectors_address) {
            problems.push(format!(
                ".vector_table 的地址 {:#010x} 不在 DTCM 内",
                vectors.address
            ));
        }
    }

    // 3.5) `.dtcm` 是向量表之后的 DTCM 尾部；不得与栈/向量表重叠，且必须仍在 DTCM 内。
    let dtcm = sections.iter().find(|section| section.name == ".dtcm");
    if let Some(dtcm) = dtcm {
        let start = u32::try_from(dtcm.address).map_err(|_| "段地址超出 32 位地址空间")?;
        let end = dtcm.address.saturating_add(dtcm.size);
        let vectors_end = sections
            .iter()
            .find(|section| section.name == ".vector_table")
            .map_or(stack.address.saturating_add(stack.size), |vectors| {
                vectors.address.saturating_add(vectors.size)
            });
        if !memory_layout::DTCM.contains(start) || end > u64::from(memory_layout::DTCM.end()) {
            problems.push(format!(
                ".dtcm 的地址范围 {:#010x}..{:#010x} 越出 DTCM（{:#010x}..{:#010x}）",
                dtcm.address,
                end,
                memory_layout::DTCM.base,
                memory_layout::DTCM.end()
            ));
        }
        if dtcm.address < vectors_end {
            problems.push(format!(
                ".dtcm 起点 {:#010x} 与栈/向量表末端 {:#010x} 重叠 ——                  它只能放在向量表之后",
                dtcm.address, vectors_end
            ));
        }
    }

    // 4) **OCRAM 里的段不得侵入 DMA 池**。
    //
    // 链接脚本仍然认为 OCRAM 是 128 KiB（`MEMORY` 声明由 `imxrt-rt` 生成），
    // 而我们把顶部 64 KiB 划给了非缓存 DMA 池（`memory_layout::DMA_POOL`）。
    // 链接器**不会**因此报错 —— 它不知道那 64 KiB 已被征用。于是"数据段长进池里"
    // 这件事只会在真机上表现为"偶发传错一帧"，没有任何构建期信号。
    // ⇒ 这条断言把它变成构建失败。
    //
    // 判据：所有 VMA 落在 OCRAM 且**实际占用 Flash/RAM 空间**的段，
    // 其末端必须 ≤ 池基址。（`.bss`/`.uninit` 虽然 `NOLOAD`，但它们确实占 RAM —— 必须算。）
    for section in sections.iter().filter(|section| {
        let address = u32::try_from(section.address).unwrap_or(u32::MAX);
        memory_layout::OCRAM.contains(address)
    }) {
        let end = section.address.saturating_add(section.size);
        if end > u64::from(memory_layout::DMA_POOL.base) {
            problems.push(format!(
                "{} 末端 {:#010x} 越过了 DMA 池基址 {:#010x} —— \
                 OCRAM 可缓存区只有 {} KiB（顶部 {} KiB 已划给非缓存 DMA 池，链接脚本并不知道）。\n\
                 处置：① 把大块数据移出 OCRAM（SDRAM 有 32 MiB，已有固定块静态池）；\
                 ② 或缩小该结构；③ 不要动池的边界 —— 它是 MPU 区域大小的 2 的幂约束下唯一的切法。",
                section.name,
                end,
                memory_layout::DMA_POOL.base,
                memory_layout::OCRAM_CACHEABLE.size / 1024,
                memory_layout::DMA_POOL.size / 1024
            ));
        }
    }

    if !problems.is_empty() {
        return Err(format!("内存放置不符：\n  {}", problems.join("\n  ")));
    }
    println!(
        "[xtask] check-memory 通过：.text {} 字节在 ITCM（预算 {}，余量 {}）；.stack {} 字节在 DTCM；\
         OCRAM 可缓存区 {} KiB（DMA 池 {} KiB 在 {:#010x}，非缓存）；栈余量阈值 {}%",
        text.size,
        memory_layout::ITCM_CODE_BUDGET_BYTES,
        memory_layout::ITCM_CODE_BUDGET_BYTES as u64 - text.size,
        memory_layout::STACK_SIZE,
        memory_layout::OCRAM_CACHEABLE.size / 1024,
        memory_layout::DMA_POOL.size / 1024,
        memory_layout::DMA_POOL.base,
        memory_layout::MIN_STACK_HEADROOM_PERCENT
    );
    if let Some(dtcm) = dtcm {
        println!(
            "[xtask] check-memory：.dtcm {} 字节在 DTCM 尾部（NOLOAD，启动清零）",
            dtcm.size
        );
    }
    Ok(())
}

/// **故障处理器必须真的存在、而且各是各的**。
///
/// # 这条门禁来自一次产物级取证
///
/// 2026-10-12 的审计从链接好的 ELF 里读出：向量表槽 3（`HardFault`）指向的函数
/// 反汇编就是 `push {r7,lr}; mov r7,sp; b .` —— **一个空死循环**；槽 4/5/6 指向
/// `DefaultHandler_`，也是空自旋。也就是说：设备挂掉时**什么都不留、也不复位**，
/// 现场只剩"串口安静"，与"任务挂死"完全不可区分。
///
/// 这种缺陷**编译得过、测试也全绿** —— 只有看链接产物才发现。所以判据放在产物级。
///
/// 判据（对每个槽）：
/// 1. 向量指向的地址落在某个**有名字的符号**上（不然连是谁都不知道）；
/// 2. 四个槽**两两不同**（共用 `DefaultHandler_` 正是"没实现"的典型形态）；
/// 3. 不指向 cortex-m-rt 的空桩（`HardFault_` / `DefaultHandler_`）；
/// 4. 符号体积 ≥ [`MIN_FAULT_HANDLER_BYTES`]（空桩是几字节的 `b .`，赶不上这个下限）。
fn check_fault_handlers() -> Result<(), String> {
    /// 真实故障处理器符号的最小体积（字节）。空桩别名一般是 0 字节。
    const MIN_FAULT_HANDLER_BYTES: u64 = 16;

    let elf_path = firmware_elf()?;
    let bytes = fs::read(&elf_path).map_err(|error| error.to_string())?;
    let file = object::File::parse(&*bytes).map_err(|error| error.to_string())?;

    // 1) 读向量表内容。
    let vectors = file
        .section_by_name(".vector_table")
        .ok_or("链接产物里找不到 .vector_table 段")?;
    let data = vectors.data().map_err(|error| error.to_string())?;

    // 2) 地址 → 符号。⚠️ 同一个地址上常常有**多个别名**（例如 `BusFault`/`UsageFault`
    //    都是默认桩的弱别名），所以这里不能简单地"后写覆盖先写"——那会让 `PWM2_FAULT`
    //    这类中断向量名顶掉真正的异常名（第一版就是这么误报的）。改为**择优**：
    //    优先"体积 > 0"，其次优先"名字不以 `_` 结尾"，最后按字母序取稳定值。
    let mut by_address: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    for symbol in file.symbols() {
        let Ok(name) = symbol.name() else { continue };
        if name.is_empty() {
            continue;
        }
        let address = symbol.address() & !1;
        let candidate = (name.to_string(), symbol.size());
        match by_address.get(&address) {
            Some((existing_name, existing_size))
                if *existing_size > 0
                    || candidate.1 == 0
                    || (existing_name.ends_with('_') && !candidate.0.ends_with('_')) =>
            {
                // 保留已有（它更好或同样好）。
            }
            _ => {
                by_address.insert(address, candidate);
            }
        }
    }

    // 3) 逐槽读出目标地址。
    const SLOTS: &[(usize, &str)] = &[
        (3, "HardFault"),
        (4, "MemManage"),
        (5, "BusFault"),
        (6, "UsageFault"),
    ];
    let mut problems: Vec<String> = Vec::new();
    let mut resolved: Vec<(usize, &str, u64, String, u64)> = Vec::new();
    for (slot, expected) in SLOTS {
        let offset = slot * 4;
        let raw = data
            .get(offset..offset + 4)
            .ok_or_else(|| format!("向量表里没有槽 {slot}（偏移 {offset}）"))?;
        let word = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let target = u64::from(word & !1);
        match by_address.get(&target) {
            None => problems.push(format!(
                "槽 {slot}（{expected}）指向 {target:#010x}，但该地址不属于任何有名字的符号"
            )),
            Some((name, size)) => resolved.push((*slot, expected, target, name.clone(), *size)),
        }
    }

    // 4) 槽 3（HardFault）：**必须**是真实实现。它是本项目"故障可解释"的唯一入口 ——
    //    审计从链接产物里读到过它是个空死循环。
    if let Some((_, _, _, name, size)) = resolved.iter().find(|(slot, ..)| *slot == 3) {
        if name == "HardFault_" || name == "DefaultHandler_" || name.starts_with("___cortex_m_rt") {
            problems.push(format!(
                "HardFault 指向空桩 `{name}` —— 故障时会静默自旋：不留现场、不复位"
            ));
        } else if *size < MIN_FAULT_HANDLER_BYTES {
            problems.push(format!(
                "HardFault 指向 `{name}`，但只有 {size} 字节（下限 {MIN_FAULT_HANDLER_BYTES}）—— 疑似空实现"
            ));
        }
    } else if !problems.is_empty() {
        // 上面已经报过"解析不到"。
    } else {
        problems.push("解析不到 HardFault 槽".to_string());
    }

    // 5) 槽 4/5/6：本项目**刻意**让 MemManage/BusFault/UsageFault 升级到 HardFault
    //    （理由：cortex-m-rt 只允许 HardFault 的处理程序拿到硬件入栈帧，而帧里有
    //    PC/LR/SP —— 现场里最值钱的部分；根因本来就在 CFSR 里）。因此这三种形态都合法：
    //      (a) 三个都是真实处理器且两两不同；
    //      (b) 三个都指向同一处桩（升级设计，本项目当前）；
    //    而**混合**（有的真实、有的桩）说明接线漏了，必须失败。
    let tail: Vec<&(usize, &str, u64, String, u64)> = resolved
        .iter()
        .filter(|(slot, ..)| (4..=6).contains(slot))
        .collect();
    if tail.len() == 3 {
        let is_real = |size: u64, name: &str| {
            size >= MIN_FAULT_HANDLER_BYTES && !name.ends_with('_') && name != "DefaultHandler_"
        };
        let real_count = tail
            .iter()
            .filter(|(_, _, _, name, size)| is_real(*size, name))
            .count();
        let addresses: BTreeSet<u64> = tail.iter().map(|(_, _, address, ..)| *address).collect();
        if real_count == 3 {
            if addresses.len() != 3 {
                problems.push(format!(
                    "MemManage/BusFault/UsageFault 里有多个槽指向同一地址（{} 个不同地址）\
                     —— 各自实现时必须分开",
                    addresses.len()
                ));
            }
        } else if real_count != 0 {
            problems.push(format!(
                "MemManage/BusFault/UsageFault 的接线**混合**：{real_count} 个是真实处理器、\
                 {} 个是默认桩 —— 要么都实现，要么都交给 HardFault（本项目当前选择后者）",
                3 - real_count
            ));
        }
        if real_count == 0 && addresses.len() != 1 {
            problems.push(
                "三个可配置故障都交给默认桩，但它们指向了不同地址 —— 这不是升级设计".to_string(),
            );
        }
    }

    if !problems.is_empty() {
        return Err(format!("故障处理器不符：\n  {}", problems.join("\n  ")));
    }
    let summary: Vec<String> = resolved
        .iter()
        .map(|(slot, expected, _, symbol, size)| format!("{expected}(槽{slot})→{symbol}/{size}B"))
        .collect();
    println!(
        "[xtask] check-fault-handlers 通过：故障向量指向真实实现或刻意升级（{}）",
        summary.join("，")
    );
    Ok(())
}

/// 跑完全部**静态门禁**并给出汇总表（不因第一个失败而中断）。
///
/// # 为什么需要它（与 `verify` 的分工）
///
/// `verify` 是"发布前的总检查"：**遇到第一个失败就停下** —— 这对调试合适，
/// 但对"现在整体健康度如何"不友好（第一个失败会掩盖后面 9 项的结果）。
/// `gates` 相反：全部跑完，逐项给出 PASS/FAIL，**退出码仍然反映是否全过**，
/// 因此它同时适合人看与脚本/Agent 消费（每行一个 `名称 PASS/FAIL`）。
fn gates() -> Result<(), String> {
    // 门禁清单来自 `COMMANDS`（唯一来源），这里不再另写一份。
    let checks: Vec<(&str, fn() -> Result<(), String>)> = COMMANDS
        .iter()
        .filter_map(|command| command.check.map(|check| (command.name, check)))
        .collect();

    let mut failures: Vec<(&str, String)> = Vec::new();
    println!("门禁汇总（每行：名称 结果）");
    for (name, check) in &checks {
        // 门禁自己的输出照常打到 stdout（人看）；这里只记结果。
        match check() {
            Ok(()) => println!("  {name:<20} PASS"),
            Err(reason) => {
                // 原因可能多行：只取首行做汇总，完整原因在下面的详情里给出。
                let first_line = reason.lines().next().unwrap_or("失败").to_string();
                println!("  {name:<20} FAIL  {first_line}");
                failures.push((name, reason));
            }
        }
    }
    if failures.is_empty() {
        println!("\n[xtask] gates：全部 {} 项通过", checks.len());
        return Ok(());
    }
    let mut detail = String::new();
    for (name, reason) in &failures {
        detail.push_str(&format!("\n--- {name} ---\n{reason}"));
    }
    Err(format!(
        "{} / {} 项门禁失败：{}",
        failures.len(),
        checks.len(),
        detail
    ))
}

/// 风格门禁：把"写在文档里的规矩"变成**可执行的检查**。
///
/// 目前强制三条（都是机械可判、且都曾真实发生的漂移）：
///
/// 1. **代码里不得出现 `todo!()` / `unimplemented!()`** —— 这是 AGENTS.md 的铁律 6，
///    但在此之前**只有文档、没有门禁**。`no_std` 固件里一个 `todo!()` 等于 ISR 静默挂死，
///    比"明确返回 NotImplemented"危险得多。文档注释里作为反例提到它是允许的。
/// 2. 每个 crate 的 `lib.rs` **必须有模块文档**（`//!`）—— 接手者第一眼看的就是它。
/// 3. 根 `Cargo.toml` 的 `[workspace.lints]` 必须仍然拒绝 `missing_docs`
///    与 `unsafe_code`（放宽 lint 等于悄悄降低标准，这类改动必须被看见）。
///
/// 测试函数名**允许中文**：本仓库全中文，`cargo test` 的输出是给人看的
/// （109 个测试函数如此，强行改名只会制造噪音）。非测试标识符仍为 ASCII —— 这条
/// 无法机械判定，因此写在 [`docs/04-开发/代码风格.md`] 里靠评审保证。
fn check_style() -> Result<(), String> {
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();

    check_forbidden_macros(&root.join("crates"), &mut violations)?;
    check_forbidden_macros(&root.join("xtask/src"), &mut violations)?;
    check_module_docs(&root, &mut violations)?;
    check_workspace_lints(&root, &mut violations)?;

    if violations.is_empty() {
        println!("[xtask] check-style 通过：无 todo!/unimplemented!、每个 crate 有模块文档、workspace lints 未放宽");
        return Ok(());
    }
    Err(format!(
        "风格违规 {} 处：\n  {}",
        violations.len(),
        violations.join("\n  ")
    ))
}

/// 递归扫描 `todo!()` / `unimplemented!()`（跳过文档注释行）。
fn check_forbidden_macros(dir: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            check_forbidden_macros(&path, violations)?;
            continue;
        }
        if path.extension().map(|ext| ext != "rs").unwrap_or(true) {
            continue;
        }
        let text = read(&path)?;
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // 注释里的提及是"解释为什么不用它"，不算违规。
            if trimmed.starts_with("//") {
                continue;
            }
            // 再去掉字符串字面量：本文件（门禁自己）就要把这两个宏名写成字符串，
            // 否则门禁会把自己的文档判成违规 —— 第一版正是这样，立刻自指报错。
            let code = strip_string_literals(line);
            for forbidden in ["todo!(", "unimplemented!("] {
                if code.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} 出现 `{forbidden}` —— 未实现必须留 GAP 标记 + 返回明确错误变体",
                        path.strip_prefix(workspace_root())
                            .unwrap_or(&path)
                            .display(),
                        number + 1
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 去掉一行里的字符串字面量（粗暴但足够：门禁只关心"代码里有没有这个宏"）。
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            continue;
        }
        out.push(ch);
    }
    out
}

/// 每个 crate 的 `lib.rs` 必须以模块文档开头。
fn check_module_docs(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(root.join("crates")).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let lib = path.join("src/lib.rs");
        if !lib.exists() {
            continue;
        }
        let text = read(&lib)?;
        if !text.starts_with("//!") {
            violations.push(format!(
                "{} 没有模块文档（文件必须以 `//!` 开头，接手者第一眼看的就是它）",
                lib.strip_prefix(root).unwrap_or(&lib).display()
            ));
        }
    }
    Ok(())
}

/// 工作区 lint 不得悄悄放宽。
fn check_workspace_lints(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let manifest = read(&root.join("Cargo.toml"))?;
    for required in ["missing_docs", "unsafe_code"] {
        if !manifest.contains(required) {
            violations.push(format!(
                "根 Cargo.toml 的 lint 配置里找不到 `{required}` —— 放宽 lint 等于降低标准，必须显式评审"
            ));
        }
    }
    Ok(())
}

/// 机器可读的项目上下文（JSON）。
///
/// # 为什么需要它
///
/// 面向"接手的 AI Agent"：与其让它逐个 grep 源码/文档去拼出"这个项目有哪些 crate、
/// 门禁在哪、Flash 怎么分、当前预算是多少"，不如给一条命令直接输出结构化事实。
/// **数字全部现算**（映像大小从链接产物读、缺口现场清点、分区表来自唯一事实来源），
/// 因此它不会像手写文档那样漂移。
///
/// 不在这里跑门禁：那会有 stdout 噪声混进 JSON。要判"过没过"请跑 `verify`
/// （或下面的 `gates` 数组里列出的单项命令）。
fn context() -> Result<(), String> {
    let mut out = String::new();
    let root = workspace_root();
    let version = fs::read_to_string(root.join("VERSION"))
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    out.push_str("{\n");
    out.push_str(&format!("  \"project\": {{\"name\": \"tessera\", \"version\": \"{version}\", \"stage\": \"阶段 1（单槽，映像从 Flash 偏移 0 起；A/B 双槽属 P4）\"}},\n"));

    // crate 划分：纯逻辑层（宿主可测）与硬件层。
    out.push_str("  \"crates\": {\n    \"pure\": [");
    for (index, name) in PURE_CRATES.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{name}\""));
    }
    out.push_str("],\n    \"hardware\": [\"board-support\", \"firmware\"]\n  },\n");

    // 门禁清单（人读描述；agent 可直接照抄命令）。
    out.push_str("  \"gates\": [\n");
    for (index, command) in COMMANDS.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(&format!(
            "    {{\"command\": \"cargo xtask {}\", \"what\": \"{}\", \"is_static_gate\": {}}}",
            command.name,
            command.what,
            command.check.is_some()
        ));
    }
    out.push_str("\n  ],\n");

    // 未实现项：现算。
    let mut gaps: Vec<(String, String, String)> = Vec::new();
    collect_gaps(&root.join("crates"), &mut gaps)?;
    // 按里程碑分组：Agent/人 都需要"每项缺口属于哪个里程碑（Px）"，
    // 而这在源码里是散落的注释，只有清点后才能给出。
    let mut milestones: Vec<(String, usize)> = Vec::new();
    for (milestone, _, _) in &gaps {
        match milestones.iter_mut().find(|(name, _)| name == milestone) {
            Some((_, count)) => *count += 1,
            None => milestones.push((milestone.clone(), 1)),
        }
    }
    milestones.sort();
    out.push_str(&format!("  \"gaps\": {{\"count\": {}", gaps.len()));
    out.push_str(", \"by_milestone\": {");
    for (index, (name, count)) in milestones.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{name}\": {count}"));
    }
    out.push_str(&format!("}}, \"items\": ["));
    for (index, (milestone, location, line)) in gaps.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let text = line.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!(
            "\n    {{\"milestone\": \"{milestone}\", \"location\": \"{location}\", \"source\": \"{text}\"}}"
        ));
    }
    out.push_str("\n  ]},\n");

    // 分区表：来自唯一事实来源。
    out.push_str("  \"flash_map\": {\"source\": \"crates/flash-layout\", \"regions\": [");
    for (index, region) in flash_layout::MAP.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // ⚠️ JSON **没有** `0x` 十六进制字面量：第一版直接 `{:#x}` 打进去，
        // 产物看起来正常、任何解析器都报错。十进制给机器，十六进制字符串给人。
        out.push_str(&format!(
            "\n    {{\"name\": \"{}\", \"offset\": {}, \"offset_hex\": \"{:#x}\", \"size\": {}, \"purpose\": \"{:?}\"}}",
            region.name, region.offset, region.offset, region.size, region.purpose
        ));
    }
    out.push_str("\n  ]},\n");

    // 当前映像与预算：现算。
    let image = flash_image_bytes(&firmware_elf()?)?;
    let unsafe_blocks = count_unsafe_blocks(&root)?;
    // 测试与覆盖率的**最近一次实际结果**（由 `test-host` / `coverage` 落盘）。
    // 为 `null` 表示"这次会话还没跑过"，而不是"没有测试"——这点必须写清楚，
    // 否则 Agent 会把 null 误读成 0。
    let host_tests = read_json_or_null(&root.join("tmp/host-tests.json"));
    let coverage_summary = read_json_or_null(&root.join("tmp/coverage-summary.json"));
    out.push_str(&format!(
        "  \"budgets\": {{\"image_bytes\": {image}, \"image_limit_bytes\": {MAX_FIRMWARE_BYTES}, \"unsafe_blocks\": {unsafe_blocks}}},\n"
    ));
    out.push_str(&format!(
        "  \"measured\": {{\"host_tests\": {host_tests}, \"coverage\": {coverage_summary}, \"note\": \"null 表示本次会话尚未运行对应命令，不是 0\"}},\n"
    ));

    // 真机基准：有基线文件就带出来（没有就为 null，表示"尚未采集"）。
    let baseline = root.join(BENCH_BASELINE);
    let bench = fs::read_to_string(&baseline).unwrap_or_else(|_| "null".to_string());
    out.push_str(&format!("  \"bench_baseline\": {},\n", bench.trim()));
    out.push_str("  \"exit_code_contract\": \"门禁命令 0 = 通过，非 0 = 失败；原因在最后一行 [xtask] 输出里。要一次看全部静态门禁的结果，用 `cargo xtask gates`（不因第一个失败而中断）\"\n");
    out.push_str("}\n");
    // 先自检再输出：宁可让 `context` 失败，也不要吐出一份"看起来正常"的坏 JSON。
    validate_json(&out)?;
    print!("{out}");
    Ok(())
}

/// 真机基准文件（提交进仓库，作为回归基线）。
const BENCH_BASELINE: &str = "bench/baseline.json";
/// 允许的回归幅度（百分比）：超过它 `xtask bench` 失败。
const BENCH_TOLERANCE_PERCENT: u32 = 25;

/// 一条基准指标。
struct Metric {
    /// 串口输出里用于取数的标签。
    label: &'static str,
    /// 从一行里抽出的数值。
    parse: fn(&str) -> Option<u64>,
    /// 越大越好（吞吐）还是越小越好（耗时）。
    higher_is_better: bool,
    /// **绝对容差**：变化量小于它就一律不算回归。
    ///
    /// 为什么光有百分比不够：毫秒级指标的抖动幅度很小，百分比却可以很大
    /// （挂载 3 ms → 5 ms 是 +67%），那会让门禁变成"随机报警器"。
    /// 判据是"百分比与绝对量**同时**超限才算回归"。
    min_absolute_delta: u64,
    /// **硬下限**：`Some(v)` 表示这条指标不是"相对基线"而是"绝对要求"
    /// （低于 `v` 直接失败，与基线无关）。
    ///
    /// 用它把文档里的阈值变成可执行的判据 —— 例如栈余量下限来自
    /// [`memory_layout::MIN_STACK_HEADROOM_PERCENT`]，不再只是文档里的数字。
    minimum: Option<u64>,
    /// **物理上限**：`Some(v)` 表示"大于 `v` 不是更好，而是测量方法可疑"。
    ///
    /// 2026-10-19 真机校准：flash 读吞吐测出 3,401,993 KiB/s（基线 48,701），
    /// `bench` 当成"改善 6885% 通过"—— 实际是缓存命中，不是器件带宽。
    /// 对这类指标，超过物理上限必须失败，而不是奖励。
    maximum: Option<u64>,
}

/// 采集的指标集合。**只放"能稳定复现"的量**：吞吐与耗时会随板子状态波动，
/// 因此留 25% 容差；容差之内不算回归，超出就说明有实质变化。
const METRICS: &[Metric] = &[
    Metric {
        label: "flash 读吞吐（KiB/s）",
        // 用「在行内找标记」而不是「行首精确匹配」：抓包常有粘行/前缀噪声，
        // 严格匹配会让门禁在数据其实存在时报"找不到"。
        parse: |line| {
            let rest = line.split("flash rd : ").nth(1)?;
            rest.split_whitespace().next()?.parse().ok()
        },
        higher_is_better: true,
        // 2000 KiB/s ≈ 基线（约 50 MiB/s）的 4%，小于它说明只是测量抖动。
        min_absolute_delta: 2000,
        minimum: None,
        maximum: Some(200_000),
    },
    Metric {
        label: "文件系统挂载（ms）",
        parse: |line| {
            let rest = line.split("filesystem : ").nth(1)?;
            rest.split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        },
        higher_is_better: false,
        min_absolute_delta: 20,
        minimum: None,
        maximum: None,
    },
    Metric {
        label: "SDRAM 全片自检（ms）",
        parse: |line| {
            let rest = line.split("sdram post : ").nth(1)?;
            rest.split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        },
        higher_is_better: false,
        min_absolute_delta: 200,
        minimum: None,
        maximum: None,
    },
    Metric {
        // 启动报告：`  stack      : peak N B / 48 KiB · headroom M B (P%)`
        label: "栈余量（%）",
        parse: |line| {
            let rest = line.split("headroom ").nth(1)?;
            let percent = rest.split('(').nth(1)?;
            percent.split('%').next()?.trim().parse().ok()
        },
        higher_is_better: true,
        min_absolute_delta: 0,
        // 硬下限：把"栈余量不得低于 30%"从文档变成真机门禁。
        minimum: Some(memory_layout::MIN_STACK_HEADROOM_PERCENT as u64),
        maximum: None,
    },
    Metric {
        // 固件心跳行：`beat=… rt=ok|VIOLATION`。
        //
        // ⚠️ 这一条补的是**我自己留下的空头承诺**：`report_beat` 的注释早就写着
        // "`cargo xtask bench` 会把这一列当作硬判据"，而 METRICS 里根本没有它
        // （审计点名："现在这句注释是空头承诺"）。注释说了不算，判据说了才算。
        //
        // 编码成 1/0 而不是直接用文本：`minimum: Some(1)` 就是"必须成立"，
        // 与基线无关（实时预算不该有"比上次慢一点也算过"的余地）。
        label: "实时预算成立（1=ok / 0=VIOLATION|none）",
        parse: |line| {
            // `rt=none`（一次都没测过）**必须**记 0：把"没在测"当成"合格"是这个项目
            // 最忌讳的失效方式，2026-10-12 真机上真的踩到过。
            if line.contains("rt=ok") {
                Some(1)
            } else if line.contains("rt=VIOLATION") || line.contains("rt=none") {
                Some(0)
            } else {
                None
            }
        },
        higher_is_better: true,
        min_absolute_delta: 0,
        minimum: Some(1),
        maximum: None,
    },
    Metric {
        // 固件心跳行：`… txdrop=N …`（因"队满且中断关着"而丢弃的控制台字节数）。
        //
        // 同样编码成 1/0：**只要丢过就是异常**（正常运行时队列不会满）。
        // 这一条把"关中断里输出超过队列容量"从"日志偶尔少几行"变成可判定的失败。
        label: "控制台无丢弃（1=txdrop 0 / 0=丢过）",
        parse: |line| {
            let rest = line.split("txdrop=").nth(1)?;
            let value: u32 = rest
                .split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse()
                .ok()?;
            Some(u64::from(u8::from(value == 0)))
        },
        higher_is_better: true,
        min_absolute_delta: 0,
        minimum: Some(1),
        maximum: None,
    },
];

/// 真机基准：`--record` 覆盖基线，默认与基线比对。
///
/// 需要台架（调试器 + 串口）。采集方式：复位板子并抓取启动输出，从**固件自报**的
/// 行里取数 —— 与门禁的"报事实"原则一致：基准来自固件打印的事实，不是主机侧估算。
fn bench() -> Result<(), String> {
    let record = std::env::args().any(|argument| argument == "--record");
    let serial_path =
        std::env::var("BASE_SERIAL").unwrap_or_else(|_| "/dev/pudu-debugger-16p-uart".to_string());
    // `--from-file <抓包>`：用**已有的抓包**跑解析与比对，不需要台架。
    // 没有它，"解析对不对"就只能等板子空闲时才能发现 —— 而解析一旦写错，
    // 基准门禁会给出错误结论（比没有门禁更糟）。抓包文件同样可用于回归留档。
    let from_file = std::env::args()
        .skip_while(|argument| argument != "--from-file")
        .nth(1);
    let capture = match from_file {
        Some(path) => {
            println!("[xtask] bench：从抓包文件 {path} 取数（不接触台架）");
            // 抓包里可能有被链路吃掉半个字节留下的非 UTF-8 序列（那正是要诊断的现象），
            // 因此**有损解码**而不是拒绝整个文件。
            let bytes = fs::read(&path).map_err(|error| format!("读抓包失败：{error}"))?;
            String::from_utf8_lossy(&bytes).to_string()
        }
        None => {
            // 只有真要连台架时才检查串口是否存在（`--from-file` 不需要台架）。
            if !Path::new(&serial_path).exists() {
                return Err(format!(
                    "串口 {serial_path} 不存在：`xtask bench` 需要台架。\
                     可用 BASE_SERIAL=<节点> 覆盖，或用 `--from-file <抓包>` 离线取数；\
                     先跑 `make check-bench` 确认台架就绪。"
                ));
            }
            println!("[xtask] bench：从 {serial_path} 采集（复位板子，抓 20 秒启动输出）…");
            capture_boot_output(&serial_path, 20)?
        }
    };
    let mut measured: Vec<(&'static str, u64)> = Vec::new();
    for metric in METRICS {
        let value = capture
            .lines()
            .find_map(|line| (metric.parse)(line.trim()))
            .ok_or_else(|| {
                format!(
                    "没有在启动输出里找到「{}」。\
                     确认固件版本包含该行（`cargo xtask check-image` 通过后再烧录）。",
                    metric.label
                )
            })?;
        println!("  {} = {value}", metric.label);
        measured.push((metric.label, value));
    }

    let baseline_path = workspace_root().join(BENCH_BASELINE);
    if record {
        let mut json = String::from("{\n");
        json.push_str(&format!("  \"recorded\": \"{}\",\n", today_iso8601()));
        if let Some(version) = read_version() {
            json.push_str(&format!("  \"version\": \"{version}\",\n"));
        }
        if let Some(commit) = git_commit() {
            json.push_str(&format!("  \"git_commit\": \"{commit}\",\n"));
        }
        for (index, (label, value)) in measured.iter().enumerate() {
            json.push_str(&format!(
                "  \"{label}\": {value}{}\n",
                if index + 1 == measured.len() { "" } else { "," }
            ));
        }
        json.push_str("}\n");
        if let Some(parent) = baseline_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("建目录失败：{error}"))?;
        }
        fs::write(&baseline_path, json).map_err(|error| format!("写基线失败：{error}"))?;
        println!("[xtask] bench：基线已写入 {}", baseline_path.display());
        return Ok(());
    }

    let baseline = fs::read_to_string(&baseline_path).map_err(|error| {
        format!(
            "读不到基线 {}（{error}）。先跑 `cargo xtask bench --record` 采集一次。",
            baseline_path.display()
        )
    })?;
    // 基线必须能回答"它是哪个版本测的"；跨版本比较只在"趋势"意义上有效。
    if let (Some(baseline_version), Some(current_version)) =
        (parse_baseline_string(&baseline, "version"), read_version())
    {
        if baseline_version != current_version {
            println!(
                "[xtask] bench：⚠ 基线版本 {baseline_version} ≠ 当前 {current_version}\
                 （跨版本比较仅供趋势参考；要作为发布判据请先 `--record`）"
            );
        }
    }
    let mut regressions = Vec::new();
    for (label, value) in &measured {
        let Some(metric) = METRICS.iter().find(|metric| metric.label == *label) else {
            return Err("指标表与实现不一致".to_string());
        };
        // 物理上限优先：超过它说明**测量方法**有问题，不是性能变好。
        if let Some(maximum) = metric.maximum {
            if *value > maximum {
                regressions.push(format!(
                    "{label}：实测 {value}，超过物理上限 {maximum} —— 测量方法可疑\
                     （很可能是缓存/预取命中，不是器件带宽）"
                ));
                continue;
            }
        }
        // 硬下限判定（与基线无关）。
        if let Some(minimum) = metric.minimum {
            if *value < minimum {
                regressions.push(format!(
                    "{label}：实测 {value}，低于硬下限 {minimum}（真机不达标，不是抖动问题）"
                ));
                continue;
            }
            println!("  {label}: 实测 {value}（硬下限 {minimum}）ok");
            continue;
        }
        let Some(previous) = parse_baseline_value(&baseline, label) else {
            continue;
        };
        if previous == 0 {
            continue;
        }
        let worse = if metric.higher_is_better {
            *value < previous
        } else {
            *value > previous
        };
        let delta_percent = (value.abs_diff(previous) * 100) / previous;
        let absolute_delta = value.abs_diff(previous);
        // 百分比与绝对量**同时**超限才算回归（见 `Metric::min_absolute_delta` 的说明）。
        let status = if worse
            && delta_percent > u64::from(BENCH_TOLERANCE_PERCENT)
            && absolute_delta > metric.min_absolute_delta
        {
            regressions.push(format!(
                "{label}：{previous} → {value}（退化 {delta_percent}%，绝对量 {absolute_delta}，\
                 容差 {BENCH_TOLERANCE_PERCENT}% 且绝对量须超 {}）",
                metric.min_absolute_delta
            ));
            "回归"
        } else {
            "ok"
        };
        println!(
            "  {label}: 基线 {previous} / 实测 {value}（差 {delta_percent}%，绝对量 {absolute_delta}）{status}"
        );
    }
    if regressions.is_empty() {
        println!("[xtask] bench 通过：所有指标在 {BENCH_TOLERANCE_PERCENT}% 容差内");
        return Ok(());
    }
    Err(format!(
        "基准回归：\n  {}\n若这是有意为之（例如换了算法），用 `cargo xtask bench --record` 更新基线并在提交信息里说明原因。",
        regressions.join("\n  ")
    ))
}

/// JSON **结构冒烟检查**：括号/引号配平、无 `,,`、无 `{,`/`,}`。
///
/// # 为什么 `context` 必须自检
///
/// `context` 的全部价值就是"机器可读"。第一版手工拼 JSON 时漏了一个逗号，
/// 输出**看起来正常**、解析器却直接报错 —— 而没有任何东西会失败。
/// 这里不引入 JSON 依赖（离线环境装不了），只做结构检查：它抓不住所有语法错误，
/// 但能抓住"手工拼 JSON"最容易犯的那几类。
fn validate_json(text: &str) -> Result<(), String> {
    // 首选**真正的 JSON 解析**（python3 在本机是台架依赖的一部分）。它抓得住
    // 结构检查抓不到的错：十六进制字面量、缺逗号、非法转义 —— 这三种我都犯过。
    let parsed = std::process::Command::new("python3")
        .args(["-c", "import json,sys; json.load(sys.stdin)"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait_with_output()
        });
    match parsed {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err("python3 解析失败：输出不是合法 JSON（见下）".to_string()),
        // 没有 python3 时退回结构检查（弱一些，但比不检查强）。
        Err(_) => validate_json_smoke(text),
    }
}

fn validate_json_smoke(text: &str) -> Result<(), String> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err("括号比开括号多".to_string());
                }
            }
            _ => {}
        }
    }
    if in_string {
        return Err("字符串没有闭合".to_string());
    }
    if depth != 0 {
        return Err(format!("括号未配平（结尾还差 {depth} 个）"));
    }
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    for bad in [",,", "{,", ",}", "[,", ",]"] {
        if compact.contains(bad) {
            return Err(format!("出现非法片段 `{bad}`（多半是拼接时漏/多了逗号）"));
        }
    }
    Ok(())
}

/// 读一个 JSON 文件原样返回；不存在则返回 `null`（用于把"没跑过"与"结果是 0"区分开）。
fn read_json_or_null(path: &Path) -> String {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "null".to_string())
}

/// 从基线 JSON 里取某个标签的值（极简解析：不引入 JSON 依赖）。
fn parse_baseline_value(baseline: &str, label: &str) -> Option<u64> {
    let needle = format!("\"{label}\":");
    let rest = baseline.split(&needle).nth(1)?;
    let value = rest
        .trim_start()
        .split(|ch: char| !ch.is_ascii_digit())
        .next()?;
    value.parse().ok()
}

/// 从基线 JSON 里取一个字符串字段（同样不引入 JSON 依赖）。
fn parse_baseline_string(baseline: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let rest = baseline.split(&needle).nth(1)?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest.split('"').next()?.to_string())
}

/// 读 `VERSION`（版本号唯一真源）。
fn read_version() -> Option<String> {
    fs::read_to_string(workspace_root().join("VERSION"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// 当前 `git` 提交短哈希（只用于基准留档；不是工作区时返回 `None`）。
fn git_commit() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(workspace_root())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 日期（ISO-8601 的日期部分）。只用于基准留档，不参与判定。
fn today_iso8601() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 找出**带 pyserial 的 python**。
///
/// 与 `scripts/tool-paths.sh` 的 `resolve_pyocd_python` 同一策略：pyocd 与 pyserial
/// 装在同一 venv 里，所以取 pyocd 旁边那个解释器；系统 `python3` 往往没有 pyserial
/// （真机上就踩过：`ModuleNotFoundError: No module named 'serial'`）。
#[must_use]
fn resolve_python() -> String {
    if let Ok(explicit) = std::env::var("BASE_PYTHON") {
        if Path::new(&explicit).exists() {
            return explicit;
        }
    }
    if let Some(pyocd) = resolve_pyocd() {
        if let Some(dir) = Path::new(&pyocd).parent() {
            let sibling = dir.join("python");
            if sibling.exists() {
                return sibling.display().to_string();
            }
        }
    }
    "python3".to_string()
}

/// 找出 pyocd 可执行文件（顺序与 `scripts/tool-paths.sh` 一致）。
#[must_use]
fn resolve_pyocd() -> Option<String> {
    if let Ok(explicit) = std::env::var("PYOCD") {
        if Path::new(&explicit).exists() {
            return Some(explicit);
        }
    }
    for candidate in [
        "./.venv/bin/pyocd",
        // 相邻项目的 .venv（台架工具共享位置；可用 PYOCD 环境变量覆盖）。
        "/home/reginald/project/elevator-protocol-gateway/.venv/bin/pyocd",
    ] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// 复位板子并抓一段启动输出（**在复位之前**就开始读，否则会漏掉最早的行）。
fn capture_boot_output(serial_path: &str, seconds: u64) -> Result<String, String> {
    // 依赖 python3 + pyserial（`make setup` 已装）。用脚本而不是内嵌 Rust 串口库：
    // 主机构建不该为一个基准门禁引入新的 crate 依赖。
    // pyocd 与解释器都在 Rust 侧解析好再传进去：脚本里写死 `python3`/`pyocd`
    // 会在"系统 python 没有 pyserial"或"pyocd 不在 PATH"时失败。
    let pyocd = resolve_pyocd().unwrap_or_else(|| "pyocd".to_string());
    let script = format!(
        "import serial,time,subprocess,threading\n\
         p=serial.Serial('{serial_path}',115200,timeout=0.2)\n\
         p.reset_input_buffer()\n\
         threading.Thread(target=lambda: subprocess.run(['{pyocd}','reset','--target','mimxrt1020'],capture_output=True),daemon=True).start()\n\
         raw=b''\n\
         start=time.time(); end=start+{seconds}; sent=False\n\
         try:\n\
         \x20   while time.time()<end:\n\
         \x20       if not sent and time.time()-start>6:\n\
         \x20           p.write(b'beat on\\n'); sent=True\n\
         \x20       raw+=p.read(65536)\n\
         finally:\n\
         \x20   # 抓包结束必须关掉心跳：否则板子会一直每秒打印，\n\
         \x20   # 用户看到的就是“默认关闭却一直在打”。\n\
         \x20   if sent:\n\
         \x20       p.write(b'beat off\\n')\n\
         \x20       time.sleep(0.2)\n\
         print(raw.decode('utf-8','replace'))\n"
    );
    let output = std::process::Command::new(resolve_python())
        .args(["-c", &script])
        .output()
        .map_err(|error| format!("执行 python3 失败：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "抓包失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 统计 `crates/` 里的 unsafe 形态总数（与 `check-unsafe` **同口径**，供 `context` 输出）。
///
/// 口径必须一致，否则 Agent 会从 `context` 读到与门禁不同的数字：
/// - 只扫 `crates/`（预算与 `check-unsafe` 都只覆盖成员 crate，不含 xtask）；
/// - 计 `unsafe {` 块 **与** `unsafe fn`（两者都占预算）；
/// - 跳过文档注释（文档里的示例文本不是代码）。
fn count_unsafe_blocks(root: &Path) -> Result<u64, String> {
    let crates_dir = root.join("crates");
    if !crates_dir.exists() {
        return Ok(0);
    }
    count_unsafe_in(&crates_dir)
}

/// 递归统计目录下的 unsafe 形态数（块 + `unsafe fn`）。
fn count_unsafe_in(dir: &Path) -> Result<u64, String> {
    let mut count = 0u64;
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            count += count_unsafe_in(&path)?;
            continue;
        }
        if path.extension().map(|ext| ext != "rs").unwrap_or(true) {
            continue;
        }
        let text = read(&path)?;
        let (blocks, unsafe_fns, _, _) = count_unsafe(&text);
        count += u64::try_from(blocks + unsafe_fns).unwrap_or(u64::MAX);
    }
    Ok(count)
}

/// 主机侧代码覆盖率（纯逻辑层）。
///
/// # 为什么只对纯逻辑层测覆盖率
///
/// 覆盖率只对"能在主机上跑的东西"有意义。本项目把硬件相关代码全部压在
/// `board-support` / `firmware`（它们依赖 ARM 外设，主机上根本链接不了），
/// 其余 10 个 crate 都是纯逻辑 —— **覆盖率统计的正是这 10 个**。
/// 硬件侧的正确性由另一套机制承担：产物级断言（`check-image` / `check-flash-map`）、
/// 真机自报事实（启动横幅回读寄存器）、以及受限的 HIL 台架。
///
/// # 为什么不用 cargo-llvm-cov
///
/// 离线环境装不了它。但 `llvm-tools` 组件（`llvm-cov` / `llvm-profdata`）已在工具链里，
/// 而 `-C instrument-coverage` 是稳定选项 —— 因此这里直接用工具链自带的工具，
/// **不引入任何新依赖**。
fn coverage() -> Result<(), String> {
    let sysroot = String::from_utf8(
        std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .map_err(|error| format!("执行 rustc 失败：{error}"))?
            .stdout,
    )
    .map_err(|error| format!("sysroot 不是合法 UTF-8：{error}"))?
    .trim()
    .to_string();
    let llvm_bin = PathBuf::from(&sysroot).join("lib/rustlib/x86_64-unknown-linux-gnu/bin");
    let llvm_profdata = llvm_bin.join("llvm-profdata");
    let llvm_cov = llvm_bin.join("llvm-cov");
    if !llvm_profdata.exists() || !llvm_cov.exists() {
        return Err("工具链缺少 llvm-tools（llvm-profdata/llvm-cov）。\
             安装：rustup component add llvm-tools-preview"
            .to_string());
    }

    // 独立的 target 目录：插桩构建会污染增量缓存，不该与常规构建混用。
    let target_dir = workspace_root().join("target/coverage");
    let profile_dir = workspace_root().join("tmp/coverage");
    let _ = fs::remove_dir_all(&profile_dir);
    fs::create_dir_all(&profile_dir)
        .map_err(|error| format!("建 {profile_dir:?} 失败：{error}"))?;

    // 1) 插桩构建（只构建测试，不运行）：拿测试可执行文件的路径。
    let packages = pure_package_args();
    let mut args = vec![
        "test",
        "--offline",
        "--no-run",
        "--message-format=json",
        "--target",
        "x86_64-unknown-linux-gnu",
    ];
    args.extend(packages.iter().map(String::as_str));
    let output = std::process::Command::new("cargo")
        .args(&args)
        .env("RUSTFLAGS", "-C instrument-coverage")
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(workspace_root())
        .output()
        .map_err(|error| format!("执行 cargo test --no-run 失败：{error}"))?;
    if !output.status.success() {
        // ⚠️ 这里**必须**把 cargo 的输出带出来。旧实现只返回一句"插桩构建失败（见上方
        // cargo 输出）"—— 而 `Command::output()` 根本不会把子进程输出透到终端，
        // 于是失败时**看不到任何原因**（实测踩到过：覆盖率命令瞬时失败，只能靠"过几分钟
        // 再跑一次"猜是并发中间态）。门禁的失败信息必须自解释，否则它只是在报告"有东西坏了"。
        //
        // 只打末尾若干行：cargo 的插桩构建输出很长，而根因通常在最后的 `error[...]` 段。
        let mut detail = String::new();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        for (label, text) in [("stderr", stderr.as_ref()), ("stdout", stdout.as_ref())] {
            let tail: Vec<&str> = text.lines().rev().take(40).collect();
            if tail.is_empty() {
                continue;
            }
            detail.push_str(&format!("\n--- cargo {label}（末尾 40 行）---\n"));
            for line in tail.into_iter().rev() {
                detail.push_str(line);
                detail.push('\n');
            }
        }
        return Err(format!(
            "插桩构建失败（退出码 {:?}）：{detail}",
            output.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 从 JSON 流里挑出可执行文件路径。这里只做字符串扫描而不引入 JSON 依赖：
    // cargo 的 message-format=json 每行一个对象，`"executable":"<path>"` 形式稳定。
    let mut binaries: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.split("\"executable\":\"").nth(1) {
            if let Some(path) = rest.split('"').next() {
                if !path.is_empty() && !binaries.iter().any(|kept| kept == path) {
                    binaries.push(path.to_string());
                }
            }
        }
    }
    if binaries.is_empty() {
        return Err("没有解析出任何测试可执行文件".to_string());
    }

    // 2) 运行测试并收集 .profraw。
    let mut args = vec!["test", "--offline", "--target", "x86_64-unknown-linux-gnu"];
    args.extend(packages.iter().map(String::as_str));
    let status = std::process::Command::new("cargo")
        .args(&args)
        .env("RUSTFLAGS", "-C instrument-coverage")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("LLVM_PROFILE_FILE", profile_dir.join("cov-%p-%m.profraw"))
        .current_dir(workspace_root())
        .status()
        .map_err(|error| format!("运行测试失败：{error}"))?;
    if !status.success() {
        return Err("测试未全部通过；覆盖率报告没有意义".to_string());
    }

    // 3) 合并 profile 并出报告。
    let profdata = profile_dir.join("merged.profdata");
    let status = std::process::Command::new(&llvm_profdata)
        .arg("merge")
        .arg("-sparse")
        .arg(format!("-o={}", profdata.display()))
        .arg(&profile_dir)
        .status()
        .map_err(|error| format!("执行 llvm-profdata 失败：{error}"))?;
    if !status.success() {
        return Err("llvm-profdata merge 失败".to_string());
    }

    let mut args = vec![
        "report".to_string(),
        format!("--instr-profile={}", profdata.display()),
        // 排除：外部仓库与构建目录；以及**度量工具**本身
        // （`tests_readcost.rs` 是 `#[ignore]` 的取证脚本，跑它才会被覆盖，
        //  把它算进产品覆盖率只会让阈值失去意义 —— 它测的是别人花多少时间）。
        "--ignore-filename-regex=(/\\.cargo/registry|/rustc/|/target/|tests_readcost\\.rs)"
            .to_string(),
    ];
    for binary in &binaries {
        args.push(format!("--object={binary}"));
    }
    println!("\n== 覆盖率（主机可测的纯逻辑层）==");
    let output = std::process::Command::new(&llvm_cov)
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .map_err(|error| format!("执行 llvm-cov 失败：{error}"))?;
    if !output.status.success() {
        return Err("llvm-cov report 失败".to_string());
    }
    let report = String::from_utf8_lossy(&output.stdout);
    print!("{report}");

    // 阈值门禁：把"覆盖率高"变成**可执行的约束**，而不是一句口号。
    // 阈值定在 90%（实测 94%），留出余量以免正常重构就失败 —— 它拦的是"整块没测"
    // 这种回归，不是"少测了两行"。
    const MIN_LINE_COVERAGE_PERCENT: f64 = 90.0;
    let total = report
        .lines()
        .find(|line| line.starts_with("TOTAL"))
        .ok_or("覆盖率报告里没有 TOTAL 行")?;
    // TOTAL 行的列：Region 覆盖/未覆盖/%、Function 覆盖/未覆盖/%、Line 覆盖/未覆盖/%
    let percent = total
        .split_whitespace()
        .filter_map(|token| token.strip_suffix('%'))
        .filter_map(|value| value.parse::<f64>().ok())
        .nth(2) // 第 3 个百分比 = 行覆盖率
        .ok_or("无法从 TOTAL 行解析行覆盖率")?;
    println!(
        "\n[xtask] coverage：行覆盖率 {percent:.2}%（阈值 {MIN_LINE_COVERAGE_PERCENT:.0}%）；\
         原始 profile 在 {}（已在 .gitignore 内）",
        profile_dir.display()
    );
    if percent < MIN_LINE_COVERAGE_PERCENT {
        return Err(format!(
            "行覆盖率 {percent:.2}% 低于阈值 {MIN_LINE_COVERAGE_PERCENT:.0}%。\n\
             注意：覆盖率只统计**能在主机上运行**的纯逻辑 crate；新增硬件相关逻辑应放进 \
             BSP 并用产物级断言/真机自报事实来保证，而不是拉低这里的数字。"
        ));
    }
    let summary_path = workspace_root().join("tmp/coverage-summary.json");
    if let Some(parent) = summary_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        &summary_path,
        format!("{{\"line_percent\": {percent:.2}, \"threshold_percent\": {MIN_LINE_COVERAGE_PERCENT:.0}}}\n"),
    );
    println!(
        "注：覆盖率只统计**主机可测**的纯逻辑 crate（名单见 `cargo xtask pure-crates`）；BSP/固件的正确性由产物级断言 \
         （check-image / check-flash-map）与真机自报事实承担。"
    );
    Ok(())
}

/// 硬件相关的 crate 名（出现即视为分层破坏）。
/// 纯逻辑 crate 里**不得出现**的依赖。
///
/// 前缀匹配（`imxrt-` / `cortex-m` / `rtic-`）而不是逐个列名字：2026-10-12 审计发现
/// 旧名单只有 7 个名字，漏了 `imxrt-iomuxc`、`imxrt-dma`（**已经在 lockfile 里**）、
/// `ral-registers`、`volatile-register`、`vcell`、`bare-metal`、`embedded-hal`
/// —— 逐个列名字的名单天生会漏，而前缀不会。
const FORBIDDEN_PREFIXES: &[&str] = &[
    "imxrt-",
    "cortex-m",
    "rtic",
    "ral-registers",
    "volatile-register",
    "vcell",
    "bare-metal",
    "embedded-hal",
];

/// 精确匹配的禁用项（不与上面前缀重叠的独立库）。
const FORBIDDEN_EXACT: &[&str] = &["imxrt", "cortex_m", "rtic"];

/// 某个依赖名是否属于"硬件相关"。
fn is_forbidden_dependency(name: &str) -> bool {
    FORBIDDEN_EXACT.contains(&name)
        || FORBIDDEN_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// 固件镜像体积上限（字节）：1 MiB。
const MAX_FIRMWARE_BYTES: u64 = 1024 * 1024;

/// 每个纯逻辑 crate 的源码文件必须包含这些标记（标准 lint 头 + 禁止 unsafe）。
const REQUIRED_HEADER_MARKERS: &[&str] = &[
    "forbid(unsafe_code)",
    "clippy::unwrap_used",
    "clippy::indexing_slicing",
];

/// ELF 中 Flash 的映射基址（`imxrt-rt` 为 Imxrt1020 生成）。
const FLASH_BASE: u64 = 0x6000_0000;
/// FCB 在 Flash 内的偏移。
const FCB_OFFSET: u64 = 0x0000;
/// IVT 在 Flash 内的偏移。
const IVT_OFFSET: u64 = 0x1000;
/// FCB 中 `sflashA1Size`（A1 区容量）的偏移。
///
/// 由 `imxrt-boot-gen` 的 `ConfigurationBlock` 字段布局确定：
/// `_reserved5: [u8; 8]` 起始于 `0x048`，紧接着的 `serial_flash_sizes: [u32; 4]`
/// 因此从 `0x050` 开始，A1 是它的第 0 项。
/// ⚠️ 这个偏移是**版式事实**：`imxrt-boot-gen` 升版若改了字段顺序，本门禁会失败 ——
/// 那正是需要有人去核对 FCB 版式的时刻。
const FCB_FLASH_SIZE_A1_OFFSET: u64 = 0x50;

/// FCB 里查找表（LUT）的偏移。取自 NXP `flexspi_nor_config_t` 的字段注释：
/// `uint32_t lookupTable[64]; //!< [0x080-0x17f]`。
const FCB_LUT_OFFSET: u64 = 0x080;

/// FCB 魔数（`"FCFB"` 小端 = `0x42464346`）。
const FCB_TAG: u32 = 0x4246_4346;
/// IVT 头魔数。
const IVT_MAGIC: u32 = 0x4020_00D1;
/// DTCM 基址（向量表初始栈指针必须落在 RAM 里）。
const DTCM_BASE: u64 = 0x2000_0000;
const DTCM_END: u64 = 0x2001_0000;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");
    let result = match command {
        "check-layering" => check_layering(),
        "check-capsules" => check_capsules(),
        "check-interrupt-priorities" => check_interrupt_priorities(),
        "check-lint-headers" => check_lint_headers(),
        "check-unsafe" => check_unsafe(),
        "check-gaps" => check_gaps(),
        "check-image" => check_image(),
        "check-size" => check_size(),
        "check-flash-map" => check_flash_map(),
        "test-host" => test_host(),
        "lint-host" => lint_host(),
        "lint-target" => lint_target(),
        "pure-crates" => pure_crates(),
        "coverage" => coverage(),
        "check-fault-handlers" => check_fault_handlers(),
        "check-critical" => check_critical(),
        "check-api-usage" => check_api_usage(),
        "check-style" => check_style(),
        "check-docs" => check_docs(),
        "check-memory" => check_memory(),
        "check-code-placement" => check_code_placement(),
        "gates" => gates(),
        "context" => context(),
        "bench" => bench(),
        "tock-build" => tock_build(),
        "tock-flash" => tock_flash(),
        "tock-verify" => tock_verify(),
        "tock-sync-boot-header" => tock_sync_boot_header(),
        "check-compile-fail" => check_compile_fail(),
        "trace" => trace(),
        "verify" => verify(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!(
            "未知子命令：{other}（用 `cargo xtask help` 看列表）"
        )),
    };

    match result {
        Ok(()) => {}
        Err(message) => {
            eprintln!("\n[xtask] 门禁失败：{message}");
            std::process::exit(1);
        }
    }
}

/// **命令清单的唯一来源**：`help`、`gates`、`context` 三个消费者都读它。
///
/// 2026-10-10 的教训：此前 `help` 手写一份、`gates` 手写一份、`context` 又手写一份，
/// 于是新增的 8 个子命令（含 `check-flash-map`）**只在源码里存在、在 `help` 里查不到**。
/// 接手者（尤其是 Agent）判断"有什么可用"的第一入口就是 `cargo xtask`，它不能漏。
struct Subcommand {
    /// 子命令名。
    name: &'static str,
    /// 一句话说明。
    what: &'static str,
    /// 是否属于 `gates` 汇总的静态门禁，以及对应哪个检查函数。
    check: Option<fn() -> Result<(), String>>,
}

/// 全部子命令。
const COMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "check-layering",
        what: "纯逻辑 crate 的依赖闭包里不含硬件 crate",
        check: Some(check_layering),
    },
    Subcommand {
        name: "check-capsules",
        what: "capsule 插件只依赖 sdk/端口层，不碰硬件/组合根",
        check: Some(check_capsules),
    },
    Subcommand {
        name: "check-interrupt-priorities",
        what: "中断任务优先级与预算表一致（RTIC 字面量无法被编译器绑定）",
        check: Some(check_interrupt_priorities),
    },
    Subcommand {
        name: "check-lint-headers",
        what: "每个 crate 带标准 lint 头（含 forbid(unsafe_code)）",
        check: Some(check_lint_headers),
    },
    Subcommand {
        name: "check-unsafe",
        what: "unsafe 块与 // SAFETY: 配平，且不超预算",
        check: Some(check_unsafe),
    },
    Subcommand {
        name: "check-gaps",
        what: "清点 GAP(Px) 未实现项（刻意不失败）",
        check: Some(check_gaps),
    },
    Subcommand {
        name: "check-fault-handlers",
        what: "故障处理器：四个故障向量必须各有真实实现（空桩会被抓）",
        check: Some(check_fault_handlers),
    },
    Subcommand {
        name: "check-critical",
        what: "关中断只能走测量入口（否则没人知道这个窗口有多长）",
        check: Some(check_critical),
    },
    Subcommand {
        name: "check-api-usage",
        what: "列出没有任何调用者的公开项（刻意不失败，但必须可见）",
        check: Some(check_api_usage),
    },
    Subcommand {
        name: "check-style",
        what: "风格门禁：无 todo!/unimplemented!、每 crate 有模块文档、lint 未放宽",
        check: Some(check_style),
    },
    Subcommand {
        name: "check-docs",
        what: "文档里的关键数字与代码/产物一致（防止文档漂移）",
        check: Some(check_docs),
    },
    Subcommand {
        name: "check-memory",
        what: "内存放置：.text 在 ITCM 且不超预算、.stack/.vector_table 在 DTCM",
        check: Some(check_memory),
    },
    Subcommand {
        name: "check-code-placement",
        what: "代码放置：.xip 在 Flash 内，且擦写期间必须能跑的模块不在 .xip",
        check: Some(check_code_placement),
    },
    Subcommand {
        name: "check-image",
        what: "对链接后的 ELF 断言 FCB/IVT/向量表布局",
        check: Some(check_image),
    },
    Subcommand {
        name: "check-size",
        what: "固件体积 < 1 MiB",
        check: Some(check_size),
    },
    Subcommand {
        name: "check-flash-map",
        what: "分区表自洽 + 文档表格与代码一致",
        check: Some(check_flash_map),
    },
    Subcommand {
        name: "check-compile-fail",
        what: "编译失败用例确实编译失败（类型约束是硬的）",
        check: Some(check_compile_fail),
    },
    Subcommand {
        name: "trace",
        what: "生成需求追溯矩阵并检查覆盖",
        check: Some(trace),
    },
    Subcommand {
        name: "verify",
        what: "以上全部 + fmt + clippy + 主机测试 + 目标构建",
        check: None,
    },
    Subcommand {
        name: "help",
        what: "列出全部子命令（与不带参数相同）",
        check: None,
    },
    Subcommand {
        name: "gates",
        what: "跑完全部静态门禁并给出汇总表（不因第一个失败而中断）",
        check: None,
    },
    Subcommand {
        name: "test-host",
        what: "主机测试（纯逻辑层全部 crate）",
        check: None,
    },
    Subcommand {
        name: "lint-host",
        what: "主机 lint",
        check: None,
    },
    Subcommand {
        name: "lint-target",
        what: "固件目标 lint",
        check: None,
    },
    Subcommand {
        name: "pure-crates",
        what: "打印纯逻辑 crate 列表（一行一个）",
        check: None,
    },
    Subcommand {
        name: "context",
        what: "机器可读的项目上下文（JSON）",
        check: None,
    },
    Subcommand {
        name: "coverage",
        what: "主机侧行覆盖率（阈值 90%）",
        check: None,
    },
    Subcommand {
        name: "bench",
        what: "真机基准与基线回归（--record 采集，--from-file 离线取数）",
        check: None,
    },
    Subcommand {
        name: "tock-build",
        what: "构建 runtime/tock 下的最小 Tock 镜像（实验性，不参与生产固件）",
        check: None,
    },
    Subcommand {
        name: "tock-flash",
        what: "构建并全片擦除烧录 Tock 镜像（需要台架）",
        check: None,
    },
    Subcommand {
        name: "tock-verify",
        what: "构建→烧录→串口验证 Tock 镜像（启动横幅 + 行回显）",
        check: None,
    },
    Subcommand {
        name: "tock-sync-boot-header",
        what: "从生产固件 ELF 重新生成 Tock 板级 crate 的 8 KiB 启动头（FCB+IVT）",
        check: None,
    },
];

fn print_help() {
    println!("cargo xtask <子命令>");
    println!();
    for command in COMMANDS {
        println!("  {:<20} {}", command.name, command.what);
    }
    println!();
    println!("  （`cargo xtask context` 输出同一份清单的 JSON，供脚本/Agent 取用）");
}

fn workspace_root() -> PathBuf {
    // xtask 位于 <root>/xtask，因此上一级就是工作区根。
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().map(Path::to_path_buf).unwrap_or(manifest)
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("读取 {} 失败：{error}", path.display()))
}

// ---------------------------------------------------------------- 工具链

// ---------------------------------------------------------------- 分层

/// 从 Cargo.toml 中抽出依赖的 crate 名（够用即可，不引入 cargo_metadata）。
fn declared_dependencies(manifest: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // ⚠️ 这里有两个**真实存在过的绕过点**（2026-10-12 审计发现）：
            //
            // 1. `imxrt-hal.workspace = true` —— 依赖名后面跟 `.键`。旧实现用
            //    `split_once('=')` 取左边，于是名字变成 `"imxrt-hal.workspace"`，
            //    与禁用名单做**精确匹配**时**不命中**，直接绕过整道门禁。
            // 2. `[dependencies.imxrt-hal]` —— 表头写法。旧实现只认 `[dependencies]`
            //    那一行，这种写法整段都被跳过。
            //
            // 因此表头解析必须同时处理 `[dependencies]`、`[dependencies.名字]`、
            // `[dev-dependencies.名字]` 等所有形式。
            let section = trimmed.trim_start_matches('[').trim_end_matches(']');
            let mut parts = section.split('.');
            let kind = parts.next().unwrap_or_default();
            in_dependencies = kind.ends_with("dependencies");
            if in_dependencies {
                if let Some(name) = parts.next() {
                    // `[dependencies.名字]`：名字就在这里。
                    let name = name.trim();
                    if !name.is_empty() {
                        names.insert(name.to_string());
                    }
                }
            }
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((left, _)) = trimmed.split_once('=') {
            // `名字 = ...` 或 `名字.键 = ...`：取第一段点号之前的部分。
            let name = left.split('.').next().unwrap_or_default().trim();
            if !name.is_empty() {
                names.insert(name.to_string());
            }
        }
    }
    names
}

/// [`declared_dependencies`] 的**自检**。
///
/// 为什么要自带夹具：这道门禁曾经被 `imxrt-hal.workspace = true` 绕过（精确匹配
/// 拿着 `"imxrt-hal.workspace"` 去比 `"imxrt-hal"`），而**没有任何测试发现**。
/// 门禁的判据本身也必须被测 —— 否则它只是"看起来很严格"。
fn self_test_dependency_parser() -> Result<(), String> {
    let fixtures: &[(&str, &[&str])] = &[
        // 普通写法
        ("[dependencies]\nimxrt-hal = \"0.6\"\n", &["imxrt-hal"]),
        // ⚠️ 曾经的绕过点 1：工作区继承
        (
            "[dependencies]\nimxrt-hal.workspace = true\n",
            &["imxrt-hal"],
        ),
        // ⚠️ 曾经的绕过点 2：表头写法
        (
            "[dependencies.imxrt-hal]\nversion = \"0.6\"\n",
            &["imxrt-hal"],
        ),
        // dev/build 依赖同样算数
        (
            "[dev-dependencies]\ncortex-m = \"0.7\"\n[build-dependencies]\nrtic = \"2\"\n",
            &["cortex-m", "rtic"],
        ),
        // 与分层无关的字段不应被误当依赖名
        (
            "[package]\nname = \"x\"\n[dependencies]\nheapless = \"0.9\"\n",
            &["heapless"],
        ),
    ];
    for (manifest, expected) in fixtures {
        let parsed = declared_dependencies(manifest);
        for name in *expected {
            if !parsed.contains(*name) {
                return Err(format!(
                    "依赖解析自检失败：清单 `{manifest:?}` 里应当解析出 `{name}`，实际得到 {parsed:?}"
                ));
            }
        }
    }
    // 反向：`[package]` 段的字段名不能被当成依赖。
    let parsed = declared_dependencies("[package]\nname = \"x\"\nversion = \"1\"\n");
    if !parsed.is_empty() {
        return Err(format!(
            "依赖解析自检失败：`[package]` 段不应解析出依赖，实际得到 {parsed:?}"
        ));
    }
    Ok(())
}

fn check_layering() -> Result<(), String> {
    // 0) 先自检判据本身：这道门禁**曾经被 `imxrt-hal.workspace = true` 绕过**
    //    （解析出 `"imxrt-hal.workspace"`，与名单做精确匹配时不命中）。没有负例/自检的
    //    门禁等于没有门禁，所以解析器必须每次被验证。
    self_test_dependency_parser()?;

    let root = workspace_root();
    let crates_dir = root.join("crates");

    // 1) 收集工作区内全部 crate 的名字 → 声明依赖（用于算**传递闭包**）。
    //
    // 旧实现只看**直接依赖**，而 AGENTS.md 一直宣称的是"依赖闭包" —— 于是
    // `pure → 内部 crate → imxrt-hal` 这种中间人链路可以整条绕过去。
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut manifests: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in fs::read_dir(&crates_dir).map_err(|error| error.to_string())? {
        let dir = entry.map_err(|error| error.to_string())?.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("Cargo.toml");
        if !manifest_path.exists() {
            continue;
        }
        let manifest = read(&manifest_path)?;
        let name = manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("name = "))
            .map(|value| value.trim().trim_matches('"').to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        graph.insert(name.clone(), declared_dependencies(&manifest));
        manifests.insert(name, manifest_path);
    }

    // 2) 求某个 crate 的**传递闭包**（只跟随工作区内部的边；外部依赖名直接进集合）。
    let closure_of = |start: &str| -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<String> = vec![start.to_string()];
        while let Some(current) = queue.pop() {
            let Some(deps) = graph.get(&current) else {
                continue;
            };
            for dep in deps {
                if !seen.insert(dep.clone()) {
                    continue;
                }
                // 只有工作区内部的 crate 才有下一层。
                if graph.contains_key(dep) {
                    queue.push(dep.clone());
                }
            }
        }
        seen
    };

    // 3) 逐个检查**声明的**纯逻辑 crate：闭包里不得出现硬件相关依赖。
    let mut violations = Vec::new();
    for crate_name in PURE_CRATES {
        let path = manifests
            .get(*crate_name)
            .cloned()
            .unwrap_or_else(|| crates_dir.join(crate_name).join("Cargo.toml"));
        for forbidden in closure_of(crate_name)
            .iter()
            .filter(|name| is_forbidden_dependency(name))
            .cloned()
            .collect::<Vec<_>>()
        {
            violations.push(format!(
                "  {crate_name} 的依赖闭包里有 {forbidden}（{}）",
                path.display()
            ));
        }
    }

    // 4) **双向比对**：实际算出来的纯净集必须与声明清单一致。
    //
    //    这一条防的是"判据被悄悄放宽"与"新增纯逻辑 crate 忘了登记"：
    //    - 声明为纯、实际不纯 → 上面第 3 步已经报错；
    //    - 实际纯净、却没登记 → 这里报错（因为它不会被主机测试/lint 覆盖到）。
    let derived: BTreeSet<String> = graph
        .keys()
        .filter(|name| {
            !closure_of(name)
                .iter()
                .any(|dep| is_forbidden_dependency(dep))
        })
        .cloned()
        .collect();
    let declared: BTreeSet<String> = PURE_CRATES.iter().map(|name| (*name).to_string()).collect();
    for name in derived.difference(&declared) {
        violations.push(format!(
            "  {name} 的依赖闭包是纯净的，但**没有**登记进 `PURE_CRATES` \
             —— 它不会被主机测试与 lint 覆盖"
        ));
    }

    if !violations.is_empty() {
        return Err(format!(
            "分层被破坏：纯逻辑层不得依赖硬件 crate
{}
\
             理由：这层必须能在主机上 `cargo test`，且\"业务逻辑不认识硬件\"要是编译期事实。\n\
             注：判据是**依赖闭包**（含 dev/build 依赖与工作区内部链路），不是直接依赖。",
            violations.join("\n")
        ));
    }
    println!(
        "[xtask] check-layering 通过：{} 个纯逻辑 crate 的**依赖闭包**里没有硬件 crate \
         （含传递闭包与工作区中间人；判据自身带自检）",
        PURE_CRATES.len()
    );
    Ok(())
}

// ---------------------------------------------------------------- lint 头

fn check_capsules() -> Result<(), String> {
    let root = workspace_root();
    if !PURE_CRATES.contains(&"sdk") {
        return Err("sdk 必须是纯逻辑 crate（PURE_CRATES）".to_string());
    }
    let sdk_source = read(&root.join("crates/sdk/src/lib.rs"))?;
    for forbidden in ["board_support", "board-support", "rtic", "imxrt"] {
        if sdk_source.contains(forbidden) {
            return Err(format!("sdk 门面出现硬件依赖：{forbidden}"));
        }
    }
    let mut offenders = Vec::new();
    for entry in fs::read_dir(root.join("crates")).map_err(|error| error.to_string())? {
        let dir = entry.map_err(|error| error.to_string())?.path();
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        // 组合根本身允许接触硬件；门禁只约束业务插件。
        if matches!(
            dir.file_name().and_then(|name| name.to_str()),
            Some("firmware" | "board-support")
        ) {
            continue;
        }
        let text = read(&manifest)?;
        if !text.contains("sdk") {
            continue;
        }
        for forbidden in ["board-support", "firmware", "rtic", "imxrt-"] {
            if text.contains(forbidden) {
                offenders.push(format!("{} 依赖 {forbidden}", manifest.display()));
            }
        }
    }
    if !offenders.is_empty() {
        return Err(format!("capsule 插件门禁失败：\n{}", offenders.join("\n")));
    }
    println!("[xtask] check-capsules 通过：sdk 纯逻辑，且没有插件直接依赖硬件/组合根");
    Ok(())
}

// ---------------------------------------------------------------- 中断优先级

/// 已落地的硬件中断任务及其**期望的 RTIC 优先级**。
///
/// R1：RTIC 的 `priority = N` 只接受整数字面量，编译器无法把它与
/// `primitives::budget::prio` 绑定；本门禁把这条接线变成**可失败判据**：
/// - 新增中断任务必须登记在这里（否则报"未登记的中断任务"）；
/// - 已登记任务的优先级字面量变了必须同步改这里与文档。
///
/// `console_rx`（`LPUART1`）与 `deadline_tick`（`PIT`）**同为 1** 是刻意的：
/// 两者互不抢占，相对字节间隔/时基周期分别有 4×/50× 余量。
/// `budget::prio::SERIAL_RX=2` 属于**尚未落地**的串行总线任务，不是 `console_rx`
/// （见 `docs/02-架构/中断与idle轮询分析.md` §3）。
const EXPECTED_TASK_PRIORITIES: &[(&str, u8)] = &[("LPUART1", 1), ("PIT", 1)];

/// 中断任务优先级门禁。
fn check_interrupt_priorities() -> Result<(), String> {
    let root = workspace_root();
    let main_source = read(&root.join("crates/firmware/src/main.rs"))?;
    let budget = read(&root.join("crates/primitives/src/budget.rs"))?;
    let deadline_tick = parse_u8_const(&budget, "DEADLINE_TICK")
        .ok_or("budget.rs 里解析不到 `pub const DEADLINE_TICK: u8`")?;

    let mut found: Vec<(String, u8)> = Vec::new();
    for line in main_source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("#[task(") {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("#[task(binds = ") else {
            return Err(format!("无法解析任务属性：{trimmed}"));
        };
        let Some((bind, tail)) = rest.split_once(',') else {
            return Err(format!("任务属性缺 `binds` 后的逗号：{trimmed}"));
        };
        let Some(rest) = tail.trim_start().strip_prefix("priority = ") else {
            return Err(format!("任务 {bind} 缺 `priority =`：{trimmed}"));
        };
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        let priority: u8 = digits
            .parse()
            .map_err(|_| format!("任务 {bind} 的 priority 不是数字：{trimmed}"))?;
        found.push((bind.trim().to_string(), priority));
    }
    if found.is_empty() {
        return Err(
            "main.rs 里一个 `#[task(binds = ...)]` 都解析不到（代码结构或解析器变了）".to_string(),
        );
    }

    for (bind, expected) in EXPECTED_TASK_PRIORITIES {
        let actual = found
            .iter()
            .find(|(name, _)| name == bind)
            .map(|(_, priority)| *priority)
            .ok_or_else(|| format!("已登记的中断任务 {bind} 没有出现在 main.rs 里"))?;
        // PIT 的期望值直接取预算表的 `DEADLINE_TICK`（改预算表会逼这里一起改）。
        let expected_value = if *bind == "PIT" {
            deadline_tick
        } else {
            *expected
        };
        if actual != expected_value {
            return Err(format!(
                "任务 {bind} 的 priority={actual}，期望 {expected_value}（改优先级必须同时改本门禁、budget 与文档）"
            ));
        }
    }
    for (name, _) in &found {
        if !EXPECTED_TASK_PRIORITIES
            .iter()
            .any(|(expected, _)| expected == name)
        {
            return Err(format!(
                "发现未登记的中断任务 {name}：新增任务必须同时登记本门禁与 docs/02-架构/中断与idle轮询分析.md"
            ));
        }
    }
    println!(
        "[xtask] check-interrupt-priorities 通过：LPUART1=1、PIT={deadline_tick}（budget::DEADLINE_TICK），无未登记中断任务"
    );
    Ok(())
}

/// 从源码里解析 `pub const NAME: u8 = N;`（只用于预算表数字，不做通用 Rust 解析）。
fn parse_u8_const(source: &str, name: &str) -> Option<u8> {
    for line in source.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("pub const ") else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(name) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(": u8 = ") else {
            continue;
        };
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        return digits.parse().ok();
    }
    None
}

fn check_lint_headers() -> Result<(), String> {
    let root = workspace_root();
    let mut problems = Vec::new();
    for crate_name in PURE_CRATES {
        let source = root
            .join("crates")
            .join(crate_name)
            .join("src")
            .join("lib.rs");
        let text = read(&source)?;
        for marker in REQUIRED_HEADER_MARKERS {
            if !text.contains(marker) {
                problems.push(format!("  {} 缺少 `{marker}`", source.display()));
            }
        }
    }
    if !problems.is_empty() {
        return Err(format!(
            "标准 lint 头不完整（这些约束不该靠人记）\n{}",
            problems.join("\n")
        ));
    }
    println!("[xtask] check-lint-headers 通过");
    Ok(())
}

// ---------------------------------------------------------------- unsafe 预算

fn check_unsafe() -> Result<(), String> {
    let root = workspace_root();
    let budget_path = root.join("unsafe.budget");
    let budget_text = read(&budget_path)?;
    let budget: usize = budget_text
        .lines()
        .find_map(|line| line.strip_prefix("blocks="))
        .and_then(|value| value.trim().parse().ok())
        .ok_or("unsafe.budget 缺少 `blocks=` 行")?;

    let mut total = 0usize;
    let mut missing_safety = Vec::new();
    for entry in fs::read_dir(root.join("crates")).map_err(|error| error.to_string())? {
        let dir = entry.map_err(|error| error.to_string())?.path();
        let src = dir.join("src");
        if !src.is_dir() {
            continue;
        }
        collect_unsafe(&src, &mut total, &mut missing_safety)?;
    }

    if !missing_safety.is_empty() {
        return Err(format!(
            "有 unsafe 块缺少 `// SAFETY:` 说明：\n{}",
            missing_safety.join("\n")
        ));
    }
    if total > budget {
        return Err(format!(
            "unsafe 块数超出预算：实际 {total}，预算 {budget}\n\
             新增 unsafe 必须：① 说明为什么无法避免；② 更新 unsafe.budget 并在提交信息里解释。\n\
             预算只允许减少或经显式批准增加。"
        ));
    }
    println!(
        "[xtask] check-unsafe 通过：{total} 个 unsafe 块（预算 {budget}），全部带 SAFETY 说明"
    );
    Ok(())
}

/// 统计一个文件里的 unsafe 形态与举证材料（按行，跳过文档注释）。
///
/// 返回 `(unsafe 块数, unsafe fn 数, // SAFETY: 条数, # Safety 段数)`。
fn count_unsafe(text: &str) -> (usize, usize, usize, usize) {
    let mut blocks = 0usize;
    let mut unsafe_fns = 0usize;
    let mut safety_comments = 0usize;
    let mut safety_docs = 0usize;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_doc = trimmed.starts_with("///") || trimmed.starts_with("//!");
        if is_doc {
            if trimmed.contains("# Safety") {
                safety_docs += 1;
            }
            continue;
        }
        blocks += line.matches("unsafe {").count();
        unsafe_fns += line.matches("unsafe fn ").count();
        safety_comments += line.matches("// SAFETY:").count();
    }
    (blocks, unsafe_fns, safety_comments, safety_docs)
}

fn collect_unsafe(dir: &Path, total: &mut usize, missing: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_unsafe(&path, total, missing)?;
            continue;
        }
        if path.extension().map(|ext| ext != "rs").unwrap_or(true) {
            continue;
        }
        let text = read(&path)?;
        // 两类 unsafe 有不同的举证义务，因此分开计：
        //
        // | 形态 | 必须提供 | 理由 |
        // |------|----------|------|
        // | `unsafe { .. }` 块 | 紧邻的 `// SAFETY:` | 读者要在**使用点**看到不变式 |
        // | `unsafe fn` | 文档里的 `# Safety` 段 | 义务转移给调用者，必须写清前提 |
        //
        // 统计按**行**进行并跳过文档注释：文档里常出现 `unsafe fn steal()` 这样的
        // 示例文本，把它当代码计数会产生假失败 —— 而假失败会训练人忽略门禁。
        let (blocks, unsafe_fns, safety_comments, safety_docs) = count_unsafe(&text);
        if blocks == 0 && unsafe_fns == 0 {
            continue;
        }
        *total += blocks + unsafe_fns;

        if safety_comments < blocks || safety_docs < unsafe_fns {
            missing.push(format!(
                "  {}：unsafe 块 {blocks} 处（// SAFETY: {safety_comments} 条）、unsafe fn {unsafe_fns} 处（# Safety 段 {safety_docs} 个）",
                path.display()
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 未实现项

fn check_gaps() -> Result<(), String> {
    let root = workspace_root();
    let mut gaps: Vec<(String, String, String)> = Vec::new();
    collect_gaps(&root.join("crates"), &mut gaps)?;
    if gaps.is_empty() {
        println!("[xtask] check-gaps：无未实现项");
        return Ok(());
    }
    println!(
        "[xtask] check-gaps：发现 {} 个未实现项（这些就是当前交付的诚实边界）",
        gaps.len()
    );
    for (milestone, location, line) in &gaps {
        println!("  {milestone}  {location}");
        println!("      {line}");
    }
    // 刻意**不失败**：未实现项是事实，不是错误。它需要被看见、被计划，
    // 而不是被隐藏。真正需要失败的是"假装实现了"。
    Ok(())
}

fn collect_gaps(dir: &Path, gaps: &mut Vec<(String, String, String)>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_gaps(&path, gaps)?;
            continue;
        }
        if path.extension().map(|ext| ext != "rs").unwrap_or(true) {
            continue;
        }
        let text = read(&path)?;
        let short = path
            .strip_prefix(workspace_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // 跳过文档注释：里面的 `GAP(Px)` 往往是在**解释**这套机制，不是未实现项。
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            if let Some(rest) = line.split("GAP(").nth(1) {
                if let Some((milestone, _)) = rest.split_once(')') {
                    // **带文件与行号**：缺口清单的价值在于"指到哪儿去改"，
                    // 只给一行源码文本，接手的 Agent 还得自己全仓搜一遍。
                    gaps.push((
                        format!("GAP({milestone})"),
                        format!("{short}:{}", number + 1),
                        line.trim().to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 编译失败用例

fn check_compile_fail() -> Result<(), String> {
    let root = workspace_root();
    let fixtures = root.join("tests").join("compile-fail");
    if !fixtures.is_dir() {
        return Err("缺少 tests/compile-fail 目录：类型约束的负例没有被验证".to_string());
    }
    let mut checked = 0usize;
    for entry in fs::read_dir(&fixtures).map_err(|error| error.to_string())? {
        let case = entry.map_err(|error| error.to_string())?.path();
        if !case.is_dir() {
            continue;
        }
        let expected = read(&case.join("EXPECTED"))?;
        let expected_code = expected.trim();
        // 负例共用主仓库的 target 目录：否则每个负例都会生成自己的 target/，
        // 既污染工作树，又让每次门禁都重新编译一遍依赖。
        let shared_target = root.join("target").join("compile-fail");
        let output = Command::new("cargo")
            .args([
                "build",
                "--offline",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--manifest-path",
            ])
            .arg(case.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &shared_target)
            .output()
            .map_err(|error| format!("运行 cargo 失败：{error}"))?;
        if output.status.success() {
            return Err(format!(
                "{} 竟然编译通过了：类型约束没有起作用",
                case.display()
            ));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains(expected_code) {
            return Err(format!(
                "{} 编译失败的原因与预期不符（期望包含 {expected_code}）\n--- stderr ---\n{stderr}",
                case.display()
            ));
        }
        checked += 1;
    }
    println!("[xtask] check-compile-fail 通过：{checked} 个负例确实被编译期拦住");
    Ok(())
}

// ---------------------------------------------------------------- 需求追溯

/// 从需求文档里抽出形如 `| RT-1 |` 的编号。
fn requirement_ids(text: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let first = trimmed
            .trim_start_matches('|')
            .split('|')
            .next()
            .unwrap_or("")
            .trim();
        if is_requirement_id(first) {
            ids.insert(first.to_string());
        }
    }
    ids
}

/// 需求编号的形状：`字母(1..=4) - 数字(1..=3)`，例如 `RT-1`、`SF-2`、`DB-4`。
///
/// 限定形状是必要的：markdown 表格的分隔行（`|------|----|`）与代码块里的横线
/// 都会落进"首列"，如果不限定形状，它们会被当成需求编号，产出满屏假覆盖。
fn is_requirement_id(candidate: &str) -> bool {
    let Some((prefix, number)) = candidate.split_once('-') else {
        return false;
    };
    let prefix_ok =
        !prefix.is_empty() && prefix.len() <= 4 && prefix.chars().all(|c| c.is_ascii_uppercase());
    let number_ok =
        !number.is_empty() && number.len() <= 3 && number.chars().all(|c| c.is_ascii_digit());
    prefix_ok && number_ok
}

/// 扫描所有 Rust 源文件，找出每个需求编号出现在哪些测试函数名/注释里。
fn scan_sources_for_ids(
    root: &Path,
    ids: &BTreeSet<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    let mut files = Vec::new();
    collect_rs_files(&root.join("crates"), &mut files)?;
    for file in files {
        let text = read(&file)?;
        for id in ids {
            // 需求编号用下划线形式出现在测试名里（`rt_1_...`）或原文出现在注释里。
            let underscored = id.replace('-', "_").to_ascii_lowercase();
            let lowered = text.to_ascii_lowercase();
            if lowered.contains(&underscored) || text.contains(id) {
                found.push((id.clone(), file.display().to_string()));
            }
        }
    }
    Ok(found)
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().map(|ext| ext == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

fn trace() -> Result<(), String> {
    let root = workspace_root();
    let spec_path = root.join("docs").join("01-需求").join("需求规格.md");
    let spec = read(&spec_path)?;
    let ids = requirement_ids(&spec);
    if ids.is_empty() {
        return Err("需求规格里没解析到任何编号（格式应为 `| RT-1 | ... |`）".to_string());
    }
    let found = scan_sources_for_ids(&root, &ids)?;
    let covered: BTreeSet<String> = found.iter().map(|(id, _)| id.clone()).collect();
    let missing: Vec<&String> = ids.difference(&covered).collect();

    // 生成追溯矩阵（人工不维护，避免与代码漂移）。
    let mut matrix = String::from(
        "# 需求追溯矩阵\n\n\
         > **本文件由 `cargo xtask trace` 生成，请勿手改。**\n\
         > 手写的追溯矩阵一定会与代码漂移；生成它则永远与代码一致。\n\
         > 判定依据：需求编号是否出现在 Rust 源码中（测试名用下划线形式，如 `rt_1_...`）。\n\n\
         | 需求 | 覆盖状态 | 出现位置 |\n|------|----------|----------|\n",
    );
    for id in &ids {
        let locations: Vec<&String> = found
            .iter()
            .filter(|(found_id, _)| found_id == id)
            .map(|(_, path)| path)
            .collect();
        let status = if locations.is_empty() {
            "❌ 未覆盖"
        } else {
            "✅"
        };
        let where_text = if locations.is_empty() {
            "—".to_string()
        } else {
            locations
                .iter()
                .map(|path| {
                    let root_text = root.display().to_string();
                    path.strip_prefix(root_text.as_str())
                        .unwrap_or(path)
                        .trim_start_matches('/')
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("<br>")
        };
        matrix.push_str(&format!("| {id} | {status} | {where_text} |\n"));
    }
    let matrix_path = root.join("docs").join("01-需求").join("需求追溯矩阵.md");
    fs::write(&matrix_path, matrix).map_err(|error| format!("写追溯矩阵失败：{error}"))?;

    if !missing.is_empty() {
        return Err(format!(
            "以下需求没有任何代码/测试引用：{}\n\
             追溯矩阵已生成到 {} —— 未覆盖的需求必须补测试，或从需求里删掉（两者都不做才是问题）。",
            missing
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            matrix_path.display()
        ));
    }
    println!(
        "[xtask] trace 通过：{} 条需求全部有代码/测试引用",
        ids.len()
    );
    Ok(())
}

// ---------------------------------------------------------------- 镜像与体积

/// 在 ELF 里找到覆盖 `flash_address` 的段，返回镜像内偏移与所在段数据。
fn locate_in_image(file: &object::File<'_>, flash_address: u64) -> Result<(u64, Vec<u8>), String> {
    for section in file.sections() {
        let address = section.address();
        let size = section.size();
        if address <= flash_address && flash_address < address.saturating_add(size) {
            let data = section
                .data()
                .map_err(|error| format!("读取段数据失败：{error}"))?
                .to_vec();
            return Ok((flash_address - address, data));
        }
    }
    Err(format!(
        "在镜像里找不到地址 {flash_address:#x} 所在的可加载段（链接脚本是否变了？）"
    ))
}

fn firmware_elf() -> Result<PathBuf, String> {
    let root = workspace_root();
    let candidate = root.join("target/thumbv7em-none-eabihf/release/firmware");
    if !candidate.is_file() {
        return Err(format!(
            "找不到固件 ELF：{}\n先跑 `cargo build --release`。",
            candidate.display()
        ));
    }
    Ok(candidate)
}

/// 读出所有**已定义符号**的（地址，名字）。
///
/// 与 [`read_sections`] 同一条路（`object` 直接解析链接产物）——
/// 判据必须来自产物，而不是源码里的结构体或名字。
fn read_symbols(path: &Path) -> Result<Vec<(u64, String)>, String> {
    let data = fs::read(path).map_err(|error| format!("读取 {path:?} 失败：{error}"))?;
    let file = object::File::parse(&*data).map_err(|error| format!("解析 ELF 失败：{error}"))?;
    let mut symbols = Vec::new();
    for symbol in file.symbols() {
        if !symbol.is_definition() {
            continue;
        }
        if let Ok(name) = symbol.name() {
            symbols.push((symbol.address(), name.to_string()));
        }
    }
    Ok(symbols)
}

/// **允许出现在 `.xip`（Flash 原地执行）里的符号**（名字片段或前缀）。
///
/// 每一条都必须能回答"它为什么不影响 Flash 擦写的实时性"。
const XIP_ALLOWED: &[&str] = &[
    // 空闲循环实体：优先级 0，被一切抢占；而长 Flash 操作本来就只发生在 idle 里。
    // （真机实测它一个函数占 13,122 字节 = 22% 的 ITCM，是搬出去的最大收益。）
    "idle_body",
    // 启动初始化：Flash 只读挂载，不擦写；放 XIP 冷区，省 4.3 KiB ITCM。
    "app4init",
    // 链接器为"从 XIP 跨区调用 ITCM"生成的 veneer（`__Thumbv7ABSLongThunk_*`）：
    // 只有几条指令，且是被调用的跳板，自身不做任何轮询或长活。
    "__Thumbv7ABSLongThunk_",
    // 启动报告/诊断输出：只在启动、终端命令和按需心跳时跑，不属于 Flash 擦写
    // 期间必须执行的路径；搬出 ITCM 给 S2 的实时统计留预算（2026-10-19）。
    "print_boot_report",
    "report_",
    "dump_",
    // FS 写请求的"投递+打印"：不碰 Flash，只入队；放在 .xip 给目标端 selftest 腾 ITCM。
    "submit_and_report",
    // 终端读/提交路径：不擦写 Flash；搬出 ITCM 给文件系统命令腾预算。
    "fs_snapshot",
    "miniz_oxide",
    "read_range",
    "inode_size",
    "read_inode_range",
    "inode_size",
    "read_log_range",
    "latest_data_version",
    "log_size",
    "file_size",
    "cmd_fs",
    // 终端 capsule：只搬串口字节，不做 Flash 擦写；放 XIP 省 ITCM。
    "TerminalService",
    "alloc_pool",
    "alloc",
    "dealloc",
    "describe",
    "banner",
    "prompt",
    "feed",
    "refresh_prompt",
    "fs_scan_snapshot",
    "fs_snapshot_from_stats",
    // 终端显式 GC 的入口：只负责调用 ITCM 里的 `filesystem::gc`，真正的擦/写
    // 不在 XIP 里执行；放在 .xip 是为了不挤占 core kernel 的 ITCM 余量。
    "fs_gc",
    // SDRAM 快速自检：只访问 SDRAM，不碰 Flash；与全片 run_post 同类。
    "run_quick_post",
    // 日志级别/终端开关/上一轮快照：只改 SDRAM 原子量或读 `.uninit`，
    // 不发起 Flash 擦写；放在 XIP 以节省 ITCM。
    "log_set_terminal",
    "log_set_level",
    "log_previous",
    // `.uninit` 崩溃日志镜像：只读写 OCRAM 静态区并做 D-Cache 维护，
    // 不发起 Flash 擦写；放 XIP 以保留 ITCM。
    "crash_log_init",
    "crash_log_write",
    "crash_log_used",
    "crash_log_clear",
    "crash_log_dump",
    // 持久化索引快照：只在 mount/checkpoint 冷路径使用，不参与 Flash 擦写热路径。
    "export_index",
    "mount_with_index",
    "parse_index",
    "mount",
    "checkpoint_index",
    "mark_index_dirty",
    // 这两个函数只调用 board-support 的 ITCM 擦写入口；擦除期间 XIP 调用者被挂起。
    "selftest_flash_write",
    "report_flash_selftest",
    // `fs checkpoint` 只调用 board-support 的 ITCM 擦写入口；擦除期间 XIP caller 被挂起。
    "fs_checkpoint",
    "fs_mkdir",
    "fs_rmdir",
    "fs_rename",
    "fs_link",
    "fs_unlink",
    "fs_write_queue",
    "status_lines",
    "can_status",
    "run_memtest",
    "fs_list",
    "fs_cat",
    "fs_stat",
    "fs_is_dir",
    "tail_file",
    "head_file",
    "wc_file",
    "gunzip_file",
    "decompress_file_into",
    "log_restore",
    "verify_gzip_file",
    "log_verify",
    "audit_command",
    "gzip_stack_check",
    "gzip_stack_paint",
    "stat",
    "is_empty",
    "reset_counters",
    "list_dir",
    "gzip_compress_job",
    "switch_to_sdram_stack",
    "gzip_on_sdram_stack",
    "gzip_probe",
    "gzip_file",
    "gzip_store",
    "log_event",
    "init_text_log",
    "log_try_flush",
    "flush_text_log",
    "log_file",
    "append_file_chunks",
    "terminal_color_log_line",
    "rotate_log_if_needed",
    "log_stats",
    "log_flush",
    "log_clear",
    "log_rotate",
    "log_grep",
    // 日志归档：只做目录扫描、读文件、gzip 缓冲与写队列投递；真正的 Flash 擦写
    // 由 ITCM 里的 `board_support::fs::step` 执行，因此这些函数在擦写期间不会被取指。
    "archive_",
    "restore_windows",
    "run_selftest",
    "current_beat",
    "run_boot_post",
    "run_post",
    "request_reboot",
    "inject_fault",
    "ShellContext8selftest",
    // 终端命令处理层：只在 idle 冷路径运行；Flash 驱动/擦写轮询仍留在 ITCM。
    "shell8commands",
];

/// **代码放置**门禁：`.xip`（Flash 原地执行）里不得出现"擦写期间必须能跑"的代码。
///
/// # 这条门禁防的是什么（自锁，不是性能）
///
/// 把冷代码放进 `.xip` 是刻意的（`idle` 一个函数就占 13,122 字节 = 22% 的 ITCM），
/// 代价是 Flash 擦/写期间 **XIP 取指会停等**（真机：擦 64 KiB = 571 ms）。
///
/// 架构上它被挡在安全侧：**长 Flash 操作只发生在 `idle` 里**，而 `idle` 被抢占时
/// 跑的是 ITCM 代码。但这个安全性**完全依赖"Flash 驱动及其调用链留在 ITCM"** ——
/// 一旦有人把驱动也标上 `.xip`，就会变成：
///
/// ```text
/// 擦除 → 驱动在 XIP 里取指 → 等器件 WIP 清 → 取指停等（器件正忙）
///      → 永远等不到下一次轮询 → 自锁
/// ```
///
/// 而且它在构建、主机测试、静态门禁里**全都是绿的** —— 只有真机擦一次才暴露。
/// ⇒ 必须由产物断言钉住。
///
/// # 判据
///
/// 1. `.xip` 段的 VMA 必须落在 Flash 的 XIP 窗口内（否则"原地执行"这个前提不成立）；
/// 2. **`.xip` 的机器码里不得出现 FlexSPI 控制器的寄存器字面量** ——
///    即"Flash 驱动不能被编译进 XIP 代码"。
fn check_code_placement() -> Result<(), String> {
    let elf = firmware_elf()?;
    let sections = read_sections(&elf)?;
    let xip = sections
        .iter()
        .find(|section| section.name == ".xip")
        .ok_or("链接产物里没有 .xip 段 —— 链接脚本变了？")?;

    let mut problems: Vec<String> = Vec::new();
    let xip_end = xip.address.saturating_add(xip.size);

    // 判据 1：`.xip` 必须真的在 Flash 的 XIP 窗口里。
    let xip_base = u32::try_from(xip.address).map_err(|_| ".xip 地址超出 32 位")?;
    if !memory_layout::FLASH_XIP.contains(xip_base) {
        problems.push(format!(
            ".xip 的地址 {:#010x} 不在 Flash XIP 窗口（{:#010x}..{:#010x}）内 —— \
             `> REGION_LOAD_TEXT`（VMA == LMA）这个前提不成立",
            xip.address,
            memory_layout::FLASH_XIP.base,
            memory_layout::FLASH_XIP.end()
        ));
    }

    // 判据 2（**核心**）：`.xip` 里只允许出现**明确批准过**的符号。
    //
    // # 为什么用"正向白名单"而不是"按名字找驱动"
    //
    // 第一版我写的是"名字里含 `board_support10flexspi_ip` 的符号不得进 `.xip`"，
    // 负例（把 `program_page_staged` 标成 `.xip`）跑出来**门禁照样 PASS** ——
    // 因为 `lto = "fat"` 把那个模块整个内联了，`nm | grep flexspi_ip` 返回**空**，
    // 没有任何符号可供匹配。**按名字的断言在 fat LTO 下是无效判据。**
    //
    // 第二版改成"扫 `.xip` 机器码里的 FlexSPI 寄存器地址字面量"，负例仍然 PASS ——
    // 反汇编后发现 Thumb-2 用 `movw`/`movt` 把地址**拆进指令字段**
    //（`f2c4 0a2a` 里没有连续的 `2a 40` 字节），字节扫描抓不到。
    //
    // ⇒ 第三版（本版）：**正向白名单**。`.xip` 是一块"必须经过评审才能进"的区域，
    // 因此判据应当是"里面只有批准过的东西"，而不是"里面没有某个坏东西"。
    // 任何新符号出现在 `.xip` 都会失败，负例因此是可构造的。
    //
    // ⚠️ **这条判据看不见内联**：如果驱动代码被内联进 `idle_body`，
    // 符号表不会有变化。所以它**不是**充分判据 —— 充分判据是真机上
    // **从 `idle` 里发起一次擦除+编程**（见 `docs` 的验证矩阵）。
    // 静态门禁负责"挡新增放置"，真机自证负责"内联没把驱动带进来"，两者缺一不可。
    for (address, name) in read_symbols(&elf)? {
        if address < xip.address || address >= xip_end {
            continue;
        }
        // 链接器符号（`__sitext` 等绝对符号）与段边界不是代码，跳过。
        if name.starts_with("__") {
            continue;
        }
        if XIP_ALLOWED
            .iter()
            .any(|pattern| name.contains(pattern) || name.starts_with(pattern))
        {
            continue;
        }
        problems.push(format!(
            "{name}\n  位于 .xip（{address:#010x}），但不在批准名单里。\n  \
             `.xip` 是 Flash 原地执行的区域：里面的代码在 Flash 擦/写期间会**取指停等**\n  \
             （真机：擦 64 KiB = 571 ms）。只有「与 Flash 操作无关」的冷代码才可以放进来。\n  \
             处置：① 若确实该放这里，在 xtask 的 `XIP_ALLOWED` 里加一条并写明理由；\n  \
             ② 否则去掉该函数上的 `#[unsafe(link_section = \".xip\")]`。"
        ));
    }

    if !problems.is_empty() {
        return Err(format!("代码放置不符：\n  {}", problems.join("\n  ")));
    }
    println!(
        "[xtask] check-code-placement 通过：.xip {} 字节在 Flash 原地执行（{:#010x}..{:#010x}）；\
         其中只有批准符号（`XIP_ALLOWED` 共 {} 条）",
        xip.size,
        xip.address,
        xip_end,
        XIP_ALLOWED.len()
    );
    Ok(())
}

fn check_image() -> Result<(), String> {
    let elf = firmware_elf()?;
    let data = fs::read(&elf).map_err(|error| format!("读取 ELF 失败：{error}"))?;
    let file = object::File::parse(&*data).map_err(|error| format!("解析 ELF 失败：{error}"))?;

    // 1) FCB：Flash 偏移 0 处的魔数与版本。
    let (offset, section) = locate_in_image(&file, FLASH_BASE + FCB_OFFSET)?;
    let tag = read_u32_le(&section, offset)?;
    if tag != FCB_TAG {
        return Err(format!(
            "Flash 偏移 0 处的 FCB 魔数不对：期望 {FCB_TAG:#010x}，实际 {tag:#010x}\n\
             最可能的原因：`FLEXSPI_CONFIGURATION_BLOCK` 被改名或去掉了 \
             `#[unsafe(no_mangle)]`，链接器因此没有保留它。"
        ));
    }
    let version = read_u32_le(&section, offset + 4)?;
    if version != 0x5601_0400 {
        return Err(format!(
            "FCB 版本字段异常：{version:#010x}（期望 0x56010400 = 1.4.0）"
        ));
    }
    let flash_size = read_u32_le(&section, offset + FCB_FLASH_SIZE_A1_OFFSET)?;
    let expected_size = 32 * 1024 * 1024u32;
    if flash_size != expected_size {
        return Err(format!(
            "FCB 里的器件容量 {flash_size:#x} 与实装 W25Q256（{expected_size:#x}）不符"
        ));
    }

    // 2) IVT：Flash 偏移 0x1000 处的头魔数。
    let (ivt_offset, ivt_section) = locate_in_image(&file, FLASH_BASE + IVT_OFFSET)?;
    let magic = read_u32_le(&ivt_section, ivt_offset)?;
    if magic != IVT_MAGIC {
        return Err(format!(
            "Flash 偏移 0x1000 处的 IVT 魔数不对：期望 {IVT_MAGIC:#010x}，实际 {magic:#010x}"
        ));
    }

    // 3) 向量表：初始栈指针必须落在 DTCM 内。
    //    Boot ROM 会把向量表从 Flash 搬到 RAM，因此这里找的是 RAM 里的那一份。
    let sp = find_initial_stack_pointer(&file)?;
    if !(DTCM_BASE..DTCM_END).contains(&sp) {
        return Err(format!(
            "向量表的初始栈指针 {sp:#010x} 不在 DTCM（{DTCM_BASE:#x}..{DTCM_END:#x}）内\n\
             栈指针错位的镜像**能烧录但会立刻跑飞**。"
        ));
    }

    // 4) LUT 的地址相位必须是 **4 字节**（`0x20`），读操作码必须是 `0xEC`。
    //
    // 这一条把两件必须同时成立的事绑在一起：分区表声称"整片 32 MiB 可寻址"
    // （`XIP_WINDOW == FLASH_SIZE`，由 `check-flash-map` 检查），而它成立的前提
    // 就是这里的地址相位。**若有人把 LUT 换回 3 字节（操作码 0xEB / 相位 0x18），
    // 这条断言会在构建期失败** —— 否则后果是静默别名：越过 16 MiB 的读写会落到
    // 低 16 MiB 上（"擦日志区"擦掉启动头）。
    //
    // FlexSPI LUT 的编码（NXP `FSL_ROM_FLEXSPI_LUT_SEQ`）：
    // 低半字 = 指令0（操作码 bits15:10、线数 bits9:8、操作数 bits7:0），
    // 高半字 = 指令1（同上，各右移 16 位）。
    // 三个序列（读/页编程/扇区擦除）都必须是 4 字节地址相位。只查读序列不够：
    // 读相位对了而写/擦相位是 3 字节的话，**写操作会落到错误地址**（越界别名），
    // 那正是"擦日志区却擦掉启动头"的成因。
    let lut = offset + FCB_LUT_OFFSET;
    for (seq_index, what, expected_opcode) in [
        (0u64, "读", 0xECu32),
        (5, "扇区擦除", 0x21u32),
        (9, "页编程", 0x34u32),
    ] {
        let word0 = read_u32_le(&section, lut + seq_index * 16)?;
        let opcode = word0 & 0xFF;
        let pads0 = (word0 >> 8) & 0x3;
        let address_phase = (word0 >> 16) & 0xFF;
        let pads1 = (word0 >> 24) & 0x3;
        if opcode != expected_opcode || address_phase != 0x20 {
            return Err(format!(
                "FCB 的{what}序列不是 4 字节地址相位：命令={opcode:#04x}（期望 {expected_opcode:#04x}）、\
                 地址相位={address_phase:#04x}（期望 0x20 = 32 位）、命令线数={pads0}、地址线数={pads1}\n\
                 LUT 索引 {seq_index}（FCB 偏移 {:#x}）第 0 个字 = {word0:#010x}。\n\
                 3 字节地址只能寻址前 16 MiB；越界访问不会报错，而会被静默别名到低 16 MiB。",
                lut + seq_index * 16
            ));
        }
    }

    println!(
        "[xtask] check-image 通过：FCB tag/版本/容量、LUT 三个序列均为 4 字节地址相位\
         （读 0xEC / 擦 0x21 / 写 0x34）、IVT 魔数、初始 SP={sp:#010x}"
    );
    Ok(())
}

fn read_u32_le(data: &[u8], offset: u64) -> Result<u32, String> {
    let start = usize::try_from(offset).map_err(|_| "偏移超出地址空间".to_string())?;
    let bytes = data.get(start..start.saturating_add(4)).ok_or_else(|| {
        format!(
            "读取 {start:#x} 处 4 字节越界（段只有 {} 字节）",
            data.len()
        )
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// 找到 Cortex-M 向量表的第一个字（初始栈指针）。
///
/// 判据：一个落在 RAM 范围里的字，且紧跟着的第二个字（复位向量）是奇数
/// （Thumb 代码地址最低位为 1）。两个条件一起用，避免把数据段误判成向量表。
fn find_initial_stack_pointer(file: &object::File<'_>) -> Result<u64, String> {
    for section in file.sections() {
        let Ok(data) = section.data() else { continue };
        if data.len() < 8 || section.address() == 0 {
            continue;
        }
        let candidate = u64::from(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
        let reset = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        if (DTCM_BASE..=DTCM_END).contains(&candidate) && reset % 2 == 1 {
            return Ok(candidate);
        }
    }
    Err(
        "找不到向量表（没有段的头两个字满足\"初始 SP 在 DTCM + 复位向量为 Thumb 地址\"）"
            .to_string(),
    )
}

/// 把"哪些字节进 Flash"算出来（供 `check-size` 与 `check-flash-map` 共用）。
///
/// 不能只数"VMA 落在 Flash 的段"：`.text` 的 VMA 在 ITCM、`.rodata`/`.data` 在 OCRAM，
/// 但它们的**加载映像**都在 Flash 里。用 VMA 过滤会得到明显偏小的数字。
fn flash_image_bytes(elf: &Path) -> Result<u64, String> {
    let data = fs::read(elf).map_err(|error| format!("读取 ELF 失败：{error}"))?;
    let file = object::File::parse(&*data).map_err(|error| format!("解析 ELF 失败：{error}"))?;
    let mut total = 0u64;
    for section in file.sections() {
        if section.kind() == object::SectionKind::UninitializedData {
            continue; // .bss 不占 Flash
        }
        let allocated = matches!(
            section.flags(),
            object::SectionFlags::Elf { sh_flags } if sh_flags & u64::from(object::elf::SHF_ALLOC) != 0
        );
        if !allocated {
            continue;
        }
        if let Some((_offset, size)) = section.file_range() {
            total = total.saturating_add(size);
        }
    }
    Ok(total)
}

/// Flash 分区表门禁：**表本身自洽**，且**当前映像放得下**。
///
/// 这一项存在的理由很直接：分区表错了的后果不是"报错"，而是**擦掉别人的数据**。
/// 因此把不变量做成可执行的检查，而不是文档里的一句"注意不要重叠"。
/// 表来自 `flash-layout`（与固件共用同一份数据，不解析源码）。
/// 把字节数写成便于人读的 KiB/MiB。
fn format_kib(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", f64::from(bytes) / (1024.0 * 1024.0))
    } else {
        format!("{} KiB", bytes / 1024)
    }
}

/// 用途的中文标签（门禁输出给人看，不用 Debug 名）。
fn purpose_label(purpose: &flash_layout::Purpose) -> &'static str {
    use flash_layout::Purpose;
    match purpose {
        Purpose::BootHeader => "启动头",
        Purpose::Bootloader => "引导程序",
        Purpose::AppSlot => "应用槽",
        Purpose::BootMeta => "A/B 元数据",
        Purpose::Filesystem => "文件系统",
    }
}

/// 掉电策略的中文标签。
fn atomicity_label(atomicity: &flash_layout::Atomicity) -> &'static str {
    use flash_layout::Atomicity;
    match atomicity {
        Atomicity::ReadOnly => "运行期只读",
        Atomicity::DoubleBank => "双份+序号+CRC",
        Atomicity::WholeRegionSwap => "整区替换",
        Atomicity::Filesystem => "文件系统保证",
        Atomicity::Unused => "未启用",
    }
}

/// 过渡期存储槽大小：与 `board_support::config::STORE_SLOT_BYTES` 一致。
///
/// 门禁不依赖 BSP（那是目标平台 crate），因此这里重复一次常量并**断言**它够小：
/// 真正的约束是"6 个槽位不得越出文件系统分区"，重复一个 4 KiB 常量不构成风险。
fn board_support_store_slot_bytes() -> u32 {
    4 * 1024
}

fn check_flash_map() -> Result<(), String> {
    use flash_layout as map;

    // --- 1) 表级不变量 ---
    let mut cursor = 0u32;
    for region in map::MAP {
        if region.offset != cursor {
            return Err(format!(
                "分区 {} 起始 {:#x} 与前一个分区的末尾 {:#x} 不连续（重叠或留缝）",
                region.name, region.offset, cursor
            ));
        }
        if region.offset % map::SECTOR != 0 {
            return Err(format!("分区 {} 起始未按 4 KiB 扇区对齐", region.name));
        }
        if region.size == 0 || region.size % map::SECTOR != 0 {
            return Err(format!(
                "分区 {} 大小 {:#x} 不是 4 KiB 的整数倍（擦除会波及邻居）",
                region.name, region.size
            ));
        }
        if region.why.is_empty() {
            return Err(format!("分区 {} 没有写'为什么这样分'", region.name));
        }
        if region.end() > map::XIP_WINDOW {
            return Err(format!(
                "分区 {} 末尾 {:#x} 越过可寻址窗口 {:#x}",
                region.name,
                region.end(),
                map::XIP_WINDOW
            ));
        }
        cursor = region.end();
    }
    if cursor != map::FLASH_SIZE {
        return Err(format!(
            "分区表覆盖到 {cursor:#x}，而 Flash 是 {:#x}",
            map::FLASH_SIZE
        ));
    }

    // --- 1.5) **单一属主**：除文件系统外，任何分区都不得与文件系统区间相交 ---
    //
    // 为什么要有这条（2026-10-12 审计的核心）：历史上曾出现"双属主"事故 ——
    // 一套过渡期的原始偏移表与文件系统**块 0 物理重叠**，其中一方不知道自己会被
    // 另一方擦掉：文件系统的第一次 `erase_block(0)` 就会把它抹掉。
    //
    // 光"现在删掉了"不够：这条规则要保证它**不可能被重新引入**。判据是几何的：
    // 文件系统的区间一旦确定，任何别的分区与它有交集就是错误（无论它叫什么名字、
    // 声称自己多小）。
    let mut filesystem: Option<(u32, u32, &str)> = None;
    for region in map::MAP {
        if region.offset == map::FILESYSTEM_OFFSET && region.size == map::FILESYSTEM_SIZE {
            let end = region.offset + region.size;
            if let Some((start, prev_end, name)) = filesystem {
                if region.offset < prev_end {
                    return Err(format!(
                        "文件系统分区 `{}`（{:#x}..{:#x}）与 `{name}`（{start:#x}..{prev_end:#x}）重叠 —— \
                         存储只允许一个属主",
                        region.name, region.offset, end
                    ));
                }
            }
            filesystem = Some((region.offset, end, region.name));
        }
    }
    let Some((fs_start, fs_end, fs_name)) = filesystem else {
        return Err("分区表里没有文件系统分区：持久化没有属主".to_string());
    };
    for region in map::MAP {
        if region.offset == map::FILESYSTEM_OFFSET && region.size == map::FILESYSTEM_SIZE {
            continue;
        }
        let end = region.offset + region.size;
        // 只读启动头/引导区**允许**在文件系统之外；不允许的是"与文件系统有交集"。
        if region.offset < fs_end && end > fs_start {
            return Err(format!(
                "分区 `{}`（{:#x}..{:#x}）与文件系统 `{fs_name}`（{fs_start:#x}..{fs_end:#x}）相交 —— \
                 存储必须只有一个属主（历史上的双属主事故就是这么发生的）",
                region.name, region.offset, end
            ));
        }
    }

    // --- 2) run/backup 与启动头这两条"安全论证依赖的形状" ---
    let slot_run = map::find("slot_run").ok_or("分区表缺少 slot_run")?;
    let slot_backup = map::find("slot_backup").ok_or("分区表缺少 slot_backup")?;
    if slot_run.size != slot_backup.size {
        return Err("run/backup 槽大小不同：复制恢复的论证不成立".to_string());
    }
    let header = map::find("boot_header").ok_or("分区表缺少 boot_header")?;
    if header.offset != 0 || header.atomicity != map::Atomicity::ReadOnly {
        return Err("启动头必须位于偏移 0 且标记为运行期只读".to_string());
    }

    // --- 3) 当前映像放得下 ---
    let elf = firmware_elf()?;
    let bytes = flash_image_bytes(&elf)?;
    if u64::from(slot_run.size) < bytes {
        return Err(format!(
            "映像 {bytes} 字节放不进 slot_run（{} 字节）",
            slot_run.size
        ));
    }
    // 阶段 1（尚未引入引导程序）：映像从偏移 0 起，因此不得越过第一个
    // **非启动/升级**分区（当前是 boot_meta）。
    let first_data = map::MAP
        .iter()
        .find(|region| {
            !matches!(
                region.purpose,
                map::Purpose::BootHeader | map::Purpose::Bootloader | map::Purpose::AppSlot
            )
        })
        .ok_or("分区表里没有数据分区")?;
    if bytes > u64::from(first_data.offset) {
        return Err(format!(
            "映像 {bytes} 字节越过第一个数据分区 {}（偏移 {:#x}）：\
             阶段 1 的映像从偏移 0 起，必须留在它之前",
            first_data.name, first_data.offset
        ));
    }
    if bytes > u64::from(map::SLOT_RUN_OFFSET) {
        return Err(format!(
            "阶段 1 映像 {bytes} 字节进入 slot_run（偏移 {:#x}）：\
             过渡期只允许临时占用 bootloader 区，不得占用 run/backup 槽",
            map::SLOT_RUN_OFFSET
        ));
    }
    if bytes > u64::from(map::BOOTLOADER_OFFSET) {
        println!(
            "[xtask] 阶段 1 临时占用：映像 {bytes} 字节覆盖 bootloader 区 \
             {:#x}..{:#x}（Bootloader 接入后必须迁到 slot_run）",
            map::BOOTLOADER_OFFSET,
            map::SLOT_RUN_OFFSET
        );
    }
    // 过渡期的原始偏移存储必须落在文件系统分区内，且不能碰到它的邻居。
    let fs = map::find("filesystem").ok_or("分区表缺少 filesystem")?;
    let transitional_store_bytes = 6 * u32::try_from(board_support_store_slot_bytes())
        .map_err(|_| "槽位大小溢出".to_string())?;
    if map::FILESYSTEM_OFFSET + transitional_store_bytes > fs.end() {
        return Err(format!(
            "过渡期存储区（{} 字节）越出文件系统分区",
            transitional_store_bytes
        ));
    }

    // --- 4) 文档里的表格必须与分区表一致 ---
    //
    // 这一条是**针对"两处事实"本身**的检查：分区表的数字在代码里，理由在文档里，
    // 而最容易发生的漂移是"改了代码忘了改文档"（或反过来）。
    // 与其在文档里写一句"请保持同步"，不如让门禁读它。
    let doc = fs::read_to_string(workspace_root().join("docs/02-架构/Flash布局与用途划分.md"))
        .map_err(|error| format!("读取 Flash 布局文档失败：{error}"))?;
    for region in map::MAP {
        // 只认**表格行**（以 `|` 开头且分区名带反引号）：文档里还有描述既有固件
        // 分区表的正文，正文里也会出现同样的名字与偏移 —— 那是别的东西。
        let label = format!("`{}`", region.name);
        let row = doc
            .lines()
            .find(|line| line.trim_start().starts_with('|') && line.contains(&label))
            .ok_or_else(|| format!("文档的分区表里找不到 {} 那一行", region.name))?;
        let offset = format!("`{:#08x}`", region.offset);
        if !row.contains(&offset) {
            return Err(format!(
                "文档里 {} 的偏移与代码不一致：文档行 `{}`，代码偏移 {}",
                region.name,
                row.trim(),
                offset
            ));
        }
    }

    // 人读的分区表：直接从 `flash-layout` 打印。文档只解释**为什么**这样分，
    // 这里的数字才是事实 —— 让人查布局时不必在两处之间来回核对。
    println!("  偏移        大小        用途            掉电策略        名称");
    for region in map::MAP {
        println!(
            "  {:#010x}  {:>9}  {:<14}  {:<14}  {}",
            region.offset,
            format_kib(region.size),
            purpose_label(&region.purpose),
            atomicity_label(&region.atomicity),
            region.name
        );
    }
    println!(
        "  合计：{:#x} 字节（{} MiB）已全部划完",
        map::FLASH_SIZE,
        map::FLASH_SIZE / (1024 * 1024)
    );

    println!(
        "[xtask] check-flash-map 通过：{} 个分区（启动头/引导程序/槽A/槽B/元数据/文件系统），\
         映像 {bytes} 字节 / slot_run {} 字节；首个非启动分区 {:#x}",
        map::COUNT,
        slot_run.size,
        first_data.offset
    );
    Ok(())
}

fn check_size() -> Result<(), String> {
    let total = flash_image_bytes(&firmware_elf()?)?;
    if total > MAX_FIRMWARE_BYTES {
        return Err(format!(
            "固件占 Flash {total} 字节，超过上限 {MAX_FIRMWARE_BYTES} 字节（1 MiB）\n\
             这个上限不是审美问题：它保证单槽升级的映像能放得进预留区。"
        ));
    }
    println!("[xtask] check-size 通过：Flash 占用 {total} 字节 / 上限 {MAX_FIRMWARE_BYTES}");
    Ok(())
}

// ---------------------------------------------------------------- verify

fn run(command: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(command)
        .args(args)
        .current_dir(workspace_root())
        .status()
        .map_err(|error| format!("执行 {command} 失败：{error}"))?;
    if !status.success() {
        return Err(format!(
            "`{command} {}` 失败（退出码 {status}）",
            args.join(" ")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- Tock（实验性）

/// 板级 crate 目录（作为 cargo 工作目录，才能取到板级 `.cargo/config.toml`）。
const TOCK_BOARD_DIR: &str = "runtime/tock/boards/tessera-rt1021";
/// 构建产物（cargo 工作区 target 目录）。
const TOCK_ELF: &str = "runtime/tock/target/thumbv7em-none-eabihf/release/tessera-rt1021";

/// 构建最小 Tock 镜像（`cargo build --release`，不用 make/脚本）。
fn tock_build() -> Result<(), String> {
    let root = workspace_root();
    let board = root.join(TOCK_BOARD_DIR);
    if !board.join("Cargo.toml").is_file() {
        return Err(format!(
            "找不到 Tock 板级 crate：{}（runtime/tock 未入仓？）",
            board.display()
        ));
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&board)
        .status()
        .map_err(|error| format!("执行 cargo 失败：{error}"))?;
    if !status.success() {
        return Err("Tock 镜像构建失败（见上方 cargo 输出）".to_string());
    }
    let elf = root.join(TOCK_ELF);
    if !elf.is_file() {
        return Err(format!("构建结束但找不到 ELF：{}", elf.display()));
    }
    println!("[xtask] tock-build 完成：{}", elf.display());
    Ok(())
}

/// 构建并烧录 Tock 镜像（全片擦除；会清掉板上文件系统）。
fn tock_flash() -> Result<(), String> {
    tock_build()?;
    let root = workspace_root();
    let elf = root.join(TOCK_ELF);
    let pyocd = resolve_pyocd().ok_or(
        "找不到 pyocd：先跑一次台架准备（见 docs/04-开发/台架准备与排障.md），或设置 PYOCD",
    )?;
    let status = std::process::Command::new(&pyocd)
        .args([
            "load",
            "--target",
            "mimxrt1020",
            "--format",
            "elf",
            "-e",
            "chip",
        ])
        .arg(&elf)
        .status()
        .map_err(|error| format!("执行 pyocd 失败：{error}"))?;
    if !status.success() {
        return Err("pyocd 烧录失败".to_string());
    }
    println!("[xtask] tock-flash 完成：已烧录 {}", elf.display());
    Ok(())
}

/// 构建 → 全片擦除烧录 → 串口验证（启动横幅 + 慢速行回显）。
///
/// 验证判据（失败即返回错误）：
/// 1. 输出包含 `tock: tessera-rt1021 kernel boot`；
/// 2. 输出包含 `tock: kernel_loop running`；
/// 3. 逐字节（20 ms/字节）发送一行后，输出包含该行的 `rx line(NB)` 回显。
fn tock_verify() -> Result<(), String> {
    tock_flash()?;
    let serial =
        std::env::var("BASE_SERIAL").unwrap_or_else(|_| "/dev/pudu-debugger-16p-uart".to_string());
    let pyocd = resolve_pyocd().unwrap_or_else(|| "pyocd".to_string());
    let python = resolve_python();
    let script = format!(
        "import serial,time,subprocess,threading\n\
         p=serial.Serial('{serial}',115200,timeout=0.2)\n\
         p.reset_input_buffer()\n\
         threading.Thread(target=lambda: subprocess.run(['{pyocd}','reset','--target','mimxrt1020'],capture_output=True),daemon=True).start()\n\
         raw=b''\n\
         end=time.time()+4\n\
         while time.time()<end:\n\
         \x20   raw+=p.read(65536)\n\
         for ch in b'TOCK-VERIFY\\n':\n\
         \x20   p.write(bytes([ch])); time.sleep(0.02)\n\
         time.sleep(0.7)\n\
         end=time.time()+1.5\n\
         while time.time()<end:\n\
         \x20   raw+=p.read(65536)\n\
         print(raw.decode('utf-8','replace'))\n"
    );
    let output = std::process::Command::new(&python)
        .args(["-c", &script])
        .output()
        .map_err(|error| format!("执行串口抓包失败（{python}）：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "串口抓包失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let checks = [
        ("启动横幅", text.contains("tock: tessera-rt1021 kernel boot")),
        ("内核 loop", text.contains("tock: kernel_loop running")),
        ("行回显", text.contains("rx line(11B): TOCK-VERIFY")),
    ];
    let mut failed = Vec::new();
    for (name, ok) in checks {
        println!("  {} {}", if ok { "✅" } else { "❌" }, name);
        if !ok {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        println!("\n---- 原始串口输出 ----\n{text}\n----------------------");
        return Err(format!("Tock 验证失败：{}", failed.join("、")));
    }
    println!("[xtask] tock-verify 通过：Tock 内核启动 + 中断驱动行回显");
    Ok(())
}


/// 从生产固件 ELF 同步 Tock 板级 crate 的 8 KiB 启动头（FCB + IVT）。
///
/// 为什么需要它：`boot_hdr.bin` 是**构建输入源文件**（不是构建产物），
/// 内容必须与生产固件的启动头逐字节一致；生产侧改启动头（FCB/IVT/分区）后，
/// 用本命令重新生成并跑 `cargo xtask tock-verify`，避免两份事实漂移。
fn tock_sync_boot_header() -> Result<(), String> {
    let root = workspace_root();
    // 1) 构建生产固件（Tessera 默认 target 就是 firmware target）。
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&root)
        .status()
        .map_err(|error| format!("执行 cargo 失败：{error}"))?;
    if !status.success() {
        return Err("生产固件构建失败".to_string());
    }
    let firmware = root.join("target/thumbv7em-none-eabihf/release/firmware");
    if !firmware.is_file() {
        return Err(format!("找不到生产固件 ELF：{}", firmware.display()));
    }

    // 2) 找工具链自带的 llvm-objcopy（与 `check-*` 门禁同一来源，不依赖 PATH）。
    let sysroot = String::from_utf8(
        std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .map_err(|error| format!("执行 rustc 失败：{error}"))?
            .stdout,
    )
    .map_err(|error| format!("rustc sysroot 不是 UTF-8：{error}"))?
    .trim()
    .to_string();
    let rustlib = Path::new(&sysroot).join("lib/rustlib");
    let mut objcopy: Option<PathBuf> = None;
    for entry in fs::read_dir(&rustlib).map_err(|error| error.to_string())? {
        let candidate = entry
            .map_err(|error| error.to_string())?
            .path()
            .join("bin/llvm-objcopy");
        if candidate.is_file() {
            objcopy = Some(candidate);
            break;
        }
    }
    let objcopy = objcopy.ok_or("工具链里找不到 llvm-objcopy（需要 llvm-tools 组件）")?;

    // 3) ELF → 二进制，取前 8192 B。
    let tmp_dir = root.join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(|error| error.to_string())?;
    let raw = tmp_dir.join("tock-boot-header.bin");
    let status = std::process::Command::new(&objcopy)
        .args(["-O", "binary"])
        .arg(&firmware)
        .arg(&raw)
        .status()
        .map_err(|error| format!("执行 llvm-objcopy 失败：{error}"))?;
    if !status.success() {
        return Err("llvm-objcopy 转换失败".to_string());
    }
    let bytes = fs::read(&raw).map_err(|error| error.to_string())?;
    let header = bytes
        .get(..8192)
        .ok_or("生产固件二进制不足 8192 B，无法提取启动头")?;
    // 4) 自检：FCB tag + IVT magic/entry（与硬件启动约束一致，失败就不要写入）。
    if header.get(..4) != Some(b"FCFB") {
        return Err("生产固件偏移 0 不是 FCFB（启动头异常）".to_string());
    }
    let ivt_magic = u32::from_le_bytes(
        header
            .get(0x1000..0x1004)
            .and_then(|slice| slice.try_into().ok())
            .ok_or("读不到 IVT magic")?,
    );
    let ivt_entry = u32::from_le_bytes(
        header
            .get(0x1004..0x1008)
            .and_then(|slice| slice.try_into().ok())
            .ok_or("读不到 IVT entry")?,
    );
    if ivt_magic != 0x4020_00D1 || ivt_entry != 0x6000_2000 {
        return Err(format!(
            "IVT 自检失败：magic={ivt_magic:#010x} entry={ivt_entry:#010x}（期望 0x402000d1 / 0x60002000）"
        ));
    }
    let target = root.join("runtime/tock/boards/tessera-rt1021/boot_hdr.bin");
    fs::write(&target, header).map_err(|error| error.to_string())?;
    println!(
        "[xtask] tock-sync-boot-header 完成：{}（FCB + IVT entry=0x60002000）",
        target.display()
    );
    println!("        下一步：cargo xtask tock-verify");
    Ok(())
}

fn verify() -> Result<(), String> {
    println!("== 静态门禁 ==");
    check_layering()?;
    check_capsules()?;
    check_interrupt_priorities()?;
    check_lint_headers()?;
    check_unsafe()?;
    check_gaps()?;
    check_style()?;
    check_fault_handlers()?;
    check_critical()?;
    check_api_usage()?;
    check_docs()?;
    check_memory()?;
    trace()?;

    println!("\n== 格式与 lint ==");
    run("cargo", &["fmt", "--all", "--check"])?;
    lint_target()?;
    lint_host()?;

    println!("\n== 主机测试 ==");
    // 只测纯逻辑层：BSP/固件依赖 ARM 外设，不能在主机上运行（这正是分层要换来的东西）。
    test_host()?;

    println!("\n== 目标构建 ==");
    run("cargo", &["build", "--offline", "--release"])?;

    println!("\n== 产物级断言 ==");
    check_compile_fail()?;
    check_image()?;
    check_size()?;
    check_flash_map()?;
    // ⚠️ 这一条此前只在 `gates` 里跑，导致"唯一验收入口 verify 全绿"不等于
    // "CI 的 gates 全绿"（2026-10-19 审计点名）。放在目标构建之后读真 ELF。
    check_code_placement()?;

    println!("\n[xtask] verify 全绿");
    Ok(())
}
