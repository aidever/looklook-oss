//! Claude Code 对话界面的接口（docs/CLAUDE_UI.md §4.7）。
//!
//! - `POST /hook`：`looklook hook` 转来的 hook 事件。只接受这台机器自己发来的、带本次运行令牌的请求。
//! - `/api/instances/{id}/chat…`：管理台用。和终端一样要登录、账户可用；写请求经过 `/api` 的来源检查。
//!   事件用 SSE 推送（`chat/events`）：先发一份快照，之后是状态、新增或更新的条目、要求重新加载。

use std::convert::Infallible;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::access::Peer;
use super::{Access, App};
use crate::chat::{hooks, Event, PermissionAnswer};
use crate::error::LocalError;

type R<T> = Result<T, LocalError>;

pub async fn hook(State(app): State<App>, access: Option<Extension<Access>>, peer: Option<Extension<Peer>>, headers: HeaderMap, body: Bytes) -> Response {
    let local = access.is_none_or(|a| a.0.is_local()) && peer.is_none_or(|p| p.0 == Peer::Local);
    if !local {
        return StatusCode::NOT_FOUND.into_response();
    }
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    // 浏览器发不出这个请求：带 Origin 的一律拒绝
    if headers.contains_key(axum::http::header::ORIGIN) || !app.chat.token_ok(header(hooks::TOKEN_HEADER)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id = header(hooks::INSTANCE_HEADER).to_string();
    let Ok(ev) = serde_json::from_slice::<Value>(&body) else { return (StatusCode::OK, "").into_response() };
    let out = app.chat.hook(&id, ev).await;
    (StatusCode::OK, out).into_response()
}

pub async fn snapshot(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.snapshot(&id).await?))
}

/// 一个对话的事件（事件名、JSON 文本）：`snapshot`（第一条，以及落后太多时）、`status`、`items`、`reset`（界面重新取快照）。
/// 看着的期间（流还在）算一个观看者。SSE（[`events`]）和多路 WebSocket（`chat_mux`）共用。
pub(super) async fn feed(app: App, id: String) -> R<impl Stream<Item = (&'static str, String)> + Send + 'static> {
    let (rx, viewer) = app.chat.subscribe(&id)?;
    let first = app.chat.snapshot(&id).await?;
    Ok(futures_util::stream::unfold((app, id, rx, viewer, Some(first)), |(app, id, mut rx, viewer, first)| async move {
        if let Some(snap) = first {
            return Some((("snapshot", snap.to_string()), (app, id, rx, viewer, None)));
        }
        let ev = match rx.recv().await {
            Ok(Event::Status(s)) => ("status", s.to_string()),
            Ok(Event::Items { start, items }) => ("items", json!({ "start": start, "items": items }).to_string()),
            Ok(Event::Reset) => ("reset", "{}".to_string()),
            // 落后太多（界面卡住了）：重新发一份快照
            Err(RecvError::Lagged(_)) => match app.chat.snapshot(&id).await {
                Ok(s) => ("snapshot", s.to_string()),
                Err(_) => return None,
            },
            Err(RecvError::Closed) => return None,
        };
        Some((ev, (app, id, rx, viewer, None)))
    }))
}

/// SSE：见 [`feed`]。远程打开（平台的 HTTP/2、WebRTC 的 Service Worker）、别的电脑转发过来时用；
/// 本机明文 http 打开的管理台用多路 WebSocket（`chat_mux`，不占浏览器同一地址的 6 条 HTTP/1.1 连接）。
pub async fn events(State(app): State<App>, Path(id): Path<String>) -> R<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    use futures_util::StreamExt;
    let stream = feed(app, id).await?.map(|(ev, data)| Ok(SseEvent::default().event(ev).data(data)));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    before: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    crate::chat::SNAPSHOT_ITEMS
}

pub async fn history(State(app): State<App>, Path(id): Path<String>, Query(q): Query<HistoryQuery>) -> R<Json<Value>> {
    Ok(Json(app.chat.history(&id, q.before, q.limit).await?))
}

