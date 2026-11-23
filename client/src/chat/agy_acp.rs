//! 和 `agy acp`（Antigravity CLI 的 Agent Client Protocol 服务）说话（docs/AGY_UI.md §2）。
//!
//! 一台电脑一个 `agy acp --process-per-session`：看看起的子进程，stdin / stdout 上一行一条 JSON-RPC 2.0。
//! 每个会话在 agy 自己的 worker 进程里跑，几个会话同时跑一轮互不挡。
//! `session/prompt` 一直阻塞到这一轮结束，所以请求要能同时挂着好几个（按 id 对回去）。
//!
//! agy 主动发来的：`session/update` 通知（内容、用量、标题……）、`session/request_permission` 请求（审批）。
//! 别的请求（读写文件、终端）在 `initialize` 里声明了不支持，万一来了回“方法不存在”。
//!
//! 进程跟着客户端走（stdio 断了它就退出），客户端重启后会话要 `session/load` 重新加载。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::error::LocalError;

/// 普通请求（新建、加载、设置）最多等这么久；`session/prompt` 不限时
pub const CALL_WAIT: Duration = Duration::from_secs(90);
/// 一行最长多少字节（再长就是出错了，丢掉这一行）
const LINE_LIMIT: usize = 64 << 20;

/// agy 主动发来的
#[derive(Debug)]
pub enum Incoming {
    /// `session/update`
    Update { session: String, update: Value },
    /// `session/request_permission`：用 [`AcpConn::respond`] 答复
    Permission { id: Value, params: Value },
    /// 进程退出了 / stdout 断了
    Closed,
}

type Waiter = oneshot::Sender<Result<Value, Value>>;

