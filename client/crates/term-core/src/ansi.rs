//! Rendering screen and scrollback rows into SGR ANSI text and plain text.

use crate::types::{CellData, CellSpacer, ScreenSnapshot, ScrollbackRow};
use term_proto::render_row_ansi;

/// Render visible screen cells with SGR attributes (`\x1b[2m`, `\x1b[7m`, colors, `\x1b[0m`)
/// compatible with `tmux capture-pane -e -p` and `chat::screen`.
pub fn render_screen_ansi(snap: &ScreenSnapshot) -> String {
    let mut out = String::new();
    for (idx, row) in snap.lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        render_row_ansi(&row.cells, &snap.style_table.styles, &mut out);
    }
    out
}

/// Render historical rows followed by screen rows with SGR escapes.
pub fn render_history_and_screen_ansi(
    history: &[ScrollbackRow],
    snap: &ScreenSnapshot,
    tail_lines: Option<usize>,
) -> String {
    let total = history.len() + snap.lines.len();
    let skip = tail_lines.map_or(0, |n| total.saturating_sub(n));
    let mut out = String::new();
    let mut line_idx = 0usize;
    let mut wrote_any = false;

    for h in history {
        if line_idx >= skip {
            if wrote_any {
                out.push('\n');
            }
            render_row_ansi(&h.cells, &h.styles, &mut out);
            wrote_any = true;
        }
        line_idx += 1;
    }

    for r in &snap.lines {
        if line_idx >= skip {
            if wrote_any {
                out.push('\n');
            }
            render_row_ansi(&r.cells, &snap.style_table.styles, &mut out);
            wrote_any = true;
        }
        line_idx += 1;
    }

    out
}

/// Render plain UTF-8 text joining soft-wrapped lines (`wrapped == true`).
pub fn render_plain_text(history: &[ScrollbackRow], snap: &ScreenSnapshot) -> String {
    let mut out = String::new();
    for h in history {
        push_row_plain(&h.cells, h.wrapped, &mut out);
    }
    for (i, r) in snap.lines.iter().enumerate() {
        let is_last = i + 1 == snap.lines.len();
        push_row_plain(&r.cells, r.wrapped && !is_last, &mut out);
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn push_row_plain(cells: &[CellData], wrapped: bool, out: &mut String) {
    let end = if wrapped {
        cells.len()
    } else {
        cells
            .iter()
            .rposition(|c| !c.is_blank_default() && c.spacer != CellSpacer::WideLeading)
            .map_or(0, |i| i + 1)
    };
    for cell in cells.iter().take(end) {
        if cell.spacer != CellSpacer::None {
            continue;
        }
        out.push(cell.c);
        out.push_str(&cell.extra);
    }
    if !wrapped {
        out.push('\n');
    }
}
