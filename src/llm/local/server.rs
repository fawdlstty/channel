//! Exposes local models through the four protocol HTTP endpoints
//! (`local-server` feature).
//!
//! [`LocalLlmServer`] mounts one or more [`LocalClient`] models and serves
//! OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Ollama
//! chat on a single potato [`HttpServer`], so any standard client — including
//! channel's own four protocol clients — can talk to a local model over HTTP.
//!
//! ```no_run
//! # async fn demo() -> Result<(), channel::Error> {
//! let mut server = channel::LocalLlmServer::bind("127.0.0.1:8817")?;
//! server.mount_model("./Qwen3-0.6B")?;
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Every request is routed to the model named in the protocol's `model`
//! field; with a single model mounted, requests naming any (or no) model
//! are served by it. A model can serve one generation at a time: while a
//! request is being generated, other requests fail with HTTP 409 (the same
//! [`Error::Busy`] semantics as the local client). Only error-free
//! generation frames are protocol-shaped; an engine error mid-stream is
//! passed through as a final content delta.

use super::LocalClient;
use crate::llm::{AnthropicSender, ChatMessage, MessageRole, OllamaSender, OpenAISender, StreamChunk};
use crate::protocol::Error;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// The per-request generation plan extracted synchronously from the HTTP
/// request; execution happens on the async side.
enum Plan {
    OpenAiModels,
    OllamaTags,
    OpenAiChat(ChatRequest),
    OpenAiResponses(ChatRequest),
    AnthropicMessages(ChatRequest),
    OllamaChat(ChatRequest),
}

/// The protocol-independent content of one generation request.
struct ChatRequest {
    model: Option<String>,
    messages: Vec<ChatMessage>,
    params: crate::llm::GenerationParams,
    stream: bool,
}

/// A one-click HTTP server exposing mounted local models through the four
/// supported protocol endpoints:
///
/// | Endpoint | Protocol |
/// |---|---|
/// | `GET /v1/models` | OpenAI model list |
/// | `POST /v1/chat/completions` | OpenAI Chat Completions |
/// | `POST /v1/responses` | OpenAI Responses |
/// | `POST /v1/messages` | Anthropic Messages |
/// | `POST /api/chat` | Ollama chat |
/// | `GET /api/tags` | Ollama model list |
pub struct LocalLlmServer {
    addr: String,
    token: Option<String>,
    models: HashMap<String, LocalClient>,
}

impl LocalLlmServer {
    /// Creates the server bound to `addr` (for example `127.0.0.1:8817`).
    /// The socket is only opened by [`LocalLlmServer::serve`].
    pub fn bind(addr: impl Into<String>) -> Result<Self, Error> {
        Ok(Self {
            addr: addr.into(),
            token: None,
            models: HashMap::new(),
        })
    }

    /// Requires every request to carry `Authorization: Bearer <token>`.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Loads `path` (same dispatch rules as [`LocalClient::load`]) and
    /// mounts it under its file stem or directory name. Blocking and
    /// heavyweight; intended for the setup phase. Mounting the same name
    /// twice replaces the earlier model.
    pub fn mount_model(&mut self, path: impl AsRef<Path>) -> Result<&mut Self, Error> {
        let client = LocalClient::load_blocking(path)?;
        let name = client
            .model()
            .unwrap_or_default()
            .to_owned();
        self.models.insert(name, client);
        Ok(self)
    }

    /// The mounted model names.
    pub fn models(&self) -> Vec<String> {
        let mut names: Vec<String> = self.models.keys().cloned().collect();
        names.sort();
        names
    }

    /// Serves the protocol endpoints until the process exits.
    pub async fn serve(self) -> Result<(), Error> {
        let models = Arc::new(self.models);
        let token = Arc::new(self.token);
        let mut server = potato::HttpServer::new(self.addr);
        server
            .configure(|ctx| {
                ctx.use_limit_size(64 * 1024, 32 * 1024 * 1024);
                let models = Arc::clone(&models);
                let token = Arc::clone(&token);
                ctx.use_custom_async(move |request| {
                    let plan = route(request, &token);
                    let models = Arc::clone(&models);
                    Box::pin(async move { execute(plan, &models).await })
                });
            })
            .map_err(|error| Error::Backend(format!("server configuration failed: {error}")))?;
        server
            .serve_http()
            .await
            .map_err(|error| Error::Backend(format!("local model server failed: {error}")))
    }
}

