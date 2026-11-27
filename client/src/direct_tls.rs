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

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use axum::http::{self, header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, CertificateIdentifier, ChallengeType, Identifier, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use looklook_protocol::direct::{self as proto, CheckRequest, DirectEndpoint, DirectInfo, EndpointKind};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::gateway::access::AccessConfig;
use crate::store::Store;

const KEY_CONFIG: &str = "direct";
const KEY_ACCOUNT: &str = "acme_account";
const KEY_CERT: &str = "direct_tls";
const KEY_SELF_CERT: &str = "direct_self_tls";
const KEY_SECRET: &str = "direct_secret";

pub const COOKIE: &str = "ll_direct";
/// 公网访问码至少这么多个字符（默认生成的访问码是 12 个）。
pub const MIN_PUBLIC_CODE: usize = 12;
/// 最多配置多少条穿透 / DDNS / 端口映射地址。
pub const MAX_CUSTOM_URLS: usize = 16;
/// Let's Encrypt 订户协议（第一次打开时在界面上显示）。
pub const TOS_URL: &str = "https://letsencrypt.org/repository/";

const LE_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
const DEFAULT_PROFILE: &str = "shortlived";
/// 失败后的重试间隔：1、5、30 分钟，之后每 2 小时。
const BACKOFF: [u64; 4] = [60, 300, 1800, 7200];
/// 没有公网 IP、等 DDNS 解析、平时检查 IP 或自定义证书文件有没有变的间隔。
const POLL: Duration = Duration::from_secs(60);
/// ARI 隔这么久重新问一次（Let's Encrypt 出事故要提前续期时会改窗口）。
const ARI_EVERY: i64 = 6 * 3600;
/// 访问码全局失败限制：一小时内最多错 100 次，超了锁 1 小时（防止从很多 IP 同时猜）。
const GLOBAL_MAX_FAILS: u32 = 100;
const GLOBAL_WINDOW_MS: i64 = 3600 * 1000;
/// 最近的公网登录记多少条（只在内存里，界面显示）。
const MAX_LOGINS: usize = 20;
const CODE_COOKIE_MAX_AGE: i64 = 30 * 86400;
/// 同一张 `mux` 票据允许在多候选并发竞速窗口内握手几次。
const MUX_JTI_RACE_MS: i64 = 15_000;
const MUX_JTI_MAX_USES: u8 = 8;

type HmacSha256 = Hmac<Sha256>;
type Pending = Arc<Mutex<HashMap<String, String>>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DirectConfig {
    /// 开启“公网直连（IPv6 / IPv4）”（默认关闭）
    pub enabled: bool,
    /// 用户添加的内网穿透 / 反向代理 / DDNS / 端口映射地址
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_urls: Vec<String>,
    /// 高级选项：自定义 TLS 证书链 PEM 路径
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tls_cert_path: String,
    /// 高级选项：自定义 TLS 私钥 PEM 路径
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tls_key_path: String,
}

impl DirectConfig {
    pub fn load(store: &Store) -> Self {
        store.get(KEY_CONFIG).ok().flatten().unwrap_or_default()
    }
}

/// 存在本机的 ACME 证书（兼容旧版 `Ipv4Addr` JSON 字符串与新版 `IpAddr`）。
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    ip: IpAddr,
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

/// 本机持久自签 TLS 证书（无公网受信任证书时兜底，供桌面端与手机端通过 `ctl hello` 签名绑定安全直连）。
#[derive(Clone, Serialize, Deserialize)]
struct SelfStored {
    chain: String,
    key: String,
    not_after: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CertSan {
    Dns(String),
    Ip(IpAddr),
}

#[derive(Clone, Default)]
struct ActiveTls {
    /// 是否为浏览器受信任的证书（ACME 或用户提供的有效 PEM）
    web_tls: bool,
    not_after: i64,
    /// 叶子证书 DER 的 SHA-256 小写 hex（64 位）
    sha256_hex: String,
    sans: Vec<CertSan>,
}

#[derive(Serialize, Deserialize)]
struct StoredAccount {
    directory: String,
    creds: AccountCredentials,
}

/// 界面显示的状态。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    /// `off` 没开、`no_ip` 没有公网地址、`working` 正在申请证书、`ok` 有浏览器受信任证书、`failed` 仅桌面端/手机端自签直连可用（浏览器回退 P2P）
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpAddr>,
    /// 直连地址 `https://IP:端口`（有可用证书时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renew_at: Option<i64>,
    /// `http01`、`tls_alpn01`、`forward`、`custom`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// 最近一次失败的原因
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
    /// `code`（访问码）或 `grant`（平台票据）或 `site`（复用直连）
    pub via: &'static str,
}

/// 访问码全局失败计数。
#[derive(Default)]
struct Global {
    window_start: i64,
    fails: u32,
    locked_until: i64,
}

#[derive(Clone, Copy)]
struct JtiUse {
    first_ms: i64,
    uses: u8,
    exp_ms: i64,
}

pub struct Direct {
    store: Arc<Store>,
    /// 管理台端口（直连地址用它）
    port: u16,
    self_cert: Arc<CertifiedKey>,
    self_sha256: String,
    cert: RwLock<Option<Arc<CertifiedKey>>>,
    tls_meta: RwLock<ActiveTls>,
    status: Mutex<Status>,
    /// 当前待验证的 http-01 token → key authorization（管理台端口和临时监听的 80 端口共用）
    pending: Pending,
    wake: Notify,
    secret: RwLock<[u8; 32]>,
    logins: Mutex<VecDeque<Login>>,
    global: Mutex<Global>,
    /// 用过的票据 jti → 使用记录
    jtis: Mutex<HashMap<String, JtiUse>>,
}

static GLOBAL_DIRECT: RwLock<Option<Weak<Direct>>> = RwLock::new(None);
/// 心跳里带的直连地址兜底缓存。
static ADVERTISED: RwLock<Option<DirectInfo>> = RwLock::new(None);

pub fn local_port() -> Option<u16> {
    let weak = GLOBAL_DIRECT.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    weak.upgrade().map(|d| d.port).filter(|p| *p > 0)
}

pub fn advertised_endpoints() -> Vec<DirectEndpoint> {
    let Some(weak) = GLOBAL_DIRECT.read().unwrap_or_else(|e| e.into_inner()).clone() else { return Vec::new() };
    let Some(d) = weak.upgrade() else { return Vec::new() };
    let access = AccessConfig::load(&d.store);
    d.endpoints(&access)
}

pub fn advertised() -> Option<DirectInfo> {
    if let Some(weak) = GLOBAL_DIRECT.read().unwrap_or_else(|e| e.into_inner()).clone() {
        if let Some(d) = weak.upgrade() {
            let access = AccessConfig::load(&d.store);
            if let Some(info) = d.direct_info(&access) {
                return Some(info);
            }
        }
    }
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

/// 解析并规范化一条用户填写的穿透 / 反代 / DDNS / 端口映射地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCustomUrl {
    /// 规范化后的展示/存储格式
    pub canonical: String,
    /// Host 主机名（小写，IPv6 不带方括号）
    pub host: String,
    /// 对应的 `https://` 源（若用户显式填了 `http://` 则为 `None`）
    pub https_origin: Option<String>,
    pub kind: EndpointKind,
    pub explicit_https: bool,
    pub explicit_http: bool,
}

const KNOWN_HTTPS_TUNNEL_SUFFIXES: &[&str] = &[
    ".trycloudflare.com",
    ".cfargotunnel.com",
    ".ts.net",
    ".ngrok-free.app",
    ".ngrok.app",
    ".ngrok.io",
];

pub fn parse_custom_url(raw: &str, default_port: u16) -> Result<ParsedCustomUrl, &'static str> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 240 {
        return Err("url_format");
    }
    let (explicit_https, explicit_http, rest) = if let Some(r) = s.strip_prefix("https://").or_else(|| s.strip_prefix("HTTPS://")) {
        (true, false, r)
    } else if let Some(r) = s.strip_prefix("http://").or_else(|| s.strip_prefix("HTTP://")) {
        (false, true, r)
    } else if s.contains("://") {
        return Err("url_format");
    } else {
        (false, false, s)
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.bytes().any(|b| matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\') || b <= b' ' || b >= 0x7f) {
        return Err("url_format");
    }

    // 解析 (host, port_opt, is_v6)
    let (host, port_opt, is_v6) = if let Some(after_b) = rest.strip_prefix('[') {
        let Some((v6_s, tail)) = after_b.split_once(']') else { return Err("url_format") };
        if v6_s.contains('%') {
            return Err("url_format");
        }
        let v6: Ipv6Addr = v6_s.parse().map_err(|_| "url_format")?;
        let p = if tail.is_empty() {
            None
        } else {
            let ps = tail.strip_prefix(':').ok_or("url_format")?;
            let pn: u16 = ps.parse().ok().filter(|n| *n > 0 && !ps.starts_with('0')).ok_or("url_format")?;
            Some(pn)
        };
        (v6.to_string(), p, true)
    } else if rest.matches(':').count() > 1 {
        // 裸 IPv6（没写方括号也没带端口）
        let v6: Ipv6Addr = rest.parse().map_err(|_| "url_format")?;
        (v6.to_string(), None, true)
    } else if let Some((h, ps)) = rest.split_once(':') {
        let pn: u16 = ps.parse().ok().filter(|n| *n > 0 && !ps.starts_with('0')).ok_or("url_format")?;
        (h.trim_end_matches('.').to_ascii_lowercase(), Some(pn), false)
    } else {
        (rest.trim_end_matches('.').to_ascii_lowercase(), None, false)
    };

    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err("host_unusable");
    }

    let kind = if is_v6 {
        let v6: Ipv6Addr = host.parse().map_err(|_| "url_format")?;
        let seg0 = v6.segments()[0];
        if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (seg0 & 0xffc0) == 0xfe80 || v6.to_ipv4_mapped().is_some() {
            return Err("host_unusable");
        }
        if (seg0 & 0xfe00) == 0xfc00 {
            EndpointKind::Lan
        } else if (seg0 & 0xe000) == 0x2000 && !(seg0 == 0x2001 && v6.segments()[1] == 0x0db8) {
            EndpointKind::Ipv6
        } else {
            return Err("host_unusable");
        }
    } else if let Ok(v4) = host.parse::<Ipv4Addr>() {
        let o = v4.octets();
        if v4.is_loopback() || v4.is_unspecified() || v4.is_link_local() || v4.is_multicast() || v4.is_broadcast() || o[0] == 0 || o[0] >= 240 || (o[0] == 198 && (o[1] & 0xfe) == 18) {
            return Err("host_unusable");
        }
        if v4.is_private() || (o[0] == 100 && (64..128).contains(&o[1])) {
            EndpointKind::Lan
        } else {
            EndpointKind::Public
        }
    } else {
        // 域名
        if crate::settings::parse_open_host(&host).is_err() || !host.contains('.') {
            return Err("url_format");
        }
        if explicit_https || explicit_http || (port_opt.is_none() && KNOWN_HTTPS_TUNNEL_SUFFIXES.iter().any(|suf| host.ends_with(suf))) {
            EndpointKind::Tunnel
        } else {
            EndpointKind::Ddns
        }
    };

    let auto_https_tunnel = !explicit_https && !explicit_http && kind == EndpointKind::Tunnel && port_opt.is_none();
    let explicit_https = explicit_https || auto_https_tunnel;
    let host_lit = if is_v6 { format!("[{host}]") } else { host.clone() };

    let (canonical, https_origin) = if explicit_http {
        let authority = match port_opt {
            Some(80) | None => host_lit,
            Some(p) => format!("{host_lit}:{p}"),
        };
        (format!("http://{authority}"), None)
    } else if explicit_https {
        let authority = match port_opt {
            Some(443) | None => host_lit,
            Some(p) => format!("{host_lit}:{p}"),
        };
        let u = format!("https://{authority}");
        if !proto::valid_https_origin(&u, kind == EndpointKind::Lan) {
            return Err("url_format");
        }
        (u.clone(), Some(u))
    } else {
        let p = port_opt.unwrap_or(default_port.max(1));
        let authority = format!("{host_lit}:{p}");
        let u = format!("https://{authority}");
        if !proto::valid_https_origin(&u, kind == EndpointKind::Lan) {
            return Err("url_format");
        }
        (authority, Some(u))
    };

    Ok(ParsedCustomUrl {
        canonical,
        host,
        https_origin,
        kind,
        explicit_https,
        explicit_http,
    })
}

