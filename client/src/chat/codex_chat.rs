//! 对话模式的 Codex 走 `codex app-server`（docs/CODEX_UI.md）：不再在终端里跑 TUI、不用 hooks。
//!
//! - 每个对话模式的 Codex 终端对应 app-server 里的一个 thread（会话号记在 [`Binding`] 里，和 Claude Code 一样）；
//!   终端标签里是同一个工作目录下的普通 shell。
//! - 对话内容照旧读 rollout（`codex_rollout.rs`）：app-server 写的和 TUI 写的是同一种文件，以前的对话也能接着看。
//! - 状态、正在输出的回复、审批、提问、上下文和 token 用量来自 app-server 的通知和请求。
//! - 发消息是 `turn/start`（这一轮还在跑时是 `turn/steer`）；停止是 `turn/interrupt`；模型和思考强度随下一轮带上。
//! - 终端标签和对话没有关系（不同步、不交接）：里面跑着什么（包括用户自己开的 Codex）对话都不管。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::codex_app::{self, AppServer, Incoming};
use super::{lock, Binding, Chat, Inst, Pending, Phase, PermissionAnswer};
use crate::error::LocalError;

/// 正在输出的回复最多推这么多字（只留最后这段）
const PREVIEW_LIMIT: usize = 4000;
/// 正在输出时最多每隔这么久推一次状态
const PREVIEW_GAP: Duration = Duration::from_millis(120);
/// 模型列表、账号用量的缓存
/// 登录方式缓存多久（换账号、登录登出都会走 codex_reload，这里只是少等几秒）
const ACCOUNT_TTL: Duration = Duration::from_secs(120);
const MODELS_TTL: Duration = Duration::from_secs(300);
const LIMITS_TTL: Duration = Duration::from_secs(30);
/// 以前的 Codex 会话等它空闲转到对话模式：多久看一次、最多等多久
/// 自动起名：一轮结束后等多久再看有没有名字（别处接着这段对话的 TUI 也会起，先让它）、生成最多等多久
const TITLE_DELAY: Duration = Duration::from_secs(3);
const TITLE_WAIT: Duration = Duration::from_secs(30);
/// 和 Codex TUI 起的名字一样长
const TITLE_MAX: usize = 36;
/// 空闲、没人看、终端里没有接着的 TUI、没有后台命令超过这么久的对话：退订，app-server 随后卸载它
/// （连同它起的 MCP 等进程），用到时再 `thread/resume`。和 Claude Code 的 `IDLE_REAP` 一样长
const IDLE_UNLOAD: Duration = Duration::from_secs(30 * 60);
/// 资源看板问后台命令时每个对话最多等多久
const BG_LIST_WAIT: Duration = Duration::from_secs(2);

/// 这个终端的对话已经有记录文件（聊过至少一轮）：TUI 能接、app-server 能 resume
fn has_rollout(st: &super::State) -> bool {
    st.binding.as_ref().is_some_and(|b| b.transcript_path.is_file())
}
/// 界面上能选的思考强度（app-server 的 `ReasoningEffort`）
pub const EFFORTS: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

#[derive(Default)]
pub struct Hub {
    pub(super) server: tokio::sync::Mutex<Option<Arc<AppServer>>>,
    /// 第几个 app-server 进程（0 表示还没起过）；thread 只在加载它的那个进程里有效
    generation: AtomicU64,
    /// thread id → 终端 id
    threads: Mutex<HashMap<String, String>>,
    /// 文件改动的审批里要显示改了什么：`item/started` 的 fileChange 先到，按 item id 记下
    patches: Mutex<HashMap<String, Value>>,
    models: Mutex<Option<(Instant, Value)>>,
    /// 用第三方接口的会话里点了登录（ChatGPT 设备码）：终端 id → 开始时 `account/read` 的账号。账号变了（登录好了）就把这个
    /// 会话换到官方账号（见 [`Chat::codex_login_switch`]），不然登录了模型菜单还是第三方服务商的
    login_switch: Mutex<HashMap<String, Value>>,
    /// 上次 `account/read` 的结果（开快速模式时查登录方式用，见 [`ACCOUNT_TTL`]）：每次点都现查要等好几秒
    account: Mutex<Option<(Instant, Value)>>,
    limits: Mutex<Option<(Instant, Value)>>,
    /// 每个终端最近一次算出的“产出的文件”（取文件时核对用）
    artifacts: Mutex<HashMap<String, (Instant, Vec<PathBuf>)>>,
    /// 正在等着转到对话模式的以前的 Claude Code 终端（见 `Chat::claude_adopt_later`），同一个终端同时只有一个
    pub(super) adopting: Mutex<HashSet<String>>,
    /// 已经确认在 Codex 配置里有信任记录的工作目录（见 [`Chat::codex_trust`]）
    trusted: Mutex<HashSet<String>>,
    /// 新版本检查的结果（见 [`Chat::codex_update_info`]）
    update: Mutex<Option<(Instant, Value)>>,
    /// 更新过 Codex：app-server 还是旧的，下次要用时（没有在跑的对话）换成新的
    pub(super) stale: std::sync::atomic::AtomicBool,
    /// 自动起名（见 [`Chat::codex_auto_title`]）：起名用的临时 thread → 收到的回答、答完时通知谁
    titling: Mutex<HashMap<String, (String, Option<oneshot::Sender<String>>)>>,
    /// 开着的旁聊（见 [`Chat::codex_side`]）：终端 id → (分叉自哪个对话, 临时对话, 在哪个 app-server 里)
    sides: Mutex<HashMap<String, (String, String, std::sync::Weak<AppServer>)>>,
    /// 这些对话起过名或正在起（没拿到名字的会拿掉，下一轮再试）；还有每个对话等着起名用的那句话
    titled: Mutex<HashSet<String>>,
    title_prompts: Mutex<HashMap<String, String>>,
    /// `skills/list` 的结果（工作目录 → 什么时候取的、技能列表），输入 `$` 时提示、发送时把 `$名字` 换成技能
    skills: Mutex<HashMap<String, (Instant, Vec<Skill>)>>,
}

/// Codex 的一个技能（app-server `skills/list` 里启用了的）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub path: String,
    pub description: String,
    /// user | repo | system | admin
    pub scope: String,
}

const SKILLS_TTL: Duration = Duration::from_secs(20);

fn parse_skills(r: &Value) -> Vec<Skill> {
    let mut out: Vec<Skill> = vec![];
    for e in r["data"].as_array().into_iter().flatten() {
        for s in e["skills"].as_array().into_iter().flatten() {
            let (Some(name), Some(path)) = (s["name"].as_str(), s["path"].as_str()) else { continue };
            if s["enabled"] == false || out.iter().any(|x| x.name == name) {
                continue;
            }
            let short = s["interface"]["shortDescription"].as_str().or(s["shortDescription"].as_str());
            let description = short.or(s["description"].as_str()).unwrap_or("").lines().next().unwrap_or("").trim().chars().take(200).collect();
            out.push(Skill { name: name.into(), path: path.into(), description, scope: s["scope"].as_str().unwrap_or("user").into() });
        }
    }
    out
}

/// 文字里点名的技能（`$名字`，和 Codex TUI 一样）：前面是开头或空白，名字后面不能紧跟名字里的字符
fn mentioned_skills<'a>(text: &str, skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut out: Vec<&Skill> = vec![];
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.');
    for (i, _) in text.match_indices('$') {
        if text[..i].chars().next_back().is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches('.');
        if let Some(s) = skills.iter().find(|s| s.name == word) {
            if !out.iter().any(|x| x.name == s.name) {
                out.push(s);
            }
        }
    }
    out
}

/// Codex 这边每个终端多记的状态
#[derive(Default)]
pub struct CodexSt {
    /// 这个 thread 加载在第几个 app-server 里（不等于当前的就要重新 `thread/resume`）
    pub generation: u64,
    /// 正在跑的这一轮
    pub turn: Option<String>,
    /// app-server 最近报的这段对话是不是 `active`（`thread/status/changed`、`thread/start`、`thread/resume`）
    pub thread_active: bool,
    /// 这段对话收到过几次状态 / 轮次变化：后台 `thread/resume` 回来得晚时，不拿旧快照盖掉新状态
    status_seq: u64,
    /// 结束过几次轮次（`turn/completed` 或 `thread/status/changed` 变成非 active）：`turn/start` 等请求晚返回时不把已结束的一轮重新标成运行中
    done_seq: u64,
    /// 最近已经 `turn/completed` 的 turn id（防晚到的 `turn/start` 答复或迟到的 `item/started` 把空闲对话重新改成处理中）
    completed_turns: VecDeque<String>,
    pub window: Option<u64>,
    /// 这个会话累计的 token（app-server 的 `tokenUsage.total`）
    pub totals: Option<Value>,
    preview_at: Option<Instant>,
    /// 运行中插话发出去、Codex 还没读到的（做完正在跑的这一步才会进对话），界面上显示成“排队中”
    pub steered: Vec<String>,
    /// Tab 排到这一轮之后的（Codex TUI 的 Tab：不插话，这一轮结束后作为下一轮发出）：(文字, 图片)。
    /// 一轮结束发出第一条；被停下的那一轮之后不自动发（和 TUI 一样，留给用户决定）
    pub later: Vec<(String, Vec<String>)>,
    /// 正在重连模型服务（`error` 带 `willRetry`）：活动文字是 Codex 的提示，有新进展就去掉
    retrying: bool,
    /// 这段对话的工作目录（app-server 的 `thread.cwd`；在托管工作树里时不是终端的工作目录）
    pub cwd: Option<String>,
    /// 界面上选的权限（[`PERMS`] 之一）：之后每一轮、重新加载对话都带上；没选过是 None，按 Codex 自己的配置
    pub perm: Option<String>,
    /// 对话页正在发消息（`codex_input` 还没返回：可能正在 `thread/start`、拍检查点、`turn/start`）
    sending: u32,
    /// 对话页发过消息的那段对话：它还没写出记录文件时也不是“还没聊过”，终端里 TUI 自己开的空对话不能把它顶掉
    prompted: Option<String>,
    /// 最近一次用到（加载、发消息、收到这段对话的通知）：空闲卸载按它算（见 [`Chat::codex_reap`]）
    used_at: Option<Instant>,
    /// 这段对话的目标（`/goal`，app-server 的 ThreadGoal）：设了以后 Codex 自己一轮轮接着做，直到完成、暂停或用完预算
    pub goal: Option<Value>,
    /// 最近一次更新 `goal` 对应的 app-server WebSocket 帧序号（防止 `oneshot` 请求答复之后被队列里更早的通知盖回旧状态）
    goal_seq: u64,
    /// 这段对话现在的速度档（app-server 的 `serviceTier`：`thread/start`、`thread/resume` 的答复、`thread/settings/updated`）。
    /// None / `default` = 标准速度；`priority`（模型目录里叫 Fast）= 快速模式；`flex` = 慢而便宜
    pub tier: Option<String>,
    /// 界面上（设置栏的开关、`/fast`）选的速度档：之后每一轮 `turn/start` 都带上；没选过是 None，照 Codex 自己的配置（`service_tier`）
    pub tier_set: Option<String>,
}

const COMPLETED_TURNS_CAP: usize = 32;
const STATUS_IDLE_SETTLE: Duration = Duration::from_millis(500);

impl CodexSt {
    /// 换到另一段对话时清掉上一段对话的运行态与 `/goal`，避免串到新对话上
    fn reset_thread(&mut self) {
        self.turn = None;
        self.thread_active = false;
        self.status_seq = self.status_seq.wrapping_add(1);
        self.done_seq = self.done_seq.wrapping_add(1);
        self.completed_turns.clear();
        self.steered.clear();
        self.retrying = false;
        self.goal = None;
        self.goal_seq = 0;
    }

    fn set_goal(&mut self, seq: u64, goal: Option<Value>) -> bool {
        if seq != 0 && seq < self.goal_seq {
            return false;
        }
        self.goal_seq = self.goal_seq.max(seq);
        let changed = self.goal != goal;
        self.goal = goal;
        changed
    }

    fn mark_turn_completed(&mut self, turn_id: Option<&str>) {
        if let Some(tid) = turn_id.filter(|s| !s.is_empty()) {
            if !self.completed_turns.iter().any(|x| x == tid) {
                if self.completed_turns.len() >= COMPLETED_TURNS_CAP {
                    self.completed_turns.pop_front();
                }
                self.completed_turns.push_back(tid.to_string());
            }
        }
    }

    fn is_turn_completed(&self, turn_id: &str) -> bool {
        self.completed_turns.iter().any(|x| x == turn_id)
    }
}

fn goal_active(st: &CodexSt) -> bool {
    st.goal.as_ref().is_some_and(|g| g["status"] == "active")
}

/// 快速模式开着：这段对话的速度档不是标准、也不是 flex（`priority`、`fast`、`ultrafast`…）
pub fn fast_on(st: &CodexSt) -> bool {
    st.tier_set.as_deref().or(st.tier.as_deref()).is_some_and(|t| !matches!(t, "" | "default" | "flex"))
}

/// 模型目录里这个模型的快速档 id（`serviceTiers` 里名字叫 Fast 的，一般是 `priority`；老的 app-server 只有 `additionalSpeedTiers: ["fast"]`）
fn fast_tier_of(m: &Value) -> Option<String> {
    let tiers = m["serviceTiers"].as_array();
    let named = tiers.and_then(|a| a.iter().find(|t| t["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("fast")) || matches!(t["id"].as_str(), Some("priority" | "fast"))));
    if let Some(id) = named.and_then(|t| t["id"].as_str()) {
        return Some(id.to_string());
    }
    m["additionalSpeedTiers"].as_array().is_some_and(|a| a.iter().any(|x| x == "fast")).then(|| "priority".to_string())
}

/// Codex 对话起的后台命令（app-server 的 `thread/backgroundTerminals/list`），资源看板用
#[derive(Debug, Clone)]
pub struct BgTerm {
    /// 终端 id
    pub session: String,
    /// app-server 里的进程号（停止时用），不是系统进程号
    pub process_id: String,
    pub command: String,
    pub pid: Option<u32>,
}

/// `codex_input` 期间标着“正在发”，返回（包括出错）时去掉
struct Sending(Arc<Inst>);

impl Sending {
    fn new(inst: Arc<Inst>) -> Self {
        {
            let mut st = lock(&inst.st);
            st.codex.sending += 1;
            st.codex.used_at = Some(Instant::now());
        }
        Self(inst)
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        let mut st = lock(&self.0.st);
        st.codex.sending = st.codex.sending.saturating_sub(1);
    }
}

/// 发出排在一轮之后的消息（在通知处理里起任务；装箱是为了断开 codex_input → 通知处理 → codex_input 的类型递归）
fn send_later(me: Arc<Chat>, id: String, text: String, images: Vec<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        if let Err(e) = me.codex_input(&id, &text, &images).await {
            tracing::warn!(instance = %id, "排在这一轮之后的消息没发出去：{e:?}");
        }
    })
}

/// 权限模式，和 TUI 的 `/permissions` 一样三档：只读（改文件、联网要问）、默认（工作目录里随便改，出了目录要问）、完全访问（不问、不用沙箱），
/// 另有助理派活用的两档（[`ASK_COMMANDS`]、[`FULL_GUARDED`]，菜单里列在后面，Shift+Tab 不轮换到）
pub const PERMS: [&str; 5] = ["readOnly", "workspaceWrite", "fullAccess", ASK_COMMANDS, FULL_GUARDED];
/// 动手前问：工作区可写，每条命令都来问（untrusted）；改项目里的文件看看替它放（[`auto_accept`]）
pub const ASK_COMMANDS: &str = "askCommands";
/// 全权但危险的问：不用沙箱，每条命令都先来问（untrusted），看看照命令规则、风险替它放，命中的才问用户。
/// Codex 自己不问的命令看看拦不到，所以全权不直接用 never
pub const FULL_GUARDED: &str = "fullGuarded";
/// Codex 问“要不要让 MCP 服务跑这个工具”（`mcpServer/elicitation/request`，`_meta.codex_approval_kind = mcp_tool_call`）：
/// 没标只读的 MCP 工具在 untrusted / on-request 下每次调用都来问（实测 0.171）。挂起时 `Pending.tool` 记成这个
const MCP_CALL: &str = "mcp_tool_call";

/// 挂起的审批按什么算：MCP 工具调用是 [`MCP_CALL`]（`tool` 是服务名，界面标题要用），其余就是 `tool`
fn approval_tool<'a>(tool: &'a str, input: &Value) -> &'a str {
    if input.get("mcp_call").is_some() { MCP_CALL } else { tool }
}

/// 替用户放行时给 app-server 的答复
fn accept_result(tool: &str, input: &Value) -> Value {
    match tool {
        MCP_CALL => json!({ "action": "accept", "content": {} }),
        "permissions" => json!({ "permissions": input["permissions"], "scope": "turn" }),
        _ => json!({ "decision": "accept" }),
    }
}

/// 来问的审批看看能不能照会话现在的权限模式直接替用户放（`tool` 是 [`MCP_CALL`] 时是 MCP 工具调用）：
/// - 全权（[`FULL_GUARDED`]）：命令规则没拦（调用前已经看过）、不是高风险的命令都放，改文件、MCP 工具都放；
/// - 完全访问：都放。Codex 在完全访问下本来不问，来问的是换模式前就开始的这一轮（审批策略一轮里换不了）；
/// - 动手前问：只放改项目里的文件。
fn auto_accept(perm: &str, tool: &str, input: &Value, root: &std::path::Path) -> bool {
    match (perm, tool) {
        ("fullAccess", _) => true,
        (FULL_GUARDED, MCP_CALL) => true,
        (FULL_GUARDED, "Bash") => super::risk::level("Bash", input) != "high",
        (FULL_GUARDED, "apply_patch") => true,
        (ASK_COMMANDS, "apply_patch") => {
            let inside = |p: &str| {
                let p = std::path::Path::new(p);
                let abs = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
                // 不认 ..（拼出来的路径可能跳出项目）
                !abs.components().any(|c| matches!(c, std::path::Component::ParentDir)) && abs.starts_with(root)
            };
            let files: Vec<&str> = input["changes"].as_array().into_iter().flatten().filter_map(|c| c["path"].as_str()).collect();
            input["grant_root"].is_null() && !files.is_empty() && files.iter().all(|f| inside(f))
        }
        _ => false,
    }
}

