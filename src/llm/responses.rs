//! OpenAI Responses protocol client (`POST {base_url}/responses`).

#[cfg(feature = "llm")]
use super::{
    bearer_headers, check_model_against_list, check_stream_response, parse_json_body,
    read_checked_body, read_json_body, send_request, spawn_sse_stream, ClientCore, LlmModelInfo,
    SseAction, StreamChunk,
};
#[cfg(feature = "llm")]
use super::{ChatMessage, MessageRole};
#[cfg(feature = "llm")]
use crate::protocol::{Error, ReasoningEffort};

#[cfg(feature = "llm")]
const PROVIDER_ID: &str = "openai-responses";

/// A multi-turn client for the OpenAI Responses API. System prompts are
/// transported as the top-level `instructions` field and history entries map
/// onto `input` items with typed content blocks.
#[cfg(feature = "llm")]
pub struct ResponsesClient {
    pub(crate) core: ClientCore,
}

#[cfg(feature = "llm")]
impl ResponsesClient {
    /// Creates a client for `base_url` (for example `https://api.openai.com`).
    /// The API key is optional for gateways that do not require one.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            core: ClientCore::new(base_url, api_key),
        }
    }

    /// Adds a system prompt; it is sent as the `instructions` field.
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

    /// Restores a client from [`ResponsesClient::serialize`] output.
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
        let url = format!("{}/responses", self.core.base_url);
        let input: Vec<serde_json::Value> = self
            .core
            .read_messages()
            .iter()
            .filter(|message| message.role != MessageRole::System)
            .map(|message| {
                let content_type = match message.role {
                    MessageRole::Assistant => "output_text",
                    _ => "input_text",
                };
                serde_json::json!({
                    "role": message.role.as_str(),
                    "content": [{
                        "type": content_type,
                        "text": message.content,
                    }],
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "model": self.core.ensure_model()?,
            "input": input,
            "stream": stream,
        });
        if let Some(instructions) = self.core.system_prompt() {
            body["instructions"] = serde_json::Value::String(instructions);
        }
        if let Some(effort) = self.core.reasoning_effort {
            body["reasoning"] = serde_json::json!({"effort": effort.as_str()});
        }
        Ok((url, body, bearer_headers(self.core.api_key.as_deref())))
    }
}

/// Concatenates the `output_text` blocks of every `message` item in
/// `output[]`.
#[cfg(feature = "llm")]
pub(crate) fn parse_response(text: &str) -> Result<String, Error> {
    let json = parse_json_body(text)?;
    let mut result = String::new();
    if let Some(output) = json["output"].as_array() {
        for item in output {
            if item["type"].as_str() != Some("message") {
                continue;
            }
            if let Some(content) = item["content"].as_array() {
                for block in content {
                    if block["type"].as_str() == Some("output_text") {
                        if let Some(text) = block["text"].as_str() {
                            result.push_str(text);
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Parses one SSE event block of a streaming Responses request.
#[cfg(feature = "llm")]
pub(crate) fn parse_sse_event(event: &str) -> SseAction {
    let Some(data) = super::sse_data_payload(event) else {
        return SseAction::Ignore;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
        return SseAction::Ignore;
    };
    match json["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => match json["delta"].as_str() {
            Some(delta) => SseAction::Content(delta.to_owned()),
            None => SseAction::Ignore,
        },
        "response.completed" | "response.incomplete" => SseAction::Done,
        "response.failed" => {
            let message = json["error"]["message"].as_str().unwrap_or(data).to_owned();
            SseAction::Error(message)
        }
        _ => SseAction::Ignore,
    }
}

#[cfg(all(test, feature = "llm"))]
mod tests {
    use super::*;

    fn client() -> ResponsesClient {
        let mut client = ResponsesClient::new("https://example.invalid", Some("key".to_owned()));
        client.core.model = Some("gpt-test".to_owned());
        client
    }

    #[test]
    fn build_request_defaults_without_instructions_or_reasoning() {
        let client = client();
        let (url, body, headers) = client.build_request(false).unwrap();
        assert_eq!(url, "https://example.invalid/responses");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], false);
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
        assert!(body.get("instructions").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(
            super::super::header_pairs(&headers),
            vec![("Authorization".to_owned(), "Bearer key".to_owned())]
        );
    }

    #[test]
    fn build_request_maps_history_system_and_reasoning() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.core.push_assistant_message("hi".to_owned());
        client.set_reasoning_effort(Some(ReasoningEffort::High));

        let (_, body, _) = client.build_request(true).unwrap();
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["reasoning"], serde_json::json!({"effort": "high"}));
        assert_eq!(body["stream"], true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn build_request_requires_a_model() {
        let client = ResponsesClient::new("https://example.invalid", None);
        assert!(matches!(
            client.build_request(false),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn parse_response_concatenates_output_text_blocks() {
        let text = r#"{"output":[
            {"type":"reasoning","summary":[]},
            {"type":"message","content":[
                {"type":"output_text","text":"Hel"},
                {"type":"refusal","refusal":"no"},
                {"type":"output_text","text":"lo"}
            ]}
        ]}"#;
        assert_eq!(parse_response(text).unwrap(), "Hello");
        assert_eq!(parse_response(r#"{"output":[]}"#).unwrap(), "");
        assert!(matches!(
            parse_response("not json"),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn sse_parser_dispatches_on_event_type() {
        let events = [
            ("data: {\"type\":\"response.created\"}", SseAction::Ignore),
            (
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}",
                SseAction::Content("Hel".to_owned()),
            ),
            (
                "data: {\"type\":\"response.output_text.done\",\"text\":\"Hello\"}",
                SseAction::Ignore,
            ),
            (
                "data: {\"type\":\"response.completed\",\"response\":{}}",
                SseAction::Done,
            ),
        ];
        for (event, expected) in events {
            assert_eq!(parse_sse_event(event), expected);
        }
        assert_eq!(
            parse_sse_event("data: {\"type\":\"response.incomplete\"}"),
            SseAction::Done
        );
    }

    #[test]
    fn sse_parser_reports_failed_responses() {
        let event =
            "data: {\"type\":\"response.failed\",\"error\":{\"code\":\"x\",\"message\":\"boom\"}}";
        assert_eq!(parse_sse_event(event), SseAction::Error("boom".to_owned()));
        let bare = "data: {\"type\":\"response.failed\"}";
        assert_eq!(
            parse_sse_event(bare),
            SseAction::Error("{\"type\":\"response.failed\"}".to_owned())
        );
    }

    #[test]
    fn sse_stream_end_to_end_accumulation() {
        let stream = "event: response.output_text.delta\n\
                      data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n\n\
                      data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n\
                      data: {\"type\":\"response.completed\"}\n\n";
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

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut client = client();
        client.set_system_prompt("be brief");
        client.core.push_user_message("hello");
        client.set_reasoning_effort(Some(ReasoningEffort::Max));
        let json = client.serialize().unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"], "openai-responses");

        let restored = ResponsesClient::deserialize(&json).unwrap();
        assert_eq!(restored.model(), Some("gpt-test"));
        assert_eq!(restored.reasoning_effort(), Some(ReasoningEffort::Max));
        assert_eq!(restored.messages(), client.messages());
        assert!(matches!(
            ClientCore::deserialize_state("openai-chat-completions", &json),
            Err(Error::InvalidConfig(_))
        ));
    }
}
