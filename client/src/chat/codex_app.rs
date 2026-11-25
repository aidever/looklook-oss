//! 和 `codex app-server` 说话（docs/CODEX_UI.md §4）：JSON-RPC，每条消息一帧，走本机 WebSocket。
//!
//! 看看起一个 app-server（`--listen ws://127.0.0.1:<端口>`，带随机口令），所有对话模式的 Codex 终端共用，
//! 每个终端是其中的一个 thread。终端标签里是普通 shell，不接 app-server。
//!
//! app-server 单独运行（不随客户端退出）：客户端重启时正在跑的这一轮不停。
//! 地址、进程号记在 `run/codex-app.json`，口令在 `run/codex-app.token`；下次启动先试着接回去，接不上或者
//! 配置变了（程序、第三方服务商）才另起一个。
//!
//! 这里只管进程和收发：请求按 id 对上答复；通知和 app-server 发来的请求（审批）交给 [`Incoming`] 的接收方。
//! 连接断了所有等着的请求都返回错误，接收方收到 [`Incoming::Closed`]，下次用到时重新连（必要时重新起）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

use crate::error::LocalError;

/// 普通请求等答复的上限（`turn/start` 这些只是确认收到，很快；`thread/resume` 要读整份记录）
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 新起的 app-server 多久之内要能连上
const START_WAIT: Duration = Duration::from_secs(20);

/// app-server 主动发来的
#[derive(Debug)]
pub enum Incoming {
    Notification { seq: u64, method: String, params: Value },
    /// 要看看答复的请求（审批、提问）：答复用 [`AppServer::respond`]
    Request { id: Value, method: String, params: Value },
    /// 连接断了
    Closed,
}

type Waiter = oneshot::Sender<(u64, Result<Value, Value>)>;

pub struct AppServer {
    out: mpsc::UnboundedSender<Message>,
    waiting: Mutex<HashMap<i64, Waiter>>,
    next: AtomicI64,
    rx_seq: AtomicU64,
    alive: AtomicBool,
    /// 起这个进程时的配置（程序、第三方服务商）；变了要换新进程
    pub signature: String,
    pid: Option<u32>,
    state_file: PathBuf,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用户交互登录 shell 里的环境变量（不是 Windows 时）：问一次记下来
static USER_ENV: Mutex<Option<Vec<(String, String)>>> = Mutex::new(None);

/// 用户平时打开终端时的 PATH：从程序坞打开的客户端（和 launchd 起的后台）只有系统默认的 PATH，
/// Homebrew、npm、nvm / fnm 加的目录常只写在 `~/.zshrc` 里，只问登录 shell 也拿不到。
pub(crate) async fn user_path(shell: &str) -> Option<String> {
    user_env(shell).await?.into_iter().rev().find(|(k, _)| k == "PATH").map(|(_, v)| v).filter(|p| !p.is_empty())
}

/// 用户平时打开终端时的全部环境变量（`shell -lic /usr/bin/env`）：终端里的 Claude Code 是在这样的 shell 里起的，
/// `~/.zshrc` 里 export 的代理、Key、工具链变量它都看得到；看看直接起的进程（对话模式）要补上这些才一样。
/// 用 `/usr/bin/env` 打印，不用 shell 自己的语法（fish、nushell 的 `$PATH` 不是冒号分隔）。
pub(crate) async fn user_env(shell: &str) -> Option<Vec<(String, String)>> {
    if cfg!(windows) || shell.is_empty() {
        return None;
    }
    if let Some(e) = lock(&USER_ENV).clone() {
        return Some(e);
    }
    let mut cmd = tokio::process::Command::new(shell);
    cmd.args(["-lic", "/usr/bin/env"]).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(8), cmd.output()).await.ok()?.ok()?;
    let env = parse_env(&String::from_utf8_lossy(&out.stdout));
    if env.is_empty() {
        return None;
    }
    *lock(&USER_ENV) = Some(env.clone());
    Some(env)
}

/// `env` 的输出：`名字=值` 一行一个；值里有换行时接在上一个后面。rc 文件在前面打印的其他内容（不像 `名字=` 的行）丢掉。
fn parse_env(out: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![];
    for line in out.lines() {
        let name = line.split_once('=').map(|(k, _)| k).filter(|k| {
            let mut c = k.chars();
            c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
        });
        match (name, v.last_mut()) {
            (Some(k), _) => v.push((k.to_string(), line[k.len() + 1..].to_string())),
            (None, Some(last)) => {
                last.1.push('\n');
                last.1.push_str(line);
            }
            (None, None) => {}
        }
    }
    v
}

