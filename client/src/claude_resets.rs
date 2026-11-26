//! Claude Code 的限额重置卡（`/limit-reset`）：在后台开一个看不见的终端运行用户自己的 `claude`，替用户敲命令、读屏幕。
//!
//! 看看不拿令牌去问 Anthropic（重置卡的查询、使用接口都是 Claude Code 内部的）：请求都是 Claude Code 自己发的，
//! 和用户在终端里敲 `/limit-reset fable` 一样。终端用 termd 的 PTY + 终端内核（不经 tmux，Windows 也一样）。
//!
//! Claude Code 2.1.332 里这个命令的行为（从它的代码里看到的）：
//! - 隐藏命令，账号开了重置卡（GrowthBook 开关 `tengu_cedar_ember`）或老的“每周一次重置 5 小时”（`tengu_nifty_lemur`）才有，
//!   都没开时回 `Unknown command: /limit-reset`（实测）。
//! - **不带参数时可能直接用掉**每周一次的 5 小时重置（老功能，或者新功能里没有别的卡时），不弹确认。
//!   所以查询时只在输入 `/limit-reset `（带空格）后出现参数提示 `[fable|full|5-hour|weekly]`（只有新功能开着时才有）
//!   才带上种类回车；带了种类时它只会弹确认框（我们按 Esc，卡留着）或回一句说明，不会直接用。
//! - 卡的种类：`fable`（把这周 Fable 用掉的还回周限额）、`full`（全部重置）、`5-hour`、`weekly`。
//! - 确认框：标题 `Use a {kind}?`，选项 `Yes, use my {kind}` / `No, keep it`；刚弹出时有一小段时间不收按键。
//!
//! 对话记录：用 `--session-id` 起，跑完删掉它的记录文件，不在历史里留下这个后台会话。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use term_core::{AlacrittyCore, CoreConfig, TerminalCore};
use term_proto::types::CoreEvent;
use termd::pty::{PlatformPty, PtyEvent};
use tokio::sync::mpsc::UnboundedReceiver;

/// 参数里能写的种类（Claude Code 认的名字），按它排的顺序
pub const KINDS: [&str; 4] = ["fable", "full", "5-hour", "weekly"];
/// 新功能开着时，输入 `/limit-reset ` 后出现的参数提示
const ARG_HINT: &str = "[fable|full|5-hour|weekly]";
/// 老功能（每周一次重置 5 小时）的命令说明
const LEGACY_DESC: &str = "Reset your 5-hour limit now";
const COLS: u16 = 220;
const ROWS: u16 = 60;
/// 查询结果留多久（打开用量面板时不用每次都开终端）
const STATUS_TTL: Duration = Duration::from_secs(10 * 60);

// ---------------- 后台终端 ----------------

struct Tui {
    pty: PlatformPty,
    core: AlacrittyCore,
    rx: UnboundedReceiver<PtyEvent>,
    closed: bool,
}

