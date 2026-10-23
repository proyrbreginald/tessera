//! FlexSPI 上的存储介质。
//!
//! # 读走 XIP，擦写走 ROM 驱动 + 寄存器级 IP 命令
//!
//! 读路径直接走 FlexSPI 的 **XIP 映射**（`0x6000_0000`）：Boot ROM 已经把控制器
//! 配到能取指，运行期读同一区域即可，不需要额外驱动。
//!
//! 擦除与编程必须自研：`imxrt-hal` 0.6 没有 FlexSPI 驱动，而 `imxrt-ral` 只提供
//! 寄存器块。现行实现：**擦除**走 Boot ROM 的 NOR 驱动（`crate::rom_flash`），
//! **页编程**走寄存器级 IP 命令（`crate::flexspi_ip`，ROM 的编程路径在本板报成功
//! 但不写）。每次写入都先过 `check_write_gates`（fail-closed），写后按范围
//! 失效 D-Cache 行。
//!
//! 所有写入口都**不**在关中断路径上执行（写队列 + `idle` 属主，**平台需求 RT-3**）。

use ports::raw::RawMedium;
use ports::store::StoreError;

use crate::config;

/// FlexSPI1 的 XIP 映射基址。✅（`imxrt-rt` 为 `Family::Imxrt1020` 生成的 FLASH 区域）
pub const XIP_BASE: usize = 0x6000_0000;

/// 外部 QSPI NOR 介质。
#[derive(Debug, Clone, Copy, Default)]
pub struct FlexSpiMedium;

impl FlexSpiMedium {
    /// 构造。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 判断一个偏移是否落在 XIP **可寻址**区内。
    ///
    /// 判据来自分区表的 [`flash_layout::is_addressable`]（16 MiB 窗口），
    /// **不是**器件容量（32 MiB）：器件上电处于 3 字节地址模式，越过 16 MiB 的访问
    /// 会被控制器丢掉高位地址、别名到低 16 MiB —— 也就是"擦日志区"可能实际擦掉启动头。
    /// 边界规则只有一处定义（在分区表 crate 里），且被那里的测试钉住。
    #[must_use]
    pub fn is_mapped(offset: usize) -> bool {
        // 用 `try_from` 而不是 `as`：转换失败（偏移超出 u32）本身就意味着越界。
        u32::try_from(offset).is_ok_and(flash_layout::is_addressable)
    }
}

impl RawMedium for FlexSpiMedium {
    const CAPACITY: usize = config::ADDRESSABLE_BYTES;
    /// 4 KiB 扇区（器件参数 ✅）。
    const SECTOR: usize = 4096;

    fn read(&mut self, offset: usize, out: &mut [u8]) -> Result<usize, StoreError> {
        if !Self::is_mapped(offset) {
            return Err(StoreError::Driver);
        }
        let n = core::cmp::min(out.len(), Self::CAPACITY.saturating_sub(offset));
        let base = XIP_BASE.checked_add(offset).ok_or(StoreError::Driver)?;

        // **批量拷贝，而不是逐字节 volatile 循环。**
        //
        // 2026-10-10 真机教训：逐字节 `read_volatile`（每字节还有一次 checked_add）
        // 让文件系统挂载扫描整片 27.88 MiB 慢到几十秒 —— 表现为"启动后串口长时间
        // 没有任何输出"，看起来像死机，实际只是读得太慢（用调试器连续采样 PC 才
        // 分辨出"在动"而不是"卡住"）。
        //
        // 语义上这也是正确的原语：XIP 映射区是**普通可读内存**，一次拷贝拿到的
        // 就是这一瞬间的快照；逐字节 volatile 并不能提供更强的保证，只是把
        // 一个 memcpy 变成几千万次单独读。
        //
        // ⚠️ 写路径的缺口（见本文件 `erase` / `program` 处的标记）要注意：
        // 若该区域被配置为可缓存，改完 Flash 需要数据缓存维护；读路径不需要。
        // SAFETY: 源区间 `[base, base+n)` 落在 FlexSPI1 的 XIP 映射区内
        // （`is_mapped` 与 `CAPACITY` 已界定），目标 `out` 是长度 ≥ n 的可写切片，
        // 两者不重叠（一个是外部器件映射，一个是 RAM）。
        unsafe {
            core::ptr::copy_nonoverlapping(
                core::ptr::with_exposed_provenance::<u8>(base),
                out.as_mut_ptr(),
                n,
            );
        }
        Ok(n)
    }

    fn erase(&mut self, offset: usize, len: usize) -> Result<(), StoreError> {
        // ⚠️ 判据先于动作：闸门**必须**在碰控制器之前过。
        check_write_gates(offset, len, Gate::Erase)?;
        // 逐扇区调用 ROM 驱动（ROM 的 `erase` 一次一个扇区）。
        // 扇区大小安全收窄一次（它是 2 的幂且远小于 usize）。
        let sector = usize::try_from(nor_flash::SECTOR_BYTES).map_err(|_| StoreError::Rejected)?;
        let mut at = offset;
        let end = offset + len;
        while at < end {
            crate::rom_flash::erase_sector(u32::try_from(at).map_err(|_| StoreError::Rejected)?)?;
            at += sector;
        }
        Ok(())
    }

