# ADR-0005：FlexCAN 与 FlexSPI 擦写由 BSP 自研

## 状态

已采纳（决策）

## 决策

`board-support` 直写寄存器实现 FlexCAN 与 FlexSPI 擦写：FlexCAN 走 `imxrt-ral` 的寄存器块；
FlexSPI 因 `imxrt-ral` 没有寄存器块，走"裸地址 + `volatile`"（页编程），擦除复用
Boot ROM 的 NOR 驱动（ABI 用 `const` 尺寸/偏移断言钉住）。焊盘手工配置：LPUART 走
`imxrt-hal` 的 pads，CAN 按官方 IOMUXC 表直写 mux/daisy。

**在实现之前，收发一律返回 `NotImplemented`，绝不假装成功。**

## 理由

`imxrt-hal` 0.6 **没有** FlexCAN 与 FlexSPI 驱动（其 `chip/imxrt1020.rs` 的导出表里
没有这两个模块）。`imxrt-iomuxc` 0.3.2 只实现了 **imxrt1060** 的 `flexcan::Pin`，
1020/1021 没有；由于 orphan rule，下游 crate **无法**为 1020 的焊盘补实现。

因此 CAN 只有两条路：手配焊盘 + 直写寄存器。把它们关在一个 crate 内，
用 unsafe 预算与逐处举证约束，上层只看到 `ports::Bus`。

## 后果

- **正面**：架构不被"上游没做驱动"阻塞，且缺口被隔离成一个可计划的里程碑。
- **代价**：自研驱动需要自己负责位时序计算、MB 管理、错误界定 —— 且**必须有 HIL
  证据**（编译通过不算驱动可用）。当前状态：只读探针与写序列已落地，
  **外部总线接收未在真机验证通过**（内部 loopback 已于 2026-10-19 实测 PASS），见 `GAP(P2)`。
- **额外发现**：CAN2 的焊盘与复用值**曾经是一处真实风险**（按"宏名目录"猜焊盘
  会把 ENET 焊盘顺手改坏）。现在已用官方 `fsl_iomuxc.h` + 板级 C 固件
  `pin_mux.c` **双证确认**：`GPIO_AD_B1_00/01`、ALT=**2**、RX daisy=**3**
  （逐条标注在 `board-support/src/config.rs`，证据见 [`板级事实`](../03-硬件/板级事实.md) §2.4）。
  当前缺口不在引脚，而在接收路径未通（`GAP(P2)`，见 [模块分解](../02-架构/模块分解.md)）。
