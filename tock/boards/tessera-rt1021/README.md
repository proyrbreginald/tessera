# tessera-rt1021 —— 最小 Tock 板级 crate（RTIC 替换可行性验证）

目标只有一个：在现有 Tock 2.3.0 源码树里**编译并链接通过**一个最小的
`MIMXRT1021CAG4A` 板级 crate，用来判断「把 Tessera 的 RTIC 应用换成 Tock」
在工具链/内存布局层面是否可行。

范围刻意压到最小：

- 无 userspace / 无进程加载（`Kernel::new(&[])`，进程槽 0 个）；
- 不引入 capsules / components / alloc / async；
- 只初始化 LPUART1 控制台，并把收到的字节**按行**回显；
- 启动头（FCB + IVT，8 KiB）**原样复用**现有 Tessera 固件，不改内容。

> 本 crate 只到「编译 + 静态自检」为止，烧录与串口验证由 Lead 负责。

## 1. 构建

```bash
cd tmp/tock-release-2.3.0/boards/tessera-rt1021
RUSTUP_TOOLCHAIN=stable cargo build --release
```

- 工具链：stable `rustc 1.99.1 (58cf8f9c3 2026-10-13)`，target `thumbv7em-none-eabihf`。
- 产出：`tmp/tock-release-2.3.0/target/thumbv7em-none-eabihf/release/tessera-rt1021`
- 注意：**不要**用 nightly。Tock 默认配置里的 `-Z emit-stack-sizes` / `build-std`
  是 nightly-only，本板 `.cargo/config.toml` 已改为 stable 兼容的 flags。

## 2. 交付物 / 改动清单

本板内：

| 文件 | 状态 | 说明 |
|---|---|---|
| `src/main.rs` | 新增 | 内核入口、CCM/IOMUXC/LPUART1 配置、空进程 `Kernel`、`NoSched` 调度器、`Platform`/`KernelResources` 实现、双缓冲 RX + 行缓冲 + 延迟按行回显 |
| `src/io.rs` | 新增 | 最小 `Writer`（`core::fmt::Write` → LPUART1 `send_byte`）+ `#[panic_handler]` |
| `.cargo/config.toml` | 修改 | stable flags；`rustflags` 从 `[build]` 移到 `[target.thumbv7em-none-eabihf]`；原因见 §5.1 |
| `imxrt-link.x` | 新增（空占位） | 吸收上层工作区配置漏进来的 `-Timxrt-link.x`；原因见 §5.1 |
| `tock-patches/0001-imxrt10xx-lpuart-rt1021-baud.patch` | 新增 | 3 处波特率 `TESSERA-PATCH` 的 unified diff |
| `tock-patches/0002-imxrt10xx-lpuart-clear-overrun.patch` | 新增 | `clear_overrun()` 清错寄存器的 `TESSERA-PATCH`（真机第二轮修改） |
| `src/boot_header.rs`、`boot_hdr.bin` | 未改 | 复用现有 Tessera 固件前 8 KiB；来源见 §7 |
| `layout.ld`、`chip_layout.ld` | 未改 | hdr 0x6000_0000/8K，rom 0x6000_2000/0x3E000，prog 0x6004_0000/2M，ram 0x2020_0000/128K |

对 Tock 源码的修改（全部在 `chips/imxrt10xx/src/lpuart.rs`，均带 `TESSERA-PATCH:` 注释，
共 **4 处**，已导出为 §2 的两个 patch）：

| # | 位置 | 上游值 | 本板值 | 原因 | patch |
|---|---|---|---|---|---|
| 1 | `Lpuart::set_baud()` | `SBR=139`（5x 过采样） | `OSR=15(16x) + SBR=13 + BOTHEDGE=0` | 控制台时钟源是 24 MHz 晶振而非 EVKB 的 PLL3 80 MHz | 0001 |
| 2 | `configure()` OSR 行 | `OSR=0b100(5x)` | `OSR=0b1111(16x)` | 同上；SBR 只在 16x 过采样下等于 115200 | 0001 |
| 3 | `configure()` SBR 行 | `SBR=139` | `SBR=13` | 同上 | 0001 |
| 4 | `clear_overrun()` | `CTRL::ORIE::CLEAR`（关中断使能，没清标志） | `STAT::OR::SET`（W1C 正确清法） | 否则一次 overrun 后 `STAT.OR` 永久置位，每次中断都会走一遍 OR 分支抢走刚重挂的接收缓冲 | 0002 |

