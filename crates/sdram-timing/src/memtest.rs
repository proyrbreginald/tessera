//! 外部内存（SDRAM）测试算法：地址线、数据线、图样与伪随机。
//!
//! # 为什么这些算法要独立于硬件
//!
//! SDRAM 初始化写错时，现场表现往往是"内存随机出错"——这是最难查的一类故障：
//! 它可能是地址线粘连（两根地址线短在一起，两个地址映到同一个存储单元）、
//! 数据线固定（某条数据线恒低/恒高）、行/列译码错误，也可能是刷新没配好导致
//! 数据放一会儿就变。这些**故障模式**都有标准图样可以检出，而图样本身与
//! 具体芯片、具体寄存器无关。
//!
//! 因此算法放在纯逻辑层，只通过 [`MemoryDevice`] 这一个接口访问内存：
//!
//! - 主机上可以用一个**会故意出错**的替身（见本文件 `tests` 里的 `MockMem`）
//!   验证"算法真的能抓到错"，而不只是验证"跑完没报错"——后者是自欺欺人；
//! - 固件侧只需要实现 3 个方法就能复用全部算法，BSP 不必再抄一遍图样。
//!
//! # 取舍一：首错即停，而不是统计错误总数
//!
//! 现场诊断关心的是"**最早**错在哪、期望什么、读回什么"，而不是"一共错了
//! 多少个字节"。一块坏内存往往有几十万个错误，把它们数完只会让测试多跑几分钟
//! 而信息量为零（第一个错的现场已经足以定位是哪根线/哪一片）。所以四个测试都在
//! **首个不相等处记录现场并提前返回**，同时 [`MemTestReport::errors`] 仍然保证
//! 至少为 1。
//!
//! 代价是 `errors` 在失败时通常就等于 1，不能当作"坏得有多严重"的度量。
//! 这是刻意的：需要更细的结论时，应该换用不同的图样重跑，而不是看一个计数。
//!
//! # 取舍二：只按 32 位字访问
//!
//! 接口只有 `read32` / `write32`，因此容量末尾不足一个字的字节**无法被访问到**，
//! 也就无法测试（`len_bytes() / 4` 向下取整）。这对 SDRAM 是合理的：数据总线
//! 就是 32 位，只测整字不会漏掉任何一条真实的数据线。
//!
//! # 取舍三：`random_test` 不准备镜像缓冲区
//!
//! 本 crate 是 `no_std` 且全项目零堆分配，不可能为整块内存准备一份"应该写入什么"
//! 的镜像。校验时按**同一个 `seed`** 重跑一遍同一个确定性 PRNG 即可复现写入序列
//! （[`primitives::prng::Rng`] 的 xorshift64* 序列只由种子决定）。
//! 这要求写遍与校验遍的**遍历次数与顺序严格一致**——两遍都是完整的 `0..字数`，
//! 中途不提前退出，所以这一点由代码结构本身保证，而不是靠约定。

use primitives::prng::Rng;

/// 被测内存：以 32 位字访问，偏移以**字节**计。
///
/// # 实现契约
///
/// - 测试只会访问 `0..len_bytes()`，且偏移只会取 **4 的倍数**；
/// - 对于 4 的倍数的偏移 `off`，保证 `off + 4 <= len_bytes()`；
/// - 因此实现方无需自行处理越界，也不必支持非对齐访问。
///
/// 契约故意收得这么窄：它是"固件侧的实现足够薄"的前提，
/// 也是主机侧替身能用一个 `Vec<u32>` 就写完的原因。
pub trait MemoryDevice {
    /// 容量（字节）。测试只会访问 `0..len_bytes()`。
    fn len_bytes(&self) -> u32;

    /// 读一个 32 位字。`offset` 为字节偏移，且必为 4 的倍数。
    fn read32(&mut self, offset: u32) -> u32;

    /// 写一个 32 位字。`offset` 为字节偏移，且必为 4 的倍数。
    fn write32(&mut self, offset: u32, value: u32);
}

