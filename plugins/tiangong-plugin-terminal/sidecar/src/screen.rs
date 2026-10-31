//! 终端屏幕模拟：headless 模式下的画面快照来源（issue #697）。
//!
//! 桌面端画面由前端 xterm 渲染并回传（`terminalScreenUpdate`）；CLI /
//! Server 没有前端，PTY 输出同时喂给本模块的 [`ScreenGrid`]（vte 状态机
//! 加网格模型），由它生成与前端一致形态的可见文本（viewport、去行尾
//! 空白、裁尾部空行、以 `\n` 连接），供 `terminal_send` 返回 vi / top /
//! less 等全屏程序的当前画面。
//!
//! 覆盖常用控制序列：光标移动与定位、擦除、行插入/删除、滚动区域
//! （DECSTBM）、字符插入/删除，以及 alternate screen（1049/1047/47，
//! vi 与 less 使用）。SGR 颜色、OSC 应答等不影响文本内容的序列忽略。
//! 宽字符（CJK）按单列计——快照中的行内位置可能与真实终端有细微偏移，
//! 不影响读取内容本身。
//!

use vte::{Params, Perform};

/// 主屏保留的回滚行数上限：可见区之外的滚动历史。提示符基线等判定
/// 使用原始输出流另行处理，这里只服务画面快照与回看，无需过长。
const SCROLLBACK_LINES: usize = 200;

/// 终端屏幕网格：vte 状态机驱动，按 xterm 常见行为模拟。
///
/// 状态在 [`GridHandler`]（`Perform` 实现），解析器独立持有——`advance`
/// 需要同时可变借用两者（与 [`crate::persist::TerminalLineProcessor`] 同
/// 模式）。
pub(crate) struct ScreenGrid {
    parser: vte::Parser,
    handler: GridHandler,
}

impl ScreenGrid {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vte::Parser::new(),
            handler: GridHandler::new(cols, rows),
        }
    }

    /// 喂入一段 PTY 输出（过滤内部 marker 之后的可见流），更新网格。
    pub(crate) fn feed(&mut self, text: &str) {
        self.parser.advance(&mut self.handler, text.as_bytes());
    }

    /// 尺寸变化：保留现有内容左上对齐，光标与滚动区域钳制到新范围。
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.handler.resize(cols, rows);
    }

    /// 当前可见画面文本（与前端快照同形态）：活动屏 viewport 各行去行尾
    /// 空白，裁掉尾部空行后以 `\n` 连接。
    pub(crate) fn visible_text(&self) -> String {
        self.handler.visible_text()
    }
}
/// 屏幕状态与控制序列处理。
struct GridHandler {
    cols: usize,
    rows: usize,
    /// 主屏行缓冲：前段为回滚区（上限 [`SCROLLBACK_LINES`]），末段
    /// `rows` 行为可见区。
    main: Vec<Vec<char>>,
    /// alternate screen（1049）：激活时 Some，固定 `rows` 行、无回滚。
    alt: Vec<Vec<char>>,
    in_alt: bool,
    /// 进入 alternate screen 时保存的主屏光标（1049 语义）。
    saved_cursor: (usize, usize),
    /// 光标（行、列）。行索引相对活动屏幕的**可见区**起点（0 起）。
    cursor_row: usize,
    cursor_col: usize,
    /// 滚动区域（可见区内 0 起行号，含端点）；未设置时为全屏。
    scroll_top: usize,
    scroll_bottom: usize,
}

