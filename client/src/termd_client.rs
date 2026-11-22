//! `looklook-termd` 本机 IPC 客户端与守护进程生命周期管理。
//!
//! 职责：
//! - 通过 Unix domain socket（或 Windows Named Pipe）与用户级 `looklook-termd` 通信；
//! - 查询/列表/进程探测等非交互调用走“连得上才问”，无活跃会话时不拉起空进程；
//! - 创建会话或浏览器接入前按需拉起守护进程（`--home` / `--socket` / `--parent-pid`）；
//! - 启动或升级后发现旧版 `termd` 仍在运行时，在 Unix 上触发 `looklook-termd --takeover`
//!   通过 `SCM_RIGHTS` 无缝接管既有 PTY 与状态，Windows 按第 0 步结论仅在 0 会话时重启。

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use term_proto::{
    CaptureMode, ControlRequest, ControlResponse, CreateSessionReq, HistorySlice, LegacyDownMsg,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, PaneInfo, SessionInfo, TERMD_CONTROL_VERSION,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use crate::error::LocalError;
use crate::paths::Paths;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// 双工本地 IPC 连接抽象（UnixSocket / Windows NamedPipe）。
pub trait AsyncIpc: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIpc for T {}
pub type BoxedIpc = Box<dyn AsyncIpc>;

/// `Ping` 响应快照，供版本协商、升级交接与监控看板使用。
#[derive(Debug, Clone)]
pub struct PongInfo {
    pub control_version: u32,
    pub protocol_version: String,
    pub binary_version: String,
    pub pid: u32,
    pub session_count: usize,
    pub handover_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinStamp {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    len: u64,
}

/// 面向单份 `Paths`（数据目录）的 `looklook-termd` 客户端。
pub struct TermdClient {
    paths: Paths,
    endpoint: String,
    bin: Option<PathBuf>,
    spawn_lock: tokio::sync::Mutex<()>,
    disk_version: OnceLock<Option<String>>,
    upgraded_pids: Mutex<HashSet<u32>>,
}

impl TermdClient {
    pub fn new(paths: Paths) -> Self {
        let endpoint = paths.termd_socket();
        let bin = paths.termd.clone();
        Self {
            paths,
            endpoint,
            bin,
            spawn_lock: tokio::sync::Mutex::new(()),
            disk_version: OnceLock::new(),
            upgraded_pids: Mutex::new(HashSet::new()),
        }
    }

    pub fn available(&self) -> bool {
        self.bin.is_some()
    }

    /// 探测当前运行中的 `termd`（不会拉起新进程）。
    pub async fn ping_if_running(&self) -> Option<PongInfo> {
        let mut io = connect_endpoint(&self.endpoint).await.ok()?;
        ping_on(&mut *io).await.ok()
    }

    /// 确保本机存在匹配版本的 `termd` 在运行；必要时拉起或执行 Unix 无缝接管。
    pub async fn ensure_ready(&self) -> Result<PongInfo, LocalError> {
        if let Ok(mut io) = connect_endpoint(&self.endpoint).await {
            if let Ok(pong) = ping_on(&mut *io).await {
                drop(io);
                if !self.needs_upgrade(&pong) {
                    return Ok(pong);
                }
                if let Some(upgraded) = self.try_upgrade_locked(&pong).await? {
                    return Ok(upgraded);
                }
                return Ok(pong);
            }
        }

        let _g = self.spawn_lock.lock().await;
        if let Ok(mut io) = connect_endpoint(&self.endpoint).await {
            if let Ok(pong) = ping_on(&mut *io).await {
                drop(io);
                if !self.needs_upgrade(&pong) {
                    return Ok(pong);
                }
                return self.upgrade_while_locked(&pong).await.map(|o| o.unwrap_or(pong));
            }
        }

        let bin = self.bin.clone().ok_or_else(|| LocalError::new("TERMD_MISSING"))?;
        self.spawn_and_wait(&bin, false, None).await
    }

    /// 使用指定的 `new_bin` 对正在运行的 `termd` 发起升级接管（Unix 通过 `SCM_RIGHTS` 无缝交接，
    /// 即使会话数 > 0 也不中断；Windows 在 0 会话时平滑重启，有活跃会话时返回 Ok(false) 由调用方提示）。
    pub async fn takeover_with_bin(&self, new_bin: &Path) -> Result<bool, LocalError> {
        let _g = self.spawn_lock.lock().await;
        let Some(pong) = self.ping_if_running().await else {
            return Ok(false);
        };
        if pong.handover_supported {
            let new_pong = self.spawn_and_wait(new_bin, true, Some(pong.pid)).await?;
            tracing::info!(old_pid = pong.pid, new_pid = new_pong.pid, sessions = new_pong.session_count, "termd 升级交接完成");
            return Ok(true);
        }
        if pong.session_count == 0 {
            if let Ok(mut io) = connect_endpoint(&self.endpoint).await {
                let _ = rpc_on(&mut *io, &ControlRequest::ShutdownIfIdle).await;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
            let _ = self.spawn_and_wait(new_bin, false, Some(pong.pid)).await?;
            return Ok(true);
        }
        tracing::warn!(pid = pong.pid, sessions = pong.session_count, "当前平台不支持在线交接且仍有活跃会话，继续沿用旧版 termd 直至所有会话关闭");
        Ok(false)
    }

    /// 检查已运行的 `termd` 是否需要切换到当前版本的二进制（客户端升级重启后由 `recover` 调用）。
    pub async fn upgrade_if_running(&self) {
        let Some(bin) = self.bin.clone() else { return };
        let Some(pong) = self.ping_if_running().await else { return };
        if !self.needs_upgrade(&pong) {
            return;
        }
        if !self.mark_upgrade_attempt(pong.pid) {
            return;
        }
        if let Err(e) = self.takeover_with_bin(&bin).await {
            tracing::warn!(error = ?e, "启动检查 termd 升级交接失败，继续沿用当前进程");
        }
    }

    async fn try_upgrade_locked(&self, initial: &PongInfo) -> Result<Option<PongInfo>, LocalError> {
        let _g = self.spawn_lock.lock().await;
        let current = self.ping_if_running().await.unwrap_or_else(|| initial.clone());
        if !self.needs_upgrade(&current) {
            return Ok(Some(current));
        }
        self.upgrade_while_locked(&current).await
    }

    async fn upgrade_while_locked(&self, pong: &PongInfo) -> Result<Option<PongInfo>, LocalError> {
        let Some(bin) = self.bin.clone() else {
            return Ok(None);
        };
        if !self.mark_upgrade_attempt(pong.pid) {
            return Ok(None);
        }
        if pong.handover_supported {
            match self.spawn_and_wait(&bin, true, Some(pong.pid)).await {
                Ok(new_pong) => {
                    tracing::info!(old_pid = pong.pid, new_pid = new_pong.pid, "检测到新版 looklook-termd，已完成会话交接");
                    return Ok(Some(new_pong));
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "termd 升级交接失败，继续沿用现有守护进程");
                    return Ok(None);
                }
            }
        }
        if pong.session_count == 0 {
            if let Ok(mut io) = connect_endpoint(&self.endpoint).await {
                if matches!(rpc_on(&mut *io, &ControlRequest::ShutdownIfIdle).await, Ok(ControlResponse::Ok { changed: true })) {
                    drop(io);
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    if let Ok(new_pong) = self.spawn_and_wait(&bin, false, Some(pong.pid)).await {
                        return Ok(Some(new_pong));
                    }
                }
            }
        }
        Ok(None)
    }

    fn mark_upgrade_attempt(&self, pid: u32) -> bool {
        let mut g = self.upgraded_pids.lock().unwrap_or_else(|e| e.into_inner());
        if g.contains(&pid) {
            return false;
        }
        g.insert(pid);
        true
    }

    fn needs_upgrade(&self, pong: &PongInfo) -> bool {
        if pong.control_version != TERMD_CONTROL_VERSION || pong.protocol_version != PROTOCOL_VERSION {
            return true;
        }
        if self.bin.is_some() {
            if let Some(expected_ver) = self.expected_binary_version() {
                if expected_ver != pong.binary_version {
                    return true;
                }
            }
            if running_binary_replaced(pong.pid, self.bin.as_deref()) {
                return true;
            }
        }
        false
    }

    fn expected_binary_version(&self) -> Option<&str> {
        self.disk_version
            .get_or_init(|| {
                let bin = self.bin.as_ref()?;
                let mut cmd = std::process::Command::new(bin);
                cmd.arg("--version").stdin(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(CREATE_NO_WINDOW);
                }
                let out = cmd.output().ok()?;
                if !out.status.success() {
                    return None;
                }
                let s = String::from_utf8_lossy(&out.stdout);
                s.split_whitespace().last().map(str::to_string)
            })
            .as_deref()
    }

    async fn spawn_and_wait(
        &self,
        bin: &Path,
        takeover: bool,
        old_pid: Option<u32>,
    ) -> Result<PongInfo, LocalError> {
        let log_path = self.paths.home.join("logs").join("termd.log");
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        #[cfg(unix)]
        if let Some(sock_dir) = Path::new(&self.endpoint).parent() {
            let _ = std::fs::create_dir_all(sock_dir);
        }
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 2_000_000) {
            let _ = std::fs::remove_file(&log_path);
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        let log_err = log
            .try_clone()
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;

        let mut cmd = Command::new(bin);
        cmd.arg("--home")
            .arg(&self.paths.home)
            .arg("--socket")
            .arg(&self.endpoint)
            .arg("--parent-pid")
            .arg(std::process::id().to_string());
        if takeover {
            cmd.arg("--takeover");
        }
        cmd.stdin(Stdio::null()).stdout(log).stderr(log_err);
        #[cfg(unix)]
        unsafe {
            // 新会话独立进程组，避免 `looklook` 重启/Ctrl+C 信号误杀运行中的持久会话守护进程。
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;

        for _ in 0..100 {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(LocalError::new("START_FAILED").with(
                    "detail",
                    format!("looklook-termd exited early ({status}): {}", tail_log(&log_path)),
                ));
            }
            if let Some(pong) = self.ping_if_running().await {
                if old_pid.is_none_or(|old| pong.pid != old) {
                    return Ok(pong);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(LocalError::new("START_FAILED").with("detail", "looklook-termd startup timeout"))
    }

    async fn rpc_ready(&self, req: &ControlRequest) -> Result<ControlResponse, LocalError> {
        let _ = self.ensure_ready().await?;
        let mut io = connect_endpoint(&self.endpoint)
            .await
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        rpc_on(&mut *io, req).await
    }

    async fn rpc_if_running(&self, req: &ControlRequest) -> Option<ControlResponse> {
        let mut io = connect_endpoint(&self.endpoint).await.ok()?;
        rpc_on(&mut *io, req).await.ok()
    }

    // ---------------- 对齐 §4.6 / instances.rs 的业务接口 ----------------

    pub async fn create_session(&self, req: CreateSessionReq) -> Result<SessionInfo, LocalError> {
        match self.rpc_ready(&ControlRequest::CreateSession(req)).await? {
            ControlResponse::Session(s) => Ok(s),
            ControlResponse::Error { message, .. } => Err(LocalError::new("START_FAILED").with("detail", message)),
            other => Err(LocalError::new("START_FAILED").with("detail", format!("unexpected termd response: {other:?}"))),
        }
    }

    pub async fn stop_session(&self, id: &str) -> Result<bool, LocalError> {
        let Some(resp) = self.rpc_if_running(&ControlRequest::StopSession { id: id.to_string() }).await else {
            return Ok(false);
        };
        match resp {
            ControlResponse::Ok { changed } => Ok(changed),
            ControlResponse::Error { message, .. } => Err(LocalError::new("STOP_FAILED").with("detail", message)),
            _ => Ok(false),
        }
    }

    pub async fn list_sessions_if_running(&self) -> Option<Vec<SessionInfo>> {
        match self.rpc_if_running(&ControlRequest::ListSessions).await? {
            ControlResponse::Sessions(v) => Some(v),
            _ => None,
        }
    }

    pub async fn get_pane(&self, id: &str) -> Option<PaneInfo> {
        match self.rpc_if_running(&ControlRequest::GetPane { id: id.to_string() }).await? {
            ControlResponse::Pane(p) => p,
            _ => None,
        }
    }

    pub async fn pane_pids(&self, id: &str) -> Vec<u32> {
        match self.rpc_if_running(&ControlRequest::PanePids { id: id.to_string() }).await {
            Some(ControlResponse::Pids(v)) => v,
            _ => vec![],
        }
    }

    pub async fn send_keys(&self, id: &str, keys: &[&str]) -> Result<(), LocalError> {
        let req = ControlRequest::SendKeys {
            id: id.to_string(),
            keys: keys.iter().map(|s| (*s).to_string()).collect(),
        };
        expect_ok(self.rpc_if_running(&req).await)
    }

    pub async fn send_literal(&self, id: &str, text: &str) -> Result<(), LocalError> {
        let req = ControlRequest::SendLiteral {
            id: id.to_string(),
            data: text.to_string(),
        };
        expect_ok(self.rpc_if_running(&req).await)
    }

    pub async fn paste(&self, id: &str, text: &str) -> Result<(), LocalError> {
        let req = ControlRequest::Paste {
            id: id.to_string(),
            data: text.to_string(),
        };
        expect_ok(self.rpc_if_running(&req).await)
    }

    pub async fn capture(&self, id: &str, mode: CaptureMode) -> Option<String> {
        match self
            .rpc_if_running(&ControlRequest::Capture {
                id: id.to_string(),
                mode,
            })
            .await?
        {
            ControlResponse::Text(t) => t,
            _ => None,
        }
    }

    pub async fn history(
        &self,
        id: &str,
        epoch: Option<u64>,
        start_seq: Option<u64>,
        count: Option<u32>,
        tail_lines: Option<u32>,
    ) -> Option<(Option<HistorySlice>, bool)> {
        match self
            .rpc_if_running(&ControlRequest::History {
                id: id.to_string(),
                epoch,
                start_seq,
                count,
                tail_lines,
            })
            .await?
        {
            ControlResponse::History { slice, epoch_mismatch } => Some((slice, epoch_mismatch)),
            _ => None,
        }
    }

    pub async fn output_activity(&self) -> HashMap<String, i64> {
        match self.rpc_if_running(&ControlRequest::OutputActivity).await {
            Some(ControlResponse::Activity(items)) => items.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    pub async fn daemon_pid(&self) -> Option<u32> {
        self.ping_if_running().await.map(|p| p.pid)
    }

    /// 建立 `looklook-term.v1` 双工流，并将客户端首包（Hello）透传给 `termd`。
    pub async fn open_term_stream(&self, id: &str, hello_frame: &[u8]) -> Result<BoxedIpc, LocalError> {
        let _ = self.ensure_ready().await?;
        let mut io = connect_endpoint(&self.endpoint)
            .await
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        match rpc_on(&mut *io, &ControlRequest::OpenTermStream { id: id.to_string() }).await? {
            ControlResponse::StreamReady => {}
            ControlResponse::Error { code, message } => {
                let err_code = if code == "not_found" { "NOT_FOUND" } else { "START_FAILED" };
                return Err(LocalError::new(err_code).with("detail", message));
            }
            other => {
                return Err(LocalError::new("START_FAILED").with("detail", format!("unexpected response: {other:?}")));
            }
        }
        write_frame(&mut *io, hello_frame)
            .await
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        Ok(io)
    }

    /// 建立旧前端 ttyd 兼容字节流，等待并返回初始屏缓冲区快照 + 标题。
    pub async fn open_legacy_stream(
        &self,
        id: &str,
        cols: u16,
        rows: u16,
        client_id: String,
    ) -> Result<(LegacyDownMsg, BoxedIpc), LocalError> {
        let _ = self.ensure_ready().await?;
        let mut io = connect_endpoint(&self.endpoint)
            .await
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        let req = ControlRequest::OpenLegacyStream {
            id: id.to_string(),
            cols,
            rows,
            client_id,
        };
        match rpc_on(&mut *io, &req).await? {
            ControlResponse::StreamReady => {}
            ControlResponse::Error { code, message } => {
                let err_code = if code == "not_found" { "NOT_FOUND" } else { "START_FAILED" };
                return Err(LocalError::new(err_code).with("detail", message));
            }
            other => {
                return Err(LocalError::new("START_FAILED").with("detail", format!("unexpected response: {other:?}")));
            }
        }
        let first = read_frame(&mut *io)
            .await
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?
            .ok_or_else(|| LocalError::new("START_FAILED").with("detail", "termd legacy EOF"))?;
        let init: LegacyDownMsg = bincode::deserialize(&first)
            .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
        Ok((init, io))
    }
}

fn expect_ok(resp: Option<ControlResponse>) -> Result<(), LocalError> {
    match resp {
        Some(ControlResponse::Ok { changed: true }) => Ok(()),
        Some(ControlResponse::Ok { changed: false }) => Err(LocalError::new("NOT_FOUND")),
        Some(ControlResponse::Error { message, .. }) => Err(LocalError::new("START_FAILED").with("detail", message)),
        _ => Err(LocalError::new("START_FAILED").with("detail", "termd unavailable")),
    }
}

async fn ping_on(io: &mut (impl AsyncRead + AsyncWrite + Unpin + ?Sized)) -> Result<PongInfo> {
    match rpc_on(io, &ControlRequest::Ping).await {
        Ok(ControlResponse::Pong {
            control_version,
            protocol_version,
            binary_version,
            pid,
            session_count,
            handover_supported,
        }) => Ok(PongInfo {
            control_version,
            protocol_version,
            binary_version,
            pid,
            session_count,
            handover_supported,
        }),
        Ok(other) => anyhow::bail!("unexpected Ping response: {other:?}"),
        Err(e) => anyhow::bail!("Ping failed: {:?}", e.code),
    }
}

async fn rpc_on(
    io: &mut (impl AsyncRead + AsyncWrite + Unpin + ?Sized),
    req: &ControlRequest,
) -> Result<ControlResponse, LocalError> {
    let payload = bincode::serialize(req)
        .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
    write_frame(io, &payload)
        .await
        .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?;
    let raw = read_frame(io)
        .await
        .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))?
        .ok_or_else(|| LocalError::new("START_FAILED").with("detail", "termd EOF"))?;
    bincode::deserialize(&raw)
        .map_err(|e| LocalError::new("START_FAILED").with("detail", e.to_string()))
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(io: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        anyhow::bail!("ipc frame too large: {} bytes", payload.len());
    }
    let len = (payload.len() as u32).to_le_bytes();
    io.write_all(&len).await?;
    io.write_all(payload).await?;
    io.flush().await?;
    Ok(())
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin + ?Sized>(io: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match io.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read ipc frame len"),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("ipc frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await.context("read ipc frame body")?;
    Ok(Some(buf))
}

#[cfg(unix)]
async fn connect_endpoint(endpoint: &str) -> Result<BoxedIpc> {
    let s = tokio::net::UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("connect unix socket {endpoint}"))?;
    Ok(Box::new(s))
}

#[cfg(windows)]
async fn connect_endpoint(endpoint: &str) -> Result<BoxedIpc> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    for attempt in 0..20 {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(Box::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempt < 19 => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("connect named pipe {endpoint}")),
        }
    }
    anyhow::bail!("named pipe busy: {endpoint}")
}

fn stamp_of(path: &Path) -> Option<BinStamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(BinStamp {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
        })
    }
    #[cfg(not(unix))]
    {
        Some(BinStamp { len: m.len() })
    }
}

fn running_binary_replaced(pid: u32, disk_bin: Option<&Path>) -> bool {
    let Some(disk_bin) = disk_bin else { return false };
    #[cfg(target_os = "linux")]
    {
        let proc_exe = PathBuf::from(format!("/proc/{pid}/exe"));
        if let Ok(target) = std::fs::read_link(&proc_exe) {
            if target.to_string_lossy().ends_with(" (deleted)") {
                return true;
            }
        }
        if let (Some(run_s), Some(disk_s)) = (stamp_of(&proc_exe), stamp_of(disk_bin)) {
            return run_s != disk_s;
        }
    }
    let _ = (pid, stamp_of(disk_bin));
    false
}

fn tail_log(path: &Path) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = s.lines().rev().take(12).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}