/// 测试种类，用于在报告里标明"这份现场是哪个测试留下的"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    /// 地址线测试（[`address_bus_test`]）。
    AddressBus,
    /// 数据线走 1 / 走 0 测试（[`data_bus_test`]）。
    DataBus,
    /// 固定图样全容量测试（[`pattern_test`]）。
    Pattern,
    /// 伪随机两遍测试（[`random_test`]）。
    Random,
}

/// 第一个出错的现场（用于现场诊断：偏移 + 期望值 + 实际值）。
///
/// 三个字段缺一不可：只有偏移说不清是"写丢了"还是"读错了"；
/// 而期望值与实际值的**差异位**往往直接指出是哪条数据线或哪条地址线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemError {
    /// 出错处的字节偏移。
    pub offset: u32,
    /// 期望读回的值。
    pub expected: u32,
    /// 实际读回的值。
    pub actual: u32,
}

/// 一次内存测试的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemTestReport {
    /// 本次测试的种类。
    pub kind: TestKind,
    /// **实际做过比较**的字节数（不是容量）。
    ///
    /// 定义是"校验遍里逐字比对过的字节数之和"：写入阶段不计（写无法失败），
    /// 而一个测试可能有多个校验遍——[`random_test`] 有 A 与 `~A` 两遍，
    /// 所以在满容量通过时它等于容量的 **2 倍**。首错即停时，它反映的是
    /// "扫到哪里就停了"，这正是现场需要的"测试实际覆盖了多少"。
    pub bytes_tested: u32,
    /// 错误计数。首错即停，因此失败时通常为 1；加法是**饱和**的，
    /// 不会在长循环里溢出 panic。至少为 1（有错时）。
    pub errors: u32,
    /// 第一个出错的现场；全通过时为 `None`。
    pub first_error: Option<MemError>,
}

impl MemTestReport {
    /// 是否通过（`errors == 0`）。
    #[must_use]
    pub fn passed(&self) -> bool {
        self.errors == 0
    }
}

/// 测试过程中的累计器。
///
/// 四个测试的"计数—比较—记录现场—首错即停"逻辑完全一样，只有遍历方式不同。
/// 把这段逻辑收在一处，是为了让两个不变量只存在一份实现：
/// **饱和的错误计数**与**首错即停**。分散在四个函数里，迟早会有一处漏掉。
#[derive(Debug)]
struct Recorder {
    kind: TestKind,
    bytes_tested: u32,
    errors: u32,
    first_error: Option<MemError>,
}

impl Recorder {
    /// 开始一次测试。
    const fn begin(kind: TestKind) -> Self {
        Self {
            kind,
            bytes_tested: 0,
            errors: 0,
            first_error: None,
        }
    }

    /// 记下"刚刚比较过一个字（4 字节）"。
    ///
    /// 用饱和加法：容量上限是 4 GiB，而 [`random_test`] 要校验两遍，
    /// 在 `u32::MAX` 附近理论上有回绕空间；`overflow-checks = true` 下那会 panic。
    fn count_verified(&mut self) {
        self.bytes_tested = self.bytes_tested.saturating_add(4);
    }

    /// 比较期望值与实际值；返回 `true` 表示**已经出现首个错误，调用方应立刻收尾返回**。
    ///
    /// 错误计数用 `saturating_add`：本模块首错即停，正常情况下 `errors` 只可能是
    /// 0 或 1；但 `overflow-checks = true` 下，一旦将来有人把它改成"继续跑完并统计
    /// 总数"，长循环里的 `+= 1` 就会在 `u32::MAX` 处 panic。用一个不会 panic 的
    /// 饱和加法把这条路提前堵死——**测试代码本身不该成为新的故障源**。
    fn check(&mut self, offset: u32, expected: u32, actual: u32) -> bool {
        if expected == actual {
            return false;
        }
        self.errors = self.errors.saturating_add(1);
        if self.first_error.is_none() {
            self.first_error = Some(MemError {
                offset,
                expected,
                actual,
            });
        }
        true
    }

    /// 收尾成对外报告。
    fn finish(self) -> MemTestReport {
        MemTestReport {
            kind: self.kind,
            bytes_tested: self.bytes_tested,
            errors: self.errors,
            first_error: self.first_error,
        }
    }
}