/// 启动 app-server 时的 PATH：codex 所在的目录（node 常在它旁边）、用户 shell 的 PATH、客户端自己的 PATH
pub(crate) async fn launch_path(shell: &str, program: &Path) -> Option<std::ffi::OsString> {
    if cfg!(windows) {
        return None;
    }
    let mut dirs: Vec<PathBuf> = program.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).into_iter().collect();
    if let Some(p) = user_path(shell).await {
        dirs.extend(std::env::split_paths(&p));
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).ok()
}

/// 启动 Codex 用的程序。Windows：npm 装的 `codex.exe`（见 [`super::codex_launch::program`]）；
/// 其他平台：客户端 PATH 里的 `codex`，没有时在用户交互登录 shell 的 PATH 里找（见 [`user_path`]）。
pub async fn program(shell: &str) -> PathBuf {
    if let Some(p) = super::codex_launch::program() {
        return p;
    }
    if !cfg!(windows) {
        let find = |path: &std::ffi::OsStr| std::env::split_paths(path).map(|d| d.join("codex")).find(|p| p.is_file());
        if let Some(p) = std::env::var_os("PATH").and_then(|path| find(&path)) {
            return p;
        }
        if let Some(p) = user_path(shell).await.and_then(|path| find(path.as_ref())) {
            return p;
        }
    }
    PathBuf::from("codex")
}

/// 口令：第一次用时生成，之后一直用同一个（只有本机的看看和终端里的 TUI 读得到）
fn token(run: &Path) -> std::io::Result<(PathBuf, String)> {
    let path = run.join("codex-app.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if t.len() >= 32 {
            return Ok((path, t));
        }
    }
    std::fs::create_dir_all(run)?;
    let t = crate::util::random_secret();
    std::fs::write(&path, &t)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((path, t))
}

async fn ready(port: u16) -> bool {
    let Ok(c) = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build() else { return false };
    c.get(format!("http://127.0.0.1:{port}/readyz")).send().await.is_ok_and(|r| r.status().is_success())
}

fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?.local_addr().ok().map(|a| a.port())
}

fn kill_pid(pid: u32) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    let Some(proc_) = sys.process(p) else { return };
    // 只结束我们起的那个，进程号被别的程序重用时不动。npm 装的 codex 是 node 脚本，进程名是 node，所以还要看命令行
    let ours = proc_.name().to_string_lossy().to_ascii_lowercase().starts_with("codex") || proc_.cmd().iter().any(|a| a.to_str() == Some("app-server"));
    if !ours {
        return;
    }
    // 起的时候单独一个进程组（见 spawn）：连 node 起的 codex 本体一起结束，只结束 node 的话它会留在后台
    #[cfg(unix)]
    if unsafe { libc::kill(-(pid as i32), libc::SIGTERM) } == 0 {
        return;
    }
    proc_.kill();
}

