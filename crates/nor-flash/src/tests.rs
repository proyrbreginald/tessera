//! `nor-flash` 的主机测试。
//!
//! 覆盖的重点是**三条曾经在嵌入式项目里造成过灾难的判据**：扇区对齐、不跨页、
//! 不碰只读分区；以及"4 字节操作码必须配 4 字节相位"这条数据安全命门。

use crate::geometry::{PAGE_BYTES, SECTOR_BYTES};
use crate::ops::{ADDRESS_PHASE_4B, AddressMode, Opcode, byte_address_phase};
use crate::range::{FlashError, check_erase, check_program, pages_in, sectors_in};
use flash_layout::{FILESYSTEM_OFFSET, FILESYSTEM_SIZE};

/// 擦除：4 KiB 对齐才放过；不对齐、长度为 0 都拒绝。
#[test]
fn 擦除必须扇区对齐() {
    // 文件系统分区起点本身是 64 KiB 对齐的 ⇒ 合法。
    assert_eq!(check_erase(FILESYSTEM_OFFSET, SECTOR_BYTES), Ok(()));
    assert_eq!(
        check_erase(FILESYSTEM_OFFSET + 1, SECTOR_BYTES),
        Err(FlashError::NotAligned)
    );
    assert_eq!(
        check_erase(FILESYSTEM_OFFSET, 1),
        Err(FlashError::NotAligned)
    );
    assert_eq!(check_erase(FILESYSTEM_OFFSET, 0), Err(FlashError::Empty));
}

/// **绝不碰启动头**：那里是 ReadOnly，写坏的后果是"只能靠 BOOT_MODE 引脚救"。
#[test]
fn 擦除不得碰只读分区() {
    assert_eq!(check_erase(0, SECTOR_BYTES), Err(FlashError::ReadOnly));
    assert_eq!(check_erase(0x2000, SECTOR_BYTES), Err(FlashError::ReadOnly));
}

/// 擦除不得**跨越**两个分区（哪怕每个字节都在"某个分区内"）。
#[test]
fn 擦除不得跨分区() {
    // 从 FS 前一个扇区开始擦 2 个扇区 ⇒ 横跨 FS 边界。
    let straddle = FILESYSTEM_OFFSET - SECTOR_BYTES;
    assert_eq!(
        check_erase(straddle, SECTOR_BYTES * 2),
        Err(FlashError::OutOfRange)
    );
    // 单个扇区落在 boot_meta 里是可以的（它可写）。
    assert_eq!(check_erase(straddle, SECTOR_BYTES), Ok(()));
}

/// 页编程**不能跨页** —— 器件跨页会回卷到页首，静默破坏该页开头。
#[test]
fn 页编程不得跨页() {
    let page = FILESYSTEM_OFFSET;
    assert_eq!(check_program(page, PAGE_BYTES), Ok(()));
    assert_eq!(
        check_program(page + 1, PAGE_BYTES - 1),
        Ok(()),
        "写到页尾仍在同页内"
    );
    assert_eq!(
        check_program(page + 1, PAGE_BYTES),
        Err(FlashError::SpansPage)
    );
    assert_eq!(
        check_program(page, PAGE_BYTES + 1),
        Err(FlashError::SpansPage)
    );
    assert_eq!(check_program(page, 0), Err(FlashError::Empty));
}

