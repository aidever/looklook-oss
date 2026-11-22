//! Alacritty-backed terminal emulation core with scrollback epoch/seq and handover serialization.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::COUNT as COLOR_COUNT;
use alacritty_terminal::term::{Config as AlacrittyConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    self, Attr, CharsetIndex, ClearMode, Color, CursorShape as VteCursorShape, CursorStyle,
    Handler, Hyperlink, KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode,
    ModifyOtherKeys, NamedColor, NamedMode, NamedPrivateMode, PrivateMode, Rgb, ScpCharPath,
    ScpUpdateMode, StandardCharset, StdSyncHandler, TabulationClearMode,
};
use serde::{Deserialize, Serialize};

use crate::ansi::{render_history_and_screen_ansi, render_plain_text, render_screen_ansi};
use crate::osc::OscScanner;
use crate::scrollback::{ScrollbackSnapshot, ScrollbackStore};
use crate::types::{
    CellData, CellSpacer, ColorSpec, CoreEvent, CursorShape, CursorState, HyperlinkData,
    MouseEncoding, MouseProtocol, NamedColorCode, RgbColor, ScreenRow, ScreenSnapshot,
    ScrollbackRow, Style, StyleFlags, StyleTableBuilder, TermModes,
};
use crate::{CoreConfig, CoreError, HistoryError, HistorySlice, TerminalCore};

/// Lines retained inside `Term`'s own grid so height growth / reflow can pull recent lines.
const HOT_GRID_HISTORY: usize = 128;
/// Additional headroom above `HOT_GRID_HISTORY` before trimming `Term`'s grid history.
const TRIM_SLACK: usize = 128;

