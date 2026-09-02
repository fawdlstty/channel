use crate::process::JsonlProcess;
use crate::protocol::{
    Confidence, Error, Event, FileRead, FileReadSource, Finish, ProtocolInfo, Status, ToolCall,
    ToolStatus,
};
use crate::session::{Backend, BackendFuture, SendMode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

const COMMAND: &str = "codex";
const ARGS: &[&str] = &["app-server", "--stdio"];

enum CodexTransport {
    Process(JsonlProcess),
    WebSocket(Box<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>),
}

impl CodexTransport {
    async fn write(&mut self, message: &Value) -> Result<(), Error> {
        match self {
            Self::Process(process) => process.write(message).await,
            Self::WebSocket(stream) => {
                let payload = serde_json::to_string(message).map_err(|error| {
                    Error::Backend(format!("failed to encode Codex message: {error}"))
                })?;
                stream
                    .send(Message::Text(payload.into()))
                    .await
                    .map_err(|error| {
                        Error::Backend(format!("failed to write Codex WebSocket message: {error}"))
                    })
            }
        }
    }

    async fn read(&mut self) -> Result<Option<Value>, Error> {
        match self {
            Self::Process(process) => process.read().await,
            Self::WebSocket(stream) => loop {
                match stream.next().await {
                    Some(Ok(Message::Text(message))) => return parse_ws_message(&message),
                    Some(Ok(Message::Binary(message))) => {
                        let message = std::str::from_utf8(&message).map_err(|error| {
                            Error::ProtocolError(format!("Codex WebSocket was not UTF-8: {error}"))
                        })?;
                        return parse_ws_message(message);
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(Error::Backend(format!(
                            "failed to read Codex WebSocket message: {error}"
                        )))
                    }
                    None => return Ok(None),
                }
            },
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        match self {
            Self::Process(process) => process.close().await,
            Self::WebSocket(stream) => {
                let _ = stream.send(Message::Close(None)).await;
                stream.flush().await.map_err(|error| {
                    Error::Backend(format!("failed to close Codex WebSocket: {error}"))
                })
            }
        }
    }
}

fn parse_ws_message(message: &str) -> Result<Option<Value>, Error> {
    serde_json::from_str(message)
        .map(Some)
        .map_err(|error| Error::ProtocolError(format!("invalid Codex JSON message: {error}")))
}

pub(crate) struct CodexBackend {
    transport: CodexTransport,
    thread_id: String,
    turn_id: Option<String>,
    next_request_id: u64,
    queued: VecDeque<Event>,
    text: String,
}

impl CodexBackend {
    pub(crate) async fn connect_with_config(
        config: &crate::protocol::SessionConfig,
    ) -> Result<Self, Error> {
        let (command, args) = match &config.backend {
            crate::protocol::BackendSpec::CodexAppServer { command, args } => {
                (command.clone(), args.clone())
            }
            crate::protocol::BackendSpec::Auto => (
                COMMAND.to_owned(),
                ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
            ),
            _ => {
                return Err(Error::InvalidConfig(
                    "Codex requires Auto or CodexAppServer backend".to_owned(),
                ))
            }
        };
        let command = config
            .runtime
            .process
            .executable
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(command);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let endpoint = crate::runtime::codex_endpoint(&config.endpoint, config.port)?;
        let transport = if let Some(endpoint) = endpoint {
            CodexTransport::WebSocket(Box::new(connect_websocket(&endpoint).await?))
        } else {
            CodexTransport::Process(
                JsonlProcess::spawn_with_cwd(&command, &arg_refs, Some(&config.workspace.cwd))
                    .await?,
            )
        };
        let mut backend = Self {
            transport,
            thread_id: String::new(),
            turn_id: None,
            next_request_id: 1,
            queued: VecDeque::new(),
            text: String::new(),
        };

        backend
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "channel",
                        "title": "channel",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {}
                }),
            )
            .await?;
        backend
            .notify("initialized", Value::Object(Default::default()))
            .await?;

        let result = backend
            .request(
                "thread/start",
                json!({
                    "cwd": config.workspace.cwd.to_string_lossy(),
                }),
            )
            .await?;
        backend.thread_id = result
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Backend("thread/start returned no thread id".to_owned()))?
            .to_owned();
        backend.queued.clear();
        Ok(backend)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), Error> {
        self.transport
            .write(&json!({ "method": method, "params": params }))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.transport
            .write(&json!({ "id": id, "method": method, "params": params }))
            .await?;

        loop {
            let message = self.transport.read().await?.ok_or_else(|| {
                Error::Backend("Codex app server ended before completing the request".to_owned())
            })?;

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queue_notification(&message);
                }
                continue;
            }

            if message.get("id") == Some(&json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(Error::Backend(format!(
                        "Codex RPC {method} failed: {error}"
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    async fn handle_server_request(&mut self, message: &Value, method: &str) -> Result<(), Error> {
        let id = message
            .get("id")
            .cloned()
            .ok_or_else(|| Error::Backend("Codex server request has no id".to_owned()))?;
        let detail = message
            .get("params")
            .map(Value::to_string)
            .unwrap_or_else(|| "Codex requested permission".to_owned());
        let is_permission = is_permission_request(method);
        if is_permission {
            self.queued.push_back(Event::PermissionRequired {
                id: id.to_string(),
                detail,
            });
        } else {
            self.queued.push_back(Event::Raw(message.clone()));
        }

        let response = if method.contains("requestApproval") || method.contains("approval") {
            json!({ "id": id, "result": { "decision": "decline" } })
        } else {
            json!({
                "id": id,
                "error": { "code": -32601, "message": "request is not supported by channel" }
            })
        };
        self.transport.write(&response).await
    }

    fn queue_notification(&mut self, message: &Value) {
        self.queued.extend(parse_notification(message));
    }

    async fn next(&mut self) -> Result<Option<Event>, Error> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(self.record(event)));
        }

        loop {
            let message = match self.transport.read().await? {
                Some(message) => message,
                None => return Ok(None),
            };
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queue_notification(&message);
                }
                if let Some(event) = self.queued.pop_front() {
                    return Ok(Some(self.record(event)));
                }
            }
        }
    }

    fn record(&mut self, event: Event) -> Event {
        match &event {
            Event::TextDelta(delta) => self.text.push_str(delta),
            Event::Finished(finish) if finish.text.is_empty() => {
                return Event::Finished(Finish::new(finish.status, self.text.clone()));
            }
            _ => {}
        }
        event
    }

    fn start_turn<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.text.clear();
            let result = self
                .request(
                    "turn/start",
                    json!({
                        "threadId": self.thread_id,
                        "input": [{ "type": "text", "text": message }]
                    }),
                )
                .await?;
            self.turn_id = Some(
                result
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Backend("turn/start returned no turn id".to_owned()))?
                    .to_owned(),
            );
            Ok(())
        })
    }
}