/// Synchronous request triage: authentication, routing and protocol
/// extraction. Returns a plan carrying the parsed request, or a ready-made
/// error response.
fn route(
    request: &mut potato::HttpRequest,
    token: &Option<String>,
) -> Result<Plan, potato::HttpResponse> {
    if let Some(token) = token {
        let expected = format!("Bearer {token}");
        let matches = request
            .get_header("authorization")
            .is_some_and(|value| value == expected);
        if !matches {
            return Err(json_response(
                401,
                &serde_json::json!({"error": {"message": "invalid or missing bearer token"}}),
            ));
        }
    }
    let path = request.url_path.as_str().to_owned();
    if request.method == potato::HttpMethod::GET {
        return match path.as_str() {
            "/v1/models" => Ok(Plan::OpenAiModels),
            "/api/tags" => Ok(Plan::OllamaTags),
            _ => Err(potato::HttpResponse::not_found()),
        };
    }
    if request.method != potato::HttpMethod::POST {
        return Err(potato::HttpResponse::not_found());
    }
    let flavor = match path.as_str() {
        "/v1/chat/completions" => Flavor::OpenAi,
        "/v1/responses" => Flavor::Responses,
        "/v1/messages" => Flavor::Anthropic,
        "/api/chat" => Flavor::Ollama,
        _ => return Err(potato::HttpResponse::not_found()),
    };
    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => {
            return Err(error_response(
                flavor,
                400,
                &Error::ProtocolError(format!("invalid request body: {error}")),
            ))
        }
    };
    Ok(match flavor {
        Flavor::OpenAi => Plan::OpenAiChat(parse_chat_request(&body, flavor)?),
        Flavor::Responses => Plan::OpenAiResponses(parse_responses_request(&body)?),
        Flavor::Anthropic => Plan::AnthropicMessages(parse_anthropic_request(&body)?),
        Flavor::Ollama => Plan::OllamaChat(parse_chat_request(&body, flavor)?),
    })
}

#[derive(Clone, Copy)]
enum Flavor {
    OpenAi,
    Responses,
    Anthropic,
    Ollama,
}

/// A protocol-shaped error response (`400`/`409`/`500`).
fn error_response(flavor: Flavor, status: u16, error: &Error) -> potato::HttpResponse {
    let message = error.to_string();
    let payload = match flavor {
        Flavor::OpenAi | Flavor::Responses => serde_json::json!({
            "error": {"message": message, "type": "server_error"}
        }),
        Flavor::Anthropic => serde_json::json!({
            "type": "error",
            "error": {"type": "api_error", "message": message}
        }),
        Flavor::Ollama => serde_json::json!({"error": message}),
    };
    json_response(status, &payload)
}

/// Maps a generation error onto a status: busy is 409 (the model serves one
/// request at a time), everything else 500.
fn error_status(error: &Error) -> u16 {
    match error {
        Error::Busy => 409,
        _ => 500,
    }
}

fn json_response(status: u16, payload: &serde_json::Value) -> potato::HttpResponse {
    let mut response = potato::HttpResponse::new();
    response.http_code = status;
    response
        .add_header("Content-Type".into(), "application/json".into());
    response.body = potato::HttpResponseBody::Data(payload.to_string().into_bytes());
    response
}

/// Extracts a text `content` field that may be a plain string or an array
/// of `{type, text}` blocks (Anthropic / Responses styles).
fn content_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let mut text = String::new();
    if let Some(blocks) = value.as_array() {
        for block in blocks {
            if let Some(part) = block["text"].as_str() {
                text.push_str(part);
            }
        }
    }
    text
}

fn role_of(value: &str) -> MessageRole {
    match value {
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        _ => MessageRole::User,
    }
}

/// Applies the common sampling knobs of the protocol requests.
fn apply_params(body: &serde_json::Value, params: &mut crate::llm::GenerationParams) {
    if let Some(temperature) = body["temperature"].as_f64() {
        params.temperature = temperature;
    }
    if let Some(top_p) = body["top_p"].as_f64() {
        params.top_p = top_p;
    }
    if let Some(max_tokens) = body["max_tokens"].as_u64() {
        params.max_tokens = max_tokens.min(u64::from(u32::MAX)) as u32;
    }
    match body["stop"].as_str() {
        Some(stop) => params.stop.push(stop.to_owned()),
        None => {
            if let Some(stops) = body["stop"].as_array() {
                params
                    .stop
                    .extend(stops.iter().filter_map(|stop| stop.as_str().map(str::to_owned)));
            }
        }
    }
}

