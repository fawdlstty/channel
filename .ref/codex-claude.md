# Codex CLI 与 Claude Code 会话接口调研

- 调研日期：2026-09-01（Asia/Shanghai）
- 目标：核实如何创建会话、发送消息、接收流式/最终输出、识别 thinking/结束/中断，以及 CLI/协议限制。
- 证据优先级：官方文档 > 官方 GitHub 源码/协议 schema > 本机命令实际输出。
- 本文只讨论 Codex CLI 与 Claude Code；不把未公开的内部 UI 协议当作稳定 API。

## 1. 结论摘要

### Codex

Codex 有两种不同编程面：

1. `codex exec`：一次性、非交互命令。输入是命令行 prompt 或 stdin，输出可以是普通文本或 `--json` JSONL。它适合批处理；连续对话通常通过新的进程执行 `codex exec resume <session-id> <prompt>`，而不是在同一个 stdin 进程内反复发送消息。
2. `codex app-server`：面向 IDE/富客户端的长连接、双向 JSON-RPC 风格协议。默认 transport 是 stdio JSONL，可创建/恢复 thread，向 thread 启动 turn，实时接收 item/delta，调用 `turn/interrupt` 中断。这是实现真正 `send -> stream -> wait -> send again` 的首选入口，但官方 README 明确仍是 experimental/部分 transport unsupported，必须按实际运行版本生成并使用 schema。

Codex 的核心层级是：`Thread`（会话） -> `Turn`（一次用户请求及其代理执行过程） -> `Item`（用户消息、推理摘要、模型消息、命令、文件修改等）。不要把一条文本输出或一个 token 流误当作会话状态；结束边界应以 `turn/completed` 为准。

### Claude Code

Claude Code 有三种相关方式：

1. 交互 CLI：`claude`，适合人工终端操作，不是稳定的机器协议。
2. Headless CLI：`claude -p`，一次调用后退出；`--output-format json` 得到最终 JSON，`--output-format stream-json --verbose --include-partial-messages` 得到 JSONL 事件流。可用 `--continue` 或 `--resume <session-id>` 继续历史。
3. 官方 Claude Agent SDK：Python/TypeScript。`ClaudeSDKClient` 用子进程 transport 保持双向、stateful、interactive 会话，支持发送 follow-up、接收消息、实时流、`interrupt()` 和 `disconnect()`；`query()` 是适合一次性查询的 async iterator。

因此，Claude 的长会话实现应优先使用 Agent SDK；如果项目只允许启动 CLI 子进程，则把 `claude -p` 封装成“每个 turn 一个进程”，以 `session_id` 恢复上下文，不应假设普通 `-p` stdin 能像 socket 一样无限接收多轮消息。

## 2. 证据与版本

### 2.1 Codex 本机验证

本机命令：

```text
/home/fawdlstty/.nvm/versions/node/v22.22.3/bin/codex
codex-cli 0.151.0
```

本机 `codex app-server --help` 验证到：

- `codex app-server` 支持 `--listen stdio://`（默认）、`unix://`、`ws://IP:PORT`、`off`。
- `generate-ts` 与 `generate-json-schema` 可生成与当前二进制严格匹配的协议定义。
- `codex exec --help` 支持 `--json`、`--output-last-message`、`--ephemeral`；`codex exec resume` 接受 session ID/name 与后续 prompt。

本机运行：

```bash
codex app-server generate-json-schema --experimental --out /tmp/codex-schema
```

成功生成 `ClientRequest.json`、`ServerNotification.json`、`ServerRequest.json` 以及 v1/v2 schema。当前 schema 验证到以下关键方法和枚举：

- 请求：`initialize`、`thread/start`、`thread/resume`、`thread/fork`、`turn/start`、`turn/interrupt`。
- 通知：`thread/started`、`thread/status/changed`、`turn/started`、`turn/completed`、`item/started`、`item/completed`、`item/agentMessage/delta`、`item/reasoning/summaryTextDelta`、`item/reasoning/textDelta`。
- Turn 状态：`inProgress`、`completed`、`interrupted`、`failed`。
- Thread 状态：`notLoaded`、`idle`、`active`、`systemError`；active 可带 `waitingOnApproval`、`waitingOnUserInput`。