impl Tui {
    fn spawn(cmd: String, args: Vec<String>, cwd: &Path, env: Vec<(String, String)>) -> anyhow::Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let req = term_proto::CreateSessionReq {
            id: "limit-reset".into(),
            cmd,
            args,
            cwd: cwd.display().to_string(),
            env,
            env_remove: vec!["TMUX".into(), "TMUX_PANE".into(), "CLAUDECODE".into(), "CLAUDE_CODE_ENTRYPOINT".into()],
            cols: COLS,
            rows: ROWS,
        };
        let pty = PlatformPty::spawn(&req, tx)?;
        let core = AlacrittyCore::new(CoreConfig { cols: COLS, rows: ROWS, ..CoreConfig::default() }).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self { pty, core, rx, closed: false })
    }

    fn send(&self, s: &str) {
        let _ = self.pty.write_all(s.as_bytes());
    }

    /// 收输出喂给终端内核（终端查询的回答写回去），最多等 `dur`
    async fn pump(&mut self, dur: Duration) {
        let end = tokio::time::Instant::now() + dur;
        loop {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(PtyEvent::Output(b))) => {
                    self.core.feed_bytes(&b);
                    for ev in self.core.take_events() {
                        if let CoreEvent::PtyWrite(reply) = ev {
                            let _ = self.pty.write_all(&reply);
                        }
                    }
                }
                Ok(Some(PtyEvent::Closed { .. })) | Ok(None) => {
                    self.closed = true;
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn screen(&mut self) -> String {
        self.core.flush_sync();
        term_core::ansi::render_plain_text(&[], &self.core.snapshot())
    }

    /// 等屏幕满足条件（每 200 毫秒看一次），超时返回 None
    async fn wait_for(&mut self, timeout: Duration, mut pred: impl FnMut(&str) -> bool) -> Option<String> {
        let start = Instant::now();
        loop {
            self.pump(Duration::from_millis(200)).await;
            let s = self.screen();
            if pred(&s) {
                return Some(s);
            }
            if self.closed || start.elapsed() > timeout {
                tracing::debug!(screen = %s, "后台 Claude Code 终端没等到");
                return None;
            }
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.pty.terminate();
    }
}

// ---------------- 读屏幕 ----------------

/// 输入框就绪：有 `❯` 开头的输入行，上下都是横线
fn ready(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    lines.iter().enumerate().any(|(i, l)| l.trim_start().starts_with('❯') && i > 0 && is_rule(lines[i - 1]) && lines.get(i + 1).is_some_and(|n| is_rule(n)))
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 20 && t.chars().all(|c| c == '─')
}

/// 启动时要用户处理的界面：没登录（选登录方式）、信任目录
fn blocker(screen: &str) -> Option<&'static str> {
    let low = screen.to_lowercase();
    if low.contains("select login method") || low.contains("/login") && low.contains("not logged in") {
        return Some("logged_out");
    }
    if low.contains("trust this folder") || low.contains("i trust this folder") {
        return Some("trust");
    }
    None
}

/// 敲过的命令（`❯ /limit-reset fable` 那一行）后面 Claude Code 回的话：到下一条横线（输入框）为止，去掉 `⎿`、`●`，连成一行
fn answer_after(screen: &str, echo: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let at = lines.iter().rposition(|l| {
        let t = l.trim_start().trim_start_matches(['❯', '>']).trim();
        t == echo
    })?;
    let mut out: Vec<String> = vec![];
    for l in &lines[at + 1..] {
        if is_rule(l) {
            break;
        }
        let t = l.trim().trim_start_matches(['⎿', '●']).trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    let s = out.join(" ");
    (!s.is_empty()).then_some(s)
}

/// 确认框：标题（`Use a … ?`）到选项之间的几行 + 光标在不在“用”那一项上
struct Confirm {
    title: String,
    lines: Vec<String>,
    on_yes: bool,
}

fn confirm_box(screen: &str) -> Option<Confirm> {
    let lines: Vec<&str> = screen.lines().collect();
    let yes = lines.iter().rposition(|l| l.contains("Yes, use my"))?;
    let title = lines[..yes].iter().rposition(|l| {
        let t = l.trim().trim_matches('│').trim();
        t.starts_with("Use a") && t.ends_with('?')
    })?;
    let clean = |l: &str| l.trim().trim_matches('│').trim().to_string();
    let body: Vec<String> = lines[title + 1..yes].iter().map(|l| clean(l)).filter(|t| !t.is_empty() && !t.chars().all(|c| c == '─' || c == '╌')).collect();
    Some(Confirm { title: clean(lines[title]), lines: body, on_yes: lines[yes].contains('❯') })
}

/// Claude Code 说法里的种类名 → 参数名
fn kind_of(text: &str) -> Option<&'static str> {
    let low = text.to_lowercase();
    if low.contains("fable reset") {
        Some("fable")
    } else if low.contains("full reset") {
        Some("full")
    } else if low.contains("5-hour reset") || low.contains("five-hour reset") {
        Some("5-hour")
    } else if low.contains("weekly reset") {
        Some("weekly")
    } else {
        None
    }
}

/// `… · you can use a 5-hour reset or a full reset`：还能用哪几种
fn can_use(text: &str) -> Option<Vec<&'static str>> {
    let i = text.find("you can use ")?;
    let rest = &text[i + "you can use ".len()..];
    let v: Vec<&'static str> = rest.split([',', '·']).flat_map(|p| p.split(" or ")).filter_map(kind_of).collect();
    Some(v)
}

/// 用了之后 Claude Code 回的话 → `reset`（用掉了）/ `kept`（什么都没用）/ `unknown`
fn outcome(text: &str) -> &'static str {
    let low = text.to_lowercase();
    if ["nothing was used", "nothing changed", "already reset on its own", "your limits are clear", " kept ·", "this one wasn't used"].iter().any(|p| low.contains(p)) {
        "kept"
    } else if ["your weekly reset day stays", "next reset available", "reset used", " used · you're still at", "limits reset", "limit reset ·"].iter().any(|p| low.contains(p)) {
        "reset"
    } else {
        "unknown"
    }
}

