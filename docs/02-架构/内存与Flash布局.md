# 内存与 Flash 布局

> **布局常量的唯一事实来源**是 [`crates/memory-layout`](../../crates/memory-layout/src/lib.rs)
> （`ITCM`/`DTCM`/`OCRAM`/`FLASH_XIP`/`SDRAM` 的基址与大小、`DMA_POOL`、
> `STACK_SIZE`/`STACK_BOTTOM`/`STACK_TOP`、SDRAM 两段池、`MIN_STACK_HEADROOM_PERCENT`）。
> BSP、`firmware/build.rs` 的编译期断言、`cargo xtask check-memory` 与本文引用同一份数据；
> 本文只解释**为什么这样分**。文中的实测数字是**快照**（每处都写了日期），会随构建变化 ——
> 要当前值就用 `cargo xtask context` 或 `cargo xtask check-memory` 的输出。

## 片上内存（FlexRAM，256 KB）

由 `imxrt-rt` 的 `Family::Imxrt1020` 默认配置（全零熔丝布局）决定：

| 区域 | 地址 | 大小 | 内容 |
|------|------|------|------|
| ITCM | `0x0000_0000` | 64 KB（2 bank） | `.text`（取指零等待） |
| DTCM | `0x2000_0000` | 64 KB（2 bank） | `.stack`（**48 KB**，最底部 `0x2000_0000`..`0x2000_C000`）、`.vector_table` @ `0x2000_C000`（632 B）、`.dtcm`（向量表后，高频小对象 + 启动清零） |
| OCRAM 可缓存区 | `0x2020_0000` | 64 KB | `.rodata`、`.data`、`.bss`、`.uninit` |
| OCRAM 非缓存 DMA 池 | `0x2021_0000` | 64 KB | USB/ENET/eDMA 的描述符与缓冲（目前**没有使用者**，只做非缓存自证） |
| Flash（FlexSPI1 XIP） | `0x6000_0000` | 32 MiB | FCB、IVT、boot data、`.xip` |
| SDRAM（SEMC CS0） | `0x8000_0000` | 32 MiB | 固定块静态池 + 空闲（见下） |

FlexRAM 共 8 bank × 32 KB。**Boot ROM 需要一个 OCRAM bank**，因此 OCRAM 从
`0x2020_0000` 而非 `0x2000_0000` 开始 —— 把 OCRAM 写成"紧接 DTCM 之后"是一个
会立刻错 2 MiB 的直觉。

### OCRAM 为什么是 64 + 64 的两半

顶部那 64 KB 划成**非缓存 DMA 池**（`memory_layout::DMA_POOL`，固定地址
`0x2021_0000`），理由是三条都成立才安全：

- **三个 DMA 引擎都不在缓存一致性域内**（核内 EHCI、ENET uDMA、eDMA，依据见
  `memory-layout` 的模块文档与 `[RM 38.1]/[RM 37.1]`）。描述符落在可缓存区会出现
  "CPU 写的描述符还停在缓存里、DMA 看到旧内容"——症状是**偶发传错一帧**，比"读回旧值"
  更难查。让它们在地址上就不可缓存，维护点就少一处；顺带规避勘误 `ERR011572`
  （Cortex-M7 的 Write-Through 路径，非缓存区不涉及）。
- **对齐有硬要求**：USB dQH / 周期帧表 4 KB 页对齐（`[RM 38.5]`），dTD 32 B 对齐，
  且 32 B 正是 cache line 大小 —— 池基址 4 KB 对齐、内部按 32 B 划分。
- **为什么是 64 KB 而不是"刚够用的 32 KB"**：ARMv7-M 的 MPU 区域大小由 `RASR.SIZE`
  编码，**必须是 ≥ 32 的 2 的幂**。第一次按 96 KB 划分时被 `mpu-regions` 的编译期断言
  当场拦下；128 KB 只能二分为 64 KB + 64 KB，没有"32 KB 池 + 96 KB 数据"这种切法。
  不留夹缝是刻意的：夹缝的缓存属性取决于它落在哪个 MPU 区域，而那是没人会去核对的
  隐式属性。

MPU 上不单独给池占区域（8 个区域已满）：把 `region6`（OCRAM）缩到 64 KB 之后，
池自然落回 `region3` 的**非缓存兜底** —— 即"高编号区域优先覆盖低编号"的直接应用。
`cargo xtask check-memory` 会断言所有落在 OCRAM 且占用 RAM 的段（含 `.bss`/`.uninit`）
**不得越过池基址** —— 链接脚本并不知道那 64 KB 已被征用，这条只能由门禁来抓。
启动报告的 `dma` 行（`  dma : pool 0x20210000+64 KiB · non-cacheable PASS`）（2026-10-13 抓包）
就是这块区域在真机上的回读证据。

