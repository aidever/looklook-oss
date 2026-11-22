//! `looklook attach <终端>`（设计稿 §8.3）：在当前终端窗口里接上一个 termd 会话。
//!
//! - 连本机看看主进程的 `/i/{id}/term`（和浏览器同一条路，`looklook-term.v1`）。本机回环本来就是受信任的
//!   （管理台在本机打开不用登录），所以不另发令牌；App 用原生终端打开会话时跑的也是这个命令；
//! - 画法见 [`render`]：滚出去的行进本地终端自己的回滚区，用本地终端的滚动、搜索、选字；
//! - 键盘、鼠标、粘贴的字节原样发给程序（程序开的鼠标、括号粘贴等模式已经照搬到本地终端）；
//! - 断开：关掉窗口，或者在行首按 `~.`（和 ssh 一样；`~~` 输入一个 `~`）。会话照旧在 termd 里跑；
//! - 不是 termd 会话（还在 tmux 上）时，Linux / macOS 直接接上随包的 tmux；
//! - 会话在同一账户的另一台电脑上（`--device`）：还是连本机主进程，地址带 `__lldev`，主进程经 WebRTC 转给那台电脑
//!   （`gateway::to_peer`，和管理台里打开别的电脑上的终端同一条路）；
//! - 本地预测回显（`term_proto::predict`，和网页终端同一套）：往返慢时打的字先画出来，termd 确认后以它的画面为准。

mod render;

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use term_proto::predict::Predictor;
use term_proto::{ClientMsg, ClientScreen, CoreEvent, ScreenSnapshot, ServerMsg, ZstdStreamDecoder, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub use render::Painter;

/// 连上时先取这么多行回滚，打进本地回滚区
const INITIAL_HISTORY: u32 = 2000;

/// 排查用：设了 LOOKLOOK_ATTACH_LOG 时把收发的消息记到那个文件（终端在原始模式，不能往屏幕上打）
fn debug(msg: impl FnOnce() -> String) {
    if let Some(path) = std::env::var_os("LOOKLOOK_ATTACH_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", msg());
        }
    }
}

fn encode(msg: &ClientMsg) -> Message {
    Message::Binary(term_proto::encode_client_msg(msg).unwrap_or_default().into())
}

/// 名字或 id → 终端 id（要在运行）
fn pick(list: &serde_json::Value, target: &str) -> Result<String> {
    let items = list.as_array().cloned().unwrap_or_default();
    let name_of = |v: &serde_json::Value| v["name"].as_str().unwrap_or("").to_string();
    let exact: Vec<_> = items.iter().filter(|v| v["id"].as_str() == Some(target) || name_of(v) == target).collect();
    let found: Vec<_> = if exact.is_empty() {
        items.iter().filter(|v| name_of(v).starts_with(target) || v["id"].as_str().is_some_and(|i| i.starts_with(target))).collect()
    } else {
        exact
    };
    match found.as_slice() {
        [one] => {
            if one["running"].as_bool() != Some(true) {
                bail!("终端“{}”没有在运行，先在看看里启动它 / The terminal isn't running; start it in Looklook first", name_of(one));
            }
            Ok(one["id"].as_str().unwrap_or_default().to_string())
        }
        [] => bail!("没有叫“{target}”的终端（looklook attach 不带参数列出全部）/ No terminal named “{target}”"),
        many => {
            let names: Vec<String> = many.iter().map(|v| format!("{} ({})", name_of(v), v["id"].as_str().unwrap_or(""))).collect();
            bail!("有多个终端对得上，请写全名或 id / Several terminals match: {}", names.join(", "))
        }
    }
}

/// 列出终端（不带参数时）
pub fn print_list(list: &serde_json::Value) {
    for v in list.as_array().into_iter().flatten() {
        let state = if v["running"].as_bool() == Some(true) { "running" } else { "stopped" };
        println!("{:<10} {:<8} {}", v["id"].as_str().unwrap_or(""), state, v["name"].as_str().unwrap_or(""));
    }
}

/// 预测回显开不开
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum PredictMode {
    /// 往返慢（> 30 ms）时开
    Auto,
    On,
    Off,
}

