//! `looklook-termd` 终端网关路由：
//! - `/i/{id}/ws`：兼容现有 `src-term/`（ttyd WebSocket 协议 `'0'/'1'/'2'`、`AuthToken`、`tty-zstd`、`input_tap`、`trzsz`）；
//! - `/i/{id}/term`：新版 `looklook-term.v1` 结构化协议流（全程 zstd 流式压缩）；
//! - `/i/{id}/term-history`：回滚历史查询（同时支持 HTTP JSON 与 WebSocket `HistoryReq`/`HistoryChunk`）。

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use term_proto::{
    ClientMsg, LegacyDownMsg, LegacyUpMsg, PROTOCOL_VERSION, ServerMsg, TermStreamDown,
    ZstdStreamEncoder, decode_client_msg,
};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::{App, input_tap, term_zstd};
use crate::termd_client::{read_frame, write_frame};

const TTYD_OUTPUT: u8 = b'0';
const TTYD_SET_TITLE: u8 = b'1';
const TTYD_SET_PREFS: u8 = b'2';

const TTYD_INPUT: u8 = b'0';
const TTYD_RESIZE: u8 = b'1';
const TTYD_PAUSE: u8 = b'2';
const TTYD_RESUME: u8 = b'3';

#[derive(Debug, Deserialize)]
struct AuthInit {
    #[serde(rename = "AuthToken", default)]
    _auth_token: String,
    #[serde(default)]
    columns: u16,
    #[serde(default)]
    rows: u16,
}

#[derive(Debug, Deserialize)]
struct ResizePayload {
    columns: u16,
    rows: u16,
}

/// 检查 `Sec-WebSocket-Protocol` 列表是否包含目标子协议（忽略大小写）。
fn offers_subprotocol(h: &HeaderMap, proto: &str) -> bool {
    h.get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim().eq_ignore_ascii_case(proto))
}

fn ws_handshake_response(req: &mut Request, subprotocol: Option<&str>) -> Option<(Response, hyper::upgrade::OnUpgrade)> {
    let key = req.headers().get(header::SEC_WEBSOCKET_KEY)?.clone();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut b = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept);
    if let Some(proto) = subprotocol {
        if let Ok(v) = HeaderValue::from_str(proto) {
            b = b.header(header::SEC_WEBSOCKET_PROTOCOL, v);
        }
    }
    let resp = b.body(Body::empty()).ok()?;
    let upgrade = hyper::upgrade::on(req);
    Some((resp, upgrade))
}

/// 处理 `/i/{id}/ws`：兼容现有 `src-term/` 前端的 ttyd WebSocket 协议。
pub(super) async fn serve_legacy_ws(app: App, id: String, session_id: String, mut req: Request) -> Response {
    let zstd_on = term_zstd::enabled() && term_zstd::client_offers_zstd(req.headers());
    let chosen_proto = if zstd_on {
        Some(term_zstd::SUBPROTOCOL)
    } else if offers_subprotocol(req.headers(), "tty") {
        Some("tty")
    } else {
        None
    };

    let Some((resp, upgrade)) = ws_handshake_response(&mut req, chosen_proto) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // 若会话是 `want_running` 但守护进程还没建好（或上次进程退出后用户按回车重连），连上前先补建。
    app.instances.ensure_attachable(&id).await;

    tokio::spawn(async move {
        let _guard = crate::updater::TerminalGuard::new();
        let Ok(upgraded) = upgrade.await else { return };
        let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        if let Err(e) = run_legacy_ws_bridge(app, id, session_id, ws, zstd_on).await {
            tracing::debug!(error = %e, "termd legacy ws ended");
        }
    });

    resp
}

