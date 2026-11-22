//! Bounded scrollback storage with epoch/seq tracking and per-line zstd compression.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use zstd::bulk::{Compressor, Decompressor};

use crate::types::{CellData, HyperlinkData, ScrollbackRow, Style};
use crate::{CoreError, HistoryError, HistorySlice};

/// Default capacity of lines retained in scrollback (design §4.3).
pub const DEFAULT_SCROLLBACK_LIMIT: usize = 20_000;

/// Number of most-recent scrollback rows kept uncompressed for fast access/reflow.
const HOT_CACHE_LINES: usize = 256;

/// Number of rows compressed together into a cold chunk to avoid per-line zstd overhead.
const COLD_CHUNK_LINES: usize = 64;

/// Max uncompressed payload budget when decompressing a cold chunk.
const MAX_CHUNK_DECOMPRESS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ColdChunk {
    first_seq: u64,
    row_count: usize,
    compressed: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredRowPayload {
    wrapped: bool,
    cells: Vec<CellData>,
    styles: Vec<Style>,
    hyperlinks: Vec<HyperlinkData>,
}

/// Serializable scrollback snapshot for process handover.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollbackSnapshot {
    pub epoch: u64,
    pub next_seq: u64,
    pub max_lines: usize,
    pub hot: Vec<ScrollbackRow>,
    cold: Vec<ColdChunk>,
}

/// Bounded scrollback ring storing recent lines uncompressed and older lines zstd-compressed in chunks.
pub struct ScrollbackStore {
    epoch: u64,
    next_seq: u64,
    max_lines: usize,
    hot: VecDeque<ScrollbackRow>,
    cold: VecDeque<ColdChunk>,
    cold_lines: usize,
    compressor: Compressor<'static>,
    decompressor: Decompressor<'static>,
}

impl std::fmt::Debug for ScrollbackStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScrollbackStore")
            .field("epoch", &self.epoch)
            .field("next_seq", &self.next_seq)
            .field("max_lines", &self.max_lines)
            .field("hot_len", &self.hot.len())
            .field("cold_chunks", &self.cold.len())
            .field("cold_lines", &self.cold_lines)
            .finish()
    }
}

