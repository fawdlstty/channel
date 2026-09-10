use crate::process::JsonlProcess;
use crate::protocol::{
    Confidence, Error, Event, FileRead, FileReadSource, Finish, Status, ToolCall, ToolStatus,
};
use crate::runtime::HarnessDiscovery;
use crate::session::{Backend, BackendFuture, PermissionResponse, SendMode};
use crate::utils::websocket::LocalWebSocket;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

#[cfg(windows)]
mod windows;

const COMMAND: &str = "codex";
const ARGS: &[&str] = &["app-server", "--stdio"];

enum CodexTransport {
    Process(Box<JsonlProcess>),
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
                    Some(Ok(Message::Text(message))) => return message.parse_message(),
                    Some(Ok(Message::Binary(message))) => {
                        let message = std::str::from_utf8(&message).map_err(|error| {
                            Error::ProtocolError(format!("Codex WebSocket was not UTF-8: {error}"))
                        })?;
                        return message.parse_message();
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(Error::Backend(format!(
                            "failed to read Codex WebSocket message: {error}"
                        )));
                    }
                    None => return Ok(None),
                }
            },
        }
    }

    async fn close(&mut self, grace: Duration) -> Result<(), Error> {
        match self {
            Self::Process(process) => process.close(grace).await,
            Self::WebSocket(stream) => {
                let _ = stream.send(Message::Close(None)).await;
                stream.flush().await.map_err(|error| {
                    Error::Backend(format!("failed to close Codex WebSocket: {error}"))
                })
            }
        }
    }
}

trait CodexMessage {
    fn parse_message(&self) -> Result<Option<Value>, Error>;
}

impl CodexMessage for str {
    fn parse_message(&self) -> Result<Option<Value>, Error> {
        serde_json::from_str(self)
            .map(Some)
            .map_err(|error| Error::ProtocolError(format!("invalid Codex JSON message: {error}")))
    }
}

