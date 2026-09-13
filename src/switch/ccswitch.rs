use crate::protocol::Error;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

const DEFAULT_API_FORMAT: &str = "openai_responses";

/// cc-switch stores every supported app in one table; channel only reads the
/// Codex entries.
const APP_TYPE: &str = "codex";

/// Read-only mirror of the cc-switch `providers` table. Column names must
/// match the external schema exactly; columns the relay never reads
/// (`is_current`) are omitted so the generated SELECT stays minimal.
#[derive(Debug, Clone, ormer::Model)]
#[table = "providers"]
struct ProviderRow {
    #[primary]
    id: String,
    #[primary]
    app_type: String,
    name: String,
    settings_config: String,
    meta: String,
    created_at: Option<i64>,
    sort_index: Option<i64>,
}

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

    /// Connects to the cc-switch database. turso creates missing database
    /// files on open, but the cc-switch database is an external artifact, so
    /// a missing file is surfaced here instead of querying an empty store.
    async fn connect(&self) -> Result<ormer::Database, Error> {
        if !self.0.is_file() {
            return Err(Error::Initialization(format!(
                "failed to open cc-switch database {}: database file does not exist",
                self.0.display()
            )));
        }
        ormer::Database::connect(ormer::DbType::Sqlite, &self.0.to_string_lossy())
            .await
            .map_err(|error| {
                Error::Initialization(format!(
                    "failed to open cc-switch database {}: {error}",
                    self.0.display()
                ))
            })
    }

    pub(crate) async fn available_keys(&self) -> Result<Vec<String>, Error> {
        let connection = self.connect().await?;
        let rows = connection
            .select::<ProviderRow>()
            .filter(|provider| provider.app_type.eq(APP_TYPE))
            .order_by(|provider| provider.sort_index.asc())
            .order_by(|provider| provider.created_at.desc())
            .order_by(|provider| provider.id.asc())
            .collect::<Vec<_>>()
            .await
            .db_error("failed to read cc-switch providers")?;
        Ok(rows.into_iter().map(|row| row.name).collect())
    }

    pub(crate) async fn resolve_provider(&self, name: &str) -> Result<ResolvedProvider, Error> {
        let connection = self.connect().await?;
        let rows = connection
            .select::<ProviderRow>()
            .filter(|provider| provider.app_type.eq(APP_TYPE))
            .filter(|provider| provider.name.eq(name))
            .collect::<Vec<_>>()
            .await
            .db_error("failed to resolve cc-switch provider")?;
        let [row] = rows.as_slice() else {
            return Err(Error::InvalidConfig(if rows.is_empty() {
                format!("cc-switch provider was not found: {name}")
            } else {
                format!("cc-switch provider name is ambiguous: {name}")
            }));
        };
        ResolvedProvider::parse(&row.id, &row.name, &row.settings_config, &row.meta)
    }
}

