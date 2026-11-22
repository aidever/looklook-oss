use std::collections::HashMap;

use term_core::{AlacrittyCore, CoreConfig, TerminalCore};
use term_proto::{encode_raw_packet, DeltaEncoder, ServerMsg};
use term_web::TermClient;

use crate::scene::{GlyphKey, Glyphs, Slot, KIND_MASK, KIND_RECT};
use crate::select::{link_at, word_at, Mode, Point, Search, Selection};
use crate::state::State;
use crate::theme::{argb, Theme, BG, FG};

/// 真实的内核 + 差量编码器（和 termd 一样），输出喂给 TermClient
struct Host {
    core: AlacrittyCore,
    enc: DeltaEncoder,
    tc: TermClient,
}

impl Host {
    fn new(cols: u16, rows: u16) -> Self {
        let mut tc = TermClient::new();
        tc.keep_history();
        Host { core: AlacrittyCore::new(CoreConfig { cols, rows, ..CoreConfig::default() }).unwrap(), enc: DeltaEncoder::new(), tc }
    }

    fn out(&mut self, bytes: &str) -> String {
        self.core.feed_bytes(bytes.as_bytes());
        let mut out = Vec::new();
        let (epoch, rows) = self.core.take_new_scrollback();
        if !rows.is_empty() {
            out.extend(encode_raw_packet(&ServerMsg::Scrolled { epoch, rows }).unwrap());
        }
        let snap = self.core.snapshot();
        out.extend(encode_raw_packet(&self.enc.encode_frame(&snap, 0)).unwrap());
        self.tc.feed(&out)
    }
}

/// 假图集：每个字一个位置，记下要过哪些字
#[derive(Default)]
struct FakeGlyphs {
    map: HashMap<GlyphKey, Slot>,
    limit: Option<usize>,
}

impl Glyphs for FakeGlyphs {
    fn glyph(&mut self, key: &GlyphKey) -> Option<Slot> {
        if let Some(s) = self.map.get(key) {
            return Some(*s);
        }
        if self.limit.is_some_and(|l| self.map.len() >= l) {
            return None;
        }
        let n = self.map.len() as u32;
        let s = Slot { x: n * 16, y: 0, w: 8 * key.cells as u32, h: 16, color: false, empty: false };
        self.map.insert(key.clone(), s);
        Some(s)
    }
}

fn state() -> State {
    State::default()
}

#[test]
fn history_kept_in_wasm_and_lines_are_continuous() {
    let mut h = Host::new(20, 3);
    let json = h.out("one\r\ntwo\r\nthree\r\nfour\r\nfive");
    // 回滚行不再编进 JSON，只给行数
    assert!(json.contains("\"t\":\"scrolled\""), "{json}");
    assert!(json.contains("\"n\":2"), "{json}");
    assert!(!json.contains("two"), "{json}");
    let base = h.tc.screen_base();
    assert_eq!(base, 2);
    let text = |l: u64| h.tc.line(l).map(|x| x.text().trim_end().to_string());
    assert_eq!(text(0).as_deref(), Some("one"));
    assert_eq!(text(1).as_deref(), Some("two"));
    assert_eq!(text(2).as_deref(), Some("three"));
    assert_eq!(text(4).as_deref(), Some("five"));
    assert_eq!(h.tc.history_bounds(), vec![0.0, 0.0, 2.0]);
    assert!(h.tc.history_missing(0.0, 5.0).is_empty());
    // 清屏（回滚区清空，换 epoch）
    h.out("\x1b[3J");
    assert!(h.tc.history().unwrap().rows.iter().all(|r| r.is_none()));
}

#[test]
fn selection_text_trims_and_joins_wrapped_lines() {
    let mut h = Host::new(10, 4);
    h.out("hello   \r\n0123456789abc\r\nlast");
    let tc = &h.tc;
    // 第 0 行 "hello"，第 1、2 行是一行折成两行
    let mut sel = Selection::new(tc, Point { line: 0, col: 0 }, Mode::Char, false);
    sel.extend(tc, Point { line: 3, col: 4 });
    assert_eq!(sel.text(tc), "hello\n0123456789abc\nlast");
    // 选词、三击选整个逻辑行
    let (a, b) = word_at(tc, Point { line: 0, col: 2 });
    assert_eq!((a.col, b.col), (0, 5));
    let line = Selection::new(tc, Point { line: 2, col: 1 }, Mode::Line, false);
    assert_eq!(line.text(tc), "0123456789abc");
    // 按列选
    let mut block = Selection::new(tc, Point { line: 0, col: 1 }, Mode::Char, true);
    block.extend(tc, Point { line: 1, col: 3 });
    assert_eq!(block.text(tc), "el\n12");
}

#[test]
fn wide_chars_select_and_search_by_cell() {
    let mut h = Host::new(20, 2);
    h.out("中文 abc 中文");
    let tc = &h.tc;
    let (a, b) = word_at(tc, Point { line: 0, col: 1 }); // 后半格算在前一个字上
    assert_eq!((a.col, b.col), (0, 4));
    let mut s = Search::default();
    s.run(tc, "中文", false);
    assert_eq!(s.matches.iter().map(|m| (m.from, m.to)).collect::<Vec<_>>(), vec![(0, 4), (9, 13)]);
    s.run(tc, "ABC", false);
    assert_eq!(s.matches.len(), 1);
    s.run(tc, "ABC", true);
    assert_eq!(s.matches.len(), 0);
}

