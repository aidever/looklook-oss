//! 助理的全文索引（设计稿 §5.1“搜原文”）：本机 SQLite FTS5，不调模型，返回原文和前后几句。
//!
//! - 索引的东西：记忆页（按小节）、每日流水、助理的全部对话、看看里所有 Claude Code / Codex 会话的记录、
//!   导进来只供搜索的 Claude Code / Codex 自己的记忆（`imported`）。
//! - **索引只是索引**：放在 `memory/index.sqlite`，坏了、对不上了就删掉重建（[`rebuild`]），原始的记录和记忆文件才是底。
//! - 中文没有空格：字按相邻两个一组（“支付回调” → 支付 付回 回调）放进 FTS，查的时候同样拆，英文按词。
//! - 记录文件按大小和修改时间增量更新；“彻底忘掉”的那段记下指纹，以后重建也不再收进来。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::paths::Paths;

/// 一条消息最多收多少字
const BODY_MAX: usize = 3000;
/// 太大的记录文件不收（多半是工具输出塞满的）
const FILE_MAX: u64 = 200 * 1024 * 1024;
/// 导进来的记忆文件按段切，一段最多多少字
const CHUNK: usize = 1500;

static DB: Mutex<Option<(PathBuf, Connection)>> = Mutex::new(None);

