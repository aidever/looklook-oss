//! Antigravity (agy) 会话的记录：`agy acp` 推来的 `session/update` 事件 → 对话界面的条目。
//!
//! 结构与 `dsh_log.rs` 相同：内存里按会话 id 共用一份 [`AgyLog`]，
//! 对话界面通过 [`TranscriptReader::poll`] 增量取走新条目。
//! 工具名和参数翻成看看认识的格式（`run_command` → `Bash`、`read_file` → `Read`、`edit_file` → `Edit`……），
//! 这样工具卡片、diff、文件链接直接复用现有界面。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::agent::TranscriptReader;
use super::transcript::{truncate, Block, Body, Change, Item, Stats, TEXT_LIMIT};

/// 记录在内存里的路径：`agy/<会话>`（不是真的文件，只用来区分会话）
pub fn log_path(session: &str) -> PathBuf {
    PathBuf::from(format!("agy/{session}"))
}

pub fn session_of(path: &Path) -> Option<&str> {
    path.to_str()?.strip_prefix("agy/")
}

pub fn valid_session(id: &str) -> bool {
    crate::history::valid_id(id)
}

/// 把 agy 的工具名和参数翻成看看界面认识的格式（Claude Code 风格）
pub fn map_tool(name: &str, raw_input: &Value) -> (String, Value) {
    let args = raw_input.as_object();
    let get = |k: &str| args.and_then(|m| m.get(k)).cloned();
    match name {
        "run_command" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = get("CommandLine").or_else(|| get("command")) {
                m.insert("command".into(), v);
            }
            if let Some(v) = get("toolSummary").or_else(|| get("description")) {
                m.insert("description".into(), v);
            }
            ("Bash".into(), Value::Object(m))
        }
        "read_file" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = get("Path").or_else(|| get("file_path")) {
                m.insert("file_path".into(), v);
            }
            if let Some(v) = get("StartLine") {
                m.insert("offset".into(), v);
            }
            if let Some(v) = get("EndLine") {
                m.insert("end_line".into(), v);
            }
            ("Read".into(), Value::Object(m))
        }
        "write_file" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = get("Path").or_else(|| get("file_path")) {
                m.insert("file_path".into(), v);
            }
            if let Some(v) = get("Content").or_else(|| get("content")) {
                m.insert("content".into(), v);
            }
            ("Write".into(), Value::Object(m))
        }
        "edit_file" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = get("Path").or_else(|| get("file_path")) {
                m.insert("file_path".into(), v);
            }
            if let Some(v) = get("Target").or_else(|| get("old_string")) {
                m.insert("old_string".into(), v);
            }
            if let Some(v) = get("Replacement").or_else(|| get("new_string")) {
                m.insert("new_string".into(), v);
            }
            ("Edit".into(), Value::Object(m))
        }
        "search_web" => ("WebSearch".into(), raw_input.clone()),
        "grep_search" | "grep" => ("Grep".into(), raw_input.clone()),
        "find_by_name" | "glob" => ("Glob".into(), raw_input.clone()),
        "invoke_subagent" => {
            let mut m = serde_json::Map::new();
            if let Some(v) = get("Role").or_else(|| get("description")) {
                m.insert("description".into(), v);
            }
            if let Some(v) = get("Prompt").or_else(|| get("prompt")) {
                m.insert("prompt".into(), v);
            }
            if let Some(v) = get("TypeName") {
                m.insert("subagent_type".into(), v);
            }
            ("Agent".into(), Value::Object(m))
        }
        "ask_question" => ("AskUserQuestion".into(), raw_input.clone()),
        other => (other.to_string(), raw_input.clone()),
    }
}

