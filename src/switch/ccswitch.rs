use crate::protocol::Error;
use rusqlite::OpenFlags;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const DEFAULT_API_FORMAT: &str = "openai_responses";

#[derive(Clone, Debug)]
pub(crate) struct ResolvedProvider {
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    pub api_key: String,
    pub api_format: String,
    pub model: String,
    pub base_url: String,
}

pub(crate) struct CodexDatabase(std::path::PathBuf);

impl CodexDatabase {
    pub(crate) fn path() -> Result<Self, Error> {
        if let Some(path) = std::env::var_os("CHANNEL_CC_SWITCH_DB") {
            return Ok(Self(std::path::PathBuf::from(path)));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| {
                Error::Initialization("cannot locate the user home directory".to_owned())
            })?;
        Ok(Self(
            std::path::PathBuf::from(home)
                .join(".cc-switch")
                .join("cc-switch.db"),
        ))
    }

    pub(crate) fn available_keys(&self) -> Result<Vec<String>, Error> {
        let connection = self.0.open_read_only()?;
        let mut statement = connection
            .prepare(
                "SELECT name FROM providers WHERE app_type = 'codex' \
                 ORDER BY sort_index, created_at DESC, id",
            )
            .db_error("failed to read cc-switch providers")?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))
            .db_error("failed to read cc-switch providers")?
            .collect::<Result<Vec<_>, _>>()
            .db_error("failed to read cc-switch providers")?;
        Ok(keys)
    }

    pub(crate) fn resolve_provider(&self, name: &str) -> Result<ResolvedProvider, Error> {
        let connection = self.0.open_read_only()?;
        let count = connection
            .query_row(
                "SELECT count(*) FROM providers WHERE app_type = 'codex' AND name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .db_error("failed to resolve cc-switch provider")?;
        if count == 0 {
            return Err(Error::InvalidConfig(format!(
                "cc-switch provider was not found: {name}"
            )));
        }
        if count > 1 {
            return Err(Error::InvalidConfig(format!(
                "cc-switch provider name is ambiguous: {name}"
            )));
        }

        let (id, name, settings, meta) = connection
            .query_row(
                "SELECT id, name, settings_config, meta FROM providers \
                 WHERE app_type = 'codex' AND name = ?1",
                [name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .db_error("failed to resolve cc-switch provider")?;
        ResolvedProvider::parse(&id, &name, &settings, &meta)
    }
}

trait SqliteDatabase {
    fn open_read_only(&self) -> Result<rusqlite::Connection, Error>;
}

impl SqliteDatabase for Path {
    fn open_read_only(&self) -> Result<rusqlite::Connection, Error> {
        rusqlite::Connection::open_with_flags(
            self,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            Error::Initialization(format!(
                "failed to open cc-switch database {}: {error}",
                self.display()
            ))
        })
    }
}

trait CodexDatabaseResult<T> {
    fn db_error(self, message: &'static str) -> Result<T, Error>;
}

impl<T> CodexDatabaseResult<T> for Result<T, rusqlite::Error> {
    fn db_error(self, message: &'static str) -> Result<T, Error> {
        self.map_err(|error| Error::InvalidConfig(format!("{message}: {error}")))
    }
}

impl ResolvedProvider {
    fn parse(id: &str, name: &str, settings: &str, meta: &str) -> Result<Self, Error> {
        let settings: Value = serde_json::from_str(settings).map_err(|error| {
            Error::InvalidConfig(format!("invalid cc-switch settings for {name}: {error}"))
        })?;
        let meta: Value = if meta.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(meta).map_err(|error| {
                Error::InvalidConfig(format!("invalid cc-switch metadata for {name}: {error}"))
            })?
        };
        let config_text = settings
            .get("config")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::InvalidConfig(format!("cc-switch provider has no Codex config: {name}"))
            })?;
        let config: toml::Value = toml::from_str(config_text).map_err(|error| {
            Error::InvalidConfig(format!("invalid Codex TOML for {name}: {error}"))
        })?;

        let api_key = settings
            .get("auth")
            .and_then(Value::as_object)
            .and_then(|auth| {
                auth.iter()
                    .filter(|(_, value)| value.as_str().is_some_and(|value| !value.is_empty()))
                    .map(|(_, value)| value.as_str().unwrap().to_owned())
                    .next()
            })
            .ok_or_else(|| {
                Error::UnsupportedCapability(
                    "OAuth cc-switch providers are not supported".to_owned(),
                )
            })?;
        let model = config
            .get("model")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                Error::InvalidConfig(format!("cc-switch provider has no default model: {name}"))
            })?;
        let base_url = config
            .get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| {
                providers
                    .values()
                    .filter_map(toml::Value::as_table)
                    .filter_map(|provider| provider.get("base_url"))
                    .filter_map(toml::Value::as_str)
                    .next()
            })
            .ok_or_else(|| {
                Error::InvalidConfig(format!("cc-switch provider has no upstream URL: {name}"))
            })?;
        let api_format = meta
            .get("apiFormat")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_API_FORMAT);

        Ok(Self {
            id: id.to_owned(),
            name: name.to_owned(),
            api_key,
            api_format: api_format.to_owned(),
            model: model.to_owned(),
            base_url: base_url.to_owned(),
        })
    }
}

