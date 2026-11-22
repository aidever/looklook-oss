//! 专属域名下的公网 HTTPS 直连（服务端 docs/09 §13）：用户站上的页面跨源连 `wss://IP:端口/__looklook/mux?g=<票据>`，
//! 代替 WebRTC 连到这台电脑。
//!
//! - **认证**：平台签的直连票据（[`crate::direct_tls::Direct::check_grant`]，一次性、60 秒）；票据里的 `site`
//!   必须等于 WebSocket 的 Origin，`host` / `site` 当作请求来自的用户站（同 WebRTC 授权票据的 host / origin）；
//! - **复用**：一条 WebSocket 里跑多条通道（[`MUX_OPEN`] / [`MUX_MSG`] / [`MUX_CLOSE`]），每条通道交给
//!   [`bridge::serve`]，帧与 DataChannel 上完全相同，所以请求、终端、续接、`ctl` 心跳都沿用 WebRTC 的逻辑；
//! - **生命周期**：登记在 [`super::Rtc`] 的连接表里，吊销、退出登录、子站会话到期时同样关闭。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use looklook_protocol::direct::{MUX_CLOSE, MUX_MSG, MUX_OPEN};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, protocol::Role, Message};
use tokio_util::sync::CancellationToken;

use super::bridge;
use super::peer::{Channel, DcEvent, LOW_WATER};
use crate::gateway::App;

/// 一条 WebSocket 上最多同时开着的通道。
const MAX_CHANNELS: usize = 1024;
/// 空闲时发 Ping，让中间的代理不因为空闲断开。
const PING: Duration = Duration::from_secs(25);
/// 关闭时等已排队的数据发完最多这么久。
const FLUSH_LIMIT: Duration = Duration::from_secs(3);

enum Out {
    Msg(u32, Vec<u8>),
    Close(u32),
    Ping,
}

/// 复用通道的发送端（[`Channel::mux`]）。释放时在已排队的数据后面发关闭。
pub struct ChannelSink {
    id: u32,
    out: mpsc::UnboundedSender<Out>,
    /// 整条 WebSocket 还没写出去的字节数（各通道共用）。
    pending: Arc<AtomicUsize>,
}

