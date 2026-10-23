//! 调试控制台：LPUART1 + 全项目共享的安全访问器（**平台需求 DB-2 / TS-4**）。
//!
//! # 为什么控制台要放进全局
//!
//! panic 处理器也要能写字。如果控制台只存在于某个任务的局部资源里，
//! panic 时就拿不到它 —— 而"panic 时什么都看不到"正是最需要避免的情形。
//!
//! # 为什么用 `Mutex<RefCell<Option<..>>>` 而不是自己写忙标志位
//!
//! 早期做法是一个 `Cell<bool>` 忙标志 + `static mut` 控制台。它的问题是
//! **标志位与真实借用状态会不一致**：一旦在持有期间发生 panic 或提前返回，
//! 标志位就永远为真，控制台从此哑掉。
//!
//! 现在的做法让借用状态自己说话：`try_borrow_mut` 失败即表示"正被占用"，
//! 于是 `with_console` 立刻返回 `None` 而**不阻塞**。丢一条诊断信息，
//! 好过让核心带着锁停在那里 —— 后者会让调试器也看不到原因。

use core::cell::RefCell;

use critical_section::Mutex;
use imxrt_hal as hal;
use imxrt_ral as ral;
use ports::log::{Level, LogSink, Record};
use primitives::format::FixedBuf;

use crate::config;

/// 控制台私有状态 —— 现在只剩一个**所有权令牌**。
///
/// 字段名前缀 `_`：初始化之后没有任何代码再去读这个句柄了。发送走
/// [`write_bytes_unlocked`]（环形队列 + 中断），接收走 `LPUART1` 中断自己搬进队列；
/// 两者都直接用 RAL 实例，不需要 HAL 句柄。保留它的唯一理由是**所有权**：
/// 这个 `Lpuart` 持有 LPUART1 实例与两个引脚，把它丢掉就等于放弃"这块外设归我"的声明。
///
/// 这正是"每个寄存器只能有一个属主"的同一原则在类型层的体现（发送侧那条
/// 「`DATA` 只能有一个写入者」是它在运行期的表现）。
pub struct Console {
    _uart: hal::lpuart::Lpuart,
}

impl Console {
    /// 初始化 LPUART1 并配置为 115200 8N1。
    ///
    /// # Safety
    ///
    /// 调用者必须保证：
    /// - 全程只调用一次（重复初始化会让已发出的字节丢失）；
    /// - 此前没有任何其它代码持有 LPUART1 的实例。
    pub(crate) unsafe fn new() -> Self {
        // SAFETY: 见上方 `# Safety` 段 —— 本函数由 `board::init` 在中断未使能时
        // 调用一次，CCM 实例不存在别名使用者。
        let mut ccm = unsafe { ral::ccm::CCM::instance() };

        // 时钟门控：不开门控就去写外设寄存器，写入会被静默丢弃。
        hal::ccm::clock_gate::lpuart::<1>().set(&mut ccm, hal::ccm::clock_gate::ON);

        // UART 功能时钟取 24 MHz 晶振：不依赖 PLL3 是否锁定。
        // 这样即使时钟树配错，控制台仍能出字 —— bring-up 阶段这比 0.16% 的
        // 波特率误差重要得多。
        hal::ccm::uart_clk::set_selection(&mut ccm, hal::ccm::uart_clk::Selection::Oscillator);
        hal::ccm::uart_clk::set_divider(&mut ccm, 1);

        // SAFETY: 上电初始化阶段独占 IOMUXC，且引脚对象随即被交给 UART 持有。
        let tx = unsafe { hal::iomuxc::pads::gpio_ad_b0::GPIO_AD_B0_06::new() };
        // SAFETY: 同上，RX 焊盘。
        let rx = unsafe { hal::iomuxc::pads::gpio_ad_b0::GPIO_AD_B0_07::new() };

        // SAFETY: LPUART1 实例在此被取出一次，此后只通过本模块的全局访问器使用。
        let uart_instance = unsafe { ral::lpuart::LPUART1::instance() };
        let mut uart = hal::lpuart::Lpuart::with_pins(uart_instance, hal::lpuart::Pins { tx, rx });

        uart.disable(|disabled| {
            disabled.set_baud(&hal::lpuart::Baud::compute(
                config::CONSOLE_CLOCK_HZ,
                config::CONSOLE_BAUD,
            ));
        });

        Self { _uart: uart }
    }