#[derive(Deserialize)]
pub struct ItemQuery {
    key: String,
}

pub async fn item(State(app): State<App>, Path(id): Path<String>, Query(q): Query<ItemQuery>) -> R<Json<Value>> {
    Ok(Json(app.chat.item(&id, &q.key).await?))
}

#[derive(Deserialize)]
pub struct ImageQuery {
    key: String,
    #[serde(default)]
    n: usize,
}

/// 用户消息里的图片（对话界面上的缩略图、点开的大图）
pub async fn image(State(app): State<App>, Path(id): Path<String>, Query(q): Query<ImageQuery>) -> R<Response> {
    use axum::http::{header, HeaderValue};
    let (mime, bytes) = app.chat.user_image(&id, &q.key, q.n).await?;
    let mut res = (StatusCode::OK, [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response();
    res.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    Ok(res)
}

#[derive(Deserialize)]
pub struct AgentQuery {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tool: Option<String>,
}

pub async fn agent(State(app): State<App>, Path(id): Path<String>, Query(q): Query<AgentQuery>) -> R<Json<Value>> {
    Ok(Json(app.chat.agent(&id, q.agent.as_deref(), q.tool.as_deref()).await?))
}

pub async fn commands(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.commands(&id).await?))
}

#[derive(Deserialize)]
pub struct MentionsQuery {
    #[serde(default)]
    q: String,
}

/// `GET /api/instances/{id}/chat/mentions?q=`：输入框里打 `@` 时提示的文件、目录
pub async fn mentions(State(app): State<App>, Path(id): Path<String>, Query(q): Query<MentionsQuery>) -> R<Json<Value>> {
    Ok(Json(app.chat.mentions(&id, &q.q).await?))
}

pub async fn prompts(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.prompts(&id)?))
}

#[derive(Deserialize)]
pub struct InputReq {
    #[serde(default)]
    text: String,
    /// 附件目录里的图片（先经“发给 AI”上传）
    #[serde(default)]
    images: Vec<String>,
    /// 立即发送（Claude Code 的 Ctrl+X Ctrl+S）：排着的连同这条马上发出；text 可以为空
    #[serde(default)]
    now: bool,
    /// Codex 的 Tab：排到这一轮之后再发
    #[serde(default)]
    later: bool,
}

pub async fn input(State(app): State<App>, Path(id): Path<String>, Json(req): Json<InputReq>) -> R<Json<Value>> {
    app.instances.note_input(&id);
    if req.text.len() > 200_000 || req.images.len() > 10 {
        return Err(LocalError::invalid("text", "too_long"));
    }
    // 图片只能来自这个终端的附件目录
    let dir = app.paths.attach_dir(&id);
    if req.images.iter().any(|p| !std::path::Path::new(p).starts_with(&dir) || p.contains("..")) {
        return Err(LocalError::invalid("images", "invalid"));
    }
    if req.now {
        return Ok(Json(app.chat.send_now(&id, &req.text, &req.images).await?));
    }
    if req.later {
        return Ok(Json(app.chat.send_later(&id, &req.text, &req.images).await?));
    }
    Ok(Json(app.chat.input(&id, &req.text, &req.images).await?))
}

#[derive(Deserialize)]
pub struct EditReq {
    /// 要改的那条用户消息的 key
    key: String,
    text: String,
    /// 原来那条带的图片一起再发
    #[serde(default)]
    keep_images: bool,
}

/// 编辑重发：丢掉这条和之后的对话，发出改过的（见 `Chat::edit`）
pub async fn edit(State(app): State<App>, Path(id): Path<String>, Json(req): Json<EditReq>) -> R<Json<Value>> {
    app.instances.note_input(&id);
    if req.text.len() > 200_000 || req.key.len() > 200 {
        return Err(LocalError::invalid("text", "too_long"));
    }
    Ok(Json(app.chat.edit(&id, &req.key, &req.text, req.keep_images).await?))
}