pub fn db_path(paths: &Paths) -> PathBuf {
    crate::assistant_memory::root(paths).join("index.sqlite")
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, src TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL, title TEXT NOT NULL DEFAULT '', workdir TEXT NOT NULL DEFAULT '', conv TEXT NOT NULL DEFAULT '');
CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(toks, body UNINDEXED, src UNINDEXED, ref UNINDEXED, role UNINDEXED, at UNINDEXED, file UNINDEXED, h UNINDEXED, tokenize='unicode61');
CREATE TABLE IF NOT EXISTS forgotten (h TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
";

fn with<T>(paths: &Paths, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Option<T> {
    let mut g = DB.lock().unwrap_or_else(|e| e.into_inner());
    let want = db_path(paths);
    if g.as_ref().is_none_or(|(p, _)| *p != want) {
        let _ = std::fs::create_dir_all(want.parent()?);
        let open = || -> rusqlite::Result<Connection> {
            let c = Connection::open(&want)?;
            let _ = c.pragma_update(None, "journal_mode", "WAL");
            c.execute_batch(SCHEMA)?;
            Ok(c)
        };
        let c = match open() {
            Ok(c) => c,
            Err(e) => {
                // 坏了：删掉重建（索引只是索引）
                tracing::warn!(error = %e, "助理索引打不开，重建");
                let _ = std::fs::remove_file(&want);
                *MEM_STAMP.lock().unwrap_or_else(|e| e.into_inner()) = None;
                open().ok()?
            }
        };
        *g = Some((want, c));
    }
    let (_, c) = g.as_mut()?;
    match f(c) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "助理索引出错");
            None
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// 拆成要放进索引（或者拿去查）的词：中文两字一组，英文、数字按词（小写）
pub fn toks(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut word = String::new();
    let mut run: Vec<char> = vec![];
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<String>| {
        if run.len() == 1 {
            out.push(run[0].to_string());
        }
        for w in run.windows(2) {
            out.push(w.iter().collect());
        }
        run.clear();
    };
    for c in s.chars() {
        if is_cjk(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            run.push(c);
        } else if c.is_alphanumeric() || c == '_' {
            flush_run(&mut run, &mut out);
            word.extend(c.to_lowercase());
        } else {
            flush_run(&mut run, &mut out);
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
    }
    flush_run(&mut run, &mut out);
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn hash(src: &str, r: &str, body: &str) -> String {
    let d = Sha256::digest(format!("{src}\0{r}\0{body}").as_bytes());
    d.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

struct Doc<'a> {
    src: &'a str,
    r: &'a str,
    role: &'a str,
    at: i64,
    file: &'a str,
    body: String,
}

fn insert(c: &Connection, d: &Doc) -> rusqlite::Result<()> {
    let body = clip(d.body.trim(), BODY_MAX);
    if body.is_empty() {
        return Ok(());
    }
    let h = hash(d.src, d.r, &body);
    if c.query_row("SELECT 1 FROM forgotten WHERE h=?1", [&h], |_| Ok(())).optional()?.is_some() {
        return Ok(());
    }
    c.execute("INSERT INTO docs (toks, body, src, ref, role, at, file, h) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![toks(&body).join(" "), body, d.src, d.r, d.role, d.at, d.file, h])?;
    Ok(())
}

/// 上次收记忆、流水时它们文件的样子（没变就不用重收；换了库、重建时清掉）
static MEM_STAMP: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);

/// 记忆页、流水这几个目录里文件的名字、大小、修改时间
fn memory_stamp(paths: &Paths) -> String {
    let r = crate::assistant_memory::root(paths);
    let mut v: Vec<String> = vec![];
    for d in crate::assistant_memory::Dir::ALL.iter().map(|k| k.name()).chain(["journal"]) {
        let Ok(rd) = std::fs::read_dir(r.join(d)) else { continue };
        for f in rd.flatten() {
            let Ok(m) = f.metadata() else { continue };
            let (t, n) = file_stamp(&m);
            v.push(format!("{d}/{}:{t}:{n}", f.file_name().to_string_lossy()));
        }
    }
    v.sort();
    let d = Sha256::digest(v.join("\n").as_bytes());
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// 记忆页、流水：小，变了就整个重收
fn index_memory(paths: &Paths, c: &mut Connection) -> rusqlite::Result<()> {
    let stamp = memory_stamp(paths);
    let want = db_path(paths);
    let mut g = MEM_STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|(p, s)| *p == want && *s == stamp) {
        return Ok(());
    }
    index_memory_now(paths, c)?;
    *g = Some((want, stamp));
    Ok(())
}

fn index_memory_now(paths: &Paths, c: &mut Connection) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    tx.execute("DELETE FROM docs WHERE src IN ('memory','archive','journal')", [])?;
    for p in crate::assistant_memory::pages(paths) {
        let Some(full) = crate::assistant_memory::page(paths, &p.id) else { continue };
        // 按“## ”小节分段：搜到的是那一节
        let body = crate::assistant_memory::read_front(&full.text).1;
        let mut parts: Vec<String> = vec![format!("{} — {}", p.name, p.description)];
        for l in body.lines() {
            if l.starts_with("## ") || parts.last().is_some_and(|x| x.chars().count() > CHUNK) {
                parts.push(String::new());
            }
            let last = parts.last_mut().unwrap();
            last.push_str(l);
            last.push('\n');
        }
        if parts.len() > 1 {
            let head = parts.remove(0);
            parts[0] = format!("{head}\n{}", parts[0]);
        }
        for part in parts {
            insert(&tx, &Doc { src: "memory", r: &p.id, role: p.dir.name(), at: p.updated, file: "", body: part })?;
        }
    }
    let dir = crate::assistant_memory::root(paths).join("journal");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut days: Vec<PathBuf> = rd.flatten().map(|f| f.path()).collect();
        days.sort();
        for f in days {
            let day = f.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Ok(s) = std::fs::read_to_string(&f) else { continue };
            let at0 = day_ms(&day);
            for l in s.lines().filter(|l| !l.trim().is_empty()) {
                let t = l.trim_start_matches("- ");
                let at = at0 + t.get(..5).and_then(|hm| Some(hm[..2].parse::<i64>().ok()? * 3_600_000 + hm[3..].parse::<i64>().ok()? * 60_000)).unwrap_or(0);
                insert(&tx, &Doc { src: "journal", r: &day, role: "", at, file: "", body: t.to_string() })?;
            }
        }
    }
    tx.commit()
}

/// `2026-11-26` → 那天本地零点（毫秒）；解析不了是 0
fn day_ms(day: &str) -> i64 {
    let mut it = day.split('-').map(|x| x.parse::<i64>().ok());
    let (Some(Some(y)), Some(Some(m)), Some(Some(d))) = (it.next(), it.next(), it.next()) else { return 0 };
    crate::util::local_ms(y as i32, m as u32, d as u32).unwrap_or(0)
}

/// 各账号的 Claude Code 配置目录（默认的在 history 里另算）
fn claude_roots(store: &crate::store::Store, home: &Path) -> Vec<PathBuf> {
    crate::claude_profiles::list(store).iter().map(|p| crate::claude_profiles::config_paths(home, p).0).collect()
}

/// 导进来只供搜索的：Claude Code 各项目的自动记忆、Codex 的记忆（设计稿 §5.6）
pub fn imported_files(store: &crate::store::Store, paths: &Paths) -> Vec<PathBuf> {
    let mut roots = claude_roots(store, &paths.home);
    roots.push(crate::chat::models::config_dir());
    roots.sort();
    roots.dedup();
    let own = crate::assistant::dir(paths);
    let own_key: String = own.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut out = vec![];
    for r in roots {
        let Ok(rd) = std::fs::read_dir(r.join("projects")) else { continue };
        for proj in rd.flatten() {
            // 助理自己的工作目录不要（它不用 Claude Code 自带的记忆）
            if proj.file_name().to_string_lossy().starts_with(&own_key) {
                continue;
            }
            if let Ok(md) = std::fs::read_dir(proj.path().join("memory")) {
                out.extend(md.flatten().map(|f| f.path()).filter(|p| p.extension().is_some_and(|x| x == "md")));
            }
        }
    }
    let codex = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    if let Some(c) = codex {
        let mut stack = vec![(c.join("memories"), 0)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for f in rd.flatten() {
                let p = f.path();
                if depth < 3 && p.is_dir() {
                    stack.push((p, depth + 1));
                } else if p.extension().is_some_and(|x| x == "md") {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// 大脑以前用 Claude Code 自带记忆时留下的目录（所有账号下 `projects/<大脑目录>/memory/`）：升级时导入成页一次
pub fn brain_memory_dirs(store: &crate::store::Store, paths: &Paths) -> Vec<PathBuf> {
    let mut roots = claude_roots(store, &paths.home);
    roots.push(crate::chat::models::config_dir());
    roots.sort();
    roots.dedup();
    let own_key: String = crate::assistant::dir(paths).to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut out = vec![];
    for r in roots {
        let Ok(rd) = std::fs::read_dir(r.join("projects")) else { continue };
        for proj in rd.flatten() {
            if proj.file_name().to_string_lossy().starts_with(&own_key) && proj.path().join("memory").is_dir() {
                out.push(proj.path().join("memory"));
            }
        }
    }
    out
}

fn file_stamp(m: &std::fs::Metadata) -> (i64, i64) {
    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
    (mtime, m.len() as i64)
}

/// 对话记录、导进来的记忆：改过的文件才重收。返回收了几份文件。
/// 读文件、拆对话（可能很大）时不占着库：只在写一份文件的那一下拿锁，搜索、记忆页不用等它整个扫完
fn index_files(paths: &Paths, store: &crate::store::Store) -> usize {
    // 测试里不去扫这台电脑上真的对话记录
    if cfg!(test) {
        return 0;
    }
    let known: HashMap<String, (i64, i64)> = with(paths, |c| {
        let mut st = c.prepare("SELECT path, mtime, size FROM files")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))))?.collect();
        v
    })
    .unwrap_or_default();
    let own = crate::assistant::dir(paths);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut n = 0;
    for (p, m, agent) in crate::history::log_files(&claude_roots(store, &paths.home)) {
        let key = p.display().to_string();
        seen.insert(key.clone());
        let stamp = file_stamp(&m);
        if m.len() > FILE_MAX || known.get(&key) == Some(&stamp) {
            continue;
        }
        let Some(t) = crate::history::log_text(&p, agent) else { continue };
        // 助理自己的对话（大脑、跑腿都在助理目录里起）单独一类
        let src = if !t.workdir.is_empty() && Path::new(&t.workdir).starts_with(&own) { "assistant" } else { agent };
        let ok = with(paths, |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM docs WHERE file=?1", [&key])?;
            for (role, text, at) in &t.messages {
                insert(&tx, &Doc { src, r: &t.id, role, at: *at, file: &key, body: text.clone() })?;
            }
            tx.execute("INSERT OR REPLACE INTO files (path, src, mtime, size, title, workdir, conv) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![key, src, stamp.0, stamp.1, clip(&t.title, 80), t.workdir, t.id])?;
            tx.commit()
        });
        n += ok.is_some() as usize;
    }
    for p in imported_files(store, paths) {
        let key = p.display().to_string();
        seen.insert(key.clone());
        let Ok(m) = std::fs::metadata(&p) else { continue };
        let stamp = file_stamp(&m);
        if known.get(&key) == Some(&stamp) {
            continue;
        }
        let Ok(s) = std::fs::read_to_string(&p) else { continue };
        // 按空行分段，再拼到不超过一段的长度
        let mut chunk = String::new();
        let mut parts = vec![];
        for para in s.split("\n\n") {
            if chunk.chars().count() + para.chars().count() > CHUNK && !chunk.is_empty() {
                parts.push(std::mem::take(&mut chunk));
            }
            chunk.push_str(para);
            chunk.push_str("\n\n");
        }
        parts.push(chunk);
        let title = imported_title(&p);
        let ok = with(paths, |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM docs WHERE file=?1", [&key])?;
            for part in &parts {
                insert(&tx, &Doc { src: "imported", r: &key, role: "", at: stamp.0, file: &key, body: part.clone() })?;
            }
            tx.execute("INSERT OR REPLACE INTO files (path, src, mtime, size, title, workdir, conv) VALUES (?1,'imported',?2,?3,?4,'','')", params![key, stamp.0, stamp.1, title])?;
            tx.commit()
        });
        n += ok.is_some() as usize;
    }
    // 没了的文件：从索引里去掉
    let gone: Vec<String> = known.into_keys().filter(|p| !seen.contains(p)).collect();
    if !gone.is_empty() {
        with(paths, |c| {
            let tx = c.transaction()?;
            for p in &gone {
                tx.execute("DELETE FROM docs WHERE file=?1", [p])?;
                tx.execute("DELETE FROM files WHERE path=?1", [p])?;
            }
            tx.commit()
        });
    }
    n
}

/// 导进来的记忆文件的名字：“Claude Code · 项目文件夹 · 文件名”或“Codex · 文件名”
fn imported_title(p: &Path) -> String {
    let name = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let comps: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if let Some(i) = comps.iter().position(|c| c == "projects") {
        let proj = comps.get(i + 1).cloned().unwrap_or_default();
        let proj = proj.rsplit('-').find(|s| !s.is_empty()).unwrap_or(&proj).to_string();
        return format!("Claude Code · {proj} · {name}");
    }
    format!("Codex · {name}")
}

/// 更新索引：记忆、流水整个重收；记录文件改过的才重收（第一次会久一些）。在后台线程里调
pub fn refresh(paths: &Paths, store: &crate::store::Store) {
    // 同一时间只跑一个（后台定时的和手动重建的不要一起扫）
    static ONE: Mutex<()> = Mutex::new(());
    let _g = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let started = std::time::Instant::now();
    refresh_memory(paths);
    let n = index_files(paths, store);
    if n > 0 {
        tracing::info!(files = n, ms = started.elapsed().as_millis() as u64, "助理索引：更新了");
    }
}

/// 只重收记忆和流水（搜之前、改了记忆以后，很快）
pub fn refresh_memory(paths: &Paths) {
    with(paths, |c| index_memory(paths, c));
}

/// 删掉重建
pub fn rebuild(paths: &Paths, store: &crate::store::Store) {
    {
        let mut g = DB.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }
    // 彻底忘掉的指纹要留着：先读出来
    let forgotten: Vec<String> = with(paths, |c| {
        let mut st = c.prepare("SELECT h FROM forgotten")?;
        let v = st.query_map([], |r| r.get(0))?.collect();
        v
    })
    .unwrap_or_default();
    {
        let mut g = DB.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }
    let _ = std::fs::remove_file(db_path(paths));
    for ext in ["sqlite-wal", "sqlite-shm"] {
        let _ = std::fs::remove_file(db_path(paths).with_extension(ext));
    }
    *MEM_STAMP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    with(paths, |c| {
        for h in &forgotten {
            c.execute("INSERT OR IGNORE INTO forgotten (h) VALUES (?1)", [h])?;
        }
        Ok(())
    });
    refresh(paths, store);
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    /// memory | journal | assistant | claude | codex | agy | imported
    pub src: String,
    /// 记忆页 id、流水的日期、对话 id、导入文件的路径
    #[serde(rename = "ref")]
    pub r: String,
    pub role: String,
    pub at: i64,
    /// 对话标题、导入文件的名字
    pub title: String,
    pub workdir: String,
    /// 命中的那一段（前后几句）
    pub snippet: String,
    /// 位置（也是彻底忘掉用的指纹）：`source_read` 按它展开前后文，整理的依据记它
    pub h: String,
    /// 记录文件（对话、导入的记忆）
    #[serde(skip_serializing_if = "String::is_empty")]
    pub file: String,
}

fn snippet(body: &str, terms: &[String]) -> String {
    let lower = body.to_lowercase();
    let at = terms.iter().filter_map(|t| lower.find(t.as_str())).min().map(|b| lower[..b].chars().count()).unwrap_or(0);
    let chars: Vec<char> = body.chars().collect();
    let from = at.saturating_sub(100);
    let to = (at + 200).min(chars.len());
    let mut s: String = chars[from..to].iter().collect();
    if from > 0 {
        s.insert(0, '…');
    }
    if to < chars.len() {
        s.push('…');
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按词找（每个词都要有）。`srcs` 只找这几类（空 = 全部）。新的、命中好的在前
pub fn search(paths: &Paths, q: &str, srcs: &[&str], limit: usize) -> Vec<Hit> {
    refresh_memory(paths);
    let words: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    let ts: Vec<String> = words.iter().flat_map(|w| toks(w)).collect();
    if ts.is_empty() {
        return vec![];
    }
    let fts = ts.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join(" ");
    let filter = if srcs.is_empty() { String::new() } else { format!(" AND src IN ({})", srcs.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(",")) };
    let rows: Vec<(String, String, String, String, i64, String, String)> = with(paths, |c| {
        let sql = format!("SELECT body, src, ref, role, at, file, h FROM docs WHERE docs MATCH ?1{filter} ORDER BY rank LIMIT ?2");
        let mut st = c.prepare(&sql)?;
        let v = st.query_map(params![fts, (limit * 4) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, i64>(4)?, r.get(5)?, r.get(6)?)))?.collect();
        v
    })
    .unwrap_or_default();
    // 两字一组会有误中（“支付回调” 能配上 “支付…付回…回调” 散在各处的）：再按原词核对一遍
    let files: HashMap<String, (String, String)> = with(paths, |c| {
        let mut st = c.prepare("SELECT path, title, workdir FROM files")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, String>(2)?))))?.collect();
        v
    })
    .unwrap_or_default();
    let mut out: Vec<Hit> = rows
        .into_iter()
        .filter(|(body, ..)| {
            let b = body.to_lowercase();
            words.iter().all(|w| b.contains(w.as_str()))
        })
        .map(|(body, src, r, role, at, file, h)| {
            let (title, workdir) = files.get(&file).cloned().unwrap_or_default();
            Hit { snippet: snippet(&body, &words), src, r, role, at, title, workdir, h, file }
        })
        .collect();
    out.truncate(limit);
    out
}

