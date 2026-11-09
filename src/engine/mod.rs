//! In-process ZCode-style agent-loop executor.
//!
//! [`EngineBackend`] implements the [`crate::session::Backend`] trait
//! against an OpenAI Chat Completions compatible endpoint: every turn runs
//! the loop `stream → tool calls → permission gate → execute → tool
//! results → stream` inside channel (no subprocess). The tool surface is
//! the ZCode built-in set (see [`tools`]); the host receives the same
//! normalized [`Event`] stream the CLI backends produce.
//!
//! Session continuity: the model client's message log IS the engine
//! session. Live sessions reuse the in-memory log via the host's existing
//! slot reuse; after a drop/restart the host re-creates the backend from
//! the serialized client state it persisted (`initial_client_state`),
//! which is this backend's provider-resume equivalent.
//!
//! Feature: `engine` (implies `llm`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::llm::{ChatCompletionsClient, ToolCallData, ToolSpec};
use crate::protocol::{
    EngineProvider, Error, Event, Finish, HarnessKind, Status, TodoSnapshot, ToolCall, ToolStatus,
    UserInputRequest, UserQuestion, UserQuestionOption,
};
use crate::session::{Backend, BackendFuture, PermissionResponse, SendMode};


use crate::engine::hooks::{HookEvent, HookVerdict, HooksConfig};
use crate::engine::mcp::McpConnection;
use crate::engine::permission::{gate, PermMode, Verdict};
use crate::engine::tools::ToolContext;

pub(crate) mod adapter;
pub mod agents;
mod automation;
pub mod compact;
pub mod hooks;
pub mod mcp;
pub mod memory;
mod permission;
pub mod shell;
mod tools;

/// Host-registered automation store. The engine tool surface routes
/// Cron* tool calls through this trait object; the channel crate cannot
/// construct the host's scheduler, so the host drops its implementation
/// here once at startup (`set_global_automation_store`).
static AUTOMATION_STORE: std::sync::OnceLock<Mutex<Option<Arc<dyn AutomationStore>>>> =
    std::sync::OnceLock::new();

/// Registers the host's automation store (call once before the first
/// engine session). Replaces any previous registration.
pub fn set_global_automation_store(store: Arc<dyn AutomationStore>) {
    AUTOMATION_STORE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .replace(store);
}

/// The registered store, if any (`Cron*` tools fail gracefully without
/// one: "定时任务存储未注册").
pub(crate) fn global_automation_store() -> Option<Arc<dyn AutomationStore>> {
    AUTOMATION_STORE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
}

pub use automation::{
    local_utc_offset_seconds, next_cron_fire, next_interval_fire, AutomationStore, IntervalUnit,
    ScheduleSpec,
};
pub use permission::PermMode as EnginePermMode;
pub use tools::{InteractionKind, ToolContext as EngineToolContext};

/// 无工具轮数上限（2026-11-09 主人定案：「不应该存在这样的限制」）。
/// 旧版的每段 50 轮预算 + 撞限压缩续跑（最多 2 次，最坏 150 轮判死）已
/// 整体拆除：合法的长批量任务（整理上百文件等）不该被人为轮数预算打断，
/// 真失控由既有护栏兜底——流活性看门狗（首包/空闲/单轮总时长）、用户
/// 中断、宿主侧单轮总超时与事件空闲超时，模型停手即自然收尾。

/// Time-to-first-chunk watchdog for model requests (potato sets no
/// client-side timeouts, so a silently-dead upstream would otherwise wedge
/// the turn forever): both request establishment and the first stream event
/// must land within this window, after which a healthy stream is assumed
/// (chunk pacing stays interrupt-driven, not timeout-driven). 300s is
/// deliberately generous — slow reasoning endpoints still emit their first
/// SSE event well inside it.
const MODEL_FIRST_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Same window, in whole seconds, for error-message interpolation.
const MODEL_FIRST_CHUNK_TIMEOUT_SECS: u64 = 300;

/// Mid-stream idle watchdog: with no stream event for this long the
/// connection is declared black-holed and the round retried (deadline
/// resets on every real chunk). DeepSeek-style reasoning endpoints emit
/// thinking deltas continuously, so even thinking phases stay far inside
/// the window; 180s only ever fires on genuinely dead transports.
const STREAM_CHUNK_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// Same window, in whole seconds, for error-message interpolation.
const STREAM_CHUNK_IDLE_TIMEOUT_SECS: u64 = 180;

/// 单轮总时长硬上限（自建流起算）。空闲看门狗只看事件间隔——「僵尸
/// 滴流」态（连接半死、上游每几分钟滴一个事件）会被持续重置而永不判
/// 死，turn 呈无限僵持（2026-11-08 护栏第 6/7 轮真机复现：round 22 后
/// 12 分钟静默、事件偶滴、600s 单次等待永不超时）。超限即强断本轮流并
/// 原样重试：重试是全新连接，自然摆脱僵尸态。正常单轮远低于此上限，
/// 本地大模型长考也远在界内。
const ROUND_TOTAL_CAP: std::time::Duration = std::time::Duration::from_secs(900);
/// Same cap, in whole seconds, for error-message interpolation.
const ROUND_TOTAL_CAP_SECS: u64 = 900;

