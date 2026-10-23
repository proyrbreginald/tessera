# 中断与 idle 轮询分析（优先级 / 执行公平性）

> 本文回答三个问题：**现在用了哪些中断、各绑定了什么处理**；**idle 里有哪些轮询/推进任务**；
> **优先级与公平性是否需要优化**。
>
> 依据来自源码 + 2026-10-23 真机实测（`tessera 0.1.0.0`，MIMXRT1021CAG4A）。
> 实测抓包：`tmp/evidence/rt-analysis.log`、`tmp/evidence/rt-rotate-twice.log`（`tmp/` 不入库）。
> 相关：`docs/02-架构/并发与实时预算.md`、`docs/04-开发/测试策略.md`。

## 1. 结论速览

1. **RTIC 只绑定了 2 个硬件中断**：`LPUART1`（控制台收/发，priority=1）与 `PIT`（1 ms 时基，priority=1）；没有软件任务（`dispatchers = []`）。
2. **优先级本身目前是合理的**：`console_rx` 与 `deadline_tick` 同为 priority 1，两者互不抢占，但各自 ≤20 µs、相对字节间隔（86.8 µs）与时基周期（1 ms）都有 4×/50× 余量（[`并发与实时预算`](并发与实时预算.md) 已按此口径记录）。`budget::prio` 的 `SERIAL_RX=2`/`CAN_TX=3`/`CAN_RX=4` 指的是**尚未落地的串行总线与 CAN 任务**（P2），不是 `console_rx`——这一点容易误读。真正要防的是**将来接线时优先级漂移**：目前只有 `DEADLINE_TICK == 1` 一条编译期断言。
3. **idle 没有"轮询多个平级任务"的调度器**，而是一个协作式单循环：`Input → Maintenance → Health → 1 Hz 杂项 → wfi`。公平性问题不在"任务之间抢"，而在 **维护步骤无上限**：
   - 真机实测：一次 `log rotate` 让 `idle-step` 达到 **67,973 µs**（预算 1000 µs，超 68 倍）；
   - 同一窗口 `shell-input` 1,162 µs（预算 1000 µs）；
   - 首次归档还测到 `irq-off` **2,886 µs**（预算 80 µs）——第二次归档不再出现，**可定位到 gzip 压缩器状态初始化在临界区内**（`main.rs:424`）。
4. **长维护步骤会直接吃掉输入**（已实测，不是推断）：RX 环形队列 256 B ≈ 22 ms @115200（`console.rs:469`），而单步维护可达 68 ms。在 `log rotate` 启动的同时灌入 1,600 B 输入，真机 `rxdrop=511`、`txdrop=26,502`、`rt=VIOLATION` —— 输入确实丢了。
5. 建议先做三件判据明确的事：**① 把优先级接线变成可失败的判据（当前不改优先级）；② 把 gzip 状态初始化移出临界区；③ 给 idle 的单步工作量设上限（分片再切细）**。详见 §6。

## 2. 中断清单（硬件 IRQ / 异常）

### 2.1 RTIC 绑定的硬件中断

| 中断线 | 处理函数 | priority | 使能路径 | 中断里做什么 | 预算 |
|---|---|---|---|---|---|
| `LPUART1` | `console_rx`（`crates/firmware/src/main.rs:1249`） | **1** | RTIC 按 `binds` 打开 NVIC；外设侧 `RIE`/`TIE` 打开（`console.rs:134`、`:306`） | `console::isr_service`：① 清 RX 错误标志；② 把 RX FIFO 搬进 256 B 环形队列（队满丢最旧并计数）；③ **只写 1 个 TX 字节**，队空则关 `TIE` | 20 µs（`realtime::BUDGET_ISR_CONSOLE_RX_MICROS`） |
| `PIT`（Chan2，1 ms） | `deadline_tick`（`main.rs:1260`） | **1** | RTIC 打开 NVIC；`PIT` 侧 `TIE`（`timer.rs:140`）、`TEN`（`timer.rs:206`） | 清 `TFLG` → 显式 `NVIC::unpend(PIT)` → `TICKS++` → `HEALTH \|= DEADLINE_TICK`；全程在 `rt::measured` 内 | 20 µs（`budget::ISR_DEADLINE_TICK_MICROS`） |