pub(crate) struct CodexBackend {
    transport: CodexTransport,
    thread_id: String,
    turn_id: Option<String>,
    model: crate::protocol::ModelOptions,
    next_request_id: u64,
    queued: VecDeque<Event>,
    pending_permissions: HashMap<String, Value>,
    pending_responses: HashMap<u64, String>,
    text: String,
    kill_grace_period: Duration,
    #[allow(dead_code)]
    resources: Option<crate::switch::PreparedResources>,
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
                ));
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
        let endpoint = config.endpoint.codex_endpoint(config.port)?;
        let resources = crate::protocol::SwitchProvider::CcSwitch
            .prepare_resources(config)
            .await?;
        let resource_env = resources
            .as_ref()
            .map(crate::switch::PreparedResources::env);
        let transport = if let Some(endpoint) = endpoint {
            CodexTransport::WebSocket(Box::new(
                CodexTransport::connect_websocket(&endpoint).await?,
            ))
        } else {
            CodexTransport::Process(Box::new(
                JsonlProcess::spawn_with_cwd_and_env(
                    &command,
                    &arg_refs,
                    Some(config.workspace_cwd()),
                    resource_env
                        .as_ref()
                        .map(|environment| &environment[..])
                        .unwrap_or(&[]),
                )
                .await?,
            ))
        };
        let mut backend = Self {
            transport,
            thread_id: String::new(),
            turn_id: None,
            model: config.model.clone(),
            next_request_id: 1,
            queued: VecDeque::new(),
            pending_permissions: HashMap::new(),
            pending_responses: HashMap::new(),
            text: String::new(),
            kill_grace_period: config.runtime.process.kill_grace_period,
            resources,
        };

        if let Some(model) = config.model.requested.as_deref() {
            if config.model.available_models.is_empty() {
                return Err(Error::InvalidConfig(
                    "Codex model availability is unknown; initialize the harness before selecting a model"
                        .to_owned(),
                ));
            }
            if !config
                .model
                .available_models
                .iter()
                .any(|available| available == model)
            {
                return Err(Error::InvalidConfig(format!(
                    "Codex model is unavailable: {model}"
                )));
            }
        }

        let initialize_result = backend
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
        if initialize_result.get("protocolVersion").is_none()
            && initialize_result.get("protocol_version").is_none()
            && initialize_result.get("serverInfo").is_none()
            && initialize_result.get("server_info").is_none()
            && initialize_result.get("userAgent").is_none()
            && initialize_result.get("user_agent").is_none()
        {
            return Err(Error::ProtocolError(
                "Codex initialize response contains no protocol or server information".to_owned(),
            ));
        }
        backend
            .notify("initialized", Value::Object(Default::default()))
            .await?;

        let resume_id = config.conversation_thread_id()?;
        let (method, params) = match resume_id {
            Some(id) => ("thread/resume", config.thread_resume_params(id)),
            None => ("thread/start", config.thread_start_params()),
        };
        let result = backend.request(method, params).await?;
        backend.thread_id = result
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Backend(format!("{method} returned no thread id")))?
            .to_owned();
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
        self.pending_responses.insert(id, method.to_owned());
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
                self.pending_responses.remove(&id);
                if let Some(error) = message.get("error") {
                    return Err(Error::Backend(format!(
                        "Codex RPC {method} failed: {error}"
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            return Err(Error::ProtocolError(format!(
                "Codex returned an unexpected response while waiting for {method}: {message}"
            )));
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
        if method.is_permission_request() {
            self.queued.push_back(Event::PermissionRequired {
                id: id.to_string(),
                detail,
            });
            self.pending_permissions.insert(id.to_string(), id);
            return Ok(());
        }
        self.queued.push_back(Event::Raw(message.clone()));
        let response = json!({
            "id": id,
            "error": { "code": -32601, "message": "request is not supported by channel" }
        });
        self.transport.write(&response).await
    }

    async fn respond(&mut self, id: &str, response: PermissionResponse) -> Result<(), Error> {
        let id = self
            .pending_permissions
            .remove(id)
            .ok_or_else(|| Error::Backend(format!("unknown permission request id: {id}")))?;
        // codex 0.153.2 的审批应答枚举是 accept/acceptForSession/decline/cancel
        // 等（与请求里的 availableDecisions 对应），不是 approve/decline。
        let decision = match response {
            PermissionResponse::Approve => "accept",
            PermissionResponse::Deny => "decline",
        };
        self.transport
            .write(&json!({ "id": id, "result": { "decision": decision } }))
            .await
    }

    async fn deny_pending_permissions(&mut self) {
        let ids: Vec<Value> = self.pending_permissions.values().cloned().collect();
        self.pending_permissions.clear();
        for id in ids {
            let _ = self
                .transport
                .write(&json!({ "id": id, "result": { "decision": "decline" } }))
                .await;
        }
    }

    fn queue_notification(&mut self, message: &Value) {
        self.queued.extend(CodexNotification(message).events());
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
                    self.model.turn_start_params(&self.thread_id, message),
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

impl crate::protocol::ModelOptions {
    fn turn_start_params(&self, thread_id: &str, message: String) -> Value {
        let mut params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": message }]
        });
        if let Some(model_name) = self.requested.as_ref() {
            params["model"] = Value::String(model_name.clone());
        }
        if let Some(effort) = self.reasoning_effort {
            params["effort"] = Value::String(effort.as_str().to_owned());
        }
        params
    }
}

impl crate::protocol::SessionConfig {
    fn thread_start_params(&self) -> Value {
        let mut params = json!({
            "cwd": self.workspace_cwd().to_string_lossy(),
        });
        if self.observability == Some(false) {
            params["ephemeral"] = Value::Bool(true);
        }
        if let Some(sandbox) = self.full_access_sandbox() {
            params["sandbox"] = Value::String(sandbox.to_owned());
        }
        params
    }

    fn thread_resume_params(&self, thread_id: &str) -> Value {
        let mut params = json!({
            "threadId": thread_id,
            "cwd": self.workspace_cwd().to_string_lossy(),
        });
        if let Some(sandbox) = self.full_access_sandbox() {
            params["sandbox"] = Value::String(sandbox.to_owned());
        }
        params
    }

    /// Full access is expressed as an unsandboxed thread while keeping the
    /// provider's default approval policy: with sandbox `never` approvals are
    /// also disabled, but escalated tool calls are then rejected outright
    /// instead of surfacing as answerable permission requests (verified
    /// against codex 0.153.2). An unsandboxed thread already grants escalated
    /// commands without asking, and any residual request stays auto-answerable.
    fn full_access_sandbox(&self) -> Option<&'static str> {
        self.is_full_access().then_some("danger-full-access")
    }

    fn conversation_thread_id(&self) -> Result<Option<&str>, Error> {
        match &self.conversation.mode {
            crate::protocol::ConversationSpec::New => Ok(None),
            crate::protocol::ConversationSpec::Resume(target) => {
                target.provider_id().map(Some).ok_or_else(|| {
                    Error::UnsupportedCapability(
                        "native UI handles cannot be resumed by the Codex backend".to_owned(),
                    )
                })
            }
            crate::protocol::ConversationSpec::Fork(_)
            | crate::protocol::ConversationSpec::Attach(_) => Err(Error::UnsupportedCapability(
                "conversation fork and attach are not supported by the Codex backend".to_owned(),
            )),
        }
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
        Box::pin(async move {
            self.deny_pending_permissions().await;
            self.transport.close(self.kill_grace_period).await
        })
    }

    fn respond_permission<'a>(
        &'a mut self,
        id: &'a str,
        response: PermissionResponse,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.respond(id, response).await })
    }
}

