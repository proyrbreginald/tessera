# AGENTS.md —— 接手前必须读完的一页

> 面向**接手的 AI Agent 与新人**。这里只写"怎么不犯已知的错"。
> 依据与细节在 [`docs/00-索引/项目地图.md`](docs/00-索引/项目地图.md) 与 `docs/`。
> 当前阶段：**阶段 1（单槽，映像从 Flash 偏移 0 起）**；run/backup 分区已预留，Bootloader 属 P4。
> 唯一验收口径是 `cargo xtask verify`。

## 1. 这是什么

| 项 | 事实 |
|---|---|
| 项目 | **Tessera**：MCU 实时**底座**，core kernel + 基础 capsule，提供启动链、外设驱动、存储、日志、实时仪表、故障解码、终端与门禁 |
| 没有的东西 | 业务 capsule。接入方在**仓库之外**建立自己的业务 crate，通过 capsule SDK/端口契约接入，用 `simulator` 的主机替身测试（见 [`业务接入指南`](docs/02-架构/业务接入指南.md)） |
| 架构方向 | **静态 capsule 化内核**：core kernel 独占硬件/中断/调度；基础 capsule 提供平台服务；业务 capsule 是仓库外 crate；编译期静态组合，不做运行期热替换（[`微内核与capsule架构`](docs/02-架构/微内核与capsule架构.md)、[ADR-0008](docs/adr/0008-静态capsule化内核.md)） |
| 信任边界 | `forbid(unsafe_code)` 是代码纪律，**不是**安全隔离；capsule 仍共享内核地址空间。不可信第三方代码需要 userspace/MPU 进程模型，本架构不做 |
| 语言 | 100% Rust：`no_std` + RTIC v2 + `imxrt-hal/ral/rt/boot-gen`；零 `async`；`alloc` **受限**（SDRAM 固定块静态池，只服务终端/诊断与压缩器状态，见 [ADR-0003](docs/adr/0003-不使用alloc与async.md)） |
| 工作区 | **20 个成员 crate**（其中 **18 个纯逻辑**，可主机测试）+ `xtask`（非成员） |
| MCU | NXP `MIMXRT1021CAG4A`，Cortex-M7 @396 MHz，144-LQFP，**无内部程序 Flash** |
| 外部 Flash | Winbond `W25Q256JVEQ` 32 MiB QSPI NOR（FlexSPI1 → XIP `0x6000_0000`）：程序与数据的唯一非易失落点 |
| 外部 SDRAM | `W9825G6KH-6I` 32 MiB @ `0x8000_0000`，SEMC CS0 @62.5 MHz；全片自检通过 |
| 控制台 | LPUART1 115200 8N1（`GPIO_AD_B0_06/07`）；节点 `/dev/pudu-debugger-16p-uart`，SWD `/dev/pudu-debugger-swd` |
| CAN | `CAN2` + `TJA1044T`（`U13`）：探针、写序列与**内部 loopback** 已 PASS（2026-10-19）；**外部总线接收未通**（`GAP(P2)`） |
| 实时仪表 | DWT 周期计数器 + `realtime` 的 span 统计；心跳（`beat on` 开启；**默认不打印**）带 `rt=ok\|VIOLATION\|none` |

**会变的测量值不写在这里**（写了必然漂移）：用 `cargo xtask context` 一次拿到当前的主机测试数、
覆盖率、映像字节数、缺口清单与基准基线；`cargo xtask gates` 给门禁现状。

**capsule 化的三个当前事实**（防止把方向当现状）：