两者都通过 `board_support::rt::measured` 记录实际耗时，超预算会让心跳 `rt=VIOLATION`。

### 2.2 故障异常（不是 RTIC 任务，不受优先级表影响）

| 向量 | 处理 | 策略 |
|---|---|---|
| `HardFault`（槽 3） | `#[exception] unsafe fn HardFault`（`main.rs:1303`）→ `fault_from_frame` | 保存 CFSR/HFSR/入栈帧到跨复位黑匣子，按连续故障次数决定复位或停机 |
| `MemManage` / `BusFault` / `UsageFault`（槽 4–6） | 默认处理器（刻意升级） | `cargo xtask check-fault-handlers` 从**链接产物**断言它们是"真实实现或刻意升级"，不是空桩 |

异常优先级由内核架构固定，不在 `budget::prio` 的讨论范围内。

### 2.3 没有接进来的中断

- **CAN2**：只有内部 loopback 轮询自检，未开 NVIC 中断，也未接入 `Bus` 端口（`GAP(P2)`）。`prio::CAN_RX/CAN_TX` 目前**没有任何 RTIC 任务使用**。
- 其余外设（DMA、ADC、定时器其它通道等）均未使能中断。
- `device.rs` 是 RTIC 的 PAC 垫片；`dispatchers = []` ⇒ **没有软件任务**，所有 `#[task]` 都是硬件中断直绑。

## 3. 优先级：现状、设计表与防漂移

### 3.1 现状（代码事实）

| 优先级 | 任务 | 说明 |
|---|---|---|
| **1** | `console_rx`（`LPUART1`） | 控制台收/发共用一个 ISR；预算 20 µs（`realtime::BUDGET_ISR_CONSOLE_RX_MICROS`） |
| **1** | `deadline_tick`（`PIT`） | 1 ms 时基；预算 20 µs（`budget::ISR_DEADLINE_TICK_MICROS`） |
| **0** | `idle` | 协作式单循环，见 §4 |

两者同优先级 ⇒ **彼此不能抢占**。这是可接受的：PIT 最多把串口接收推迟 20 µs（字节间隔 86.8 µs，约 4× 余量）；串口 ISR 最多把时基推迟 20 µs（时基周期 1000 µs，约 50× 余量）。`并发与实时预算` 已按"当前固件只有 2 个硬件任务、同为优先级 1"记录。

### 3.2 `budget::prio` 里的 2/3/4 是谁

`crates/primitives/src/budget.rs:58-73` 的优先级表是**为尚未落地的总线任务排的**：

| 常量 | 目标任务 | 状态 |
|---|---|---|
| `SERIAL_RX = 2` | **串行总线**（`Bus` 端口）接收任务 | 未落地（P2） |
| `CAN_TX = 3` | CAN 发送完成任务 | 未落地（P2） |
| `CAN_RX = 4` | CAN 接收任务（FIFO 有限） | 未落地（P2） |
| `DEADLINE_TICK = 1` | PIT 时基 | 已落地 |
| `IDLE = 0` | idle | 已落地 |

**注意**：`console_rx` 不是 `SERIAL_RX`。前者是调试控制台（LPUART1），后者是面向业务的串行总线端口；把两者混为一谈会得出"console 应该改成优先级 2"的错误结论。

### 3.3 真正需要优化的地方：防漂移

- `budget.rs:102-105` 的 `const_assert!` 只保证常量之间的顺序；**没有任何断言把 RTIC 的
  `priority = N` 字面量与常量绑定**（全仓只有 `main.rs:1123` 的 `DEADLINE_TICK == 1`）。
- 当 P2 的串行总线/CAN 任务落地时，若只改常量、忘改 `#[task(priority = ...)]`，编译不会失败，
  而 `budget.rs` 里"高优先级 ISR 之和 < 半个字节时间"的论证会**默认成立**却与实际不符。
- 建议（R1）：为每个已落地任务增加编译期断言或注释锚点；在 P2 接线时把
  `ISR_SERIAL_RX_MICROS` / `ISR_CAN_*` 与实测预算一起复核。**当前不需要改优先级。**

