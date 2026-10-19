use crate::protocol::{
    Confidence, Error, Event, FileChange, FileChangeKind, FileRead, FileReadSource, Finish,
    HarnessKind, ObservationSource, Status, ToolCall, ToolStatus,
};
use crate::session::{Backend, BackendFuture, SendMode};
use crate::utils::json::JsonValueExt;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

struct CliProcess {
    child: Child,
    stdout: BufReader<ChildStdout>,
    /// stderr tail collector. The structured CLIs keep their real failure
    /// reason (provider business errors, config problems) on stderr; the
    /// tail is surfaced when the process exits nonzero without a terminal
    /// event, instead of dropping it into a null sink.
    stderr_tail: Option<tokio::task::JoinHandle<String>>,
}

impl Drop for CliProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Keeps at most the last [`STDERR_TAIL_BYTES`] of a CLI's stderr.
const STDERR_TAIL_BYTES: usize = 4096;

async fn collect_stderr_tail(
    mut stderr: tokio::process::ChildStderr,
) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > STDERR_TAIL_BYTES * 2 {
                    let excess = buffer.len() - STDERR_TAIL_BYTES;
                    buffer.drain(..excess);
                }
            }
        }
    }
    if buffer.len() > STDERR_TAIL_BYTES {
        let excess = buffer.len() - STDERR_TAIL_BYTES;
        buffer.drain(..excess);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

pub(crate) struct CliBackend {
    backend_kind: crate::protocol::BackendKind,
    kind: HarnessKind,
    command: String,
    args: Vec<String>,
    session_id: Option<String>,
    process: Option<CliProcess>,
    queued: VecDeque<Event>,
    text_seen: bool,
    finished: bool,
    interrupt_requested: bool,
    stream_tools: HashMap<usize, StreamTool>,
    cwd: PathBuf,
    kill_grace_period: Duration,
    /// 渠道切换资源（cc-switch）：ZCode 经 ZCODE_PERSONAL_PROVIDER_CONFIG_FILE
    /// 指向按渠道改写过的 provider 配置副本；持有它保证会话期间副本不被丢弃。
    resources: Option<crate::switch::PreparedResources>,
    /// fully-unrestricted permission mode: forwarded to ZCode as `--mode yolo`.
    full_access: bool,
    /// 权限模式覆盖（ZCode --mode 原样透传，build/edit/plan/yolo）；
    /// 优先于 full_access 推导。
    permission_mode: Option<String>,
    /// Attachment paths for the turn being spawned (consumed by spawn_turn).
    pending_attachments: Vec<String>,
}

impl CliBackend {
    pub(crate) fn new_with_config(
        config: crate::protocol::SessionConfig,
        resources: Option<crate::switch::PreparedResources>,
    ) -> Result<Self, Error> {
        let kind = config.kind.clone();
        let (mut command, args) = config.backend.cli_command(&kind)?;
        if let Some(executable) = config.runtime.process.executable.as_ref() {
            command = executable.to_string_lossy().into_owned();
        }
        let session_id = match &config.conversation.mode {
            crate::protocol::ConversationSpec::New => None,
            crate::protocol::ConversationSpec::Resume(target) => {
                if !matches!(kind, HarnessKind::ClaudeCode | HarnessKind::ZCode) {
                    return Err(Error::UnsupportedCapability(format!(
                        "provider session resume is not supported by the {kind:?} CLI"
                    )));
                }
                target.provider_id().map(str::to_owned)
            }
            crate::protocol::ConversationSpec::Fork(_)
            | crate::protocol::ConversationSpec::Attach(_) => {
                return Err(Error::UnsupportedCapability(
                    "conversation fork and attach are not supported by CLI backends".to_owned(),
                ))
            }
        };
        Ok(Self {
            backend_kind: match config.backend {
                crate::protocol::BackendSpec::PlainCli { .. } => {
                    crate::protocol::BackendKind::PlainCli
                }
                _ => crate::protocol::BackendKind::StructuredCli,
            },
            kind,
            command,
            args,
            session_id,
            process: None,
            queued: VecDeque::new(),
            text_seen: false,
            finished: false,
            interrupt_requested: false,
            stream_tools: HashMap::new(),
            cwd: config.workspace_cwd().to_path_buf(),
            kill_grace_period: config.runtime.process.kill_grace_period,
            full_access: config.is_full_access(),
            permission_mode: config.permission_mode().map(str::to_owned),
            pending_attachments: Vec::new(),
            resources,
        })
    }

    async fn spawn_turn(&mut self, message: String) -> Result<(), Error> {
        self.stop_process().await?;
        let attachments = std::mem::take(&mut self.pending_attachments);
        let args = self.kind.turn_args(
            &self.args,
            self.session_id.as_deref(),
            &message,
            self.full_access,
            self.permission_mode.as_deref(),
            &attachments,
        );
        let mut command = Command::new(&self.command);
        command
            .kill_on_drop(true)
            .args(&args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in self.resources.as_ref().map(|r| r.env()).unwrap_or_default() {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|error| {
            Error::Backend(format!(
                "failed to start CLI command {}: {error}",
                self.command
            ))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Backend("CLI stdout is unavailable".to_owned()))?;
        let stderr_tail = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(collect_stderr_tail(stderr)));
        self.process = Some(CliProcess {
            child,
            stdout: BufReader::new(stdout),
            stderr_tail,
        });
        self.queued.clear();
        self.text_seen = false;
        self.finished = false;
        self.interrupt_requested = false;
        self.stream_tools.clear();
        Ok(())
    }

    async fn stop_process(&mut self) -> Result<(), Error> {
        if let Some(mut process) = self.process.take() {
            if process
                .child
                .try_wait()
                .map_err(|error| Error::Backend(format!("failed to inspect CLI process: {error}")))?
                .is_none()
                && tokio::time::timeout(self.kill_grace_period, process.child.wait())
                    .await
                    .is_err()
            {
                process.child.start_kill().map_err(|error| {
                    Error::Backend(format!("failed to stop CLI process: {error}"))
                })?;
            }
            process
                .child
                .wait()
                .await
                .map_err(|error| Error::Backend(format!("failed to reap CLI process: {error}")))?;
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<Event>, Error> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let process = match self.process.as_mut() {
                Some(process) => process,
                None => return Ok(None),
            };
            let mut line = String::new();
            let bytes =
                process.stdout.read_line(&mut line).await.map_err(|error| {
                    Error::Backend(format!("failed to read CLI output: {error}"))
                })?;
            if bytes == 0 {
                let mut process = self
                    .process
                    .take()
                    .expect("CLI process exists while reading");
                let status = process.child.wait().await.map_err(|error| {
                    Error::Backend(format!("failed to reap CLI process: {error}"))
                })?;
                if !self.finished {
                    if self.interrupt_requested {
                        self.queue_finished(Status::Interrupted);
                    } else if !status.success() {
                        // Nonzero exit without a terminal event: the CLI died
                        // before reporting why. Its stderr (kept providers
                        // print the business error there) is the only source
                        // of the real reason — surface its tail verbatim.
                        let stderr_tail = match process.stderr_tail.take() {
                            Some(handle) => tokio::time::timeout(
                                std::time::Duration::from_secs(2),
                                handle,
                            )
                            .await
                            .ok()
                            .and_then(|result| result.ok())
                            .unwrap_or_default(),
                            None => String::new(),
                        };
                        let mut message = format!("CLI exited with status {status}");
                        let tail = stderr_tail.trim();
                        if !tail.is_empty() {
                            message.push_str(": ");
                            message.push_str(tail);
                        }
                        self.queued.push_back(Event::Error(message));
                        self.queue_finished(Status::Failed);
                    } else {
                        self.queue_finished(Status::Completed);
                    }
                }
                if let Some(event) = self.queued.pop_front() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            let trimmed = line.trim_end();
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                let events = self.parse_json(&value);
                self.queued.extend(events);
            } else {
                if self.backend_kind == crate::protocol::BackendKind::StructuredCli {
                    return Err(Error::ProtocolError(format!(
                        "invalid structured CLI JSON output: {trimmed}"
                    )));
                }
                self.text_seen = true;
                self.queued.push_back(Event::TextDelta(line));
            }
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
        }
    }

    fn parse_json(&mut self, value: &Value) -> Vec<Event> {
        if let Some(id) = value.session_id() {
            self.session_id = Some(id);
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.kind == HarnessKind::ZCode {
            return self.parse_zcode_event(value, kind);
        }
        match kind {
            "system" => Vec::new(),
            "stream_event" => self.parse_stream_event(value),
            "assistant" => {
                let message = value.get("message").unwrap_or(value);
                let mut events = if self.text_seen {
                    Vec::new()
                } else {
                    message.text_events()
                };
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for item in content {
                        if matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("tool_use" | "tool_call")
                        ) {
                            events.extend(item.tool_events());
                        }
                    }
                }
                self.text_seen |= events
                    .iter()
                    .any(|event| matches!(event, Event::TextDelta(_)));
                events
            }
            "result" => {
                let mut events = Vec::new();
                if !self.text_seen {
                    if let Some(text) = value.get("result").and_then(Value::as_str) {
                        self.text_seen = true;
                        events.push(Event::TextDelta(text.to_owned()));
                    }
                }
                let status = if value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    Status::Failed
                } else {
                    match value
                        .get("subtype")
                        .or_else(|| value.get("stop_reason"))
                        .and_then(Value::as_str)
                    {
                        Some("cancelled") | Some("canceled") | Some("interrupted") => {
                            Status::Interrupted
                        }
                        Some("error") | Some("failure") => Status::Failed,
                        _ => Status::Completed,
                    }
                };
                self.finished = true;
                events.push(Event::Finished(Finish::new(status, "")));
                events
            }
            "error" => {
                self.finished = true;
                vec![
                    Event::Error(value.to_string()),
                    Event::Finished(Finish::new(Status::Failed, "")),
                ]
            }
            "tool_use" | "tool_call" | "toolCall" | "tool_result" | "toolResult" | "file_read"
            | "fileRead" => value.tool_events(),
            _ => {
                let mut events = value.text_events();
                if events.is_empty() {
                    if let Some(status) = value.get("status").and_then(CliJson::status) {
                        if status.is_terminal() {
                            self.finished = true;
                            events.push(Event::Finished(Finish::new(status, "")));
                        } else {
                            events.push(Event::Status(status));
                        }
                    } else {
                        events.push(Event::Raw(value.clone()));
                    }
                }
                self.text_seen |= events
                    .iter()
                    .any(|event| matches!(event, Event::TextDelta(_)));
                events
            }
        }
    }

    fn queue_finished(&mut self, status: Status) {
        if !self.finished {
            self.finished = true;
            self.queued
                .push_back(Event::Finished(Finish::new(status, "")));
        }
    }

    /// Parses one NDJSON event emitted by the ZCode headless CLI
    /// (`zcode -p <message> --output-format stream-json`). Each stdout line
    /// carries a top-level `type`; per-turn results arrive as a terminal
    /// `result` line, while provider failures end with `turn.failed` and no
    /// `result`. Unknown event types are ignored: the ZCode event surface is
    /// still evolving.
    fn parse_zcode_event(&mut self, value: &Value, kind: &str) -> Vec<Event> {
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        match kind {
            "model.streaming" => self.parse_zcode_streaming(&payload),
            "turn.failed" => {
                self.finished = true;
                let message = payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("zcode turn failed without an error message")
                    .to_owned();
                vec![
                    Event::Error(message),
                    Event::Finished(Finish::new(Status::Failed, "")),
                ]
            }
            "result" => {
                let mut events = Vec::new();
                // ZCode 在 result 行的 projection 里带上下文占用（token 已用 /
                // 窗口总量），供 UI 展示上下文容量；缺字段或非数值时静默跳过。
                if let Some(used) = value
                    .get("projection")
                    .and_then(|projection| projection.get("contextUsed"))
                    .and_then(Value::as_u64)
                {
                    if let Some(total) = value
                        .get("projection")
                        .and_then(|projection| projection.get("contextWindow"))
                        .and_then(Value::as_u64)
                    {
                        events.push(Event::ContextUsage { used, total });
                    }
                }
                // After a `turn.failed` the turn is already terminal; a late
                // result line must not re-complete it.
                if !self.finished {
                    if !self.text_seen {
                        if let Some(text) = value
                            .get("response")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            self.text_seen = true;
                            events.push(Event::TextDelta(text.to_owned()));
                        }
                    }
                    self.finished = true;
                    events.push(Event::Finished(Finish::new(Status::Completed, "")));
                }
                events
            }
            "session.updated" => {
                // ZCode 投影时 compact_started/completed/failed/boundary 等
                // 内部事件全部落进 session.updated 兜底分支且 payload 原样
                // 透传。只有 compact_boundary 载荷特有的键组（strict zod
                // schema 独有，其他事件 payload 不含，无误判面）才提升为
                // 一等事件；其余维持现状静默忽略。
                if is_zcode_compaction_boundary_payload(&payload) {
                    vec![Event::CompactionBoundary { payload }]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn parse_zcode_streaming(&mut self, payload: &Value) -> Vec<Event> {
        let kind = payload
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "reasoning_delta" => Self::zcode_delta(payload)
                .map(|delta| vec![Event::ReasoningDelta(delta)])
                .unwrap_or_default(),
            "text_delta" => Self::zcode_delta(payload)
                .map(|delta| {
                    self.text_seen = true;
                    vec![Event::TextDelta(delta)]
                })
                .unwrap_or_default(),
            "tool_input_start" => self.zcode_tool_events(payload, "requested"),
            "tool_input_end" => self.zcode_tool_events(payload, "running"),
            "tool_call" => self.zcode_tool_events(payload, "completed"),
            _ => Vec::new(),
        }
    }

    fn zcode_delta(payload: &Value) -> Option<String> {
        payload
            .get("delta")
            .and_then(Value::as_str)
            .filter(|delta| !delta.is_empty())
            .map(str::to_owned)
    }

    fn zcode_tool_events(&self, payload: &Value, status: &'static str) -> Vec<Event> {
        let id = payload
            .get("toolCallId")
            .or_else(|| payload.get("tool_call_id"))
            .and_then(JsonValueExt::string_value);
        let name = payload
            .get("toolName")
            .or_else(|| payload.get("tool_name"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let input = payload
            .get("input")
            .filter(|input| !input.is_null())
            .cloned()
            .unwrap_or(Value::Null);
        let output = payload
            .get("output")
            .or_else(|| payload.get("result"))
            .filter(|output| !output.is_null());
        ToolEvent::new(id, name, input, status, output, false).events()
    }
}

/// `compact_boundary` 载荷键组判定（对齐 ZCode contracts 的 strict zod
/// schema）：payload 为 object 且四条同时满足——`boundaryId` 非空 string、
/// `summaryMessageIds` 为 array、`preCompactTokenCount` 为 number、
/// `traceId` 非空 string。该键组为 compact_boundary 载荷独有，其他
/// session.updated 载荷（普通状态快照等）不含，无误判面。
fn is_zcode_compaction_boundary_payload(payload: &Value) -> bool {
    payload.is_object()
        && payload
            .get("boundaryId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && payload
            .get("summaryMessageIds")
            .is_some_and(Value::is_array)
        && payload
            .get("preCompactTokenCount")
            .is_some_and(Value::is_number)
        && payload
            .get("traceId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
}

impl Backend for CliBackend {
    fn id(&self) -> Option<String> {
        self.session_id.clone()
    }
    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.spawn_turn(message).await })
    }

    fn send_with_attachments<'a>(
        &'a mut self,
        message: String,
        attachments: &[String],
    ) -> BackendFuture<'a, ()> {
        // Attachments ride on the next spawned turn; the structured CLI backends
        // that understand them (ZCode) take `--attach <path>` per file.
        self.pending_attachments = attachments.to_vec();
        Box::pin(async move { self.spawn_turn(message).await })
    }

    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
        Box::pin(async move { self.next().await })
    }

    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.interrupt_requested = true;
            if let Some(process) = self.process.as_mut() {
                if process
                    .child
                    .try_wait()
                    .map_err(|error| {
                        Error::Backend(format!("failed to inspect CLI process: {error}"))
                    })?
                    .is_none()
                {
                    process.child.start_kill().map_err(|error| {
                        Error::Backend(format!("failed to interrupt CLI process: {error}"))
                    })?;
                }
            }
            Ok(())
        })
    }

    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.stop_process().await })
    }
}