// ---------------- 对外 ----------------

/// 本地缓存的开关（`.claude.json` 的 `cachedGrowthBookFeatures`，不联网）：`cards`（重置卡）/ `legacy`（每周一次 5 小时）/ `off`
fn flags_mode(state_file: &Path) -> &'static str {
    let v: Value = std::fs::read(state_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let f = &v["cachedGrowthBookFeatures"];
    if f["tengu_cedar_ember"]["enabled"] == true {
        "cards"
    } else if f["tengu_nifty_lemur"]["enabled"] == true {
        "legacy"
    } else {
        "off"
    }
}

struct Ctx {
    shell: String,
    home: PathBuf,
    config_dir: PathBuf,
    state_file: PathBuf,
    /// 本机默认账号时为 None（不改 CLAUDE_CONFIG_DIR）
    account_dir: Option<PathBuf>,
    profile: crate::claude_profiles::Profile,
}

impl Ctx {
    fn new(shell: &str, home: &Path, p: &crate::claude_profiles::Profile) -> Self {
        let (config_dir, state_file) = crate::claude_profiles::config_paths(home, p);
        let account_dir = (p.id != crate::claude_profiles::OFFICIAL).then(|| crate::claude_profiles::account_dir(home, &p.id));
        Self { shell: shell.into(), home: home.into(), config_dir, state_file, account_dir, profile: p.clone() }
    }
}

/// 一个账号同时只开一个后台终端
fn busy() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn cache() -> &'static Mutex<HashMap<PathBuf, (Instant, Value)>> {
    static C: std::sync::OnceLock<Mutex<HashMap<PathBuf, (Instant, Value)>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// 开着的后台终端；用完删掉这次的对话记录
struct Session {
    tui: Tui,
    id: String,
    config_dir: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        // 先关终端（Tui 的 Drop 在这之后，这里先让它退出），再删记录
        self.tui.pty.terminate();
        if let Some(p) = crate::chat::claude_sdk::find_transcript(std::slice::from_ref(&self.config_dir), &self.id) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 打开后台的 Claude Code，等到输入框出来。`Err(state)`：`not_installed` / `logged_out` / `failed`
async fn open(ctx: &Ctx) -> Result<Session, &'static str> {
    let Some(cli) = crate::agents::claude_cli(&ctx.shell).await else { return Err("not_installed") };
    let cwd = ctx.home.join("run").join("limit-reset");
    let _ = std::fs::create_dir_all(&cwd);
    // 信任目录、首次启动的主题选择等先写好，不弹窗
    let _ = crate::claude_profiles::skip_first_run(&ctx.home, &ctx.profile, &cwd);
    let id = crate::chat::claude_sdk::uuid_v4();
    let claude_args = vec!["--strict-mcp-config".to_string(), "--session-id".into(), id.clone()];
    let (cmd, args) = if cfg!(windows) {
        let p = Path::new(&cli);
        let cli = match p.with_extension("cmd") {
            c if p.extension().is_none() && c.is_file() => c,
            _ => p.to_path_buf(),
        };
        if cli.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat")) {
            let mut a = vec!["/d".to_string(), "/c".into(), cli.display().to_string()];
            a.extend(claude_args);
            ("cmd.exe".to_string(), a)
        } else {
            (cli.display().to_string(), claude_args)
        }
    } else {
        let mut a = vec!["-lc".to_string(), "exec \"$0\" \"$@\"".into(), cli.clone()];
        a.extend(claude_args);
        (ctx.shell.clone(), a)
    };
    let mut env = vec![("DISABLE_AUTOUPDATER".to_string(), "1".to_string())];
    if let Some(d) = &ctx.account_dir {
        env.push(("CLAUDE_CONFIG_DIR".into(), d.display().to_string()));
    }
    let tui = Tui::spawn(cmd, args, &cwd, env).map_err(|e| {
        tracing::warn!(error = %e, "后台启动 Claude Code 失败");
        "failed"
    })?;
    let mut s = Session { tui, id, config_dir: ctx.config_dir.clone() };
    let screen = s.tui.wait_for(Duration::from_secs(40), |sc| ready(sc) || blocker(sc).is_some()).await.ok_or("failed")?;
    match blocker(&screen) {
        Some("logged_out") => return Err("logged_out"),
        Some("trust") => {
            // skip_first_run 没写进去（文件被锁着等）：选“信任”
            s.tui.send("\x1b[B\r");
            s.tui.wait_for(Duration::from_secs(20), ready).await.ok_or("failed")?;
        }
        _ => {}
    }
    Ok(s)
}

