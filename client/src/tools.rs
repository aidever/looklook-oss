//! AI 命令行工具：检测 Codex / Claude Code / OpenCode / DSH（DeepSeek Harness）/ 飞书 CLI（lark-cli）/ Node.js 是否已安装，并生成安装命令。
//! 安装是一个临时的后台任务（不进 tmux、不建终端）：输出留在内存里给界面看进度，客户端重启就没了；
//! npm 源自动测速选（也可指定淘宝 npmmirror、腾讯或官方）。没有 Node.js 时先下官方 Node.js 装到看看自己的目录（不要管理员权限），
//! Claude Code 先用官方原生安装，npm 全局目录没有写权限时装到看看的目录；`setup` 一次装好 Node.js → Claude Code → Codex。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const TOOLS: &[&str] = &["codex", "claude", "opencode", "dsh", "agy", "lark-cli", "node", "npm"];

#[derive(Debug, Clone, Serialize, Default)]
pub struct Detected {
    pub codex: Option<String>,
    pub claude: Option<String>,
    pub opencode: Option<String>,
    pub dsh: Option<String>,
    /// Antigravity CLI
    pub agy: Option<String>,
    /// 飞书官方的命令行（`@larksuite/cli`）：AI 助手用它以你本人的身份读、发飞书
    #[serde(rename = "lark-cli")]
    pub lark: Option<String>,
    pub node: Option<String>,
    pub npm: Option<String>,
}

impl Detected {
    fn set(&mut self, name: &str, path: String) {
        let slot = match name {
            "codex" => &mut self.codex,
            "claude" => &mut self.claude,
            "opencode" => &mut self.opencode,
            "dsh" => &mut self.dsh,
            "agy" => &mut self.agy,
            "lark-cli" => &mut self.lark,
            "node" => &mut self.node,
            "npm" => &mut self.npm,
            _ => return,
        };
        *slot = Some(path);
    }
}

/// 在用户的登录 shell 里查找命令（PATH 与用户平时打开终端时一致）。
pub async fn detect(shell: &str) -> Detected {
    let mut d = Detected::default();
    let out = if cfg!(windows) {
        let mut cmd = Command::new("where.exe");
        cmd.args(TOOLS);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        tokio::time::timeout(Duration::from_secs(8), cmd.output()).await
    } else {
        let script = TOOLS.iter().map(|t| format!("printf '%s=%s\\n' {t} \"$(command -v {t} 2>/dev/null)\";")).collect::<String>();
        let mut cmd = Command::new(shell);
        cmd.args(["-lic", &script]).stdin(std::process::Stdio::null());
        tokio::time::timeout(Duration::from_secs(8), cmd.output()).await
    };
    let Ok(Ok(out)) = out else {
        d.agy = crate::agents::agy_fallback();
        return d;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if cfg!(windows) {
            // where.exe 每行一个完整路径
            let lower = line.to_ascii_lowercase();
            for t in TOOLS {
                let stem = std::path::Path::new(&lower).file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if stem == *t {
                    d.set(t, line.to_string());
                }
            }
        } else if let Some((name, path)) = line.split_once('=') {
            if TOOLS.contains(&name) && path.starts_with('/') {
                d.set(name, path.to_string());
            }
        }
    }
    // agy 的官方安装器装在 `~/.local/bin`，有的 shell 配置里没把它加进 PATH
    if d.agy.is_none() {
        d.agy = crate::agents::agy_fallback();
    }
    d
}

pub fn registry(mirror: &str) -> &'static str {
    match mirror {
        "tencent" => "https://mirrors.cloud.tencent.com/npm/",
        "official" => "https://registry.npmjs.org/",
        _ => "https://registry.npmmirror.com/",
    }
}

