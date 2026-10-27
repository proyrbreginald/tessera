//! Tessera RT1021 最小 Tock 板级 crate。
//!
//! 目的：验证「用 Tock 替换 RTIC」在编译/链接层面可行。范围刻意压到最小：
//!
//! - 不做 userspace / 进程加载（`Kernel::new(&[])`）；
//! - 不引入 capsules / components；
//! - 只配 LPUART1（控制台），并把收到的字节按行回显，用来证明内核 loop 在服务中断；
//! - 启动头（FCB + IVT，8 KiB）直接复用现有 Tessera 固件，不改内容。
//!
//! 与官方 EVKB 参考板的差异（全部集中在本 crate）：
//! - 控制台引脚：GPIO_AD_B0_**06/07**（EVKB 是 12/13，即 `13/14` 编号）；
//! - 控制台时钟源：**24 MHz 晶振** + podf=1（EVKB 是 PLL3 80 MHz）；
//! - 串口分频：24 MHz 下必须用 SBR=13（Tock 上游硬编码 139，见 `chips/imxrt10xx/src/lpuart.rs`
//!   里的 `TESSERA-PATCH`）。
//!
//! ## RX 回显的稳健性（2026-10-27 真机反馈后重写）
//!
//! 第一版在 `received_buffer` 回调里**先同步逐字节输出、最后才重挂接收**。真机上
//! 发 22 B 只回显了 3 B：逐字节输出一行约 15 B × 87 µs ≈ 1.3 ms，期间驱动关掉了
//! RX 中断、又没有挂缓冲，硬件的 RDRF/OR 直接溢出，后面的字节全丢。
//!
//! 现在改成：
//! 1. **回调第一件事**是把一条备用 1 B 缓冲（双缓冲 A/B 轮换）重新挂给驱动，
//!    不先做任何拷贝/输出；
//! 2. 回调只把字节塞进静态行缓冲（64 B，O(1)），`\n` 时做个快照；
//! 3. 真正的串口输出放在 [`kernel::deferred_call::DeferredCall`] 里，在内核
//!    loop 的任务上下文执行（把整行的阻塞输出挪出中断回调）；
//! 4. 驱动侧 `clear_overrun()` 原本写错寄存器（清 `CTRL.ORIE` 而不是写 1 清
//!    `STAT.OR`），一次 overrun 之后 `STAT.OR` 永远置位 —— 已加 `TESSERA-PATCH`。

#![no_std]
#![no_main]

use core::cell::Cell;
use core::fmt::Write;

use kernel::capabilities;
use kernel::create_capability;
use kernel::deferred_call::{DeferredCall, DeferredCallClient};
use kernel::hil::uart;
use kernel::hil::uart::{Configure, Receive, ReceiveClient};
use kernel::platform::chip::Chip;
use kernel::platform::{KernelResources, SyscallDriverLookup};
use kernel::process::StoppedExecutingReason;
use kernel::scheduler::{Scheduler, SchedulingDecision};
use kernel::static_init;

pub mod boot_header;
pub mod io;

// 内核栈：16 KiB（验证用，未做涂抹测量；EVKB 用 8 KiB）。
kernel::stack_size! {0x4000}

type ChipHw = imxrt10xx::chip::Imxrt10xx<imxrt10xx::chip::Imxrt10xxDefaultPeripherals>;

/// 启动头：FCB + IVT + boot data，内容来自现有 Tessera 固件的前 8 KiB。
///
/// 只负责把它放进 `.boot_hdr`（链接脚本里排在 0x6000_0000）。
#[cfg_attr(target_os = "none", link_section = ".boot_hdr")]
#[used]
static BOOT_HDR: [u8; 8192] = boot_header::BOOT_HDR;

/// 空进程集合的容量：本验证不加载任何 userspace 进程。
const NUM_PROCS: usize = 0;

/// 行缓冲容量（字节，不含 `\n`）。超长行会被截断并标记。
const LINE_CAP: usize = 64;

/// 无进程调度器。
///
/// 没有进程可跑，`next()` 永远返回 `TrySleep`；内核会在有中断/延迟调用时
/// 先处理内核工作再睡下去。
struct NoSched;

impl<C: Chip> Scheduler<C> for NoSched {
    fn next(&self) -> SchedulingDecision {
        SchedulingDecision::TrySleep
    }

