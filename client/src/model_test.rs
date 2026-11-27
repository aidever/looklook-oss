//! 测试第三方接口：每个档位的模型发一句话，看能不能正常回。
//!
//! - **Claude Code**：真的跑 `claude -p`（和会话里一样的请求：同样的 beta 头、思考、强度参数），每个档位一个进程并行跑。
//!   服务商回的错误 Claude Code 只说一句“模型有问题”，原话在 `--debug-file` 里（`[ERROR] API error …: 404 {…}`），从那里取。
//!   用 `--setting-sources ""`：不读用户的 settings（CC Switch 写的服务商、hooks 都不掺进来），只测这个配置。
//! - **Codex**：直接请求服务商的 Responses 接口（`POST {base}/responses`，流式，和 Codex 发的一样的形状），
//!   每个要测的模型一个请求。

use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde_json::{json, Value};

/// 一个档位最多等多久
const WAIT: Duration = Duration::from_secs(90);
const PROMPT: &str = "Reply with exactly: OK";

/// 一个档位的结果：`slot`、请求的模型（没指定 = null，由 Claude Code / 接口自己对应）、`ok`、毫秒、出错时的原话
fn row(slot: &str, model: Option<&str>, ok: bool, ms: u128, actual: Option<String>, error: Option<String>) -> Value {
    json!({ "slot": slot, "model": model, "ok": ok, "ms": ms as u64, "actual": actual, "error": error })
}

/// 错误原话太长时截短（服务商有时回一整页 HTML）
fn short(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(400).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

/// `--debug-file` 里最后一条接口错误：`[ERROR] API error (attempt 1/11): 404 404 {"type":"error","error":{"message":"…"}}`
fn api_error(log: &str) -> Option<String> {
    let line = log.lines().rev().find(|l| l.contains("API error"))?;
    let rest = line.split_once("): ").map(|(_, r)| r).unwrap_or(line);
    let status = rest.split_whitespace().next().filter(|s| s.chars().all(|c| c.is_ascii_digit()));
    let msg = rest.find('{').and_then(|i| serde_json::from_str::<Value>(&rest[i..]).ok()).and_then(|v| {
        v["error"]["message"].as_str().or_else(|| v["message"].as_str()).or_else(|| v["error"].as_str()).map(String::from)
    });
    Some(short(&match (status, msg) {
        (Some(s), Some(m)) => format!("{s} {m}"),
        (_, Some(m)) => m,
        _ => rest.to_string(),
    }))
}

/// Claude Code：每个档位跑一次 `claude -p`。`env`：这个配置的启动环境（见 `claude_profiles::test_env`）
pub async fn claude(shell: &str, home: &Path, env: &crate::model::LaunchEnv, slots: &[(&'static str, Option<&str>)]) -> Value {
    let Some(program) = crate::chat::claude_sdk::program(shell).await else {
        return json!({ "error": "claude_missing", "results": [] });
    };
    let path = crate::chat::codex_app::launch_path(shell, &program).await;
    let dir = home.join("run").join("model-test");
    let cwd = home.join("run").join("no-project");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::create_dir_all(&cwd);
    let runs = slots.iter().map(|&(slot, model)| {
        let (program, path, dir, cwd) = (program.clone(), path.clone(), dir.clone(), cwd.clone());
        async move {
            let log = dir.join(format!("{}.log", crate::util::random_id(10)));
            let mut cmd = crate::chat::claude_sdk::command(&program);
            cmd.args(["-p", "--output-format", "json", "--no-session-persistence", "--setting-sources", "", "--strict-mcp-config", "--tools", "", "--disable-slash-commands"]);
            if slot != "main" {
                cmd.args(["--model", slot]);
            }
            cmd.arg("--debug-file").arg(&log).arg(PROMPT);
            cmd.current_dir(&cwd).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
            if let Some(p) = path {
                cmd.env("PATH", p);
            }
            cmd.env_remove(crate::chat::hooks::INSTANCE_ENV).env_remove("TMUX").env_remove("TMUX_PANE");
            // 出错马上报（429、5xx 默认要重试十次，等很久）
            cmd.env("CLAUDE_CODE_MAX_RETRIES", "0");
            env.apply(&mut cmd);
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let start = Instant::now();
            let out = match cmd.spawn() {
                Ok(child) => tokio::time::timeout(WAIT, child.wait_with_output()).await,
                Err(e) => return row(slot, model, false, 0, None, Some(e.to_string())),
            };
            let ms = start.elapsed().as_millis();
            let debug = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            let out = match out {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return row(slot, model, false, ms, None, Some(e.to_string())),
                Err(_) => return row(slot, model, false, ms, None, Some("timeout".into())),
            };
            let text = String::from_utf8_lossy(&out.stdout);
            let v = text.find('{').and_then(|i| serde_json::from_str::<Value>(&text[i..]).ok()).unwrap_or(Value::Null);
            // 实际请求的模型：modelUsage 的键（Claude Code 按请求的模型记）
            let actual = v["modelUsage"].as_object().and_then(|m| m.keys().next().cloned());
            if v["is_error"] == false && v["subtype"] == "success" {
                return row(slot, model, true, ms, actual, None);
            }
            let why = api_error(&debug)
                .or_else(|| v["result"].as_str().filter(|s| !s.is_empty()).map(short))
                .or_else(|| Some(short(&String::from_utf8_lossy(&out.stderr))).filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "failed".into());
            row(slot, model, false, ms, actual, Some(why))
        }
    });
    json!({ "results": join_all(runs).await })
}

/// Codex：`models` 是要测的（档位名 = 界面上的名字，模型 = 发给服务商的）
pub async fn codex(base_url: &str, key: &str, models: &[(String, String)]) -> Value {
    let client = match reqwest::Client::builder().timeout(WAIT).build() {
        Ok(c) => c,
        Err(e) => return json!({ "error": e.to_string(), "results": [] }),
    };
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let runs = models.iter().map(|(slot, model)| {
        let (client, url) = (client.clone(), url.clone());
        async move {
            let body = json!({
                "model": model,
                "instructions": "You are a connectivity test.",
                "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": PROMPT }] }],
                "stream": true,
                "store": false,
            });
            let start = Instant::now();
            let r = client.post(&url).bearer_auth(key).json(&body).send().await;
            let res = match r {
                Err(e) => Err(short(&e.to_string())),
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    if !status.is_success() {
                        let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
                            v["error"]["message"].as_str().or_else(|| v["message"].as_str()).or_else(|| v["detail"].as_str()).map(String::from)
                        });
                        Err(short(&format!("{} {}", status.as_u16(), msg.unwrap_or(text))))
                    } else {
                        sse_result(&text)
                    }
                }
            };
            let ms = start.elapsed().as_millis();
            match res {
                Ok(actual) => row(slot, Some(model), true, ms, actual, None),
                Err(e) => row(slot, Some(model), false, ms, None, Some(e)),
            }
        }
    });
    json!({ "results": join_all(runs).await })
}

