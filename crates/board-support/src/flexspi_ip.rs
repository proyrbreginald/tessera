//! FlexSPI 控制器寄存器直访 —— **照官方 `fsl_flexspi.c` 的时序翻译**。
//!
//! # 为什么不用 Boot ROM API
//!
//! ROM 路径只返回"成功/失败"，把 `INTR` 里的错误位吞掉了 ⇒
//! 出现过"`program` 返回成功、一个字节都没写"这种**没有出口**的故障（排查了几天）。
//! 官方的 `FLEXSPI_TransferBlocking` 每一步之后都会
//! `FLEXSPI_CheckAndClearError(base, base->INTR)` —— **错误必须可见**。
//!
//! # 常量从哪来（不猜）
//!
//! - 基址 `0x402A_8000`：CMSIS `MIMXRT1021.h:17694` 的 `FLEXSPI_BASE`
//! - 寄存器偏移与位域：`nor_flash::flexspi`（逐条注明出处）
//! - 时序：`fsl_flexspi.c` 的 `FLEXSPI_TransferBlocking`（9 步）
//!
//! # 本模块当前**只做读**
//!
//! 写路径（擦除/编程）依赖同一个 `ip_command`，但会先只验证读：
//! 读是**零风险**的（不改器件状态、不改 LUT），却能证明整条链是通的。

use nor_flash::flexspi::{Command, Intr, offset};

/// FlexSPI1 寄存器基址（CMSIS `MIMXRT1021.h:17694`）。
const FLEXSPI_BASE: usize = 0x402A_8000;

/// 本板用的 FlexSPI 端口（CS0 ⇒ A1）。
const PORT: usize = 0;

/// 读一个寄存器。
fn read(register_offset: usize) -> u32 {
    let address = FLEXSPI_BASE + register_offset;
    // SAFETY: `FLEXSPI_BASE + offset` 是 FlexSPI1 的寄存器区（CMSIS 头定义的固定地址），
    // 偏移取自 `nor_flash::flexspi::offset`（照抄 CMSIS 的字段注释）。
    // `read_volatile` 保证这次读不被优化掉（寄存器值可随硬件变化）。
    unsafe { core::ptr::read_volatile(core::ptr::with_exposed_provenance::<u32>(address)) }
}

/// 写一个寄存器。
fn write(register_offset: usize, value: u32) {
    let address = FLEXSPI_BASE + register_offset;
    // SAFETY: 同上（地址是固定的寄存器区，偏移照抄 CMSIS）。
    // 写这些寄存器是**有意**的副作用：它们就是控制器的控制面。
    unsafe {
        core::ptr::write_volatile(
            core::ptr::with_exposed_provenance_mut::<u32>(address),
            value,
        );
    }
}

/// 读一个寄存器（供诊断打印：终端/自证可以如实报出控制器状态）。
#[must_use]
pub fn peek(register_offset: usize) -> u32 {
    read(register_offset)
}

/// 执行一次 IP 命令（照官方 `FLEXSPI_TransferBlocking` 的 9 步）。
///
/// 只在 `Command::Read` 时把 FIFO 数据搬到 `out`（写路径下一步再做）。
///
/// # Errors
///
/// 命令未被授予或出错时返回 `Err(Intr)` 供调用方如实上报
/// （**不折叠成 bool** —— 错误的原因必须能传到人眼前）。
pub fn ip_command(
    sequence: u8,
    address: u32,
    data_size: u32,
    command: Command,
    out: &mut [u8],
) -> Result<Intr, Intr> {
    // ① 清序列指针（官方：`FLSHCR2[port] |= CLRINSTRPTR`）
    let flshcr2 = offset::FLSHCR2 + PORT * 4;
    write(
        flshcr2,
        read(flshcr2) | nor_flash::flexspi::bits::FLSHCR2_CLRINSTRPTR,
    );

    // ② 清挂起的中断标志（官方：写 1 清除 `AHBCMDERR|IPCMDERR|AHBCMDGE|IPCMDGE`）
    const CLEAR_MASK: u32 = 0x0000_0001 | 0x0000_000A | 0x0000_0002; // IPCMDERR(0x8)|IPCMDGE(0x2) 与其 AHB 对应位
    write(offset::INTR, CLEAR_MASK);

    // ③ 命令地址
    write(offset::IPCR0, address);

    // ④ 清 FIFO —— **用"读改写"而不是整字覆盖**：官方是
    //    `base->IPRXFCR |= CLRIPRXF`（`fsl_flexspi.c:1110` 一带），
    //    整字写会顺手把 `RXWMRK`/`TXWMRK` 水位清掉（水位由 `FLEXSPI_Init` 先建好）。
    let iprxfcr = offset::IPRXFCR;
    write(
        iprxfcr,
        read(iprxfcr) | nor_flash::flexspi::bits::IPRXFCR_CLRIPRXF,
    );
    let iptxfcr = offset::IPTXFCR;
    write(
        iptxfcr,
        read(iptxfcr) | nor_flash::flexspi::bits::IPTXFCR_CLRIPTXF,
    );

    // ⑤ 打包 IPCR1（数据量 + 序列号 + 条数）
    let ipcr1 = match nor_flash::flexspi::pack_ipcr1(data_size, sequence, 1) {
        Some(value) => value,
        None => return Err(Intr::decode(0)),
    };
    write(offset::IPCR1, ipcr1);

    // ⑥ 触发
    write(offset::IPCMD, nor_flash::flexspi::bits::IPCMD_TRG);

    // ⑦ 读路径：**先确认 RX FIFO 里真有数据，再搬**。
    //
    // 官方 `FLEXSPI_ReadBlocking`（`fsl_flexspi.c:949`）对小数据量走的是
    // "等 `IPRXFSTS.FILL`" 分支，判据原文：
    // ```c
    // while (size > ((((base->IPRXFSTS) & FILL_MASK) >> SHIFT) * 8U)) { ... }
    // ```
    // ⇒ **`IPRXFSTS.FILL` 的一个单位是 8 字节**（不是 4，也不是 1）。
    //
    // ⚠️ 这一步以前**完全缺失**：触发之后直接从 `RFDR[0]` 连着读 `ceil(size/4)` 个字。
    // 只要 RX 侧还没准备好，读到的就是**残留或 0**，而 `INTR` 仍然是
    // `done && !err` ⇒ **"命令成功"与"读到真值"是两件事**。
    // 状态读（`0x05`）正好走这条路径，所以它此前的读数并不可信。
    if command == Command::Read {
        let need_units = data_size.div_ceil(8);
        let mut waited = 0u32;
        while (read(offset::IPRXFSTS) & nor_flash::flexspi::bits::IPRXFSTS_FILL_MASK) < need_units {
            waited += 1;
            if waited > FILL_WAIT_LIMIT {
                return Err(Intr::decode(read(offset::INTR)));
            }
        }
        let words = usize::try_from(data_size).unwrap_or(0).div_ceil(4);
        for index in 0..words {
            let word = read(offset::RFDR + index * 4);
            let bytes = word.to_le_bytes();
            for (byte_index, byte) in bytes.iter().enumerate() {
                let at = index * 4 + byte_index;
                if let Some(slot) = out.get_mut(at) {
                    *slot = *byte;
                }
            }
        }
    }

    // ⑧ 等 `IPCMDDONE`（官方 `TransferBlocking` 是死等；这里加界，卡死比报错糟糕）
    let mut waited = 0u32;
    while read(offset::INTR) & nor_flash::flexspi::bits::INTR_IPCMDDONE == 0 {
        waited += 1;
        if waited > BUS_IDLE_LIMIT {
            return Err(Intr::decode(read(offset::INTR)));
        }
    }
    // ⑨ 等总线空闲
    while !bus_idle() {}

    // ⑩ 读 `INTR` 并解码 —— **错误可见性的落点**
    let intr = Intr::decode(read(offset::INTR));
    // 清掉刚刚看到的标志，避免污染下一次判断
    write(offset::INTR, CLEAR_MASK);
    if intr.is_success() {
        Ok(intr)
    } else {
        Err(intr)
    }
}

