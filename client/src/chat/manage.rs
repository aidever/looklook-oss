//! 对话页里管终端 TUI 才能管的东西（docs/CHAT_TUI_PARITY.md）：插件和插件市场、子代理、hook、沙箱、Codex 的实验功能。
//!
//! Claude Code：插件走它自己的命令行（`claude plugin … --json`，用这个会话的账号目录），装完让在跑的进程 `reload_plugins`；
//! 子代理、hook、沙箱是 settings / `.claude/agents` 里的文件，Claude Code 自己盯着，改了就生效。
//! Codex：app-server 的 `plugin/*`、`marketplace/*`、`experimentalFeature/*`、`config/value/write`。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::{lock, models, Chat};
use crate::error::LocalError;

/// 插件命令最多等多久（装插件要 git clone）
const PLUGIN_WAIT: Duration = Duration::from_secs(180);

impl Chat {
    /// 这个会话的 Claude Code 命令行：账号目录不是默认的就带上 `CLAUDE_CONFIG_DIR`，在工作目录里跑（项目范围的插件）
    async fn claude_cli(&self, id: &str, args: &[&str]) -> Result<(bool, String, String), LocalError> {
        let shell = self.instances.shell().to_string();
        let cli = crate::agents::claude_cli(&shell).await.ok_or_else(|| LocalError::new("CLAUDE_NOT_FOUND"))?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let (dir, _) = self.claude_paths(id);
        let custom = dir != models::config_dir();
        let env: Vec<(&str, Option<&std::ffi::OsStr>)> = if custom { vec![("CLAUDE_CONFIG_DIR", Some(dir.as_os_str()))] } else { vec![] };
        let mut cmd = crate::agents::cli_command(&shell, &cli, args, &env);
        cmd.env_remove("CLAUDECODE").current_dir(&row.workdir).stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(PLUGIN_WAIT, cmd.output()).await.map_err(|_| LocalError::new("CLAUDE_CLI_TIMEOUT"))??;
        Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
    }

    /// 插件：装了的、市场里能装的、市场
    pub async fn plugins(self: &Arc<Self>, id: &str) -> Result<Value, LocalError> {
        self.require(id)?;
        if self.is_codex(id) {
            return self.codex_plugins(id).await;
        }
        if !self.is_sdk(id) {
            return Err(LocalError::new("CHAT_UNAVAILABLE"));
        }
        let (list, markets) = tokio::join!(self.claude_cli(id, &["plugin", "list", "--json", "--available"]), self.claude_cli(id, &["plugin", "marketplace", "list", "--json"]));
        let (ok, out, err) = list?;
        if !ok {
            return Err(LocalError::new("PLUGIN_FAILED").with("detail", tail(&err, &out)));
        }
        let v: Value = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
        let installed: Vec<Value> = v["installed"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| json!({ "id": p["id"], "version": p["version"], "scope": p["scope"], "enabled": p["enabled"], "project": p["projectPath"] }))
            .collect();
        let available: Vec<Value> = v["available"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| json!({ "id": p["pluginId"], "name": p["name"], "description": p["description"], "market": p["marketplaceName"], "installs": p["installCount"] }))
            .collect();
        let markets: Vec<Value> = markets
            .ok()
            .filter(|(ok, ..)| *ok)
            .and_then(|(_, out, _)| serde_json::from_str::<Value>(out.trim()).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|m| json!({ "name": m["name"], "source": m["repo"].as_str().or(m["url"].as_str()).or(m["path"].as_str()).or(m["source"].as_str()) }))
            .collect();
        Ok(json!({ "agent": "claude", "installed": installed, "available": available, "markets": markets }))
    }

