# Tessera —— MCU 实时底座

Tessera 是一块 NXP **MIMXRT1021CAG4A**（Cortex-M7 @396 MHz）的实时**底座**：
启动链、外设驱动、存储/日志、实时仪表、故障解码、终端与构建门禁。
它**不含业务语义**；业务 crate 在仓库之外，通过 capsule SDK/端口契约接入。

- 语言/运行时：100% Rust，`no_std` + **RTIC v2**，零 `async`，`alloc` 仅限固定块静态池；
- 外部 Flash：W25Q256JVEQ 32 MiB（QSPI XIP `0x6000_0000`）；外部 SDRAM 32 MiB @ `0x8000_0000`；
- 控制台：LPUART1 115200 8N1；SWD 调试；
- 唯一验收口径：`cargo xtask verify`。

## 快速开始

```bash
cargo xtask verify          # 门禁 + fmt/clippy + 主机测试 + 目标构建 + 产物断言
cargo build --release       # 只构建固件 ELF（默认 target 就是固件目标）
cargo xtask context         # 机器可读项目上下文（crate/门禁/缺口/预算/最近测试）
```

真机（需要台架 pyocd/pyserial；详见 [构建与验证](docs/构建与验证.md)）：

```bash
bash scripts/flash.sh -e chip   # 全片擦除 + 烧录（改分区/FS 格式后必须）
bash scripts/read_console.py --seconds 5   # 看串口
cargo xtask bytecheck                  # 控制台逐字节校验（改时钟/中断后必跑）
cargo xtask test-target                # 目标端自检 5 项
```

## 仓库结构

```text
crates/       生产固件 + 纯逻辑层：20 个成员 crate（18 个纯逻辑，可主机测试）
tock/         后续固定运行时（vendored Tock 2.3.0 裁剪版 + 本地板级 crate）
docs/         核心文档（重构后的精简版）
scripts/      台架/烧录/抓包脚本（bash）
xtask/        构建与门禁（Rust，独立工作区；不是成员 crate）
tests/        主机集成测试与编译失败负例
```

> 原来的 `bench/`（真机性能基线）与对应命令已删除：
> 测量受缓存/温度影响大、维护成本高于收益；真机事实改由启动横幅回读 +
> `bytecheck`/`test-target`/`monitor` 提供。

## 当前状态

| 项 | 值 |
|---|---|
| 版本 | `0.1.2.0`（`VERSION` 唯一真源；改固件行为才递增） |
| 工作区成员 crate | 20（18 个纯逻辑） |
| 静态门禁        18 项子命令 | `cargo xtask gates` 逐条查看 |
| 未实现缺口 | 3（P2 CAN 外部接收、P4 Bootloader、P5 看门狗） |
| Tock 运行时 | **后续固定采用，不再替换**；已真机验证启动/串口；当前生产仍是 RTIC，迁移中 |

会漂移的测量值（主机测试数、覆盖率、映像字节数）不写死在这里：
用 `cargo xtask context` 获取。

## 文档

| 文档 | 内容 |
|---|---|
| [项目地图](docs/00-索引/项目地图.md) | 一页纸总览、分层、缺口、验证入口 |
| [架构与实现](docs/架构与实现.md) | 启动链、RTIC 任务/中断/idle、capsule、FS/日志/控制台 |
| [硬件与板级事实](docs/硬件与板级事实.md) | MCU/时钟/引脚/Flash/SDRAM/启动头/诊断 |
| [构建与验证](docs/构建与验证.md) | 全部 cargo/台架命令、门禁清单、排障 |
| [安全与约束](docs/安全与约束.md) | unsafe 预算、fail-closed、安全门控、缺口 |
| [业务接入](docs/业务接入.md) | capsule SDK/端口契约、仓库外业务 crate |
| [Tock 运行时](docs/Tock运行时.md) | 固定运行时（迁移中）：构建/烧录/限制 |
| [Flash 布局](docs/02-架构/Flash布局与用途划分.md) · [分层规则](docs/02-架构/分层与依赖规则.md) | 分区表与依赖边界（门禁核对） |
| [需求规格](docs/01-需求/需求规格.md) | 30 条平台需求（`cargo xtask trace` 生成追溯矩阵） |

## 硬约束（详见文档）

- 全部注释/文档/提交信息用中文；标识符、日志、命令、路径保持英文；
- 新增 `unsafe` 必须配 `// SAFETY:` 并更新 `unsafe.budget`（当前 81/81，只减不增）；
- 未实现必须留 `GAP(Px)` + 显式错误变体，禁止 `todo!()`/假装成功；
- 临时产物只放 `tmp/`；
- 不在运行期写启动头；改分区/FS 格式必须全片擦除；
- 工具链跟随最新 stable，不锁定具体补丁版本。
