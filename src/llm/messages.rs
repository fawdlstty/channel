//! Anthropic Messages protocol client and SSE emitter.

#[cfg(feature = "llm")]
use super::{
    check_model_against_list, check_stream_response, parse_json_body, read_checked_body,
    send_request, spawn_sse_stream, ClientCore, LlmModelInfo, SseAction, StreamChunk,
};
#[cfg(feature = "llm")]
use super::{ChatMessage, MessageRole};
use crate::protocol::Error;
#[cfg(feature = "llm")]
use crate::protocol::ReasoningEffort;

#[cfg(feature = "llm")]
const PROVIDER_ID: &str = "anthropic-messages";

/// A multi-turn client for the Anthropic Messages API
/// (`POST {base_url}/messages`). System prompts travel as the top-level
/// `system` field and the request always pins `anthropic-version`.
#[cfg(feature = "llm")]
pub struct MessagesClient {
    pub(crate) core: ClientCore,
}

#[cfg(feature = "llm")]
impl MessagesClient {
    /// Creates a client for `base_url` (for example
    /// `https://api.anthropic.com`). The API key is sent both as a bearer
    /// token and as `x-api-key`.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            core: ClientCore::new(base_url, api_key),
        }
    }

    /// Adds a system prompt; it is sent as the top-level `system` field.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.core.set_system_prompt(prompt);
    }

    /// Sets the model. Anthropic does not expose a standard model list API
    /// through this client, so `list_models` fails and the lenient potato
    /// semantics store the value without validation.
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

    /// Sets or clears the reasoning effort.
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
        Ok(spawn_sse_stream(
            response,
            self.core.messages.clone(),
            parse_sse_event,
        ))
    }

    /// Anthropic does not expose a standard model list API via this client.
    pub async fn list_models(&mut self) -> Result<Vec<LlmModelInfo>, Error> {
        Err(Error::UnsupportedCapability(
            "anthropic does not expose a standard model list API via this client".to_owned(),
        ))
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

    /// Restores a client from [`MessagesClient::serialize`] output.
    pub fn deserialize(json: &str) -> Result<Self, Error> {
        Ok(Self {
            core: ClientCore::deserialize_state(PROVIDER_ID, json)?,
        })
    }

    /// Builds the endpoint URL, request body and headers. `stream` selects
    /// streaming mode.
    pub(crate) fn build_request(
        &self,
        stream: bool,
    ) -> Result<(String, serde_json::Value, Vec<potato::Headers>), Error> {
        let url = format!("{}/messages", self.core.base_url);
        let mut headers = Vec::new();
        if let Some(key) = self.core.api_key.as_deref() {
            headers.push(potato::Headers::Custom((
                "Authorization".to_owned(),
                format!("Bearer {key}"),
            )));
            headers.push(potato::Headers::Custom((
                "x-api-key".to_owned(),
                key.to_owned(),
            )));
        }
        headers.push(potato::Headers::Custom((
            "anthropic-version".to_owned(),
            "2023-06-01".to_owned(),
        )));

        let messages: Vec<serde_json::Value> = self
            .core
            .read_messages()
            .iter()
            .filter(|message| message.role != MessageRole::System)
            .map(|message| {
                serde_json::json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "model": self.core.ensure_model()?,
            "messages": messages,
            "max_tokens": 4096,
            "stream": stream,
        });
        if let Some(system) = self.core.system_prompt() {
            body["system"] = serde_json::Value::String(system);
        }
        match self.core.reasoning_effort {
            Some(effort) => {
                body["thinking"] = serde_json::json!({"type": "enabled"});
                body["output_config"] = serde_json::json!({"effort": effort.as_str()});
            }
            None => {
                body["thinking"] = serde_json::json!({"type": "disabled"});
            }
        }
        Ok((url, body, headers))
    }
}

/// Concatenates the `text` blocks of the response `content[]`.
#[cfg(feature = "llm")]
pub(crate) fn parse_response(text: &str) -> Result<String, Error> {
    let json = parse_json_body(text)?;
    let mut result = String::new();
    if let Some(content) = json["content"].as_array() {
        for block in content {
            if block["type"].as_str() == Some("text") {
                if let Some(text) = block["text"].as_str() {
                    result.push_str(text);
                }
            }
        }
    }
    Ok(result)
}

