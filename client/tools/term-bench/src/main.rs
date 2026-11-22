//! 现有终端链路（WS 客户端 → looklook gateway → ttyd → tmux）基准测量工具。
//!
//! 测量项目（对应设计稿第 10 节第 0 步）：
//! 1. 打字回显延迟（单字符往返 RTT，p50 / p95 / p99 / min / max / mean）；
//! 2. vim 翻页（Ctrl+F）下行字节数与帧数；
//! 3. htop（缺失时退回 top）持续刷新下行字节数与帧率；
//! 4. cat 大文件下行字节数、放大倍数与耗时。
//!
//! 无浏览器运行：通过 `POST /api/instances`（带 `X-Looklook-Client: 1`）建临时 shell 会话，
//! 拉取 `/i/{id}/token`，以子协议 `tty` 直连 `/i/{id}/ws` 完成 ttyd 握手。

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CLIENT_HDR: &str = "x-looklook-client";

// ttyd WebSocket 协议首字节
const CMD_OUTPUT: u8 = b'0';
const CMD_SET_TITLE: u8 = b'1';
const CMD_SET_PREFS: u8 = b'2';
const CMD_INPUT: u8 = b'0';
const CMD_RESIZE: u8 = b'1';

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Only {
    All,
    Echo,
    Vim,
    Htop,
    Cat,
}

#[derive(Parser)]
#[command(name = "term-bench", about = "测量 looklook gateway → ttyd → tmux 终端链路延迟与下行字节数")]
struct Cli {
    /// 看看网关地址（本机默认 http://127.0.0.1:1234）
    #[arg(long, default_value = "http://127.0.0.1:1234", env = "LOOKLOOK_URL")]
    url: String,
    /// 复用已有终端实例 ID；不传则自动创建临时 shell 实例并在结束后删除
    #[arg(long)]
    instance: Option<String>,
    /// 若经局域网/白名单且启用了访问码，可传入访问码获取 ll_access Cookie
    #[arg(long)]
    access_code: Option<String>,
    /// 终端列数
    #[arg(long, default_value_t = 120)]
    cols: u16,
    /// 终端行数
    #[arg(long, default_value_t = 36)]
    rows: u16,
    /// 打字回显采样次数（不含 10 次预热）
    #[arg(long, default_value_t = 80)]
    rounds: usize,
    /// vim 翻页（Ctrl+F）次数
    #[arg(long, default_value_t = 10)]
    vim_pages: usize,
    /// htop 持续采集秒数
    #[arg(long, default_value_t = 6)]
    htop_secs: u64,
    /// cat 生成测试文件的行数（每行 ~103 字节）
    #[arg(long, default_value_t = 20000)]
    cat_lines: usize,
    /// 只跑某一项
    #[arg(long, value_enum, default_value_t = Only::All)]
    only: Only,
    /// 输出 JSON 报告
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct BenchReport {
    gateway_url: String,
    cols: u16,
    rows: u16,
    echo: Option<EchoStats>,
    vim: Option<VimStats>,
    htop: Option<HtopStats>,
    cat: Option<CatStats>,
}

#[derive(Debug, Serialize)]
struct EchoStats {
    rounds: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    avg_output_bytes_per_char: f64,
}

#[derive(Debug, Serialize)]
struct VimStats {
    pages: usize,
    total_output_bytes: usize,
    total_frames: usize,
    bytes_per_page: f64,
    frames_per_page: f64,
}

#[derive(Debug, Serialize)]
struct HtopStats {
    program: String,
    duration_secs: f64,
    total_output_bytes: usize,
    total_frames: usize,
    bytes_per_sec: f64,
    frames_per_sec: f64,
    bursts: usize,
    bytes_per_burst: f64,
}

#[derive(Debug, Serialize)]
struct CatStats {
    file_bytes: usize,
    total_output_bytes: usize,
    total_frames: usize,
    output_to_file_ratio: f64,
    elapsed_ms: f64,
    throughput_mb_per_sec: f64,
}

#[derive(Default)]
struct Capture {
    bytes: usize,
    frames: usize,
    bursts: usize,
    data: Vec<u8>,
}

fn http_to_ws(base: &str, path: &str) -> Result<String> {
    let trimmed = base.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        Ok(format!("wss://{rest}{path}"))
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        Ok(format!("ws://{rest}{path}"))
    } else {
        bail!("无效的网关 URL（需要 http:// 或 https://）: {base}");
    }
}