impl CliBackend {
    pub(crate) fn initialize(
        kind: &HarnessKind,
        backend: &crate::protocol::BackendSpec,
        init: &crate::protocol::HarnessInit,
    ) -> Result<crate::runtime::HarnessDiscovery, Error> {
        if init.port.is_some() || !matches!(init.endpoint, crate::protocol::EndpointRequest::Auto) {
            return Err(Error::InvalidConfig(
                "the selected CLI harness only supports managed stdio connections".to_owned(),
            ));
        }
        let cwd = crate::runtime::HarnessDiscovery::resolve_cwd(init.cwd.as_deref())?;
        let (command, args) = backend.cli_command(kind)?;
        let backend_kind = match backend {
            crate::protocol::BackendSpec::PlainCli { .. } => crate::protocol::BackendKind::PlainCli,
            _ => crate::protocol::BackendKind::StructuredCli,
        };
        let executable = crate::runtime::HarnessDiscovery::resolve_executable(
            init.executable.as_deref(),
            &command,
            Some(&cwd),
        )?;
        Ok(crate::runtime::HarnessDiscovery::discover(
            kind,
            backend_kind,
            &command,
            &args,
            executable,
            crate::protocol::TransportKind::Stdio,
            None,
        ))
    }

    pub(crate) fn structured_capabilities(kind: &HarnessKind) -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            // The ZCode headless CLI streams `reasoning_delta` events.
            reasoning_summary: matches!(kind, HarnessKind::ZCode),
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            file_reads: true,
            file_writes: true,
            command_execution: true,
            turn_cancel: true,
            session_close: true,
            // Both CLIs resume via `--resume <sessionId>`.
            provider_resume: matches!(kind, HarnessKind::ClaudeCode | HarnessKind::ZCode),
            raw_events: true,
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) fn plain_capabilities() -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            turn_cancel: true,
            session_close: true,
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) async fn create_session_with_config(
        config: crate::protocol::SessionConfig,
        first_message: String,
        attachments: Vec<String>,
        initialized: Option<crate::protocol::HarnessState>,
    ) -> Result<crate::session::Session, Error> {
        let resources = crate::protocol::SwitchProvider::CcSwitch
            .prepare_resources(&config)
            .await?;
        let backend = Self::new_with_config(config.clone(), resources)?;
        let mut session = crate::session::Session::with_backend_config(
            config,
            None,
            Box::new(backend),
            initialized,
        );
        if !first_message.is_empty() {
            session
                .send_with_attachments(first_message, attachments, SendMode::Immediate)
                .await?;
        }
        Ok(session)
    }
}