    /// 阻塞写出一段字节。
    ///
    /// **只允许在空闲上下文调用**（或 panic 路径）：115200 下 60 字节约 5 ms，
    /// 远超任何 ISR 预算。ISR 只允许置计数位。
    /// 写一段字节。
    ///
    /// **这是唯一的发送实现**：全局 `write_line`、`core::fmt::Write` 适配器、
    /// 日志 sink 全部走 [`write_bytes_unlocked`]。
    ///
    /// 2026-10-10 的教训：这里曾经是**独立的一份** HAL 阻塞写实现，与新的环形队列
    /// 实现并存 —— 两条路径都直接写 `DATA` 寄存器，谁也没有互斥，真机上表现为
    /// **随机丢字节**（3 字节的汉字被整体错位）。同一条数据通道只能有一个实现。
    pub fn write_bytes(&self, bytes: &[u8]) {
        write_bytes_unlocked(bytes);
    }

    /// 写一行（自动补 CRLF）。
    pub fn write_line(&self, text: &str) {
        write_bytes_unlocked(text.as_bytes());
        write_bytes_unlocked(b"\r\n");
    }

    // 这里原本还有一个"轮询读一个字节"的方法（`try_read`）：查 `STAT[RDRF]` 再读 `DATA`。
    // 2026-10-12 删除，理由与发送侧的「`DATA` 只能有一个写入者」同源：
    //
    // **接收也必须只有一个属主**。接收现在由 `LPUART1` 中断独占（`isr_drain_rx` 把字节搬进
    // 256 B 队列，上层用 `rx_pop` 取），再留一条轮询路径就意味着有人可能从 ISR 手里
    // **偷走**一个字节 —— 那是随机丢输入的隐患，而且只在"恰好有并发读取"时才现形。
    // 它当时已无调用者（由 `cargo xtask check-api-usage` 清点出来）。

    /// 清接收错误标志（`OR`/`FE`/`PF`/`NF`，都是写 1 清零）。
    ///
    /// **为什么必须在中断里做这件事**：LPUART 的接收溢出标志（`OR`）一旦置位，
    /// 若不清除，`RDRF` 就不再正常上报 —— 现场表现是"终端回显一两个字符之后
    /// 彻底没反应"，而且**不会自愈**。启动期 POST 会连续输出上百字节（阻塞式发送），
    /// 这段时间到达的输入字节足以把它撑溢出，所以这不是罕见路径。
    fn clear_rx_errors(&self) {
        let lpuart = lpuart();
        ral::write_reg!(ral::lpuart, lpuart, STAT, OR: 1, FE: 1, PF: 1, NF: 1);
    }

    /// 把接收水位设为 0：`RDRF` 在"FIFO 里只要有 1 个字节"时就置位。
    ///
    /// 水位大于 0 时，`RDRF` 要等 FIFO 攒够字节才置位，于是**一次突发输入的最后
    /// 几个字节会滞留在 FIFO 里**直到下一批数据到来 —— 真机上表现为"命令串起
    /// 上一次的残留"（例如输入 `help` 却执行了 `upmhelp`）。
    fn set_rx_watermark_zero(&self) {
        let lpuart = lpuart();
        ral::modify_reg!(ral::lpuart, lpuart, WATER, RXWATER: 0);
    }

    /// 打开接收中断（RIE）。此后每收到一个字节都会触发 LPUART1 中断。
    fn enable_rx_interrupt(&self) {
        let lpuart = lpuart();
        ral::modify_reg!(ral::lpuart, lpuart, CTRL, RIE: 1);
    }
}

