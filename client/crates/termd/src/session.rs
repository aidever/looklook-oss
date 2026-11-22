//! Session lifecycle, PTY output ingestion, paced `looklook-term.v1` frame delivery,
//! multi-viewer resize ownership, disconnect resume, and §4.6 control operations.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use term_core::{AlacrittyCore, CoreConfig, CoreEvent, DEFAULT_SCROLLBACK_LIMIT, TerminalCore};
use term_proto::{
    CaptureMode, ClientMsg, CreateSessionReq, DeltaEncoder, FEATURE_TRZSZ, LegacyDownMsg, PaneInfo,
    ScreenSnapshot, ScrollbackRow, ServerMsg, SessionInfo, ViewState, ZstdStreamEncoder,
};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc};

use crate::keys::{encode_mouse_report, translate_tmux_keys};
use crate::pty::{PlatformPty, PtyEvent};

const RAW_RING_CAP: usize = 256 * 1024;
const MIN_RESIZE_COLS: u16 = 20;
const MIN_RESIZE_ROWS: u16 = 5;
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(100);
const ACTIVE_MIN_INTERVAL: Duration = Duration::from_millis(16);
const ACTIVE_MAX_INTERVAL: Duration = Duration::from_millis(100);
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(1000);
const RESUME_CACHE_TTL: Duration = Duration::from_secs(180);
/// 输入写进 PTY 后，程序有了输出、或者过了这么久，才在帧里算“处理过了”（`acked_input_seq`）。
/// 端上的本地预测（§5.7）据此判断回显对不对：太早确认会把“回显还在路上”当成“没回显”。
const ECHO_GRACE: Duration = Duration::from_millis(100);
/// trzsz 握手的开头（trzsz 各端都用这个前缀，后面跟 `[SRD]:版本`）
const TRZSZ_MAGIC: &[u8] = b"::TRZSZ:TRANSFER:";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[derive(Debug)]
struct ViewerMeta {
    client_id: String,
    cols: u16,
    rows: u16,
    attached_at: Instant,
    last_input_at: Option<Instant>,
    view_state: ViewState,
    last_input_seq: u64,
    tx_wake: Arc<Notify>,
    pending_events: Vec<CoreEvent>,
    pending_scrollback: Vec<ScrollbackRow>,
    /// `FEATURE_*`
    features: u32,
    /// 写进 PTY 了、还没算“处理过”的输入（序号、写入时间）
    unechoed: VecDeque<(u64, Instant)>,
    /// 帧里告诉端的“已处理到的输入序号”
    echo_seq: u64,
    /// trzsz 原始字节模式下要发给这个端的字节
    pending_raw: Vec<u8>,
}

impl ViewerMeta {
    /// 程序有输出了：之前写进去的输入都算处理过
    fn note_output(&mut self) {
        if !self.unechoed.is_empty() {
            self.unechoed.clear();
            self.echo_seq = self.echo_seq.max(self.last_input_seq);
        }
    }

    /// 过了宽限期的输入算处理过；返回下一个要到期的还要等多久
    fn expire_echo(&mut self, now: Instant) -> Option<Duration> {
        while let Some(&(seq, at)) = self.unechoed.front() {
            let age = now.saturating_duration_since(at);
            if age < ECHO_GRACE {
                return Some(ECHO_GRACE - age);
            }
            self.echo_seq = self.echo_seq.max(seq);
            self.unechoed.pop_front();
        }
        None
    }
}

/// trzsz 原始字节模式（§5.8）：PTY 输出不进内核，原样交给一个端（发起传输的那个）的 trzsz 处理
#[derive(Debug)]
struct RawTransfer {
    id: u32,
    owner: String,
    /// 发出去、端还没说处理过的字节；`base` 是它第一个字节的偏移（从这一段开始算）
    buf: VecDeque<u8>,
    base: u64,
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[derive(Debug)]
struct DetachedClientState {
    last_input_seq: u64,
    encoder: DeltaEncoder,
    detached_at: Instant,
}

#[derive(Clone, Debug)]
pub enum LegacyBroadcast {
    Output(Vec<u8>),
    Title(String),
    Exited { exit_code: Option<i32> },
}

pub struct SessionState {
    pub id: String,
    pub core: AlacrittyCore,
    pub pty: PlatformPty,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub created_at_ms: i64,
    pub last_output_at_ms: i64,
    pub last_input_at_ms: i64,
    pub raw_ring: VecDeque<u8>,
    pub legacy_tx: broadcast::Sender<LegacyBroadcast>,
    viewers: HashMap<String, ViewerMeta>,
    detached: HashMap<String, DetachedClientState>,
    desired_size: (u16, u16),
    pending_resize: Option<((u16, u16), Instant)>,
    next_conn_id: u64,
    raw: Option<RawTransfer>,
    raw_seq: u32,
    /// 上一段输出的末尾（trzsz 魔术串跨两次读的时候也认得出）
    magic_tail: Vec<u8>,
}

impl SessionState {
    fn push_raw_ring(&mut self, bytes: &[u8]) {
        if bytes.len() >= RAW_RING_CAP {
            self.raw_ring.clear();
            self.raw_ring
                .extend(&bytes[bytes.len() - RAW_RING_CAP..]);
            return;
        }
        let excess = (self.raw_ring.len() + bytes.len()).saturating_sub(RAW_RING_CAP);
        if excess > 0 {
            self.raw_ring.drain(..excess);
        }
        self.raw_ring.extend(bytes.iter().copied());
    }

