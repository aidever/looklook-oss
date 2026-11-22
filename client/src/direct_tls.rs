//! 公网 HTTPS 直连（服务端 docs/09）：这台电脑自己向 Let's Encrypt 申请公网 IPv4 的证书，管理台在原端口上同时提供 HTTPS
//! （同一个端口按第一个字节区分 TLS 和明文 HTTP，见 `gateway::serve_console`）。浏览器直接打开 `https://<公网IP>:<端口>`，
//! 不经平台和中转，用户可以用 Clash 一条 TCP 规则走自己的加速线路。
//!
//! - **申请**（§4.1）：`shortlived` 配置（160 小时），只有 http-01（80 端口）和 tls-alpn-01（443 端口）两种验证。
//!   只在验证的那几秒里绑 80，绑不上用 443；都绑不上时让用户自己的 Web 服务器把 `/.well-known/acme-challenge/` 转发到
//!   管理台端口（管理台端口一直回应当前待验证的 token，[`Direct::challenge`]）；
//! - **自检**（§6）：登录了就请主服务从外面访问一次，确认端口通了再下单，免得浪费“每小时失败 5 次”的额度；
//!   转发模式先在本机经 127.0.0.1:80 请求一次验证路径，确认用户的 nginx / Caddy 配好了；
//! - **续期**：按 ARI 建议的时间窗里的随机时间续期（取不到时在有效期过半时续），失败按 1、5、30 分钟、2 小时退避；
//!   证书到期前一直失败就停用 HTTPS；公网 IP 变了马上用新 IP 重新申请；
//! - **访问**（§4.3）：HTTPS 上的公网来源是 `Peer::Public`，必须认证：平台签的直连票据（登录后），或者至少 12 位的访问码。
//!   认证后发 Cookie `ll_direct`（由本机密钥派生；关掉直连时换密钥，所有 Cookie 失效）。访问码另有全局失败次数限制。
//!
//! 存在本机数据库：`direct`（开关）、`acme_account`（ACME 账户）、`direct_tls`（证书与私钥）、`direct_secret`（Cookie 密钥）。
//! 测试用的环境变量：`LOOKLOOK_ACME_DIRECTORY`、`LOOKLOOK_ACME_ROOT`（额外信任的根证书 PEM，Pebble 用）、
//! `LOOKLOOK_ACME_PROFILE`（空字符串表示不指定）、`LOOKLOOK_ACME_HTTP_PORT`、`LOOKLOOK_ACME_TLS_PORT`。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::http::{self, header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, CertificateIdentifier, ChallengeType, Identifier, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use looklook_protocol::direct::{self as proto, CheckRequest, DirectInfo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Notify;

use crate::gateway::access::AccessConfig;
use crate::store::Store;

const KEY_CONFIG: &str = "direct";
const KEY_ACCOUNT: &str = "acme_account";
const KEY_CERT: &str = "direct_tls";
const KEY_SECRET: &str = "direct_secret";

pub const COOKIE: &str = "ll_direct";
/// 公网访问码至少这么多个字符（默认生成的访问码是 12 个）。
pub const MIN_PUBLIC_CODE: usize = 12;
/// Let's Encrypt 订户协议（第一次打开时在界面上显示）。
pub const TOS_URL: &str = "https://letsencrypt.org/repository/";

const LE_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
const DEFAULT_PROFILE: &str = "shortlived";
/// 失败后的重试间隔：1、5、30 分钟，之后每 2 小时。
const BACKOFF: [u64; 4] = [60, 300, 1800, 7200];
/// 没有公网 IP、等 DDNS 解析、平时检查 IP 有没有变的间隔。
const POLL: Duration = Duration::from_secs(60);
/// ARI 隔这么久重新问一次（Let's Encrypt 出事故要提前续期时会改窗口）。
const ARI_EVERY: i64 = 6 * 3600;
/// 访问码全局失败限制：一小时内最多错 100 次，超了锁 1 小时（防止从很多 IP 同时猜）。
const GLOBAL_MAX_FAILS: u32 = 100;
const GLOBAL_WINDOW_MS: i64 = 3600 * 1000;
/// 最近的公网登录记多少条（只在内存里，界面显示）。
const MAX_LOGINS: usize = 20;
const CODE_COOKIE_MAX_AGE: i64 = 30 * 86400;

type HmacSha256 = Hmac<Sha256>;
type Pending = Arc<Mutex<HashMap<String, String>>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DirectConfig {
    /// 开启“公网 HTTPS 直连”（默认关闭）
    pub enabled: bool,
}

impl DirectConfig {
    pub fn load(store: &Store) -> Self {
        store.get(KEY_CONFIG).ok().flatten().unwrap_or_default()
    }
}

/// 存在本机的证书。
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    ip: Ipv4Addr,
    /// 证书链（PEM）
    chain: String,
    /// 私钥（PKCS#8 PEM）
    key: String,
    not_before: i64,
    not_after: i64,
    /// 计划续期的时间（Unix 秒）
    renew_at: i64,
    /// 上次问 ARI 的时间
    #[serde(default)]
    ari_at: i64,
    /// 用的哪种验证（界面显示）
    #[serde(default)]
    method: String,
}

#[derive(Serialize, Deserialize)]
struct StoredAccount {
    directory: String,
    creds: AccountCredentials,
}

/// 界面显示的状态。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    /// `off` 没开、`no_ip` 没有公网 IPv4、`working` 正在申请、`ok` 可以用、`failed` 申请失败（还没有可用的证书）
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<Ipv4Addr>,
    /// 直连地址 `https://IP:端口`（有可用证书时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renew_at: Option<i64>,
    /// `http01`、`tls_alpn01`、`forward`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// 最近一次失败的原因：`no_privilege`（Linux 普通用户绑不了 80 / 443）、`port_busy`（80、443 都被占用且没有转发）、
    /// `unreachable`（外面连不到 80 / 443）、`rate_limited`、`acme`（Let's Encrypt 拒绝，见 `detail`）、`network`、`expired`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 下次重试的时间（失败后）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_try: Option<i64>,
    /// 主服务从外面访问 `https://IP:端口/__looklook/ping` 的结果：`ok`、`unknown`（没登录或旧服务端），其余是失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
}