/// **把序列装进控制器的 LUT**（照官方 `FLEXSPI_UpdateLUT` 的流程）。
///
/// # 为什么必须自己装（而不是指望 FCB 里带着）
///
/// 官方示例允许从 FCB 拷配置（宏 `FLASH_USE_CUSTOM_FCB`），但那要求 **FCB 里真的有可用 LUT**；
/// 我们产物里那段是填充值 ⇒ 控制器里现在这套 LUT 是 **Boot ROM 自己装的**，
/// 它够用来取指/读（所以能启动、XIP 能跑），但**不足以做擦写**。
/// 这与"能启动、能读、不能写"的现象完全吻合。
///
/// # 流程（逐句对照 `drivers/flexspi/fsl_flexspi.c` 的 `FLEXSPI_UpdateLUT`）
///
/// ① 等总线空闲；② `LUTKEY = 0x5AF0_5AF0`（官方 `FLEXSPI_LUT_KEY_VAL`，`fsl_flexspi.c:22`）；
/// ③ `LUTCR = 0x02` 解锁；④ 写 `LUT[index + i]`；⑤ `LUTCR = 0x01` 上锁。
///
/// 返回是否全部写入（定位越界时返回 `false`，绝不静默跳过）。
pub fn install_lut() -> bool {
    /// LUT 解锁键（官方 `FLEXSPI_LUT_KEY_VAL`）。
    const LUT_KEY: u32 = 0x5AF0_5AF0;
    /// `LUTCR.UNLOCK`（bit1）。
    const LUTCR_UNLOCK: u32 = 0x02;
    /// `LUTCR.LOCK`（bit0）。
    const LUTCR_LOCK: u32 = 0x01;

    use nor_flash::flexspi::{lut_word, lut_words, seq};

    // 官方 `FLEXSPI_Init` 在这里之前就把 IP FIFO 水位设好了（`fsl_flexspi.c:405-412`），
    // 我们此前跳过了这一步，代价见 `set_tx_watermark_full` 的真机读数。
    set_tx_watermark_full();

    while !bus_idle() {}
    write(offset::LUTKEY, LUT_KEY);
    write(offset::LUTCR, LUTCR_UNLOCK);

    let table: [(u8, &[u32]); 5] = [
        (seq::READ, &lut_words::READ),
        (seq::READ_STATUS, &lut_words::READ_STATUS),
        (seq::WRITE_ENABLE, &lut_words::WRITE_ENABLE),
        (seq::ERASE_SECTOR, &lut_words::ERASE_SECTOR),
        (seq::PAGE_PROGRAM, &lut_words::PAGE_PROGRAM),
    ];
    let mut ok = true;
    for (sequence, words) in table {
        let Some(base) = lut_word(sequence) else {
            ok = false;
            continue;
        };
        // ⚠️ 整条 4 字槽都要写：官方约定「**零字即 STOP**」（opcode 0 = STOP），
        // 所以 SDK 的 LUT 表里只写 word0、其余靠默认 0 终止。
        // 而控制器里那几字**不是 0**（实测回读到 `0xe21e2088` 这类残留 ✗），
        // 于是没有显式 STOP 的序列会**跑进垃圾** ⇒ 表现为 `INTR.IPCMDERR`。
        // 因此这里把整槽写满，剩余字显式清零 —— 与官方表的最终状态等价。
        for index in 0..nor_flash::flexspi::offset::LUT_WORDS_PER_SEQUENCE {
            let word = words.get(index).copied().unwrap_or(0);
            write(offset::LUT + (base + index) * 4, word);
        }
    }

    write(offset::LUTCR, LUTCR_LOCK);
    ok
}

