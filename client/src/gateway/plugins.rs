//! 插件的网关部分（src/plugins.rs）：
//!
//! - 管理：`GET /api/plugin-store`（本机装的 + 服务端目录 + 授权）、`POST /api/plugin-store/{名}/install`（装 / 更新）、
//!   `DELETE /api/plugin-store/{名}`（卸载，数据留着）、`POST /api/plugin-store/{名}/restart`；
//! - 转发：`/p/{名}/…`（页面、画面 WebSocket）和 `/api/plugins/{名}/…`（接口，和别的 `/api` 一样过写请求的来源检查）
//!   → 插件的 Unix Socket。转发前去掉 Cookie 和认证头，加上插件口令和是不是这台电脑打开的；
//!   插件页面要用 WebRTC 远程打开，HTML 里注入 `/static/web.js`（WebSocket 接到启动页的通道上）；
//! - 宿主接口：`/_host/license`、`/_host/info`，只在本机、只认看看给插件的口令（不经 WebRTC 转给别的电脑）。

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use looklook_protocol::plugin as proto;
use serde_json::Value;

use super::{access, assets, origin_matches, proxy, Access, App};
use crate::error::LocalError;

type R<T> = Result<T, LocalError>;

/// `/api` 下的管理接口和插件接口转发
pub fn api_routes() -> Router<App> {
    Router::new()
        .route("/plugin-store", get(list))
        .route("/plugin-store/{name}", axum::routing::delete(uninstall))
        .route("/plugin-store/install-file", post(install_file))
        .route("/plugin-store/offline", post(offline_add))
        .route("/plugin-store/offline/{name}", axum::routing::delete(offline_remove))
        .route("/plugin-store/{name}/offline-package", get(offline_package))
        .route("/plugin-store/{name}/install", post(install))
        .route("/plugin-store/{name}/restart", post(restart))
        .route("/plugins/{name}/{*rest}", any(api_proxy))
}

/// 页面
pub fn page_routes() -> Router<App> {
    Router::new()
        .route(
            "/p/{name}",
            get(|Path(name): Path<String>, uri: axum::http::Uri| async move { Redirect::permanent(&format!("/p/{name}/{}", uri.query().map(|q| format!("?{q}")).unwrap_or_default())) }),
        )
        .route("/p/{name}/", any(page_proxy))
        .route("/p/{name}/{*rest}", any(page_proxy))
}

/// 宿主接口（只挂在本机入口上）
pub fn host_routes() -> Router<App> {
    Router::new().route(proto::HOST_LICENSE_PATH, post(host_license)).route(proto::HOST_INFO_PATH, get(host_info))
}