#[derive(Clone, Debug)]
struct TermDimensions {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone, Debug)]
struct EventProxy {
    events: Arc<Mutex<Vec<CoreEvent>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacrittyEvent) {
        let mapped = match event {
            AlacrittyEvent::Title(t) => Some(CoreEvent::Title(Some(t))),
            AlacrittyEvent::ResetTitle => Some(CoreEvent::Title(None)),
            AlacrittyEvent::Bell => Some(CoreEvent::Bell),
            AlacrittyEvent::PtyWrite(s) => Some(CoreEvent::PtyWrite(s.into_bytes())),
            AlacrittyEvent::ClipboardStore(_, text) => Some(CoreEvent::ClipboardStore(text)),
            _ => None,
        };
        if let Some(ev) = mapped {
            if let Ok(mut guard) = self.events.lock() {
                guard.push(ev);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedGrid {
    cols: u16,
    rows: u16,
    lines: Vec<ScreenRow>,
    styles: Vec<Style>,
    hyperlinks: Vec<HyperlinkData>,
    cursor_row: i32,
    cursor_col: usize,
    cursor_needs_wrap: bool,
    saved_cursor_row: i32,
    saved_cursor_col: usize,
    saved_needs_wrap: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CoreHandoverState {
    version: u32,
    cols: u16,
    rows: u16,
    in_alt_screen: bool,
    primary: SavedGrid,
    alt: Option<SavedGrid>,
    modes: TermModes,
    cursor_shape: CursorShape,
    cursor_blinking: bool,
    scroll_top: i32,
    scroll_bottom: i32,
    title: Option<String>,
    cwd: Option<String>,
    palette_overrides: Vec<(u16, RgbColor)>,
    scrollback: ScrollbackSnapshot,
    scrollback_emitted_epoch: u64,
    scrollback_emitted_seq: u64,
    osc_scanner: OscScanner,
}

/// Terminal emulation core built on `alacritty_terminal = "0.26.0"`.
pub struct AlacrittyCore {
    term: Term<EventProxy>,
    parser: ansi::Processor<StdSyncHandler>,
    scrollback: ScrollbackStore,
    events_sink: Arc<Mutex<Vec<CoreEvent>>>,
    osc_scanner: OscScanner,
    scroll_region: Range<Line>,
    title: Option<String>,
    cwd: Option<String>,
    primary_backup: Option<SavedGrid>,
    in_sync_update: bool,
    sync_started_at: Option<Instant>,
    palette_overrides: BTreeMap<u16, RgbColor>,
    emitted_epoch: u64,
    emitted_seq: u64,
}

impl AlacrittyCore {
    pub fn new(config: CoreConfig) -> Result<Self, CoreError> {
        let cols = config.cols.max(2);
        let rows = config.rows.max(1);
        let events_sink = Arc::new(Mutex::new(Vec::new()));
        let proxy = EventProxy {
            events: Arc::clone(&events_sink),
        };
        let alacritty_cfg = AlacrittyConfig {
            scrolling_history: grid_history_limit(rows as usize),
            ..AlacrittyConfig::default()
        };
        let dims = TermDimensions {
            cols: cols as usize,
            rows: rows as usize,
        };
        let term = Term::new(alacritty_cfg, &dims, proxy);
        let scrollback = ScrollbackStore::new(config.scrollback_limit)?;
        Ok(Self {
            term,
            parser: ansi::Processor::new(),
            scrollback,
            events_sink,
            osc_scanner: OscScanner::new(),
            scroll_region: Line(0)..Line(rows as i32),
            title: None,
            cwd: None,
            primary_backup: None,
            in_sync_update: false,
            sync_started_at: None,
            palette_overrides: BTreeMap::new(),
            emitted_epoch: 0,
            emitted_seq: 0,
        })
    }

    /// Reconstruct a core instance from serialized handover state (`TerminalCore::serialize_state`).
    pub fn from_serialized(bytes: &[u8]) -> Result<Self, CoreError> {
        let raw = zstd::bulk::decompress(bytes, 64 * 1024 * 1024)
            .map_err(|e| CoreError::Compression(format!("decompress handover state: {e}")))?;
        let state: CoreHandoverState = bincode::deserialize(&raw)
            .map_err(|e| CoreError::Serialization(format!("decode handover state: {e}")))?;
        if state.version != 1 {
            return Err(CoreError::Serialization(format!(
                "unsupported handover state version {}",
                state.version
            )));
        }

        let mut core = Self::new(CoreConfig {
            cols: state.cols,
            rows: state.rows,
            scrollback_limit: state.scrollback.max_lines,
        })?;
        core.scrollback = ScrollbackStore::from_snapshot(state.scrollback)?;
        core.emitted_epoch = state.scrollback_emitted_epoch;
        core.emitted_seq = state.scrollback_emitted_seq;
        core.title = state.title.clone();
        core.cwd = state.cwd.clone();
        core.osc_scanner = state.osc_scanner;

        // Seed primary grid cells & cursors first.
        apply_saved_grid(core.term.grid_mut(), &state.primary);

        // If handover occurred inside alternate screen, swap to alt and apply alt grid.
        if state.in_alt_screen {
            core.primary_backup = Some(state.primary.clone());
            core.term.swap_alt();
            if let Some(alt_grid) = &state.alt {
                apply_saved_grid(core.term.grid_mut(), alt_grid);
            }
        }

        // Restore modes, cursor style, palette colors, title, scroll region, then re-apply active cursor.
        apply_modes(&mut core.term, state.modes);
        core.term.set_cursor_style(Some(CursorStyle {
            shape: to_vte_cursor_shape(state.cursor_shape),
            blinking: state.cursor_blinking,
        }));
        for (idx, rgb) in &state.palette_overrides {
            core.term.set_color(
                *idx as usize,
                Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                },
            );
            core.palette_overrides.insert(*idx, *rgb);
        }
        if let Some(title) = &state.title {
            core.term.set_title(Some(title.clone()));
        }

        let top = (state.scroll_top + 1).max(1) as usize;
        let bottom = state.scroll_bottom.max(top as i32) as usize;
        core.term.set_scrolling_region(top, Some(bottom));
        core.scroll_region = Line(state.scroll_top)..Line(state.scroll_bottom);

        // `set_scrolling_region` homes the cursor, so restore active cursor point afterward.
        let active_saved = state
            .alt
            .as_ref()
            .filter(|_| state.in_alt_screen)
            .unwrap_or(&state.primary);
        core.term.grid_mut().cursor.point = Point::new(
            Line(active_saved.cursor_row),
            Column(active_saved.cursor_col),
        );
        core.term.grid_mut().cursor.input_needs_wrap = active_saved.cursor_needs_wrap;

        // Clear spurious events emitted during reconstruction.
        if let Ok(mut guard) = core.events_sink.lock() {
            guard.clear();
        }
        core.in_sync_update = false;
        core.sync_started_at = None;
        Ok(core)
    }

    fn drain_events_with<F>(&mut self, f: F) -> Vec<CoreEvent>
    where
        F: FnOnce(&mut Vec<CoreEvent>),
    {
        let mut events = self
            .events_sink
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        f(&mut events);
        events
    }

    fn push_grid_history_rows(&mut self, from_line_neg: usize) -> Result<(), CoreError> {
        if from_line_neg == 0 {
            return Ok(());
        }
        // Push from oldest of the newly scrolled batch (`-from_line_neg`) to newest (`-1`).
        let mut rows_to_push = Vec::with_capacity(from_line_neg);
        for depth in (1..=from_line_neg).rev() {
            let grid_row = &self.term.grid()[Line(-(depth as i32))];
            rows_to_push.push(extract_scrollback_payload(grid_row));
        }
        for (wrapped, cells, styles, hyperlinks) in rows_to_push {
            self.scrollback
                .push_row(wrapped, cells, styles, hyperlinks)?;
        }
        self.trim_hot_grid_history();
        Ok(())
    }

    fn trim_hot_grid_history(&mut self) {
        let rows = self.term.screen_lines();
        if self.term.grid().history_size() > HOT_GRID_HISTORY + TRIM_SLACK {
            self.term.grid_mut().update_history(HOT_GRID_HISTORY);
            self.term
                .grid_mut()
                .update_history(grid_history_limit(rows));
        }
    }

    fn post_process_events(&mut self, events: &[CoreEvent]) {
        for ev in events {
            match ev {
                CoreEvent::Title(t) => self.title = t.clone(),
                CoreEvent::CwdChanged(p) => self.cwd = Some(p.clone()),
                _ => {}
            }
        }
    }
}

fn grid_history_limit(rows: usize) -> usize {
    HOT_GRID_HISTORY + TRIM_SLACK + rows.max(256)
}

impl TerminalCore for AlacrittyCore {
    fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    fn feed_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut osc_events = Vec::new();
        self.osc_scanner.feed(bytes, &mut osc_events);
        self.post_process_events(&osc_events);
        if !osc_events.is_empty() {
            if let Ok(mut guard) = self.events_sink.lock() {
                guard.extend(osc_events);
            }
        }

        let mut processor = std::mem::take(&mut self.parser);
        let mut handler = CoreHandler { core: self };
        processor.advance(&mut handler, bytes);
        let _ = handler;
        self.parser = processor;
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let (old_cols, old_rows) = self.size();
        if old_cols == cols && old_rows == rows {
            return;
        }

        let was_alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let before_hist = if was_alt {
            0
        } else {
            self.term.grid().history_size()
        };

        let dims = TermDimensions {
            cols: cols as usize,
            rows: rows as usize,
        };
        self.term.resize(dims);
        self.scroll_region = Line(0)..Line(rows as i32);

        if let Some(primary) = self.primary_backup.as_mut() {
            resize_saved_grid(primary, cols, rows);
        }

        if !was_alt {
            let after_hist = self.term.grid().history_size();
            if old_cols != cols {
                let _ = self.scrollback.pop_newest(before_hist);
                let _ = self.push_grid_history_rows(after_hist);
                if self.emitted_epoch == self.scrollback.epoch()
                    && self.scrollback.next_seq() < self.emitted_seq
                {
                    let epoch = self.scrollback.clear();
                    self.term.grid_mut().clear_history();
                    if let Ok(mut guard) = self.events_sink.lock() {
                        guard.push(CoreEvent::ScrollbackCleared { epoch });
                    }
                }
            } else if after_hist > before_hist {
                let _ = self.push_grid_history_rows(after_hist - before_hist);
            } else if after_hist < before_hist {
                let pulled = before_hist - after_hist;
                let _ = self.scrollback.pop_newest(pulled);
                if self.emitted_epoch == self.scrollback.epoch()
                    && self.scrollback.next_seq() < self.emitted_seq
                {
                    self.emitted_seq = self.scrollback.next_seq();
                }
            }
            self.term
                .grid_mut()
                .update_history(grid_history_limit(rows as usize));
        }
    }

    fn in_sync_update(&self) -> bool {
        self.in_sync_update
    }

    fn flush_sync(&mut self) {
        if self.in_sync_update || self.parser.sync_bytes_count() > 0 {
            let mut processor = std::mem::take(&mut self.parser);
            let mut handler = CoreHandler { core: self };
            processor.stop_sync(&mut handler);
            let _ = handler;
            self.parser = processor;
        }
        self.in_sync_update = false;
        self.sync_started_at = None;
    }

    fn check_sync_timeout(&mut self, max_wait: Duration) -> bool {
        if let Some(started) = self.sync_started_at {
            if self.in_sync_update && started.elapsed() >= max_wait {
                self.flush_sync();
                return true;
            }
        }
        false
    }

    fn snapshot(&self) -> ScreenSnapshot {
        let (cols, rows) = self.size();
        let grid = self.term.grid();
        let mut style_builder = StyleTableBuilder::new();
        let mut lines = Vec::with_capacity(rows as usize);

        for r in 0..rows as i32 {
            let row = &grid[Line(r)];
            lines.push(extract_screen_row(row, cols as usize, &mut style_builder));
        }

        let palette_overrides = self
            .palette_overrides
            .iter()
            .map(|(&idx, &rgb)| {
                let color = self.term.colors()[idx as usize].map_or(rgb, |live| RgbColor {
                    r: live.r,
                    g: live.g,
                    b: live.b,
                });
                (idx, color)
            })
            .collect();

        ScreenSnapshot {
            cols,
            rows,
            lines,
            style_table: style_builder.into_table(),
            cursor: self.cursor(),
            modes: self.modes(),
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            scrollback_epoch: self.scrollback.epoch(),
            scrollback_oldest_seq: self.scrollback.oldest_seq(),
            scrollback_next_seq: self.scrollback.next_seq(),
            palette_overrides,
        }
    }

    fn cursor(&self) -> CursorState {
        let style = self.term.cursor_style();
        let point = self.term.grid().cursor.point;
        let mode = self.term.mode();
        let rows = self.term.screen_lines().saturating_sub(1);
        let cols = self.term.columns().saturating_sub(1);
        CursorState {
            row: (point.line.0.max(0) as usize).min(rows) as u16,
            col: point.column.0.min(cols) as u16,
            shape: from_vte_cursor_shape(style.shape),
            blinking: style.blinking,
            visible: mode.contains(TermMode::SHOW_CURSOR) && style.shape != VteCursorShape::Hidden,
        }
    }

    fn modes(&self) -> TermModes {
        extract_modes(*self.term.mode(), self.in_sync_update)
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    fn take_events(&mut self) -> Vec<CoreEvent> {
        let events = self.drain_events_with(|_| {});
        self.post_process_events(&events);
        events
    }

    fn take_new_scrollback(&mut self) -> (u64, Vec<ScrollbackRow>) {
        let epoch = self.scrollback.epoch();
        let next = self.scrollback.next_seq();
        if self.emitted_epoch != epoch {
            self.emitted_epoch = epoch;
            self.emitted_seq = self.scrollback.oldest_seq();
        }
        if self.emitted_seq >= next {
            return (epoch, Vec::new());
        }
        let start = self.emitted_seq.max(self.scrollback.oldest_seq());
        let count = (next - start) as usize;
        let rows = match self.scrollback.slice(epoch, start, count) {
            Ok(slice) => slice.rows,
            Err(_) => Vec::new(),
        };
        self.emitted_seq = next;
        (epoch, rows)
    }

    fn scrollback_info(&self) -> (u64, u64, u64, usize) {
        (
            self.scrollback.epoch(),
            self.scrollback.oldest_seq(),
            self.scrollback.next_seq(),
            self.scrollback.len(),
        )
    }

    fn history_slice(
        &mut self,
        epoch: u64,
        start_seq: u64,
        count: usize,
    ) -> Result<HistorySlice, HistoryError> {
        self.scrollback.slice(epoch, start_seq, count)
    }

    fn tail_history(&mut self, count: usize) -> Result<HistorySlice, CoreError> {
        self.scrollback.tail(count)
    }

    fn clear_scrollback(&mut self) -> u64 {
        self.term.grid_mut().clear_history();
        let epoch = self.scrollback.clear();
        self.emitted_epoch = epoch;
        self.emitted_seq = 0;
        if let Ok(mut guard) = self.events_sink.lock() {
            guard.push(CoreEvent::ScrollbackCleared { epoch });
        }
        epoch
    }

    fn capture_screen_ansi(&self) -> String {
        render_screen_ansi(&self.snapshot())
    }

    fn capture_history_ansi(&mut self, tail_lines: Option<usize>) -> Result<String, CoreError> {
        let snap = self.snapshot();
        let hist = if let Some(n) = tail_lines {
            let need_hist = n.saturating_sub(snap.lines.len());
            if need_hist == 0 {
                Vec::new()
            } else {
                self.scrollback.tail(need_hist)?.rows
            }
        } else {
            self.scrollback.tail(self.scrollback.len())?.rows
        };
        Ok(render_history_and_screen_ansi(&hist, &snap, tail_lines))
    }

    fn capture_plain_text(&mut self, include_scrollback: bool) -> Result<String, CoreError> {
        let snap = self.snapshot();
        let hist = if include_scrollback {
            self.scrollback.tail(self.scrollback.len())?.rows
        } else {
            Vec::new()
        };
        Ok(render_plain_text(&hist, &snap))
    }

    fn serialize_state(&self) -> Result<Vec<u8>, CoreError> {
        let (cols, rows) = self.size();
        let in_alt_screen = self.term.mode().contains(TermMode::ALT_SCREEN);
        let active = capture_saved_grid(self.term.grid(), cols, rows);
        let (primary, alt) = if in_alt_screen {
            let fallback_primary = self.primary_backup.clone().unwrap_or_else(|| SavedGrid {
                cols,
                rows,
                lines: (0..rows)
                    .map(|_| ScreenRow {
                        cells: vec![CellData::default(); cols as usize],
                        wrapped: false,
                    })
                    .collect(),
                styles: vec![Style::default()],
                hyperlinks: Vec::new(),
                cursor_row: 0,
                cursor_col: 0,
                cursor_needs_wrap: false,
                saved_cursor_row: 0,
                saved_cursor_col: 0,
                saved_needs_wrap: false,
            });
            (fallback_primary, Some(active))
        } else {
            (active, None)
        };

        let style = self.term.cursor_style();
        let palette_overrides = (0..COLOR_COUNT)
            .filter_map(|i| {
                self.term.colors()[i].map(|c| {
                    (
                        i as u16,
                        RgbColor {
                            r: c.r,
                            g: c.g,
                            b: c.b,
                        },
                    )
                })
            })
            .collect();

        let state = CoreHandoverState {
            version: 1,
            cols,
            rows,
            in_alt_screen,
            primary,
            alt,
            modes: self.modes(),
            cursor_shape: from_vte_cursor_shape(style.shape),
            cursor_blinking: style.blinking,
            scroll_top: self.scroll_region.start.0,
            scroll_bottom: self.scroll_region.end.0,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            palette_overrides,
            scrollback: self.scrollback.to_snapshot(),
            scrollback_emitted_epoch: self.emitted_epoch,
            scrollback_emitted_seq: self.emitted_seq,
            osc_scanner: self.osc_scanner.clone(),
        };
        let encoded = bincode::serialize(&state)
            .map_err(|e| CoreError::Serialization(format!("encode handover state: {e}")))?;
        zstd::bulk::compress(&encoded, 3)
            .map_err(|e| CoreError::Compression(format!("compress handover state: {e}")))
    }
}

/// Wrapper implementing `vte::ansi::Handler` to capture scrollback mutations, sync-update edges,
/// primary screen backups before alt-screen entry, and epoch resets cleanly.
struct CoreHandler<'a> {
    core: &'a mut AlacrittyCore,
}

impl CoreHandler<'_> {
    fn with_scrollback_capture<F: FnOnce(&mut Term<EventProxy>)>(&mut self, f: F) {
        let was_alt = self.core.term.mode().contains(TermMode::ALT_SCREEN);
        let before = if was_alt {
            0
        } else {
            self.core.term.grid().history_size()
        };
        f(&mut self.core.term);
        if !was_alt {
            let after = self.core.term.grid().history_size();
            if after > before {
                let _ = self.core.push_grid_history_rows(after - before);
            }
        }
    }
}

impl Handler for CoreHandler<'_> {
    fn set_title(&mut self, title: Option<String>) {
        self.core.title = title.clone();
        self.core.term.set_title(title);
    }

    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        self.core.term.set_cursor_style(style);
    }

    fn set_cursor_shape(&mut self, shape: VteCursorShape) {
        self.core.term.set_cursor_shape(shape);
    }

    fn input(&mut self, c: char) {
        self.with_scrollback_capture(|term| term.input(c));
    }

    fn goto(&mut self, line: i32, col: usize) {
        self.core.term.goto(line, col);
    }

    fn goto_line(&mut self, line: i32) {
        self.core.term.goto_line(line);
    }

    fn goto_col(&mut self, col: usize) {
        self.core.term.goto_col(col);
    }

    fn insert_blank(&mut self, count: usize) {
        self.core.term.insert_blank(count);
    }

    fn move_up(&mut self, rows: usize) {
        self.core.term.move_up(rows);
    }

    fn move_down(&mut self, rows: usize) {
        self.core.term.move_down(rows);
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        self.core.term.identify_terminal(intermediate);
    }

    fn device_status(&mut self, arg: usize) {
        self.core.term.device_status(arg);
    }

    fn move_forward(&mut self, cols: usize) {
        self.core.term.move_forward(cols);
    }

    fn move_backward(&mut self, cols: usize) {
        self.core.term.move_backward(cols);
    }

    fn move_down_and_cr(&mut self, rows: usize) {
        self.core.term.move_down_and_cr(rows);
    }

    fn move_up_and_cr(&mut self, rows: usize) {
        self.core.term.move_up_and_cr(rows);
    }

    fn put_tab(&mut self, count: u16) {
        self.core.term.put_tab(count);
    }

    fn backspace(&mut self) {
        self.core.term.backspace();
    }

    fn carriage_return(&mut self) {
        self.core.term.carriage_return();
    }

    fn linefeed(&mut self) {
        self.with_scrollback_capture(|term| term.linefeed());
    }

    fn bell(&mut self) {
        self.core.term.bell();
    }

    fn substitute(&mut self) {
        self.core.term.substitute();
    }

    fn newline(&mut self) {
        self.with_scrollback_capture(|term| term.newline());
    }

    fn set_horizontal_tabstop(&mut self) {
        self.core.term.set_horizontal_tabstop();
    }

    fn scroll_up(&mut self, rows: usize) {
        self.with_scrollback_capture(|term| term.scroll_up(rows));
    }

    fn scroll_down(&mut self, rows: usize) {
        self.core.term.scroll_down(rows);
    }

    fn insert_blank_lines(&mut self, count: usize) {
        self.core.term.insert_blank_lines(count);
    }

    fn delete_lines(&mut self, count: usize) {
        self.with_scrollback_capture(|term| term.delete_lines(count));
    }

    fn erase_chars(&mut self, count: usize) {
        self.core.term.erase_chars(count);
    }

    fn delete_chars(&mut self, count: usize) {
        self.core.term.delete_chars(count);
    }

    fn move_backward_tabs(&mut self, count: u16) {
        self.core.term.move_backward_tabs(count);
    }

    fn move_forward_tabs(&mut self, count: u16) {
        self.core.term.move_forward_tabs(count);
    }

    fn save_cursor_position(&mut self) {
        self.core.term.save_cursor_position();
    }

    fn restore_cursor_position(&mut self) {
        self.core.term.restore_cursor_position();
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        self.core.term.clear_line(mode);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        if matches!(mode, ClearMode::Saved) {
            self.core.term.clear_screen(mode);
            let epoch = self.core.scrollback.clear();
            self.core.emitted_epoch = epoch;
            self.core.emitted_seq = 0;
            if let Ok(mut guard) = self.core.events_sink.lock() {
                guard.push(CoreEvent::ScrollbackCleared { epoch });
            }
            return;
        }
        self.with_scrollback_capture(|term| term.clear_screen(mode));
    }

    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        self.core.term.clear_tabs(mode);
    }

    fn set_tabs(&mut self, interval: u16) {
        self.core.term.set_tabs(interval);
    }

    fn reset_state(&mut self) {
        self.core.term.reset_state();
        self.core.primary_backup = None;
        self.core.title = None;
        self.core.palette_overrides.clear();
        self.core.scroll_region = Line(0)..Line(self.core.term.screen_lines() as i32);
        let epoch = self.core.scrollback.clear();
        self.core.emitted_epoch = epoch;
        self.core.emitted_seq = 0;
        if let Ok(mut guard) = self.core.events_sink.lock() {
            guard.push(CoreEvent::ScrollbackCleared { epoch });
        }
    }

    fn reverse_index(&mut self) {
        self.core.term.reverse_index();
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        self.core.term.terminal_attribute(attr);
    }

    fn set_mode(&mut self, mode: Mode) {
        self.core.term.set_mode(mode);
    }

    fn unset_mode(&mut self, mode: Mode) {
        self.core.term.unset_mode(mode);
    }

    fn report_mode(&mut self, mode: Mode) {
        self.core.term.report_mode(mode);
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(NamedPrivateMode::SyncUpdate) => {
                self.core.in_sync_update = true;
                self.core.sync_started_at = Some(Instant::now());
            }
            PrivateMode::Named(NamedPrivateMode::ColumnMode) => {
                self.core.scroll_region = Line(0)..Line(self.core.term.screen_lines() as i32);
            }
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                if !self.core.term.mode().contains(TermMode::ALT_SCREEN) {
                    let (cols, rows) = self.core.size();
                    self.core.primary_backup =
                        Some(capture_saved_grid(self.core.term.grid(), cols, rows));
                    if let Ok(mut guard) = self.core.events_sink.lock() {
                        guard.push(CoreEvent::AltScreenChanged(true));
                    }
                }
            }
            PrivateMode::Unknown(47 | 1047) => {
                if !self.core.term.mode().contains(TermMode::ALT_SCREEN) {
                    let (cols, rows) = self.core.size();
                    self.core.primary_backup =
                        Some(capture_saved_grid(self.core.term.grid(), cols, rows));
                    self.core.term.swap_alt();
                    if let Ok(mut guard) = self.core.events_sink.lock() {
                        guard.push(CoreEvent::AltScreenChanged(true));
                    }
                }
                return;
            }
            _ => {}
        }
        self.core.term.set_private_mode(mode);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(NamedPrivateMode::SyncUpdate) => {
                self.core.in_sync_update = false;
                self.core.sync_started_at = None;
            }
            PrivateMode::Named(NamedPrivateMode::ColumnMode) => {
                self.core.scroll_region = Line(0)..Line(self.core.term.screen_lines() as i32);
            }
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                if self.core.term.mode().contains(TermMode::ALT_SCREEN) {
                    self.core.primary_backup = None;
                    if let Ok(mut guard) = self.core.events_sink.lock() {
                        guard.push(CoreEvent::AltScreenChanged(false));
                    }
                }
            }
            PrivateMode::Unknown(47 | 1047) => {
                if self.core.term.mode().contains(TermMode::ALT_SCREEN) {
                    self.core.primary_backup = None;
                    self.core.term.swap_alt();
                    if let Ok(mut guard) = self.core.events_sink.lock() {
                        guard.push(CoreEvent::AltScreenChanged(false));
                    }
                }
                return;
            }
            _ => {}
        }
        self.core.term.unset_private_mode(mode);
    }

    fn report_private_mode(&mut self, mode: PrivateMode) {
        self.core.term.report_private_mode(mode);
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        let rows = self.core.term.screen_lines();
        let eff_bottom = bottom.unwrap_or(rows);
        if top < eff_bottom {
            let start = Line((top as i32 - 1).min(rows as i32));
            let end = Line((eff_bottom as i32).min(rows as i32));
            self.core.scroll_region = start..end;
        }
        self.core.term.set_scrolling_region(top, bottom);
    }

    fn set_keypad_application_mode(&mut self) {
        self.core.term.set_keypad_application_mode();
    }

    fn unset_keypad_application_mode(&mut self) {
        self.core.term.unset_keypad_application_mode();
    }

    fn set_active_charset(&mut self, index: CharsetIndex) {
        self.core.term.set_active_charset(index);
    }

    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        self.core.term.configure_charset(index, charset);
    }

    fn set_color(&mut self, index: usize, color: Rgb) {
        self.core.palette_overrides.insert(
            index as u16,
            RgbColor {
                r: color.r,
                g: color.g,
                b: color.b,
            },
        );
        self.core.term.set_color(index, color);
    }

    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        self.core
            .term
            .dynamic_color_sequence(prefix, index, terminator);
    }

    fn reset_color(&mut self, index: usize) {
        self.core.palette_overrides.remove(&(index as u16));
        self.core.term.reset_color(index);
    }

    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        self.core.term.clipboard_store(clipboard, base64);
    }

    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        self.core.term.clipboard_load(clipboard, terminator);
    }

    fn decaln(&mut self) {
        self.core.term.decaln();
    }

    fn push_title(&mut self) {
        self.core.term.push_title();
    }

    fn pop_title(&mut self) {
        self.core.term.pop_title();
        if let Ok(guard) = self.core.events_sink.lock() {
            if let Some(CoreEvent::Title(t)) = guard
                .iter()
                .rev()
                .find(|ev| matches!(ev, CoreEvent::Title(_)))
            {
                self.core.title = t.clone();
            }
        }
    }

    fn text_area_size_pixels(&mut self) {
        self.core.term.text_area_size_pixels();
    }

    fn text_area_size_chars(&mut self) {
        self.core.term.text_area_size_chars();
    }

    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        self.core.term.set_hyperlink(hyperlink);
    }

    fn report_keyboard_mode(&mut self) {
        self.core.term.report_keyboard_mode();
    }

    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        self.core.term.push_keyboard_mode(mode);
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        self.core.term.pop_keyboard_modes(to_pop);
    }

    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        self.core.term.set_keyboard_mode(mode, behavior);
    }

    fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
        self.core.term.set_modify_other_keys(mode);
    }

    fn report_modify_other_keys(&mut self) {
        self.core.term.report_modify_other_keys();
    }

    fn set_scp(&mut self, char_path: ScpCharPath, update_mode: ScpUpdateMode) {
        self.core.term.set_scp(char_path, update_mode);
    }
}

