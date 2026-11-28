//! 共享电脑：按地址（+ 访问码）加进来的另一台看看，不属于本账户，和自己的电脑一起在电脑切换里出现。
//!
//! - 只有这台电脑自己打开的管理台（`Peer::Local`）会转发，请求一律由本机后端发给对方（HTTP 与 WebSocket），
//!   浏览器从不跨域直连对方（不碰 CORS、SameSite=Strict 的 Cookie 与混合内容）。见 `gateway::to_peer`。
//! - 对方开着访问码时，本机用存下的访问码换 Cookie（局域网/白名单 `ll_access`，公网 HTTPS 直连 `ll_direct`），
//!   只放在内存里；Cookie 失效（对方改了访问码、重启换了密钥）时重新换一次，换不到就让用户更新访问码。
//! - 访问码只存在本机；接口返回给页面时只说“有没有填”。
//! - 共享电脑的 id 用本机前缀 `x-`，不会和平台的电脑 id 撞上；绝不参与设置同步（`settings_sync`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, protocol::Role, Message};

use crate::error::LocalError;
use crate::store::Store;

/// 共享电脑 id 的前缀（平台的电脑 id 不带 `-`）
pub const PREFIX: &str = "x-";
const KEY: &str = "shared_pcs";
const MAX: usize = 20;
const CONNECT_LIMIT: Duration = Duration::from_secs(8);
const PROBE_LIMIT: Duration = Duration::from_secs(4);
const PROBE_TTL: Duration = Duration::from_secs(10);
/// 请求体不超过这么大时先读进内存，Cookie 失效重新换过后能再发一次；更大的（上传）直接边读边发
const RETRY_BODY: usize = 4 << 20;
/// 访问码页面很小；只有这么小的 HTML 才看是不是访问码页面
const CODE_PAGE_MAX: u64 = 16 * 1024;
const COOKIES: [&str; 2] = [crate::gateway::access::COOKIE, crate::direct_tls::COOKIE];

pub fn is_shared(id: &str) -> bool {
    id.starts_with(PREFIX)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    /// 规范后的源：`http(s)://host[:port]`
    pub url: String,
    #[serde(default)]
    pub code: String,
    /// 备注名；空时显示对方的电脑名
    #[serde(default)]
    pub remark: String,
    /// 上次连上时对方的电脑名与系统
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub os: String,
}

impl Entry {
    pub fn label(&self) -> String {
        [&self.remark, &self.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_else(|| host_of(&self.url).to_string())
    }
}

fn host_of(url: &str) -> &str {
    url.split_once("://").map(|(_, r)| r).unwrap_or(url)
}

/// 失败原因（错误码，界面按 `error.<code>` 翻译）
fn err(code: &'static str) -> LocalError {
    let mut e = LocalError::new(code);
    e.status = match code {
        "SHARED_PC_UNREACHABLE" => StatusCode::BAD_GATEWAY,
        "SHARED_PC_DENIED" => StatusCode::FORBIDDEN,
        "SHARED_PC_NOT_FOUND" => StatusCode::NOT_FOUND,
        "SHARED_PC_CODE_REQUIRED" | "SHARED_PC_CODE_WRONG" | "SHARED_PC_CODE_LOCKED" | "SHARED_PC_CODE_SHORT" | "SHARED_PC_NOT_READY" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    e
}

/// 用户填的地址 → 规范的源。没写协议当 `http://`，只要协议、主机与端口（路径、查询都去掉）。
pub fn normalize_url(raw: &str) -> Result<String, LocalError> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 300 {
        return Err(err("SHARED_PC_BAD_URL"));
    }
    let s = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
    let u = reqwest::Url::parse(&s).map_err(|_| err("SHARED_PC_BAD_URL"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) || !u.username().is_empty() || u.password().is_some() {
        return Err(err("SHARED_PC_BAD_URL"));
    }
    Ok(u.origin().ascii_serialization())
}

/// 指向这台电脑自己的管理台（回环地址 + 本机端口）
fn is_self(url: &str, own_port: u16) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']');
    let local = host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    local && u.port_or_known_default() == Some(own_port)
}

/// 备注名：去掉控制字符，最多 32 个字
pub fn clean_remark(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(32).collect()
}

