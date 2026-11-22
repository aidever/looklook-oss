//! Incremental screen delta computation (`DeltaEncoder`) and client reconstruction (`ClientScreen`).

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::messages::{CellSpan, RowPatch, ScreenDelta, ScrollRegionShift, ServerMsg};
use crate::types::{CellData, ScreenRow, ScreenSnapshot, Style, StyleTableBuilder};

/// Re-emit a full snapshot if the accumulated connection style table grows past this threshold
/// to prevent unbounded palette growth over long-lived sessions.
const MAX_CUMULATIVE_STYLES: usize = 4096;
const MAX_CUMULATIVE_LINKS: usize = 2048;

/// Errors while applying a [`ScreenDelta`] to [`ClientScreen`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    NoBaseSnapshot,
    FrameMismatch { expected: u64, got_base: u64 },
    DimensionMismatch { screen: (u16, u16), delta: (u16, u16) },
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBaseSnapshot => write!(f, "received delta before initial snapshot"),
            Self::FrameMismatch { expected, got_base } => {
                write!(f, "delta base frame mismatch (expected {expected}, got {got_base})")
            }
            Self::DimensionMismatch { screen, delta } => {
                write!(f, "delta dimensions {delta:?} do not match client screen {screen:?}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

/// Server-side encoder transforming consecutive [`ScreenSnapshot`]s into minimal [`ServerMsg::Snapshot`]
/// or [`ServerMsg::Delta`] messages while maintaining a reusable connection style table.
#[derive(Debug)]
pub struct DeltaEncoder {
    frame: u64,
    prev: Option<ScreenSnapshot>,
    styles: StyleTableBuilder,
}

impl Default for DeltaEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DeltaEncoder {
    pub fn new() -> Self {
        Self {
            frame: 0,
            prev: None,
            styles: StyleTableBuilder::new(),
        }
    }

    pub fn current_frame(&self) -> u64 {
        self.frame
    }

    pub fn force_full_snapshot(&mut self) {
        self.prev = None;
    }

    /// Encode the next viewport state into a `Snapshot` (on initial frame, resize, or palette
    /// rollover) or a `Delta` relative to the previous frame.
    pub fn encode_frame(&mut self, current: &ScreenSnapshot, acked_input_seq: u64) -> ServerMsg {
        self.frame = self.frame.saturating_add(1);
        let frame = self.frame;

        let needs_snapshot = match &self.prev {
            None => true,
            Some(prev) => {
                prev.cols != current.cols
                    || prev.rows != current.rows
                    || self.styles.style_count() > MAX_CUMULATIVE_STYLES
                    || self.styles.hyperlink_count() > MAX_CUMULATIVE_LINKS
            }
        };

        if needs_snapshot {
            let mut builder = StyleTableBuilder::new();
            let canonical = canonicalize_snapshot(current, &mut builder);
            self.styles = builder;
            self.prev = Some(canonical.clone());
            return ServerMsg::Snapshot {
                frame,
                acked_input_seq,
                screen: canonical,
            };
        }

        let prev = self.prev.as_ref().expect("checked Some above");
        let prev_style_count = self.styles.style_count();
        let prev_link_count = self.styles.hyperlink_count();
        let canonical = canonicalize_snapshot(current, &mut self.styles);

        let new_styles = self.styles.table().styles[prev_style_count..].to_vec();
        let new_hyperlinks = self.styles.table().hyperlinks[prev_link_count..].to_vec();

        let scroll_ops = detect_scroll_shifts(&prev.lines, &canonical.lines);
        let mut simulated = prev.lines.clone();
        for op in &scroll_ops {
            apply_scroll_shift(&mut simulated, *op, canonical.cols as usize);
        }

        let mut changed_rows = Vec::new();
        for (row_idx, (sim_row, cur_row)) in
            simulated.iter().zip(canonical.lines.iter()).enumerate()
        {
            if let Some(patch) = diff_row(row_idx as u16, sim_row, cur_row) {
                changed_rows.push(patch);
            }
        }

        let delta = ScreenDelta {
            frame,
            base_frame: frame - 1,
            acked_input_seq,
            cols: canonical.cols,
            rows: canonical.rows,
            scroll_ops,
            new_styles,
            new_hyperlinks,
            changed_rows,
            cursor: (prev.cursor != canonical.cursor).then_some(canonical.cursor),
            modes: (prev.modes != canonical.modes).then_some(canonical.modes),
            title: (prev.title != canonical.title).then(|| canonical.title.clone()),
            cwd: (prev.cwd != canonical.cwd).then(|| canonical.cwd.clone()),
            scrollback_epoch: canonical.scrollback_epoch,
            scrollback_oldest_seq: canonical.scrollback_oldest_seq,
            scrollback_next_seq: canonical.scrollback_next_seq,
            palette_overrides: (prev.palette_overrides != canonical.palette_overrides)
                .then(|| canonical.palette_overrides.clone()),
        };

        self.prev = Some(canonical);
        ServerMsg::Delta(delta)
    }
}

/// Client-side state machine reconstructing exact [`ScreenSnapshot`]s from `Snapshot` + `Delta`s.
#[derive(Clone, Debug, Default)]
pub struct ClientScreen {
    frame: u64,
    acked_input_seq: u64,
    snapshot: Option<ScreenSnapshot>,
}

impl ClientScreen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn acked_input_seq(&self) -> u64 {
        self.acked_input_seq
    }

    pub fn snapshot(&self) -> Option<&ScreenSnapshot> {
        self.snapshot.as_ref()
    }

    /// Apply an incoming `Snapshot` or `Delta`. Ignores unrelated message types and returns
    /// `Ok(true)` if screen state changed, `Ok(false)` otherwise.
    pub fn apply_server_msg(&mut self, msg: &ServerMsg) -> Result<bool, ApplyError> {
        match msg {
            ServerMsg::Snapshot {
                frame,
                acked_input_seq,
                screen,
            } => {
                self.frame = *frame;
                self.acked_input_seq = *acked_input_seq;
                self.snapshot = Some(screen.clone());
                Ok(true)
            }
            ServerMsg::Delta(delta) => {
                self.apply_delta(delta)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn apply_delta(&mut self, delta: &ScreenDelta) -> Result<(), ApplyError> {
        let Some(screen) = self.snapshot.as_mut() else {
            return Err(ApplyError::NoBaseSnapshot);
        };
        if delta.base_frame != self.frame {
            return Err(ApplyError::FrameMismatch {
                expected: self.frame,
                got_base: delta.base_frame,
            });
        }
        if (screen.cols, screen.rows) != (delta.cols, delta.rows) {
            return Err(ApplyError::DimensionMismatch {
                screen: (screen.cols, screen.rows),
                delta: (delta.cols, delta.rows),
            });
        }

        screen
            .style_table
            .styles
            .extend(delta.new_styles.iter().cloned());
        screen
            .style_table
            .hyperlinks
            .extend(delta.new_hyperlinks.iter().cloned());

        for op in &delta.scroll_ops {
            apply_scroll_shift(&mut screen.lines, *op, delta.cols as usize);
        }

        for patch in &delta.changed_rows {
            if let Some(target_row) = screen.lines.get_mut(patch.row as usize) {
                target_row.wrapped = patch.wrapped;
                if target_row.cells.len() < delta.cols as usize {
                    target_row
                        .cells
                        .resize_with(delta.cols as usize, CellData::default);
                }
                for span in &patch.spans {
                    let start = span.start_col as usize;
                    for (offset, cell) in span.cells.iter().enumerate() {
                        if let Some(dst) = target_row.cells.get_mut(start + offset) {
                            *dst = cell.clone();
                        }
                    }
                }
            }
        }

        if let Some(cursor) = delta.cursor {
            screen.cursor = cursor;
        }
        if let Some(modes) = delta.modes {
            screen.modes = modes;
        }
        if let Some(title) = &delta.title {
            screen.title = title.clone();
        }
        if let Some(cwd) = &delta.cwd {
            screen.cwd = cwd.clone();
        }
        if let Some(palette) = &delta.palette_overrides {
            screen.palette_overrides = palette.clone();
        }
        screen.scrollback_epoch = delta.scrollback_epoch;
        screen.scrollback_oldest_seq = delta.scrollback_oldest_seq;
        screen.scrollback_next_seq = delta.scrollback_next_seq;

        self.frame = delta.frame;
        self.acked_input_seq = delta.acked_input_seq;
        Ok(())
    }
}

fn canonicalize_snapshot(
    src: &ScreenSnapshot,
    builder: &mut StyleTableBuilder,
) -> ScreenSnapshot {
    let mut style_remap = Vec::with_capacity(src.style_table.styles.len());
    for style in &src.style_table.styles {
        let remapped_link = style
            .hyperlink_id
            .and_then(|id| src.style_table.hyperlinks.get(id as usize))
            .map(|link| builder.intern_hyperlink(link.clone()));
        let new_id = builder.intern_style(Style {
            fg: style.fg,
            bg: style.bg,
            underline_color: style.underline_color,
            flags: style.flags,
            hyperlink_id: remapped_link,
        });
        style_remap.push(new_id);
    }

    let lines = src
        .lines
        .iter()
        .map(|row| ScreenRow {
            wrapped: row.wrapped,
            cells: row
                .cells
                .iter()
                .map(|cell| CellData {
                    c: cell.c,
                    extra: cell.extra.clone(),
                    width: cell.width,
                    spacer: cell.spacer,
                    style_id: style_remap
                        .get(cell.style_id as usize)
                        .copied()
                        .unwrap_or(0),
                })
                .collect(),
        })
        .collect();

    ScreenSnapshot {
        cols: src.cols,
        rows: src.rows,
        lines,
        style_table: builder.table().clone(),
        cursor: src.cursor,
        modes: src.modes,
        title: src.title.clone(),
        cwd: src.cwd.clone(),
        scrollback_epoch: src.scrollback_epoch,
        scrollback_oldest_seq: src.scrollback_oldest_seq,
        scrollback_next_seq: src.scrollback_next_seq,
        palette_overrides: src.palette_overrides.clone(),
    }
}

fn row_hash(row: &ScreenRow) -> u64 {
    let mut hasher = DefaultHasher::new();
    row.hash(&mut hasher);
    hasher.finish()
}

fn is_blank_row(row: &ScreenRow) -> bool {
    !row.wrapped && row.cells.iter().all(CellData::is_blank_default)
}

fn detect_scroll_shifts(prev: &[ScreenRow], cur: &[ScreenRow]) -> Vec<ScrollRegionShift> {
    let n = prev.len();
    if n < 3 || cur.len() != n {
        return Vec::new();
    }

    let prev_hashes: Vec<u64> = prev.iter().map(row_hash).collect();
    let cur_hashes: Vec<u64> = cur.iter().map(row_hash).collect();
    let prev_blank: Vec<bool> = prev.iter().map(is_blank_row).collect();

    let mut best: Option<(usize, ScrollRegionShift)> = None;

    // Candidate shift magnitudes up to half the screen height are common scrolling bursts;
    // also test up to n - 1 when beneficial.
    for dist in 1..n {
        // Scroll up by `dist`: prev[r + dist] -> cur[r]
        let mut r = 0;
        while r < n - dist {
            if prev_hashes[r + dist] == cur_hashes[r] && prev[r + dist] == cur[r] {
                let start = r;
                let mut non_blank_matches = usize::from(!prev_blank[r + dist]);
                r += 1;
                while r < n - dist
                    && prev_hashes[r + dist] == cur_hashes[r]
                    && prev[r + dist] == cur[r]
                {
                    if !prev_blank[r + dist] {
                        non_blank_matches += 1;
                    }
                    r += 1;
                }
                let end = (r + dist).min(n);
                // Require at least 2 matching rows (including at least 1 non-blank row) so a scroll
                // shift clearly saves net wire cells.
                if non_blank_matches >= 2 {
                    let candidate = ScrollRegionShift {
                        top: start as u16,
                        bottom: end as u16,
                        delta: dist as i16,
                    };
                    if best.is_none_or(|(score, _)| non_blank_matches > score) {
                        best = Some((non_blank_matches, candidate));
                    }
                }
            } else {
                r += 1;
            }
        }

        // Scroll down by `dist`: prev[r] -> cur[r + dist]
        let mut r = 0;
        while r < n - dist {
            if prev_hashes[r] == cur_hashes[r + dist] && prev[r] == cur[r + dist] {
                let start = r;
                let mut non_blank_matches = usize::from(!prev_blank[r]);
                r += 1;
                while r < n - dist
                    && prev_hashes[r] == cur_hashes[r + dist]
                    && prev[r] == cur[r + dist]
                {
                    if !prev_blank[r] {
                        non_blank_matches += 1;
                    }
                    r += 1;
                }
                let end = (r + dist).min(n);
                if non_blank_matches >= 2 {
                    let candidate = ScrollRegionShift {
                        top: start as u16,
                        bottom: end as u16,
                        delta: -(dist as i16),
                    };
                    if best.is_none_or(|(score, _)| non_blank_matches > score) {
                        best = Some((non_blank_matches, candidate));
                    }
                }
            } else {
                r += 1;
            }
        }
    }

    best.into_iter().map(|(_, shift)| shift).collect()
}

fn apply_scroll_shift(lines: &mut [ScreenRow], shift: ScrollRegionShift, cols: usize) {
    let top = (shift.top as usize).min(lines.len());
    let bottom = (shift.bottom as usize).min(lines.len());
    if top >= bottom || shift.delta == 0 {
        return;
    }
    let region_len = bottom - top;
    let abs_delta = (shift.delta.unsigned_abs() as usize).min(region_len);
    let blank_row = || ScreenRow {
        cells: vec![CellData::default(); cols],
        wrapped: false,
    };

    if shift.delta > 0 {
        // Scroll content UP inside [top..bottom): row[i] = row[i + abs_delta]
        for i in top..(bottom - abs_delta) {
            lines.swap(i, i + abs_delta);
        }
        for row in &mut lines[(bottom - abs_delta)..bottom] {
            *row = blank_row();
        }
    } else {
        // Scroll content DOWN inside [top..bottom): row[i] = row[i - abs_delta]
        for i in ((top + abs_delta)..bottom).rev() {
            lines.swap(i, i - abs_delta);
        }
        for row in &mut lines[top..(top + abs_delta)] {
            *row = blank_row();
        }
    }
}

fn diff_row(row_idx: u16, sim: &ScreenRow, cur: &ScreenRow) -> Option<RowPatch> {
    if sim == cur {
        return None;
    }

    let len = cur.cells.len();
    let mut spans = Vec::new();
    let mut col = 0;

    while col < len {
        let same = sim.cells.get(col).is_some_and(|c| c == &cur.cells[col]);
        if same {
            col += 1;
            continue;
        }
        let start = col;
        col += 1;
        // Coalesce short unchanged gaps (<= 3 identical cells) into one span so span headers stay
        // tiny and cache-friendly.
        while col < len {
            let next_same = sim.cells.get(col).is_some_and(|c| c == &cur.cells[col]);
            if !next_same {
                col += 1;
                continue;
            }
            let lookahead_same = (1..=3).all(|k| {
                col + k >= len
                    || sim
                        .cells
                        .get(col + k)
                        .is_some_and(|c| c == &cur.cells[col + k])
            });
            if lookahead_same {
                break;
            }
            col += 1;
        }
        spans.push(CellSpan {
            start_col: start as u16,
            cells: cur.cells[start..col].to_vec(),
        });
    }

    Some(RowPatch {
        row: row_idx,
        wrapped: cur.wrapped,
        spans,
    })
}