/// Responses 的流：`response.completed` 算成功（带回实际模型），`response.failed` / `error` 取原话
fn sse_result(text: &str) -> Result<Option<String>, String> {
    let mut last_err = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { continue };
        match v["type"].as_str() {
            Some("response.completed") => return Ok(v["response"]["model"].as_str().map(String::from)),
            Some("response.failed") => last_err = v["response"]["error"]["message"].as_str().map(String::from).or(last_err),
            Some("error") => last_err = v["message"].as_str().or_else(|| v["error"]["message"].as_str()).map(String::from).or(last_err),
            _ => {}
        }
    }
    Err(short(&last_err.unwrap_or_else(|| if text.trim().is_empty() { "empty response".into() } else { text.to_string() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_debug_error() {
        let log = "x\n2026-11-26T06:52:38.630Z [ERROR] API error (attempt 1/11): 404 404 {\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"channel not found\"}}\n";
        assert_eq!(api_error(log).as_deref(), Some("404 channel not found"));
        assert_eq!(api_error("[ERROR] API error (attempt 1/1): Connection error.").as_deref(), Some("Connection error."));
        assert_eq!(api_error("nothing"), None);
    }

    #[test]
    fn parses_sse() {
        let ok = "event: response.created\ndata: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"kimi-k3\"}}\n";
        assert_eq!(sse_result(ok), Ok(Some("kimi-k3".into())));
        let bad = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"no such model\"}}}\n";
        assert_eq!(sse_result(bad), Err("no such model".into()));
    }
}
