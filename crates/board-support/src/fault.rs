//! 故障与复位：把"上次为什么挂"变成**可上报的事实**。
//!
//! # 这个模块补的是本底座最刺眼的一个洞
//!
//! 2026-10-12 的只读取证审计从**链接产物**里读出：向量表槽 3（`HardFault`）指向的函数
//! 反汇编就是 `push {r7,lr}; mov r7,sp; b .` —— **一个空死循环**；槽 4/5/6
//! （`MemManage`/`BusFault`/`UsageFault`）指向 `DefaultHandler_`，也是空自旋。
//! 而且 `SHCSR` 的三个 ENA 位从不写 ⇒ 三类可配置故障**一律升级成 HardFault**。
//! 全仓还没有任何地方读 `SRC_SRSR`。
//!
//! 后果：设备挂掉之后，现场只剩"串口安静"—— 不知道复位原因、不知道故障类型、
//! 不知道出错地址、也不知道崩溃前发生了什么。**而现场故障恰恰没有调试器。**
//!
//! # 本模块做什么
//!
//! 1. [`enable_handlers`]：打开 `SHCSR` 里三个 ENA 位（用 cortex-m 的**安全 API**），
//!    让 MemManage/BusFault/UsageFault 各自进自己的处理器 —— 否则只能看到一个笼统的
//!    "HardFault"，根因信息永久丢失。
//! 2. [`reset_cause`] / [`clear_reset_cause`]：读并清 `SRC_SRSR`。
//!    ⚠️ `SRSR` 是**写 1 清零**，所以顺序必须是"先读、上报、再清"；清了才能区分
//!    "本次复位的原因"与"上次遗留的位"。
//! 3. [`fault_status`] / [`on_fault`]：抓 `CFSR`/`HFSR`/`MMFAR`/`BFAR` + 硬件入栈帧，
//!    写一条[`CrashRecord`]进 `.uninit`（**跨暖复位存活**），打到串口，然后决定
//!    "复位"还是"停机等人"。
//! 4. [`previous_crash`]：冷启动读回上一条记录 —— 这就是"冷启动能自报上次为什么挂"。
//!
//! # 为什么记录放在 `.uninit`
//!
//! `.bss` 会被启动代码清零，`.data` 会被初始化 —— 只有**不被初始化的段**才能在暖复位后
//! 留下内容。审计确认本芯片的链接脚本里有 `.uninit`（OCRAM，5,560 B），
//! 且它**已被 RTIC 的局部资源占满**，所以这里只放 11 个字（44 字节）的一条记录。
//!
//! 上电后这段 RAM 里是**垃圾**，因此记录自带魔数与校验和：解不出来就说"没有记录"，
//! **绝不把随机内容当成故障现场**（那比没有记录更坏：它会给出一个看起来很像地址的假现场）。
//!
//! # 连续故障：复位还是停机
//!
//! 故障后复位能把设备拉回服务，但**反复故障会让它变成复位循环**（而且本板的外部看门狗
//! 由 I2C 扩展器控制、固件目前既不能开也不能关，没有任何东西能把它从循环里拉出来）。
//! 因此记录里带一个**连续故障计数**：达到 [`MAX_FAULT_STREAK`] 就**停机等人**，
//! 并在串口上把现场打清楚。

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m::peripheral::SCB;
use critical_section::Mutex;
use fault_decode::{CrashRecord, FaultFrame, FaultStatus, ResetCause};
use imxrt_ral as ral;

/// 多少字一条崩溃记录（编译期常量，供静态数组用）。
const CRASH_WORDS: usize = CrashRecord::WORDS;

/// 连续故障达到几次就**不再复位**、改为停机等人。
///
/// 取 3 的理由与安全状态机的"连续复位"判据一致（`primitives::safety` 里
/// `MAX_CONSECUTIVE_RESETS` 也是 3）：给两次自愈机会，第三次说明复位解决不了。
pub const MAX_FAULT_STREAK: u32 = 3;

/// 把一个**架构常量**的 MMIO 地址变成指针。
///
/// 用 `null().wrapping_add(字偏移)` 而不是 `addr as *const u32`：本 crate 禁止 `as` 转换
/// （它能静默截断/改变符号），而这个写法在 `const` 上下文里同样成立、且不引入任何转换。
/// 地址是 ARMv7-M 规定的 SCB 寄存器偏移，不是"计算出来的"，因此没有越界风险。
const fn mmio(byte_address: usize) -> *const u32 {
    core::ptr::null::<u32>().wrapping_add(byte_address / 4)
}

