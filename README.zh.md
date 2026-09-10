# channel

![version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Ffawdlstty%2Fchannel/main/Cargo.toml&query=package.version&label=version)
![status](https://img.shields.io/github/actions/workflow/status/fawdlstty/channel/rust.yml)

[English](README.md) | 简体中文

**一个 Rust 库，覆盖与 AI 模型对话的所有路径：既能驱动 AI Coding Harness，也能直连大模型协议，还能在进程内运行本地模型——全部共用一套统一风格的异步 API。**

## 驱动 AI Coding Harness（默认）

用一套归一化的异步会话 API 驱动主流 AI Coding Harness——**Codex、Claude Code、OpenCode、Zed、ZCode、DeepSeek、Hermes**。协议差异（Codex App Server、Agent Client Protocol、托管 CLI）全部收敛为同一条事件流：回答与推理增量、工具/文件/命令活动、授权请求和回合终态。

```rust
use channel::{HarnessKind, Session, SessionConfig, SessionEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = SessionConfig::for_kind(HarnessKind::Codex);
    config.set_observability(false); // 对 harness 自身界面不可见

    // 创建会话的同时就提交了第一条消息。
    let mut session = Session::create(config, "介绍一下这个仓库").await?;

    while let Some(event) = session.wait_event().await? {
        match event {
            SessionEvent::Message(ev) if ev.reasoning => print!("[推理]{}", ev.text),
            SessionEvent::Message(ev) => print!("{}", ev.text),
            SessionEvent::Activity(ev) => println!("{} ({:?})", ev.display, ev.kind),
            SessionEvent::Permission(ev) => println!("需要授权: {}", ev.detail),
            SessionEvent::Finished(finish) => println!("\n-- {} --", finish.text),
        }
    }
    Ok(())
}
```

## 直连大模型协议（`llm` feature）

基于 potato HTTP 栈实现的四种聊天协议客户端：**OpenAI Chat Completions**（`ChatCompletionsClient`）、**OpenAI Responses**（`ResponsesClient`）、**Anthropic Messages**（`MessagesClient`）与 **Ollama**（`OllamaClient`），任何 OpenAI 兼容端点都可作为 base URL。四种客户端共享同一套接口：系统提示词、模型选择（自动对照端点模型列表校验）、推理强度、历史记录读写、状态序列化与恢复；`chat()` 一次性调用，`chat_stream()` 以 `mpsc::Receiver<StreamChunk>` 流式返回（SSE/NDJSON）。

该 feature 还提供服务端流发射器（`OpenAISender`、`AnthropicSender`、`OllamaSender`），把自有数据以协议一致的 SSE/NDJSON 帧格式发出，让自定义后端也能对外提供 OpenAI/Anthropic/Ollama 风格的流式接口。

```rust
let mut client = channel::ChatCompletionsClient::new(base_url, api_key);
client.set_model("gpt-4o-mini").await?;
let mut receiver = client.chat_stream("用一句话解释什么是 SSE 流。").await?;
while let Some(chunk) = receiver.recv().await {
    // StreamChunk::Content(text) | Error(message) | Done
}
```

## 进程内运行本地模型（`local` / `local-gguf` feature）

`LocalClient::load` 直接从磁盘加载模型、进程内推理，中间不需要任何服务端：**safetensors** 模型目录走纯 Rust 的 candle 后端；**GGUF** 文件走 llama.cpp 后端（opt-in `local-gguf`，需 cmake 与 C++ 工具链）。加载参数涵盖上下文长度、线程数、GPU 层数卸载与聊天模板覆盖；生成参数涵盖 temperature、top-p/top-k、重复惩罚、停止序列等。会话接口与 HTTP 客户端完全一致。

```rust
let mut client = channel::LocalClient::load("./Qwen3-0.6B").await?;
client.set_system_prompt("You are a concise assistant.");
let reply = client.chat("用一句话解释什么是 SSE 流。").await?;
```

## 把本地模型挂成 HTTP 服务（`local-server` feature）

`LocalLlmServer` 挂载一个或多个本地模型，以标准协议端点对外提供服务——`GET /v1/models`、`POST /v1/chat/completions`、`POST /v1/responses`、`POST /v1/messages`、`POST /api/chat`、`GET /api/tags`——**任何 OpenAI、Anthropic 或 Ollama 兼容客户端**（包括 channel 自带的四种协议客户端）都能直接对话本地模型。可选 Bearer Token 鉴权。

```rust
let mut server = channel::LocalLlmServer::bind("127.0.0.1:8817")?;
server.mount_model("./Qwen3-0.6B")?;
server.serve().await?;
```

## Feature 一览

| Feature | 启用内容 |
|---|---|
| （默认） | 仅 Harness 会话 |
| `llm` | 四种大模型直连协议客户端 + 服务端流发射器（引入 potato HTTP 栈） |
| `local` | 进程内本地推理，candle 后端（safetensors，纯 Rust） |
| `local-gguf` | llama.cpp 后端，支持 GGUF 模型（隐含 `local`） |
| `local-server` | 本地模型的 HTTP 协议端点（隐含 `local`） |
| `local-cuda` / `local-metal` / `local-vulkan` | llama.cpp 后端 GPU 加速 |
| `local-candle-cuda` / `local-candle-metal` | candle 后端 GPU 加速 |

## 示例

| 示例 | 运行方式 | 覆盖能力 |
|---|---|---|
| [basic.rs](examples/basic.rs) | `cargo run --example basic` | Harness 会话 |
| [llm_chat.rs](examples/llm_chat.rs) | `cargo run --features llm --example llm_chat` | 直连大模型 |
| [local_chat.rs](examples/local_chat.rs) | `cargo run --features local --example local_chat -- ./Qwen3-0.6B` | 本地模型 |
| [local_server.rs](examples/local_server.rs) | `cargo run --features local-server,local-gguf --example local_server -- ./Qwen3-0.6B` | 本地模型服务 |

详细的对外接口手册（中文）见 [manual.md](manual.md)。

- **许可证**：MIT