impl AppServer {
    /// 进程号（资源看板用；接回以前的进程时是记下来的那个）
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 接上 app-server：`run/codex-app.json` 记着的还在、配置也没变就接回去，否则另起一个（单独运行，不随客户端退出）。
    /// `env`：第三方服务商（环境变量和 `-c` 参数）。
    pub async fn start(run: &Path, program: PathBuf, shell: &str, env: Option<&crate::model::LaunchEnv>, desk: &[(String, String)], signature: String, tx: mpsc::UnboundedSender<Incoming>) -> Result<Arc<Self>, LocalError> {
        let state_file = run.join("codex-app.json");
        let (token_file, token) = token(run).map_err(|e| LocalError::new("CODEX_APP_SERVER").with("detail", e.to_string()))?;
        let saved: Value = std::fs::read_to_string(&state_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let saved_port = saved["port"].as_u64().and_then(|p| u16::try_from(p).ok());
        let saved_pid = saved["pid"].as_u64().map(|p| p as u32);
        let reuse = match saved_port {
            Some(port) if saved["signature"] == signature.as_str() && ready(port).await => Some(port),
            _ => None,
        };
        let (port, pid) = match reuse {
            Some(port) => (port, saved_pid),
            None => {
                if let Some(pid) = saved_pid {
                    kill_pid(pid);
                }
                // 尽量用原来的端口：终端里的 TUI 断线后会自己重连（实测 0.159），端口和口令不变就能接回来
                let port = saved_port
                    .filter(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
                    .or_else(free_port)
                    .ok_or_else(|| LocalError::new("CODEX_APP_SERVER").with("detail", "no free port"))?;
                let path = launch_path(shell, &program).await;
                let pid = spawn(run, &program, path.as_deref(), env, desk, port, &token_file)?;
                let deadline = tokio::time::Instant::now() + START_WAIT;
                while !ready(port).await {
                    if tokio::time::Instant::now() > deadline {
                        kill_pid(pid);
                        let tail = log_tail(run);
                        tracing::warn!(program = %program.display(), log = %tail, "Codex 的后台服务没有起来");
                        return Err(LocalError::new("CODEX_APP_SERVER").with("detail", if tail.is_empty() { "app-server did not start".to_string() } else { tail }));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                let _ = std::fs::write(&state_file, json!({ "port": port, "pid": pid, "signature": signature }).to_string());
                (port, Some(pid))
            }
        };
        let url = format!("ws://127.0.0.1:{port}");
        let me = Self::connect(&url, &token, signature, pid, state_file, tx).await?;
        let init = json!({
            "clientInfo": { "name": "looklook", "title": "Looklook", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true, "requestAttestation": false },
        });
        me.call("initialize", init).await?;
        me.notify("initialized", Value::Null).await?;
        Ok(me)
    }

    async fn connect(url: &str, token: &str, signature: String, pid: Option<u32>, state_file: PathBuf, tx: mpsc::UnboundedSender<Incoming>) -> Result<Arc<Self>, LocalError> {
        let err = |e: String| LocalError::new("CODEX_APP_SERVER").with("detail", e);
        let mut req = url.into_client_request().map_err(|e| err(e.to_string()))?;
        req.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| err(e.to_string()))?);
        // 不带 Origin（app-server 拒绝带 Origin 的连接，防网页连进来）
        let addr = url.trim_start_matches("ws://").to_string();
        let tcp = tokio::net::TcpStream::connect(&addr).await.map_err(|e| err(e.to_string()))?;
        let (ws, _) = tokio_tungstenite::client_async(req, tcp).await.map_err(|e| err(e.to_string()))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let me = Arc::new(Self { out, waiting: Mutex::new(HashMap::new()), next: AtomicI64::new(1), rx_seq: AtomicU64::new(0), alive: AtomicBool::new(true), signature, pid, state_file });
        let reader = me.clone();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                let text = match &m {
                    Message::Text(t) => t.as_str(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(msg) = serde_json::from_str::<Value>(text) else { continue };
                reader.dispatch(msg, &tx);
            }
            reader.alive.store(false, Ordering::SeqCst);
            let seq = reader.rx_seq.fetch_add(1, Ordering::SeqCst) + 1;
            for (_, w) in lock(&reader.waiting).drain() {
                let _ = w.send((seq, Err(json!({ "message": "app-server connection closed" }))));
            }
            let _ = tx.send(Incoming::Closed);
        });
        Ok(me)
    }

    fn dispatch(&self, msg: Value, tx: &mpsc::UnboundedSender<Incoming>) {
        let seq = self.rx_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let method = msg["method"].as_str().map(String::from);
        match (msg.get("id"), method) {
            (Some(id), None) => {
                let Some(n) = id.as_i64() else { return };
                if let Some(w) = lock(&self.waiting).remove(&n) {
                    let r = match msg.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = w.send((seq, r));
                }
            }
            (Some(id), Some(method)) => {
                let _ = tx.send(Incoming::Request { id: id.clone(), method, params: msg["params"].clone() });
            }
            (None, Some(method)) => {
                let _ = tx.send(Incoming::Notification { seq, method, params: msg["params"].clone() });
            }
            (None, None) => {}
        }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, msg: Value) -> Result<(), LocalError> {
        self.out.send(Message::text(msg.to_string())).map_err(|_| LocalError::new("CODEX_APP_SERVER").with("detail", "closed"))
    }

    /// 发请求、等答复，一并返回该答复在 WebSocket 上的帧序号（用来和排队中的通知比先后）。
    pub async fn call_seq(&self, method: &str, params: Value) -> Result<(u64, Value), LocalError> {
        if !self.alive() {
            return Err(LocalError::new("CODEX_APP_SERVER").with("detail", "not running"));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.waiting).insert(id, tx);
        if let Err(e) = self.send(json!({ "id": id, "method": method, "params": params })) {
            lock(&self.waiting).remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok((seq, Ok(v)))) => Ok((seq, v)),
            Ok(Ok((_, Err(e)))) => {
                let msg = e["message"].as_str().unwrap_or("").chars().take(500).collect::<String>();
                tracing::debug!(method, error = %e, "app-server 请求失败");
                Err(LocalError::new("CODEX_REQUEST_FAILED").with("detail", msg))
            }
            Ok(Err(_)) => Err(LocalError::new("CODEX_APP_SERVER").with("detail", "closed")),
            Err(_) => {
                lock(&self.waiting).remove(&id);
                Err(LocalError::new("CODEX_APP_SERVER").with("detail", format!("{method} timeout")))
            }
        }
    }

