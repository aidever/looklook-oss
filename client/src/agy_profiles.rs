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
use crate::model_tuning::AgyTuning;
use crate::store::Store;

/// 本机默认账号（`~/.gemini`）的 ID：一直在，不能删
pub const OFFICIAL: &str = "official";
const LIST_KEY: &str = "agy_profiles";
const DEFAULT_KEY: &str = "agy_profiles.default";
const BUILTIN_KEY: &str = "agy_profiles.builtin";
/// 新建会话时的“自动”：按额度挑一个账号记下（之后一直用它，不按请求轮换）
pub const AUTO: &str = "auto";
const AUTO_SWITCH_KEY: &str = "agy_profiles.auto_switch";
/// 两个窗口（5 小时、每周）都至少剩这么多（%）才算有额度
const ENOUGH_PCT: f64 = 10.0;
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
    /// 模型设置：自动压缩点（model_tuning.rs）。对话模式由看看到点发 /compact，终端里的 agy 带环境变量
    #[serde(default, skip_serializing_if = "AgyTuning::is_empty")]
    pub tuning: AgyTuning,
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
    #[serde(default, skip_serializing_if = "AgyTuning::is_empty")]
    tuning: AgyTuning,
}

fn builtin() -> Profile {
    Profile { id: OFFICIAL.into(), name: String::new(), email: None, plan: None, created_at: String::new(), tuning: AgyTuning::default() }
}

fn builtin_opts(store: &Store) -> BuiltinOpts {
    store.get::<BuiltinOpts>(BUILTIN_KEY).ok().flatten().unwrap_or_default()
}