    /// PTY 输出：平时喂给内核；认出 trzsz 握手、又有端能处理时，从握手起的字节交给那个端（§5.8）
    fn ingest_output(&mut self, bytes: &[u8]) {
        for v in self.viewers.values_mut() {
            v.note_output();
        }
        if let Some(raw) = self.raw.as_mut() {
            raw.buf.extend(bytes.iter().copied());
            if let Some(v) = self.viewers.get_mut(&raw.owner) {
                v.pending_raw.extend_from_slice(bytes);
                v.tx_wake.notify_one();
            }
            return;
        }
        let tail_len = self.magic_tail.len();
        let mut hay = std::mem::take(&mut self.magic_tail);
        hay.extend_from_slice(bytes);
        if let Some(pos) = find_bytes(&hay, TRZSZ_MAGIC) {
            if let Some(owner) = self.trzsz_owner() {
                // 魔术串跨在上一段里的那部分已经进了内核（屏幕上多几个字，trzsz 会重画这一行）
                self.core.feed_bytes(&bytes[..pos.saturating_sub(tail_len)]);
                let data = hay[pos..].to_vec();
                self.raw_seq = self.raw_seq.wrapping_add(1);
                let id = self.raw_seq;
                if let Some(v) = self.viewers.get_mut(&owner) {
                    v.pending_raw = data.clone();
                    v.tx_wake.notify_one();
                }
                self.raw = Some(RawTransfer { id, owner, buf: data.into(), base: 0 });
                return;
            }
        }
        self.core.feed_bytes(bytes);
        let keep = TRZSZ_MAGIC.len() - 1;
        self.magic_tail = hay[hay.len().saturating_sub(keep)..].to_vec();
    }

    /// 谁来处理 trzsz：支持它的端里最近打过字的（拖文件、敲 trz 的那个），都没打过字就取最近连上的
    fn trzsz_owner(&self) -> Option<String> {
        self.viewers
            .iter()
            .filter(|(_, v)| v.features & FEATURE_TRZSZ != 0)
            .max_by_key(|(_, v)| (v.last_input_at, v.attached_at))
            .map(|(k, _)| k.clone())
    }

    /// 端发来的原始字节模式进度。返回是否动了内核（要重画）
    fn handle_raw(&mut self, viewer_key: &str, id: u32, consumed: u64, write: &[u8], done: bool) -> bool {
        let Some(raw) = self.raw.as_mut().filter(|r| r.id == id && r.owner == viewer_key) else {
            // 过期的一段（已经结束了）：要写的字照样上屏
            if !write.is_empty() {
                self.core.feed_bytes(write);
                return true;
            }
            return false;
        };
        let drop_n = consumed.saturating_sub(raw.base).min(raw.buf.len() as u64);
        raw.buf.drain(..drop_n as usize);
        raw.base += drop_n;
        let mut fed = false;
        if !write.is_empty() {
            self.core.feed_bytes(write);
            fed = true;
        }
        if done {
            let rest: Vec<u8> = self.raw.take().map(|r| r.buf.into()).unwrap_or_default();
            if let Some(v) = self.viewers.get_mut(viewer_key) {
                v.pending_raw.clear();
            }
            if !rest.is_empty() {
                self.core.feed_bytes(&rest);
            }
            self.magic_tail.clear();
            fed = true;
        }
        fed
    }

    /// 处理 trzsz 的端走了：传输没法继续，回到正常模式（没处理的字节多半是编码后的文件内容，丢掉）
    fn drop_raw_owner(&mut self, viewer_key: &str) {
        if self.raw.as_ref().is_some_and(|r| r.owner == viewer_key) {
            self.raw = None;
            self.magic_tail.clear();
        }
    }

    fn alloc_conn_id(&mut self, prefix: &str) -> String {
        self.next_conn_id += 1;
        format!("{prefix}-{}", self.next_conn_id)
    }

    /// Recompute desired PTY dimensions using §4.9 policy: the active viewer with the latest input
    /// (or most recently attached viewer if none sent input yet) owns the session size.
    fn recompute_desired_size(&mut self) -> Option<(u16, u16)> {
        let mut best: Option<(&ViewerMeta, bool, Option<Instant>, Instant)> = None;
        for viewer in self.viewers.values() {
            if viewer.cols < MIN_RESIZE_COLS || viewer.rows < MIN_RESIZE_ROWS {
                continue;
            }
            let not_hidden = viewer.view_state != ViewState::Hidden;
            let replace = match best {
                None => true,
                Some((_, best_nh, best_in, best_att)) => {
                    if not_hidden != best_nh {
                        not_hidden
                    } else {
                        match (viewer.last_input_at, best_in) {
                            (Some(a), Some(b)) => a > b,
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            (None, None) => viewer.attached_at > best_att,
                        }
                    }
                }
            };
            if replace {
                best = Some((viewer, not_hidden, viewer.last_input_at, viewer.attached_at));
            }
        }
        let target = best.map(|(v, _, _, _)| (v.cols, v.rows))?;
        let current = self.core.size();
        if target != current {
            self.desired_size = target;
            self.pending_resize = Some((target, Instant::now()));
            Some(target)
        } else {
            self.pending_resize = None;
            None
        }
    }
}

pub struct Session {
    pub id: String,
    pub state: Mutex<SessionState>,
    resize_notify: Notify,
    dirty_gen: AtomicU64,
}

#[cfg(unix)]
pub struct HandoverRestore {
    pub id: String,
    pub pty: PlatformPty,
    pub core: AlacrittyCore,
    pub created_at_ms: i64,
    pub last_output_at_ms: i64,
    pub last_input_at_ms: i64,
    pub raw_ring: Vec<u8>,
    pub rx: mpsc::UnboundedReceiver<PtyEvent>,
}

impl Session {
    pub fn spawn(req: CreateSessionReq) -> Result<Arc<Self>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let cols = req.cols.max(MIN_RESIZE_COLS);
        let rows = req.rows.max(MIN_RESIZE_ROWS);
        let mut clean_req = req;
        clean_req.cols = cols;
        clean_req.rows = rows;

