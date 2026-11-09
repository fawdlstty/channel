//! Subagent (worker) support for the engine: the `Agent` / `SendMessage`
//! tools plus the agent-type discovery, mirroring ZCode semantics.
//!
//! - `Agent` {description, prompt, subagent_type?, run_in_background?}:
//!   spawns an isolated agent loop (depth 1 — subagents cannot spawn
//!   further subagents); the final message comes back as the tool result.
//!   `run_in_background: true` parks the task in the background table and
//!   notifies on completion.
//! - `SendMessage` {to, summary, message}: delivers text to a spawned
//!   agent; messaging a completed agent resumes it in place with its full
//!   context and reports back when done.
//! - Type discovery: `~/.blackagent/agents/*.md` with ZCode frontmatter
//!   (name/description/model/tools/disallowedTools/maxTurns...); built-ins
//!   `general-purpose` (all tools) and `Explore` (read-only set).
//! - Snapshot events: every spawn/state change re-emits the full
//!   `SubagentsSnapshot` (revision/childSessionIds/running/endedTotal),
//!   the same projection the ZCode adapters produce.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::protocol::{SubagentRunning, SubagentsSnapshot, TodoEntry};

use super::permission::PermMode;
use super::tools::ToolContext;
use crate::llm::ToolSpec;

/// Root of the user-level agent definitions (`agents/*.md`).
pub fn agents_dir() -> PathBuf {
    home_dir().join(".blackagent").join("agents")
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// One agent-type definition (built-in or user file).
#[derive(Clone, Debug)]
pub struct AgentProfile {
    pub id: String,
    pub description: String,
    pub system_prompt: String,
    /// Tool names the agent may use; `None` = all (general-purpose).
    pub tools: Option<Vec<String>>,
    pub max_turns: Option<usize>,
}

impl AgentProfile {
    /// The two built-in profiles (ZCode parity).
    pub fn builtin(id: &str) -> Option<Self> {
        match id {
            "general-purpose" => Some(Self {
                id: id.to_owned(),
                description: "General-purpose agent for researching complex questions, searching code, and executing multi-step tasks".to_owned(),
                system_prompt: "你是一个通用任务执行代理：独立完成派发给你的任务，期间可使用全部工具（文件读写、命令执行、检索）。最终回复只包含任务结果本身，不向用户直接对话。".to_owned(),
                tools: None,
                max_turns: None,
                
            }),
            "Explore" => Some(Self {
                id: id.to_owned(),
                description: "Read-only search agent for broad fan-out searches — locate code and configurations, return conclusions".to_owned(),
                system_prompt: "你是一个只读检索代理：只使用 Read/Glob/Grep/Bash（只读命令）/TodoRead 查找与归纳，禁止修改任何文件。回复给出结论与关键文件路径，不贴大段文件内容。".to_owned(),
                tools: Some(vec![
                    "Read".to_owned(),
                    "Glob".to_owned(),
                    "Grep".to_owned(),
                    "TodoRead".to_owned(),
                ]),
                max_turns: None,
                
            }),
            _ => None,
        }
    }

    /// Loads user-level profiles from `~/.blackagent/agents/*.md`.
    /// Files without a parsable name/description are ignored (ZCode
    /// diagnostics semantics).
    pub fn load_user_profiles() -> Vec<Self> {
        let dir = agents_dir();
        let mut profiles = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return profiles;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(profile) = parse_agent_md(&text) {
                profiles.push(profile);
            }
        }
        profiles
    }

    /// Resolves a profile by id: built-ins first, then user definitions.
    pub fn resolve(id: &str) -> Option<Self> {
        Self::builtin(id).or_else(|| {
            Self::load_user_profiles().into_iter().find(|p| p.id == id)
        })
    }

    /// The tool list this profile exposes (intersected with the parent
    /// registry; Explore-style read-only sets map by name).
    pub fn tool_specs(&self, registry: &[ToolSpec]) -> Vec<ToolSpec> {
        match &self.tools {
            None => registry.to_vec(),
            Some(allowed) => registry
                .iter()
                .filter(|spec| allowed.contains(&spec.name))
                .cloned()
                .collect(),
        }
    }

    /// The sub-loop's declaration surface: the profile's tool subset minus
    /// Agent/SendMessage. Subagents cannot spawn or message agents (depth-1,
    /// enforced by `run_agent_turn` and structurally by `dispatch_registry`),
    /// so declaring those tools would only lure the sub-loop into trying.
    pub fn subagent_tool_specs(&self, registry: &[ToolSpec]) -> Vec<ToolSpec> {
        self.tool_specs(registry)
            .into_iter()
            .filter(|spec| spec.name != "Agent" && spec.name != "SendMessage")
            .collect()
    }
}