/// 彻底忘掉索引里的一段：删掉，记下指纹，以后不再收。返回那一段（调用方再去原记录里处理）
pub fn forget(paths: &Paths, h: &str) -> Option<Hit> {
    with(paths, |c| {
        let hit = c
            .query_row("SELECT body, src, ref, role, at, file FROM docs WHERE h=?1", [h], |r| {
                Ok(Hit { snippet: r.get(0)?, src: r.get(1)?, r: r.get(2)?, role: r.get(3)?, at: r.get(4)?, file: r.get(5)?, title: String::new(), workdir: String::new(), h: h.to_string() })
            })
            .optional()?;
        c.execute("INSERT OR IGNORE INTO forgotten (h) VALUES (?1)", [h])?;
        c.execute("DELETE FROM docs WHERE h=?1", [h])?;
        Ok(hit)
    })
    .flatten()
}

/// 按位置展开前后文用的一段
#[derive(Debug, Clone, Serialize)]
pub struct Piece {
    pub h: String,
    pub role: String,
    pub at: i64,
    pub body: String,
    /// 就是要找的那一段
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub src: String,
    #[serde(rename = "ref")]
    pub r: String,
    pub title: String,
    pub workdir: String,
    pub pieces: Vec<Piece>,
}

/// 一个位置的前后文：同一份记录（同一个对话、同一天的流水、同一页）里前后各 `around` 段
pub fn source_read(paths: &Paths, h: &str, around: usize) -> Option<Source> {
    refresh_memory(paths);
    let around = around.clamp(0, 20) as i64;
    with(paths, |c| {
        let Some((rowid, src, r, file)) = c.query_row("SELECT rowid, src, ref, file FROM docs WHERE h=?1", [h], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, String>(1)?, x.get::<_, String>(2)?, x.get::<_, String>(3)?))).optional()? else {
            return Ok(None);
        };
        let (title, workdir) = c.query_row("SELECT title, workdir FROM files WHERE path=?1", [&file], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?))).optional()?.unwrap_or_default();
        let same = |sql: &str| -> rusqlite::Result<Vec<(i64, Piece)>> {
            let mut st = c.prepare(sql)?;
            let v = st.query_map(params![src, r, file, rowid, around], |x| Ok((x.get::<_, i64>(0)?, Piece { h: x.get(1)?, role: x.get(2)?, at: x.get(3)?, body: x.get(4)?, hit: false })))?.collect();
            v
        };
        let mut before = same("SELECT rowid, h, role, at, body FROM docs WHERE src=?1 AND ref=?2 AND file=?3 AND rowid<?4 ORDER BY rowid DESC LIMIT ?5")?;
        before.reverse();
        let mut mid = same("SELECT rowid, h, role, at, body FROM docs WHERE src=?1 AND ref=?2 AND file=?3 AND rowid=?4 LIMIT ?5 + 1")?;
        for m in &mut mid {
            m.1.hit = true;
        }
        let after = same("SELECT rowid, h, role, at, body FROM docs WHERE src=?1 AND ref=?2 AND file=?3 AND rowid>?4 ORDER BY rowid LIMIT ?5")?;
        let pieces = before.into_iter().chain(mid).chain(after).map(|x| x.1).collect();
        Ok(Some(Source { src, r, title, workdir, pieces }))
    })
    .flatten()
}