    /// 插件操作。`op`：install / uninstall / enable / disable / update / market_add / market_remove / market_update。
    /// Claude 装需要运行市场声明的命令时第一次不装、返回要运行的命令（`confirm`），用户看过点确认再带 `accept` 装
    pub async fn plugin_op(self: &Arc<Self>, id: &str, op: &str, name: &str, scope: Option<&str>, accept: Option<&str>) -> Result<Value, LocalError> {
        self.require(id)?;
        let name = name.trim();
        if name.is_empty() || name.len() > 500 || name.starts_with('-') {
            return Err(LocalError::invalid("name", "invalid"));
        }
        if self.is_codex(id) {
            return self.codex_plugin_op(id, op, name).await;
        }
        if !self.is_sdk(id) {
            return Err(LocalError::new("CHAT_UNAVAILABLE"));
        }
        let scope = scope.filter(|s| matches!(*s, "user" | "project" | "local"));
        let mut args: Vec<&str> = match op {
            "install" => vec!["plugin", "install", name, "--json"],
            "uninstall" => vec!["plugin", "uninstall", name, "--json", "-y"],
            "enable" => vec!["plugin", "enable", name, "--json"],
            "disable" => vec!["plugin", "disable", name, "--json"],
            "update" => vec!["plugin", "update", name, "--json"],
            "market_add" => vec!["plugin", "marketplace", "add", name, "--json"],
            "market_remove" => vec!["plugin", "marketplace", "remove", name],
            "market_update" => vec!["plugin", "marketplace", "update", name],
            _ => return Err(LocalError::invalid("op", "invalid")),
        };
        if let Some(s) = scope.filter(|_| op != "market_remove" && op != "market_update") {
            args.extend(["--scope", s]);
        }
        let accept = accept.filter(|a| a.len() == 64 && a.chars().all(|c| c.is_ascii_hexdigit()));
        if let Some(a) = accept.filter(|_| matches!(op, "install" | "update")) {
            args.extend(["--accept-command", a]);
        }
        let (ok, out, err) = self.claude_cli(id, &args).await?;
        // --json：最后一行是结果
        let result = out.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).unwrap_or(Value::Null);
        if let Some(shown) = result.get("shownCommand").filter(|s| s.is_object()) {
            if result["installed"] != true && result["ok"] != true {
                return Ok(json!({ "confirm": { "command": shown["command"].as_str().or(shown["display"].as_str()), "sha256": shown["sha256"] } }));
            }
        }
        if !ok {
            return Err(LocalError::new("PLUGIN_FAILED").with("detail", tail(&err, &out)));
        }
        // 在跑的进程马上用上（不在跑的下次起时自然读到）
        let proc = lock(&self.inst(id).st).sdk.proc.clone().filter(|p| p.alive());
        if let Some(p) = proc {
            if let Err(e) = p.request_within("reload_plugins", json!({}), Duration::from_secs(60)).await {
                tracing::debug!(id, error = ?e, "装完插件 reload_plugins 失败");
            }
        }
        Ok(json!({ "ok": true, "result": result }))
    }

    async fn codex_plugins(&self, id: &str) -> Result<Value, LocalError> {
        let server = self.codex_ready(id).await?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let r = server.call("plugin/list", json!({ "cwds": [row.workdir] })).await?;
        let mut installed = vec![];
        let mut available = vec![];
        let mut markets = vec![];
        for m in r["marketplaces"].as_array().into_iter().flatten() {
            let market = m["name"].as_str().unwrap_or("");
            markets.push(json!({ "name": market, "source": m["interface"]["displayName"].as_str().unwrap_or(market), "path": m["path"] }));
            for p in m["plugins"].as_array().into_iter().flatten() {
                let i = &p["interface"];
                let desc = i["shortDescription"].as_str().or(i["longDescription"].as_str()).or(i["description"].as_str());
                let view = json!({
                    "id": p["id"], "name": i["displayName"].as_str().or(p["name"].as_str()), "description": desc, "market": market,
                    "version": p["localVersion"].as_str().or(p["version"].as_str()), "enabled": p["enabled"], "path": m["path"],
                    "available": p["availability"] != "UNAVAILABLE" && p["installPolicy"] != "NOT_AVAILABLE",
                });
                if p["installed"] == true {
                    installed.push(view);
                } else {
                    available.push(view);
                }
            }
        }
        Ok(json!({ "agent": "codex", "installed": installed, "available": available, "markets": markets }))
    }

    async fn codex_plugin_op(&self, id: &str, op: &str, name: &str) -> Result<Value, LocalError> {
        let server = self.codex_ready(id).await?;
        let r = match op {
            // name = 插件 id（名字@市场）：按列表找到市场的文件
            "install" => {
                let list = self.codex_plugins(id).await?;
                let p = list["available"].as_array().into_iter().flatten().find(|p| p["id"] == name).cloned().ok_or_else(LocalError::not_found)?;
                let plugin = name.split('@').next().unwrap_or(name);
                let mut params = json!({ "pluginName": plugin });
                if p["path"].is_string() {
                    params["marketplacePath"] = p["path"].clone();
                } else {
                    params["remoteMarketplaceName"] = p["market"].clone();
                }
                server.call("plugin/install", params).await?
            }
            "uninstall" => server.call("plugin/uninstall", json!({ "pluginId": name })).await?,
            "enable" | "disable" => {
                server.call("config/value/write", json!({ "keyPath": format!("plugins.\"{name}\".enabled"), "value": op == "enable", "mergeStrategy": "upsert" })).await?
            }
            "market_add" => server.call("marketplace/add", json!({ "source": name })).await?,
            "market_remove" => server.call("marketplace/remove", json!({ "marketplaceName": name })).await?,
            "market_update" => server.call("marketplace/upgrade", json!({ "marketplaceName": name })).await?,
            _ => return Err(LocalError::invalid("op", "invalid")),
        };
        Ok(json!({ "ok": true, "result": r }))
    }

    // ---------------- Claude：子代理 ----------------

    /// 子代理：项目的 `.claude/agents/*.md`、用户的 `<配置目录>/agents/*.md`（frontmatter 里的 name、description、model）
    pub fn agents(&self, id: &str) -> Result<Value, LocalError> {
        self.require(id)?;
        let (project, user) = self.agent_dirs(id)?;
        let mut items = vec![];
        for (src, dir) in [("project", &project), ("user", &user)] {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
            files.sort();
            for p in files {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                let fm = frontmatter(&text);
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                items.push(json!({
                    "name": fm.get("name").cloned().unwrap_or(stem),
                    "description": fm.get("description"),
                    "model": fm.get("model"),
                    "tools": fm.get("tools"),
                    "source": src,
                    "path": p.display().to_string(),
                }));
            }
        }
        Ok(json!({ "items": items, "dirs": { "project": project.display().to_string(), "user": user.display().to_string() } }))
    }

    /// 新建子代理：写一个模板文件，返回路径（界面在文件查看器里打开接着改）
    pub fn agent_create(&self, id: &str, source: &str, name: &str, description: &str, prompt: &str) -> Result<Value, LocalError> {
        self.require(id)?;
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(LocalError::invalid("name", "invalid"));
        }
        let (project, user) = self.agent_dirs(id)?;
        let dir = match source {
            "project" => project,
            "user" => user,
            _ => return Err(LocalError::invalid("source", "invalid")),
        };
        let path = dir.join(format!("{name}.md"));
        if path.exists() {
            return Err(LocalError::new("AGENT_EXISTS"));
        }
        std::fs::create_dir_all(&dir)?;
        let desc = description.trim().replace('\n', " ");
        let desc = if desc.is_empty() { format!("Use this agent for {name} tasks.") } else { desc };
        let body = prompt.trim();
        let body = if body.is_empty() { "You are a focused assistant. Describe here what this agent does, how it works, and what it returns." } else { body };
        std::fs::write(&path, format!("---\nname: {name}\ndescription: {}\n---\n\n{body}\n", yaml_str(&desc)))?;
        Ok(json!({ "path": path.display().to_string() }))
    }

    /// 删子代理：只认两个子代理目录里的 .md
    pub fn agent_delete(&self, id: &str, path: &str) -> Result<(), LocalError> {
        self.require(id)?;
        let (project, user) = self.agent_dirs(id)?;
        let p = Path::new(path);
        let inside = p.extension().is_some_and(|x| x == "md") && p.parent().is_some_and(|d| d == project || d == user);
        if !inside || !p.is_file() {
            return Err(LocalError::not_found());
        }
        std::fs::remove_file(p)?;
        Ok(())
    }

    fn agent_dirs(&self, id: &str) -> Result<(PathBuf, PathBuf), LocalError> {
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let (config_dir, _) = self.claude_paths(id);
        Ok((Path::new(&row.workdir).join(".claude").join("agents"), config_dir.join("agents")))
    }

    // ---------------- Claude：hook、沙箱 ----------------

    /// 加 / 删一个命令 hook（写 settings 文件的 `hooks`）
    #[allow(clippy::too_many_arguments)]
    pub fn hook_edit(&self, id: &str, source: &str, event: &str, matcher: &str, command: &str, add: bool) -> Result<(), LocalError> {
        self.require(id)?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let workdir = PathBuf::from(&row.workdir);
        let (config_dir, state_file) = self.claude_paths(id);
        let ctx = models::Ctx { config_dir, state_file, workdir: &workdir, env: Default::default() };
        models::edit_hook(&ctx, source, event, matcher, command, add)
    }

    /// 新建记忆文件（`user` / `project` 的 CLAUDE.md），返回路径
    pub fn memory_create(&self, id: &str, source: &str) -> Result<String, LocalError> {
        self.require(id)?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let workdir = PathBuf::from(&row.workdir);
        let (config_dir, state_file) = self.claude_paths(id);
        let ctx = models::Ctx { config_dir, state_file, workdir: &workdir, env: Default::default() };
        Ok(models::create_memory(&ctx, source)?.to_string_lossy().into_owned())
    }

    /// 沙箱开关（`sandbox.enabled`，写本机 settings）
    pub fn sandbox_set(&self, id: &str, on: bool) -> Result<(), LocalError> {
        self.require(id)?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let workdir = PathBuf::from(&row.workdir);
        let (config_dir, state_file) = self.claude_paths(id);
        let ctx = models::Ctx { config_dir, state_file, workdir: &workdir, env: Default::default() };
        models::edit_settings(&ctx, "local", |s| {
            if !s["sandbox"].is_object() {
                s["sandbox"] = json!({});
            }
            s["sandbox"]["enabled"] = json!(on);
        })
    }

    // ---------------- Codex：实验功能、hook ----------------

    /// Codex 的设置栏：MCP、hook（全部，含已信任的）、实验功能
    pub async fn codex_extras(&self, id: &str) -> Result<Value, LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        let server = self.codex_ready(id).await?;
        let row = self.store.instance(id)?.ok_or_else(LocalError::not_found)?;
        let hooks = server.call("hooks/list", json!({ "cwds": [row.workdir] })).await.ok();
        let hooks: Vec<Value> = hooks
            .as_ref()
            .and_then(|r| r["data"].as_array()?.first()?["hooks"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|h| json!({ "event": h["eventName"], "matcher": h["matcher"], "command": h["command"], "source": h["source"], "trust": h["trustStatus"], "enabled": h["enabled"] }))
            .collect();
        let features = server.call("experimentalFeature/list", json!({ "limit": 500 })).await.ok();
        let features: Vec<Value> = features
            .as_ref()
            .and_then(|r| r["data"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            // TUI 的 /experimental 只列 beta（有名字、说明的），开发中的不给用户开
            .filter(|f| f["stage"] == "beta" && f["displayName"].is_string())
            .map(|f| json!({ "name": f["name"], "title": f["displayName"], "description": f["description"], "enabled": f["enabled"] }))
            .collect();
        Ok(json!({ "hooks": hooks, "features": features }))
    }

    /// 开关一个实验功能：写进 config.toml（`[features]`，和 TUI 一样记住），也马上对这个 app-server 生效
    pub async fn codex_feature(&self, id: &str, name: &str, on: bool) -> Result<(), LocalError> {
        if !self.is_codex(id) {
            return Err(LocalError::not_found());
        }
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(LocalError::invalid("name", "invalid"));
        }
        let server = self.codex_ready(id).await?;
        server.call("config/value/write", json!({ "keyPath": format!("features.{name}"), "value": on, "mergeStrategy": "upsert" })).await?;
        let mut m = serde_json::Map::new();
        m.insert(name.to_string(), json!(on));
        server.call("experimentalFeature/enablement/set", json!({ "enablement": m })).await?;
        Ok(())
    }
}