/// 越界与窗口边界。
///
/// # 这里记录一个**容易搞错的事实**（本测试的第一版就搞错了）
///
/// 文件系统分区**跨过 16 MiB**（FS 从 `0x420000` 一直铺到器件尾 32 MiB）。因此
/// "越过 16 MiB 就不能访问"**不成立** —— 本项目能访问到 32 MiB，靠的是 FlexSPI 的
/// **LUT 使用 4 字节地址相位**（读 `0xEC`/擦 `0x21`/写 `0x34`，相位 `0x20`），
/// 而这条相位由 `cargo xtask check-image` 在**构建期**从产物里断言。
///
/// 所以：范围校验的上界是**器件/XIP 窗口**（32 MiB），而
/// [`ADDRESSABLE_BYTES`]（16 MiB）只描述"**上电默认 3 字节模式**能走多远"——
/// 它是一个**风险提示**，不是本 crate 的边界。
#[test]
fn 越界与窗口边界() {
    // 16 MiB 之后的页**可以**编程：因为 LUT 走 4 字节相位（见上面的说明）。
    assert_eq!(
        check_program(crate::geometry::ADDRESSABLE_BYTES, PAGE_BYTES),
        Ok(())
    );

    // 真正的边界是 XIP 窗口（= 器件容量 32 MiB）：越过它必须拒绝。
    let last_page = crate::geometry::DEVICE_BYTES - PAGE_BYTES;
    assert_eq!(check_program(last_page, PAGE_BYTES), Ok(()));
    assert_eq!(
        check_program(crate::geometry::DEVICE_BYTES, 1),
        Err(FlashError::OutOfRange)
    );

    // 加法溢出必须被拒（而不是回绕成一个"看起来合法"的小偏移）。
    assert_eq!(check_program(u32::MAX - 8, 16), Err(FlashError::OutOfRange));
    assert_eq!(
        check_erase(u32::MAX - 4095, 4096),
        Err(FlashError::OutOfRange)
    );
}

/// 扇区/页计数：长度为 0 或未对齐要拒绝，而不是给出 0 个。
#[test]
fn 计数函数拒绝无意义输入() {
    assert_eq!(sectors_in(SECTOR_BYTES * 3), Ok(3));
    assert_eq!(sectors_in(SECTOR_BYTES + 1), Err(FlashError::NotAligned));
    assert_eq!(sectors_in(0), Err(FlashError::NotAligned));
    assert_eq!(pages_in(PAGE_BYTES * 3), Ok(3));
    assert_eq!(pages_in(PAGE_BYTES + 1), Ok(2), "不足一页按一页算");
    assert_eq!(pages_in(0), Err(FlashError::Empty));
}

/// **数据安全命门**：4 字节操作码必须配 4 字节地址相位。
///
/// 配错的表现是"命令看起来成功、实际写到低 16 MiB 的同名偏移" ——
/// 也就是"擦日志区"擦掉启动头。这条断言把那个错误钉在编译期与测试里。
#[test]
fn 四字节操作码必须配四字节相位() {
    for opcode in [Opcode::SectorErase4B, Opcode::PageProgram4B, Opcode::Read4B] {
        assert_eq!(opcode.address_mode(), Some(AddressMode::Four), "{opcode:?}");
        assert_eq!(byte_address_phase(opcode), ADDRESS_PHASE_4B, "{opcode:?}");
    }
    // 无地址的操作码不许带相位（带了会让命令被解析成"地址字节"）。
    for opcode in [Opcode::WriteEnable, Opcode::ReadStatus1] {
        assert_eq!(opcode.address_mode(), None, "{opcode:?}");
        assert_eq!(byte_address_phase(opcode), 0, "{opcode:?}");
    }
    // 操作码字节必须与器件手册一致（这里逐个列出来，改动会当场失败）。
    assert_eq!(Opcode::WriteEnable.byte(), 0x06);
    assert_eq!(Opcode::ReadStatus1.byte(), 0x05);
    assert_eq!(Opcode::SectorErase4B.byte(), 0x21);
    assert_eq!(Opcode::PageProgram4B.byte(), 0x34);
    assert_eq!(Opcode::Read4B.byte(), 0xEC);
}

/// 分区表里的文件系统必须存在且可写 —— 否则"日志/配置往哪写"这件事没有答案。
#[test]
fn 文件系统分区可写且足够大() {
    // 这些是**常量事实**，因此断言写在 const 块里：编译期就检查，而不是等测试跑起来。
    const { assert!(FILESYSTEM_SIZE > 0, "FS 分区不能为空") };
    const {
        assert!(
            FILESYSTEM_OFFSET.is_multiple_of(SECTOR_BYTES),
            "FS 起点必须扇区对齐"
        )
    };
    const {
        assert!(
            FILESYSTEM_SIZE.is_multiple_of(SECTOR_BYTES),
            "FS 容量必须是扇区整数倍"
        )
    };
    assert_eq!(check_erase(FILESYSTEM_OFFSET, SECTOR_BYTES), Ok(()));
}

