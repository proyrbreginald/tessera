//! 行编辑器：光标、历史、转义序列解析。
//!
//! # 为什么编辑器处理的是**字节**而不是字符
//!
//! 串口是字节流。一个 UTF-8 字符可能被分两次送达（中断里每次只收到一个字节），
//! 如果编辑器按"字符"处理，就必须在内部维护半个字符的状态——而中文输入法、
//! 粘贴的路径名都可能出现在这里。本终端的输入是**控制序列 + ASCII 命令**，
//! 因此按字节处理、只保证"存进去的还是合法的"：光标落在多字节字符中间时，
//! [`LineEditor::line`] 会把该位置当作不可视（返回行首到光标之前的合法前缀），
//! 而不是 panic。
//!
//! # 为什么用状态机而不是 `match` 前缀匹配
//!
//! 转义序列是**有状态**的：`\x1b` `[` `3` `~` 四个字节才构成 Delete，而
//! `\x1b[9;9;9X` 是未知序列必须整体丢弃。如果按"见到 `\x1b` 就往后偷看几个
//! 字节"来实现，那么收到半个序列（串口丢字节、终端分片）时，后面的普通字符
//! 会被当成序列参数吃掉——表现为"敲什么都没反应"。状态机的每条边都能在主机上
//! 逐字节喂出来测，这正是本 crate 存在的理由。
//!
//! # 容量与截断
//!
//! 行缓冲与历史都是常量泛型定长数组，**没有堆分配**。行满之后多余的可打印
//! 输入被**忽略**（不是覆盖尾字节）：现场粘贴一条长命令时，宁可少几个字符，
//! 也不能悄悄把已经敲对的内容改掉。

use crate::ansi;

/// 一个输入事件的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorEvent {
    /// 还没成行，继续喂字节。
    Pending,
    /// 用户按了回车：行已就绪，可执行。
    Submitted,
    /// 用户按了 Ctrl-C：丢弃当前行。
    Cancelled,
    /// 用户按了 Tab：请求补全（参数是当前行的起始片段）。
    Complete,
}

/// 按键解析状态。
///
/// 只有四个状态，因为只需要覆盖 `ESC` / `CSI` / `CSI 数字~` 三层；
/// 未知路径统一退回 [`State::Idle`]，这是"未知序列必须被安全忽略"的实现方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 普通字节。
    Idle,
    /// 刚收到 `ESC`。
    Esc,
    /// 收到 `ESC [`，等待最终字节。
    Csi,
    /// 收到 `ESC [ <数字>`，正在累积参数（用于 `3~` 这类）。
    CsiParam,
}

/// 行编辑器：定长行缓冲 + 定长环形历史。
pub struct LineEditor<const LINE: usize, const HIST: usize> {
    /// 行缓冲（已接受的字节）。
    buf: [u8; LINE],
    /// 有效字节数（`buf` 的前 `len` 个字节有效）。
    len: usize,
    /// 光标位置（字节下标，`0..=len`）。
    cursor: usize,
    /// 历史环形缓冲：`hist[phys(i)]` 是第 `i` 条（0 = 最近一条）。
    hist: [[u8; LINE]; HIST],
    /// 每条历史的长度。
    hist_len: [usize; HIST],
    /// 下一条历史要写入的物理槽位。
    hist_next: usize,
    /// 已记录的历史条数（`<= HIST`）。
    hist_count: usize,
    /// 上下翻历史的当前位置：0 = 正在编辑的新行，1 = 最近一条历史。
    hist_pos: usize,
    /// 渲染好的提示符字节：`Ctrl-L` 与补全后重绘要用它把行"原地重画"。
    prompt: [u8; 128],
    /// 提示符的有效字节数。
    prompt_len: usize,
    /// 按键解析状态。
    state: State,
    /// `ESC [ <数字>` 里累积的参数值。
    param: u32,
}

impl<const LINE: usize, const HIST: usize> LineEditor<LINE, HIST> {
    /// 编译期断言：行缓冲与历史都必须有容量。
    ///
    /// 为什么值得为"容量为 0"写一条断言：容量是固件侧写的**部署参数**，写错时
    /// 现场唯一的现象是"敲什么都不动"——这类故障在现场最难查，而在构建时
    /// 只需要一行错误信息。用常量求值而不是 `assert!`/`panic!`：`clippy::panic`
    /// 在本 crate 是 deny 的，而且编译期失败离故障点更近。
    const CAPACITY_POSITIVE: () = assert!(LINE > 0 && HIST > 0);

    /// 新建编辑器：空行、空历史、无提示符。
    pub const fn new() -> Self {
        // 触碰这个常量才会触发上面的断言求值。
        let () = Self::CAPACITY_POSITIVE;
        Self {
            buf: [0; LINE],
            len: 0,
            cursor: 0,
            hist: [[0; LINE]; HIST],
            hist_len: [0; HIST],
            hist_next: 0,
            hist_count: 0,
            hist_pos: 0,
            prompt: [0; 128],
            prompt_len: 0,
            state: State::Idle,
            param: 0,
        }
    }

