//! 通用插件管理（docs/BROWSER_PLUGIN_DESIGN_2026-11-19.md）。
//!
//! 看看只负责发现、离线公钥验签、安装/空闲更新、按需启停、短路径 Unix socket 通信与状态缓存，
//! 不认识任何具体插件。代码目录（`<home>/plugins/<名>/`）与持久化数据目录
//! （`<home>/plugin-data/<名>/`）分离，更新或重装插件不会删除用户数据。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use ed25519_dalek::VerifyingKey;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use looklook_protocol::plugin::{
    self as proto, Manifest, PackageIndex, PackageSignature, PluginStatus, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::account::{Account, VERSION};
use crate::error::LocalError;
use crate::paths;
use crate::settings::Settings;
use crate::store::Store;

/// 同一插件在 60 秒内崩溃超过此次数后暂停自动拉起，避免死循环。
const CRASH_WINDOW: Duration = Duration::from_secs(60);
const MAX_CRASHES: usize = 3;
/// 启动后等 Unix socket 开始接收连接的上限。
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// 优雅停机（POST /_plugin/shutdown）等待进程退出的上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// 空闲自动更新检查周期。
const AUTO_UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// 插件日志单文件上限（超过后启动时截断重来）。
const MAX_LOG_BYTES: u64 = 1 << 20;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rlock<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn wlock<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// 不兼容原因（`PROTOCOL_UNSUPPORTED` / `CLIENT_TOO_OLD` / `PLATFORM_UNSUPPORTED`），为 Some 时拒绝启动
    pub incompatible: Option<&'static str>,
}

/// 一次发往插件的转发连接信息（同时持有活跃连接计数，断开时刷新 `last_active_ms`）。
pub struct RunningHandle {
    pub manifest: Manifest,
    pub socket: PathBuf,
    pub token: String,
    pub guard: ActiveGuard,
}

pub struct ActiveGuard {
    conns: Arc<AtomicUsize>,
    last_active_ms: Arc<AtomicI64>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.conns.fetch_sub(1, Ordering::SeqCst);
        self.last_active_ms.store(crate::util::system_ms(), Ordering::SeqCst);
    }
}

struct RunningPlugin {
    manifest: Manifest,
    socket: PathBuf,
    token: String,
    child: Child,
    conns: Arc<AtomicUsize>,
    last_active_ms: Arc<AtomicI64>,
    status: PluginStatus,
}

