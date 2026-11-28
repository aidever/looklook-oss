//! 通用插件（docs/BROWSER_PLUGIN_DESIGN_2026-11-19.md）：看看不认识任何具体插件，只按协议
//! （`looklook_protocol::plugin`）装、起、转发。
//!
//! - **装**：从服务端取这个平台签过名的 `release.json`（服务端查这个账号有没有这个插件的授权），用编译时内置的公钥
//!   （`LOOKLOOK_PLUGIN_KEYS`）验签，下载包、核对大小和 SHA-256，解到临时目录、核对包里的清单和签过名的一致，
//!   再整个换到 `<数据目录>/plugins/<名>/`。签名、哈希、清单有一处不对就不装。
//! - **数据**：`<数据目录>/plugin-data/<名>/`，更新、卸载插件都不动它。
//! - **起**：用到时才起（打开页面、调接口）；数据目录里有清单 `autostart_if_exists` 指的文件时，看看启动后马上起
//!   （例如浏览器还在后台跑，要接回来）。插件是看看的子进程（Linux 上看看退出它也退出），监听看看给的 Unix Socket，
//!   只认带口令（[`proto::HDR_TOKEN`]）的请求。崩了下次用到时再起。
//! - **授权**：插件自己验凭证；看看只在插件调宿主接口 `/_host/license` 时，用这台设备的身份代它向服务端申请
//!   （[`Plugins::license`]），不碰凭证内容。
//! - **离线授权**：插件页显示本机机器指纹（[`proto::machine_fingerprint`]，插件按同一个算法自己算），管理后台按它签离线授权码；
//!   用户粘贴进来存在 `<数据目录>/plugin-licenses.json`，插件来要凭证时先给它（不用登录、不用联网，到期为止）。
//! - **从文件安装**：管理后台或插件页下载的离线安装包（`.llplugin`），照样验签名和包的 SHA-256。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use looklook_protocol::plugin::{self as proto, Manifest, Release, ReleaseSig};
use serde::Serialize;
use serde_json::{json, Value};

use crate::account::Account;
use crate::error::LocalError;
use crate::paths::Paths;

