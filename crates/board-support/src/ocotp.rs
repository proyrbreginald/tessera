//! OCOTP（一次性可编程熔丝）的只读访问：设备身份。
//!
//! # 现在读什么、烧什么
//!
//! **本模块只读，一个字都不烧。** 熔丝（eFuse）是**不可逆**的：写错一位就报废一颗芯片。
//! 所以熔丝策略是分阶段的（见 `docs/02-架构/资源利用与优化评审.md` §7）：
//! 现在只读设备身份用于日志归属与资产对账；`BOOT_CFG`、SRK 哈希、MAC 等留到产品定型后，
//! 按清单逐项烧写并留档。
//!
//! # 🟡 置信等级：能读到稳定值 ≠ 它就是"官方唯一 ID"
//!
//! 本模块读 `OCOTP` 的 **CFG0 / CFG1** 两个影子字（偏移 `0x410` / `0x420`），
//! 在 `MIMXRT1021.h` 里的注释是 "Value of OTP Bank0 Word1/Word2
//! (Configuration and Manufacturing Info.)"。
//!
//! - ✅ **已实测**：两个字在本板上能稳定读出（`0x613803da` / `0x0e3539d7`），
//!   且不是 0/全 1 的未烧状态；
//! - 🟡 **待确认**：NXP 参考手册的 OCOTP 章节是否把这两个字定义为"器件唯一 ID"。
//!   在拿到手册确认（或**第二块板**对照）之前，**不能**把它当作"唯一"使用 ——
//!   如果它其实是批次/晶圆信息，两台设备就会拿到同一个 ID，那是会污染资产对账的缺陷。
//!
//! 因此启动横幅与终端 `status` 里把它**明确标注为"候选"**。
//!
//! # 为什么不用 `imxrt-ral` 的字段名
//!
//! 与 SEMC 那次同类：RAL 的 `ocotp` 寄存器块把 `LOCK` 放在偏移 `0x38`
//! （手册与 `MIMXRT1021.h` 都是 `0x400`），布局对不上。所以这里按**手册偏移**直读，
//! 理由与证据写在下面这条注释里，避免下一个人以为"用 RAL 更安全"。

/// OCOTP 基址（与 `MIMXRT1021.h` 的 `OCOTP_BASE` 一致）。
const OCOTP_BASE: usize = 0x401F_4000;

/// `CFG0`（Bank0 Word1）在 OCOTP 里的偏移。取自 `MIMXRT1021.h` 的字段注释。
const CFG0_OFFSET: usize = 0x410;

/// `CFG1`（Bank0 Word2）在 OCOTP 里的偏移。
const CFG1_OFFSET: usize = 0x420;

/// 读一个熔丝影子字。
///
/// 返回值放在 `Option` 里：将来若加入"熔丝控制器未就绪"之类的判定，
/// 调用方不必改签名（也为"读到全 0/全 1 视为未烧"留出位置）。
fn read_shadow(offset: usize) -> Option<u32> {
    use core::ptr::with_exposed_provenance;
    let address = OCOTP_BASE.checked_add(offset)?;
    // SAFETY: 地址落在 OCOTP 的熔丝影子寄存器区（基址 = `MIMXRT1021.h` 的
    // `OCOTP_BASE`，偏移取自同一头文件对 `CFG0`/`CFG1` 的注释）。这些寄存器
    // **只读**：本模块不写 `CTRL`/`DATA`，因此不可能触发熔丝编程。
    let low = unsafe { core::ptr::read_volatile(with_exposed_provenance::<u32>(address)) };
    Some(low)
}

/// 设备身份（候选）：`CFG0` 与 `CFG1` 拼成的 64 位值。
///
/// 读不到时返回 `None`（而不是 0）—— "没有身份"和"身份是 0"是两件事。
#[must_use]
pub fn candidate_device_id() -> Option<u64> {
    let high = read_shadow(CFG0_OFFSET)?;
    let low = read_shadow(CFG1_OFFSET)?;
    // 全 0 或全 1 视为未烧写（空白熔丝的典型值），此时没有可用身份。
    if (high == 0 && low == 0) || (high == u32::MAX && low == u32::MAX) {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

/// 把设备身份写成 16 位十六进制（便于日志与资产对账）。
///
/// 参数用 `&mut dyn Write` 而不是 `impl Write`：调用方常常已经持有
/// `&mut dyn Write`（终端命令的输出通道），泛型版本在那种场合无法直接传。
pub fn write_device_id(out: &mut dyn core::fmt::Write) {
    match candidate_device_id() {
        Some(id) => {
            // 🟡 标注"候选"：唯一性待手册确认（见模块文档）。
            let _ = write!(out, "{id:016x}（候选，唯一性待手册确认）");
        }
        None => {
            let _ = write!(out, "不可用（熔丝为空白或读取失败）");
        }
    }
}

/// 熔丝影子寄存器的只读快照（供未来的 `fuse-status` 诊断使用）。
///
/// 返回 `(LOCK, CFG0..CFG6)`；**仅供查看**，本模块不提供任何写接口。
#[must_use]
pub fn snapshot() -> [u32; 8] {
    let mut words = [0u32; 8];
    let mut index = 0;
    while index < words.len() {
        let offset = 0x400 + index * 0x10;
        if let Some(value) = read_shadow(offset)
            && let Some(slot) = words.get_mut(index)
        {
            *slot = value;
        }
        index += 1;
    }
    words
}