    fn result(&self, _result: StoppedExecutingReason, _execution_time_us: Option<u32>) {
        // 没有进程，不存在"上一次执行结果"。
    }
}

/// LPUART1 回显客户端。
///
/// - 接收：双 1 B 缓冲 + 静态行缓冲，回调里只做 O(1) 搬运；
/// - 发送：延迟调用（kernel loop 的任务上下文），按行阻塞输出。
struct UartEcho {
    uart: &'static imxrt10xx::lpuart::Lpuart<'static>,

    /// 手里留着的、还没交给驱动的 1 B 接收缓冲。
    ///
    /// 回调第一件事就是把它挂回驱动（双缓冲轮换），这样即使处理/输出再慢，
    /// 硬件也总有一条缓冲可用，不会因为"没挂缓冲"而整行丢字节。
    spare_rx: Cell<Option<&'static mut [u8]>>,

    /// 把阻塞的按行输出挪出中断上下文的延迟调用。
    deferred: DeferredCall,

    /// 正在累积的输入行。
    line: Cell<[u8; LINE_CAP]>,
    line_len: Cell<usize>,
    line_truncated: Cell<bool>,

    /// 待输出行的快照（`\n` 时复制，回调返回后不影响它）。
    tx_line: Cell<[u8; LINE_CAP]>,
    tx_len: Cell<usize>,
    tx_truncated: Cell<bool>,

    /// rx overrun / 重挂失败次数。作为"接收是否健康"的可观测计数。
    rx_errors: Cell<u32>,
}

impl UartEcho {
    fn new(uart: &'static imxrt10xx::lpuart::Lpuart<'static>, spare_rx: &'static mut [u8]) -> Self {
        UartEcho {
            uart,
            spare_rx: Cell::new(Some(spare_rx)),
            deferred: DeferredCall::new(),
            line: Cell::new([0; LINE_CAP]),
            line_len: Cell::new(0),
            line_truncated: Cell::new(false),
            tx_line: Cell::new([0; LINE_CAP]),
            tx_len: Cell::new(0),
            tx_truncated: Cell::new(false),
            rx_errors: Cell::new(0),
        }
    }

    /// 收到一个字节：塞进行缓冲；遇到 `\n` 就快照并触发延迟输出。
    fn push_byte(&self, byte: u8) {
        match byte {
            // 终端发来的是 CRLF；LF 才是行结束，CR 忽略。
            b'\r' => {}
            b'\n' => {
                let len = self.line_len.get();
                let line = self.line.get();
                let mut snapshot = [0u8; LINE_CAP];
                snapshot[..len].copy_from_slice(&line[..len]);

                self.tx_line.set(snapshot);
                self.tx_len.set(len);
                self.tx_truncated.set(self.line_truncated.get());

                self.line_len.set(0);
                self.line_truncated.set(false);

                // 只置位，不做输出：真正的 send_byte 在 handle_deferred_call 里。
                self.deferred.set();
            }
            _ => {
                let len = self.line_len.get();
                if len < LINE_CAP {
                    let mut line = self.line.get();
                    line[len] = byte;
                    self.line.set(line);
                    self.line_len.set(len + 1);
                } else {
                    // 超长行：丢掉超出部分，只在本行结束时提示一次。
                    self.line_truncated.set(true);
                }
            }
        }
    }
}

impl ReceiveClient for UartEcho {
    fn received_buffer(
        &self,
        rx_buffer: &'static mut [u8],
        rx_len: usize,
        rval: Result<(), kernel::ErrorCode>,
        _error: uart::Error,
    ) {
        // 1) 第一件事：把备用缓冲挂回驱动。此时驱动的 rx 状态已回到 Idle，
        //    正常必然成功；万一失败（BUSY/错误态）就计数，并尽力把本条缓冲
        //    重新挂上，绝不先做输出。
        let rearmed = match self.spare_rx.take() {
            Some(spare) => match self.uart.receive_buffer(spare, 1) {
                Ok(()) => true,
                Err((_code, spare)) => {
                    self.spare_rx.set(Some(spare));
                    false
                }
            },
            None => false,
        };

        if !rearmed {
            self.rx_errors.set(self.rx_errors.get() + 1);
            let _ = self.uart.receive_buffer(rx_buffer, 1);
            return;
        }

        // 2) 处理本条缓冲（此时它归我们所有）。只做内存搬运，不做串口输出。
        if rval.is_err() {
            // overrun/framing 等：本条内容不可信，只计数。
            self.rx_errors.set(self.rx_errors.get() + 1);
        } else {
            let n = rx_len.min(rx_buffer.len());
            for &byte in rx_buffer[..n].iter() {
                self.push_byte(byte);
            }
        }

        // 3) 本条缓冲变成下一轮回调的备用。
        self.spare_rx.set(Some(rx_buffer));
    }
}

