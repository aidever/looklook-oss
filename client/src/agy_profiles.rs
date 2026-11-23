//! Antigravity（`agy`）账号管理与切换、登录、本地状态与用量查询。
//!
//! 规则与 Claude Code 账号切换（`claude_profiles.rs`）一致：
//! - **本机默认账号**（[`OFFICIAL`] = `"official"`）：使用 `~/.gemini` + `~/.gemini/antigravity-cli`，
//!   始终存在且不能删除（可以改名）。启动 `agy acp` 时不额外加 `--gemini-dir`（终端里的 `agy` 没法换账号目录，一直用这个）。
//! - **另外添加的 Google 账号**：目录在 `<数据目录>/agy-accounts/<ID>/`：
//!   技能、MCP 配置、对话记录等（见 [`SHARED`]）用软链接与默认目录共用，
//!   登录凭据与账号状态各自独立，互不影响。
//! - **登录**：后台运行 `agy acp --login`（另外的账号带 `--gemini-dir <目录>`），
//!   解析 stdout/stderr 里的 Google OAuth 授权地址（`https://accounts.google.com/...`），
//!   把用户粘贴的授权码或完整回调链接写回 stdin；看看不读、不存、不转发令牌。
//! - **会话绑定**：新建会话时记下用的账号（kv `instance_agy_profile.<实例ID>`），
//!   之后切换默认不影响已有会话。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::LocalError;
use crate::store::Store;

/// 本机默认账号（`~/.gemini`）的 ID：一直在，不能删
pub const OFFICIAL: &str = "official";
const LIST_KEY: &str = "agy_profiles";
const DEFAULT_KEY: &str = "agy_profiles.default";
const BUILTIN_KEY: &str = "agy_profiles.builtin";
const NAME_MAX: usize = 40;
const MAX_PROFILES: usize = 30;
/// 一次登录最长等多久（用户去浏览器里登录）
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// 另外的账号与默认 `~/.gemini` 共用的目录/文件：`(相对路径, 默认目录没有时是否先建目录)`。
/// 认证/账号状态文件不共用，保证每个账号登录隔离。
const SHARED: &[(&str, bool)] = &[
    ("config/skills", false),
    ("config/mcp_config.json", false),
    ("antigravity-cli/brain", true),
    ("antigravity-cli/conversations", true),
    ("antigravity-cli/conversation_summaries.db", false),
    ("antigravity-cli/builtin", false),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    /// 用户起的名字；本机默认账号为空（界面显示“本机默认账号”）；
    /// 另外的账号未填名字时，登录成功后自动填为邮箱
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BuiltinOpts {
    #[serde(default)]
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logged_in_at: Option<String>,
}

fn builtin() -> Profile {
    Profile { id: OFFICIAL.into(), name: String::new(), email: None, plan: None, created_at: String::new() }
}

fn builtin_opts(store: &Store) -> BuiltinOpts {
    store.get::<BuiltinOpts>(BUILTIN_KEY).ok().flatten().unwrap_or_default()
}

fn builtin_of(store: &Store) -> Profile {
    let o = builtin_opts(store);
    Profile { name: o.name, email: o.email, plan: o.plan, ..builtin() }
}

fn stored(store: &Store) -> Vec<Profile> {
    store.get::<Vec<Profile>>(LIST_KEY).ok().flatten().unwrap_or_default()
}

/// 本机默认的 `~/.gemini` 目录
pub fn official_gemini_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".gemini")
}

/// 账号对应的 gemini 目录：`"official"` 为 `~/.gemini`，其余为 `<home>/agy-accounts/<id>`
pub fn account_dir(home: &Path, id: &str) -> PathBuf {
    if id == OFFICIAL {
        official_gemini_dir()
    } else {
        home.join("agy-accounts").join(id)
    }
}

/// 列出全部账号：本机默认账号固定在最前面
pub fn list(store: &Store) -> Vec<Profile> {
    let mut v = vec![builtin_of(store)];
    v.extend(stored(store));
    v
}

pub fn get(store: &Store, id: &str) -> Option<Profile> {
    list(store).into_iter().find(|p| p.id == id)
}

/// 当前默认账号 ID；记下的那个被删了时回到 `"official"`
pub fn default_id(store: &Store) -> String {
    let all = list(store);
    store
        .get::<String>(DEFAULT_KEY)
        .ok()
        .flatten()
        .filter(|id| all.iter().any(|p| &p.id == id))
        .unwrap_or_else(|| OFFICIAL.into())
}

pub fn set_default(store: &Store, id: &str) -> Result<(), LocalError> {
    get(store, id).ok_or_else(LocalError::not_found)?;
    store.set(DEFAULT_KEY, &id.to_string())?;
    Ok(())
}

fn clean_name(s: Option<&str>) -> Result<String, LocalError> {
    let name: String = s.unwrap_or("").trim().chars().filter(|c| !c.is_control()).take(NAME_MAX).collect();
    if name.is_empty() {
        return Err(LocalError::invalid("name", "required"));
    }
    Ok(name)
}

/// 添加一个新的 Google 账号并准备独立目录。`name` 可不填（登录成功后有邮箱时自动填上）。
pub fn add(store: &Store, home: &Path, name: Option<&str>) -> Result<Profile, LocalError> {
    let mut all = stored(store);
    if all.len() >= MAX_PROFILES {
        return Err(LocalError::invalid("profiles", "too_many"));
    }
    let id = loop {
        let id = crate::util::random_id(8);
        if all.iter().all(|p| p.id != id) && id != OFFICIAL {
            break id;
        }
    };
    let name = clean_name(name).unwrap_or_default();
    let p = Profile { id, name, email: None, plan: None, created_at: crate::util::now_rfc3339() };
    prepare_dir(home, &p.id)?;
    all.push(p.clone());
    store.set(LIST_KEY, &all)?;
    Ok(p)
}

