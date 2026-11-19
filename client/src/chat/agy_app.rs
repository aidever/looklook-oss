//! 和 `agy acp` 说话：Agent Client Protocol (ACP) 双向 JSON-RPC，走子进程 stdin / stdout（每行一条 JSON）。
//!
//! 所有对话模式的 Antigravity（agy）会话共用一个 `agy acp --process-per-session` 进程：
//! - 看看发：`initialize`、`session/new`、`session/load`、`session/prompt`、`session/set_mode`、
//!   `session/set_config_option`、`session/set_model`、`session/close`，以及取消通知 `session/cancel`；
//! - agy 发：`session/update` 通知（流式文字、思考、工具调用与结果、可用命令、标题、上下文用量、配置变更），
//!   以及要看看答复的请求（`session/request_permission`），用 [`AgyApp::respond`] 答。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::error::LocalError;

/// 控制请求等答复的上限（`initialize`、`session/new`、`session/load`）
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 进程出错时最多留这么多行 stderr
const STDERR_KEEP: usize = 40;

/// agy acp 主动发来的消息
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    /// 要看看答复的请求（如 `session/request_permission`）：答复用 [`AgyApp::respond`]
    Request { id: Value, method: String, params: Value },
    /// 进程退出或管道断开
    Closed { stderr: String },
}

type Waiter = oneshot::Sender<Result<Value, Value>>;

pub struct AgyApp {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    waiting: Mutex<HashMap<i64, Waiter>>,
    next: AtomicI64,
    alive: AtomicBool,
    killed: AtomicBool,
    pid: Option<u32>,
    version: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 找 `agy` 可执行文件：客户端 PATH → 用户交互登录 shell 的 PATH → `~/.local/bin/agy` 等固定位置
pub async fn program(shell: &str) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["agy.exe", "agy.cmd", "agy.bat"] } else { &["agy"] };
    let find = |path: &std::ffi::OsStr| {
        std::env::split_paths(path)
            .flat_map(|d| names.iter().map(move |n| d.join(n)))
            .find(|p| p.is_file())
    };
    if let Some(p) = std::env::var_os("PATH").and_then(|path| find(&path)) {
        return Some(p);
    }
    if !cfg!(windows) {
        if let Some(p) = super::codex_app::user_path(shell).await.and_then(|path| find(path.as_ref())) {
            return Some(p);
        }
    }
    crate::agents::agy_fallback().map(PathBuf::from)
}

impl AgyApp {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn version(&self) -> Option<String> {
        lock(&self.version).clone()
    }