/// 权限询问等待应答的上限：宿主/UI 忘记消费权限事件时，宁可判拒也不
/// 让 turn 永挂（2026-11-08 护栏真机踩坑：事件被静默吞掉后 waiter 永挂
/// 全链冻结）。600s 覆盖人肉审批的常规节奏。
const PERMISSION_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// Summary-request budget for the watermark compaction path: one extra
/// model round, bounded so a wedged summarizer degrades to "skip
/// compaction" instead of wedging the turn. Generous for slow local
/// endpoints; deep-thinking endpoints still answer ordinary summarization
/// far inside it.
const SUMMARY_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
/// Same budget, in whole seconds, for error-message interpolation.
const SUMMARY_BUDGET_SECS: u64 = 180;

/// Bounded stall retries for the model stream watchdog: a TTFB timeout
/// aborts nothing (the message log is untouched), so the identical round is
/// simply re-requested — up to this many retries (3 attempts total) before
/// the turn fails with the recorded reason.
const MODEL_STALL_RETRIES: usize = 2;

/// Assembles the engine's tool declaration surface: built-in registry
/// specs first, then host extras (`engine_tools`) and MCP tools merged by
/// name (first occurrence wins, so a same-named extra cannot shadow a
/// built-in). The subagent specs (Agent/SendMessage) are appended by the
/// caller after this so subagent nesting order stays deterministic.
fn assemble_tool_registry(
    extras: Vec<ToolSpec>,
    mcp_tools: impl IntoIterator<Item = ToolSpec>,
) -> Vec<ToolSpec> {
    let mut all: Vec<ToolSpec> = tools::registry()
        .into_iter()
        .map(|entry| entry.spec)
        .collect();
    let push_unique = |spec: ToolSpec, out: &mut Vec<ToolSpec>| {
        if !out.iter().any(|existing| existing.name == spec.name) {
            out.push(spec);
        }
    };
    for spec in extras {
        push_unique(spec, &mut all);
    }
    for spec in mcp_tools {
        push_unique(spec, &mut all);
    }
    all
}

/// Shared environment for the agent loop (everything the per-turn task
/// needs beyond the message log).
struct EngineEnv {
    events: mpsc::Sender<Event>,
    ctx: Arc<ToolContext>,
    permission_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<crate::session::PermissionResponse>>>>,
    interaction_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
    mode: PermMode,
    hooks: HooksConfig,
    mcp: Vec<Arc<tokio::sync::Mutex<McpConnection>>>,
    context_window: u64,
    max_output_tokens: Option<u64>,
}

/// The in-process engine backend. One instance per UI session.
pub struct EngineBackend {
    /// Permission mode for the session (set at creation; the host rebuilds
    /// the session when it changes, matching the existing slot logic).
    mode: PermMode,
    /// Model client shared with the loop task (message log inside).
    client: Arc<tokio::sync::Mutex<ChatCompletionsClient>>,
    ctx: Arc<ToolContext>,
    /// Interaction bridge sender (AskUserQuestion/plan tools 鈫?router).
    interact_tx: mpsc::Sender<tools::InteractRequest>,
    /// Loop 鈫?backend event queue (`next_event` drains it).
    events_tx: mpsc::Sender<Event>,
    events_rx: Option<mpsc::Receiver<Event>>,
    /// Permission id 鈫?waiter (routed from respond_permission).
    permission_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<PermissionResponse>>>>,
    /// Interaction id 鈫?waiter (plan approvals routed like permissions).
    interaction_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
    /// Agent-loop task handle for the active turn.
    loop_handle: Option<tokio::task::JoinHandle<()>>,
    interrupted: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    /// True when restored from a serialized client state (resume path).
    resumed_from_state: bool,
    /// Hooks config snapshot (read once at construction).
    hooks: HooksConfig,
    /// Live MCP server connections (tool calls route through these).
    mcp: Vec<Arc<tokio::sync::Mutex<McpConnection>>>,
    /// Provider context window (compaction watermark), 0 = unknown.
    context_window: u64,
    max_output_tokens: Option<u64>,
}