#[test]
fn search_steps_from_the_bottom_and_reveals() {
    let mut h = Host::new(20, 3);
    let mut text = String::new();
    for i in 0..30 {
        text.push_str(&format!("line {i} hit\r\n"));
    }
    h.out(&text);
    let mut st = state();
    st.sync(&h.tc);
    let n = st.search(&h.tc, "hit", false, true);
    assert_eq!(n, 30);
    assert_eq!(st.search.current, Some(29));
    st.search(&h.tc, "hit", false, true);
    assert_eq!(st.search.current, Some(28));
    // 跳到很早的一个：视图跟过去
    for _ in 0..20 {
        st.search(&h.tc, "hit", false, true);
    }
    assert_eq!(st.search.current, Some(8));
    assert!(!st.pinned);
    assert!(st.top <= 8.0 && st.top + 3.0 > 8.0, "top {}", st.top);
}

#[test]
fn links_osc8_and_plain_urls() {
    let mut h = Host::new(60, 2);
    h.out("see https://example.com/a_(b)). and \x1b]8;;https://x.y/\x1b\\here\x1b]8;;\x1b\\ end");
    let tc = &h.tc;
    let (url, a, b) = link_at(tc, 0, 10).unwrap();
    assert_eq!(url, "https://example.com/a_(b)");
    assert_eq!((a, b), (4, 29));
    assert!(link_at(tc, 0, 2).is_none());
    let start = "see https://example.com/a_(b)). and ".chars().count() as u32;
    let (url, a, b) = link_at(tc, 0, start + 1).unwrap();
    assert_eq!(url, "https://x.y/");
    assert_eq!((a, b), (start, start + 4));
}

#[test]
fn scene_merges_backgrounds_and_draws_cursor_last() {
    let mut h = Host::new(10, 2);
    h.out("\x1b[41mab\x1b[0m c\x1b[1mD");
    let mut st = state();
    let mut g = FakeGlyphs::default();
    let built = st.build(&h.tc, true, true, &mut g);
    let rects: Vec<_> = built.instances.iter().filter(|i| i.kind == KIND_RECT).collect();
    // 红底的两格并成一块
    let red = argb(Theme::default().get(1));
    let bg = rects.iter().find(|i| i.color == red).unwrap();
    assert_eq!(bg.rect, [5.0, 5.0, 16.0, 16.0]);
    // 字：a b c D（空格不画），粗体单独一个字形
    let glyphs: Vec<_> = built.instances.iter().filter(|i| i.kind == KIND_MASK).collect();
    assert_eq!(glyphs.len(), 4);
    assert!(g.map.keys().any(|k| k.text == "D" && k.bold));
    // 光标（块）在最后：第 5 列
    let last = built.instances.last().unwrap();
    assert_eq!(last.kind, KIND_RECT);
    assert_eq!(last.rect[0], 5.0 + 5.0 * 8.0);
    // 默认前景色
    assert_eq!(glyphs[0].color, argb(Theme::default().get(FG)));
    assert_ne!(argb(Theme::default().get(BG)), glyphs[0].color);
}

#[test]
fn scene_scrolls_by_pixels_and_reports_atlas_full() {
    let mut h = Host::new(10, 3);
    h.out("a\r\nb\r\nc\r\nd\r\ne");
    let mut st = state();
    st.sync(&h.tc);
    assert!(st.pinned);
    assert_eq!(st.top, 2.0);
    st.scroll_lines(&h.tc, -1.5);
    assert!(!st.pinned);
    assert_eq!(st.top, 0.5);
    let mut g = FakeGlyphs::default();
    let built = st.build(&h.tc, false, true, &mut g);
    // 半行偏移：第一行（"a"）在 y = 5 - 8
    let a = built.instances.iter().find(|i| i.kind == KIND_MASK).unwrap();
    assert_eq!(a.rect[1], 5.0 - 8.0);
    // 4 行露出来（a..d），e 在屏幕外
    assert_eq!(built.instances.iter().filter(|i| i.kind == KIND_MASK).count(), 4);
    // 新输出时不跟着走
    h.out("\r\nf");
    st.sync(&h.tc);
    assert_eq!(st.top, 0.5);
    st.scroll_lines(&h.tc, 100.0);
    assert!(st.pinned);
    let mut tiny = FakeGlyphs { limit: Some(1), ..Default::default() };
    assert!(st.build(&h.tc, false, true, &mut tiny).atlas_full);
}

#[test]
fn alt_screen_has_no_scrollback_and_clears_selection() {
    let mut h = Host::new(10, 3);
    h.out("a\r\nb\r\nc\r\nd\r\ne");
    let mut st = state();
    st.sync(&h.tc);
    st.select_all(&h.tc);
    h.out("\x1b[?1049h");
    st.sync(&h.tc);
    assert!(st.selection.is_none());
    assert_eq!(st.bounds(&h.tc), (2.0, 2.0));
    st.scroll_lines(&h.tc, -5.0);
    assert!(st.pinned);
}