        let core = AlacrittyCore::new(CoreConfig {
            cols,
            rows,
            scrollback_limit: DEFAULT_SCROLLBACK_LIMIT,
        })?;
        let pty = PlatformPty::spawn(&clean_req, tx)?;
        let now = now_ms();
        let (legacy_tx, _) = broadcast::channel(256);

        let session = Arc::new(Self {
            id: clean_req.id.clone(),
            state: Mutex::new(SessionState {
                id: clean_req.id,
                core,
                pty,
                alive: true,
                exit_code: None,
                created_at_ms: now,
                last_output_at_ms: now,
                last_input_at_ms: now,
                raw_ring: VecDeque::with_capacity(64 * 1024),
                legacy_tx,
                viewers: HashMap::new(),
                detached: HashMap::new(),
                desired_size: (cols, rows),
                pending_resize: None,
                next_conn_id: 0,
                raw: None,
                raw_seq: 0,
                magic_tail: Vec::new(),
            }),
            resize_notify: Notify::new(),
            dirty_gen: AtomicU64::new(0),
        });

        spawn_session_loops(&session, rx);
        Ok(session)
    }

    #[cfg(unix)]
    pub fn from_handover(params: HandoverRestore) -> Arc<Self> {
        let size = params.core.size();
        let (legacy_tx, _) = broadcast::channel(256);
        let session = Arc::new(Self {
            id: params.id.clone(),
            state: Mutex::new(SessionState {
                id: params.id,
                core: params.core,
                pty: params.pty,
                alive: true,
                exit_code: None,
                created_at_ms: params.created_at_ms,
                last_output_at_ms: params.last_output_at_ms,
                last_input_at_ms: params.last_input_at_ms,
                raw_ring: params.raw_ring.into(),
                legacy_tx,
                viewers: HashMap::new(),
                detached: HashMap::new(),
                desired_size: size,
                pending_resize: None,
                next_conn_id: 0,
                raw: None,
                raw_seq: 0,
                magic_tail: Vec::new(),
            }),
            resize_notify: Notify::new(),
            dirty_gen: AtomicU64::new(0),
        });
        spawn_session_loops(&session, params.rx);
        session
    }

    pub fn notify_all_viewers_in_state(guard: &mut SessionState) {
        for v in guard.viewers.values() {
            v.tx_wake.notify_one();
        }
    }

    pub fn dirty_generation(&self) -> u64 {
        self.dirty_gen.load(Ordering::Acquire)
    }
}

fn spawn_session_loops(session: &Arc<Session>, mut rx: mpsc::UnboundedReceiver<PtyEvent>) {
    // Task 1: ingest PTY output into `term-core`, distribute events/scrollback, enforce DEC 2026
    // 100ms timeout.
    let s_read = Arc::clone(session);
    tokio::spawn(async move {
        let mut sync_timer = tokio::time::interval(Duration::from_millis(25));
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { break };
                    match ev {
                        PtyEvent::Output(mut bytes) => {
                            let mut closed_code: Option<Option<i32>> = None;
                            const MAX_COALESCE_BYTES: usize = 64 * 1024;
                            while bytes.len() < MAX_COALESCE_BYTES {
                                match rx.try_recv() {
                                    Ok(PtyEvent::Output(more)) => bytes.extend_from_slice(&more),
                                    Ok(PtyEvent::Closed { exit_code }) => {
                                        closed_code = Some(exit_code);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let mut guard = s_read.state.lock().await;
                            guard.last_output_at_ms = now_ms();
                            guard.push_raw_ring(&bytes);
                            guard.ingest_output(&bytes);
                            let _ = guard.legacy_tx.send(LegacyBroadcast::Output(bytes));
                            handle_core_side_effects(&mut guard);
                            if let Some(exit_code) = closed_code {
                                guard.alive = false;
                                guard.exit_code = guard.pty.poll_exit().flatten().or(exit_code);
                                let code = guard.exit_code;
                                let _ = guard.legacy_tx.send(LegacyBroadcast::Exited { exit_code: code });
                                s_read.dirty_gen.fetch_add(1, Ordering::Release);
                                Session::notify_all_viewers_in_state(&mut guard);
                                break;
                            }
                            if !guard.core.in_sync_update() {
                                s_read.dirty_gen.fetch_add(1, Ordering::Release);
                                Session::notify_all_viewers_in_state(&mut guard);
                            }
                        }
                        PtyEvent::Closed { exit_code } => {
                            let mut guard = s_read.state.lock().await;
                            guard.alive = false;
                            guard.exit_code = guard.pty.poll_exit().flatten().or(exit_code);
                            let code = guard.exit_code;
                            let _ = guard.legacy_tx.send(LegacyBroadcast::Exited { exit_code: code });
                            s_read.dirty_gen.fetch_add(1, Ordering::Release);
                            Session::notify_all_viewers_in_state(&mut guard);
                            break;
                        }
                    }
                }
                _ = sync_timer.tick() => {
                    let mut guard = s_read.state.lock().await;
                    if guard.core.check_sync_timeout(SYNC_UPDATE_TIMEOUT) {
                        handle_core_side_effects(&mut guard);
                        s_read.dirty_gen.fetch_add(1, Ordering::Release);
                        Session::notify_all_viewers_in_state(&mut guard);
                    }
                }
            }
        }
    });

    // Task 2: 150ms debounced multi-viewer resize task.
    let s_resize = Arc::clone(session);
    tokio::spawn(async move {
        loop {
            s_resize.resize_notify.notified().await;
            loop {
                let sleep_for = {
                    let mut guard = s_resize.state.lock().await;
                    let Some((target, since)) = guard.pending_resize else {
                        break;
                    };
                    let elapsed = since.elapsed();
                    if elapsed >= RESIZE_DEBOUNCE {
                        guard.pending_resize = None;
                        if target != guard.core.size()
                            && target.0 >= MIN_RESIZE_COLS
                            && target.1 >= MIN_RESIZE_ROWS
                        {
                            let _ = guard.pty.resize(target.0, target.1);
                            guard.core.resize(target.0, target.1);
                            s_resize.dirty_gen.fetch_add(1, Ordering::Release);
                            Session::notify_all_viewers_in_state(&mut guard);
                        }
                        break;
                    }
                    RESIZE_DEBOUNCE - elapsed
                };
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {}
                    _ = s_resize.resize_notify.notified() => {}
                }
            }
        }
    });
}

