# Tessera —— 静态 capsule 化 MCU 实时内核

**Tessera** 是运行在 `PJ0007_TF2` 主控板（NXP **MIMXRT1021CAG4A**，
i.MX RT1021 / Cortex-M7 @396 MHz / 144-LQFP）上的**静态 capsule 化实时内核**：启动链、外设驱动、存储、日志、实时仪表、故障解码、终端与门禁，
**完全用 Rust 与其生态**实现，运行时用 **RTIC v2**，芯片支持用 NXP 官方的
`imxrt-hal` / `imxrt-ral` / `imxrt-rt` / `imxrt-boot-gen`。

> **当前版本：`0.1.1.0`**。工具链**永远跟随最新 stable Rust**，
> 不锁定具体 rustc 版本；可复现性由入库的 `Cargo.lock` 与 `xtask/Cargo.lock` 保证。

> 底座**不含业务逻辑**：接入方在仓库之外建立自己的业务 capsule crate，通过 capsule SDK/
> `ports` 的端口契约接入，用 `simulator` 的主机替身脱离目标机测试，用同一套门禁守护质量。
> 见 [业务接入指南](docs/02-架构/业务接入指南.md)。
>
> **架构方向：静态 capsule 化内核**。core kernel 独占硬件/中断/调度，基础 capsule 提供
> 平台服务，业务 capsule 是仓库外 crate；全部编译期静态组合，不做运行期热替换。
> 安全边界是“可信插件”：safe Rust 不等于安全隔离。见
> [微内核与 capsule 架构](docs/02-架构/微内核与capsule架构.md) 与
> [ADR-0008](docs/adr/0008-静态capsule化内核.md)。

## 先看这个

```bash
make            # 列出全部目标
make verify     # 全量离线验证：门禁 + lint + 主机测试 + 目标构建 + 产物断言
make gaps       # 列出当前所有"未实现项"—— 这就是交付的诚实边界
```

首次接手硬件相关工作时再执行 `make setup && make check-bench`。

## 分层：core kernel + 基础 capsule，约束由 crate 边界保证

工作区共 **20 个成员 crate**（另 `xtask` 是独立工作区、非成员）；其中 **18 个是纯逻辑
crate**，`cargo xtask check-layering` 会逐个检查它们的依赖闭包。

```text
基础 capsule / kernel-support（18 个纯逻辑 crate：无 imxrt-*/cortex-m/rtic 依赖，可在主机上 cargo test）
primitives      平台原语：时间、实时预算、健康门控、格式化、随机数、校验
wire-format     线路编解码：Modbus-RTU 帧、CRC-16（不解释设备语义）
ports     端口契约：Bus / Clock / Store / LogSink / RawMedium + 半双工类型状态机
simulator       主机替身：脚本化总线、手动时钟、可注入掉电的介质、捕获日志
sdram-timing      SDRAM 规格、SEMC 时序换算、内存测试算法（纯算术）
shell     仿 Fedora 串口终端：行编辑、ANSI、命令分发
flash-layout  Flash 用途划分：BSP / 门禁 / 文档的共同事实来源
filesystem        v3 文件系统：inode/目录项/硬链接/rename、分块记录、GC、写队列
mpu-regions       MPU 区域表与 RBAR/RASR 编解码（把 RBAR 区域号编码 bug 变成单测）
memory-layout    片内内存布局事实 + 栈余量测量算法（真读写内存仍在 BSP）
realtime        实时性仪表：span 统计与预算判据（纯逻辑）
fault-decode     复位原因与故障现场解码（不可信的信息不打印）
block-pool      固定块静态池的分配策略（无碎片、失败是一个值）
log       文本日志环 + gzip generation 归档索引 + 级别过滤
gzip      gzip 容器 + DEFLATE（压缩侧靠大块静态池）
nor-flash     QSPI NOR 的纪律：范围/对齐校验、命令相位、自证
sdk      capsule SDK 稳定门面：Capsule<C>/CapsuleSet/Product/prelude（事件分发未实现）
product        默认产品组合根：Platform 注入 + build，只依赖 sdk

core kernel（唯一允许 unsafe / 硬件依赖的边界）
board-support       板级支持：**唯一**允许接触 imxrt-* 寄存器的 crate
firmware        RTIC 应用与平台注入：产品组合策略在 product（S1 已外提）
xtask              构建与产物门禁（主机专用，独立工作区，不计入 20）
```

依赖方向单向向下，且**纯逻辑层的依赖闭包里不可能出现 `imxrt-*`** ——
因为它们的 `Cargo.toml` 里没有。`cargo xtask check-layering` 强制这一点。