fn extract_scrollback_payload(
    row: &Row<Cell>,
) -> (bool, Vec<CellData>, Vec<Style>, Vec<HyperlinkData>) {
    let mut style_builder = StyleTableBuilder::new();
    let cols = row[..].len();
    let screen_row = extract_screen_row(row, cols, &mut style_builder);
    let table = style_builder.into_table();
    (
        screen_row.wrapped,
        screen_row.cells,
        table.styles,
        table.hyperlinks,
    )
}

fn extract_screen_row(
    row: &Row<Cell>,
    cols: usize,
    style_builder: &mut StyleTableBuilder,
) -> ScreenRow {
    let slice = &row[..];
    let mut cells = Vec::with_capacity(cols);
    for col_idx in 0..cols {
        if let Some(cell) = slice.get(col_idx) {
            cells.push(convert_cell(cell, style_builder));
        } else {
            cells.push(CellData::default());
        }
    }
    let wrapped = slice
        .last()
        .is_some_and(|c| c.flags.contains(Flags::WRAPLINE));
    ScreenRow { cells, wrapped }
}

fn convert_cell(cell: &Cell, style_builder: &mut StyleTableBuilder) -> CellData {
    let hyperlink_id = cell.hyperlink().map(|link| {
        style_builder.intern_hyperlink(HyperlinkData {
            id: link.id().to_owned(),
            uri: link.uri().to_owned(),
        })
    });
    let style = Style {
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        underline_color: cell.underline_color().map(convert_color),
        flags: convert_style_flags(cell.flags),
        hyperlink_id,
    };
    let style_id = style_builder.intern_style(style);

    let (width, spacer) = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        (0, CellSpacer::WideTrailing)
    } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        (0, CellSpacer::WideLeading)
    } else if cell.flags.contains(Flags::WIDE_CHAR) {
        (2, CellSpacer::None)
    } else {
        (1, CellSpacer::None)
    };

    let mut extra = String::new();
    if let Some(zw) = cell.zerowidth() {
        for &ch in zw {
            extra.push(ch);
        }
    }

    CellData {
        c: cell.c,
        extra,
        width,
        spacer,
        style_id,
    }
}

