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
use std::time::{Duration, Instant};

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
/// 缓存的 configOptions 存在数据库的键（草稿页用）
const LAST_CONFIG_KEY: &str = "agy.last_config";
/// 思考强度通用档位（模型选项里没带时兜底）
pub(super) const EFFORTS: &[&str] = &["low", "medium", "high"];

#[derive(Default)]
pub struct Hub {
    /// 每个账号一个 `agy acp`（另外的账号带 `--gemini-dir`，登录各自独立）：账号 → (进程, 第几个进程)
    conns: tokio::sync::Mutex<HashMap<String, (Arc<AcpConn>, u64)>>,
    /// 起过几个 `agy acp` 进程（每个进程一个编号，从 1 开始）；会话只在加载它的那个进程里有效
    generation: AtomicU64,
    /// 会话 → 终端
    sessions: Mutex<HashMap<String, String>>,
    /// agy 更新了：没有会话在跑的时候把 `agy acp` 换成新版本（下次用到时）
    stale: std::sync::atomic::AtomicBool,
    /// 最近一次看到的 configOptions（草稿页选模型用）
    last_config: Mutex<Value>,
    /// 还没对上终端的会话发来的上下文用量：`session/new` 一返回 agy 就报 `{used, size}`，
    /// 这时会话还没记到 `sessions` 里，先放这里，[`Chat::agy_finish_attach`] 时补上
    early_usage: Mutex<HashMap<String, (Option<u64>, Option<u64>)>>,
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
    /// 这一轮是手动压缩（`/compact`）
    compacting: bool,
    /// 上一轮因为账号额度用完失败了：界面上提示换哪个账号接着（换了、或再发一轮就清掉）
    pub quota_out: Option<Value>,
    /// 额度用完时自动换过账号：换到了哪个（界面上提示一下），什么时候换的（短时间内不再自动换，免得几个账号来回换）
    pub switched: Option<Value>,
    switched_at: Option<Instant>,
    /// 防止同一个终端并发 attach / load
    attach_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Usage {
    fn is_zero(self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }
}

/// `agy_setting` / `draft_remember` 允许的值（兼容含 `/`、`(`、`)`、`+`、空格的 ACP 模型 ID，拒绝控制字符和超长串）
pub(super) fn valid_setting_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.chars().any(char::is_control)
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-:/()+ ".contains(c))
}

/// agy 本地会话记录文件路径（用于客户端重启后恢复累计用量）
fn agy_brain_transcript(session: &str) -> Option<std::path::PathBuf> {
    if !agy_log::valid_session(session) {
        return None;
    }
    Some(dirs::home_dir()?.join(".gemini/antigravity-cli/brain").join(session).join(".system_generated/logs/transcript.jsonl"))
}

/// 扫描 agy `transcript.jsonl` 里的 `PLANNER_RESPONSE` 行，汇总累计 token 用量
fn transcript_usage_from(path: &std::path::Path) -> Usage {
    use std::io::{BufRead, BufReader};
    let Ok(f) = std::fs::File::open(path) else { return Usage::default() };
    let mut u = Usage::default();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("PLANNER_RESPONSE") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["type"].as_str() != Some("PLANNER_RESPONSE") {
            continue;
        }
        u.input += v["input_tokens"].as_u64().unwrap_or(0);
        u.output += v["output_tokens"].as_u64().unwrap_or(0) + v["thought_tokens"].as_u64().unwrap_or(0);
        u.cache_read += v["cache_read_tokens"].as_u64().unwrap_or(0);
        u.cache_write += v["cache_write_tokens"].as_u64().unwrap_or(0);
    }
    u
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

fn collect_flat_values(items: &[Value], out: &mut Vec<Value>) {
    for o in items {
        if let Some(v) = o["value"].as_str() {
            out.push(json!(v));
        }
        if let Some(children) = o["options"].as_array() {
            collect_flat_values(children, out);
        }
    }
}

