//! 命令表与命令实现。
//!
//! # 为什么命令是"表 + 函数指针"而不是一长串 `match`
//!
//! `help`、Tab 补全、"未知命令"报错这三件事都需要**枚举全部命令**。用 `match`
//! 分发时，这三个功能各自维护一份命令名列表，于是"加了命令却忘了补进 help"
//! 这种缺陷迟早出现。表驱动把"有哪些命令"变成**一个**数据源：新增命令只改
//! [`COMMANDS`] 一处，`help` 与补全自动跟上。
//!
//! # 未实现项直接可见
//!
//! 终端**不假装成功**：没有真实实现的能力要么没有命令入口，要么返回
//! [`CommandStatus::NonZero`] 并说明去哪里看缺口清单。配合 `gaps` 命令，
//! 现场工程师不必翻文档就知道"这台设备现在能做什么、不能做什么"——
//! 这正是本项目对"未实现"的既定处理方式（见 `docs/架构与实现.md`）。
//!
//! # 为什么 `history` 要经过 [`HistorySource`]
//!
//! 历史存在行编辑器里（那是它的家：上下键要用），而命令表在 `commands` 里。
//! 与其让 `shell` 为 `history` 开一个特例分支（表里就少一条命令，补全和 `help`
//! 都会漏），不如抽一个只读视图 trait：命令表保持"每条命令都是表里一行"。

#[cfg(not(target_os = "none"))]
use realtime::Span;

use crate::ansi::{self, Tag, write_ignore};
use crate::shell::ShellContext;

/// 命令执行结果。
///
/// 目前只有"成功"与"非 0"两态：终端不需要更细的退出码，而 `shell` 需要知道
/// 是否要在提示符里体现失败（现场脚本化抓串口输出时需要一个可判定的标记）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// 成功。
    Ok,
    /// 非 0（未实现、参数错误等）。
    NonZero,
}

/// 心跳输出控制（[`ShellContext::beat_control`] 的参数）。
///
/// 默认**关闭**：终端就绪后不主动输出，只等待输入。
/// `bench` / `monitor` 需要长期观测量时显式 `beat on`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatAction {
    /// 开启持续输出（每秒一行）。
    On,
    /// 关闭持续输出（回到默认）。
    Off,
    /// 只打印一次当前快照。
    Now,
}

/// 故障注入类型（[`ShellContext::inject_fault`] 的参数）。
///
/// 只用于**台架自检**：验证"故障 → 记录 → 复位 → 下次启动自报"闭环。
/// 生产使用时应由构建配置裁剪；当前终端命令已把它限制为显式 `fault`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// 执行 `UDF`（UsageFault，按本平台的处理器配置会升级成 HardFault）。
    UsageFault,
}

/// 只读历史视图：让 `history` 命令不需要把编辑器搬进命令表。
pub trait HistorySource {
    /// 历史条数。
    fn history_len(&self) -> usize;
    /// 按序号取历史（0 = 最近一条）。
    fn history_get(&self, index: usize) -> Option<&str>;
}

/// 文件系统写队列的动作（[`ShellContext::fs_write_queue`] 的参数）。
///
/// 刻意不依赖 `filesystem` 的类型：`shell` 是纯逻辑 crate，
/// 而"写队列"是固件侧的能力。两者的接缝就是这四个变体。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsWriteAction<'a> {
    /// 整体写一个文件（覆盖语义）。
    Write {
        /// 文件名。
        name: &'a str,
        /// 内容。
        data: &'a [u8],
    },
    /// 追加（日志语义）。
    Append {
        /// 文件名。
        name: &'a str,
        /// 追加的内容。
        data: &'a [u8],
    },
    /// 删除（墓碑语义）。
    Remove {
        /// 文件名。
        name: &'a str,
    },
    /// 查队列统计。
    Stats,
}

/// 命令执行环境。
///
/// 三个字段都是借用：本 crate 零分配，命令既不能持有也不能复制它们。
pub struct CommandEnv<'a> {
    /// 固件实现的能力集合。
    pub ctx: &'a mut dyn ShellContext,
    /// 当前工作目录（`cd` 修改；路径参数相对它解析）。
    pub cwd: &'a mut crate::cwd::Cwd,
    /// 输出流。
    pub out: &'a mut dyn core::fmt::Write,
    /// 命令名之后的原始参数（保留内部空格，命令自己决定怎么切）。
    pub args: &'a str,
    /// 历史视图（只有 `history` 用得上）。
    pub history: &'a dyn HistorySource,
}

/// 命令处理函数。
type Handler = fn(&mut CommandEnv<'_>) -> CommandStatus;

/// 一条命令的静态描述。
pub struct Command {
    /// 命令名（全小写，大小写敏感）。
    pub name: &'static str,
    /// 别名，用 `|` 分隔（例如 `ver|release`）。
    pub aliases: &'static str,
    /// 用法（`help` 里展示）。
    pub usage: &'static str,
    /// 一行说明。
    pub desc: &'static str,
    handler: Handler,
}

impl Command {
    /// 该命令是否响应该名字（含别名）。
    ///
    /// 空 token 一律不匹配：`"".split('|')` 会产出一个空元素，若不挡住，
    /// 空命令名就会命中第一条"没有别名"的命令——空行会被执行成 `help`。
    #[must_use]
    pub fn matches(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        if self.name == token {
            return true;
        }
        self.aliases
            .split('|')
            .any(|alias| !alias.is_empty() && alias == token)
    }

    /// 别名列表（`help` 里提示"还能怎么敲"）。
    #[must_use]
    pub fn alias_list(&self) -> &'static str {
        self.aliases
    }

    /// 执行。
    pub fn run(&self, env: &mut CommandEnv<'_>) -> CommandStatus {
        (self.handler)(env)
    }
}

/// 命令表。
///
/// 顺序即 `help` 的展示顺序：按"现场最常用 → 最少用"排，而不是字母序——
/// 排障时最常敲的几条应当落在屏幕上最容易看到的位置。
pub static COMMANDS: &[Command] = &[
    Command {
        name: "mem",
        aliases: "pool",
        usage: "mem",
        desc: "分配池状态：占用/高水位/池满与拒绝计数（SDRAM 固定块池，不是通用堆）",
        handler: cmd_mem,
    },
    Command {
        name: "colors",
        aliases: "colour",
        usage: "colors",
        desc: "调色板自测",
        handler: cmd_colors,
    },
    Command {
        name: "help",
        aliases: "?",
        usage: "help",
        desc: "列出全部命令",
        handler: cmd_help,
    },
    Command {
        name: "status",
        aliases: "stat",
        usage: "status",
        desc: "打印固件自报的逐行状态",
        handler: cmd_status,
    },
    Command {
        name: "memtest",
        // 别名原为 `mem`；`mem` 现在归**分配池**命令（池状态比自检更常查）。
        // 自检改用 `sdram`（它测的正是 SDRAM），避免两个命令抢同一个别名 ——
        // 这个冲突是 `命令表没有重复名字或别名` 那条测试当场抓到的。
        aliases: "sdram",
        usage: "memtest",
        desc: "SDRAM 自检（PASS/FAIL）",
        handler: cmd_memtest,
    },
    Command {
        name: "fs",
        aliases: "df",
        usage: "fs [scan|gc]",
        desc: "文件系统容量/空闲/文件数/磨损（默认不读介质；scan 做全盘存活统计）",
        handler: cmd_fs,
    },
    Command {
        name: "cd",
        aliases: "chdir",
        usage: "cd [目录]",
        desc: "切换当前工作目录（支持相对路径、. 与 ..；空参数回根）",
        handler: cmd_cd,
    },
    Command {
        name: "pwd",
        aliases: "",
        usage: "pwd",
        desc: "打印当前工作目录",
        handler: cmd_pwd,
    },
    Command {
        name: "ls",
        aliases: "dir",
        usage: "ls [绝对路径]",
        desc: "列出目录：文件给字节数，子目录带 /（默认 /）",
        handler: cmd_ls,
    },
    Command {
        name: "cat",
        aliases: "",
        usage: "cat <绝对路径>",
        desc: "以十六进制+ASCII 打印文件内容（分块读，不整文件装内存）",
        handler: cmd_cat,
    },
    Command {
        name: "gzip",
        aliases: "",
        usage: "gzip <绝对路径>",
        desc: "gzip 成新的 generation 归档（/log/archive.<gen>.gz）",
        handler: cmd_gzip,
    },
    Command {
        name: "gunzip",
        aliases: "zcat",
        usage: "gunzip <绝对路径.gz>",
        desc: "解压 gzip 到 /log/unpacked.log",
        handler: cmd_gunzip,
    },
    Command {
        name: "wc",
        aliases: "",
        usage: "wc <绝对路径>",
        desc: "统计文件字节数与行数",
        handler: cmd_wc,
    },
    Command {
        name: "head",
        aliases: "",
        usage: "head [-n N] <绝对路径>",
        desc: "输出文件头部（日志按文本着色）",
        handler: cmd_head,
    },
    Command {
        name: "tail",
        aliases: "",
        usage: "tail [-n N] <绝对路径>",
        desc: "输出文件尾部（日志按文本着色）",
        handler: cmd_tail,
    },
    Command {
        name: "fstat",
        aliases: "fileinfo",
        usage: "fstat <绝对路径>",
        desc: "打印文件类型与字节数（status 的别名 stat 已占用）",
        handler: cmd_stat,
    },
    Command {
        name: "rm",
        aliases: "del",
        usage: "rm <绝对路径>",
        desc: "删除文件（投递墓碑请求，由 idle 写队列执行）",
        handler: cmd_rm,
    },
    Command {
        name: "wq",
        aliases: "writeq",
        usage: "wq [stat|write|append|remove] ...",
        desc: "文件系统写队列：投递写/追加/删除请求（由 idle 循环执行）或查统计",
        handler: cmd_wq,
    },
    Command {
        name: "log",
        aliases: "",
        usage: "log [stats|flush|clear|verify|rotate|restore [n]|grep <子串>]",
        desc: "文本日志状态（环/落盘/归档）",
        handler: cmd_log,
    },
    Command {
        name: "rt",
        aliases: "lat",
        usage: "rt [reset]",
        desc: "实时预算：关中断 / ISR / 终端输入 / 存储访问 的 last/max/mean/p99（µs）与余量",
        handler: cmd_rt,
    },
    Command {
        name: "free",
        aliases: "",
        usage: "free",
        desc: "时钟频率与运行时间",
        handler: cmd_free,
    },
    Command {
        name: "uptime",
        aliases: "",
        usage: "uptime",
        desc: "已运行时间",
        handler: cmd_uptime,
    },
    Command {
        name: "version",
        aliases: "ver",
        usage: "version",
        desc: "固件版本",
        handler: cmd_version,
    },
    Command {
        name: "uname",
        aliases: "",
        usage: "uname -a",
        desc: "系统信息（-a 全量）",
        handler: cmd_uname,
    },
    Command {
        name: "whoami",
        aliases: "",
        usage: "whoami",
        desc: "当前用户名",
        handler: cmd_whoami,
    },
    Command {
        name: "hostname",
        aliases: "",
        usage: "hostname",
        desc: "设备名",
        handler: cmd_hostname,
    },
    Command {
        name: "history",
        aliases: "hist",
        usage: "history",
        desc: "最近敲过的命令",
        handler: cmd_history,
    },
    Command {
        name: "echo",
        aliases: "",
        usage: "echo <文本>",
        desc: "回显参数（脚本用）",
        handler: cmd_echo,
    },
    Command {
        name: "clear",
        aliases: "cls",
        usage: "clear",
        desc: "清屏",
        handler: cmd_clear,
    },
    Command {
        name: "gaps",
        aliases: "",
        usage: "gaps",
        desc: "未实现项清单（按里程碑）",
        handler: cmd_gaps,
    },
    Command {
        name: "mkdir",
        aliases: "",
        usage: "mkdir <路径>",
        desc: "创建目录",
        handler: cmd_mkdir,
    },
    Command {
        name: "rmdir",
        aliases: "",
        usage: "rmdir <路径>",
        desc: "删除空目录",
        handler: cmd_rmdir,
    },
    Command {
        name: "rename",
        aliases: "mv",
        usage: "rename <旧路径> <新路径>",
        desc: "改名/移动目录项",
        handler: cmd_rename,
    },
    Command {
        name: "ln",
        aliases: "link",
        usage: "ln <已有文件> <新路径>",
        desc: "创建硬链接",
        handler: cmd_ln,
    },
    Command {
        name: "unlink",
        aliases: "",
        usage: "unlink <路径>",
        desc: "删除文件/空目录项",
        handler: cmd_unlink,
    },
    Command {
        name: "reboot",
        aliases: "reset",
        usage: "reboot",
        desc: "请求重启",
        handler: cmd_reboot,
    },
    Command {
        name: "flash",
        aliases: "",
        usage: "flash",
        desc: "台架用 Flash 基准：擦/写 boot_meta 扇区并统计耗时（会破坏该扇区）",
        handler: cmd_flash,
    },
    Command {
        name: "selftest",
        aliases: "",
        usage: "selftest",
        desc: "目标端硬件自检：逐条 test ... ok/FAILED，末尾 TEST-END",
        handler: cmd_selftest,
    },
    Command {
        name: "can",
        aliases: "",
        usage: "can",
        desc: "CAN 控制器状态（只读探针）",
        handler: cmd_can,
    },
    Command {
        name: "beat",
        aliases: "",
        usage: "beat [on|off]",
        desc: "心跳输出（默认关闭；不带参数=打印一次快照）",
        handler: cmd_beat,
    },
    Command {
        name: "diag",
        aliases: "diag",
        usage: "diag",
        desc: "启动期原始细节（PIT/FlexSPI/FlexCAN/MPU/DWT）",
        handler: cmd_diag,
    },
];

