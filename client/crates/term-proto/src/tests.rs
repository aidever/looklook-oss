use proptest::prelude::*;

use crate::messages::{ClientMsg, MouseEventKind, ServerMsg, ViewState};
use crate::types::{
    CellData, CellSpacer, ColorSpec, CursorShape, CursorState, HyperlinkData, MouseEncoding,
    MouseProtocol, NamedColorCode, RgbColor, ScreenRow, ScreenSnapshot, Style, StyleFlags,
    StyleTableBuilder, TermModes,
};
use crate::{
    ClientScreen, DeltaEncoder, ZstdStreamDecoder, ZstdStreamEncoder, RuzstdStreamDecoder,
    decode_client_msg, encode_client_msg,
};

fn build_style(variant: u8) -> (Style, Option<HyperlinkData>) {
    match variant % 8 {
        0 => (Style::default(), None),
        1 => (
            Style {
                fg: ColorSpec::Named(NamedColorCode::Green),
                bg: ColorSpec::Named(NamedColorCode::Black),
                underline_color: None,
                flags: StyleFlags::BOLD,
                hyperlink_id: None,
            },
            None,
        ),
        2 => (
            Style {
                fg: ColorSpec::Rgb(RgbColor {
                    r: 255,
                    g: 120,
                    b: 40,
                }),
                bg: ColorSpec::Indexed(236),
                underline_color: Some(ColorSpec::Rgb(RgbColor {
                    r: 0,
                    g: 200,
                    b: 255,
                })),
                flags: StyleFlags::DIM | StyleFlags::UNDERCURL,
                hyperlink_id: None,
            },
            None,
        ),
        3 => (
            Style {
                fg: ColorSpec::Named(NamedColorCode::Cyan),
                bg: ColorSpec::Named(NamedColorCode::Background),
                underline_color: None,
                flags: StyleFlags::INVERSE,
                hyperlink_id: Some(0),
            },
            Some(HyperlinkData {
                id: "L1".into(),
                uri: "https://looklook.dev/1".into(),
            }),
        ),
        4 => (
            Style {
                fg: ColorSpec::Rgb(RgbColor {
                    r: variant,
                    g: variant.wrapping_mul(3),
                    b: 180,
                }),
                bg: ColorSpec::Named(NamedColorCode::Blue),
                underline_color: None,
                flags: StyleFlags::ITALIC | StyleFlags::STRIKEOUT,
                hyperlink_id: None,
            },
            None,
        ),
        _ => (
            Style {
                fg: ColorSpec::Indexed(variant),
                bg: ColorSpec::Named(NamedColorCode::Background),
                underline_color: None,
                flags: StyleFlags::UNDERLINE,
                hyperlink_id: None,
            },
            None,
        ),
    }
}