/// 容量能吃下多少个完整的 32 位字（向下取整）。
///
/// 末尾不足一个字的字节无法通过 [`MemoryDevice`] 访问，因此不参与测试。
const fn word_count(len_bytes: u32) -> u32 {
    len_bytes / 4
}

/// 探针偏移 → 探针图样。
///
/// 取反是最省事的一一映射：**偏移不同 ⇒ 图样必不同**。地址线粘连会把两个偏移
/// 映到同一个单元，于是"后写的覆盖先写的"必然让先写的那个探针读回别人的图样，
/// 差异立刻可见。
const fn probe_pattern(offset: u32) -> u32 {
    !offset
}

/// 生成一遍伪随机数据并写入（`invert == true` 时写入 `~A`）。
///
/// 校验遍用同样的 `(seed, 字数, invert)` 调用就能复现这一序列，
/// 因此不需要为整块内存准备镜像缓冲区。
fn write_random_pass(dev: &mut impl MemoryDevice, seed: u64, words: u32, invert: bool) {
    let mut rng = Rng::new(seed);
    for index in 0..words {
        let value = if invert {
            !rng.next_u32()
        } else {
            rng.next_u32()
        };
        dev.write32(index * 4, value);
    }
}

/// 复现一遍伪随机数据并逐字校验；返回 `true` 表示已出错（现场已记入 `rec`）。
///
/// 之所以每次都重新 `Rng::new(seed)` 而不是复用同一个 RNG 实例：写遍与校验遍之间
/// 不允许有"共享的推进状态"，否则任何一次提前退出都会让两边错位，而错位是
/// **静默**的——测试会"通过"，只是通过得毫无意义。重新播种让两遍各自独立、
/// 可以单独推理。
fn verify_random_pass(
    dev: &mut impl MemoryDevice,
    rec: &mut Recorder,
    seed: u64,
    words: u32,
    invert: bool,
) -> bool {
    let mut rng = Rng::new(seed);
    for index in 0..words {
        let value = if invert {
            !rng.next_u32()
        } else {
            rng.next_u32()
        };
        let offset = index * 4;
        let actual = dev.read32(offset);
        rec.count_verified();
        if rec.check(offset, value, actual) {
            return true;
        }
    }
    false
}

/// 地址线测试：在 `0` 与各 **2 的幂次字节偏移**（`4, 8, 16, ...`）处写入
/// **互不相同**且与该偏移一一对应的图样，全部写完之后再整体读回校验。
///
/// # 为什么这样能抓到地址线粘连 / 译码错误
///
/// 若地址线 `k` 粘连（`2^k` 与 `0` 映到同一个存储单元），"写 `2^k`"就会**覆盖**
/// "写 `0`"留下的数据。两阶段的顺序是刻意的：如果边写边读，后面探针的写入反而
/// 会掩盖前面探针被折叠的事实。图样取 `!offset`，偏移不同则图样必不同，
/// 所以任何折叠都会变成一次可见的不相等。
///
/// # 为什么只走 2 的幂次偏移
///
/// 第 `k` 个探针恰好只把第 `k` 条地址线拉高，**用最少的访问次数覆盖每一条地址线**
/// ——地址线只有几十条，而单元有上千万个。全容量逐字比对是 [`pattern_test`] 的活，
/// 两者互补：这里查"地址是否唯一译码"，那里查"每个单元是否记得住"。
///
/// 偏移 `0` 也放进探针表：它不是 2 的幂，但地址折叠最典型的后果就是把高地址
/// 折回到 0 号单元，漏掉它就漏掉了最常见的症状。
#[must_use]
pub fn address_bus_test(dev: &mut impl MemoryDevice) -> MemTestReport {
    let mut rec = Recorder::begin(TestKind::AddressBus);
    let words = word_count(dev.len_bytes());
    if words == 0 {
        // 容量不足一个字：没有任何单元可访问，空报告（`passed()` 为真）。
        return rec.finish();
    }

    // 探针表。32 个槽足够覆盖"容量 < 4 GiB"下所有 2 的幂次字节偏移，
    // 所以这里不需要动态容器（本 crate 是 no_std、零堆分配）。
    let mut probes = [0u32; 32];
    let mut count = 0usize;
    if let Some(slot) = probes.get_mut(count) {
        *slot = 0;
        count += 1;
    }
    // 从 4 起：字节偏移 4 恰好是"第 0 条有效地址线"（数据宽度 32 位，低 2 位不是地址线）。
    let mut probe = 4u32;
    loop {
        // `probe / 4 >= words` 等价于 `probe + 4 > len_bytes`，即该探针跨出容量。
        if probe / 4 >= words {
            break;
        }
        match probes.get_mut(count) {
            Some(slot) => {
                *slot = probe;
                count += 1;
            }
            None => break,
        }
        match probe.checked_mul(2) {
            Some(next) => probe = next,
            None => break,
        }
    }

    // 写阶段：先把所有探针铺满，再统一读回。
    for probe in probes.iter().take(count) {
        dev.write32(*probe, probe_pattern(*probe));
    }

    // 读阶段：按写序（低地址在前）读回，第一个不相等即记录现场并收尾。
    for probe in probes.iter().take(count) {
        let expected = probe_pattern(*probe);
        let actual = dev.read32(*probe);
        rec.count_verified();
        if rec.check(*probe, expected, actual) {
            return rec.finish();
        }
    }
    rec.finish()
}

