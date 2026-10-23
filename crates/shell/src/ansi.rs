//! ANSI 转义序列与配色。
//!
//! # 为什么把转义序列做成常量而不是"格式化函数"
//!
//! 串口终端是现场唯一的排障窗口，输出里混进一个半截的转义序列会把后面几行
//! 全部染成垃圾——而现场没人能从这个现象反推回代码。把每个序列固化成
//! `&'static str` 常量，构造输出时就只是"把字节拼起来"，没有"转义字符串被
//! 动态拼错"这条失败路径。
//!
//! # 为什么终端要自带配色
//!
//! Fedora 的终端之所以一眼能认，靠的是提示符里的**颜色分区**（用户/主机/路径
//! 各自一色）与"错误必须是红的"。颜色在这里不是装饰：`memtest` 的 PASS/FAIL、
//! `status` 的异常行都要在滚动的串口日志里被一眼捞出来，靠纯文本前缀会淹没在
//! 刷屏里。
//!
//! # 关于收尾
//!
//! 每个带颜色的输出片段都**自己收尾**（写完就 [`RESET`]）。原因是本 crate 的
//! 输出目标是任意 `core::fmt::Write`：固件侧可能因为环形缓冲满而丢掉中间一段
//! 字节，如果颜色状态跨越多个写入片段，丢掉一个 `RESET` 就会让后续所有输出
//! 变色——而现场只能靠断电或重连终端救回来。

/// 关闭所有属性，回到终端默认样式。
pub const RESET: &str = "\x1b[0m";
/// 高亮（提示符里的用户与主机名）。
pub const BOLD: &str = "\x1b[1m";
/// 暗淡（提示符里的分隔符）。
pub const DIM: &str = "\x1b[2m";
/// 红色：错误与失败。
pub const RED: &str = "\x1b[31m";
/// 绿色：成功与通过。
pub const GREEN: &str = "\x1b[32m";
/// 黄色：警告与未证实的状态。
pub const YELLOW: &str = "\x1b[33m";
/// 蓝色：命令名与信息性提示。
pub const BLUE: &str = "\x1b[34m";
/// 品红：缺口标记 `GAP(Px)`。
pub const MAGENTA: &str = "\x1b[35m";
/// 青色：数值与单位。
pub const CYAN: &str = "\x1b[36m";
/// 清屏并把光标移到左上角（`Ctrl-L` 与 `clear` 共用）。
pub const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
/// 清掉整行并把光标移到行首。
pub const CLEAR_LINE: &str = "\x1b[2K\r";
/// 把光标移到行首再清到行尾：重绘输入行之前的固定动作。
///
/// 与 [`CLEAR_LINE`] 的区别是顺序：先 `\r` 再 `\x1b[K`，光标停在行首，
/// 因此紧跟着写提示符就能**原地重绘**；反过来会让光标留在上一行的行尾。
pub const CR_CLEAR_EOL: &str = "\r\x1b[K";

/// 写不进 `core::fmt::Write` 时的统一处理。
///
/// 本 crate 的输出目标是固件的串口环形缓冲：它满的时候丢弃字节，而不是报错。
/// 既然"写失败"没有可执行的补救动作，就不该让每个调用点都去处理 `Result`——
/// 那只会淹没真正的逻辑。刻意用 `let _ =` 而不是 `unwrap`：前者在缓冲满时
/// 安静丢弃，后者在 `no_std` 下等价于直接挂死。
macro_rules! write_ignore {
    ($out:expr, $($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt($out, format_args!($($arg)*));
    }};
}
// 供 `commands` / `shell` 复用：写成宏而不是函数，是因为它要接受任意
// `format_args!`；做成宏再 `pub(crate) use` 出去，避免每个模块各抄一份。
pub(crate) use write_ignore;

/// 三态标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    /// 通过。
    Ok,
    /// 警告 / 未证实。
    Warn,
    /// 失败。
    Err,
    /// 中性信息。
    Info,
}

impl Tag {
    /// 标签文本。
    ///
    /// 刻意固定成四个字符宽（`OK  `/`WARN`/`ERR `/`INFO`）：命令输出要按列对齐，
    /// 宽度不齐的"对齐表"在现场滚动日志里比不排版更难读。
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Ok => "OK  ",
            Self::Warn => "WARN",
            Self::Err => "ERR ",
            Self::Info => "INFO",
        }
    }

    /// 标签颜色。
    ///
    /// `Info` 用青色而不是终端默认色：默认色的标签会与正文混在一起，
    /// 恰恰让"没有颜色"变成最难发现的异常。
    #[must_use]
    pub const fn color(self) -> &'static str {
        match self {
            Self::Ok => GREEN,
            Self::Warn => YELLOW,
            Self::Err => RED,
            Self::Info => CYAN,
        }
    }
}

/// 写一个带颜色的标签，光标留在标签之后，方便调用方接着写正文。
///
/// 颜色码写在标签**之前**、复位写在正文开始之后由调用方负责：把复位放在标签
/// 自己末尾会让"标签 + 正文"的颜色被截断成两段（正文恢复成默认色），
/// 而三态标签的意义恰恰是给**整行**定调。
pub fn tag(out: &mut dyn core::fmt::Write, kind: Tag) {
    write_ignore!(out, "{}{}", kind.color(), kind.text());
}