/// Parses one agent definition file: YAML-ish frontmatter + body system
/// prompt (camelCase keys, unknown keys ignored — ZCode semantics).
fn parse_agent_md(text: &str) -> Option<AgentProfile> {
    let rest = text.strip_prefix("---")?;
    let (frontmatter, body) = rest.split_once("---")?;
    let mut name = None;
    let mut description = None;
    let mut tools = None;
    let mut max_turns = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            "tools" => {
                let items: Vec<String> = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|item| item.trim().to_owned())
                    .filter(|item| !item.is_empty())
                    .collect();
                if !items.is_empty() {
                    tools = Some(items);
                }
            }
            "maxTurns" => max_turns = value.parse().ok(),
            _ => {}
        }
    }
    let name = name?;
    let description = description?;
    Some(AgentProfile {
        id: name,
        description,
        system_prompt: body.trim().to_owned(),
        tools,
        max_turns,
    })
}

/// Live state of one spawned agent.
struct AgentRecord {
    agent_id: String,
    child_session_id: String,
    subagent_type: String,
    title: String,
    running: bool,
    /// Ended in failure (failed status in snapshots).
    failed: bool,
    /// Full task prompt (truncated) for the detail view.
    prompt: String,
    /// Rolling tool-activity log (capped) shown live in the detail view.
    activity: Vec<String>,
    /// Final report text (present once ended).
    report: Option<String>,
/// Accumulated conversation (role, content) — the context SendMessage
    /// resumes into.
    history: Vec<(String, String)>,
}

/// 快照携带的活动/任务文本上限（字符）：防超长 prompt/报告把快照撑爆。
const RECORD_TEXT_CAP: usize = 4000;
/// 活动日志保留条数（超出截头，保留最近记录）。
const ACTIVITY_CAP: usize = 60;

/// 截断 helper：超长取前 `cap` 字符并补省略号。
fn cap_text(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(cap).collect();
    out.push('…');
    out
}

/// 从工具调用参数提取一行简报：优先取语义键（描述/路径/命令/检索词等），
/// 全缺回落原始 JSON 首段；截 80 字符。
fn brief_tool_args(args: &Value) -> String {
    let keys = [
        "description",
        "file_path",
        "filePath",
        "path",
        "command",
        "pattern",
        "query",
        "url",
        "name",
        "prompt",
    ];
    for key in keys {
        if let Some(text) = args.get(key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return cap_text(text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text), 80);
            }
        }
    }
    cap_text(&args.to_string(), 80)
}

/// Shared directory of spawned agents + snapshot revision counter.
pub struct AgentDirectory {
    records: Mutex<Vec<AgentRecord>>,
    revision: AtomicU64,
    /// The loop's event sender for snapshot re-emission.
    events: Mutex<Option<tokio::sync::mpsc::Sender<crate::protocol::Event>>>,
    /// Hard stop flag shared with spawned loops.
    stopped: Arc<AtomicBool>,
}

impl AgentDirectory {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            revision: AtomicU64::new(0),
            events: Mutex::new(None),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn attach_events(&self, events: tokio::sync::mpsc::Sender<crate::protocol::Event>) {
        *self.events.lock().unwrap() = Some(events);
    }