波特率算式：`24e6 / 16 / 13 = 115384.6 Baud`，相对 115200 误差 **+0.16%**
（NXP 要求 < 3%，Tessera 现有固件同样用 +0.16%）。

`BOTHEDGE` 说明：上游 `configure()` 无条件置位；NXP 只在 OSR=4..7 才允许双沿采样
（imxrt-hal `Baud::compute` 按 `4 <= osr <= 7` 决定），16x 过采样下必须清零。
置 0 后与现有 Tessera 固件（imxrt-hal 对 24 MHz/115200 算出的 `osr=8, sbr=26,
bothedge=false`）**波特率完全等价**。本板调用顺序是 `configure()` → `set_baud()`，
最终状态由 `set_baud()` 决定。

## 3. 自检结论（stable 1.99.1，`cargo build --release` 成功、无 warning）

用 `$(RUSTUP_TOOLCHAIN=stable rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/`
下的 `llvm-objdump` / `llvm-nm` / `llvm-objcopy`（stable 未附带 `llvm-readelf`，用
`llvm-objdump -h` 等价读取段表）：

| 项 | 结果 |
|---|---|
| ELF | 477,780 B（含 debug info）；`llvm-nm` 可见 `initialize_ram_jump_to_main` / `main` |
| 第一个 PT_LOAD | VMA=LMA `0x6000_0000`，大小 `0x2000`（`.boot_hdr`） |
| `.boot_hdr` 前 4 字节 | `46 43 46 42` = **`FCFB`**（ELF 文件偏移 0x174；`.bin` 偏移 0） |
| IVT（boot_hdr+0x1000） | tag `0xd1`，`entry = 0x6000_2000` —— 正好是本板 `.text` 起始 |
| `.text`（含 `.vectors`/`.irqs`） | VMA `0x6000_2000`，大小 `0x44AC`（17,580 B） |
| 向量表 | `BASE_VECTORS @ 0x6000_2000`，`IRQS @ 0x6000_2040`；word0（初始 SP）= `0x2020_4000`，word1（复位向量）= `0x6000_22c1` → `initialize_ram_jump_to_main` |
| 复位路径 | `initialize_ram_jump_to_main` @ `0x6000_22c0`：清 `.sram`（`0x2020_4000..0x2020_4ea0`）→ 拷 `.relocate`（本板为空，size 0）→ `bl main`（`main @ 0x6000_3404`） |
| `.ARM.exidx` | `0x6000_64ac`，16 B |
| `.storage` | `0x6000_64bc`，`0x144`（324 B，含 deferred-call 的存储卷） |
| `_etext` | `0x6000_6600`（`.relocate` 的 flash 源位置） |
| `.attributes`（TOCK TLV，ROM 末尾） | LMA `0x6003_ffd4`，44 B；`.bin` 末尾可见 `TOCK` 哨兵 |
| ELF Entry | `0x6000_22c1`（= 向量表 word1；lld 取了第一个 `ENTRY(initialize_ram_jump_to_main)`，与硬件实际入口一致） |
| `.stack` | `0x2020_0000`，16,384 B（`stack_size!{0x4000}`，Tock 把栈放在 RAM 最低地址） |
| `.sram`（.bss/COMMON） | `0x2020_4000`，3,744 B（`_szero.._ezero = 0x2020_4000..0x2020_4ea0`） |
| RAM 范围 | `0x2020_0000..0x2022_0000`（128 KiB OCRAM）；`_eappmem = 0x2022_0000` |
| `.apps`（prog） | `0x6004_0000`，4 B 占位（`_sapps=0x6004_0000`、`_eapps=0x6024_0000`），本板不加载 app |
| 真实 ROM 内容 | `.boot_hdr` 8,192 + `.text` 17,580 + `.exidx` 16 + `.storage` 324 + `.attributes` 44 = **26,156 B（约 25.5 KiB）**；rom 区 0x3E000，绰绰有余 |
| `.bin`（`llvm-objcopy -O binary`） | **262,144 B（256 KiB）** —— 因 `.attributes` 放在 rom 末尾（0x6003_ffd4），objcopy 会把 0x6000_6600..0x6003_ffd4 填 0 补齐；`.bin` 前 4 字节同样是 `FCFB` |

