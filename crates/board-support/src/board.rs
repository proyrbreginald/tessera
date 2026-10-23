//! 板级初始化：把所有硬件装配成上层可以直接使用的对象。
//!
//! # 顺序是设计的一部分
//!
//! ```text
//! 1. 时钟门控 → UART 时钟根 → 控制台      先让"看得见"成立
//! 2. 时基（PIT）                          再让"时间"成立
//! 3. 存储（FlexSPI 读）                   然后才是数据
//! 4. SDRAM（SEMC，不在启动链路上）         失败只降级，不变砖
//! ```
//!
//! 前两步失败会表现为"完全无输出"，那是最难诊断的情形；因此控制台与时基排在最前，
//! 于是后面任何一步出问题都至少有输出可看。
//!
//! # 看门狗刻意**不在**这里启动
//!
//! 板上是外部看门狗（`SGM706`，原理图 `U5`，`WDG_CTR` **高电平禁用**）。
//! 若在初始化早期就让它生效，一次 Flash 读失败会导致复位循环 ——
//! 连串口横幅都来不及打出来，现场只能看到"什么都没发生"。
//! 正确顺序是：主循环健康门控就绪之后，才允许硬件看门狗生效。

use crate::console::{self, Console};
use ports::clock::Clock;

// GAP(P5): 看门狗未启用 —— 板上是外部 SGM706（`WDG_CTR` **高电平禁用**），
// 而该信号的驱动源尚未确认：贸然让它生效，一次故障就会变成"只能断电恢复"。
// 启用的前提与顺序见本文件模块文档（健康门控就绪之后）。

use crate::sdram::{self, Sdram, SdramError};
use crate::timer::{self, PitClock, TickSource};

/// 初始化完成后的板级资源。
pub struct Board {
    /// 单调时基（只读句柄）。
    pub clock: PitClock,
    /// 1 ms 心跳通道句柄（交给中断任务清标志）。
    pub tick: TickSource,
    /// 启动阶段耗时（微秒）：由 `idle` 在**统一的启动报告**里一次打印，
    /// 而不是在初始化途中零散输出（输出顺序稳定、易读、易被脚本解析）。
    pub timings: BoardTimings,
    /// 外部 SDRAM（32 MB @ `0x8000_0000`）。
    ///
    /// ⚠️ **不在启动链路上**：初始化失败不影响启动，因此这里用 `Result` 承载，
    /// 由上层决定"报告并降级"而不是让设备变砖。
    pub sdram: Result<Sdram, SdramError>,
}

/// 启动阶段耗时（微秒）。
#[derive(Debug, Clone, Copy, Default)]
pub struct BoardTimings {
    /// 外设 + SDRAM 初始化。
    pub peripherals_us: u32,
    /// 文件系统挂载。
    pub filesystem_us: u32,
}

/// 初始化整块板。
///
/// # Safety
///
/// 必须**只调用一次**，且在调用前没有任何代码取用过 LPUART1 / PIT / IOMUXC /
/// FlexSPI 的实例。RTIC 的 `init` 保证了这个前提（它在主栈上执行一次且此时中断未使能）。
pub unsafe fn init(banner_art: &str, version: &str) -> Board {
    // SAFETY: 见函数文档 —— 独占上电初始化阶段。
    let console = unsafe { Console::new() };
    console::install(console);
    // 控制台装好之后立刻允许接收中断：终端的输入不能被阻塞式发送拖住
    // （轮询收字节的失败教训见 `console` 模块的接收路径说明）。
    console::enable_rx_interrupt();
    // banner 必须在 SDRAM/文件系统长活之前输出：启动早期中断未使能，
    // print_banner 使用阻塞写，因此用户不用等到 7 s 后才看到设备起来。
    print_banner(banner_art, version);

    // SAFETY: 同上，PIT 只初始化一次。
    unsafe { timer::init() };

    // ---- 启动阶段计时 ----
    //
    // 2026-10-10：真机上出现过"复位后 ~20 秒才出第一个字节"，而时间戳显示所有横幅行
    // 是一齐出现的 —— 也就是说延迟在**某个初始化阶段内部**，而不是输出慢。
    // 这里只**收集**耗时；打印统一放到 `idle` 的启动报告里（输出顺序稳定、不零散）。
    // 只报阶段耗时、不报累计值：调试复位（pyocd reset）不会复位 PIT 计数器。
    let t_start = crate::timer::PitClock.now().as_micros();

    // SDRAM 走 SEMC，与 Flash/控制台/时基都不共享引脚与时钟源。
    // 注意顺序：必须在控制台可用之后初始化，这样失败原因能立刻打印出来。
    // SAFETY: 独占上电初始化阶段；SEMC 与 IOMUXC 的 EMC 焊盘此前未被取用。
    let sdram = unsafe { sdram::init() }.map(|(handle, _timing)| handle);
    let peripherals_us = u32::try_from(
        crate::timer::PitClock
            .now()
            .as_micros()
            .saturating_sub(t_start),
    )
    .unwrap_or(u32::MAX);

    // FlexSPI 的**写 LUT**：Boot ROM 只装了读序列；页编程/写使能/读状态这些序列
    // 必须由固件安装。不装的结果是真机 `wq write` 返回 `Driver`（2026-10-19 实测）——
    // 擦除走 ROM 能成功，编程走寄存器路径却找不到序列。
    if !crate::flexspi_ip::install_lut() {
        console::write_line("flexspi  : LUT install FAIL —— 介质写路径不可用（FS 只读）");
    }

    // 文件系统挂载由固件在 SDRAM 备用栈上完成（索引快照解析的栈峰接近 48 KiB，
    // 不能挤占主栈）；这里只返回其它板级资源。
    let filesystem_us = 0;

    Board {
        clock: PitClock,
        // SAFETY: PIT 已由 `timer::init` 配置完成。
        tick: TickSource::new(),
        timings: BoardTimings {
            peripherals_us,
            filesystem_us,
        },
        sdram,
    }
}

/// 打印启动报告的**头部**：横幅图形 + 身份行 + 分隔线。
///
/// - `art` 来自仓库根 `banner.txt`（构建期注入，改文件即改横幅；支持 `{version}`）；
/// - 身份行给脚本/人一个稳定的"这是哪一版固件在哪块板上"；
/// - 其后的分节内容由 `firmware` 的启动报告打印（见 `print_boot_report`）。
pub fn print_banner(art: &str, version: &str) {
    // 启动早期中断尚未使能，队列化的 `write_line` 要等到 idle 才会被排空；
    // 这里直接阻塞写，保证 banner 在 SDRAM/FS 长活之前就出现在串口上。
    fn write_line(text: &str) {
        console::write_bytes_blocking(text.as_bytes());
        console::write_bytes_blocking(b"\r\n");
    }
    write_line("");
    for text in art.lines() {
        write_line(text);
    }
    let mut storage = [0u8; 128];
    let mut buffer = primitives::format::FixedBuf::new(&mut storage);
    let _ = core::fmt::Write::write_fmt(
        &mut buffer,
        format_args!("tessera {version} · MIMXRT1021CAG4A · Cortex-M7 @396 MHz"),
    );
    console::write_bytes_blocking(buffer.as_bytes());
    console::write_bytes_blocking(b"\r\n");
    write_line("------------------------------------------------------------");
}