/// 缺口清单的出处：终端只复述文档结论，**不**在这里新建事实来源。
const GAP_DOC: &str = "docs/安全与约束.md";

/// 缺口标记的前缀（配合里程碑编号拼成现场熟悉的那个标记）。
const GAP_PREFIX: &str = "GAP";

/// 把命令行切成（命令名，参数）。
///
/// 规则：按空白切分、容忍连续空格与前导空格；命令名**大小写敏感**且全小写
/// （`LS` 是未知命令）。大小写敏感是刻意的：现场往往一边看文档一边敲，
/// 而"敲错大小写却执行了另一个命令"比"报错"危险得多。
#[must_use]
pub fn parse(line: &str) -> (&str, &str) {
    let trimmed = line.trim_start();
    match trimmed.find(char::is_whitespace) {
        Some(index) => {
            let name = trimmed.get(..index).unwrap_or("");
            let rest = trimmed.get(index..).unwrap_or("");
            (name, rest.trim_start())
        }
        None => (trimmed, ""),
    }
}

/// 按名字（含别名）查命令。
#[must_use]
pub fn lookup(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.matches(name))
}

/// 遍历所有以 `prefix` 开头的命令（含别名匹配）。
///
/// 用回调而不是返回列表：本 crate 零分配，返回 `&[&Command]` 需要一块调用方
/// 提供的缓冲区，而调用方（补全）只关心"有几个、唯一那个是谁"。
pub fn for_each_match(prefix: &str, mut visit: impl FnMut(&'static Command)) {
    for command in COMMANDS {
        if command.name.starts_with(prefix) {
            visit(command);
        }
    }
}

/// 把补全文本（命令名 + 尾部空格）写进调用方的缓冲区。
///
/// 为什么需要尾部空格：补全成 `help` 之后用户接着要敲参数，多一个空格就少一次
/// 按键。缓冲区可能太小，此时退化为命令名本身（宁可不补空格，也不能截断命令名）。
#[must_use]
pub fn completion_text<'a>(command: &Command, buffer: &'a mut [u8]) -> &'a str {
    let name = command.name.as_bytes();
    let needed = name.len().saturating_add(1);
    if buffer.len() < needed {
        return command.name;
    }
    for (slot, byte) in buffer.iter_mut().zip(name.iter()) {
        *slot = *byte;
    }
    if let Some(slot) = buffer.get_mut(name.len()) {
        *slot = b' ';
    }
    let end = needed.min(buffer.len());
    match buffer.get(..end) {
        Some(bytes) => core::str::from_utf8(bytes).unwrap_or(command.name),
        None => command.name,
    }
}

/// 未知命令的报错（红色）。
///
/// 文案取 `command not found: <name>`，**不**照抄 bash 的
/// `bash: <name>: command not found`：本项目明说"不追求与 bash 兼容"
/// （见 crate 文档），而把命令名放在冒号之后、错误短语之前，肉眼扫一串
/// 串口日志时更容易定位"是哪一个词敲错了"。
pub fn not_found(out: &mut dyn core::fmt::Write, name: &str) {
    write_ignore!(
        out,
        "{RED}command not found: {name}{RESET}\r\n",
        RED = ansi::RED,
        RESET = ansi::RESET
    );
}

// ---------------------------------------------------------------- 内置命令

/// `help`：命令表 + 一行说明。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_help(env: &mut CommandEnv<'_>) -> CommandStatus {
    let out = &mut *env.out;
    write_ignore!(
        out,
        "{}tessera shell{} —— 可用命令（Tab 可补全命令名）：\r\n",
        ansi::BOLD,
        ansi::RESET
    );
    for command in COMMANDS {
        // 有别名时把别名也列出来：现场排障常靠肌肉记忆敲 `?`、`stat` 这类短名。
        let aliases = command.alias_list();
        let alias_note = if aliases.is_empty() { "" } else { aliases };
        write_ignore!(
            out,
            "  {}{:<9}{} {:<16} {:<12} {}\r\n",
            ansi::GREEN,
            command.name,
            ansi::RESET,
            command.usage,
            alias_note,
            command.desc
        );
    }
    write_ignore!(
        out,
        "{}提示：Ctrl-A/E 行首行尾、Ctrl-U/K 删除、Ctrl-L 清屏、上下键翻历史。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    CommandStatus::Ok
}

/// `status`：逐行状态由固件填写（终端不知道有哪些传感器）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_status(env: &mut CommandEnv<'_>) -> CommandStatus {
    ansi::tag(env.out, Tag::Info);
    write_ignore!(
        env.out,
        " 设备状态（固件自报）{RESET}\r\n",
        RESET = ansi::RESET
    );
    env.ctx.status_lines(env.out);
    CommandStatus::Ok
}