## 八条主线（贯穿全项目的工程主张）

| 主张 | 落地方式 | 门禁/证据 |
|------|----------|-----------|
| 实时性 | 关中断窗口全部经测量入口（DWT 周期计数）；预算写成编译期断言 | `check-critical`、心跳（`beat on`）`rt=`、`bench` 硬判据 |
| 性能 | 关键路径有真机基准与基线回归；内存静态可知（固定块池） | `bench`、`check-memory`、`check-size` |
| 可维护性 | 分层是编译期事实；文档常量数字由门禁核对 | `check-layering`、`check-docs` |
| 可发展性 | Flash 分区表单一事实来源；端口契约替换实现不动上层 | `check-flash-map`、`check-image` |
| 可调试性 | 故障可解释；报事实不报意图（回读打印）；控制台逐字节校验 | `check-fault-handlers`、`bytecheck` |
| 安全性 | MPU/cache 可回读；栈余量硬下限；失联禁输出；写入 fail-closed | `check-memory`、`bench`、`primitives::safety` |
| 可测试性 | 纯逻辑主机重放；掉电原子性穷举注入；覆盖率门禁 | `test-host`、`coverage`、`check-compile-fail` |
| 可组合性 | capsule 化：业务是仓库外 crate，编译期静态组合；接口不兼容在编译期失败 | [ADR-0008](docs/adr/0008-静态capsule化内核.md)、[ADR-0010](docs/adr/0010-产品组合根外提.md)（方向已冻结；SDK/组合根 S1 已落地，S2/S3 未实现） |

## 验证现状（本机实测，2026-10-23）

```text
主机测试        450 个用例全绿（18 个纯逻辑 crate 口径；含 7 个穷举掉电注入）
目标构建        thumbv7em-none-eabihf release：text 44,792 B（ITCM 预算 65,536，余 20,744）
                .xip 84,112 B / .stack 48 KiB
Flash 映像      check-size：184,760 B / 上限 1,048,576 B（17.6%）
产物级断言      FCB tag+版本+容量、LUT 三序列 4 字节地址相位、IVT 魔数、
                向量表初始 SP=0x2000C000（DTCM 栈顶）、分区表与文档一致
编译失败负例    1 个（`Cycles` 非零：record(span, 0) 必须编译不过）
静态门禁        18 项子命令（分层 18 个纯逻辑 crate / 插件门禁 / lint 头 / unsafe 预算 81（实测 80）/
                风格 / 需求追溯 30/30 / 未实现项清点 3 项 / 产物断言 5 项）
一键验证        `make verify` == `cargo xtask verify` 全绿
目标端自检      `make test-target`：5 项（已恢复）（PIT 链 / MPU+cache / IVT 魔数 / 时基单调 / Flash 擦写回读）
日志落盘        `/log/blackbox.log` **纯文本**事件行；**64 KiB SDRAM 环**；刷盘走静态写队列；真机已读回
文件路径        Linux 风格绝对路径 + 目录合成（`/config`、`/log`、`/factory`）；终端 `ls`/`cat`/`fstat`/`rm`/`cd`/`pwd`（相对路径）
```

真机侧已上机（2026-10-09 ~ 10-13）：FCB 启动、LPUART1 控制台（逐字节校验）、
PIT 64 位链式时基（漂移 +0.002%）、SDRAM 全片 32 MiB 自检、MPU + I/D cache、
文件系统挂载与介质写入（擦除/页编程真机自证）、4 字节寻址自检、栈余量实测、
非缓存 DMA 池自证。
**2026-10-27**：`0.1.1.0`（中断/idle 的 R1–R5 优化；详见 `docs/02-架构/中断与idle轮询分析.md`）—— 真机 bytecheck / test-target 5/5 / bench --record 全过（rt=ok、txdrop=0、栈余量 89%）；上一节点 `0.1.0.0`（新仓库起点：S1 组合根外提 + 工具链跟随 stable + 适配 1.99 clippy）已完成真机验证（`bytecheck` / `test-target` 5/5 / `bench --record` 全过：`rt=ok`、`txdrop=0`、栈余量 89%）。
**未通**：CAN 外部总线接收（内部 loopback 已 PASS）、Bootloader/run-backup、看门狗
—— 见 `make gaps`。完整进展与规划见 [当前进展](docs/00-索引/当前进展.md)。

## 文档

文档地图与阅读顺序见 [`docs/README.md`](docs/README.md)；交接前先读
[`AGENTS.md`](AGENTS.md)，当前状态与规划见
[`当前进展`](docs/00-索引/当前进展.md)。
