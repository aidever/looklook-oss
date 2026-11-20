//! 对话模式的 Antigravity（agy，docs/AGY_UI.md）：看看通过 `agy acp`（ACP）直接驱动对话，界面是看看自己的对话界面。
//!
//! - 一台电脑上所有对话模式的 agy 终端共用一个 `agy acp --process-per-session`（[`agy_acp::AcpConn`]）；
//!   每个终端是其中一个会话（会话号记在 [`Binding`] 里），终端标签里是同一个工作目录下的普通 shell。
//! - 打开时：新终端 `session/new`；已经有会话的（客户端重启、agy 重启后）`session/load`，agy 把历史重放成
//!   `session/update`，记进 `agy_log.rs`。
//! - 发消息 `session/prompt`，一直等到这一轮结束；在跑的时候再发的先排着，这一轮结束后接着发（ACP 不能插话）。
//! - 审批是 agy 发来的 `session/request_permission`：先过命令规则（guard.rs），“全部权限”下没命中规则的直接放行，
//!   其余挂起等界面回答。停止是 `session/cancel`，模型、思考强度是 `session/set_config_option`，
//!   权限模式是 `session/set_mode`（看看的五档对到 agy 的四档，见 [`acp_mode`]）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::agy_acp::{self, AcpConn, Incoming};
use super::agy_log;
use super::{lock, Binding, Chat, Inst, Pending, Phase, PermissionAnswer};
use crate::error::LocalError;

/// 一张图片最大多少字节
const IMAGE_LIMIT: u64 = 20 << 20;
/// 运行中最多排几条
const QUEUE_LIMIT: usize = 20;

#[derive(Default)]
pub struct Hub {
    conn: tokio::sync::Mutex<Option<Arc<AcpConn>>>,
    /// 第几个 `agy acp` 进程（0 表示还没起过）；会话只在加载它的那个进程里有效
    generation: AtomicU64,
    /// 会话 → 终端
    sessions: Mutex<HashMap<String, String>>,
    /// agy 更新了：没有会话在跑的时候把 `agy acp` 换成新版本（下次用到时）
    stale: std::sync::atomic::AtomicBool,
}

/// agy 这边每个终端多记的状态
#[derive(Default)]
pub struct AgySt {
    /// 会话是在第几个进程里加载的（不等于当前的就要重新加载）
    pub generation: u64,
    /// 运行中发的、等这一轮结束再发的：文字和图片
    pub queued: Vec<(String, Vec<String>)>,
    /// 会话的设置项（`configOptions`：模型、思考强度、模式）
    config: Value,
    /// 斜杠命令（`available_commands_update`）
    commands: Vec<Value>,
    /// 累计 token（每轮 `session/prompt` 的 `usage` 加起来）
    usage: Usage,
    pub window: Option<u64>,
    /// 这一轮开始的时间
    turn_at: Option<Instant>,
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// 看看的权限模式 → agy 的模式（`session/set_mode`）。全部权限：agy 的 auto，审批由看看直接放行
pub fn acp_mode(mode: &str) -> Option<&'static str> {
    Some(match mode {
        "default" => "default",
        "acceptEdits" => "accept-edits",
        "plan" => "plan",
        "auto" | "bypassPermissions" => "auto",
        _ => return None,
    })
}

/// agy 报的模式 → 看看的
fn looklook_mode(acp: &str) -> Option<&'static str> {
    Some(match acp {
        "default" => "default",
        "accept-edits" => "acceptEdits",
        "plan" => "plan",
        "auto" => "auto",
        _ => return None,
    })
}

fn mode_key(id: &str) -> String {
    format!("agy.mode.{id}")
}

/// `configOptions` 里某一项的当前值
fn config_value<'a>(config: &'a Value, id: &str) -> Option<&'a str> {
    config.as_array()?.iter().find(|o| o["id"] == id)?["currentValue"].as_str()
}

fn config_options(config: &Value, id: &str) -> Vec<Value> {
    config.as_array().and_then(|a| a.iter().find(|o| o["id"] == id)).and_then(|o| o["options"].as_array().cloned()).unwrap_or_default()
}

/// 模型菜单（和 Claude Code 的同一个格式）：`configOptions` 里的 `model`，思考强度是 `thought_level` 的选项
fn model_options(config: &Value) -> Vec<Value> {
    let efforts: Vec<Value> = config_options(config, "thought_level").iter().filter_map(|o| o["value"].as_str().map(|v| json!(v))).collect();
    let current = config_value(config, "model");
    config_options(config, "model")
        .iter()
        .filter_map(|o| {
            let v = o["value"].as_str()?;
            Some(json!({
                "value": v,
                "label": o["name"].as_str().unwrap_or(v),
                "description": o["description"].as_str().unwrap_or(""),
                "section": "main",
                "model": v,
                "efforts": efforts,
                "default_effort": Value::Null,
                "default": current == Some(v),
            }))
        })
        .collect()
}