impl ChannelSink {
    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.pending.fetch_add(msg.len(), Ordering::Relaxed);
        self.out.send(Out::Msg(self.id, msg.to_vec())).map_err(|_| anyhow!("直连已断开"))
    }

    pub fn buffered(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

impl Drop for ChannelSink {
    fn drop(&mut self) {
        let _ = self.out.send(Out::Close(self.id));
    }
}

fn frame(op: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 5);
    f.push(op);
    f.extend_from_slice(&id.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

fn reject(status: StatusCode, why: &'static str) -> Response {
    tracing::debug!(why, "直连传输被拒绝");
    (status, axum::Json(serde_json::json!({ "error": why }))).into_response()
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// `GET /__looklook/mux?g=<票据>`（WebSocket，只在 HTTPS 上，见 `gateway::local_guard`）。
pub async fn entry(app: &App, ip: IpAddr, mut req: Request) -> Response {
    let h = req.headers();
    let upgrade = h.get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let Some(key) = h.get(header::SEC_WEBSOCKET_KEY).cloned().filter(|_| upgrade) else { return reject(StatusCode::BAD_REQUEST, "not_websocket") };
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let own = format!("https://{host}");
    let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // 票据只含 base64url 字符和点，不用解码
    let token = req.uri().query().and_then(|q| query_param(q, "g")).unwrap_or("").to_string();
    let g = match app.direct.check_grant(&app.account, ip, &own, &token, "site") {
        Ok(g) => g,
        Err(why) => return reject(StatusCode::FORBIDDEN, why),
    };
    // 旧平台签的票据没有用户站：只能用来打开直连页面
    if g.host.is_empty() || g.site.is_empty() {
        return reject(StatusCode::FORBIDDEN, "no_site");
    }
    // WebSocket 不受 CORS 限制：只接受签票据的那个用户站上的页面
    if !origin.eq_ignore_ascii_case(&g.site) {
        return reject(StatusCode::FORBIDDEN, "origin");
    }
    let (id, cancel) = app.rtc.add(&g.sid, "");
    let ttl = Duration::from_millis((g.abs_exp - app.account.clock.now_ms()).max(0) as u64);
    let ctx = bridge::Ctx {
        router: crate::gateway::router(app.clone()),
        host: g.host.clone(),
        origin: g.site.clone(),
        sid: g.sid.clone(),
        device: g.dev.clone(),
        terms: app.rtc.terms.clone(),
        rtc: Arc::downgrade(&app.rtc),
        expires: tokio::time::Instant::now() + ttl,
    };
    let on = hyper::upgrade::on(&mut req);
    let app = app.clone();
    tokio::spawn(async move {
        match on.await {
            Ok(io) => {
                app.rtc.set_connected(id);
                let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(io), Role::Server, None).await;
                tracing::info!(sid = %ctx.sid, %ip, peers = app.rtc.count(), "直连传输已连接");
                let sid = ctx.sid.clone();
                let why = serve(ws, ctx, cancel, ttl).await;
                tracing::info!(%sid, why, "直连传输关闭");
            }
            Err(e) => tracing::debug!(error = %e, "直连传输的 WebSocket 升级失败"),
        }
        app.rtc.remove(id);
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, tungstenite::handshake::derive_accept_key(key.as_bytes()))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

type Chans = Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<DcEvent>>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一条直连传输：读浏览器的帧，开、关通道，把消息交给对应的通道；写由单独的任务按顺序发。返回关闭的原因。
pub(crate) async fn serve<S>(ws: tokio_tungstenite::WebSocketStream<S>, ctx: bridge::Ctx, cancel: CancellationToken, ttl: Duration) -> &'static str
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Out>();
    let pending = Arc::new(AtomicUsize::new(0));
    let chans: Chans = Arc::default();

    // 写：按顺序发出；还没写出去的字节数降到低水位以下时通知各通道（同 DataChannel 的 bufferedamountlow）
    let writer = {
        let pending = pending.clone();
        let chans = chans.clone();
        tokio::spawn(async move {
            while let Some(o) = out_rx.recv().await {
                let (msg, n) = match o {
                    Out::Msg(id, b) => (Message::Binary(frame(MUX_MSG, id, &b).into()), b.len()),
                    Out::Close(id) => (Message::Binary(frame(MUX_CLOSE, id, &[]).into()), 0),
                    Out::Ping => (Message::Ping(Vec::new().into()), 0),
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
                if n > 0 {
                    let before = pending.fetch_sub(n, Ordering::Relaxed);
                    if before > LOW_WATER && before - n <= LOW_WATER {
                        lock(&chans).values().for_each(|tx| drop(tx.send(DcEvent::Low)));
                    }
                }
            }
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        })
    };

    let mut tasks = tokio::task::JoinSet::new();
    let expires = tokio::time::sleep(ttl);
    tokio::pin!(expires);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING, PING);
    let why = loop {
        tokio::select! {
            _ = cancel.cancelled() => break "closed",
            _ = &mut expires => break "session_expired",
            _ = ping.tick() => { let _ = out_tx.send(Out::Ping); }
            m = stream.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    if b.len() < 5 {
                        continue;
                    }
                    let id = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
                    let payload = &b[5..];
                    match b[0] {
                        MUX_OPEN => {
                            let mut m = lock(&chans);
                            if m.len() >= MAX_CHANNELS || m.contains_key(&id) {
                                let _ = out_tx.send(Out::Close(id));
                                continue;
                            }
                            let (tx, rx) = mpsc::unbounded_channel();
                            m.insert(id, tx);
                            drop(m);
                            let label = String::from_utf8_lossy(payload).into_owned();
                            let ch = Channel::mux(ChannelSink { id, out: out_tx.clone(), pending: pending.clone() }, rx, label);
                            let (ctx, chans) = (ctx.clone(), chans.clone());
                            tasks.spawn(async move {
                                bridge::serve(ctx, ch).await;
                                lock(&chans).remove(&id);
                            });
                        }
                        MUX_MSG => {
                            if let Some(tx) = lock(&chans).get(&id) {
                                let _ = tx.send(DcEvent::Msg(payload.to_vec()));
                            }
                        }
                        MUX_CLOSE => {
                            // 浏览器关了这条通道：通道的事件流随之结束
                            lock(&chans).remove(&id);
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break "disconnected",
                Some(Ok(_)) => {}
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    };
    // 先结束各条通道（终端会话自己决定保留还是结束），再让写任务发完已排队的数据并关闭
    lock(&chans).clear();
    tasks.shutdown().await;
    drop(out_tx);
    let _ = tokio::time::timeout(FLUSH_LIMIT, writer).await;
    why
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio_tungstenite::WebSocketStream;

    fn ctl(v: &Value) -> Vec<u8> {
        super::super::frame::ctl(v)
    }

    async fn next_frame(c: &mut WebSocketStream<tokio::io::DuplexStream>) -> (u8, u32, Vec<u8>) {
        loop {
            if let Message::Binary(b) = c.next().await.unwrap().unwrap() {
                return (b[0], u32::from_be_bytes([b[1], b[2], b[3], b[4]]), b[5..].to_vec());
            }
        }
    }

    /// 真的 WebSocket（内存里的一对流）：ctl 通道 hello、HTTP 请求经通道进路由拿到响应、关通道、会话到期关闭
    #[tokio::test]
    async fn channels_over_one_websocket() {
        use axum::routing::get;
        let router = axum::Router::new().route("/api/x", get(|req: Request| async move {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            format!("hi {host}")
        }));
        let ctx = bridge::Ctx {
            router,
            host: "alice.looklook.test".into(),
            origin: "https://alice.looklook.test".into(),
            sid: "s1".into(),
            device: "dev1".into(),
            terms: Arc::default(),
            rtc: std::sync::Weak::new(),
            expires: tokio::time::Instant::now() + Duration::from_secs(60),
        };
        let (a, b) = tokio::io::duplex(1 << 16);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut c = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        let cancel = CancellationToken::new();
        let done = tokio::spawn(serve(server, ctx, cancel.clone(), Duration::from_secs(60)));

        c.send(Message::Binary(frame(MUX_OPEN, 1, b"ctl").into())).await.unwrap();
        c.send(Message::Binary(frame(MUX_MSG, 1, &ctl(&json!({ "t": "hello", "proto": 1 }))).into())).await.unwrap();
        let (op, id, body) = next_frame(&mut c).await;
        assert_eq!((op, id), (MUX_MSG, 1));
        let hello: Value = serde_json::from_slice(&body[1..]).unwrap();
        assert_eq!((hello["t"].as_str(), hello["dev"].as_str()), (Some("hello"), Some("dev1")));

        c.send(Message::Binary(frame(MUX_OPEN, 3, b"r").into())).await.unwrap();
        c.send(Message::Binary(frame(MUX_MSG, 3, &ctl(&json!({ "t": "req", "m": "GET", "p": "/api/x", "h": [] }))).into())).await.unwrap();
        c.send(Message::Binary(frame(MUX_MSG, 3, &ctl(&json!({ "t": "end" }))).into())).await.unwrap();
        let mut got = (None, Vec::new(), false, false);
        while !got.3 {
            let (op, id, body) = next_frame(&mut c).await;
            assert_eq!(id, 3);
            match (op, body.first()) {
                (MUX_MSG, Some(0)) => {
                    let v: Value = serde_json::from_slice(&body[1..]).unwrap();
                    match v["t"].as_str() {
                        Some("res") => got.0 = v["s"].as_u64(),
                        Some("end") => got.2 = true,
                        _ => {}
                    }
                }
                (MUX_MSG, _) => got.1.extend_from_slice(&body[1..]),
                (MUX_CLOSE, _) => got.3 = true,
                _ => {}
            }
        }
        assert_eq!(got.0, Some(200));
        assert_eq!(String::from_utf8(got.1).unwrap(), "hi alice.looklook.test", "Host 是票据里的用户站");
        assert!(got.2, "响应完了有 end，然后关通道");

        // 打开同一个 id 两次：第二次被拒（回关闭）
        c.send(Message::Binary(frame(MUX_OPEN, 1, b"ctl").into())).await.unwrap();
        assert_eq!(next_frame(&mut c).await, (MUX_CLOSE, 1, Vec::new()));

        cancel.cancel();
        assert_eq!(done.await.unwrap(), "closed");
    }

    #[test]
    fn query() {
        assert_eq!(query_param("g=abc.def&x=1", "g"), Some("abc.def"));
        assert_eq!(query_param("x=1&g=z", "g"), Some("z"));
        assert_eq!(query_param("gg=1", "g"), None);
    }
}
