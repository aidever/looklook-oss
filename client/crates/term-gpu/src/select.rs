//! 选字、搜索、找链接（设计稿 §7.3）：都按“行号 + 列”算，行号见 term-web 的 history.rs（回滚序号和屏幕行连成一条），
//! 滚动、新输出时选区和搜索结果不会错位。

use term_web::{LineRef, TermClient};

/// 读行的来源（真实的是 TermClient；测试里也用它）
pub trait Lines {
    fn line(&self, abs: u64) -> Option<LineRef<'_>>;
    /// 屏幕第 0 行的行号
    fn base(&self) -> u64;
    fn rows(&self) -> u16;
    fn cols(&self) -> u16;
    /// 本地最早的一行
    fn first(&self) -> u64;
}

impl Lines for TermClient {
    fn line(&self, abs: u64) -> Option<LineRef<'_>> {
        TermClient::line(self, abs)
    }
    fn base(&self) -> u64 {
        self.screen_base()
    }
    fn rows(&self) -> u16 {
        self.screen().map_or(0, |s| s.rows)
    }
    fn cols(&self) -> u16 {
        self.screen().map_or(0, |s| s.cols)
    }
    fn first(&self) -> u64 {
        let alt = self.screen().is_some_and(|s| s.modes.alt_screen);
        match self.history() {
            Some(h) if !alt => h.first_local().min(self.screen_base()),
            _ => self.screen_base(),
        }
    }
}

/// 格子之间的位置：`col` 是左边界（0..=cols），`line` 行号
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: u64,
    pub col: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Char,
    Word,
    Line,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub mode: Mode,
    /// Alt 拖动：按列选一个矩形
    pub block: bool,
    /// 按下处（选词 / 选行时是那个词 / 那一行的范围）
    pub anchor: (Point, Point),
    pub head: (Point, Point),
}

/// 一行里被选中的列：[from, to)
pub type Span = (u64, u32, u32);

const SEPARATORS: &str = "()[]{}<>'\"`,;|│";

fn class(c: char) -> u8 {
    if c == ' ' || c == '\0' || c == '\t' || c.is_whitespace() {
        0
    } else if SEPARATORS.contains(c) {
        2
    } else {
        1
    }
}

/// 一行的格子：每格（起始列、宽度、字符），后半格不算
fn cells(line: &LineRef<'_>) -> Vec<(u32, u32, char)> {
    let mut out = Vec::with_capacity(line.len());
    for i in 0..line.len() {
        let Some(c) = line.cell(i) else { break };
        if c.width == 0 {
            continue;
        }
        out.push((i as u32, c.width as u32, c.c));
    }
    out
}

fn char_at(line: &LineRef<'_>, col: u32) -> char {
    // 后半格算作它前面那个宽字符
    let mut i = col as usize;
    loop {
        match line.cell(i) {
            Some(c) if c.width == 0 && i > 0 => i -= 1,
            Some(c) => return c.c,
            None => return ' ',
        }
    }
}

/// 点在 `p` 上的那个“词”（同一类字符连成的一段；分隔符单独一个），返回 [起, 止)
pub fn word_at(lines: &dyn Lines, p: Point) -> (Point, Point) {
    let Some(line) = lines.line(p.line) else { return (p, Point { line: p.line, col: p.col + 1 }) };
    let k = class(char_at(&line, p.col));
    let cs = cells(&line);
    let cols = lines.cols().max(line.len() as u16) as u32;
    let idx = cs.iter().position(|&(c, w, _)| p.col >= c && p.col < c + w);
    let Some(idx) = idx else {
        // 行尾之后的空白：选到行尾
        let end_text = cs.last().map_or(0, |&(c, w, _)| c + w);
        return (Point { line: p.line, col: end_text.min(p.col) }, Point { line: p.line, col: cols });
    };
    if k == 2 {
        let (c, w, _) = cs[idx];
        return (Point { line: p.line, col: c }, Point { line: p.line, col: c + w });
    }
    let mut s = idx;
    while s > 0 && class(cs[s - 1].2) == k {
        s -= 1;
    }
    let mut e = idx;
    while e + 1 < cs.len() && class(cs[e + 1].2) == k {
        e += 1;
    }
    (Point { line: p.line, col: cs[s].0 }, Point { line: p.line, col: cs[e].0 + cs[e].1 })
}