- **组合根已外提**：产品组合策略在纯逻辑 crate `product`（只依赖 `sdk`），`firmware` 通过 `src/platform.rs` 实现 `product::Platform` 注入六个内置服务；旧的 `src/composition.rs` 已删除。`sdk` 是稳定门面（`Capsule<C>` / `CapsuleSet` / `Product`）；
- 插件门禁 `check-capsules` 已存在，但它**只扫仓库内 `crates/`**：检查依赖了 `sdk` 的 crate 不直接依赖 `board-support`/`firmware`/`rtic`/`imxrt-*`；
- **尚未实现**：S2 上下文能力对象化（`sdk::CapsuleContext` / 能力句柄）、S3 基础 capsule 外提与仓库外插件门禁。因此仓库外业务 capsule 目前只能写无能力的 `impl<C> Capsule<C>`；capsule 化也不改变 P4 的 A/B 整包更新缺口（没有 Bootloader 时，现场更新仍靠调试器烧录）。

## 1.5 接手第一小时（照这个顺序做）

1. `cargo xtask verify` —— 建立基线。**必须全绿**再动任何代码；红了先修到绿。
2. `cargo xtask context` —— 一次拿到：crate 划分、门禁清单、缺口（带 `文件:行`）、分区表、
   预算、基准基线、最近一次测试结果。**别去 grep 源码拼项目状态**。
3. `cargo xtask gates` —— 17 项静态门禁的现状（不因第一个失败而中断）。
4. 读 [`docs/00-索引/项目地图.md`](docs/00-索引/项目地图.md)：启动链、存储地图、
   **怎么验证一件事**、缺口与里程碑。
5. 要动某一层之前，先读该 crate 的 `lib.rs` 模块文档（每个都有，且写了「为什么」）。

**两条反直觉的规矩**（最容易违反）：

- **会变的测量值不写进文档**（测试数、覆盖率、映像字节数）—— 写了必然漂移，用
  `cargo xtask context` 取；常量类数字（crate 数、unsafe 预算、缺口数）由
  `cargo xtask check-docs` 强制与代码一致。
- **未实现不是错误，但假装完成是**：留 `GAP(Px)` + 返回明确错误变体；`check-gaps` 会把它们
  连 `文件:行` 一起列出来。

## 2. 铁律（违反即视为交付失败）

| # | 铁律 | 具体做法 |
|---|---|---|
| 1 | **全中文** | 注释、文档、提交信息一律中文；例外保持原文：**标识符**（ASCII）、日志/串口输出、命令、路径、错误信息。**测试函数名允许中文**（`cargo test` 输出给人看；这是唯一例外，见 [`代码风格`](docs/04-开发/代码风格.md)） |
| 2 | **不猜** | 任何硬件事实先查①板级参考实现 `/home/reginald/winshare/test/master-control-board-a-06`（在这块板上跑过）②`docs/`，并给出证据（文件+行 / 手册页码 / 实测记录）。`🟡` 标记项**不得**当事实使用 |
| 3 | **报事实，不报意图** | 寄存器与状态必须**回读并打印**（横幅里的 `pit regs :`、`region2:`、`flash 4B :`、`stack :`、`dwt :`、`device id:`）。禁止只打印"期望值"——MPU `RBAR` 那次 bug 就是靠回读抓到的 |
| 4 | **改动前后各跑一次门禁** | 改前 `cargo xtask verify` 建立基线；改后必须再跑且**全绿**。单项排查用 `cargo xtask <子命令>`（`verify` 在 fmt 失败时会中断后续步骤） |
| 5 | **unsafe 有预算** | 新增 `unsafe {}` 必须紧邻 `// SAFETY:`；`unsafe fn` 必须有 `# Safety` 段；同时更新根目录 [`unsafe.budget`](unsafe.budget) 并写明"为什么无法避免"。预算只减不增（当前 **81/81**；每次增加都必须在 `unsafe.budget` 与提交信息里写明理由） |
| 6 | **未实现要留痕** | 未实现的能力：留 `GAP(Px)` 标记 + 返回明确错误变体（`NotImplemented` / `Rejected`）。**禁止** `todo!()`/`unimplemented!()`，**禁止**假装成功 |
| 7 | **临时产物只放 `tmp/`** | 抓包、日志、脚本输出、导出物一律进 `tmp/`（已在 `.gitignore` 里），不要污染仓库根目录 |
| 8 | **改固件行为就递增版本** | 根目录 [`VERSION`](VERSION) 是版本号唯一真源（由 `crates/firmware/build.rs` 在构建期注入并校验格式）。只改文档/工具不必递增 |