/// **自证目标必须能通过写入闸门** —— 否则真机上第一步就失败，白跑一趟。
#[test]
fn 自证目标扇区通过写入判据() {
    use crate::selftest::{PATTERN_BYTES, scratch_page_offset, scratch_sector_offset};
    let sector = scratch_sector_offset();
    assert_eq!(check_erase(sector, SECTOR_BYTES), Ok(()), "擦除应当被放行");
    assert_eq!(
        check_program(scratch_page_offset(), PATTERN_BYTES as u32),
        Ok(()),
        "页编程应当被放行"
    );
}

/// **自证目标不得与任何"写了会出事"的区域重叠**（这是这一步的安全底线）。
#[test]
fn 自证目标避开启动头与元数据与映像槽() {
    use crate::selftest::scratch_sector_offset;
    use flash_layout::{MAP, Purpose};

    let target_start = scratch_sector_offset();
    let target_end = target_start + SECTOR_BYTES;
    for region in MAP {
        let overlaps = target_start < region.offset + region.size && target_end > region.offset;
        if overlaps {
            assert_eq!(
                region.purpose,
                Purpose::Filesystem,
                "自证目标与 `{}` 重叠 —— 那里写坏会影响启动/引导",
                region.name
            );
        }
    }
}

/// 图案**必须随地址变化**：常量图案在"写错地址/被别名"时会假通过。
#[test]
fn 图案随地址变化且可重复() {
    use crate::selftest::{PATTERN_BYTES, pattern};
    let mut first = [0u8; PATTERN_BYTES];
    let mut again = [0u8; PATTERN_BYTES];
    let mut other = [0u8; PATTERN_BYTES];
    pattern(0x420000, &mut first);
    pattern(0x420000, &mut again);
    pattern(0x420100, &mut other);
    assert_eq!(first, again, "同一地址必须给出同一图案（可重复）");
    assert_ne!(
        first, other,
        "不同地址必须给出不同图案（否则写错地址也会通过）"
    );
    assert!(first.iter().any(|byte| *byte != first[0]), "图案不该是常量");
}

/// 回读比对要**指出第一个不符的字节**（现场排查时"第 37 字节不符"远比"对不上"有用）。
#[test]
fn 回读比对指出首个不符字节() {
    use crate::selftest::{PATTERN_BYTES, pattern, verify};
    let offset = 0x420000;
    let mut buffer = [0u8; PATTERN_BYTES];
    pattern(offset, &mut buffer);
    assert_eq!(verify(offset, &buffer), None, "原样写回的必须判定通过");

    buffer[37] ^= 0x01;
    assert_eq!(verify(offset, &buffer), Some(37), "应当报出第 37 字节");

    // 换一个地址比对：整个图案都不同 ⇒ 第一个字节就不符
    // （这正是"越过 16 MiB 被别名"时会发生的情形：数据写对了，但落在别的地址上）。
    // 换个地址比对：图案由地址派生 ⇒ 必然有不符字节（不是"可能"）。
    // 这正是"越过 16 MiB 被别名"会发生的情形：数据写对了，但落在别的地址上。
    let mut alias = [0u8; PATTERN_BYTES];
    pattern(offset, &mut alias);
    assert!(
        verify(offset + 0x10_0000, &alias).is_some(),
        "别名地址必须当场暴露"
    );
}

/// 目标页必须与目标扇区起点一致（自证只碰一页，且就在扇区开头）。
#[test]
fn 自证页位于扇区开头() {
    use crate::selftest::{scratch_page_offset, scratch_sector_offset};
    assert_eq!(scratch_page_offset(), scratch_sector_offset());
    assert_eq!(scratch_page_offset() % PAGE_BYTES, 0, "页必须 256 B 对齐");
}

/// 任意长度图案与定长图案必须**一致**（两条规则只能是同一条，否则自证的判据会分叉）。
#[test]
fn 任意长度图案与定长图案一致() {
    use crate::selftest::{PATTERN_BYTES, pattern, pattern_at};
    let mut short = [0u8; PATTERN_BYTES];
    let mut long = [0u8; PATTERN_BYTES];
    pattern(0x420000, &mut short);
    pattern_at(0x420000, &mut long);
    assert_eq!(short, long, "同一段范围两条路径必须给出同一份期望值");
}