/// 模型菜单（和 Claude Code 的同一个格式）：`configOptions` 里的 `model`，思考强度是 `thought_level` 的选项。
/// 同时支持扁平选项和分组选项（ACP `SessionConfigSelectGroup`），以及本身代表一组模型的选项。
pub(super) fn model_options(config: &Value) -> Vec<Value> {
    let mut efforts = vec![];
    collect_flat_values(&config_options(config, "thought_level"), &mut efforts);
    let current = config_value(config, "model");

    let make_opt = |o: &Value, group_id: Option<&str>, group_name: Option<&str>, force_group: bool, fallback_models: Option<Vec<Value>>| -> Option<Value> {
        let v = o["value"].as_str()?;
        let explicit_group = force_group
            || o["isGroup"].as_bool() == Some(true)
            || o["is_group"].as_bool() == Some(true)
            || o["models"].as_array().is_some_and(|a| !a.is_empty());
        let g = group_id.or_else(|| o["group"].as_str());
        let gn = group_name.or_else(|| o["groupName"].as_str()).or_else(|| o["group_name"].as_str());
        let mut item = json!({
            "value": v,
            "label": o["name"].as_str().unwrap_or(v),
            "description": o["description"].as_str().unwrap_or(""),
            "section": "main",
            "model": v,
            "efforts": efforts,
            "default_effort": Value::Null,
            "default": current == Some(v),
            "group": g,
            "group_name": gn,
        });
        if explicit_group {
            item["is_group"] = json!(true);
        }
        if let Some(ms) = o["models"].as_array().filter(|a| !a.is_empty()) {
            item["models"] = Value::Array(ms.clone());
        } else if let Some(ms) = fallback_models.filter(|a| !a.is_empty()) {
            item["models"] = Value::Array(ms);
        }
        Some(item)
    };

    let mut out = vec![];
    for entry in config_options(config, "model") {
        if let Some(children) = entry["options"].as_array() {
            let group_id = entry["group"].as_str().or_else(|| entry["id"].as_str());
            let group_name = entry["name"].as_str().or(group_id);
            let child_values: Vec<Value> = children.iter().filter_map(|c| c["value"].as_str().map(|v| json!(v))).collect();
            // 分组自身带有可选 `value` 时，把分组也作为一个可选项（标 `is_group: true`）
            if entry["value"].as_str().is_some() {
                if let Some(item) = make_opt(&entry, group_id, group_name, true, Some(child_values)) {
                    out.push(item);
                }
            }
            for child in children {
                if let Some(item) = make_opt(child, group_id, group_name, false, None) {
                    out.push(item);
                }
            }
        } else if let Some(item) = make_opt(&entry, None, None, false, None) {
            out.push(item);
        }
    }
    out
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

/// 发给 agy 的内容：文字和图片（base64），同时返回图片原始字节供对话界面看大图
fn prompt_content_with_images(text: &str, images: &[String]) -> Result<(Value, Vec<(&'static str, Vec<u8>)>), LocalError> {
    use base64::Engine;
    let mut parts = vec![];
    let mut raw_imgs = vec![];
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
        parts.push(json!({ "type": "image", "mimeType": mime, "data": base64::engine::general_purpose::STANDARD.encode(&data) }));
        raw_imgs.push((mime, data));
    }
    Ok((Value::Array(parts), raw_imgs))
}

/// 发给 agy 的内容：文字和图片（base64）
fn prompt_content(text: &str, images: &[String]) -> Result<Value, LocalError> {
    Ok(prompt_content_with_images(text, images)?.0)
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

    /// 这个会话用的 agy 账号（agy_profiles.rs；新建时记下的，没记时跟默认）
    fn agy_profile(&self, id: &str) -> crate::agy_profiles::Profile {
        crate::agy_profiles::for_instance(&self.store, id)
    }

    /// 这个会话的账号现在起着的 `agy acp` 和它的编号（没起不去起）
    async fn agy_live(&self, id: &str) -> Option<(Arc<AcpConn>, u64)> {
        let profile = self.agy_profile(id).id;
        self.agy.conns.lock().await.get(&profile).filter(|(c, _)| c.alive()).cloned()
    }

    /// 这个会话的账号的 `agy acp` 编号（没起是 0）
    async fn agy_gen_of(&self, id: &str) -> u64 {
        self.agy_live(id).await.map_or(0, |(_, g)| g)
    }

    /// 连上这个会话的账号的 `agy acp`（已经起着就直接用），返回进程和它的编号
    async fn agy_conn(&self, id: &str) -> Result<(Arc<AcpConn>, u64), LocalError> {
        let profile = self.agy_profile(id);
        let mut conns = self.agy.conns.lock().await;
        if let Some((c, g)) = conns.get(&profile.id).filter(|(c, _)| c.alive()) {
            if !self.agy.stale.load(Ordering::SeqCst) || self.agy_busy() {
                return Ok((c.clone(), *g));
            }
            // 装了新版本、现在没有会话在跑：都换掉（各会话下次用到时重新加载）
            tracing::info!("agy 更新过了，换成新版本的 agy acp");
            for (c, _) in conns.values() {
                c.kill();
            }
            conns.clear();
        }
        self.agy.stale.store(false, Ordering::SeqCst);
        let shell = crate::shells::login_shell();
        let program = agy_acp::program(&shell).await;
        let path = super::codex_app::launch_path(&shell, &program).await;
        let env = super::codex_app::user_env(&shell).await.unwrap_or_default();
        let extra = crate::agy_profiles::acp_args(&self.paths.home, &profile);
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = AcpConn::start(&program, path.as_deref(), &env, &extra, &self.paths.home.join("run"), tx).await?;
        let gen = self.agy.generation.fetch_add(1, Ordering::SeqCst) + 1;
        conns.insert(profile.id.clone(), (conn.clone(), gen));
        drop(conns);
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(me.agy_events(conn.clone(), rx, gen));
        }
        tracing::info!(pid = ?conn.pid, profile = %profile.id, version = ?lock(&conn.version), "已起 agy acp");
        Ok((conn, gen))
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

    /// 缓存最近一次看到的带模型选项的 configOptions（草稿页用）
    fn agy_cache_config(&self, config: &Value) {
        if config_options(config, "model").is_empty() {
            return;
        }
        *lock(&self.agy.last_config) = config.clone();
        let _ = self.store.set(LAST_CONFIG_KEY, config);
    }

    /// 取缓存的 configOptions（内存优先，客户端重启后回退数据库）
    pub(super) fn agy_cached_config(&self) -> Value {
        let cached = lock(&self.agy.last_config).clone();
        if cached.is_array() {
            return cached;
        }
        let stored = self.store.get::<Value>(LAST_CONFIG_KEY).ok().flatten().unwrap_or(Value::Null);
        if stored.is_array() {
            *lock(&self.agy.last_config) = stored.clone();
        }
        stored
    }

    /// 打开快照时的 attach：已有绑定且已加载/正在加载时不阻塞；首次加载已有绑定放到后台避免卡住页面
    pub(super) async fn agy_snapshot_attach(self: &Arc<Self>, id: &str) {
        let inst = self.inst(id);
        let gen = self.agy_gen_of(id).await;
        let (has_binding, ready, attaching) = {
            let st = lock(&inst.st);
            let b = st.binding.as_ref();
            let ready = b.is_some_and(|b| st.agy.generation == gen && gen != 0 && lock(&agy_log::shared(&b.session_id)).loaded);
            let attaching = st.agy.attach_lock.try_lock().is_err();
            (b.is_some(), ready, attaching)
        };
        if ready || attaching {
            return;
        }
        if has_binding {
            let (me, id, inst) = (self.clone(), id.to_string(), inst.clone());
            tokio::spawn(async move {
                match me.agy_attach(&id).await {
                    Ok(_) => {
                        let _ = inst.tx.send(super::Event::Reset);
                    }
                    Err(e) => Self::agy_attach_failed(&inst, &e),
                }
            });
        } else if let Err(e) = self.agy_attach(id).await {
            Self::agy_attach_failed(&inst, &e);
        }
    }

    /// 完成会话绑定后的公共收尾：设置版本、configOptions、模式，新建会话恢复记住的模型和思考强度
    async fn agy_finish_attach(&self, conn: &AcpConn, inst: &Arc<Inst>, session: &str, gen: u64, r: &Value, is_new: bool) {
        lock(&self.agy.sessions).insert(session.to_string(), inst.id.clone());
        let early = lock(&self.agy.early_usage).remove(session);
        agy_log::shared(session).lock().unwrap_or_else(|e| e.into_inner()).loaded = true;
        // 套餐只有这里报（本地文件里没有）：记到账号上，账号页和用量面板显示
        if let Some(tier) = r["_meta"]["antigravity"]["tier"]["name"].as_str() {
            crate::agy_profiles::remember_plan(&self.store, &self.agy_profile(&inst.id).id, tier);
        }
        let conn_ver = lock(&conn.version).clone();
        let config_opt = r.get("configOptions").filter(|c| c.is_array()).cloned();
        if let Some(ref c) = config_opt {
            self.agy_cache_config(c);
        }
        {
            let mut st = lock(&inst.st);
            st.agy.generation = gen;
            if let Some(v) = conn_ver {
                st.version = Some(v);
            }
            if let Some(c) = config_opt {
                Self::agy_config(&mut st, c);
            }
            if let Some((used, size)) = early {
                st.agy.window = size.or(st.agy.window);
                st.context = used.or(st.context);
            }
            if st.phase == Phase::Detached {
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
            st.error = None;
            Self::emit_status(inst, &st);
        }
        // 权限模式：看看记着的（客户端重启后接着用）；没记过时“全部权限”打开的终端用全部权限
        let mode = self.store.get::<String>(&mode_key(&inst.id)).ok().flatten().or_else(|| self.instances.opts(&inst.id).full_access.then(|| "bypassPermissions".to_string()));
        if let Some(m) = mode {
            if let Err(e) = self.agy_set_mode(conn, inst, session, &m).await {
                tracing::debug!(id = %inst.id, error = ?e, "agy 的权限模式没换上");
            }
        }
        // 新建会话时恢复记住的模型和思考强度（没有显式覆盖过时）
        if is_new {
            self.agy_restore_memory(conn, inst, session).await;
        }
    }

    async fn agy_restore_memory(&self, conn: &AcpConn, inst: &Arc<Inst>, session: &str) {
        let (overridden, config, cur_model, cur_effort) = {
            let st = lock(&inst.st);
            (st.model_set_at.is_some(), st.agy.config.clone(), st.model.clone(), st.effort.clone())
        };
        if overridden {
            return;
        }
        let Some((agent, profile, base)) = self.memory_scope_of(&inst.id) else { return };
        let Some(last) = super::model_memory::get(&self.store, agent, &profile, &base) else { return };
        let options = model_options(&config);
        let Some(usable) = super::model_memory::usable(&last, &options, EFFORTS) else { return };
        if let Some(m) = usable["model"].as_str().filter(|m| cur_model.as_deref() != Some(*m)) {
            match conn.call("session/set_config_option", json!({ "sessionId": session, "configId": "model", "value": m })).await {
                Ok(r) => {
                    if r["configOptions"].is_array() {
                        self.agy_cache_config(&r["configOptions"]);
                        let mut st = lock(&inst.st);
                        Self::agy_config(&mut st, r["configOptions"].clone());
                        Self::emit_status(inst, &st);
                    }
                }
                Err(e) => tracing::debug!(id = %inst.id, model = m, error = ?e, "agy 恢复记住的模型失败"),
            }
        }
        if let Some(e) = usable["effort"].as_str().filter(|e| cur_effort.as_deref() != Some(*e)) {
            match conn.call("session/set_config_option", json!({ "sessionId": session, "configId": "thought_level", "value": e })).await {
                Ok(r) => {
                    if r["configOptions"].is_array() {
                        self.agy_cache_config(&r["configOptions"]);
                        let mut st = lock(&inst.st);
                        Self::agy_config(&mut st, r["configOptions"].clone());
                        Self::emit_status(inst, &st);
                    }
                }
                Err(e2) => tracing::debug!(id = %inst.id, effort = e, error = ?e2, "agy 恢复记住的思考强度失败"),
            }
        }
    }

    /// 这个终端的会话在当前的 agy 进程里加载好了（没有会话就新建一个）
    pub(super) async fn agy_attach(&self, id: &str) -> Result<Arc<AcpConn>, LocalError> {
        let inst = self.inst(id);
        let attach_lock = lock(&inst.st).agy.attach_lock.clone();
        let _guard = attach_lock.lock().await;
        let (conn, gen) = self.agy_conn(id).await?;
        let (session, loaded) = {
            let st = lock(&inst.st);
            (st.binding.as_ref().map(|b| b.session_id.clone()), st.agy.generation == gen)
        };
        if loaded && session.is_some() {
            return Ok(conn);
        }
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let loaded_res = match session {
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
        let (r, is_new) = match loaded_res {
            Some(r) => (r, false),
            None => {
                let r = conn.call("session/new", json!({ "cwd": row.workdir, "mcpServers": [] })).await?;
                let sid = r["sessionId"].as_str().filter(|s| agy_log::valid_session(s)).ok_or_else(|| LocalError::new("AGY_REQUEST_FAILED").with("detail", "no sessionId"))?.to_string();
                self.agy_bind(&inst, &sid);
                (r, true)
            }
        };
        let session = Self::agy_session(&inst).ok_or_else(LocalError::not_found)?;
        // 客户端重启加载旧会话时，如果累计用量是 0，从本地 transcript.jsonl 回填
        if !is_new && lock(&inst.st).agy.usage.is_zero() {
            let sid = session.clone();
            let recovered = tokio::task::block_in_place(|| agy_brain_transcript(&sid).map(|p| transcript_usage_from(&p))).unwrap_or_default();
            if !recovered.is_zero() {
                let mut st = lock(&inst.st);
                if st.agy.usage.is_zero() {
                    st.agy.usage = recovered;
                }
            }
        }
        self.agy_finish_attach(&conn, &inst, &session, gen, &r, is_new).await;
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
                let attach_lock = st.agy.attach_lock.clone();
                st.agy = AgySt { queued, attach_lock, ..Default::default() };
                st.title = None;
                st.context = None;
                st.error = None;
                st.model_set_at = None;
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
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        let Some(inst) = self.agy_inst_of(session) else {
            if kind == "usage_update" && agy_log::valid_session(session) {
                let mut early = lock(&self.agy.early_usage);
                if early.len() > 64 {
                    early.clear();
                }
                early.insert(session.to_string(), (u["used"].as_u64(), u["size"].as_u64()));
            }
            return;
        };
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
                    let cfg = u["configOptions"].clone();
                    Self::agy_config(&mut st, cfg.clone());
                    Self::emit_status(&inst, &st);
                    drop(st);
                    self.agy_cache_config(&cfg);
                    return;
                }
            }
            "available_commands_update" => {
                st.agy.commands = u["availableCommands"].as_array().cloned().unwrap_or_default();
            }
            "tool_call" | "tool_call_update" if st.phase == Phase::Running => {
                let done = matches!(u["status"].as_str(), Some("completed" | "failed"));
                let next = if done {
                    Some(None)
                } else {
                    u["name"].as_str().filter(|s| !s.is_empty()).map(|n| Some(agy_log::tool_view(n, &Value::Null).0))
                };
                if let Some(name) = next {
                    if st.tool != name {
                        st.tool = name;
                        Self::emit_status(&inst, &st);
                    }
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
            let (conn, gen) = self.agy_conn(id).await?;
            match resume_to {
                Some(s) if agy_log::valid_session(s) => {
                    self.agy_bind(&inst, s);
                    lock(&inst.st).agy.generation = 0;
                    self.agy_attach(id).await?;
                }
                Some(_) => return Err(LocalError::invalid("resume", "invalid")),
                None => {
                    let attach_lock = lock(&inst.st).agy.attach_lock.clone();
                    let _guard = attach_lock.lock().await;
                    let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
                    let r = conn.call("session/new", json!({ "cwd": row.workdir, "mcpServers": [] })).await?;
                    let sid = r["sessionId"].as_str().filter(|s| agy_log::valid_session(s)).ok_or_else(|| LocalError::new("AGY_REQUEST_FAILED").with("detail", "no sessionId"))?.to_string();
                    self.agy_bind(&inst, &sid);
                    // 新建空会话直接应用 configOptions、标记已加载并恢复模式/模型，不再多余地 session/load
                    self.agy_finish_attach(&conn, &inst, &sid, gen, &r, true).await;
                }
            }
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
        let (content, imgs) = tokio::task::block_in_place(|| prompt_content_with_images(&text, &images))?;
        let log = agy_log::shared(&session);
        {
            let mut l = lock(&log);
            l.turn_start();
            l.user_with_images(&text, imgs.len() as u32, imgs);
        }
        {
            let mut st = lock(&inst.st);
            st.error = None;
            st.agy.quota_out = None;
            // 自动换账号后发的那句 continue 之外，用户再发一轮就不用再提示“已换账号”了
            if text != "continue" {
                st.agy.switched = None;
            }
            st.agy.turn_at = Some(Instant::now());
            st.agy.compacting = text.trim() == "/compact";
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
        // 模型设置里的自动压缩点：agy acp 不认 AGY_CLI_AUTO_COMPACT_OVERRIDE（1.5.17 实测只有终端里的 agy 认），由看看到点替用户发 /compact
        let tuning = self.agy_profile(&inst.id).tuning;
        let mut quota_hit = false;
        let (next, compact_now) = {
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
                    // 手动压缩做完：agy 不再报压缩后的上下文（下一轮才报），先当不知道，别还显示压缩前的
                    if st.agy.compacting && stop == Some("end_turn") {
                        lock(&log).compacted();
                        st.context = None;
                    }
                }
                Err(e) => {
                    let why = e.params.get("detail").and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.code.to_string());
                    st.error = Some(why.chars().take(500).collect());
                    lock(&log).turn_end(None, Some(&why), ms);
                    quota_hit = crate::agy_profiles::quota_error(&why);
                }
            }
            let was_compact = std::mem::take(&mut st.agy.compacting);
            Self::drop_pending(&mut st);
            st.last_reply = lock(&log).last_reply();
            let cancelled = matches!(&r, Ok(v) if v["stopReason"].as_str() == Some("cancelled"));
            let ok = conn.alive() && !cancelled && r.is_ok();
            // 到了压缩点：先压缩，排着的等压缩完再发
            let compact_now = ok && !was_compact && tuning.compact_at(st.agy.window).zip(st.context).is_some_and(|(at, used)| used >= at);
            // 进程没了或被停止：排着的留着（可在界面上取回或编辑），正常完成才接着发下一条
            let next = if !compact_now && conn.alive() && !cancelled && !st.agy.queued.is_empty() { Some(st.agy.queued.remove(0)) } else { None };
            if next.is_none() && !compact_now {
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
            Self::emit_status(inst, &st);
            (next, compact_now)
        };
        if quota_hit {
            self.agy_quota_out(inst);
            return;
        }
        if compact_now {
            tracing::info!(id = %inst.id, "agy 上下文到了模型设置里的压缩点，自动压缩");
            if let Err(e) = self.agy_start_turn(conn.clone(), inst.clone(), "/compact".into(), vec![]) {
                let mut st = lock(&inst.st);
                st.error = Some(e.code.to_string());
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
            return;
        }
        if let Some((text, images)) = next {
            self.checkpoint_turn(&inst.id, &text, false).await;
            if let Err(e) = self.agy_start_turn(conn.clone(), inst.clone(), text, images) {
                let mut st = lock(&inst.st);
                st.error = Some(e.code.to_string());
                Self::set_phase(inst, &mut st, Phase::Idle, None);
            }
        }
    }

    /// 这一轮因为账号额度用完失败：挑一个额度够的账号（同一组模型、两个窗口都 ≥10%，剩得最多的）提示“换到它接着”；
    /// 开了自动换账号就直接换过去发 continue（同一段对话，见 agy_switch_profile）。10 分钟内自动换过就不再自动换，只提示
    fn agy_quota_out(&self, inst: &Arc<Inst>) {
        use crate::agy_profiles as ap;
        let from = self.agy_profile(&inst.id);
        // 用完的这个也重新查一下（用量面板上看得到），别的账号过时了也查
        ap::refresh_all_soon(&self.store, &self.paths.home);
        let gemini = ap::gemini_model(lock(&inst.st).model.as_deref());
        let to = ap::best(&self.store, &self.paths.home, gemini, Some(&from.id)).and_then(|(id, left)| ap::get(&self.store, &id).map(|p| (p, left)));
        let auto = {
            let mut st = lock(&inst.st);
            let recent = st.agy.switched_at.is_some_and(|t| t.elapsed() < Duration::from_secs(600));
            let auto = to.is_some() && ap::auto_switch(&self.store) && !recent && self.instances.container(&inst.id).is_none();
            st.agy.quota_out = Some(json!({
                "from": { "id": from.id, "name": from.name },
                "to": to.as_ref().map(|(p, left)| json!({ "id": p.id, "name": p.name, "email": p.email, "left": left })),
                "auto": auto,
            }));
            if auto {
                st.agy.switched_at = Some(Instant::now());
            }
            Self::emit_status(inst, &st);
            auto
        };
        let (Some((to, _)), true) = (to, auto) else { return };
        let Some(me) = self.me.upgrade() else { return };
        let inst = inst.clone();
        tracing::info!(id = %inst.id, from = %from.id, to = %to.id, "agy 账号额度用完，自动换账号接着");
        tokio::spawn(async move {
            match me.agy_switch_profile(&inst.id, &to.id, true).await {
                Ok(()) => {
                    let mut st = lock(&inst.st);
                    st.agy.switched = Some(json!({ "from": { "id": from.id, "name": from.name }, "to": { "id": to.id, "name": to.name, "email": to.email } }));
                    Self::emit_status(&inst, &st);
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "agy 自动换账号没成");
                    let mut st = lock(&inst.st);
                    st.error = Some(e.code.to_string());
                    Self::emit_status(&inst, &st);
                }
            }
        });
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

    /// 停止：取消这一轮（agy 那边结束后 `session/prompt` 返回 `cancelled`），排着的消息留着供取回
    pub(super) async fn agy_interrupt(&self, id: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        let session = {
            let mut st = lock(&inst.st);
            if !matches!(st.phase, Phase::Running | Phase::Approval) {
                return Ok(());
            }
            Self::drop_pending(&mut st);
            st.binding.as_ref().map(|b| b.session_id.clone())
        };
        let Some(session) = session else { return Ok(()) };
        let Some((conn, _)) = self.agy_live(id).await else { return Ok(()) };
        conn.notify("session/cancel", json!({ "sessionId": session }));
        Self::stop_tool(&inst, &mut lock(&inst.st));
        Ok(())
    }

    /// 取回排在这一轮之后的第 `n` 条（界面放回输入框）
    pub fn agy_later_take(&self, id: &str, n: usize) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let mut st = lock(&inst.st);
        if n >= st.agy.queued.len() {
            return Err(LocalError::not_found());
        }
        let (text, images) = st.agy.queued.remove(n);
        Self::emit_status(&inst, &st);
        Ok(json!({ "text": text, "images": images }))
    }

    /// 强制重启：标记 stale，重新加载当前会话
    pub(super) async fn agy_restart(&self, id: &str) -> Result<(), LocalError> {
        let _ = self.agy_interrupt(id).await;
        self.agy.stale.store(true, Ordering::SeqCst);
        let inst = self.inst(id);
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.agy.generation = 0;
            st.error = None;
            if matches!(st.phase, Phase::Running | Phase::Approval) {
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
            }
            if st.binding.is_none() {
                return Ok(());
            }
        }
        self.agy_attach(id).await.map(|_| ())
    }

    /// 换账号接着同一段对话：记下新账号，在那个账号的 `agy acp` 里重新加载（对话记录各账号共用，见 agy_profiles.rs 的 SHARED）。
    /// `go_on`：起来后发 continue
    pub(super) async fn agy_switch_profile(&self, id: &str, profile: &str, go_on: bool) -> Result<(), LocalError> {
        crate::agy_profiles::get(&self.store, profile).ok_or_else(|| LocalError::invalid("agy_profile", "not_found"))?;
        if self.instances.container(id).is_some() {
            return Err(LocalError::new("CHAT_UNAVAILABLE"));
        }
        let _ = self.agy_interrupt(id).await;
        let old = self.agy_live(id).await;
        crate::agy_profiles::pin(&self.store, id, Some(profile))?;
        let inst = self.inst(id);
        // 旧账号进程里的这段对话关掉（换回同一个账号时不关）
        if let (Some((old, _)), Some(s)) = (old, Self::agy_session(&inst)) {
            if self.agy_live(id).await.is_none_or(|(c, _)| !Arc::ptr_eq(&c, &old)) {
                if let Err(e) = old.call_timeout("session/close", json!({ "sessionId": s }), Duration::from_secs(5)).await {
                    tracing::debug!(error = ?e, "换账号时旧进程里的 agy 会话没关上");
                }
            }
        }
        let has_binding = {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.agy.generation = 0;
            st.error = None;
            st.agy.quota_out = None;
            st.agy.switched = None;
            if matches!(st.phase, Phase::Running | Phase::Approval) {
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
            }
            st.binding.is_some()
        };
        if !has_binding {
            return Ok(());
        }
        self.agy_attach(id).await?;
        let _ = inst.tx.send(super::Event::Reset);
        if go_on {
            self.agy_input(id, "continue", &[]).await?;
        }
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
        if !valid_setting_value(value) {
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
                let cfg = r["configOptions"].is_array().then(|| r["configOptions"].clone());
                if let Some(ref c) = cfg {
                    self.agy_cache_config(c);
                }
                let mut st = lock(&inst.st);
                if let Some(c) = cfg {
                    Self::agy_config(&mut st, c);
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

    /// 用量栏：累计 token、上下文占用、运行版本与已安装版本
    pub(super) fn agy_usage(&self, id: &str) -> Value {
        let inst = self.inst(id);
        let st = lock(&inst.st);
        let u = st.agy.usage;
        let totals = json!({ "input": u.input + u.cache_write, "cache_read": u.cache_read, "output": u.output });
        // 额度：这个会话用的账号上次查到的（`agy -p /usage`），过时了后台再查
        let profile = self.agy_profile(id);
        crate::agy_profiles::refresh_quota_soon(&self.paths.home, &profile.id);
        let limits = crate::agy_profiles::cached_quota(&profile.id);
        let email = profile.email.clone().or_else(|| crate::agy_profiles::local_email(&self.paths.home, &profile.id));
        let account = json!({ "id": profile.id, "name": profile.name, "email": email, "plan": profile.plan });
        // 自动压缩点：模型设置里设了才知道（没设是 agy 自己的默认，它不报）
        let compact = profile.tuning.compact_at(st.agy.window).map(|at| json!({ "enabled": true, "at": at, "source": "agy" }));
        json!({
            "model": st.model,
            "context": st.context,
            "window": st.agy.window,
            "compact": compact,
            "totals": totals,
            "limits": limits,
            "account": account,
            "version": st.version,
            "installed": crate::agent_update::installed_version("agy"),
        })
    }

    /// `/status`：这段 agy 对话的会话信息、配置和用量
    pub(super) async fn agy_status(&self, id: &str) -> Result<Value, LocalError> {
        let _ = self.agy_attach(id).await;
        let inst = self.inst(id);
        let (session, model, effort, mode, usage, context, window, version, phase) = {
            let st = lock(&inst.st);
            (
                st.binding.as_ref().map(|b| b.session_id.clone()),
                st.model.clone(),
                st.effort.clone(),
                st.mode.clone(),
                st.agy.usage,
                st.context,
                st.agy.window,
                st.version.clone(),
                st.phase,
            )
        };
        let dir = self.store.instance(id).ok().flatten().map(|r| r.workdir);
        let installed = crate::agent_update::installed_version("agy");
        let row = |label: &str, value: Option<String>| value.filter(|v| !v.is_empty()).map(|v| json!({ "label": label, "value": v }));
        let session_rows: Vec<Value> = [
            row("Version", version.or(installed)),
            row("Session", session),
            row("Directory", dir),
            row("Status", Some(format!("{phase:?}").to_ascii_lowercase())),
        ]
        .into_iter()
        .flatten()
        .collect();
        let config_rows: Vec<Value> = [row("Model", model), row("Thought level", effort), row("Permissions", mode)].into_iter().flatten().collect();
        let total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        let usage_rows: Vec<Value> = [
            row("Context used", context.map(|c| c.to_string())),
            row("Context window", window.map(|w| w.to_string())),
            row("Input tokens", (usage.input + usage.cache_write > 0).then(|| (usage.input + usage.cache_write).to_string())),
            row("Cache read", (usage.cache_read > 0).then(|| usage.cache_read.to_string())),
            row("Output tokens", (usage.output > 0).then(|| usage.output.to_string())),
            row("Total tokens", (total_tokens > 0).then(|| total_tokens.to_string())),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(json!({ "sections": [
            { "title": "Session", "rows": session_rows },
            { "title": "Config", "rows": config_rows },
            { "title": "Usage", "rows": usage_rows },
        ] }))
    }

    /// 资源看板：各账号的 `agy acp` 进程号和一共加载着几段对话
    pub async fn agy_usage_roots(&self) -> (Vec<u32>, usize) {
        let pids = self.agy.conns.lock().await.values().filter(|(c, _)| c.alive()).filter_map(|(c, _)| c.pid).collect();
        (pids, lock(&self.agy.sessions).len())
    }

    /// 有人看着的时候：agy 进程没了（崩了）就重新起、重新加载
    pub(super) async fn agy_keep(&self, id: &str) {
        let inst = self.inst(id);
        let gen = self.agy_gen_of(id).await;
        if lock(&inst.st).agy.generation == gen && gen != 0 {
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
        // 终端已经删了、不知道是哪个账号的进程：每个起着的都关一下（不是它的会回错，不管）
        let conns: Vec<Arc<AcpConn>> = self.agy.conns.lock().await.values().filter(|(c, _)| c.alive()).map(|(c, _)| c.clone()).collect();
        for c in conns {
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
        chat.agy_live(&id).await.unwrap().0.kill();
        tokio::time::sleep(Duration::from_secs(1)).await;
        chat.relaunch(&id).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let s = chat.snapshot(&id).await.unwrap();
        let text = s["items"].to_string();
        assert!(text.contains("PONG") && text.contains("DONE"), "{text}");
        println!("条目 {before} → 重新加载后 {}", s["total"]);
        let session = Chat::agy_session(&chat.inst(&id)).unwrap();
        let conn = chat.agy_live(&id).await.unwrap().0;
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
        let (c, imgs) = prompt_content_with_images("look", &[png.display().to_string()]).unwrap();
        assert_eq!(c[0], json!({ "type": "text", "text": "look" }));
        assert_eq!((c[1]["type"].as_str(), c[1]["mimeType"].as_str()), (Some("image"), Some("image/png")));
        assert_eq!(imgs, vec![("image/png", b"\x89PNG".to_vec())]);
        assert!(prompt_content("x", &[dir.path().join("a.bmp").display().to_string()]).is_err());
        assert_eq!(prompt_content("", &[]).unwrap(), json!([{ "type": "text", "text": "" }]));
    }

    #[test]
    fn grouped_model_options_and_validation() {
        let config = json!([
            {
                "id": "model",
                "currentValue": "auto-group",
                "options": [
                    {
                        "value": "auto-group",
                        "name": "Auto (Best)",
                        "description": "Picks best model",
                        "isGroup": true,
                        "models": ["gemini-4-neon", "claude-opus-5-5"]
                    },
                    {
                        "group": "google",
                        "name": "Google Gemini",
                        "value": "google/auto",
                        "description": "Any Gemini model",
                        "options": [
                            { "value": "gemini-4-neon", "name": "Gemini 4 Neon", "description": "Fast" },
                            { "value": "gemini-4-pro (preview)", "name": "Gemini 4 Pro" }
                        ]
                    },
                    {
                        "group": "anthropic",
                        "name": "Anthropic Claude",
                        "options": [
                            { "value": "claude-opus-5-5", "name": "Claude Opus 5.5" }
                        ]
                    }
                ]
            },
            {
                "id": "thought_level",
                "currentValue": "high",
                "options": [
                    { "group": "std", "name": "Standard", "options": [{ "value": "low" }, { "value": "high" }] }
                ]
            }
        ]);
        let opts = model_options(&config);
        assert_eq!(opts.len(), 5);
        // 单独的 group 选项
        assert_eq!(opts[0]["value"], "auto-group");
        assert_eq!(opts[0]["is_group"], true);
        assert_eq!(opts[0]["default"], true);
        assert_eq!(opts[0]["models"], json!(["gemini-4-neon", "claude-opus-5-5"]));
        assert_eq!(opts[0]["efforts"], json!(["low", "high"]));
        // 带可选 value 的分组自身
        assert_eq!(opts[1]["value"], "google/auto");
        assert_eq!(opts[1]["is_group"], true);
        assert_eq!(opts[1]["group"], "google");
        assert_eq!(opts[1]["group_name"], "Google Gemini");
        assert_eq!(opts[1]["models"], json!(["gemini-4-neon", "gemini-4-pro (preview)"]));
        // 分组下的子选项
        assert_eq!(opts[2]["value"], "gemini-4-neon");
        assert_eq!(opts[2]["group"], "google");
        assert_eq!(opts[2]["group_name"], "Google Gemini");
        assert!(opts[2].get("is_group").is_none());
        assert_eq!(opts[3]["value"], "gemini-4-pro (preview)");
        // 不带 value 的纯分组只展开子选项
        assert_eq!(opts[4]["value"], "claude-opus-5-5");
        assert_eq!(opts[4]["group"], "anthropic");
        assert_eq!(opts[4]["group_name"], "Anthropic Claude");

        // 放宽后的值校验
        assert!(valid_setting_value("google/gemini-4 (preview)+v2"));
        assert!(valid_setting_value("claude-opus-5-5"));
        assert!(!valid_setting_value(""));
        assert!(!valid_setting_value("bad\nmodel"));
        assert!(!valid_setting_value(&"a".repeat(129)));
    }

    #[test]
    fn recovers_usage_from_brain_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("transcript.jsonl");
        let lines = [
            json!({ "type": "USER_MESSAGE", "text": "hi" }).to_string(),
            json!({ "type": "PLANNER_RESPONSE", "input_tokens": 1200, "cache_read_tokens": 300, "output_tokens": 450 }).to_string(),
            "not json".to_string(),
            json!({ "type": "PLANNER_RESPONSE", "input_tokens": 800, "cache_read_tokens": 500, "output_tokens": 150, "thought_tokens": 50 }).to_string(),
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();
        let u = transcript_usage_from(&file);
        assert_eq!(u, Usage { input: 2000, output: 650, cache_read: 800, cache_write: 0 });
        assert!(transcript_usage_from(&dir.path().join("missing.jsonl")).is_zero());
    }

    #[tokio::test]
    async fn agy_later_take_and_usage_version_and_cache() {
        use crate::instances::Instances;
        use crate::paths::Paths;
        use crate::store::Store;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let paths = Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(Store::memory().unwrap());
        let instances = Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input { name: Some("agy-u".into()), workdir: Some(work.path().display().to_string()), launch: Some("agy".into()), ..Default::default() };
        let id = instances.create(input).await.unwrap().row.id;
        let chat = Chat::new(store.clone(), paths.clone(), instances.clone());

        // 排队取回 & status_of 暴露在 later
        {
            let inst = chat.inst(&id);
            let mut st = lock(&inst.st);
            st.version = Some("1.9.0".into());
            st.agy.usage = Usage { input: 100, output: 50, cache_read: 20, cache_write: 10 };
            st.agy.queued.push(("first".into(), vec!["/tmp/a.png".into()]));
            st.agy.queued.push(("second".into(), vec![]));
            let status = Chat::status_of(&inst, &st);
            assert_eq!(status["caps"]["live_settings"], true);
            assert_eq!(status["caps"]["user_images"], true);
            assert_eq!(status["later"], json!([{ "text": "first", "images": 1 }, { "text": "second", "images": 0 }]));
        }
        let taken = chat.later_take(&id, 0).unwrap();
        assert_eq!(taken, json!({ "text": "first", "images": ["/tmp/a.png"] }));
        assert!(chat.later_take(&id, 5).is_err());

        // usage 返回 version 和 installed 字段
        let u = chat.agy_usage(&id);
        assert_eq!(u["version"], "1.9.0");
        assert!(u.get("installed").is_some());
        assert_eq!(u["totals"], json!({ "input": 110, "cache_read": 20, "output": 50 }));

        // 缓存 configOptions 供 draft_models / draft_remember 使用
        let cfg = json!([{ "id": "model", "currentValue": "gemini-4-neon", "options": [{ "value": "gemini-4-neon", "name": "Gemini 4" }, { "value": "claude-opus-5-5", "name": "Opus" }] }]);
        chat.agy_cache_config(&cfg);
        assert_eq!(chat.agy_cached_config(), cfg);
        chat.draft_remember("agy", "", Some("claude-opus-5-5"), Some("high")).unwrap();
        let (agent, profile, base) = chat.memory_scope_of(&id).unwrap();
        assert_eq!((agent, profile.as_str()), ("agy", crate::agy_profiles::OFFICIAL));
        let mem = super::super::model_memory::get(&store, agent, &profile, &base).unwrap();
        assert_eq!((mem.model.as_deref(), mem.effort.as_deref()), (Some("claude-opus-5-5"), Some("high")));
    }
}