/// 修改账号显示名（本机默认账号清空时恢复默认显示名）
pub fn update(store: &Store, id: &str, name: Option<&str>) -> Result<Profile, LocalError> {
    if id == OFFICIAL {
        let mut o = builtin_opts(store);
        if let Some(name) = name {
            o.name = clean_name(Some(name)).unwrap_or_default();
            store.set(BUILTIN_KEY, &o)?;
        }
        return Ok(builtin_of(store));
    }
    let mut all = stored(store);
    let p = all.iter_mut().find(|p| p.id == id).ok_or_else(LocalError::not_found)?;
    if let Some(name) = name {
        p.name = clean_name(Some(name))?;
    }
    let out = p.clone();
    store.set(LIST_KEY, &all)?;
    Ok(out)
}

/// 删除额外账号和它的隔离目录（不删共享目标里的任何文件）。本机默认账号不能删除。
pub fn remove(store: &Store, home: &Path, id: &str) -> Result<(), LocalError> {
    if id == OFFICIAL {
        return Err(LocalError::invalid("id", "builtin"));
    }
    cancel_login(id);
    let mut all = stored(store);
    let before = all.len();
    all.retain(|p| p.id != id);
    if all.len() == before {
        return Err(LocalError::not_found());
    }
    store.set(LIST_KEY, &all)?;
    if store.get::<String>(DEFAULT_KEY).ok().flatten().as_deref() == Some(id) {
        let _ = store.remove(DEFAULT_KEY);
    }
    remove_dir(&account_dir(home, id))?;
    Ok(())
}

// ---------------- 会话绑定 ----------------

fn pin_key(inst_id: &str) -> String {
    format!("instance_agy_profile.{inst_id}")
}

/// 会话记下的账号 ID（没记下时 None）
pub fn pinned(store: &Store, inst_id: &str) -> Option<String> {
    store.get::<String>(&pin_key(inst_id)).ok().flatten()
}

/// 新建会话时记下用哪个账号（`None` 或空串用当前默认）
pub fn pin(store: &Store, inst_id: &str, profile: Option<&str>) -> Result<(), LocalError> {
    let id = match profile.filter(|s| !s.is_empty()) {
        Some(id) => get(store, id).ok_or_else(|| LocalError::invalid("agy_profile", "not_found"))?.id,
        None => default_id(store),
    };
    store.set(&pin_key(inst_id), &id)?;
    Ok(())
}

pub fn unpin(store: &Store, inst_id: &str) {
    let _ = store.remove(&pin_key(inst_id));
}

/// 这个会话用的账号（被删了或没记下时回落到当前默认）
pub fn for_instance(store: &Store, inst_id: &str) -> Profile {
    pinned(store, inst_id)
        .and_then(|id| get(store, &id))
        .or_else(|| get(store, &default_id(store)))
        .unwrap_or_else(|| builtin_of(store))
}

// ---------------- 启动参数 ----------------
// 只有 `agy acp` 有 `--gemini-dir`；终端里的 `agy` 没有这个参数（ANTIGRAVITY_GEMINI_DIR、GEMINI_DIR 也不认，1.5.14 实测），终端会话一直用本机默认账号

/// 启动 `agy acp` 时要附加的参数：额外账号返回 `["--gemini-dir", "<dir>"]`，本机默认账号返回空。
pub fn acp_args(home: &Path, profile: &Profile) -> Vec<String> {
    if profile.id == OFFICIAL {
        return vec![];
    }
    if let Err(e) = prepare_dir(home, &profile.id) {
        tracing::warn!(error = %e, profile = %profile.id, "准备 Antigravity 账号目录失败");
    }
    vec!["--gemini-dir".into(), account_dir(home, &profile.id).display().to_string()]
}

// ---------------- 隔离目录与软链接 ----------------

/// 准备额外账号的独立 gemini 目录，并把共享的技能、MCP、对话记录软链到默认 `~/.gemini`。
pub fn prepare_dir(home: &Path, id: &str) -> std::io::Result<()> {
    if id == OFFICIAL {
        return Ok(());
    }
    prepare_dir_from(&official_gemini_dir(), &account_dir(home, id))
}

/// 以 `main` 为共享源，初始化独立账号目录 `dir`（便于单元测试隔离运行）
pub(crate) fn prepare_dir_from(main: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("config"))?;
    std::fs::create_dir_all(dir.join("antigravity-cli"))?;
    for &(rel, create_dir) in SHARED {
        let target = main.join(rel);
        let link = dir.join(rel);
        if std::fs::symlink_metadata(&link).is_ok() {
            // 已链接或 agy 自己建了文件：不动
            continue;
        }
        if !target.exists() {
            if !create_dir {
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&target) {
                tracing::debug!(error = %e, item = rel, "无法创建默认的 Antigravity 共享目录");
                continue;
            }
        }
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(e) = make_link(&target, &link) {
            tracing::debug!(error = %e, item = rel, "无法链接到默认的 Antigravity 配置");
        }
    }
    Ok(())
}

/// 删账号目录：先拆掉共享软链接（绝不跟进去删 `~/.gemini` 里的文件），再删目录里剩下的认证/状态文件。
fn remove_dir(dir: &Path) -> std::io::Result<()> {
    for &(rel, _) in SHARED {
        let p = dir.join(rel);
        if is_link(&p) {
            unlink(&p)?;
        }
    }
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink() || (cfg!(windows) && m.is_dir() && std::fs::read_link(p).is_ok()))
}

#[cfg(unix)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        use std::os::windows::process::CommandExt;
        let st = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .creation_flags(0x0800_0000)
            .output()?;
        return if st.status.success() { Ok(()) } else { Err(std::io::Error::other(String::from_utf8_lossy(&st.stderr).trim().to_string())) };
    }
    std::os::windows::fs::symlink_file(target, link).or_else(|_| std::fs::copy(target, link).map(|_| ()))
}

