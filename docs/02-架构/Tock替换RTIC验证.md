# Tock 替换 RTIC：可行性验证记录

> **状态：验证已通过（实验性），生产固件仍为 RTIC。**
> 实验产物在仓库根 [`tock/`](../../tock/README.md)（板级 crate + 补丁 + 构建脚本）；
> 本文只记录**做了什么、结论是什么、还差什么**。

## 1. 背景

平台当前运行时是 RTIC v2（[ADR-0001](../adr/0001-运行时选型rtic-v2.md)）。
2026-10-27 要求评估 **Tock OS** 作为替代：用 Tock 的 kernel/chip/interrupt 模型替换
RTIC 的任务与中断绑定，并**完全沿用现有 Flash 布局与启动头（FCB/IVT）**。
验收口径定为「最小化可跑测」：真机启动、串口可用、中断驱动路径成立。

## 2. 做法（最小替换）

- 使用外部 Tock 2.3.0 源码树（交接提供，位于 `tmp/`，**不内联进本仓库**）；
- 在 Tock 里新增板级 crate `boards/tessera-rt1021`：
  - 芯片支持复用 Tock 的 `chips/imxrt10xx`（同族 i.MX RT）；
  - 架构复用 `arch/cortex-m7`；
  - **启动头直接复用**现有生产固件的前 8 KiB（FCB+IVT，IVT entry = `0x6000_2000`），
    `.boot_hdr` 放在 `0x6000_0000`，内核 `.text` 从 `0x6000_2000` 开始；
  - 内存布局：hdr `0x6000_0000`/8 KiB、rom `0x6000_2000`/248 KiB（现有 `bootloader` 区）、
    prog `0x6004_0000`/2 MiB（现有 `slot_run`，不装 app）、ram `0x2020_0000`/128 KiB（OCRAM）；
  - 无 userspace：`Kernel::new(&[])` + `NoSched`；
  - 只初始化 LPUART1（本板 `GPIO_AD_B0_06/07`，24 MHz 晶振 → 115200），
    RX 用双 1 B 缓冲 + 行缓冲，输出走 `DeferredCall`；启动后按行回显。
- 对 Tock 源码只改 `chips/imxrt10xx/src/lpuart.rs`（4 处，导出为 2 个 patch）：
  ① RT1021 的 24 MHz 晶振下 OSR=16x/SBR=13；② `clear_overrun()` 用 W1C 正确清 `STAT.OR`。

工具链：**stable rustc 1.99.1**（Tock 默认 nightly 配置中的 `-Z` 参数已替换为 stable
兼容 flags）。完整构建/烧录步骤见 [`tock/README.md`](../../tock/README.md)。

## 3. 结论（真机事实）

镜像烧录后（全片擦除）串口输出：

```text
tock: tessera-rt1021 kernel boot
tock: kernel_loop running (echo mode)

tock: rx line(10B): HELLO-TOCK
tock: rx line(16B): alpha-0123456789
tock: rx line(11B): second-line
```

由此确认：

| 验证点 | 结果 |
|---|---|
| FCB/IVT/复位向量/向量表链路 | ✅ Boot ROM 能从现有 8 KiB 启动头启动 Tock 镜像 |
| Tock `Chip::init` / NVIC / 向量表偏移 | ✅ `BASE_VECTORS` 在 `0x6000_2000`，复位向量指向 `initialize_ram_jump_to_main` → `main` |
| Tock `Kernel::kernel_loop` 运行 | ✅ 空闲循环进入 `wfi`，中断可唤醒并分发 |
| 中断驱动 UART | ✅ LPUART1 RX 中断 → 内核 → 行缓冲 → `DeferredCall` 回显 |
| 现有 Flash 布局复用 | ✅ 未改分区表/启动头；内核落在 `bootloader` 预留区，`slot_run` 仍保留 |

镜像规模：ROM 内容约 **26 KiB**（rom 区 248 KiB 富余）；RAM 用 OCRAM 128 KiB 中约 20 KiB。

**已知限制（真实存在，不掩饰）**：

1. **连续输入会丢字节**：LPUART RX FIFO 只有 1 B，Tock 逐字节回调模型没有 RX 环形队列；
   逐字节/按行发送完整正确，流水线连续多行会丢（实测每 3 字节左右丢 1）。
2. 无 userspace/TBF/MPU 进程隔离、无 syscall；无 capsule/组件（终端/FS/日志/诊断未迁移）。
3. 生产 BSP（SEMC/SDRAM、MPU/cache、FlexSPI 自检、看门狗）未接入 Tock。
4. 栈 16 KiB 为粗估未做涂抹测量；`boot_data.size` 仍是旧固件长度（未开 HAB，未影响启动）。

## 4. 与生产固件的关系

- **生产固件不变**：`crates/firmware` 仍用 RTIC，`VERSION` 未递增（本次不改变交付行为）；
- 实验镜像独立构建（`tock/build.sh`），烧录会覆盖板上的生产固件；
  验证完用 `bash scripts/flash.sh -e chip` 恢复生产固件；
- 两套入口共用同一份 Flash 分区与启动头，这是本次验证刻意保持的约束 ——
  它证明「换运行时不必换存储布局」，也说明后续迁移可以在同一分区模型下进行。

## 5. 继续推进的前置条件

在把 `crates/firmware` 真正切到 Tock 之前，至少需要：

1. RX/TX 环形队列或 DMA（解决连续输入丢字节）；
2. 把终端/日志/FS 作为 Tock in-kernel 组件（或 userspace 进程）迁移，并保留现有语义；
3. 时基/调度：Tock 调度器面向进程，纯内核任务需要明确优先级与 1 ms 时基（GPT/PIT）；
4. BSP 迁移：SDRAM、MPU/cache、FlexSPI 与启动自检；栈预算用涂抹实测；
5. 重新建立等价门禁：`cargo xtask verify`、真机 `bytecheck`/`test-target`/`bench`
   全部通过后，才谈替换。

在此之前，[ADR-0001](../adr/0001-运行时选型rtic-v2.md) 仍是生产运行时的有效决策；
本次验证只作为「Tock 可行性」的证据记录，不改变架构现状。