pub(crate) struct CodexInitialization {
    pub command: String,
    pub args: Vec<String>,
    pub executable: std::path::PathBuf,
    pub endpoint: Option<String>,
    pub models: Vec<String>,
}

fn resolve_executable(
    requested: Option<&std::path::Path>,
    command: &str,
    cwd: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, Error> {
    if requested.is_none() && command.eq_ignore_ascii_case("codex") {
        #[cfg(windows)]
        if let Some(path) = windows::find_codex() {
            return Ok(path);
        }

        #[cfg(not(windows))]
        let _ = command;
    }

    HarnessDiscovery::resolve_executable(requested, command, cwd)
}

impl CodexBackend {
    pub(crate) async fn initialize(
        backend: &crate::protocol::BackendSpec,
        init: &crate::protocol::HarnessInit,
    ) -> Result<CodexInitialization, Error> {
        let cwd = HarnessDiscovery::resolve_cwd(init.cwd.as_deref())?;
        let endpoint = init.endpoint.codex_endpoint(init.port)?;
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
                ));
            }
        };
        let executable = resolve_executable(init.executable.as_deref(), &command, Some(&cwd))?;
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let executable_path = executable.to_string_lossy().into_owned();
        let models = match init.model.as_deref() {
            Some(_) => {
                CodexTransport::list_models(
                    &executable_path,
                    &arg_refs,
                    Some(&cwd),
                    endpoint.as_deref(),
                )
                .await?
            }
            None => Vec::new(),
        };

        Ok(CodexInitialization {
            command,
            args,
            executable,
            endpoint,
            models,
        })
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
            provider_persistence: true,
            provider_resume: true,
            raw_events: true,
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) async fn create_session_with_config(
        config: crate::protocol::SessionConfig,
        first_message: String,
        initialized: Option<crate::protocol::HarnessState>,
    ) -> Result<crate::session::Session, Error> {
        let backend = Self::connect_with_config(&config).await?;
        let id = backend.thread_id.clone();
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

impl CodexTransport {
    async fn list_models(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        endpoint: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        // codex app-server 启动时会同步 curated 插件目录（git → GitHub HTTP →
        // archive 多级回退），并阻塞到同步结束后才响应请求；受限网络下该同步
        // 可能远超 5 秒，超时过短会把可用渠道误判为初始化失败。
        let models = tokio::time::timeout(
            Duration::from_secs(30),
            Self::list_models_once(command, args, cwd, endpoint),
        )
        .await
        .map_err(|_| Error::Initialization("timed out loading Codex models".to_owned()))??;
        Ok(models)
    }

    async fn list_models_once(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        endpoint: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        let mut stream = Self::connect(command, args, cwd, endpoint).await?;
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
        stream.read_response(1, "initialize").await?;
        stream
            .write(&json!({
                "method": "initialized",
                "params": Value::Object(Default::default())
            }))
            .await?;
        stream
            .write(&json!({
                "id": 2,
                "method": "model/list",
                "params": {}
            }))
            .await?;
        let result = stream.read_response(2, "model/list").await?;
        let models = result.model_ids();
        stream.close(Duration::from_secs(1)).await?;
        Ok(models)
    }

    async fn connect(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        endpoint: Option<&str>,
    ) -> Result<Self, Error> {
        if let Some(endpoint) = endpoint {
            return Ok(Self::WebSocket(Box::new(
                Self::connect_websocket(endpoint).await?,
            )));
        }
        Ok(Self::Process(Box::new(
            JsonlProcess::spawn_with_cwd(command, args, cwd).await?,
        )))
    }

    async fn connect_websocket(
        endpoint: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Error> {
        let endpoint = endpoint
            .normalize_local_websocket()
            .map_err(Error::InvalidConfig)?;
        let stream = tokio::time::timeout(Duration::from_secs(3), connect_async(endpoint))
            .await
            .map_err(|_| {
                Error::Initialization("timed out connecting to the local Codex endpoint".to_owned())
            })?
            .map_err(|error| Error::Initialization(format!("failed to connect to Codex: {error}")))?
            .0;
        Ok(stream)
    }

    async fn read_response(&mut self, request_id: u64, method: &str) -> Result<Value, Error> {
        loop {
            match self.read().await? {
                Some(message) if message.get("id") == Some(&json!(request_id)) => {
                    if let Some(error) = message.get("error") {
                        return Err(Error::Initialization(format!(
                            "Codex RPC {method} failed: {error}"
                        )));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Some(_) => {}
                None => {
                    return Err(Error::Initialization(format!(
                        "Codex closed before {method} completed"
                    )));
                }
            }
        }
    }
}

trait CodexModelList {
    fn model_ids(&self) -> Vec<String>;
}

impl CodexModelList for Value {
    fn model_ids(&self) -> Vec<String> {
        self.get("data")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct CodexNotification<'a>(&'a Value);

impl<'a> CodexNotification<'a> {
    fn events(&self) -> Vec<Event> {
        let Some(method) = self.0.get("method").and_then(Value::as_str) else {
            return Vec::new();
        };
        let params = self.0.get("params").cloned().unwrap_or(Value::Null);

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
                    .and_then(CodexStatus::from_value)
                    .or_else(|| params.get("status").and_then(CodexStatus::from_value))
                    .unwrap_or(Status::Failed);
                vec![Event::Finished(Finish::new(status, ""))]
            }
            "turn/status/changed" => params
                .get("status")
                .and_then(CodexStatus::non_terminal_from_value)
                .map(|status| vec![Event::Status(status)])
                .unwrap_or_default(),
            "error" => {
                if params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    Vec::new()
                } else {
                    vec![Event::Error(CodexNotification::error_message(&params))]
                }
            }
            "thread/status/changed" => params
                .get("status")
                .and_then(CodexStatus::thread_from_value)
                .map(|status| vec![Event::Status(status)])
                .unwrap_or_default(),
            "item/commandExecution/outputDelta" | "item/command_execution/output_delta" => params
                .get("delta")
                .and_then(Value::as_str)
                .map(|delta| {
                    vec![Event::ToolCall(ToolCall {
                        id: params.item_id(),
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
                        id: params.item_id(),
                        name: "fileChange".to_owned(),
                        status: ToolStatus::Running,
                        input: None,
                        output: Some(delta.to_owned()),
                        error: None,
                        sequence: 0,
                    })]
                })
                .unwrap_or_default(),
            "item/started" | "item/completed" => {
                CodexItemLifecycle::new(self.0, &params, method).events()
            }
            _ => vec![Event::Raw(self.0.clone())],
        }
    }

    fn error_message(params: &Value) -> String {
        params
            .pointer("/error/message")
            .or_else(|| params.get("error"))
            .or_else(|| params.get("message"))
            .map(CodexText::text)
            .unwrap_or_else(|| "Codex reported an error".to_owned())
    }
}

struct CodexItemLifecycle<'a> {
    message: &'a Value,
    item: &'a Value,
    item_type: &'a str,
    method: &'a str,
}

impl<'a> CodexItemLifecycle<'a> {
    fn new(message: &'a Value, params: &'a Value, method: &'a str) -> Self {
        Self {
            message,
            item: params.get("item").unwrap_or(&Value::Null),
            item_type: Self::item_type(params),
            method,
        }
    }

    fn item_type(params: &Value) -> &str {
        params
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn events(&self) -> Vec<Event> {
        if !self.item.is_object() {
            return vec![Event::Raw(self.message.clone())];
        }
        let Some(name) = self.item.tool_name(self.item_type) else {
            return vec![Event::Raw(self.message.clone())];
        };
        let status = self.item.status(self.method);
        let id = self
            .item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let input = self.item.tool_input(self.item_type);
        let output = self
            .item
            .get("aggregatedOutput")
            .or_else(|| self.item.get("output"))
            .or_else(|| self.item.get("result"));
        let error = if matches!(status, ToolStatus::Failed) {
            output.map(CodexText::text)
        } else {
            self.item.get("error").map(CodexText::text)
        };
        let mut events = vec![Event::ToolCall(ToolCall {
            id: id.clone(),
            name,
            status,
            input: input.as_ref().map(CodexText::text),
            output: output.map(CodexText::text),
            error,
            sequence: 0,
        })];

        for path in self.item.read_paths(self.item_type) {
            events.push(Event::FileRead(FileRead {
                path,
                tool_id: id.clone(),
                status,
                line_start: input.as_ref().and_then(|value| {
                    value.line_number(&["line_start", "start_line", "startLine"])
                }),
                line_end: input
                    .as_ref()
                    .and_then(|value| value.line_number(&["line_end", "end_line", "endLine"])),
                summary: output.map(CodexText::text),
                source: FileReadSource::Protocol,
                confidence: Confidence::High,
                sequence: 0,
            }));
        }
        events
    }
}

trait CodexItem {
    fn tool_name(&self, item_type: &str) -> Option<String>;
    fn tool_input(&self, item_type: &str) -> Option<Value>;
    fn status(&self, method: &str) -> ToolStatus;
    fn read_paths(&self, item_type: &str) -> Vec<String>;
}

impl CodexItem for Value {
    fn tool_name(&self, item_type: &str) -> Option<String> {
        match item_type {
            "commandExecution" | "fileChange" | "webSearch" | "imageView" | "imageGeneration"
            | "sleep" | "fileRead" | "readFile" | "file_read" => Some(
                match item_type {
                    "fileRead" | "readFile" | "file_read" => "fileRead",
                    other => other,
                }
                .to_owned(),
            ),
            "mcpToolCall" | "dynamicToolCall" | "collabAgentToolCall" => self
                .get("tool")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(item_type.to_owned())),
            "functionCallOutput" => self
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(item_type.to_owned())),
            _ => None,
        }
    }

    fn tool_input(&self, item_type: &str) -> Option<Value> {
        match item_type {
            "commandExecution" => self.get("command").cloned(),
            "mcpToolCall" | "dynamicToolCall" => {
                self.get("arguments").or_else(|| self.get("input")).cloned()
            }
            "collabAgentToolCall" => self.get("prompt").cloned(),
            "functionCallOutput" => self.get("output").cloned(),
            _ => self.get("input").cloned(),
        }
    }

    fn status(&self, method: &str) -> ToolStatus {
        let status = self
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

    fn read_paths(&self, item_type: &str) -> Vec<String> {
        let mut paths = Vec::new();
        if matches!(item_type, "fileRead" | "readFile" | "file_read") {
            for key in ["path", "filePath", "file_path"] {
                if let Some(path) = self.get(key).and_then(Value::as_str) {
                    paths.push(path.to_owned());
                }
            }
        }
        if item_type == "commandExecution" {
            if let Some(actions) = self.get("commandActions").and_then(Value::as_array) {
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
}

trait CodexItemIdentifier {
    fn item_id(&self) -> Option<String>;
}

impl CodexItemIdentifier for Value {
    fn item_id(&self) -> Option<String> {
        self.get("itemId")
            .or_else(|| self.get("item_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

trait CodexText {
    fn text(&self) -> String;
    fn line_number(&self, keys: &[&str]) -> Option<usize>;
}

impl CodexText for Value {
    fn text(&self) -> String {
        self.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| self.to_string())
    }

    fn line_number(&self, keys: &[&str]) -> Option<usize> {
        let object = self.as_object()?;
        keys.iter()
            .find_map(|key| object.get(*key).and_then(Value::as_u64).map(|n| n as usize))
    }
}

trait PermissionMethod {
    fn is_permission_request(&self) -> bool;
}

impl PermissionMethod for str {
    fn is_permission_request(&self) -> bool {
        self.contains("requestApproval")
            || self.contains("approval")
            || self.contains("requestUserInput")
            || self.contains("userInput")
    }
}

struct CodexStatus;

impl CodexStatus {
    fn from_value(value: &Value) -> Option<Status> {
        value.as_str().and_then(Self::from_str).or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .and_then(Self::from_str)
        })
    }

    fn non_terminal_from_value(value: &Value) -> Option<Status> {
        match Self::from_value(value)? {
            Status::Starting | Status::Running | Status::WaitingForPermission => {
                Self::from_value(value)
            }
            _ => None,
        }
    }

    fn thread_from_value(value: &Value) -> Option<Status> {
        match value
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| value.as_str())?
        {
            "active" => Some(Status::Running),
            _ => None,
        }
    }

    fn from_str(status: &str) -> Option<Status> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HarnessKind;
    use crate::protocol::ModelOptions;
    use crate::protocol::ReasoningEffort;

    #[test]
    fn private_threads_are_started_ephemerally() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.set_workspace(Some("/tmp/channel".into()));
        config.set_observability(false);

        assert_eq!(
            config.thread_start_params(),
            json!({
                "cwd": "/tmp/channel",
                "ephemeral": true,
            })
        );
    }

    #[test]
    fn full_access_threads_run_without_sandbox() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.set_workspace(Some("/tmp/channel".into()));
        config.set_observability(false);
        config.set_full_access(true);

        assert_eq!(
            config.thread_start_params(),
            json!({
                "cwd": "/tmp/channel",
                "ephemeral": true,
                "sandbox": "danger-full-access",
            })
        );
        assert_eq!(
            config.thread_resume_params("th-1"),
            json!({
                "threadId": "th-1",
                "cwd": "/tmp/channel",
                "sandbox": "danger-full-access",
            })
        );
    }

    #[test]
    fn default_threads_do_not_override_sandbox() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.set_workspace(Some("/tmp/channel".into()));
        assert!(config.thread_start_params().get("sandbox").is_none());
    }

    #[test]
    fn provider_default_threads_are_not_ephemeral() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.set_workspace(Some("/tmp/channel".into()));

        assert_eq!(
            config.thread_start_params(),
            json!({ "cwd": "/tmp/channel" })
        );
    }

    #[test]
    fn conversation_modes_map_to_thread_lifecycle_requests() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::Codex);
        config.set_workspace(Some("/tmp/channel".into()));
        assert_eq!(config.conversation_thread_id().unwrap(), None);

        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::ProviderThread {
                id: "th-1".to_owned(),
            },
        );
        assert_eq!(config.conversation_thread_id().unwrap(), Some("th-1"));
        assert_eq!(
            config.thread_resume_params("th-1"),
            json!({ "threadId": "th-1", "cwd": "/tmp/channel" })
        );

        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::NativeUiHandle {
                id: "ui-1".to_owned(),
            },
        );
        assert!(matches!(
            config.conversation_thread_id(),
            Err(Error::UnsupportedCapability(_))
        ));

        config.conversation.mode = crate::protocol::ConversationSpec::Fork(
            crate::protocol::ResumeTarget::ProviderSession {
                id: "s-1".to_owned(),
            },
        );
        assert!(matches!(
            config.conversation_thread_id(),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    #[test]
    fn parses_model_list_ids() {
        let result = json!({
            "data": [
                { "id": "gpt-5", "displayName": "GPT-5" },
                { "id": "gpt-5-mini", "displayName": "GPT-5 mini" }
            ]
        });

        assert_eq!(result.model_ids(), vec!["gpt-5", "gpt-5-mini"]);
    }

    #[test]
    fn turn_start_includes_selected_model_and_effort() {
        let model = ModelOptions {
            requested: Some("gpt-5".to_owned()),
            reasoning_effort: Some(ReasoningEffort::XHigh),
            ..ModelOptions::default()
        };

        assert_eq!(
            model.turn_start_params("thread-1", "hello".to_owned()),
            json!({
                "threadId": "thread-1",
                "input": [{ "type": "text", "text": "hello" }],
                "model": "gpt-5",
                "effort": "xhigh",
            })
        );
    }

    fn notification(method: &str, params: Value) -> Value {
        json!({ "method": method, "params": params })
    }

    #[test]
    fn parses_streaming_text_and_reasoning_without_login() {
        assert_eq!(
            CodexNotification(&notification(
                "item/agentMessage/delta",
                json!({ "delta": "你好" }),
            ))
            .events(),
            vec![Event::TextDelta("你好".to_owned())]
        );
        assert_eq!(
            CodexNotification(&notification(
                "item/reasoning/summaryTextDelta",
                json!({ "delta": "先思考" }),
            ))
            .events(),
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
                CodexNotification(&notification(
                    "turn/completed",
                    json!({ "turn": { "status": wire } }),
                ))
                .events(),
                vec![Event::Finished(Finish::new(status, ""))]
            );
        }
    }

    #[test]
    fn ignores_retryable_errors_and_surfaces_final_errors() {
        let retryable = notification(
            "error",
            json!({
                "error": { "message": "Reconnecting... 1/5" },
                "willRetry": true
            }),
        );
        assert!(CodexNotification(&retryable).events().is_empty());

        let failed = notification(
            "error",
            json!({
                "error": { "message": "provider unavailable" },
                "willRetry": false
            }),
        );
        assert_eq!(
            CodexNotification(&failed).events(),
            vec![Event::Error("provider unavailable".to_owned())]
        );
    }

    #[test]
    fn parses_codex_object_status_notifications_without_login() {
        assert_eq!(
            CodexNotification(&notification(
                "thread/status/changed",
                json!({ "status": { "type": "active", "activeFlags": [] } }),
            ))
            .events(),
            vec![Event::Status(Status::Running)]
        );
    }

    #[test]
    fn preserves_unknown_notifications_as_raw_events() {
        let message = notification("some/new/event", json!({ "value": 1 }));
        assert_eq!(
            CodexNotification(&message).events(),
            vec![Event::Raw(message)]
        );
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

        let started_events = CodexNotification(&started).events();
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

        let completed_events = CodexNotification(&completed).events();
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
            CodexNotification(&notification(
                "item/commandExecution/outputDelta",
                json!({ "itemId": "cmd-1", "delta": "line 1\n" }),
            ))
            .events(),
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
            CodexNotification(&notification(
                "item/command_execution/output_delta",
                json!({ "item_id": "cmd-2", "delta": "line 2" }),
            ))
            .events()[0],
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
        let events = CodexNotification(&notification(
            "item/completed",
            json!({
                "item": {
                    "id": "change-1",
                    "type": "fileChange",
                    "changes": [{ "path": "src/lib.rs" }],
                    "status": "completed"
                }
            }),
        ))
        .events();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::ToolCall(ToolCall { name, .. }) if name == "fileChange")
        );
    }

    #[test]
    fn preserves_malformed_item_notifications_as_raw_events() {
        let message = notification("item/completed", json!({ "item": { "id": "x" } }));
        assert_eq!(
            CodexNotification(&message).events(),
            vec![Event::Raw(message)]
        );
    }

    #[test]
    fn recognizes_approval_and_user_input_requests_without_treating_unknowns_as_permission() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/tool/requestUserInput",
        ] {
            assert!(method.is_permission_request());
        }
        assert!(!"item/newFutureRequest".is_permission_request());
    }
}
