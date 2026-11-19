//! 对话模式的 Antigravity（agy）：看看通过 `agy acp --process-per-session` 驱动对话，
//! 界面是看看自己的对话界面，终端标签里是同一个工作目录下的普通 shell。
//!
//! - 所有对话模式的 agy 终端共用一个 `agy acp` 进程（`agy_app.rs`），每个终端是其中的一个 session
//!   （会话号记在 [`Binding`] 里）。
//! - 对话内容来自 `session/update`（`agy_log.rs`），正在输出的回复推到 `st.live.text`；
//!   打开已有会话时通过 `session/load` 回放历史。
//! - 审批通过 `session/request_permission` 挂起等界面回答；命中命令安全网（`guard.rs`）时先拦截/强制确认；
//!   全部权限模式（`bypassPermissions`）下未命中安全网的审批自动放行。
//! - 发消息 `session/prompt`，停止 `session/cancel`，`/clear` / `/new` 建新会话，`/resume <id>` 切到已有会话，
//!   模型与思考强度走 `session/set_config_option`，权限模式走 `session/set_mode`。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::agy_app::{AgyApp, Incoming, CALL_TIMEOUT};
use super::agy_log;
use super::{lock, Binding, Chat, Inst, Pending, Phase, PermissionAnswer};
use crate::error::LocalError;

const PREVIEW_LIMIT: usize = 4000;
const PREVIEW_GAP: Duration = Duration::from_millis(120);
const MODELS_TTL: Duration = Duration::from_secs(120);
const IMAGE_LIMIT: u64 = 20 << 20;

pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Default)]
pub struct Hub {
    app: tokio::sync::Mutex<Option<Arc<AgyApp>>>,
    generation: AtomicU64,
    /// agy session_id → looklook instance id
    sessions: Mutex<HashMap<String, String>>,
    /// 正在执行 `session/load` 回放的会话集合
    replaying: Mutex<HashSet<String>>,
    /// 缓存的模型菜单（草稿页和对话页共用）
    models: Mutex<Option<(Instant, Value)>>,
}

/// 每个 agy 终端多记的状态
#[derive(Default)]
pub struct AgySt {
    pub generation: u64,
    pub cwd: Option<String>,
    pub commands: Vec<Value>,
    pub models: Vec<Value>,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub window: Option<u64>,
    /// 全部权限（bypassPermissions）：未命中安全网的权限请求自动放行
    pub bypass: bool,
    turn_started: Option<Instant>,
    preview: String,
    preview_at: Option<Instant>,
}

/// Looklook 权限模式 → (ACP modeId, bypass)
fn to_acp_mode(mode: &str) -> Option<(&'static str, bool)> {
    match mode {
        "default" => Some(("default", false)),
        "acceptEdits" => Some(("accept-edits", false)),
        "plan" => Some(("plan", false)),
        "auto" => Some(("auto", false)),
        "bypassPermissions" => Some(("auto", true)),
        _ => None,
    }
}

/// ACP modeId → Looklook 权限模式
fn from_acp_mode(mode: &str, bypass: bool) -> &'static str {
    if bypass {
        return "bypassPermissions";
    }
    match mode {
        "accept-edits" => "acceptEdits",
        "plan" => "plan",
        "auto" => "auto",
        _ => "default",
    }
}

/// 从 `session/new` / `session/load` 返回值解析模型菜单（按基模型分组，每个模型附带支持的思考强度）
pub fn model_options_from_session(resp: &Value) -> Vec<Value> {
    let available = resp["models"]["availableModels"].as_array();
    let cfg = resp["configOptions"].as_array();
    let model_cfg = cfg.and_then(|a| a.iter().find(|c| c["id"] == "model"));
    let thought_cfg = cfg.and_then(|a| a.iter().find(|c| c["id"] == "thought_level"));
    let cur_model = model_cfg.and_then(|c| c["currentValue"].as_str());
    let cur_thought = thought_cfg.and_then(|c| c["currentValue"].as_str());

    if let Some(opts) = model_cfg.and_then(|c| c["options"].as_array()) {
        let mut out = vec![];
        for o in opts {
            let Some(val) = o["value"].as_str() else { continue };
            let label = o["name"].as_str().unwrap_or(val);
            let mut efforts: Vec<&str> = vec![];
            if let Some(av) = available {
                let prefix = format!("{val}-");
                for m in av {
                    if let Some(mid) = m["modelId"].as_str() {
                        if let Some(suf) = mid.strip_prefix(&prefix) {
                            if let Some(&e) = EFFORTS.iter().find(|&&e| e == suf) {
                                if !efforts.contains(&e) {
                                    efforts.push(e);
                                }
                            }
                        }
                    }
                }
            }
            efforts.sort_by_key(|e| EFFORTS.iter().position(|x| x == e).unwrap_or(99));
            let default_effort = if cur_model == Some(val) {
                cur_thought.filter(|t| efforts.contains(t)).or_else(|| efforts.first().copied())
            } else if efforts.contains(&"high") {
                Some("high")
            } else {
                efforts.first().copied()
            };
            out.push(json!({
                "value": val,
                "label": label,
                "description": "",
                "section": "main",
                "model": val,
                "efforts": efforts,
                "default_effort": default_effort,
                "default": cur_model == Some(val),
            }));
        }
        if !out.is_empty() {
            return out;
        }
    }

    // 兜底：没有 configOptions 时直接用 availableModels
    available
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["modelId"].as_str()?;
            let label = m["name"].as_str().unwrap_or(id);
            Some(json!({
                "value": id,
                "label": label,
                "description": "",
                "section": "main",
                "model": id,
                "efforts": [],
                "default": resp["models"]["currentModelId"].as_str() == Some(id),
            }))
        })
        .collect()
}