/// 访问码页面上的错误 → 错误码（`assets::code_page`）
fn code_page_error(body: &str) -> &'static str {
    if body.contains("输错次数太多") {
        "SHARED_PC_CODE_LOCKED"
    } else if body.contains("不到 12 位") {
        "SHARED_PC_CODE_SHORT"
    } else if body.contains("还没准备好") {
        "SHARED_PC_NOT_READY"
    } else {
        "SHARED_PC_CODE_WRONG"
    }
}

fn is_code_page(body: &[u8]) -> bool {
    let needle = format!("action=\"{}\"", crate::gateway::access::FORM_PATH);
    body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// 从 `Set-Cookie` 里取访问码 Cookie（`name=value`）
fn take_cookie(h: &reqwest::header::HeaderMap) -> Option<String> {
    h.get_all(reqwest::header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|v| {
        let pair = v.split(';').next()?.trim();
        let (k, val) = pair.split_once('=')?;
        (COOKIES.contains(&k.trim()) && !val.is_empty()).then(|| pair.to_string())
    })
}

#[derive(Default, Clone)]
struct Live {
    /// 换到的 Cookie：`None` 还没连过；`Some(None)` 对方不要访问码
    cookie: Option<Option<String>>,
    checked: Option<Instant>,
    online: bool,
    problem: Option<&'static str>,
}

pub struct SharedPcs {
    store: Arc<Store>,
    http: reqwest::Client,
    live: Mutex<HashMap<String, Live>>,
    /// 同一时间只换一次 Cookie（同一台电脑连着来的请求不重复提交访问码，免得被对方当成试错）
    login: tokio::sync::Mutex<()>,
    /// 有没有共享电脑（每个请求都要看一下，不每次读库）
    any: std::sync::atomic::AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SharedPcs {
    pub fn new(store: Arc<Store>) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_LIMIT)
            .no_proxy()
            .build()
            .unwrap_or_default();
        let any = store.get::<Vec<Entry>>(KEY).ok().flatten().is_some_and(|v| !v.is_empty());
        Self { store, http, live: Mutex::new(HashMap::new()), login: tokio::sync::Mutex::new(()), any: any.into() }
    }

    pub fn any(&self) -> bool {
        self.any.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<Entry> {
        self.store.get::<Vec<Entry>>(KEY).ok().flatten().unwrap_or_default()
    }

    fn save(&self, items: &[Entry]) -> Result<(), LocalError> {
        self.store.set(KEY, &items)?;
        self.any.store(!items.is_empty(), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Entry, LocalError> {
        self.list().into_iter().find(|e| e.id == id).ok_or_else(|| err("SHARED_PC_NOT_FOUND"))
    }

    /// 给页面的样子：访问码只说有没有填
    fn view_of(&self, e: &Entry) -> Value {
        let l = lock(&self.live).get(&e.id).cloned().unwrap_or_default();
        json!({
            "id": e.id, "url": e.url, "remark": e.remark, "name": e.label(), "peer_name": e.name, "os": e.os,
            "has_code": !e.code.is_empty(), "online": l.online, "problem": l.problem,
        })
    }

    /// 页面上的列表（先探一下谁在线，结果缓存 10 秒）
    pub async fn view(&self) -> Vec<Value> {
        let items = self.list();
        futures_util::future::join_all(items.iter().map(|e| self.probe(e))).await;
        let items = self.list();
        items.iter().map(|e| self.view_of(e)).collect()
    }

    pub async fn add(&self, url: &str, code: &str, remark: &str, own_port: u16) -> Result<Value, LocalError> {
        let url = normalize_url(url)?;
        if is_self(&url, own_port) {
            return Err(err("SHARED_PC_SELF"));
        }
        let mut items = self.list();
        if items.iter().any(|e| e.url == url) {
            return Err(err("SHARED_PC_DUPLICATE"));
        }
        if items.len() >= MAX {
            return Err(err("SHARED_PC_TOO_MANY"));
        }
        let mut e = Entry { id: format!("{PREFIX}{}", crate::util::random_id(10)), url, code: code.trim().to_string(), remark: clean_remark(remark), name: String::new(), os: String::new() };
        // 加之前先连一次：地址、访问码不对就不加，马上告诉用户
        let cookie = self.login(&e).await?;
        if let Some((name, os)) = self.status(&e, &cookie).await {
            e.name = name;
            e.os = os;
        }
        lock(&self.live).insert(e.id.clone(), Live { cookie: Some(cookie), checked: Some(Instant::now()), online: true, problem: None });
        items.push(e.clone());
        self.save(&items)?;
        tracing::info!(id = %e.id, url = %e.url, "加了共享电脑");
        Ok(self.view_of(&e))
    }

    /// 改备注、改访问码（`Some("")` 清掉）。改了访问码就丢掉旧 Cookie，下一个请求重新换。
    pub async fn update(&self, id: &str, remark: Option<&str>, code: Option<&str>) -> Result<Value, LocalError> {
        let mut items = self.list();
        let e = items.iter_mut().find(|e| e.id == id).ok_or_else(|| err("SHARED_PC_NOT_FOUND"))?;
        if let Some(r) = remark {
            e.remark = clean_remark(r);
        }
        if let Some(c) = code {
            e.code = c.trim().to_string();
            lock(&self.live).remove(id);
        }
        let e = e.clone();
        self.save(&items)?;
        if code.is_some() {
            self.probe(&e).await;
        }
        Ok(self.view_of(&e))
    }

    pub fn remove(&self, id: &str) -> Result<(), LocalError> {
        let mut items = self.list();
        let n = items.len();
        items.retain(|e| e.id != id);
        if items.len() == n {
            return Err(err("SHARED_PC_NOT_FOUND"));
        }
        self.save(&items)?;
        lock(&self.live).remove(id);
        Ok(())
    }

    fn set_live(&self, id: &str, f: impl FnOnce(&mut Live)) {
        f(lock(&self.live).entry(id.to_string()).or_default());
    }

    /// 连那台电脑：对方不要访问码时为 `None`，要的话用存下的访问码换一个 Cookie。
    async fn login(&self, e: &Entry) -> Result<Option<String>, LocalError> {
        let ping = |cookie: Option<&str>| {
            let mut r = self.http.get(format!("{}/api/ping", e.url)).timeout(PROBE_LIMIT + CONNECT_LIMIT);
            if let Some(c) = cookie {
                r = r.header(reqwest::header::COOKIE, c);
            }
            r.send()
        };
        let r = ping(None).await.map_err(|_| err("SHARED_PC_UNREACHABLE"))?;
        match r.status().as_u16() {
            200..=299 => return Ok(None),
            401 => {}
            403 => return Err(err("SHARED_PC_DENIED")),
            _ => return Err(err("SHARED_PC_UNREACHABLE")),
        }
        if e.code.is_empty() {
            return Err(err("SHARED_PC_CODE_REQUIRED"));
        }
        let form = [("code", e.code.as_str()), ("next", "/")];
        let r = self
            .http
            .post(format!("{}{}", e.url, crate::gateway::access::FORM_PATH))
            .header(reqwest::header::ORIGIN, &e.url)
            .form(&form)
            .timeout(PROBE_LIMIT + CONNECT_LIMIT)
            .send()
            .await
            .map_err(|_| err("SHARED_PC_UNREACHABLE"))?;
        let Some(cookie) = take_cookie(r.headers()) else {
            let body = r.text().await.unwrap_or_default();
            return Err(err(code_page_error(&body)));
        };
        // 换到的 Cookie 再试一次：确实能用才算数
        let ok = ping(Some(&cookie)).await.map_err(|_| err("SHARED_PC_UNREACHABLE"))?;
        if !ok.status().is_success() {
            return Err(err("SHARED_PC_CODE_WRONG"));
        }
        Ok(Some(cookie))
    }

    /// 当前可用的 Cookie；`fresh` 时丢掉旧的重新换
    async fn ensure(&self, e: &Entry, fresh: bool) -> Result<Option<String>, LocalError> {
        if !fresh {
            if let Some(c) = lock(&self.live).get(&e.id).and_then(|l| l.cookie.clone()) {
                return Ok(c);
            }
        }
        let _g = self.login.lock().await;
        // 等锁期间别的请求可能已经换好了
        if !fresh {
            if let Some(c) = lock(&self.live).get(&e.id).and_then(|l| l.cookie.clone()) {
                return Ok(c);
            }
        }
        let r = self.login(e).await;
        self.set_live(&e.id, |l| {
            l.checked = Some(Instant::now());
            match &r {
                Ok(c) => {
                    l.cookie = Some(c.clone());
                    l.online = true;
                    l.problem = None;
                }
                Err(x) => {
                    l.cookie = None;
                    l.online = false;
                    l.problem = Some(problem_code(&x.code));
                }
            }
        });
        r
    }

    /// 对方的电脑名与系统（`/api/status`；老版本没有 `device_name` 时用设置里的名字）
    async fn status(&self, e: &Entry, cookie: &Option<String>) -> Option<(String, String)> {
        let mut r = self.http.get(format!("{}/api/status", e.url)).timeout(PROBE_LIMIT);
        if let Some(c) = cookie {
            r = r.header(reqwest::header::COOKIE, c);
        }
        let v: Value = r.send().await.ok().filter(|r| r.status().is_success())?.json().await.ok()?;
        let name = v
            .get("device_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| v.pointer("/settings/device_name").and_then(Value::as_str))
            .unwrap_or("");
        let os = v.get("target").and_then(Value::as_str).and_then(|t| t.split('-').next()).filter(|t| *t != "unknown").unwrap_or("");
        Some((clean_remark(name), os.to_string()))
    }

    /// 探一下在不在线（缓存 10 秒），顺便记下对方现在的名字
    async fn probe(&self, e: &Entry) {
        if lock(&self.live).get(&e.id).and_then(|l| l.checked).is_some_and(|t| t.elapsed() < PROBE_TTL) {
            return;
        }
        self.set_live(&e.id, |l| l.checked = Some(Instant::now()));
        let cookie = match self.ensure(e, false).await {
            Ok(c) => c,
            Err(_) => return,
        };
        let st = self.status(e, &cookie).await;
        self.set_live(&e.id, |l| l.online = st.is_some());
        if let Some((name, os)) = st.filter(|(n, o)| *n != e.name || *o != e.os) {
            let mut items = self.list();
            if let Some(x) = items.iter_mut().find(|x| x.id == e.id) {
                x.name = name;
                x.os = os;
                let _ = self.save(&items);
            }
        }
    }

    /// 电脑列表里的条目（和 `/api/rtc/devices` 的 `RemoteDevice` 同样的字段，外加 `shared` 等）
    pub async fn devices(&self) -> Vec<Value> {
        self.view()
            .await
            .into_iter()
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.insert("shared".into(), true.into());
                    o.insert("terminals".into(), json!([]));
                    o.insert("last_seen_ms".into(), Value::Null);
                }
                v
            })
            .collect()
    }

    /// 转发管理台的一个请求到共享电脑（`gateway::to_peer` 已经校验过来源）
    pub async fn proxy(self: &Arc<Self>, id: &str, req: Request, path: String) -> Response {
        let e = match self.get(id) {
            Ok(e) => e,
            Err(x) => return x.into_response(),
        };
        // 设置同步只在同一账户的电脑之间（经平台验过身份的通道），绝不和共享电脑对账
        if path.starts_with("/api/sync") {
            return LocalError::new("REMOTE_FORBIDDEN").into_response();
        }
        let ws = req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        if ws {
            self.websocket(e, req, path).await
        } else {
            self.http(e, req, path).await
        }
    }

    async fn http(&self, e: Entry, req: Request, path: String) -> Response {
        let (parts, body) = req.into_parts();
        let small = matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS)
            || parts.headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n <= RETRY_BODY);
        let mut body = if small {
            match axum::body::to_bytes(body, RETRY_BODY).await {
                Ok(b) => ReqBody::Bytes(b),
                Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            }
        } else {
            ReqBody::Stream(body)
        };
        let mut fresh = false;
        loop {
            let cookie = match self.ensure(&e, fresh).await {
                Ok(c) => c,
                Err(x) => return x.into_response(),
            };
            let (b, again) = match std::mem::replace(&mut body, ReqBody::Used) {
                ReqBody::Bytes(b) => (reqwest::Body::from(b.clone()), ReqBody::Bytes(b)),
                ReqBody::Stream(s) => (reqwest::Body::wrap_stream(s.into_data_stream()), ReqBody::Used),
                ReqBody::Used => return err("SHARED_PC_UNREACHABLE").into_response(),
            };
            body = again;
            let mut rb = self.http.request(parts.method.clone(), format!("{}{path}", e.url)).headers(outgoing(&parts.headers, &e.url, &cookie)).body(b);
            if small {
                rb = rb.timeout(Duration::from_secs(120));
            }
            let r = match rb.send().await {
                Ok(r) => r,
                Err(x) => {
                    tracing::debug!(id = %e.id, error = %x, "共享电脑请求失败");
                    self.set_live(&e.id, |l| {
                        l.online = false;
                        l.problem = Some("SHARED_PC_UNREACHABLE");
                    });
                    return err("SHARED_PC_UNREACHABLE").into_response();
                }
            };
            // Cookie 失效（访问码改了）：重新换一次再发；还不行就告诉页面去更新访问码
            let (stale, head) = stale_check(r).await;
            if stale {
                lock(&self.live).remove(&e.id);
                if !fresh && matches!(body, ReqBody::Bytes(_)) {
                    fresh = true;
                    continue;
                }
                return err("SHARED_PC_CODE_WRONG").into_response();
            }
            return head;
        }
    }

    async fn websocket(&self, e: Entry, mut req: Request, path: String) -> Response {
        let Some(key) = req.headers().get(header::SEC_WEBSOCKET_KEY).cloned() else { return StatusCode::BAD_REQUEST.into_response() };
        let protos = req.headers().get(header::SEC_WEBSOCKET_PROTOCOL).cloned();
        let upgrade = hyper::upgrade::on(&mut req);
        let ws_url = format!("{}{path}", e.url.replacen("http", "ws", 1));
        let mut fresh = false;
        let (peer, proto) = loop {
            let cookie = match self.ensure(&e, fresh).await {
                Ok(c) => c,
                Err(x) => return x.into_response(),
            };
            let Ok(mut r) = ws_url.as_str().into_client_request() else { return StatusCode::BAD_REQUEST.into_response() };
            let h = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&e.url) {
                h.insert(header::ORIGIN, v);
            }
            if let Some(c) = cookie.as_deref().and_then(|c| HeaderValue::from_str(c).ok()) {
                h.insert(header::COOKIE, c);
            }
            if let Some(p) = &protos {
                h.insert(header::SEC_WEBSOCKET_PROTOCOL, p.clone());
            }
            match tokio::time::timeout(CONNECT_LIMIT * 2, tokio_tungstenite::connect_async(r)).await {
                Ok(Ok((ws, resp))) => break (ws, resp.headers().get(header::SEC_WEBSOCKET_PROTOCOL).cloned()),
                // 没升级、回了访问码页面或 401：Cookie 失效，重新换一次
                Ok(Err(tungstenite::Error::Http(resp))) if (resp.status().is_success() || resp.status() == StatusCode::UNAUTHORIZED) && !fresh => {
                    lock(&self.live).remove(&e.id);
                    fresh = true;
                }
                Ok(Err(tungstenite::Error::Http(resp))) => {
                    return StatusCode::from_u16(resp.status().as_u16()).ok().filter(|s| s.as_u16() >= 400).unwrap_or(StatusCode::BAD_GATEWAY).into_response();
                }
                _ => return err("SHARED_PC_UNREACHABLE").into_response(),
            }
        };
        let mut resp = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_ACCEPT, tungstenite::handshake::derive_accept_key(key.as_bytes()));
        if let Some(p) = proto {
            resp = resp.header(header::SEC_WEBSOCKET_PROTOCOL, p);
        }
        tokio::spawn(async move {
            match upgrade.await {
                Ok(io) => {
                    let browser = tokio_tungstenite::WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(io), Role::Server, None).await;
                    bridge(browser, peer).await;
                }
                Err(e) => tracing::debug!(error = %e, "WebSocket 升级失败"),
            }
        });
        resp.body(Body::empty()).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

