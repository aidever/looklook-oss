use std::time::Duration;

use crate::{
    AlacrittyCore, CellSpacer, ColorSpec, CoreConfig, CoreEvent, CursorShape, HistoryError,
    MouseEncoding, MouseProtocol, ProgressState, PromptMarkKind, RgbColor, ScreenSnapshot,
    StyleFlags, TerminalCore,
};

fn row_text(snap: &ScreenSnapshot, row: usize) -> String {
    let mut s = String::new();
    for cell in &snap.lines[row].cells {
        if cell.spacer != CellSpacer::None {
            continue;
        }
        s.push(cell.c);
        s.push_str(&cell.extra);
    }
    s.trim_end().to_owned()
}

fn cell_style(snap: &ScreenSnapshot, row: usize, col: usize) -> &crate::Style {
    let id = snap.lines[row].cells[col].style_id;
    &snap.style_table.styles[id as usize]
}

#[test]
fn replay_24bit_color_attributes_and_hyperlink() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 60,
        rows: 10,
        ..Default::default()
    })
    .unwrap();

    // Truecolor fg+bg, dim, reverse, undercurl with underline color, OSC 8 hyperlink.
    let stream = concat!(
        "\x1b[38;2;12;34;56;48;2;200;210;220mRGB\x1b[0m ",
        "\x1b[2mDIM\x1b[0m ",
        "\x1b[7mREV\x1b[0m ",
        "\x1b[4:3;58;2;255;0;128mWAVE\x1b[0m ",
        "\x1b]8;id=doc1;https://example.com/a;b\x07LINK\x1b]8;;\x07"
    );
    core.feed_bytes(stream.as_bytes());
    let snap = core.snapshot();

    assert_eq!(
        row_text(&snap, 0),
        "RGB DIM REV WAVE LINK"
    );

    let s_rgb = cell_style(&snap, 0, 0);
    assert_eq!(
        s_rgb.fg,
        ColorSpec::Rgb(RgbColor {
            r: 12,
            g: 34,
            b: 56
        })
    );
    assert_eq!(
        s_rgb.bg,
        ColorSpec::Rgb(RgbColor {
            r: 200,
            g: 210,
            b: 220
        })
    );

    let s_dim = cell_style(&snap, 0, 4);
    assert!(s_dim.flags.contains(StyleFlags::DIM));

    let s_rev = cell_style(&snap, 0, 8);
    assert!(s_rev.flags.contains(StyleFlags::INVERSE));

    let s_wave = cell_style(&snap, 0, 12);
    assert!(s_wave.flags.contains(StyleFlags::UNDERCURL));
    assert_eq!(
        s_wave.underline_color,
        Some(ColorSpec::Rgb(RgbColor {
            r: 255,
            g: 0,
            b: 128
        }))
    );

    let s_link = cell_style(&snap, 0, 17);
    let link_id = s_link.hyperlink_id.expect("hyperlink id present");
    let link = &snap.style_table.hyperlinks[link_id as usize];
    assert_eq!(link.id, "doc1");
    assert_eq!(link.uri, "https://example.com/a;b");

    // Verify SGR ANSI capture retains dim (\x1b[2m) and inverse (\x1b[7m) for chat::screen.
    let ansi = core.capture_screen_ansi();
    assert!(ansi.contains("\x1b[2mDIM"), "ansi: {ansi:?}");
    assert!(ansi.contains("\x1b[7mREV"), "ansi: {ansi:?}");
    assert!(ansi.contains("\x1b[38;2;12;34;56m"));
}

