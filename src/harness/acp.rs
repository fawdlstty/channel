use crate::protocol::{
    Confidence, Error, Event, FileRead, FileReadSource, Finish, HarnessKind, Status, ToolCall,
    ToolStatus,
};
use crate::runtime::HarnessDiscovery;
use crate::session::{Backend, BackendFuture, SendMode};
use crate::utils::json::JsonValueExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct AcpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AcpProcess {
    async fn spawn_with_cwd(
        command: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let mut command_builder = Command::new(command);
        command_builder.args(args);
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        let mut child = command_builder
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                Error::Backend(format!("failed to start ACP command {command}: {error}"))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Backend("ACP stdin is unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Backend("ACP stdout is unavailable".to_owned()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn write(&mut self, message: &Value) -> Result<(), Error> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| Error::Backend(format!("failed to encode ACP message: {error}")))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| Error::Backend(format!("failed to write ACP message: {error}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| Error::Backend(format!("failed to flush ACP message: {error}")))
    }

    async fn read(&mut self) -> Result<Option<Value>, Error> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|error| Error::Backend(format!("failed to read ACP message: {error}")))?;
        if bytes == 0 {
            return Ok(None);
        }
        serde_json::from_str(line.trim_end())
            .map(Some)
            .map_err(|error| Error::Backend(format!("invalid ACP JSON message: {error}")))
    }

    async fn stop(&mut self) -> Result<(), Error> {
        if self
            .child
            .try_wait()
            .map_err(|error| Error::Backend(format!("failed to inspect ACP process: {error}")))?
            .is_none()
        {
            let _ = self.child.kill().await;
        }
        self.child
            .wait()
            .await
            .map_err(|error| Error::Backend(format!("failed to reap ACP process: {error}")))?;
        Ok(())
    }
}

pub(crate) struct AcpBackend {
    process: AcpProcess,
    session_id: String,
    next_request_id: u64,
    prompt_id: Option<u64>,
    cancel_requested: bool,
    finished: bool,
    queued: VecDeque<Event>,
    protocol_version: Option<String>,
    server_name: Option<String>,
}