/// 全局控制台。
static CONSOLE: Mutex<RefCell<Option<Console>>> = Mutex::new(RefCell::new(None));

/// 安装控制台（由 [`crate::board::init`] 调用一次）。
pub(crate) fn install(console: Console) {
    crate::rt::measured_cs(|cs| {
        *CONSOLE.borrow(cs).borrow_mut() = Some(console);
    });
}

/// 借用控制台执行一段代码；正被占用时返回 `None` 而**不阻塞**。
pub fn with_console<R>(f: impl FnOnce(&Console) -> R) -> Option<R> {
    crate::rt::measured_cs(|cs| {
        let cell = CONSOLE.borrow(cs);
        let guard = cell.try_borrow().ok()?;
        let console = guard.as_ref()?;
        Some(f(console))
    })
}

/// 通过全局控制台写一行；控制台不可用时返回 `false`。
pub fn write_line(text: &str) -> bool {
    write_bytes_unlocked(text.as_bytes());
    write_bytes_unlocked(b"\r\n");
    true
}

/// 用 `format_args!` 输出一行（**零分配**，内部用定长缓冲）。
///
/// # 为什么要有这个入口
///
/// 固件里原先反复出现这样四行样板：
///
/// ```ignore
/// let mut storage = [0u8; 192];
/// let mut buf = FixedBuf::new(&mut storage);
/// let _ = write!(buf, "…");
/// if let Ok(text) = core::str::from_utf8(buf.as_bytes()) { console::write_line(text); }
/// ```
///
/// 它有两个问题：**重复 18 次**（缓冲大小各写各的），以及**溢出时静默什么都不输出**
/// （`write!` 返回 `Err` 就被丢掉）—— 现场最怕的就是"本该有输出却什么都没有"。
/// 现在溢出会输出**已写出的部分**并追加 `…(截断)`，让"输出被截断"这件事可见。
///
/// 需要更大空间时用 [`write_line`] 自己准备缓冲；这里的 256 字节覆盖固件里
/// 所有按行输出（最长的是 MPU 区域行，约 110 字节）。
pub fn write_fmt_line(args: core::fmt::Arguments<'_>) {
    let mut storage = [0u8; 256];
    let mut buffer = FixedBuf::new(&mut storage);
    let complete = core::fmt::Write::write_fmt(&mut buffer, args).is_ok();
    if let Ok(text) = core::str::from_utf8(buffer.as_bytes()) {
        write_line(text);
        if !complete {
            write_line("…(截断)");
        }
    }
}

/// 把日志记录写成一行文本并输出。
///
/// 这是本项目在**没有 defmt** 情况下的现场可读通道：文本格式可被台架脚本解析，
/// 也能被人直接读。字段顺序固定为 `级别 秒.微秒 来源: 正文`。
/// LPUART1 寄存器句柄 —— **本模块唯一的硬件别名点**。
///
/// 中断里（`isr_drain`）与发送路径都不能经过 `CONSOLE` 那个 `RefCell`：
/// 中断可能打断正在写控制台的 idle，借用会失败；而用临界区保护阻塞发送又会
/// 饿死 1 字节深的接收 FIFO。因此接收/发送的寄存器访问走这里，理由只需论证一次。
fn lpuart() -> ral::lpuart::LPUART1 {
    // SAFETY: LPUART1 是单实例外设；这些访问只读写 STAT/DATA/WATER/CTRL 的
    // 幂等位或只读状态，不构造持有所有权的 HAL 对象，也不改变引脚配置。
    unsafe { ral::lpuart::LPUART1::instance() }
}