/// 输入框里有字时清掉（Ctrl+U 删到行首）
async fn clear_input(s: &mut Session) {
    s.tui.send("\x15");
    s.tui.pump(Duration::from_millis(300)).await;
}

/// 看这个账号的命令是哪种：输入 `/limit-reset` 看说明，再输入空格看参数提示。看完清掉输入，不回车。
async fn detect_mode(s: &mut Session) -> &'static str {
    s.tui.send("/limit-reset");
    let first = s.tui.wait_for(Duration::from_secs(4), |sc| sc.contains("No commands match") || sc.contains(LEGACY_DESC) || sc.contains("Use an available limit reset")).await;
    s.tui.send(" ");
    let hint = s.tui.wait_for(Duration::from_secs(3), |sc| sc.contains(ARG_HINT)).await;
    clear_input(s).await;
    if hint.is_some() {
        "cards"
    } else if first.is_some_and(|sc| sc.contains(LEGACY_DESC)) {
        "legacy"
    } else {
        "off"
    }
}

/// 敲一条 `/limit-reset <kind>`，等到确认框或者回话
enum Reply {
    Confirm(Confirm),
    Answer(String),
}

async fn run_kind(s: &mut Session, kind: &str) -> Option<Reply> {
    let echo = format!("/limit-reset {kind}");
    s.tui.send(&echo);
    s.tui.pump(Duration::from_millis(400)).await;
    s.tui.send("\r");
    let screen = s
        .tui
        .wait_for(Duration::from_secs(45), |sc| {
            confirm_box(sc).is_some() || answer_after(sc, &echo).is_some_and(|a| !a.contains("Checking for a reset") && !a.contains("This takes a moment")) && ready(sc)
        })
        .await?;
    if let Some(c) = confirm_box(&screen) {
        return Some(Reply::Confirm(c));
    }
    answer_after(&screen, &echo).map(Reply::Answer)
}

/// 确认框里按 Esc（卡留着），等它回“… kept …”
async fn keep(s: &mut Session) {
    s.tui.send("\x1b");
    let _ = s.tui.wait_for(Duration::from_secs(10), |sc| confirm_box(sc).is_none() && ready(sc)).await;
}

/// 查这个账号的重置卡（结果留 10 分钟，`force` 时重新查）：
/// - `state`：`ok`（有能用的）/ `none`（没有能用的，`message` 是 Claude Code 的原话）/ `legacy`（只有每周一次重置 5 小时，
///   不带参数一敲就用，所以查不了、只给“用”按钮）/ `unsupported`（账号没开）/ `logged_out` / `not_installed` / `failed`
/// - `cards`：`[{ kind, title, lines }]` 能用的（确认框里的话：能补哪些限额、还剩几张、用到哪天）
/// - `unusable`：`[{ kind, text }]` 有但现在用不了的
pub async fn status(shell: &str, home: &Path, p: &crate::claude_profiles::Profile, force: bool) -> Value {
    let ctx = Ctx::new(shell, home, p);
    if !force {
        if let Some(v) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&ctx.config_dir).filter(|(t, _)| t.elapsed() < STATUS_TTL).map(|(_, v)| v.clone()) {
            return v;
        }
    }
    // 本地开关都没开：不开终端（开关在 Claude Code 每次启动时更新，下次就能看到）
    let flags = flags_mode(&ctx.state_file);
    let v = if flags == "off" {
        json!({ "state": "unsupported" })
    } else {
        let _g = busy().lock().await;
        probe(&ctx).await
    };
    let mut v = v;
    v["checked_at"] = json!(crate::util::system_ms());
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(ctx.config_dir.clone(), (Instant::now(), v.clone()));
    v
}