/// 写提示符：`[root@tessera ~]# `。
///
/// `root` 为真时用户名着红并用 `#`，否则着绿并用 `$`——现场一眼就能区分
/// "以什么身份"敲的命令，这在多人轮流接同一根串口时尤其重要。
pub fn prompt(out: &mut dyn core::fmt::Write, user: &str, host: &str, cwd: &str, root: bool) {
    // 用户/主机加粗：提示符里最先被眼睛抓到的是"谁在哪台设备上"。
    write_ignore!(out, "{DIM}[{RESET}");
    if root {
        write_ignore!(out, "{RED}{user}{RESET}");
    } else {
        write_ignore!(out, "{GREEN}{user}{RESET}");
    }
    write_ignore!(out, "{DIM}@{RESET}{BOLD}{host}{RESET}");
    // 路径用蓝色：与命令名（正文默认色）区分开。
    write_ignore!(out, " {BLUE}{cwd}{RESET}{DIM}]{RESET}");
    // 提示符的最后一个字符：`#`（root）/ `$`（普通用户），带颜色。
    if root {
        write_ignore!(out, "{RED}# ");
    } else {
        write_ignore!(out, "{GREEN}$ ");
    }
    // 提示符自带收尾：`RESET` 写在提示符**之后**（而不是每个片段之后），
    // 这样"用户回显的内容"不会继承提示符最后一个字符的颜色。行编辑器重绘时
    // 会再发一次复位，重复复位无副作用，换来的是"任何调用方都不用操心颜色
    // 有没有收干净"。
    write_ignore!(out, "{RESET}");
}

#[cfg(test)]
mod tests {
    use super::{CLEAR_LINE, CLEAR_SCREEN, CR_CLEAR_EOL, RESET, Tag, prompt, tag};

    /// 测试用输出目标。`String` 在 `std`（`alloc`）下已实现 `core::fmt::Write`，
    /// 因此不需要为本 crate 再定义一个输出 trait —— 那会把上游依赖带进产品代码。
    fn out() -> String {
        String::new()
    }

    #[test]
    fn 控制序列常量逐字节正确() {
        // 这些常量会被逐字节写进串口：多一个空格终端就不认。
        assert_eq!(RESET, "\u{1b}[0m");
        assert_eq!(CLEAR_SCREEN, "\u{1b}[2J\u{1b}[H");
        assert_eq!(CLEAR_LINE, "\u{1b}[2K\r");
        assert_eq!(CR_CLEAR_EOL, "\r\u{1b}[K");
    }

    #[test]
    fn 提示符分色且以井号空格结尾() {
        let mut buffer = out();
        prompt(&mut buffer, "root", "tessera", "~", true);
        // 断言关键片段而不是整段：颜色的具体组合允许后续微调，但
        // "包含用户名/主机名/路径、以 `# ` 收尾、并且收尾后立刻复位"是行为约定。
        assert!(buffer.contains("root"), "{buffer:?}");
        assert!(buffer.contains("tessera"), "{buffer:?}");
        assert!(buffer.contains('~'), "{buffer:?}");
        assert!(buffer.ends_with("# \u{1b}[0m"), "{buffer:?}");
        // 结尾必须是 RESET：否则颜色会渗进用户敲进来的回显。
        assert!(buffer.contains(RESET), "{buffer:?}");
    }

    #[test]
    fn 非_root_用美元号() {
        let mut buffer = out();
        prompt(&mut buffer, "user", "host", "/", false);
        assert!(buffer.ends_with("$ \u{1b}[0m"), "{buffer:?}");
    }

    #[test]
    fn 标签按语义着色且宽度一致() {
        let mut ok = out();
        tag(&mut ok, Tag::Ok);
        let mut warn = out();
        tag(&mut warn, Tag::Warn);
        let mut err = out();
        tag(&mut err, Tag::Err);
        let mut info = out();
        tag(&mut info, Tag::Info);

        assert!(ok.contains("\u{1b}[32m"), "{ok:?}");
        assert!(warn.contains("\u{1b}[33m"), "{warn:?}");
        assert!(err.contains("\u{1b}[31m"), "{err:?}");
        assert!(info.contains("\u{1b}[36m"), "{info:?}");

        // 对齐：四个标签的纯文本宽度必须相同，否则按列对齐的输出会错位。
        let widths = [
            Tag::Ok.text().len(),
            Tag::Warn.text().len(),
            Tag::Err.text().len(),
            Tag::Info.text().len(),
        ];
        assert!(widths.iter().all(|width| *width == widths[0]), "{widths:?}");
    }

    #[test]
    fn 标签颜色互不相同() {
        // 三态如果撞色，"一眼看出哪一行失败"就失效了。
        let colors = [
            Tag::Ok.color(),
            Tag::Warn.color(),
            Tag::Err.color(),
            Tag::Info.color(),
        ];
        for (index, left) in colors.iter().enumerate() {
            for right in colors.iter().skip(index.saturating_add(1)) {
                assert_ne!(left, right, "{colors:?}");
            }
        }
    }
}