pub fn package(tool: &str) -> Option<&'static str> {
    match tool {
        "codex" => Some("@openai/codex"),
        "claude" => Some("@anthropic-ai/claude-code"),
        "opencode" => Some("opencode-ai"),
        "dsh" => Some("@deepseek-ai/dsh"),
        "lark-cli" => Some("@larksuite/cli"),
        _ => None,
    }
}

pub fn install_command(tool: &str, mirror: &str) -> Option<String> {
    Some(format!("npm install -g {} --registry={}", package(tool)?, registry(mirror)))
}

// ---------------- 看看自己管的目录（Node.js、npm 全局目录） ----------------

static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// 启动时调一次：记下数据目录，把看看装的 Node.js 等目录加到自己进程的 `PATH` 最前面
/// （终端会话、对话模式、检测都从这里继承，见 `Paths::session_path`）
pub fn init(home: &Path) {
    let _ = HOME.set(home.to_path_buf());
    extend_path();
}

fn tools_dir() -> Option<PathBuf> {
    HOME.get().map(|h| h.join("tools"))
}

/// 看看装的 Node.js：`{数据目录}/tools/node`
pub fn node_root() -> Option<PathBuf> {
    tools_dir().map(|d| d.join("node"))
}

/// 系统的 npm 全局目录没有写权限时（Linux 上用发行版装的 Node.js 常见），全局包装到这里：`{数据目录}/tools/npm-global`
pub fn npm_prefix() -> Option<PathBuf> {
    tools_dir().map(|d| d.join("npm-global"))
}

/// npm 前缀里放命令的目录（Windows 直接在前缀下面）
fn bin_of(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.to_path_buf()
    } else {
        prefix.join("bin")
    }
}

/// 要加进 `PATH` 的目录（存在的才加）：看看装的 Node.js、看看的 npm 全局目录、Claude Code 原生安装的 `~/.local/bin`
pub fn managed_bins() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [node_root(), npm_prefix()].into_iter().flatten().map(|p| bin_of(&p)).collect();
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".local").join("bin"));
    }
    // Windows 上 Node.js 自带的 npmrc 把全局目录设在 %APPDATA%\npm（官方安装包会把它加进 PATH，解压的不会）
    if cfg!(windows) {
        if let Some(d) = dirs::data_dir() {
            v.push(d.join("npm"));
        }
    }
    v.retain(|p| p.is_dir());
    v
}

/// 把 `managed_bins` 放到本进程 `PATH` 最前面（已经有的不重复加）
pub fn extend_path() {
    let bins = managed_bins();
    if bins.is_empty() {
        return;
    }
    let cur: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let mut v = bins.clone();
    v.extend(cur.into_iter().filter(|p| !bins.contains(p)));
    if let Ok(p) = std::env::join_paths(v) {
        std::env::set_var("PATH", p);
    }
}

fn shell_quote(s: &str) -> String {
    if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// 交给 shell 的命令前面补上 `PATH`：看看的目录 + 用户终端里的 PATH（`-lc` 不读 `.zshrc`，nvm 的 npm 常只写在那里）
async fn with_path(shell: &str, line: &str) -> String {
    if cfg!(windows) {
        return line.to_string();
    }
    let mut parts: Vec<String> = managed_bins().iter().map(|p| p.display().to_string()).collect();
    if let Some(u) = crate::chat::codex_app::user_path(shell).await {
        parts.push(u);
    }
    if parts.is_empty() {
        return line.to_string();
    }
    format!("PATH={}:\"$PATH\" {line}", shell_quote(&parts.join(":")))
}

// ---------------- 选源（自动测速） ----------------

/// 自动选 npm 源：同时连 npmmirror 和官方，谁先回就用谁（国内一般是 npmmirror，海外是官方）；都不通用 npmmirror
pub async fn pick_mirror() -> &'static str {
    static PICKED: Mutex<Option<(std::time::Instant, &'static str)>> = Mutex::new(None);
    if let Some((at, m)) = *PICKED.lock().unwrap_or_else(|e| e.into_inner()) {
        if at.elapsed() < Duration::from_secs(600) {
            return m;
        }
    }
    let c = reqwest::Client::builder().connect_timeout(Duration::from_secs(4)).timeout(Duration::from_secs(6)).build().unwrap_or_default();
    let probe = |m: &'static str| {
        let c = c.clone();
        async move {
            let u = format!("{}@openai%2fcodex/latest", registry(m));
            match c.get(&u).send().await {
                Ok(r) if r.status().is_success() => Ok(m),
                _ => Err(()),
            }
        }
    };
    let m = match futures_util::future::select_ok([Box::pin(probe("npmmirror")), Box::pin(probe("official"))]).await {
        Ok((m, _)) => m,
        Err(_) => "npmmirror",
    };
    *PICKED.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), m));
    m
}