#[derive(Deserialize)]
pub struct RenameReq {
    name: String,
}

/// 给对话改名（见 `Chat::rename`）
pub async fn rename(State(app): State<App>, Path(id): Path<String>, Json(req): Json<RenameReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.rename(&id, &req.name).await?))
}

#[derive(Deserialize)]
pub struct Btw {
    question: String,
}

/// `/btw`：顺带问一句，答案不进对话
pub async fn btw(State(app): State<App>, Path(id): Path<String>, Json(req): Json<Btw>) -> R<Json<Value>> {
    app.instances.note_input(&id);
    Ok(Json(app.chat.btw(&id, &req.question).await?))
}

#[derive(Deserialize)]
pub struct Background {
    #[serde(default)]
    tool: Option<String>,
}

/// 前台在跑的命令、子代理转到后台（Ctrl+B）
pub async fn background(State(app): State<App>, Path(id): Path<String>, Json(req): Json<Background>) -> R<StatusCode> {
    app.chat.background_now(&id, req.tool.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct LaterTake {
    n: usize,
}

/// 取回 Tab 排到这一轮之后的一条（放回输入框）
pub async fn later_take(State(app): State<App>, Path(id): Path<String>, Json(req): Json<LaterTake>) -> R<Json<Value>> {
    Ok(Json(app.chat.later_take(&id, req.n)?))
}

/// 以前的会话“现在就转过来”：不等空闲，退出终端里以前的 Claude Code / Codex，接着同一段对话转到对话模式
pub async fn adopt(State(app): State<App>, Path(id): Path<String>) -> R<StatusCode> {
    app.chat.adopt(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}


pub async fn interrupt(State(app): State<App>, Path(id): Path<String>) -> R<StatusCode> {
    app.instances.note_input(&id);
    app.chat.interrupt(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn permission(State(app): State<App>, Path(id): Path<String>, Json(req): Json<PermissionAnswer>) -> R<StatusCode> {
    app.instances.note_input(&id);
    app.chat.answer(&id, &req)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PlanReq {
    choice: u8,
    /// choice 为 3 时：要 Claude 怎么改
    feedback: Option<String>,
}

pub async fn plan(State(app): State<App>, Path(id): Path<String>, Json(req): Json<PlanReq>) -> R<StatusCode> {
    app.instances.note_input(&id);
    app.chat.plan(&id, req.choice, req.feedback.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SwitchProfileReq {
    profile: String,
    /// 起来后发 continue（额度用完、账号用不了时）；在用量栏里随手换的不发
    #[serde(default = "yes", rename = "continue")]
    go_on: bool,
}

fn yes() -> bool {
    true
}

/// 换个 Claude Code 账号接着同一段对话继续
pub async fn switch_profile(State(app): State<App>, Path(id): Path<String>, Json(req): Json<SwitchProfileReq>) -> R<StatusCode> {
    // 助理的大脑：换了一家模型服务时要开新对话把前面的带过去（gateway::assistant::brain_to）
    if crate::assistant::is_brain(&app.store, &id) {
        super::assistant::brain_to(&app, &id, &req.profile, Some(req.go_on)).await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    app.chat.switch_profile(&id, &req.profile, req.go_on).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn relaunch(State(app): State<App>, Path(id): Path<String>) -> R<StatusCode> {
    app.chat.relaunch(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SettingReq {
    kind: String,
    value: String,
}

/// 快捷设置：模型、思考强度、权限模式
pub async fn setting(State(app): State<App>, Path(id): Path<String>, Json(req): Json<SettingReq>) -> R<StatusCode> {
    app.chat.setting(&id, &req.kind, &req.value).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 模型菜单
pub async fn models(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.models(&id).await?))
}

/// 上下文、token、订阅用量
/// Codex 的托管工作树
pub async fn worktrees(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.worktrees(&id).await?))
}

#[derive(Deserialize)]
pub struct WorktreeReq {
    /// `new` / `fork` / `open`
    mode: String,
    /// `open`：哪个工作树
    root: Option<String>,
}

pub async fn worktree(State(app): State<App>, Path(id): Path<String>, Json(req): Json<WorktreeReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.worktree(&id, &req.mode, req.root.as_deref()).await?))
}

pub async fn usage(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.usage(&id).await?))
}

/// 对话生成的文件（Codex 生成的图片）
pub async fn files(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.files(&id)?))
}

#[derive(Deserialize)]
pub struct FileQ {
    name: String,
    /// 1：作为附件下载
    #[serde(default)]
    download: Option<u8>,
}

/// `GET /api/instances/{id}/chat/file?name=`：对话生成的图片（完整内容，按图片类型返回，可以直接放进 `<img>`）
pub async fn file(State(app): State<App>, Path(id): Path<String>, Query(q): Query<FileQ>) -> R<Response> {
    use axum::http::header;
    let path = app.chat.file_path(&id, &q.name)?;
    let bytes = tokio::fs::read(&path).await.map_err(|_| LocalError::not_found())?;
    let mime = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    };
    let mut res = (StatusCode::OK, [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response();
    res.headers_mut().insert("x-content-type-options", axum::http::HeaderValue::from_static("nosniff"));
    if q.download == Some(1) {
        if let Ok(v) = crate::transfer::content_disposition(&q.name).parse() {
            res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(res)
}

/// 对话产出的文件（Codex 新建、改过、提到过的本机文件）
/// `GET /api/instances/{id}/chat/setup`：要用户先处理的——工作目录里新的、改过的 hook（Codex 要信任过才运行）、还没登录
pub async fn setup(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.setup(&id).await?))
}

#[derive(Deserialize)]
pub struct HooksReq {
    trust: bool,
    /// 界面上列出的 `{ key, hash }`
    hooks: Vec<HookRef>,
}

#[derive(Deserialize)]
pub struct HookRef {
    key: String,
    hash: String,
}

#[derive(Deserialize)]
pub struct UpdateReq {
    /// true：开始更新；false：跳过这个版本
    install: bool,
}

/// `POST /api/instances/{id}/chat/codex-update`：更新 Codex（返回安装任务，进度看 `GET /tools/install/codex`）或跳过这个版本
pub async fn codex_update(State(app): State<App>, Path(id): Path<String>, Json(req): Json<UpdateReq>) -> R<Json<Value>> {
    app.store.instance(&id)?.ok_or_else(LocalError::not_found)?;
    Ok(Json(app.chat.codex_update(req.install).await?))
}

/// `POST /api/instances/{id}/chat/hooks`：信任（`trust: true`）或跳过这几条
pub async fn hooks_answer(State(app): State<App>, Path(id): Path<String>, Json(req): Json<HooksReq>) -> R<StatusCode> {
    app.instances.note_input(&id);
    let picked: Vec<(String, String)> = req.hooks.into_iter().map(|h| (h.key, h.hash)).collect();
    app.chat.hooks_answer(&id, req.trust, &picked).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn artifacts(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.artifacts(&id).await?))
}

#[derive(Deserialize)]
pub struct ArtifactQ {
    path: String,
    #[serde(default)]
    download: Option<u8>,
}

/// `GET /api/instances/{id}/chat/artifact?path=`：这段对话产出的某个文件。图片、PDF、音视频按类型返回，
/// 网页、代码这类按纯文本返回（加 `sandbox`，不会在管理台的源上运行），其余（和 `download=1`）作为附件下载。
pub async fn artifact(State(app): State<App>, Path(id): Path<String>, Query(q): Query<ArtifactQ>) -> R<Response> {
    use axum::http::{header, HeaderValue};
    let path = app.chat.artifact_path(&id, &q.path).await?;
    let f = tokio::fs::File::open(&path).await.map_err(|_| LocalError::not_found())?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let (mime, viewable) = crate::chat::codex_artifacts::content_type(&path);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let mut res = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(f, 64 * 1024)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    if mime.starts_with("text/") {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    }
    if !viewable || q.download == Some(1) {
        if let Ok(v) = crate::transfer::content_disposition(&name).parse() {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(res)
}

/// 权限规则和 MCP 服务器
pub async fn config(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.config(&id).await?))
}

#[derive(Deserialize)]
pub struct CodexLoginReq {
    /// device / api_key
    how: String,
    #[serde(default)]
    api_key: Option<String>,
}

/// 在对话页登录 Codex
pub async fn codex_login(State(app): State<App>, Path(id): Path<String>, Json(req): Json<CodexLoginReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.codex_login(&id, &req.how, req.api_key.as_deref()).await?))
}

/// Codex 的 MCP 服务器
pub async fn codex_mcp(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.codex_mcp(&id).await?))
}