## 3. 命令速查

| 命令 | 作用 |
|---|---|
| `cargo xtask verify`（= `make verify` = `bash scripts/verify.sh`） | **唯一全量入口**：静态门禁 + fmt/clippy + 主机测试 + 目标构建 + 产物断言 |
| `make` / `make help` | 列出全部目标 |
| `cargo xtask check-layering` | 18 个纯逻辑 crate 的依赖闭包（含 `dev-dependencies`）里没有 `imxrt-*`/`cortex-m*`/`rtic` |
| `cargo xtask check-capsules` | 插件门禁：`sdk` 保持纯逻辑；依赖了 `sdk` 的插件 crate **不得**直接依赖 `board-support`/`firmware`/`rtic`/`imxrt-*`。**只扫仓库内 `crates/`**（仓库外业务待 S3） |
| `cargo xtask check-lint-headers` | 每个纯逻辑 crate 的 `lib.rs` 带 `forbid(unsafe_code)` + 两条 clippy 拒绝项 |
| `cargo xtask check-unsafe` | unsafe 块与 `// SAFETY:` 配平、`unsafe fn` 与 `# Safety` 配平，且不超预算 |
| `cargo xtask check-fault-handlers` | 从**链接产物**断言四个故障向量（槽 3–6）各自指向真实实现或**刻意升级**：审计曾读到槽 3 是个 `b .` 空死循环、槽 4/5/6 共用默认桩 —— 那种缺陷编译得过、测试也全绿，只有看产物才发现 |
| `cargo xtask check-critical` | **关中断只能走测量入口**（`board_support::rt::measured_cs`）：一个没被测量的临界区就是没人知道有多长的关中断窗口。2026-10-12 终端 `fs` 第一版在关中断里做全盘扫描（真机折算 0.5–2 s），当时构建/测试/门禁全都照样通过 —— 这条把它变成机器判据 |
| `cargo xtask check-api-usage` | 列出没有任何调用者的公开项（刻意不失败，但必须可见） |
| `cargo xtask check-gaps` | 清点全部 `GAP(Px)`（**刻意不失败**：未实现是事实，要可见）。当前 3 项：P2/P4/P5 |
| `cargo xtask check-docs` | 文档门禁（三类）：① **常量类数字**与代码一致（成员/纯逻辑 crate 数、静态门禁条数、unsafe 预算、缺口数）；② **文档可达性** —— 每篇 `docs/**/*.md` 都必须被某处链接（孤儿文档等于不存在）；③ **命令存在性** —— 文档里出现的每一条 xtask 子命令与每一个 make 目标都必须真实存在（照抄文档却报「命令不存在」是最伤信任的错误） |
| `cargo xtask check-memory` | 内存放置：从**链接产物**断言 `.text` 在 ITCM 且不超预算、`.stack` 大小等于声明值、`.vector_table` 在 DTCM（判据常量来自 [`memory-layout`](crates/memory-layout/src/lib.rs)） |
| `cargo xtask check-code-placement` | 代码放置：`.xip` 在 Flash 内，且擦写期间必须能跑的模块不在 `.xip` |
| `cargo xtask gates` | 全部静态门禁的汇总表（**不因第一个失败而中断**，每行 `名称 PASS/FAIL`；退出码仍反映是否全过）。每条的判据、真实事件与失败处置见 [`门禁手册`](docs/04-开发/门禁手册.md) |
| `cargo xtask check-style` | 风格门禁：无 `todo!()`/`unimplemented!()`、每 crate 有模块文档、lint 未放宽（[`代码风格`](docs/04-开发/代码风格.md)） |
| `cargo xtask check-image` | 对链接后的 ELF 断言 FCB tag/版本/容量、LUT 三个序列的 4 字节地址相位（读 `0xEC`/擦 `0x21`/写 `0x34`，相位 `0x20`）、IVT 魔数、初始 SP 落在 DTCM |
| `cargo xtask check-size` | Flash 映像占用 < 1 MiB（保证单槽升级放得下） |
| `cargo xtask check-flash-map` | 分区表自洽 + 映像放得下 + **文档表格与代码分区表一致** |
| `cargo xtask check-compile-fail` | 编译失败负例确实失败（`Cycles` 非零：`record(span, 0)` 必须编译不过） |
| `cargo xtask trace` | 生成 [`docs/01-需求/需求追溯矩阵.md`](docs/01-需求/需求追溯矩阵.md)（**勿手改**）并检查 30 条平台需求全覆盖 |
| `cargo xtask test-host` / `lint-host` / `lint-target` | 主机测试 / 主机 lint / 固件目标 lint。**crate 列表由 `xtask` 的 `PURE_CRATES` 唯一生成**（`make test`、CI 都委托它们，不再各写一份） |
| `cargo xtask pure-crates` | 打印纯逻辑 crate 列表（一行一个，供脚本/Agent 取用） |
| `cargo xtask context` | **机器可读**的项目上下文（JSON：crate 划分、门禁清单、缺口、分区表、预算、基准基线） |
| `cargo xtask bench` / `bench --record` | 真机基准与基线回归（**百分比与绝对量同时超限**才判回归）；另含**硬下限**判据（与基线无关，低于即失败：栈余量 ≥ [`memory_layout::MIN_STACK_HEADROOM_PERCENT`](crates/memory-layout/src/lib.rs)、**实时预算成立** `rt=ok`、**控制台零丢弃** `txdrop=0`）；`--record` 刷新 [`bench/baseline.json`](bench/baseline.json)，`--from-file <抓包>` 可**离线取数**（不需要台架，用于验证解析逻辑本身） |
| `cargo xtask coverage` | 主机侧覆盖率（`-C instrument-coverage` + 工具链自带 `llvm-cov`，**不新增依赖**）。**行覆盖率阈值 90%**（低于即失败）；当前值看命令输出或 `cargo xtask context` 的 `measured.coverage` |
| `make build` / `cargo build --release` | 默认目标**就是**固件目标 `thumbv7em-none-eabihf`；主机测试必须显式 `--target x86_64-unknown-linux-gnu` |
| `make image` / `make size` / `make objcopy` | 产物断言 / 体积 / 生成 `.bin`/`.hex` |