fn need_tty() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("looklook attach 要在终端窗口里运行 / looklook attach must run in a terminal");
    }
    Ok(())
}

async fn connect(url: &str) -> std::result::Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = url.into_client_request()?;
    req.headers_mut().insert("Sec-WebSocket-Protocol", PROTOCOL_VERSION.parse().expect("header"));
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

fn status_of(e: &tokio_tungstenite::tungstenite::Error) -> Option<u16> {
    match e {
        tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status().as_u16()),
        _ => None,
    }
}

/// 设备 id、会话 id 只能是这些字符（拼进地址里）
fn plain_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub async fn run(addr: String, list: &serde_json::Value, target: &str, mux: Option<PathBuf>, predict: PredictMode) -> Result<i32> {
    let id = pick(list, target)?;
    need_tty()?;
    let ws = match connect(&format!("ws://{addr}/i/{id}/term")).await {
        Ok(ws) => ws,
        // 还在 tmux 上的会话（没开 termd）：直接接上随包的 tmux
        Err(e) if status_of(&e) == Some(404) => return attach_tmux(&id, mux),
        Err(e) => return Err(e).context("连不上看看客户端 / Can't reach the Looklook client"),
    };
    attached(ws, predict, false).await
}

/// 接上另一台电脑上的会话：连本机主进程，地址带 `__lldev`，由主进程转过去
pub async fn run_remote(addr: String, device: &str, id: &str, predict: PredictMode) -> Result<i32> {
    if !plain_id(device) || !plain_id(id) {
        bail!("设备或会话 id 不对 / Bad device or session id");
    }
    need_tty()?;
    eprintln!("[looklook] 正在连接那台电脑… / Connecting to the other computer…");
    let ws = match connect(&format!("ws://{addr}/i/{id}/term?__lldev={device}")).await {
        Ok(ws) => ws,
        Err(e) if status_of(&e) == Some(404) => {
            bail!("那台电脑上这个会话不在 termd 上（或那台电脑的看看太旧），只能在看看里打开 / The session there doesn't run on termd; open it in Looklook")
        }
        Err(e) => return Err(e).context("连不上那台电脑 / Can't reach the other computer"),
    };
    attached(ws, predict, true).await
}