impl RunningPlugin {
    fn handle(&self) -> RunningHandle {
        self.conns.fetch_add(1, Ordering::SeqCst);
        self.last_active_ms.store(crate::util::system_ms(), Ordering::SeqCst);
        RunningHandle {
            manifest: self.manifest.clone(),
            socket: self.socket.clone(),
            token: self.token.clone(),
            guard: ActiveGuard { conns: self.conns.clone(), last_active_ms: self.last_active_ms.clone() },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallJob {
    pub name: String,
    pub state: &'static str, // "running" | "ok" | "failed"
    pub step: &'static str,  // "ticket" | "download" | "verify" | "install" | "done"
    pub received: u64,
    pub total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Plugins {
    home: PathBuf,
    keys: HashMap<String, VerifyingKey>,
    host_url: RwLock<String>,
    installed: RwLock<BTreeMap<String, InstalledPlugin>>,
    running: Mutex<HashMap<String, RunningPlugin>>,
    crashes: Mutex<HashMap<String, VecDeque<Instant>>>,
    start_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    jobs: Mutex<HashMap<String, InstallJob>>,
    gen: AtomicU64,
}

impl Plugins {
    pub fn new(home: PathBuf, keys: HashMap<String, VerifyingKey>) -> Arc<Self> {
        let p = Arc::new(Self {
            home,
            keys,
            host_url: RwLock::new(String::new()),
            installed: RwLock::new(BTreeMap::new()),
            running: Mutex::new(HashMap::new()),
            crashes: Mutex::new(HashMap::new()),
            start_locks: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            gen: AtomicU64::new(1),
        });
        p.scan();
        p
    }

    pub fn set_host_url(&self, url: String) {
        *wlock(&self.host_url) = url;
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.home.join("plugins")
    }

    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.home.join("plugin-data").join(name)
    }

    /// 扫描 `<home>/plugins/*`：只接受目录名合法且离线签名 + 文件哈希全部通过的插件。
    pub fn scan(&self) {
        let mut found = BTreeMap::new();
        let Ok(rd) = std::fs::read_dir(self.plugins_dir()) else {
            *wlock(&self.installed) = found;
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if !proto::valid_name(&name) {
                continue;
            }
            match verify_dir(&path, &self.keys, Some(&name)) {
                Ok((manifest, _)) => {
                    let incompatible = check_compat(&manifest);
                    found.insert(name, InstalledPlugin { manifest, dir: path, incompatible });
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, error = %e, "跳过未通过验签的插件目录");
                }
            }
        }
        *wlock(&self.installed) = found;
    }

    pub fn get_installed(&self, name: &str) -> Option<InstalledPlugin> {
        rlock(&self.installed).get(name).cloned()
    }

    pub fn list_installed(&self) -> Vec<InstalledPlugin> {
        rlock(&self.installed).values().cloned().collect()
    }

    /// 给 `GET /api/status` 的摘要：**绝不拉起未运行的插件**。
    pub fn brief(&self) -> Vec<Value> {
        let installed = self.list_installed();
        let running = lock(&self.running);
        installed
            .into_iter()
            .map(|p| {
                let run = running.get(&p.manifest.name);
                json!({
                    "name": p.manifest.name,
                    "version": p.manifest.version,
                    "title": p.manifest.title,
                    "description": p.manifest.description,
                    "icon": p.manifest.icon,
                    "entries": p.manifest.entries,
                    "capabilities": p.manifest.capabilities,
                    "readonly_policy": p.manifest.readonly_policy,
                    "incompatible": p.incompatible,
                    "process_running": run.is_some(),
                    "running": run.is_some_and(|r| r.status.running),
                    "busy": run.is_some_and(|r| r.status.busy),
                    "badge": run.and_then(|r| r.status.badge.clone()),
                })
            })
            .collect()
    }

    pub fn all_jobs(&self) -> HashMap<String, InstallJob> {
        lock(&self.jobs).clone()
    }

    /// 校验插件调宿主白名单接口时带的 `HDR_TOKEN`（常量时间比较），返回对应的插件名。
    pub fn verify_host_token(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let running = lock(&self.running);
        for (name, r) in running.iter() {
            if crate::gateway::ct_eq(&r.token, token) {
                return Some(name.clone());
            }
        }
        None
    }

    fn start_lock(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = lock(&self.start_locks);
        map.entry(name.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }

    /// 看看启动时调用：对声明了 `autostart_if_exists` 且数据目录下存在该标记文件的插件立即拉起（如接回重启前在跑的浏览器）。
    pub fn autostart(self: &Arc<Self>) {
        for p in self.list_installed() {
            if p.incompatible.is_some() {
                continue;
            }
            let Some(rel) = p.manifest.autostart_if_exists.as_deref() else { continue };
            if self.data_dir(&p.manifest.name).join(rel).exists() {
                let this = self.clone();
                let name = p.manifest.name.clone();
                tokio::spawn(async move {
                    tracing::info!(plugin = %name, "检测到运行标记，自动拉起插件以接回后台任务");
                    if let Err(e) = this.ensure_running(&name).await {
                        tracing::warn!(plugin = %name, code = %e.code, "自动拉起插件失败");
                    }
                });
            }
        }
    }

    /// 确保插件已启动并返回连接句柄（未运行时按需拉起；启动前再次核验目录签名与文件哈希）。
    pub async fn ensure_running(&self, name: &str) -> Result<RunningHandle, LocalError> {
        if !proto::valid_name(name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        // 快路径：已在跑且进程活着
        if let Some(h) = self.running_handle(name) {
            return Ok(h);
        }
        let mu = self.start_lock(name);
        let _guard = mu.lock().await;
        if let Some(h) = self.running_handle(name) {
            return Ok(h);
        }
        #[cfg(not(unix))]
        {
            return Err(LocalError::new("PLUGIN_UNSUPPORTED"));
        }
        #[cfg(unix)]
        {
            let inst = self.get_installed(name).ok_or_else(|| LocalError::new("PLUGIN_NOT_FOUND"))?;
            if let Some(reason) = inst.incompatible {
                return Err(LocalError::new("PLUGIN_INCOMPATIBLE").with("reason", reason));
            }
            if self.crash_limited(name) {
                return Err(LocalError::new("PLUGIN_START_FAILED").with("reason", "crash_limit"));
            }
            // 启动前再次校验离线签名与每个文件 SHA-256，防止安装后被篡改
            let (manifest, _) = verify_dir(&inst.dir, &self.keys, Some(name)).map_err(|e| {
                tracing::error!(plugin = %name, error = %e, "插件启动前验签失败");
                LocalError::new("PLUGIN_BAD_SIGNATURE")
            })?;
            if let Some(reason) = check_compat(&manifest) {
                return Err(LocalError::new("PLUGIN_INCOMPATIBLE").with("reason", reason));
            }
            let r = self.spawn_unix(name, &inst.dir, manifest).await.map_err(|e| {
                tracing::error!(plugin = %name, error = %e, "启动插件失败");
                self.record_crash(name);
                LocalError::new("PLUGIN_START_FAILED")
            })?;
            let h = r.handle();
            lock(&self.running).insert(name.to_string(), r);
            Ok(h)
        }
    }

    fn running_handle(&self, name: &str) -> Option<RunningHandle> {
        let mut map = lock(&self.running);
        let alive = matches!(map.get_mut(name)?.child.try_wait(), Ok(None));
        if !alive {
            if let Some(r) = map.remove(name) {
                let _ = std::fs::remove_file(&r.socket);
            }
            return None;
        }
        map.get(name).map(RunningPlugin::handle)
    }

    fn crash_limited(&self, name: &str) -> bool {
        let now = Instant::now();
        let mut map = lock(&self.crashes);
        let q = map.entry(name.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            q.pop_front();
        }
        q.len() >= MAX_CRASHES
    }

    fn record_crash(&self, name: &str) {
        let now = Instant::now();
        let mut map = lock(&self.crashes);
        let q = map.entry(name.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            q.pop_front();
        }
        q.push_back(now);
    }

    #[cfg(unix)]
    async fn spawn_unix(&self, name: &str, dir: &Path, manifest: Manifest) -> Result<RunningPlugin> {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = self.data_dir(name);
        std::fs::create_dir_all(&data_dir)?;
        let _ = std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700));

        let sock_dir = unix_socket_dir()?;
        let gen = self.gen.fetch_add(1, Ordering::Relaxed);
        let socket = sock_dir.join(format!("{name}-{gen}.sock"));
        let _ = std::fs::remove_file(&socket);

        let token = data_encoding::HEXLOWER.encode(&crate::util::random_bytes::<32>());
        let host_url = rlock(&self.host_url).clone();
        let exec = dir.join(&manifest.exec);

        let logs = self.home.join("logs");
        std::fs::create_dir_all(&logs)?;
        let log_path = logs.join(format!("plugin-{name}.log"));
        if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            let _ = std::fs::File::create(&log_path);
        }
        let log_out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let log_err = log_out.try_clone()?;

        let mut cmd = tokio::process::Command::new(&exec);
        cmd.current_dir(dir)
            .env(proto::ENV_SOCKET, &socket)
            .env(proto::ENV_TOKEN, &token)
            .env(proto::ENV_DATA_DIR, &data_dir)
            .env(proto::ENV_HOST_URL, &host_url)
            .env(proto::ENV_NAME, name)
            .env(proto::ENV_PROTOCOL, PROTOCOL_VERSION.to_string())
            .env(proto::ENV_CLIENT_VERSION, VERSION)
            .env(proto::ENV_HOME, &self.home)
            .stdin(std::process::Stdio::null())
            .stdout(log_out)
            .stderr(log_err);

        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }

        let mut child = cmd.spawn().with_context(|| format!("spawn {}", exec.display()))?;

        // 等 Unix socket 开始监听
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                let _ = std::fs::remove_file(&socket);
                bail!("插件进程启动后立即退出: {st}");
            }
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = std::fs::remove_file(&socket);
                bail!("等待插件监听 socket 超时");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        tracing::info!(plugin = %name, version = %manifest.version, socket = %socket.display(), "插件已启动");
        Ok(RunningPlugin {
            manifest,
            socket,
            token,
            child,
            conns: Arc::new(AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicI64::new(crate::util::system_ms())),
            status: PluginStatus::default(),
        })
    }

    /// 优雅停止插件：先发 `POST /_plugin/shutdown`，等 5 秒未退出再 SIGTERM / KILL。
    pub async fn stop(&self, name: &str) {
        let running = lock(&self.running).remove(name);
        let Some(mut r) = running else { return };
        #[cfg(unix)]
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                unix_request(&r.socket, Method::POST, proto::PLUGIN_SHUTDOWN_PATH, &r.token, None, None),
            )
            .await;
        }
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, r.child.wait()).await.is_err() {
            #[cfg(unix)]
            if let Some(pid) = r.child.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            if tokio::time::timeout(Duration::from_secs(2), r.child.wait()).await.is_err() {
                let _ = r.child.kill().await;
                let _ = r.child.wait().await;
            }
        }
        let _ = std::fs::remove_file(&r.socket);
        tracing::info!(plugin = %name, "插件已停止");
    }

    pub async fn stop_all(&self) {
        let names: Vec<String> = lock(&self.running).keys().cloned().collect();
        for name in names {
            self.stop(&name).await;
        }
    }

    /// 查询插件状态（只在已运行时问，不会触发拉起）。
    #[cfg(unix)]
    async fn refresh_status(&self, name: &str) -> Option<PluginStatus> {
        let (socket, token) = {
            let map = lock(&self.running);
            let r = map.get(name)?;
            (r.socket.clone(), r.token.clone())
        };
        let (status, body) = unix_request(&socket, Method::GET, proto::PLUGIN_STATUS_PATH, &token, None, None).await.ok()?;
        if status != StatusCode::OK {
            return None;
        }
        let parsed: PluginStatus = serde_json::from_slice(&body).ok()?;
        if let Some(r) = lock(&self.running).get_mut(name) {
            r.status = parsed.clone();
        }
        Some(parsed)
    }

    /// 后台看护：回收意外退出的进程、刷新运行中插件状态、按 `busy` 与闲置时间自动停、空闲时自动更新。
    pub async fn watch(self: Arc<Self>, account: Arc<Account>, store: Arc<Store>, shutdown: CancellationToken) {
        let mut last_update_check = Instant::now();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.stop_all().await;
                    return;
                }
                _ = tokio::time::sleep(Duration::from_secs(15)) => {}
            }
            self.tick().await;
            if last_update_check.elapsed() >= AUTO_UPDATE_INTERVAL {
                last_update_check = Instant::now();
                if Settings::load(&store).auto_update && account.session().is_some() {
                    self.auto_update_idle(&account).await;
                }
            }
        }
    }

    async fn tick(&self) {
        // 1. 清理意外退出的插件
        let mut exited = Vec::new();
        {
            let mut map = lock(&self.running);
            let names: Vec<String> = map.keys().cloned().collect();
            for name in names {
                if let Some(r) = map.get_mut(&name) {
                    if let Ok(Some(st)) = r.child.try_wait() {
                        tracing::warn!(plugin = %name, status = %st, "插件意外退出");
                        if let Some(dead) = map.remove(&name) {
                            let _ = std::fs::remove_file(&dead.socket);
                            exited.push((name, dead.manifest.autostart_if_exists));
                        }
                    }
                }
            }
        }
        for (name, autostart) in exited {
            self.record_crash(&name);
            if let Some(rel) = autostart {
                if self.data_dir(&name).join(rel).exists() && !self.crash_limited(&name) {
                    tracing::info!(plugin = %name, "插件存在运行标记，尝试自动拉起");
                    let _ = self.ensure_running(&name).await;
                }
            }
        }

        // 2. 刷新运行中插件状态 & 闲置回收（尊重 busy）
        let names: Vec<String> = lock(&self.running).keys().cloned().collect();
        let now = crate::util::system_ms();
        for name in names {
            #[cfg(unix)]
            let st = self.refresh_status(&name).await;
            #[cfg(not(unix))]
            let st: Option<PluginStatus> = None;

            let should_stop = {
                let map = lock(&self.running);
                map.get(&name).is_some_and(|r| {
                    let idle_min = r.manifest.idle_stop_minutes;
                    let busy = st.as_ref().map_or(r.status.busy, |s| s.busy);
                    let active = r.conns.load(Ordering::SeqCst) > 0;
                    let idle_ms = now.saturating_sub(r.last_active_ms.load(Ordering::SeqCst));
                    idle_min > 0 && !busy && !active && idle_ms >= idle_min as i64 * 60_000
                })
            };
            if should_stop {
                tracing::info!(plugin = %name, "插件闲置且不忙，自动停止");
                self.stop(&name).await;
            }
        }
    }

    async fn auto_update_idle(&self, account: &Arc<Account>) {
        let Ok(cat) = account.plugin_catalog(paths::target()).await else { return };
        for item in cat.plugins {
            if !item.licensed || item.sha256.is_empty() {
                continue;
            }
            let Some(inst) = self.get_installed(&item.name) else { continue };
            if !is_newer(&item.version, &inst.manifest.version) {
                continue;
            }
            // 只在空闲（未运行，或运行中且 !busy && 无活跃连接）时自动更新
            let busy = {
                let map = lock(&self.running);
                map.get(&item.name).is_some_and(|r| r.status.busy || r.conns.load(Ordering::SeqCst) > 0)
            };
            if busy {
                continue;
            }
            tracing::info!(plugin = %item.name, from = %inst.manifest.version, to = %item.version, "发现插件新版本，空闲自动更新");
            if let Err(e) = self.install_now(account, &item.name, false).await {
                tracing::warn!(plugin = %item.name, error = %e.code, "插件自动更新失败");
            }
        }
    }

    /// 异步启动安装/更新任务（立即返回进行中的任务）。
    pub fn start_install(self: &Arc<Self>, account: Arc<Account>, name: String) -> Result<InstallJob, LocalError> {
        if !proto::valid_name(&name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        if account.session().is_none() {
            return Err(LocalError::new("NOT_LOGGED_IN"));
        }
        let job = {
            let mut jobs = lock(&self.jobs);
            if let Some(cur) = jobs.get(&name) {
                if cur.state == "running" {
                    return Err(LocalError::new("PLUGIN_INSTALLING"));
                }
            }
            let j = InstallJob { name: name.clone(), state: "running", step: "ticket", received: 0, total: 0, error: None };
            jobs.insert(name.clone(), j.clone());
            j
        };
        let this = self.clone();
        tokio::spawn(async move {
            match this.install_now(&account, &name, true).await {
                Ok(()) => {
                    this.update_job(&name, |j| {
                        j.state = "ok";
                        j.step = "done";
                        j.error = None;
                    });
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, code = %e.code, "安装插件失败");
                    this.update_job(&name, |j| {
                        j.state = "failed";
                        j.error = Some(e.code.clone());
                    });
                }
            }
        });
        Ok(job)
    }

    fn update_job(&self, name: &str, f: impl FnOnce(&mut InstallJob)) {
        if let Some(j) = lock(&self.jobs).get_mut(name) {
            f(j);
        }
    }

    /// 完整安装流程：向平台申请下载票据 → 下载并校验 tar.gz SHA-256 → 解包到暂存目录 → 验离线签名与每文件 SHA-256 → 停旧进程 → 原子替换（不动 plugin-data）。
    pub async fn install_now(&self, account: &Arc<Account>, name: &str, track_job: bool) -> Result<(), LocalError> {
        let ticket = account.plugin_download(name, paths::target()).await?;
        if ticket.size > proto::MAX_PACKAGE_BYTES {
            return Err(LocalError::new("VALIDATION_FAILED").with("field", "size"));
        }
        if track_job {
            self.update_job(name, |j| {
                j.step = "download";
                j.total = ticket.size;
            });
        }
        let stage_root = self.home.join("plugins").join(".staging");
        std::fs::create_dir_all(&stage_root)?;
        let tmp_tar = stage_root.join(format!("{name}-{}.tar.gz", crate::util::random_id(6)));
        let dl_res = account
            .platform()
            .download(&ticket.url, &tmp_tar, proto::MAX_PACKAGE_BYTES, |got, total| {
                if track_job {
                    self.update_job(name, |j| {
                        j.received = got;
                        if let Some(t) = total {
                            j.total = t;
                        }
                    });
                }
            })
            .await;
        let got_sha = match dl_res {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_tar);
                return Err(e.into());
            }
        };
        if !got_sha.eq_ignore_ascii_case(&ticket.sha256) {
            let _ = std::fs::remove_file(&tmp_tar);
            return Err(LocalError::new("PLUGIN_BAD_SIGNATURE"));
        }

        if track_job {
            self.update_job(name, |j| j.step = "verify");
        }
        let res = self.install_from_archive(&tmp_tar, name, Some(&ticket.version)).await;
        let _ = std::fs::remove_file(&tmp_tar);
        res
    }

    /// 从本地已下载的 `.tar.gz` 验签并原子安装（安装逻辑与单测共用）。
    pub async fn install_from_archive(&self, archive: &Path, name: &str, expected_version: Option<&str>) -> Result<(), LocalError> {
        let stage_root = self.plugins_dir().join(".staging");
        std::fs::create_dir_all(&stage_root)?;
        let stage = stage_root.join(format!("{name}-{}", crate::util::random_id(6)));
        std::fs::create_dir_all(&stage)?;

        let unpack = crate::archive::extract_plugin_tar_gz(archive, &stage);
        if let Err(e) = unpack {
            let _ = std::fs::remove_dir_all(&stage);
            tracing::warn!(plugin = %name, error = %e, "解包插件失败");
            return Err(LocalError::new("PLUGIN_BAD_SIGNATURE"));
        }

        let (manifest, pkg) = match verify_dir(&stage, &self.keys, Some(name)) {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                tracing::warn!(plugin = %name, error = %e, "插件验签失败");
                return Err(LocalError::new("PLUGIN_BAD_SIGNATURE"));
            }
        };
        if let Some(v) = expected_version {
            if manifest.version != v {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(LocalError::new("PLUGIN_BAD_SIGNATURE"));
            }
        }
        if let Some(reason) = check_compat(&manifest) {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(LocalError::new("PLUGIN_INCOMPATIBLE").with("reason", reason));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700));
            let mut execs = pkg.executables.clone();
            if !execs.iter().any(|e| e == &manifest.exec) {
                execs.push(manifest.exec.clone());
            }
            for rel in execs {
                let _ = std::fs::set_permissions(stage.join(rel), std::fs::Permissions::from_mode(0o755));
            }
        }
        #[cfg(not(unix))]
        let _ = pkg;

        // 停旧进程后再原子替换代码目录；plugin-data 目录原样保留
        self.stop(name).await;
        let dest = self.plugins_dir().join(name);
        let backup = stage_root.join(format!("{name}-old-{}", crate::util::random_id(6)));
        if dest.exists() {
            std::fs::rename(&dest, &backup)?;
        }
        if let Err(e) = std::fs::rename(&stage, &dest) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, &dest);
            }
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e.into());
        }
        if backup.exists() {
            let _ = std::fs::remove_dir_all(&backup);
        }
        self.scan();
        tracing::info!(plugin = %name, version = %manifest.version, "插件安装完成");
        Ok(())
    }

    pub async fn uninstall(&self, name: &str, remove_data: bool) -> Result<(), LocalError> {
        if !proto::valid_name(name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        if self.get_installed(name).is_none() {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        self.stop(name).await;
        let dir = self.plugins_dir().join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        if remove_data {
            let data = self.data_dir(name);
            if data.exists() {
                std::fs::remove_dir_all(&data)?;
            }
        }
        self.scan();
        Ok(())
    }

    /// 调插件的 JSON HTTP 接口（用于 `open_url` 等宿主聚合能力）。
    #[cfg(unix)]
    pub async fn call_json(&self, name: &str, method: Method, path: &str, locale: Option<&str>, body: Option<Vec<u8>>) -> Result<(StatusCode, Value), LocalError> {
        let h = self.ensure_running(name).await?;
        let (status, bytes) = unix_request(&h.socket, method, path, &h.token, locale, body)
            .await
            .map_err(|_| LocalError::new("PLUGIN_START_FAILED"))?;
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, v))
    }

    #[cfg(not(unix))]
    pub async fn call_json(&self, _name: &str, _method: Method, _path: &str, _locale: Option<&str>, _body: Option<Vec<u8>>) -> Result<(StatusCode, Value), LocalError> {
        Err(LocalError::new("PLUGIN_UNSUPPORTED"))
    }
}