struct TempCodexHome {
    path: PathBuf,
}

impl TempCodexHome {
    fn create() -> Result<Self, Error> {
        let root = std::env::temp_dir();
        for attempt in 0..32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| Error::Initialization(error.to_string()))?
                .as_nanos();
            let path = root.join(format!(
                "channel-codex-{}-{nanos}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                            .map_err(|error| Error::Initialization(error.to_string()))?;
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(Error::Initialization(format!(
                        "failed to create temporary Codex home: {error}"
                    )))
                }
            }
        }
        Err(Error::Initialization(
            "failed to allocate a temporary Codex home".to_owned(),
        ))
    }

    fn write(&self, provider: &ResolvedProvider, upstream: &str) -> Result<(), Error> {
        let config = format!(
            "model = {}\nmodel_provider = \"channel\"\n\n[model_providers.channel]\nname = \"channel\"\nbase_url = {}\nwire_api = \"responses\"\nrequires_openai_auth = true\n",
            toml::Value::String(provider.model.clone()),
            toml::Value::String(upstream.to_owned())
        );
        std::fs::write(self.path.join("config.toml"), config).map_err(|error| {
            Error::Initialization(format!("failed to write Codex config: {error}"))
        })?;
        let auth = json!({ "OPENAI_API_KEY": provider.api_key });
        std::fs::write(
            self.path.join("auth.json"),
            serde_json::to_vec(&auth).map_err(|error| Error::Initialization(error.to_string()))?,
        )
        .map_err(|error| Error::Initialization(format!("failed to write Codex auth: {error}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| Error::Initialization(error.to_string()))?;
            std::fs::set_permissions(
                self.path.join("auth.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .map_err(|error| Error::Initialization(error.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) struct CodexResources {
    codex_home: TempCodexHome,
    #[allow(dead_code)]
    provider: ResolvedProvider,
    #[allow(dead_code)]
    relay: Option<Relay>,
}

impl CodexResources {
    pub(crate) fn env(&self) -> [(&str, &Path); 1] {
        [("CODEX_HOME", self.codex_home.path.as_path())]
    }

    pub(crate) async fn prepare(
        config: &crate::protocol::SessionConfig,
    ) -> Result<Option<Self>, Error> {
        let Some(provider_name) = config.get_switch_key(crate::protocol::SwitchProvider::CcSwitch)
        else {
            return Ok(None);
        };
        if !matches!(config.endpoint, crate::protocol::EndpointRequest::Auto)
            || config.port.is_some()
        {
            return Err(Error::InvalidConfig(
                "a configured cc-switch key requires the default Codex process endpoint".to_owned(),
            ));
        }
        let provider = CodexDatabase::path()?.resolve_provider(provider_name)?;
        let codex_home = TempCodexHome::create()?;
        let relay = if provider.api_format == DEFAULT_API_FORMAT {
            codex_home.write(&provider, &provider.base_url)?;
            None
        } else {
            match provider.api_format.as_str() {
                "openai_chat" => {
                    let relay = Relay::start(&provider).await?;
                    let upstream = format!("{}/v1", relay.endpoint.trim_end_matches('/'));
                    codex_home.write(&provider, &upstream)?;
                    Some(relay)
                }
                "anthropic" => {
                    let relay = Relay::start(&provider).await?;
                    let upstream = relay.endpoint.trim_end_matches('/').to_owned();
                    codex_home.write(&provider, &upstream)?;
                    Some(relay)
                }
                unsupported => {
                    return Err(Error::InvalidConfig(format!(
                        "unsupported cc-switch API format: {unsupported}"
                    )))
                }
            }
        };
        Ok(Some(Self {
            codex_home,
            provider,
            relay,
        }))
    }
}

pub(crate) struct Relay {
    pub endpoint: String,
    task: JoinHandle<()>,
}

impl Relay {
    async fn start(provider: &ResolvedProvider) -> Result<Self, Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|error| {
            Error::Initialization(format!("failed to start local relay: {error}"))
        })?;
        let address = listener
            .local_addr()
            .map_err(|error| Error::Initialization(error.to_string()))?
            .to_string();
        let client = reqwest::Client::builder().build().map_err(|error| {
            Error::Initialization(format!("failed to create relay client: {error}"))
        })?;
        let provider = Arc::new(provider.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let client = client.clone();
                let provider = provider.clone();
                tokio::spawn(
                    RelayConnection {
                        stream,
                        client,
                        provider,
                    }
                    .serve(),
                );
            }
        });
        Ok(Self {
            endpoint: format!("http://{address}"),
            task,
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RelayConnection {
    stream: TcpStream,
    client: reqwest::Client,
    provider: Arc<ResolvedProvider>,
}

impl RelayConnection {
    async fn serve(mut self) {
        let body = match self.stream.read_request().await {
            Ok(body) => body,
            Err(_) => return,
        };
        let request: Value = match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(_) => {
                let _ = self
                    .stream
                    .write_json_response(400, "invalid Responses request")
                    .await;
                return;
            }
        };
        let result = if self.provider.api_format == "openai_chat" {
            self.provider.forward_chat(&self.client, request).await
        } else {
            self.provider.forward_anthropic(&self.client, request).await
        };
        match result {
            Ok(upstream) => {
                let response = upstream.responses_sse();
                let _ = self.stream.write_sse_response(200, &response).await;
            }
            Err((status, message)) => {
                let _ = self.stream.write_json_response(status, &message).await;
            }
        }
    }
}

trait HttpConnection {
    async fn read_request(&mut self) -> Result<Vec<u8>, std::io::Error>;
    async fn write_json_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
    async fn write_sse_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
}

impl HttpConnection for TcpStream {
    async fn read_request(&mut self) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        let header_end = loop {
            let count = self.read(&mut chunk).await?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "relay request ended before headers",
                ));
            }
            buffer.extend_from_slice(&chunk[..count]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        let mut body = buffer.split_off(header_end + 4);
        if body.len() < content_length {
            let mut remainder = vec![0_u8; content_length - body.len()];
            self.read_exact(&mut remainder).await?;
            body.extend_from_slice(&remainder);
        } else {
            body.truncate(content_length);
        }
        Ok(body)
    }

    async fn write_json_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error> {
        let reason = if status < 400 { "OK" } else { "Bad Request" };
        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        self.write_all(headers.as_bytes()).await?;
        self.write_all(body.as_bytes()).await?;
        self.flush().await?;
        self.shutdown().await
    }

    async fn write_sse_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error> {
        let headers = format!(
            "HTTP/1.1 {status} OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
        );
        self.write_all(headers.as_bytes()).await?;
        self.write_all(body.as_bytes()).await?;
        self.flush().await?;
        self.shutdown().await
    }
}

type UpstreamResult = Result<Value, (u16, String)>;

impl ResolvedProvider {
    async fn forward_chat(&self, client: &reqwest::Client, request: Value) -> UpstreamResult {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&request.chat_completion_request())
            .send()
            .await
            .map_err(|error| (502, error.to_string()))?;
        response.read_upstream().await
    }

    async fn forward_anthropic(&self, client: &reqwest::Client, request: Value) -> UpstreamResult {
        let base = self.base_url.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        let response = client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request.anthropic_request())
            .send()
            .await
            .map_err(|error| (502, error.to_string()))?;
        response.read_upstream().await
    }
}

