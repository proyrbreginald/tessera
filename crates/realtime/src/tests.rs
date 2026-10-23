//! `realtime` 的测试。
//!
//! 这些测试钉住的都是**会静默出错**的地方：预算的单一来源、嵌套语义、预算为 0 的含义、
//! 换算方向、以及"没记录过"与"记录到 0"的区别。

extern crate std;

use std::{format, string::String, vec::Vec};

use super::*;

/// 枚举、名字表、预算表三者的映射必须一致 —— 错位会让读数**张冠李戴**。
#[test]
fn span_的索引必须唯一且覆盖全部槽位() {
    let mut seen = [false; SPAN_COUNT];
    for span in Span::ALL.iter().copied() {
        let index = span.index();
        assert!(index < SPAN_COUNT, "{} 的索引越界", span.name());
        assert!(!seen[index], "索引 {index} 被两个 span 占用");
        seen[index] = true;
    }
    assert_eq!(Span::ALL.len(), SPAN_COUNT);
    assert!(seen.iter().all(|used| *used), "有槽位没有对应的 span");
}

/// 预算只有**一个来源**：`primitives::budget`。
///
/// 这条测试的意义是防止"实时性数字有两份"：`budget.rs` 改了、这里没跟上，
/// 于是门禁按旧预算判定通过 —— 那正是纸面预算最容易失效的方式。
#[test]
fn 预算必须来自_primitives_的单一来源() {
    assert_eq!(
        Span::IsrDeadlineTick.budget_micros(),
        budget::ISR_DEADLINE_TICK_MICROS
    );
    assert_eq!(Span::ShellInput.budget_micros(), budget::TICK_PERIOD_MICROS);
    assert_eq!(
        Span::IrqOff.budget_cycles(),
        budget::cycles_for_micros(BUDGET_IRQ_OFF_MICROS)
    );
}

/// 最硬的那条关系：关中断预算必须**严格小于** 串行总线的一个字节间隔。
#[test]
fn 关中断预算必须小于串行总线的字节间隔() {
    let budget_nanos = BUDGET_IRQ_OFF_MICROS * 1_000;
    assert!(
        budget_nanos < budget::SERIAL_BYTE_INTERVAL_NANOS,
        "关中断预算 {BUDGET_IRQ_OFF_MICROS} µs 不得达到字节间隔 {} ns",
        budget::SERIAL_BYTE_INTERVAL_NANOS
    );
}

/// 记录次数/上次/最大/累计/均值。
#[test]
fn 记录会更新全部字段且最大值不回落() {
    let mut metrics = RtMetrics::new();
    let span = Span::IsrConsoleRx;
    metrics.record(span, Cycles::new(10).unwrap());
    metrics.record(span, Cycles::new(5).unwrap());
    metrics.record(span, Cycles::new(30).unwrap());
    metrics.record(span, Cycles::new(7).unwrap());

    let stats = metrics.stats(span);
    assert_eq!(stats.count, 4);
    assert_eq!(stats.last_cycles, 7);
    assert_eq!(stats.max_cycles, 30, "最大值不得被后来的小值覆盖");
    assert_eq!(stats.total_cycles, 52);
    assert_eq!(stats.mean_cycles(), Some(13));
}

/// 超预算的判据是**严格大于**预算；等于预算算合格（预算是上限，不是"必须小于"）。
#[test]
fn 超预算判据是严格大于且边界不误报() {
    let mut metrics = RtMetrics::new();
    let span = Span::ShellInput;
    let budget_cycles = metrics.stats(span).budget_cycles;
    assert!(budget_cycles > 0, "本测试假定 shell-input 有预算");
    let at_budget = if budget_cycles > u64::from(u32::MAX) {
        u32::MAX
    } else {
        budget_cycles as u32
    };

    assert!(
        !metrics.record(span, Cycles::new(at_budget).unwrap()),
        "等于预算不应判超"
    );
    assert_eq!(metrics.stats(span).overruns, 0);

    if at_budget < u32::MAX {
        assert!(
            metrics.record(span, Cycles::new(at_budget + 1).unwrap()),
            "超一周期必须判超"
        );
    }
    assert_eq!(metrics.stats(span).overruns, 1);
    assert!(!metrics.all_within_budget());
}

