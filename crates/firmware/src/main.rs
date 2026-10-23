//! 固件入口：RTIC 应用（组合根）。
//!
//! # 本文件只做三件事
//!
//! 1. **接线**：把 BSP 提供的硬件对象装配成任务可见的状态；
//! 2. **任务划分**：把优先级表（`primitives::budget::prio`）落到 RTIC 任务上；
//! 3. **健康门控的原子聚合**：ISR 只做 `fetch_or`，判定是主机可穷举的纯函数。
//!
//! 应用逻辑一律不在这里 —— 这是"固件层不可测、逻辑层可测"这条分界的落地方式：
//! 需要业务能力时，在另一个 crate（可放在本仓库之外）里通过 `ports`
//! 的端口接入，并用 `simulator` 的替身在主机上测试
//! （见 `docs/04-开发/测试策略.md` 与 `docs/02-架构/业务接入指南.md`）。
//!
//! # 任务与优先级（**平台需求 RT-1 / RT-2**）
//!
//! ```text
//! 1  console_rx     ← 串口接收（有界搬运）
//! 1  deadline_tick  ← 1 ms 心跳 + 健康计数
//! 0  idle           ← 终端、FS 写队列、诊断输出、喂狗决策
//! ```
//!
//! 预算与"串行总线不丢字节"的充分条件由 `primitives::budget` 的 `const_assert` 在
//! **编译期**守住：改动预算而破坏余量，构建就会失败。

#![no_std]
#![no_main]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::indexing_slicing,
    clippy::float_arithmetic
)]

// 引入 alloc：只允许**终端/诊断路径**使用（控制路径仍一律用编译期定容的 heapless 容器）。
// 分配器是 SDRAM 上的固定块静态池，见 `board_support::alloc_pool` 的模块文档。
extern crate alloc;

/// 全局分配器：SDRAM 固定块静态池。
///
/// 这里就是"引入 alloc"这件事的全部落点：池的**内存**静态可知（常量地址 + 常量大小，
/// 4 条编译期断言钉在 SDRAM 窗口内）、**无碎片**（固定块）、**失败返回空指针而不 panic**。
#[global_allocator]
static GLOBAL_POOL: board_support::alloc_pool::SdramPool = board_support::alloc_pool::SdramPool;

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use critical_section::Mutex;
use log::archive::{
    self as log_archive, INDEX_LEN, Index as ArchiveIndex, MAX_WINDOWS, PATH_LEN,
    Window as ArchiveWindow,
};
use log::{Level, TextLog};

// 强制链接：缺了这两个，"symbol not found: DefaultHandler_" / "undefined symbol: __pre_init"
// 会在链接期报出来，而错误信息不会指向"忘了 use"。
use cortex_m_rt as _;
use cortex_m_rt::exception;
use imxrt_rt as _;

use board_support::sdram::Sdram;
use board_support::{board, console};
use primitives::safety::{all_healthy, health};
use primitives::time::{Duration, Timepoint};
// `Clock` trait 必须显式在作用域内，`now()` 才可用（PitClock 实现了它）。
use ports::clock::Clock;
// `write!` 需要一个 `Write` 实现在作用域内。**模块级引入一次**，
// 不要放在函数里：函数级 `use` 会被同一函数内的其它块共享，删一处会连累别处。
use core::fmt::Write as _;

mod device;
mod version;

// 构建期生成的一致性断言（Flash 容量、存储槽位对齐、时基装载值）。
include!(concat!(env!("OUT_DIR"), "/consistency.rs"));

/// 终端行：`writeln!` 只发 `\n`（终端光标不回列首），这里显式 `\r\n`。
macro_rules! terminal_line {
    ($out:expr, $($arg:tt)*) => {{
        let _ = write!($out, $($arg)*);
        let _ = write!($out, "\r\n");
    }};
}

/// 启动横幅：内容来自**仓库根** [`banner.txt`](../../../banner.txt)，由 `build.rs`
/// 在构建期注入（改那个文本文件即可改横幅，不必动代码；`cargo` 会因文件变化重新构建）。
///
/// - 支持占位符 `{version}`：构建期替换成 `VERSION` 的当前值；
/// - 长度上限（64 行 / 4 KiB）在构建期校验（横幅属于启动突发，要留 TX 队列余量）。
const BANNER_ART: &str = include_str!(concat!(env!("OUT_DIR"), "/banner.txt"));

/// 文本日志环：64 KiB，来自 SDRAM 大块池（`Box::leak` 得到 `'static` 缓冲）。
static LOG: Mutex<RefCell<Option<TextLog>>> = Mutex::new(RefCell::new(None));
/// 当前日志级别阈值（默认 Info：Error/Warn/Info 落盘，Debug 默认关闭）。
static LOG_LEVEL: AtomicU8 = AtomicU8::new(2);
/// 是否把新日志同时输出到终端（默认关闭；`log on` 开启）。
static LOG_TERMINAL: AtomicBool = AtomicBool::new(false);
/// 被级别过滤掉的日志行数（可观测；不是静默丢弃）。
static LOG_FILTERED: AtomicU32 = AtomicU32::new(0);
#[unsafe(link_section = ".sdram_log")]
static mut LOG_BUFFER: [u8; 64 * 1024] = [0u8; 64 * 1024];

/// 取日志环缓冲（SDRAM NOLOAD；只由 idle/临界区顺序使用）。
fn log_buffer() -> &'static mut [u8] {
    // SAFETY: `.sdram_log` 是独占静态区；日志路径只在单线程顺序使用，
    // 返回的切片不会与另一份同时存在。
    unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(LOG_BUFFER).cast::<u8>(), 64 * 1024)
    }
}

/// 崩溃尾部日志的 `.uninit` 镜像容量。
///
/// `.uninit` 是 OCRAM 里不被启动代码初始化的段（本板链接脚本约 5.5 KiB），
/// 因此它能跨暖复位/看门狗复位保留最后几行文本。4 KiB 足够覆盖终端日志环的
/// 一个短窗口；掉电（冷启动）后内容无意义，由 magic 校验丢弃。
const CRASH_LOG_CAP: usize = 4 * 1024;
const CRASH_LOG_MAGIC: u32 = 0x4C4F_4731; // "LOG1"

#[repr(C, align(4))]
struct CrashLog {
    magic: u32,
    head: u32,
    used: u32,
    data: [u8; CRASH_LOG_CAP],
}

/// **跨暖复位**保留的日志尾部。
///
/// 这是对"崩溃时最后一段日志丢失"的兜底：不依赖 HardFault 能安全地碰文件系统，
/// 而是在每次 `log_event` 时把同一行同步写进 `.uninit` 环形区；fault 发生时，
/// 最后一行已经在 RAM 里，下一次启动由 `log prev` 读出。
#[unsafe(link_section = ".uninit")]
static mut CRASH_LOG: CrashLog = CrashLog {
    magic: 0,
    head: 0,
    used: 0,
    data: [0; CRASH_LOG_CAP],
};

/// 初始化/校验 `.uninit` 崩溃日志环。
///
/// 冷启动时 `.uninit` 是随机值；magic 不符就清空。暖复位保留上一次的快照。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn crash_log_init() {
    // SAFETY: 只访问本文件独占的 `.uninit` 静态；先让 D-Cache 失效，避免读到
    // 上一次复位前留在缓存里的旧行。没有并发访问者（启动单线程）。
    unsafe {
        let ptr = core::ptr::addr_of_mut!(CRASH_LOG).cast::<u8>();
        board_support::cache::invalidate_dcache_range(
            ptr as usize,
            core::mem::size_of::<CrashLog>(),
        );
        let log = &mut *core::ptr::addr_of_mut!(CRASH_LOG);
        if log.magic != CRASH_LOG_MAGIC || log.used as usize > CRASH_LOG_CAP {
            log.magic = CRASH_LOG_MAGIC;
            log.head = 0;
            log.used = 0;
            board_support::cache::clean_dcache_range(
                ptr as usize,
                core::mem::size_of::<CrashLog>(),
            );
        }
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn crash_log_write(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    // SAFETY: 同 `crash_log_init`：独占静态、单线程；写入后清理 D-Cache，
    // 保证崩溃/复位后 SRAM 里的内容是最新值。环形写入有界，不越界。
    unsafe {
        let log = &mut *core::ptr::addr_of_mut!(CRASH_LOG);
        if log.magic != CRASH_LOG_MAGIC || log.used as usize > CRASH_LOG_CAP {
            return;
        }
        let head = usize::try_from(log.head).unwrap_or(0) % CRASH_LOG_CAP;
        let take = bytes.len().min(CRASH_LOG_CAP);
        let src = bytes.get(bytes.len().saturating_sub(take)..).unwrap_or(&[]);
        let first = (CRASH_LOG_CAP - head).min(take);
        if let (Some(dst), Some(src_first)) =
            (log.data.get_mut(head..head + first), src.get(..first))
        {
            dst.copy_from_slice(src_first);
        }
        if take > first
            && let (Some(dst), Some(src_rest)) =
                (log.data.get_mut(..take - first), src.get(first..))
        {
            dst.copy_from_slice(src_rest);
        }
        log.head = u32::try_from((head + take) % CRASH_LOG_CAP).unwrap_or(0);
        log.used = u32::try_from(
            usize::try_from(log.used)
                .unwrap_or(0)
                .saturating_add(take)
                .min(CRASH_LOG_CAP),
        )
        .unwrap_or(0);
        let ptr = core::ptr::addr_of_mut!(CRASH_LOG).cast::<u8>();
        board_support::cache::clean_dcache_range(ptr as usize, core::mem::size_of::<CrashLog>());
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn crash_log_used() -> usize {
    // SAFETY: 只读本文件独占的 `.uninit` 静态；调用点在启动/命令上下文，单线程。
    unsafe {
        let log = &*core::ptr::addr_of!(CRASH_LOG);
        if log.magic == CRASH_LOG_MAGIC && log.used as usize <= CRASH_LOG_CAP {
            usize::try_from(log.used).unwrap_or(0)
        } else {
            0
        }
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn crash_log_clear() {
    // SAFETY: 同 `crash_log_init`；清空后清理缓存，避免下一次复位又读回旧值。
    unsafe {
        let log = &mut *core::ptr::addr_of_mut!(CRASH_LOG);
        log.magic = CRASH_LOG_MAGIC;
        log.head = 0;
        log.used = 0;
        let ptr = core::ptr::addr_of_mut!(CRASH_LOG).cast::<u8>();
        board_support::cache::clean_dcache_range(ptr as usize, core::mem::size_of::<CrashLog>());
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn crash_log_dump(out: &mut dyn core::fmt::Write) -> bool {
    // SAFETY: 只读独占 `.uninit` 静态；先失效缓存，保证看到复位前写回的最新内容。
    let used = unsafe {
        let ptr = core::ptr::addr_of_mut!(CRASH_LOG).cast::<u8>();
        board_support::cache::invalidate_dcache_range(
            ptr as usize,
            core::mem::size_of::<CrashLog>(),
        );
        let log = &*core::ptr::addr_of!(CRASH_LOG);
        if log.magic != CRASH_LOG_MAGIC || log.used as usize > CRASH_LOG_CAP {
            return false;
        }
        usize::try_from(log.used).unwrap_or(0)
    };
    if used == 0 {
        terminal_line!(out, "log prev : 没有上一轮崩溃日志快照");
        return true;
    }
    // SAFETY: 同上；按环形顺序逐字节读，容量已校验。
    let (head, data) = unsafe {
        let log = &*core::ptr::addr_of!(CRASH_LOG);
        (
            usize::try_from(log.head).unwrap_or(0) % CRASH_LOG_CAP,
            &log.data,
        )
    };
    let start = if used == CRASH_LOG_CAP { head } else { 0 };
    let mut line = [0u8; 256];
    let mut len = 0usize;
    for index in 0..used {
        let byte = data
            .get((start + index) % CRASH_LOG_CAP)
            .copied()
            .unwrap_or(b'\n');
        if byte == b'\n' {
            if len > 0 {
                terminal_color_log_line(out, line.get(..len).unwrap_or(&[]));
                len = 0;
            }
        } else if let Some(slot) = line.get_mut(len) {
            *slot = byte;
            len += 1;
        }
    }
    if len > 0 {
        terminal_color_log_line(out, line.get(..len).unwrap_or(&[]));
    }
    true
}

#[unsafe(link_section = ".sdram_gzip")]
static mut GZIP_INPUT: [u8; 64 * 1024] = [0u8; 64 * 1024];
#[unsafe(link_section = ".sdram_gzip")]
static mut GZIP_OUTPUT: [u8; 128 * 1024] = [0u8; 128 * 1024];

fn gzip_buffers() -> (&'static mut [u8], &'static mut [u8]) {
    // SAFETY: `.sdram_gzip` 独占；只在终端命令/idle 单线程顺序使用。
    unsafe {
        (
            core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(GZIP_INPUT).cast::<u8>(),
                64 * 1024,
            ),
            core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(GZIP_OUTPUT).cast::<u8>(),
                128 * 1024,
            ),
        )
    }
}

const GZIP_STACK_BYTES: usize = 128 * 1024;
const GZIP_STACK_PAINT: u8 = 0xA5;
const GZIP_STACK_CANARY_BYTES: usize = 16;

#[unsafe(link_section = ".sdram_stack")]
static mut GZIP_STACK: [u8; GZIP_STACK_BYTES] = [0u8; GZIP_STACK_BYTES];

static GZIP_STACK_PEAK_BYTES: AtomicU32 = AtomicU32::new(0);
/// 压缩器状态常驻：避免每次 rotation 重新分配约 85 KiB 大块。
static GZIP_STATE: Mutex<RefCell<Option<alloc::boxed::Box<gzip::CompressorOxide>>>> =
    Mutex::new(RefCell::new(None));

/// 压缩前把整段 SDRAM 栈涂成指纹，用于测峰值并检查底部 canary。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn gzip_stack_paint() {
    // SAFETY: `.sdram_stack` 是本模块独占的 SDRAM 区域；压缩前无并发使用者。
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(GZIP_STACK).cast::<u8>(),
            GZIP_STACK_PAINT,
            GZIP_STACK_BYTES,
        );
    }
}

/// 压缩后回读栈：返回是否没有越过 canary，并更新峰值。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn gzip_stack_check() -> bool {
    let base = core::ptr::addr_of_mut!(GZIP_STACK).cast::<u8>();
    // SAFETY: 同上；调用发生在切换回正常栈之后。
    unsafe {
        for offset in 0..GZIP_STACK_CANARY_BYTES {
            if *base.add(offset) != GZIP_STACK_PAINT {
                return false;
            }
        }
        let mut low = 0usize;
        while low < GZIP_STACK_BYTES && *base.add(low) == GZIP_STACK_PAINT {
            low += 1;
        }
        let used = u32::try_from(GZIP_STACK_BYTES - low).unwrap_or(u32::MAX);
        GZIP_STACK_PEAK_BYTES.fetch_max(used, Ordering::Relaxed);
    }
    true
}

fn gzip_stack_peak() -> u32 {
    GZIP_STACK_PEAK_BYTES.load(Ordering::Relaxed)
}

#[repr(C)]
struct CompressJob {
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_len: usize,
    produced: usize,
    ok: bool,
    state: *mut gzip::CompressorOxide,
}

/// 真正把 SP 切到 SDRAM 栈的 naked 调用。
#[unsafe(naked)]
unsafe extern "C" fn switch_to_sdram_stack(
    _top: u32,
    _f: unsafe extern "C" fn(*mut ()) -> u32,
    _arg: *mut (),
) -> u32 {
    core::arch::naked_asm!(
        "push {{r4, lr}}",
        "mov r4, sp",
        "mov sp, r0",
        "mov r0, r2",
        "blx r1",
        "mov sp, r4",
        "pop {{r4, pc}}",
    );
}

#[unsafe(link_section = ".xip")]
unsafe extern "C" fn gzip_compress_job(arg: *mut ()) -> u32 {
    let job_ptr = arg.cast::<CompressJob>();
    // SAFETY: 调用方保证 `arg` 指向有效 `CompressJob`，其输入/输出指向 SDRAM 独占缓冲；
    // 压缩期间无并发访问。
    let (input, output) = unsafe {
        let job = &*job_ptr;
        (
            core::slice::from_raw_parts(job.input, job.input_len),
            core::slice::from_raw_parts_mut(job.output, job.output_len),
        )
    };
    // SAFETY: `state` 由 `gzip_on_sdram_stack` 在进入裸栈前创建并保证存活。
    let state = unsafe { &mut *(*job_ptr).state };
    match gzip::compress(input, output, state) {
        Ok(len) => {
            // SAFETY: `arg` 在本次调用期间一直有效；写入的是任务结构自身的字段。
            unsafe {
                let job = &mut *job_ptr;
                job.produced = len;
                job.ok = true;
            }
            1
        }
        Err(_) => 0,
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn gzip_on_sdram_stack(input: &[u8], output: &mut [u8]) -> Option<usize> {
    let state: *mut gzip::CompressorOxide = board_support::rt::measured_cs(|cs| {
        let mut guard = GZIP_STATE.borrow(cs).borrow_mut();
        if guard.is_none() {
            *guard = Some(alloc::boxed::Box::new(gzip::new_compressor()));
        }
        guard.as_mut().map_or(core::ptr::null_mut(), |state| {
            &mut **state as *mut gzip::CompressorOxide
        })
    });
    if state.is_null() {
        return None;
    }
    let mut job = CompressJob {
        input: input.as_ptr(),
        input_len: input.len(),
        output: output.as_mut_ptr(),
        output_len: output.len(),
        produced: 0,
        ok: false,
        state,
    };
    let top = core::ptr::addr_of_mut!(GZIP_STACK).cast::<u8>() as usize + 128 * 1024 - 8;
    let arg = (&mut job as *mut CompressJob).cast::<()>();
    gzip_stack_paint();
    // SAFETY: 栈顶在 `.sdram_stack` 内且 8 字节对齐；压缩任务只访问 `job`
    // 指向的 SDRAM 缓冲，返回前恢复 MSP。中断保持开启：异常帧会落在新栈，
    // 128 KiB 足够覆盖压缩期间的嵌套中断。
    let ok = unsafe { switch_to_sdram_stack(top as u32, gzip_compress_job, arg) != 0 };
    if !gzip_stack_check() {
        return None;
    }
    if ok { Some(job.produced) } else { None }
}

static FS_MOUNT_US: AtomicU32 = AtomicU32::new(0);
static FS_MOUNT_OK: AtomicBool = AtomicBool::new(false);

/// 在 SDRAM 备用栈上挂载文件系统。
///
/// 索引快照解析会构造完整的 `FileSystem`（块视图 + 文件表），栈峰约 45 KiB；
/// 主栈只有 48 KiB，放不下。这里复用 `.sdram_stack`（128 KiB），与 gzip 不并发。
#[unsafe(link_section = ".xip")]
extern "C" fn mount_fs_job(_arg: *mut ()) -> u32 {
    let start = board_support::timer::PitClock.now().as_micros();
    let mounted = board_support::fs::mount();
    let elapsed = board_support::timer::PitClock
        .now()
        .as_micros()
        .saturating_sub(start);
    FS_MOUNT_US.store(
        u32::try_from(elapsed).unwrap_or(u32::MAX),
        Ordering::Relaxed,
    );
    match mounted {
        Ok(fs) => {
            board_support::fs::install(fs);
            FS_MOUNT_OK.store(true, Ordering::Relaxed);
            1
        }
        Err(_) => {
            FS_MOUNT_OK.store(false, Ordering::Relaxed);
            0
        }
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn mount_fs_on_sdram_stack() -> bool {
    let top = core::ptr::addr_of_mut!(GZIP_STACK).cast::<u8>() as usize + GZIP_STACK_BYTES - 8;
    gzip_stack_paint();
    // SAFETY: 栈顶在 `.sdram_stack` 内且 8 字节对齐；任务只调用 FS mount/install，
    // 返回前恢复 MSP；中断保持开启，异常帧落在 128 KiB SDRAM 栈上。
    let ok = unsafe { switch_to_sdram_stack(top as u32, mount_fs_job, core::ptr::null_mut()) != 0 };
    let stack_ok = gzip_stack_check();
    // 不用 gzip 的峰值统计误导 `log stats` / 诊断；挂载峰值单独记录。
    GZIP_STACK_PEAK_BYTES.store(0, Ordering::Relaxed);
    ok && stack_ok && FS_MOUNT_OK.load(Ordering::Relaxed)
}

static LOG_PERSISTED: AtomicU32 = AtomicU32::new(0);
static LOG_PERSIST_FAILED: AtomicU32 = AtomicU32::new(0);
/// 环内累计到该字节数才尝试落盘，避免每条小日志都产生一条独立 FS 记录。
/// 崩溃尾部由 `.uninit` 镜像兜底，因此不必"1 字节就刷"。
const LOG_FLUSH_BYTES: usize = 192;
/// 活动日志达到该字节数后，新建一个 generation 归档并清空活动文件。
const LOG_ROTATE_BYTES: u32 = 8 * 1024;
/// 归档窗口索引（64 B，一次读写完成）。索引只是缓存，窗口文件才是事实来源。
const LOG_ARCHIVE_INDEX: &str = "/log/archive.idx";
const LOG_RESTORED: &str = "/log/restored.log";
/// 启动后延迟自动 rotation，避开 boot/selftest 窗口。
const LOG_ROTATE_AFTER_MS: u64 = 30_000;
/// 旧版三窗口归档路径，按"从旧到新"顺序一次性迁移为 generation 文件。
const LOG_LEGACY_ARCHIVES: [&str; 3] =
    ["/log/archive.2.gz", "/log/archive.1.gz", "/log/archive.gz"];
/// 归档 generation 索引；只在 idle/命令上下文读写。
static ARCHIVES: Mutex<RefCell<Option<ArchiveIndex>>> = Mutex::new(RefCell::new(None));

/// 终端日志着色：只写终端，不写 Flash。按级别选色，未知行保持默认。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn terminal_color_log_line(out: &mut dyn core::fmt::Write, line: &[u8]) {
    let color = if line.windows(3).any(|w| w == b" E ") {
        "\x1b[31m"
    } else if line.windows(3).any(|w| w == b" W ") {
        "\x1b[33m"
    } else if line.windows(3).any(|w| w == b" I ") {
        "\x1b[32m"
    } else {
        ""
    };
    let _ = out.write_str(color);
    if let Ok(text) = core::str::from_utf8(line) {
        let _ = out.write_str(text);
    } else {
        let _ = out.write_str("<非 UTF-8 日志行>");
    }
    let _ = out.write_str("\x1b[0m\r\n");
}

/// 初始化文本日志状态。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn init_text_log() {
    board_support::rt::measured_cs(|cs| {
        *LOG.borrow(cs).borrow_mut() = Some(TextLog::new());
    });
}

struct LineWriter<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl LineWriter<'_> {
    fn as_bytes(&self) -> &[u8] {
        self.buffer.get(..self.len).unwrap_or(&[])
    }
}

impl core::fmt::Write for LineWriter<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let end = self.len.saturating_add(bytes.len());
        if let Some(dst) = self.buffer.get_mut(self.len..end) {
            dst.copy_from_slice(bytes);
            self.len = end;
            Ok(())
        } else {
            Err(core::fmt::Error)
        }
    }
}