### 2.2 Claude Code 官方资料与源码版本

官方文档页面在 2026-09-01 可访问，页面标出的部分页面修改时间为 2026-08-29。官方 Python Agent SDK 仓库当前 main commit（调研时读取）为：

```text
9597fc956a2a18ff8a6dc5675d9823b457e8264f
```

官方 Codex 仓库当前 main commit（调研时读取）为：

```text
2e5ee418ad6bef8b418ba1a809cfa53a56ae4aee
```

本机没有 `claude` 可执行文件，因此没有伪造本机 Claude 版本；Claude 的行为依据官方 CLI 文档和官方 Agent SDK Python 源码核实。

## 3. Codex CLI：会话与消息流程

### 3.1 `codex exec` 一次性模式

创建并执行一个新会话：

```bash
codex exec --json "你好"
```

prompt 也可以来自 stdin：

```bash
printf '%s' '你好' | codex exec --json -
```

关键点：

- `exec` 是非交互模式，命令完成后进程退出。
- `--json` 是 stdout JSONL 事件流，不能按“整个 stdout 是一个 JSON 文档”解析。
- `--output-last-message FILE` 额外落盘最后一条 agent message，适合只需要最终文本的脚本。
- `--ephemeral` 禁止持久化 session 文件，适合不需要恢复的临时运行。
- 认证、模型、工作目录、sandbox、approval 等由 CLI 配置和 flags 决定；危险权限 flags 会改变安全边界，封装库不应默认开启。

恢复并发送下一条消息：

```bash
codex exec resume <session-id> --json "继续刚才的任务"
```

`codex exec resume --help` 明确支持：

- 传 UUID 或 thread name；UUID 优先。
- 不传 id 时用 `--last` 选择最新记录的会话。
- resume 进程仍然是一次性执行；它处理一个后续 prompt 后退出。

因此，`exec` 的抽象更接近：

```text
spawn(prompt) -> consume JSONL until process exit -> persist session_id -> spawn(resume session_id, next prompt)
```

而不是：

```text
spawn once -> write prompt 1 -> write prompt 2 -> read prompt 1/2 independently
```

### 3.2 `codex app-server` transport

官方 app-server README 描述的协议如下：

- 类似 MCP 的双向 JSON-RPC 2.0 消息；wire 上省略 `jsonrpc: "2.0"` header。
- stdio：换行分隔 JSON（JSONL），默认且最适合本地子进程封装。
- WebSocket：每个 text frame 一条 JSON-RPC 消息，但文档标注 experimental/unsupported，不应直接作为生产默认。
- Unix socket：本地控制面连接，底层为 HTTP Upgrade 后的 websocket frames，使用 `$CODEX_HOME/app-server-control/app-server-control.sock` 或自定义路径。
- app-server 有有界队列；入口过载时返回 JSON-RPC error code `-32001`、`Server overloaded; retry later.`，客户端应指数退避并加 jitter。

### 3.3 app-server 创建/恢复会话

连接建立后必须先完成握手：

```json
{"id":1,"method":"initialize","params":{"clientInfo":{"name":"channel","title":"channel","version":"0.1.0"}}}
{"method":"initialized"}
```

限制：

- 每个 transport connection 只允许一次 `initialize`。
- `initialize` 完成前的其他请求会被拒绝。
- 重复 initialize 会得到 `Already initialized`。
- `initialize.params.capabilities.optOutNotificationMethods` 可按完整方法名精确关闭通知，不支持通配符。

创建新 thread：

```json
{"id":2,"method":"thread/start","params":{"cwd":"/abs/project"}}
```

响应/通知包含 thread 对象，至少应保存：

- `thread.id`：后续 turn 的目标 ID。
- `thread.sessionId`：同一 session tree 的 session ID。
- `thread.status`：当前 runtime 状态。
- `thread.ephemeral`：是否只存在内存中。
- `thread.path`：持久化路径字段目前标为 unstable，不应作为跨版本主键。

恢复：

```json
{"id":3,"method":"thread/resume","params":{"threadId":"<thread-id>"}}
```