/// 预算为 0 的含义是"只观测、不判定"，**不是**"随便超"。
#[test]
fn 预算为_0_时不判定也不报余量() {
    let mut stats = SpanStats::new(0);
    assert!(!stats.record(Cycles::new(u32::MAX).unwrap()));
    assert_eq!(stats.overruns, 0);
    assert_eq!(stats.headroom_percent(), None, "没预算就没有余量可言");
    assert!(!stats.over_budget());
}

/// 余量百分比：整数算术，且超预算时返回 `None`（不返回负数或 0 冒充）。
#[test]
fn 余量百分比按整数算且超预算为_none() {
    let mut stats = SpanStats::new(1_000);
    stats.record(Cycles::new(250).unwrap());
    assert_eq!(stats.headroom_percent(), Some(75));

    stats.record(Cycles::new(1_001).unwrap());
    assert_eq!(stats.headroom_percent(), None);
    assert!(stats.over_budget());
}

/// **嵌套语义**：关中断可以嵌套，只有最外层那次计时会被采纳。
///
/// 这条如果错了，最长关中断时间会被"内层的小窗口"顶掉（或重复计数放大），
/// 而报告会看起来完全正常。
#[test]
fn 嵌套的关中断只记最外层() {
    let mut metrics = RtMetrics::new();

    assert!(metrics.enter_irq_off(), "第一次进入应是最外层");
    assert!(!metrics.enter_irq_off(), "嵌套进入不是最外层");
    assert_eq!(metrics.irq_depth(), 2);

    // 内层退出：不记录。
    assert!(!metrics.exit_irq_off(Cycles::new(999).unwrap()));
    assert_eq!(metrics.stats(Span::IrqOff).count, 0);

    // 外层退出：记录。
    assert!(!metrics.exit_irq_off(Cycles::new(120).unwrap()));
    let stats = metrics.stats(Span::IrqOff);
    assert_eq!(stats.count, 1);
    assert_eq!(stats.max_cycles, 120);
    assert_eq!(metrics.irq_depth(), 0);
}

/// 多退一次（配对错误）不允许制造假数据。
#[test]
fn 多余的退出不记录任何东西() {
    let mut metrics = RtMetrics::new();
    assert!(!metrics.exit_irq_off(Cycles::new(500).unwrap()));
    assert_eq!(metrics.stats(Span::IrqOff).count, 0);
    assert_eq!(metrics.irq_depth(), 0);
}

/// `reset` 清统计但**不清嵌套深度**：在临界区里复位统计不应把当前窗口算成"从 0 开始"。
#[test]
fn reset_清空统计但不动嵌套深度() {
    let mut metrics = RtMetrics::new();
    metrics.record(Span::IsrConsoleRx, Cycles::new(42).unwrap());
    assert!(metrics.enter_irq_off());

    metrics.reset();

    assert_eq!(metrics.stats(Span::IsrConsoleRx).count, 0);
    assert_eq!(
        metrics.stats(Span::IsrConsoleRx).budget_cycles,
        Span::IsrConsoleRx.budget_cycles()
    );
    assert_eq!(metrics.irq_depth(), 1, "复位统计不应影响嵌套深度");
    assert!(
        !metrics.exit_irq_off(Cycles::new(80).unwrap()),
        "外层退出仍应记录而不是再次进入"
    );
    assert_eq!(metrics.stats(Span::IrqOff).count, 1);
}

