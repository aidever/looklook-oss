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
        other => (other.to_string(), args.clone()),
    }
}

/// 一段内容
#[derive(Debug, Clone)]
enum Entry {
    User { text: String, images: u32, mid: bool },
    Thought(String),
    Text(String),
    Tool { id: String, raw: Value, name: String, input: Value, done: bool },
    Result { id: String, error: bool, text: String },
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

/// ACP 的内容块（`{type: "text", text}`、`{type: "image"}`……）里的文字和图片数
fn content_of(c: &Value) -> (String, u32) {
    match c["type"].as_str().unwrap_or("") {
        "text" => (c["text"].as_str().unwrap_or("").to_string(), 0),
        "image" => (String::new(), 1),
        "resource_link" => (c["uri"].as_str().unwrap_or("").to_string(), 0),
        "resource" => (c["resource"]["text"].as_str().unwrap_or("").to_string(), 0),
        _ => (String::new(), 0),
    }
}

/// 工具结果的文字：`content` 里的文字块，没有就用 `rawOutput`
fn result_text(u: &Value) -> String {
    let mut parts = vec![];
    for c in u["content"].as_array().into_iter().flatten() {
        match c["type"].as_str().unwrap_or("") {
            "content" => {
                let (t, _) = content_of(&c["content"]);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            "diff" => parts.push(format!("{}", c["path"].as_str().unwrap_or(""))),
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
    parts.join("\n")
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
            Entry::User { text, images, mid } => {
                let (text, truncated) = truncate(text, TEXT_LIMIT);
                Body::User { text, images: *images, truncated, mid: *mid }
            }
            Entry::Thought(t) => Body::Assistant { model: None, blocks: vec![Block::Thinking { text: t.clone() }] },
            Entry::Text(t) => Body::Assistant { model: None, blocks: vec![Block::Text { text: t.clone() }] },
            Entry::Tool { id, name, input, .. } => Body::Assistant { model: None, blocks: vec![Block::ToolUse { id: id.clone(), name: name.clone(), input: input.clone(), truncated: false }] },
            Entry::Result { id, error, text } => {
                let (content, truncated) = truncate(text, TEXT_LIMIT);
                Body::ToolResult { tool_use_id: id.clone(), is_error: *error, content, images: 0, truncated, extra: None }
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
    pub fn user(&mut self, text: &str, images: u32) {
        let mid = self.replied;
        self.push(Entry::User { text: text.to_string(), images, mid }, now_iso());
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
                let (text, images) = content_of(&u["content"]);
                // 重放时一条消息的几块（文字、图片）接在一起
                if let Some((Entry::User { text: t, images: n, .. }, _)) = self.entries.last_mut() {
                    if !text.is_empty() {
                        if !t.is_empty() {
                            t.push('\n');
                        }
                        t.push_str(&text);
                    }
                    *n += images;
                    let at = self.entries.len() - 1;
                    self.touch(at);
                    return;
                }
                self.replied = false;
                self.push(Entry::User { text, images, mid: false }, None);
            }
            "agent_message_chunk" | "agent_thought_chunk" => {
                let (text, _) = content_of(&u["content"]);
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
                // 参数可能后来才补全
                let args = u.get("rawInput").or(u.get("args")).filter(|a| a.is_object());
                if let (Some(args), Entry::Tool { raw, name, input, .. }) = (args, &mut self.entries[at].0) {
                    if args != raw {
                        let (n, i) = tool_view(u["name"].as_str().unwrap_or(""), args);
                        *raw = args.clone();
                        *name = n;
                        *input = i;
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
                self.push(Entry::Tool { id: id.to_string(), raw, name, input, done: false }, now_iso());
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
            self.push(Entry::Result { id: id.to_string(), error: status == "failed", text: result_text(u) }, now_iso());
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
            Entry::Result { text, error, .. } => json!({ "content": text, "is_error": error }),
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
}