trait UpstreamResponse {
    async fn read_upstream(self) -> UpstreamResult;
}

impl UpstreamResponse for reqwest::Response {
    async fn read_upstream(self) -> UpstreamResult {
        let status = self.status().as_u16();
        let text = self
            .text()
            .await
            .map_err(|error| (502, error.to_string()))?;
        if !(200..300).contains(&status) {
            return Err((
                if status == 401 || status == 403 {
                    status
                } else {
                    502
                },
                text,
            ));
        }
        serde_json::from_str(&text)
            .map_err(|error| (502, format!("invalid upstream response: {error}")))
    }
}

struct RelayResponse;

impl RelayResponse {
    fn id() -> String {
        format!("resp_channel_{}", std::process::id())
    }
}

trait ResponsesProtocol {
    fn responses_sse(&self) -> String;
    fn responses_usage(&self) -> Value;
    fn chat_completion_request(&self) -> Value;
    fn responses_messages(&self) -> Vec<Value>;
    fn response_message_to_chat(&self) -> Option<Value>;
    fn responses_tool_to_chat(&self) -> Option<Value>;
    fn anthropic_request(&self) -> Value;
    fn responses_tool_to_anthropic(&self) -> Option<Value>;
    fn chat_completion_output(&self) -> Vec<Value>;
    fn anthropic_output(&self) -> Vec<Value>;
}