/// `verify_at` 必须在任意长度上都指出**第一个**不符字节（非对齐/跨页用例靠它判定）。
#[test]
fn 任意长度比对指出首个不符字节() {
    use crate::selftest::{pattern_at, verify_at};
    let mut region = [0u8; 300];
    pattern_at(0x4200C8, &mut region);
    assert_eq!(verify_at(0x4200C8, &region), None);

    region[7] ^= 0x40;
    assert_eq!(verify_at(0x4200C8, &region), Some(7));

    // 换个起始地址比对：地址派生的图案必然整体不符（"写到错误地址"会被抓住）。
    assert!(verify_at(0x4201C8, &region).is_some());
}

/// 寄存器偏移与位域**照抄自官方头文件** —— 抄错就应当在这里失败，而不是在板子上失败。
#[test]
fn flexspi_寄存器常量与官方一致() {
    use crate::flexspi::{bits, lut_word, offset, seq};

    // 偏移（MIMXRT1021.h 的 FLEXSPI_Type 字段 offset 注释）
    assert_eq!(offset::MCR0, 0x000);
    assert_eq!(offset::INTR, 0x014);
    assert_eq!(offset::FLSHCR2, 0x080);
    assert_eq!(offset::IPCR0, 0x0A0);
    assert_eq!(offset::IPCR1, 0x0A4);
    assert_eq!(offset::IPCMD, 0x0B0);
    assert_eq!(offset::IPTXFSTS, 0x0F4);
    assert_eq!(offset::RFDR, 0x100);
    assert_eq!(offset::LUT, 0x200);
    assert_eq!(offset::LUT_ENTRIES, 64);

    // 位域（CMSIS 的 FLEXSPI_*_MASK/​*_SHIFT）
    assert_eq!(bits::MCR0_MDIS, 0x2);
    assert_eq!(bits::INTR_IPCMDDONE, 0x1);
    assert_eq!(bits::INTR_IPCMDGE, 0x2);
    assert_eq!(bits::INTR_IPCMDERR, 0x8);
    assert_eq!(bits::FLSHCR2_CLRINSTRPTR, 0x8000_0000);
    assert_eq!(bits::IPCR1_ISEQID_SHIFT, 16);
    assert_eq!(bits::IPCR1_ISEQNUM_SHIFT, 24);
    assert_eq!(bits::IPCMD_TRG, 0x1);

    // 序列号（与 fsl_romapi.h 的 NOR_CMD_LUT_SEQ_IDX_* 完全一致）
    assert_eq!(seq::READ, 0);
    assert_eq!(seq::READ_STATUS, 1);
    assert_eq!(seq::WRITE_ENABLE, 3);
    assert_eq!(seq::ERASE_SECTOR, 5);
    assert_eq!(seq::PAGE_PROGRAM, 9);

    // LUT 字索引 = 序列号 × 4（每条序列占 4 项）
    assert_eq!(lut_word(0), Some(0));
    assert_eq!(lut_word(9), Some(36));
    assert_eq!(lut_word(15), Some(60));
    assert_eq!(lut_word(16), None, "超出 64 项必须返回 None 而不是越界");
}

/// `IPCR1` 打包：与官方 `FLEXSPI_TransferBlocking` 的写法逐位一致。
#[test]
fn flexspi_ipcr1_打包与官方一致() {
    use crate::flexspi::pack_ipcr1;

    // 页编程一页：数据量 256、序列 9、1 条序列 ⇒ IDATSZ=0x100、ISEQID=9、ISEQNUM=0
    assert_eq!(pack_ipcr1(256, 9, 1), Some(0x0009_0100));
    // 擦除：无数据、序列 5、2 条序列 ⇒ ISEQNUM 存"条数-1"=1
    assert_eq!(pack_ipcr1(0, 5, 2), Some(0x0105_0000));
    // IDATSZ 只有 16 位 ⇒ 超出部分被掩掉（与官方宏一致）
    assert_eq!(pack_ipcr1(0x1_0000, 0, 1), Some(0));
    // 0 条序列非法（官方会算出 0xFF...，我们直接拒绝）
    assert_eq!(pack_ipcr1(0, 0, 0), None);
}