/// 三击：整个逻辑行（自动折行的上下行算一行）
pub fn line_at(lines: &dyn Lines, line: u64) -> (Point, Point) {
    let mut s = line;
    while s > 0 && lines.line(s - 1).is_some_and(|l| l.wrapped()) {
        s -= 1;
    }
    let mut e = line;
    while lines.line(e).is_some_and(|l| l.wrapped()) && lines.line(e + 1).is_some() {
        e += 1;
    }
    (Point { line: s, col: 0 }, Point { line: e, col: u32::MAX })
}

impl Selection {
    pub fn new(lines: &dyn Lines, p: Point, mode: Mode, block: bool) -> Selection {
        let r = match mode {
            Mode::Char => (p, p),
            Mode::Word => word_at(lines, p),
            Mode::Line => line_at(lines, p.line),
        };
        Selection { mode, block, anchor: r, head: r }
    }

    pub fn extend(&mut self, lines: &dyn Lines, p: Point) {
        self.head = match self.mode {
            Mode::Char => (p, p),
            Mode::Word => word_at(lines, p),
            Mode::Line => line_at(lines, p.line),
        };
    }

    /// 选区的起止（按阅读顺序）
    pub fn range(&self) -> (Point, Point) {
        let (a0, a1) = self.anchor;
        let (h0, h1) = self.head;
        if h0 < a0 {
            (h0, a1.max(h1))
        } else {
            (a0, h1.max(a1))
        }
    }

    pub fn is_empty(&self) -> bool {
        let (s, e) = self.range();
        !self.block && s == e
    }

    /// 每一行被选中的列
    pub fn spans(&self, cols: u32) -> Vec<Span> {
        let (s, e) = self.range();
        if self.block {
            let (c0, c1) = {
                let a = self.anchor.0.col.min(self.head.0.col);
                let b = self.anchor.1.col.max(self.head.1.col).max(self.anchor.0.col.max(self.head.0.col));
                (a, b.min(cols))
            };
            if c0 >= c1 {
                return Vec::new();
            }
            return (s.line..=e.line).map(|l| (l, c0, c1)).collect();
        }
        if s == e {
            return Vec::new();
        }
        let mut out = Vec::new();
        for l in s.line..=e.line {
            let from = if l == s.line { s.col } else { 0 };
            let to = if l == e.line { e.col.min(cols) } else { cols };
            if from < to {
                out.push((l, from, to));
            }
        }
        out
    }

    pub fn contains(&self, line: u64, col: u32, cols: u32) -> bool {
        self.spans(cols).iter().any(|&(l, a, b)| l == line && col >= a && col < b)
    }

    /// 复制出来的文字：行尾空白去掉，自动折行的行拼回一行
    pub fn text(&self, lines: &dyn Lines) -> String {
        let cols = lines.cols() as u32;
        let spans = self.spans(cols.max(1));
        let mut out = String::new();
        for (i, &(l, a, b)) in spans.iter().enumerate() {
            let Some(line) = lines.line(l) else {
                if i + 1 < spans.len() {
                    out.push('\n');
                }
                continue;
            };
            let mut seg = String::new();
            for (c, w, ch) in cells(&line) {
                if c + w <= a || c >= b {
                    continue;
                }
                let cell = line.cell(c as usize).expect("cell exists");
                seg.push(if ch == '\0' || ch == '\t' { ' ' } else { ch });
                seg.push_str(cell.extra);
            }
            let to_end = b >= cols;
            let joined = !self.block && to_end && line.wrapped();
            if !joined {
                let t = seg.trim_end_matches(' ').len();
                seg.truncate(t);
            }
            out.push_str(&seg);
            if i + 1 < spans.len() && !joined {
                out.push('\n');
            }
        }
        out
    }
}

// ---------- 搜索 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub line: u64,
    pub from: u32,
    pub to: u32,
}

#[derive(Default)]
pub struct Search {
    pub query: String,
    pub case_sensitive: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
}

const MAX_MATCHES: usize = 20_000;