/// 超预算清单要给出**可直接判断的数字**（span / 实测微秒 / 预算微秒 / 次数）。
#[test]
fn 超预算清单带数字而不是只给一个布尔() {
    let mut metrics = RtMetrics::new();
    // 撑爆一个预算最小的 span。
    let span = Span::IsrConsoleRx;
    let budget_cycles = metrics.stats(span).budget_cycles;
    assert!(budget_cycles > 0);
    // 用远超预算的值（预算只有 20 µs，取其 3 倍周期数）。
    let way_over = if budget_cycles * 3 > u64::from(u32::MAX) {
        u32::MAX
    } else {
        (budget_cycles * 3) as u32
    };
    metrics.record(span, Cycles::new(way_over).unwrap());

    let summary = metrics.check().expect_err("应当判为超预算");
    assert_eq!(summary.count, 1);
    assert_eq!(summary.first.span, span);
    assert!(format!("{summary}").contains("超预算"));

    let violations = metrics.violations();
    assert_eq!(violations.len(), 1);
    let item = violations.iter().next().expect("至少要有一项");
    assert_eq!(item.span, span);
    assert_eq!(item.budget_micros, span.budget_micros());
    assert!(item.max_micros > item.budget_micros);
    assert_eq!(item.overruns, 1);

    // Display 必须可读（会被打进串口日志）。
    let text = format!("{violations}");
    assert!(
        text.contains("isr-lpuart1"),
        "报告要写清是哪个 span：{text}"
    );
    assert!(text.contains("超预算"), "{text}");
}

/// 没记录过 ≠ 记录到 0：均值与余量必须是 `None`，报告要打 `-`。
#[test]
fn 没有记录时不给假数字() {
    let metrics = RtMetrics::new();
    let stats = metrics.stats(Span::ShellInput);
    assert_eq!(stats.count, 0);
    assert_eq!(stats.mean_cycles(), None, "没记录过就没有均值");
    assert_eq!(stats.headroom_percent(), None);
    assert!(metrics.all_within_budget(), "没有任何记录时不应报违反");
    assert!(metrics.check().is_ok());

    let mut text = String::new();
    metrics.report(&mut text).expect("写入 String 不会失败");
    assert!(text.contains("shell-input"));
    assert!(text.contains('-'), "空统计应显示为 -：{text}");
    // 表头之外的每一行都不得出现"单独的数字 0"（0 会被读成"耗时为零"）。
    for line in text.lines().skip(1) {
        let columns: Vec<&str> = line.split_whitespace().collect();
        // 第 2 列是 count；第 3/4 列是 last/max —— 无样本时都应为 `-`。
        assert_eq!(columns.get(1), Some(&"0"), "count 应为 0：{line}");
        assert_eq!(columns.get(2), Some(&"-"), "无样本时 last 应为 -：{line}");
        assert_eq!(columns.get(3), Some(&"-"), "无样本时 max 应为 -：{line}");
    }
}

/// 报告必须覆盖每一个 span（少一行就可能把出问题的那个藏起来）。
#[test]
fn 报告覆盖全部_span_且带预算() {
    let mut metrics = RtMetrics::new();
    metrics.record(Span::IrqOff, Cycles::new(100).unwrap());
    metrics.record(Span::IsrDeadlineTick, Cycles::new(200).unwrap());

    let mut text = String::new();
    metrics.report(&mut text).expect("写入 String 不会失败");

    for span in Span::ALL.iter().copied() {
        assert!(
            text.contains(span.name()),
            "报告缺少 {}：{text}",
            span.name()
        );
    }
    let rows = text.lines().count();
    assert_eq!(rows, Span::ALL.len() + 1, "应有表头 + 每个 span 一行");
}

/// 周期 ↔ 微秒换算必须自洽（方向、取整、CPU_HZ 的用法）。
#[test]
fn 周期与微秒换算自洽() {
    assert_eq!(micros_from_cycles(budget::CPU_HZ), 1_000_000);
    assert_eq!(micros_from_cycles(0), 0);

    // 20 µs 的周期数换算回来应当还是 20 µs（取整误差不超过 1 µs）。
    let cycles = budget::cycles_for_micros(20);
    let back = micros_from_cycles(cycles);
    assert!((19..=20).contains(&back), "20 µs 往返变成 {back} µs");

    // u32 入口与 u64 入口一致。
    assert_eq!(
        cycles_to_micros(Cycles::new(1_000).unwrap()),
        micros_from_cycles(1_000)
    );
}