/// OpenAI Chat Completions and Ollama share the `messages` array shape.
fn parse_chat_request(
    body: &serde_json::Value,
    flavor: Flavor,
) -> Result<ChatRequest, potato::HttpResponse> {
    let mut messages = Vec::new();
    if let Some(entries) = body["messages"].as_array() {
        for entry in entries {
            let role = role_of(entry["role"].as_str().unwrap_or("user"));
            messages.push(ChatMessage::new(role, content_text(&entry["content"])));
        }
    }
    let mut params = crate::llm::GenerationParams::default();
    match flavor {
        Flavor::Ollama => {
            let options = &body["options"];
            if let Some(temperature) = options["temperature"].as_f64() {
                params.temperature = temperature;
            }
            if let Some(top_p) = options["top_p"].as_f64() {
                params.top_p = top_p;
            }
            if let Some(top_k) = options["top_k"].as_i64() {
                params.top_k = top_k as i32;
            }
            if let Some(num_predict) = options["num_predict"].as_u64() {
                params.max_tokens = num_predict.min(u64::from(u32::MAX)) as u32;
            }
            if let Some(stops) = options["stop"].as_array() {
                params
                    .stop
                    .extend(stops.iter().filter_map(|stop| stop.as_str().map(str::to_owned)));
            }
        }
        _ => apply_params(body, &mut params),
    }
    Ok(ChatRequest {
        model: body["model"].as_str().map(str::to_owned),
        messages,
        params,
        stream: body["stream"].as_bool().unwrap_or(false),
    })
}

/// OpenAI Responses: `instructions` becomes the system message and `input`
/// is either a plain string or an array of `{role, content}` items.
fn parse_responses_request(body: &serde_json::Value) -> Result<ChatRequest, potato::HttpResponse> {
    let mut messages = Vec::new();
    if let Some(instructions) = body["instructions"].as_str() {
        messages.push(ChatMessage::system(instructions));
    }
    match &body["input"] {
        serde_json::Value::String(text) => {
            messages.push(ChatMessage::user(text.clone()));
        }
        serde_json::Value::Array(entries) => {
            for entry in entries {
                let role = role_of(entry["role"].as_str().unwrap_or("user"));
                messages.push(ChatMessage::new(role, content_text(&entry["content"])));
            }
        }
        _ => {}
    }
    let mut params = crate::llm::GenerationParams::default();
    apply_params(body, &mut params);
    Ok(ChatRequest {
        model: body["model"].as_str().map(str::to_owned),
        messages,
        params,
        stream: body["stream"].as_bool().unwrap_or(false),
    })
}

/// Anthropic Messages: the top-level `system` field is lifted into a system
/// message, mirroring [`crate::MessagesClient`].
fn parse_anthropic_request(body: &serde_json::Value) -> Result<ChatRequest, potato::HttpResponse> {
    let mut messages = Vec::new();
    if let Some(system) = body["system"].as_str() {
        messages.push(ChatMessage::system(system));
    }
    if let Some(entries) = body["messages"].as_array() {
        for entry in entries {
            let role = role_of(entry["role"].as_str().unwrap_or("user"));
            messages.push(ChatMessage::new(role, content_text(&entry["content"])));
        }
    }
    let mut params = crate::llm::GenerationParams::default();
    apply_params(body, &mut params);
    Ok(ChatRequest {
        model: body["model"].as_str().map(str::to_owned),
        messages,
        params,
        stream: body["stream"].as_bool().unwrap_or(false),
    })
}

/// Resolves the target model: the named one, or the only mounted model.
fn resolve<'m>(
    models: &'m HashMap<String, LocalClient>,
    name: Option<&str>,
) -> Result<&'m LocalClient, potato::HttpResponse> {
    match name {
        Some(name) => models.get(name).ok_or_else(|| {
            error_response(
                Flavor::OpenAi,
                400,
                &Error::InvalidConfig(format!(
                    "model '{name}' is not mounted; mounted: {:?}",
                    models.keys().collect::<Vec<_>>()
                )),
            )
        }),
        None => {
            let mut iter = models.values();
            let first = iter.next().ok_or_else(|| {
                error_response(
                    Flavor::OpenAi,
                    500,
                    &Error::InvalidConfig("no model is mounted".to_owned()),
                )
            })?;
            if iter.next().is_some() {
                return Err(error_response(
                    Flavor::OpenAi,
                    400,
                    &Error::InvalidConfig(
                        "several models are mounted; the request must name one".to_owned(),
                    ),
                ));
            }
            Ok(first)
        }
    }
}