/// 一次公网登录。
#[derive(Debug, Clone, Serialize)]
pub struct Login {
    pub ip: IpAddr,
    pub at: i64,
    /// `code`（访问码）或 `grant`（平台票据）
    pub via: &'static str,
}

/// 访问码全局失败计数。
#[derive(Default)]
struct Global {
    window_start: i64,
    fails: u32,
    locked_until: i64,
}

pub struct Direct {
    store: Arc<Store>,
    /// 管理台端口（直连地址用它）
    port: u16,
    cert: RwLock<Option<Arc<CertifiedKey>>>,
    status: Mutex<Status>,
    /// 当前待验证的 http-01 token → key authorization（管理台端口和临时监听的 80 端口共用）
    pending: Pending,
    wake: Notify,
    secret: RwLock<[u8; 32]>,
    logins: Mutex<VecDeque<Login>>,
    global: Mutex<Global>,
    /// 用过的票据 jti → 过期时间（毫秒）
    jtis: Mutex<HashMap<String, i64>>,
}

/// 心跳里带的直连地址（[`Direct::advertise`] 更新）。
static ADVERTISED: RwLock<Option<DirectInfo>> = RwLock::new(None);

pub fn advertised() -> Option<DirectInfo> {
    ADVERTISED.read().unwrap_or_else(|e| e.into_inner()).clone().filter(|d| d.until > now_s())
}

fn now_s() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl std::fmt::Debug for Direct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Direct").field("port", &self.port).finish_non_exhaustive()
    }
}

impl Direct {
    pub fn new(store: Arc<Store>, port: u16) -> Arc<Self> {
        let secret = match store.get::<String>(KEY_SECRET).ok().flatten().and_then(|h| data_encoding::HEXLOWER.decode(h.as_bytes()).ok()).and_then(|v| <[u8; 32]>::try_from(v).ok()) {
            Some(s) => s,
            None => {
                let s = crate::util::random_bytes::<32>();
                let _ = store.set(KEY_SECRET, &data_encoding::HEXLOWER.encode(&s));
                s
            }
        };
        let d = Arc::new(Self {
            store,
            port,
            cert: RwLock::new(None),
            status: Mutex::new(Status { phase: "off", ..Status::default() }),
            pending: Pending::default(),
            wake: Notify::new(),
            secret: RwLock::new(secret),
            logins: Mutex::new(VecDeque::new()),
            global: Mutex::new(Global::default()),
            jtis: Mutex::new(HashMap::new()),
        });
        // 启动时马上加载存着的证书，不等后台任务
        if DirectConfig::load(&d.store).enabled {
            if let Some(s) = d.stored() {
                if s.not_after > now_s() {
                    d.install(&s);
                }
            }
        }
        d
    }

    pub fn config(&self) -> DirectConfig {
        DirectConfig::load(&self.store)
    }

    /// 开关。关掉时丢掉证书、换 Cookie 密钥（已经登录的公网浏览器全部失效）。
    pub fn set_enabled(&self, on: bool) -> anyhow::Result<()> {
        let mut c = self.config();
        if c.enabled == on {
            return Ok(());
        }
        c.enabled = on;
        self.store.set(KEY_CONFIG, &c)?;
        if !on {
            self.uninstall();
            let s = crate::util::random_bytes::<32>();
            self.store.set(KEY_SECRET, &data_encoding::HEXLOWER.encode(&s))?;
            *self.secret.write().unwrap_or_else(|e| e.into_inner()) = s;
            lock(&self.logins).clear();
        }
        tracing::info!(on, "公网 HTTPS 直连开关");
        self.wake();
        Ok(())
    }