/// 发送环形队列深度。
///
/// 2048 字节 ≈ 一条完整命令输出的量级（`help` 约 1.7 KB）。取这个值的理由：
/// 队列足够装下一次典型输出，于是**绝大多数发送都不阻塞**。
/// 发送环形队列深度。
///
/// # 定容依据（2026-10-12 真机实测，不是估的）
///
/// 启动期的输出是**一次性突发**：`#[init]` 到第一条心跳之间要打 ~3.7 KB（实测抓到
/// 3,189 字节 + 被丢 479 字节），而这段时间里队列是**唯一**的缓冲 —— 关中断期间 ISR
/// 排不出去，中断使能后线速也只有 11.5 KB/s。
///
/// 原来的 2048 B 装不下：真机上直接看到 `txdrop=479`，`region4: 0x00000d` 这种
/// **行被截半、UTF-8 被切断**的痕迹，`make bytecheck` 因此失败。
///
/// 现在取 8192 B ≈ 实测突发的 2.2 倍，留出余量给"启动信息再长一点"的正常演进。
/// ⚠️ 定容只是把余量做大；**根治**仍是把长活（POST/忙等/基准）搬出 `#[init]`
/// （判据 `rt::Span::BootInit` 已在盯着它），以及 `bench` 的 `txdrop=0` 硬判据 ——
/// 将来谁再撑爆队列，门禁会立刻红，不会静默丢日志。
const TX_QUEUE: usize = 7680;

/// 待发字节。生产者是任意上下文（日志、终端、心跳），消费者是 `LPUART1` 中断。
#[unsafe(link_section = ".dtcm")]
static TX: Mutex<RefCell<heapless::Deque<u8, TX_QUEUE>>> =
    Mutex::new(RefCell::new(heapless::Deque::new()));

/// 写一段字节到控制台：**优先入队（不阻塞），队满才退回按字节等待**。
///
/// # 为什么不是"一律阻塞写"
///
/// 2026-10-10 之前是逐字节轮询 `TDRE` 的阻塞写：一行 60 字节 ≈ 5 ms 里 CPU 只等寄存器；
/// `help` 那种输出要 150 ms。这段时间 `idle` 被占满，串行/CAN 总线的实时余量也就无从谈起。
/// 现在发送交给 `LPUART1` 中断驱动：**CPU 只把字节放进队列就返回**。
///
/// # 为什么队满时就退回阻塞，而不是丢字节或死等
///
/// - 丢字节：日志与终端输出会残缺，现场排障最恨这个；
/// - 死等队列腾空：一旦在关中断的上下文里调用（例如 panic 路径），
///   中断排不出去 ⇒ **永远等不到** ⇒ 死锁。
///
/// 队满时**只等中断排空，绝不自己写 `DATA`**。
///
/// # 2026-10-12：这一条是**真机实测**定的，不是推理
///
/// 本函数原先还有一个分支："如果中断关着就退回按字节直写"。真机上它会被走到，
/// 于是同一时刻**有两个写入者往 `DATA` 写**，互相覆盖，症状是：
///
/// | 构造 | 结果（4096 字节可计数图案） |
/// |---|---|
/// | 纯阻塞发送（单写入者） | **4160/4160 逐字节一致** |
/// | 环形队列 8 KB（突发期间**不满**） | **4160/4160 逐字节一致** |
/// | 环形队列 2 KB（突发期间**会满**） | **整行整行地丢**（只到 53/64 行） |
/// | 环形队列 + 阻塞写混用 | 彻底乱（计数器一行都到不了） |
///
/// 结论：**`DATA` 只能有一个写入者**。中断开着时它是 ISR；中断关着时调用方应走
/// [`write_bytes_blocking`]（panic 路径正是这样），而不是从这条队列化路径里直写。
///
/// # 不关中断的理由
///
/// 见下方 `isr_service` 与模块文档：这块 LPUART 的接收 FIFO 只有 1 字节深，
/// 关中断哪怕几毫秒都会把输入吃掉。这里只在入队时短暂进入临界区（拷贝一个字节）。
pub fn write_bytes_unlocked(bytes: &[u8]) {
    let lpuart = lpuart();
    for &byte in bytes {
        // 先试一次（常见路径：队列没满，一次成功，不进临界区循环）。
        let queued =
            crate::rt::measured_cs(|cs| TX.borrow(cs).borrow_mut().push_back(byte).is_ok());
        if !queued {
            // 队满。这里有一个**必须区分的岔路**（2026-10-12 实时性审计指出）：
            //
            // - **中断开着**：`LPUART1` 中断会排空队列 ⇒ 自旋等待是安全的（等到空间才入队，
            //   并且**绝不**自己写 `DATA` —— 那会与 ISR 抢同一个寄存器、整行整行丢字节）；
            // - **中断关着**（临界区里、或 RTIC 的 `#[init]` 期间 PRIMASK=1）：
            //   中断永远不会来 ⇒ 在这里自旋就是**整机静默挂死**（无输出、不复位）。
            //
            // 审计把这个窗口量化过：`#[init]` 期间设备要输出 **1,117 B**，而 `TX_QUEUE` 是
            // 2048 B ⇒ **余量只有 931 B**。也就是说"再多打几行启动信息"就足以让设备
            // 上电即挂死，而且现场只看到"串口安静"。
            //
            // 因此关中断时改成**丢弃并计数**：少几个字节的日志，远好过一个挂死的设备。
            // 丢弃数会在每秒心跳行里以 `txdrop=` 出现 —— 静默丢弃是不允许的，必须可见。
            if interrupts_enabled() {
                while crate::rt::measured_cs(|cs| {
                    TX.borrow(cs).borrow_mut().push_back(byte).is_err()
                }) {
                    core::hint::spin_loop();
                }
            } else {
                TX_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                continue;
            }
        }
        // 有新数据了，确保"发送寄存器空"中断是开的（ISR 会在队空时自己关掉）。
        ral::modify_reg!(ral::lpuart, lpuart, CTRL, TIE: 1);
    }
}

