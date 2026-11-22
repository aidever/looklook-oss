//! 本地预测回显（设计稿 §5.7，参照 Mosh）：往返慢的时候，打的字先画在光标处（带一条淡下划线），
//! 等 termd 的帧确认了再以 termd 的画面为准。
//!
//! - 只预测“光标往后走一格”的输入：可打印的 ASCII（一次输入最多 [`MAX_TYPED`] 个字）；退格擦掉最后一个预测；
//! - 其他输入（回车、方向键、Ctrl 组合键、粘贴、没预测的字）都立一道“栅栏”：光标去哪了只有等 termd 说，
//!   栅栏之前的输入被确认前不再预测；
//! - 帧里“已处理到的输入序号”（termd 写进 PTY 后、程序有输出或过了一小会儿才算，见 termd `ECHO_GRACE`）
//!   覆盖了某个预测：屏幕上那一格是这个字就算对，去掉；不是（没回显，比如输密码；或回显不一样）就把预测全撤掉，
//!   暂停到下一次回车；
//! - 备用屏幕（vim、htop）、光标不可见、行尾附近不预测。

use crate::ScreenSnapshot;

/// 一次输入（一个 input 事件、输入法一次上屏）最多预测这么多字，再多多半是粘贴
pub const MAX_TYPED: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prediction {
    pub seq: u64,
    pub row: u16,
    pub col: u16,
    pub c: char,
}

#[derive(Debug, Default)]
pub struct Predictor {
    /// 往返够慢（JS 量的，> 30 ms）才开
    enabled: bool,
    /// 猜错了：到下一次回车之前不预测
    paused: bool,
    /// 这个序号之前的输入没被确认前不预测（光标位置不确定）
    barrier: u64,
    preds: Vec<Prediction>,
    /// 预测变了（JS 据此决定要不要重画）
    version: u32,
}

fn typed_chars(data: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(data).ok()?;
    (!s.is_empty() && s.len() <= MAX_TYPED && s.bytes().all(|b| (0x20..0x7f).contains(&b))).then_some(s)
}

fn screen_allows(s: &ScreenSnapshot) -> bool {
    !s.modes.alt_screen && s.modes.show_cursor && s.cursor.visible
}

impl Predictor {
    pub fn predictions(&self) -> &[Prediction] {
        &self.preds
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 光标该画在哪（有预测时在最后一个预测后面）
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.preds.last().map(|p| (p.row, p.col + 1))
    }

    fn changed(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    fn clear(&mut self) {
        if !self.preds.is_empty() {
            self.preds.clear();
            self.changed();
        }
    }

    pub fn set_enabled(&mut self, on: bool, last_seq: u64) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if !on {
            self.clear();
        }
        // 开关之间的输入没记位置
        self.barrier = self.barrier.max(last_seq);
    }

    /// 每条输入都要过一遍（`typed`：键盘打的字，不是粘贴、不是编好的转义），`acked`：当前帧确认到的序号。
    pub fn on_input(&mut self, seq: u64, data: &[u8], typed: bool, screen: Option<&ScreenSnapshot>, acked: u64) {
        if data == b"\r" || data == b"\n" || data == b"\r\n" {
            self.paused = false;
            self.barrier = seq;
            return;
        }
        if data == b"\x7f" || data == b"\x08" {
            if self.preds.pop().is_some() {
                self.changed();
            } else {
                self.barrier = seq;
            }
            return;
        }
        let Some(text) = typed.then(|| typed_chars(data)).flatten() else {
            self.barrier = seq;
            return;
        };
        let Some(s) = screen.filter(|s| screen_allows(s)) else {
            self.barrier = seq;
            return;
        };
        if !self.enabled || self.paused || acked < self.barrier {
            self.barrier = seq;
            return;
        }
        let (row, mut col) = match self.preds.last() {
            Some(p) => (p.row, p.col + 1),
            None => (s.cursor.row, s.cursor.col),
        };
        for c in text.chars() {
            // 最后一列留给真实回显（折行、zsh 右侧提示）
            if col + 1 >= s.cols || row >= s.rows {
                self.barrier = seq;
                break;
            }
            self.preds.push(Prediction { seq, row, col, c });
            col += 1;
        }
        self.changed();
    }

    /// 新的一帧应用完了：确认或撤掉预测。`last_seq`：已经发出的最后一个输入序号。
    pub fn on_frame(&mut self, s: &ScreenSnapshot, acked: u64, last_seq: u64) {
        if self.preds.is_empty() {
            return;
        }
        if !screen_allows(s) {
            self.clear();
            self.barrier = self.barrier.max(last_seq);
            return;
        }
        let mut confirmed = 0;
        for p in &self.preds {
            if p.seq > acked {
                break;
            }
            let ok = s
                .lines
                .get(p.row as usize)
                .and_then(|l| l.cells.get(p.col as usize))
                .is_some_and(|cell| cell.c == p.c && cell.width == 1);
            if !ok {
                self.clear();
                self.paused = true;
                self.barrier = self.barrier.max(last_seq);
                return;
            }
            confirmed += 1;
        }
        if confirmed > 0 {
            self.preds.drain(..confirmed);
            self.changed();
        }
    }