fn fold(c: char, case_sensitive: bool) -> char {
    if case_sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

impl Search {
    /// 在本地的回滚区 + 屏幕里找（不跨行）
    pub fn run(&mut self, lines: &dyn Lines, query: &str, case_sensitive: bool) {
        let prev = self.current.and_then(|i| self.matches.get(i)).copied();
        self.query = query.to_string();
        self.case_sensitive = case_sensitive;
        self.matches.clear();
        self.current = None;
        let q: Vec<char> = query.chars().map(|c| fold(c, case_sensitive)).collect();
        if q.is_empty() {
            return;
        }
        let end = lines.base() + lines.rows() as u64;
        for l in lines.first()..end {
            let Some(line) = lines.line(l) else { continue };
            let cs = cells(&line);
            if cs.len() < q.len() {
                continue;
            }
            let chars: Vec<char> = cs.iter().map(|&(_, _, c)| fold(c, case_sensitive)).collect();
            let mut i = 0;
            while i + q.len() <= chars.len() {
                if chars[i..i + q.len()] == q[..] {
                    let last = cs[i + q.len() - 1];
                    self.matches.push(Match { line: l, from: cs[i].0, to: last.0 + last.1 });
                    if self.matches.len() >= MAX_MATCHES {
                        return;
                    }
                    i += q.len();
                } else {
                    i += 1;
                }
            }
        }
        // 重新搜（有新输出）时尽量停在原来那一个上
        if let Some(p) = prev {
            self.current = self.matches.iter().position(|m| *m == p);
        }
    }

    /// 下一个：`older` 往上（更早的输出）找。第一次从最下面开始。返回当前这个
    pub fn step(&mut self, older: bool) -> Option<Match> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }
        let n = self.matches.len();
        let i = match self.current {
            None => n - 1,
            Some(i) if older => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
        };
        self.current = Some(i);
        Some(self.matches[i])
    }

    pub fn clear(&mut self) {
        *self = Search::default();
    }
}

// ---------- 链接 ----------

/// (line, col) 上的链接：OSC 8 的优先，再找文字里的 http(s)://… 。返回 (地址, 起列, 止列)
pub fn link_at(lines: &dyn Lines, line: u64, col: u32) -> Option<(String, u32, u32)> {
    let l = lines.line(line)?;
    let cell = l.cell(col as usize)?;
    if let Some(uri) = l.link(cell.style) {
        // OSC 8：同一个链接连着的格子
        let id = cell.style.hyperlink_id;
        let same = |i: usize| l.cell(i).is_some_and(|c| c.style.hyperlink_id == id);
        let mut s = col as usize;
        while s > 0 && same(s - 1) {
            s -= 1;
        }
        let mut e = col as usize + 1;
        while e < l.len() && same(e) {
            e += 1;
        }
        return Some((uri.to_string(), s as u32, e as u32));
    }
    let cs = cells(&l);
    let chars: Vec<char> = cs.iter().map(|c| c.2).collect();
    let text: String = chars.iter().collect();
    let mut search_from = 0;
    while let Some(off) = ["https://", "http://"]
        .iter()
        .filter_map(|p| text[search_from..].find(p).map(|i| i + search_from))
        .min()
    {
        let start_char = text[..off].chars().count();
        let mut end_char = start_char;
        while end_char < chars.len() {
            let c = chars[end_char];
            if c.is_whitespace() || c == '\0' || "<>\"'`|│".contains(c) || (c as u32) < 0x20 {
                break;
            }
            end_char += 1;
        }
        // 句末标点、没配对的右括号不算在链接里
        while end_char > start_char {
            let c = chars[end_char - 1];
            let opens = chars[start_char..end_char].iter().filter(|&&x| x == '(').count();
            let closes = chars[start_char..end_char].iter().filter(|&&x| x == ')').count();
            if ".,;:!?]}".contains(c) || (c == ')' && closes > opens) {
                end_char -= 1;
            } else {
                break;
            }
        }
        let from = cs[start_char].0;
        let to = cs.get(end_char - 1).map_or(from, |c| c.0 + c.1);
        if col >= from && col < to && end_char > start_char + 8 {
            return Some((chars[start_char..end_char].iter().collect(), from, to));
        }
        search_from = off + 1;
        while !text.is_char_boundary(search_from) {
            search_from += 1;
        }
    }
    None
}