async fn run_legacy_ws_bridge(
    app: App,
    id: String,
    session_id: String,
    mut ws: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    zstd_on: bool,
) -> anyhow::Result<()> {
    // 1. 读取 src-term/main.js 首包 `{"AuthToken":...,"columns":cols,"rows":rows}`
    let first = match ws.next().await {
        Some(Ok(Message::Binary(b))) => serde_json::from_slice::<AuthInit>(&b).ok(),
        Some(Ok(Message::Text(t))) => serde_json::from_str::<AuthInit>(&t).ok(),
        _ => return Ok(()),
    };
    let (cols, rows) = first
        .map(|a| (a.columns.max(2), a.rows.max(1)))
        .unwrap_or((80, 24));

    let client_id = format!("ws-{}", crate::util::random_id(8));
    let (init, ipc) = app
        .instances
        .termd
        .open_legacy_stream(&session_id, cols, rows, client_id)
        .await
        .map_err(|e| anyhow::anyhow!("{:?}: {:?}", e.code, e.params))?;

    let mut encoder = if zstd_on {
        Some(term_zstd::TermZstdEncoder::new()?)
    } else {
        None
    };

    let (backlog, title) = match init {
        LegacyDownMsg::Init { backlog, title } => (backlog, title),
        _ => (Vec::new(), None),
    };

    // 2. 下发 ttyd 初始标题（'1'）、偏好设置（'2'）与回放屏缓冲（'0'）
    let row_name = app
        .instances
        .instance_name(&id)
        .unwrap_or_else(|| id.clone());
    let title_str = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| row_name.clone());
    let mut title_frame = Vec::with_capacity(1 + title_str.len());
    title_frame.push(TTYD_SET_TITLE);
    title_frame.extend_from_slice(title_str.as_bytes());
    ws.send(Message::Binary(title_frame.into())).await?;

    let prefs_json = legacy_prefs_json(&app, &row_name);
    let mut prefs_frame = Vec::with_capacity(1 + prefs_json.len());
    prefs_frame.push(TTYD_SET_PREFS);
    prefs_frame.extend_from_slice(prefs_json.as_bytes());
    ws.send(Message::Binary(prefs_frame.into())).await?;

    if !backlog.is_empty() {
        let out_frame = encode_legacy_output(&backlog, &mut encoder)?;
        ws.send(Message::Binary(out_frame.into())).await?;
    }

    let (mut ipc_r, mut ipc_w) = tokio::io::split(ipc);
    let mut paused = false;
    let mut pending_out: Vec<u8> = Vec::new();
    let (mut ws_sink, mut ws_stream) = ws.split();

    loop {
        tokio::select! {
            ws_msg = ws_stream.next() => {
                match ws_msg {
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws_sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(msg)) => {
                        let raw: Vec<u8> = match msg {
                            Message::Binary(b) => b.to_vec(),
                            Message::Text(t) => t.as_bytes().to_vec(),
                            _ => continue,
                        };
                        if raw.is_empty() {
                            continue;
                        }
                        match raw[0] {
                            TTYD_INPUT => {
                                if input_tap::is_user_input_msg(&raw) {
                                    app.instances.note_input(&id);
                                }
                                let pkt = bincode::serialize(&LegacyUpMsg::Input(raw[1..].to_vec()))?;
                                write_frame(&mut ipc_w, &pkt).await?;
                            }
                            TTYD_RESIZE => {
                                if let Ok(sz) = serde_json::from_slice::<ResizePayload>(&raw[1..]) {
                                    let pkt = bincode::serialize(&LegacyUpMsg::Resize {
                                        cols: sz.columns,
                                        rows: sz.rows,
                                    })?;
                                    write_frame(&mut ipc_w, &pkt).await?;
                                }
                            }
                            TTYD_PAUSE => {
                                paused = true;
                            }
                            TTYD_RESUME => {
                                paused = false;
                                if !pending_out.is_empty() {
                                    let flush_buf = std::mem::take(&mut pending_out);
                                    let frame = encode_legacy_output(&flush_buf, &mut encoder)?;
                                    ws_sink.send(Message::Binary(frame.into())).await?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            ipc_res = read_frame(&mut ipc_r), if !paused || pending_out.len() < 4 * 1024 * 1024 => {
                match ipc_res? {
                    Some(frame) => {
                        let msg: LegacyDownMsg = bincode::deserialize(&frame)?;
                        match msg {
                            LegacyDownMsg::Output(data) => {
                                if paused {
                                    pending_out.extend_from_slice(&data);
                                } else {
                                    let frame = encode_legacy_output(&data, &mut encoder)?;
                                    ws_sink.send(Message::Binary(frame.into())).await?;
                                }
                            }
                            LegacyDownMsg::Title(t) => {
                                let mut out = Vec::with_capacity(1 + t.len());
                                out.push(TTYD_SET_TITLE);
                                out.extend_from_slice(t.as_bytes());
                                ws_sink.send(Message::Binary(out.into())).await?;
                            }
                            LegacyDownMsg::Exited { .. } => {
                                if !pending_out.is_empty() {
                                    let flush_buf = std::mem::take(&mut pending_out);
                                    let frame = encode_legacy_output(&flush_buf, &mut encoder)?;
                                    ws_sink.send(Message::Binary(frame.into())).await?;
                                }
                                let _ = ws_sink
                                    .send(Message::Close(Some(CloseFrame {
                                        code: CloseCode::Normal,
                                        reason: "".into(),
                                    })))
                                    .await;
                                break;
                            }
                            LegacyDownMsg::Init { .. } => {}
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let _ = ipc_w.shutdown().await;
    Ok(())
}

fn encode_legacy_output(
    raw: &[u8],
    encoder: &mut Option<term_zstd::TermZstdEncoder>,
) -> std::io::Result<Vec<u8>> {
    match encoder {
        Some(enc) => {
            let compressed = enc.compress_chunk(raw)?;
            let mut out = Vec::with_capacity(1 + compressed.len());
            out.push(TTYD_OUTPUT);
            out.extend_from_slice(&compressed);
            Ok(out)
        }
        None => {
            let mut out = Vec::with_capacity(1 + raw.len());
            out.push(TTYD_OUTPUT);
            out.extend_from_slice(raw);
            Ok(out)
        }
    }
}

/// termd 会话的终端页里多插一段脚本：告诉 src-term/main.js 可以走 `/i/{id}/term`（手机上用 DOM 渲染，设计稿 §6），
/// 顺带给出配色、字体（新协议里没有 ttyd 那条“首选项”消息）。JSON 里的 `<` 转义掉，字体名等内容不会提前结束 `<script>`。
pub(super) fn term_boot_tag(app: &App, id: &str) -> String {
    let row_name = app.instances.instance_name(id).unwrap_or_else(|| id.to_string());
    let prefs = legacy_prefs_json(app, &row_name).replace('<', "\\u003c");
    format!("<script>window.__llTermd={prefs};</script>")
}

fn legacy_prefs_json(app: &App, row_name: &str) -> String {
    let s = crate::settings::Settings::load(&app.store);
    let theme_val: serde_json::Value =
        serde_json::from_str(crate::instances::xterm_theme(&s.theme)).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "fontFamily": s.font_family(),
        "fontSize": s.font_size,
        "lineHeight": 1.15,
        "rendererType": "webgl",
        "disableLeaveAlert": true,
        "disableResizeOverlay": true,
        "cursorBlink": true,
        "enableTrzsz": true,
        "isWindows": cfg!(windows),
        "titleFixed": row_name,
        "theme": theme_val,
    })
    .to_string()
}

/// 处理 `/i/{id}/term`：新版 `looklook-term.v1` 结构化终端流（WS & WebRTC DataChannel 共用）。
pub(super) async fn serve_term_ws(app: App, id: String, session_id: String, mut req: Request) -> Response {
    let proto = offers_subprotocol(req.headers(), PROTOCOL_VERSION).then_some(PROTOCOL_VERSION);
    let Some((resp, upgrade)) = ws_handshake_response(&mut req, proto) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    app.instances.ensure_attachable(&id).await;

    tokio::spawn(async move {
        let _guard = crate::updater::TerminalGuard::new();
        let Ok(upgraded) = upgrade.await else { return };
        let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        if let Err(e) = run_term_ws_bridge(app, id, session_id, ws).await {
            tracing::debug!(error = %e, "termd term.v1 ws ended");
        }
    });

    resp
}

async fn run_term_ws_bridge(
    app: App,
    id: String,
    session_id: String,
    mut ws: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
) -> anyhow::Result<()> {
    let first_hello = match ws.next().await {
        Some(Ok(Message::Binary(b))) => b.to_vec(),
        Some(Ok(Message::Text(t))) => t.as_bytes().to_vec(),
        _ => return Ok(()),
    };

    let ipc = app
        .instances
        .termd
        .open_term_stream(&session_id, &first_hello)
        .await
        .map_err(|e| anyhow::anyhow!("{:?}: {:?}", e.code, e.params))?;

    let (mut ipc_r, mut ipc_w) = tokio::io::split(ipc);
    let (mut ws_sink, mut ws_stream) = ws.split();

    loop {
        tokio::select! {
            ws_msg = ws_stream.next() => {
                match ws_msg {
                    Some(Ok(Message::Binary(b))) => {
                        if let Ok(cmsg) = decode_client_msg(&b) {
                            if matches!(cmsg, ClientMsg::Input { .. } | ClientMsg::Mouse { .. }) {
                                app.instances.note_input(&id);
                            }
                        }
                        write_frame(&mut ipc_w, &b).await?;
                    }
                    Some(Ok(Message::Text(t))) => {
                        let b = t.as_bytes();
                        if let Ok(cmsg) = decode_client_msg(b) {
                            if matches!(cmsg, ClientMsg::Input { .. } | ClientMsg::Mouse { .. }) {
                                app.instances.note_input(&id);
                            }
                        }
                        write_frame(&mut ipc_w, b).await?;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws_sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            ipc_res = read_frame(&mut ipc_r) => {
                match ipc_res? {
                    Some(pkt) => {
                        let down: TermStreamDown = bincode::deserialize(&pkt)?;
                        match down {
                            TermStreamDown::ZstdChunk(z) => {
                                ws_sink.send(Message::Binary(z.into())).await?;
                            }
                            TermStreamDown::Closed { reason } => {
                                let _ = ws_sink
                                    .send(Message::Close(Some(CloseFrame {
                                        code: CloseCode::Normal,
                                        reason: reason.into(),
                                    })))
                                    .await;
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let _ = ipc_w.shutdown().await;
    Ok(())
}

/// 处理 `/i/{id}/term-history`：同时支持 HTTP JSON 与 WebSocket `HistoryReq`/`HistoryChunk`（WS 与 RTC bridge 均可用）。
pub(super) async fn serve_term_history(app: App, _id: String, session_id: String, mut req: Request) -> Response {
    if super::proxy::is_websocket(req.headers()) {
        let proto = offers_subprotocol(req.headers(), PROTOCOL_VERSION).then_some(PROTOCOL_VERSION);
        let Some((resp, upgrade)) = ws_handshake_response(&mut req, proto) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        tokio::spawn(async move {
            let Ok(upgraded) = upgrade.await else { return };
            let mut ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            let Ok(mut zstd) = ZstdStreamEncoder::new() else { return };
            while let Some(Ok(msg)) = ws.next().await {
                let raw = match msg {
                    Message::Binary(b) => b.to_vec(),
                    Message::Text(t) => t.as_bytes().to_vec(),
                    Message::Ping(p) => {
                        let _ = ws.send(Message::Pong(p)).await;
                        continue;
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                if let Ok(ClientMsg::HistoryReq {
                    request_id,
                    epoch,
                    start_seq,
                    count,
                }) = decode_client_msg(&raw)
                {
                    let reply = match app
                        .instances
                        .termd
                        .history(&session_id, Some(epoch), Some(start_seq), Some(count), None)
                        .await
                    {
                        Some((Some(slice), epoch_mismatch)) => ServerMsg::HistoryChunk {
                            request_id,
                            slice,
                            epoch_mismatch,
                        },
                        _ => ServerMsg::Error {
                            code: "not_found".into(),
                            message: "session history unavailable".into(),
                        },
                    };
                    if let Ok(chunk) = zstd.encode_server_msg(&reply) {
                        if ws.send(Message::Binary(chunk.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        return resp;
    }

    let query = req.uri().query().unwrap_or("");
    let q_u64 = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse::<u64>().ok())
    };
    let q_u32 = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse::<u32>().ok())
    };
    let epoch = q_u64("epoch");
    let start_seq = q_u64("start_seq");
    let count = q_u32("count").map(|c| c.min(2000));
    let tail_lines = q_u32("tail").map(|t| t.min(20000));

    match app
        .instances
        .termd
        .history(&session_id, epoch, start_seq, count, tail_lines)
        .await
    {
        Some((Some(slice), epoch_mismatch)) => (
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(serde_json::json!({
                "epoch_mismatch": epoch_mismatch,
                "slice": slice,
            })),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Read;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Extension;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use term_proto::{
        ClientMsg, ClientScreen, HistorySlice, PROTOCOL_VERSION, ServerMsg, ViewState,
        ZstdStreamDecoder, encode_client_msg,
    };
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    use crate::gateway::{Access, App, router};
    use crate::instances::Input;
    use crate::paths::{Paths, TERMD_EXE};
    use crate::settings::Settings;

    #[derive(Default)]
    struct ChunkReader(VecDeque<u8>);

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "empty"));
            }
            let n = buf.len().min(self.0.len());
            for b in &mut buf[..n] {
                *b = self.0.pop_front().expect("len checked");
            }
            Ok(n)
        }
    }

    fn decompress_step(
        dec: &mut zstd::stream::read::Decoder<'static, std::io::BufReader<ChunkReader>>,
        chunk: &[u8],
    ) -> Vec<u8> {
        dec.get_mut().get_mut().0.extend(chunk);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match dec.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        out
    }

    fn find_termd_candidate(exe: &std::path::Path) -> Option<std::path::PathBuf> {
        if let Some(p) = std::env::var_os("LOOKLOOK_TERMD_BIN").map(std::path::PathBuf::from) {
            if p.is_file() {
                return Some(p);
            }
        }
        for dir in exe.ancestors().skip(1).take(6) {
            for rel in [
                std::path::PathBuf::from(TERMD_EXE),
                std::path::Path::new("debug").join(TERMD_EXE),
                std::path::Path::new("release").join(TERMD_EXE),
            ] {
                let candidate = dir.join(rel);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from) {
            for profile in ["debug", "release"] {
                let candidate = target_dir.join(profile).join(TERMD_EXE);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn locate_termd_bin() -> std::path::PathBuf {
        static BUILD_ONCE: std::sync::Once = std::sync::Once::new();
        let exe = std::env::current_exe().unwrap();
        if let Some(found) = find_termd_candidate(&exe) {
            return found;
        }
        BUILD_ONCE.call_once(|| {
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "--manifest-path"])
                .arg(&manifest)
                .args(["-p", "termd", "--bin", "looklook-termd"])
                .status()
                .expect("spawn cargo build for looklook-termd");
            assert!(status.success(), "failed to build looklook-termd for integration test");
        });
        find_termd_candidate(&exe).expect("looklook-termd still missing after cargo build")
    }

    fn test_app(home: &std::path::Path, termd_bin: Option<std::path::PathBuf>) -> App {
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let paths = Paths {
            home: home.to_path_buf(),
            ttyd: None,
            mux: None,
            termd: termd_bin,
            fonts: None,
            trzsz: None,
        };
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let account = crate::account::test_support::logged_in(
            store.clone(),
            paths.clone(),
            "http://127.0.0.1:9",
            "dev_termd_test",
            key,
        );
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let updater = crate::updater::Updater::new(
            store.clone(),
            paths.clone(),
            account.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let chat = crate::chat::Chat::new(store.clone(), paths.clone(), instances.clone());
        Arc::new(crate::gateway::Inner {
            direct: crate::direct_tls::Direct::new(store.clone(), 1),
            account: account.clone(),
            instances,
            updater,
            metrics: crate::metrics::Metrics::new(),
            relay_watch: crate::relay_watch::RelayWatch::new(account.clone()),
            transfers: crate::transfer::Transfers::new(paths.clone()),
            chat,
            store,
            paths: paths.clone(),
            ui_addr: "127.0.0.1:0".parse().unwrap(),
            relay_addr: "127.0.0.1:0".parse().unwrap(),
            http: crate::gateway::proxy::client(),
            access: std::sync::RwLock::new(crate::gateway::access::AccessConfig::default()),
            limiter: Default::default(),
            resolved: Default::default(),
            lan_ips: Vec::new(),
            remote: Default::default(),
            rtc: Default::default(),
            outbound: Default::default(),
            ssh_transfer: Default::default(),
            sync: Default::default(),
            notify: Default::default(),
            plugins: crate::plugins::Plugins::new(paths, account, &crate::hardening::builtin_plugin_keys()),
        })
    }

    #[tokio::test]
    async fn termd_default_on_and_hybrid_coexistence() {
        let dir = tempfile::tempdir().unwrap();
        let bin = locate_termd_bin();
        let app = test_app(dir.path(), Some(bin));
        let create = || async {
            app.instances
                .create(Input {
                    launch: Some("shell".into()),
                    workdir: Some("/tmp".into()),
                    ..Default::default()
                })
                .await
                .unwrap()
        };

        // 1. 默认开启 termd（第 6 步）：capabilities 是 termd，新建实例 opts.termd == true
        assert!(Settings::default().termd);
        assert_eq!(app.instances.capabilities()["backend"].as_str(), Some("termd"));
        assert_eq!(app.instances.capabilities()["termd"].as_bool(), Some(true));
        assert_eq!(app.instances.capabilities()["persistent"].as_bool(), Some(true));
        let on_view = create().await;
        assert!(app.instances.opts(&on_view.row.id).termd);
        assert!(app.instances.uses_termd(&on_view.row.id));

        // 2. 关掉 Settings.termd → 新建的回到旧后端，已在 termd 上的继续走 termd（§4.7）
        Settings {
            termd: false,
            ..Settings::default()
        }
        .save(&app.store, 1000)
        .unwrap();
        assert!(!Settings::load(&app.store).termd, "自己关掉的不会被迁移再打开");
        assert_ne!(app.instances.capabilities()["backend"].as_str(), Some("termd"));
        let off_view = create().await;
        assert!(!app.instances.opts(&off_view.row.id).termd);
        assert!(!app.instances.uses_termd(&off_view.row.id));
        assert!(app.instances.uses_termd(&on_view.row.id));

        // 3. 再打开 → 新建的又用 termd，tmux 上的那个不迁
        Settings {
            termd: true,
            ..Settings::default()
        }
        .save(&app.store, 2000)
        .unwrap();
        let on2 = create().await;
        assert!(app.instances.uses_termd(&on2.row.id));
        assert!(!app.instances.uses_termd(&off_view.row.id));
    }

    #[tokio::test]
    async fn termd_instances_gateway_legacy_v1_history_rtc_and_handover() {
        let dir = tempfile::tempdir().unwrap();
        let bin = locate_termd_bin();
        let app = test_app(dir.path(), Some(bin.clone()));

        Settings {
            termd: true,
            ..Settings::default()
        }
        .save(&app.store, 1000)
        .unwrap();

        let created = app
            .instances
            .create(Input {
                name: Some("termd-e2e".into()),
                launch: Some("shell".into()),
                workdir: Some("/tmp".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let id = created.row.id.clone();
        assert!(app.instances.uses_termd(&id));

        // 网关未启动前：endpoint() == None -> 503
        assert!(app.instances.endpoint(&id).await.is_none());

        let started = app.instances.start(&id).await.unwrap();
        assert!(started.running);
        assert_eq!(started.task_alive, Some(true));
        assert!(started.started_at.is_some());

        let daemon_pid = app.instances.termd.daemon_pid().await.expect("daemon running");
        let pane = app.instances.pane(&id).await.expect("pane info");
        assert!(pane.pid > 0);
        assert!(!app.instances.pane_pids(&id).await.is_empty());

        // process_roots 包含 termd 守护进程与该终端的子进程树
        let roots = app.instances.process_roots().await;
        assert_eq!(roots.termd, Some(daemon_pid));
        assert!(roots.sessions.iter().any(|s| s.id == id && !s.roots.is_empty()));

        // 起一个临时 HTTP 网关测试 /i/{id}/token、/i/{id}/ws (5.9)、/i/{id}/term、/i/{id}/term-history
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let gw_router = router(app.clone()).layer(Extension(Access::Local));
        let srv = tokio::spawn(async move {
            axum::serve(listener, gw_router).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let tok: Value = http.get(format!("{base}/i/{id}/token")).send().await.unwrap().json().await.unwrap();
        assert_eq!(tok["token"].as_str(), Some(""));

        // §5.9 旧前端 /i/{id}/ws + tty-zstd 协商 + AuthToken + resize + input_tap + trzsz 兼容
        let mut ws_req = format!("ws://{addr}/i/{id}/ws").into_client_request().unwrap();
        ws_req.headers_mut().insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            axum::http::HeaderValue::from_static("tty-zstd, tty"),
        );
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut ws, resp) = tokio_tungstenite::client_async(ws_req, tcp).await.unwrap();
        assert_eq!(resp.headers()[axum::http::header::SEC_WEBSOCKET_PROTOCOL], "tty-zstd");

        let auth = json!({ "AuthToken": "", "columns": 100, "rows": 30 }).to_string();
        ws.send(Message::Binary(auth.into_bytes().into())).await.unwrap();

        // 读首帧 TITLE ('1') & PREFS ('2')
        let msg1 = ws.next().await.unwrap().unwrap();
        assert_eq!(msg1.into_data().first().copied(), Some(b'1'));
        let msg2 = ws.next().await.unwrap().unwrap();
        let m2_bytes = msg2.into_data();
        assert_eq!(m2_bytes[0], b'2');
        let prefs: Value = serde_json::from_slice(&m2_bytes[1..]).unwrap();
        assert_eq!(prefs["enableTrzsz"], Value::Bool(true));

        let mut dec = zstd::stream::read::Decoder::new(ChunkReader::default()).unwrap();
        // 输入命令并读取 zstd 解压后的输出
        let mut cmd_frame = vec![b'0'];
        cmd_frame.extend_from_slice(b"printf 'LEGACY_ZSTD_OK\\n'\r");
        ws.send(Message::Binary(cmd_frame.into())).await.unwrap();

        // 也通过 Resize ('1') 更新尺寸
        let mut resize_frame = vec![b'1'];
        resize_frame.extend_from_slice(br#"{"columns":100,"rows":30}"#);
        ws.send(Message::Binary(resize_frame.into())).await.unwrap();

        let mut decoded_all = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline {
            if String::from_utf8_lossy(&decoded_all).contains("LEGACY_ZSTD_OK") {
                break;
            }
            if let Ok(Some(Ok(Message::Binary(b)))) = tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                if b.first() == Some(&b'0') && b.len() > 1 {
                    let chunk = decompress_step(&mut dec, &b[1..]);
                    decoded_all.extend_from_slice(&chunk);
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&decoded_all).contains("LEGACY_ZSTD_OK"),
            "decoded output missing marker: {}",
            String::from_utf8_lossy(&decoded_all)
        );
        assert!(app.instances.input_at(&id).is_some(), "input_tap 应记录到用户输入时间");
        assert!(app.instances.output_activity().await.contains_key(&id));
        ws.close(None).await.unwrap();

        // 验证 §4.6 查询接口：send_literal / send_keys / paste / screen / recent_output / text / full_output
        app.instances
            .send_literal(&id, "UPGRADE_KEEP=alive_123 && seq 1 60 | xargs -I{} printf 'LINE_{}\\n'")
            .await
            .unwrap();
        app.instances.send_keys(&id, &["Enter"]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        app.instances.paste(&id, "echo PASTED_OK\r").await.unwrap();

        let mut ok_recent = false;
        for _ in 0..25 {
            if let Some(txt) = app.instances.recent_output(&id).await {
                if txt.contains("PASTED_OK") && txt.contains("LINE_60") {
                    ok_recent = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        assert!(ok_recent);
        assert!(app.instances.screen(&id).await.is_some());
        assert!(app.instances.text(&id).await.unwrap().contains("PASTED_OK"));
        assert!(app.instances.full_output(&id).await.unwrap().contains("LINE_1"));

        // 验证 /i/{id}/ll-history 与 /i/{id}/term-history（HTTP JSON）
        let ll_hist: Value = http
            .get(format!("{base}/i/{id}/ll-history?n=500"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ll_hist["width"].as_u64(), Some(100));
        let sz = ll_hist["size"].as_u64().unwrap();
        assert!(sz > 0);
        let ll_inc: Value = http
            .get(format!("{base}/i/{id}/ll-history?n=500&since={sz}&w=100"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ll_inc["full"].as_bool(), Some(false));

        let term_hist_resp = http
            .get(format!("{base}/i/{id}/term-history?tail=15"))
            .send()
            .await
            .unwrap();
        assert_eq!(term_hist_resp.status(), reqwest::StatusCode::OK);
        let th_json: Value = term_hist_resp.json().await.unwrap();
        let slice: HistorySlice = serde_json::from_value(th_json["slice"].clone()).unwrap();
        assert!(!slice.rows.is_empty());

        // 模拟 WebRTC DataChannel Bridge（rtc/bridge.rs 同款 duplex HTTP/1.1 内存连接 + Access::Rtc）访问 /i/{id}/term 与 /i/{id}/term-history WS
        let (client_io, server_io) = tokio::io::duplex(256 * 1024);
        let svc = router(app.clone()).layer(Extension(Access::Rtc {
            origin: "https://rtc.looklook.test".into(),
        }));
        let rtc_srv = tokio::spawn(async move {
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(server_io), svc)
                .with_upgrades()
                .await;
        });

        let mut rtc_req = format!("ws://rtc.looklook.test/i/{id}/term").into_client_request().unwrap();
        rtc_req.headers_mut().insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            axum::http::HeaderValue::from_static(PROTOCOL_VERSION),
        );
        let (mut v1_ws, v1_resp) = tokio_tungstenite::client_async(rtc_req, client_io).await.unwrap();
        assert_eq!(v1_resp.headers()[axum::http::header::SEC_WEBSOCKET_PROTOCOL], PROTOCOL_VERSION);

        let hello = ClientMsg::Hello {
            protocol: PROTOCOL_VERSION.into(),
            cols: 100,
            rows: 30,
            client_id: Some("rtc-v1".into()),
            last_frame: None,
            last_input_seq: 0,
        };
        v1_ws.send(Message::Binary(encode_client_msg(&hello).unwrap().into())).await.unwrap();

        let mut v1_dec = ZstdStreamDecoder::new().unwrap();
        let mut screen = ClientScreen::new();
        let mut got_v1_marker = false;
        let mut sent_v1_input = false;
        let mut last_frame;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline && !got_v1_marker {
            if let Ok(Some(Ok(Message::Binary(z)))) = tokio::time::timeout(Duration::from_millis(250), v1_ws.next()).await {
                for msg in v1_dec.feed(&z).unwrap() {
                    let changed = screen.apply_server_msg(&msg).unwrap();
                    match &msg {
                        ServerMsg::Snapshot { frame, .. } => {
                            last_frame = *frame;
                            v1_ws
                                .send(Message::Binary(encode_client_msg(&ClientMsg::Ack { frame: last_frame }).unwrap().into()))
                                .await
                                .unwrap();
                            if !sent_v1_input {
                                sent_v1_input = true;
                                v1_ws
                                    .send(Message::Binary(
                                        encode_client_msg(&ClientMsg::Input {
                                            seq: 1,
                                            data: b"echo V1_RTC_OK\r".to_vec(),
                                        })
                                        .unwrap()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                                let _ = ViewState::Active;
                            }
                        }
                        ServerMsg::Delta(d) => {
                            last_frame = d.frame;
                            v1_ws
                                .send(Message::Binary(encode_client_msg(&ClientMsg::Ack { frame: last_frame }).unwrap().into()))
                                .await
                                .unwrap();
                            if changed {
                                if let Some(snap) = screen.snapshot() {
                                    let rendered = snap
                                        .lines
                                        .iter()
                                        .map(|r| {
                                            r.cells
                                                .iter()
                                                .filter(|c| c.spacer == term_proto::CellSpacer::None)
                                                .map(|c| c.c)
                                                .collect::<String>()
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    if rendered.contains("V1_RTC_OK") {
                                        got_v1_marker = true;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        assert!(got_v1_marker, "term.v1 over RTC bridge failed to receive V1_RTC_OK delta");
        v1_ws.close(None).await.ok();
        rtc_srv.abort();

        // 测试 WS /i/{id}/term-history
        let mut h_ws_req = format!("ws://{addr}/i/{id}/term-history").into_client_request().unwrap();
        h_ws_req.headers_mut().insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            axum::http::HeaderValue::from_static(PROTOCOL_VERSION),
        );
        let h_tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut h_ws, _) = tokio_tungstenite::client_async(h_ws_req, h_tcp).await.unwrap();
        h_ws.send(Message::Binary(
            encode_client_msg(&ClientMsg::HistoryReq {
                request_id: 42,
                epoch: slice.epoch,
                start_seq: slice.oldest_seq,
                count: 10,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        let Message::Binary(hz) = h_ws.next().await.unwrap().unwrap() else { panic!("expected history binary chunk") };
        let mut h_dec = ZstdStreamDecoder::new().unwrap();
        let h_msgs = h_dec.feed(&hz).unwrap();
        assert!(matches!(h_msgs.first(), Some(ServerMsg::HistoryChunk { request_id: 42, .. })));
        h_ws.close(None).await.ok();

        // 测试 Linux/Unix 下升级交接后会话不断
        #[cfg(unix)]
        {
            let took = app.instances.termd.takeover_with_bin(&bin).await.unwrap();
            assert!(took);
            let new_pid = app.instances.termd.daemon_pid().await.expect("new daemon running");
            assert_ne!(new_pid, daemon_pid);
            app.instances.send_literal(&id, "echo HANDOVER=$UPGRADE_KEEP").await.unwrap();
            app.instances.send_keys(&id, &["Enter"]).await.unwrap();
            let mut preserved = false;
            for _ in 0..25 {
                if app
                    .instances
                    .recent_output(&id)
                    .await
                    .is_some_and(|t| t.contains("HANDOVER=alive_123"))
                {
                    preserved = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            assert!(preserved, "upgrade handover lost child shell state");
        }

        // 停止并清理
        app.instances.stop(&id).await.unwrap();
        assert!(app.instances.endpoint(&id).await.is_none());
        let stopped_view = app.instances.get(&id).await.unwrap();
        assert!(!stopped_view.running);
        assert_eq!(stopped_view.task_alive, Some(false));
        let _ = app.instances.termd.stop_session(&id).await;
        #[cfg(unix)]
        if let Some(_pid) = app.instances.termd.daemon_pid().await {
            // 让临时目录里的守护进程自己有序退出（空会话时关闭 socket）
            let sock = app.paths.termd_socket();
            if let Ok(mut io) = tokio::net::UnixStream::connect(&sock).await {
                if let Ok(bytes) = bincode::serialize(&term_proto::ControlRequest::ShutdownIfIdle) {
                    let _ = crate::termd_client::write_frame(&mut io, &bytes).await;
                    let _ = crate::termd_client::read_frame(&mut io).await;
                }
            }
        }
        srv.abort();
    }
}