async fn list(State(app): State<App>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> Json<Value> {
    // `?local=1`：只看本机装的（不问服务端），工具菜单用
    Json(app.plugins.list(q.get("local").map(String::as_str) != Some("1")).await)
}

async fn install(State(app): State<App>, Path(name): Path<String>) -> R<impl IntoResponse> {
    app.plugins.start_install(&name)?;
    Ok((StatusCode::ACCEPTED, Json(app.plugins.list(false).await)))
}

/// 从离线安装包装：`{ path }`（插件页先把文件分块传到 `incoming_dir`）
async fn install_file(State(app): State<App>, Json(b): Json<Value>) -> R<Json<Value>> {
    let path = b["path"].as_str().filter(|p| !p.is_empty()).ok_or_else(|| LocalError::invalid("path", "required"))?;
    let name = app.plugins.install_file(std::path::Path::new(path)).await?;
    app.plugins.forget_features();
    let mut v = app.plugins.list(false).await;
    v["installed_name"] = Value::from(name);
    Ok(Json(v))
}

/// 粘贴离线授权码：`{ code }`
async fn offline_add(State(app): State<App>, Json(b): Json<Value>) -> R<Json<Value>> {
    let added = app.plugins.add_offline(b["code"].as_str().unwrap_or(""))?;
    app.plugins.forget_features();
    let mut v = app.plugins.list(false).await;
    v["added"] = serde_json::json!(added);
    Ok(Json(v))
}

async fn offline_remove(State(app): State<App>, Path(name): Path<String>) -> R<Json<Value>> {
    app.plugins.remove_offline(&name)?;
    app.plugins.forget_features();
    Ok(Json(app.plugins.list(false).await))
}

/// 离线安装包的下载地址：`?platform=`（默认本机平台）
async fn offline_package(State(app): State<App>, Path(name): Path<String>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> R<Json<Value>> {
    let platform = q.get("platform").cloned().unwrap_or_else(proto::current_platform);
    Ok(Json(serde_json::json!({ "url": app.plugins.offline_package_url(&name, &platform).await? })))
}

async fn uninstall(State(app): State<App>, Path(name): Path<String>) -> R<Json<Value>> {
    app.plugins.uninstall(&name).await?;
    app.plugins.forget_features();
    Ok(Json(app.plugins.list(false).await))
}

async fn restart(State(app): State<App>, Path(name): Path<String>) -> R<Json<Value>> {
    app.plugins.stop(&name).await;
    app.plugins.forget_features();
    app.plugins.ensure(&name).await?;
    Ok(Json(app.plugins.list(false).await))
}

async fn api_proxy(State(app): State<App>, Path((name, _)): Path<(String, String)>, req: Request) -> Response {
    forward(&app, &name, req, false).await
}

/// `/p/{name}/` 只有一个路径参数，`/p/{name}/{*rest}` 有两个，按名字取 `name`（`Path<String>` 遇到两个参数会直接 500）
async fn page_proxy(State(app): State<App>, Path(params): Path<std::collections::HashMap<String, String>>, req: Request) -> Response {
    let name = params.get("name").map(String::as_str).unwrap_or_default();
    forward(&app, name, req, true).await
}

async fn forward(app: &App, name: &str, req: Request, page: bool) -> Response {
    if !proto::valid_name(name) {
        return LocalError::not_found().into_response();
    }
    let access = req.extensions().get::<Access>().cloned().unwrap_or(Access::Local);
    // 画面的 WebSocket：只接受管理台自己的页面连（防别的网站借浏览器连进来）
    if proxy::is_websocket(req.headers()) && !origin_matches(req.headers(), &access) {
        return LocalError::new("CSRF_FAILED").into_response();
    }
    let p = match app.plugins.ensure(name).await {
        Ok(p) => p,
        Err(e) if page => return plugin_error_page(&e),
        Err(e) => return e.into_response(),
    };
    #[cfg(unix)]
    {
        unix::forward(req, &p.socket, &p.token, access.is_local(), page).await
    }
    #[cfg(not(unix))]
    {
        let _ = (p, page);
        LocalError::new("PLUGIN_UNSUPPORTED").into_response()
    }
}

/// 插件起不来、没装、没授权时页面上显示的：管理台的提示页（iframe 里看得到原因）
fn plugin_error_page(e: &LocalError) -> Response {
    let kind = match e.code.as_ref() {
        "PLUGIN_NOT_INSTALLED" | "PLUGIN_NOT_FOUND" => "plugin_missing",
        _ => "plugin_failed",
    };
    tracing::debug!(error = ?e, "插件页面打不开");
    assets::message_page(StatusCode::SERVICE_UNAVAILABLE, kind)
}

/// 宿主接口：口令对、从这台电脑来
fn host_check(app: &App, req: &Request) -> R<()> {
    let local = req.extensions().get::<access::Peer>() == Some(&access::Peer::Local) || req.extensions().get::<access::Peer>().is_none();
    let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if !local || !app.plugins.host_token_ok(token) {
        return Err(LocalError::new("FORBIDDEN"));
    }
    Ok(())
}

async fn host_info(State(app): State<App>, req: Request) -> R<Json<Value>> {
    host_check(&app, &req)?;
    Ok(Json(app.plugins.info()))
}

/// 代插件申请凭证：`{ plugin, machine, version }`
async fn host_license(State(app): State<App>, req: Request) -> R<Json<proto::LicenseResponse>> {
    host_check(&app, &req)?;
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024).await.map_err(|_| LocalError::invalid("body", "too_large"))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| LocalError::invalid("body", "json"))?;
    let name = v["plugin"].as_str().filter(|n| proto::valid_name(n)).ok_or_else(|| LocalError::invalid("plugin", "invalid"))?;
    // 只给正在跑的插件签（看看起的进程才拿得到口令）
    if !app.plugins.running_names().await.iter().any(|n| n == name) {
        return Err(LocalError::new("FORBIDDEN"));
    }
    let req = proto::LicenseRequest { machine: v["machine"].as_str().unwrap_or("").to_string(), version: v["version"].as_str().unwrap_or("").chars().take(40).collect() };
    Ok(Json(app.plugins.license(name, &req).await?))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, Version};
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;
    use std::time::Duration;

    const HOP: &[&str] = &["connection", "keep-alive", "te", "trailer", "transfer-encoding", "upgrade", "proxy-connection", "proxy-authenticate", "proxy-authorization"];

    fn strip(h: &mut HeaderMap) {
        let listed: Vec<HeaderName> = h
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
            .collect();
        for n in listed {
            h.remove(n);
        }
        for n in HOP {
            h.remove(*n);
        }
    }

    pub async fn forward(mut req: Request, socket: &std::path::Path, token: &str, local: bool, page: bool) -> Response {
        let ws = proxy::is_websocket(req.headers());
        let client_upgrade = ws.then(|| hyper::upgrade::on(&mut req));
        let (mut parts, body) = req.into_parts();
        parts.version = Version::HTTP_11;
        let h = &mut parts.headers;
        strip(h);
        // 看看自己的登录 Cookie、认证头不给插件
        h.remove(header::COOKIE);
        h.remove(header::AUTHORIZATION);
        h.remove(proto::HDR_TOKEN);
        h.remove(proto::HDR_LOCAL);
        if let Ok(v) = HeaderValue::from_str(token) {
            h.insert(proto::HDR_TOKEN, v);
        }
        h.insert(proto::HDR_LOCAL, HeaderValue::from_static(if local { "1" } else { "0" }));
        h.insert(header::HOST, HeaderValue::from_static("plugin"));
        if ws {
            h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        }
        if page {
            // 要往页面里注入脚本：请插件返回不压缩的
            h.remove(header::ACCEPT_ENCODING);
        }
        let stream = match tokio::net::UnixStream::connect(socket).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "连不上插件");
                return assets::message_page(StatusCode::BAD_GATEWAY, "upstream_down");
            }
        };
        let (mut sender, conn) = match hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(stream)).await {
            Ok(x) => x,
            Err(_) => return assets::message_page(StatusCode::BAD_GATEWAY, "upstream_down"),
        };
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let body = if ws { Body::empty() } else { body };
        let fut = sender.send_request(Request::from_parts(parts, body));
        // 接口可以慢（装浏览器内核这类），只限制等响应头的时间
        let mut resp = match tokio::time::timeout(Duration::from_secs(120), fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "插件没有响应");
                return assets::message_page(StatusCode::BAD_GATEWAY, "upstream_down");
            }
            Err(_) => return assets::message_page(StatusCode::GATEWAY_TIMEOUT, "upstream_down"),
        };
        if ws && resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            let server_upgrade = hyper::upgrade::on(&mut resp);
            let client_upgrade = client_upgrade.expect("websocket");
            tokio::spawn(async move {
                if let Ok((c, s)) = tokio::try_join!(client_upgrade, server_upgrade) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(s)).await;
                }
            });
            let (parts, _) = resp.into_parts();
            return Response::from_parts(parts, Body::empty());
        }
        let (mut parts, body) = resp.into_parts();
        strip(&mut parts.headers);
        // 插件不能给看看的域名设 Cookie
        parts.headers.remove(header::SET_COOKIE);
        let html = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/html"));
        if page && html && parts.status == StatusCode::OK && parts.headers.get(header::CONTENT_ENCODING).is_none() {
            let bytes = match tokio::time::timeout(Duration::from_secs(30), body.collect()).await {
                Ok(Ok(b)) => b.to_bytes(),
                _ => return assets::message_page(StatusCode::BAD_GATEWAY, "upstream_down"),
            };
            let html = proxy::inject(&String::from_utf8_lossy(&bytes), proxy::WEB_TAGS);
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Response::from_parts(parts, Body::from(html));
        }
        Response::from_parts(parts, Body::new(body.map_err(axum::Error::new)))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 假插件：读一个请求，把收到的头原样写回去（HTML），确认口令、Cookie、注入都对
    #[tokio::test]
    async fn forwards_over_unix_socket() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("p.sock");
        let l = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_lowercase().replace("\r\n", "|");
            let body = format!("<html><head></head><body>{head}</body></html>");
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nset-cookie: x=1\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        let req = Request::builder().uri("/p/demo/?a=1").header("cookie", "ll_access=secret").header("authorization", "Basic x").header(proto::HDR_TOKEN, "forged").body(Body::empty()).unwrap();
        let resp = unix::forward(req, &sock, "tok123", false, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_none(), "插件不能设 Cookie");
        let text = String::from_utf8(axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
        assert!(text.contains("x-ll-plugin-token: tok123"), "{text}");
        assert!(!text.contains("forged") && !text.contains("secret") && !text.contains("basic x"), "{text}");
        assert!(text.contains("x-ll-local: 0"));
        assert!(text.contains("get /p/demo/?a=1 http/1.1"), "路径原样转过去：{text}");
        assert!(text.contains(proxy::WEB_TAGS), "页面里注入了 web.js");
    }

    #[tokio::test]
    async fn plugin_not_listening() {
        let d = tempfile::tempdir().unwrap();
        let req = Request::builder().uri("/api/plugins/demo/x").body(Body::empty()).unwrap();
        let resp = unix::forward(req, &d.path().join("none.sock"), "t", true, false).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    /// `/p/{名}/` 和 `/p/{名}/{*rest}` 都要进到插件转发（以前子路径的 JS、CSS 全 500，插件页白屏）
    #[tokio::test]
    async fn page_routes_take_name_with_or_without_rest() {
        use tower::ServiceExt;
        let d = tempfile::tempdir().unwrap();
        let app = crate::ssh_transfer::e2e::app(d.path(), "http://127.0.0.1:1", "dev", ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]));
        for path in ["/p/demo/", "/p/demo/assets/index.js", "/p/demo/view/a/b.css"] {
            let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
            req.extensions_mut().insert(Access::Local);
            let resp = super::super::router(app.clone()).oneshot(req).await.unwrap();
            // 没装这个插件：走到转发里，给出插件提示页，而不是路径参数解析失败的 500
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        }
    }
}