pub struct AcpConn {
    out: mpsc::UnboundedSender<String>,
    waiting: Mutex<HashMap<i64, Waiter>>,
    next: AtomicI64,
    alive: AtomicBool,
    pub pid: Option<u32>,
    /// `initialize` 报的版本
    pub version: Mutex<Option<String>>,
    child: Mutex<Option<tokio::process::Child>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn err(code: &'static str, detail: impl std::fmt::Display) -> LocalError {
    LocalError::new(code).with("detail", detail.to_string().chars().take(500).collect::<String>())
}

/// JSON-RPC 的错误 → 看看的错误（`AGY_REQUEST_FAILED`，带上 agy 的说明）
fn rpc_error(e: &Value) -> LocalError {
    let msg = e["message"].as_str().unwrap_or("request failed");
    let detail = match e["data"].as_str().or(e["data"]["message"].as_str()) {
        Some(d) if !d.is_empty() && d != msg => format!("{msg}: {d}"),
        _ => msg.to_string(),
    };
    err("AGY_REQUEST_FAILED", detail)
}

impl AcpConn {
    /// 起 `agy acp` 并完成 `initialize`
    /// `extra`：附加参数（另外的账号：`--gemini-dir <目录>`）
    pub async fn start(program: &Path, path: Option<&std::ffi::OsStr>, env: &[(String, String)], extra: &[String], run: &Path, tx: mpsc::UnboundedSender<Incoming>) -> Result<Arc<Self>, LocalError> {
        let _ = std::fs::create_dir_all(run);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(run.join("agy-acp.log")).ok();
        let mut cmd = tokio::process::Command::new(program);
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        if let Some(p) = path {
            cmd.env("PATH", p);
        }
        cmd.args(["acp", "--process-per-session"])
            .args(extra)
            .current_dir(run)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null))
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let mut child = cmd.spawn().map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::NotFound { "AGY_NOT_FOUND" } else { "AGY_START" };
            tracing::warn!(program = %program.display(), error = %e, "agy acp 起不来");
            err(code, format!("{}: {e}", program.display()))
        })?;
        let pid = child.id();
        let mut stdin = child.stdin.take().ok_or_else(|| err("AGY_START", "no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| err("AGY_START", "no stdout"))?;
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let me = Arc::new(Self {
            out,
            waiting: Mutex::new(HashMap::new()),
            next: AtomicI64::new(1),
            alive: AtomicBool::new(true),
            pid,
            version: Mutex::new(None),
            child: Mutex::new(Some(child)),
        });
        let reader = me.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match lines.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if buf.len() > LINE_LIMIT {
                    continue;
                }
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
                reader.dispatch(msg, &tx);
            }
            reader.alive.store(false, Ordering::SeqCst);
            for (_, w) in lock(&reader.waiting).drain() {
                let _ = w.send(Err(json!({ "message": "agy acp exited" })));
            }
            let _ = tx.send(Incoming::Closed);
        });
        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
            "clientInfo": { "name": "looklook", "title": "Looklook", "version": env!("CARGO_PKG_VERSION") },
        });
        let r = match me.call_timeout("initialize", init, Duration::from_secs(30)).await {
            Ok(r) => r,
            Err(e) => {
                me.kill();
                let tail = log_tail(run);
                return Err(if tail.is_empty() { e } else { err("AGY_START", tail) });
            }
        };
        *lock(&me.version) = r["agentInfo"]["version"].as_str().map(String::from);
        Ok(me)
    }

    fn dispatch(&self, msg: Value, tx: &mpsc::UnboundedSender<Incoming>) {
        let method = msg["method"].as_str().map(String::from);
        match (msg.get("id").filter(|v| !v.is_null()), method) {
            // 我们请求的答复
            (Some(id), None) => {
                let Some(n) = id.as_i64() else { return };
                if let Some(w) = lock(&self.waiting).remove(&n) {
                    let r = match msg.get("error") {
                        Some(e) if !e.is_null() => Err(e.clone()),
                        _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = w.send(r);
                }
            }
            // agy 发来的请求
            (Some(id), Some(method)) => {
                if method == "session/request_permission" {
                    let _ = tx.send(Incoming::Permission { id: id.clone(), params: msg["params"].clone() });
                } else {
                    self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} is not supported") } }));
                }
            }
            // 通知
            (None, Some(method)) => {
                if method == "session/update" {
                    let p = &msg["params"];
                    if let Some(s) = p["sessionId"].as_str() {
                        let _ = tx.send(Incoming::Update { session: s.to_string(), update: p["update"].clone() });
                    }
                }
            }
            (None, None) => {}
        }
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send_request(&self, method: &str, params: Value) -> Result<(i64, oneshot::Receiver<Result<Value, Value>>), LocalError> {
        if !self.alive() {
            return Err(err("AGY_UNAVAILABLE", "agy acp is not running"));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.waiting).insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        Ok((id, rx))
    }

    fn map_response(r: Result<Result<Value, Value>, oneshot::error::RecvError>) -> Result<Value, LocalError> {
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(rpc_error(&e)),
            Err(_) => Err(err("AGY_UNAVAILABLE", "agy acp exited")),
        }
    }

    /// 发请求，等答复（不限时：`session/prompt` 用）
    pub async fn call_long(&self, method: &str, params: Value) -> Result<Value, LocalError> {
        let (_, rx) = self.send_request(method, params)?;
        Self::map_response(rx.await)
    }

    pub async fn call_timeout(&self, method: &str, params: Value, wait: Duration) -> Result<Value, LocalError> {
        let (id, rx) = self.send_request(method, params)?;
        match tokio::time::timeout(wait, rx).await {
            Ok(r) => Self::map_response(r),
            Err(_) => {
                // 等不到就不等了（只移除这一个请求，不影响并发的其他请求）
                lock(&self.waiting).remove(&id);
                Err(err("AGY_REQUEST_FAILED", format!("{method}: timed out")))
            }
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, LocalError> {
        self.call_timeout(method, params, CALL_WAIT).await
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// 答复 agy 的请求（审批）
    pub fn respond(&self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.start_kill();
        }
    }
}

impl Drop for AcpConn {
    fn drop(&mut self) {
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.start_kill();
        }
    }
}

