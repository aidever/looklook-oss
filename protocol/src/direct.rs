//! 公网 HTTPS 直连（服务端 docs/09）：电脑自己向 Let's Encrypt 申请 IP 证书，浏览器直接打开 `https://<公网IP>:<端口>`。
//!
//! - **心跳**带上直连地址（[`DirectInfo`]），平台在设备列表里显示“直接打开”；
//! - **直连票据** DirectGrant（主服务 → 浏览器 → 电脑，§5）：格式、签名密钥同 RtcGrant（`base64url(json).base64url(Ed25519(json))`），
//!   绑定用户、子站会话、目标设备与直连地址的源（`origin`），60 秒有效、一次性（`jti`）；
//!   浏览器跳到 `{origin}/__looklook/direct#<票据>`，页面脚本把票据 POST 给电脑，电脑校验通过后发 Cookie；
//! - **自检**（§6）：设备签名的 `POST /api/client/v1/direct/check`，主服务从外面访问这台电脑，确认端口通、证书对。

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::signing::{b64, b64_decode, sign, verify};

/// 浏览器 → 主服务（用户站）：签一张直连票据，`{ "dev": "<public_id>" }` → [`DirectGrantResponse`]。
pub const GRANT_PATH: &str = "/_ll/direct-grant";
/// 电脑上接收票据的页面（GET 返回页面，POST 提交票据）。
pub const CLIENT_PATH: &str = "/__looklook/direct";
/// 电脑上的直连传输（专属域名下的页面跨源连过来）：`wss://IP:端口/__looklook/mux?g=<票据>`。
///
/// 一条 WebSocket 里复用多条通道，每条通道上跑的帧与 WebRTC DataChannel 完全相同（服务端 docs/06 §6.2）。
/// WebSocket 的每条二进制消息：`[op: u8][通道 id: u32 大端][内容]`，op 见 [`MUX_OPEN`]、[`MUX_MSG`]、[`MUX_CLOSE`]。
/// 通道只由浏览器打开（id 由浏览器分配），任一方都可以关。
pub const MUX_PATH: &str = "/__looklook/mux";
/// 打开通道，内容是通道名（`ctl`、`r`）。
pub const MUX_OPEN: u8 = 1;
/// 通道上的一条消息。
pub const MUX_MSG: u8 = 2;
/// 关闭通道（内容为空）。
pub const MUX_CLOSE: u8 = 3;

/// 电脑上的自检地址：只返回固定内容 [`PING_BODY`]，不需要认证。
pub const PING_PATH: &str = "/__looklook/ping";
pub const PING_BODY: &str = "looklook-direct-ok";
/// ACME http-01 的验证路径前缀。
pub const ACME_CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";
/// 客户端接口（设备签名）：自检。
pub const CLIENT_CHECK_PATH: &str = "/direct/check";

pub const GRANT_VERSION: u8 = 1;
/// 票据有效期：只约束跳转或建连那一下。
pub const GRANT_TTL_MS: i64 = 60_000;
/// 客户端校验 `exp` 时允许的误差。
pub const LEEWAY_MS: i64 = 30_000;

/// 直连端点的类别：客户端和启动页按优先级（本机回环 → `Lan` → `Tunnel` → `Ipv6` → `Public` → `Ddns`）并发竞速。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// 局域网私有地址（RFC1918）、虚拟组网（Tailscale/ZeroTier `100.64/10`）或 IPv6 ULA（`fc00::/7`）。
    Lan,
    /// 用户配置的内网穿透 / 反向代理 HTTPS 地址（Cloudflare Tunnel、frp、Tailscale Funnel、Caddy/Nginx/Lucky 等）。
    Tunnel,
    /// 公网全局单播 IPv6（`2000::/3`）。
    Ipv6,
    /// 公网单播 IPv4（含端口映射 `IP:外部端口`）。
    Public,
    /// 动态域名 DDNS 或自定义域名直连。
    Ddns,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// 设备上报的一条直连可达地址，随设备列表下发给浏览器、桌面端和手机端竞速连接。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectEndpoint {
    /// `https://<主机>[:<端口>]`（不带结尾 `/`、路径或查询串）。
    pub url: String,
    pub kind: EndpointKind,
    /// 是否有公共受信任的 TLS 证书（浏览器可直接建 `wss://` 连接）；
    /// `false` 表示使用设备持久自签 TLS 证书（桌面端/手机端通过 `ctl hello` 的证书指纹 + 设备 Ed25519 公钥签名校验连接）。
    #[serde(default)]
    pub web_tls: bool,
    /// 证书到期时间（Unix 秒）；`0` 表示外部穿透自己维护证书或本机自签证书（随设备在线状态有效）。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub until: i64,
}