## 4. RX 回显设计（2026-10-27 真机反馈后的第二轮修改）

第一版在 `received_buffer` 回调里**先同步逐字节输出、最后才重挂接收**。真机上发送
22 B 只回显了 3 B。原因：逐字节输出一行约 15 B × 87 µs ≈ 1.3 ms，期间驱动关掉了
RX 中断、又没有挂接收缓冲 → 硬件 `RDRF`/`OR` 溢出，后面的字节全丢，且驱动
`clear_overrun()` 清错寄存器导致 `STAT.OR` 粘滞。

现在的实现（`src/main.rs`）：

1. **双缓冲、回调第一件事重挂**：两条 1 B 静态缓冲 A/B 轮换。`received_buffer`
   的第一条动作是从 `spare_rx` 取出另一条立刻 `receive_buffer(spare, 1)`，
   **在此之前不做任何拷贝/输出**；重挂成功后，本条缓冲才归回调处理，处理完再
   放回 `spare_rx`。所以驱动任何时候都有一条可用接收缓冲。
2. **回调只做 O(1) 搬运**：字节存入 64 B 静态行缓冲（`line/line_len`）；遇到
   `\n` 把当前行快照到 `tx_line/tx_len` 并置位延迟调用；超长行截断并打标记。
   回调里**没有任何串口输出**。
3. **输出走 `DeferredCall`**：`handle_deferred_call` 在内核 loop 的任务上下文里
   （不在中断回调里）用阻塞 `send_byte` 输出整行：
   `\r\ntock: rx line(23B): TESSERA-TOCK-ECHO-TEST\r\n`；
   非可见字符用 `.` 代替；若曾发生 overrun/重挂失败，行尾追加 ` rxerr=N`。
4. **驱动修 overrun 粘滞**：patch 0002 把 `clear_overrun()` 改成写 1 清 `STAT.OR`
   （原来清的是 `CTRL.ORIE`），避免一次溢出之后每次中断都多走一次 OR 分支。
5. 启动横幅两行**保持原样**（Lead 验收脚本的判据）：
   `tock: tessera-rt1021 kernel boot` / `tock: kernel_loop running (echo mode)`。

**已知残余风险**：延迟输出整行（约 40 B ≈ 3.5 ms）期间，内核在跑 `send_byte`，
不会处理新的 RX 中断；若上位机在收到回显前**连续流水线发多行**，中间仍可能丢字节
（硬件只有 1 B RX 缓冲，驱动是每字节回调模型）。单行"发一行→等回显"的场景已足够
稳健。要彻底解决需要 Tock HIL 层做 RX 环形队列/中断驱动 TX，属于下一阶段工作。

## 5. 踩坑与说明

### 5.1 上层 Tessera 工作区的 cargo 配置会漏进本源码树

本板源码树在 `/home/reginald/project/tessera` 之下，cargo 会合并
`/home/reginald/project/tessera/.cargo/config.toml`。该文件给
`[target.thumbv7em-none-eabihf]` 设了 `-C link-arg=-Timxrt-link.x`
（RTIC 固件用、由 imxrt-rt 的 build.rs 生成）。两个后果：

1. **rustflags 被覆盖**：cargo 规则是 `target.<triple>.rustflags` 优先于
   `[build].rustflags`，本板原来的 `[build] rustflags`（含 Tock build script 要求的
   `cfg_tock_buildflagssentinel`）被整体忽略，build script 直接 panic：
   `Incorrect build configuration`。
   处置：本板 `.cargo/config.toml` 改成 `[target.thumbv7em-none-eabihf] rustflags`。
2. **多出一条 `-Timxrt-link.x`**：cargo 对 rustflags 数组是**跨配置文件合并**的，
   无法在本板 config 里删除上层那一项，链接时 rust-lld 报
   `cannot find linker script imxrt-link.x`。
   处置：在板目录放一个**空的合法链接脚本** `imxrt-link.x`（板目录本来就在链接
   `-L` 搜索路径里），真正布局仍全部由 `layout.ld` 决定。
   **上层 tessera 仓库没有任何文件被修改。**

