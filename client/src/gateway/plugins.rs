//! 通用插件网关：
//! - 管理接口：`/api/plugins`、`/api/plugins/_manage/{name}/{install,uninstall,start,stop}`、`/api/plugins/_capabilities/open-url`
//! - 反向代理：`/api/plugins/{name}/{*rest}`、`/p/{name}/…` → 插件 Unix socket（原样保留路径前缀，支持 WebSocket 与 HTML `web.js` 注入）
//! - 宿主白名单接口（仅本机 `local_app` 挂载，不经中转/WebRTC）：`/_host/info`、`/_host/notify`、`/_host/license`、`/_host/data`

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use looklook_protocol::plugin::{self as proto, HostInfo, HostNotify, LicenseRequest, ReadonlyPolicy};
use serde::Deserialize;
use serde_json::{json, Value};

use super::access::Peer;
use super::{assets, origin_matches, proxy, Access, App};
use crate::account::VERSION;
use crate::error::LocalError;
use crate::paths;

type R<T> = Result<T, LocalError>;

/// 标明当前请求属于只读访问（如未来的只读分享会话），供网关按 `manifest.readonly_policy` 兜底拦截。
#[derive(Debug, Clone, Copy)]
pub struct ReadOnly(pub bool);

/// 挂在 `/api` 下（继承 CSRF `guard` 中间件）。
pub fn api_routes() -> Router<App> {
    Router::new()
        .route("/plugins", get(list_plugins))
        .route("/plugins/_capabilities/open-url", get(open_url_providers))
        .route("/plugins/_manage/{name}/install", post(install_plugin))
        .route("/plugins/_manage/{name}/uninstall", post(uninstall_plugin))
        .route("/plugins/_manage/{name}/start", post(start_plugin))
        .route("/plugins/_manage/{name}/stop", post(stop_plugin))
        .route("/plugins/{name}/{*rest}", any(api_proxy))
}

/// 挂在 `local_app` 顶层（只接受本机直连 + 运行中插件的随机 `HDR_TOKEN`）。
pub fn host_routes() -> Router<App> {
    Router::new()
        .route("/_host/info", get(host_info))
        .route("/_host/notify", post(host_notify))
        .route("/_host/license", post(host_license))
        .route("/_host/data", get(host_data))
}

fn de_bool_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    Ok(matches!(s.as_deref(), Some("1" | "true" | "TRUE" | "yes")))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default, deserialize_with = "de_bool_lenient")]
    catalog: bool,
}

async fn list_plugins(State(app): State<App>, Query(q): Query<ListQuery>) -> Json<Value> {
    let installed = app.plugins.brief();
    let jobs = app.plugins.all_jobs();
    let (catalog, catalog_error) = if q.catalog {
        match app.account.plugin_catalog(paths::target()).await {
            Ok(c) => (Some(c.plugins), None),
            Err(e) => {
                let le: LocalError = e.into();
                (None, Some(le.code))
            }
        }
    } else {
        (None, None)
    };
    Json(json!({
        "target": paths::target(),
        "protocol": proto::PROTOCOL_VERSION,
        "installed": installed,
        "catalog": catalog,
        "catalog_error": catalog_error,
        "jobs": jobs,
    }))
}

#[derive(Deserialize, Default)]
struct OpenUrlQuery {
    /// 为 true 时向已安装的 `open_url` 插件拉取实时 targets（必要时按需拉起插件）。
    #[serde(default, deserialize_with = "de_bool_lenient")]
    fetch: bool,
}

async fn open_url_providers(State(app): State<App>, headers: HeaderMap, Query(q): Query<OpenUrlQuery>) -> Json<Value> {
    let locale = headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut providers = Vec::new();
    for p in app.plugins.list_installed() {
        if p.incompatible.is_some() {
            continue;
        }
        let Some(cap) = p.manifest.capabilities.open_url.clone() else { continue };
        let targets = if q.fetch {
            let path = proto::api_url(&p.manifest.name, &cap.targets);
            match app.plugins.call_json(&p.manifest.name, Method::GET, &path, locale.as_deref(), None).await {
                Ok((StatusCode::OK, v)) => Some(v),
                _ => None,
            }
        } else {
            None
        };
        providers.push(json!({
            "name": p.manifest.name,
            "icon": p.manifest.icon,
            "title": cap.title,
            "targets_path": proto::api_url(&p.manifest.name, &cap.targets),
            "open_path": proto::api_url(&p.manifest.name, &cap.open),
            "data": targets,
        }));
    }
    Json(json!({ "providers": providers }))
}

