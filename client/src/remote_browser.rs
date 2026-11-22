//! 当前设备绑定的远程指纹浏览器接入（基于 Profile Access Key）。
//!
//! - 配置仅保存在当前设备的 `<LOOKLOOK_HOME>/browser_binding.json`（权限 `0600`），不参与跨设备同步；
//! - 支持 `http://` 与 `https://` 双协议（内网/容器网桥直连与公网连接均兼容）；
//! - 密钥永不返回明文给前端，所有向远端浏览器服务的调用均由本机 Rust 后端代发。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::LocalError;
use crate::gateway::App;

const FILE_NAME: &str = "browser_binding.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserBinding {
    pub endpoint: String,
    pub portal_base: String,
    pub access_key: String,
    pub insecure_tls: bool,
    pub profile_id: String,
    pub profile_name: String,
}

fn binding_path(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

pub fn load(home: &Path) -> Option<BrowserBinding> {
    let raw = std::fs::read_to_string(binding_path(home)).ok()?;
    let b: BrowserBinding = serde_json::from_str(&raw).ok()?;
    if b.endpoint.trim().is_empty() || b.access_key.trim().is_empty() {
        return None;
    }
    Some(b)
}

pub fn save(home: &Path, b: &BrowserBinding) -> std::io::Result<()> {
    let path = binding_path(home);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(b).unwrap_or_default();
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

pub fn remove(home: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(binding_path(home)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn mask_key(key: &str) -> String {
    let s = key.trim();
    if s.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 10 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

pub fn normalize_base_url(raw: &str) -> Result<String, LocalError> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 {
        return Err(LocalError::invalid("endpoint", "url"));
    }
    let u = reqwest::Url::parse(s).map_err(|_| LocalError::invalid("endpoint", "url"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err(LocalError::invalid("endpoint", "url"));
    }
    Ok(s.trim_end_matches('/').to_string())
}

fn validate_target_url(raw: &str) -> Result<String, LocalError> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 8192 {
        return Err(LocalError::invalid("url", "url"));
    }
    let u = reqwest::Url::parse(s).map_err(|_| LocalError::invalid("url", "url"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err(LocalError::invalid("url", "url"));
    }
    Ok(s.to_string())
}

fn is_private_or_loopback(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let Some(host) = u.host_str() else { return false };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => false,
    }
}

fn build_http_client(endpoint: &str, insecure_tls: bool, timeout: Duration) -> Result<reqwest::Client, LocalError> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if is_private_or_loopback(endpoint) {
        builder = builder.no_proxy();
    }
    if endpoint.starts_with("https://") && insecure_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().map_err(|_| LocalError::new("INTERNAL"))
}

pub fn resolve_portal_url(endpoint: &str, portal_base: &str, portal_path: Option<&str>) -> String {
    let p = portal_path.unwrap_or("/b/").trim();
    if p.starts_with("http://") || p.starts_with("https://") {
        return p.to_string();
    }
    let base = if !portal_base.trim().is_empty() {
        portal_base.trim().trim_end_matches('/')
    } else {
        endpoint.trim().trim_end_matches('/')
    };
    if p.starts_with('/') {
        format!("{base}{p}")
    } else {
        format!("{base}/{p}")
    }
}

async fn fetch_remote_info(endpoint: &str, key: &str, insecure_tls: bool, timeout: Duration) -> Result<Value, LocalError> {
    let client = build_http_client(endpoint, insecure_tls, timeout)?;
    let url = format!("{}/b/api/v1/info", endpoint.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", key.trim()))
        .send()
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, url = %url, "远程指纹浏览器连接失败");
            LocalError::new("NETWORK")
        })?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(LocalError { status: StatusCode::UNAUTHORIZED, code: "BROWSER_KEY_INVALID".into(), params: Default::default() });
    }
    if !status.is_success() {
        return Err(LocalError { status: StatusCode::BAD_GATEWAY, code: "NETWORK".into(), params: Default::default() });
    }
    Ok(body)
}

#[derive(Deserialize, Default)]
pub struct GetQuery {
    #[serde(default)]
    pub refresh: bool,
}