fn encode_input(bytes: &[u8]) -> Message {
    let mut buf = Vec::with_capacity(bytes.len() + 1);
    buf.push(CMD_INPUT);
    buf.extend_from_slice(bytes);
    Message::Binary(buf.into())
}

fn encode_resize(cols: u16, rows: u16) -> Message {
    let mut buf = vec![CMD_RESIZE];
    buf.extend_from_slice(json!({ "columns": cols, "rows": rows }).to_string().as_bytes());
    Message::Binary(buf.into())
}

fn encode_auth(token: &str, cols: u16, rows: u16) -> Message {
    let payload = json!({ "AuthToken": token, "columns": cols, "rows": rows }).to_string();
    Message::Binary(payload.into_bytes().into())
}

fn decode_output_frame(msg: &Message) -> Option<&[u8]> {
    match msg {
        Message::Binary(b) if !b.is_empty() && b[0] == CMD_OUTPUT => Some(&b[1..]),
        _ => None,
    }
}

/// 线性插值百分位（输入需已排序）。
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

async fn login_cookie(http: &reqwest::Client, base: &str, code: &str) -> Result<Option<String>> {
    let resp = http
        .post(format!("{}/__looklook/access", base.trim_end_matches('/')))
        .form(&[("code", code), ("next", "/")])
        .send()
        .await
        .context("提交访问码")?;
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = v.to_str() {
            if let Some(kv) = s.split(';').next() {
                if kv.starts_with("ll_access=") {
                    return Ok(Some(kv.to_string()));
                }
            }
        }
    }
    Ok(None)
}

async fn create_temp_instance(http: &reqwest::Client, base: &str, cookie: Option<&str>) -> Result<String> {
    let name = format!("term-bench-{}", std::process::id());
    let mut req = http
        .post(format!("{}/api/instances", base.trim_end_matches('/')))
        .header(CLIENT_HDR, "1")
        .json(&json!({
            "name": name,
            "launch": "shell",
            "workdir": "/tmp",
            "start": true,
        }));
    if let Some(c) = cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req.send().await.context("POST /api/instances")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("解析实例创建响应")?;
    if !status.is_success() {
        bail!("创建临时终端实例失败 ({status}): {body}");
    }
    let id = body["id"].as_str().context("响应缺少 id")?.to_string();
    Ok(id)
}

async fn ensure_started(http: &reqwest::Client, base: &str, id: &str, cookie: Option<&str>) -> Result<()> {
    let mut req = http
        .post(format!("{}/api/instances/{id}/start", base.trim_end_matches('/')))
        .header(CLIENT_HDR, "1");
    if let Some(c) = cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req.send().await.context("POST /api/instances/{id}/start")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("启动终端实例 {id} 失败 ({status}): {text}");
    }
    Ok(())
}

async fn delete_instance(http: &reqwest::Client, base: &str, id: &str, cookie: Option<&str>) {
    let mut req = http
        .delete(format!("{}/api/instances/{id}", base.trim_end_matches('/')))
        .header(CLIENT_HDR, "1");
    if let Some(c) = cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let _ = req.send().await;
}

async fn fetch_ttyd_token(http: &reqwest::Client, base: &str, id: &str, cookie: Option<&str>) -> Result<String> {
    let mut req = http.get(format!("{}/i/{id}/token", base.trim_end_matches('/')));
    if let Some(c) = cookie {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req.send().await.context("GET /i/{id}/token")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("获取 /i/{id}/token 失败 ({status}): {text}");
    }
    let v: Value = resp.json().await.context("解析 /i/{id}/token")?;
    Ok(v["token"].as_str().unwrap_or_default().to_string())
}

async fn connect_ttyd_ws(base: &str, id: &str, token: &str, cols: u16, rows: u16, cookie: Option<&str>) -> Result<WsStream> {
    let ws_url = http_to_ws(base, &format!("/i/{id}/ws"))?;
    let mut req = ws_url.as_str().into_client_request()?;
    req.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("tty"));
    if let Some(c) = cookie {
        req.headers_mut().insert("cookie", HeaderValue::from_str(c)?);
    }

    let (mut ws, _) = tokio_tungstenite::connect_async_with_config(req, None, true)
        .await
        .with_context(|| format!("连接 WS 失败: {ws_url}"))?;

    ws.send(encode_auth(token, cols, rows)).await?;
    ws.send(encode_resize(cols, rows)).await?;
    Ok(ws)
}