/// 核验一个插件目录：离线 Ed25519 签名 + `package.json` 列表里的每个文件 SHA-256 + `manifest.json` 合法性。
pub fn verify_dir(dir: &Path, keys: &HashMap<String, VerifyingKey>, expected_name: Option<&str>) -> Result<(Manifest, PackageIndex)> {
    let pkg_bytes = std::fs::read(dir.join(proto::PACKAGE_FILE)).context("缺少 package.json")?;
    let sig_bytes = std::fs::read(dir.join(proto::SIGNATURE_FILE)).context("缺少 package.sig")?;
    let sig: PackageSignature = serde_json::from_slice(&sig_bytes).context("package.sig 格式错误")?;
    if !proto::verify_package(keys, &pkg_bytes, &sig) {
        bail!("插件包签名校验失败");
    }
    let pkg = PackageIndex::parse(&pkg_bytes).map_err(|e| anyhow::anyhow!(e))?;
    if let Some(n) = expected_name {
        if pkg.name != n {
            bail!("插件名不匹配: 期望 {n}, 实际 {}", pkg.name);
        }
    }
    for (rel, expected_sha) in &pkg.files {
        let p = dir.join(rel);
        let meta = std::fs::symlink_metadata(&p).with_context(|| format!("缺少文件 {rel}"))?;
        if !meta.file_type().is_file() {
            bail!("非法文件类型（禁止符号链接）: {rel}");
        }
        let actual = sha256_file(&p)?;
        if !actual.eq_ignore_ascii_case(expected_sha) {
            bail!("文件校验和不匹配: {rel}");
        }
    }
    let manifest_bytes = std::fs::read(dir.join(proto::MANIFEST_FILE)).context("缺少 manifest.json")?;
    let manifest = Manifest::parse(&manifest_bytes).map_err(|e| anyhow::anyhow!(e))?;
    if manifest.name != pkg.name || manifest.version != pkg.version {
        bail!("manifest 与 package 信息不一致");
    }
    if !pkg.files.contains_key(&manifest.exec) {
        bail!("可执行文件不在签名清单中: {}", manifest.exec);
    }
    Ok((manifest, pkg))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(data_encoding::HEXLOWER.encode(&h.finalize()))
}