/// 界面传来的源：空、`auto` 都自动选
async fn resolve_mirror(mirror: &str) -> &'static str {
    match mirror {
        "npmmirror" => "npmmirror",
        "tencent" => "tencent",
        "official" => "official",
        _ => pick_mirror().await,
    }
}

// ---------------- 安装任务 ----------------

/// 输出最多留这么多字节（只留最后的部分）。
const OUTPUT_CAP: usize = 64 * 1024;
/// 装得再慢也不该超过这么久（每一步）。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// `node` | `claude` | `codex` | …
    pub name: String,
    /// `pending` | `running` | `ok` | `failed` | `skipped`
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub tool: String,
    pub command: String,
    /// `running` | `ok` | `failed`
    pub state: &'static str,
    pub exit_code: Option<i32>,
    pub output: String,
    pub started_at: String,
    /// 分几步装的（先装 Node.js 再装工具、一键装全套）：每步的状态；只有一步时为空
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

/// 每个工具最多一个任务（重新安装替换上一个结束了的）。
static JOBS: Mutex<Option<HashMap<String, Arc<Mutex<Job>>>>> = Mutex::new(None);

fn jobs<R>(f: impl FnOnce(&mut HashMap<String, Arc<Mutex<Job>>>) -> R) -> R {
    let mut g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

pub fn job(tool: &str) -> Option<Job> {
    jobs(|m| m.get(tool).map(|j| j.lock().unwrap_or_else(|e| e.into_inner()).clone()))
}

fn push_output(job: &Mutex<Job>, chunk: &str) {
    let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
    j.output.push_str(chunk);
    if j.output.len() > OUTPUT_CAP {
        let mut cut = j.output.len() - OUTPUT_CAP;
        while !j.output.is_char_boundary(cut) {
            cut += 1;
        }
        j.output.drain(..cut);
    }
}

fn set_step(job: &Mutex<Job>, name: &str, state: &'static str) {
    let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = j.steps.iter_mut().find(|s| s.name == name) {
        s.state = state;
    }
}

fn finish(job: &Mutex<Job>, ok: bool, code: Option<i32>) {
    let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
    j.state = if ok { "ok" } else { "failed" };
    j.exit_code = code;
    for s in j.steps.iter_mut().filter(|s| matches!(s.state, "pending" | "running")) {
        s.state = if ok { "ok" } else { "skipped" };
    }
    tracing::info!(tool = %j.tool, state = j.state, code, "安装 AI 工具结束");
}

/// 新建任务；同一个工具在跑时返回那个任务（Err）
fn new_job(tool: &str, command: &str, steps: &[&str]) -> Result<Arc<Mutex<Job>>, Job> {
    jobs(|m| {
        if let Some(j) = m.get(tool) {
            let j = j.lock().unwrap_or_else(|e| e.into_inner());
            if j.state == "running" {
                return Err(j.clone());
            }
        }
        let j = Arc::new(Mutex::new(Job {
            tool: tool.to_string(),
            command: command.to_string(),
            state: "running",
            exit_code: None,
            output: String::new(),
            started_at: crate::util::now_rfc3339(),
            steps: steps.iter().map(|n| Step { name: n.to_string(), state: "pending" }).collect(),
        }));
        m.insert(tool.to_string(), j.clone());
        Ok(j)
    })
}

/// 能装的工具（npm 包，或 Claude Code 的原生安装）
pub fn installable(tool: &str) -> bool {
    package(tool).is_some() || tool == "node"
}

/// 开始安装（同一个工具已经在装时返回那个任务）。没有 Node.js 时先装 Node.js；源 `auto`/空 = 自动选。
/// `tool` 还可以是 `node`（只装 Node.js）、`setup`（Node.js → Claude Code → Codex 一次装好，装过的跳过）。
pub fn start_install(shell: &str, tool: &str, mirror: &str) -> Option<Job> {
    if tool != "setup" && !installable(tool) {
        return None;
    }
    let steps: Vec<&str> = match tool {
        "setup" => vec!["node", "claude", "codex"],
        "node" => vec![],
        _ => vec!["node", tool],
    };
    let shown = match tool {
        "setup" => "Node.js → Claude Code → Codex".to_string(),
        "node" => "Node.js".to_string(),
        "claude" => "claude.ai/install".to_string(),
        "lark-cli" => crate::lark_cli::install_command(mirror != "official").0,
        _ => install_command(tool, if mirror.is_empty() || mirror == "auto" { "npmmirror" } else { mirror }).unwrap_or_default(),
    };
    let job = match new_job(tool, &shown, &steps) {
        Ok(j) => j,
        Err(running) => return Some(running),
    };
    let (shell, tool, mirror) = (shell.to_string(), tool.to_string(), mirror.to_string());
    let task = job.clone();
    tokio::spawn(async move {
        let ok = install_steps(&task, &shell, &tool, &mirror).await;
        finish(&task, ok, None);
    });
    Some(job_view(&job))
}

async fn install_steps(job: &Mutex<Job>, shell: &str, tool: &str, mirror: &str) -> bool {
    let mirror = resolve_mirror(mirror).await;
    push_output(job, &format!("# registry: {}\n", registry(mirror)));
    let found = detect(shell).await;
    let tools: Vec<&str> = match tool {
        "setup" => vec!["claude", "codex"],
        "node" => vec![],
        t => vec![t],
    };
    // Claude Code 原生安装不需要 Node.js；但原生装不上还要退回 npm，所以照样先确保有 Node.js
    let need_node = found.npm.is_none();
    if need_node {
        set_step(job, "node", "running");
        if let Err(e) = install_node(job, mirror).await {
            push_output(job, &format!("\nNode.js: {e}\n"));
            set_step(job, "node", "failed");
            return false;
        }
        set_step(job, "node", "ok");
    } else {
        if tool == "node" {
            push_output(job, "Node.js is already installed.\n");
        }
        set_step(job, "node", "skipped");
    }
    let found = if need_node { detect(shell).await } else { found };
    let mut all = true;
    for t in tools {
        let have = match t {
            "claude" => found.claude.clone().or_else(crate::agents::claude_fallback),
            "codex" => found.codex.clone(),
            _ => None,
        };
        if tool == "setup" && have.is_some() {
            push_output(job, &format!("{t}: already installed\n"));
            set_step(job, t, "skipped");
            continue;
        }
        set_step(job, t, "running");
        let ok = install_one(job, shell, t, mirror).await;
        set_step(job, t, if ok { "ok" } else { "failed" });
        all &= ok;
    }
    all
}

/// 装一个工具：Claude Code 先用官方原生安装（连得上 claude.ai 时），不行再用 npm；其他用 npm。全局目录没有写权限时装到看看的目录
async fn install_one(job: &Mutex<Job>, shell: &str, tool: &str, mirror: &str) -> bool {
    if tool == "lark-cli" {
        let (_, line) = crate::lark_cli::install_command(mirror != "official");
        return run_line(job, shell, &with_path(shell, &line).await).await.0;
    }
    if tool == "claude" && claude_ai_reachable().await {
        let line = if cfg!(windows) {
            "powershell -NoProfile -ExecutionPolicy Bypass -Command \"irm https://claude.ai/install.ps1 | iex\"".to_string()
        } else {
            "curl -fsSL https://claude.ai/install.sh | bash".to_string()
        };
        push_output(job, &format!("$ {line}\n"));
        if run_line(job, shell, &line).await.0 {
            extend_path();
            return true;
        }
        push_output(job, "\nNative installer failed, falling back to npm.\n");
    }
    let Some(base) = install_command(tool, mirror) else { return false };
    let mut line = format!("{base} --loglevel=http --no-fund --no-audit");
    if let Some(p) = global_prefix_override(shell).await {
        line.push_str(&format!(" --prefix {}", shell_quote(&p.display().to_string())));
        push_output(job, &format!("# npm global directory is not writable, installing into {}\n", p.display()));
    }
    push_output(job, &format!("$ {line}\n"));
    let ok = run_line(job, shell, &with_path(shell, &line).await).await.0;
    extend_path();
    ok
}

async fn claude_ai_reachable() -> bool {
    let c = reqwest::Client::builder().connect_timeout(Duration::from_secs(4)).timeout(Duration::from_secs(6)).build().unwrap_or_default();
    let u = if cfg!(windows) { "https://claude.ai/install.ps1" } else { "https://claude.ai/install.sh" };
    c.get(u).send().await.is_ok_and(|r| r.status().is_success())
}

/// npm 的全局目录（`npm prefix -g`）没有写权限时返回看看的 npm 目录（Windows 的全局目录在用户目录里，不管）
async fn global_prefix_override(shell: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let mut cmd = Command::new(shell);
    cmd.args(["-lc", &with_path(shell, "npm prefix -g").await]).stdin(Stdio::null()).stderr(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await.ok()?.ok()?;
    let prefix = String::from_utf8_lossy(&out.stdout).lines().last()?.trim().to_string();
    if prefix.is_empty() {
        return None;
    }
    let lib = Path::new(&prefix).join("lib").join("node_modules");
    let probe = if lib.is_dir() { lib } else { PathBuf::from(&prefix) };
    if writable(&probe) {
        return None;
    }
    let p = npm_prefix()?;
    std::fs::create_dir_all(&p).ok()?;
    Some(p)
}

fn writable(dir: &Path) -> bool {
    let f = dir.join(format!(".looklook-w-{}", std::process::id()));
    match std::fs::File::create(&f) {
        Ok(_) => {
            let _ = std::fs::remove_file(&f);
            true
        }
        Err(_) => false,
    }
}

// ---------------- 装 Node.js（官方包，装在看看自己的目录，不要管理员权限） ----------------

static NODE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Node.js 官方包的平台名（`linux-x64`、`darwin-arm64`、`win-x64`）
fn node_platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        _ => return None,
    })
}