官方 schema 的 resume 描述支持按 `thread_id`、history 或 path 恢复，但 history/path 均标为 unstable/特定用途；普通客户端应优先只使用 `threadId`。需要分支时使用 `thread/fork`，而不是手工复制历史。

### 3.4 app-server 发送消息/启动 turn

最小文本输入：

```json
{"id":4,"method":"turn/start","params":{"threadId":"<thread-id>","input":[{"type":"text","text":"你好"}]}}
```

`turn/start` 响应返回 turn；真正开始运行时还会收到 `turn/started`。`TurnStartParams` 的必填字段只有：

- `threadId`
- `input`（数组，支持 text，也可根据 schema 使用图片、音频、skill、mention 等输入类型）

可选项包括 model、effort、summary、cwd、sandboxPolicy、permissions、approval 相关字段、outputSchema、runtimeWorkspaceRoots 等。封装层应只暴露项目确实需要的少数选项，避免把实验字段固化成稳定 API。

### 3.5 app-server 接收流式与最终消息

`turn/start` 后持续读取 stdout，每行解析为一条 JSON-RPC response/notification，并按 `method` 分发：

| 事件 | 用途 |
|---|---|
| `thread/status/changed` | 更新会话 runtime 状态；`active`、`idle`、`systemError` 等 |
| `turn/started` | turn 已真正开始运行 |
| `item/started` / `item/completed` | 某个 item 的生命周期边界 |
| `item/agentMessage/delta` | agent 面向用户的增量文本；按 `threadId`、`turnId`、`itemId` 聚合 |
| `item/reasoning/summaryTextDelta` | reasoning summary 的增量；这是摘要/可展示思考，不等于可获得的完整隐藏 chain-of-thought |
| `item/reasoning/textDelta` | schema 暴露的 reasoning text 增量；是否产生、内容和可用性受模型/配置/版本影响，不应保证一定存在 |
| `item/commandExecution/outputDelta` 等 | 工具、命令、文件修改等副作用的增量 |
| `turn/completed` | turn 的最终状态、完成/中断/失败信息和 token usage |

正确的最终文本收集方式：

1. 过滤目标 `threadId`/`turnId`。
2. 对该 turn 的 agent-message item 按 `itemId` 追加 `delta`。
3. 收到 `turn/completed` 后结束等待。
4. 以 `turn.status` 判断 `completed`、`interrupted` 或 `failed`，不要用“若干秒没有 delta”判断结束。

app-server 也发送 `item/completed`，其中 item 可能含最终 agent message；但跨版本的 turn 级结束边界仍应以 `turn/completed` 为准。

### 3.6 app-server 状态、等待和中断

建议内部状态映射：

```text
Starting  = 已建立进程但尚未收到 thread/started
Idle      = thread/status/changed -> idle，且没有 active turn
Thinking  = 收到 turn/started，或 thread status -> active 且当前 turn 未结束
Waiting   = thread status active 且 activeFlags 含 waitingOnApproval / waitingOnUserInput
Finished  = turn/completed.status == completed
Interrupted = turn/completed.status == interrupted
Failed    = turn/completed.status == failed，或 thread status -> systemError
```

“thinking”不是一个单一可靠事件名。实现应把 `turn/started` 到 `turn/completed` 之间的活动状态作为 `Thinking`，并用 reasoning/item/tool 事件丰富 UI；不能承诺一定获得模型原始内部思维。

中断：

```json
{"id":5,"method":"turn/interrupt","params":{"threadId":"<thread-id>","turnId":"<turn-id>"}}
```

`TurnInterruptParams` 要求同时提供 `threadId` 和 `turnId`。调用后仍需继续读取事件，直到收到该 turn 的 `turn/completed`，然后检查最终 status 是否 `interrupted`。不要在发出 interrupt 后立即杀掉 app-server 进程，否则可能丢失最终状态和已排队输出。

### 3.7 app-server 服务端请求与审批

app-server 不只是单向输出；服务端可能反向向客户端发送 request，例如：

- `item/commandExecution/requestApproval`
- `item/fileChange/requestApproval`
- `item/tool/requestUserInput`
- `mcpServer/elicitation/request`
- `item/permissions/requestApproval`
- `item/tool/call`