async fn drain_quiet(ws: &mut WsStream, quiet: Duration, max: Duration) -> Result<Capture> {
    let mut cap = Capture::default();
    let deadline = Instant::now() + max;
    let mut last_frame: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = (deadline - now).min(quiet);
        match tokio::time::timeout(wait, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Some(payload) = decode_output_frame(&msg) {
                    if last_frame.is_none_or(|t| t.elapsed() > Duration::from_millis(80)) {
                        cap.bursts += 1;
                    }
                    last_frame = Some(Instant::now());
                    cap.frames += 1;
                    cap.bytes += payload.len();
                    cap.data.extend_from_slice(payload);
                } else if matches!(msg, Message::Close(_)) {
                    bail!("WebSocket 被关闭");
                }
            }
            Ok(Some(Err(e))) => return Err(e).context("WS read"),
            Ok(None) => bail!("WebSocket 流结束"),
            Err(_) => break,
        }
    }
    Ok(cap)
}

async fn drain_for(ws: &mut WsStream, duration: Duration) -> Result<Capture> {
    let mut cap = Capture::default();
    let deadline = Instant::now() + duration;
    let mut last_frame: Option<Instant> = None;
    while Instant::now() < deadline {
        let rem = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(rem, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Some(payload) = decode_output_frame(&msg) {
                    if last_frame.is_none_or(|t| t.elapsed() > Duration::from_millis(120)) {
                        cap.bursts += 1;
                    }
                    last_frame = Some(Instant::now());
                    cap.frames += 1;
                    cap.bytes += payload.len();
                } else if matches!(msg, Message::Close(_)) {
                    bail!("WebSocket 被关闭");
                }
            }
            Ok(Some(Err(e))) => return Err(e).context("WS read"),
            Ok(None) => bail!("WebSocket 流结束"),
            Err(_) => break,
        }
    }
    Ok(cap)
}

async fn drain_until(ws: &mut WsStream, marker: &[u8], timeout: Duration) -> Result<Capture> {
    let mut cap = Capture::default();
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let tail = String::from_utf8_lossy(&cap.data[cap.data.len().saturating_sub(400)..]);
            bail!("等待标记 {} 超时（已收 {} 字节），尾部: {:?}", String::from_utf8_lossy(marker), cap.bytes, tail);
        }
        let rem = deadline - now;
        match tokio::time::timeout(rem, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Some(payload) = decode_output_frame(&msg) {
                    cap.frames += 1;
                    cap.bytes += payload.len();
                    let prev_len = cap.data.len();
                    cap.data.extend_from_slice(payload);
                    let scan_from = prev_len.saturating_sub(marker.len().saturating_sub(1));
                    if cap.data[scan_from..].windows(marker.len()).any(|w| w == marker) {
                        return Ok(cap);
                    }
                } else if let Message::Binary(b) = &msg {
                    if b.first() == Some(&CMD_SET_TITLE) || b.first() == Some(&CMD_SET_PREFS) {
                        continue;
                    }
                } else if matches!(msg, Message::Close(_)) {
                    bail!("WebSocket 被关闭");
                }
            }
            Ok(Some(Err(e))) => return Err(e).context("WS read"),
            Ok(None) => bail!("WebSocket 流结束"),
            Err(_) => {}
        }
    }
}

async fn run_cmd_wait(ws: &mut WsStream, cmd: &str, marker: &str, timeout: Duration) -> Result<Capture> {
    ws.send(encode_input(format!("{cmd}\r").as_bytes())).await?;
    drain_until(ws, marker.as_bytes(), timeout).await
}

async fn prepare_shell(ws: &mut WsStream) -> Result<()> {
    let _ = drain_quiet(ws, Duration::from_millis(350), Duration::from_millis(2500)).await?;
    run_cmd_wait(ws, "export PS1='BENCH> '; echo __BENCH_\"\"READY__", "__BENCH_READY__", Duration::from_secs(5)).await?;
    let _ = drain_quiet(ws, Duration::from_millis(150), Duration::from_millis(800)).await?;
    Ok(())
}