fn node_base(mirror: &str) -> &'static str {
    if mirror == "official" {
        "https://nodejs.org/dist/"
    } else {
        "https://registry.npmmirror.com/-/binary/node/"
    }
}

async fn install_node(job: &Mutex<Job>, mirror: &str) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    let _g = NODE_LOCK.lock().await;
    let root = node_root().context("no data directory")?;
    if bin_of(&root).join(if cfg!(windows) { "node.exe" } else { "node" }).is_file() {
        extend_path();
        push_output(job, "Node.js is already installed by Looklook.\n");
        return Ok(());
    }
    let plat = node_platform().context("this system is not supported by Node.js")?;
    let c = reqwest::Client::builder().connect_timeout(Duration::from_secs(8)).user_agent(concat!("looklook/", env!("CARGO_PKG_VERSION"))).build()?;
    let base = node_base(mirror);
    // 最新的 LTS
    let index: serde_json::Value = c.get(format!("{base}index.json")).send().await?.error_for_status()?.json().await?;
    let ver = index
        .as_array()
        .and_then(|a| a.iter().find(|v| v["lts"].is_string()))
        .and_then(|v| v["version"].as_str())
        .context("no LTS version")?
        .to_string();
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let file = format!("node-{ver}-{plat}.{ext}");
    let sums = c.get(format!("{base}{ver}/SHASUMS256.txt")).send().await?.error_for_status()?.text().await?;
    let sha = sums.lines().find_map(|l| l.split_once("  ").filter(|(_, f)| *f == file).map(|(h, _)| h.to_string())).context("no checksum")?;
    let url = format!("{base}{ver}/{file}");
    push_output(job, &format!("Downloading Node.js {ver}: {url}\n"));
    let dir = root.parent().context("bad path")?.to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(&file);
    let mut r = c.get(&url).send().await?.error_for_status()?;
    let total = r.content_length().unwrap_or(0);
    let mut out = tokio::fs::File::create(&tmp).await?;
    let mut h = Sha256::new();
    let (mut got, mut shown) = (0u64, 0u64);
    loop {
        let chunk = match tokio::time::timeout(Duration::from_secs(30), r.chunk()).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => bail!("download: {e}"),
            Err(_) => bail!("download: timeout"),
        };
        h.update(&chunk);
        out.write_all(&chunk).await?;
        got += chunk.len() as u64;
        if got - shown >= 4 << 20 || got == total {
            shown = got;
            if total > 0 {
                push_output(job, &format!("  {} / {} MB\n", got >> 20, total >> 20));
            }
        }
    }
    out.flush().await?;
    drop(out);
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if hex != sha {
        let _ = std::fs::remove_file(&tmp);
        bail!("checksum mismatch");
    }
    push_output(job, "Extracting…\n");
    let (t, rt) = (tmp.clone(), root.clone());
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::archive::extract(&t, &rt)?;
        // zip 包没去掉最外层目录：只有一个子目录时把它提上来
        let kids: Vec<PathBuf> = std::fs::read_dir(&rt)?.flatten().map(|e| e.path()).collect();
        if let [only] = kids.as_slice() {
            if only.is_dir() {
                let up = rt.with_extension("up");
                let _ = std::fs::remove_dir_all(&up);
                std::fs::rename(only, &up)?;
                std::fs::remove_dir_all(&rt)?;
                std::fs::rename(&up, &rt)?;
            }
        }
        Ok(())
    })
    .await??;
    let _ = std::fs::remove_file(&tmp);
    extend_path();
    push_output(job, &format!("Node.js {ver} installed to {}\n", root.display()));
    tracing::info!(%ver, dir = %root.display(), "装好了 Node.js");
    Ok(())
}