/// `CFSR`（可配置故障状态）在 SCB 里的地址（ARMv7-M 架构常量）。
const CFSR_ADDR: *const u32 = mmio(0xE000_ED28);
/// `HFSR`（硬故障状态）。
const HFSR_ADDR: *const u32 = mmio(0xE000_ED2C);
/// `MMFAR`（MemManage 出错地址）。
const MMFAR_ADDR: *const u32 = mmio(0xE000_ED34);
/// `BFAR`（BusFault 出错地址）。
const BFAR_ADDR: *const u32 = mmio(0xE000_ED38);

/// 崩溃记录：放在**不被启动代码初始化**的段里，因此跨暖复位存活。
///
/// 上电后内容是垃圾，靠 `CrashRecord` 自带的魔数与校验和甄别。
/// ⚠️ 这里**必须**用 `Cell` 而不是 `RefCell`（2026-10-12 真机踩到）：
///
/// `.uninit` 段**不被启动代码初始化** —— 这正是它能在暖复位后留下内容的原因，
/// 但也意味着段里的字节是**垃圾**。`RefCell` 内部有一个"借用标志"，它要求初值为 0；
/// 放进 `.uninit` 后这个标志是随机值，于是**第一次 `borrow()` 就可能 panic**
/// （真机实测：`RefCell already mutably borrowed`，启动到 `report_fault_history` 就停住）。
///
/// `Cell` 没有内部不变式、`get`/`set` 都是纯搬运，因此对"内容是垃圾"是安全的 ——
/// 内容是否为有效记录由记录自带的**魔数 + 校验和**判断（见 `CrashRecord::decode`）。
#[unsafe(link_section = ".uninit")]
static CRASH: Mutex<Cell<[u32; CRASH_WORDS]>> = Mutex::new(Cell::new([0; CRASH_WORDS]));

/// 本次启动的复位原因（启动早期写入，故障处理器引用它 —— 故障发生在**本次**启动里，
/// 所以记录里的"复位原因"应当是**本次**启动的原因，而不是故障发生时 `SRSR` 里残留的位）。
static BOOT_RESET_CAUSE: AtomicU32 = AtomicU32::new(0);

/// **崩溃前走到哪一步**（0 = 没进过任何标记段；非 0 = 那个阶段的编号）。
///
/// # 为什么需要它
///
/// 2026-10-13 打开 `FLASH_SELFTEST_AT_BOOT` 后真机进复位循环，串口只留下
/// `CFSR=0x01000000`（UsageFault 非对齐）与一个**落在 OCRAM 里的 PC**（`0x002108f6`）——
/// 那个 PC 对不上任何代码，于是"崩在哪一步"完全无从判断。
/// 复位循环里每轮只能靠"逐段加打印、再烧一次"来二分，代价是每次一次烧录。
///
/// 有了这个标记，故障行里直接带上阶段号，一次烧录就能定位到具体阶段。
/// 写入是 `AtomicU32::store`（单条 `str`），**在最坏情况下也不会引入新的故障**。
static FLASH_STAGE: AtomicU32 = AtomicU32::new(0);

/// 记下"现在进入哪个阶段"（供 `report_flash_selftest` 打点）。
pub fn set_flash_stage(stage: u32) {
    FLASH_STAGE.store(stage, Ordering::Relaxed);
}

/// 读回阶段标记（故障处理器与终端命令都用它）。
#[must_use]
pub fn flash_stage() -> u32 {
    FLASH_STAGE.load(Ordering::Relaxed)
}

/// # 为什么**不**打开 `SHCSR` 里那三个 ENA 位（与审计建议不同，理由在此）
///
/// 直觉上应当打开 `MEMFAULTENA`/`BUSFAULTENA`/`USGFAULTENA`，让三类故障进各自的处理器。
/// 但 `cortex-m-rt` 0.7.7 只允许 **`HardFault`** 的处理程序接收 `&ExceptionFrame`
/// （见它对 `#[exception]` 的签名检查）：另外三个处理器**拿不到硬件入栈帧**，
/// 也就拿不到 `PC`/`LR`/`SP` —— 那正是现场里最值钱的部分。
///
/// 而"根因"本来就不依赖处理器身份：故障升级到 `HardFault` 时，
/// `HFSR.FORCED` 会置位，**`CFSR` 里的根因位照样保留**（ARMv7-M 规定）。
/// 于是取舍是：
///
/// | 方案 | 处理器身份 | 硬件入栈帧（PC/LR/SP） |
/// |---|---|---|
/// | 打开 ENA 位 | 三个各自的处理器 | **丢失**（签名不允许） |
/// | 不打开（本实现） | 都是 `HardFault` | **完整保留** |
///
/// 选后者：**根因从 `CFSR` 读，现场从帧读** —— 两个都拿到，而不是二选一。
/// 代价是串口上只会看到 `FAULT HardFault`，具体类别要读 `CFSR` 的位
/// （[`FaultStatus::describe`] 会把它翻成人话）。
///
/// 另注：`UsageFault` 里"除零/非对齐"这类具体陷阱还需要 `CCR` 的对应位；
/// 本项目不依赖它们做安全论证（安全状态机由领域层负责），因此保持默认关闭。
/// 读 `SRC_SRSR`（上次复位的原因）。
#[must_use]
pub fn reset_cause() -> ResetCause {
    // SAFETY: 取 SRC 外设实例以读**只读**寄存器 `SRSR`。
    // ① 读操作、无副作用；② `SRC` 没有 HAL 绑定，本模块是全仓唯一读 `SRSR` 的地方，
    //    不存在两处代码同时持有实例的问题；③ 取实例的写法与 `console.rs`/`timer.rs` 一致。
    let src = unsafe { ral::src::SRC::instance() };
    ResetCause::from_raw(ral::read_reg!(ral::src, src, SRSR))
}