enum ReqBody {
    Bytes(Bytes),
    Stream(Body),
    Used,
}

/// 错误码 → 列表上显示的问题
fn problem_code(code: &str) -> &'static str {
    match code {
        "SHARED_PC_CODE_REQUIRED" => "SHARED_PC_CODE_REQUIRED",
        "SHARED_PC_CODE_WRONG" => "SHARED_PC_CODE_WRONG",
        "SHARED_PC_CODE_LOCKED" => "SHARED_PC_CODE_LOCKED",
        "SHARED_PC_CODE_SHORT" => "SHARED_PC_CODE_SHORT",
        "SHARED_PC_NOT_READY" => "SHARED_PC_NOT_READY",
        "SHARED_PC_DENIED" => "SHARED_PC_DENIED",
        _ => "SHARED_PC_UNREACHABLE",
    }
}

/// 转给共享电脑的请求头：去掉连接相关、Host、本机的 Cookie、来源、平台保留的 `x-ll-*` 与反代头
/// （对方见到反代头会当成公网来源，见 `gateway::local_guard`），换上对方自己的来源与 Cookie。
fn outgoing(h: &HeaderMap, origin: &str, cookie: &Option<String>) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (k, v) in h {
        if forward_header(k) {
            out.append(k.clone(), v.clone());
        }
    }
    if let Ok(v) = HeaderValue::from_str(origin) {
        out.insert(header::ORIGIN, v);
    }
    out.insert(HeaderName::from_static(crate::gateway::CLIENT_HEADER), HeaderValue::from_static("1"));
    if let Some(c) = cookie.as_deref().and_then(|c| HeaderValue::from_str(c).ok()) {
        out.insert(header::COOKIE, c);
    }
    out
}