#[cfg(not(any(unix, windows)))]
fn make_link(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

fn unlink(p: &Path) -> std::io::Result<()> {
    if cfg!(windows) && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
        std::fs::remove_dir(p)
    } else {
        std::fs::remove_file(p)
    }
}

// ---------------- 查找 agy CLI ----------------

/// 在登录 shell 的 PATH 或官方默认安装位置找 `agy`
pub async fn agy_cli(shell: &str) -> Option<String> {
    crate::tools::detect(shell).await.agy.or_else(crate::agents::agy_fallback)
}

// ---------------- 账号状态检查 ----------------

#[derive(Debug, Default)]
pub(crate) struct LocalAccountState {
    logged_in: bool,
    email: Option<String>,
    plan: Option<String>,
}

/// 本机登录成功记录文件（不含任何令牌，仅记成功时间，供 agy 把令牌放进系统钥匙串或不透明文件时识别）
const LOGIN_MARKER: &str = ".looklook-agy-login.json";

fn write_login_marker(dir: &Path, email: Option<&str>, plan: Option<&str>) {
    let _ = std::fs::create_dir_all(dir);
    let body = json!({
        "logged_in_at": crate::util::now_rfc3339(),
        "email": email,
        "plan": plan,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&body) {
        let _ = crate::util::write_private(&dir.join(LOGIN_MARKER), &bytes);
    }
}

/// 从 JWT（如 Google OAuth `id_token`）的 payload 段提取 `email` 声明（只读，不验签、不暴露令牌）
fn email_from_jwt(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["email"].as_str().filter(|s| s.contains('@')).map(String::from)
}

fn non_empty_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        let mut cur = v;
        let mut ok = true;
        for part in k.split('.') {
            cur = &cur[part];
            if cur.is_null() {
                ok = false;
                break;
            }
        }
        if ok {
            if let Some(s) = cur.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(s);
            }
        }
    }
    None
}

/// 只读检查账号目录下的本地状态文件（绝不伪造凭据、绝不返回令牌内容）
pub(crate) fn inspect_dir_state(dir: &Path) -> LocalAccountState {
    let mut state = LocalAccountState::default();
    let data = dir.join("antigravity-cli");
    let candidates = [
        dir.join("google_accounts.json"),
        dir.join("oauth_creds.json"),
        dir.join("credentials.json"),
        dir.join("auth.json"),
        dir.join("account.json"),
        dir.join("user_status.json"),
        dir.join("entitlement.json"),
        dir.join("config").join("settings.json"),
        data.join("google_accounts.json"),
        data.join("oauth_creds.json"),
        data.join("credentials.json"),
        data.join("auth.json"),
        data.join("account.json"),
        data.join("user_status.json"),
        data.join("entitlement.json"),
        data.join("state.json"),
        data.join("auth").join("credentials.json"),
        data.join("auth").join("oauth_creds.json"),
        data.join("auth").join("account.json"),
        dir.join(LOGIN_MARKER),
    ];

    for path in candidates {
        // 跳过指向默认目录的软链接，防止额外账号误读主账号状态
        if is_link(&path) {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else { continue };

        if state.email.is_none() {
            state.email = non_empty_str(
                &v,
                &[
                    "active",
                    "email",
                    "accountEmail",
                    "userEmail",
                    "user.email",
                    "account.email",
                    "userStatus.email",
                    "oauthAccount.emailAddress",
                ],
            )
            .filter(|s| s.contains('@'))
            .map(String::from)
            .or_else(|| v["id_token"].as_str().and_then(email_from_jwt));
        }
        if state.plan.is_none() {
            state.plan = non_empty_str(
                &v,
                &[
                    "plan",
                    "planName",
                    "tier",
                    "userTier",
                    "subscriptionTier",
                    "subscriptionType",
                    "userStatus.userTier.name",
                    "userStatus.planStatus.planInfo.planName",
                    "entitlement.tier",
                    "entitlement.plan",
                ],
            )
            .map(String::from);
        }
        let has_cred = non_empty_str(
            &v,
            &[
                "refresh_token",
                "access_token",
                "id_token",
                "token",
                "logged_in_at",
                "active",
                "oauthAccount.emailAddress",
            ],
        )
        .is_some();
        if has_cred || state.email.is_some() {
            state.logged_in = true;
        }
    }

    state
}

/// 检查账号状态：是否安装了 `agy`、本地账号目录里是否有真实登录态、邮箱和套餐。
/// 找不到 `agy` 时带上 `error: "not_installed"` 以便界面复用登录状态展示。
pub async fn status(home: &Path, shell: &str, id: &str) -> Value {
    let installed = agy_cli(shell).await.is_some();
    let dir = account_dir(home, id);
    let dir_clone = dir.clone();
    let local = tokio::task::spawn_blocking(move || inspect_dir_state(&dir_clone)).await.unwrap_or_default();
    let mut out = json!({
        "installed": installed,
        "logged_in": local.logged_in,
        "email": local.email,
        "plan": local.plan,
        "dir": dir.display().to_string(),
    });
    if !installed {
        out["error"] = json!("not_installed");
    }
    out
}

// ---------------- 额度与本地用量统计 ----------------

#[derive(Debug, Default, Clone)]
struct TokenTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    requests: u64,
    cost: f64,
}

impl TokenTotals {
    fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read
    }

    fn view(&self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "tokens": self.tokens(),
            "requests": self.requests,
            "cost": (self.cost * 10000.0).round() / 10000.0,
        })
    }
}

/// 去掉 agy 模型名末尾的思考强度后缀，匹配 models.dev 价格表
fn strip_effort(m: &str) -> &str {
    const LEVELS: [&str; 6] = ["-minimal", "-low", "-medium", "-high", "-xhigh", "-max"];
    LEVELS.iter().find_map(|l| m.strip_suffix(l)).unwrap_or(m)
}