#[derive(Deserialize)]
pub struct NameReq {
    name: String,
}

pub async fn codex_mcp_login(State(app): State<App>, Path(id): Path<String>, Json(req): Json<NameReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.codex_mcp_login(&id, &req.name).await?))
}

#[derive(Deserialize)]
pub struct CodexLoginCancelReq {
    login_id: String,
}

pub async fn codex_login_cancel(State(app): State<App>, Path(id): Path<String>, Json(req): Json<CodexLoginCancelReq>) -> R<StatusCode> {
    app.chat.codex_login_cancel(&id, &req.login_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RuleReq {
    source: String,
    /// allow / ask / deny / additionalDirectories
    list: String,
    value: String,
    add: bool,
}

/// 加 / 删权限规则、额外目录
pub async fn config_rule(State(app): State<App>, Path(id): Path<String>, Json(req): Json<RuleReq>) -> R<StatusCode> {
    app.chat.config_edit(&id, &req.source, &req.list, &req.value, req.add)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct McpReq {
    name: String,
    op: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    url: Option<String>,
}

/// MCP 服务器开关、重连、登录
pub async fn config_mcp(State(app): State<App>, Path(id): Path<String>, Json(req): Json<McpReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.mcp_op(&id, &req.name, &req.op, req.enabled, req.url.as_deref()).await?))
}

#[derive(Deserialize)]
pub struct PluginReq {
    op: String,
    name: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    accept: Option<String>,
}

/// 插件列表
pub async fn plugins(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.plugins(&id).await?))
}