impl crate::protocol::BackendSpec {
    fn cli_command(&self, kind: &HarnessKind) -> Result<(String, Vec<String>), Error> {
        match self {
            crate::protocol::BackendSpec::StructuredCli { command, args }
            | crate::protocol::BackendSpec::PlainCli { command, args } => {
                Ok((command.clone(), args.clone()))
            }
            crate::protocol::BackendSpec::Auto => kind.cli_command(),
            crate::protocol::BackendSpec::CodexAppServer { .. }
            | crate::protocol::BackendSpec::Acp { .. } => Err(Error::InvalidConfig(
                "CLI backend cannot use an ACP or Codex App Server specification".to_owned(),
            )),
        }
    }
}

impl HarnessKind {
    fn cli_command(&self) -> Result<(String, Vec<String>), Error> {
        match self {
            HarnessKind::ClaudeCode => Ok(("claude".to_owned(), Vec::new())),
            HarnessKind::ZCode => Ok(("zcode".to_owned(), Vec::new())),
            _ => Err(Error::UnsupportedHarness(self.clone())),
        }
    }
}

impl HarnessKind {
    fn turn_args(
        &self,
        base: &[String],
        session_id: Option<&str>,
        message: &str,
        full_access: bool,
        mode_override: Option<&str>,
        attachments: &[String],
    ) -> Vec<String> {
        if !matches!(self, HarnessKind::ClaudeCode | HarnessKind::ZCode) {
            return base.to_vec();
        }
        if !base.is_empty() {
            let mut args: Vec<String> = base
                .iter()
                .map(|arg| {
                    arg.replace("{message}", message)
                        .replace("{session_id}", session_id.unwrap_or_default())
                })
                .collect();
            if !base.iter().any(|arg| arg.contains("{message}")) {
                args.push(message.to_owned());
            }
            for path in attachments {
                args.extend(["--attach".to_owned(), path.clone()]);
            }
            return args;
        }
        let mut args = if matches!(self, HarnessKind::ZCode) {
            vec![
                "-p".to_owned(),
                message.to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ]
        } else {
            vec![
                "-p".to_owned(),
                message.to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
                "--verbose".to_owned(),
                "--include-partial-messages".to_owned(),
            ]
        };
        if matches!(self, HarnessKind::ZCode) {
            // ZCode 的「完全访问」= yolo 权限模式（减少确认次数）。注意 zcode
            // 对 `-p` 默认即 yolo，关闭时必须显式给 `--mode build` 才会回到标准
            // 模式。permission_mode 覆盖优先（edit = 自动编辑档）。
            let mode = mode_override
                .map(str::to_owned)
                .unwrap_or_else(|| (if full_access { "yolo" } else { "build" }).to_owned());
            args.extend(["--mode".to_owned(), mode]);
        }
        for path in attachments {
            args.extend(["--attach".to_owned(), path.clone()]);
        }
        if let Some(id) = session_id {
            args.extend(["--resume".to_owned(), id.to_owned()]);
        }
        args
    }
}