/// 因"队满且中断关着"而丢弃的字节数。
///
/// 它**必须可见**（每秒心跳行里的 `txdrop=`）：如果只是静默丢弃，那就会变成
/// "日志偶尔少几行、没人知道为什么" —— 与"报事实"的纪律相悖。
#[unsafe(link_section = ".dtcm")]
static TX_DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 取（并清零）因队满且关中断而丢弃的字节数。
pub fn take_dropped_bytes() -> u32 {
    TX_DROPPED.swap(0, core::sync::atomic::Ordering::Relaxed)
}

/// 当前是否允许中断（`PRIMASK` 为空即"中断开着"）。
///
/// 用硬件事实而不是"我认为现在在什么上下文里"：判断错了的后果是挂死或误丢字节。
fn interrupts_enabled() -> bool {
    cortex_m::register::primask::read() == cortex_m::register::primask::Primask::Inactive
}

/// 取一个待发字节。
fn tx_pop() -> Option<u8> {
    crate::rt::measured_cs(|cs| TX.borrow(cs).borrow_mut().pop_front())
}

/// 队空时关掉发送中断（避免空转进中断）。
///
/// ⚠️ 关掉之后**必须再确认一次队列**：生产者可能恰好在我们"弹出发现队空"与
/// "关中断"之间推入字节。若不再确认，那个字节会一直躺在队列里 ——
/// 直到下一次有人写控制台才被发出去（表现为"最后一行输出少了尾巴"）。
/// 这是中断驱动队列的经典"丢唤醒"窗口。
fn tx_disable_interrupt() {
    let lpuart = lpuart();
    ral::modify_reg!(ral::lpuart, lpuart, CTRL, TIE: 0);
    // 关掉之后**必须再确认一次队列**：生产者可能恰好在我们"弹出发现队空"
    // 与"关中断"之间推入字节。不再确认的话，那个字节会一直躺在队列里，
    // 直到下一次有人写控制台才被发出（表现为"最后一行输出少了尾巴"）。
    if !tx_is_empty() {
        ral::modify_reg!(ral::lpuart, lpuart, CTRL, TIE: 1);
    }
}

/// 发送队列是否为空。
fn tx_is_empty() -> bool {
    crate::rt::measured_cs(|cs| TX.borrow(cs).borrow().is_empty())
}