impl Backend for CodexBackend {
    fn id(&self) -> Option<String> {
        Some(self.thread_id.clone())
    }
    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        self.start_turn(message)
    }

    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
        Box::pin(async move { self.next().await })
    }

    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let turn_id = self.turn_id.clone().ok_or(Error::NoActiveTurn)?;
            self.request(
                "turn/interrupt",
                json!({ "threadId": self.thread_id, "turnId": turn_id }),
            )
            .await?;
            Ok(())
        })
    }

    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.transport.close().await })
    }
}

pub(crate) struct CodexInitialization {
    pub command: String,
    pub args: Vec<String>,
    pub executable: std::path::PathBuf,
    pub endpoint: Option<String>,
}

pub(crate) async fn initialize(
    backend: &crate::protocol::BackendSpec,
    init: &crate::protocol::HarnessInit,
) -> Result<CodexInitialization, Error> {
    let cwd = crate::runtime::resolve_cwd(init.cwd.as_deref())?;
    let endpoint = crate::runtime::codex_endpoint(&init.endpoint, init.port)?;
    let (command, args) = match backend {
        crate::protocol::BackendSpec::Auto => (
            COMMAND.to_owned(),
            ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
        ),
        crate::protocol::BackendSpec::CodexAppServer { command, args } => {
            (command.clone(), args.clone())
        }
        _ => {
            return Err(Error::InvalidConfig(
                "Codex backend requires a Codex App Server or Auto specification".to_owned(),
            ))
        }
    };
    let executable =
        crate::runtime::resolve_executable(init.executable.as_deref(), &command, Some(&cwd))?;
    if let Some(endpoint) = &endpoint {
        probe_websocket(endpoint).await?;
    }

    Ok(CodexInitialization {
        command,
        args,
        executable,
        endpoint,
    })
}