/// 启动期的"故意超标"不得污染运行期判据。
///
/// 这条测试的存在理由：`boot-init` 曾实测 ≈3.25 s（2026-10-12；SDRAM 自检搬走后为 ≈8 ms），
/// 目的是把结构性盲区变成刺眼的数字。如果它参与 [`RtMetrics::check`]，
/// 那么 `rt=` 会永远报 `VIOLATION` —— 现场很快就不再相信这个信号。
#[test]
fn 启动期超标不影响运行期判据() {
    let mut metrics = RtMetrics::new();
    // 造一个"启动期严重超标"：远超 50 ms 的周期数。
    let way_over = u32::MAX;
    metrics.record(Span::BootInit, Cycles::new(way_over).unwrap());

    assert!(
        metrics.all_within_budget(),
        "启动期超标不应让运行期判据变红"
    );
    assert!(metrics.check().is_ok());

    // 但启动期自己必须能被单独看到（不能"眼不见为净"）。
    let startup = metrics.startup_violations();
    assert_eq!(startup.len(), 1, "启动期超标必须单独报出来");
    let item = startup.iter().next().unwrap();
    assert_eq!(item.span, Span::BootInit);
    assert!(item.max_micros > item.budget_micros);

    // 全量视图里也要有（`rt` 命令打的是全量）。
    assert_eq!(metrics.violations().len(), 1);
    assert!(Span::BootInit.is_startup());
    assert!(!Span::IrqOff.is_startup());
}

/// **零周期在类型上就构造不出来** —— 这是"没测到"与"测到 0"的分界。
///
/// 真机教训：DWT 没在计时 ⇒ 每次测量都是 0 周期 ⇒ 旧 API 老老实实记下来 ⇒
/// `count > 0` 而 `max == 0` ⇒ 报告显示"0 µs"、心跳报 `rt=ok`。
/// 现在 `record` 只接受 [`Cycles`]，而 `Cycles::new(0)` 返回 `None` ——
/// 调用方**只能**选择跳过这次测量，没有别的写法。
#[test]
fn 零周期无法构造() {
    assert!(Cycles::new(0).is_none(), "0 周期不是一次有效测量");
    assert_eq!(Cycles::new(1).map(Cycles::get), Some(1));
    assert_eq!(Cycles::new(u32::MAX).map(Cycles::get), Some(u32::MAX));
}

/// **疑似跨越 `CYCCNT` 回绕**的窗口：宁可当成"极长"，绝不当成"极快"。
///
/// 32 位 `CYCCNT` @396 MHz ⇒ 10.85 s 回绕。超过保护阈值的窗口，`wrapping_sub` 会给出
/// 一个**很小**的差值 —— 若不拦住，一次 11 秒的关中断会被显示成"几百微秒、很健康"。
#[test]
fn 疑似回绕的窗口必须被当成极长而不是极快() {
    // 阈值以下：正常有效测量。
    assert!(matches!(
        Measurement::from_delta(1_000),
        Some(Measurement::Valid(_))
    ));
    // 零：没测到（跳过，而不是记 0）。
    assert_eq!(Measurement::from_delta(0), None);
    // 阈值及以上：疑似回绕。
    assert_eq!(
        Measurement::from_delta(WRAP_GUARD_CYCLES),
        Some(Measurement::SuspectedWrap)
    );
    assert_eq!(
        Measurement::from_delta(u32::MAX),
        Some(Measurement::SuspectedWrap)
    );

    // 记进去之后：max 被抬到下界（绝不长成小值），且**一定**计入超预算。
    let mut metrics = RtMetrics::new();
    assert!(metrics.record_measurement(Span::IrqOff, Measurement::SuspectedWrap));
    let stats = metrics.stats(Span::IrqOff);
    assert_eq!(stats.max_cycles, WRAP_GUARD_CYCLES, "max 不得小于回绕下界");
    assert_eq!(stats.wraps, 1);
    assert_eq!(stats.overruns, 1, "疑似回绕必须算超预算");
    assert!(!metrics.all_within_budget());

    // 明细里要说清"真实长度不可知"。
    let detail = format!("{}", metrics.violations());
    assert!(detail.contains("回绕"), "{detail}");
}