/// 排空发送队列（由 `LPUART1` 中断调用）。
///
/// 只在 `TDRE` 置位时取字节；`TDRE` 未置位就返回，中断会在可写时再来。
fn tx_drain() {
    let lpuart = lpuart();
    if ral::read_reg!(ral::lpuart, lpuart, STAT, TDRE == 0) {
        return;
    }
    match tx_pop() {
        Some(byte) => {
            ral::write_reg!(ral::lpuart, lpuart, DATA, u32::from(byte))
        }
        None => tx_disable_interrupt(),
    }
    // ⚠️ **一次中断只写一个字节，然后返回**：不要在这里循环连写。
    //
    // 2026-10-10 真机定位：第一版在中断里 `loop` 连写，直到 `TDRE == 0` 才退出。
    // 结果启动输出的**后半段约 40% 的字节丢失**（汉字整段错位），而前半段完好。
    // 字节级 A/B 对照（把发送换成阻塞式 → 9/9 条逐字节一致）把范围锁在发送实现里：
    // 写完 `DATA` 之后立刻再读 `STAT` 可能仍读到"可写"的陈旧状态，于是下一次写
    // **覆盖**掉还没被移位寄存器取走的上一字节。
    //
    // "一次中断一个字节"是这类外设的规范用法：`TDRE` 会再次触发中断，
    // 连写带来的那点效率换不来正确性。
}

/// 阻塞地把已排队的字节全部送出去（供 panic 路径使用）。
///
/// panic 时可能处在关中断的临界区里：中断驱动的队列**永远不会**被排空，
/// 于是"之前排队的输出"会连同 panic 消息一起丢掉 —— 而那往往正是最需要的那几行。
pub fn flush_blocking() {
    let lpuart = lpuart();
    while let Some(byte) = tx_pop() {
        while ral::read_reg!(ral::lpuart, lpuart, STAT, TDRE == 0) {
            core::hint::spin_loop();
        }
        ral::write_reg!(ral::lpuart, lpuart, DATA, u32::from(byte));
    }
}

/// **阻塞式**写一段字节：只在"中断不可能排空队列"的场合使用（panic 路径）。
///
/// panic 时可能处在关中断的临界区里，中断驱动的队列永远排不出去；
/// 这时必须直接轮询发送，哪怕慢，也要把消息送出去 —— 看不到原因的 panic 最难查。
pub fn write_bytes_blocking(bytes: &[u8]) {
    let lpuart = lpuart();
    for &byte in bytes {
        while ral::read_reg!(ral::lpuart, lpuart, STAT, TDRE == 0) {
            core::hint::spin_loop();
        }
        ral::write_reg!(ral::lpuart, lpuart, DATA, u32::from(byte));
    }
}

/// **有界**阻塞写：等 `TDRE` 最多 `max_spins` 轮，超时就放弃并返回 `false`。
///
/// # 为什么必须有这个变体
///
/// [`write_bytes_blocking`] 是"等到 `TDRE` 置位"的**无界**自旋。2026-10-12 的实时性审计
/// 指出它的一个致命前提：**它假定 UART 有时钟**。若时钟门控/时钟根被改坏、或串口尚未初始化
/// 就 panic/故障，`STAT` 会一直读回 0 ⇒ `TDRE` 恒为 0 ⇒ **panic 处理器自己死在自旋里**，
/// 一个字节都出不来、也不复位 —— 与"用 `udf()` 而不是死循环，好让调试器能定位"的设计意图**相反**。
///
/// 因此在**故障处理器**这类"系统已经不正常"的路径上，只用有界版本：
/// 写出多少算多少，超时即放弃并把控制权交给上层（通常紧接着复位）。
///
/// 返回 `true` 表示整段写完；`false` 表示中途超时（已写出的部分**已经发出**）。
pub fn write_bytes_blocking_bounded(bytes: &[u8], max_spins_per_byte: u32) -> bool {
    let lpuart = lpuart();
    for &byte in bytes {
        let mut spins: u32 = 0;
        while ral::read_reg!(ral::lpuart, lpuart, STAT, TDRE == 0) {
            spins = spins.saturating_add(1);
            if spins >= max_spins_per_byte {
                return false;
            }
            core::hint::spin_loop();
        }
        ral::write_reg!(ral::lpuart, lpuart, DATA, u32::from(byte));
    }
    true
}