/// 解析 `agy models` 命令行输出（草稿页还没起 ACP 时用）
pub fn model_options_from_cli(output: &str) -> Vec<Value> {
    let mut order: Vec<String> = vec![];
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut efforts_map: HashMap<String, Vec<&'static str>> = HashMap::new();

    for raw in output.lines() {
        // 去掉进度旋转行（含 \r 时取最后一段）
        let line = raw.rsplit('\r').next().unwrap_or(raw).trim();
        if line.is_empty() || line.starts_with("Fetching") {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(id) = parts.next().filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))) else {
            continue;
        };
        let label = parts.next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id);

        // 拆出基模型与末尾的思考强度（例如 gemini-4-neon-high → gemini-4-neon + high）
        let split = EFFORTS.iter().find_map(|&e| {
            let base = id.strip_suffix(&format!("-{e}"))?;
            // 像 gpt-oss-120b-medium 只有一档时也按完整 id 保留，只有 label 以 `(...)` 结尾时才拆
            if label.ends_with(')') && !base.is_empty() {
                Some((base, e))
            } else {
                None
            }
        });
        let (base, effort, base_label) = match split {
            Some((b, e)) => {
                let lbl = label.rsplit_once(" (").map(|(h, _)| h.trim()).unwrap_or(label);
                (b.to_string(), Some(e), lbl.to_string())
            }
            None => (id.to_string(), None, label.to_string()),
        };
        if !order.contains(&base) {
            order.push(base.clone());
            labels.insert(base.clone(), base_label);
        }
        if let Some(e) = effort {
            let v = efforts_map.entry(base).or_default();
            if !v.contains(&e) {
                v.push(e);
            }
        }
    }

    order
        .into_iter()
        .enumerate()
        .map(|(i, base)| {
            let mut eff = efforts_map.remove(&base).unwrap_or_default();
            // 只有一个档位的模型（如 gpt-oss-120b-medium）：用完整 id、不拆强度
            let (value, label, efforts, default_effort) = if eff.len() == 1 && !base.starts_with("gemini-") && !base.starts_with("claude-") {
                let full = format!("{base}-{}", eff[0]);
                let lbl = format!("{} ({})", labels.get(&base).cloned().unwrap_or_else(|| base.clone()), cap(eff[0]));
                (full, lbl, vec![], None)
            } else {
                eff.sort_by_key(|e| EFFORTS.iter().position(|x| x == e).unwrap_or(99));
                let def = if eff.contains(&"high") { Some("high") } else { eff.first().copied() };
                (base.clone(), labels.get(&base).cloned().unwrap_or(base), eff, def)
            };
            json!({
                "value": value,
                "label": label,
                "description": "",
                "section": "main",
                "model": value,
                "efforts": efforts,
                "default_effort": default_effort,
                "default": i == 0,
            })
        })
        .collect()
}

fn cap(s: &str) -> &'static str {
    match s {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "XHigh",
        "max" => "Max",
        _ => "",
    }
}

/// 从 `session/request_permission` 的 `options` 里挑最合适的 optionId
fn pick_option(options: &Value, want_kinds: &[&str], fallback: &str) -> String {
    if let Some(arr) = options.as_array() {
        for kind in want_kinds {
            if let Some(id) = arr.iter().find(|o| o["kind"].as_str() == Some(*kind)).and_then(|o| o["optionId"].as_str()) {
                return id.to_string();
            }
        }
        if let Some(id) = arr.first().and_then(|o| o["optionId"].as_str()) {
            return id.to_string();
        }
    }
    fallback.to_string()
}

/// 界面的回答 → ACP `session/request_permission` 的返回值
pub fn decision(suggestions: &Value, req: &PermissionAnswer) -> Result<Value, LocalError> {
    let opt = match req.decision.as_str() {
        "allow" => pick_option(suggestions, &["allow_once", "allow_always"], "allow"),
        "allow_session" => pick_option(suggestions, &["allow_always", "allow_once"], "allow"),
        "deny" => pick_option(suggestions, &["reject_once", "reject_always"], "deny"),
        _ => return Err(LocalError::invalid("decision", "invalid")),
    };
    Ok(json!({ "outcome": { "outcome": "selected", "optionId": opt } }))
}