fn capture_saved_grid(grid: &Grid<Cell>, cols: u16, rows: u16) -> SavedGrid {
    let mut style_builder = StyleTableBuilder::new();
    let mut lines = Vec::with_capacity(rows as usize);
    for r in 0..rows as i32 {
        lines.push(extract_screen_row(
            &grid[Line(r)],
            cols as usize,
            &mut style_builder,
        ));
    }
    let table = style_builder.into_table();
    SavedGrid {
        cols,
        rows,
        lines,
        styles: table.styles,
        hyperlinks: table.hyperlinks,
        cursor_row: grid.cursor.point.line.0,
        cursor_col: grid.cursor.point.column.0,
        cursor_needs_wrap: grid.cursor.input_needs_wrap,
        saved_cursor_row: grid.saved_cursor.point.line.0,
        saved_cursor_col: grid.saved_cursor.point.column.0,
        saved_needs_wrap: grid.saved_cursor.input_needs_wrap,
    }
}

fn apply_saved_grid(grid: &mut Grid<Cell>, saved: &SavedGrid) {
    let rows = grid.screen_lines();
    let cols = grid.columns();
    for r in 0..rows.min(saved.lines.len()) {
        let src_row = &saved.lines[r];
        for c in 0..cols.min(src_row.cells.len()) {
            let src = &src_row.cells[c];
            let style = saved
                .styles
                .get(src.style_id as usize)
                .cloned()
                .unwrap_or_default();
            let mut flags = to_alacritty_flags(style.flags);
            match src.spacer {
                CellSpacer::None => {
                    if src.width == 2 {
                        flags.insert(Flags::WIDE_CHAR);
                    }
                }
                CellSpacer::WideTrailing => flags.insert(Flags::WIDE_CHAR_SPACER),
                CellSpacer::WideLeading => flags.insert(Flags::LEADING_WIDE_CHAR_SPACER),
            }
            if src_row.wrapped && c + 1 == cols {
                flags.insert(Flags::WRAPLINE);
            }

            let cell = &mut grid[Line(r as i32)][Column(c)];
            *cell = Cell {
                c: src.c,
                fg: to_vte_color(style.fg),
                bg: to_vte_color(style.bg),
                flags,
                extra: None,
            };
            if let Some(uc) = style.underline_color {
                cell.set_underline_color(Some(to_vte_color(uc)));
            }
            if let Some(link_id) = style.hyperlink_id {
                if let Some(link) = saved.hyperlinks.get(link_id as usize) {
                    cell.set_hyperlink(Some(alacritty_terminal::term::cell::Hyperlink::new(
                        Some(link.id.clone()),
                        link.uri.clone(),
                    )));
                }
            }
            for zw in src.extra.chars() {
                cell.push_zerowidth(zw);
            }
        }
    }

    let max_row = rows.saturating_sub(1) as i32;
    let max_col = cols.saturating_sub(1);
    grid.cursor.point = Point::new(
        Line(saved.cursor_row.clamp(0, max_row)),
        Column(saved.cursor_col.min(max_col)),
    );
    grid.cursor.input_needs_wrap = saved.cursor_needs_wrap;
    grid.saved_cursor.point = Point::new(
        Line(saved.saved_cursor_row.clamp(0, max_row)),
        Column(saved.saved_cursor_col.min(max_col)),
    );
    grid.saved_cursor.input_needs_wrap = saved.saved_needs_wrap;
}