impl ScrollbackStore {
    pub fn new(max_lines: usize) -> Result<Self, CoreError> {
        let compressor = Compressor::new(1)
            .map_err(|e| CoreError::Compression(format!("create zstd compressor: {e}")))?;
        let decompressor = Decompressor::new()
            .map_err(|e| CoreError::Compression(format!("create zstd decompressor: {e}")))?;
        Ok(Self {
            epoch: 0,
            next_seq: 0,
            max_lines,
            hot: VecDeque::new(),
            cold: VecDeque::new(),
            cold_lines: 0,
            compressor,
            decompressor,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn len(&self) -> usize {
        self.cold_lines + self.hot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cold_lines == 0 && self.hot.is_empty()
    }

    pub fn oldest_seq(&self) -> u64 {
        if let Some(first_cold) = self.cold.front() {
            first_cold.first_seq
        } else if let Some(first_hot) = self.hot.front() {
            first_hot.seq
        } else {
            self.next_seq
        }
    }

    /// Push a newly scrolled-out row, assigning the next monotonic sequence number.
    pub fn push_row(
        &mut self,
        wrapped: bool,
        cells: Vec<CellData>,
        styles: Vec<Style>,
        hyperlinks: Vec<HyperlinkData>,
    ) -> Result<u64, CoreError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        if self.max_lines == 0 {
            return Ok(seq);
        }

        let row = ScrollbackRow {
            seq,
            wrapped,
            cells,
            styles: if styles.is_empty() {
                vec![Style::default()]
            } else {
                styles
            },
            hyperlinks,
        };
        self.hot.push_back(row);

        let batch = COLD_CHUNK_LINES.min(self.max_lines.max(1));
        if self.hot.len() >= HOT_CACHE_LINES + batch {
            let drain_n = self.hot.len().saturating_sub(HOT_CACHE_LINES);
            let rows: Vec<ScrollbackRow> = self.hot.drain(..drain_n).collect();
            self.push_cold_chunk(rows)?;
        }
        self.enforce_limit()?;
        Ok(seq)
    }

    /// Remove up to `count` most-recent lines (used when growing terminal height pulls lines back
    /// from active history or during column reflow).
    pub fn pop_newest(&mut self, mut count: usize) -> Result<usize, CoreError> {
        let mut popped = 0;
        while count > 0 {
            if self.hot.pop_back().is_some() {
                popped += 1;
                count -= 1;
                self.next_seq = self.next_seq.saturating_sub(1);
                continue;
            }
            if let Some(chunk) = self.cold.pop_back() {
                self.cold_lines = self.cold_lines.saturating_sub(chunk.row_count);
                let mut rows = decompress_chunk(&mut self.decompressor, &chunk)?;
                let drop_here = count.min(rows.len());
                rows.truncate(rows.len() - drop_here);
                popped += drop_here;
                count -= drop_here;
                self.next_seq = self.next_seq.saturating_sub(drop_here as u64);
                self.hot.extend(rows);
                continue;
            }
            break;
        }
        Ok(popped)
    }

    /// Clear all stored history and advance the scrollback epoch (CSI 3 J semantics).
    pub fn clear(&mut self) -> u64 {
        self.hot.clear();
        self.cold.clear();
        self.cold_lines = 0;
        self.epoch = self.epoch.saturating_add(1);
        self.next_seq = 0;
        self.epoch
    }

    /// Retrieve a contiguous range of rows starting at `start_seq`.
    pub fn slice(
        &mut self,
        epoch: u64,
        start_seq: u64,
        count: usize,
    ) -> Result<HistorySlice, HistoryError> {
        if epoch != self.epoch {
            return Err(HistoryError::EpochMismatch {
                current_epoch: self.epoch,
                oldest_seq: self.oldest_seq(),
                next_seq: self.next_seq,
            });
        }
        let oldest = self.oldest_seq();
        let next = self.next_seq;
        if count == 0 || start_seq >= next || self.is_empty() {
            return Ok(HistorySlice {
                epoch: self.epoch,
                start_seq: start_seq.max(oldest),
                oldest_seq: oldest,
                next_seq: next,
                rows: Vec::new(),
            });
        }

        let effective_start = start_seq.max(oldest);
        let available = (next - effective_start) as usize;
        let take_count = count.min(available);
        let end_seq = effective_start + take_count as u64;
        let rows = self.read_range(effective_start, end_seq)?;

        Ok(HistorySlice {
            epoch: self.epoch,
            start_seq: effective_start,
            oldest_seq: oldest,
            next_seq: next,
            rows,
        })
    }

    /// Return the most recent `count` rows in scrollback.
    pub fn tail(&mut self, count: usize) -> Result<HistorySlice, CoreError> {
        let oldest = self.oldest_seq();
        let next = self.next_seq;
        let total = (next - oldest) as usize;
        let take = count.min(total);
        let start = next - take as u64;
        let rows = self.read_range(start, next)?;
        Ok(HistorySlice {
            epoch: self.epoch,
            start_seq: start,
            oldest_seq: oldest,
            next_seq: next,
            rows,
        })
    }

    pub fn to_snapshot(&self) -> ScrollbackSnapshot {
        ScrollbackSnapshot {
            epoch: self.epoch,
            next_seq: self.next_seq,
            max_lines: self.max_lines,
            hot: self.hot.iter().cloned().collect(),
            cold: self.cold.iter().cloned().collect(),
        }
    }

    pub fn from_snapshot(snap: ScrollbackSnapshot) -> Result<Self, CoreError> {
        let mut store = Self::new(snap.max_lines)?;
        store.epoch = snap.epoch;
        store.next_seq = snap.next_seq;
        store.hot = snap.hot.into();
        store.cold_lines = snap.cold.iter().map(|c| c.row_count).sum();
        store.cold = snap.cold.into();
        store.enforce_limit()?;
        Ok(store)
    }

    fn read_range(&mut self, start_seq: u64, end_seq: u64) -> Result<Vec<ScrollbackRow>, CoreError> {
        if start_seq >= end_seq || self.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end_seq - start_seq) as usize);
        let oldest = self.oldest_seq();
        let cold_end = self
            .cold
            .back()
            .map_or(oldest, |c| c.first_seq + c.row_count as u64);

        if start_seq < cold_end && !self.cold.is_empty() {
            for chunk in &self.cold {
                let chunk_start = chunk.first_seq;
                let chunk_end = chunk_start + chunk.row_count as u64;
                if chunk_end <= start_seq {
                    continue;
                }
                if chunk_start >= end_seq {
                    break;
                }
                let decoded = decompress_chunk(&mut self.decompressor, chunk)?;
                for row in decoded {
                    if row.seq >= start_seq && row.seq < end_seq {
                        out.push(row);
                    }
                }
            }
        }

        if end_seq > cold_end && !self.hot.is_empty() {
            let hot_first_seq = self.hot.front().map_or(cold_end, |r| r.seq);
            let hot_start = start_seq.max(hot_first_seq);
            let from_idx = hot_start.saturating_sub(hot_first_seq) as usize;
            let to_idx = end_seq.saturating_sub(hot_first_seq) as usize;
            for row in self.hot.iter().take(to_idx).skip(from_idx) {
                out.push(row.clone());
            }
        }

        Ok(out)
    }