fn estimate_cost(cat: Option<&crate::pricing::Catalog>, model: &str, input: u64, output: u64, cache_read: u64) -> f64 {
    let Some(c) = cat else { return 0.0 };
    let clean = strip_effort(model);
    let Some((cost, _)) = crate::pricing::price(c, Some("google"), clean).or_else(|| crate::pricing::guess_price(c, clean)) else {
        return 0.0;
    };
    let p = cost.at(input + cache_read);
    let read = p.cache_read.unwrap_or(p.input);
    (input as f64 * p.input + output as f64 * p.output + cache_read as f64 * read) / 1_000_000.0
}

/// 扫描某个账号目录下 `antigravity-cli/brain/*/.system_generated/logs/transcript.jsonl`，
/// 统计今天、近 7 天、近 30 天的真实 token、请求数与估算费用。
fn scan_local_brain(store: &Store, dir: &Path, id: &str, now_secs: i64) -> (TokenTotals, TokenTotals, TokenTotals) {
    let mut today = TokenTotals::default();
    let mut seven = TokenTotals::default();
    let mut thirty = TokenTotals::default();
    let brain = dir.join("antigravity-cli").join("brain");
    let Ok(rd) = std::fs::read_dir(&brain) else {
        return (today, seven, thirty);
    };
    let conv_map: HashMap<String, String> = store.get("usage_convs").ok().flatten().unwrap_or_default();
    let my_key = format!("agy:{id}");
    let cat = crate::pricing::catalog();
    let today_day = now_secs.div_euclid(86_400);
    let first_7d = today_day - 6;
    let first_30d = today_day - 29;
    let mut seen: HashSet<String> = HashSet::new();

    for entry in rd.flatten() {
        let conv = entry.file_name().to_string_lossy().into_owned();
        // 共享 brain/ 时按对话归属过滤：明确归属到其他 agy 账号的跳过；未标记的归属到本机默认账号
        match conv_map.get(&conv).map(String::as_str) {
            Some(k) if k.starts_with("agy:") && k != my_key => continue,
            None if id != OFFICIAL => continue,
            _ => {}
        }
        let log = entry.path().join(".system_generated").join("logs").join("transcript.jsonl");
        let Ok(text) = std::fs::read_to_string(&log) else { continue };
        for line in text.lines() {
            if !line.contains("\"input_tokens\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let Some(model) = v["model"].as_str().filter(|m| !m.is_empty()) else { continue };
            let Some(ts) = v["created_at"].as_str() else { continue };
            let Ok(dt) = time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339) else { continue };
            let day = dt.unix_timestamp().div_euclid(86_400);
            if day < first_30d || day > today_day {
                continue;
            }
            let dedup = format!("{conv}:{}:{ts}", v["step_index"]);
            if !seen.insert(dedup) {
                continue;
            }
            let inp = v["input_tokens"].as_u64().unwrap_or(0);
            let out = v["output_tokens"].as_u64().unwrap_or(0);
            let cr = v["cache_read_tokens"].as_u64().unwrap_or(0);
            let cost = estimate_cost(cat.as_deref(), model, inp, out, cr);
            let add = |t: &mut TokenTotals| {
                t.input += inp;
                t.output += out;
                t.cache_read += cr;
                t.requests += 1;
                t.cost += cost;
            };
            add(&mut thirty);
            if day >= first_7d {
                add(&mut seven);
            }
            if day == today_day {
                add(&mut today);
            }
        }
    }
    (today, seven, thirty)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct QuotaLimits {
    pub five_hour: Option<Value>,
    pub seven_day: Option<Value>,
    pub groups: Vec<Value>,
}

fn pct_round(v: f64) -> f64 {
    (v.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

fn parse_window(v: &Value) -> Option<Value> {
    if !v.is_object() {
        return None;
    }
    let used = v["used"]
        .as_f64()
        .or_else(|| v["usedPercent"].as_f64())
        .or_else(|| v["used_percent"].as_f64())
        .or_else(|| v["remainingFraction"].as_f64().or_else(|| v["remaining_fraction"].as_f64()).map(|r| (1.0 - r) * 100.0))?;
    if !used.is_finite() {
        return None;
    }
    let resets_at = non_empty_str(v, &["resets_at", "resetTime", "reset_time", "resetsAt"]).map(String::from);
    Some(json!({ "used": pct_round(used), "resets_at": resets_at }))
}

fn parse_group_item(id_hint: Option<&str>, v: &Value) -> Option<Value> {
    if !v.is_object() {
        return None;
    }
    let q = if v["quotaInfo"].is_object() { &v["quotaInfo"] } else { v };
    let (used, remaining) = if let Some(u) = q["used"].as_f64().or_else(|| q["usedPercent"].as_f64()).or_else(|| q["used_percent"].as_f64()) {
        let u = pct_round(u);
        (u, pct_round(100.0 - u))
    } else if let Some(rem) = q["remaining"].as_f64().or_else(|| q["remainingPercent"].as_f64()).or_else(|| q["remaining_percent"].as_f64()) {
        let rem = pct_round(rem);
        (pct_round(100.0 - rem), rem)
    } else if let Some(frac) = q["remainingFraction"].as_f64().or_else(|| q["remaining_fraction"].as_f64()) {
        let rem = pct_round(frac * 100.0);
        (pct_round(100.0 - rem), rem)
    } else {
        return None;
    };
    let id = non_empty_str(v, &["id", "groupId", "modelId", "model", "name"])
        .or(id_hint)
        .unwrap_or("default")
        .to_string();
    let label = non_empty_str(v, &["label", "displayName", "name", "id"])
        .or(id_hint)
        .unwrap_or(&id)
        .to_string();
    let resets_at = non_empty_str(q, &["resets_at", "resetTime", "reset_time", "resetsAt"]).map(String::from);
    Some(json!({
        "id": id,
        "label": label,
        "used": used,
        "remaining": remaining,
        "resets_at": resets_at,
    }))
}

/// 从本地 quota / entitlement 缓存 JSON 解析额度窗口与模型分组额度（宽松解析，无则返回 None）
pub(crate) fn parse_quota_cache(v: &Value) -> Option<QuotaLimits> {
    if !v.is_object() {
        return None;
    }
    let mut out = QuotaLimits::default();
    let root = if v["limits"].is_object() { &v["limits"] } else { v };
    out.five_hour = parse_window(&root["five_hour"]).or_else(|| parse_window(&root["fiveHour"]));
    out.seven_day = parse_window(&root["seven_day"]).or_else(|| parse_window(&root["sevenDay"]));

    // 数组形式：groups / buckets / modelQuotas / cascadeModelConfigData.clientModelConfigs
    let array_sources = [
        root.get("groups"),
        root.get("buckets"),
        root.get("modelQuotas"),
        root.get("model_quotas"),
        v.get("groups"),
        v.get("buckets"),
        v.get("cascadeModelConfigData").and_then(|c| c.get("clientModelConfigs")),
        v.get("userStatus").and_then(|u| u.get("cascadeModelConfigData")).and_then(|c| c.get("clientModelConfigs")),
    ];
    for arr in array_sources.into_iter().flatten().filter_map(Value::as_array) {
        for item in arr {
            if let Some(g) = parse_group_item(None, item) {
                if !out.groups.iter().any(|x| x["id"] == g["id"]) {
                    out.groups.push(g);
                }
            }
        }
    }
    // 对象映射形式：models: { "gemini-pro": { remainingFraction: 0.8, resetTime: "..." } }
    for map in [root.get("models"), v.get("models")].into_iter().flatten().filter_map(Value::as_object) {
        for (k, item) in map {
            if let Some(g) = parse_group_item(Some(k), item) {
                if !out.groups.iter().any(|x| x["id"] == g["id"]) {
                    out.groups.push(g);
                }
            }
        }
    }

    if out.five_hour.is_some() || out.seven_day.is_some() || !out.groups.is_empty() {
        Some(out)
    } else {
        None
    }
}

fn load_local_quota(dir: &Path) -> Option<QuotaLimits> {
    let data = dir.join("antigravity-cli");
    let candidates = [
        dir.join("quota.json"),
        dir.join("quota_cache.json"),
        dir.join("entitlement.json"),
        dir.join("user_status.json"),
        data.join("quota.json"),
        data.join("quota_cache.json"),
        data.join("entitlement.json"),
        data.join("user_status.json"),
        data.join("cache").join("quota.json"),
        data.join("state").join("quota.json"),
    ];
    for path in candidates {
        if is_link(&path) {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&raw) else { continue };
        if let Some(q) = parse_quota_cache(&v) {
            return Some(q);
        }
    }
    None
}

/// 查账号的本地用量与额度缓存：
/// - 汇总该账号在 `brain/` 与 `usage_stats` 的真实本地 token / 请求 / 估算费用（今日 / 7 天 / 30 天）
/// - 解析账号目录下若存在的本地 quota / entitlement 缓存（含 `five_hour` / `seven_day` / `groups`）
/// 返回 `{ state, account, limits, totals }`，与 Looklook 用量界面兼容。
pub async fn usage(store: &Store, home: &Path, id: &str) -> Value {
    let profile = get(store, id).unwrap_or_else(builtin);
    let dir = account_dir(home, id);
    let local_state = inspect_dir_state(&dir);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let (mut today, mut seven, thirty) = scan_local_brain(store, &dir, id, now);

    // 若 brain/ 已被清理但 usage_stats 里留着历史统计，默认账号可以兜底读 usage_stats
    if id == OFFICIAL && seven.requests == 0 {
        let rep = crate::usage_stats::report(store, 7, 0, None);
        if rep["ready"] == true {
            if let Some(a) = rep["agents"].as_array().and_then(|arr| arr.iter().find(|x| x["agent"] == "agy")) {
                seven.input = a["input"].as_u64().unwrap_or(0);
                seven.output = a["output"].as_u64().unwrap_or(0);
                seven.cache_read = a["cache_read"].as_u64().unwrap_or(0);
                seven.requests = a["requests"].as_u64().unwrap_or(0);
                seven.cost = a["cost"].as_f64().unwrap_or(0.0);
            }
            if let Some(t) = rep["days"].as_array().and_then(|d| d.last()).and_then(|d| d["agents"].get("agy")) {
                today.input = t["input"].as_u64().unwrap_or(0);
                today.output = t["output"].as_u64().unwrap_or(0);
                today.cache_read = t["cache_read"].as_u64().unwrap_or(0);
                today.requests = t["requests"].as_u64().unwrap_or(0);
                today.cost = t["cost"].as_f64().unwrap_or(0.0);
            }
        }
    }

    let quota = load_local_quota(&dir).unwrap_or_default();
    let local_stats = json!({
        "today": today.view(),
        "seven_day": seven.view(),
    });
    let totals = json!({
        "today": today.view(),
        "seven_day": seven.view(),
        "thirty_day": thirty.view(),
    });
    let has_quota = quota.five_hour.is_some() || quota.seven_day.is_some() || !quota.groups.is_empty();
    let has_local = today.requests > 0 || seven.requests > 0 || thirty.requests > 0;
    let state = if has_quota || has_local { "ok" } else { "no_limits" };

    let limits = json!({
        "five_hour": quota.five_hour,
        "seven_day": quota.seven_day,
        "groups": quota.groups,
        "local_stats": local_stats,
    });
    let account = json!({
        "id": profile.id,
        "name": profile.name,
        "email": local_state.email.or(profile.email),
        "plan": local_state.plan.or(profile.plan),
        "dir": dir.display().to_string(),
    });

    json!({
        "state": state,
        "account": account,
        "limits": limits,
        "totals": totals,
    })
}

// ---------------- 交互式 OAuth 登录 ----------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct LoginView {
    /// `running` / `ok` / `failed`
    pub state: &'static str,
    /// Google OAuth 授权页地址（`agy acp --login` 打印出来之前为 None）
    pub url: Option<String>,
    /// 失败时的错误说明
    pub error: Option<String>,
    /// 已经把授权码或回调链接交给 `agy`，正在等待换令牌
    pub code_sent: bool,
}

struct Login {
    view: Mutex<LoginView>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    cancel: tokio::sync::Notify,
}

static LOGINS: Mutex<Option<HashMap<String, Arc<Login>>>> = Mutex::new(None);

fn login_slot(id: &str) -> Option<Arc<Login>> {
    LOGINS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(id).cloned())
}

/// 发起登录：后台运行 `agy acp --login`（额外账号带 `--gemini-dir <dir>`），
/// 解析 Google OAuth 授权链接并等待输入授权码或回调 URL。
pub async fn start_login(home: &Path, shell: &str, store: Arc<Store>, id: &str) -> Result<LoginView, LocalError> {
    let profile = get(&store, id).ok_or_else(LocalError::not_found)?;
    cancel_login(&profile.id);
    let cli = agy_cli(shell).await.ok_or_else(|| LocalError::new("AGENT_NOT_FOUND").with("agent", "agy"))?;
    let dir = account_dir(home, &profile.id);
    let dir_str = dir.display().to_string();
    let mut args: Vec<&str> = vec!["acp", "--login"];
    if profile.id != OFFICIAL {
        prepare_dir(home, &profile.id)?;
        args.push("--gemini-dir");
        args.push(&dir_str);
    }
    let env: Vec<(&str, Option<&std::ffi::OsStr>)> = vec![("BROWSER", Some(std::ffi::OsStr::new("looklook-no-browser")))];
    let mut cmd = crate::agents::cli_command(shell, &cli, &args, &env);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| LocalError::new("AGENT_NOT_FOUND").with("agent", "agy").with("detail", e.to_string()))?;

    let login = Arc::new(Login {
        view: Mutex::new(LoginView { state: "running", ..Default::default() }),
        stdin: tokio::sync::Mutex::new(child.stdin.take()),
        cancel: tokio::sync::Notify::new(),
    });
    LOGINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(profile.id.clone(), login.clone());

    let out = Arc::new(Mutex::new(String::new()));
    let readers: Vec<_> = [
        child.stdout.take().map(|r| Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|r| Box::new(r) as _),
    ]
    .into_iter()
    .flatten()
    .map(|mut r| {
        let (out, login) = (out.clone(), login.clone());
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
                o.push_str(&String::from_utf8_lossy(&buf[..n]));
                if o.len() > 64 * 1024 {
                    let cut = o.len() - 32 * 1024;
                    let cut = (cut..o.len()).find(|i| o.is_char_boundary(*i)).unwrap_or(o.len());
                    o.drain(..cut);
                }
                let mut v = login.view.lock().unwrap_or_else(|e| e.into_inner());
                if v.url.is_none() {
                    v.url = auth_url(&o);
                }
            }
        })
    })
    .collect();

    let task_login = login.clone();
    let (home_owned, profile_id) = (home.to_path_buf(), profile.id.clone());
    tokio::spawn(async move {
        let exit = tokio::select! {
            s = child.wait() => s.ok(),
            _ = task_login.cancel.notified() => None,
            _ = tokio::time::sleep(LOGIN_TIMEOUT) => None,
        };
        if exit.is_none() {
            let _ = child.kill().await;
        }
        for r in readers {
            let _ = r.await;
        }
        task_login.stdin.lock().await.take();
        let ok = exit.is_some_and(|s| s.success());
        if ok {
            sync_after_login(&store, &home_owned, &profile_id);
        }
        let mut v = task_login.view.lock().unwrap_or_else(|e| e.into_inner());
        if ok {
            v.state = "ok";
        } else {
            v.state = "failed";
            v.error = exit.map(|_| last_line(&out.lock().unwrap_or_else(|e| e.into_inner())));
        }
    });

    for _ in 0..80 {
        let v = login.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if v.url.is_some() || v.state != "running" {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let v = login.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(v)
}

/// 登录成功后：记录登录标记并把本地发现的邮箱/套餐同步到 Profile（未命名的额外账号自动以邮箱命名）
fn sync_after_login(store: &Store, home: &Path, id: &str) {
    let dir = account_dir(home, id);
    let local = inspect_dir_state(&dir);
    write_login_marker(&dir, local.email.as_deref(), local.plan.as_deref());
    if id == OFFICIAL {
        let mut o = builtin_opts(store);
        if local.email.is_some() {
            o.email = local.email;
        }
        if local.plan.is_some() {
            o.plan = local.plan;
        }
        o.logged_in_at = Some(crate::util::now_rfc3339());
        let _ = store.set(BUILTIN_KEY, &o);
        return;
    }
    let mut all = stored(store);
    if let Some(p) = all.iter_mut().find(|p| p.id == id) {
        if let Some(email) = local.email {
            if p.name.is_empty() {
                p.name = email.chars().take(NAME_MAX).collect();
            }
            p.email = Some(email);
        }
        if let Some(plan) = local.plan {
            p.plan = Some(plan);
        }
        if let Err(e) = store.set(LIST_KEY, &all) {
            tracing::warn!(error = %e, "无法保存 Antigravity 账号信息");
        }
    }
}

pub fn login_state(id: &str) -> Option<LoginView> {
    login_slot(id).map(|l| l.view.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// 把用户粘贴的授权码或完整回调 URL 交给 `agy acp --login` 的 stdin（不保存、不写日志）
pub async fn submit_login_code(id: &str, code: &str) -> Result<LoginView, LocalError> {
    let clean: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.is_empty() || clean.len() > 4096 || clean.chars().any(char::is_control) {
        return Err(LocalError::invalid("code", "invalid"));
    }
    let login = login_slot(id).ok_or_else(|| LocalError::invalid("code", "not_running"))?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = login.stdin.lock().await;
        let w = stdin.as_mut().ok_or_else(|| LocalError::invalid("code", "not_running"))?;
        w.write_all(format!("{clean}\n").as_bytes())
            .await
            .map_err(|_| LocalError::invalid("code", "not_running"))?;
        let _ = w.flush().await;
    }
    let mut v = login.view.lock().unwrap_or_else(|e| e.into_inner());
    v.code_sent = true;
    Ok(v.clone())
}

pub fn cancel_login(id: &str) {
    if let Some(l) = LOGINS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(id)) {
        l.cancel.notify_one();
    }
}

/// 从 `agy acp --login` 输出里提取 OAuth 授权地址：优先 `https://accounts.google.com/...`，其次含 oauth/auth 的 https 链接。
pub fn auth_url(out: &str) -> Option<String> {
    let clean = crate::claude_profiles::strip_ansi(out);
    let mut fallback: Option<String> = None;
    let mut rest = clean.as_str();
    while let Some(i) = rest.find("https://") {
        let tail = &rest[i..];
        let end = tail.find(|c: char| c.is_whitespace() || c.is_control()).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',', ')', ']', ';']);
        if url.starts_with("https://accounts.google.com/") {
            return Some(url.to_string());
        }
        if fallback.is_none() && (url.contains("oauth") || url.contains("/auth?") || url.contains("login")) {
            fallback = Some(url.to_string());
        }
        rest = &tail[end..];
    }
    fallback
}