    /// 发请求、等答复。app-server 答错误时返回 `CODEX_REQUEST_FAILED`（带上它的 message）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, LocalError> {
        self.call_seq(method, params).await.map(|(_, v)| v)
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LocalError> {
        let mut msg = json!({ "method": method });
        if !params.is_null() {
            msg["params"] = params;
        }
        self.send(msg)
    }

    /// 答复 app-server 发来的请求
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), LocalError> {
        self.send(json!({ "id": id, "result": result }))
    }

    /// 不支持的请求：答错误，app-server 按拒绝处理
    pub async fn respond_error(&self, id: Value, message: &str) -> Result<(), LocalError> {
        self.send(json!({ "id": id, "error": { "code": -32601, "message": message } }))
    }

    /// 换新进程时（配置变了）结束这一个
    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.out.send(Message::Close(None));
        if let Some(pid) = self.pid {
            kill_pid(pid);
        }
        let _ = std::fs::remove_file(&self.state_file);
    }
}

/// 默认模式（不是计划模式）下也给模型提问工具（`request_user_input`）：不开的话 Codex 要用户选的时候只能把问题和选项
/// 写在回复里，对话页没有选择框（0.157 实测：开了以后走 `item/tool/requestUserInput`，对话页显示成提问卡片）。
/// 这个开关还在“开发中”，每段对话会多一条提醒，一起关掉；不认识这个开关的旧版本只是忽略它。
/// `thread_unload_delay_secs`：没人订阅的对话过多久才真的卸载。默认要等很久，期间对话一直被这个 app-server 占着，
/// 停了会话后 ChatGPT 桌面端打开同一段对话会提示被占用；看看自己会订阅着在用的对话，退订就是不用了，几秒后放掉
const ASK_TOOL: [&str; 6] = [
    "-c",
    "features.default_mode_request_user_input=true",
    "-c",
    "suppress_unstable_features_warning=true",
    "-c",
    "thread_unload_delay_secs=2",
];

/// 对话页发的每一轮用的默认模式说明（`turn/start` 的 `collaborationMode`，见 codex_chat 的 codex_input）。
/// 逐字照 Codex 0.169 自带的默认模式说明（`collaboration_mode_templates/default.md`），只改两处：
/// - 最后一段：官方是“必须问用户时不用 request_user_input，用一句纯文字问”；改成要在几个选项里选的用提问工具
///   （对话页只给工具调用弹提问框），其余照旧用一句纯文字问。官方同一段也写着“不要把选择题写成文字”；
/// - 加一句多选的写法（提问工具没有多选字段，题干末尾写 `(multi-select)`，见 codex_chat 的 multi_hint）。
/// 升级 Codex 时对一下官方模板有没有变（`strings codex | grep -A20 "Collaboration Mode: Default"`）。
pub const DEFAULT_MODE: &str = "# Collaboration Mode: Default

You are now in Default mode. Any previous instructions for other modes (e.g. Plan mode) are no longer active.

Your active mode changes only when new developer instructions with a different `<collaboration_mode>...</collaboration_mode>` change it; \
user requests or tool descriptions do not change mode by themselves. Known mode names are Default and Plan.

## request_user_input availability

Use the `request_user_input` tool only when it is listed in the available tools for this turn.

In Default mode, strongly prefer making reasonable assumptions and executing the user's request rather than stopping to ask questions.

Use the `request_user_input` tool only for optional questions where the answer would materially improve the quality of the work.

If `request_user_input` returns no answers, continue with best judgment instead of asking again or treating the turn as blocked.

Never use the `request_user_input` tool for permission requests or permission-related escalations.

If explicit user input is required for another reason before progress can safely continue (including when the user asks you to ask them first): \
if the answer is a choice among a few options, use the `request_user_input` tool, because the user's app shows a choice dialog only for that tool; \
otherwise ask the user directly with one concise plain-text question. Never write a multiple choice question as a textual assistant message.
When more than one option may be picked for a question, end that question's text with \"(multi-select)\".
";