trait CliJson {
    fn session_id(&self) -> Option<String>;
    fn text_events(&self) -> Vec<Event>;
    fn status(&self) -> Option<Status>;
    fn tool_events(&self) -> Vec<Event>;
}

impl CliBackend {
    fn parse_stream_event(&mut self, value: &Value) -> Vec<Event> {
        let event = value.get("event").unwrap_or(value);
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "content_block_delta" => {
                let delta = event.get("delta").unwrap_or(event);
                if delta
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "input_json_delta")
                {
                    return self.stream_tool_delta(event, delta);
                }
                let events = delta.text_events();
                self.text_seen |= events
                    .iter()
                    .any(|event| matches!(event, Event::TextDelta(_)));
                events
            }
            "content_block_start" => {
                let block = event.get("content_block").unwrap_or(event);
                let block_kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(block_kind, "tool_use" | "tool_call") {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let tool = StreamTool {
                        id: block.get("id").and_then(JsonValueExt::string_value),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_owned(),
                        input: block
                            .get("input")
                            .cloned()
                            .unwrap_or(Value::Object(Map::new())),
                    };
                    let result = ToolEvent::new(
                        tool.id.clone(),
                        tool.name.clone(),
                        tool.input.clone(),
                        "requested",
                        None,
                        false,
                    )
                    .events();
                    self.stream_tools.insert(index, tool);
                    result
                } else {
                    block.text_events()
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.stream_tools
                    .remove(&index)
                    .map(|tool| {
                        ToolEvent::new(tool.id, tool.name, tool.input, "completed", None, false)
                            .events()
                    })
                    .unwrap_or_default()
            }
            "message_start" | "message_stop" => Vec::new(),
            _ => vec![Event::Raw(value.clone())],
        }
    }

    fn stream_tool_delta(&mut self, event: &Value, delta: &Value) -> Vec<Event> {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let Some(tool) = self.stream_tools.get_mut(&index) else {
            return vec![Event::Raw(event.clone())];
        };
        let Some(partial) = delta.get("partial_json").and_then(Value::as_str) else {
            return vec![Event::Raw(event.clone())];
        };
        if let Some(existing) = tool.input.as_str() {
            tool.input = Value::String(format!("{existing}{partial}"));
        } else {
            tool.input = Value::String(partial.to_owned());
        }
        let input = tool
            .input
            .as_str()
            .and_then(|input| serde_json::from_str(input).ok())
            .unwrap_or_else(|| tool.input.clone());
        ToolEvent::new(
            tool.id.clone(),
            tool.name.clone(),
            input,
            "running",
            None,
            false,
        )
        .events()
    }
}