impl AcpBackend {
    pub(crate) async fn connect_with_cwd(
        command: String,
        args: Vec<String>,
        cwd: Option<&std::path::Path>,
        executable: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let command = executable
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(command);
        let mut backend = Self {
            process: AcpProcess::spawn_with_cwd(&command, &args, cwd).await?,
            session_id: String::new(),
            next_request_id: 1,
            prompt_id: None,
            cancel_requested: false,
            finished: false,
            queued: VecDeque::new(),
            protocol_version: None,
            server_name: None,
        };

        let initialize_result = backend
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "channel",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        backend.protocol_version = initialize_result
            .get("protocolVersion")
            .or_else(|| initialize_result.get("protocol_version"))
            .and_then(AcpValue::protocol_version_string);
        backend.server_name = initialize_result
            .get("serverInfo")
            .or_else(|| initialize_result.get("server_info"))
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let cwd = cwd
            .map(|path| path.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
        let result = backend
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        backend.session_id = result
            .get("sessionId")
            .or_else(|| result.get("session_id"))
            .or_else(|| result.get("session").and_then(|session| session.get("id")))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Backend("ACP session/new returned no session id".to_owned()))?
            .to_owned();
        Ok(backend)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.process
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;

        loop {
            let message =
                self.process.read().await?.ok_or_else(|| {
                    Error::Backend(format!("ACP ended before completing {method}"))
                })?;
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queue_update(&message);
                }
                continue;
            }
            if message.get("id") == Some(&json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(Error::Backend(format!("ACP RPC {method} failed: {error}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.queued.push_back(Event::Raw(message));
        }
    }

    async fn handle_server_request(&mut self, message: &Value, method: &str) -> Result<(), Error> {
        let id = message
            .get("id")
            .cloned()
            .ok_or_else(|| Error::Backend("ACP server request has no id".to_owned()))?;
        self.queued.push_back(Event::PermissionRequired {
            id: id.to_string(),
            detail: message
                .get("params")
                .map(Value::to_string)
                .unwrap_or_else(|| method.to_owned()),
        });
        let response = if method.contains("permission") || method.contains("approval") {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "outcome": { "outcome": "cancelled" } }
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "request is not supported by channel" }
            })
        };
        self.process.write(&response).await
    }

    fn queue_update(&mut self, message: &Value) {
        for event in message.parse_update() {
            if let Event::Status(Status::Completed) = event {
                self.queue_finished(Status::Completed);
            } else if let Event::Status(Status::Interrupted) = event {
                self.queue_finished(Status::Interrupted);
            } else {
                self.queued.push_back(event);
            }
        }
    }

    fn queue_finished(&mut self, status: Status) {
        if !self.finished {
            self.finished = true;
            self.prompt_id = None;
            self.queued
                .push_back(Event::Finished(Finish::new(status, "")));
        }
    }

    async fn next(&mut self) -> Result<Option<Event>, Error> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let message = match self.process.read().await? {
                Some(message) => message,
                None => return Ok(None),
            };
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queue_update(&message);
                }
            } else if message.get("id") == self.prompt_id.map(|id| json!(id)).as_ref() {
                if let Some(error) = message.get("error") {
                    self.queued
                        .push_back(Event::Error(format!("ACP prompt failed: {error}")));
                    self.queue_finished(Status::Failed);
                } else {
                    let result = message.get("result").cloned().unwrap_or(Value::Null);
                    let status = result
                        .get("stopReason")
                        .or_else(|| result.get("stop_reason"))
                        .and_then(Value::as_str)
                        .map(WireStatus::stop_reason_status)
                        .unwrap_or_else(|| {
                            if self.cancel_requested {
                                Status::Interrupted
                            } else {
                                Status::Completed
                            }
                        });
                    self.queue_finished(status);
                }
            } else {
                self.queued.push_back(Event::Raw(message));
            }
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
        }
    }
}

impl Backend for AcpBackend {
    fn id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }
    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.cancel_requested = false;
            self.finished = false;
            self.queued.clear();
            let id = self.next_request_id;
            self.next_request_id += 1;
            self.prompt_id = Some(id);
            self.process
                .write(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "session/prompt",
                    "params": {
                        "sessionId": self.session_id,
                        "prompt": [{ "type": "text", "text": message }]
                    }
                }))
                .await
        })
    }

    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
        Box::pin(async move { self.next().await })
    }

    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.cancel_requested = true;
            self.process
                .write(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": { "sessionId": self.session_id }
                }))
                .await
        })
    }

    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let _ = self
                .request("session/close", json!({ "sessionId": self.session_id }))
                .await;
            self.process.stop().await
        })
    }
}

pub(crate) struct AcpInitialization {
    pub command: String,
    pub args: Vec<String>,
    pub executable: PathBuf,
    pub protocol_version: Option<String>,
    pub server_name: Option<String>,
}

impl AcpBackend {
    pub(crate) async fn initialize(
        kind: &HarnessKind,
        backend: &crate::protocol::BackendSpec,
        init: &crate::protocol::HarnessInit,
    ) -> Result<AcpInitialization, Error> {
        if init.port.is_some() || !matches!(init.endpoint, crate::protocol::EndpointRequest::Auto) {
            return Err(Error::InvalidConfig(
                "the selected ACP harness only supports managed stdio connections".to_owned(),
            ));
        }
        let cwd = HarnessDiscovery::resolve_cwd(init.cwd.as_deref())?;
        let (command, args) = match backend {
            crate::protocol::BackendSpec::Auto => kind.acp_command()?,
            crate::protocol::BackendSpec::Acp { command, args } => (command.clone(), args.clone()),
            _ => {
                return Err(Error::InvalidConfig(
                    "ACP backend requires an Acp or Auto specification".to_owned(),
                ));
            }
        };
        let executable =
            HarnessDiscovery::resolve_executable(init.executable.as_deref(), &command, Some(&cwd))?;
        let mut backend = AcpBackend::connect_with_cwd(
            command.clone(),
            args.clone(),
            Some(&cwd),
            Some(&executable),
        )
        .await?;
        let initialization = AcpInitialization {
            command,
            args,
            executable,
            protocol_version: backend.protocol_version.clone(),
            server_name: backend.server_name.clone(),
        };
        backend.close().await?;
        Ok(initialization)
    }

