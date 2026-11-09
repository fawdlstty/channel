//! OpenAI Chat Completions protocol client and SSE emitter.

#[cfg(feature = "llm")]
use super::{
    bearer_headers, check_model_against_list, check_stream_response, parse_json_body,
    read_checked_body, read_json_body, send_request, spawn_sse_stream, ChatMessage, ClientCore,
    LlmModelInfo, MessageRole, SseAction, StreamChunk,
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
    /// streaming mode. Tool definitions set via [`Self::set_tools`] are sent
    /// with every request (`tool_choice: "auto"`).
    pub(crate) fn build_request(
        &self,
        stream: bool,
    ) -> Result<(String, serde_json::Value, Vec<potato::Headers>), Error> {
        let url = format!("{}/chat/completions", self.core.base_url);
        let messages: Vec<serde_json::Value> = normalize_system_first(self.core.read_messages())
            .iter()
            .map(|message| {
                let mut entry = serde_json::json!({
                    "role": message.role.as_str(),
                    "content": message.content,
                });
                if !message.tool_calls.is_empty() {
                    let calls: Vec<serde_json::Value> = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                },
                            })
                        })
                        .collect();
                    entry["tool_calls"] = serde_json::Value::Array(calls);
                }
                if let Some(tool_call_id) = &message.tool_call_id {
                    entry["tool_call_id"] =
                        serde_json::Value::String(tool_call_id.clone());
                }
                entry
            })
            .collect();
        let mut body = serde_json::json!({
            "model": self.core.ensure_model()?,
            "messages": messages,
            "stream": stream,
        });
        if !self.core.tools.is_empty() {
            let tools: Vec<serde_json::Value> = self
                .core
                .tools
                .iter()
                .map(|spec| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
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

    /// Replaces the tool definitions sent with every request.
    pub fn set_tools(&mut self, tools: Vec<super::ToolSpec>) {
        self.core.set_tools(tools);
    }

    /// Appends a tool result message linked to its invocation id (agent
    /// loop backfill between rounds).
    pub fn push_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) {
        let message = super::ChatMessage::tool_result(tool_call_id, content);
        self.core
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message);
    }

    /// The shared message log handle (agent loop reads history for
    /// persistence/compaction without cloning the client).
    pub fn message_log(&self) -> super::MessageLog {
        self.core.messages.clone()
    }

    /// The first logged system prompt, if any.
    pub fn system_prompt(&self) -> Option<String> {
        self.core.system_prompt()
    }

    /// Starts one completion over the current message log (no user message
    /// appended) and returns the live [`StreamChunk`] receiver: the agent
    /// loop consumes `Content` chunks incrementally for real-time deltas
    /// and inspects the aggregated tool calls after the terminator.
    pub async fn stream_completion(
        &mut self,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>, Error> {
        let (url, body, headers) = self.build_request(true)?;
        let mut response = send_request(&mut self.core.session, &url, body, headers).await?;
        check_stream_response(&mut response).await?;
        Ok(spawn_sse_stream(
            response,
            self.core.messages.clone(),
            parse_sse_event,
        ))
    }

    /// Runs one completion over the current message log (without appending
    /// a user message) and returns the assistant turn: streamed text (also
    /// kept in the message log), aggregated tool invocations and the
    /// provider-reported usage when present. This is the agent-loop entry
    /// point: the caller executes the requested tools and appends their
    /// results with [`Self::push_tool_result`] before the next round.
    pub async fn complete(&mut self) -> Result<AssistantTurn, Error> {
        let mut rx = self.stream_completion().await?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = None;
        while let Some(chunk) = rx.recv().await {
            match chunk {
                StreamChunk::Content(text) => content.push_str(&text),
                StreamChunk::ToolCalls(calls) => tool_calls = calls,
                StreamChunk::Usage(usage_chunk) => usage = Some(usage_chunk),
                StreamChunk::Error(message) => return Err(Error::ProviderRejected(message)),
                StreamChunk::Done => break,
            }
        }
        Ok(AssistantTurn {
            content,
            tool_calls,
            usage,
        })
    }
}

/// One assistant turn produced by [`ChatCompletionsClient::complete`]: the
/// visible text (possibly empty when the model only called tools), the tool
/// invocations to execute and the reported usage.
#[cfg(feature = "llm")]
#[derive(Clone, Debug, Default)]
pub struct AssistantTurn {
    pub content: String,
    pub tool_calls: Vec<super::ToolCallData>,
    pub usage: Option<super::StreamUsage>,
}