/// 非阻塞读一个控制台字节（全局访问器）。
///
/// 返回 `None` 有两种含义：没有数据，或控制台正被 panic 处理器占用 ——
/// 两者对调用方的处置相同（下轮再试），因此不再区分。
#[must_use]
/// 把控制台适配成 [`core::fmt::Write`]，让终端可以直接 `write!`。
///
/// 写失败被**刻意吞掉**：终端不该因为控制台正忙（例如 panic 正在输出）而崩溃或阻塞。
pub struct Writer;

impl core::fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_bytes_unlocked(s.as_bytes());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 接收路径：**中断驱动 + 环形队列**
// ---------------------------------------------------------------------------
//
// 2026-10-10 真机教训：最早用"在 idle 里轮询 STAT[RDRF]"收字节，结果**收到两个
// 字节后就不再上报** —— 因为发送是阻塞的（一行心跳 60 字节 = 5 ms），
// 这期间到达的字节把 LPUART 的接收 FIFO 撑溢出，而轮询路径不处理溢出标志，
// 于是接收从此"哑"掉。现场表现就是"终端回显打了两个字就卡住"。
//
// 现在的做法：RX 中断里只做"读一个字节 → 推入队列"，其余（回显、执行命令、
// 格式化）全部留在 idle。TX 阻塞再久也不会丢输入。

/// 接收环形队列深度。256 字节 ≈ 22 ms @115200，足够覆盖最长的阻塞输出。
const RX_QUEUE: usize = 256;

static RX: Mutex<RefCell<heapless::Deque<u8, RX_QUEUE>>> =
    Mutex::new(RefCell::new(heapless::Deque::new()));

/// 因接收队列满而丢弃的字节数（累计）。
///
/// 发送侧有 `txdrop=` 与硬门禁；接收侧原先**静默丢最旧字节**，没有任何计数 ——
/// 现场表现是"输入没反应/少字符"，却无法证明发生过丢弃（2026-10-19 审计）。
static RX_DROPPED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 允许接收中断开始工作（由 `board::init` 在控制台装好之后调用）。
pub(crate) fn enable_rx_interrupt() {
    // 使能"接收数据可用"中断，并打开 NVIC 里对应的线。
    // 同时清一次错误标志：启动期的连续输出可能已经让接收溢出过一次。
    with_console(|console| {
        console.clear_rx_errors();
        console.set_rx_watermark_zero();
        console.enable_rx_interrupt();
    });
}

/// 在中断里搬运一个字节：把已经到达的字节全部推入队列。
///
/// 由固件的 `LPUART1` 中断任务调用。**只做有界操作**：读寄存器 + 入队。
pub fn isr_service() {
    // 先收：接收 FIFO 只有 1 字节深，优先把它腾空（见模块文档）。
    isr_drain_rx();
    // 再发：把排队中的字节送出去。
    tx_drain();
}