/// 权限模式 → (`approvalPolicy`, `thread/start` 的 `sandbox`, `turn/start` 的 `sandboxPolicy`)
fn perm_params(perm: &str) -> (Value, Value, Value) {
    match perm {
        "readOnly" => (json!("on-request"), json!("read-only"), json!({ "type": "readOnly", "networkAccess": false })),
        "fullAccess" => (json!("never"), json!("danger-full-access"), json!({ "type": "dangerFullAccess" })),
        FULL_GUARDED => (json!("untrusted"), json!("danger-full-access"), json!({ "type": "dangerFullAccess" })),
        ASK_COMMANDS => (
            json!("untrusted"),
            json!("workspace-write"),
            json!({ "type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false }),
        ),
        _ => (
            json!("on-request"),
            json!("workspace-write"),
            json!({ "type": "workspaceWrite", "writableRoots": [], "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false }),
        ),
    }
}

/// app-server 报的审批策略和沙箱 → 权限模式；对不上三档的（`untrusted`、细分的审批）原样给界面，显示成“自定义”
fn perm_of(approval: &Value, sandbox: &Value) -> Option<String> {
    let kind = sandbox["type"].as_str().or(sandbox.as_str())?;
    Some(match (approval.as_str(), kind) {
        (Some("never"), "dangerFullAccess" | "danger-full-access") => "fullAccess".into(),
        (Some("on-request"), "readOnly" | "read-only") => "readOnly".into(),
        (Some("on-request"), "workspaceWrite" | "workspace-write") => "workspaceWrite".into(),
        (Some("untrusted"), "workspaceWrite" | "workspace-write") => ASK_COMMANDS.into(),
        (Some("untrusted"), "dangerFullAccess" | "danger-full-access") => FULL_GUARDED.into(),
        (a, k) => format!("{}/{k}", a.unwrap_or("custom")),
    })
}

/// 界面或助理切换过的权限模式（客户端重启后重新 `thread/resume` 照旧沿用，不回退到建会话时的 `full_access`）
pub(super) fn perm_key(id: &str) -> String {
    format!("codex.perm.{id}")
}

/// 在托管工作树里开的对话：记下它的目录，之后加载（`thread/resume`）时用它，不用终端的工作目录
fn thread_cwd_key(thread: &str) -> String {
    format!("codex_thread_cwd.{thread}")
}

fn resets_iso(secs: Option<i64>) -> Value {
    secs.and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok())
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .map(Value::String)
        .unwrap_or(Value::Null)
}

/// app-server 的限额窗口 → 界面用的 `{ used, resets_at }`；按窗口长度认 5 小时 / 7 天。
/// 另带 credits（额度用完后接着用的余额，ChatGPT 里买的：`{ has, unlimited, balance }`，`balance` 是 credits 数，不是钱）
/// 和 `reset_credits`（能把限额立刻清零的重置机会还有几次）、`reset_cards`（每张的说明和期限，后端给了才有，null 时只知道张数）
fn limits_view(r: &Value) -> Value {
    let snap = &r["rateLimits"];
    let c = &snap["credits"];
    let credits = c["hasCredits"].as_bool().map(|has| {
        let balance = c["balance"].as_str().and_then(|b| b.trim().parse::<f64>().ok()).or_else(|| c["balance"].as_f64());
        json!({ "has": has, "unlimited": c["unlimited"].as_bool().unwrap_or(false), "balance": balance })
    });
    let reset_credits = r["rateLimitResetCredits"]["availableCount"].as_i64();
    let reset_cards = r["rateLimitResetCredits"]["credits"].as_array().map(|a| {
        a.iter()
            .filter(|c| c["status"] != "redeemed")
            .map(|c| {
                json!({ "id": c["id"], "title": c["title"], "description": c["description"], "status": c["status"],
                        "granted_at": resets_iso(c["grantedAt"].as_i64()), "expires_at": resets_iso(c["expiresAt"].as_i64()) })
            })
            .collect::<Vec<_>>()
    });
    let mut out = json!({ "five_hour": null, "seven_day": null, "plan": snap["planType"], "credits": credits, "reset_credits": reset_credits,
                          "reset_cards": reset_cards, "reached": snap["rateLimitReachedType"] });
    for w in [&snap["primary"], &snap["secondary"]] {
        if w.is_null() {
            continue;
        }
        let view = json!({ "used": w["usedPercent"], "resets_at": resets_iso(w["resetsAt"].as_i64()), "minutes": w["windowDurationMins"] });
        let key = match w["windowDurationMins"].as_i64() {
            Some(m) if m <= 24 * 60 => "five_hour",
            _ => "seven_day",
        };
        out[key] = view;
    }
    out
}

/// Codex 还没报窗口时估一个（照 0.157 的算法）：看看的模型设置 / 用户 config.toml 的 `model_context_window`，
/// 否则 `models_cache.json` 里这个模型的 `context_window × effective_context_window_percent%`（Codex 报的就是这个，例如 272000 → 258400）。
/// 第三方接口的模型目录里没有时返回 None（界面不显示百分比）。
fn codex_window_guess(codex_home: &Path, profile: &crate::codex_profiles::Profile, model: &str) -> Option<(u64, super::models::WindowSource)> {
    use super::models::WindowSource;
    // 看看的模型设置（`-c`）优先于用户的 config.toml；两者都和目录里的一样再乘有效比例
    let set = profile.tuning.context_window.or_else(|| {
        std::fs::read_to_string(codex_home.join("config.toml"))
            .ok()
            .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
            .and_then(|d| d.get("model_context_window").and_then(|v| v.as_integer()))
            .filter(|&w| w > 0)
            .map(|w| w as u64)
    });
    let cache: Value = std::fs::read(codex_home.join("models_cache.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let m = cache["models"].as_array().and_then(|a| a.iter().find(|m| m["slug"].as_str() == Some(model)));
    let pct = m.and_then(|m| m["effective_context_window_percent"].as_u64()).unwrap_or(95).clamp(1, 100);
    if let Some(w) = set {
        return Some((w * pct / 100, WindowSource::Setting));
    }
    // 第三方接口（看看配的服务商）的模型名不在 OpenAI 的目录里
    let w = m.and_then(|m| m["context_window"].as_u64()).filter(|&w| w > 0)?;
    Some((w * pct / 100, WindowSource::Catalog))
}

/// Codex 的自动压缩点（`model_auto_compact_token_limit`）：看看的模型设置（`-c`）优先于用户的 config.toml；
/// 都没设 = Codex 按模型自己定（界面就按窗口算）。和 Claude 的 `models::Compact` 同一个形状，界面共用一个算法
fn codex_compact(codex_home: &Path, profile: &crate::codex_profiles::Profile) -> Option<Value> {
    if let Some(n) = profile.tuning.compact_limit {
        return Some(json!({ "enabled": true, "at": n, "source": "tuning" }));
    }
    let path = codex_home.join("config.toml");
    let n = std::fs::read_to_string(&path).ok()?.parse::<toml_edit::DocumentMut>().ok()?.get("model_auto_compact_token_limit")?.as_integer().filter(|&n| n > 0)?;
    let shown = dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| format!("~/{}", r.display()))).unwrap_or_else(|| path.display().to_string());
    Some(json!({ "enabled": true, "at": n as u64, "source": shown }))
}

/// `model/list` → 模型菜单的选项（和 Claude Code 的同一个格式，见 `models::Opt`）
fn model_options(list: &Value) -> Vec<Value> {
    list["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m["model"].as_str().or_else(|| m["id"].as_str())?;
                    let efforts: Vec<&str> = m["supportedReasoningEfforts"].as_array().map(|e| e.iter().filter_map(|x| x["reasoningEffort"].as_str()).collect()).unwrap_or_default();
                    Some(json!({
                        "value": id,
                        "label": m["displayName"].as_str().unwrap_or(id),
                        "description": m["description"].as_str().unwrap_or(""),
                        "section": if m["hidden"].as_bool() == Some(true) { "more" } else { "main" },
                        "model": id,
                        "efforts": efforts,
                        "default_effort": m["defaultReasoningEffort"],
                        // 这个模型的快速档（没有 = 不支持快速模式）
                        "fast_tier": fast_tier_of(m),
                        "default": m["isDefault"].as_bool() == Some(true),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 第三方接口里对应过的 Codex 模型 → 模型菜单的选项：名字、强度照 Codex 目录（`catalog`，取不到时就用模型名），
/// `model` 是服务商的名字（回复里报的是它）、说明里写上；配置的默认模型是对应过的那个时它当默认
fn mapped_options(profile: &crate::codex_profiles::Profile, catalog: &Value) -> Vec<Value> {
    let known = catalog.as_array().cloned().unwrap_or_default();
    let mut out: Vec<Value> = profile
        .models
        .iter()
        .map(|(native, remote)| {
            let mut o = known.iter().find(|o| o["value"].as_str() == Some(native.as_str())).cloned().unwrap_or_else(|| json!({ "value": native, "label": native, "efforts": [] }));
            o["model"] = json!(remote);
            o["description"] = json!(remote);
            o["section"] = json!("main");
            o["fast_tier"] = Value::Null;
            o["default"] = json!(profile.model.as_deref() == Some(native.as_str()));
            o
        })
        .collect();
    // 目录里的顺序（新的在前）；不在目录里的放后面
    let rank = |o: &Value| known.iter().position(|k| k["value"] == o["value"]).unwrap_or(usize::MAX);
    out.sort_by_key(rank);
    out
}

/// 第三方接口的模型 → 模型菜单的选项：指定的模型放最前面当默认，服务商的其他模型放“服务商的其他模型”
fn provider_model_options(chosen: Option<&str>, ids: &[String]) -> Vec<Value> {
    let opt = |id: &str, main: bool| json!({ "value": id, "label": id, "section": if main { "main" } else { "provider" }, "model": id, "efforts": [], "default": main });
    chosen
        .map(|m| opt(m, true))
        .into_iter()
        .chain(ids.iter().filter(|id| Some(id.as_str()) != chosen && id.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:".contains(c))).map(|id| opt(id, false)))
        .collect()
}

/// app-server 的提问 → 界面认的 AskUserQuestion 格式。
/// `other`：Codex 的 `isOther`，false 时不给“自己填”；`auto_at`：带 `autoResolutionMs` 时 Codex 到点自己往下走（毫秒时间戳），
/// 界面显示倒计时，到点后 `serverRequest/resolved` 收走卡片
fn questions_view(params: &Value) -> Value {
    let qs: Vec<Value> = params["questions"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|q| {
                    json!({
                        "id": q["id"],
                        "header": q["header"],
                        "question": q["question"],
                        "multiSelect": multi_hint(q),
                        "options": q["options"].as_array().cloned().unwrap_or_default(),
                        "secret": q["isSecret"],
                        "other": q["isOther"].as_bool().unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut v = json!({ "questions": qs });
    if let Some(ms) = params["autoResolutionMs"].as_u64().filter(|ms| *ms > 0) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        v["auto_at"] = json!(now + ms);
    }
    v
}

/// 题干、标题里写了可以多选（Codex 的提问工具没有多选字段；默认模式说明里让模型写 `(multi-select)`）
fn multi_hint(q: &Value) -> bool {
    const HINTS: [&str; 12] = ["multi-select", "multiselect", "multiple choice", "select all that apply", "choose all that apply", "one or more", "select multiple", "choose multiple", "多选", "可多选", "选多个", "可以选多个"];
    let text = format!("{} {}", q["header"].as_str().unwrap_or(""), q["question"].as_str().unwrap_or("")).to_lowercase();
    HINTS.iter().any(|h| text.contains(h))
}

/// 界面的回答（问题原文 → 选的答案）→ app-server 的 `{ answers: { <问题 id>: { answers: [..] } } }`
fn answers_result(input: &Value, answers: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for q in input["questions"].as_array().into_iter().flatten() {
        let (Some(id), Some(text)) = (q["id"].as_str(), q["question"].as_str()) else { continue };
        let picked: Vec<Value> = match &answers[text] {
            Value::String(s) => s.split(", ").filter(|s| !s.is_empty()).map(|s| json!(s)).collect(),
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        out.insert(id.to_string(), json!({ "answers": picked }));
    }
    json!({ "answers": out })
}

/// `/命令` 或 `/命令 参数`：参数（没有时空串）；不是这个命令时 None
fn slash_arg<'a>(t: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = t.strip_prefix(cmd)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.starts_with(char::is_whitespace).then(|| rest.trim())
}

/// `/review` 的参数 → review/start 的 target：空 = 未提交的改动（TUI 菜单第一项），`--base 分支`、`--commit 提交`，
/// 其余是自己写的审查要求
fn review_target(arg: &str) -> Value {
    if arg.is_empty() {
        return json!({ "type": "uncommittedChanges" });
    }
    if let Some(b) = arg.strip_prefix("--base ").map(str::trim).filter(|b| !b.is_empty()) {
        return json!({ "type": "baseBranch", "branch": b });
    }
    if let Some(c) = arg.strip_prefix("--commit ").map(str::trim).filter(|c| !c.is_empty()) {
        return json!({ "type": "commit", "sha": c });
    }
    json!({ "type": "custom", "instructions": arg })
}

/// Codex TUI 的 /init 送的说明（codex-rs/tui 的 prompt_for_init_command.md，0.169）
const INIT_PROMPT: &str = "Generate a file named AGENTS.md that serves as a contributor guide for this repository.
Before writing, check whether AGENTS.md already exists in the current working directory. If it does, do not overwrite or modify it.
Your goal is to produce a clear, concise, and well-structured document with descriptive headings and actionable explanations for each section.
Follow the outline below, but adapt as needed — add sections if relevant, and omit those that do not apply to this project.

Document Requirements

- Title the document \"Repository Guidelines\".
- Use Markdown headings (#, ##, etc.) for structure.
- Keep the document concise. 200-400 words is optimal.
- Keep explanations short, direct, and specific to this repository.
- Provide examples where helpful (commands, directory paths, naming patterns).
- Maintain a professional, instructional tone.

Recommended Sections

Project Structure & Module Organization

- Outline the project structure, including where the source code, tests, and assets are located.

Build, Test, and Development Commands

- List key commands for building, testing, and running locally (e.g., npm test, make build).
- Briefly explain what each command does.

Coding Style & Naming Conventions

- Specify indentation rules, language-specific style preferences, and naming patterns.
- Include any formatting or linting tools used.

Testing Guidelines

- Identify testing frameworks and coverage requirements.
- State test naming conventions and how to run tests.

Commit & Pull Request Guidelines

- Summarize commit message conventions found in the project’s Git history.
- Outline pull request requirements (descriptions, linked issues, screenshots, etc.).

(Optional) Add other sections if relevant, such as Security & Configuration Tips, Architecture Overview, or Agent-Specific Instructions.
";

/// 界面的回答 → app-server 请求的答复
pub fn decision(tool: &str, kind: &str, input: &Value, req: &PermissionAnswer) -> Result<Value, LocalError> {
    if kind == "question" {
        return match req.decision.as_str() {
            "answer" => Ok(answers_result(input, req.answers.as_ref().filter(|a| a.is_object()).ok_or_else(|| LocalError::invalid("answers", "required"))?)),
            // 不回答：空答案，Codex 照常往下走
            "deny" => Ok(json!({ "answers": {} })),
            _ => Err(LocalError::invalid("decision", "invalid")),
        };
    }
    // 多给权限：允许就照它要的给，这一轮或者本会话；拒绝 = 什么都不给
    if tool == "permissions" {
        return match req.decision.as_str() {
            "allow" => Ok(json!({ "permissions": input["permissions"], "scope": "turn" })),
            "allow_session" => Ok(json!({ "permissions": input["permissions"], "scope": "session" })),
            "deny" => Ok(json!({ "permissions": {} })),
            _ => Err(LocalError::invalid("decision", "invalid")),
        };
    }
    let d = match req.decision.as_str() {
        "allow" => "accept",
        "allow_session" => "acceptForSession",
        "deny" => "decline",
        _ => return Err(LocalError::invalid("decision", "invalid")),
    };
    Ok(json!({ "decision": d }))
}

/// 命令审批里的命令：字符串，或者 argv 数组（拼成一行看）
fn command_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// 用户消息条目（`userMessage`）里的文字，和发出去时的一样拼起来
fn user_text(item: &Value) -> String {
    let parts: Vec<&str> = item["content"].as_array().map(|a| a.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect()).unwrap_or_default();
    parts.join("\n")
}

/// 运行中的命令显示成什么：Codex 拆出来的命令（`commandActions`），没有就是整条（带着 `bash -lc` 之类的外壳）
fn command_shown(item: &Value) -> Option<String> {
    let parts: Vec<&str> = item["commandActions"].as_array().map(|a| a.iter().filter_map(|c| c["command"].as_str()).collect()).unwrap_or_default();
    if parts.is_empty() {
        item["command"].as_str().map(String::from)
    } else {
        Some(parts.join(" && "))
    }
}

/// 不是用户自己的对话：临时的（起名用的）、子代理、Codex 自己内部用的（`threadSource` 不是 user）
fn side_thread(t: &Value) -> bool {
    t["ephemeral"] == true || t["parentThreadId"].is_string() || t["source"].get("subAgent").is_some() || t["threadSource"].as_str().is_some_and(|s| s != "user")
}

/// 起名的提示词，照 Codex TUI 的（`tui/src/app/thread_title.rs`），用户的话最多带 960 字节
fn title_prompt(text: &str) -> String {
    let head = format!(
        "Generate a concise, single-line task title of at most {TITLE_MAX} characters and under five words where possible. \
         Start with an imperative verb. Capitalize only the first word unless the user's language, proper nouns, acronyms, or code terms require otherwise. \
         Preserve ticket references exactly. Write in the user's language. Do not use quotes, markdown, or trailing punctuation. Do not answer the request.\n\nUser prompt:\n"
    );
    let room = 960usize.saturating_sub(head.len());
    let text: String = text.trim().char_indices().take_while(|(i, c)| i + c.len_utf8() <= room).map(|(_, c)| c).collect();
    format!("{head}{text}")
}

/// 起名的回答（`{"title": "…"}`）→ 名字：去掉引号、多余的空白和结尾的标点
fn parse_title(answer: &str) -> Option<String> {
    let v: Value = serde_json::from_str(answer.trim()).ok()?;
    let t = v["title"].as_str()?.trim().trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'));
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_end_matches(['.', '?', '!', '。', '？', '！']).trim_end();
    (!t.is_empty()).then(|| t.chars().take(TITLE_MAX).collect())
}

fn text_input(text: &str, images: &[String]) -> Value {
    let mut v = vec![];
    if !text.is_empty() {
        v.push(json!({ "type": "text", "text": text, "text_elements": [] }));
    }
    for p in images {
        v.push(json!({ "type": "localImage", "path": p }));
    }
    json!(v)
}

impl Chat {
    pub(super) fn is_codex(&self, id: &str) -> bool {
        self.inst(id).agent.launch() == "codex"
    }

    fn codex_saved_perm(&self, id: &str) -> Option<String> {
        self.store.get::<String>(&perm_key(id)).ok().flatten().filter(|m| PERMS.contains(&m.as_str()))
    }

    /// 界面上选过权限就用选的；否则全部权限的终端不询问、不用沙箱（和 `codex --yolo` 一样）。
    /// 助理的大脑：规矩、助理工具、按助理权限模式定的沙箱（见 `assistant::codex_thread`），不看界面选的
    fn codex_policy(&self, id: &str) -> Value {
        if crate::assistant::is_brain(&self.store, id) {
            if let Some(addr) = *lock(&self.addr) {
                return crate::assistant::codex_thread(&self.store, &self.paths, addr);
            }
        }
        if crate::assistant::is_ask(&self.store, id) {
            return crate::assistant::codex_ask_thread();
        }
        if crate::assistant::is_errand(&self.store, id) {
            let perm = lock(&self.inst(id).st).codex.perm.clone().or_else(|| crate::assistant::errand_mode(&self.store, id));
            return match perm {
                Some(p) => {
                    let (approval, sandbox, _) = perm_params(&p);
                    json!({ "developerInstructions": crate::assistant::ERRAND_RULES, "approvalPolicy": approval, "sandbox": sandbox })
                }
                None => crate::assistant::codex_errand_thread(),
            };
        }
        let mut v = if let Some(perm) = lock(&self.inst(id).st).codex.perm.clone().or_else(|| self.codex_saved_perm(id)) {
            let (approval, sandbox, _) = perm_params(&perm);
            json!({ "approvalPolicy": approval, "sandbox": sandbox })
        } else if self.instances.opts(id).full_access {
            json!({ "approvalPolicy": "never", "sandbox": "danger-full-access" })
        } else {
            json!({})
        };
        // 助理开的会话：多一个找助理的工具（assistant_note）
        if let Some(addr) = *lock(&self.addr) {
            if let Some(cfg) = crate::assistant::codex_worker_config(&self.store, id, addr) {
                v["config"] = cfg;
            }
        }
        v
    }

    /// `thread/start`、`thread/resume`、`thread/fork` 的参数：权限，加上这个会话的 Codex 配置选的服务商和模型
    fn codex_thread_opts(&self, id: &str) -> Value {
        let mut v = self.codex_policy(id);
        let profile = crate::codex_profiles::for_instance(&self.store, id);
        let mut p = crate::codex_profiles::thread_params(&self.paths.home, &profile);
        // 两边都有 `config`（大脑的助理工具 + 配置里的调优）：合在一起，不互相盖掉
        if let (Some(a), Some(b)) = (v.get("config").and_then(Value::as_object).cloned(), p.get_mut("config").and_then(Value::as_object_mut)) {
            for (k, x) in a {
                b.entry(k).or_insert(x);
            }
        }
        merge(&mut v, p);
        // 会话起之前已经选过模型或思考强度（助理派活 / 跑腿默认、草稿页预选）：建 thread 时就带上，不等第一轮 turn/start
        let (chosen_model, chosen_effort) = {
            let inst = self.inst(id);
            let st = lock(&inst.st);
            if st.model_set_at.is_some() { (st.model.clone(), st.effort.clone()) } else { (None, None) }
        };
        if let Some(m) = chosen_model {
            v["model"] = json!(profile.remote(&m));
        }
        if let Some(e) = chosen_effort {
            if !v["config"].is_object() {
                v["config"] = json!({});
            }
            v["config"]["model_reasoning_effort"] = json!(e);
        }
        v
    }

    /// 拿到能用的 app-server：没起过、退出了、第三方接口的配置变了（且没有在跑的对话）就起一个新的。
    /// 所有 Codex 配置都定义在这一个 app-server 上，每个会话用哪个由 [`Self::codex_thread_opts`] 选。
    async fn codex_server(&self) -> Result<Arc<AppServer>, LocalError> {
        let env = crate::codex_profiles::server_env(&self.store, &self.paths.home);
        let program = codex_app::program(self.instances.shell()).await;
        let desk = codex_app::desktop_env();
        let mut signature = format!("{}|{}", program.display(), env.as_ref().map(|e| e.signature()).unwrap_or_default());
        let pipe = codex_app::desktop_signature(&desk);
        if !pipe.is_empty() {
            signature = format!("{signature}|desk:{pipe}");
        }
        let mut slot = self.codex.server.lock().await;
        if let Some(s) = slot.as_ref().filter(|s| s.alive()) {
            let busy = lock(&self.codex.threads).values().any(|id| lock(&self.inst(id).st).phase != Phase::Idle);
            let stale = self.codex.stale.load(Ordering::SeqCst);
            if (s.signature == signature && !stale) || busy {
                return Ok(s.clone());
            }
            self.codex.stale.store(false, Ordering::SeqCst);
            s.kill();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let server = AppServer::start(&self.paths.home.join("run"), program, self.instances.shell(), env.as_ref(), &desk, signature, tx).await?;
        self.codex.generation.fetch_add(1, Ordering::SeqCst);
        lock(&self.codex.threads).clear();
        *slot = Some(server.clone());
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(me.codex_events(rx));
        }
        Ok(server)
    }

    fn codex_inst_of(&self, params: &Value) -> Option<Arc<Inst>> {
        let thread = params["threadId"].as_str()?;
        let id = lock(&self.codex.threads).get(thread).cloned()?;
        Some(self.inst(&id))
    }

    /// app-server 起好了。不加载这个终端现在的会话（换会话、新开会话用：现在的会话接不上也不妨碍换走）。
    /// 终端标签是普通 shell，和对话没有关系：里面跑着什么都不管
    pub(super) async fn codex_ready(&self, _id: &str) -> Result<Arc<AppServer>, LocalError> {
        self.codex_server().await
    }

    /// 如果 Codex app-server 正在运行，顺便通知它退出登录（让对话页里的状态立即更新）
    pub async fn codex_logout_server(&self) {
        let server = self.codex.server.lock().await.as_ref().filter(|s| s.alive()).cloned();
        if let Some(s) = server {
            if let Err(e) = s.call("account/logout", json!({})).await {
                tracing::debug!(error = ?e, "codex app-server account/logout 失败");
            }
        }
    }

    /// 让这个终端的 thread 在当前 app-server 里加载着（打开对话、发消息、回答之前调用）。
    /// 还没有会话时什么都不做：第一条消息时再 `thread/start`。
    /// app-server 好好的、只是这段会话接不上时返回 `CODEX_RESUME_FAILED`（界面上让用户换一段，不当成 Codex 没启动）。
    pub(super) async fn codex_attach(&self, id: &str) -> Result<Arc<AppServer>, LocalError> {
        let server = self.codex_ready(id).await?;
        let gen = self.codex.generation.load(Ordering::SeqCst);
        let inst = self.inst(id);
        let (thread, loaded) = {
            let mut st = lock(&inst.st);
            st.codex.used_at = Some(Instant::now());
            (st.binding.as_ref().map(|b| b.session_id.clone()), st.codex.generation == gen)
        };
        if loaded {
            return Ok(server);
        }
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        // 刚开的、还没聊过的对话：app-server 里有它，但没有记录文件，resume 不了。
        // 先记下对应关系（状态通知能找到这个终端），第一轮写出记录后收到 thread/status/changed 再接（见 codex_note）
        let unwritten = {
            let st = lock(&inst.st);
            st.binding.as_ref().is_some_and(|b| !b.transcript_path.as_os_str().is_empty() && !b.transcript_path.is_file())
        };
        if let (Some(t), true) = (&thread, unwritten) {
            lock(&self.codex.threads).insert(t.clone(), id.to_string());
            let mut st = lock(&inst.st);
            if st.phase == Phase::Detached {
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
            }
            return Ok(server);
        }
        match thread {
            Some(t) => {
                let seq_before = lock(&inst.st).codex.status_seq;
                let mut params = json!({ "threadId": t, "cwd": self.codex_thread_cwd(&t, &row.workdir), "excludeTurns": true });
                merge(&mut params, self.codex_thread_opts(id));
                let r = resume_thread(&server, &t, params).await.map_err(|e| match e.code.as_str() {
                    "CODEX_REQUEST_FAILED" => LocalError::new("CODEX_RESUME_FAILED").with("detail", e.params.get("detail").cloned().unwrap_or(Value::Null)),
                    _ => e,
                })?;
                self.codex_loaded(&inst, &r, gen, Some(seq_before));
                self.codex_sync_resumed_goal(&inst, &server, &t, r["thread"]["status"]["type"] == "idle").await;
            }
            None => {
                let mut st = lock(&inst.st);
                st.codex.generation = gen;
                if st.phase == Phase::Detached {
                    Self::set_phase(&inst, &mut st, Phase::Idle, None);
                }
            }
        }
        Ok(server)
    }

    /// 重新接上一段对话时顺便同步 `/goal`；对话本身已经是 `idle`、数据库里的 goal 却还挂着 `active`
    /// （上次在 TUI 按 Esc 停下、或者 app-server 重启过）时，跟 TUI 中断一样改成 `paused`，免得用户发下一条普通任务做完后突然接着跑旧目标
    async fn codex_sync_resumed_goal(&self, inst: &Inst, server: &AppServer, tid: &str, resumed_idle: bool) {
        let Ok((mut seq, r)) = server.call_seq("thread/goal/get", json!({ "threadId": tid })).await else { return };
        let mut goal = Some(r["goal"].clone()).filter(|g| !g.is_null());
        let still_idle = {
            let st = lock(&inst.st);
            st.binding.as_ref().map(|b| b.session_id.as_str()) == Some(tid) && st.phase == Phase::Idle && !st.codex.thread_active && st.codex.turn.is_none()
        };
        if resumed_idle && still_idle && goal.as_ref().is_some_and(|g| g["status"] == "active") {
            if let Ok((s2, paused)) = server.call_seq("thread/goal/set", json!({ "threadId": tid, "status": "paused", "origin": "user" })).await {
                seq = s2;
                if !paused["goal"].is_null() {
                    goal = Some(paused["goal"].clone());
                }
            }
        }
        let mut st = lock(&inst.st);
        if st.binding.as_ref().map(|b| b.session_id.as_str()) == Some(tid) && st.codex.set_goal(seq, goal) {
            Self::emit_status(inst, &st);
        }
    }

    /// 把这段对话里还在 `active` 的 `/goal` 改成 `paused`（点停止、收到 `interrupted` 时用，和 Codex TUI 的 `thread_goal_actions.rs` 一致）
    async fn codex_pause_active_goal(&self, inst: &Inst, server: &AppServer, tid: &str, check_remote: bool) {
        let (known_active, unknown) = {
            let st = lock(&inst.st);
            (goal_active(&st.codex), st.codex.goal.is_none())
        };
        let active = if known_active {
            true
        } else if check_remote && unknown {
            server.call("thread/goal/get", json!({ "threadId": tid })).await.is_ok_and(|r| r["goal"]["status"] == "active")
        } else {
            false
        };
        if !active {
            return;
        }
        if let Ok((seq, r)) = server.call_seq("thread/goal/set", json!({ "threadId": tid, "status": "paused", "origin": "user" })).await {
            if !r["goal"].is_null() {
                let mut st = lock(&inst.st);
                if st.binding.as_ref().map(|b| b.session_id.as_str()) == Some(tid) && st.codex.set_goal(seq, Some(r["goal"].clone())) {
                    Self::emit_status(inst, &st);
                }
            }
        }
    }

    /// `thread/start`、`thread/resume` 的答复：记下会话、记录文件、模型；按 thread 的状态设好空闲 / 运行中。
    fn codex_loaded(&self, inst: &Inst, r: &Value, gen: u64, seq_before: Option<u64>) {
        let t = &r["thread"];
        let Some(tid) = t["id"].as_str() else { return };
        lock(&self.codex.threads).insert(tid.to_string(), inst.id.clone());
        let path = t["path"].as_str().map(PathBuf::from).filter(|p| inst.agent.valid_transcript(p));
        let mut st = lock(&inst.st);
        let switched = st.binding.as_ref().is_none_or(|b| b.session_id != tid);
        if switched {
            Self::drop_pending(&mut st);
            st.codex.reset_thread();
        }
        let old_path = (!switched).then(|| st.binding.as_ref().map(|b| b.transcript_path.clone())).flatten().filter(|p| !p.as_os_str().is_empty());
        let b = Binding { session_id: tid.to_string(), transcript_path: path.or(old_path).unwrap_or_default() };
        if st.binding.as_ref() != Some(&b) {
            let _ = self.store.set(&super::binding_key(&inst.id), &b);
            st.binding = Some(b);
        }
        st.codex.generation = gen;
        st.codex.used_at = Some(Instant::now());
        if st.model_set_at.is_none() {
            st.model = r["model"].as_str().map(String::from);
            st.effort = r["reasoningEffort"].as_str().map(String::from);
        }
        st.actual_model = r["model"].as_str().map(String::from).or(st.actual_model.take());
        st.codex.tier = r["serviceTier"].as_str().map(String::from);
        st.title = t["name"].as_str().filter(|s| !s.is_empty()).map(String::from).or(st.title.take());
        st.version = t["cliVersion"].as_str().map(String::from).or(st.version.take());
        st.codex.cwd = t["cwd"].as_str().map(String::from).or(st.codex.cwd.take());
        if st.codex.perm.is_none() {
            st.codex.perm = crate::assistant::errand_mode(&self.store, &inst.id).or_else(|| self.codex_saved_perm(&inst.id));
        }
        st.mode = st.codex.perm.clone().or_else(|| perm_of(&r["approvalPolicy"], &r["sandbox"]));
        st.error = None;
        // 后台 resume 在路上时，如果这段对话已经收到了更新的实时状态（比如刚跑完发了 turn/completed），不再拿旧快照盖回去
        let fresh_snapshot = switched || seq_before.is_none_or(|s| s == st.codex.status_seq);
        if fresh_snapshot {
            let active = t["status"]["type"] == "active";
            st.codex.thread_active = active;
            if st.phase != Phase::Approval || st.pending.is_none() {
                Self::set_phase(inst, &mut st, if active { Phase::Running } else { Phase::Idle }, None);
            }
        }
        Self::emit_status(inst, &st);
        // 上下文用量：app-server 要等下一轮才发，先从记录文件里最后一次的用量填上（输入框旁的小圆环、“用量”栏）
        let seed = st.context.is_none().then(|| st.binding.as_ref().map(|b| b.transcript_path.clone())).flatten().filter(|p| !p.as_os_str().is_empty());
        drop(st);
        if let Some(u) = seed.and_then(|p| super::codex_rollout::last_token_usage(&p)) {
            let mut st = lock(&inst.st);
            if st.context.is_none() {
                set_usage(&mut st, &u);
                Self::emit_status(inst, &st);
            }
        }
    }

    /// 这段对话的工作目录：在托管工作树里开的用记下的那个，其他的用终端的工作目录
    fn codex_thread_cwd(&self, thread: &str, workdir: &str) -> String {
        self.store.get::<String>(&thread_cwd_key(thread)).ok().flatten().filter(|p| Path::new(p).is_dir()).unwrap_or_else(|| workdir.to_string())
    }

    /// 还没有会话：建一个
    async fn codex_new_thread(&self, server: &AppServer, inst: &Inst) -> Result<String, LocalError> {
        let row = self.store.instance(&inst.id)?.ok_or_else(LocalError::not_found)?;
        self.codex_new_thread_in(server, inst, &row.workdir).await
    }

    /// 在 `cwd` 里开一段新对话
    async fn codex_new_thread_in(&self, server: &AppServer, inst: &Inst, cwd: &str) -> Result<String, LocalError> {
        self.codex_trust(server, cwd).await;
        lock(&inst.st).codex.cwd = None;
        let mut params = json!({ "cwd": cwd });
        merge(&mut params, self.codex_thread_opts(&inst.id));
        let r = server.call("thread/start", params).await?;
        let tid = r["thread"]["id"].as_str().ok_or_else(|| LocalError::new("CODEX_REQUEST_FAILED"))?.to_string();
        self.codex_loaded(inst, &r, self.codex.generation.load(Ordering::SeqCst), None);
        let _ = inst.tx.send(super::Event::Reset);
        Ok(tid)
    }

    /// 换成接着以前的另一段对话：加载它、界面重新取。
    async fn codex_resume(&self, id: &str, old: Option<String>, to: &str) -> Result<(), LocalError> {
        self.codex_forget(old).await;
        self.codex_bind(id, to);
        let inst = self.inst(id);
        {
            let mut st = lock(&inst.st);
            st.title = None;
            st.context = None;
            Self::drop_pending(&mut st);
            st.codex.reset_thread();
        }
        self.codex_attach(id).await?;
        *inst.tr.lock().await = None;
        let _ = inst.tx.send(super::Event::Reset);
        Ok(())
    }

    /// 记录文件路径还没有时（新建的会话第一轮开始后才写文件）问一下 app-server
    async fn codex_fill_path(&self, inst: &Inst) {
        let tid = {
            let st = lock(&inst.st);
            match &st.binding {
                Some(b) if b.transcript_path.as_os_str().is_empty() => b.session_id.clone(),
                _ => return,
            }
        };
        let Some(server) = self.codex.server.lock().await.clone() else { return };
        let Ok(r) = server.call("thread/read", json!({ "threadId": tid })).await else { return };
        let Some(path) = r["thread"]["path"].as_str().map(PathBuf::from).filter(|p| inst.agent.valid_transcript(p)) else { return };
        let mut st = lock(&inst.st);
        if let Some(b) = st.binding.as_mut().filter(|b| b.session_id == tid) {
            b.transcript_path = path;
            let b = b.clone();
            let _ = self.store.set(&super::binding_key(&inst.id), &b);
        }
    }

    // ---------------- app-server 发来的 ----------------

    async fn codex_events(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Incoming>) {
        let gen = self.codex.generation.load(Ordering::SeqCst);
        while let Some(m) = rx.recv().await {
            match m {
                Incoming::Notification { seq, method, params } => self.codex_note_seq(seq, &method, &params).await,
                Incoming::Request { id, method, params } => {
                    let me = self.clone();
                    tokio::spawn(async move { me.codex_request(id, &method, params).await });
                }
                Incoming::Closed => break,
            }
        }
        // 进程退出：这个进程里的会话都要重新加载；挂着的审批作废
        if self.codex.generation.load(Ordering::SeqCst) != gen {
            return;
        }
        let ids: Vec<String> = lock(&self.codex.threads).drain().map(|(_, id)| id).collect();
        for id in ids {
            let inst = self.inst(&id);
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.codex.generation = 0;
            st.codex.turn = None;
            st.codex.thread_active = false;
            st.codex.goal_seq = 0;
            st.codex.status_seq = st.codex.status_seq.wrapping_add(1);
            st.codex.done_seq = st.codex.done_seq.wrapping_add(1);
            if st.phase != Phase::Idle {
                st.error = Some("Codex app-server exited".into());
                Self::set_phase(&inst, &mut st, Phase::Idle, None);
                Self::emit_status(&inst, &st);
            }
        }
    }

    #[cfg(test)]
    async fn codex_note(&self, method: &str, p: &Value) {
        self.codex_note_seq(0, method, p).await;
    }

    async fn codex_note_seq(&self, seq: u64, method: &str, p: &Value) {
        if self.codex_title_note(method, p) {
            return;
        }
        match method {
            "account/rateLimits/updated" => {
                *lock(&self.codex.limits) = None;
                return;
            }
            "item/started" if p["item"]["type"] == "fileChange" => {
                if let Some(item) = p["item"]["id"].as_str() {
                    lock(&self.codex.patches).insert(item.to_string(), p["item"]["changes"].clone());
                }
            }
            "thread/started" => return,
            _ => {}
        }
        let Some(inst) = self.codex_inst_of(p) else { return };
        // 认领来的对话第一轮写出记录了：让界面重新加载，重新加载时 codex_attach 会 resume 它，
        // 之后逐条输出、审批才会推给看看（这里直接调 codex_attach 会让 async 的类型绕回自己）
        if method == "thread/status/changed" {
            let gen = self.codex.generation.load(Ordering::SeqCst);
            let mut settle_seq = None;
            {
                let mut st = lock(&inst.st);
                if st.codex.generation != gen {
                    if has_rollout(&st) {
                        let _ = inst.tx.send(super::Event::Reset);
                    }
                    return;
                }
                let kind = p["status"]["type"].as_str().unwrap_or("");
                let active = kind == "active";
                st.active_at = crate::util::system_ms();
                st.codex.used_at = Some(Instant::now());
                st.codex.status_seq = st.codex.status_seq.wrapping_add(1);
                st.codex.thread_active = active;
                if !active {
                    st.codex.done_seq = st.codex.done_seq.wrapping_add(1);
                    if kind == "notLoaded" {
                        st.codex.generation = 0;
                        st.codex.turn = None;
                        st.codex.retrying = false;
                        Self::drop_pending(&mut st);
                        Self::set_phase(&inst, &mut st, Phase::Idle, None);
                    } else if st.codex.turn.is_none() && st.phase != Phase::Idle {
                        // 没有正在等的 turn/completed，但状态还挂在运行中 / 审批中：按权威状态立即收回空闲
                        st.codex.retrying = false;
                        Self::drop_pending(&mut st);
                        Self::set_phase(&inst, &mut st, Phase::Idle, None);
                    } else if st.codex.turn.is_some() && st.phase != Phase::Idle {
                        // 正常结束时 turn/completed 紧跟在 idle / systemError 后面（带着 failed / interrupted 原因）；
                        // 万一 turn/completed 没来或被竞态错过，兜底收回空闲，不让界面永远停在“正在处理…”
                        settle_seq = Some(st.codex.status_seq);
                    }
                }
            }
            if let (Some(seq), Some(me)) = (settle_seq, self.me.upgrade()) {
                let inst = inst.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(STATUS_IDLE_SETTLE).await;
                    let settled = {
                        let mut st = lock(&inst.st);
                        if st.codex.status_seq == seq && !st.codex.thread_active && st.codex.sending == 0 && st.phase != Phase::Idle {
                            st.codex.turn = None;
                            st.codex.retrying = false;
                            Self::drop_pending(&mut st);
                            Self::set_phase(&inst, &mut st, Phase::Idle, None);
                            true
                        } else {
                            false
                        }
                    };
                    if settled {
                        me.codex_fill_path(&inst).await;
                        me.poll(&inst).await;
                    }
                });
            }
            return;
        }
        let mut poll = false;
        let mut pause_goal_tid: Option<String> = None;
        {
            let mut st = lock(&inst.st);
            st.active_at = crate::util::system_ms();
            st.codex.used_at = Some(Instant::now());
            match method {
                "turn/started" => {
                    // 终端里直接打字开始的一轮也拍检查点（界面发的在 codex_input 里已经先拍了，一样的不会重复）
                    if let Some(me) = self.me.upgrade() {
                        let id = inst.id.clone();
                        tokio::spawn(async move { me.checkpoint_turn(&id, "", false).await });
                    }
                    let tid = p["turn"]["id"].as_str().map(String::from);
                    if let Some(t) = &tid {
                        st.codex.completed_turns.retain(|x| x != t);
                    }
                    st.codex.status_seq = st.codex.status_seq.wrapping_add(1);
                    st.codex.thread_active = true;
                    st.codex.turn = tid;
                    st.codex.retrying = false;
                    st.error = None;
                    inst.prompts.send_modify(|n| *n += 1);
                    Self::set_phase(&inst, &mut st, Phase::Running, None);
                }
                // 权限在别处改了（同一个 app-server 上的别的客户端）：界面跟着显示；界面选过的也换成它，免得下一轮又改回去
                "thread/settings/updated" => {
                    let set = &p["threadSettings"];
                    if let Some(tier) = set.get("serviceTier") {
                        st.codex.tier = tier.as_str().map(String::from);
                        Self::emit_status(&inst, &st);
                    }
                    if let Some(perm) = perm_of(&set["approvalPolicy"], &set["sandboxPolicy"]) {
                        if st.codex.perm.is_some() {
                            st.codex.perm = PERMS.contains(&perm.as_str()).then(|| perm.clone());
                        }
                        st.mode = Some(perm);
                        Self::emit_status(&inst, &st);
                    }
                }
                "turn/completed" => {
                    st.codex.status_seq = st.codex.status_seq.wrapping_add(1);
                    st.codex.done_seq = st.codex.done_seq.wrapping_add(1);
                    st.codex.thread_active = false;
                    st.codex.mark_turn_completed(p["turn"]["id"].as_str());
                    st.codex.turn = None;
                    st.codex.steered.clear();
                    st.codex.retrying = false;
                    Self::drop_pending(&mut st);
                    if p["turn"]["status"] == "failed" {
                        st.error = p["turn"]["error"]["message"].as_str().map(|s| s.chars().take(500).collect());
                    }
                    if p["turn"]["status"] == "interrupted" {
                        st.end_hint = Some(super::board::Outcome::Interrupted);
                        if goal_active(&st.codex) {
                            pause_goal_tid = p["threadId"].as_str().map(String::from);
                        }
                    }
                    Self::set_phase(&inst, &mut st, Phase::Idle, None);
                    // Tab 排在这一轮之后的：发出第一条（下一轮结束时再发下一条）
                    if p["turn"]["status"] != "interrupted" && !st.codex.later.is_empty() {
                        let (text, images) = st.codex.later.remove(0);
                        if let Some(me) = self.me.upgrade() {
                            tokio::spawn(send_later(me, inst.id.clone(), text, images));
                        }
                    }
                    Self::emit_status(&inst, &st);
                    poll = true;
                    // 这一轮没跑成（比如连不上模型服务）就先不起名，那句话留到下一轮
                    if let Some(tid) = p["threadId"].as_str().filter(|_| st.title.is_none() && p["turn"]["status"] == "completed") {
                        let prompt = lock(&self.codex.title_prompts).remove(tid);
                        if let (Some(me), Some(prompt)) = (self.me.upgrade(), prompt) {
                            tokio::spawn(me.codex_auto_title(inst.id.clone(), tid.to_string(), prompt));
                        }
                    }
                }
                "item/started" => {
                    let item = &p["item"];
                    let item_turn = p["turnId"].as_str();
                    let stale_turn = item_turn.is_some_and(|t| st.codex.is_turn_completed(t));
                    let wrong_turn = matches!((st.codex.turn.as_deref(), item_turn), (Some(cur), Some(it)) if cur != it);
                    if st.codex.turn.is_none() && st.codex.thread_active && !stale_turn {
                        st.codex.turn = item_turn.map(String::from);
                    }
                    let has_active_turn = !stale_turn && !wrong_turn && (st.codex.turn.is_some() || st.codex.thread_active);
                    if has_active_turn {
                        let (tool, activity) = match item["type"].as_str() {
                            Some("commandExecution") => (Some("Bash"), command_shown(item)),
                            Some("fileChange") => (Some("Edit"), None),
                            Some("mcpToolCall") => (Some("mcp"), item["tool"].as_str().map(String::from)),
                            Some("webSearch") => (Some("WebSearch"), None),
                            Some("reasoning") => (None, Some("thinking".to_string())),
                            // 压缩（手动 /compact 和一轮当中自动压缩都是这个条目）
                            Some("contextCompaction") => (None, Some("compacting".to_string())),
                            // 等子代理交回结果
                            Some("collabAgentToolCall") if item["tool"] == "wait" => (Some("Agent"), Some("waiting_agents".to_string())),
                            _ => (None, None),
                        };
                        st.codex.retrying = false;
                        // 新的一段回复：正在输出的从头来。运行中插话时 Codex 会丢掉写了一半的那段（没有 item/completed）重新写
                        if item["type"] == "agentMessage" {
                            st.live.text = None;
                        }
                        if st.phase != Phase::Approval {
                            st.tool = tool.map(String::from);
                            if let Some(t) = tool {
                                let detail = match t {
                                    "Edit" => super::board::changed_files(&item["changes"]),
                                    _ => activity.clone(),
                                };
                                st.doing = Some(json!({ "tool": t, "detail": detail }));
                            }
                            st.live.activity = activity;
                            if st.phase != Phase::Running {
                                Self::enter(&mut st, Phase::Running);
                                st.phase = Phase::Running;
                                st.reason = None;
                            }
                            Self::emit_status(&inst, &st);
                        }
                    }
                }
                "item/agentMessage/delta" => {
                    if std::mem::take(&mut st.codex.retrying) {
                        st.live.activity = None;
                    }
                    let text = st.live.text.get_or_insert_with(String::new);
                    text.push_str(p["delta"].as_str().unwrap_or(""));
                    if text.chars().count() > PREVIEW_LIMIT {
                        let skip = text.chars().count() - PREVIEW_LIMIT;
                        *text = text.chars().skip(skip).collect();
                    }
                    if st.codex.preview_at.is_none_or(|t| t.elapsed() >= PREVIEW_GAP) {
                        st.codex.preview_at = Some(Instant::now());
                        Self::emit_status(&inst, &st);
                    }
                }
                "item/completed" => {
                    if p["item"]["type"] == "agentMessage" {
                        st.live.text = None;
                        if let Some(r) = p["item"]["text"].as_str().and_then(super::board::first_para) {
                            st.last_reply = Some(r);
                        }
                    }
                    // 插话进了对话：从“排队中”里拿掉。只按文字对：跟读记录时可能已经拿掉了（Chat::poll），
                    // 对不上就拿最早的那条会误删另一条还在排队的
                    if p["item"]["type"] == "userMessage" {
                        let text = user_text(&p["item"]);
                        if let Some(tid) = p["threadId"].as_str().filter(|_| st.title.is_none() && !text.trim().is_empty() && !text.starts_with('/')) {
                            lock(&self.codex.title_prompts).entry(tid.to_string()).or_insert_with(|| text.clone());
                        }
                        if let Some(at) = st.codex.steered.iter().position(|q| *q == text) {
                            st.codex.steered.remove(at);
                        }
                    }
                    st.codex.retrying = false;
                    if let Some(item) = p["item"]["id"].as_str() {
                        lock(&self.codex.patches).remove(item);
                    }
                    st.tool = None;
                    st.live.activity = None;
                    Self::emit_status(&inst, &st);
                    poll = true;
                }
                "serverRequest/resolved" => {
                    // 审批在别处答了（或者这一轮被停了）
                    if st.phase == Phase::Approval {
                        Self::drop_pending(&mut st);
                        let next = if st.codex.turn.is_some() || st.codex.thread_active { Phase::Running } else { Phase::Idle };
                        Self::set_phase(&inst, &mut st, next, None);
                    }
                }
                "thread/tokenUsage/updated" => {
                    set_usage(&mut st, &p["tokenUsage"]);
                    Self::emit_status(&inst, &st);
                }
                "thread/goal/updated" => {
                    if st.codex.set_goal(seq, Some(p["goal"].clone())) {
                        Self::emit_status(&inst, &st);
                    }
                }
                "thread/goal/cleared" => {
                    if st.codex.set_goal(seq, None) {
                        Self::emit_status(&inst, &st);
                    }
                }
                "thread/name/updated" => {
                    st.title = p["threadName"].as_str().or_else(|| p["name"].as_str()).map(String::from);
                    Self::emit_status(&inst, &st);
                }
                "model/rerouted" => {
                    st.actual_model = p["toModel"].as_str().or_else(|| p["model"].as_str()).map(String::from).or(st.actual_model.take());
                    Self::emit_status(&inst, &st);
                }
                // 连不上模型服务、正在重试（“Reconnecting... 2/5”）：不算出错，运行中的活动文字换成它
                "error" if p["willRetry"] == true => {
                    if st.phase == Phase::Running {
                        st.codex.retrying = true;
                        st.live.activity = p["error"]["message"].as_str().map(|s| s.chars().take(120).collect());
                        Self::emit_status(&inst, &st);
                    }
                }
                "error" if p["willRetry"] != true => {
                    st.error = p["error"]["message"].as_str().map(|s| s.chars().take(500).collect());
                    Self::emit_status(&inst, &st);
                }
                "thread/closed" => {
                    st.codex.generation = 0;
                }
                _ => {}
            }
        }
        if let (Some(tid), Some(me)) = (pause_goal_tid, self.me.upgrade()) {
            let inst = inst.clone();
            tokio::spawn(async move {
                if let Some(server) = me.codex.server.lock().await.clone() {
                    me.codex_pause_active_goal(&inst, &server, &tid, false).await;
                }
            });
        }
        if poll {
            self.codex_fill_path(&inst).await;
            self.poll(&inst).await;
        }
    }

    /// app-server 要看看答复的请求：审批、提问挂起来等界面回答；其余不支持的答错误（Codex 按拒绝处理）。
    async fn codex_request(&self, rid: Value, method: &str, p: Value) {
        let Some(server) = self.codex.server.lock().await.clone() else { return };
        let inst = self.codex_inst_of(&p);
        let (tool, kind, input) = match method {
            "item/commandExecution/requestApproval" => {
                ("Bash", "permission", json!({ "command": p["command"], "cwd": p["cwd"], "description": p["reason"] }))
            }
            "item/fileChange/requestApproval" => {
                let changes = p["itemId"].as_str().and_then(|i| lock(&self.codex.patches).get(i).cloned()).unwrap_or(Value::Null);
                ("apply_patch", "permission", json!({ "changes": changes, "description": p["reason"], "grant_root": p["grantRoot"] }))
            }
            "item/tool/requestUserInput" => ("AskUserQuestion", "question", questions_view(&p)),
            // 要多给权限（联网、目录外读写）：和终端里一样问一次，允许 = 这一轮 / 本会话都给
            "item/permissions/requestApproval" => ("permissions", "permission", json!({ "permissions": p["permissions"], "description": p["reason"], "cwd": p["cwd"] })),
            // MCP 服务要用户填表 / 打开网址
            // （Codex 问要不要跑某个 MCP 工具也走这里：带上工具名、参数，好按权限模式替用户放）
            "mcpServer/elicitation/request" => {
                let mut view = super::elicitation_view(p["message"].as_str().unwrap_or(""), p["mode"].as_str().unwrap_or("form"), p["url"].as_str(), &p["requestedSchema"], None, None);
                if p["_meta"]["codex_approval_kind"] == "mcp_tool_call" {
                    view["mcp_call"] = json!({ "tool": p["_meta"]["tool_name"], "params": p["_meta"]["tool_params"] });
                }
                (p["serverName"].as_str().unwrap_or("MCP"), "elicitation", view)
            }
            _ => {
                let _ = server.respond_error(rid, &format!("{method} is not supported by Looklook")).await;
                return;
            }
        };
        let Some(inst) = inst else {
            let none = match (kind, tool) {
                ("question", _) => json!({ "answers": {} }),
                ("elicitation", _) => json!({ "action": "cancel" }),
                (_, "permissions") => json!({ "permissions": {} }),
                _ => json!({ "decision": "cancel" }),
            };
            let _ = server.respond(rid, none).await;
            return;
        };
        let what = approval_tool(tool, &input);
        // 助理的大脑：沙箱挡下来、来问的，先过助理的闸门（放行的不用问用户，拒绝的告诉它为什么）
        if (kind == "permission" || what == MCP_CALL) && tool != "permissions" && crate::assistant::is_gated(&self.store, &inst.id) {
            use crate::assistant::Verdict;
            let verdict = if what == MCP_CALL {
                let name = format!("mcp__{tool}__{}", input["mcp_call"]["tool"].as_str().unwrap_or_default());
                self.assistant_gate(&inst.id, &name, &input["mcp_call"]["params"])
            } else if tool == "Bash" {
                self.assistant_gate(&inst.id, "Bash", &json!({ "command": command_text(&p["command"]) }))
            } else {
                // 改文件：每个文件都要放行才放行，有一个拒绝就拒绝
                let files: Vec<String> = input["changes"].as_array().into_iter().flatten().filter_map(|c| c["path"].as_str().map(String::from)).collect();
                let all: Vec<Verdict> = if files.is_empty() { vec![self.assistant_gate(&inst.id, "Edit", &json!({}))] } else { files.iter().map(|f| self.assistant_gate(&inst.id, "Edit", &json!({ "file_path": f }))).collect() };
                all.iter().find(|v| matches!(v, Verdict::Deny(_))).cloned().or_else(|| all.iter().find(|v| **v == Verdict::Ask).cloned()).unwrap_or(Verdict::Allow)
            };
            match verdict {
                Verdict::Allow => {
                    let _ = server.respond(rid, accept_result(what, &input)).await;
                    return;
                }
                Verdict::Deny(why) => {
                    let _ = server.respond(rid, if what == MCP_CALL { json!({ "action": "decline" }) } else { json!({ "decision": "decline" }) }).await;
                    self.codex_say_later(&inst.id, why);
                    return;
                }
                Verdict::Ask | Verdict::Pass => {}
            }
        }
        // 群里提问：闸门以外的（提问、填表、多给权限）一律不给，从不让用户审批
        if crate::assistant::is_ask(&self.store, &inst.id) {
            let none = match (kind, tool) {
                ("question", _) => json!({ "answers": {} }),
                ("elicitation", _) => json!({ "action": "decline" }),
                (_, "permissions") => json!({ "permissions": {} }),
                _ => json!({ "decision": "decline" }),
            };
            let _ = server.respond(rid, none).await;
            return;
        }
        // 命令规则（docs/PLAN_2026-10-27.md §3.2）：Codex 只在它自己来问时能用上。拒绝的替用户拒绝，并告诉它为什么
        let cmd = p["command"].as_str().filter(|_| tool == "Bash").unwrap_or_default();
        let guard = match Some(cmd).filter(|c| !c.is_empty()).and_then(|c| crate::guard::check(&crate::guard::load(&self.store), c)) {
            Some(hit) if hit.action == crate::guard::Action::Deny => {
                let _ = server.respond(rid, json!({ "decision": "decline" })).await;
                self.codex_say_later(&inst.id, format!("A Looklook safety rule blocks that command ({}). Do not retry it or work around it; use another approach, or ask me to run it myself.", hit.rule));
                return;
            }
            hit => hit.map(|h| super::GuardWait::new(h, cmd)),
        };
        // 照会话现在的权限模式替用户放（助理派出去的会话那几档；一轮里换了模式也按新的，见 auto_accept）。命令规则要确认的不放
        if (kind == "permission" || what == MCP_CALL) && guard.is_none() {
            let perm = lock(&inst.st).codex.perm.clone().or_else(|| crate::assistant::errand_mode(&self.store, &inst.id)).unwrap_or_default();
            let root = self.codex_current_cwd(&inst.id).map(PathBuf::from).unwrap_or_default();
            if auto_accept(&perm, what, &input, &root) {
                let _ = server.respond(rid, accept_result(what, &input)).await;
                return;
            }
        }
        let (tx, rx) = oneshot::channel();
        let pid = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            st.pending = Some(Pending { id: pid, at: Instant::now(), tool: tool.to_string(), kind, input, suggestions: Value::Null, guard, tx: Some(tx) });
            Self::enter(&mut st, Phase::Approval);
            st.phase = Phase::Approval;
            st.reason = None;
            Self::emit_status(&inst, &st);
        }
        // 没有 TUI 可以兜底：一直等到界面回答、这一轮被停、或者 app-server 退出
        let out = rx.await.unwrap_or_default();
        let result = serde_json::from_str::<Value>(&out).unwrap_or_else(|_| match (kind, tool) {
            ("question", _) => json!({ "answers": {} }),
            ("elicitation", _) => json!({ "action": "cancel" }),
            (_, "permissions") => json!({ "permissions": {} }),
            _ => json!({ "decision": "cancel" }),
        });
        let _ = server.respond(rid, result).await;
    }

    /// 过一会儿给 Codex 插一句话（拒绝不带话）。不是 async：在 app-server 的事件处理里直接 spawn `codex_input` 会形成类型上的循环
    pub(super) fn codex_say_later(&self, id: &str, msg: String) {
        let (Some(me), id) = (self.me.upgrade(), id.to_string()) else { return };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if let Err(e) = me.codex_input(&id, &msg, &[]).await {
                tracing::debug!(error = ?e, "给 Codex 的话没送到");
            }
        });
    }

    // ---------------- 界面的操作 ----------------

    /// Tab：排到这一轮之后再发（见 [`CodexSt::later`]）；没在跑时就是普通发送
    pub(super) async fn codex_later(&self, id: &str, text: &str, images: &[String]) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        {
            let mut st = lock(&inst.st);
            if matches!(st.phase, Phase::Running | Phase::Approval) {
                if st.codex.later.len() >= 20 {
                    return Err(LocalError::invalid("text", "too_many"));
                }
                st.codex.later.push((text.to_string(), images.to_vec()));
                Self::emit_status(&inst, &st);
                return Ok(json!({ "delivered": true, "later": true }));
            }
        }
        self.codex_input(id, text, images).await
    }

    /// 取回排在这一轮之后的第 `n` 条（界面放回输入框）
    pub fn codex_later_take(&self, id: &str, n: usize) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let mut st = lock(&inst.st);
        if n >= st.codex.later.len() {
            return Err(LocalError::not_found());
        }
        let (text, images) = st.codex.later.remove(n);
        Self::emit_status(&inst, &st);
        Ok(json!({ "text": text, "images": images }))
    }

    /// `turn/start`、`review/start` 返回后：如果通知通道里这轮已经 `turn/completed`（极短一轮或立刻报错），不再把它重新设回 Running
    fn codex_apply_started_turn(inst: &Inst, st: &mut super::State, r: &Value, done_before: u64) {
        let tid = r["turn"]["id"].as_str();
        let status_done = matches!(r["turn"]["status"].as_str(), Some("completed" | "failed" | "interrupted"));
        let already_done = status_done
            || tid.is_some_and(|id| st.codex.is_turn_completed(id))
            || (tid.is_none() && st.codex.done_seq != done_before && !st.codex.thread_active);
        if already_done {
            return;
        }
        if let Some(id) = tid {
            st.codex.turn = Some(id.to_string());
        }
        st.error = None;
        if st.phase != Phase::Approval {
            Self::set_phase(inst, st, Phase::Running, None);
        }
    }

    /// 只记进对话、不起一轮（`thread/inject_items`，0.172 起有）：下一轮模型看得到。记录里是一条 `response_item` 的用户消息，
    /// 界面只显示看看自己的事件（codex_rollout.rs `injected_event`）。老版本不认、或者请求失败：返回 false，调用方改成普通发送
    pub(super) async fn codex_inject(&self, id: &str, text: &str) -> Result<bool, LocalError> {
        let inst = self.inst(id);
        let server = self.codex_attach(id).await?;
        let thread = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone());
        let thread = match thread {
            Some(t) => t,
            None => self.codex_new_thread(&server, &inst).await?,
        };
        let items = json!([{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }]);
        if let Err(e) = server.call("thread/inject_items", json!({ "threadId": thread, "items": items })).await {
            tracing::info!(id, error = ?e, "Codex 记进对话失败（可能是老版本），改成普通发送");
            return Ok(false);
        }
        self.poll(&inst).await;
        Ok(true)
    }

    pub(super) async fn codex_input(&self, id: &str, text: &str, images: &[String]) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let non_starting_goal = images.is_empty() && matches!(slash_arg(text.trim(), "/goal"), Some("" | "pause" | "clear"));
        let phase = lock(&inst.st).phase;
        if phase == Phase::Approval && !non_starting_goal {
            return Err(LocalError::new("CHAT_BUSY"));
        }
        // 换走现在的会话：不用先把它加载上（它接不上时，用户就靠这两个离开）
        let resume_to = text.trim().strip_prefix("/resume ").map(str::trim).filter(|s| !s.is_empty() && images.is_empty());
        let fresh = matches!(text.trim(), "/clear" | "/new") && images.is_empty();
        if resume_to.is_some() || fresh {
            if phase == Phase::Running {
                return Err(LocalError::new("CHAT_RUNNING"));
            }
            let thread = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone());
            // /resume <会话>：界面上选的以前的对话，在这个终端里接着（原来的留在“以前的对话”里）
            if let Some(to) = resume_to {
                if !to.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err(LocalError::invalid("resume", "invalid"));
                }
                if thread.as_deref() != Some(to) {
                    self.codex_resume(id, thread, to).await?;
                }
                return Ok(json!({ "delivered": true }));
            }
            // /clear、/new：开一个新会话（原来的留在“以前的对话”里）
            let server = self.codex_ready(id).await?;
            self.codex_forget(thread).await;
            self.codex_new_thread(&server, &inst).await?;
            return Ok(json!({ "delivered": true }));
        }
        // 不用先加载对话（还没开对话时也能先开好）
        // /fast [on|off|status]：快速模式（Codex TUI 的 /fast），下一轮起生效；不支持时说明原因
        if let Some(arg) = slash_arg(text.trim(), "/fast").filter(|_| images.is_empty()) {
            let on = match arg.to_ascii_lowercase().as_str() {
                "" => !fast_on(&lock(&inst.st).codex),
                "on" => true,
                "off" => false,
                "status" => return Ok(json!({ "delivered": true, "fast": fast_on(&lock(&inst.st).codex) })),
                _ => return Err(LocalError::invalid("value", "invalid")),
            };
            self.codex_set_fast(id, on).await?;
            return Ok(json!({ "delivered": true, "fast": on }));
        }
        // 从这里到 turn/start 返回之前算在发（空闲卸载等着）
        let _sending = Sending::new(inst.clone());
        let server = self.codex_attach(id).await?;
        let (thread, turn, phase, model, effort, current) = {
            let st = lock(&inst.st);
            let chosen = st.model_set_at.is_some();
            let current = st.model.clone().or(st.actual_model.clone());
            (st.binding.as_ref().map(|b| b.session_id.clone()), st.codex.turn.clone(), st.phase, st.model.clone().filter(|_| chosen), st.effort.clone(), current)
        };
        // 界面上选的是 Codex 的模型名、配置里对应到了服务商的模型：发服务商的（见 codex_profiles::Profile::remote）
        let (model, current) = {
            let p = crate::codex_profiles::for_instance(&self.store, id);
            (model.map(|m| p.remote(&m)), current.map(|m| p.remote(&m)))
        };
        if phase == Phase::Approval && !non_starting_goal {
            return Err(LocalError::new("CHAT_BUSY"));
        }
        let thread = match thread {
            Some(t) => t,
            None => self.codex_new_thread(&server, &inst).await?,
        };
        // `!命令`（TUI 的命令模式）：app-server 在这段对话里直接跑（不经过模型、不走沙箱），命令和输出记进对话
        if let Some(cmd) = super::bang_command(text).filter(|_| images.is_empty()) {
            if phase == Phase::Running {
                return Err(LocalError::new("CHAT_BUSY"));
            }
            server.call("thread/shellCommand", json!({ "threadId": thread, "command": cmd })).await?;
            return Ok(json!({ "delivered": true }));
        }
        lock(&inst.st).codex.prompted = Some(thread.clone());
        // 界面上的“压缩上下文”
        if text.trim() == "/compact" && images.is_empty() {
            let done_before = lock(&inst.st).codex.done_seq;
            let r = server.call("thread/compact/start", json!({ "threadId": thread })).await?;
            let mut st = lock(&inst.st);
            Self::codex_apply_started_turn(&inst, &mut st, &r, done_before);
            return Ok(json!({ "delivered": true }));
        }
        // TUI 里的 /review、/goal、/fork、/init（docs/CODEX_UI.md §20）
        let t = text.trim();
        if images.is_empty() {
            if let Some(arg) = slash_arg(t, "/review") {
                if phase == Phase::Running {
                    return Err(LocalError::new("CHAT_BUSY"));
                }
                self.checkpoint_turn(id, text, true).await;
                let done_before = lock(&inst.st).codex.done_seq;
                let r = server.call("review/start", json!({ "threadId": thread, "target": review_target(arg) })).await?;
                let mut st = lock(&inst.st);
                Self::codex_apply_started_turn(&inst, &mut st, &r, done_before);
                return Ok(json!({ "delivered": true }));
            }
            if let Some(arg) = slash_arg(t, "/goal") {
                let (seq, goal) = match arg {
                    "" => {
                        let (s, r) = server.call_seq("thread/goal/get", json!({ "threadId": thread })).await?;
                        (s, r["goal"].clone())
                    }
                    "clear" => {
                        let (s, _) = server.call_seq("thread/goal/clear", json!({ "threadId": thread })).await?;
                        (s, Value::Null)
                    }
                    "pause" | "resume" => {
                        let status = if arg == "pause" { "paused" } else { "active" };
                        let (s, r) = server.call_seq("thread/goal/set", json!({ "threadId": thread, "status": status, "origin": "user" })).await?;
                        (s, r["goal"].clone())
                    }
                    // 设了目标 Codex 自己开始做（空闲时马上开一轮）
                    objective => {
                        let (s, r) = server.call_seq("thread/goal/set", json!({ "threadId": thread, "objective": objective, "status": "active", "origin": "user" })).await?;
                        (s, r["goal"].clone())
                    }
                };
                let mut st = lock(&inst.st);
                st.codex.set_goal(seq, (!goal.is_null()).then_some(goal));
                Self::emit_status(&inst, &st);
                return Ok(json!({ "delivered": true }));
            }
            // /rename 名字
            if let Some(name) = slash_arg(t, "/rename").filter(|n| !n.is_empty()) {
                let name: String = name.chars().take(200).collect();
                server.call("thread/name/set", json!({ "threadId": thread, "name": name })).await?;
                let mut st = lock(&inst.st);
                st.title = Some(name);
                Self::emit_status(&inst, &st);
                return Ok(json!({ "delivered": true }));
            }
            // /archive（还能在“以前的对话”里找回）、/delete（彻底删掉，连子代理）：然后这里开一段新的
            if t == "/archive" || t == "/delete" {
                if phase == Phase::Running {
                    return Err(LocalError::new("CHAT_BUSY"));
                }
                let method = if t == "/archive" { "thread/archive" } else { "thread/delete" };
                server.call(method, json!({ "threadId": thread })).await?;
                self.codex_forget(Some(thread)).await;
                self.codex_new_thread(&server, &inst).await?;
                return Ok(json!({ "delivered": true }));
            }
            // /stop：停掉这段对话所有后台命令
            if t == "/stop" {
                server.call("thread/backgroundTerminals/clean", json!({ "threadId": thread })).await?;
                return Ok(json!({ "delivered": true }));
            }
            // /logout：退出这个账号，界面上出登录卡片
            if t == "/logout" {
                server.call("account/logout", json!({})).await?;
                return Ok(json!({ "delivered": true }));
            }
            // /fork：从这里分出一段新对话（原来的留在“以前的对话”里），在这个会话里接着新的
            if t == "/fork" {
                if phase == Phase::Running {
                    return Err(LocalError::new("CHAT_BUSY"));
                }
                let r = server.call("thread/fork", json!({ "threadId": thread })).await?;
                let to = r["thread"]["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| LocalError::new("CODEX_REQUEST_FAILED"))?.to_string();
                self.codex_resume(id, Some(thread), &to).await?;
                return Ok(json!({ "delivered": true }));
            }
        }
        // /init：照 TUI 送它自带的那段说明；已经有 AGENTS.md 时和 TUI 一样不做
        let init = t == "/init" && images.is_empty();
        if init {
            let dir = lock(&inst.st).codex.cwd.clone().map(PathBuf::from).or_else(|| self.store.instance(id).ok().flatten().map(|r| PathBuf::from(r.workdir)));
            if dir.is_some_and(|d| d.join("AGENTS.md").exists()) {
                return Err(LocalError::new("CODEX_AGENTS_EXISTS"));
            }
        }
        let text = if init { INIT_PROMPT } else { text };
        // 助理的大脑：记下用户原话（派活时附上；看看送进去的结果消息不算）
        let brain = crate::assistant::is_brain(&self.store, id);
        if brain {
            if phase == Phase::Running {
                crate::assistant::note_said_more(&self.store, text);
            } else {
                crate::assistant::note_said(&self.store, text);
            }
        }
        let mut input = text_input(text, images);
        // `$技能名`：和 TUI 一样另外带上技能（名字 + SKILL.md 的路径），Codex 才会按技能读进来
        if text.contains('$') {
            let skills = self.codex_skill_list(id, false).await.unwrap_or_default();
            if let Some(v) = input.as_array_mut() {
                for s in mentioned_skills(text, &skills) {
                    v.push(json!({ "type": "skill", "name": s.name, "path": s.path }));
                }
            }
        }
        // 这一轮还在跑：插话（Codex 会在下一步之前看到）
        if let (Phase::Running, Some(turn)) = (phase, turn) {
            // 先记下再发：Codex 读到它的通知可能比这次请求的答复先到
            lock(&inst.st).codex.steered.push(text.to_string());
            let r = server.call("turn/steer", json!({ "threadId": thread, "input": input, "expectedTurnId": turn })).await;
            let mut st = lock(&inst.st);
            if r.is_err() {
                if let Some(at) = st.codex.steered.iter().rposition(|q| q == text) {
                    st.codex.steered.remove(at);
                }
            }
            Self::emit_status(&inst, &st);
            drop(st);
            r?;
            return Ok(json!({ "delivered": true }));
        }
        let mut params = json!({ "threadId": thread, "input": input });
        if let Some(m) = &model {
            params["model"] = json!(m);
        }
        // 默认模式自带的说明不让模型用提问工具（要用户回答时“用一句纯文字问”），对话页就没有选择框：换成允许它用的说明。
        // collaborationMode 会盖过 model / effort，所以一起放进去；还不知道用的哪个模型时不换（照 Codex 默认的来）
        // 助理的大脑：每一轮的环境、约定、在干的活也放进这段说明（developer 消息，不进用户看到的对话，压缩后照样每轮都有）。
        // 刚开的对话（上面才 thread/start）这时才知道模型，再取一次——以前只有大脑再取，普通对话的第一轮就没带这段说明，
        // 模型照 Codex 自带的说法把问题写成文字，对话页不弹提问框
        let mut current = current.or_else(|| lock(&inst.st).actual_model.clone());
        let effort = effort.or_else(|| lock(&inst.st).effort.clone());
        // 还是不知道（终端里的 TUI 开的对话、没 resume 过）：问一下 app-server
        if model.is_none() && current.is_none() {
            if let Ok(r) = server.call("thread/read", json!({ "threadId": thread })).await {
                current = r["thread"]["model"].as_str().filter(|m| !m.is_empty()).map(String::from);
                if current.is_some() {
                    lock(&inst.st).actual_model = current.clone();
                }
            }
        }
        if let Some(m) = model.or(current) {
            let mut instructions = codex_app::DEFAULT_MODE.to_string();
            if brain {
                instructions = format!("{instructions}\n\n{}", self.assistant_context());
            }
            params["collaborationMode"] = json!({
                "mode": "default",
                "settings": { "model": m, "reasoning_effort": effort, "developer_instructions": instructions },
            });
        }
        if let Some(e) = effort {
            params["effort"] = json!(e);
        }
        // 快速模式：界面上开关过的每一轮都带上（这一轮和之后的轮次都按它；`default` = 标准速度）
        let tier = lock(&inst.st).codex.tier_set.clone();
        if let Some(tier) = &tier {
            params["serviceTier"] = json!(tier);
        }
        if brain {
            // 权限只由助理的权限模式定（沙箱挡下来的来问时过闸门）
            params["approvalPolicy"] = json!("on-request");
            params["sandboxPolicy"] = crate::assistant::codex_sandbox(crate::assistant::settings(&self.store).mode).1;
        } else if crate::assistant::is_ask(&self.store, id) {
            // 群里提问：只读沙箱，要写、要联网的来问时过闸门
            params["approvalPolicy"] = json!("on-request");
            params["sandboxPolicy"] = crate::assistant::codex_sandbox(crate::assistant::Mode::ReadOnly).1;
        } else if crate::assistant::is_errand(&self.store, id) {
            // 跑腿：用户在设置里给这种引擎选了默认权限（或手动切过）就按选的，没选时固定只读沙箱、来问时过闸门
            if let Some(perm) = lock(&inst.st).codex.perm.clone().or_else(|| crate::assistant::errand_mode(&self.store, id)) {
                let (approval, _, sandbox) = perm_params(&perm);
                params["approvalPolicy"] = approval;
                params["sandboxPolicy"] = sandbox;
            } else {
                params["approvalPolicy"] = json!("on-request");
                params["sandboxPolicy"] = crate::assistant::codex_sandbox(crate::assistant::Mode::ReadOnly).1;
            }
        } else if let Some(perm) = lock(&inst.st).codex.perm.clone().or_else(|| self.codex_saved_perm(id)) {
            let (approval, _, sandbox) = perm_params(&perm);
            params["approvalPolicy"] = approval;
            params["sandboxPolicy"] = sandbox;
        }
        self.checkpoint_turn(id, text, true).await;
        let done_before = lock(&inst.st).codex.done_seq;
        let r = server.call("turn/start", params).await?;
        let mut st = lock(&inst.st);
        Self::codex_apply_started_turn(&inst, &mut st, &r, done_before);
        Ok(json!({ "delivered": true }))
    }

    // ---------------- 托管工作树（见 codex_worktree.rs） ----------------

    /// 现在这段对话所在的目录（还没有对话时是终端的工作目录）
    pub(super) fn codex_current_cwd(&self, id: &str) -> Result<String, LocalError> {
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let inst = self.inst(id);
        let st = lock(&inst.st);
        Ok(st.codex.cwd.clone().or_else(|| st.binding.as_ref().map(|b| self.codex_thread_cwd(&b.session_id, &row.workdir))).unwrap_or(row.workdir))
    }

    /// 这个对话能不能用托管工作树（目录在 Git 仓库里），以及这个仓库已有的托管工作树。
    pub(super) async fn codex_worktrees(&self, id: &str) -> Result<Value, LocalError> {
        let cwd = PathBuf::from(self.codex_current_cwd(id)?);
        let root = super::codex_worktree::managed_root(&crate::agents::codex_home());
        let current = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone());
        tokio::task::spawn_blocking(move || {
            let Some(repo) = super::codex_worktree::repo_root(&cwd) else {
                return Ok(json!({ "available": false, "items": [] }));
            };
            let items = super::codex_worktree::list(&root, &cwd)?;
            let items: Vec<Value> = items
                .into_iter()
                .map(|c| {
                    let here = cwd.starts_with(&c.root) || current.is_some() && c.owner == current;
                    json!({ "root": c.root, "cwd": c.cwd, "owner": c.owner, "head": c.head, "current": here })
                })
                .collect();
            Ok(json!({ "available": true, "repo": repo, "root": root, "items": items }))
        })
        .await
        .map_err(|e| LocalError::from(anyhow::anyhow!(e)))?
    }

    /// 对话换到托管工作树里：
    /// - `new`：新建工作树，在里面开一段新对话（TUI 的 `/new` → “新建 Git 工作树”）；
    /// - `fork`：新建工作树，把现在这段对话复制过去接着（`/fork` → “新建 Git 工作树”、`/worktree` → “接着现在的对话”）；
    /// - `open`：接着已有工作树里的那段对话（`/worktree` → “浏览工作树”）。
    pub(super) async fn codex_worktree(&self, id: &str, mode: &str, root: Option<&str>) -> Result<Value, LocalError> {
        let inst = self.inst(id);
        let (phase, thread) = {
            let st = lock(&inst.st);
            (st.phase, st.binding.as_ref().map(|b| b.session_id.clone()))
        };
        if matches!(phase, Phase::Running | Phase::Approval) {
            return Err(LocalError::new("CHAT_RUNNING"));
        }
        let codex_home = crate::agents::codex_home();
        let managed = super::codex_worktree::managed_root(&codex_home);
        if mode == "open" {
            let root = PathBuf::from(root.ok_or_else(|| LocalError::invalid("root", "required"))?);
            let cwd = PathBuf::from(self.codex_current_cwd(id)?);
            let found = tokio::task::spawn_blocking(move || super::codex_worktree::list(&managed, &cwd))
                .await
                .map_err(|e| LocalError::from(anyhow::anyhow!(e)))??
                .into_iter()
                .find(|c| c.root == root)
                .ok_or_else(LocalError::not_found)?;
            let owner = found.owner.clone().ok_or_else(|| LocalError::new("CODEX_WORKTREE_NO_OWNER"))?;
            self.store.set(&thread_cwd_key(&owner), &found.cwd.to_string_lossy().to_string())?;
            if thread.as_deref() != Some(owner.as_str()) {
                self.codex_resume(id, thread, &owner).await?;
            }
            return Ok(json!({ "cwd": found.cwd, "thread": owner }));
        }
        if mode == "fork" && thread.is_none() {
            return Err(LocalError::new("CODEX_WORKTREE_NO_THREAD"));
        }
        if mode != "new" && mode != "fork" {
            return Err(LocalError::invalid("mode", "unknown"));
        }
        let server = self.codex_ready(id).await?;
        let source = PathBuf::from(self.codex_current_cwd(id)?);
        let checkout = tokio::task::spawn_blocking(move || super::codex_worktree::create(&managed, &source)).await.map_err(|e| LocalError::from(anyhow::anyhow!(e)))??;
        let cwd = checkout.cwd.to_string_lossy().to_string();
        let started: Result<String, LocalError> = async {
            if mode == "new" {
                self.codex_forget(thread).await;
                self.codex_new_thread_in(&server, &inst, &cwd).await
            } else {
                self.codex_trust(&server, &cwd).await;
                let mut params = json!({ "threadId": thread, "cwd": cwd });
                merge(&mut params, self.codex_thread_opts(id));
                let r = server.call("thread/fork", params).await?;
                let tid = r["thread"]["id"].as_str().ok_or_else(|| LocalError::new("CODEX_REQUEST_FAILED"))?.to_string();
                self.codex_forget(thread).await;
                self.codex_loaded(&inst, &r, self.codex.generation.load(Ordering::SeqCst), None);
                let _ = inst.tx.send(super::Event::Reset);
                Ok(tid)
            }
        }
        .await;
        let tid = match started {
            Ok(t) => t,
            Err(e) => {
                // 和 TUI 一样留着建好的工作树，告诉用户在哪
                return Err(e.with("worktree", checkout.root.to_string_lossy().to_string()));
            }
        };
        self.store.set(&thread_cwd_key(&tid), &cwd)?;
        let root = checkout.root.clone();
        let t2 = tid.clone();
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || super::codex_worktree::bind(&root, &t2)).await {
            tracing::warn!(error = ?e, "没能记下托管工作树属于哪段对话");
        }
        Ok(json!({ "cwd": cwd, "root": checkout.root, "thread": tid }))
    }

    /// 编辑重发（[`Chat::edit`]）：`thread/fork` 复制到上一轮 `after` 为止（含），换到新的 thread 上；
    /// `after` 为空（改的是第一轮）开一段新对话。原来那段对话不动，还在历史里。工作目录照旧（托管工作树里的接着在工作树里）。
    pub(super) async fn codex_edit(&self, id: &str, after: Option<String>) -> Result<(), LocalError> {
        let inst = self.inst(id);
        let thread = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone());
        let server = self.codex_ready(id).await?;
        let cwd = self.codex_current_cwd(id)?;
        let tid = match after {
            None => {
                self.codex_forget(thread).await;
                self.codex_new_thread_in(&server, &inst, &cwd).await?
            }
            Some(turn) => {
                let thread = thread.ok_or_else(|| LocalError::new("CHAT_EDIT_STALE"))?;
                let mut params = json!({ "threadId": thread, "cwd": cwd, "lastTurnId": turn });
                merge(&mut params, self.codex_thread_opts(id));
                let r = server.call("thread/fork", params).await?;
                let tid = r["thread"]["id"].as_str().ok_or_else(|| LocalError::new("CODEX_REQUEST_FAILED"))?.to_string();
                self.codex_forget(Some(thread)).await;
                self.codex_loaded(&inst, &r, self.codex.generation.load(Ordering::SeqCst), None);
                let _ = inst.tx.send(super::Event::Reset);
                tid
            }
        };
        self.store.set(&thread_cwd_key(&tid), &cwd)?;
        Ok(())
    }

    pub(super) async fn codex_interrupt(&self, id: &str) -> Result<(), LocalError> {
        let inst = self.inst(id);
        // 没有在跑的一轮、也没有进行中的 /goal：不用去加载会话（会话接不上时“停止”也不报错）
        let idle = {
            let st = lock(&inst.st);
            st.codex.turn.is_none() && matches!(st.phase, Phase::Idle | Phase::Detached) && !goal_active(&st.codex)
        };
        if idle {
            return Ok(());
        }
        let server = self.codex_attach(id).await?;
        let (thread, mut turn, busy) = {
            let st = lock(&inst.st);
            (
                st.binding.as_ref().map(|b| b.session_id.clone()),
                st.codex.turn.clone(),
                matches!(st.phase, Phase::Running | Phase::Approval),
            )
        };
        let Some(thread) = thread else { return Ok(()) };
        // 先把进行中的 /goal 暂停，再停当前这轮：避免 turn/interrupt 刚发出前这一轮恰好自然结束、app-server 抢先拉起下一轮自动续跑
        self.codex_pause_active_goal(&inst, &server, &thread, true).await;
        // 半途接上的会话（resume 时已在跑）可能还没记下 turn id：向 app-server 查当前在跑的那一轮
        if turn.is_none() && busy {
            turn = codex_active_turn_id(&server, &thread).await;
        }
        if let Some(turn) = turn {
            if let Err(e) = server.call("turn/interrupt", json!({ "threadId": thread, "turnId": turn })).await {
                // 刚好在发请求前这一轮已经跑完了：不算错
                let still_running = {
                    let st = lock(&inst.st);
                    st.codex.turn.is_some() && st.phase != Phase::Idle
                };
                if still_running {
                    return Err(e);
                }
            }
        }
        {
            let mut st = lock(&inst.st);
            Self::drop_pending(&mut st);
            Self::stop_tool(&inst, &mut st);
        }
        Ok(())
    }

    /// 快速模式现在能不能开：能开返回这个模型的快速档 id，不能开返回原因——
    /// `provider`（用的第三方接口：速度档是 OpenAI 的服务等级，第三方不认）、`login`（不是 ChatGPT 账号登录：
    /// 官方只给 ChatGPT 登录开放 Codex 的快速模式，API key 按 API 价格计费）、`model`（这个模型的目录里没有快速档）
    pub(super) async fn codex_fast_tier(&self, id: &str) -> Result<String, &'static str> {
        let profile = crate::codex_profiles::for_instance(&self.store, id);
        if profile.base_url.is_some() && crate::codex_profiles::key_set(&self.paths.home, &profile.id) {
            return Err("provider");
        }
        let cached = lock(&self.codex.account).as_ref().filter(|(t, _)| t.elapsed() < ACCOUNT_TTL).map(|(_, v)| v.clone());
        let account = match cached {
            Some(v) => v,
            None => {
                let server = self.codex_server().await.map_err(|_| "login")?;
                let v = server.call("account/read", json!({})).await.unwrap_or(Value::Null);
                if !v.is_null() {
                    *lock(&self.codex.account) = Some((Instant::now(), v.clone()));
                }
                v
            }
        };
        // config.toml 里自己配的模型服务（不要 OpenAI 登录）：也是第三方
        if account["account"].is_null() && account["requiresOpenaiAuth"] == false {
            return Err("provider");
        }
        if account["account"]["type"].as_str().map(|t| t.eq_ignore_ascii_case("chatgpt")) != Some(true) {
            return Err("login");
        }
        let (options, _) = self.codex_profile_models(&profile).await.map_err(|_| "model")?;
        let current = {
            let inst = self.inst(id);
            let st = lock(&inst.st);
            st.model.clone().or(st.actual_model.clone())
        };
        let list = options.as_array().cloned().unwrap_or_default();
        let opt = match current {
            Some(m) => list.iter().find(|o| o["value"].as_str() == Some(m.as_str())),
            None => list.iter().find(|o| o["default"] == true),
        };
        opt.and_then(|o| o["fast_tier"].as_str()).map(String::from).ok_or("model")
    }

    /// 开、关快速模式：记下来，下一轮 `turn/start` 起带上（之后的轮次都沿用）。开的时候先查能不能开
    pub(super) async fn codex_set_fast(&self, id: &str, on: bool) -> Result<(), LocalError> {
        let tier = if on {
            self.codex_fast_tier(id).await.map_err(|why| LocalError::new("CODEX_FAST_UNAVAILABLE").with("reason", json!(why)))?
        } else {
            "default".to_string()
        };
        let inst = self.inst(id);
        let mut st = lock(&inst.st);
        st.codex.tier_set = Some(tier);
        Self::emit_status(&inst, &st);
        Ok(())
    }

    /// 模型、思考强度：记下来，下一轮 `turn/start` 带上（之后的轮次都沿用）
    pub(super) fn codex_setting(&self, id: &str, kind: &str, value: &str) -> Result<(), LocalError> {
        // 换权限模式时重判等着的改文件审批要用（先拿：里面要锁 st）
        let root = if kind == "mode" { self.codex_current_cwd(id).map(PathBuf::from).unwrap_or_default() } else { PathBuf::new() };
        let inst = self.inst(id);
        let mut st = lock(&inst.st);
        match kind {
            "model" => {
                if value.is_empty() || value.len() > 64 || !value.chars().all(|c| c.is_ascii_alphanumeric() || "._-[]:/".contains(c)) {
                    return Err(LocalError::invalid("value", "invalid"));
                }
                st.model = Some(value.to_string());
                st.model_set_at = Some(st.model_msg.clone());
            }
            "effort" => {
                if !EFFORTS.contains(&value) {
                    return Err(LocalError::invalid("value", "invalid"));
                }
                st.effort = Some(value.to_string());
                // 强度跟着模型一起带上：没选过模型时用会话当前的模型
                if st.model_set_at.is_none() {
                    st.model_set_at = Some(st.model_msg.clone());
                }
            }
            // 权限：下一轮开始时生效（正在跑的这一轮不变）
            "mode" => {
                if !PERMS.contains(&value) {
                    return Err(LocalError::invalid("value", "invalid"));
                }
                // 助理的大脑、群里提问：权限由助理固定；跑腿允许用户在会话里手动切换
                if inst.brain.load(Ordering::Relaxed) || crate::assistant::is_brain(&self.store, id) || crate::assistant::is_ask(&self.store, id) {
                    return Err(LocalError::new("CHAT_MODE_UNAVAILABLE"));
                }
                if crate::assistant::is_errand(&self.store, id) {
                    crate::assistant::set_errand_mode(&self.store, id, value);
                } else {
                    let _ = self.store.set(&perm_key(id), &value.to_string());
                }
                st.codex.perm = Some(value.to_string());
                st.mode = Some(value.to_string());
                // 正在等用户点的审批：新模式下该放的马上放（这一轮的审批策略换不了，之后来问的也按新模式放，见 auto_accept）
                let fits = st.pending.as_ref().is_some_and(|p| {
                    p.guard.is_none() && (p.kind == "permission" || approval_tool(&p.tool, &p.input) == MCP_CALL) && auto_accept(value, approval_tool(&p.tool, &p.input), &p.input, &root)
                });
                if fits {
                    if let Some(p) = st.pending.as_mut() {
                        tracing::info!(id, mode = value, "换了权限模式：等着的审批照新模式放行");
                        p.resolve(accept_result(approval_tool(&p.tool, &p.input), &p.input).to_string());
                    }
                    Self::settle(&inst, &mut st, Phase::Running);
                }
            }
            _ => return Err(LocalError::invalid("kind", "invalid")),
        }
        Self::emit_status(&inst, &st);
        Ok(())
    }

    /// 这段对话工作目录下能用的技能（app-server `skills/list`，缓存 [`SKILLS_TTL`]）
    pub(super) async fn codex_skill_list(&self, id: &str, fresh: bool) -> Result<Vec<Skill>, LocalError> {
        let cwd = self.codex_current_cwd(id)?;
        self.codex_skills_at(cwd, fresh).await
    }

    /// 一个目录下 Codex 认得的技能（草稿页还没有会话时也能列）
    pub(super) async fn codex_skills_at(&self, cwd: String, fresh: bool) -> Result<Vec<Skill>, LocalError> {
        if !fresh {
            if let Some((at, v)) = lock(&self.codex.skills).get(&cwd) {
                if at.elapsed() < SKILLS_TTL {
                    return Ok(v.clone());
                }
            }
        }
        let server = self.codex_server().await?;
        let r = server.call("skills/list", json!({ "cwds": [cwd], "forceReload": fresh })).await?;
        let list = parse_skills(&r);
        let mut m = lock(&self.codex.skills);
        m.retain(|_, (at, _)| at.elapsed() < SKILLS_TTL);
        m.insert(cwd, (Instant::now(), list.clone()));
        Ok(list)
    }

    /// 输入框打 `$` 时提示的技能：名字写成 `$名字`
    pub(super) async fn codex_commands(&self, id: &str) -> Result<Value, LocalError> {
        let list = self.codex_skill_list(id, true).await?;
        Ok(skill_items(&list))
    }

    pub(super) async fn codex_commands_at(&self, cwd: String) -> Result<Value, LocalError> {
        Ok(skill_items(&self.codex_skills_at(cwd, false).await?))
    }

    pub(super) async fn codex_models(&self, id: &str) -> Result<Value, LocalError> {
        let profile = crate::codex_profiles::for_instance(&self.store, id);
        let (options, provider) = self.codex_profile_models(&profile).await?;
        // 快速模式能不能开（设置栏的开关不能开时点了说原因）
        let fast = match self.codex_fast_tier(id).await {
            Ok(tier) => json!({ "tier": tier }),
            Err(why) => json!({ "why": why }),
        };
        let inst = self.inst(id);
        let st = lock(&inst.st);
        Ok(json!({ "options": options, "provider": provider, "model": st.model, "actual_model": st.actual_model, "effort": st.effort, "fast": fast }))
    }

    /// 这个配置能选的模型和界面显示的服务商（本机默认时 null）
    pub(super) async fn codex_profile_models(&self, profile: &crate::codex_profiles::Profile) -> Result<(Value, Value), LocalError> {
        use crate::codex_profiles as xp;
        // 第三方接口：列服务商自己的模型（`model/list` 是 OpenAI 的目录，第三方一般不认这些名字）。
        // 服务商列不出来时只列指定的模型；都没有时照常用目录（由接口自己对应 OpenAI 的模型名）
        let third = profile.base_url.clone().filter(|_| xp::key_set(&self.paths.home, &profile.id));
        let mut ours = vec![];
        if let Some(base) = &third {
            let key = std::fs::read_to_string(xp::key_path(&self.paths.home, &profile.id)).unwrap_or_default();
            let ids = super::models::provider_models(&format!("{}/models", base.trim_end_matches('/')), key.trim()).await;
            // 对应过的 Codex 模型：照 Codex 目录里的名字、强度显示，实际发服务商的（`model` 填服务商的名字，回复里的模型对得上）
            let catalog = if profile.models.is_empty() { Value::Null } else { self.codex_catalog().await.unwrap_or(Value::Null) };
            ours = mapped_options(profile, &catalog);
            let taken: Vec<String> = ours.iter().filter_map(|o| o["model"].as_str().map(String::from)).collect();
            let chosen = profile.model.as_deref().filter(|m| !profile.models.contains_key(*m));
            for o in provider_model_options(chosen, &ids) {
                if !o["model"].as_str().is_some_and(|m| taken.iter().any(|t| t == m)) {
                    ours.push(o);
                }
            }
        }
        let options = if ours.is_empty() { self.codex_catalog().await? } else { json!(ours) };
        let provider = third.map(|_| json!({ "name": profile.name })).unwrap_or(Value::Null);
        Ok((options, provider))
    }

    /// 不属于哪个会话的 Codex 模型目录（宠物生成台词选模型用）：共用的 app-server 的 `model/list`
    pub(super) async fn codex_catalog(&self) -> Result<Value, LocalError> {
        if let Some(v) = lock(&self.codex.models).as_ref().filter(|(t, _)| t.elapsed() < MODELS_TTL).map(|(_, v)| v.clone()) {
            return Ok(v);
        }
        let server = self.codex_server().await?;
        let r = server.call("model/list", json!({})).await?;
        let v = json!(model_options(&r));
        *lock(&self.codex.models) = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// 用一次重置机会（app-server 的 `account/rateLimitResetCredit/consume`，和 Codex 里 /usage 的“重置”一样）。
    /// `credit` 为 None 时由后端挑下一张。返回 `{ result: reset|nothingToReset|noCredit|alreadyRedeemed }`
    pub(super) async fn codex_use_reset(&self, credit: Option<&str>) -> Result<Value, LocalError> {
        let server = self.codex_server().await?;
        let r = server.call("account/rateLimitResetCredit/consume", json!({ "idempotencyKey": super::claude_sdk::uuid_v4(), "creditId": credit })).await;
        // 不管成没成，限额都重新取
        *lock(&self.codex.limits) = None;
        Ok(json!({ "result": r?["outcome"] }))
    }

    /// 账号用量（5 小时、每周的限额）和这个会话的上下文、token
    pub(super) async fn codex_usage(&self, id: &str) -> Result<Value, LocalError> {
        let cached = lock(&self.codex.limits).as_ref().filter(|(t, _)| t.elapsed() < LIMITS_TTL).map(|(_, v)| v.clone());
        let limits = match cached {
            Some(v) => v,
            None => {
                let server = self.codex_server().await?;
                // 用第三方接口、没登录 ChatGPT 时没有限额
                let v = match server.call("account/rateLimits/read", Value::Null).await {
                    Ok(r) => limits_view(&r),
                    Err(_) => Value::Null,
                };
                let account = server.call("account/read", json!({})).await.ok();
                let mut v = v;
                if let (Some(a), Some(o)) = (account, v.as_object_mut()) {
                    o.insert("account".into(), json!({ "type": a["account"]["type"], "plan": a["account"]["planType"], "email": a["account"]["email"] }));
                }
                *lock(&self.codex.limits) = Some((Instant::now(), v.clone()));
                v
            }
        };
        let inst = self.inst(id);
        let profile = crate::codex_profiles::for_instance(&self.store, id);
        let compact = codex_compact(&crate::agents::codex_home(), &profile);
        let st = lock(&inst.st);
        let model = st.actual_model.clone().or(st.model.clone());
        // Codex 自己报的（每轮的 token_count、接着以前的对话时从记录里读到的）最准；新对话第一轮之前按它的算法估
        let (window, window_source) = match st.codex.window {
            Some(w) => (Some(w), Some(super::models::WindowSource::Reported)),
            None => match model.as_deref().and_then(|m| codex_window_guess(&crate::agents::codex_home(), &profile, m)) {
                Some((w, s)) => (Some(w), Some(s)),
                None => (None, None),
            },
        };
        let t = st.codex.totals.clone().unwrap_or(Value::Null);
        let totals = json!({
            "input": t["inputTokens"].as_u64().unwrap_or(0).saturating_sub(t["cachedInputTokens"].as_u64().unwrap_or(0)),
            "cache_read": t["cachedInputTokens"].as_u64().unwrap_or(0),
            "output": t["outputTokens"].as_u64().unwrap_or(0),
        });
        Ok(json!({ "model": model, "context": st.context, "window": window, "window_source": window_source, "compact": compact, "totals": totals, "limits": limits, "version": st.version, "installed": crate::agent_update::installed_version("codex") }))
    }

    /// 这段对话里 Codex 生成的图片（`$CODEX_HOME/generated_images/<会话>/`），最新的在前。
    /// 界面用 `/api/instances/<终端>/chat/file?name=<文件名>` 取（完整内容，可以直接放进 `<img>`、复制链接、下载）。
    pub(super) fn codex_files(&self, id: &str) -> Value {
        let inst = self.inst(id);
        let Some(thread) = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone()) else { return json!({ "items": [] }) };
        if !crate::history::valid_id(&thread) {
            return json!({ "items": [] });
        }
        let dir = crate::agents::codex_home().join("generated_images").join(&thread);
        let mut files: Vec<(std::time::SystemTime, Value)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let ext = p.extension()?.to_str()?.to_ascii_lowercase();
                if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
                    return None;
                }
                let m = e.metadata().ok().filter(|m| m.is_file())?;
                let t = m.modified().ok()?;
                let ms = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                Some((t, json!({ "kind": "image", "path": p.display().to_string(), "name": p.file_name()?.to_string_lossy(), "size": m.len(), "mtime_ms": ms })))
            })
            .collect();
        files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        json!({ "items": files.into_iter().map(|(_, v)| v).take(200).collect::<Vec<_>>() })
    }

    /// 这段对话生成的某张图片的完整路径：只认 `generated_images/<会话>/` 下、名字里没有路径分隔符的图片文件
    pub fn codex_file_path(&self, id: &str, name: &str) -> Option<PathBuf> {
        let ok_name = !name.is_empty() && name.len() <= 200 && !name.contains(['/', '\\', ':']) && !name.starts_with('.');
        let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
        if !ok_name || !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
            return None;
        }
        let thread = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone())?;
        if !crate::history::valid_id(&thread) {
            return None;
        }
        let p = crate::agents::codex_home().join("generated_images").join(thread).join(name);
        p.is_file().then_some(p)
    }

    /// 这段对话产出的文件（[`codex_artifacts`](super::codex_artifacts)），最近提到的在前
    async fn codex_artifact_list(&self, id: &str) -> Vec<(PathBuf, Vec<String>)> {
        let inst = self.inst(id);
        self.poll(&inst).await;
        let workdir = self.store.instance(id).ok().flatten().map(|r| PathBuf::from(r.workdir)).unwrap_or_default();
        let mut texts: Vec<String> = vec![];
        {
            let tr = inst.tr.lock().await;
            let Some(t) = tr.as_ref().map(super::Reader::get) else { return vec![] };
            for it in t.items(0, t.len()).iter().rev() {
                if let Ok(v) = serde_json::to_value(it) {
                    super::codex_artifacts::strings(&v, &mut texts);
                }
            }
        }
        let mut skip = super::codex_artifacts::system_dirs();
        skip.push(crate::agents::codex_home());
        let found = tokio::task::block_in_place(|| super::codex_artifacts::find(&texts, &workdir, &skip));
        lock(&self.codex.artifacts).insert(id.to_string(), (Instant::now(), found.iter().map(|(p, _)| p.clone()).collect()));
        found
    }

    pub(super) async fn codex_artifacts(&self, id: &str) -> Value {
        let found = self.codex_artifact_list(id).await;
        json!({ "items": found.iter().map(|(p, m)| super::codex_artifacts::view(p, m)).collect::<Vec<_>>() })
    }

    /// 取某个产出的文件：只认这段对话里出现过的（一分钟内算过就用算过的，否则重算一次）
    pub(super) async fn codex_artifact_path(&self, id: &str, path: &str) -> Option<PathBuf> {
        let want = super::codex_artifacts::normalize(Path::new(path));
        let known = lock(&self.codex.artifacts).get(id).filter(|(t, _)| t.elapsed() < super::codex_artifacts::TTL).is_some_and(|(_, v)| v.contains(&want));
        let ok = known || self.codex_artifact_list(id).await.iter().any(|(p, _)| *p == want);
        (ok && want.is_file()).then_some(want)
    }

    /// 从“以前的对话”接着、或者迁移时：直接记下要接着的会话（记录文件路径在加载时由 app-server 给）
    pub fn codex_bind(&self, id: &str, thread: &str) {
        // 记录文件找得到就记上
        // 压缩过的（`.jsonl.zst`）不记：接上时 Codex 自己解压，app-server 会给解压后的路径
        let path = tokio::task::block_in_place(|| crate::history::codex_rollout(thread)).filter(|p| p.extension().is_none_or(|e| e != "zst")).unwrap_or_default();
        let b = Binding { session_id: thread.to_string(), transcript_path: path };
        let _ = self.store.set(&super::binding_key(id), &b);
        if let Some(inst) = lock(&self.insts).get(id).cloned() {
            let mut st = lock(&inst.st);
            if st.binding.as_ref().is_none_or(|old| old.session_id != thread) {
                Self::drop_pending(&mut st);
                st.codex.reset_thread();
            }
            st.binding = Some(b);
            st.codex.generation = 0;
        }
    }

    /// 新建的对话模式 Codex 终端：接着以前的对话（复制一份时用 `thread/fork`）、送第一句话。
    pub async fn codex_first(&self, id: &str, prompt: Option<String>, resume: Option<crate::instances::Resume>) {
        let r: Result<(), LocalError> = async {
            if let Some(r) = resume {
                if r.fork {
                    let server = self.codex_server().await?;
                    let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
                    let mut params = json!({ "threadId": r.id, "cwd": row.workdir });
                    merge(&mut params, self.codex_thread_opts(id));
                    let res = server.call("thread/fork", params).await?;
                    self.codex_loaded(&self.inst(id), &res, self.codex.generation.load(Ordering::SeqCst), None);
                } else {
                    self.codex_bind(id, &r.id);
                }
            }
            if let Some(p) = prompt {
                self.record_input(id, &p);
                self.codex_input(id, &p, &[]).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(id, error = ?e, "Codex 对话没能开始");
            let inst = self.inst(id);
            let mut st = lock(&inst.st);
            st.error = Some(format!("{e:?}").chars().take(500).collect());
            Self::emit_status(&inst, &st);
        }
    }

    /// 换了配置（服务商、Key、登录）：放下这段对话，下次用到时按现在的配置重新接上（`thread/resume` 带新的参数）
    pub(super) async fn codex_reload(&self, id: &str) {
        *lock(&self.codex.account) = None;
        let inst = self.inst(id);
        let tid = {
            let mut st = lock(&inst.st);
            let Some(t) = st.binding.as_ref().map(|b| b.session_id.clone()) else { return };
            st.codex.generation = 0;
            t
        };
        lock(&self.codex.threads).remove(&tid);
        if let Some(server) = self.codex.server.lock().await.clone() {
            if let Err(e) = server.call("thread/unsubscribe", json!({ "threadId": tid })).await {
                tracing::debug!(thread = %tid, error = ?e, "退订 Codex 对话失败");
            }
        }
    }

    /// 会话停掉：这段对话从 app-server 里卸载（app-server 只在没人订阅、也没在跑时才卸载，所以先等这一轮停下，最多几秒）。
    /// 绑定留着，用到时 codex_attach 重新 resume。
    pub(super) async fn codex_release(&self, inst: &Arc<Inst>) {
        let tid = {
            let st = lock(&inst.st);
            let Some(t) = st.binding.as_ref().map(|b| b.session_id.clone()) else { return };
            t
        };
        if lock(&self.codex.threads).get(&tid) != Some(&inst.id) {
            return;
        }
        for _ in 0..50 {
            if lock(&inst.st).codex.turn.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        {
            let mut st = lock(&inst.st);
            Self::stop_tool(inst, &mut st);
            st.codex.generation = 0;
        }
        lock(&self.codex.threads).remove(&tid);
        let Some(server) = self.codex.server.lock().await.clone() else { return };
        match server.call("thread/unsubscribe", json!({ "threadId": tid })).await {
            Ok(r) => tracing::info!(id = %inst.id, thread = %tid, status = %r["status"], "停会话：Codex 对话已卸载"),
            Err(e) => tracing::warn!(thread = %tid, error = ?e, "停会话：退订 Codex 对话失败"),
        }
    }

    pub(super) async fn codex_forget(&self, thread: Option<String>) {
        let Some(t) = thread else { return };
        lock(&self.codex.threads).remove(&t);
        if let Some(server) = self.codex.server.lock().await.clone() {
            let _ = server.call("thread/unsubscribe", json!({ "threadId": t })).await;
        }
    }

    // ---------------- 启动前 ----------------

    /// 对话模式的 Codex 终端启动时：先起好 app-server、信任好工作目录，第一句话不用等。起不来时原因记在日志里，对话页显示连不上。
    pub async fn codex_warm(&self, workdir: &str) {
        match self.codex_server().await {
            Ok(server) => self.codex_trust(&server, workdir).await,
            Err(e) => tracing::warn!(error = ?e, "Codex 的后台服务起不来"),
        }
    }


    /// Codex 的“信任这个文件夹？”用户不用管（和 Claude Code 一样，见 `claude_profiles::skip_first_run`）：
    /// 没信任的目录里项目自己的 `.codex` 配置、hooks 不生效，用户在终端里自己开 Codex 时还会停下来问。
    /// 在 app-server 里写好用户选“Trust and continue”时 TUI 自己写的同一个值：
    /// `projects."<工作目录>".trust_level = "trusted"`（用户的 config.toml）。对话页新开的会话也是同一个目录，一起算上。
    /// 已经有 trust_level 的不动（包括用户自己选的 `untrusted`）；读写失败只记日志。
    async fn codex_trust(&self, server: &AppServer, workdir: &str) {
        if workdir.is_empty() || lock(&self.codex.trusted).contains(workdir) {
            return;
        }
        let r: Result<(), LocalError> = async {
            let cfg = server.call("config/read", json!({ "includeLayers": false, "cwd": workdir })).await?;
            if !has_trust_level(&cfg["config"]["projects"], workdir) {
                let edit = json!({ "keyPath": trust_key_path(workdir), "value": "trusted", "mergeStrategy": "replace" });
                server.call("config/batchWrite", json!({ "edits": [edit], "reloadUserConfig": true })).await?;
                tracing::info!(workdir, "已在 Codex 配置里信任工作目录");
            }
            Ok(())
        }
        .await;
        match r {
            Ok(()) => {
                lock(&self.codex.trusted).insert(workdir.to_string());
            }
            Err(e) => tracing::warn!(workdir, error = ?e, "没能在 Codex 配置里信任工作目录，终端里会再问一次"),
        }
    }

    /// 起名用的临时对话发来的通知：记下回答、答完通知等着的那边。是它的就返回 true（不再往下分给终端）。
    fn codex_title_note(&self, method: &str, p: &Value) -> bool {
        let Some(tid) = p["threadId"].as_str() else { return false };
        let mut titling = lock(&self.codex.titling);
        let Some((text, done)) = titling.get_mut(tid) else { return false };
        match method {
            "item/completed" if p["item"]["type"] == "agentMessage" => *text = p["item"]["text"].as_str().unwrap_or_default().to_string(),
            "turn/completed" => {
                // 这一轮出错了、没有回答：带上原因（开头 \u{1} 标出是错误，旁聊显示出来；起名时解析不出标题就当没起）
                if text.is_empty() {
                    if let Some(e) = p["turn"]["error"]["message"].as_str().filter(|e| !e.is_empty()) {
                        *text = format!("\u{1}{e}");
                    }
                }
                if let Some(tx) = done.take() {
                    let _ = tx.send(std::mem::take(text));
                }
            }
            _ => {}
        }
        true
    }

    /// 旁聊（TUI / 桌面端的 /side）：第一次问时把这段对话复制成一个临时对话（ephemeral、只读、不审批），之后的问题
    /// 接着在这个临时对话里问（能追问），原对话不受影响、正在跑也不打断；关掉旁聊（[`Chat::codex_side_close`]）才丢掉。
    /// 事件借用起名的通道（[`Chat::codex_title_note`]）收
    pub(super) async fn codex_side(&self, id: &str, question: &str) -> Result<Value, LocalError> {
        let server = self.codex_ready(id).await?;
        let thread = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone()).ok_or_else(|| LocalError::new("CHAT_NOTHING_TO_FORK"))?;
        // 还开着、分叉自同一个对话、还是这个 app-server 的：接着问
        let kept = lock(&self.codex.sides)
            .get(id)
            .filter(|(parent, _, srv)| *parent == thread && srv.upgrade().is_some_and(|s| Arc::ptr_eq(&s, &server)))
            .map(|(_, tmp, _)| tmp.clone());
        let (tmp, first) = match kept {
            Some(t) => (t, false),
            None => {
                self.codex_side_close(id).await;
                let r = server.call("thread/fork", json!({ "threadId": thread, "ephemeral": true, "excludeTurns": true, "sandbox": "read-only", "approvalPolicy": "never" })).await?;
                let tmp = r["thread"]["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| LocalError::new("CODEX_REQUEST_FAILED"))?.to_string();
                lock(&self.codex.sides).insert(id.to_string(), (thread.clone(), tmp.clone(), Arc::downgrade(&server)));
                (tmp, true)
            }
        };
        let (tx, rx) = oneshot::channel();
        lock(&self.codex.titling).insert(tmp.clone(), (String::new(), Some(tx)));
        let prompt = if first {
            format!("<side_question>\nThis is a side chat branched from the conversation above. Answer from the conversation so far; keep it focused. Do not change files.\n\n{question}\n</side_question>")
        } else {
            question.to_string()
        };
        let started = server.call("turn/start", json!({ "threadId": tmp, "input": text_input(&prompt, &[]) })).await;
        let answer = match started {
            Ok(_) => tokio::time::timeout(Duration::from_secs(600), rx).await.ok().and_then(|r| r.ok()),
            Err(e) => {
                lock(&self.codex.titling).remove(&tmp);
                self.codex_side_close(id).await;
                return Err(e.into());
            }
        };
        lock(&self.codex.titling).remove(&tmp);
        match answer {
            Some(a) if a.starts_with('\u{1}') => Err(LocalError::new("CODEX_REQUEST_FAILED").with("detail", a[1..].chars().take(500).collect::<String>())),
            Some(a) => Ok(json!({ "response": a, "synthetic": false })),
            None => Err(LocalError::new("CODEX_REQUEST_FAILED").with("detail", "timed out")),
        }
    }

    /// 关掉旁聊：放掉临时对话
    pub(super) async fn codex_side_close(&self, id: &str) {
        let Some((_, tmp, srv)) = lock(&self.codex.sides).remove(id) else { return };
        lock(&self.codex.titling).remove(&tmp);
        if let Some(server) = srv.upgrade().filter(|s| s.alive()) {
            let _ = server.call("thread/unsubscribe", json!({ "threadId": tmp })).await;
        }
    }

    /// 对话界面发的消息没经过 TUI，Codex 不会给对话起名（起名是 TUI 做的：看到第一句话就开一个临时对话让模型起，
    /// 再 `thread/name/set`）。一轮结束后还没有名字就照着做一遍；TUI 也起了的话以先起好的为准。
    async fn codex_auto_title(self: Arc<Self>, id: String, tid: String, prompt: String) {
        if !lock(&self.codex.titled).insert(tid.clone()) {
            return;
        }
        tokio::time::sleep(TITLE_DELAY).await;
        let inst = self.inst(&id);
        let model = {
            let st = lock(&inst.st);
            if st.title.is_some() || st.binding.as_ref().is_none_or(|b| b.session_id != tid) {
                return;
            }
            st.actual_model.clone().or_else(|| st.model.clone())
        };
        let model = model.map(|m| crate::codex_profiles::for_instance(&self.store, &id).remote(&m));
        let Some(server) = self.codex.server.lock().await.clone() else { return };
        let named = |r: &Value| r["thread"]["name"].as_str().is_some_and(|n| !n.trim().is_empty());
        match server.call("thread/read", json!({ "threadId": tid })).await {
            Ok(r) if !named(&r) => {}
            _ => return,
        }
        let cwd = self.codex_current_cwd(&id).ok();
        // 和 TUI 一样：只读、不连 MCP、不用工具和技能，只要一句结构化的回答
        let mut config = json!({
            "features.apps": false, "features.code_mode": false, "features.goals": false, "features.hooks": false,
            "features.image_generation": false, "features.memories": false, "features.multi_agent": false, "features.plugins": false,
            "features.shell_snapshot": false, "features.shell_tool": false, "features.unified_exec": false, "features.view_image": false,
            "skills.include_instructions": false, "tools.update_plan.enabled": false, "web_search": "disabled",
        });
        if let Ok(c) = server.call("config/read", json!({ "includeLayers": false, "cwd": cwd })).await {
            if let Some(m) = c["config"]["mcp_servers"].as_object() {
                config["mcp_servers"] = m.keys().map(|k| (k.clone(), json!({ "enabled": false }))).collect::<serde_json::Map<_, _>>().into();
            }
        }
        let mut params = self.codex_thread_opts(&id);
        merge(&mut params, json!({ "cwd": cwd, "ephemeral": true, "threadSource": "thread_title", "sandbox": "read-only", "approvalPolicy": "never", "config": config }));
        // 起名不用助理的规矩
        if let Some(o) = params.as_object_mut() {
            o.remove("developerInstructions");
        }
        if let Some(m) = model {
            params["model"] = json!(m);
        }
        let tmp = match server.call("thread/start", params).await {
            Ok(r) => r["thread"]["id"].as_str().map(String::from),
            Err(e) => {
                tracing::debug!(error = ?e, "起名用的临时对话没开起来");
                None
            }
        };
        let Some(tmp) = tmp else { return };
        let (tx, rx) = oneshot::channel();
        lock(&self.codex.titling).insert(tmp.clone(), (String::new(), Some(tx)));
        let schema = json!({
            "type": "object",
            "properties": { "title": { "type": "string", "minLength": 1, "maxLength": TITLE_MAX } },
            "required": ["title"],
            "additionalProperties": false,
        });
        let started = server.call("turn/start", json!({ "threadId": tmp, "input": text_input(&title_prompt(&prompt), &[]), "outputSchema": schema })).await;
        let answer = match started {
            Ok(_) => tokio::time::timeout(TITLE_WAIT, rx).await.ok().and_then(|r| r.ok()),
            Err(_) => None,
        };
        lock(&self.codex.titling).remove(&tmp);
        let _ = server.call("thread/unsubscribe", json!({ "threadId": tmp })).await;
        let Some(title) = answer.as_deref().and_then(parse_title) else {
            tracing::debug!(thread = tid, "自动起名没拿到名字，下一轮再试");
            lock(&self.codex.titled).remove(&tid);
            return;
        };
        match server.call("thread/read", json!({ "threadId": tid })).await {
            Ok(r) if !named(&r) => {}
            _ => return,
        }
        // 设好之后 app-server 发 thread/name/updated，名字从那里进界面；Codex 自己也记进 session_index.jsonl
        if let Err(e) = server.call("thread/name/set", json!({ "threadId": tid, "name": title })).await {
            tracing::debug!(error = ?e, "自动起的名字没设上");
        }
    }

    // ---------------- 新版本 ----------------

    /// Codex 有没有新版本：看看开的 TUI 不再自己提示（见 `codex_app::NO_UPDATE_PROMPT`），由这里查、在对话页提示。
    /// 用 npm 装的可以一键更新（`npm install -g @openai/codex`，和新建会话时装 Codex 同一个任务）；别的装法只提示。
    /// 最新版本查 npm 镜像（和装的时候同一个源，国内能连上），[`UPDATE_TTL`] 内用上次的结果；跳过的版本不再提示。
    /// 没有新版本、查不到时为 null。
    pub(super) async fn codex_update_info(&self) -> Value {
        let cached = lock(&self.codex.update).as_ref().filter(|(t, _)| t.elapsed() < UPDATE_TTL).map(|(_, v)| v.clone());
        let info = match cached {
            Some(v) => v,
            None => {
                let program = codex_app::program(self.instances.shell()).await;
                let (current, npm) = installed_version(&program).await;
                let latest = latest_version().await;
                let v = json!({ "current": current, "latest": latest, "npm": npm });
                // 查不到最新版本（没联网）时不缓存太久：过一会儿再查
                if latest.is_some() {
                    *lock(&self.codex.update) = Some((Instant::now(), v.clone()));
                }
                v
            }
        };
        let skipped: Option<String> = self.store.get(UPDATE_SKIPPED).ok().flatten();
        let (Some(cur), Some(latest)) = (info["current"].as_str(), info["latest"].as_str()) else { return Value::Null };
        if !newer(latest, cur) || skipped.as_deref() == Some(latest) {
            return Value::Null;
        }
        info
    }

    /// 对话页上的“更新”：开始装（进度用 `GET /tools/install/codex` 看）；装好后 app-server 等空闲时换成新的。
    /// “跳过这个版本”：记下来，同一个版本不再提示。
    pub async fn codex_update(self: &Arc<Self>, install: bool) -> Result<Value, LocalError> {
        let info = self.codex_update_info().await;
        if !install {
            if let Some(v) = info["latest"].as_str() {
                self.store.set(UPDATE_SKIPPED, &v)?;
            }
            return Ok(Value::Null);
        }
        if info["npm"] != true {
            return Err(LocalError::new("CODEX_UPDATE_MANUAL"));
        }
        let job = crate::tools::start_install(self.instances.shell(), "codex", "auto").ok_or_else(|| LocalError::new("CHAT_UNAVAILABLE"))?;
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match crate::tools::job("codex").map(|j| j.state) {
                    Some("running") => continue,
                    Some("ok") => me.codex_installed(),
                    _ => {}
                }
                break;
            }
        });
        Ok(json!(job))
    }

    /// 装了新的 Codex（对话页的“更新”、AI 工具更新 agent_update.rs）：app-server 空闲时换成新版本，版本提示和模型列表重新取
    pub fn codex_installed(&self) {
        tracing::info!("Codex 更新好了，app-server 空闲时换成新版本");
        self.codex.stale.store(true, Ordering::SeqCst);
        *lock(&self.codex.update) = None;
        *lock(&self.codex.models) = None;
    }

    // ---------------- 待确认的 hook ----------------

    /// 工作目录里新的、改过的 hook（用户的、项目 `.codex/` 的、插件的）：Codex 只运行用户信任过的，
    /// 没信任的在对话里也不运行，终端里的 TUI 每次启动都会问（“Hooks need review”）。
    /// 对话页显示成卡片，让用户在这里信任或跳过；跳过过的（同一条、内容没变）不再列出。
    /// 同时带上还没登录（`login_required`）：要用 OpenAI 账号、又没登录时，对话发不出去，TUI 启动时停在登录页。
    pub async fn codex_setup(&self, id: &str) -> Result<Value, LocalError> {
        let (server, list) = self.codex_hooks_list(id).await?;
        let skipped: Vec<String> = self.store.get(HOOKS_SKIPPED)?.unwrap_or_default();
        let hooks: Vec<Value> = list.iter().filter(|h| !skipped.contains(&hook_id(h))).map(hook_view).collect();
        // 共用的 app-server 按官方配置起，account/read 总说要 OpenAI 登录；这个会话用的是第三方接口（Key 在）时
        // 线程按它的服务商起（thread_params），不用登录
        let mut api = crate::codex_profiles::usable(&self.paths.home, &crate::codex_profiles::for_instance(&self.store, id));
        let mut switched = false;
        let before = lock(&self.codex.login_switch).get(id).cloned();
        if let Some(before) = before.filter(|_| api) {
            // 在这个会话里点了设备码登录：登录好了（账号变了）就换到官方账号
            if let Ok(a) = server.call("account/read", json!({})).await {
                if !a["account"].is_null() && a["account"] != before && self.codex_login_switch(id).await? {
                    (api, switched) = (false, true);
                }
            }
        } else if !api {
            lock(&self.codex.login_switch).remove(id);
        }
        let login_required = !api && server.call("account/read", json!({})).await.is_ok_and(|a| a["requiresOpenaiAuth"] == true && a["account"].is_null());
        // 目标：打开对话、每轮做完时顺便取一次（换了对话、别处改过）
        let inst = self.inst(id);
        let thread = lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone());
        if let Some(tid) = thread {
            if let Ok((seq, r)) = server.call_seq("thread/goal/get", json!({ "threadId": tid })).await {
                let mut st = lock(&inst.st);
                let goal = Some(r["goal"].clone()).filter(|g| !g.is_null());
                if st.binding.as_ref().map(|b| b.session_id.as_str()) == Some(tid.as_str()) && st.codex.set_goal(seq, goal) {
                    Self::emit_status(&inst, &st);
                }
            }
        }
        let update = self.codex_update_info().await;
        Ok(json!({ "hooks": hooks, "login_required": login_required, "update": update, "switched": switched }))
    }

    /// 用第三方接口的会话里登录了 OpenAI 账号：这个会话改用官方账号（和用量面板里换配置一样，重新接上对话），
    /// 模型菜单跟着换成 OpenAI 的。在跑、在等审批时先不换（返回 false），下次取 setup 时再换
    async fn codex_login_switch(&self, id: &str) -> Result<bool, LocalError> {
        if self.busy(id) {
            return Ok(false);
        }
        lock(&self.codex.login_switch).remove(id);
        crate::codex_profiles::pin(&self.store, id, Some(crate::codex_profiles::OFFICIAL))?;
        {
            // 选过的是服务商的模型名，OpenAI 不认：清掉，重新接上对话时按官方配置的模型来
            let inst = self.inst(id);
            let mut st = lock(&inst.st);
            (st.model_set_at, st.model, st.actual_model) = (None, None, None);
            Self::emit_status(&inst, &st);
        }
        self.codex_reload(id).await;
        Ok(true)
    }

    /// 在对话页登录 Codex（TUI 启动时的登录页）：`device` = ChatGPT 设备码（远程也能用：在任何浏览器打开网址填码），
    /// `api_key` = 填 API Key。设备码登录由 app-server 自己等，做完发 `account/login/completed`，界面重新取 setup
    pub async fn codex_login(&self, id: &str, how: &str, key: Option<&str>) -> Result<Value, LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex_ready(id).await?;
        let params = match how {
            "device" => json!({ "type": "chatgptDeviceCode" }),
            "api_key" => {
                let key = key.map(str::trim).filter(|k| !k.is_empty() && k.len() < 512).ok_or_else(|| LocalError::invalid("api_key", "required"))?;
                json!({ "type": "apiKey", "apiKey": key })
            }
            _ => return Err(LocalError::invalid("how", "invalid")),
        };
        let api = crate::codex_profiles::usable(&self.paths.home, &crate::codex_profiles::for_instance(&self.store, id));
        let before = if api && how == "device" { Some(server.call("account/read", json!({})).await.map(|a| a["account"].clone()).unwrap_or(Value::Null)) } else { None };
        let r = server.call("account/login/start", params).await?;
        // 用第三方接口的会话里登录：点登录就是要在这个会话里用这个 OpenAI 账号。API Key 当场就登录好了，直接换；
        // 设备码要等浏览器那边，记下现在的账号，取 setup 时看到变了再换
        let mut switched = false;
        if api {
            match before {
                Some(b) => {
                    lock(&self.codex.login_switch).insert(id.to_string(), b);
                }
                None => {
                    switched = self.codex_login_switch(id).await?;
                    if !switched {
                        lock(&self.codex.login_switch).insert(id.to_string(), Value::Null);
                    }
                }
            }
        }
        Ok(json!({ "login_id": r["loginId"], "url": r["verificationUrl"], "code": r["userCode"], "switched": switched }))
    }

    /// Codex 的 MCP 服务器（TUI 的 /mcp）：连接状态、登录状态、工具数
    pub async fn codex_mcp(&self, id: &str) -> Result<Value, LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex_ready(id).await?;
        let thread = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone());
        let mut params = json!({ "detail": "toolsAndAuthOnly", "limit": 100 });
        if let Some(t) = thread.filter(|_| lock(&self.inst(id).st).codex.generation != 0) {
            params["threadId"] = json!(t);
        }
        let r = server.call("mcpServerStatus/list", params).await?;
        let items: Vec<Value> = r["data"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                json!({
                    "name": m["name"],
                    "status": m["runtimeStatus"],
                    "auth": m["authStatus"],
                    "tools": m["tools"].as_object().map(|t| t.len()).unwrap_or(0),
                    "error": m["toolsError"],
                    "plugin": m["pluginId"],
                })
            })
            .collect();
        Ok(json!({ "items": items }))
    }

    /// MCP 服务器的 OAuth 登录：给出要打开的网址（回调到这台电脑上，所以要在这台电脑的浏览器里打开）
    pub async fn codex_mcp_login(&self, id: &str, name: &str) -> Result<Value, LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex_ready(id).await?;
        let r = server.call("mcpServer/oauth/login", json!({ "name": name })).await?;
        Ok(json!({ "url": r["authorizationUrl"] }))
    }

    pub async fn codex_login_cancel(&self, id: &str, login_id: &str) -> Result<(), LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex_ready(id).await?;
        lock(&self.codex.login_switch).remove(id);
        server.call("account/login/cancel", json!({ "loginId": login_id })).await?;
        Ok(())
    }

    /// /status：这段对话的配置（模型、强度、权限、目录）、账号、token 用量、记录文件
    pub(super) async fn codex_status(&self, id: &str) -> Result<Value, LocalError> {
        let server = self.codex_ready(id).await?;
        let inst = self.inst(id);
        let (thread, path, model, effort, perm, cwd, totals, window, version) = {
            let st = lock(&inst.st);
            let b = st.binding.as_ref();
            (
                b.map(|b| b.session_id.clone()),
                b.map(|b| b.transcript_path.display().to_string()).filter(|p| !p.is_empty()),
                st.actual_model.clone().or(st.model.clone()),
                st.effort.clone(),
                st.codex.perm.clone(),
                st.codex.cwd.clone(),
                st.codex.totals.clone(),
                st.codex.window,
                st.version.clone(),
            )
        };
        let account = server.call("account/read", json!({})).await.unwrap_or(Value::Null);
        let row = |label: &str, value: Option<String>| value.filter(|v| !v.is_empty()).map(|v| json!({ "label": label, "value": v }));
        let acct = &account["account"];
        let who = acct["email"].as_str().map(String::from).or_else(|| acct["type"].as_str().map(|t| if t == "apiKey" { "API key".to_string() } else { t.to_string() }));
        let tokens = totals.as_ref().and_then(|t| t["totalTokens"].as_u64().or_else(|| t["total_tokens"].as_u64())).map(|n| format!("{n}"));
        let session: Vec<Value> = [
            row("Version", version),
            row("Thread", thread),
            row("Rollout", path),
            row("Directory", cwd.or_else(|| self.store.instance(id).ok().flatten().map(|r| r.workdir))),
        ]
        .into_iter()
        .flatten()
        .collect();
        let config: Vec<Value> = [row("Model", model), row("Reasoning effort", effort), row("Permissions", perm), row("Account", who), row("Plan", acct["planType"].as_str().map(String::from))]
            .into_iter()
            .flatten()
            .collect();
        let usage: Vec<Value> = [row("Tokens used", tokens), row("Context window", window.map(|w| w.to_string()))].into_iter().flatten().collect();
        Ok(json!({ "sections": [
            { "title": "Session", "rows": session },
            { "title": "Config", "rows": config },
            { "title": "Usage", "rows": usage },
        ] }))
    }

    /// /feedback：交给 Codex 上传（带上这段对话的 id；勾了才带日志）
    pub(super) async fn codex_feedback(&self, id: &str, text: &str, logs: bool) -> Result<Value, LocalError> {
        let server = self.codex_ready(id).await?;
        let thread = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone());
        server.call("feedback/upload", json!({ "classification": "other", "reason": text, "includeLogs": logs, "threadId": thread })).await
    }

    /// 信任（和 TUI 的“Trust all and continue”写同一个值：`hooks.state.<key>.trusted_hash`）或跳过界面上列出的这几条。
    /// `picked` 是 `(key, hash)`：只认现在还是这个内容的，用户看过之后命令又改了的不算（要重新确认）。
    pub async fn codex_hooks_answer(&self, id: &str, trust: bool, picked: &[(String, String)]) -> Result<(), LocalError> {
        let (server, list) = self.codex_hooks_list(id).await?;
        let hooks: Vec<&Value> = list.iter().filter(|h| picked.iter().any(|(k, hash)| h["key"] == k.as_str() && h["currentHash"] == hash.as_str())).collect();
        if hooks.len() != picked.len() {
            return Err(LocalError::new("CODEX_HOOKS_CHANGED"));
        }
        if trust {
            let state: serde_json::Map<String, Value> = hooks.iter().map(|h| (h["key"].as_str().unwrap_or_default().to_string(), json!({ "trusted_hash": h["currentHash"] }))).collect();
            let edit = json!({ "keyPath": "hooks.state", "value": state, "mergeStrategy": "upsert" });
            server.call("config/batchWrite", json!({ "edits": [edit], "reloadUserConfig": true })).await?;
        } else {
            let mut skipped: Vec<String> = self.store.get(HOOKS_SKIPPED)?.unwrap_or_default();
            skipped.extend(hooks.iter().map(|h| hook_id(h)).filter(|s| !skipped.contains(s)).collect::<Vec<_>>());
            self.store.set(HOOKS_SKIPPED, &skipped)?;
        }
        Ok(())
    }

    /// app-server 的 `hooks/list`（这个终端的工作目录）里要用户确认的那些
    async fn codex_hooks_list(&self, id: &str) -> Result<(Arc<AppServer>, Vec<Value>), LocalError> {
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let server = self.codex_server().await?;
        let r = server.call("hooks/list", json!({ "cwds": [row.workdir] })).await?;
        let hooks = r["data"].as_array().and_then(|d| d.first()).and_then(|e| e["hooks"].as_array()).cloned().unwrap_or_default();
        Ok((server, hooks.into_iter().filter(|h| matches!(h["trustStatus"].as_str(), Some("untrusted" | "modified"))).collect()))
    }

    // ---------------- 以前的会话转到对话模式 ----------------

    // ---------------- 空闲卸载、后台命令（资源看板） ----------------

    /// 后台（每分钟）：空闲很久、没人看的对话退订，app-server 过一会儿（默认 60 秒）卸载它，它起的 MCP、
    /// code mode 等进程跟着退出；再打开、发消息时 `codex_attach` 重新加载。这些情况不动：
    /// - 还有后台命令（dev server 之类）：卸载可能把它们一起结束；问不到（老版本 Codex）也当成有。
    pub(super) async fn codex_reap(&self, inst: &Inst) {
        let gen = self.codex.generation.load(Ordering::SeqCst);
        let idle = |st: &super::State| {
            gen != 0
                && st.codex.generation == gen
                && st.phase == Phase::Idle
                && st.codex.turn.is_none()
                && st.codex.sending == 0
                && st.codex.later.is_empty()
                && st.codex.used_at.is_some_and(|t| t.elapsed() >= IDLE_UNLOAD)
                && inst.viewers.load(Ordering::SeqCst) == 0
        };
        let tid = {
            let st = lock(&inst.st);
            match st.binding.as_ref() {
                Some(b) if idle(&st) => b.session_id.clone(),
                _ => return,
            }
        };
        if lock(&self.codex.threads).get(&tid) != Some(&inst.id) {
            return;
        }
        let Some(server) = self.codex.server.lock().await.clone() else { return };
        match tokio::time::timeout(BG_LIST_WAIT, server.call("thread/backgroundTerminals/list", json!({ "threadId": tid, "limit": 1 }))).await {
            Ok(Ok(r)) if r["data"].as_array().is_some_and(Vec::is_empty) => {}
            _ => return,
        }
        // 先标成没加载（这期间要是有人发消息，codex_attach 会重新 resume），再退订
        {
            let mut st = lock(&inst.st);
            if !idle(&st) || st.binding.as_ref().map(|b| b.session_id.as_str()) != Some(tid.as_str()) {
                return;
            }
            st.codex.generation = 0;
        }
        lock(&self.codex.threads).remove(&tid);
        tracing::info!(id = %inst.id, thread = %tid, "Codex 对话空闲很久了，先卸载，用到时再接着");
        if let Err(e) = server.call("thread/unsubscribe", json!({ "threadId": tid })).await {
            tracing::debug!(thread = %tid, error = ?e, "退订 Codex 对话失败");
        }
    }

    /// 资源看板：app-server 的进程号、加载着的对话数、各对话的后台命令
    pub(super) async fn codex_usage_roots(&self) -> Option<(u32, usize, Vec<BgTerm>)> {
        let server = self.codex.server.lock().await.as_ref().filter(|s| s.alive()).cloned()?;
        let pid = server.pid()?;
        let threads: Vec<(String, String)> = lock(&self.codex.threads).iter().map(|(t, id)| (t.clone(), id.clone())).collect();
        let mut bg = vec![];
        for (t, id) in &threads {
            let Ok(Ok(r)) = tokio::time::timeout(BG_LIST_WAIT, server.call("thread/backgroundTerminals/list", json!({ "threadId": t }))).await else { continue };
            for b in r["data"].as_array().into_iter().flatten() {
                let Some(process_id) = b["processId"].as_str() else { continue };
                bg.push(BgTerm {
                    session: id.clone(),
                    process_id: process_id.to_string(),
                    command: b["command"].as_str().unwrap_or("").chars().take(300).collect(),
                    pid: b["osPid"].as_u64().and_then(|p| u32::try_from(p).ok()),
                });
            }
        }
        Some((pid, threads.len(), bg))
    }

    /// 停止这个对话的一个后台命令（`process_id` 是 app-server 列出来的）
    pub(super) async fn codex_stop_bg(&self, id: &str, process_id: &str) -> Result<(), LocalError> {
        let tid = lock(&self.inst(id).st).binding.as_ref().map(|b| b.session_id.clone()).ok_or_else(LocalError::not_found)?;
        if lock(&self.codex.threads).get(&tid).map(String::as_str) != Some(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex.server.lock().await.clone().ok_or_else(LocalError::not_found)?;
        let r = server.call("thread/backgroundTerminals/terminate", json!({ "threadId": tid, "processId": process_id })).await?;
        if r["terminated"] == false {
            return Err(LocalError::not_found());
        }
        Ok(())
    }
}