/// 写一行事件日志：`[12 位毫秒] 级别 tag 正文`。
#[inline(never)]
#[unsafe(link_section = ".xip")]
#[allow(dead_code)]
fn log_event(level: char, tag: &str, body: core::fmt::Arguments<'_>) {
    let level = match level {
        'E' => Level::Error,
        'W' => Level::Warn,
        'I' => Level::Info,
        'D' => Level::Debug,
        _ => Level::Info,
    };
    let threshold = match LOG_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        _ => Level::Debug,
    };
    if !level.allowed(threshold) {
        LOG_FILTERED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut line = [0u8; 1024];
    let mut writer = LineWriter {
        buffer: &mut line,
        len: 0,
    };
    let tag = tag.get(..tag.len().min(10)).unwrap_or(tag);
    let uptime = board_support::timer::PitClock.now().as_micros() / 1000;
    let _ = write!(writer, "[{uptime:012}] {} {tag:<10} ", level.tag());
    let _ = writer.write_fmt(body);
    let text = writer.as_bytes();
    board_support::rt::measured_cs(|cs| {
        let mut guard = LOG.borrow(cs).borrow_mut();
        if let Some(log) = guard.as_mut() {
            let buffer = log_buffer();
            log.push_line(buffer, text);
        }
    });
    // 先镜像到 `.uninit`：即使随后 HardFault，最后一行也已经跨复位保住。
    crash_log_write(text);
    crash_log_write(b"\n");
    if LOG_TERMINAL.load(Ordering::Relaxed)
        && let Ok(text) = core::str::from_utf8(text)
    {
        console::write_line(text);
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn log_try_flush(force: bool) -> bool {
    let pending = board_support::rt::measured_cs(|cs| {
        LOG.borrow(cs).borrow().as_ref().map_or(0, TextLog::len)
    });
    if pending == 0 || (!force && pending < LOG_FLUSH_BYTES) {
        return false;
    }
    // 关键：**不把 FlashFs 搬上栈**。日志只投递到静态写队列，由 idle 的
    // `fs::step()` 在正常上下文执行；一次最多 192 B（队列定义的上限）。
    let mut chunk = [0u8; board_support::fs::WRITE_MAX_DATA];
    let mut submitted = 0usize;
    loop {
        let count = board_support::rt::measured_cs(|cs| {
            let guard = LOG.borrow(cs).borrow();
            let Some(log) = guard.as_ref() else {
                return 0;
            };
            let ring = log_buffer();
            log.peek_into(ring, &mut chunk)
        });
        if count == 0 {
            break;
        }
        let Some(payload) = chunk.get(..count) else {
            break;
        };
        if !board_support::fs::submit(
            board_support::fs::WriteKind::Append,
            board_support::config::files::BLACKBOX,
            payload,
        ) {
            // 队列满：保留环内数据，下一轮再试；不丢日志。
            break;
        }
        // 真正消费队首的 count 字节（submit 已拷贝成功）。
        board_support::rt::measured_cs(|cs| {
            if let Some(log) = LOG.borrow(cs).borrow_mut().as_mut() {
                let ring = log_buffer();
                if let Some(sink) = chunk.get_mut(..count) {
                    let _ = log.drain_into(ring, sink);
                }
            }
        });
        LOG_PERSISTED.fetch_add(u32::try_from(count).unwrap_or(u32::MAX), Ordering::Relaxed);
        submitted += count;
        if submitted >= pending {
            break;
        }
    }
    submitted > 0
}

// ---------------------------------------------------------------------------
// 归档 generation 索引
// ---------------------------------------------------------------------------

/// 取当前归档索引快照（Copy，不持有借用）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_snapshot() -> Option<ArchiveIndex> {
    board_support::rt::measured_cs(|cs| ARCHIVES.borrow(cs).borrow().as_ref().copied())
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_set(index: ArchiveIndex) {
    board_support::rt::measured_cs(|cs| {
        *ARCHIVES.borrow(cs).borrow_mut() = Some(index);
    });
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_file_size(path: &str) -> u32 {
    board_support::fs::with_fs_read(|fs| fs.stat(path))
        .and_then(Result::ok)
        .map_or(0, |info| info.size)
}

/// 读一个不超过 `buffer` 的小文件（归档索引 / 旧归档 / 活动日志）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_read_file(path: &str, buffer: &mut [u8]) -> Option<usize> {
    let size = archive_file_size(path);
    if size == 0 {
        return Some(0);
    }
    if usize::try_from(size).ok()? > buffer.len() {
        return None;
    }
    let mut offset = 0u32;
    while offset < size {
        let mut chunk = [0u8; 64];
        let result = board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut chunk))?;
        let Ok(read) = result else {
            return None;
        };
        if read == 0 {
            break;
        }
        let at = usize::try_from(offset).ok()?;
        let dst = buffer.get_mut(at..at.checked_add(read)?)?;
        dst.copy_from_slice(chunk.get(..read)?);
        offset = offset.checked_add(u32::try_from(read).ok()?)?;
    }
    usize::try_from(offset).ok()
}

/// 流式计算一个文件的 CRC-32（用于 `log restore` 的汇总）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_crc_file(path: &str) -> Option<u32> {
    let mut offset = 0u32;
    let mut crc = log_archive::stream_crc_init();
    loop {
        let mut chunk = [0u8; 64];
        let result = board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut chunk))?;
        let Ok(read) = result else {
            return None;
        };
        if read == 0 {
            break;
        }
        crc = log_archive::stream_crc_update(crc, chunk.get(..read)?);
        offset = offset.checked_add(u32::try_from(read).ok()?)?;
    }
    Some(primitives::crc::crc32_finish(crc))
}

/// 属主侧扫描 `/log` 下的 generation 文件（不关中断，允许慢）。
fn archive_scan_from(fs: &mut board_support::fs::FlashFs) -> ([(u32, u32); 8], usize) {
    let mut found = [(0u32, 0u32); 8];
    let mut count = 0usize;
    let _ = fs.list_dir("/log", &mut |name, size| {
        let Some(generation) = log_archive::parse_path(name) else {
            return;
        };
        if count >= found.len() {
            return;
        }
        if let Some(slot) = found.get_mut(count) {
            *slot = (generation, size.unwrap_or(0));
            count = count.saturating_add(1);
        }
    });
    if let Some(slice) = found.get_mut(..count) {
        slice.sort_by_key(|item| item.0);
    }
    (found, count)
}

/// 属主侧读索引文件：64 B 一次读完。
fn archive_load_from(fs: &mut board_support::fs::FlashFs) -> ArchiveIndex {
    let mut raw = [0u8; INDEX_LEN];
    let Ok(read) = fs.read_range(LOG_ARCHIVE_INDEX, 0, &mut raw) else {
        return ArchiveIndex::new();
    };
    if read != INDEX_LEN {
        return ArchiveIndex::new();
    }
    ArchiveIndex::decode(&raw).unwrap_or_default()
}

/// 把持久索引和目录里实际存在的窗口合并。
///
/// 这一段在**属主上下文**（`with_fs_owner`）执行：`list_dir` 对"索引说不清"的名字
/// 可能触发全盘下探，放在 `with_fs` 的关中断闭包里会直接把 `irq-off` /
/// `storage-crit` 顶爆（真机实测 3.4 ms，预算 80 µs）。属主上下文可被中断抢占，
/// 慢一点不会破坏实时预算。
fn archive_reconcile_from(
    fs: &mut board_support::fs::FlashFs,
    loaded: ArchiveIndex,
) -> ArchiveIndex {
    let mut windows = [ArchiveWindow::default(); MAX_WINDOWS + 8];
    let mut used = 0usize;
    // 先信索引并逐个 `stat`：即使名字表溢出导致 `list_dir` 漏项，已记录窗口也不会丢。
    for window in loaded.windows() {
        let mut path_buffer = [0u8; PATH_LEN];
        let Some(path) = log_archive::format_path(window.generation, &mut path_buffer) else {
            continue;
        };
        let Ok(info) = fs.stat(path) else {
            continue;
        };
        if info.size == 0 {
            continue;
        }
        let merged = if info.size == window.gzip_bytes {
            *window
        } else {
            ArchiveWindow::new(window.generation, info.size, 0, 0)
        };
        if let Some(slot) = windows.get_mut(used) {
            *slot = merged;
            used = used.saturating_add(1);
        }
    }
    // 再扫描目录补索引丢失的孤儿窗口（掉电发生在写文件之后、写索引之前）。
    let (found, count) = archive_scan_from(fs);
    for (generation, size) in found.get(..count).unwrap_or(&[]) {
        if *size == 0 || loaded.contains(*generation) {
            continue;
        }
        if let Some(slot) = windows.get_mut(used) {
            *slot = ArchiveWindow::new(*generation, *size, 0, 0);
            used = used.saturating_add(1);
        }
    }
    if let Some(slice) = windows.get_mut(..used) {
        slice.sort_by_key(|window| window.generation);
    }
    let newest = used.min(MAX_WINDOWS);
    let start = used.saturating_sub(newest);
    let max_generation = windows
        .get(..used)
        .and_then(|slice| slice.last())
        .map_or(0, |window| window.generation);
    let next = loaded
        .next_generation()
        .max(max_generation.saturating_add(1));
    ArchiveIndex::from_windows(next, windows.get(start..used).unwrap_or(&[])).unwrap_or_default()
}

/// 持久化索引。失败不 panic，但计数可见。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_persist(index: ArchiveIndex) -> bool {
    let mut raw = [0u8; INDEX_LEN];
    let Some(len) = index.encode(&mut raw) else {
        return false;
    };
    let payload = raw.get(..len).unwrap_or(&[]);
    for _ in 0..3 {
        if board_support::fs::submit(
            board_support::fs::WriteKind::Write,
            LOG_ARCHIVE_INDEX,
            payload,
        ) {
            let _ = board_support::fs::step();
            if archive_file_size(LOG_ARCHIVE_INDEX) == u32::try_from(len).unwrap_or(0) {
                return true;
            }
        }
        let _ = board_support::fs::step();
    }
    LOG_PERSIST_FAILED.fetch_add(1, Ordering::Relaxed);
    false
}

/// 显式命令/首次归档时确保索引存在：索引文件缺失时再做一次完整重建。
///
/// 启动路径不调用它（避免把终端就绪拖慢）；只有 `log stats`/`log verify`/
/// `log restore`/首次归档这类显式动作才会承担重建成本。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_ensure() -> Option<ArchiveIndex> {
    if let Some(index) = archive_snapshot() {
        return Some(index);
    }
    let index = board_support::fs::with_fs_owner(|fs| {
        let loaded = archive_load_from(fs);
        archive_reconcile_from(fs, loaded)
    })?;
    archive_set(index);
    Some(index)
}

/// 把一段压缩字节写成一个新的 generation 窗口，并更新索引。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_store_bytes(bytes: &[u8], source_bytes: u32) -> Option<ArchiveWindow> {
    let index = archive_snapshot()?;
    let generation = index.next_generation();
    let mut path_buffer = [0u8; PATH_LEN];
    let path = log_archive::format_path(generation, &mut path_buffer)?;
    let mut at = 0usize;
    // 全部用 `Append`：新路径由第一次 Append 创建成 **Log** 文件。
    // 若第一块用 `Write`，文件会是 Data 类型，紧接着的 Append 会把它当成
    // "普通文件被日志取代" 而从头重写，表现是归档丢掉前 192 字节。
    while at < bytes.len() {
        let end = at
            .saturating_add(board_support::fs::WRITE_MAX_DATA)
            .min(bytes.len());
        let payload = bytes.get(at..end)?;
        if !board_support::fs::submit(board_support::fs::WriteKind::Append, path, payload) {
            return None;
        }
        let _ = board_support::fs::step();
        at = end;
    }
    let written = archive_file_size(path);
    if written != u32::try_from(bytes.len()).ok()? {
        return None;
    }
    let window = ArchiveWindow::new(
        generation,
        written,
        source_bytes,
        log_archive::file_crc32(bytes),
    );
    let mut next = index;
    let evicted = next.push(window).ok()?;
    let _ = archive_persist(next);
    archive_set(next);
    if let Some(old) = evicted {
        let mut old_buffer = [0u8; PATH_LEN];
        if let Some(old_path) = log_archive::format_path(old.generation, &mut old_buffer) {
            let _ = board_support::fs::submit(board_support::fs::WriteKind::Remove, old_path, &[]);
            let _ = board_support::fs::step();
        }
    }
    Some(window)
}

/// 旧版三窗口里还有几个文件存在（用于 `log stats` 如实报告"待迁移"）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_legacy_pending() -> usize {
    LOG_LEGACY_ARCHIVES
        .iter()
        .filter(|path| archive_file_size(path) != 0)
        .count()
}

/// 把旧版三窗口归档一次性迁移成 generation 文件（迁移后删除旧文件）。
///
/// 迁移是唯一还会"复制归档字节"的路径；它只发生一次，之后的轮转不再复制。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_migrate_legacy() -> bool {
    let mut migrated = false;
    for source in LOG_LEGACY_ARCHIVES {
        let size = archive_file_size(source);
        if size == 0 {
            continue;
        }
        let (input, _) = gzip_buffers();
        let Some(read) = archive_read_file(source, input) else {
            return false;
        };
        let Some(bytes) = input.get(..read) else {
            return false;
        };
        if bytes.is_empty() {
            continue;
        }
        // 掉电可能发生在"复制完、删旧文件前"：先按大小 + CRC 判断它是否已经
        // 迁移过，避免下次启动重复复制。
        let crc = log_archive::file_crc32(bytes);
        if let Some(index) = archive_snapshot() {
            let size = u32::try_from(bytes.len()).unwrap_or(0);
            if index
                .windows()
                .iter()
                .any(|window| window.gzip_bytes == size && window.crc32 == crc)
            {
                let _ =
                    board_support::fs::submit(board_support::fs::WriteKind::Remove, source, &[]);
                let _ = board_support::fs::step();
                if archive_file_size(source) != 0 {
                    return false;
                }
                migrated = true;
                continue;
            }
        }
        let Some(_window) = archive_store_bytes(bytes, 0) else {
            return false;
        };
        let _ = board_support::fs::submit(board_support::fs::WriteKind::Remove, source, &[]);
        let _ = board_support::fs::step();
        if archive_file_size(source) != 0 {
            return false;
        }
        migrated = true;
    }
    if migrated && let Some(index) = archive_snapshot() {
        let _ = archive_persist(index);
    }
    true
}

/// 把源文件 gzip 成新的 generation 归档；**不复制**任何已有归档。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn archive_archive_file(source: &str) -> Option<ArchiveWindow> {
    let _ = archive_ensure()?;
    if !archive_migrate_legacy() {
        return None;
    }
    let (input, output) = gzip_buffers();
    let len = archive_read_file(source, input)?;
    if len == 0 {
        return None;
    }
    let input = input.get(..len)?;
    let source_bytes = u32::try_from(len).ok()?;
    let len = gzip_on_sdram_stack(input, output)?;
    let bytes = output.get(..len)?;
    archive_store_bytes(bytes, source_bytes)
}

/// 执行终端提交的后台维护作业。
///
/// 这些操作都不在 Input phase 里执行，避免终端输入被长操作直接卡住；
/// 它们仍在单线程 idle 内运行，因此下一步是把单个作业再分片。
pub(crate) fn execute_maintenance(job: capsules::MaintenanceJob) {
    match job {
        capsules::MaintenanceJob::FsGc => {
            let result = board_support::fs::with_fs_owner(|fs| fs.gc());
            match result {
                Some(Ok(reclaimed)) => {
                    console::write_fmt_line(format_args!("fs gc    : 后台回收 {reclaimed} 个块"))
                }
                Some(Err(error)) => {
                    console::write_fmt_line(format_args!("fs gc    : 后台失败 {error:?}"))
                }
                None => {
                    console::write_line("fs gc    : 文件系统不可用（busy / mount failed）");
                }
            }
        }
        capsules::MaintenanceJob::FsCheckpoint => {
            if board_support::fs::checkpoint_index() {
                console::write_line("fs checkpoint: 持久化索引已写入");
            } else {
                console::write_line("fs checkpoint: 写入失败，下一次启动将回退全盘扫描");
            }
        }
        capsules::MaintenanceJob::LogRotate => {
            if archive_rotate_blackbox().is_some() {
                console::write_line("log rotate: 后台归档完成，活动日志已清空");
            } else {
                console::write_line("log rotate: 后台归档失败");
            }
        }
    }
}

/// 把活动日志压缩成一个 generation 归档，并清空活动日志。
fn archive_rotate_blackbox() -> Option<u32> {
    let _ = log_try_flush(true);
    let window = archive_archive_file(board_support::config::files::BLACKBOX)?;
    if !board_support::fs::submit(
        board_support::fs::WriteKind::Write,
        board_support::config::files::BLACKBOX,
        b"\n[rotated]\n",
    ) {
        return None;
    }
    let _ = board_support::fs::step();
    Some(window.generation)
}

/// 喂狗所需的健康位集合。
///
/// 哪些任务"必须在场"由**应用**决定（**平台需求 SF-4**）。当前平台固件里只有
/// 时基与空闲两个上报者；总线接收任务落地后，应当把它们各自的位加进来，
/// 否则看门狗会在"链路静默"时继续被喂 —— 那正是要防的事。
const REQUIRED_HEALTH_BITS: u32 = health::DEADLINE_TICK | health::IDLE;

/// 1 ms 心跳累计计数。
///
/// 放在固件层而不是 BSP：它属于"并发状态"，而并发状态的所有权是组合根的职责；
/// BSP 只提供硬件（PIT）本身。
/// 稳定运行多少秒之后认为"故障台账可以归零"。
///
/// 取 10 秒的理由：它远大于"启动即崩"（那会在 1 秒内重复）与"首次总线超时"的时间尺度，
/// 又远小于现场巡检的间隔。数值本身不重要，重要的是**有人负责把它归零** ——
/// 否则"连续故障"计数只会单调上涨，最后把一次孤立的早崩误判成反复故障。
const STABLE_RUN_SECONDS: u64 = 10;

#[unsafe(link_section = ".dtcm")]
static TICKS: AtomicU32 = AtomicU32::new(0);

/// 已过的秒数（心跳编号）。即使不打印心跳也继续计数（`beat` 命令要看）。
#[unsafe(link_section = ".dtcm")]
static BEATS: AtomicU32 = AtomicU32::new(0);

/// 心跳是否**持续打印**。
///
/// 默认 `false`：进入终端后**不主动输出**，只等待输入（用户要求）。
/// `bench`/`monitor` 通过终端命令 `beat on` 显式打开；`beat` 可打印一次快照。
static BEAT_ENABLED: AtomicBool = AtomicBool::new(false);

/// 各硬实时任务的健康位聚合。
///
/// ISR 只做一次 `fetch_or`（无锁、有界），聚合与判定在空闲任务里完成。
/// 这样"谁能喂狗"这条安全策略是主机可穷举的纯函数。
#[unsafe(link_section = ".dtcm")]
pub(crate) static HEALTH: AtomicU32 = AtomicU32::new(0);