    pub(crate) fn supported_capabilities() -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            file_reads: true,
            command_execution: true,
            permission_requests: true,
            permission_responses: true,
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
        if !matches!(
            config.conversation.mode,
            crate::protocol::ConversationSpec::New
        ) {
            return Err(Error::UnsupportedCapability(
                "ACP sessions cannot be resumed across process restarts".to_owned(),
            ));
        }
        let kind = config.kind.clone();
        let cwd = HarnessDiscovery::resolve_cwd(Some(config.workspace_cwd()))?;
        let (command, args) = match &config.backend {
            crate::protocol::BackendSpec::Acp { command, args } => (command.clone(), args.clone()),
            crate::protocol::BackendSpec::Auto => kind.acp_command()?,
            _ => {
                return Err(Error::InvalidConfig(
                    "ACP backend requires an Acp or Auto specification".to_owned(),
                ));
            }
        };
        let executable = HarnessDiscovery::resolve_executable(
            config.runtime.process.executable.as_deref(),
            &command,
            Some(&cwd),
        )?;
        let backend =
            AcpBackend::connect_with_cwd(command, args, Some(&cwd), Some(&executable)).await?;
        let id = backend.session_id.clone();
        let mut session = crate::session::Session::with_backend_config(
            config,
            Some(id),
            Box::new(backend),
            initialized,
        );
        if !first_message.is_empty() {
            session.send(first_message, SendMode::Immediate).await?;
        }
        Ok(session)
    }
}

trait AcpCommand {
    fn acp_command(&self) -> Result<(String, Vec<String>), Error>;
}

impl AcpCommand for HarnessKind {
    fn acp_command(&self) -> Result<(String, Vec<String>), Error> {
        match self {
            HarnessKind::OpenCode => Ok(("opencode".to_owned(), vec!["acp".to_owned()])),
            HarnessKind::ZedAcp => Ok(("zed".to_owned(), vec!["acp".to_owned()])),
            HarnessKind::ZCode => Ok(("zcode".to_owned(), vec!["acp".to_owned()])),
            HarnessKind::DeepSeek => Ok(("deepseek".to_owned(), vec!["acp".to_owned()])),
            HarnessKind::Hermes => Ok(("hermes".to_owned(), vec!["acp".to_owned()])),
            _ => Err(Error::UnsupportedHarness(self.clone())),
        }
    }
}

trait AcpValue {
    fn protocol_version_string(&self) -> Option<String>;
    fn parse_update(&self) -> Vec<Event>;
    fn tool_events(&self, kind: &str) -> Vec<Event>;
    fn tool_status(&self, kind: &str) -> ToolStatus;
    fn text_event(&self, reasoning: bool) -> Option<Event>;
    fn status(&self) -> Option<Status>;
}