fn check_compat(m: &Manifest) -> Option<&'static str> {
    if !m.supports_protocol(PROTOCOL_VERSION) {
        return Some("PROTOCOL_UNSUPPORTED");
    }
    if !m.supports_platform(paths::target()) {
        return Some("PLATFORM_UNSUPPORTED");
    }
    if !m.min_client.is_empty() {
        if let (Ok(cur), Ok(min)) = (semver::Version::parse(VERSION), semver::Version::parse(&m.min_client)) {
            if cur < min {
                return Some("CLIENT_TOO_OLD");
            }
        }
    }
    None
}

fn is_newer(remote: &str, local: &str) -> bool {
    match (semver::Version::parse(remote), semver::Version::parse(local)) {
        (Ok(r), Ok(l)) => r > l,
        _ => remote != local,
    }
}

/// 短路径 Unix socket 目录（避开 108 字节 `sun_path` 上限），目录权限 0700。
#[cfg(unix)]
fn unix_socket_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = unsafe { libc::geteuid() };
    let candidate = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("looklook-p"))
        .filter(|p| p.to_string_lossy().len() <= 64)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/looklook-p-{uid}")));
    std::fs::create_dir_all(&candidate)?;
    let meta = std::fs::symlink_metadata(&candidate)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        bail!("socket 目录所有权异常: {}", candidate.display());
    }
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700))?;
    Ok(candidate)
}

