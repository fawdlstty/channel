//! OpenAI Chat Completions protocol client and SSE emitter.

#[cfg(feature = "llm")]
use super::{
    bearer_headers, check_model_against_list, check_stream_response, next_sse_event,
    parse_json_body, read_checked_body, read_json_body, send_request, spawn_sse_stream,
    ClientCore, LlmModelInfo, SseAction, StreamChunk,
};
use crate::protocol::Error;
#[cfg(feature = "llm")]
use crate::protocol::ReasoningEffort;

#[cfg(feature = "llm")]
const PROVIDER_ID: &str = "openai-chat-completions";

/// A multi-turn client for the OpenAI Chat Completions API
/// (`POST {base_url}/chat/completions`).
#[cfg(feature = "llm")]
pub struct ChatCompletionsClient {
    pub(crate) core: ClientCore,
}

#[cfg(feature = "llm")]
impl ChatCompletionsClient {
    /// Creates a client for `base_url` (for example `https://api.openai.com`).
    /// The API key is optional for gateways that do not require one.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            core: ClientCore::new(base_url, api_key),
        }
    }

    /// Adds a system prompt to the conversation history.
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

    /// Sets or clears the reasoning effort.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.core.reasoning_effort = effort;
    }

    /// The currently configured reasoning effort.
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.core.reasoning_effort()
    }

    /// Returns a copy of the conversation history.
    pub fn messages(&self) -> Vec<super::ChatMessage> {
        self.core.messages()
    }

    /// Replaces the conversation history, keeping the already logged system
    /// messages.
    pub fn set_messages(&mut self, messages: Vec<super::ChatMessage>) {
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

    /// Lists the models advertised by `GET {base_url}/models`.
    pub async fn list_models(&mut self) -> Result<Vec<LlmModelInfo>, Error> {
        let url = format!("{}/models", self.core.base_url);
        let mut response = self
            .core
            .session
            .get(&url, bearer_headers(self.core.api_key.as_deref()))
            .await
            .map_err(|error| super::transport_error(&format!("request to {url} failed"), error))?;
        let json = read_json_body(&mut response).await?;
        let mut models = Vec::new();
        if let Some(data) = json["data"].as_array() {
            for item in data {
                let id = item["id"].as_str().unwrap_or_default();
                if !id.is_empty() {
                    models.push(LlmModelInfo {
                        id: id.to_owned(),
                        name: id.to_owned(),
                        provider_id: "openai".to_owned(),
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

    /// Restores a client from [`ChatCompletionsClient::serialize`] output.
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
        let url = format!("{}/chat/completions", self.core.base_url);
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
        let mut body = serde_json::json!({
            "model": self.core.ensure_model()?,
            "messages": messages,
            "stream": stream,
        });
        match self.core.reasoning_effort {
            Some(effort) => {
                body["reasoning_effort"] = serde_json::Value::String(effort.as_str().to_owned());
                body["thinking"] = serde_json::json!({"type": "enabled"});
            }
            None => {
                body["thinking"] = serde_json::json!({"type": "disabled"});
            }
        }
        Ok((url, body, bearer_headers(self.core.api_key.as_deref())))
    }
}

/// Extracts `choices[0].message.content` from a non-streaming response.
#[cfg(feature = "llm")]
pub(crate) fn parse_response(text: &str) -> Result<String, Error> {
    let json = parse_json_body(text)?;
    Ok(json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Parses one SSE event block of a streaming chat completion.
#[cfg(feature = "llm")]
pub(crate) fn parse_sse_event(event: &str) -> SseAction {
    let Some(data) = super::sse_data_payload(event) else {
        return SseAction::Ignore;
    };
    if data == "[DONE]" {
        return SseAction::Done;
    }
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(json) => match json["choices"][0]["delta"]["content"].as_str() {
            Some(content) => SseAction::Content(content.to_owned()),
            None => SseAction::Ignore,
        },
        Err(_) => SseAction::Ignore,
    }
}

/// The result of a server-side forwarding request made with
/// [`forward_chat_completions`]: the two possible shapes of an OpenAI Chat
/// Completions exchange.
#[cfg(feature = "llm")]
pub enum ForwardedCompletion {
    /// Non-streaming: the upstream response body in full (usage, choices,
    /// tool_calls and any extension fields preserved as-is).
    Full(serde_json::Value),
    /// Streaming: raw SSE events, one complete event per item (including the
    /// `data: ` prefix and the terminating blank line; the `data: [DONE]`
    /// terminator passes through unchanged). Sender close = end of stream.
    Stream(tokio::sync::mpsc::Receiver<Vec<u8>>),
}

/// Shapes a forwarded request body. The two transformations a relay may
/// apply: replace `model` (when `model` is `Some`) and inject
/// `stream_options.include_usage = true` for streaming requests that do not
/// carry it (so the final chunk carries usage for metering). Every other
/// field — tool_calls, vision payloads, sampling parameters — is preserved
/// verbatim.
#[cfg(feature = "llm")]
pub(crate) fn shape_forward_body(
    mut body: serde_json::Value,
    model: Option<&str>,
) -> serde_json::Value {
    if let Some(model) = model {
        body["model"] = serde_json::Value::String(model.to_owned());
    }
    let streaming = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if streaming {
        let already = body
            .get("stream_options")
            .and_then(|options| options.get("include_usage"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !already {
            let mut options = body
                .get("stream_options")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            options.insert("include_usage".to_owned(), serde_json::Value::Bool(true));
            body["stream_options"] = serde_json::Value::Object(options);
        }
    }
    body
}

/// Forwards an arbitrary OpenAI Chat Completions body to `base_url`
/// (server-side relay entry point; unlike [`ChatCompletionsClient`] it does
/// not assemble the conversation — tool_calls, vision and other extension
/// fields pass through untouched). Returns the non-streaming body in full or
/// a per-event raw SSE stream; see [`ForwardedCompletion`].
#[cfg(feature = "llm")]
pub async fn forward_chat_completions(
    base_url: impl AsRef<str>,
    api_key: Option<&str>,
    model: Option<&str>,
    body: serde_json::Value,
) -> Result<ForwardedCompletion, Error> {
    let url = format!("{}/chat/completions", base_url.as_ref());
    let body = shape_forward_body(body, model);
    let streaming = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut session = potato::Session::new();
    let mut response = send_request(&mut session, &url, body, bearer_headers(api_key)).await?;
    if streaming {
        check_stream_response(&mut response).await?;
        Ok(ForwardedCompletion::Stream(spawn_sse_passthrough(response)))
    } else {
        Ok(ForwardedCompletion::Full(read_json_body(&mut response).await?))
    }
}

/// Spawns the background task that re-frames an upstream SSE byte stream
/// into complete events (blank-line delimited) and forwards each event's raw
/// bytes. A trailing partial event (upstream closed without the final blank
/// line) is flushed as-is instead of being dropped.
#[cfg(feature = "llm")]
fn spawn_sse_passthrough(response: potato::HttpResponse) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut response = response;
        let mut stream = response.body.stream_data();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(mut event) = next_sse_event(&mut buffer) {
                event.push_str("\n\n");
                if tx.send(event.into_bytes()).await.is_err() {
                    // Downstream closed (client went away); stop pulling.
                    return;
                }
            }
        }
        if !buffer.is_empty() {
            buffer.push_str("\n\n");
            let _ = tx.send(buffer.into_bytes()).await;
        }
    });
    rx
}

/// Emits an OpenAI Chat Completions SSE stream, for example to feed a
/// potato-based HTTP server.
pub struct OpenAISender {
    id: String,
    object: String,
    model: String,
    role: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl OpenAISender {
    /// Creates the sender, its buffering channel and the SSE response. The
    /// first frame (carrying only the assistant role) is emitted
    /// immediately.
    pub async fn new(
        id: impl Into<String>,
        object: impl Into<String>,
        model: impl Into<String>,
        role: impl Into<String>,
        buffer_size: usize,
    ) -> Result<(Self, potato::HttpResponse), Error> {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);
        let sender = Self {
            id: id.into(),
            object: object.into(),
            model: model.into(),
            role: role.into(),
            tx,
        };

        let frame = serde_json::json!({
            "id": sender.id,
            "object": sender.object,
            "created": super::unix_seconds(),
            "model": sender.model,
            "choices": [{
                "index": 0,
                "delta": {
                    "role": sender.role,
                },
                "finish_reason": null,
            }]
        });
        sender.send_frame(&frame).await?;

        Ok((sender, potato::HttpResponse::sse(rx)))
    }

    /// Emits one content delta frame.
    pub async fn send(&self, message: impl Into<String>) -> Result<(), Error> {
        let frame = serde_json::json!({
            "id": self.id,
            "object": self.object,
            "created": super::unix_seconds(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {
                    "content": message.into(),
                },
                "finish_reason": null,
            }]
        });
        self.send_frame(&frame).await
    }

    /// Emits the finish frame followed by the `data: [DONE]` terminator.
    pub async fn send_finish(&self, finish_reason: impl Into<String>) -> Result<(), Error> {
        let frame = serde_json::json!({
            "id": self.id,
            "object": self.object,
            "created": super::unix_seconds(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": finish_reason.into(),
            }]
        });
        self.send_frame(&frame).await?;
        self.send_raw(b"data: [DONE]\n\n").await
    }

    async fn send_frame(&self, frame: &serde_json::Value) -> Result<(), Error> {
        let payload = format!("data: {frame}\n\n");
        self.send_raw(payload.as_bytes()).await
    }

    async fn send_raw(&self, payload: &[u8]) -> Result<(), Error> {
        self.tx
            .send(payload.to_vec())
            .await
            .map_err(|_| Error::Closed)
    }
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::*;

    fn client() -> ChatCompletionsClient {
        let mut client =
            ChatCompletionsClient::new("https://example.invalid", Some("key".to_owned()));
        client.core.model = Some("gpt-test".to_owned());
        client
    }

    #[test]
    fn build_request_defaults_disable_thinking() {
        let client = client();
        let (url, body, headers) = client.build_request(false).unwrap();
        assert_eq!(url, "https://example.invalid/chat/completions");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(
            super::super::header_pairs(&headers),
            vec![("Authorization".to_owned(), "Bearer key".to_owned())]
        );
    }

    #[test]
    fn build_request_maps_history_and_streaming() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.core.push_assistant_message("hi".to_owned());
        client.core.push_user_message("again");

        let (_, body, _) = client.build_request(true).unwrap();
        assert_eq!(body["stream"], true);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["content"], "again");
    }

    #[test]
    fn build_request_enabled_thinking_with_effort() {
        let mut client = ChatCompletionsClient::new("https://example.invalid", None);
        client.core.model = Some("o-test".to_owned());
        client.set_reasoning_effort(Some(ReasoningEffort::XHigh));
        let (_, body, headers) = client.build_request(false).unwrap();
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
        assert!(headers.is_empty());
    }

    #[test]
    fn build_request_requires_a_model() {
        let client = ChatCompletionsClient::new("https://example.invalid", None);
        assert!(matches!(
            client.build_request(false),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn parse_response_extracts_message_content() {
        let text = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        assert_eq!(parse_response(text).unwrap(), "hello");
        assert_eq!(parse_response(r#"{"choices":[]}"#).unwrap(), "");
        assert!(matches!(
            parse_response("not json"),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn sse_parser_accumulates_deltas_and_stops_at_done() {
        let stream = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                      data: [DONE]\n\n";
        let mut buffer = stream.to_owned();
        let mut collected = String::new();
        let mut saw_done = false;
        while let Some(event) = super::super::next_sse_event(&mut buffer) {
            match parse_sse_event(&event) {
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
    fn sse_parser_ignores_malformed_events() {
        assert_eq!(parse_sse_event("event: ping"), SseAction::Ignore);
        assert_eq!(parse_sse_event("data: not-json"), SseAction::Ignore);
        assert_eq!(
            parse_sse_event("data: {\"choices\":[{\"delta\":{}}]}"),
            SseAction::Ignore
        );
        assert_eq!(
            parse_sse_event("data:[DONE]"),
            SseAction::Done,
            "data without a space must still terminate"
        );
    }

    #[tokio::test]
    async fn openai_sender_emits_role_content_and_done_frames() {
        // Four frames are emitted (role, content, finish, [DONE]) so the
        // buffer must hold them all before the consumer starts draining.
        let (sender, mut response) =
            OpenAISender::new("id", "chat.completion.chunk", "model", "assistant", 8)
                .await
                .unwrap();
        sender.send("Hello").await.unwrap();
        sender.send_finish("stop").await.unwrap();
        drop(sender);

        let mut stream = response.body.stream_data();
        let mut payload = String::new();
        while let Some(chunk) = stream.next().await {
            payload.push_str(&String::from_utf8(chunk).unwrap());
        }

        let events: Vec<&str> = payload.split("\n\n").collect();
        // Four frames plus the trailing empty element after the last "\n\n".
        assert_eq!(events.len(), 5);
        assert_eq!(events[3], "data: [DONE]");
        assert_eq!(events[4], "");
        assert!(payload.ends_with("data: [DONE]\n\n"));

        let role_frame: serde_json::Value =
            serde_json::from_str(events[0].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(role_frame["id"], "id");
        assert_eq!(role_frame["object"], "chat.completion.chunk");
        assert_eq!(role_frame["model"], "model");
        assert!(role_frame["created"].as_i64().unwrap() > 0);
        assert_eq!(role_frame["choices"][0]["index"], 0);
        assert_eq!(role_frame["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(
            role_frame["choices"][0]["finish_reason"],
            serde_json::Value::Null
        );

        let content_frame: serde_json::Value =
            serde_json::from_str(events[1].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(content_frame["choices"][0]["delta"]["content"], "Hello");
        assert_eq!(
            content_frame["choices"][0]["finish_reason"],
            serde_json::Value::Null
        );

        let finish_frame: serde_json::Value =
            serde_json::from_str(events[2].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(finish_frame["choices"][0]["delta"], serde_json::json!({}));
        assert_eq!(finish_frame["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.set_reasoning_effort(Some(ReasoningEffort::Low));
        let json = client.serialize().unwrap();

        let restored = ChatCompletionsClient::deserialize(&json).unwrap();
        assert_eq!(restored.model(), Some("gpt-test"));
        assert_eq!(restored.reasoning_effort(), Some(ReasoningEffort::Low));
        assert_eq!(restored.messages(), client.messages());
        assert_eq!(restored.core.api_key.as_deref(), Some("key"));
    }

    #[test]
    fn shape_forward_body_preserves_extension_fields_verbatim() {
        // tool_calls / vision 等扩展字段原样保留，仅做允许的两处改动之外零触碰。
        let body = serde_json::json!({
            "model": "client-fixed-name",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": [
                    {"type": "text", "text": "看这张图"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,xxx"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto",
            "temperature": 0.7,
            "stream": false
        });
        let original = body.clone();
        let shaped = shape_forward_body(body, None);
        assert_eq!(shaped, original, "model=None 且非流式：body 必须原样");
    }

    #[test]
    fn shape_forward_body_replaces_model_and_injects_include_usage() {
        let body = serde_json::json!({
            "model": "Recorder-0.1-Flash",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let shaped = shape_forward_body(body, Some("deepseek-flash"));
        assert_eq!(shaped["model"], "deepseek-flash", "model 替换为上游真实名");
        assert_eq!(
            shaped["stream_options"]["include_usage"], true,
            "流式且客户端未带 stream_options：注入 include_usage=true"
        );

        // 客户端已带 include_usage=true：不重复注入也不覆盖。
        let body = serde_json::json!({
            "model": "m", "stream": true,
            "stream_options": {"include_usage": true}
        });
        let shaped = shape_forward_body(body, None);
        assert_eq!(shaped["stream_options"]["include_usage"], true);

        // 客户端带 include_usage=false：服务器计费依赖末块 usage，覆写为 true
        // （客户端置 false 意味着拿不到 usage → 0 计费，中转不允许）。
        let body = serde_json::json!({
            "model": "m", "stream": true,
            "stream_options": {"include_usage": false, "custom": 1}
        });
        let shaped = shape_forward_body(body, None);
        assert_eq!(shaped["stream_options"]["include_usage"], true);
        assert_eq!(shaped["stream_options"]["custom"], 1);

        // 非流式：不注入 stream_options。
        let body = serde_json::json!({"model": "m", "messages": [], "stream": false});
        let shaped = shape_forward_body(body, None);
        assert!(shaped.get("stream_options").is_none());
        // stream 字段缺失（缺省非流式）：同样不注入。
        let body = serde_json::json!({"model": "m", "messages": []});
        let shaped = shape_forward_body(body, None);
        assert!(shaped.get("stream_options").is_none());
    }
}