### 5.2 与 EVKB 参考板的差异（全部集中在本 crate）

- 控制台引脚：`GPIO_AD_B0_06`（TX）/ `GPIO_AD_B0_07`（RX），IOMUXC ALT2。
  注意 Tock 的 iomuxc API 里 EVKB 的 12/13 号脚写作 `13/14`，本板对应写作 `6/7`。
- 控制台时钟：`UartClockSelection::Oscillator` + podf=1（24 MHz 晶振），不依赖 PLL3。
- 波特率分频：见 §2 的 3 处 `TESSERA-PATCH`。
- 无 capsule、无 LED、无 `PANIC_RESOURCES` / `ProcessPrinter`（panic 只打串口）。
- 调度器 `NoSched`：`next() = TrySleep`，`result()` 空实现；`SchedulerTimer` 用
  `cortexm7::systick::SysTick::new_with_calibration(396_000_000)`。

## 6. 如何复制进 Tessera 仓库

前提：Tessera 仓库本身**不**包含 Tock 源码树；本验证的 Tock 树是
`tmp/tock-release-2.3.0/`（gitignored）。要把它固化进仓库 / CI，按下面做。

### 6.1 需要复制的产物

从本目录复制到**目标 Tock 源码树的 `boards/` 下**（保持 `boards/tessera-rt1021/` 这一层）：

```
boards/tessera-rt1021/
├── Cargo.toml
├── .cargo/config.toml
├── layout.ld
├── chip_layout.ld
├── imxrt-link.x          # 见 §6.2 第 2 条（Tock 树放在 Tessera 仓库下时才需要）
├── boot_hdr.bin          # 来源见 §7
├── README.md             # 本文
├── src/
│   ├── main.rs
│   ├── io.rs
│   └── boot_header.rs
└── tock-patches/
    ├── 0001-imxrt10xx-lpuart-rt1021-baud.patch
    └── 0002-imxrt10xx-lpuart-clear-overrun.patch
```

并在目标 Tock 树根目录应用两个 patch（顺序 0001 → 0002，两个都基于干净上游：

```bash
cd <tock-tree>
patch -p1 --dry-run < boards/tessera-rt1021/tock-patches/0001-imxrt10xx-lpuart-rt1021-baud.patch
patch -p1 <         boards/tessera-rt1021/tock-patches/0001-imxrt10xx-lpuart-rt1021-baud.patch
patch -p1 <         boards/tessera-rt1021/tock-patches/0002-imxrt10xx-lpuart-clear-overrun.patch
```

本目录的 patch 已用 `patch -p1 --dry-run` 在"原始上游 `lpuart.rs`"的干净副本上验证：
`0001`、`0002` 单独 dry-run 均 `exit=0`（无 fuzz、无 offset）；顺序应用后与
本树当前 `chips/imxrt10xx/src/lpuart.rs` **逐字节一致**。
`0001` 单独运行时两个 hunk 分别在 `@@ -433` / `@@ -828`，`0002` 在 `@@ -478`。

### 6.2 除 patch 外对 Tock 树的 2 处构建绕行

1. **workspace member**：在 Tock 树根 `Cargo.toml` 的 `members` 里加一行
   `"boards/tessera-rt1021",`（本次已在 `tmp/tock-release-2.3.0/Cargo.toml`
   加过，与 EVKB 相邻）。否则 `cargo build` 会报 "package is not a member of
   the workspace"。
2. **`imxrt-link.x` 空占位 + rustflags 层级**：仅当 Tock 源码树位于
   `/home/reginald/project/tessera` **目录之下**（本验证就是这种情况）才需要：
   上层 `tessera/.cargo/config.toml` 的 target rustflags 会被合并进来
   （`-Timxrt-link.x`），因此板目录必须保留空 `imxrt-link.x` 供链接器解析，
   且板级 `.cargo/config.toml` 的 rustflags 必须写在
   `[target.thumbv7em-none-eabihf]` 下（写在 `[build]` 下会被整体覆盖）。
   若把 Tock 树放到 Tessera 目录之外（如 `/opt/tock`），这两项都**不需要**：
   可删掉 `imxrt-link.x`，rustflags 放回 `[build]` 即可。

### 6.3 复制后的验证

