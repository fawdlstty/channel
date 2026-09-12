//! Local model loading and in-process inference.
//!
//! [`LocalClient`] loads a safetensors model directory (candle backend,
//! pure Rust) or a GGUF file (llama.cpp backend, opt-in `local-gguf-cpu`)
//! straight from disk and speaks the same conversation interface as the
//! four HTTP protocol clients: [`LocalClient::set_system_prompt`],
//! [`LocalClient::chat`], [`LocalClient::chat_stream`],
//! [`LocalClient::messages`], [`LocalClient::serialize`].
//!
//! ```no_run
//! # async fn demo() -> Result<(), channel::Error> {
//! let mut client = channel::LocalClient::load("./Qwen3-0.6B").await?;
//! client.set_system_prompt("You are a concise assistant.");
//! let reply = client.chat("Explain SSE in one sentence.").await?;
//! # Ok(())
//! # }
//! ```

pub(crate) mod candle_backend;
pub(crate) mod engine;
pub(crate) mod llama_backend;
mod template;
pub mod server;

use crate::llm::{
    finalize_assistant_message, record_assistant_delta, ChatMessage, MessageLog, MessageRole,
    StreamChunk,
};
use crate::protocol::{Error, ReasoningEffort};
use engine::{EngineActor, LocalEngine};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// The provider identifier of the serialized [`LocalClient`] state.
pub(crate) const PROVIDER_ID: &str = "local";

/// Tokens kept free for the generation when deciding whether the rendered
/// prompt fits the context window.
const GENERATION_RESERVE: u32 = 64;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Which inference backend a model was dispatched to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalBackendKind {
    /// llama.cpp (GGUF weights; `local-gguf-cpu` feature family).
    #[serde(rename = "llama_cpp")]
    LlamaCpp,
    /// candle (safetensors weights; pure-Rust `local` baseline).
    #[serde(rename = "candle")]
    Candle,
}

/// Load-time parameters; fixed once the model is loaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOptions {
    /// Context window size in tokens. Defaults to 4096.
    pub n_ctx: u32,
    /// CPU inference threads; `None` lets each backend pick.
    pub n_threads: Option<u32>,
    /// GPU layers to offload; `None` stays on CPU. Only the llama.cpp
    /// backend honors this (and only with a GPU feature enabled).
    pub n_gpu_layers: Option<u32>,
    /// Memory-map the weights instead of reading them into memory.
    /// Defaults to `true`.
    pub use_mmap: bool,
    /// Overrides the model's chat template with this Jinja source.
    pub explicit_template: Option<String>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadOptions {
    /// The default options: a 4096-token context, mmap loading, backend
    /// default threads, no GPU offload and the model's own template.
    pub fn new() -> Self {
        Self {
            n_ctx: 4096,
            n_threads: None,
            n_gpu_layers: None,
            use_mmap: true,
            explicit_template: None,
        }
    }
}

/// Per-request sampling parameters. Defaults follow the ollama
/// conventions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Sampling temperature. Defaults to 0.8; values below ~1e-7 select
    /// greedy decoding.
    pub temperature: f64,
    /// Nucleus sampling probability mass. Defaults to 0.9.
    pub top_p: f64,
    /// Top-k sampling width. Defaults to 40; `<= 0` disables top-k.
    pub top_k: i32,
    /// Repetition penalty applied over the recent context. Defaults to
    /// 1.1 (disabled at exactly 1.0).
    pub repeat_penalty: f32,
    /// Maximum tokens to generate. Defaults to 1024.
    pub max_tokens: u32,
    /// Sampling seed; `None` picks a fresh random seed per request.
    pub seed: Option<u32>,
    /// Stop sequences; generation ends when one appears, and the sequence
    /// itself is never part of the output.
    pub stop: Vec<String>,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: 0.8,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.1,
            max_tokens: 1024,
            seed: None,
            stop: Vec::new(),
        }
    }
}

/// Read-only model metadata, computed at load time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalModelMeta {
    /// The path the model was loaded from.
    pub path: PathBuf,
    /// The backend the path was dispatched to.
    pub backend: LocalBackendKind,
    /// The architecture identifier (`"qwen3"`, `"llama"`, ...).
    pub architecture: String,
    /// The quantization label (`"Q4_K_M"`), or `None` for unquantized
    /// safetensors weights.
    pub quantization: Option<String>,
    /// The context window: the model's trained maximum for safetensors,
    /// the effective `n_ctx` for llama.cpp.
    pub context_length: u32,
}