/// 数据线测试：在若干**代表性地址**上做 32 位走 1（`1 << b`）与走 0（`!(1 << b)`）
/// 并立即读回，抓数据线固定（某条数据线恒低或恒高）。
///
/// # 为什么走 1 与走 0 都要做
///
/// 走 1 每个字只有一位为 1：某位**恒低**时这一位写不进去，读回少了它，必被抓住。
/// 走 0 每个字只有一位为 0：某位**恒高**时读回多了它，同样必被抓住。
/// 只做其中一个方向就只能抓一半的故障——这是本测试存在的全部理由。
///
/// # 为什么只在代表性地址上做
///
/// "数据线固定"与"哪个单元"几乎无关，而与"哪条数据线"有关。全容量做 64 遍
/// （32 走 1 + 32 走 0）只会在真机上白白多花几十秒，抓错能力并不增加。
/// 取**首、中、末**三个字：行/列译码造成的个别单元差异通常出现在边界与中点，
/// 三个点足以覆盖；容量只有一个字时自然退化为只测那一个字。
#[must_use]
pub fn data_bus_test(dev: &mut impl MemoryDevice) -> MemTestReport {
    let mut rec = Recorder::begin(TestKind::DataBus);
    let words = word_count(dev.len_bytes());
    if words == 0 {
        return rec.finish();
    }

    // `words >= 1`，所以 `words - 1` 不会下溢（`overflow-checks = true` 下这条很重要）。
    let candidates = [0u32, words / 2, words - 1];
    let mut targets = [0u32; 3];
    let mut count = 0usize;
    for word in candidates {
        // 小容量（1~2 个字）时首/中/末会重合，去重避免重复劳动。
        if targets.iter().take(count).any(|seen| *seen == word) {
            continue;
        }
        if let Some(slot) = targets.get_mut(count) {
            *slot = word;
            count += 1;
        }
    }

    for word in targets.iter().take(count) {
        let offset = word * 4;
        for bit in 0..32u32 {
            // `bit < 32`，移位不会越界，也不会触发溢出检查。
            let walking_one = 1u32 << bit;
            dev.write32(offset, walking_one);
            rec.count_verified();
            if rec.check(offset, walking_one, dev.read32(offset)) {
                return rec.finish();
            }

            let walking_zero = !walking_one;
            dev.write32(offset, walking_zero);
            rec.count_verified();
            if rec.check(offset, walking_zero, dev.read32(offset)) {
                return rec.finish();
            }
        }
    }
    rec.finish()
}