    fn snapshot_locked(&self) -> SubagentsSnapshot {
        let records = self.records.lock().unwrap();
        let entry = |record: &AgentRecord, status: &str| SubagentRunning {
            child_session_id: record.child_session_id.clone(),
            agent_id: Some(record.agent_id.clone()),
            subagent_type: record.subagent_type.clone(),
            title: record.title.clone(),
            status: status.to_owned(),
            summary: record.activity.last().cloned(),
            prompt: Some(record.prompt.clone()),
            activity: record.activity.clone(),
            report: record.report.clone(),
        };
        SubagentsSnapshot {
            revision: self.revision.load(Ordering::Relaxed),
            child_session_ids: records
                .iter()
                .map(|record| record.child_session_id.clone())
                .collect(),
            running: records
                .iter()
                .filter(|record| record.running)
                .map(|record| entry(record, "running"))
                .collect(),
            ended_total: records.iter().filter(|record| !record.running).count() as u64,
            ended: records
                .iter()
                .filter(|record| !record.running)
                .map(|record| {
                    entry(record, if record.failed { "failed" } else { "completed" })
                })
                .collect(),
        }
    }

    fn emit_snapshot(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        let snapshot = self.snapshot_locked();
        if let Some(events) = self.events.lock().unwrap().as_ref() {
            let _ = events.try_send(crate::protocol::Event::Subagents(snapshot));
        }
    }

    fn insert(&self, record: AgentRecord) {
        self.records.lock().unwrap().push(record);
        self.emit_snapshot();
    }

    fn set_running(&self, agent_id: &str, running: bool) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.agent_id == agent_id) {
            record.running = running;
        }
        drop(records);
        self.emit_snapshot();
    }

    /// 追加一条工具活动日志（容量截尾）并重发快照：详情窗格的实时活动源。
    fn record_activity(&self, agent_id: &str, line: String) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.agent_id == agent_id) {
            record.activity.push(line);
            if record.activity.len() > ACTIVITY_CAP {
                let excess = record.activity.len() - ACTIVITY_CAP;
                record.activity.drain(..excess);
            }
        }
        drop(records);
        self.emit_snapshot();
    }

    /// 结束一个 worker：置结束态、落最终报告（截断）并重发快照。
    fn finish(&self, agent_id: &str, failed: bool, report: Option<String>) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.agent_id == agent_id) {
            record.running = false;
            record.failed = failed;
            record.report = report.map(|text| cap_text(&text, RECORD_TEXT_CAP));
        }
        drop(records);
        self.emit_snapshot();
    }

    fn append_history(&self, agent_id: &str, role: &str, content: &str) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.agent_id == agent_id) {
            record.history.push((role.to_owned(), content.to_owned()));
        }
    }
}

/// Handle the parent loop keeps for the tool implementations.
pub struct AgentHost {
    pub directory: Arc<AgentDirectory>,
    /// Constructor parameters for spawned loops (parent provider/config).
    pub base_url: String,
    pub api_key: Option<String>,
    /// The parent session's model id; subagent clients must carry it too
    /// (Chat Completions requires an explicit model on every request).
    /// Set after the host resolves the requested model (engine `set_model`).
    model: std::sync::Mutex<String>,
    pub workspace: PathBuf,
    pub mode: PermMode,
    pub registry: Vec<ToolSpec>,
    pub system_prompt_suffix: String,
}

impl AgentHost {
    pub fn new(
        base_url: String,
        api_key: Option<String>,
        workspace: PathBuf,
        mode: PermMode,
        registry: Vec<ToolSpec>,
    ) -> Self {
        // Subagents ride the same `# Environment` disclosure as the main
        // loop: their tool subset includes Bash, so they need the
        // platform/shell disclosure to write runnable commands too.
        let system_prompt_suffix = super::shell::environment_section(&workspace);
        Self {
            directory: Arc::new(AgentDirectory::new()),
            base_url,
            api_key,
            model: std::sync::Mutex::new(String::new()),
            workspace,
            mode,
            registry,
            system_prompt_suffix,
        }
    }

    /// Propagates the parent session's model to future subagent spawns.
    pub fn set_model(&self, model: String) {
        *self.model.lock().unwrap() = model;
    }