impl ResponsesProtocol for Value {
    fn responses_sse(&self) -> String {
        let output = if self.get("object").and_then(Value::as_str) == Some("chat.completion") {
            self.chat_completion_output()
        } else {
            self.anthropic_output()
        };
        let response = json!({
            "id": RelayResponse::id(),
            "object": "response",
            "status": "completed",
            "output": output,
            "usage": self.responses_usage(),
        });
        let events = [
            (
                "response.created",
                json!({ "type": "response.created", "response": response.clone() }),
            ),
            (
                "response.completed",
                json!({ "type": "response.completed", "response": response }),
            ),
        ];
        events
            .into_iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    fn responses_usage(&self) -> Value {
        if self.get("object").and_then(Value::as_str) == Some("chat.completion") {
            json!({
                "input_tokens": self.pointer("/usage/prompt_tokens"),
                "output_tokens": self.pointer("/usage/completion_tokens"),
            })
        } else {
            json!({
                "input_tokens": self.pointer("/usage/input_tokens"),
                "output_tokens": self.pointer("/usage/output_tokens"),
            })
        }
    }

    fn chat_completion_request(&self) -> Value {
        let mut body = json!({
            "model": self.get("model").cloned().unwrap_or_else(|| json!("default")),
            "messages": self.responses_messages(),
            "stream": false,
        });
        if let Some(tools) = self
            .get("tools")
            .and_then(Value::as_array)
            .filter(|tools| !tools.is_empty())
        {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .filter_map(ResponsesProtocol::responses_tool_to_chat)
                    .collect(),
            );
        }
        if let Some(choice) = self.get("tool_choice") {
            body["tool_choice"] = choice.clone();
        }
        if let Some(temperature) = self.get("temperature") {
            body["temperature"] = temperature.clone();
        }
        if let Some(tokens) = self.get("max_output_tokens") {
            body["max_tokens"] = tokens.clone();
        }
        body
    }