async fn attached(ws: Ws, predict: PredictMode, remote: bool) -> Result<i32> {
    let raw = tty::RawMode::enter()?;
    let res = session(ws, predict, remote).await;
    drop(raw);
    match res {
        Ok(Exit::Detached) => {
            eprintln!("[looklook] 已断开，会话还在运行 / Detached; the session keeps running");
            Ok(0)
        }
        Ok(Exit::Ended) => {
            eprintln!("[looklook] 会话已结束 / The session has ended");
            Ok(0)
        }
        Ok(Exit::Lost) => {
            eprintln!("[looklook] 连接断了 / Connection lost");
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn attach_tmux(id: &str, mux: Option<PathBuf>) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let mux = mux.context("这个终端不在 termd 上，也找不到 tmux / Not a termd session and no tmux found")?;
    let err = std::process::Command::new(mux)
        .args(["-L", "looklook", "attach-session", "-t", &crate::instances::Instances::session_name(id)])
        .exec();
    Err(err.into())
}

#[cfg(not(unix))]
fn attach_tmux(_id: &str, _mux: Option<PathBuf>) -> Result<i32> {
    bail!("这个终端还在旧的会话方式上，打开设置里的 termd 后新建的终端才能接上 / This terminal doesn't run on termd")
}

enum Exit {
    Detached,
    Ended,
    Lost,
}

/// `~.` 断开：只在行首（上一次输入以回车结尾，或刚连上）认
struct Escape {
    at_line_start: bool,
    tilde: bool,
}

impl Escape {
    /// 返回要发给程序的字节；`None` 表示要断开
    fn filter(&mut self, input: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.tilde {
                self.tilde = false;
                match b {
                    b'.' => return None,
                    b'~' => {
                        out.push(b'~');
                        self.at_line_start = false;
                        continue;
                    }
                    _ => out.push(b'~'),
                }
            } else if self.at_line_start && b == b'~' {
                self.tilde = true;
                continue;
            }
            out.push(b);
            self.at_line_start = b == b'\r' || b == b'\n';
        }
        Some(out)
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 往返时间：从发出输入到帧里确认它（termd 写进 PTY、程序有了回显）。取最近几次里最小的，
/// 不让偶尔一次没回显（要等 termd 的 ECHO_GRACE）把它拉高。
struct Rtt {
    sent: std::collections::VecDeque<(u64, std::time::Instant)>,
    samples: std::collections::VecDeque<Duration>,
}

impl Rtt {
    const KEEP: usize = 8;

    fn new() -> Self {
        Rtt { sent: Default::default(), samples: Default::default() }
    }

    fn sent(&mut self, seq: u64) {
        if self.sent.len() >= 64 {
            self.sent.pop_front();
        }
        self.sent.push_back((seq, std::time::Instant::now()));
    }

    fn acked(&mut self, acked: u64) {
        let mut last = None;
        while self.sent.front().is_some_and(|(s, _)| *s <= acked) {
            last = self.sent.pop_front();
        }
        if let Some((_, t)) = last {
            if self.samples.len() >= Self::KEEP {
                self.samples.pop_front();
            }
            self.samples.push_back(t.elapsed());
        }
    }

    fn estimate(&self) -> Option<Duration> {
        self.samples.iter().min().copied()
    }
}

/// 自动模式下要不要预测：> 30 ms 开，< 20 ms 关（中间保持，免得来回跳）
fn auto_predict(rtt: Option<Duration>, now: bool) -> bool {
    match rtt {
        Some(d) if d > Duration::from_millis(30) => true,
        Some(d) if d < Duration::from_millis(20) => false,
        _ => now,
    }
}

/// 把预测的字写进画面（拷一份），光标挪到最后一个预测后面；交给 [`Painter`] 画，
/// 猜对了 termd 的画面和它一样、不用再画，猜错了下一帧自然改回来。
fn with_predictions<'a>(s: &'a ScreenSnapshot, p: &Predictor) -> std::borrow::Cow<'a, ScreenSnapshot> {
    if p.predictions().is_empty() {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut s = s.clone();
    for x in p.predictions() {
        if let Some(cell) = s.lines.get_mut(x.row as usize).and_then(|l| l.cells.get_mut(x.col as usize)) {
            if cell.width == 1 {
                cell.c = x.c;
                cell.extra.clear();
            }
        }
    }
    if let Some((row, col)) = p.cursor() {
        s.cursor.row = row;
        s.cursor.col = col.min(s.cols.saturating_sub(1));
    }
    std::borrow::Cow::Owned(s)
}

async fn session(ws: Ws, predict: PredictMode, remote: bool) -> Result<Exit> {
    let (mut tx, mut rx) = ws.split();
    let (cols, rows) = tty::size();
    tx.send(encode(&ClientMsg::Hello {
        protocol: PROTOCOL_VERSION.into(),
        cols,
        rows,
        client_id: None,
        last_frame: None,
        last_input_seq: 0,
    }))
    .await?;

    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
    tty::spawn_stdin(in_tx);
    let mut resized = tty::resize_events();

    let mut zdec = ZstdStreamDecoder::new()?;
    let mut screen = ClientScreen::new();
    let mut painter = Painter::new(cols, rows);
    let mut size = (cols, rows);
    let mut seq = 0u64;
    let mut esc = Escape { at_line_start: true, tilde: false };
    let mut pred = Predictor::default();
    let mut rtt = Rtt::new();
    // 自动模式：另一台电脑上的会话先开着（经 WebRTC 多半够慢），本机的先关着，按量到的往返改
    pred.set_enabled(match predict {
        PredictMode::On => true,
        PredictMode::Off => false,
        PredictMode::Auto => remote,
    }, 0);
    // 第一帧到了先取一段回滚（取到或超时再画）
    let mut history_wait: Option<tokio::time::Instant> = None;
    let mut history_asked = false;
    let mut stdout = std::io::stdout();
    let write = |stdout: &mut std::io::Stdout, s: &str| -> Result<()> {
        stdout.write_all(s.as_bytes())?;
        stdout.flush()?;
        Ok(())
    };

    let exit = loop {
        let deadline = history_wait.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            msg = rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(_)) | None => break Exit::Lost,
                };
                debug(|| format!("ws {:?}", match &msg { Message::Binary(b) => format!("binary {}", b.len()), m => format!("{m:?}") }));
                let data = match msg {
                    Message::Binary(b) => b,
                    Message::Close(frame) => {
                        break if frame.is_some_and(|f| u16::from(f.code) == 1000) { Exit::Ended } else { Exit::Lost };
                    }
                    _ => continue,
                };
                let mut out = String::new();
                let mut new_frame = false;
                for m in zdec.feed(&data)? {
                    debug(|| format!("msg {}", format!("{m:?}").chars().take(120).collect::<String>()));
                    match &m {
                        ServerMsg::Snapshot { .. } | ServerMsg::Delta(_) => {
                            if screen.apply_server_msg(&m).is_err() {
                                break;
                            }
                            new_frame = true;
                        }
                        ServerMsg::Scrolled { rows, .. } => painter.add_scrollback(rows),
                        ServerMsg::HistoryChunk { slice, .. } => {
                            painter.add_scrollback(&slice.rows);
                            history_wait = None;
                            new_frame = true;
                        }
                        ServerMsg::Event(CoreEvent::Bell) => out.push('\x07'),
                        ServerMsg::Event(CoreEvent::ClipboardStore(s)) => {
                            use base64::Engine as _;
                            out.push_str(&format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(s)));
                        }
                        _ => {}
                    }
                }
                let Some(snap) = screen.snapshot() else { continue };
                if !painter.started() && !history_asked && new_frame && !snap.modes.alt_screen && snap.scrollback_next_seq > snap.scrollback_oldest_seq {
                    // 第一帧：先要最近的回滚行
                    let start = snap.scrollback_next_seq.saturating_sub(INITIAL_HISTORY as u64).max(snap.scrollback_oldest_seq);
                    let count = (snap.scrollback_next_seq - start) as u32;
                    tx.send(encode(&ClientMsg::HistoryReq { request_id: 1, epoch: snap.scrollback_epoch, start_seq: start, count })).await?;
                    history_wait = Some(tokio::time::Instant::now() + Duration::from_millis(1500));
                    history_asked = true;
                    continue;
                }
                if history_wait.is_some() {
                    continue;
                }
                if new_frame {
                    let acked = screen.acked_input_seq();
                    rtt.acked(acked);
                    if predict == PredictMode::Auto {
                        pred.set_enabled(auto_predict(rtt.estimate(), pred.enabled()), seq);
                    }
                    pred.on_frame(snap, acked, seq);
                    out.push_str(&painter.frame(&with_predictions(snap, &pred)));
                }
                if !out.is_empty() {
                    write(&mut stdout, &out)?;
                }
                if new_frame {
                    tx.send(encode(&ClientMsg::Ack { frame: screen.frame() })).await?;
                }
            }
            _ = tokio::time::sleep_until(deadline), if history_wait.is_some() => {
                history_wait = None;
                if let Some(snap) = screen.snapshot() {
                    let out = painter.frame(snap);
                    write(&mut stdout, &out)?;
                    tx.send(encode(&ClientMsg::Ack { frame: screen.frame() })).await?;
                }
            }
            input = in_rx.recv() => {
                let Some(input) = input else { break Exit::Lost };
                let Some(bytes) = esc.filter(&input) else { break Exit::Detached };
                if bytes.is_empty() {
                    continue;
                }
                seq += 1;
                let before = pred.version();
                pred.on_input(seq, &bytes, true, screen.snapshot(), screen.acked_input_seq());
                rtt.sent(seq);
                tx.send(encode(&ClientMsg::Input { seq, data: bytes })).await?;
                // 预测变了马上画（不等 termd 的帧）；还在等历史、还没画过时不画
                if pred.version() != before && painter.started() && history_wait.is_none() {
                    if let Some(snap) = screen.snapshot() {
                        write(&mut stdout, &painter.frame(&with_predictions(snap, &pred)))?;
                    }
                }
            }
            _ = resized.recv() => {
                let now = tty::size();
                if now != size {
                    size = now;
                    painter.resize(now.0, now.1);
                    pred.reset(seq);
                    tx.send(encode(&ClientMsg::Resize { cols: now.0, rows: now.1 })).await?;
                    if let Some(snap) = screen.snapshot() {
                        write(&mut stdout, &painter.frame(snap))?;
                    }
                }
            }
        }
    };
    let _ = write(&mut stdout, &painter.finish());
    let _ = tx.send(Message::Close(None)).await;
    Ok(exit)
}