    fn program(&mut self, offset: usize, data: &[u8]) -> Result<(), StoreError> {
        // ⚠️ 闸门按**单页**判（`check_program` 明确拒绝跨页与超页），
        // 而本函数要把任意长度拆成逐页写 —— 所以闸门必须**逐块**过，
        // 不能在入口处拿整段长度去判（那样任何跨页写都会被 `SpansPage` 直接拒掉，
        // 真机表现：`自证(跨页+非对齐) FAIL bad=3`）。
        check_write_gates(offset, core::cmp::min(data.len(), 1), Gate::Program)?;
        // ⚠️ ROM 一次**只编程一整页**（256 B），而 NOR 的语义是"只能把 1 写成 0"。
        // 因此任意长度的写入必须做**读-改-写**：读出当前页、拼接新字节、整页编程。
        // 这与本板本板既有 C 固件的做法一致（其 `ProgramBytes` 内部就是按页拆分的循环）。
        let page_bytes =
            usize::try_from(nor_flash::PAGE_BYTES).map_err(|_| StoreError::Rejected)?;
        let mut done = 0usize;
        while done < data.len() {
            let at = offset + done;
            let page_start = at - (at % page_bytes);
            let in_page = at - page_start;
            let room = page_bytes - in_page;
            let take = core::cmp::min(room, data.len() - done);

            // 读出整页（XIP 拷贝），把这一段替换掉，再整页编程。
            let mut page = [0u8; 256];
            let read = self.read(page_start, &mut page)?;
            if read != page.len() {
                return Err(StoreError::Driver);
            }
            let Some(target) = page.get_mut(in_page..in_page + take) else {
                return Err(StoreError::Rejected);
            };
            let Some(source) = data.get(done..done + take) else {
                return Err(StoreError::Rejected);
            };
            target.copy_from_slice(source);

            // **整页编程走寄存器路径**（ROM 的 `program` 在本板报成功但不写，
            // 判据见 `rom_flash::program_page` 的文档）。
            crate::flexspi_ip::program_page(
                u32::try_from(page_start).map_err(|_| StoreError::Rejected)?,
                &page,
            )
            .map_err(|_| StoreError::Driver)?;
            done += take;
        }
        Ok(())
    }
}

/// 这次写请求要走哪条判据。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    /// 扇区擦除（4 KiB 对齐）。
    Erase,
    /// 页编程（不跨 256 B 页）。
    Program,
}

/// 供 `rom_flash` 复用的闸门入口（`erase = true` 走擦除判据，`false` 走页编程判据）。
pub(crate) fn check_write_gates_usize(
    offset: usize,
    len: usize,
    erase: bool,
) -> Result<(), StoreError> {
    check_write_gates(offset, len, if erase { Gate::Erase } else { Gate::Program })
}

/// **写入前的全部判据**：一次性把"该不该写"问清楚。
///
/// # 顺序为什么是这样
///
/// 1. **别名闸门（最优先）**：`four_byte_addressing_ok()` 每次启动做两次读，判"越过 16 MiB
///    的访问是否被静默别名到低 16 MiB"。它一旦不通过，**任何越界写入都可能落到启动头上**
///    —— 那是这块板上唯一需要 BOOT_MODE 引脚 + 串口下载器才能救的事故。所以它排在第一个，
///    而且**失败即拒绝**（fail closed），不是"警告后照写"。
/// 2. **范围与对齐**：交给纯逻辑的 [`nor_flash`]（主机上已被反复测试）——
///    未对齐、跨页、落只读分区一律拒绝。
/// 3. **映射检查**：最后确认目标确实在 XIP 映射区内。
///
/// # Errors
///
/// 任一判据不通过返回 [`StoreError::Rejected`]。**它绝不返回"看起来成功"** ——
/// 在这条路径上，假装成功等于损坏数据。
fn check_write_gates(offset: usize, len: usize, gate: Gate) -> Result<(), StoreError> {
    // 1) 别名闸门：失败即关闭。
    if !crate::flash::four_byte_addressing_ok() {
        return Err(StoreError::Rejected);
    }
    // 2) 纯逻辑判据（对齐 / 不跨页 / 在可写分区内 / 不越界）。
    let (Ok(offset32), Ok(len32)) = (u32::try_from(offset), u32::try_from(len)) else {
        return Err(StoreError::Rejected);
    };
    let verdict = match gate {
        Gate::Erase => nor_flash::check_erase(offset32, len32),
        Gate::Program => nor_flash::check_program(offset32, len32),
    };
    if verdict.is_err() {
        return Err(StoreError::Rejected);
    }
    // 3) 映射检查。
    if !FlexSpiMedium::is_mapped(offset) {
        return Err(StoreError::Rejected);
    }
    Ok(())
}
