# Tock 运行时

> **Tock 是后续固定运行时（已确定采用，不再选型/不会替换）**，源码在顶层 `tock/`。
> 当前生产固件仍是 RTIC；迁移按本文 §4 的前置条件推进，完成前不以 Tock 替换生产入口。

## 1. 目录

```text
tock/
  kernel/ arch/ chips/ libraries/ boards/build_scripts/  # vendored 上游 2.3.0（裁剪）
  boards/tessera-rt1021/   # ★ 唯一本地维护的板级 crate
  patches/                 # 对上游的 2 个补丁（lpuart 4 处）
  README.md / LOCAL_PATCHES.md
```

- 裁剪：只保留本板需要的 kernel/arch/cortex-m{,-m7,-v7m}/chips/imxrt10xx/libraries/构建脚本；
- 启动头**直接复用**生产固件的 8 KiB（FCB+IVT，entry=`0x6000_2000`），Flash 分区/布局不变；
- 无 userspace（`Kernel::new(&[])`），只验证启动 + 中断驱动串口回显。

## 2. 使用

```bash
cargo xtask tock-build            # 构建
cargo xtask tock-verify           # 构建 + 烧录 + 串口断言
cargo xtask tock-sync-boot-header # 生产启动头变更后同步 boot_hdr.bin
```

真机输出（已验证）：

```text
tock: tessera-rt1021 kernel boot
tock: kernel_loop running (echo mode)
tock: rx line(11B): TOCK-VERIFY
```

## 3. 当前差距（迁移完成前必须补齐）

1. **连续输入丢字节**：LPUART RX FIFO 仅 1 B + Tock 逐字节回调；逐字节/按行正确，
   流水线连续多行约每 3 字节丢 1。需补 RX 环形队列/中断驱动 TX/DMA。
2. 无 userspace/TBF/MPU 进程隔离、无 syscall、无 capsule 接线；终端/FS/日志/诊断未迁移。
3. 生产 BSP（SEMC/SDRAM、MPU/cache、FlexSPI 自检、看门狗）未接入。
4. 栈 16 KiB 粗估未测量；`boot_data.size` 仍是旧固件长度（未开 HAB，未影响启动）。

## 4. 继续推进的前置条件

RX/TX 队列 → capsule/进程迁移 → 时基/调度语义 → BSP 迁移 → 重新建立等价门禁
（`cargo xtask verify` + 真机 `bytecheck`/`test-target`）后，才谈切换 `crates/firmware`。

迁移计划见 [Tock 迁移计划](Tock迁移计划.md)；维护约定与升级步骤见 [`tock/README.md`](../tock/README.md)、
[`tock/README.md`](../tock/README.md)、
[`tock/LOCAL_PATCHES.md`](../tock/LOCAL_PATCHES.md)。