/// 在后台跑一条命令（和安装共用同一个任务位置，同一个工具同时只跑一个；在跑时返回那个任务）。
/// `command` 是界面上显示的，`line` 是真正交给 shell（Windows 上是 cmd）跑的。更新 AI 工具也走这里（agent_update.rs）。
pub fn start_command(shell: &str, tool: &str, command: &str, line: &str) -> Job {
    let job = match new_job(tool, command, &[]) {
        Ok(j) => j,
        Err(running) => return running,
    };
    push_output(&job, &format!("$ {command}\n"));
    let (shell, line) = (shell.to_string(), line.to_string());
    let task = job.clone();
    tokio::spawn(async move {
        let line = with_path(&shell, &line).await;
        let (ok, code) = run_line(&task, &shell, &line).await;
        finish(&task, ok, code);
    });
    job_view(&job)
}

/// 跑一条命令，输出接到任务里；返回（成功没有, 退出码）
async fn run_line(job: &Mutex<Job>, shell: &str, line: &str) -> (bool, Option<i32>) {
    let mut cmd = if cfg!(windows) {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut c = Command::new("cmd.exe");
        // 原样交给 cmd（`/S` 去掉最外层的引号）：命令里带引号的路径（`"C:\…\claude.exe" update`）按 Windows 的参数转义会被弄坏
        #[cfg(windows)]
        {
            c.args(["/D", "/S", "/C"]).raw_arg(format!("\"{line} 2>&1\""));
            c.creation_flags(0x0800_0000);
        }
        c
    } else {
        let mut c = Command::new(shell);
        c.args(["-lc", &format!("{{ {line} ; }} 2>&1")]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).env("NO_COLOR", "1").kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            push_output(job, &format!("{e}\n"));
            return (false, None);
        }
    };
    let mut out = child.stdout.take().expect("piped");
    let run = async {
        let mut buf = vec![0u8; 8192];
        let mut pending = Vec::new();
        loop {
            match out.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    // 只交出完整的 UTF-8（Windows 上也可能是本地编码，那就按有损转换）
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => pending.len(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).replace("\r\n", "\n");
                    pending.drain(..valid);
                    push_output(job, &text);
                }
            }
        }
        child.wait().await
    };
    match tokio::time::timeout(INSTALL_TIMEOUT, run).await {
        Ok(Ok(st)) => (st.success(), st.code()),
        Ok(Err(e)) => {
            push_output(job, &format!("\n{e}\n"));
            (false, None)
        }
        Err(_) => {
            push_output(job, "\ntimed out\n");
            (false, None)
        }
    }
}