/// 界面的回答 → `session/request_permission` 的结果。`options` 是 agy 给的选项
pub fn decision(options: &Value, req: &PermissionAnswer) -> Result<Value, LocalError> {
    let want: &[&str] = match req.decision.as_str() {
        "allow" => &["allow_once", "allow_always"],
        "allow_session" => &["allow_always", "allow_once"],
        "deny" => &["reject_once", "reject_always"],
        _ => return Err(LocalError::invalid("decision", "invalid")),
    };
    Ok(pick(options, want))
}

/// 按种类挑一个选项（`want` 里排前面的优先）；一个都没有就当作取消
fn pick(options: &Value, want: &[&str]) -> Value {
    let opts = options.as_array().cloned().unwrap_or_default();
    for w in want {
        if let Some(id) = opts.iter().find(|o| o["kind"] == *w).and_then(|o| o["optionId"].as_str()) {
            return json!({ "outcome": { "outcome": "selected", "optionId": id } });
        }
    }
    json!({ "outcome": { "outcome": "cancelled" } })
}

/// 发给 agy 的内容：文字和图片（base64）
fn prompt_content(text: &str, images: &[String]) -> Result<Value, LocalError> {
    use base64::Engine;
    let mut parts = vec![];
    if !text.trim().is_empty() || images.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for p in images {
        let path = std::path::Path::new(p);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => return Err(LocalError::invalid("images", "unsupported")),
        };
        if std::fs::metadata(path).map_err(|_| LocalError::invalid("images", "not_found"))?.len() > IMAGE_LIMIT {
            return Err(LocalError::invalid("images", "too_large"));
        }
        let data = std::fs::read(path).map_err(|_| LocalError::invalid("images", "not_found"))?;
        parts.push(json!({ "type": "image", "mimeType": mime, "data": base64::engine::general_purpose::STANDARD.encode(data) }));
    }
    Ok(Value::Array(parts))
}

impl Chat {
    pub(super) fn is_agy(&self, id: &str) -> bool {
        self.inst(id).agent.launch() == "agy"
    }

    fn agy_inst_of(&self, session: &str) -> Option<Arc<Inst>> {
        let id = lock(&self.agy.sessions).get(session).cloned()?;
        Some(self.inst(&id))
    }