/// 按官方算式设置 **IP TX FIFO 水位**（`TXWMRK`）。
///
/// # 官方算式（`fsl_flexspi.c:405-412`，`FLEXSPI_Init` 内）
///
/// ```c
/// base->IPTXFCR &= ~FLEXSPI_IPTXFCR_TXWMRK_MASK;
/// base->IPTXFCR |= FLEXSPI_IPTXFCR_TXWMRK((uint32_t)config->txWatermark / 8U - 1U);
/// ```
/// 默认 `config->txWatermark = 8`（`fsl_flexspi.c:445`）。
///
/// # 但真机给出了一个**必须偏离默认**的读数（2026-10-13，四组对照）
///
/// ```text
/// FIFO 对照 w0=0 w0c=1 w15=0 w15c=16
/// ```
/// （`w<水位>c` 表示"写完清 `IPTXWE`"。）两条结论：
///
/// 1. **`TFDR` 的写要"写完清 `IPTXWE`"才被推进 FIFO** —— `w0=0`/`w15=0` 是 0，
///    清过标志的 `w0c=1`/`w15c=16` 才是真值；
/// 2. **能推进多少字由 `TXWMRK` 决定**：`TXWMRK=0`（官方默认，水位 8 **字节** = 1 字）
///    只进 **1 个字**；`TXWMRK=15`（水位 8 **字**）进满 **16 个字**。
///
/// ⇒ 官方那个 `txWatermark/8-1 = 0` 在**本板的 `IPTXFCR` 复位值**下只买到 1 个字的缓冲，
/// 一页 256 B 就得"1 字 1 字地喂"，于是真机 `INTR.SCKSTOPBYWR`
///（写相位因 TX FIFO 空而停摆）。取满容量（16 字 = 64 B）既符合官方 `IPTXFCR.TXWMRK`
/// 字段定义（"Watermark level is (TXWMRK+1)*64 Bits" ⇒ 16 字），也让一页只需 4 轮补数。
pub fn set_tx_watermark_full() {
    use nor_flash::flexspi::bits;
    let iptxfcr = offset::IPTXFCR;
    write(
        iptxfcr,
        (read(iptxfcr) & !bits::IPTXFCR_TXWMRK_MASK) | bits::IPTXFCR_TXWMRK_MAX,
    );
}

/// 把 `MCR0.ATDFEN` / `ARDFEN` 置位（**AHB 总线访问 IP FIFO 的使能**）。
///
/// 官方 `FLEXSPI_GetDefaultConfig` 默认两者都是 0（`fsl_flexspi.c:426+` 的
/// `enableAHBWriteIpTxFifo` / `enableAHBWriteIpRxFifo`），我们实测也是 0。
/// 本函数只为**对照实验**服务：打开它们再跑一次自证，就能把
/// "`TFDR` 的写是否需要这两位"变成二值判据，而不是留在推理里。
pub fn enable_ahb_fifo_access() {
    let mcr0 = read(offset::MCR0) | 0x80 | 0x40;
    write(offset::MCR0, mcr0);
}