/// 从 `tool_call_update` / `tool_call` 中提取工具结果文字
fn extract_tool_output(u: &Value) -> String {
    let mut parts = vec![];
    for c in u["content"].as_array().into_iter().flatten() {
        if let Some(t) = c["content"]["text"].as_str() {
            parts.push(t.to_string());
        } else if let Some(t) = c["text"].as_str() {
            parts.push(t.to_string());
        }
    }
    if !parts.is_empty() {
        return parts.join("\n");
    }
    match &u["rawOutput"] {
        Value::String(s) => s.clone(),
        Value::Object(m) => {
            if let Some(o) = m.get("output").and_then(Value::as_str) {
                o.to_string()
            } else if let Some(e) = m.get("error").and_then(Value::as_str) {
                e.to_string()
            } else {
                serde_json::to_string_pretty(&u["rawOutput"]).unwrap_or_default()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 一个会话的事件和整理出来的条目
#[derive(Default)]
pub struct AgyLog {
    items: Vec<Item>,
    raws: HashMap<String, Value>,
    inputs: HashMap<String, Value>,
    user_images: HashMap<String, Vec<(&'static str, Vec<u8>)>>,
    seen_tools: HashSet<String>,
    done_tools: HashSet<String>,
    /// 正在累积的文字 / 思考 / 回放用户消息
    pending_user: String,
    pending_thought: String,
    pending_text: String,
    pending_model: Option<String>,
    seq: u64,
    dirty: Option<usize>,
    reset: bool,
    seeded: bool,
    stats: Stats,
}

impl AgyLog {
    fn next_key(&mut self) -> String {
        self.seq += 1;
        format!("a:{}", self.seq)
    }

    fn mark_dirty(&mut self, at: usize) {
        self.dirty = Some(self.dirty.map_or(at, |d| d.min(at)));
    }

    fn push_item(&mut self, body: Body, raw: Option<Value>) -> String {
        let key = self.next_key();
        let idx = self.items.len();
        self.items.push(Item { key: key.clone(), ts: Some(crate::util::now_rfc3339()), body });
        if let Some(r) = raw {
            self.raws.insert(key.clone(), r);
        }
        self.mark_dirty(idx);
        key
    }

    fn flush_user(&mut self) {
        if self.pending_user.is_empty() {
            return;
        }
        let raw_text = std::mem::take(&mut self.pending_user);
        let (text, truncated) = truncate(&raw_text, TEXT_LIMIT);
        let raw = json!({ "role": "user", "text": raw_text });
        self.push_item(Body::User { text, images: 0, truncated, mid: false }, Some(raw));
    }

    fn flush_assistant(&mut self, extra_tool: Option<Block>) {
        let thought = std::mem::take(&mut self.pending_thought);
        let text = std::mem::take(&mut self.pending_text);
        let mut blocks = vec![];
        if !thought.trim().is_empty() {
            blocks.push(Block::Thinking { text: thought });
        }
        if !text.is_empty() {
            blocks.push(Block::Text { text });
        }
        if let Some(t) = extra_tool {
            blocks.push(t);
        }
        if !blocks.is_empty() {
            let model = self.pending_model.clone();
            self.push_item(Body::Assistant { model, blocks }, None);
        }
    }

    /// 开始 `session/load` 历史回放：清空现有条目
    pub fn begin_replay(&mut self) {
        self.items.clear();
        self.raws.clear();
        self.inputs.clear();
        self.seen_tools.clear();
        self.done_tools.clear();
        self.pending_user.clear();
        self.pending_thought.clear();
        self.pending_text.clear();
        self.seq = 0;
        self.dirty = None;
        self.seeded = false;
    }

    /// `session/load` 回放结束：落盘残余块，通知界面整页刷新
    pub fn end_replay(&mut self) {
        self.flush_user();
        self.flush_assistant(None);
        self.seeded = true;
        self.reset = true;
        self.dirty = None;
    }

    /// 用户发了一条新消息（实时对话）
    pub fn push_user(&mut self, text: &str, images: Vec<(&'static str, Vec<u8>)>, mid: bool) {
        self.flush_user();
        self.flush_assistant(None);
        let img_count = images.len() as u32;
        let (clipped, truncated) = truncate(text, TEXT_LIMIT);
        let raw = json!({ "role": "user", "text": text });
        let key = self.push_item(Body::User { text: clipped, images: img_count, truncated, mid }, Some(raw));
        if !images.is_empty() {
            self.user_images.insert(key, images);
        }
        self.seeded = true;
    }

    /// 收到一条 `session/update` 的 `update` 对象
    pub fn on_update(&mut self, u: &Value, model: Option<&str>) {
        self.stats.lines += 1;
        if let Some(m) = model {
            self.pending_model = Some(m.to_string());
        }
        match u["sessionUpdate"].as_str().unwrap_or("") {
            "user_message_chunk" => {
                self.flush_assistant(None);
                if let Some(t) = u["content"]["text"].as_str() {
                    self.pending_user.push_str(t);
                }
            }
            "agent_thought_chunk" => {
                self.flush_user();
                if let Some(t) = u["content"]["text"].as_str() {
                    self.pending_thought.push_str(t);
                }
            }
            "agent_message_chunk" => {
                self.flush_user();
                if let Some(t) = u["content"]["text"].as_str() {
                    self.pending_text.push_str(t);
                }
            }
            "tool_call" | "tool_call_update" => {
                self.flush_user();
                let call_id = u["toolCallId"].as_str().unwrap_or_default().to_string();
                if call_id.is_empty() {
                    return;
                }
                let raw_name = u["name"].as_str().unwrap_or("tool");
                let raw_in = if !u["rawInput"].is_null() { &u["rawInput"] } else { &u["args"] };
                let (tool_name, mapped_input) = map_tool(raw_name, raw_in);

                if self.seen_tools.insert(call_id.clone()) {
                    self.inputs.insert(call_id.clone(), mapped_input.clone());
                    self.flush_assistant(Some(Block::ToolUse {
                        id: call_id.clone(),
                        name: tool_name,
                        input: mapped_input,
                        truncated: false,
                    }));
                }

                let status = u["status"].as_str().unwrap_or("");
                if matches!(status, "completed" | "failed") && self.done_tools.insert(call_id.clone()) {
                    // 先把工具执行期间可能已经收到的任何思考/文字落盘（一般为空）
                    let output = extract_tool_output(u);
                    let is_error = status == "failed"
                        || u["rawOutput"]["exit_code"].as_i64().is_some_and(|c| c != 0)
                        || u["isError"].as_bool() == Some(true);
                    let (content, truncated) = truncate(&output, TEXT_LIMIT);
                    self.push_item(
                        Body::ToolResult {
                            tool_use_id: call_id,
                            is_error,
                            content,
                            images: 0,
                            truncated,
                            extra: None,
                        },
                        Some(u.clone()),
                    );
                }
            }
            _ => {}
        }
    }

    /// 一轮结束（`session/prompt` 返回）
    pub fn end_turn(&mut self, stop_reason: &str, elapsed_ms: Option<u64>, error: Option<&str>) {
        self.flush_user();
        self.flush_assistant(None);
        if stop_reason == "cancelled" {
            self.push_item(Body::Interrupt, None);
        } else if let Some(err) = error.filter(|e| !e.is_empty()) {
            self.push_item(Body::Info { text: err.chars().take(500).collect() }, None);
        }
        if let Some(ms) = elapsed_ms {
            self.push_item(Body::Turn { ms }, None);
        }
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

    pub fn last_reply(&self) -> Option<String> {
        self.items.iter().rev().find_map(|i| match &i.body {
            Body::Assistant { blocks, .. } => blocks.iter().rev().find_map(|b| if let Block::Text { text } = b { Some(text.clone()) } else { None }),
            _ => None,
        })
    }

    pub fn user_image(&self, key: &str, n: usize) -> Option<(&'static str, Vec<u8>)> {
        self.user_images.get(key)?.get(n).cloned()
    }

    fn raw(&self, key: &str) -> Option<Value> {
        self.raws.get(key).cloned()
    }
}

// ---------------- 按会话共用 ----------------

static LOGS: std::sync::LazyLock<Mutex<HashMap<String, Arc<Mutex<AgyLog>>>>> = std::sync::LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn shared(session: &str) -> Arc<Mutex<AgyLog>> {
    lock(&LOGS).entry(session.to_string()).or_default().clone()
}

pub fn drop_log(session: &str) {
    lock(&LOGS).remove(session);
}

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

    #[test]
    fn maps_tools_to_canonical_format() {
        let (n, i) = map_tool("run_command", &json!({ "CommandLine": "ls -la", "toolSummary": "List files" }));
        assert_eq!(n, "Bash");
        assert_eq!(i["command"], "ls -la");
        assert_eq!(i["description"], "List files");

        let (n, i) = map_tool("edit_file", &json!({ "Path": "/a.rs", "Target": "old", "Replacement": "new" }));
        assert_eq!(n, "Edit");
        assert_eq!(i["file_path"], "/a.rs");
        assert_eq!(i["old_string"], "old");
        assert_eq!(i["new_string"], "new");
    }

    #[test]
    fn replays_session_updates_into_items() {
        let mut log = AgyLog::default();
        log.begin_replay();
        log.on_update(&json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "hello" } }), None);
        log.on_update(&json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "thinking..." } }), Some("gemini-4-neon"));
        log.on_update(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "name": "run_command",
                "status": "completed",
                "rawInput": { "CommandLine": "echo hi" },
                "rawOutput": { "output": "hi\n", "exit_code": 0 }
            }),
            Some("gemini-4-neon"),
        );
        log.on_update(&json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Done!" } }), Some("gemini-4-neon"));
        log.end_replay();

        assert!(matches!(log.take(), Change::Reset));
        assert_eq!(log.items.len(), 4);
        assert!(matches!(&log.items[0].body, Body::User { text, .. } if text == "hello"));
        assert!(matches!(&log.items[1].body, Body::Assistant { blocks, .. } if blocks.len() == 2));
        assert!(matches!(&log.items[2].body, Body::ToolResult { tool_use_id, is_error: false, content, .. } if tool_use_id == "call_1" && content == "hi\n"));
        assert!(matches!(&log.items[3].body, Body::Assistant { blocks, .. } if matches!(&blocks[0], Block::Text { text } if text == "Done!")));
        assert_eq!(log.last_reply().as_deref(), Some("Done!"));
    }
}
