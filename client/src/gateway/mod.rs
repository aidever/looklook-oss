//! 本机网关：同一套路由挂在两个入口上。
//!
//! - **直接入口**（默认 `0.0.0.0:1234`）：浏览器直接连到这台机器打开管理台。按来源 IP 放行：
//!   本机总是可以；局域网、白名单要用户在设置里允许，并可要求访问码；Host 只接受本机、IP 与局域网名字
//!   （防 DNS 重绑定）。见 `access.rs`。
//! - **远程入口**（随机端口，rathole 的 `local_addr`）：每个请求必须带平台签名的会话凭证和引用它的
//!   网关令牌 v3（服务端 docs/04 §4.4，见 `remote.rs`）。`user_host` 进管理台/终端路由，`tunnel` 转到凭证里的本机端口。
//!
//! 路由：`/api/*` 本机接口；`/mcp` AI 助手用的 MCP 接口（只限本机，见 `mcp.rs`）；`/i/{id}/…` 终端（反向代理到 ttyd）；`/fonts/*` 终端字体；其余为管理台页面。

pub mod access;
mod api;

pub use api::create_session;
mod assets;
mod chat;
mod chat_mux;
mod input_tap;
mod live_ws;
mod notify;
pub mod assistant;
pub mod mcp;
pub mod plugins;
pub(crate) mod proxy;
pub mod remote;
pub(crate) mod term_zstd;
mod termd_ws;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get};
use axum::Router;
use looklook_protocol::edge_auth;
use looklook_protocol::gateway_token::{self, Kind};

use crate::account::Account;
use crate::error::LocalError;
use crate::instances::Instances;
use crate::paths::Paths;
use crate::store::Store;

pub use proxy::{client as proxy_client, HttpClient};
pub use api::local_only;

/// 请求从哪个入口进来。
#[derive(Debug, Clone)]
pub enum Access {
    Local,
    Remote { host: String, proto: String },
    /// 经 WebRTC 通道（服务端 docs/06）：只有已授权的 PeerConnection 才能发请求。`origin` 是稳定地址的源（平台签在授权票据里）。
    Rtc { origin: String },
}

impl Access {
    pub fn is_local(&self) -> bool {
        matches!(self, Access::Local)
    }
}

pub struct Inner {
    pub account: Arc<Account>,
    pub instances: Arc<Instances>,
    pub updater: Arc<crate::updater::Updater>,
    pub metrics: Arc<crate::metrics::Metrics>,
    /// 后台线路测速（自动选择、偏慢提示）
    pub relay_watch: Arc<crate::relay_watch::RelayWatch>,
    /// 终端文件传输：进行中的分块上传（docs/FILE_TRANSFER.md §4）
    pub transfers: Arc<crate::transfer::Transfers>,
    /// Claude Code 对话界面（docs/CLAUDE_UI.md）
    pub chat: Arc<crate::chat::Chat>,
    pub store: Arc<Store>,
    pub paths: Paths,
    pub ui_addr: SocketAddr,
    pub relay_addr: SocketAddr,
    pub http: HttpClient,
    /// 直接访问控制（局域网、白名单、访问码、打开地址），改设置时同步更新
    pub access: RwLock<access::AccessConfig>,
    pub limiter: access::Limiter,
    /// 白名单里域名（DDNS）的解析结果
    pub resolved: Arc<access::Resolved>,
    /// 这台机器的局域网 IP（启动时取一次，只用于显示）
    pub lan_ips: Vec<IpAddr>,
    /// 远程入口的凭证缓存与 jti 去重
    pub remote: remote::Verifier,
    /// 管理台经 WebRTC 的连接（src/rtc）
    pub rtc: Arc<crate::rtc::Rtc>,
    /// 本机 App 连同一账户其他电脑的连接（src/rtc/outbound.rs）
    pub outbound: Arc<crate::rtc::outbound::Outbound>,
    /// 从另一台电脑导入 SSH 主机：进行中的导出与导入（src/ssh_transfer.rs）
    pub ssh_transfer: crate::ssh_transfer::SshTransfer,
    /// 设置在电脑之间同步（src/settings_sync.rs）
    pub sync: Arc<crate::settings_sync::Syncer>,
    /// 飞书通知（src/notify.rs）
    pub notify: Arc<crate::notify::Notifier>,
    /// 插件（src/plugins.rs）
    pub plugins: Arc<crate::plugins::Plugins>,
    /// 公网 HTTPS 直连（src/direct_tls.rs）
    pub direct: Arc<crate::direct_tls::Direct>,
    /// 共享电脑：按地址加进来的其他看看（src/shared_pcs.rs）
    pub shared: Arc<crate::shared_pcs::SharedPcs>,
}