/// Parses one SSE event block of a streaming Messages request.
#[cfg(feature = "llm")]
pub(crate) fn parse_sse_event(event: &str) -> SseAction {
    let Some(data) = super::sse_data_payload(event) else {
        return SseAction::Ignore;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return SseAction::Ignore;
    };
    match json["type"].as_str().unwrap_or_default() {
        "content_block_delta" => match json["delta"]["text"].as_str() {
            Some(text) => SseAction::Content(text.to_owned()),
            None => SseAction::Ignore,
        },
        "message_stop" => SseAction::Done,
        "error" => {
            let message = json["error"]["message"].as_str().unwrap_or(data).to_owned();
            SseAction::Error(message)
        }
        _ => SseAction::Ignore,
    }
}

/// Emits an Anthropic Messages SSE stream, for example to feed a
/// potato-based HTTP server.
pub struct AnthropicSender {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl AnthropicSender {
    /// Creates the sender, its buffering channel and the SSE response. The
    /// `message_start` and `content_block_start` frames are emitted
    /// immediately.
    pub async fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        role: impl Into<String>,
        buffer_size: usize,
    ) -> Result<(Self, potato::HttpResponse), Error> {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);
        let sender = Self { tx };

        let message_start = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": id.into(),
                "type": "message",
                "role": role.into(),
                "model": model.into(),
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            }
        });
        sender.send_event("message_start", &message_start).await?;

        let content_block_start = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "text",
                "text": ""
            }
        });
        sender
            .send_event("content_block_start", &content_block_start)
            .await?;

        Ok((sender, potato::HttpResponse::sse(rx)))
    }

    /// Emits one `content_block_delta` frame with the given text.
    pub async fn send(&self, message: impl Into<String>) -> Result<(), Error> {
        let delta = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "text": message.into()
            }
        });
        self.send_event("content_block_delta", &delta).await
    }

    /// Emits the closing sequence: `content_block_stop`, `message_delta`
    /// (with `end_turn`) and `message_stop`.
    pub async fn send_finish(&self) -> Result<(), Error> {
        let content_block_stop = serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        });
        self.send_event("content_block_stop", &content_block_stop)
            .await?;

        let message_delta = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "stop_sequence": null
            },
            "usage": {
                "output_tokens": 0
            }
        });
        self.send_event("message_delta", &message_delta).await?;

        let message_stop = serde_json::json!({
            "type": "message_stop"
        });
        self.send_event("message_stop", &message_stop).await
    }

    async fn send_event(&self, name: &str, payload: &serde_json::Value) -> Result<(), Error> {
        let text = serde_json::to_string(payload).map_err(|error| {
            Error::ProtocolError(format!("failed to encode SSE event: {error}"))
        })?;
        let frame = format!("event: {name}\ndata: {text}\n\n");
        self.tx
            .send(frame.into_bytes())
            .await
            .map_err(|_| Error::Closed)
    }
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::*;

    fn client() -> MessagesClient {
        let mut client = MessagesClient::new("https://example.invalid", Some("key".to_owned()));
        client.core.model = Some("claude-test".to_owned());
        client
    }

    #[test]
    fn build_request_sends_version_and_key_headers() {
        let client = client();
        let (url, body, headers) = client.build_request(false).unwrap();
        assert_eq!(url, "https://example.invalid/messages");
        assert_eq!(
            super::super::header_pairs(&headers),
            vec![
                ("Authorization".to_owned(), "Bearer key".to_owned()),
                ("x-api-key".to_owned(), "key".to_owned()),
                ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
            ]
        );
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], false);
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body.get("system").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn build_request_without_key_only_sends_version() {
        let mut client = MessagesClient::new("https://example.invalid", None);
        client.core.model = Some("claude-test".to_owned());
        let (_, _, headers) = client.build_request(true).unwrap();
        assert_eq!(
            super::super::header_pairs(&headers),
            vec![("anthropic-version".to_owned(), "2023-06-01".to_owned())]
        );
    }

    #[test]
    fn build_request_lifts_system_and_enables_thinking() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.core.push_assistant_message("hi".to_owned());
        client.set_reasoning_effort(Some(ReasoningEffort::Medium));

        let (_, body, _) = client.build_request(true).unwrap();
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
        assert_eq!(
            body["output_config"],
            serde_json::json!({"effort": "medium"})
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "hi");
    }

    #[test]
    fn build_request_requires_a_model() {
        let client = MessagesClient::new("https://example.invalid", None);
        assert!(matches!(
            client.build_request(false),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn parse_response_concatenates_text_blocks() {
        let text = r#"{"content":[
            {"type":"text","text":"Hel"},
            {"type":"tool_use","id":"t1"},
            {"type":"text","text":"lo"}
        ]}"#;
        assert_eq!(parse_response(text).unwrap(), "Hello");
        assert_eq!(parse_response(r#"{"content":[]}"#).unwrap(), "");
        assert!(matches!(
            parse_response("not json"),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn sse_parser_dispatches_on_event_type() {
        assert_eq!(
            parse_sse_event("data: {\"type\":\"message_start\",\"message\":{}}"),
            SseAction::Ignore
        );
        assert_eq!(
            parse_sse_event(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hel\"}}"
            ),
            SseAction::Content("Hel".to_owned())
        );
        assert_eq!(
            parse_sse_event(
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"other\"}}"
            ),
            SseAction::Ignore
        );
        assert_eq!(
            parse_sse_event("data: {\"type\":\"message_stop\"}"),
            SseAction::Done
        );
        assert_eq!(
            parse_sse_event(
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded\",\"message\":\"boom\"}}"
            ),
            SseAction::Error("boom".to_owned())
        );
    }

    #[test]
    fn sse_stream_end_to_end_accumulation() {
        let stream = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                      event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"He\"}}\n\n\
                      event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"llo\"}}\n\n\
                      event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut buffer = stream.to_owned();
        let mut collected = String::new();
        let mut done = false;
        while let Some(event) = super::super::next_sse_event(&mut buffer) {
            match parse_sse_event(&event) {
                SseAction::Content(content) => collected.push_str(&content),
                SseAction::Done => done = true,
                SseAction::Error(message) => panic!("unexpected error: {message}"),
                SseAction::Ignore => {}
            }
        }
        assert_eq!(collected, "Hello");
        assert!(done);
    }

    #[tokio::test]
    async fn anthropic_sender_emits_full_event_sequence() {
        let (sender, mut response) = AnthropicSender::new("msg_1", "claude-test", "assistant", 8)
            .await
            .unwrap();
        sender.send("Hello").await.unwrap();
        sender.send_finish().await.unwrap();
        drop(sender);

        let mut stream = response.body.stream_data();
        let mut payload = String::new();
        while let Some(chunk) = stream.next().await {
            payload.push_str(&String::from_utf8(chunk).unwrap());
        }

        let events: Vec<&str> = payload.split("\n\n").filter(|e| !e.is_empty()).collect();
        let expected_headers = [
            "event: message_start",
            "event: content_block_start",
            "event: content_block_delta",
            "event: content_block_stop",
            "event: message_delta",
            "event: message_stop",
        ];
        assert_eq!(events.len(), expected_headers.len());
        for (event, expected) in events.iter().zip(expected_headers) {
            assert_eq!(event.lines().next(), Some(expected), "got {event:?}");
        }

        let frame_data = |event: &str| {
            serde_json::from_str::<serde_json::Value>(
                event
                    .lines()
                    .nth(1)
                    .unwrap()
                    .strip_prefix("data: ")
                    .unwrap(),
            )
            .unwrap()
        };
        let message_start = frame_data(events[0]);
        assert_eq!(message_start["type"], "message_start");
        assert_eq!(message_start["message"]["id"], "msg_1");
        assert_eq!(message_start["message"]["role"], "assistant");
        assert_eq!(message_start["message"]["model"], "claude-test");
        assert_eq!(
            message_start["message"]["stop_reason"],
            serde_json::Value::Null
        );

        let block_start = frame_data(events[1]);
        assert_eq!(block_start["type"], "content_block_start");
        assert_eq!(block_start["index"], 0);
        assert_eq!(block_start["content_block"]["type"], "text");

        let delta = frame_data(events[2]);
        assert_eq!(delta["type"], "content_block_delta");
        assert_eq!(delta["index"], 0);
        assert_eq!(delta["delta"]["text"], "Hello");

        let message_delta = frame_data(events[4]);
        assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");

        let message_stop = frame_data(events[5]);
        assert_eq!(message_stop["type"], "message_stop");
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        let json = client.serialize().unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"], "anthropic-messages");

        let restored = MessagesClient::deserialize(&json).unwrap();
        assert_eq!(restored.model(), Some("claude-test"));
        assert_eq!(restored.reasoning_effort(), None);
        assert_eq!(restored.messages(), client.messages());
    }

    #[tokio::test]
    async fn set_model_stores_value_without_validation() {
        let mut client = MessagesClient::new("https://example.invalid", None);
        assert!(matches!(
            client.list_models().await,
            Err(Error::UnsupportedCapability(message)) if message.contains("anthropic")
        ));
        client.set_model("claude-anything").await.unwrap();
        assert_eq!(client.model(), Some("claude-anything"));
    }
}