/// `config/read` 的 `projects` 里这个目录已经有 trust_level（Windows 上路径不分大小写，和 Codex 的查法一样）
fn has_trust_level(projects: &Value, workdir: &str) -> bool {
    let windows = workdir.contains('\\') || workdir.as_bytes().get(1) == Some(&b':');
    projects.as_object().is_some_and(|m| {
        m.iter().any(|(k, v)| (k == workdir || windows && k.eq_ignore_ascii_case(workdir)) && v["trust_level"].is_string())
    })
}

/// 查询一段对话当前正在运行的轮次 id：0.157+ 分页对话走 `thread/turns/list`（`thread/read` 的 `turns` 为空），老版本回退 `thread/read`
async fn codex_active_turn_id(server: &AppServer, thread: &str) -> Option<String> {
    if let Ok(r) = server.call("thread/turns/list", json!({ "threadId": thread, "limit": 5, "itemsView": "notLoaded" })).await {
        if let Some(id) = r["data"].as_array().and_then(|a| a.iter().find(|t| t["status"] == "inProgress")).and_then(|t| t["id"].as_str()) {
            return Some(id.to_string());
        }
    }
    let r = server.call("thread/read", json!({ "threadId": thread, "includeTurns": true })).await.ok()?;
    r["thread"]["turns"].as_array()?.iter().rev().find(|t| t["status"] == "inProgress")?.get("id")?.as_str().map(String::from)
}