trait DatabaseResultExt<T> {
    fn db_error(self, message: &'static str) -> Result<T, Error>;
}

impl<T> DatabaseResultExt<T> for ormer::Result<T> {
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

#[derive(Clone)]
struct CodexHome {
    path: std::sync::Arc<PathBuf>,
}

impl CodexHome {
    /// Uses a home directory that persists per switch key. Codex app-server
    /// performs a curated-plugin catalog sync on every startup and gates its
    /// `initialize` response on it; with an empty home that sync reaches
    /// github.com over flaky networks and can block for minutes. A persistent
    /// home keeps the synced catalog cache, so only the first startup pays
    /// the cost and later sessions start instantly.
    fn create(provider_name: &str) -> Result<Self, Error> {
        let database = CodexDatabase::path()?;
        let base = database
            .0
            .parent()
            .ok_or_else(|| {
                Error::Initialization("cannot locate the cc-switch data directory".to_owned())
            })?
            .join("channel-codex-homes");
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&provider_name, &mut hasher);
        let path = base.join(format!("{:016x}", std::hash::Hasher::finish(&hasher)));
        std::fs::create_dir_all(&path).map_err(|error| {
            Error::Initialization(format!(
                "failed to create the Codex home {}: {error}",
                path.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // create_dir_all has no mode parameter on stable; enforce 0o700
            // on the leaf afterwards so the home stays private.
            std::fs::set_permissions(&*path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    Error::Initialization(format!(
                        "failed to restrict the Codex home {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        // Codex rejects CODEX_HOME values containing 8.3 short path
        // components (e.g. FAWDLS~1), so hand it the canonical long path.
        let canonical = std::fs::canonicalize(&path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let canonical = canonical
            .strip_prefix(r"\\?\")
            .map(str::to_owned)
            .unwrap_or(canonical);
        Ok(Self {
            path: std::sync::Arc::new(PathBuf::from(canonical)),
        })
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
            std::fs::set_permissions(&*self.path, std::fs::Permissions::from_mode(0o700))
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

pub(crate) struct CodexResources {
    codex_home: CodexHome,
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
        let database = CodexDatabase::path()?;
        let provider_name = provider_name.to_owned();
        let provider = database.resolve_provider(&provider_name).await?;
        let codex_home = CodexHome::create(&provider_name)?;
        let relay = if provider.api_format == DEFAULT_API_FORMAT {
            codex_home.write(&provider, &provider.base_url)?;
            None
        } else {
            match provider.api_format.as_str() {
                "openai_chat" => {
                    let relay = Relay::start(&provider).await?;
                    let upstream = format!("{}/v1", relay.endpoint.trim_end_matches('/'));
                    let home = codex_home.clone();
                    let provider = provider.clone();
                    let upstream = upstream.clone();
                    tokio::task::spawn_blocking(move || home.write(&provider, &upstream))
                        .await
                        .map_err(|error| Error::Initialization(error.to_string()))??;
                    Some(relay)
                }
                "anthropic" => {
                    let relay = Relay::start(&provider).await?;
                    let upstream = relay.endpoint.trim_end_matches('/').to_owned();
                    let home = codex_home.clone();
                    let provider = provider.clone();
                    tokio::task::spawn_blocking(move || home.write(&provider, &upstream))
                        .await
                        .map_err(|error| Error::Initialization(error.to_string()))??;
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
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
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
        let provider = Arc::new(provider.clone());
        let connections = Arc::new(Mutex::new(Vec::new()));
        let permits = Arc::new(Semaphore::new(32));
        let accept_connections = connections.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let provider = provider.clone();
                let connections = accept_connections.clone();
                let permits = permits.clone();
                let connection = tokio::spawn(
                    RelayConnection {
                        stream,
                        provider,
                        permits,
                    }
                    .serve(),
                );
                connections
                    .lock()
                    .expect("relay connections")
                    .push(connection);
            }
        });
        Ok(Self {
            endpoint: format!("http://{address}"),
            task,
            connections,
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        let mut connections = self.connections.lock().expect("relay connections");
        let connections = connections.drain(..);
        for connection in connections {
            connection.abort();
        }
    }
}

/// 上游按非流式整段生成（推理模型一轮可能超过一分钟），总超时必须覆盖
/// 完整生成；健康请求被中途掐断后 codex 会全量重试，白烧上游额度。
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

struct RelayConnection {
    stream: TcpStream,
    provider: Arc<ResolvedProvider>,
    permits: Arc<Semaphore>,
}

impl RelayConnection {
    async fn serve(mut self) {
        let _permit = match self.permits.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let request: Result<Value, String> =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let body = self
                    .stream
                    .read_request("/responses")
                    .await
                    .map_err(|error| error.to_string())?;
                serde_json::from_slice(&body)
                    .map_err(|error| format!("invalid Responses request: {error}"))
            })
            .await
            .unwrap_or_else(|_| Err("relay request timed out".to_owned()));
        let request = match request {
            Ok(request) => request,
            Err(message) => {
                let _ = self.stream.write_json_response(400, &message).await;
                return;
            }
        };
        let mut request = request;
        if request.get("model").is_none() {
            request["model"] = Value::String(self.provider.model.clone());
        }
        // Fields the chat/anthropic conversions cannot represent are simply
        // dropped; newer Codex clients always send reasoning settings.
        request.strip_unsupported_responses_fields();
        let result = tokio::time::timeout(UPSTREAM_TIMEOUT, async {
            if self.provider.api_format == "openai_chat" {
                self.provider.forward_chat(request).await
            } else {
                self.provider.forward_anthropic(request).await
            }
        })
        .await
        .unwrap_or_else(|_| Err((502, "upstream request timed out".to_owned())));
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
    async fn read_request(&mut self, path: &str) -> Result<Vec<u8>, std::io::Error>;
    async fn write_json_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
    async fn write_sse_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
}

impl HttpConnection for TcpStream {
    async fn read_request(&mut self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        const MAX_HEADER_BYTES: usize = 16 * 1024;
        const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
        let header_end = loop {
            let count = self.read(&mut chunk).await?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "relay request ended before headers",
                ));
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.len() > MAX_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "relay request headers are too large",
                ));
            }
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut header_lines = headers.lines();
        let request_line = header_lines.next().unwrap_or_default();
        let (method, request_path) = {
            let mut parts = request_line.split_whitespace();
            (
                parts.next().unwrap_or_default(),
                parts.next().unwrap_or_default(),
            )
        };
        // Codex appends /responses to the configured base_url, which already
        // carries the /v1 prefix written into config.toml.
        if method != "POST" || !(request_path == path || request_path.ends_with(path)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "relay accepts only POST /responses",
            ));
        }
        if headers
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:"))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "chunked relay requests are not accepted",
            ));
        }
        let content_length = headers
            .lines()
            .find_map(|line| {
                if line.len() >= "content-length:".len()
                    && line[.."content-length:".len()].eq_ignore_ascii_case("content-length:")
                {
                    line["content-length:".len()..].trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "relay requests require content-length",
                )
            })?;
        if content_length == 0 || content_length > MAX_BODY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "relay request body size is invalid",
            ));
        }
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
            "HTTP/1.1 {status} OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
        );
        self.write_all(headers.as_bytes()).await?;
        // Chunked framing instead of a bare close-delimited body: HTTP/1.1
        // clients then know where the stream ends without relying on the
        // connection teardown (potato, unlike reqwest, refuses to guess).
        // The payload is rendered in one pass (non-streaming upstream), so a
        // single chunk plus the zero-chunk terminator carries it all.
        let mut chunk = format!("{:x}\r\n", body.len());
        chunk.push_str(body);
        chunk.push_str("\r\n0\r\n\r\n");
        self.write_all(chunk.as_bytes()).await?;
        self.flush().await?;
        self.shutdown().await
    }
}