async fn probe_websocket(endpoint: &str) -> Result<crate::protocol::ProtocolInfo, Error> {
    let endpoint =
        crate::runtime::normalize_local_websocket(endpoint).map_err(Error::InvalidConfig)?;
    let mut stream = CodexTransport::WebSocket(Box::new(connect_websocket(&endpoint).await?));
    stream
        .write(&json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "channel",
                    "title": "channel",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {}
            }
        }))
        .await?;
    loop {
        match stream.read().await? {
            Some(message) if message.get("id") == Some(&json!(1)) => {
                if let Some(error) = message.get("error") {
                    return Err(Error::Initialization(format!(
                        "Codex WebSocket handshake failed: {error}"
                    )));
                }
                break;
            }
            Some(_) => {}
            None => {
                return Err(Error::Initialization(
                    "Codex WebSocket closed before initialize completed".to_owned(),
                ))
            }
        }
    }
    stream.close().await?;
    Ok(ProtocolInfo {
        name: "codex-app-server".to_owned(),
        version: None,
        initialized: true,
    })
}

async fn connect_websocket(
    endpoint: &str,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Error> {
    let endpoint =
        crate::runtime::normalize_local_websocket(endpoint).map_err(Error::InvalidConfig)?;
    let stream = tokio::time::timeout(Duration::from_secs(3), connect_async(endpoint))
        .await
        .map_err(|_| {
            Error::Initialization("timed out connecting to the local Codex endpoint".to_owned())
        })?
        .map_err(|error| Error::Initialization(format!("failed to connect to Codex: {error}")))?
        .0;
    Ok(stream)
}

pub(crate) fn supported_capabilities() -> crate::protocol::CapabilitySet {
    crate::protocol::CapabilitySet {
        streaming_events: true,
        structured_text: true,
        reasoning_summary: true,
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
    initialized: Option<crate::protocol::HarnessRuntime>,
) -> Result<crate::session::Session, Error> {
    let backend = CodexBackend::connect_with_config(&config).await?;
    let id = backend.thread_id.clone();
    let mut session = crate::session::Session::with_backend_config(
        config,
        Some(id),
        Box::new(backend),
        initialized,
    );
    session.send(first_message, SendMode::Immediate).await?;
    Ok(session)
}

pub(crate) fn parse_notification(message: &Value) -> Vec<Event> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Vec::new();
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| vec![Event::TextDelta(delta.to_owned())])
            .unwrap_or_default(),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| vec![Event::ReasoningDelta(delta.to_owned())])
            .unwrap_or_default(),
        "turn/started" => vec![Event::Status(Status::Running)],
        "turn/completed" => {
            let status = params
                .get("turn")
                .and_then(|turn| turn.get("status"))
                .and_then(status_from_value)
                .or_else(|| params.get("status").and_then(status_from_value))
                .unwrap_or(Status::Failed);
            vec![Event::Finished(Finish::new(status, ""))]
        }
        "turn/status/changed" => params
            .get("status")
            .and_then(non_terminal_status_from_value)
            .map(|status| vec![Event::Status(status)])
            .unwrap_or_default(),
        "thread/status/changed" => params
            .get("status")
            .and_then(thread_status_from_value)
            .map(|status| vec![Event::Status(status)])
            .unwrap_or_default(),
        "item/commandExecution/outputDelta" | "item/command_execution/output_delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| {
                vec![Event::ToolCall(ToolCall {
                    id: item_id(&params),
                    name: "commandExecution".to_owned(),
                    status: ToolStatus::Running,
                    input: None,
                    output: Some(delta.to_owned()),
                    error: None,
                    sequence: 0,
                })]
            })
            .unwrap_or_default(),
        "item/fileChange/outputDelta" | "item/file_change/output_delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| {
                vec![Event::ToolCall(ToolCall {
                    id: item_id(&params),
                    name: "fileChange".to_owned(),
                    status: ToolStatus::Running,
                    input: None,
                    output: Some(delta.to_owned()),
                    error: None,
                    sequence: 0,
                })]
            })
            .unwrap_or_default(),
        "item/started" | "item/completed" => parse_item_lifecycle(message, &params, method),
        _ => vec![Event::Raw(message.clone())],
    }
}