    /// 设置变了（开关、公网地址）：后台马上重新看一遍。
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// 有可用的证书（HTTPS 可以用，公网来源可以算 `Public`）。
    pub fn active(&self) -> bool {
        self.cert.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    pub fn logins(&self) -> Vec<Login> {
        lock(&self.logins).iter().rev().cloned().collect()
    }

    fn set_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut lock(&self.status));
    }

    fn stored(&self) -> Option<Stored> {
        self.store.get::<Stored>(KEY_CERT).ok().flatten()
    }

    /// 装上证书（热替换，不用重启监听）。
    fn install(&self, s: &Stored) -> bool {
        match certified_key(&s.chain, &s.key) {
            Ok(ck) => {
                *self.cert.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ck));
                let url = format!("https://{}:{}", s.ip, self.port);
                *ADVERTISED.write().unwrap_or_else(|e| e.into_inner()) = Some(DirectInfo { url: url.clone(), until: s.not_after });
                self.set_status(|st| {
                    st.phase = "ok";
                    st.ip = Some(s.ip);
                    st.url = Some(url);
                    st.not_after = Some(s.not_after);
                    st.renew_at = Some(s.renew_at);
                    st.method = Some(s.method.clone()).filter(|m| !m.is_empty());
                });
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "存着的直连证书用不了");
                false
            }
        }
    }

    fn uninstall(&self) {
        *self.cert.write().unwrap_or_else(|e| e.into_inner()) = None;
        *ADVERTISED.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn tls_config(self: &Arc<Self>) -> Arc<rustls::ServerConfig> {
        let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(self.clone())));
        // 只用 HTTP/1.1：和明文入口一样（WebSocket、长连接都按现在的方式处理）
        c.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(c)
    }

    // ---------------- 免认证的几个路径 ----------------

    /// `/.well-known/acme-challenge/<token>`：只回应当前待验证的 token，其余 404。
    pub fn challenge(&self, path: &str) -> Response {
        let token = path.strip_prefix(proto::ACME_CHALLENGE_PREFIX).unwrap_or("");
        match lock(&self.pending).get(token) {
            Some(ka) => ([(header::CONTENT_TYPE, "text/plain"), (header::CACHE_CONTROL, "no-store")], ka.clone()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    // ---------------- Cookie ----------------

    fn mac(&self, parts: &[&[u8]]) -> String {
        let key = *self.secret.read().unwrap_or_else(|e| e.into_inner());
        let mut m = <HmacSha256 as Mac>::new_from_slice(&key).expect("hmac key");
        for p in parts {
            m.update(p);
            m.update(b"\0");
        }
        data_encoding::BASE64URL_NOPAD.encode(&m.finalize().into_bytes())
    }

    /// 访问码换来的 Cookie：换了访问码就失效。
    fn code_cookie(&self, cfg: &AccessConfig) -> String {
        format!("c.{}", self.mac(&[b"code", cfg.cookie_value().as_bytes()]))
    }

    /// 票据换来的 Cookie：绑定用户与子站会话，最多活到会话的绝对期限。
    fn grant_cookie(&self, uid: &str, sid: &str, abs_exp_ms: i64) -> String {
        format!("g.{sid}.{abs_exp_ms}.{}", self.mac(&[b"grant", uid.as_bytes(), sid.as_bytes(), abs_exp_ms.to_string().as_bytes()]))
    }

    /// Cookie 还有效吗。`account`：本机登录（票据 Cookie 要求还是同一个用户、会话没被吊销）。
    pub fn cookie_ok(&self, value: Option<&str>, cfg: &AccessConfig, account: &crate::account::Account) -> bool {
        let Some(v) = value else { return false };
        if v.starts_with("c.") {
            return public_code_ok(cfg) && crate::gateway::ct_eq(v, &self.code_cookie(cfg));
        }
        let Some(rest) = v.strip_prefix("g.") else { return false };
        let mut it = rest.splitn(3, '.');
        let (Some(sid), Some(exp), Some(_)) = (it.next(), it.next(), it.next()) else { return false };
        let Ok(exp_ms) = exp.parse::<i64>() else { return false };
        let Some(session) = account.session() else { return false };
        exp_ms > account.clock.now_ms() && !account.is_revoked(sid) && crate::gateway::ct_eq(v, &self.grant_cookie(&session.user.id, sid, exp_ms))
    }

    fn set_cookie(r: &mut Response, value: &str, max_age: i64) {
        let c = format!("{COOKIE}={value}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}", max_age.max(0));
        if let Ok(v) = HeaderValue::from_str(&c) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
    }

    fn note_login(&self, ip: IpAddr, via: &'static str) {
        tracing::info!(%ip, via, "公网直连登录");
        let mut l = lock(&self.logins);
        // 同一来源、同一方式 10 分钟内又来（直连传输断了重连）：只更新时间
        if let Some(last) = l.back_mut().filter(|x| x.ip == ip && x.via == via && now_ms() - x.at < 600_000) {
            last.at = now_ms();
            return;
        }
        l.push_back(Login { ip, at: now_ms(), via });
        while l.len() > MAX_LOGINS {
            l.pop_front();
        }
    }

    // ---------------- 访问码（公网） ----------------

    /// 访问码全局限制：锁着时返回 true。
    pub fn globally_locked(&self) -> bool {
        let g = lock(&self.global);
        g.locked_until > now_ms()
    }

    fn global_fail(&self) {
        let now = now_ms();
        let mut g = lock(&self.global);
        if now - g.window_start > GLOBAL_WINDOW_MS {
            g.window_start = now;
            g.fails = 0;
        }
        g.fails += 1;
        if g.fails >= GLOBAL_MAX_FAILS {
            g.locked_until = now + GLOBAL_WINDOW_MS;
            g.fails = 0;
            g.window_start = now;
            tracing::warn!("公网访问码一小时内错了 {GLOBAL_MAX_FAILS} 次，锁 1 小时");
        }
    }

    /// 公网来源提交访问码。返回 `Err(界面错误名)`：`short`（访问码不够 12 位，不能用于公网）、`too_many`、`wrong`。
    pub fn submit_code(&self, cfg: &AccessConfig, limiter: &crate::gateway::access::Limiter, ip: IpAddr, code: &str, next: &str) -> Result<Response, &'static str> {
        if !public_code_ok(cfg) {
            return Err("short");
        }
        if self.globally_locked() || limiter.blocked(ip) {
            return Err("too_many");
        }
        if !cfg.code_matches(code) {
            limiter.fail(ip);
            self.global_fail();
            tracing::warn!(%ip, "公网访问码错误");
            return Err(if limiter.blocked(ip) || self.globally_locked() { "too_many" } else { "wrong" });
        }
        limiter.clear(ip);
        self.note_login(ip, "code");
        let mut r = axum::response::Redirect::to(next).into_response();
        Self::set_cookie(&mut r, &self.code_cookie(cfg), CODE_COOKIE_MAX_AGE);
        Ok(r)
    }

    // ---------------- 直连票据 ----------------

    /// `POST /__looklook/direct`：校验平台签的票据，通过就发 Cookie。`origin`：这次请求的源（`https://` + Host）。
    pub fn accept_grant(&self, account: &crate::account::Account, ip: IpAddr, origin: &str, token: &str) -> Response {
        let g = match self.check_grant(account, ip, origin, token, "grant") {
            Ok(g) => g,
            Err(why) => return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": why }))).into_response(),
        };
        let Some(session) = account.session() else { return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": "not_logged_in" }))).into_response() };
        let now = account.clock.now_ms();
        let mut r = axum::Json(serde_json::json!({ "ok": true })).into_response();
        Self::set_cookie(&mut r, &self.grant_cookie(&session.user.id, &g.sid, g.abs_exp), (g.abs_exp - now) / 1000);
        r
    }

    /// 校验直连票据（§5）：签名、本机用户与设备、`origin` 是这次请求的源、没过期、没吊销、`jti` 没用过。
    /// 通过时记一次公网登录（`via`：`grant` 打开直连页面，`site` 专属域名下的直连传输 [`crate::rtc::mux`]）。
    pub fn check_grant(&self, account: &crate::account::Account, ip: IpAddr, origin: &str, token: &str, via: &'static str) -> Result<proto::DirectGrant, &'static str> {
        let fail = |why: &'static str| {
            tracing::info!(%ip, why, via, "直连票据没通过");
            Err(why)
        };
        let Some(session) = account.session() else { return fail("not_logged_in") };
        let now = account.clock.now_ms();
        let platform = account.platform();
        let g = match proto::verify_grant(token, |kid| platform.key(kid), &session.user.id, &session.device_id, origin, now) {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!(?e, "直连票据校验失败");
                return fail("grant");
            }
        };
        if account.is_revoked(&g.sid) {
            return fail("revoked");
        }
        if g.abs_exp <= now {
            return fail("session_expired");
        }
        {
            let mut j = lock(&self.jtis);
            j.retain(|_, exp| *exp > now);
            if j.insert(g.jti.clone(), g.exp + proto::LEEWAY_MS).is_some() {
                return fail("replay");
            }
        }
        self.note_login(ip, via);
        Ok(g)
    }

    // ---------------- 后台：申请与续期 ----------------

    /// 后台任务：按开关、公网 IP 与证书有效期申请、续期。
    pub async fn run(self: Arc<Self>, account: Arc<crate::account::Account>) {
        let mut failures: usize = 0;
        let mut retry_at: i64 = 0;
        loop {
            let wait = self.step(&account, &mut failures, &mut retry_at).await;
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.wake.notified() => { retry_at = 0; }
            }
        }
    }

    /// 走一步，返回下次什么时候再看。
    async fn step(self: &Arc<Self>, account: &Arc<crate::account::Account>, failures: &mut usize, retry_at: &mut i64) -> Duration {
        if !self.config().enabled {
            self.uninstall();
            *failures = 0;
            self.set_status(|s| *s = Status { phase: "off", ..Status::default() });
            return Duration::from_secs(3600);
        }
        let Some(ip) = public_v4() else {
            self.uninstall();
            self.set_status(|s| *s = Status { phase: "no_ip", ..Status::default() });
            return POLL;
        };
        let now = now_s();
        let mut stored = self.stored().filter(|s| s.ip == ip);
        // 证书过期了：停用 HTTPS（不继续用过期证书，docs/09 §11 第 2 条）
        if stored.as_ref().is_some_and(|s| s.not_after <= now) {
            stored = None;
        }
        match &stored {
            Some(s) => {
                if !self.active() || lock(&self.status).ip != Some(ip) {
                    self.install(s);
                }
            }
            None => {
                if self.active() {
                    tracing::info!(%ip, "公网 IP 变了或证书过期，停用 HTTPS，重新申请");
                    self.uninstall();
                }
                // 刚停用（或者 IP 换了）：状态从头开始；还在重试的保留上次的失败原因
                self.set_status(|st| {
                    if st.phase == "ok" || st.ip != Some(ip) || matches!(st.phase, "off" | "no_ip") {
                        let expired = st.phase == "ok" && st.ip == Some(ip);
                        *st = Status { phase: "working", ip: Some(ip), error: expired.then(|| "expired".into()), ..Status::default() };
                    }
                });
            }
        }
        // 定期重新问 ARI（只更新计划时间）
        if let Some(s) = stored.as_mut() {
            if now - s.ari_at > ARI_EVERY {
                if let Ok(acct) = self.acme_account().await {
                    if let Some(at) = ari_renew_at(&acct, &s.chain).await {
                        s.renew_at = at;
                    }
                }
                s.ari_at = now;
                let _ = self.store.set(KEY_CERT, &*s);
                self.set_status(|st| st.renew_at = Some(s.renew_at));
            }
        }
        let due = stored.as_ref().map_or(0, |s| s.renew_at);
        let next = due.max(*retry_at);
        if next > now {
            return Duration::from_secs((next - now) as u64).min(POLL);
        }
        self.set_status(|st| {
            if st.phase != "ok" {
                st.phase = "working";
            }
            st.ip = Some(ip);
        });
        match self.issue(account, ip, stored.as_ref()).await {
            Ok(s) => {
                *failures = 0;
                *retry_at = 0;
                let _ = self.store.set(KEY_CERT, &s);
                self.install(&s);
                self.set_status(|st| {
                    st.error = None;
                    st.detail = None;
                    st.next_try = None;
                });
                tracing::info!(%ip, not_after = s.not_after, renew_at = s.renew_at, method = %s.method, "直连证书已更新");
                let me = self.clone();
                let account = account.clone();
                tokio::spawn(async move { me.check_reach(&account, ip).await });
            }
            Err(e) => {
                let wait = BACKOFF[(*failures).min(BACKOFF.len() - 1)] as i64;
                *failures += 1;
                *retry_at = now_s() + wait;
                tracing::warn!(%ip, code = e.code, detail = %e.detail, retry_in = wait, "申请直连证书失败");
                self.set_status(|st| {
                    if !self.active() {
                        st.phase = "failed";
                    }
                    st.error = Some(e.code.to_string());
                    st.detail = Some(e.detail.clone()).filter(|d| !d.is_empty());
                    st.next_try = Some(*retry_at);
                });
                // 证书到期前一直续不上：到期那一刻在上面停用
                if let Some(s) = &stored {
                    *retry_at = (*retry_at).min(s.not_after);
                }
            }
        }
        Duration::from_secs(1)
    }

    /// 申请成功后请主服务从外面访问 `https://IP:端口/__looklook/ping`。
    async fn check_reach(&self, account: &crate::account::Account, ip: Ipv4Addr) {
        let r = account.direct_check(&CheckRequest::Ping { ip: ip.to_string(), port: self.port }).await;
        let reach = match r {
            Some(Ok(c)) if c.ok => "ok".to_string(),
            Some(Ok(c)) => c.error.unwrap_or_else(|| "failed".into()),
            _ => "unknown".to_string(),
        };
        tracing::info!(%ip, reach, "直连自检");
        self.set_status(|st| st.reach = Some(reach));
    }

    async fn acme_account(&self) -> Result<Account, Fail> {
        let directory = std::env::var("LOOKLOOK_ACME_DIRECTORY").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| LE_PRODUCTION.into());
        let http = || Box::new(Http(acme_http())) as Box<dyn instant_acme::HttpClient>;
        if let Some(sa) = self.store.get::<StoredAccount>(KEY_ACCOUNT).ok().flatten().filter(|a| a.directory == directory) {
            return Account::builder_with_http(http()).from_credentials(sa.creds).await.map_err(Fail::acme);
        }
        let (acct, creds) = Account::builder_with_http(http())
            .create(&NewAccount { contact: &[], terms_of_service_agreed: true, only_return_existing: false }, directory.clone(), None)
            .await
            .map_err(Fail::acme)?;
        self.store.set(KEY_ACCOUNT, &StoredAccount { directory, creds }).map_err(|e| Fail::new("internal", e.to_string()))?;
        tracing::info!("已创建 ACME 账户");
        Ok(acct)
    }

    /// 申请一张证书。`old`：同一个 IP 的旧证书（下单时带 `replaces`）。
    async fn issue(&self, account: &crate::account::Account, ip: Ipv4Addr, old: Option<&Stored>) -> Result<Stored, Fail> {
        // 先占好验证用的端口，再自检，最后下单
        // 不管怎么返回，待验证的 token 都清掉
        let _clear = ClearPending(&self.pending);
        let validator = Validator::open(self).await?;
        let method = validator.method();
        self.precheck(account, ip, &validator).await?;
        let acct = self.acme_account().await?;
        let ids = [Identifier::Ip(IpAddr::V4(ip))];
        let profile = std::env::var("LOOKLOOK_ACME_PROFILE").unwrap_or_else(|_| DEFAULT_PROFILE.into());
        let old_id = old.and_then(|s| leaf_der(&s.chain)).and_then(|der| CertificateIdentifier::try_from(&der).ok());
        let order_req = |replaces: Option<CertificateIdentifier<'static>>| {
            let mut o = NewOrder::new(&ids);
            if !profile.is_empty() {
                o = o.profile(&profile);
            }
            if let Some(r) = replaces {
                o = o.replaces(r);
            }
            o
        };
        let mut order = match acct.new_order(&order_req(old_id.clone())).await {
            Ok(o) => o,
            // 服务端不支持 ARI、旧证书已经被替换过等：不带 replaces 再试一次
            Err(_) if old_id.is_some() => acct.new_order(&order_req(None)).await.map_err(Fail::acme)?,
            Err(e) => return Err(Fail::acme(e)),
        };
        let mut authzs = order.authorizations();
        while let Some(a) = authzs.next().await {
            let mut a = a.map_err(Fail::acme)?;
            match a.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                s => return Err(Fail::new("acme", format!("authorization {s:?}"))),
            }
            let kind = validator.challenge_type();
            let mut ch = a.challenge(kind.clone()).ok_or_else(|| Fail::new("acme", format!("no {kind:?} challenge")))?;
            let ka = ch.key_authorization();
            validator.prepare(self, &ch.token, ka.as_str(), ka.digest().as_ref(), ip)?;
            ch.set_ready().await.map_err(Fail::acme)?;
        }
        let status = order.poll_ready(&RetryPolicy::new().timeout(Duration::from_secs(90))).await.map_err(Fail::acme)?;
        lock(&self.pending).clear();
        if status != OrderStatus::Ready {
            // 验证失败的具体原因在 challenge 的 error 里
            let mut detail = order.state().error.as_ref().and_then(|p| p.detail.clone());
            let mut az = order.authorizations();
            while let Some(Ok(a)) = az.next().await {
                if let Some(d) = a.challenges.iter().find_map(|c| c.error.as_ref().and_then(|p| p.detail.clone())) {
                    detail = Some(d);
                }
            }
            return Err(Fail::validation(detail.unwrap_or_else(|| format!("order {status:?}")), method));
        }
        drop(validator);
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| Fail::new("internal", e.to_string()))?;
        let mut params = rcgen::CertificateParams::new(vec![ip.to_string()]).map_err(|e| Fail::new("internal", e.to_string()))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let csr = params.serialize_request(&key).map_err(|e| Fail::new("internal", e.to_string()))?;
        order.finalize_csr(csr.der()).await.map_err(Fail::acme)?;
        let chain = order.poll_certificate(&RetryPolicy::new().timeout(Duration::from_secs(60))).await.map_err(Fail::acme)?;
        let key = key.serialize_pem();
        certified_key(&chain, &key).map_err(|e| Fail::new("acme", format!("bad certificate: {e}")))?;
        let der = leaf_der(&chain).ok_or_else(|| Fail::new("acme", "empty chain"))?;
        let (not_before, not_after) = validity(&der).ok_or_else(|| Fail::new("acme", "bad certificate"))?;
        let renew_at = ari_renew_at(&acct, &chain).await.unwrap_or(not_before + (not_after - not_before) / 2);
        Ok(Stored { ip, chain, key, not_before, not_after, renew_at, ari_at: now_s(), method: method.into() })
    }

    /// 下单前的自检（docs/09 §6）：转发模式先在本机确认 nginx / Caddy 转发配好了；登录了再请主服务从外面看一次。
    async fn precheck(&self, account: &crate::account::Account, ip: Ipv4Addr, v: &Validator) -> Result<(), Fail> {
        let probe = data_encoding::BASE64URL_NOPAD.encode(&crate::util::random_bytes::<16>());
        let expect = format!("{probe}.selfcheck");
        lock(&self.pending).insert(probe.clone(), expect.clone());
        let guard = PendingGuard(self, probe.clone());
        if let Validator::Forward { busy_or_denied } = v {
            if !local_forward_ok(ip, &probe, &expect).await {
                return Err(Fail::new(if *busy_or_denied == BindFail::Denied { "no_privilege" } else { "port_busy" }, ""));
            }
        }
        let req = match v {
            Validator::Http { .. } | Validator::Forward { .. } => CheckRequest::Challenge { ip: ip.to_string(), port: 80, token: probe.clone(), expect },
            Validator::Tls { .. } => CheckRequest::Tcp { ip: ip.to_string(), port: 443 },
        };
        let r = account.direct_check(&req).await;
        drop(guard);
        match r {
            Some(Ok(c)) if !c.ok => Err(Fail::new("unreachable", c.error.unwrap_or_default())),
            // 没登录、旧服务端（404）、主服务暂时连不上：不挡着，直接下单
            _ => Ok(()),
        }
    }
}