/// 把优先级表钉在代码上：这里写死 RTIC 的 `priority` 字面量，
/// 但用编译期断言确保它与 `budget::prio` 一致，避免两处漂移。
const _: () = assert!(primitives::budget::prio::DEADLINE_TICK == 1);

mod capsules;
mod platform;

#[rtic::app(device = crate::device, dispatchers = [])]
mod app {
    use super::*;

    #[shared]
    struct Shared {}

    #[local]
    struct Local {
        /// 1 ms 心跳通道句柄：中断里只需清标志。
        tick: board_support::timer::TickSource,
        /// 时基句柄（零大小，只读寄存器）。
        clock: board_support::timer::PitClock,
        /// 启动阶段耗时（由 `board::init` 收集，统一在启动报告里打印）。
        timings: board_support::board::BoardTimings,
        /// 文件系统是否挂载成功（启动报告要用；失败原因已在 `init` 里打印）。
        fs_ok: bool,
        /// 外部 SDRAM 句柄（自检通过后为 `Some`；失败时为 `None`，功能降级但设备可用）。
        sdram: Option<Sdram>,
    }

    #[init]
    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn init(mut cx: init::Context) -> (Shared, Local) {
        // `.dtcm` 是 NOLOAD；必须在 `rt::reset` 等任何高频静态量使用前清零。
        // SAFETY: 启动早期、单核、尚未使用 `.dtcm` 中任何对象。
        unsafe { board_support::stacks::zero_dtcm() };
        // MPU + cache：必须在任何大数据量访问之前（文件系统、SDRAM 都吃这个）。
        // 顺序是"先 MPU 区域、后开缓存"：缓存的内存属性完全由 MPU 决定。
        if board_support::config::MPU_AND_CACHE_ENABLED {
            board_support::mpu::init();
        }

        // 栈涂抹在 DWT 安装之前：它本身是启动期一次约 100 µs 的关中断批处理，
        // 放到仪表启动前，避免把"启动准备"误报成运行期 irq-off 违规；涂抹仍由
        // measured_cs 包住（此时 DWT 未开，只不记录样本）。
        board_support::stacks::paint();
        // 涂抹是启动准备（约 100 µs 的批量写），不计入运行期 irq-off 统计。
        board_support::rt::reset();

        // 实时性仪表：打开 DWT 周期计数器。放在**最早**处，让后续每一次关中断
        // 都被计入 —— 初始化期间的关中断窗口同样是窗口。
        //
        // 用 DWT 而不是定时器：`CYCCNT` 是核心私有计数器，读它就是一条寄存器读，
        // 不占外设、不产生中断（`DWT::cycle_count()` 是安全 API，不新增 unsafe）。
        board_support::rt::install(&mut cx.core.DCB, &mut cx.core.DWT);
        // 记下"上电窗口"的起点：RTIC 用 `interrupt::disable()` 把整个 `#[init]` 括住，
        // 因此从这里到 `idle` 第一句之间的**整段时间都没有中断被服务** —— 那是全系统
        // 最长的关中断窗口，而它不由任何 `critical_section::with` 造成，普通仪表看不见。
        board_support::rt::mark_boot_start();

        // 复位原因：读出来 → 记下（供故障记录引用）→ **写 1 清零**。
        //
        // 为什么要清：`SRSR` 的位是累积的，不清的话下次读到的仍是"上上次"的原因，
        // 于是"复位原因"这条信息永远指不到真正那次复位。
        let cause = board_support::fault::reset_cause();
        board_support::fault::set_boot_reset_cause(cause);
        board_support::fault::clear_reset_cause(cause);
        // 连续复位台账：每次启动 +1；稳定运行 10 s 后由 idle 清零。
        // 这是 SF-5 的**可观测输入**（当前只上报，不据此进入安全状态）。
        let _ = board_support::fault::bump_reset_streak();

        // 栈余量测量：在**最早**处涂抹未使用的栈区，此后任何更深的使用都会留下痕迹。
        // 放在 `board::init()` 之前，是为了让这次初始化自己的深帧也被计入。
        // SAFETY: `init` 由 RTIC 在主栈上执行一次，且此时中断未使能 ——
        // 正是"独占初始化"需要的两个前提。
        let board::Board {
            clock,
            tick,
            timings,
            sdram,
            ..
        } = unsafe { board::init(BANNER_ART, version::VERSION) };
        // banner 是阻塞输出（约 1 KiB / 115200），不应当算进 `boot-init` 的
        // 初始化预算；从板级初始化完成处重新起算，让 boot-init 只反映后续初始化。
        board_support::rt::mark_boot_start();

        // 分段打点：`install()` 之后到现在，调试域有没有被清掉。
        board_support::rt::probe_stage(1);

        // 文件系统挂载推迟到 idle：那里中断已使能，SDRAM 备用栈切换与 gzip 同路径，
        // 不再夹在 RTIC `#[init]` 的关中断上下文里。报告前会安装好。
        let fs_ok = false;

        // SDRAM：初始化失败也当场说（与文件系统同一条口径）；成功时的细节
        // （容量/实际时钟/自检结果）统一由 `idle` 的启动报告打印。
        //
        // 为什么实际时钟必须打出来：SEMC 时钟从 PLL2_PFD2 分频而来（不能改 PLL），
        // "到底跑了多少 MHz"是**读出来的**而不是写死的。
        let sdram = report_sdram(sdram);
        // 二分定位：SDRAM 自检/保持检查/1 MiB 基准这一段之后，调试域还在不在？
        board_support::rt::probe_stage(4);

        // 分段打点：板级对象装配完成（供 `report_dwt` 的启动期清除判据使用）。
        board_support::rt::probe_stage(5);
        board_support::rt::probe_stage(2);

        // ⚠️ 2026-10-12 真机实测：`install()` 后 CTRL=0x40000001/DEMCR=0x01000000（成功），
        // 但到了 idle 再读就是 **0** —— 中间某个初始化阶段把调试域清了。
        // 因此**在 `init` 末尾重新使能一次**：这是"实测驱动的修补"，待分段打点定位到具体
        // 阶段后再把根因写清楚（现阶段先保证仪表在真机上真的在测，而不是空转报 ok）。
        board_support::rt::install(&mut cx.core.DCB, &mut cx.core.DWT);
        board_support::rt::probe_stage(3);

        (
            Shared {},
            Local {
                tick,
                clock,
                timings,
                fs_ok,
                sdram,
            },
        )
    }

    /// 控制台接收：**优先级 1**。
    ///
    /// 只做"把已到达的字节搬进环形队列"。回显、执行命令、格式化全部留在 idle ——
    /// 中断里只允许有界、非阻塞的操作（与 `deadline_tick` 同一条纪律）。
    #[task(binds = LPUART1, priority = 1)]
    fn console_rx(_cx: console_rx::Context) {
        // 这条 ISR 的预算只有 20 µs（见 `realtime::BUDGET_ISR_CONSOLE_RX_MICROS`）：
        // 这块 LPUART 的接收 FIFO 只有 1 字节深，下一个字节 86.8 µs 就到。
        board_support::rt::measured(realtime::Span::IsrConsoleRx, console::isr_service);
    }

    /// 1 ms 心跳：**优先级 1**。
    ///
    /// 中断里只允许做有界的工作：清标志 + 置健康位。协议解析、状态机推进
    /// 都在空闲任务里做（`deadline_tick` 的预算见 `primitives::budget`）。
    #[task(binds = PIT, priority = 1, local = [tick])]
    fn deadline_tick(cx: deadline_tick::Context) {
        // 整个 ISR 实体都在测量闭包里：**时基中断的预算只有 20 µs**
        // （`realtime::BUDGET_ISR_DEADLINE_TICK_MICROS`），必须能实测而不是靠估。
        board_support::rt::measured(realtime::Span::IsrDeadlineTick, || {
            // 第一件事必须是清标志，否则中断会立刻重入。
            cx.local.tick.clear();
            // 清标志后再显式清一次 NVIC 挂起位。
            //
            // 为什么需要：TFLG 的清除是**外设侧写 1 清零**，而"是否再进一次中断"由 NVIC
            // 的挂起位决定。若在本次返回前挂起位已被再次锁存（清标志写尚未落到外设、
            // 或恰好跨越到期边界），处理器会立刻重入一次 —— 表现为 ISR 频率高于设计值，
            // 且频率随 ISR 体长度漂移（2026-10-09 实测 1493 Hz vs 设计 1000 Hz，
            // 加一条诊断读即回落到 991 Hz）。这里显式清挂起位把该竞态窗口关掉。
            cortex_m::peripheral::NVIC::unpend(crate::device::interrupt::PIT);
            // ISR 里只允许有界操作：一次原子自增 + 一次原子置位。
            TICKS.fetch_add(1, Ordering::Relaxed);
            HEALTH.fetch_or(health::DEADLINE_TICK, Ordering::Relaxed);
        });
    }

    // ---- 故障处理器：留下现场，再决定"复位"还是"停机等人" ----
    //
    // 审计从链接产物里读出：改之前这里指向 `HardFault_`/`DefaultHandler_`，
    // 反汇编就是 `b .` —— **空死循环**，现场只剩"串口安静"。下面四个处理器把
    // `CFSR/HFSR/MMFAR/BFAR` + 硬件入栈帧（PC/LR/SP/R0-R3/R12/xPSR）写进 `.uninit`
    // 的崩溃记录（**跨暖复位存活**），打到串口，然后按"连续故障次数"决定复位或停机。

    /// 硬故障处理器：把现场留下来，再决定"复位"还是"停机等人"。
    ///
    /// # Safety
    ///
    /// 本函数**由硬件调用**（向量表槽 3），调用者不是 Rust 代码，因此这里的"安全性义务"
    /// 落在**实现**一侧，而不是调用方：
    ///
    /// - 签名必须与 `cortex-m-rt` 对 `HardFault` 的约定一致（`unsafe fn(&ExceptionFrame) -> !`），
    ///   否则向量表会指向一个行为未定义的入口；
    /// - `frame` 由异常进入时硬件压栈，指向**当前栈顶**；本函数只读它，不修改栈；
    /// - 函数体内不得返回：故障后继续执行原上下文是未定义行为，因此返回类型是 `!`，
    ///   且实现只会走到"系统复位"或"停机循环"。
    ///
    /// 之所以标记为 `unsafe fn`：`cortex-m-rt` 要求如此 —— 硬故障处理程序在
    /// 中断/异常上下文里运行，无法用 Rust 的类型系统表达其全部前提。
    #[exception]
    unsafe fn HardFault(frame: &cortex_m_rt::ExceptionFrame) -> ! {
        fault_from_frame("HardFault", frame)
    }

    /// 把 cortex-m-rt 的异常帧映射成纯逻辑层的 [`fault_decode::FaultFrame`] 并交给 BSP。
    ///
    /// `sp` 取的是**帧自身的地址**：硬件把帧压在当前栈顶，所以 `&ExceptionFrame` 就是
    /// 故障发生时的栈指针 —— 只打 `PC` 不知道 `SP`，现场就少一半。
    fn fault_from_frame(name: &str, frame: &cortex_m_rt::ExceptionFrame) -> ! {
        let stacked = fault_decode::FaultFrame {
            r0: frame.r0(),
            r1: frame.r1(),
            r2: frame.r2(),
            r3: frame.r3(),
            r12: frame.r12(),
            lr: frame.lr(),
            pc: frame.pc(),
            xpsr: frame.xpsr(),
            sp: core::ptr::from_ref(frame) as u32,
        };
        // 运行时长取心跳计数（1 ms 一拍），异常上下文里不做任何可能阻塞的事。
        board_support::fault::on_fault(name, stacked, TICKS.load(Ordering::Relaxed))
    }

    /// 空闲任务：终端、FS 写队列、诊断输出、喂狗决策。
    #[idle(local = [clock, sdram, timings, fs_ok])]
    fn idle(cx: idle::Context) -> ! {
        idle_body(
            cx.local.clock,
            cx.local.sdram,
            *cx.local.timings,
            *cx.local.fs_ok,
        )
    }

    /// 空闲循环的实体 —— **放在 Flash 里原地执行**（`.xip` 段）。
    ///
    /// # 为什么 `idle` 可以（也应该）离开 ITCM
    ///
    /// 真机实测（`arm-none-eabi-nm --size-sort`）：`idle` 一个函数占
    /// **13,122 字节 = 22% 的 ITCM**，是整个映像里最大的符号 ——
    /// **远比所有诊断函数加起来（948 字节）大**。这条测量直接推翻了我原先
    /// "靠 `feature = "diag"` 省 ITCM"的计划：那条路只能省不到 1 KB。
    ///
    /// 而 `idle` 是**优先级 0 的任务**：任何中断、任何任务都能抢占它。
    /// 它的代码放在单周期 ITCM 里换不来任何实时性收益；反过来，把它挪出去能立刻
    /// 腾出 13 KB 给真正需要确定性取指的执行路径（ISR、调度、CAN/Flash 驱动）。
    ///
    /// # 为什么放 Flash 而不是 SDRAM
    ///
    /// `.xip` 段在链接脚本里是 `> REGION_LOAD_TEXT`（**没有** `AT>`）⇒ VMA == LMA
    /// ⇒ **原地在 Flash 中执行**：不需要拷贝、不需要改启动顺序、不依赖 SEMC/SDRAM
    /// 初始化成功、也不需要"初始化失败就降级"的分支。而 `.text` 是
    /// `> REGION_TEXT AT> REGION_LOAD_TEXT`（ITCM + 启动拷贝）。
    /// `.xip` 本来就被 `__image_size` 与 `check-size` 统计，因此不引入新的记账。
    ///
    /// # 代价与它换来的硬约束
    ///
    /// 唯一代价：Flash 擦/写期间 XIP 取指会**停等**（真机：擦 64 KiB = 571 ms）。
    /// 本项目的架构恰好把它挡在安全侧 —— **长 Flash 操作只发生在 `idle` 里**，
    /// 而 `idle` 被抢占时执行的是 ITCM 里的代码，不受影响。
    ///
    /// ⇒ 由此得到一条必须由门禁钉住的硬约束：
    /// **Flash 驱动及其调用链必须留在 ITCM**（`check-code-placement`），
    /// 否则"擦除 ⇒ 取指停等 ⇒ 擦除永远等不到下一次轮询"就成自锁。
    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn idle_body(
        clock: &mut board_support::timer::PitClock,
        sdram: &mut Option<Sdram>,
        mut timings: board_support::board::BoardTimings,
        _fs_ok: bool,
    ) -> ! {
        // 到这里 RTIC 的任务局部资源已经就绪，可以安全地放开 1 ms 心跳中断了。
        // （在 `init` 里就使能的话，ISR 可能早于资源就位而触发。）
        // 结算"上电窗口"：此刻 `post_init` 已使能中断，因此 `now - 起点` 就是
        // "上电 → 中断使能"的真实长度（真机约 3.25 s，见 `BUDGET_BOOT_INIT_MICROS`）。
        board_support::rt::finish_boot_span();
        // 启动窗口已结算并写入 boot-init；终端运行期从此刻重新起算，避免把
        // 阻塞 banner/板级初始化的一次性开销混进运行期 `rt` 结论。
        board_support::rt::reset();
        clock.start_tick();

        // 文件系统挂载：在 SDRAM 备用栈上执行，索引快照解析的栈峰不再压主栈。
        let fs_ok = mount_fs_on_sdram_stack();
        timings.filesystem_us = FS_MOUNT_US.load(Ordering::Relaxed);
        if !fs_ok {
            console::write_line("fs       : MOUNT FAILED —— 应用数据不可用");
        }

        // ---- SDRAM 上电自检（**长活，故意放在这里**）----
        //
        // 这一段要跑遍 32 MiB（真机约 3.3 s）。放在 `idle` 的**循环之前**而不是 `#[init]`：
        // `init` 全程关中断（RTIC 的 `interrupt::disable()`），3.3 s 的长活在那儿就是
        // **全系统最长的关中断窗口**；在这里中断已经使能，同样的 3.3 s 变成**可被打断**。
        //
        // 放在循环**之前**而不是循环里：`idle-step` 的预算是一个时基周期（1 ms），
        // 把它记进那一项会让"整轮"直接被顶爆；它有自己的 span（`boot-post`，
        // 启动期预算 4 s），于是"启动期到底哪段长、有多长"第一次有真数字。
        let post = run_boot_post(sdram.as_mut(), clock);

        // ---- Flash 擦写自证（默认关闭；见 config::FLASH_SELFTEST_AT_BOOT）----
        if board_support::config::FLASH_SELFTEST_AT_BOOT {
            report_flash_selftest(clock);
            run_flash_write_selftest();
        }

        // ---- 统一启动报告 ----
        // 所有启动期事实在**这里一次打印**：格式稳定、按节分组，不零散穿插在初始化路径里。
        // 探针/自证都在报告函数内部按需执行（它们的结果只在这里输出）。
        print_boot_report(timings, &post, sdram.as_ref(), fs_ok);

        // ---- 日志与归档索引初始化（必须在终端提示符之前）----
        // `archive_init` 会读 `/log/archive.idx` 并扫描目录/逐窗口 stat；在名字索引
        // 不完整时这一步可能花数秒。它以前放在提示符之后，导致"终端看起来已经
        // ready，但输入要等几秒才被处理"。现在把它放到 prompt 之前：所有长初始化
        // 完成后才宣告终端就绪，输入不再被静默卡住。
        init_text_log();
        crash_log_init();

        // ---- 终端 ----
        let mut terminal_service = capsules::TerminalService::new();
        let mut writer = console::Writer;
        terminal_service.greet(clock, sdram, &mut writer);
        log_event('I', "boot", format_args!("v={}", version::VERSION));
        log_event(
            'I',
            "boot",
            format_args!(
                "fs={} sdram={} post={}ms SDRAM={}KiB",
                u8::from(fs_ok),
                u8::from(post.ran && post.pass),
                post.elapsed_ms,
                post.bytes_kib
            ),
        );
        log_event(
            'I',
            "boot",
            format_args!("stack_free={}B", board_support::stacks::headroom_bytes()),
        );
        log_event(
            'I',
            "boot",
            format_args!(
                "reset_streak={} flash_stage={}",
                board_support::fault::reset_streak(),
                board_support::fault::flash_stage()
            ),
        );
        if let Some(record) = board_support::fault::previous_crash() {
            log_event(
                'E',
                "fault",
                format_args!(
                    "prev pc={:#010x} cfsr={:#010x} hfsr={:#010x} streak={}",
                    record.pc, record.cfsr, record.hfsr, record.fault_streak
                ),
            );
        }

        let mut fs_service = capsules::FsService::new();
        let mut log_service = capsules::LogService;
        let mut rotate_service = capsules::RotateService;
        let mut health_service = capsules::HealthService;
        let mut maintenance_service = capsules::MaintenanceService::new();
        // 产品组合根在独立 crate `product`：它决定 capsule 集合与注册顺序，
        // 本 crate 只通过 `Platform` 把服务实例借出去（见 `crates/product/src/lib.rs`）。
        let mut platform = platform::PlatformImpl {
            terminal: &mut terminal_service,
            fs: &mut fs_service,
            log: &mut log_service,
            rotate: &mut rotate_service,
            health: &mut health_service,
            maintenance: &mut maintenance_service,
        };
        let mut capsule_set = match product::build::<_, { product::CAPSULE_SLOTS }>(&mut platform) {
            Ok(product) => product,
            Err(error) => {
                // 组合根构建失败是启动期缺陷（容量不足或重名）。这里**没有安全的降级产品**：
                // 回退成空产品等于放弃终端/日志/健康聚合，设备虽然还能跑，但已经不可诊断。
                // 因此至少把失败显式打出来；为什么失败见 `error`（BuildError 带原因）。
                log_event('E', "capsule", format_args!("build_failed={error:?}"));
                sdk::Product::new(product::PRODUCT_NAME)
            }
        };
        // 启动自报：产品名与已注册 capsule 名字表（按注册顺序），写进文本日志环，
        // 终端 `log` 可读回。它是"组合根决定集合"在真机上的可核对记录 ——
        // 改顺序/改名字会在这里显形。（提示符已在此前打出，所以不走控制台以防插在提示符后。）
        log_event(
            'I',
            "capsule",
            format_args!(
                "product={} registered={}",
                product::PRODUCT_NAME,
                capsule_set.len()
            ),
        );
        for name in capsule_set.names().iter().flatten() {
            log_event('I', "capsule", format_args!("has={name}"));
        }
        let init_failed = capsule_set.init_all();
        if init_failed > 0 {
            log_event('E', "capsule", format_args!("init_failed={init_failed}"));
        }
        let mut terminal_ctx = capsules::TerminalStepContext::new(
            clock,
            sdram,
            u32::try_from(init_failed).unwrap_or(u32::MAX),
        );
        let mut last_capsule_failed = init_failed;

        let mut last_beat = clock.now();

        loop {
            // 本轮"干了活没有"。为 `false` 时下面会 `wfi` 睡到下一个中断 ——
            // 这是本循环唯一的节流点。
            //
            // # 为什么把整轮测量**只在干活期间**计时（块作用域，而不是整轮）
            //
            // `_step` 是 RAII guard：它在块结束时 Drop 并结算。把它放在这个块里，
            // 意味着**睡眠时间不计入 `idle-step`** —— 否则一次 1 ms 的 `wfi` 会把
            // "整轮耗时"直接顶到预算上限（1000 µs），让这个指标变得毫无意义。
            // 语义上也是对的：`idle-step` 量的是"这一轮干了多久活"，不是"空转了多久"。
            let did_work = {
                // 整轮 idle 的实测（RAII：`_step` 在本轮结束/提前离开时 Drop 并结算）。
                // 与 `shell-input` 的差值 = "除终端之外一轮还要花多久" —— 这是审计指出
                // 我一度丢掉的那个指标（当时用 ShellInput 取代了 IdleStep）。
                let _step = board_support::rt::scope(realtime::Span::IdleStep);
                // 本轮是否"干了活"：消费过输入 / 服务推进过 / 总线这一轮问成功了。
                // 全为假 ⇒ 下面睡到下一个中断（唯一的节流点）。
                let mut did_work = false;

                // 0) 终端输入。字节由 `console_rx` 中断搬进队列，这里只负责消费 ——
                //    因此"发送阻塞几毫秒"不会再导致输入丢失（这正是轮询方案的坑）。
                // 整轮输入处理单独计时（`shell-input`）：命令分发就在这里面，
                // "某条命令把空闲任务占住多久"要能被量出来，而不是靠感觉。
                // 终端输入阶段单独计时：真正的 `shell.feed()` 就在这一个 phase 内，
                // 不再只测 `rx_empty()` 这个空检查。
                let mut input_report = sdk::StepReport::default();
                board_support::rt::measured(realtime::Span::ShellInput, || {
                    input_report =
                        capsule_set.step_phase(sdk::CapsulePhase::Input, &mut terminal_ctx);
                });
                did_work |= input_report.worked;

                // 后台维护阶段在 IdleStep 内执行；先更新失败数，再跑 Health 阶段，
                // 这样健康聚合看到的是本轮维护结果。
                let maintenance_report =
                    capsule_set.step_phase(sdk::CapsulePhase::Maintenance, &mut terminal_ctx);
                did_work |= maintenance_report.worked;
                let failures_before_health = init_failed
                    .saturating_add(input_report.failed)
                    .saturating_add(maintenance_report.failed);
                terminal_ctx.set_capsule_failures(
                    u32::try_from(failures_before_health).unwrap_or(u32::MAX),
                );
                let health_report =
                    capsule_set.step_phase(sdk::CapsulePhase::Health, &mut terminal_ctx);
                did_work |= health_report.worked;

                let cycle_failed = failures_before_health.saturating_add(health_report.failed);
                if cycle_failed != last_capsule_failed {
                    log_event('E', "capsule", format_args!("step_failed={cycle_failed}"));
                    last_capsule_failed = cycle_failed;
                }

                // 1) 健康位：只累积、不清零 —— 清理点放在 `current_beat`（按需心跳）
                //    或未来的看门狗路径上。这样"这一窗口里谁没报到"在任意时刻都可查。
                //    `REQUIRED_HEALTH_BITS` 决定"谁必须在场"（**平台需求 SF-4**）。

                // 3) 每秒维护一次；**心跳默认不打印**（进入终端后不主动输出）。
                //    `beat on` 才开启持续输出 —— 这是给 bench/monitor 的显式开关。
                let now = clock.now();
                if now.elapsed_since(last_beat) >= Duration::from_secs(1) {
                    last_beat = now;
                    let seconds = BEATS.fetch_add(1, Ordering::Relaxed).saturating_add(1);

                    // 连续正常运行足够久 ⇒ 故障台账归零（**静默**，不打印：
                    // 终端就绪后不允许非请求输出）。
                    if u64::from(seconds) == STABLE_RUN_SECONDS {
                        if board_support::fault::previous_crash().is_some() {
                            board_support::fault::clear_crash();
                        }
                        board_support::fault::clear_reset_streak();
                    }

                    // 每次心跳检查一次周期计数器是否还活着：活着只花两次 PPB 读；
                    // 死了就**自动重装**并计数 —— 这样"仪表静默停摆"不可能再无声无息，
                    // 也不可能继续拿陈旧样本报 `rt=ok`。
                    if board_support::rt::rearm_if_stopped() {
                        let _ =
                            DWT_REARMS.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                                Some(value.saturating_add(1))
                            });
                    }

                    if BEAT_ENABLED.load(Ordering::Relaxed) {
                        report_beat(&current_beat(clock));
                    }
                }

                // 本轮"干了活没有"——块的值。
                did_work
            };
            // 4) 空闲节流：本轮什么都没做 ⇒ 睡到下一个中断。
            //
            // # 为什么不会丢唤醒
            //
            // `wfi` 在**任何**未屏蔽中断挂起时立即返回。进入这里的条件是"终端 RX 队列已空、
            // 服务没有推进、总线没有新读数"；若在此之后、执行到 `wfi` 之前来了一个串口字节，
            // `LPUART1` 的 RX 中断会**挂起**，`wfi` 立刻返回（不会睡过去）。全程不关中断，
            // 因此不存在"检查完就睡、中断被吞掉"的窗口 —— 这是 `wfi` 能安全使用的前提。
            //
            // # 为什么需要它（实测驱动）
            //
            // 审计实测：节流之前空闲循环约 **2 万轮/秒**，每轮都白跑一遍无效轮询 ——
            // 把 CPU 烧在"没人要求它做的事"上。
            // 睡眠把这一项变成"被中断叫醒才动"，轮询频率由中断驱动（1 kHz）。
            //
            // 注意：睡眠**不计入** `idle-step`（guard 在块作用域内），
            // 因此那一项量的是"这一轮干了多久活"，不是"空转了多久"。
            if !did_work {
                let _ = WFIS.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_add(1))
                });
                cortex_m::asm::wfi();
            }

            // 5) 不喂狗是**主动行为**：健康位不齐时保留现状，
            //    让外部看门狗把设备拉回已知状态。真正的喂狗实现属于 P5（看门狗启用）。
        }
    }
}