fn parse_item_lifecycle(message: &Value, params: &Value, method: &str) -> Vec<Event> {
    let Some(item) = params.get("item").filter(|item| item.is_object()) else {
        return vec![Event::Raw(message.clone())];
    };
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return vec![Event::Raw(message.clone())];
    };
    let Some(name) = tool_name(item, item_type) else {
        return vec![Event::Raw(message.clone())];
    };
    let status = item_status(item, method);
    let id = item.get("id").and_then(Value::as_str).map(str::to_owned);
    let input = tool_input(item, item_type);
    let output = item
        .get("aggregatedOutput")
        .or_else(|| item.get("output"))
        .or_else(|| item.get("result"));
    let error = if matches!(status, ToolStatus::Failed) {
        output.map(value_text)
    } else {
        item.get("error").map(value_text)
    };
    let mut events = vec![Event::ToolCall(ToolCall {
        id: id.clone(),
        name,
        status,
        input: input.as_ref().map(value_text),
        output: output.map(value_text),
        error,
        sequence: 0,
    })];

    for path in read_paths(item, item_type) {
        events.push(Event::FileRead(FileRead {
            path,
            tool_id: id.clone(),
            status,
            line_start: input
                .as_ref()
                .and_then(|value| line_number(value, &["line_start", "start_line", "startLine"])),
            line_end: input
                .as_ref()
                .and_then(|value| line_number(value, &["line_end", "end_line", "endLine"])),
            summary: output.map(value_text),
            source: FileReadSource::Protocol,
            confidence: Confidence::High,
            sequence: 0,
        }));
    }
    events
}

fn tool_name(item: &Value, item_type: &str) -> Option<String> {
    match item_type {
        "commandExecution" | "fileChange" | "webSearch" | "imageView" | "imageGeneration"
        | "sleep" | "fileRead" | "readFile" | "file_read" => Some(
            match item_type {
                "fileRead" | "readFile" | "file_read" => "fileRead",
                other => other,
            }
            .to_owned(),
        ),
        "mcpToolCall" | "dynamicToolCall" | "collabAgentToolCall" => item
            .get("tool")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(item_type.to_owned())),
        "functionCallOutput" => item
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(item_type.to_owned())),
        _ => None,
    }
}

fn tool_input(item: &Value, item_type: &str) -> Option<Value> {
    match item_type {
        "commandExecution" => item.get("command").cloned(),
        "mcpToolCall" | "dynamicToolCall" => {
            item.get("arguments").or_else(|| item.get("input")).cloned()
        }
        "collabAgentToolCall" => item.get("prompt").cloned(),
        "functionCallOutput" => item.get("output").cloned(),
        _ => item.get("input").cloned(),
    }
}

fn item_status(item: &Value, method: &str) -> ToolStatus {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    match status.as_deref().or_else(|| {
        if method == "item/started" {
            Some("inprogress")
        } else {
            Some("completed")
        }
    }) {
        Some("inprogress") | Some("in_progress") | Some("running") => ToolStatus::Running,
        Some("completed") | Some("complete") | Some("success") | Some("succeeded") => {
            ToolStatus::Completed
        }
        Some("failed") | Some("error") => ToolStatus::Failed,
        Some("declined") | Some("cancelled") | Some("canceled") | Some("interrupted") => {
            ToolStatus::Cancelled
        }
        _ => ToolStatus::Unknown,
    }
}

