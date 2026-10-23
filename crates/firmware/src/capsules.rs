//! firmware 的内置服务 capsule。
//!
//! 这里只做**接线**：每个 capsule 把已有的后台服务推进包装成 `sdk::Capsule`，
//! 由 idle 的 `CapsuleSet` 统一调用；具体逻辑仍在原模块。

use ports::clock::Clock as _;
use sdk::{Capsule, CapsuleError, CapsulePhase, CapsuleStep};

/// 终端提交的后台维护作业。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceJob {
    FsGc,
    FsCheckpoint,
    LogRotate,
}

impl MaintenanceJob {
    const fn code(self) -> u8 {
        match self {
            Self::FsGc => 1,
            Self::FsCheckpoint => 2,
            Self::LogRotate => 3,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::FsGc),
            2 => Some(Self::FsCheckpoint),
            3 => Some(Self::LogRotate),
            _ => None,
        }
    }
}

/// 维护队列容量（单生产者：终端；单消费者：MaintenanceService）。
const MAINTENANCE_QUEUE: usize = 3;
/// 单个维护作业的最长总耗时；超时后取消/暂停，避免异常慢作业无限占用。
const MAINTENANCE_DEADLINE_US: u64 = 10_000_000;
/// 单个维护 step 的目标时间预算；超出后返回 `Deferred`，下一轮再继续。
const MAINTENANCE_STEP_BUDGET_US: u64 = 5_000;
/// 低 24 位是作业码，高 8 位是队列长度。
static PENDING_JOBS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 高优先级单槽：`fs checkpoint` 优先于 GC/rotate。
static HIGH_JOB: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
static CANCEL_REQUESTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 请求取消当前维护作业。
pub(crate) fn request_cancel() -> bool {
    CANCEL_REQUESTED.store(true, core::sync::atomic::Ordering::Release);
    true
}

/// 投递一个维护作业；队列满时返回 `false`。
pub(crate) fn request_maintenance(job: MaintenanceJob) -> bool {
    // checkpoint 优先：先占高优先级单槽，满了再退回普通队列。
    if job == MaintenanceJob::FsCheckpoint
        && HIGH_JOB
            .compare_exchange(
                0,
                job.code(),
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    {
        return true;
    }
    let code = u32::from(job.code());
    let mut state = PENDING_JOBS.load(core::sync::atomic::Ordering::Relaxed);
    loop {
        let count = usize::try_from(state >> 24).unwrap_or(MAINTENANCE_QUEUE);
        if count >= MAINTENANCE_QUEUE {
            return false;
        }
        let shift = u32::try_from(count.saturating_mul(8)).unwrap_or(24);
        let next = (state & 0x00FF_FFFF)
            | (code << shift)
            | (u32::try_from(count.saturating_add(1)).unwrap_or(4) << 24);
        match PENDING_JOBS.compare_exchange_weak(
            state,
            next,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => state = current,
        }
    }
}

fn take_high_priority() -> Option<MaintenanceJob> {
    let code = HIGH_JOB.swap(0, core::sync::atomic::Ordering::AcqRel);
    MaintenanceJob::from_code(code)
}

fn take_maintenance() -> Option<MaintenanceJob> {
    if let Some(job) = take_high_priority() {
        return Some(job);
    }
    let mut state = PENDING_JOBS.load(core::sync::atomic::Ordering::Acquire);
    loop {
        let count = usize::try_from(state >> 24).unwrap_or(0);
        if count == 0 {
            return None;
        }
        let code = u8::try_from(state & 0xFF).ok()?;
        let next = ((state & 0x00FF_FFFF) >> 8)
            | (u32::try_from(count.saturating_sub(1)).unwrap_or(0) << 24);
        match PENDING_JOBS.compare_exchange_weak(
            state,
            next,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return MaintenanceJob::from_code(code),
            Err(current) => state = current,
        }
    }
}

/// `fs gc` 的两个分片阶段。
#[derive(Clone, Copy, PartialEq, Eq)]
enum GcPhase {
    Dead,
    Compact,
}

/// 正在执行的维护作业。
enum ActiveJob {
    FsGc {
        phase: GcPhase,
        cursor: u32,
        reclaimed: u32,
        copied: u32,
    },
    FsCheckpoint,
    LogRotate(crate::RotateState),
}

/// 后台维护服务：把命令行里的长操作移出 Input phase。
pub(crate) struct MaintenanceService {
    active: Option<ActiveJob>,
    started_us: u64,
    overruns: u32,
}

impl MaintenanceService {
    /// 新建维护服务。
    pub(crate) const fn new() -> Self {
        Self {
            active: None,
            started_us: 0,
            overruns: 0,
        }
    }
}

impl Capsule<TerminalStepContext> for MaintenanceService {
    fn name(&self) -> &'static str {
        "maintenance"
    }

    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Maintenance
    }

    fn init(&mut self) -> Result<(), CapsuleError> {
        Ok(())
    }

    fn step(&mut self, _ctx: &mut TerminalStepContext) -> CapsuleStep {
        let start = board_support::timer::PitClock.now().as_micros();
        let outcome = self.step_inner(_ctx);
        if outcome.worked() {
            let elapsed = board_support::timer::PitClock
                .now()
                .as_micros()
                .saturating_sub(start);
            if elapsed > MAINTENANCE_STEP_BUDGET_US {
                self.overruns = self.overruns.saturating_add(1);
                return CapsuleStep::Deferred;
            }
        }
        outcome
    }
}