fn resize_saved_grid(saved: &mut SavedGrid, cols: u16, rows: u16) {
    saved.cols = cols;
    saved.rows = rows;
    let target_rows = rows as usize;
    let target_cols = cols as usize;
    if saved.lines.len() > target_rows {
        let drop = saved.lines.len() - target_rows;
        saved.lines.drain(..drop);
        saved.cursor_row = (saved.cursor_row - drop as i32).max(0);
        saved.saved_cursor_row = (saved.saved_cursor_row - drop as i32).max(0);
    } else {
        while saved.lines.len() < target_rows {
            saved.lines.push(ScreenRow {
                cells: vec![CellData::default(); target_cols],
                wrapped: false,
            });
        }
    }
    for row in &mut saved.lines {
        row.cells.resize_with(target_cols, CellData::default);
    }
    saved.cursor_col = saved.cursor_col.min(target_cols.saturating_sub(1));
    saved.saved_cursor_col = saved.saved_cursor_col.min(target_cols.saturating_sub(1));
}

fn extract_modes(mode: TermMode, in_sync_update: bool) -> TermModes {
    let mouse_protocol = if mode.contains(TermMode::MOUSE_MOTION) {
        MouseProtocol::AnyMotion
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        MouseProtocol::Drag
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        MouseProtocol::Normal
    } else {
        MouseProtocol::None
    };

    let mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
        MouseEncoding::Sgr
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        MouseEncoding::Utf8
    } else {
        MouseEncoding::X10
    };

    TermModes {
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        show_cursor: mode.contains(TermMode::SHOW_CURSOR),
        app_cursor: mode.contains(TermMode::APP_CURSOR),
        app_keypad: mode.contains(TermMode::APP_KEYPAD),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
        alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        line_wrap: mode.contains(TermMode::LINE_WRAP),
        origin: mode.contains(TermMode::ORIGIN),
        insert: mode.contains(TermMode::INSERT),
        sync_update: in_sync_update,
        mouse_protocol,
        mouse_encoding,
    }
}