/// 空闲睡眠（`wfi`）累计次数。
///
/// 它是"节流真的生效了"的**可观测证据**：节流前空闲循环约 2 万轮/秒、从不睡眠；
/// 现在应当稳定在"时基节拍"量级（约 1000 次/秒，被 `PIT` 每秒叫醒 1000 次）。
/// 心跳行里报的是**每秒新增**（与 `txdrop=` 同一口径），因此能看出"节流有没有掉"。
/// ⚠️ 必须是 `AtomicU32`：**Cortex-M7 没有 64 位原子操作**（不过 `AtomicU64` 在这个目标上
/// 根本不存在，这一点由编译器保证，不用靠记）。每秒约 1000 次 ⇒ `u32` 可用约 49 天，
/// 而且这里每次心跳都清零，所以上限无关紧要；用 `saturating_add` 保证极端情况下也不回绕。
///
/// 顺带说明一个容易混淆的区分：`realtime` 里的 `count`/`overruns` 是 `u64`，
/// 但那是**临界区保护的普通字段**（不需要原子性，只需不回绕）；这里要的是**真原子量**，
/// 所以只能是 `u32`。两者需求不同，别互相套用。
/// 仪表被外部清掉后**自动重装**的次数。
///
/// 这个数字本身就是诊断结论：如果它长期为 0，说明"运行期会不会再被清"这件事
/// **在这块板子上没发生**；一旦非 0，就抓到了运行期清除的现场（配合 `rt=` 三态即可定位）。
static DWT_REARMS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

static WFIS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 空闲睡眠**累计**次数（心跳只报每秒增量，这里给终端查询用累计值）。
static WFIS_TOTAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 取走（并清零）自上次心跳以来的睡眠次数。
fn take_wfis() -> u32 {
    let _ = WFIS_TOTAL.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(WFIS.load(Ordering::Relaxed)))
    });
    WFIS.swap(0, Ordering::Relaxed)
}

/// 投递一条写请求，并**如实**报告受理结果。
///
/// `false` 有三种原因（队列满 / 内容超长 / 能力未就绪），这里不细分 ——
/// 统计行里 `rejected` 与 `too_long` 已经把它们分开了，调用方按需再查 `wq` 即可。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn submit_and_report(
    out: &mut dyn core::fmt::Write,
    kind: board_support::fs::WriteKind,
    name: &str,
    data: &[u8],
    stats: &dyn Fn(&mut dyn core::fmt::Write),
) -> bool {
    if board_support::fs::submit(kind, name, data) {
        terminal_line!(
            out,
            "wq       : 已入队 kind={:?} name={} bytes={}",
            kind,
            name,
            data.len()
        );
        stats(out);
        true
    } else {
        terminal_line!(
            out,
            "wq       : **未被受理**（队列满、内容超过 {} 字节、或能力未就绪）",
            board_support::fs::WRITE_MAX_DATA
        );
        stats(out);
        false
    }
}

/// 终端需要的上下文：把固件里已有的东西暴露给 `shell`。
///
/// 这里**故意只暴露数据与两个动作**（自检、重启请求），不放任何硬件细节 ——
/// 终端因此能在主机上被完整测试，而固件这一侧只剩"取值"的搬运。
struct TerminalContext<'a> {
    clock: &'a board_support::timer::PitClock,
    sdram: &'a mut Option<Sdram>,
}

impl TerminalContext<'_> {
    /// `log restore [n]`：按 generation 从旧到新恢复最近 `n` 个窗口（默认全部）。
    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn restore_windows(&mut self, newest: Option<usize>, out: &mut dyn core::fmt::Write) -> bool {
        if !archive_migrate_legacy() {
            terminal_line!(out, "log restore: 旧窗口迁移失败");
            return false;
        }
        let Some(index) = archive_ensure() else {
            terminal_line!(out, "log restore: 归档索引未就绪");
            return false;
        };
        let windows = match newest {
            Some(count) => index.select_newest(count),
            None => index.windows(),
        };
        if windows.is_empty() {
            terminal_line!(out, "log restore: 没有可恢复的归档");
            return false;
        }
        let mut total = 0u32;
        let mut restored = 0usize;
        let mut skipped = 0usize;
        for window in windows {
            let mut path_buffer = [0u8; PATH_LEN];
            let Some(path) = log_archive::format_path(window.generation, &mut path_buffer) else {
                skipped = skipped.saturating_add(1);
                continue;
            };
            let size = archive_file_size(path);
            if size == 0 {
                skipped = skipped.saturating_add(1);
                continue;
            }
            let append = total > 0;
            match decompress_file_into(path, LOG_RESTORED, append) {
                Some(part) => {
                    total = total.saturating_add(part);
                    restored = restored.saturating_add(1);
                    terminal_line!(
                        out,
                        "log restore: gen {:08x} · {} B → restored",
                        window.generation,
                        part
                    );
                }
                None => {
                    skipped = skipped.saturating_add(1);
                    terminal_line!(
                        out,
                        "log restore: gen {:08x} **FAIL** · 已跳过",
                        window.generation
                    );
                }
            }
        }
        if total == 0 {
            terminal_line!(out, "log restore: 没有成功恢复任何窗口");
            return false;
        }
        let crc = archive_crc_file(LOG_RESTORED).unwrap_or(0);
        terminal_line!(
            out,
            "log restore: {restored} 窗口 · {total} B → {LOG_RESTORED} · CRC32 {crc:#010x} · 跳过 {skipped}"
        );
        true
    }
}