客户端若不处理这些请求，turn 可能停在 `waitingOnApproval` 或 `waitingOnUserInput`。因此生产 adapter 至少必须具备：

1. 通用 request/response correlation（按 JSON-RPC `id` 回复）。
2. 明确的 approval policy：自动拒绝、自动允许或转交上层回调。
3. 反向请求超时和取消策略。
4. 将等待审批与模型 thinking 区分开。

## 4. Claude Code：CLI 与 Agent SDK 流程

### 4.1 `claude -p` 创建一次性会话

最小调用：

```bash
claude -p "你好"
```

stdin 也可输入内容：

```bash
cat build-error.txt | claude -p '解释这个构建错误'
```

官方文档说明：

- `-p`/`--print` 是 non-interactive mode。
- 默认输出是 text。
- `--output-format json` 输出包含最终 `result`、`session_id` 和 metadata。
- `--output-format stream-json` 输出实时 JSONL。
- `-p` 成功退出码为 0；运行失败为非 0。某些运行时失败会把失败结果写到 stdout，因此不能只靠 stderr 判断模型失败。
- stdin pipe 上限为 10MB；大输入应写文件并在 prompt 中引用路径。
- `--bare` 可跳过 hooks、skills、custom commands、subagents、plugins、MCP、auto memory、CLAUDE.md 等自动发现，官方推荐脚本/SDK 场景使用，以减少环境差异。

### 4.2 Claude CLI 的最终 JSON

```bash
claude -p "总结项目" --output-format json
```

程序应读取 JSON 的：

- `result`：最终文本。
- `session_id`：后续恢复句柄。
- usage/cost 等 metadata（成本是客户端估算，不一定等于最终账单）。
- 错误相关字段/状态；应同时检查 `is_error`（若版本/输出带有该字段）与进程退出码。

使用 JSON Schema 约束最终结构：

```bash
claude -p "提取函数名" --output-format json \
  --json-schema '{"type":"object","properties":{"functions":{"type":"array","items":{"type":"string"}}},"required":["functions"]}'
```

### 4.3 Claude CLI 流式输出

官方推荐：

```bash
claude -p "解释递归" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages
```

规则：

- 每行是一个事件 JSON 对象。
- `stream_event` 中的 Anthropic raw event 可携带 partial message/token delta。
- 过滤 `type == "stream_event"` 且 `event.delta.type == "text_delta"` 可以得到实时用户可见文本。
- 最后一行是 `result` message，包含最终 response text、cost、session metadata；它是一次 `-p` 运行的终止边界。
- 慢速消费者会让 Claude Code 等待输出队列 drain，当前文档说明上限为 30 秒；消费端应及时读 stdout，避免管道阻塞。
- subagent 默认主要输出 `tool_use`/`tool_result`；需要重建 subagent 文本和 thinking blocks 时，使用 `--forward-subagent-text`（要求相应版本、print、stream-json）。

### 4.4 Claude CLI 会话恢复与多轮

首次调用拿到 session ID：

```bash
session_id=$(claude -p "开始代码审查" --output-format json | jq -r '.session_id')
claude -p "继续审查数据库查询" --resume "$session_id"
```

或者继续当前目录最近会话：

```bash
claude -p "继续任务" --continue
```

`--resume` 可以传 session ID 或名称；`--fork-session` 可在恢复时生成新 session ID，不复用原会话。官方文档说明恢复搜索范围和名称解析随版本变化，因此 adapter 应保存真实 `session_id`，不要把显示名当主键。

默认 transcript 是 JSONL，位置形如：

```text
~/.claude/projects/<project>/<session-id>.jsonl
```

项目名由工作目录转换而来；可用 `CLAUDE_CONFIG_DIR`、`CLAUDE_CODE_PROJECT_DIR_NAME` 改变存储隔离。`--no-session-persistence` 可关闭单次非交互运行的 transcript 持久化。

### 4.5 Claude Agent SDK：长连接双向会话

官方 Python SDK 的最小概念：

```python
from claude_agent_sdk import ClaudeSDKClient

async with ClaudeSDKClient() as client:
    await client.query("你好")
    async for message in client.receive_response():
        print(message)

    await client.query("继续")
    async for message in client.receive_response():
        print(message)
```

