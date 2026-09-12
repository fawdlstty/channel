//! Direct large-language-model protocol clients.
//!
//! This module bundles the four protocol implementations:
//! [`chat_completions`] (OpenAI Chat Completions), [`responses`] (OpenAI
//! Responses), [`messages`] (Anthropic Messages) and [`ollama`] (Ollama
//! chat), all enabled by the crate's `llm` feature. All clients reuse a
//! single [`potato::Session`] for their HTTP transport and surface
//! [`crate::Error`] instead of leaking transport error types.
//!
//! With the `local-safetensors-cpu` feature family the [`local`] submodule adds
//! in-process model inference ([`crate::LocalClient`]) and the local-model
//! HTTP server without further opt-ins.

#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
mod chat_completions;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
mod messages;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
mod ollama;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
mod responses;

#[cfg(feature = "local-safetensors-cpu")]
mod local;

#[cfg(feature = "llm")]
pub use chat_completions::ChatCompletionsClient;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub use chat_completions::OpenAISender;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub use messages::AnthropicSender;
#[cfg(feature = "llm")]
pub use messages::MessagesClient;
#[cfg(feature = "llm")]
pub use ollama::OllamaClient;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub use ollama::OllamaSender;
#[cfg(feature = "llm")]
pub use responses::ResponsesClient;

#[cfg(feature = "local-safetensors-cpu")]
pub use local::{GenerationParams, LoadOptions, LocalBackendKind, LocalClient, LocalModelMeta};
#[cfg(feature = "local-safetensors-cpu")]
pub use local::server::LocalLlmServer;

#[cfg(feature = "llm")]
use crate::protocol::{Error, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// The role of a chat message in a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single message in a conversation, stamped with a microsecond Unix
/// timestamp taken from [`SystemTime`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub ts_micros: i64,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            ts_micros: unix_micros(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content)
    }
}

/// One chunk of a streaming chat response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamChunk {
    /// An incremental piece of assistant text.
    Content(String),
    /// The stream or the protocol reported a failure.
    Error(String),
    /// The stream finished; no further chunks are sent.
    Done,
}

/// A model entry returned by [`crate::ChatCompletionsClient::list_models`] and
/// [`crate::ResponsesClient::list_models`]. Named `LlmModelInfo` because
/// [`crate::ModelInfo`] already describes harness-level model state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmModelInfo {
    pub id: String,
    pub name: String,
    pub provider_id: String,
}

/// Shared message log handed to background streaming tasks.
pub(crate) type MessageLog = Arc<RwLock<Vec<ChatMessage>>>;

/// The action extracted from one server-sent event block or one NDJSON
/// line.
#[cfg(feature = "llm")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SseAction {
    Content(String),
    Done,
    /// Only emitted by the openai-responses, anthropic-messages and ollama
    /// parsers.
    Error(String),
    Ignore,
}

/// Current wall-clock time as microseconds since the Unix epoch.
pub(crate) fn unix_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as i64)
        .unwrap_or(0)
}

/// Current wall-clock time as seconds since the Unix epoch; used by
/// [`crate::OpenAISender`].
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub(crate) fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Pops the next complete SSE event block (delimited by a blank line) from
/// `buffer`; incomplete trailing data stays buffered.
#[cfg(feature = "llm")]
pub(crate) fn next_sse_event(buffer: &mut String) -> Option<String> {
    let position = buffer.find("\n\n")?;
    let event = buffer[..position].to_owned();
    buffer.drain(..position + 2);
    Some(event)
}

/// The `data:` payload of an SSE event block, if present.
#[cfg(feature = "llm")]
pub(crate) fn sse_data_payload(event: &str) -> Option<&str> {
    event
        .lines()
        .find_map(|line| line.strip_prefix("data:").map(str::trim_start))
}

/// Pops the next complete NDJSON line (delimited by a single newline) from
/// `buffer`; incomplete trailing data stays buffered.
#[cfg(feature = "llm")]
pub(crate) fn next_ndjson_line(buffer: &mut String) -> Option<String> {
    let position = buffer.find('\n')?;
    let line = buffer[..position].to_owned();
    buffer.drain(..position + 1);
    Some(line)
}

/// Errors returned by potato's transport, mapped immediately so `anyhow`
/// never leaks into the public API.
#[cfg(feature = "llm")]
pub(crate) fn transport_error(context: &str, error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("{context}: {error}"))
}

/// The bearer-auth header set shared by the OpenAI-compatible protocols.
#[cfg(feature = "llm")]
pub(crate) fn bearer_headers(api_key: Option<&str>) -> Vec<potato::Headers> {
    let mut headers = Vec::new();
    if let Some(key) = api_key {
        headers.push(potato::Headers::Custom((
            "Authorization".to_owned(),
            format!("Bearer {key}"),
        )));
    }
    headers
}

