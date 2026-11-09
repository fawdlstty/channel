//! Hooks runner for the engine (ZCode seven-event semantics):
//! `SessionStart / UserPromptSubmit / PreToolUse / PermissionRequest /
//! PostToolUse / PostToolUseFailure / Stop`, configured at
//! `~/.blackagent/cli/config.json → hooks`.
//!
//! Matcher rules, exit-code semantics (`0` = success/JSON, `2` = block,
//! other = recoverable failure), the stdin payload shape and the
//! `permissionDecision(allow|ask|deny)` / `updatedInput` outputs all follow
//! the ZCode hooks contract.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use serde_json::Value;

use super::tools::ToolContext;
use crate::utils::text::decode_child_output;

/// The seven hook events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
        }
    }
}

/// One hook execution verdict for tool-affecting events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookVerdict {
    /// Continue (optionally with extra context to inject).
    Continue { additional_context: Option<String> },
    /// Block the action (exit 2 or explicit deny) with the reason text.
    Block { reason: String },
    /// Replace the tool input wholesale (PreToolUse `updatedInput`) and
    /// continue.
    ModifyInput { updated_input: Value },
}

/// Hooks configuration snapshot (session-start semantics: the config is
/// read once and kept).
#[derive(Clone, Debug, Default)]
pub struct HooksConfig {
    pub enabled: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    /// event → list of (matcher, command lines).
    pub events: BTreeMap<String, Vec<HookEntry>>,
}

#[derive(Clone, Debug)]
pub struct HookEntry {
    pub matcher: String,
    pub command: String,
    /// `type` field: "command" (shell) | "process" (no shell).
    pub kind: String,
}

impl HooksConfig {
    /// Reads the hooks section from the engine config file. Missing file /
    /// section → disabled.
    pub fn load() -> Self {
        let path = super::memory::engine_config_path();
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        Self::from_json(&json)
    }

    fn from_json(json: &Value) -> Self {
        let Some(hooks) = json.get("hooks") else {
            return Self::default();
        };
        let enabled = hooks.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let timeout_ms = hooks.get("timeoutMs").and_then(Value::as_u64).unwrap_or(60_000);
        let max_output_bytes = hooks
            .get("maxOutputBytes")
            .and_then(Value::as_u64)
            .unwrap_or(32_768) as usize;
        let mut events = BTreeMap::new();
        if let Some(map) = hooks.get("events").and_then(Value::as_object) {
            for (event, entries) in map {
                let mut list = Vec::new();
                if let Some(items) = entries.as_array() {
                    for item in items {
                        // Shape: {matcher?, hooks: [{type, command, ...}]}.
                        let matcher = item
                            .get("matcher")
                            .and_then(Value::as_str)
                            .unwrap_or("*")
                            .to_owned();
                        if let Some(hook_list) = item.get("hooks").and_then(Value::as_array) {
                            for hook in hook_list {
                                let Some(command) = hook.get("command").and_then(Value::as_str)
                                else {
                                    continue;
                                };
                                list.push(HookEntry {
                                    matcher: matcher.clone(),
                                    command: command.to_owned(),
                                    kind: hook
                                        .get("type")
                                        .and_then(Value::as_str)
                                        .unwrap_or("command")
                                        .to_owned(),
                                });
                            }
                        }
                    }
                }
                events.insert(event.clone(), list);
            }
        }
        Self {
            enabled,
            timeout_ms,
            max_output_bytes,
            events,
        }
    }
}

/// Matcher semantics: empty/`*` = match all; `[A-Za-z0-9_|]` = exact-name
/// alternation; anything else = regex (invalid regex silently matches
/// nothing — ZCode skips it).
pub fn matcher_matches(matcher: &str, tool_name: &str) -> bool {
    let matcher = matcher.trim();
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let is_plain = matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');
    if is_plain {
        matcher
            .split('|')
            .any(|name| name == tool_name || alias_equivalent(name, tool_name))
    } else if let Ok(regex) = regex::Regex::new(matcher) {
        regex.is_match(tool_name)
    } else {
        false
    }
}

/// Hook-name aliases: `Task` matches `Agent` and vice versa.
fn alias_equivalent(a: &str, b: &str) -> bool {
    (a == "Task" && b == "Agent") || (a == "Agent" && b == "Task")
}