fn apply_modes<T: EventListener>(term: &mut Term<T>, modes: TermModes) {
    set_or_unset_priv(term, NamedPrivateMode::ShowCursor, modes.show_cursor);
    set_or_unset_priv(term, NamedPrivateMode::CursorKeys, modes.app_cursor);
    set_or_unset_priv(term, NamedPrivateMode::BracketedPaste, modes.bracketed_paste);
    set_or_unset_priv(term, NamedPrivateMode::ReportFocusInOut, modes.focus_in_out);
    set_or_unset_priv(term, NamedPrivateMode::AlternateScroll, modes.alternate_scroll);
    set_or_unset_priv(term, NamedPrivateMode::LineWrap, modes.line_wrap);
    set_or_unset_priv(term, NamedPrivateMode::Origin, modes.origin);

    set_or_unset_priv(
        term,
        NamedPrivateMode::ReportMouseClicks,
        modes.mouse_protocol == MouseProtocol::Normal,
    );
    set_or_unset_priv(
        term,
        NamedPrivateMode::ReportCellMouseMotion,
        modes.mouse_protocol == MouseProtocol::Drag,
    );
    set_or_unset_priv(
        term,
        NamedPrivateMode::ReportAllMouseMotion,
        modes.mouse_protocol == MouseProtocol::AnyMotion,
    );
    set_or_unset_priv(
        term,
        NamedPrivateMode::Utf8Mouse,
        modes.mouse_encoding == MouseEncoding::Utf8,
    );
    set_or_unset_priv(
        term,
        NamedPrivateMode::SgrMouse,
        modes.mouse_encoding == MouseEncoding::Sgr,
    );

    if modes.app_keypad {
        term.set_keypad_application_mode();
    } else {
        term.unset_keypad_application_mode();
    }

    if modes.insert {
        term.set_mode(Mode::Named(NamedMode::Insert));
    } else {
        term.unset_mode(Mode::Named(NamedMode::Insert));
    }
}