/// Validates a streaming response without consuming its body: non-200
/// codes are drained and reported, 200 codes keep the stream untouched.
#[cfg(feature = "llm")]
pub(crate) async fn check_stream_response(
    response: &mut potato::HttpResponse,
) -> Result<(), Error> {
    if response.http_code != 200 {
        let data = response.body.data().await;
        return Err(Error::ProviderRejected(format!(
            "HTTP {}: {}",
            response.http_code,
            String::from_utf8_lossy(data)
        )));
    }
    Ok(())
}

/// Reads the whole response body and turns non-200 codes into
/// [`Error::ProviderRejected`].
#[cfg(feature = "llm")]
pub(crate) async fn read_checked_body(
    response: &mut potato::HttpResponse,
) -> Result<String, Error> {
    let data = response.body.data().await;
    let text = String::from_utf8_lossy(data).into_owned();
    if response.http_code != 200 {
        return Err(Error::ProviderRejected(format!(
            "HTTP {}: {}",
            response.http_code, text
        )));
    }
    Ok(text)
}

/// Parses a JSON response body, mapping failures to [`Error::ProtocolError`].
#[cfg(feature = "llm")]
pub(crate) fn parse_json_body(text: &str) -> Result<serde_json::Value, Error> {
    serde_json::from_str(text)
        .map_err(|error| Error::ProtocolError(format!("invalid JSON response: {error}")))
}

/// Reads the whole response body and parses it as JSON; used by the model
/// listing of the OpenAI-compatible protocols and of ollama.
#[cfg(feature = "llm")]
pub(crate) async fn read_json_body(
    response: &mut potato::HttpResponse,
) -> Result<serde_json::Value, Error> {
    let text = read_checked_body(response).await?;
    parse_json_body(&text)
}

/// Sends a request with `potato` and returns the raw response.
#[cfg(feature = "llm")]
pub(crate) async fn send_request(
    session: &mut potato::Session,
    url: &str,
    body: serde_json::Value,
    headers: Vec<potato::Headers>,
) -> Result<potato::HttpResponse, Error> {
    session
        .post_json(url, body, headers)
        .await
        .map_err(|error| transport_error(&format!("request to {url} failed"), error))
}

/// Shared conversation state used by every protocol client.
#[cfg(feature = "llm")]
pub(crate) struct ClientCore {
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) session: potato::Session,
    pub(crate) messages: MessageLog,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

#[cfg(feature = "llm")]
impl ClientCore {
    pub(crate) fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            model: None,
            session: potato::Session::new(),
            messages: Arc::new(RwLock::new(Vec::new())),
            reasoning_effort: None,
        }
    }

    pub(crate) fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.write_messages(|messages| {
            messages.push(ChatMessage::new(MessageRole::System, prompt));
        });
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(crate) fn ensure_model(&self) -> Result<&str, Error> {
        self.model.as_deref().ok_or_else(|| {
            Error::InvalidConfig("model is not set. Call set_model() first.".to_owned())
        })
    }

    pub(crate) fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    pub(crate) fn messages(&self) -> Vec<ChatMessage> {
        self.read_messages()
    }

    /// Replaces the conversation history, keeping the system messages that
    /// are already logged (matching the potato semantics).
    pub(crate) fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.write_messages(|history| {
            history.retain(|message| message.role == MessageRole::System);
            history.extend(messages);
        });
    }

    pub(crate) fn append_assistant_message(&mut self, content: impl Into<String>) {
        self.write_messages(|messages| {
            messages.push(ChatMessage::new(MessageRole::Assistant, content));
        });
    }

    pub(crate) fn push_user_message(&mut self, message: impl Into<String>) {
        self.write_messages(|messages| {
            messages.push(ChatMessage::new(MessageRole::User, message));
        });
    }

    pub(crate) fn push_assistant_message(&mut self, content: String) {
        self.write_messages(|messages| {
            messages.push(ChatMessage::new(MessageRole::Assistant, content));
        });
    }

    /// The first logged system prompt; lifted into protocol-specific fields
    /// by openai-responses and anthropic-messages.
    pub(crate) fn system_prompt(&self) -> Option<String> {
        self.read_messages()
            .into_iter()
            .find(|message| message.role == MessageRole::System)
            .map(|message| message.content)
    }

    pub(crate) fn read_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn write_messages(&self, write: impl FnOnce(&mut Vec<ChatMessage>)) {
        let mut messages = self
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write(&mut messages);
    }

    /// Serializes the protocol-independent part of the client state. The
    /// protocol client prepends its `provider` identifier.
    pub(crate) fn serialize_state(&self, provider: &str) -> Result<String, Error> {
        let state = serde_json::json!({
            "provider": provider,
            "base_url": self.base_url,
            "api_key": self.api_key,
            "model": self.model,
            "messages": self.read_messages(),
            "reasoning_effort": self.reasoning_effort.map(|effort| effort.as_str()),
        });
        serde_json::to_string(&state)
            .map_err(|error| Error::ProtocolError(format!("failed to serialize client: {error}")))
    }

    /// Restores the protocol-independent part of the client state; the
    /// `provider` field must match `expected`.
    pub(crate) fn deserialize_state(expected: &str, json: &str) -> Result<Self, Error> {
        let state: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| Error::ProtocolError(format!("invalid serialized client: {error}")))?;
        let provider = state
            .get("provider")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                Error::InvalidConfig("serialized client is missing the provider field".to_owned())
            })?;
        if provider != expected {
            return Err(Error::InvalidConfig(format!(
                "serialized client uses provider '{provider}' but this client implements '{expected}'"
            )));
        }
        let base_url = state
            .get("base_url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                Error::InvalidConfig("serialized client is missing base_url".to_owned())
            })?
            .to_owned();
        let api_key = state
            .get("api_key")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let model = state
            .get("model")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let messages: Vec<ChatMessage> = state
            .get("messages")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|error| Error::ProtocolError(format!("invalid serialized messages: {error}")))?
            .unwrap_or_default();
        let reasoning_effort = state
            .get("reasoning_effort")
            .and_then(|value| value.as_str())
            .map(parse_reasoning_effort)
            .transpose()?;

        Ok(Self {
            base_url,
            api_key,
            model,
            session: potato::Session::new(),
            messages: Arc::new(RwLock::new(messages)),
            reasoning_effort,
        })
    }
}