/// 本地终端：原始模式、尺寸、改尺寸事件、读键盘
mod tty {
    use tokio::sync::mpsc;

    /// 键盘输入在单独的线程里阻塞读（标准输入在各系统上都不好做成异步的）
    pub fn spawn_stdin(tx: mpsc::Sender<Vec<u8>>) {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    #[cfg(unix)]
    pub struct RawMode(libc::termios);

    #[cfg(unix)]
    impl RawMode {
        pub fn enter() -> anyhow::Result<Self> {
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) != 0 {
                    anyhow::bail!("tcgetattr failed");
                }
                let saved = t;
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(0, libc::TCSANOW, &t);
                Ok(RawMode(saved))
            }
        }
    }

    #[cfg(unix)]
    impl Drop for RawMode {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }

    #[cfg(unix)]
    pub fn size() -> (u16, u16) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
                return (ws.ws_col, ws.ws_row);
            }
        }
        (80, 24)
    }

    /// 窗口改了尺寸（SIGWINCH）
    #[cfg(unix)]
    pub fn resize_events() -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) else { return };
            while sig.recv().await.is_some() {
                if tx.send(()).await.is_err() {
                    break;
                }
            }
        });
        rx
    }

    #[cfg(windows)]
    pub struct RawMode {
        input: u32,
        output: u32,
        in_cp: u32,
        out_cp: u32,
    }

    #[cfg(windows)]
    mod win {
        use windows_sys::Win32::System::Console::*;
        pub fn handles() -> (isize, isize) {
            unsafe { (GetStdHandle(STD_INPUT_HANDLE) as isize, GetStdHandle(STD_OUTPUT_HANDLE) as isize) }
        }
    }

    #[cfg(windows)]
    impl RawMode {
        pub fn enter() -> anyhow::Result<Self> {
            use windows_sys::Win32::System::Console::*;
            let (hin, hout) = win::handles();
            unsafe {
                let (mut input, mut output) = (0u32, 0u32);
                GetConsoleMode(hin as _, &mut input);
                GetConsoleMode(hout as _, &mut output);
                let saved = RawMode { input, output, in_cp: GetConsoleCP(), out_cp: GetConsoleOutputCP() };
                // 按键编成 VT 序列、关掉行编辑和回显；输出解释 VT 序列
                SetConsoleMode(hin as _, ENABLE_VIRTUAL_TERMINAL_INPUT);
                SetConsoleMode(
                    hout as _,
                    output | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN | ENABLE_PROCESSED_OUTPUT,
                );
                SetConsoleCP(65001);
                SetConsoleOutputCP(65001);
                Ok(saved)
            }
        }
    }

    #[cfg(windows)]
    impl Drop for RawMode {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Console::*;
            let (hin, hout) = win::handles();
            unsafe {
                SetConsoleMode(hin as _, self.input);
                SetConsoleMode(hout as _, self.output);
                SetConsoleCP(self.in_cp);
                SetConsoleOutputCP(self.out_cp);
            }
        }
    }

    #[cfg(windows)]
    pub fn size() -> (u16, u16) {
        use windows_sys::Win32::System::Console::*;
        let (_, hout) = win::handles();
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(hout as _, &mut info) != 0 {
                let w = info.srWindow;
                return ((w.Right - w.Left + 1).max(1) as u16, (w.Bottom - w.Top + 1).max(1) as u16);
            }
        }
        (80, 24)
    }

    /// Windows 控制台没有 SIGWINCH：每 300 ms 看一眼
    #[cfg(windows)]
    pub fn resize_events() -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut last = size();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let now = size();
                if now != last {
                    last = now;
                    if tx.send(()).await.is_err() {
                        break;
                    }
                }
            }
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_core::{AlacrittyCore, CoreConfig, TerminalCore};

    fn row0(t: &AlacrittyCore) -> String {
        t.snapshot().lines[0].cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn predictions_paint_then_settle() {
        let core = || AlacrittyCore::new(CoreConfig { cols: 20, rows: 4, scrollback_limit: 100 }).unwrap();
        let (mut remote, mut local) = (core(), core());
        let mut painter = Painter::new(20, 4);
        let mut pred = Predictor::default();
        pred.set_enabled(true, 0);
        remote.feed_bytes(b"$ ");
        let s = remote.snapshot();
        local.feed_bytes(painter.frame(&s).as_bytes());

        // 打的字先画出来，光标跟着走
        pred.on_input(1, b"ls", true, Some(&s), 0);
        local.feed_bytes(painter.frame(&with_predictions(&s, &pred)).as_bytes());
        assert_eq!(row0(&local), "$ ls");
        assert_eq!(local.snapshot().cursor.col, 4);

        // 回显到了、确认了：不用再写字
        remote.feed_bytes(b"ls");
        let s = remote.snapshot();
        pred.on_frame(&s, 1, 1);
        assert!(pred.predictions().is_empty());
        let out = painter.frame(&with_predictions(&s, &pred));
        assert!(!out.contains("ls"), "{out:?}");
        assert_eq!(row0(&local), "$ ls");

        // 没有回显（比如输密码）：预测撤掉，本地改回 termd 的样子
        pred.on_input(2, b"x", true, Some(&s), 1);
        local.feed_bytes(painter.frame(&with_predictions(&s, &pred)).as_bytes());
        assert_eq!(row0(&local), "$ lsx");
        pred.on_frame(&s, 2, 2);
        local.feed_bytes(painter.frame(&with_predictions(&s, &pred)).as_bytes());
        assert_eq!(row0(&local), "$ ls");
        assert_eq!(local.snapshot().cursor.col, 4);
    }

    #[test]
    fn rtt_and_auto_switch() {
        let mut r = Rtt::new();
        assert_eq!(r.estimate(), None);
        r.sent(1);
        r.sent(2);
        r.acked(2);
        assert!(r.estimate().is_some_and(|d| d < Duration::from_millis(20)));
        assert!(r.sent.is_empty());
        assert!(!auto_predict(r.estimate(), true));
        assert!(auto_predict(Some(Duration::from_millis(80)), false));
        // 中间保持原样
        assert!(auto_predict(Some(Duration::from_millis(25)), true));
        assert!(!auto_predict(Some(Duration::from_millis(25)), false));
        assert!(auto_predict(None, true));
    }

    #[test]
    fn ids_for_urls() {
        assert!(plain_id("dev-1_A"));
        assert!(!plain_id("a&b"));
        assert!(!plain_id(""));
    }

    #[test]
    fn tilde_dot_detaches_only_at_line_start() {
        let mut e = Escape { at_line_start: true, tilde: false };
        assert_eq!(e.filter(b"a~."), Some(b"a~.".to_vec()));
        assert_eq!(e.filter(b"\r"), Some(b"\r".to_vec()));
        assert_eq!(e.filter(b"~~"), Some(b"~".to_vec()));
        assert_eq!(e.filter(b"\r~x"), Some(b"\r~x".to_vec()));
        assert_eq!(e.filter(b"\r~"), Some(b"\r".to_vec()));
        assert_eq!(e.filter(b"."), None);
    }

    #[test]
    fn pick_by_name_or_id() {
        let list = serde_json::json!([
            { "id": "abc12345", "name": "build", "running": true },
            { "id": "def67890", "name": "builder", "running": true },
            { "id": "ghi00000", "name": "idle", "running": false },
        ]);
        assert_eq!(pick(&list, "build").unwrap(), "abc12345");
        assert_eq!(pick(&list, "def").unwrap(), "def67890");
        assert!(pick(&list, "bu").is_err(), "ambiguous");
        assert!(pick(&list, "idle").is_err());
        assert!(pick(&list, "nope").is_err());
    }
}