impl MaintenanceService {
    fn step_inner(&mut self, _ctx: &mut TerminalStepContext) -> CapsuleStep {
        // 输入优先：还有待处理字节时主动让出，避免维护作业压在输入前面。
        if !crate::console::rx_empty() {
            return CapsuleStep::Deferred;
        }
        if self.active.is_none()
            && let Some(job) = take_maintenance()
        {
            self.started_us = board_support::timer::PitClock.now().as_micros();
            self.active = match job {
                MaintenanceJob::FsGc => Some(ActiveJob::FsGc {
                    phase: GcPhase::Dead,
                    cursor: 0,
                    reclaimed: 0,
                    copied: 0,
                }),
                MaintenanceJob::FsCheckpoint => Some(ActiveJob::FsCheckpoint),
                MaintenanceJob::LogRotate => match crate::rotate_start() {
                    Some(state) => Some(ActiveJob::LogRotate(state)),
                    None => {
                        crate::console::write_line("log rotate: 后台归档失败");
                        None
                    }
                },
            };
        }
        if CANCEL_REQUESTED.swap(false, core::sync::atomic::Ordering::AcqRel) {
            if let Some(active) = self.active.take() {
                match active {
                    ActiveJob::LogRotate(state) => crate::rotate_cancel(&state),
                    ActiveJob::FsGc { .. } | ActiveJob::FsCheckpoint => {}
                }
                crate::console::write_line("maintenance: 当前作业已取消");
            } else {
                crate::console::write_line("maintenance: 当前没有后台作业");
            }
            return CapsuleStep::Worked;
        }
        if self.active.is_some() {
            let now = board_support::timer::PitClock.now().as_micros();
            if now.saturating_sub(self.started_us) > MAINTENANCE_DEADLINE_US {
                if let Some(active) = self.active.take() {
                    match active {
                        ActiveJob::LogRotate(state) => crate::rotate_cancel(&state),
                        ActiveJob::FsGc { .. } | ActiveJob::FsCheckpoint => {}
                    }
                }
                crate::console::write_line("maintenance: 作业超时，已取消/暂停");
                return CapsuleStep::Worked;
            }
        }
        // 安全抢占点：当前有 GC/rotate 正在推进时，高优先级 checkpoint 可以在
        // 下一个块边界插入执行；不打断已经开始的块擦除/压缩。
        if self.active.is_some()
            && let Some(job) = take_high_priority()
        {
            crate::execute_maintenance(job);
            return CapsuleStep::Worked;
        }
        let Some(active) = self.active.as_mut() else {
            return CapsuleStep::Idle;
        };
        match active {
            ActiveJob::FsGc {
                phase,
                cursor,
                reclaimed,
                copied,
            } => {
                let total = board_support::fs::data_blocks();
                if *phase == GcPhase::Dead && *cursor >= total {
                    *phase = GcPhase::Compact;
                    *cursor = 0;
                    return CapsuleStep::Worked;
                }
                if *phase == GcPhase::Dead {
                    return match board_support::fs::gc_dead_step(*cursor, 1) {
                        Some(Ok((count, next))) => {
                            *reclaimed = reclaimed.saturating_add(count);
                            *cursor = next;
                            if next >= total {
                                *phase = GcPhase::Compact;
                                *cursor = 0;
                            }
                            CapsuleStep::Worked
                        }
                        Some(Err(error)) => {
                            crate::console::write_fmt_line(format_args!(
                                "fs gc    : 后台死块扫描失败 {error:?}"
                            ));
                            self.active = None;
                            CapsuleStep::Worked
                        }
                        None => {
                            crate::console::write_line(
                                "fs gc    : 文件系统不可用（busy / mount failed）",
                            );
                            self.active = None;
                            CapsuleStep::Worked
                        }
                    };
                }
                match board_support::fs::compact_step(1) {
                    Some(Ok((count, done))) => {
                        *copied = copied.saturating_add(count);
                        if done {
                            crate::console::write_fmt_line(format_args!(
                                "fs gc    : 后台回收 {reclaimed} 个死块，搬移 {copied} 条记录"
                            ));
                            self.active = None;
                        }
                        CapsuleStep::Worked
                    }
                    Some(Err(error)) => {
                        crate::console::write_fmt_line(format_args!(
                            "fs gc    : 搬移压缩失败 {error:?}"
                        ));
                        self.active = None;
                        CapsuleStep::Worked
                    }
                    None => {
                        crate::console::write_line(
                            "fs gc    : 文件系统不可用（busy / mount failed）",
                        );
                        self.active = None;
                        CapsuleStep::Worked
                    }
                }
            }
            ActiveJob::FsCheckpoint => {
                crate::execute_maintenance(MaintenanceJob::FsCheckpoint);
                self.active = None;
                CapsuleStep::Worked
            }
            ActiveJob::LogRotate(state) => {
                match crate::rotate_step(state) {
                    Ok(true) => {
                        crate::console::write_line("log rotate: 后台归档完成，活动日志已清空");
                        self.active = None;
                    }
                    Ok(false) => {}
                    Err(()) => {
                        crate::console::write_line("log rotate: 后台归档失败");
                        self.active = None;
                    }
                }
                CapsuleStep::Worked
            }
        }
    }
}