/// 访问码够长，可以用于公网。
pub fn public_code_ok(cfg: &AccessConfig) -> bool {
    cfg.code.chars().filter(|c| !c.is_whitespace() && *c != '-').count() >= MIN_PUBLIC_CODE
}

/// 现在用的公网 IPv4：填的地址（IP，或 DDNS 域名最近一次的解析结果）。
fn public_v4() -> Option<Ipv4Addr> {
    let c = crate::rtc::net::current();
    match crate::rtc::net::public_ip(c.public_v4.as_deref()?, false)? {
        IpAddr::V4(v) => Some(v),
        _ => None,
    }
}

struct ClearPending<'a>(&'a Pending);

impl Drop for ClearPending<'_> {
    fn drop(&mut self) {
        lock(self.0).clear();
    }
}

struct PendingGuard<'a>(&'a Direct, String);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.0.pending).remove(&self.1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindFail {
    /// 被占用
    Busy,
    /// 没有权限（Linux 普通用户）
    Denied,
}

/// 验证用的端口：申请的那几秒里临时绑 80 或 443；都绑不上时经管理台端口（用户自己转发）。
enum Validator {
    Http { stop: tokio_util::sync::CancellationToken },
    Tls { stop: tokio_util::sync::CancellationToken, cert: Arc<RwLock<Option<Arc<CertifiedKey>>>> },
    Forward { busy_or_denied: BindFail },
}

impl Drop for Validator {
    fn drop(&mut self) {
        match self {
            Validator::Http { stop } | Validator::Tls { stop, .. } => stop.cancel(),
            Validator::Forward { .. } => {}
        }
    }
}

fn port_env(name: &str, default: u16) -> u16 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn bind_fail(e: &std::io::Error) -> BindFail {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        BindFail::Denied
    } else {
        BindFail::Busy
    }
}

