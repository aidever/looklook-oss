//! 终端 WebSocket 输出流式 zstd 压缩（主进程 ↔ 浏览器之间；本机主进程 ↔ ttyd 不压缩）。
//!
//! 协商方式：WebSocket 子协议 `tty-zstd`。
//! - 新版浏览器握手时发 `Sec-WebSocket-Protocol: tty-zstd, tty`；
//! - 主进程网关向本机 ttyd 转发前去掉 `tty-zstd`（ttyd 只认 `tty`），并去掉 `Sec-WebSocket-Extensions`
//!   （避免叠加 permessage-deflate 二次压缩）；
//! - 本机 ttyd 接受 `tty` 后，网关把返回给浏览器的 `Sec-WebSocket-Protocol` 改为 `tty-zstd`，并在升级后的
//!   连接上只压缩 ttyd → 浏览器方向的 `OUTPUT`（首字节 `'0'`）负载；`SET_WINDOW_TITLE`（`'1'`）、
//!   `SET_PREFERENCES`（`'2'`）和浏览器 → ttyd 输入方向均原样转发，不改协议结构；
//! - 整条连接共用一个 zstd 流式压缩上下文（128 KiB 滑动窗口），每条输出消息 `flush()` 一次，到达浏览器即可
//!   立即解出，不攒批增加延迟；
//! - 旧前端只发 `tty`，或设置了 `LOOKLOOK_TERM_ZSTD=0` 时，不协商 `tty-zstd`，走原来的原始字节直通路径；
//! - WebRTC DataChannel 桥（`rtc/bridge.rs`）和 `rtc/outbound.rs` 会原样透传子协议与二进制消息；断线续传
//!   （`rtc/resume.rs`）按消息序号重发 `Outbox` 中已压缩的消息，与浏览器的流式解压状态精确对齐；续接失败或普通
//!   重连时会新建 WebSocket，两端同步重建压缩/解压上下文。

use std::io::{self, Write};
use std::sync::Arc;

use axum::http::{header, HeaderMap, HeaderValue};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::input_tap::{self, OnInput};

/// 协商启用终端输出 zstd 流式压缩的 WebSocket 子协议名。
pub const SUBPROTOCOL: &str = "tty-zstd";

/// 关闭终端 zstd 压缩的环境变量（设为 `0` / `false` / `off` / `no` 时关闭，默认开启）。
pub const ENV: &str = "LOOKLOOK_TERM_ZSTD";

/// ttyd 输出消息前缀字节（`OUTPUT = '0'`）。
const TTYD_OUTPUT: u8 = b'0';

/// 压缩级别（3 为 zstd 默认，压缩比与延迟均衡）。
const COMPRESSION_LEVEL: i32 = 3;

/// 滑动窗口 2^17 = 128 KiB：覆盖数十屏终端历史，同时把浏览器纯 JS 解码器（fzstd）的窗口内存与滑窗拷贝开销压在极低水平。
const WINDOW_LOG: u32 = 17;

/// fzstd 0.1.1 的流式解码器在解析帧头前需要缓冲满 18 字节；首条压缩消息不足时补空 Raw 块（每块 3 字节 `00 00 00`，标准 RFC 8878）。
const FZSTD_MIN_FIRST_CHUNK: usize = 18;
const EMPTY_RAW_BLOCK: [u8; 3] = [0x00, 0x00, 0x00];

/// 开关：环境变量未关闭时默认开启。
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV).ok().as_deref())
}

fn enabled_from(val: Option<&str>) -> bool {
    match val.map(str::trim) {
        Some(v)
            if v.eq_ignore_ascii_case("0")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off")
                || v.eq_ignore_ascii_case("no") =>
        {
            false
        }
        _ => true,
    }
}

/// 客户端请求头里是否声明支持 `tty-zstd`。
pub fn client_offers_zstd(h: &HeaderMap) -> bool {
    h.get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim().eq_ignore_ascii_case(SUBPROTOCOL))
}