impl AcpValue for Value {
    fn protocol_version_string(&self) -> Option<String> {
        match self {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    fn parse_update(&self) -> Vec<Event> {
        let params = self.get("params").unwrap_or(self);
        let update = params.get("update").unwrap_or(params);
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .or_else(|| update.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = update.get("content").unwrap_or(update);
        match kind {
            "agent_message_chunk" | "agentMessageChunk" | "text_delta" => {
                content.text_event(false).into_iter().collect()
            }
            "agent_thought_chunk" | "agentThoughtChunk" | "reasoning_delta" => {
                content.text_event(true).into_iter().collect()
            }
            "tool_call" | "toolCall" | "tool_call_update" | "toolCallUpdate" | "tool_result"
            | "toolResult" | "file_read" | "fileRead" => update.tool_events(kind),
            "permission_request" | "permissionRequest" => vec![Event::PermissionRequired {
                id: update
                    .get("id")
                    .and_then(JsonValueExt::string_value)
                    .unwrap_or_else(|| "permission".to_owned()),
                detail: update.to_string(),
            }],
            "status" => update
                .get("status")
                .and_then(AcpValue::status)
                .map(|status| {
                    if status.is_terminal() {
                        Event::Finished(Finish::new(status, ""))
                    } else {
                        Event::Status(status)
                    }
                })
                .into_iter()
                .collect(),
            _ => {
                if let Some(status) = params
                    .get("status")
                    .and_then(AcpValue::status)
                    .or_else(|| update.get("status").and_then(AcpValue::status))
                {
                    if status.is_terminal() {
                        vec![Event::Finished(Finish::new(status, ""))]
                    } else {
                        vec![Event::Status(status)]
                    }
                } else {
                    vec![Event::Raw(self.clone())]
                }
            }
        }
    }

    fn tool_events(&self, kind: &str) -> Vec<Event> {
        let name = self
            .get("title")
            .or_else(|| self.get("name"))
            .or_else(|| self.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                if kind.to_ascii_lowercase().contains("file") {
                    "file_read"
                } else {
                    "tool"
                }
            });
        let id = self
            .get("toolCallId")
            .or_else(|| self.get("tool_call_id"))
            .or_else(|| self.get("id"))
            .and_then(JsonValueExt::string_value);
        let input = self
            .get("rawInput")
            .or_else(|| self.get("raw_input"))
            .or_else(|| self.get("input"))
            .or_else(|| self.get("arguments"))
            .or_else(|| self.get("args"));
        let output = self
            .get("rawOutput")
            .or_else(|| self.get("raw_output"))
            .or_else(|| self.get("output"))
            .or_else(|| self.get("result"));
        let error = self.get("error").or_else(|| self.get("errorMessage"));
        let status = self.tool_status(kind);
        let mut events = vec![Event::ToolCall(ToolCall {
            id: id.clone(),
            name: name.to_owned(),
            status,
            input: input.map(JsonValueExt::value_text),
            output: output.map(JsonValueExt::value_text),
            error: error.map(JsonValueExt::value_text),
            sequence: 0,
        })];

        let explicit = matches!(kind, "file_read" | "fileRead");
        if let Some(path) = input
            .and_then(JsonValueExt::file_path)
            .or_else(|| explicit.then(|| self.file_path()).flatten())
            .or_else(|| name.inferred_command_path(input))
        {
            events.push(Event::FileRead(FileRead {
                path,
                tool_id: id,
                status,
                line_start: input.and_then(|input| {
                    input.line_number(&["line_start", "start_line", "startLine"])
                }),
                line_end: input
                    .and_then(|input| input.line_number(&["line_end", "end_line", "endLine"])),
                summary: output.map(JsonValueExt::value_text),
                source: if explicit {
                    FileReadSource::Protocol
                } else {
                    FileReadSource::ToolInput
                },
                confidence: if explicit {
                    Confidence::High
                } else {
                    Confidence::Medium
                },
                sequence: 0,
            }));
        }
        events
    }

    fn tool_status(&self, kind: &str) -> ToolStatus {
        let status = self
            .get("status")
            .or_else(|| self.get("state"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        match status.as_deref().or({
            if matches!(kind, "tool_call" | "toolCall") {
                Some("pending")
            } else if matches!(kind, "tool_result" | "toolResult") {
                Some("completed")
            } else {
                None
            }
        }) {
            Some("pending") | Some("requested") => ToolStatus::Requested,
            Some("running") | Some("in_progress") | Some("inprogress") => ToolStatus::Running,
            Some("completed") | Some("complete") | Some("success") | Some("succeeded") => {
                ToolStatus::Completed
            }
            Some("failed") | Some("error") => ToolStatus::Failed,
            Some("cancelled") | Some("canceled") | Some("interrupted") => ToolStatus::Cancelled,
            _ => ToolStatus::Unknown,
        }
    }

    fn text_event(&self, reasoning: bool) -> Option<Event> {
        let text = self
            .get("text")
            .or_else(|| self.get("delta"))
            .and_then(Value::as_str)?
            .to_owned();
        Some(if reasoning {
            Event::ReasoningDelta(text)
        } else {
            Event::TextDelta(text)
        })
    }

    fn status(&self) -> Option<Status> {
        self.as_str().and_then(WireStatus::status).or_else(|| {
            self.get("type")
                .and_then(Value::as_str)
                .and_then(WireStatus::status)
        })
    }
}

trait CommandName {
    fn inferred_command_path(&self, input: Option<&Value>) -> Option<String>;
}

impl CommandName for str {
    fn inferred_command_path(&self, input: Option<&Value>) -> Option<String> {
        input.and_then(|input| input.inferred_command_path(self))
    }
}

trait WireStatus {
    fn status(&self) -> Option<Status>;
    fn stop_reason_status(&self) -> Status;
}

impl WireStatus for str {
    fn status(&self) -> Option<Status> {
        Some(match self {
            "working" | "running" | "in_progress" | "inProgress" => Status::Running,
            "idle" | "completed" | "complete" => Status::Completed,
            "cancelled" | "canceled" | "interrupted" => Status::Interrupted,
            "failed" | "error" => Status::Failed,
            "waitingForPermission" | "waiting_for_permission" | "permission" => {
                Status::WaitingForPermission
            }
            _ => return None,
        })
    }

    fn stop_reason_status(&self) -> Status {
        match self {
            "cancelled" | "canceled" | "interrupted" => Status::Interrupted,
            "refusal" | "error" => Status::Failed,
            _ => Status::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_acp_chunks_and_tools() {
        let message = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "你好" }
                }
            }
        });
        assert_eq!(
            message.parse_update(),
            vec![Event::TextDelta("你好".to_owned())]
        );

        let message = json!({
            "method": "session/update",
            "params": { "update": { "sessionUpdate": "tool_call", "title": "shell" } }
        });
        let events = message.parse_update();
        assert!(
            matches!(events[0], Event::ToolCall(ToolCall { ref name, status: ToolStatus::Requested, .. }) if name == "shell")
        );
    }

    #[test]
    fn parses_tool_lifecycle_and_inferred_file_read() {
        let message = json!({
            "method": "session/update",
            "params": { "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "read-1",
                "title": "read_file",
                "status": "completed",
                "rawInput": { "filePath": "src/lib.rs", "startLine": 2, "endLine": 4 },
                "rawOutput": "contents"
            }}
        });
        let events = message.parse_update();
        assert!(matches!(&events[0], Event::ToolCall(call)
            if call.id.as_deref() == Some("read-1")
                && call.status == ToolStatus::Completed
                && call.input.as_deref().unwrap().contains("src/lib.rs")));
        assert!(matches!(&events[1], Event::FileRead(file)
            if file.path == "src/lib.rs"
                && file.source == FileReadSource::ToolInput
                && file.confidence == Confidence::Medium
                && file.line_start == Some(2)
                && file.line_end == Some(4)));
    }

    #[test]
    fn maps_stop_reasons() {
        assert_eq!("end_turn".stop_reason_status(), Status::Completed);
        assert_eq!("cancelled".stop_reason_status(), Status::Interrupted);
        assert_eq!("error".stop_reason_status(), Status::Failed);
    }

    #[test]
    fn hermes_uses_acp_command() {
        assert_eq!(
            HarnessKind::Hermes.acp_command().unwrap(),
            ("hermes".to_owned(), vec!["acp".to_owned()])
        );
    }

    #[tokio::test]
    async fn resume_mode_is_rejected_before_connecting() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Hermes);
        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::ProviderSession { id: "s-1".to_owned() },
        );
        assert!(matches!(
            AcpBackend::create_session_with_config(config, String::new(), None).await,
            Err(Error::UnsupportedCapability(message)) if message.contains("ACP")
        ));
    }
}