impl EngineBackend {
    /// Builds a backend from the provider spec + the session-level fields
    /// the adapter extracted. `initial_client_state` (optional serialized
    /// [`ChatCompletionsClient`]) restores a previous engine session
    /// (message log continuity after a drop/restart).
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        _kind: HarnessKind,
        provider: &EngineProvider,
        workspace: PathBuf,
        permission_mode: Option<&str>,
        system_prompt: Option<String>,
        initial_client_state: Option<String>,
        automation: Option<Arc<dyn AutomationStore>>,
        available_tools: Vec<ToolSpec>,
        context_window: u64,
        max_output_tokens: Option<u64>,
    ) -> Result<Self, Error> {
        let mut client = match &initial_client_state {
            Some(state) => ChatCompletionsClient::deserialize(state).map_err(|err| {
                Error::InvalidConfig(format!("寮曟搸浼氳瘽鐘舵€佹仮澶嶅け璐? {err}"))
            })?,
            None => {
                let mut client = ChatCompletionsClient::new(
                    provider.base_url.clone(),
                    provider.api_key.clone(),
                );
                // System prompt = host custom prompt (if any) + the `#
                // Environment` disclosure (ZCode context semantics: platform
                // / shell / OS so the model writes commands for the shell
                // that will actually run them) + the persistent-memory
                // section when the switch is on.
                let mut prompt = system_prompt.unwrap_or_default();
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&crate::engine::shell::environment_section(&workspace));
                if crate::engine::memory::memory_enabled() {
                    prompt.push_str(&crate::engine::memory::system_prompt_section(&workspace));
                }
                if !prompt.is_empty() {
                    client.set_system_prompt(prompt);
                }
                client
            }
        };

        let (events_tx, events_rx) = mpsc::channel::<Event>(256);
        let (interact_tx, interact_rx) = mpsc::channel::<tools::InteractRequest>(16);
        let ctx = Arc::new(ToolContext::new(
            &workspace,
            format!("engine-{}", crate::llm::unix_micros()),
            interact_tx.clone(),
        ));
        *ctx.automation.lock().unwrap() = automation;

        // MCP servers: connect enabled ones; their tools join the registry
        // (`mcp__<server>__<tool>` naming). A failing server is skipped.
        let (mcp_connections, mut mcp_tools, _mcp_errors) =
            crate::engine::mcp::connect_all().await;
        // Hooks config snapshot (session-start semantics).
        let hooks = HooksConfig::load();
        if hooks.enabled {
            // SessionStart hook (source: startup).
            let hook_ctx = ctx.clone();
            let _ = crate::engine::hooks::run_hooks(
                &hooks,
                HookEvent::SessionStart,
                &hook_ctx,
                None,
                None,
            )
            .await;
        }

        // Tool declaration surface: the built-in registry (Bash/Read/... in
        // `tools::registry`) is the baseline 鈥?`engine_tools` is an *extra*
        // beyond it (adapter doc semantics), never a replacement. Missing
        // this left sessions (and subagents) declaring only Agent/
        // SendMessage. Extras merge by name, built-ins win.
        let mut all_tools = assemble_tool_registry(available_tools, mcp_tools.drain(..));
        all_tools.extend(agents::tool_specs());
        let agent_host = Arc::new(agents::AgentHost::new(
            provider.base_url.clone(),
            provider.api_key.clone(),
            workspace.clone(),
            PermMode::parse(permission_mode),
            all_tools.clone(),
        ));
        *ctx.agent_host.lock().unwrap() = Some(agent_host.clone());
        agent_host.directory.attach_events(events_tx.clone());
        client.set_tools(all_tools);

        let permission_waiters: Arc<
            Mutex<HashMap<String, oneshot::Sender<PermissionResponse>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let interaction_waiters: Arc<
            Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>,
        > = Arc::new(Mutex::new(HashMap::new()));

        // Interaction router: tools hand requests here, the router emits
        // the wire event and parks the answer slot until the host responds.
        spawn_interaction_router(
            interact_rx,
            events_tx.clone(),
            permission_waiters.clone(),
            interaction_waiters.clone(),
        );

        Ok(Self {
            mode: PermMode::parse(permission_mode),
            client: Arc::new(tokio::sync::Mutex::new(client)),
            ctx,
            events_tx,
            events_rx: Some(events_rx),
            interact_tx,
            permission_waiters,
            interaction_waiters,
            loop_handle: None,
            interrupted: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            resumed_from_state: initial_client_state.is_some(),
            hooks,
            mcp: mcp_connections,
            context_window,
            max_output_tokens,
        })
    }

    /// True when this backend was restored from a serialized client state
    /// (the engine's provider-resume equivalent).
    pub fn is_resumed(&self) -> bool {
        self.resumed_from_state
    }

    /// Serializes the model client (full conversation state) for the host
    /// to persist; restoring it later rebuilds the engine session with its
    /// history intact.
    pub async fn serialize_client(&self) -> Result<String, Error> {
        self.client.lock().await.serialize()
    }

    /// Replaces the tool specs (MCP tools are appended through the same
    /// path before the first turn).
    pub async fn set_tools(&self, tools: Vec<ToolSpec>) {
        self.client.lock().await.set_tools(tools);
    }

    /// The interaction bridge sender (host-side plumbing tests).
    pub fn interact_sender(&self) -> mpsc::Sender<tools::InteractRequest> {
        self.interact_tx.clone()
    }

    /// Applies the session model to the main client AND the subagent host
    /// (subagent clients must carry the same model on every request).
    pub async fn configure_model(&self, model: impl Into<String>) -> Result<(), crate::protocol::Error> {
        let model = model.into();
        if let Some(host) = self.ctx.agent_host.lock().unwrap().as_ref() {
            host.set_model(model.clone());
        }
        self.client.lock().await.set_model(model).await
    }

    /// Spawns the agent loop for the current message log (used by `send`).
    fn spawn_loop(&mut self) {
        let env = EngineEnv {
            events: self.events_tx.clone(),
            ctx: self.ctx.clone(),
            permission_waiters: self.permission_waiters.clone(),
            interaction_waiters: self.interaction_waiters.clone(),
            mode: self.mode,
            hooks: self.hooks.clone(),
            mcp: self.mcp.clone(),
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
        };
        let client = self.client.clone();
        let interrupted = self.interrupted.clone();
        let closed = self.closed.clone();
        let handle = tokio::spawn(run_agent_loop(env, client, interrupted, closed));
        self.loop_handle = Some(handle);
    }
}