impl Inner {
    pub fn access(&self) -> access::AccessConfig {
        self.access.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 把浏览器打开地址写到 `run/open`（`looklook open`、托盘菜单使用），返回不带结尾 `/` 的地址。
    pub fn write_open_file(&self) -> std::io::Result<String> {
        let base = match self.access().open_host() {
            Some(h) => format!("http://{}:{}", h.url_host(), self.ui_addr.port()),
            None => format!("http://{}", self.ui_addr),
        };
        std::fs::write(self.paths.home.join("run").join("open"), &base)?;
        Ok(base)
    }

    /// 管理台的访问情况，界面据此显示端口、局域网地址与安全提醒。
    pub fn console_info(&self) -> serde_json::Value {
        let c = self.access();
        let lan_ips = access::own_lan_ips();
        serde_json::json!({
            "port": self.ui_addr.port(),
            "lan_ips": lan_ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
            "in_container": access::in_container(),
            "allow_lan": c.allow_lan,
            "allowed_ips": c.allowed_ips.len(),
            "code_enabled": c.code_enabled,
        })
    }
}

pub type App = Arc<Inner>;

// The platform reserves and strips X-LL-* before proxying browser requests.
pub const CLIENT_HEADER: &str = "x-looklook-client";

pub fn router(app: App) -> Router {
    Router::new()
        .nest("/api", api::routes(app.clone()))
        .route("/i/{id}", get(|axum::extract::Path(id): axum::extract::Path<String>| async move { Redirect::permanent(&format!("/i/{id}/")) }))
        .route("/i/{id}/", any(terminal))
        .route("/i/{id}/{*rest}", any(terminal))
        // 插件的页面和画面（`/p/<名>/…` → 插件的 Unix Socket），见 plugins.rs
        .merge(plugins::page_routes())
        .route(looklook_protocol::rtc::OFFER_PATH, axum::routing::post(crate::rtc::offer))
        .route(mcp::PATH, axum::routing::post(mcp::handle).get(mcp::not_allowed).delete(mcp::not_allowed))
        // 助理的大脑用的工具（只认它的令牌，见 assistant.rs）
        .route(assistant::MCP_PATH, axum::routing::post(assistant::mcp).get(assistant::mcp_not_allowed).delete(assistant::mcp_not_allowed))
        .route(crate::chat::hooks::PATH, axum::routing::post(chat::hook))
        .route("/fonts/{*path}", get(assets::font))
        .route("/static/term.js", get(assets::term_js))
        .route("/static/web.js", get(assets::web_js))
        .fallback(assets::ui)
        .layer(middleware::from_fn_with_state(app.clone(), remote_gate))
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

/// 本机、局域网打开不需要账户；远程打开（专属地址、WebRTC）要登录且会员有效。
/// 不能用时只留状态接口，远程页面据此说明原因、引导续费。
async fn remote_gate(State(app): State<App>, req: Request, next: Next) -> Response {
    let local = req.extensions().get::<Access>().is_none_or(Access::is_local);
    if local || req.uri().path() == "/api/status" {
        return next.run(req).await;
    }
    let gate = app.account.gate();
    if gate.allowed() {
        return next.run(req).await;
    }
    if req.uri().path().starts_with("/api/") {
        return LocalError::locked(gate.reason()).into_response();
    }
    let kind = match gate.reason() {
        Some("MEMBERSHIP_EXPIRED") => "locked_expired",
        Some(crate::account::OFFLINE_TOO_LONG) => "locked_offline",
        _ => "locked",
    };
    assets::message_page(StatusCode::FORBIDDEN, kind)
}

/// 管理台、终端页只允许同源嵌入（本机直连的终端页改用 CSP `frame-ancestors`，见 `terminal`）；地址里的一次性口令不随 Referer 泄露。
async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.entry("x-frame-options").or_insert(HeaderValue::from_static("SAMEORIGIN"));
    }
    h.entry("x-content-type-options").or_insert(HeaderValue::from_static("nosniff"));
    h.entry("referrer-policy").or_insert(HeaderValue::from_static("no-referrer"));
    r
}

/// 是否包含反向代理或内网穿透软件附加的转发头。
fn has_proxy_headers(h: &HeaderMap) -> bool {
    const KEYS: &[&str] = &[
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-real-ip",
        "cf-connecting-ip",
        "true-client-ip",
    ];
    KEYS.iter().any(|k| h.contains_key(*k))
}