    /// Runs one agent turn synchronously (from the tool handler's point of
    /// view): spawn → loop → final message. `agent_id` feeds the live
    /// activity log (one line per tool call, plus failure details).
    async fn run_agent_turn(
        &self,
        agent_id: &str,
        profile: &AgentProfile,
        prompt: &str,
        history: &[(String, String)],
    ) -> Result<String, String> {
        // The sub-loop is a plain chat-completions exchange with the
        // profile's system prompt, the seeded history and the profile's
        // tool subset; tools execute against a throwaway ToolContext
        // (isolated cwd state is shared — the workspace is the same).
        let mut client =
            crate::llm::ChatCompletionsClient::new(self.base_url.clone(), self.api_key.clone());
        let system = format!(
            "{}{}",
            profile.system_prompt,
            self.system_prompt_suffix
        );
        client.set_system_prompt(system);
        client.set_tools(profile.subagent_tool_specs(&self.registry));
        // Subagent requests ride the parent's model (engine clients reject
        // requests without an explicit model).
        let parent_model = self.model.lock().unwrap().clone();
        if !parent_model.is_empty() {
            client.core.model = Some(parent_model);
        }
        let ctx = Arc::new(ToolContext::new(
            &self.workspace,
            format!("subagent-{}", uuid_stub()),
            // Subagents cannot interact with the user; a closed channel
            // makes interaction tools fail fast.
            {
                let (tx, _rx) = tokio::sync::mpsc::channel(1);
                tx
            },
        ));

        // Seed prior history (SendMessage continuation).
        for (role, content) in history {
            match role.as_str() {
                "user" => client.core.push_user_message(content.clone()),
                _ => client.core.push_assistant_message(content.clone()),
            }
        }
        client.core.push_user_message(prompt.to_owned());

        // 轮数上限只在 profile 显式配置 maxTurns 时生效（防失控的自选
        // 保险）；缺省不限——与主循环「无人工轮数上限」同一语义（2026-11-09
        // 主人定案），旧版 unwrap_or(30) 的隐式默认已拆除。
        let max_rounds = profile.max_turns;
        let mut round = 0usize;
        loop {
            round += 1;
            if self.directory.stopped.load(Ordering::Relaxed) {
                return Err("子代理已被中断".to_owned());
            }
            if let Some(max_rounds) = max_rounds {
                if round > max_rounds {
                    return Err(format!("子代理工具轮数超过上限（{max_rounds}）"));
                }
            }
            let turn = client
                .complete()
                .await
                .map_err(|err| format!("子代理模型请求失败: {err}"))?;
            if turn.tool_calls.is_empty() {
                return Ok(turn.content);
            }
            for call in turn.tool_calls {
                // Depth-1: no further Agent spawning from subagents.
                if matches!(call.name.as_str(), "Agent" | "SendMessage") {
                    client.push_tool_result(
                        &call.id,
                        "子代理不能再派生子代理或发送消息（深度限制）".to_owned(),
                    );
                    continue;
                }
                let arguments: Value = serde_json::from_str(&call.arguments)
                    .unwrap_or(Value::Object(Default::default()));
                // 活动日志：调用即记一行（工具名 + 参数简报），详情窗格实时可见
                self.directory.record_activity(
                    agent_id,
                    format!("▸ {} {}", call.name, brief_tool_args(&arguments)),
                );
                // Subagent tool runs skip the permission gate only for
                // read-only tools; write-class tools follow the parent's
                // mode through the shared gate (ask → auto-deny in the
                // subagent context; the parent approves its own tools).
                let verdict = super::permission::gate(&call.name, self.mode, None);
                let result = match verdict {
                    super::permission::Verdict::Deny => {
                        Err("计划模式（只读）：该操作被拒绝".to_owned())
                    }
                    super::permission::Verdict::Ask if !super::permission::is_read_only_tool(&call.name) => {
                        // Interactive approval from inside a subagent is not
                        // available (no UI channel); deny with a clear text.
                        Err("该操作需要权限批准，子代理无交互通道，已拒绝".to_owned())
                    }
                    _ => super::tools::dispatch_registry(&ctx, &call.name, &arguments).await,
                };
                if let Err(err) = &result {
                    self.directory.record_activity(
                        agent_id,
                        format!("✗ {} 失败: {}", call.name, cap_text(&err.to_string(), 120)),
                    );
                }
                let text = result.unwrap_or_else(|err| format!("工具执行失败: {err}"));
                client.push_tool_result(&call.id, text);
            }
        }
    }
}

