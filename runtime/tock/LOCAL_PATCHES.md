# 本地补丁（相对上游 Tock 2.3.0）

本目录是**裁剪后的 Tock 2.3.0 源码**。除以下一个文件外，全部为上游内容，未做修改：

| 文件 | 修改 | 原因 |
|---|---|---|
| `chips/imxrt10xx/src/lpuart.rs` | 4 处 `TESSERA-PATCH`（见 `patches/` 两个 patch） | 本板控制台用 **24 MHz 晶振**（不是 EVKB 的 PLL3 80 MHz），且上游 `clear_overrun()` 清错寄存器写错 |

- `patches/0001-imxrt10xx-lpuart-rt1021-baud.patch`：OSR 5x → 16x、SBR 139 → 13（115200 @24 MHz，+0.16%）。
- `patches/0002-imxrt10xx-lpuart-clear-overrun.patch`：`CTRL::ORIE::CLEAR` → 写 1 清 `STAT.OR`。

上游版本：交接提供的 `tock-release-2.3.0`（工作区版本 `0.2.3`）。
裁剪范围：只保留 `kernel`、`arch/{cortex-m,cortex-m7,cortex-v7m}`、`chips/imxrt10xx`、
`libraries/{tock-cells,tock-tbf,enum_primitive}`、`boards/{build.rs,build_scripts}`；
上游的其它 boards/chips/capsules/tools 未入仓。

升级步骤：

1. 用新的上游 Tock 替换 `kernel/ arch/ chips/ libraries/ boards/build_scripts/`（保留 `boards/tessera-rt1021/`）；
2. 按 `patches/*.patch` 重新应用（补丁基于 2.3.0 原始 `lpuart.rs`，新版本需检查上下文）；
3. `cargo xtask tock-build && cargo xtask tock-verify`。