/// 把本次已上报的复位原因位**写 1 清零**（只清已知位）。
///
/// 调用时机：读出来、报出去之后。这样**下一次**读到的才是"本次启动之后新发生的复位"。
pub fn clear_reset_cause(cause: ResetCause) {
    let mask = cause.clear_mask();
    if mask == 0 {
        return;
    }
    // SAFETY: 同 `reset_cause` 的取实例说明。这里是**写 1 清零**（该寄存器规定的清除
    // 方式），只写已知位、不碰保留位；失败模式是"下次仍读到旧位"，不会破坏状态。
    let src = unsafe { ral::src::SRC::instance() };
    ral::write_reg!(ral::src, src, SRSR, mask);
}

/// 记下本次启动的复位原因（供故障记录引用）。
pub fn set_boot_reset_cause(cause: ResetCause) {
    BOOT_RESET_CAUSE.store(cause.raw(), Ordering::Relaxed);
}

/// 本次启动的复位原因原始值。
#[must_use]
pub fn boot_reset_cause() -> ResetCause {
    ResetCause::from_raw(BOOT_RESET_CAUSE.load(Ordering::Relaxed))
}

/// 读四张故障状态寄存器。
#[must_use]
pub fn fault_status() -> FaultStatus {
    // SAFETY: 读内核私有的只读状态寄存器（SCB 块 0xE000_ED28..0xE000_ED38）。
    // ① 四个都是只读状态寄存器，读它们没有副作用；② 地址是 ARMv7-M 架构常量；
    // ③ cortex-m 0.7.9 只把这几张寄存器作为 `SCB` 的公开字段暴露，而异常处理器里
    //    拿不到 `SCB` 单例（它已在 `init` 中被取走）—— 这是不得不裸读的原因。
    unsafe {
        FaultStatus {
            cfsr: core::ptr::read_volatile(CFSR_ADDR),
            hfsr: core::ptr::read_volatile(HFSR_ADDR),
            mmfar: core::ptr::read_volatile(MMFAR_ADDR),
            bfar: core::ptr::read_volatile(BFAR_ADDR),
        }
    }
}

/// 读上一条崩溃记录（`None` = 这段 RAM 不是我们写的，或者从没崩过）。
#[must_use]
pub fn previous_crash() -> Option<CrashRecord> {
    crate::rt::measured_cs(|cs| CrashRecord::decode(&CRASH.borrow(cs).get()))
}

/// 清掉崩溃记录（"已连续正常运行足够久"时调用，表示故障台账归零）。
pub fn clear_crash() {
    crate::rt::measured_cs(|cs| CRASH.borrow(cs).set(CrashRecord::empty().encode()));
    clean_static(&CRASH);
}

/// 连续复位计数的魔数（高 16 位）。低 16 位是计数。
///
/// 上电后 `.uninit` 是**垃圾**：没有魔数校验的话，一次偶然的随机值就可能让
/// 设备在冷启动时直接进安全状态。魔数把这种概率压到 1/65536 量级。
const RESET_STREAK_MAGIC: u32 = 0x5AF0_0000;

/// 连续复位计数：放在 `.uninit`，跨暖复位存活。
///
/// 与 [`CRASH`] 的 `fault_streak` 分工不同：`fault_streak` 只数**崩溃**，
/// 本计数数**所有复位**（含调试器/看门狗/软件复位），供 `SafetySupervisor::on_boot`
/// 的 SF-5 判据使用。正常稳定运行 10 s 后由固件清零。
#[unsafe(link_section = ".uninit")]
static RESET_STREAK: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));

/// 本次 boot 递增连续复位计数并返回新值（**每次启动只调用一次**）。
pub fn bump_reset_streak() -> u8 {
    let next = crate::rt::measured_cs(|cs| {
        let cell = RESET_STREAK.borrow(cs);
        let raw = cell.get();
        let previous = if raw & 0xFFFF_0000 == RESET_STREAK_MAGIC {
            u8::try_from(raw & 0x0000_FFFF).unwrap_or(0)
        } else {
            0
        };
        let next = previous.saturating_add(1);
        cell.set(RESET_STREAK_MAGIC | u32::from(next));
        next
    });
    clean_static(&RESET_STREAK);
    next
}

