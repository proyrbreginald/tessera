//! 文件系统 v3 的**纯逻辑命名空间核心**（inode / 目录项 / 硬链接 / rename）。
//!
//! 这是 [ADR-0009](../../../docs/架构与实现.md) 的第一层：
//! 只定义命名空间语义，**不碰块设备、不做持久化**。持久化层随后把这里的状态编码成
//! v3 记录；在主机上先把"目录/硬链接/rename 到底该是什么行为"钉死。
//!
//! # 零分配
//!
//! 所有容量都是 const 泛型：inode 表与目录项表在编译期定长。没有堆、没有哈希表。
//!
//! # 硬链接语义
//!
//! 目录项指向 inode；同一个 inode 可以有多个目录项。`link_count` 是引用计数，
//! `unlink` 归零才释放 inode。目录不允许硬链接（与 POSIX 的普通用户接口一致），
//! 避免形成环。

use crate::MAX_NAME_LEN;

/// 根目录 inode。0 保留给"无 inode"。
pub const ROOT_INODE: u32 = 1;

/// inode 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeKind {
    /// 普通文件。
    Data,
    /// 只追加日志。
    Log,
    /// 目录。
    Directory,
}

/// 一个目录项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    /// 父目录 inode。
    pub parent: u32,
    /// 名字（ASCII，≤31 字节）。
    pub name: [u8; MAX_NAME_LEN],
    /// 名字长度。
    pub name_len: u8,
    /// 子 inode。
    pub child: u32,
}

/// 命名空间错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceError {
    /// inode 不存在。
    NoInode,
    /// 父路径不是目录。
    NotDirectory,
    /// 目录项不存在。
    NotFound,
    /// 名字/目录项已存在。
    Exists,
    /// 名字非法（空、超长、不可打印或含 `/`）。
    BadName,
    /// 目录非空，不能删除。
    NotEmpty,
    /// 容量不足。
    Full,
    /// 不允许对目录做硬链接。
    IsDirectory,
    /// 不能删除根目录。
    IsRoot,
    /// 内部不一致（目录项槽位在半空状态；正常路径不应出现）。
    Corrupt,
}