fn uuid_stub() -> String {
    // Lightweight unique suffix (no uuid dependency in this module).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

/// `try_dispatch`: the subagent tool surface (Agent / SendMessage). Returns
/// `None` when the tool name is not one of ours so the main registry can
/// try its own entries.
pub async fn try_dispatch(
    ctx: &ToolContext,
    name: &str,
    args: &Value,
) -> Option<Result<String, String>> {
    // The host is attached to the context by the backend wiring.
    let host = ctx.agent_host()?;
    match name {
        "Agent" => Some(run_agent_tool(&host, ctx, args).await),
        "SendMessage" => Some(run_send_message_tool(&host, args).await),
        _ => None,
    }
}

/// Registers the two subagent tool specs (called when the host wires the
/// registry with an AgentHost attached).
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "Agent".to_owned(),
            description: "Launches a specialized subagent (worker) that runs its own tool loop with an isolated context and returns its final report as the tool result. Use SendMessage with the returned agent id to continue a previously spawned agent with its context intact; a new Agent call starts fresh. Subagents cannot spawn further subagents.".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "A short (3-5 word) description of the task"},
                    "prompt": {"type": "string", "description": "The task for the agent to perform"},
                    "subagent_type": {"type": "string", "description": "The type of specialized agent to use (default general-purpose)"},
                    "run_in_background": {"type": "boolean", "description": "Set to true to run this agent in the background; you will be notified when it completes"}
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "SendMessage".to_owned(),
            description: "Sends a plain-text message to another agent (recipient format agent_<id> returned by Agent). Messaging a completed agent resumes it in the background with its context intact; you will be notified when it finishes again.".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "minLength": 1, "maxLength": 200, "description": "Recipient agent id (agent_...)"},
                    "summary": {"type": "string", "minLength": 1, "maxLength": 200, "description": "A 5-10 word summary shown as a preview in the UI"},
                    "message": {"type": "string", "minLength": 1, "maxLength": 20000, "description": "Plain text message content"}
                },
                "required": ["to", "summary", "message"],
                "additionalProperties": false
            }),
        },
    ]
}

