//! 擦写自证：**选哪一页、写什么图案、怎么判回读** —— 全部是纯逻辑。
//!
//! # 为什么自证也需要"纯逻辑先行"
//!
//! **写 Flash 是这块板上唯一能把器件写坏的动作**。自证本身也是"写操作"，所以它的
//! **决策与判据**必须先在没有硬件的地方被测透：目标扇区选错 = 破坏数据；图案选错 =
//! 假通过；回读判据太松 = 器件没真写也判成功。这些都不是寄存器问题，是逻辑问题。

use crate::geometry::{PAGE_BYTES, SECTOR_BYTES};
use flash_layout::{FILESYSTEM_OFFSET, FILESYSTEM_SIZE};

/// 自证用的目标扇区：**文件系统的最后一个扇区**。
///
/// # 为什么是它
///
/// 分区表把整片 Flash 划满了（`check-flash-map` 断言"无缝隙"），所以**没有**一块
/// 天然的空闲地。可选的位置里，这一块最安全：
///
/// - **不在启动头**：写坏那里只能靠 BOOT_MODE 引脚救 —— 这是唯一不可接受的结果；
/// - **不在 `boot_meta`**：A/B 元数据（当前槽位/试启次数）被写坏会影响引导选择；
/// - **不在映像槽位**：那是在写正在运行的程序；
/// - **在文件系统里、而且是它的最后一个扇区**：文件系统当前**从未被写入过**
///   （挂载是只读的），整片对它而言都是"擦除态"= 空白；自证结束还会把它**擦回空白**，
///   因此文件系统不会看到任何"写过的痕迹"。
///
/// 另外它落在**可写分区**内，所以写入闸门（对齐 / 可写 / 不越界 / 别名自检）都会放行
/// —— 自证走的正是将来真实写入要走的同一条路径。
#[must_use]
pub const fn scratch_sector_offset() -> u32 {
    FILESYSTEM_OFFSET + FILESYSTEM_SIZE - SECTOR_BYTES
}

/// 自证目标页在自证扇区内的偏移。
#[must_use]
pub const fn scratch_page_offset() -> u32 {
    scratch_sector_offset()
}

/// 自证时用的**数据长度**（一页）。
///
/// 这个 `as` 按项目既有先例单独放行：数组长度必须是 `usize`，而页大小是 `u32`，
/// 常量上下文里没有可用的安全收窄（`try_from` 在 const 中尚未稳定）。
/// 两侧都是编译期常量，且下面的断言把"相等"钉死。
#[allow(clippy::as_conversions)]
pub const PATTERN_BYTES: usize = PAGE_BYTES as usize;

/// 长度必须与页大小一致 —— 改错一个会当场编译失败。
const _: () = assert!(PATTERN_BYTES == 256);

/// 编译期核对：自证目标确实落在文件系统分区内、且扇区对齐。
const _: () = {
    assert!(scratch_sector_offset() >= FILESYSTEM_OFFSET);
    assert!(scratch_sector_offset() + SECTOR_BYTES <= FILESYSTEM_OFFSET + FILESYSTEM_SIZE);
    assert!(scratch_sector_offset().is_multiple_of(SECTOR_BYTES));
    // "目标落在可写分区内"这条由运行时测试覆盖（`自证目标避开启动头与元数据与映像槽`
    // 与 `自证目标扇区通过写入判据`）—— `MAP` 的常量遍历在 const 上下文里写不出来。
};

/// 依据**页地址**生成图案。
///
/// # 为什么图案必须依赖地址
///
/// 常量图案（比如全 `0x5A`）有一个致命弱点：**写到了错误的地址也会通过**。
/// 让图案由目标地址派生（例如每字节 = `地址低 8 位 ^ 字节下标`），那么"写对了但读错了地址"
/// 或者"写到了别名地址"都会在比对时暴露 —— 这正是本项目最怕的那类静默错误
/// （越过 16 MiB 被别名到低 16 MiB）。
pub fn pattern(flash_offset: u32, out: &mut [u8; PATTERN_BYTES]) {
    // **单一事实来源**：定长版直接委托任意长度版。
    // （第一版这里各写了一份混合规则，两条路径给出不同图案 —— 被"任意长度图案与定长图案一致"
    // 那条测试当场抓住。分叉的判据比没有判据更危险，所以这里改成委托。）
    pattern_at(flash_offset, out);
}