### 真机（烧录与抓包）

```bash
make setup                        # 建 .venv 并装 pyocd/pyserial（无需 root）
make check-bench                  # 台架门禁：探针、SWD 节点、pyocd。串口别名缺失只警告
bash scripts/flash.sh -e chip     # 改了分区布局后**必须**全片擦除（见 §5）
make monitor                      # 抓串口并校验时基漂移（自动发 `beat on`；判据：设备 uptime vs 主机时间，±2%）
make reset                        # .venv/bin/pyocd reset --target mimxrt1020
make bytecheck                    # 控制台**逐字节**校验（改动控制台/时钟/中断后必跑；判据是剥离 ANSI 后逐条比对期望文本）
make soak                        # 长稳采集（默认 30 min；SOAK_MINUTES=60/1440 可调；失败条件见脚本头）
make test-target                 # 目标端自检：复位 → selftest → 逐条 test ... 解析（需要板子）
```

- `pyocd` 解析顺序（[`scripts/tool-paths.sh`](scripts/tool-paths.sh)）：`$PYOCD` → `./.venv/bin/pyocd` → 相邻项目的 `.venv/bin/pyocd` → `PATH`。
- `make flash` **不会**带 `-e chip`（`scripts/flash.sh` 只把多余参数透传给 pyocd），要全片擦除得直接调脚本。
- 长期观测量用 `scripts/read_console.py`；不要写死 `ttyUSBn`，用 udev 别名。
- **启动横幅的图形**来自仓库根 `banner.txt`（当前字体 **ANSI Shadow**，figlet）：`firmware/build.rs` 构建期注入（支持 `{version}` 占位符），
  改文件即改横幅；`make bytecheck` 会自动从该文件取期望行（不必改脚本）。