/// 出站消息序列的自适应防线（2026-11-08 真机报障）：Qwen 系严格 chat
/// template 要求 system 只出现在 messages[0]，其后任何 system 消息一律
/// `raise_exception('System message must be at the beginning')`（上游 HTTP
/// 500）。把全部 system 条目折叠为首条单一 system——内容相同去重、不同
/// 内容按出现顺序以空行拼接——其余消息保持原有相对顺序。日志自身不动，
/// 只影响这一请求的线上载荷；历史脏状态（旧版压缩产生的重复 system）
/// 也在这一层被兜住。
pub(crate) fn normalize_system_first(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut system: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    for message in messages {
        if message.role == MessageRole::System {
            if !system.contains(&message.content) {
                system.push(message.content);
            }
        } else {
            rest.push(message);
        }
    }
    if system.is_empty() {
        return rest;
    }
    let mut merged = vec![ChatMessage::new(MessageRole::System, system.join("\n\n"))];
    merged.extend(rest);
    merged
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

/// Parses one SSE event block of a streaming chat completion. Recognizes
/// content deltas, `delta.tool_calls` fragments (several per chunk allowed),
/// `finish_reason`, provider `usage` (final chunk with
/// `stream_options.include_usage`) and the `[DONE]` terminator.
#[cfg(feature = "llm")]
pub(crate) fn parse_sse_event(event: &str) -> SseAction {
    let Some(data) = super::sse_data_payload(event) else {
        return SseAction::Ignore;
    };
    if data == "[DONE]" {
        return SseAction::Done;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return SseAction::Ignore;
    };
    if let Some(usage) = json.get("usage").filter(|usage| usage.is_object()) {
        let usage = super::StreamUsage {
            prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
        };
        // Some providers emit the usage alongside the last content chunk.
        return match parse_choice_action(&json) {
            SseAction::Ignore => SseAction::Usage(usage),
            other => other,
        };
    }
    parse_choice_action(&json)
}

/// Extracts the action from `choices[0]` of a streaming chunk.
#[cfg(feature = "llm")]
fn parse_choice_action(json: &serde_json::Value) -> SseAction {
    let choice = &json["choices"][0];
    let delta = &choice["delta"];
    if let Some(calls) = delta["tool_calls"].as_array() {
        let fragments: Vec<super::ToolCallFragment> = calls
            .iter()
            .filter_map(|call| {
                let index = call["index"].as_u64()?;
                Some(super::ToolCallFragment {
                    index: index as usize,
                    id: call["id"].as_str().map(str::to_owned),
                    name: call["function"]["name"].as_str().map(str::to_owned),
                    arguments: call["function"]["arguments"].as_str().map(str::to_owned),
                })
            })
            .collect();
        if !fragments.is_empty() {
            return SseAction::ToolCallDelta(fragments);
        }
    }
    if let Some(content) = delta["content"].as_str() {
        return SseAction::Content(content.to_owned());
    }
    if let Some(reason) = choice["finish_reason"].as_str() {
        return SseAction::FinishReason(reason.to_owned());
    }
    SseAction::Ignore
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
/// line) is flushed as-is instead of being dropped. Re-framing stays
/// byte-level end to end: per-chunk lossy decoding would corrupt multibyte
/// characters split across TCP chunks.
#[cfg(feature = "llm")]
fn spawn_sse_passthrough(response: potato::HttpResponse) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut response = response;
        let mut stream = response.body.stream_data();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buffer.extend_from_slice(&chunk);
            while let Some(event) = next_raw_sse_event(&mut buffer) {
                let mut frame = event;
                frame.extend_from_slice(b"\n\n");
                if tx.send(frame).await.is_err() {
                    // Downstream closed (client went away); stop pulling.
                    return;
                }
            }
        }
        if !buffer.is_empty() {
            buffer.extend_from_slice(b"\n\n");
            let _ = tx.send(buffer).await;
        }
    });
    rx
}