async fn run_agent_tool(
    host: &Arc<AgentHost>,
    _ctx: &ToolContext,
    args: &Value,
) -> Result<String, String> {
    let prompt = args["prompt"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("缺少 prompt 参数")?
        .to_owned();
    let type_id = args["subagent_type"].as_str().unwrap_or("general-purpose");
    let background = args["run_in_background"].as_bool().unwrap_or(false);
    let profile = AgentProfile::resolve(type_id)
        .ok_or_else(|| format!("未知的子代理类型: {type_id}"))?;
    let description = args["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&profile.description)
        .to_owned();

    let agent_id = format!("agent_{}", uuid_stub());
    let child_session_id = format!("sess_subagent_{agent_id}");
    host.directory.insert(AgentRecord {
        agent_id: agent_id.clone(),
        child_session_id: child_session_id.clone(),
        subagent_type: type_id.to_owned(),
        title: description.clone(),
        running: true,
        failed: false,
        prompt: cap_text(&prompt, RECORD_TEXT_CAP),
        activity: Vec::new(),
        report: None,
        history: Vec::new(),
    });

    if background {
        // Park the task; the completion notice rides the next turn's
        // injected system note (see mod.rs send()).
        let host = host.clone();
        let bg_agent_id = agent_id.clone();
        let history: Vec<(String, String)> = Vec::new();
        tokio::spawn(async move {
            let result = host
                .run_agent_turn(&bg_agent_id, &profile, &prompt, &history)
                .await;
            match result {
                Ok(text) => {
                    host.directory.append_history(&bg_agent_id, "assistant", &text);
                    host.directory.finish(&bg_agent_id, false, Some(text.clone()));
                    background_results()
                        .lock()
                        .unwrap()
                        .insert(bg_agent_id, format!("子代理任务完成：{}", text.chars().take(400).collect::<String>()));
                }
                Err(err) => {
                    host.directory.finish(&bg_agent_id, true, Some(err.clone()));
                    background_results()
                        .lock()
                        .unwrap()
                        .insert(bg_agent_id, format!("子代理任务失败：{err}"));
                }
            }
        });
        return Ok(format!(
            "子代理已在后台启动（agent id: {agent_id}，类型 {type_id}）。完成后会收到通知。"
        ));
    }

    let result = host.run_agent_turn(&agent_id, &profile, &prompt, &[]).await;
    match result {
        Ok(text) => {
            host.directory
                .append_history(&agent_id, "assistant", &text);
            host.directory.finish(&agent_id, false, Some(text.clone()));
            Ok(format!(
                "[{agent_id} 报告]\n{text}"
            ))
        }
        Err(err) => {
            host.directory.finish(&agent_id, true, Some(err.clone()));
            Err(err)
        }
    }
}

/// Results of finished background agents/commands, consumed (and cleared)
/// by the next `send` as an injected system note.
pub static BACKGROUND_RESULTS: std::sync::OnceLock<Mutex<HashMap<String, String>>> =
    std::sync::OnceLock::new();

fn background_results() -> &'static Mutex<HashMap<String, String>> {
    BACKGROUND_RESULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drains the finished-background notices (next-turn injection).
pub fn drain_background_results() -> Vec<String> {
    let mut guard = background_results().lock().unwrap();
    guard.drain().map(|(_, value)| value).collect()
}

async fn run_send_message_tool(host: &Arc<AgentHost>, args: &Value) -> Result<String, String> {
    let to = args["to"].as_str().ok_or("缺少 to 参数")?.to_owned();
    let message = args["message"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("缺少 message 参数")?
        .to_owned();
    let record = {
        let records = host.directory.records.lock().unwrap();
        records
            .iter()
            .find(|record| record.agent_id == to)
            .map(|record| (record.history.clone(), record.running))
    };
    let Some((history, running)) = record else {
            return Err(format!("未找到收件代理 {to}"));
    };
    if running {
            return Err("该代理仍在运行中，请等它完成后再发消息".to_owned());
    }
    // Resume in place: re-run the loop with the full history plus the new
    // user message.
    host.directory.set_running(&to, true);
    let profile = AgentProfile::resolve("general-purpose").expect("general-purpose builtin");
    let result = host
        .run_agent_turn(&to, &profile, &message, &history)
        .await;
    host.directory.append_history(&to, "user", &message);
    match result {
        Ok(text) => {
            host.directory.append_history(&to, "assistant", &text);
            host.directory.finish(&to, false, Some(text.clone()));
        Ok(format!("[{to} 回复]\n{text}"))
        }
        Err(err) => {
            host.directory.finish(&to, true, Some(err.clone()));
            Err(err)
        }
    }
}

/// TodoEntry import guard (snapshot fields use the protocol type).
#[allow(unused)]
fn _todo_entry_type_hint(_entry: TodoEntry) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    /// 子代理声明面回归（2026-11-07 子代理无工具报障）：general-purpose
    ///（tools: None）应看到全量 registry 但不含 Agent/SendMessage（深度
    /// 限制在 dispatch 端结构性禁止递归，声明面必须对齐，否则诱导子代理
    /// 去派发然后被拒，任务原地打转）；Explore 按名单过滤且同样隐藏二者。
    #[test]
    fn subagent_specs_hide_agent_tools_and_keep_builtin_surface() {
        let registry = vec![
            spec("Bash"),
            spec("Read"),
            spec("Glob"),
            spec("TodoRead"),
            spec("Agent"),
            spec("SendMessage"),
        ];
        let general = AgentProfile::builtin("general-purpose").unwrap();
        let names: Vec<String> = general
            .subagent_tool_specs(&registry)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["Bash", "Read", "Glob", "TodoRead"]);

        let explore = AgentProfile::builtin("Explore").unwrap();
        let names: Vec<String> = explore
            .subagent_tool_specs(&registry)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["Read", "Glob", "TodoRead"]);
    }
}