/// 装、卸、开关插件，加减市场
pub async fn plugin_op(State(app): State<App>, Path(id): Path<String>, Json(req): Json<PluginReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.plugin_op(&id, &req.op, &req.name, req.scope.as_deref(), req.accept.as_deref()).await?))
}

/// 子代理
pub async fn agents(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(tokio::task::block_in_place(|| app.chat.agents(&id))?))
}

#[derive(Deserialize)]
pub struct AgentReq {
    /// create / delete
    op: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    path: String,
}

pub async fn agent_op(State(app): State<App>, Path(id): Path<String>, Json(req): Json<AgentReq>) -> R<Json<Value>> {
    match req.op.as_str() {
        "create" => Ok(Json(app.chat.agent_create(&id, &req.source, &req.name, &req.description, &req.prompt)?)),
        "delete" => {
            app.chat.agent_delete(&id, &req.path)?;
            Ok(Json(json!({ "ok": true })))
        }
        _ => Err(LocalError::invalid("op", "invalid")),
    }
}

#[derive(Deserialize)]
pub struct HookReq {
    source: String,
    event: String,
    #[serde(default)]
    matcher: String,
    command: String,
    add: bool,
}

/// 加 / 删 hook
pub async fn config_hook(State(app): State<App>, Path(id): Path<String>, Json(req): Json<HookReq>) -> R<StatusCode> {
    app.chat.hook_edit(&id, &req.source, &req.event, &req.matcher, &req.command, req.add)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct OnReq {
    on: bool,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
pub struct MemoryReq {
    source: String,
}

/// 新建记忆文件（/memory 里还没有的），返回路径
pub async fn config_memory(State(app): State<App>, Path(id): Path<String>, Json(req): Json<MemoryReq>) -> R<Json<Value>> {
    Ok(Json(json!({ "path": app.chat.memory_create(&id, &req.source)? })))
}

/// 沙箱开关
pub async fn config_sandbox(State(app): State<App>, Path(id): Path<String>, Json(req): Json<OnReq>) -> R<StatusCode> {
    app.chat.sandbox_set(&id, req.on)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Codex 的 hook、实验功能
pub async fn codex_extras(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.codex_extras(&id).await?))
}

/// Codex 实验功能开关
pub async fn codex_feature(State(app): State<App>, Path(id): Path<String>, Json(req): Json<OnReq>) -> R<StatusCode> {
    app.chat.codex_feature(&id, &req.name, req.on).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// /status
pub async fn status_view(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.status_view(&id).await?))
}

