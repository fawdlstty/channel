//! Ollama chat protocol client and NDJSON emitter.

#[cfg(feature = "llm")]
use super::ChatMessage;
#[cfg(feature = "llm")]
use super::{
    check_model_against_list, check_stream_response, parse_json_body, read_checked_body,
    read_json_body, send_request, spawn_ndjson_stream, ClientCore, LlmModelInfo, SseAction,
    StreamChunk,
};
use crate::protocol::Error;
#[cfg(feature = "llm")]
use crate::protocol::ReasoningEffort;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "llm")]
const PROVIDER_ID: &str = "ollama";

/// A multi-turn client for the Ollama chat API (`POST {base_url}/api/chat`).
/// System prompts travel as regular `system` entries of `messages`, streaming
/// responses are newline-delimited JSON instead of SSE, and the model list
/// comes from `GET {base_url}/api/tags`. Ollama needs no Authorization
/// header, so `api_key` is accepted for interface parity but never sent; the
/// reasoning effort is likewise kept client-side and never put on the wire.
#[cfg(feature = "llm")]
pub struct OllamaClient {
    pub(crate) core: ClientCore,
}

#[cfg(feature = "llm")]
impl OllamaClient {
    /// Creates a client for `base_url` (for example `http://127.0.0.1:11434`).
    /// The API key is ignored by the protocol and kept only for interface
    /// parity with the other clients.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            core: ClientCore::new(base_url, api_key),
        }
    }

    /// Adds a system prompt; it is sent as a `system` message inside
    /// `messages`.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.core.set_system_prompt(prompt);
    }

    /// Sets the model after verifying that the endpoint lists it. Mirrors the
    /// potato semantics: a failing `list_models` skips the validation.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<(), Error> {
        let model = model.into();
        check_model_against_list(&model, &self.list_models().await)?;
        self.core.model = Some(model);
        Ok(())
    }

    /// The currently selected model, if any.
    pub fn model(&self) -> Option<&str> {
        self.core.model()
    }

    /// Sets or clears the reasoning effort. Ollama has no matching request
    /// field, so the value is only kept in the serialized client state.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.core.reasoning_effort = effort;
    }

    /// The currently configured reasoning effort.
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.core.reasoning_effort()
    }

    /// Returns a copy of the conversation history.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.core.messages()
    }

    /// Replaces the conversation history, keeping the already logged system
    /// messages.
    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.core.set_messages(messages);
    }

    /// Sends a message and returns the full assistant reply.
    pub async fn chat(&mut self, message: impl Into<String>) -> Result<String, Error> {
        self.core.push_user_message(message);
        let (url, body, headers) = self.build_request(false)?;
        let mut response = send_request(&mut self.core.session, &url, body, headers).await?;
        let text = read_checked_body(&mut response).await?;
        let content = parse_response(&text)?;
        self.core.push_assistant_message(content.clone());
        Ok(content)
    }

    /// Sends a message and streams the assistant reply. The message log's
    /// trailing assistant entry is updated with every increment.
    pub async fn chat_stream(
        &mut self,
        message: impl Into<String>,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>, Error> {
        self.core.push_user_message(message);
        let (url, body, headers) = self.build_request(true)?;
        let mut response = send_request(&mut self.core.session, &url, body, headers).await?;
        check_stream_response(&mut response).await?;
        Ok(spawn_ndjson_stream(
            response,
            self.core.messages.clone(),
            parse_ndjson_line,
        ))
    }

    /// Lists the models advertised by `GET {base_url}/api/tags`.
    pub async fn list_models(&mut self) -> Result<Vec<LlmModelInfo>, Error> {
        let url = format!("{}/api/tags", self.core.base_url);
        let mut response = self
            .core
            .session
            .get(&url, Vec::new())
            .await
            .map_err(|error| super::transport_error(&format!("request to {url} failed"), error))?;
        let json = read_json_body(&mut response).await?;
        let mut models = Vec::new();
        if let Some(data) = json["models"].as_array() {
            for item in data {
                let name = item["name"].as_str().unwrap_or_default();
                if !name.is_empty() {
                    models.push(LlmModelInfo {
                        id: name.to_owned(),
                        name: name.to_owned(),
                        provider_id: "ollama".to_owned(),
                    });
                }
            }
        }
        Ok(models)
    }

    /// Appends an assistant message to the history without contacting the
    /// endpoint.
    pub fn append_assistant_message(&mut self, content: impl Into<String>) {
        self.core.append_assistant_message(content);
    }

    /// Serializes the client state (protocol, endpoints, key, model,
    /// history and reasoning effort) to a JSON string.
    pub fn serialize(&self) -> Result<String, Error> {
        self.core.serialize_state(PROVIDER_ID)
    }

    /// Restores a client from [`OllamaClient::serialize`] output.
    pub fn deserialize(json: &str) -> Result<Self, Error> {
        Ok(Self {
            core: ClientCore::deserialize_state(PROVIDER_ID, json)?,
        })
    }

    /// Builds the endpoint URL, request body and headers. `stream` selects
    /// streaming mode. `post_json` adds the `Content-Type: application/json`
    /// header; Ollama uses no authentication, so no extra headers are set
    /// even when an API key is configured.
    pub(crate) fn build_request(
        &self,
        stream: bool,
    ) -> Result<(String, serde_json::Value, Vec<potato::Headers>), Error> {
        let url = format!("{}/api/chat", self.core.base_url);
        let messages: Vec<serde_json::Value> = self
            .core
            .read_messages()
            .iter()
            .map(|message| {
                serde_json::json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                })
            })
            .collect();
        let body = serde_json::json!({
            "model": self.core.ensure_model()?,
            "messages": messages,
            "stream": stream,
        });
        Ok((url, body, Vec::new()))
    }
}