impl DirectEndpoint {
    /// 校验地址形态：`Lan` 允许私网/CGNAT/ULA，其余只允许公网 IPv4、公网 IPv6 或公网域名。
    pub fn valid_url(&self) -> bool {
        valid_https_origin(&self.url, matches!(self.kind, EndpointKind::Lan))
    }
}

/// 心跳里的外部浏览器直连地址，平台存到 `devices.direct`，随设备列表下发（供“直接打开”跳转）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectInfo {
    /// `https://<公网IP或域名>[:<端口>]`（不带结尾的 `/`）
    pub url: String,
    /// 到期时间（Unix 秒）：过了这个时间平台不再显示“直接打开”。
    pub until: i64,
}

impl DirectInfo {
    /// 合法的外部 `https://` 源（公网 IPv4、公网 `[IPv6]` 或域名，可选非零端口，不带路径或查询参数）。
    pub fn valid_url(&self) -> bool {
        valid_https_origin(&self.url, false)
    }
}

/// 校验 `https://<host>[:<port>]` 源格式。
/// `allow_private = true` 时额外允许 RFC1918 私网、`100.64/10` 组网网段与 IPv6 ULA `fc00::/7`。
pub fn valid_https_origin(url: &str, allow_private: bool) -> bool {
    if url.len() > 255 {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if rest.is_empty() || rest.bytes().any(|b| matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\') || b <= b' ' || b >= 0x7f) {
        return false;
    }
    if let Some(after_bracket) = rest.strip_prefix('[') {
        let Some((v6_str, tail)) = after_bracket.split_once(']') else { return false };
        if !tail.is_empty() {
            let Some(port) = tail.strip_prefix(':') else { return false };
            if !valid_port_str(port) {
                return false;
            }
        }
        // IPv6 字符统一不允许大写混杂之外的怪字符，且拒绝 zone id（`%eth0`）
        if v6_str.contains('%') {
            return false;
        }
        let Ok(ip) = v6_str.parse::<std::net::Ipv6Addr>() else { return false };
        return valid_ipv6(ip, allow_private);
    }
    let (host, port_opt) = match rest.split_once(':') {
        Some((h, p)) => {
            if p.contains(':') || !valid_port_str(p) {
                return false;
            }
            (h, Some(p))
        }
        None => (rest, None),
    };
    let _ = port_opt;
    if host.is_empty() {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        // 标准点分十进制（避免奇怪缩写）且非保留回环地址
        return host.split('.').count() == 4
            && host.split('.').all(|s| !s.is_empty() && (s == "0" || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit()))
            && valid_ipv4(ip, allow_private);
    }
    valid_dns_host(host)
}

fn valid_port_str(p: &str) -> bool {
    !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n > 0)
}

fn valid_ipv4(ip: std::net::Ipv4Addr, allow_private: bool) -> bool {
    let o = ip.octets();
    if ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
    {
        return false;
    }
    let is_lan_or_cgnat = ip.is_private() || (o[0] == 100 && (64..128).contains(&o[1]));
    if is_lan_or_cgnat {
        return allow_private;
    }
    true
}

fn valid_ipv6(ip: std::net::Ipv6Addr, allow_private: bool) -> bool {
    let s = ip.segments();
    if ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.to_ipv4_mapped().is_some()
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
    {
        return false;
    }
    let is_ula = (s[0] & 0xfe00) == 0xfc00;
    if is_ula {
        return allow_private;
    }
    (s[0] & 0xe000) == 0x2000
}