/// Parses a serialized reasoning effort identifier.
#[cfg(feature = "llm")]
pub(crate) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, Error> {
    match value {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::XHigh),
        "max" => Ok(ReasoningEffort::Max),
        other => Err(Error::ProtocolError(format!(
            "unknown reasoning effort '{other}'"
        ))),
    }
}

/// Records a streaming increment on the message log: the trailing assistant
/// message is updated in place, or a fresh one is appended.
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub(crate) fn record_assistant_delta(messages: &MessageLog, content: &str) {
    let mut history = messages
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if history
        .last()
        .is_some_and(|message| message.role == MessageRole::Assistant)
    {
        if let Some(last) = history.last_mut() {
            last.content = content.to_owned();
        }
    } else {
        history.push(ChatMessage::new(MessageRole::Assistant, content));
    }
}

/// Ensures a trailing assistant message exists once the stream is complete.
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub(crate) fn finalize_assistant_message(messages: &MessageLog, content: String) {
    let mut history = messages
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if history
        .last()
        .is_none_or(|message| message.role != MessageRole::Assistant)
    {
        history.push(ChatMessage::new(MessageRole::Assistant, content));
    }
}

/// Spawns the background task that turns an SSE response into a
/// [`StreamChunk`] channel while keeping the message log up to date.
#[cfg(feature = "llm")]
pub(crate) fn spawn_sse_stream(
    response: potato::HttpResponse,
    messages: MessageLog,
    parse_event: fn(&str) -> SseAction,
) -> tokio::sync::mpsc::Receiver<StreamChunk> {
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);
    tokio::spawn(async move {
        let mut response = response;
        let mut stream = response.body.stream_data();
        let mut buffer = String::new();
        let mut assistant_content = String::new();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(event) = next_sse_event(&mut buffer) {
                match parse_event(&event) {
                    SseAction::Content(content) => {
                        assistant_content.push_str(&content);
                        record_assistant_delta(&messages, &assistant_content);
                        if tx.send(StreamChunk::Content(content)).await.is_err() {
                            return;
                        }
                    }
                    SseAction::Done => {
                        finalize_assistant_message(&messages, assistant_content);
                        let _ = tx.send(StreamChunk::Done).await;
                        return;
                    }
                    SseAction::Error(message) => {
                        let _ = tx.send(StreamChunk::Error(message)).await;
                        let _ = tx.send(StreamChunk::Done).await;
                        return;
                    }
                    SseAction::Ignore => {}
                }
            }
        }
        // The server closed the stream without an explicit terminator.
        finalize_assistant_message(&messages, assistant_content);
        let _ = tx.send(StreamChunk::Done).await;
    });
    rx
}