SDK 源码明确把 `ClaudeSDKClient` 定义为：

- bidirectional：可随时发送/接收。
- stateful：跨消息维持上下文。
- interactive：可根据前一轮响应发送 follow-up。
- 支持 streaming、interrupt、session management。
- 底层默认通过 Claude Code CLI 子进程 transport 通信。

SDK `query()` 则是一次性 async iterator：

```python
from claude_agent_sdk import query

async for message in query(prompt="你好"):
    print(message)
```

它适合 one-shot/batch，不适合需要主动 follow-up 或中断的交互会话。

### 4.6 Claude SDK 消息类型与结束边界

官方 Python 类型包括：

- `UserMessage`：用户消息。
- `AssistantMessage`：assistant 内容 blocks、model、stop_reason、session_id 等。
- `SystemMessage`：系统元数据，含 `subtype` 和原始 `data`。
- `StreamEvent`：实时 partial message，含 `session_id` 和 raw Anthropic event。
- `ResultMessage`：一次 query/turn 的终止结果。

`receive_response()` 的实现约定是：

- 按到达顺序 yield 每条消息。
- 包含最后的 `ResultMessage`。
- yield `ResultMessage` 后立即结束迭代。
- 如果没有 `ResultMessage`，迭代器会继续等待。

`ResultMessage` 当前类型携带：

- `subtype`
- `duration_ms`、`duration_api_ms`
- `is_error`
- `num_turns`
- `session_id`
- `stop_reason`
- `result`
- usage/cost/model_usage
- `terminal_reason`，例如 `completed`、`max_turns`、`aborted_streaming`；`aborted_streaming` 或 `aborted_tools` 表示由 interrupt 或 interrupt control request 取消。

因此 SDK adapter 应以 `ResultMessage` 作为 turn 完成边界，以 `is_error`/`terminal_reason` 判断成功、失败和中断，不能以最后一条 `AssistantMessage` 作为唯一结束信号。

### 4.7 Claude thinking 状态

Claude stream 中可能出现：

- 普通 assistant text blocks/deltas。
- Anthropic partial events。
- tool_use/tool_result。
- system init、retry 等系统事件。
- thinking blocks；是否能看到 subagent thinking 还受 `--forward-subagent-text`、版本和配置影响。

推荐状态机：

```text
Starting  = CLI/SDK transport 已连接，尚未收到 system init 或首条消息
Thinking  = 已提交 prompt，尚未收到 ResultMessage；可因 text/tool/thinking 事件持续更新
Waiting   = permission/tool/user-input 回调挂起
Finished  = 收到 ResultMessage 且 terminal_reason 为 completed/max_turns 等正常终止
Interrupted = ResultMessage.terminal_reason 为 aborted_streaming/aborted_tools，或收到明确中断结果
Failed    = ResultMessage.is_error == true，或 transport/进程错误
```

“thinking”在统一层只能表示“该 turn 尚未结束且 agent 正在处理”，不要把它定义成“客户端必然能读取完整模型思维”。对于安全、隐私和模型能力原因，完整 hidden chain-of-thought 不应成为跨 harness API 的承诺。

### 4.8 Claude SDK 中断与关闭

SDK 提供：

```python
await client.interrupt()
await client.disconnect()
```

推荐顺序：

1. `interrupt()` 结束当前 turn。
2. 继续消费消息直到收到终止 `ResultMessage`（若 SDK/版本提供）。
3. 不再使用时调用 `disconnect()`。

官方 headless 文档还区分信号：

- 对 `claude -p` 发 SIGTERM，进程退出码为 143；正在进行的 turn 会留下 unfinished 状态且不记录 result。
- 要结束 turn，应发送 SIGINT，或使用 Agent SDK `interrupt()` 后再关闭进程。
- SIGTERM 会终止仍在运行的 Bash 命令进程树，并运行 SessionEnd hooks。
- 恢复该 session 时，Claude Code 可能继续 SIGTERM 留下的未完成 turn。

因此 CLI adapter 的“硬 kill”只能作为 transport 兜底，不能冒充业务层 `Interrupted`；若只收到 SIGTERM，应报告为 `AbortedTransport`/`Unknown`，除非恢复或 transcript 明确给出终止原因。

