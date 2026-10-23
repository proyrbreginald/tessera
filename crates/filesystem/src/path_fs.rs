//! v3 卷层的**路径门面**：把 `/a/b` 拆成父 inode + 名字，供终端/后端适配使用。
//!
//! 这是 0.28 适配层的第一步：不改变生产 v2，只让 v3 拥有与终端一致的路径接口。

use crate::device::BlockDevice;
use crate::namespace::{InodeKind, NamespaceError};
use crate::volume::{Volume, VolumeError};

/// 路径门面错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacadeError {
    /// 路径必须以 `/` 开头。
    AbsolutePathRequired,
    /// 路径没有父目录/名字，或父目录不存在。
    BadPath,
    /// 卷层错误。
    Volume(VolumeError),
}

impl From<VolumeError> for FacadeError {
    fn from(value: VolumeError) -> Self {
        Self::Volume(value)
    }
}

impl From<NamespaceError> for FacadeError {
    fn from(value: NamespaceError) -> Self {
        Self::Volume(VolumeError::Namespace(value))
    }
}

/// v3 路径门面。
pub struct PathFs<D: BlockDevice, const MAX_INODES: usize, const MAX_ENTRIES: usize> {
    volume: Volume<D, MAX_INODES, MAX_ENTRIES>,
}