/// Spawns the router that turns tool interaction requests into wire events
/// and routes the host's answers back to the parked tool.
fn spawn_interaction_router(
    mut interact_rx: mpsc::Receiver<tools::InteractRequest>,
    events: mpsc::Sender<Event>,
    _permission_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<PermissionResponse>>>>,
    interaction_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
) {
    tokio::spawn(async move {
        while let Some(request) = interact_rx.recv().await {
            let (waiter_tx, waiter_rx) = oneshot::channel();
            match request.kind {
                tools::InteractionKind::Ask => {
                    let wire = build_user_input_request(&request.id, &request.payload);
                    interaction_waiters
                        .lock()
                        .unwrap()
                        .insert(request.id.clone(), waiter_tx);
                    if events.send(Event::UserInputRequired(wire)).await.is_err() {
                        break;
                    }
                }
                tools::InteractionKind::Plan => {
                    let detail = match request.payload.get("plan").and_then(|p| p.as_str()) {
                        Some(plan) => format!(
                            "计划审批\n\n{plan}\n\n（批准后开始实施；拒绝则继续修订计划）"
                        ),
                        None => "请求进入计划模式（只读规划，提交计划后再实施）".to_owned(),
                    };
                    interaction_waiters
                        .lock()
                        .unwrap()
                        .insert(request.id.clone(), waiter_tx);
                    if events
                        .send(Event::PermissionRequired {
                            id: request.id.clone(),
                            detail,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            // Wait for the host answer; a dropped channel means the host is
            // gone 鈥?resolve as declined so the tool unblocks.
            let answer = waiter_rx
                .await
                .unwrap_or_else(|_| serde_json::json!({"approved": false, "declined": true}));
            let _ = request.responder.send(answer);
        }
    });
}

/// Maps AskUserQuestion tool payload questions into the normalized
/// [`UserInputRequest`] (ids `q0..qn` preserve order).
fn build_user_input_request(id: &str, payload: &serde_json::Value) -> UserInputRequest {
    let mut questions = Vec::new();
    if let Some(items) = payload.get("questions").and_then(|q| q.as_array()) {
        for (index, item) in items.iter().enumerate() {
            let options: Vec<UserQuestionOption> = item
                .get("options")
                .and_then(|o| o.as_array())
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| UserQuestionOption {
                            label: entry
                                .get("label")
                                .and_then(|l| l.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            description: entry
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(str::to_owned),
                        })
                        .collect()
                })
                .unwrap_or_default();
            questions.push(UserQuestion {
                id: format!("q{index}"),
                header: item
                    .get("header")
                    .and_then(|h| h.as_str())
                    .map(str::to_owned),
                question: item
                    .get("question")
                    .and_then(|q| q.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                options,
                multi_select: item
                    .get("multiSelect")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false),
            });
        }
    }
    UserInputRequest {
        id: id.to_owned(),
        source: Some("AskUserQuestion".to_owned()),
        questions,
        payload: serde_json::Value::Null,
    }
}

/// Runs one agent turn to completion: stream rounds until the model stops
/// calling tools, then emit `Finished`.
async fn run_agent_loop(
    env: EngineEnv,
    client: Arc<tokio::sync::Mutex<ChatCompletionsClient>>,
    interrupted: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
) {
    let EngineEnv {
        events,
        ctx,
        permission_waiters,
        interaction_waiters,
        mode,
        hooks,
        mcp,
        context_window,
        max_output_tokens,
    } = env;
    let _ = (&interaction_waiters, &ctx);

    // UserPromptSubmit hook fires before the first model round; its
    // additionalContext is appended to the log as a system-side note.
    let prompt_context = if hooks.enabled {
        match crate::engine::hooks::run_hooks(
            &hooks,
            HookEvent::UserPromptSubmit,
            &ctx,
            None,
            None,
        )
        .await
        {
            HookVerdict::Continue {
                additional_context: Some(context),
            } => Some(context),
            HookVerdict::Block { reason } => {
                // A blocked prompt never reaches the model; surface the
                // reason and finish the turn without a model call.
                let _ = events.send(Event::Error(reason)).await;
                let _ = events
                    .send(Event::Finished(Finish {
                        status: Status::Failed,
                        text: String::new(),
                    }))
                    .await;
                return;
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some(context) = prompt_context {
        client
            .lock()
            .await
            .core
            .push_assistant_message(format!("[hook 注入] {context}"));
    }

    let _ = events.send(Event::Status(Status::Running)).await;
    // round 只是观测计数（日志心跳用）：无轮数上限，循环直到模型停手、
    // 用户中断或看门狗护栏触发。
    let mut round = 0usize;
    let outcome: Result<String, String> = loop {
        if stopped(&interrupted, &closed) {
            break Err(INTERRUPTED.to_owned());
        }
        round += 1;

        // 1) Stream one model round over the current message log. Every
        // phase is watchdogged — potato sets no client-side timeouts and its
        // SSE reader task can stall forever on a black-holed connection:
        // ① request establishment and ② the first chunk must land within
        // MODEL_FIRST_CHUNK_TIMEOUT; ③ mid-stream gaps must stay under
        // STREAM_CHUNK_IDLE_TIMEOUT with the deadline reset on every real
        // chunk. All watchdog trips are retryable — the round is re-requested
        // up to MODEL_STALL_RETRIES times (nothing reaches the message log
        // until the round finalizes, so retries are idempotent); provider
        // rejections, stream errors and channel-closed failures stay
        // terminal.
        enum RoundOutcome {
            Done,
            Failed(String),
        }
        let mut tool_calls: Vec<ToolCallData> = Vec::new();
        let mut last_usage: Option<crate::llm::StreamUsage> = None;
        let mut chunk_watch = 0usize;
        let round_outcome: RoundOutcome = 'round: {
            let mut reason = String::new();
            for attempt in 0..=MODEL_STALL_RETRIES {
                eprintln!(
                    "[blackagent-engine] round {round} attempt {attempt}: opening model stream"
                );
                if attempt > 0 {
                    eprintln!(
                        "[blackagent-engine] 模型流停滞（第 {attempt}/{MODEL_STALL_RETRIES} 次）：{reason}，原样重试"
                    );
                }
                // ①② 建流 + 首包，同一看门狗窗口。
                let (mut rx, mut pending_chunk) = {
                    eprintln!("[blackagent-engine] acquiring client lock");
                    let mut client = client.lock().await;
                    eprintln!("[blackagent-engine] lock acquired, awaiting stream");
                    let stream = tokio::time::timeout(
                        MODEL_FIRST_CHUNK_TIMEOUT,
                        client.stream_completion(),
                    )
                    .await;
                    eprintln!("[blackagent-engine] stream establishment returned");
                    let mut rx = match stream {
                        Ok(Ok(rx)) => rx,
                        Ok(Err(err)) => break 'round RoundOutcome::Failed(format!(
                            "模型请求失败: {err}"
                        )),
                        Err(_) => {
                            reason = format!(
                                "模型请求超时：{MODEL_FIRST_CHUNK_TIMEOUT_SECS}s 内未建立响应流（上游无响应）"
                            );
                            continue;
                        }
                    };
                    let first =
                        tokio::time::timeout(MODEL_FIRST_CHUNK_TIMEOUT, rx.recv()).await;
                    eprintln!("[blackagent-engine] first-chunk await returned");
                    match first {
                        Ok(chunk) => (rx, chunk),
                        Err(_) => {
                            reason = format!(
                                "模型流首包超时：连接建立后 {MODEL_FIRST_CHUNK_TIMEOUT_SECS}s 无任何流事件（上游疑似挂死）"
                            );
                            continue;
                        }
                    }
                };
                // ③ 消费流：每个真实 chunk 重置空闲期限；期限内没有任何
                // 事件即判流死（可重试）；单轮总时长超 ROUND_TOTAL_CAP
                // 强断重试（防僵尸滴流绕过空闲看门狗）；中断与事件通道
                // 关闭终局化。
                let mut deadline =
                    tokio::time::Instant::now() + STREAM_CHUNK_IDLE_TIMEOUT;
                let round_cap = tokio::time::Instant::now() + ROUND_TOTAL_CAP;
                let mut round_tool_calls: Vec<ToolCallData> = Vec::new();
                let mut round_usage: Option<crate::llm::StreamUsage> = None;
                let failure: Option<String> = loop {
                    let chunk = match pending_chunk.take() {
                        Some(chunk) => Some(chunk),
                        None => tokio::select! {
                            chunk = rx.recv() => chunk,
                            _ = tokio::time::sleep_until(deadline) => {
                                reason = format!(
                                    "模型流中段无事件超过 {STREAM_CHUNK_IDLE_TIMEOUT_SECS}s（连接疑似黑洞）"
                                );
                                break None; // stalled → retryable
                            }
                            _ = tokio::time::sleep_until(round_cap) => {
                                reason = format!(
                                    "单轮总时长超过 {ROUND_TOTAL_CAP_SECS}s（疑似僵尸滴流：事件偶滴重置空闲看门狗而永不判死）"
                                );
                                break None; // stalled → retryable
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                                if stopped(&interrupted, &closed) {
                                    break Some(INTERRUPTED.to_owned());
                                }
                                continue;
                            }
                        },
                    };
                    match chunk {
                        Some(crate::llm::StreamChunk::Content(text)) => {
                            deadline =
                                tokio::time::Instant::now() + STREAM_CHUNK_IDLE_TIMEOUT;
                            chunk_watch += 1;
                            if chunk_watch.is_multiple_of(25) {
                                eprintln!(
                                    "[blackagent-engine] round {round}: {chunk_watch} stream chunks consumed"
                                );
                            }
                            if events.send(Event::TextDelta(text)).await.is_err() {
                                break Some("事件通道已关闭".to_owned());
                            }
                        }
                        Some(crate::llm::StreamChunk::ToolCalls(calls)) => {
                            deadline =
                                tokio::time::Instant::now() + STREAM_CHUNK_IDLE_TIMEOUT;
                            round_tool_calls = calls;
                        }
                        Some(crate::llm::StreamChunk::Usage(chunk_usage)) => {
                            deadline =
                                tokio::time::Instant::now() + STREAM_CHUNK_IDLE_TIMEOUT;
                            let _ = events
                                .send(Event::ContextUsage {
                                    used: chunk_usage.prompt_tokens,
                                    total: context_window,
                                })
                                .await;
                            round_usage = Some(chunk_usage);
                        }
                        Some(crate::llm::StreamChunk::Error(message)) => {
                            break Some(format!("模型流错误: {message}"));
                        }
                        // 正常收尾：提交本轮采集的状态并结束整轮。
                        Some(crate::llm::StreamChunk::Done) | None => {
                            tool_calls = round_tool_calls;
                            last_usage = round_usage;
                            eprintln!(
                                "[blackagent-engine] round {round}: stream done after {chunk_watch} chunks, {} tool call(s)",
                                tool_calls.len()
                            );
                            break 'round RoundOutcome::Done;
                        }
                    }
                };
                match failure {
                    Some(err) => break 'round RoundOutcome::Failed(err),
                    None => {} // stalled：可重试，落入下一轮 attempt
                }
            }
            RoundOutcome::Failed(format!(
                "模型流反复停滞（原样重试 {MODEL_STALL_RETRIES} 次仍无进展）：{reason}"
            ))
        };
        if let RoundOutcome::Failed(err) = round_outcome {
            break Err(err);
        }
        if stopped(&interrupted, &closed) {
            break Err(INTERRUPTED.to_owned());
        }

        // 1.5) Compaction check (ZCode auto-compact semantics): when the
        // observed usage crosses the watermark, summarize and replace the
        // history; the boundary event goes to the host for transcription.
        // Providers that never report usage fall back to a byte estimate
        // (~4 bytes/token; errs toward compacting earlier).
        let usage = match last_usage {
            Some(usage) => Some(usage),
            None => {
                let guard = client.lock().await;
                crate::engine::compact::estimate_usage_from_log(&guard.messages())
            }
        };
        if let Some(usage) = usage {
            let mut client_guard = client.lock().await;
            let compaction = tokio::time::timeout(
                SUMMARY_BUDGET,
                crate::engine::compact::maybe_compact(
                    &mut client_guard,
                    usage,
                    context_window,
                    max_output_tokens,
                ),
            )
            .await;
            match compaction {
                // 摘要超时与摘要失败同权：记日志、不压缩、回合继续。
                Ok(crate::engine::compact::CompactionOutcome::Completed { payload, .. }) => {
                    let _ = events
                        .send(Event::CompactionBoundary { payload })
                        .await;
                }
                Ok(crate::engine::compact::CompactionOutcome::Failed(err)) => {
                    eprintln!("[blackagent-engine] 压缩失败（继续）: {err}");
                }
                Ok(crate::engine::compact::CompactionOutcome::NotNeeded) => {}
                Err(_) => {
                    eprintln!(
                        "[blackagent-engine] 压缩摘要请求超时（{SUMMARY_BUDGET_SECS}s，继续不压缩）"
                    );
                }
            }
        }

        // 2) No tool calls → the turn's visible answer is the streamed text.
        if tool_calls.is_empty() {
            break Ok(last_assistant_text(&client).await);
        }

        // 3) Execute each requested tool behind the permission gate.
        let mut was_interrupted = false;
        for call in tool_calls {
            if stopped(&interrupted, &closed) {
                was_interrupted = true;
                break;
            }
            let result = execute_tool_call(
                &events, &ctx, &permission_waiters, mode, &hooks, &mcp, &call,
            )
            .await;
            client
                .lock()
                .await
                .push_tool_result(&call.id, result);
            eprintln!("[blackagent-engine] round {round}: tool {} done", call.name);
        }
        if was_interrupted {
            break Err(INTERRUPTED.to_owned());
        }
    };

    // 4) Stop hook, then the finish event.
    if hooks.enabled {
        let _ = crate::engine::hooks::run_hooks(
            &hooks,
            HookEvent::Stop,
            &ctx,
            None,
            None,
        )
        .await;
    }
    let finish = match outcome {
        Ok(text) => Finish {
            status: Status::Completed,
            text,
        },
        Err(err) if err == INTERRUPTED => Finish {
            status: Status::Interrupted,
            text: last_assistant_text(&client).await,
        },
        Err(err) => {
            let _ = events.send(Event::Error(err.clone())).await;
            Finish {
                status: Status::Failed,
                text: last_assistant_text(&client).await,
            }
        }
    };
    let _ = events.send(Event::Finished(finish)).await;
}

const INTERRUPTED: &str = "__ENGINE_INTERRUPTED__";

fn stopped(interrupted: &AtomicBool, closed: &AtomicBool) -> bool {
    interrupted.load(Ordering::Relaxed) || closed.load(Ordering::Relaxed)
}

/// The trailing assistant text (the turn's visible answer).
async fn last_assistant_text(
    client: &Arc<tokio::sync::Mutex<ChatCompletionsClient>>,
) -> String {
    client
        .lock()
        .await
        .messages()
        .into_iter()
        .rev()
        .find(|message| message.role == crate::llm::MessageRole::Assistant)
        .map(|message| message.content)
        .unwrap_or_default()
}

/// Executes one tool call: permission gate 鈫?hooks 鈫?run 鈫?wire events.
/// Returns the tool result text (both success and recoverable-error text
/// become tool result messages for the model).
async fn execute_tool_call(
    events: &mpsc::Sender<Event>,
    ctx: &Arc<ToolContext>,
    permission_waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<PermissionResponse>>>>,
    mode: PermMode,
    hooks: &HooksConfig,
    mcp: &[Arc<tokio::sync::Mutex<McpConnection>>],
    call: &ToolCallData,
) -> String {
    let mut arguments: serde_json::Value = serde_json::from_str(&call.arguments)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    // PreToolUse hook: may block, or replace the tool input wholesale.
    if hooks.enabled {
        match crate::engine::hooks::run_hooks(
            hooks,
            HookEvent::PreToolUse,
            ctx,
            Some(&call.name),
            Some(&arguments),
        )
        .await
        {
            HookVerdict::Block { reason } => {
                return format!("鎿嶄綔琚?hook 闃绘柇: {reason}");
            }
            HookVerdict::ModifyInput { updated_input } => {
                arguments = updated_input;
            }
            HookVerdict::Continue {
                additional_context: Some(context),
            } => {
                // Context rides into the result text (the model sees it).
                let _ = context;
            }
            HookVerdict::Continue { .. } => {}
        }
    }
    let command = arguments.get("command").and_then(|c| c.as_str());

    // Permission gate.
    match gate(&call.name, mode, command) {
        Verdict::Deny => {
            return format!(
                "权限模式为 plan（只读规划），工具 {} 被拒绝。请先用 EnterPlanMode 提交计划获取批准。",
                call.name
            );
        }
        Verdict::Ask => {
            // PermissionRequest hooks observe (or veto) before the UI ask.
            if hooks.enabled {
                if let HookVerdict::Block { reason } = crate::engine::hooks::run_hooks(
                    hooks,
                    HookEvent::PermissionRequest,
                    ctx,
                    Some(&call.name),
                    Some(&arguments),
                )
                .await
                {
                    return format!("权限请求被 hook 阻断: {reason}");
                }
            }
            let request_id = format!("perm-{}", call.id);
            let detail = match command {
                Some(command) => format!("执行命令: {command}"),
                None => format!("执行工具: {}", call.name),
            };
            let (waiter_tx, waiter_rx) = oneshot::channel();
            permission_waiters
                .lock()
                .unwrap()
                .insert(request_id.clone(), waiter_tx);
            if events
                .send(Event::PermissionRequired {
                    id: request_id.clone(),
                    detail,
                })
                .await
                .is_err()
            {
                return "事件通道已关闭".to_owned();
            }
            // 等待应答同样套看门狗：宿主忘记消费/转发权限事件时，宁可
            // 拒绝也不能把整个 turn 永久楔死（2026-11-08 护栏真机踩坑：
            // 事件被静默吞掉后 waiter 永挂，全链冻结）。超时按拒绝处理，
            // 模型会收到原因并自行改道。
            let answer =
                match tokio::time::timeout(PERMISSION_WAIT_BUDGET, waiter_rx).await {
                    Ok(Ok(answer)) => answer,
                    Ok(Err(_)) | Err(_) => {
                        permission_waiters.lock().unwrap().remove(&request_id);
                        PermissionResponse::Deny
                    }
                };
            if answer == PermissionResponse::Deny {
                return "权限请求未获批准（被拒绝或等待应答超时）。请调整方案，或换用当前权限模式下允许的操作继续任务。".to_owned();
            }
        }
        Verdict::Allow => {}
    }

    // Pre-tool wire event (activity row in "running" state).
    let _ = events
        .send(Event::ToolCall(ToolCall {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            status: ToolStatus::Running,
            input: Some(call.arguments.clone()),
            output: None,
            error: None,
            sequence: 0,
        }))
        .await;

    // Run the handler (MCP tools route through the connections; built-ins
    // through the registry).
    let started = std::time::Instant::now();
    let result = if call.name.starts_with("mcp__") {
        crate::engine::mcp::dispatch_mcp_call(mcp, &call.name, arguments.clone())
            .await
    } else {
        tools::dispatch(ctx, &call.name, &arguments).await
    };
    let elapsed = started.elapsed();

    // Post-tool hooks (failure variant gets the structured error).
    if hooks.enabled {
        let hook_event = match &result {
            Ok(_) => HookEvent::PostToolUse,
            Err(_) => HookEvent::PostToolUseFailure,
        };
        let hook_payload = serde_json::json!({
            "tool_input": arguments,
            "tool_response": match &result {
                Ok(text) => serde_json::json!({"content": text.chars().take(4000).collect::<String>()}),
                Err(err) => serde_json::json!({"error": err}),
            },
            "duration_ms": elapsed.as_millis(),
        });
        if let HookVerdict::Block { reason } =
            crate::engine::hooks::run_hooks(hooks, hook_event, ctx, Some(&call.name), Some(&hook_payload)).await
        {
            // A blocking post-hook escalates the result into an error the
            // model sees.
            return format!("hook 阻断: {reason}");
        }
    }

    // Post-tool wire event with the outcome.
    let (status, output, error) = match &result {
        Ok(text) => (
            ToolStatus::Completed,
            Some(text.chars().take(2000).collect()),
            None,
        ),
        Err(err) => (
            ToolStatus::Failed,
            None,
            Some(err.chars().take(500).collect()),
        ),
    };
    let _ = events
        .send(Event::ToolCall(ToolCall {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            status,
            input: Some(call.arguments.clone()),
            output,
            error,
            sequence: 0,
        }))
        .await;

    // TodoWrite emits the normalized snapshot event for the UI card.
    if call.name == "TodoWrite" {
        let todo_entries = ctx.todos.lock().unwrap().clone();
        let _ = events
            .send(Event::Todo(TodoSnapshot {
                entries: todo_entries,
                explanation: None,
            }))
            .await;
    }

    // Both success and recoverable-error text become the tool result the
    // model sees; the wire event above already carries the failure state.
    match result {
        Ok(text) => text,
        Err(err) => format!("工具执行失败: {err}"),
    }
}

// ---------------------------------------------------------------------------
// Backend impl
// ---------------------------------------------------------------------------

impl Backend for EngineBackend {
    fn id(&self) -> Option<String> {
        None
    }

    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            if self.closed.load(Ordering::Relaxed) {
                return Err(Error::Closed);
            }
            // Finished background work (agents/commands) is injected ahead
            // of the new user message so the model sees the notices.
            let mut text = message;
            for notice in crate::engine::agents::drain_background_results() {
                text = format!("[后台任务通知] {notice}\n\n{text}");
            }
            self.client.lock().await.core.push_user_message(text);
            self.interrupted.store(false, Ordering::Relaxed);
            self.spawn_loop();
            Ok(())
        })
    }

    fn send_with_attachments<'a>(
        &'a mut self,
        message: String,
        attachments: &[String],
    ) -> BackendFuture<'a, ()> {
        // Owned copy so the future only borrows `self`.
        let attachments = attachments.to_vec();
        Box::pin(async move {
            // Attachments become path references; the model reads them with
            // its own Read tool (ZCode --attach surface).
            let mut text = message;
            for path in attachments {
                text.push_str(&format!("\n\n[附件] {path}"));
            }
            Backend::send(self, text).await
        })
    }

    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
        Box::pin(async move {
            let rx = self.events_rx.as_mut().ok_or(Error::Closed)?;
            match rx.recv().await {
                Some(event) => Ok(Some(event)),
                // The sender lives as long as the backend, so `None` only
                // happens after close() dropped everything; report stream
                // end (the session layer maps it to a backend error like a
                // dead CLI process).
                None => Ok(None),
            }
        })
    }

    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.interrupted.store(true, Ordering::Relaxed);
            Ok(())
        })
    }

    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::Relaxed);
            self.interrupted.store(true, Ordering::Relaxed);
            Ok(())
        })
    }

    fn respond_permission<'a>(
        &'a mut self,
        id: &'a str,
        response: PermissionResponse,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            // Permission-gate waiters get the raw decision; interaction
            // waiters (plan approval) get the JSON shape the tool expects.
            let permission = self.permission_waiters.lock().unwrap().remove(id);
            if let Some(waiter) = permission {
                let _ = waiter.send(response);
                return Ok(());
            }
            let interaction = self.interaction_waiters.lock().unwrap().remove(id);
            if let Some(waiter) = interaction {
                let approved = response == PermissionResponse::Approve;
                let _ = waiter.send(serde_json::json!({ "approved": approved }));
            }
            Ok(())
        })
    }

    fn respond_user_input<'a>(
        &'a mut self,
        id: &'a str,
        answers: BTreeMap<String, Vec<String>>,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let waiter = self.interaction_waiters.lock().unwrap().remove(id);
            match waiter {
                Some(waiter) => {
                    let _ = waiter.send(serde_json::json!({ "answers": answers }));
                    Ok(())
                }
                None => Err(Error::UnsupportedCapability(format!(
                    "no pending user input with id {id}"
                ))),
            }
        })
    }
}