// ---------------------------------------------------------------------------
// Path dispatch (§4.3)
// ---------------------------------------------------------------------------

/// The internal dispatch decision for a model path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// A `*.gguf` file (possibly the first shard of a split set).
    GgufFile(PathBuf),
    /// A directory with `config.json` + `*.safetensors`.
    SafetensorsDir(PathBuf),
    /// A single `*.safetensors` file; siblings supply the metadata.
    SafetensorsFile(PathBuf),
    /// A directory with `config.json` + `*.gguf` weights.
    GgufDir(PathBuf),
}

fn first_with_extension(dir: &Path, extension: &str) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(extension))
        .collect();
    files.sort();
    files.into_iter().next()
}

/// Maps a model path onto a backend dispatch decision:
///
/// 1. a `*.gguf` file (including the `-00001-of-` first shard) goes to
///    llama.cpp;
/// 2. a directory with `config.json` plus `*.safetensors` goes to candle,
///    with `*.gguf` weights it goes to llama.cpp (first shard);
/// 3. a single `*.safetensors` file is treated like its directory.
pub(crate) fn classify(path: &Path) -> Result<Dispatch, Error> {
    if path.is_dir() {
        if !path.join("config.json").is_file() {
            return Err(Error::InvalidConfig(format!(
                "model directory {} is missing config.json",
                path.display()
            )));
        }
        if first_with_extension(path, "safetensors").is_some() {
            return Ok(Dispatch::SafetensorsDir(path.to_path_buf()));
        }
        if let Some(gguf) = first_with_extension(path, "gguf") {
            return Ok(Dispatch::GgufDir(gguf));
        }
        return Err(Error::InvalidConfig(format!(
            "model directory {} contains neither *.safetensors nor *.gguf weights",
            path.display()
        )));
    }
    if !path.exists() {
        return Err(Error::InvalidConfig(format!(
            "model path {} does not exist",
            path.display()
        )));
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("gguf") => Ok(Dispatch::GgufFile(path.to_path_buf())),
        Some("safetensors") => Ok(Dispatch::SafetensorsFile(path.to_path_buf())),
        _ => Err(Error::InvalidConfig(format!(
            "unsupported model file {}: expected a .gguf file, a .safetensors file \
             or a model directory",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Engine bootstrap
// ---------------------------------------------------------------------------

/// Everything a backend load produces: the engine (to be moved onto the
/// actor thread), the metadata and the chat-template bundle.
pub(crate) struct EngineBoot {
    pub engine: BackendEngine,
    pub meta: LocalModelMeta,
    /// The model-provided chat template, if any.
    pub template: Option<String>,
    pub bos_token: Option<String>,
    pub eos_token: Option<String>,
    /// The effective context window the client truncates against.
    pub n_ctx: u32,
}

/// The engine variant selected by the path dispatch.
pub(crate) enum BackendEngine {
    #[cfg(feature = "local-gguf-cpu")]
    LlamaCpp(llama_backend::LlamaEngine),
    /// Boxed: the candle engines are far larger than the llama.cpp one.
    Candle(Box<candle_backend::CandleEngine>),
}

impl LocalEngine for BackendEngine {
    fn load(path: &Path, options: &LoadOptions) -> Result<Self, Error>
    where
        Self: Sized,
    {
        // `dispatch_and_load` owns the feature gating for GGUF paths (it
        // reports `UnsupportedCapability` when `local-gguf-cpu` is compiled
        // out).
        Ok(dispatch_and_load(path, options)?.engine)
    }

    fn meta(&self) -> LocalModelMeta {
        match self {
            #[cfg(feature = "local-gguf-cpu")]
            BackendEngine::LlamaCpp(engine) => engine.meta(),
            BackendEngine::Candle(engine) => engine.meta(),
        }
    }

    fn tokenize(&self, text: &str) -> Result<Vec<u32>, Error> {
        match self {
            #[cfg(feature = "local-gguf-cpu")]
            BackendEngine::LlamaCpp(engine) => engine.tokenize(text),
            BackendEngine::Candle(engine) => engine.tokenize(text),
        }
    }

    fn generate(
        &mut self,
        prompt_tokens: &[u32],
        params: &GenerationParams,
        emit: &mut dyn FnMut(&str) -> bool,
    ) -> Result<engine::GenerateFinish, Error> {
        match self {
            #[cfg(feature = "local-gguf-cpu")]
            BackendEngine::LlamaCpp(engine) => engine.generate(prompt_tokens, params, emit),
            BackendEngine::Candle(engine) => engine.generate(prompt_tokens, params, emit),
        }
    }

    fn reset(&mut self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "local-gguf-cpu")]
            BackendEngine::LlamaCpp(engine) => engine.reset(),
            BackendEngine::Candle(engine) => engine.reset(),
        }
    }
}

/// Dispatches `path` and loads the matching backend.
pub(crate) fn dispatch_and_load(path: &Path, options: &LoadOptions) -> Result<EngineBoot, Error> {
    match classify(path)? {
        Dispatch::GgufFile(file) | Dispatch::GgufDir(file) => {
            llama_backend::load_boot(&file, path, options)
        }
        Dispatch::SafetensorsDir(dir) => candle_backend::load_boot(&dir, None, path, options),
        Dispatch::SafetensorsFile(file) => {
            let dir = file
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            candle_backend::load_boot(&dir, Some(&file), path, options)
        }
    }
}

// ---------------------------------------------------------------------------
// Reasoning effort mapping (D7)
// ---------------------------------------------------------------------------

/// The weak sampling heuristic behind
/// [`LocalClient::set_reasoning_effort`]: local backends have no reasoning
/// budget knob, so each effort level overrides `temperature`/`top_p` at
/// generation time (the stored [`GenerationParams`] are untouched and
/// `None` restores them).
///
/// | effort  | temperature | top_p |
/// |---------|-------------|-------|
/// | Minimal | 0.1         | 1.0   |
/// | Low     | 0.3         | 0.95  |
/// | Medium  | 0.6         | 0.9   |
/// | High    | 0.8         | 0.9   |
/// | XHigh   | 0.9         | 0.9   |
/// | Max     | 1.0         | 0.95  |
///
/// Temperature rises monotonically with the effort; the nucleus widens
/// back toward 1.0 at the extremes so near-greedy decoding stays exact
/// (Minimal) and maximum-effort long-form stays coherent at temperature
/// 1.0 (Max). This is a heuristic, not a calibrated mapping.
pub(crate) fn effort_sampling(effort: ReasoningEffort) -> (f64, f64) {
    match effort {
        ReasoningEffort::Minimal => (0.1, 1.0),
        ReasoningEffort::Low => (0.3, 0.95),
        ReasoningEffort::Medium => (0.6, 0.9),
        ReasoningEffort::High => (0.8, 0.9),
        ReasoningEffort::XHigh => (0.9, 0.9),
        ReasoningEffort::Max => (1.0, 0.95),
    }
}

// ---------------------------------------------------------------------------
// LocalClient
// ---------------------------------------------------------------------------

/// The resolved chat-template bundle.
#[derive(Clone)]
struct TemplateInfo {
    source: String,
    bos_token: Option<String>,
    eos_token: Option<String>,
}

/// State shared between the client and its clones: the actor handle, the
/// generation mutex and the conversation log.
struct EngineShared {
    actor: EngineActor,
    busy: AtomicBool,
    messages: MessageLog,
}

/// Releases the generation mutex when the streaming forwarder finishes.
struct BusyGuard(Arc<EngineShared>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

/// A client that runs a local model in-process. Cloning is cheap (channel
/// handles plus the shared conversation log); the underlying engine is a
/// single actor thread, so concurrent generations on any clone fail with
/// [`Error::Busy`] until the active stream completes.
#[derive(Clone)]
pub struct LocalClient {
    shared: Arc<EngineShared>,
    path: PathBuf,
    model_name: String,
    load_options: LoadOptions,
    generation_params: GenerationParams,
    reasoning_effort: Option<ReasoningEffort>,
    meta: LocalModelMeta,
    template: Option<TemplateInfo>,
    n_ctx: u32,
}

impl std::fmt::Debug for LocalClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalClient")
            .field("path", &self.path)
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

/// The serializable [`LocalClient`] state (`provider = "local"`). Split
/// out from the client so the round trip can be unit tested without a
/// model on disk.
#[derive(Debug, Serialize, Deserialize)]
struct LocalClientState {
    provider: String,
    path: PathBuf,
    backend: LocalBackendKind,
    load_options: LoadOptions,
    generation_params: GenerationParams,
    messages: Vec<ChatMessage>,
}

fn model_name_from(path: &Path) -> String {
    let name = if path.is_dir() {
        path.file_name()
    } else {
        path.file_stem()
    };
    name.and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "local-model".to_owned())
}

impl LocalClient {
    /// Loads a model and infers with it in-process.
    ///
    /// `path` may be a `*.gguf` file (requires the `local-gguf-cpu`
    /// feature), a `*.safetensors` file, or a model directory containing
    /// `config.json` plus weights; see the dispatch rules in the crate
    /// documentation. Loading is heavy and runs on a blocking thread, so
    /// the async entry points never stall the runtime.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::load_with(path, LoadOptions::new()).await
    }

    /// [`LocalClient::load`] with explicit [`LoadOptions`].
    pub async fn load_with(path: impl AsRef<Path>, options: LoadOptions) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::load_with_blocking(&path, options))
            .await
            .map_err(|error| Error::Backend(format!("model load task failed: {error}")))?
    }

    /// Synchronous variant of [`LocalClient::load`].
    pub fn load_blocking(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::load_with_blocking(path, LoadOptions::new())
    }

    /// Synchronous variant of [`LocalClient::load_with`].
    pub fn load_with_blocking(path: impl AsRef<Path>, options: LoadOptions) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let boot = dispatch_and_load(&path, &options)?;
        let actor = EngineActor::spawn(boot.engine);
        // The explicit template wins over whatever the model embeds.
        let template = options
            .explicit_template
            .clone()
            .map(|source| TemplateInfo {
                source,
                bos_token: boot.bos_token.clone(),
                eos_token: boot.eos_token.clone(),
            })
            .or_else(|| {
                boot.template.map(|source| TemplateInfo {
                    source,
                    bos_token: boot.bos_token.clone(),
                    eos_token: boot.eos_token.clone(),
                })
            });
        Ok(Self {
            shared: Arc::new(EngineShared {
                actor,
                busy: AtomicBool::new(false),
                messages: Arc::new(RwLock::new(Vec::new())),
            }),
            model_name: model_name_from(&path),
            path,
            load_options: options,
            generation_params: GenerationParams::default(),
            reasoning_effort: None,
            meta: boot.meta,
            template,
            n_ctx: boot.n_ctx,
        })
    }

    /// Adds a system prompt to the conversation history.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.write_messages(|messages| messages.push(ChatMessage::system(prompt)));
    }

    /// The model name: the file stem (`qwen3-0.6b-q4_k_m`) or directory
    /// name of the loaded path.
    pub fn model(&self) -> Option<&str> {
        Some(&self.model_name)
    }

    /// Returns a copy of the conversation history.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.shared
            .messages
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replaces the conversation history, keeping the system messages
    /// already logged (the same semantics as the HTTP clients).
    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.write_messages(|history| {
            history.retain(|message| message.role == MessageRole::System);
            history.extend(messages);
        });
    }

    /// Appends an assistant message to the history without generating.
    pub fn append_assistant_message(&mut self, content: impl Into<String>) {
        self.write_messages(|messages| messages.push(ChatMessage::assistant(content)));
    }

    /// Removes every message from the history, including system prompts.
    /// Unlike [`LocalClient::set_messages`], which keeps the logged system
    /// entries, this gives the caller a blank conversation.
    pub fn clear_messages(&mut self) {
        self.write_messages(|messages| messages.clear());
    }

    /// Sends a message and returns the full assistant reply.
    pub async fn chat(&mut self, message: impl Into<String>) -> Result<String, Error> {
        let mut receiver = self.chat_stream(message).await?;
        let mut content = String::new();
        let mut failure = None;
        while let Some(chunk) = receiver.recv().await {
            match chunk {
                StreamChunk::Content(text) => content.push_str(&text),
                StreamChunk::Error(message) => failure = Some(message),
                StreamChunk::Done => break,
            }
        }
        match failure {
            Some(message) => Err(Error::Backend(message)),
            None => Ok(content),
        }
    }

    /// Sends a message and streams the assistant reply token by token.
    /// The trailing assistant entry of the message log is updated with
    /// every increment, exactly like the HTTP clients' streams.
    ///
    /// Dropping the returned receiver cancels the generation; the engine
    /// is reset and the next request works normally.
    pub async fn chat_stream(
        &mut self,
        message: impl Into<String>,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>, Error> {
        self.shared.actor.check()?;
        if self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(Error::Busy);
        }
        self.write_messages(|messages| messages.push(ChatMessage::user(message)));
        match self.start_generation().await {
            Ok(receiver) => Ok(receiver),
            Err(error) => {
                // No trace of the failed turn may survive in the history.
                self.rollback_last_message();
                self.shared.busy.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    /// Serializes the client state (provider, model path, load and
    /// generation parameters, history) to a JSON string.
    pub fn serialize(&self) -> Result<String, Error> {
        let state = LocalClientState {
            provider: PROVIDER_ID.to_owned(),
            path: self.path.clone(),
            backend: self.meta.backend,
            load_options: self.load_options.clone(),
            generation_params: self.generation_params.clone(),
            messages: self.messages(),
        };
        serde_json::to_string(&state).map_err(|error| {
            Error::ProtocolError(format!("failed to serialize local client: {error}"))
        })
    }

    /// Restores a client from [`LocalClient::serialize`] output by
    /// re-loading the model (a heavyweight operation on par with the
    /// first load) and then replaying the history and parameters.
    ///
    /// A moved or deleted model file fails with [`Error::InvalidConfig`]
    /// carrying the original path.
    pub fn deserialize(json: &str) -> Result<Self, Error> {
        let state: LocalClientState = serde_json::from_str(json).map_err(|error| {
            Error::ProtocolError(format!("invalid serialized local client: {error}"))
        })?;
        if state.provider != PROVIDER_ID {
            return Err(Error::InvalidConfig(format!(
                "serialized client uses provider '{}' but this client implements '{PROVIDER_ID}'",
                state.provider
            )));
        }
        if !state.path.exists() {
            return Err(Error::InvalidConfig(format!(
                "model path no longer exists: {}",
                state.path.display()
            )));
        }
        let mut client = Self::load_with_blocking(&state.path, state.load_options)?;
        *client
            .shared
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state.messages;
        client.generation_params = state.generation_params;
        Ok(client)
    }

    /// Sets the per-request generation parameters.
    pub fn set_generation_params(&mut self, params: GenerationParams) {
        self.generation_params = params;
    }

    /// The current generation parameters.
    pub fn generation_params(&self) -> &GenerationParams {
        &self.generation_params
    }

    /// The model metadata (architecture, backend, quantization, context
    /// length).
    pub fn meta(&self) -> &LocalModelMeta {
        &self.meta
    }

    /// Sets or clears the reasoning-effort override. Local engines have no
    /// reasoning budget, so the effort weakly maps onto sampling
    /// parameters for subsequent generations (see the table on
    /// [`LocalClient`]'s effort mapping); `None` restores the stored
    /// [`GenerationParams`] values.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
    }

    /// The currently configured reasoning effort.
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    // -- internals ----------------------------------------------------------

    fn write_messages(&self, write: impl FnOnce(&mut Vec<ChatMessage>)) {
        let mut history = self
            .shared
            .messages
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write(&mut history);
    }

    fn rollback_last_message(&self) {
        self.write_messages(|history| {
            history.pop();
        });
    }

    /// Renders, tokenizes and context-checks the prompt, then hands it to
    /// the actor and spawns the forwarding task. Only called with the
    /// busy flag held; failures propagate to [`LocalClient::chat_stream`],
    /// which owns the rollback.
    async fn start_generation(
        &mut self,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamChunk>, Error> {
        let (tokens, params) = self.prepare_prompt().await?;
        let (actor_tx, actor_rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);
        self.shared.actor.generate(tokens, params, actor_tx)?;
        let (consumer_tx, consumer_rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);
        spawn_forwarder(self.shared.clone(), actor_rx, consumer_tx);
        Ok(consumer_rx)
    }

    /// Renders the history through the chat template and tokenizes it,
    /// dropping the oldest non-system turns (with a one-line notice) until
    /// the prompt fits the context window.
    async fn prepare_prompt(&self) -> Result<(Vec<u32>, GenerationParams), Error> {
        let mut params = self.generation_params.clone();
        if let Some(effort) = self.reasoning_effort {
            let (temperature, top_p) = effort_sampling(effort);
            params.temperature = temperature;
            params.top_p = top_p;
        }
        let template = self.template.as_ref().ok_or_else(|| {
            Error::InvalidConfig(
                "the model ships no chat template; pass one through \
                 LoadOptions::explicit_template"
                    .to_owned(),
            )
        })?;
        let mut history = self.messages();
        let limit = self.n_ctx.saturating_sub(GENERATION_RESERVE);
        let mut warned = false;
        loop {
            let prompt = template::render_chat(
                &template.source,
                &history,
                template.bos_token.as_deref(),
                template.eos_token.as_deref(),
            )?;
            let tokens = self.shared.actor.tokenize(&prompt).await?;
            if (tokens.len() as u32) <= limit {
                return Ok((tokens, params));
            }
            // Drop the earliest non-system message that is not the newest
            // turn; keep the stored history intact (only the prompt is
            // truncated).
            let droppable = history
                .iter()
                .enumerate()
                .take(history.len().saturating_sub(1))
                .find(|(_, message)| message.role != MessageRole::System)
                .map(|(index, _)| index);
            match droppable {
                Some(index) => {
                    history.remove(index);
                    if !warned {
                        eprintln!(
                            "channel: the prompt exceeds the {}-token context window; \
                             dropping older messages (raise LoadOptions::n_ctx to keep them)",
                            self.n_ctx
                        );
                        warned = true;
                    }
                }
                None => {
                    return Err(Error::InvalidConfig(format!(
                        "the rendered prompt needs {} tokens but the context window is {} \
                         (with a {}-token generation reserve); increase LoadOptions::n_ctx",
                        tokens.len(),
                        self.n_ctx,
                        GENERATION_RESERVE
                    )));
                }
            }
        }
    }
}

/// Bridges the actor's chunk stream to the consumer channel while keeping
/// the shared message log and the busy flag in order.
fn spawn_forwarder(
    shared: Arc<EngineShared>,
    mut source: tokio::sync::mpsc::Receiver<StreamChunk>,
    destination: tokio::sync::mpsc::Sender<StreamChunk>,
) {
    let guard = BusyGuard(shared.clone());
    tokio::spawn(async move {
        let _busy = guard;
        let mut content = String::new();
        while let Some(chunk) = source.recv().await {
            match chunk {
                StreamChunk::Content(text) => {
                    content.push_str(&text);
                    record_assistant_delta(&shared.messages, &content);
                    if destination.send(StreamChunk::Content(text)).await.is_err() {
                        // Consumer gone: dropping `source` cancels the actor.
                        return;
                    }
                }
                StreamChunk::Error(message) => {
                    let _ = destination.send(StreamChunk::Error(message)).await;
                }
                StreamChunk::Done => {
                    finalize_assistant_message(&shared.messages, content);
                    let _ = destination.send(StreamChunk::Done).await;
                    return;
                }
            }
        }
        // The actor ended without Done (thread exited); finish the stream.
        finalize_assistant_message(&shared.messages, content);
        let _ = destination.send(StreamChunk::Done).await;
    });
}

// ---------------------------------------------------------------------------
// Tests (no model files needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_options_defaults() {
        let options = LoadOptions::default();
        assert_eq!(options.n_ctx, 4096);
        assert_eq!(options.n_threads, None);
        assert_eq!(options.n_gpu_layers, None);
        assert!(options.use_mmap);
        assert_eq!(options.explicit_template, None);
        assert_eq!(options, LoadOptions::new());
    }

    #[test]
    fn generation_params_defaults() {
        let params = GenerationParams::default();
        assert_eq!(params.temperature, 0.8);
        assert_eq!(params.top_p, 0.9);
        assert_eq!(params.top_k, 40);
        assert_eq!(params.repeat_penalty, 1.1);
        assert_eq!(params.max_tokens, 1024);
        assert_eq!(params.seed, None);
        assert!(params.stop.is_empty());
    }

    fn touch(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), b"stub").unwrap();
    }

    #[test]
    fn classify_dispatches_all_four_shapes() {
        let root = tempfile::tempdir().unwrap();

        // 1. Plain gguf file.
        touch(root.path(), "model-q4_k_m.gguf");
        let dispatch = classify(&root.path().join("model-q4_k_m.gguf")).unwrap();
        assert!(matches!(dispatch, Dispatch::GgufFile(_)));

        // 2. Safetensors directory.
        let st = root.path().join("st-model");
        std::fs::create_dir_all(&st).unwrap();
        touch(&st, "config.json");
        touch(&st, "tokenizer.json");
        touch(&st, "model-00001-of-00002.safetensors");
        touch(&st, "model-00002-of-00002.safetensors");
        let dispatch = classify(&st).unwrap();
        assert_eq!(dispatch, Dispatch::SafetensorsDir(st.clone()));

        // 3. Single safetensors file.
        let dispatch = classify(&st.join("model-00001-of-00002.safetensors")).unwrap();
        assert!(matches!(dispatch, Dispatch::SafetensorsFile(_)));

        // 4. GGUF directory; the lexicographically first shard is chosen.
        let gd = root.path().join("gguf-model");
        std::fs::create_dir_all(&gd).unwrap();
        touch(&gd, "config.json");
        touch(&gd, "part-00002-of-00002.gguf");
        touch(&gd, "part-00001-of-00002.gguf");
        let dispatch = classify(&gd).unwrap();
        assert_eq!(
            dispatch,
            Dispatch::GgufDir(gd.join("part-00001-of-00002.gguf"))
        );

        // Missing weights / missing config / missing path.
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(classify(&empty), Err(Error::InvalidConfig(_))));

        let no_config = root.path().join("no-config");
        std::fs::create_dir_all(&no_config).unwrap();
        touch(&no_config, "model.safetensors");
        assert!(matches!(classify(&no_config), Err(Error::InvalidConfig(_))));

        assert!(matches!(
            classify(&root.path().join("does-not-exist.gguf")),
            Err(Error::InvalidConfig(_))
        ));

        // Unknown file type.
        touch(root.path(), "weights.bin");
        assert!(matches!(
            classify(&root.path().join("weights.bin")),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = LocalClientState {
            provider: PROVIDER_ID.to_owned(),
            path: PathBuf::from("/models/qwen3-0.6b"),
            backend: LocalBackendKind::Candle,
            load_options: LoadOptions {
                n_ctx: 8192,
                n_threads: Some(4),
                n_gpu_layers: None,
                use_mmap: false,
                explicit_template: Some("{{ messages }}".to_owned()),
            },
            generation_params: GenerationParams {
                temperature: 0.5,
                seed: Some(7),
                stop: vec!["\n\n".to_owned()],
                ..GenerationParams::default()
            },
            messages: vec![ChatMessage::system("be brief"), ChatMessage::user("hi")],
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: LocalClientState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.path, state.path);
        assert_eq!(restored.backend, LocalBackendKind::Candle);
        assert_eq!(restored.load_options, state.load_options);
        assert_eq!(restored.generation_params, state.generation_params);
        assert_eq!(restored.messages.len(), 2);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["provider"], "local");
        assert_eq!(value["backend"], "candle");
        assert_eq!(value["load_options"]["n_ctx"], 8192);
        assert_eq!(value["generation_params"]["seed"], 7);
    }

    #[test]
    fn deserialize_validates_provider_and_path() {
        // Wrong provider.
        let foreign = serde_json::json!({
            "provider": "openai-chat-completions",
            "path": "/models/whatever",
            "backend": "candle",
            "load_options": LoadOptions::new(),
            "generation_params": GenerationParams::default(),
            "messages": [],
        });
        let error = LocalClient::deserialize(&foreign.to_string()).unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));

        // Missing model path: the original path must appear in the error.
        let missing = serde_json::json!({
            "provider": "local",
            "path": "/models/definitely-not-here",
            "backend": "llama_cpp",
            "load_options": LoadOptions::new(),
            "generation_params": GenerationParams::default(),
            "messages": [],
        });
        let error = LocalClient::deserialize(&missing.to_string()).unwrap_err();
        assert!(
            matches!(error, Error::InvalidConfig(ref message) if message.contains("/models/definitely-not-here")),
            "got: {error:?}"
        );

        // Malformed JSON.
        assert!(matches!(
            LocalClient::deserialize("not json"),
            Err(Error::ProtocolError(_))
        ));
    }

    #[test]
    fn reasoning_effort_mapping_is_monotonic() {
        let levels = [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ];
        let samples: Vec<(f64, f64)> = levels.iter().map(|level| effort_sampling(*level)).collect();
        // Temperature strictly increases with the effort level.
        for pair in samples.windows(2) {
            assert!(pair[0].0 < pair[1].0, "not monotonic: {samples:?}");
        }
        // The documented extremes.
        assert_eq!(samples[0], (0.1, 1.0));
        assert_eq!(samples[5], (1.0, 0.95));
        // All values stay in the valid sampling ranges.
        for (temperature, top_p) in samples {
            assert!((0.0..=1.0).contains(&temperature));
            assert!((0.0..=1.0).contains(&top_p));
        }
    }
}