fn handle_core_side_effects(guard: &mut SessionState) {
    let events = guard.core.take_events();
    let mut forward_events = Vec::new();
    for ev in events {
        match ev {
            CoreEvent::PtyWrite(reply) => {
                let _ = guard.pty.write_all(&reply);
            }
            CoreEvent::Title(Some(ref t)) => {
                let _ = guard.legacy_tx.send(LegacyBroadcast::Title(t.clone()));
                forward_events.push(ev);
            }
            other => forward_events.push(other),
        }
    }
    let (epoch, new_rows) = guard.core.take_new_scrollback();
    let _ = epoch;
    for viewer in guard.viewers.values_mut() {
        viewer.pending_events.extend(forward_events.iter().cloned());
        if !new_rows.is_empty() {
            viewer.pending_scrollback.extend(new_rows.iter().cloned());
        }
    }
}

/// Manages all sessions in `termd` plus handover lock/drain state.
#[derive(Clone)]
pub struct SessionManager {
    pub inner: Arc<RwLock<HashMap<String, Arc<Session>>>>,
    pub handover_lock: Arc<StdMutex<bool>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            handover_lock: Arc::new(StdMutex::new(false)),
        }
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    pub async fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.inner.read().await.get(id).cloned()
    }

    pub async fn create_or_ensure(&self, req: CreateSessionReq) -> Result<SessionInfo> {
        if *self.handover_lock.lock().unwrap() {
            bail!("daemon is performing upgrade handover");
        }
        if let Some(existing) = self.get(&req.id).await {
            let guard = existing.state.lock().await;
            return Ok(session_info_of(&guard));
        }
        let session = Session::spawn(req.clone())?;
        let info = {
            let guard = session.state.lock().await;
            session_info_of(&guard)
        };
        let mut map = self.inner.write().await;
        if let Some(existing) = map.get(&req.id) {
            drop(session);
            let guard = existing.state.lock().await;
            return Ok(session_info_of(&guard));
        }
        map.insert(req.id, session);
        Ok(info)
    }

    pub async fn stop_session(&self, id: &str) -> bool {
        let removed = self.inner.write().await.remove(id);
        if let Some(session) = removed {
            let guard = session.state.lock().await;
            guard.pty.terminate();
            true
        } else {
            false
        }
    }

    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions: Vec<Arc<Session>> = self.inner.read().await.values().cloned().collect();
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            let guard = s.state.lock().await;
            out.push(session_info_of(&guard));
        }
        out
    }

    pub async fn get_pane(&self, id: &str) -> Option<PaneInfo> {
        let session = self.get(id).await?;
        let guard = session.state.lock().await;
        Some(pane_info_of(&guard))
    }

    pub async fn pane_pids(&self, id: &str) -> Vec<u32> {
        let Some(session) = self.get(id).await else {
            return Vec::new();
        };
        let guard = session.state.lock().await;
        guard.pty.inspect_processes().pids
    }

    pub async fn send_keys(&self, id: &str, keys: &[String]) -> Result<bool> {
        let Some(session) = self.get(id).await else {
            return Ok(false);
        };
        let mut guard = session.state.lock().await;
        let app_cursor = guard.core.modes().app_cursor;
        let bytes = translate_tmux_keys(keys, app_cursor);
        guard.last_input_at_ms = now_ms();
        guard.pty.write_all(&bytes)?;
        Ok(true)
    }

    pub async fn send_literal(&self, id: &str, data: &str) -> Result<bool> {
        let Some(session) = self.get(id).await else {
            return Ok(false);
        };
        let mut guard = session.state.lock().await;
        guard.last_input_at_ms = now_ms();
        guard.pty.write_all(data.as_bytes())?;
        Ok(true)
    }

    pub async fn paste(&self, id: &str, data: &str) -> Result<bool> {
        let Some(session) = self.get(id).await else {
            return Ok(false);
        };
        let mut guard = session.state.lock().await;
        let bracketed = guard.core.modes().bracketed_paste;
        guard.last_input_at_ms = now_ms();
        if bracketed {
            let mut framed = Vec::with_capacity(data.len() + 12);
            framed.extend_from_slice(b"\x1b[200~");
            framed.extend_from_slice(data.as_bytes());
            framed.extend_from_slice(b"\x1b[201~");
            guard.pty.write_all(&framed)?;
        } else {
            guard.pty.write_all(data.as_bytes())?;
        }
        Ok(true)
    }

    pub async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<bool> {
        if cols < MIN_RESIZE_COLS || rows < MIN_RESIZE_ROWS {
            return Ok(false);
        }
        let Some(session) = self.get(id).await else {
            return Ok(false);
        };
        let mut guard = session.state.lock().await;
        guard.pty.resize(cols, rows)?;
        guard.core.resize(cols, rows);
        session.dirty_gen.fetch_add(1, Ordering::Release);
        Session::notify_all_viewers_in_state(&mut guard);
        Ok(true)
    }

    pub async fn capture(&self, id: &str, mode: CaptureMode) -> Result<Option<String>> {
        let Some(session) = self.get(id).await else {
            return Ok(None);
        };
        let mut guard = session.state.lock().await;
        let text = match mode {
            CaptureMode::ScreenAnsi => guard.core.capture_screen_ansi(),
            CaptureMode::RecentAnsi { lines } => guard.core.capture_history_ansi(Some(lines))?,
            CaptureMode::FullAnsi => guard.core.capture_history_ansi(None)?,
            CaptureMode::PlainText { include_scrollback } => {
                guard.core.capture_plain_text(include_scrollback)?
            }
        };
        Ok(Some(text))
    }

    pub async fn history(
        &self,
        id: &str,
        epoch: Option<u64>,
        start_seq: Option<u64>,
        count: Option<u32>,
        tail_lines: Option<u32>,
    ) -> Result<(Option<term_proto::HistorySlice>, bool)> {
        let Some(session) = self.get(id).await else {
            return Ok((None, false));
        };
        let mut guard = session.state.lock().await;
        if let Some(tail) = tail_lines {
            let slice = guard.core.tail_history(tail as usize)?;
            return Ok((Some(slice), false));
        }
        let cur_epoch = guard.core.scrollback_info().0;
        let req_epoch = epoch.unwrap_or(cur_epoch);
        match guard.core.history_slice(
            req_epoch,
            start_seq.unwrap_or(0),
            count.unwrap_or(200) as usize,
        ) {
            Ok(slice) => Ok((Some(slice), false)),
            Err(term_core::HistoryError::EpochMismatch {
                current_epoch,
                oldest_seq,
                next_seq,
            }) => Ok((
                Some(term_proto::HistorySlice {
                    epoch: current_epoch,
                    start_seq: oldest_seq,
                    oldest_seq,
                    next_seq,
                    rows: Vec::new(),
                }),
                true,
            )),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn output_activity(&self) -> Vec<(String, i64)> {
        let sessions: Vec<Arc<Session>> = self.inner.read().await.values().cloned().collect();
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            let guard = s.state.lock().await;
            out.push((guard.id.clone(), guard.last_output_at_ms));
        }
        out
    }

    pub async fn list_orphans(&self, known_ids: &[String]) -> Vec<String> {
        let known: HashSet<&str> = known_ids.iter().map(String::as_str).collect();
        self.inner
            .read()
            .await
            .keys()
            .filter(|id| !known.contains(id.as_str()))
            .cloned()
            .collect()
    }

    pub async fn sweep_orphans(&self, keep_ids: &[String]) -> Vec<String> {
        let orphans = self.list_orphans(keep_ids).await;
        let mut stopped = Vec::new();
        for id in orphans {
            if self.stop_session(&id).await {
                stopped.push(id);
            }
        }
        stopped
    }
}