fn set_or_unset_priv<T: EventListener>(term: &mut Term<T>, mode: NamedPrivateMode, enabled: bool) {
    if enabled {
        term.set_private_mode(mode.into());
    } else {
        term.unset_private_mode(mode.into());
    }
}

fn convert_style_flags(flags: Flags) -> StyleFlags {
    let mut out = StyleFlags::empty();
    if flags.contains(Flags::BOLD) {
        out.insert(StyleFlags::BOLD);
    }
    if flags.contains(Flags::DIM) {
        out.insert(StyleFlags::DIM);
    }
    if flags.contains(Flags::ITALIC) {
        out.insert(StyleFlags::ITALIC);
    }
    if flags.contains(Flags::UNDERLINE) {
        out.insert(StyleFlags::UNDERLINE);
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        out.insert(StyleFlags::DOUBLE_UNDERLINE);
    }
    if flags.contains(Flags::UNDERCURL) {
        out.insert(StyleFlags::UNDERCURL);
    }
    if flags.contains(Flags::DOTTED_UNDERLINE) {
        out.insert(StyleFlags::DOTTED_UNDERLINE);
    }
    if flags.contains(Flags::DASHED_UNDERLINE) {
        out.insert(StyleFlags::DASHED_UNDERLINE);
    }
    if flags.contains(Flags::INVERSE) {
        out.insert(StyleFlags::INVERSE);
    }
    if flags.contains(Flags::HIDDEN) {
        out.insert(StyleFlags::HIDDEN);
    }
    if flags.contains(Flags::STRIKEOUT) {
        out.insert(StyleFlags::STRIKEOUT);
    }
    out
}