/// Host 头主机名（去掉端口与方括号后）是否为本机回环。
fn is_loopback_host(name: &str) -> bool {
    let n = name.trim_end_matches('.');
    n.eq_ignore_ascii_case("localhost")
        || n.to_ascii_lowercase().ends_with(".localhost")
        || n.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// 直接入口：按来源 IP 放行，识别反代/穿透来源强制认证，检查 Host，需要时检查访问码。
pub async fn local_guard(State(app): State<App>, mut req: Request, next: Next) -> Response {
    use looklook_protocol::direct as dp;
    // 没有连接信息只会出现在测试里，按本机处理。
    let conn = req.extensions().get::<ConnectInfo<Conn>>().map(|c| c.0);
    let peer_ip = conn.map(|c| c.remote.ip().to_canonical()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let tls = conn.is_some_and(|c| c.tls);
    let path = req.uri().path();
    if req.method() == Method::GET && path.starts_with(dp::ACME_CHALLENGE_PREFIX) {
        return app.direct.challenge(path);
    }
    if req.method() == Method::GET && path == dp::PING_PATH && tls {
        return ([(header::CACHE_CONTROL, "no-store")], dp::PING_BODY).into_response();
    }

    let cfg = app.access();
    let custom_hosts = app.direct.custom_hosts();
    let req_host = host_name(req.headers());
    let proxied = has_proxy_headers(req.headers());
    let custom_match = req_host.and_then(|h| app.direct.match_custom_host(h));
    let is_custom_host = custom_match.is_some();
    let is_custom_public = custom_match.as_ref().is_some_and(|m| m.kind != looklook_protocol::direct::EndpointKind::Lan);
    let is_https_custom = custom_match.as_ref().is_some_and(|m| !m.explicit_http);

    // 真正的本机直连（免密）：不能是 TLS 端口转发，不能带反代头，也不能是穿透域名；
    // 回环来源还要求 Host 本身是 localhost / 127.0.0.1 / ::1（或用户设的本机 open_host），防止 frpc/cloudflared 转到 127.0.0.1 时绕过认证。
    let open_host = cfg.open_host();
    let loopback_host_ok = req_host.is_some_and(|h| is_loopback_host(h) || open_host.as_ref().is_some_and(|oh| oh.matches(&h.trim_end_matches('.').to_ascii_lowercase())));
    let genuine_loopback = peer_ip.is_loopback() && !tls && !proxied && !is_custom_public && loopback_host_ok;
    let same_host = !tls
        && !proxied
        && !is_custom_public
        && (conn.and_then(|c| c.local).is_some_and(|l| !peer_ip.is_loopback() && l.ip().to_canonical() == peer_ip) || access::local_from(peer_ip));

    let mut peer = if genuine_loopback || same_host {
        access::Peer::Local
    } else if is_custom_public || proxied {
        // 凡是匹配外部穿透/公网域名，或带反代转发头的请求，一律按公网入口强制要求认证，绝不当成免密本机或免密局域网
        if (tls && app.direct.accepts_public()) || is_custom_host {
            access::Peer::Public
        } else {
            access::Peer::Denied
        }
    } else {
        let mut p = access::classify(peer_ip, &cfg, false, &app.resolved.ips(&cfg));
        if p == access::Peer::Local {
            // peer_ip 是 127.0.0.1 但没过 genuine_loopback（例如 frp TCP 隧道带了外部 Host 或走 TLS）
            p = if (tls && app.direct.accepts_public()) || is_custom_host {
                access::Peer::Public
            } else {
                access::Peer::Denied
            };
        }
        if p == access::Peer::Denied && app.resolved.refresh_if_stale(&cfg).await {
            p = access::classify(peer_ip, &cfg, false, &app.resolved.ips(&cfg));
        }
        if p == access::Peer::Denied && tls && app.direct.accepts_public() {
            p = access::Peer::Public;
        }
        p
    };

    if !req_host.is_some_and(|n| access::host_allowed_with_custom(n, open_host.as_ref(), &custom_hosts)) {
        if peer == access::Peer::Denied {
            return assets::denied_page(&peer_ip.to_string());
        }
        return (StatusCode::FORBIDDEN, "forbidden host").into_response();
    }
    // 连通性探活：Host 已通过校验且来源不是 Denied（支持反代明文 HTTP 转发与局域网测试；反代漏传 Host 会在上面直接 403）
    if req.method() == Method::GET && path == dp::PING_PATH && peer != access::Peer::Denied {
        return ([(header::CACHE_CONTROL, "no-store")], dp::PING_BODY).into_response();
    }
    // 浏览器经本机回环、局域网/公网 TLS，或用户配置的 HTTPS 穿透/反代连 `/__looklook/mux`（凭平台签发的 DirectGrant 认证）
    if (tls || genuine_loopback || is_https_custom) && req.method() == Method::GET && req.uri().path() == dp::MUX_PATH {
        if peer == access::Peer::Denied && (is_https_custom || (tls && (cfg.allow_lan || app.direct.accepts_public()))) {
            peer = access::Peer::Public;
        }
        if peer != access::Peer::Denied {
            return crate::rtc::mux::entry(&app, peer_ip, tls, is_https_custom, req).await;
        }
    }
    if peer == access::Peer::Denied {
        return assets::denied_page(&peer_ip.to_string());
    }
    if (tls || is_https_custom) && req.uri().path() == dp::CLIENT_PATH {
        return direct_entry(&app, peer_ip, req).await;
    }
    let site = req.method() == Method::GET && req.uri().path().starts_with("/api/fs/site/");
    let secure_cookie = tls || is_https_custom || req.headers().get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    if !site && peer == access::Peer::Public && !app.direct.cookie_ok(cookie(req.headers(), crate::direct_tls::COOKIE), &cfg, &app.account) {
        if req.uri().path() == access::FORM_PATH && req.method() == Method::POST {
            return submit_public_code(&app, &cfg, peer_ip, secure_cookie, req).await;
        }
        if req.uri().path().starts_with("/api/") {
            return LocalError::new("ACCESS_CODE_REQUIRED").into_response();
        }
        let next_path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let short = (!crate::direct_tls::public_code_ok(&cfg)).then_some("short");
        return assets::code_page(next_path, short);
    }
    if !site && peer.needs_code(&cfg) && !cookie(req.headers(), access::COOKIE).is_some_and(|v| ct_eq(v, &cfg.cookie_value())) {
        if req.uri().path() == access::FORM_PATH && req.method() == Method::POST {
            return submit_code(&app, &cfg, peer_ip, req).await;
        }
        if req.uri().path().starts_with("/api/") {
            return LocalError::new("ACCESS_CODE_REQUIRED").into_response();
        }
        let next_path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        return assets::code_page(next_path, None);
    }
    req.extensions_mut().insert(Access::Local);
    req.extensions_mut().insert(peer);
    next.run(req).await
}

/// 访问码表单：对了就发 Cookie 并回到原来的页面。
async fn submit_code(app: &App, cfg: &access::AccessConfig, ip: IpAddr, req: Request) -> Response {
    if app.limiter.blocked(ip) {
        return assets::code_page("/", Some("too_many"));
    }
    use axum::extract::FromRequest;
    let form = axum::Form::<std::collections::HashMap<String, String>>::from_request(req, &()).await.map(|f| f.0).unwrap_or_default();
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or("");
    // 只回到本站的相对路径
    let next = Some(field("next")).filter(|n| n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\")).unwrap_or("/");
    if !cfg.code_matches(field("code")) {
        app.limiter.fail(ip);
        tracing::warn!(%ip, "访问码错误");
        return assets::code_page(next, Some(if app.limiter.blocked(ip) { "too_many" } else { "wrong" }));
    }
    app.limiter.clear(ip);
    let mut r = Redirect::to(next).into_response();
    let c = format!("{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000", access::COOKIE, cfg.cookie_value());
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).expect("cookie"));
    r
}

/// 公网来源提交访问码（docs/09 §4.3）：至少 12 位，按 IP 与全局限制失败次数，对了发 `ll_direct`。
async fn submit_public_code(app: &App, cfg: &access::AccessConfig, ip: IpAddr, secure_cookie: bool, req: Request) -> Response {
    use axum::extract::FromRequest;
    let form = axum::Form::<std::collections::HashMap<String, String>>::from_request(req, &()).await.map(|f| f.0).unwrap_or_default();
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or("");
    let next = Some(field("next")).filter(|n| n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\")).unwrap_or("/");
    match app.direct.submit_code_ext(cfg, &app.limiter, ip, field("code"), next, secure_cookie) {
        Ok(r) => r,
        Err(e) => assets::code_page(next, Some(e)),
    }
}

/// `/__looklook/direct`（docs/09 §5）：GET 返回页面；POST 提交平台签的票据（`{"grant": "..."}`）。
async fn direct_entry(app: &App, ip: IpAddr, req: Request) -> Response {
    if req.method() == Method::GET {
        return crate::direct_tls::grant_page();
    }
    if req.method() != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let origin = format!("https://{host}");
    // 只接受本页面自己发的请求
    if req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).is_some_and(|o| !o.eq_ignore_ascii_case(&origin)) {
        return LocalError::new("CSRF_FAILED").into_response();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 16 * 1024).await else { return StatusCode::PAYLOAD_TOO_LARGE.into_response() };
    #[derive(serde::Deserialize)]
    struct Body {
        grant: String,
    }
    let Ok(b) = serde_json::from_slice::<Body>(&body) else { return StatusCode::BAD_REQUEST.into_response() };
    app.direct.accept_grant(&app.account, ip, &origin, &b.grant)
}

/// 远程入口：验证会话凭证与网关令牌；隧道请求直接转发。
async fn relay_entry(app: App, inner: Router, mut req: Request) -> Response {
    let header = |h: &HeaderMap, name: &str| h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (cap, token) = (header(req.headers(), edge_auth::CAP_HEADER), header(req.headers(), gateway_token::HEADER));
    let remote = match app.remote.verify(&app.account, &cap, &token) {
        Ok(r) => r,
        Err(why) => {
            tracing::debug!(why, "远程请求未通过校验");
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };
    req.headers_mut().remove(gateway_token::HEADER);
    req.headers_mut().remove(edge_auth::CAP_HEADER);
    let proto = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|p| *p == "http" || *p == "https")
        .unwrap_or("https")
        .to_string();
    match remote.kind {
        Kind::Tunnel => proxy::tunnel(&app, req, &remote, &proto).await,
        Kind::UserHost => {
            req.extensions_mut().insert(remote.clone());
            req.extensions_mut().insert(Access::Remote { host: remote.host, proto });
            use tower::ServiceExt;
            inner.oneshot(req).await.into_response()
        }
    }
}

/// 终端：`/i/{id}/…` → 对应 ttyd 或 looklook-termd。
async fn terminal(State(app): State<App>, req: Request) -> Response {
    let access = req.extensions().get::<Access>().cloned().unwrap_or(Access::Local);
    let id = req.uri().path().trim_start_matches("/i/").split('/').next().unwrap_or("").to_string();
    let Some(endpoint) = app.instances.endpoint(&id).await else {
        return assets::message_page(StatusCode::SERVICE_UNAVAILABLE, "not_running");
    };
    if proxy::is_websocket(req.headers()) && !origin_matches(req.headers(), &access) {
        return (StatusCode::FORBIDDEN, "origin").into_response();
    }
    // 手机上本地翻看、选择文字用的回滚区（src-term/touch.js）。跟终端页同一个地址下，本机、中转、WebRTC 都能取到。
    if req.method() == Method::GET && req.uri().path() == format!("/i/{id}/ll-history") {
        let query = req.uri().query().unwrap_or("").to_string();
        let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse::<usize>().ok());
        let n = q("n").unwrap_or(2000);
        let since = q("since").zip(q("w"));
        return match app.instances.history(&id, n.min(10000), since).await {
            Some(v) => ([(header::CACHE_CONTROL, "no-store")], axum::Json(v)).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let index = req.method() == Method::GET && req.uri().path() == format!("/i/{id}/");
    if index {
        let boot = matches!(endpoint, crate::instances::InstanceEndpoint::Termd { .. }).then(|| termd_ws::term_boot_tag(&app, &id));
        if let Some(page) = assets::term_page(boot.as_deref()) {
            return page;
        }
    }

    match endpoint {
        crate::instances::InstanceEndpoint::Termd { session_id } => {
            let p = req.uri().path();
            if p == format!("/i/{id}/token") && req.method() == Method::GET {
                return ([(header::CACHE_CONTROL, "no-store")], axum::Json(serde_json::json!({ "token": "" }))).into_response();
            }
            if p == format!("/i/{id}/ws") && proxy::is_websocket(req.headers()) {
                return termd_ws::serve_legacy_ws(app, id, session_id, req).await;
            }
            if p == format!("/i/{id}/term") && proxy::is_websocket(req.headers()) {
                return termd_ws::serve_term_ws(app, id, session_id, req).await;
            }
            if p == format!("/i/{id}/term-history") {
                return termd_ws::serve_term_history(app, id, session_id, req).await;
            }
            assets::message_page(StatusCode::NOT_FOUND, "not_found")
        }
        crate::instances::InstanceEndpoint::Ttyd { port, auth } => {
            // Windows：ttyd 只接入会话，连上之前先确保会话在（见 Instances::command_for）
            if proxy::is_websocket(req.headers()) {
                app.instances.ensure_attachable(&id).await;
            }
            // 终端里按键、粘贴记成用户输入（会话列表按它排，见 Instances::note_input）
            let insts = app.instances.clone();
            let on_input: input_tap::OnInput = std::sync::Arc::new(move || insts.note_input(&id));
            let opts = proxy::Opts { auth: Some(auth), host: None, inject: index.then_some(proxy::TERM_TAGS), rewrite_location: None, terminal: true, on_input: Some(on_input), keep: None };
            proxy::forward(&app.http, req, port, opts).await
        }
    }
}

/// 写请求与 WebSocket 的来源检查：`Origin` 必须是当前站点本身。
pub fn origin_matches(h: &HeaderMap, access: &Access) -> bool {
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // 非浏览器请求没有 Origin；本机入口已有访问口令，远程入口已有网关令牌。
        return true;
    };
    match access {
        // 同一个端口同时提供 HTTP 和 HTTPS（docs/09 §4.2），两个源都是这台电脑自己的页面
        Access::Local => {
            let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
            origin.eq_ignore_ascii_case(&format!("http://{host}")) || origin.eq_ignore_ascii_case(&format!("https://{host}"))
        }
        // 平台转发的主机名不带端口（开发环境的非标准端口会出现在 Origin 里），只比较协议与主机名。
        Access::Remote { host, proto } => origin
            .split_once("://")
            .is_some_and(|(scheme, rest)| scheme.eq_ignore_ascii_case(proto) && strip_port(rest).eq_ignore_ascii_case(strip_port(host))),
        // 通道上的请求由 Service Worker 转来，浏览器不会带 Origin；带了就必须是稳定地址本身。
        Access::Rtc { origin: own, .. } => origin.eq_ignore_ascii_case(own),
    }
}

fn strip_port(hostport: &str) -> &str {
    match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => hostport,
    }
}