/// Extracts `message.content` from a non-streaming response.
#[cfg(feature = "llm")]
pub(crate) fn parse_response(text: &str) -> Result<String, Error> {
    let json = parse_json_body(text)?;
    Ok(json["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Parses one NDJSON line of a streaming chat: `message.content` carries the
/// increment, `done: true` terminates the stream and an `error` field
/// reports a failure.
#[cfg(feature = "llm")]
pub(crate) fn parse_ndjson_line(line: &str) -> SseAction {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
        return SseAction::Ignore;
    };
    if let Some(error) = json.get("error") {
        let message = error.as_str().unwrap_or(line).to_owned();
        return SseAction::Error(message);
    }
    if json["done"].as_bool().unwrap_or(false) {
        return SseAction::Done;
    }
    match json["message"]["content"].as_str() {
        Some(content) => SseAction::Content(content.to_owned()),
        None => SseAction::Ignore,
    }
}

/// Formats `seconds`/`nanos` since the Unix epoch as an RFC 3339 UTC
/// timestamp, for example `2026-09-10T08:30:00.123456789Z`, without pulling
/// in chrono. The fractional part follows chrono's `AutoSi` style: 0, 3, 6
/// or 9 digits.
pub(crate) fn format_rfc3339(seconds: u64, nanos: u32) -> String {
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let secs_of_day = seconds % 86_400;
    let fraction = match nanos {
        0 => String::new(),
        n if n % 1_000_000 == 0 => format!(".{:03}", n / 1_000_000),
        n if n % 1_000 == 0 => format!(".{:06}", n / 1_000),
        n => format!(".{n:09}"),
    };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{fraction}Z",
        secs_of_day / 3_600,
        secs_of_day / 60 % 60,
        secs_of_day % 60
    )
}

/// The civil (year, month, day) for a day count relative to 1970-01-01,
/// using the standard civil-from-days conversion (valid for the whole
/// Gregorian calendar).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2), month, day)
}