impl DeferredCallClient for UartEcho {
    fn handle_deferred_call(&self) {
        let len = self.tx_len.get();
        let truncated = self.tx_truncated.get();
        let line = self.tx_line.get();

        // 整行的阻塞输出，在内核 loop 的任务上下文里做（不在中断回调里）。
        let mut writer = io::Writer::new(self.uart);
        let _ = write!(writer, "\r\ntock: rx line({}B): ", len);
        for &byte in line[..len].iter() {
            // 非可见字符用 '.' 代替，避免把终端搞乱。
            let printable = byte.is_ascii_graphic() || byte == b' ';
            writer.send_byte(if printable { byte } else { b'.' });
        }
        if truncated {
            writer.send_str("...[truncated]");
        }
        let errors = self.rx_errors.get();
        if errors > 0 {
            let _ = write!(writer, " rxerr={}", errors);
        }
        writer.send_str("\r\n");
    }

    fn register(&'static self) {
        self.deferred.register(self);
    }
}

/// 本板的平台资源表（最小实现：没有任何 syscall driver）。
struct TesseraPlatform {
    scheduler: NoSched,
    systick: cortexm7::systick::SysTick,
}

impl SyscallDriverLookup for TesseraPlatform {
    fn with_driver<F, R>(&self, _driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::syscall::SyscallDriver>) -> R,
    {
        // 没有任何 capsule，也就没有 driver 号。
        f(None)
    }
}

impl KernelResources<ChipHw> for TesseraPlatform {
    type SyscallDriverLookup = Self;
    type SyscallFilter = ();
    type ProcessFault = ();
    type Scheduler = NoSched;
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

/// 配置 IOMUXC：把 GPIO_AD_B0_06 / 07 切成 LPUART1 的 TX / RX（ALT2）。
///
/// 本板控制台是 GPIO_AD_B0_06(TX) / GPIO_AD_B0_07(RX)；
/// EVKB 参考板用的是 GPIO_AD_B0_12 / 13（本 API 里叫 13 / 14）。
fn setup_pins(peripherals: &'static imxrt10xx::chip::Imxrt10xxDefaultPeripherals) {
    use imxrt10xx::iomuxc::{
        DriveStrength, MuxMode, OpenDrainEn, PadId, PullKeepEn, PullUpDown, Sion, Speed,
    };

    peripherals.ccm.enable_iomuxc_clock();
    peripherals.ccm.enable_iomuxc_snvs_clock();

    // TX: GPIO_AD_B0_06
    peripherals
        .iomuxc
        .enable_sw_mux_ctl_pad_gpio(PadId::AdB0, MuxMode::ALT2, Sion::Disabled, 6);
    // RX: GPIO_AD_B0_07
    peripherals
        .iomuxc
        .enable_sw_mux_ctl_pad_gpio(PadId::AdB0, MuxMode::ALT2, Sion::Disabled, 7);

    // 两个 pin 的电气属性：与 EVKB 的 LPUART 引脚配置保持一致。
    for pin in [6, 7] {
        peripherals.iomuxc.configure_sw_pad_ctl_pad_gpio(
            PadId::AdB0,
            pin,
            PullUpDown::Pus0_100kOhmPullDown,
            PullKeepEn::Pke1PullKeeperEnabled,
            OpenDrainEn::Ode0OpenDrainDisabled,
            Speed::Medium2,
            DriveStrength::DSE6,
        );
    }
}

/// 配置并启动 LPUART1：115200 8N1，时钟源 24 MHz 晶振。
fn setup_uart(peripherals: &'static imxrt10xx::chip::Imxrt10xxDefaultPeripherals) {
    // 切换 UART 时钟源前先把所有 UART 时钟门关掉（CCM 的要求）。
    peripherals.lpuart1.disable_clock();
    peripherals.lpuart2.disable_clock();

    // TESSERA-PATCH 对侧：`lpuart.rs` 里的 SBR/OSR 已按 24 MHz 晶振改过。
    peripherals
        .ccm
        .set_uart_clock_sel(imxrt10xx::ccm::UartClockSelection::Oscillator);
    peripherals.ccm.set_uart_clock_podf(1);

    // `configure()` 会自己开时钟、软件复位 LPUART、并打开 TE/RE。
    peripherals
        .lpuart1
        .configure(uart::Parameters {
            baud_rate: 115200,
            stop_bits: uart::StopBits::One,
            parity: uart::Parity::None,
            hw_flow_control: false,
            width: uart::Width::Eight,
        })
        .expect("LPUART1 configure failed");

    // 显式再写一次分频（24 MHz / 16x 过采样 / SBR=13 ≈ 115384 Baud）。
    peripherals.lpuart1.set_baud();

    // LPUART1 的 IRQ 号是 20。
    cortexm7::nvic::Nvic::new(imxrt10xx::nvic::LPUART1).enable();
}

/// 起中断接收 + 延迟按行回显。
unsafe fn setup_echo(peripherals: &'static imxrt10xx::chip::Imxrt10xxDefaultPeripherals) {
    // 双缓冲：一条先挂给驱动，另一条留给回调里"立刻重挂"。
    let rx_first: &'static mut [u8] = static_init!([u8; 1], [0]);
    let rx_spare: &'static mut [u8] = static_init!([u8; 1], [0]);

    let echo = static_init!(UartEcho, UartEcho::new(&peripherals.lpuart1, rx_spare));

    peripherals.lpuart1.set_receive_client(echo);
    // 内核 loop 启动时会检查"创建的 deferred call 数 == 注册数"，这里必须注册。
    DeferredCallClient::register(echo);

    peripherals
        .lpuart1
        .receive_buffer(rx_first, 1)
        .expect("LPUART1 receive_buffer failed");
}

/// 程序入口（复位向量在 `.vectors` 里跳到 `initialize_ram_jump_to_main`，再到 `main`）。
#[no_mangle]
pub unsafe fn main() {
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);