    /// 当前行内容。
    ///
    /// 返回**整行**而不是"光标之前的前缀"：提交时的行内容不该取决于回车那一刻
    /// 光标停在哪里（用户完全可能先 Ctrl-A 再回车）。缓冲里只会有 ASCII 与从
    /// `&str` 拷来的字节，因此 `from_utf8` 的失败分支在实践中不可达；保留它只是
    /// 为了不用 `unwrap`：`no_std` 下 panic 等于挂死。
    #[must_use]
    pub fn line(&self) -> &str {
        match self.buf.get(..self.len) {
            Some(bytes) => core::str::from_utf8(bytes).unwrap_or(""),
            None => "",
        }
    }

    /// 光标位置（字节），测试用。
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// 清空当前行（提交或取消后调用）。
    pub fn clear_line(&mut self) {
        self.len = 0;
        self.cursor = 0;
        self.hist_pos = 0;
    }

    /// 设置提示符（用于行重绘：`Ctrl-L`、补全、历史回翻）。
    ///
    /// 存下来而不是每次重绘时向 `ShellContext` 重新取：重绘发生在**按键处理
    /// 中途**，此时 `ctx` 已被借用（补全要读命令表），再取一次会自找借用冲突。
    pub fn set_prompt(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.prompt.len());
        for (slot, byte) in self.prompt.iter_mut().zip(bytes.iter()) {
            *slot = *byte;
        }
        self.prompt_len = take;
    }

    /// 把当前行压入历史（提交成功后调用）。
    ///
    /// 空行与"与最近一条完全相同"的行不入库：重复执行同一条命令时，
    /// 让上下键翻到的每一条都是**不同的**命令——这是现场排障时最省事的性质。
    pub fn push_history(&mut self) {
        // 先把行长拷到栈上：随后要以可变借用访问历史槽位，
        // 不能再持有 `self.buf` 的不可变借用（借用检查器不允许）。
        let take = self.len.min(LINE);
        if take == 0 {
            // 空行不入历史：现场连续回车很常见，把空行记进去只会让上下键变难用。
            return;
        }
        let mut staged = [0u8; LINE];
        for (slot, byte) in staged.iter_mut().zip(self.buf.iter().take(take)) {
            *slot = *byte;
        }
        // 与最近一条比较：相同则不重复入栈。
        if let Some(newest) = self.entry(0)
            && newest.len() == take
        {
            let mut same = true;
            for (left, right) in newest.iter().zip(staged.iter()) {
                if left != right {
                    same = false;
                    break;
                }
            }
            if same {
                return;
            }
        }
        // 写进 `hist_next` 指向的槽位，然后把它推进一步。
        //
        // 这里**不能**用 `phys(0)` 取槽位：`phys(0)` 是"最近一条的位置"，
        // 而"最近一条"正是靠 `hist_next` 推出来的——自己引用自己会让
        // 连续两次压栈落在同一个槽位，历史看起来只有一条。
        let slot = self.hist_next;
        if let (Some(target), Some(target_len), Some(source)) = (
            self.hist.get_mut(slot),
            self.hist_len.get_mut(slot),
            staged.get(..take),
        ) {
            for (dst, src) in target.iter_mut().zip(source.iter()) {
                *dst = *src;
            }
            *target_len = take;
        }
        self.hist_next = slot.saturating_add(1) % HIST;
        self.hist_count = self.hist_count.saturating_add(1).min(HIST);
        self.hist_pos = 0;
    }

    /// 历史条数。
    #[must_use]
    pub const fn history_len(&self) -> usize {
        self.hist_count
    }

    /// 按序号取历史（0 = 最近一条）。
    #[must_use]
    pub fn history_get(&self, index: usize) -> Option<&str> {
        let bytes = self.entry(index)?;
        Some(core::str::from_utf8(bytes).unwrap_or(""))
    }

    /// 用历史第 `index` 条替换当前行，并重绘。
    ///
    /// 光标落在行尾：翻历史之后最常见的动作是"回车重跑"或"接着改尾部"。
    pub fn recall(&mut self, index: usize, out: &mut dyn core::fmt::Write) {
        // 翻历史会改变 `hist_pos`：调用方是 shell（例如补全、重绘），
        // 让它与上下键共用同一套语义。
        if index < self.hist_count {
            self.hist_pos = index.saturating_add(1);
        }
        self.recall_internal(index);
        self.redraw(out);
    }

    /// 用补全结果替换当前行（Tab）。
    ///
    /// 只做"整行替换"这一件事：候选列表的打印属于交互展示，放在 `shell` 里，
    /// 这样编辑器不必知道命令表长什么样。
    pub fn apply_completion(&mut self, full: &str, out: &mut dyn core::fmt::Write) {
        let bytes = full.as_bytes();
        let take = bytes.len().min(LINE);
        // 先清空再按长度拷入：如果只复制 `take` 个字节，补全结果比原行短时
        // 会把原行的尾部残余留在缓冲里，`line()` 就会读出"补全 + 残影"。
        for slot in self.buf.iter_mut() {
            *slot = 0;
        }
        let mut index = 0;
        while index < take {
            if let (Some(source), Some(target)) =
                (bytes.get(index).copied(), self.buf.get_mut(index))
            {
                *target = source;
            }
            index = index.saturating_add(1);
        }
        self.len = take;
        self.cursor = take;
        self.redraw(out);
    }

    /// 喂一个字节；必要时把**重绘所需的字节**写进 `out`。
    pub fn feed(&mut self, byte: u8, out: &mut dyn core::fmt::Write) -> EditorEvent {
        // 转义序列优先：处在序列中间时，字节是参数而不是按键。
        if self.state != State::Idle {
            // 序列中间出现**控制字符**：说明这个序列已经残缺（串口丢字节是常态）。
            //
            // 两种做法里选后者：把控制字节当成序列的一部分丢掉，或者判定序列中断、
            // 并把该字节**重新当作普通输入**处理。后者才对——现场最痛的场景是
            // "丢了一个字节，于是回车被吃掉，敲什么都不提交"，而控制字节几乎不可能
            // 是合法转义序列的组成部分。
            if byte < 0x20 {
                self.state = State::Idle;
                self.param = 0;
            } else {
                return self.feed_escape(byte, out);
            }
        }
        match byte {
            0x0D | 0x0A => EditorEvent::Submitted,
            0x03 => EditorEvent::Cancelled,
            0x09 => EditorEvent::Complete,
            0x7F | 0x08 => {
                self.backspace(out);
                EditorEvent::Pending
            }
            0x01 => {
                self.cursor = 0;
                EditorEvent::Pending
            }
            0x05 => {
                self.cursor = self.len;
                EditorEvent::Pending
            }
            0x0B => {
                // Ctrl-K：删光标到行尾。光标不动，因此只需擦掉尾部再补光标。
                self.len = self.cursor;
                self.redraw_tail(out);
                EditorEvent::Pending
            }
            0x15 => {
                // Ctrl-U：删行首到光标。删掉的内容在光标**左侧**，终端没有
                // "删除光标之前"的序列，因此只能把光标右侧的内容整体左移、
                // 再整行重绘。
                let tail = self.len.saturating_sub(self.cursor);
                let mut offset = 0;
                while offset < tail {
                    if let (Some(source), Some(target)) = (
                        self.buf.get(self.cursor.saturating_add(offset)).copied(),
                        self.buf.get_mut(offset),
                    ) {
                        *target = source;
                    }
                    offset = offset.saturating_add(1);
                }
                self.len = tail;
                self.cursor = 0;
                self.redraw(out);
                EditorEvent::Pending
            }
            0x0C => {
                // Ctrl-L：清屏重绘。现场刷屏之后不用重连终端。
                let _ = out.write_str(ansi::CLEAR_SCREEN);
                self.redraw(out);
                EditorEvent::Pending
            }
            0x1B => {
                self.state = State::Esc;
                EditorEvent::Pending
            }
            // 可打印 ASCII 之外的控制字节一律忽略：把未知控制字节当字符插入
            // 会让行里出现不可见字节，命令解析随后莫名失败。
            0x20..=0x7E => {
                self.insert(byte, out);
                EditorEvent::Pending
            }
            _ => EditorEvent::Pending,
        }
    }

    // ------------------------------------------------------------ 内部：编辑

    /// 在光标处插入一个字节（行满则忽略）。
    fn insert(&mut self, byte: u8, out: &mut dyn core::fmt::Write) {
        if self.len >= LINE {
            return;
        }
        // 从尾部往右挪一格，给新字节腾位置。必须**从后往前**搬，
        // 否则会把前一个字节复制两遍。
        let mut index = self.len;
        while index > self.cursor {
            if let (Some(source), Some(target)) = (
                self.buf.get(index.saturating_sub(1)).copied(),
                self.buf.get_mut(index),
            ) {
                *target = source;
            }
            index = index.saturating_sub(1);
        }
        if let Some(slot) = self.buf.get_mut(self.cursor) {
            *slot = byte;
        }
        self.cursor = self.cursor.saturating_add(1).min(LINE);
        self.len = self.len.saturating_add(1).min(LINE);
        self.redraw_tail(out);
    }

    /// 退格：删光标左侧一个字节（已到行首则忽略）。
    fn backspace(&mut self, out: &mut dyn core::fmt::Write) {
        if self.cursor == 0 {
            return;
        }
        let from = self.cursor.saturating_sub(1);
        self.remove_at(from);
        self.cursor = from;
        // 整行从新光标处重画：终端上"退格"看似只删一个字符，但在**中间**
        // 退格时右侧内容要整体左移，只发一个 `\x08` 会留下残影。
        self.redraw_tail(out);
    }

    /// 删掉光标处的一个字节（Delete 键）。
    fn delete_forward(&mut self, out: &mut dyn core::fmt::Write) {
        if self.cursor >= self.len {
            return;
        }
        self.remove_at(self.cursor);
        self.redraw_tail(out);
    }

    /// 把 `at` 之后的字节整体左移一格。
    ///
    /// 光标只在"被删的字节位于光标之前"时才跟着左移：删光标右侧的字符（Delete）
    /// 时如果无条件把光标减一，光标就会跑到被删字符的左边——现场表现是
    /// "按一次 Delete 之后光标莫名退了一格"。
    fn remove_at(&mut self, at: usize) {
        if at >= self.len {
            return;
        }
        let mut index = at;
        while index.saturating_add(1) < self.len {
            if let (Some(source), Some(target)) = (
                self.buf.get(index.saturating_add(1)).copied(),
                self.buf.get_mut(index),
            ) {
                *target = source;
            }
            index = index.saturating_add(1);
        }
        if at < self.cursor {
            self.cursor = self.cursor.saturating_sub(1);
        }
        self.len = self.len.saturating_sub(1);
        self.cursor = self.cursor.min(self.len);
    }

    // ------------------------------------------------------------ 内部：绘制

    /// 重绘提示符与整行内容（光标回到原位置）。
    ///
    /// 供 `shell` 在"打印了候选列表/警告"之后调用：那些输出占了整行，
    /// 必须把提示符与用户已经敲进去的内容重新画出来，否则用户看到的是
    /// 半截命令行——现场会以为自己的输入丢了。
    pub fn redraw_line(&mut self, out: &mut dyn core::fmt::Write) {
        self.redraw(out);
    }

    /// 从光标处重画到行尾，并把光标移回原位置。
    ///
    /// 这是最常用的重绘路径：插入/退格/删除都只需要动光标右侧的内容，
    /// 不需要清掉提示符——少写字节在 115200 的串口上就是更少的等待。
    fn redraw_tail(&mut self, out: &mut dyn core::fmt::Write) {
        let _ = out.write_str(ansi::CR_CLEAR_EOL);
        self.write_prompt(out);
        self.write_range(out, 0, self.len);
        // 光标之后的内容已经写出，用左移键把光标送回原处。
        // `\x1b[D` 逐格左移，不需要终端支持绝对定位（老终端也认）。
        let back = self.len.saturating_sub(self.cursor);
        for _ in 0..back {
            let _ = out.write_str("\x1b[D");
        }
    }

    /// 清掉整行并重画（提示符 + 整行内容），光标留回原位置。
    fn redraw(&mut self, out: &mut dyn core::fmt::Write) {
        let _ = out.write_str(ansi::CR_CLEAR_EOL);
        self.write_prompt(out);
        self.write_range(out, 0, self.len);
        let back = self.len.saturating_sub(self.cursor);
        for _ in 0..back {
            let _ = out.write_str("\x1b[D");
        }
    }

    /// 输出提示符。
    fn write_prompt(&self, out: &mut dyn core::fmt::Write) {
        if let Some(bytes) = self.prompt.get(..self.prompt_len)
            && let Ok(text) = core::str::from_utf8(bytes)
        {
            let _ = out.write_str(text);
        }
    }

    /// 输出 `buf[from..to)`。
    fn write_range(&self, out: &mut dyn core::fmt::Write, from: usize, to: usize) {
        if let Some(bytes) = self.buf.get(from..to)
            && let Ok(text) = core::str::from_utf8(bytes)
        {
            let _ = out.write_str(text);
        }
    }

    // ------------------------------------------------------------ 内部：转义

    /// 处理处于转义序列中的字节。
    ///
    /// `out` 只在"结束一个会**改变行内容**的序列"时用到（Delete / 翻历史）：
    /// 光标单纯移动不发字节，因为光标本来就是终端自己维护的。
    fn feed_escape(&mut self, byte: u8, out: &mut dyn core::fmt::Write) -> EditorEvent {
        match self.state {
            State::Esc => {
                if byte == b'[' {
                    self.state = State::Csi;
                } else if byte == b'O' {
                    // `ESC O A` 是另一种终端发方向键的方式（应用模式）。
                    self.state = State::Csi;
                } else {
                    // 未知序列：整体丢弃，回到普通输入。
                    self.state = State::Idle;
                }
                EditorEvent::Pending
            }
            State::Csi => match byte {
                b'A' => {
                    self.state = State::Idle;
                    self.history_up(out);
                    EditorEvent::Pending
                }
                b'B' => {
                    self.state = State::Idle;
                    self.history_down(out);
                    EditorEvent::Pending
                }
                b'C' => {
                    self.state = State::Idle;
                    self.cursor = self.cursor.saturating_add(1).min(self.len);
                    EditorEvent::Pending
                }
                b'D' => {
                    self.state = State::Idle;
                    self.cursor = self.cursor.saturating_sub(1);
                    EditorEvent::Pending
                }
                b'H' => {
                    self.state = State::Idle;
                    self.cursor = 0;
                    EditorEvent::Pending
                }
                b'F' => {
                    self.state = State::Idle;
                    self.cursor = self.len;
                    EditorEvent::Pending
                }
                // 参数字节：数字累积（`3~` 的 `3`），分隔符只改状态。
                //
                // 分隔符逐个列出来而不是写 `0x30..=0x3F`：那个范围与上面的数字
                // 分支重叠，clippy 会（正确地）报出来——而列出 `:;<=>?` 也顺便
                // 说明了"ANSI 的参数字节就这几个"。
                b'0'..=b'9' => {
                    self.accumulate_param(byte);
                    self.state = State::CsiParam;
                    EditorEvent::Pending
                }
                b':' | b';' | b'<' | b'=' | b'>' | b'?' => {
                    self.state = State::CsiParam;
                    EditorEvent::Pending
                }
                // 最终字节（ASCII 0x40..=0x7E）：`K`、`J`、`?25h` 的 `h` 都落在这里。
                // 本终端不需要它们，但**必须**回到 Idle——否则后续按键会被当成
                // 序列的一部分吞掉，现场表现为"敲什么都没反应"。
                0x40..=0x7E => {
                    self.state = State::Idle;
                    EditorEvent::Pending
                }
                // 其余（中间字节、控制字符、非 ASCII）：序列已经畸形，整体丢弃。
                _ => {
                    self.state = State::Idle;
                    EditorEvent::Pending
                }
            },
            State::CsiParam => match byte {
                b'0'..=b'9' => {
                    self.accumulate_param(byte);
                    EditorEvent::Pending
                }
                b'~' => {
                    self.state = State::Idle;
                    let param = self.param;
                    self.param = 0;
                    match param {
                        3 => self.delete_forward(out),
                        1 | 7 => self.cursor = 0,
                        4 | 8 => self.cursor = self.len,
                        // 未知的 `CSI n ~`（例如某些终端的 `5~`）忽略。
                        _ => {}
                    }
                    EditorEvent::Pending
                }
                // 又出现参数字节（`?25h` 的 `?`）：留在参数状态继续等最终字节。
                b':' | b';' | b'<' | b'=' | b'>' | b'?' => EditorEvent::Pending,
                // 最终字节：序列结束，累积的参数没有对应动作，丢弃。
                0x40..=0x7E => {
                    self.state = State::Idle;
                    self.param = 0;
                    EditorEvent::Pending
                }
                // 畸形序列：整体丢弃，并清掉累积值，避免污染下一个序列。
                _ => {
                    self.state = State::Idle;
                    self.param = 0;
                    EditorEvent::Pending
                }
            },
            State::Idle => EditorEvent::Pending,
        }
    }

    /// 累积一个数字参数。
    ///
    /// 用饱和运算：恶意/噪声字节可以发 `ESC [ 99999999...`，而"参数溢出"在
    /// 本终端只意味着"这不是我们要的序列"，绝不该让 `no_std` 固件发生回绕。
    fn accumulate_param(&mut self, byte: u8) {
        let digit = u32::from(byte.saturating_sub(b'0'));
        self.param = self.param.saturating_mul(10).saturating_add(digit);
    }

    /// 历史环形缓冲的物理下标：`index` 0 = 最近一条。
    const fn phys(&self, index: usize) -> usize {
        // `hist_next` 指向"下一条要写的槽位"，因此最近一条在它前一个槽位。
        let base = if self.hist_next == 0 {
            HIST.saturating_sub(1)
        } else {
            self.hist_next.saturating_sub(1)
        };
        if index == 0 {
            return base;
        }
        let back = index % HIST;
        if back <= base {
            base.saturating_sub(back)
        } else {
            base.saturating_add(HIST).saturating_sub(back)
        }
    }

    /// 取第 `index` 条历史的字节。
    fn entry(&self, index: usize) -> Option<&[u8]> {
        if index >= self.hist_count {
            return None;
        }
        let slot = self.phys(index);
        let len = self.hist_len.get(slot).copied()?;
        self.hist.get(slot)?.get(..len)
    }

    /// 上翻历史。`hist_pos` 0 表示正在编辑新行。
    fn history_up(&mut self, out: &mut dyn core::fmt::Write) {
        if self.hist_count == 0 {
            return;
        }
        let next = self.hist_pos.saturating_add(1).min(self.hist_count);
        self.hist_pos = next;
        // 到顶之后重绘是多余的，但保留它只有一次 `\r\x1b[K` 的代价；
        // 换来的性质是"上翻一定把行缓冲的变化呈现在屏幕上"，不会出现
        // 内部状态已变而终端显示未变的分裂。
        self.recall_internal(next.saturating_sub(1));
        self.redraw(out);
    }

    /// 下翻历史：`hist_pos` 归零表示回到"正在编辑的新行"（当前行被清空）。
    fn history_down(&mut self, out: &mut dyn core::fmt::Write) {
        if self.hist_pos == 0 {
            return;
        }
        self.hist_pos = self.hist_pos.saturating_sub(1);
        if self.hist_pos == 0 {
            // 回到空行：与 bash 一致，避免"上翻再下翻"之后留下上一条的残影
            // 被误当成用户输入提交。
            self.len = 0;
            self.cursor = 0;
        } else {
            self.recall_internal(self.hist_pos.saturating_sub(1));
        }
        self.redraw(out);
    }

    /// 把历史第 `index` 条装进行缓冲（无输出）。
    fn recall_internal(&mut self, index: usize) {
        let take = match self.entry(index) {
            Some(bytes) => {
                let source_len = bytes.len().min(LINE);
                let mut staged = [0u8; LINE];
                for (slot, byte) in staged.iter_mut().zip(bytes.iter()) {
                    *slot = *byte;
                }
                for (dst, src) in self.buf.iter_mut().zip(staged.iter()) {
                    *dst = *src;
                }
                source_len
            }
            None => return,
        };
        self.len = take;
        self.cursor = take;
    }
}

