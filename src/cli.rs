use crate::protocol::{
    Confidence, Error, Event, FileRead, FileReadSource, Finish, HarnessType, Status, ToolCall,
    ToolStatus,
};
use crate::session::{Backend, BackendFuture, SendMode};
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
    harness: HarnessType,
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
        let harness = config.harness.clone();
        let (mut command, args) = command_for_config(&harness, &config.backend)?;
        if let Some(executable) = config.runtime.process.executable {
            command = executable.to_string_lossy().into_owned();
        }
        Ok(Self {
            harness,
            command,
            args,
            session_id: None,
            process: None,
            queued: VecDeque::new(),
            text_seen: false,
            finished: false,
            interrupt_requested: false,
            stream_tools: HashMap::new(),
            cwd: config.workspace.cwd,
        })
    }

    async fn spawn_turn(&mut self, message: String) -> Result<(), Error> {
        self.stop_process().await?;
        let args = build_args(
            &self.harness,
            &self.args,
            self.session_id.as_deref(),
            &message,
        );
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
        if let Some(id) = session_id(value) {
            self.session_id = Some(id);
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "system" => Vec::new(),
            "stream_event" => parse_stream_event(self, value),
            "assistant" => {
                let message = value.get("message").unwrap_or(value);
                let mut events = if self.text_seen {
                    Vec::new()
                } else {
                    text_events(message)
                };
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for item in content {
                        if matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("tool_use" | "tool_call")
                        ) {
                            events.extend(parse_tool_json(item));
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
            | "fileRead" => parse_tool_json(value),
            _ => {
                let mut events = text_events(value);
                if events.is_empty() {
                    if let Some(status) = value.get("status").and_then(status_from_value) {
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

pub(crate) fn initialize(
    harness: &HarnessType,
    backend: &crate::protocol::BackendSpec,
    init: &crate::protocol::HarnessInit,
) -> Result<crate::runtime::HarnessDiscovery, Error> {
    if init.port.is_some() || !matches!(init.endpoint, crate::protocol::EndpointRequest::Auto) {
        return Err(Error::InvalidConfig(
            "the selected CLI harness only supports managed stdio connections".to_owned(),
        ));
    }
    let cwd = crate::runtime::resolve_cwd(init.cwd.as_deref())?;
    let (command, args) = command_for_config(harness, backend)?;
    let executable =
        crate::runtime::resolve_executable(init.executable.as_deref(), &command, Some(&cwd))?;
    Ok(crate::runtime::discovery(
        harness,
        crate::protocol::BackendKind::PlainCli,
        &command,
        &args,
        executable,
        crate::protocol::TransportKind::Stdio,
        None,
    ))
}

pub(crate) fn supported_capabilities() -> crate::protocol::CapabilitySet {
    crate::protocol::CapabilitySet::default()
}

pub(crate) async fn create_session_with_config(
    config: crate::protocol::SessionConfig,
    first_message: String,
    initialized: Option<crate::protocol::HarnessRuntime>,
) -> Result<crate::session::Session, Error> {
    let backend = CliBackend::new_with_config(config.clone())?;
    let mut session =
        crate::session::Session::with_backend_config(config, None, Box::new(backend), initialized);
    session.send(first_message, SendMode::Immediate).await?;
    Ok(session)
}

fn command_for_config(
    harness: &HarnessType,
    backend: &crate::protocol::BackendSpec,
) -> Result<(String, Vec<String>), Error> {
    match backend {
        crate::protocol::BackendSpec::StructuredCli { command, args }
        | crate::protocol::BackendSpec::PlainCli { command, args }
        | crate::protocol::BackendSpec::Custom { command, args } => {
            Ok((command.clone(), args.clone()))
        }
        crate::protocol::BackendSpec::Auto => command_for(harness),
        crate::protocol::BackendSpec::CodexAppServer { .. }
        | crate::protocol::BackendSpec::Acp { .. } => Err(Error::InvalidConfig(
            "CLI backend cannot use an ACP or Codex App Server specification".to_owned(),
        )),
    }
}

fn command_for(harness: &HarnessType) -> Result<(String, Vec<String>), Error> {
    match harness {
        HarnessType::ClaudeCode => Ok(("claude".to_owned(), Vec::new())),
        HarnessType::Aider => Ok(("aider".to_owned(), Vec::new())),
        HarnessType::GeminiCli => Ok(("gemini".to_owned(), Vec::new())),
        HarnessType::Continue => Ok(("cn".to_owned(), Vec::new())),
        HarnessType::Goose => Ok(("goose".to_owned(), Vec::new())),
        HarnessType::Cline => Ok(("cline".to_owned(), Vec::new())),
        HarnessType::RooCode => Ok(("roo".to_owned(), Vec::new())),
        HarnessType::OpenHands => Ok(("openhands".to_owned(), Vec::new())),
        HarnessType::SweAgent => Ok(("sweagent".to_owned(), Vec::new())),
        HarnessType::Custom { command, args } => Ok((command.clone(), args.clone())),
        _ => Err(Error::UnsupportedHarness(harness.clone())),
    }
}

fn build_args(
    harness: &HarnessType,
    base: &[String],
    session_id: Option<&str>,
    message: &str,
) -> Vec<String> {
    if matches!(harness, HarnessType::ClaudeCode) {
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
        return args;
    }

    let mut args = base
        .iter()
        .map(|arg| {
            arg.replace("{message}", message)
                .replace("{session_id}", session_id.unwrap_or_default())
        })
        .collect::<Vec<_>>();
    if !base.iter().any(|arg| arg.contains("{message}")) {
        match harness {
            HarnessType::Aider => args.extend(["--message".to_owned(), message.to_owned()]),
            HarnessType::GeminiCli => args.extend([
                "-p".to_owned(),
                message.to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ]),
            HarnessType::Continue => args.extend(["-p".to_owned(), message.to_owned()]),
            _ => args.push(message.to_owned()),
        }
    }
    args
}

fn session_id(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .or_else(|| value.get("session").and_then(|session| session.get("id")))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_stream_event(backend: &mut CliBackend, value: &Value) -> Vec<Event> {
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
                return stream_tool_delta(backend, event, delta);
            }
            let events = text_events(delta);
            backend.text_seen |= events
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
                    id: block.get("id").and_then(string_value),
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
                let result = tool_event(
                    tool.id.clone(),
                    tool.name.clone(),
                    tool.input.clone(),
                    "requested",
                    None,
                    false,
                );
                backend.stream_tools.insert(index, tool);
                result
            } else {
                text_events(block)
            }
        }
        "content_block_stop" => {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            backend
                .stream_tools
                .remove(&index)
                .map(|tool| tool_event(tool.id, tool.name, tool.input, "completed", None, false))
                .unwrap_or_default()
        }
        "message_start" | "message_stop" => Vec::new(),
        _ => vec![Event::Raw(value.clone())],
    }
}

fn stream_tool_delta(backend: &mut CliBackend, event: &Value, delta: &Value) -> Vec<Event> {
    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let Some(tool) = backend.stream_tools.get_mut(&index) else {
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
    tool_event(
        tool.id.clone(),
        tool.name.clone(),
        input,
        "running",
        None,
        false,
    )
}

#[derive(Debug)]
struct StreamTool {
    id: Option<String>,
    name: String,
    input: Value,
}

fn tool_event(
    id: Option<String>,
    name: String,
    input: Value,
    status: &str,
    result: Option<&Value>,
    explicit_file: bool,
) -> Vec<Event> {
    let status = tool_status(status);
    let mut events = vec![Event::ToolCall(ToolCall {
        id: id.clone(),
        name: name.clone(),
        status,
        input: (!input.is_null()).then(|| value_text(&input)),
        output: result.map(value_text),
        error: None,
        sequence: 0,
    })];
    if let Some(path) = file_path(Some(&input)).or_else(|| inferred_command_path(&name, &input)) {
        events.push(Event::FileRead(FileRead {
            path,
            tool_id: id,
            status,
            line_start: line_number(Some(&input), &["line_start", "start_line", "startLine"]),
            line_end: line_number(Some(&input), &["line_end", "end_line", "endLine"]),
            summary: result.map(value_text),
            source: if explicit_file {
                FileReadSource::Protocol
            } else {
                FileReadSource::ToolInput
            },
            confidence: if explicit_file {
                Confidence::High
            } else {
                Confidence::Medium
            },
            sequence: 0,
        }));
    }
    events
}

fn tool_status(status: &str) -> ToolStatus {
    match status {
        "requested" => ToolStatus::Requested,
        "running" => ToolStatus::Running,
        "completed" => ToolStatus::Completed,
        "failed" => ToolStatus::Failed,
        "cancelled" => ToolStatus::Cancelled,
        _ => ToolStatus::Unknown,
    }
}

fn parse_tool_json(value: &Value) -> Vec<Event> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (id, name, input, status, result, explicit_file) = match kind {
        "tool_use" | "tool_call" | "toolCall" => (
            value
                .get("id")
                .or_else(|| value.get("tool_call_id"))
                .and_then(string_value),
            value
                .get("name")
                .or_else(|| value.get("tool_name"))
                .or_else(|| value.get("toolName"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            value
                .get("input")
                .or_else(|| value.get("arguments"))
                .or_else(|| value.get("args"))
                .cloned()
                .unwrap_or(Value::Null),
            "requested",
            None,
            false,
        ),
        "tool_result" | "toolResult" => {
            let failed = value
                .get("is_error")
                .or_else(|| value.get("isError"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (
                value
                    .get("tool_use_id")
                    .or_else(|| value.get("tool_call_id"))
                    .or_else(|| value.get("id"))
                    .and_then(string_value),
                value
                    .get("name")
                    .or_else(|| value.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                value.get("input").cloned().unwrap_or(Value::Null),
                if failed { "failed" } else { "completed" },
                value
                    .get("output")
                    .or_else(|| value.get("content"))
                    .or_else(|| value.get("result")),
                false,
            )
        }
        "file_read" | "fileRead" => (
            value.get("id").and_then(string_value),
            "file_read".to_owned(),
            value
                .get("input")
                .or_else(|| value.get("path").map(|_| value))
                .cloned()
                .unwrap_or(Value::Null),
            "completed",
            value.get("output").or_else(|| value.get("content")),
            true,
        ),
        _ => return Vec::new(),
    };
    tool_event(id, name, input, status, result, explicit_file)
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn line_number(value: Option<&Value>, keys: &[&str]) -> Option<usize> {
    let object = value?.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64).map(|n| n as usize))
}

fn file_path(value: Option<&Value>) -> Option<String> {
    let value = value?.as_object()?;
    [
        "path",
        "file_path",
        "filePath",
        "filename",
        "file",
        "target",
        "uri",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_owned))
}

fn inferred_command_path(name: &str, input: &Value) -> Option<String> {
    let name = name.to_ascii_lowercase();
    if !matches!(
        name.as_str(),
        "cat" | "grep" | "rg" | "ripgrep" | "read" | "read_file" | "readfile"
    ) {
        return None;
    }
    let command = input.as_str()?;
    command
        .split_whitespace()
        .find(|part| {
            part.starts_with("./")
                || part.starts_with("../")
                || part.starts_with('/')
                || part.contains('.')
        })
        .map(|part| part.trim_matches('"').trim_matches('\'').to_owned())
}

fn string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn text_events(value: &Value) -> Vec<Event> {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return vec![Event::TextDelta(text.to_owned())];
    }
    if let Some(delta) = value.get("delta").and_then(Value::as_str) {
        return vec![Event::TextDelta(delta.to_owned())];
    }
    let Some(content) = value.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|item| {
            let text = item.get("text").and_then(Value::as_str)?;
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("text");
            Some(if kind.contains("reason") {
                Event::ReasoningDelta(text.to_owned())
            } else {
                Event::TextDelta(text.to_owned())
            })
        })
        .collect()
}

fn status_from_value(value: &Value) -> Option<Status> {
    value.as_str().and_then(status_from_str).or_else(|| {
        value
            .get("type")
            .and_then(Value::as_str)
            .and_then(status_from_str)
    })
}

fn status_from_str(value: &str) -> Option<Status> {
    Some(match value {
        "running" | "working" | "in_progress" => Status::Running,
        "completed" | "complete" | "idle" => Status::Completed,
        "cancelled" | "canceled" | "interrupted" => Status::Interrupted,
        "failed" | "error" => Status::Failed,
        "waitingForPermission" | "waiting_for_permission" => Status::WaitingForPermission,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_args_include_resume_and_streaming() {
        let args = build_args(&HarnessType::ClaudeCode, &[], Some("session-1"), "你好");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--resume", "session-1"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--output-format", "stream-json"]));
    }

    #[test]
    fn custom_args_support_placeholders() {
        let args = build_args(
            &HarnessType::Custom {
                command: "agent".to_owned(),
                args: vec![
                    "--prompt".to_owned(),
                    "{message}".to_owned(),
                    "{session_id}".to_owned(),
                ],
            },
            &[
                "--prompt".to_owned(),
                "{message}".to_owned(),
                "{session_id}".to_owned(),
            ],
            Some("s-1"),
            "hello",
        );
        assert_eq!(args, vec!["--prompt", "hello", "s-1"]);
    }

    #[test]
    fn parses_claude_stream_and_result() {
        let mut backend = CliBackend::new_with_config(crate::protocol::SessionConfig::default_for(
            HarnessType::ClaudeCode,
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
            HarnessType::ClaudeCode,
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
            HarnessType::ClaudeCode,
        ))
        .unwrap();
        assert_eq!(
            backend.parse_json(&serde_json::json!({ "type": "future_event" })),
            vec![Event::Raw(serde_json::json!({ "type": "future_event" }))]
        );
        assert!(matches!(
            parse_stream_event(
                &mut backend,
                &serde_json::json!({
                    "type": "stream_event",
                    "event": { "type": "future_block" }
                })
            )[0],
            Event::Raw(_)
        ));
    }
}