    // 1. 芯片初始化（NVIC + 向量表 offset）。
    ChipHw::init();

    // 2. 延迟调用必须很早初始化。
    kernel::deferred_call::initialize_deferred_call_state::<
        <ChipHw as Chip>::ThreadIdProvider,
    >();

    // 3. CCM 与默认外设。
    let ccm = static_init!(imxrt10xx::ccm::Ccm, imxrt10xx::ccm::Ccm::new());
    let peripherals = static_init!(
        imxrt10xx::chip::Imxrt10xxDefaultPeripherals,
        imxrt10xx::chip::Imxrt10xxDefaultPeripherals::new(ccm)
    );
    peripherals.ccm.set_low_power_mode();

    // 4. 控制台：引脚 + 时钟 + 波特率 + 中断，然后挂接收/回显。
    setup_pins(peripherals);
    setup_uart(peripherals);
    setup_echo(peripherals);

    // 5. 内核数据结构。
    let processes = static_init!(
        kernel::process::ProcessArray<NUM_PROCS>,
        kernel::process::ProcessArray::new()
    );
    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(processes.as_slice()));
    let chip = static_init!(ChipHw, ChipHw::new(peripherals));

    let platform = TesseraPlatform {
        scheduler: NoSched,
        // RT1021 内核默认跑在 396 MHz（Tock 本身不改时钟树；本验证没有进程，
        // timeslice 用不到，这个值只影响 SysTick 的换算）。
        systick: cortexm7::systick::SysTick::new_with_calibration(396_000_000),
    };

    // 6. 启动横幅 —— 看到这两行就说明链接布局、时钟/引脚、内核 loop 都跑起来了。
    //    这是 Lead 验收脚本的判据，保持两行不变。
    let mut writer = io::Writer::new(&peripherals.lpuart1);
    let _ = write!(
        writer,
        "\r\ntock: tessera-rt1021 kernel boot\r\n\
         tock: kernel_loop running (echo mode)\r\n"
    );

    // 7. 进内核主循环。
    board_kernel.kernel_loop(
        &platform,
        chip,
        None::<&kernel::ipc::IPC<0>>,
        &main_loop_capability,
    );
}