#[test]
fn replay_cjk_wide_chars_and_emoji_clusters() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 10,
        rows: 5,
        ..Default::default()
    })
    .unwrap();

    // 9 columns before CJK char '中' -> forces leading spacer & wrap to line 1.
    // Then emoji with combining variation selector + combining accent.
    let stream = concat!(
        "123456789中文",
        "\r\n",
        "👨‍👩‍👧a\u{0301}👍\u{FE0F}"
    );
    core.feed_bytes(stream.as_bytes());
    let snap = core.snapshot();

    // Line 0 ends with leading wide spacer at col 9 and wrapped == true.
    assert!(snap.lines[0].wrapped);
    assert_eq!(snap.lines[0].cells[9].spacer, CellSpacer::WideLeading);
    assert_eq!(snap.lines[0].cells[9].width, 0);

    // Line 1 has '中' (col 0-1) and '文' (col 2-3).
    assert_eq!(snap.lines[1].cells[0].c, '中');
    assert_eq!(snap.lines[1].cells[0].width, 2);
    assert_eq!(snap.lines[1].cells[1].spacer, CellSpacer::WideTrailing);
    assert_eq!(snap.lines[1].cells[2].c, '文');
    assert_eq!(snap.lines[1].cells[2].width, 2);

    // Plain text merges soft-wrapped row 0 + row 1 cleanly!
    let plain = core.capture_plain_text(false).unwrap();
    assert!(plain.starts_with("123456789中文\n"), "plain={plain:?}");

    // Line 2 checks ZWJ / combining mark / VS16 preservation.
    let row2 = row_text(&snap, 2);
    assert!(row2.contains("a\u{0301}"), "row2={row2:?}");
    assert!(row2.contains('\u{200D}'), "ZWJ preserved: {row2:?}");
    assert!(row2.contains("👍\u{FE0F}"), "VS16 preserved: {row2:?}");
}

#[test]
fn replay_vim_alt_screen_cursor_and_title() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 40,
        rows: 8,
        ..Default::default()
    })
    .unwrap();

    // Shell primary prompt before entering vim.
    core.feed_bytes(b"user@host:~$ vim notes.txt\r\n");

    // Realistic vim startup: ?1049h, title, ?1000h+?1006h SGR mouse, ?2004h, DECSCUSR bar cursor,
    // scroll region top..7, status line + buffer lines + tildes.
    let vim_bytes = concat!(
        "\x1b[?1049h",
        "\x1b]2;notes.txt - VIM\x07",
        "\x1b[?1000h\x1b[?1006h\x1b[?2004h",
        "\x1b[5 q",
        "\x1b[1;7r",
        "\x1b[2J\x1b[H",
        "hello vim\r\n",
        "\x1b[34m~\x1b[0m\r\n",
        "\x1b[8;1H\x1b[7m notes.txt [+]                1,1   All \x1b[0m",
        "\x1b[1;10H"
    );
    core.feed_bytes(vim_bytes.as_bytes());

    let snap = core.snapshot();
    assert!(snap.modes.alt_screen);
    assert!(snap.modes.bracketed_paste);
    assert_eq!(snap.modes.mouse_protocol, MouseProtocol::Normal);
    assert_eq!(snap.modes.mouse_encoding, MouseEncoding::Sgr);
    assert_eq!(snap.title.as_deref(), Some("notes.txt - VIM"));
    assert_eq!(snap.cursor.shape, CursorShape::Beam);
    assert_eq!((snap.cursor.row, snap.cursor.col), (0, 9));
    assert_eq!(row_text(&snap, 0), "hello vim");
    assert_eq!(row_text(&snap, 1), "~");
    assert!(row_text(&snap, 7).starts_with(" notes.txt [+]"));
    // Alt screen never appends to primary scrollback.
    assert_eq!(core.scrollback_info().3, 0);

    // Leaving vim restores primary screen with earlier prompt intact.
    core.feed_bytes(b"\x1b[?1049l\x1b[?1000l\x1b[?1006l\x1b[?2004l");
    let after = core.snapshot();
    assert!(!after.modes.alt_screen);
    assert_eq!(after.modes.mouse_protocol, MouseProtocol::None);
    assert_eq!(row_text(&after, 0), "user@host:~$ vim notes.txt");
}

