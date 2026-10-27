//! 最小串口 Writer 与 panic handler。
//!
//! 只做一件事：把字节从 LPUART1 发出去。没有 LED，也不依赖
//! `kernel::debug::panic_old`（那需要 LED / ProcessPrinter / PanicResources，
//! 本验证板一概没有）。
//!
//! 引脚与波特率的配置在 `main.rs` 里完成（`setup_uart`）；本模块只负责输出。
//! 因此若在 `setup_uart` 之前 panic，串口不会有任何输出——这是刻意的最小化。

use core::fmt;

use imxrt10xx::ccm::Ccm;
use imxrt10xx::lpuart::Lpuart;

/// 串口 Writer：包一条 LPUART1 的引用。
///
/// 两个生命周期分别是「引用本身」和「CCM 借用」：panic handler 里会用栈上的
/// `Ccm` 临时构造一条 LPUART1 视图，这样也能复用同一个 Writer。
pub struct Writer<'a, 'c> {
    uart: &'a Lpuart<'c>,
}

impl<'a, 'c> Writer<'a, 'c> {
    /// 用一条已有的 LPUART1 引用构造 Writer。
    pub fn new(uart: &'a Lpuart<'c>) -> Self {
        Writer { uart }
    }

    /// 直接发送一个字节（阻塞等待 TDRE/TC）。
    pub fn send_byte(&mut self, byte: u8) {
        self.uart.send_byte(byte);
    }

    /// 直接发送一段字节。
    pub fn send_str(&mut self, s: &str) {
        for byte in s.bytes() {
            self.uart.send_byte(byte);
        }
    }
}

impl fmt::Write for Writer<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.send_str(s);
        Ok(())
    }
}

/// Panic handler：打印 `PANIC` 与 `info`，然后停在这里等复位。
///
/// 刻意不尝试打印回溯/进程信息：本板没有进程，也没有调试依赖。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // 用一条独立构造的 LPUART1 视图输出。若 `setup_uart` 尚未跑过，这里
    // 可能会卡在 `send_byte` 的轮询上——同样属于可接受的最小化行为。
    let ccm = Ccm::new();
    let uart = Lpuart::new_lpuart1(&ccm);
    let mut writer = Writer::new(&uart);

    writer.send_str("\r\nPANIC: ");
    let _ = fmt::write(&mut writer, format_args!("{}\r\n", info));
    writer.send_str("tock: halted in panic handler\r\n");

    loop {
        // SAFETY: 让 CPU 进入低功耗等待中断状态；本函数永不返回。
        unsafe {
            cortexm7::support::wfi();
        }
    }
}