/// 执行一次**写**方向的数据传输（页编程用）。
///
/// # 顺序：**先预灌 TX FIFO，再触发命令**（官方原文，不是我的理解）
///
/// 官方 `FLEXSPI_TransferBlocking`（`drivers/flexspi/fsl_flexspi.c:1056`）对写方向的步骤列表
/// 写在函数体的注释里，逐条如下：
///
/// ```text
/// 1. Fill IP transmit FIFO with programming data (can be done before or after configuring registers)
/// 2. Write flash memory access start address to IPCR0[SFAR]
/// 3. Write read or program data size to IPCR1[IDATSZ], sequence index to IPCR1[ISEQID], ...
/// 4. Write 1 to IPCMD[TRG] to trigger flash memory access command
/// 5. Wait for INTR[IPCMDDONE] flag
/// ```
///
/// 而 `FLEXSPI_WriteBlocking`（同文件 `:817`）的注释把理由写得更直白：
///
/// ```text
/// /* Phase 1: Pre-fill TX FIFO before triggering command.
///  * Write directly without waiting for watermark flag (FIFO was just cleared by caller).
///  * Stop when FIFO is full or all data has been written, whichever comes first.
///  * This ensures CMD is never triggered with an empty FIFO, preventing SCLK stalls. */
/// ...
/// /* Trigger IP command: FIFO is now pre-filled. */
/// base->IPCMD |= FLEXSPI_IPCMD_TRG_MASK;
/// /* Phase 2: Stream remaining data using watermark mechanism. */
/// ```
///
/// ⚠️ **我此前把顺序做反了**：先 `IPCMD |= TRG`、再灌 FIFO。观测到的现象正是它的后果 ——
/// 命令在**空 FIFO** 上被触发 ⇒ 出向数据相位立刻结束（`IPCMDMONE` 立即置位）、
/// 器件一个字节都没收到 ⇒ 控制器报 `IPCMDDONE` 无错误、而读回全 `0xFF`。
/// 这条**不是猜的**：官方注释明写 "preventing SCLK stalls"，
/// 而本项目此前实测到过 `INTR.SCKSTOPBYWR`（写相位 SCLK 停摆）—— 同一个根因。
///
/// # Errors
///
/// 同 [`ip_command`]。
pub fn ip_write(sequence: u8, address: u32, data: &[u8]) -> Result<(Intr, usize), Intr> {
    use nor_flash::flexspi::{bits, offset, tx_fifo_words};

    // ⚠️ 本函数的形状**逐句照抄 ROM 的真实实现**
    //（`middleware/mcu_bootloader/src/drivers/flexspi/bl_flexspi.c:1100-1155`，
    //  即烧进这颗芯片 ROM 的那份驱动）。它与 SDK 的 `FLEXSPI_WriteBlocking` 有**实质差别**，
    //  而那些差别正是此前反复失败的地方：
    //
    // | 项 | SDK `WriteBlocking` | **ROM 实现（本函数照抄）** |
    // |---|---|---|
    // | 水位 | 只读 `TXWMRK`，不设 | **整字写 `IPTXFCR = TXWMRK(watermark-1)`** |
    // | 突发大小 | `2*txWatermark` 字 | **`FIFO 容量 / 2`** 字 |
    // | 触发时机 | Phase 1 之后统一触发 | **首灌之后立刻触发**（`is_transfer_started`） |
    // | 写入前置判据 | 先写、最后清 `IPTXWE` | **每轮先查 `IPTXWE` 再灌** |
    // | 尾块 | 按剩余长度 | **按整突发写，不足补 0** |

    // ① 清序列指针
    let flshcr2 = offset::FLSHCR2 + PORT * 4;
    write(flshcr2, read(flshcr2) | bits::FLSHCR2_CLRINSTRPTR);
    // ② 清挂起标志
    write(offset::INTR, CLEAR_MASK);
    // ③ 命令地址
    write(offset::IPCR0, address);
    // ④ 数据量与序列号
    let size = u32::try_from(data.len()).unwrap_or(0);
    let _ = size;
    // ⑤ **整字设水位**：ROM 的算式是
    //    `watermark = tx_fifo_size / 8 / 2`（16 字 ⇒ 2），再写 `TXWMRK(watermark - 1)`。
    //    ⚠️ 传进宏的是**水位的单位数**，宏自己左移（`FLEXSPI_IPTXFCR_TXWMRK(x) = (x<<2)&0x3C`）；
    //    我先前直接把"字数"当字段值传，写出来的 `TXWMRK` 差了 4 倍。
    let fifo_words = tx_fifo_words();
    let half = fifo_words / 2;
    write(
        offset::IPTXFCR,
        nor_flash::flexspi::bits::txfcr_txwmrk(u32::try_from(half.saturating_sub(1)).unwrap_or(0)),
    );
    // ⑥ 清 FIFO
    let iptxfcr = offset::IPTXFCR;
    write(iptxfcr, read(iptxfcr) | bits::IPTXFCR_CLRIPTXF);

    // ⑦ 数据相位：**一个 IP 命令只声明"这一轮真正入队的数据量"**。
    //
    // # 为什么这里偏离了 ROM 的形状（并给出真机依据）
    //
    // ROM 的做法是"`IDATSZ` = 整页 256 B，然后边发边补"。它成立的前提是**预灌真的进了 FIFO**。
    // 真机把它否掉了：`note=0x0600c008` ⇒ `cursor=8`、`burst=8` —— 我每轮写了 8 个槽
    //（`TFDR[0..8]`），但**只有 1 个字真正被推进 FIFO**，而 `IDATSZ` 声明的是 256。
    // 控制器要 256 字节、FIFO 里只有 8（64 B 中的 8 个槽只落了 1-2 个），于是必然停摆
    //（`INTR` 只剩 `IPRXWA`，正是"等着数据却等不到"）。
    //
    // 而控制器**在触发命令之后**不再把 IP 总线的 `TFDR` 写入当作数据源（官方 Phase 2 靠
    // `INR.IPTXWE` + 一次水位量勉强成立）。因此正确的形状是：
    // **每个 IP 命令只声明并发送"一个 FIFO 能装下的块"**，块与块之间重新触发命令。
    // 这完全符合手册：`34h` 允许 1..256 字节数据（Rev Q §8.2.26 / Note 3），
    // 而页内的多次编程由调用方按页内偏移累加（`program_page_staged` 已经这么做）。
    let burst = half.max(1);
    let want = usize::try_from(size).unwrap_or(0);
    let burst_bytes = burst * 4;
    let take = core::cmp::min(want, burst_bytes);
    let Some(ipcr1) = nor_flash::flexspi::pack_ipcr1(u32::try_from(take).unwrap_or(0), sequence, 1)
    else {
        return Err(Intr::decode(0));
    };
    write(offset::IPCR1, ipcr1);
    let mut cursor = 0usize;
    // 在**触发之前**把这一块写满 FIFO（官方 Phase 1 的语义：先预灌、再触发）
    for slot in 0..burst {
        let (word, used) = if cursor < take {
            let end = core::cmp::min(cursor + 4, take);
            let chunk = data.get(cursor..end).unwrap_or(&[]);
            (word_of(chunk), chunk.len())
        } else {
            (0u32, 0usize)
        };
        if used > 0 {
            cursor += used;
        }
        write(offset::TFDR + slot * 4, word);
    }
    // 推数据进 FIFO（写 1 清 `IPTXWE`）——**真机四组对照已证实：写入要靠这一句才被推进**
    write(offset::INTR, bits::INTR_IPTXWE);
    // 触发
    write(offset::IPCMD, bits::IPCMD_TRG);

    // ⑧ 有界等 `IPCMDDONE`
    let mut waited = 0u32;
    while read(offset::INTR) & bits::INTR_IPCMDDONE == 0 {
        waited += 1;
        if waited > BUS_IDLE_LIMIT {
            let mut e = Intr::decode(read(offset::INTR));
            e.note = 0x0300_0000 | mk_note(data.len(), cursor, fifo_words);
            return Err(e);
        }
    }
    // ⑨ 有界等总线空闲
    waited = 0;
    while !bus_idle() {
        waited += 1;
        if waited > BUS_IDLE_LIMIT {
            let mut e = Intr::decode(read(offset::INTR));
            e.note = 0x0400_0000 | mk_note(data.len(), cursor, fifo_words);
            return Err(e);
        }
    }

    // ⑩ 检查最终状态（官方 `FLEXSPI_CheckAndClearError`）
    let mut intr = Intr::decode(read(offset::INTR));
    intr.note = 0x0500_0000 | mk_note(data.len(), cursor, fifo_words);
    write(offset::INTR, CLEAR_MASK);
    if intr.is_success() {
        Ok((intr, cursor))
    } else {
        Err(intr)
    }
}