fn read_paths(item: &Value, item_type: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if matches!(item_type, "fileRead" | "readFile" | "file_read") {
        for key in ["path", "filePath", "file_path"] {
            if let Some(path) = item.get(key).and_then(Value::as_str) {
                paths.push(path.to_owned());
            }
        }
    }
    if item_type == "commandExecution" {
        if let Some(actions) = item.get("commandActions").and_then(Value::as_array) {
            for action in actions {
                if action.get("type").and_then(Value::as_str) == Some("read") {
                    if let Some(path) = action.get("path").and_then(Value::as_str) {
                        paths.push(path.to_owned());
                    }
                }
            }
        }
    }
    paths
}

fn item_id(params: &Value) -> Option<String> {
    params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn line_number(value: &Value, keys: &[&str]) -> Option<usize> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64).map(|n| n as usize))
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn is_permission_request(method: &str) -> bool {
    method.contains("requestApproval")
        || method.contains("approval")
        || method.contains("requestUserInput")
        || method.contains("userInput")
}

fn status_from_value(value: &Value) -> Option<Status> {
    value.as_str().and_then(status_from_str).or_else(|| {
        value
            .get("type")
            .and_then(Value::as_str)
            .and_then(status_from_str)
    })
}

fn non_terminal_status_from_value(value: &Value) -> Option<Status> {
    match status_from_value(value)? {
        Status::Starting | Status::Running | Status::WaitingForPermission => {
            status_from_value(value)
        }
        _ => None,
    }
}

fn thread_status_from_value(value: &Value) -> Option<Status> {
    match value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())?
    {
        "active" => Some(Status::Running),
        _ => None,
    }
}

