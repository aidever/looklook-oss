//! 助理记忆的后台活（设计稿 §5.2、§5.5、§5.6）：不占大脑的对话，不打断用户。
//!
//! - **后台复盘**（[`reflect`]）：每十来轮对话、压缩之前、隔夜换新之前，空闲时用一次小模型（haiku / Codex 低强度）看最近的对话，
//!   补上大脑漏记的偏好和项目的新情况。模型只回“改哪条、改成什么”，由看看过同一套检查后落盘（`via=reflect`）。
//! - **每日整理**（[`tidy`]）：一天一次，空闲时：合并重复的、放错类的挪过去、过时的归档；项目目录不在了、半年没用到的提议归档。
//!   动到用户写的、钉住的，或者常驻已经装不下时的合并，都变成提议，用户点头才做。
//! - **第一次启用**（[`seed`]）：从已有的会话和对话记录列出项目，生成项目名册草稿和只放事实的项目档案，作为提议给用户过目。
//! - **对话一直是一条**（[`tend`]）：上下文用到六成时先复盘再原地压缩（界面上留“这之前的内容已整理”）；
//!   隔了一夜又空闲很久：复盘、压缩，再重起大脑（带上新的常驻记忆），还是同一段对话。
//! - 全文索引（assistant_index.rs）也在这里定时更新。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::assistant_memory as mem;
use crate::gateway::App;

const JOBS_KEY: &str = "assistant.memory_jobs";
const BG_KEY: &str = "assistant.memory_bg";
const TURNS_KEY: &str = "assistant.turns";

/// 几轮对话复盘一次
const REFLECT_EVERY: u64 = 10;
/// 上下文用到多少压缩
const COMPACT_AT: f64 = 0.6;
/// 空闲多久才动（用户可能马上接着说）
const IDLE_MS: i64 = 60_000;
/// 每日整理至少空闲多久
const TIDY_IDLE_MS: i64 = 10 * 60_000;
/// 隔夜换新：空闲这么久、过了早上四点
const OVERNIGHT_IDLE_MS: i64 = 3 * 3600_000;
const NEW_DAY_HOUR: u32 = 4;
/// 多久没用到提议归档
const STALE_MS: i64 = 180 * 24 * 3600_000;
/// 全文索引多久更新一次
const INDEX_EVERY_MS: i64 = 10 * 60_000;
/// 复盘时最多给模型看多少字的对话
const EXCERPT_MAX: usize = 16_000;

static RUNNING: AtomicBool = AtomicBool::new(false);
static INDEXING: AtomicBool = AtomicBool::new(false);
static INDEXED_AT: AtomicI64 = AtomicI64::new(0);

/// 后台活的进度（记忆页上显示）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Jobs {
    /// 复盘看到哪条消息了（消息时间）
    #[serde(default)]
    pub reflect_upto: i64,
    #[serde(default)]
    pub reflected: i64,
    #[serde(default)]
    pub reflect_result: Value,
    #[serde(default)]
    pub turns_at_reflect: u64,
    #[serde(default)]
    pub tidy_day: String,
    #[serde(default)]
    pub tidied: i64,
    #[serde(default)]
    pub tidy_result: Value,
    #[serde(default)]
    pub compacted: i64,
    #[serde(default)]
    pub rollover_day: String,
    #[serde(default)]
    pub turns_at_rollover: u64,
    /// 隔夜换新：压缩完了要重起大脑
    #[serde(default)]
    pub restart_pending: bool,
    #[serde(default)]
    pub seeded: bool,
}

pub fn jobs(store: &crate::store::Store) -> Jobs {
    store.get(JOBS_KEY).ok().flatten().unwrap_or_default()
}

fn update(store: &crate::store::Store, f: impl FnOnce(&mut Jobs)) {
    let mut j = jobs(store);
    f(&mut j);
    let _ = store.set(JOBS_KEY, &j);
}

/// 后台复盘和整理开着没有（花 token：用户能关）
pub fn bg_enabled(store: &crate::store::Store) -> bool {
    store.get::<bool>(BG_KEY).ok().flatten().unwrap_or(true)
}