/// 一次 IP 命令**最多能可靠送出的字节数**（= 半个 TX FIFO）。
///
/// # 为什么调用方必须知道它（真机代价换来的）
///
/// 控制器在一个 IP 命令里能推送的数据量受 FIFO 容量限制（本板 `TXWMRK` 取满后为 16 字 = 64 B；
/// 取一半 8 字 = 32 B 留安全余量）。我一度把 `IPCR1.IDATSZ` 裁到这个块长、却让调用方仍按
/// **整页 256 B** 传数据 ⇒ 每个命令只写前 32 B，后面全是 `0xFF`。真机判据：
/// `失配 @32 want=77954796b9d0ef8b got=77954796ffffffff` —— **前 32 字节完全正确、
/// 第 33 字节起是擦除态**，指针对得整整齐齐。
///
/// ⇒ 调用方按这个块长切分，并用 `ip_write` 返回的"实际写入字节数"推进页内偏移。
#[must_use]
pub fn ip_burst_bytes() -> usize {
    (nor_flash::flexspi::tx_fifo_words() / 2).max(1) * 4
}

/// 把"失败时的中间量"打成附注（`Intr::note`）：`[23:16]=len [15:8]=cursor [7:4]=fifo`。
fn mk_note(len: usize, cursor: usize, fifo: usize) -> u32 {
    ((u32::try_from(len & 0xFF).unwrap_or(0)) << 16)
        | ((u32::try_from(cursor & 0xFF).unwrap_or(0)) << 8)
        | u32::try_from(fifo & 0xF).unwrap_or(0)
}

/// 把 ≤4 字节按小端拼成一个 FIFO 字（不足 4 字节时高位补 0）。
fn word_of(chunk: &[u8]) -> u32 {
    let mut word = 0u32;
    for (index, byte) in chunk.iter().enumerate() {
        word |= u32::from(*byte) << (8 * u32::try_from(index).unwrap_or(0));
    }
    word
}

/// 等 FIFO 腾位置的上限。**短**：太长会踩看门狗（把板子送进复位循环）。
const FILL_WAIT_LIMIT: u32 = 400_000;
/// 等总线空闲的上限（同上）。
const BUS_IDLE_LIMIT: u32 = 4_000_000;

/// **FIFO 写入实验**：清 TX FIFO → 写 4 个字 → 回读 `IPTXFSTS.FILL` → 再清。
///
/// 这是"一刀切开"的判据：
/// - 返回值 **0** ⇒ 我的 `TFDR` 写入**根本没进 FIFO**（偏移或语义错）；
/// - 返回值 **4** ⇒ 写进去了 ⇒ 那问题在"控制器没把 FIFO 里的数据发出去"。
///
/// 纯寄存器操作，**不发任何命令**，零风险、与 LUT/序列无关。
#[must_use]
pub fn fill_probe() -> [u32; 6] {
    use nor_flash::flexspi::bits;
    let iptxfcr = offset::IPTXFCR;
    let iptxfsts = offset::IPTXFSTS;
    // 四种组合，每种都：设 TXWMRK → 清 FIFO → 写 8 个字 → 回读 FILL。
    // 目的只有一个：把"`TFDR` 写到底在什么条件下才落地"变成二值判据。
    let mut out = [0u32; 6];
    for (slot, wmrk, clear_we) in [
        (0usize, 0u32, false), // 官方默认：TXWMRK=0，不清 IPTXWE
        (1, 0, true),          // TXWMRK=0，写完清 IPTXWE（官方 Phase 1 的形状）
        (2, 15, false),        // TXWMRK=15，不清
        (3, 15, true),         // TXWMRK=15，清
    ] {
        write(
            iptxfcr,
            (read(iptxfcr) & !bits::IPTXFCR_TXWMRK_MASK) | (wmrk << bits::IPTXFCR_TXWMRK_SHIFT),
        );
        write(iptxfcr, read(iptxfcr) | bits::IPTXFCR_CLRIPTXF);
        for index in 0..8usize {
            write(offset::TFDR + index * 4, 0x5A5A_5A5A);
        }
        if clear_we {
            write(offset::INTR, bits::INTR_IPTXWE);
        }
        if let Some(target) = out.get_mut(slot) {
            *target = read(iptxfsts) & bits::IPTXFSTS_FILL_MASK;
        }
    }
    // 清干净，免得影响后续自证
    write(iptxfcr, read(iptxfcr) | bits::IPTXFCR_CLRIPTXF);
    out[4] = read(iptxfsts) & bits::IPTXFSTS_FILL_MASK;
    out[5] = read(offset::IPTXFCR);
    out
}