impl Validator {
    async fn open(d: &Direct) -> Result<Self, Fail> {
        let http_port = port_env("LOOKLOOK_ACME_HTTP_PORT", 80);
        let tls_port = port_env("LOOKLOOK_ACME_TLS_PORT", 443);
        let e80 = match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], http_port))).await {
            Ok(l) => {
                let stop = tokio_util::sync::CancellationToken::new();
                tokio::spawn(http01_server(l, d.pending.clone(), stop.clone()));
                d.set_status(|s| s.method = Some("http01".into()));
                return Ok(Validator::Http { stop });
            }
            Err(e) => e,
        };
        let e443 = match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], tls_port))).await {
            Ok(l) => {
                let stop = tokio_util::sync::CancellationToken::new();
                let cert = Arc::new(RwLock::new(None));
                tokio::spawn(tls_alpn_server(l, cert.clone(), stop.clone()));
                d.set_status(|s| s.method = Some("tls_alpn01".into()));
                return Ok(Validator::Tls { stop, cert });
            }
            Err(e) => e,
        };
        tracing::info!(port80 = %e80, port443 = %e443, "80、443 都绑不上，经管理台端口验证（需要用户转发）");
        d.set_status(|s| s.method = Some("forward".into()));
        let both_denied = bind_fail(&e80) == BindFail::Denied && bind_fail(&e443) == BindFail::Denied;
        Ok(Validator::Forward { busy_or_denied: if both_denied { BindFail::Denied } else { BindFail::Busy } })
    }

    fn challenge_type(&self) -> ChallengeType {
        match self {
            Validator::Tls { .. } => ChallengeType::TlsAlpn01,
            _ => ChallengeType::Http01,
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Validator::Http { .. } => "http01",
            Validator::Tls { .. } => "tls_alpn01",
            Validator::Forward { .. } => "forward",
        }
    }

    /// 准备好验证的回应。
    fn prepare(&self, d: &Direct, token: &str, key_auth: &str, digest: &[u8], ip: Ipv4Addr) -> Result<(), Fail> {
        match self {
            Validator::Tls { cert, .. } => {
                let ck = alpn_cert(ip, digest).map_err(|e| Fail::new("internal", e))?;
                *cert.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ck));
            }
            _ => {
                lock(&d.pending).insert(token.to_string(), key_auth.to_string());
            }
        }
        Ok(())
    }
}