fn session_info_of(guard: &SessionState) -> SessionInfo {
    let inspect = guard.pty.inspect_processes();
    let (cols, rows) = guard.core.size();
    SessionInfo {
        id: guard.id.clone(),
        title: guard.core.title().map(str::to_owned),
        cwd: guard.core.cwd().map(str::to_owned).or(inspect.cwd),
        cols,
        rows,
        alive: guard.alive && inspect.alive,
        exit_code: guard.exit_code,
        child_pid: guard.pty.child_pid(),
        fg_pid: inspect.fg_pid,
        fg_process: inspect.fg_process,
        created_at_ms: guard.created_at_ms,
        last_output_at_ms: guard.last_output_at_ms,
        last_input_at_ms: guard.last_input_at_ms,
        viewer_count: guard.viewers.len(),
    }
}

fn pane_info_of(guard: &SessionState) -> PaneInfo {
    let inspect = guard.pty.inspect_processes();
    let (cols, rows) = guard.core.size();
    let modes = guard.core.modes();
    PaneInfo {
        id: guard.id.clone(),
        alive: guard.alive && inspect.alive,
        exit_code: guard.exit_code,
        pid: guard.pty.child_pid(),
        fg_pid: inspect.fg_pid,
        fg_process: inspect.fg_process,
        cwd: guard.core.cwd().map(str::to_owned).or(inspect.cwd),
        title: guard.core.title().map(str::to_owned),
        cols,
        rows,
        last_activity_at_ms: guard.last_output_at_ms.max(guard.last_input_at_ms),
        alt_screen: modes.alt_screen,
        mouse_active: modes.mouse_protocol != term_proto::MouseProtocol::None,
    }
}