## 栈：48 KiB，且余量必须来自测量

> 实测（`cargo xtask check-image` 从链接产物断言）：初始栈指针 = **`0x2000C000`**，
> 即栈位于 DTCM 最底部、向量表紧随其后 —— 与上表一致。

**栈大小是实测定的，不是估计的**：启动时做一次栈涂抹（纯逻辑在
[`memory-layout`](../../crates/memory-layout/src/lib.rs)，读 SP 与写内存的薄适配在
`board_support::stacks`），启动完成后打印最坏情况用量。当前实测
**峰值 29,860 B / 48 KiB，余量 39%**（2026-10-13 真机抓包）。

这个项目被栈打穿三次（8 KiB、16 KiB、32 KiB），症状都是"串口一行都不出"。所以：

- 48 KiB 这个值由 `crates/firmware/build.rs` 的 `STACK_BYTES` 唯一给出，
  `memory-layout` 的 `STACK_SIZE` 与链接脚本必须与它一致；
  `cargo xtask check-memory` 会断言**链接产物里 `.stack` 的实际大小**等于声明值 ——
  否则启动报告报的"栈余量百分比"就没有意义。
- 余量的**硬下限 30%**（`memory_layout::MIN_STACK_HEADROOM_PERCENT`）由
  `cargo xtask bench` 按真机自报值判定，与基线无关；基线记录 34%（2026-10-12）。
- 涂抹算法本身给过一条教训：第一版涂抹把下界写成 `STACK_BOTTOM + 4`，**栈底那个字
  从来没被涂过**，于是扫描永远"命中"在第一个字，横幅恒定打印"整段栈 / 余量 0" ——
  一个与真实用量无关的假警报。现在这段是 `memory-layout` 里的纯逻辑，含"漏涂栈底"
  的回归测试（`missing_the_bottom_word_reports_the_whole_stack_as_used`）。

## Flash 映像布局

> 本节段表为 **2026-10-16 链接产物**的历史快照；当前值以 `cargo xtask check-memory` / `check-size` 为准。

```text
偏移        内容                          大小
0x0000      FCB（FlexSPI 配置块）          512 B   tag "FCFB"、版本 1.4.0
0x0200      （填充）
0x1000      IVT（映像向量表）              32 B    魔数 0x402000D1
0x1020      boot data                      16 B    映像起点/长度/plugin 标志
0x2000      Cortex-M 向量表（LMA）         632 B   搬到 DTCM 0x2000_C000
0x2278      .xip（LMA，复位入口在其中）    16,220 B
0x61d4      .text（LMA）                   54,868 B 搬到 ITCM
0x13828     .rodata（LMA）                 28,188 B 搬到 OCRAM
0x1a648     .data（LMA）                   15,968 B 搬到 OCRAM（启动时拷入）
```

（偏移是 2026-10-16 链接产物的**实际 LMA**；各段大小之和 = `check-size` 口径的
124,068 B，与上表的段值一一对应。）

**FCB 必须位于偏移 0**：Boot ROM 只从那里读配置块。`imxrt-rt` 生成的链接脚本用
`EXTERN(FLEXSPI_CONFIGURATION_BLOCK)` 把 `#[unsafe(link_section = ".fcb")]` 的静态量
拉进 `.boot` 段；**改名或去掉 `#[unsafe(no_mangle)]`，链接器就没有理由保留它**，
FCB 槽位会被填 0，得到一个能编译却永不启动的镜像。
`cargo xtask check-image` 从链接产物断言偏移 0 处的 tag、版本（`0x56010400` = 1.4.0）、
容量与三个 LUT 序列，断言初始 SP 落在 DTCM —— 这条已经在实测中生效过。

## 外部 SDRAM 与两段固定块静态池

SDRAM 32 MiB @ `0x8000_0000`（SEMC CS0），启动时初始化并在 `idle` 循环之前做全片
自检（真机 ≈ **3.34 s**，`bench/baseline.json` 记 3342 ms）。它**不在启动链路上**：
初始化失败只降级、不变砖。

`alloc` 是**受限**引入的（见 [ADR-0003](../adr/0003-不使用alloc与async.md) 修订）：
只服务终端/诊断与压缩器状态，内存来自两段**固定块静态池**，全部常量地址 + 常量大小，
无碎片、不合并、不切分，分配失败返回空指针、绝不 panic（**平台需求 PF-2**）：

| 池 | 基址 | 块 | 块数 | 用途 |
|---|---|---:|---:|---|
| 小块池 | `0x8100_0000`（SDRAM + 16 MiB） | 1 KiB | 2048 | 终端输出缓冲、诊断临时区（失败是一个值） |
| 大块池 | `0x8120_0000`（SDRAM + 18 MiB） | 128 KiB | 32 | gzip/DEFLATE 状态（实测最大单次分配 85,196 B；128 KiB 块可接住） |