/// 固定容量的 inode + 目录项命名空间。
#[derive(Debug, Clone)]
pub struct Namespace<const MAX_INODES: usize, const MAX_ENTRIES: usize> {
    inodes: [InodeSlot; MAX_INODES],
    entries: [Option<DirEntry>; MAX_ENTRIES],
    entry_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct InodeSlot {
    used: bool,
    kind: Option<InodeKind>,
    links: u16,
}

impl<const MAX_INODES: usize, const MAX_ENTRIES: usize> Namespace<MAX_INODES, MAX_ENTRIES> {
    /// 建立只含根目录的命名空间。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inodes: [InodeSlot {
                used: false,
                kind: None,
                links: 0,
            }; MAX_INODES],
            entries: [None; MAX_ENTRIES],
            entry_count: 0,
        }
    }

    /// 根 inode 必须存在且是目录。构造后 `new()` 尚未初始化根；调用方必须调用一次。
    pub fn init_root(&mut self) -> Result<(), NamespaceError> {
        if MAX_INODES < 2 || self.inodes.is_empty() {
            return Err(NamespaceError::Full);
        }
        self.inodes[0] = InodeSlot {
            used: true,
            kind: Some(InodeKind::Directory),
            links: 1,
        };
        Ok(())
    }

    /// 根 inode。
    #[must_use]
    pub const fn root(&self) -> u32 {
        ROOT_INODE
    }

    /// inode 是否存在。
    #[must_use]
    pub fn inode_exists(&self, inode: u32) -> bool {
        self.slot(inode).map(|s| s.used).unwrap_or(false)
    }

    /// inode 类型。
    #[must_use]
    pub fn inode_kind(&self, inode: u32) -> Option<InodeKind> {
        self.slot(inode)
            .and_then(|s| if s.used { s.kind } else { None })
    }

    /// 引用计数。
    #[must_use]
    pub fn link_count(&self, inode: u32) -> Option<u16> {
        self.slot(inode)
            .and_then(|s| if s.used { Some(s.links) } else { None })
    }

    /// 目录项数量。
    #[must_use]
    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// 遍历全部有效 inode（编号、类型、引用计数）。
    pub fn inodes(&self) -> impl Iterator<Item = (u32, InodeKind, u16)> + '_ {
        self.inodes.iter().enumerate().filter_map(|(index, slot)| {
            if !slot.used {
                return None;
            }
            let inode = u32::try_from(index.checked_add(1)?).ok()?;
            Some((inode, slot.kind?, slot.links))
        })
    }

    /// 遍历全部目录项。
    pub fn dir_entries(&self) -> impl Iterator<Item = DirEntry> + '_ {
        self.entries.iter().filter_map(|entry| *entry)
    }

    /// 在 `parent` 下创建目录。
    ///
    /// # Errors
    ///
    /// 父不是目录、名字非法/重复、容量不足。
    pub fn mkdir(&mut self, parent: u32, name: &[u8]) -> Result<u32, NamespaceError> {
        self.create_child(parent, name, InodeKind::Directory)
    }

    /// 在 `parent` 下创建普通文件。
    ///
    /// # Errors
    ///
    /// 父不是目录、名字非法/重复、容量不足。
    pub fn create_file(&mut self, parent: u32, name: &[u8]) -> Result<u32, NamespaceError> {
        self.create_child(parent, name, InodeKind::Data)
    }

    /// 在 `parent` 下创建日志文件。
    ///
    /// # Errors
    ///
    /// 父不是目录、名字非法/重复、容量不足。
    pub fn create_log(&mut self, parent: u32, name: &[u8]) -> Result<u32, NamespaceError> {
        self.create_child(parent, name, InodeKind::Log)
    }

    /// 为已有 inode 增加一个硬链接。
    ///
    /// # Errors
    ///
    /// inode 不存在、目标是目录、父不是目录、名字重复、容量不足。
    pub fn link(&mut self, inode: u32, parent: u32, name: &[u8]) -> Result<(), NamespaceError> {
        if !self.inode_exists(inode) {
            return Err(NamespaceError::NoInode);
        }
        if self.inode_kind(inode) == Some(InodeKind::Directory) {
            return Err(NamespaceError::IsDirectory);
        }
        self.require_dir(parent)?;
        validate_name(name)?;
        if self.find_entry(parent, name).is_some() {
            return Err(NamespaceError::Exists);
        }
        self.insert_entry(DirEntry {
            parent,
            name: name_array(name)?,
            name_len: u8::try_from(name.len()).map_err(|_| NamespaceError::BadName)?,
            child: inode,
        })?;
        if let Some(slot) = self.slot_mut(inode) {
            slot.links = slot.links.saturating_add(1);
        }
        Ok(())
    }

    /// 删除目录项；link count 归零时释放 inode。
    ///
    /// 目录必须为空；根目录不能删除。
    ///
    /// # Errors
    ///
    /// 目录项不存在、目录非空、根目录、容量/名字错误。
    pub fn unlink(&mut self, parent: u32, name: &[u8]) -> Result<u32, NamespaceError> {
        if parent == ROOT_INODE && name.is_empty() {
            return Err(NamespaceError::IsRoot);
        }
        let index = self
            .find_entry(parent, name)
            .ok_or(NamespaceError::NotFound)?;
        let entry = self
            .entries
            .get(index)
            .copied()
            .flatten()
            .ok_or(NamespaceError::Corrupt)?;
        let child = entry.child;
        if self.inode_kind(child) == Some(InodeKind::Directory) && self.child_count(child) > 0 {
            return Err(NamespaceError::NotEmpty);
        }
        if let Some(slot) = self.entries.get_mut(index) {
            *slot = None;
        }
        self.entry_count = self.entry_count.saturating_sub(1);
        if let Some(slot) = self.slot_mut(child) {
            slot.links = slot.links.saturating_sub(1);
            if slot.links == 0 {
                slot.used = false;
                slot.kind = None;
            }
        }
        Ok(child)
    }

    /// 改名：只改目录项，不复制内容。
    ///
    /// # Errors
    ///
    /// 源不存在、目标父不是目录、目标名已存在、跨目录/名字非法。
    pub fn rename(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
    ) -> Result<(), NamespaceError> {
        let index = self
            .find_entry(old_parent, old_name)
            .ok_or(NamespaceError::NotFound)?;
        self.require_dir(new_parent)?;
        validate_name(new_name)?;
        if self.find_entry(new_parent, new_name).is_some() {
            return Err(NamespaceError::Exists);
        }
        let Some(mut entry) = self.entries.get(index).copied().flatten() else {
            return Err(NamespaceError::NotFound);
        };
        entry.parent = new_parent;
        entry.name = name_array(new_name)?;
        entry.name_len = u8::try_from(new_name.len()).map_err(|_| NamespaceError::BadName)?;
        if let Some(slot) = self.entries.get_mut(index) {
            *slot = Some(entry);
        }
        Ok(())
    }

    /// 在 `parent` 下查找名字。
    #[must_use]
    pub fn lookup(&self, parent: u32, name: &[u8]) -> Option<u32> {
        self.find_entry(parent, name)
            .and_then(|index| self.entries.get(index).copied().flatten())
            .map(|entry| entry.child)
    }

    /// 解析绝对路径（`/a/b`），根为 `/`。
    #[must_use]
    pub fn resolve(&self, path: &[u8]) -> Option<u32> {
        if path.first() != Some(&b'/') {
            return None;
        }
        let mut current = ROOT_INODE;
        let mut start = 1usize;
        while start <= path.len() {
            let Some(position) = path
                .get(start..)
                .and_then(|rest| rest.iter().position(|byte| *byte == b'/'))
            else {
                break;
            };
            let end = start.saturating_add(position);
            let component = path.get(start..end)?;
            if !component.is_empty() {
                current = self.lookup(current, component)?;
            }
            start = end.saturating_add(1);
        }
        let tail = path.get(start..)?;
        if tail.is_empty() {
            Some(current)
        } else {
            self.lookup(current, tail)
        }
    }

    /// 设置 inode 引用计数（索引恢复用）。
    pub fn set_links(&mut self, inode: u32, links: u16) -> Result<(), NamespaceError> {
        let Some(slot) = self.slot_mut(inode) else {
            return Err(NamespaceError::NoInode);
        };
        if !slot.used {
            return Err(NamespaceError::NoInode);
        }
        slot.links = links;
        Ok(())
    }

    /// 挂载恢复：登记一个已知 inode（不改变已有条目）。
    pub fn restore_inode(&mut self, inode: u32, kind: InodeKind) -> Result<(), NamespaceError> {
        let Some(index) = inode
            .checked_sub(1)
            .and_then(|value| usize::try_from(value).ok())
        else {
            return Err(NamespaceError::NoInode);
        };
        let Some(slot) = self.inodes.get_mut(index) else {
            return Err(NamespaceError::NoInode);
        };
        *slot = InodeSlot {
            used: true,
            kind: Some(kind),
            links: 0,
        };
        Ok(())
    }

    /// 挂载恢复：登记一个已存在的目录项，并增加子 inode 的引用计数。
    pub fn restore_dirent(
        &mut self,
        parent: u32,
        name: &[u8],
        child: u32,
        kind: InodeKind,
    ) -> Result<(), NamespaceError> {
        self.require_dir(parent)?;
        validate_name(name)?;
        if self.find_entry(parent, name).is_some() {
            return Err(NamespaceError::Exists);
        }
        if !self.inode_exists(child) {
            self.restore_inode(child, kind)?;
        }
        self.insert_entry(DirEntry {
            parent,
            name: name_array(name)?,
            name_len: u8::try_from(name.len()).map_err(|_| NamespaceError::BadName)?,
            child,
        })?;
        if let Some(slot) = self.slot_mut(child) {
            slot.links = slot.links.saturating_add(1);
        }
        Ok(())
    }

    /// 挂载恢复：删除一个目录项并递减引用计数。
    pub fn restore_remove(&mut self, parent: u32, name: &[u8]) -> Result<(), NamespaceError> {
        let index = self
            .find_entry(parent, name)
            .ok_or(NamespaceError::NotFound)?;
        let child = self
            .entries
            .get(index)
            .copied()
            .flatten()
            .ok_or(NamespaceError::Corrupt)?
            .child;
        if let Some(slot) = self.entries.get_mut(index) {
            *slot = None;
        }
        self.entry_count = self.entry_count.saturating_sub(1);
        if let Some(slot) = self.slot_mut(child) {
            slot.links = slot.links.saturating_sub(1);
            if slot.links == 0 {
                slot.used = false;
                slot.kind = None;
            }
        }
        Ok(())
    }

    fn create_child(
        &mut self,
        parent: u32,
        name: &[u8],
        kind: InodeKind,
    ) -> Result<u32, NamespaceError> {
        self.require_dir(parent)?;
        validate_name(name)?;
        if self.find_entry(parent, name).is_some() {
            return Err(NamespaceError::Exists);
        }
        let inode = self.alloc_inode(kind)?;
        self.insert_entry(DirEntry {
            parent,
            name: name_array(name)?,
            name_len: u8::try_from(name.len()).map_err(|_| NamespaceError::BadName)?,
            child: inode,
        })?;
        Ok(inode)
    }

    fn alloc_inode(&mut self, kind: InodeKind) -> Result<u32, NamespaceError> {
        let mut index = 1usize;
        while index < MAX_INODES {
            if let Some(slot) = self.inodes.get_mut(index)
                && !slot.used
            {
                *slot = InodeSlot {
                    used: true,
                    kind: Some(kind),
                    links: 1,
                };
                return u32::try_from(index + 1).map_err(|_| NamespaceError::Full);
            }
            index += 1;
        }
        Err(NamespaceError::Full)
    }

    fn insert_entry(&mut self, entry: DirEntry) -> Result<(), NamespaceError> {
        let mut index = 0usize;
        while index < MAX_ENTRIES {
            if let Some(slot) = self.entries.get_mut(index)
                && slot.is_none()
            {
                *slot = Some(entry);
                self.entry_count = self.entry_count.saturating_add(1);
                return Ok(());
            }
            index += 1;
        }
        Err(NamespaceError::Full)
    }

    fn find_entry(&self, parent: u32, name: &[u8]) -> Option<usize> {
        let mut index = 0usize;
        while index < MAX_ENTRIES {
            if let Some(entry) = self.entries.get(index).copied().flatten()
                && entry.parent == parent
                && usize::from(entry.name_len) == name.len()
                && entry
                    .name
                    .get(..name.len())
                    .map(|stored| stored == name)
                    .unwrap_or(false)
            {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    fn child_count(&self, parent: u32) -> usize {
        self.entries
            .iter()
            .filter_map(|entry| *entry)
            .filter(|entry| entry.parent == parent)
            .count()
    }

    fn require_dir(&self, inode: u32) -> Result<(), NamespaceError> {
        match self.inode_kind(inode) {
            Some(InodeKind::Directory) => Ok(()),
            Some(_) => Err(NamespaceError::NotDirectory),
            None => Err(NamespaceError::NoInode),
        }
    }

    fn slot(&self, inode: u32) -> Option<&InodeSlot> {
        let index = usize::try_from(inode.checked_sub(1)?).ok()?;
        self.inodes.get(index)
    }

    fn slot_mut(&mut self, inode: u32) -> Option<&mut InodeSlot> {
        let index = usize::try_from(inode.checked_sub(1)?).ok()?;
        self.inodes.get_mut(index)
    }
}

impl<const MAX_INODES: usize, const MAX_ENTRIES: usize> Default
    for Namespace<MAX_INODES, MAX_ENTRIES>
{
    fn default() -> Self {
        let mut namespace = Self::new();
        let _ = namespace.init_root();
        namespace
    }
}

fn validate_name(name: &[u8]) -> Result<(), NamespaceError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(NamespaceError::BadName);
    }
    if name
        .iter()
        .any(|byte| !(0x21..=0x7E).contains(byte) || *byte == b'/')
    {
        return Err(NamespaceError::BadName);
    }
    Ok(())
}