#[cfg(unix)]
async fn unix_request(
    socket: &Path,
    method: Method,
    path: &str,
    token: &str,
    locale: Option<&str>,
    body: Option<Vec<u8>>,
) -> Result<(StatusCode, Bytes)> {
    let stream = tokio::time::timeout(Duration::from_secs(5), tokio::net::UnixStream::connect(socket)).await??;
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "localhost")
        .header(proto::HDR_TOKEN, token);
    if let Some(l) = locale {
        builder = builder.header(proto::HDR_LOCALE, l);
    }
    let body_bytes = if let Some(b) = body {
        builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
        Bytes::from(b)
    } else {
        Bytes::new()
    };
    let req = builder.body(Full::new(body_bytes))?;
    let resp: hyper::Response<Incoming> = tokio::time::timeout(Duration::from_secs(15), sender.send_request(req)).await??;
    let status = resp.status();
    let bytes = tokio::time::timeout(Duration::from_secs(15), resp.into_body().collect()).await??.to_bytes();
    Ok((status, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use looklook_protocol::plugin::{Capabilities, Entry, Localized, ProtocolRange, ReadonlyPolicy};
    use looklook_protocol::signing::sha256_hex;

    fn make_test_archive(dir: &Path, key: &SigningKey, name: &str, version: &str, script: &str) -> PathBuf {
        let manifest = Manifest {
            name: name.into(),
            version: version.into(),
            protocol: ProtocolRange { min: 1, max: 1 },
            min_client: "1.0.0".into(),
            platforms: vec![paths::target().into()],
            exec: "bin/run.sh".into(),
            title: Localized { zh: "测试插件".into(), en: "Test plugin".into() },
            description: Localized::default(),
            icon: "globe".into(),
            entries: vec![Entry { id: "main".into(), title: Localized { zh: "测试".into(), en: "Test".into() }, icon: "globe".into(), path: String::new() }],
            capabilities: Capabilities::default(),
            autostart_if_exists: None,
            readonly_policy: ReadonlyPolicy::Deny,
            inject_web_prefixes: vec![],
            idle_stop_minutes: 10,
            requires_license: false,
        };
        let m_bytes = serde_json::to_vec(&manifest).unwrap();
        let s_bytes = script.as_bytes();
        let mut files = BTreeMap::new();
        files.insert(proto::MANIFEST_FILE.into(), sha256_hex(&m_bytes));
        files.insert("bin/run.sh".into(), sha256_hex(s_bytes));
        let idx = PackageIndex {
            name: name.into(),
            version: version.into(),
            target: paths::target().into(),
            files,
            executables: vec!["bin/run.sh".into()],
        };
        let idx_bytes = serde_json::to_vec(&idx).unwrap();
        let sig = proto::sign_package(key, "k1", &idx_bytes);
        let sig_bytes = serde_json::to_vec(&sig).unwrap();

        let tar_path = dir.join(format!("{name}-{version}.tar.gz"));
        let f = std::fs::File::create(&tar_path).unwrap();
        let enc = GzEncoder::new(f, Compression::default());
        let mut ar = tar::Builder::new(enc);
        let mut append = |path: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append_data(&mut h, path, data).unwrap();
        };
        append(proto::MANIFEST_FILE, &m_bytes);
        append(proto::PACKAGE_FILE, &idx_bytes);
        append(proto::SIGNATURE_FILE, &sig_bytes);
        append("bin/run.sh", s_bytes);
        ar.into_inner().unwrap().finish().unwrap();
        tar_path
    }

    #[tokio::test]
    async fn install_verify_tamper_and_preserve_data() {
        let tmp = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[11u8; 32]);
        let keys = HashMap::from([("k1".into(), key.verifying_key())]);
        let plugins = Plugins::new(tmp.path().to_path_buf(), keys.clone());

        let pkg1 = make_test_archive(tmp.path(), &key, "demo", "1.0.0", "#!/bin/sh\nexit 0\n");
        plugins.install_from_archive(&pkg1, "demo", Some("1.0.0")).await.unwrap();
        assert_eq!(plugins.list_installed().len(), 1);
        assert_eq!(plugins.brief().len(), 1);

        // 在数据目录放文件：升级插件后必须保留
        let data = plugins.data_dir("demo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("keep.txt"), "ok").unwrap();

        let pkg2 = make_test_archive(tmp.path(), &key, "demo", "1.1.0", "#!/bin/sh\nexit 0\n");
        plugins.install_from_archive(&pkg2, "demo", Some("1.1.0")).await.unwrap();
        assert_eq!(plugins.get_installed("demo").unwrap().manifest.version, "1.1.0");
        assert_eq!(std::fs::read_to_string(data.join("keep.txt")).unwrap(), "ok");

        // 篡改文件：验签必须拒绝
        std::fs::write(plugins.plugins_dir().join("demo/bin/run.sh"), "evil").unwrap();
        assert!(verify_dir(&plugins.plugins_dir().join("demo"), &keys, Some("demo")).is_err());

        // 错误公钥安装必须拒绝
        let other = Plugins::new(tmp.path().join("other"), HashMap::new());
        assert!(other.install_from_archive(&pkg2, "demo", None).await.is_err());
    }
}