async fn bench_echo(ws: &mut WsStream, rounds: usize) -> Result<EchoStats> {
    // 预热 10 次按键
    for i in 0..10usize {
        let ch = b'a' + (i % 26) as u8;
        ws.send(encode_input(&[ch])).await?;
        let _ = drain_until(ws, &[ch], Duration::from_secs(3)).await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ws.send(encode_input(&[0x15])).await?; // Ctrl+U 清行
    let _ = drain_quiet(ws, Duration::from_millis(120), Duration::from_millis(600)).await?;

    let mut samples_ms = Vec::with_capacity(rounds);
    let mut total_out_bytes = 0usize;

    for i in 0..rounds {
        if i > 0 && i % 32 == 0 {
            ws.send(encode_input(&[0x15])).await?;
            let _ = drain_quiet(ws, Duration::from_millis(80), Duration::from_millis(400)).await?;
        }
        let ch = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        ws.send(encode_input(&[ch])).await?;
        let cap = drain_until(ws, &[ch], Duration::from_secs(3)).await?;
        let rtt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        samples_ms.push(rtt_ms);
        total_out_bytes += cap.bytes;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    ws.send(encode_input(&[0x15])).await?;
    let _ = drain_quiet(ws, Duration::from_millis(120), Duration::from_millis(600)).await?;

    samples_ms.sort_by(|a, b| a.total_cmp(b));
    let sum: f64 = samples_ms.iter().sum();
    Ok(EchoStats {
        rounds,
        min_ms: samples_ms[0],
        p50_ms: percentile(&samples_ms, 0.50),
        p95_ms: percentile(&samples_ms, 0.95),
        p99_ms: percentile(&samples_ms, 0.99),
        max_ms: *samples_ms.last().unwrap_or(&0.0),
        mean_ms: sum / rounds.max(1) as f64,
        avg_output_bytes_per_char: total_out_bytes as f64 / rounds.max(1) as f64,
    })
}

async fn bench_vim(ws: &mut WsStream, pages: usize) -> Result<VimStats> {
    let gen = r#"awk 'BEGIN{for(i=1;i<=800;i++) printf "%04d fn demo_handler_%d(ctx: &Context, n: usize) -> usize { let v = n * 3 + %d; v ^ 0x55aa }\n", i, i, i%97}' > /tmp/ll_bench_vim.rs && echo __VIM_""READY__"#;
    run_cmd_wait(ws, gen, "__VIM_READY__", Duration::from_secs(5)).await?;
    let _ = drain_quiet(ws, Duration::from_millis(150), Duration::from_millis(500)).await?;

    ws.send(encode_input(b"vim -Nu NONE -n -c \"syntax on\" /tmp/ll_bench_vim.rs\r")).await?;
    let _ = drain_quiet(ws, Duration::from_millis(450), Duration::from_secs(3)).await?;

    let mut total_output_bytes = 0usize;
    let mut total_frames = 0usize;
    for _ in 0..pages {
        ws.send(encode_input(&[0x06])).await?; // Ctrl+F
        let cap = drain_quiet(ws, Duration::from_millis(220), Duration::from_millis(1500)).await?;
        total_output_bytes += cap.bytes;
        total_frames += cap.frames;
    }

    ws.send(encode_input(b"\x1b:q!\r")).await?;
    let _ = drain_quiet(ws, Duration::from_millis(250), Duration::from_secs(2)).await?;
    run_cmd_wait(ws, "rm -f /tmp/ll_bench_vim.rs && echo __VIM_\"\"CLEAN__", "__VIM_CLEAN__", Duration::from_secs(5)).await?;

    Ok(VimStats {
        pages,
        total_output_bytes,
        total_frames,
        bytes_per_page: total_output_bytes as f64 / pages.max(1) as f64,
        frames_per_page: total_frames as f64 / pages.max(1) as f64,
    })
}

async fn bench_htop(ws: &mut WsStream, secs: u64) -> Result<HtopStats> {
    let probe = run_cmd_wait(
        ws,
        "command -v htop >/dev/null && echo __USE_HTOP_\"\"OK__ || echo __USE_TOP_\"\"OK__",
        "_OK__",
        Duration::from_secs(5),
    )
    .await?;
    let _ = drain_quiet(ws, Duration::from_millis(150), Duration::from_millis(500)).await?;
    let has_htop = String::from_utf8_lossy(&probe.data).contains("__USE_HTOP_OK__");
    let (program, launch_cmd) = if has_htop {
        ("htop".to_string(), "htop -d 10\r")
    } else {
        ("top".to_string(), "top -d 1\r")
    };

    ws.send(encode_input(launch_cmd.as_bytes())).await?;
    // 跳过首屏初始化绘制
    let _ = drain_for(ws, Duration::from_millis(1500)).await?;

    let t0 = Instant::now();
    let cap = drain_for(ws, Duration::from_secs(secs)).await?;
    let actual_secs = t0.elapsed().as_secs_f64();

    ws.send(encode_input(b"q")).await?;
    let _ = drain_quiet(ws, Duration::from_millis(300), Duration::from_secs(2)).await?;

    Ok(HtopStats {
        program,
        duration_secs: actual_secs,
        total_output_bytes: cap.bytes,
        total_frames: cap.frames,
        bytes_per_sec: cap.bytes as f64 / actual_secs.max(0.001),
        frames_per_sec: cap.frames as f64 / actual_secs.max(0.001),
        bursts: cap.bursts,
        bytes_per_burst: if cap.bursts > 0 { cap.bytes as f64 / cap.bursts as f64 } else { 0.0 },
    })
}

async fn bench_cat(ws: &mut WsStream, lines: usize) -> Result<CatStats> {
    let gen = format!(
        "awk -v n={lines} 'BEGIN{{for(i=1;i<=n;i++) printf \"[%06d] 2026-11-20T22:00:00Z INFO looklook::bench throughput payload abcdefghijklmnopqrstuvwxyz0123456789\\n\", i}}' > /tmp/ll_bench_cat.log && printf '__CAT_SIZE''=%s__''DONE\\n' \"$(wc -c < /tmp/ll_bench_cat.log)\""
    );
    let prep = run_cmd_wait(ws, &gen, "__DONE", Duration::from_secs(15)).await?;
    let _ = drain_quiet(ws, Duration::from_millis(180), Duration::from_millis(800)).await?;

    let prep_str = String::from_utf8_lossy(&prep.data);
    let file_bytes = prep_str
        .rsplit("__CAT_SIZE=")
        .next()
        .and_then(|s| s.split("__").next())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(lines * 103);

    let t0 = Instant::now();
    ws.send(encode_input(b"cat /tmp/ll_bench_cat.log && echo __CAT_\"\"DONE__\r")).await?;
    let cap = drain_until(ws, b"__CAT_DONE__", Duration::from_secs(60)).await?;
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let _ = drain_quiet(ws, Duration::from_millis(150), Duration::from_millis(600)).await?;
    run_cmd_wait(ws, "rm -f /tmp/ll_bench_cat.log && echo __CAT_\"\"CLEAN__", "__CAT_CLEAN__", Duration::from_secs(5)).await?;

    let secs = (elapsed_ms / 1000.0).max(0.0001);
    Ok(CatStats {
        file_bytes,
        total_output_bytes: cap.bytes,
        total_frames: cap.frames,
        output_to_file_ratio: cap.bytes as f64 / file_bytes.max(1) as f64,
        elapsed_ms,
        throughput_mb_per_sec: (cap.bytes as f64 / (1024.0 * 1024.0)) / secs,
    })
}

fn print_report(r: &BenchReport) {
    println!("=== 终端现有链路基准测量 (WS → gateway → ttyd → tmux) ===");
    println!("网关: {} | 尺寸: {}x{}", r.gateway_url, r.cols, r.rows);
    if let Some(e) = &r.echo {
        println!(
            "[1] 打字回显延迟 (n={}): p50={:.2} ms, p95={:.2} ms, p99={:.2} ms, min={:.2} ms, max={:.2} ms, mean={:.2} ms, avg_bytes={:.1} B/char",
            e.rounds, e.p50_ms, e.p95_ms, e.p99_ms, e.min_ms, e.max_ms, e.mean_ms, e.avg_output_bytes_per_char
        );
    }
    if let Some(v) = &r.vim {
        println!(
            "[2] vim 翻页 ({} 页): 总计 {} B ({:.1} B/页), {} 帧 ({:.1} 帧/页)",
            v.pages, v.total_output_bytes, v.bytes_per_page, v.total_frames, v.frames_per_page
        );
    }
    if let Some(h) = &r.htop {
        println!(
            "[3] {} 刷新 ({:.2}s): 总计 {} B, {:.1} B/s ({:.2} KB/s), {:.1} 帧/s, 约 {} 轮刷新 (~{:.1} B/轮)",
            h.program,
            h.duration_secs,
            h.total_output_bytes,
            h.bytes_per_sec,
            h.bytes_per_sec / 1024.0,
            h.frames_per_sec,
            h.bursts,
            h.bytes_per_burst
        );
    }
    if let Some(c) = &r.cat {
        println!(
            "[4] cat 大文件 (原文件 {} B): 下行 {} B (ratio={:.3}), {} 帧, 耗时 {:.1} ms, 吞吐 {:.2} MB/s",
            c.file_bytes, c.total_output_bytes, c.output_to_file_ratio, c.total_frames, c.elapsed_ms, c.throughput_mb_per_sec
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;

    let cookie = match cli.access_code.as_deref() {
        Some(code) => login_cookie(&http, &cli.url, code).await?,
        None => None,
    };

    let (id, created_temp) = match &cli.instance {
        Some(id) => {
            ensure_started(&http, &cli.url, id, cookie.as_deref()).await?;
            (id.clone(), false)
        }
        None => (create_temp_instance(&http, &cli.url, cookie.as_deref()).await?, true),
    };

    let run_res: Result<BenchReport> = async {
        let token = fetch_ttyd_token(&http, &cli.url, &id, cookie.as_deref()).await?;
        let mut ws = connect_ttyd_ws(&cli.url, &id, &token, cli.cols, cli.rows, cookie.as_deref()).await?;
        prepare_shell(&mut ws).await?;

        let echo = if matches!(cli.only, Only::All | Only::Echo) {
            Some(bench_echo(&mut ws, cli.rounds).await?)
        } else {
            None
        };
        let vim = if matches!(cli.only, Only::All | Only::Vim) {
            Some(bench_vim(&mut ws, cli.vim_pages).await?)
        } else {
            None
        };
        let htop = if matches!(cli.only, Only::All | Only::Htop) {
            Some(bench_htop(&mut ws, cli.htop_secs).await?)
        } else {
            None
        };
        let cat = if matches!(cli.only, Only::All | Only::Cat) {
            Some(bench_cat(&mut ws, cli.cat_lines).await?)
        } else {
            None
        };

        let _ = ws.close(None).await;
        Ok(BenchReport {
            gateway_url: cli.url.clone(),
            cols: cli.cols,
            rows: cli.rows,
            echo,
            vim,
            htop,
            cat,
        })
    }
    .await;

    if created_temp {
        delete_instance(&http, &cli.url, &id, cookie.as_deref()).await;
    }

    let report = run_res?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_mapping() {
        assert_eq!(http_to_ws("http://127.0.0.1:1234/", "/i/abc/ws").unwrap(), "ws://127.0.0.1:1234/i/abc/ws");
        assert_eq!(http_to_ws("https://demo.example", "/i/abc/ws").unwrap(), "wss://demo.example/i/abc/ws");
        assert!(http_to_ws("ftp://bad", "/ws").is_err());
    }

    #[test]
    fn ttyd_frame_encoding_and_decoding() {
        let Message::Binary(inp) = encode_input(b"ls\n") else { panic!() };
        assert_eq!(&inp[..], b"0ls\n");

        let Message::Binary(rsz) = encode_resize(120, 36) else { panic!() };
        assert_eq!(rsz[0], b'1');
        let v: Value = serde_json::from_slice(&rsz[1..]).unwrap();
        assert_eq!((v["columns"].as_u64(), v["rows"].as_u64()), (Some(120), Some(36)));

        let out_msg = Message::Binary(b"0hello".to_vec().into());
        assert_eq!(decode_output_frame(&out_msg), Some(b"hello".as_slice()));
        let title_msg = Message::Binary(b"1bash".to_vec().into());
        assert_eq!(decode_output_frame(&title_msg), None);
    }

    #[test]
    fn percentile_calculation() {
        let s = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&s, 0.50) - 3.0).abs() < 1e-9);
        assert!((percentile(&s, 0.0) - 1.0).abs() < 1e-9);
        assert!((percentile(&s, 1.0) - 5.0).abs() < 1e-9);
        assert!((percentile(&s, 0.95) - 4.8).abs() < 1e-9);
    }
}
