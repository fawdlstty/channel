# channel

![version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Ffawdlstty%2Fchannel/main/Cargo.toml&query=package.version&label=version)
![status](https://img.shields.io/github/actions/workflow/status/fawdlstty/channel/rust.yml)

English | [简体中文](README.zh.md)

**One Rust crate covering every way to reach an AI model: drive coding harnesses, call LLM APIs directly, or run models locally in-process — all behind one uniform async API style.**

## Drive coding harnesses (default)

One normalized async session API over the major AI coding harnesses — **Codex, Claude Code, OpenCode, Zed, ZCode, DeepSeek, and Hermes**. Protocol differences (Codex App Server, Agent Client Protocol, managed CLIs) disappear into a single event stream of message deltas (with reasoning), tool/file/command activity, permission requests, and final results.

```rust
use channel::{HarnessKind, Session, SessionConfig, SessionEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = SessionConfig::for_kind(HarnessKind::Codex);
    config.set_observability(false); // invisible to the harness's own UI

    // Creating a session also submits the first message.
    let mut session = Session::create(config, "Introduce this repository").await?;

    while let Some(event) = session.wait_event().await? {
        match event {
            SessionEvent::Message(ev) if ev.reasoning => print!("[reasoning]{}", ev.text),
            SessionEvent::Message(ev) => print!("{}", ev.text),
            SessionEvent::Activity(ev) => println!("{} ({:?})", ev.display, ev.kind),
            SessionEvent::Permission(ev) => println!("permission needed: {}", ev.detail),
            SessionEvent::Finished(finish) => println!("\n-- {} --", finish.text),
        }
    }
    Ok(())
}
```

## Call LLM providers directly (`llm`)

Typed clients for four chat protocols over the potato HTTP stack: **OpenAI Chat Completions** (`ChatCompletionsClient`), **OpenAI Responses** (`ResponsesClient`), **Anthropic Messages** (`MessagesClient`) and **Ollama** (`OllamaClient`). Any OpenAI-compatible endpoint works as a base URL. All four share one surface: system prompt, model selection validated against the endpoint's model list, reasoning effort, message history, state (de)serialization, plus `chat()` for one-shot calls and `chat_stream()` for SSE/NDJSON streaming through an `mpsc::Receiver<StreamChunk>`.

The feature also ships server-side stream emitters (`OpenAISender`, `AnthropicSender`, `OllamaSender`) that frame your own data as protocol-conformant SSE/NDJSON streams, so a custom backend can serve OpenAI/Anthropic/Ollama-style streaming endpoints.

```rust
let mut client = channel::ChatCompletionsClient::new(base_url, api_key);
client.set_model("gpt-4o-mini").await?;
let mut receiver = client.chat_stream("Explain what an SSE stream is.").await?;
while let Some(chunk) = receiver.recv().await {
    // StreamChunk::Content(text) | Error(message) | Done
}
```

## Run models locally in-process (`local-safetensors-cpu`, `local-gguf-cpu`)

`LocalClient::load` runs a model straight from disk with no server in between: a **safetensors** model directory goes through the pure-Rust candle backend, and a **GGUF** file goes through the llama.cpp backend (opt-in `local-gguf-cpu`, needs cmake and a C++ toolchain). Load options cover context size, thread count, GPU layer offload and chat-template overrides; generation parameters cover temperature, top-p/top-k, repetition penalty, stop sequences and more. The conversation API is identical to the HTTP clients.

**Supported model architectures**

- **safetensors** (candle backend): a fixed whitelist — `config.json` `model_type` must be `llama`, `qwen2`, `qwen3`, `phi3`, or `gemma` (via candle-transformers 0.11); other architectures are rejected with a hint to use GGUF instead.
- **GGUF** (llama.cpp backend): no whitelist of its own — every architecture the bundled llama.cpp (llama-cpp-2 0.1.156) supports loads directly, around 140 in total: llama/llama4, qwen2/qwen3 (incl. MoE and VL variants), gemma/gemma2/gemma3/gemma3n, phi2/phi3, the deepseek and GLM families, mistral3/mistral4, gpt-oss, smollm3, nemotron, and more.

```rust
let mut client = channel::LocalClient::load("./Qwen3-0.6B").await?;
client.set_system_prompt("You are a concise assistant.");
let reply = client.chat("Explain what an SSE stream is.").await?;
```

## Serve local models over HTTP (ships with `local-safetensors-cpu`)

`LocalLlmServer` mounts one or more local models and exposes them on the standard protocol endpoints — `GET /v1/models`, `POST /v1/chat/completions`, `POST /v1/responses`, `POST /v1/messages`, `POST /api/chat`, `GET /api/tags` — so **any OpenAI-, Anthropic- or Ollama-compatible client**, including channel's own protocol clients, can talk to a local model. Bearer-token auth is optional.

```rust
let mut server = channel::LocalLlmServer::bind("127.0.0.1:8817")?;
server.mount_model("./Qwen3-0.6B")?;
server.serve().await?;
```

## Feature flags

| Feature | Enables |
|---|---|
| *(default)* | Harness sessions only |
| `llm` | Four direct LLM protocol clients plus server-side stream emitters |
| `ccswitch` | cc-switch provider switching (SQLite config db + TOML config + local upstream proxy) |
| `local-safetensors-cpu` | In-process local inference via the candle backend (safetensors, pure Rust), plus the local-model HTTP server |
| `local-safetensors-cuda` / `local-safetensors-metal` | GPU support for the candle backend |
| `local-gguf-cpu` | llama.cpp backend for GGUF models (implies `local-safetensors-cpu`) |
| `local-gguf-cuda` / `local-gguf-metal` / `local-gguf-vulkan` | CUDA / Metal / Vulkan offload for the llama.cpp backend |
| `local-safetensors-full` / `local-gguf-full` | Every backend of the safetensors / GGUF family respectively |
| `full` | Every feature of this crate |
| `harness` | Desktop-sensing harness: UIA/AT-SPI control-tree awareness, screen capture, actuation tools, JSON-lines service — every platform integration compiles in; the runtime environment picks the active one |

## Examples

| Example | Run | Covers |
|---|---|---|
| [basic.rs](examples/basic.rs) | `cargo run --example basic` | harness session |
| [llm_chat.rs](examples/llm_chat.rs) | `cargo run --features llm --example llm_chat` | direct LLM call |
| [local_chat.rs](examples/local_chat.rs) | `cargo run --features local-safetensors-cpu --example local_chat -- ./Qwen3-0.6B` | local model |
| [local_server.rs](examples/local_server.rs) | `cargo run --features local-safetensors-cpu --example local_server -- ./Qwen3-0.6B` | local model server |

The detailed harness API walkthrough (in Chinese) lives in [manual.md](manual.md).

- **License**: MIT