/// 任意长度的图案（供"非对齐/跨页"用例使用）。
///
/// 与 [`pattern`] 同一条规则：**每字节由它在 Flash 上的绝对地址派生** ——
/// 于是一段范围无论从哪里开始、多长，"写与读"都能各自独立算出同一份期望值，
/// 而"写到错误地址"必然暴露。
pub fn pattern_at(flash_offset: u32, out: &mut [u8]) {
    // ⚠️ **纯函数：只依赖绝对地址**（这里踩过一次坑，值得写下来）。
    //
    // 第一版把切片内下标也混了进去（`^ (index % 256)`），于是"同一段 Flash 上的同一个字节"
    // 会因为**你从哪儿开始切**而算出不同的期望值：写入时用切片下标 40，逐字节核验时用下标 0
    // ⇒ 期望值不一致 ⇒ 真机自证报 FAIL（`bad=140`，即写入范围的第一个字节）。
    //
    // 后果比"自证失败"更值得警惕：**此前那次 `自证 PASS` 是比我说得更弱的证据** ——
    // 它只证明了"写与读在使用同一个切片相对函数时彼此一致"，并没有证明
    // "写到错误地址会被抓住"。这正是不该在判据里掺入"位置相对量"的原因。
    for (index, slot) in out.iter_mut().enumerate() {
        let absolute = flash_offset.wrapping_add(u32::try_from(index).unwrap_or(u32::MAX));
        let mixed = absolute.wrapping_mul(2_654_435_761);
        let bytes = mixed.to_le_bytes();
        // ⚠️ 必须混入**全部 4 个字节**：只用低 2 字节时，`+16 MiB`（2^20）这种偏移
        // 在低 16 位上恒为 0（2^20 × 奇数 ⇒ 低 16 位不变）⇒ 图案在别名偏移下**完全不变**，
        // 而那恰恰是最需要被抓住的情形（越过 16 MiB 静默别名）。测试当场证实了这一点。
        *slot = bytes[0] ^ bytes[1] ^ bytes[2] ^ bytes[3] ^ 0x5A;
    }
}

/// 任意长度的回读比对（返回第一个不符字节的下标）。
#[must_use]
pub fn verify_at(flash_offset: u32, read_back: &[u8]) -> Option<usize> {
    // 与 `pattern_at` 同一份规则：**只依赖绝对地址**，与切片从哪里开始无关。
    for (index, actual) in read_back.iter().enumerate() {
        let absolute = flash_offset.wrapping_add(u32::try_from(index).unwrap_or(u32::MAX));
        let mixed = absolute.wrapping_mul(2_654_435_761);
        let bytes = mixed.to_le_bytes();
        let expected = bytes[0] ^ bytes[1] ^ bytes[2] ^ bytes[3] ^ 0x5A;
        if *actual != expected {
            return Some(index);
        }
    }
    None
}

/// 回读比对。
///
/// 返回 `Some(index)` 表示**第 index 字节不符**（给出位置便于现场定位 ——
/// "对不上"和"第 37 字节对不上"在排查时价值完全不同）；`None` 表示全部相符。
#[must_use]
pub fn verify(flash_offset: u32, read_back: &[u8; PATTERN_BYTES]) -> Option<usize> {
    // 同上：委托给任意长度版，保证两条路径的判据永远一致。
    verify_at(flash_offset, read_back)
}

/// 自证的三个阶段（供终端命令与日志如实报告"走到哪一步就失败了"）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// 擦除目标扇区。
    Erase,
    /// 编程一页图案。
    Program,
    /// 回读比对。
    Verify,
    /// 擦回空白（无论前面成败都要做）。
    Restore,
}

impl Stage {
    /// 名字（终端/日志用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Erase => "erase",
            Self::Program => "program",
            Self::Verify => "verify",
            Self::Restore => "restore",
        }
    }
}