/// Current wall-clock time formatted as an RFC 3339 UTC timestamp; used by
/// [`crate::OllamaSender`].
pub(crate) fn rfc3339_utc_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format_rfc3339(duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or_else(|_| format_rfc3339(0, 0))
}

/// Emits an Ollama NDJSON stream, for example to feed a potato-based HTTP
/// server. Unlike the SSE emitters, every frame is a single JSON object
/// terminated by a newline and the response advertises
/// `application/x-ndjson`.
pub struct OllamaSender {
    model: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl OllamaSender {
    /// Creates the sender, its buffering channel and the NDJSON response.
    /// No frame is emitted until [`OllamaSender::send`] is called.
    pub async fn new(
        model: impl Into<String>,
        buffer_size: usize,
    ) -> Result<(Self, potato::HttpResponse), Error> {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);
        let sender = Self {
            model: model.into(),
            tx,
        };
        let mut response = potato::HttpResponse::sse(rx);
        response.add_header("Content-Type".into(), "application/x-ndjson".into());
        Ok((sender, response))
    }

    /// Emits one content frame (`done: false`) carrying `message`.
    pub async fn send(&self, message: impl Into<String>) -> Result<(), Error> {
        let frame = serde_json::json!({
            "model": self.model,
            "created_at": rfc3339_utc_now(),
            "response": message.into(),
            "done": false,
        });
        self.send_frame(&frame).await
    }

    /// Emits one `/api/chat` content frame (`done: false`) carrying
    /// `message.content` (the `/api/generate` shape of [`OllamaSender::send`]
    /// uses a top-level `response` field instead).
    pub async fn send_chat(&self, content: impl Into<String>) -> Result<(), Error> {
        let frame = serde_json::json!({
            "model": self.model,
            "created_at": rfc3339_utc_now(),
            "message": {
                "role": "assistant",
                "content": content.into(),
            },
            "done": false,
        });
        self.send_frame(&frame).await
    }

    /// Emits the `/api/chat` finish frame: `done: true` with
    /// `done_reason: "stop"` and an empty assistant message.
    pub async fn send_chat_finish(&self) -> Result<(), Error> {
        let frame = serde_json::json!({
            "model": self.model,
            "created_at": rfc3339_utc_now(),
            "message": {
                "role": "assistant",
                "content": "",
            },
            "done": true,
            "done_reason": "stop",
        });
        self.send_frame(&frame).await
    }

    /// Emits the finish frame: `done: true` with `done_reason: "stop"` and
    /// an empty `response`.
    pub async fn send_finish(&self) -> Result<(), Error> {
        let frame = serde_json::json!({
            "model": self.model,
            "created_at": rfc3339_utc_now(),
            "response": "",
            "done": true,
            "done_reason": "stop",
        });
        self.send_frame(&frame).await
    }

    async fn send_frame(&self, frame: &serde_json::Value) -> Result<(), Error> {
        // Ollama streams newline-delimited JSON, not SSE event blocks.
        let payload = format!("{frame}\n");
        self.tx
            .send(payload.into_bytes())
            .await
            .map_err(|_| Error::Closed)
    }
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::super::{header_pairs, MessageLog};
    use super::*;
    use std::sync::{Arc, RwLock};

    fn client() -> OllamaClient {
        let mut client = OllamaClient::new("http://127.0.0.1:11434", Some("key".to_owned()));
        client.core.model = Some("llama3".to_owned());
        client
    }

    #[test]
    fn build_request_sends_no_auth_headers() {
        let client = client();
        let (url, body, headers) = client.build_request(false).unwrap();
        assert_eq!(url, "http://127.0.0.1:11434/api/chat");
        // Even with an API key configured, Ollama gets no Authorization
        // header; post_json adds Content-Type by itself.
        assert!(headers.is_empty());
        assert_eq!(header_pairs(&headers), Vec::<(String, String)>::new());
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
        for field in ["think", "reasoning_effort", "thinking"] {
            assert!(body.get(field).is_none());
        }
    }