fn to_alacritty_flags(flags: StyleFlags) -> Flags {
    let mut out = Flags::empty();
    if flags.contains(StyleFlags::BOLD) {
        out.insert(Flags::BOLD);
    }
    if flags.contains(StyleFlags::DIM) {
        out.insert(Flags::DIM);
    }
    if flags.contains(StyleFlags::ITALIC) {
        out.insert(Flags::ITALIC);
    }
    if flags.contains(StyleFlags::UNDERLINE) {
        out.insert(Flags::UNDERLINE);
    }
    if flags.contains(StyleFlags::DOUBLE_UNDERLINE) {
        out.insert(Flags::DOUBLE_UNDERLINE);
    }
    if flags.contains(StyleFlags::UNDERCURL) {
        out.insert(Flags::UNDERCURL);
    }
    if flags.contains(StyleFlags::DOTTED_UNDERLINE) {
        out.insert(Flags::DOTTED_UNDERLINE);
    }
    if flags.contains(StyleFlags::DASHED_UNDERLINE) {
        out.insert(Flags::DASHED_UNDERLINE);
    }
    if flags.contains(StyleFlags::INVERSE) {
        out.insert(Flags::INVERSE);
    }
    if flags.contains(StyleFlags::HIDDEN) {
        out.insert(Flags::HIDDEN);
    }
    if flags.contains(StyleFlags::STRIKEOUT) {
        out.insert(Flags::STRIKEOUT);
    }
    out
}

fn convert_color(color: Color) -> ColorSpec {
    match color {
        Color::Indexed(i) => ColorSpec::Indexed(i),
        Color::Spec(rgb) => ColorSpec::Rgb(RgbColor {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }),
        Color::Named(named) => ColorSpec::Named(match named {
            NamedColor::Black => NamedColorCode::Black,
            NamedColor::Red => NamedColorCode::Red,
            NamedColor::Green => NamedColorCode::Green,
            NamedColor::Yellow => NamedColorCode::Yellow,
            NamedColor::Blue => NamedColorCode::Blue,
            NamedColor::Magenta => NamedColorCode::Magenta,
            NamedColor::Cyan => NamedColorCode::Cyan,
            NamedColor::White => NamedColorCode::White,
            NamedColor::BrightBlack => NamedColorCode::BrightBlack,
            NamedColor::BrightRed => NamedColorCode::BrightRed,
            NamedColor::BrightGreen => NamedColorCode::BrightGreen,
            NamedColor::BrightYellow => NamedColorCode::BrightYellow,
            NamedColor::BrightBlue => NamedColorCode::BrightBlue,
            NamedColor::BrightMagenta => NamedColorCode::BrightMagenta,
            NamedColor::BrightCyan => NamedColorCode::BrightCyan,
            NamedColor::BrightWhite => NamedColorCode::BrightWhite,
            NamedColor::Foreground => NamedColorCode::Foreground,
            NamedColor::Background => NamedColorCode::Background,
            NamedColor::Cursor => NamedColorCode::Cursor,
            NamedColor::DimBlack => NamedColorCode::DimBlack,
            NamedColor::DimRed => NamedColorCode::DimRed,
            NamedColor::DimGreen => NamedColorCode::DimGreen,
            NamedColor::DimYellow => NamedColorCode::DimYellow,
            NamedColor::DimBlue => NamedColorCode::DimBlue,
            NamedColor::DimMagenta => NamedColorCode::DimMagenta,
            NamedColor::DimCyan => NamedColorCode::DimCyan,
            NamedColor::DimWhite => NamedColorCode::DimWhite,
            NamedColor::BrightForeground => NamedColorCode::BrightForeground,
            NamedColor::DimForeground => NamedColorCode::DimForeground,
        }),
    }
}

fn to_vte_color(color: ColorSpec) -> Color {
    match color {
        ColorSpec::Indexed(i) => Color::Indexed(i),
        ColorSpec::Rgb(rgb) => Color::Spec(Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }),
        ColorSpec::Named(named) => Color::Named(match named {
            NamedColorCode::Black => NamedColor::Black,
            NamedColorCode::Red => NamedColor::Red,
            NamedColorCode::Green => NamedColor::Green,
            NamedColorCode::Yellow => NamedColor::Yellow,
            NamedColorCode::Blue => NamedColor::Blue,
            NamedColorCode::Magenta => NamedColor::Magenta,
            NamedColorCode::Cyan => NamedColor::Cyan,
            NamedColorCode::White => NamedColor::White,
            NamedColorCode::BrightBlack => NamedColor::BrightBlack,
            NamedColorCode::BrightRed => NamedColor::BrightRed,
            NamedColorCode::BrightGreen => NamedColor::BrightGreen,
            NamedColorCode::BrightYellow => NamedColor::BrightYellow,
            NamedColorCode::BrightBlue => NamedColor::BrightBlue,
            NamedColorCode::BrightMagenta => NamedColor::BrightMagenta,
            NamedColorCode::BrightCyan => NamedColor::BrightCyan,
            NamedColorCode::BrightWhite => NamedColor::BrightWhite,
            NamedColorCode::Foreground => NamedColor::Foreground,
            NamedColorCode::Background => NamedColor::Background,
            NamedColorCode::Cursor => NamedColor::Cursor,
            NamedColorCode::DimBlack => NamedColor::DimBlack,
            NamedColorCode::DimRed => NamedColor::DimRed,
            NamedColorCode::DimGreen => NamedColor::DimGreen,
            NamedColorCode::DimYellow => NamedColor::DimYellow,
            NamedColorCode::DimBlue => NamedColor::DimBlue,
            NamedColorCode::DimMagenta => NamedColor::DimMagenta,
            NamedColorCode::DimCyan => NamedColor::DimCyan,
            NamedColorCode::DimWhite => NamedColor::DimWhite,
            NamedColorCode::BrightForeground => NamedColor::BrightForeground,
            NamedColorCode::DimForeground => NamedColor::DimForeground,
        }),
    }
}

fn from_vte_cursor_shape(shape: VteCursorShape) -> CursorShape {
    match shape {
        VteCursorShape::Block => CursorShape::Block,
        VteCursorShape::Underline => CursorShape::Underline,
        VteCursorShape::Beam => CursorShape::Beam,
        VteCursorShape::HollowBlock => CursorShape::HollowBlock,
        VteCursorShape::Hidden => CursorShape::Hidden,
    }
}

fn to_vte_cursor_shape(shape: CursorShape) -> VteCursorShape {
    match shape {
        CursorShape::Block => VteCursorShape::Block,
        CursorShape::Underline => VteCursorShape::Underline,
        CursorShape::Beam => VteCursorShape::Beam,
        CursorShape::HollowBlock => VteCursorShape::HollowBlock,
        CursorShape::Hidden => VteCursorShape::Hidden,
    }
}