/// Pops the next complete SSE event (raw bytes, without the trailing blank
/// line) from the buffer; incomplete trailing data stays buffered.
#[cfg(feature = "llm")]
fn next_raw_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let position = buffer.windows(2).position(|pair| pair == b"\n\n")?;
    Some(buffer.drain(..position + 2).take(position).collect())
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

    /// 出站自适应防线回归：日志里首条之后的 system（历史脏状态/未来写入
    /// 路径兜底）在出站载荷里必须折叠进首条 system，其余消息保序——Qwen
    /// 系严格 chat template 对 messages[0] 之后的 system 一律 500（
    /// "System message must be at the beginning"，2026-11-08 真机报障）。
    #[test]
    fn build_request_folds_mid_log_system_entries_to_front() {
        // 局部绑定不叫 `client`：那会遮蔽本测试组的 client() 辅助函数。
        let mut folded_client = client();
        folded_client.set_system_prompt("主线系统提示");
        folded_client.core.push_user_message("第一轮");
        folded_client
            .core
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ChatMessage::new(MessageRole::System, "中途注入"));
        folded_client.core.push_assistant_message("答".to_owned());
        folded_client.core.push_user_message("第二轮");

        let (_, body, _) = folded_client.build_request(false).unwrap();
        let messages = body["messages"].as_array().unwrap();
        // 5 条原始消息折叠为 4 条：system(合并) + user + assistant + user。
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        let system_text = messages[0]["content"].as_str().unwrap();
        assert!(system_text.contains("主线系统提示"));
        assert!(system_text.contains("中途注入"));
        assert!(messages[1..].iter().all(|m| m["role"] != "system"));
        assert_eq!(messages[1]["content"], "第一轮");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["content"], "第二轮");

        // 内容完全相同的重复 system 只保留一份文本（干净日志场景）。
        let mut dedupe_client = client();
        dedupe_client.set_system_prompt("主线系统提示");
        dedupe_client.core.push_user_message("第一轮");
        dedupe_client
            .core
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ChatMessage::new(MessageRole::System, "主线系统提示"));
        let (_, body, _) = dedupe_client.build_request(false).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "主线系统提示");
        assert_eq!(messages[1]["content"], "第一轮");
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
        let mut buffer = stream.to_owned().into_bytes();
        let mut collected = String::new();
        let mut saw_done = false;
        while let Some(event) = super::super::next_sse_event_bytes(&mut buffer) {
            match parse_sse_event(&event) {
                SseAction::Content(content) => collected.push_str(&content),
                SseAction::Done => saw_done = true,
                SseAction::Error(message) => panic!("unexpected error: {message}"),
                SseAction::Ignore => {}
                SseAction::ToolCallDelta(_)
                | SseAction::FinishReason(_)
                | SseAction::Usage(_) => {}
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

    #[test]
    fn sse_parser_extracts_tool_call_fragments_finish_and_usage() {
        // 两个分片各带增量 arguments + 一个事件多分片 + finish_reason + usage。
        let first = parse_sse_event(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\
             \"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"file\"}}]}}]}",
        );
        let second = parse_sse_event(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"_path\\\":\\\"a.txt\\\"}\"}}]}}],\"usage\":null}",
        );
        let third = parse_sse_event(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\
             \"function\":{\"name\":\"bash\"}},{\"index\":2,\"id\":\"call_3\",\
             \"function\":{\"name\":\"edit\"}}]},\"finish_reason\":\"tool_calls\"}]}",
        );
        let usage = parse_sse_event(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30}}",
        );

        assert_eq!(
            first,
            SseAction::ToolCallDelta(vec![super::super::ToolCallFragment {
                index: 0,
                id: Some("call_1".to_owned()),
                name: Some("read_file".to_owned()),
                arguments: Some("{\"file".to_owned()),
            }])
        );
        assert_eq!(
            second,
            SseAction::ToolCallDelta(vec![super::super::ToolCallFragment {
                index: 0,
                id: None,
                name: None,
                arguments: Some("_path\":\"a.txt\"}".to_owned()),
            }])
        );
        assert_eq!(
            third,
            SseAction::ToolCallDelta(vec![
                super::super::ToolCallFragment {
                    index: 1,
                    id: Some("call_2".to_owned()),
                    name: Some("bash".to_owned()),
                    arguments: None,
                },
                super::super::ToolCallFragment {
                    index: 2,
                    id: Some("call_3".to_owned()),
                    name: Some("edit".to_owned()),
                    arguments: None,
                },
            ])
        );
        assert_eq!(
            usage,
            SseAction::Usage(super::super::StreamUsage {
                prompt_tokens: 120,
                completion_tokens: 30,
            })
        );
    }

    #[test]
    fn build_request_sends_tools_and_tool_round_trip_messages() {
        let mut client = client();
        client.set_tools(vec![super::super::ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        }]);
        client.core.push_user_message("read a.txt");
        let calls = vec![super::super::ToolCallData {
            id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{\"file_path\":\"a.txt\"}".to_owned(),
        }];
        client
            .core
            .messages
            .write()
            .unwrap()
            .push(super::super::ChatMessage::assistant_with_tool_calls(
                String::new(),
                calls,
            ));
        client.core.messages.write().unwrap().push(
            super::super::ChatMessage::tool_result("call_1", "file body"),
        );

        let (_, body, _) = client.build_request(false).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tool_choice"], "auto");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[1]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["content"], "file body");
    }

    #[test]
    fn chat_message_tool_fields_round_trip_via_serde() {
        let assistant = super::super::ChatMessage::assistant_with_tool_calls(
            "calling",
            vec![super::super::ToolCallData {
                id: "call_1".to_owned(),
                name: "bash".to_owned(),
                arguments: "{}".to_owned(),
            }],
        );
        let encoded = serde_json::to_value(&assistant).unwrap();
        assert_eq!(encoded["tool_calls"][0]["name"], "bash");
        let decoded: super::super::ChatMessage = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, assistant);

        // 无工具消息序列化不带 tool 字段（兼容旧载荷）。
        let plain = super::super::ChatMessage::user("hi");
        let encoded = serde_json::to_value(&plain).unwrap();
        assert!(encoded.get("tool_calls").is_none());
        assert!(encoded.get("tool_call_id").is_none());
        let decoded: super::super::ChatMessage = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, plain);
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
