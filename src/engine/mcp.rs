//! MCP (Model Context Protocol) stdio client for the engine: connects the
//! servers configured at `~/.blackagent/cli/config.json → mcp.servers`,
//! lists their tools and calls them; tools surface to the model under the
//! `mcp__<server>__<tool>` naming.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

/// One configured MCP server (stdio transport).
#[derive(Clone, Debug)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_ms: u64,
}

impl McpServerConfig {
    /// Reads the mcp.servers section of the engine config file (strict
    /// schema: unknown keys on a server entry drop that server — ZCode
    /// semantics).
    pub fn load_all() -> Vec<Self> {
        let path = super::memory::engine_config_path();
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        let Some(servers) = json.pointer("/mcp/servers").and_then(Value::as_object) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (name, entry) in servers {
            let (Some(command), Some(args)) = (
                entry.get("command").and_then(Value::as_str),
                entry.get("args").and_then(Value::as_array),
            ) else {
                continue; // stdio requires command+args
            };
            let Some(args) = args
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
                .map(|items| items.into_iter().map(str::to_owned).collect::<Vec<_>>())
            else {
                continue;
            };
            // Strict schema: only the known keys may be present.
            let known = ["command", "args", "cwd", "env", "enabled", "timeoutMs", "type"];
            let has_unknown = entry
                .as_object()
                .is_some_and(|map| map.keys().any(|key| !known.contains(&key.as_str())));
            if has_unknown {
                continue;
            }
            let env = entry
                .get("env")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(Self {
                name: name.clone(),
                command: command.to_owned(),
                args,
                cwd: entry
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                env,
                enabled: entry.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                timeout_ms: entry
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000),
            });
        }
        out
    }
}

/// A live connection to one stdio MCP server (child process + JSON-RPC).
pub struct McpConnection {
    config: McpServerConfig,
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    next_id: u64,
}

impl McpConnection {
    /// Spawns the server process and performs the initialize handshake.
    pub async fn connect(config: McpServerConfig) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        super::tools::hide_console(&mut cmd);
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|err| format!("MCP 服务器 {} 启动失败: {err}", config.name))?;
        let stdin = child.stdin.take().ok_or("MCP stdin 不可用")?;
        let stdout = child.stdout.take().ok_or("MCP stdout 不可用")?;
        let mut connection = Self {
            config,
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        // initialize handshake + initialized notification.
        let _ = connection
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "blackagent-engine", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let _ = connection.notify("notifications/initialized", serde_json::json!({})).await;
        Ok(connection)
    }

    /// One JSON-RPC request/response round trip (NDJSON framing).
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = &mut self.stdin;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|err| format!("MCP 写入失败: {err}"))?;
        stdin.flush().await.map_err(|err| format!("MCP 刷新失败: {err}"))?;

        let mut reader = tokio::io::BufReader::new(&mut self.stdout);
        let deadline = Duration::from_millis(self.config.timeout_ms);
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(deadline, reader.read_line(&mut line)).await;
            match read {
                Ok(Ok(0)) => return Err("MCP 服务器已退出".to_owned()),
                Ok(Ok(_)) => {
                    let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if message.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = message.get("error") {
                            return Err(format!(
                                "MCP 错误: {}",
                                error.get("message").and_then(Value::as_str).unwrap_or("?")
                            ));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Notification or unrelated response: keep reading.
                }
                Ok(Err(err)) => return Err(format!("MCP 读取失败: {err}")),
                Err(_) => return Err("MCP 请求超时".to_owned()),
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|err| format!("MCP 写入失败: {err}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| format!("MCP 刷新失败: {err}"))
    }

    /// tools/list → the tool specs in the engine registry shape
    /// (`mcp__<server>__<tool>` naming).
    pub async fn list_tools(&mut self) -> Result<Vec<crate::llm::ToolSpec>, String> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        let mut specs = Vec::new();
        if let Some(tools) = result.get("tools").and_then(Value::as_array) {
            for tool in tools {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                specs.push(crate::llm::ToolSpec {
                    name: format!("mcp__{}__{}", self.config.name, name),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    parameters: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                });
            }
        }
        Ok(specs)
    }

    /// tools/call with the inner (unprefixed) tool name.
    pub async fn call_tool(&mut self, tool: &str, arguments: Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({"name": tool, "arguments": arguments}),
            )
            .await?;
        // Result shape: {content: [{type: "text", text: ...}], isError?}.
        let mut text = String::new();
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            for item in content {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(item.get("text").and_then(Value::as_str).unwrap_or_default());
                }
            }
        }
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error {
            Err(if text.is_empty() {
                "MCP 工具返回错误".to_owned()
            } else {
                text
            })
        } else {
            Ok(if text.is_empty() {
                "（无输出）".to_owned()
            } else {
                text
            })
        }
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Connects every enabled server and returns (connections, tool specs).
/// A failing server is skipped (logged by the caller through the error
/// list) so one broken server doesn't take the session down.
pub async fn connect_all() -> (Vec<Arc<tokio::sync::Mutex<McpConnection>>>, Vec<crate::llm::ToolSpec>, Vec<String>) {
    let mut connections = Vec::new();
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    for config in McpServerConfig::load_all() {
        if !config.enabled {
            continue;
        }
        match McpConnection::connect(config).await {
            Ok(mut connection) => match connection.list_tools().await {
                Ok(tools) => {
                    specs.extend(tools);
                    connections.push(Arc::new(tokio::sync::Mutex::new(connection)));
                }
                Err(err) => errors.push(err),
            },
            Err(err) => errors.push(err),
        }
    }
    (connections, specs, errors)
}

/// Routes an `mcp__<server>__<tool>` call to the right connection.
pub async fn dispatch_mcp_call(
    connections: &[Arc<tokio::sync::Mutex<McpConnection>>],
    tool_name: &str,
    arguments: Value,
) -> Result<String, String> {
    let inner = tool_name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .ok_or_else(|| format!("MCP 工具名无效: {tool_name}"))?;
    let (server, tool) = inner;
    for connection in connections {
        let mut guard = connection.lock().await;
        if guard.config.name == server {
            return guard.call_tool(tool, arguments).await;
        }
    }
    Err(format!("未连接的 MCP 服务器: {server}"))
}