/// `thread/resume`；会话被归档了（在 TUI 里 `/archive`、Codex 桌面版里归档）就先取消归档再接：
/// 用户打开的就是这个终端的对话，不然对话页接不上，终端里的 TUI 也会停在 “This conversation is archived” 上。
/// 只认 app-server 对这个会话说的“已归档”，别的错误照常返回（和 TUI 的 session_start.rs 一样）
async fn resume_thread(server: &AppServer, thread: &str, params: Value) -> Result<Value, LocalError> {
    match server.call("thread/resume", params.clone()).await {
        Err(e) if e.params.get("detail").and_then(Value::as_str).is_some_and(|m| m.starts_with(&format!("session {thread} is archived. "))) => {
            tracing::info!(thread, "Codex 会话已归档，取消归档后接着用");
            server.call("thread/unarchive", json!({ "threadId": thread })).await?;
            server.call("thread/resume", params).await
        }
        r => r,
    }
}

/// 多久查一次 Codex 的新版本（Codex 自己是 20 小时）
const UPDATE_TTL: Duration = Duration::from_secs(6 * 3600);
/// 跳过的版本号
const UPDATE_SKIPPED: &str = "codex_update_skipped";

/// 装着的 Codex 版本，以及是不是 npm 装的。npm 装的从包里的 package.json 读（不用起进程）：
/// `…/node_modules/@openai/codex/bin/codex.js`（macOS、Linux 的 PATH 里是指向它的链接）或
/// `…/@openai/codex/node_modules/@openai/codex-<平台>/vendor/<三元组>/bin/codex(.exe)`（Windows 上直接调的）
async fn installed_version(program: &Path) -> (Option<String>, bool) {
    let real = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let pkg = real.ancestors().find(|d| d.file_name().is_some_and(|n| n == "codex") && d.parent().and_then(Path::file_name).is_some_and(|n| n == "@openai"));
    if let Some(v) = pkg.and_then(|d| std::fs::read_to_string(d.join("package.json")).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|j| j["version"].as_str().map(String::from)) {
        return (Some(v), true);
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await.ok().and_then(Result::ok);
    let v = out.and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().last().map(String::from)).filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()));
    (v, false)
}