## 4. 代码地图

| crate | 一句话 | 关键文件 |
|---|---|---|
| [`primitives`](crates/primitives/src/lib.rs) | 平台原语：时间、实时预算、健康门控、格式化、随机数、校验 | `time.rs` `budget.rs` `safety.rs` `format.rs` `prng.rs` `crc.rs` |
| [`wire-format`](crates/wire-format/src/lib.rs) | 线路编解码：Modbus-RTU 帧与 CRC（**不解释设备语义**，功能码是不透明 newtype） | `modbus_rtu.rs` `frame.rs` `error.rs` |
| [`ports`](crates/ports/src/lib.rs) | 端口契约 `Bus`/`Clock`/`Store`/`LogSink`/`RawMedium` + 半双工类型状态机 + 双份存储策略 | `bus.rs` `store.rs` `double_bank.rs` `raw.rs` |
| [`simulator`](crates/simulator/src/lib.rs) | 主机替身：脚本化总线、手动时钟、可注入掉电的介质、捕获日志 | `bus.rs` `clock.rs` `store.rs` `log.rs` |
| [`sdram-timing`](crates/sdram-timing/src/lib.rs) | SDRAM 规格、SEMC 时序换算、内存测试算法（纯算术） | `sdram.rs` `memtest.rs` |
| [`shell`](crates/shell/src/lib.rs) | 串口终端：行编辑、ANSI、命令分发（纯逻辑，命令表是唯一来源） | `editor.rs` `ansi.rs` `commands.rs` `shell.rs` |
| [`flash-layout`](crates/flash-layout/src/lib.rs) | Flash 用途划分：分区表是 BSP / 门禁 / 文档的**共同事实来源** | `lib.rs`（`MAP`/`XIP_WINDOW`） |
| [`filesystem`](crates/filesystem/src/lib.rs) | 日志结构文件系统：记录级 CRC、版本完整性、GC、磨损分散、写队列 | `fs.rs` `layout.rs` `index.rs` `write_queue.rs` |
| [`mpu-regions`](crates/mpu-regions/src/lib.rs) | MPU 区域表与 `RBAR`/`RASR` 编解码（把 RBAR 区域号编码 bug 变成单测） | `lib.rs` |
| [`memory-layout`](crates/memory-layout/src/lib.rs) | 片内内存布局事实 + 栈涂抹测量 + 预算（栈大小、ITCM 预算、池常量） | `lib.rs` |
| [`realtime`](crates/realtime/src/lib.rs) | 实时性仪表：span 注册表、预算判据、关中断最外层语义（纯逻辑） | `lib.rs` |
| [`fault-decode`](crates/fault-decode/src/lib.rs) | 复位原因与故障现场解码：`SRC_SRSR`/`CFSR`/`HFSR`/入栈帧/跨暖复位记录 | `lib.rs` |
| [`block-pool`](crates/block-pool/src/lib.rs) | 固定块静态池的分配策略（无碎片、失败是一个值） | `lib.rs` |
| [`log`](crates/log/src/lib.rs) | 文本日志环 + gzip generation 归档索引（纯逻辑） | `text_log.rs` `archive.rs` `level.rs` |
| [`gzip`](crates/gzip/src/lib.rs) | gzip 容器 + DEFLATE（纯 Rust、`no_std`；压缩侧靠大块静态池） | `lib.rs` |
| [`nor-flash`](crates/nor-flash/src/lib.rs) | QSPI NOR 的纪律：几何、对齐/范围校验、命令相位、自证、FlexSPI 寄存器常量 | `geometry.rs` `ops.rs` `range.rs` `flexspi.rs` `selftest.rs` |
| [`sdk`](crates/sdk/src/lib.rs) | capsule SDK 稳定门面：`Capsule<C>`（name/phase/init/step）、固定容量 `CapsuleSet`、`Product` 模板与 `prelude`；事件分发 `on_event` **未实现** | `lib.rs` |
| [`product`](crates/product/src/lib.rs) | 默认产品组合根：定义 `Platform` 注入契约、六个具名服务槽与 `build`；**只依赖 `sdk`** | `lib.rs` |
| [`board-support`](crates/board-support/src/lib.rs) | **唯一**允许接触寄存器的 crate：时钟/控制台/PIT/FCB/XIP/SEMC/MPU/OCOTP/CAN/池 | `board.rs` `flash.rs` `flexcan.rs` `flexspi_ip.rs` `mpu.rs` `console.rs` `stacks.rs` `config.rs` |
| [`firmware`](crates/firmware/src/main.rs) | RTIC 应用与平台注入：只做接线、任务划分、健康位原子聚合；产品组合策略在 `product` | `main.rs` `platform.rs` `capsules.rs` `device.rs` `version.rs` |
| [`xtask`](xtask/src/main.rs) | 构建与产物门禁（主机专用，**独立工作区**，不是成员） | `main.rs`（`PURE_CRATES` 在 L27） |