fn job_view(job: &Mutex<Job>) -> Job {
    job.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(install_command("codex", "npmmirror").unwrap(), "npm install -g @openai/codex --registry=https://registry.npmmirror.com/");
        assert!(install_command("claude", "tencent").unwrap().contains("mirrors.cloud.tencent.com"));
        assert_eq!(install_command("opencode", "official").unwrap(), "npm install -g opencode-ai --registry=https://registry.npmjs.org/");
        assert_eq!(install_command("dsh", "npmmirror").unwrap(), "npm install -g @deepseek-ai/dsh --registry=https://registry.npmmirror.com/");
        assert!(install_command("vim", "official").is_none());
        assert_eq!(install_command("lark-cli", "official").unwrap(), "npm install -g @larksuite/cli --registry=https://registry.npmjs.org/");
    }

    #[test]
    fn output_keeps_the_tail() {
        let j = Mutex::new(Job { tool: "x".into(), command: String::new(), state: "running", exit_code: None, output: String::new(), started_at: String::new(), steps: vec![] });
        push_output(&j, &"安".repeat(OUTPUT_CAP));
        push_output(&j, "end");
        let out = j.lock().unwrap().output.clone();
        assert!(out.len() <= OUTPUT_CAP && out.ends_with("end"));
    }

    /// 真下载装一遍 Node.js（要网络）：`cargo test real_node_install -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_node_install() {
        let home = tempfile::tempdir().unwrap();
        let _ = HOME.set(home.path().to_path_buf());
        let j = new_job("node-test", "", &[]).unwrap();
        let m = pick_mirror().await;
        install_node(&j, m).await.unwrap();
        let out = j.lock().unwrap().output.clone();
        println!("{m}\n{out}");
        let node = bin_of(&node_root().unwrap()).join(if cfg!(windows) { "node.exe" } else { "node" });
        let v = std::process::Command::new(&node).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&v.stdout).starts_with('v'));
        assert!(bin_of(&node_root().unwrap()).join(if cfg!(windows) { "npm.cmd" } else { "npm" }).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detects_shell_builtins_path() {
        let d = detect("/bin/sh").await;
        // 测试环境不一定装了这些工具；只要求解析不出错，且找到的都是绝对路径。
        for p in [d.codex, d.claude, d.opencode, d.dsh, d.node, d.npm].into_iter().flatten() {
            assert!(p.starts_with('/'));
        }
    }
}