#[derive(Deserialize)]
pub struct FeedbackReq {
    text: String,
    #[serde(default)]
    transcript: bool,
}

/// /feedback
pub async fn feedback(State(app): State<App>, Path(id): Path<String>, Json(req): Json<FeedbackReq>) -> R<Json<Value>> {
    Ok(Json(app.chat.feedback(&id, &req.text, req.transcript).await?))
}

#[derive(Deserialize)]
pub struct UrlReq {
    url: String,
}

/// 远程时 OAuth 的本机回调：看看替浏览器访问
pub async fn oauth_forward(State(app): State<App>, Path(id): Path<String>, Json(req): Json<UrlReq>) -> R<StatusCode> {
    app.chat.oauth_forward(&id, &req.url).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 这个会话改过的文件
pub async fn changes(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.changes(&id).await?))
}

#[derive(Deserialize)]
pub struct DialogReq {
    /// 选第几项；没有时是取消（Esc）
    index: Option<usize>,
    #[serde(default)]
    title: String,
    /// 选的那一项的文字（标题对不上时按它核对）
    #[serde(default)]
    label: String,
    /// 选的是 AskUserQuestion 的“Type something.”（自己填）时填的内容
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
pub struct SeenReq {
    #[serde(default)]
    active: bool,
}

/// 看过了（看板上点“知道了”），或者会话页正开着（`active`，每 20 秒一次）
pub async fn seen(State(app): State<App>, Path(id): Path<String>, Json(req): Json<SeenReq>) -> R<StatusCode> {
    app.chat.seen(&id, req.active)?;
    Ok(StatusCode::NO_CONTENT)
}

/// TUI 上的选择框：选一项或取消
pub async fn dialog(State(app): State<App>, Path(id): Path<String>, Json(req): Json<DialogReq>) -> R<StatusCode> {
    app.instances.note_input(&id);
    match req.index {
        Some(i) => app.chat.dialog_choose(&id, &req.title, &req.label, i, req.text.as_deref()).await?,
        None => app.chat.dialog_cancel(&id).await?,
    }
    Ok(StatusCode::NO_CONTENT)
}

/// 子代理和后台命令
pub async fn tasks(State(app): State<App>, Path(id): Path<String>) -> R<Json<Value>> {
    Ok(Json(app.chat.tasks(&id).await?))
}

#[derive(Deserialize)]
pub struct TaskQuery {
    #[serde(default)]
    tool: String,
    /// 停止对话记录里没有的后台命令时：任务列表给的命令
    #[serde(default)]
    command: Option<String>,
}

pub async fn task_output(State(app): State<App>, Path(id): Path<String>, Query(q): Query<TaskQuery>) -> R<Json<Value>> {
    Ok(Json(app.chat.task_output(&id, &q.tool).await?))
}

pub async fn task_stop(State(app): State<App>, Path(id): Path<String>, Json(q): Json<TaskQuery>) -> R<StatusCode> {
    app.chat.stop_task(&id, &q.tool, q.command.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}
