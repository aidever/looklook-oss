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
/// 票据有效期：只约束跳转那一下。
pub const GRANT_TTL_MS: i64 = 60_000;
/// 客户端校验 `exp` 时允许的误差。
pub const LEEWAY_MS: i64 = 30_000;

/// 心跳里的直连地址，平台存到 `devices.direct`，随设备列表下发。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectInfo {
    /// `https://34.168.24.68:1234`（不带结尾的 `/`）
    pub url: String,
    /// 证书到期时间（Unix 秒）：过了这个时间平台不再显示“直接打开”。
    pub until: i64,
}

impl DirectInfo {
    /// 地址是 `https://<IPv4>:<端口>` 的样子（平台只存、只跳这样的地址）。
    pub fn valid_url(&self) -> bool {
        let Some(rest) = self.url.strip_prefix("https://") else { return false };
        let Some((ip, port)) = rest.rsplit_once(':') else { return false };
        ip.parse::<std::net::Ipv4Addr>().is_ok() && port.parse::<u16>().is_ok_and(|p| p > 0) && !port.starts_with('0')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectGrantRequest {
    pub dev: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectGrantResponse {
    /// 跳转地址：`{origin}/__looklook/direct#<票据>`
    pub url: String,
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

/// 电脑侧校验：签名、版本、`exp`、`uid`、`dev`、`origin`。`jti` 去重与吊销由调用方完成。
pub fn verify_grant(
    token: &str,
    key_for: impl Fn(&str) -> Option<VerifyingKey>,
    own_user: &str,
    own_device: &str,
    own_origin: &str,
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
    if !g.origin.eq_ignore_ascii_case(own_origin) {
        return Err(GrantError::WrongOrigin);
    }
    Ok(g)
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
    fn urls() {
        let d = |u: &str| DirectInfo { url: u.into(), until: 0 };
        assert!(d("https://203.0.113.7:1234").valid_url());
        for bad in ["http://203.0.113.7:1234", "https://evil.example:1234", "https://203.0.113.7", "https://203.0.113.7:0", "https://203.0.113.7:1234/x", "https://[::1]:1234"] {
            assert!(!d(bad).valid_url(), "{bad}");
        }
    }

    #[test]
    fn check_request_format() {
        let r = CheckRequest::Ping { ip: "203.0.113.7".into(), port: 1234 };
        assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!({ "kind": "ping", "ip": "203.0.113.7", "port": 1234 }));
    }
}