## 5. 两套接口的通用流程

跨 Codex 与 Claude 抽象后，稳定的共同流程是：

```text
1. Resolve harness executable/SDK and runtime configuration.
2. Create or connect to a session.
3. Capture durable session handle immediately.
4. Submit one user turn.
5. Read events until the harness-specific terminal event.
6. Normalize visible text deltas, final text, usage and status.
7. On interrupt, request graceful cancellation and keep draining events.
8. Reuse the same session handle for the next turn.
9. On close, release transport; keep durable session identity separate from process identity.
```

共同要点：

- **Session handle 与 process handle 分离**：进程可以重启，会话应可恢复。
- **Turn 是最小等待单元**：一个 session 可有多个 turn；`wait_finish()` 等待的是当前 turn，不是整个 session。
- **最终消息与增量消息分离**：增量用于 UI；最终消息用于稳定存储/返回值。
- **状态由显式终止事件驱动**：Codex `turn/completed`；Claude SDK `ResultMessage`；Claude CLI stream 的最后 `result`。
- **thinking 是派生状态**：`turn started && !terminal`，不是跨 harness 的必有事件。
- **中断后继续 drain**：否则可能丢掉 cancellation receipt、最终状态或尾部 token。
- **错误分层**：启动错误、协议错误、权限等待、模型错误、turn failed、用户中断、进程被杀必须分别建模。
- **反向请求不可忽略**：Codex approval/user-input 和 Claude SDK permission callback 都可能使 agent 等待。
- **版本能力探测优于硬编码版本号**：Codex 使用当前二进制生成 schema；Claude system/init capabilities（如 interrupt receipt capability）应按字段/能力检测。

## 6. CLI/协议限制对实现的影响

### Codex 限制

1. `app-server` 协议与生成 schema 随 CLI 版本变化；不要把另一版本 schema 复制为永久协议。
2. app-server 默认 stdio 是 JSONL，stdout 只能放协议数据；日志读 stderr。
3. WebSocket transport 当前标为 experimental/unsupported。
4. 服务器可能要求 approval 或 user input；不处理反向 request 会让 turn 长期 active。
5. `turn/start` 返回“已创建 turn”不等于模型已开始；必须等待 `turn/started`。
6. `thread/status/changed` 是 thread 级状态，不能替代 turn 级 `turn/completed`。
7. ephemeral thread 不落盘，不能假设重启后可以 resume。
8. `thread.path`、resume `history/path` 等字段在 schema 中标注 unstable，不宜做稳定依赖。
9. 过载错误 `-32001` 可重试，但重复提交 turn 可能造成重复任务；需要请求/turn 关联与幂等策略。

### Claude Code CLI 限制

1. `claude -p` 是一次性进程；多轮要 `--resume`/`--continue`，或使用 Agent SDK。
2. `--output-format stream-json` 需要配合 `--verbose`；要 token 级 partial 通常还需要 `--include-partial-messages`。
3. `stream-json` 是 JSONL 事件流，最后 `result` 才是一次调用的业务完成边界。
4. stdin pipe 最大 10MB。
5. `--bare` 会跳过项目/用户定制，能提高可重复性，但会改变可用 hooks、MCP、CLAUDE.md、skills 等环境。
6. 非交互模式可能没有人工权限提示；必须预先用 `--allowedTools`、permission mode 或 permission prompt tool 配置行为。
7. 慢速读取 stdout 可能反向阻塞 Claude，当前 drain 等待上限为 30 秒。
8. SIGTERM 与 graceful interrupt 语义不同，不能统一映射成成功结束或用户中断。
9. session transcript 默认落本地磁盘；需要隔离租户或隐私时配置 `CLAUDE_CONFIG_DIR` 或关闭 persistence。

### Claude Agent SDK 限制