> capsule 视角下的代码地图：`board-support`/`firmware` = core kernel；纯逻辑层 = 基础 capsule
> 与 kernel-support；业务 crate 不在仓库内。产品组合根已外提为 `product`（S1，见
> [ADR-0010](docs/adr/0010-产品组合根外提.md)）；S2 上下文能力对象化与 S3 基础 capsule
> 外提**尚未实现**（见 [ADR-0008](docs/adr/0008-静态capsule化内核.md) 与
> [`产品组合根外提设计`](docs/02-架构/产品组合根外提设计.md)）。

## 5. 常见坑（都踩过，别再踩）

| 坑 | 事实 |
|---|---|
| 运行期写启动头 | `boot_header`（偏移 0..8 KiB）是 `ReadOnly`。写坏它 = Boot ROM 不启动 = 只能靠 BOOT_MODE 引脚 + 串口下载器救。**绝不允许** |
| 改分区布局**或 FS 磁盘格式**不擦全片 | 旧数据会留在新分区里，FS 挂载要逐字节重同步（实测 21 秒，看起来像死机）。必须 `bash scripts/flash.sh -e chip`（即全片擦除）。**FS 块头布局变更也算**（例如 2026-10-12 把擦除计数 u16→u32、偏移 12→10）：真机残留旧格式会被判成损坏。注意「现在没有真机数据」不等于「以后改也免费」—— Flash 擦写落地后就变成格式迁移了 |
| LUT 退回 3 字节地址 | 越过 16 MiB 的访问**不报错**，会静默别名到低 16 MiB（"擦日志区"擦掉启动头）。`check-image` 会在构建期拦住 |
| MPU `RBAR` 编码 | **区域号在 `bits[3:0]`、`bit[4]` 是 VALID**。写成 `number << 4` 会让 8 次写全落进 region0，且从 index≥2 起连基址都被改掉；当年只能靠横幅回读发现。现在提取出 [`mpu-regions`](crates/mpu-regions/src/lib.rs) 把编码做成被测函数——**这类错误应当在 `cargo test` 里失败，而不是在目标现场失败** |
| **装了 DWT 不等于它一直在走** | 2026-10-12 真机实测：`install()` 后立刻回读 `CTRL=0x40000001 / DEMCR=0x01000000`（**成功**），但启动跑完再读就是 **0**。**分段打点已把范围锁死到 `sdram_timing::memtest::pattern_test` 那一次 32 MiB 写遍+读遍期间** —— 而那个函数**只做内存读写、不碰任何寄存器**，所以这是一个**尚未解释的硬件层反常**，不要编故事。处置（三层，缺一不可）：①`init` 末尾重新使能一次；②**每次心跳检查并按需自动重装**（`board_support::rt::rearm_if_stopped()`），重装次数以 `rearm=` 上报 —— 于是"运行期到底会不会再被清"变成一个**实测数字**；③心跳 `dwt=ok\|DEAD` 直接说明"上面那张 rt 表的数字还可信吗"，且 `rt=none`（一次都没测过）被 `bench` 判为**失败**而非合格。⚠️ 排查时**不能信调试器读到的值**：pyOCD attach 会自己打开 `TRCENA/CYCCNTENA`，所以「调试器读到 1」完全可能是它开的，而不是固件开的 |
| **关中断里输出超过队列容量 = 静默丢日志（旧实现会挂死）** | 控制台 TX 队列按实测定容到 **7680 B**。队满时按 `PRIMASK` 分流 —— 中断开着才自旋（ISR 会排空），**关中断时丢弃并计数**；丢弃数每秒随心跳行以 `txdrop=` 出现（静默丢弃是不允许的），`bench` 把 `txdrop≠0` 当硬性失败。⚠️ 这也是「`init` 里不该做长活」的理由 |
| 发送路径**只能有一个 `DATA` 写入者** | 真机定位（决定性证据，不是推理）：曾出现"队列满且中断关着"时退回**按字节直写** `DATA` —— 那一刻它与中断里的排空**同时写同一个寄存器**，症状是**整行整行地丢**。修法：队列化路径**只入队、绝不直写**；关中断上下文要输出就走 [`write_bytes_blocking`](crates/board-support/src/console.rs) |
| 栈大小靠"估一个够大的" | 被真机打穿三次（8/16/32 KiB），症状都是"串口一行都不出"。现在 48 KiB + 启动时栈涂抹测量。改栈大小前先有测量 |
| FlexSPI 逐字节 `read_volatile` | 挂载扫描 27.88 MiB 慢到几十秒，表现也像"死机"。必须批量拷贝 |
| 改 FlexSPI 时钟不重算 dummy | `0x06`（6 dummy）只对 100 MHz 正确。只改时钟 = 读到垃圾 = **启动即崩溃**。两者要么一起改，要么都别动 |
| 只改时钟/波特率不当回事 | 控制台取 **24 MHz 晶振**而不是 PLL，就是"不依赖 PLL 锁定"的刻意选择；串口出字只证明波特率与引脚对，不证明时钟树对——`make monitor` 看 `uptime` 漂移才是判据 |
| 用 pyocd 的返回状态判断板子是否在跑 | 这块板上 `reset`/`reset_and_halt` 都返回"成功"。要看**可验证的副作用**：`SRC_SRSR`、写入是否落地、flash 算法能否初始化 |
| 调试器读数 vs 固件自报冲突 | **以固件自报为准**（同时插两个探针时读到过 Cortex-M4 的 CPUID `0x410FC241`；RT1021 应为 `0x411FC271`）。调试前先核对 CPUID |
| 起不来就怀疑 FCB | 先跑 `make image`（产物断言）+ `cargo xtask check-image`。`flash init timed out` 指的是**核心没在执行指令**（复位/供电问题），不是 Flash 问题 |
| 终端就绪后突然开始刷屏 | 默认**不会**：进入终端后固件只等待输入，不主动输出（心跳默认关闭）。需要长期观测量时显式 `beat on`（`make monitor` / `cargo xtask bench` 会自动发）；`beat` 打印一次快照，`beat off` 恢复静默。 |
| 旧文档里的"现场固件" | 指**本板既有 C 固件参考实现**（`/home/reginald/winshare/test/master-control-board-a-06`，非猜测来源）。引用它时给出文件:行 |