```bash
cd <tock-tree>/boards/tessera-rt1021
RUSTUP_TOOLCHAIN=stable cargo build --release
# 期望：Finished `release` profile，产出 <tock-tree>/target/thumbv7em-none-eabihf/release/tessera-rt1021
```

## 7. `boot_hdr.bin` 的来源（可复现）

`boot_hdr.bin` = **现有生产固件**（Tessera 0.1.1.0，RTIC）链接产物 `firmware` 的
原始二进制映像的**前 8192 字节**（FCB + IVT + boot data 所在的 8 KiB 启动头）。
生成命令（本机已实测 `cmp` 与当前 `boot_hdr.bin` 逐字节一致）：

```bash
# 在 Tessera 仓库根目录（VERSION = 0.1.1.0）
cargo build --release
ELF=target/thumbv7em-none-eabihf/release/firmware
LLVM_BIN="$(RUSTUP_TOOLCHAIN=stable rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin"
"$LLVM_BIN/llvm-objcopy" -O binary "$ELF" /tmp/tessera-fw-0.1.1.0.bin
head -c 8192 /tmp/tessera-fw-0.1.1.0.bin > boards/tessera-rt1021/boot_hdr.bin
# 自检：前 4 字节应为 FCFB，且大小恰好 8192
od -A x -t x1z -N 4 boards/tessera-rt1021/boot_hdr.bin
stat -c '%s' boards/tessera-rt1021/boot_hdr.bin
```

说明：`firmware` ELF 本身约 2.85 MB（含 debug），`llvm-objcopy -O binary` 后是
184,760 B（0.1.1.0），取前 8 KiB 即得该头。文件内容**不要手改**：它是 Boot ROM
读取的 FCB + IVT，写坏 = 只能靠 BOOT_MODE 引脚救砖。

## 8. 还没确定 / 需要在真机上确认的点

1. **24 MHz 晶振是否已在运行**：本板只改 CCM 的 UART 时钟选择，没有去
   `CCM_ANALOG` 使能/等待晶振 ready。参照实现（`crates/board-support/src/console.rs`）
   同样只做选择，依赖 boot ROM 已经把晶振打开。真机第一轮已验证能出字，此项成立。
2. **IRQ 号**：`imxrt10xx::nvic::LPUART1 = 20` 取自 Tock 同族芯片 crate，未在本树内
   对照 RT1021 手册复核；真机中断回显已工作，号是对的。
3. **波特率**：`SBR=13 / OSR=16 / BOTHEDGE=0` 与现有固件的
   `SBR=26 / OSR=8 / BOTHEDGE=0` 数值等价（都是 115384.6 Bd），真机通信正常。
4. **SysTick 校准频率 396 MHz** 是假设值：Tock 本身不配置时钟树（除 UART 时钟选择），
   CPU/PLL 频率取决于 boot ROM / 启动头；本验证没有进程，timeslice 用不到。
5. **内存布局**：栈 16 KiB 放在 OCRAM 最低地址（Tock 布局惯例），`_estack=0x2020_4000`；
   与现有 Tessera 固件的（ITCM/DTCM/OCRAM）布局无关。未做栈涂抹测量，
   16 KiB 只是按 kernel 无 capsule 的粗估，不是实测。
6. **启动头里的 `boot_data.size`**：复用现有固件 8 KiB，其中 `boot_data.size = 0x2CA98`
   （= 旧固件大小，非本 Tock 镜像的 26,156 B）。IVT `entry=0x6000_2000` 正确、`csf=0`
   （未开 HAB 安全启动），XIP 启动只用 entry，真机启动成功证明不影响；但若有工具
   （如 NXP 下载器）校验 boot_data.size，这里会不一致 —— 按任务要求不改该 8 KiB。
7. **MPU**：没有配置任何 MPU 区域，也没有进程；`ChipHw::init()` 只做 NVIC 与向量表
   offset。若后续要加 capsule/进程，需要单独设计 MPU 区域表。
8. **看门狗**：未启用（`WatchDog = ()`），与现有固件 P5 缺口一致。
9. **RX 残余风险**：见 §4 末尾——整行阻塞输出期间无法处理新 RX，流水线式连续多行
   可能仍丢字节。单行往返场景已验证稳健。