impl<D: BlockDevice, const MAX_INODES: usize, const MAX_ENTRIES: usize>
    PathFs<D, MAX_INODES, MAX_ENTRIES>
{
    /// 包装已格式化/挂载的卷。
    #[must_use]
    pub const fn new(volume: Volume<D, MAX_INODES, MAX_ENTRIES>) -> Self {
        Self { volume }
    }

    /// 卷只读视图。
    #[must_use]
    pub const fn volume(&self) -> &Volume<D, MAX_INODES, MAX_ENTRIES> {
        &self.volume
    }

    /// 卷可变视图。
    #[must_use]
    pub const fn volume_mut(&mut self) -> &mut Volume<D, MAX_INODES, MAX_ENTRIES> {
        &mut self.volume
    }

    /// 是否存在。
    #[must_use]
    pub fn exists(&self, path: &[u8]) -> bool {
        self.volume.resolve(path).is_some()
    }

    /// 文件类型与引用计数。
    #[must_use]
    pub fn stat(&self, path: &[u8]) -> Option<(InodeKind, u16)> {
        let inode = self.volume.resolve(path)?;
        Some((
            self.volume.namespace().inode_kind(inode)?,
            self.volume.namespace().link_count(inode)?,
        ))
    }

    /// 创建普通文件。
    ///
    /// # Errors
    ///
    /// 路径非法、父目录不存在、名字重复、卷错误。
    pub fn create_file(&mut self, path: &[u8]) -> Result<(), FacadeError> {
        let (parent, name) = self.split(path)?;
        self.volume.create_file(parent, name)?;
        Ok(())
    }

    /// 创建日志文件。
    ///
    /// # Errors
    ///
    /// 路径非法、父目录不存在、名字重复、卷错误。
    pub fn create_log(&mut self, path: &[u8]) -> Result<(), FacadeError> {
        let (parent, name) = self.split(path)?;
        self.volume.create_log(parent, name)?;
        Ok(())
    }

    /// 文件大小。
    ///
    /// # Errors
    ///
    /// 路径不存在、记录不一致、卷错误。
    pub fn file_size(&mut self, path: &[u8]) -> Result<usize, FacadeError> {
        let inode = self.volume.resolve(path).ok_or(FacadeError::BadPath)?;
        Ok(self.volume.inode_size(inode)?)
    }

    /// 创建目录。
    ///
    /// # Errors
    ///
    /// 路径非法、父目录不存在、名字重复、卷错误。
    pub fn mkdir(&mut self, path: &[u8]) -> Result<(), FacadeError> {
        let (parent, name) = self.split(path)?;
        self.volume.mkdir(parent, name)?;
        Ok(())
    }

    /// 删除空目录。
    ///
    /// # Errors
    ///
    /// 路径非法、目标不是目录/非空、卷错误。
    pub fn rmdir(&mut self, path: &[u8]) -> Result<(), FacadeError> {
        let (parent, name) = self.split(path)?;
        self.volume.rmdir(parent, name)?;
        Ok(())
    }

    /// 删除文件或空目录项。
    ///
    /// # Errors
    ///
    /// 路径非法、目标不存在、卷错误。
    pub fn unlink(&mut self, path: &[u8]) -> Result<(), FacadeError> {
        let (parent, name) = self.split(path)?;
        self.volume.unlink(parent, name)?;
        Ok(())
    }

    /// 改名。
    ///
    /// # Errors
    ///
    /// 路径非法、源/目标错误、卷错误。
    pub fn rename(&mut self, old: &[u8], new: &[u8]) -> Result<(), FacadeError> {
        let (old_parent, old_name) = self.split(old)?;
        let (new_parent, new_name) = self.split(new)?;
        self.volume
            .rename(old_parent, old_name, new_parent, new_name)?;
        Ok(())
    }

    /// 为已有文件创建硬链接。
    ///
    /// # Errors
    ///
    /// 源/目标路径非法、目标是目录、目标已存在、卷错误。
    pub fn link(&mut self, existing: &[u8], new: &[u8]) -> Result<(), FacadeError> {
        let inode = self.volume.resolve(existing).ok_or(FacadeError::BadPath)?;
        let (parent, name) = self.split(new)?;
        self.volume.link(inode, parent, name)?;
        Ok(())
    }

    /// 列目录。
    ///
    /// # Errors
    ///
    /// 路径不是目录、卷错误。
    pub fn read_dir(
        &self,
        path: &[u8],
        callback: &mut dyn FnMut(&[u8], u32, InodeKind),
    ) -> Result<(), FacadeError> {
        let inode = self.volume.resolve(path).ok_or(FacadeError::BadPath)?;
        self.volume.read_dir(inode, callback)?;
        Ok(())
    }

    /// 追加日志内容（仅日志类型文件）。
    ///
    /// # Errors
    ///
    /// 路径不存在/不是日志、单条过大、卷错误。
    pub fn append_log(&mut self, path: &[u8], data: &[u8]) -> Result<(), FacadeError> {
        let inode = self.volume.resolve(path).ok_or(FacadeError::BadPath)?;
        self.volume.append_log(inode, data)?;
        Ok(())
    }

    /// 写文件内容（整段）。
    ///
    /// # Errors
    ///
    /// 路径不存在、卷错误。
    pub fn write_file(&mut self, path: &[u8], data: &[u8]) -> Result<(), FacadeError> {
        self.volume.write_file(path, data)?;
        Ok(())
    }

    /// 读文件区间。
    ///
    /// # Errors
    ///
    /// 路径不存在、缓冲不足、卷错误。
    pub fn read_file(
        &mut self,
        path: &[u8],
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, FacadeError> {
        Ok(self.volume.read_file_range(path, offset, out)?)
    }

    fn split<'a>(&self, path: &'a [u8]) -> Result<(u32, &'a [u8]), FacadeError> {
        let rest = path
            .strip_prefix(b"/")
            .ok_or(FacadeError::AbsolutePathRequired)?;
        let (parent_path, name) = match rest.iter().rposition(|byte| *byte == b'/') {
            Some(index) => (
                rest.get(..index).ok_or(FacadeError::BadPath)?,
                rest.get(index.saturating_add(1)..)
                    .ok_or(FacadeError::BadPath)?,
            ),
            None => (b"".as_slice(), rest),
        };
        if name.is_empty() {
            return Err(FacadeError::BadPath);
        }
        let parent = if parent_path.is_empty() {
            self.volume.namespace().root()
        } else {
            let total = parent_path.len().saturating_add(1);
            let mut absolute = [0u8; 64];
            let buffer = absolute.get_mut(..total).ok_or(FacadeError::BadPath)?;
            if let Some(first) = buffer.get_mut(0) {
                *first = b'/';
            }
            buffer
                .get_mut(1..)
                .ok_or(FacadeError::BadPath)?
                .copy_from_slice(parent_path);
            self.volume
                .resolve(absolute.get(..total).ok_or(FacadeError::BadPath)?)
                .ok_or(FacadeError::BadPath)?
        };
        Ok((parent, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockDevice;
    use crate::namespace::ROOT_INODE;

    type Facade = PathFs<MockDevice<32768>, 16, 32>;

    fn fresh() -> Facade {
        let volume = Volume::format(MockDevice::new(4096, 8)).unwrap();
        PathFs::new(volume)
    }

    #[test]
    fn 路径门面创建目录与文件() {
        let mut facade = fresh();
        facade.mkdir(b"/a").unwrap();
        facade.mkdir(b"/a/b").unwrap();
        assert!(facade.exists(b"/a/b"));
        facade.write_file(b"/a/b", b"").unwrap_err();
        assert!(facade.stat(b"/a").is_some());
        let mut count = 0;
        facade.read_dir(b"/a", &mut |_, _, _| count += 1).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn 路径门面硬链接与_rename() {
        let mut facade = fresh();
        facade.mkdir(b"/d").unwrap();
        facade.volume_mut().create_file(ROOT_INODE, b"f").unwrap();
        facade.link(b"/f", b"/g").unwrap();
        facade.rename(b"/g", b"/d/h").unwrap();
        assert!(facade.exists(b"/d/h"));
        facade.unlink(b"/f").unwrap();
        assert!(facade.exists(b"/d/h"));
        facade.unlink(b"/d/h").unwrap();
        facade.rmdir(b"/d").unwrap();
    }
    #[test]
    fn 路径门面覆盖全部读写与错误分支() {
        let mut facade = fresh();
        facade.mkdir(b"/d").unwrap();
        facade.create_file(b"/d/f").unwrap();
        facade.write_file(b"/d/f", b"hello").unwrap();
        assert_eq!(facade.file_size(b"/d/f").unwrap(), 5);
        let mut out = [0u8; 8];
        assert_eq!(facade.read_file(b"/d/f", 1, &mut out).unwrap(), 4);
        assert_eq!(out.get(..4), Some(b"ello".as_slice()));
        assert!(facade.stat(b"/d/f").is_some());
        assert!(facade.stat(b"/d").is_some());
        facade.create_log(b"/d/l").unwrap();
        facade.append_log(b"/d/l", b"log").unwrap();
        assert_eq!(facade.file_size(b"/d/l").unwrap(), 3);
        facade.link(b"/d/f", b"/d/h").unwrap();
        facade.rename(b"/d/h", b"/d/g").unwrap();
        assert!(facade.exists(b"/d/g"));
        facade.unlink(b"/d/g").unwrap();
        facade.unlink(b"/d/f").unwrap();
        facade.unlink(b"/d/l").unwrap();
        facade.rmdir(b"/d").unwrap();
        assert!(facade.mkdir(b"relative").is_err());
        assert!(facade.mkdir(b"/").is_err());
        assert!(facade.create_file(b"/missing/f").is_err());
        assert!(facade.write_file(b"/nope", b"x").is_err());
        assert!(facade.read_file(b"/nope", 0, &mut out).is_err());
        assert!(facade.file_size(b"/nope").is_err());
        assert!(facade.link(b"/nope", b"/x").is_err());
        assert!(facade.rename(b"/nope", b"/x").is_err());
        assert!(facade.unlink(b"/nope").is_err());
        assert!(facade.rmdir(b"/nope").is_err());
        assert!(facade.read_dir(b"/nope", &mut |_, _, _| {}).is_err());
    }
}