#[derive(Debug)]
struct StreamTool {
    id: Option<String>,
    name: String,
    input: Value,
}

struct ToolEvent {
    id: Option<String>,
    name: String,
    input: Value,
    status: &'static str,
    result: Option<Value>,
    explicit_file: bool,
}

impl ToolEvent {
    fn new(
        id: Option<String>,
        name: String,
        input: Value,
        status: &'static str,
        result: Option<&Value>,
        explicit_file: bool,
    ) -> Self {
        Self {
            id,
            name,
            input,
            status,
            result: result.cloned(),
            explicit_file,
        }
    }
}

impl ToolEvent {
    fn events(self) -> Vec<Event> {
        let status = self.status.tool_status();
        let mut events = vec![Event::ToolCall(ToolCall {
            id: self.id.clone(),
            name: self.name.clone(),
            status,
            input: (!self.input.is_null()).then(|| self.input.value_text()),
            output: self.result.as_ref().map(|result| result.value_text()),
            error: None,
            sequence: 0,
        })];
        let path = self
            .input
            .file_path()
            .or_else(|| self.input.inferred_command_path(&self.name));
        if self.name.is_file_write_tool() {
            if let Some(path) = path {
                events.push(Event::FileChange(FileChange {
                    path,
                    kind: self.name.file_change_kind(),
                    tool_id: self.id,
                    status,
                    diff_summary: self.result.map(|result| result.value_text()),
                    source: ObservationSource::ProcessInspection,
                    confidence: Confidence::Medium,
                    sequence: 0,
                }));
            }
        } else if let Some(path) = path {
            events.push(Event::FileRead(FileRead {
                path,
                tool_id: self.id,
                status,
                line_start: self
                    .input
                    .line_number(&["line_start", "start_line", "startLine"]),
                line_end: self.input.line_number(&["line_end", "end_line", "endLine"]),
                summary: self.result.map(|result| result.value_text()),
                source: if self.explicit_file {
                    FileReadSource::Protocol
                } else {
                    FileReadSource::ToolInput
                },
                confidence: if self.explicit_file {
                    Confidence::High
                } else {
                    Confidence::Medium
                },
                sequence: 0,
            }));
        }
        events
    }
}

trait CliToolName {
    fn is_file_write_tool(&self) -> bool;
    fn file_change_kind(&self) -> FileChangeKind;
    fn tool_status(&self) -> ToolStatus;
}

impl CliToolName for str {
    fn is_file_write_tool(&self) -> bool {
        let lower = self.to_ascii_lowercase();
        lower == "write"
            || lower == "edit"
            || lower == "multiedit"
            || lower == "notebookedit"
            || lower == "file_write"
            || lower == "filewrite"
    }

    fn file_change_kind(&self) -> FileChangeKind {
        let lower = self.to_ascii_lowercase();
        if lower == "write" || lower == "file_write" || lower == "filewrite" {
            FileChangeKind::Created
        } else {
            FileChangeKind::Modified
        }
    }

    fn tool_status(&self) -> ToolStatus {
        match self {
            "requested" => ToolStatus::Requested,
            "running" => ToolStatus::Running,
            "completed" => ToolStatus::Completed,
            "failed" => ToolStatus::Failed,
            "cancelled" => ToolStatus::Cancelled,
            _ => ToolStatus::Unknown,
        }
    }
}