    #[test]
    fn build_request_maps_history_with_system_and_streaming() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.core.push_assistant_message("hi".to_owned());
        client.core.push_user_message("again");
        client.set_reasoning_effort(Some(ReasoningEffort::High));

        let (_, body, _) = client.build_request(true).unwrap();
        assert_eq!(body["stream"], true);
        // The reasoning effort stays client-side and never reaches the wire.
        assert!(body.get("think").is_none());
        // Unlike anthropic-messages/openai-responses, the system entry is
        // not lifted out of the message array.
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "hi");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "again");
    }

    #[test]
    fn build_request_requires_a_model() {
        let client = OllamaClient::new("http://127.0.0.1:11434", None);
        assert!(matches!(
            client.build_request(false),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn parse_response_extracts_message_content() {
        let text = r#"{"message":{"role":"assistant","content":"hello"},"done":true}"#;
        assert_eq!(parse_response(text).unwrap(), "hello");
        assert_eq!(parse_response(r#"{"done":true}"#).unwrap(), "");
        assert!(matches!(
            parse_response("not json"),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn ndjson_parser_accumulates_deltas_and_stops_at_done() {
        let stream = "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\
                      {\"done\":true}\n";
        let mut buffer = stream.to_owned();
        let mut collected = String::new();
        let mut saw_done = false;
        while let Some(line) = super::super::next_ndjson_line(&mut buffer) {
            match parse_ndjson_line(&line) {
                SseAction::Content(content) => collected.push_str(&content),
                SseAction::Done => saw_done = true,
                SseAction::Error(message) => panic!("unexpected error: {message}"),
                SseAction::Ignore => {}
            }
        }
        assert_eq!(collected, "Hello");
        assert!(saw_done);
        assert!(buffer.is_empty());
    }

    #[test]
    fn ndjson_parser_reports_error_lines_and_ignores_junk() {
        assert_eq!(
            parse_ndjson_line("{\"error\":\"boom\"}"),
            SseAction::Error("boom".to_owned())
        );
        assert_eq!(
            parse_ndjson_line("{\"error\":{\"code\":500}}"),
            SseAction::Error("{\"error\":{\"code\":500}}".to_owned())
        );
        assert_eq!(parse_ndjson_line("not json"), SseAction::Ignore);
        assert_eq!(
            parse_ndjson_line("{\"message\":{\"role\":\"assistant\"},\"done\":false}"),
            SseAction::Ignore
        );
    }

    #[tokio::test]
    async fn ndjson_stream_reassembles_lines_split_across_chunks() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut response = potato::HttpResponse::new();
        response.http_code = 200;
        response.body = potato::HttpResponseBody::Stream(rx);
        let messages: MessageLog = Arc::new(RwLock::new(Vec::new()));
        let mut receiver = spawn_ndjson_stream(response, messages.clone(), parse_ndjson_line);

        // One NDJSON line torn apart by network chunk boundaries.
        tx.send(br#"{"message":{"role":"assist"#.to_vec())
            .await
            .unwrap();
        tx.send(br#"ant","content":"He"#.to_vec()).await.unwrap();
        tx.send(br#"l"},"done":false}"#.to_vec()).await.unwrap();
        tx.send(
            br#"
{"message":{"content":"lo"},"done":false}
{"done":tru"#
                .to_vec(),
        )
        .await
        .unwrap();
        tx.send(b"e}\n".to_vec()).await.unwrap();
        drop(tx);

        let mut collected = String::new();
        let mut chunks = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            match chunk {
                StreamChunk::Content(content) => {
                    collected.push_str(&content);
                    chunks.push(StreamChunk::Content(content));
                }
                StreamChunk::Error(message) => panic!("unexpected error: {message}"),
                StreamChunk::Done => {
                    chunks.push(StreamChunk::Done);
                    break;
                }
            }
        }
        assert_eq!(collected, "Hello");
        assert_eq!(chunks.len(), 3, "two deltas plus the terminator");
        assert_eq!(chunks.last(), Some(&StreamChunk::Done));

        // The background task kept the shared message log up to date.
        let history = messages.read().unwrap().clone();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, super::super::MessageRole::Assistant);
        assert_eq!(history[0].content, "Hello");
    }

    #[test]
    fn rfc3339_formats_known_timestamps() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00Z");
        // Leap day of a century leap year.
        assert_eq!(format_rfc3339(951_782_400, 0), "2000-02-29T00:00:00Z");
        assert_eq!(
            format_rfc3339(951_825_600, 500_000_000),
            "2000-02-29T12:00:00.500Z"
        );
        // Regular leap year.
        assert_eq!(
            format_rfc3339(1_709_164_800, 1_000),
            "2024-02-29T00:00:00.000001Z"
        );
        // 2100 is not a leap year: the day after 2100-02-28 is 2100-03-01.
        assert_eq!(
            format_rfc3339(4_107_542_400, 123_456_789),
            "2100-03-01T00:00:00.123456789Z"
        );
        assert_eq!(
            format_rfc3339(1_789_029_000, 123_456_789),
            "2026-09-10T08:30:00.123456789Z"
        );
        // AutoSi trims the fraction to 3 digits when only millis are set.
        assert_eq!(format_rfc3339(1, 20_000_000), "1970-01-01T00:00:01.020Z");
    }

    #[tokio::test]
    async fn ollama_sender_emits_content_and_finish_frames() {
        let (sender, mut response) = OllamaSender::new("llama3", 8).await.unwrap();
        assert_eq!(
            response.get_header("Content-Type"),
            Some("application/x-ndjson")
        );
        sender.send("Hello").await.unwrap();
        sender.send_finish().await.unwrap();
        drop(sender);

        let mut stream = response.body.stream_data();
        let mut payload = String::new();
        while let Some(chunk) = stream.next().await {
            payload.push_str(&String::from_utf8(chunk).unwrap());
        }

        // Two newline-terminated frames; no SSE "data:" prefix anywhere.
        let lines: Vec<&str> = payload.split('\n').collect();
        assert_eq!(lines.len(), 3, "two frames plus the trailing empty piece");
        assert_eq!(lines[2], "");
        assert!(payload.ends_with("}\n"));
        assert!(!payload.contains("data:"));

        let content_frame: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(content_frame["model"], "llama3");
        assert_eq!(content_frame["response"], "Hello");
        assert_eq!(content_frame["done"], false);
        let created_at = content_frame["created_at"].as_str().unwrap();
        assert!(created_at.ends_with('Z'), "got {created_at}");
        // Shape check: YYYY-MM-DDTHH:MM:SS with an optional fraction.
        let bytes = created_at.as_bytes();
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b'T');
        assert_eq!(bytes[13], b':');
        assert_eq!(bytes[16], b':');
        assert!(created_at.len() >= "1970-01-01T00:00:00Z".len());

        let finish_frame: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(finish_frame["model"], "llama3");
        assert_eq!(finish_frame["response"], "");
        assert_eq!(finish_frame["done"], true);
        assert_eq!(finish_frame["done_reason"], "stop");
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.set_reasoning_effort(Some(ReasoningEffort::Low));
        let json = client.serialize().unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"], "ollama");
        assert_eq!(value["base_url"], "http://127.0.0.1:11434");

        let restored = OllamaClient::deserialize(&json).unwrap();
        assert_eq!(restored.model(), Some("llama3"));
        assert_eq!(restored.reasoning_effort(), Some(ReasoningEffort::Low));
        assert_eq!(restored.messages(), client.messages());
        assert_eq!(restored.core.api_key.as_deref(), Some("key"));

        assert!(matches!(
            OllamaClient::deserialize(&json.replace(
                "\"provider\":\"ollama\"",
                "\"provider\":\"openai-responses\""
            )),
            Err(Error::InvalidConfig(_))
        ));
    }
}