#[test]
fn replay_htop_decstbm_scroll_region_and_clear() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 50,
        rows: 8,
        ..Default::default()
    })
    .unwrap();

    // First emit 5 primary lines then clear screen cleanly, or test htop-like fixed header + footer
    // with scroll region restricted to rows 3..=6 (1-based) scrolling inside without adding
    // intermediate rows to scrollback!
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice(b"\x1b[1;1H\x1b[32mCPU[||||      25%]\x1b[0m");
    bytes.extend_from_slice(b"\x1b[2;1H\x1b[36mMem[|||       30%]\x1b[0m");
    bytes.extend_from_slice(b"\x1b[8;1H\x1b[7mF1Help  F10Quit\x1b[0m");
    core.feed_bytes(&bytes);
    let hist_before_decstbm = core.scrollback_info().3;

    // Restricted DECSTBM rows 3..6 -> lines inside this region scroll without pushing into scrollback.
    let mut region_bytes = Vec::new();
    region_bytes.extend_from_slice(b"\x1b[3;6r\x1b[3;1H");
    for i in 1..=10 {
        region_bytes.extend_from_slice(format!("proc-{i:02}\r\n").as_bytes());
    }
    core.feed_bytes(&region_bytes);

    let snap = core.snapshot();
    assert_eq!(row_text(&snap, 0), "CPU[||||      25%]");
    assert_eq!(row_text(&snap, 1), "Mem[|||       30%]");
    assert_eq!(row_text(&snap, 7), "F1Help  F10Quit");
    // Scroll region 3..6 keeps newest rows in region.
    let mid: Vec<String> = (2..6).map(|r| row_text(&snap, r)).collect();
    assert!(mid.contains(&"proc-09".to_string()), "mid={mid:?}");
    assert!(mid.contains(&"proc-10".to_string()), "mid={mid:?}");
    assert_eq!(
        core.scrollback_info().3,
        hist_before_decstbm,
        "DECSTBM top>1 must not pollute scrollback: {:?}",
        core.tail_history(5)
    );

    // Now reset scroll region to full screen and trigger scrollback + CSI 3 J epoch bump.
    core.feed_bytes(b"\x1b[r\x1b[8;1H");
    for i in 1..=20 {
        core.feed_bytes(format!("hist-{i:02}\r\n").as_bytes());
    }
    let (epoch0, oldest0, next0, len0) = core.scrollback_info();
    assert_eq!(epoch0, 0);
    assert_eq!(oldest0, 0);
    assert!(len0 >= 12 && next0 == len0 as u64, "len={len0}, next={next0}");

    // Take incremental scrollback and verify epoch mismatch after CSI 3 J.
    let (ep, delta_rows) = core.take_new_scrollback();
    assert_eq!(ep, 0);
    assert_eq!(delta_rows.len(), len0);
    assert_eq!(delta_rows[0].seq, 0);

    core.feed_bytes(b"\x1b[3J");
    let (epoch1, oldest1, next1, len1) = core.scrollback_info();
    assert_eq!(epoch1, 1);
    assert_eq!(oldest1, 0);
    assert_eq!(next1, 0);
    assert_eq!(len1, 0);

    let err = core.history_slice(0, 0, 10).unwrap_err();
    assert!(matches!(
        err,
        HistoryError::EpochMismatch {
            current_epoch: 1,
            ..
        }
    ));
}

#[test]
fn scrollback_cold_compression_and_ring_eviction() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 40,
        rows: 4,
        scrollback_limit: 500,
    })
    .unwrap();

    // Emit > 800 lines so > 500 are written and hot cache (256 lines) spills into cold zstd rows.
    let mut payload = Vec::new();
    for i in 0..800 {
        payload.extend_from_slice(format!("line-{i:04}-abcdef\r\n").as_bytes());
    }
    core.feed_bytes(&payload);

    let (epoch, oldest, next, len) = core.scrollback_info();
    assert_eq!(epoch, 0);
    assert_eq!(len, 500);
    assert_eq!(next - oldest, 500);

    // Fetch a slice spanning cold storage and verify exact line numbers and text.
    let cold_slice = core.history_slice(0, oldest, 10).unwrap();
    assert_eq!(cold_slice.rows.len(), 10);
    assert_eq!(cold_slice.rows[0].seq, oldest);
    assert_eq!(
        cold_slice.rows[0].plain_text(),
        format!("line-{oldest:04}-abcdef")
    );

    // Fetch slice spanning the tail (hot cache).
    let tail_slice = core.tail_history(5).unwrap();
    assert_eq!(tail_slice.rows.len(), 5);
    assert_eq!(tail_slice.rows[4].seq, next - 1);
}

