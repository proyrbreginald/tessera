# AGENTS.md —— 接手一页

> 面向接手的 AI Agent/新人，只写"怎么不犯已知的错"。
> 项目：Tessera，MIMXRT1021CAG4A 的 MCU 实时底座（RTIC v2，`no_std`）。
> **唯一验收口径：`cargo xtask verify`。**

## 1. 先做三件事

1. `cargo xtask verify` —— 建立基线，必须全绿再动代码；
2. `cargo xtask context` —— 一次拿到 crate 划分、门禁、缺口、分区、预算、最近测试；
3. `cargo xtask gates` —— **18 项静态门禁**逐条状态。

规模：**20 个成员 crate**，其中 **18 个纯逻辑**（可主机测试）。

## 2. 铁律（违反即交付失败）

| # | 规则 |
|---|---|
| 1 | **全中文**：注释/文档/提交信息中文；标识符、日志、命令、路径保持英文 |
| 2 | **不猜**：硬件事实先查数据手册/原理图/板级参考固件/`docs/`，给出证据；🟡 标记不得当事实 |
| 3 | **报事实**：关键寄存器/状态必须回读打印，禁止只打印期望值 |
| 4 | **门禁前后各跑一次**：改前 `cargo xtask verify` 建基线，改后必须全绿 |
| 5 | **unsafe 有预算**：当前 **81/81**，只减不增；新增必须写 `// SAFETY:` + 更新 `unsafe.budget` + 提交说明理由 |
| 6 | **未实现留痕**：`GAP(Px)` + 明确错误变体，禁止 `todo!()`/假装成功 |
| 7 | **临时产物只放 `tmp/`** |
| 8 | **改固件行为递增 `VERSION`**（唯一真源，格式 `0.1.2.0`） |

## 3. 命令速查

| 命令 | 作用 |
|---|---|
| `cargo xtask verify` | 全量：静态门禁 + fmt/clippy + 主机测试 + 目标构建 + 产物断言 |
| `cargo xtask gates` | 全部静态门禁汇总（不中断） |
| `cargo xtask context` | 机器可读上下文（JSON） |
| `cargo xtask test-host` / `lint-host` / `lint-target` | 主机测试 / 主机 lint / 目标 lint |
| `cargo xtask coverage` | 主机行覆盖率（阈值 90%） |
| `cargo xtask check-layering` | 18 个纯逻辑 crate 的依赖闭包不含 `imxrt-*`/`cortex-m*`/`rtic` |
| `cargo xtask check-capsules` | 插件 crate 不直接依赖 BSP/固件 |
| `cargo xtask check-interrupt-priorities` | 中断任务优先级与预算表一致 |
| `cargo xtask check-unsafe` | unsafe 配平与预算 |
| `cargo xtask check-docs` | 文档数字/可达性/命令存在性 |
| `cargo xtask check-memory` / `check-image` / `check-flash-map` / `check-size` / `check-code-placement` | 产物级断言 |
| `cargo xtask check-fault-handlers` / `check-critical` / `check-compile-fail` | 故障向量 / 关中断入口 / 编译负例 |
| `cargo xtask check-style` / `check-lint-headers` / `check-api-usage` / `check-gaps` | 风格 / lint 头 / 公开项 / 缺口 |
| `cargo xtask trace` | 需求追溯矩阵（生成，勿手改） |
| `cargo xtask tock-build` / `tock-flash` / `tock-verify` / `tock-sync-boot-header` | Tock（后续固定运行时，迁移中） |

真机与交付命令见 [构建与验证](docs/构建与验证.md)；**所有入口统一为 `cargo`/`cargo xtask`，仓库不使用 make**。

## 4. 代码地图

| crate | 职责 |
|---|---|
| `primitives` `wire-format` `ports` `simulator` `sdk` `product` | 基础原语/协议/端口/主机替身/capsule SDK/组合根 |
| `sdram-timing` `shell` `flash-layout` `filesystem` `mpu-regions` `memory-layout` `realtime` `fault-decode` `block-pool` `log` `gzip` `nor-flash` | 纯逻辑子系统 |
| `board-support` | 唯一允许碰寄存器的 BSP |
| `firmware` | RTIC 应用：接线、任务划分、健康位聚合、平台注入 |
| `xtask` | 构建与门禁（独立工作区） |

分层规则与依赖表见 [分层与依赖规则](docs/02-架构/分层与依赖规则.md)。

## 5. 常见坑（都踩过）

- **不要在运行期写启动头**（偏移 0..8 KiB 只读；写坏只能 BOOT_MODE + 串口下载器救）；
- 改分区布局/FS 磁盘格式必须 `bash scripts/flash.sh -e chip` 全片擦除，否则旧数据会被误读；
- FlexSPI 不要逐字节 `read_volatile`（会像死机）；改 FlexSPI 时钟必须同时重算 dummy；
- 4 字节寻址不要退回 3 字节模式（越过 16 MiB 会静默别名，"擦日志区"可能擦掉启动头）；
- MPU `RBAR` 区域号在 bits[3:0]、bit[4] 是 VALID（编码错误会让 8 次写全落 region0）；
- DWT 会被启动期大范围内存测试清掉（根因未解释）：心跳自动重装并计数，`rt=none` 不算合格；
- 控制台全系统只能有一个 `DATA` 写入者：队列化路径只入队、绝不直写；
- 栈大小先测量再改（被 8/16/32 KiB 打穿过三次）；
- 调试器读数与固件自报冲突时以固件自报为准。

## 6. 不要做

- 换掉现有文件系统（littlefs/SPIFFS 等）：零分配/无依赖/掉电点穷举测试是资产；
- 引入 async、通用堆、`defmt`；
- 运行期热替换 capsule / ELF 动态加载；
- 把 safe capsule 当安全隔离；
- 为"看起来完成"给未知接口填默认值；
- 恢复已删除的性能基线门禁（原 bench 命令，见 [构建与验证](docs/构建与验证.md) §6）。

## 7. 未实现（别当已解决）

**3 项**，全部是平台能力：

| 里程碑 | 缺口 |
|---|---|
| **P2** | CAN 外部总线接收未在真机验证（内部 loopback 已 PASS） |
| **P4** | run/backup 引导与复制恢复未实现 |
| **P5** | 看门狗未启用（`WDG_CTR` 驱动源未确认） |

详见 [安全与约束](docs/安全与约束.md)。业务不在本仓库；接入见 [业务接入](docs/业务接入.md)。