    /// 起一个 `agy acp --process-per-session` 并完成 `initialize` 握手
    pub async fn start(shell: &str, tx: mpsc::UnboundedSender<Incoming>) -> Result<Arc<Self>, LocalError> {
        let Some(prog) = program(shell).await else {
            return Err(LocalError::new("AGY_NOT_FOUND"));
        };
        let mut cmd = tokio::process::Command::new(&prog);
        cmd.args(["acp", "--process-per-session"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = super::codex_app::user_env(shell).await {
            for (k, v) in env {
                if !matches!(k.as_str(), "PATH" | "PWD" | "SHLVL" | "_") {
                    cmd.env(k, v);
                }
            }
        }
        if let Some(p) = super::codex_app::launch_path(shell, &prog).await {
            cmd.env("PATH", p);
        }
        crate::instances::apply_env(&mut cmd);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let mut child = cmd.spawn().map_err(|e| LocalError::new("AGY_START_FAILED").with("detail", format!("{}: {e}", prog.display())))?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let me = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            waiting: Mutex::new(HashMap::new()),
            next: AtomicI64::new(1),
            alive: AtomicBool::new(true),
            killed: AtomicBool::new(false),
            pid,
            version: Mutex::new(None),
        });

        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!(target: "agy_acp", "stderr: {l}");
                    let mut t = lock(&tail);
                    t.push_back(l);
                    while t.len() > STDERR_KEEP {
                        t.pop_front();
                    }
                }
            });
        }

        let weak = Arc::downgrade(&me);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                            tracing::debug!(target: "agy_acp", "忽略非 JSON 行: {}", trimmed.chars().take(200).collect::<String>());
                            continue;
                        };
                        let has_method = v["method"].is_string();
                        let has_id = !v["id"].is_null();
                        if has_method && has_id {
                            let _ = tx.send(Incoming::Request {
                                id: v["id"].clone(),
                                method: v["method"].as_str().unwrap_or_default().to_string(),
                                params: v["params"].clone(),
                            });
                        } else if has_method {
                            let _ = tx.send(Incoming::Notification {
                                method: v["method"].as_str().unwrap_or_default().to_string(),
                                params: v["params"].clone(),
                            });
                        } else if let Some(id) = v["id"].as_i64() {
                            if let Some(me) = weak.upgrade() {
                                if let Some(w) = lock(&me.waiting).remove(&id) {
                                    let r = if !v["error"].is_null() { Err(v["error"].clone()) } else { Ok(v["result"].clone()) };
                                    let _ = w.send(r);
                                }
                            }
                        }
                    }
                }
            }
            let _ = child.wait().await;
            let err_text = lock(&tail).iter().cloned().collect::<Vec<_>>().join("\n");
            if let Some(me) = weak.upgrade() {
                me.alive.store(false, Ordering::Relaxed);
                let pending: Vec<Waiter> = lock(&me.waiting).drain().map(|(_, w)| w).collect();
                for w in pending {
                    let _ = w.send(Err(json!({ "message": if err_text.is_empty() { "agy acp exited" } else { &err_text } })));
                }
            }
            let _ = tx.send(Incoming::Closed { stderr: err_text });
        });

        let init = me
            .call(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": { "name": "looklook", "version": env!("CARGO_PKG_VERSION") },
                }),
                CALL_TIMEOUT,
            )
            .await?;
        if let Some(v) = init["agentInfo"]["version"].as_str() {
            *lock(&me.version) = Some(v.to_string());
        }
        tracing::info!(pid = ?me.pid, version = ?me.version(), "已连上 agy acp");
        Ok(me)
    }

    async fn write_json(&self, v: &Value) -> Result<(), LocalError> {
        if !self.alive() {
            return Err(LocalError::new("AGY_UNAVAILABLE"));
        }
        let mut line = serde_json::to_string(v).map_err(|e| LocalError::new("AGY_REQUEST_FAILED").with("detail", e.to_string()))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| LocalError::new("AGY_UNAVAILABLE").with("detail", e.to_string()))?;
        stdin.flush().await.map_err(|e| LocalError::new("AGY_UNAVAILABLE").with("detail", e.to_string()))?;
        Ok(())
    }

    /// 发一个 JSON-RPC 请求并等答复；`timeout` 为 None 时不限时长（用于 `session/prompt`）
    pub async fn call(&self, method: &str, params: Value, timeout: impl Into<Option<Duration>>) -> Result<Value, LocalError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.waiting).insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write_json(&msg).await {
            lock(&self.waiting).remove(&id);
            return Err(e);
        }
        let res = match timeout.into() {
            Some(t) => match tokio::time::timeout(t, rx).await {
                Ok(r) => r,
                Err(_) => {
                    lock(&self.waiting).remove(&id);
                    return Err(LocalError::new("AGY_REQUEST_FAILED").with("detail", format!("{method}: timed out")));
                }
            },
            None => rx.await,
        };
        match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                let detail = e["message"].as_str().map(String::from).unwrap_or_else(|| e.to_string());
                Err(LocalError::new("AGY_REQUEST_FAILED").with("detail", detail.chars().take(400).collect::<String>()))
            }
            Err(_) => Err(LocalError::new("AGY_UNAVAILABLE")),
        }
    }

    /// 发一条不需要答复的 JSON-RPC 通知（例如 `session/cancel`）
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LocalError> {
        self.write_json(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// 答复 agy 发来的请求（例如 `session/request_permission`）
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), LocalError> {
        self.write_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).await
    }

    /// 结束进程
    pub fn kill(&self) {
        self.killed.store(true, Ordering::Relaxed);
        self.alive.store(false, Ordering::Relaxed);
        if let Some(pid) = self.pid {
            #[cfg(unix)]
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
            }
            #[cfg(not(unix))]
            {
                use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
                let mut sys = System::new();
                let p = Pid::from_u32(pid);
                sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
                if let Some(proc_) = sys.process(p) {
                    proc_.kill();
                }
            }
        }
    }
}

impl Drop for AgyApp {
    fn drop(&mut self) {
        if !self.killed.load(Ordering::Relaxed) {
            self.kill();
        }
    }
}