pub async fn get_binding(State(app): State<App>, Query(q): Query<GetQuery>) -> Result<Json<Value>, LocalError> {
    let Some(mut b) = load(&app.paths.home) else {
        return Ok(Json(json!({ "bound": false })));
    };
    let mut live_status: Option<String> = None;
    let mut conflict = false;
    let mut installed = true;
    let mut portal_path: Option<String> = None;

    if q.refresh {
        match fetch_remote_info(&b.endpoint, &b.access_key, b.insecure_tls, Duration::from_secs(4)).await {
            Ok(info) => {
                if let Some(s) = info.get("status").and_then(|v| v.as_str()) {
                    live_status = Some(s.to_string());
                }
                conflict = info.get("conflict").and_then(|v| v.as_bool()).unwrap_or(false);
                installed = info.get("installed").and_then(|v| v.as_bool()).unwrap_or(true);
                portal_path = info.get("portal_path").and_then(|v| v.as_str()).map(str::to_owned);
                let new_name = info.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let new_id = info.get("profile_id").and_then(|v| v.as_str()).unwrap_or("");
                if (!new_name.is_empty() && new_name != b.profile_name) || (!new_id.is_empty() && new_id != b.profile_id) {
                    if !new_name.is_empty() {
                        b.profile_name = new_name.to_string();
                    }
                    if !new_id.is_empty() {
                        b.profile_id = new_id.to_string();
                    }
                    let _ = save(&app.paths.home, &b);
                }
            }
            Err(e) if e.code == "BROWSER_KEY_INVALID" => {
                live_status = Some("invalid_key".into());
            }
            Err(_) => {
                live_status = Some("offline".into());
            }
        }
    }

    let portal_url = resolve_portal_url(&b.endpoint, &b.portal_base, portal_path.as_deref());
    Ok(Json(json!({
        "bound": true,
        "endpoint": b.endpoint,
        "portal_base": b.portal_base,
        "key_masked": mask_key(&b.access_key),
        "insecure_tls": b.insecure_tls,
        "profile_id": b.profile_id,
        "profile_name": b.profile_name,
        "status": live_status,
        "conflict": conflict,
        "installed": installed,
        "portal_url": portal_url,
    })))
}

#[derive(Deserialize)]
pub struct SaveBindingReq {
    pub endpoint: String,
    #[serde(default)]
    pub portal_base: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default = "default_true")]
    pub insecure_tls: bool,
}

fn default_true() -> bool {
    true
}

pub async fn test_binding(State(app): State<App>, Json(req): Json<SaveBindingReq>) -> Result<Json<Value>, LocalError> {
    let endpoint = normalize_base_url(&req.endpoint)?;
    let portal_base = if req.portal_base.trim().is_empty() {
        String::new()
    } else {
        normalize_base_url(&req.portal_base)?
    };
    let key = if !req.access_key.trim().is_empty() {
        req.access_key.trim().to_string()
    } else {
        load(&app.paths.home)
            .map(|b| b.access_key)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| LocalError::invalid("access_key", "required"))?
    };
    let info = fetch_remote_info(&endpoint, &key, req.insecure_tls, Duration::from_secs(8)).await?;
    let portal_url = resolve_portal_url(&endpoint, &portal_base, info.get("portal_path").and_then(|v| v.as_str()));
    Ok(Json(json!({
        "ok": true,
        "profile_id": info.get("profile_id").cloned().unwrap_or(Value::Null),
        "profile_name": info.get("name").cloned().unwrap_or(Value::Null),
        "os": info.get("os").cloned().unwrap_or(Value::Null),
        "status": info.get("status").cloned().unwrap_or(Value::Null),
        "installed": info.get("installed").cloned().unwrap_or(json!(true)),
        "conflict": info.get("conflict").cloned().unwrap_or(json!(false)),
        "portal_url": portal_url,
    })))
}