/// http-01：临时监听 80，只回应 `/.well-known/acme-challenge/<token>`。
async fn http01_server(l: tokio::net::TcpListener, pending: Pending, stop: tokio_util::sync::CancellationToken) {
    let app = axum::Router::new().fallback(move |uri: http::Uri| async move {
        let token = uri.path().strip_prefix(proto::ACME_CHALLENGE_PREFIX).unwrap_or("");
        let ka = lock(&pending).get(token).cloned();
        match ka {
            Some(ka) => ([(header::CONTENT_TYPE, "text/plain")], ka).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    });
    let _ = axum::serve(l, app).with_graceful_shutdown(async move { stop.cancelled().await }).await;
}

/// tls-alpn-01：临时监听 443，ALPN `acme-tls/1` 握手时出示带 acmeIdentifier 扩展的自签证书，握手完就断开。
async fn tls_alpn_server(l: tokio::net::TcpListener, cert: Arc<RwLock<Option<Arc<CertifiedKey>>>>, stop: tokio_util::sync::CancellationToken) {
    #[derive(Debug)]
    struct One(Arc<RwLock<Option<Arc<CertifiedKey>>>>);
    impl ResolvesServerCert for One {
        fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }
    let mut c = match rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions() {
        Ok(b) => b.with_no_client_auth().with_cert_resolver(Arc::new(One(cert))),
        Err(_) => return,
    };
    c.alpn_protocols = vec![b"acme-tls/1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(c));
    loop {
        let (s, _) = tokio::select! {
            r = l.accept() => match r { Ok(x) => x, Err(_) => continue },
            _ = stop.cancelled() => return,
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(s)).await;
        });
    }
}

/// tls-alpn-01 的自签证书（RFC 8737 / 8738）：SAN 是这个 IP，带 critical 的 acmeIdentifier 扩展。
fn alpn_cert(ip: Ipv4Addr, digest: &[u8]) -> Result<CertifiedKey, String> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(vec![ip.to_string()]).map_err(|e| e.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    certified_key(&cert.pem(), &key.serialize_pem())
}

/// 转发模式的本机自检：经 127.0.0.1:80 用这个 IP 当 Host 请求验证路径，能拿到回应说明 nginx / Caddy 的转发配好了。
async fn local_forward_ok(ip: Ipv4Addr, token: &str, expect: &str) -> bool {
    let port = port_env("LOOKLOOK_ACME_HTTP_PORT", 80);
    let Ok(client) = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::limited(3)).build() else { return false };
    let url = format!("http://127.0.0.1:{port}{}{token}", proto::ACME_CHALLENGE_PREFIX);
    let host = if port == 80 { ip.to_string() } else { format!("{ip}:{port}") };
    match client.get(&url).header(header::HOST, host).send().await {
        Ok(r) if r.status().is_success() => r.text().await.is_ok_and(|t| t.trim() == expect),
        _ => false,
    }
}

#[derive(Debug)]
struct Resolver(Arc<Direct>);

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.0.cert.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn certified_key(chain_pem: &str, key_pem: &str) -> Result<CertifiedKey, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem.as_bytes()).collect::<Result<_, _>>().map_err(|e| format!("{e:?}"))?;
    if certs.is_empty() {
        return Err("empty chain".into());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|e| format!("{e:?}"))?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| e.to_string())?;
    Ok(CertifiedKey::new(certs, signer))
}