/// 日志最后几行（起不来时给界面看）
fn log_tail(run: &Path) -> String {
    let text = std::fs::read_to_string(run.join("agy-acp.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("\n").chars().rev().take(800).collect::<String>().chars().rev().collect()
}

/// 用哪个 `agy`：客户端 PATH、用户登录 shell 的 PATH 里找，再看官方安装器放的位置
pub async fn program(shell: &str) -> PathBuf {
    if let Some(p) = crate::tools::detect(shell).await.agy {
        return PathBuf::from(p);
    }
    crate::agents::agy_fallback().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "agy.exe" } else { "agy" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_error_detail() {
        let e = rpc_error(&json!({ "code": -32603, "message": "Internal error", "data": "session not found" }));
        assert_eq!(e.code, "AGY_REQUEST_FAILED");
        assert_eq!(e.params["detail"], "Internal error: session not found");
        let e = rpc_error(&json!({ "code": -32603, "message": "boom" }));
        assert_eq!(e.params["detail"], "boom");
    }

    #[tokio::test]
    async fn call_timeout_does_not_drop_concurrent_waiter() {
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (in_tx, _in_rx) = mpsc::unbounded_channel();
        let conn = Arc::new(AcpConn {
            out: out_tx,
            waiting: Mutex::new(HashMap::new()),
            next: AtomicI64::new(1),
            alive: AtomicBool::new(true),
            pid: None,
            version: Mutex::new(None),
            child: Mutex::new(None),
        });
        // 请求 A（id=1）很快超时；请求 B（id=2）并发等待且 id 更大
        let c1 = conn.clone();
        let req_a = tokio::spawn(async move { c1.call_timeout("slow_op", json!({}), Duration::from_millis(40)).await });
        // 确保 A 先注册 id=1
        tokio::time::sleep(Duration::from_millis(10)).await;
        let c2 = conn.clone();
        let req_b = tokio::spawn(async move { c2.call_long("session/prompt", json!({ "sessionId": "s2" })).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        // A 超时失败
        let err_a = req_a.await.unwrap().unwrap_err();
        assert_eq!(err_a.code, "AGY_REQUEST_FAILED");
        assert!(err_a.params["detail"].as_str().unwrap_or("").contains("timed out"));
        // 只有 id=1 被移除了，id=2 仍然在等答复
        {
            let w = lock(&conn.waiting);
            assert!(!w.contains_key(&1), "超时的请求应被移除");
            assert!(w.contains_key(&2), "并发的另一个请求不能被误删");
        }
        // 分发 id=2 的答复，请求 B 正常完成
        conn.dispatch(json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } }), &in_tx);
        let res_b = req_b.await.unwrap().unwrap();
        assert_eq!(res_b["stopReason"], "end_turn");
    }

    /// 用一个假的 agy（shell 脚本）走一遍：initialize、请求对号、审批请求、通知、退出
    #[cfg(unix)]
    #[tokio::test]
    async fn talks_json_rpc_over_stdio() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("agy");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"x","version":"9.9.9"}}}'
read line
echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"session_info_update","title":"T"}}}'
echo '{"jsonrpc":"2.0","id":"p:1","method":"session/request_permission","params":{"sessionId":"s1","options":[]}}'
echo '{"jsonrpc":"2.0","id":"x:2","method":"fs/read_text_file","params":{}}'
echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}'
read line
echo "$line" > "$(dirname "$0")/answer"
read line
echo "$line" > "$(dirname "$0")/perm"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let conn = AcpConn::start(&fake, None, &[], &[], dir.path(), tx).await.unwrap();
        assert_eq!(lock(&conn.version).as_deref(), Some("9.9.9"));
        let r = conn.call("session/new", json!({ "cwd": "/" })).await.unwrap();
        assert_eq!(r["sessionId"], "s1");
        match rx.recv().await.unwrap() {
            Incoming::Update { session, update } => assert_eq!((session.as_str(), update["title"].as_str()), ("s1", Some("T"))),
            other => panic!("{other:?}"),
        }
        let Incoming::Permission { id, .. } = rx.recv().await.unwrap() else { panic!() };
        assert_eq!(id, json!("p:1"));
        conn.respond(&id, json!({ "outcome": { "outcome": "cancelled" } }));
        for _ in 0..100 {
            if dir.path().join("perm").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let answer: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("answer")).unwrap()).unwrap();
        assert_eq!(answer["id"], "x:2", "不支持的请求回了错误");
        assert_eq!(answer["error"]["code"], -32601);
        let perm: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("perm")).unwrap()).unwrap();
        assert_eq!(perm["id"], "p:1");
        assert!(matches!(rx.recv().await.unwrap(), Incoming::Closed));
        assert!(!conn.alive());
        assert!(conn.call("session/new", json!({})).await.is_err());
    }
}