/// Spawns the background task that turns an NDJSON response (one JSON
/// object per newline-delimited line, as spoken by the Ollama protocol)
/// into a [`StreamChunk`] channel while keeping the message log up to
/// date. Lines split across network chunks are reassembled first.
#[cfg(feature = "llm")]
pub(crate) fn spawn_ndjson_stream(
    response: potato::HttpResponse,
    messages: MessageLog,
    parse_line: fn(&str) -> SseAction,
) -> tokio::sync::mpsc::Receiver<StreamChunk> {
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);
    tokio::spawn(async move {
        let mut response = response;
        let mut stream = response.body.stream_data();
        let mut buffer = String::new();
        let mut assistant_content = String::new();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line) = next_ndjson_line(&mut buffer) {
                match parse_line(&line) {
                    SseAction::Content(content) => {
                        assistant_content.push_str(&content);
                        record_assistant_delta(&messages, &assistant_content);
                        if tx.send(StreamChunk::Content(content)).await.is_err() {
                            return;
                        }
                    }
                    SseAction::Done => {
                        finalize_assistant_message(&messages, assistant_content);
                        let _ = tx.send(StreamChunk::Done).await;
                        return;
                    }
                    SseAction::Error(message) => {
                        let _ = tx.send(StreamChunk::Error(message)).await;
                        let _ = tx.send(StreamChunk::Done).await;
                        return;
                    }
                    SseAction::Ignore => {}
                }
            }
        }
        // The server closed the stream without an explicit terminator.
        finalize_assistant_message(&messages, assistant_content);
        let _ = tx.send(StreamChunk::Done).await;
    });
    rx
}

/// Validates that `model` appears in the listed models, keeping potato's
/// lenient semantics: a failing `list_models` skips validation.
#[cfg(feature = "llm")]
pub(crate) fn check_model_against_list(
    model: &str,
    listed: &Result<Vec<LlmModelInfo>, Error>,
) -> Result<(), Error> {
    if let Ok(available) = listed {
        if !available.iter().any(|info| info.id == model) {
            return Err(Error::InvalidConfig(format!(
                "Model '{model}' is invalid. Use list_models() to get valid models"
            )));
        }
    }
    Ok(())
}