pub async fn put_binding(State(app): State<App>, Json(req): Json<SaveBindingReq>) -> Result<Json<Value>, LocalError> {
    let endpoint = normalize_base_url(&req.endpoint)?;
    let portal_base = if req.portal_base.trim().is_empty() {
        String::new()
    } else {
        normalize_base_url(&req.portal_base)?
    };
    let key = if !req.access_key.trim().is_empty() {
        req.access_key.trim().to_string()
    } else {
        load(&app.paths.home)
            .map(|b| b.access_key)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| LocalError::invalid("access_key", "required"))?
    };
    let info = fetch_remote_info(&endpoint, &key, req.insecure_tls, Duration::from_secs(8)).await?;
    let binding = BrowserBinding {
        endpoint: endpoint.clone(),
        portal_base: portal_base.clone(),
        access_key: key,
        insecure_tls: req.insecure_tls,
        profile_id: info.get("profile_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        profile_name: info.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    };
    save(&app.paths.home, &binding)?;
    let portal_url = resolve_portal_url(&binding.endpoint, &binding.portal_base, info.get("portal_path").and_then(|v| v.as_str()));
    Ok(Json(json!({
        "bound": true,
        "endpoint": binding.endpoint,
        "portal_base": binding.portal_base,
        "key_masked": mask_key(&binding.access_key),
        "insecure_tls": binding.insecure_tls,
        "profile_id": binding.profile_id,
        "profile_name": binding.profile_name,
        "status": info.get("status").cloned().unwrap_or(Value::Null),
        "conflict": info.get("conflict").cloned().unwrap_or(json!(false)),
        "installed": info.get("installed").cloned().unwrap_or(json!(true)),
        "portal_url": portal_url,
    })))
}

pub async fn delete_binding(State(app): State<App>) -> Result<Json<Value>, LocalError> {
    remove(&app.paths.home)?;
    Ok(Json(json!({ "bound": false })))
}

#[derive(Deserialize)]
pub struct OpenUrlReq {
    pub url: String,
    #[serde(default)]
    pub auto_start: bool,
}

pub async fn open_url(State(app): State<App>, Json(req): Json<OpenUrlReq>) -> Result<Json<Value>, LocalError> {
    let target = validate_target_url(&req.url)?;
    let Some(b) = load(&app.paths.home) else {
        return Err(LocalError {
            status: StatusCode::BAD_REQUEST,
            code: "BROWSER_NOT_BOUND".into(),
            params: Default::default(),
        });
    };
    let timeout = if req.auto_start { Duration::from_secs(60) } else { Duration::from_secs(12) };
    let client = build_http_client(&b.endpoint, b.insecure_tls, timeout)?;
    let url = format!("{}/b/api/v1/open-url", b.endpoint.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", b.access_key.trim()))
        .json(&json!({ "url": target, "auto_start": req.auto_start }))
        .send()
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, url = %url, "调用远程指纹浏览器打开链接失败");
            LocalError::new("NETWORK")
        })?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if status == StatusCode::UNAUTHORIZED {
        return Err(LocalError { status, code: "BROWSER_KEY_INVALID".into(), params: Default::default() });
    }
    if status == StatusCode::CONFLICT || body.get("error").and_then(|v| v.as_str()) == Some("ANOTHER_PROFILE_RUNNING") {
        return Err(LocalError {
            status: StatusCode::CONFLICT,
            code: "BROWSER_PROFILE_CONFLICT".into(),
            params: Default::default(),
        });
    }
    if !status.is_success() {
        let code = body.get("error").and_then(|v| v.as_str()).unwrap_or("BROWSER_OPEN_FAILED");
        return Err(LocalError {
            status: StatusCode::BAD_REQUEST,
            code: code.to_string(),
            params: Default::default(),
        });
    }
    let portal_url = resolve_portal_url(&b.endpoint, &b.portal_base, body.get("portal_path").and_then(|v| v.as_str()));
    let mut out = body;
    if let Some(m) = out.as_object_mut() {
        m.insert("portal_url".into(), json!(portal_url));
        if !m.contains_key("profile_name") && !b.profile_name.is_empty() {
            m.insert("profile_name".into(), json!(b.profile_name));
        }
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https_endpoints() {
        assert_eq!(normalize_base_url("http://127.0.0.1:1333/").unwrap(), "http://127.0.0.1:1333");
        assert_eq!(normalize_base_url("http://10.10.10.1:333").unwrap(), "http://10.10.10.1:333");
        assert_eq!(normalize_base_url("https://browser.example.com:333/").unwrap(), "https://browser.example.com:333");
        assert!(normalize_base_url("ftp://example.com").is_err());
        assert!(normalize_base_url("not-a-url").is_err());
    }

    #[test]
    fn detects_private_addresses_and_resolves_portal() {
        assert!(is_private_or_loopback("http://127.0.0.1:1333"));
        assert!(is_private_or_loopback("http://10.10.10.1:333"));
        assert!(is_private_or_loopback("http://192.168.1.5:333"));
        assert!(!is_private_or_loopback("https://browser.example.com:333"));

        assert_eq!(resolve_portal_url("http://10.10.10.1:1333", "", Some("/b/")), "http://10.10.10.1:1333/b/");
        assert_eq!(
            resolve_portal_url("http://10.10.10.1:1333", "https://browser.example.com:333/", Some("/b/")),
            "https://browser.example.com:333/b/"
        );
    }

    #[test]
    fn masks_key_and_roundtrips_disk() {
        assert_eq!(mask_key("dbk_1234567890abcdef"), "dbk_12****cdef");
        let dir = std::env::temp_dir().join(format!("ll-browser-bind-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let b = BrowserBinding {
            endpoint: "http://127.0.0.1:1333".into(),
            portal_base: "https://browser.example.com:333".into(),
            access_key: "dbk_secret_test_key_1234".into(),
            insecure_tls: true,
            profile_id: "p1".into(),
            profile_name: "Work".into(),
        };
        save(&dir, &b).unwrap();
        let got = load(&dir).unwrap();
        assert_eq!(got.endpoint, b.endpoint);
        assert_eq!(got.access_key, b.access_key);
        remove(&dir).unwrap();
        assert!(load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