async fn install_plugin(State(app): State<App>, Path(name): Path<String>) -> R<impl IntoResponse> {
    let job = app.plugins.start_install(app.account.clone(), name)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job": job }))))
}

#[derive(Deserialize, Default)]
struct UninstallReq {
    #[serde(default)]
    remove_data: bool,
}

async fn uninstall_plugin(State(app): State<App>, Path(name): Path<String>, body: Option<Json<UninstallReq>>) -> R<Json<Value>> {
    let remove_data = body.is_some_and(|b| b.remove_data);
    app.plugins.uninstall(&name, remove_data).await?;
    Ok(Json(json!({ "installed": app.plugins.brief() })))
}

async fn start_plugin(State(app): State<App>, Path(name): Path<String>) -> R<Json<Value>> {
    let _ = app.plugins.ensure_running(&name).await?;
    Ok(Json(json!({ "installed": app.plugins.brief() })))
}

async fn stop_plugin(State(app): State<App>, Path(name): Path<String>) -> R<Json<Value>> {
    app.plugins.stop(&name).await;
    Ok(Json(json!({ "installed": app.plugins.brief() })))
}

fn access_label(access: &Access, peer: Option<&Peer>) -> proto::Access {
    match access {
        Access::Local => match peer {
            Some(Peer::Local) | None => proto::Access::Local,
            _ => proto::Access::Lan,
        },
        Access::Remote { .. } => proto::Access::Remote,
        Access::Rtc { .. } => proto::Access::Rtc,
    }
}

fn check_readonly(policy: ReadonlyPolicy, readonly: bool, method: &Method, ws: bool) -> bool {
    if !readonly {
        return true;
    }
    match policy {
        ReadonlyPolicy::Deny => false,
        ReadonlyPolicy::ReadMethods => !ws && matches!(*method, Method::GET | Method::HEAD),
        ReadonlyPolicy::Plugin => true,
    }
}

fn apply_plugin_headers(req: &mut Request, token: &str, access: proto::Access, readonly: bool) {
    let h = req.headers_mut();
    // 防止浏览器伪造任何插件内部头
    h.remove(proto::HDR_TOKEN);
    h.remove(proto::HDR_ACCESS);
    h.remove(proto::HDR_READONLY);
    h.remove(header::COOKIE);
    h.remove(header::AUTHORIZATION);
    h.remove(header::REFERER);
    if let Ok(v) = HeaderValue::from_str(token) {
        h.insert(HeaderName::from_static(proto::HDR_TOKEN), v);
    }
    h.insert(HeaderName::from_static(proto::HDR_ACCESS), HeaderValue::from_static(access.as_str()));
    if readonly {
        h.insert(HeaderName::from_static(proto::HDR_READONLY), HeaderValue::from_static("1"));
    }
    if !h.contains_key(proto::HDR_LOCALE) {
        if let Some(lang) = h.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).and_then(|s| s.split(',').next()).map(str::trim) {
            if let Ok(v) = HeaderValue::from_str(lang) {
                h.insert(HeaderName::from_static(proto::HDR_LOCALE), v);
            }
        }
    }
}