/// npm 上 `@openai/codex` 的最新正式版（npmmirror，连不上再试官方源）
async fn latest_version() -> Option<String> {
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(10)).build().ok()?;
    for mirror in ["npmmirror", "official"] {
        let url = format!("{}@openai%2fcodex/latest", crate::tools::registry(mirror));
        if let Ok(r) = client.get(&url).send().await.and_then(reqwest::Response::error_for_status) {
            if let Some(v) = r.json::<Value>().await.ok().and_then(|j| j["version"].as_str().map(String::from)) {
                return Some(v);
            }
        }
    }
    None
}

/// `a` 比 `b` 新（按数字比 `x.y.z`；带预发布后缀的不算新版本）
fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect() };
    matches!((parse(a), parse(b)), (Some(x), Some(y)) if x > y)
}


/// 跳过过的 hook：`<key>\n<哈希>`（命令改了哈希就变，会再问）
const HOOKS_SKIPPED: &str = "codex_hooks_skipped";

fn hook_id(h: &Value) -> String {
    format!("{}\n{}", h["key"].as_str().unwrap_or_default(), h["currentHash"].as_str().unwrap_or_default())
}

/// 界面上显示的一条 hook：什么时候运行、运行什么、写在哪里
fn hook_view(h: &Value) -> Value {
    json!({
        "key": h["key"],
        "hash": h["currentHash"],
        "event": h["eventName"],
        "matcher": h["matcher"],
        "type": h["handlerType"],
        "command": h["command"],
        "source": h["source"],
        "path": h["sourcePath"],
        "plugin": h["pluginId"],
        "modified": h["trustStatus"] == "modified",
        "enabled": h["enabled"],
    })
}