/// 包最大多少（防止服务端出错时无限下载）
const PACKAGE_MAX: u64 = 1 << 30;
/// 起插件最多等多久（等它的 Socket 能连上）
const START_WAIT: Duration = Duration::from_secs(20);
/// 插件开着的功能缓存多久
const FEATURES_TTL: Duration = Duration::from_secs(60);
/// 钩子的回答最大多少
const HOOK_BODY_MAX: usize = 256 * 1024;
/// 离线授权码存在这里：`{ 插件名: 凭证 }`
const OFFLINE_FILE: &str = "plugin-licenses.json";

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn rfc3339(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn err(code: &'static str, detail: impl std::fmt::Display) -> LocalError {
    LocalError::new(code).with("detail", detail.to_string().chars().take(500).collect::<String>())
}

/// 装好的插件
#[derive(Debug, Clone)]
pub struct Installed {
    pub release: Release,
    pub dir: PathBuf,
}

impl Installed {
    pub fn manifest(&self) -> &Manifest {
        &self.release.manifest
    }
}

/// 一个在跑的插件
pub struct Proc {
    pub socket: PathBuf,
    pub token: String,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Proc {
    pub fn alive(&self) -> bool {
        lock(&self.child).as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn kill(&self) {
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.start_kill();
        }
    }
}

/// 装 / 更新的进度（界面轮询）
#[derive(Debug, Clone, Serialize, Default)]
pub struct Job {
    /// `download` | `verify` | `install` | `done` | `failed`
    pub step: &'static str,
    pub done: u64,
    pub total: Option<u64>,
    pub error: Option<Value>,
}

pub struct Plugins {
    paths: Paths,
    account: Arc<Account>,
    keys: Vec<(String, VerifyingKey)>,
    /// 插件调宿主接口（`/_host/*`）带的口令
    host_token: String,
    /// 管理台的本机地址（插件调宿主接口用），网关起来后设置
    host_url: Mutex<Option<String>>,
    running: tokio::sync::Mutex<HashMap<String, Arc<Proc>>>,
    jobs: Mutex<HashMap<String, Job>>,
    /// 本机机器指纹（算一次）
    machine: std::sync::OnceLock<Result<String, String>>,
    /// 插件开着的功能（[`proto::HOOK_FEATURES`]），缓存一会儿：状态接口、新建会话都要看
    features: Mutex<Option<(Instant, Vec<String>)>>,
}

impl Plugins {
    pub fn new(paths: Paths, account: Arc<Account>, keys: &str) -> Arc<Self> {
        Arc::new(Self {
            paths,
            account,
            keys: proto::parse_keys(keys),
            host_token: crate::util::random_secret(),
            host_url: Mutex::new(None),
            running: Default::default(),
            jobs: Mutex::new(HashMap::new()),
            machine: std::sync::OnceLock::new(),
            features: Mutex::new(None),
        })
    }

    pub fn set_host(&self, addr: std::net::SocketAddr) {
        *lock(&self.host_url) = Some(format!("http://127.0.0.1:{}", addr.port()));
    }

    /// 宿主接口的口令对不对
    pub fn host_token_ok(&self, token: &str) -> bool {
        let a = token.as_bytes();
        let b = self.host_token.as_bytes();
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |x, (p, q)| x | (p ^ q)) == 0
    }

    fn root(&self) -> PathBuf {
        self.paths.home.join("plugins")
    }

    pub fn dir(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.paths.home.join("plugin-data").join(name)
    }

    /// 装好的插件：签名重新验一遍（防止别的程序改了 `release.json`），清单的协议版本要对得上
    pub fn installed(&self, name: &str) -> Result<Installed, LocalError> {
        if !proto::valid_name(name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        let dir = self.dir(name);
        let raw = std::fs::read(dir.join("release.json")).map_err(|_| LocalError::new("PLUGIN_NOT_INSTALLED").with("plugin", name))?;
        let sig: ReleaseSig = std::fs::read(dir.join("release.sig")).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| LocalError::new("PLUGIN_INVALID").with("plugin", name))?;
        let release = proto::verify_release(&self.keys, &raw, &sig).map_err(|e| err("PLUGIN_INVALID", e).with("plugin", name))?;
        if release.name != name {
            return Err(LocalError::new("PLUGIN_INVALID").with("plugin", name));
        }
        Ok(Installed { release, dir })
    }

    /// 本机装着的插件名（目录在就算，装坏了的也列出来，界面上提示重新装）
    pub fn installed_names(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.root()) else { return vec![] };
        let mut v: Vec<String> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| proto::valid_name(n)).collect();
        v.sort();
        v
    }

    /// 这个插件能不能在这个看看上跑：协议版本、最低客户端版本
    fn compatible(m: &Manifest) -> Result<(), LocalError> {
        if m.protocol != proto::PROTOCOL {
            return Err(LocalError::new("PLUGIN_PROTOCOL").with("plugin", m.name.clone()));
        }
        if !m.min_client.is_empty() && proto::version_cmp(env!("CARGO_PKG_VERSION"), &m.min_client) == std::cmp::Ordering::Less {
            return Err(LocalError::new("PLUGIN_NEEDS_CLIENT").with("plugin", m.name.clone()).with("version", m.min_client.clone()));
        }
        Ok(())
    }

    /// 界面上的插件列表：本机装着的、服务端目录里的（带这个账号的授权），合在一起
    pub async fn list(&self, with_catalog: bool) -> Value {
        let platform = proto::current_platform();
        let catalog = if with_catalog && self.account.session().is_some() {
            match self.account.plugin_catalog(&platform).await {
                Ok(c) => Ok(c.items),
                Err(e) => Err(LocalError::from(e)),
            }
        } else {
            Ok(vec![])
        };
        let running = self.running.lock().await;
        let mut names = self.installed_names();
        if let Ok(c) = &catalog {
            for i in c {
                if !names.contains(&i.name) {
                    names.push(i.name.clone());
                }
            }
        }
        let jobs = lock(&self.jobs).clone();
        let offline = self.offline_valid();
        let incoming = self.incoming_dir();
        let _ = std::fs::create_dir_all(&incoming);
        let items: Vec<Value> = names
            .iter()
            .map(|n| {
                let local = self.installed(n);
                let remote = catalog.as_ref().ok().and_then(|c| c.iter().find(|i| &i.name == n));
                let m = local.as_ref().ok().map(|i| i.manifest().clone());
                let update = match (&local, remote) {
                    (Ok(l), Some(r)) => proto::version_cmp(&r.version, &l.release.version) == std::cmp::Ordering::Greater,
                    _ => false,
                };
                json!({
                    "name": n,
                    "installed": local.is_ok(),
                    // 目录在但签名不对、旧格式：要重新装
                    "broken": local.as_ref().err().filter(|e| e.code != "PLUGIN_NOT_INSTALLED").map(|e| e.code.to_string()),
                    "version": m.as_ref().map(|m| m.version.clone()),
                    "latest": remote.map(|r| r.version.clone()),
                    "update": update,
                    "title": m.as_ref().map(|m| json!(m.title)).or_else(|| remote.map(|r| json!(r.title))),
                    "description": m.as_ref().map(|m| json!(m.description)).or_else(|| remote.map(|r| json!(r.description))),
                    "icon": m.as_ref().map(|m| m.icon.clone()).or_else(|| remote.map(|r| r.icon.clone())),
                    "size": remote.map(|r| r.size),
                    "open_url": m.as_ref().is_some_and(|m| m.open_url),
                    "incompatible": m.as_ref().and_then(|m| Self::compatible(m).err()).map(|e| json!({ "code": e.code, "params": e.params })),
                    // 有离线授权码就算；没有时看服务端目录（没登录、没问服务端时不知道：null）
                    "licensed": if offline.iter().any(|(p, _)| p == n) { json!(true) } else { json!(remote.map(|r| r.licensed)) },
                    "expires_at": remote.and_then(|r| r.expires_at.clone()),
                    // 离线授权码到什么时候（RFC 3339）
                    "offline_until": offline.iter().find(|(p, _)| p == n).map(|(_, e)| rfc3339(*e)),
                    "running": running.get(n).is_some_and(|p| p.alive()),
                    "job": jobs.get(n),
                })
            })
            .collect();
        json!({
            "platform": platform,
            "items": items,
            "signed_in": self.account.session().is_some(),
            "catalog_error": catalog.err().map(|e| json!({ "code": e.code, "params": e.params })),
            "keys": !self.keys.is_empty(),
            "machine": self.machine().ok(),
            "offline": self.offline_view(),
            "incoming_dir": incoming.display().to_string(),
        })
    }

    /// 状态栏：在跑的插件（工具菜单上的“运行中”）
    pub async fn brief(&self) -> Vec<Value> {
        let running = self.running.lock().await;
        self.installed_names()
            .into_iter()
            .filter_map(|n| {
                let i = self.installed(&n).ok()?;
                let m = i.manifest();
                Some(json!({ "name": n, "title": m.title, "icon": m.icon, "open_url": m.open_url, "running": running.get(&n).is_some_and(|p| p.alive()), "incompatible": Self::compatible(m).is_err() }))
            })
            .collect()
    }

    fn set_job(&self, name: &str, f: impl FnOnce(&mut Job)) {
        let mut j = lock(&self.jobs);
        f(j.entry(name.to_string()).or_default());
    }

    /// 后台装 / 更新：进度在 [`Plugins::list`] 的 `job` 里
    pub fn start_install(self: &Arc<Self>, name: &str) -> Result<(), LocalError> {
        if !proto::valid_name(name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        if self.keys.is_empty() {
            return Err(LocalError::new("PLUGIN_NO_KEYS"));
        }
        {
            let mut j = lock(&self.jobs);
            if j.get(name).is_some_and(|j| !matches!(j.step, "done" | "failed")) {
                return Err(LocalError::new("PLUGIN_BUSY"));
            }
            j.insert(name.to_string(), Job { step: "download", ..Default::default() });
        }
        let (me, name) = (self.clone(), name.to_string());
        tokio::spawn(async move {
            match me.install(&name).await {
                Ok(()) => {
                    me.forget_features();
                    me.set_job(&name, |j| j.step = "done");
                }
                Err(e) => {
                    tracing::warn!(plugin = name, error = ?e, "插件没装上");
                    me.set_job(&name, |j| {
                        j.step = "failed";
                        j.error = Some(json!({ "code": e.code, "params": e.params }));
                    });
                }
            }
        });
        Ok(())
    }

    async fn install(&self, name: &str) -> Result<(), LocalError> {
        let platform = proto::current_platform();
        let r = self.account.plugin_release(name, &platform).await?;
        let signed: proto::SignedRelease = serde_json::from_value(r["signed"].clone()).map_err(|e| err("PLUGIN_INVALID", e))?;
        let raw = looklook_protocol::signing::b64_decode(&signed.release).ok_or_else(|| err("PLUGIN_INVALID", "release"))?;
        let release = proto::verify_release(&self.keys, &raw, &signed.sig).map_err(|e| err("PLUGIN_INVALID", e))?;
        if release.name != name || release.platform != platform {
            return Err(err("PLUGIN_INVALID", "release does not match"));
        }
        Self::compatible(&release.manifest)?;
        let path = r["download_path"].as_str().filter(|p| p.starts_with("/plugins/download/")).ok_or_else(|| err("PLUGIN_INVALID", "download_path"))?;
        let platform_api = self.account.platform();
        let url = format!("{}{path}", platform_api.base);
        let root = self.root();
        std::fs::create_dir_all(&root).map_err(|e| err("PLUGIN_INSTALL", e))?;
        let tmp = root.join(format!(".{name}.download"));
        self.set_job(name, |j| {
            j.step = "download";
            j.total = Some(release.size);
        });
        let jobs = &self.jobs;
        let n = name.to_string();
        let sha = platform_api
            .download(&url, &tmp, release.size.min(PACKAGE_MAX), |done, _| {
                if let Some(j) = lock(jobs).get_mut(&n) {
                    j.done = done;
                }
            })
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                LocalError::from(e)
            })?;
        self.set_job(name, |j| j.step = "verify");
        let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if sha != release.sha256 || size != release.size {
            let _ = std::fs::remove_file(&tmp);
            return Err(err("PLUGIN_INVALID", "package hash mismatch"));
        }
        self.set_job(name, |j| j.step = "install");
        self.put_in_place(name, &raw, &signed.sig, &release, &tmp).await
    }

    /// 校验过的包（`tmp`）解开、写上签过名的 `release.json`，整个换进插件目录；`tmp` 用完删掉
    async fn put_in_place(&self, name: &str, raw: &[u8], sig: &ReleaseSig, release: &Release, tmp: &Path) -> Result<(), LocalError> {
        let root = self.root();
        let staging = root.join(format!(".{name}.new"));
        let (t, s, rel) = (tmp.to_path_buf(), staging.clone(), release.clone());
        let unpacked = tokio::task::spawn_blocking(move || unpack(&t, &s, &rel)).await.map_err(|e| err("PLUGIN_INSTALL", e))?;
        let _ = std::fs::remove_file(tmp);
        if let Err(e) = unpacked {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        std::fs::write(staging.join("release.json"), raw).map_err(|e| err("PLUGIN_INSTALL", e))?;
        std::fs::write(staging.join("release.sig"), serde_json::to_vec(sig).unwrap_or_default()).map_err(|e| err("PLUGIN_INSTALL", e))?;
        // 换之前停掉旧的（下次用到时起新的）
        self.stop(name).await;
        let dir = self.dir(name);
        let old = root.join(format!(".{name}.old"));
        let _ = std::fs::remove_dir_all(&old);
        if dir.exists() {
            std::fs::rename(&dir, &old).map_err(|e| err("PLUGIN_INSTALL", e))?;
        }
        if let Err(e) = std::fs::rename(&staging, &dir) {
            let _ = std::fs::rename(&old, &dir);
            return Err(err("PLUGIN_INSTALL", e));
        }
        let _ = std::fs::remove_dir_all(&old);
        restrict(&dir);
        tracing::info!(plugin = name, version = %release.version, "插件装好了");
        // 原来在后台跑着的（例如浏览器）：按清单接回来
        if self.should_autostart(name) {
            let _ = self.ensure(name).await;
        }
        Ok(())
    }

    /// 卸载程序（数据目录留着，重新装后接着用）
    pub async fn uninstall(&self, name: &str) -> Result<(), LocalError> {
        if !proto::valid_name(name) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        self.stop(name).await;
        let dir = self.dir(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| err("PLUGIN_INSTALL", e))?;
        }
        lock(&self.jobs).remove(name);
        Ok(())
    }

    fn should_autostart(&self, name: &str) -> bool {
        self.installed(name).ok().and_then(|i| i.manifest().autostart_if_exists.clone()).is_some_and(|f| self.data_dir(name).join(f).is_file())
    }

    /// 看看启动后：清单说要接回来的插件马上起
    pub async fn autostart(&self) {
        for n in self.installed_names() {
            if self.should_autostart(&n) {
                match self.ensure(&n).await {
                    Ok(_) => tracing::info!(plugin = n, "插件随看看启动"),
                    Err(e) => tracing::warn!(plugin = n, error = ?e, "插件没能随看看启动"),
                }
            }
        }
    }

    /// 接了这个钩子、能在这个看看上跑的插件
    pub fn with_hook(&self, hook: &str) -> Vec<String> {
        self.installed_names().into_iter().filter(|n| self.installed(n).is_ok_and(|i| i.manifest().has_hook(hook) && Self::compatible(i.manifest()).is_ok())).collect()
    }

    /// 调插件的钩子：`POST /_hooks/<钩子>`，JSON 进 JSON 出。插件没起就先起
    pub async fn hook(&self, name: &str, hook: &str, body: &Value, wait: Duration) -> Result<Value, LocalError> {
        let p = self.ensure(name).await?;
        tokio::time::timeout(wait, hook_call(&p.socket, &p.token, hook, body)).await.map_err(|_| err("PLUGIN_HOOK", "timeout"))?
    }

    /// 插件开着的功能（所有接了 `features` 钩子的插件合起来），缓存 60 秒；问不到的插件算没有
    pub async fn features(&self) -> Vec<String> {
        if let Some((at, f)) = lock(&self.features).clone() {
            if at.elapsed() < FEATURES_TTL {
                return f;
            }
        }
        let mut all = Vec::new();
        for n in self.with_hook(proto::HOOK_FEATURES) {
            match self.hook(&n, proto::HOOK_FEATURES, &json!({}), Duration::from_secs(5)).await {
                Ok(v) => {
                    let f: proto::Features = serde_json::from_value(v).unwrap_or_default();
                    for x in f.features {
                        if !all.contains(&x) {
                            all.push(x);
                        }
                    }
                }
                Err(e) => tracing::debug!(plugin = n, error = ?e, "插件的功能没问到"),
            }
        }
        *lock(&self.features) = Some((Instant::now(), all.clone()));
        all
    }

    pub async fn has_feature(&self, feature: &str) -> bool {
        self.features().await.iter().any(|f| f == feature)
    }

    /// 装、卸、换授权以后：下次重新问
    pub fn forget_features(&self) {
        *lock(&self.features) = None;
    }

    /// 插件在跑（没跑就起）：返回它的 Socket 和口令
    pub async fn ensure(&self, name: &str) -> Result<Arc<Proc>, LocalError> {
        let mut running = self.running.lock().await;
        if let Some(p) = running.get(name).filter(|p| p.alive()) {
            return Ok(p.clone());
        }
        if let Some(old) = running.remove(name) {
            old.kill();
        }
        let inst = self.installed(name)?;
        Self::compatible(inst.manifest())?;
        let p = Arc::new(self.spawn(&inst).await?);
        running.insert(name.to_string(), p.clone());
        Ok(p)
    }

    pub async fn stop(&self, name: &str) {
        if let Some(p) = self.running.lock().await.remove(name) {
            p.kill();
        }
    }

    pub async fn stop_all(&self) {
        for (_, p) in self.running.lock().await.drain() {
            p.kill();
        }
    }

    /// Socket 放在短路径下（Unix Socket 路径有 108 字节的上限）：`$XDG_RUNTIME_DIR/llp-<数据目录摘要>/`，
    /// 没有时 `/tmp/llp-<uid>-<数据目录摘要>/`，目录只有自己能进
    fn socket_dir(&self) -> PathBuf {
        let tag = &looklook_protocol::signing::sha256_hex(self.paths.home.to_string_lossy().as_bytes())[..8];
        match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()) {
            Some(d) => d.join(format!("llp-{tag}")),
            None => std::env::temp_dir().join(format!("llp-{}-{tag}", uid())),
        }
    }

    #[cfg(unix)]
    async fn spawn(&self, inst: &Installed) -> Result<Proc, LocalError> {
        let m = inst.manifest();
        let exe = inst.dir.join(&m.exec);
        if !exe.is_file() {
            return Err(err("PLUGIN_INVALID", format!("{} missing", m.exec)));
        }
        let sdir = self.socket_dir();
        std::fs::create_dir_all(&sdir).map_err(|e| err("PLUGIN_START", e))?;
        restrict(&sdir);
        let socket = sdir.join(format!("{}.sock", m.name));
        let _ = std::fs::remove_file(&socket);
        let data = self.data_dir(&m.name);
        std::fs::create_dir_all(&data).map_err(|e| err("PLUGIN_START", e))?;
        restrict(&data);
        let token = crate::util::random_secret();
        let log_path = self.paths.home.join("logs").join(format!("plugin-{}.log", m.name));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.current_dir(&data)
            .env(proto::ENV_SOCKET, &socket)
            .env(proto::ENV_TOKEN, &token)
            .env(proto::ENV_DATA_DIR, &data)
            .env(proto::ENV_BASE, format!("{}{}/", proto::PAGE_PREFIX, m.name))
            .env(proto::ENV_HOST_TOKEN, &self.host_token)
            .env(proto::ENV_CLIENT_VERSION, env!("CARGO_PKG_VERSION"))
            // 1.86 以前的内置浏览器数据在 `<数据目录>/browser/`：插件第一次起时自己搬过去
            // 插件按它找安装号算机器指纹（[`proto::machine_fingerprint`]），和插件页上显示的一样
            .env(proto::ENV_HOME, &self.paths.home)
            .stdin(std::process::Stdio::null())
            .stdout(log.as_ref().and_then(|f| f.try_clone().ok()).map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null))
            .stderr(log.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null))
            .kill_on_drop(true);
        if let Some(h) = lock(&self.host_url).clone() {
            cmd.env(proto::ENV_HOST_URL, h);
        }
        // 看看退出（包括被强制结束）时插件也退出；插件自己的子进程（浏览器）由插件决定留不留
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| err("PLUGIN_START", format!("{}: {e}", exe.display())))?;
        let proc = Proc { socket: socket.clone(), token, child: Mutex::new(Some(child)) };
        let deadline = Instant::now() + START_WAIT;
        loop {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                tracing::info!(plugin = %m.name, version = %m.version, "插件起来了");
                return Ok(proc);
            }
            if !proc.alive() || Instant::now() > deadline {
                proc.kill();
                let tail = log_tail(&log_path);
                return Err(err("PLUGIN_START", if tail.is_empty() { "plugin did not start".to_string() } else { tail }).with("plugin", m.name.clone()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[cfg(not(unix))]
    async fn spawn(&self, _inst: &Installed) -> Result<Proc, LocalError> {
        Err(LocalError::new("PLUGIN_UNSUPPORTED"))
    }

    /// 宿主接口 `/_host/license`：代插件向服务端申请授权凭证
    pub async fn license(&self, name: &str, req: &proto::LicenseRequest) -> Result<proto::LicenseResponse, LocalError> {
        self.installed(name)?;
        // 有离线授权码、对得上这台机器、没过期：直接给，不用登录、不用联网
        if let Some(token) = self.offline_tokens().remove(name) {
            if let Some(c) = proto::peek_license(&token).filter(|c| c.plugin == name && c.mach == req.machine && c.exp > now_secs()) {
                return Ok(proto::LicenseResponse { token, exp: c.exp, expires_at: rfc3339(c.ent_exp) });
            }
        }
        if self.account.session().is_none() {
            return Err(LocalError::new("NOT_LOGGED_IN"));
        }
        Ok(self.account.plugin_license(name, req).await?)
    }

    /// 宿主接口 `/_host/info`：这台设备、这个看看
    pub fn info(&self) -> Value {
        let s = self.account.session();
        json!({
            "client_version": env!("CARGO_PKG_VERSION"),
            "protocol": proto::PROTOCOL,
            "platform": proto::current_platform(),
            "signed_in": s.is_some(),
            "device_id": s.as_ref().map(|s| s.device_id.clone()),
            "machine": self.machine().ok(),
        })
    }

    // ---------------- 机器指纹、离线授权 ----------------

    /// 本机机器指纹（和插件自己算的一样：系统机器号 + 数据目录里的安装号）
    pub fn machine(&self) -> Result<String, LocalError> {
        self.machine.get_or_init(|| proto::machine_fingerprint(&self.paths.home).map_err(|e| e.to_string())).clone().map_err(|e| err("PLUGIN_MACHINE", e))
    }

    fn platform_keys(&self) -> Vec<(String, VerifyingKey)> {
        let p = self.account.platform();
        p.keys_b64().into_keys().filter_map(|k| Some((k.clone(), p.key(&k)?))).collect()
    }

    fn offline_tokens(&self) -> HashMap<String, String> {
        std::fs::read(self.paths.home.join(OFFLINE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_offline(&self, m: &HashMap<String, String>) -> Result<(), LocalError> {
        let path = self.paths.home.join(OFFLINE_FILE);
        if m.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        crate::util::write_private(&path, &serde_json::to_vec_pretty(m).unwrap_or_default()).map_err(|e| err("PLUGIN_INSTALL", e))
    }

    /// 还能用的离线授权：（插件名，到期 Unix 秒）
    fn offline_valid(&self) -> Vec<(String, i64)> {
        let mach = self.machine().ok();
        let now = now_secs();
        self.offline_tokens()
            .into_iter()
            .filter_map(|(p, t)| proto::peek_license(&t).filter(|c| c.plugin == p && Some(&c.mach) == mach.as_ref() && c.exp > now).map(|c| (p, c.exp)))
            .collect()
    }

    /// 界面上的离线授权列表（不回显凭证）
    fn offline_view(&self) -> Vec<Value> {
        let mach = self.machine().ok();
        let now = now_secs();
        let mut v: Vec<Value> = self
            .offline_tokens()
            .into_iter()
            .map(|(p, t)| {
                let c = proto::peek_license(&t);
                let state = match &c {
                    None => "invalid",
                    Some(c) if Some(&c.mach) != mach.as_ref() => "other_machine",
                    Some(c) if c.exp <= now => "expired",
                    Some(_) => "valid",
                };
                json!({ "plugin": p, "expires_at": c.map(|c| rfc3339(c.exp)), "state": state })
            })
            .collect();
        v.sort_by(|a, b| a["plugin"].as_str().cmp(&b["plugin"].as_str()));
        v
    }

    /// 粘贴离线授权码：每个凭证验签（有平台公钥时）、核对这台机器和到期，都对才存（同一个插件新的换掉旧的）
    pub fn add_offline(&self, code: &str) -> Result<Vec<String>, LocalError> {
        let tokens = proto::split_offline_code(code);
        if tokens.is_empty() || tokens.len() > 20 {
            return Err(LocalError::new("LICENSE_CODE_INVALID"));
        }
        let mach = self.machine()?;
        let keys = self.platform_keys();
        let now = now_secs();
        let mut store = self.offline_tokens();
        let mut added = vec![];
        for t in tokens {
            let c = proto::peek_license(&t).filter(|c| proto::valid_name(&c.plugin)).ok_or_else(|| LocalError::new("LICENSE_CODE_INVALID"))?;
            if c.mach != mach {
                return Err(LocalError::new("LICENSE_CODE_MACHINE").with("plugin", c.plugin));
            }
            if c.exp <= now {
                return Err(LocalError::new("LICENSE_CODE_EXPIRED").with("plugin", c.plugin));
            }
            // 没有平台公钥（没内置、也没登录过的开发构建）时只能交给插件自己验
            if !keys.is_empty() {
                proto::verify_license(&keys, &t, &c.plugin, &mach, now).map_err(|_| LocalError::new("LICENSE_CODE_INVALID"))?;
            }
            added.push(c.plugin.clone());
            store.insert(c.plugin, t);
        }
        self.save_offline(&store)?;
        Ok(added)
    }

    pub fn remove_offline(&self, plugin: &str) -> Result<(), LocalError> {
        let mut store = self.offline_tokens();
        store.remove(plugin);
        self.save_offline(&store)
    }

    // ---------------- 从文件安装 ----------------

    /// 离线安装包的下载地址（要登录、有授权；票据 10 分钟有效），给别的平台的电脑下也行
    pub async fn offline_package_url(&self, name: &str, platform: &str) -> Result<String, LocalError> {
        if !proto::valid_name(name) || !proto::valid_platform(platform) {
            return Err(LocalError::new("PLUGIN_NOT_FOUND"));
        }
        if self.account.session().is_none() {
            return Err(LocalError::new("NOT_LOGGED_IN"));
        }
        let r = self.account.plugin_release(name, platform).await?;
        let path = r["download_path"].as_str().filter(|p| p.starts_with("/plugins/download/")).ok_or_else(|| err("PLUGIN_INVALID", "download_path"))?;
        Ok(format!("{}{path}?offline=1", self.account.platform().base))
    }

    /// 离线安装包先传到这里（插件页用分块上传，远程打开也能传大文件）
    pub fn incoming_dir(&self) -> PathBuf {
        self.root().join(".incoming")
    }

    /// 从离线安装包（`.llplugin`）装：验 `release.json` 的签名、平台、协议版本，包的大小和 SHA-256，再和联网装一样换进去。
    /// 传到 [`Plugins::incoming_dir`] 里的文件装完（不管成没成）就删掉
    pub async fn install_file(&self, path: &Path) -> Result<String, LocalError> {
        if self.keys.is_empty() {
            return Err(LocalError::new("PLUGIN_NO_KEYS"));
        }
        let incoming = self.incoming_dir();
        let cleanup = path.parent().is_some_and(|d| d == incoming);
        let r = self.install_file_inner(path).await;
        if cleanup {
            let _ = std::fs::remove_file(path);
        }
        r
    }

    async fn install_file_inner(&self, path: &Path) -> Result<String, LocalError> {
        let root = self.root();
        std::fs::create_dir_all(&root).map_err(|e| err("PLUGIN_INSTALL", e))?;
        let (src, keys, platform) = (path.to_path_buf(), self.keys.clone(), proto::current_platform());
        let tmp_dir = root.clone();
        // 读开头、验签名，把后面的包拷出来顺便算哈希
        let (raw, sig, release, tmp) = tokio::task::spawn_blocking(move || -> Result<_, LocalError> {
            use sha2::Digest;
            let f = std::fs::File::open(&src).map_err(|e| err("PLUGIN_FILE", e))?;
            let mut r = std::io::BufReader::new(f);
            let (raw, sig) = proto::read_offline_package_head(&mut r).map_err(|e| err("PLUGIN_FILE", e))?;
            let release = proto::verify_release(&keys, &raw, &sig).map_err(|e| err("PLUGIN_INVALID", e))?;
            if release.platform != platform {
                return Err(LocalError::new("PLUGIN_PLATFORM").with("plugin", release.name.clone()).with("platform", release.platform.clone()));
            }
            let tmp = tmp_dir.join(format!(".{}.download", release.name));
            let mut out = std::fs::File::create(&tmp).map_err(|e| err("PLUGIN_INSTALL", e))?;
            let mut h = sha2::Sha256::new();
            let mut buf = vec![0u8; 1 << 16];
            let mut size = 0u64;
            loop {
                let n = std::io::Read::read(&mut r, &mut buf).map_err(|e| err("PLUGIN_FILE", e))?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > release.size.min(PACKAGE_MAX) {
                    break;
                }
                h.update(&buf[..n]);
                std::io::Write::write_all(&mut out, &buf[..n]).map_err(|e| err("PLUGIN_INSTALL", e))?;
            }
            if size != release.size || data_encoding::HEXLOWER.encode(&h.finalize()) != release.sha256 {
                let _ = std::fs::remove_file(&tmp);
                return Err(err("PLUGIN_INVALID", "package hash mismatch"));
            }
            Ok((raw, sig, release, tmp))
        })
        .await
        .map_err(|e| err("PLUGIN_INSTALL", e))??;
        let name = release.name.clone();
        if let Err(e) = Self::compatible(&release.manifest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.put_in_place(&name, &raw, &sig, &release, &tmp).await?;
        Ok(name)
    }

    /// 这个 Socket 是哪个插件的（宿主接口按口令认插件之外，再核对一下名字）
    pub async fn running_names(&self) -> Vec<String> {
        self.running.lock().await.iter().filter(|(_, p)| p.alive()).map(|(n, _)| n.clone()).collect()
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// 只有自己能进（插件程序、数据、Socket 目录）
fn restrict(p: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = p;
}

fn log_tail(p: &Path) -> String {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

/// 把包解到 `dest`：只要普通文件和目录，路径不能跑出去；解完核对 `manifest.json` 和签过名的一致
fn unpack(tgz: &Path, dest: &Path, release: &Release) -> Result<(), LocalError> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| err("PLUGIN_INSTALL", e))?;
    let f = std::fs::File::open(tgz).map_err(|e| err("PLUGIN_INSTALL", e))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for entry in ar.entries().map_err(|e| err("PLUGIN_INVALID", e))? {
        let mut e = entry.map_err(|e| err("PLUGIN_INVALID", e))?;
        let path = e.path().map_err(|e| err("PLUGIN_INVALID", e))?.into_owned();
        let safe = !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, std::path::Component::Normal(_)));
        if !safe {
            return Err(err("PLUGIN_INVALID", format!("bad path {}", path.display())));
        }
        let out = dest.join(&path);
        match e.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&out).map_err(|e| err("PLUGIN_INSTALL", e))?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(d) = out.parent() {
                    std::fs::create_dir_all(d).map_err(|e| err("PLUGIN_INSTALL", e))?;
                }
                let mode = e.header().mode().unwrap_or(0o644);
                let mut w = std::fs::File::create(&out).map_err(|e| err("PLUGIN_INSTALL", e))?;
                std::io::copy(&mut e, &mut w).map_err(|e| err("PLUGIN_INSTALL", e))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o700 } else { 0o600 }));
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
            other => return Err(err("PLUGIN_INVALID", format!("unsupported entry {other:?} {}", path.display()))),
        }
    }
    let m: Manifest = std::fs::read(dest.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| err("PLUGIN_INVALID", "manifest.json"))?;
    if m != release.manifest {
        return Err(err("PLUGIN_INVALID", "manifest.json does not match the signed release"));
    }
    if !dest.join(&m.exec).is_file() {
        return Err(err("PLUGIN_INVALID", format!("{} missing", m.exec)));
    }
    Ok(())
}