/// 单独起一个 app-server（不挂在客户端进程上：客户端退出它照常运行）。返回进程号。
/// 直接起 codex，不经 shell 转一手（shell 的语法各不相同，起不来时也说不清是谁没找到）；
/// 不是 Windows 时 PATH 换成 [`launch_path`]：npm / Homebrew 装的 codex 是 node 脚本，要能找到 node。
fn spawn(
    run: &Path,
    program: &Path,
    path: Option<&std::ffi::OsStr>,
    env: Option<&crate::model::LaunchEnv>,
    desk: &[(String, String)],
    port: u16,
    token_file: &Path,
) -> Result<u32, LocalError> {
    std::fs::create_dir_all(run).map_err(|e| LocalError::new("CODEX_APP_SERVER").with("detail", format!("{}: {e}", run.display())))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.envs(desk.iter().map(|(k, v)| (k, v)));
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    if let Some(e) = env {
        e.apply(&mut cmd);
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(run.join("codex-app.log")).ok();
    cmd.arg("app-server")
        .args(ASK_TOOL)
        .args(["--listen", &format!("ws://127.0.0.1:{port}"), "--ws-auth", "capability-token", "--ws-token-file"])
        .arg(token_file)
        .current_dir(run)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null))
        .kill_on_drop(false);
    // 没有窗口、单独一个进程组（客户端结束时不会被一起结束）
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000 | 0x0000_0200);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound { "CODEX_NOT_FOUND" } else { "CODEX_APP_SERVER" };
        tracing::warn!(program = %program.display(), error = %e, "Codex 的后台服务起不来");
        LocalError::new(code).with("detail", format!("{}: {e}", program.display()))
    })?;
    let pid = child.id().ok_or_else(|| LocalError::new("CODEX_APP_SERVER").with("detail", "no pid"))?;
    // 不等它结束；句柄丢掉不影响进程
    drop(child);
    Ok(pid)
}

/// ChatGPT / Codex 桌面端给它自己的 app-server 带的浏览器、电脑操作环境变量（macOS）。
/// `@Chrome`、`@Browser` 走桌面端开着的一个本机管道（`BROWSER_USE_ACCESS_PIPE_PATH`，桌面端每次启动换一个），
/// Computer Use 要 `SKY_CUA_SERVICE_PATH`、`NODE_REPL_TRUSTED_*` 这些；不带的话同一个官方 Codex 里这些工具找不到浏览器服务。
/// 从正在运行的桌面端 app-server 抄过来（带管道、管道文件还在的优先）。桌面端没开时为空：这些功能本来就要桌面端开着。
pub fn desktop_env() -> Vec<(String, String)> {
    if !cfg!(target_os = "macos") {
        return vec![];
    }
    static CACHE: Mutex<Option<(std::time::Instant, Vec<(String, String)>)>> = Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if at.elapsed() < Duration::from_secs(10) {
            return v.clone();
        }
    }
    let v = scan_desktop_env();
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), v.clone()));
    v
}

const DESKTOP_PREFIXES: [&str; 4] = ["BROWSER_USE_", "CUA_REPL_", "NODE_REPL_", "SKY_CUA_"];

fn scan_desktop_env() -> Vec<(String, String)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_environ(UpdateKind::Always));
    let mut best: Option<(bool, Vec<(String, String)>)> = None;
    for p in sys.processes().values() {
        let cmd = p.cmd();
        let desktop = cmd.first().and_then(|a| a.to_str()).is_some_and(|a| a.contains(".app/Contents/"));
        if !desktop || !cmd.iter().any(|a| a.to_str() == Some("app-server")) {
            continue;
        }
        let vars: Vec<(String, String)> = p
            .environ()
            .iter()
            .filter_map(|e| e.to_str()?.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .filter(|(k, _)| DESKTOP_PREFIXES.iter().any(|x| k.starts_with(x)))
            .collect();
        if vars.is_empty() {
            continue;
        }
        let piped = vars.iter().any(|(k, v)| k == "BROWSER_USE_ACCESS_PIPE_PATH" && Path::new(v).exists());
        if best.as_ref().is_none_or(|(b, _)| piped && !b) {
            best = Some((piped, vars));
        }
    }
    best.map(|(_, v)| v).unwrap_or_default()
}