/// 图样测试：用同一个图样**全容量**写一遍，再全容量读回校验。
///
/// 典型图样：`0xFFFF_FFFF` / `0x0000_0000` / `0xAAAA_AAAA` / `0x5555_5555`。
/// 前两个抓"固定电平"（写不进 1 或写不进 0）；后两个是相邻位交替的棋盘图样，
/// 抓相邻数据线之间的短路/串扰——这是全 0/全 1 图样在原理上就看不见的。
///
/// 写读分两遍而不是边写边读：分两遍时数据要在存储单元里**停留**一整个写遍，
/// 于是刷新没配好（数据放一会儿就变）这类故障也会顺带暴露出来。
#[must_use]
pub fn pattern_test(dev: &mut impl MemoryDevice, pattern: u32) -> MemTestReport {
    let mut rec = Recorder::begin(TestKind::Pattern);
    let words = word_count(dev.len_bytes());

    // 写遍：只写不比较，因此没有"失败"的可能，一定完整跑完。
    for index in 0..words {
        dev.write32(index * 4, pattern);
    }

    // 校验遍：首错即停。
    for index in 0..words {
        let offset = index * 4;
        let actual = dev.read32(offset);
        rec.count_verified();
        if rec.check(offset, pattern, actual) {
            return rec.finish();
        }
    }
    rec.finish()
}