/// 出错时给界面看的几行（stderr 优先）
fn tail(err: &str, out: &str) -> String {
    let s = if err.trim().is_empty() { out } else { err };
    let lines: Vec<&str> = s.trim().lines().collect();
    lines[lines.len().saturating_sub(8)..].join("\n").chars().take(1500).collect()
}

/// Markdown 开头 `---` 之间的 `键: 值`（只认一层）
fn frontmatter(text: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    let Some(rest) = text.strip_prefix("---") else { return m };
    let Some(end) = rest.find("\n---") else { return m };
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if !k.is_empty() && !k.starts_with(' ') && !line.starts_with(' ') {
                m.insert(k.to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    m
}

/// YAML 里的一个字符串值：有冒号、引号、# 之类的就加引号
fn yaml_str(s: &str) -> String {
    if s.chars().any(|c| matches!(c, ':' | '#' | '"' | '\'' | '{' | '}' | '[' | ']' | ',' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`')) {
        serde_json::to_string(s).unwrap_or_default()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_top_level() {
        let m = frontmatter("---\nname: reviewer\ndescription: \"Reviews: code\"\ntools: Read, Grep\n  nested: x\n---\nbody");
        assert_eq!(m["name"], "reviewer");
        assert_eq!(m["description"], "Reviews: code");
        assert_eq!(m["tools"], "Read, Grep");
        assert!(!m.contains_key("nested"));
        assert!(frontmatter("no front").is_empty());
    }

    #[test]
    fn yaml_quotes_when_needed() {
        assert_eq!(yaml_str("plain words"), "plain words");
        assert_eq!(yaml_str("a: b"), "\"a: b\"");
    }
}
