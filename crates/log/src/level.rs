//! 日志级别：只做**顺序**与**解析**，不绑终端/文件系统。
//!
//! 级别过滤由固件持有（`LOG_LEVEL` 原子量），本模块只提供稳定的排序口径，
//! 避免固件与终端各自写一套 `E/W/I/D` 映射。

/// 日志级别。`rank` 越小越紧急。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 错误。
    Error,
    /// 警告。
    Warn,
    /// 信息。
    Info,
    /// 调试。
    Debug,
}

impl Level {
    /// 级别序：`Error=0 .. Debug=3`。
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }

    /// 短标签。
    #[must_use]
    pub const fn tag(self) -> char {
        match self {
            Self::Error => 'E',
            Self::Warn => 'W',
            Self::Info => 'I',
            Self::Debug => 'D',
        }
    }

    /// 从短标签解析；也接受小写。非法值返回 `None`，**不猜默认级别**。
    #[must_use]
    pub const fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 1 {
            return None;
        }
        match bytes.first().copied() {
            Some(b'e' | b'E') => Some(Self::Error),
            Some(b'w' | b'W') => Some(Self::Warn),
            Some(b'i' | b'I') => Some(Self::Info),
            Some(b'd' | b'D') => Some(Self::Debug),
            _ => None,
        }
    }

    /// 是否允许输出：当前级别不高于阈值。
    #[must_use]
    pub const fn allowed(self, threshold: Self) -> bool {
        self.rank() <= threshold.rank()
    }
}

#[cfg(test)]
mod tests {
    use super::Level;

    #[test]
    fn level_parse_and_allowed_are_ordered() {
        assert_eq!(Level::parse("e"), Some(Level::Error));
        assert_eq!(Level::parse("W"), Some(Level::Warn));
        assert_eq!(Level::parse("x"), None);
        assert!(Level::Error.allowed(Level::Warn));
        assert!(!Level::Info.allowed(Level::Warn));
    }
}