/// `/api/plugins/{name}/{*rest}` → 插件（恢复 `/api` 前缀原样转给插件）。
async fn api_proxy(State(app): State<App>, Path((name, _rest)): Path<(String, String)>, mut req: Request) -> Response {
    let access = req.extensions().get::<Access>().cloned().unwrap_or(Access::Local);
    let peer = req.extensions().get::<Peer>().copied();
    let readonly = req.extensions().get::<ReadOnly>().is_some_and(|r| r.0);
    let ws = proxy::is_websocket(req.headers());
    if ws && !origin_matches(req.headers(), &access) {
        return LocalError::new("CSRF_FAILED").into_response();
    }
    let handle = match app.plugins.ensure_running(&name).await {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    if !check_readonly(handle.manifest.readonly_policy, readonly, req.method(), ws) {
        return LocalError::new("FORBIDDEN").into_response();
    }
    // Axum `.nest("/api", ...)` 去掉了 `/api`，恢复为协议约定的完整路径 `/api/plugins/<名>/…`
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let full = if pq.starts_with("/api/") { pq.to_string() } else { format!("/api{pq}") };
    if let Ok(uri) = full.parse() {
        *req.uri_mut() = uri;
    }
    apply_plugin_headers(&mut req, &handle.token, access_label(&access, peer.as_ref()), readonly);
    let opts = proxy::Opts {
        auth: None,
        host: Some("localhost".into()),
        inject: None,
        rewrite_location: None,
        terminal: false,
        on_input: None,
        keep: Some(Box::new(handle.guard)),
    };
    proxy::forward_unix(req, &handle.socket, opts).await
}

/// `/p/{name}/` 与 `/p/{name}/{*rest}` → 插件（页面、静态资源、画面 WebSocket）。
pub async fn page_proxy(State(app): State<App>, mut req: Request) -> Response {
    let access = req.extensions().get::<Access>().cloned().unwrap_or(Access::Local);
    let peer = req.extensions().get::<Peer>().copied();
    let readonly = req.extensions().get::<ReadOnly>().is_some_and(|r| r.0);
    let ws = proxy::is_websocket(req.headers());
    if (ws || !matches!(*req.method(), Method::GET | Method::HEAD)) && req.headers().contains_key(header::ORIGIN) && !origin_matches(req.headers(), &access) {
        return (StatusCode::FORBIDDEN, "origin").into_response();
    }
    let Some(name) = proto::plugin_of_path(req.uri().path()).map(str::to_owned) else {
        return assets::message_page(StatusCode::NOT_FOUND, "not_running");
    };
    let handle = match app.plugins.ensure_running(&name).await {
        Ok(h) => h,
        Err(_) => return assets::message_page(StatusCode::NOT_FOUND, "not_running"),
    };
    if !check_readonly(handle.manifest.readonly_policy, readonly, req.method(), ws) {
        return (StatusCode::FORBIDDEN, "readonly").into_response();
    }
    let inject = (req.method() == Method::GET && handle.manifest.wants_web_inject(req.uri().path())).then_some(proxy::WEB_TAGS);
    apply_plugin_headers(&mut req, &handle.token, access_label(&access, peer.as_ref()), readonly);
    if req.headers().contains_key(header::ORIGIN) {
        req.headers_mut().insert(header::ORIGIN, HeaderValue::from_static("http://localhost"));
    }
    let opts = proxy::Opts {
        auth: None,
        host: Some("localhost".into()),
        inject,
        rewrite_location: Some((vec!["http://localhost".into()], String::new())),
        terminal: false,
        on_input: None,
        keep: Some(Box::new(handle.guard)),
    };
    proxy::forward_unix(req, &handle.socket, opts).await
}

// ---------------------------------------------------------------- 宿主白名单接口（/_host/*）

fn auth_plugin(app: &App, peer: Option<Extension<Peer>>, headers: &HeaderMap) -> R<String> {
    if peer.is_some_and(|p| p.0 != Peer::Local) {
        return Err(LocalError::new("FORBIDDEN"));
    }
    let tok = headers.get(proto::HDR_TOKEN).and_then(|v| v.to_str().ok()).unwrap_or("");
    app.plugins.verify_host_token(tok).ok_or_else(|| LocalError::new("UNAUTHENTICATED"))
}

async fn host_info(State(app): State<App>, peer: Option<Extension<Peer>>, headers: HeaderMap) -> R<Json<HostInfo>> {
    let _plugin = auth_plugin(&app, peer, &headers)?;
    let session = app.account.session();
    Ok(Json(HostInfo {
        protocol: proto::PROTOCOL_VERSION,
        client_version: VERSION.to_string(),
        target: paths::target().to_string(),
        logged_in: session.is_some(),
        device: session.as_ref().map(|s| s.device_id.clone()),
        server: session.as_ref().map(|_| app.account.server_url()),
        server_time_ms: app.account.now_ms(),
    }))
}

async fn host_notify(State(app): State<App>, peer: Option<Extension<Peer>>, headers: HeaderMap, Json(req): Json<HostNotify>) -> R<StatusCode> {
    let plugin = auth_plugin(&app, peer, &headers)?;
    tracing::info!(plugin = %plugin, title = %req.title, body = %req.body, path = ?req.path, "插件通知");
    Ok(StatusCode::NO_CONTENT)
}

async fn host_license(State(app): State<App>, peer: Option<Extension<Peer>>, headers: HeaderMap, Json(req): Json<LicenseRequest>) -> R<Json<proto::SignedLicense>> {
    let plugin = auth_plugin(&app, peer, &headers)?;
    if req.machine.trim().is_empty() || req.machine.len() > 128 {
        return Err(LocalError::invalid("machine", "invalid"));
    }
    let lic = app.account.plugin_license(&plugin, &req).await?;
    Ok(Json(lic))
}

async fn host_data(State(app): State<App>, peer: Option<Extension<Peer>>, headers: HeaderMap) -> R<Json<proto::PluginData>> {
    let plugin = auth_plugin(&app, peer, &headers)?;
    let data = app.account.plugin_data(&plugin).await?;
    Ok(Json(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_policy_enforced() {
        let q1: ListQuery = serde_urlencoded::from_str("catalog=1").unwrap();
        let q2: ListQuery = serde_urlencoded::from_str("catalog=true").unwrap();
        let q0: ListQuery = serde_urlencoded::from_str("catalog=0").unwrap();
        assert!(q1.catalog && q2.catalog && !q0.catalog);
        assert!(check_readonly(ReadonlyPolicy::Deny, false, &Method::POST, false));
        assert!(!check_readonly(ReadonlyPolicy::Deny, true, &Method::GET, false));
        assert!(check_readonly(ReadonlyPolicy::ReadMethods, true, &Method::GET, false));
        assert!(check_readonly(ReadonlyPolicy::ReadMethods, true, &Method::HEAD, false));
        assert!(!check_readonly(ReadonlyPolicy::ReadMethods, true, &Method::POST, false));
        assert!(!check_readonly(ReadonlyPolicy::ReadMethods, true, &Method::GET, true));
        assert!(check_readonly(ReadonlyPolicy::Plugin, true, &Method::POST, true));
    }

    #[test]
    fn strips_sensitive_headers_and_sets_plugin_headers() {
        let mut req = Request::builder()
            .uri("/api/plugins/demo/ping")
            .header(header::COOKIE, "ll_access=secret")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(proto::HDR_TOKEN, "forged")
            .header(proto::HDR_READONLY, "0")
            .header(header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
            .body(axum::body::Body::empty())
            .unwrap();
        apply_plugin_headers(&mut req, "real-token", proto::Access::Rtc, true);
        let h = req.headers();
        assert!(h.get(header::COOKIE).is_none());
        assert!(h.get(header::AUTHORIZATION).is_none());
        assert_eq!(h.get(proto::HDR_TOKEN).unwrap(), "real-token");
        assert_eq!(h.get(proto::HDR_ACCESS).unwrap(), "rtc");
        assert_eq!(h.get(proto::HDR_READONLY).unwrap(), "1");
        assert_eq!(h.get(proto::HDR_LOCALE).unwrap(), "zh-CN");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_proxy_forwards_and_injects_web_tags() {
        use axum::routing::get;
        use http_body_util::BodyExt;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use tower::ServiceExt;

        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("p.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let upstream = axum::Router::new().route(
            "/p/demo/view/",
            get(|headers: HeaderMap| async move {
                let tok = headers.get(proto::HDR_TOKEN).and_then(|v| v.to_str().ok()).unwrap_or("");
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    format!("<html><head></head><body>tok={tok}</body></html>"),
                )
            }),
        );
        let srv = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let svc = service_fn(move |req| upstream.clone().oneshot(req));
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
            }
        });

        let mut req = Request::builder().uri("/p/demo/view/").body(axum::body::Body::empty()).unwrap();
        apply_plugin_headers(&mut req, "tok123", proto::Access::Local, false);
        let opts = proxy::Opts {
            auth: None,
            host: Some("localhost".into()),
            inject: Some(proxy::WEB_TAGS),
            rewrite_location: None,
            terminal: false,
            on_input: None,
            keep: None,
        };
        let resp = proxy::forward_unix(req, &sock, opts).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("tok=tok123"), "{html}");
        assert!(html.contains(proxy::WEB_TAGS), "{html}");
        srv.abort();
    }
}