/// Interactive `looklook-term.v1` viewer state attached to a session.
pub struct ViewerAttachment {
    pub viewer_key: String,
    pub client_id: String,
    pub encoder: DeltaEncoder,
    pub zstd: ZstdStreamEncoder,
    pub wake: Arc<Notify>,
    pub last_sent_frame: u64,
    pub last_acked_frame: u64,
    pub last_sent_at: Instant,
    pub min_interval: Duration,
    pub view_state: ViewState,
    pub last_seen_gen: u64,
    pub force_immediate_frame: bool,
    /// 上一帧里的 `acked_input_seq`
    pub last_sent_echo: u64,
}

impl Session {
    pub async fn attach_viewer(
        self: &Arc<Self>,
        hello: ClientMsg,
    ) -> Result<ViewerAttachment, String> {
        let ClientMsg::Hello {
            protocol,
            cols,
            rows,
            client_id,
            last_frame,
            last_input_seq,
        } = hello
        else {
            return Err("first message must be Hello".into());
        };
        if protocol != term_proto::PROTOCOL_VERSION {
            return Err(format!("unsupported protocol {protocol}"));
        }

        let wake = Arc::new(Notify::new());
        let mut guard = self.state.lock().await;
        // Purge stale resume entries.
        guard
            .detached
            .retain(|_, st| st.detached_at.elapsed() < RESUME_CACHE_TTL);

        let stable_id = client_id.unwrap_or_else(|| guard.alloc_conn_id("c"));
        let viewer_key = if guard.viewers.contains_key(&stable_id) {
            guard.alloc_conn_id(&stable_id)
        } else {
            stable_id.clone()
        };

        let mut encoder = DeltaEncoder::new();
        let mut resumed_input_seq = last_input_seq;

        if let Some(prev) = guard.detached.remove(&stable_id) {
            resumed_input_seq = resumed_input_seq.max(prev.last_input_seq);
            if last_frame == Some(prev.encoder.current_frame()) {
                encoder = prev.encoder;
            }
        }

        guard.viewers.insert(
            viewer_key.clone(),
            ViewerMeta {
                client_id: stable_id.clone(),
                cols,
                rows,
                attached_at: Instant::now(),
                last_input_at: None,
                view_state: ViewState::Active,
                last_input_seq: resumed_input_seq,
                tx_wake: Arc::clone(&wake),
                pending_events: Vec::new(),
                pending_scrollback: Vec::new(),
                features: 0,
                unechoed: VecDeque::new(),
                echo_seq: resumed_input_seq,
                pending_raw: Vec::new(),
            },
        );
        if guard.recompute_desired_size().is_some() {
            // Apply initial valid attach size immediately if no other viewers already own it.
            if guard.viewers.len() == 1
                && cols >= MIN_RESIZE_COLS
                && rows >= MIN_RESIZE_ROWS
                && guard.core.size() != (cols, rows)
            {
                guard.pending_resize = None;
                let _ = guard.pty.resize(cols, rows);
                guard.core.resize(cols, rows);
            } else {
                self.resize_notify.notify_one();
            }
        }

        let encoder_zstd = ZstdStreamEncoder::new().map_err(|e| e.to_string())?;
        drop(guard);

        Ok(ViewerAttachment {
            viewer_key,
            client_id: stable_id,
            last_sent_frame: encoder.current_frame(),
            last_acked_frame: encoder.current_frame(),
            encoder,
            zstd: encoder_zstd,
            wake,
            last_sent_at: Instant::now() - ACTIVE_MAX_INTERVAL,
            min_interval: ACTIVE_MIN_INTERVAL,
            view_state: ViewState::Active,
            last_seen_gen: u64::MAX,
            force_immediate_frame: true,
            last_sent_echo: 0,
        })
    }

    pub async fn detach_viewer(self: &Arc<Self>, mut att: ViewerAttachment) {
        let mut guard = self.state.lock().await;
        guard.drop_raw_owner(&att.viewer_key);
        if let Some(meta) = guard.viewers.remove(&att.viewer_key) {
            guard.detached.insert(
                meta.client_id.clone(),
                DetachedClientState {
                    last_input_seq: meta.last_input_seq,
                    encoder: std::mem::take(&mut att.encoder),
                    detached_at: Instant::now(),
                },
            );
            if guard.recompute_desired_size().is_some() {
                self.resize_notify.notify_one();
            }
        }
    }