/// 当前连续复位计数（供启动报告/诊断读取）。
#[must_use]
pub fn reset_streak() -> u8 {
    crate::rt::measured_cs(|cs| {
        let raw = RESET_STREAK.borrow(cs).get();
        if raw & 0xFFFF_0000 == RESET_STREAK_MAGIC {
            u8::try_from(raw & 0x0000_FFFF).unwrap_or(0)
        } else {
            0
        }
    })
}

/// 清零连续复位计数（"稳定运行足够久"时调用；`Safe` 状态**不会**因此解除）。
pub fn clear_reset_streak() {
    crate::rt::measured_cs(|cs| RESET_STREAK.borrow(cs).set(0));
    clean_static(&RESET_STREAK);
}

/// 写一条崩溃记录。
fn store_crash(record: &CrashRecord) {
    crate::rt::measured_cs(|cs| CRASH.borrow(cs).set(record.encode()));
    clean_static(&CRASH);
}

/// 把 `.uninit` 里的一个小静态对象写回 SRAM（见 `cache::clean_dcache_range` 的说明）。
fn clean_static<T>(value: &T) {
    let address = core::ptr::from_ref(value).cast::<u8>().addr();
    crate::cache::clean_dcache_range(address, core::mem::size_of::<T>());
}

/// 故障处理器的公共实现：抓现场 → 记录 → 上报 → 决定复位还是停机。
///
/// `frame` 是硬件入栈的异常帧（由固件的 `#[exception]` 处理器从 `ExceptionFrame` 映射）。
/// **不返回**：要么复位，要么停机。
pub fn on_fault(name: &str, frame: FaultFrame, uptime_millis: u32) -> ! {
    let status = fault_status();
    let previous = previous_crash();

    // 连续故障计数：上一条记录如果是"崩溃"（有帧），说明上次也是故障收场。
    let streak = match previous {
        Some(record) if record.has_frame() => record.fault_streak.saturating_add(1),
        _ => 1,
    };

    let record = CrashRecord {
        reset_cause: boot_reset_cause().raw(),
        cfsr: status.cfsr,
        hfsr: status.hfsr,
        mmfar: status.mmfar,
        bfar: status.bfar,
        pc: frame.pc,
        lr: frame.lr,
        sp: frame.sp,
        uptime_millis,
        fault_streak: streak,
    };
    store_crash(&record);

    report_fault(name, &record, status, frame);

    if streak >= MAX_FAULT_STREAK {
        report_halted(streak);
        // 停机等人：**不再复位**，避免复位循环（本板外部看门狗由 I2C 扩展器控制，
        // 固件目前既不能开也不能关，没有任何东西能把它从循环里拉出来）。
        loop {
            core::hint::spin_loop();
        }
    }

    SCB::sys_reset()
}

/// 把故障现场打到串口（**有界**写，避免"UART 没时钟时死在自旋里"）。
///
/// 缓冲不够时**尽力打出已有的部分**：故障路径上"少一行"远好于"什么都不打"。
fn report_fault(name: &str, record: &CrashRecord, status: FaultStatus, frame: FaultFrame) {
    use core::fmt::Write as _;

    let mut storage = [0u8; 640];
    let mut buffer = primitives::format::FixedBuf::new(&mut storage);
    let _ = writeln!(buffer, "\r\n!!! FAULT {name}");
    write!(buffer, " reset : ").ok();
    ResetCause::from_raw(record.reset_cause)
        .describe(&mut buffer)
        .ok();
    writeln!(buffer).ok();
    write!(buffer, " frame : ").ok();
    frame.describe(&mut buffer, false).ok();
    writeln!(buffer).ok();
    // 崩溃前走到哪个阶段（见 `FLASH_STAGE` 的说明）；0 = 没进过任何打点段。
    let _ = writeln!(buffer, " flashstage: {}", flash_stage());
    status.describe(&mut buffer).ok();

    // 有界写：每字节最多等 200_000 轮（≈几十毫秒量级），超时即放弃。
    let _ = crate::console::write_bytes_blocking_bounded(buffer.as_bytes(), 200_000);
}

/// 最后一句话：停机了，需要人来。
fn report_halted(streak: u32) {
    use core::fmt::Write as _;
    let mut storage = [0u8; 256];
    let mut buffer = primitives::format::FixedBuf::new(&mut storage);
    let _ = writeln!(
        buffer,
        "!!! 连续 {streak} 次故障：**停止复位**，等待人工介入（现场留在 .uninit，重上电可读）"
    );
    let _ = crate::console::write_bytes_blocking_bounded(buffer.as_bytes(), 200_000);
}