1. `ClaudeSDKClient` 只能在同一个 async runtime context 中完整使用；官方 Python 源码特别提示连接期间维护 persistent anyio task group，不能随意跨不同 asyncio/trio task group 搬运实例。
2. SDK 默认仍依赖 Claude Code CLI 子进程，需处理 CLI 启动、stdout/stderr、退出和版本兼容。
3. `receive_response()` 没有 ResultMessage 时会一直等待；必须有独立取消/超时/关闭机制。
4. `query()` 是 one-shot 便利接口；要双向发消息、follow-up、interrupt 应使用 `ClaudeSDKClient`。
5. custom tools/hooks/permission callbacks 会引入反向调用和生命周期管理，不能只实现单向文本流。
6. SDK 类型与 CLI 版本应锁定兼容版本；不要依赖未知字段或把 `data` 的内部结构当稳定标准。

## 7. 推荐统一语义（供后续实现使用）

以下不是厂商协议，而是 adapter 层应提供的语义：

```text
Session
  id: stable harness session id
  harness: Codex | ClaudeCode
  capabilities: stream, interrupt, resume, approvals, tool_events, reasoning_summary

Turn
  id: optional harness turn id
  status: Starting | Thinking | Waiting | Completed | Interrupted | Failed | AbortedTransport
  text_delta: visible assistant text increments
  reasoning_delta: optional public reasoning summary increments
  tool_event: optional normalized tool/command progress
  final_text: optional final visible answer
  usage: optional tokens/cost/model metadata
  error: optional structured error
```

最小 adapter 操作应能表达：

```text
create_session(initial_prompt?) -> Session
send(session, prompt) -> TurnHandle
next_event(turn) -> Event
wait_finish(turn) -> TurnResult
interrupt(turn) -> acknowledgment
resume(session_id) -> Session
close(session) -> result
```

约束：

- `read_all()` 只能读取当前 turn 的最终可见文本，不应吞掉下一 turn 的事件。
- `wait_finish()` 必须消费并保存流式事件，不能只 sleep/poll。
- `send()` 在前一 turn 仍 active 时应明确返回 Busy，除非厂商能力支持 steer/queue。
- 对没有真实 session ID 的 ephemeral/one-shot adapter，应显式标记 `resumable=false`。
- 状态查询可以是缓存状态，但最终状态必须由终止事件确认。

## 8. 极简调用形态建议

面向使用者，目标可以保持接近：

```rust
let mut codex = channel::create_session(Harness::Codex, "你好").await?;
let output = codex.wait_finish().await?.text;
codex.send("你是什么模型？").await?;
let output = codex.wait_finish().await?.text;
```

若用户需要实时 UI，再提供可选事件流，而不是强迫最简单调用处理所有协议细节：

```rust
while let Some(event) = codex.next_event().await? {
    // text_delta / reasoning_delta / tool / status / finished
}
```

内部实现必须分别维护：

- transport reader task
- request/response correlation map
- current session/thread ID
- current turn ID
- per-item text buffers
- terminal result cache
- approval/input callback
- graceful interrupt state
- process exit/error state

这样才能同时适配 Codex app-server 与 Claude Agent SDK，而不把某一个 CLI 的 stdout 文本格式泄漏到公共接口。

## 9. 官方/公开来源

### Codex

- Codex 官方仓库 README：
  https://github.com/openai/codex/blob/main/README.md
- Codex app-server 官方协议 README：
  https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- 当前运行 CLI 的 schema 生成命令：
  `codex app-server generate-json-schema --experimental --out <DIR>`
- Codex 官方文档入口：
  https://developers.openai.com/codex/
- Codex CLI app-server 文档入口：
  https://developers.openai.com/codex/app-server/

### Claude Code / Agent SDK

- Claude Code 官方 CLI reference：
  https://code.claude.com/docs/en/cli-reference
- Claude Code 官方 headless/programmatic 文档：
  https://code.claude.com/docs/en/headless
- Claude Code 官方 session 管理文档：
  https://code.claude.com/docs/en/sessions
- Claude Agent SDK 官方 overview：
  https://platform.claude.com/docs/en/agent-sdk/overview
- Claude Agent SDK 官方 Python 仓库：
  https://github.com/anthropics/claude-agent-sdk-python
- Claude Agent SDK 官方 TypeScript 仓库：
  https://github.com/anthropics/claude-agent-sdk-typescript
- Claude Code 文档索引：
  https://code.claude.com/docs/llms.txt