fn valid_dns_host(host: &str) -> bool {
    if host.len() > 253 || !host.contains('.') {
        return false;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return false;
    }
    let mut last = "";
    for label in host.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return false;
        }
        last = label;
    }
    last.len() >= 2 && !last.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectGrantRequest {
    pub dev: String,
    /// 为 `true` 时签发设备级复用通道票据（`origin` 为空，供 `/__looklook/mux` 多候选竞速）。
    #[serde(default)]
    pub mux: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectGrantResponse {
    /// 跳转地址：`{origin}/__looklook/direct#<票据>`；仅请求 `mux` 票据且无外部 `DirectInfo` 时为空串。
    #[serde(default)]
    pub url: String,
    /// 票据原文（`mux` 竞速直接取用）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectGrant {
    pub v: u8,
    /// 平台签名密钥 id。
    pub kid: String,
    /// users.id（字符串），电脑校验等于本机登录的用户。
    pub uid: String,
    /// 子站会话短 id：吊销列表按它下发，电脑发的 Cookie 绑定它。
    pub sid: String,
    /// 目标设备 public_id，电脑校验等于自己。
    pub dev: String,
    /// 直连地址的源（`https://34.168.24.68:1234`），电脑校验等于这次请求的源。
    pub origin: String,
    /// 签发票据的用户站（稳定地址，`alice.looklook.example`）与它的源：专属域名下经 [`MUX_PATH`] 直连时，
    /// 电脑把请求当作来自这个站（同 RtcGrant 的 host / origin），并校验 WebSocket 的 Origin 等于 `site`。旧平台签的票据没有。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub site: String,
    /// 子站会话的绝对到期时间（Unix 毫秒）：电脑发的 Cookie 最多活到这时。
    pub abs_exp: i64,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    Malformed,
    UnknownKey,
    BadSignature,
    WrongVersion,
    Expired,
    WrongUser,
    WrongDevice,
    WrongOrigin,
}

pub fn encode_grant(key: &SigningKey, g: &DirectGrant) -> String {
    let json = serde_json::to_string(g).expect("grant serializes");
    format!("{}.{}", b64(json.as_bytes()), sign(key, &json))
}

/// 电脑侧校验（浏览器跳转 `POST /__looklook/direct`）：签名、版本、`exp`、`uid`、`dev`、严格匹配 `origin`。
pub fn verify_grant(
    token: &str,
    key_for: impl Fn(&str) -> Option<VerifyingKey>,
    own_user: &str,
    own_device: &str,
    own_origin: &str,
    now_ms: i64,
) -> Result<DirectGrant, GrantError> {
    verify_grant_inner(token, key_for, own_user, own_device, Some(own_origin), now_ms)
}

/// 电脑侧校验（`GET /__looklook/mux` 多候选并发竞速）：允许设备级票据 `g.origin` 为空，或与当前连接源匹配。
pub fn verify_mux_grant(
    token: &str,
    key_for: impl Fn(&str) -> Option<VerifyingKey>,
    own_user: &str,
    own_device: &str,
    own_origin: &str,
    now_ms: i64,
) -> Result<DirectGrant, GrantError> {
    verify_grant_inner(token, key_for, own_user, own_device, None, now_ms).and_then(|g| {
        if g.origin.is_empty() || g.origin.eq_ignore_ascii_case(own_origin) {
            Ok(g)
        } else {
            Err(GrantError::WrongOrigin)
        }
    })
}

fn verify_grant_inner(
    token: &str,
    key_for: impl Fn(&str) -> Option<VerifyingKey>,
    own_user: &str,
    own_device: &str,
    strict_origin: Option<&str>,
    now_ms: i64,
) -> Result<DirectGrant, GrantError> {
    let (p, s) = token.split_once('.').ok_or(GrantError::Malformed)?;
    let json = String::from_utf8(b64_decode(p).ok_or(GrantError::Malformed)?).map_err(|_| GrantError::Malformed)?;
    let g: DirectGrant = serde_json::from_str(&json).map_err(|_| GrantError::Malformed)?;
    let key = key_for(&g.kid).ok_or(GrantError::UnknownKey)?;
    if !verify(&key, &json, s) {
        return Err(GrantError::BadSignature);
    }
    if g.v != GRANT_VERSION {
        return Err(GrantError::WrongVersion);
    }
    if g.exp + LEEWAY_MS < now_ms {
        return Err(GrantError::Expired);
    }
    if g.uid != own_user {
        return Err(GrantError::WrongUser);
    }
    if g.dev != own_device {
        return Err(GrantError::WrongDevice);
    }
    if let Some(own_origin) = strict_origin {
        if g.origin.is_empty() || !g.origin.eq_ignore_ascii_case(own_origin) {
            return Err(GrantError::WrongOrigin);
        }
    }
    Ok(g)
}

/// 从已签名的 `DirectGrant` 令牌原文里提取 `jti`（发起方构造 [`mux_hello_canonical`] 验签用）。
pub fn extract_jti(token: &str) -> Option<String> {
    let (p, _) = token.split_once('.')?;
    let v: serde_json::Value = serde_json::from_slice(&b64_decode(p)?).ok()?;
    v.get("jti")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// `/__looklook/mux` 通道内 `ctl hello` 双向身份与 TLS 证书绑定规范串：
/// `LL1-MUX-HELLO\n{票据 jti}\n{发起方随机 nonce}\n{对端 TLS 叶子证书 SHA-256 小写 hex}`。
pub fn mux_hello_canonical(jti: &str, nonce: &str, cert_sha256_hex: &str) -> String {
    format!("LL1-MUX-HELLO\n{jti}\n{nonce}\n{}", cert_sha256_hex.to_ascii_lowercase())
}

/// 自检请求：主服务从外面访问这台电脑。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckRequest {
    /// `GET http://{ip}:{port}/.well-known/acme-challenge/{token}`，回应要等于 `expect`（申请证书前确认 80 端口或转发配好了）。
    Challenge { ip: String, port: u16, token: String, expect: String },
    /// 只做 TCP 连接（tls-alpn-01 用 443 时）。
    Tcp { ip: String, port: u16 },
    /// `GET https://{ip}:{port}/__looklook/ping`，按正常的证书校验（申请成功后确认整条路通）。
    Ping { ip: String, port: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResponse {
    pub ok: bool,
    /// 不通时的原因：`connect`（连不上）、`timeout`、`tls`（证书不对）、`status`（回应不对）、`body`（内容不对）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> DirectGrant {
        DirectGrant {
            v: GRANT_VERSION,
            kid: "k1".into(),
            uid: "1".into(),
            sid: "s1".into(),
            dev: "dev1".into(),
            origin: "https://203.0.113.7:1234".into(),
            host: "alice.looklook.example".into(),
            site: "https://alice.looklook.example".into(),
            abs_exp: 10_000_000,
            iat: 0,
            exp: GRANT_TTL_MS,
            jti: "j".into(),
        }
    }

    #[test]
    fn old_grant_without_site_parses() {
        let mut g = grant();
        g.host.clear();
        g.site.clear();
        let json = serde_json::to_string(&g).unwrap();
        assert!(!json.contains("site"), "空的不写出来：{json}");
        assert_eq!(serde_json::from_str::<DirectGrant>(&json).unwrap(), g);
    }

    #[test]
    fn grant_roundtrip_and_checks() {
        let k = SigningKey::from_bytes(&[3u8; 32]);
        let pubk = k.verifying_key();
        let keys = |kid: &str| (kid == "k1").then_some(pubk);
        let t = encode_grant(&k, &grant());
        let o = "https://203.0.113.7:1234";
        assert_eq!(verify_grant(&t, keys, "1", "dev1", o, 1000).unwrap(), grant());
        assert_eq!(verify_grant(&t, keys, "2", "dev1", o, 1000), Err(GrantError::WrongUser));
        assert_eq!(verify_grant(&t, keys, "1", "dev2", o, 1000), Err(GrantError::WrongDevice));
        assert_eq!(verify_grant(&t, keys, "1", "dev1", "https://203.0.113.8:1234", 1000), Err(GrantError::WrongOrigin));
        assert_eq!(verify_grant(&t, keys, "1", "dev1", o, GRANT_TTL_MS + LEEWAY_MS + 1), Err(GrantError::Expired));
        assert_eq!(verify_grant(&t, |_| None, "1", "dev1", o, 0), Err(GrantError::UnknownKey));
        let other = SigningKey::from_bytes(&[4u8; 32]).verifying_key();
        assert_eq!(verify_grant(&t, |_| Some(other), "1", "dev1", o, 0), Err(GrantError::BadSignature));
        assert_eq!(verify_grant("x", keys, "1", "dev1", o, 0), Err(GrantError::Malformed));
        // RtcGrant 不能当直连票据用（字段不全）
        let rtc = format!("{}.{}", b64(br#"{"v":1,"kid":"k1"}"#), sign(&k, r#"{"v":1,"kid":"k1"}"#));
        assert_eq!(verify_grant(&rtc, keys, "1", "dev1", o, 0), Err(GrantError::Malformed));
    }

    #[test]
    fn urls_and_endpoints() {
        let d = |u: &str| DirectInfo { url: u.into(), until: 0 };
        for good in [
            "https://203.0.113.7:1234",
            "https://34.168.24.68",
            "https://dev.example.com",
            "https://home.example.com:8443",
            "https://[2408:8207:1234::1]:1234",
            "https://[2606:4700:4700::1111]",
        ] {
            assert!(d(good).valid_url(), "expected valid public DirectInfo: {good}");
        }
        for bad in [
            "http://203.0.113.7:1234",
            "https://192.168.1.5:1234",
            "https://100.64.0.8:1234",
            "https://127.0.0.1:1234",
            "https://localhost:1234",
            "https://box.local:1234",
            "https://203.0.113.7:0",
            "https://203.0.113.7:0123",
            "https://203.0.113.7:1234/x",
            "https://203.0.113.7:1234?q=1",
            "https://[::1]:1234",
            "https://[fe80::1]:1234",
            "https://[fd00::1]:1234",
            "https://UPPER.example.com:1234",
            "https://999.1.1.1:1234",
        ] {
            assert!(!d(bad).valid_url(), "expected invalid public DirectInfo: {bad}");
        }

        let lan = |u: &str| DirectEndpoint { url: u.into(), kind: EndpointKind::Lan, web_tls: false, until: 0 };
        assert!(lan("https://192.168.1.20:1234").valid_url());
        assert!(lan("https://10.0.0.5:1234").valid_url());
        assert!(lan("https://100.101.102.103:1234").valid_url());
        assert!(lan("https://[fd12:3456::1]:1234").valid_url());
        assert!(!lan("https://127.0.0.1:1234").valid_url());
        assert!(!lan("https://[::1]:1234").valid_url());

        let pub_ep = DirectEndpoint { url: "https://192.168.1.20:1234".into(), kind: EndpointKind::Public, web_tls: false, until: 0 };
        assert!(!pub_ep.valid_url(), "私网地址不能冒充公网端点");
    }

    #[test]
    fn mux_grant_and_hello_binding() {
        let k = SigningKey::from_bytes(&[3u8; 32]);
        let pubk = k.verifying_key();
        let keys = |kid: &str| (kid == "k1").then_some(pubk);
        let mut g = grant();
        g.origin.clear();
        g.jti = "jti_race_1".into();
        let t = encode_grant(&k, &g);
        assert_eq!(extract_jti(&t).as_deref(), Some("jti_race_1"));
        // 空 origin 的设备级票据只能用于 mux，不能用于设 Cookie 的跳转接口
        assert_eq!(verify_grant(&t, keys, "1", "dev1", "https://192.168.1.5:1234", 1000), Err(GrantError::WrongOrigin));
        assert_eq!(verify_mux_grant(&t, keys, "1", "dev1", "https://192.168.1.5:1234", 1000).unwrap(), g);
        assert_eq!(verify_mux_grant(&t, keys, "1", "dev1", "http://127.0.0.1:1234", 1000).unwrap(), g);
        assert_eq!(
            mux_hello_canonical("jti_race_1", "n1", "ABCDEF"),
            "LL1-MUX-HELLO\njti_race_1\nn1\nabcdef"
        );
    }

    #[test]
    fn check_request_format() {
        let r = CheckRequest::Ping { ip: "203.0.113.7".into(), port: 1234 };
        assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!({ "kind": "ping", "ip": "203.0.113.7", "port": 1234 }));
    }
}