type UpstreamResult = Result<Value, (u16, String)>;

impl ResolvedProvider {
    async fn forward_chat(&self, request: Value) -> UpstreamResult {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut response = potato::post_json(
            &url,
            request.chat_completion_request(),
            vec![potato::Headers::Custom((
                "Authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ))],
        )
        .await
        .map_err(|error| (502, error.to_string()))?;
        response.read_upstream().await
    }

    async fn forward_anthropic(&self, request: Value) -> UpstreamResult {
        let base = self.base_url.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        let mut response = potato::post_json(
            &url,
            request.anthropic_request(),
            vec![
                potato::Headers::Custom(("x-api-key".to_owned(), self.api_key.clone())),
                potato::Headers::Custom((
                    "anthropic-version".to_owned(),
                    "2023-06-01".to_owned(),
                )),
            ],
        )
        .await
        .map_err(|error| (502, error.to_string()))?;
        response.read_upstream().await
    }
}

trait UpstreamResponse {
    async fn read_upstream(&mut self) -> UpstreamResult;
}

impl UpstreamResponse for potato::HttpResponse {
    async fn read_upstream(&mut self) -> UpstreamResult {
        let status = self.http_code;
        let data = self.body.data().await;
        let text = String::from_utf8_lossy(data).into_owned();
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
    fn strip_unsupported_responses_fields(&mut self);
    fn responses_sse(&self) -> String;
    fn responses_usage(&self) -> Value;
    fn responses_status(&self) -> &'static str;
    fn responses_cached_input_tokens(&self) -> Value;
    fn responses_reasoning_tokens(&self) -> Value;
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
    fn strip_unsupported_responses_fields(&mut self) {
        if let Some(object) = self.as_object_mut() {
            for field in [
                "previous_response_id",
                "reasoning",
                "metadata",
                "parallel_tool_calls",
            ] {
                object.remove(field);
            }
        }
    }

    fn responses_sse(&self) -> String {
        let output = if self.get("object").and_then(Value::as_str) == Some("chat.completion") {
            self.chat_completion_output()
        } else {
            self.anthropic_output()
        };
        let response = json!({
            "id": RelayResponse::id(),
            "object": "response",
            "status": self.responses_status(),
            "output": output,
            "usage": self.responses_usage(),
        });
        // Codex only surfaces agent messages and tool calls from the
        // item-level events, so a bare created+completed pair yields an
        // empty turn even when the final response carries output.
        let mut events = vec![(
            "response.created",
            json!({ "type": "response.created", "response": response.clone() }),
        )];
        for (index, item) in output.iter().enumerate() {
            events.push((
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": index,
                    "item": item,
                }),
            ));
            let text = item
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if item.get("type").and_then(Value::as_str) == Some("message") {
                let part = json!({ "type": "output_text", "text": text, "annotations": [] });
                let item_id = item.get("id").cloned().unwrap_or(Value::Null);
                events.push((
                    "response.content_part.added",
                    json!({
                        "type": "response.content_part.added",
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "part": part,
                    }),
                ));
                events.push((
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
                events.push((
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "text": text,
                    }),
                ));
                events.push((
                    "response.content_part.done",
                    json!({
                        "type": "response.content_part.done",
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "part": part,
                    }),
                ));
            }
            events.push((
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": item,
                }),
            ));
        }
        events.push((
            "response.completed",
            json!({ "type": "response.completed", "response": response }),
        ));
        events
            .into_iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    fn responses_usage(&self) -> Value {
        let (input, output) =
            if self.get("object").and_then(Value::as_str) == Some("chat.completion") {
                (
                    self.pointer("/usage/prompt_tokens")
                        .cloned()
                        .unwrap_or(json!(0)),
                    self.pointer("/usage/completion_tokens")
                        .cloned()
                        .unwrap_or(json!(0)),
                )
            } else {
                (
                    self.pointer("/usage/input_tokens")
                        .cloned()
                        .unwrap_or(json!(0)),
                    self.pointer("/usage/output_tokens")
                        .cloned()
                        .unwrap_or(json!(0)),
                )
            };
        let tokens = match (&input, &output) {
            (Value::Number(input), Value::Number(output)) => input
                .as_u64()
                .zip(output.as_u64())
                .map(|(input, output)| json!(input + output)),
            _ => None,
        }
        .unwrap_or(json!(0));
        json!({
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": tokens,
            "cached_input_tokens": self.responses_cached_input_tokens(),
            "reasoning_tokens": self.responses_reasoning_tokens(),
        })
    }

    fn responses_status(&self) -> &'static str {
        if self.get("object").and_then(Value::as_str) == Some("chat.completion") {
            match self
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
            {
                Some("length") => "incomplete",
                Some("content_filter") | Some("failure") => "failed",
                _ => "completed",
            }
        } else {
            match self.get("stop_reason").and_then(Value::as_str) {
                Some("max_tokens") => "incomplete",
                Some("refusal") | Some("error") => "failed",
                _ => "completed",
            }
        }
    }