async fn probe(ctx: &Ctx) -> Value {
    let mut s = match open(ctx).await {
        Ok(s) => s,
        Err(state) => return json!({ "state": state }),
    };
    match detect_mode(&mut s).await {
        "cards" => {}
        "legacy" => return json!({ "state": "legacy" }),
        _ => return json!({ "state": "unsupported" }),
    }
    let mut cards = vec![];
    let mut unusable = vec![];
    let mut message = None;
    let mut todo: Vec<&str> = KINDS.to_vec();
    let mut i = 0;
    while i < todo.len() {
        let kind = todo[i];
        i += 1;
        match run_kind(&mut s, kind).await {
            Some(Reply::Confirm(c)) => {
                cards.push(json!({ "kind": kind, "title": c.title, "lines": c.lines }));
                keep(&mut s).await;
            }
            Some(Reply::Answer(text)) => match can_use(&text) {
                // 这种用不了，还能用哪几种：只再看那几种
                Some(ok) => {
                    unusable.push(json!({ "kind": kind, "text": text }));
                    let rest: Vec<&str> = todo[i..].iter().copied().filter(|k| ok.contains(k)).collect();
                    todo.truncate(i);
                    todo.extend(rest);
                }
                // 和种类无关的话（没有能用的卡、不在限额上、开关刚关了……）：不用再看别的
                None => {
                    message = Some(text);
                    break;
                }
            },
            None => return json!({ "state": "failed" }),
        }
    }
    let state = if cards.is_empty() { "none" } else { "ok" };
    json!({ "state": state, "cards": cards, "unusable": unusable, "message": message })
}

/// 用一张卡：`kind` 是 [`KINDS`] 之一；`legacy` 时用每周一次的 5 小时重置（不带参数的 `/limit-reset`）。
/// 返回 `{ result: reset|kept|unknown|unsupported|logged_out|not_installed|failed, message }`，`message` 是 Claude Code 的原话
pub async fn use_card(shell: &str, home: &Path, p: &crate::claude_profiles::Profile, kind: &str) -> Value {
    let ctx = Ctx::new(shell, home, p);
    let _g = busy().lock().await;
    let r = use_inner(&ctx, kind).await;
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(&ctx.config_dir);
    // 限额变了：马上请 Claude Code 重新取一次用量
    if r["result"] == "reset" {
        let cwd = ctx.home.join("run").join("usage");
        let _ = crate::chat::models::fetch_limits(&ctx.shell, &ctx.config_dir, ctx.account_dir.as_deref(), &cwd).await;
    }
    r
}