fn ensure_self_cert(store: &Store) -> (Arc<CertifiedKey>, String) {
    let now = now_s();
    if let Some(s) = store.get::<SelfStored>(KEY_SELF_CERT).ok().flatten().filter(|s| s.not_after > now + 30 * 86400) {
        if let Ok(ck) = certified_key(&s.chain, &s.key) {
            let fp = cert_sha256_hex(&ck.cert[0]);
            return (Arc::new(ck), fp);
        }
    }
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("ecdsa keypair");
    let mut params = rcgen::CertificateParams::new(vec!["looklook.internal".to_string()]).expect("cert params");
    params.distinguished_name = rcgen::DistinguishedName::new();
    let now_dt = time::OffsetDateTime::now_utc();
    params.not_before = now_dt - time::Duration::days(1);
    params.not_after = now_dt + time::Duration::days(3650);
    let cert = params.self_signed(&key).expect("self signed cert");
    let (chain, key_pem) = (cert.pem(), key.serialize_pem());
    let not_after = (now_dt + time::Duration::days(3650)).unix_timestamp();
    let _ = store.set(KEY_SELF_CERT, &SelfStored { chain: chain.clone(), key: key_pem.clone(), not_after });
    let ck = certified_key(&chain, &key_pem).expect("certified key");
    let fp = cert_sha256_hex(&ck.cert[0]);
    (Arc::new(ck), fp)
}