/// `IPTXFCR` / `MCR0` / `IPTXFSTS` 的**原始回读**（只读，不下任何命令）。
///
/// # 为什么单列一条
///
/// `fill_probe` 的结论是"`TFDR` 写进去一个字都没进 FIFO"（四个读数全 0）。
/// 那接下来必须先排除"**写根本没落到寄存器**"这一类可能，而不是直接怀疑器件：
///
/// - `IPTXFCR` 的**回读值**：`CLRIPTXF`(bit0) 是否自清？`TXWMRK`(bits5:2) 是多少？
///   若 `CLRIPTXF` 一直是 1，说明 FIFO 处于**持续清空**状态 ⇒ 写进去也会立刻被丢掉；
/// - `MCR0.MDIS`(bit1)：模块是否被关（关着时寄存器写可能不生效）；
/// - `IPTXFSTS.FILL`：再确认一次它可读（若它恒为 0，那"FILL=0"就毫无信息量 ✗）。
///
/// 这条探针的作用是把"是不是这两个位挡住了 `TFDR` 写入"变成可读事实。
#[must_use]
pub fn mcr0_fifo_bits() -> (u32, u32) {
    let mcr0 = read(offset::MCR0);
    (
        (mcr0 >> 7) & 1, // ATDFEN
        (mcr0 >> 6) & 1, // ARDFEN
    )
}

/// 回读某个端口的 **`FLSHCR0/1/2`**（**纯读**）。
///
/// 用途：与官方 `FLEXSPI_SetFlashConfig` 应设的值对照 ——
/// 该函数第一件事就是 `base->FLSHCR0[port] = config->flashSize;`，
/// 而 `FLEXSPI_Init` **结尾会把所有端口的 `FLSHCR0` 清零** ✗
/// ⇒ 若没人调 `SetFlashConfig`，控制器就认为 **flash 容量 = 0** ✗，
/// 而容量参与 IP 命令的地址范围判定 ✓。
#[must_use]
pub fn flash_cr(port: usize) -> (u32, u32, u32) {
    (
        read(offset::FLSHCR0 + port * 4),
        read(offset::FLSHCR1 + port * 4),
        read(offset::FLSHCR2 + port * 4),
    )
}

