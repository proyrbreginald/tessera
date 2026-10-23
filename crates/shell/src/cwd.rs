//! 终端当前工作目录（`pwd`/`cd` 的状态）。
//!
//! 只做路径规范化与校验，不碰文件系统：目录是否存在由固件侧的
//! [`crate::shell::ShellContext::fs_is_dir`] 回答。路径规则与 `filesystem::path`
//! 共用同一份实现（`shell` 依赖纯逻辑的 `filesystem`，不引入硬件 crate）。

use filesystem::path::{MAX_PATH_LEN, validate, validate_dir};

/// 当前工作目录（绝对路径，容量固定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cwd {
    bytes: [u8; MAX_PATH_LEN],
    len: u8,
}

impl Default for Cwd {
    fn default() -> Self {
        Self::new()
    }
}

impl Cwd {
    /// 根目录。
    #[must_use]
    pub const fn new() -> Self {
        let mut bytes = [0u8; MAX_PATH_LEN];
        bytes[0] = b'/';
        Self { bytes, len: 1 }
    }

    /// 当前路径字符串（总是绝对路径）。
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..usize::from(self.len))
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("/")
    }

    /// 直接设置为已校验的绝对目录。
    ///
    /// 返回 `false` = 路径非法或过长（不修改当前目录）。
    pub fn set(&mut self, path: &str) -> bool {
        if validate_dir(path).is_err() {
            return false;
        }
        let bytes = path.as_bytes();
        if bytes.len() > MAX_PATH_LEN {
            return false;
        }
        let mut next = Self::new();
        let Some(slot) = next.bytes.get_mut(..bytes.len()) else {
            return false;
        };
        slot.copy_from_slice(bytes);
        let Ok(len) = u8::try_from(bytes.len()) else {
            return false;
        };
        next.len = len;
        *self = next;
        true
    }

    /// 把 `input`（绝对/相对、可含 `.`/`..`）规范化到绝对路径。
    ///
    /// `out` 是调用方提供的缓冲区；返回的 `&str` 指向它，零分配。
    /// `want_dir` = `true` 时按目录规则校验，否则按文件规则校验。
    #[must_use]
    pub fn resolve<'a>(
        &self,
        input: &str,
        out: &'a mut [u8; MAX_PATH_LEN],
        want_dir: bool,
    ) -> Option<&'a str> {
        let mut scratch = [0u8; MAX_PATH_LEN];
        let mut len: usize;
        if input.starts_with('/') {
            scratch[0] = b'/';
            len = 1;
        } else {
            let current = self.as_str().as_bytes();
            len = current.len();
            scratch.get_mut(..len)?.copy_from_slice(current);
        }
        for component in input.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                if len > 1 {
                    let slash = scratch
                        .get(..len)?
                        .iter()
                        .rposition(|byte| *byte == b'/')
                        .unwrap_or(0);
                    len = if slash == 0 { 1 } else { slash };
                }
                continue;
            }
            if !component.bytes().all(|byte| (0x21..=0x7E).contains(&byte)) {
                return None;
            }
            if len > 1 {
                if len >= MAX_PATH_LEN {
                    return None;
                }
                *scratch.get_mut(len)? = b'/';
                len += 1;
            } else if len == 1 && scratch.first() == Some(&b'/') {
                // 根后面直接接分量，不能再加一个 `/`。
            }
            let bytes = component.as_bytes();
            let end = len.checked_add(bytes.len())?;
            if end > MAX_PATH_LEN {
                return None;
            }
            scratch.get_mut(len..end)?.copy_from_slice(bytes);
            len = end;
        }
        let text = core::str::from_utf8(scratch.get(..len)?).ok()?;
        if want_dir {
            validate_dir(text).ok()?;
        } else {
            validate(text).ok()?;
        }
        let out_len = text.len();
        out.get_mut(..out_len)?.copy_from_slice(text.as_bytes());
        core::str::from_utf8(out.get(..out_len)?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 相对路径与点段规范化() {
        let cwd = Cwd::new();
        let mut buffer = [0u8; MAX_PATH_LEN];
        assert_eq!(cwd.resolve("log", &mut buffer, true), Some("/log"));
        assert_eq!(
            cwd.resolve("./a/../b", &mut buffer, false),
            Some("/a/../b").map(|_| "/b")
        );
        assert_eq!(cwd.resolve("../x", &mut buffer, false), Some("/x"));
        assert_eq!(cwd.resolve("/a/b/..", &mut buffer, true), Some("/a"));
        assert_eq!(cwd.resolve("/a/./b", &mut buffer, false), Some("/a/b"));
    }

    #[test]
    fn 当前目录可切换且非法路径被拒() {
        let mut cwd = Cwd::new();
        assert!(cwd.set("/log"));
        assert_eq!(cwd.as_str(), "/log");
        let mut buffer = [0u8; MAX_PATH_LEN];
        assert_eq!(cwd.resolve("..", &mut buffer, true), Some("/"));
        assert_eq!(
            cwd.resolve("sub/file", &mut buffer, false),
            Some("/log/sub/file")
        );
        assert!(!cwd.set("not/absolute"));
        assert!(!cwd.set("/a/../b"));
        assert_eq!(cwd.as_str(), "/log", "失败不得改当前目录");
    }
}