fn cert_sha256_hex(der: &CertificateDer<'_>) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(der.as_ref()))
}

fn extract_sans(der: &CertificateDer<'_>) -> Vec<CertSan> {
    let Ok((_, c)) = x509_parser::parse_x509_certificate(der.as_ref()) else { return Vec::new() };
    let mut out = Vec::new();
    for ext in c.extensions() {
        if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for gn in &san.general_names {
                match gn {
                    x509_parser::extensions::GeneralName::DNSName(d) => out.push(CertSan::Dns(d.trim_end_matches('.').to_ascii_lowercase())),
                    x509_parser::extensions::GeneralName::IPAddress(b) if b.len() == 4 => {
                        out.push(CertSan::Ip(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]))));
                    }
                    x509_parser::extensions::GeneralName::IPAddress(b) if b.len() == 16 => {
                        if let Ok(arr) = <[u8; 16]>::try_from(*b) {
                            out.push(CertSan::Ip(IpAddr::V6(Ipv6Addr::from(arr))));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

fn san_matches_host(san: &CertSan, host: &str) -> bool {
    let h = host.trim_matches(['[', ']']).trim_end_matches('.').to_ascii_lowercase();
    if let Ok(ip) = h.parse::<IpAddr>() {
        return matches!(san, CertSan::Ip(s_ip) if s_ip.to_canonical() == ip.to_canonical());
    }
    let CertSan::Dns(pattern) = san else { return false };
    if let Some(rest) = pattern.strip_prefix("*.") {
        // 通配符只匹配单级子域名
        let Some((sub, tail)) = h.split_once('.') else { return false };
        !sub.is_empty() && tail == rest
    } else {
        *pattern == h
    }
}

fn load_custom_pem(cert_path: &str, key_path: &str) -> Result<(CertifiedKey, i64, String, Vec<CertSan>), &'static str> {
    let chain = std::fs::read_to_string(cert_path).map_err(|_| "cert_read")?;
    let key = std::fs::read_to_string(key_path).map_err(|_| "key_read")?;
    let ck = certified_key(&chain, &key).map_err(|_| "cert_invalid")?;
    let (_, not_after) = validity(&ck.cert[0]).ok_or("cert_invalid")?;
    if not_after <= now_s() {
        return Err("cert_expired");
    }
    let fp = cert_sha256_hex(&ck.cert[0]);
    let sans = extract_sans(&ck.cert[0]);
    Ok((ck, not_after, fp, sans))
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
        let (self_cert, self_sha256) = ensure_self_cert(&store);
        let init_meta = ActiveTls { web_tls: false, not_after: 0, sha256_hex: self_sha256.clone(), sans: Vec::new() };
        let d = Arc::new(Self {
            store,
            port,
            cert: RwLock::new(Some(self_cert.clone())),
            self_cert,
            self_sha256,
            tls_meta: RwLock::new(init_meta),
            status: Mutex::new(Status { phase: "off", ..Status::default() }),
            pending: Pending::default(),
            wake: Notify::new(),
            secret: RwLock::new(secret),
            logins: Mutex::new(VecDeque::new()),
            global: Mutex::new(Global::default()),
            jtis: Mutex::new(HashMap::new()),
        });
        let cfg = DirectConfig::load(&d.store);
        if !cfg.tls_cert_path.is_empty() && !cfg.tls_key_path.is_empty() {
            let _ = d.reload_custom_tls(&cfg.tls_cert_path, &cfg.tls_key_path);
        } else if cfg.enabled {
            if let Some(s) = d.stored() {
                if s.not_after > now_s() {
                    d.install(&s);
                }
            }
        }
        *GLOBAL_DIRECT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&d));
        d
    }

    pub fn config(&self) -> DirectConfig {
        DirectConfig::load(&self.store)
    }

    /// 是否允许来自公网的访问（开启了公网直连，或配置了外部穿透/加速地址）。
    pub fn accepts_public(&self) -> bool {
        let c = self.config();
        c.enabled || !c.custom_urls.is_empty()
    }

    /// 当前正在使用的 TLS 叶子证书 SHA-256（小写 hex，64 字符）。
    pub fn leaf_sha256(&self) -> String {
        self.tls_meta.read().unwrap_or_else(|e| e.into_inner()).sha256_hex.clone()
    }

    fn rotate_cookie_secret(&self) -> anyhow::Result<()> {
        let s = crate::util::random_bytes::<32>();
        self.store.set(KEY_SECRET, &data_encoding::HEXLOWER.encode(&s))?;
        *self.secret.write().unwrap_or_else(|e| e.into_inner()) = s;
        lock(&self.logins).clear();
        Ok(())
    }

    /// 开关“公网直连”。关掉且无自定义穿透地址时换 Cookie 密钥（已经登录的公网浏览器全部失效）。
    pub fn set_enabled(&self, on: bool) -> anyhow::Result<()> {
        let mut c = self.config();
        if c.enabled == on {
            return Ok(());
        }
        c.enabled = on;
        self.store.set(KEY_CONFIG, &c)?;
        if !on {
            if c.tls_cert_path.is_empty() || c.tls_key_path.is_empty() {
                self.uninstall();
            }
            if c.custom_urls.is_empty() {
                self.rotate_cookie_secret()?;
            }
        }
        tracing::info!(on, "公网直连开关");
        self.wake();
        Ok(())
    }

    /// 更新用户填写的穿透 / 反代 / DDNS / 端口映射地址列表。
    pub fn set_custom_urls(&self, raw_list: &[String]) -> Result<Vec<String>, (&'static str, String)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for item in raw_list {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed = parse_custom_url(trimmed, self.port).map_err(|rule| (rule, trimmed.to_string()))?;
            if seen.insert(parsed.canonical.clone()) {
                out.push(parsed.canonical);
            }
        }
        if out.len() > MAX_CUSTOM_URLS {
            return Err(("url_too_many", String::new()));
        }
        let mut c = self.config();
        let was_non_empty = !c.custom_urls.is_empty();
        c.custom_urls = out.clone();
        self.store.set(KEY_CONFIG, &c).map_err(|_| ("internal", String::new()))?;
        if was_non_empty && out.is_empty() && !c.enabled {
            let _ = self.rotate_cookie_secret();
        }
        self.wake();
        Ok(out)
    }

    /// 设置或清除高级自定义 TLS 证书 PEM 文件路径。
    pub fn set_custom_tls(&self, cert_path: &str, key_path: &str) -> Result<(), &'static str> {
        let (cp, kp) = (cert_path.trim(), key_path.trim());
        if cp.is_empty() != kp.is_empty() {
            return Err("cert_pair");
        }
        if !cp.is_empty() {
            self.reload_custom_tls(cp, kp)?;
        }
        let mut c = self.config();
        c.tls_cert_path = cp.to_string();
        c.tls_key_path = kp.to_string();
        self.store.set(KEY_CONFIG, &c).map_err(|_| "internal")?;
        if cp.is_empty() {
            self.uninstall();
            self.set_status(|st| {
                if !self.config().enabled {
                    *st = Status { phase: "off", ..Status::default() };
                }
            });
        }
        self.wake();
        Ok(())
    }

    fn reload_custom_tls(&self, cert_path: &str, key_path: &str) -> Result<(), &'static str> {
        let (ck, not_after, sha256_hex, sans) = load_custom_pem(cert_path, key_path)?;
        let current_fp = self.leaf_sha256();
        if current_fp == sha256_hex && self.active() {
            return Ok(());
        }
        *self.cert.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ck));
        *self.tls_meta.write().unwrap_or_else(|e| e.into_inner()) = ActiveTls {
            web_tls: true,
            not_after,
            sha256_hex,
            sans,
        };
        self.set_status(|st| {
            st.phase = "ok";
            st.not_after = Some(not_after);
            st.renew_at = None;
            st.method = Some("custom".into());
            st.error = None;
            st.detail = None;
            st.next_try = None;
        });
        Ok(())
    }

    /// 当前配置的自定义穿透/DDNS 主机名列表（小写，供 `local_guard` Host 白名单使用）。
    pub fn custom_hosts(&self) -> Vec<String> {
        self.config()
            .custom_urls
            .iter()
            .filter_map(|u| parse_custom_url(u, self.port).ok().map(|p| p.host))
            .collect()
    }

    /// 匹配请求 `Host` 是否命中某条自定义穿透/公网地址。
    pub fn match_custom_host(&self, req_host: &str) -> Option<ParsedCustomUrl> {
        let h = req_host.trim_matches(['[', ']']).trim_end_matches('.');
        self.config()
            .custom_urls
            .iter()
            .filter_map(|u| parse_custom_url(u, self.port).ok())
            .find(|p| p.host.eq_ignore_ascii_case(h))
    }

    /// 判断本机当前安装的受信任证书是否覆盖这个主机或 IP，返回 `(web_tls, until)`。
    fn cert_covers(&self, host: &str) -> (bool, i64) {
        let meta = self.tls_meta.read().unwrap_or_else(|e| e.into_inner());
        let now = now_s();
        if meta.web_tls && meta.not_after > now && meta.sans.iter().any(|san| san_matches_host(san, host)) {
            (true, meta.not_after)
        } else {
            (false, 0)
        }
    }

    /// 生成当前全部直连可达端点（按优先级：局域网/组网 → 穿透与自定义地址 → 公网 IPv6 → 公网 IPv4 / DDNS）。
    pub fn endpoints(&self, access: &AccessConfig) -> Vec<DirectEndpoint> {
        let cfg = self.config();
        let net = crate::rtc::net::current();
        let port = self.port;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |ep: DirectEndpoint| {
            if out.len() < MAX_CUSTOM_URLS && ep.valid_url() && seen.insert(ep.url.clone()) {
                out.push(ep);
            }
        };

        // 1. 局域网 / Tailscale / ZeroTier 直连
        if access.allow_lan {
            for ip in crate::gateway::access::own_lan_ips().into_iter().take(6) {
                let host_lit = match ip {
                    IpAddr::V4(v4) => v4.to_string(),
                    IpAddr::V6(v6) => format!("[{v6}]"),
                };
                let (web_tls, until) = self.cert_covers(&ip.to_string());
                push(DirectEndpoint {
                    url: format!("https://{host_lit}:{port}"),
                    kind: EndpointKind::Lan,
                    web_tls,
                    until,
                });
            }
        }

        // 2. 用户配置的穿透 / 反代 / DDNS / 端口映射地址
        for raw in &cfg.custom_urls {
            let Ok(p) = parse_custom_url(raw, port) else { continue };
            let Some(url) = p.https_origin else { continue };
            let (web_tls, until) = if p.explicit_https {
                (true, 0)
            } else {
                self.cert_covers(&p.host)
            };
            push(DirectEndpoint { url, kind: p.kind, web_tls, until });
        }

        // 3. 公网直连（IPv6 / IPv4 / RTC 网络里填的公网地址或 DDNS）
        if cfg.enabled {
            if net.ipv6 {
                if let Some(s) = net.public_v6.as_deref() {
                    if let Ok(crate::rtc::net::Addr::Name(domain)) = crate::rtc::net::parse_addr(s, true) {
                        let (web_tls, until) = self.cert_covers(&domain);
                        push(DirectEndpoint { url: format!("https://{domain}:{port}"), kind: EndpointKind::Ddns, web_tls, until });
                    }
                    if let Some(IpAddr::V6(v6)) = crate::rtc::net::public_ip(s, true) {
                        let (web_tls, until) = self.cert_covers(&v6.to_string());
                        push(DirectEndpoint { url: format!("https://[{v6}]:{port}"), kind: EndpointKind::Ipv6, web_tls, until });
                    }
                }
                for v6 in crate::rtc::net::nic_v6().into_iter().take(2) {
                    let (web_tls, until) = self.cert_covers(&v6.to_string());
                    push(DirectEndpoint { url: format!("https://[{v6}]:{port}"), kind: EndpointKind::Ipv6, web_tls, until });
                }
            }
            if let Some(s) = net.public_v4.as_deref() {
                if let Ok(crate::rtc::net::Addr::Name(domain)) = crate::rtc::net::parse_addr(s, false) {
                    let (web_tls, until) = self.cert_covers(&domain);
                    push(DirectEndpoint { url: format!("https://{domain}:{port}"), kind: EndpointKind::Ddns, web_tls, until });
                }
            }
            if let Some(v4) = public_v4() {
                let (web_tls, until) = self.cert_covers(&v4.to_string());
                push(DirectEndpoint { url: format!("https://{v4}:{port}"), kind: EndpointKind::Public, web_tls, until });
            }
        }

        out
    }

    /// 供平台设备列表“直接打开”按钮跳转的最优外部受信任 HTTPS 地址。
    pub fn direct_info(&self, access: &AccessConfig) -> Option<DirectInfo> {
        let eps = self.endpoints(access);
        let rank = |k: EndpointKind| match k {
            EndpointKind::Tunnel => 0,
            EndpointKind::Ddns => 1,
            EndpointKind::Public => 2,
            EndpointKind::Ipv6 => 3,
            EndpointKind::Lan => 99,
        };
        let best = eps.into_iter().filter(|e| e.kind != EndpointKind::Lan && e.web_tls).min_by_key(|e| rank(e.kind))?;
        let now = now_s();
        Some(DirectInfo {
            url: best.url,
            until: if best.until > now { best.until } else { now + 2 * 86400 },
        })
    }

    /// 设置变了（开关、公网地址、自定义证书）：后台马上重新看一遍。
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// 当前是否装有浏览器受信任的证书（ACME 或用户自定义 PEM）。
    /// 注意：即使为 `false`，本机持久自签证书也始终在位，供桌面端与手机端安全直连。
    pub fn active(&self) -> bool {
        self.tls_meta.read().unwrap_or_else(|e| e.into_inner()).web_tls
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

    /// 装上受信任的 ACME 证书（热替换，不用重启监听）。
    fn install(&self, s: &Stored) -> bool {
        match certified_key(&s.chain, &s.key) {
            Ok(ck) => {
                let sha256_hex = cert_sha256_hex(&ck.cert[0]);
                let sans = extract_sans(&ck.cert[0]);
                *self.cert.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(ck));
                *self.tls_meta.write().unwrap_or_else(|e| e.into_inner()) = ActiveTls {
                    web_tls: true,
                    not_after: s.not_after,
                    sha256_hex,
                    sans,
                };
                let host_lit = match s.ip {
                    IpAddr::V4(v4) => v4.to_string(),
                    IpAddr::V6(v6) => format!("[{v6}]"),
                };
                let url = format!("https://{host_lit}:{}", self.port);
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

    /// 退回到本机持久自签 TLS 证书（桌面端和手机端仍可经设备公钥签名校验直连）。
    fn uninstall(&self) {
        *self.cert.write().unwrap_or_else(|e| e.into_inner()) = Some(self.self_cert.clone());
        *self.tls_meta.write().unwrap_or_else(|e| e.into_inner()) = ActiveTls {
            web_tls: false,
            not_after: 0,
            sha256_hex: self.self_sha256.clone(),
            sans: Vec::new(),
        };
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

    fn set_cookie(r: &mut Response, value: &str, max_age: i64, secure: bool) {
        let sec = if secure { " Secure;" } else { "" };
        let c = format!("{COOKIE}={value}; HttpOnly;{sec} SameSite=Strict; Path=/; Max-Age={}", max_age.max(0));
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

    /// 公网来源提交访问码（默认发 `Secure` Cookie）。
    #[cfg(test)]
    pub fn submit_code(&self, cfg: &AccessConfig, limiter: &crate::gateway::access::Limiter, ip: IpAddr, code: &str, next: &str) -> Result<Response, &'static str> {
        self.submit_code_ext(cfg, limiter, ip, code, next, true)
    }

    /// 公网 / 穿透来源提交访问码。`secure_cookie`：仅当用户显式填了 `http://` 明文穿透地址时为 `false`。
    pub fn submit_code_ext(
        &self,
        cfg: &AccessConfig,
        limiter: &crate::gateway::access::Limiter,
        ip: IpAddr,
        code: &str,
        next: &str,
        secure_cookie: bool,
    ) -> Result<Response, &'static str> {
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
        Self::set_cookie(&mut r, &self.code_cookie(cfg), CODE_COOKIE_MAX_AGE, secure_cookie);
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
        Self::set_cookie(&mut r, &self.grant_cookie(&session.user.id, &g.sid, g.abs_exp), (g.abs_exp - now) / 1000, true);
        r
    }

    /// 校验直连票据：
    /// - `via == "grant"`（打开直连页面设长效 Cookie）：严格要求 `g.origin == origin`，且 `jti` 严格一次性；
    /// - `via == "site"`（`/__looklook/mux` WebSocket）：允许设备级空 `g.origin`，且允许同一 `jti` 在 15 秒竞速窗口内并发探测多条候选路径。
    pub fn check_grant(&self, account: &crate::account::Account, ip: IpAddr, origin: &str, token: &str, via: &'static str) -> Result<proto::DirectGrant, &'static str> {
        let fail = |why: &'static str| {
            tracing::info!(%ip, why, via, "直连票据没通过");
            Err(why)
        };
        let Some(session) = account.session() else { return fail("not_logged_in") };
        let now = account.clock.now_ms();
        let platform = account.platform();
        let verified = if via == "site" {
            proto::verify_mux_grant(token, |kid| platform.key(kid), &session.user.id, &session.device_id, origin, now)
        } else {
            proto::verify_grant(token, |kid| platform.key(kid), &session.user.id, &session.device_id, origin, now)
        };
        let g = match verified {
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
            j.retain(|_, u| u.exp_ms > now);
            let max_uses = if via == "site" { MUX_JTI_MAX_USES } else { 1 };
            match j.get_mut(&g.jti) {
                Some(u) => {
                    if via != "site" || u.uses >= max_uses || now.saturating_sub(u.first_ms) > MUX_JTI_RACE_MS {
                        return fail("replay");
                    }
                    u.uses += 1;
                }
                None => {
                    j.insert(g.jti.clone(), JtiUse { first_ms: now, uses: 1, exp_ms: g.exp + proto::LEEWAY_MS });
                }
            }
        }
        if !ip.is_loopback() {
            self.note_login(ip, via);
        }
        Ok(g)
    }

    // ---------------- 后台：申请与续期 ----------------

    /// 后台任务：按开关、公网 IP（IPv4 或 IPv6）、自定义证书文件与证书有效期申请、续期。
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
        let cfg = self.config();
        // 1. 用户指定了自定义证书 PEM 文件：定期检查文件是否更新（如 acme.sh / Lucky 自动续期），不再跑 ACME
        if !cfg.tls_cert_path.is_empty() && !cfg.tls_key_path.is_empty() {
            let prev_fp = self.leaf_sha256();
            match self.reload_custom_tls(&cfg.tls_cert_path, &cfg.tls_key_path) {
                Ok(()) => {
                    if self.leaf_sha256() != prev_fp {
                        tracing::info!("已热加载更新后的自定义 TLS 证书");
                        account.wake();
                    }
                }
                Err(code) => {
                    tracing::warn!(code, "自定义 TLS 证书读取失败或已过期，回退自签证书");
                    self.uninstall();
                    self.set_status(|st| {
                        st.phase = "failed";
                        st.method = Some("custom".into());
                        st.error = Some(code.into());
                    });
                }
            }
            return POLL;
        }

        if !cfg.enabled {
            self.uninstall();
            *failures = 0;
            self.set_status(|s| *s = Status { phase: "off", ..Status::default() });
            return Duration::from_secs(3600);
        }
        let Some(ip) = acme_target_ip() else {
            self.uninstall();
            self.set_status(|s| *s = Status { phase: "no_ip", ..Status::default() });
            return POLL;
        };
        let now = now_s();
        let mut stored = self.stored().filter(|s| s.ip == ip);
        if stored.as_ref().is_some_and(|s| s.not_after <= now) {
            stored = None;
        }
        match &stored {
            Some(s) => {
                if !self.active() || lock(&self.status).ip != Some(ip) {
                    self.install(s);
                    account.wake();
                }
            }
            None => {
                if self.active() {
                    tracing::info!(%ip, "公网地址变了或证书过期，回退自签证书并重新申请");
                    self.uninstall();
                }
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
                account.wake();
                tracing::info!(%ip, not_after = s.not_after, renew_at = s.renew_at, method = %s.method, "直连证书已更新");
                let me = self.clone();
                let account = account.clone();
                tokio::spawn(async move { me.check_reach(&account, ip).await });
            }
            Err(e) => {
                let wait = BACKOFF[(*failures).min(BACKOFF.len() - 1)] as i64;
                *failures += 1;
                *retry_at = now_s() + wait;
                tracing::info!(%ip, code = e.code, detail = %e.detail, retry_in = wait, "未获取到浏览器公网证书，保持桌面端/手机端自签直连与 P2P 可用");
                self.set_status(|st| {
                    if !self.active() {
                        st.phase = "failed";
                    }
                    st.error = Some(e.code.to_string());
                    st.detail = Some(e.detail.clone()).filter(|d| !d.is_empty());
                    st.next_try = Some(*retry_at);
                });
                if let Some(s) = &stored {
                    *retry_at = (*retry_at).min(s.not_after);
                }
            }
        }
        Duration::from_secs(1)
    }

    /// 申请成功后请主服务从外面访问 `https://IP:端口/__looklook/ping`。
    async fn check_reach(&self, account: &crate::account::Account, ip: IpAddr) {
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

    /// 申请一张证书（支持公网 IPv4 与公网 IPv6）。`old`：同一个 IP 的旧证书（下单时带 `replaces`）。
    async fn issue(&self, account: &crate::account::Account, ip: IpAddr, old: Option<&Stored>) -> Result<Stored, Fail> {
        let _clear = ClearPending(&self.pending);
        let validator = Validator::open(self, ip.is_ipv6()).await?;
        let method = validator.method();
        self.precheck(account, ip, &validator).await?;
        let acct = self.acme_account().await?;
        let ids = [Identifier::Ip(ip)];
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

    /// 下单前的自检：转发模式先在本机确认 nginx / Caddy 转发配好了；登录了再请主服务从外面看一次。
    async fn precheck(&self, account: &crate::account::Account, ip: IpAddr, v: &Validator) -> Result<(), Fail> {
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

/// ACME 自动申请证书的目标公网 IP：优先配置的公网 IPv4，其次公网 IPv6（配置值或本机网卡 GUA）。
fn acme_target_ip() -> Option<IpAddr> {
    if let Some(v4) = public_v4() {
        return Some(IpAddr::V4(v4));
    }
    let c = crate::rtc::net::current();
    if !c.ipv6 {
        return None;
    }
    if let Some(s) = c.public_v6.as_deref() {
        if let Some(IpAddr::V6(v6)) = crate::rtc::net::public_ip(s, true) {
            return Some(IpAddr::V6(v6));
        }
    }
    crate::rtc::net::nic_v6().into_iter().next().map(IpAddr::V6)
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
    async fn open(d: &Direct, ipv6: bool) -> Result<Self, Fail> {
        let http_port = port_env("LOOKLOOK_ACME_HTTP_PORT", 80);
        let tls_port = port_env("LOOKLOOK_ACME_TLS_PORT", 443);
        let bind_addr = |port: u16| if ipv6 { SocketAddr::from(([0u16; 8], port)) } else { SocketAddr::from(([0, 0, 0, 0], port)) };
        let e80 = match tokio::net::TcpListener::bind(bind_addr(http_port)).await {
            Ok(l) => {
                let stop = tokio_util::sync::CancellationToken::new();
                tokio::spawn(http01_server(l, d.pending.clone(), stop.clone()));
                d.set_status(|s| s.method = Some("http01".into()));
                return Ok(Validator::Http { stop });
            }
            Err(e) => e,
        };
        let e443 = match tokio::net::TcpListener::bind(bind_addr(tls_port)).await {
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
    fn prepare(&self, d: &Direct, token: &str, key_auth: &str, digest: &[u8], ip: IpAddr) -> Result<(), Fail> {
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
fn alpn_cert(ip: IpAddr, digest: &[u8]) -> Result<CertifiedKey, String> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut params = rcgen::CertificateParams::new(vec![ip.to_string()]).map_err(|e| e.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    certified_key(&cert.pem(), &key.serialize_pem())
}

/// 转发模式的本机自检：经回环用这个 IP 当 Host 请求验证路径，能拿到回应说明 nginx / Caddy 的转发配好了。
async fn local_forward_ok(ip: IpAddr, token: &str, expect: &str) -> bool {
    let port = port_env("LOOKLOOK_ACME_HTTP_PORT", 80);
    let Ok(client) = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::limited(3)).build() else { return false };
    let loop_lit = if ip.is_ipv6() { "[::1]" } else { "127.0.0.1" };
    let url = format!("http://{loop_lit}:{port}{}{token}", proto::ACME_CHALLENGE_PREFIX);
    let host_ip = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let host = if port == 80 { host_ip } else { format!("{host_ip}:{port}") };
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
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
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
        let ck = alpn_cert(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), &[7u8; 32]).unwrap();
        let (nb, na) = validity(&ck.cert[0]).unwrap();
        assert!(na > nb);
        let (_, c) = x509_parser::parse_x509_certificate(ck.cert[0].as_ref()).unwrap();
        let ext = c.extensions().iter().find(|e| e.oid.to_id_string() == "1.3.6.1.5.5.7.1.31").expect("acmeIdentifier");
        assert!(ext.critical);
    }

    #[test]
    fn custom_urls_and_always_on_self_tls() {
        let (dir, s) = store();
        let d = Direct::new(s, 1234);
        assert_eq!(d.leaf_sha256().len(), 64, "启动即自带本机自签证书指纹");
        assert!(!d.active(), "自签证书不标为公网受信任证书");

        let saved = d
            .set_custom_urls(&[
                "https://Code.Example.com/".into(),
                "home.example.com:18443".into(),
                "[2408:8207::1]:1234".into(),
                "demo.trycloudflare.com".into(),
                "http://frp.example.com:8080/".into(),
            ])
            .unwrap();
        assert_eq!(
            saved,
            vec![
                "https://code.example.com",
                "home.example.com:18443",
                "[2408:8207::1]:1234",
                "https://demo.trycloudflare.com",
                "http://frp.example.com:8080",
            ]
        );
        let eps = d.endpoints(&AccessConfig::default());
        assert_eq!(eps.len(), 4, "明文 http 不进 wss 端点列表");
        assert!(eps[0].web_tls && eps[0].kind == EndpointKind::Tunnel);
        assert!(!eps[1].web_tls && eps[1].kind == EndpointKind::Ddns);
        assert!(!eps[2].web_tls && eps[2].kind == EndpointKind::Ipv6);
        assert!(eps[3].web_tls && eps[3].kind == EndpointKind::Tunnel);

        // 自定义 PEM 证书覆盖 *.example.com 后，home.example.com 自动升级为 web_tls = true
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["*.example.com".into()]).unwrap().self_signed(&key).unwrap();
        let (cp, kp) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cp, cert.pem()).unwrap();
        std::fs::write(&kp, key.serialize_pem()).unwrap();
        d.set_custom_tls(cp.to_str().unwrap(), kp.to_str().unwrap()).unwrap();
        assert!(d.active());
        let eps = d.endpoints(&AccessConfig::default());
        assert!(eps.iter().find(|e| e.url == "https://home.example.com:18443").unwrap().web_tls);
    }
}
