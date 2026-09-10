# channel 对外接口手册

channel 把不同 AI Coding Harness 统一成同一个会话模型。调用方按五步使用它：选择并验证 harness，创建会话并提交第一条消息，消费回合事件，观察会话结果，必要时中断或查询详情。

## 1. 选择并准备 Harness

先用 `HarnessKind` 表达业务上要接入哪类工具：

- `Codex` 走 Codex App Server。
- `OpenCode`、`ZedAcp`、`ZCode`、`DeepSeek`、`Hermes` 走 Agent Client Protocol。
- `ClaudeCode` 走托管 CLI。

`Harness::initialize(kind)` 用于在真正开始对话前确认运行环境，默认使用当前工作目录。发现失败时，用 `Harness::initialize_with(config)` 传入工作目录、可执行文件、endpoint 或 port：

- `workspace` 决定业务工作目录，默认使用当前目录；
- `executable` 覆盖默认可执行文件；
- `endpoint` 或 `port` 当前只有 Codex 后端消费，且二者不能同时指定；Codex endpoint 只允许本机 `ws://` 地址；
- 普通 CLI 只接受托管 stdio，传入 endpoint 或 port 会失败。

成功后得到不透明的 `Harness`，解析出的运行时类型不对用户暴露。`initialized.capabilities()` 用于判断该后端能提供流式事件、工具调用、文件读取、取消等能力。Codex 的 `initialized.available_models()` 返回协议层当前报告的模型 ID。

## 2. 创建会话

会话通过 `Session::create(config, first_message)` 创建；它使用配置初始化 harness，并立刻把第一条用户消息提交给后端；返回的 `Session` 通常已经处于 `Running`。

需要控制工作目录、模型、网络、审批、权限、运行时或后端协议时，改用分层配置：

1. `SessionConfig::for_kind(kind)` 直接返回可变的 `SessionConfig`。
2. 按业务约束修改安全、运行时、会话、元数据、provider 选项或后端字段；工作目录、观测性、模型和推理强度可用 `set_workspace`、`set_observability`、`set_model`、`set_reasoning_effort` 设置。`set_workspace(None)` 表示初始化时使用当前目录；`set_observability(false)` 请求目标 harness 不可见，`set_observability(true)` 使用 provider 默认可见性。
3. 通常保持 `Auto` 后端；确有需要时显式修改 `backend` 为 `CodexAppServer`、`Acp`、`StructuredCli` 或 `PlainCli`。
4. `Session::create(config, first_message)` 负责初始化并创建会话；`Harness::initialize_with(config)` 可单独用于提前检查 `Harness` 能力和模型列表。

配置对象可直接回读：`get_workspace`、`get_observability`、`get_model` 和 `get_reasoning_effort`；推理强度使用 `ReasoningEffort`（`Minimal`、`Low`、`Medium`、`High`、`XHigh`、`Max`）。其他只读访问器包括 `kind()`、`security()`、`runtime()`、`conversation()`、`metadata()` 和 `provider_options()`。常用默认值有 `WorkspaceOptions::directory(path)` 和 `PermissionSpec::workspace_read_write()`。

创建即提交首条消息，所以创建成功后不需要再调用一次发送。

## 3. 推进一个回合

一个 `Session` 同一时间只允许一个回合。回合结束后才能继续 `send`；否则返回 `Busy`。

两种发送语义：

- `send(message, SendMode::Wait)` 等到本回合结束，直接返回 `SendResult::Finished`。适合批处理，但期间拿不到中间事件。
- `send(message, SendMode::Immediate)` 只确认消息已提交，返回 `SendResult::SendAccepted` 和回合 ID。界面或日志场景应随后循环调用 `wait_event()`，直到收到 `Some(SessionEvent::Finished)` 或返回 `Ok(None)`。

`wait_event()` 返回 `Result<Option<SessionEvent>, Error>`。`Ok(Some(event))` 是归一化后的业务事件：