fn forward_header(name: &HeaderName) -> bool {
    let n = name.as_str();
    !(matches!(
        n,
        "host"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "proxy-connection"
            | "proxy-authorization"
            | "cookie"
            | "origin"
            | "referer"
            | "forwarded"
            | "x-real-ip"
            | "cf-connecting-ip"
            | "true-client-ip"
            | "content-length"
    ) || n.starts_with("x-ll-")
        || n.starts_with("x-forwarded-")
        || n.starts_with("sec-websocket-"))
}

/// 看对方的回应是不是“要访问码”（接口回 `ACCESS_CODE_REQUIRED`，页面回访问码页面）；不是就照原样转回去。
async fn stale_check(r: reqwest::Response) -> (bool, Response) {
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let len = r.content_length();
    let ctype = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let suspect = (status == StatusCode::UNAUTHORIZED && ctype.contains("json") && len.is_none_or(|n| n <= CODE_PAGE_MAX))
        || (ctype.starts_with("text/html") && len.is_some_and(|n| n <= CODE_PAGE_MAX));
    let mut resp = Response::builder().status(status);
    for (k, v) in r.headers() {
        if matches!(k.as_str(), "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "set-cookie") {
            continue;
        }
        resp = resp.header(k, v);
    }
    if !suspect {
        let resp = resp.body(Body::from_stream(r.bytes_stream()));
        return (false, resp.unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()));
    }
    let body = r.bytes().await.unwrap_or_default();
    let stale = if status == StatusCode::UNAUTHORIZED {
        serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.pointer("/error/code").and_then(Value::as_str).map(|c| c == "ACCESS_CODE_REQUIRED")).unwrap_or(false)
    } else {
        is_code_page(&body)
    };
    (stale, resp.body(Body::from(body)).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()))
}

