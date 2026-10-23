//! 捕获日志出口：让"该报的错报了吗"成为可断言的事实。
//!
//! 日志本身不是功能，但**诊断能力是需求**（N6：可远程定位现场故障）。
//! 因此"关键事件确实产生了日志"必须有测试，否则一次重构就能静默删掉它。

use heapless::{String, Vec};
use ports::log::{Level, LogSink, Record};

/// 把日志存进定长数组。
#[derive(Debug)]
pub struct CapturingLog<const N: usize> {
    lines: Vec<String<96>, N>,
    emitted: usize,
    dropped: usize,
}

impl<const N: usize> Default for CapturingLog<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> CapturingLog<N> {
    /// 空日志。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lines: Vec::new(),
            emitted: 0,
            dropped: 0,
        }
    }

    /// 已记录的行。
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|line| line.as_str())
    }

    /// 记录行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 成功输出次数。
    #[must_use]
    pub const fn emitted(&self) -> usize {
        self.emitted
    }

    /// 被丢弃次数。
    ///
    /// 丢弃**必须**可观测：现场最怕"日志缺失但无人知道"。
    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    /// 是否存在包含 `needle` 的行。
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.lines.iter().any(|line| line.as_str().contains(needle))
    }

    /// 是否存在某个级别的行。
    #[must_use]
    pub fn has_level(&self, level: Level) -> bool {
        self.lines
            .iter()
            .any(|line| line.as_str().starts_with(level.tag()))
    }
}

impl<const N: usize> LogSink for CapturingLog<N> {
    fn emit(&mut self, record: &Record<'_>) -> bool {
        let mut line = String::new();
        use core::fmt::Write as _;
        if write!(line, "{record}").is_err() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        if self.lines.push(line).is_err() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.emitted = self.emitted.saturating_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record<'a>(level: Level, message: &'a str) -> Record<'a> {
        Record {
            level,
            at_micros: 1_000,
            target: "sim",
            message,
        }
    }

    #[test]
    fn captures_and_indexes_lines() {
        let mut log = CapturingLog::<8>::new();
        assert!(log.emit(&record(Level::Error, "bus lost")));
        assert!(log.emit(&record(Level::Info, "ready")));
        assert_eq!(log.len(), 2);
        assert!(log.contains("bus lost"));
        assert!(log.has_level(Level::Error));
        assert!(log.contains("sim"));
        assert_eq!(log.dropped(), 0);
    }

    #[test]
    fn overflow_is_counted_not_swallowed() {
        let mut log = CapturingLog::<1>::new();
        assert!(log.emit(&record(Level::Warn, "first")));
        assert!(!log.emit(&record(Level::Warn, "second")));
        assert_eq!(log.len(), 1);
        assert_eq!(log.dropped(), 1);
    }

    #[test]
    fn empty_log_reports_state_honestly() {
        let log = CapturingLog::<4>::new();
        assert!(log.is_empty());
        assert!(!log.contains("anything"));
        assert_eq!(log.emitted(), 0);
    }
}