## 4. idle 里的"轮询任务"（协作式单循环）

`idle_body`（`main.rs:1371`）在启动段之后进入循环：`main.rs:1532` 起，每轮固定顺序执行下列工作。所谓"轮询"是**每轮都无条件跑一遍的协作式推进**，不是抢占式任务。

| 顺序 | 名称 | 触发/条件 | 单轮工作量（代码事实） | 实测/预算 | 是否有界 |
|---|---|---|---|---|---|
| 0 | **终端输入** `TerminalService`（`capsules.rs:419`） | 每轮 | 最多消费 **64 B** RX 队列（`capsules.rs:389`），每字节 `rx_pop` 一次临界区；命令分发在 `shell.feed` 内 | `shell-input` 预算 1000 µs；实测命令可达 1,162 µs | 字节数有界；**单条命令本身无界**（由 `rt` 观测） |
| 1 | **文件系统写队列** `FsService`（`capsules.rs:464`） | 每轮 | `fs::step()` **一次性排空整个队列**（容量 8，`fs.rs:176/420-449`）；每个请求可能触发 Flash 擦/写（单块擦除约几十 ms 量级） | 计入 `idle-step`（预算 1000 µs） | **无界**（最多 8 个请求 × 单请求成本） |
| 2 | **文本日志落盘** `LogService`（`capsules.rs:540`） | 每轮 | `log_try_flush`：≥192 B 才触发；循环最多把日志环按 192 B 提交到 FS 队列（队列满即停） | 计入 `idle-step` | 近似有界（受 FS 队列 8 槽限制） |
| 3 | **自动日志轮转** `RotateService`（`capsules.rs:564`） | **每轮都检查**；运行 30 s 后才可能触发 | 先 `log_try_flush(true)`，再查活动文件大小；≥8 KiB 时**整文件 gzip + 写归档**（`main.rs:3416-3437`、`1014-1029`） | 计入 `idle-step` | **无界**（真机单步到 68 ms） |
| 4 | **健康聚合** `HealthService`（`capsules.rs:505`） | 每轮 | 一次原子置位/清位 | 微秒级 | 有界 |
| 5 | **后台维护作业** `MaintenanceService`（`capsules.rs:160`） | 有作业时 | 单步：GC 1 个死块 / compact 1 条记录 / rotate 1 个 2 KiB 分片 / checkpoint；自测 5 ms 预算，超时返回 `Deferred` | 计入 `idle-step` | **软有界**：5 ms 只用于"下次是否让出"，当前步本身已经做完；`overruns` 只写不读 |
| 6 | **1 Hz 杂项** | 每秒一次 | 心跳计数、10 s 后清故障台账、`rearm_if_stopped()`、按需打心跳 | 微秒级 | 有界 |
| 7 | `wfi` | 本轮 `did_work == false` | 睡到下一个中断 | — | — |

启动段（循环之前）另有：FS 挂载、SDRAM 自检（暖复位 1 MiB ≈143 ms；冷启动 32 MiB ≈3.3 s）、启动报告、`product` 装配与 `init_all`。它们不是轮询，且 `BootPost` 有单独预算。

### 4.1 公平性的真实含义

- **中断层面**：没有公平性问题（两个 IRQ 都是短、有界；没有软件任务，也没有优先级反转）。
- **idle 层面**：单循环是"先输入、再维护、再健康"的**固定优先级**，输入永远先跑 —— 这是刻意的，也是对的。
- 真正的问题是 **"一轮"没有总预算**：`IdleStep` 预算 1000 µs 只用于**观测**，没有任何代码在超预算时中断当前步骤、把剩余工作留到下一轮。于是：
  - 一次 68 ms 的 rotate 步骤会阻塞：下一次终端输入处理（最坏 68 ms 回显延迟）、Health 相位（最坏 68 ms 后才更新健康位）、以及 `wfi`；
  - 68 ms 远大于 RX 队列可缓冲的 22 ms ⇒ 若这期间持续输入，**会丢字节**（`rxdrop` 计数，心跳行可见）；
  - `bench` 在"无维护负载"的安静状态采样，`rt=ok` 只说明安静状态成立，**不能**说明维护期也成立。