fn status_from_str(status: &str) -> Option<Status> {
    Some(match status {
        "inProgress" | "running" | "started" => Status::Running,
        "completed" | "complete" | "idle" => Status::Completed,
        "interrupted" | "cancelled" | "canceled" => Status::Interrupted,
        "failed" | "error" => Status::Failed,
        "waitingForApproval" | "waitingForPermission" | "requiresApproval" => {
            Status::WaitingForPermission
        }
        "starting" => Status::Starting,
        "closed" => Status::Closed,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(method: &str, params: Value) -> Value {
        json!({ "method": method, "params": params })
    }

    #[test]
    fn parses_streaming_text_and_reasoning_without_login() {
        assert_eq!(
            parse_notification(&notification(
                "item/agentMessage/delta",
                json!({ "delta": "你好" }),
            )),
            vec![Event::TextDelta("你好".to_owned())]
        );
        assert_eq!(
            parse_notification(&notification(
                "item/reasoning/summaryTextDelta",
                json!({ "delta": "先思考" }),
            )),
            vec![Event::ReasoningDelta("先思考".to_owned())]
        );
    }

    #[test]
    fn parses_turn_completion_states_without_login() {
        for (wire, status) in [
            ("completed", Status::Completed),
            ("interrupted", Status::Interrupted),
            ("failed", Status::Failed),
        ] {
            assert_eq!(
                parse_notification(&notification(
                    "turn/completed",
                    json!({ "turn": { "status": wire } }),
                )),
                vec![Event::Finished(Finish::new(status, ""))]
            );
        }
    }

    #[test]
    fn parses_codex_object_status_notifications_without_login() {
        assert_eq!(
            parse_notification(&notification(
                "thread/status/changed",
                json!({ "status": { "type": "active", "activeFlags": [] } }),
            )),
            vec![Event::Status(Status::Running)]
        );
    }

    #[test]
    fn preserves_unknown_notifications_as_raw_events() {
        let message = notification("some/new/event", json!({ "value": 1 }));
        assert_eq!(parse_notification(&message), vec![Event::Raw(message)]);
    }

    #[test]
    fn parses_tool_lifecycle_and_confirmed_file_reads() {
        let started = notification(
            "item/started",
            json!({
                "item": {
                    "id": "cmd-1",
                    "type": "commandExecution",
                    "command": "cat src/lib.rs",
                    "commandActions": [{ "type": "read", "path": "src/lib.rs" }],
                    "status": "inProgress"
                }
            }),
        );
        let completed = notification(
            "item/completed",
            json!({
                "item": {
                    "id": "cmd-1",
                    "type": "commandExecution",
                    "command": "cat src/lib.rs",
                    "commandActions": [{ "type": "read", "path": "src/lib.rs" }],
                    "aggregatedOutput": "contents",
                    "status": "completed"
                }
            }),
        );

        let started_events = parse_notification(&started);
        assert_eq!(started_events.len(), 2);
        assert!(matches!(
            &started_events[0],
            Event::ToolCall(ToolCall { id: Some(id), name, status: ToolStatus::Running, input: Some(input), .. })
                if id == "cmd-1" && name == "commandExecution" && input == "cat src/lib.rs"
        ));
        assert!(matches!(
            &started_events[1],
            Event::FileRead(FileRead { path, tool_id: Some(id), status: ToolStatus::Running, source: FileReadSource::Protocol, confidence: Confidence::High, .. })
                if path == "src/lib.rs" && id == "cmd-1"
        ));

        let completed_events = parse_notification(&completed);
        assert_eq!(completed_events.len(), 2);
        assert!(matches!(
            &completed_events[0],
            Event::ToolCall(ToolCall { id: Some(id), name, status: ToolStatus::Completed, .. })
                if id == "cmd-1" && name == "commandExecution"
        ));
        assert_eq!(
            completed_events[1],
            Event::FileRead(FileRead {
                path: "src/lib.rs".to_owned(),
                tool_id: Some("cmd-1".to_owned()),
                status: ToolStatus::Completed,
                line_start: None,
                line_end: None,
                summary: Some("contents".to_owned()),
                source: FileReadSource::Protocol,
                confidence: Confidence::High,
                sequence: 0,
            })
        );
    }

    #[test]
    fn keeps_command_output_associated_with_item_id() {
        assert_eq!(
            parse_notification(&notification(
                "item/commandExecution/outputDelta",
                json!({ "itemId": "cmd-1", "delta": "line 1\n" }),
            )),
            vec![Event::ToolCall(ToolCall {
                id: Some("cmd-1".to_owned()),
                name: "commandExecution".to_owned(),
                status: ToolStatus::Running,
                input: None,
                output: Some("line 1\n".to_owned()),
                error: None,
                sequence: 0,
            })]
        );
        assert_eq!(
            parse_notification(&notification(
                "item/command_execution/output_delta",
                json!({ "item_id": "cmd-2", "delta": "line 2" }),
            ))[0],
            Event::ToolCall(ToolCall {
                id: Some("cmd-2".to_owned()),
                name: "commandExecution".to_owned(),
                status: ToolStatus::Running,
                input: None,
                output: Some("line 2".to_owned()),
                error: None,
                sequence: 0,
            })
        );
    }

    #[test]
    fn does_not_report_file_changes_as_file_reads() {
        let events = parse_notification(&notification(
            "item/completed",
            json!({
                "item": {
                    "id": "change-1",
                    "type": "fileChange",
                    "changes": [{ "path": "src/lib.rs" }],
                    "status": "completed"
                }
            }),
        ));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::ToolCall(ToolCall { name, .. }) if name == "fileChange")
        );
    }

    #[test]
    fn preserves_malformed_item_notifications_as_raw_events() {
        let message = notification("item/completed", json!({ "item": { "id": "x" } }));
        assert_eq!(parse_notification(&message), vec![Event::Raw(message)]);
    }

    #[test]
    fn recognizes_approval_and_user_input_requests_without_treating_unknowns_as_permission() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/tool/requestUserInput",
        ] {
            assert!(is_permission_request(method));
        }
        assert!(!is_permission_request("item/newFutureRequest"));
    }
}