pub fn set_bg_enabled(store: &crate::store::Store, on: bool) {
    let _ = store.set(BG_KEY, &on);
}

/// 用户跟大脑说了一句（assistant::note_said 里调）
pub fn count_turn(store: &crate::store::Store) {
    let n = turns(store) + 1;
    let _ = store.set(TURNS_KEY, &n);
}

fn turns(store: &crate::store::Store) -> u64 {
    store.get::<u64>(TURNS_KEY).ok().flatten().unwrap_or(0)
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

// ---------------- 问一次小模型 ----------------

/// 用助理的引擎和账号问一次（不带工具、不留记录、不读用户的 CLAUDE.md）
pub async fn ask(app: &App, prompt: &str) -> Result<String, String> {
    let brain = crate::assistant::brain_id(&app.store).unwrap_or_default();
    let shell = app.instances.shell().to_string();
    match crate::assistant::settings(&app.store).engine {
        crate::assistant::Engine::Claude => {
            let program = crate::chat::claude_sdk::program(&shell).await.ok_or("Claude Code is not installed")?;
            let profile = crate::claude_profiles::for_instance(&app.store, &brain);
            let env = crate::claude_profiles::launch_env(&app.paths.home, &profile);
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let mut cmd = crate::chat::claude_sdk::command(&program);
            cmd.args(["-p", "--model", "haiku", "--output-format", "json", "--no-session-persistence", "--setting-sources", "", "--strict-mcp-config", "--tools", "", "--disable-slash-commands"]);
            cmd.current_dir(dir.path()).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
            if let Some(p) = crate::chat::codex_app::launch_path(&shell, &program).await {
                cmd.env("PATH", p);
            }
            cmd.env_remove(crate::chat::hooks::INSTANCE_ENV).env_remove("TMUX").env_remove("TMUX_PANE");
            cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1").env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
            crate::chat::claude_sdk::apply_identity(&mut cmd, &app.store);
            if let Some(e) = &env {
                e.apply(&mut cmd);
            }
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000);
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(prompt.as_bytes()).await;
            }
            let out = tokio::time::timeout(std::time::Duration::from_secs(300), child.wait_with_output()).await.map_err(|_| "timeout".to_string())?.map_err(|e| e.to_string())?;
            let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| format!("bad output: {}", String::from_utf8_lossy(&out.stdout).chars().take(200).collect::<String>()))?;
            if v["is_error"] == true {
                return Err(v["result"].as_str().unwrap_or("error").chars().take(300).collect());
            }
            Ok(v["result"].as_str().unwrap_or("").to_string())
        }
        crate::assistant::Engine::Codex => {
            let exe = crate::tools::detect(&shell).await.codex.ok_or("Codex is not installed")?;
            let profile = crate::codex_profiles::for_instance(&app.store, &brain);
            let env = crate::codex_profiles::launch_env(&app.paths.home, &profile);
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let out = dir.path().join("last.txt");
            let args: Vec<String> = ["exec", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-c", "model_reasoning_effort=\"low\"", "-o"].into_iter().map(String::from).chain([out.display().to_string()]).collect();
            crate::pet_lines::by_cli(exe, &args, prompt, Some(&out), &shell, env.as_ref(), &[]).await.map_err(|e| e.params.get("detail").and_then(Value::as_str).unwrap_or(&e.code).to_string())
        }
    }
}

/// 回答里的 JSON（模型可能包在 ``` 里、前后多说几句）
fn json_in(s: &str) -> Option<Value> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    serde_json::from_str(&s[a..=b]).ok()
}

