# ADR-0002：工作区分层与 unsafe 边界

## 状态

已采纳（决策）

## 决策

**20 个成员 crate**（其中 **18 个纯逻辑**）单向依赖；**只有 `board-support`（库层）与
`firmware`（组合根）允许依赖 `imxrt-*` / RTIC**；纯逻辑层
`#![forbid(unsafe_code)]`；unsafe 站点数进 [`unsafe.budget`](../../unsafe.budget)，
**只减不增**。

```text
primitives ──▶ wire-format ──▶ ports
   │                            ▲
   │                            └── simulator（主机替身；另依赖 core/proto，仅测试/自检）
   ├─▶ flash-layout（无依赖） / mpu-regions / memory-layout / realtime / fault-decode /
   │    block-pool / nor-flash / sdram-timing / log / gzip / filesystem /
   │    shell（另依赖 realtime）
                              │
                              ▼
       18 个纯逻辑 crate ──▶ board-support（唯一允许依赖 imxrt-* 的库）──▶ firmware（RTIC 组合根）
```

（逐 crate 的"允许依赖谁"见 [`分层与依赖规则`](../02-架构/分层与依赖规则.md)，
名单的唯一来源是 `xtask` 的 `PURE_CRATES`。）

## 理由

真实的寄存器写入、中断时序与电气连接**无法**在主机上验证。既然无法消除这一层，
就让"应用逻辑不认识硬件"成为**编译期事实**：纯逻辑 crate 的 `Cargo.toml` 里
根本没有 `imxrt-*` —— 写 `use imxrt_hal::...` 直接编译失败。

这比"请在应用代码里不要写寄存器"这类约定强得多：约定会被忘记，依赖图不会。

## 配套门禁（缺一不可）

| 门禁 | 防的是什么 |
|------|-----------|
| `cargo xtask check-layering` | 依赖方向被破坏（含 `dev-dependencies` 与传递闭包） |
| `cargo xtask check-lint-headers` | `forbid(unsafe_code)` 被静默删掉 |
| `cargo xtask check-unsafe` | unsafe 增长、以及缺少逐处举证 |

## 后果

- **正面**：不可测的面积被压到最小；换芯片只需改 BSP 与一处 feature 名。
- **代价**：多出 18 个 crate 的样板与一次跨 crate 的调用开销（可被 LTO 消除）。
- **代价**：`unsafe` 无法真正消除，只能"集中 + 计数 + 逐处举证"（当前 **55 处 / 预算 56**，
  全部集中在 `board-support` 与 `firmware`；`cargo xtask check-unsafe` 复测）。