/// 经 Unix Socket 发一个钩子请求
#[cfg(unix)]
async fn hook_call(socket: &Path, token: &str, hook: &str, body: &Value) -> Result<Value, LocalError> {
    use http_body_util::BodyExt;
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| err("PLUGIN_HOOK", e))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, axum::body::Body>(hyper_util::rt::TokioIo::new(stream)).await.map_err(|e| err("PLUGIN_HOOK", e))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = axum::http::Request::post(proto::hook_path(hook))
        .header(axum::http::header::HOST, "plugin")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(proto::HDR_TOKEN, token)
        .header(proto::HDR_LOCAL, "1")
        .body(axum::body::Body::from(body.to_string()))
        .map_err(|e| err("PLUGIN_HOOK", e))?;
    let resp = sender.send_request(req).await.map_err(|e| err("PLUGIN_HOOK", e))?;
    let status = resp.status();
    let bytes = http_body_util::Limited::new(resp.into_body(), HOOK_BODY_MAX).collect().await.map_err(|e| err("PLUGIN_HOOK", e))?.to_bytes();
    if !status.is_success() {
        return Err(err("PLUGIN_HOOK", format!("status {status}")));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|e| err("PLUGIN_HOOK", e))
}

#[cfg(not(unix))]
async fn hook_call(_socket: &Path, _token: &str, _hook: &str, _body: &Value) -> Result<Value, LocalError> {
    Err(LocalError::new("PLUGIN_UNSUPPORTED"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::collections::BTreeMap;
    use std::io::Write;

    fn manifest(exec: &str) -> Manifest {
        Manifest {
            name: "demo".into(),
            version: "1.0.0".into(),
            protocol: proto::PROTOCOL,
            min_client: String::new(),
            title: BTreeMap::from([("en-US".into(), "Demo".into())]),
            description: BTreeMap::new(),
            icon: String::new(),
            exec: exec.into(),
            autostart_if_exists: Some("run.json".into()),
            open_url: false,
            hooks: vec![],
        }
    }

    fn tgz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut out = vec![];
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut t = tar::Builder::new(gz);
            for (p, data, mode) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(*mode);
                h.set_cksum();
                t.append_data(&mut h, p, *data).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap().flush().unwrap();
        }
        out
    }

    fn release(m: Manifest) -> Release {
        Release { name: m.name.clone(), version: m.version.clone(), platform: proto::current_platform(), sha256: "0".repeat(64), size: 1, manifest: m, published_at: String::new() }
    }

    #[test]
    fn unpack_checks_paths_and_manifest() {
        let d = tempfile::tempdir().unwrap();
        let m = manifest("bin/demo");
        let mj = serde_json::to_vec(&m).unwrap();
        let good = d.path().join("good.tgz");
        std::fs::write(&good, tgz(&[("manifest.json", &mj, 0o644), ("bin/demo", b"#!/bin/sh\n", 0o755)])).unwrap();
        let dest = d.path().join("out");
        unpack(&good, &dest, &release(m.clone())).unwrap();
        assert!(dest.join("bin/demo").is_file());
        // 清单和签过名的不一样
        let other = Manifest { version: "2.0.0".into(), ..m.clone() };
        assert!(unpack(&good, &dest, &release(other)).is_err());
        // 程序不在包里
        let nobin = d.path().join("nobin.tgz");
        std::fs::write(&nobin, tgz(&[("manifest.json", &mj, 0o644)])).unwrap();
        assert!(unpack(&nobin, &dest, &release(m.clone())).is_err());
        // 路径跑出去：tar 库本身也拒绝写 `..`，这里手写一个头
        let mut raw = vec![];
        {
            let gz = flate2::write::GzEncoder::new(&mut raw, flate2::Compression::fast());
            let mut t = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.as_gnu_mut().unwrap().name[..9].copy_from_slice(b"../evil.x");
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            t.append(&h, &b"x"[..]).unwrap();
            t.into_inner().unwrap().finish().unwrap();
        }
        let evil = d.path().join("evil.tgz");
        std::fs::write(&evil, raw).unwrap();
        assert!(unpack(&evil, &dest, &release(m)).is_err());
        assert!(!d.path().join("evil.x").exists());
    }

    /// 装好的插件要验签：用测试密钥签一个，改了 release.json 就不认
    #[test]
    fn installed_checks_signature() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let account = crate::account::Account::new(store, paths.clone(), "https://looklook.example", "").unwrap();
        let sk = SigningKey::from_bytes(&[3; 32]);
        let keys = format!("pk1={}", looklook_protocol::signing::b64(sk.verifying_key().as_bytes()));
        let p = Plugins::new(paths, account, &keys);
        let dir = p.dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = serde_json::to_vec(&release(manifest("bin/demo"))).unwrap();
        std::fs::write(dir.join("release.json"), &raw).unwrap();
        std::fs::write(dir.join("release.sig"), serde_json::to_vec(&proto::sign_release(&sk, "pk1", &raw)).unwrap()).unwrap();
        assert_eq!(p.installed("demo").unwrap().release.version, "1.0.0");
        assert_eq!(p.installed_names(), ["demo"]);
        let bad = serde_json::to_vec(&release(Manifest { version: "9.0.0".into(), ..manifest("bin/demo") })).unwrap();
        std::fs::write(dir.join("release.json"), bad).unwrap();
        assert_eq!(p.installed("demo").unwrap_err().code, "PLUGIN_INVALID");
        assert_eq!(p.installed("missing").unwrap_err().code, "PLUGIN_NOT_INSTALLED");
        assert_eq!(p.installed("../x").unwrap_err().code, "PLUGIN_NOT_FOUND");
        assert!(p.host_token_ok(&p.host_token.clone()) && !p.host_token_ok("nope"));
        // 要的看看版本比现在高
        assert_eq!(Plugins::compatible(&Manifest { min_client: "99.0.0".into(), ..manifest("x") }).unwrap_err().code, "PLUGIN_NEEDS_CLIENT");
        assert_eq!(Plugins::compatible(&Manifest { protocol: 99, ..manifest("x") }).unwrap_err().code, "PLUGIN_PROTOCOL");
    }

    /// 起一个假插件（shell 脚本 + python 监听 Socket），确认环境变量给到了、能连上
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn spawns_and_stops() {
        if crate::shells::which("python3").is_none() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let account = crate::account::Account::new(store, paths.clone(), "https://looklook.example", "").unwrap();
        let sk = SigningKey::from_bytes(&[3; 32]);
        let p = Plugins::new(paths, account, &format!("pk1={}", looklook_protocol::signing::b64(sk.verifying_key().as_bytes())));
        let dir = p.dir("demo");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let script = "#!/bin/sh\nexec python3 -c 'import os,socket\ns=socket.socket(socket.AF_UNIX)\ns.bind(os.environ[\"LOOKLOOK_PLUGIN_SOCKET\"])\ns.listen(1)\nopen(\"env.txt\",\"w\").write(os.environ[\"LOOKLOOK_PLUGIN_BASE\"])\nimport time\ntime.sleep(60)'\n";
        std::fs::write(dir.join("bin/demo"), script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("bin/demo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let raw = serde_json::to_vec(&release(manifest("bin/demo"))).unwrap();
        std::fs::write(dir.join("release.json"), &raw).unwrap();
        std::fs::write(dir.join("release.sig"), serde_json::to_vec(&proto::sign_release(&sk, "pk1", &raw)).unwrap()).unwrap();
        assert!(!p.should_autostart("demo"));
        let proc = p.ensure("demo").await.unwrap();
        assert!(proc.alive());
        let again = p.ensure("demo").await.unwrap();
        assert!(Arc::ptr_eq(&proc, &again), "在跑就不再起");
        assert_eq!(std::fs::read_to_string(p.data_dir("demo").join("env.txt")).unwrap(), "/p/demo/");
        std::fs::write(p.data_dir("demo").join("run.json"), "{}").unwrap();
        assert!(p.should_autostart("demo"));
        p.stop("demo").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!proc.alive());
    }

    fn host(platform_sk: &SigningKey) -> (tempfile::TempDir, Arc<Plugins>, SigningKey) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let pk = format!("p1={}", looklook_protocol::signing::b64(platform_sk.verifying_key().as_bytes()));
        let account = crate::account::Account::new(store, paths.clone(), "https://looklook.example", &pk).unwrap();
        let sk = SigningKey::from_bytes(&[3; 32]);
        let p = Plugins::new(paths, account, &format!("pk1={}", looklook_protocol::signing::b64(sk.verifying_key().as_bytes())));
        (home, p, sk)
    }

    fn offline_token(sk: &SigningKey, plugin: &str, mach: &str, exp: i64) -> String {
        let c = proto::LicenseClaims { v: 1, plugin: plugin.into(), uid: 0, did: proto::OFFLINE_DID.into(), mach: mach.into(), iat: 0, exp, ent_exp: exp, wm: "w".into() };
        proto::issue_license(sk, "p1", &c)
    }

    /// 离线授权码：验签、核对本机指纹和到期；存下后插件来要凭证时直接给，不用登录
    #[tokio::test]
    async fn offline_license_code() {
        let psk = SigningKey::from_bytes(&[9; 32]);
        let (home, p, sk) = host(&psk);
        let mach = p.machine().unwrap();
        // 和插件按同一个算法自己算的一样
        assert_eq!(proto::machine_fingerprint(home.path()).unwrap(), mach);
        let far = now_secs() + 400 * 86400;
        assert_eq!(p.add_offline("").unwrap_err().code, "LICENSE_CODE_INVALID");
        assert_eq!(p.add_offline("hello").unwrap_err().code, "LICENSE_CODE_INVALID");
        assert_eq!(p.add_offline(&offline_token(&psk, "demo", "ffffffffffffffffffffffffffffffff", far)).unwrap_err().code, "LICENSE_CODE_MACHINE");
        assert_eq!(p.add_offline(&offline_token(&psk, "demo", &mach, now_secs() - 1)).unwrap_err().code, "LICENSE_CODE_EXPIRED");
        // 不是平台私钥签的
        let fake = SigningKey::from_bytes(&[1; 32]);
        assert_eq!(p.add_offline(&offline_token(&fake, "demo", &mach, far)).unwrap_err().code, "LICENSE_CODE_INVALID");
        // 一个码里两个插件
        let code = format!("{}\n{}", offline_token(&psk, "demo", &mach, far), offline_token(&psk, "team", &mach, far));
        assert_eq!(p.add_offline(&code).unwrap(), ["demo", "team"]);
        let list = p.list(false).await;
        assert_eq!(list["machine"], mach.as_str());
        assert_eq!(list["offline"].as_array().unwrap().len(), 2);
        assert_eq!(list["offline"][0]["state"], "valid");
        assert!(list["offline"][0].get("token").is_none(), "不回显凭证");
        // 插件要凭证：装着才给，机器对得上才给（不用登录）
        let req = proto::LicenseRequest { machine: mach.clone(), version: "1".into() };
        assert_eq!(p.license("demo", &req).await.unwrap_err().code, "PLUGIN_NOT_INSTALLED");
        let dir = p.dir("demo");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = serde_json::to_vec(&release(manifest("bin/demo"))).unwrap();
        std::fs::write(dir.join("release.json"), &raw).unwrap();
        std::fs::write(dir.join("release.sig"), serde_json::to_vec(&proto::sign_release(&sk, "pk1", &raw)).unwrap()).unwrap();
        let r = p.license("demo", &req).await.unwrap();
        assert_eq!(r.exp, far);
        assert!(proto::verify_license(&[("p1".into(), psk.verifying_key())], &r.token, "demo", &mach, now_secs()).is_ok());
        let other = proto::LicenseRequest { machine: "x".into(), version: "1".into() };
        assert_eq!(p.license("demo", &other).await.unwrap_err().code, "NOT_LOGGED_IN");
        let list = p.list(false).await;
        let item = list["items"].as_array().unwrap().iter().find(|i| i["name"] == "demo").unwrap().clone();
        assert_eq!(item["licensed"], true);
        assert!(item["offline_until"].is_string());
        // 删掉
        p.remove_offline("demo").unwrap();
        assert_eq!(p.license("demo", &req).await.unwrap_err().code, "NOT_LOGGED_IN");
    }

    /// 离线安装包：验签名、平台、哈希，装好；传进来的文件装完删掉
    #[tokio::test]
    async fn install_from_file() {
        let psk = SigningKey::from_bytes(&[9; 32]);
        let (_home, p, sk) = host(&psk);
        let m = manifest("bin/demo");
        let mj = serde_json::to_vec(&m).unwrap();
        let pkg = tgz(&[("manifest.json", &mj, 0o644), ("bin/demo", b"#!/bin/sh\n", 0o755)]);
        use sha2::Digest;
        let rel = Release { sha256: data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&pkg)), size: pkg.len() as u64, ..release(m.clone()) };
        let raw = serde_json::to_vec(&rel).unwrap();
        let file = |raw: &[u8], sig: &ReleaseSig, body: &[u8]| {
            let mut f = proto::offline_package_head(raw, sig);
            f.extend_from_slice(body);
            f
        };
        let incoming = p.incoming_dir();
        std::fs::create_dir_all(&incoming).unwrap();
        let put = |name: &str, bytes: Vec<u8>| {
            let path = incoming.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        // 不是安装包
        let a = put("a.llplugin", b"PK\x03\x04".to_vec());
        assert_eq!(p.install_file(&a).await.unwrap_err().code, "PLUGIN_FILE");
        assert!(!a.exists(), "装完就删");
        // 别人签的
        let bad = SigningKey::from_bytes(&[4; 32]);
        let b = put("b.llplugin", file(&raw, &proto::sign_release(&bad, "pk1", &raw), &pkg));
        assert_eq!(p.install_file(&b).await.unwrap_err().code, "PLUGIN_INVALID");
        // 包被改过
        let mut tampered = pkg.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        let c = put("c.llplugin", file(&raw, &proto::sign_release(&sk, "pk1", &raw), &tampered));
        assert_eq!(p.install_file(&c).await.unwrap_err().code, "PLUGIN_INVALID");
        // 别的平台
        let other = Release { platform: "plan9-mips".into(), ..rel.clone() };
        let oraw = serde_json::to_vec(&other).unwrap();
        let d = put("d.llplugin", file(&oraw, &proto::sign_release(&sk, "pk1", &oraw), &pkg));
        assert_eq!(p.install_file(&d).await.unwrap_err().code, "PLUGIN_PLATFORM");
        // 对的
        let e = put("e.llplugin", file(&raw, &proto::sign_release(&sk, "pk1", &raw), &pkg));
        assert_eq!(p.install_file(&e).await.unwrap(), "demo");
        assert_eq!(p.installed("demo").unwrap().release.version, "1.0.0");
        assert!(p.dir("demo").join("bin/demo").is_file());
        assert!(!e.exists());
    }
}
