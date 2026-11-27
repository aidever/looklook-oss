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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use looklook_protocol::direct::{DirectEndpoint, EndpointKind, MUX_CLOSE, MUX_MSG, MUX_OPEN};
use looklook_protocol::dto::DeviceKey;
use looklook_protocol::signing::{parse_public_key, verify};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, protocol::Role, Message};
use tokio_util::sync::CancellationToken;

use super::bridge;
use super::frame::{self as rtc_frame, Frame};
use super::peer::{Channel, DcEvent, PathInfo, PathKind, LOW_WATER};
use crate::gateway::App;

/// 一条 WebSocket 上最多同时开着的通道。
const MAX_CHANNELS: usize = 1024;
/// 空闲时发 Ping，让中间的代理不因为空闲断开。
const PING: Duration = Duration::from_secs(25);
/// 关闭时等已排队的数据发完最多这么久。
const FLUSH_LIMIT: Duration = Duration::from_secs(3);
/// 客户端探测单个直连端点的超时。
const DIAL_LIMIT: Duration = Duration::from_secs(4);

enum Out {
    Open(u32, Vec<u8>),
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

/// `GET /__looklook/mux?g=<票据>`（WebSocket，支持本机回环 `ws://`、本机 TLS `wss://` 与 HTTPS 反代/穿透 `wss://`）。
pub async fn entry(app: &App, ip: IpAddr, tls: bool, https_proxy: bool, mut req: Request) -> Response {
    let h = req.headers();
    let upgrade = h.get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let Some(key) = h.get(header::SEC_WEBSOCKET_KEY).cloned().filter(|_| upgrade) else { return reject(StatusCode::BAD_REQUEST, "not_websocket") };
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let own = if tls || https_proxy { format!("https://{host}") } else { format!("http://{host}") };
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
    let cert_hex = if tls { app.direct.leaf_sha256() } else { String::new() };
    let jti = g.jti.clone();
    let acct = app.account.clone();
    let hello_signer: bridge::HelloSigner = Arc::new(move |nonce: &str| {
        let canonical = looklook_protocol::direct::mux_hello_canonical(&jti, nonce, &cert_hex);
        let (key, sig) = acct.sign_device(&canonical)?;
        Some(bridge::HelloProof { key, cert: cert_hex.clone(), sig })
    });
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
                let why = serve_with_signer(ws, ctx, Some(hello_signer), cancel, ttl).await;
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

#[cfg(test)]
pub(crate) async fn serve<S>(ws: tokio_tungstenite::WebSocketStream<S>, ctx: bridge::Ctx, cancel: CancellationToken, ttl: Duration) -> &'static str
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    serve_with_signer(ws, ctx, None, cancel, ttl).await
}

/// 一条直连传输：读对端的帧，开、关通道，把消息交给对应的通道；写由单独的任务按顺序发。返回关闭的原因。
pub(crate) async fn serve_with_signer<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    ctx: bridge::Ctx,
    hello_signer: Option<bridge::HelloSigner>,
    cancel: CancellationToken,
    ttl: Duration,
) -> &'static str
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
                    Out::Open(id, b) => (Message::Binary(frame(MUX_OPEN, id, &b).into()), 0),
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
                            let (ctx, chans, signer) = (ctx.clone(), chans.clone(), hello_signer.clone());
                            tasks.spawn(async move {
                                bridge::serve_mux(ctx, ch, signer).await;
                                lock(&chans).remove(&id);
                            });
                        }
                        MUX_MSG => {
                            if let Some(tx) = lock(&chans).get(&id) {
                                let _ = tx.send(DcEvent::Msg(payload.to_vec()));
                            }
                        }
                        MUX_CLOSE => {
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
    lock(&chans).clear();
    tasks.shutdown().await;
    drop(out_tx);
    let _ = tokio::time::timeout(FLUSH_LIMIT, writer).await;
    why
}

// ---------------- 客户端侧直连复用（桌面端连同一账户其他电脑） ----------------

#[derive(Debug)]
struct PinCaptureVerifier(Arc<Mutex<Option<String>>>);

impl rustls::client::danger::ServerCertVerifier for PinCaptureVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let hex = data_encoding::HEXLOWER.encode(&Sha256::digest(end_entity.as_ref()));
        *lock(&self.0) = Some(hex);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

/// 桌面端作为发起方建立的一条 `/__looklook/mux` 复用连接。
pub struct MuxConn {
    next_id: AtomicU32,
    out: mpsc::UnboundedSender<Out>,
    pending: Arc<AtomicUsize>,
    chans: Chans,
    pub closed: CancellationToken,
    pub path: PathInfo,
}

