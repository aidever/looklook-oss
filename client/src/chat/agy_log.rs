//! Antigravity（agy）会话的记录（docs/AGY_UI.md §3）：ACP 的 `session/update` → 对话界面的条目。
//!
//! agy 的会话文件是它自己的内部格式，不直接读：内容全部来自 ACP。打开会话时 `session/load` 先把历史
//! 一条条重放成 `session/update`（用户消息、思考、工具调用、回复），之后的轮次实时推来。条目存在内存里
//! （[`AgyLog`]，按会话共用），对话界面的跟读（[`TranscriptReader::poll`]）取走新整理出来的部分。
//!
//! - 实时的这一轮里 agy 不回放用户自己发的话：发出去时由看看记进来（[`AgyLog::user`]）；
//! - 回复、思考是一段段增量（`agent_message_chunk` / `agent_thought_chunk`）：接在上一条同类的后面；
//! - 工具调用（`tool_call`、`tool_call_update`）翻成 Claude Code 的说法（`run_command` → `Bash`，
//!   `edit_file` → `Edit`……，参数名也换成一样的），界面直接复用 Claude 的卡片；结束时（`completed` / `failed`）
//!   再加一条结果；
//! - 计划（`plan`）显示成 `TodoWrite` 卡片。
//!
//! 条目的 key 是 `g:<第几段>:<序号>`，同一段内容增长时 key 不变（界面原地更新）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use super::agent::TranscriptReader;
use super::transcript::{truncate, Block, Body, Change, Item, Stats, TEXT_LIMIT};

/// 记录在内存里的路径：`agy/<会话>`（不是真的文件，只用来区分会话）
pub fn log_path(session: &str) -> PathBuf {
    PathBuf::from(format!("agy/{session}"))
}

pub fn session_of(path: &Path) -> Option<&str> {
    path.to_str()?.strip_prefix("agy/")
}