#[derive(Clone, Debug)]
enum GridMutation {
    WriteCell {
        row: usize,
        col: usize,
        ch: char,
        style_var: u8,
        wide: bool,
    },
    ScrollUp {
        top: usize,
        bottom: usize,
        lines: usize,
    },
    ScrollDown {
        top: usize,
        bottom: usize,
        lines: usize,
    },
    SetTitle(Option<String>),
    SetCursor {
        row: u16,
        col: u16,
        shape_idx: u8,
        visible: bool,
    },
    ToggleAltScreen(bool),
    AdvanceScrollback {
        lines: u64,
        clear_epoch: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
}

fn mutation_strategy() -> impl Strategy<Value = GridMutation> {
    prop_oneof![
        (
            0usize..24,
            0usize..80,
            prop_oneof![Just('a'), Just('Z'), Just('中'), Just(' '), Just('!')],
            any::<u8>(),
            any::<bool>(),
        )
            .prop_map(|(row, col, ch, style_var, wide)| GridMutation::WriteCell {
                row,
                col,
                ch,
                style_var,
                wide,
            }),
        (0usize..6, 10usize..24, 1usize..5).prop_map(|(top, bottom, lines)| {
            GridMutation::ScrollUp { top, bottom, lines }
        }),
        (0usize..6, 10usize..24, 1usize..4).prop_map(|(top, bottom, lines)| {
            GridMutation::ScrollDown { top, bottom, lines }
        }),
        prop_oneof![
            Just(None),
            Just(Some("bash".to_string())),
            Just(Some("vim - test".to_string())),
        ]
        .prop_map(GridMutation::SetTitle),
        (0u16..24, 0u16..80, 0u8..5, any::<bool>()).prop_map(|(row, col, shape_idx, visible)| {
            GridMutation::SetCursor {
                row,
                col,
                shape_idx,
                visible,
            }
        }),
        any::<bool>().prop_map(GridMutation::ToggleAltScreen),
        (1u64..15, any::<bool>()).prop_map(|(lines, clear_epoch)| {
            GridMutation::AdvanceScrollback { lines, clear_epoch }
        }),
        (20u16..60, 6u16..18).prop_map(|(cols, rows)| GridMutation::Resize { cols, rows }),
    ]
}

type MockCell = (char, String, u8, CellSpacer, Style, Option<HyperlinkData>);

struct MockTermState {
    cols: u16,
    rows: u16,
    cells: Vec<Vec<MockCell>>,
    wrapped: Vec<bool>,
    cursor: CursorState,
    modes: TermModes,
    title: Option<String>,
    epoch: u64,
    next_seq: u64,
}

impl MockTermState {
    fn new(cols: u16, rows: u16) -> Self {
        let row_vec = vec![
            (' ', String::new(), 1, CellSpacer::None, Style::default(), None);
            cols as usize
        ];
        Self {
            cols,
            rows,
            cells: vec![row_vec; rows as usize],
            wrapped: vec![false; rows as usize],
            cursor: CursorState::default(),
            modes: TermModes::default(),
            title: None,
            epoch: 0,
            next_seq: 0,
        }
    }

    fn apply(&mut self, m: GridMutation) {
        match m {
            GridMutation::WriteCell {
                row,
                col,
                ch,
                style_var,
                wide,
            } => {
                let r = row % self.rows as usize;
                let c = col % self.cols as usize;
                let (mut st, link) = build_style(style_var);
                if link.is_none() {
                    st.hyperlink_id = None;
                }
                let blank = (' ', String::new(), 1, CellSpacer::None, Style::default(), None);
                // Clear any existing wide neighbor at target before overwriting.
                if self.cells[r][c].3 == CellSpacer::WideTrailing && c > 0 {
                    self.cells[r][c - 1] = blank.clone();
                } else if self.cells[r][c].2 == 2 && c + 1 < self.cols as usize {
                    self.cells[r][c + 1] = blank.clone();
                }

                if (wide || ch == '中') && c + 1 < self.cols as usize {
                    self.cells[r][c] = (ch, String::new(), 2, CellSpacer::None, st.clone(), link.clone());
                    self.cells[r][c + 1] = (' ', String::new(), 0, CellSpacer::WideTrailing, st, link);
                } else {
                    let narrow_ch = if ch == '中' { 'X' } else { ch };
                    self.cells[r][c] = (narrow_ch, String::new(), 1, CellSpacer::None, st, link);
                }
            }
            GridMutation::ScrollUp { top, bottom, lines } => {
                let t = top.min(self.rows as usize);
                let b = bottom.min(self.rows as usize);
                if t + 1 < b {
                    let count = lines.min(b - t);
                    for r in t..(b - count) {
                        self.cells.swap(r, r + count);
                        self.wrapped.swap(r, r + count);
                    }
                    let blank_row = vec![
                        (' ', String::new(), 1, CellSpacer::None, Style::default(), None);
                        self.cols as usize
                    ];
                    for r in (b - count)..b {
                        self.cells[r] = blank_row.clone();
                        self.wrapped[r] = false;
                    }
                }
            }
            GridMutation::ScrollDown { top, bottom, lines } => {
                let t = top.min(self.rows as usize);
                let b = bottom.min(self.rows as usize);
                if t + 1 < b {
                    let count = lines.min(b - t);
                    for r in ((t + count)..b).rev() {
                        self.cells.swap(r, r - count);
                        self.wrapped.swap(r, r - count);
                    }
                    let blank_row = vec![
                        (' ', String::new(), 1, CellSpacer::None, Style::default(), None);
                        self.cols as usize
                    ];
                    for r in t..(t + count) {
                        self.cells[r] = blank_row.clone();
                        self.wrapped[r] = false;
                    }
                }
            }
            GridMutation::SetTitle(t) => self.title = t,
            GridMutation::SetCursor {
                row,
                col,
                shape_idx,
                visible,
            } => {
                let shape = match shape_idx {
                    0 => CursorShape::Block,
                    1 => CursorShape::Underline,
                    2 => CursorShape::Beam,
                    3 => CursorShape::HollowBlock,
                    _ => CursorShape::Hidden,
                };
                self.cursor = CursorState {
                    row: row % self.rows,
                    col: col % self.cols,
                    shape,
                    blinking: shape_idx % 2 == 1,
                    visible,
                };
            }
            GridMutation::ToggleAltScreen(alt) => {
                self.modes.alt_screen = alt;
                self.modes.mouse_protocol = if alt {
                    MouseProtocol::Drag
                } else {
                    MouseProtocol::None
                };
                self.modes.mouse_encoding = if alt {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::X10
                };
            }
            GridMutation::AdvanceScrollback { lines, clear_epoch } => {
                if clear_epoch {
                    self.epoch += 1;
                    self.next_seq = 0;
                } else {
                    self.next_seq += lines;
                }
            }
            GridMutation::Resize { cols, rows } => {
                self.cols = cols;
                self.rows = rows;
                let blank_cell =
                    (' ', String::new(), 1, CellSpacer::None, Style::default(), None);
                self.cells.resize_with(rows as usize, || {
                    vec![blank_cell.clone(); cols as usize]
                });
                for r in &mut self.cells {
                    r.resize_with(cols as usize, || blank_cell.clone());
                }
                self.wrapped.resize(rows as usize, false);
                self.cursor.row = self.cursor.row.min(rows - 1);
                self.cursor.col = self.cursor.col.min(cols - 1);
            }
        }
    }