fn leaf_der(chain_pem: &str) -> Option<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(chain_pem.as_bytes()).next()?.ok()
}

/// 证书的 (notBefore, notAfter)，Unix 秒。
fn validity(der: &CertificateDer<'_>) -> Option<(i64, i64)> {
    let (_, c) = x509_parser::parse_x509_certificate(der.as_ref()).ok()?;
    Some((c.validity().not_before.timestamp(), c.validity().not_after.timestamp()))
}

/// 按 ARI 的建议窗口取一个随机的续期时间。
async fn ari_renew_at(acct: &Account, chain_pem: &str) -> Option<i64> {
    let der = leaf_der(chain_pem)?;
    let id = CertificateIdentifier::try_from(&der).ok()?;
    let (info, _) = acct.renewal_info(&id).await.ok()?;
    let (start, end) = (info.suggested_window.start.unix_timestamp(), info.suggested_window.end.unix_timestamp());
    if end <= start {
        return Some(start);
    }
    use rand::Rng;
    Some(rand::thread_rng().gen_range(start..end))
}

// ---------------- 错误 ----------------

#[derive(Debug)]
struct Fail {
    code: &'static str,
    detail: String,
}

impl Fail {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }

    fn acme(e: instant_acme::Error) -> Self {
        match &e {
            instant_acme::Error::Api(p) if p.r#type.as_deref().is_some_and(|t| t.ends_with(":rateLimited")) => Self::new("rate_limited", p.detail.clone().unwrap_or_default()),
            instant_acme::Error::Api(p) => Self::new("acme", p.detail.clone().unwrap_or_else(|| e.to_string())),
            instant_acme::Error::Other(_) | instant_acme::Error::Timeout(_) => Self::new("network", e.to_string()),
            _ => Self::new("acme", e.to_string()),
        }
    }

    /// 验证没通过：多半是外面连不到验证端口。
    fn validation(detail: String, method: &str) -> Self {
        let lower = detail.to_ascii_lowercase();
        let unreachable = ["connection", "timeout", "refused", "fetching"].iter().any(|k| lower.contains(k));
        Self::new(if unreachable && method != "forward" { "unreachable" } else { "acme" }, detail)
    }
}

// ---------------- ACME 的 HTTP 客户端（reqwest，ring） ----------------

fn acme_http() -> reqwest::Client {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(30)).user_agent(concat!("looklook/", env!("CARGO_PKG_VERSION")));
    if let Some(path) = std::env::var("LOOKLOOK_ACME_ROOT").ok().filter(|s| !s.is_empty()) {
        match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|pem| reqwest::Certificate::from_pem(&pem).map_err(|e| e.to_string())) {
            Ok(c) => b = b.add_root_certificate(c),
            Err(e) => tracing::warn!(path, error = %e, "LOOKLOOK_ACME_ROOT 读不了"),
        }
    }
    b.build().unwrap_or_default()
}

struct Http(reqwest::Client);

impl instant_acme::HttpClient for Http {
    fn request(&self, req: http::Request<BodyWrapper<Bytes>>) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move {
            use http_body_util::BodyExt;
            let other = |e: Box<dyn std::error::Error + Send + Sync>| instant_acme::Error::Other(e);
            let (parts, body) = req.into_parts();
            let body = body.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
            let r = client.request(parts.method, parts.uri.to_string()).headers(parts.headers).body(body).send().await.map_err(|e| other(Box::new(e)))?;
            let status = r.status();
            let headers: HeaderMap = r.headers().clone();
            let bytes = r.bytes().await.map_err(|e| other(Box::new(e)))?;
            let mut resp = http::Response::new(());
            *resp.status_mut() = status;
            *resp.headers_mut() = headers;
            let (parts, _) = resp.into_parts();
            Ok(BytesResponse { parts, body: Box::new(bytes) })
        })
    }
}

// ---------------- 页面 ----------------

