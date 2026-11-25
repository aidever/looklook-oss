//! 宠物版本号、桌面宠物状态的推送（`GET /api/live/ws`）：一个页面一条 WebSocket。
//!
//! 以前是长轮询（`/api/pets/version?since=`、`/api/desktop/pet?since=`，每条最多挂 25 秒）。本机管理台是明文 http，
//! 浏览器同一地址最多 6 条 HTTP 连接、所有窗口共用：桌面应用的主窗口两条，每块屏幕的桌面宠物窗口又各两条，
//! 两块屏幕就把 6 条占满，发图片、看文件、新建会话这些请求都要排队等某条长轮询回来（最长 25 秒）。
//! WebSocket 升级后不占这 6 条（同 `chat_mux`）。远程打开（HTTP/2、WebRTC）没有这个限制，页面仍用长轮询。
//!
//! 这里 → 页面：连上先发一份，之后变了再发：`{"t":"pets","ver":…}`、`{"t":"desk","data":<同 GET /api/desktop/pet>}`。
//! 页面不用发什么。

use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::{self, protocol::Role, Message};

use super::{origin_matches, proxy, Access};

/// 心跳：页面已经没了时发送失败、及时收掉
const PING: Duration = Duration::from_secs(15);

pub async fn ws(Extension(access): Extension<Access>, mut req: Request) -> Response {
    if !proxy::is_websocket(req.headers()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // WebSocket 不受 CORS 限制：别的网站也能连 127.0.0.1，来源必须是管理台自己
    if !origin_matches(req.headers(), &access) {
        return (StatusCode::FORBIDDEN, "origin").into_response();
    }
    let Some(key) = req.headers().get(header::SEC_WEBSOCKET_KEY).cloned() else { return StatusCode::BAD_REQUEST.into_response() };
    let here = access.is_local();
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match upgrade.await {
            Ok(io) => {
                let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(io), Role::Server, None).await;
                serve(ws, here).await;
            }
            Err(e) => tracing::debug!(error = %e, "宠物推送的 WebSocket 升级失败"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, tungstenite::handshake::derive_accept_key(key.as_bytes()))
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn pets_msg() -> String {
    json!({ "t": "pets", "ver": crate::pets::ver() }).to_string()
}

fn desk_msg(here: bool) -> String {
    let mut v = super::api::desk_view(&crate::desk_pet::get());
    v["here"] = json!(here);
    json!({ "t": "desk", "data": v }).to_string()
}

async fn serve<S>(mut ws: tokio_tungstenite::WebSocketStream<S>, here: bool)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut pets = crate::pets::subscribe();
    let mut desk = crate::desk_pet::subscribe();
    pets.mark_unchanged();
    desk.mark_unchanged();
    if ws.send(Message::Text(pets_msg().into())).await.is_err() || ws.send(Message::Text(desk_msg(here).into())).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING);
    ping.tick().await;
    loop {
        let text = tokio::select! {
            r = pets.changed() => match r {
                Ok(()) => pets_msg(),
                Err(_) => break,
            },
            r = desk.changed() => match r {
                Ok(()) => desk_msg(here),
                Err(_) => break,
            },
            m = ws.next() => match m {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => {
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                continue;
            }
        };
        if ws.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}