fn name_array(name: &[u8]) -> Result<[u8; MAX_NAME_LEN], NamespaceError> {
    let mut bytes = [0u8; MAX_NAME_LEN];
    bytes
        .get_mut(..name.len())
        .ok_or(NamespaceError::BadName)?
        .copy_from_slice(name);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Namespace<32, 64> {
        Namespace::default()
    }

    #[test]
    fn 目录创建与路径解析() {
        let mut ns = ns();
        let a = ns.mkdir(ROOT_INODE, b"a").unwrap();
        let b = ns.mkdir(a, b"b").unwrap();
        assert_eq!(ns.resolve(b"/a").unwrap(), a);
        assert_eq!(ns.resolve(b"/a/b").unwrap(), b);
        assert_eq!(ns.resolve(b"/a/missing"), None);
    }

    #[test]
    fn 硬链接共享_inode_且_unlink_递减引用() {
        let mut ns = ns();
        let dir = ns.mkdir(ROOT_INODE, b"d").unwrap();
        let file = ns.create_file(dir, b"f").unwrap();
        ns.link(file, dir, b"g").unwrap();
        assert_eq!(ns.link_count(file), Some(2));
        assert_eq!(ns.resolve(b"/d/g").unwrap(), file);
        ns.unlink(dir, b"f").unwrap();
        assert_eq!(ns.link_count(file), Some(1));
        assert!(ns.inode_exists(file));
        ns.unlink(dir, b"g").unwrap();
        assert!(!ns.inode_exists(file));
    }

    #[test]
    fn rename_只改目录项_保留_inode() {
        let mut ns = ns();
        let a = ns.mkdir(ROOT_INODE, b"a").unwrap();
        let f = ns.create_file(a, b"x").unwrap();
        ns.rename(a, b"x", ROOT_INODE, b"y").unwrap();
        assert_eq!(ns.resolve(b"/y").unwrap(), f);
        assert_eq!(ns.resolve(b"/a/x"), None);
        assert!(ns.rename(a, b"x", ROOT_INODE, b"y").is_err());
    }

    #[test]
    fn 目录非空不能删且目录不能硬链接() {
        let mut ns = ns();
        let dir = ns.mkdir(ROOT_INODE, b"d").unwrap();
        let _ = ns.create_file(dir, b"f").unwrap();
        assert_eq!(ns.unlink(ROOT_INODE, b"d"), Err(NamespaceError::NotEmpty));
        assert_eq!(
            ns.link(dir, ROOT_INODE, b"alias"),
            Err(NamespaceError::IsDirectory)
        );
    }

    #[test]
    fn 容量耗尽与非法名字() {
        let mut ns: Namespace<3, 2> = Namespace::default();
        assert!(ns.mkdir(ROOT_INODE, b"a").is_ok());
        assert!(ns.mkdir(ROOT_INODE, b"b").is_ok());
        assert_eq!(ns.mkdir(ROOT_INODE, b"c"), Err(NamespaceError::Full));
        assert_eq!(ns.mkdir(ROOT_INODE, b""), Err(NamespaceError::BadName));
        assert_eq!(ns.mkdir(ROOT_INODE, b"a/b"), Err(NamespaceError::BadName));
    }
    #[test]
    fn 命名空间错误路径() {
        let mut ns = Namespace::<4, 4>::new();
        ns.init_root().unwrap();
        assert_eq!(ns.mkdir(99, b"x"), Err(NamespaceError::NoInode));
        let f = ns.create_file(ROOT_INODE, b"f").unwrap();
        assert_eq!(ns.mkdir(f, b"d"), Err(NamespaceError::NotDirectory));
        assert_eq!(ns.mkdir(ROOT_INODE, b"f"), Err(NamespaceError::Exists));
        assert_eq!(ns.link(f, ROOT_INODE, b"f"), Err(NamespaceError::Exists));
        assert_eq!(ns.link(99, ROOT_INODE, b"x"), Err(NamespaceError::NoInode));
        assert_eq!(ns.unlink(ROOT_INODE, b"f").unwrap(), f);
        assert_eq!(ns.unlink(ROOT_INODE, b"f"), Err(NamespaceError::NotFound));
        assert_eq!(
            ns.rename(ROOT_INODE, b"f", ROOT_INODE, b"g"),
            Err(NamespaceError::NotFound)
        );
        assert!(ns.set_links(99, 1).is_err());
        assert!(ns.restore_inode(3, InodeKind::Data).is_ok());
        assert_eq!(ns.inode_kind(3), Some(InodeKind::Data));
        assert!(
            ns.restore_dirent(ROOT_INODE, b"z", f, InodeKind::Data)
                .is_ok()
        );
        assert!(ns.restore_remove(ROOT_INODE, b"z").is_ok());
        assert!(ns.resolve(b"/").is_some());
    }
}