#[test]
fn sync_update_2026_and_osc_events() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 40,
        rows: 5,
        ..Default::default()
    })
    .unwrap();

    // OSC 7 split across chunks, OSC 9;4, OSC 133, and BSU/ESU (?2026h / ?2026l).
    core.feed_bytes(b"\x1b]7;file://host/home/dev%20space");
    core.feed_bytes(b"/proj\x07\x1b]9;4;1;65\x07\x1b]133;A\x07\x1b]133;D;0\x07");

    let events = core.take_events();
    assert!(events.contains(&CoreEvent::CwdChanged("/home/dev space/proj".into())));
    assert!(events.contains(&CoreEvent::Progress(ProgressState::Normal(65))));
    assert!(events.contains(&CoreEvent::PromptMark(PromptMarkKind::PromptStart)));
    assert!(events.contains(&CoreEvent::PromptMark(PromptMarkKind::CommandFinished(Some(0)))));
    assert_eq!(core.cwd(), Some("/home/dev space/proj"));

    // Inside DEC 2026 sync update, screen should stay unmodified until ESU or timeout flush.
    core.feed_bytes(b"before\r\n\x1b[?2026hhidden_during_sync");
    assert!(core.in_sync_update());
    assert_eq!(row_text(&core.snapshot(), 1), "");

    std::thread::sleep(Duration::from_millis(25));
    assert!(core.check_sync_timeout(Duration::from_millis(10)));
    assert!(!core.in_sync_update());
    assert_eq!(row_text(&core.snapshot(), 1), "hidden_during_sync");
}

#[test]
fn serialize_and_restore_handover_primary_and_alt_screen() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 50,
        rows: 10,
        scrollback_limit: 500,
    })
    .unwrap();

    for i in 0..30 {
        core.feed_bytes(format!("shell-line-{i:02}\r\n").as_bytes());
    }
    core.feed_bytes(b"\x1b]7;file:///tmp/work\x07\x1b]2;nvim\x07");
    core.feed_bytes(b"shell-prompt$ ");
    // Enter alt screen mid-session (e.g., nvim running) and upgrade happens now.
    core.feed_bytes(b"\x1b[?1049h\x1b[?2004h\x1b[H\x1b[38;2;10;200;30mALT TEXT\x1b[0m\x1b[2;5H");

    let blob = core.serialize_state().unwrap();
    let mut restored = AlacrittyCore::from_serialized(&blob).unwrap();

    assert_eq!(restored.size(), (50, 10));
    assert_eq!(restored.title(), Some("nvim"));
    assert_eq!(restored.cwd(), Some("/tmp/work"));
    let alt_snap = restored.snapshot();
    assert!(alt_snap.modes.alt_screen);
    assert!(alt_snap.modes.bracketed_paste);
    assert_eq!((alt_snap.cursor.row, alt_snap.cursor.col), (1, 4));
    assert_eq!(row_text(&alt_snap, 0), "ALT TEXT");
    assert_eq!(
        cell_style(&alt_snap, 0, 0).fg,
        ColorSpec::Rgb(RgbColor {
            r: 10,
            g: 200,
            b: 30
        })
    );

    // Exiting alt screen on restored core reveals the intact primary grid AND scrollback!
    restored.feed_bytes(b"\x1b[?1049l");
    let prim_snap = restored.snapshot();
    assert!(!prim_snap.modes.alt_screen);
    let plain = restored.capture_plain_text(true).unwrap();
    assert!(plain.contains("shell-line-00"), "plain={plain}");
    assert!(plain.contains("shell-prompt$"), "plain={plain}");

    // Original scrollback epoch & next_seq survived intact.
    assert_eq!(restored.scrollback_info(), core.scrollback_info());
}

#[test]
fn resize_reflow_and_height_change_scrollback_consistency() {
    let mut core = AlacrittyCore::new(CoreConfig {
        cols: 20,
        rows: 5,
        scrollback_limit: 500,
    })
    .unwrap();

    for i in 1..=12 {
        core.feed_bytes(format!("item-{i:02}\r\n").as_bytes());
    }
    let before_plain = core.capture_plain_text(true).unwrap();
    assert!(before_plain.contains("item-01"));
    assert!(before_plain.contains("item-12"));

    // Grow taller: pulls lines from grid history back into viewport without duplicating in scrollback.
    core.resize(20, 8);
    let grown_plain = core.capture_plain_text(true).unwrap();
    let count_08 = grown_plain.matches("item-08").count();
    assert_eq!(
        count_08, 1,
        "item-08 must not be duplicated between scrollback and viewport: {grown_plain:?}"
    );
}