/// 计数用 `u64`：`u32` 在 ~5.6×10⁵ 次/秒下 **1.3–2 小时**就会顶满并静默停住。
#[test]
fn 计数不会在两小时内饱和() {
    let mut stats = SpanStats::new(0);
    // 模拟两小时的量级（7.2e9 次 > u32::MAX 的 4.29e9）：`u32` 早就停住了。
    let two_hours_at_1mhz: u64 = 7_200_000_000;
    stats.count = two_hours_at_1mhz;
    stats.count += 1;
    assert_eq!(stats.count, two_hours_at_1mhz + 1, "u64 计数不得掉头");

    // 类型层面也钉住：字段类型是 u64（若哪天被改回 u32，这一行会编译失败）。
    let _: u64 = stats.count;
    let _: u64 = stats.overruns;
    let _: u64 = stats.wraps;
}

/// panic 路径必须能把嵌套深度清零，否则仪表此后**静默失效**。
#[test]
fn 放弃全部关中断窗口后仪表仍能继续记录() {
    let mut metrics = RtMetrics::new();
    // 模拟"在临界区里 panic"：`enter_irq_off` 执行了，**没有**配对的 `exit_irq_off`
    // （panic 直接把控制流带走）⇒ 深度**停在 1**。
    assert!(metrics.enter_irq_off());
    assert_eq!(metrics.irq_depth(), 1);

    // 之后另一个**正常配对**的关中断窗口：进入时深度变 2（被当成内层），
    // 退出时只把深度退回 1 ⇒ **这一样本被静默丢弃**，`count` 仍是 0。
    // 真机上的表现就是"表还在打、数字再也不动"。
    assert!(!metrics.enter_irq_off());
    assert!(!metrics.exit_irq_off(Cycles::new(500).unwrap()));
    assert_eq!(
        metrics.stats(Span::IrqOff).count,
        0,
        "深度泄漏后样本被静默丢弃"
    );
    assert_eq!(metrics.irq_depth(), 1, "泄漏的深度还挂在那里");

    // panic 处理器调用它之后，仪表恢复工作。
    metrics.abandon_all();
    assert_eq!(metrics.irq_depth(), 0);
    assert!(metrics.enter_irq_off());
    assert!(!metrics.exit_irq_off(Cycles::new(500).unwrap()));
    assert_eq!(metrics.stats(Span::IrqOff).count, 1);
    assert_eq!(metrics.stats(Span::IrqOff).max_cycles, 500);
}

/// 长时间运行的累计不溢出（`total_cycles` 是 u64 的理由）。
#[test]
fn 累计不会被_u32_上限截断() {
    let mut stats = SpanStats::new(0);
    stats.record(Cycles::new(u32::MAX).unwrap());
    stats.record(Cycles::new(u32::MAX).unwrap());
    assert_eq!(stats.total_cycles, u64::from(u32::MAX) * 2);
    assert_eq!(stats.count, 2);
}

/// 直方图近似 p99：无样本时是 `None`；有样本时给出**保守上界**（≥ 实际最大样本）。
#[test]
fn 直方图百分位给出保守上界且没样本时是_none() {
    let mut stats = SpanStats::new(0);
    assert_eq!(stats.p99_upper_cycles(), None);
    for cycles in [10, 20, 30, 40, 100] {
        stats.record(Cycles::new(cycles).unwrap());
    }
    let p99 = stats.p99_upper_cycles().expect("有样本");
    assert!(p99 >= 100, "p99 上界 {p99} 不得小于实际最大样本 100");
    assert!(p99 <= 255, "p99 上界 {p99} 不应跨到更高的桶");
    let p50 = stats.percentile_upper_cycles(50).expect("有样本");
    assert!(p50 >= 30, "p50 上界 {p50} 不得小于中位样本 30");
}
