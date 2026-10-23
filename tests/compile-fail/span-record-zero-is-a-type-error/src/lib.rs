//! 负例：**"0 周期样本"在类型上不可表示**。
//!
//! 真机教训（2026-10-12）：DWT 没在计时 ⇒ 每次测量拿到 0 周期 ⇒ 旧 API 照记不误 ⇒
//! `count > 0` 而 `max == 0`，于是"这项没被观测"被显示成"这项快得测不出来"，
//! 心跳还据此报 `rt=ok`。
//!
//! 现在 `record` 只接受 `Cycles`（非零由类型保证），因此下面这行**必须编译失败** ——
//! 这就是"把不变式放进类型"的可验证形式：不是靠注释提醒，也不是靠代码评审。

use realtime::{Span, SpanStats};

/// 应当编译失败：`0` 不是 `Cycles`（零周期不是一次有效测量）。
pub fn bad() -> SpanStats {
    let mut stats = SpanStats::new(0);
    stats.record(0);
    stats
}

/// 应当编译失败：即使 `Cycles` 在手，`Span` 也不能被省略。
pub fn also_bad(stats: &mut SpanStats, span: Span) {
    let _ = stats.record(span, 1);
}