- `rotate` 的顺序（fs → log → rotate → maintenance）本身没有明显饥饿：`fs::step` 排空后下一轮自然轮到 log/rotate；真正的风险是每一步太长。

## 5. 真机实测证据（2026-10-23）

### 5.1 安静状态

`beat on` 连跑：`rt=ok`、`txdrop=0`、`rxdrop=0`、`wfi≈1100~6000/s`，`isr-pit`/`isr-lpuart1` 无超预算记录。说明**空闲路径本身健康**。

### 5.2 一次 `log rotate`（无输入突发）

```
log rotate: 已提交后台维护（结果稍后打印）
log rotate: 后台归档完成，活动日志已清空
ERR  ⚠️ 有 3 项超预算：irq-off 实测 2886 µs > 80 µs（1 次）;
     shell-input 1162 µs > 1000 µs（1 次）;
     idle-step 67973 µs > 1000 µs（3 次）
```

### 5.3 定位 `irq-off`：只有第一次归档发生

连续两次 `log rotate`：

| 时刻 | irq-off 超预算次数 | idle-step 超预算次数 |
|---|---|---|
| 第一次归档后 | **1**（最大 2,886 µs） | 3（最大 67,973 µs） |
| 第二次归档后 | **仍是 1**（没有新增） | 4 |

第二次不再出现 `irq-off` 超预算 ⇒ 这 2.9 ms 与**一次性初始化**有关。代码上唯一候选是
`gzip_on_sdram_stack`（`main.rs:424-433`）：它在 `measured_cs` 闭包里执行
`alloc::boxed::Box::new(gzip::new_compressor())`，而 `new_compressor()` 的注释说明状态约 **320 KiB**（`crates/gzip/src/lib.rs:210`）—— 在关中断状态下初始化 320 KiB 内存，量级与 2.9 ms 吻合。
**这是推断，但实验（第二次不复发）与代码位置一致；确认方法见 §6-R2 的判据。**

### 5.4 维护期 + 同时输入（决定性证据）

把 `log rotate` 与 1,600 B 输入**放在同一次串口写里**（输入约 139 ms，必然与 68 ms 的
归档步骤重叠），真机结果：

```
log rotate: 已提交后台维护（结果稍后打印）
log rotate: 后台归档失败
command not found: yyyy…（输入只剩一部分）
beat=8 … txdrop=26502 rxdrop=511 … fsq=0/0/9/0 rt=VIOLATION
```

- **`rxdrop=511`**：1,600 B 输入里有 511 B 因为 RX 队列满被丢（丢最旧字节）——这是
  "维护长步 → 输入丢失"的直接证据，不是理论风险；
- `txdrop=26,502`：终端回显量（约 1,600 字 × 每字提示符重绘）在某个关中断窗口里撞上 TX
  队列满被丢弃；**具体关中断窗口尚未定位**（候选：归档路径里的 `measured_cs`），列为待查；
- 同一窗口 `fsq` 有 9 个失败请求、`rt=VIOLATION`。

另一轮（700 B 输入晚于归档结束）`rxdrop=0`，说明**是否丢输入取决于输入与长步的时序重叠**，
而不是"维护期一定丢"——这正好说明需要一个可重复的压测判据（见 §7）。

### 5.5 归档失败本身

多次实验里 `log rotate` 以"后台归档失败"结束，`fsq` 出现 8~9 个失败请求；同时也会出现
"后台归档完成"（第二次实验）。归档失败与输入丢字节是**两个独立问题**，本文只记录现象，
不把归档失败归因于调度；它需要单独排查（`rotate_start`/`rotate_step` 的 FS 写入路径）。

## 6. 优化建议（按价值/风险排序）

### R1（低风险，防漂移）把优先级接线变成可失败的判据

- **当前不改优先级**：`console_rx` 与 `deadline_tick` 同为 1 是合理配置（见 §3.1）。
- 在 `main.rs` 的任务声明旁加编译期断言/注释锚点，例如把各任务的预算常量与
  `budget::prio` 一起断言；将来 P2 的串行总线/CAN 任务落地时，**先加断言再接线**。