    fn responses_cached_input_tokens(&self) -> Value {
        self.pointer("/usage/prompt_tokens_details/cached_tokens")
            .or_else(|| self.pointer("/usage/cache_read_input_tokens"))
            .cloned()
            .unwrap_or(json!(0))
    }

    fn responses_reasoning_tokens(&self) -> Value {
        self.pointer("/usage/completion_tokens_details/reasoning_tokens")
            .or_else(|| self.pointer("/usage/reasoning_tokens"))
            .cloned()
            .unwrap_or(json!(0))
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
        // The Responses API may label instructions as "developer", which
        // chat-completions upstreams such as GLM reject outright.
        let role = if role == "developer" { "system" } else { role };
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
                                // Anthropic only accepts user/assistant turns here;
                                // system content belongs to the top-level field.
                                if message.get("role").and_then(Value::as_str) == Some("assistant")
                                {
                                    messages.push(message);
                                } else if let Some(content) =
                                    message.get("content").and_then(Value::as_str)
                                {
                                    messages.push(json!({ "role": "user", "content": content }));
                                }
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
        let mut output = Vec::new();
        if let Some(choices) = self.get("choices").and_then(Value::as_array) {
            for (choice_index, choice) in choices.iter().enumerate() {
                let message = choice.get("message").cloned().unwrap_or(Value::Null);
                let contents = match message.get("content") {
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                    Some(Value::String(text)) if !text.is_empty() => vec![text.clone()],
                    _ => Vec::new(),
                };
                for (part_index, text) in contents.into_iter().enumerate() {
                    output.push(json!({
                        "id": format!("msg_{}_{}_{}", RelayResponse::id(), choice_index, part_index),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    }));
                }
                if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for (tool_index, tool) in tool_calls.iter().enumerate() {
                        output.push(json!({
                            "id": tool.get("id").cloned().unwrap_or_else(|| json!(format!("fc_{}_{}_{}", RelayResponse::id(), choice_index, tool_index))),
                            "type": "function_call",
                            "status": "completed",
                            "call_id": tool.get("id").cloned().unwrap_or_else(|| json!(format!("fc_{}_{}_{}", RelayResponse::id(), choice_index, tool_index))),
                            "name": tool.pointer("/function/name"),
                            "arguments": tool.pointer("/function/arguments").cloned().unwrap_or_else(|| json!("{}")),
                        }));
                    }
                }
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
        async fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "channel-ccswitch-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let connection =
                ormer::Database::connect(ormer::DbType::Sqlite, &path.to_string_lossy())
                    .await
                    .unwrap();
            connection.create_table::<ProviderRow>().execute().await.unwrap();
            Self(path)
        }

        async fn insert(&self, id: &str, name: &str, sort_index: i64, meta: &str) {
            let connection =
                ormer::Database::connect(ormer::DbType::Sqlite, &self.0.to_string_lossy())
                    .await
                    .unwrap();
            connection
                .insert(&ProviderRow {
                    id: id.to_owned(),
                    app_type: APP_TYPE.to_owned(),
                    name: name.to_owned(),
                    settings_config: r#"{"auth":{"OPENAI_API_KEY":"secret"},"config":"model = \"demo\"\n[model_providers.custom]\nbase_url = \"https://example.invalid/v1\"\n"}"#.to_owned(),
                    meta: meta.to_owned(),
                    created_at: None,
                    sort_index: Some(sort_index),
                })
                .execute()
                .await
                .unwrap();
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn lists_codex_keys_in_database_order() {
        let database = TestDatabase::new().await;
        database.insert("second", "beta", 2, "{}").await;
        database.insert("first", "alpha", 1, "{}").await;
        assert_eq!(
            CodexDatabase(database.0.clone())
                .available_keys()
                .await
                .unwrap(),
            ["alpha", "beta"]
        );
    }

    #[tokio::test]
    async fn resolves_exact_unique_names_only() {
        let database = TestDatabase::new().await;
        database.insert("one", "team", 1, r#"{"apiFormat":"openai_chat"}"#).await;
        assert_eq!(
            CodexDatabase(database.0.clone())
                .resolve_provider("team")
                .await
                .unwrap()
                .id,
            "one"
        );
        assert!(matches!(
            CodexDatabase(database.0.clone())
                .resolve_provider("missing")
                .await,
            Err(Error::InvalidConfig(message)) if message.contains("not found")
        ));
        database.insert("two", "team", 2, "{}").await;
        assert!(matches!(
            CodexDatabase(database.0.clone()).resolve_provider("team").await,
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

    #[test]
    fn sse_stream_carries_item_level_events_codex_requires() {
        let upstream = json!({
            "object": "chat.completion",
            "choices": [{"message": {"content": "done"}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1}
        });
        let sse = upstream.responses_sse();
        for event in [
            "event: response.created",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
            "event: response.output_text.done",
            "event: response.content_part.done",
            "event: response.output_item.done",
            "event: response.completed",
        ] {
            assert!(sse.contains(event), "missing {event} in SSE stream");
        }
        assert!(sse.contains("\"delta\":\"done\""));
    }

    #[test]
    fn normalizes_developer_role_for_chat_upstreams() {
        let request = json!({
            "model": "demo",
            "instructions": "be brief",
            "input": [
                { "type": "message", "role": "developer", "content": "system prompt" },
                { "type": "message", "role": "user", "content": "hello" }
            ]
        });
        let messages = request.responses_messages();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[1]["content"], "system prompt");
        assert_eq!(messages[2]["role"], "user");
    }

    #[tokio::test]
    async fn relays_responses_requests_as_session_scoped_chat_completions() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let body = stream.read_request("/chat/completions").await.unwrap();
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
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({
                "model": "demo",
                "input": "hello",
                "stream": true
            }),
            vec![],
        )
        .await
        .unwrap();
        assert!((200..300).contains(&response.http_code));
        let data = response.body.data().await;
        let body = String::from_utf8_lossy(data);
        assert!(body.contains("response.completed"));
        assert!(body.contains("relayed"));
    }
}