/// 某个时刻以后的原始记录（整理拼包用）：`srcs` 这几类，`role` 只要这一种（空 = 都要），从旧到新，最多 `limit` 段
pub fn since(paths: &Paths, srcs: &[&str], role: &str, at: i64, limit: usize) -> Vec<Hit> {
    if srcs.is_empty() {
        return vec![];
    }
    let filter = srcs.iter().map(|s| format!("'{}'", s.replace('\'', ""))).collect::<Vec<_>>().join(",");
    let files: HashMap<String, (String, String)> = with(paths, |c| {
        let mut st = c.prepare("SELECT path, title, workdir FROM files")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, String>(2)?))))?.collect();
        v
    })
    .unwrap_or_default();
    with(paths, |c| {
        let sql = format!("SELECT body, src, ref, role, at, file, h FROM docs WHERE src IN ({filter}) AND at>?1 AND (?2='' OR role=?2) ORDER BY at, rowid LIMIT ?3");
        let mut st = c.prepare(&sql)?;
        let v = st
            .query_map(params![at, role, limit as i64], |r| {
                let file: String = r.get(5)?;
                let (title, workdir) = files.get(&file).cloned().unwrap_or_default();
                Ok(Hit { snippet: r.get(0)?, src: r.get(1)?, r: r.get(2)?, role: r.get(3)?, at: r.get(4)?, title, workdir, h: r.get(6)?, file })
            })?
            .collect();
        v
    })
    .unwrap_or_default()
}