/// `config/batchWrite` 的键：目录里有 `.`，要用双引号括起来当一整段（和 Codex TUI 写的一样）
fn trust_key_path(workdir: &str) -> String {
    format!("projects.\"{}\".trust_level", workdir.replace('\\', "\\\\").replace('"', "\\\""))
}

fn merge(a: &mut Value, b: Value) {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
}

/// `thread/tokenUsage/updated` 的 `tokenUsage`（或从记录文件里换算的同样形状）
fn set_usage(st: &mut super::State, u: &Value) {
    st.context = u["last"]["inputTokens"].as_u64();
    st.codex.window = u["modelContextWindow"].as_u64().or(st.codex.window);
    st.codex.totals = Some(u["total"].clone());
}

#[cfg(test)]
mod tests {
    #[test]
    fn fast_tier_and_state() {
        let m = json!({ "serviceTiers": [{ "id": "priority", "name": "Fast", "description": "1.5x speed" }] });
        assert_eq!(fast_tier_of(&m).as_deref(), Some("priority"));
        assert_eq!(fast_tier_of(&json!({ "additionalSpeedTiers": ["fast"] })).as_deref(), Some("priority"));
        assert_eq!(fast_tier_of(&json!({ "serviceTiers": [] })), None);
        let mut st = CodexSt::default();
        assert!(!fast_on(&st));
        st.tier = Some("priority".into());
        assert!(fast_on(&st));
        st.tier_set = Some("default".into());
        assert!(!fast_on(&st), "界面上关了的优先");
        st.tier = Some("flex".into());
        st.tier_set = None;
        assert!(!fast_on(&st));
        let opts = model_options(&json!({ "data": [{ "model": "gpt-5.5", "serviceTiers": [{ "id": "priority", "name": "Fast", "description": "" }] }, { "model": "x" }] }));
        assert_eq!(opts[0]["fast_tier"], "priority");
        assert!(opts[1]["fast_tier"].is_null());
    }