impl MuxConn {
    pub fn channel(&self, label: &str) -> Result<Channel> {
        if self.closed.is_cancelled() {
            bail!("直连已关闭");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(DcEvent::Open);
        lock(&self.chans).insert(id, tx);
        self.out.send(Out::Open(id, label.as_bytes().to_vec())).map_err(|_| anyhow!("直连已关闭"))?;
        Ok(Channel::mux(
            ChannelSink { id, out: self.out.clone(), pending: self.pending.clone() },
            rx,
            label.to_string(),
        ))
    }

    /// 连接到目标设备的一条直连端点，并在 `ctl` 通道上完成 `hello`（含自签 TLS 证书指纹 + 设备 Ed25519 公钥签名绑定校验）。
    pub async fn connect(
        ep: &DirectEndpoint,
        token: &str,
        site_origin: &str,
        expected_dev: &str,
        peer_keys: &[DeviceKey],
    ) -> Result<(Self, Channel)> {
        if !ep.web_tls && peer_keys.is_empty() {
            bail!("目标设备缺少公钥，无法校验自签直连");
        }
        let jti = looklook_protocol::direct::extract_jti(token).ok_or_else(|| anyhow!("直连票据缺少 jti"))?;
        let authority = ep.url.strip_prefix("https://").ok_or_else(|| anyhow!("端点协议不支持"))?;
        let (host, port) = parse_authority(authority)?;

        let tcp = tokio::time::timeout(DIAL_LIMIT, tokio::net::TcpStream::connect((host.as_str(), port))).await??;
        let _ = tcp.set_nodelay(true);

        let captured_fp: Arc<Mutex<Option<String>>> = Arc::default();
        let connector = if ep.web_tls {
            None
        } else {
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| anyhow!("TLS 配置失败：{e}"))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinCaptureVerifier(captured_fp.clone())))
                .with_no_client_auth();
            Some(tokio_tungstenite::Connector::Rustls(Arc::new(cfg)))
        };

        use tungstenite::client::IntoClientRequest;
        let ws_url = format!("wss://{authority}{}?g={token}", looklook_protocol::direct::MUX_PATH);
        let mut req = ws_url.into_client_request()?;
        req.headers_mut().insert(header::ORIGIN, header::HeaderValue::from_str(site_origin)?);

        let (ws, _) = tokio::time::timeout(
            DIAL_LIMIT,
            tokio_tungstenite::client_async_tls_with_config(req, tcp, None, connector),
        )
        .await??;

        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Out>();
        let pending = Arc::new(AtomicUsize::new(0));
        let chans: Chans = Arc::default();
        let closed = CancellationToken::new();

        {
            let pending = pending.clone();
            let chans = chans.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = closed.cancelled() => break,
                        o = out_rx.recv() => match o {
                            Some(Out::Open(id, b)) => {
                                if sink.send(Message::Binary(frame(MUX_OPEN, id, &b).into())).await.is_err() { break; }
                            }
                            Some(Out::Msg(id, b)) => {
                                let n = b.len();
                                if sink.send(Message::Binary(frame(MUX_MSG, id, &b).into())).await.is_err() { break; }
                                if n > 0 {
                                    let before = pending.fetch_sub(n, Ordering::Relaxed);
                                    if before > LOW_WATER && before - n <= LOW_WATER {
                                        lock(&chans).values().for_each(|tx| drop(tx.send(DcEvent::Low)));
                                    }
                                }
                            }
                            Some(Out::Close(id)) => {
                                if sink.send(Message::Binary(frame(MUX_CLOSE, id, &[]).into())).await.is_err() { break; }
                            }
                            Some(Out::Ping) => {
                                if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                            }
                            None => break,
                        }
                    }
                }
                let _ = sink.send(Message::Close(None)).await;
                let _ = sink.close().await;
            });
        }

        {
            let chans = chans.clone();
            let out_tx = out_tx.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING, PING);
                loop {
                    tokio::select! {
                        _ = closed.cancelled() => break,
                        _ = ping.tick() => { let _ = out_tx.send(Out::Ping); }
                        m = stream.next() => match m {
                            Some(Ok(Message::Binary(b))) if b.len() >= 5 => {
                                let id = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
                                let payload = &b[5..];
                                match b[0] {
                                    MUX_MSG => {
                                        if let Some(tx) = lock(&chans).get(&id) {
                                            let _ = tx.send(DcEvent::Msg(payload.to_vec()));
                                        }
                                    }
                                    MUX_CLOSE => {
                                        lock(&chans).remove(&id);
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            Some(Ok(_)) => {}
                        }
                    }
                }
                lock(&chans).clear();
                closed.cancel();
            });
        }

        let kind = if ep.kind == EndpointKind::Lan { PathKind::Lan } else { PathKind::Https };
        let conn = Self {
            next_id: AtomicU32::new(1),
            out: out_tx,
            pending,
            chans,
            closed: closed.clone(),
            path: PathInfo { kind, local: "mux".into(), remote: ep.url.clone() },
        };

        let mut ctl = conn.channel("ctl")?;
        let nonce = data_encoding::HEXLOWER.encode(&crate::util::random_bytes::<16>());
        ctl.send(&rtc_frame::ctl(&json!({ "t": "hello", "proto": looklook_protocol::rtc::PROTO, "nonce": nonce })))?;
        let hello = tokio::time::timeout(DIAL_LIMIT, async {
            while let Some(m) = ctl.recv().await {
                if let Some(Frame::Ctl(v)) = rtc_frame::parse(&m) {
                    if rtc_frame::kind(&v) == "hello" {
                        return Some(v);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            closed.cancel();
            anyhow!("直连握手没有回应")
        })?;

        if hello.get("dev").and_then(Value::as_str) != Some(expected_dev) {
            closed.cancel();
            bail!("对端设备不匹配");
        }

        // 校验 `ctl hello` 里的设备签名与 TLS 叶子证书绑定
        if !ep.web_tls || !peer_keys.is_empty() {
            let key_id = hello.get("key").and_then(Value::as_str).unwrap_or("");
            let cert_claim = hello.get("cert").and_then(Value::as_str).unwrap_or("");
            let sig = hello.get("sig").and_then(Value::as_str).unwrap_or("");
            if !ep.web_tls {
                let peer_fp = lock(&captured_fp).clone().unwrap_or_default();
                if peer_fp.is_empty() || !cert_claim.eq_ignore_ascii_case(&peer_fp) {
                    closed.cancel();
                    bail!("自签 TLS 证书指纹与对端声明不一致");
                }
            }
            if !ep.web_tls || !sig.is_empty() {
                let canonical = looklook_protocol::direct::mux_hello_canonical(&jti, &nonce, cert_claim);
                let ok = peer_keys
                    .iter()
                    .filter(|k| k.id == key_id)
                    .filter_map(|k| parse_public_key(&k.public_key))
                    .any(|vk| verify(&vk, &canonical, sig));
                if !ok {
                    closed.cancel();
                    bail!("对端设备签名校验失败");
                }
            }
        }

        Ok((conn, ctl))
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16)> {
    if let Some(after) = authority.strip_prefix('[') {
        let (v6, tail) = after.split_once(']').ok_or_else(|| anyhow!("IPv6 格式错误"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>()?,
            None => 443,
        };
        return Ok((v6.to_string(), port));
    }
    match authority.split_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse::<u16>()?)),
        None => Ok((authority.to_string(), 443)),
    }
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

    /// 真实 TCP + 自签 TLS（`serve_console`）端到端测试：
    /// `MuxConn::connect` 校验证书 SHA-256 与设备 Ed25519 签名绑定，转发 HTTP 请求成功；换错公钥立即拒绝。
    #[tokio::test]
    async fn self_signed_tls_mux_verifies_cert_binding_and_rejects_wrong_key() {
        use looklook_protocol::direct::{encode_grant, DirectEndpoint, DirectGrant, EndpointKind};
        use looklook_protocol::signing::b64;

        let home = tempfile::tempdir().unwrap();
        let dev_sk = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let dev_pk_b64 = b64(dev_sk.verifying_key().as_bytes());
        let app = crate::ssh_transfer::e2e::app(home.path(), "http://127.0.0.1:9", "devb", dev_sk);
        {
            let mut c = app.access();
            c.allow_lan = true;
            c.save(&app.store).unwrap();
            *app.access.write().unwrap() = c;
        }

        // 用平台密钥（e2e::app 里的 platform_key 种子是 [5u8; 32]，kid "p1"）签发设备级 mux 票据
        let srv_sk = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let now_ms = app.account.clock.now_ms();
        let now_s = time::OffsetDateTime::now_utc().unix_timestamp();
        let token = encode_grant(
            &srv_sk,
            &DirectGrant {
                v: 1,
                kid: "p1".into(),
                uid: "1".into(),
                sid: "s1".into(),
                dev: "devb".into(),
                origin: String::new(),
                host: "alice.looklook.test".into(),
                site: "https://alice.looklook.test".into(),
                abs_exp: now_ms + 3_600_000,
                iat: now_ms,
                exp: now_ms + 60_000,
                jti: "jti-e2e-1".into(),
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let srv_app = app.clone();
        let srv_stop = shutdown.clone();
        tokio::spawn(async move {
            let _ = crate::gateway::serve_console(listener, crate::gateway::local_app(srv_app.clone()), srv_app.direct.clone(), srv_stop).await;
        });

        let ep = DirectEndpoint {
            url: format!("https://127.0.0.1:{port}"),
            kind: EndpointKind::Lan,
            web_tls: false,
            until: now_s + 3600,
        };
        let right_keys = vec![DeviceKey { id: "k-devb".into(), public_key: dev_pk_b64 }];

        // 1) 正确的设备公钥：TLS + ctl hello 签名绑定通过
        let (conn, _ctl) = MuxConn::connect(&ep, &token, "https://alice.looklook.test", "devb", &right_keys)
            .await
            .expect("valid self-signed mux connect");
        assert_eq!(conn.path.kind, PathKind::Lan);

        // 2) 伪造的设备公钥：握手阶段必须因签名不匹配被拒绝
        let wrong_sk = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let wrong_pk_b64 = b64(wrong_sk.verifying_key().as_bytes());
        let wrong_keys = vec![DeviceKey { id: "k-devb".into(), public_key: wrong_pk_b64 }];
        let err = MuxConn::connect(&ep, &token, "https://alice.looklook.test", "devb", &wrong_keys)
            .await
            .err()
            .expect("wrong key must fail");
        assert!(err.to_string().contains("签名校验失败"), "unexpected error: {err}");

        shutdown.cancel();
    }
}