fn memory_list(paths: &crate::paths::Paths) -> String {
    let mut s = String::new();
    for e in mem::entries(paths) {
        let mark = match (e.pinned, e.by) {
            (true, _) => "（钉住）",
            (_, mem::By::User) => "（用户写的）",
            _ => "",
        };
        s.push_str(&format!("- [{}] {}{} · {}{}：{}\n", e.id, e.kind.id(), mark, e.title, e.project.as_deref().map(|p| format!("（项目 {p}）")).unwrap_or_default(), e.text.replace('\n', " ")));
    }
    if s.is_empty() {
        s.push_str("（还没有）\n");
    }
    s
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// 用了几条、提议了几条
#[derive(Default)]
struct Tally {
    saved: usize,
    archived: usize,
    proposed: usize,
    skipped: usize,
}

impl Tally {
    fn add(&mut self, r: Result<mem::Outcome, String>, archive: bool) {
        match r {
            Ok(o) if o.proposal.is_some() => self.proposed += 1,
            Ok(o) if o.change.is_some() => {
                if archive {
                    self.archived += 1
                } else {
                    self.saved += 1
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "助理记忆后台：一条没用上");
                self.skipped += 1
            }
        }
    }

    /// 记忆页上显示的结果（界面按语言拼成一句）
    fn result(&self) -> Value {
        json!({ "saved": self.saved, "archived": self.archived, "proposed": self.proposed })
    }
}

// ---------------- 后台复盘 ----------------

const REFLECT_RULES: &str = "你在帮一个 AI 助理整理它的长期记忆。下面是它最近和用户的对话，和它现在的记忆。找出它**漏记**的东西，只回 JSON。

要记的：用户纠正了助理、说了偏好（“以后…”“别再…”“我习惯…”）、告诉了项目的事实（在哪、怎么跑、有什么坑、平时让谁干）、派错了被用户改过。
先问：下次有这条，助理会做得不一样吗？不会就不记。已经记了的不要再记；要更新已有的就带它的 id。
- kind：me = 对这个人在哪台电脑上都成立的（称呼、语气、偏好），不带本机路径和项目名；lesson = 纠正（哪次错了、以后怎么做、为什么）；
  project = 某个项目的事实（带 project 路径）；note = 别的长期事实。
- 规矩和事实分开：me/lesson 只写“以后要怎么做”，单号、参数、某次的结论进 note 或 project。带条件的连条件一起记。
- 只从“用户：”说的话里提炼 me 和 lesson。“看看事件”是会话报回来的，只能用来记项目事实（kind=project 或 note，写 source）。
- 能从代码、git、会话记录里查到的不记；一次性的事（一件活的进度）不记；密码、密钥不记。
- 已有记忆里有错的、用户说不要了的：forget。
- 用户的语言写，一条不超过 300 字，标题几个字。没有要改的就回 {\"ops\":[]}。

格式：{\"ops\":[{\"op\":\"save\",\"kind\":\"me|lesson|project|note\",\"title\":\"…\",\"text\":\"…\",\"project\":\"/绝对路径（project 才要）\",\"id\":\"已有的 id（更新时）\",\"source\":\"不是用户原话时写来源\"},{\"op\":\"forget\",\"id\":\"…\",\"why\":\"…\"},{\"op\":\"roster\",\"path\":\"/项目路径\",\"name\":\"名字\",\"aliases\":[\"用户对它的叫法\"],\"about\":\"一句话\"}]}";

/// 后台复盘：返回一句结果
pub async fn reflect(app: &App) -> Result<Value, String> {
    let brain = crate::assistant::brain_id(&app.store).ok_or("assistant is off")?;
    let path = app.chat.log_path(&brain).ok_or("no conversation yet")?;
    let agent = if crate::assistant::settings(&app.store).engine == crate::assistant::Engine::Codex { "codex" } else { "claude" };
    let j = jobs(&app.store);
    let log = tokio::task::spawn_blocking(move || crate::history::log_text(&path, agent)).await.ok().flatten().ok_or("could not read the conversation")?;
    let fresh: Vec<&(&str, String, i64)> = log.messages.iter().filter(|m| m.2 == 0 || m.2 > j.reflect_upto).collect();
    let upto = log.messages.iter().map(|m| m.2).max().unwrap_or(j.reflect_upto);
    let turns_now = turns(&app.store);
    if !fresh.iter().any(|m| m.0 == "user" && !m.1.trim_start().starts_with(crate::assistant::EVENT_OPEN)) {
        update(&app.store, |j| {
            j.reflect_upto = upto;
            j.turns_at_reflect = turns_now;
        });
        return Ok(Tally::default().result());
    }
    // 从最新的往前取，取到上限
    let mut lines: Vec<String> = vec![];
    let mut used = 0;
    for (role, text, _) in fresh.iter().rev() {
        let t = text.trim();
        let line = if *role == "user" && t.starts_with(crate::assistant::EVENT_OPEN) {
            format!("看看事件：{}", clip(t, 500))
        } else if *role == "user" {
            format!("用户：{}", clip(t, 1200))
        } else {
            format!("助理：{}", clip(t, 800))
        };
        used += line.chars().count();
        if used > EXCERPT_MAX {
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    let roster: String = mem::roster(&app.paths).iter().map(|r| format!("- {} · {}\n", r.name, r.path)).collect();
    let prompt = format!("{REFLECT_RULES}\n\n## 现在的记忆\n{}\n## 项目名册\n{}\n## 最近的对话\n{}\n", memory_list(&app.paths), if roster.is_empty() { "（空）\n".into() } else { roster }, lines.join("\n\n"));
    let answer = ask(app, &prompt).await?;
    let v = json_in(&answer).ok_or("the model did not answer in JSON")?;
    let words = crate::gateway::assistant::local_words(app).await;
    let mut t = Tally::default();
    for op in v["ops"].as_array().into_iter().flatten().take(20) {
        let s = |k: &str| op[k].as_str().map(str::trim).filter(|x| !x.is_empty()).map(String::from);
        match op["op"].as_str() {
            Some("save") => {
                let Some(kind) = s("kind").and_then(|k| mem::Kind::parse(&k)) else {
                    t.skipped += 1;
                    continue;
                };
                let save = mem::Save { id: s("id"), kind: Some(kind), title: s("title").unwrap_or_default(), text: s("text").unwrap_or_default(), project: s("project"), source: s("source"), by: mem::By::Assistant, why: "后台复盘".into(), via: Some("reflect".into()), allow_new: false };
                t.add(mem::save(&app.paths, save, &words), false);
            }
            Some("forget") => t.add(mem::archive_via(&app.paths, &s("id").unwrap_or_default(), mem::By::Assistant, &s("why").unwrap_or_default(), Some("reflect")), true),
            Some("roster") => {
                // 名册改了一条另算（不进“记了几条”）
                let item = mem::RosterItem { path: s("path").unwrap_or_default(), name: s("name").unwrap_or_default(), aliases: op["aliases"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(), about: s("about").unwrap_or_default(), updated: 0 };
                // 只改已经在名册里的（新项目进名册要用户过目）
                if mem::roster(&app.paths).iter().any(|r| crate::history::dir_key(&r.path) == crate::history::dir_key(&item.path)) {
                    let same = mem::roster(&app.paths).into_iter().any(|r| r.path == item.path && r.name == item.name && r.aliases == item.aliases && r.about == item.about);
                    if !same && mem::roster_put(&app.paths, item).is_ok() {
                        t.saved += 1;
                    }
                }
            }
            _ => t.skipped += 1,
        }
    }
    let note = t.result();
    update(&app.store, |j| {
        j.reflect_upto = upto;
        j.reflected = crate::util::system_ms();
        j.reflect_result = note.clone();
        j.turns_at_reflect = turns_now;
    });
    tracing::info!(note = %note, "助理记忆：后台复盘");
    Ok(note)
}

// ---------------- 每日整理 ----------------

const TIDY_RULES: &str = "你在帮一个 AI 助理整理它的长期记忆，只回 JSON。看下面的全部条目，找出：
- 重复、说的是一回事的：合并成一条（merge：被合并的 id，合并后的 kind/title/text）；
- 放错类的：me / lesson 里写的是事实（单号、参数、配置值、某次的结论）而不是“以后要怎么做”：挪到 note 或 project（move）；
  me 里只针对某个项目的：挪到 project（带 project 路径）；
- 明显过时、互相矛盾里旧的那条：归档（forget，写原因）。
拿不准就不动。用户写的、钉住的也可以提，看看会先问用户。没有要改的就回 {\"ops\":[]}。

格式：{\"ops\":[{\"op\":\"merge\",\"ids\":[\"…\",\"…\"],\"kind\":\"…\",\"title\":\"…\",\"text\":\"…\",\"project\":\"…\"},{\"op\":\"move\",\"id\":\"…\",\"kind\":\"note|project|lesson|me\",\"project\":\"…\"},{\"op\":\"forget\",\"id\":\"…\",\"why\":\"…\"}]}";

/// 每日整理：返回一句结果
pub async fn tidy(app: &App) -> Result<Value, String> {
    let paths = &app.paths;
    let now = crate::util::system_ms();
    let mut t = Tally::default();
    // 不用模型的：名册里目录不在了的、项目档案的目录不在了的、半年没用到的 → 提议
    for r in mem::roster(paths) {
        if !std::path::Path::new(&r.path).exists() {
            let p = mem::Proposal { id: String::new(), at: now, from: "tidy".into(), summary: format!("从项目名册拿掉「{}」", r.name), why: format!("{} 这个目录不在了", r.path), ops: vec![mem::Op::RosterRemove { path: r.path.clone() }] };
            if mem::propose(paths, p).is_ok() {
                t.proposed += 1;
            }
        }
    }
    let all = mem::entries(paths);
    for e in &all {
        let gone = e.kind == mem::Kind::Project && e.project.as_deref().is_some_and(|p| !std::path::Path::new(p).exists());
        let stale = !e.pinned && now - e.used_at.max(e.updated) > STALE_MS;
        if gone || stale {
            let why = if gone { "项目目录不在了".to_string() } else { "半年没用到".to_string() };
            let p = mem::Proposal { id: String::new(), at: now, from: "tidy".into(), summary: format!("归档「{}」", e.title), why, ops: vec![mem::Op::Archive { id: e.id.clone() }] };
            if mem::propose(paths, p).is_ok() {
                t.proposed += 1;
            }
        }
    }
    if all.len() >= 3 {
        let (_, st) = mem::resident(paths);
        let prompt = format!("{TIDY_RULES}\n\n## 全部条目\n{}", memory_list(paths));
        let answer = ask(app, &prompt).await?;
        let v = json_in(&answer).ok_or("the model did not answer in JSON")?;
        let words = crate::gateway::assistant::local_words(app).await;
        let by_id = |id: &str| all.iter().find(|e| e.id == id);
        for op in v["ops"].as_array().into_iter().flatten().take(20) {
            let s = |k: &str| op[k].as_str().map(str::trim).filter(|x| !x.is_empty()).map(String::from);
            match op["op"].as_str() {
                Some("merge") => {
                    let ids: Vec<String> = op["ids"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).filter(|id| by_id(id).is_some()).collect();
                    let Some(kind) = s("kind").and_then(|k| mem::Kind::parse(&k)) else { continue };
                    if ids.len() < 2 {
                        continue;
                    }
                    let (title, text) = match mem::check(kind, &s("title").unwrap_or_default(), &s("text").unwrap_or_default(), &words, mem::By::Assistant) {
                        Ok(x) => x,
                        Err(_) => {
                            t.skipped += 1;
                            continue;
                        }
                    };
                    let project = s("project").or_else(|| by_id(&ids[0]).and_then(|e| e.project.clone()));
                    let protected = ids.iter().any(|id| by_id(id).is_some_and(|e| e.pinned || e.by == mem::By::User));
                    // 动到用户的、常驻已经装不下（设计稿：合并要用户点头）：提议
                    if protected || st.left_out > 0 {
                        let mut ops = vec![mem::Op::Save { id: None, kind, title: title.clone(), text, project }];
                        ops.extend(ids.iter().map(|id| mem::Op::Archive { id: id.clone() }));
                        let p = mem::Proposal { id: String::new(), at: now, from: "tidy".into(), summary: format!("把 {} 条合并成「{title}」", ids.len()), why: "说的是一回事".into(), ops };
                        if mem::propose(paths, p).is_ok() {
                            t.proposed += 1;
                        }
                        continue;
                    }
                    let saved = mem::save(paths, mem::Save { kind: Some(kind), title, text, project, by: mem::By::Assistant, why: "每日整理：合并重复的".into(), via: Some("tidy".into()), allow_new: true, ..Default::default() }, &words);
                    if saved.as_ref().is_ok_and(|o| o.entry.is_some()) {
                        let keep = saved.as_ref().ok().and_then(|o| o.entry.as_ref()).map(|e| e.id.clone()).unwrap_or_default();
                        for id in ids.iter().filter(|id| **id != keep) {
                            let _ = mem::archive_via(paths, id, mem::By::Assistant, "合并进了别的条目", Some("tidy"));
                        }
                    }
                    t.add(saved, false);
                }
                Some("move") => {
                    let (Some(id), Some(kind)) = (s("id"), s("kind").and_then(|k| mem::Kind::parse(&k))) else { continue };
                    let Some(e) = by_id(&id) else { continue };
                    let save = mem::Save { id: Some(id), kind: Some(kind), title: e.title.clone(), text: e.text.clone(), project: s("project").or(e.project.clone()), by: mem::By::Assistant, why: "每日整理：放错了类".into(), via: Some("tidy".into()), allow_new: true, ..Default::default() };
                    t.add(mem::save(paths, save, &words), false);
                }
                Some("forget") => t.add(mem::archive_via(paths, &s("id").unwrap_or_default(), mem::By::Assistant, &s("why").unwrap_or_else(|| "过时了".into()), Some("tidy")), true),
                _ => t.skipped += 1,
            }
        }
    }
    let note = t.result();
    let (y, mo, d, ..) = crate::util::local_now();
    update(&app.store, |j| {
        j.tidy_day = format!("{y}-{mo:02}-{d:02}");
        j.tidied = crate::util::system_ms();
        j.tidy_result = note.clone();
    });
    tracing::info!(note = %note, "助理记忆：每日整理");
    Ok(note)
}

// ---------------- 第一次启用 ----------------

/// 项目名册草稿 + 只放事实的项目档案，作为提议给用户过目（不替项目写规矩，规矩以仓库里的 CLAUDE.md / AGENTS.md 为准）
pub async fn seed(app: &App) -> usize {
    let mut n = 0;
    let known: Vec<String> = mem::roster(&app.paths).iter().map(|r| crate::history::dir_key(&r.path)).collect();
    for p in crate::gateway::assistant::projects(app).await.into_iter().take(20) {
        let (Some(path), Some(name)) = (p["path"].as_str(), p["name"].as_str()) else { continue };
        // 家目录不算项目
        if known.contains(&crate::history::dir_key(path)) || dirs::home_dir().is_some_and(|h| crate::history::dir_key(&h.to_string_lossy()) == crate::history::dir_key(path)) {
            continue;
        }
        let mut facts = vec![format!("在 {path}。")];
        if let Some(r) = p["remote"].as_str() {
            facts.push(format!("git 远端 {r}。"));
        } else if let Some(rs) = p["repos_inside"].as_array().filter(|a| !a.is_empty()) {
            let names: Vec<String> = rs.iter().take(6).filter_map(|r| r["path"].as_str().or(r.as_str())).map(|x| std::path::Path::new(x).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
            facts.push(format!("不是 git 仓库，里面有这些仓库：{}。", names.join("、")));
        }
        let rules: Vec<&str> = ["CLAUDE.md", "AGENTS.md"].into_iter().filter(|f| std::path::Path::new(path).join(f).is_file()).collect();
        if !rules.is_empty() {
            facts.push(format!("仓库里有 {}（项目规矩以它为准）。", rules.join("、")));
        }
        let topics: Vec<String> = p["recent_topics"].as_array().into_iter().flatten().filter_map(Value::as_str).take(3).map(|t| clip(t, 60)).collect();
        if !topics.is_empty() {
            facts.push(format!("最近的对话：{}。", topics.join("；")));
        }
        let profile = clip(&facts.join(""), 580);
        let item = mem::RosterItem { path: path.to_string(), name: clip(name, 40), ..Default::default() };
        let pr = mem::Proposal { id: String::new(), at: crate::util::system_ms(), from: "seed".into(), summary: format!("加进项目名册：{name}"), why: profile.clone(), ops: vec![mem::Op::Roster { item, profile: Some(profile) }] };
        if mem::propose(&app.paths, pr).is_ok() {
            n += 1;
        }
    }
    update(&app.store, |j| j.seeded = true);
    n
}

// ---------------- 定时 ----------------

fn today() -> String {
    let (y, mo, d, ..) = crate::util::local_now();
    format!("{y}-{mo:02}-{d:02}")
}

/// 这一刻算哪一天（早上四点前算前一天）
fn work_day(ms: i64) -> String {
    let (y, mo, d, h, ..) = crate::util::local_at(ms);
    if h < NEW_DAY_HOUR {
        let (y, mo, d, ..) = crate::util::local_at(ms - 24 * 3600_000);
        return format!("{y}-{mo:02}-{d:02}");
    }
    format!("{y}-{mo:02}-{d:02}")
}

/// 上下文用了几成
async fn context_ratio(app: &App, brain: &str) -> f64 {
    let Ok(u) = app.chat.usage(brain).await else { return 0.0 };
    match (u["context"].as_f64(), u["window"].as_f64()) {
        (Some(c), Some(w)) if w > 0.0 => c / w,
        _ => 0.0,
    }
}

/// 让大脑原地压缩（Claude Code 带上要保留什么）
async fn compact(app: &App, brain: &str) {
    let text = match crate::assistant::settings(&app.store).engine {
        crate::assistant::Engine::Claude => "/compact 保留：在干的活和它们的会话 id、没回答的问题、用户最近的决定、还有效的约定、在等的事。细节（路径、命令、配置值）不用留，要用时去搜原文。",
        crate::assistant::Engine::Codex => "/compact",
    };
    if let Err(e) = app.chat.input(brain, text, &[]).await {
        tracing::warn!(error = ?e, "助理：压缩没发出去");
    }
    update(&app.store, |j| j.compacted = crate::util::system_ms());
}

fn spawn_job(app: &App, f: impl FnOnce(App) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + 'static) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        f(app).await;
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// 后台看守每一圈调一次（gateway::assistant::run）
pub async fn tend(app: &App) {
    let Some(brain) = crate::assistant::brain_id(&app.store) else { return };
    let now = crate::util::system_ms();
    // 全文索引：定时在后台线程里更新
    if now - INDEXED_AT.load(Ordering::SeqCst) >= INDEX_EVERY_MS && !INDEXING.swap(true, Ordering::SeqCst) {
        INDEXED_AT.store(now, Ordering::SeqCst);
        let (paths, store) = (app.paths.clone(), app.store.clone());
        tokio::task::spawn_blocking(move || {
            crate::assistant_index::refresh(&paths, &store);
            INDEXING.store(false, Ordering::SeqCst);
        });
    }
    let j = jobs(&app.store);
    if !j.seeded && !running() {
        let n = seed(app).await;
        if n > 0 {
            tracing::info!(n, "助理记忆：第一次启用，项目名册草稿等用户过目");
        }
    }
    if running() || app.chat.busy(&brain) {
        return;
    }
    let b = app.chat.brief(&brain).unwrap_or_default();
    let active = b["active_at"].as_i64().unwrap_or(now);
    let idle = now - active;
    // 隔夜换新的后半步：压缩做完了，重起大脑带上新的常驻记忆
    if j.restart_pending {
        if now - j.compacted > 20_000 {
            update(&app.store, |j| j.restart_pending = false);
            if let Err(e) = app.chat.restart(&brain).await {
                tracing::warn!(error = ?e, "助理：隔夜换新时重起没成");
            } else {
                tracing::info!("助理：隔夜换新，带上新的记忆接着同一段对话");
            }
        }
        return;
    }
    if idle < IDLE_MS {
        return;
    }
    let bg = bg_enabled(&app.store);
    let n_turns = turns(&app.store);
    let overnight = idle >= OVERNIGHT_IDLE_MS && j.rollover_day != work_day(now) && work_day(active) != work_day(now) && n_turns > j.turns_at_rollover;
    let full = now - j.compacted > 10 * 60_000 && context_ratio(app, &brain).await >= COMPACT_AT;
    if overnight || full {
        spawn_job(app, move |app| {
            Box::pin(async move {
                if bg {
                    if let Err(e) = reflect(&app).await {
                        tracing::info!(error = %e, "助理记忆：压缩前的复盘没做成");
                    }
                }
                let Some(brain) = crate::assistant::brain_id(&app.store) else { return };
                compact(&app, &brain).await;
                if overnight {
                    let day = work_day(crate::util::system_ms());
                    let t = turns(&app.store);
                    update(&app.store, |j| {
                        j.rollover_day = day;
                        j.turns_at_rollover = t;
                        j.restart_pending = true;
                    });
                }
            })
        });
        return;
    }
    if !bg {
        return;
    }
    if n_turns >= j.turns_at_reflect + REFLECT_EVERY {
        spawn_job(app, |app| {
            Box::pin(async move {
                if let Err(e) = reflect(&app).await {
                    update(&app.store, |j| {
                        j.reflected = crate::util::system_ms();
                        j.reflect_result = json!({ "error": e });
                        j.turns_at_reflect = turns(&app.store);
                    });
                }
            })
        });
    } else if j.tidy_day != today() && idle >= TIDY_IDLE_MS {
        spawn_job(app, |app| {
            Box::pin(async move {
                if let Err(e) = tidy(&app).await {
                    update(&app.store, |j| {
                        j.tidy_day = today();
                        j.tidied = crate::util::system_ms();
                        j.tidy_result = json!({ "error": e });
                    });
                }
            })
        });
    }
}

/// 记忆页上点“现在复盘 / 现在整理”
pub async fn run_now(app: &App, what: &str) -> Result<Value, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("another memory job is running".into());
    }
    let r = match what {
        "reflect" => reflect(app).await,
        "tidy" => tidy(app).await,
        "seed" => {
            update(&app.store, |j| j.seeded = false);
            Ok(json!({ "seeded": seed(app).await }))
        }
        "index" => {
            let (paths, store) = (app.paths.clone(), app.store.clone());
            tokio::task::spawn_blocking(move || crate::assistant_index::rebuild(&paths, &store)).await.map_err(|e| e.to_string())?;
            INDEXED_AT.store(crate::util::system_ms(), Ordering::SeqCst);
            Ok(json!({ "rebuilt": true }))
        }
        _ => Err("unknown job".into()),
    };
    RUNNING.store(false, Ordering::SeqCst);
    r
}

/// 记忆页显示用
pub fn view(store: &crate::store::Store) -> Value {
    let j = jobs(store);
    json!({ "enabled": bg_enabled(store), "running": running(), "reflected": j.reflected, "reflect_result": j.reflect_result, "tidied": j.tidied, "tidy_result": j.tidy_result, "compacted": j.compacted, "turns": turns(store), "turns_at_reflect": j.turns_at_reflect, "reflect_every": REFLECT_EVERY })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_in_answers() {
        assert_eq!(json_in("好的：\n```json\n{\"ops\":[]}\n```").unwrap()["ops"], json!([]));
        assert!(json_in("没有").is_none());
    }

    #[test]
    fn work_day_starts_at_four() {
        let now = crate::util::system_ms();
        let (_, _, _, h, ..) = crate::util::local_at(now);
        if h >= NEW_DAY_HOUR {
            assert_eq!(work_day(now), today());
        }
    }
}