/// Flattens the headers used by the protocol clients into comparable
/// pairs. `potato::Headers` does not implement `Debug` or `PartialEq`, so
/// tests (and only tests) go through this helper.
#[cfg(all(test, feature = "llm"))]
pub(crate) fn header_pairs(headers: &[potato::Headers]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|header| match header {
            potato::Headers::Custom((name, value)) => (name.clone(), value.clone()),
            _ => (String::new(), String::new()),
        })
        .collect()
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::*;

    #[test]
    fn sse_event_splitter_handles_partial_and_multiple_events() {
        let mut buffer = String::new();
        assert_eq!(next_sse_event(&mut buffer), None);

        buffer.push_str("data: {\"a\":1}\n\ndata: {\"b\"");
        assert_eq!(
            next_sse_event(&mut buffer).as_deref(),
            Some("data: {\"a\":1}")
        );
        assert_eq!(next_sse_event(&mut buffer), None);

        buffer.push_str(":2}\n\n");
        assert_eq!(
            next_sse_event(&mut buffer).as_deref(),
            Some("data: {\"b\":2}")
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn sse_data_payload_reads_first_data_line() {
        assert_eq!(
            sse_data_payload("event: delta\ndata: hello\nid: 1"),
            Some("hello")
        );
        assert_eq!(sse_data_payload("event: delta"), None);
        assert_eq!(sse_data_payload("data:[DONE]"), Some("[DONE]"));
    }

    #[test]
    fn ndjson_line_splitter_handles_partial_and_multiple_lines() {
        let mut buffer = String::new();
        assert_eq!(next_ndjson_line(&mut buffer), None);

        buffer.push_str("{\"a\":1}\n{\"b\"");
        assert_eq!(next_ndjson_line(&mut buffer).as_deref(), Some("{\"a\":1}"));
        assert_eq!(next_ndjson_line(&mut buffer), None);

        buffer.push_str(":2}\n");
        assert_eq!(next_ndjson_line(&mut buffer).as_deref(), Some("{\"b\":2}"));
        assert!(buffer.is_empty());
    }

    #[test]
    fn chat_message_constructors_stamp_monotonic_timestamps() {
        let first = ChatMessage::user("hello");
        let second = ChatMessage::assistant("hi");
        assert_eq!(first.role, MessageRole::User);
        assert_eq!(second.role, MessageRole::Assistant);
        assert!(second.ts_micros >= first.ts_micros);
        let restored: ChatMessage =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(restored, first);
        // The wire format matches potato: role/ts_micros/content with the
        // variant name as the role identifier.
        let encoded = serde_json::to_value(ChatMessage::system("prompt")).unwrap();
        assert_eq!(encoded["role"], "System");
        assert_eq!(encoded["content"], "prompt");
        let decoded: ChatMessage =
            serde_json::from_str(r#"{"role":"System","ts_micros":42,"content":"prompt"}"#).unwrap();
        assert_eq!(decoded.role, MessageRole::System);
        assert_eq!(decoded.ts_micros, 42);
    }

    #[test]
    fn reasoning_effort_round_trips_all_levels() {
        for effort in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(parse_reasoning_effort(effort.as_str()).unwrap(), effort);
        }
        assert!(parse_reasoning_effort("disabled").is_err());
    }

    fn core_with_history() -> ClientCore {
        let mut core = ClientCore::new("https://example.invalid", None);
        core.set_system_prompt("be brief");
        core.push_user_message("hello");
        core.push_assistant_message("hi".to_owned());
        core
    }

    #[test]
    fn set_messages_keeps_existing_system_entries() {
        let mut core = core_with_history();
        core.set_messages(vec![ChatMessage::user("next")]);
        let messages = core.messages();
        // potato semantics: only the logged system entries survive, the
        // provided history replaces everything else.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[0].content, "be brief");
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(messages[1].content, "next");
    }

    #[test]
    fn append_assistant_message_appends_to_log() {
        let mut core = core_with_history();
        core.append_assistant_message("done");
        let messages = core.messages();
        assert_eq!(messages.last().unwrap().role, MessageRole::Assistant);
        assert_eq!(messages.last().unwrap().content, "done");
    }

    #[test]
    fn model_accessors_and_validation() {
        let core = ClientCore::new("https://example.invalid", None);
        assert!(matches!(
            core.ensure_model(),
            Err(Error::InvalidConfig(message)) if message.contains("set_model")
        ));
        assert!(check_model_against_list(
            "m1",
            &Ok(vec![LlmModelInfo {
                id: "m1".to_owned(),
                name: "m1".to_owned(),
                provider_id: "openai".to_owned(),
            }])
        )
        .is_ok());
        assert!(matches!(
            check_model_against_list("m2", &Ok(vec![])),
            Err(Error::InvalidConfig(message)) if message.contains("m2")
        ));
        // A failing list_models skips validation entirely.
        assert!(check_model_against_list(
            "m2",
            &Err(Error::UnsupportedCapability("no list".to_owned()))
        )
        .is_ok());
    }

    #[test]
    fn serialize_state_round_trips_via_chat_completions_format() {
        let mut core = core_with_history();
        core.model = Some("gpt-test".to_owned());
        core.reasoning_effort = Some(ReasoningEffort::High);
        let json = core.serialize_state("openai-chat-completions").unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"], "openai-chat-completions");
        assert_eq!(value["base_url"], "https://example.invalid");
        assert_eq!(value["model"], "gpt-test");
        assert_eq!(value["reasoning_effort"], "high");
        assert_eq!(value["messages"].as_array().unwrap().len(), 3);

        let restored = ClientCore::deserialize_state("openai-chat-completions", &json).unwrap();
        assert_eq!(restored.base_url, core.base_url);
        assert_eq!(restored.model.as_deref(), Some("gpt-test"));
        assert_eq!(restored.reasoning_effort(), Some(ReasoningEffort::High));
        assert_eq!(restored.messages(), core.messages());

        assert!(matches!(
            ClientCore::deserialize_state("anthropic-messages", &json),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            ClientCore::deserialize_state("openai-chat-completions", "{\"provider\":1}"),
            Err(Error::InvalidConfig(_))
        ));
        let legacy = json.replace(
            "\"reasoning_effort\":\"high\"",
            "\"reasoning_effort\":\"bogus\"",
        );
        assert!(matches!(
            ClientCore::deserialize_state("openai-chat-completions", &legacy),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn record_and_finalize_assistant_history() {
        let log: MessageLog = Arc::new(RwLock::new(Vec::new()));
        record_assistant_delta(&log, "Hel");
        record_assistant_delta(&log, "Hello");
        assert_eq!(log.read().unwrap().len(), 1);
        assert_eq!(log.read().unwrap()[0].content, "Hello");
        finalize_assistant_message(&log, "unused".to_owned());
        assert_eq!(log.read().unwrap().len(), 1);

        let empty: MessageLog = Arc::new(RwLock::new(Vec::new()));
        finalize_assistant_message(&empty, String::new());
        assert_eq!(empty.read().unwrap().len(), 1);
    }
}
