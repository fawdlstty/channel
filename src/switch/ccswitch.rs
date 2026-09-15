use crate::protocol::Error;
use potato::HttpResponseBody;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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
    ///
    /// Each session gets its own sub-home under the per-key root: config.toml
    /// carries the session-local relay port, and concurrent sessions on the
    /// same key used to overwrite each other's config (the last writer's port
    /// won, so killing one session's relay broke every concurrent one). The
    /// shared plugin cache (`skills`) is symlinked in so the warm start
    /// survives the per-session split.
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
        let root = base.join(format!("{:016x}", std::hash::Hasher::finish(&hasher)));
        std::fs::create_dir_all(&root).map_err(|error| {
            Error::Initialization(format!(
                "failed to create the Codex home {}: {error}",
                root.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // create_dir_all has no mode parameter on stable; enforce 0o700
            // on the leaf afterwards so the home stays private.
            std::fs::set_permissions(&*root, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    Error::Initialization(format!(
                        "failed to restrict the Codex home {}: {error}",
                        root.display()
                    ))
                },
            )?;
        }
        let path = root.join("sessions").join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&path).map_err(|error| {
            Error::Initialization(format!(
                "failed to create the Codex session home {}: {error}",
                path.display()
            ))
        })?;
        Self::prune_stale_sessions(&root.join("sessions"));
        #[cfg(unix)]
        {
            // 只共享只读性质的暖缓存目录；缺失时（首次使用该 key）codex 会在
            // 会话 home 里自行同步，下一次会话可把软链升到共享根上。
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name == "skills" && entry.path().is_dir() {
                        let _ = std::os::unix::fs::symlink(entry.path(), path.join("skills"));
                    }
                }
            }
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

    /// 会话 home 用完即弃但没人删；按修改时间惰性清理超过 48 小时的残留，
    /// 失败静默（下次会话再试）。
    fn prune_stale_sessions(sessions_dir: &Path) {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(48 * 3600));
        let Some(cutoff) = cutoff else { return };
        let Ok(entries) = std::fs::read_dir(sessions_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|modified| modified < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
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
                    let relay = Relay::start(&provider, config.upstream_retries()).await?;
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
                    let relay = Relay::start(&provider, config.upstream_retries()).await?;
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
    async fn start(provider: &ResolvedProvider, upstream_retries: u32) -> Result<Self, Error> {
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
                        upstream_retries,
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

/// openai_chat 流式上游只约束首字节等待（TTFB）；流建立后生成不设总时长上限，
/// 推理模型一轮可超过任何固定值，硬上限只会把健康请求掐断后迫使 codex 全量
/// 重试（白烧上游额度），真正的兜底是 codex 自身的流空闲超时和调用方看门狗。
/// anthropic 路径仍为非流式整段转发，该值继续充当其总超时。
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

struct RelayConnection {
    stream: TcpStream,
    provider: Arc<ResolvedProvider>,
    permits: Arc<Semaphore>,
    /// 上游“首字节前失败”的额外重试次数（总尝试 = 1 + 该值）。
    upstream_retries: u32,
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
        if self.provider.api_format == "openai_chat" {
            Self::serve_chat_stream(
                self.stream,
                self.provider,
                request,
                self.upstream_retries,
            )
            .await;
        } else {
            let result = tokio::time::timeout(UPSTREAM_TIMEOUT, async {
                self.provider.forward_anthropic(request).await
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

    /// openai_chat 上游按 SSE 流式转发：token 增量尽早到达 codex（思考/文本
    /// delta 会实时变成 app-server 通知，供调用方判活），也避免非流式整段
    /// 生成被上游网关按时长掐死——2026-09-15 溯源全渠道 "no session events
    /// for 480s" 的根因，即大上下文非流式请求每轮 90~120 秒被上游拒绝后
    /// codex 静默重试、stdout 全程零事件。
    ///
    /// 首字节阶段失败（上游排队超时/5xx/429 等）按会话配置的
    /// `upstream_retries` 额外重试；认证类 401/403 与其余 4xx 不重试。
    /// 重试耗尽后把“总尝试次数 + 最后错误”作为错误体返回，codex 会原样
    /// 上抛，调用方（如 ai-trace 的失败记录）因此能得知重试情况。
    async fn serve_chat_stream(
        mut stream: TcpStream,
        provider: Arc<ResolvedProvider>,
        request: Value,
        upstream_retries: u32,
    ) {
        let payload = request.chat_completion_request();
        let attempts = upstream_retries.saturating_add(1) as usize;
        let mut upstream: Option<UpstreamSse> = None;
        let mut last_error: Option<UpstreamOpenError> = None;
        let mut attempts_made: usize = 0;
        for attempt in 1..=attempts {
            attempts_made = attempt;
            match tokio::time::timeout(
                UPSTREAM_TIMEOUT,
                provider.open_chat_stream(payload.clone()),
            )
            .await
            {
                Ok(Ok(opened)) => {
                    upstream = Some(opened);
                    break;
                }
                Ok(Err(error)) => {
                    let retryable = error.retryable;
                    last_error = Some(error);
                    if !retryable {
                        break;
                    }
                }
                Err(_) => {
                    last_error = Some(UpstreamOpenError {
                        codex_status: 502,
                        retryable: true,
                        message: "upstream request timed out before first byte".to_owned(),
                    });
                }
            }
        }
        let Some(mut upstream) = upstream else {
            let error = last_error.expect("retry loop exited without success or error");
            let message =
                format!("{} (upstream attempts: {attempts_made}/{attempts})", error.message);
            let _ = stream.write_json_response(error.codex_status, &message).await;
            return;
        };
        if stream.write_sse_head().await.is_err() {
            return;
        }
        let mut converter = ChatStreamConverter::new();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(bytes) = upstream.next_chunk().await {
            buffer.extend_from_slice(&bytes);
            while let Some(frame) = take_sse_frame(&mut buffer) {
                for event in converter.feed_frame(&frame) {
                    if stream.write_sse_event(&event).await.is_err() {
                        // codex 已断开（会话结束或被看门狗终止），收尾无意义。
                        return;
                    }
                }
            }
        }
        // 流结束（[DONE]、EOF 或流内错误）后补齐收尾事件：关闭未完结条目、
        // 聚合的工具调用、以及终结状态。异常断流以 response.failed 收场，
        // codex 会判本轮失败并自行重试。
        for event in converter.finish() {
            let _ = stream.write_sse_event(&event).await;
        }
        let _ = stream.write_sse_finish().await;
    }
}

trait HttpConnection {
    async fn read_request(&mut self, path: &str) -> Result<Vec<u8>, std::io::Error>;
    async fn write_json_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
    async fn write_sse_response(&mut self, status: u16, body: &str) -> Result<(), std::io::Error>;
    async fn write_sse_head(&mut self) -> Result<(), std::io::Error>;
    async fn write_sse_event(&mut self, event: &str) -> Result<(), std::io::Error>;
    async fn write_sse_finish(&mut self) -> Result<(), std::io::Error>;
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

    /// 只发 SSE 响应头（流式路径随后逐事件写出）。
    async fn write_sse_head(&mut self) -> Result<(), std::io::Error> {
        let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        self.write_all(headers.as_bytes()).await?;
        self.flush().await
    }

    /// 以一个 HTTP chunk 发送一条 SSE 事件。HTTP chunk 边界与 SSE 帧无需对齐，
    /// codex 侧的 HTTP/SSE 解析会自行重组。
    async fn write_sse_event(&mut self, event: &str) -> Result<(), std::io::Error> {
        let frame = format!("{:x}\r\n{event}\r\n", event.len());
        self.write_all(frame.as_bytes()).await?;
        self.flush().await
    }

    /// 终止 chunked body（零长度块）并关闭写端。
    async fn write_sse_finish(&mut self) -> Result<(), std::io::Error> {
        self.write_all(b"0\r\n\r\n").await?;
        self.flush().await?;
        self.shutdown().await
    }
}

type UpstreamResult = Result<Value, (u16, String)>;

/// 打开流式上游的失败信息：codex 拿到的状态码（401/403 原样透传、其余归一
/// 为 502）、是否值得重试、以及供上层记录的原始错误文本。
struct UpstreamOpenError {
    codex_status: u16,
    retryable: bool,
    message: String,
}

impl ResolvedProvider {
    /// 打开流式 chat completions：返回时响应头已就绪，body 随后按 SSE 增量
    /// 读取。错误映射：401/403 原样透传且不重试（认证失败重试无意义），
    /// 408/429/5xx 归一为 502 且可重试（上游排队/过载，重试常能成功），
    /// 其余 4xx 归一为 502 但不重试（请求本身有问题，重试掩盖不了）。
    async fn open_chat_stream(
        &self,
        request: Value,
    ) -> Result<UpstreamSse, UpstreamOpenError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut response = potato::post_json(
            &url,
            request,
            vec![potato::Headers::Custom((
                "Authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            ))],
        )
        .await
        .map_err(|error| UpstreamOpenError {
            codex_status: 502,
            retryable: true,
            message: error.to_string(),
        })?;
        let status = response.http_code;
        if !(200..300).contains(&status) {
            let data = response.body.data().await;
            let text = String::from_utf8_lossy(data).into_owned();
            let retryable = status == 408 || status == 429 || (500..600).contains(&status);
            let codex_status = if status == 401 || status == 403 {
                status
            } else {
                502
            };
            return Err(UpstreamOpenError {
                codex_status,
                retryable,
                message: format!("unexpected upstream status {status}: {text}"),
            });
        }
        Ok(UpstreamSse {
            response,
            exhausted: false,
        })
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

/// 流式 chat completions 上游：`next_chunk` 逐段吐出原始字节，SSE 帧解析
/// 与协议转换交给 `ChatStreamConverter`。
struct UpstreamSse {
    response: potato::HttpResponse,
    exhausted: bool,
}

impl UpstreamSse {
    async fn next_chunk(&mut self) -> Option<Vec<u8>> {
        if self.exhausted {
            return None;
        }
        match &mut self.response.body {
            HttpResponseBody::Stream(rx) => rx.recv().await,
            HttpResponseBody::Data(data) => {
                // 未分块的 200 响应（异常上游）：整段一次性交付。
                self.exhausted = true;
                if data.is_empty() {
                    None
                } else {
                    Some(data.clone())
                }
            }
        }
    }
}

/// 从缓冲中取出下一个完整 SSE 帧（空行分隔，兼容 CRLF）。无完整帧时返回 None。
fn take_sse_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (position, skip) = match (lf, crlf) {
        (Some(a), Some(b)) if b < a => (b, 4),
        (Some(a), _) => (a, 2),
        (None, Some(b)) => (b, 4),
        (None, None) => return None,
    };
    let frame = String::from_utf8_lossy(&buffer[..position]).into_owned();
    buffer.drain(..position + skip);
    Some(frame)
}

/// 提取 SSE 帧中 data 行的负载（多行 data 按行拼接）；注释/心跳帧返回 None。
fn sse_data_payload(frame: &str) -> Option<String> {
    let mut payload: Option<String> = None;
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.strip_prefix(' ').unwrap_or(data);
        payload = Some(match payload {
            Some(existing) => format!("{existing}\n{data}"),
            None => data.to_owned(),
        });
    }
    payload
}

/// 聚合中的流式工具调用片段（按上游 tool_calls 数组 index 分桶）。
#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// 流式输出中的单条目（message / reasoning）。
struct StreamingItem {
    id: String,
    output_index: u64,
    text: String,
}

/// 把 chat completions SSE 增量转换为 codex（Responses wire API）的 SSE 事件：
/// 思考与文本内容逐 delta 外发——codex 由此实时产生 reasoning/agentMessage
/// delta 通知，调用方的无事件看门狗才能看到渠道活着；工具调用片段聚合为
/// 完整 function_call 条目，在收尾时整体下发。
struct ChatStreamConverter {
    response_id: String,
    created_sent: bool,
    reasoning: Option<StreamingItem>,
    message: Option<StreamingItem>,
    tools: BTreeMap<u64, ToolCallAccumulator>,
    next_output_index: u64,
    finish_reason: Option<String>,
    usage: Option<Value>,
    failed_message: Option<String>,
    saw_done: bool,
}

impl ChatStreamConverter {
    fn new() -> Self {
        Self {
            response_id: RelayResponse::id(),
            created_sent: false,
            reasoning: None,
            message: None,
            tools: BTreeMap::new(),
            next_output_index: 0,
            finish_reason: None,
            usage: None,
            failed_message: None,
            saw_done: false,
        }
    }

    fn event(&self, name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn response_value(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "status": status,
            "output": output,
            "usage": self.usage_value(),
        })
    }

    fn usage_value(&self) -> Value {
        let usage = self.usage.as_ref();
        let input = usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .cloned()
            .unwrap_or(json!(0));
        let output = usage
            .and_then(|usage| usage.get("completion_tokens"))
            .cloned()
            .unwrap_or(json!(0));
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
            "cached_input_tokens": usage
                .and_then(|usage| usage.pointer("/prompt_tokens_details/cached_tokens"))
                .cloned()
                .unwrap_or(json!(0)),
            "reasoning_tokens": usage
                .and_then(|usage| usage.pointer("/completion_tokens_details/reasoning_tokens"))
                .cloned()
                .unwrap_or(json!(0)),
        })
    }

    fn final_status(&self) -> &'static str {
        match self.finish_reason.as_deref() {
            Some("length") => "incomplete",
            Some("content_filter") | Some("failure") => "failed",
            _ => "completed",
        }
    }

    /// 消费一个上游 SSE 帧的 data 负载，返回应转发给 codex 的事件。
    fn feed_frame(&mut self, frame: &str) -> Vec<String> {
        let Some(payload) = sse_data_payload(frame) else {
            return Vec::new();
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            self.saw_done = true;
            return Vec::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        // 上游在流内直接下发错误（无 choices）：记为失败，由 finish 终结。
        if let Some(error) = chunk.get("error") {
            self.failed_message = Some(error.to_string());
            return Vec::new();
        }
        if self.failed_message.is_some() {
            return Vec::new();
        }
        let mut events = Vec::new();
        let delta = chunk.pointer("/choices/0/delta");
        if let Some(delta) = delta {
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !text.is_empty() {
                    events.extend(self.push_reasoning(text));
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    events.extend(self.push_message_text(text));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let tool = self.tools.entry(index).or_default();
                    if tool.id.is_none() {
                        tool.id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_owned);
                    }
                    if let Some(function) = call.get("function") {
                        if tool.name.is_none() {
                            tool.name = function
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|name| !name.is_empty())
                                .map(str::to_owned);
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            tool.arguments.push_str(arguments);
                        }
                    }
                }
            }
        }
        if let Some(finish) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(finish.to_owned());
        }
        if chunk
            .get("usage")
            .map(|usage| !usage.is_null())
            .unwrap_or(false)
        {
            self.usage = chunk.get("usage").cloned();
        }
        events
    }

    fn push_reasoning(&mut self, text: &str) -> Vec<String> {
        let mut events = Vec::new();
        if !self.created_sent {
            self.created_sent = true;
            events.push(self.event(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": self.response_value("in_progress", Vec::new()),
                }),
            ));
        }
        if self.reasoning.is_none() {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let id = format!("rs_{}", self.response_id);
            events.push(self.event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": { "id": id, "type": "reasoning", "summary": [] },
                }),
            ));
            self.reasoning = Some(StreamingItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let (id, output_index) = {
            let item = self.reasoning.as_ref().expect("reasoning item opened above");
            (item.id.clone(), item.output_index)
        };
        self.reasoning
            .as_mut()
            .expect("reasoning item opened above")
            .text
            .push_str(text);
        events.push(self.event(
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": id,
                "output_index": output_index,
                "summary_index": 0,
                "delta": text,
            }),
        ));
        events
    }

    fn push_message_text(&mut self, text: &str) -> Vec<String> {
        let mut events = Vec::new();
        if !self.created_sent {
            self.created_sent = true;
            events.push(self.event(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": self.response_value("in_progress", Vec::new()),
                }),
            ));
        }
        if self.message.is_none() {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let id = format!("msg_{}", self.response_id);
            events.push(self.event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "id": id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                }),
            ));
            events.push(self.event(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            ));
            self.message = Some(StreamingItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let (id, output_index) = {
            let item = self.message.as_ref().expect("message item opened above");
            (item.id.clone(), item.output_index)
        };
        self.message
            .as_mut()
            .expect("message item opened above")
            .text
            .push_str(text);
        events.push(self.event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text,
            }),
        ));
        events
    }

    /// 流结束后的收尾：关闭未完结条目、下发聚合的工具调用、发出终结状态。
    /// 正常路径以 `response.completed` 收场；上游流内报错或未以 [DONE] 结束
    /// （连接被截断）时以 `response.failed` 收场，codex 会判本轮失败并重试。
    fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        let mut output = Vec::new();
        if let Some(item) = self.reasoning.take() {
            let done = json!({
                "id": item.id,
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": item.text }],
            });
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": item.output_index,
                    "item": done,
                }),
            ));
            output.push(done);
        }
        if let Some(item) = self.message.take() {
            events.push(self.event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": item.id,
                    "output_index": item.output_index,
                    "content_index": 0,
                    "text": item.text,
                }),
            ));
            let done = json!({
                "id": item.id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": item.text, "annotations": [] }],
            });
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": item.output_index,
                    "item": done,
                }),
            ));
            output.push(done);
        }
        for (index, tool) in std::mem::take(&mut self.tools) {
            let output_index = self.next_output_index;
            self.next_output_index += 1;
            let call_id = tool
                .id
                .unwrap_or_else(|| format!("fc_{}_{}", self.response_id, index));
            let done = json!({
                "id": call_id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": tool.name.unwrap_or_default(),
                "arguments": if tool.arguments.is_empty() { "{}".to_owned() } else { tool.arguments },
            });
            events.push(self.event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": done,
                }),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": done,
                }),
            ));
            output.push(done);
        }
        if self.failed_message.is_some() || !self.saw_done {
            events.push(self.event(
                "response.failed",
                json!({
                    "type": "response.failed",
                    "response": self.response_value("failed", output),
                }),
            ));
        } else {
            events.push(self.event(
                "response.completed",
                json!({
                    "type": "response.completed",
                    "response": self.response_value(self.final_status(), output),
                }),
            ));
        }
        events
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
        // 流式是硬要求：非流式整段生成会被上游网关按时长掐死（2026-09-15
        // 事故），且 token 增量只有流式才能尽早到达 codex 供调用方判活。
        let mut body = json!({
            "model": self.get("model").cloned().unwrap_or_else(|| json!("default")),
            "messages": self.responses_messages(),
            "stream": true,
            "stream_options": { "include_usage": true },
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

    /// 假上游：接收一条 POST 请求后按 SSE 流式应答（单 chunk 携带全部帧）。
    async fn serve_sse_upstream(
        upstream: TcpListener,
        expect_path: &str,
        payloads: Vec<Value>,
        done: bool,
        truncate_before_done: bool,
    ) {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let body = stream.read_request(expect_path).await.unwrap();
        let _request: Value = serde_json::from_slice(&body).unwrap();
        let mut sse = String::new();
        for payload in &payloads {
            sse.push_str(&format!("data: {payload}\n\n"));
        }
        if done && !truncate_before_done {
            sse.push_str("data: [DONE]\n\n");
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        let framed = format!("{:x}\r\n{sse}\r\n", sse.len());
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(framed.as_bytes()).await.unwrap();
        if done && !truncate_before_done {
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        }
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
    }

    fn stream_chunk(delta: Value) -> Value {
        json!({ "choices": [{ "delta": delta }] })
    }

    #[tokio::test]
    async fn relays_responses_requests_as_streamed_chat_completions() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let upstream_task = tokio::spawn(serve_sse_upstream(
            upstream,
            "/chat/completions",
            vec![
                stream_chunk(json!({ "role": "assistant", "reasoning_content": "思考" })),
                stream_chunk(json!({ "content": "relayed" })),
                stream_chunk(json!({})),
                json!({
                    "choices": [{ "delta": {}, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 2 },
                }),
            ],
            true,
            false,
        ));

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
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
        assert!(body.contains("response.created"));
        assert!(body.contains("response.reasoning_summary_text.delta"));
        assert!(body.contains("\"delta\":\"relayed\""));
        assert!(body.contains("response.output_item.done"));
        assert!(body.contains("response.completed"));
        assert!(body.contains("\"input_tokens\":1"));
        assert!(body.contains("\"output_tokens\":2"));
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn relays_streamed_events_incrementally() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let (stage_tx, stage_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let body = stream.read_request("/chat/completions").await.unwrap();
            let _request: Value = serde_json::from_slice(&body).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let first = stream_chunk(json!({ "content": "he" }));
            let frame = format!("data: {first}\n\n");
            stream
                .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
            // 等调用方确认第一个 delta 已到达 relay 下游后再发后续帧，
            // 以此证明 relay 是增量转发而不是整段缓冲（sender 被释放即放行）。
            let _ = stage_rx.await;
            let rest = [
                stream_chunk(json!({ "content": "llo" })),
                json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            ];
            let mut sse = String::new();
            for payload in &rest {
                sse.push_str(&format!("data: {payload}\n\n"));
            }
            sse.push_str("data: [DONE]\n\n");
            stream
                .write_all(format!("{:x}\r\n{sse}\r\n0\r\n\r\n", sse.len()).as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        let mut streamed = response.body.stream_data();
        let mut seen = String::new();
        loop {
            let chunk = streamed.next().await.expect("stream ended before first delta");
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("response.output_text.delta") {
                break;
            }
        }
        assert!(seen.contains("\"delta\":\"he\""));
        assert!(!seen.contains("response.completed"));
        drop(stage_tx);
        while let Some(chunk) = streamed.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(seen.contains("\"delta\":\"llo\""));
        assert!(seen.contains("response.completed"));
    }

    #[tokio::test]
    async fn aggregates_streamed_tool_call_fragments() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let upstream_task = tokio::spawn(serve_sse_upstream(
            upstream,
            "/chat/completions",
            vec![
                stream_chunk(json!({ "reasoning_content": "想一想" })),
                stream_chunk(json!({ "tool_calls": [{
                    "index": 0,
                    "id": "call_x",
                    "type": "function",
                    "function": { "name": "shell", "arguments": "{\"cmd\"" }
                }] })),
                stream_chunk(json!({ "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": ":\"ls\"}" }
                }] })),
                json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
                json!({
                    "choices": [{ "delta": {}, "finish_reason": null }],
                    "usage": { "prompt_tokens": 3, "completion_tokens": 5,
                               "completion_tokens_details": { "reasoning_tokens": 2 } },
                }),
            ],
            true,
            false,
        ));

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        let data = response.body.data().await;
        let body = String::from_utf8_lossy(data);
        assert!(body.contains("response.reasoning_summary_text.delta"));
        assert!(body.contains("\"delta\":\"想一想\""));
        // 工具调用聚合为单条 function_call：added、done、completed.output 各一次。
        assert_eq!(body.matches("\"type\":\"function_call\"").count(), 3);
        assert!(body.contains("\"name\":\"shell\""));
        assert!(body.contains(r#""arguments":"{\"cmd\":\"ls\"}""#));
        assert!(body.contains("response.completed"));
        assert!(body.contains("\"reasoning_tokens\":2"));
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn auth_failures_pass_through_before_streaming() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let body = stream.read_request("/chat/completions").await.unwrap();
            let _request: Value = serde_json::from_slice(&body).unwrap();
            stream
                .write_json_response(401, "身份验证失败。")
                .await
                .unwrap();
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(response.http_code, 401);
        let data = response.body.data().await;
        assert!(String::from_utf8_lossy(data).contains("身份验证失败"));
    }

    #[tokio::test]
    async fn truncated_upstream_stream_fails_the_response() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let upstream_task = tokio::spawn(serve_sse_upstream(
            upstream,
            "/chat/completions",
            vec![stream_chunk(json!({ "content": "half" }))],
            true,
            true,
        ));

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        let data = response.body.data().await;
        let body = String::from_utf8_lossy(data);
        assert!(body.contains("\"delta\":\"half\""));
        assert!(body.contains("response.failed"));
        assert!(body.contains("response.output_text.delta"));
        upstream_task.await.unwrap();
    }

    #[test]
    fn sse_frames_parse_across_chunk_boundaries() {
        let mut buffer = b"data: {\"a\"".to_vec();
        assert!(take_sse_frame(&mut buffer).is_none());
        buffer.extend_from_slice(b"}\n\ndata: [DONE]\n\n");
        let first = take_sse_frame(&mut buffer).unwrap();
        assert_eq!(sse_data_payload(&first).unwrap(), "{\"a\"}");
        let second = take_sse_frame(&mut buffer).unwrap();
        assert_eq!(sse_data_payload(&second).unwrap(), "[DONE]");
        assert!(take_sse_frame(&mut buffer).is_none());
    }

    #[test]
    fn sse_data_payload_ignores_comments_and_crlf() {
        assert_eq!(
            sse_data_payload(": keep-alive\r\ndata: hello\r\n"),
            Some("hello".to_owned())
        );
        assert_eq!(sse_data_payload(": ping"), None);
        assert_eq!(
            // SSE 多行 data 的每一行都携带 data: 前缀。
            sse_data_payload("data: multi\ndata: notes"),
            Some("multi\nnotes".to_owned())
        );
    }

    #[test]
    fn converter_emits_created_then_deltas_then_completed() {
        let mut converter = ChatStreamConverter::new();
        let events = converter.feed_frame(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"推理\"}}]}",
        );
        // 首个 delta 需要先补发 created 与 reasoning 条目开档事件。
        assert_eq!(events.len(), 3);
        assert!(events[0].starts_with("event: response.created\n"));
        assert!(events[1].starts_with("event: response.output_item.added\n"));
        assert!(events[1].contains("\"type\":\"reasoning\""));
        assert!(events[2].starts_with("event: response.reasoning_summary_text.delta\n"));
        let events = converter.feed_frame(
            "data: {\"choices\":[{\"delta\":{\"content\":\"答案\"}}]}",
        );
        assert_eq!(events.len(), 3);
        assert!(events[0].starts_with("event: response.output_item.added\n"));
        assert!(events[1].starts_with("event: response.content_part.added\n"));
        assert!(events[2].starts_with("event: response.output_text.delta\n"));
        assert_eq!(
            converter.feed_frame("data: [DONE]"),
            Vec::<String>::new()
        );
        let finish = converter.finish();
        // reasoning 条目收尾 + 文本 done/条目收尾 + completed 终结。
        assert_eq!(finish.len(), 4);
        assert!(finish[0].starts_with("event: response.output_item.done\n"));
        assert!(finish[3].starts_with("event: response.completed\n"));
        assert!(finish[0].contains("summary_text"));
        assert!(finish[3].contains("\"text\":\"答案\""));
    }

    #[test]
    fn converter_marks_in_band_errors_and_truncations_as_failed() {
        let mut converter = ChatStreamConverter::new();
        assert!(converter
            .feed_frame("data: {\"error\":{\"message\":\"upstream overloaded\"}}")
            .is_empty());
        let finish = converter.finish();
        assert!(finish[0].starts_with("event: response.failed\n"));

        let mut truncated = ChatStreamConverter::new();
        truncated.feed_frame("data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}");
        let finish = truncated.finish();
        assert!(finish.last().unwrap().starts_with("event: response.failed\n"));
    }

    #[tokio::test]
    async fn retries_transient_upstream_failures_within_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = upstream.accept().await {
                let hit = hits_task.fetch_add(1, Ordering::SeqCst);
                let body = stream.read_request("/chat/completions").await.unwrap();
                let _request: Value = serde_json::from_slice(&body).unwrap();
                if hit == 0 {
                    // 第一次：模拟上游过载拒绝（可重试类 5xx）。
                    stream.write_json_response(503, "upstream busy").await.unwrap();
                    continue;
                }
                let mut sse = String::new();
                sse.push_str(&format!(
                    "data: {}\n\n",
                    json!({ "choices": [{ "delta": { "content": "ok" } }] })
                ));
                sse.push_str("data: [DONE]\n\n");
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                stream.write_all(head.as_bytes()).await.unwrap();
                stream
                    .write_all(format!("{:x}\r\n{sse}\r\n0\r\n\r\n", sse.len()).as_bytes())
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 2).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        assert!((200..300).contains(&response.http_code));
        let data = response.body.data().await;
        let body = String::from_utf8_lossy(data);
        assert!(body.contains("\"delta\":\"ok\""));
        assert!(body.contains("response.completed"));
        // 首次失败 + 重试一次成功：上游共被请求两次。
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausted_retries_report_attempt_count() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = upstream.accept().await {
                hits_task.fetch_add(1, Ordering::SeqCst);
                let body = stream.read_request("/chat/completions").await.unwrap();
                let _request: Value = serde_json::from_slice(&body).unwrap();
                stream
                    .write_json_response(503, "upstream busy")
                    .await
                    .unwrap();
            }
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        // 重试 2 次 → 总尝试 3 次；耗尽后 502 错误体携带尝试次数供调用方获知。
        let relay = Relay::start(&provider, 2).await.unwrap();
        let mut response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(response.http_code, 502);
        let data = response.body.data().await;
        let body = String::from_utf8_lossy(data);
        assert!(body.contains("upstream attempts: 3/3"), "body: {body}");
        assert!(body.contains("upstream busy"));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn auth_failures_are_not_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = upstream.accept().await {
                hits_task.fetch_add(1, Ordering::SeqCst);
                let body = stream.read_request("/chat/completions").await.unwrap();
                let _request: Value = serde_json::from_slice(&body).unwrap();
                stream.write_json_response(401, "身份验证失败。").await.unwrap();
            }
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "test-key".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 5).await.unwrap();
        let response = potato::post_json(
            &format!("{}/responses", relay.endpoint),
            json!({ "model": "demo", "input": "hello", "stream": true }),
            vec![],
        )
        .await
        .unwrap();
        // 认证失败不做无谓重试：原样透传 401 且上游只被请求一次。
        assert_eq!(response.http_code, 401);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// 端到端：真实 codex app-server 经 relay 对接伪 chat completions 流式
    /// 上游，验证思考/文本 delta 以 app-server 通知逐条到达 stdout（调用方
    /// 判活依赖的事件链）。需本机装有 codex，手动运行：
    /// `cargo test --features ccswitch --lib -- --ignored --nocapture codex_e2e`
    #[tokio::test]
    #[ignore = "spawns the real codex binary"]
    async fn codex_e2e_streams_deltas_through_relay() {
        let codex = std::env::var("CODEX_E2E_BIN").unwrap_or_else(|_| "codex".to_owned());
        // 伪 chat completions 上游：分多帧流式下发思考与文本。
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let body = stream.read_request("/chat/completions").await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["stream"], true);
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            stream.write_all(head.as_bytes()).await.unwrap();
            let frames = [
                stream_chunk(json!({ "role": "assistant", "reasoning_content": "先" })),
                stream_chunk(json!({ "reasoning_content": "推理一下" })),
                stream_chunk(json!({ "content": "E2E " })),
                stream_chunk(json!({ "content": "完成。" })),
                json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
                json!({ "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 9 } }),
            ];
            for frame in &frames {
                let payload = format!("data: {frame}\n\n");
                stream
                    .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            }
            stream.write_all(b"data: [DONE]\n\n").await.unwrap();
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let provider = ResolvedProvider {
            id: "id".to_owned(),
            name: "name".to_owned(),
            api_key: "sk-test".to_owned(),
            api_format: "openai_chat".to_owned(),
            model: "demo".to_owned(),
            base_url: format!("http://{upstream_address}"),
        };
        let relay = Relay::start(&provider, 0).await.unwrap();
        let relay_port = relay
            .endpoint
            .trim_start_matches("http://")
            .split(':')
            .next_back()
            .expect("relay port")
            .to_owned();

        let home = std::env::temp_dir().join(format!(
            "channel-codex-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "model = \"demo\"\nmodel_provider = \"channel_e2e\"\n\n\
                 [model_providers.channel_e2e]\nname = \"channel_e2e\"\n\
                 base_url = \"http://127.0.0.1:{relay_port}/v1\"\n\
                 wire_api = \"responses\"\nrequires_openai_auth = true\n"
            ),
        )
        .unwrap();

        let mut child = tokio::process::Command::new(&codex)
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", &home)
            // 插件目录同步在新 home 上会先走 git/HTTPS 探测；让它们毫秒级
            // 失败（git 全局配置改写 + 不可达代理），但放行回环接口。
            .env(
                "GIT_CONFIG_GLOBAL",
                home.join("gitconfig").to_string_lossy().to_string(),
            )
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn codex; install codex or set CODEX_E2E_BIN");
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(&mut stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if event_tx.send(message).is_err() {
                    break;
                }
            }
        });

        async fn send_json_line(stdin: &mut tokio::process::ChildStdin, line: String) {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(line.as_bytes()).await.unwrap();
            stdin.write_all(b"\n").await.unwrap();
            stdin.flush().await.unwrap();
        }

        send_json_line(
            &mut stdin,
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": { "name": "channel-e2e", "title": "channel-e2e", "version": "0.0.0" },
                    "capabilities": {},
                },
            })
            .to_string(),
        )
        .await;
        let mut notifications: Vec<(String, Value)> = Vec::new();
        let mut pending: Option<u64> = Some(1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            let message = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .expect("e2e timed out")
                .expect("codex closed stdout");
            let incoming_id = message.get("id").and_then(Value::as_u64);
            if message.get("method").is_none() {
                if incoming_id == pending {
                    if pending == Some(1) {
                        assert!(message.get("result").is_some(), "initialize failed: {message}");
                        send_json_line(
                            &mut stdin,
                            json!({ "method": "initialized", "params": {} }).to_string(),
                        )
                        .await;
                        send_json_line(
                            &mut stdin,
                            json!({
                                "id": 2,
                                "method": "thread/start",
                                "params": {
                                    "cwd": home.to_string_lossy(),
                                    "ephemeral": true,
                                    "sandbox": "danger-full-access",
                                },
                            })
                            .to_string(),
                        )
                        .await;
                        pending = Some(2);
                    } else if pending == Some(2) {
                        let thread_id = message
                            .pointer("/result/thread/id")
                            .and_then(Value::as_str)
                            .expect("thread/start result")
                            .to_owned();
                        send_json_line(
                            &mut stdin,
                            json!({
                                "id": 3,
                                "method": "turn/start",
                                "params": {
                                    "threadId": thread_id,
                                    "input": [{ "type": "text", "text": "请直接回复一句话。不要调用任何工具。" }],
                                    "model": "demo",
                                },
                            })
                            .to_string(),
                        )
                        .await;
                        pending = Some(3);
                    } else {
                        // turn/start 的响应已到，继续等通知流直到 turn/completed。
                        pending = None;
                    }
                }
                continue;
            }
            let method = message["method"].as_str().unwrap_or_default().to_owned();
            if message.get("id").is_some() {
                // 服务端请求（审批等）：E2E 里不应出现，直接拒绝。
                let id = incoming_id.unwrap();
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(
                        json!({ "id": id, "error": { "code": -32601, "message": "not supported" } })
                            .to_string()
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                stdin.write_all(b"\n").await.unwrap();
                continue;
            }
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let finished = method == "turn/completed";
            notifications.push((method.clone(), params));
            if finished {
                break;
            }
        }
        let _ = child.kill().await;

        let methods: Vec<&str> = notifications.iter().map(|(m, _)| m.as_str()).collect();
        let reasoning_methods: Vec<&&str> = methods
            .iter()
            .filter(|m| m.starts_with("item/reasoning/"))
            .collect();
        assert!(
            !reasoning_methods.is_empty(),
            "no reasoning delta notification; methods={methods:?}"
        );
        let agent_deltas: Vec<&(String, Value)> = notifications
            .iter()
            .filter(|(m, _)| m == "item/agentMessage/delta")
            .collect();
        assert!(
            agent_deltas.len() >= 2,
            "agent message should arrive as multiple deltas; methods={methods:?}"
        );
        let mut text = String::new();
        for (_, params) in &agent_deltas {
            text.push_str(params.get("delta").and_then(Value::as_str).unwrap_or_default());
        }
        assert_eq!(text, "E2E 完成。");
        let (last, _) = notifications.last().expect("turn/completed recorded");
        assert_eq!(last, "turn/completed");
        let _ = std::fs::remove_dir_all(&home);
    }
}