    fn push_cold_chunk(&mut self, rows: Vec<ScrollbackRow>) -> Result<(), CoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let chunk = compress_chunk(&mut self.compressor, rows)?;
        self.cold_lines += chunk.row_count;
        self.cold.push_back(chunk);
        Ok(())
    }

    fn enforce_limit(&mut self) -> Result<(), CoreError> {
        while self.cold_lines + self.hot.len() > self.max_lines {
            let excess = self.cold_lines + self.hot.len() - self.max_lines;
            if let Some(front) = self.cold.pop_front() {
                self.cold_lines = self.cold_lines.saturating_sub(front.row_count);
                if excess < front.row_count {
                    let mut rows = decompress_chunk(&mut self.decompressor, &front)?;
                    rows.drain(..excess);
                    if !rows.is_empty() {
                        let chunk = compress_chunk(&mut self.compressor, rows)?;
                        self.cold_lines += chunk.row_count;
                        self.cold.push_front(chunk);
                    }
                }
            } else {
                self.hot.pop_front();
            }
        }
        Ok(())
    }
}

fn compress_chunk(
    compressor: &mut Compressor<'_>,
    rows: Vec<ScrollbackRow>,
) -> Result<ColdChunk, CoreError> {
    let first_seq = rows[0].seq;
    let row_count = rows.len();
    let payloads: Vec<StoredRowPayload> = rows
        .into_iter()
        .map(|r| StoredRowPayload {
            wrapped: r.wrapped,
            cells: r.cells,
            styles: r.styles,
            hyperlinks: r.hyperlinks,
        })
        .collect();
    let encoded = bincode::serialize(&payloads)
        .map_err(|e| CoreError::Serialization(format!("encode scrollback chunk: {e}")))?;
    let compressed = compressor
        .compress(&encoded)
        .map_err(|e| CoreError::Compression(format!("compress scrollback chunk: {e}")))?;
    Ok(ColdChunk {
        first_seq,
        row_count,
        compressed,
    })
}

fn decompress_chunk(
    decompressor: &mut Decompressor<'_>,
    chunk: &ColdChunk,
) -> Result<Vec<ScrollbackRow>, CoreError> {
    let raw = decompressor
        .decompress(&chunk.compressed, MAX_CHUNK_DECOMPRESS_BYTES)
        .map_err(|e| CoreError::Compression(format!("decompress scrollback chunk: {e}")))?;
    let payloads: Vec<StoredRowPayload> = bincode::deserialize(&raw)
        .map_err(|e| CoreError::Serialization(format!("decode scrollback chunk: {e}")))?;
    Ok(payloads
        .into_iter()
        .enumerate()
        .map(|(idx, payload)| ScrollbackRow {
            seq: chunk.first_seq + idx as u64,
            wrapped: payload.wrapped,
            cells: payload.cells,
            styles: payload.styles,
            hyperlinks: payload.hyperlinks,
        })
        .collect())
}
