//! Tessera RT1021 的 Tock 板级入口（Phase 1：Tock 接管启动 + 复用现有 BSP）。
//!
//! 目标：Tock 内核启动后调用 `board_support::board::init()`（时钟 / PIT / SDRAM /
//! 控制台 / FlexSPI LUT），并让 Tock 的 NVIC 分发把 LPUART1/PIT 中断交给 BSP；
//! 内核 loop 里驱动 1 Hz 心跳与按行回显。生产固件仍是 RTIC（双轨）。

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use kernel::capabilities;
use kernel::create_capability;
use kernel::deferred_call::DeferredCall;
use kernel::platform::chip::{Chip, InterruptService};
use kernel::platform::{KernelResources, SyscallDriverLookup};
use kernel::process::StoppedExecutingReason;
use kernel::scheduler::{Scheduler, SchedulingDecision};
use kernel::static_init;

use board_support::{console, timer};
use ports::clock::Clock;

pub mod boot_header;
pub mod io;

// 内核栈：48 KiB（与生产固件一致）。
kernel::stack_size! {0xC000}

/// 启动头（FCB + IVT）：复用现有 Tessera 固件的前 8 KiB。
#[cfg_attr(target_os = "none", link_section = ".boot_hdr")]
#[used]
static BOOT_HDR: [u8; 8192] = boot_header::BOOT_HDR;

/// 横幅与版本号直接取仓库根的真源（构建期注入）。
const BANNER: &str = include_str!("../../../../banner.txt");
const VERSION: &str = include_str!("../../../../VERSION");

/// PIT 中断号（RT1021）。
const PIT_IRQ: u32 = 122;

/// BSP 的 1 ms 心跳句柄（ZST，只清 PIT 标志）。
static TICK: timer::TickSource = timer::TickSource::new();

/// 自本次启动以来的 1 ms 心跳计数（ISR 只做这一个原子加）。
static TICKS: AtomicU32 = AtomicU32::new(0);

/// LPUART1/PIT 的中断分发：把 Tock 的 `InterruptService` 桥接到 BSP。
struct BspInterrupts;