/// `GET /__looklook/direct`：从平台跳过来的页面，把 `#` 后面的票据交给电脑，然后进管理台。
pub fn grant_page() -> Response {
    const PAGE: &str = r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>看看</title>
<style>body{font:15px/1.6 system-ui,sans-serif;margin:0;display:flex;min-height:100vh;align-items:center;justify-content:center;background:#f6f7f9;color:#1f2328}@media(prefers-color-scheme:dark){body{background:#16181c;color:#e6e6e6}}p{max-width:28em;padding:0 16px;text-align:center}</style></head>
<body><p id="m">正在打开…</p><script>
(function(){var t=location.hash.slice(1);history.replaceState(null,'',location.pathname);var m=document.getElementById('m');
if(!t){m.textContent='链接不完整，请回到看看网站重新点“直接打开”。';return}
fetch(location.pathname,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({grant:t}),credentials:'same-origin'})
.then(function(r){if(r.ok){location.replace('/');return}return r.json().catch(function(){return{}}).then(function(j){m.textContent=(j.error==='not_logged_in'?'这台电脑已经退出登录，请在电脑上登录看看，或者用访问码打开。':'链接已失效，请回到看看网站重新点“直接打开”。')+(j.error?' ('+j.error+')':'')})})
.catch(function(){m.textContent='连不上这台电脑，请稍后再试。'})})();
</script></body></html>"#;
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        PAGE,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(Store::open(&dir.path().join("t.db")).unwrap());
        (dir, s)
    }

    #[test]
    fn public_code_length() {
        let cfg = |code: &str| AccessConfig { code: code.into(), ..AccessConfig::default() };
        assert!(public_code_ok(&cfg("K7MQ-2XRA-9WTD")));
        assert!(!public_code_ok(&cfg("short-code")));
        assert!(public_code_ok(&AccessConfig::default()), "默认生成的访问码可以用于公网");
    }

    #[test]
    fn challenge_only_answers_pending_tokens() {
        let (_d, s) = store();
        let d = Direct::new(s, 1234);
        assert_eq!(d.challenge("/.well-known/acme-challenge/abc").status(), StatusCode::NOT_FOUND);
        lock(&d.pending).insert("abc".into(), "abc.thumb".into());
        assert_eq!(d.challenge("/.well-known/acme-challenge/abc").status(), StatusCode::OK);
        assert_eq!(d.challenge("/.well-known/acme-challenge/abd").status(), StatusCode::NOT_FOUND);
        assert_eq!(d.challenge("/.well-known/acme-challenge/").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn code_cookie_follows_code_and_secret() {
        let (_d, s) = store();
        let d = Direct::new(s, 1234);
        let a = AccessConfig { code: "K7MQ-2XRA-9WTD".into(), ..AccessConfig::default() };
        let b = AccessConfig { code: "K7MQ-2XRA-9WTE".into(), ..AccessConfig::default() };
        assert_ne!(d.code_cookie(&a), d.code_cookie(&b));
        let before = d.code_cookie(&a);
        d.set_enabled(true).unwrap();
        d.set_enabled(false).unwrap();
        assert_ne!(before, d.code_cookie(&a), "关掉直连后换密钥，旧 Cookie 全部失效");
    }

    #[test]
    fn global_limit_locks_after_many_failures() {
        let (_d, s) = store();
        let d = Direct::new(s, 1234);
        let cfg = AccessConfig { code: "K7MQ-2XRA-9WTD".into(), ..AccessConfig::default() };
        let limiter = crate::gateway::access::Limiter::default();
        for i in 0..GLOBAL_MAX_FAILS {
            // 每次换一个 IP（按 IP 的限制挡不住）
            let ip = IpAddr::from([203, 0, (i / 250) as u8, (i % 250) as u8 + 1]);
            assert!(d.submit_code(&cfg, &limiter, ip, "WRONG-WRONG-WRONG", "/").is_err());
        }
        assert!(d.globally_locked());
        let ip = IpAddr::from([198, 51, 100, 1]);
        assert_eq!(d.submit_code(&cfg, &limiter, ip, "K7MQ-2XRA-9WTD", "/").err(), Some("too_many"), "锁着时对的访问码也不行");
        let short = AccessConfig { code: "abcdefgh".into(), ..AccessConfig::default() };
        assert_eq!(d.submit_code(&short, &limiter, ip, "abcdefgh", "/").err(), Some("short"));
    }

    #[test]
    fn right_code_sets_secure_cookie() {
        let (_d, s) = store();
        let d = Direct::new(s, 1234);
        let cfg = AccessConfig { code: "K7MQ-2XRA-9WTD".into(), ..AccessConfig::default() };
        let limiter = crate::gateway::access::Limiter::default();
        let r = d.submit_code(&cfg, &limiter, IpAddr::from([198, 51, 100, 1]), "k7mq 2xra 9wtd", "/x").unwrap();
        let c = r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
        assert!(c.starts_with("ll_direct=c.") && c.contains("Secure") && c.contains("HttpOnly") && c.contains("SameSite=Strict"), "{c}");
        assert_eq!(r.headers().get(header::LOCATION).unwrap(), "/x");
        assert_eq!(d.logins().len(), 1);
    }

    /// 用 Pebble（Let's Encrypt 的测试 ACME 服务器）跑完整的申请流程，手动运行：
    /// `pebble -config pebble-config.json`（httpPort 5002、tlsPort 5001），然后
    /// `LOOKLOOK_ACME_DIRECTORY=https://127.0.0.1:14000/dir LOOKLOOK_ACME_ROOT=pebble.minica.pem LOOKLOOK_ACME_HTTP_PORT=5002 LOOKLOOK_ACME_TLS_PORT=5001 cargo test pebble -- --ignored --test-threads=1`
    /// 先占住 5002 再跑一次就走 tls-alpn-01。
    #[tokio::test]
    #[ignore]
    async fn pebble_issue() {
        let (dir, s) = store();
        let paths = crate::paths::Paths { home: dir.path().into(), ttyd: None, mux: None, termd: None, fonts: None, trzsz: None };
        let account = crate::account::Account::new(s.clone(), paths, "https://looklook.test", "").unwrap();
        let d = Direct::new(s.clone(), 1234);
        let ip = Ipv4Addr::LOCALHOST;
        let block = std::env::var("PEBBLE_BLOCK_HTTP").is_ok();
        let _held = if block { Some(std::net::TcpListener::bind(("0.0.0.0", port_env("LOOKLOOK_ACME_HTTP_PORT", 80))).unwrap()) } else { None };
        let st = d.issue(&account, ip, None).await.unwrap_or_else(|e| panic!("{} {}", e.code, e.detail));
        assert_eq!(st.method, if block { "tls_alpn01" } else { "http01" });
        assert!(st.not_after > now_s() && st.renew_at > st.not_before && st.renew_at < st.not_after, "{} {} {}", st.not_before, st.renew_at, st.not_after);
        assert!(d.install(&st) && d.active());
        // 续期：带上 replaces
        let again = d.issue(&account, ip, Some(&st)).await.unwrap_or_else(|e| panic!("{} {}", e.code, e.detail));
        assert_ne!(again.chain, st.chain);
        // 同一个端口：TLS 和明文 HTTP 都能用
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let app = axum::Router::new().route("/", axum::routing::get(|axum::extract::ConnectInfo(c): axum::extract::ConnectInfo<crate::gateway::Conn>| async move { if c.tls { "tls" } else { "plain" } }));
        tokio::spawn(crate::gateway::serve_console(l, app, d.clone(), tokio_util::sync::CancellationToken::new()));
        let plain = reqwest::get(format!("http://{addr}/")).await.unwrap().text().await.unwrap();
        assert_eq!(plain, "plain");
        // Pebble 的证书链根是每次启动随机生成的：只验证握手，不校验信任链
        let c = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
        let tls = c.get(format!("https://{addr}/")).send().await.unwrap().text().await.unwrap();
        assert_eq!(tls, "tls");
    }

    #[test]
    fn self_signed_alpn_cert_and_validity() {
        let ck = alpn_cert(Ipv4Addr::new(203, 0, 113, 7), &[7u8; 32]).unwrap();
        let (nb, na) = validity(&ck.cert[0]).unwrap();
        assert!(na > nb);
        let (_, c) = x509_parser::parse_x509_certificate(ck.cert[0].as_ref()).unwrap();
        let ext = c.extensions().iter().find(|e| e.oid.to_id_string() == "1.3.6.1.5.5.7.1.31").expect("acmeIdentifier");
        assert!(ext.critical);
    }
}