fn image_type(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// 发给 ACP `session/prompt` 的内容块，同时返回图片原文以便写入内存记录
fn prompt_blocks(text: &str, images: &[String]) -> Result<(Value, Vec<(&'static str, Vec<u8>)>), LocalError> {
    use base64::Engine;
    let mut parts = vec![];
    let mut raw_imgs = vec![];
    if !text.trim().is_empty() || images.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for p in images {
        let path = std::path::Path::new(p);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mime = image_type(&ext).ok_or_else(|| LocalError::invalid("images", "unsupported"))?;
        if std::fs::metadata(path).map_err(|_| LocalError::invalid("images", "not_found"))?.len() > IMAGE_LIMIT {
            return Err(LocalError::invalid("images", "too_large"));
        }
        let data = std::fs::read(path).map_err(|_| LocalError::invalid("images", "not_found"))?;
        parts.push(json!({
            "type": "image",
            "mimeType": mime,
            "data": base64::engine::general_purpose::STANDARD.encode(&data),
        }));
        raw_imgs.push((mime, data));
    }
    Ok((Value::Array(parts), raw_imgs))
}

impl Chat {
    pub(super) fn is_agy(&self, id: &str) -> bool {
        self.inst(id).agent.launch() == "agy"
    }

    /// 新版本装好了：没有在忙的 agy 会话时结束旧进程，下次用自动起新的
    pub fn agy_installed(&self) {
        let busy = lock(&self.insts)
            .values()
            .filter(|i| i.agent.launch() == "agy")
            .any(|i| matches!(lock(&i.st).phase, Phase::Running | Phase::Approval));
        if busy {
            return;
        }
        if let Ok(mut slot) = self.agy.app.try_lock() {
            if let Some(app) = slot.take() {
                app.kill();
            }
        }
    }

    fn agy_inst_of(&self, session: &str) -> Option<Arc<Inst>> {
        let id = lock(&self.agy.sessions).get(session).cloned()?;
        Some(self.inst(&id))
    }

    /// 连上（或启动）`agy acp`
    async fn agy_app(&self) -> Result<Arc<AgyApp>, LocalError> {
        let mut slot = self.agy.app.lock().await;
        if let Some(a) = slot.as_ref().filter(|a| a.alive()) {
            return Ok(a.clone());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let app = AgyApp::start(self.instances.shell(), tx).await?;
        let gen = self.agy.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *slot = Some(app.clone());
        drop(slot);
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(me.agy_loop(rx, gen));
        }
        Ok(app)
    }

    /// 处理 `agy acp` 推来的通知与权限请求
    async fn agy_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Incoming>, gen: u64) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Incoming::Notification { method, params } => {
                    if method == "session/update" {
                        self.agy_update(&params);
                    }
                }
                Incoming::Request { id, method, params } => {
                    let me = self.clone();
                    tokio::spawn(async move { me.agy_request(id, &method, params).await });
                }
                Incoming::Closed { stderr } => {
                    if self.agy.generation.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let ids: Vec<String> = lock(&self.agy.sessions).values().cloned().collect();
                    let detail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("Antigravity ACP disconnected");
                    for id in ids {
                        let inst = self.inst(&id);
                        let mut st = lock(&inst.st);
                        Self::drop_pending(&mut st);
                        st.agy.generation = 0;
                        if st.phase != Phase::Detached {
                            st.error = Some(detail.chars().take(400).collect());
                            Self::set_phase(&inst, &mut st, Phase::Detached, Some("agy_unavailable"));
                        }
                    }
                    return;
                }
            }
        }
    }

    fn agy_update(&self, params: &Value) {
        let Some(sid) = params["sessionId"].as_str() else { return };
        let Some(inst) = self.agy_inst_of(sid) else { return };
        let u = &params["update"];
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        let replaying = lock(&self.agy.replaying).contains(sid);

        // 先写入内存日志（回放和实时都走同一套解析）
        let model = lock(&inst.st).model.clone();
        if matches!(kind, "user_message_chunk" | "agent_thought_chunk" | "agent_message_chunk" | "tool_call" | "tool_call_update") {
            let log = agy_log::shared(sid);
            lock(&log).on_update(u, model.as_deref());
        }

        let mut st = lock(&inst.st);
        st.active_at = crate::util::system_ms();
        match kind {
            "available_commands_update" => {
                let items: Vec<Value> = u["availableCommands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        let name = c["name"].as_str()?;
                        Some(json!({
                            "name": format!("/{name}"),
                            "description": c["description"].as_str().unwrap_or(""),
                        }))
                    })
                    .collect();
                st.agy.commands = items;
            }
            "session_info_update" => {
                if let Some(t) = u["title"].as_str().filter(|t| !t.is_empty()) {
                    st.title = Some(t.to_string());
                    Self::emit_status(&inst, &st);
                }
            }
            "usage_update" => {
                let mut changed = false;
                if let Some(used) = u["used"].as_u64() {
                    if st.context != Some(used) {
                        st.context = Some(used);
                        changed = true;
                    }
                }
                if let Some(size) = u["size"].as_u64().filter(|s| *s > 0) {
                    st.agy.window = Some(size);
                }
                if changed {
                    Self::emit_status(&inst, &st);
                }
            }
            "config_option_update" => {
                Self::agy_apply_config_options(&mut st, &u["configOptions"]);
                Self::emit_status(&inst, &st);
            }
            "current_mode_update" => {
                if let Some(m) = u["currentModeId"].as_str() {
                    st.mode = Some(from_acp_mode(m, st.agy.bypass).to_string());
                    Self::emit_status(&inst, &st);
                }
            }
            "agent_thought_chunk" if !replaying => {
                if st.agy.preview.is_empty() && st.live.activity.as_deref() != Some("thinking") {
                    st.live.activity = Some("thinking".into());
                    Self::emit_status(&inst, &st);
                }
            }
            "agent_message_chunk" if !replaying => {
                if let Some(t) = u["content"]["text"].as_str() {
                    st.agy.preview.push_str(t);
                    let n = st.agy.preview.chars().count();
                    if n > PREVIEW_LIMIT {
                        st.agy.preview = st.agy.preview.chars().skip(n - PREVIEW_LIMIT).collect();
                    }
                    if st.agy.preview_at.is_none_or(|at| at.elapsed() >= PREVIEW_GAP) {
                        st.agy.preview_at = Some(Instant::now());
                        st.live.activity = None;
                        st.live.text = Some(st.agy.preview.clone());
                        Self::emit_status(&inst, &st);
                    }
                }
            }
            "tool_call" | "tool_call_update" if !replaying => {
                let status = u["status"].as_str().unwrap_or("");
                match status {
                    "in_progress" | "pending" => {
                        let raw_name = u["name"].as_str().unwrap_or("tool");
                        let raw_in = if !u["rawInput"].is_null() { &u["rawInput"] } else { &u["args"] };
                        let (tool_name, mapped) = agy_log::map_tool(raw_name, raw_in);
                        st.agy.preview.clear();
                        st.live.text = None;
                        st.live.activity = None;
                        st.tool = Some(tool_name.clone());
                        st.doing = Some(super::board::doing(&tool_name, &mapped));
                        Self::emit_status(&inst, &st);
                    }
                    "completed" | "failed" => {
                        st.tool = None;
                        st.doing = None;
                        Self::emit_status(&inst, &st);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn agy_apply_config_options(st: &mut super::State, cfg: &Value) {
        for c in cfg.as_array().into_iter().flatten() {
            match c["id"].as_str().unwrap_or("") {
                "model" => {
                    if let Some(v) = c["currentValue"].as_str() {
                        st.model = Some(v.to_string());
                        st.actual_model = Some(v.to_string());
                    }
                }
                "thought_level" => {
                    if let Some(v) = c["currentValue"].as_str() {
                        st.effort = Some(v.to_string());
                    }
                }
                "mode" => {
                    if let Some(v) = c["currentValue"].as_str() {
                        st.mode = Some(from_acp_mode(v, st.agy.bypass).to_string());
                    }
                }
                _ => {}
            }
        }
    }

    /// 处理 `agy acp` 发起的请求（主要是 `session/request_permission`）
    async fn agy_request(&self, req_id: Value, method: &str, params: Value) {
        let Some(app) = self.agy.app.lock().await.clone() else { return };
        if method != "session/request_permission" {
            let _ = app.respond(req_id, json!({})).await;
            return;
        }
        let sid = params["sessionId"].as_str().unwrap_or_default();
        let Some(inst) = self.agy_inst_of(sid) else {
            let _ = app.respond(req_id, json!({ "outcome": { "outcome": "cancelled" } })).await;
            return;
        };
        let options = params["options"].clone();
        let tc = &params["toolCall"];
        let raw_name = tc["name"].as_str().unwrap_or("tool");
        let raw_in = if !tc["rawInput"].is_null() { &tc["rawInput"] } else { &tc["args"] };
        let (tool, mut input) = agy_log::map_tool(raw_name, raw_in);
        if let Some(title) = tc["title"].as_str().filter(|t| !t.is_empty()) {
            if input.is_object() && input["description"].is_null() {
                input["description"] = json!(title);
            }
        }

        // 先跑看看的命令安全网（guard.rs）
        let cmd = input["command"].as_str().filter(|_| tool == "Bash").unwrap_or_default().to_string();
        let guard = match Some(cmd.as_str()).filter(|c| !c.is_empty()).and_then(|c| crate::guard::check(&crate::guard::load(&self.store), c)) {
            Some(hit) if hit.action == crate::guard::Action::Deny => {
                let deny_opt = pick_option(&options, &["reject_once", "reject_always"], "deny");
                let _ = app.respond(req_id, json!({ "outcome": { "outcome": "selected", "optionId": deny_opt } })).await;
                return;
            }
            hit => hit.map(|h| super::GuardWait::new(h, &cmd)),
        };

        // 全部权限模式下未命中安全网：自动同意
        let bypass = lock(&inst.st).agy.bypass;
        if bypass && guard.is_none() {
            let allow_opt = pick_option(&options, &["allow_once", "allow_always"], "allow");
            let _ = app.respond(req_id, json!({ "outcome": { "outcome": "selected", "optionId": allow_opt } })).await;
            return;
        }

        let (tx, rx) = oneshot::channel();
        let pid = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.pending = Some(Pending {
                id: pid,
                at: Instant::now(),
                tool,
                kind: "permission",
                input,
                suggestions: options,
                guard,
                tx: Some(tx),
            });
            Self::enter(&mut st, Phase::Approval);
            st.phase = Phase::Approval;
            st.reason = None;
            Self::emit_status(&inst, &st);
        }

        let out = rx.await.unwrap_or_default();
        let resp = serde_json::from_str::<Value>(&out).unwrap_or_else(|_| json!({ "outcome": { "outcome": "cancelled" } }));
        if let Err(e) = app.respond(req_id, resp).await {
            tracing::debug!(error = ?e, "agy 审批答复没送到");
        }
    }

    /// 确保这个终端已经在当前 `agy acp` 进程里加载好会话
    pub(super) async fn agy_attach(&self, id: &str) -> Result<Arc<AgyApp>, LocalError> {
        let app = self.agy_app().await?;
        let gen = self.agy.generation.load(Ordering::SeqCst);
        let inst = self.inst(id);
        let (session, loaded) = {
            let st = lock(&inst.st);
            (st.binding.as_ref().map(|b| b.session_id.clone()), st.agy.generation == gen)
        };
        if loaded && session.is_some() {
            return Ok(app);
        }
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let opts = self.instances.opts(id);
        match session {
            Some(sid) => {
                if let Err(e) = self.agy_load_session(&app, &inst, &sid, &row.workdir, gen, opts.full_access).await {
                    tracing::warn!(id, session = %sid, error = ?e, "agy session/load 失败，新建会话");
                    self.agy_new_session(&app, &inst, &row.workdir, gen, opts.full_access).await?;
                }
            }
            None => {
                self.agy_new_session(&app, &inst, &row.workdir, gen, opts.full_access).await?;
            }
        }
        Ok(app)
    }

    async fn agy_new_session(&self, app: &AgyApp, inst: &Inst, workdir: &str, gen: u64, full_access: bool) -> Result<String, LocalError> {
        let resp = app.call("session/new", json!({ "cwd": workdir, "mcpServers": [] }), CALL_TIMEOUT).await?;
        let sid = resp["sessionId"]
            .as_str()
            .filter(|s| agy_log::valid_session(s))
            .ok_or_else(|| LocalError::new("AGY_REQUEST_FAILED").with("detail", "no sessionId"))?
            .to_string();
        self.agy_bind(inst, &sid, workdir, gen, &resp, app.version(), full_access);
        if full_access {
            let _ = app.call("session/set_mode", json!({ "sessionId": sid, "modeId": "auto" }), CALL_TIMEOUT).await;
        }
        Ok(sid)
    }

    async fn agy_load_session(&self, app: &AgyApp, inst: &Inst, sid: &str, workdir: &str, gen: u64, full_access: bool) -> Result<(), LocalError> {
        // 先登记 session → inst，再标记 replaying，这样 `session/load` 期间推来的历史事件都能正确落入 AgyLog
        lock(&self.agy.sessions).insert(sid.to_string(), inst.id.clone());
        lock(&self.agy.replaying).insert(sid.to_string());
        let log = agy_log::shared(sid);
        lock(&log).begin_replay();
        let res = app.call("session/load", json!({ "sessionId": sid, "cwd": workdir, "mcpServers": [] }), CALL_TIMEOUT).await;
        lock(&self.agy.replaying).remove(sid);
        match res {
            Ok(resp) => {
                lock(&log).end_replay();
                self.agy_bind(inst, sid, workdir, gen, &resp, app.version(), full_access);
                if full_access {
                    let _ = app.call("session/set_mode", json!({ "sessionId": sid, "modeId": "auto" }), CALL_TIMEOUT).await;
                }
                let _ = inst.tx.send(super::Event::Reset);
                Ok(())
            }
            Err(e) => {
                lock(&log).end_replay();
                Err(e)
            }
        }
    }

    fn agy_bind(&self, inst: &Inst, session: &str, workdir: &str, gen: u64, resp: &Value, version: Option<String>, full_access: bool) {
        let b = Binding { session_id: session.to_string(), transcript_path: agy_log::log_path(session) };
        let opts = model_options_from_session(resp);
        if !opts.is_empty() {
            *lock(&self.agy.models) = Some((Instant::now(), json!(opts)));
        }
        let old = {
            let mut st = lock(&inst.st);
            let old = st.binding.as_ref().map(|x| x.session_id.clone()).filter(|s| s != session);
            let changed = st.binding.as_ref() != Some(&b);
            if changed {
                let _ = self.store.set(&super::binding_key(&inst.id), &b);
                st.binding = Some(b);
                st.agy = AgySt::default();
                st.title = None;
                st.context = None;
                st.error = None;
            }
            st.agy.generation = gen;
            st.agy.cwd = Some(workdir.to_string());
            st.agy.models = opts;
            st.agy.bypass = full_access;
            if let Some(v) = version {
                st.version = Some(v);
            }
            Self::agy_apply_config_options(&mut st, &resp["configOptions"]);
            if st.mode.is_none() {
                let m = resp["modes"]["currentModeId"].as_str().unwrap_or("default");
                st.mode = Some(from_acp_mode(m, full_access).to_string());
            } else if full_access {
                st.mode = Some("bypassPermissions".into());
            }
            if st.phase == Phase::Detached {
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            } else {
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

    /// 新建 agy 对话会话时带了第一句话或要接着以前的对话（gateway/api.rs 调用）
    pub async fn agy_first(&self, id: &str, prompt: Option<String>, resume: Option<crate::instances::Resume>) {
        if let Some(r) = resume {
            let inst = self.inst(id);
            let b = Binding { session_id: r.id.clone(), transcript_path: agy_log::log_path(&r.id) };
            let _ = self.store.set(&super::binding_key(id), &b);
            lock(&inst.st).binding = Some(b);
            if let Err(e) = self.agy_attach(id).await {
                let mut st = lock(&inst.st);
                st.error = e.params.get("detail").and_then(Value::as_str).map(String::from).or(Some(e.code.to_string()));
                let reason = if e.code == "AGY_NOT_FOUND" { "agy_not_found" } else { "agy_unavailable" };
                Self::set_phase(&inst, &mut st, Phase::Detached, Some(reason));
            }
        }
        if let Some(p) = prompt {
            if let Err(e) = self.agy_input(id, &p, &[]).await {
                tracing::warn!(id, error = ?e, "agy 第一句话发送失败");
            }
        }
    }

    // ---------------- 界面的操作 ----------------

    pub(super) async fn agy_input(&self, id: &str, text: &str, images: &[String]) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let phase = lock(&inst.st).phase;
        if phase == Phase::Approval {
            return Err(LocalError::new("CHAT_BUSY"));
        }
        let trimmed = text.trim();
        let resume_to = trimmed.strip_prefix("/resume ").map(str::trim).filter(|s| !s.is_empty());
        if resume_to.is_some() || matches!(trimmed, "/clear" | "/new") {
            if phase == Phase::Running {
                return Err(LocalError::new("CHAT_RUNNING"));
            }
            let app = self.agy_app().await?;
            let gen = self.agy.generation.load(Ordering::SeqCst);
            let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
            let opts = self.instances.opts(id);
            let old = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone());
            if let Some(old_sid) = old {
                let _ = app.call("session/close", json!({ "sessionId": old_sid }), Duration::from_secs(5)).await;
            }
            match resume_to {
                Some(s) if agy_log::valid_session(s) => {
                    self.agy_load_session(&app, &inst, s, &row.workdir, gen, opts.full_access).await?;
                }
                Some(_) => return Err(LocalError::invalid("resume", "invalid")),
                None => {
                    self.agy_new_session(&app, &inst, &row.workdir, gen, opts.full_access).await?;
                    let _ = inst.tx.send(super::Event::Reset);
                }
            }
            return Ok(json!({ "delivered": true }));
        }
        if phase == Phase::Running {
            return Err(LocalError::new("CHAT_RUNNING"));
        }

        let app = self.agy_attach(id).await?;
        let session = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone()).ok_or_else(LocalError::not_found)?;

        self.checkpoint_turn(id, text, true).await;
        let (prompt, raw_imgs) = tokio::task::block_in_place(|| prompt_blocks(text, images))?;

        let log = agy_log::shared(&session);
        lock(&log).push_user(text, raw_imgs, false);
        {
            let mut st = lock(&inst.st);
            st.error = None;
            st.agy.preview.clear();
            st.agy.turn_started = Some(Instant::now());
            Self::note_prompt(&mut st, text);
            inst.prompts.send_modify(|n| *n += 1);
            Self::set_phase(&inst, &mut st, Phase::Running, None);
        }

        // `session/prompt` 一直阻塞到本轮完成，放到后台任务里跑，立刻返回 delivered: true
        if let Some(me) = self.me.upgrade() {
            let sid = session.clone();
            let inst_c = inst.clone();
            tokio::spawn(async move {
                let res = app.call("session/prompt", json!({ "sessionId": sid, "prompt": prompt }), None).await;
                me.agy_turn_done(&inst_c, &sid, res);
            });
        }
        Ok(json!({ "delivered": true }))
    }

    fn agy_turn_done(&self, inst: &Arc<Inst>, session: &str, res: Result<Value, LocalError>) {
        let log = agy_log::shared(session);
        let elapsed_ms = lock(&inst.st).agy.turn_started.take().map(|t| t.elapsed().as_millis() as u64);
        match res {
            Ok(v) => {
                let stop = v["stopReason"].as_str().unwrap_or("end_turn");
                lock(&log).end_turn(stop, elapsed_ms, None);
                let mut st = lock(&inst.st);
                Self::drop_pending(&mut st);
                st.agy.preview.clear();
                st.live = Default::default();
                if let Some(u) = v["usage"].as_object() {
                    st.agy.input_tokens += u.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
                    st.agy.cache_read_tokens += u.get("cachedReadTokens").and_then(Value::as_u64).unwrap_or(0);
                    st.agy.output_tokens += u.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
                }
                if stop == "cancelled" {
                    st.end_hint = Some(super::board::Outcome::Interrupted);
                }
                st.last_reply = lock(&log).last_reply();
                Self::set_phase(inst, &mut st, Phase::Idle, None);
                Self::emit_status(inst, &st);
            }
            Err(e) => {
                let msg = e.params.get("detail").and_then(Value::as_str).unwrap_or(&e.code).to_string();
                lock(&log).end_turn("error", elapsed_ms, Some(&msg));
                let mut st = lock(&inst.st);
                Self::drop_pending(&mut st);
                st.agy.preview.clear();
                st.live = Default::default();
                st.error = Some(msg.chars().take(500).collect());
                st.last_reply = lock(&log).last_reply();
                Self::set_phase(inst, &mut st, Phase::Idle, None);
                Self::emit_status(inst, &st);
            }
        }
    }

    pub(super) async fn agy_interrupt(&self, id: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        if !matches!(lock(&inst.st).phase, Phase::Running | Phase::Approval) {
            return Ok(());
        }
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            Self::stop_tool(&inst, &mut st);
        }
        let Some(app) = self.agy.app.lock().await.clone().filter(|a| a.alive()) else { return Ok(()) };
        let Some(session) = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone()) else { return Ok(()) };
        let _ = app.notify("session/cancel", json!({ "sessionId": session })).await;
        Ok(())
    }

    pub(super) async fn agy_setting(&self, id: &str, kind: &str, value: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        let app = self.agy_attach(id).await?;
        let session = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone()).ok_or_else(LocalError::not_found)?;
        match kind {
            "mode" => {
                let Some((acp_mode, bypass)) = to_acp_mode(value) else {
                    return Err(LocalError::invalid("value", "invalid"));
                };
                app.call("session/set_mode", json!({ "sessionId": session, "modeId": acp_mode }), CALL_TIMEOUT).await?;
                let mut st = lock(&inst.st);
                st.agy.bypass = bypass;
                st.mode = Some(value.to_string());
                Self::emit_status(&inst, &st);
                Ok(())
            }
            "model" => {
                let ok = !value.is_empty() && value.len() <= 80 && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c));
                if !ok {
                    return Err(LocalError::invalid("value", "invalid"));
                }
                let r = app
                    .call("session/set_config_option", json!({ "sessionId": session, "configId": "model", "value": value }), CALL_TIMEOUT)
                    .await;
                let r = match r {
                    Ok(v) => v,
                    // 自定义完整 modelId 时退回 `session/set_model`
                    Err(_) => app.call("session/set_model", json!({ "sessionId": session, "modelId": value }), CALL_TIMEOUT).await?,
                };
                let mut st = lock(&inst.st);
                if r["configOptions"].is_array() {
                    Self::agy_apply_config_options(&mut st, &r["configOptions"]);
                } else {
                    st.model = Some(value.to_string());
                }
                st.model_set_at = Some(st.model_msg.clone());
                Self::emit_status(&inst, &st);
                super::model_memory::record(&self.store, "agy", "default", "model", value, super::model_memory::agy_base());
                Ok(())
            }
            "effort" => {
                if !EFFORTS.contains(&value) {
                    return Err(LocalError::invalid("value", "invalid"));
                }
                let r = app
                    .call("session/set_config_option", json!({ "sessionId": session, "configId": "thought_level", "value": value }), CALL_TIMEOUT)
                    .await?;
                let mut st = lock(&inst.st);
                if r["configOptions"].is_array() {
                    Self::agy_apply_config_options(&mut st, &r["configOptions"]);
                } else {
                    st.effort = Some(value.to_string());
                }
                Self::emit_status(&inst, &st);
                super::model_memory::record(&self.store, "agy", "default", "effort", value, super::model_memory::agy_base());
                Ok(())
            }
            _ => Err(LocalError::invalid("kind", "invalid")),
        }
    }

    pub(super) async fn agy_models(&self, id: &str) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let options = {
            let local = lock(&inst.st).agy.models.clone();
            if !local.is_empty() {
                json!(local)
            } else {
                self.agy_cached_models().await?
            }
        };
        let st = lock(&inst.st);
        Ok(json!({
            "options": options,
            "provider": null,
            "model": st.model,
            "actual_model": st.actual_model,
            "effort": st.effort,
        }))
    }

    /// 草稿页的模型菜单：有缓存直接用，没有则调用 `agy models`
    pub(super) async fn agy_cached_models(&self) -> Result<Value, LocalError> {
        if let Some(v) = lock(&self.agy.models).as_ref().filter(|(t, _)| t.elapsed() < MODELS_TTL).map(|(_, v)| v.clone()) {
            return Ok(v);
        }
        let Some(cli) = crate::agents::agy_cli(self.instances.shell()).await else {
            return Err(LocalError::new("AGY_NOT_FOUND"));
        };
        let out = crate::agents::cli_output(self.instances.shell(), &cli, &["models"], &[], Duration::from_secs(20))
            .await
            .map_err(|e| LocalError::new("AGY_REQUEST_FAILED").with("detail", e))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let opts = json!(model_options_from_cli(&text));
        if opts.as_array().is_some_and(|a| !a.is_empty()) {
            *lock(&self.agy.models) = Some((Instant::now(), opts.clone()));
        }
        Ok(opts)
    }

    pub(super) async fn agy_commands(&self, id: &str) -> Result<Value, LocalError> {
        let _ = self.agy_attach(id).await;
        let items = lock(&self.inst(id).st).agy.commands.clone();
        Ok(json!({ "items": items }))
    }

    pub(super) fn agy_usage(&self, id: &str) -> Value {
        let inst = self.inst(id);
        let st = lock(&inst.st);
        let totals = json!({
            "input": st.agy.input_tokens,
            "cache_read": st.agy.cache_read_tokens,
            "output": st.agy.output_tokens,
        });
        json!({
            "model": st.actual_model.clone().or(st.model.clone()),
            "context": st.context,
            "window": st.agy.window,
            "totals": totals,
            "limits": null,
            "version": st.version,
        })
    }

    pub(super) fn agy_user_image(&self, id: &str, key: &str, n: usize) -> Result<(&'static str, Vec<u8>), LocalError> {
        let session = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone()).ok_or_else(LocalError::not_found)?;
        let log = agy_log::shared(&session);
        let img = lock(&log).user_image(key, n);
        img.ok_or_else(LocalError::not_found)
    }

    pub(super) async fn agy_keep(&self, id: &str) {
        let inst = self.inst(id);
        let gen = self.agy.generation.load(Ordering::SeqCst);
        if lock(&inst.st).agy.generation == gen && gen != 0 && self.agy.app.lock().await.as_ref().is_some_and(|a| a.alive()) {
            return;
        }
        if let Err(e) = self.agy_attach(id).await {
            let mut st = lock(&inst.st);
            st.error = e.params.get("detail").and_then(Value::as_str).map(String::from).or(Some(e.code.to_string()));
            let reason = if e.code == "AGY_NOT_FOUND" { "agy_not_found" } else { "agy_unavailable" };
            Self::set_phase(&inst, &mut st, Phase::Detached, Some(reason));
        }
    }

    /// 停会话时取消当前轮并关闭 ACP worker 会话（绑定留着，下次打开通过 `session/load` 接回）
    pub(super) async fn agy_release(&self, inst: &Arc<Inst>) {
        let sid = {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.agy.generation = 0;
            st.binding.as_ref().map(|b| b.session_id.clone())
        };
        if let Some(sid) = sid {
            if let Some(app) = self.agy.app.lock().await.clone().filter(|a| a.alive()) {
                let _ = app.notify("session/cancel", json!({ "sessionId": sid })).await;
                let _ = app.call("session/close", json!({ "sessionId": sid }), Duration::from_secs(5)).await;
            }
        }
    }

    pub(super) async fn agy_forget(&self, session: Option<String>) {
        if let Some(sid) = session {
            lock(&self.agy.sessions).remove(&sid);
            agy_log::drop_log(&sid);
            if let Some(app) = self.agy.app.lock().await.clone().filter(|a| a.alive()) {
                let _ = app.call("session/close", json!({ "sessionId": sid }), Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_model_options() {
        let resp = json!({
            "models": {
                "currentModelId": "gemini-4-neon-high",
                "availableModels": [
                    { "modelId": "gemini-4-neon-low", "name": "Gemini 4 Neon (Low)" },
                    { "modelId": "gemini-4-neon-high", "name": "Gemini 4 Neon (High)" },
                    { "modelId": "gemini-4-neon-xhigh", "name": "Gemini 4 Neon (XHigh)" },
                    { "modelId": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                ]
            },
            "configOptions": [
                {
                    "id": "model",
                    "currentValue": "gemini-4-neon",
                    "options": [
                        { "value": "gemini-4-neon", "name": "Gemini 4 Neon" },
                        { "value": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)" }
                    ]
                },
                {
                    "id": "thought_level",
                    "currentValue": "high",
                    "options": [{ "value": "low" }, { "value": "high" }, { "value": "xhigh" }]
                }
            ]
        });
        let opts = model_options_from_session(&resp);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["value"], "gemini-4-neon");
        assert_eq!(opts[0]["efforts"], json!(["low", "high", "xhigh"]));
        assert_eq!(opts[0]["default"], true);
        assert_eq!(opts[1]["value"], "gpt-oss-120b-medium");
        assert_eq!(opts[1]["efforts"], json!([]));
    }

    #[test]
    fn parses_cli_models_output() {
        let raw = "\r⠋ Fetching available models...\rgemini-4-neon-high   Gemini 4 Neon (High)\ngemini-4-neon-low    Gemini 4 Neon (Low)\ngpt-oss-120b-medium  GPT-OSS 120B (Medium)\n";
        let opts = model_options_from_cli(raw);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["value"], "gemini-4-neon");
        assert_eq!(opts[0]["efforts"], json!(["low", "high"]));
        assert_eq!(opts[1]["value"], "gpt-oss-120b-medium");
        assert_eq!(opts[1]["efforts"], json!([]));
    }

    #[test]
    fn maps_permissions_and_modes() {
        let opts = json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
        ]);
        let ans = |d: &str| PermissionAnswer { id: 1, decision: d.into(), message: None, answers: None };
        assert_eq!(decision(&opts, &ans("allow")).unwrap(), json!({ "outcome": { "outcome": "selected", "optionId": "allow" } }));
        assert_eq!(decision(&opts, &ans("deny")).unwrap(), json!({ "outcome": { "outcome": "selected", "optionId": "deny" } }));
        assert_eq!(to_acp_mode("acceptEdits"), Some(("accept-edits", false)));
        assert_eq!(to_acp_mode("bypassPermissions"), Some(("auto", true)));
        assert_eq!(from_acp_mode("accept-edits", false), "acceptEdits");
        assert_eq!(from_acp_mode("auto", true), "bypassPermissions");
    }
}