/// 失败时提取最后一行有意义的提示（去掉 ANSI 控制序列和 URL）
fn last_line(out: &str) -> String {
    let clean = crate::claude_profiles::strip_ansi(out);
    let line = clean
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.contains("https://") && !l.starts_with("Then paste "))
        .unwrap_or("");
    let line = line.rfind("> ").map_or(line, |i| &line[i + 2..]);
    line.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!("ll-agy-prof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn crud_default_and_pin() {
        let (dir, store) = tmp_store("crud");
        assert_eq!(default_id(&store), OFFICIAL);
        assert_eq!(list(&store).len(), 1);
        assert_eq!(list(&store)[0].id, OFFICIAL);

        // 本机默认账号不能删，可以改名和清空恢复
        assert!(remove(&store, &dir, OFFICIAL).is_err());
        let renamed = update(&store, OFFICIAL, Some("  个人 Google  ")).unwrap();
        assert_eq!(renamed.name, "个人 Google");
        assert_eq!(get(&store, OFFICIAL).unwrap().name, "个人 Google");
        assert_eq!(update(&store, OFFICIAL, Some("")).unwrap().name, "");

        // 添加额外账号（允许空名或自定义名）
        let a = add(&store, &dir, Some("Work")).unwrap();
        assert_eq!(a.name, "Work");
        assert_eq!(account_dir(&dir, &a.id), dir.join("agy-accounts").join(&a.id));
        assert!(account_dir(&dir, &a.id).is_dir());

        let b = add(&store, &dir, None).unwrap();
        assert_eq!(b.name, "");
        assert_eq!(list(&store).len(), 3);

        // 改名校验
        assert!(update(&store, &b.id, Some("   ")).is_err());
        let b2 = update(&store, &b.id, Some("Backup")).unwrap();
        assert_eq!(b2.name, "Backup");

        // 默认切换与会话绑定互不影响
        pin(&store, "inst-1", None).unwrap();
        assert_eq!(pinned(&store, "inst-1").as_deref(), Some(OFFICIAL));
        set_default(&store, &a.id).unwrap();
        assert_eq!(default_id(&store), a.id);
        pin(&store, "inst-2", None).unwrap();
        pin(&store, "inst-3", Some(&b.id)).unwrap();
        assert!(pin(&store, "inst-bad", Some("nonexistent")).is_err());

        assert_eq!(for_instance(&store, "inst-1").id, OFFICIAL);
        assert_eq!(for_instance(&store, "inst-2").id, a.id);
        assert_eq!(for_instance(&store, "inst-3").id, b.id);
        assert_eq!(for_instance(&store, "unpinned").id, a.id);

        // 启动参数与环境变量
        assert!(acp_args(&dir, &get(&store, OFFICIAL).unwrap()).is_empty());
        let args = acp_args(&dir, &a);
        assert_eq!(args, vec!["--gemini-dir", &account_dir(&dir, &a.id).display().to_string()]);

        // 删除默认账号回落到 official；删除后对应目录也被清理
        remove(&store, &dir, &a.id).unwrap();
        assert_eq!(default_id(&store), OFFICIAL);
        assert_eq!(for_instance(&store, "inst-2").id, OFFICIAL);
        assert!(!account_dir(&dir, &a.id).exists());
        assert!(remove(&store, &dir, &a.id).is_err());

        unpin(&store, "inst-3");
        assert!(pinned(&store, "inst-3").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_dir_symlinks_and_safe_cleanup() {
        let (dir, _store) = tmp_store("symlinks");
        let main = dir.join("main-gemini");
        std::fs::create_dir_all(main.join("config/skills/s1")).unwrap();
        std::fs::write(main.join("config/mcp_config.json"), r#"{"mcpServers":{}}"#).unwrap();
        std::fs::create_dir_all(main.join("antigravity-cli/builtin")).unwrap();
        std::fs::write(main.join("antigravity-cli/conversation_summaries.db"), "db").unwrap();
        std::fs::write(main.join("oauth_creds.json"), r#"{"refresh_token":"main-secret"}"#).unwrap();

        let acct = dir.join("agy-accounts/acc1");
        prepare_dir_from(&main, &acct).unwrap();

        // 共享内容均已软链，缺失的 brain/ 与 conversations/ 自动在 main 创建后软链
        for &(rel, _) in SHARED {
            assert!(is_link(&acct.join(rel)), "{rel} 应该是软链接");
        }
        assert!(main.join("antigravity-cli/brain").is_dir());
        assert!(main.join("antigravity-cli/conversations").is_dir());

        // 凭据文件不被共享
        assert!(!acct.join("oauth_creds.json").exists());
        std::fs::write(acct.join("oauth_creds.json"), r#"{"email":"extra@example.com"}"#).unwrap();

        // 删除额外账号目录后，主目录的共享文件和目录完好无损
        remove_dir(&acct).unwrap();
        assert!(!acct.exists());
        assert!(main.join("config/skills/s1").is_dir());
        assert!(main.join("config/mcp_config.json").is_file());
        assert!(main.join("antigravity-cli/brain").is_dir());
        assert!(main.join("antigravity-cli/conversation_summaries.db").is_file());
        assert!(main.join("oauth_creds.json").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_url_and_error_extraction() {
        let real_output = "\
Open this link in a browser to log in with Google:

  https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=1071006060591.apps.googleusercontent.com&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&response_type=code&state=abc123

Then paste the authorization code, or the full URL of the page you land on, and press Enter:

login cancelled: no authorization code was entered
";
        assert_eq!(
            auth_url(real_output).as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=1071006060591.apps.googleusercontent.com&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&response_type=code&state=abc123")
        );
        assert_eq!(last_line(real_output), "login cancelled: no authorization code was entered");

        // 带 OSC-8 终端超链接也能提取
        let osc = "Visit \u{1b}]8;;https://accounts.google.com/o/oauth2/auth?state=x\u{7}https://accounts.google.com/o/oauth2/auth?state=x\u{1b}]8;;\u{7}\n";
        assert_eq!(auth_url(osc).as_deref(), Some("https://accounts.google.com/o/oauth2/auth?state=x"));

        // 兜底其他 OAuth 链接，忽略普通网址
        assert_eq!(auth_url("Docs: https://example.com/help\nAuth: https://auth.example.com/oauth/start?x=1").as_deref(), Some("https://auth.example.com/oauth/start?x=1"));
        assert_eq!(auth_url("Docs: https://example.com/help"), None);
    }

    #[test]
    fn local_state_and_quota_and_brain_usage() {
        let (dir, store) = tmp_store("state_quota");
        let acct = dir.join("agy-accounts/u1");
        std::fs::create_dir_all(acct.join("antigravity-cli/brain/conv-1/.system_generated/logs")).unwrap();

        // 未登录时 logged_in 为 false
        let empty = inspect_dir_state(&acct);
        assert!(!empty.logged_in && empty.email.is_none());

        // 写入本地状态 + quota 缓存 + transcript
        std::fs::write(acct.join("google_accounts.json"), r#"{"active":"alice@example.com","tier":"Pro"}"#).unwrap();
        let st = inspect_dir_state(&acct);
        assert!(st.logged_in);
        assert_eq!(st.email.as_deref(), Some("alice@example.com"));
        assert_eq!(st.plan.as_deref(), Some("Pro"));

        let quota_json = json!({
            "five_hour": { "used": 23.4, "resets_at": "2026-11-22T18:00:00Z" },
            "groups": [
                { "id": "gemini-pro", "label": "Gemini Pro", "remainingFraction": 0.75, "resetTime": "2026-11-23T00:00:00Z" },
                { "id": "claude", "label": "Claude", "used": 60.0 }
            ]
        });
        let parsed = parse_quota_cache(&quota_json).unwrap();
        assert_eq!(parsed.five_hour.unwrap()["used"], 23.4);
        assert_eq!(parsed.groups.len(), 2);
        assert_eq!(parsed.groups[0]["id"], "gemini-pro");
        assert_eq!(parsed.groups[0]["used"], 25.0);
        assert_eq!(parsed.groups[0]["remaining"], 75.0);
        assert_eq!(parsed.groups[1]["remaining"], 40.0);

        // 本地 brain transcript 聚合（今天与 3 天前）
        let now = time::OffsetDateTime::now_utc();
        let now_s = now.format(&time::format_description::well_known::Rfc3339).unwrap();
        let old_s = (now - time::Duration::days(3)).format(&time::format_description::well_known::Rfc3339).unwrap();
        let lines = [
            json!({ "step_index": 1, "type": "PLANNER_RESPONSE", "created_at": now_s, "model": "gemini-4-neon-high", "input_tokens": 1000, "cache_read_tokens": 500, "output_tokens": 200 }).to_string(),
            json!({ "step_index": 2, "type": "PLANNER_RESPONSE", "created_at": old_s, "model": "gemini-4-neon-low", "input_tokens": 2000, "cache_read_tokens": 0, "output_tokens": 300 }).to_string(),
        ]
        .join("\n");
        std::fs::write(acct.join("antigravity-cli/brain/conv-1/.system_generated/logs/transcript.jsonl"), lines).unwrap();
        // 标记该对话属于 agy:u1
        store.set("usage_convs", &HashMap::from([("conv-1".to_string(), "agy:u1".to_string())])).unwrap();
        let (today, seven, thirty) = scan_local_brain(&store, &acct, "u1", now.unix_timestamp());
        assert_eq!((today.requests, today.tokens()), (1, 1700));
        assert_eq!((seven.requests, seven.tokens()), (2, 4000));
        assert_eq!((thirty.requests, thirty.tokens()), (2, 4000));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