/// 控制器静态配置的**快照**（纯读）：`(MCR0, MCR1, MCR2, AHBCR, DLLCR0, FLSHCR4)`。
///
/// 用途：与官方 `FLEXSPI_GetDefaultConfig` + `FLEXSPI_Init` + `SetFlashConfig` 的期望**逐位对照**。
/// 重点看 `AHBCR` 的 `PREFETCHEN`/`BUFFERABLEEN`/`CACHABLEEN`（Boot ROM 为 XIP 会打开它们 ✗，
/// 而它们与"IP 数据相位拿不到 flash"这一假设直接相关）以及 `DLLCR`（采样时序 ✓）。
#[must_use]
pub fn ctrl_snapshot() -> [(&'static str, u32); 6] {
    [
        ("MCR0", read(offset::MCR0)),
        ("MCR1", read(offset::MCR1)),
        ("MCR2", read(offset::MCR2)),
        ("AHBCR", read(offset::AHBCR)),
        ("DLLCR0", read(offset::DLLCR)),
        ("FLSHCR4", read(offset::FLSHCR4)),
    ]
}

/// **`FLSHCR2` 里 `AWRSEQID`/`ARDSEQID` 的只读探针**（4 个端口各读一次）。
///
/// # 为什么单独看它（官方源码给出的一条对照）
///
/// 官方 `flexspi_config_flash_control_registers`（ROM 源码
/// `middleware/mcu_bootloader/src/drivers/flexspi/bl_flexspi.c:440`）会显式写：
///
/// ```c
/// temp  = FLEXSPI_FLSHCR2_ARDSEQID(CMD_LUT_SEQ_IDX_READ);        // AHB 读 ⇒ 序列 0
/// temp |= FLEXSPI_FLSHCR2_AWRSEQID(CMD_LUT_SEQ_IDX_WRITE);       // AHB 写 ⇒ 序列 9
/// base->FLSHCR2[index] = temp;
/// ```
///
/// 而**我们从没设过这两个字段**：控制器里现在是 Boot ROM 为 XIP 取指留下的值。
/// 于是"出向数据相位从未成真"有了一个**可验证**的形状 ——
/// 若 `AWRSEQID` 指向一条我们不认识的序列，写相位就会当场结束（`IPCMDMONE` 立刻置位）。
///
/// 返回每个端口的 `(原始值, ARDSEQID, AWRSEQID, AWRSEQNUM)`。**纯读**，零风险。
#[must_use]
pub fn flshcr2_awr_probe() -> [(u32, u32, u32, u32); 4] {
    use nor_flash::flexspi::{bits, offset};
    let mut out = [(0u32, 0u32, 0u32, 0u32); 4];
    for (index, slot) in out.iter_mut().enumerate() {
        let raw = read(offset::FLSHCR2 + index * 4);
        *slot = (
            raw,
            (raw & bits::FLSHCR2_ARDSEQID_MASK) >> bits::FLSHCR2_ARDSEQID_SHIFT,
            (raw & bits::FLSHCR2_AWRSEQID_MASK) >> bits::FLSHCR2_AWRSEQID_SHIFT,
            (raw & bits::FLSHCR2_AWRSEQNUM_MASK) >> bits::FLSHCR2_AWRSEQNUM_SHIFT,
        );
    }
    out
}

/// 立刻读一次器件状态寄存器（SR1）。**只读**，用于把"写使能到底有没有建立"变成事实。
///
/// 判据：bit0 = `WIP`（忙）、bit1 = `WEL`（写使能锁存）。
///
/// # Errors
///
/// 命令失败时返回 `Err(Intr)`。
pub fn status_now() -> Result<u8, Intr> {
    let mut status = [0u8; 1];
    ip_command(
        nor_flash::flexspi::seq::READ_STATUS,
        0,
        1,
        Command::Read,
        &mut status,
    )?;
    Ok(status[0])
}

/// 轮询器件状态寄存器，直到 **WIP（bit0）清零**；超时返回 `false`。
///
/// NOR 的擦除/编程是**异步**的：命令下发后器件还在忙，必须轮询状态位。
/// 上限取一个保守的迭代次数（**不是时间上限** —— 真机实测擦 64 KiB 约 557 ms，
/// 因此这个值要给得足够宽；具体数字应由实测确定，而不是拍脑袋）。
#[must_use]
pub fn wait_not_busy(max_polls: u32) -> bool {
    for _ in 0..max_polls {
        let mut status = [0u8; 1];
        match ip_command(
            nor_flash::flexspi::seq::READ_STATUS,
            0,
            1,
            Command::Read,
            &mut status,
        ) {
            Ok(_) => {}
            Err(intr) => {
                // ⚠️ `IPCMDGE`（**授予**错误）在编程/擦除刚发出之后是**瞬时**的：
                // 器件内部还在忙，而我们的固件**从 XIP 执行**（AHB 引擎持续占用同一片 flash），
                // 控制器会**拒绝**紧随其后的 IP 命令。真机实测：页编程成功之后那条读状态
                // 返回 `IPCMDGE|IPCMDERR`（stage 14 带出来的 `intr=0x0b`）。
                // ⇒ 这类错误**重试**（有界循环），而不是当场判死。
                if intr.grant_error {
                    continue;
                }
                return false;
            }
        }
        if status[0] & 0x01 == 0 {
            return true;
        }
    }
    false
}

/// 发一次**写使能**（`0x06`）—— 擦除与编程之前都必须有它。
///
/// 写使能是**驱动侧的责任**，不是 LUT 的一部分：LUT 只描述"命令怎么发"。
///
/// # Errors
///
/// 命令失败时返回 `Err(Intr)`。
pub fn write_enable() -> Result<Intr, Intr> {
    ip_command(
        nor_flash::flexspi::seq::WRITE_ENABLE,
        0,
        0,
        Command::CommandOnly,
        &mut [],
    )
}

/// 擦除一个 4 KiB 扇区（**调用方必须先过写入闸门**）。
///
/// # Errors
///
/// 写使能失败、命令失败、或器件一直忙（超时）时返回 `Err`。
pub fn erase_sector(address: u32) -> Result<(), Intr> {
    erase_sector_staged(address).map_err(|_stage| Intr::decode(0))
}

/// 与 [`erase_sector`] 相同，但**把失败落在哪一步报出来**（诊断用）。
///
/// 三处都可能失败，而它们的原因完全不同：
/// 1 = 写使能没成功（LUT 的序列 3 或 IP 命令本身）；
/// 2 = 擦除命令被拒（`INTR` 会说明是授予错误还是命令错误）；
/// 3 = 命令收下了但器件一直忙（超时 —— 轮询上限或状态位判据的问题）。
///
/// # Errors
///
/// 返回失败的那一步（1/2/3）。
pub fn erase_sector_staged(address: u32) -> Result<(), u8> {
    write_enable().map_err(|_| 1u8)?;
    ip_command(
        nor_flash::flexspi::seq::ERASE_SECTOR,
        address,
        0,
        Command::CommandOnly,
        &mut [],
    )
    .map_err(|_| 2u8)?;
    if wait_not_busy(MAX_POLLS_ERASE) {
        // 器件内容变了 ⇒ **两层**都要失效（真机定位见 `crate::cache` 的模块文档）：
        //   ① 内核 L1 D-Cache（本行）；② 控制器侧 AHB RX 缓冲/预取（ROM 的 clear_cache，
        //      对应勘误 ERR011207 的那一半）。
        if let Ok(off) = usize::try_from(address) {
            crate::cache::invalidate_dcache_range(crate::store::XIP_BASE + off, 4096);
        }
        crate::rom_flash::clear_flash_cache();
        Ok(())
    } else {
        Err(3u8)
    }
}

/// 编程一页并**把失败落在哪一步报出来**（11=写使能 / 12=命令 / 13=等忙）。
///
/// # Errors
///
/// 返回失败的那一步。
pub fn program_page_staged(address: u32, data: &[u8]) -> Result<(), (u8, Intr)> {
    // ⚠️ **分块单位是"页"，不是"FIFO 容量"**（这里此前做错了）。
    //
    // 页编程的数据相位长度由 `IPCR1.IDATSZ` 决定（官方 `TransferBlocking` 第 3 步），
    // 而 TX FIFO 只有 [`tx_fifo_words`] 个字 —— 控制器**边发边从 FIFO 取**，
    // 取空了就等我们补（官方 `WriteBlocking` 的 Phase 2 存在的全部理由）。
    // 所以"一次 IP 命令 = 一页"；按 FIFO 容量切块会让**第 2 块起写到页内错误的偏移**
    // （而且是在同一页里重复编程，NOR 上必然出错）。
    // 分块单位 = **一个 IP 命令能送出的字节数**（`ip_burst_bytes`），不是整页。
    // 每个块都重新写使能（`WEL` 每次编程后由器件自动清零）并重新触发 IP 命令。
    let burst = ip_burst_bytes().max(1);
    let page_bytes = usize::try_from(nor_flash::PAGE_BYTES).unwrap_or(256);
    let mut done = 0usize;
    while done < data.len() {
        // **每个分块都要写使能** —— `WEL` 在每次编程结束后由器件自动清零
        write_enable().map_err(|e| (11u8, e))?;

        // 编程前回读 `status` 断言 `WEL=1`
        let mut raw_status = [0u8; 1];
        ip_command(
            nor_flash::flexspi::seq::READ_STATUS,
            0,
            1,
            Command::Read,
            &mut raw_status,
        )
        .map_err(|e| (16u8, e))?;
        if raw_status[0] & 0x02 == 0 {
            return Err((17u8, Intr::decode(u32::from(raw_status[0]))));
        }

        // 本块长度：`burst` 与"页内剩余"取小（同一页内绝不跨页）
        let page_left = page_bytes - (done % page_bytes);
        let take = core::cmp::min(burst, core::cmp::min(page_left, data.len() - done));
        let end = done + take;
        let Some(chunk) = data.get(done..end) else {
            break;
        };
        let at = address + u32::try_from(done).unwrap_or(0);
        let (_, wrote) =
            ip_write(nor_flash::flexspi::seq::PAGE_PROGRAM, at, chunk).map_err(|e| (12u8, e))?;
        if wrote == 0 {
            return Err((12u8, Intr::decode(0)));
        }
        if !wait_not_busy(MAX_POLLS_PROGRAM) {
            return Err((13u8, Intr::decode(0)));
        }
        // **每个块之后只做廉价的 D-Cache 失效**（下一块的读-改-写要读到刚写进去的内容，
        // 否则会把陈旧内容拼回器件 —— 本项目踩过的"编程后读回是旧值"那一类）。
        //
        // ⚠️ **不要在这里调 `clear_flash_cache()`**：ROM 的 `clear_cache` 是
        // **FlexSPI 软件复位**，它会清掉控制器侧的 **AHB 预取缓冲** ——
        // 真机实测那次影响的量级是 **70 倍**（同一段 1 MiB 读：缓冲预热时 3.4 GB/s、
        // 缓冲被清后 48 MB/s ≈ 100 MHz × 4 bit 的物理上限）。
        // 一页 256 B 分 8 个块 ⇒ 每页复位 8 次，且代码若在 XIP 里执行，
        // 每次复位都会让取指停顿。⇒ 控制器复位**每次编程/擦除只做一次**（循环之后）。
        if let Ok(off) = usize::try_from(at) {
            crate::cache::invalidate_dcache_range(crate::store::XIP_BASE + off, take);
        }
        done = end;
    }
    // **整页（整段）完成后**才复位一次控制器，让控制器侧预取缓冲与器件一致
    //（对应勘误 ERR011207 的那一半；内层循环里刻意不做，理由见上面的注释）。
    crate::rom_flash::clear_flash_cache();
    Ok(())
}

/// 编程**一页**（≤256 B，且不得跨页；调用方保证）。
///
/// # Errors
///
/// 同 [`erase_sector`]。
pub fn program_page(address: u32, data: &[u8]) -> Result<(), Intr> {
    // ⚠️ **与 [`program_page_staged`] 共用同一套分块逻辑** —— 这里此前各写了一份，
    // 而这一份是错的：它只调一次 `ip_write`、并把返回的游标（实际写出字节数）丢掉。
    //
    // 真机判据（2026-10-13）：介质层自证
    // `失配 @200 want=1744b6262908ed6c got=ffffffffffffffff`
    // —— 写入范围的**第一个字节**就是擦除态。根因是一个 IP 命令最多送出
    // [`ip_burst_bytes`]（半个 TX FIFO = 32 B），而这里声明了整页
    // ⇒ 一页 256 B 只落了前 32 B。
    //（页对齐自证之所以能过，是因为它走的是 `program_page_staged`，那条路分块是对的。）
    //
    // 让两条路径共用同一个函数，也顺手消灭了"两份分块逻辑各自漂移"的可能 ——
    // 本项目已有过一次教训：**分叉的判据比没有判据更危险**。
    program_page_staged(address, data).map_err(|(stage, intr)| Intr {
        // 把"失败在哪一步"原样带出去，**不折叠**（现场只有一行日志）。
        note: u32::from(stage),
        ..intr
    })
}

/// 擦除的轮询上限（保守：真机擦一个扇区约 35 ms）。
const MAX_POLLS_ERASE: u32 = 200_000;
/// 编程的轮询上限（保守：真机编程一页约 85 µs）。
const MAX_POLLS_PROGRAM: u32 = 50_000;

/// 中断清标志掩码（`IPCMDERR` | `IPCMDGE` 及其 AHB 对应位；写 1 清除）。
const CLEAR_MASK: u32 = 0x0000_000A | 0x0000_0002;

/// 总线是否空闲（官方 `FLEXSPI_GetBusIdleStatus` 用的判据）。
///
/// 官方实现读 `STS0.SEQIDLE`（bit0）。这里保守地把 `STS0` 与 `STS2` 的非零
/// 都视为"还有事在做"是不对的 —— 所以只认官方那一位。
#[must_use]
pub fn bus_idle() -> bool {
    /// `STS0.SEQIDLE`（bit0）。
    const STS0_SEQIDLE: u32 = 0x0000_0001;
    read(offset::STS0) & STS0_SEQIDLE != 0
}