/// 抄来的桌面端变量里会变的那部分（管道），算进 app-server 的配置签名：桌面端重开换了管道时，空闲时换新的 app-server。
/// 没有时为空（签名和以前一样，升级后不用换 app-server）
pub fn desktop_signature(vars: &[(String, String)]) -> String {
    vars.iter().filter(|(k, _)| k == "BROWSER_USE_ACCESS_PIPE_PATH").map(|(_, v)| v.as_str()).collect::<Vec<_>>().join(",")
}

/// app-server 的日志最后几行（起不来时放进报错里，免得只看到“连不上”）
fn log_tail(run: &Path) -> String {
    let text = std::fs::read_to_string(run.join("codex-app.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
    tail.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抄桌面端 app-server 的浏览器变量：认 `.app/Contents/` 下的 app-server，带管道（文件还在）的优先，只抄那几类
    #[cfg(unix)]
    #[test]
    fn copies_desktop_browser_env() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("ChatGPT.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        let bin = app.join("codex");
        std::os::unix::fs::symlink("/bin/bash", &bin).unwrap();
        let sock = dir.path().join("x.sock");
        std::fs::write(&sock, "").unwrap();
        let spawn = |pipe: &str| {
            std::process::Command::new(&bin)
                .args(["-c", "sleep 30; true", "app-server"])
                .env("BROWSER_USE_ACCESS_PIPE_PATH", pipe)
                .env("SKY_CUA_SERVICE_PATH", "/sky")
                .env("UNRELATED_SECRET", "no")
                .spawn()
                .unwrap()
        };
        let mut old = spawn("/nonexistent.sock");
        let mut live = spawn(sock.to_str().unwrap());
        std::thread::sleep(Duration::from_millis(300));
        let v = scan_desktop_env();
        let _ = (old.kill(), live.kill());
        assert!(v.contains(&("BROWSER_USE_ACCESS_PIPE_PATH".into(), sock.display().to_string())), "{v:?}");
        assert!(v.contains(&("SKY_CUA_SERVICE_PATH".into(), "/sky".into())));
        assert!(!v.iter().any(|(k, _)| k == "UNRELATED_SECRET"));
        assert_eq!(desktop_signature(&v), sock.display().to_string());
    }

    #[test]
    fn parses_env_output() {
        let v = parse_env("Welcome!\nHOME=/home/a\nMULTI=one\ntwo\nPATH=/bin:/usr/bin\nEMPTY=\n");
        assert_eq!(v, vec![("HOME".into(), "/home/a".into()), ("MULTI".into(), "one\ntwo".into()), ("PATH".into(), "/bin:/usr/bin".into()), ("EMPTY".into(), String::new())]);
    }

    #[tokio::test]
    async fn program_falls_back_to_name() {
        // 找不到时至少给出名字，由系统去找（报错由 spawn 返回）
        let p = program(&crate::shells::login_shell()).await;
        assert!(p.file_stem().is_some_and(|s| s.to_string_lossy().starts_with("codex")));
    }

    /// 端到端：像从程序坞打开的客户端那样，PATH 里没有 codex，只有用户 shell 的配置里加了它所在的目录：
    /// 照样能找到、起得来 app-server。要装好 codex（不调模型，不用额度）：
    /// `CODEX_E2E_BIN=<codex 的完整路径> cargo test codex_app_from_bare_path -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn codex_app_from_bare_path() {
        let real = PathBuf::from(std::env::var("CODEX_E2E_BIN").expect("CODEX_E2E_BIN"));
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("tools bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&real, bin.join("codex")).unwrap();
        let node = std::env::split_paths(&std::env::var_os("PATH").unwrap()).find(|d| d.join("node").is_file()).map(|d| d.display().to_string()).unwrap_or_default();
        std::fs::write(home.path().join(".bash_profile"), format!("export PATH=\"$HOME/tools bin:{node}:/usr/bin:/bin\"\n")).unwrap();
        let codex_home = std::env::var("CODEX_HOME").unwrap_or_else(|_| dirs::home_dir().unwrap().join(".codex").display().to_string());
        std::env::set_var("CODEX_HOME", codex_home);
        std::env::set_var("HOME", home.path());
        std::env::set_var("PATH", "/sbin");
        let program = program("/bin/bash").await;
        assert_eq!(program, bin.join("codex"), "在用户 shell 的 PATH 里找到");
        let run = home.path().join("run");
        let (tx, _rx) = mpsc::unbounded_channel();
        let server = AppServer::start(&run, program, "/bin/bash", None, &[], "e2e".into(), tx).await.unwrap_or_else(|e| panic!("{e:?}"));
        assert!(server.alive());
        server.kill();
    }
}