/// `INTR` 解码：**两个错误位都要看**，且"未完成"要与"出错"分开。
#[test]
fn flexspi_intr_解码区分成功与各类失败() {
    use crate::flexspi::Intr;

    // 完成、无错
    let ok = Intr::decode(0x1);
    assert!(ok.is_success());
    assert_eq!(ok.describe(), "ok");

    // 完成 + 授予错误（IPCMDGE 是**错误位**）
    let grant = Intr::decode(0x3);
    assert!(grant.grant_error);
    assert!(!grant.is_success());
    assert!(grant.describe().contains("IPCMDGE"));

    // 完成 + 命令错误
    let err = Intr::decode(0x9);
    assert!(err.cmd_error);
    assert!(!err.is_success());
    assert!(err.describe().contains("IPCMDERR"));

    // 全 0：什么都没发生（未触发或超时）—— **不是**"授予错误"
    let idle = Intr::decode(0);
    assert!(!idle.done && !idle.grant_error && !idle.cmd_error);
    assert_eq!(idle.describe(), "未完成（超时或未触发）");
}

/// LUT 字：**硬编码值必须等于公式反算值** —— 抄错一个数字就测试失败。
///
/// 这样做的意义：这些字最终要写进控制器的 LUT，写错了表现为"命令发出去没反应"，
/// 在真机上极难分辨是编码错、时序错还是器件错。放在这里，抄错当场就暴露。
#[test]
fn flexspi_lut_字与官方公式一致() {
    use crate::flexspi::{lut_seq, lut_words, opcode, pad};

    assert_eq!(
        lut_words::READ[0],
        lut_seq(opcode::SDR, pad::ONE, 0xEC, opcode::RADDR, pad::FOUR, 0x20)
    );
    assert_eq!(lut_words::READ[0], 0x0A20_04EC);
    assert_eq!(
        lut_words::READ[1],
        lut_seq(
            opcode::DUMMY,
            pad::FOUR,
            0x06,
            opcode::READ,
            pad::FOUR,
            0x04
        )
    );
    assert_eq!(lut_words::READ[1], 0x2604_3206);

    assert_eq!(lut_words::READ_STATUS[0], 0x2401_0405);
    assert_eq!(lut_words::WRITE_ENABLE[0], 0x0000_0406);
    // ⚠️ 擦除的地址相位是 **1PAD**（官方 `fsl_adapter_*.c` 与 `flash.rs` 的文档一致），
    // 所以是 `0x0820_0421` 而不是 `0x0A20_0421` —— 我第一版按 4PAD 手算写错了，
    // 正是这条断言把它抓出来（`const` 的值由公式算出，所以错的是我的手算期望值）。
    assert_eq!(lut_words::ERASE_SECTOR[0], 0x0820_0421);
    assert_eq!(
        lut_words::PAGE_PROGRAM[0],
        lut_seq(opcode::SDR, pad::ONE, 0x34, opcode::RADDR, pad::ONE, 0x20)
    );
    assert_eq!(lut_words::PAGE_PROGRAM[0], 0x0820_0434);
    assert_eq!(lut_words::PAGE_PROGRAM[1], 0x0000_2204); // 写相位 4PAD（本板既有 C 固件原文）

    // 字段掩码：操作码 6 位、pad 2 位、操作数 8 位 —— 越界位不该串到邻居字段里
    assert_eq!(lut_seq(0x3F, 0x3, 0xFF, 0, 0, 0), 0x0000_FFFF);
}

/// TX FIFO 容量是**16 字**（官方按字段掩码算），**不是** `TFDR[32]` 的数组长度 32。
///
/// 我用错那个数字时，真机报 `INTR.SCKSTOPBYWR`（写相位 SCLK 停摆）。
#[test]
fn flexspi_tx_fifo_容量按官方掩码表达式为16字() {
    use crate::flexspi::tx_fifo_words;

    assert_eq!(
        tx_fifo_words(),
        16,
        "官方表达式 = (TXWMRK_MASK >> SHIFT) + 1"
    );
    assert_eq!(
        crate::flexspi::offset::FIFO_WORDS,
        32,
        "数组长度是 32 —— 正因为它不是容量，才有这次踩坑"
    );
}
