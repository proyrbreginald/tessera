//! 终端外壳：把字节喂给编辑器、分发命令。
//!
//! # 为什么用 trait 反向注入能力，而不是让终端直接读寄存器
//!
//! 终端需要知道"现在是谁、运行了多久、SDRAM 自检过没有"，还需要能"请求重启"。
//! 这些能力全都在固件里。如果终端自己去读 RAL 寄存器，它就不再是纯逻辑层——
//! 主机的单元测试将无法覆盖任何一条命令。
//!
//! 因此这里定义 [`ShellContext`]：**固件实现它，终端只通过它获取信息与触发动作**。
//! 现场排障的绝大部分缺陷（命令分发改错、参数解析、输出格式、补全）都能用一个
//! 测试替身在主机上验证——包括"`reboot` 到底有没有真的请求重启"这种副作用。
//!
//! # 为什么 `feed` 一次只吃一个字节
//!
//! 串口接收是中断驱动的，固件在中断/空闲循环里拿到的就是"一个字节"。把回车、
//! 转义序列的**跨字节状态**留在编辑器里（而不是让固件攒一整行再交过来），
//! 现场按上下键翻历史才能即时生效。
//!
//! # 提示符为什么要缓存进编辑器
//!
//! 行重绘（退格、Ctrl-L、Tab 补全、翻历史）需要"提示符 + 当前行"整行重画。
//! 提示符里含固件提供的用户名/主机名/路径，属于 [`ShellContext`]；重绘发生在
//! 按键处理**中途**，此时 `ctx` 往往已被可变借用（`memtest`/`reboot` 要走
//! `&mut`）。因此在每次提示符输出时把渲染好的字节交给编辑器缓存，重绘只读缓存。

use realtime::RtMetrics;

/// **产品身份**：横幅、`version`、`uname` 都用它，只有这一处定义。
///
/// 为什么是常量而不是 `env!("CARGO_PKG_NAME")`：crate 名（`firmware`）是**代码组织**的
/// 名字，产品名（`tessera`）是**这台设备**的名字 —— 两者会分道扬镳。
/// 把它们绑在一起，等于让"改 crate 名"变成"改设备身份"这种重型操作。
pub const PRODUCT: &str = "tessera";

/// 分配池的当前状态（终端 `mem` 打印）。
///
/// # 为什么在 shell 侧再定义一份，而不直接用 BSP 的 `PoolStats`
///
/// 分层是编译期事实：`shell` 是纯逻辑 crate，**不能**依赖 `board-support`（那会把
/// 寄存器访问拖进终端层）。这与 `FsSnapshot`/`InstrumentHealth` 是同一条分工 ——
/// shell 定义"要显示什么"，固件负责"把事实填进来"。
///
/// 字段刻意与池的**契约**一一对应，而不是随便挑几个数字：
/// - `used_blocks`/`high_water_blocks`：用得多不多、峰值多少（决定池该多大）；
/// - `exhaustion_failures`：池满导致分配失败几次（**这是池太小的唯一证据**）；
/// - `rejected_layout`：请求过大/对齐不足被拒几次（"无碎片"这条论证的代价被计了多少）；
/// - `rejected_releases`：重复释放/越界被拒几次（内存管理出错的直接信号）；
/// - `empty`：现在是否全空闲（有没有长期泄漏）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryReport {
    /// 池总字节数。
    pub total_bytes: u32,
    /// 单块字节数。
    pub block_bytes: u32,
    /// 总块数。
    pub blocks: u32,
    /// 当前占用块数。
    pub used_blocks: u32,
    /// 历史最高同时占用块数。
    pub high_water_blocks: u32,
    /// 因池满而失败的分配次数。
    pub exhaustion_failures: u32,
    /// 因请求过大/对齐不足而拒绝的次数。
    pub rejected_layout: u32,
    /// 被拒绝的释放次数（重复释放 + 越界）。
    pub rejected_releases: u32,
    /// 当前是否全空闲。
    pub empty: bool,

    /// 大块池块大小（64 KiB）。
    pub big_block_bytes: u32,
    /// 大块池总块数。
    pub big_blocks: u32,
    /// 大块池当前占用块数。
    pub big_used_blocks: u32,
    /// 大块池历史最高同时占用块数。
    pub big_high_water_blocks: u32,
    /// 大块池上因请求过大/对齐不足而拒绝的次数。
    pub big_rejected_layout: u32,
    /// 大块池被拒绝的释放次数。
    pub big_rejected_releases: u32,
}

impl MemoryReport {
    /// 已用字节数。
    #[must_use]
    pub const fn used_bytes(&self) -> u32 {
        self.used_blocks * self.block_bytes
    }

    /// 占用率（整数百分比）。
    #[must_use]
    pub const fn used_percent(&self) -> u32 {
        if self.blocks == 0 {
            return 0;
        }
        (self.used_blocks * 100) / self.blocks
    }

    /// 高水位占池的百分比（"池该多大"的依据）。
    #[must_use]
    pub const fn high_water_percent(&self) -> u32 {
        if self.blocks == 0 {
            return 0;
        }
        (self.high_water_blocks * 100) / self.blocks
    }

    /// 有没有出过"坏信号"（池满、请求被拒、释放被拒）。
    ///
    /// 终端据此决定用 `OK` 还是 `ERR` 标签，并且**退出码跟随它** ——
    /// "能打印出来"不等于"一切正常"（这条是本项目反复吃过的教训）。
    #[must_use]
    pub const fn has_anomaly(&self) -> bool {
        self.exhaustion_failures > 0 || self.rejected_layout > 0 || self.rejected_releases > 0
    }
}

/// 仪表自身的健康度（终端 `rt` 在表格之后打印）。
///
/// 三个字段分别回答三个不同的问题：
/// - `dwt_alive`：周期计数器现在**在走吗**（不在走 ⇒ 上面那张表不可信）；
/// - `dwt_rearms`：它被外部清掉后**被自动重装过几次**（>0 说明"运行期清除"真的发生过）；
/// - `sleeps`：空闲任务累计 `wfi` 睡眠次数（节流有没有生效、有没有被谁拿掉）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrumentHealth {
    /// 周期计数器当前是否在走。
    pub dwt_alive: bool,
    /// 被清掉后自动重装的累计次数。
    pub dwt_rearms: u32,
    /// 空闲睡眠（`wfi`）累计次数。
    pub sleeps: u64,
}

use crate::ansi::{self, Tag, write_ignore};
use crate::commands::{self, CommandEnv, CommandStatus, HistorySource};
use crate::cwd::Cwd;
use crate::editor::{EditorEvent, LineEditor};

/// 文件系统诊断快照：一次取值的**纯数据**。
///
/// # 为什么是快照，而不是让终端去问文件系统
///
/// `shell` 是纯逻辑层，**不能**依赖 `filesystem`（那会把硬件 crate
/// 的依赖闭包带进来，主机测试就没了）。但诊断命令又必须显示真实统计。折中是
/// 这里定义一个只含数字的结构：固件用一次调用把 `FileSystem::stats()` 的结果
/// 搬进来，终端之后只读这份拷贝。
///
/// 每个字段的**含义与口径**由 `filesystem` 定义（本结构只是搬运），终端负责
/// 把它们翻译成现场看得懂的话，但绝不改变口径。特别地：
///
/// - [`Self::live_bytes`] 是"当前可读文件的内容 + 记录头 + 名字"的字节数，
///   **不是**"已用空间"，也不含空闲块；
/// - [`Self::dead_bytes`] 是"写了但已作废"的字节数，要经过 GC 才能变回可写空间；
/// - 日志结构 + GC 的文件系统里，"块数 × 块大小"是**分区容量**，
///   与"当前还能写多少"不是一回事（后者取决于空闲块与回收收益）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsSnapshot {
    /// 是否已挂载。
    ///
    /// 取到快照就意味着文件系统实例已挂载（`fs_snapshot` 拿不到实例时返回 `None`），
    /// 因此当前固件里它恒为 `true`。保留这一位是让快照**自描述**：命令的输出文案
    /// 不必靠"有没有值"去推断状态，将来若出现"实例在、分区未就绪"的状态也不用改结构。
    pub mounted: bool,
    /// 块大小（字节）。文件系统按块擦除，这是磨损统计的分母。
    pub block_size: u32,
    /// 块总数。`block_count × block_size` 是文件系统分到的介质容量。
    pub block_count: u32,
    /// 空闲块数（擦除态、可直接分配）。
    pub free_blocks: u32,
    /// 已被使用的块数（含只装着死记录的块）。
    pub used_blocks: u32,
    /// `live_bytes` / `dead_bytes` 是否真的算过。
    ///
    /// `true` = 固件做过一次**介质扫描**（`filesystem::FileSystem::stats`）；
    /// `false` = 只取了内存索引（`stats_index_only`），此时 [Self::live_bytes] 与
    /// [Self::dead_bytes] 恒为 0，含义是"**需扫描，未计算**"，**不是**"确实是 0"。
    ///
    /// 为什么快照要带这一位：固件在 `with_fs`（`critical_section::with`，**关中断**）
    /// 里取这份数据。主机实测（446 块 × 64 KiB、live 约 18 MiB）一次全盘存活统计要
    /// 读 28.3 MB 介质、44.5 万次 `read`，折算到真机是 0.5 s 量级（计入每次访问开销
    /// 后接近 2 s），而这块板子的 LPUART 接收 FIFO 只有 1 字节深。因此 `fs` 命令走
    /// 的是**不读介质**的入口，并照实说明哪两个量没算 —— 终端不得把 0 当成实测值打印。
    pub liveness_scanned: bool,
    /// 仍然有效的字节数（当前可读文件的内容 + 记录头 + 名字）。
    ///
    /// ⚠️ 仅当 [Self::liveness_scanned] 为 `true` 时才是实测值。
    pub live_bytes: u32,
    /// 已经写出去、但不再有效的字节数（被覆盖的旧版本、墓碑、跳过的坏记录）——
    /// 需 GC 才能变回可写空间。
    ///
    /// ⚠️ 口径按 `filesystem` 的**实现**写：它只数"已写区域里的死字节"，
    /// **不含**块尾尚未使用的空闲区（那些字节本来就能用，算进来会误导出"值得回收"）。
    ///
    /// ⚠️ 仅当 [Self::liveness_scanned] 为 `true` 时才是实测值。
    pub dead_bytes: u32,
    /// 目录里的文件数（不含墓碑）。
    ///
    /// ⚠️ 索引"说不清"的条目（掉电后遗留的半版本）**不计入**：`stats()` 会跳过
    /// `needs_deep()` 的条目，而 `FileSystem::list()` 会再逐级下探把它找回来。
    /// 因此本数字可能**小于**目录里实际可读的文件数，不能当作"文件总数"的硬判据。
    pub file_count: u32,
    /// 因名字索引槽位不足而被忽略的名字数（正常为 0）。
    ///
    /// 非 0 说明有文件读不到了，或读路径会退化为逐级下探的全盘扫描，属于要报的异常。
    pub table_overflow: u32,
    /// 全部块里最小的擦除次数。
    pub erase_min: u32,
    /// 全部块里最大的擦除次数（磨损诊断最关键的一个数）。
    ///
    /// `u32` 而不是 `u16`：器件擦写寿命规格是 10 万次/块，而 `u16` 的上限 65535
    /// 只有这段寿命的 65.5%，到顶后计数静默饱和 —— 那样"磨损百分比"永远读不出
    /// "寿命用尽"。口径由 `filesystem` 的块头定义（见其 `layout` 模块文档）。
    pub erase_max: u32,
    /// 全部块擦除次数之和。
    pub erase_total: u32,
    /// **显式**调用 `FileSystem::gc()` 的次数（入口调用次数，不是回收到的块数）。
    pub gc_explicit_runs: u32,
    /// 写路径**自动**回收的次数（`FileSystem::collect_garbage()` 被调用的次数：
    /// 分配失败后的重试，以及空闲块低于阈值时的收尾回收）。
    ///
    /// 为什么与显式计数分开：固件在生产里走的是自动回收，旧的单字段口径下它恒为 0，
    /// 而终端把它标成"显式 gc() N 次（自动回收不计入）"—— 那句话字面容易被读成
    /// "从未回收过"。拆成两个字段之后，"两个都是 0"才是真的"从未回收过"。
    pub gc_auto_runs: u32,
    /// 内存索引是否**尚未与介质核对**（`filesystem` 写路径的脏位）。
    ///
    /// `true` = 上一次写操作没走完，索引派生字段可能过期。终端据此如实说明
    /// "索引未核对"，而不是把一个可能过期的数字当实测值打印。
    ///
    /// 这一位是**索引侧入口唯一能给出的坏消息**：`stats_index_only()` 不读介质，
    /// 因此它不可能"读失败"；它只可能"没核对过"。
    pub index_dirty: bool,
}