/// agy 的会话号是 UUID
pub fn valid_session(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// 保留原始参数并补充可读字段（已存在且非空时不覆盖）
fn enrich(args: &Value, extras: &[(&str, Value)]) -> Value {
    let mut m = args.as_object().cloned().unwrap_or_default();
    for (k, v) in extras {
        if !v.is_null() && !v.as_str().is_some_and(str::is_empty) && m.get(*k).is_none_or(Value::is_null) {
            m.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(m)
}

/// agy 的工具 → Claude Code 的工具名和参数（界面的卡片按 Claude 的认）。认不出的原样留着
pub fn tool_view(name: &str, args: &Value) -> (String, Value) {
    let s = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let pick = |pairs: &[(&str, &str)]| {
        let mut m = Map::new();
        for (from, to) in pairs {
            if let Some(v) = args.get(*from).filter(|v| !v.is_null()) {
                m.insert((*to).to_string(), v.clone());
            }
        }
        Value::Object(m)
    };
    match name {
        "run_command" => {
            let mut v = pick(&[("CommandLine", "command"), ("toolSummary", "description"), ("Cwd", "cwd")]);
            if v.get("command").is_none() {
                v["command"] = s("command");
            }
            ("Bash".into(), v)
        }
        "read_file" | "view_file" => {
            let mut v = pick(&[("Path", "file_path"), ("AbsolutePath", "file_path")]);
            // agy 的行号从 1 开始、含两头；Claude 的 Read 是起始行和行数
            if let Some(start) = args["StartLine"].as_u64() {
                v["offset"] = json!(start);
                if let Some(end) = args["EndLine"].as_u64().filter(|e| *e >= start) {
                    v["limit"] = json!(end - start + 1);
                }
            }
            ("Read".into(), v)
        }
        "write_file" | "write_to_file" => ("Write".into(), pick(&[("Path", "file_path"), ("TargetFile", "file_path"), ("Content", "content"), ("CodeContent", "content")])),
        "edit_file" | "replace_file_content" => ("Edit".into(), pick(&[("Path", "file_path"), ("TargetFile", "file_path"), ("Target", "old_string"), ("TargetContent", "old_string"), ("Replacement", "new_string"), ("ReplacementContent", "new_string")])),
        "grep" | "grep_search" => ("Grep".into(), pick(&[("Query", "pattern"), ("Pattern", "pattern"), ("SearchPath", "path"), ("Path", "path"), ("Includes", "glob")])),
        "glob" | "find_by_name" => ("Glob".into(), pick(&[("Pattern", "pattern"), ("SearchDirectory", "path"), ("Path", "path")])),
        "list_dir" => ("LS".into(), pick(&[("DirectoryPath", "path"), ("Path", "path")])),
        "search_web" | "web_search" => ("WebSearch".into(), pick(&[("query", "query"), ("Query", "query")])),
        "read_url_content" | "web_fetch" => ("WebFetch".into(), pick(&[("Url", "url"), ("url", "url")])),
        "invoke_subagent" => ("Agent".into(), pick(&[("Role", "description"), ("Prompt", "prompt"), ("TypeName", "subagent_type"), ("Model", "model")])),
        "ask_question" => ("AskUserQuestion".into(), args.clone()),
        "manage_task" => {
            let action = args["Action"].as_str().unwrap_or("");
            let tid = args["TaskId"].as_str().unwrap_or("");
            let desc = if !action.is_empty() && !tid.is_empty() { format!("{action} {tid}") } else { action.to_string() };
            ("manage_task".into(), enrich(args, &[("action", s("Action")), ("task_id", s("TaskId")), ("description", json!(desc))]))
        }
        "schedule" => {
            let desc = args["CronExpression"]
                .as_str()
                .map(String::from)
                .or_else(|| args["DurationSeconds"].as_u64().map(|sec| format!("{sec}s")))
                .unwrap_or_default();
            ("schedule".into(), enrich(args, &[("description", json!(desc)), ("prompt", s("Prompt"))]))
        }
        "run_workflow" => ("run_workflow".into(), enrich(args, &[("description", s("Description")), ("path", s("ScriptPath")), ("cwd", s("Cwd"))])),
        "swarm" => {
            let action = args["action"].as_str().unwrap_or("");
            let target = args["name"].as_str().or(args["title"].as_str()).unwrap_or("");
            let desc = if !action.is_empty() && !target.is_empty() { format!("{action} {target}") } else { action.to_string() };
            ("swarm".into(), enrich(args, &[("description", json!(desc))]))
        }
        "call_mcp_tool" => {
            let srv = args["ServerName"].as_str().unwrap_or("");
            let tool = args["ToolName"].as_str().unwrap_or("");
            let desc = if !srv.is_empty() && !tool.is_empty() { format!("{srv}/{tool}") } else { tool.to_string() };
            ("call_mcp_tool".into(), enrich(args, &[("server", s("ServerName")), ("tool", s("ToolName")), ("description", json!(desc))]))
        }
        "list_resources" => ("list_resources".into(), enrich(args, &[("server", s("ServerName")), ("description", s("ServerName"))])),
        "read_resource" => {
            let srv = args["ServerName"].as_str().unwrap_or("");
            let uri = args["Uri"].as_str().unwrap_or("");
            let desc = if !srv.is_empty() && !uri.is_empty() { format!("{srv}: {uri}") } else { uri.to_string() };
            ("read_resource".into(), enrich(args, &[("server", s("ServerName")), ("uri", s("Uri")), ("description", json!(desc))]))
        }
        other => (other.to_string(), args.clone()),
    }
}

/// 一段内容
#[derive(Debug, Clone)]
enum Entry {
    User { text: String, images: u32, imgs: Vec<(&'static str, Vec<u8>)>, mid: bool },
    Thought(String),
    Text(String),
    Tool { id: String, raw_name: String, raw: Value, name: String, input: Value, done: bool },
    Result { id: String, error: bool, text: String, extra: Option<Value> },
    Plan(Value),
    Interrupt,
    Info(String),
    Error(String),
    Turn(u64),
}

/// 一个会话整理出来的内容
#[derive(Default)]
pub struct AgyLog {
    entries: Vec<(Entry, Option<String>)>,
    /// 第几段对应第几个条目（每段一个条目）
    items: Vec<Item>,
    /// 工具调用 id → 第几段
    tools: HashMap<String, usize>,
    inputs: HashMap<String, Value>,
    /// 这一轮里已经有过回复（再来的用户消息算插话）
    replied: bool,
    dirty: Option<usize>,
    reset: bool,
    /// `session/load` 已经重放完（或者是新会话）
    pub loaded: bool,
    stats: Stats,
}

fn now_iso() -> Option<String> {
    time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok()
}

pub fn normalize_mime(m: &str) -> Option<&'static str> {
    match m.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        _ => None,
    }
}

/// ACP 的内容块（`{type: "text", text}`、`{type: "image", mimeType, data}`……）里的文字、图片数和解码后的图片
fn content_of(c: &Value) -> (String, u32, Option<(&'static str, Vec<u8>)>) {
    use base64::Engine;
    match c["type"].as_str().unwrap_or("") {
        "text" => (c["text"].as_str().unwrap_or("").to_string(), 0, None),
        "image" => {
            let img = c["mimeType"]
                .as_str()
                .or_else(|| c["mediaType"].as_str())
                .and_then(normalize_mime)
                .and_then(|mime| {
                    let raw = c["data"].as_str()?.trim();
                    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
                    Some((mime, bytes))
                });
            (String::new(), 1, img)
        }
        "resource_link" => (c["uri"].as_str().unwrap_or("").to_string(), 0, None),
        "resource" => (c["resource"]["text"].as_str().unwrap_or("").to_string(), 0, None),
        _ => (String::new(), 0, None),
    }
}

/// 比较旧/新文本生成 Claude 风格的 `structuredPatch`（给界面画 diff 卡片和统计增删行数）
fn diff_to_patch(old: &str, new: &str) -> Vec<Value> {
    if old == new {
        return vec![];
    }
    let old_lines: Vec<&str> = if old.is_empty() { vec![] } else { old.lines().collect() };
    let new_lines: Vec<&str> = if new.is_empty() { vec![] } else { new.lines().collect() };

    let mut prefix = 0usize;
    let min_len = old_lines.len().min(new_lines.len());
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < min_len - prefix && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix] {
        suffix += 1;
    }

    let mut ops: Vec<(char, &str)> = Vec::with_capacity(old_lines.len() + new_lines.len());
    for &l in &old_lines[..prefix] {
        ops.push((' ', l));
    }

    let mid_old = &old_lines[prefix..old_lines.len() - suffix];
    let mid_new = &new_lines[prefix..new_lines.len() - suffix];
    if mid_old.len().saturating_mul(mid_new.len()) <= 250_000 {
        let n = mid_old.len();
        let m = mid_new.len();
        let mut dp = vec![0u32; (n + 1) * (m + 1)];
        let w = m + 1;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * w + j] = if mid_old[i] == mid_new[j] {
                    dp[(i + 1) * w + (j + 1)] + 1
                } else {
                    dp[(i + 1) * w + j].max(dp[i * w + (j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if mid_old[i] == mid_new[j] {
                ops.push((' ', mid_old[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * w + j] >= dp[i * w + (j + 1)] {
                ops.push(('-', mid_old[i]));
                i += 1;
            } else {
                ops.push(('+', mid_new[j]));
                j += 1;
            }
        }
        while i < n {
            ops.push(('-', mid_old[i]));
            i += 1;
        }
        while j < m {
            ops.push(('+', mid_new[j]));
            j += 1;
        }
    } else {
        for &l in mid_old {
            ops.push(('-', l));
        }
        for &l in mid_new {
            ops.push(('+', l));
        }
    }

    for &l in &old_lines[old_lines.len() - suffix..] {
        ops.push((' ', l));
    }

    const CTX: usize = 3;
    let mut ranges: Vec<(usize, usize)> = vec![];
    for (idx, (t, _)) in ops.iter().enumerate() {
        if *t == ' ' {
            continue;
        }
        let s = idx.saturating_sub(CTX);
        let e = (idx + CTX).min(ops.len() - 1);
        if let Some(last) = ranges.last_mut() {
            if s <= last.1 + 1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        ranges.push((s, e));
    }

    let mut out = vec![];
    for (start, end) in ranges {
        let mut old_before = 0u64;
        let mut new_before = 0u64;
        for &(t, _) in &ops[..start] {
            if t != '+' {
                old_before += 1;
            }
            if t != '-' {
                new_before += 1;
            }
        }
        let mut old_cnt = 0u64;
        let mut new_cnt = 0u64;
        let mut lines: Vec<String> = Vec::with_capacity(end - start + 1);
        for &(t, text) in &ops[start..=end] {
            if t != '+' {
                old_cnt += 1;
            }
            if t != '-' {
                new_cnt += 1;
            }
            lines.push(format!("{t}{text}"));
        }
        let old_start = if old_cnt == 0 { old_before } else { old_before + 1 };
        let new_start = if new_cnt == 0 { new_before } else { new_before + 1 };
        out.push(json!({
            "oldStart": old_start,
            "oldLines": old_cnt,
            "newStart": new_start,
            "newLines": new_cnt,
            "lines": lines,
        }));
    }
    out
}

/// 工具结果的文字和结构化附加信息（`diff` 块转为 `structuredPatch`）
fn result_content(u: &Value) -> (String, Option<Value>) {
    let mut parts = vec![];
    let mut patches: Vec<Value> = vec![];
    let mut diff_path: Option<String> = None;
    for c in u["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or("") {
            "content" => {
                let (t, _, _) = content_of(&c["content"]);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            "diff" => {
                let path = c["path"].as_str().unwrap_or("");
                if !path.is_empty() {
                    parts.push(path.to_string());
                    if diff_path.is_none() {
                        diff_path = Some(path.to_string());
                    }
                }
                let old_text = c["oldText"].as_str().unwrap_or("");
                let new_text = c["newText"].as_str().unwrap_or("");
                patches.extend(diff_to_patch(old_text, new_text));
            }
            "terminal" => parts.push(format!("[terminal {}]", c["terminalId"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    if parts.is_empty() {
        match &u["rawOutput"] {
            Value::Null => {}
            Value::String(s) => parts.push(s.clone()),
            v => match v["output"].as_str().or(v["error"].as_str()) {
                Some(s) => parts.push(s.to_string()),
                None if v.as_object().is_some_and(|o| !o.is_empty()) => parts.push(v.to_string()),
                None => {}
            },
        }
    }
    let extra = if !patches.is_empty() {
        let mut obj = json!({ "structuredPatch": patches });
        if let Some(p) = diff_path {
            obj["filePath"] = json!(p);
        }
        Some(obj)
    } else {
        None
    };
    (parts.join("\n"), extra)
}

impl AgyLog {
    fn mark(&mut self, at: usize) {
        self.dirty = Some(self.dirty.map_or(at, |d| d.min(at)));
    }

    fn push(&mut self, e: Entry, ts: Option<String>) {
        let at = self.entries.len();
        self.entries.push((e, ts));
        self.items.push(Self::item_of(at, &self.entries[at]));
        self.mark(at);
    }

    /// 改了第 `at` 段：重新生成它的条目
    fn touch(&mut self, at: usize) {
        self.items[at] = Self::item_of(at, &self.entries[at]);
        self.mark(at);
    }

    fn item_of(at: usize, (e, ts): &(Entry, Option<String>)) -> Item {
        let body = match e {
            Entry::User { text, images, mid, .. } => {
                let (text, truncated) = truncate(text, TEXT_LIMIT);
                Body::User { text, images: *images, truncated, mid: *mid }
            }
            Entry::Thought(t) => Body::Assistant { model: None, blocks: vec![Block::Thinking { text: t.clone() }] },
            Entry::Text(t) => Body::Assistant { model: None, blocks: vec![Block::Text { text: t.clone() }] },
            Entry::Tool { id, name, input, .. } => Body::Assistant { model: None, blocks: vec![Block::ToolUse { id: id.clone(), name: name.clone(), input: input.clone(), truncated: false }] },
            Entry::Result { id, error, text, extra } => {
                let (content, truncated) = truncate(text, TEXT_LIMIT);
                Body::ToolResult { tool_use_id: id.clone(), is_error: *error, content, images: 0, truncated, extra: extra.clone() }
            }
            Entry::Plan(todos) => Body::Assistant { model: None, blocks: vec![Block::ToolUse { id: format!("plan-{at}"), name: "TodoWrite".into(), input: json!({ "todos": todos }), truncated: false }] },
            Entry::Interrupt => Body::Interrupt,
            Entry::Info(t) => Body::Info { text: t.clone() },
            Entry::Error(t) => Body::Error { text: t.clone() },
            Entry::Turn(ms) => Body::Turn { ms: *ms },
        };
        Item { key: format!("g:{at}:0"), ts: ts.clone(), body }
    }

    /// 看看替用户发出去的一句话（实时的轮次里 agy 不回放它）
    #[cfg(test)]
    pub fn user(&mut self, text: &str, images: u32) {
        self.user_with_images(text, images, vec![]);
    }

    /// 带图片原始数据的用户消息（供对话界面点开看大图）
    pub fn user_with_images(&mut self, text: &str, images: u32, imgs: Vec<(&'static str, Vec<u8>)>) {
        let mid = self.replied;
        let images = images.max(imgs.len() as u32);
        self.push(Entry::User { text: text.to_string(), images, imgs, mid }, now_iso());
    }

    /// 用户消息 `key` 里的第 `n` 张图片
    pub fn user_image(&self, key: &str, n: usize) -> Option<(&'static str, Vec<u8>)> {
        let at: usize = key.strip_prefix("g:")?.split(':').next()?.parse().ok()?;
        match &self.entries.get(at)?.0 {
            Entry::User { imgs, .. } => imgs.get(n).cloned(),
            _ => None,
        }
    }

    /// 一轮开始
    pub fn turn_start(&mut self) {
        self.replied = false;
    }

    /// 一轮结束：`stop` 是 `session/prompt` 的 `stopReason`，`error` 是这一轮出错的说明
    pub fn turn_end(&mut self, stop: Option<&str>, error: Option<&str>, ms: Option<u64>) {
        self.replied = false;
        // 还没结束的工具调用：这一轮已经停了，不会再有结果
        let open: Vec<usize> = self.tools.values().copied().filter(|&i| matches!(&self.entries[i].0, Entry::Tool { done: false, .. })).collect();
        for i in open {
            if let Entry::Tool { done, .. } = &mut self.entries[i].0 {
                *done = true;
            }
        }
        match (stop, error) {
            (_, Some(e)) => self.push(Entry::Error(e.chars().take(1000).collect()), now_iso()),
            (Some("cancelled"), _) => self.push(Entry::Interrupt, now_iso()),
            (Some("end_turn") | None, _) => {}
            (Some(other), _) => self.push(Entry::Info(format!("stop: {other}")), now_iso()),
        }
        if let Some(ms) = ms {
            self.push(Entry::Turn(ms), None);
        }
    }

    /// 一条 `session/update`
    pub fn update(&mut self, u: &Value) {
        self.stats.lines += 1;
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "user_message_chunk" => {
                let (text, images, img) = content_of(&u["content"]);
                // 重放时一条消息的几块（文字、图片）接在一起
                if let Some((Entry::User { text: t, images: n, imgs, .. }, _)) = self.entries.last_mut() {
                    if !text.is_empty() {
                        if !t.is_empty() {
                            t.push('\n');
                        }
                        t.push_str(&text);
                    }
                    *n += images;
                    if let Some(img) = img {
                        imgs.push(img);
                    }
                    let at = self.entries.len() - 1;
                    self.touch(at);
                    return;
                }
                self.replied = false;
                self.push(Entry::User { text, images, imgs: img.into_iter().collect(), mid: false }, None);
            }
            "agent_message_chunk" | "agent_thought_chunk" => {
                let (text, _, _) = content_of(&u["content"]);
                if text.is_empty() {
                    return;
                }
                self.replied = true;
                let thought = kind == "agent_thought_chunk";
                let last = self.entries.len().checked_sub(1);
                if let Some(at) = last {
                    match &mut self.entries[at].0 {
                        Entry::Thought(t) if thought => t.push_str(&text),
                        Entry::Text(t) if !thought => t.push_str(&text),
                        _ => {
                            self.push(if thought { Entry::Thought(text) } else { Entry::Text(text) }, now_iso());
                            return;
                        }
                    }
                    self.touch(at);
                } else {
                    self.push(if thought { Entry::Thought(text) } else { Entry::Text(text) }, now_iso());
                }
            }
            "tool_call" | "tool_call_update" => self.tool(u),
            "plan" => {
                let todos: Vec<Value> = u["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        let c = e["content"].as_str().unwrap_or("");
                        json!({ "content": c, "activeForm": c, "status": e["status"].as_str().unwrap_or("pending") })
                    })
                    .collect();
                self.replied = true;
                self.push(Entry::Plan(Value::Array(todos)), now_iso());
            }
            _ => {}
        }
    }

    fn tool(&mut self, u: &Value) {
        let Some(id) = u["toolCallId"].as_str().filter(|s| !s.is_empty()) else { return };
        let status = u["status"].as_str().unwrap_or("");
        let at = match self.tools.get(id) {
            Some(&at) => {
                // 参数或工具名可能后来才补全；`tool_call_update` 不带 `name` 时复用原来的 `raw_name`
                let args = u.get("rawInput").or(u.get("args")).filter(|a| a.is_object());
                let new_name = u["name"].as_str().filter(|s| !s.is_empty());
                if let Entry::Tool { raw_name, raw, name, input, .. } = &mut self.entries[at].0 {
                    let name_changed = new_name.is_some_and(|n| n != raw_name.as_str());
                    let args_changed = args.is_some_and(|a| a != raw);
                    if name_changed || args_changed {
                        if let Some(n) = new_name {
                            *raw_name = n.to_string();
                        }
                        if let Some(a) = args {
                            *raw = a.clone();
                        }
                        if !raw_name.is_empty() {
                            let (n, i) = tool_view(raw_name, raw);
                            *name = n;
                            *input = i;
                        } else {
                            *input = raw.clone();
                        }
                        self.inputs.insert(id.to_string(), input.clone());
                        self.touch(at);
                    }
                }
                at
            }
            None => {
                let raw = u.get("rawInput").or(u.get("args")).filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
                let tool = u["name"].as_str().unwrap_or("").to_string();
                let (name, input) = if tool.is_empty() { (u["title"].as_str().unwrap_or("tool").to_string(), raw.clone()) } else { tool_view(&tool, &raw) };
                self.inputs.insert(id.to_string(), input.clone());
                self.replied = true;
                self.push(Entry::Tool { id: id.to_string(), raw_name: tool, raw, name, input, done: false }, now_iso());
                let at = self.entries.len() - 1;
                self.tools.insert(id.to_string(), at);
                at
            }
        };
        if matches!(status, "completed" | "failed") {
            if let Entry::Tool { done, .. } = &mut self.entries[at].0 {
                if *done {
                    return;
                }
                *done = true;
            }
            let (text, extra) = result_content(u);
            self.push(Entry::Result { id: id.to_string(), error: status == "failed", text, extra }, now_iso());
        }
    }

    /// 这个工具调用的参数（审批时给界面看）
    pub fn input_of(&self, call: &str) -> Option<Value> {
        self.inputs.get(call).cloned()
    }

    /// 最后一段回复的文字（摘要、提醒用）
    pub fn last_reply(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|(e, _)| if let Entry::Text(t) = e { Some(t.clone()) } else { None })
    }

    /// 重新加载前清空（`session/load` 会把历史全部重放一遍）
    pub fn clear(&mut self) {
        *self = AgyLog { reset: true, ..Default::default() };
    }

    pub fn take(&mut self) -> Change {
        if std::mem::take(&mut self.reset) {
            self.dirty = None;
            return Change::Reset;
        }
        match self.dirty.take() {
            Some(start) => Change::Upsert { start, items: self.items[start.min(self.items.len())..].to_vec() },
            None => Change::None,
        }
    }

    fn raw(&self, key: &str) -> Option<Value> {
        let at: usize = key.strip_prefix("g:")?.split(':').next()?.parse().ok()?;
        let (e, _) = self.entries.get(at)?;
        Some(match e {
            Entry::User { text, .. } | Entry::Thought(text) | Entry::Text(text) | Entry::Info(text) | Entry::Error(text) => json!({ "text": text }),
            Entry::Tool { raw, name, input, .. } => json!({ "name": name, "input": input, "raw": raw }),
            Entry::Result { text, error, extra, .. } => json!({ "content": text, "is_error": error, "extra": extra }),
            Entry::Plan(t) => json!({ "todos": t }),
            Entry::Interrupt | Entry::Turn(_) => Value::Null,
        })
    }
}

// ---------------- 按会话共用 ----------------

static LOGS: std::sync::LazyLock<Mutex<HashMap<String, Arc<Mutex<AgyLog>>>>> = std::sync::LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 这个会话的记录（没有就建一个空的）
pub fn shared(session: &str) -> Arc<Mutex<AgyLog>> {
    lock(&LOGS).entry(session.to_string()).or_default().clone()
}

/// 读这个会话里某条用户消息的第 `n` 张图片
pub fn user_image(session: &str, key: &str, n: usize) -> Option<(&'static str, Vec<u8>)> {
    let log = lock(&LOGS).get(session).cloned()?;
    let out = lock(&log).user_image(key, n);
    out
}

/// 不再跟读这个会话了（换了会话、终端删了）
pub fn drop_log(session: &str) {
    lock(&LOGS).remove(session);
}

/// 对话界面的跟读：读的是 [`shared`] 里的那份
pub struct Reader {
    path: PathBuf,
    log: Arc<Mutex<AgyLog>>,
    items: Vec<Item>,
    inputs: HashMap<String, Value>,
    stats: Stats,
}

impl Reader {
    pub fn new(path: PathBuf) -> Self {
        let log = shared(session_of(&path).unwrap_or_default());
        Self { path, log, items: vec![], inputs: HashMap::new(), stats: Stats::default() }
    }
}

impl TranscriptReader for Reader {
    fn path(&self) -> &Path {
        &self.path
    }

    fn poll(&mut self) -> std::io::Result<Change> {
        let mut log = lock(&self.log);
        let change = log.take();
        if !matches!(change, Change::None) || self.items.len() != log.items.len() {
            self.items = log.items.clone();
            self.inputs = log.inputs.clone();
        }
        self.stats = log.stats;
        Ok(change)
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn items(&self, from: usize, to: usize) -> Vec<Item> {
        let to = to.min(self.items.len());
        self.items[from.min(to)..to].to_vec()
    }

    fn full_item(&self, key: &str) -> Option<Value> {
        lock(&self.log).raw(key)
    }

    fn tool_input(&self, tool_use_id: &str) -> Option<&Value> {
        self.inputs.get(tool_use_id)
    }

    fn version(&self) -> Option<String> {
        None
    }

    fn stats(&self) -> Stats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(v: Value) -> Value {
        v
    }

    fn kinds(log: &AgyLog) -> Vec<String> {
        log.items.iter().map(|i| serde_json::to_value(&i.body).unwrap()["k"].as_str().unwrap_or("").to_string()).collect()
    }

    #[test]
    fn replay_of_a_turn() {
        let mut log = AgyLog::default();
        log.update(&up(json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "do it" } })));
        log.update(&up(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm " } })));
        log.update(&up(json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "ok" } })));
        log.update(&up(json!({
            "sessionUpdate": "tool_call", "toolCallId": "c1", "name": "run_command", "status": "completed",
            "rawInput": { "CommandLine": "ls -la", "toolSummary": "List" },
            "rawOutput": { "output": "total 0", "exit_code": 0 },
            "content": [{ "type": "content", "content": { "type": "text", "text": "total 0" } }]
        })));
        log.update(&up(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "do" } })));
        log.update(&up(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "ne" } })));
        log.update(&up(json!({ "sessionUpdate": "usage_update", "used": 5, "size": 10 })));
        assert_eq!(kinds(&log), ["user", "assistant", "assistant", "tool_result", "assistant"]);
        let v = serde_json::to_value(&log.items[1].body).unwrap();
        assert_eq!(v["blocks"][0]["text"], "hmm ok", "思考接在一起");
        let v = serde_json::to_value(&log.items[2].body).unwrap();
        assert_eq!(v["blocks"][0]["name"], "Bash");
        assert_eq!(v["blocks"][0]["input"], json!({ "command": "ls -la", "description": "List" }));
        let v = serde_json::to_value(&log.items[3].body).unwrap();
        assert_eq!((v["tool_use_id"].as_str(), v["content"].as_str()), (Some("c1"), Some("total 0")));
        assert_eq!(log.last_reply().as_deref(), Some("done"));
        assert_eq!(log.items[4].key, "g:4:0");
        match log.take() {
            Change::Upsert { start, items } => assert_eq!((start, items.len()), (0, 5)),
            _ => panic!(),
        }
        assert!(matches!(log.take(), Change::None));
    }

    #[test]
    fn live_turn_with_pending_tool_and_cancel() {
        let mut log = AgyLog::default();
        log.turn_start();
        log.user("hi", 1);
        log.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "c2", "name": "edit_file", "status": "in_progress", "rawInput": { "Path": "/a.txt", "Target": "x", "Replacement": "y" } }));
        log.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c2", "name": "edit_file", "status": "pending" }));
        assert_eq!(log.input_of("c2").unwrap(), json!({ "file_path": "/a.txt", "old_string": "x", "new_string": "y" }));
        let _ = log.take();
        // 插话：这一轮已经有回复了
        log.user("also", 0);
        let v = serde_json::to_value(&log.items.last().unwrap().body).unwrap();
        assert_eq!(v["mid"], true);
        log.turn_end(Some("cancelled"), None, Some(1200));
        assert_eq!(kinds(&log)[log.items.len() - 2..], ["interrupt", "turn"]);
        // 停了以后同一个调用再报完成也不重复加结果
        log.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "completed" }));
        assert_eq!(kinds(&log).iter().filter(|k| *k == "tool_result").count(), 0);
        match log.take() {
            Change::Upsert { start, .. } => assert_eq!(start, 2),
            _ => panic!(),
        }
    }

    #[test]
    fn failed_tool_errors_and_plan() {
        let mut log = AgyLog::default();
        log.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "c3", "name": "read_file", "status": "in_progress", "rawInput": { "Path": "/x", "StartLine": 3, "EndLine": 7 } }));
        log.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c3", "status": "failed", "rawOutput": { "error": "no such file" } }));
        let v = serde_json::to_value(&log.items[0].body).unwrap();
        assert_eq!(v["blocks"][0]["input"], json!({ "file_path": "/x", "offset": 3, "limit": 5 }));
        let v = serde_json::to_value(&log.items[1].body).unwrap();
        assert_eq!((v["is_error"].as_bool(), v["content"].as_str()), (Some(true), Some("no such file")));
        log.update(&json!({ "sessionUpdate": "plan", "entries": [{ "content": "step", "status": "in_progress", "priority": "high" }] }));
        let v = serde_json::to_value(&log.items[2].body).unwrap();
        assert_eq!(v["blocks"][0]["name"], "TodoWrite");
        assert_eq!(v["blocks"][0]["input"]["todos"][0]["status"], "in_progress");
        log.turn_end(None, Some("quota exceeded"), None);
        assert_eq!(kinds(&log).last().unwrap(), "error");
        assert_eq!(log.raw("g:0:0").unwrap()["name"], "Read");
        log.clear();
        assert!(matches!(log.take(), Change::Reset));
        assert!(log.items.is_empty());
    }

    #[test]
    fn sessions_and_paths() {
        assert!(valid_session("a4bdcd21-6008-4367-95ac-0c4869deac85"));
        assert!(!valid_session("../etc"));
        assert!(!valid_session(""));
        assert_eq!(session_of(&log_path("abc-123")), Some("abc-123"));
        assert_eq!(tool_view("mystery", &json!({ "a": 1 })), ("mystery".to_string(), json!({ "a": 1 })));
    }

    #[test]
    fn tool_call_update_without_name_preserves_mapping() {
        let mut log = AgyLog::default();
        // 先收到不带完整参数的 tool_call，再收到不带 name 的 tool_call_update
        log.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "e1", "name": "edit_file", "status": "in_progress", "rawInput": {} }));
        log.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "in_progress", "rawInput": { "Path": "/foo.rs", "Target": "old", "Replacement": "new" } }));
        let v = serde_json::to_value(&log.items[0].body).unwrap();
        assert_eq!(v["blocks"][0]["name"], "Edit");
        assert_eq!(v["blocks"][0]["input"], json!({ "file_path": "/foo.rs", "old_string": "old", "new_string": "new" }));
        assert_eq!(log.input_of("e1").unwrap()["file_path"], "/foo.rs");

        log.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": "b1", "name": "run_command", "status": "in_progress", "rawInput": {} }));
        log.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "b1", "status": "in_progress", "rawInput": { "CommandLine": "cargo test", "toolSummary": "Run tests" } }));
        let v = serde_json::to_value(&log.items[1].body).unwrap();
        assert_eq!(v["blocks"][0]["name"], "Bash");
        assert_eq!(v["blocks"][0]["input"], json!({ "command": "cargo test", "description": "Run tests" }));
    }

    #[test]
    fn user_images_live_and_replayed() {
        use base64::Engine;
        let sid = "11111111-2222-3333-4444-555555555555";
        drop_log(sid);
        let shared_log = shared(sid);
        {
            let mut log = lock(&shared_log);
            // 实时发送的图片
            log.turn_start();
            log.user_with_images("see attached", 2, vec![("image/png", b"\x89PNG-1".to_vec()), ("image/jpeg", b"\xff\xd8-2".to_vec())]);
            assert_eq!(log.user_image("g:0:0", 0), Some(("image/png", b"\x89PNG-1".to_vec())));
            assert_eq!(log.user_image("g:0:0", 1), Some(("image/jpeg", b"\xff\xd8-2".to_vec())));
            assert!(log.user_image("g:0:0", 2).is_none());
            // 模拟 session/load 重放 base64 图片
            log.clear();
            let b64_a = base64::engine::general_purpose::STANDARD.encode(b"webp-bytes");
            let b64_b = base64::engine::general_purpose::STANDARD.encode(b"gif-bytes");
            log.update(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "replayed" } }));
            log.update(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "image", "mimeType": "image/webp", "data": b64_a } }));
            log.update(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "image", "mimeType": "image/gif", "data": b64_b } }));
            let body = serde_json::to_value(&log.items[0].body).unwrap();
            assert_eq!((body["text"].as_str(), body["images"].as_u64()), (Some("replayed"), Some(2)));
        }
        assert_eq!(user_image(sid, "g:0:0", 0), Some(("image/webp", b"webp-bytes".to_vec())));
        assert_eq!(user_image(sid, "g:0:0", 1), Some(("image/gif", b"gif-bytes".to_vec())));
        assert!(user_image(sid, "g:0:0", 2).is_none());
        assert!(user_image("nonexistent-session", "g:0:0", 0).is_none());
        drop_log(sid);
    }

    #[test]
    fn extended_tool_mappings_and_diff_patch() {
        let (n, v) = tool_view("invoke_subagent", &json!({ "Role": "Code Reviewer", "Prompt": "check src", "TypeName": "research", "Model": "pro" }));
        assert_eq!(n, "Agent");
        assert_eq!(v, json!({ "description": "Code Reviewer", "prompt": "check src", "subagent_type": "research", "model": "pro" }));

        let (n, v) = tool_view("ask_question", &json!({ "questions": [{ "question": "Pick one", "options": ["A", "B"] }] }));
        assert_eq!(n, "AskUserQuestion");
        assert_eq!(v["questions"][0]["question"], "Pick one");

        let (n, v) = tool_view("call_mcp_tool", &json!({ "ServerName": "looklook", "ToolName": "list_mappings", "Arguments": {} }));
        assert_eq!(n, "call_mcp_tool");
        assert_eq!((v["server"].as_str(), v["tool"].as_str(), v["description"].as_str()), (Some("looklook"), Some("list_mappings"), Some("looklook/list_mappings")));
        assert_eq!(v["ServerName"], "looklook", "原始参数保留");

        let (_, v) = tool_view("manage_task", &json!({ "Action": "kill", "TaskId": "t1" }));
        assert_eq!((v["action"].as_str(), v["task_id"].as_str(), v["description"].as_str()), (Some("kill"), Some("t1"), Some("kill t1")));
        let (_, v) = tool_view("schedule", &json!({ "CronExpression": "*/5 * * * *", "Prompt": "ping" }));
        assert_eq!((v["description"].as_str(), v["prompt"].as_str()), (Some("*/5 * * * *"), Some("ping")));
        let (_, v) = tool_view("run_workflow", &json!({ "Description": "fanout", "ScriptPath": "/a.py", "Cwd": "/repo" }));
        assert_eq!((v["description"].as_str(), v["path"].as_str(), v["cwd"].as_str()), (Some("fanout"), Some("/a.py"), Some("/repo")));
        let (_, v) = tool_view("swarm", &json!({ "action": "add", "name": "worker-1" }));
        assert_eq!(v["description"], "add worker-1");
        let (_, v) = tool_view("list_resources", &json!({ "ServerName": "looklook" }));
        assert_eq!(v["server"], "looklook");
        let (_, v) = tool_view("read_resource", &json!({ "ServerName": "looklook", "Uri": "res://a" }));
        assert_eq!((v["uri"].as_str(), v["description"].as_str()), (Some("res://a"), Some("looklook: res://a")));

        // diff content -> structuredPatch
        let mut log = AgyLog::default();
        log.update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "d1", "name": "edit_file", "status": "completed",
            "rawInput": { "Path": "/a.rs" },
            "content": [{ "type": "diff", "path": "/a.rs", "oldText": "line1\nold\nline3\n", "newText": "line1\nnew1\nnew2\nline3\n" }]
        }));
        let res = serde_json::to_value(&log.items[1].body).unwrap();
        let patch = res["extra"]["structuredPatch"].as_array().unwrap();
        assert_eq!(patch.len(), 1);
        assert_eq!(patch[0], json!({ "oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 4, "lines": [" line1", "-old", "+new1", "+new2", " line3"] }));
        assert_eq!(res["extra"]["filePath"], "/a.rs");

        // 新建文件（oldText 为空）
        let p_new = diff_to_patch("", "hello\nworld\n");
        assert_eq!(p_new, vec![json!({ "oldStart": 0, "oldLines": 0, "newStart": 1, "newLines": 2, "lines": ["+hello", "+world"] })]);
    }
}