impl GridHandler {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(2) as usize;
        let rows = rows.max(2) as usize;
        Self {
            cols,
            rows,
            main: vec![Vec::new(); rows],
            alt: Vec::new(),
            in_alt: false,
            saved_cursor: (0, 0),
            cursor_row: 0,
            cursor_col: 0,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
        }
    }

    /// 尺寸变化：保留现有内容左上对齐，光标与滚动区域钳制到新范围。
    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2) as usize;
        let rows = rows.max(2) as usize;
        if cols == self.cols && rows == self.rows {
            return;
        }
        let old_rows = self.rows;
        // 光标绝对行须在尺寸更新前按旧口径计算（viewport_start 依赖 rows）。
        let cursor_abs = self.viewport_start() + self.cursor_row;
        self.cols = cols;
        self.rows = rows;
        if rows < old_rows {
            // 行数收缩：以光标行为锚保留最新内容（光标上方最多保留
            // rows-1 行），被挤出的行进入回滚（主屏）或丢弃（alt）。
            let keep_from = cursor_abs.saturating_sub(rows.saturating_sub(1));
            let mut cut = 0usize;
            {
                let screen = self.active_lines_mut();
                let keep_from = keep_from.min(screen.len());
                if keep_from > 0 {
                    screen.drain(..keep_from);
                    cut += keep_from;
                }
                if screen.len() > rows {
                    let extra = screen.len() - rows;
                    screen.drain(rows..);
                    cut += extra;
                }
            }
            self.cursor_row = self.cursor_row.saturating_sub(cut);
        } else if rows > old_rows {
            self.active_lines_mut().resize_with(rows, Vec::new);
        }
        if !self.in_alt && self.main.len() > rows {
            // 主屏回滚超限淘汰。
            let overflow = self.main.len() - rows;
            if overflow > SCROLLBACK_LINES {
                let drop = overflow - SCROLLBACK_LINES;
                self.main.drain(..drop);
            }
        }
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
    }

    /// 当前可见画面文本（与前端快照同形态）：活动屏 viewport 各行去行尾
    /// 空白，裁掉尾部空行后以 `\n` 连接。
    fn visible_text(&self) -> String {
        let lines = self.active_lines();
        let start = lines.len().saturating_sub(self.rows);
        let mut text = String::new();
        let mut printed = 0usize;
        for (index, line) in lines.iter().enumerate().skip(start) {
            let content: String = line.iter().collect();
            let content = content.trim_end();
            if content.is_empty() && index > start + self.cursor_row {
                // 光标行之后的空行不再输出（对齐前端裁尾）。
                continue;
            }
            if printed > 0 {
                text.push('\n');
            }
            text.push_str(content);
            printed += 1;
        }
        // 再裁一次尾部空行（光标行之前可能有整块空行需要保留中间形态）。
        while text.ends_with('\n') {
            text.pop();
        }
        text
    }

    /// 活动屏行引用（主屏=含回滚的完整缓冲；alt=固定 rows 行）。
    fn active_lines(&self) -> &Vec<Vec<char>> {
        if self.in_alt { &self.alt } else { &self.main }
    }

    fn active_lines_mut(&mut self) -> &mut Vec<Vec<char>> {
        if self.in_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    /// 活动屏可见区首行的绝对索引（主屏 = 缓冲长度 - rows）。
    fn viewport_start(&self) -> usize {
        if self.in_alt {
            0
        } else {
            self.main.len().saturating_sub(self.rows)
        }
    }

    /// 光标绝对行索引（活动屏缓冲内）。
    fn absolute_row(&self) -> usize {
        self.viewport_start() + self.cursor_row
    }

    /// 确保缓冲至少覆盖到绝对行 `row`（不足补空行；主屏可能追加进回滚）。
    fn ensure_row(&mut self, row: usize) {
        let lines = self.active_lines_mut();
        if row >= lines.len() {
            lines.resize(row + 1, Vec::new());
        }
    }

    /// 当前行引用内写入：列越界先软换行（DECAWM 简化：立即折行）。
    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.linefeed();
            self.cursor_col = 0;
        }
        let row = self.absolute_row();
        self.ensure_row(row);
        let col = self.cursor_col;
        let cols = self.cols;
        let line = self.active_lines_mut().get_mut(row).expect("行已确保存在");
        while line.len() < col {
            line.push(' ');
        }
        if col < line.len() {
            line[col] = ch;
        } else {
            line.push(ch);
        }
        let _ = cols;
        self.cursor_col += 1;
    }

    /// 换行：光标下移；越过滚动区域底端时区域整体上滚一行。
    fn linefeed(&mut self) {
        if self.cursor_row >= self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
            let row = self.absolute_row();
            self.ensure_row(row);
        }
    }

    /// 反向换行：光标上移；越过滚动区域顶端时区域下滚一行。
    fn reverse_linefeed(&mut self) {
        if self.cursor_row <= self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    /// 滚动区域内上滚 `n` 行：区域覆盖整个可见区时（常见于普通输出翻
    /// 屏），淘汰行保留在主屏缓冲头部（回滚区），底部补空行；区域只是
    /// 可见区的一部分时（vi 状态行等），区域内上移、顶行直接丢弃
    /// （xterm 语义：部分区域滚动不进回滚区）。
    fn scroll_up(&mut self, n: usize) {
        let viewport_top = self.viewport_start();
        let viewport_bottom = viewport_top + self.rows.saturating_sub(1);
        let top = viewport_top + self.scroll_top;
        let bottom = viewport_top + self.scroll_bottom;
        let full_screen = top == viewport_top && bottom == viewport_bottom && !self.in_alt;
        let lines = self.active_lines_mut();
        let bottom = bottom.min(lines.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let take = n.min(bottom - top + 1);
        if full_screen {
            lines.truncate(bottom + 1);
            for _ in 0..take {
                lines.push(Vec::new());
            }
        } else {
            lines.drain(top..top + take);
            for _ in 0..take {
                lines.insert(bottom + 1 - take, Vec::new());
            }
        }
        self.trim_scrollback();
    }

    /// 滚动区域内下滚 `n` 行：底部行丢弃，顶部补空行。
    fn scroll_down(&mut self, n: usize) {
        let top = self.viewport_start() + self.scroll_top;
        let bottom = self.viewport_start() + self.scroll_bottom;
        let lines = self.active_lines_mut();
        let bottom = bottom.min(lines.len().saturating_sub(1));
        if top > bottom {
            return;
        }
        let take = n.min(bottom - top + 1);
        lines.drain(bottom + 1 - take..bottom + 1);
        for _ in 0..take {
            lines.insert(top, Vec::new());
        }
        self.trim_scrollback();
    }

    /// 主屏回滚超限淘汰（alt 无回滚，不处理）。
    fn trim_scrollback(&mut self) {
        if self.in_alt {
            return;
        }
        let overflow = self.main.len().saturating_sub(self.rows + SCROLLBACK_LINES);
        if overflow > 0 {
            self.main.drain(..overflow);
        }
    }

    /// 进入 alternate screen：保存光标、切换并清屏（1049 语义）。
    fn enter_alt_screen(&mut self) {
        if self.in_alt {
            return;
        }
        self.saved_cursor = (self.cursor_row, self.cursor_col);
        self.in_alt = true;
        self.alt = vec![Vec::new(); self.rows];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    /// 退出 alternate screen：恢复主屏与保存的光标。
    fn leave_alt_screen(&mut self) {
        if !self.in_alt {
            return;
        }
        self.in_alt = false;
        self.alt.clear();
        let (row, col) = self.saved_cursor;
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    /// 可见区全清（ED 2 等价）：主屏清可见区与回滚由参数决定。
    fn clear_screen(&mut self, including_scrollback: bool) {
        if self.in_alt {
            self.alt = vec![Vec::new(); self.rows];
        } else {
            if including_scrollback {
                self.main.clear();
            } else {
                let start = self.viewport_start();
                self.main.truncate(start);
            }
            self.ensure_row(self.viewport_start());
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }
}

/// 从 vte 参数取第 `index` 个值（缺省 `default`）。
fn param(params: &Params, index: usize, default: usize) -> usize {
    params
        .iter()
        .nth(index)
        .and_then(|param| param.first())
        .copied()
        .map(usize::from)
        .unwrap_or(default)
}

impl Perform for GridHandler {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0B | 0x0C => self.linefeed(),
            b'\r' => self.cursor_col = 0,
            b'\x08' => self.cursor_col = self.cursor_col.saturating_sub(1),
            b'\t' => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols.saturating_sub(1));
            }
            0x07 => {}
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // 私有参数（`?` 前缀）：模式设置/复位（DECSET / DECRST）。
        if intermediates == b"?" {
            if action == 'h' || action == 'l' {
                let enable = action == 'h';
                for index in 0..params.len() {
                    match param(params, index, 0) {
                        // 1047/1049：alternate screen（1049 附带保存光标与清屏）。
                        1049 if enable => self.enter_alt_screen(),
                        1049 => self.leave_alt_screen(),
                        // 47/1047：无保存语义的切换，同样按进入/退出处理。
                        47 | 1047 if enable => self.enter_alt_screen(),
                        47 | 1047 => self.leave_alt_screen(),
                        _ => {}
                    }
                }
            }
            return;
        }
        match action {
            // 光标上/下/右/左移（CUU/CUD/CUF/CUB）。
            'A' => {
                let n = param(params, 0, 1).max(1);
                self.cursor_row = self.cursor_row.saturating_sub(n).max(self.scroll_top);
            }
            'B' => {
                let n = param(params, 0, 1).max(1);
                self.cursor_row = (self.cursor_row + n).min(self.scroll_bottom);
            }
            'C' => {
                let n = param(params, 0, 1).max(1);
                self.cursor_col = (self.cursor_col + n).min(self.cols.saturating_sub(1));
            }
            'D' => {
                let n = param(params, 0, 1).max(1);
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            // 绝对定位（CUP/HVP）与单轴定位（VPA/CHA）。
            'H' | 'f' => {
                let row = param(params, 0, 1).max(1) - 1;
                let col = param(params, 1, 1).max(1) - 1;
                self.cursor_row = row.min(self.rows.saturating_sub(1));
                self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            'd' => {
                let row = param(params, 0, 1).max(1) - 1;
                self.cursor_row = row.min(self.rows.saturating_sub(1));
            }
            'G' => {
                let col = param(params, 0, 1).max(1) - 1;
                self.cursor_col = col.min(self.cols.saturating_sub(1));
            }
            // 擦除显示（ED）。
            'J' => {
                let row = self.absolute_row();
                let col = self.cursor_col;
                match param(params, 0, 0) {
                    0 => {
                        self.ensure_row(row);
                        let lines = self.active_lines_mut();
                        if let Some(line) = lines.get_mut(row) {
                            line.truncate(col);
                        }
                        if lines.len() > row + 1 {
                            lines.truncate(row + 1);
                        }
                    }
                    1 => {
                        self.ensure_row(row);
                        let lines = self.active_lines_mut();
                        if let Some(line) = lines.get_mut(row) {
                            for cell in line.iter_mut().take(col + 1) {
                                *cell = ' ';
                            }
                        }
                    }
                    2 => self.clear_screen(false),
                    3 => self.clear_screen(true),
                    _ => {}
                }
            }
            // 擦除行内（EL）。
            'K' => {
                let row = self.absolute_row();
                self.ensure_row(row);
                let col = self.cursor_col;
                let lines = self.active_lines_mut();
                let Some(line) = lines.get_mut(row) else {
                    return;
                };
                match param(params, 0, 0) {
                    0 => line.truncate(col),
                    1 => {
                        while line.len() < col {
                            line.push(' ');
                        }
                        for cell in line.iter_mut().take(col + 1) {
                            *cell = ' ';
                        }
                    }
                    2 => line.clear(),
                    _ => {}
                }
            }
            // 行插入/删除（IL/DL）：光标行起、滚动区域内。
            'L' => {
                let n = param(params, 0, 1).max(1);
                let row = self.absolute_row();
                let bottom = self.viewport_start() + self.scroll_bottom;
                self.ensure_row(row);
                let lines = self.active_lines_mut();
                let bottom = bottom.min(lines.len().saturating_sub(1));
                if row <= bottom {
                    let take = n.min(bottom - row + 1);
                    lines.drain(bottom + 1 - take..bottom + 1);
                    for _ in 0..take {
                        lines.insert(row, Vec::new());
                    }
                }
            }
            'M' => {
                let n = param(params, 0, 1).max(1);
                let row = self.absolute_row();
                let bottom = self.viewport_start() + self.scroll_bottom;
                self.ensure_row(row);
                let lines = self.active_lines_mut();
                let bottom = bottom.min(lines.len().saturating_sub(1));
                if row <= bottom {
                    let take = n.min(bottom - row + 1);
                    lines.drain(row..row + take);
                    for _ in 0..take {
                        lines.insert(bottom + 1 - take, Vec::new());
                    }
                }
            }
            // 滚动（SU/SD）。
            'S' => {
                let n = param(params, 0, 1).max(1);
                self.scroll_up(n);
            }
            'T' => {
                let n = param(params, 0, 1).max(1);
                self.scroll_down(n);
            }
            // 擦除字符（ECH）/ 删除字符（DCH）/ 插入空白（ICH）。
            'X' => {
                let n = param(params, 0, 1).max(1);
                let row = self.absolute_row();
                self.ensure_row(row);
                let col = self.cursor_col;
                let lines = self.active_lines_mut();
                if let Some(line) = lines.get_mut(row) {
                    while line.len() < col + n {
                        line.push(' ');
                    }
                    for offset in 0..n {
                        if let Some(cell) = line.get_mut(col + offset) {
                            *cell = ' ';
                        }
                    }
                }
            }
            'P' => {
                let n = param(params, 0, 1).max(1);
                let row = self.absolute_row();
                self.ensure_row(row);
                let col = self.cursor_col;
                let lines = self.active_lines_mut();
                if let Some(line) = lines.get_mut(row)
                    && col < line.len()
                {
                    let end = (col + n).min(line.len());
                    line.drain(col..end);
                }
            }
            '@' => {
                let n = param(params, 0, 1).max(1);
                let row = self.absolute_row();
                self.ensure_row(row);
                let col = self.cursor_col;
                let lines = self.active_lines_mut();
                if let Some(line) = lines.get_mut(row) {
                    while line.len() < col {
                        line.push(' ');
                    }
                    for _ in 0..n {
                        line.insert(col, ' ');
                    }
                }
            }
            // 滚动区域（DECSTBM）：top/bottom 为 1 起全屏坐标。
            'r' => {
                let top = param(params, 0, 1).max(1) - 1;
                let bottom = match params.iter().next() {
                    Some(_) => param(params, 1, self.rows).max(1) - 1,
                    None => self.rows.saturating_sub(1),
                };
                if top < bottom {
                    self.scroll_top = top.min(self.rows.saturating_sub(1));
                    self.scroll_bottom = bottom.min(self.rows.saturating_sub(1));
                    self.cursor_row = self.scroll_top;
                    self.cursor_col = 0;
                }
            }
            // SGR（m）与其他不影响文本的序列忽略。
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // IND：换行（区域滚动）。
            b'D' => self.linefeed(),
            // NEL：回车 + 换行。
            b'E' => {
                self.cursor_col = 0;
                self.linefeed();
            }
            // RI：反向换行。
            b'M' => self.reverse_linefeed(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> ScreenGrid {
        ScreenGrid::new(80, 24)
    }

    #[test]
    fn plain_lines_are_joined() {
        let mut grid = grid();
        grid.feed("one\r\ntwo\r\nthree");
        assert_eq!(grid.visible_text(), "one\ntwo\nthree");
    }

    #[test]
    fn trailing_blank_lines_are_trimmed() {
        let mut grid = grid();
        grid.feed("hello\r\n\r\n\r\n");
        assert_eq!(grid.visible_text(), "hello");
    }

    #[test]
    fn cursor_position_overwrites() {
        let mut grid = grid();
        grid.feed("abcdef\x1b[3Dxyz");
        assert_eq!(grid.visible_text(), "abcxyz");
    }

    #[test]
    fn erase_line_variants() {
        let mut grid = grid();
        grid.feed("hello world\x1b[6D\x1b[K");
        assert_eq!(grid.visible_text(), "hello");
    }

    #[test]
    fn alternate_screen_roundtrip() {
        let mut grid = grid();
        grid.feed("main prompt\r\n");
        grid.feed("\x1b[?1049h");
        grid.feed("vi screen");
        assert_eq!(grid.visible_text(), "vi screen");
        grid.feed("\x1b[?1049l");
        assert_eq!(grid.visible_text(), "main prompt");
    }

    #[test]
    fn full_screen_clear() {
        let mut grid = grid();
        grid.feed("a\r\nb\r\nc\r\n");
        grid.feed("\x1b[2J\x1b[H");
        assert_eq!(grid.visible_text(), "");
    }

    #[test]
    fn scroll_region_keeps_status_line() {
        // vi 风格：顶部滚动区域 + 底部状态行。区域内滚动时状态行不动、
        // 也不把区域顶行挤进回滚（部分区域滚动直接丢弃顶行）。
        let mut g = ScreenGrid::new(20, 4);
        g.feed("\x1b[1;2r"); // 区域 = 行 1..2
        g.feed("\x1b[1;1Haa\r\nbb\r\n"); // 写满区域并触发一次区域内滚动
        g.feed("\x1b[4;1HSTATUS"); // 状态行（区域外）
        let text = g.visible_text();
        assert!(text.contains("bb"), "区域内滚动保留区域底行: {text}");
        assert!(text.contains("STATUS"), "状态行不被滚动挤掉: {text}");
        assert!(!text.contains("aa"), "区域顶行被淘汰: {text}");
    }

    #[test]
    fn bottom_overflow_scrolls() {
        let mut g = ScreenGrid::new(80, 3);
        g.feed("a\r\nb\r\nc\r\nd\r\ne");
        assert_eq!(g.visible_text(), "c\nd\ne");
    }

    #[test]
    fn line_insert_and_delete() {
        let mut grid = grid();
        grid.feed("one\r\ntwo\r\nthree");
        grid.feed("\x1b[2;1H\x1b[L");
        assert_eq!(grid.visible_text(), "one\n\ntwo\nthree");
        grid.feed("\x1b[2;1H\x1b[2M");
        assert_eq!(grid.visible_text(), "one\nthree");
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut grid = grid();
        grid.feed("\ta");
        // 8 列 tab stop：第 9 列写 a。
        assert_eq!(grid.visible_text(), "        a");
    }

    #[test]
    fn resize_keeps_content_and_clamps_cursor() {
        let mut grid = grid();
        grid.feed("one\r\ntwo\r\nthree");
        grid.resize(100, 30);
        assert_eq!(grid.visible_text(), "one\ntwo\nthree");
        grid.resize(40, 2);
        let text = grid.visible_text();
        assert!(
            text.ends_with("two\nthree") || text == "two\nthree",
            "收缩保留最新内容: {text}"
        );
    }

    #[test]
    fn backspace_moves_cursor() {
        let mut grid = grid();
        grid.feed("abc\x08\x08Z");
        assert_eq!(grid.visible_text(), "aZc");
    }

    #[test]
    fn wide_output_scrolls_into_scrollback() {
        let mut g = ScreenGrid::new(80, 3);
        for index in 0..10 {
            g.feed(&format!("line-{index}\r\n"));
        }
        g.feed("tail");
        // 可见区保留最后三行内容（倒数第二行 line-9、空行由 tail 占据）。
        assert_eq!(g.visible_text(), "line-8\nline-9\ntail");
    }
}