/// 排空接收：把已到达的字节搬进接收队列。
fn isr_drain_rx() {
    // **刻意不走 `with_console`**：中断可能打断正在写控制台的 idle，
    // 那时 `try_borrow` 会失败，于是"这一轮一个字节都不搬" —— 表现就是
    // 字节随机丢失（真机实测：连发 5 个字符只进去 2 个）。
    // 接收路径只需要寄存器，不需要 Console 持有的那个句柄。
    //
    let lpuart = lpuart();

    // 先清接收错误标志（OR/FE/PF/NF，写 1 清零）：OR 一旦挂着不清，
    // `RDRF` 就不再正常上报，接收会永久静默。
    ral::write_reg!(ral::lpuart, lpuart, STAT, OR: 1, FE: 1, PF: 1, NF: 1);

    // 把当前已经到达的字节一次搬空。
    //
    // 判据用 `FIFO[RXEMPT]` 而**不是** `STAT[RDRF]`：`RDRF` 受水位影响，
    // 用它在突发输入时会漏掉 FIFO 里最后几个字节（真机实测的症状见
    // `set_rx_watermark_zero` 的说明）。RXEMPT 直接反映"还有没有数据"。
    loop {
        if ral::read_reg!(ral::lpuart, lpuart, FIFO, RXEMPT == 1) {
            break;
        }
        let byte = u8::try_from(ral::read_reg!(ral::lpuart, lpuart, DATA) & 0xFF).unwrap_or(0);
        crate::rt::measured_cs(|cs| {
            // 队满时丢**最旧**的字节：终端更关心最近输入。
            let mut queue = RX.borrow(cs).borrow_mut();
            if queue.push_back(byte).is_err() {
                let _ = queue.pop_front();
                let _ = queue.push_back(byte);
                RX_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        });
    }
}

/// 取走并清零"接收队列满丢弃"计数（心跳上报用）。
#[must_use]
pub fn take_rx_dropped() -> u32 {
    RX_DROPPED.swap(0, core::sync::atomic::Ordering::Relaxed)
}

/// 接收队列是否为空（不消费）。
#[must_use]
pub fn rx_empty() -> bool {
    crate::rt::measured_cs(|cs| RX.borrow(cs).borrow().is_empty())
}

/// 取一个已接收的字节（供 idle 里的终端消费）。
#[must_use]
pub fn rx_pop() -> Option<u8> {
    crate::rt::measured_cs(|cs| RX.borrow(cs).borrow_mut().pop_front())
}

/// 把控制台接成 `LogSink`：本项目的日志出口就是串口。
pub struct ConsoleSink;

impl LogSink for ConsoleSink {
    fn emit(&mut self, record: &Record<'_>) -> bool {
        let mut storage = [0u8; 160];
        let mut buffer = FixedBuf::new(&mut storage);
        use core::fmt::Write as _;
        if write!(buffer, "{record}").is_err() {
            return false;
        }
        // 刻意**不经过 `with_console`**：那会持有临界区（关中断）直到整行发完，
        // 而这块 LPUART 的接收 FIFO 只有 1 字节深 —— 关中断十几毫秒会把输入吃掉。
        // 发送只需要"环形队列 + RAL 实例"，不需要 Console 句柄。
        // 级别低于 Error 的记录也照样输出：现场问题定位通常需要上下文，
        // 而输出量受"只在空闲任务输出"这一条约束限制。
        let text = core::str::from_utf8(buffer.as_bytes()).unwrap_or("<invalid>");
        // 直接走队列，不取 Console 句柄 —— 见上一条注释（不关中断）。
        write_bytes_unlocked(text.as_bytes());
        write_bytes_unlocked(b"\r\n");
        true
    }
}

/// 把 panic 信息写出去（尽力而为，绝不阻塞）。
pub fn report_panic(info: &core::panic::PanicInfo<'_>) {
    // panic 路径**全程阻塞**：此刻可能已关中断，队列排不出去，
    // 而"看不到原因"比"慢一点"严重得多。先把已排队的输出冲出去，再报 panic。
    flush_blocking();
    write_bytes_blocking(b"\r\n!!! panic: ");
    let mut storage = [0u8; 192];
    let mut buffer = FixedBuf::new(&mut storage);
    use core::fmt::Write as _;
    if write!(buffer, "{info}").is_ok()
        && let Ok(text) = core::str::from_utf8(buffer.as_bytes())
    {
        write_bytes_blocking(text.as_bytes());
    }
    write_bytes_blocking(b"\r\n");
}

/// 当前控制台使用的日志级别（编译期常量，避免运行期比较）。
pub const MAX_LEVEL: Level = Level::Info;