/// 发往本机 ttyd 前去掉 `tty-zstd`（ttyd 只认 `tty`），保留其余子协议。
/// `fallback_tty` 为 true 时（已决定启用 zstd），若客户端只发了 `tty-zstd` 则补回 `tty`。
pub fn strip_zstd_subprotocol(h: &mut HeaderMap, fallback_tty: bool) {
    let kept: Vec<String> = h
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case(SUBPROTOCOL))
        .map(str::to_owned)
        .collect();
    h.remove(header::SEC_WEBSOCKET_PROTOCOL);
    let val = if !kept.is_empty() {
        Some(kept.join(", "))
    } else if fallback_tty {
        Some("tty".to_string())
    } else {
        None
    };
    if let Some(val) = val {
        if let Ok(v) = HeaderValue::from_str(&val) {
            h.insert(header::SEC_WEBSOCKET_PROTOCOL, v);
        }
    }
}

/// 单条终端连接共用的 zstd 流式压缩器：每次 `compress_chunk` 写入后立即 `flush()`，保证对端收到即可解出原文。
pub struct TermZstdEncoder {
    enc: zstd::stream::write::Encoder<'static, Vec<u8>>,
    first: bool,
}

impl TermZstdEncoder {
    pub fn new() -> io::Result<Self> {
        let mut enc = zstd::stream::write::Encoder::new(Vec::with_capacity(4096), COMPRESSION_LEVEL)?;
        enc.window_log(WINDOW_LOG)?;
        Ok(Self { enc, first: true })
    }

    /// 压缩一段终端输出字节并立刻 flush；空输入返回空 Vec。
    pub fn compress_chunk(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        self.enc.write_all(input)?;
        self.enc.flush()?;
        let mut out = std::mem::take(self.enc.get_mut());
        if self.first {
            self.first = false;
            while out.len() < FZSTD_MIN_FIRST_CHUNK {
                out.extend_from_slice(&EMPTY_RAW_BLOCK);
            }
        }
        Ok(out)
    }

    /// 处理一条 ttyd 发往浏览器的 WebSocket 消息：仅压缩 `OUTPUT`（`'0'`）负载，保留首字节与其余消息不变。
    pub fn compress_ttyd_message(&mut self, msg: Message) -> io::Result<Message> {
        match msg {
            Message::Binary(data) if data.first() == Some(&TTYD_OUTPUT) && data.len() > 1 => {
                let compressed = self.compress_chunk(&data[1..])?;
                let mut out = Vec::with_capacity(1 + compressed.len());
                out.push(TTYD_OUTPUT);
                out.extend_from_slice(&compressed);
                Ok(Message::Binary(out.into()))
            }
            other => Ok(other),
        }
    }
}