impl InterruptService for BspInterrupts {
    fn service_interrupt(&self, interrupt: u32) -> bool {
        match interrupt {
            imxrt10xx::nvic::LPUART1 => {
                console::isr_service();
                true
            }
            PIT_IRQ => {
                // 中断第一件事必须是清标志，否则会立刻重入。
                TICK.clear();
                TICKS.fetch_add(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

static BSP_INTERRUPTS: BspInterrupts = BspInterrupts;

type ChipHw = imxrt10xx::chip::Imxrt10xx<BspInterrupts>;

/// 无进程调度器：没有 userspace；`execute_kernel_work` 里额外驱动 Phase 1 的协作工作。
struct Phase1Scheduler;

impl<C: Chip> Scheduler<C> for Phase1Scheduler {
    fn next(&self) -> SchedulingDecision {
        SchedulingDecision::TrySleep
    }

    fn result(&self, _result: StoppedExecutingReason, _execution_time_us: Option<u32>) {}

    fn execute_kernel_work(&self, chip: &C) {
        chip.service_pending_interrupts();
        while DeferredCall::has_tasks() && !chip.has_pending_interrupts() {
            DeferredCall::service_next_pending();
        }
        phase1_work();
    }
}

/// 行缓冲容量。
const LINE_CAP: usize = 64;

/// 行缓冲（回显用；内核上下文单线程访问）。
static mut LINE: [u8; LINE_CAP] = [0; LINE_CAP];
static mut LINE_LEN: usize = 0;

/// 每秒心跳的基准（uptime 微秒）。
static mut LAST_BEAT_US: u64 = 0;

/// 内核 loop 的协作工作：1 Hz 心跳 + 消费 BSP 的 RX 队列（按行回显）。
fn phase1_work() {
    // 心跳：uptime 取 BSP 的 64 位 PIT 时基，monitor 判据可直接用。
    let now_us = timer::PitClock.now().as_micros();
    // SAFETY: 只在 Tock 内核上下文（单线程）调用；中断不访问该静态量。
    unsafe {
        if now_us.saturating_sub(LAST_BEAT_US) >= 1_000_000 {
            LAST_BEAT_US = now_us;
            let ticks = TICKS.load(Ordering::Relaxed);
            console::write_fmt_line(format_args!(
                "tock: beat uptime={}.{:03}s ticks={} txdrop={} rxdrop={}",
                now_us / 1_000_000,
                (now_us % 1_000_000) / 1_000,
                ticks,
                console::take_dropped_bytes(),
                console::take_rx_dropped(),
            ));
        }
    }

    // 行回显：RX 字节由 BSP 的 LPUART1 ISR 搬进队列。
    while let Some(byte) = console::rx_pop() {
        if byte == b'\r' {
            continue;
        }
        if byte == b'\n' {
            // SAFETY: 同 `phase1_work` 的单线程约定。
            unsafe {
                let len = LINE_LEN;
                let mut text = [0u8; 64];
                for (index, &raw) in LINE[..len].iter().enumerate() {
                    text[index] = if raw.is_ascii_graphic() || raw == b' ' {
                        raw
                    } else {
                        b'.'
                    };
                }
                let line = core::str::from_utf8(&text[..len]).unwrap_or("<?>");
                console::write_fmt_line(format_args!("tock: rx line({len}B): {line}"));
                LINE_LEN = 0;
            }
        } else {
            // SAFETY: 同上；只追加到固定 64 B 缓冲，满则丢弃后续字节。
            unsafe {
                if LINE_LEN < LINE_CAP {
                    LINE[LINE_LEN] = byte;
                    LINE_LEN += 1;
                }
            }
        }
    }
}

/// 平台资源表：本阶段没有 syscall driver。
struct TesseraPlatform {
    scheduler: Phase1Scheduler,
    systick: cortexm7::systick::SysTick,
}

impl SyscallDriverLookup for TesseraPlatform {
    fn with_driver<F, R>(&self, _driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::syscall::SyscallDriver>) -> R,
    {
        f(None)
    }
}

impl KernelResources<ChipHw> for TesseraPlatform {
    type SyscallDriverLookup = Self;
    type SyscallFilter = ();
    type ProcessFault = ();
    type Scheduler = Phase1Scheduler;
    type SchedulerTimer = cortexm7::systick::SysTick;
    type WatchDog = ();
    type ContextSwitchCallback = ();

    fn syscall_driver_lookup(&self) -> &Self::SyscallDriverLookup {
        self
    }
    fn syscall_filter(&self) -> &Self::SyscallFilter {
        &()
    }
    fn process_fault(&self) -> &Self::ProcessFault {
        &()
    }
    fn scheduler(&self) -> &Self::Scheduler {
        &self.scheduler
    }
    fn scheduler_timer(&self) -> &Self::SchedulerTimer {
        &self.systick
    }
    fn watchdog(&self) -> &Self::WatchDog {
        &()
    }
    fn context_switch_callback(&self) -> &Self::ContextSwitchCallback {
        &()
    }
}

/// 程序入口。
#[no_mangle]
pub unsafe fn main() {
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);

    // 1. 先把 Tock 的向量表/NVIC 装好；随后屏蔽 CPU 中断做 BSP 初始化，
    //    避免 BSP 尚未装好时 Boot ROM 遗留的 PIT/LPUART 中断进来。
    ChipHw::init();
    cortexm7::nvic::disable_all();
    cortexm7::nvic::clear_all_pending();
    // SAFETY: 只关 CPU 中断（cpsid i），不改其它状态。
    unsafe {
        core::arch::asm!("cpsid i");
    }

    // 2. 延迟调用必须很早初始化。
    kernel::deferred_call::initialize_deferred_call_state::<
        <ChipHw as Chip>::ThreadIdProvider,
    >();

    // 3. BSP 的 `.dtcm` 段是 NOLOAD，Tock 启动不会清零；先显式清零，
    //    否则 BSP 的队列/RefCell 会读到上电垃圾。
    //
    // SAFETY: 链接脚本定义了 `__sdtcm..__edtcm`，只清零本段，不碰其它内存。
    unsafe {
        extern "C" {
            static mut __sdtcm: u8;
            static mut __edtcm: u8;
        }
        let start = core::ptr::addr_of_mut!(__sdtcm);
        let end = core::ptr::addr_of_mut!(__edtcm);
        core::ptr::write_bytes(start, 0, end.offset_from(start) as usize);
    }

    // 4. BSP 初始化：控制台 + PIT + SDRAM + FlexSPI LUT。
    let board = board_support::board::init(BANNER, VERSION.trim());

    // 5. 启动 1 ms 心跳并重新打开需要的中断线。
    board.clock.start_tick();
    cortexm7::nvic::Nvic::new(imxrt10xx::nvic::LPUART1).enable();
    cortexm7::nvic::Nvic::new(PIT_IRQ).enable();
    // SAFETY: 只开 CPU 中断（cpsie i）。
    unsafe {
        core::arch::asm!("cpsie i");
    }

    // 6. Phase 1 证明行（保持与最小验证版一致的判据前缀）。
    console::write_line("tock: tessera-rt1021 kernel boot");
    console::write_line("tock: kernel_loop running (echo mode)");

    // 7. Tock 内核数据结构。
    let processes = static_init!(
        kernel::process::ProcessArray<0>,
        kernel::process::ProcessArray::new()
    );
    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(processes.as_slice()));
    let chip = static_init!(ChipHw, ChipHw::new(&BSP_INTERRUPTS));

    let platform = TesseraPlatform {
        scheduler: Phase1Scheduler,
        systick: cortexm7::systick::SysTick::new_with_calibration(396_000_000),
    };

    board_kernel.kernel_loop(
        &platform,
        chip,
        None::<&kernel::ipc::IPC<0>>,
        &main_loop_capability,
    );
}