/// 请求头 Host 里的主机名（去掉端口与 IPv6 方括号）。
fn host_name(h: &HeaderMap) -> Option<&str> {
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok())?;
    Some(if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(n, _)| n).unwrap_or(host)
    })
}



pub fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name).and_then(|r| r.strip_prefix('=')))
}

pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 连接两端的地址：判断“从这台机器自己连过来”（两端同一个 IP）要用到本端地址。
#[derive(Debug, Clone, Copy)]
pub struct Conn {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
    /// 这条连接是 HTTPS（公网直连，docs/09）
    pub tls: bool,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for Conn {
    fn connect_info(s: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        // 每个新连接调用一次，顺便关掉 Nagle：终端按键与回显都是小包，不能攒着等确认
        // （macOS / Windows 的回环上能多等几十到两百毫秒）。
        let _ = s.io().set_nodelay(true);
        Conn { remote: *s.remote_addr(), local: s.io().local_addr().ok(), tls: false }
    }
}

pub async fn serve(listener: tokio::net::TcpListener, app: axum::Router, shutdown: tokio_util::sync::CancellationToken) {
    // 带上连接的来源地址，直接入口按来源 IP 放行。
    let r = axum::serve(listener, app.into_make_service_with_connect_info::<Conn>()).with_graceful_shutdown(async move { shutdown.cancelled().await }).await;
    if let Err(e) = r {
        tracing::error!(error = %e, "网关退出");
    }
}

/// 管理台入口（docs/09 §4.2）：同一个端口同时提供 HTTP 和 HTTPS。每条连接先看第一个字节，`0x16`（TLS ClientHello）
/// 就做 TLS 握手（证书由 [`crate::direct_tls`] 热替换），其余按明文 HTTP 处理。没有证书时 TLS 握手失败，和以前一样只有 HTTP。
pub async fn serve_console(listener: tokio::net::TcpListener, app: axum::Router, direct: Arc<crate::direct_tls::Direct>, shutdown: tokio_util::sync::CancellationToken) {
    let acceptor = tokio_rustls::TlsAcceptor::from(direct.tls_config());
    loop {
        let (stream, remote) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    // 文件描述符用完之类：歇一下再接
                    tracing::debug!(error = %e, "管理台 accept 失败");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = shutdown.cancelled() => break,
        };
        let _ = stream.set_nodelay(true);
        let local = stream.local_addr().ok();
        let (app, acceptor, shutdown) = (app.clone(), acceptor.clone(), shutdown.clone());
        tokio::spawn(async move {
            let mut first = [0u8; 1];
            // 浏览器预连接的 socket 可能过一会儿才发请求
            let tls = match tokio::time::timeout(std::time::Duration::from_secs(60), stream.peek(&mut first)).await {
                Ok(Ok(1)) => first[0] == 0x16,
                _ => return,
            };
            let conn = Conn { remote, local, tls };
            if tls {
                let Ok(Ok(s)) = tokio::time::timeout(std::time::Duration::from_secs(15), acceptor.accept(stream)).await else { return };
                serve_conn(s, conn, app, shutdown).await;
            } else {
                serve_conn(stream, conn, app, shutdown).await;
            }
        });
    }
}