/// `memtest`：跑固件的 SDRAM 自检并给出结论。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_selftest(env: &mut CommandEnv<'_>) -> CommandStatus {
    if !env.args.trim().is_empty() {
        write_ignore!(env.out, "用法：selftest（无参数）\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.selftest(env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "selftest: FAILED（见上面的 test 行）{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_flash(env: &mut CommandEnv<'_>) -> CommandStatus {
    if !env.args.trim().is_empty() {
        write_ignore!(env.out, "用法：flash（无参数）\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.flash_bench(env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "本实现不提供 Flash 基准{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_memtest(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "开始 SDRAM 自检，逐区段结果如下：\r\n");
    let passed = env.ctx.run_memtest(env.out);
    ansi::tag(env.out, if passed { Tag::Ok } else { Tag::Err });
    if passed {
        // 计数来自固件逐段输出（本终端只拿到总判定）：
        // 判 PASS 就是零错误，判 FAIL 时具体计数在上一段逐区段结果里。
        write_ignore!(
            env.out,
            " 自检：PASS，错误计数 0{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::Ok
    } else {
        write_ignore!(
            env.out,
            " 自检：FAIL，错误计数见上方逐区段结果{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// 单个块的擦写寿命参考值（次）。
///
/// 依据：本板外部 Flash 是 Winbond `W25Q256JVEQ`（型号与容量出处见
/// `docs/硬件与板级事实.md` §1「MCU 与存储器」）。该系列 NOR 的**最小**擦写
/// 循环规格量级为 10 万次/块。仓库里没有这份器件数据手册的全文，因此这里
/// **不引用页码**，按器件规格取整到便于现场心算的量级 —— 报告与输出里都写
/// 「按器件规格」，而不是伪造成一个实测值。
const ERASE_ENDURANCE_PER_BLOCK: u32 = 100_000;

/// 磨损告警阈值：最重的一块用掉寿命的 50% 就提示。
///
/// 取 50% 而不是 90%：擦除次数是**累积且不可逆**的，现场需要的是"还能提前处理"
/// 的提醒，而不是"已经到期"的通知。
const WEAR_WARN_PERCENT: u32 = 50;

/// "寿命用尽"阈值：最重的一块用掉寿命的 100% 就按器件规格判为用尽。
///
/// 这条阈值只有在擦除计数是 `u32` 时才**有可能**被触发：`u16` 的上限 65535
/// 只够表达 65.5%，计数到顶后静默饱和，于是这条判断永远不会成立
/// （这正是把计数加宽到 `u32` 的理由，见 `filesystem` 的 `layout` 模块文档）。
///
/// 为什么不与 [`WEAR_WARN_PERCENT`] 合并成一个"≥100% 用同一句话"：两者的**处置**
/// 不同 —— 50% 是"检查 GC 与写入模式"，100% 是"这块不可再依赖"。措辞不同，
/// 因此阈值也要分开命名，不能在格式化字符串里写死 100。
const WEAR_EXHAUSTED_PERCENT: u32 = 100;

/// 最重的一块用掉了寿命的百分之几（整数、向下取整）。
///
/// 用整数而不是浮点：本 crate 禁浮点（`clippy::float_arithmetic`），
/// 而诊断只需要量级判断。
///
/// 中间量用 `u64`：`erase_max` 现在是 `u32`，`u32 × 100` 在计数很大时会溢出
/// （`saturating_mul` 一旦饱和就会把"用了几十万次"显示成一个偏小的百分比 ——
/// 诊断命令宁可显示一个很大的百分数，也不能把它算小）。
fn wear_percent(erase_max: u32) -> u32 {
    let percent = u64::from(erase_max)
        .saturating_mul(100)
        .checked_div(u64::from(ERASE_ENDURANCE_PER_BLOCK))
        .unwrap_or(0);
    u32::try_from(percent).unwrap_or(u32::MAX)
}

/// 平均每块被擦了多少次（整数、向下取整）。
///
/// 块数为 0（几何非法，正常不会发生）时返回 0 —— 除零在这里是"没有数据"，
/// 不该让一条诊断命令挂掉。
fn wear_average(erase_total: u32, block_count: u32) -> u32 {
    erase_total.checked_div(block_count).unwrap_or(0)
}

/// 打印"文件系统不可用"。
///
/// 刻意**不**打印任何统计数字：排成零的表格会被现场读成"空文件系统、一切正常"，
/// 而真实情况恰恰相反 —— 应用数据此刻不可用。这是本命令存在的意义之一
/// （见 `docs/架构与实现.md` §4 的 F4）。
fn fs_unavailable(out: &mut dyn core::fmt::Write) {
    ansi::tag(out, Tag::Err);
    write_ignore!(
        out,
        " 文件系统不可用（未挂载成功，或正被其它上下文占用）{RESET}\r\n",
        RESET = ansi::RESET
    );
    write_ignore!(
        out,
        "{}不打印统计数字：排成全零的表格会被误读成“空文件系统，一切正常”。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    write_ignore!(
        out,
        "{}挂载失败的原因见启动横幅的 fs 行。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
}

/// `fs`：文件系统占用与 Flash 磨损统计。
///
/// # 为什么每个数字后面都要跟一句"它是什么"
///
/// 这是**日志结构 + GC** 的文件系统，容量口径与"桌面上的 df"不一样：
///
/// - `块数 × 块大小` 是**分给文件系统的介质容量**，不是"还能写多少"；
/// - `live_bytes` 是"当前可读文件的内容 + 记录头 + 名字"，不等于"已用空间"；
/// - `dead_bytes` 要经过一次 GC 才会变回可写空间。
///
/// 不加解释地把它们排成一行，现场很容易把"分区容量 27 MiB"读成"还有 27 MiB
/// 可用"，从而得出完全相反的结论。因此本命令逐项写明口径 —— 宁可多几行字，
/// 也不要一个会被误读的数字。
///
/// 磨损部分给的是**量级判断**（最重的一块用掉寿命的百分之几），并在过半时
/// 显眼提示、在达到器件规格寿命（10 万次/块）时明确判为"用尽"；不可用时照实说
/// "不可用"，不打印一排 0。
///
/// 擦除计数是 `u32`（`filesystem` 块头定义的版式）：`u16` 的上限 65535 只够
/// 表达 10 万次寿命的 65.5%，到顶后静默饱和，磨损百分比永远读不出"用尽"。
/// 计数加宽之后，"≥ 100%" 是一条**真能成立**的判断（阈值见
/// [`WEAR_EXHAUSTED_PERCENT`]）。
///
/// 回收次数按**显式**（`gc()`）与**自动**（写路径的 `collect_garbage()`）分开报：
/// 合并成一个数时，生产里恒为 0 的那一项会让整句话被读成"从未回收过"。
///
/// # 为什么"有效/可回收"两行可能是"未计算"
///
/// 固件取快照的实现在 `with_fs` 里（`critical_section::with`，即**关中断**），
/// 而 `live_bytes` / `dead_bytes` 只有在**扫描介质**（存活统计）之后才有值。
/// 主机实测（446 块 × 64 KiB、live 约 18 MiB + dead 约 9 MiB）一次全盘扫描读
/// 28.3 MB 介质、44.5 万次 `read`，折算到真机是 0.5 s 量级（计入每次访问开销后
/// 接近 2 s）；这块板子的 LPUART 接收 FIFO 只有 1 字节深，关中断这么久必然
/// 吃掉串口输入。
///
/// 因此固件改用**不读介质**的入口，并把快照的 `liveness_scanned` 置为 `false`。
/// 本命令的处置是照实说"需扫描，未计算" —— 显示成 0 会被现场读成"有效数据
/// 0 字节、一切正常"，那是编造数据。
fn resolve_path<'a>(
    env: &CommandEnv<'_>,
    raw: &str,
    default: Option<&str>,
    want_dir: bool,
    out: &'a mut [u8; filesystem::path::MAX_PATH_LEN],
) -> Option<&'a str> {
    let input = if raw.trim().is_empty() {
        default?
    } else {
        raw.trim()
    };
    if input.split_whitespace().nth(1).is_some() {
        return None;
    }
    env.cwd.resolve(input, out, want_dir)
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_cd(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let raw = env.args.trim();
    let Some(path) = resolve_path(env, raw, Some("/"), true, &mut buffer) else {
        write_ignore!(
            env.out,
            "用法：cd [目录]（绝对/相对路径，支持 . 与 ..）\r\n"
        );
        return CommandStatus::NonZero;
    };
    if !env.ctx.fs_is_dir(path) {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "cd: 目录不存在：{path}{RESET}\r\n",
            RESET = ansi::RESET
        );
        return CommandStatus::NonZero;
    }
    if !env.cwd.set(path) {
        return CommandStatus::NonZero;
    }
    CommandStatus::Ok
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_pwd(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "{}\r\n", env.cwd.as_str());
    CommandStatus::Ok
}

/// `ls [绝对路径]`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_ls(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = resolve_path(env, env.args, Some(env.cwd.as_str()), true, &mut buffer) else {
        write_ignore!(env.out, "用法：ls [目录]（绝对/相对路径）\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_list(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "ls: 路径不可用（不存在 / 非法 / 文件系统忙）{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// `cat <绝对路径>`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_cat(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = resolve_path(env, env.args, None, false, &mut buffer) else {
        write_ignore!(env.out, "用法：cat <路径>\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_cat(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "cat: 读取失败（不存在 / 非法 / 介质错误）{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// `stat <绝对路径>`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_stat(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = resolve_path(env, env.args, None, false, &mut buffer) else {
        write_ignore!(env.out, "用法：fstat <路径>\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_stat(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            "stat: 路径不存在或非法{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// `rm <绝对路径>`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_rm(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = resolve_path(env, env.args, None, false, &mut buffer) else {
        write_ignore!(env.out, "用法：rm <路径>\r\n");
        return CommandStatus::NonZero;
    };
    if env
        .ctx
        .fs_write_queue(FsWriteAction::Remove { name: path }, env.out)
    {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_mkdir(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = env.cwd.resolve(env.args.trim(), &mut buffer, true) else {
        write_ignore!(env.out, "用法：mkdir <路径>（绝对/相对）\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_mkdir(path, env.out) {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_rmdir(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = env.cwd.resolve(env.args.trim(), &mut buffer, true) else {
        write_ignore!(env.out, "用法：rmdir <路径>（绝对/相对）\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_rmdir(path, env.out) {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_rename(env: &mut CommandEnv<'_>) -> CommandStatus {
    let Some((old, new)) = two_args(env.args) else {
        write_ignore!(env.out, "用法：rename <旧路径> <新路径>（绝对/相对）\r\n");
        return CommandStatus::NonZero;
    };
    let mut old_buf = [0u8; filesystem::path::MAX_PATH_LEN];
    let mut new_buf = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(old) = env.cwd.resolve(old, &mut old_buf, false) else {
        write_ignore!(env.out, "rename: 旧路径非法\r\n");
        return CommandStatus::NonZero;
    };
    let Some(new) = env.cwd.resolve(new, &mut new_buf, false) else {
        write_ignore!(env.out, "rename: 新路径非法\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_rename(old, new, env.out) {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_ln(env: &mut CommandEnv<'_>) -> CommandStatus {
    let Some((existing, new)) = two_args(env.args) else {
        write_ignore!(env.out, "用法：ln <已有文件> <新路径>（绝对/相对）\r\n");
        return CommandStatus::NonZero;
    };
    let mut old_buf = [0u8; filesystem::path::MAX_PATH_LEN];
    let mut new_buf = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(existing) = env.cwd.resolve(existing, &mut old_buf, false) else {
        write_ignore!(env.out, "ln: 源路径非法\r\n");
        return CommandStatus::NonZero;
    };
    let Some(new) = env.cwd.resolve(new, &mut new_buf, false) else {
        write_ignore!(env.out, "ln: 新路径非法\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_link(existing, new, env.out) {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_unlink(env: &mut CommandEnv<'_>) -> CommandStatus {
    let mut buffer = [0u8; filesystem::path::MAX_PATH_LEN];
    let Some(path) = env.cwd.resolve(env.args.trim(), &mut buffer, false) else {
        write_ignore!(env.out, "用法：unlink <路径>（绝对/相对）\r\n");
        return CommandStatus::NonZero;
    };
    if env.ctx.fs_unlink(path, env.out) {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

fn two_args(args: &str) -> Option<(&str, &str)> {
    let mut parts = args.split_whitespace();
    let first = parts.next()?;
    let second = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some((first, second))
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_fs(env: &mut CommandEnv<'_>) -> CommandStatus {
    let args = env.args.trim();
    if !args.is_empty()
        && args != "scan"
        && args != "gc"
        && args != "checkpoint"
        && args != "cancel"
        && args != "index"
    {
        write_ignore!(
            env.out,
            "用法：fs [scan|gc|checkpoint|cancel|index]（scan 全盘存活统计；gc 后台回收；checkpoint 写索引；cancel 取消后台作业；index 查看本次是否走索引）\r\n"
        );
        return CommandStatus::NonZero;
    }
    // 先取快照、再打表头：不可用时只留一条明确的错误，不留"文件系统（…）"的表头 ——
    // 表头加错误行会被读成"这一节有内容"，而现场要的是"现在没有数据"。
    // 取快照：`None` = 未挂载成功或正被占用；两者对现场的含义都是"现在拿不到数据"。
    if args == "gc" {
        let ok = env.ctx.fs_gc(env.out);
        return if ok {
            CommandStatus::Ok
        } else {
            CommandStatus::NonZero
        };
    }
    if args == "checkpoint" {
        let ok = env.ctx.fs_checkpoint(env.out);
        return if ok {
            CommandStatus::Ok
        } else {
            CommandStatus::NonZero
        };
    }
    if args == "cancel" {
        return if env.ctx.fs_cancel(env.out) {
            CommandStatus::Ok
        } else {
            CommandStatus::NonZero
        };
    }
    if args == "index" {
        ansi::tag(env.out, Tag::Info);
        write_ignore!(
            env.out,
            "fs index: {}{RESET}\r\n",
            if env.ctx.fs_index_loaded() {
                "持久化索引（快速挂载）"
            } else {
                "全盘扫描（无可用索引）"
            },
            RESET = ansi::RESET
        );
        return CommandStatus::Ok;
    }
    let snapshot = if args == "scan" {
        env.ctx.fs_scan_snapshot()
    } else {
        env.ctx.fs_snapshot()
    };
    let Some(snapshot) = snapshot else {
        fs_unavailable(env.out);
        return CommandStatus::NonZero;
    };
    if !snapshot.mounted {
        // 快照自带的挂载位为假：与取不到同样照实说明（当前固件取到即已挂载，
        // 这条分支是给"实例在、分区未就绪"这类将来状态留的）。
        fs_unavailable(env.out);
        return CommandStatus::NonZero;
    }

    ansi::tag(env.out, Tag::Info);
    write_ignore!(
        env.out,
        " 文件系统（filesystem：日志结构 + GC）{RESET}\r\n",
        RESET = ansi::RESET
    );

    // 容量：块大小按字节给（`stats()` 的单位就是字节），总量再折算 KiB 便于人读。
    // 设备上块大小恒为 64 KiB，折算无损；用 u64 做乘法，避开 u32 溢出的边界。
    let partition_kib =
        u64::from(snapshot.block_count).saturating_mul(u64::from(snapshot.block_size)) / 1024;
    let free_kib =
        u64::from(snapshot.free_blocks).saturating_mul(u64::from(snapshot.block_size)) / 1024;
    write_ignore!(
        env.out,
        " 状态    : {GREEN}已挂载{RESET}\r\n",
        GREEN = ansi::GREEN,
        RESET = ansi::RESET
    );
    // 索引没和介质核对过：下面的数字**不是实测值**，是"上次已知"。
    // 与"未做存活统计"同一取舍：不把可能过期的数字当实测值沉默地打印。
    if snapshot.index_dirty {
        ansi::tag(env.out, Tag::Warn);
        write_ignore!(
            env.out,
            " 警告    : 内存索引尚未与介质核对（上一次写操作没走完）—— 下面的块数 / 文件数 / 磨损是“上次已知”值，可能已过期；\
再写一次或重启后本行会消失{RESET}\r\n",
            RESET = ansi::RESET
        );
    }
    write_ignore!(
        env.out,
        " 数据分区: {CYAN}{blocks} 块 × {size} B{RESET} ≈ {total} KiB —— 块数×块大小，是分给文件系统的介质容量，不是“当前可写入量”\r\n",
        blocks = snapshot.block_count,
        size = snapshot.block_size,
        total = partition_kib,
        CYAN = ansi::CYAN,
        RESET = ansi::RESET
    );
    write_ignore!(
        env.out,
        " 可写入量: 空闲 {CYAN}{free} 块{RESET} ≈ {free_kib} KiB（擦除态、可直接分配；写满后需 GC 回收才有新空间）\r\n",
        free = snapshot.free_blocks,
        free_kib = free_kib,
        CYAN = ansi::CYAN,
        RESET = ansi::RESET
    );
    write_ignore!(
        env.out,
        " 已用块  : {used} / {blocks}（含只装着死记录的块）\r\n",
        used = snapshot.used_blocks,
        blocks = snapshot.block_count
    );
    if snapshot.liveness_scanned {
        write_ignore!(
            env.out,
            " 数据占用: 有效 {CYAN}{live} B{RESET}（当前可读文件的内容 + 记录头 + 名字；不含空闲与死数据）\r\n",
            live = snapshot.live_bytes,
            CYAN = ansi::CYAN,
            RESET = ansi::RESET
        );
        write_ignore!(
            env.out,
            " 可回收  : 死数据 {CYAN}{dead} B{RESET}（已写过但已作废的字节：被覆盖的旧版本、墓碑、跳过的坏记录；需 GC 才能变回可写空间，不含块尾未用空间）\r\n",
            dead = snapshot.dead_bytes,
            CYAN = ansi::CYAN,
            RESET = ansi::RESET
        );
    } else {
        // 没做存活统计：**不打印数字**。`live`/`dead` 这时恒为 0，打出来会被
        // 读成"有效数据 0 字节、一切正常"——那是最坏的一种误导（真实情况可能
        // 是分区里满是数据）。这里也顺带把"为什么没有"当场说清，
        // 免得现场以为命令坏了。
        ansi::tag(env.out, Tag::Warn);
        write_ignore!(
            env.out,
            " 数据占用: {DIM}需扫描，未计算（这两个量要逐块读记录才能得出）{RESET}\r\n",
            DIM = ansi::DIM,
            RESET = ansi::RESET
        );
        write_ignore!(
            env.out,
            " 可回收  : {DIM}需扫描，未计算 —— 本命令在关中断的临界区里取数据，全盘扫描会让串口丢字；\
要这两个数请用不关中断的诊断通道{RESET}\r\n",
            DIM = ansi::DIM,
            RESET = ansi::RESET
        );
    }
    write_ignore!(
        env.out,
        " 文件数  : {CYAN}{files}{RESET} 个（不含墓碑）\r\n",
        files = snapshot.file_count,
        CYAN = ansi::CYAN,
        RESET = ansi::RESET
    );

    // 索引溢出非 0 才报：正常为 0，天天打一行 0 只会训练现场忽略这一行。
    if snapshot.table_overflow > 0 {
        ansi::tag(env.out, Tag::Warn);
        write_ignore!(
            env.out,
            " 索引溢出: {overflow} 个名字被丢弃 —— 有文件可能读不到，或读路径退化为全盘扫描{RESET}\r\n",
            overflow = snapshot.table_overflow,
            RESET = ansi::RESET
        );
    }

    let percent = wear_percent(snapshot.erase_max);
    let average = wear_average(snapshot.erase_total, snapshot.block_count);
    write_ignore!(
        env.out,
        " 磨损    : 最重一块 {CYAN}{max}{RESET} 次 / 最轻 {CYAN}{min}{RESET} 次 / 合计 {CYAN}{total}{RESET} 次（平均 {average} 次/块）\r\n",
        max = snapshot.erase_max,
        min = snapshot.erase_min,
        total = snapshot.erase_total,
        average = average,
        CYAN = ansi::CYAN,
        RESET = ansi::RESET
    );
    write_ignore!(
        env.out,
        " 寿命    : 最重一块已用 {percent}%（{max} / {endurance} 次，按器件规格）\r\n",
        percent = percent,
        max = snapshot.erase_max,
        endurance = ERASE_ENDURANCE_PER_BLOCK
    );

    if percent >= WEAR_EXHAUSTED_PERCENT {
        // 寿命用尽：这条判断要求擦除计数能表达到 10 万次以上 —— `u16` 时代
        // 计数到 65535 就饱和，因此这一段**永远不可能**被触发（这正是加宽到
        // `u32` 的理由）。
        ansi::tag(env.out, Tag::Warn);
        write_ignore!(
            env.out,
            " 警告    : 最重的一块已擦 {max} 次，达到/超过器件规格寿命（{endurance} 次，已用 {percent}%）—— 该块不可再依赖，请更换器件或调整写入模式{RESET}\r\n",
            max = snapshot.erase_max,
            endurance = ERASE_ENDURANCE_PER_BLOCK,
            percent = percent,
            RESET = ansi::RESET
        );
    } else if percent >= WEAR_WARN_PERCENT {
        ansi::tag(env.out, Tag::Warn);
        write_ignore!(
            env.out,
            " 警告    : 最重的一块已擦 {max} 次，用掉寿命的 {percent}%（≥ {threshold}%）—— 该块可能被反复擦写，请检查 GC 与写入模式{RESET}\r\n",
            max = snapshot.erase_max,
            percent = percent,
            threshold = WEAR_WARN_PERCENT,
            RESET = ansi::RESET
        );
    }

    // 回收次数：显式与自动**分开报**。
    //
    // 旧版只报显式 `gc()`，而生产固件走的是写路径的自动回收
    // （`collect_garbage()`）—— 于是这个数在生产里恒为 0，再配上一句
    // "自动回收不计入此计数"，字面很容易被读成"从未回收过"。
    // 拆成两个数之后，"两个都是 0"才是真的"从未回收过"。
    // 口径是**入口调用次数**，不是回收到的块数（一轮可能回收 0 个块）。
    write_ignore!(
        env.out,
        " {}回收    : 显式 gc() {explicit} 次 / 自动回收 {auto} 次（合计 {total} 次；自动回收由写操作在空间紧张时触发）{RESET}\r\n",
        ansi::DIM,
        explicit = snapshot.gc_explicit_runs,
        auto = snapshot.gc_auto_runs,
        total = snapshot
            .gc_explicit_runs
            .saturating_add(snapshot.gc_auto_runs),
        RESET = ansi::RESET
    );
    CommandStatus::Ok
}

/// 打印"取不到实时实测数据"。
///
/// 与 [`fs_unavailable`] 同一条纪律：**一个统计数字都不打印**。全零的表格会被现场
/// 读成"关中断几乎不占时间、预算很宽裕"，而真实情况恰恰相反 —— 这一项**根本没被
/// 测过**（周期计数器没开）。那是最坏的一种误导：它让人放心。
///
/// 文案里刻意**不出现阿拉伯数字**（"零"用汉字写），并有测试逐字钉住这一条：
/// 只要有人往这个分支里塞一个 `0`，"照实说不可用"就退化成"打印了一个数字"。
fn rt_unavailable(out: &mut dyn core::fmt::Write) {
    ansi::tag(out, Tag::Err);
    write_ignore!(
        out,
        " 实时实测数据取不到（周期计数器未启用，或本实现不提供该项）{RESET}\r\n",
        RESET = ansi::RESET
    );
    write_ignore!(
        out,
        "{}不打印统计数字：排成一张零表会被误读成“耗时极短、预算充裕”，而事实是这一项没被测过。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    write_ignore!(
        out,
        "{}测量入口：`board_support::rt::measured` / `measured_cs`；启动横幅的 `rt=` 列是同一份统计的结论。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
}

/// `rt reset`：请求固件清空实测统计（终端只发起，固件受理）。
///
/// 复位之后**不**顺手打表：刚清空的表每个 span 都是 `count` 零，正是
/// [`rt_unavailable`] 里说的那种会被误读的形状。空表要看，用户自己再敲一次 `rt`。
fn rt_reset(env: &mut CommandEnv<'_>) -> CommandStatus {
    if !env.ctx.rt_reset() {
        // 不受理（trait 的默认实现）时**照实说**，绝不打印"已复位"——
        // 那会让现场把上一次的统计当成复位后的新数据读（假装成功是本项目禁止的）。
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            " 本实现不支持复位实时统计（统计保持原样，未复位）{RESET}\r\n",
            RESET = ansi::RESET
        );
        return CommandStatus::NonZero;
    }
    write_ignore!(
        env.out,
        "已复位实时统计（预算保留）：`max` 从下一次测量重新开始累计。\r\n"
    );
    write_ignore!(
        env.out,
        "{}复位后立刻跑 `rt` 会看到一张没有样本的表，那含义是“还没测过”，不是“耗时为零”。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    CommandStatus::Ok
}

/// `rt`：实时性实测（关中断 / ISR / 终端输入 / 存储访问）。
///
/// # 这条命令回答什么
///
/// "纸面预算到底还成不成立"。`primitives::budget` 里的那些微秒值原本只是**预算**
/// （注释里老实标着"需 DWT 实测替换"），本命令把 `realtime` 收上来的**实测**
/// 最长耗时与预算余量摆出来，并先给一句机器可判的结论。
///
/// # 为什么第一行是结论，而不是先打表
///
/// 表格有 5 行 × 7 列，现场扫读时"到底有没有超"很容易被淹没在数字里。先把结论打在
/// 最上面（并用颜色区分），后面才是明细 —— 与 `fs` 把"已挂载"放在第一行同一个取舍。
///
/// # 口径（每条都影响怎么读那些数字）
///
/// - 时间单位是**微秒（µs）**（`report` 的列宽按此设计，表格自身不带单位）；
/// - `max` 是**自上次复位以来**的最大值，不是最近一次 —— 要观察某个具体动作，
///   先 `rt reset`；
/// - `count` 为零的行是**还没有样本**，其 `max`/`last` 列打印的 `0` 含义是"没测过"，
///   **不是**"耗时为零"（`realtime` 对均值/余量给的是 `-`，正是为了避开这个坑；
///   `report` 里 `last`/`max` 是纯数字列，所以本命令把这种行的**条数**单独说一句）；
/// - 超预算判据是**严格大于**预算（等于预算算合格）。
///
/// # 退出状态
///
/// 有 span 超预算时返回 [`CommandStatus::NonZero`]：与 `memtest` 判 FAIL 同一口径 ——
/// "命令跑通了"与"实时预算达成了"是两件事，脚本要能只看退出状态就分辨。
/// `mem` / `pool`：分配池状态。
///
/// # 为什么值得一条独立命令
///
/// 池的启动自检只在开机时打一行，**滚走就没了**。而"池到底够不够"这件事只有在
/// **真实负载跑起来之后**才能回答（高水位、池满次数、请求被拒次数都是累积量）。
/// 所以它必须随时可查。
///
/// # 口径
///
/// - `has_anomaly()` 为真（池满 / 请求被拒 / 释放被拒）⇒ 用 `ERR` 标签并把退出码置为非零。
///   **能打印出数字 ≠ 一切正常** —— 这是本终端反复强调的纪律（`rt` 命令同理）。
/// - 任何一项取不到就照实说"取不到"，**不打一行看起来正常的零**。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_mem(env: &mut CommandEnv<'_>) -> CommandStatus {
    #[cfg(not(target_os = "none"))]
    {
        cmd_mem_detailed(env)
    }
    #[cfg(target_os = "none")]
    {
        write_ignore!(
            env.out,
            "{}mem      : 目标端已裁剪（用启动报告/`fs` 查看池事实）{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
        CommandStatus::Ok
    }
}

#[cfg(not(target_os = "none"))]
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_mem_detailed(env: &mut CommandEnv<'_>) -> CommandStatus {
    let Some(report) = env.ctx.memory_report() else {
        write_ignore!(
            env.out,
            "{}mem: 取不到分配池状态（本实现不提供）{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
        return CommandStatus::NonZero;
    };

    ansi::tag(
        env.out,
        if report.has_anomaly() {
            Tag::Err
        } else {
            Tag::Ok
        },
    );
    write_ignore!(
        env.out,
        "{RESET}分配池：{} KiB（{} × {} B 固定块），非通用堆；超一块的请求按契约被拒\r\n",
        report.total_bytes / 1024,
        report.blocks,
        report.block_bytes,
        RESET = ansi::RESET
    );
    write_ignore!(
        env.out,
        "  占用    : {}/{} 块（{} KiB，{}%）｜高水位 {} 块（{}%）\r\n",
        report.used_blocks,
        report.blocks,
        report.used_bytes() / 1024,
        report.used_percent(),
        report.high_water_blocks,
        report.high_water_percent()
    );
    write_ignore!(
        env.out,
        "  异常    : 池满 {} 次｜请求被拒 {} 次｜释放被拒 {} 次\r\n",
        report.exhaustion_failures,
        report.rejected_layout,
        report.rejected_releases
    );
    // 大块池单独一行：它的消费者（gzip 压缩器状态等）是**常驻**的，
    // 所以"占了几块"本身就是要看的数，不能混进小块池的占用率里。
    write_ignore!(
        env.out,
        "  大块池  : {} KiB（{} × {} KiB 固定块）｜占用 {}/{} 块（高水位 {}）\r\n",
        u64::from(report.big_block_bytes) * u64::from(report.big_blocks) / 1024,
        report.big_blocks,
        report.big_block_bytes / 1024,
        report.big_used_blocks,
        report.big_blocks,
        report.big_high_water_blocks
    );
    if report.big_rejected_layout != 0 || report.big_rejected_releases != 0 {
        write_ignore!(
            env.out,
            "  大块池异常: 请求被拒 {} 次｜释放被拒 {} 次\r\n",
            report.big_rejected_layout,
            report.big_rejected_releases
        );
    }
    if report.empty {
        write_ignore!(
            env.out,
            "{}  现状    : 全空闲（无长期泄漏）{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
    } else {
        write_ignore!(
            env.out,
            "  现状    : 有 {} 块在用（长期不归还即为泄漏）\r\n",
            report.used_blocks
        );
    }

    if report.has_anomaly() {
        CommandStatus::NonZero
    } else {
        CommandStatus::Ok
    }
}

/// `wq` —— 文件系统**写队列**：投递写请求，或查队列统计。
///
/// # 为什么要有这条命令
///
/// 写请求的**执行者是 `idle` 循环**（见 `board-support::fs::step` 的文档）：一条请求
/// 可能花几十毫秒（擦块），必须在可被打断的上下文里跑。于是"投递"与"执行"是分开的，
/// 终端投递完立刻返回 —— 这条命令就是那个投递口，也是真机上第一次真正写 Flash 的入口。
///
/// 用法：
///
/// ```text
/// wq                         查队列统计
/// wq write  <名字> <文本>     整体写（覆盖）
/// wq append <名字> <文本>     追加
/// wq remove <名字>            删除
/// ```
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_wq(env: &mut CommandEnv<'_>) -> CommandStatus {
    match parse_wq(env.args.trim()) {
        Ok(action) => {
            if env.ctx.fs_write_queue(action, &mut *env.out) {
                CommandStatus::Ok
            } else {
                CommandStatus::NonZero
            }
        }
        Err(usage) => {
            write_ignore!(env.out, "{usage}\r\n");
            CommandStatus::NonZero
        }
    }
}

/// 解析 `wq` 的参数 —— **纯函数**，因此不需要任何上下文就能把参数处理测掉。
///
/// 返回 `Err(提示文本)` 时，提示文本就是**正确用法**（而不是"参数错误"四个字）。
/// 参数不猜：认不出的子命令、缺文件名、缺内容，各自给出对应的用法。
///
/// 用法：
///
/// ```text
/// wq                          查队列统计
/// wq stat                     同上
/// wq write  <名字> <文本>     整体写（覆盖）
/// wq append <名字> <文本>     追加
/// wq remove <名字>            删除
/// ```
///
/// `write`/`append` 的**内容保留内部空格**（与 `args` 的约定一致）：
/// `wq write a hello world` 写进去的是 `hello world`，不是 `hello`。
fn parse_wq(args: &str) -> Result<FsWriteAction<'_>, &'static str> {
    if args.is_empty() || args == "stat" {
        return Ok(FsWriteAction::Stats);
    }
    let (verb, tail) = match args.split_once(char::is_whitespace) {
        Some((verb, tail)) => (verb, tail.trim_start()),
        None => (args, ""),
    };
    match verb {
        "remove" | "rm" => {
            if tail.is_empty() {
                return Err("wq: remove 需要文件名（用法：wq remove <名字>）");
            }
            Ok(FsWriteAction::Remove { name: tail })
        }
        "write" | "append" => {
            let Some((name, data)) = tail.split_once(char::is_whitespace) else {
                return Err("wq: write/append 需要文件名与内容（用法：wq write <名字> <文本>）");
            };
            if name.is_empty() {
                return Err("wq: 文件名不能为空");
            }
            if verb == "write" {
                Ok(FsWriteAction::Write {
                    name,
                    data: data.as_bytes(),
                })
            } else {
                Ok(FsWriteAction::Append {
                    name,
                    data: data.as_bytes(),
                })
            }
        }
        _ => Err("wq: 用法为 wq [stat|write|append|remove]"),
    }
}

#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_rt(env: &mut CommandEnv<'_>) -> CommandStatus {
    let args = env.args.trim();
    if args == "reset" {
        return rt_reset(env);
    }
    if !args.is_empty() {
        write_ignore!(env.out, "rt: 仅支持 rt reset（用法：rt [reset]）\r\n");
        return CommandStatus::NonZero;
    }
    #[cfg(not(target_os = "none"))]
    {
        cmd_rt_detailed(env)
    }
    #[cfg(target_os = "none")]
    {
        rt_compact(env)
    }
}

/// 目标端精简 `rt`：不拉入整张表，只给预算结论与仪表健康度。
#[cfg(target_os = "none")]
fn rt_compact(env: &mut CommandEnv<'_>) -> CommandStatus {
    let Some(metrics) = env.ctx.rt_metrics() else {
        rt_unavailable(env.out);
        return CommandStatus::NonZero;
    };
    let violations = metrics.violations();
    ansi::tag(
        env.out,
        if violations.is_empty() {
            Tag::Ok
        } else {
            Tag::Err
        },
    );
    if violations.is_empty() {
        write_ignore!(
            env.out,
            " 实时预算：全部在预算内{RESET}\r\n",
            RESET = ansi::RESET
        );
    } else {
        write_ignore!(
            env.out,
            " ⚠️ 有 {} 项超预算（完整表已从目标端裁掉以省 ITCM）：{}{RESET}\r\n",
            violations.len(),
            violations,
            RESET = ansi::RESET
        );
    }
    let mut instrument_ok = false;
    match env.ctx.instrument_health() {
        Some(health) => {
            instrument_ok = health.dwt_alive;
            ansi::tag(env.out, if health.dwt_alive { Tag::Ok } else { Tag::Err });
            write_ignore!(
                env.out,
                "仪表    : DWT {state}；rearm={}；sleep={}{RESET}\r\n",
                health.dwt_rearms,
                health.sleeps,
                state = if health.dwt_alive {
                    "在走"
                } else {
                    "**已停**"
                },
                RESET = ansi::RESET
            );
        }
        None => {
            write_ignore!(
                env.out,
                "{}仪表    : 健康度取不到{RESET}\r\n",
                ansi::DIM,
                RESET = ansi::RESET
            );
        }
    }
    if violations.is_empty() && instrument_ok {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

#[cfg(not(target_os = "none"))]
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_rt_detailed(env: &mut CommandEnv<'_>) -> CommandStatus {
    let args = env.args.trim();
    if args == "reset" {
        return rt_reset(env);
    }
    if !args.is_empty() {
        // 只认 `reset`：其余参数不猜，直接给出正确用法（比"忽略参数装作成功"好）。
        write_ignore!(env.out, "rt: 仅支持 rt reset（用法：rt [reset]）\r\n");
        return CommandStatus::NonZero;
    }

    // 取快照：`None` = 周期计数器没开（或本实现不提供）。此时照实说"取不到"，
    // **不**打表 —— 一张全零的表会被读成"一切正常"，与事实相反。
    let Some(metrics) = env.ctx.rt_metrics() else {
        rt_unavailable(env.out);
        return CommandStatus::NonZero;
    };

    // 违规清单只算一次：结论行、明细、退出码三处都要用它。
    let violations = metrics.violations();
    ansi::tag(
        env.out,
        if violations.is_empty() {
            Tag::Ok
        } else {
            Tag::Err
        },
    );
    if violations.is_empty() {
        write_ignore!(
            env.out,
            " 实时预算：全部在预算内{RESET}\r\n",
            RESET = ansi::RESET
        );
    } else {
        write_ignore!(
            env.out,
            " ⚠️ 有 {} 项超预算{RESET}\r\n",
            violations.len(),
            RESET = ansi::RESET
        );
    }

    // 口径先行：单位 + `max` 的含义。这两条不写清，表里的数字就会被读错。
    write_ignore!(
        env.out,
        "{}单位：微秒（µs）；`max` 是自上次复位以来的最大值，不是最近一次。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    // 还没测过的行要单独点出来：它们的 max/last 列打印的是 0，而 0 与"很快"长得一样。
    // 只在真有这种行时打，免得天天出现一句没人看的提示。
    let unsampled = Span::ALL
        .iter()
        .copied()
        .filter(|span| metrics.stats(*span).count == 0)
        .count();
    if unsampled > 0 {
        write_ignore!(
            env.out,
            "{}注意：{unsampled} 项还没有样本（count=0），其 max/last 列的 0 含义是“没测过”，不是“耗时为零”。{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
    }

    // 表格由 `realtime` 打（固定列宽）：列格式与口径只在那边有一份，终端不复述。
    let _ = metrics.report(env.out);

    // 超预算时补一份明细：结论行只说"有几项超"，现场需要知道**是哪一项、超了多少**。
    if !violations.is_empty() {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            " 超预算明细：{violations}{RESET}\r\n",
            RESET = ansi::RESET
        );
    }

    // 仪表自身的健康度：**表格之后**必须回答"上面这些数字还可信吗"。
    //
    // 为什么不能省：真机上出现过周期计数器被外部清成 0 的情况（AGENTS.md 坑清单）。
    // 那时表里会继续显示**陈旧样本**、结论照样是"在预算内" —— 一张看起来健康的表配上
    // 一个已经停摆的计数器，比没有仪表更危险。所以这一段的措辞是**断言式**的：
    // 要么明确说"计数在走、没被清过"，要么明确说"取不到 / 已停"。
    // 仪表自身是否可信：`None`（本实现不提供）与"已停"都**不能**算合格 ——
    // 前者是"无法确认"，后者是"已确认不可信"，两者都不适合给出 `Ok`。
    let mut instrument_ok = false;
    match env.ctx.instrument_health() {
        Some(health) => {
            instrument_ok = health.dwt_alive;
            ansi::tag(env.out, if health.dwt_alive { Tag::Ok } else { Tag::Err });
            // 零分配（本 crate 无 `alloc`）：两个分支各写自己的文本，不拼字符串。
            if health.dwt_rearms == 0 {
                write_ignore!(
                    env.out,
                    "仪表    : 周期计数器 {state}；运行期未发生过清除；空闲睡眠 {} 次{RESET}\r\n",
                    health.sleeps,
                    state = if health.dwt_alive {
                        "计数在走"
                    } else {
                        "**已停**（本表的数字不可信）"
                    },
                    RESET = ansi::RESET
                );
            } else {
                write_ignore!(
                    env.out,
                    "仪表    : 周期计数器 {state}；运行期自动重装过 {} 次；空闲睡眠 {} 次{RESET}\r\n",
                    health.dwt_rearms,
                    health.sleeps,
                    state = if health.dwt_alive {
                        "计数在走"
                    } else {
                        "**已停**（本表的数字不可信）"
                    },
                    RESET = ansi::RESET
                );
            }
        }
        None => {
            write_ignore!(
                env.out,
                "{}仪表    : 健康度取不到（本实现不提供）—— 因此本表的数字**无法**被确认可信{RESET}\r\n",
                ansi::DIM,
                RESET = ansi::RESET
            );
        }
    }

    // 退出码判据：**超预算**或**仪表自身失效**都算失败。
    //
    // 后一条是测试逼出来的（`rt_计数器已停时必须说清不可信且判失败`）：先前这里只看
    // `violations`，于是"计数器已停、表里的数字不可信"的情况下仍然返回 `Ok` ——
    // 终端退出码是给脚本用的，那样等于告诉脚本"一切正常"。**能说清不等于判得动**。
    if violations.is_empty() && instrument_ok {
        CommandStatus::Ok
    } else {
        CommandStatus::NonZero
    }
}

/// `log`：文本日志状态。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_log(env: &mut CommandEnv<'_>) -> CommandStatus {
    let args = env.args.trim();
    match args {
        "" | "stats" => {
            if env.ctx.log_stats(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(env.out, "log: 日志不可用{RESET}\r\n", RESET = ansi::RESET);
                CommandStatus::NonZero
            }
        }
        "on" | "off" => {
            let enabled = args == "on";
            if env.ctx.log_set_terminal(enabled, env.out) {
                CommandStatus::Ok
            } else {
                CommandStatus::NonZero
            }
        }
        "level" => {
            if env.ctx.log_set_level("", env.out) {
                CommandStatus::Ok
            } else {
                CommandStatus::NonZero
            }
        }
        _ if args.starts_with("level ") => {
            let level = args.trim_start_matches("level ").trim();
            if env.ctx.log_set_level(level, env.out) {
                CommandStatus::Ok
            } else {
                write_ignore!(env.out, "用法：log level [e|w|i|d]\r\n");
                CommandStatus::NonZero
            }
        }
        "dump" => {
            // 空子串匹配所有行；复用已验证的流式读取/着色路径。
            if env.ctx.log_grep("", env.out) {
                CommandStatus::Ok
            } else {
                CommandStatus::NonZero
            }
        }
        "prev" => {
            if env.ctx.log_previous(env.out) {
                CommandStatus::Ok
            } else {
                CommandStatus::NonZero
            }
        }
        _ if args.starts_with("grep ") => {
            let pattern = args.trim_start_matches("grep ").trim();
            if pattern.is_empty() {
                write_ignore!(env.out, "用法：log grep <子串>\r\n");
                return CommandStatus::NonZero;
            }
            if env.ctx.log_grep(pattern, env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(
                    env.out,
                    "log grep: 读取失败{RESET}\r\n",
                    RESET = ansi::RESET
                );
                CommandStatus::NonZero
            }
        }
        "verify" => {
            if env.ctx.log_verify(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(
                    env.out,
                    "log verify: 有坏归档{RESET}\r\n",
                    RESET = ansi::RESET
                );
                CommandStatus::NonZero
            }
        }
        "restore" => {
            if env.ctx.log_restore(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(env.out, "log restore: 失败{RESET}\r\n", RESET = ansi::RESET);
                CommandStatus::NonZero
            }
        }
        _ if args.starts_with("restore ") => {
            let count = args.trim_start_matches("restore ").trim();
            match count.parse::<usize>() {
                Ok(count) if count > 0 => {
                    if env.ctx.log_restore_count(Some(count), env.out) {
                        CommandStatus::Ok
                    } else {
                        ansi::tag(env.out, Tag::Err);
                        write_ignore!(env.out, "log restore: 失败{RESET}\r\n", RESET = ansi::RESET);
                        CommandStatus::NonZero
                    }
                }
                _ => {
                    write_ignore!(
                        env.out,
                        "用法：log restore [n]（n 为正整数，省略表示全部）\r\n"
                    );
                    CommandStatus::NonZero
                }
            }
        }
        "rotate" => {
            if env.ctx.log_rotate(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(
                    env.out,
                    "log rotate: 不支持或失败{RESET}\r\n",
                    RESET = ansi::RESET
                );
                CommandStatus::NonZero
            }
        }
        "flush" => {
            if env.ctx.log_flush(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(
                    env.out,
                    "log flush: 不支持或失败{RESET}\r\n",
                    RESET = ansi::RESET
                );
                CommandStatus::NonZero
            }
        }
        "clear" => {
            if env.ctx.log_clear(env.out) {
                CommandStatus::Ok
            } else {
                ansi::tag(env.out, Tag::Err);
                write_ignore!(
                    env.out,
                    "log clear: 不支持或失败{RESET}\r\n",
                    RESET = ansi::RESET
                );
                CommandStatus::NonZero
            }
        }
        _ => {
            write_ignore!(
                env.out,
                "用法：log [stats|on|off|level [e|w|i|d]|dump|prev|flush|clear|verify|rotate|restore [n]|grep <子串>]\r\n"
            );
            CommandStatus::NonZero
        }
    }
}

/// `gzip`：把文件以 gzip 容器追加到 `/log/archive.gz`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_gzip(env: &mut CommandEnv<'_>) -> CommandStatus {
    let path = env.args.trim();
    if path.is_empty() || !path.starts_with('/') {
        write_ignore!(env.out, "用法：gzip <绝对路径>\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.gzip_file(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(env.out, "gzip: 失败{RESET}\r\n", RESET = ansi::RESET);
        CommandStatus::NonZero
    }
}

/// `gunzip`：解压 gzip 到 `/log/unpacked.log`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_gunzip(env: &mut CommandEnv<'_>) -> CommandStatus {
    let path = env.args.trim();
    if path.is_empty() || !path.starts_with('/') {
        write_ignore!(env.out, "用法：gunzip <绝对路径.gz>\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.gunzip_file(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(env.out, "gunzip: 解压失败{RESET}\r\n", RESET = ansi::RESET);
        CommandStatus::NonZero
    }
}

/// `wc`：统计文件字节数与行数。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_wc(env: &mut CommandEnv<'_>) -> CommandStatus {
    let path = env.args.trim();
    if path.is_empty() || !path.starts_with('/') {
        write_ignore!(env.out, "用法：wc <绝对路径>\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.wc_file(path, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(env.out, "wc: 读取失败{RESET}\r\n", RESET = ansi::RESET);
        CommandStatus::NonZero
    }
}

/// 解析 `-n N <路径>`；行数限制在 1..=20，避免终端一次刷太多。
fn parse_lines_path(args: &str) -> (usize, &str) {
    let args = args.trim();
    if let Some(rest) = args.strip_prefix("-n ")
        && let Some((number, path)) = rest.split_once(' ')
        && let Ok(lines) = number.parse::<usize>()
    {
        let lines = lines.clamp(1, 20);
        let path = path.trim();
        if !path.is_empty() {
            return (lines, path);
        }
    }
    (20, args)
}

/// `head`：输出文件头部。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_head(env: &mut CommandEnv<'_>) -> CommandStatus {
    let (lines, path) = parse_lines_path(env.args);
    if path.is_empty() || !path.starts_with('/') {
        write_ignore!(env.out, "用法：head [-n N] <绝对路径>\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.head_file(path, lines, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(env.out, "head: 读取失败{RESET}\r\n", RESET = ansi::RESET);
        CommandStatus::NonZero
    }
}

/// `tail`：输出文件尾部。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_tail(env: &mut CommandEnv<'_>) -> CommandStatus {
    let (lines, path) = parse_lines_path(env.args);
    if path.is_empty() || !path.starts_with('/') {
        write_ignore!(env.out, "用法：tail [-n N] <绝对路径>\r\n");
        return CommandStatus::NonZero;
    }
    if env.ctx.tail_file(path, lines, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(env.out, "tail: 读取失败{RESET}\r\n", RESET = ansi::RESET);
        CommandStatus::NonZero
    }
}

/// `free`：时钟频率与运行时间（本项目没有堆，"内存占用"不可报）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_free(env: &mut CommandEnv<'_>) -> CommandStatus {
    let hz = env.ctx.clock_hz();
    let uptime = env.ctx.uptime_ms();
    // 频率同时给 Hz 与 MHz：`clock_hz` 是实测值，MHz 只是便于人读，
    // 不做四舍五入（终端不该在诊断数值上引入额外假设）。
    write_ignore!(
        env.out,
        "内核时钟：{}{hz}{RESET} Hz（{} MHz）\r\n",
        ansi::CYAN,
        hz / 1_000_000,
        RESET = ansi::RESET
    );
    write_ignore!(
        env.out,
        "运行时间：{}{uptime}{RESET} ms（{}）\r\n",
        ansi::CYAN,
        human_duration(uptime),
        RESET = ansi::RESET
    );
    // 零堆分配，因此没有可报的"空闲内存"；报"内存自检"入口比报假数字诚实。
    write_ignore!(
        env.out,
        "{}内存：零堆分配（静态布局），可用 `memtest` 校验 SDRAM。{RESET}\r\n",
        ansi::DIM,
        RESET = ansi::RESET
    );
    CommandStatus::Ok
}

/// `uptime`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_uptime(env: &mut CommandEnv<'_>) -> CommandStatus {
    let uptime = env.ctx.uptime_ms();
    write_ignore!(
        env.out,
        "已运行 {}（{uptime} ms）\r\n",
        human_duration(uptime)
    );
    CommandStatus::Ok
}

/// `version`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_version(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(
        env.out,
        "{} {}\r\n",
        crate::shell::PRODUCT,
        env.ctx.version()
    );
    CommandStatus::Ok
}

/// `uname -a`：与 `version` 同源，外加主机名/用户/build 口径。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_uname(env: &mut CommandEnv<'_>) -> CommandStatus {
    if env.args.trim() != "-a" {
        // 只认 `-a`：其余参数组合不猜，直接给出正确用法（比"忽略参数装作成功"好）。
        write_ignore!(env.out, "uname: 仅支持 -a（用法：uname -a）\r\n");
        return CommandStatus::NonZero;
    }
    // 刻意不报"机器架构"：本 crate 是纯逻辑层，看不到芯片型号，硬写
    // `cortex-m7` 会在换芯片时变成一句假话。架构由固件在 `version` 里自报。
    write_ignore!(
        env.out,
        "{} {} {} {}\r\n",
        crate::shell::PRODUCT,
        env.ctx.version(),
        env.ctx.host(),
        env.ctx.user()
    );
    CommandStatus::Ok
}

/// `whoami`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_whoami(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "{}\r\n", env.ctx.user());
    CommandStatus::Ok
}

/// `hostname`。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_hostname(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "{}\r\n", env.ctx.host());
    CommandStatus::Ok
}

/// `history`：0 号是最新一条（与上下键的上翻顺序一致）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_history(env: &mut CommandEnv<'_>) -> CommandStatus {
    let count = env.history.history_len();
    if count == 0 {
        write_ignore!(
            env.out,
            "{}（历史为空）{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
        return CommandStatus::Ok;
    }
    // 编号从 1 开始、越老编号越大：与 `history` 在桌面 shell 里的直觉一致，
    // 而"取历史"的 API 用 0 = 最近，这里的换算只发生在这个函数里。
    for index in 0..count {
        let number = count.saturating_sub(index);
        match env.history.history_get(index) {
            Some(text) => write_ignore!(env.out, "  {number:>3}  {text}\r\n"),
            None => continue,
        }
    }
    CommandStatus::Ok
}

/// `echo <...>`：回显参数原文（保留内部空格，供串口脚本做标记）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_echo(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "{}\r\n", env.args);
    CommandStatus::Ok
}

/// `colors`：打印调色板，用于确认终端确实支持 ANSI（现场换过终端软件时要用）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_colors(env: &mut CommandEnv<'_>) -> CommandStatus {
    #[cfg(not(target_os = "none"))]
    {
        cmd_colors_detailed(env)
    }
    #[cfg(target_os = "none")]
    {
        write_ignore!(
            env.out,
            "{}colors   : 目标端已裁剪（调色板自测仅主机测试）{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
        CommandStatus::Ok
    }
}

#[cfg(not(target_os = "none"))]
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_colors_detailed(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "调色板自测（每段末尾复位）：\r\n");
    for (name, code) in [
        ("RED", ansi::RED),
        ("GREEN", ansi::GREEN),
        ("YELLOW", ansi::YELLOW),
        ("BLUE", ansi::BLUE),
        ("MAGENTA", ansi::MAGENTA),
        ("CYAN", ansi::CYAN),
        ("BOLD", ansi::BOLD),
        ("DIM", ansi::DIM),
    ] {
        write_ignore!(
            env.out,
            "  {code}{name:<8}{RESET} ███\r\n",
            RESET = ansi::RESET
        );
    }
    write_ignore!(
        env.out,
        "如果上面几行没有颜色，说明当前终端/串口软件不认 ANSI：\
         其余命令的文字仍然可读。\r\n"
    );
    CommandStatus::Ok
}

/// `clear`：清屏并把光标放到左上角。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_clear(env: &mut CommandEnv<'_>) -> CommandStatus {
    write_ignore!(env.out, "{}", ansi::CLEAR_SCREEN);
    CommandStatus::Ok
}

/// `gaps`：本项目的未实现项清单。
///
/// 内容与 `docs/架构与实现.md` 对齐：
/// 这里**只是复述**，加一条新缺口要改的是那两份文档与固件里的标记注释，
/// 而不是这个数组。这么做是为了避免"终端自报的缺口清单"变成第二份事实来源，
/// 两份清单迟早会互相矛盾。
///
/// # 为什么数组里存 `"P2"` 而不是 `"GAP(P2)"`
///
/// 打印出来仍然是 `GAP(P2)`（与源码标记同形，现场可 grep）。但**字面量**刻意
/// 拆成"里程碑 + 文本"：`cargo xtask gate gaps` 是按行搜索 `GAP(` 清点的，
/// 如果这里也写出完整标记，同一条缺口会被数两遍（实现点一次、复述一次），
/// 于是门禁报出的"未实现项总数"就不再是"缺口个数"——而那个数字正是它存在的
/// 意义。终端要展示的是**结论**，不该污染清点口径。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_gaps(env: &mut CommandEnv<'_>) -> CommandStatus {
    const GAPS: &[(&str, &str)] = &[
        (
            "P2",
            "CAN 接收路径未在真机验证通过（loopback 停在 stage=7；未接入 Bus 端口契约）",
        ),
        (
            "P3",
            "日志落盘通道未接线（记录/压缩/写队列各自就绪，调用链未接）",
        ),
        ("P4", "A/B 双槽升级与引导程序（分区已预留，切换逻辑未实现）"),
        (
            "P5",
            "看门狗启用（外部 SGM706 的 WDG_CTR 驱动源未确认，当前既不能开也不能关）",
        ),
    ];
    write_ignore!(
        env.out,
        "{BOLD}未实现项清单{RESET}（与 {GAP_DOC} 对齐，共 {} 条）：\r\n",
        GAPS.len(),
        BOLD = ansi::BOLD,
        RESET = ansi::RESET
    );
    for (milestone, text) in GAPS {
        // 每条一行、带缺口标记：与源码里的标记同形，便于现场 grep 串起来。
        // 前缀来自 [`GAP_PREFIX`] 而不是写在格式串里，理由见上面的"为什么
        // 数组里存 `"P2"`"——保持 `check-gaps` 只清点实现点。
        write_ignore!(
            env.out,
            "  {MAGENTA}{GAP_PREFIX}({milestone}){RESET} {text}\r\n",
            MAGENTA = ansi::MAGENTA,
            RESET = ansi::RESET
        );
    }
    write_ignore!(
        env.out,
        "这些项在各自的实现点都带缺口标记，`cargo xtask gate gaps` 会清点。\r\n"
    );
    CommandStatus::Ok
}

/// `reboot`：只发起请求，真正复位由固件决定（终端不该自己去碰 SCB）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_reboot(env: &mut CommandEnv<'_>) -> CommandStatus {
    env.ctx.request_reboot();
    write_ignore!(env.out, "已请求重启（由固件执行）。\r\n");
    CommandStatus::Ok
}

/// `can`：打印 CAN 控制器的**只读**状态（由固件提供）。
///
/// 为什么这条命令值得存在：CAN 是平台上第一个"自研寄存器级驱动"，它的探针读数
/// （`MCR`/`ECR`/`ESR1`/`IFLAG1`、PE 时钟与实际波特率）是现场排"总线静默"的
/// 第一手材料。终端只负责显示，读寄存器在固件一侧完成（见
/// [`ShellContext::can_status`]）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_can(env: &mut CommandEnv<'_>) -> CommandStatus {
    if env.ctx.can_status(env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            " 本实现不提供 CAN 状态（这不等于「没有控制器」——以启动日志为准）{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// `beat [on|off]`：心跳输出控制（默认关闭）。
///
/// 进入终端后**不主动输出**；`bench`/`monitor` 用 `beat on` 显式打开，
/// 现场排障可以 `beat` 打印一次快照而不改变持续输出状态。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_beat(env: &mut CommandEnv<'_>) -> CommandStatus {
    let action = match env.args.trim() {
        "" => BeatAction::Now,
        "on" => BeatAction::On,
        "off" => BeatAction::Off,
        other => {
            write_ignore!(
                env.out,
                "beat: 未知参数 {other:?}（用法：beat [on|off]）\r\n"
            );
            return CommandStatus::NonZero;
        }
    };
    if env.ctx.beat_control(action, env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            " 本实现不支持心跳控制{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// `diag`：打印启动期的**原始细节**（按需查看，不放进启动报告）。
#[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
fn cmd_diag(env: &mut CommandEnv<'_>) -> CommandStatus {
    if env.ctx.diag_lines(env.out) {
        CommandStatus::Ok
    } else {
        ansi::tag(env.out, Tag::Err);
        write_ignore!(
            env.out,
            " 本实现不提供启动期细节{RESET}\r\n",
            RESET = ansi::RESET
        );
        CommandStatus::NonZero
    }
}

/// 把毫秒换算成"人读"的时长。
///
/// 用整数除法而不是浮点：本 crate 禁浮点（`clippy::float_arithmetic`），
/// 而且诊断数值不需要小数精度。天数也一并给出：现场设备可能连续跑几周，
/// 只报小时会让人做心算。
fn human_duration(ms: u64) -> Duration {
    // 返回一个实现了 `Display` 的小类型，而不是 `heapless::String`：
    // 零依赖、零分配，且格式化结果直接写进输出流。
    Duration { ms }
}

/// 可显示时长（见 [`human_duration`]）。
struct Duration {
    ms: u64,
}

impl core::fmt::Display for Duration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let total_seconds = self.ms / 1_000;
        let millis = self.ms % 1_000;
        let days = total_seconds / 86_400;
        let hours = (total_seconds % 86_400) / 3_600;
        let minutes = (total_seconds % 3_600) / 60;
        let seconds = total_seconds % 60;
        if days > 0 {
            write!(f, "{days}d {hours:02}:{minutes:02}:{seconds:02}")?;
        } else {
            write!(f, "{hours:02}:{minutes:02}:{seconds:02}")?;
        }
        // 毫秒也报出来：`uptime` 的一个用途是核对 1 ms 时基有没有丢中断，
        // 只到秒的粒度看不出这件事。
        write!(f, ".{millis:03}")
    }
}

#[cfg(test)]
mod tests {

    use super::{
        CommandStatus, HistorySource, completion_text, for_each_match, lookup, not_found, parse,
    };

    /// 只读历史替身：给 `commands` 的测试提供可控的历史视图。
    struct Hist(&'static [&'static str]);

    impl HistorySource for Hist {
        fn history_len(&self) -> usize {
            self.0.len()
        }
        fn history_get(&self, index: usize) -> Option<&str> {
            self.0.get(index).copied()
        }
    }

    #[test]
    fn 解析按空白切分且容忍多空格() {
        assert_eq!(parse("help"), ("help", ""));
        assert_eq!(parse("echo hello world"), ("echo", "hello world"));
        // 连续空格与前导空格都要容忍：串口终端上多按一个空格太常见。
        assert_eq!(parse("  echo   a  b  "), ("echo", "a  b  "));
        assert_eq!(parse("uname -a"), ("uname", "-a"));
        // 只有空白：命令名为空。
        assert_eq!(parse("   "), ("", ""));
    }

    #[test]
    fn 查表命中命令名与别名() {
        assert_eq!(lookup("help").map(|c| c.name), Some("help"));
        assert_eq!(lookup("?").map(|c| c.name), Some("help"));
        assert_eq!(lookup("stat").map(|c| c.name), Some("status"));
        assert_eq!(lookup("reset").map(|c| c.name), Some("reboot"));
        // 大小写敏感：`HELP` 不是 `help`（避免"敲错大小写却执行了别的东西"）。
        assert!(lookup("HELP").is_none());
        assert!(lookup("Help").is_none());
        assert!(lookup("nope").is_none());
        // 空串不该匹配任何命令（否则空行也会被执行）。
        assert!(lookup("").is_none());
    }

    #[test]
    fn 补全前缀匹配命中别名不重复() {
        let mut names = Vec::new();
        for_each_match("he", |command| names.push(command.name));
        assert_eq!(names, vec!["help", "head"]);
        // `c` 前缀能同时命中 can / colors / clear —— 多匹配是"打印候选"的触发条件。
        let mut names = Vec::new();
        for_each_match("c", |command| names.push(command.name));
        assert!(names.len() >= 3, "{names:?}");
        assert!(names.contains(&"clear"), "{names:?}");
        assert!(names.contains(&"can"), "{names:?}");
        // 无匹配。
        let mut names = Vec::new();
        for_each_match("zz", |command| names.push(command.name));
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn 补全文本带尾空格且缓冲区不足时退化() {
        let command = lookup("help").expect("help 必须在表里");
        let mut buffer = [0u8; 16];
        assert_eq!(completion_text(command, &mut buffer), "help ");
        // 只有 4 字节：放不下 "help "，退化为命令名本身而不是截断。
        let mut tiny = [0u8; 4];
        assert_eq!(completion_text(command, &mut tiny), "help");
        let mut empty = [0u8; 0];
        assert_eq!(completion_text(command, &mut empty), "help");
    }

    #[test]
    fn 未知命令报错是红色且带命令名() {
        let mut out = String::new();
        not_found(&mut out, "foo");
        // 文案格式是行为约定（脚本会 grep 它），因此这里断言到冒号与命令名。
        assert!(out.contains("command not found: foo"), "{out:?}");
        assert!(out.contains("\u{1b}[31m"), "{out:?}");
        // 收尾必须复位，否则后面所有输出都被染红。
        assert!(out.contains("\u{1b}[0m"), "{out:?}");
    }

    #[test]
    fn 命令表没有重复名字或别名() {
        // 重复会在 `lookup` 里静默地只命中前一条：`help` 显示两条、
        // 实际永远执行同一条，这是很难在运行中发现的一类缺陷。
        for left in super::COMMANDS {
            for right in super::COMMANDS {
                if core::ptr::eq(left, right) {
                    continue;
                }
                assert!(
                    !right.matches(left.name),
                    "{} 与 {} 冲突",
                    left.name,
                    right.name
                );
                for alias in left.aliases.split('|').filter(|a| !a.is_empty()) {
                    assert!(!right.matches(alias), "{alias} 与 {} 冲突", right.name);
                }
            }
        }
    }

    #[test]
    fn 每条命令都有说明与非空用法() {
        for command in super::COMMANDS {
            assert!(!command.name.is_empty(), "命令名不能为空");
            assert!(!command.desc.is_empty(), "{} 缺少说明", command.name);
            assert!(!command.usage.is_empty(), "{} 缺少用法", command.name);
            // 命令名必须全小写（解析口径是大小写敏感 + 全小写）。
            assert!(
                command.name.chars().all(|c| !c.is_ascii_uppercase()),
                "{} 不是全小写",
                command.name
            );
        }
    }

    #[test]
    fn 历史命令格式化包含全部条目() {
        let history = Hist(&["pwd", "status"]);
        assert_eq!(history.history_len(), 2);
        assert_eq!(history.history_get(0), Some("pwd"));
        assert_eq!(history.history_get(2), None);
        let empty = Hist(&[]);
        assert_eq!(empty.history_len(), 0);
    }

    #[test]
    fn 状态枚举可用于断言非零语义() {
        assert_ne!(CommandStatus::Ok, CommandStatus::NonZero);
    }

    #[test]
    fn fs_命令名与别名都在表里() {
        // `df` 是现场肌肉记忆（磁盘占用），因此别名必须真的能命中同一条命令。
        assert_eq!(lookup("fs").map(|c| c.name), Some("fs"));
        assert_eq!(lookup("df").map(|c| c.name), Some("fs"));
    }

    #[test]
    fn rt_命令名与别名都在表里() {
        // 别名 `lat` = latency：现场问"延迟多少"时敲的词不一样，但必须是同一条命令。
        assert_eq!(lookup("rt").map(|c| c.name), Some("rt"));
        assert_eq!(lookup("lat").map(|c| c.name), Some("rt"));
    }

    /// 命令说明是**唯一**会被现场看到的口径入口（`help` 里那一行）：单位与"它回答什么"
    /// 都必须在里面，否则表格里的数字没有量纲、也没人知道该不该看这张表。
    #[test]
    fn rt_说明写清单位与它回答的问题() {
        let command = lookup("rt").expect("rt 必须在表里");
        assert!(
            command.desc.contains("µs"),
            "说明缺少单位：{}",
            command.desc
        );
        for keyword in ["关中断", "ISR", "终端输入", "存储访问"] {
            assert!(
                command.desc.contains(keyword),
                "说明缺少「{keyword}」：{}",
                command.desc
            );
        }
        // `rt reset` 是这张表唯一的参数，用法里要看得见（`help` 直接展示 usage）。
        assert!(
            command.usage.contains("reset"),
            "用法缺少参数：{}",
            command.usage
        );
    }

    #[test]
    fn rt_补全前缀唯一命中() {
        // Tab 补全走的是同一张命令表：前缀 `rt` 必须唯一命中 `rt`，否则补全要么
        // 只列候选、要么补错命令。
        let mut names = Vec::new();
        for_each_match("rt", |command| names.push(command.name));
        assert_eq!(names, vec!["rt"]);
        // `lat` 是别名，不参与前缀补全（补全只补命令名），确认它没有意外变成一条新命令。
        let mut alias_names = Vec::new();
        for_each_match("lat", |command| alias_names.push(command.name));
        assert!(alias_names.is_empty(), "{alias_names:?}");
    }

    #[test]
    fn 磨损百分比与平均值按整数计算并守住边界() {
        use super::{ERASE_ENDURANCE_PER_BLOCK, wear_average, wear_percent};
        assert_eq!(wear_percent(0), 0);
        // 不到 1% 也报 0：向下取整是刻意的，诊断不需要小数。
        assert_eq!(wear_percent(999), 0);
        assert_eq!(wear_percent(50_000), 50);
        assert_eq!(wear_percent(60_000), 60);
        // 擦除计数是 u32：必须能表达到器件寿命本身，而 u16 上限只到 65%。
        assert_eq!(wear_percent(u32::from(u16::MAX)), 65);
        assert_eq!(wear_percent(ERASE_ENDURANCE_PER_BLOCK), 100);
        // 超过寿命照实报（>100%），不封顶：封顶会把"用到 3 倍寿命"显示成"刚好用尽"。
        assert_eq!(wear_percent(300_000), 300);
        // 极大值不得因为 u32 中间量溢出而算小（用 u64 做中间量的理由）。
        assert!(wear_percent(u32::MAX) > 4_000_000);
        assert_eq!(wear_average(1_234, 446), 2);
        assert_eq!(wear_average(0, 446), 0);
        // 块数为 0（几何非法）时是"没有数据"，不是除零崩溃。
        assert_eq!(wear_average(1_234, 0), 0);
    }

    // -----------------------------------------------------------------------
    // `wq`（文件系统写队列）的参数解析
    //
    // 解析被抽成纯函数 `parse_wq`，因此这里**不需要任何 ShellContext 替身** ——
    // 把"参数怎么切"与"动作怎么执行"分开，前者是纯逻辑、后者才需要上下文。
    // -----------------------------------------------------------------------

    #[test]
    fn wq_无参数与_stat_都是查统计() {
        use super::{FsWriteAction, parse_wq};
        assert!(matches!(parse_wq(""), Ok(FsWriteAction::Stats)));
        assert!(matches!(parse_wq("stat"), Ok(FsWriteAction::Stats)));
    }

    #[test]
    fn wq_写入保留内容里的空格() {
        use super::{FsWriteAction, parse_wq};
        // 名字之后的内容**原样保留**：`hello world` 不能被切成 `hello`。
        match parse_wq("write cfg hello world 42") {
            Ok(FsWriteAction::Write { name, data }) => {
                assert_eq!(name, "cfg");
                assert_eq!(data, b"hello world 42");
            }
            _ => panic!("应当是 Write 动作"),
        }
    }

    #[test]
    fn wq_追加与删除各有对应动作() {
        use super::{FsWriteAction, parse_wq};
        assert!(matches!(
            parse_wq("append log tail"),
            Ok(FsWriteAction::Append { name: "log", .. })
        ));
        assert!(matches!(
            parse_wq("remove cfg"),
            Ok(FsWriteAction::Remove { name: "cfg" })
        ));
        // 别名与多空格都要容忍。
        assert!(matches!(
            parse_wq("rm   cfg"),
            Ok(FsWriteAction::Remove { name: "cfg" })
        ));
    }

    #[test]
    fn wq_参数不全时给出正确用法而不是猜() {
        use super::parse_wq;
        // 缺内容：不能"写一个空文件"装作成功。
        assert!(parse_wq("write cfg").is_err());
        // 缺文件名。
        assert!(parse_wq("remove").is_err());
        assert!(parse_wq("rm").is_err());
        // 不认识的子命令。
        let err = parse_wq("purge cfg").expect_err("认不出的子命令必须报错");
        assert!(
            err.contains("wq [stat|write|append|remove]"),
            "提示应当给出用法：{err}"
        );
    }

    #[test]
    fn wq_命令已注册且可补全() {
        assert_eq!(lookup("wq").map(|c| c.name), Some("wq"));
        assert_eq!(lookup("writeq").map(|c| c.name), Some("wq"));
    }
}