/// 升级后的终端 WebSocket 桥：
/// - `client → server`（输入）：不压缩，顺带触发 `on_input` 记录用户输入；
/// - `server → client`（输出）：流式 zstd 压缩 `OUTPUT` 消息，每条 flush 一次。
pub async fn bridge<C, S>(client_io: C, server_io: S, on_input: Option<OnInput>)
where
    C: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut encoder = match TermZstdEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "初始化终端 zstd 压缩器失败");
            return;
        }
    };
    let c_ws = WebSocketStream::from_raw_socket(client_io, Role::Server, None).await;
    let s_ws = WebSocketStream::from_raw_socket(server_io, Role::Client, None).await;
    let (mut c_sink, mut c_stream) = c_ws.split();
    let (mut s_sink, mut s_stream) = s_ws.split();
    // tungstenite 读到 Ping 时会自动把 Pong 排进本侧写缓冲；通过 Notify 立刻刷出，避免空闲时 ttyd 心跳超时
    let flush_c = Arc::new(Notify::new());
    let flush_s = Arc::new(Notify::new());

    let c_to_s = async {
        loop {
            tokio::select! {
                _ = flush_s.notified() => {
                    if s_sink.flush().await.is_err() {
                        break;
                    }
                }
                item = c_stream.next() => match item {
                    Some(Ok(Message::Ping(_))) => {
                        flush_c.notify_one();
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(msg @ Message::Close(_))) => {
                        let _ = s_sink.send(msg).await;
                        break;
                    }
                    Some(Ok(msg)) => {
                        if let Some(on) = &on_input {
                            let hit = match &msg {
                                Message::Binary(b) => input_tap::is_user_input_msg(b),
                                Message::Text(t) => input_tap::is_user_input_msg(t.as_bytes()),
                                _ => false,
                            };
                            if hit {
                                on();
                            }
                        }
                        if s_sink.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => {
                        let _ = s_sink.close().await;
                        break;
                    }
                }
            }
        }
    };

    let s_to_c = async {
        loop {
            tokio::select! {
                _ = flush_c.notified() => {
                    if c_sink.flush().await.is_err() {
                        break;
                    }
                }
                item = s_stream.next() => match item {
                    Some(Ok(Message::Ping(_))) => {
                        flush_s.notify_one();
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(msg @ Message::Close(_))) => {
                        let _ = c_sink.send(msg).await;
                        break;
                    }
                    Some(Ok(msg)) => {
                        let out = match encoder.compress_ttyd_message(msg) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::debug!(error = %e, "终端输出 zstd 压缩失败");
                                break;
                            }
                        };
                        if c_sink.send(out).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) | None => {
                        let _ = c_sink.close().await;
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = c_to_s => {}
        _ = s_to_c => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// 模拟网络收包缓冲区：每次只喂入单条消息的压缩字节，验证对端不等下一条消息就能立刻解出原文。
    #[derive(Default)]
    struct ChunkReader {
        buf: VecDeque<u8>,
    }

    impl ChunkReader {
        fn push(&mut self, data: &[u8]) {
            self.buf.extend(data);
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.buf.is_empty() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "empty"));
            }
            let n = out.len().min(self.buf.len());
            for slot in &mut out[..n] {
                *slot = self.buf.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    /// 用 libzstd 流式解码器逐条解压，验证每条 flush 的消息到达后都能立刻读出完整原文。
    fn decode_step(dec: &mut zstd::stream::read::Decoder<'_, io::BufReader<ChunkReader>>, compressed: &[u8], expected_len: usize) -> Vec<u8> {
        dec.get_mut().get_mut().push(compressed);
        let mut out = Vec::with_capacity(expected_len);
        let mut tmp = [0u8; 4096];
        while out.len() < expected_len {
            match dec.read(&mut tmp) {
                Ok(0) => panic!("unexpected EOF after {} / {} bytes", out.len(), expected_len),
                Ok(n) => out.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    panic!("message was not flushed completely: got {} / {} bytes", out.len(), expected_len);
                }
                Err(e) => panic!("zstd decode error: {e}"),
            }
        }
        out
    }

    #[test]
    fn streaming_flush_decodes_each_message_immediately() {
        let mut enc = TermZstdEncoder::new().unwrap();
        let mut dec = zstd::stream::read::Decoder::new(ChunkReader::default()).unwrap();

        let messages: Vec<Vec<u8>> = vec![
            // 首条极短（1 字节）：也必须满足 fzstd >= 18 字节要求并立即可解
            b"a".to_vec(),
            b"hello world\r\n".to_vec(),
            // 重复 ANSI 终端输出：共用压缩上下文应明显变小
            "\x1b[32muser@host\x1b[0m:\x1b[34m~/project\x1b[0m$ ls -la\r\n".repeat(40).into_bytes(),
            "\x1b[32muser@host\x1b[0m:\x1b[34m~/project\x1b[0m$ ls -la\r\n".repeat(40).into_bytes(),
            // 超过单个 128 KiB 块的大输出
            (0..200_000).map(|i| b"0123456789abcdef\n"[i % 17]).collect(),
            // 包含 UTF-8 中文和二进制字节（如 trzsz 协议头）
            "中文输出 \x1b[1;31m错误\x1b[0m ::TRZSZ:TRANSFER:R:1.1.6:0\r\n".as_bytes().to_vec(),
        ];

        let mut compressed_sizes = Vec::new();
        for (idx, msg) in messages.iter().enumerate() {
            let z = enc.compress_chunk(msg).unwrap();
            if idx == 0 {
                assert!(z.len() >= FZSTD_MIN_FIRST_CHUNK, "first chunk {} < 18", z.len());
            }
            compressed_sizes.push(z.len());
            let decoded = decode_step(&mut dec, &z, msg.len());
            assert_eq!(decoded, *msg, "mismatch at message #{idx}");
        }

        // 第二次相同的大段输出受益于共享历史窗口，应比第一次压得更小
        assert!(
            compressed_sizes[3] <= compressed_sizes[2],
            "expected shared context to help: first={} second={}",
            compressed_sizes[2],
            compressed_sizes[3]
        );
        // 大输出应有显著压缩比
        assert!(compressed_sizes[4] < 2000, "200KB repetitive output compressed to {} bytes", compressed_sizes[4]);
    }

    #[test]
    fn ruzstd_also_decodes_flushed_stream() {
        // 用另一个独立实现（ruzstd）验证标准 RFC 8878 兼容性（含首块补空 Raw 块）
        let mut enc = TermZstdEncoder::new().unwrap();
        let parts = [b"x".as_slice(), b"\x1b[2J\x1b[Hprompt$ ", b"echo ok\r\nok\r\n"];
        let mut stream = Vec::new();
        for p in parts {
            stream.extend_from_slice(&enc.compress_chunk(p).unwrap());
        }
        // 追加空 Last_Block 结束帧以便 StreamingDecoder 读到 EOF
        stream.extend_from_slice(&[0x01, 0x00, 0x00]);
        let mut dec = ruzstd::decoding::StreamingDecoder::new(stream.as_slice()).unwrap();
        let mut all = Vec::new();
        dec.read_to_end(&mut all).unwrap();
        assert_eq!(all, parts.concat());
    }

    #[test]
    fn compress_ttyd_message_only_compresses_output_payload() {
        let mut enc = TermZstdEncoder::new().unwrap();
        // SET_WINDOW_TITLE ('1') 与 SET_PREFERENCES ('2') 原样保留
        let title = Message::Binary(b"1my-term".to_vec().into());
        assert_eq!(enc.compress_ttyd_message(title.clone()).unwrap(), title);
        let prefs = Message::Binary(b"2{\"enableTrzsz\":true}".to_vec().into());
        assert_eq!(enc.compress_ttyd_message(prefs.clone()).unwrap(), prefs);
        // 空 OUTPUT ('0') 原样保留
        let empty = Message::Binary(b"0".to_vec().into());
        assert_eq!(enc.compress_ttyd_message(empty.clone()).unwrap(), empty);
        // 非空 OUTPUT ('0' + payload) 保留首字节 '0' 并压缩后面
        let payload = b"0hello terminal output";
        let out = enc.compress_ttyd_message(Message::Binary(payload.to_vec().into())).unwrap();
        let Message::Binary(bytes) = out else { panic!("expected binary") };
        assert_eq!(bytes[0], b'0');
        let mut dec = zstd::stream::read::Decoder::new(ChunkReader::default()).unwrap();
        assert_eq!(decode_step(&mut dec, &bytes[1..], payload.len() - 1), &payload[1..]);
    }

    #[test]
    fn subprotocol_negotiation_and_env_toggle() {
        let mut h = HeaderMap::new();
        assert!(!client_offers_zstd(&h));
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty"));
        assert!(!client_offers_zstd(&h));

        h.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd, tty"));
        assert!(client_offers_zstd(&h));
        strip_zstd_subprotocol(&mut h, true);
        assert_eq!(h[header::SEC_WEBSOCKET_PROTOCOL], "tty");

        let mut only = HeaderMap::new();
        only.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd"));
        strip_zstd_subprotocol(&mut only, true);
        assert_eq!(only[header::SEC_WEBSOCKET_PROTOCOL], "tty");

        let mut only_no_fb = HeaderMap::new();
        only_no_fb.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd"));
        strip_zstd_subprotocol(&mut only_no_fb, false);
        assert!(!only_no_fb.contains_key(header::SEC_WEBSOCKET_PROTOCOL));

        let mut both_no_fb = HeaderMap::new();
        both_no_fb.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd, tty"));
        strip_zstd_subprotocol(&mut both_no_fb, false);
        assert_eq!(both_no_fb[header::SEC_WEBSOCKET_PROTOCOL], "tty");

        assert!(enabled_from(None));
        assert!(enabled_from(Some("")));
        assert!(enabled_from(Some("1")));
        assert!(enabled_from(Some("true")));
        for off in ["0", "false", "FALSE", "off", "no", " 0 "] {
            assert!(!enabled_from(Some(off)), "should disable for {off:?}");
        }
    }

    #[tokio::test]
    async fn bridge_compresses_output_and_keeps_input_raw_with_tap() {
        let (c_peer, c_srv) = tokio::io::duplex(64 * 1024);
        let (s_cli, s_peer) = tokio::io::duplex(64 * 1024);
        let hits = Arc::new(AtomicUsize::new(0));
        let on_input: OnInput = {
            let hits = hits.clone();
            Arc::new(move || {
                hits.fetch_add(1, Ordering::Relaxed);
            })
        };

        let task = tokio::spawn(bridge(c_srv, s_cli, Some(on_input)));
        let mut browser = WebSocketStream::from_raw_socket(c_peer, Role::Client, None).await;
        let mut ttyd = WebSocketStream::from_raw_socket(s_peer, Role::Server, None).await;

        // 1. ttyd 发 prefs / title（不压）+ 多条 output（压）
        ttyd.send(Message::Binary(b"2{\"enableTrzsz\":true}".to_vec().into())).await.unwrap();
        ttyd.send(Message::Binary(b"1zsh".to_vec().into())).await.unwrap();
        assert_eq!(browser.next().await.unwrap().unwrap(), Message::Binary(b"2{\"enableTrzsz\":true}".to_vec().into()));
        assert_eq!(browser.next().await.unwrap().unwrap(), Message::Binary(b"1zsh".to_vec().into()));

        let mut dec = zstd::stream::read::Decoder::new(ChunkReader::default()).unwrap();
        for chunk in [b"prompt$ ".as_slice(), b"ls\r\n", "file1  file2  中文\r\n".as_bytes()] {
            let mut raw = vec![b'0'];
            raw.extend_from_slice(chunk);
            ttyd.send(Message::Binary(raw.into())).await.unwrap();
            let Message::Binary(got) = browser.next().await.unwrap().unwrap() else { panic!("binary") };
            assert_eq!(got[0], b'0');
            assert_eq!(decode_step(&mut dec, &got[1..], chunk.len()), chunk);
        }

        // 2. 浏览器发输入（不压）且触发 on_input；焦点应答不触发
        browser.send(Message::Binary(b"0\x1b[I".to_vec().into())).await.unwrap();
        assert_eq!(ttyd.next().await.unwrap().unwrap(), Message::Binary(b"0\x1b[I".to_vec().into()));
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        browser.send(Message::Binary(b"0ls\r".to_vec().into())).await.unwrap();
        assert_eq!(ttyd.next().await.unwrap().unwrap(), Message::Binary(b"0ls\r".to_vec().into()));
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        browser.close(None).await.unwrap();
        let _ = task.await;
    }

    /// 模拟本机 ttyd：只认 `tty` 子协议，收到输入后回显 `0<payload>`（模拟终端输出）。
    async fn mock_ttyd_server() -> u16 {
        use axum::body::Body;
        use axum::extract::Request;
        use axum::http::StatusCode;
        use axum::response::Response;
        use axum::routing::get;
        use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

        let app = axum::Router::new().route(
            "/i/t1/ws",
            get(|mut req: Request| async move {
                let proto = req.headers().get(header::SEC_WEBSOCKET_PROTOCOL).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                // 本机 ttyd 只认 `tty`，绝不能看到 `tty-zstd`
                assert_eq!(proto, "tty", "upstream ttyd must only see tty subprotocol");
                let key = req.headers()[header::SEC_WEBSOCKET_KEY].clone();
                let upgrade = hyper::upgrade::on(&mut req);
                tokio::spawn(async move {
                    let io = upgrade.await.unwrap();
                    let mut ws = WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(io), Role::Server, None).await;
                    let _ = ws.send(Message::Binary(b"1ttyd-title".to_vec().into())).await;
                    while let Some(Ok(m)) = ws.next().await {
                        match m {
                            Message::Close(_) => break,
                            Message::Binary(b) => {
                                if ws.send(Message::Binary(b)).await.is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                });
                Response::builder()
                    .status(StatusCode::SWITCHING_PROTOCOLS)
                    .header(header::CONNECTION, "upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header(header::SEC_WEBSOCKET_ACCEPT, derive_accept_key(key.as_bytes()))
                    .header(header::SEC_WEBSOCKET_PROTOCOL, "tty")
                    .body(Body::empty())
                    .unwrap()
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        port
    }

    async fn start_proxy(ttyd_port: u16, zstd_enabled: bool) -> std::net::SocketAddr {
        use axum::extract::Request;
        use axum::routing::get;
        let client = super::super::proxy::client();
        let app = axum::Router::new().route(
            "/i/t1/ws",
            get(move |req: Request| {
                let client = client.clone();
                async move {
                    let opts = super::super::proxy::Opts {
                        auth: None,
                        host: None,
                        inject: None,
                        rewrite_location: None,
                        terminal: true,
                        on_input: None,
                        keep: None,
                    };
                    super::super::proxy::forward_inner(&client, req, ttyd_port, opts, zstd_enabled).await
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn proxy_negotiation_enable_fallback_disable_and_reconnect() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let ttyd_port = mock_ttyd_server().await;
        let proxy_on = start_proxy(ttyd_port, true).await;
        let proxy_off = start_proxy(ttyd_port, false).await;

        // 1. 新前端（tty-zstd, tty）+ 开关开启 → 协商 tty-zstd，输出压缩、输入原样；重连后重建独立压缩上下文
        for round in 0..2 {
            let mut req = format!("ws://{proxy_on}/i/t1/ws").into_client_request().unwrap();
            req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd, tty"));
            req.headers_mut().insert(header::SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_static("permessage-deflate"));
            let tcp = tokio::net::TcpStream::connect(proxy_on).await.unwrap();
            let (mut ws, resp) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
            assert_eq!(resp.headers()[header::SEC_WEBSOCKET_PROTOCOL], SUBPROTOCOL);
            assert!(!resp.headers().contains_key(header::SEC_WEBSOCKET_EXTENSIONS));
            assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"1ttyd-title".to_vec().into()));
            // 每轮重连都新建独立的解压上下文
            let mut dec = zstd::stream::read::Decoder::new(ChunkReader::default()).unwrap();
            for chunk in [format!("round-{round}-a").into_bytes(), format!("round-{round}-hello-terminal\r\n").into_bytes()] {
                let mut frame = vec![b'0'];
                frame.extend_from_slice(&chunk);
                ws.send(Message::Binary(frame.into())).await.unwrap();
                let Message::Binary(got) = ws.next().await.unwrap().unwrap() else { panic!("binary") };
                assert_eq!(got[0], b'0');
                assert_eq!(decode_step(&mut dec, &got[1..], chunk.len()), chunk);
            }
            ws.close(None).await.unwrap();
        }

        // 2. 旧前端（只发 tty）→ 协商 tty，收原始字节
        let mut req = format!("ws://{proxy_on}/i/t1/ws").into_client_request().unwrap();
        req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty"));
        let tcp = tokio::net::TcpStream::connect(proxy_on).await.unwrap();
        let (mut ws, resp) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
        assert_eq!(resp.headers()[header::SEC_WEBSOCKET_PROTOCOL], "tty");
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"1ttyd-title".to_vec().into()));
        ws.send(Message::Binary(b"0raw-old-client".to_vec().into())).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"0raw-old-client".to_vec().into()));
        ws.close(None).await.unwrap();

        // 3. 开关关闭（LOOKLOOK_TERM_ZSTD=0 等价）→ 即使新前端发 tty-zstd, tty 也回退 tty，收原始字节
        let mut req = format!("ws://{proxy_off}/i/t1/ws").into_client_request().unwrap();
        req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("tty-zstd, tty"));
        let tcp = tokio::net::TcpStream::connect(proxy_off).await.unwrap();
        let (mut ws, resp) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
        assert_eq!(resp.headers()[header::SEC_WEBSOCKET_PROTOCOL], "tty");
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"1ttyd-title".to_vec().into()));
        ws.send(Message::Binary(b"0raw-disabled".to_vec().into())).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::Binary(b"0raw-disabled".to_vec().into()));
        ws.close(None).await.unwrap();
    }
}