impl CliJson for Value {
    fn tool_events(&self) -> Vec<Event> {
        let kind = self.get("type").and_then(Value::as_str).unwrap_or_default();
        let (id, name, input, status, result, explicit_file) = match kind {
            "tool_use" | "tool_call" | "toolCall" => (
                self.get("id")
                    .or_else(|| self.get("tool_call_id"))
                    .and_then(JsonValueExt::string_value),
                self.get("name")
                    .or_else(|| self.get("tool_name"))
                    .or_else(|| self.get("toolName"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                self.get("input")
                    .or_else(|| self.get("arguments"))
                    .or_else(|| self.get("args"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "requested",
                None,
                false,
            ),
            "tool_result" | "toolResult" => {
                let failed = self
                    .get("is_error")
                    .or_else(|| self.get("isError"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (
                    self.get("tool_use_id")
                        .or_else(|| self.get("tool_call_id"))
                        .or_else(|| self.get("id"))
                        .and_then(JsonValueExt::string_value),
                    self.get("name")
                        .or_else(|| self.get("tool_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    self.get("input").cloned().unwrap_or(Value::Null),
                    if failed { "failed" } else { "completed" },
                    self.get("output")
                        .or_else(|| self.get("content"))
                        .or_else(|| self.get("result")),
                    false,
                )
            }
            "file_read" | "fileRead" => (
                self.get("id").and_then(JsonValueExt::string_value),
                "file_read".to_owned(),
                self.get("input")
                    .or_else(|| self.get("path").map(|_| self))
                    .cloned()
                    .unwrap_or(Value::Null),
                "completed",
                self.get("output").or_else(|| self.get("content")),
                true,
            ),
            _ => return Vec::new(),
        };
        ToolEvent::new(id, name, input, status, result, explicit_file).events()
    }

    fn text_events(&self) -> Vec<Event> {
        if let Some(thinking) = self.get("thinking").and_then(Value::as_str) {
            return vec![Event::ReasoningDelta(thinking.to_owned())];
        }
        if let Some(text) = self.get("text").and_then(Value::as_str) {
            return vec![Event::TextDelta(text.to_owned())];
        }
        if let Some(delta) = self.get("delta").and_then(Value::as_str) {
            return vec![Event::TextDelta(delta.to_owned())];
        }
        let Some(content) = self.get("content").and_then(Value::as_array) else {
            return Vec::new();
        };
        content
            .iter()
            .filter_map(|item| {
                let text = item.get("text").and_then(Value::as_str)?;
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("text");
                Some(if kind.contains("reason") || kind == "thinking" {
                    Event::ReasoningDelta(text.to_owned())
                } else {
                    Event::TextDelta(text.to_owned())
                })
            })
            .collect()
    }

    fn status(&self) -> Option<Status> {
        self.as_str().and_then(CliStatus::status).or_else(|| {
            self.get("type")
                .and_then(Value::as_str)
                .and_then(CliStatus::status)
        })
    }

    fn session_id(&self) -> Option<String> {
        self.get("session_id")
            .or_else(|| self.get("sessionId"))
            .or_else(|| self.get("session").and_then(|session| session.get("id")))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

trait CliStatus {
    fn status(&self) -> Option<Status>;
}

impl CliStatus for str {
    fn status(&self) -> Option<Status> {
        Some(match self {
            "running" | "working" | "in_progress" => Status::Running,
            "completed" | "complete" | "idle" => Status::Completed,
            "cancelled" | "canceled" | "interrupted" => Status::Interrupted,
            "failed" | "error" => Status::Failed,
            "waitingForPermission" | "waiting_for_permission" => Status::WaitingForPermission,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_args_include_resume_and_streaming() {
        let args = HarnessKind::ClaudeCode.turn_args(&[], Some("session-1"), "你好", false, None, &[]);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--resume", "session-1"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"]));
    }

    #[test]
    fn resume_mode_seeds_the_claude_session_id() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::ClaudeCode);
        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::ProviderSession {
                id: "s-9".to_owned(),
            },
        );
        let backend = CliBackend::new_with_config(config, None).unwrap();
        assert_eq!(backend.session_id.as_deref(), Some("s-9"));
        assert_eq!(backend.id().as_deref(), Some("s-9"));
    }

    #[test]
    fn resume_mode_is_rejected_by_unsupported_cli_kinds() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.backend = crate::protocol::BackendSpec::StructuredCli {
            command: "codex".to_owned(),
            args: Vec::new(),
        };
        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::ChannelDefault("s-1".to_owned()),
        );
        assert!(matches!(
            CliBackend::new_with_config(config, None),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn fork_and_attach_modes_are_rejected() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::ClaudeCode);
        config.conversation.mode = crate::protocol::ConversationSpec::Fork(
            crate::protocol::ResumeTarget::ProviderSession {
                id: "s-1".to_owned(),
            },
        );
        assert!(matches!(
            CliBackend::new_with_config(config, None),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn parses_claude_stream_and_result() {
        let mut backend = CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessKind::ClaudeCode,
        ), None)
        .unwrap();
        let stream = serde_json::json!({
            "type": "stream_event",
            "event": { "type": "content_block_delta", "delta": { "type": "text_delta", "text": "ok" } }
        });
        assert_eq!(
            backend.parse_json(&stream),
            vec![Event::TextDelta("ok".to_owned())]
        );
        let result =
            serde_json::json!({ "type": "result", "subtype": "success", "session_id": "s-1" });
        assert_eq!(
            backend.parse_json(&result),
            vec![Event::Finished(Finish::new(Status::Completed, ""))]
        );
        assert_eq!(backend.session_id.as_deref(), Some("s-1"));
    }

    #[test]
    fn parses_structured_tool_and_file_read_events() {
        let mut backend = CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessKind::ClaudeCode,
        ), None)
        .unwrap();
        let events = backend.parse_json(&serde_json::json!({
            "type": "tool_use",
            "id": "tool-1",
            "name": "Read",
            "input": { "file_path": "src/lib.rs" }
        }));
        assert!(matches!(&events[0], Event::ToolCall(call)
            if call.id.as_deref() == Some("tool-1") && call.name == "Read"));
        assert!(matches!(&events[1], Event::FileRead(file)
            if file.path == "src/lib.rs"
                && file.source == FileReadSource::ToolInput
                && file.confidence == Confidence::Medium));
    }

    #[test]
    fn preserves_plain_text_and_unknown_json_as_fallbacks() {
        let mut backend = CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessKind::ClaudeCode,
        ), None)
        .unwrap();
        assert_eq!(
            backend.parse_json(&serde_json::json!({ "type": "future_event" })),
            vec![Event::Raw(serde_json::json!({ "type": "future_event" }))]
        );
        assert!(matches!(
            backend.parse_stream_event(&serde_json::json!({
                "type": "stream_event",
                "event": { "type": "future_block" }
            }))[0],
            Event::Raw(_)
        ));
    }

    fn zcode_backend() -> CliBackend {
        CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessKind::ZCode,
        ), None)
        .unwrap()
    }

    #[test]
    fn zcode_turn_args_use_headless_stream_json_and_resume() {
        // zcode 对 -p 默认即 yolo，标准模式必须显式 --mode build 才会生效。
        let args = HarnessKind::ZCode.turn_args(&[], None, "你好", false, None, &[]);
        assert_eq!(
            args,
            vec![
                "-p".to_owned(),
                "你好".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
                "--mode".to_owned(),
                "build".to_owned()
            ]
        );
        let full_access_args = HarnessKind::ZCode.turn_args(&[], None, "你好", true, None, &[]);
        assert!(full_access_args
            .windows(2)
            .any(|pair| pair == ["--mode", "yolo"]));
        let resumed = HarnessKind::ZCode.turn_args(&[], Some("sess-1"), "继续", false, None, &[]);
        assert!(resumed
            .windows(2)
            .any(|pair| pair == ["--resume", "sess-1"]));
        assert!(!resumed.contains(&"--verbose".to_owned()));
    }

    #[test]
    fn zcode_turn_args_permission_mode_override_wins() {
        // permission_mode 覆盖优先于 full_access 推导（edit = 自动编辑档）。
        let args = HarnessKind::ZCode.turn_args(&[], None, "你好", true, Some("edit"), &[]);
        assert!(args.windows(2).any(|pair| pair == ["--mode", "edit"]));
        assert!(!args
            .windows(2)
            .any(|pair| pair == ["--mode", "yolo"]));
    }

    #[test]
    fn zcode_turn_args_carry_attachments() {
        let args = HarnessKind::ZCode.turn_args(
            &[],
            None,
            "看图",
            false,
            None,
            &["/tmp/a.png".to_owned(), "/tmp/b.jpg".to_owned()],
        );
        assert!(args.windows(2).any(|pair| pair == ["--attach", "/tmp/a.png"]));
        assert!(args.windows(2).any(|pair| pair == ["--attach", "/tmp/b.jpg"]));
    }

    #[test]
    fn zcode_parses_reasoning_and_text_deltas() {
        let mut backend = zcode_backend();
        let reasoning = serde_json::json!({
            "type": "model.streaming",
            "payload": { "kind": "reasoning_delta", "delta": "The" }
        });
        assert_eq!(
            backend.parse_json(&reasoning),
            vec![Event::ReasoningDelta("The".to_owned())]
        );
        let text = serde_json::json!({
            "type": "model.streaming",
            "payload": { "kind": "text_delta", "delta": "答案" }
        });
        assert_eq!(
            backend.parse_json(&text),
            vec![Event::TextDelta("答案".to_owned())]
        );
        assert!(backend.text_seen);
        // Empty deltas and lifecycle kinds are ignored.
        for payload in [
            serde_json::json!({ "kind": "text_delta", "delta": "" }),
            serde_json::json!({ "kind": "reasoning_delta", "delta": "" }),
            serde_json::json!({ "kind": "start" }),
            serde_json::json!({ "kind": "reasoning_start" }),
            serde_json::json!({ "kind": "text_end" }),
            serde_json::json!({ "kind": "finish" }),
            serde_json::json!({ "kind": "tool_input_delta", "partial": "{}" }),
        ] {
            assert_eq!(
                backend.parse_json(&serde_json::json!({
                    "type": "model.streaming",
                    "payload": payload
                })),
                Vec::new()
            );
        }
    }

    #[test]
    fn zcode_tool_streaming_maps_to_tool_calls() {
        let mut backend = zcode_backend();
        let events = backend.parse_json(&serde_json::json!({
            "type": "model.streaming",
            "payload": {
                "kind": "tool_input_start",
                "toolCallId": "tc-1",
                "toolName": "Read",
            }
        }));
        assert!(matches!(&events[0], Event::ToolCall(call)
            if call.id.as_deref() == Some("tc-1")
                && call.name == "Read"
                && call.status == ToolStatus::Requested));
        let events = backend.parse_json(&serde_json::json!({
            "type": "model.streaming",
            "payload": {
                "kind": "tool_call",
                "toolCallId": "tc-1",
                "toolName": "Read",
                "input": { "file_path": "src/lib.rs" }
            }
        }));
        assert!(matches!(&events[0], Event::ToolCall(call)
            if call.status == ToolStatus::Completed && call.input.is_some()));
        assert!(matches!(&events[1], Event::FileRead(file) if file.path == "src/lib.rs"));
    }

    #[test]
    fn zcode_result_line_completes_and_extracts_session_id() {
        let mut backend = zcode_backend();
        let events = backend.parse_json(&serde_json::json!({
            "type": "result",
            "sessionId": "sess_9",
            "response": "完整回答",
            "usage": {}
        }));
        assert_eq!(
            events,
            vec![
                Event::TextDelta("完整回答".to_owned()),
                Event::Finished(Finish::new(Status::Completed, "")),
            ]
        );
        assert!(backend.finished);
        assert_eq!(backend.session_id.as_deref(), Some("sess_9"));

        // A streamed turn falls back to the result only when no delta was seen.
        let mut backend = zcode_backend();
        backend.parse_json(&serde_json::json!({
            "type": "model.streaming",
            "payload": { "kind": "text_delta", "delta": "流式" }
        }));
        let events = backend.parse_json(&serde_json::json!({
            "type": "result",
            "sessionId": "sess_10",
            "response": "完整回答"
        }));
        assert_eq!(
            events,
            vec![Event::Finished(Finish::new(Status::Completed, ""))]
        );
    }

    #[test]
    fn zcode_turn_failed_reports_error_and_failed_terminal() {
        let mut backend = zcode_backend();
        let events = backend.parse_json(&serde_json::json!({
            "type": "turn.failed",
            "payload": {
                "error": { "message": "[1309] 套餐到期", "code": "1309" },
                "turnPhase": "processing_input"
            }
        }));
        assert_eq!(
            events,
            vec![
                Event::Error("[1309] 套餐到期".to_owned()),
                Event::Finished(Finish::new(Status::Failed, "")),
            ]
        );
        assert!(backend.finished);
        // A trailing result line after a failure must not re-complete the turn.
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "result", "sessionId": "sess_11", "response": "late"
            })),
            Vec::new()
        );
    }

    #[test]
    fn zcode_ignores_lifecycle_and_unknown_event_types() {
        let mut backend = zcode_backend();
        for kind in [
            "turn.started",
            "turn.completed",
            "session.created",
            "session.resumed",
            "session.titleUpdated",
            "session.updated",
            "message.upserted",
            "totally_unknown",
        ] {
            assert_eq!(
                backend.parse_json(&serde_json::json!({
                    "type": kind,
                    "sessionId": "sess_1",
                    "payload": {}
                })),
                Vec::new()
            );
        }
        // Session-id extraction still works for ignored events.
        assert_eq!(backend.session_id.as_deref(), Some("sess_1"));
    }

    /// stream-json `session.updated` 行实拍样本：payload 为 ZCode
    /// CompactBoundaryPayload（compact_boundary 事件经 catch-all 透传）。
    #[test]
    fn zcode_compaction_boundary_payload_becomes_a_first_class_event() {
        let mut backend = zcode_backend();
        let payload = serde_json::json!({
            "boundaryId": "compact_7f3a2c",
            "trigger": "auto",
            "phase": "mid_turn",
            "compactReason": "context_limit",
            "summarySource": "model",
            "preCompactTokenCount": 155320,
            "postCompactTokenCount": 21330,
            "truePostCompactTokenCount": 22470,
            "autoCompactThreshold": 166000,
            "willRetriggerNextTurn": false,
            "summarizedMessageCount": 42,
            "keptMessageCount": 2,
            "lastSummarizedMessageId": "msg_41",
            "preservedSegment": {
                "headMessageId": "msg_40",
                "anchorMessageId": "msg_40",
                "tailMessageId": "msg_41"
            },
            "summaryMessageIds": ["msg_42"],
            "customInstructions": false,
            "traceId": "trace_9f2",
            "turnId": "turn_77"
        });
        let events = backend.parse_json(&serde_json::json!({
            "type": "session.updated",
            "sessionId": "sess_cb",
            "eventId": "evt_1",
            "payload": payload,
        }));
        assert_eq!(
            events,
            vec![Event::CompactionBoundary { payload: payload.clone() }],
            "boundary 键组命中 → 提升为一等事件，payload 原样透传"
        );
        assert_eq!(backend.session_id.as_deref(), Some("sess_cb"));
    }

    /// 非 boundary 载荷（普通状态快照 / 键组不全 / 非 object）维持忽略，
    /// 既有 session.updated 静默语义不变。
    #[test]
    fn zcode_session_updated_without_boundary_key_group_stays_ignored() {
        let mut backend = zcode_backend();
        // 空 payload（既有忽略语义）
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "session.updated", "sessionId": "s", "payload": {}
            })),
            Vec::new()
        );
        // 普通状态快照：无 boundaryId / traceId
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "session.updated", "sessionId": "s",
                "payload": {"status": "running", "turns": 3}
            })),
            Vec::new()
        );
        // 键组不全：缺 summaryMessageIds 数组与 number 型 token 计数
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "session.updated", "sessionId": "s",
                "payload": {"boundaryId": "compact_x", "traceId": "tr"}
            })),
            Vec::new()
        );
        // payload 非 object
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "session.updated", "sessionId": "s", "payload": "text"
            })),
            Vec::new()
        );
        // 无 payload 行
        assert_eq!(
            backend.parse_json(&serde_json::json!({
                "type": "session.updated", "sessionId": "s"
            })),
            Vec::new()
        );
    }
}