- 起点可选 16 MiB 偏移而不是紧贴 SDRAM 起点：起点留给"未来可能的大块连续缓冲"，
  16 MiB 之后仍有 16 MiB 空闲 —— 两侧都有余地，且基址是编译期常量。
- 两段池**首尾相接、不重叠**，分配器靠"指针落在哪个区间"分流 `dealloc`；
  区间重叠会让它归还到错误的池 —— 这条由编译期断言钉住。
- 启动报告的 `alloc` 行（`OK · 0/2048 blocks · high-water 1 …`）
  （**小块池**）是真机回读（2026-10-13 抓包），不是期望值；大块池的状态由终端 `mem` 查询。

**当前 SDRAM 没有真实负载**：流式访问没有缓存收益（见 [资源利用与优化评审 §2.4](资源利用与优化评审.md)），要放就放
大缓冲与有复用的工作集 —— 在拿到真实负载之前不改区域属性。

## 存储区布局

**不在本文重复**：应用数据**不各占一个分区**，而是作为文件住在**一整块文件系统分区**
里（单一属主；曾因"两套代码写同一片区域"出过事故，现在由 `check-flash-map` 的几何
判据保证不可能重新引入）。分区表与各文件的语义见
[Flash 用途划分](Flash布局与用途划分.md)；分区表的**唯一事实来源**是
[`crates/flash-layout`](../../crates/flash-layout/src/lib.rs)，
文件系统实现见 [`crates/filesystem`](../../crates/filesystem/src/lib.rs)。
`cargo xtask check-flash-map` 每次 `make verify` 都会校验对齐、不重叠、映像放得下、
以及文档表格与代码一致。

一条仍然成立的取舍：任何"双份槽位"都必须与 NOR 的 4 KiB 擦除扇区对齐 ——
`DoubleBankStore` 在编译期断言 `SLOT % SECTOR == 0`，不对齐的槽位会在擦写时
波及其它区域。

## 容量与门禁

> 本节的字节数是 2026-10-16 链接产物的**历史快照**；当前值以
> `cargo xtask check-memory` / `cargo xtask check-size` 的实时输出为准。

| 项 | 数值（2026-10-16 本机链接产物） | 门禁 |
|----|------|------|
| 固件 Flash 占用 | **124,068 B**（`check-size` 口径的加载映像） | `cargo xtask check-size` < 1 MiB |
| `.text` | 54,868 B | `cargo xtask check-memory`：必须在 ITCM 且 ≤ `ITCM_CODE_BUDGET_BYTES`（= 64 KiB − 8 KiB 余量 = 57,344 B；当前余 2,476 B） |
| `.xip` | 16,220 B | `cargo xtask check-code-placement`：在 Flash 内，且擦写期间必须能跑的模块不在 `.xip` |
| `.rodata` / `.data` | 28,188 B / 15,968 B | — |
| `.bss` / `.uninit` | 8,944 B / 72 B | 四段合计 53,172 B，落在 OCRAM 可缓存区（64 KiB）内、不侵入 DMA 池 —— 由 `check-memory` 断言 |
| `.stack` | 49,152 B @ DTCM 最低处（SP 初值 `0x2000C000`） | `check-memory`：必须等于 `memory_layout::STACK_SIZE`；余量 ≥ 30% 由 `cargo xtask bench` 判定 |
| 文件系统实例（块索引 + 文件表） | 13,752 B 常驻（`nm` 实测，2026-10-16；446 块 × 64 KiB 分区） | `check-memory`（不得侵入 DMA 池） |
| 控制台 TX 环形队列 | 8,192 B（按真机启动突发约 3.7 KB 定容，约 2.2 倍余量） | `bench` 的 `txdrop=0` 硬判据 |

OCRAM 可缓存区是当前**最紧**的片内余量（≈10 KB 量级）：四段一长，先由 `check-memory`
在"侵入 DMA 池"时给出可读的失败信息，处置是"把大块数据移去 SDRAM"，不是动池的边界。

## 2026-10-20：`.rodata` 放 Flash

`firmware/build.rs` 设置 `runtime.rodata(Memory::Flash)`；常量表/字符串留在 QSPI XIP，
不再占用 OCRAM。OCRAM 只剩 `.data/.bss/.uninit`（约 25.6 KiB），为后续静态数据留出约 39 KiB。
控制台 TX 队列迁 SDRAM 的首次尝试导致启动 Lockup，已回退；后续需先解决 SDRAM 队列的启动初始化顺序。
