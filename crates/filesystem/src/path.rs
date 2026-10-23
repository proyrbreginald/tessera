//! Linux 风格的**绝对路径**（扁平命名空间上的路径语义）。
//!
//! # 设计边界：路径是名字的一部分，不是目录 inode
//!
//! 本文件系统是日志结构、零分配的；再引入"目录记录 + 目录项 + 每个目录一块元数据"
//! 会把 RAM 索引和掉电恢复复杂度放大一个量级。这里采用更小的设计：
//!
//! - **文件记录里存完整绝对路径**（例如 `/log/blackbox.log`）；
//! - 目录**由路径前缀隐含**（`/log` 目录存在，当且仅当有文件以 `/log/` 开头）；
//! - `list_dir` 从文件路径**合成**直接子项（文件与子目录），不额外占字节；
//! - 没有空目录、没有 `mkdir`/`rename`/硬链接 —— 这些需要真实目录对象。
//!
//! 对"现场日志、配置、标定"这类用途，这个取舍换来的是：
//! **零新增介质格式、零新增 RAM、掉电语义与现有记录完全一致**。
//!
//! # 规则
//!
//! - 必须是绝对路径（以 `/` 开头），长度 ≤ [`MAX_PATH_LEN`]；
//! - 不能有 `//`、末尾 `/`、`.`/`..` 分量；
//! - 分量只允许可打印 ASCII（0x21..=0x7E）；`/` 只作分隔符；
//! - `/` 是根目录（只可用于 `list_dir`，不能当文件名）。

use crate::MAX_NAME_LEN;

/// 路径长度上限（与记录名字段一致）。
pub const MAX_PATH_LEN: usize = MAX_NAME_LEN;

/// 路径错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// 空路径。
    Empty,
    /// 不是绝对路径。
    NotAbsolute,
    /// 超过记录名字段上限。
    TooLong,
    /// 含非法字符（不可打印 ASCII 或控制字符）。
    BadChar,
    /// 空分量（`//`）、`.`、`..`、或末尾 `/`。
    BadComponent,
    /// 根目录不能当文件路径。
    IsRoot,
}

impl PathError {
    /// 稳定名字（终端/日志用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::NotAbsolute => "not-absolute",
            Self::TooLong => "too-long",
            Self::BadChar => "bad-char",
            Self::BadComponent => "bad-component",
            Self::IsRoot => "is-root",
        }
    }
}

fn components_valid(bytes: &[u8]) -> bool {
    // 跳过开头的 `/`：它不是分量分隔符产生的空分量。
    let mut start = 1usize;
    let mut index = 1usize;
    while index <= bytes.len() {
        let at_end = index == bytes.len();
        if at_end || bytes.get(index) == Some(&b'/') {
            let Some(component) = bytes.get(start..index) else {
                return false;
            };
            if component.is_empty() || component == b"." || component == b".." {
                return false;
            }
            if !component.iter().all(|byte| (0x21..=0x7E).contains(byte)) {
                return false;
            }
            start = index.saturating_add(1);
        }
        index = index.saturating_add(1);
    }
    true
}

/// 校验一个**文件路径**（绝对路径）。
///
/// # Errors
///
/// 见 [`PathError`]。
pub fn validate(path: &str) -> Result<(), PathError> {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Err(PathError::Empty);
    }
    if bytes.len() > MAX_PATH_LEN {
        return Err(PathError::TooLong);
    }
    if bytes.first() != Some(&b'/') {
        return Err(PathError::NotAbsolute);
    }
    if bytes == b"/" {
        return Err(PathError::IsRoot);
    }
    if bytes.last() == Some(&b'/') {
        return Err(PathError::BadComponent);
    }
    if !components_valid(bytes) {
        return Err(PathError::BadComponent);
    }
    Ok(())
}

/// 校验一个**目录路径**；`/` 合法，末尾不带 `/`。
///
/// # Errors
///
/// 见 [`PathError`]。
pub fn validate_dir(path: &str) -> Result<(), PathError> {
    if path == "/" {
        return Ok(());
    }
    validate(path)
}

/// 父目录（`/` 的父目录是 `None`）。
#[must_use]
pub fn parent(path: &str) -> Option<&str> {
    if path == "/" {
        return None;
    }
    let slash = path.rfind('/')?;
    if slash == 0 {
        Some("/")
    } else {
        path.get(..slash)
    }
}

/// 最后一段（根没有名字）。
#[must_use]
pub fn file_name(path: &str) -> Option<&str> {
    let (_, name) = path.rsplit_once('/')?;
    if name.is_empty() { None } else { Some(name) }
}

/// 取 `dir` 下 `path` 的**直接子项**：返回 `(名字, 是否是子目录)`。
///
/// 例：`direct_child("/log", "/log/sub/a") == Some(("sub", true))`；
/// `direct_child("/log", "/log/a") == Some(("a", false))`。
/// `dir` 必须是 `/` 或以 `/` 开头且无尾斜杠；`path` 必须是其严格后代。
#[must_use]
pub fn direct_child<'a>(dir: &str, path: &'a str) -> Option<(&'a str, bool)> {
    let rest = if dir == "/" {
        path.strip_prefix('/')?
    } else {
        let after = path.strip_prefix(dir)?;
        after.strip_prefix('/')?
    };
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('/') {
        Some((name, tail)) if !name.is_empty() && !tail.is_empty() => Some((name, true)),
        Some(_) => None,
        None => Some((rest, false)),
    }
}