/// 固件要实现的能力集合：终端只通过它获取信息与触发动作。
pub trait ShellContext {
    /// 用户名（提示符里的 `root`）。
    fn user(&self) -> &str;
    /// 主机名（提示符里的 `tessera`）。
    fn host(&self) -> &str;
    /// 当前路径（提示符里的 `~`；本项目没有文件系统，但现场习惯这个位置）。
    fn cwd(&self) -> &str;
    /// 固件版本。
    fn version(&self) -> &str;
    /// 开机以来的毫秒数。
    fn uptime_ms(&self) -> u64;
    /// 内核时钟（Hz）。
    fn clock_hz(&self) -> u32;
    /// 一行行状态（`free`/`health` 等由固件填）：每调用一次 `write_line` 写一行。
    fn status_lines(&self, out: &mut dyn core::fmt::Write);
    /// 创建目录。返回 `false` = 不支持/失败。
    fn fs_mkdir(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }
    /// 删除空目录。
    fn fs_rmdir(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }
    /// 改名。
    fn fs_rename(&mut self, old: &str, new: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (old, new, out);
        false
    }
    /// 创建硬链接。
    fn fs_link(&mut self, existing: &str, new: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (existing, new, out);
        false
    }
    /// 删除文件/空目录项。
    fn fs_unlink(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 文件系统诊断快照。
    ///
    /// 返回 `None` 表示**当前取不到**：文件系统未挂载成功，或正被别的上下文占用
    /// （全局实例的借用失败）。命令要照实说明"不可用"，**不得**打印一排 0 ——
    /// 那会让人误以为"文件系统是空的、一切正常"。
    ///
    /// ⚠️ 实现注意：取这份数据可能触发一次全盘扫描（`stats()` 的语义如此），
    /// 因此只应在**空闲任务**（本项目的命令分发点）里调用，不要放进中断。
    fn fs_snapshot(&self) -> Option<FsSnapshot>;

    /// 文件系统诊断快照，**允许做一次全盘存活统计**（`fs scan`）。
    ///
    /// 默认实现退回 [`Self::fs_snapshot`]：不支持扫描的实现仍能正常响应，
    /// 只是不会把 `liveness_scanned` 置真。实现必须把这个调用放在**属主上下文**
    /// （不关中断）里；`fs` 的普通路径仍然只取内存索引。
    fn fs_scan_snapshot(&self) -> Option<FsSnapshot> {
        self.fs_snapshot()
    }

    /// 显式垃圾回收。返回 `false` = 不支持或失败。
    ///
    /// 这是**长操作**（要读、搬移、擦除块），只能在显式终端命令里触发；
    /// 实现必须放在属主上下文，不得关中断执行。
    fn fs_gc(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }
    /// 请求取消当前后台维护作业。返回 `false` = 不支持/没有可取消作业。
    fn fs_cancel(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }
    /// 实时性实测快照（关中断 / ISR / 终端输入 / 存储访问的耗时统计）。
    ///
    /// 返回 `None` 表示**当前取不到实测数据**，命令要照实说明，**不得**打印一张
    /// 全零的表 —— 那会被现场读成"耗时极短、预算充裕"，而事实是这一项没被测过。
    /// 这与 [`Self::fs_snapshot`] 的 `None` 是同一条纪律，理由是同一个：
    /// 设备上最危险的假数据不是"错的数"，而是"**没有的数被显示成 0**"。
    ///
    /// # 为什么直接用 `realtime::RtMetrics`，而不是像 `FsSnapshot` 那样再造一个本地镜像结构
    ///
    /// 判据只有一条：**这个类型所在的 crate 是不是纯逻辑层**。`realtime` 是纯逻辑
    /// crate（零寄存器访问、零 `unsafe`、可主机测试；`cargo xtask gate layering`
    /// 验证它的依赖闭包里没有 `imxrt-*` / `cortex-m` / `rtic`），因此终端可以直接依赖它、
    /// 直接在 trait 上用它的类型 —— 与 `shell` 早就依赖 `primitives` 同理。
    /// [`FsSnapshot`] 走的是另一条路（在本 crate 里重新声明一遍字段），理由见它自己的文档。
    ///
    /// 能直接借类型就不要再抄一遍字段：**抄一遍就会有两份口径**，而实时预算的口径
    /// （单位、`max` 是"自复位以来"、超预算判据是"严格大于"）正是最不能有第二份的东西。
    ///
    /// # 默认实现返回 `None` 的用意
    ///
    /// 这是**新增**能力，给一个"取不到"的默认实现，别的 `ShellContext` 实现者
    /// 就不会因为多了一个方法而编译失败。默认值必须是 `None`（"没有数据"），
    /// 分配池状态；`None` = 本实现不提供（终端会照实说"取不到"）。
    fn memory_report(&self) -> Option<MemoryReport> {
        None
    }

    /// 仪表**自身**的健康度。
    ///
    /// # 为什么这需要单独一项
    ///
    /// `rt` 表里的数字全部来自 `DWT` 周期计数器；如果那个计数器被外部清掉（真机上真的
    /// 发生过，见 AGENTS.md 坑清单），表会继续显示**陈旧样本**、结论仍是"在预算内" ——
    /// 那是**假通过**。所以除了"数字是多少"，还必须能问"这些数字还可信吗"。
    ///
    /// 与 [`ShellContext::rt_metrics`] 同一条纪律：设备侧不提供时返回 `None`，
    /// 由终端明说"取不到"，绝不打一行看起来正常的零。
    fn instrument_health(&self) -> Option<InstrumentHealth> {
        None
    }

    /// **不能**是 `Some(RtMetrics::new())`：后者是一张全零的表，等于编造数据。
    fn rt_metrics(&self) -> Option<RtMetrics> {
        None
    }
    /// 请求清空实时统计（保留预算）。返回**是否真的受理**。
    ///
    /// # 为什么要在 trait 上再开一个方法（而不是让终端自己去复位）
    ///
    /// 终端是纯逻辑 crate，**不能**调 `board_support::rt::reset()`（那会引入硬件依赖，
    /// 见 `check-layering`）。而"复位后再观察一段"是 `realtime` 自己在文档里
    /// 推荐的用法（`max` 的口径是"自复位以来"）：不复位就只能重启设备才能观察
    /// 一个具体的测试动作，代价远大于这个方法带来的 trait 表面增长。
    ///
    /// 这里的写法与 [`Self::request_reboot`] 完全同构：**终端只发起，固件受理**。
    ///
    /// # 为什么返回 `bool`，而不是给一个"默默成功"的默认实现
    ///
    /// 默认实现返回 `false`（**不受理**），`rt reset` 就会照实说"本实现不支持复位统计"。
    /// 若默认实现什么都不做却让命令打印"已复位"，那就是**假装成功**——
    /// 本项目明令禁止（未实现要留痕、要明确报错，见 `AGENTS.md` 铁律 6）。
    fn rt_reset(&mut self) -> bool {
        false
    }
    /// SDRAM 自检；返回是否全部通过。
    fn run_memtest(&mut self, out: &mut dyn core::fmt::Write) -> bool;
    /// 请求重启（终端只负责发起）。
    fn request_reboot(&mut self);

    /// 强制把日志队列推进一次（不保证一定落盘）。返回 `false` = 不支持。
    fn log_flush(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 在活动日志中查找子串并输出匹配行。返回 `false` = 不支持/失败。
    fn log_grep(&mut self, pattern: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (pattern, out);
        false
    }

    /// 解压全部归档窗口到 `/log/restored.log`。返回 `false` = 不支持/失败。
    fn log_restore(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 只恢复最近 `count` 个归档窗口；`None` 表示全部。
    ///
    /// 默认实现退回 [`Self::log_restore`]，因此旧的实现者不需要改动。
    fn log_restore_count(&mut self, count: Option<usize>, out: &mut dyn core::fmt::Write) -> bool {
        let _ = count;
        self.log_restore(out)
    }

    /// 手动把活动日志压缩归档并清空。返回 `false` = 不支持/失败。
    fn log_rotate(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 逻辑清空活动日志（覆盖为短标记，不删除文件）。返回 `false` = 不支持。
    fn log_clear(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 打印文本日志状态。返回 `false` = 本实现不支持。
    fn log_stats(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 把日志同时输出到终端（`log on` / `log off`）。返回 `false` = 不支持。
    fn log_set_terminal(&mut self, enabled: bool, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (enabled, out);
        false
    }

    /// 设置/查询日志级别阈值（`log level [e|w|i|d]`）。空串 = 查询。
    ///
    /// 返回 `false` = 不支持或参数非法；**不猜默认级别**。
    fn log_set_level(&mut self, level: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (level, out);
        false
    }

    /// 打印上一轮崩溃前保留的 `.uninit` 日志尾部（`log prev`）。
    fn log_previous(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 本次挂载是否来自持久化索引（`fs index`）。
    fn fs_index_loaded(&self) -> bool {
        false
    }

    /// 把当前文件系统索引写入持久化保留块（`fs checkpoint`）。
    fn fs_checkpoint(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 目录是否存在（隐式目录：至少有一个文件以该前缀开头；根总存在）。
    fn fs_is_dir(&mut self, path: &str) -> bool {
        let _ = path;
        false
    }

    /// 列出目录（只读）。返回 `false` = 本实现不支持。
    fn fs_list(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 读取并打印文件（只读）。返回 `false` = 本实现不支持。
    fn fs_cat(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 把文件以 gzip 容器追加到 `/log/archive.gz`。返回 `false` = 不支持。
    fn gzip_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 解压 gzip 文件到 `/log/unpacked.log`。返回 `false` = 不支持/失败。
    fn gunzip_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 统计文件字节数与行数。返回 `false` = 不支持/失败。
    fn wc_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 输出文件头部（前若干行）。返回 `false` = 本实现不支持。
    fn head_file(&mut self, path: &str, max_lines: usize, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, max_lines, out);
        false
    }

    /// 输出文件尾部（最后若干行）。返回 `false` = 本实现不支持。
    fn tail_file(&mut self, path: &str, max_lines: usize, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, max_lines, out);
        false
    }

    /// 打印文件元数据（只读）。返回 `false` = 本实现不支持。
    fn fs_stat(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let _ = (path, out);
        false
    }

    /// 校验全部归档窗口的 gzip 完整性。
    fn log_verify(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 审计一条已提交的命令行（默认忽略）。
    fn audit_command(&mut self, line: &str) {
        let _ = line;
    }

    /// 目标端硬件自检。返回 `false` = 有失败项或本实现不支持（默认）。
    fn selftest(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 台架用 Flash 擦/写基准。返回 `false` = 本实现不支持（默认）。
    fn flash_bench(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// 台架用故障注入。返回 `false` = 本实现不支持（默认）。
    ///
    /// 实现在触发后通常**不会返回**（记录现场并复位）；返回 `true` 只用于
    /// "注入入口存在但没有真的触发"这种异常情况，命令会把它当失败。
    fn inject_fault(&mut self, kind: crate::commands::FaultKind) -> bool {
        let _ = kind;
        false
    }

    /// 文件系统**写队列**动作：投递一条写请求，或查队列统计。
    ///
    /// 返回 `false` = 本实现不支持（默认实现如此）。命令据此报"不可用"，
    /// **不假装成功** —— 与 `fs_snapshot()` 返回 `None` 时打印"取不到"同一口径。
    ///
    /// # 为什么是"动作枚举"而不是三个方法
    ///
    /// 三种写操作（整体写/追加/删除）加一个查询共享同一套参数解析与同一条错误口径，
    /// 拆成四个方法会让 trait 表面翻倍，而调用点只多一个 `match`。
    fn fs_write_queue(
        &mut self,
        action: crate::commands::FsWriteAction<'_>,
        out: &mut dyn core::fmt::Write,
    ) -> bool {
        let _ = (action, out);
        false
    }

    /// 心跳输出控制（**默认关闭**：进入终端后不主动输出）。
    ///
    /// 返回 `false` = 本实现不支持（默认实现如此），命令会照实说明。
    fn beat_control(
        &mut self,
        action: crate::commands::BeatAction,
        out: &mut dyn core::fmt::Write,
    ) -> bool {
        let _ = (action, out);
        false
    }

    /// 打印**启动期原始细节**（PIT 寄存器、FlexSPI/FlexCAN 探针、MPU 区域表、DWT 分段回读）。
    ///
    /// 这些内容刻意不放进启动横幅：横幅要"一眼能读完"，原始寄存器留给按需查看。
    fn diag_lines(&self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }

    /// CAN 控制器的**只读**状态（供 `can` 命令）。
    ///
    /// 返回 `false` = 本实现不提供该信息（默认实现如此），命令会照实说明，
    /// **不假装成功** —— 与 [`Self::rt_metrics`] 返回 `None` 同一口径。
    ///
    /// 为什么值得有一个真实现在这里：CAN 是平台上**第一个自研寄存器级驱动**，
    /// 它的探针读数（`MCR`/`ECR`/`ESR1`/`IFLAG1`、PE 时钟与实际波特率）
    /// 是现场排"总线静默"的第一手材料 —— 终端不该只会在启动日志里翻它。
    fn can_status(&self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = out;
        false
    }
}

/// 终端外壳。
pub struct Shell<const LINE: usize, const HIST: usize> {
    /// 行编辑器（行缓冲、历史、转义序列状态）。
    editor: LineEditor<LINE, HIST>,
    /// 当前命令的退出状态：非 0 时 `feed` 会在提示符前给出可视标记。
    last_status: CommandStatus,
    /// 当前工作目录（Linux 风格绝对路径；`cd` 修改）。
    cwd: Cwd,
}

impl<const LINE: usize, const HIST: usize> Shell<LINE, HIST> {
    /// 新建外壳。
    pub const fn new() -> Self {
        Self {
            editor: LineEditor::new(),
            last_status: CommandStatus::Ok,
            cwd: Cwd::new(),
        }
    }

    /// 启动横幅（Fedora 风格：版本行 + 提示"help 查看命令"）。
    ///
    /// 横幅存在的意义是**自证身份**：现场可能同时挂着好几台设备，
    /// 一眼看到版本号与主机名，才知道自己连的是哪一块板子的哪一版固件。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn banner(&self, ctx: &dyn ShellContext, out: &mut dyn core::fmt::Write) {
        write_ignore!(
            out,
            "纯逻辑终端：输入 `{GREEN}help{RESET}` 查看命令，Tab 补全，上下键翻历史。\r\n",
            GREEN = ansi::GREEN,
            RESET = ansi::RESET
        );
        write_ignore!(
            out,
            "{}未实现项用 `gaps` 查看；原始寄存器/探针详情用 `diag`；心跳默认关闭（`beat on` 开启）。{RESET}\r\n",
            ansi::DIM,
            RESET = ansi::RESET
        );
        let _ = ctx;
    }

    /// 最近一条命令的退出状态。
    ///
    /// 固件可以据此设置告警灯/上报；测试用它断言"非 0 语义"而不必解析输出文本。
    #[must_use]
    pub const fn last_status(&self) -> CommandStatus {
        self.last_status
    }

    /// 打印提示符。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn prompt(&self, ctx: &dyn ShellContext, out: &mut dyn core::fmt::Write) {
        // 上一行命令非 0 时先打一个标记：现场脚本抓串口输出时，"哪条命令失败了"
        // 需要一个不依赖命令自身输出的判定点。
        if self.last_status == CommandStatus::NonZero {
            ansi::tag(out, Tag::Err);
            write_ignore!(out, " 上一条命令未成功{RESET}\r\n", RESET = ansi::RESET);
        }
        let cwd = if self.cwd.as_str() == "/" {
            "~"
        } else {
            self.cwd.as_str()
        };
        ansi::prompt(out, ctx.user(), ctx.host(), cwd, is_root(ctx.user()));
    }

    /// 喂一个字节（固件中断/空闲循环里调用）。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    pub fn feed(&mut self, byte: u8, ctx: &mut dyn ShellContext, out: &mut dyn core::fmt::Write) {
        // 先把提示符缓存进编辑器：之后的重绘（插入/退格/Ctrl-L/补全）都要用它。
        // 只在这里刷新一次，是因为**普通按键不改变**用户名/主机名/路径；而每个
        // 字节都重渲染一次提示符，在 115200 的串口上等于给每次按键多加两次
        // 格式化——没必要。命令执行之后再刷新一次（见下），那才是唯一会变的时候。
        self.refresh_prompt(ctx);

        let event = self.editor.feed(byte, out);
        match event {
            EditorEvent::Pending => {}
            EditorEvent::Submitted => {
                // 提交：行内容先取出来（随后刷历史要可变借用编辑器）。
                let mut line_buffer = [0u8; LINE];
                let length = self.copy_line(&mut line_buffer);
                let line = match line_buffer.get(..length) {
                    Some(bytes) => core::str::from_utf8(bytes).unwrap_or(""),
                    None => "",
                };
                self.editor.push_history();
                self.editor.clear_line();
                write_ignore!(out, "\r\n");
                self.last_status = self.run_line(line, ctx, out);
                // 命令可能改变了提示符内容（例如改了 `cwd`），因此重刷缓存后再输出。
                self.refresh_prompt(ctx);
                self.prompt(ctx, out);
            }
            EditorEvent::Cancelled => {
                // Ctrl-C：丢弃当前行，**不执行**任何东西。回显 `^C` 是现场的
                // 直观期待（敲错了要看得见"我确实取消了"）。
                self.editor.clear_line();
                write_ignore!(out, "^C\r\n");
                self.last_status = CommandStatus::Ok;
                self.prompt(ctx, out);
            }
            EditorEvent::Complete => {
                self.complete(out);
            }
        }
    }

    /// 执行一行（提交时由 `feed` 调用）。
    ///
    /// 返回退出状态，便于测试直接断言语义（不必先去解析输出文本）。
    pub fn run_line(
        &mut self,
        line: &str,
        ctx: &mut dyn ShellContext,
        out: &mut dyn core::fmt::Write,
    ) -> CommandStatus {
        let (name, args) = commands::parse(line);
        // 空行不报错：连续回车是现场最常见的动作，报 `command not found: `
        // 只会把屏幕刷满噪声。
        if name.is_empty() {
            return CommandStatus::Ok;
        }
        ctx.audit_command(line);
        match commands::lookup(name) {
            Some(command) => {
                let mut env = CommandEnv {
                    ctx,
                    out,
                    args,
                    cwd: &mut self.cwd,
                    history: &self.editor,
                };
                command.run(&mut env)
            }
            None => {
                commands::not_found(out, name);
                CommandStatus::NonZero
            }
        }
    }

    /// 把当前行拷进调用方的缓冲区，返回字节数。
    ///
    /// 为什么不直接借用 `editor.line()`：`run_line` 要 `&mut self`（它得构造
    /// 借用编辑器历史的命令环境），而 `line()` 借用 `self.editor`——两者不能同时
    /// 成立。零分配的替代方案就是一块定长栈缓冲：行容量本来就是常量泛型，
    /// 栈上放得下（`LINE` 是几百字节量级，而 Cortex-M7 的栈有几十 KB）。
    fn copy_line(&self, buffer: &mut [u8; LINE]) -> usize {
        let source = self.editor.line().as_bytes();
        let take = source.len().min(LINE);
        for (slot, byte) in buffer.iter_mut().zip(source.iter()) {
            *slot = *byte;
        }
        take
    }

    /// 刷新提示符缓存：渲染一次提示符交给编辑器，重绘时直接复用。
    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn refresh_prompt(&mut self, ctx: &dyn ShellContext) {
        // 提示符可能比 `LINE` 长（长主机名 + 长路径），这里给足 128 字节；
        // 溢出时编辑器只存前 128 字节——显示会截断，但行内容与命令分发不受影响。
        let mut buffer = [0u8; 128];
        let length = {
            let mut cursor = SliceWriter {
                buffer: &mut buffer,
                length: 0,
            };
            let cwd = if self.cwd.as_str() == "/" {
                "~"
            } else {
                self.cwd.as_str()
            };
            ansi::prompt(
                &mut cursor,
                ctx.user(),
                ctx.host(),
                cwd,
                is_root(ctx.user()),
            );
            cursor.length
        };
        match buffer.get(..length) {
            Some(bytes) => self.editor.set_prompt(bytes),
            None => self.editor.set_prompt(b""),
        }
    }

    /// Tab 补全：唯一匹配则补全，多匹配则列出候选并重绘提示行。
    fn complete(&mut self, out: &mut dyn core::fmt::Write) {
        // 已经在行里敲了参数时不做补全：本终端只补命令名，硬补会破坏参数。
        let prefix = match self.editor.line().find(char::is_whitespace) {
            Some(_) => return,
            None => self.editor.line(),
        };
        let mut count = 0usize;
        let mut only: Option<&'static commands::Command> = None;
        commands::for_each_match(prefix, |command| {
            count = count.saturating_add(1);
            if count == 1 {
                only = Some(command);
            }
        });

        match (count, only) {
            (1, Some(command)) => {
                // 补全文本先渲染进栈缓冲，再交给编辑器整行替换：编辑器只认
                // `&str`，而本 crate 零分配，`String` 不是选项。
                let mut buffer = [0u8; 32];
                let full = commands::completion_text(command, &mut buffer);
                // 补全后重绘整行：编辑器知道提示符，也知道光标该在哪。
                self.editor.apply_completion(full, out);
            }
            (0, _) => {
                // 无匹配：不改行。补全失败是常态（用户还在思考命令名），
                // 此时清行或报错都会打断输入。
            }
            _ => {
                // 多匹配：先列出候选（换行输出），再重绘提示行与当前输入。
                write_ignore!(out, "\r\n");
                ansi::tag(out, Tag::Info);
                write_ignore!(out, " 候选：{}", ansi::RESET);
                commands::for_each_match(prefix, |command| {
                    write_ignore!(out, " {}", command.name);
                });
                write_ignore!(out, "\r\n");
                // 重绘：把提示符与已输入前缀重新画出来，光标回到行尾。
                self.editor.redraw_line(out);
            }
        }
    }
}

impl<const LINE: usize, const HIST: usize> Default for Shell<LINE, HIST> {
    fn default() -> Self {
        Self::new()
    }
}

/// 只读历史视图：`history` 命令通过它读取编辑器里的历史。
///
/// 实现放在 `shell` 而不是 `editor`：编辑器不需要知道"命令"这个概念，
/// 而这里恰好是"编辑器 + 命令表"的汇合点。
impl<const LINE: usize, const HIST: usize> HistorySource for LineEditor<LINE, HIST> {
    fn history_len(&self) -> usize {
        self.history_len()
    }

    fn history_get(&self, index: usize) -> Option<&str> {
        self.history_get(index)
    }
}

/// 判断是否是 root：本项目没有用户数据库，用户名即身份。
///
/// 固件把 `user` 写成 `root` 就是 root 提示符（`#`），否则 `$`。
/// 这比"再加一个 `is_root()` 方法"更少一处可写错的状态。
fn is_root(user: &str) -> bool {
    user == "root"
}

/// 写进定长切片的 `core::fmt::Write`：用于渲染提示符。
///
/// 为什么需要它：本 crate 没有 `String`/`heapless`，而 `ansi::prompt` 要一个
/// `&mut dyn Write`。这个 8 行的适配器把两者接起来，写满就丢弃（提示符被截断
/// 只是显示问题，行编辑与命令分发不受影响）。
struct SliceWriter<'a> {
    buffer: &'a mut [u8],
    length: usize,
}

impl core::fmt::Write for SliceWriter<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let remaining = self.buffer.len().saturating_sub(self.length);
        let take = bytes.len().min(remaining);
        if let (Some(target), Some(source)) = (
            self.buffer
                .get_mut(self.length..self.length.saturating_add(take)),
            bytes.get(..take),
        ) {
            for (slot, byte) in target.iter_mut().zip(source.iter()) {
                *slot = *byte;
            }
        }
        self.length = self.length.saturating_add(take);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use crate::shell::InstrumentHealth;
    use realtime::{Cycles, RtMetrics, Span, cycles_to_micros};

    use super::{FsSnapshot, Shell, ShellContext};
    use crate::commands::CommandStatus;

    /// 固件替身：记录每个"能力"被调用的次数。
    ///
    /// 这是本 crate 最重要的一类测试——命令有没有**真的**触发固件动作
    /// （而不是只打印了一行字），只有在这里才能验证。
    struct FakeContext {
        user: &'static str,
        host: &'static str,
        cwd: &'static str,
        version: &'static str,
        uptime_ms: u64,
        clock_hz: u32,
        status_lines: &'static [&'static str],
        memtest_ok: bool,
        /// 文件系统快照；`None` 模拟"未挂载 / 正被占用"。
        fs: Option<FsSnapshot>,
        /// 实时性实测快照；`None` 模拟"周期计数器没开 / 本实现不提供"。
        rt: Option<RtMetrics>,
        health: Option<InstrumentHealth>,
        memory: Option<crate::shell::MemoryReport>,
        /// `rt reset` 是否受理（模拟固件实现了 / 没实现这个能力）。
        rt_reset_ok: bool,
        // `status_lines` 在 trait 里是 `&self`（终端只需要读状态），因此计数器
        // 必须内部可变：`Cell` 在 `core` 里，且本项目禁 unsafe，这是零成本的唯一选择。
        status_calls: Cell<usize>,
        memtest_calls: Cell<usize>,
        reboot_calls: Cell<usize>,
        fs_calls: Cell<usize>,
        rt_calls: Cell<usize>,
        rt_reset_calls: Cell<usize>,
    }

    impl FakeContext {
        fn new() -> Self {
            Self {
                user: "root",
                host: "tessera",
                cwd: "~",
                version: "0.2.0-test",
                uptime_ms: 3_661_000,
                clock_hz: 528_000_000,
                status_lines: &["healthy", "can: down"],
                memtest_ok: true,
                fs: Some(normal_snapshot()),
                rt: Some(normal_metrics()),

                memory: Some(crate::shell::MemoryReport {
                    total_bytes: 2 * 1024 * 1024,
                    block_bytes: 1024,
                    blocks: 2048,
                    used_blocks: 3,
                    high_water_blocks: 3,
                    exhaustion_failures: 0,
                    rejected_layout: 1,
                    rejected_releases: 0,
                    empty: false,
                    big_block_bytes: 64 * 1024,
                    big_blocks: 32,
                    big_used_blocks: 0,
                    big_high_water_blocks: 0,
                    big_rejected_layout: 0,
                    big_rejected_releases: 0,
                }),
                health: Some(InstrumentHealth {
                    dwt_alive: true,
                    dwt_rearms: 0,
                    sleeps: 1234,
                }),
                rt_reset_ok: true,
                status_calls: Cell::new(0),
                memtest_calls: Cell::new(0),
                reboot_calls: Cell::new(0),
                fs_calls: Cell::new(0),
                rt_calls: Cell::new(0),
                rt_reset_calls: Cell::new(0),
            }
        }
    }

    /// 一份"正常"的快照：数字取自真机首次上电的形状（446 块 × 64 KiB）。
    fn normal_snapshot() -> FsSnapshot {
        FsSnapshot {
            mounted: true,
            block_size: 65_536,
            block_count: 446,
            free_blocks: 434,
            used_blocks: 12,
            liveness_scanned: true,
            live_bytes: 10_240,
            dead_bytes: 2_048,
            file_count: 3,
            table_overflow: 0,
            erase_min: 7,
            erase_max: 42,
            erase_total: 1_234,
            gc_explicit_runs: 0,
            gc_auto_runs: 0,
            index_dirty: false,
        }
    }

    /// 一份"每个 span 都有样本、且都在预算内"的实测统计。
    ///
    /// 周期数按 `primitives::budget::CPU_HZ`（396 MHz，约 396 周期/µs）折算，
    /// 取值都远低于各自的预算：关中断 80 µs、时基 ISR 20 µs、控制台 ISR 20 µs、
    /// 终端输入 1000 µs、存储临界区 80 µs。
    ///
    /// 为什么**每个** span 都要给样本：测试要断言"表格里每一行 span 名都在"，
    /// 若某个 span 没有样本，那一行就会被"还没有样本"的提示掩盖，断言也就白写了。
    fn normal_metrics() -> RtMetrics {
        let mut metrics = RtMetrics::new();
        for (span, cycles) in [
            (Span::IrqOff, 12_000),          // ≈30 µs / 预算 80
            (Span::IsrDeadlineTick, 4_000),  // ≈10 µs / 预算 20
            (Span::IsrConsoleRx, 2_000),     // ≈5 µs / 预算 20
            (Span::ShellInput, 40_000),      // ≈101 µs / 预算 1000
            (Span::StorageCritical, 8_000),  // ≈20 µs / 预算 80
            (Span::BootInit, 20_000),        // ≈50 µs / 预算 50000（启动期，故意宽松）
            (Span::IdleStep, 8_000),         // ≈20 µs / 预算 1000
            (Span::BootPost, 1_306_800_000), // ≈3.3 s / 预算 4 s（启动期，实测值量级）
        ] {
            metrics.record(span, Cycles::new(cycles).unwrap());
        }
        // ⚠️ 夹具必须覆盖**全部** span：漏一个的话，那一行会被"还没有样本"的提示掩盖，
        // 别处的断言（"表格里每一行 span 名都在"）就白写了。将来新增 span 时，
        // 这里会**明确失败**并指出是哪一个，而不是打乱别人的测试。
        let missing: Vec<&str> = Span::ALL
            .iter()
            .copied()
            .filter(|span| metrics.stats(*span).count == 0)
            .map(Span::name)
            .collect();
        assert!(
            missing.is_empty(),
            "normal_metrics 夹具缺这些 span 的样本：{missing:?}"
        );
        metrics
    }

    impl ShellContext for FakeContext {
        fn user(&self) -> &str {
            self.user
        }
        fn host(&self) -> &str {
            self.host
        }
        fn cwd(&self) -> &str {
            self.cwd
        }
        fn version(&self) -> &str {
            self.version
        }
        fn uptime_ms(&self) -> u64 {
            self.uptime_ms
        }
        fn clock_hz(&self) -> u32 {
            self.clock_hz
        }
        fn status_lines(&self, out: &mut dyn core::fmt::Write) {
            self.status_calls.set(self.status_calls.get() + 1);
            for line in self.status_lines {
                let _ = core::fmt::Write::write_fmt(out, format_args!("{line}\r\n"));
            }
        }
        fn fs_snapshot(&self) -> Option<FsSnapshot> {
            self.fs_calls.set(self.fs_calls.get() + 1);
            self.fs
        }
        fn fs_scan_snapshot(&self) -> Option<FsSnapshot> {
            // 用两个与普通快照不同的数字，证明 `fs scan` 真的走了扫描入口，
            // 而不是又拿了一次索引侧快照。
            self.fs.map(|mut snapshot| {
                snapshot.live_bytes = 4242;
                snapshot.dead_bytes = 242;
                snapshot
            })
        }
        fn rt_metrics(&self) -> Option<RtMetrics> {
            self.rt_calls.set(self.rt_calls.get() + 1);
            self.rt
        }
        fn instrument_health(&self) -> Option<InstrumentHealth> {
            self.health
        }
        fn memory_report(&self) -> Option<crate::shell::MemoryReport> {
            self.memory
        }
        fn rt_reset(&mut self) -> bool {
            self.rt_reset_calls.set(self.rt_reset_calls.get() + 1);
            if !self.rt_reset_ok {
                return false;
            }
            // 受理：真的把统计清空（与固件侧 `board_support::rt::reset()` 同义）。
            // 替身必须**真的**改状态，否则"复位"这条测试只能证明打印了一行字。
            if let Some(metrics) = self.rt.as_mut() {
                metrics.reset();
            }
            true
        }
        fn run_memtest(&mut self, out: &mut dyn core::fmt::Write) -> bool {
            self.memtest_calls.set(self.memtest_calls.get() + 1);
            let _ = core::fmt::Write::write_str(out, "  区段 0x8000_0000..0x8100_0000 OK\r\n");
            self.memtest_ok
        }
        fn request_reboot(&mut self) {
            self.reboot_calls.set(self.reboot_calls.get() + 1);
        }
        fn head_file(
            &mut self,
            path: &str,
            max_lines: usize,
            out: &mut dyn core::fmt::Write,
        ) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("head:{path}:{max_lines}\r\n"));
            true
        }
        fn tail_file(
            &mut self,
            path: &str,
            max_lines: usize,
            out: &mut dyn core::fmt::Write,
        ) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("tail:{path}:{max_lines}\r\n"));
            true
        }
        fn wc_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("wc:{path}\r\n"));
            true
        }
        fn gunzip_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("gunzip:{path}\r\n"));
            true
        }
        fn log_grep(&mut self, pattern: &str, out: &mut dyn core::fmt::Write) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("grep:{pattern}\r\n"));
            true
        }
        fn log_restore(&mut self, out: &mut dyn core::fmt::Write) -> bool {
            let _ = core::fmt::Write::write_str(out, "restore:ok\r\n");
            true
        }
        fn log_restore_count(
            &mut self,
            count: Option<usize>,
            out: &mut dyn core::fmt::Write,
        ) -> bool {
            let _ = core::fmt::Write::write_fmt(out, format_args!("restore:{count:?}\r\n"));
            true
        }
    }

    /// 测试驱动：把外壳、上下文、输出绑在一起。
    struct Harness {
        shell: Shell<128, 8>,
        ctx: FakeContext,
        out: String,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                shell: Shell::new(),
                ctx: FakeContext::new(),
                out: String::new(),
            }
        }

        /// 逐字节喂入（模拟串口中断）。
        fn feed(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.shell.feed(*byte, &mut self.ctx, &mut self.out);
            }
        }

        /// 敲一行命令并回车，返回退出状态。
        fn run(&mut self, line: &str) -> CommandStatus {
            self.out.clear();
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\r');
            self.feed(&bytes);
            self.shell.last_status()
        }
    }

    #[test]
    fn 横幅提示_help_与_diag_且不再重复设备身份() {
        let shell = Shell::<64, 4>::new();
        let ctx = FakeContext::new();
        let mut out = String::new();
        shell.banner(&ctx, &mut out);
        // 身份行由固件的统一启动报告打印（避免重复）；终端横幅只负责"怎么用"。
        assert!(
            !out.contains("0.2.0-test"),
            "终端横幅不应重复版本行：{out:?}"
        );
        assert!(out.contains("help"), "{out:?}");
        assert!(out.contains("diag"), "{out:?}");
        assert!(out.contains("beat on"), "{out:?}");
    }

    #[test]
    fn 提示符含用户名主机名与路径() {
        let shell = Shell::<64, 4>::new();
        let ctx = FakeContext::new();
        let mut out = String::new();
        shell.prompt(&ctx, &mut out);
        assert!(out.contains("root"), "{out:?}");
        assert!(out.contains("tessera"), "{out:?}");
        assert!(out.contains('~'), "{out:?}");
        assert!(out.ends_with("# \u{1b}[0m"), "{out:?}");
    }

    #[test]
    fn 非_root_用户提示符用美元号() {
        let shell = Shell::<64, 4>::new();
        let mut ctx = FakeContext::new();
        ctx.user = "operator";
        let mut out = String::new();
        shell.prompt(&ctx, &mut out);
        assert!(out.ends_with("$ \u{1b}[0m"), "{out:?}");
    }

    #[test]
    fn 逐字节输入并回车会执行命令() {
        let mut harness = Harness::new();
        harness.feed(b"echo hi\r");
        assert!(harness.out.contains("hi"), "{:?}", harness.out);
        // 回显 + 执行 + 新提示符：提示符至少出现两次（首次输出与提交后）。
        assert!(
            harness.out.contains("root"),
            "提交后必须重新给出提示符：{:?}",
            harness.out
        );
    }

    #[test]
    fn 回车空行不报错() {
        let mut harness = Harness::new();
        let status = harness.run("");
        assert_eq!(status, CommandStatus::Ok, "{:?}", harness.out);
        assert!(
            !harness.out.contains("command not found"),
            "{:?}",
            harness.out
        );
    }

    #[test]
    fn help_列出全部命令与别名() {
        let mut harness = Harness::new();
        harness.run("help");
        for command in crate::commands::COMMANDS {
            assert!(
                harness.out.contains(command.name),
                "help 缺少 {}：{:?}",
                command.name,
                harness.out
            );
        }
        // 别名也要出现（现场靠肌肉记忆敲 `?`、`stat`）。
        assert!(harness.out.contains('?'), "{:?}", harness.out);
        assert!(harness.out.contains("stat"), "{:?}", harness.out);
    }

    #[test]
    fn version_与_uname_a() {
        let mut harness = Harness::new();
        harness.run("version");
        assert!(harness.out.contains("0.2.0-test"), "{:?}", harness.out);

        harness.run("uname -a");
        assert!(harness.out.contains("0.2.0-test"), "{:?}", harness.out);
        assert!(harness.out.contains("tessera"), "{:?}", harness.out);

        // `uname` 少了 `-a`：给用法并返回非 0（不猜参数）。
        let status = harness.run("uname");
        assert_eq!(status, CommandStatus::NonZero, "{:?}", harness.out);
        assert!(harness.out.contains("uname -a"), "{:?}", harness.out);
    }

    #[test]
    fn uptime_与_free_带单位() {
        let mut harness = Harness::new();
        harness.run("uptime");
        // 3_661_000 ms = 1h 1m 1s，人类可读格式里三段都要在。
        assert!(harness.out.contains("01:01:01"), "{:?}", harness.out);

        harness.run("free");
        assert!(harness.out.contains("528000000"), "{:?}", harness.out);
        assert!(harness.out.contains("Hz"), "{:?}", harness.out);
        assert!(harness.out.contains("ms"), "{:?}", harness.out);
    }

    #[test]
    fn echo_保留参数内部空格() {
        let mut harness = Harness::new();
        harness.run("echo hello world");
        assert!(harness.out.contains("hello world"), "{:?}", harness.out);

        // 多个空格不被压掉：串口脚本用 echo 打标记时依赖原文。
        harness.run("echo  a   b ");
        assert!(harness.out.contains("a   b"), "{:?}", harness.out);
    }

    #[test]
    fn whoami_与_hostname() {
        let mut harness = Harness::new();
        harness.run("whoami");
        assert!(harness.out.contains("root"), "{:?}", harness.out);
        harness.run("hostname");
        assert!(harness.out.contains("tessera"), "{:?}", harness.out);
    }

    #[test]
    fn 未知命令报错且是非零() {
        let mut harness = Harness::new();
        let status = harness.run("frobnicate");
        assert_eq!(status, CommandStatus::NonZero);
        assert!(
            harness.out.contains("command not found"),
            "{:?}",
            harness.out
        );
        assert!(harness.out.contains("frobnicate"), "{:?}", harness.out);
        assert!(
            harness.out.contains("\u{1b}[31m"),
            "错误应当是红色：{:?}",
            harness.out
        );
    }

    #[test]
    fn 大小写敏感() {
        let mut harness = Harness::new();
        let status = harness.run("HELP");
        assert_eq!(status, CommandStatus::NonZero, "{:?}", harness.out);
        assert!(
            harness.out.contains("command not found"),
            "{:?}",
            harness.out
        );
    }

    #[test]
    fn 多个空格与前导空格都能解析() {
        let mut harness = Harness::new();
        let status = harness.run("   echo    spaced");
        assert_eq!(status, CommandStatus::Ok, "{:?}", harness.out);
        assert!(harness.out.contains("spaced"), "{:?}", harness.out);
    }

    #[test]
    fn status_调用固件状态行() {
        let mut harness = Harness::new();
        harness.run("status");
        assert_eq!(harness.ctx.status_calls.get(), 1, "status 必须调用一次固件");
        assert!(harness.out.contains("healthy"), "{:?}", harness.out);
        assert!(harness.out.contains("can: down"), "{:?}", harness.out);
    }

    #[test]
    fn memtest_调用固件并报_pass() {
        let mut harness = Harness::new();
        let status = harness.run("memtest");
        assert_eq!(harness.ctx.memtest_calls.get(), 1);
        assert_eq!(status, CommandStatus::Ok);
        assert!(harness.out.contains("PASS"), "{:?}", harness.out);
        assert!(harness.out.contains("错误计数"), "{:?}", harness.out);
    }

    #[test]
    fn memtest_失败时报_fail_且非零() {
        let mut harness = Harness::new();
        harness.ctx.memtest_ok = false;
        let status = harness.run("memtest");
        assert_eq!(harness.ctx.memtest_calls.get(), 1);
        assert_eq!(status, CommandStatus::NonZero);
        assert!(harness.out.contains("FAIL"), "{:?}", harness.out);
    }

    #[test]
    fn reboot_触发固件请求() {
        let mut harness = Harness::new();
        let status = harness.run("reboot");
        assert_eq!(harness.ctx.reboot_calls.get(), 1, "reboot 必须真的请求固件");
        assert_eq!(status, CommandStatus::Ok);
    }

    #[test]
    fn clear_发出清屏序列() {
        let mut harness = Harness::new();
        harness.run("clear");
        assert!(harness.out.contains("\u{1b}[2J"), "{:?}", harness.out);
    }

    #[test]
    fn colors_打印调色板() {
        let mut harness = Harness::new();
        harness.run("colors");
        for name in ["RED", "GREEN", "YELLOW", "BLUE", "MAGENTA", "CYAN"] {
            assert!(harness.out.contains(name), "缺少 {name}：{:?}", harness.out);
        }
    }

    #[test]
    fn gaps_逐行列出未实现项() {
        let mut harness = Harness::new();
        let status = harness.run("gaps");
        assert_eq!(status, CommandStatus::Ok);
        // 断言"每条都带标记与里程碑编号"，刻意拆成前缀 + 编号两段匹配：
        // 本 crate 源码里不写出完整的缺口标记字面量，`check-gaps` 的清点
        // 口径才只统计**实现点**，不把这条命令的复述也算成缺口。
        assert!(harness.out.contains("GAP"), "{:?}", harness.out);
        for milestone in ["P2)", "P3)", "P4)", "P5)"] {
            assert!(
                harness.out.contains(milestone),
                "缺少里程碑 {milestone}：{:?}",
                harness.out
            );
        }
        // 关键词要出现：现场靠它判断"CAN 收发 / 看门狗 / 升级现在是什么状态"。
        assert!(harness.out.contains("CAN"), "{:?}", harness.out);
        assert!(harness.out.contains("看门狗"), "{:?}", harness.out);
    }

    #[test]
    fn can_命令在有实现时输出状态在无实现时诚实拒绝() {
        // 真实现分支：状态**原样**输出（内容由固件填入），退出码为零。
        struct CanContext;
        impl ShellContext for CanContext {
            fn user(&self) -> &str {
                "root"
            }
            fn host(&self) -> &str {
                "tessera"
            }
            fn cwd(&self) -> &str {
                "~"
            }
            fn version(&self) -> &str {
                "0.0.0-test"
            }
            fn uptime_ms(&self) -> u64 {
                0
            }
            fn clock_hz(&self) -> u32 {
                0
            }
            fn status_lines(&self, _out: &mut dyn core::fmt::Write) {}
            fn fs_snapshot(&self) -> Option<FsSnapshot> {
                None
            }
            fn run_memtest(&mut self, _out: &mut dyn core::fmt::Write) -> bool {
                false
            }
            fn request_reboot(&mut self) {}
            fn can_status(&self, out: &mut dyn core::fmt::Write) -> bool {
                let _ = core::fmt::Write::write_str(out, "mcr=0x5980000f PE=12000000Hz");
                true
            }
        }

        let mut shell = Shell::<64, 8>::new();
        let mut ctx = CanContext;
        let mut out = String::new();
        let status = shell.run_line("can", &mut ctx, &mut out);
        assert_eq!(status, CommandStatus::Ok, "{out:?}");
        assert!(out.contains("mcr=0x5980000f"), "{out:?}");

        // 默认实现分支：必须明说"本实现不提供"，而不是假装成功。
        let mut ctx = MinimalContext;
        out.clear();
        let status = shell.run_line("can", &mut ctx, &mut out);
        assert_eq!(status, CommandStatus::NonZero, "{out:?}");
        assert!(out.contains("本实现不提供"), "{out:?}");
    }

    #[test]
    fn history_在提交后列出历史() {
        let mut harness = Harness::new();
        harness.run("echo one");
        harness.run("echo two");
        harness.run("echo three");
        harness.run("history");
        // 提交过的三条都要在：`history` 的价值就是"我前面到底敲了什么"。
        assert!(harness.out.contains("echo one"), "{:?}", harness.out);
        assert!(harness.out.contains("echo two"), "{:?}", harness.out);
        assert!(harness.out.contains("echo three"), "{:?}", harness.out);
        // `history` 自己也被记进去了（它在执行前入历史）。
        assert!(harness.out.contains("history"), "{:?}", harness.out);
    }

    #[test]
    fn ctrl_c_取消当前行不执行命令() {
        let mut harness = Harness::new();
        harness.out.clear();
        // 敲一条会触发固件动作的命令，但用 Ctrl-C 取消。
        harness.feed(b"reboot\x03");
        assert_eq!(harness.ctx.reboot_calls.get(), 0, "取消的行绝不能被执行");
        assert!(harness.out.contains("^C"), "{:?}", harness.out);
        // 行被清空：随后回车不该执行 `reboot`。
        harness.feed(b"\r");
        assert_eq!(harness.ctx.reboot_calls.get(), 0, "取消之后行必须是空的");
    }

    #[test]
    fn tab_唯一匹配补全并加空格() {
        let mut harness = Harness::new();
        harness.feed(b"hel\t");
        // 补全成 `help `，随后回车即执行 help。
        harness.out.clear();
        harness.feed(b"\r");
        assert!(harness.out.contains("可用命令"), "{:?}", harness.out);
    }

    #[test]
    fn tab_无匹配不改行() {
        let mut harness = Harness::new();
        harness.feed(b"zz\t");
        harness.out.clear();
        harness.feed(b"\r");
        // 行仍是 `zz` → 未知命令；如果 Tab 把行清空了，这里就不会报错。
        assert!(
            harness.out.contains("command not found"),
            "{:?}",
            harness.out
        );
        assert!(harness.out.contains("zz"), "{:?}", harness.out);
    }

    #[test]
    fn tab_多匹配打印候选并保留已输入内容() {
        let mut harness = Harness::new();
        harness.out.clear();
        harness.feed(b"c\t");
        assert!(harness.out.contains("候选"), "{:?}", harness.out);
        // 候选里要有同前缀的命令。
        assert!(harness.out.contains("clear"), "{:?}", harness.out);
        assert!(harness.out.contains("can"), "{:?}", harness.out);
        // 已输入的前缀不被丢掉：补全只补不删。
        harness.feed(b"lear\r");
        assert!(
            harness.out.contains("\u{1b}[2J"),
            "clear 应当被执行：{:?}",
            harness.out
        );
    }

    #[test]
    fn 非零退出后提示符给出标记() {
        let mut harness = Harness::new();
        harness.run("nosuchcmd");
        // 新提示符之前应当有 ERR 标记，脚本据此判断上一条失败。
        let tail = harness.out.clone();
        assert!(tail.contains("ERR"), "{tail:?}");
    }

    #[test]
    fn 历史被上翻后可直接回车重跑() {
        let mut harness = Harness::new();
        harness.run("whoami");
        harness.out.clear();
        // 上键翻出上一条（whoami），回车重跑。
        harness.feed(b"\x1b[A");
        harness.feed(b"\r");
        assert!(harness.out.contains("root"), "{:?}", harness.out);
    }

    /// 剥掉 ANSI 转义序列：断言文案时不受配色码干扰。
    ///
    /// 颜色是行为约定（错误必须红），但它不该让"文案里有没有这个数字"这类断言
    /// 变成一串 `\u{1b}[36m` 的拼接。
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for ch in text.chars() {
            if in_escape {
                if ch == 'm' {
                    in_escape = false;
                }
                continue;
            }
            if ch == '\u{1b}' {
                in_escape = true;
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn head_tail_wc_gunzip_grep参数与分发() {
        let mut harness = Harness::new();
        harness.feed(b"head -n 2 /log/x\r");
        assert!(harness.out.contains("head:/log/x:2"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"tail -n 3 /log/x\r");
        assert!(harness.out.contains("tail:/log/x:3"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"wc /log/x\r");
        assert!(harness.out.contains("wc:/log/x"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"gunzip /log/x.gz\r");
        assert!(
            harness.out.contains("gunzip:/log/x.gz"),
            "{:?}",
            harness.out
        );
        harness.out.clear();
        harness.feed(b"log grep boot\r");
        assert!(harness.out.contains("grep:boot"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"log restore\r");
        assert!(harness.out.contains("restore:ok"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"log restore 2\r");
        assert!(harness.out.contains("restore:Some(2)"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"log restore 0\r");
        assert!(
            harness.out.contains("用法：log restore"),
            "{:?}",
            harness.out
        );
        harness.out.clear();
        harness.feed(b"log restore x\r");
        assert!(
            harness.out.contains("用法：log restore"),
            "{:?}",
            harness.out
        );
    }

    #[test]
    fn head_tail_非法参数给出用法() {
        let mut harness = Harness::new();
        harness.feed(b"head -n 2 relative\r");
        assert!(harness.out.contains("用法：head"), "{:?}", harness.out);
        harness.out.clear();
        harness.feed(b"tail -n 2 relative\r");
        assert!(harness.out.contains("用法：tail"), "{:?}", harness.out);
    }

    #[test]
    fn fs_命令打印快照的全部关键数字与单位() {
        let mut harness = Harness::new();
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        // 命令必须**真的**向固件要了快照，而不是打印了一份写死的表。
        assert_eq!(harness.ctx.fs_calls.get(), 1, "{text:?}");
        // 挂载状态：`fs` 要回答的第一个问题。
        assert!(text.contains("已挂载"), "{text:?}");
        // 容量：块数、块大小、折算后的 KiB 都要在，且注明是"分区容量"而非可用空间。
        assert!(text.contains("446 块 × 65536 B"), "{text:?}");
        assert!(text.contains("28544 KiB"), "{text:?}");
        assert!(text.contains("不是“当前可写入量”"), "{text:?}");
        // 可写入量、已用块。
        assert!(text.contains("空闲 434 块"), "{text:?}");
        assert!(text.contains("12 / 446"), "{text:?}");
        // 数据占用（有效 / 可回收）与文件数。
        assert!(text.contains("10240 B"), "{text:?}");
        assert!(text.contains("2048 B"), "{text:?}");
        assert!(text.contains("文件数  : 3 个"), "{text:?}");
        // 磨损三个量：最重 / 最轻 / 合计 + 平均。
        assert!(text.contains("最重一块 42 次"), "{text:?}");
        assert!(text.contains("最轻 7 次"), "{text:?}");
        assert!(text.contains("合计 1234 次"), "{text:?}");
        assert!(text.contains("平均 2 次/块"), "{text:?}");
        // 寿命口径必须是"按器件规格"，且给出参考值 10 万次 —— 不伪造成实测值。
        assert!(text.contains("按器件规格"), "{text:?}");
        assert!(text.contains("100000"), "{text:?}");
        // 回收两个口径都要写清，否则 0 会被误读成"从未回收"。
        // （口径说明由 `fs_回收次数分开报显式与自动` 逐字钉住。）
        assert!(text.contains("显式 gc() 0 次"), "{text:?}");
        assert!(text.contains("自动回收 0 次"), "{text:?}");
    }

    #[test]
    fn fs_scan_走全盘统计入口而非法参数给用法() {
        let mut harness = Harness::new();
        let status = harness.run("fs scan");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("4242 B"), "{text:?}");
        assert!(text.contains("242 B"), "{text:?}");

        harness.out.clear();
        let status = harness.run("fs deep");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(
            text.contains("用法：fs [scan|gc|checkpoint|cancel|index]"),
            "{text:?}"
        );
    }

    #[test]
    fn fs_不可用时明确说明且不打印统计数字() {
        let mut harness = Harness::new();
        harness.ctx.fs = None;
        let status = harness.run("fs");
        let text = plain(&harness.out);
        // 非 0：脚本据此判定"这条命令没拿到数据"。
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("文件系统不可用"), "{text:?}");
        // 关键：**不得**打印任何统计行/数字，否则会被读成"空文件系统，一切正常"。
        assert!(!text.contains("文件数"), "{text:?}");
        assert!(!text.contains("磨损"), "{text:?}");
        assert!(!text.contains("已挂载"), "{text:?}");
        assert!(!text.contains("446"), "{text:?}");
        assert!(!text.contains("KiB"), "{text:?}");
        // 出处提示必须给：现场要能自己往下查。
        assert!(text.contains("启动横幅"), "{text:?}");
    }

    #[test]
    fn fs_快照自报未挂载时与不可用同样处理() {
        let mut harness = Harness::new();
        let snapshot = harness.ctx.fs.map(|mut value| {
            value.mounted = false;
            value
        });
        harness.ctx.fs = snapshot;
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("文件系统不可用"), "{text:?}");
        // 即使快照里带着数字，未挂载时也一个都不打印。
        assert!(!text.contains("446"), "{text:?}");
        assert!(!text.contains("文件数"), "{text:?}");
    }

    #[test]
    fn fs_磨损过半时给出显眼告警() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.erase_max = 60_000;
            snapshot.erase_total = 600_000;
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("警告"), "{text:?}");
        assert!(text.contains("60000 次"), "{text:?}");
        assert!(text.contains("60%"), "{text:?}");
        // 告警必须带颜色：串口日志一屏一屏刷，文本前缀容易被忽略。
        assert!(harness.out.contains("\u{1b}[33m"), "{text:?}");
    }

    #[test]
    fn fs_磨损很低时不误报告警() {
        let mut harness = Harness::new();
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(!text.contains("警告"), "{text:?}");
        // 但仍要给出量级判断（0%），而不是只罗列次数。
        assert!(text.contains("已用 0%"), "{text:?}");
    }

    /// `u16` 时代的 65535 是"计数到顶"，现在它只是寿命的 65%：照 65% 报，
    /// 而且**不得**再出现"计数饱和、无法反映真实磨损"那类提示。
    ///
    /// 这条测试是"加宽到 u32"这个改动的反向判据：如果谁把计数改回 `u16`，
    /// 65535 又会变成饱和点 —— 那时磨损百分比会停在 65% 而不再增长。
    #[test]
    fn fs_擦除计数到_u16_上限不再是饱和点() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.erase_max = u32::from(u16::MAX);
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        // 数字照给，百分比按 65535 / 100000 = 65% 算（过半 → 有寿命告警）。
        assert!(text.contains("65535"), "{text:?}");
        assert!(text.contains("65%"), "{text:?}");
        assert!(text.contains("警告"), "{text:?}");
        // 关键：不再有"计数饱和"这一类提示（u16 版式的症状）。
        assert!(!text.contains("u16"), "{text:?}");
        assert!(!text.contains("计数饱和"), "{text:?}");
        assert!(!text.contains("真实次数只会更多"), "{text:?}");
        assert!(!text.contains("用尽"), "{text:?}");
    }

    /// 达到器件规格寿命（10 万次/块）时必须判为**用尽** —— 这条判断在 `u16`
    /// 版式下永远不可能成立（65535 < 100000），因此它同时也是"计数能表达到寿命"
    /// 的证据。超过寿命时照实报 >100%，不封顶。
    #[test]
    fn fs_擦除计数达到器件寿命时判为用尽() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.erase_max = 300_000;
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("300000 次"), "{text:?}");
        assert!(text.contains("300%"), "{text:?}");
        assert!(text.contains("达到/超过器件规格寿命"), "{text:?}");
        assert!(text.contains("100000 次"), "{text:?}");
        // 用尽的措辞与"过半提醒"不同：处置不一样。
        assert!(!text.contains("请检查 GC 与写入模式"), "{text:?}");
        assert!(harness.out.contains("\u{1b}[33m"), "{text:?}");
    }

    /// 回收次数必须**分开**报：显式 `gc()` 与写路径的自动回收。
    ///
    /// 旧版把两者合成一个数（只数显式），于是生产固件里恒为 0，再配上一句
    /// "自动回收不计入此计数"，字面很容易被读成"从未回收过"。
    #[test]
    fn fs_回收次数分开报显式与自动() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.gc_explicit_runs = 2;
            snapshot.gc_auto_runs = 9;
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("显式 gc() 2 次"), "{text:?}");
        assert!(text.contains("自动回收 9 次"), "{text:?}");
        assert!(text.contains("合计 11 次"), "{text:?}");
        // 口径说明里不得再出现"不计入"这种会让人读成"没回收过"的说法。
        assert!(!text.contains("不计入"), "{text:?}");
    }

    /// 两个回收计数都是 0 时，输出才真的在说"从未回收过"。
    #[test]
    fn fs_两个回收计数都为零时报从未回收() {
        let mut harness = Harness::new();
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("显式 gc() 0 次 / 自动回收 0 次"), "{text:?}");
        assert!(text.contains("合计 0 次"), "{text:?}");
    }

    /// 索引没和介质核对过：必须明说下面的数字是"上次已知"，而不是当实测值打印。
    ///
    /// 这是索引侧入口（`stats_index_only`，不读介质）**唯一**能报的坏消息：
    /// 它不可能"读失败"，只可能"没核对过"。
    #[test]
    fn fs_索引未核对时明确说数字可能过期() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.index_dirty = true;
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("尚未与介质核对"), "{text:?}");
        assert!(text.contains("上次已知"), "{text:?}");
        assert!(harness.out.contains("\u{1b}[33m"), "{text:?}");
        // 索引派生数字照常给：命令在"索引脏"时仍然有用（死板地不打印等于让
        // 现场在最需要诊断的时候失去入口）。
        assert!(text.contains("空闲 434 块"), "{text:?}");
    }

    #[test]
    fn fs_索引溢出时给出告警() {
        let mut harness = Harness::new();
        if let Some(snapshot) = harness.ctx.fs.as_mut() {
            snapshot.table_overflow = 5;
        }
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("索引溢出"), "{text:?}");
        assert!(text.contains("5 个名字被丢弃"), "{text:?}");
    }

    /// 一份"只取了内存索引、没做存活统计"的快照。
    ///
    /// 固件终端的 `fs` 走的就是这条路径：`live_bytes`/`dead_bytes` 需要全盘扫描
    /// 介质才能得出，而取快照的 `with_fs` 是关中断的临界区。
    fn index_only_snapshot() -> FsSnapshot {
        FsSnapshot {
            liveness_scanned: false,
            live_bytes: 0,
            dead_bytes: 0,
            ..normal_snapshot()
        }
    }

    #[test]
    fn fs_未做存活统计时照实说未计算而不是打印零() {
        let mut harness = Harness::new();
        harness.ctx.fs = Some(index_only_snapshot());
        let status = harness.run("fs");
        let text = plain(&harness.out);
        // 命令本身成功（索引派生字段拿到了），但必须把"没算"说出来。
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("需扫描，未计算"), "{text:?}");
        // 关键：不得出现任何会被读成实测值的 0 字节口径。
        assert!(!text.contains("有效 0 B"), "{text:?}");
        assert!(!text.contains("死数据 0 B"), "{text:?}");
        assert!(!text.contains("有效 0 B"), "{text:?}");
        // 索引派生字段照常给：空闲块、已用块、文件数、磨损都不能因为"没扫描"而消失。
        assert!(text.contains("空闲 434 块"), "{text:?}");
        assert!(text.contains("12 / 446"), "{text:?}");
        assert!(text.contains("文件数  : 3 个"), "{text:?}");
        assert!(text.contains("最重一块 42 次"), "{text:?}");
        // 告警色：这一节"没有数据"要显眼，避免被当成正常输出扫过去。
        assert!(harness.out.contains("\u{1b}[33m"), "{text:?}");
    }

    #[test]
    fn fs_做过存活统计时正常打印两个字节口径() {
        // 反向对照：把 `liveness_scanned` 置真（例如将来接上不关中断的扫描通道），
        // 两个量就必须真的打出来。否则上面那条测试可能因为"分支写反了"而假通过。
        let mut harness = Harness::new();
        let status = harness.run("fs");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(!text.contains("需扫描，未计算"), "{text:?}");
        assert!(text.contains("有效 10240 B"), "{text:?}");
        assert!(text.contains("死数据 2048 B"), "{text:?}");
    }

    #[test]
    fn fs_别名_df_可用() {
        let mut harness = Harness::new();
        let status = harness.run("df");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(
            text.contains("文件系统（filesystem：日志结构 + GC）"),
            "{text:?}"
        );
        assert_eq!(harness.ctx.fs_calls.get(), 1, "{text:?}");
    }

    // ------------------------------------------------------------------ rt

    #[test]
    fn rt_打印结论行与每个_span_的实测行() {
        let mut harness = Harness::new();
        let status = harness.run("rt");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        // 命令必须**真的**向固件要了快照，而不是打印一份写死的表。
        assert_eq!(harness.ctx.rt_calls.get(), 1, "{text:?}");
        // 结论在最上面：现场扫一眼就知道"预算还成不成立"。
        assert!(text.contains("实时预算：全部在预算内"), "{text:?}");
        assert!(!text.contains("超预算"), "{text:?}");
        // 单位与 `max` 的口径必须在表**之前**说清（`report` 的表头不带单位）。
        assert!(text.contains("单位：微秒"), "{text:?}");
        assert!(text.contains("自上次复位以来"), "{text:?}");
        // 表头来自 `realtime::RtMetrics::report`。
        assert!(text.contains("count"), "{text:?}");
        assert!(text.contains("余量"), "{text:?}");
        // 每一行 span 名都要在：少一行就可能把出问题的那一项藏起来。
        for span in Span::ALL.iter().copied() {
            // 名字先取出来再断言：`assert!` 的格式化参数只在**失败时**求值，
            // 直接写在消息里会留下一行永远跑不到的代码（覆盖率会如实报出来）。
            let name = span.name();
            assert!(text.contains(name), "表格缺少 {name}：{text:?}");
        }
        // 每个 span 都有样本时，不该出现"还没有样本"的提示。
        assert!(!text.contains("还没有样本"), "{text:?}");
    }

    /// `mem` 把池状态摊开给人看：占用、高水位、三类异常计数都要在。
    #[test]
    fn mem_打印池状态与异常计数() {
        let mut harness = Harness::new();
        let status = harness.run("mem");
        let text = plain(&harness.out);
        // 夹具里 `rejected_layout: 1` ⇒ 有异常 ⇒ 必须是非零退出码。
        assert_ne!(
            status,
            CommandStatus::Ok,
            "有异常计数就不该算成功：{text:?}"
        );
        assert!(text.contains("分配池"), "{text:?}");
        assert!(text.contains("2048"), "{text:?}");
        assert!(text.contains("1024 B 固定块"), "{text:?}");
        assert!(text.contains("高水位"), "{text:?}");
        assert!(text.contains("池满 0 次"), "{text:?}");
        assert!(text.contains("请求被拒 1 次"), "{text:?}");
        // 池不是通用堆这件事必须写在输出里（否则读者会以为它能满足任意大小请求）。
        assert!(text.contains("非通用堆"), "{text:?}");
    }

    /// 一次异常都没有时必须是 `OK`（并且说清"全空闲"= 没有长期泄漏）。
    #[test]
    fn mem_无异常时判成功并说明全空闲() {
        let mut harness = Harness::new();
        harness.ctx.memory = Some(crate::shell::MemoryReport {
            total_bytes: 2 * 1024 * 1024,
            block_bytes: 1024,
            blocks: 2048,
            used_blocks: 0,
            high_water_blocks: 7,
            exhaustion_failures: 0,
            rejected_layout: 0,
            rejected_releases: 0,
            empty: true,
            big_block_bytes: 64 * 1024,
            big_blocks: 32,
            big_used_blocks: 0,
            big_high_water_blocks: 0,
            big_rejected_layout: 0,
            big_rejected_releases: 0,
        });
        let status = harness.run("mem");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("全空闲"), "{text:?}");
        assert!(text.contains("高水位 7 块"), "{text:?}");
    }

    /// 取不到就照实说，并且**不算成功**（不能打一行看起来正常的零）。
    #[test]
    fn mem_取不到时如实说且判失败() {
        let mut harness = Harness::new();
        harness.ctx.memory = None;
        let status = harness.run("mem");
        let text = plain(&harness.out);
        assert_ne!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("取不到"), "{text:?}");
    }

    /// `rt` 表之后必须回答"这些数字还可信吗" —— 这是仪表健康的唯一可查询出口。
    #[test]
    fn rt_表之后给出仪表健康度() {
        let mut harness = Harness::new();
        harness.ctx.health = Some(InstrumentHealth {
            dwt_alive: true,
            dwt_rearms: 0,
            sleeps: 1234,
        });
        let status = harness.run("rt");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("计数在走"), "{text:?}");
        assert!(text.contains("运行期未发生过清除"), "{text:?}");
        assert!(text.contains("空闲睡眠 1234 次"), "{text:?}");
    }

    /// 最危险的情形：计数器已停 ⇒ 上面的表不可信，必须说出来，并且判失败。
    ///
    /// 为什么这条最重要：计数器停摆时表里会继续显示陈旧样本、结论仍是"在预算内"。
    /// 一张看起来健康的表配一个停摆的计数器，比没有仪表更危险。
    #[test]
    fn rt_计数器已停时必须说清不可信且判失败() {
        let mut harness = Harness::new();
        harness.ctx.health = Some(InstrumentHealth {
            dwt_alive: false,
            dwt_rearms: 3,
            sleeps: 7,
        });
        let status = harness.run("rt");
        let text = plain(&harness.out);
        assert!(text.contains("已停"), "{text:?}");
        assert!(text.contains("不可信"), "{text:?}");
        assert!(text.contains("自动重装过 3 次"), "{text:?}");
        assert_ne!(status, CommandStatus::Ok, "仪表失效不该算成功：{text:?}");
    }

    /// 一条样本都没收到时：结论仍是"在预算内"（判据是**超预算次数**，不是耗时），
    /// 但必须明确说清 `count=0` 的行不是"耗时为零"。
    #[test]
    fn rt_没有样本时说清没测过而不是耗时为零() {
        let mut harness = Harness::new();
        harness.ctx.rt = Some(RtMetrics::new());
        let status = harness.run("rt");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("实时预算：全部在预算内"), "{text:?}");
        assert!(text.contains("还没有样本"), "{text:?}");
        assert!(
            text.contains(&format!("{} 项", Span::ALL.len())),
            "要说清有几项没有样本：{text:?}"
        );
        // "没有样本"也要看得见是**哪几项**没有样本。
        for span in Span::ALL.iter().copied() {
            let name = span.name();
            assert!(text.contains(name), "表格缺少 {name}：{text:?}");
        }
    }

    #[test]
    fn rt_超预算时给出结论数字与明细() {
        let mut harness = Harness::new();
        let mut metrics = normal_metrics();
        // 控制台接收 ISR 的预算只有 20 µs（LPUART1 的 FIFO 只有 1 字节深），
        // 这里塞一个远超它的样本（1e6 周期 ≈ 2525 µs），模拟"某次中断里干了重活"。
        metrics.record(Span::IsrConsoleRx, Cycles::new(1_000_000).unwrap());
        harness.ctx.rt = Some(metrics);

        let status = harness.run("rt");
        let text = plain(&harness.out);
        // 超预算 = 实时预算没达成：与 `memtest` 判 FAIL 同一口径，退出状态非零。
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("有 1 项超预算"), "{text:?}");
        assert!(text.contains("超预算明细"), "{text:?}");

        // 具体数字：哪个 span、实测多少 µs、预算多少 µs、超了几次。
        // 期望值用 `realtime` 的换算函数算出，而不是把 µs 数写死在测试里
        // （CPU_HZ 改了，这条断言跟着变，而不是变成一句测试里的假话）。
        let max_micros = cycles_to_micros(Cycles::new(1_000_000).unwrap());
        assert!(max_micros > 20, "样本必须真的超预算：{max_micros} µs");
        assert!(text.contains("isr-lpuart1"), "{text:?}");
        assert!(
            text.contains(&format!("实测 {max_micros} µs > 预算 20 µs（1 次）")),
            "{text:?}"
        );
        // 明细之外，表格里的 max 列也要有这个数（否则"表 + 明细"两处不一致）。
        assert!(
            text.matches(&format!("{max_micros}")).count() >= 2,
            "表格与明细都该出现 {max_micros}：{text:?}"
        );
        // 超预算必须带颜色：串口日志一屏一屏刷，文本容易被扫过去。
        assert!(harness.out.contains("\u{1b}[31m"), "{text:?}");
    }

    #[test]
    fn rt_取不到实测数据时明确说不可用且不含任何统计数字() {
        let mut harness = Harness::new();
        harness.ctx.rt = None;
        let status = harness.run("rt");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        // 先要有个明确的"取不到"。
        assert!(text.contains("取不到"), "{text:?}");
        assert!(text.contains("没被测过"), "{text:?}");
        // 关键：**不得**打印任何统计行/数字，否则会被读成"空统计、一切正常"。
        assert!(!text.contains("span"), "{text:?}");
        assert!(!text.contains("余量"), "{text:?}");
        assert!(!text.contains('%'), "{text:?}");
        assert!(!text.contains("预算内"), "{text:?}");
        for span in Span::ALL.iter().copied() {
            assert!(!text.contains(span.name()), "{text:?}");
        }
        // 最强的判据：整段输出里**没有阿拉伯数字**（回显、提示符、文案都没有）。
        // 只要有人往这条分支里塞一个 `0`，"照实说不可用"就退化成"打印了一个数字"。
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "不可用分支不得出现任何数字：{text:?}"
        );
    }

    #[test]
    fn rt_reset_请求固件复位并真的清空统计() {
        let mut harness = Harness::new();
        let status = harness.run("rt reset");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert_eq!(harness.ctx.rt_reset_calls.get(), 1, "必须真的请求固件");
        assert!(text.contains("已复位实时统计"), "{text:?}");
        // 复位要**真的**生效（替身改了状态），不是只打印了一句话。
        let metrics = harness.ctx.rt.expect("复位之后仍应取得到快照");
        for span in Span::ALL.iter().copied() {
            let name = span.name();
            assert_eq!(metrics.stats(span).count, 0, "{name} 的统计没有被清空");
        }
        // 复位不打表：刚清空的表全是 `count=0`，正是会被误读的形状。
        assert!(!text.contains("span"), "{text:?}");
    }

    #[test]
    fn rt_reset_不受理时照实说未复位() {
        let mut harness = Harness::new();
        harness.ctx.rt_reset_ok = false;
        let status = harness.run("rt reset");
        let text = plain(&harness.out);
        // 默认实现（不受理）必须明确报错，**不得**假装成功。
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("不支持复位"), "{text:?}");
        assert!(!text.contains("已复位"), "{text:?}");
        assert_eq!(harness.ctx.rt_reset_calls.get(), 1, "{text:?}");
    }

    #[test]
    fn rt_不认识的参数给用法且非零() {
        let mut harness = Harness::new();
        let status = harness.run("rt wibble");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("rt: 仅支持 rt reset"), "{text:?}");
        // 参数错误时不该顺手去取快照（更不该打表）。
        assert_eq!(harness.ctx.rt_calls.get(), 0, "{text:?}");
        assert_eq!(harness.ctx.rt_reset_calls.get(), 0, "{text:?}");
    }

    #[test]
    fn rt_别名_lat_走同一条命令() {
        let mut harness = Harness::new();
        let status = harness.run("lat");
        let text = plain(&harness.out);
        assert_eq!(status, CommandStatus::Ok, "{text:?}");
        assert!(text.contains("实时预算：全部在预算内"), "{text:?}");
        assert_eq!(harness.ctx.rt_calls.get(), 1, "{text:?}");
    }

    /// 把两种 `rt` 的实际输出原文打进测试日志（`--nocapture` 可见）。
    ///
    /// 这条测试的作用是**留下现场会看到的原文**：交付报告、文档与真机比对都以它为准，
    /// 而不是靠嘴描述格式。它同时也断言样例里该有的行都在 —— 只打印不检查的测试
    /// 会在某天输出变成一句错误信息时**照样通过**。
    #[test]
    fn rt_打印输出样例供现场对照() {
        // 样例一：正常（全部在预算内，每个 span 都有样本）。
        let mut harness = Harness::new();
        let status = harness.run("rt");
        let normal = plain(&harness.out);
        println!("---- rt 输出样例①（全部在预算内） ----\n{normal}---- 样例①结束 ----");
        assert_eq!(status, CommandStatus::Ok, "{normal:?}");
        assert!(normal.contains("实时预算：全部在预算内"), "{normal:?}");

        // 样例二：控制台接收 ISR 某次在中断里干了重活（1e6 周期 ≈ 2525 µs > 20 µs）。
        let mut harness = Harness::new();
        let mut metrics = normal_metrics();
        metrics.record(Span::IsrConsoleRx, Cycles::new(1_000_000).unwrap());
        harness.ctx.rt = Some(metrics);
        let status = harness.run("rt");
        let over = plain(&harness.out);
        println!("---- rt 输出样例②（isr-lpuart1 超预算） ----\n{over}---- 样例②结束 ----");
        assert_eq!(status, CommandStatus::NonZero, "{over:?}");
        assert!(over.contains("超预算明细"), "{over:?}");

        // 样例三：一条样本都还没有（刚上电，或刚 `rt reset`）—— 这张表最容易被误读成
        // "耗时为零、余量充足"，所以命令必须把"没测过"这件事**写出来**。
        let mut harness = Harness::new();
        harness.ctx.rt = Some(RtMetrics::new());
        let status = harness.run("rt");
        let fresh = plain(&harness.out);
        println!("---- rt 输出样例③（还没有样本） ----\n{fresh}---- 样例③结束 ----");
        assert_eq!(status, CommandStatus::Ok, "{fresh:?}");
        assert!(fresh.contains("还没有样本"), "{fresh:?}");
    }

    /// 一个**只实现必须实现的方法**的最小上下文：刻意不碰 `rt_metrics` / `rt_reset`。
    ///
    /// 它验证的是新增 trait 方法时最要紧的两件事：别人的 `ShellContext` 不必因为
    /// 多了一个方法而编译失败（默认实现在），而且默认路径必须是**诚实的**
    /// （"取不到" / "不受理"），不能是"一张全零的表"或"假装复位成功"。
    struct MinimalContext;

    impl ShellContext for MinimalContext {
        fn user(&self) -> &str {
            "root"
        }
        fn host(&self) -> &str {
            "tessera"
        }
        fn cwd(&self) -> &str {
            "~"
        }
        fn version(&self) -> &str {
            "0.0.0-min"
        }
        fn uptime_ms(&self) -> u64 {
            0
        }
        fn clock_hz(&self) -> u32 {
            0
        }
        fn status_lines(&self, _out: &mut dyn core::fmt::Write) {}
        fn fs_snapshot(&self) -> Option<FsSnapshot> {
            None
        }
        fn run_memtest(&mut self, _out: &mut dyn core::fmt::Write) -> bool {
            false
        }
        fn request_reboot(&mut self) {}
    }

    #[test]
    fn rt_默认实现对最小实现者照实说取不到与不受理() {
        let mut shell = Shell::<64, 8>::new();
        let mut ctx = MinimalContext;
        let mut out = String::new();

        // 先把这个"只实现了必须实现的方法"的上下文**跑一遍**：新增方法没有把
        // 实现者挡在门外（编译期成立），而且默认能力下终端本身照样可用（运行期成立）。
        // 顺带把 MinimalContext 的每个方法都走到 —— 否则它们就是一堆没人跑的代码。
        for line in [
            "whoami", "hostname", "version", "uptime", "free", "status", "fs", "memtest", "reboot",
        ] {
            out.clear();
            shell.run_line(line, &mut ctx, &mut out);
        }
        // `cwd` 只在提示符渲染里用到（`run_line` 不经过它）。
        shell.prompt(&ctx, &mut out);

        // 默认 `rt_metrics()` 是"取不到实测数据"，而不是一张全零的表。
        assert!(ctx.rt_metrics().is_none(), "默认实现不得编造快照");
        let status = shell.run_line("rt", &mut ctx, &mut out);
        let text = plain(&out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("取不到"), "{text:?}");
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "默认路径也不得出现任何统计数字：{text:?}"
        );

        // 默认 `rt_reset()` 是"不受理"，而不是假装复位成功。
        out.clear();
        assert!(!ctx.rt_reset(), "默认实现不得声称受理了复位");
        let status = shell.run_line("rt reset", &mut ctx, &mut out);
        let text = plain(&out);
        assert_eq!(status, CommandStatus::NonZero, "{text:?}");
        assert!(text.contains("不支持复位"), "{text:?}");
        assert!(!text.contains("已复位"), "{text:?}");
    }
}