/// 浏览器 ↔ 共享电脑：两边的消息原样转，任一边断了就都关掉。
async fn bridge<A, B>(browser: tokio_tungstenite::WebSocketStream<A>, peer: tokio_tungstenite::WebSocketStream<B>)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut bt, mut br) = browser.split();
    let (mut pt, mut pr) = peer.split();
    let up = async {
        while let Some(Ok(m)) = br.next().await {
            match m {
                Message::Text(_) | Message::Binary(_) => {
                    if pt.send(m).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = pt.close().await;
    };
    let down = async {
        while let Some(Ok(m)) = pr.next().await {
            match m {
                Message::Text(_) | Message::Binary(_) => {
                    if bt.send(m).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = bt.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{any, get, post};
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn urls() {
        assert_eq!(normalize_url("192.168.1.5:1234").unwrap(), "http://192.168.1.5:1234");
        assert_eq!(normalize_url(" https://1.2.3.4/some/path?x=1 ").unwrap(), "https://1.2.3.4");
        assert_eq!(normalize_url("https://pc.example.com:443").unwrap(), "https://pc.example.com");
        assert_eq!(normalize_url("HTTP://Box.LAN:80/").unwrap(), "http://box.lan");
        for bad in ["", "ftp://a", "http://u:p@a:1", "javascript:alert(1)", "http://"] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
        assert!(is_self("http://127.0.0.1:1234", 1234));
        assert!(is_self("http://localhost:1234", 1234));
        assert!(!is_self("http://127.0.0.1:1235", 1234));
        assert!(!is_self("http://192.168.1.5:1234", 1234));
        assert_eq!(clean_remark("  a\u{7}b  "), "ab");
        assert_eq!(clean_remark(&"字".repeat(40)).chars().count(), 32);
    }

    #[test]
    fn headers_and_pages() {
        let mut h = HeaderMap::new();
        for (k, v) in [("host", "127.0.0.1:1234"), ("cookie", "ll_access=mine"), ("origin", "http://127.0.0.1:1234"), ("x-ll-device", "x-1"), ("x-forwarded-for", "1.1.1.1"), ("accept", "a/b")] {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let out = outgoing(&h, "http://10.0.0.2:1234", &Some("ll_access=theirs".into()));
        assert_eq!(out.get("origin").unwrap(), "http://10.0.0.2:1234");
        assert_eq!(out.get("cookie").unwrap(), "ll_access=theirs");
        assert_eq!(out.get("x-looklook-client").unwrap(), "1");
        assert_eq!(out.get("accept").unwrap(), "a/b");
        assert!(out.get("host").is_none() && out.get("x-ll-device").is_none() && out.get("x-forwarded-for").is_none());

        let mut rh = reqwest::header::HeaderMap::new();
        rh.append("set-cookie", "other=1; Path=/".parse().unwrap());
        rh.append("set-cookie", "ll_direct=abc; HttpOnly; Secure".parse().unwrap());
        assert_eq!(take_cookie(&rh).as_deref(), Some("ll_direct=abc"));
        assert_eq!(code_page_error("<p class=\"e\">输错次数太多，请稍后再试。</p>"), "SHARED_PC_CODE_LOCKED");
        assert_eq!(code_page_error("访问码不对"), "SHARED_PC_CODE_WRONG");
        assert!(is_code_page(code_page_html().as_bytes()));
        assert!(!is_code_page(b"<html>terminal</html>"));
    }

    fn code_page_html() -> String {
        format!("<h1>输入访问码</h1><form method=\"post\" action=\"{}\"><input name=\"code\"></form>", crate::gateway::access::FORM_PATH)
    }

    /// 假的对端：开着访问码，Cookie 值可以换（模拟对方改了访问码）
    struct Fake {
        code: Mutex<String>,
        logins: AtomicUsize,
    }

    async fn fake_peer(f: Arc<Fake>) -> String {
        fn ok(f: &Fake, h: &HeaderMap) -> bool {
            let want = format!("ll_access=c-{}", lock(&f.code));
            h.get("cookie").and_then(|v| v.to_str().ok()).is_some_and(|c| c.split(';').any(|p| p.trim() == want))
        }
        let need = || (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": { "code": "ACCESS_CODE_REQUIRED", "params": {} } }))).into_response();
        let app = Router::new()
            .route(
                "/api/ping",
                get({
                    let f = f.clone();
                    move |h: HeaderMap| async move { if ok(&f, &h) { StatusCode::NO_CONTENT.into_response() } else { need() } }
                }),
            )
            .route(
                "/api/echo",
                post({
                    let f = f.clone();
                    move |h: HeaderMap, body: Bytes| async move {
                        if !ok(&f, &h) {
                            return need();
                        }
                        let origin = h.get("origin").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                        let host = h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                        (
                            [(header::SET_COOKIE, "ll_access=leak")],
                            axum::Json(json!({ "origin": origin, "host": host, "body": String::from_utf8_lossy(&body), "client": h.get("x-looklook-client").is_some() })),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/i/t1/",
                get({
                    let f = f.clone();
                    move |h: HeaderMap| async move {
                        if ok(&f, &h) {
                            ([(header::CONTENT_TYPE, "text/html")], "<html>terminal</html>").into_response()
                        } else {
                            ([(header::CONTENT_TYPE, "text/html")], code_page_html()).into_response()
                        }
                    }
                }),
            )
            .route(
                "/__looklook/access",
                post({
                    let f = f.clone();
                    move |axum::Form(m): axum::Form<HashMap<String, String>>| async move {
                        f.logins.fetch_add(1, Ordering::SeqCst);
                        let code = lock(&f.code).clone();
                        if m.get("code").map(String::as_str) == Some(code.as_str()) {
                            (StatusCode::SEE_OTHER, [(header::SET_COOKIE, format!("ll_access=c-{code}; HttpOnly; SameSite=Strict; Path=/")), (header::LOCATION, "/".into())])
                                .into_response()
                        } else {
                            (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "text/html")], "<p class=\"e\">访问码不对，请再看一下。</p>").into_response()
                        }
                    }
                }),
            )
            .route("/api/status", any(|| async { axum::Json(json!({ "device_name": "共享机", "target": "linux-x86_64" })) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn call(s: &Arc<SharedPcs>, id: &str, method: &str, path: &str, body: &str) -> (StatusCode, Bytes, HeaderMap) {
        let req = Request::builder().method(method).uri(path).header("content-length", body.len()).body(Body::from(body.to_string())).unwrap();
        let r = s.proxy(id, req, path.to_string()).await;
        let (st, h) = (r.status(), r.headers().clone());
        (st, axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap(), h)
    }

    #[tokio::test]
    async fn code_handshake_and_expiry() {
        let f = Arc::new(Fake { code: Mutex::new("ABCD-EFGH".into()), logins: AtomicUsize::new(0) });
        let url = fake_peer(f.clone()).await;
        let s = Arc::new(SharedPcs::new(Arc::new(Store::memory().unwrap())));

        // 没填访问码、填错都不加
        assert_eq!(s.add(&url, "", "", 1).await.unwrap_err().code, "SHARED_PC_CODE_REQUIRED");
        assert_eq!(s.add(&url, "nope", "", 1).await.unwrap_err().code, "SHARED_PC_CODE_WRONG");
        assert!(s.list().is_empty());

        let v = s.add(&url, "ABCD-EFGH", "", 1).await.unwrap();
        assert_eq!(v["has_code"], true);
        assert_eq!(v["name"], "共享机", "没填备注时用对方的电脑名");
        assert!(v.get("code").is_none(), "访问码不回给页面");
        assert!(!v.to_string().contains("ABCD"), "访问码不回给页面");
        let id = v["id"].as_str().unwrap().to_string();
        assert!(is_shared(&id));
        assert_eq!(s.add(&url, "ABCD-EFGH", "", 1).await.unwrap_err().code, "SHARED_PC_DUPLICATE");

        // 转发：对方看到的是自己的来源，拿到对方的 Cookie；对方发的 Set-Cookie 不转给浏览器
        let (st, body, h) = call(&s, &id, "POST", "/api/echo", "hi").await;
        assert_eq!(st, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["origin"], url.as_str());
        assert_eq!(format!("http://{}", v["host"].as_str().unwrap()), url);
        assert_eq!(v["body"], "hi");
        assert_eq!(v["client"], true);
        assert!(h.get("set-cookie").is_none());
        assert_eq!(f.logins.load(Ordering::SeqCst), 2, "一次填错 + 一次成功，之后复用 Cookie");

        // 对方改了访问码：用存下的旧码重新换不到 → 提示更新访问码
        *lock(&f.code) = "NEW-CODE".into();
        let (st, body, _) = call(&s, &id, "POST", "/api/echo", "hi").await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(String::from_utf8_lossy(&body).contains("SHARED_PC_CODE_WRONG"));
        assert_eq!(s.view().await[0]["problem"], "SHARED_PC_CODE_WRONG");

        // 用户更新访问码后，下一个请求重新换 Cookie 并成功；页面（访问码页面）同理
        s.update(&id, Some("前台"), Some("NEW-CODE")).await.unwrap();
        let (st, _, _) = call(&s, &id, "POST", "/api/echo", "again").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(s.view().await[0]["name"], "前台");

        // Cookie 过期（对方重启换了密钥）但访问码没变：自动重新换一次，请求照常成功
        let before = f.logins.load(Ordering::SeqCst);
        s.set_live(&id, |l| l.cookie = Some(Some("ll_access=stale".into())));
        let (st, body, _) = call(&s, &id, "GET", "/i/t1/", "").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(&body[..], b"<html>terminal</html>");
        assert_eq!(f.logins.load(Ordering::SeqCst), before + 1);

        // 设置同步的接口不转
        let (st, _, _) = call(&s, &id, "POST", "/api/sync/exchange", "{}").await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        s.remove(&id).unwrap();
        assert!(s.list().is_empty());
        assert_eq!(call(&s, &id, "GET", "/api/ping", "").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn open_peer_and_unreachable() {
        let app = Router::new().route("/api/ping", get(|| async { StatusCode::NO_CONTENT })).route("/api/status", get(|| async { axum::Json(json!({ "target": "darwin-aarch64", "settings": { "device_name": "" } })) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let s = Arc::new(SharedPcs::new(Arc::new(Store::memory().unwrap())));
        let v = s.add(&addr.to_string(), "", "  ", 1).await.unwrap();
        assert_eq!(v["has_code"], false);
        assert_eq!(v["os"], "darwin");
        assert_eq!(v["name"], addr.to_string(), "没有名字时显示地址");
        assert_eq!(s.add(&format!("http://127.0.0.1:{}", addr.port()), "", "", addr.port()).await.unwrap_err().code, "SHARED_PC_SELF");
        // 连不上的地址不加
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        assert_eq!(s.add(&dead.to_string(), "", "", 1).await.unwrap_err().code, "SHARED_PC_UNREACHABLE");
    }
}