## 6. 不要做的事（已被否决的方案）

| 不要 | 为什么（提出处） |
|---|---|
| 换掉现有文件系统（littlefs / SPIFFS 等） | 现有实现零分配、无外部依赖、掉电点穷举测试到位；换外部 crate 会引入离线不可得的依赖并丢掉测试资产（[`资源利用与优化评审`](docs/02-架构/资源利用与优化评审.md) §4） |
| 在没有真实负载前改 SDRAM 区域属性 / FlexSPI 时钟 | 那是"猜"；SDRAM 流式访问无缓存收益，要改先拿真实负载测 |
| 用 `EN4B(0xB7)` 全局 4 字节模式切换 | 会把器件留在非上电默认状态，序列没生效就变砖。用专用 4 字节操作码（读 `0xEC`） |
| 搬运式升级（先擦运行槽再拷入） | 中途掉电就没有可启动映像。普通升级只写备份槽；Bootloader 的复制恢复必须可重试、不得破坏备份 |
| 用 `defmt` / **通用堆** / `async`（Embassy） | 现场故障恰恰没有调试器；单执行器 async 无抢占，硬实时不可论证。`alloc` 已按 [ADR-0003 修订](docs/adr/0003-不使用alloc与async.md) **受限**引入（固定块静态池、失败返回空指针、只服务终端/诊断与压缩器）；**通用堆（变长块 + 合并）仍被否决** —— 它会把「无碎片、内存静态可知」这两条论证一并推翻 |
| 把 capsule 化理解成运行期热替换 / ELF 动态加载 | 本架构是**编译期静态组合**：零加载开销、接口在编译期校验；动态加载要重定位/符号解析/堆，与固定内存和 A/B 整包模型冲突（[ADR-0008](docs/adr/0008-静态capsule化内核.md)） |
| 把 safe capsule 当成“不可信隔离” | safe Rust 只解决内存安全，不防 panic/死循环/资源耗尽/数据泄露；可信插件模型与安全隔离是两件事 |
| 手写需求追溯矩阵 / 只写正例 | 手写必然漂移且给出虚假安全感；负例才是类型约束的证据（[`测试策略`](docs/04-开发/测试策略.md)） |
| 在纯逻辑 crate 里碰寄存器或加 `unsafe` | 分层是编译期事实。只有 `board-support` 与 `firmware` 例外 |
| 为了"看起来完成"而填外部接口的默认值 | 未知必须隔离成没有默认值的类型；漏填的后果是错误扩散到测试与文档（[ADR-0007](docs/adr/0007-未知协议做成类型.md)） |

## 7. 还不知道的事（别当成已解决）

`cargo xtask check-gaps` 当前报告 **3 项**（全部是平台能力，与业务无关）：

| 里程碑 | 条目 | 位置 | 阻塞点 |
|---|---|---|---|
| **P2** | CAN **外部总线接收**未在真机验证通过（内部 loopback 已于 2026-10-19 实测 PASS；未接入 `Bus` 端口契约） | `crates/board-support/src/flexcan.rs` | 需要第二块板/示波器验证总线电气，**不靠改序列去猜** |
| **P4** | run/backup 引导与复制恢复未实现（分区已预留、门禁已就位） | `crates/flash-layout/src/lib.rs` | 复制进度、校验、回退逻辑待实现 |
| **P5** | 看门狗未启用（外部 SGM706，`WDG_CTR` 驱动源未确认；贸然生效只能断电恢复） | `crates/board-support/src/board.rs` | 需硬件侧确认 `WDG_CTR` 由谁驱动（Q-I） |

边界提醒：**消费方业务不在这里**。底座只提供端口、替身与门禁；业务 crate 建在仓库之外，
用 `simulator` 在主机上完整重放（见 [`业务接入指南`](docs/02-架构/业务接入指南.md)）。