impl shell::shell::ShellContext for TerminalContext<'_> {
    fn user(&self) -> &str {
        "root"
    }

    fn host(&self) -> &str {
        "tessera"
    }

    fn cwd(&self) -> &str {
        "~"
    }

    fn version(&self) -> &str {
        version::VERSION
    }

    fn uptime_ms(&self) -> u64 {
        self.clock.now().as_micros() / 1_000
    }

    fn clock_hz(&self) -> u32 {
        board_support::config::TIMEBASE_HZ
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn status_lines(&self, out: &mut dyn core::fmt::Write) {
        terminal_line!(
            out,
            "uptime   : {} s",
            self.clock.now().as_micros() / 1_000_000
        );
        terminal_line!(
            out,
            "clock    : {} Hz (PIT0+PIT1 chained)",
            board_support::config::TIMEBASE_HZ
        );
        terminal_line!(out, "console  : LPUART1 115200 8N1");
        match self.sdram.as_ref() {
            Some(sdram) => {
                terminal_line!(
                    out,
                    "sdram    : {} MiB @{:#010x} on SEMC ({} MHz)",
                    sdram.len_bytes() / (1024 * 1024),
                    sdram.base_address(),
                    board_support::sdram::semc_clock_mhz()
                );
            }
            None => {
                terminal_line!(out, "sdram    : 不可用（初始化失败，功能降级）");
            }
        }
        terminal_line!(
            out,
            "store    : FlexSPI1 XIP @0x60000000（擦写已实现：擦 4 KiB ≈35 ms、页编程 ≈85 µs）"
        );
        let _ = write!(out, "device id: ");
        board_support::ocotp::write_device_id(out);
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_snapshot(&self) -> Option<shell::shell::FsSnapshot> {
        // 普通 `fs`：只取**不读介质**的索引侧统计。
        //
        // `with_fs` 是关中断闭包；`stats()` 的存活统计会逐块逐记录读介质，主机实测
        // 接近写满时要读 28.3 MB、44.5 万次 `read`，真机折算 0.5–2 s。关中断这么久
        // 必然吃掉串口输入（LPUART RX FIFO 只有 1 字节深），因此这条路径只用
        // `stats_index_only()`；`live_bytes`/`dead_bytes` 保持"未计算"。
        board_support::fs::with_fs_read(|fs| fs_snapshot_from_stats(fs.stats_index_only()))
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_gc(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        if capsules::request_maintenance(capsules::MaintenanceJob::FsGc) {
            terminal_line!(out, "fs gc    : 已提交后台维护（结果稍后打印）");
            true
        } else {
            terminal_line!(out, "fs gc    : 维护队列忙，请稍后重试");
            false
        }
    }

    fn fs_index_loaded(&self) -> bool {
        board_support::fs::index_loaded()
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_mkdir(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        if board_support::fs::mkdir(path) {
            terminal_line!(out, "mkdir: {path}");
            true
        } else {
            terminal_line!(out, "mkdir: 失败（父目录不存在/名字非法/已存在）");
            false
        }
    }

    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn fs_rmdir(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        if board_support::fs::rmdir(path) {
            terminal_line!(out, "rmdir: {path}");
            true
        } else {
            terminal_line!(out, "rmdir: 失败（目标不是目录/目录非空/不存在）");
            false
        }
    }

    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn fs_rename(&mut self, old: &str, new: &str, out: &mut dyn core::fmt::Write) -> bool {
        if board_support::fs::rename(old, new) {
            terminal_line!(out, "rename: {old} -> {new}");
            true
        } else {
            terminal_line!(out, "rename: 失败（源不存在/目标已存在/父目录不存在）");
            false
        }
    }

    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn fs_link(&mut self, existing: &str, new: &str, out: &mut dyn core::fmt::Write) -> bool {
        if board_support::fs::link(existing, new) {
            terminal_line!(out, "ln: {existing} -> {new}");
            true
        } else {
            terminal_line!(out, "ln: 失败（源不存在/目标是目录/目标已存在）");
            false
        }
    }

    #[cfg_attr(target_os = "none", unsafe(link_section = ".xip"))]
    fn fs_unlink(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        if board_support::fs::unlink(path) {
            terminal_line!(out, "unlink: {path}");
            true
        } else {
            terminal_line!(out, "unlink: 失败（不存在/目录非空）");
            false
        }
    }

    fn fs_cancel(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let _ = capsules::request_cancel();
        terminal_line!(out, "fs cancel: 已请求取消后台维护（若当前有作业）");
        true
    }

    fn fs_checkpoint(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        if capsules::request_maintenance(capsules::MaintenanceJob::FsCheckpoint) {
            terminal_line!(out, "fs checkpoint: 已提交后台维护（结果稍后打印）");
            true
        } else {
            terminal_line!(out, "fs checkpoint: 维护队列忙，请稍后重试");
            false
        }
    }

    fn fs_scan_snapshot(&self) -> Option<shell::shell::FsSnapshot> {
        // `fs scan`：显式要求一次全盘存活统计。必须走**属主上下文**（不关中断），
        // 否则一次 `stats()` 的全盘读会变成秒级关中断窗口。属主上下文可被中断抢占，
        // 慢一点不破坏实时预算；代价是这期间 `with_fs` 读者会看到"忙"。
        board_support::fs::with_fs_owner(|fs| fs_snapshot_from_stats(fs.stats()))
    }

    fn rt_metrics(&self) -> Option<realtime::RtMetrics> {
        // 与 `fs_snapshot` 同一条纪律：**取不到就返回 `None`**，由终端照实说
        // "实测数据取不到"，绝不编造一份全零快照（全零会被读成"耗时极短、预算充裕"）。
        //
        // # 为什么这里要 `is_installed()` 这一道判断
        //
        // `board_support::rt::snapshot()` 在**周期计数器没开**时返回的是一份全零统计：
        // 那时每次测量都被跳过（BSP 刻意不记 0 周期样本），于是"一次都没测过"与
        // "测到 0 周期"在快照里长得一模一样。这两种状态对现场的含义完全不同 ——
        // 前者是"这项没有被观测"，后者是"这项快得测不出来"。`is_installed()` 能把
        // 它们分开，而 `realtime` 的表格分不开（它只看数字）。
        //
        // 正常情况下 DWT 在 `init` 里就打开了（见上面 `rt::install`），所以设备上
        // 这条分支恒为 `Some`；它防的是"将来 DWT 没开成"这种**静默降级**：
        // 那时终端会明说取不到，而不是天天打一张全零的表让人放心。
        let metrics = board_support::rt::snapshot();
        // 两个条件都要：①计数器真的在工作（`is_installed`）；②**确实有样本**。
        // ⚠️ 2026-10-12 真机教训：只判前者不够 —— 计数器没开时全部测量被跳过，
        // 快照是一张空表，而 `check()` 对空表返回 `Ok`，于是心跳打 `rt=ok`
        // —— "一次都没测过"被显示成"预算成立"。`has_samples()` 把它挡住。
        if board_support::rt::is_installed() && metrics.has_samples() {
            Some(metrics)
        } else {
            None
        }
    }

    fn memory_report(&self) -> Option<shell::MemoryReport> {
        // 与 `rt_metrics` 同一纪律：**取不到就说取不到**。这里永远取得到（池是本固件的一部分），
        // 但"取得到"与"一切正常"是两件事 —— 异常由 `has_anomaly()` 表达，终端据此改标签与退出码。
        let stats = board_support::alloc_pool::stats();
        Some(shell::MemoryReport {
            total_bytes: stats.total_bytes,
            block_bytes: stats.block_bytes,
            blocks: stats.blocks,
            used_blocks: stats.used_blocks,
            high_water_blocks: stats.high_water_blocks,
            exhaustion_failures: stats.exhaustion_failures,
            rejected_layout: stats.rejected_layout,
            rejected_releases: stats.rejected_releases,
            empty: stats.empty,
            big_block_bytes: stats.big_block_bytes,
            big_blocks: stats.big_blocks,
            big_used_blocks: stats.big_used_blocks,
            big_high_water_blocks: stats.big_high_water_blocks,
            big_rejected_layout: stats.big_rejected_layout,
            big_rejected_releases: stats.big_rejected_releases,
        })
    }

    fn instrument_health(&self) -> Option<shell::InstrumentHealth> {
        // 与 `rt_metrics` 同一纪律：**取不到就说取不到**。
        // 这里能取到，但 `dwt_alive=false` 也是有效信息（表不可信），所以照样返回 `Some`。
        Some(shell::InstrumentHealth {
            dwt_alive: board_support::rt::is_installed(),
            dwt_rearms: DWT_REARMS.load(Ordering::Relaxed),
            sleeps: u64::from(WFIS_TOTAL.load(Ordering::Relaxed)),
        })
    }

    fn rt_reset(&mut self) -> bool {
        // 终端只是**发起**复位，真正的清空在 BSP 里做（终端是纯逻辑层，碰不到它）。
        // 复位本身没有失败路径（`RtMetrics::reset` 只是覆盖统计、保留预算），
        // 因此这里恒返回 `true`（受理）。返回 `bool` 是给**别**的实现留的诚实出口：
        // 默认实现返回 `false`，`rt reset` 就会明说"本实现不支持"，而不是假装成功。
        board_support::rt::reset();
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn can_status(&self, out: &mut dyn core::fmt::Write) -> bool {
        // 只读探针：不写任何寄存器（与启动横幅同一份读数，读法在 `board_support::flexcan`）。
        // 这条命令的价值是"现场不必重启就能看总线控制器现在长什么样"。
        let probe = board_support::flexcan::probe();
        terminal_line!(
            out,
            "can      : mcr={:#010x} ctrl1={:#010x} ecr={:#010x} esr1={:#010x} iflag1={:#010x}",
            probe.mcr,
            probe.ctrl1,
            probe.ecr,
            probe.esr1,
            probe.iflag1
        );
        terminal_line!(
            out,
            "           PE 时钟={} Hz 实际波特率={} bps（由 CTRL1 与 CSCMR2[CAN_CLK_PODF] 算出）",
            board_support::flexcan::pe_clock_hz(),
            board_support::flexcan::actual_baud()
        );
        terminal_line!(
            out,
            "           存活={} frz_ack={} 邮箱={} rxerr={} txerr={} fltconf={} busoff={} mbflag={}",
            if board_support::flexcan::looks_alive(&probe) {
                "是"
            } else {
                "否（先查 CCM 模块门控，不要先写序列）"
            },
            probe.in_freeze(),
            probe.mailboxes(),
            probe.rx_error_count(),
            probe.tx_error_count(),
            probe.fault_confinement(),
            u8::from(probe.bus_off()),
            u8::from(probe.any_mailbox_flag())
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn run_memtest(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let Some(sdram) = self.sdram.as_mut() else {
            terminal_line!(out, "sdram 不可用，无法自检");
            return false;
        };
        use sdram_timing::memtest as mt;
        let address = mt::address_bus_test(sdram);
        let data = mt::data_bus_test(sdram);
        let pattern = mt::pattern_test(sdram, 0xAAAA_AAAA);
        terminal_line!(
            out,
            "地址线 : {} (错误 {})",
            if address.passed() { "PASS" } else { "FAIL" },
            address.errors
        );
        terminal_line!(
            out,
            "数据线 : {} (错误 {})",
            if data.passed() { "PASS" } else { "FAIL" },
            data.errors
        );
        terminal_line!(
            out,
            "图样   : {} (错误 {}，扫描 {} KiB)",
            if pattern.passed() { "PASS" } else { "FAIL" },
            pattern.errors,
            pattern.bytes_tested / 1024
        );
        if let Some(first) = address
            .first_error
            .or(data.first_error)
            .or(pattern.first_error)
        {
            terminal_line!(
                out,
                "首个错误 @+{:#x}: 期望 {:#010x} 实读 {:#010x}",
                first.offset,
                first.expected,
                first.actual
            );
        }
        address.passed() && data.passed() && pattern.passed()
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn audit_command(&mut self, line: &str) {
        let line = line.trim();
        if !line.is_empty() {
            log_event('I', "cmd", format_args!("{line}"));
        }
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_verify(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        if !archive_migrate_legacy() {
            terminal_line!(out, "log verify: 旧窗口迁移失败");
            return false;
        }
        let Some(index) = archive_ensure() else {
            terminal_line!(out, "log verify: 归档索引未就绪");
            return false;
        };
        if index.is_empty() {
            terminal_line!(out, "log verify: 没有归档窗口");
            return true;
        }
        let mut all_ok = true;
        let mut total = 0u32;
        let mut summary_crc = log_archive::stream_crc_init();
        for window in index.windows() {
            let mut path_buffer = [0u8; PATH_LEN];
            let Some(path) = log_archive::format_path(window.generation, &mut path_buffer) else {
                all_ok = false;
                continue;
            };
            let size = archive_file_size(path);
            if size != window.gzip_bytes {
                terminal_line!(
                    out,
                    "log verify: gen {:08x} **FAIL** · 大小 {} B ≠ 索引 {} B",
                    window.generation,
                    size,
                    window.gzip_bytes
                );
                all_ok = false;
                continue;
            }
            match verify_gzip_file(path, summary_crc) {
                Some(verified) => {
                    summary_crc = verified.data_crc;
                    total = total.saturating_add(verified.bytes);
                    let index_ok = window.crc32 == 0 || window.crc32 == verified.file_crc;
                    if !index_ok {
                        terminal_line!(
                            out,
                            "log verify: gen {:08x} **FAIL** · CRC {} ≠ 索引 {:#010x}",
                            window.generation,
                            verified.file_crc,
                            window.crc32
                        );
                        all_ok = false;
                    } else {
                        terminal_line!(
                            out,
                            "log verify: gen {:08x} OK · {} B · crc {:#010x}",
                            window.generation,
                            verified.bytes,
                            verified.window_crc
                        );
                    }
                }
                None => {
                    terminal_line!(
                        out,
                        "log verify: gen {:08x} **FAIL** · gzip 损坏/解压失败",
                        window.generation
                    );
                    all_ok = false;
                }
            }
        }
        terminal_line!(
            out,
            "log verify: 汇总 {} B · CRC32 {:#010x}",
            total,
            primitives::crc::crc32_finish(summary_crc)
        );
        all_ok
    }

    fn selftest(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let ok = run_selftest(out);
        log_event(
            'I',
            "selftest",
            format_args!("{}", if ok { "ok" } else { "FAIL" }),
        );
        ok
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_set_terminal(&mut self, enabled: bool, out: &mut dyn core::fmt::Write) -> bool {
        LOG_TERMINAL.store(enabled, Ordering::Relaxed);
        terminal_line!(
            out,
            "log on/off: 终端日志输出已{}",
            if enabled { "开启" } else { "关闭" }
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_set_level(&mut self, level: &str, out: &mut dyn core::fmt::Write) -> bool {
        if level.is_empty() {
            let current = match LOG_LEVEL.load(Ordering::Relaxed) {
                0 => Level::Error,
                1 => Level::Warn,
                2 => Level::Info,
                _ => Level::Debug,
            };
            terminal_line!(
                out,
                "log level: {} ({}) · filtered {} 行",
                current.tag(),
                match current {
                    Level::Error => "error",
                    Level::Warn => "warn",
                    Level::Info => "info",
                    Level::Debug => "debug",
                },
                LOG_FILTERED.load(Ordering::Relaxed)
            );
            return true;
        }
        let Some(next) = Level::parse(level) else {
            return false;
        };
        LOG_LEVEL.store(next.rank(), Ordering::Relaxed);
        terminal_line!(
            out,
            "log level: {} ({})",
            next.tag(),
            match next {
                Level::Error => "error",
                Level::Warn => "warn",
                Level::Info => "info",
                Level::Debug => "debug",
            }
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_previous(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        crash_log_dump(out)
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_stats(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let stats = board_support::rt::measured_cs(|cs| {
            LOG.borrow(cs).borrow().as_ref().map(TextLog::stats)
        });
        let Some(stats) = stats else {
            return false;
        };
        let active =
            board_support::fs::with_fs_read(|fs| fs.stat(board_support::config::files::BLACKBOX))
                .and_then(Result::ok)
                .map_or(0, |info| info.size);
        terminal_line!(
            out,
            "log      : ring {} B · dropped_lines {} · truncated {} B",
            stats.bytes,
            stats.dropped_lines,
            stats.truncated_bytes
        );
        let threshold = match LOG_LEVEL.load(Ordering::Relaxed) {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            _ => Level::Debug,
        };
        terminal_line!(
            out,
            "           level {} · terminal {} · filtered {} · crash_prev {} B",
            threshold.tag(),
            if LOG_TERMINAL.load(Ordering::Relaxed) {
                "on"
            } else {
                "off"
            },
            LOG_FILTERED.load(Ordering::Relaxed),
            crash_log_used()
        );
        terminal_line!(
            out,
            "           persisted {} B · active {} B · persist_failed {}",
            LOG_PERSISTED.load(Ordering::Relaxed),
            active,
            LOG_PERSIST_FAILED.load(Ordering::Relaxed)
        );
        let legacy = archive_legacy_pending();
        if legacy > 0 {
            terminal_line!(
                out,
                "           legacy   : {legacy} 个旧窗口待迁移（首次归档/恢复时迁移）"
            );
        }
        match archive_ensure() {
            Some(index) => {
                let gzip_total = index
                    .windows()
                    .iter()
                    .map(|window| window.gzip_bytes)
                    .fold(0u32, u32::saturating_add);
                let source_total = index
                    .windows()
                    .iter()
                    .map(|window| window.source_bytes)
                    .fold(0u32, u32::saturating_add);
                terminal_line!(
                    out,
                    "           archives {} 窗口 · {} B(gz) / {} B(源) · next gen {:08x}",
                    index.count(),
                    gzip_total,
                    source_total,
                    index.next_generation()
                );
                for window in index.windows() {
                    terminal_line!(
                        out,
                        "             gen {:08x} · {} B gz · {} B src · crc {:#010x}",
                        window.generation,
                        window.gzip_bytes,
                        window.source_bytes,
                        window.crc32
                    );
                }
            }
            None => terminal_line!(out, "           archives 索引未就绪（文件系统不可用）"),
        }
        terminal_line!(
            out,
            "           gzip stack peak {} B / {} B",
            gzip_stack_peak(),
            GZIP_STACK_BYTES
        );
        true
    }

    fn log_flush(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        let submitted = log_try_flush(true);
        let _ = board_support::fs::step();
        terminal_line!(
            out,
            "log flush: 队列已推进（本次提交={}）",
            u8::from(submitted)
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_clear(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        // 真实清空：删除活动日志文件（墓碑），再跑一轮 GC 回收它的死记录。
        // 不擦含其它活数据的块；GC 只擦确认没有活记录的块，因此不会破坏归档/配置。
        let result = board_support::fs::with_fs_owner(|fs| {
            let _ = fs.remove(board_support::config::files::BLACKBOX);
            fs.gc()
        });
        let Some(result) = result else {
            terminal_line!(out, "log clear: 文件系统不可用（busy / mount failed）");
            return false;
        };
        let reclaimed = match result {
            Ok(count) => count,
            Err(error) => {
                terminal_line!(out, "log clear: 删除/GC 失败 {error:?}");
                return false;
            }
        };
        board_support::rt::measured_cs(|cs| {
            *LOG.borrow(cs).borrow_mut() = Some(TextLog::new());
        });
        crash_log_clear();
        LOG_PERSISTED.store(0, Ordering::Relaxed);
        LOG_PERSIST_FAILED.store(0, Ordering::Relaxed);
        terminal_line!(
            out,
            "log clear: 活动日志已删除 · GC 回收 {reclaimed} 块 · 归档保留"
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn tail_file(&mut self, path: &str, max_lines: usize, out: &mut dyn core::fmt::Write) -> bool {
        let size = board_support::fs::with_fs_read(|fs| fs.stat(path))
            .and_then(Result::ok)
            .map_or(0, |info| info.size);
        if size == 0 {
            return false;
        }
        let take = size.min(512);
        let mut start = size - take;
        let mut data = [0u8; 512];
        let base = start;
        while start < size {
            let mut chunk = [0u8; 64];
            let Some(result) =
                board_support::fs::with_fs_read(|fs| fs.read_range(path, start, &mut chunk))
            else {
                return false;
            };
            let Ok(read) = result else {
                return false;
            };
            if read == 0 {
                break;
            }
            let at = usize::try_from(start - base).unwrap_or(0);
            let Some(dst) = data.get_mut(at..at.saturating_add(read)) else {
                return false;
            };
            dst.copy_from_slice(chunk.get(..read).unwrap_or(&[]));
            start = start.saturating_add(u32::try_from(read).unwrap_or(0));
        }
        let used = usize::try_from(start - base).unwrap_or(0);
        let slice = data.get(..used).unwrap_or(&[]);
        // 只保留最后一个换行之后的完整行集合；不足一屏时从头开始。
        let mut line_start = 0usize;
        for (index, byte) in slice.iter().enumerate() {
            if *byte == b'\n' {
                line_start = index.saturating_add(1);
            }
        }
        let _ = line_start;
        // 输出最后 20 行。
        let mut starts = [0usize; 21];
        let mut count = 0usize;
        if let Some(slot) = starts.get_mut(0) {
            *slot = 0;
        }
        for (index, byte) in slice.iter().enumerate() {
            if *byte == b'\n' && count + 1 < starts.len() {
                count += 1;
                if let Some(slot) = starts.get_mut(count) {
                    *slot = index.saturating_add(1);
                }
            }
        }
        let first = count.saturating_sub(max_lines);
        for line in 0..count {
            if line < first {
                continue;
            }
            let a = starts.get(line).copied().unwrap_or(0);
            let b = starts
                .get(line.saturating_add(1))
                .copied()
                .unwrap_or(slice.len());
            let mut text = slice.get(a..b).unwrap_or(&[]);
            if text.last() == Some(&b'\n') {
                text = text.get(..text.len().saturating_sub(1)).unwrap_or(&[]);
            }
            if path.ends_with(".log") {
                terminal_color_log_line(out, text);
            } else if let Ok(text) = core::str::from_utf8(text) {
                let _ = out.write_str(text);
                let _ = out.write_str("\r\n");
            }
        }
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn gzip_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let Some(window) = archive_archive_file(path) else {
            return false;
        };
        terminal_line!(
            out,
            "gzip     : {} B → gen {:08x} · {} B gz",
            window.source_bytes,
            window.generation,
            window.gzip_bytes
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn head_file(&mut self, path: &str, max_lines: usize, out: &mut dyn core::fmt::Write) -> bool {
        let size = board_support::fs::with_fs_read(|fs| fs.stat(path))
            .and_then(Result::ok)
            .map_or(0, |info| info.size);
        if size == 0 {
            return false;
        }
        let take = size.min(512);
        let mut data = [0u8; 512];
        let mut offset = 0u32;
        while offset < take {
            let mut chunk = [0u8; 64];
            let Some(result) =
                board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut chunk))
            else {
                return false;
            };
            let Ok(read) = result else {
                return false;
            };
            if read == 0 {
                break;
            }
            let at = usize::try_from(offset).unwrap_or(0);
            let Some(dst) = data.get_mut(at..at.saturating_add(read)) else {
                return false;
            };
            dst.copy_from_slice(chunk.get(..read).unwrap_or(&[]));
            offset = offset.saturating_add(u32::try_from(read).unwrap_or(0));
        }
        let slice = data
            .get(..usize::try_from(offset).unwrap_or(0))
            .unwrap_or(&[]);
        let mut emitted = 0usize;
        let mut start = 0usize;
        for (index, byte) in slice.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let text = slice.get(start..index).unwrap_or(&[]);
            if path.ends_with(".log") {
                terminal_color_log_line(out, text);
            } else if let Ok(text) = core::str::from_utf8(text) {
                let _ = out.write_str(text);
                let _ = out.write_str("\r\n");
            }
            emitted += 1;
            start = index.saturating_add(1);
            if emitted >= max_lines {
                break;
            }
        }
        if emitted < max_lines {
            let text = slice.get(start..).unwrap_or(&[]);
            if path.ends_with(".log") {
                terminal_color_log_line(out, text);
            } else if let Ok(text) = core::str::from_utf8(text) {
                let _ = out.write_str(text);
                let _ = out.write_str("\r\n");
            }
        }
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_rotate(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        if capsules::request_maintenance(capsules::MaintenanceJob::LogRotate) {
            terminal_line!(out, "log rotate: 已提交后台维护（结果稍后打印）");
            true
        } else {
            terminal_line!(out, "log rotate: 维护队列忙，请稍后重试");
            false
        }
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_grep(&mut self, pattern: &str, out: &mut dyn core::fmt::Write) -> bool {
        let needle = pattern.as_bytes();
        let mut offset = 0u32;
        let mut line = [0u8; 256];
        let mut used = 0usize;
        let mut matched = 0u32;
        loop {
            let mut chunk = [0u8; 64];
            let Some(result) = board_support::fs::with_fs_read(|fs| {
                fs.read_range(board_support::config::files::BLACKBOX, offset, &mut chunk)
            }) else {
                return false;
            };
            let Ok(read) = result else {
                return false;
            };
            if read == 0 {
                break;
            }
            for byte in chunk.get(..read).unwrap_or(&[]) {
                if *byte == b'\n' {
                    if used > 0 {
                        let text = line.get(..used).unwrap_or(&[]);
                        if needle.is_empty() || text.windows(needle.len()).any(|w| w == needle) {
                            terminal_color_log_line(out, text);
                            matched = matched.saturating_add(1);
                        }
                    }
                    used = 0;
                } else if let Some(slot) = line.get_mut(used) {
                    *slot = *byte;
                    used += 1;
                }
            }
            offset = offset.saturating_add(u32::try_from(read).unwrap_or(0));
        }
        if used > 0 {
            let text = line.get(..used).unwrap_or(&[]);
            if needle.is_empty() || text.windows(needle.len()).any(|w| w == needle) {
                terminal_color_log_line(out, text);
                matched = matched.saturating_add(1);
            }
        }
        terminal_line!(out, "log grep: {matched} 行匹配");
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn wc_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let mut offset = 0u32;
        let mut bytes = 0u32;
        let mut lines = 0u32;
        loop {
            let mut chunk = [0u8; 64];
            let Some(result) =
                board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut chunk))
            else {
                return false;
            };
            let Ok(read) = result else {
                return false;
            };
            if read == 0 {
                break;
            }
            for byte in chunk.get(..read).unwrap_or(&[]) {
                if *byte == b'\n' {
                    lines = lines.saturating_add(1);
                }
            }
            let add = u32::try_from(read).unwrap_or(0);
            bytes = bytes.saturating_add(add);
            offset = offset.saturating_add(add);
        }
        terminal_line!(out, "wc       : {lines} lines · {bytes} bytes");
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn gunzip_file(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let Some(total) = decompress_file_into(path, "/log/unpacked.log", false) else {
            return false;
        };
        terminal_line!(out, "gunzip   : {total} B → /log/unpacked.log");
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn log_restore(&mut self, out: &mut dyn core::fmt::Write) -> bool {
        self.restore_windows(None, out)
    }

    fn log_restore_count(&mut self, count: Option<usize>, out: &mut dyn core::fmt::Write) -> bool {
        self.restore_windows(count, out)
    }

    fn fs_is_dir(&mut self, path: &str) -> bool {
        if path == "/" {
            return true;
        }
        let mut found = false;
        let result =
            board_support::fs::with_fs_read(|fs| fs.list_dir(path, &mut |_, _| found = true));
        matches!(result, Some(Ok(()))) && found
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_list(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let result = board_support::fs::with_fs_read(|fs| {
            fs.list_dir(path, &mut |name, size| match size {
                Some(bytes) => {
                    terminal_line!(out, "\x1b[36m{name}\x1b[0m  \x1b[2m{bytes} B\x1b[0m");
                }
                None => {
                    terminal_line!(out, "\x1b[34m{name}/\x1b[0m");
                }
            })
        });
        matches!(result, Some(Ok(())))
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_cat(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        // `.log` 文件按**文本**渲染：时间戳暗色、按级别着色；ANSI 只到终端，
        // Flash 内容保持纯文本。非 .log 仍走十六进制+ASCII 视图。
        if path.ends_with(".log") {
            let mut offset = 0u32;
            let mut line = [0u8; 256];
            let mut used = 0usize;
            loop {
                let mut chunk = [0u8; 64];
                let result =
                    board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut chunk));
                let read = match result {
                    Some(Ok(n)) => n,
                    _ => return false,
                };
                if read == 0 {
                    break;
                }
                for byte in chunk.get(..read).unwrap_or(&[]) {
                    if *byte == b'\n' {
                        if used > 0
                            && let Some(text) = line.get(..used)
                        {
                            terminal_color_log_line(out, text);
                        }
                        used = 0;
                    } else if let Some(slot) = line.get_mut(used) {
                        *slot = *byte;
                        used += 1;
                    }
                }
                offset = offset.saturating_add(u32::try_from(read).unwrap_or(0));
            }
            if used > 0
                && let Some(text) = line.get(..used)
            {
                terminal_color_log_line(out, text);
            }
            return offset > 0;
        }
        let mut offset = 0u32;
        loop {
            let mut buffer = [0u8; 64];
            let result =
                board_support::fs::with_fs_read(|fs| fs.read_range(path, offset, &mut buffer));
            let read = match result {
                Some(Ok(read)) => read,
                _ => return false,
            };
            if read == 0 {
                return offset > 0;
            }
            for (row, chunk) in buffer.get(..read).unwrap_or(&[]).chunks(16).enumerate() {
                let row_offset =
                    offset.saturating_add(u32::try_from(row.saturating_mul(16)).unwrap_or(0));
                let _ = write!(out, "\x1b[2m{row_offset:08x}\x1b[0m: ");
                for index in 0..16 {
                    match chunk.get(index) {
                        Some(byte) => {
                            let _ = write!(out, "{byte:02x} ");
                        }
                        None => {
                            let _ = write!(out, "   ");
                        }
                    }
                }
                let _ = write!(out, "\x1b[2m|\x1b[0m");
                for byte in chunk {
                    let shown = if byte.is_ascii_graphic() || *byte == b' ' {
                        char::from(*byte)
                    } else {
                        '.'
                    };
                    let _ = write!(out, "{shown}");
                }
                let _ = out.write_str("\x1b[0m\r\n");
            }
            offset = offset.saturating_add(u32::try_from(read).unwrap_or(0));
        }
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_stat(&mut self, path: &str, out: &mut dyn core::fmt::Write) -> bool {
        let result = board_support::fs::with_fs_read(|fs| fs.stat(path));
        let Some(Ok(info)) = result else {
            return false;
        };
        let kind = match info.kind {
            board_support::fs::FileKind::Data => "file",
            board_support::fs::FileKind::Log => "log",
        };
        terminal_line!(
            out,
            "\x1b[36m{path}\x1b[0m: \x1b[32m{kind}\x1b[0m, \x1b[2m{} B\x1b[0m",
            info.size
        );
        true
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn fs_write_queue(
        &mut self,
        action: shell::commands::FsWriteAction<'_>,
        out: &mut dyn core::fmt::Write,
    ) -> bool {
        use board_support::fs::{self, QueueStats, WriteKind};
        use shell::commands::FsWriteAction;

        // 统计行：四个计数分开报，**失败与被拒必须可见**。
        let write_stats = |out: &mut dyn core::fmt::Write| {
            let QueueStats {
                pending,
                submitted,
                executed,
                failed,
                rejected,
                too_long,
            } = fs::queue_stats();
            terminal_line!(out, "wq       : pending={pending} submitted={submitted}");
            terminal_line!(
                out,
                "           executed={executed} failed={failed} rejected={rejected} too_long={too_long}"
            );
            terminal_line!(
                out,
                "           执行者是 idle 循环；投递只入队，不代表已经落盘（擦一块约几十毫秒）"
            );
        };

        match action {
            FsWriteAction::Stats => {
                write_stats(out);
                true
            }
            FsWriteAction::Write { name, data } => {
                submit_and_report(out, WriteKind::Write, name, data, &write_stats)
            }
            FsWriteAction::Append { name, data } => {
                if let Ok(text) = core::str::from_utf8(data)
                    && let Some(source) = text.strip_prefix('@')
                {
                    if source == name {
                        terminal_line!(out, "wq       : 源与目标相同，拒绝");
                        return false;
                    }
                    return match append_file_chunks(source, name) {
                        Some(total) => {
                            terminal_line!(out, "wq       : 已批量追加 {total} B → {name}");
                            write_stats(out);
                            true
                        }
                        None => {
                            terminal_line!(out, "wq       : 批量追加失败（读取/入队）");
                            false
                        }
                    };
                }
                submit_and_report(out, WriteKind::Append, name, data, &write_stats)
            }
            FsWriteAction::Remove { name } => {
                submit_and_report(out, WriteKind::Remove, name, &[], &write_stats)
            }
        }
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn request_reboot(&mut self) {
        // 先把日志队列推空，再写持久化索引；这样重启后能走快速挂载路径。
        // checkpoint 是 64 KiB 块擦写，发生在终端命令上下文，不在实时热路径。
        let _ = log_try_flush(true);
        let _ = board_support::fs::step();
        let _ = board_support::fs::checkpoint_index();
        // 用 Cortex-M 的 SYSRESETREQ 请求整机复位；实际复位由芯片完成，
        // 这里不"模拟"重启 —— 要么真的复位，要么什么也不做。
        cortex_m::peripheral::SCB::sys_reset();
    }

    fn beat_control(
        &mut self,
        action: shell::commands::BeatAction,
        out: &mut dyn core::fmt::Write,
    ) -> bool {
        use shell::commands::BeatAction;
        match action {
            BeatAction::On => {
                BEAT_ENABLED.store(true, Ordering::Relaxed);
                terminal_line!(out, "beat     : 已开启（每秒一行；`beat off` 关闭）");
                report_beat(&current_beat(self.clock));
            }
            BeatAction::Off => {
                BEAT_ENABLED.store(false, Ordering::Relaxed);
                terminal_line!(out, "beat     : 已关闭（默认；`beat` 打印一次快照）");
            }
            BeatAction::Now => report_beat(&current_beat(self.clock)),
        }
        true
    }

    fn diag_lines(&self, _out: &mut dyn core::fmt::Write) -> bool {
        dump_pit(self.clock);
        dump_peripheral_regs();
        dump_mpu_regions();
        dump_dwt_detail();
        true
    }
}

/// 报告 SDRAM 状态，并在配置允许时做上电自检（POST）。
///
/// 返回可用的 SDRAM 句柄（初始化或自检失败时为 `None`）。
///
/// 取舍：SDRAM **不在启动链路上**，所以这里绝不 panic —— 失败就降级，
/// 让设备仍能提供其余功能，并把原因留在串口上。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_sdram(result: Result<Sdram, board_support::sdram::SdramError>) -> Option<Sdram> {
    match result {
        Err(err) => {
            // 失败是罕见路径，允许"早说"（不能等报告：设备已经降级）。
            console::write_fmt_line(format_args!(
                "sdram    : INIT FAILED: {err:?}（功能降级，设备继续运行）"
            ));
            None
        }
        Ok(sdram) => Some(sdram),
    }
}

/// FlexSPI / FlexCAN 的**原始寄存器探针**（诊断命令 `diag` 用；只读）。
///
/// 它证明的是整条链是否通：寄存器访问 → `IPCR1` 打包 → `IPCMD` 触发 →
/// RX FIFO 读取 → **`INTR` 解码**。读器件状态不改任何状态，所以零风险。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn dump_peripheral_regs() {
    use board_support::flexspi_ip;
    use nor_flash::flexspi::{Command, seq};

    // ---- 只读探针：`FLSHCR2` 的 AHB 读/写序列号 ----
    // 官方 `flexspi_config_flash_control_registers` 会显式设 `ARDSEQID=0`（读）、
    // `AWRSEQID=9`（写），而我们从没设过。这条只读探针把"控制器此刻到底指向哪条序列"
    // 变成事实 —— 若 `AWRSEQID` 不是 9，写相位就会走一条我们不认识的序列。
    let awr = flexspi_ip::flshcr2_awr_probe();
    console::write_fmt_line(format_args!(
        "flexspi  : FLSHCR2 cs0={:#010x}(ard={} awr={} awn={}) cs1={:#010x}(ard={} awr={})",
        awr[0].0, awr[0].1, awr[0].2, awr[0].3, awr[1].0, awr[1].1, awr[1].2
    ));

    // ---- FlexCAN2 只读探针（**零副作用**：本模块没有任何写路径）----
    // 顺序刻意排在写序列之前：FlexSPI 那次的教训是"先读全寄存器、确认地址与复位值
    // 对得上，再去翻译写序列"。它还顺带回答一个 TODO-VERIFY：实际邮箱个数。
    {
        let probe = board_support::flexcan::probe();
        console::write_fmt_line(format_args!(
            "can      : mcr={:#010x} ctrl1={:#010x} ecr={:#010x} esr1={:#010x} iflag1={:#010x}",
            probe.mcr, probe.ctrl1, probe.ecr, probe.esr1, probe.iflag1
        ));
        console::write_fmt_line(format_args!(
            "can      : 存活={} mdis={} halt={} not_rdy={} frz_ack={} srxdis={} 邮箱={} 故障域={} rxerr={} txerr={}",
            if board_support::flexcan::looks_alive(&probe) {
                "是"
            } else {
                "否（先查 CCM 模块门控，不要先写序列）"
            },
            probe.disabled(),
            probe.mcr & board_support::flexcan::mcr::HALT != 0,
            probe.not_ready(),
            probe.in_freeze(),
            probe.self_reception_disabled(),
            probe.mailboxes(),
            probe.fault_confinement(),
            probe.rx_error_count(),
            probe.tx_error_count()
        ));
    }

    // ---- FlexCAN2 **PE 时钟与派生波特率**（这是位时序的依据，必须报事实）----
    console::write_fmt_line(format_args!(
        "can      : PE 时钟={} Hz 实际波特率={} bps（由 CTRL1 位时序与 CSCMR2[CAN_CLK_PODF] 算出）",
        board_support::flexcan::pe_clock_hz(),
        board_support::flexcan::actual_baud()
    ));

    // ---- 只读探针：读器件状态寄存器（FCB 的 LUT 里已有该序列）----
    let mut status = [0u8; 1];
    match flexspi_ip::ip_command(seq::READ_STATUS, 0, 1, Command::Read, &mut status) {
        Ok(intr) => console::write_fmt_line(format_args!(
            "flexspi  : reg rd status={:#04x} intr={}",
            status[0],
            intr.describe()
        )),
        Err(intr) => {
            console::write_fmt_line(format_args!(
                "flexspi  : reg rd FAIL —— {}",
                intr.describe()
            ));
        }
    }
}

/// SDRAM 与外设的启动期**摘要**（每项一行，进入统一启动报告）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_dma_summary() {
    console::write_fmt_line(format_args!(
        "  dma        : pool {:#010x}+{} KiB · non-cacheable {}",
        board_support::dma::POOL_BASE,
        board_support::dma::POOL_BYTES / 1024,
        if board_support::dma::probe_non_cacheable() {
            "PASS"
        } else {
            "FAIL（池所在区域仍可缓存 ⇒ DMA 会读到旧描述符）"
        }
    ));
}

/// FlexCAN2 的启动期**摘要**：存活 + 实测 PE 时钟/波特率 + loopback 结果。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_can_summary() {
    let probe = board_support::flexcan::probe();
    let loopback = if board_support::config::CAN_LOOPBACK_SELFTEST {
        match board_support::flexcan::loopback_selftest() {
            Ok(()) => "PASS",
            Err((stage, _value)) => {
                // 失败时保留 stage 细节（现场排查的第一手信息）。
                console::write_fmt_line(format_args!(
                    "  can        : {} · PE {} Hz · {} bps · loopback FAIL stage={stage}（见 gaps 的 P2）",
                    if board_support::flexcan::looks_alive(&probe) {
                        "alive"
                    } else {
                        "NO-RESPONSE"
                    },
                    board_support::flexcan::pe_clock_hz(),
                    board_support::flexcan::actual_baud(),
                ));
                return;
            }
        }
    } else {
        "skipped"
    };
    console::write_fmt_line(format_args!(
        "  can        : {} · PE {} Hz · {} bps · loopback {}",
        if board_support::flexcan::looks_alive(&probe) {
            "alive"
        } else {
            "NO-RESPONSE"
        },
        board_support::flexcan::pe_clock_hz(),
        board_support::flexcan::actual_baud(),
        loopback
    ));
}

/// PIT 寄存器回读（诊断命令 `diag` 用；启动报告只给摘要）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn dump_pit(clock: &board_support::timer::PitClock) {
    let r = clock.config_snapshot();
    console::write_fmt_line(format_args!(
        "pit regs : ch0 LD={:#010x} TCTRL={:#x} | ch1 LD={:#010x} TCTRL={:#x} | ch2 LD={:#010x} TCTRL={:#x}",
        r[0], r[1], r[2], r[3], r[4], r[5]
    ));
}

/// MPU 区域表回读（诊断命令 `diag` 用）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn dump_mpu_regions() {
    let mut storage = [0u8; 128];
    let mut buf = primitives::format::FixedBuf::new(&mut storage);
    board_support::mpu::describe(&mut buf);
    if let Ok(text) = core::str::from_utf8(buf.as_bytes()) {
        console::write_line(text);
    }
    for index in 0..board_support::mpu::region_count() {
        let mut storage = [0u8; 192];
        let mut buf = primitives::format::FixedBuf::new(&mut storage);
        board_support::mpu::describe_region(index, &mut buf);
        if let Ok(text) = core::str::from_utf8(buf.as_bytes()) {
            console::write_line(text);
        }
    }
}

/// 把源文件按 64 B 分块追加到目标文件；用于 `wq append <dst> @<src>` 的批量路径。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn append_file_chunks(source: &str, destination: &str) -> Option<u32> {
    let mut offset = 0u32;
    let mut total = 0u32;
    loop {
        let mut chunk = [0u8; 64];
        let result =
            board_support::fs::with_fs_read(|fs| fs.read_range(source, offset, &mut chunk))?;
        let read = result.ok()?;
        if read == 0 {
            return Some(total);
        }
        let payload = chunk.get(..read)?;
        let mut accepted = false;
        for _ in 0..4 {
            if board_support::fs::submit(board_support::fs::WriteKind::Append, destination, payload)
            {
                accepted = true;
                break;
            }
            let _ = board_support::fs::step();
        }
        if !accepted {
            return None;
        }
        let _ = board_support::fs::step();
        offset = offset.saturating_add(u32::try_from(read).ok()?);
        total = total.saturating_add(u32::try_from(read).ok()?);
    }
}

/// 一个归档窗口的只读校验结果。
struct VerifiedArchive {
    /// 解压后的字节数。
    bytes: u32,
    /// 压缩文件的 CRC-32（与索引里的口径一致）。
    file_crc: u32,
    /// 解压数据的**流式 CRC 状态**（未做最终异或），可跨窗口继续累计。
    data_crc: u32,
    /// 本窗口解压数据的 CRC-32（已做最终异或，便于单窗口对照）。
    window_crc: u32,
}

/// 只读校验一个 gzip 文件：解压到 SDRAM 输出缓冲，不落盘。
///
/// `data_crc` 是上一窗口的流式状态；返回值里的 `data_crc` 已把本窗口解压结果接上，
/// 因此 `log verify` 可以给出全部窗口的**合并 CRC-32**，而不是每窗口各自独立的值。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn verify_gzip_file(source: &str, data_crc: u32) -> Option<VerifiedArchive> {
    let size = board_support::fs::with_fs_read(|fs| fs.stat(source))
        .and_then(Result::ok)
        .map_or(0, |info| info.size);
    if size == 0 || size > 64 * 1024 {
        return None;
    }
    let (input, output) = gzip_buffers();
    let mut offset = 0u32;
    loop {
        let mut chunk = [0u8; 64];
        let result =
            board_support::fs::with_fs_read(|fs| fs.read_range(source, offset, &mut chunk))?;
        let read = result.ok()?;
        if read == 0 {
            break;
        }
        let at = usize::try_from(offset).ok()?;
        let dst = input.get_mut(at..at.saturating_add(read))?;
        dst.copy_from_slice(chunk.get(..read)?);
        offset = offset.saturating_add(u32::try_from(read).ok()?);
    }
    let input = input.get(..usize::try_from(offset).ok()?)?;
    let file_crc = log_archive::file_crc32(input);
    let mut state = gzip::DecompressorOxide::new();
    let mut at = 0usize;
    let mut total = 0u32;
    let mut crc = data_crc;
    let mut window_crc = log_archive::stream_crc_init();
    while at < input.len() {
        let rest = input.get(at..)?;
        let outcome = gzip::decompress(rest, output, &mut state).ok()?;
        if outcome.consumed == 0 || outcome.produced == 0 {
            break;
        }
        let produced = output.get(..outcome.produced)?;
        crc = log_archive::stream_crc_update(crc, produced);
        window_crc = log_archive::stream_crc_update(window_crc, produced);
        total = total.saturating_add(u32::try_from(outcome.produced).ok()?);
        at = at.saturating_add(outcome.consumed);
    }
    Some(VerifiedArchive {
        bytes: total,
        file_crc,
        data_crc: crc,
        window_crc: primitives::crc::crc32_finish(window_crc),
    })
}

/// 把 gzip 文件解压写入目标：`append_first=false` 时首块用 Write 覆盖，
/// `true` 时首块用 Append 追加（用于多窗口合并）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn decompress_file_into(source: &str, destination: &str, append_first: bool) -> Option<u32> {
    let size = board_support::fs::with_fs_read(|fs| fs.stat(source))
        .and_then(Result::ok)
        .map_or(0, |info| info.size);
    if size == 0 || size > 64 * 1024 {
        return None;
    }
    let (input, output) = gzip_buffers();
    let mut offset = 0u32;
    loop {
        let mut chunk = [0u8; 64];
        let result =
            board_support::fs::with_fs_read(|fs| fs.read_range(source, offset, &mut chunk))?;
        let read = result.ok()?;
        if read == 0 {
            break;
        }
        let at = usize::try_from(offset).ok()?;
        let dst = input.get_mut(at..at.saturating_add(read))?;
        dst.copy_from_slice(chunk.get(..read)?);
        offset = offset.saturating_add(u32::try_from(read).ok()?);
    }
    let input = input.get(..usize::try_from(offset).ok()?)?;
    if !append_first && archive_file_size(destination) != 0 {
        // 覆盖语义 = 先删除，再用 Append 重建。不能第一块用 Write：
        // Write 建的是 Data 文件，下一块 Append 会把它从头重写成 Log，丢掉第一块。
        let _ = board_support::fs::submit(board_support::fs::WriteKind::Remove, destination, &[]);
        let _ = board_support::fs::step();
    }
    let mut state = gzip::DecompressorOxide::new();
    let mut at = 0usize;
    let mut total = 0u32;
    while at < input.len() {
        let rest = input.get(at..)?;
        let outcome = gzip::decompress(rest, output, &mut state).ok()?;
        if outcome.consumed == 0 || outcome.produced == 0 {
            break;
        }
        let produced = output.get(..outcome.produced)?;
        let mut wrote = 0usize;
        while wrote < produced.len() {
            let end = wrote
                .saturating_add(board_support::fs::WRITE_MAX_DATA)
                .min(produced.len());
            let payload = produced.get(wrote..end)?;
            if !board_support::fs::submit(
                board_support::fs::WriteKind::Append,
                destination,
                payload,
            ) {
                return None;
            }
            let _ = board_support::fs::step();
            wrote = end;
        }
        total = total.saturating_add(u32::try_from(outcome.produced).ok()?);
        at = at.saturating_add(outcome.consumed);
    }
    Some(total)
}

/// 分片日志轮转的进度。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RotateState {
    generation: u32,
    offset: u32,
    source_bytes: u32,
    gzip_bytes: u32,
    crc: u32,
}

/// 分片轮转的单块大小。
const LOG_ROTATE_CHUNK: usize = 2048;

/// 开始一次分片日志轮转：确保索引可用，返回 generation 与初始进度。
pub(crate) fn rotate_start() -> Option<RotateState> {
    let _ = archive_ensure()?;
    if !archive_migrate_legacy() {
        return None;
    }
    let _ = log_try_flush(true);
    let source = archive_file_size(board_support::config::files::BLACKBOX);
    if source == 0 {
        return None;
    }
    let index = archive_snapshot()?;
    Some(RotateState {
        generation: index.next_generation(),
        offset: 0,
        source_bytes: 0,
        gzip_bytes: 0,
        crc: log_archive::stream_crc_init(),
    })
}

/// 推进一个分片；返回 `Ok(true)` 表示已归档完成，`Ok(false)` 表示还要继续。
pub(crate) fn rotate_step(state: &mut RotateState) -> Result<bool, ()> {
    let mut chunk = [0u8; LOG_ROTATE_CHUNK];
    let read = board_support::fs::with_fs_read(|fs| {
        fs.read_range(
            board_support::config::files::BLACKBOX,
            state.offset,
            &mut chunk,
        )
    })
    .ok_or(())?
    .map_err(|_| ())?;
    if read == 0 {
        return rotate_finish(state);
    }
    let (_, output) = gzip_buffers();
    let Some(len) = gzip_on_sdram_stack(chunk.get(..read).ok_or(())?, output) else {
        rotate_abort(state);
        return Err(());
    };
    let Some(compressed) = output.get(..len) else {
        rotate_abort(state);
        return Err(());
    };
    let mut path_buffer = [0u8; PATH_LEN];
    let Some(path) = log_archive::format_path(state.generation, &mut path_buffer) else {
        rotate_abort(state);
        return Err(());
    };
    let mut at = 0usize;
    while at < compressed.len() {
        let end = at
            .saturating_add(board_support::fs::WRITE_MAX_DATA)
            .min(compressed.len());
        let Some(payload) = compressed.get(at..end) else {
            rotate_abort(state);
            return Err(());
        };
        if !board_support::fs::submit(board_support::fs::WriteKind::Append, path, payload) {
            rotate_abort(state);
            return Err(());
        }
        let _ = board_support::fs::step();
        at = end;
    }
    state.crc = log_archive::stream_crc_update(state.crc, compressed);
    state.offset = state
        .offset
        .saturating_add(u32::try_from(read).map_err(|_| ())?);
    state.source_bytes = state
        .source_bytes
        .saturating_add(u32::try_from(read).map_err(|_| ())?);
    state.gzip_bytes = state
        .gzip_bytes
        .saturating_add(u32::try_from(len).map_err(|_| ())?);
    if read < LOG_ROTATE_CHUNK {
        rotate_finish(state)
    } else {
        Ok(false)
    }
}

fn rotate_finish(state: &mut RotateState) -> Result<bool, ()> {
    if state.source_bytes == 0 || state.gzip_bytes == 0 {
        rotate_abort(state);
        return Err(());
    }
    let mut path_buffer = [0u8; PATH_LEN];
    let path = log_archive::format_path(state.generation, &mut path_buffer).ok_or(())?;
    let written = archive_file_size(path);
    if written != state.gzip_bytes {
        rotate_abort(state);
        return Err(());
    }
    let window = ArchiveWindow::new(
        state.generation,
        written,
        state.source_bytes,
        primitives::crc::crc32_finish(state.crc),
    );
    let index = archive_snapshot().ok_or(())?;
    let mut next = index;
    let evicted = next.push(window).map_err(|_| ())?;
    let _ = archive_persist(next);
    archive_set(next);
    if let Some(old) = evicted {
        let mut old_buffer = [0u8; PATH_LEN];
        if let Some(old_path) = log_archive::format_path(old.generation, &mut old_buffer) {
            let _ = board_support::fs::submit(board_support::fs::WriteKind::Remove, old_path, &[]);
            let _ = board_support::fs::step();
        }
    }
    if !board_support::fs::submit(
        board_support::fs::WriteKind::Write,
        board_support::config::files::BLACKBOX,
        b"\n[rotated]\n",
    ) {
        return Err(());
    }
    let _ = board_support::fs::step();
    Ok(true)
}

/// 取消一次尚未完成的轮转，删除半成品归档。
pub(crate) fn rotate_cancel(state: &RotateState) {
    rotate_abort(state);
}

fn rotate_abort(state: &RotateState) {
    let mut path_buffer = [0u8; PATH_LEN];
    if let Some(path) = log_archive::format_path(state.generation, &mut path_buffer) {
        let _ = board_support::fs::submit(board_support::fs::WriteKind::Remove, path, &[]);
        let _ = board_support::fs::step();
    }
}

/// 活动日志超阈值时：新建 generation 归档，成功后再清空活动文件。
///
/// 不再复制任何旧窗口；旧窗口的删除由 `archive_store_bytes` 在索引更新后完成。
/// 失败时保留活动文件，宁可重复也不丢数据。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn rotate_log_if_needed() -> bool {
    // 启动/自检窗口内不自动切分；等系统稳定 30 s 后再做。
    let uptime_ms = board_support::timer::PitClock.now().as_micros() / 1000;
    if uptime_ms < LOG_ROTATE_AFTER_MS {
        return false;
    }
    let _ = log_try_flush(true);
    let active = archive_file_size(board_support::config::files::BLACKBOX);
    if active < LOG_ROTATE_BYTES {
        return false;
    }
    let Some(_window) = archive_archive_file(board_support::config::files::BLACKBOX) else {
        return false;
    };
    let _ = board_support::fs::submit(
        board_support::fs::WriteKind::Write,
        board_support::config::files::BLACKBOX,
        b"\n[rotated]\n",
    );
    let _ = board_support::fs::step();
    true
}

/// selftest 的 Flash 擦/写/回读段（Flash 擦写路径保持 ITCM）。
#[inline(never)]
fn selftest_flash_write() -> bool {
    use board_support::store::FlexSpiMedium;
    use ports::raw::RawMedium as _;

    const OFFSET: u32 = flash_layout::BOOT_META_OFFSET;
    board_support::fault::set_flash_stage(20);
    let mut medium = FlexSpiMedium::new();
    let mut flash_ok = board_support::flexspi_ip::install_lut();
    if flash_ok {
        board_support::fault::set_flash_stage(21);
        flash_ok = board_support::flexspi_ip::erase_sector_staged(OFFSET).is_ok();
    }
    if flash_ok {
        board_support::fault::set_flash_stage(22);
        let mut payload = [0u8; 256];
        for (index, slot) in payload.iter_mut().enumerate() {
            *slot = u8::try_from(index % 251).unwrap_or(0);
        }
        flash_ok = board_support::flexspi_ip::program_page_staged(OFFSET, &payload).is_ok();
        if flash_ok {
            board_support::fault::set_flash_stage(23);
            let mut readback = [0u8; 256];
            flash_ok = medium.read(OFFSET as usize, &mut readback).is_ok() && readback == payload;
        }
    }
    board_support::fault::set_flash_stage(0);
    flash_ok
}

/// 目标端自检：逐条执行并打印，`make test-target` 解析。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn run_selftest(out: &mut dyn core::fmt::Write) -> bool {
    use board_support::store::FlexSpiMedium;
    use ports::raw::RawMedium as _;

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut check = |name: &str, ok: bool, out: &mut dyn core::fmt::Write| {
        if ok {
            passed = passed.saturating_add(1);
            terminal_line!(out, "test {name} ... ok");
        } else {
            failed = failed.saturating_add(1);
            terminal_line!(out, "test {name} ... FAILED");
        }
    };

    let pit = board_support::timer::PitClock.config_snapshot();
    check(
        "timer_pit_chain",
        pit[1] & 1 != 0 && pit[3] & 1 != 0 && pit[3] & 4 != 0,
        out,
    );
    let (mpu, icache, dcache) = board_support::mpu::state();
    check("mpu_cache_state", mpu && icache && dcache, out);
    let mut medium = FlexSpiMedium::new();
    let mut ivt = [0u8; 4];
    let ivt_ok = medium
        .read(board_support::flash::IVT_OFFSET, &mut ivt)
        .is_ok()
        && u32::from_le_bytes(ivt) == board_support::flash::IVT_MAGIC;
    check("flash_ivt_magic", ivt_ok, out);
    let first = board_support::timer::PitClock.now().as_micros();
    let second = board_support::timer::PitClock.now().as_micros();
    check("timer_monotonic", second >= first, out);

    // Flash 擦/写/回读：只用一个扇区、一页，保持 selftest 本身足够小。
    // 擦写代码必须在 ITCM；这里只调用，不把 XIP 取指留在擦除期间。
    check("flash_erase_program_readback", selftest_flash_write(), out);

    terminal_line!(out, "TEST-END passed={passed} failed={failed}");
    failed == 0
}

/// Flash 写路径自证（仅在 `config::FLASH_SELFTEST_AT_BOOT` 打开时执行）。
///
/// 目标、图案与判据全部来自纯逻辑的 [`nor_flash::selftest`]（主机测试已覆盖），
/// 这里只做"按顺序调用 + 如实打印"。**每一步都打印**：走到哪一步失败，
/// 比"失败了"三个字有价值得多。
fn run_flash_write_selftest() {
    use board_support::flexspi_ip;
    let config = board_support::config::FLASH_SELFTEST_AT_BOOT;
    if !config {
        return;
    }

    // 装 LUT：控制器里那套是 Boot ROM 装的（只够取指/读），擦写序列必须自己装。
    // ⚠️ 关键教训：**整条 4 字槽都要写**（官方约定「零字即 STOP」）；只写 word0 会让序列
    // 跑进槽内残留数据 ⇒ 实测 `INTR.IPCMDERR`。装完这条之后**擦除在真机上成功**。
    board_support::fault::set_flash_stage(30);
    let lut_ok = flexspi_ip::install_lut();
    console::write_fmt_line(format_args!(
        "flexspi  : install LUT {}",
        if lut_ok {
            "ok"
        } else {
            "FAIL（序列索引越界）"
        }
    ));

    let sector = nor_flash::selftest::scratch_sector_offset();

    // 临时诊断（FIFO 写入实验 / 单字节实验）已裁掉：结论都在提交信息与
    // `config::FLASH_SELFTEST_AT_BOOT` 的文档里，留着会把 .text 顶出 ITCM 预算。
    // 下一步要查的是"`TFDR` 写入是否真的进 FIFO"，用 BSP 里的 `fill_probe()`
    // 在**命令进行中**读 `FILL`（空闲时它可能不更新 ✗）。

    let mut outcome = "PASS";
    let mut detail = 0u32;

    // 判决性实验：**擦除路径也读一次 WEL**。
    // 擦除同样需要 WEL 而擦除一直是成功的 ⇒ 若这里读到 WEL=0 却仍能擦除，
    // 说明**我的状态读法不可靠**；若读到 WEL=1，则说明**编程路径的 WREN 没生效**。
    let wel_before_erase = flexspi_ip::write_enable().is_ok();
    let status_after_wren = flexspi_ip::status_now().unwrap_or(0xFF);
    // 读两次：`WREN` 之后的状态读是**整条链上最可疑的一环**（本轮诊断的结论），
    // 所以把两次独立读数都打出来 —— 一次读数无法区分"器件没置 WEL"与"读法不可信"。
    let wren_again = flexspi_ip::write_enable().is_ok();
    let status_second = flexspi_ip::status_now().unwrap_or(0xFF);
    console::write_fmt_line(format_args!(
        "flexspi  : 写使能探针 wren1_ok={} sr1a={:#04x} wren2_ok={} sr1b={:#04x} (WIP={} WEL={})",
        wel_before_erase,
        status_after_wren,
        wren_again,
        status_second,
        status_second & 0x01,
        (status_second >> 1) & 0x01
    ));

    board_support::fault::set_flash_stage(31);
    if let Err(stage) = flexspi_ip::erase_sector_staged(sector) {
        outcome = "FAIL";
        detail = u32::from(stage); // 1=写使能 2=擦除命令 3=等忙超时
    } else {
        board_support::fault::set_flash_stage(32);
        let mut payload = [0u8; 256];
        nor_flash::selftest::pattern_at(sector, &mut payload);
        if let Err((stage, err)) = flexspi_ip::program_page_staged(sector, &payload) {
            outcome = "FAIL";
            detail = 10 + u32::from(stage); // 11=写使能 12=命令 13=等忙
            // **把失败时的错误位带出来** —— 用数据决定下一步，而不是在 ip_write 里继续试。
            console::write_fmt_line(format_args!(
                "flexspi  : 编程失败 stage={} intr={:#06x} | note={:#010x} len={} done={} cap={} —— {}",
                stage,
                err.raw,
                err.note,
                (err.note >> 16) & 0xFF,
                (err.note >> 8) & 0xFF,
                err.note & 0xF,
                err.describe()
            ));
        } else {
            board_support::fault::set_flash_stage(33);
            let mut read_back = [0u8; 256];
            let mut medium = board_support::store::FlexSpiMedium::new();
            let read = ports::raw::RawMedium::read(
                &mut medium,
                usize::try_from(sector).unwrap_or(0),
                &mut read_back,
            );
            match read {
                Ok(n) if n == read_back.len() => {
                    if let Some(index) = nor_flash::selftest::verify_at(sector, &read_back) {
                        outcome = "FAIL";
                        detail = 100 + u32::try_from(index).unwrap_or(u32::MAX);
                    }
                }
                _ => {
                    outcome = "FAIL";
                    detail = 4;
                }
            }
        }
    }
    board_support::fault::set_flash_stage(34);
    let restored = flexspi_ip::erase_sector(sector).is_ok();
    board_support::fault::set_flash_stage(0);
    console::write_fmt_line(format_args!(
        "flash    : reg 写路径自证 {} detail={} 擦回={} sector={:#x}",
        outcome,
        detail,
        if restored { "ok" } else { "FAIL" },
        sector
    ));
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_flash_selftest(clock: &board_support::timer::PitClock) {
    // **一个用例覆盖两条路径**：从 `sector+200` 写 300 字节 ——
    //   ① 起始非页对齐 ⇒ 第 1 页走**读-改-写**（RMW）；
    //   ② 200+300 > 256 ⇒ 跨页 ⇒ **页拆分**。
    // 判据（唯一）：写后从 XIP 读回，写入范围内与 `pattern_at` 逐字节一致。
    use nor_flash::selftest;
    use ports::raw::RawMedium;

    let _ = clock;
    let mut medium = board_support::store::FlexSpiMedium::new();
    let sector = selftest::scratch_sector_offset();
    let sector_usize = usize::try_from(sector).unwrap_or(0);
    let page_bytes = usize::try_from(nor_flash::PAGE_BYTES).unwrap_or(256);

    if !board_support::rom_flash::available() {
        console::write_line("flash    : 自证跳过（ROM 驱动不可用）");
        return;
    }

    let offset = sector + 200;
    let len = 300usize;
    let mut bad = 0u32;
    if RawMedium::erase(&mut medium, sector_usize, 4096).is_err() {
        bad = 1;
    } else {
        let mut payload = [0u8; 300];
        selftest::pattern_at(offset, &mut payload);
        if RawMedium::program(&mut medium, usize::try_from(offset).unwrap_or(0), &payload).is_err()
        {
            bad = 3;
        } else {
            let first_page = usize::try_from(offset).unwrap_or(0)
                - (usize::try_from(offset).unwrap_or(0) % page_bytes);
            let span = page_bytes * 2;
            let mut read_back = [0u8; 512];
            match RawMedium::read(&mut medium, first_page, &mut read_back) {
                Ok(n) if n == span => {
                    let in_page = usize::try_from(offset).unwrap_or(0) - first_page;
                    for position in 0..span {
                        let actual = read_back.get(position).copied().unwrap_or(0);
                        let in_range = position >= in_page && position < in_page + len;
                        if in_range {
                            let mut want = [0u8; 1];
                            let absolute = u32::try_from(first_page + position).unwrap_or(0);
                            selftest::pattern_at(absolute, &mut want);
                            if actual != want.first().copied().unwrap_or(0) {
                                bad = u32::try_from(position).unwrap_or(0) + 100;
                                break;
                            }
                        } else if actual != 0xFF {
                            // 范围外必须仍是擦除态（RMW 写坏邻居会在这里露出来）
                            bad = u32::try_from(position).unwrap_or(0) + 1000;
                            break;
                        }
                    }
                }
                _ => bad = 4,
            }
        }
    }
    let restored = RawMedium::erase(&mut medium, sector_usize, 4096).is_ok();
    console::write_fmt_line(format_args!(
        "flash    : 自证(跨页+非对齐) {} 擦写三例={} bad={} 擦回={} sector={:#x} page_size={}",
        if bad == 0 && restored { "PASS" } else { "FAIL" },
        if bad == 0 { "ok" } else { "FAIL" },
        bad,
        if restored { "ok" } else { "FAIL" },
        sector,
        page_bytes
    ));
}

/// SDRAM 上电自检的结果（**收集**，不打印；统一由启动报告输出）。
#[derive(Debug, Clone, Copy, Default)]
struct PostReport {
    /// 是否真的跑了（未启用/无 SDRAM 时为 false）。
    ran: bool,
    /// 三段算法测试是否全部通过。
    pass: bool,
    /// 耗时（毫秒）。
    elapsed_ms: u32,
    /// 地址线/数据线/图案测试的错误数。
    addr_err: u32,
    data_err: u32,
    pattern_err: u32,
    /// 扫描容量（KiB）。
    bytes_kib: u32,
    /// 250 ms 保持性检查：保持住的探针数 / 总数。
    retained: u32,
    probes: u32,
    /// 首个错误现场（偏移、期望、实读）。
    first_error: Option<(u32, u32, u32)>,
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn run_boot_post(sdram: Option<&mut Sdram>, clock: &board_support::timer::PitClock) -> PostReport {
    if !board_support::config::SDRAM_POST_AT_BOOT {
        return PostReport::default();
    }
    let Some(sdram) = sdram else {
        return PostReport::default();
    };
    board_support::rt::measured(realtime::Span::BootPost, || {
        let start = clock.now().as_micros();
        // 全片 32 MiB 自检只在**上电/故障复位**时跑；调试器软复位或终端 reboot
        // 走 1 MiB 快速自检。SDRAM 控制器与内容在暖复位中保持，冷启动已经把
        // 全片跑过一遍；这样正常重启不必每次都等 3.3 s。
        let cause = board_support::fault::boot_reset_cause();
        let full = cause.is_power_on() || cause.is_watchdog() || cause.is_lockup();
        let mut report = if full {
            run_post(sdram, clock)
        } else {
            run_quick_post(sdram, clock)
        };
        let elapsed = clock.now().as_micros().saturating_sub(start);
        report.elapsed_ms = u32::try_from(elapsed / 1000).unwrap_or(u32::MAX);
        report.ran = true;
        report
    })
}

/// 暖复位用的 1 MiB 快速自检（地址线/数据线/图样 + 50 ms 保持）。
///
/// 不是替代全片测试：上电与看门狗/lockup 复位仍走 [`run_post`] 的 32 MiB 全片。
/// 这里只覆盖 SDRAM 的低 1 MiB，抓“暖复位后完全不可用”的粗故障，换取启动时间。
struct QuickMem<'a> {
    inner: &'a mut Sdram,
    len: u32,
}

impl sdram_timing::memtest::MemoryDevice for QuickMem<'_> {
    fn len_bytes(&self) -> u32 {
        self.len
    }

    fn read32(&mut self, offset: u32) -> u32 {
        self.inner.read32(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.inner.write32(offset, value);
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn run_quick_post(sdram: &mut Sdram, clock: &board_support::timer::PitClock) -> PostReport {
    use sdram_timing::memtest as mt;
    use sdram_timing::memtest::MemoryDevice as _;

    const QUICK_BYTES: u32 = 1024 * 1024;
    let mut dev = QuickMem {
        inner: sdram,
        len: QUICK_BYTES,
    };
    let address = mt::address_bus_test(&mut dev);
    let data = mt::data_bus_test(&mut dev);
    let pattern = mt::pattern_test(&mut dev, 0xAAAA_AAAA);
    let first = address
        .first_error
        .or(data.first_error)
        .or(pattern.first_error)
        .map(|error| (error.offset, error.expected, error.actual));

    const PROBE_OFFSETS: [u32; 2] = [0x0000_0040, 0x000F_FFC0];
    const PROBE_VALUE: u32 = 0x5A5A_A5A5;
    for offset in PROBE_OFFSETS {
        dev.write32(offset, PROBE_VALUE);
    }
    let start = clock.now();
    while clock.now().elapsed_since(start) < Duration::from_millis(50) {
        core::hint::spin_loop();
    }
    let mut retained = 0u32;
    for offset in PROBE_OFFSETS {
        if dev.read32(offset) == PROBE_VALUE {
            retained += 1;
        }
    }

    PostReport {
        ran: true,
        pass: address.passed() && data.passed() && pattern.passed(),
        elapsed_ms: 0,
        addr_err: address.errors,
        data_err: data.errors,
        pattern_err: pattern.errors,
        bytes_kib: QUICK_BYTES / 1024,
        retained,
        probes: 2,
        first_error: first,
    }
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn run_post(sdram: &mut Sdram, clock: &board_support::timer::PitClock) -> PostReport {
    use sdram_timing::memtest as mt;

    let address = mt::address_bus_test(sdram);
    board_support::rt::probe_stage(6);
    let data = mt::data_bus_test(sdram);
    board_support::rt::probe_stage(7);
    let pattern = mt::pattern_test(sdram, 0xAAAA_AAAA);
    board_support::rt::probe_stage(8);

    let first = address
        .first_error
        .or(data.first_error)
        .or(pattern.first_error)
        .map(|error| (error.offset, error.expected, error.actual));

    // --- 保持性检查：刷新是否真的在跑 ---
    const PROBE_OFFSETS: [u32; 4] = [0x0000_0040, 0x0080_0000, 0x0100_0000, 0x01FF_FFC0];
    const PROBE_VALUE: u32 = 0x5A5A_A5A5;
    for offset in PROBE_OFFSETS {
        sdram.write32(offset, PROBE_VALUE);
    }
    let start = clock.now();
    while clock.now().elapsed_since(start) < Duration::from_millis(250) {
        core::hint::spin_loop();
    }
    let mut retained = 0u32;
    for offset in PROBE_OFFSETS {
        if sdram.read32(offset) == PROBE_VALUE {
            retained += 1;
        }
    }

    PostReport {
        ran: true,
        pass: address.passed() && data.passed() && pattern.passed(),
        elapsed_ms: 0,
        addr_err: address.errors,
        data_err: data.errors,
        pattern_err: pattern.errors,
        bytes_kib: pattern.bytes_tested / 1024,
        retained,
        probes: u32::try_from(PROBE_OFFSETS.len()).unwrap_or(4),
        first_error: first,
    }
}

/// 打印每秒心跳行。
/// 一秒一次的心跳快照。
///
/// # 为什么要一个类型，而不是 9 个位置参数
///
/// 心跳行是这个设备**唯一**的长期观测量，字段只会越来越多（已经有过 `rt=`/`txdrop=`/
/// `wfi=`/`dwt=` 四次增加）。位置参数一旦到 9 个，调用点就退化成"一排没有名字的值"：
/// 谁也不会发现 `bus_failures` 与 `ticks` 是不是写反了 —— 而 clippy 的
/// `too_many_arguments` 正是在提醒这件事。收成一个**命名结构**之后，调用点自解释，
/// 加字段也只是加一行并填一个字段，不用再数第几个参数。
struct Beat {
    /// 第几次心跳（自本次上电起）。
    beats: u64,
    /// 当前时刻（PIT 时基）。
    now: Timepoint,
    /// 时基中断累计次数（用于核对时基有没有停）。
    ticks: u32,
    /// 本次健康位是否到齐（决定是否允许喂狗）。
    watchdog_fed: bool,
    /// 自上次心跳以来丢弃的控制台字节数。
    tx_dropped: u32,
    /// 自上次心跳以来接收队列满而丢弃的字节数（ISR 搬入时队列满）。
    rx_dropped: u32,
    /// 自上次心跳以来的空闲睡眠次数。
    wfis: u32,
    /// 仪表被外部清掉后自动重装的累计次数。
    dwt_rearms: u32,
    /// 周期计数器当前是否在走。
    dwt_alive: bool,
    /// 文件系统写队列的统计快照（待处理/成功/失败/被拒）。
    fs_queue: board_support::fs::QueueStats,
}

#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_beat(beat: &Beat) {
    // `rt=` 是**实测**的实时预算结论（`ok` / `VIOLATION`），不是配置值：
    // 每秒随心跳出现一次，因此长期运行的日志里也能看出"有没有哪次关中断超了预算"。
    // `cargo xtask bench` 会把这一列当作硬判据（`rt=VIOLATION` 即失败）。
    // 三态而不是两态：**没有样本**必须是独立的一态（`none`），不能混进 `ok`。
    // 真机踩过：DWT 没启用 ⇒ 所有测量被跳过 ⇒ 空表 ⇒ `check()` 返回 Ok ⇒ 打 `rt=ok`。
    let rt_verdict = {
        let metrics = board_support::rt::snapshot();
        if !metrics.has_samples() {
            "none"
        } else if metrics.all_within_budget() {
            "ok"
        } else {
            "VIOLATION"
        }
    };
    // `txdrop=` / `rxdrop=` 都是**自上一行心跳以来**的丢弃字节数：
    // - `txdrop=`：发送队列满且中断关着；
    // - `rxdrop=`：接收队列满（ISR 搬入时覆盖最旧字节）。
    //
    // `txdrop=` 正常运行时恒为 0；一旦非 0，
    // 说明有人试图在关中断上下文里输出超过队列容量的数据 —— 那是真实的故障前兆
    // （启动期最危险：审计实测 `#[init]` 要输出 1,117 B，而队列只有 2,048 B）。
    console::write_fmt_line(format_args!(
        "beat={} uptime={}.{:03}s tick={} wdg_ok={} txdrop={} rxdrop={} wfi={} dwt={} rearm={} fsq={}/{}/{}/{} rt={}",
        beat.beats,
        beat.now.as_micros() / 1_000_000,
        (beat.now.as_micros() / 1_000) % 1_000,
        beat.ticks,
        u8::from(beat.watchdog_fed),
        beat.tx_dropped,
        beat.rx_dropped,
        beat.wfis,
        // `dwt=` 是**实时仪表本身的健康状态**：`ok` = 计数在走，`rearm(N)` = 被清过 N 次
        // （已自动重装），`DEAD` = 重装后仍没走。它回答的是"上面那张 rt 表的数字还可信吗"。
        if beat.dwt_alive { "ok" } else { "DEAD" },
        beat.dwt_rearms,
        // `fsq=pending/executed/failed/rejected`：写队列的四个数。
        // **失败与被拒都必须可见** —— 静默丢一次写入等于对着调用方说谎，
        // 而"队列满被拒"与"执行失败"是两件不同的事，分开报才能定位。
        beat.fs_queue.pending,
        beat.fs_queue.executed,
        beat.fs_queue.failed,
        beat.fs_queue.rejected,
        rt_verdict
    ));
}

/// panic 处理：尽力写日志，然后让核心停在**可检查**的状态。
///
/// 用 `udf` 而不是死循环：`udf` 会置 `SCB::CFSR` 的 `UNDEFINSTR`，
/// 调试器能看到确切的停机位置与故障寄存器；死循环则只能看到一个"在跑"的核心。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // 关中断窗口的配对可能被 panic 打断了：先把嵌套深度清零，否则此后**所有**样本
    // 都会被当成「内层窗口」丢弃 —— 仪表看起来还在打表、其实一条都不再记（审计缺口 F）。
    board_support::rt::abandon_all_irq_offs();

    console::report_panic(info);
    cortex_m::asm::udf()
}

/// 打印**统一启动报告**（图形 + 身份 + 分节事实 + 就绪行）。
///
/// 设计目标：**一次打印、按节分组、标签对齐**。所有启动期的探针/自检在这里执行，
/// 结果只在这里输出 —— 不再散落在初始化路径里（那是"零散/难读"的根因）。
/// 关键硬件状态必须**回读打印**而不是只打期望值（**平台需求 DB-5**）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn print_boot_report(
    timings: board_support::board::BoardTimings,
    post: &PostReport,
    sdram: Option<&Sdram>,
    fs_ok: bool,
) {
    section("启动");
    console::write_fmt_line(format_args!(
        "  board      : {} ms",
        timings.peripherals_us / 1000
    ));
    console::write_fmt_line(format_args!(
        "  filesystem : {} ms",
        timings.filesystem_us / 1000
    ));
    if post.ran {
        console::write_fmt_line(format_args!(
            "  sdram post : {} ms · {} · {} KiB · hold {}/{}",
            post.elapsed_ms,
            if post.pass { "PASS" } else { "FAIL" },
            post.bytes_kib,
            post.retained,
            post.probes
        ));
        if !post.pass {
            console::write_fmt_line(format_args!(
                "               errors: addr={} data={} pattern={}",
                post.addr_err, post.data_err, post.pattern_err
            ));
            if let Some((offset, expected, actual)) = post.first_error {
                console::write_fmt_line(format_args!(
                    "               first @+{offset:#x}: want={expected:#010x} got={actual:#010x}"
                ));
            }
        }
    } else {
        console::write_line("  sdram post : skipped");
    }

    section("平台");
    console::write_fmt_line(format_args!(
        "  timebase   : PIT0+PIT1 chained · {} MHz · 1 ms tick",
        board_support::config::TIMEBASE_HZ / 1_000_000
    ));
    console::write_fmt_line(format_args!(
        "  console    : LPUART1 · {} 8N1 · GPIO_AD_B0_06/07",
        board_support::config::CONSOLE_BAUD
    ));
    console::write_line("  flash      : W25Q256JVEQ · 32 MiB · XIP 0x60000000");
    report_addressing();
    match sdram {
        Some(handle) => {
            console::write_fmt_line(format_args!(
                "  sdram      : W9825G6KH-6I · {} MiB @{:#010x} · SEMC {} MHz",
                handle.len_bytes() / (1024 * 1024),
                handle.base_address(),
                board_support::sdram::semc_clock_mhz()
            ));
        }
        None => {
            console::write_line("  sdram      : unavailable（初始化失败，已降级）");
        }
    }
    report_fs(fs_ok);
    report_alloc_pool();
    report_dma_summary();

    section("安全");
    report_mpu_summary();
    report_stack();
    report_fault_history();
    console::write_line("  watchdog   : not enabled（见 gaps 的 P5）");

    section("实时");
    report_dwt_summary();
    report_flash_benchmark();

    section("设备");
    report_device_id();
    report_can_summary();

    console::write_line("------------------------------------------------------------");
}

/// 分节标题（统一格式：`[节名]`）。
fn section(title: &str) {
    console::write_fmt_line(format_args!("[{title}]"));
}

/// 设备身份（OCOTP 的**候选**唯一 ID，只读、不烧熔丝）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_device_id() {
    let mut storage = [0u8; 128];
    let mut buf = primitives::format::FixedBuf::new(&mut storage);
    let _ = write!(buf, "  id         : ");
    board_support::ocotp::write_device_id(&mut buf);
    if let Ok(text) = core::str::from_utf8(buf.as_bytes()) {
        console::write_line(text);
    }
}

/// MPU/cache 的**启动判定** + 一行摘要。
///
/// 判定逻辑（含"非全开即报错"的穷举）在纯逻辑 crate [`mpu_regions::check_state`] 里，
/// 主机上有测试。属性没生效时打显眼的 `ERROR` 行（不 panic：现场设备停在启动阶段
/// 比降级运行更糟，而"缓存没开"这类问题不影响协议正确性）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_mpu_summary() {
    let (icache, dcache, mpu_enabled) = board_support::mpu::state();
    match mpu_regions::check_state(icache, dcache, mpu_enabled) {
        Ok(()) => {
            console::write_line("  mpu        : 8 regions verified · I-cache ON · D-cache ON");
        }
        Err(reason) => {
            console::write_fmt_line(format_args!(
                "  mpu        : **ERROR** {reason}（实测 icache={icache} dcache={dcache} mpu={mpu_enabled}）"
            ));
            console::write_line("               请核对 mpu::init() 是否在 board::init() 之前调用");
        }
    }
}

/// Flash 读吞吐基准（**平台需求 PF-1**；`cargo xtask bench` 采集这一行）。
///
/// 做法：把映像所在的 1 MiB 按 4 KiB 块读一遍，用 PIT 计时换算成 KiB/s。
/// ⚠️ 数字强依赖"控制器预取缓冲是否预热"（同一段代码可差 70 倍）；读这个数时
/// 必须知道之前有没有发生过写/擦（它们会清掉预取缓冲）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_flash_benchmark() {
    use board_support::config;
    use board_support::store::FlexSpiMedium;
    use ports::clock::Clock as _;
    use ports::raw::RawMedium as _;

    const CHUNK: usize = 4 * 1024;
    const ROUNDS: usize = 256; // 256 × 4 KiB = 1 MiB
    let mut medium = FlexSpiMedium::new();
    let mut buffer = [0u8; CHUNK];
    let mut checksum = 0u32;
    let start = board_support::timer::PitClock.now().as_micros();
    for round in 0..ROUNDS {
        let offset = (round % 16) * CHUNK; // 在映像前 64 KiB 内轮转（确保真的读 Flash）
        if medium.read(offset, &mut buffer).is_ok() {
            // 必须消费整个缓冲并 black_box，否则 LLVM 会把"只读首字节"优化成
            // 实际上不读 Flash —— 真机曾因此报出 3.4 GiB/s 的假吞吐。
            for byte in &buffer {
                checksum = checksum.wrapping_add(u32::from(*byte));
            }
            core::hint::black_box(&buffer);
        }
    }
    let elapsed = board_support::timer::PitClock
        .now()
        .as_micros()
        .saturating_sub(start);

    let mut storage = [0u8; 160];
    let mut buf = primitives::format::FixedBuf::new(&mut storage);
    // KiB/s = 读到的 KiB ÷ 微秒 × 1e6。用整数运算避免浮点（本项目禁用浮点算术）。
    let kib = (ROUNDS * CHUNK / 1024) as u64;
    let kib_per_s = (kib * 1_000_000).checked_div(elapsed).unwrap_or(0);
    let _ = write!(
        buf,
        "  flash rd : {} KiB/s（{} KiB / {} µs，checksum={checksum:#x}，cache {}）",
        kib_per_s,
        kib,
        elapsed,
        if config::MPU_AND_CACHE_ENABLED {
            "ON"
        } else {
            "OFF"
        }
    );
    if let Ok(text) = core::str::from_utf8(buf.as_bytes()) {
        console::write_line(text);
    }
}

/// 4 字节寻址自检（一行）。把"整片 32 MiB 可用"从声明变成每次启动都被验证的事实。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_addressing() {
    let ok = board_support::flash::four_byte_addressing_ok();
    console::write_fmt_line(format_args!(
        "  flash 4B   : {} · {} MiB addressable（>16 MiB 未被别名）",
        if ok { "PASS" } else { "FAIL" },
        board_support::config::ADDRESSABLE_BYTES / (1024 * 1024)
    ));
}

/// 分配池启动自检：**真的分配、真的写、真的校验、真的释放**（结论一行）。
///
/// 任何一步不符都如实报出来 —— 不 panic、不假装成功。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_alloc_pool() {
    use alloc::vec::Vec;

    let mut ok = true;

    // 1) 小块池：512 B（1 KiB 块）。
    {
        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve(512).is_err() {
            console::write_line("  alloc      : **FAIL** 512 B 请求被拒绝");
            ok = false;
        } else {
            for index in 0..512u32 {
                buffer.push((index % 251) as u8);
            }
            if buffer
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte != (index % 251) as u8)
            {
                console::write_line("  alloc      : **FAIL** 小块读回不符");
                ok = false;
            }
        }
    }

    // 2) 大块池：2048 B（64 KiB 块）必须成功。
    {
        let mut big: Vec<u8> = Vec::new();
        if big.try_reserve(2048).is_err() {
            console::write_line("  alloc      : **FAIL** 2048 B 被拒绝（大块池应接住）");
            ok = false;
        } else {
            for index in 0..2048u32 {
                big.push((index % 251) as u8);
            }
            if big
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte != (index % 251) as u8)
            {
                console::write_line("  alloc      : **FAIL** 大块读回不符");
                ok = false;
            }
        }
    }

    // 3) gzip 状态级别的大块：90,000 B 必须落进 128 KiB 块。
    {
        let mut gzip_like: Vec<u8> = Vec::new();
        if gzip_like.try_reserve(90_000).is_err() {
            console::write_line("  alloc      : **FAIL** 90,000 B 被拒绝（大块应为 128 KiB）");
            ok = false;
        }
    }

    // 4) 超过 128 KiB：256 KiB 必须被明确拒绝。
    {
        let mut too_big: Vec<u8> = Vec::new();
        if too_big.try_reserve(256 * 1024).is_ok() {
            console::write_line(
                "  alloc      : **FAIL** 256 KiB 竟然分配成功（契约是单次 ≤ 128 KiB）",
            );
            ok = false;
        }
    }

    // 4) 全部释放后必须回到全空闲。
    let stats = board_support::alloc_pool::stats();
    if !stats.empty {
        console::write_fmt_line(format_args!(
            "  alloc      : **FAIL** 自检后仍有 {} 块未归还（泄漏）",
            stats.used_blocks
        ));
        ok = false;
    }

    // 5) 结论 + 两段池事实。
    console::write_fmt_line(format_args!(
        "  alloc      : {} · 小块 {}/{} · 大块 {}/{} · 拒绝布局 {} · 拒绝释放 {} · exhaust {}",
        if ok { "OK" } else { "**FAIL**" },
        stats.used_blocks,
        stats.blocks,
        stats.big_used_blocks,
        stats.big_blocks,
        stats.rejected_layout,
        stats.rejected_releases,
        stats.exhaustion_failures
    ));
    board_support::alloc_pool::reset_counters();
}

/// DWT 摘要（一行）：它回答"上面那张 rt 表还可信吗"。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_dwt_summary() {
    // SDRAM 自检期间 DWT 可能被清掉；读之前先按需重装，否则会把 DEAD 当成仪表故障。
    let _ = board_support::rt::rearm_if_stopped();
    let (ctrl, demcr, running) = board_support::rt::dwt_state();
    console::write_fmt_line(format_args!(
        "  dwt        : {} · CTRL={:#010x} DEMCR={:#010x} · rearm {}",
        if running { "running" } else { "**DEAD**" },
        ctrl,
        demcr,
        DWT_REARMS.load(Ordering::Relaxed)
    ));
    if !running {
        console::write_line(
            "               ⚠ 周期计数器没走：rt 表的数字不可信（见 `diag` 的分段回读）",
        );
    }
}

/// DWT 的详细回读（`diag` 命令用）：装完即刻值 + 分段打点。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn dump_dwt_detail() {
    let (at_install_ctrl, at_install_demcr) = board_support::rt::install_readback();
    console::write_fmt_line(format_args!(
        "dwt@init : CTRL={at_install_ctrl:#010x} DEMCR={at_install_demcr:#010x}"
    ));
    // 已知的启动期清除（pattern_test 期间）一行带过；只有"不在已知集合里"的异常才展开。
    const KNOWN_BOOT_CLEARED: [usize; 4] = [4, 5, 2, 8];
    let mut unexpected = 0u32;
    for (stage, name) in [
        (1usize, "board::init 后"),
        (4, "SDRAM 自检/基准 后"),
        (5, "板级对象装配后"),
        (2, "启动自检后"),
        (3, "重新使能后"),
        (6, "地址总线测试后"),
        (7, "数据总线测试后"),
        (8, "图案测试后"),
    ] {
        let (c, d) = board_support::rt::stage_readback(stage);
        let healthy = c & 1 != 0 && d & (1 << 24) != 0;
        let known = KNOWN_BOOT_CLEARED.contains(&stage);
        if !healthy && !known {
            unexpected += 1;
            console::write_fmt_line(format_args!(
                "dwt@{stage}({name}): **意外异常** CTRL={c:#010x} DEMCR={d:#010x}"
            ));
        }
    }
    console::write_fmt_line(format_args!(
        "dwt@stages: 已知启动期清除 4 处（pattern_test 期间，已重装）；意外异常 {unexpected} 处"
    ));
}

/// 复位原因与上次故障（启动报告里一行；有故障记录时展开细节）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_fault_history() {
    use core::fmt::Write as _;

    let cause = board_support::fault::boot_reset_cause();
    let mut storage = [0u8; 384];
    let mut buffer = primitives::format::FixedBuf::new(&mut storage);
    let _ = write!(buffer, "  reset      : ");
    let _ = cause.describe(&mut buffer);

    match board_support::fault::previous_crash() {
        None => {
            let _ = write!(buffer, " · last crash: none");
            console::write_bytes_unlocked(buffer.as_bytes());
            console::write_line("");
        }
        Some(record) if !record.has_frame() => {
            let _ = write!(buffer, " · last crash: record without frame");
            console::write_bytes_unlocked(buffer.as_bytes());
            console::write_line("");
        }
        Some(record) => {
            let _ = write!(buffer, " · last crash: 见下方");
            console::write_bytes_unlocked(buffer.as_bytes());
            console::write_line("");
            // 故障现场是**异常路径**，展开细节是值得的（也是"可解释"的证据）。
            let mut storage = [0u8; 384];
            let mut buffer = primitives::format::FixedBuf::new(&mut storage);
            let _ = write!(buffer, "  crash      : ");
            let _ = record.describe(&mut buffer);
            console::write_bytes_unlocked(buffer.as_bytes());
            console::write_line("");
            let status = fault_decode::FaultStatus {
                cfsr: record.cfsr,
                hfsr: record.hfsr,
                mmfar: record.mmfar,
                bfar: record.bfar,
            };
            let mut storage = [0u8; 512];
            let mut buffer = primitives::format::FixedBuf::new(&mut storage);
            let _ = status.describe(&mut buffer);
            console::write_bytes_unlocked(buffer.as_bytes());
        }
    }
}

/// 栈峰值与余量（**平台需求 SF-2**；`cargo xtask bench` 的硬下限判据）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_stack() {
    use board_support::stacks;
    let peak = stacks::high_water_bytes();
    let headroom = stacks::headroom_bytes();
    console::write_fmt_line(format_args!(
        "  stack      : peak {} B / {} KiB · headroom {} B ({}%)",
        peak,
        stacks::STACK_SIZE / 1024,
        headroom,
        // 百分比只用于人读：整数运算避免浮点（本项目禁用浮点算术）。
        (headroom * 100) / stacks::STACK_SIZE
    ));
}

