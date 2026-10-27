//! panic handler（Phase 1）：交给 BSP 的控制台 panic 路径输出后停机。
//!
//! BSP 的 `report_panic` 使用阻塞写，不依赖 Tock 的 UART 驱动；
//! 即使 panic 发生在控制台初始化之前，也不会把内核卡在自旋里（有界写）。

/// Tock 内核/板级 panic 入口。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    board_support::console::report_panic(info);
    loop {
        // SAFETY: `wfi` 只等中断，不改变任何状态。
        unsafe {
            cortexm7::support::wfi();
        }
    }
}