/// Runs the hooks for one event. `tool_name`/`tool_input` apply to the
/// tool events; the stdin payload is assembled per the ZCode contract.
pub async fn run_hooks(
    config: &HooksConfig,
    event: HookEvent,
    ctx: &Arc<ToolContext>,
    tool_name: Option<&str>,
    tool_input: Option<&Value>,
) -> HookVerdict {
    if !config.enabled {
        return HookVerdict::Continue {
            additional_context: None,
        };
    }
    let Some(entries) = config.events.get(event.as_str()) else {
        return HookVerdict::Continue {
            additional_context: None,
        };
    };
    let mut additional_context: Option<String> = None;
    for entry in entries {
        if let Some(name) = tool_name {
            if !matcher_matches(&entry.matcher, name) {
                continue;
            }
        }
        let mut payload = serde_json::json!({
            "session_id": ctx.session_id,
            "cwd": ctx.cwd.lock().unwrap().display().to_string(),
            "hook_event_name": event.as_str(),
        });
        if let Some(name) = tool_name {
            payload["tool_name"] = Value::String(name.to_owned());
        }
        if let Some(input) = tool_input {
            payload["tool_input"] = input.clone();
        }
        let outcome = run_one(entry, &payload, config.timeout_ms, config.max_output_bytes).await;
        match outcome {
            HookOutcome::Success(output) => {
                if let Some(context) = output.get("additionalContext").and_then(Value::as_str) {
                    additional_context = Some(
                        additional_context
                            .map(|existing| format!("{existing}\n{context}"))
                            .unwrap_or_else(|| context.to_owned()),
                    );
                }
                // PreToolUse permissionDecision allow|deny + updatedInput.
                let decision = output
                    .pointer("/hookSpecificOutput/permissionDecision")
                    .or_else(|| output.get("permissionDecision"))
                    .and_then(Value::as_str);
                match decision {
                    Some("deny") => {
                        let reason = output
                            .pointer("/hookSpecificOutput/permissionDecisionReason")
                            .and_then(Value::as_str)
                            .unwrap_or("hook denied the action")
                            .to_owned();
                        return HookVerdict::Block { reason };
                    }
                    Some("allow") | Some("ask") | None => {
                        if let Some(updated) =
                            output.pointer("/hookSpecificOutput/updatedInput").cloned()
                        {
                            if updated.is_object() {
                                return HookVerdict::ModifyInput { updated_input: updated };
                            }
                        }
                    }
                    _ => {}
                }
            }
            HookOutcome::Blocked(reason) => {
                return HookVerdict::Block { reason };
            }
            HookOutcome::Failed(reason) => {
                // Recoverable failure: log and continue with the next hook.
                eprintln!("[blackagent-engine] hook 执行失败（继续）: {reason}");
            }
        }
    }
    HookVerdict::Continue {
        additional_context,
    }
}

enum HookOutcome {
    /// Exit 0 with (parsed-JSON?) output.
    Success(Value),
    /// Exit 2 (or explicit deny): blocking.
    Blocked(String),
    /// Any other failure.
    Failed(String),
}

/// Byte-count cap that never splits a character: backs off to the previous
/// char boundary (mirrors the Bash tool's output truncation).
fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

async fn run_one(
    entry: &HookEntry,
    payload: &Value,
    timeout_ms: u64,
    max_output_bytes: usize,
) -> HookOutcome {
    let stdin_text = payload.to_string();
    let (program, argv) = if entry.kind == "process" {
        // No shell: split on whitespace (simple contract; hooks needing
        // shell semantics use type "command").
        let mut parts = entry.command.split_whitespace().map(str::to_owned);
        let Some(program) = parts.next() else {
            return HookOutcome::Failed("hook command 为空".to_owned());
        };
        (program, parts.collect::<Vec<_>>())
    } else if cfg!(windows) {
        ("cmd".to_owned(), vec!["/c".to_owned(), entry.command.clone()])
    } else {
        ("sh".to_owned(), vec!["-c".to_owned(), entry.command.clone()])
    };
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    super::tools::hide_console(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return HookOutcome::Failed(format!("hook 启动失败: {err}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(stdin_text.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return HookOutcome::Failed("hook 超时".to_owned());
    };
    // 解码后再按字节在字符边界截断：先截字节会把多字节字符（或 GBK 码对）
    // 切成替换符。
    let decoded = decode_child_output(&output.stdout);
    let stdout = truncate_on_char_boundary(&decoded, max_output_bytes).to_owned();
    if output.status.code() == Some(2) {
        let reason = if stdout.trim().is_empty() {
            "hook 以退出码 2 阻断"
        } else {
            stdout.trim()
        };
        return HookOutcome::Blocked(reason.to_owned());
    }
    if !output.status.success() {
        return HookOutcome::Failed(format!("hook 退出码 {:?}", output.status.code()));
    }
    let parsed = if stdout.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&stdout).unwrap_or(Value::Null)
    };
    HookOutcome::Success(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_rules_follow_the_contract() {
        assert!(matcher_matches("", "Bash"));
        assert!(matcher_matches("*", "Bash"));
        assert!(matcher_matches("Bash", "Bash"));
        assert!(!matcher_matches("Bash|Edit", "Write"));
        assert!(matcher_matches("Write|Edit", "Edit"));
        // Alias: Task ↔ Agent.
        assert!(matcher_matches("Agent", "Task"));
        assert!(matcher_matches("Task", "Agent"));
        // Regex form.
        assert!(matcher_matches("^mcp__.*__js$", "mcp__node_repl__js"));
        // Invalid regex matches nothing.
        assert!(!matcher_matches("[invalid", "Bash"));
    }

    #[test]
    fn config_loads_from_json() {
        let json = serde_json::json!({
            "hooks": {
                "enabled": true,
                "timeoutMs": 5000,
                "events": {
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo hi"}]}
                    ]
                }
            }
        });
        let config = HooksConfig::from_json(&json);
        assert!(config.enabled);
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.events["PreToolUse"].len(), 1);
        assert_eq!(config.events["PreToolUse"][0].matcher, "Bash");

        let disabled = HooksConfig::from_json(&serde_json::json!({"hooks": {"enabled": false}}));
        assert!(!disabled.enabled);
    }
}
