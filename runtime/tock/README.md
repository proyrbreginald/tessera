# runtime/tock —— 裁剪后的 Tock 2.3.0（实验性运行时）

> 本目录是 **vendored 上游源码 + 唯一本地板级 crate**，不是 Tessera 生产固件，
> 也不属于 Tessera 的 cargo workspace。维护约定：
>
> - `kernel/ arch/ chips/ libraries/ boards/build_scripts/`：**vendored 上游 Tock 2.3.0**，
>   只保留本板需要的子集；不要在这里做业务改动，升级时整体替换；
> - `boards/tessera-rt1021/`：**本仓库唯一本地维护**的板级 crate（板级入口/引脚/UART 回显）；
> - `patches/`：对上游的 2 个本地补丁（`chips/imxrt10xx/src/lpuart.rs`），
>   vendored 源码里已应用并带 `TESSERA-PATCH` 标记；升级上游时按 patch 重新应用。
>
> 构建/烧录/验证（不用 make）：
> ```bash
> cargo xtask tock-build    # 构建
> cargo xtask tock-flash    # 构建 + 全片擦除烧录
> cargo xtask tock-verify   # 构建 + 烧录 + 串口验证（启动横幅 + 行回显）
> cargo xtask tock-sync-boot-header  # 生产固件启动头变化后，重新同步 boot_hdr.bin
> ```

## 原验证记录


> **状态：验证已通过，不是生产替换。** 生产固件仍是 RTIC（`crates/firmware`，`VERSION` 0.1.2.0）。
> 本目录（`runtime/tock`）只是一次**技术可行性验证**：把 Tessera 的固件入口换成 Tock 2.3.0 内核，
> 复用现有 Flash 布局与启动头（FCB/IVT），在真实板卡上跑通「启动 + 中断驱动串口回显」。

## 1. 结论（事实）

| 项 | 结果 |
|---|---|
| 工具链 | stable `rustc 1.99.1` + Tock 2.3.0（官方 nightly 配置已替换为 stable 兼容 flags） |
| 启动 | **真机通过**：串口打印 `tock: tessera-rt1021 kernel boot` / `tock: kernel_loop running (echo mode)` |
| Flash/FCB | **完全复用**生产固件的 8 KiB 启动头（FCB+IVT，entry=0x6000_2000），未改分区表 |
| 内核 | Tock `Kernel::kernel_loop` 运行，无 userspace 进程（`Kernel::new(&[])`） |
| 中断 | LPUART1 RX 中断由 Tock 内核分发；收到整行后经 `DeferredCall` 回显：`tock: rx line(NB): ...` |
| 镜像 | ROM 内容约 **26 KiB**，`.boot_hdr` 在 0x6000_0000、`.text` 在 0x6000_2000，RAM 用 OCRAM 0x2020_0000 |
| 版本 | 生产 `VERSION` **不变**（本次不改变交付固件行为）；实验镜像独立构建 |

真机证据（2026-10-27，台架 0.1.2.0 之后的新镜像）：

```text
tock: tessera-rt1021 kernel boot
tock: kernel_loop running (echo mode)

tock: rx line(10B): HELLO-TOCK
tock: rx line(16B): alpha-0123456789
tock: rx line(11B): second-line
```

（连续流水发送时每 3 字节左右会丢 1 字节：LPUART RX FIFO 只有 1 字节，
Tock 的逐字节回调模型没有 RX 环形队列。逐字节/按行发送时完整正确。这是本验证的
已知限制，不是内核启动问题。）

## 2. 复现

前置：Tock 2.3.0 源码放在 `tmp/tock-release-2.3.0`（交接时提供；`tmp/` 不入库），
或设置 `TOCK_DIR` 指向其它位置。

```bash
# 仓库根目录执行（工具链为仓库 stable）：
cargo xtask tock-build     # 产物：runtime/tock/target/thumbv7em-none-eabihf/release/tessera-rt1021
cargo xtask tock-flash     # 全片擦除后烧录
cargo xtask tock-verify    # 烧录并验证启动横幅 + 行回显
```

板级 crate 的详细说明、逐项自检与踩坑记录见
[`boards/tessera-rt1021/README.md`](boards/tessera-rt1021/README.md)。

## 3. 相对生产固件做了什么

| 项 | 生产（RTIC） | 本验证（Tock） |
|---|---|---|
| 调度/中断 | RTIC v2 静态任务（LPUART1/PIT 各一个） | Tock `Kernel` + `Chip`/NVIC 中断分发 + `NoSched`（无进程） |
| 启动头 | `imxrt-rt` 构建期生成 FCB/IVT | **原样复用**生产固件的 8 KiB 启动头 |
| Flash 布局 | boot_header/bootloader/slot_run/... | 完全沿用；内核在 `bootloader` 区 0x6000_2000 起 |
| 控制台 | 生产 BSP（imxrt-hal）+ 终端/日志/FS | Tock `imxrt10xx::lpuart` + 行缓冲 + DeferredCall 回显 |
| 功能 | 完整平台（终端/FS/日志/诊断） | 仅启动 + 串口回显 |

对 Tock 源码的修改只有 `chips/imxrt10xx/src/lpuart.rs` 的 2 个 patch（4 处）：
1. `0001`：RT1021 控制台时钟是 24 MHz 晶振 → OSR=16x / SBR=13（原为 EVKB 的 PLL3 5x/139）；
2. `0002`：`clear_overrun()` 改为写 1 清 `STAT.OR`（原来清错寄存器，overrun 标志会粘滞）。

## 4. 为什么还不算「替换完成」

本验证只证明**工具链/启动链/中断分发**可行，以下都未做，不能当成现状：

- **没有 userspace/进程模型**：`Kernel::new(&[])`、`NoSched`，没有 TBF、MPU 进程隔离、syscall；
- **没有 capsule/组件接线**：终端、文件系统、日志、诊断、PIT 时基都还在生产固件里；
- **没有移植生产 BSP**：SDRAM、SEMC、MPU/cache、FlexSPI、FCB 自检、看门狗等未接入 Tock；
- **RX 吞吐限制**：1 字节 FIFO + 逐字节回调，连续输入会丢字节；需要 RX 环形队列/DMA；
- **栈未做涂抹测量**（16 KiB 粗估），未启用看门狗；`boot_data.size` 仍是旧固件长度（未开 HAB，未影响启动）。

## 5. 若继续推进的合理顺序

1. **RX 队列**：在 Tock HIL 之上补环形队列/中断驱动 TX，解决连续输入丢字节；
2. **Capsule 接线**：把生产里的终端/日志/FS 作为 Tock in-kernel 组件接入（或进程化）；
3. **调度与实时性**：Tock 的调度器面向进程；纯内核任务需要明确优先级/时基（GPT/PIT）；
4. **BSP 迁移**：SDRAM/MPU/cache/FlexSPI 自检与现有 Flash 布局的对齐；
5. 全部通过现有 `cargo xtask verify` + 真机 `bytecheck`/`test-target`/`bench` 后，
   才谈把 `crates/firmware` 切到 Tock。