    fn responses_messages(&self) -> Vec<Value> {
        let mut messages = Vec::new();
        if let Some(instructions) = self.get("instructions").and_then(Value::as_str) {
            messages.push(json!({ "role": "system", "content": instructions }));
        }
        match self.get("input") {
            Some(Value::String(input)) => {
                messages.push(json!({ "role": "user", "content": input }));
            }
            Some(Value::Array(items)) => {
                for item in items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("message") | None => {
                            if let Some(message) = item.response_message_to_chat() {
                                messages.push(message);
                            }
                        }
                        Some("function_call") => {
                            let arguments = item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}");
                            messages.push(json!({
                                "role": "assistant",
                                "content": Value::Null,
                                "tool_calls": [{
                                    "id": item.get("call_id").or_else(|| item.get("id")),
                                    "type": "function",
                                    "function": {
                                        "name": item.get("name"),
                                        "arguments": arguments,
                                    },
                                }],
                            }));
                        }
                        Some("function_call_output") => {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": item.get("call_id"),
                                "content": item.get("output").cloned().unwrap_or(Value::String(String::new())),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        messages
    }

    fn response_message_to_chat(&self) -> Option<Value> {
        let role = self.get("role").and_then(Value::as_str)?;
        let content = self.get("content").map(|content| match content {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        });
        Some(json!({ "role": role, "content": content.unwrap_or_default() }))
    }

    fn responses_tool_to_chat(&self) -> Option<Value> {
        if self.get("type").and_then(Value::as_str) != Some("function") {
            return None;
        }
        let definition = self.get("function").unwrap_or(self);
        Some(json!({
            "type": "function",
            "function": {
                "name": definition.get("name")?,
                "description": definition.get("description").cloned().unwrap_or(Value::Null),
                "parameters": definition.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" })),
            }
        }))
    }

    fn anthropic_request(&self) -> Value {
        let mut messages = Vec::new();
        match self.get("input") {
            Some(Value::String(input)) => {
                messages.push(json!({ "role": "user", "content": input }))
            }
            Some(Value::Array(items)) => {
                for item in items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("function_call") => {
                            let input = serde_json::from_str(
                                item.get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("{}"),
                            )
                            .unwrap_or_else(|_| json!({}));
                            messages.push(json!({
                                "role": "assistant",
                                "content": [{
                                    "type": "tool_use",
                                    "id": item.get("call_id").or_else(|| item.get("id")),
                                    "name": item.get("name"),
                                    "input": input,
                                }]
                            }));
                        }
                        Some("function_call_output") => {
                            messages.push(json!({
                                "role": "user",
                                "content": [{
                                    "type": "tool_result",
                                    "tool_use_id": item.get("call_id"),
                                    "content": item.get("output").cloned().unwrap_or(Value::String(String::new())),
                                }]
                            }));
                        }
                        Some("message") | None => {
                            if let Some(message) = item.response_message_to_chat() {
                                messages.push(message);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let mut body = json!({
            "model": self.get("model").cloned().unwrap_or_else(|| json!("default")),
            "max_tokens": self.get("max_output_tokens").cloned().unwrap_or(json!(4096_u64)),
            "messages": messages,
        });
        if let Some(instructions) = self.get("instructions") {
            body["system"] = instructions.clone();
        }
        if let Some(tools) = self
            .get("tools")
            .and_then(Value::as_array)
            .filter(|tools| !tools.is_empty())
        {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .filter_map(ResponsesProtocol::responses_tool_to_anthropic)
                    .collect(),
            );
        }
        body
    }

    fn responses_tool_to_anthropic(&self) -> Option<Value> {
        if self.get("type").and_then(Value::as_str) != Some("function") {
            return None;
        }
        let definition = self.get("function").unwrap_or(self);
        Some(json!({
            "name": definition.get("name")?,
            "description": definition.get("description").cloned().unwrap_or(Value::Null),
            "input_schema": definition.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" })),
        }))
    }

    fn chat_completion_output(&self) -> Vec<Value> {
        let message = self
            .pointer("/choices/0/message")
            .cloned()
            .unwrap_or(Value::Null);
        let mut output = Vec::new();
        if let Some(text) = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            output.push(json!({
                "id": format!("msg_{}", RelayResponse::id()),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }));
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool in tool_calls {
                output.push(json!({
                    "id": tool.get("id").cloned().unwrap_or_else(|| json!(format!("fc_{}", RelayResponse::id()))),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": tool.get("id").cloned().unwrap_or_else(|| json!(format!("fc_{}", RelayResponse::id()))),
                    "name": tool.pointer("/function/name"),
                    "arguments": tool.pointer("/function/arguments").cloned().unwrap_or_else(|| json!("{}")),
                }));
            }
        }
        output
    }

    fn anthropic_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if let Some(content) = self.get("content").and_then(Value::as_array) {
            for part in content {
                match part.get("type").and_then(Value::as_str) {
                    Some("text")
                        if !part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .is_empty() =>
                    {
                        output.push(json!({
                            "id": format!("msg_{}", RelayResponse::id()),
                            "type": "message",
                            "status": "completed",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": part.get("text"), "annotations": [] }],
                        }));
                    }
                    Some("tool_use") => {
                        output.push(json!({
                            "id": part.get("id"),
                            "type": "function_call",
                            "status": "completed",
                            "call_id": part.get("id"),
                            "name": part.get("name"),
                            "arguments": serde_json::to_string(part.get("input").unwrap_or(&Value::Object(Default::default())))
                                .unwrap_or_else(|_| "{}".to_owned()),
                        }));
                    }
                    _ => {}
                }
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestDatabase(PathBuf);

    impl TestDatabase {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "channel-ccswitch-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute(
                    "CREATE TABLE providers (
                        id TEXT NOT NULL,
                        app_type TEXT NOT NULL,
                        name TEXT NOT NULL,
                        settings_config TEXT NOT NULL,
                        meta TEXT NOT NULL DEFAULT '{}',
                        is_current BOOLEAN NOT NULL DEFAULT 0,
                        created_at INTEGER,
                        sort_index INTEGER,
                        PRIMARY KEY (id, app_type)
                    )",
                    [],
                )
                .unwrap();
            Self(path)
        }

        fn insert(&self, id: &str, name: &str, sort_index: i64, meta: &str) {
            let connection = rusqlite::Connection::open(&self.0).unwrap();
            connection
                .execute(
                    "INSERT INTO providers (id, app_type, name, settings_config, meta, sort_index) \
                     VALUES (?1, 'codex', ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        id,
                        name,
                        r#"{"auth":{"OPENAI_API_KEY":"secret"},"config":"model = \"demo\"\n[model_providers.custom]\nbase_url = \"https://example.invalid/v1\"\n"}"#,
                        meta,
                        sort_index
                    ],
                )
                .unwrap();
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn lists_codex_keys_in_database_order() {
        let database = TestDatabase::new();
        database.insert("second", "beta", 2, "{}");
        database.insert("first", "alpha", 1, "{}");
        assert_eq!(
            CodexDatabase(database.0.clone()).available_keys().unwrap(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn resolves_exact_unique_names_only() {
        let database = TestDatabase::new();
        database.insert("one", "team", 1, r#"{"apiFormat":"openai_chat"}"#);
        assert_eq!(
            CodexDatabase(database.0.clone())
                .resolve_provider("team")
                .unwrap()
                .id,
            "one"
        );
        assert!(matches!(
            CodexDatabase(database.0.clone()).resolve_provider("missing"),
            Err(Error::InvalidConfig(message)) if message.contains("not found")
        ));
        database.insert("two", "team", 2, "{}");
        assert!(matches!(
            CodexDatabase(database.0.clone()).resolve_provider("team"),
            Err(Error::InvalidConfig(message)) if message.contains("ambiguous")
        ));
    }

    #[tokio::test]
    async fn skips_cc_switch_when_no_key_is_selected() {
        let config = crate::protocol::SessionConfig::for_kind(crate::protocol::HarnessKind::Codex);
        assert!(CodexResources::prepare(&config).await.unwrap().is_none());
    }

    #[test]
    fn parses_codex_provider_settings_without_storing_secrets() {
        let provider = ResolvedProvider::parse(
            "provider-id",
            "example",
            r#"{"auth":{"OPENAI_API_KEY":"secret"},"config":"model = \"demo\"\n[model_providers.custom]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"}"#,
            r#"{"apiFormat":"openai_chat"}"#,
        )
        .unwrap();
        assert_eq!(provider.id, "provider-id");
        assert_eq!(provider.api_format, "openai_chat");
        assert_eq!(provider.model, "demo");
        assert_eq!(provider.base_url, "https://example.invalid/v1");
    }

    #[test]
    fn rejects_oauth_only_settings() {
        assert!(matches!(
            ResolvedProvider::parse("id", "oauth", r#"{"auth":{"OPENAI_API_KEY":null},"config":"model = \"x\"\n"}"#, "{}"),
            Err(Error::UnsupportedCapability(message)) if message.contains("OAuth")
        ));
    }

    #[test]
    fn converts_chat_completion_to_responses_output() {
        let upstream = json!({
            "object": "chat.completion",
            "choices": [{"message": {"content": "done", "tool_calls": []}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1}
        });
        let output = upstream.chat_completion_output();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["content"][0]["text"], "done");
        assert_eq!(upstream.responses_usage()["input_tokens"], 2);
    }

    #[tokio::test]
    async fn relays_responses_requests_as_session_scoped_chat_completions() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let body = stream.read_request().await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let response = json!({
                "object": "chat.completion",
                "choices": [{"message": {"content": "relayed"}}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            });
            stream
                .write_json_response(200, &response.to_string())
                .await
                .unwrap();
            assert_eq!(request["messages"][0]["content"], "hello");
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider).await.unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}/responses", relay.endpoint))
            .json(&json!({
                "model": "demo",
                "input": "hello",
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = response.text().await.unwrap();
        assert!(body.contains("response.completed"));
        assert!(body.contains("relayed"));
    }
}
