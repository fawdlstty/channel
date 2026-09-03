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
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

struct CliProcess {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

pub(crate) struct CliBackend {
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
}

impl CliBackend {
    pub(crate) fn new_with_config(config: crate::protocol::SessionConfig) -> Result<Self, Error> {
        let kind = config.kind.clone();
        let (mut command, args) = config.backend.cli_command(&kind)?;
        if let Some(executable) = config.runtime.process.executable.as_ref() {
            command = executable.to_string_lossy().into_owned();
        }
        let session_id = match &config.conversation.mode {
            crate::protocol::ConversationSpec::New => None,
            crate::protocol::ConversationSpec::Resume(target) => {
                if !matches!(kind, HarnessKind::ClaudeCode) {
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
        })
    }

    async fn spawn_turn(&mut self, message: String) -> Result<(), Error> {
        self.stop_process().await?;
        let args = self
            .kind
            .turn_args(&self.args, self.session_id.as_deref(), &message);
        let mut child = Command::new(&self.command)
            .args(&args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                Error::Backend(format!(
                    "failed to start CLI command {}: {error}",
                    self.command
                ))
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Backend("CLI stdout is unavailable".to_owned()))?;
        self.process = Some(CliProcess {
            child,
            stdout: BufReader::new(stdout),
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
            {
                let _ = process.child.kill().await;
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
                        self.queued
                            .push_back(Event::Error(format!("CLI exited with status {status}")));
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
}

impl Backend for CliBackend {
    fn id(&self) -> Option<String> {
        self.session_id.clone()
    }
    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
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
                    process.child.kill().await.map_err(|error| {
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
            crate::protocol::BackendSpec::PlainCli { .. } => {
                crate::protocol::BackendKind::PlainCli
            }
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

    pub(crate) fn structured_capabilities() -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            file_reads: true,
            file_writes: true,
            command_execution: true,
            turn_cancel: true,
            session_close: true,
            provider_resume: true,
            raw_events: true,
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) fn plain_capabilities() -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            file_reads: true,
            file_writes: true,
            command_execution: true,
            turn_cancel: true,
            session_close: true,
            raw_events: true,
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) async fn create_session_with_config(
        config: crate::protocol::SessionConfig,
        first_message: String,
        initialized: Option<crate::protocol::HarnessState>,
    ) -> Result<crate::session::Session, Error> {
        let backend = Self::new_with_config(config.clone())?;
        let mut session = crate::session::Session::with_backend_config(
            config,
            None,
            Box::new(backend),
            initialized,
        );
        if !first_message.is_empty() {
            session.send(first_message, SendMode::Immediate).await?;
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
            _ => Err(Error::UnsupportedHarness(self.clone())),
        }
    }
}

impl HarnessKind {
    fn turn_args(&self, base: &[String], session_id: Option<&str>, message: &str) -> Vec<String> {
        if !matches!(self, HarnessKind::ClaudeCode) {
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
            return args;
        }
        let mut args = vec![
            "-p".to_owned(),
            message.to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--include-partial-messages".to_owned(),
        ];
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
        let args = HarnessKind::ClaudeCode.turn_args(&[], Some("session-1"), "你好");
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
            crate::protocol::ResumeTarget::ProviderSession { id: "s-9".to_owned() },
        );
        let backend = CliBackend::new_with_config(config).unwrap();
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
            CliBackend::new_with_config(config),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn fork_and_attach_modes_are_rejected() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::ClaudeCode);
        config.conversation.mode = crate::protocol::ConversationSpec::Fork(
            crate::protocol::ResumeTarget::ProviderSession { id: "s-1".to_owned() },
        );
        assert!(matches!(
            CliBackend::new_with_config(config),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn parses_claude_stream_and_result() {
        let mut backend = CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessKind::ClaudeCode,
        ))
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
        ))
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
        ))
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
}