/// 终端 step 需要的上下文。
pub(crate) struct TerminalStepContext {
    clock: *mut board_support::timer::PitClock,
    sdram: *mut Option<crate::Sdram>,
    /// 上一轮 capsule 失败数；HealthService 用它决定是否维持 idle 健康位。
    capsule_failures: u32,
}

impl TerminalStepContext {
    /// 构造终端上下文；裸指针只在本类型内部使用。
    pub(crate) fn new(
        clock: &mut board_support::timer::PitClock,
        sdram: &mut Option<crate::Sdram>,
        capsule_failures: u32,
    ) -> Self {
        Self {
            clock: clock as *mut _,
            sdram: sdram as *mut _,
            capsule_failures,
        }
    }

    /// 取终端需要的两个能力引用；这是上下文内部唯一的解引用点。
    fn terminal(
        &mut self,
    ) -> (
        &mut board_support::timer::PitClock,
        &mut Option<crate::Sdram>,
    ) {
        // SAFETY: 组合根保证两个指针在 step 期间有效；它们来自 idle 的局部变量，
        // 且在 step 期间不会以其他方式被借用。
        unsafe { (&mut *self.clock, &mut *self.sdram) }
    }

    pub(crate) fn capsule_failures(&self) -> u32 {
        self.capsule_failures
    }

    pub(crate) fn set_capsule_failures(&mut self, failures: u32) {
        self.capsule_failures = failures;
    }
}

/// 单个终端 capsule step 最多消费的 RX 字节数。
///
/// 防止粘贴大段输入或长命令把整轮 idle 占满；剩余字节下一轮继续。
const TERMINAL_RX_PER_STEP: usize = 64;

/// 终端输入服务。
pub(crate) struct TerminalService {
    shell: shell::shell::Shell<192, 16>,
}

impl TerminalService {
    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    pub(crate) fn new() -> Self {
        Self {
            shell: shell::shell::Shell::<192, 16>::new(),
        }
    }

    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    pub(crate) fn greet(
        &mut self,
        clock: &mut board_support::timer::PitClock,
        sdram: &mut Option<crate::Sdram>,
        writer: &mut crate::console::Writer,
    ) {
        let ctx = crate::TerminalContext { clock, sdram };
        self.shell.banner(&ctx, writer);
        self.shell.prompt(&ctx, writer);
    }
}