- 复核 `budget.rs` 的串行字节余量论证：它把 `CAN_RX/CAN_TX` 当作"高于串行接收"的任务，
  这个前提只有在 P2 接线后才成立；在那之前该断言是"为未来准备"，不是当前配置的证明。

**判据**：`verify` 全绿；文档明确区分 `console_rx`（LPUART1，已落地）与 `serial_rx`
（串行总线端口，未落地）。**风险**：几乎为零（不改运行行为）。

### R2（高价值，改动小）把 gzip 状态初始化移出关中断窗口

- 把 `GZIP_STATE` 的 `Box::new(gzip::new_compressor())` 初始化从 `measured_cs` 闭包里拿出来：
  先在关中断外构造（或 `Option::get_or_insert_with` 在锁外做重活、锁内只发布指针）；
- 判据：重新做 §5.3 的"两次归档"实验，`irq-off` 超预算次数与最大值都应为 **0**；`verify` 全绿。

### R3（高价值，中等改动）给 idle 单步设上限，把长维护分片切细

- `fs::step()` 改为"每步最多处理 N 个请求 / 不超过 T µs"，剩余留到下一轮（返回值区分 `Drained` 与 `Partial`）；
- 自动轮转从"整文件 gzip"改为走 `rotate_start`/`rotate_step` 的**分片路径**（2 KiB 一段），并把单段成本压到 ≤1 ms（必要时 512 B/段）；
- 判据：`log rotate`、`fs` 写队列打满、`log rotate` + 粘贴流三种场景下，`rt` 的 `idle-step` **不再超 1000 µs**（或至少 ≤ 2×），`rxdrop=0`、`txdrop=0`；主机侧给 `fs::step` 的"部分完成"补单测。

### R4（低成本）自动轮转的检查频率

`RotateService::step` 每轮都 `log_try_flush(true)` + 查文件大小（30 s 之后）。可以改为
"距上次检查 ≥1 s 才检查"，避免每轮一次 FS 元数据查询；同时把自动轮转改为投递维护作业，
与手动 `log rotate` 共用同一条分片路径。

### R5（可观测性补强）

- `MaintenanceService::overruns` 目前只写不读（`capsules.rs:146/182`）：把它并入心跳行
  （如 `cap_overrun=`），否则"5 ms 预算"没有对外证据；
- `rt` 命令在目标端裁剪了完整表，只报超预算项；建议心跳里带"本轮 `idle-step` 最大值"
  的轻量摘要，或保留 `rt` 的分项查询。

### R6（暂不做）中断内 TX 一字节一次

满速输出时 `LPUART1` 每秒进约 11.5 k 次中断（一次一字节，`console.rs:359`）。
当前 `isr-lpuart1` 没有超预算记录，CPU 占用未测到成为问题；若将来日志吞吐成为瓶颈，
再考虑 TX FIFO/ DMA（会新增复杂度与 DMI 通路，暂不引入）。

### 不建议做的事

- 不要把 idle 的维护工作搬进 ISR（会直接推高 `irq-off`/ISR 预算，且违反现有纪律）；
- 不要靠"提高所有优先级"解决公平性（问题在单步长度，不在抢占关系）；
- 不要删掉 1 ms 的 `idle-step` 预算——它是**发现**这类问题的唯一机器判据。

## 7. 未确认/待补

1. §5.3 的 gzip 初始化定位是"代码位置 + 第二次不复发"的推断；R2 的判据就是确认实验。
2. §5.4 的 `txdrop=26,502` 的**关中断窗口**尚未定位（是 echo 撞上哪个 `measured_cs`）；
   需要可重复脚本 + 在归档路径加 span 才能确认。
3. §5.5 的归档失败（`fsq` 8~9 个失败请求）需要单独排查；它与调度公平性是独立问题。
4. 可重复压测建议：固定"`log rotate` + 1,600 B 输入同窗"，判据为 `rxdrop=0`
   （R3 完成后）；当前基线记录为 `rxdrop=511`。