fn builtin_of(store: &Store) -> Profile {
    let o = builtin_opts(store);
    Profile { name: o.name, email: o.email, plan: o.plan, tuning: o.tuning, ..builtin() }
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
    let p = Profile { id, name, email: None, plan: None, created_at: crate::util::now_rfc3339(), tuning: AgyTuning::default() };
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

/// 模型设置（自动压缩点）；本机默认账号也能设
pub fn set_tuning(store: &Store, id: &str, tuning: AgyTuning) -> Result<Profile, LocalError> {
    let tuning = tuning.clean()?;
    if id == OFFICIAL {
        let mut o = builtin_opts(store);
        o.tuning = tuning;
        store.set(BUILTIN_KEY, &o)?;
        return Ok(builtin_of(store));
    }
    let mut all = stored(store);
    let p = all.iter_mut().find(|p| p.id == id).ok_or_else(LocalError::not_found)?;
    p.tuning = tuning;
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
    quota_forget(id);
    remove_fake_home(home, id);
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

/// agy 账号目录下的本地认证/凭据文件（退出登录时只删这些非软链接文件，不碰共享的技能、MCP、对话记录）
fn auth_files(dir: &Path) -> Vec<PathBuf> {
    let data = dir.join("antigravity-cli");
    vec![
        dir.join("google_accounts.json"),
        dir.join("oauth_creds.json"),
        dir.join("credentials.json"),
        dir.join("auth.json"),
        dir.join("account.json"),
        dir.join("user_status.json"),
        dir.join("entitlement.json"),
        data.join("antigravity-oauth-token"),
        data.join("google_accounts.json"),
        data.join("oauth_creds.json"),
        data.join("credentials.json"),
        data.join("auth.json"),
        data.join("account.json"),
        data.join("user_status.json"),
        data.join("entitlement.json"),
        data.join("auth").join("credentials.json"),
        data.join("auth").join("oauth_creds.json"),
        data.join("auth").join("account.json"),
        dir.join(LOGIN_MARKER),
    ]
}

/// 清除账号目录下的本地登录凭据文件（跳过软链接，绝不删共享目标）
pub(crate) fn clear_dir_auth(dir: &Path) -> std::io::Result<()> {
    for path in auth_files(dir) {
        if is_link(&path) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// 退出登录（包括本机默认账号和额外账号）：取消进行中的登录、清除该账号目录下的本地凭据与登录标记、
/// 清空已缓存的邮箱与套餐信息。不删除账号本身，也不碰共用的技能、MCP 与对话记录。
pub fn logout(store: &Store, home: &Path, id: &str) -> Result<(), LocalError> {
    get(store, id).ok_or_else(LocalError::not_found)?;
    cancel_login(id);
    clear_dir_auth(&account_dir(home, id))?;
    if id == OFFICIAL {
        let mut o = builtin_opts(store);
        o.email = None;
        o.plan = None;
        o.logged_in_at = None;
        store.set(BUILTIN_KEY, &o)?;
    } else {
        let mut all = stored(store);
        if let Some(p) = all.iter_mut().find(|p| p.id == id) {
            p.email = None;
            p.plan = None;
            store.set(LIST_KEY, &all)?;
        }
    }
    Ok(())
}

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
    // agy CLI 实际写入的 OAuth 令牌文件（可能是非 JSON 的不透明文件）：非软链接且非空即认为已登录
    let oauth_token = data.join("antigravity-oauth-token");
    if !is_link(&oauth_token) && std::fs::metadata(&oauth_token).is_ok_and(|m| m.is_file() && m.len() > 0) {
        state.logged_in = true;
    }
    let candidates = [
        // agy 1.5 真正的登录凭据：{ token: { access_token, refresh_token, … }, auth_method, id_token_email }
        oauth_token,
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
                    "id_token_email",
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
                "token.refresh_token",
                "token.access_token",
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

/// 账号目录里登录的邮箱（没登录时 None）
pub fn local_email(home: &Path, id: &str) -> Option<String> {
    inspect_dir_state(&account_dir(home, id)).email
}

/// 检查账号状态：是否安装了 `agy`、本地账号目录里是否有真实登录态、邮箱和套餐。
/// 找不到 `agy` 时带上 `error: "not_installed"` 以便界面复用登录状态展示。
/// 套餐不在本地文件里：用会话里 agy 报过的（[`remember_plan`]）。
pub async fn status(store: &Store, home: &Path, shell: &str, id: &str) -> Value {
    let installed = agy_cli(shell).await.is_some();
    let dir = account_dir(home, id);
    let dir_clone = dir.clone();
    let local = tokio::task::spawn_blocking(move || inspect_dir_state(&dir_clone)).await.unwrap_or_default();
    let saved = get(store, id);
    let mut out = json!({
        "installed": installed,
        "logged_in": local.logged_in,
        "email": local.email.or_else(|| saved.as_ref().and_then(|p| p.email.clone()).filter(|_| local.logged_in)),
        "plan": local.plan.or_else(|| saved.as_ref().and_then(|p| p.plan.clone()).filter(|_| local.logged_in)),
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

// ---------------- 额度：`agy -p /usage` ----------------
// agy 不把额度写到本地文件里，TUI 的 /usage 每次现问服务器。打印模式下 `/usage` 不进模型、不建对话、不花额度，
// `--output-format json` 给结构化的分组：每组（Gemini / Claude 和 GPT）各有每周和 5 小时两个额度。
// 打印模式不认 `--gemini-dir`：额外账号把 HOME 指到一个只有 `.gemini → 账号目录` 的假家目录里。

/// 额度查到后多久内直接用（对话页的用量面板会频繁问）
const QUOTA_FRESH_MS: i64 = 5 * 60_000;
/// 问一次最多等多久（要联网，一般 3～6 秒）
const QUOTA_TIMEOUT: Duration = Duration::from_secs(40);

/// 额度的一格：`{ id, group, window: "weekly" | "5h", used, remaining, resets_at }`（百分比）
pub(crate) fn parse_usage_command(v: &Value) -> Option<Vec<Value>> {
    let groups = v["command"]["data"]["groups"].as_array()?;
    let mut out = Vec::new();
    for g in groups {
        let name = g["name"].as_str().unwrap_or("").trim();
        // “Gemini Models” / “Claude and GPT models” → “Gemini” / “Claude and GPT”
        let group = name.strip_suffix(" models").or_else(|| name.strip_suffix(" Models")).unwrap_or(name).trim().to_string();
        for b in g["buckets"].as_array().into_iter().flatten() {
            let Some(frac) = b["remaining_fraction"].as_f64().filter(|f| f.is_finite()) else { continue };
            let remaining = pct_round(frac * 100.0);
            // 满额时 agy 给的重置时间是 1970-01-01（没有在计时）
            let resets_at = b["reset_time"].as_str().filter(|t| !t.is_empty() && !t.starts_with("1970-")).map(String::from);
            out.push(json!({
                "id": b["id"].as_str().unwrap_or(""),
                "group": group,
                "window": b["window"].as_str().unwrap_or(""),
                "name": format!("{group} · {}", b["name"].as_str().unwrap_or("")),
                "used": pct_round(100.0 - remaining),
                "remaining": remaining,
                "resets_at": resets_at,
            }));
        }
    }
    (!out.is_empty()).then_some(out)
}

fn pct_round(v: f64) -> f64 {
    (v.clamp(0.0, 100.0) * 10.0).round() / 10.0
}

pub(crate) enum QuotaFetch {
    Ok(Vec<Value>),
    LoggedOut,
    Failed(String),
}

/// 额外账号给 `agy -p` 用的假家目录：`<数据目录>/agy-homes/<ID>/.gemini → 账号目录`
fn fake_home(home: &Path, id: &str) -> PathBuf {
    home.join("agy-homes").join(id)
}

fn prepare_fake_home(home: &Path, id: &str) -> std::io::Result<PathBuf> {
    prepare_dir(home, id)?;
    let fh = fake_home(home, id);
    std::fs::create_dir_all(&fh)?;
    let link = fh.join(".gemini");
    if std::fs::symlink_metadata(&link).is_err() {
        make_link(&account_dir(home, id), &link)?;
    }
    Ok(fh)
}

fn remove_fake_home(home: &Path, id: &str) {
    let fh = fake_home(home, id);
    let link = fh.join(".gemini");
    if is_link(&link) || std::fs::symlink_metadata(&link).is_ok_and(|m| cfg!(windows) && m.is_dir()) {
        let _ = unlink(&link);
    }
    // 只删空目录：万一链接没拆掉，也不跟进去删账号文件
    let _ = std::fs::remove_dir(&fh);
}

/// 运行 `agy -p /usage --output-format json` 问这个账号的额度（不花额度、不建对话）
pub(crate) async fn fetch_quota(home: &Path, shell: &str, id: &str) -> QuotaFetch {
    // 账号目录里没有登录凭据：不用问了
    if !inspect_dir_state(&account_dir(home, id)).logged_in {
        return QuotaFetch::LoggedOut;
    }
    let Some(cli) = agy_cli(shell).await else { return QuotaFetch::Failed("not_installed".into()) };
    let program = PathBuf::from(&cli);
    let mut cmd = tokio::process::Command::new(&program);
    if let Some(env) = crate::chat::codex_app::user_env(shell).await {
        cmd.envs(env);
    }
    if let Some(p) = crate::chat::codex_app::launch_path(shell, &program).await {
        cmd.env("PATH", p);
    }
    if id != OFFICIAL {
        match prepare_fake_home(home, id) {
            Ok(fh) => {
                cmd.env("HOME", &fh).env("USERPROFILE", &fh);
            }
            Err(e) => return QuotaFetch::Failed(format!("home: {e}")),
        }
    }
    let run = home.join("run");
    let _ = std::fs::create_dir_all(&run);
    cmd.args(["-p", "/usage", "--output-format", "json"])
        .current_dir(&run)
        // 没登录时 agy 会打印授权地址然后等着读授权码：不给 stdin，让它直接失败
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return QuotaFetch::Failed(e.to_string()),
    };
    // 边读边看：要登录时 agy 打印授权地址后会一直等浏览器回调（关了 stdin 也不退），看到就杀掉
    use tokio::io::AsyncReadExt;
    let (mut so, mut se) = (child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + QUOTA_TIMEOUT;
    let mut buf = [0u8; 8192];
    let mut buf2 = [0u8; 8192];
    let asks_login = |b: &[u8]| {
        let t = String::from_utf8_lossy(b);
        t.contains("Authentication required") || t.contains("accounts.google.com/o/oauth2") || t.contains("not logged in")
    };
    while so.is_some() || se.is_some() {
        tokio::select! {
            n = async { so.as_mut().unwrap().read(&mut buf).await }, if so.is_some() => match n {
                Ok(0) | Err(_) => so = None,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            },
            n = async { se.as_mut().unwrap().read(&mut buf2).await }, if se.is_some() => match n {
                Ok(0) | Err(_) => se = None,
                Ok(n) => err.extend_from_slice(&buf2[..n]),
            },
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                return QuotaFetch::Failed("timeout".into());
            }
        }
        if asks_login(&out) || asks_login(&err) {
            let _ = child.kill().await;
            return QuotaFetch::LoggedOut;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let stdout = String::from_utf8_lossy(&out);
    let all = format!("{stdout}\n{}", String::from_utf8_lossy(&err));
    // 输出前面偶尔会有别的提示行：从第一个 `{` 开始解析
    let parsed = stdout.find('{').and_then(|i| serde_json::from_str::<Value>(stdout[i..].trim()).ok());
    match parsed.as_ref().and_then(parse_usage_command) {
        Some(b) => QuotaFetch::Ok(b),
        None => QuotaFetch::Failed(last_line(&all)),
    }
}

/// 每个账号上次查到的额度：`(查到的时间 ms, 各格)`
static QUOTA: Mutex<Option<HashMap<String, (i64, Vec<Value>)>>> = Mutex::new(None);
/// 正在查的账号（同一个账号不同时起两个 `agy -p`）
static QUOTA_BUSY: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn quota_put(id: &str, buckets: Vec<Value>) {
    QUOTA.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(id.to_string(), (crate::util::system_ms() as i64, buckets));
}

fn quota_forget(id: &str) {
    if let Some(m) = QUOTA.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(id);
    }
}

/// 上次查到的额度（给对话页的用量面板）：`{ groups, fetched_at }`，没查过时 None
pub fn cached_quota(id: &str) -> Option<Value> {
    let g = QUOTA.lock().unwrap_or_else(|e| e.into_inner());
    let (at, b) = g.as_ref()?.get(id)?;
    Some(json!({ "groups": b, "fetched_at": at }))
}

/// 查一次并记下（同一个账号正在查时不再起一个，返回 None）
async fn refresh_quota(home: &Path, shell: &str, id: &str) -> Option<QuotaFetch> {
    if !QUOTA_BUSY.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(id.to_string()) {
        return None;
    }
    let r = fetch_quota(home, shell, id).await;
    if let Some(s) = QUOTA_BUSY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.remove(id);
    }
    match &r {
        QuotaFetch::Ok(b) => quota_put(id, b.clone()),
        QuotaFetch::LoggedOut => quota_forget(id),
        QuotaFetch::Failed(e) => tracing::debug!(profile = id, error = %e, "agy 额度没查到"),
    }
    Some(r)
}

/// 记下的额度过时了（或没有）：后台再查一次
pub fn refresh_quota_soon(home: &Path, id: &str) {
    if cfg!(test) {
        return;
    }
    let stale = {
        let g = QUOTA.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().and_then(|m| m.get(id)).is_none_or(|(at, _)| crate::util::system_ms() as i64 - at > QUOTA_FRESH_MS)
    };
    if stale {
        let (home, id) = (home.to_path_buf(), id.to_string());
        tokio::spawn(async move {
            let shell = crate::shells::login_shell();
            refresh_quota(&home, &shell, &id).await;
        });
    }
}

// ---------------- 多账号：挑账号、额度用完换账号 ----------------
// 照成熟做法（opencode-antigravity-auth 的 sticky、agy-account-manager 的 10% 门槛）：会话开始时挑一个，之后一直用它；
// 只在这一轮因为额度用完失败时才换。不轮换、不代理请求、不替用户发消息去“刷”额度（Google 封号的常见原因）。

/// 模型属于哪一组额度：agy 的 /usage 分 “Gemini” 和 “Claude and GPT” 两组。没指定模型时按 agy 的默认（Gemini）
pub fn gemini_model(model: Option<&str>) -> bool {
    model.filter(|m| !m.is_empty() && *m != "default").is_none_or(|m| m.to_ascii_lowercase().contains("gemini"))
}

/// 这一组额度里最紧的那个窗口还剩多少（%）；没查到这一组时 None
pub(crate) fn headroom(buckets: &[Value], gemini: bool) -> Option<f64> {
    buckets
        .iter()
        .filter(|b| b["group"].as_str().is_some_and(|g| g.to_ascii_lowercase().contains("gemini") == gemini))
        .filter_map(|b| b["remaining"].as_f64())
        .reduce(f64::min)
}

/// 从候选 `(账号, 剩多少)` 里挑：够 10% 的里面剩得最多的；一样多时默认账号优先，再按列表顺序
pub(crate) fn pick_best(cands: &[(String, Option<f64>)], default: &str) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for (id, left) in cands {
        let Some(left) = left.filter(|l| *l >= ENOUGH_PCT) else { continue };
        let better = match &best {
            None => true,
            Some((bid, bl)) => left > *bl || (left == *bl && id == default && bid != default),
        };
        if better {
            best = Some((id.clone(), left));
        }
    }
    best
}

fn logged_in(home: &Path, id: &str) -> bool {
    inspect_dir_state(&account_dir(home, id)).logged_in
}

/// 额度够的账号里剩得最多的那个（只看上次查到的额度，没查过的不算）；`except` 不算在内（额度用完的那个）
pub fn best(store: &Store, home: &Path, gemini: bool, except: Option<&str>) -> Option<(String, f64)> {
    let cands: Vec<(String, Option<f64>)> = list(store)
        .into_iter()
        .filter(|p| Some(p.id.as_str()) != except && logged_in(home, &p.id))
        .map(|p| {
            let left = cached_quota(&p.id).and_then(|q| q["groups"].as_array().and_then(|b| headroom(b, gemini)));
            (p.id, left)
        })
        .collect();
    pick_best(&cands, &default_id(store))
}

/// 新建会话选了“自动”：挑一个记下；都没查到额度（或都不够）时用默认账号
pub fn resolve_auto(store: &Store, home: &Path, model: Option<&str>) -> String {
    refresh_all_soon(store, home);
    best(store, home, gemini_model(model), None).map(|(id, _)| id).unwrap_or_else(|| default_id(store))
}

/// 登录着的账号的额度都过时了就后台再查（`agy -p /usage`，不花额度；5 分钟内查过的不再查）
pub fn refresh_all_soon(store: &Store, home: &Path) {
    for p in list(store) {
        if logged_in(home, &p.id) {
            refresh_quota_soon(home, &p.id);
        }
    }
}

/// 额度用完时自动换到额度够的账号接着（默认关）
pub fn auto_switch(store: &Store) -> bool {
    store.get::<bool>(AUTO_SWITCH_KEY).ok().flatten().unwrap_or(false)
}

pub fn set_auto_switch(store: &Store, on: bool) -> Result<(), LocalError> {
    store.set(AUTO_SWITCH_KEY, &on)?;
    Ok(())
}

/// 这一轮的错误是额度用完：agy 报的是 Google API 的 `RESOURCE_EXHAUSTED` / HTTP 429（agy 自己判断额度错误也是这几样，
/// 见它内置的 `QUOTA_RE`）。不算“rate limit”：那是一时请求太快，等一下就好，不该换账号
pub fn quota_error(why: &str) -> bool {
    let l = why.to_ascii_lowercase();
    l.contains("resource_exhausted")
        || l.contains("resource exhausted")
        || l.contains("exhausted your capacity")
        || l.contains("quota")
        || l.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "429")
}

/// agy 的额度用完多半不是报错：`session/prompt` 照常返回，报错当成这一轮最后一段回复发来（1.5.18 实测：
/// `⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 58m39s.`）。
/// `text` 是这一轮最后一段回复：最后一行是 `⚠` 开头、Google 接口额度用完的说法时，返回 `⚠` 后面的那句。
/// 模型自己写的回复里提到 quota（比如正在改额度相关的代码）不算：要是单独一行、`⚠` 开头、而且就是这一轮的结尾
pub fn quota_reply(text: &str) -> Option<&str> {
    let line = text.trim_end().rsplit('\n').next()?.trim();
    let tail = line.strip_prefix('⚠')?.trim_start_matches('\u{fe0f}').trim();
    let l = tail.to_ascii_lowercase();
    let google = l.contains("quota reached")
        || l.contains("quota exceeded")
        || l.contains("resource_exhausted")
        || l.contains("resource exhausted")
        || l.contains("exhausted your capacity")
        || (l.contains("quota") && l.contains("resets in"));
    (google && quota_error(tail)).then_some(tail)
}

/// 报错里说的多久后重置（秒）：`Resets in 58m39s`、`Resets in 154h46m27s`（Go 的时长写法；也认 `1h 2m`、`2 hours 5 minutes`）。
/// 没写、写得认不出、或者超过一个月（不像是真的）时 None
pub fn reset_after_secs(text: &str) -> Option<u64> {
    let l = text.to_ascii_lowercase();
    let at = l.find("resets in").map(|i| i + "resets in".len()).or_else(|| l.find("reset in").map(|i| i + "reset in".len()))?;
    let s = l[at..].trim_start();
    let b = s.as_bytes();
    let skip_spaces = |mut i: usize| {
        while b.get(i) == Some(&b' ') {
            i += 1;
        }
        i
    };
    let (mut i, mut total) = (0, 0f64);
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        let st = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
            i += 1;
        }
        let n: f64 = s[st..i].trim_end_matches('.').parse().ok()?;
        i = skip_spaces(i);
        let w = i;
        while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
        }
        total += n * match &s[w..i] {
            "ms" | "msec" | "millisecond" | "milliseconds" => 0.001,
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
            "d" | "day" | "days" => 86_400.0,
            _ => return None,
        };
        i = skip_spaces(i);
    }
    (total > 0.0 && total <= 31.0 * 86_400.0).then(|| total.ceil() as u64)
}

/// 用完的那组额度什么时候重置（Unix 毫秒）：这一组里用完了的窗口（每周 / 5 小时）里最晚的那个，都回来了才能接着用
pub(crate) fn reset_of(buckets: &[Value], gemini: bool) -> Option<i64> {
    buckets
        .iter()
        .filter(|b| b["group"].as_str().is_some_and(|g| g.to_ascii_lowercase().contains("gemini") == gemini))
        .filter(|b| b["remaining"].as_f64().is_some_and(|r| r < 1.0))
        .filter_map(|b| b["resets_at"].as_str())
        .filter_map(|t| time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok())
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
        .max()
}

/// 现问一次这个账号的额度（`agy -p /usage`，几秒，不花额度）：各格；没查到时 None。
/// 正在别处查着（额度用完时刚叫过 [`refresh_all_soon`]）：等那次查完用它的结果，不再起一个
pub async fn quota_now(home: &Path, id: &str) -> Option<Vec<Value>> {
    // 测试里不起 agy：用 quota_put_for_test 放的
    let since = if cfg!(test) { 0 } else { crate::util::system_ms() };
    let fresh = || {
        let g = QUOTA.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref()?.get(id).filter(|(at, _)| *at >= since).map(|(_, b)| b.clone())
    };
    if cfg!(test) {
        return fresh();
    }
    let shell = crate::shells::login_shell();
    let deadline = tokio::time::Instant::now() + QUOTA_TIMEOUT + Duration::from_secs(5);
    loop {
        match refresh_quota(home, &shell, id).await {
            Some(QuotaFetch::Ok(b)) => return Some(b),
            Some(_) => return None,
            None if tokio::time::Instant::now() >= deadline => return None,
            None => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(b) = fresh() {
                    return Some(b);
                }
            }
        }
    }
}

/// 测试用：直接放一份额度
#[cfg(test)]
pub(crate) fn quota_put_for_test(id: &str, buckets: Vec<Value>) {
    quota_put(id, buckets);
}

/// 查账号的用量：
/// - 额度：现问 agy（`agy -p /usage`），每组的每周 / 5 小时剩多少、什么时候重置
/// - 本地统计：该账号在 `brain/` 与 `usage_stats` 的 token / 请求 / 估算费用（今日 / 7 天 / 30 天）
/// 返回 `{ state, account, limits, totals }`，与 Looklook 用量界面兼容；没登录时 `state: "logged_out"`。
pub async fn usage(store: &Store, home: &Path, shell: &str, id: &str) -> Value {
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

    let fetched = refresh_quota(home, shell, id).await;
    let logged_out = matches!(fetched, Some(QuotaFetch::LoggedOut));
    // 这次没查成（或正有一个在查）：用上次的
    let quota = cached_quota(id);
    let groups = quota.as_ref().map(|q| q["groups"].clone()).unwrap_or_else(|| json!([]));
    let local_stats = json!({
        "today": today.view(),
        "seven_day": seven.view(),
    });
    let totals = json!({
        "today": today.view(),
        "seven_day": seven.view(),
        "thirty_day": thirty.view(),
    });
    let has_quota = groups.as_array().is_some_and(|g| !g.is_empty());
    let has_local = today.requests > 0 || seven.requests > 0 || thirty.requests > 0;
    let state = if logged_out {
        "logged_out"
    } else if has_quota || has_local {
        "ok"
    } else if matches!(fetched, Some(QuotaFetch::Failed(_))) {
        "failed"
    } else {
        "no_limits"
    };

    let limits = json!({
        "groups": groups,
        "fetched_at": quota.as_ref().map(|q| q["fetched_at"].clone()),
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

/// 会话里 agy 报的套餐（`session/new` 的 `_meta.antigravity.tier.name`，如 “Google AI Pro”）记到账号上
pub fn remember_plan(store: &Store, id: &str, plan: &str) {
    let plan = plan.trim();
    if plan.is_empty() {
        return;
    }
    if id == OFFICIAL {
        let mut o = builtin_opts(store);
        if o.plan.as_deref() != Some(plan) {
            o.plan = Some(plan.to_string());
            let _ = store.set(BUILTIN_KEY, &o);
        }
        return;
    }
    let mut all = stored(store);
    if let Some(p) = all.iter_mut().find(|p| p.id == id).filter(|p| p.plan.as_deref() != Some(plan)) {
        p.plan = Some(plan.to_string());
        let _ = store.set(LIST_KEY, &all);
    }
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
    fn pick_account_by_quota() {
        let b = |g: &str, w: &str, r: f64| json!({ "group": g, "window": w, "remaining": r });
        let buckets = vec![b("Gemini", "5h", 40.0), b("Gemini", "weekly", 12.0), b("Claude and GPT", "5h", 90.0), b("Claude and GPT", "weekly", 80.0)];
        assert_eq!(headroom(&buckets, true), Some(12.0), "两个窗口取紧的");
        assert_eq!(headroom(&buckets, false), Some(80.0));
        assert_eq!(headroom(&[], true), None);
        assert!(gemini_model(None) && gemini_model(Some("gemini-4-neon-high")) && !gemini_model(Some("claude-opus-4-6")));
        let c = |id: &str, l: Option<f64>| (id.to_string(), l);
        assert_eq!(pick_best(&[c("official", Some(9.9)), c("a", Some(50.0)), c("b", Some(70.0))], "official"), Some(("b".into(), 70.0)));
        assert_eq!(pick_best(&[c("official", Some(9.9)), c("a", None)], "official"), None, "不够 10%、没查到的都不挑");
        assert_eq!(pick_best(&[c("a", Some(60.0)), c("official", Some(60.0))], "official").unwrap().0, "official", "一样多时默认优先");
        assert!(quota_error("Internal error: RESOURCE_EXHAUSTED: You have exhausted your capacity"));
        assert!(quota_error("googleapi: Error 429: Too Many Requests"));
        assert!(!quota_error("rate limited, retry later") && !quota_error("port 4290 refused") && !quota_error("session not found"));
    }

    /// agy 1.5.18 额度用完是这一轮的最后一段回复（不是报错）；模型自己说到 quota 的不算
    #[test]
    fn quota_in_reply() {
        let real = "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 58m39s.";
        assert_eq!(quota_reply(&format!("\n⚠ {real}\n")), Some(real));
        assert_eq!(quota_reply(&format!("I'll look at the tests first.\n\n⚠️ {real}")), Some(real), "接在回复后面、带 VS16 的 ⚠️");
        assert_eq!(quota_reply("⚠ RESOURCE_EXHAUSTED: You have exhausted your capacity on this model."), Some("RESOURCE_EXHAUSTED: You have exhausted your capacity on this model."));
        assert_eq!(quota_reply(&format!("⚠ {real}\nBut here is the rest of the answer.")), None, "不在最后");
        assert_eq!(quota_reply(real), None, "不是 ⚠ 开头");
        assert_eq!(quota_reply("Done. The quota check now resets the counter.\n⚠ Remember to run the migrations."), None);
        assert_eq!(quota_reply("⚠ The quota module is untested"), None, "提到 quota 但不是额度用完");
        assert_eq!(quota_reply(""), None);
    }

    #[test]
    fn reset_after() {
        assert_eq!(reset_after_secs("Individual quota reached. Resets in 58m39s."), Some(58 * 60 + 39));
        assert_eq!(reset_after_secs("Resets in 154h46m27s."), Some(154 * 3600 + 46 * 60 + 27));
        assert_eq!(reset_after_secs("resets in 1h 2m"), Some(3720));
        assert_eq!(reset_after_secs("Quota will reset in 2 hours 5 minutes"), Some(2 * 3600 + 5 * 60));
        assert_eq!(reset_after_secs("Resets in 2d3h"), Some(2 * 86_400 + 3 * 3600));
        assert_eq!(reset_after_secs("Resets in 1.5s"), Some(2));
        assert_eq!(reset_after_secs("Resets in 500ms"), Some(1));
        assert_eq!(reset_after_secs("Resets in 0s"), None);
        assert_eq!(reset_after_secs("Resets in 40d"), None, "超过一个月不像真的");
        assert_eq!(reset_after_secs("Resets in a while"), None);
        assert_eq!(reset_after_secs("Resets in 5 fortnights"), None);
        assert_eq!(reset_after_secs("Individual quota reached."), None);
    }

    #[test]
    fn reset_of_exhausted_group() {
        let b = |g: &str, w: &str, r: f64, t: &str| json!({ "group": g, "window": w, "remaining": r, "resets_at": t });
        let ms = |t: &str| (time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).unwrap().unix_timestamp_nanos() / 1_000_000) as i64;
        let buckets = vec![
            b("Gemini", "weekly", 42.2, "2026-12-01T07:10:37Z"),
            b("Gemini", "5h", 0.0, "2026-11-26T05:55:03Z"),
            b("Claude and GPT", "5h", 0.0, "2026-11-26T09:00:00Z"),
            b("Claude and GPT", "weekly", 0.4, "2026-11-30T00:00:00Z"),
        ];
        assert_eq!(reset_of(&buckets, true), Some(ms("2026-11-26T05:55:03Z")), "只看用完的窗口");
        assert_eq!(reset_of(&buckets, false), Some(ms("2026-11-30T00:00:00Z")), "两个窗口都用完：等最晚的");
        assert_eq!(reset_of(&buckets[..1], true), None, "没有用完的");
        assert_eq!(reset_of(&[json!({ "group": "Gemini", "remaining": 0.0 })], true), None, "没写什么时候重置");
    }

    #[test]
    fn auto_resolves_to_best_logged_in() {
        let (dir, store) = tmp_store("auto");
        let a = add(&store, &dir, Some("a")).unwrap();
        let b = add(&store, &dir, Some("b")).unwrap();
        for p in [&a, &b] {
            std::fs::create_dir_all(account_dir(&dir, &p.id).join("antigravity-cli")).unwrap();
            std::fs::write(account_dir(&dir, &p.id).join("antigravity-cli/antigravity-oauth-token"), r#"{"token":{"access_token":"x","refresh_token":"y"}}"#).unwrap();
        }
        let q = |g: f64, c: f64| vec![json!({ "group": "Gemini", "window": "5h", "remaining": g }), json!({ "group": "Claude and GPT", "window": "5h", "remaining": c })];
        quota_put_for_test(&a.id, q(30.0, 95.0));
        quota_put_for_test(&b.id, q(80.0, 5.0));
        assert_eq!(resolve_auto(&store, &dir, Some("gemini-4-neon-high")), b.id);
        assert_eq!(resolve_auto(&store, &dir, Some("claude-sonnet-4-6")), a.id);
        assert_eq!(best(&store, &dir, true, Some(&b.id)).unwrap().0, a.id, "换账号时不算用完的那个");
        assert!(!auto_switch(&store));
        set_auto_switch(&store, true).unwrap();
        assert!(auto_switch(&store));
    }

    #[test]
    fn tuning_per_account() {
        let (dir, store) = tmp_store("tuning");
        let a = add(&store, &dir, Some("a")).unwrap();
        let t = AgyTuning { compact_pct: Some(80), ..Default::default() };
        assert_eq!(set_tuning(&store, OFFICIAL, t.clone()).unwrap().tuning, t);
        assert_eq!(get(&store, OFFICIAL).unwrap().tuning, t, "本机默认账号也存得住");
        assert!(get(&store, &a.id).unwrap().tuning.is_empty(), "各账号分开");
        set_tuning(&store, &a.id, AgyTuning { compact_tokens: Some(600_000), ..Default::default() }).unwrap();
        assert_eq!(get(&store, &a.id).unwrap().tuning.vars()[0].1, "600000");
        assert!(set_tuning(&store, &a.id, AgyTuning { compact_pct: Some(10), ..Default::default() }).is_err());
        assert!(set_tuning(&store, "nope", AgyTuning::default()).is_err());
        // 旧数据没有 tuning 字段也读得出
        assert!(update(&store, OFFICIAL, Some("me")).unwrap().tuning == t);
    }

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
    fn local_state_usage_command_and_brain_usage() {
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

        // agy 1.5 真正的凭据文件（antigravity-cli/antigravity-oauth-token，没有扩展名）
        let acct2 = dir.join("agy-accounts/u2");
        std::fs::create_dir_all(acct2.join("antigravity-cli")).unwrap();
        std::fs::write(
            acct2.join("antigravity-cli/antigravity-oauth-token"),
            r#"{"token":{"access_token":"ya29.x","token_type":"Bearer","refresh_token":"1//x","expiry":"2026-11-25T20:41:10-05:00","expires_in":3599},"auth_method":"consumer","id_token_email":"bob@example.com"}"#,
        )
        .unwrap();
        let st2 = inspect_dir_state(&acct2);
        assert!(st2.logged_in);
        assert_eq!(st2.email.as_deref(), Some("bob@example.com"));

        // `agy -p /usage --output-format json` 的输出（1.5.17 实测）
        let out = json!({ "conversation_id": "", "status": "SUCCESS", "command": { "name": "usage", "data": { "groups": [
            { "name": "Gemini Models", "buckets": [
                { "id": "gemini-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0.4219624698162079, "reset_time": "2026-12-01T07:10:37Z" },
                { "id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 0.9997, "reset_time": "2026-11-26T05:55:03Z" }
            ] },
            { "name": "Claude and GPT models", "buckets": [
                { "id": "3p-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 1, "reset_time": "1970-01-01T00:00:00Z" }
            ] }
        ] } } });
        let b = parse_usage_command(&out).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!((b[0]["group"].as_str(), b[0]["window"].as_str()), (Some("Gemini"), Some("weekly")));
        assert_eq!((b[0]["used"].as_f64(), b[0]["remaining"].as_f64()), (Some(57.8), Some(42.2)));
        assert_eq!(b[0]["resets_at"], "2026-12-01T07:10:37Z");
        assert_eq!(b[2]["group"], "Claude and GPT");
        assert_eq!(b[2]["used"], 0.0);
        assert!(b[2]["resets_at"].is_null(), "满额时 1970 年的重置时间不算");
        assert!(parse_usage_command(&json!({ "response": "x" })).is_none());

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

    #[test]
    fn oauth_token_detection_and_logout_cleanup() {
        let (dir, store) = tmp_store("logout");
        let a = add(&store, &dir, Some("Work")).unwrap();
        let acct = account_dir(&dir, &a.id);

        // 只有 agy 自己的不透明令牌文件时，也能识别为已登录
        std::fs::create_dir_all(acct.join("antigravity-cli")).unwrap();
        std::fs::write(acct.join("antigravity-cli/antigravity-oauth-token"), "opaque-token-bytes").unwrap();
        assert!(inspect_dir_state(&acct).logged_in);

        // 再写登录标记与邮箱缓存，退出登录后凭据与缓存均被清空，但账号本身保留
        write_login_marker(&acct, Some("work@example.com"), Some("Pro"));
        sync_after_login(&store, &dir, &a.id);
        assert_eq!(get(&store, &a.id).unwrap().email.as_deref(), Some("work@example.com"));

        logout(&store, &dir, &a.id).unwrap();
        assert!(!inspect_dir_state(&acct).logged_in);
        assert!(!acct.join("antigravity-cli/antigravity-oauth-token").exists());
        assert!(!acct.join(LOGIN_MARKER).exists());
        let after = get(&store, &a.id).unwrap();
        assert_eq!(after.name, "Work");
        assert!(after.email.is_none());
        assert!(after.plan.is_none());

        // 不存在的账号返回 not_found
        assert!(logout(&store, &dir, "nonexistent").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