async fn serve_conn<S>(io: S, conn: Conn, app: axum::Router, shutdown: tokio_util::sync::CancellationToken)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::ServiceExt;
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        req.extensions_mut().insert(ConnectInfo(conn));
        app.clone().oneshot(req.map(axum::body::Body::new))
    });
    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    let c = builder.serve_connection_with_upgrades(TokioIo::new(io), svc);
    tokio::pin!(c);
    tokio::select! {
        _ = c.as_mut() => {}
        _ = shutdown.cancelled() => {
            c.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c).await;
        }
    }
}

pub fn local_app(app: App) -> Router {
    router(app.clone())
        .merge(plugins::host_routes().with_state(app.clone()))
        .layer(middleware::from_fn_with_state(app.clone(), to_peer))
        .layer(middleware::from_fn_with_state(app, local_guard))
}

/// 本机管理台发给另一台电脑的请求（服务端 docs/06 §14）：带 `X-LL-Device`（不是本机），或者是那台电脑上的终端
/// （`/i/{id}…`、`/api/instances/{id}…`，按终端路由表与地址上的 `__lldev`），经 WebRTC 转给它（`rtc::outbound`）。
/// 只转发这台电脑自己（`Peer::Local`）打开的管理台的请求；写请求与 WebSocket 先在这里做和本机接口一样的来源检查，
/// 那台电脑那边不再看来源。
async fn to_peer(State(app): State<App>, req: Request, next: Next) -> Response {
    use crate::rtc::outbound;
    if req.extensions().get::<access::Peer>() != Some(&access::Peer::Local) {
        return next.run(req).await;
    }
    // 本机桌面窗口/RTC 接口、给插件的宿主接口只在本机处理，绝不转发到其他电脑
    let uri_path = req.uri().path();
    if uri_path.starts_with("/api/desktop/") || uri_path.starts_with("/api/rtc/") || uri_path.starts_with(looklook_protocol::plugin::HOST_PREFIX) {
        return next.run(req).await;
    }
    // 只看本机：一律在本机处理（页面也不会再指定别的电脑）
    if api::local_only(&app) {
        return next.run(req).await;
    }
    // 没登录也能用共享电脑（crate::shared_pcs），这时本机在页面上是 `self`
    let session = app.account.session();
    if session.is_none() && !app.shared.any() {
        return next.run(req).await;
    }
    let own = session.as_ref().map(|s| s.device_id.clone()).unwrap_or_else(|| "self".into());
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let (path, hint) = outbound::take_device_hint(&pq);
    let terminal = outbound::terminal_of(req.uri().path()).map(str::to_owned);
    // 本机已有的会话一律走 127.0.0.1 本机直连，不记成别的电脑、也不经 WebRTC 转发
    if let Some(id) = terminal.as_deref() {
        if app.instances.exists(id) {
            app.outbound.remember(id, &own);
            return next.run(req).await;
        }
    }
    // 插件（`/p/<名>/…`、`/api/plugins/<名>/…`）：插件页面在 iframe 里，第一次打开的地址带 `__lldev`，
    // 之后页面自己的请求（静态文件、接口、画面）不带，按记下的那台电脑转
    let route_key = terminal.clone().or_else(|| looklook_protocol::plugin::plugin_of_path(req.uri().path()).map(|n| format!("p:{n}")));
    if let (Some(k), Some(d)) = (&route_key, &hint) {
        app.outbound.remember(k, d);
    }
    let header_dev = req.headers().get(looklook_protocol::DEVICE_HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let target = header_dev.or(hint).or_else(|| route_key.as_deref().and_then(|k| app.outbound.owner(k)));
    let Some(dev) = target.filter(|d| *d != own && d != "self") else { return next.run(req).await };
    let ws = req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if ws || !matches!(*req.method(), Method::GET | Method::HEAD) {
        let marked = ws || req.headers().get(CLIENT_HEADER).and_then(|v| v.to_str().ok()) == Some("1");
        if !marked || !origin_matches(req.headers(), &Access::Local) {
            return LocalError::new("CSRF_FAILED").into_response();
        }
    }
    if crate::shared_pcs::is_shared(&dev) {
        return app.shared.proxy(&dev, req, path).await;
    }
    if session.is_none() {
        return next.run(req).await;
    }
    let link = match app.outbound.link(&app.account, &dev).await {
        Ok(l) => l,
        Err(e) => return e.response(),
    };
    if ws {
        outbound::websocket(link, req, path).await
    } else {
        outbound::http(link, req, path).await
    }
}

pub fn relay_app(app: App) -> Router {
    let inner = router(app.clone());
    let revoke = {
        let app = app.clone();
        move |headers: HeaderMap, body: axum::body::Bytes| crate::rtc::revoke(State(app), headers, body)
    };
    Router::new()
        .route(looklook_protocol::PRESENCE_PATH, get(presence))
        .route(looklook_protocol::rtc::REVOKE_PATH, axum::routing::post(revoke))
        .fallback(move |req: Request| {
            let (app, inner) = (app.clone(), inner.clone());
            async move { relay_entry(app, inner, req).await }
        })
}

/// 设备在线（中转经通道请求，见 `looklook_protocol::PRESENCE_PATH`）：连上就立即回一个心跳，之后定时回，
/// 中转收到第一个心跳就把这台电脑标为在线，连接断了、心跳停了就马上标为离线。
/// 只有中转本机能连到这个端口，内容也只是心跳，不需要令牌。
/// 新中转会定期重新请求，并在请求头里说明周期：记下来给通道看门狗（`relay::presence_seen`）。
async fn presence(headers: HeaderMap) -> Response {
    use futures_util::StreamExt;
    let cycle = headers.get(looklook_protocol::PRESENCE_CYCLE_HEADER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
    crate::relay::presence_seen(cycle);
    let beats = futures_util::stream::unfold(true, |first| async move {
        if !first {
            tokio::time::sleep(std::time::Duration::from_secs(looklook_protocol::PRESENCE_BEAT_SECS)).await;
        }
        Some((Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"1\n")), false))
    })
    .boxed();
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/plain"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::HeaderName::from_static(looklook_protocol::PRESENCE_HEADER), "1"),
        ],
        axum::body::Body::from_stream(beats),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        m
    }

    /// 本机、局域网不看账户；远程打开要登录且会员有效，不能用时只留状态接口。
    #[tokio::test]
    async fn remote_needs_account_local_does_not() {
        use tower::ServiceExt;
        let home = tempfile::tempdir().unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let a = crate::ssh_transfer::e2e::app(home.path(), "http://127.0.0.1:9", "deva", key);
        let call = |access: Access, path: &str| {
            let mut req = Request::builder().uri(path).body(axum::body::Body::empty()).unwrap();
            req.extensions_mut().insert(access);
            let r = router(a.clone());
            async move {
                let r = r.oneshot(req).await.unwrap();
                let status = r.status();
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                (status, serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(), String::from_utf8_lossy(&body).into_owned())
            }
        };
        let remote = || Access::Remote { host: "alice.looklook.test".into(), proto: "https".into() };
        let rtc = || Access::Rtc { origin: "https://alice.looklook.test".into() };

        let (st, _, _) = call(remote(), "/api/settings").await;
        assert_eq!(st, StatusCode::OK, "会员有效时远程照常");

        let mut s = a.account.session().unwrap();
        s.expires_ms = 1;
        a.account.set_for_test(s, &[]);
        let (_, v, _) = call(Access::Local, "/api/status").await;
        assert_eq!((v["account"]["allowed"].as_bool(), v["account"]["remote_allowed"].as_bool()), (Some(true), Some(false)));
        let (st, _, _) = call(Access::Local, "/api/settings").await;
        assert_eq!(st, StatusCode::OK, "会员到期不影响本机");
        let (st, v, _) = call(remote(), "/api/status").await;
        assert_eq!((st, v["account"]["allowed"].as_bool(), v["account"]["reason"].as_str()), (StatusCode::OK, Some(false), Some("MEMBERSHIP_EXPIRED")));
        for access in [remote(), rtc()] {
            let (st, v, _) = call(access, "/api/settings").await;
            assert_eq!((st, v["error"]["code"].as_str(), v["error"]["params"]["reason"].as_str()), (StatusCode::FORBIDDEN, Some("LOCKED"), Some("MEMBERSHIP_EXPIRED")));
        }
        let (st, _, html) = call(remote(), "/i/abcdefgh/").await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(html.contains("会员已到期"), "{html}");

        a.account.clear_for_test();
        let (_, v, _) = call(Access::Local, "/api/status").await;
        assert_eq!((v["account"]["logged_in"].as_bool(), v["account"]["allowed"].as_bool()), (Some(false), Some(true)));
        for path in ["/api/settings", "/api/instances", "/api/notify"] {
            let (st, v, _) = call(Access::Local, path).await;
            assert_eq!(st, StatusCode::OK, "没登录也能在本机用：{path} {v}");
        }
        let (_, v, _) = call(Access::Local, "/api/tunnels").await;
        assert_eq!(v["error"]["code"].as_str(), Some("NOT_LOGGED_IN"), "本机网页是远程功能");
        let (st, v, _) = call(remote(), "/api/instances").await;
        assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("NOT_LOGGED_IN")));
    }

    #[test]
    fn host_names() {
        assert_eq!(host_name(&h(&[("host", "127.0.0.1:1234")])), Some("127.0.0.1"));
        assert_eq!(host_name(&h(&[("host", "[::1]:1234")])), Some("::1"));
        assert_eq!(host_name(&h(&[("host", "nas.local")])), Some("nas.local"));
        assert_eq!(host_name(&h(&[])), None);
    }

    #[test]
    fn origins() {
        let local = Access::Local;
        assert!(origin_matches(&h(&[("host", "127.0.0.1:1234"), ("origin", "http://127.0.0.1:1234")]), &local));
        assert!(!origin_matches(&h(&[("host", "127.0.0.1:1234"), ("origin", "https://evil.example")]), &local));
        assert!(origin_matches(&h(&[("host", "127.0.0.1:1234")]), &local));
        let remote = Access::Remote { host: "alice.looklook.example".into(), proto: "https".into() };
        assert!(origin_matches(&h(&[("origin", "https://alice.looklook.example")]), &remote));
        assert!(!origin_matches(&h(&[("origin", "https://alice--k7m2q9xa.looklook.example")]), &remote));
        assert!(!origin_matches(&h(&[("origin", "http://alice.looklook.example")]), &remote));
        assert!(!origin_matches(&h(&[("origin", "https://alice.looklook.example.evil.example")]), &remote));
        let dev = Access::Remote { host: "carol.4.looklook.example".into(), proto: "http".into() };
        assert!(origin_matches(&h(&[("origin", "http://carol.4.looklook.example:31235")]), &dev));
    }

    #[test]
    fn cookies() {
        let m = h(&[("cookie", "a=1; ll_local=xyz; b=2")]);
        assert_eq!(cookie(&m, "ll_local"), Some("xyz"));
        assert_eq!(cookie(&m, "ll_loc"), None);
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
    }

    /// 验证统一入口边界：
    /// 1. 本机真实回环免密；
    /// 2. 穿透/反代从 127.0.0.1 转发（带外部 Host 或 X-Forwarded-For）绝不免密，强制要求公网认证；
    /// 3. 局域网在 `allow_lan=true, code_enabled=false` 时免密不误锁；
    /// 4. 公网即使在 `code_enabled=false` 时也始终强制要求 ≥12 位访问码或平台票据。
    #[tokio::test]
    async fn local_guard_enforces_tunnel_and_public_auth_without_locking_lan() {
        use std::net::SocketAddr;
        use tower::ServiceExt;

        let home = tempfile::tempdir().unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let a = crate::ssh_transfer::e2e::app(home.path(), "http://127.0.0.1:9", "deva", key);

        // 开启局域网直连（code_enabled 保持默认 false），开启公网直连，添加一条穿透地址
        {
            let mut c = a.access();
            c.allow_lan = true;
            c.code_enabled = false;
            c.save(&a.store).unwrap();
            *a.access.write().unwrap() = c;
        }
        a.direct.set_enabled(true).unwrap();
        a.direct.set_custom_urls(&["https://code.example.com".into()]).unwrap();

        let send = |remote: &str, tls: bool, headers: &[(&str, &str)], path: &str| {
            let app = local_app(a.clone());
            let remote_addr: SocketAddr = remote.parse().unwrap();
            let mut b = Request::builder().uri(path);
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            let mut req = b.body(axum::body::Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(Conn { remote: remote_addr, local: None, tls }));
            async move {
                let res = app.oneshot(req).await.unwrap();
                let st = res.status();
                let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
                (st, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default())
            }
        };

        // 1) 真实本机回环免密通过
        let (st, _) = send("127.0.0.1:54321", false, &[("host", "127.0.0.1:1234")], "/api/settings").await;
        assert_eq!(st, StatusCode::OK, "本机回环应免密放行");

        // 2) cloudflared / Caddy 转到 127.0.0.1（Host 是穿透域名）必须拦截要访问码
        let (st, v) = send(
            "127.0.0.1:54322",
            false,
            &[("host", "code.example.com"), ("x-forwarded-for", "198.51.100.8"), ("x-forwarded-proto", "https")],
            "/api/settings",
        )
        .await;
        assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("ACCESS_CODE_REQUIRED")), "穿透域名经回环转发绝不能免密");

        // 3) frpc TCP 隧道不改 Host 但也不带反代头？若带外部 Host（如 home.example.com）且未配白名单则拒绝；配进 custom_urls 则走 Public 认证
        let (st, _) = send("127.0.0.1:54323", false, &[("host", "code.example.com")], "/api/settings").await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "就算反代没加 X-Forwarded-For，只要是穿透域名也必须要求认证");

        // 4) 反代把 Host 改写成 127.0.0.1 但带了 X-Forwarded-For：拒绝免密
        let (st, _) = send(
            "127.0.0.1:54324",
            false,
            &[("host", "127.0.0.1:1234"), ("x-forwarded-for", "198.51.100.8")],
            "/api/settings",
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "带反代转发头伪装 127.0.0.1 必须拒绝");

        // 5) 局域网设备（含 Tailscale 100.64/10）在 code_enabled=false 时免密放行
        let (st, _) = send("192.168.1.50:54325", false, &[("host", "192.168.1.2:1234")], "/api/settings").await;
        assert_eq!(st, StatusCode::OK, "局域网直连默认免密，避免误锁");
        let (st, _) = send("100.88.1.9:54326", false, &[("host", "100.88.1.2:1234")], "/api/settings").await;
        assert_eq!(st, StatusCode::OK, "Tailscale CGNAT 网段按虚拟局域网直连处理");

        // 6) 公网 IP 经 TLS 连入：即使 code_enabled=false 也强制要求访问码
        let (st, v) = send("203.0.113.9:54327", true, &[("host", "203.0.113.1:1234")], "/api/settings").await;
        assert_eq!((st, v["error"]["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("ACCESS_CODE_REQUIRED")));
    }
}