/// 把 `filesystem` 的统计搬成终端快照（字段口径由 `filesystem` 定义，这里只搬运）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn fs_snapshot_from_stats(stats: board_support::fs::FsStats) -> shell::shell::FsSnapshot {
    shell::shell::FsSnapshot {
        mounted: true,
        block_size: stats.block_size,
        block_count: stats.block_count,
        free_blocks: stats.free_blocks,
        used_blocks: stats.used_blocks,
        liveness_scanned: stats.scanned,
        live_bytes: stats.live_bytes,
        dead_bytes: stats.dead_bytes,
        file_count: stats.file_count,
        table_overflow: stats.table_overflow,
        erase_min: stats.erase_min,
        erase_max: stats.erase_max,
        erase_total: stats.erase_total,
        gc_explicit_runs: stats.gc_explicit_runs,
        gc_auto_runs: stats.gc_auto_runs,
        index_dirty: stats.index_dirty,
    }
}

/// 文件系统摘要（一行），并**真正用它做一次读**。
///
/// 为什么要真的读一次：只把 `mount()` 的结果打印出来，编译器会发现实例
/// **没有任何字段被读过**而把整块存储优化掉 —— `.bss` 一点都不长，看起来"省内存"，
/// 实际是这个集成根本没落地。因此必须真的 list + 读一个文件。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn report_fs(fs_ok: bool) {
    if !fs_ok {
        console::write_line("  fs         : MOUNT FAILED —— 应用数据不可用");
        return;
    }

    // 1) 文件数：只读内存索引，不读介质。
    let files = match board_support::fs::with_fs_owner(|fs| fs.stats_index_only().file_count) {
        None => {
            console::write_line("  fs         : unavailable（busy / mount failed）");
            return;
        }
        Some(files) => files,
    };

    // 2) 真读一个文件：确认"读路径 + 分区几何 + 文件系统索引"三者对得上。
    let mut slot = [0u8; 64];
    let read = board_support::fs::with_fs_owner(|fs| {
        fs.read_range(board_support::config::files::CONFIG, 0, &mut slot)
    });
    let config_state = match read {
        None => "busy",
        Some(Ok(size)) => {
            if size == 0 {
                "empty"
            } else {
                "present"
            }
        }
        Some(Err(board_support::fs::FsError::NotFound)) => "absent",
        Some(Err(_)) => "read-error",
    };

    console::write_fmt_line(format_args!(
        "  fs         : mounted · {} × {} KiB · {} files · config.bin {}",
        board_support::fs::DATA_BLOCKS,
        64,
        files,
        config_state
    ));
    let blackbox = board_support::fs::with_fs_owner(|fs| {
        fs.stat(board_support::config::files::BLACKBOX)
            .map(|info| info.size)
            .unwrap_or(0)
    })
    .unwrap_or(0);
    console::write_fmt_line(format_args!("  blackbox   : {blackbox} B（跨复位保留）"));
}

/// 心跳快照（按需打印；默认不输出）。
#[inline(never)]
#[unsafe(link_section = ".xip")]
fn current_beat(clock: &board_support::timer::PitClock) -> Beat {
    Beat {
        beats: u64::from(BEATS.load(Ordering::Relaxed)),
        now: clock.now(),
        ticks: TICKS.load(Ordering::Relaxed),
        watchdog_fed: all_healthy(
            HEALTH.load(Ordering::Relaxed) | health::IDLE,
            REQUIRED_HEALTH_BITS,
        ),
        tx_dropped: console::take_dropped_bytes(),
        rx_dropped: console::take_rx_dropped(),
        wfis: take_wfis(),
        dwt_rearms: DWT_REARMS.load(Ordering::Relaxed),
        fs_queue: board_support::fs::queue_stats(),
        dwt_alive: board_support::rt::is_installed(),
    }
}
