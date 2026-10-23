# ADR-0009：文件系统 v3 —— inode、目录项、rename 与硬链接

- 状态：部分实施（2026-10-22）—— 2C-1 命名空间、2C-2 记录编解码、2C-3a 块头、2C-3b 卷层、2C-4c rename intent/commit、2C-5b 卷层 checkpoint/索引挂载、2C-6a 保守块 GC、2C-7a read_dir/rmdir、2C-6b 旧版本数据块回收已落地；活记录搬移/终端/固件未开始
- 决策者：平台维护者
- 相关文档：[当前进展](../00-索引/当前进展.md)、
  [Flash 布局](../02-架构/Flash布局与用途划分.md)、
  [测试策略](../04-开发/测试策略.md)

## 背景

迁移前（旧版本）的文件系统是**路径即名字**的扁平模型：

- 记录里存完整绝对路径（≤31 字节）；
- 目录由路径前缀隐含，没有目录 inode；
- 没有 `rename`、硬链接、真实 `mkdir`；
- 文件表 16 项，归档/日志已占用多个槽位。

这满足了启动、配置、黑匣子和归档，但无法表达：

1. 一个文件被两个路径引用（硬链接）；
2. 目录整体改名而不重写内容；
3. 目录项与文件内容的原子重命名；
4. 按 inode 回收，而不是按路径回收。

继续在路径字符串上打补丁（例如“复制后删除”实现 rename）会产生**非原子、重复写、
掉电后半新半旧**的语义，与文件系统现有的“记录级 CRC + 版本完整性”承诺冲突。
因此需要一次正式的介质格式升级。

## 决策

文件系统升级到 **v3：inode + 目录项模型**，采用新的块头/记录头版式，并做整片擦除迁移。

### 1. 命名空间

- 每个 inode 有 `u32` 编号；根目录固定为 inode 1；
- inode 类型：`Data`、`Log`、`Directory`；
- inode 元数据：类型、link count、版本、大小/分块位图；
- 目录 inode 的内容是一组**目录项记录**：
  - `parent_inode`、`name`（≤31 字节）、`child_inode`、类型；
- 硬链接 = 多个目录项指向同一 `child_inode`，`link_count` 增加；
- `unlink` 减少 link count；归零后 inode 与内容记录才可被 GC 回收；
- 路径解析从根 inode 逐级查目录项；不再把完整路径写进每条内容记录。

### 2. 记录格式

块头保留 v2 的魔数、seq、state、擦除计数与 CRC；`FORMAT_VERSION` 升到 **2**。
记录头扩展为 v2：

```text
记录头（24 B）
  crc32       u32
  flags       u8   （data/log/tombstone/dirent/rename-intent）
  name_len    u8
  inode       u32
  version     u32
  chunk_index u16
  chunk_total u16
  data_len    u16
  name[≤31]   （仅目录项/旧兼容记录使用）
  payload
```

- 内容记录以 `inode` 标识归属，不再携带完整路径；
- 目录项记录以 `(parent_inode, name)` 唯一；
- 旧 v1 记录在格式版本不匹配时**整体拒绝**，不尝试混读。

### 3. rename 的原子性

rename 不是“读旧文件 + 写新文件 + 删旧文件”，而是**目录项事务**：

1. 写 `rename-intent` 记录：`old_parent/old_name → new_parent/new_name`；
2. 写新目录项记录；
3. 写旧目录项的 tombstone；
4. 写 `rename-commit` 记录。

挂载恢复规则：

- 有 intent 无 commit：按 intent 的旧状态解释；已写的新项视为未提交；
- intent 与 commit 都在：以新目录项为准，旧项为墓碑；
- 目标名已存在：拒绝 rename，不覆盖；
- 跨目录 rename 只改目录项，不搬内容。

### 4. GC 与引用

- GC 的存活判据从“路径+版本”改为“inode 仍被目录项引用且版本完整”；
- 目录 inode 的目录项是活记录；
- link count 归零且无目录项的 inode 才允许回收；
- 遍历目录树用显式栈（零堆），深度上限编译期常量。

### 5. 持久化索引

索引快照升级为 v2：

- 块视图不变；
- 文件表改为 inode 表 + 目录项表；
- 仍用 clean/CRC；索引损坏回退全盘扫描；
- 索引大小重新核算，保留块容量必须覆盖最大目录项数。

### 6. 迁移

- `FORMAT_VERSION`、索引版本、`flash-layout` 文档同步升版；
- 更新固件必须 `bash scripts/flash.sh -e chip`；
- 旧 v1 数据不做在线迁移：格式不匹配一律回退全盘扫描，且旧记录不可读；
- 版本：旧仓库时期完成迁移；本仓库起点 `0.1.0.0` 已包含 v3（不再有单独的 v3 目标版本号）。

## 备选方案与拒绝理由

| 方案 | 拒绝理由 |
|---|---|
| 继续用路径前缀模拟目录 | 无法表达空目录、rename、硬链接；删除目录需要全路径扫描 |
| rename = 读旧写新再删旧 | 非原子；大文件双倍写；掉电会出现两份/零份 |
| 在扁平 FS 上叠一个隐藏 namespace 文件 | 多一次整文件重写，硬链接/rename 的原子性仍依赖上层；与“真实目录”目标不符 |
| 动态分配目录树 | 违反零堆；目录项数量必须编译期可知 |

## 后果

### 正面

- 目录、rename、硬链接成为介质层事实，而不是 API 伪装；
- rename 只改目录项，不复制文件内容；
- GC 与 link count 一致，避免“删了路径但内容永远活着”；
- 为未来多业务文件/目录配额打基础。

### 代价与风险

- 一次完整的介质格式迁移，现有测试与掉电注入需要重写；
- 记录头变大（16→24 B），小文件开销上升；
- 目录遍历/路径解析需要新的零分配算法与深度上限；
- 持久化索引快照需要升级，索引块容量重新核算；
- 必须重新做全片擦除、真机自检、bytecheck、bench、release-check。

## 实施顺序（建议）

1. 纯逻辑记录/inode/目录项编解码与主机测试；✅ 已完成：`namespace.rs` 命名空间 + `volume_record.rs` 记录编解码 + `volume_block.rs` 块头编解码 + `volume.rs` 卷层（目录项持久化 + 分块内容读写 + rename intent/commit）；`volume_index.rs` 命名空间索引快照 + 卷层保留块 checkpoint；卷层 GC（全墓碑块 + 旧版本数据块）+ read_dir/rmdir。
2. `FileSystem` v3 的创建/解析/读写/目录遍历；
3. rename intent/commit 与掉电注入；
4. GC + link count；
5. 持久化索引 v2；
6. 终端 `mkdir`/`rmdir`/`rename`/`ln`/`unlink` 命令；
7. 全片擦除、真机回归（旧仓库时期已完成）。

在 1–4 的主机测试通过前，不把 v3 接到固件启动路径。