    #[test]
    fn codex_compact_point() {
        let dir = std::env::temp_dir().join(format!("ll-cx-compact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = serde_json::from_value::<crate::codex_profiles::Profile>(json!({ "id": "x", "name": "" })).unwrap();
        assert!(codex_compact(&dir, &p).is_none());
        std::fs::write(dir.join("config.toml"), "model_auto_compact_token_limit = 250000\n").unwrap();
        let v = codex_compact(&dir, &p).unwrap();
        assert_eq!(v["at"], 250000);
        assert!(v["source"].as_str().unwrap().ends_with("config.toml"));
        p.tuning.compact_limit = Some(180_000);
        assert_eq!(codex_compact(&dir, &p).unwrap(), json!({ "enabled": true, "at": 180000, "source": "tuning" }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn review_and_slash_args() {
        assert_eq!(slash_arg("/review", "/review"), Some(""));
        assert_eq!(slash_arg("/review  look at auth ", "/review"), Some("look at auth"));
        assert_eq!(slash_arg("/reviewer", "/review"), None);
        assert_eq!(slash_arg("/goal pause", "/goal"), Some("pause"));
        assert_eq!(review_target(""), json!({ "type": "uncommittedChanges" }));
        assert_eq!(review_target("--base main"), json!({ "type": "baseBranch", "branch": "main" }));
        assert_eq!(review_target("--commit abc123"), json!({ "type": "commit", "sha": "abc123" }));
        assert_eq!(review_target("focus on errors"), json!({ "type": "custom", "instructions": "focus on errors" }));
    }

    use super::*;

    /// 停会话：对话从共用的 app-server 里真的卸载（不然 ChatGPT 桌面端打开同一段对话提示被占用）
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_unloads_thread() {
        if std::process::Command::new("codex").arg("--version").output().is_err() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input { name: Some("c".into()), workdir: Some(workdir.clone()), launch: Some("codex".into()), chat_ui: Some(true), ..Default::default() };
        let id = instances.create(input).await.unwrap().row.id;
        let chat = Chat::new(store.clone(), paths.clone(), instances.clone());
        chat.watch_stops();
        let inst = chat.inst(&id);
        let server = chat.codex_server().await.expect("起 app-server");
        let tid = chat.codex_new_thread(&server, &inst).await.unwrap();
        let loaded = |r: Value| r["data"].as_array().is_some_and(|a| a.iter().any(|t| t.as_str() == Some(tid.as_str())));
        assert!(loaded(server.call("thread/loaded/list", json!({})).await.unwrap()));
        tokio::time::sleep(Duration::from_secs(1)).await;
        instances.stop(&id).await.unwrap();
        let mut gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if !loaded(server.call("thread/loaded/list", json!({})).await.unwrap()) {
                gone = true;
                break;
            }
        }
        assert!(gone, "停了会话，对话还加载在 app-server 里");
        assert!(!lock(&chat.codex.threads).contains_key(&tid));
        assert_eq!(lock(&inst.st).binding.as_ref().map(|b| b.session_id.clone()), Some(tid), "绑定留着，再开接着这段");
        server.kill();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_thread_unloads() {
        if std::process::Command::new("codex").arg("--version").output().is_err() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input { name: Some("c".into()), workdir: Some(workdir.clone()), launch: Some("codex".into()), chat_ui: Some(true), ..Default::default() };
        let id = instances.create(input).await.unwrap().row.id;
        let chat = Chat::new(store.clone(), paths.clone(), instances.clone());
        let inst = chat.inst(&id);
        let server = chat.codex_server().await.expect("起 app-server");
        let tid = chat.codex_new_thread(&server, &inst).await.unwrap();
        assert_eq!(lock(&chat.codex.threads).get(&tid), Some(&id));
        let (pid, loaded, bg) = chat.codex_usage_roots().await.unwrap();
        assert!(pid > 0 && loaded == 1 && bg.is_empty());

        // 刚用过：不动
        chat.codex_reap(&inst).await;
        assert!(lock(&chat.codex.threads).contains_key(&tid));
        // 很久没用，但有人在看：不动（先等新对话启动时的通知都到了，它们也算用到）
        tokio::time::sleep(Duration::from_secs(2)).await;
        lock(&inst.st).codex.used_at = Some(Instant::now() - IDLE_UNLOAD);
        inst.viewers.fetch_add(1, Ordering::SeqCst);
        chat.codex_reap(&inst).await;
        assert!(lock(&chat.codex.threads).contains_key(&tid));
        inst.viewers.fetch_sub(1, Ordering::SeqCst);
        // 没人看了：退订、标成没加载
        chat.codex_reap(&inst).await;
        assert!(!lock(&chat.codex.threads).contains_key(&tid));
        assert_eq!(lock(&inst.st).codex.generation, 0);
        assert_eq!(chat.codex_usage_roots().await.unwrap().1, 0);
        server.kill();
    }

    #[test]
    fn dispatch_auto_accept() {
        let root = std::path::Path::new("/work/app");
        let bash = |c: &str| json!({ "command": c });
        assert!(auto_accept(FULL_GUARDED, "Bash", &bash("npm test"), root));
        assert!(!auto_accept(FULL_GUARDED, "Bash", &bash("git push --force origin main"), root), "高风险的问用户");
        assert!(!auto_accept(FULL_GUARDED, "Bash", &bash("sudo apt install x"), root));
        assert!(auto_accept(FULL_GUARDED, "apply_patch", &json!({ "changes": [{ "path": "/etc/hosts" }] }), root));
        assert!(!auto_accept(ASK_COMMANDS, "Bash", &bash("ls"), root), "动手前问：命令都问");
        let patch = |paths: &[&str], grant: Value| json!({ "changes": paths.iter().map(|p| json!({ "path": p })).collect::<Vec<_>>(), "grant_root": grant });
        assert!(auto_accept(ASK_COMMANDS, "apply_patch", &patch(&["/work/app/src/a.rs", "b.rs"], Value::Null), root));
        assert!(!auto_accept(ASK_COMMANDS, "apply_patch", &patch(&["/work/app/a.rs", "/work/other/b.rs"], Value::Null), root));
        assert!(!auto_accept(ASK_COMMANDS, "apply_patch", &patch(&["/work/app/../other/b.rs"], Value::Null), root));
        assert!(!auto_accept(ASK_COMMANDS, "apply_patch", &patch(&["/work/app/a.rs"], json!("/work")), root), "要多给目录的问用户");
        assert!(!auto_accept(ASK_COMMANDS, "apply_patch", &patch(&[], Value::Null), root));
        assert!(!auto_accept("workspaceWrite", "Bash", &bash("ls"), root), "用户自己选的档不替他放");
        assert!(auto_accept("fullAccess", "Bash", &bash("git push --force origin main"), root), "换成完全访问前开始的一轮：都放");
        assert!(auto_accept(FULL_GUARDED, MCP_CALL, &json!({}), root), "全权：MCP 工具（连服务器等）不问");
        assert!(!auto_accept(ASK_COMMANDS, MCP_CALL, &json!({}), root));
        assert!(!auto_accept("workspaceWrite", MCP_CALL, &json!({}), root));
    }

    #[test]
    fn perms_round_trip() {
        for p in PERMS {
            let (approval, sandbox, policy) = perm_params(p);
            assert_eq!(perm_of(&approval, &sandbox).as_deref(), Some(p), "thread/start 的写法");
            assert_eq!(perm_of(&approval, &policy).as_deref(), Some(p), "turn/start、thread/settings/updated 的写法");
        }
        assert_eq!(perm_of(&json!("untrusted"), &json!({ "type": "readOnly" })).as_deref(), Some("untrusted/readOnly"));
        assert_eq!(perm_of(&json!("never"), &Value::Null), None);
    }

    #[test]
    fn versions() {
        assert!(newer("0.165.0", "0.164.0"));
        assert!(newer("0.164.10", "0.164.9"), "按数字比");
        assert!(!newer("0.164.0", "0.164.0"));
        assert!(!newer("0.165.0-alpha.1", "0.164.0"), "预发布版不提示");
        assert!(!newer("0.163.0", "0.164.0"));
    }

    #[tokio::test]
    async fn npm_install_version() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("lib/node_modules/@openai/codex");
        let bin = pkg.join("node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{"name":"@openai/codex","version":"0.164.0"}"#).unwrap();
        std::fs::write(bin.join("codex"), "").unwrap();
        assert_eq!(installed_version(&bin.join("codex")).await, (Some("0.164.0".into()), true), "Windows 上直接调的平台包里的程序");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("bin/codex.js"), "").unwrap();
        assert_eq!(installed_version(&pkg.join("bin/codex.js")).await, (Some("0.164.0".into()), true));
    }

    #[test]
    fn hook_views() {
        let h = json!({ "key": "/u/.codex/config.toml:pre_tool_use:0:0", "eventName": "preToolUse", "handlerType": "command", "command": "echo hi", "matcher": "Bash", "sourcePath": "/u/.codex/config.toml", "source": "user", "pluginId": null, "enabled": true, "currentHash": "sha256:ab", "trustStatus": "modified" });
        assert_eq!(hook_id(&h), "/u/.codex/config.toml:pre_tool_use:0:0\nsha256:ab");
        let v = hook_view(&h);
        assert_eq!((v["event"].as_str(), v["command"].as_str(), v["hash"].as_str(), v["modified"].as_bool()), (Some("preToolUse"), Some("echo hi"), Some("sha256:ab"), Some(true)));
    }

    #[test]
    fn folder_trust() {
        // 0.164 的 TUI 自己选“Trust and continue”时写的就是这个键（config_update.rs 的 trusted_project_edit）
        assert_eq!(trust_key_path("/home/a/my.proj"), r#"projects."/home/a/my.proj".trust_level"#);
        assert_eq!(trust_key_path(r#"C:\Users\a "b""#), r#"projects."C:\\Users\\a \"b\"".trust_level"#);
        let projects = json!({ "/a": { "trust_level": "untrusted" }, "/b": {}, r"c:\users\x": { "trust_level": "trusted" } });
        assert!(has_trust_level(&projects, "/a"), "用户自己选的不信任不改");
        assert!(!has_trust_level(&projects, "/b"));
        assert!(!has_trust_level(&projects, "/A"), "Unix 路径分大小写");
        assert!(has_trust_level(&projects, r"C:\Users\x"), "Windows 路径不分大小写");
        assert!(!has_trust_level(&Value::Null, "/a"));
    }

    #[test]
    fn limits_map_windows() {
        let r = json!({ "rateLimits": { "planType": "plus", "primary": { "usedPercent": 24, "windowDurationMins": 300, "resetsAt": 1792583484 }, "secondary": { "usedPercent": 12, "windowDurationMins": 10080, "resetsAt": 1793082793 } } });
        let v = limits_view(&r);
        assert_eq!(v["five_hour"]["used"], 24);
        assert_eq!(v["seven_day"]["used"], 12);
        assert!(v["five_hour"]["resets_at"].as_str().unwrap().starts_with("2026-"));
        assert_eq!(v["plan"], "plus");
        assert!(v["credits"].is_null() && v["reset_credits"].is_null());
        let r = json!({ "rateLimits": { "credits": { "hasCredits": true, "unlimited": false, "balance": "125.5" } }, "rateLimitResetCredits": { "availableCount": 2, "credits": null } });
        let v = limits_view(&r);
        assert_eq!(v["credits"], json!({ "has": true, "unlimited": false, "balance": 125.5 }));
        assert_eq!(v["reset_credits"], 2);
        assert!(v["reset_cards"].is_null());
        let r = json!({ "rateLimits": {}, "rateLimitResetCredits": { "availableCount": 1, "credits": [
            { "id": "c1", "title": "Limit reset", "description": null, "grantedAt": 1792583484, "expiresAt": 1793082793, "status": "available", "resetType": "codexRateLimits" },
            { "id": "c0", "title": "Old", "grantedAt": 1792583484, "expiresAt": null, "status": "redeemed", "resetType": "codexRateLimits" } ] } });
        let v = limits_view(&r);
        assert_eq!(v["reset_cards"].as_array().unwrap().len(), 1);
        assert_eq!(v["reset_cards"][0]["id"], "c1");
        assert!(v["reset_cards"][0]["expires_at"].as_str().unwrap().starts_with("2026-"));
    }

    #[test]
    fn models_to_options() {
        let r = json!({ "data": [
            { "id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol", "description": "d", "hidden": false, "isDefault": true,
              "supportedReasoningEfforts": [{ "reasoningEffort": "low" }, { "reasoningEffort": "high" }], "defaultReasoningEffort": "low" },
            { "id": "old", "model": "old", "displayName": "Old", "hidden": true, "supportedReasoningEfforts": [] }
        ] });
        let o = model_options(&r);
        assert_eq!(o.len(), 2);
        assert_eq!(o[0]["value"], "gpt-6.1-sol");
        assert_eq!(o[0]["efforts"], json!(["low", "high"]));
        assert_eq!(o[1]["section"], "more");
    }

    #[test]
    fn provider_models_to_options() {
        let ids: Vec<String> = ["glm-4.6", "glm-4.5-air", "bad model"].map(String::from).into();
        let o = provider_model_options(Some("glm-4.5-air"), &ids);
        let v: Vec<(&str, &str)> = o.iter().map(|x| (x["value"].as_str().unwrap(), x["section"].as_str().unwrap())).collect();
        assert_eq!(v, [("glm-4.5-air", "main"), ("glm-4.6", "provider")]);
        assert_eq!(o[0]["default"], true);
        assert!(provider_model_options(None, &[]).is_empty());
    }

    /// 对应过的 Codex 模型：名字、强度照目录，`model` 是服务商的（回复里报的是它，按钮上照样显示 Codex 的名字）
    #[test]
    fn mapped_model_options() {
        let catalog = json!([
            { "value": "gpt-6.1-sol", "label": "GPT-6.1-Sol", "efforts": ["low", "high"], "section": "main", "model": "gpt-6.1-sol", "fast_tier": "priority" },
            { "value": "gpt-mini", "label": "GPT-Mini", "efforts": [], "section": "main", "model": "gpt-mini" },
        ]);
        let mut p = crate::codex_profiles::for_instance(&crate::store::Store::open(&tempfile::tempdir().unwrap().path().join("t.db")).unwrap(), "x");
        p.model = Some("gpt-6.1-sol".into());
        p.models = [("gpt-6.1-sol", "kimi-k3"), ("zz-custom", "glm-5")].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let o = mapped_options(&p, &catalog);
        let v: Vec<(&str, &str, &str)> = o.iter().map(|x| (x["value"].as_str().unwrap(), x["label"].as_str().unwrap(), x["model"].as_str().unwrap())).collect();
        assert_eq!(v, [("gpt-6.1-sol", "GPT-6.1-Sol", "kimi-k3"), ("zz-custom", "zz-custom", "glm-5")]);
        assert_eq!((o[0]["efforts"].clone(), o[0]["default"].clone(), o[0]["fast_tier"].clone()), (json!(["low", "high"]), json!(true), Value::Null));
    }

    #[test]
    fn decisions_and_answers() {
        let ans = |d: &str, a: Option<Value>| PermissionAnswer { id: 1, decision: d.into(), message: None, answers: a };
        assert_eq!(decision("Bash", "permission", &Value::Null, &ans("allow", None)).unwrap(), json!({ "decision": "accept" }));
        assert_eq!(decision("Bash", "permission", &Value::Null, &ans("allow_session", None)).unwrap(), json!({ "decision": "acceptForSession" }));
        assert_eq!(decision("apply_patch", "permission", &Value::Null, &ans("deny", None)).unwrap(), json!({ "decision": "decline" }));
        assert!(decision("Bash", "permission", &Value::Null, &ans("allow_dirs", None)).is_err());
        let input = questions_view(&json!({ "questions": [{ "id": "q1", "header": "H", "question": "选哪个？", "isOther": false, "isSecret": false, "options": [{ "label": "A", "description": "" }] }] }));
        assert_eq!(input["questions"][0]["multiSelect"], false);
        assert_eq!(input["questions"][0]["other"], false);
        assert!(input.get("auto_at").is_none());
        let auto = questions_view(&json!({ "questions": [{ "id": "q", "question": "Q", "isOther": true, "options": [] }], "autoResolutionMs": 60000 }));
        assert_eq!(auto["questions"][0]["other"], true);
        assert!(auto["auto_at"].as_u64().unwrap() > 60000);
        let multi = questions_view(&json!({ "questions": [{ "id": "q2", "header": "H", "question": "Which ones? (multi-select)", "options": [] }, { "id": "q3", "header": "功能", "question": "要哪些（可多选）？", "options": [] }] }));
        assert_eq!(multi["questions"][0]["multiSelect"], true);
        assert_eq!(multi["questions"][1]["multiSelect"], true);
        let r = decision("AskUserQuestion", "question", &input, &ans("answer", Some(json!({ "选哪个？": "A" })))).unwrap();
        assert_eq!(r, json!({ "answers": { "q1": { "answers": ["A"] } } }));
    }

    #[test]
    fn input_with_images() {
        let v = text_input("hi", &["C:\\a.png".into()]);
        let skills = parse_skills(&json!({ "data": [{ "cwd": "/p", "errors": [], "skills": [
            { "name": "pdf", "path": "/s/pdf/SKILL.md", "description": "Read PDFs\nmore", "enabled": true, "scope": "user" },
            { "name": "off", "path": "/s/off/SKILL.md", "description": "x", "enabled": false, "scope": "user" },
            { "name": "my-tool", "path": "/r/SKILL.md", "description": "d", "shortDescription": "short", "enabled": true, "scope": "repo" },
        ] }] }));
        assert_eq!(skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect::<Vec<_>>(), [("pdf", "Read PDFs"), ("my-tool", "short")]);
        let hit = |t: &str| mentioned_skills(t, &skills).iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(hit("用 $pdf 读一下，再 $my-tool."), ["pdf", "my-tool"]);
        assert_eq!(hit("a$pdf $pdfx $off $pdf"), ["pdf"]);
        assert_eq!(v[0]["type"], "text");
        assert_eq!(v[1], json!({ "type": "localImage", "path": "C:\\a.png" }));
    }

    #[test]
    fn auto_title() {
        assert_eq!(parse_title(r#"{"title":"“修复登录页。”"}"#).as_deref(), Some("修复登录页"));
        assert_eq!(parse_title(r#"{"title":"  Fix   login page! "}"#).as_deref(), Some("Fix login page"));
        assert_eq!(parse_title(r#"{"title":"  "}"#), None);
        assert_eq!(parse_title("Fix login"), None);
        let long = "字".repeat(2000);
        let p = title_prompt(&long);
        assert!(p.len() <= 960 && p.ends_with('字'));
        assert!(side_thread(&json!({ "ephemeral": true })));
        assert!(side_thread(&json!({ "threadSource": "thread_title" })));
        assert!(!side_thread(&json!({ "threadSource": "user", "source": "vscode" })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_errand_and_preselected_defaults_apply_to_thread_opts() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let mk = |name: &str| crate::instances::Input { name: Some(name.into()), workdir: Some(workdir.clone()), launch: Some("codex".into()), chat_ui: Some(true), ..Default::default() };
        let id_brain = instances.create(mk("brain")).await.unwrap().row.id;
        let id_ro = instances.create(mk("e-ro")).await.unwrap().row.id;
        let id_full = instances.create(mk("e-full")).await.unwrap().row.id;
        crate::assistant::set_brain(&store, crate::assistant::Engine::Codex, &id_brain).unwrap();
        crate::assistant::errand_update(&store, |m| {
            m.insert(id_ro.clone(), crate::assistant::Errand { title: "查".into(), project: workdir.clone(), started_at: 1, mode: None });
            m.insert(id_full.clone(), crate::assistant::Errand { title: "查告警".into(), project: workdir.clone(), started_at: 2, mode: Some("fullAccess".into()) });
        });
        let chat = Chat::new(store.clone(), paths.clone(), instances.clone());

        // 大脑依然不允许在会话里切换权限
        assert!(chat.setting(&id_brain, "mode", "fullAccess").await.is_err());

        // 未固定权限的跑腿启动时保持只读沙箱 + 助理闸门，但允许用户在会话里手动切换（且立刻放行等着的审批）
        let ro_opts = chat.codex_thread_opts(&id_ro);
        assert_eq!(ro_opts["sandbox"], "read-only");
        assert_eq!(ro_opts["approvalPolicy"], "on-request");
        assert_eq!(ro_opts["developerInstructions"], crate::assistant::ERRAND_RULES);
        assert!(crate::assistant::is_gated(&store, &id_ro));
        let (tx, rx) = oneshot::channel();
        {
            let inst = chat.inst(&id_ro);
            let mut st = lock(&inst.st);
            st.phase = Phase::Approval;
            st.pending = Some(Pending {
                id: 1,
                at: Instant::now(),
                tool: "Bash".into(),
                kind: "permission",
                input: json!({ "command": "lark-cli im +messages-search" }),
                suggestions: Value::Null,
                guard: None,
                tx: Some(tx),
            });
        }
        chat.setting(&id_ro, "mode", "fullAccess").await.unwrap();
        assert!(!crate::assistant::is_gated(&store, &id_ro));
        assert_eq!(crate::assistant::errand_mode(&store, &id_ro).as_deref(), Some("fullAccess"));
        assert_eq!(rx.await.unwrap(), json!({ "decision": "accept" }).to_string());
        let ro_after = chat.codex_thread_opts(&id_ro);
        assert_eq!(ro_after["approvalPolicy"], "never");
        assert_eq!(ro_after["sandbox"], "danger-full-access");
        assert_eq!(ro_after["developerInstructions"], crate::assistant::ERRAND_RULES);

        // 固定了权限、模型、思考强度的跑腿：建 thread 时就带上，且不会被 thread/start 默认的 medium 盖掉
        assert_eq!(chat.apply_mode(&id_full, "fullAccess").await.unwrap(), "fullAccess");
        chat.apply_model(&id_full, "gpt-6.2-sol").await.unwrap();
        chat.apply_effort(&id_full, "high").await.unwrap();
        let opts = chat.codex_thread_opts(&id_full);
        assert_eq!(opts["developerInstructions"], crate::assistant::ERRAND_RULES);
        assert_eq!(opts["approvalPolicy"], "never");
        assert_eq!(opts["sandbox"], "danger-full-access");
        assert_eq!(opts["model"], "gpt-6.2-sol");
        assert_eq!(opts["config"]["model_reasoning_effort"], "high");

        let inst = chat.inst(&id_full);
        chat.codex_loaded(
            &inst,
            &json!({
                "thread": { "id": "t-full", "cwd": workdir, "status": { "type": "idle" } },
                "model": "gpt-6.2-sol",
                "reasoningEffort": "medium",
                "approvalPolicy": "never",
                "sandbox": { "type": "dangerFullAccess" }
            }),
            1,
            None,
        );
        let st = lock(&inst.st);
        assert_eq!(st.mode.as_deref(), Some("fullAccess"));
        assert_eq!(st.model.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(st.effort.as_deref(), Some("high"));
        drop(st);

        // 普通会话从 full_access=true 降成 readOnly 后，重启（新建 Chat 实例）仍保持 readOnly，不回退到 full_access；forget 时清理
        let id_norm = instances
            .create(crate::instances::Input {
                name: Some("norm".into()),
                workdir: Some(workdir.clone()),
                launch: Some("codex".into()),
                chat_ui: Some(true),
                full_access: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .row
            .id;
        chat.setting(&id_norm, "mode", "readOnly").await.unwrap();
        let chat2 = Chat::new(store.clone(), paths, instances);
        let opts2 = chat2.codex_thread_opts(&id_norm);
        assert_eq!(opts2["approvalPolicy"], "on-request");
        assert_eq!(opts2["sandbox"], "read-only");
        chat2.forget(&id_norm);
        assert!(store.get::<String>(&perm_key(&id_norm)).unwrap().is_none());
    }

    /// 回放真录的 Codex app-server 通知（testdata/replay/codex_*.json）：喂进真的 Chat → 看板 → 助理的看守判断 → 送进大脑的事件。
    /// 最后说的话从同一次录下的 rollout 里读（和对话页同一个读法）
    #[tokio::test(flavor = "multi_thread")]
    async fn replay_codex_dispatch_fixtures() {
        for name in ["codex_done.json", "codex_failed_503.json", "codex_failed_unauthorized.json"] {
            let f = crate::gateway::assistant::replay::fixture(name);
            let home = tempfile::tempdir().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let workdir = dir.path().display().to_string();
            let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
            let store = Arc::new(crate::store::Store::memory().unwrap());
            let instances = crate::instances::Instances::new(store.clone(), paths.clone());
            let input = crate::instances::Input { name: Some("c".into()), workdir: Some(workdir.clone()), launch: Some("codex".into()), chat_ui: Some(true), ..Default::default() };
            let id = instances.create(input).await.unwrap().row.id;
            let chat = Chat::new(store, paths, instances);
            chat.codex.generation.store(1, Ordering::SeqCst);
            let inst = chat.inst(&id);
            let tid = f["thread"]["id"].as_str().unwrap();
            chat.codex_loaded(&inst, &json!({ "thread": { "id": tid, "cwd": workdir, "status": { "type": "idle" } }, "model": "gpt-5.5" }), 1, None);

            let mut ro = super::super::codex_rollout::Rollout::new(dir.path().join("rollout.jsonl"));
            let lines: String = f["rollout"].as_array().unwrap().iter().map(|l| format!("{l}\n")).collect();
            let items = match ro.feed(lines.as_bytes()) {
                super::super::transcript::Change::Upsert { items, .. } => items.iter().map(|i| serde_json::to_value(i).unwrap()).collect::<Vec<_>>(),
                _ => vec![],
            };
            let reply = crate::gateway::assistant::replay::reply(&items);

            let mut r = crate::gateway::assistant::replay::Replayer::default();
            let mut now = crate::util::system_ms();
            for n in f["notes"].as_array().unwrap() {
                chat.codex_note(n[0].as_str().unwrap(), &n[1]).await;
                now += 200;
                r.tick(now, &chat.brief(&id).unwrap(), &reply);
            }
            // 结束后看守循环再转几圈（等记录写完、确认不再重复）
            for _ in 0..4 {
                now += 2000;
                r.tick(now, &chat.brief(&id).unwrap(), &reply);
            }
            r.check(name, &f["expect"]);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_state_sync_races_stay_idle() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input { name: Some("c".into()), workdir: Some(workdir.clone()), launch: Some("codex".into()), chat_ui: Some(true), ..Default::default() };
        let id = instances.create(input).await.unwrap().row.id;
        let chat = Chat::new(store, paths, instances);
        chat.codex.generation.store(1, Ordering::SeqCst);
        let inst = chat.inst(&id);
        let tid = "019a0000-0000-7000-8000-000000000001";
        chat.codex_loaded(&inst, &json!({ "thread": { "id": tid, "cwd": workdir, "status": { "type": "idle" } }, "model": "gpt-5.4" }), 1, None);
        assert_eq!(lock(&inst.st).phase, Phase::Idle);

        // 1. 空闲、没有进行中轮次时收到迟到 / 异步的 item/started，不能把会话卡进 Running（“正在处理…”）
        chat.codex_note("item/started", &json!({ "threadId": tid, "turnId": "t-stray", "item": { "id": "i1", "type": "agentMessage", "text": "" } })).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle);

        // 2. 正常一轮：status active → turn/started → status idle → turn/completed（failed 原因保留在看板结果里）
        let seq_before = lock(&inst.st).codex.status_seq;
        chat.codex_note("thread/status/changed", &json!({ "threadId": tid, "status": { "type": "active", "activeFlags": [] } })).await;
        chat.codex_note("turn/started", &json!({ "threadId": tid, "turn": { "id": "t1", "status": "inProgress" } })).await;
        assert_eq!(lock(&inst.st).phase, Phase::Running);
        let done_before = lock(&inst.st).codex.done_seq;
        chat.codex_note("thread/status/changed", &json!({ "threadId": tid, "status": { "type": "systemError" } })).await;
        // turn/completed 紧跟其后：等它带上失败原因再进 Idle
        assert_eq!(lock(&inst.st).phase, Phase::Running);
        chat.codex_note("turn/completed", &json!({ "threadId": tid, "turn": { "id": "t1", "status": "failed", "error": { "message": "boom" } } })).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle);
        assert_eq!(lock(&inst.st).error.as_deref(), Some("boom"));
        assert_eq!(lock(&inst.st).board.turn.as_ref().and_then(|t| t.outcome), Some(super::super::board::Outcome::Failed));

        // 3. turn/start 答复比 turn/completed 晚拿锁：不能把刚结束的 t1 重新标成 Running，也不能冲掉报错
        {
            let mut st = lock(&inst.st);
            Chat::codex_apply_started_turn(&inst, &mut st, &json!({ "turn": { "id": "t1", "status": "inProgress" } }), done_before);
            assert_eq!(st.phase, Phase::Idle);
            assert_eq!(st.codex.turn, None);
            assert_eq!(st.error.as_deref(), Some("boom"));
        }
        // 已完成的轮次再来迟到的 item/started 也不能变回 Running
        chat.codex_note("item/started", &json!({ "threadId": tid, "turnId": "t1", "item": { "id": "i2", "type": "agentMessage", "text": "" } })).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle);

        // 4. 后台 thread/resume 带着旧的 active 快照晚返回：期间已经有过实时状态变化，不能把 Idle 盖回 Running
        chat.codex_loaded(&inst, &json!({ "thread": { "id": tid, "cwd": workdir, "status": { "type": "active" } }, "model": "gpt-5.4" }), 1, Some(seq_before));
        assert_eq!(lock(&inst.st).phase, Phase::Idle);

        // 5. 万一 turn/completed 丢失，只收到了 thread/status/changed(idle)：兜底定时器收回 Idle
        chat.codex_note("turn/started", &json!({ "threadId": tid, "turn": { "id": "t2", "status": "inProgress" } })).await;
        assert_eq!(lock(&inst.st).phase, Phase::Running);
        chat.codex_note("thread/status/changed", &json!({ "threadId": tid, "status": { "type": "idle" } })).await;
        tokio::time::sleep(STATUS_IDLE_SETTLE + Duration::from_millis(120)).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle);
        assert_eq!(lock(&inst.st).codex.turn, None);

        // 6. 请求答复已经把 goal 更新到更高帧序号时，队列里更早帧的通知不能把 goal 闪回旧状态
        lock(&inst.st).codex.set_goal(20, Some(json!({ "objective": "g", "status": "paused" })));
        chat.codex_note_seq(15, "thread/goal/updated", &json!({ "threadId": tid, "goal": { "objective": "g", "status": "active" } })).await;
        assert_eq!(lock(&inst.st).codex.goal.as_ref().and_then(|g| g["status"].as_str()), Some("paused"));
        chat.codex_note_seq(21, "thread/goal/updated", &json!({ "threadId": tid, "goal": { "objective": "g", "status": "complete" } })).await;
        assert_eq!(lock(&inst.st).codex.goal.as_ref().and_then(|g| g["status"].as_str()), Some("complete"));

        // 7. 换到新对话（/new、/fork 等）：旧对话的 /goal、排队插话清掉
        {
            let mut st = lock(&inst.st);
            st.codex.goal = Some(json!({ "objective": "old", "status": "active" }));
            st.codex.steered.push("queued".into());
        }
        let tid2 = "019a0000-0000-7000-8000-000000000002";
        chat.codex_loaded(&inst, &json!({ "thread": { "id": tid2, "cwd": workdir, "status": { "type": "idle" } }, "model": "gpt-5.4" }), 1, None);
        let st = lock(&inst.st);
        assert!(st.codex.goal.is_none());
        assert!(st.codex.steered.is_empty());
        assert_eq!(st.phase, Phase::Idle);
    }

    /// 端到端回归：使用 `/goal` 后点停止（或残留 active goal 重新接上），再发后面的普通任务，普通任务完成后必须回到 `Idle`，不再自动拉起旧 goal 显示“正在处理…”
    #[tokio::test(flavor = "multi_thread")]
    async fn goal_stop_and_subsequent_task_settle_idle() {
        if std::process::Command::new("codex").arg("--version").output().is_err() {
            return;
        }
        use std::sync::atomic::AtomicBool;

        let slow = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicU64::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let slow2 = slow.clone();
        let calls2 = calls.clone();
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |body: axum::body::Bytes| {
                let slow = slow2.clone();
                let calls = calls2.clone();
                async move {
                    let is_title = String::from_utf8_lossy(&body).contains("Generate a concise, single-line task title");
                    let n = if is_title { 0 } else { calls.fetch_add(1, Ordering::SeqCst) + 1 };
                    if !is_title && slow.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    let text = if is_title { r#"{"title":"Test"}"#.to_string() } else { format!("Done #{n}") };
                    let created = json!({ "type": "response.created", "response": { "id": format!("r_{n}"), "status": "in_progress", "output": [] } });
                    let msg = json!({
                        "id": format!("m_{n}"),
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    });
                    let added = json!({ "type": "response.output_item.added", "output_index": 0, "item": { "id": format!("m_{n}"), "type": "message", "role": "assistant", "status": "in_progress", "content": [] } });
                    let done = json!({ "type": "response.output_item.done", "output_index": 0, "item": msg });
                    let completed = json!({ "type": "response.completed", "response": { "id": format!("r_{n}"), "status": "completed", "output": [msg], "usage": { "input_tokens": 10, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens": 5, "output_tokens_details": { "reasoning_tokens": 0 }, "total_tokens": 15 } } });
                    let sse = format!(
                        "event: response.created\ndata: {created}\n\nevent: response.output_item.added\ndata: {added}\n\nevent: response.output_item.done\ndata: {done}\n\nevent: response.completed\ndata: {completed}\n\n"
                    );
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], sse)
                }
            }),
        );
        let api = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let paths = crate::paths::Paths::discover(Some(home.path().to_path_buf())).unwrap();
        let store = Arc::new(crate::store::Store::memory().unwrap());
        let profile = crate::codex_profiles::add(
            &store,
            &paths.home,
            crate::codex_profiles::Input {
                name: Some("Mock".into()),
                base_url: Some(format!("http://127.0.0.1:{port}/v1")),
                api_key: Some("sk-mock".into()),
                model: Some("gpt-5.4".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let instances = crate::instances::Instances::new(store.clone(), paths.clone());
        let input = crate::instances::Input {
            name: Some("c".into()),
            workdir: Some(workdir),
            launch: Some("codex".into()),
            chat_ui: Some(true),
            full_access: Some(true),
            codex_profile: Some(profile.id),
            ..Default::default()
        };
        let id = instances.create(input).await.unwrap().row.id;
        let chat = Chat::new(store, paths, instances);
        let inst = chat.inst(&id);

        // 先聊一句建好记录文件
        chat.codex_input(&id, "hi", &[]).await.unwrap();
        for _ in 0..100 {
            if lock(&inst.st).phase == Phase::Idle && calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(lock(&inst.st).phase, Phase::Idle);

        // 开启 `/goal`，在它运行时点停止：goal 应变成 paused，停止后回到 Idle
        slow.store(true, Ordering::SeqCst);
        chat.codex_input(&id, "/goal keep working", &[]).await.unwrap();
        for _ in 0..100 {
            if lock(&inst.st).phase == Phase::Running && lock(&inst.st).codex.turn.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        chat.codex_interrupt(&id).await.unwrap();
        for _ in 0..100 {
            if lock(&inst.st).phase == Phase::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(lock(&inst.st).phase, Phase::Idle);
        assert_eq!(lock(&inst.st).codex.goal.as_ref().and_then(|g| g["status"].as_str()), Some("paused"), "点停止后 /goal 应暂停");

        // 发后面的普通任务：做完后应稳定停在 Idle，不再因为残留 active goal 自动续跑而显示“正在处理…”
        slow.store(false, Ordering::SeqCst);
        let before = calls.load(Ordering::SeqCst);
        chat.codex_input(&id, "subsequent normal task", &[]).await.unwrap();
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) > before && lock(&inst.st).phase == Phase::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle, "后面的任务完成后不应继续显示处理中");
        assert_eq!(calls.load(Ordering::SeqCst), before + 1, "不应自动多跑一轮旧 goal");

        // 模拟外部（如 TUI 按 Esc 或进程重启前）只中断了轮次、数据库里 goal 仍残留 active：重新 attach 后应自动暂停，后续任务完成后仍为 Idle
        let server = chat.codex_server().await.unwrap();
        let tid = lock(&inst.st).binding.as_ref().unwrap().session_id.clone();
        lock(&chat.codex.threads).remove(&tid);
        {
            let mut st = lock(&inst.st);
            st.codex.generation = 0;
            st.codex.goal = None;
            st.codex.turn = None;
            st.codex.thread_active = false;
            st.phase = Phase::Idle;
        }
        slow.store(true, Ordering::SeqCst);
        server.call("thread/goal/set", json!({ "threadId": tid, "objective": "stranded goal", "status": "active", "origin": "user" })).await.unwrap();
        let mut running_turn = None;
        for _ in 0..100 {
            running_turn = codex_active_turn_id(&server, &tid).await;
            if running_turn.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let running_turn = running_turn.expect("查到分页对话当前在跑的 turn");
        server.call("turn/interrupt", json!({ "threadId": tid, "turnId": running_turn })).await.unwrap();
        for _ in 0..60 {
            if server.call("thread/read", json!({ "threadId": tid })).await.unwrap()["thread"]["status"]["type"] == "idle" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.call("thread/goal/get", json!({ "threadId": tid })).await.unwrap()["goal"]["status"], "active");

        chat.codex_attach(&id).await.unwrap();
        assert_eq!(lock(&inst.st).codex.goal.as_ref().and_then(|g| g["status"].as_str()), Some("paused"), "重连空闲会话时残留 active goal 应自动暂停");

        slow.store(false, Ordering::SeqCst);
        let before2 = calls.load(Ordering::SeqCst);
        chat.codex_input(&id, "another normal task", &[]).await.unwrap();
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) > before2 && lock(&inst.st).phase == Phase::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(lock(&inst.st).phase, Phase::Idle);
        assert_eq!(calls.load(Ordering::SeqCst), before2 + 1);

        if let Some(s) = chat.codex.server.lock().await.take() {
            s.kill();
        }
        api.abort();
    }
}

fn skill_items(list: &[Skill]) -> Value {
    json!({ "items": list.iter().map(|s| json!({ "name": format!("${}", s.name), "description": s.description, "source": format!("codex_{}", s.scope) })).collect::<Vec<_>>() })
}

#[cfg(test)]
mod window_guess_tests {
    use super::*;

    #[test]
    fn guess_follows_config_then_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let mut p = crate::codex_profiles::Profile { id: "builtin".into(), name: String::new(), preset: None, provider_id: None, base_url: None, model: None, models: Default::default(), tuning: Default::default(), created_at: String::new() };
        assert_eq!(codex_window_guess(tmp.path(), &p, "gpt-5.5"), None, "目录里没有：不估");
        std::fs::write(tmp.path().join("models_cache.json"), r#"{"models":[{"slug":"gpt-5.5","context_window":272000,"effective_context_window_percent":95}]}"#).unwrap();
        assert_eq!(codex_window_guess(tmp.path(), &p, "gpt-5.5"), Some((258_400, super::super::models::WindowSource::Catalog)));
        std::fs::write(tmp.path().join("config.toml"), "model_context_window = 400000\n").unwrap();
        assert_eq!(codex_window_guess(tmp.path(), &p, "gpt-5.5").unwrap().0, 380_000);
        p.tuning.context_window = Some(500_000);
        assert_eq!(codex_window_guess(tmp.path(), &p, "gpt-5.5").unwrap().0, 475_000);
    }
}