/// 助理自己的对话记录文件（导出用）
pub fn assistant_logs(paths: &Paths) -> Vec<PathBuf> {
    with(paths, |c| {
        let mut st = c.prepare("SELECT path FROM files WHERE src='assistant'")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>();
        v
    })
    .unwrap_or_default()
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// 某个文件改了（比如刚抹掉了一段）：下次更新时重收
pub fn touch_file(paths: &Paths, file: &str) {
    with(paths, |c| c.execute("DELETE FROM files WHERE path=?1", [file]).map(|_| ()));
}

/// 导进来的记忆文件（记忆页列出来，可以挑了升级成助理自己的）
#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub path: String,
    pub title: String,
    pub text: String,
}

pub fn imported(store: &crate::store::Store, paths: &Paths) -> Vec<Imported> {
    imported_files(store, paths)
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(Imported { title: imported_title(&p), path: p.display().to_string(), text: clip(&text, 4000) })
        })
        .collect()
}

/// 索引现在收了多少（记忆页上显示）
pub fn stats(paths: &Paths) -> serde_json::Value {
    with(paths, |c| {
        let docs: i64 = c.query_row("SELECT count(*) FROM docs", [], |r| r.get(0))?;
        let files: i64 = c.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
        Ok(serde_json::json!({ "docs": docs, "files": files }))
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_bigrams() {
        assert_eq!(toks("修支付回调 Bug-42"), ["修支", "支付", "付回", "回调", "bug", "42"]);
        assert_eq!(toks("好"), ["好"]);
    }

    #[test]
    fn index_memory_journal_and_logs() {
        use crate::assistant_memory as mem;
        let d = tempfile::tempdir().unwrap();
        let p = Paths { home: d.path().to_path_buf(), ttyd: None, mux: None, termd: None, fonts: None, trzsz: None };
        let store = crate::store::Store::open(&d.path().join("s.db")).unwrap();
        let page = "---\nname: shop\ndescription: 商城\nmetadata:\n  type: project\n---\n\n## 支付回调\n\nshop 支付回调签名用 RSA，回调地址 /api/pay/notify\n";
        mem::apply(&p, mem::Edit::Put { page: "projects/shop".into(), text: page.into() }, &mem::Actor::user(""), &[]).unwrap();
        mem::journal(&p, "派活 · 修支付回调 · 用户说回调没到");
        refresh(&p, &store);
        let hits = search(&p, "支付回调", &[], 10);
        assert!(hits.iter().any(|h| h.src == "memory" && h.r == "projects/shop"), "{hits:?}");
        assert!(hits.iter().any(|h| h.src == "journal" && h.at > 0));
        assert!(search(&p, "回调 RSA", &["memory"], 10).len() == 1);
        // 位置能展开
        let m = hits.iter().find(|h| h.src == "memory").unwrap();
        let s = source_read(&p, &m.h, 2).unwrap();
        assert!(s.pieces.iter().any(|x| x.hit && x.body.contains("RSA")));
        assert_eq!(since(&p, &["journal"], "", 0, 10).len(), 1);
        // 散在各处的两字组不算
        assert!(search(&p, "支付地址", &[], 10).is_empty());
        // 彻底忘掉：删掉，重收也不回来
        let j = hits.iter().find(|h| h.src == "journal").unwrap().h.clone();
        assert!(forget(&p, &j).is_some());
        refresh(&p, &store);
        assert!(search(&p, "支付回调", &["journal"], 10).is_empty());
        rebuild(&p, &store);
        assert!(search(&p, "支付回调", &["journal"], 10).is_empty(), "重建以后也不回来");
        assert!(!search(&p, "支付回调", &["memory"], 10).is_empty());
        // 页改了：下次搜就是新的
        mem::apply(&p, mem::Edit::Replace { page: "projects/shop".into(), old: "RSA".into(), new: "SM2".into() }, &mem::Actor::user(""), &[]).unwrap();
        assert!(search(&p, "SM2", &["memory"], 10).iter().any(|h| h.r == "projects/shop"));
        assert!(search(&p, "RSA", &["memory"], 10).is_empty());
    }
}