async fn use_inner(ctx: &Ctx, kind: &str) -> Value {
    let legacy = kind == "legacy";
    if !legacy && !KINDS.contains(&kind) {
        return json!({ "result": "failed" });
    }
    let mut s = match open(ctx).await {
        Ok(s) => s,
        Err(state) => return json!({ "result": state }),
    };
    let mode = detect_mode(&mut s).await;
    if legacy {
        // 只在确实是老功能时敲不带参数的命令（新功能里不带参数是另一回事）
        if mode != "legacy" {
            return json!({ "result": "unsupported" });
        }
        let echo = "/limit-reset";
        s.tui.send(echo);
        s.tui.pump(Duration::from_millis(400)).await;
        s.tui.send("\r");
        let screen = s
            .tui
            .wait_for(Duration::from_secs(60), |sc| answer_after(sc, echo).is_some_and(|a| !a.contains("this usually takes") && !a.contains("Checking for a reset")) && ready(sc))
            .await;
        let Some(text) = screen.and_then(|sc| answer_after(&sc, echo)) else { return json!({ "result": "failed" }) };
        return json!({ "result": outcome(&text), "message": text });
    }
    if mode != "cards" {
        return json!({ "result": "unsupported" });
    }
    let c = match run_kind(&mut s, kind).await {
        Some(Reply::Confirm(c)) => c,
        Some(Reply::Answer(text)) => return json!({ "result": outcome(&text), "message": text }),
        None => return json!({ "result": "failed" }),
    };
    // 确认框刚弹出时不收按键：等一下；光标不在“用”上时往上移
    s.tui.pump(Duration::from_millis(1500)).await;
    if !c.on_yes {
        s.tui.send("\x1b[A");
        s.tui.pump(Duration::from_millis(400)).await;
        let sc = s.tui.screen();
        if !confirm_box(&sc).is_some_and(|c| c.on_yes) {
            keep(&mut s).await;
            return json!({ "result": "failed" });
        }
    }
    s.tui.send("\r");
    let echo = format!("/limit-reset {kind}");
    let screen = s
        .tui
        .wait_for(Duration::from_secs(90), |sc| confirm_box(sc).is_none() && answer_after(sc, &echo).is_some_and(|a| !a.contains("this usually takes")) && ready(sc))
        .await;
    let Some(text) = screen.and_then(|sc| answer_after(&sc, &echo)) else { return json!({ "result": "unknown" }) };
    json!({ "result": outcome(&text), "message": text })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────";

    #[test]
    fn reads_answer_and_kinds() {
        let s = format!("● Unknown command: /x\n❯ /limit-reset fable\n  ⎿  No Fable reset to use · you can use a 5-hour reset or a full reset\n\n{RULE}\n❯ \n{RULE}\n  ⏵⏵ auto mode on\n");
        let a = answer_after(&s, "/limit-reset fable").unwrap();
        assert_eq!(a, "No Fable reset to use · you can use a 5-hour reset or a full reset");
        assert_eq!(can_use(&a).unwrap(), vec!["5-hour", "full"]);
        assert!(ready(&s));
        assert!(can_use("No reset to use · you're not at a usage limit right now").is_none());
        assert_eq!(can_use("Your Fable reset can't be used right now · no Fable usage this week to give back · you can use a full reset, a 5-hour reset or a weekly reset").unwrap(), vec!["full", "5-hour", "weekly"]);
    }

    #[test]
    fn reads_confirm_box() {
        let s = format!(
            "❯ /limit-reset fable\n{RULE}\n Use a Fable reset?\n\n Refills your Fable limit now · returns what Fable used to your weekly limit · your weekly reset day stays Mon, Dec 1\n 2 Fable resets left · use by Dec 20\n\n ❯ Yes, use my Fable reset\n   No, keep it\n"
        );
        let c = confirm_box(&s).unwrap();
        assert_eq!(c.title, "Use a Fable reset?");
        assert_eq!(c.lines.len(), 2);
        assert!(c.lines[1].starts_with("2 Fable resets left"));
        assert!(c.on_yes);
        let s2 = s.replace(" ❯ Yes", "   Yes").replace("   No, keep", " ❯ No, keep");
        assert!(!confirm_box(&s2).unwrap().on_yes);
    }

    #[test]
    fn outcomes() {
        assert_eq!(outcome("Fable reset used · what Fable used is back in your weekly limit · your weekly reset day stays Mon · 1 Fable reset left"), "reset");
        assert_eq!(outcome("5-hour limit reset · next reset available Dec 3 · your weekly limit still applies"), "reset");
        assert_eq!(outcome("Your reset only works at a usage limit, and you're not at one now · nothing was used"), "kept");
        assert_eq!(outcome("Couldn't confirm the reset went through · if you're still at the limit in a moment, try again"), "unknown");
        assert_eq!(outcome("Fable reset kept · run /limit-reset at a limit to use it · use by Dec 20"), "kept");
    }

    #[test]
    fn flags() {
        let dir = std::env::temp_dir().join(format!("ll-resets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".claude.json");
        std::fs::write(&f, r#"{"cachedGrowthBookFeatures":{"tengu_nifty_lemur":{"enabled":false,"version":0}}}"#).unwrap();
        assert_eq!(flags_mode(&f), "off");
        std::fs::write(&f, r#"{"cachedGrowthBookFeatures":{"tengu_nifty_lemur":{"enabled":true}}}"#).unwrap();
        assert_eq!(flags_mode(&f), "legacy");
        std::fs::write(&f, r#"{"cachedGrowthBookFeatures":{"tengu_cedar_ember":{"enabled":true},"tengu_nifty_lemur":{"enabled":true}}}"#).unwrap();
        assert_eq!(flags_mode(&f), "cards");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真的开一个后台 Claude Code（本机账号），只查不用：`cargo test claude_resets::tests::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let home = std::env::temp_dir().join(format!("ll-resets-live-{}", std::process::id()));
            std::fs::create_dir_all(home.join("run")).unwrap();
            let store = crate::store::Store::open(&home.join("t.db")).unwrap();
            let p = crate::claude_profiles::get(&store, crate::claude_profiles::OFFICIAL).unwrap();
            let ctx = Ctx::new("/bin/bash", &home, &p);
            let mut s = open(&ctx).await.map_err(|e| e.to_string()).unwrap();
            let mode = detect_mode(&mut s).await;
            println!("mode = {mode}\n{}", s.tui.screen());
        });
    }
}