- `Message`：增量回答或推理摘要；
- `Activity`：工具、读文件、写文件、命令或子代理活动；
- `Permission`：后端要求授权；用 `respond_permission(id, response)` 回复 `PermissionResponse::Approve` 或 `PermissionResponse::Deny`，未回复的请求会在会话关闭或下一回合开始时自动拒绝；
- `Finished`：回合终态，包含状态和聚合后的最终文本；

状态变化只反映在 `state()` 和会话详情中，不作为事件推送。回合结束后再次等待会得到 `Ok(None)`；中断后没有后续事件时也返回 `Ok(None)`。后端或协议异常通过 `Err(Error)` 返回。

不要再从原始 JSON、ACP 帧或 Codex notification 里解析业务状态；这些协议差异已在会话层归一化。

## 4. 观察会话和结果

回合中或回合后，可从 `Session` 直接读取当前视图：

- `state()` 返回会话状态，例如 `Running`、`WaitingForPermission`、`Completed`、`Failed`、`Closed`。
- `activity()` 汇总当前回合的文件读取、文件修改、命令和工具活动，并给出运行中、完成、失败、取消计数。
- `result()` 在收到终态后返回 `Finish`。
- `info()` 返回会话详情，包括工作目录、后端、运行时、安全配置、外部 ID、模型和历史回合。
- `capabilities()` 返回能力报告；初始化时是声明/生效能力，回合推进后还会补上实际观察能力。
- `refresh_info()` 读取本地缓存的后端会话 ID 并刷新更新时间，不会向后端发起请求。

## 5. 中断或结束

运行中的回合用 `interrupt(action)` 处理：

- `InterruptAction::Continue` 请求停止当前工作，但等待后端收尾并保留会话，返回 `InterruptResult::Continued`。
- `InterruptAction::End` 在收尾后关闭后端连接，会话进入 `Closed`，返回 `InterruptResult::Ended`。

公开 API 没有单独的 `close` 方法；`End` 中断是唯一可靠的显式结束入口。不要依赖 `Session` 被 drop 来终止托管进程。

## 6. 恢复历史会话

`initialized.resume(target)` 用于 provider 侧历史恢复。它会先检查 `initialized.capabilities().effective.provider_resume`；Codex App Server（`thread/resume`）和 ClaudeCode（`--resume`）声明了该能力，ACP 与托管脚本 CLI 不支持，返回 `UnsupportedCapability`。

恢复目标用 `ResumeTarget` 表达：`ChannelDefault`、`ProviderSession` 和 `ProviderThread` 都会映射到 provider 侧会话或线程 ID；`NativeUiHandle` 当前不可恢复。恢复得到的 `Session` 处于空闲状态，随后用普通 `send` 开始新回合。会话详情中 `visibility.resumability` 反映实际可恢复性；`set_observability(false)` 创建的临时会话始终为 `InMemoryOnly`。

会话恢复后，Codex 会继续既有 thread，ClaudeCode 会在下一回合通过 `--resume` 续接同一会话 ID；新回合的外部 ID 会在事件回流后出现在 `info().external_ids` 中。

## 7. 托管 CLI 约定

CLI 每回合启动一次进程，从 stdout 读取按行分隔的输出。普通文本会归一化为回答增量；JSON 行会识别会话 ID、文本、工具调用、工具结果、文件读取、状态和最终结果。最简单的完成事件形如 `{"type":"result","result":"最终回答"}`；标记 `is_error` 或 `status: "failed"` 会形成失败终态。

## 8. 错误处理

初始化失败通常是可执行文件、工作目录、endpoint 或协议握手问题。会话期错误集中在 `Error`：`InvalidConfig` 表示配置不可执行，`Busy` 表示上一回合未结束，`NoActiveTurn` 表示尚未开始回合，`ProtocolError`、`ProcessExited`、`Backend` 表示后端通信或进程异常。调用方应把回合失败和会话失败区分开：单个回合失败后可重建或换配置，回合结果用 `result()` 查询；`Closed` 后不能再发送，`wait_event()` 返回 `Ok(None)`。

