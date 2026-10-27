# Tock 迁移计划（RTIC → Tock，最终删除 RTIC）

> 结论：**当前生产固件仍是 RTIC**（仅 `crates/firmware` 依赖 `rtic`）。
> Tock 已在 `tock/` 完成最小真机验证（启动 + 串口回显），但还没有生产功能；
> 本文是**完全切换并最终删除 `rtic` 依赖**的分阶段执行计划。

## 0. 现状与边界

| 层 | 现状 | 切 Tock 是否要动 |
|---|---|---|
| 18 个纯逻辑 crate | 与运行时无关 | 不动 |
| `product` / `sdk` / `ports` | 静态组合根与端口契约 | 复用，可能改 `CapsuleContext` 接线 |
| `board-support` | 唯一碰寄存器的 BSP（时钟/控制台/PIT/FCB/MPU/SEMC/FS） | **复用**，但链接段/初始化入口要适配 Tock |
| `firmware` | RTIC app：2 个 ISR + idle 协作循环 + 4000+ 行接线 | **整体替换** |
| `tock/` | vendored Tock + `boards/tessera-rt1021` 最小验证 | 扩展为生产入口 |

RTIC 依赖面很小：工作区 `rtic`、`firmware/Cargo.toml`、`firmware/src/device.rs`、
`main.rs` 的 `#[rtic::app]` 与 `#[task(binds=LPUART1)]`、`#[task(binds=PIT)]`。

## 1. 阶段 1：Tock 接管启动与 BSP

**目标**：Tock 入口调用现有 `board_support::init()`，串口终端可用、心跳可见；
生产固件仍为 RTIC（双轨，可回退）。

工作项：

1. `tock/boards/tessera-rt1021` 增加 `board-support`（path 依赖）；
2. 链接脚本补齐 BSP 需要的段/符号：`.xip`、`.dtcm`、`.uninit`、`.fcb`、
   `__sdtcm/__edtcm`；保持 `.boot_hdr` 复用现有 FCB/IVT；
3. 用自定义 `Chip`/`InterruptService` 替代 Tock 的 imxrt 外设分发：
   - LPUART1 → `board_support::console::isr_service()`；
   - PIT → 清 tick + 递增 1 ms 计数（避免 `Unhandled interrupt 122`）；
4. 自定义 Scheduler 的 `execute_kernel_work` 在无进程场景下驱动：
   `console::flush`、RX 消费、1 Hz 心跳打印；
5. 板级 main：`board_support::init(banner.txt, VERSION)` → 构造 Tock `Kernel`。

**验收**：Tock 镜像真机启动；`cargo xtask monitor` 能读到心跳且漂移 ±2%；
`cargo xtask bytecheck` 的理想子集（横幅/身份行/终端就绪）通过。

## 2. 阶段 2：中断与时基完全脱离 RTIC 语义

- `deadline_tick` 的健康位/`realtime` span、`console_rx` 的预算测量搬进 Tock ISR 包装；
- `check-interrupt-priorities` 改为检查 Tock ISR 注册表（不再解析 `#[task]`）；
- 心跳输出与 `beat on/off` 行为与当前固件一致。

**验收**：`cargo xtask verify` 全绿；`cargo xtask gates` 18 项全过（门禁适配后）。

## 3. 阶段 3：服务迁移（工作量最大）

把 `firmware/src/main.rs` 里的接线与 idle 相位迁到 Tock 内核上下文：

- 终端输入/命令分发（复用 `shell` 纯逻辑）；
- 文件系统挂载/写队列/日志/轮转；
- 启动报告、诊断命令、`selftest`、健康位聚合；
- 保持命令/输出格式不变（`bytecheck` 是回归判据）。

**验收**：真机 `cargo xtask bytecheck` 全量通过、`test-target` 5/5、`soak` 通过；
生产功能与当前 RTIC 固件等价。

## 4. 阶段 4：门禁/测试/启动链对齐

- `check-memory`/`check-image`/`check-size`/`check-flash-map`/`check-code-placement`
  改为对 Tock ELF 的等价断言；
- `check-layering` 允许 `tock/` 不在 Tessera workspace（但禁止生产 crate 依赖 Tock）；
- CI/xtask `tock-*` 升级为生产构建入口，保留 `verify` 单一验收口径。

## 5. 阶段 5：删除 RTIC

- 移除工作区 `rtic` 依赖、`firmware/src/device.rs`、`#[rtic::app]`/`#[task]`；
- `firmware` crate 删除或替换为 Tock 入口（推荐直接迁入 `tock/boards/tessera-rt1021`，
  保留 `crates/` 纯逻辑与 BSP 不变）；
- 文档、`AGENTS.md`、门禁表同步；`VERSION` 递增（固件行为变更）。

**完成判据**：`cargo xtask verify` 全绿、全仓库无 `rtic` 依赖、真机
`bytecheck`/`test-target`/`soak` 全过。

## 6. 风险与前置条件

1. **BSP 与 Tock linker 的段布局冲突**是阶段 1 最大不确定点；先做链接/启动验证，再谈功能。
2. **Boot ROM 遗留 PIT 中断**必须显式处理（已复现 `Unhandled interrupt 122`）。
3. 板子恢复：正常 Flash 启动模式下才能做真机验证；BOOT_MODE 下载模式会停在 Boot ROM。
4. 任何阶段失败都可回退到 RTIC 生产固件（双轨构建）。