/// Prepares a per-request session: a fresh history with everything except
/// the final user turn (which `chat_stream` appends itself).
fn prepare_session(
    client: &LocalClient,
    request: &ChatRequest,
) -> Result<LocalClient, Error> {
    let mut session = client.clone();
    session.clear_messages();
    let history = &request.messages[..request.messages.len().saturating_sub(1)];
    session.set_messages(history.to_vec());
    session.set_generation_params(request.params.clone());
    Ok(session)
}

/// Runs one generation and returns the full assistant text.
async fn generate(session: &mut LocalClient, request: &ChatRequest) -> Result<String, Error> {
    let prompt = request
        .messages
        .last()
        .map(|message| message.content.clone())
        .unwrap_or_default();
    session.chat(prompt).await
}

/// Runs one streaming generation, feeding every content delta to `emit`.
/// Returns once the stream ends (Done, error or cancellation).
async fn stream_generation<F, Fut>(
    session: &mut LocalClient,
    request: &ChatRequest,
    emit: F,
) -> Result<(), Error>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), Error>>,
{
    let prompt = request
        .messages
        .last()
        .map(|message| message.content.clone())
        .unwrap_or_default();
    let mut receiver = session.chat_stream(prompt).await?;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            StreamChunk::Content(text) => emit(text).await?,
            StreamChunk::Done => break,
            // Mid-stream engine failures are forwarded as a final content
            // delta; the protocol frames around it stay valid.
            StreamChunk::Error(message) => emit(message).await?,
        }
    }
    Ok(())
}

async fn execute(
    plan: Result<Plan, potato::HttpResponse>,
    models: &HashMap<String, LocalClient>,
) -> Option<potato::HttpResponse> {
    let plan = match plan {
        Ok(plan) => plan,
        Err(response) => return Some(response),
    };
    match plan {
        Plan::OpenAiModels => {
            let mut names: Vec<&String> = models.keys().collect();
            names.sort();
            let data: Vec<serde_json::Value> = names
                .into_iter()
                .map(|name| {
                    serde_json::json!({
                        "id": name, "object": "model", "created": unix_seconds(),
                        "owned_by": "channel-local",
                    })
                })
                .collect();
            Some(json_response(
                200,
                &serde_json::json!({"object": "list", "data": data}),
            ))
        }
        Plan::OllamaTags => {
            let mut names: Vec<&String> = models.keys().collect();
            names.sort();
            let list: Vec<serde_json::Value> = names
                .into_iter()
                .map(|name| serde_json::json!({"name": name, "model": name}))
                .collect();
            Some(json_response(200, &serde_json::json!({"models": list})))
        }
        Plan::OpenAiChat(request) => Some(openai_chat(models, request).await),
        Plan::OpenAiResponses(request) => Some(openai_responses(models, request).await),
        Plan::AnthropicMessages(request) => Some(anthropic_messages(models, request).await),
        Plan::OllamaChat(request) => Some(ollama_chat(models, request).await),
    }
}

fn unix_seconds() -> i64 {
    crate::llm::unix_seconds()
}