    /// Process an inbound `ClientMsg` from an attached viewer; returns immediate `ServerMsg` reply
    /// if the message requires an out-of-band response (such as `HistoryChunk`).
    pub async fn handle_viewer_msg(
        self: &Arc<Self>,
        att: &mut ViewerAttachment,
        msg: ClientMsg,
    ) -> Result<Option<ServerMsg>> {
        match msg {
            ClientMsg::Hello { .. } => Ok(None),
            ClientMsg::Input { seq, data } => {
                let mut guard = self.state.lock().await;
                let now = Instant::now();
                let should_write = if let Some(meta) = guard.viewers.get_mut(&att.viewer_key) {
                    if seq <= meta.last_input_seq && meta.last_input_seq > 0 {
                        false
                    } else {
                        meta.last_input_seq = seq;
                        meta.last_input_at = Some(now);
                        meta.unechoed.push_back((seq, now));
                        true
                    }
                } else {
                    true
                };
                if should_write {
                    guard.last_input_at_ms = now_ms();
                    guard.pty.write_all(&data)?;
                    // 宽限期到了要发一帧（没回显时端上靠这一帧撤掉预测）
                    att.wake.notify_one();
                    if guard.recompute_desired_size().is_some() {
                        self.resize_notify.notify_one();
                    }
                }
                Ok(None)
            }
            ClientMsg::Resize { cols, rows } => {
                let mut guard = self.state.lock().await;
                if let Some(meta) = guard.viewers.get_mut(&att.viewer_key) {
                    meta.cols = cols;
                    meta.rows = rows;
                    if meta.last_input_at.is_none() {
                        meta.last_input_at = Some(Instant::now());
                    }
                }
                if guard.recompute_desired_size().is_some() {
                    self.resize_notify.notify_one();
                }
                Ok(None)
            }
            ClientMsg::Ack { frame } => {
                if frame > att.last_acked_frame {
                    att.last_acked_frame = frame;
                    if frame == att.last_sent_frame {
                        let rtt = att.last_sent_at.elapsed();
                        att.min_interval = rtt.clamp(ACTIVE_MIN_INTERVAL, ACTIVE_MAX_INTERVAL);
                    }
                    att.wake.notify_one();
                }
                Ok(None)
            }
            ClientMsg::View { state } => {
                att.view_state = state;
                let mut guard = self.state.lock().await;
                if let Some(meta) = guard.viewers.get_mut(&att.viewer_key) {
                    meta.view_state = state;
                }
                if state != ViewState::Hidden {
                    att.force_immediate_frame = true;
                    att.wake.notify_one();
                }
                Ok(None)
            }
            ClientMsg::Mouse {
                kind,
                button,
                col,
                row,
                modifiers,
                raw,
            } => {
                let guard = self.state.lock().await;
                let bytes = if !raw.is_empty() {
                    Some(raw)
                } else {
                    let modes = guard.core.modes();
                    encode_mouse_report(
                        kind,
                        button,
                        col,
                        row,
                        modifiers,
                        modes.mouse_protocol,
                        modes.mouse_encoding,
                    )
                };
                if let Some(bytes) = bytes {
                    guard.pty.write_all(&bytes)?;
                }
                Ok(None)
            }
            ClientMsg::Features { flags } => {
                let mut guard = self.state.lock().await;
                if let Some(meta) = guard.viewers.get_mut(&att.viewer_key) {
                    meta.features = flags;
                }
                Ok(None)
            }
            ClientMsg::Raw { id, consumed, write, done } => {
                let mut guard = self.state.lock().await;
                if guard.handle_raw(&att.viewer_key, id, consumed, &write, done) {
                    handle_core_side_effects(&mut guard);
                    self.dirty_gen.fetch_add(1, Ordering::Release);
                    Session::notify_all_viewers_in_state(&mut guard);
                }
                Ok(None)
            }
            ClientMsg::HistoryReq {
                request_id,
                epoch,
                start_seq,
                count,
            } => {
                let mut guard = self.state.lock().await;
                match guard.core.history_slice(epoch, start_seq, count as usize) {
                    Ok(slice) => Ok(Some(ServerMsg::HistoryChunk {
                        request_id,
                        slice,
                        epoch_mismatch: false,
                    })),
                    Err(term_core::HistoryError::EpochMismatch {
                        current_epoch,
                        oldest_seq,
                        next_seq,
                    }) => Ok(Some(ServerMsg::HistoryChunk {
                        request_id,
                        slice: term_proto::HistorySlice {
                            epoch: current_epoch,
                            start_seq: oldest_seq,
                            oldest_seq,
                            next_seq,
                            rows: Vec::new(),
                        },
                        epoch_mismatch: true,
                    })),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Check if this viewer can send a paced screen frame (`Snapshot`/`Delta`) + pending
    /// scrollback/events right now. Returns compressed zstd chunks ready for `TermStreamDown`
    /// plus optional next wakeup delay if throttled.
    pub async fn step_viewer_output(
        self: &Arc<Self>,
        att: &mut ViewerAttachment,
    ) -> Result<(Vec<Vec<u8>>, Option<Duration>)> {
        let mut chunks = Vec::new();
        let mut guard = self.state.lock().await;
        // trzsz 的原始字节不受发帧节奏和“看不见”的限制（切到别的标签页时传输照常）
        let raw_id = guard.raw.as_ref().map_or(0, |r| r.id);
        if let Some(m) = guard.viewers.get_mut(&att.viewer_key) {
            if !m.pending_raw.is_empty() {
                let data = std::mem::take(&mut m.pending_raw);
                chunks.push(att.zstd.encode_server_msg(&ServerMsg::Raw { id: raw_id, data })?);
            }
        }
        if att.view_state == ViewState::Hidden {
            return Ok((chunks, None));
        }

        let current_gen = self.dirty_generation();
        let in_flight = att.last_sent_frame > att.last_acked_frame;

        let meta = guard.viewers.get_mut(&att.viewer_key);
        let (pending_events, pending_scrollback, input_seq, echo_wait) = if let Some(m) = meta {
            let wait = m.expire_echo(Instant::now());
            (
                std::mem::take(&mut m.pending_events),
                std::mem::take(&mut m.pending_scrollback),
                m.echo_seq,
                wait,
            )
        } else {
            (Vec::new(), Vec::new(), 0, None)
        };

        if !pending_scrollback.is_empty() {
            let epoch = guard.core.scrollback_info().0;
            let msg = ServerMsg::Scrolled {
                epoch,
                rows: pending_scrollback,
            };
            chunks.push(att.zstd.encode_server_msg(&msg)?);
        }
        for ev in pending_events {
            let msg = ServerMsg::Event(ev);
            chunks.push(att.zstd.encode_server_msg(&msg)?);
        }

        let dirty =
            att.force_immediate_frame || current_gen != att.last_seen_gen || input_seq != att.last_sent_echo;
        if !dirty {
            return Ok((chunks, echo_wait));
        }
        if guard.core.in_sync_update() && !att.force_immediate_frame {
            return Ok((chunks, Some(Duration::from_millis(25))));
        }

        // Strict Ack-gating: only emit next screen frame after previous frame has been Acked.
        if in_flight {
            return Ok((chunks, echo_wait));
        }

        let min_interval = match att.view_state {
            ViewState::Active => att.min_interval,
            ViewState::Background => BACKGROUND_INTERVAL,
            ViewState::Hidden => return Ok((chunks, None)),
        };
        let elapsed = att.last_sent_at.elapsed();
        if !att.force_immediate_frame && elapsed < min_interval {
            let wait = min_interval - elapsed;
            return Ok((chunks, Some(echo_wait.map_or(wait, |e| e.min(wait)))));
        }

        let snap: ScreenSnapshot = guard.core.snapshot();
        let frame_msg = att.encoder.encode_frame(&snap, input_seq);
        att.last_sent_frame = att.encoder.current_frame();
        att.last_sent_at = Instant::now();
        att.last_seen_gen = current_gen;
        att.force_immediate_frame = false;
        att.last_sent_echo = input_seq;

        chunks.push(att.zstd.encode_server_msg(&frame_msg)?);
        Ok((chunks, echo_wait))
    }

    pub async fn subscribe_legacy(
        self: &Arc<Self>,
        conn_key: &str,
        client_id: String,
        cols: u16,
        rows: u16,
    ) -> (LegacyDownMsg, broadcast::Receiver<LegacyBroadcast>) {
        let mut guard = self.state.lock().await;
        let rx = guard.legacy_tx.subscribe();
        let init = LegacyDownMsg::Init {
            backlog: guard.raw_ring.iter().copied().collect(),
            title: guard.core.title().map(str::to_owned),
        };
        guard.viewers.insert(
            conn_key.to_string(),
            ViewerMeta {
                client_id,
                cols,
                rows,
                attached_at: Instant::now(),
                last_input_at: None,
                view_state: ViewState::Active,
                last_input_seq: 0,
                tx_wake: Arc::new(Notify::new()),
                pending_events: Vec::new(),
                pending_scrollback: Vec::new(),
                features: 0,
                unechoed: VecDeque::new(),
                echo_seq: 0,
                pending_raw: Vec::new(),
            },
        );
        if guard.recompute_desired_size().is_some() {
            if guard.viewers.len() == 1
                && cols >= MIN_RESIZE_COLS
                && rows >= MIN_RESIZE_ROWS
                && guard.core.size() != (cols, rows)
            {
                guard.pending_resize = None;
                let _ = guard.pty.resize(cols, rows);
                guard.core.resize(cols, rows);
            } else {
                self.resize_notify.notify_one();
            }
        }
        (init, rx)
    }

    pub async fn unsubscribe_legacy(self: &Arc<Self>, conn_key: &str) {
        let mut guard = self.state.lock().await;
        guard.viewers.remove(conn_key);
        if guard.recompute_desired_size().is_some() {
            self.resize_notify.notify_one();
        }
    }

    pub async fn note_legacy_input(self: &Arc<Self>, conn_key: &str) {
        let mut guard = self.state.lock().await;
        guard.last_input_at_ms = now_ms();
        if let Some(v) = guard.viewers.get_mut(conn_key) {
            v.last_input_at = Some(Instant::now());
        }
        if guard.recompute_desired_size().is_some() {
            self.resize_notify.notify_one();
        }
    }

    pub async fn note_legacy_resize(self: &Arc<Self>, conn_key: &str, cols: u16, rows: u16) {
        let mut guard = self.state.lock().await;
        if let Some(v) = guard.viewers.get_mut(conn_key) {
            v.cols = cols;
            v.rows = rows;
            if v.last_input_at.is_none() {
                v.last_input_at = Some(Instant::now());
            }
        }
        if guard.recompute_desired_size().is_some() {
            self.resize_notify.notify_one();
        }
    }
}
