//! Panic 策略（加固版）。
//!
//! 启动完成前 panic：打印后停机（保留现场）；
//! 启动完成后 panic：打印后触发系统复位（稳定性优先）。
//! 输出走 BSP 阻塞写，不依赖 Tock 的 UART 驱动。

use core::sync::atomic::Ordering;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    board_support::console::report_panic(info);
    if crate::BOOTED.load(Ordering::Acquire) {
        cortexm7::support::reset();
    }
    loop {
        // SAFETY: `wfi` 只等中断，不改变状态。
        unsafe {
            cortexm7::support::wfi();
        }
    }
}