impl<const LINE: usize, const HIST: usize> Default for LineEditor<LINE, HIST> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const LINE: usize, const HIST: usize> core::fmt::Debug for LineEditor<LINE, HIST> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 手写而不是 derive：`[[u8; LINE]; HIST]` 的 Debug 会把整个缓冲区
        // （包含陈旧字节）倒出来，在 `no_std` 的日志里毫无价值。
        f.debug_struct("LineEditor")
            .field("line", &self.line())
            .field("cursor", &self.cursor)
            .field("history", &self.hist_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorEvent, LineEditor};

    /// 测试替身：把编辑器与输出缓冲绑在一起，省去每个用例重复声明。
    struct Harness<const LINE: usize, const HIST: usize> {
        editor: LineEditor<LINE, HIST>,
        out: String,
    }

    impl<const LINE: usize, const HIST: usize> Harness<LINE, HIST> {
        fn new() -> Self {
            Self {
                editor: LineEditor::new(),
                out: String::new(),
            }
        }

        /// 逐字节喂入，返回最后一个事件（中途的事件在 `events` 里验）。
        fn feed(&mut self, bytes: &[u8]) -> EditorEvent {
            let mut last = EditorEvent::Pending;
            for byte in bytes {
                last = self.editor.feed(*byte, &mut self.out);
            }
            last
        }

        /// 逐字节喂入并记录每个字节的事件序列。
        fn feed_events(&mut self, bytes: &[u8]) -> Vec<EditorEvent> {
            let mut events = Vec::new();
            for byte in bytes {
                events.push(self.editor.feed(*byte, &mut self.out));
            }
            events
        }

        fn submit(&mut self, line: &str) {
            self.out.clear();
            let mut bytes = line.as_bytes().to_vec();
            bytes.push(b'\r');
            let event = self.feed(&bytes);
            assert_eq!(event, EditorEvent::Submitted, "应当提交：{line:?}");
            self.editor.push_history();
            self.editor.clear_line();
        }
    }

    #[test]
    fn 逐字节输入后行内容与事件序列正确() {
        let mut harness = Harness::<32, 4>::new();
        let events = harness.feed_events(b"ls -l\r");
        // 前五个字节都是 Pending，只有回车的字节触发 Submitted。
        assert_eq!(
            events,
            vec![
                EditorEvent::Pending,
                EditorEvent::Pending,
                EditorEvent::Pending,
                EditorEvent::Pending,
                EditorEvent::Pending,
                EditorEvent::Submitted,
            ],
            "{events:?}"
        );
        assert_eq!(harness.editor.line(), "ls -l");
        // 回显必须包含敲进去的字符：否则现场看不到自己敲的命令。
        assert!(harness.out.contains("ls -l"), "{:?}", harness.out);
    }

    #[test]
    fn 换行符也提交() {
        // 有些终端只发 `\n`（LF）。
        let mut harness = Harness::<32, 4>::new();
        assert_eq!(harness.feed(b"x\n"), EditorEvent::Submitted);
        assert_eq!(harness.editor.line(), "x");
    }

    #[test]
    fn 光标处插入而不是只能追加() {
        let mut harness = Harness::<32, 4>::new();
        harness.feed(b"abc");
        // 左移两格：光标落在 `a` 之后（下标 1）。
        harness.feed(b"\x1b[D\x1b[D");
        assert_eq!(harness.editor.cursor(), 1);
        harness.feed(b"X");
        // 关键性质：插入而不是覆盖，也不是追加。
        assert_eq!(harness.editor.line(), "aXbc");
        assert_eq!(harness.editor.cursor(), 2);
    }

    #[test]
    fn 退格到行首不panic且行不变() {
        let mut harness = Harness::<32, 4>::new();
        harness.feed(b"ab");
        // 退到空之后继续退：必须安全忽略（这里是"不 panic、不清空已有内容"）。
        harness.feed(b"\x7f\x7f\x7f\x7f\x7f");
        assert_eq!(harness.editor.line(), "");
        assert_eq!(harness.editor.cursor(), 0);

        harness.feed(b"ab");
        harness.feed(b"\x7f\x7f\x7f");
        assert_eq!(harness.editor.line(), "");
        // 到行首之后再退格，行仍然是空而不是出现脏字节。
        harness.feed(b"\x08\x08");
        assert_eq!(harness.editor.line(), "");
    }

    #[test]
    fn 行满后输入被忽略() {
        let mut harness = Harness::<4, 2>::new();
        harness.feed(b"abcd");
        assert_eq!(harness.editor.line(), "abcd");
        // 行满：多余的输入被忽略，**不是**覆盖尾部。
        harness.feed(b"efg");
        assert_eq!(harness.editor.line(), "abcd");
        assert_eq!(harness.editor.cursor(), 4);
    }

    #[test]
    fn 行满后在中间插入也被忽略() {
        let mut harness = Harness::<4, 2>::new();
        harness.feed(b"abcd");
        harness.feed(b"\x1b[D\x1b[D");
        harness.feed(b"X");
        assert_eq!(harness.editor.line(), "abcd");
    }

    #[test]
    fn 历史上下翻顺序正确且下翻回到空行() {
        let mut harness = Harness::<16, 8>::new();
        harness.submit("first");
        harness.submit("second");
        harness.submit("third");
        assert_eq!(harness.editor.history_len(), 3);
        assert_eq!(harness.editor.history_get(0), Some("third"));
        assert_eq!(harness.editor.history_get(1), Some("second"));
        assert_eq!(harness.editor.history_get(2), Some("first"));
        assert_eq!(harness.editor.history_get(3), None);

        // 上翻：最近一条 → 更早。
        harness.feed(b"\x1b[A");
        assert_eq!(harness.editor.line(), "third");
        harness.feed(b"\x1b[A");
        assert_eq!(harness.editor.line(), "second");
        harness.feed(b"\x1b[A");
        assert_eq!(harness.editor.line(), "first");
        // 到最老一条继续上翻：停在原处（不清空、不回绕到最新）。
        harness.feed(b"\x1b[A");
        assert_eq!(harness.editor.line(), "first");

        // 下翻：往回走，最后回到"新行"（空）。
        harness.feed(b"\x1b[B");
        assert_eq!(harness.editor.line(), "second");
        harness.feed(b"\x1b[B");
        assert_eq!(harness.editor.line(), "third");
        harness.feed(b"\x1b[B");
        assert_eq!(harness.editor.line(), "");
    }

    #[test]
    fn 历史环形缓冲写满后仍按序取值() {
        // 容量 2：写三条之后，最老一条应当被挤掉，序号语义不变。
        let mut harness = Harness::<8, 2>::new();
        harness.submit("one");
        harness.submit("two");
        harness.submit("three");
        assert_eq!(harness.editor.history_len(), 2);
        assert_eq!(harness.editor.history_get(0), Some("three"));
        assert_eq!(harness.editor.history_get(1), Some("two"));
        assert_eq!(harness.editor.history_get(2), None);
        harness.feed(b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(harness.editor.line(), "two");
    }

    #[test]
    fn 重复命令与空行不入历史() {
        let mut harness = Harness::<8, 8>::new();
        harness.submit("");
        harness.submit("ls");
        harness.submit("ls");
        harness.submit("pwd");
        assert_eq!(harness.editor.history_len(), 2);
        assert_eq!(harness.editor.history_get(0), Some("pwd"));
        assert_eq!(harness.editor.history_get(1), Some("ls"));
    }

    #[test]
    fn ctrl_c_取消后有独立事件且行可被清空() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"rm -rf");
        assert_eq!(harness.feed(b"\x03"), EditorEvent::Cancelled);
        // 编辑器只负责发事件，清行由调用方决定——这样"取消"的语义
        // 不会被悄悄写死成"顺便清历史"。
        harness.editor.clear_line();
        assert_eq!(harness.editor.line(), "");
        assert_eq!(harness.editor.cursor(), 0);
        // 取消**不**入历史：误敲的命令不该在下一次上翻时被重放。
        assert_eq!(harness.editor.history_len(), 0);
    }

    #[test]
    fn ctrl_a_ctrl_e_移动光标() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"hello");
        harness.feed(b"\x01");
        assert_eq!(harness.editor.cursor(), 0);
        // 行首插入。
        harness.feed(b"X");
        assert_eq!(harness.editor.line(), "Xhello");
        harness.feed(b"\x05");
        assert_eq!(harness.editor.cursor(), 6);
        harness.feed(b"!");
        assert_eq!(harness.editor.line(), "Xhello!");
    }

    #[test]
    fn ctrl_k_删到行尾() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"abcdef");
        harness.feed(b"\x1b[D\x1b[D\x1b[D");
        assert_eq!(harness.editor.cursor(), 3);
        harness.feed(b"\x0b");
        assert_eq!(harness.editor.line(), "abc");
        assert_eq!(harness.editor.cursor(), 3);
    }

    #[test]
    fn ctrl_u_删到行首且保留光标右侧() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"abcdef");
        harness.feed(b"\x1b[D\x1b[D\x1b[D");
        assert_eq!(harness.editor.cursor(), 3);
        harness.feed(b"\x15");
        assert_eq!(harness.editor.line(), "def");
        assert_eq!(harness.editor.cursor(), 0);
    }

    #[test]
    fn ctrl_l_清屏并重绘当前行() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"abc");
        harness.out.clear();
        harness.editor.set_prompt(b"[root@tessera ~]# ");
        harness.feed(b"\x0c");
        // 清屏 + 重绘：现场刷屏之后不用重连终端。
        assert!(harness.out.contains("\u{1b}[2J"), "{:?}", harness.out);
        assert!(harness.out.contains("abc"), "{:?}", harness.out);
        assert!(
            harness.out.contains("[root@tessera ~]# "),
            "{:?}",
            harness.out
        );
    }

    #[test]
    fn 转义序列各自行为正确() {
        // Home / End。
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"abc");
        harness.feed(b"\x1b[H");
        assert_eq!(harness.editor.cursor(), 0);
        harness.feed(b"\x1b[F");
        assert_eq!(harness.editor.cursor(), 3);
        // 光标在行尾按 Delete：无字符可删，安全忽略。
        harness.feed(b"\x1b[3~");
        assert_eq!(harness.editor.line(), "abc");

        // Delete：删光标处一个字节。光标**不**左移——它删的是自己右边的内容，
        // 左移会让人以为删掉的是前一个字符。
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"abc");
        harness.feed(b"\x1b[D"); // 光标落在 `c` 上（下标 2）
        assert_eq!(harness.editor.cursor(), 2);
        harness.feed(b"\x1b[3~");
        assert_eq!(harness.editor.line(), "ab");
        assert_eq!(harness.editor.cursor(), 2);

        // 方向键移动不越界。
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"ab");
        harness.feed(b"\x1b[C\x1b[C\x1b[C\x1b[C");
        assert_eq!(harness.editor.cursor(), 2);
        harness.feed(b"\x1b[D\x1b[D\x1b[D\x1b[D");
        assert_eq!(harness.editor.cursor(), 0);
    }

    #[test]
    fn 未知转义序列被忽略且后续输入正常() {
        let mut harness = Harness::<32, 4>::new();
        harness.feed(b"ab");
        // 未知 / 畸形序列：`CSI 9;9;9 X` 与 `CSI ?25h`（终端查询光标可见性）
        // 都不该影响行内容，也不该让编辑器卡在序列状态里。
        harness.feed(b"\x1b[9;9;9X");
        harness.feed(b"\x1b[?25h");
        // 序列之后普通输入仍然生效——这是"状态机必须回到 Idle"的验收点。
        harness.feed(b"cd");
        assert_eq!(harness.editor.line(), "abcd");
        // 序列里的数字与字母（`9`、`;`、`X`、`?`、`h`）没有一个是行内容。
        assert_eq!(harness.editor.line(), "abcd");
    }

    #[test]
    fn 半个转义序列之后仍能正常输入() {
        // 模拟串口丢字节：只收到 `ESC [` 就没有下文了。
        //
        // 随后到来的**控制字节**（回车）必须仍然生效：如果它被当成序列的一部分
        // 吞掉，现场现象就是"丢一个字节之后回车也失灵了"，这类故障最难归因。
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"ab\x1b[");
        assert_eq!(harness.feed(b"\r"), EditorEvent::Submitted);
        assert_eq!(harness.editor.line(), "ab");
        harness.editor.clear_line();
        // 残缺序列之后，普通输入也照常。
        harness.feed(b"\x1b[");
        harness.feed(b"xy");
        assert_eq!(harness.editor.line(), "y");
    }

    #[test]
    fn tab_返回补全事件且不改行() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"he");
        assert_eq!(harness.feed(b"\t"), EditorEvent::Complete);
        // 编辑器只发事件：补全由 shell 查命令表后调用 `apply_completion`。
        assert_eq!(harness.editor.line(), "he");
    }

    #[test]
    fn 补全结果替换整行并重绘() {
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"he");
        harness.out.clear();
        harness.editor.apply_completion("help ", &mut harness.out);
        assert_eq!(harness.editor.line(), "help ");
        assert_eq!(harness.editor.cursor(), 5);
        assert!(harness.out.contains("help "), "{:?}", harness.out);
    }

    #[test]
    fn 补全结果超过行容量时按容量截断() {
        let mut harness = Harness::<4, 4>::new();
        harness.editor.apply_completion("abcdef", &mut harness.out);
        // 截断而不是越界写：泛型容量是部署参数，可能是很小的值。
        assert_eq!(harness.editor.line(), "abcd");
        assert_eq!(harness.editor.cursor(), 4);
    }

    #[test]
    fn 非ascii字节不被当成普通字符插入() {
        // 串口上出现的非 ASCII 字节（噪声、半截 UTF-8）不应被当作按键内容。
        let mut harness = Harness::<16, 4>::new();
        harness.feed(b"ab");
        harness.feed(b"\x80\xff");
        assert_eq!(harness.editor.line(), "ab");
        // 后续 ASCII 输入仍然正常。
        harness.feed(b"c");
        assert_eq!(harness.editor.line(), "abc");
    }

    #[test]
    fn 提交后调用方清行则下次输入从空行开始() {
        let mut harness = Harness::<16, 4>::new();
        assert_eq!(harness.feed(b"pwd\r"), EditorEvent::Submitted);
        harness.editor.push_history();
        harness.editor.clear_line();
        assert_eq!(harness.editor.line(), "");
        harness.feed(b"ls");
        assert_eq!(harness.editor.line(), "ls");
        assert_eq!(harness.editor.history_len(), 1);
    }

    #[test]
    fn 默认实例与调试输出可用() {
        let editor = LineEditor::<8, 2>::default();
        let text = format!("{editor:?}");
        assert!(text.contains("LineEditor"), "{text:?}");
        assert_eq!(editor.line(), "");
        assert_eq!(editor.cursor(), 0);
    }
}