impl Capsule<TerminalStepContext> for TerminalService {
    fn name(&self) -> &'static str {
        "terminal"
    }
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Input
    }
    fn init(&mut self) -> Result<(), CapsuleError> {
        Ok(())
    }
    #[inline(never)]
    #[unsafe(link_section = ".xip")]
    fn step(&mut self, ctx: &mut TerminalStepContext) -> CapsuleStep {
        let mut writer = crate::console::Writer;
        let mut worked = false;
        let mut count = 0usize;
        while count < TERMINAL_RX_PER_STEP {
            let Some(byte) = crate::console::rx_pop() else {
                break;
            };
            count = count.saturating_add(1);
            worked = true;
            let (clock, sdram) = ctx.terminal();
            let mut terminal = crate::TerminalContext { clock, sdram };
            self.shell.feed(byte, &mut terminal, &mut writer);
        }
        if worked {
            CapsuleStep::Worked
        } else {
            CapsuleStep::Idle
        }
    }
}

/// 文件系统写队列的属主推进。
pub(crate) struct FsService {
    last_gc_tick: u32,
}

impl FsService {
    pub(crate) const fn new() -> Self {
        Self { last_gc_tick: 0 }
    }
}

impl Capsule<TerminalStepContext> for FsService {
    fn name(&self) -> &'static str {
        "fs"
    }
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Maintenance
    }

    fn init(&mut self) -> Result<(), CapsuleError> {
        Ok(())
    }

    fn step(&mut self, _ctx: &mut TerminalStepContext) -> CapsuleStep {
        // 满盘且队列有请求时，最多每秒尝试一次 GC；先由 GC 腾空间再推进。
        let tick = crate::TICKS.load(core::sync::atomic::Ordering::Relaxed);
        if board_support::fs::free_blocks() <= board_support::fs::reserved_blocks()
            && board_support::fs::queue_stats().pending > 0
            && tick.wrapping_sub(self.last_gc_tick) >= 1000
        {
            self.last_gc_tick = tick;
            let _ = board_support::fs::with_fs_owner(|fs| fs.gc());
        }
        match board_support::fs::step() {
            board_support::fs::StepOutcome::Idle | board_support::fs::StepOutcome::Unavailable => {
                CapsuleStep::Idle
            }
            board_support::fs::StepOutcome::Drained { ok } => {
                if ok > 0 {
                    CapsuleStep::Worked
                } else {
                    CapsuleStep::Idle
                }
            }
            board_support::fs::StepOutcome::DrainedWithFailures { .. } => CapsuleStep::Worked,
        }
    }
}

/// 健康聚合：维护 idle 健康位。
pub(crate) struct HealthService;

impl Capsule<TerminalStepContext> for HealthService {
    fn name(&self) -> &'static str {
        "health"
    }
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Health
    }

    fn init(&mut self) -> Result<(), CapsuleError> {
        crate::HEALTH.fetch_or(
            primitives::safety::health::IDLE,
            core::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }

    fn step(&mut self, ctx: &mut TerminalStepContext) -> CapsuleStep {
        if ctx.capsule_failures() == 0 {
            crate::HEALTH.fetch_or(
                primitives::safety::health::IDLE,
                core::sync::atomic::Ordering::Relaxed,
            );
        } else {
            crate::HEALTH.fetch_and(
                !primitives::safety::health::IDLE,
                core::sync::atomic::Ordering::Relaxed,
            );
        }
        CapsuleStep::Idle
    }
}

/// 文本日志落盘。
pub(crate) struct LogService;

impl Capsule<TerminalStepContext> for LogService {
    fn name(&self) -> &'static str {
        "log"
    }
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Maintenance
    }

    fn init(&mut self) -> Result<(), CapsuleError> {
        Ok(())
    }

    fn step(&mut self, _ctx: &mut TerminalStepContext) -> CapsuleStep {
        if crate::log_try_flush(false) {
            CapsuleStep::Worked
        } else {
            CapsuleStep::Idle
        }
    }
}

/// 活动日志轮转。
pub(crate) struct RotateService;

impl Capsule<TerminalStepContext> for RotateService {
    fn name(&self) -> &'static str {
        "log-rotate"
    }
    fn phase(&self) -> CapsulePhase {
        CapsulePhase::Maintenance
    }

    fn init(&mut self) -> Result<(), CapsuleError> {
        Ok(())
    }

    fn step(&mut self, _ctx: &mut TerminalStepContext) -> CapsuleStep {
        if crate::rotate_log_if_needed() {
            CapsuleStep::Worked
        } else {
            CapsuleStep::Idle
        }
    }
}