## 9. 大模型调用（可选 feature）

除接入 coding harness 外，channel 还提供直接调用大模型 HTTP 协议的客户端。这部分由 cargo feature `llm` 按需启用，启用后会引入 `potato` 作为 HTTP 传输层：

- `llm`：启用全部四个直连大模型协议客户端——`ChatCompletionsClient` 与 `OpenAISender`（OpenAI Chat Completions，`POST {base_url}/chat/completions`）、`ResponsesClient`（OpenAI Responses，`POST {base_url}/responses`）、`MessagesClient` 与 `AnthropicSender`（Anthropic Messages，`POST {base_url}/messages`）、`OllamaClient` 与 `OllamaSender`（Ollama chat，`POST {base_url}/api/chat`，模型列表 `GET {base_url}/api/tags`）。

四种客户端共享同一套会话模型：`new(base_url, api_key)` 创建（内部复用一条 potato 连接），`set_system_prompt` 注入系统提示词，`set_model` 在端点列出模型并校验（anthropic 无模型列表接口，直接记录），`set_reasoning_effort` 设置推理强度（复用 `ReasoningEffort`），`messages`/`set_messages` 读写历史（覆盖时保留已有 System 条目）。发送消息有两种方式：

- `chat(message)` 非流式：请求一次、解析完整回复并追加到历史。
- `chat_stream(message)` 流式：发起流式请求并返回 `mpsc::Receiver<StreamChunk>`；后台任务按协议解析增量，实时更新历史末尾的 assistant 消息。`StreamChunk` 为 `Content`（增量文本）、`Error`（协议报错，随后仍会收到 `Done`）与 `Done`（流结束）。

序列化：`serialize()`/`deserialize()` 把协议标识、`base_url`、`api_key`、模型、历史与推理强度保存为 JSON 并恢复；反序列化会校验协议标识是否匹配当前客户端类型。

Ollama 与其余三种协议的差异：请求头只有 `Content-Type: application/json`（Ollama 不做鉴权，`api_key` 参数仅为接口统一保留、不发送）；system 提示词不平摊到顶层字段，而是作为普通 `system` 消息留在 `messages` 数组内；`set_reasoning_effort` 只保存在客户端状态里、不写入请求。流式响应是 NDJSON（每行一个 JSON、以 `\n` 分隔，而非 SSE 的空行分隔事件块）：每行取 `message.content` 作为增量，`done == true` 的行结束流，含 `error` 字段的行报 `StreamChunk::Error`；后台任务按行切分，一行被网络分块截断时会先拼接再解析。

服务端方向：`OpenAISender` 与 `AnthropicSender` 用与协议一致的 SSE 帧格式发射服务端流（`new` 返回发送器与 `potato::HttpResponse`，`send` 发增量，`send_finish` 发收尾帧），适合在 potato 服务器上把自有数据伪装成 OpenAI/Anthropic 流式接口。`OllamaSender` 与之同理但发射 NDJSON：响应的 `Content-Type` 为 `application/x-ndjson`，`send` 发 `done: false` 帧（`created_at` 为 RFC 3339 UTC 时间戳），`send_finish` 发 `done: true` 且 `done_reason: "stop"` 的收尾帧，每帧以换行结束。

错误映射沿用统一的 `Error`：非 200 响应为 `ProviderRejected("HTTP {code}: {body}")`，JSON 或线格式解析失败为 `ProtocolError`，连接与传输失败为 `Backend`，未设置模型为 `InvalidConfig`，anthropic 模型列表为 `UnsupportedCapability`。

启用方式示例：

```toml
[dependencies]
channel = { version = "0.3", features = ["llm"] }
```

未启用 `llm` feature 时，channel 完全不依赖 potato，`llm` 模块不参与编译。