async fn openai_chat(
    models: &HashMap<String, LocalClient>,
    request: ChatRequest,
) -> potato::HttpResponse {
    let flavor = Flavor::OpenAi;
    let client = match resolve(models, request.model.as_deref()) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut session = match prepare_session(client, &request) {
        Ok(session) => session,
        Err(error) => return error_response(flavor, error_status(&error), &error),
    };
    let model = client.model().unwrap_or_default().to_owned();
    let id = format!("chatcmpl-{}", crate::llm::unix_micros());
    if request.stream {
        let (sender, response) =
            match OpenAISender::new(&id, "chat.completion.chunk", &model, "assistant", 64).await {
                Ok(parts) => parts,
                Err(error) => return error_response(flavor, error_status(&error), &error),
            };
        // Mid-stream engine errors were already forwarded as content by
        // `stream_generation`; the closing frames are sent unconditionally.
        let sink = &sender;
        let _ = stream_generation(&mut session, &request, |text| async move {
            sink.send(text).await
        })
        .await;
        if let Err(error) = sender.send_finish("stop").await {
            return error_response(flavor, error_status(&error), &error);
        }
        return response;
    }
    match generate(&mut session, &request).await {
        Ok(content) => json_response(
            200,
            &serde_json::json!({
                "id": id,
                "object": "chat.completion",
                "created": unix_seconds(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            }),
        ),
        Err(error) => error_response(flavor, error_status(&error), &error),
    }
}

async fn openai_responses(
    models: &HashMap<String, LocalClient>,
    request: ChatRequest,
) -> potato::HttpResponse {
    let flavor = Flavor::Responses;
    let client = match resolve(models, request.model.as_deref()) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut session = match prepare_session(client, &request) {
        Ok(session) => session,
        Err(error) => return error_response(flavor, error_status(&error), &error),
    };
    let model = client.model().unwrap_or_default().to_owned();
    let id = format!("resp_{}", crate::llm::unix_micros());
    if request.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let created = serde_json::json!({
            "type": "response.created",
            "response": {"id": id, "object": "response", "model": model, "status": "in_progress"},
        });
        let _ = tx
            .send(format!("data: {created}\n\n").into_bytes())
            .await;
        let result = stream_generation(&mut session, &request, |text| {
            let tx = tx.clone();
            async move {
                let frame = serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": text,
                });
                tx.send(format!("data: {frame}\n\n").into_bytes())
                    .await
                    .map_err(|_| Error::Closed)
            }
        })
        .await;
        let completed = serde_json::json!({
            "type": "response.completed",
            "response": {"id": id, "object": "response", "model": model, "status": "completed"},
        });
        let _ = tx
            .send(format!("data: {completed}\n\n").into_bytes())
            .await;
        if let Err(error) = result {
            return error_response(flavor, error_status(&error), &error);
        }
        let mut response = potato::HttpResponse::sse(rx);
        response.http_code = 200;
        return response;
    }
    match generate(&mut session, &request).await {
        Ok(content) => json_response(
            200,
            &serde_json::json!({
                "id": id,
                "object": "response",
                "created_at": unix_seconds(),
                "model": model,
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": format!("msg_{id}"),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": content, "annotations": []}],
                }],
            }),
        ),
        Err(error) => error_response(flavor, error_status(&error), &error),
    }
}

async fn anthropic_messages(
    models: &HashMap<String, LocalClient>,
    request: ChatRequest,
) -> potato::HttpResponse {
    let flavor = Flavor::Anthropic;
    let client = match resolve(models, request.model.as_deref()) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut session = match prepare_session(client, &request) {
        Ok(session) => session,
        Err(error) => return error_response(flavor, error_status(&error), &error),
    };
    let model = client.model().unwrap_or_default().to_owned();
    let id = format!("msg_{}", crate::llm::unix_micros());
    if request.stream {
        let (sender, response) = match AnthropicSender::new(&id, &model, "assistant", 64).await {
            Ok(parts) => parts,
            Err(error) => return error_response(flavor, error_status(&error), &error),
        };
        let sink = &sender;
        let _ = stream_generation(&mut session, &request, |text| async move {
            sink.send(text).await
        })
        .await;
        if let Err(error) = sender.send_finish().await {
            return error_response(flavor, error_status(&error), &error);
        }
        return response;
    }
    match generate(&mut session, &request).await {
        Ok(content) => json_response(
            200,
            &serde_json::json!({
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [{"type": "text", "text": content}],
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }),
        ),
        Err(error) => error_response(flavor, error_status(&error), &error),
    }
}

async fn ollama_chat(
    models: &HashMap<String, LocalClient>,
    request: ChatRequest,
) -> potato::HttpResponse {
    let flavor = Flavor::Ollama;
    let client = match resolve(models, request.model.as_deref()) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let mut session = match prepare_session(client, &request) {
        Ok(session) => session,
        Err(error) => return error_response(flavor, error_status(&error), &error),
    };
    let model = client.model().unwrap_or_default().to_owned();
    if request.stream {
        let (sender, response) = match OllamaSender::new(&model, 64).await {
            Ok(parts) => parts,
            Err(error) => return error_response(flavor, error_status(&error), &error),
        };
        let sink = &sender;
        let _ = stream_generation(&mut session, &request, |text| async move {
            sink.send_chat(text).await
        })
        .await;
        if let Err(error) = sender.send_chat_finish().await {
            return error_response(flavor, error_status(&error), &error);
        }
        return response;
    }
    match generate(&mut session, &request).await {
        Ok(content) => json_response(
            200,
            &serde_json::json!({
                "model": model,
                "created_at": crate::llm::ollama::rfc3339_utc_now(),
                "message": {"role": "assistant", "content": content},
                "done": true,
                "done_reason": "stop",
            }),
        ),
        Err(error) => error_response(flavor, error_status(&error), &error),
    }
}