    /// 换了连接、整屏重来：预测作废
    pub fn reset(&mut self, last_seq: u64) {
        self.clear();
        self.barrier = self.barrier.max(last_seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CellData, ScreenRow};

    fn screen(text: &str, cursor_col: u16) -> ScreenSnapshot {
        let mut s = ScreenSnapshot {
            cols: 20,
            rows: 2,
            lines: vec![ScreenRow { cells: vec![CellData::default(); 20], wrapped: false }; 2],
            ..empty()
        };
        for (i, c) in text.chars().enumerate() {
            s.lines[0].cells[i].c = c;
        }
        s.cursor.col = cursor_col;
        s
    }

    fn empty() -> ScreenSnapshot {
        ScreenSnapshot {
            cols: 0,
            rows: 0,
            lines: Vec::new(),
            style_table: Default::default(),
            cursor: Default::default(),
            modes: Default::default(),
            title: None,
            cwd: None,
            scrollback_epoch: 0,
            scrollback_oldest_seq: 0,
            scrollback_next_seq: 0,
            palette_overrides: Vec::new(),
        }
    }

    #[test]
    fn predicts_and_confirms() {
        let mut p = Predictor::default();
        p.set_enabled(true, 0);
        let s = screen("$ ", 2);
        p.on_input(1, b"l", true, Some(&s), 0);
        p.on_input(2, b"s", true, Some(&s), 0);
        assert_eq!(p.predictions().len(), 2);
        assert_eq!(p.cursor(), Some((0, 4)));
        // 第一个字回显了
        let s1 = screen("$ l", 3);
        p.on_frame(&s1, 1, 2);
        assert_eq!(p.predictions(), &[Prediction { seq: 2, row: 0, col: 3, c: 's' }]);
        let s2 = screen("$ ls", 4);
        p.on_frame(&s2, 2, 2);
        assert!(p.predictions().is_empty());
        // 接着还能预测（位置从屏幕光标起）
        p.on_input(3, b"x", true, Some(&s2), 2);
        assert_eq!(p.predictions(), &[Prediction { seq: 3, row: 0, col: 4, c: 'x' }]);
    }

    #[test]
    fn no_echo_pauses_until_enter() {
        let mut p = Predictor::default();
        p.set_enabled(true, 0);
        let s = screen("Password: ", 10);
        p.on_input(1, b"a", true, Some(&s), 0);
        assert_eq!(p.predictions().len(), 1);
        p.on_frame(&s, 1, 1);
        assert!(p.predictions().is_empty());
        p.on_input(2, b"b", true, Some(&s), 1);
        assert!(p.predictions().is_empty(), "paused");
        p.on_input(3, b"\r", false, Some(&s), 1);
        let s2 = screen("$ ", 2);
        // 回车还没被确认：光标在哪不知道
        p.on_input(4, b"c", true, Some(&s2), 2);
        assert!(p.predictions().is_empty());
        p.on_input(5, b"d", true, Some(&s2), 4);
        assert_eq!(p.predictions().len(), 1);
    }

    #[test]
    fn backspace_and_barriers() {
        let mut p = Predictor::default();
        p.set_enabled(true, 0);
        let s = screen("$ ", 2);
        p.on_input(1, b"ab", true, Some(&s), 0);
        p.on_input(2, b"\x7f", false, Some(&s), 0);
        assert_eq!(p.predictions().len(), 1);
        p.on_input(3, b"\x1b[D", false, Some(&s), 0);
        p.on_input(4, b"z", true, Some(&s), 0);
        assert_eq!(p.predictions().len(), 1, "arrow key is a barrier");
        // 粘贴不预测
        p.on_input(5, b"hello", false, Some(&s), 5);
        assert_eq!(p.predictions().len(), 1);
        // 备用屏幕：撤掉
        let mut alt = screen("", 0);
        alt.modes.alt_screen = true;
        p.on_frame(&alt, 0, 5);
        assert!(p.predictions().is_empty());
    }

    #[test]
    fn disabled_predicts_nothing() {
        let mut p = Predictor::default();
        let s = screen("$ ", 2);
        p.on_input(1, b"a", true, Some(&s), 0);
        assert!(p.predictions().is_empty());
        p.set_enabled(true, 1);
        p.on_input(2, b"a", true, Some(&s), 0);
        assert!(p.predictions().is_empty(), "seq 1 not acked yet");
        p.on_input(3, b"a", true, Some(&s), 2);
        assert_eq!(p.predictions().len(), 1);
    }
}