    fn snapshot(&self) -> ScreenSnapshot {
        let mut builder = StyleTableBuilder::new();
        let mut lines = Vec::with_capacity(self.rows as usize);
        for (r, row_cells) in self.cells.iter().enumerate() {
            let mut cells = Vec::with_capacity(self.cols as usize);
            for (ch, extra, width, spacer, mut style, link) in row_cells.iter().cloned() {
                let link_id = link.map(|l| builder.intern_hyperlink(l));
                style.hyperlink_id = link_id;
                let style_id = builder.intern_style(style);
                cells.push(CellData {
                    c: ch,
                    extra,
                    width,
                    spacer,
                    style_id,
                });
            }
            lines.push(ScreenRow {
                cells,
                wrapped: self.wrapped[r],
            });
        }
        ScreenSnapshot {
            cols: self.cols,
            rows: self.rows,
            lines,
            style_table: builder.into_table(),
            cursor: self.cursor,
            modes: self.modes,
            title: self.title.clone(),
            cwd: None,
            scrollback_epoch: self.epoch,
            scrollback_oldest_seq: 0,
            scrollback_next_seq: self.next_seq,
            palette_overrides: Vec::new(),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn delta_and_zstd_stream_roundtrip_exact_screen_state(
        mutations in prop::collection::vec(mutation_strategy(), 1..40)
    ) {
        let mut state = MockTermState::new(40, 14);
        let mut delta_encoder = DeltaEncoder::new();
        let mut wire_encoder = ZstdStreamEncoder::new().unwrap();
        let mut wire_decoder = ZstdStreamDecoder::new().unwrap();
        let mut client_screen = ClientScreen::new();

        // Initial frame must always produce a Snapshot.
        let init_snap = state.snapshot();
        let init_msg = delta_encoder.encode_frame(&init_snap, 0);
        assert!(matches!(init_msg, ServerMsg::Snapshot { .. }));
        let bytes = wire_encoder.encode_server_msg(&init_msg).unwrap();
        assert!(!bytes.is_empty());
        let decoded = wire_decoder.feed(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        client_screen.apply_server_msg(&decoded[0]).unwrap();
        assert!(client_screen.snapshot().unwrap().semantically_eq(&init_snap));

        for (step, mutation) in mutations.into_iter().enumerate() {
            state.apply(mutation);
            let snap = state.snapshot();
            let msg = delta_encoder.encode_frame(&snap, step as u64 + 1);
            let compressed = wire_encoder.encode_server_msg(&msg).unwrap();
            assert!(!compressed.is_empty(), "every flushed message must emit bytes immediately");
            let msgs = wire_decoder.feed(&compressed).unwrap();
            assert_eq!(msgs.len(), 1);
            client_screen.apply_server_msg(&msgs[0]).unwrap();

            let reconstructed = client_screen.snapshot().unwrap();
            prop_assert!(
                reconstructed.semantically_eq(&snap),
                "step {step}: client screen diverged from server state"
            );
            prop_assert_eq!(client_screen.acked_input_seq(), step as u64 + 1);
        }
    }
}

#[test]
fn scroll_detection_emits_compact_delta_and_ruzstd_decodes_stream() {
    let mut state = MockTermState::new(30, 10);
    // Populate distinct non-blank rows 0..10.
    for r in 0..10 {
        for (c, ch) in format!("row-{r:02}-content").chars().enumerate() {
            state.apply(GridMutation::WriteCell {
                row: r,
                col: c,
                ch,
                style_var: (r as u8) % 5,
                wide: false,
            });
        }
    }

    let mut enc = DeltaEncoder::new();
    let mut zenc = ZstdStreamEncoder::new().unwrap();
    let mut ruz = RuzstdStreamDecoder::new();
    let mut client = ClientScreen::new();

    let s0 = state.snapshot();
    let m0 = enc.encode_frame(&s0, 1);
    let mut stream_bytes = Vec::new();
    stream_bytes.extend_from_slice(&zenc.encode_server_msg(&m0).unwrap());

    // Now scroll the whole viewport up by 2 rows and write 1 new line at bottom.
    state.apply(GridMutation::ScrollUp {
        top: 0,
        bottom: 10,
        lines: 2,
    });
    for (c, ch) in "new-bottom-line".chars().enumerate() {
        state.apply(GridMutation::WriteCell {
            row: 9,
            col: c,
            ch,
            style_var: 1,
            wide: false,
        });
    }
    let s1 = state.snapshot();
    let m1 = enc.encode_frame(&s1, 2);

    match &m1 {
        ServerMsg::Delta(delta) => {
            assert_eq!(delta.scroll_ops.len(), 1);
            assert_eq!(delta.scroll_ops[0].delta, 2);
            // Rows 0..8 matched via scroll shift! Only cleared bottom region rows patched!
            assert!(
                delta.changed_rows.len() <= 2,
                "changed rows should only touch newly written bottom rows, got {}",
                delta.changed_rows.len()
            );
        }
        other => panic!("expected Delta with scroll_ops, got {other:?}"),
    }

    stream_bytes.extend_from_slice(&zenc.encode_server_msg(&m1).unwrap());
    stream_bytes.extend_from_slice(&zenc.finish().unwrap());

    let decoded = ruz.feed(&stream_bytes).unwrap();
    assert_eq!(decoded.len(), 2);
    client.apply_server_msg(&decoded[0]).unwrap();
    client.apply_server_msg(&decoded[1]).unwrap();
    assert!(client.snapshot().unwrap().semantically_eq(&s1));
}

#[test]
fn client_message_variants_roundtrip() {
    let msgs = vec![
        ClientMsg::Hello {
            protocol: crate::PROTOCOL_VERSION.into(),
            cols: 120,
            rows: 36,
            client_id: Some("tab-1".into()),
            last_frame: Some(42),
            last_input_seq: 9,
        },
        ClientMsg::Input {
            seq: 10,
            data: b"ls -la\r".to_vec(),
        },
        ClientMsg::Resize { cols: 100, rows: 30 },
        ClientMsg::Ack { frame: 43 },
        ClientMsg::View {
            state: ViewState::Background,
        },
        ClientMsg::Mouse {
            kind: MouseEventKind::Down,
            button: 0,
            col: 12,
            row: 5,
            modifiers: 0,
            raw: Vec::new(),
        },
        ClientMsg::HistoryReq {
            request_id: 7,
            epoch: 2,
            start_seq: 100,
            count: 50,
        },
    ];
    for m in msgs {
        let bytes = encode_client_msg(&m).unwrap();
        let back = decode_client_msg(&bytes).unwrap();
        assert_eq!(back, m);
    }
}