    fn agy_session(inst: &Inst) -> Option<String> {
        lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone())
    }

    /// 连上 `agy acp`（已经起着就直接用）
    async fn agy_conn(&self) -> Result<Arc<AcpConn>, LocalError> {
        let mut slot = self.agy.conn.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| c.alive()) {
            if !self.agy.stale.load(Ordering::SeqCst) || self.agy_busy() {
                return Ok(c.clone());
            }
            // 装了新版本、现在没有会话在跑：换掉（各会话下次用到时重新加载）
            tracing::info!("agy 更新过了，换成新版本的 agy acp");
            c.kill();
        }
        self.agy.stale.store(false, Ordering::SeqCst);
        let shell = crate::shells::login_shell();
        let program = agy_acp::program(&shell).await;
        let path = super::codex_app::launch_path(&shell, &program).await;
        let env = super::codex_app::user_env(&shell).await.unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = AcpConn::start(&program, path.as_deref(), &env, &self.paths.home.join("run"), tx).await?;
        let gen = self.agy.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *slot = Some(conn.clone());
        drop(slot);
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(me.agy_events(conn.clone(), rx, gen));
        }
        tracing::info!(pid = ?conn.pid, version = ?lock(&conn.version), "已起 agy acp");
        Ok(conn)
    }

    /// 有 agy 会话正在跑一轮（或等确认）
    fn agy_busy(&self) -> bool {
        let ids: Vec<String> = lock(&self.agy.sessions).values().cloned().collect();
        ids.iter().any(|id| matches!(lock(&self.inst(id).st).phase, Phase::Running | Phase::Approval))
    }

    /// agy 装了新版本（看看更新的、agy 自己后台更新的）：空闲时换掉 `agy acp`
    pub fn agy_installed(&self) {
        tracing::info!("agy 更新好了，agy acp 空闲时换成新版本");
        self.agy.stale.store(true, Ordering::SeqCst);
    }

    /// 这个终端的会话在当前的 agy 进程里加载好了（没有会话就新建一个）
    pub(super) async fn agy_attach(&self, id: &str) -> Result<Arc<AcpConn>, LocalError> {
        let conn = self.agy_conn().await?;
        let gen = self.agy.generation.load(Ordering::SeqCst);
        let inst = self.inst(id);
        let (session, loaded) = {
            let st = lock(&inst.st);
            (st.binding.as_ref().map(|b| b.session_id.clone()), st.agy.generation == gen)
        };
        if loaded && session.is_some() {
            return Ok(conn);
        }
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let r = match session {
            Some(s) => {
                // 先对上终端再加载：重放的 `session/update` 才记得进来
                lock(&self.agy.sessions).insert(s.clone(), id.to_string());
                agy_log::shared(&s).lock().unwrap_or_else(|e| e.into_inner()).clear();
                match conn.call("session/load", json!({ "sessionId": s, "cwd": row.workdir, "mcpServers": [] })).await {
                    Ok(r) => Some(r),
                    Err(e) => {
                        // 会话没了（agy 里删了）：开一个新的
                        tracing::info!(id, session = s, error = ?e, "agy 会话加载不了，开新的");
                        None
                    }
                }
            }
            None => None,
        };
        let r = match r {
            Some(r) => r,
            None => {
                let r = conn.call("session/new", json!({ "cwd": row.workdir, "mcpServers": [] })).await?;
                let sid = r["sessionId"].as_str().filter(|s| agy_log::valid_session(s)).ok_or_else(|| LocalError::new("AGY_REQUEST_FAILED").with("detail", "no sessionId"))?.to_string();
                self.agy_bind(&inst, &sid);
                r
            }
        };
        let session = Self::agy_session(&inst).ok_or_else(LocalError::not_found)?;
        lock(&self.agy.sessions).insert(session.clone(), id.to_string());
        agy_log::shared(&session).lock().unwrap_or_else(|e| e.into_inner()).loaded = true;
        {
            let mut st = lock(&inst.st);
            st.agy.generation = gen;
            if let Some(c) = r.get("configOptions").filter(|c| c.is_array()) {
                Self::agy_config(&mut st, c.clone());
            }
            if st.phase == Phase::Detached {
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
            }
            st.error = None;
            Self::emit_status(&inst, &st);
        }
        // 权限模式：看看记着的（客户端重启后接着用）；没记过时“全部权限”打开的终端用全部权限
        let mode = self.store.get::<String>(&mode_key(id)).ok().flatten().or_else(|| self.instances.opts(id).full_access.then(|| "bypassPermissions".to_string()));
        if let Some(m) = mode {
            if let Err(e) = self.agy_set_mode(&conn, &inst, &session, &m).await {
                tracing::debug!(id, error = ?e, "agy 的权限模式没换上");
            }
        }
        Ok(conn)
    }

    /// 记下这个终端用的会话（换会话时，原来那个不再跟读）
    fn agy_bind(&self, inst: &Inst, session: &str) {
        let b = Binding { session_id: session.to_string(), transcript_path: agy_log::log_path(session) };
        let old = {
            let mut st = lock(&inst.st);
            let old = st.binding.as_ref().map(|b| b.session_id.clone()).filter(|s| s != session);
            if st.binding.as_ref() != Some(&b) {
                let _ = self.store.set(&super::binding_key(&inst.id), &b);
                st.binding = Some(b);
                let queued = std::mem::take(&mut st.agy.queued);
                st.agy = AgySt { queued, ..Default::default() };
                st.title = None;
                st.context = None;
                st.error = None;
                Self::set_phase(inst, &mut st, Phase::Idle, None);
                Self::emit_status(inst, &st);
            }
            old
        };
        if let Some(old) = old {
            lock(&self.agy.sessions).remove(&old);
            agy_log::drop_log(&old);
        }
        lock(&self.agy.sessions).insert(session.to_string(), inst.id.clone());
    }

    /// 设置项变了：模型、思考强度、模式跟着变
    fn agy_config(st: &mut super::State, config: Value) {
        st.model = config_value(&config, "model").map(String::from).or(st.model.take());
        st.effort = config_value(&config, "thought_level").map(String::from);
        // 全部权限在 agy 那边是 auto：看看自己记着，不被 agy 报的 auto 盖掉
        if let Some(m) = config_value(&config, "mode").and_then(looklook_mode) {
            if !(m == "auto" && st.mode.as_deref() == Some("bypassPermissions")) {
                st.mode = Some(m.to_string());
            }
        }
        st.agy.config = config;
    }

    async fn agy_set_mode(&self, conn: &AcpConn, inst: &Inst, session: &str, mode: &str) -> Result<(), LocalError> {
        let acp = acp_mode(mode).ok_or_else(|| LocalError::invalid("value", "invalid"))?;
        conn.call("session/set_mode", json!({ "sessionId": session, "modeId": acp })).await?;
        let mut st = lock(&inst.st);
        st.mode = Some(mode.to_string());
        Self::emit_status(inst, &st);
        Ok(())
    }

    /// agy 发来的：内容、状态、审批；进程退出了把这个进程里的会话都标成要重新加载
    async fn agy_events(self: Arc<Self>, conn: Arc<AcpConn>, mut rx: mpsc::UnboundedReceiver<Incoming>, gen: u64) {
        while let Some(m) = rx.recv().await {
            match m {
                Incoming::Update { session, update } => self.agy_update(&session, &update),
                Incoming::Permission { id, params } => {
                    let (me, conn) = (self.clone(), conn.clone());
                    tokio::spawn(async move { me.agy_permission(&conn, id, params).await });
                }
                Incoming::Closed => break,
            }
        }
        tracing::info!(gen, "agy acp 退出了");
        let ids: Vec<String> = lock(&self.agy.sessions).values().cloned().collect();
        for id in ids {
            let inst = self.inst(&id);
            let mut st = lock(&inst.st);
            if st.agy.generation != gen {
                continue;
            }
            Self::drop_pending(&mut st);
            st.agy.generation = 0;
            st.agy.turn_at = None;
            if matches!(st.phase, Phase::Running | Phase::Approval) {
                st.error = Some("agy acp exited".into());
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
            }
        }
    }

    fn agy_update(&self, session: &str, u: &Value) {
        let Some(inst) = self.agy_inst_of(session) else { return };
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        let log = agy_log::shared(session);
        lock(&log).update(u);
        let mut st = lock(&inst.st);
        st.active_at = crate::util::system_ms();
        match kind {
            "session_info_update" => {
                if let Some(t) = u["title"].as_str() {
                    st.title = Some(t.chars().take(200).collect()).filter(|t: &String| !t.is_empty());
                    Self::emit_status(&inst, &st);
                }
            }
            "usage_update" => {
                st.agy.window = u["size"].as_u64().or(st.agy.window);
                st.context = u["used"].as_u64().or(st.context);
                Self::emit_status(&inst, &st);
            }
            "current_mode_update" => {
                if let Some(m) = u["currentModeId"].as_str().and_then(looklook_mode) {
                    if !(m == "auto" && st.mode.as_deref() == Some("bypassPermissions")) {
                        st.mode = Some(m.to_string());
                        Self::emit_status(&inst, &st);
                    }
                }
            }
            "config_option_update" => {
                if u["configOptions"].is_array() {
                    Self::agy_config(&mut st, u["configOptions"].clone());
                    Self::emit_status(&inst, &st);
                }
            }
            "available_commands_update" => {
                st.agy.commands = u["availableCommands"].as_array().cloned().unwrap_or_default();
            }
            "tool_call" | "tool_call_update" if st.phase == Phase::Running => {
                let done = matches!(u["status"].as_str(), Some("completed" | "failed"));
                let name = (!done).then(|| u["name"].as_str().map(|n| agy_log::tool_view(n, &Value::Null).0)).flatten();
                if st.tool != name {
                    st.tool = name;
                    Self::emit_status(&inst, &st);
                }
            }
            "agent_message_chunk" if st.tool.is_some() => {
                st.tool = None;
                Self::emit_status(&inst, &st);
            }
            _ => {}
        }
    }

    /// 审批：命令规则 → 全部权限自动放行 → 挂起等界面
    async fn agy_permission(&self, conn: &AcpConn, rid: Value, params: Value) {
        let options = params["options"].clone();
        let session = params["sessionId"].as_str().unwrap_or_default();
        let Some(inst) = self.agy_inst_of(session) else {
            // 不是看看的会话：不替人做决定
            conn.respond(&rid, json!({ "outcome": { "outcome": "cancelled" } }));
            return;
        };
        let call = &params["toolCall"];
        let raw = call.get("rawInput").or(call.get("args")).filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        let (tool, mut input) = agy_log::tool_view(call["name"].as_str().unwrap_or(""), &raw);
        // 审批请求里没带参数时，用之前 `tool_call` 记下的
        if input.as_object().is_none_or(|o| o.is_empty()) {
            if let Some(i) = call["toolCallId"].as_str().and_then(|c| lock(&agy_log::shared(session)).input_of(c)) {
                input = i;
            }
        }
        if !input.is_object() {
            input = json!({ "input": input });
        }
        if let Some(t) = call["title"].as_str() {
            input["title"] = json!(t);
        }
        let cmd = input["command"].as_str().filter(|_| tool == "Bash").unwrap_or_default().to_string();
        let hit = Some(cmd.as_str()).filter(|c| !c.is_empty()).and_then(|c| crate::guard::check(&crate::guard::load(&self.store), c));
        let guard = match hit {
            Some(h) if h.action == crate::guard::Action::Deny => {
                conn.respond(&rid, pick(&options, &["reject_once", "reject_always"]));
                return;
            }
            Some(h) => Some(super::GuardWait::new(h, &cmd)),
            None => None,
        };
        if guard.is_none() && lock(&inst.st).mode.as_deref() == Some("bypassPermissions") {
            conn.respond(&rid, pick(&options, &["allow_once", "allow_always"]));
            return;
        }
        let (tx, rx) = oneshot::channel();
        let pid = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.pending = Some(Pending { id: pid, at: Instant::now(), tool, kind: "permission", input, suggestions: options.clone(), guard, tx: Some(tx) });
            Self::enter(&mut st, Phase::Approval);
            st.phase = Phase::Approval;
            st.reason = None;
            Self::emit_status(&inst, &st);
        }
        // 等界面回答；没答（停了、被顶掉、终端删了）就当作取消
        let out = rx.await.unwrap_or_default();
        let result = serde_json::from_str::<Value>(&out).unwrap_or_else(|_| json!({ "outcome": { "outcome": "cancelled" } }));
        conn.respond(&rid, result);
    }

    // ---------------- 界面的操作 ----------------

    pub(super) async fn agy_input(&self, id: &str, text: &str, images: &[String]) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let phase = lock(&inst.st).phase;
        let trimmed = text.trim();
        let resume_to = trimmed.strip_prefix("/resume ").map(str::trim).filter(|s| !s.is_empty());
        if resume_to.is_some() || matches!(trimmed, "/clear" | "/new") {
            if matches!(phase, Phase::Running | Phase::Approval) {
                return Err(LocalError::new("CHAT_RUNNING"));
            }
            let conn = self.agy_conn().await?;
            match resume_to {
                Some(s) if agy_log::valid_session(s) => self.agy_bind(&inst, s),
                Some(_) => return Err(LocalError::invalid("resume", "invalid")),
                None => {
                    let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
                    let r = conn.call("session/new", json!({ "cwd": row.workdir, "mcpServers": [] })).await?;
                    let sid = r["sessionId"].as_str().filter(|s| agy_log::valid_session(s)).ok_or_else(|| LocalError::new("AGY_REQUEST_FAILED").with("detail", "no sessionId"))?;
                    self.agy_bind(&inst, sid);
                }
            }
            // 新绑的会话还没加载：下面这次 attach 加载（新建的会话也走一遍，拿到设置项）
            lock(&inst.st).agy.generation = 0;
            self.agy_attach(id).await?;
            let _ = inst.tx.send(super::Event::Reset);
            return Ok(json!({ "delivered": true }));
        }
        let images = images.to_vec();
        // 先检查图片，出错时界面把输入框里的字放回来
        tokio::task::block_in_place(|| prompt_content(text, &images))?;
        if matches!(phase, Phase::Running | Phase::Approval) {
            let mut st = lock(&inst.st);
            if st.agy.queued.len() >= QUEUE_LIMIT {
                return Err(LocalError::new("CHAT_BUSY"));
            }
            st.agy.queued.push((text.to_string(), images));
            Self::emit_status(&inst, &st);
            return Ok(json!({ "delivered": true, "queued": true }));
        }
        let conn = self.agy_attach(id).await?;
        self.checkpoint_turn(id, text, true).await;
        self.agy_start_turn(conn, inst, text.to_string(), images)?;
        Ok(json!({ "delivered": true }))
    }

    /// 发出一轮：在后台等 `session/prompt` 返回，结束后发排着的下一条
    fn agy_start_turn(&self, conn: Arc<AcpConn>, inst: Arc<Inst>, text: String, images: Vec<String>) -> Result<(), LocalError> {
        let session = Self::agy_session(&inst).ok_or_else(LocalError::not_found)?;
        let content = tokio::task::block_in_place(|| prompt_content(&text, &images))?;
        let log = agy_log::shared(&session);
        {
            let mut l = lock(&log);
            l.turn_start();
            l.user(&text, images.len() as u32);
        }
        {
            let mut st = lock(&inst.st);
            st.error = None;
            st.agy.turn_at = Some(Instant::now());
            inst.prompts.send_modify(|n| *n += 1);
            Self::set_phase(&inst, &mut st, Phase::Running, None);
        }
        let Some(me) = self.me.upgrade() else { return Err(LocalError::new("CHAT_UNAVAILABLE")) };
        tokio::spawn(async move {
            let r = conn.call_long("session/prompt", json!({ "sessionId": session, "prompt": content })).await;
            me.agy_turn_done(&conn, &inst, &session, r).await;
        });
        Ok(())
    }

    async fn agy_turn_done(&self, conn: &Arc<AcpConn>, inst: &Arc<Inst>, session: &str, r: Result<Value, LocalError>) {
        // 这一轮结束前换了会话：结果不算到现在的会话上
        if Self::agy_session(inst).as_deref() != Some(session) {
            return;
        }
        let log = agy_log::shared(session);
        let next = {
            let mut st = lock(&inst.st);
            let ms = st.agy.turn_at.take().map(|t| t.elapsed().as_millis() as u64);
            match &r {
                Ok(v) => {
                    let u = &v["usage"];
                    let add = |k: &str| u[k].as_u64().unwrap_or(0);
                    st.agy.usage.input += add("inputTokens");
                    st.agy.usage.output += add("outputTokens") + add("thoughtTokens");
                    st.agy.usage.cache_read += add("cachedReadTokens");
                    st.agy.usage.cache_write += add("cachedWriteTokens");
                    let stop = v["stopReason"].as_str();
                    if stop == Some("cancelled") {
                        st.end_hint = Some(super::board::Outcome::Interrupted);
                    }
                    lock(&log).turn_end(stop, None, ms);
                }
                Err(e) => {
                    let why = e.params.get("detail").and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.code.to_string());
                    st.error = Some(why.chars().take(500).collect());
                    lock(&log).turn_end(None, Some(&why), ms);
                }
            }
            Self::drop_pending(&mut st);
            st.last_reply = lock(&log).last_reply();
            // 进程没了：排着的先留着，下次发消息时再发
            let next = if conn.alive() && !st.agy.queued.is_empty() { Some(st.agy.queued.remove(0)) } else { None };
            if next.is_none() {
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
            Self::emit_status(inst, &st);
            next
        };
        if let Some((text, images)) = next {
            self.checkpoint_turn(&inst.id, &text, false).await;
            if let Err(e) = self.agy_start_turn(conn.clone(), inst.clone(), text, images) {
                let mut st = lock(&inst.st);
                st.error = Some(e.code.to_string());
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
        }
    }

    /// 新建会话时带的第一句话、要接着的以前的对话（agy 的对话号就是 ACP 的会话号）
    pub async fn agy_first(&self, id: &str, prompt: Option<String>, resume: Option<crate::instances::Resume>) {
        let r: Result<(), LocalError> = async {
            if let Some(r) = resume.filter(|r| agy_log::valid_session(&r.id)) {
                self.agy_bind(&self.inst(id), &r.id);
            }
            self.agy_attach(id).await?;
            if let Some(p) = prompt {
                self.record_input(id, &p);
                self.agy_input(id, &p, &[]).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(id, error = ?e, "agy 对话没能开始");
            Self::agy_attach_failed(&self.inst(id), &e);
        }
    }

    /// 停止：取消这一轮（agy 那边结束后 `session/prompt` 返回 `cancelled`），排着的也不发了
    pub(super) async fn agy_interrupt(&self, id: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        let session = {
            let mut st = lock(&inst.st);
            if !matches!(st.phase, Phase::Running | Phase::Approval) {
                return Ok(());
            }
            st.agy.queued.clear();
            Self::drop_pending(&mut st);
            st.binding.as_ref().map(|b| b.session_id.clone())
        };
        let Some(session) = session else { return Ok(()) };
        let Some(conn) = self.agy.conn.lock().await.clone().filter(|c| c.alive()) else { return Ok(()) };
        conn.notify("session/cancel", json!({ "sessionId": session }));
        Self::stop_tool(&inst, &mut lock(&inst.st));
        Ok(())
    }

    /// 审批的回答
    pub(super) fn agy_answer(&self, inst: &Arc<Inst>, req: &PermissionAnswer) -> Result<(), LocalError> {
        let mut st = lock(&inst.st);
        let Some(p) = st.pending.as_mut().filter(|p| p.id == req.id) else { return Err(LocalError::new("CHAT_STALE")) };
        // 命令规则标出来的：只能这一次允许
        let result = if p.guard.is_some() && req.decision == "allow_session" { pick(&p.suggestions, &["allow_once"]) } else { decision(&p.suggestions, req)? };
        p.resolve(result.to_string());
        Self::settle(inst, &mut st, Phase::Running);
        Ok(())
    }

    /// 模型、思考强度、权限模式
    pub(super) async fn agy_setting(&self, id: &str, kind: &str, value: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        let ok = !value.is_empty() && value.len() <= 80 && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c));
        if !ok {
            return Err(LocalError::invalid("value", "invalid"));
        }
        let conn = self.agy_attach(id).await?;
        let session = Self::agy_session(&inst).ok_or_else(LocalError::not_found)?;
        match kind {
            "mode" => {
                if inst.brain.load(Ordering::Relaxed) {
                    return Err(LocalError::new("CHAT_MODE_UNAVAILABLE"));
                }
                self.agy_set_mode(&conn, &inst, &session, value).await?;
                let _ = self.store.set(&mode_key(id), &value.to_string());
                Ok(())
            }
            "model" | "effort" => {
                let config_id = if kind == "model" { "model" } else { "thought_level" };
                let r = conn.call("session/set_config_option", json!({ "sessionId": session, "configId": config_id, "value": value })).await?;
                let mut st = lock(&inst.st);
                if r["configOptions"].is_array() {
                    Self::agy_config(&mut st, r["configOptions"].clone());
                }
                if kind == "model" {
                    st.model_set_at = Some(st.model_msg.clone());
                }
                Self::emit_status(&inst, &st);
                Ok(())
            }
            _ => Err(LocalError::invalid("kind", "invalid")),
        }
    }

    pub(super) async fn agy_models(&self, id: &str) -> Result<Value, LocalError> {
        self.agy_attach(id).await?;
        let inst = self.inst(id);
        let st = lock(&inst.st);
        Ok(json!({ "options": model_options(&st.agy.config), "provider": null, "model": st.model, "actual_model": null, "effort": st.effort }))
    }

    /// 斜杠命令菜单：agy 报的命令和技能
    pub(super) async fn agy_commands(&self, id: &str) -> Result<Value, LocalError> {
        self.agy_attach(id).await?;
        let inst = self.inst(id);
        let st = lock(&inst.st);
        let items: Vec<Value> = st
            .agy
            .commands
            .iter()
            .filter_map(|c| Some(json!({ "name": format!("/{}", c["name"].as_str()?), "description": c["description"], "hint": c["input"]["hint"] })))
            .collect();
        Ok(json!({ "items": items }))
    }

    /// 用量栏：累计 token 和上下文占用（agy 没有报订阅额度）
    pub(super) fn agy_usage(&self, id: &str) -> Value {
        let inst = self.inst(id);
        let st = lock(&inst.st);
        let u = st.agy.usage;
        let totals = json!({ "input": u.input + u.cache_write, "cache_read": u.cache_read, "output": u.output });
        json!({ "model": st.model, "context": st.context, "window": st.agy.window, "totals": totals, "limits": null, "version": null })
    }

    /// 排着的消息（界面上显示在输入框上面）
    pub(super) fn agy_queued(st: &super::State) -> impl Iterator<Item = &String> {
        st.agy.queued.iter().map(|(t, _)| t)
    }

    /// 有人看着的时候：agy 进程没了（崩了）就重新起、重新加载
    pub(super) async fn agy_keep(&self, id: &str) {
        let inst = self.inst(id);
        let gen = self.agy.generation.load(Ordering::SeqCst);
        if lock(&inst.st).agy.generation == gen && gen != 0 && self.agy.conn.lock().await.as_ref().is_some_and(|c| c.alive()) {
            return;
        }
        if let Err(e) = self.agy_attach(id).await {
            Self::agy_attach_failed(&inst, &e);
        }
    }

    pub(super) fn agy_attach_failed(inst: &Inst, e: &LocalError) {
        let mut st = lock(&inst.st);
        st.error = e.params.get("detail").and_then(Value::as_str).map(|d| d.chars().take(500).collect()).or(Some(e.code.to_string()));
        let reason = if e.code == "AGY_NOT_FOUND" { "agy_not_found" } else { "agy_unavailable" };
        Self::set_phase(inst, &mut st, Phase::Detached, Some(reason));
    }

    /// 终端删了：关掉 agy 里的会话 worker（会话本身留在 agy 里，历史里还能找回来）
    pub(super) async fn agy_forget(&self, session: Option<String>) {
        let Some(s) = session else { return };
        lock(&self.agy.sessions).remove(&s);
        agy_log::drop_log(&s);
        let conn = self.agy.conn.lock().await.clone().filter(|c| c.alive());
        if let Some(c) = conn {
            c.notify("session/cancel", json!({ "sessionId": s }));
            if let Err(e) = c.call("session/close", json!({ "sessionId": s })).await {
                tracing::debug!(error = ?e, "agy 会话没关上");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ans(d: &str) -> PermissionAnswer {
        PermissionAnswer { id: 1, decision: d.into(), message: None, answers: None }
    }

    /// 真的 agy 走一遍（要装好并登录 agy）：`cargo test --bin looklook agy_end_to_end -- --ignored --nocapture`
    /// 新建会话、发消息拿到回复、审批（允许）后命令真的跑了、停止、重新加载后历史还在
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn agy_end_to_end() {
        use crate::instances::Instances;
        use crate::paths::Paths;
        use crate::store::Store;
        use std::time::Duration;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let dir = work.path().canonicalize().unwrap();
        let paths = Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(Store::memory().unwrap());
        let instances = Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input { name: Some("agy".into()), workdir: Some(dir.display().to_string()), launch: Some("agy".into()), ..Default::default() };
        let id = instances.create(input).await.unwrap().row.id;
        assert!(instances.opts(&id).chat_ui, "agy 默认是对话模式");
        let chat = Chat::new(store.clone(), paths.clone(), instances.clone());
        let snap = chat.snapshot(&id).await.unwrap();
        assert_eq!(snap["status"]["phase"], "idle", "{snap}");
        let wait_idle = |chat: Arc<Chat>, id: String| async move {
            for _ in 0..600 {
                let s = chat.snapshot(&id).await.unwrap();
                if s["status"]["phase"] == "idle" {
                    return s;
                }
                if s["status"]["pending"].is_object() {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            panic!("一直没停下来");
        };
        chat.input(&id, "Reply with exactly the word PONG and nothing else. Do not use any tools.", &[]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let s = wait_idle(chat.clone(), id.clone()).await;
        let text = s["items"].to_string();
        assert!(text.contains("PONG"), "{text}");
        assert!(s["items"][0]["k"] == "user", "{}", s["items"]);
        println!("model={} context={}", s["status"]["model"], s["status"]["context"]);
        let models = chat.models(&id).await.unwrap();
        assert!(models["options"].as_array().unwrap().len() > 1, "{models}");
        // 审批：默认模式下跑命令要问
        chat.setting(&id, "mode", "default").await.unwrap();
        chat.input(&id, "Use your shell command tool to run exactly: echo agy-e2e > marker.txt   Then reply DONE.", &[]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut answered = 0;
        loop {
            let s = wait_idle(chat.clone(), id.clone()).await;
            let p = &s["status"]["pending"];
            if p.is_object() {
                println!("pending: {p}");
                let pid = p["id"].as_u64().unwrap();
                chat.answer(&id, &PermissionAnswer { id: pid, decision: "allow".into(), message: None, answers: None }).unwrap();
                answered += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            break;
        }
        assert!(answered >= 1, "默认模式下应该问过");
        assert_eq!(std::fs::read_to_string(dir.join("marker.txt")).unwrap().trim(), "agy-e2e");
        // 停止
        chat.input(&id, "Count from 1 to 400, one number per line. No tools.", &[]).await.unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
        chat.interrupt(&id).await.unwrap();
        let s = wait_idle(chat.clone(), id.clone()).await;
        assert!(s["items"].as_array().unwrap().iter().any(|i| i["k"] == "interrupt"), "{}", s["items"]);
        // 进程重启后重新加载：历史重放回来
        let before = s["total"].as_u64().unwrap();
        chat.agy.conn.lock().await.as_ref().unwrap().kill();
        tokio::time::sleep(Duration::from_secs(1)).await;
        chat.relaunch(&id).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let s = chat.snapshot(&id).await.unwrap();
        let text = s["items"].to_string();
        assert!(text.contains("PONG") && text.contains("DONE"), "{text}");
        println!("条目 {before} → 重新加载后 {}", s["total"]);
        let session = Chat::agy_session(&chat.inst(&id)).unwrap();
        let conn = chat.agy.conn.lock().await.clone().unwrap();
        conn.call("session/close", json!({ "sessionId": session })).await.unwrap();
    }

    #[test]
    fn permission_answers() {
        let opts = json!([{ "optionId": "allow", "kind": "allow_once" }, { "optionId": "always", "kind": "allow_always" }, { "optionId": "deny", "kind": "reject_once" }]);
        assert_eq!(decision(&opts, &ans("allow")).unwrap()["outcome"]["optionId"], "allow");
        assert_eq!(decision(&opts, &ans("allow_session")).unwrap()["outcome"]["optionId"], "always");
        assert_eq!(decision(&opts, &ans("deny")).unwrap()["outcome"]["optionId"], "deny");
        assert!(decision(&opts, &ans("answer")).is_err());
        // agy 1.5 只给 allow / deny：本会话都允许退回允许一次
        let two = json!([{ "optionId": "allow", "kind": "allow_once" }, { "optionId": "deny", "kind": "reject_once" }]);
        assert_eq!(decision(&two, &ans("allow_session")).unwrap()["outcome"]["optionId"], "allow");
        assert_eq!(decision(&json!([]), &ans("allow")).unwrap()["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn modes_map_both_ways() {
        for m in ["default", "acceptEdits", "plan", "auto"] {
            assert_eq!(looklook_mode(acp_mode(m).unwrap()), Some(m));
        }
        assert_eq!(acp_mode("bypassPermissions"), Some("auto"));
        assert_eq!(acp_mode("dontAsk"), None);
    }

    #[test]
    fn config_to_models() {
        let config = json!([
            { "id": "model", "currentValue": "gemini-4-neon", "options": [{ "value": "gemini-4-neon", "name": "Gemini 4 Neon" }, { "value": "claude-opus-5-5", "name": "Claude Opus 5.5" }] },
            { "id": "thought_level", "currentValue": "high", "options": [{ "value": "low" }, { "value": "high" }] },
            { "id": "mode", "currentValue": "plan", "options": [] }
        ]);
        let m = model_options(&config);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0]["value"].as_str(), m[0]["default"].as_bool()), (Some("gemini-4-neon"), Some(true)));
        assert_eq!(m[1]["efforts"], json!(["low", "high"]));
        let mut st = super::super::State::default();
        Chat::agy_config(&mut st, config.clone());
        assert_eq!((st.model.as_deref(), st.effort.as_deref(), st.mode.as_deref()), (Some("gemini-4-neon"), Some("high"), Some("plan")));
        // 全部权限时 agy 报 auto 不盖掉
        st.mode = Some("bypassPermissions".into());
        let auto = json!([{ "id": "mode", "currentValue": "auto" }]);
        Chat::agy_config(&mut st, auto);
        assert_eq!(st.mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn prompt_with_text_and_images() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"\x89PNG").unwrap();
        let c = prompt_content("look", &[png.display().to_string()]).unwrap();
        assert_eq!(c[0], json!({ "type": "text", "text": "look" }));
        assert_eq!((c[1]["type"].as_str(), c[1]["mimeType"].as_str()), (Some("image"), Some("image/png")));
        assert!(prompt_content("x", &[dir.path().join("a.bmp").display().to_string()]).is_err());
        assert_eq!(prompt_content("", &[]).unwrap(), json!([{ "type": "text", "text": "" }]));
    }
}