/// `SendMode` is accepted by the session layer for interface parity; the
/// engine loop always starts immediately.
impl EngineBackend {
    /// The engine flavor of an interrupt-with-action (session close vs
    /// soft stop). `End` also closes the event channel.
    pub fn apply_interrupt(&mut self, action: crate::session::InterruptAction) {
        match action {
            crate::session::InterruptAction::Continue => {
                self.interrupted.store(true, Ordering::Relaxed);
            }
            crate::session::InterruptAction::End => {
                self.closed.store(true, Ordering::Relaxed);
                self.interrupted.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Unused-import guard for the SendMode parity note above.
#[allow(unused)]
fn _send_mode_parity(_mode: SendMode) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 工具声明面组装回归（2026-11-07 子代理无工具报障）：内置 registry
    /// 必须打底——文件、命令/检索工具缺声明会让会话只剩 Agent/SendMessage，
    /// 子代理拿到任务后无工具可用（dispatch 端 handler 完好，模型幻觉调用
    /// 仍能真执行，报障因此被真机测试掩盖）。重名 extras 不得遮蔽内置。
    #[test]
    fn assembled_registry_includes_builtin_surface_and_dedupes() {
        let extras = vec![
            ToolSpec {
                name: "Read".to_owned(),
                description: "shadow attempt".to_owned(),
                parameters: serde_json::json!({}),
            },
            ToolSpec {
                name: "mcp__srv__ping".to_owned(),
                description: "mcp tool".to_owned(),
                parameters: serde_json::json!({}),
            },
        ];
        let all = assemble_tool_registry(extras, std::iter::empty());
        let names: Vec<&str> = all.iter().map(|spec| spec.name.as_str()).collect();
        for expected in [
            "Bash", "Read", "Write", "Edit", "Glob", "Grep", "TodoWrite", "TodoRead",
            "TaskOutput", "TaskStop", "AskUserQuestion", "EnterPlanMode", "ExitPlanMode",
            "CronCreate", "CronList", "CronUpdate", "CronDelete",
        ] {
            assert!(names.contains(&expected), "内置工具未声明: {expected}");
        }
        assert!(names.contains(&"mcp__srv__ping"), "MCP 工具应并入");
        let read: Vec<&&str> = names.iter().filter(|name| **name == "Read").collect();
        assert_eq!(read.len(), 1, "重名 extras 不得遮蔽内置（Read 恰好一次）");
        let shadowed = all.iter().find(|spec| spec.name == "Read").unwrap();
        assert_ne!(shadowed.description, "shadow attempt");
        // 内置打底在前、Agent/SendMessage 由调用方追加（此处不含）。
        assert!(!names.contains(&"Agent") && !names.contains(&"SendMessage"));
    }
}
