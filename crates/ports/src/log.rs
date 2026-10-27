//! 日志端口。
//!
//! # 为什么不用 `defmt`
//!
//! `defmt` 需要调试器在场（RTT/半主机）才能读出日志，而**现场故障恰恰发生在
//! 没有调试器的时候**。另外 `imxrt-hal` 0.6 的隐式 `defmt` feature 会强拉
//! 一整套依赖进入固件构建闭包，扩大需要论证的工具面。
//!
//! 因此本项目的方案是：
//!
//! | 通道 | 用途 | 在场条件 |
//! |------|------|----------|
//! | UART 文本（LPUART1） | 现场可读、可抓包、可被台架抓包解析 | 无 |
//! | 黑匣子（`.noinit` 环形缓冲） | 掉电保留的故障现场 | 无 |
//! | `defmt`（可选 feature，需联网拉依赖） | 开发期高速日志 | 需调试器 |
//!
//! 三者的分级策略是统一的：[`Level`]，且**编译期可裁剪**——`Error`/`Warn` 常开，
//! `Info` 默认关，`Debug`/`Trace` 仅开发构建。

use core::fmt;

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 错误：已经影响了功能，必须上报。
    Error,
    /// 警告：出现异常但已处置，需要趋势观察。
    Warn,
    /// 信息：状态变化（启动、连接建立、升级阶段）。
    Info,
    /// 调试：开发期细节。
    Debug,
}

impl Level {
    /// 级别序（越小越紧急）。不用 `as` 转换：本 crate 禁止 `as`，
    /// 因为它会掩盖枚举重排带来的语义变化。
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Level::Error => 0,
            Level::Warn => 1,
            Level::Info => 2,
            Level::Debug => 3,
        }
    }

    /// 是否应当输出（与编译期上限比较）。
    #[must_use]
    pub const fn is_enabled(self, max: Level) -> bool {
        // 级别越高越紧急，rank 越小；因此"是否输出"是 self.rank() <= max.rank()。
        self.rank() <= max.rank()
    }

    /// 短标签（用于串口文本）。
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
        }
    }
}

/// 一条日志记录。
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// 级别。
    pub level: Level,
    /// 时间点（微秒）。
    pub at_micros: u64,
    /// 来源（模块名/子系统名）。
    pub target: &'a str,
    /// 正文（已格式化好的定长文本）。
    pub message: &'a str,
}

impl fmt::Display for Record<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}.{:06} {}: {}",
            self.level.tag(),
            self.at_micros / 1_000_000,
            self.at_micros % 1_000_000,
            self.target,
            self.message
        )
    }
}

/// 日志出口。
///
/// 实现者**不得**在其中长时间阻塞：本 trait 会被空闲任务调用，但绝不能被
/// 优先级 ≥1 的任务调用。ISR 只允许累加计数（见 `primitives::safety::HealthBits`）。
pub trait LogSink {
    /// 输出一条记录。
    ///
    /// # Errors
    ///
    /// 返回 `false` 表示本次输出被丢弃（缓冲满/通道忙）。
    /// 丢弃必须被**计数**，否则现场会得到"日志缺失但无人知道"的假象。
    fn emit(&mut self, record: &Record<'_>) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter {
        emitted: usize,
        dropped: usize,
        max: Level,
    }

    impl LogSink for Counter {
        fn emit(&mut self, record: &Record<'_>) -> bool {
            if record.level.is_enabled(self.max) {
                self.emitted += 1;
                true
            } else {
                self.dropped += 1;
                false
            }
        }
    }

    #[test]
    fn level_filtering_is_ordered_by_severity() {
        assert!(Level::Error.is_enabled(Level::Warn));
        assert!(Level::Warn.is_enabled(Level::Warn));
        assert!(!Level::Info.is_enabled(Level::Warn));
        assert!(!Level::Debug.is_enabled(Level::Warn));
        assert!(Level::Debug.is_enabled(Level::Debug));
    }

    #[test]
    fn sink_filters_and_counts_drops() {
        let mut sink = Counter {
            emitted: 0,
            dropped: 0,
            max: Level::Warn,
        };
        let record = |level| Record {
            level,
            at_micros: 1_500_000,
            target: "test",
            message: "hi",
        };
        assert!(sink.emit(&record(Level::Error)));
        assert!(!sink.emit(&record(Level::Debug)));
        assert_eq!(sink.emitted, 1);
        assert_eq!(sink.dropped, 1, "被丢弃的日志必须可见，否则现场会误判");
    }

    #[test]
    fn record_display_is_field_parseable() {
        let record = Record {
            level: Level::Info,
            at_micros: 12_345_678,
            target: "app",
            message: "ready",
        };
        let mut storage = [0u8; 64];
        let mut buf = primitives::format::FixedBuf::new(&mut storage);
        use core::fmt::Write as _;
        write!(buf, "{record}").unwrap();
        assert_eq!(
            core::str::from_utf8(buf.as_bytes()).unwrap(),
            "I 12.345678 app: ready"
        );
    }
}