/// 伪随机测试：用确定性 PRNG 生成数据，全容量两遍——先"写 A、校验 A"，
/// 再"写 `~A`、校验 `~A`"。`seed` 用于复现。
///
/// # 为什么要两遍互补
///
/// 只写一遍随机数时，某条数据线恒低也可能"碰巧"通过：随机数据里约一半的字
/// 在那一位上本来就是 0。把每个字再取反写一遍，等于对每一位都做了 0/1 双向覆盖。
/// 与此同时，随机数据会覆盖大量"同行不同列 / 同列不同行"的比特组合，
/// 这是少数几个固定图样做不到的——它把译码错误变成大概率事件而不是碰运气。
///
/// 数据来源是 [`primitives::prng::Rng`]（xorshift64*）：纯算术、无系统熵源，
/// 因此在固件里也能跑，且同一 `seed` 在任何机器、任何优化级别下都产生同一序列。
/// 序列确定性是这一层能被**离线复现**的前提：现场报一个 seed 就能在主机上重放。
#[must_use]
pub fn random_test(dev: &mut impl MemoryDevice, seed: u64) -> MemTestReport {
    let mut rec = Recorder::begin(TestKind::Random);
    let words = word_count(dev.len_bytes());

    // 第一遍：A。
    write_random_pass(dev, seed, words, false);
    if verify_random_pass(dev, &mut rec, seed, words, false) {
        return rec.finish();
    }

    // 第二遍：~A。只有第一遍完整通过才会走到这里。
    write_random_pass(dev, seed, words, true);
    if verify_random_pass(dev, &mut rec, seed, words, true) {
        return rec.finish();
    }
    rec.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 会**故意出错**的内存替身。
    ///
    /// 这个类型是本文件存在意义的证明：只有能造出确定的故障，才能断言
    /// "算法真的抓到了它"，而不是"算法没报错"。
    ///
    /// - `stuck_bit`：位掩码，命中的数据线**恒读为 0**（对地短路/固定低）。
    ///   写入时也把命中的位清掉——物理上"线是死的"，写进去也留不住那个 1。
    /// - `alias_shift`：忽略字节地址的第 `alias_shift` 位（模拟该地址线粘连/断开），
    ///   于是两个相距 `2^alias_shift` 的偏移会落到同一个字。
    struct MockMem {
        words: Vec<u32>,
        stuck_bit: Option<u32>,
        alias_shift: Option<u32>,
    }

    impl MockMem {
        /// 健康内存：容量为 `len_bytes` 的整字（向下取整），无故障注入。
        fn healthy(len_bytes: u32) -> Self {
            let count = usize::try_from(len_bytes / 4).unwrap();
            Self {
                words: vec![0; count],
                stuck_bit: None,
                alias_shift: None,
            }
        }

        /// 注入"数据线固定低"：`mask` 命中的位永远读回 0。
        fn with_stuck_bit(self, mask: u32) -> Self {
            Self {
                stuck_bit: Some(mask),
                ..self
            }
        }

        /// 注入"地址线粘连"：字节地址第 `shift` 位被忽略。
        fn with_alias_shift(self, shift: u32) -> Self {
            Self {
                alias_shift: Some(shift),
                ..self
            }
        }

        /// 字节偏移 → 字下标：先应用地址线粘连，再落到字。
        fn index_of(&self, offset: u32) -> usize {
            let effective = match self.alias_shift {
                Some(shift) => offset & !(1u32 << shift),
                None => offset,
            };
            usize::try_from(effective / 4).unwrap()
        }

        /// 替身同时充当**契约检查器**：算法一旦越界或非对齐访问，测试立刻失败。
        fn assert_in_contract(&self, offset: u32) {
            assert_eq!(offset % 4, 0, "内存测试只允许按字对齐访问，收到 {offset}");
            assert!(
                offset + 4 <= self.len_bytes(),
                "越界访问：offset={offset}，容量={}",
                self.len_bytes()
            );
        }
    }

    impl MemoryDevice for MockMem {
        fn len_bytes(&self) -> u32 {
            u32::try_from(self.words.len() * 4).unwrap()
        }

        fn read32(&mut self, offset: u32) -> u32 {
            self.assert_in_contract(offset);
            let index = self.index_of(offset);
            let stored = self.words.get(index).copied().unwrap_or(0);
            match self.stuck_bit {
                Some(mask) => stored & !mask,
                None => stored,
            }
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.assert_in_contract(offset);
            let index = self.index_of(offset);
            let stored = match self.stuck_bit {
                Some(mask) => value & !mask,
                None => value,
            };
            if let Some(slot) = self.words.get_mut(index) {
                *slot = stored;
            }
        }
    }

    /// 容量不是 4 的倍数时的替身：只把"对外容量"报小几个字节，其余照旧。
    ///
    /// 单独做一层包装而不是给 `MockMem` 加字段，是为了让 `MockMem` 始终只有
    /// 需求里那三个字段——替身越简单，出问题时越不需要怀疑它。
    struct RaggedLen {
        inner: MockMem,
        len: u32,
    }

    impl MemoryDevice for RaggedLen {
        fn len_bytes(&self) -> u32 {
            self.len
        }

        fn read32(&mut self, offset: u32) -> u32 {
            self.inner.read32(offset)
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.inner.write32(offset, value)
        }
    }

    /// 需求里的四种标准图样。
    const STANDARD_PATTERNS: [u32; 4] = [0xFFFF_FFFF, 0x0000_0000, 0xAAAA_AAAA, 0x5555_5555];

    #[test]
    fn healthy_memory_passes_all_four_tests() {
        let mut mem = MockMem::healthy(4096);

        let address = address_bus_test(&mut mem);
        assert!(address.passed(), "健康内存的地址线测试不应失败");
        assert_eq!(address.kind, TestKind::AddressBus);
        // 探针是 0 与 4..=2048 的 2 的幂：共 11 个，每个验证 4 字节。
        assert_eq!(address.bytes_tested, 11 * 4);

        let data = data_bus_test(&mut mem);
        assert!(data.passed(), "健康内存的数据线测试不应失败");
        assert_eq!(data.kind, TestKind::DataBus);
        // 首/中/末三个字 × 32 位 × (走 1 + 走 0) 次比较 × 4 字节。
        assert_eq!(data.bytes_tested, 3 * 32 * 2 * 4);

        let pattern = pattern_test(&mut mem, STANDARD_PATTERNS[0]);
        assert!(pattern.passed());
        assert_eq!(pattern.kind, TestKind::Pattern);
        assert_eq!(pattern.bytes_tested, 4096, "全容量图样测试应扫过全部容量");
        assert!(pattern.first_error.is_none());

        let random = random_test(&mut mem, 0x1234_5678_9ABC_DEF0);
        assert!(random.passed());
        assert_eq!(random.kind, TestKind::Random);
        // 两遍校验（A 与 ~A），所以满容量通过时是容量的 2 倍。
        assert_eq!(random.bytes_tested, 8192);
        assert_eq!(random.errors, 0);
        assert!(random.first_error.is_none());
    }

    #[test]
    fn pattern_test_passes_all_standard_patterns_on_healthy_memory() {
        for pattern in STANDARD_PATTERNS {
            let mut mem = MockMem::healthy(1024);
            let report = pattern_test(&mut mem, pattern);
            assert!(report.passed(), "图样 {pattern:#010X} 在健康内存上不应失败");
            assert_eq!(report.bytes_tested, 1024);
            // 不只是"没报错"：图样必须真的写进了每一个字。
            assert!(
                mem.words.iter().all(|word| *word == pattern),
                "图样 {pattern:#010X} 没有真正写入全部单元"
            );
        }
    }

    #[test]
    fn data_bus_walking_zero_catches_a_stuck_low_line() {
        // 第 7 位恒低。走 0 图样里其他位都是 1，于是第一次比较就少了第 7 位。
        let mut mem = MockMem::healthy(64).with_stuck_bit(0x0000_0080);
        let report = data_bus_test(&mut mem);
        assert!(!report.passed(), "数据线固定必须被数据线测试抓到");
        assert_eq!(report.kind, TestKind::DataBus);
        assert_eq!(report.errors, 1);
        assert_eq!(
            report.first_error,
            Some(MemError {
                offset: 0,
                expected: 0xFFFF_FFFE,
                actual: 0xFFFF_FF7E,
            })
        );
        // 首错即停：只走到第一个字的第 0 位——走 1 那次比较（通过）与走 0 那次
        // 比较（失败）各记 4 字节，之后 63 次比较与其余两个字都没扫到。
        assert_eq!(report.bytes_tested, 8);
    }

    #[test]
    fn data_bus_walking_one_catches_a_dead_low_byte() {
        // 低 8 位全恒低。走 1 图样只把一位拉高，第 0 位就写不进去。
        let mut mem = MockMem::healthy(64).with_stuck_bit(0x0000_00FF);
        let report = data_bus_test(&mut mem);
        assert!(!report.passed(), "恒低的数据线必须被走 1 图样抓到");
        assert_eq!(
            report.first_error,
            Some(MemError {
                offset: 0,
                expected: 0x0000_0001,
                actual: 0x0000_0000,
            })
        );
    }

    #[test]
    fn address_bus_catches_an_aliased_address_line() {
        // 忽略字节地址第 10 位：偏移 1024 与偏移 0 落到同一个字。
        let mut mem = MockMem::healthy(4096).with_alias_shift(10);
        let report = address_bus_test(&mut mem);
        assert!(!report.passed(), "地址线粘连必须被地址线测试抓到");
        assert_eq!(report.kind, TestKind::AddressBus);
        assert!(report.first_error.is_some());
        // 探针 0 先被读回，读到的却是探针 1024 的图样（它后写，覆盖了 0 号单元）。
        assert_eq!(
            report.first_error,
            Some(MemError {
                offset: 0,
                expected: probe_pattern(0),
                actual: probe_pattern(1024),
            })
        );
    }

    #[test]
    fn address_bus_writes_a_distinct_pattern_per_probe() {
        // 防"空转"：健康内存跑完之后，每个探针单元里必须躺着它自己的图样。
        // 否则测试可能只是"什么都没写、也什么都没比"。
        let mut mem = MockMem::healthy(64);
        assert!(address_bus_test(&mut mem).passed());
        for probe in [0u32, 4, 8, 16, 32] {
            let index = usize::try_from(probe / 4).unwrap();
            assert_eq!(
                mem.words.get(index).copied(),
                Some(probe_pattern(probe)),
                "探针 {probe} 的图样没有落盘"
            );
        }
    }

    #[test]
    fn pattern_test_stops_at_the_first_error() {
        let mut mem = MockMem::healthy(4096).with_stuck_bit(0x0000_0001);
        let report = pattern_test(&mut mem, 0xFFFF_FFFF);
        assert!(!report.passed());
        assert_eq!(
            report.first_error,
            Some(MemError {
                offset: 0,
                expected: 0xFFFF_FFFF,
                actual: 0xFFFF_FFFE,
            })
        );
        assert_eq!(report.errors, 1);
        assert_eq!(report.bytes_tested, 4, "首错即停，不应继续扫完 4096 字节");
    }

    #[test]
    fn random_test_is_reproducible_for_a_seed_and_sensitive_to_it() {
        // 用一个必然出错的替身，才观察得到"首次出错的现场"。
        let mut first = MockMem::healthy(4096).with_stuck_bit(0x0000_0100);
        let mut second = MockMem::healthy(4096).with_stuck_bit(0x0000_0100);
        let a = random_test(&mut first, 0xA5A5_1234_5678_9ABC);
        let b = random_test(&mut second, 0xA5A5_1234_5678_9ABC);

        assert!(!a.passed(), "第 8 位恒低必须被抓到");
        assert!(a.first_error.is_some());
        assert_eq!(a.first_error, b.first_error, "同一 seed 必须复现同一现场");
        assert_eq!(a.bytes_tested, b.bytes_tested, "同一 seed 必须扫到同一位置");

        // 不同 seed → 不同数据序列 → 不同的首次出错现场。
        let mut third = MockMem::healthy(4096).with_stuck_bit(0x0000_0100);
        let c = random_test(&mut third, 0x0F0F_8765_4321_0001);
        assert!(!c.passed());
        assert_ne!(a.first_error, c.first_error, "不同 seed 应产生不同数据");
    }

    #[test]
    fn zero_capacity_is_safe_and_passes() {
        let mut mem = MockMem::healthy(0);
        assert_eq!(mem.len_bytes(), 0);

        let address = address_bus_test(&mut mem);
        assert!(address.passed());
        assert_eq!(address.bytes_tested, 0);
        assert_eq!(address.errors, 0);

        let data = data_bus_test(&mut mem);
        assert!(data.passed());
        assert_eq!(data.bytes_tested, 0);

        let pattern = pattern_test(&mut mem, 0xFFFF_FFFF);
        assert!(pattern.passed());
        assert_eq!(pattern.bytes_tested, 0);
        assert!(pattern.first_error.is_none());

        let random = random_test(&mut mem, 1);
        assert!(random.passed());
        assert_eq!(random.bytes_tested, 0);
        assert!(random.first_error.is_none());
    }

    #[test]
    fn a_single_word_capacity_is_safe() {
        let mut mem = MockMem::healthy(4);

        assert!(address_bus_test(&mut mem).passed());
        assert!(data_bus_test(&mut mem).passed());

        let pattern = pattern_test(&mut mem, 0xAAAA_AAAA);
        assert!(pattern.passed());
        assert_eq!(pattern.bytes_tested, 4);
        assert_eq!(mem.words.first().copied(), Some(0xAAAA_AAAA));

        let random = random_test(&mut mem, 7);
        assert!(random.passed());
        assert_eq!(random.bytes_tested, 8, "一个字两遍校验 = 4 + 4 字节");
    }

    #[test]
    fn a_non_multiple_of_four_capacity_ignores_the_unreachable_tail() {
        // 对外报 4098 字节：能访问的是 1024 个字（4096 字节），末尾 2 字节够不成一个字。
        let mut mem = RaggedLen {
            inner: MockMem::healthy(4100),
            len: 4098,
        };

        let address = address_bus_test(&mut mem);
        assert!(address.passed());

        let pattern = pattern_test(&mut mem, 0x5555_5555);
        assert!(pattern.passed());
        assert_eq!(pattern.bytes_tested, 4096, "不足一个字的尾巴不参与测试");

        let random = random_test(&mut mem, 0xDEAD_BEEF);
        assert!(random.passed());
        assert_eq!(random.bytes_tested, 8192);
    }

    #[test]
    fn passed_follows_the_error_count() {
        let clean = MemTestReport {
            kind: TestKind::Pattern,
            bytes_tested: 0,
            errors: 0,
            first_error: None,
        };
        assert!(clean.passed());

        let single = MemTestReport {
            kind: TestKind::DataBus,
            bytes_tested: 4,
            errors: 1,
            first_error: Some(MemError {
                offset: 0,
                expected: 1,
                actual: 0,
            }),
        };
        assert!(!single.passed());

        // 饱和到 u32::MAX 的报告仍然是"不通过"，而不是回绕成 0。
        let saturated = MemTestReport {
            kind: TestKind::Random,
            bytes_tested: u32::MAX,
            errors: u32::MAX,
            first_error: Some(MemError {
                offset: u32::MAX - 3,
                expected: 0xAAAA_AAAA,
                actual: 0x5555_5555,
            }),
        };
        assert!(!saturated.passed());
    }
}
