# 其他主流 AI Coding Harness 调研

> 调研日期：2026-09-01（Asia/Shanghai）。
>
> 范围：优先覆盖用户点名的 aider、Goose、Gemini CLI、Continue、Cline、OpenHands、SWE-agent；同时纳入截至调研日可由官方仓库确认且 GitHub stars 超过 10k、并且与“会话式 coding agent / harness”直接相关的项目。Stars 是 GitHub 页面或公开组织页在调研时显示的快照，会随时间变化，不是历史固定值；若不同抓取页面存在差异，以“约数”表达。
>
> 证据原则：启动、会话、消息、事件、状态和取消优先依据官方仓库/官方文档；未找到稳定官方 RPC/SDK 的地方明确标记，不以第三方包装器的行为冒充官方接口。

## 1. 快照总表

| 项目 | 官方仓库 | Stars 快照（2026-09-01） | 归档/生命周期 | 适配结论 |
|---|---|---:|---|---|
| aider | `Aider-AI/aider` | 约 48.6k | 活跃；仓库页面显示最新 release 为 2025-08-09，代码仍可调研 | 可适配，但应按 CLI 子进程适配 |
| Goose | `aaif-goose/goose`（原 `block/goose`） | 约 53.8k | 活跃；2026-04-07 从 Block 迁移至 AAIF | **优先适配**，官方 ACP |
| Gemini CLI | `google-gemini/gemini-cli` | 约 106.8k | 活跃；但官方在 2026 年发布了向 Antigravity CLI 过渡的公告，未来版本/品牌需锁版本 | 可适配；优先 `stream-json` 或 ACP，需版本 pin |
| Continue | `continuedev/continue` | 约 35.7k | 活跃；2026-09-01 的仓库快照仍有更新；CLI/IDE 入口正在演进 | 可适配，但必须锁定 CLI/协议版本 |
| Cline | `cline/cline` | 约 67.3k | 活跃；同仓库包含 SDK、CLI、IDE 及 Hub | **优先适配**，官方 SDK/事件/abort |
| OpenHands | `All-Hands-AI/OpenHands` | 约 85.8k | 活跃；另有 OpenHands CLI 和独立 software-agent-sdk 仓库 | **优先适配**，SDK/Agent Server/ACP |
| SWE-agent | `SWE-agent/SWE-agent` | 约 20.2k | **维护模式**；官方文档明确已被 mini-swe-agent supersede | 适合作业/评测适配，不适合作为长会话 harness |
| Roo Code | `RooCodeInc/Roo-Code` | 约 24.3k | **已归档：2026-05-15** | 仅存量复现，不新建主适配器 |
| Kilo Code | `Kilo-Org/kilocode` | 约 27.1k | 活跃；官方说明 CLI fork 自 OpenCode | 可按 OpenCode/ACP 风格适配，但需核实版本协议 |
| Open Interpreter | `openinterpreter/open-interpreter` / 后续 `openinterpreter/openinterpreter` | 约 68k | 活跃，但仓库/产品形态发生过迁移；原 Python 与新 Rust/类 Codex 形态需区分 | 可适配；优先新版 ACP/CLI，Python 形态另做一次性流适配 |
| Tabby | `TabbyML/tabby` | 约 33.8k | 活跃但更偏自托管 coding assistant/backend，不是完整 coding-agent turn harness | 适合补全/问答 API 适配；不应假设有通用 agent session |
| Plandex | `plandex-ai/plandex` | 本次未可靠核实（不以此行证明 >10k） | 低活跃；Cloud 自 2025-10-03 起停止接受新用户（该日期早于本次调研日） | 可作为 CLI/REPL 存量适配，优先级低 |
| smol developer | `smol-ai/developer` | 约 12.2k | 原型/低活跃；最近主要提交为 2024-04-07 | 不建议作为通用长会话适配目标 |
| GPT Engineer | `gpt-engineer-org/gpt-engineer` | 约 55k | **已归档：2025-05-14**；项目 README 称其为后续 Lovable 的 precursor | 仅历史/一次性代码生成适配 |

> 说明：`Tabby` 的定位主要是自托管补全、聊天和 OpenAPI 服务，并不等同于 Codex/Claude Code 这类“能执行工具的会话式 agent”。`Open Interpreter` 的旧 Python 项目与后续 Rust/类 Codex 项目不要混为一谈。

## 2. 统一观察模型

各项目表面不同，但可抽象为：

```text
Session（持久上下文/工作区）
  └─ Turn / Run（一次用户输入触发的执行）
       └─ Event（文本增量、思考摘要、工具调用、工具结果、审批、错误、终态）
```

最重要的兼容性差异不是“能否发送一句 prompt”，而是：

1. **传输**：进程 stdin/stdout、HTTP/SSE、WebSocket、JSON-RPC/ACP、进程内 SDK。
2. **会话句柄**：显式 session id、恢复参数、最近会话、文件/数据库历史，或根本没有稳定句柄。
3. **结束边界**：结构化 `result`/`done`/`turn.completed`，还是只能等待进程退出/解析终端输出。
4. **状态来源**：实时状态事件、可查询 snapshot、退出码，或仅凭“最后一条输出”推断。
5. **取消粒度**：取消当前 turn、暂停下一步、终止整个 session，或只能发 SIGINT/SIGTERM。
6. **交互阻塞**：工具审批、用户输入、OAuth、模型登录可能让 agent 处于 waiting，而不是 thinking。

统一层必须把 `thinking` 定义成**当前 turn 已开始且未收到终止事件**，而不是承诺暴露隐藏 chain-of-thought；reasoning 只能保存 harness 明确公开的摘要/增量。

## 3. 逐项目调研

### 3.1 aider

**官方启动/创建会话**

- 安装后在项目目录执行 `aider` 进入交互式 REPL；官方 README 以 `aider --model ...` 启动。
- 一次性入口是 `aider --message "..."` 或 `--message-file ...`，处理回复和编辑后退出；这更接近 `run(prompt)` 而非可复用远程 session。
- 持久上下文主要由本地文件提供：默认 `.aider.chat.history.md`、`.aider.input.history`；可用 `--restore-chat-history` 恢复上次聊天历史。

**发送/接收**

- REPL 中一条用户输入触发一次模型请求；CLI `--message` 将一条消息送入后退出。
- `--stream/--no-stream` 控制响应流式显示；流式内容通过终端 stdout 呈现。官方没有稳定的通用事件 JSON/JSON-RPC 协议。
- 交互命令 `/add`、`/read-only`、`/run`、`/test` 等会改变下一轮上下文或执行本地工具，不应被统一层误当作普通 assistant 文本。

**状态/取消**

- 官方文档描述 `CONTROL-C` 为中断；实现层适配应首先发送 SIGINT，并继续读取进程输出/退出状态。
- 没有官方的 `get_status(session_id)` 或独立 turn 状态 RPC；`thinking` 只能由子进程运行和输出阶段推断。
- “完成”应定义为收到该轮最终输出并回到 REPL 等待输入，或一次性模式进程退出；不要只按最后一行文本判断。

**统一适配**：中等。建议实现为“长驻 REPL + stdin/stdout + 本地历史”，并提供能力降级：无结构化事件、无可靠 session id、无远程 cancel。

### 3.2 Goose

**官方协议与启动**

- Goose 当前官方主协议是 ACP（Agent Client Protocol），仓库文档明确支持 `goose acp` 在 stdio 上启动 ACP agent，也支持 `goose serve` 提供 HTTP/WebSocket 服务。
- ACP 握手：`initialize`；创建：`session/new`；恢复：`session/load`；发送：`session/prompt`；取消：`session/cancel`。
- CLI 也提供 `goose run -n name -t "..."` 创建命名 session、`goose run -n name -r` 恢复，以及 `goose session -r` 从历史恢复；`--interactive` 可在初始任务后继续交互。

**发送/接收事件**

- `session/prompt` 返回停止原因，并在过程中通过 ACP notification 流式发送：assistant message chunk、thought chunk、tool call、tool call update，以及权限请求。
- 工具审批是独立的 request/response 交互；统一层应将其映射为 `WaitingForApproval`，不能将其当作普通 thinking。
- 官方文档还说明 session 历史、复制、fork、恢复等能力，适合映射为 session id + durable history。

**状态/取消**

- 正常结束可用 ACP `stopReason`，而不是猜测输出；运行中的状态通过更新通知和本地 session 状态展示。
- 官方 ACP 文档列有 `session/cancel`，但实际版本/transport 可能有能力差异；初始化返回的 capabilities 必须作为真值，不能无条件调用可选方法。
- 2026-06-30 的官方仓库 issue 报告跨客户端 `session/update` 不广播，说明多客户端观察同一 session 仍有版本限制；适配器应优先绑定创建该 session 的 ACP 通道。

**统一适配**：高。它是最接近目标标准接口的项目之一；直接复用 ACP adapter，保留 capabilities、permission request 和 stop reason。

### 3.3 Gemini CLI

**官方启动/创建会话**

- 安装后执行 `gemini` 进入交互式会话。
- 非交互入口：`gemini --prompt "..."` / `-p`；带初始 prompt 进入交互：`--prompt-interactive` / `-i`。
- 恢复：`--resume [session_id]` / `-r`，支持 latest、索引和完整 UUID；`--list-sessions` 列出当前项目会话。
- 官方还提供实验性 `--experimental-acp`，新适配应优先检测该能力并采用 ACP，而不是长期依赖 TUI。

**发送/接收**

- 单次 headless 调用使用 `--output-format json` 获取机器可读结果，或 `--output-format stream-json` 获取实时 JSONL 事件。
- stream-json 至少会发 init、user message、实时 agent progress/工具相关事件和最终 result；官方源代码明确使用 `StreamJsonFormatter` 并在结束时输出 result/stats/error 类信息。
- 交互模式输入由 TUI 处理；要做稳定程序化多轮，优先使用 ACP/锁定版本，而不是向 TUI 注入按键。

**状态/取消**

- 非交互模式捕获 Ctrl-C，调用 AbortController，等待 abort 流程完成并以适当退出码结束；适配器仍应继续消费 stdout/stderr 到进程退出。
- `stream-json` 的 init、progress、result/error 可以映射到 `Starting/Thinking/ToolRunning/Finished/Failed`；若某版本缺少某事件，应以 result/error/退出码兜底。
- 2026 年 Google 已发布从 Gemini CLI 过渡到 Antigravity CLI 的公告；这属于**未来/迁移风险**，不是说 2026-09-01 当天仓库已归档。必须 pin npm 版本并记录 `--output-format` schema。

**统一适配**：高（版本 pin 后）。`stream-json` 适合一次 turn 观察，ACP 适合长期 session；取消在纯 CLI 上是进程级，ACP 能力取决于版本。

### 3.4 Continue

**官方启动/创建会话**

- Continue CLI 命令是 `cn`；`cn` 开启 TUI session。
- `cn -p "..."` 为 headless 单次执行并退出；`cn -p --resume` 恢复最近 session。
- 配置来自 `~/.continue/config.yaml`、本地 agent/config 和 Continue 账号/API key；CLI 与 IDE 使用同一套 agent 配置思想。

**发送/接收**

- headless 默认将最终回复写 stdout；`--format json` 输出结构化 JSON，`--silent` 适合管道。
- IDE/CLI 内部会有 session history、stream aborter、active/inactive 状态和 tool call 生命周期；官方源码显示流通过 AbortSignal 终止，完成后将 session 标记 inactive。
- 官方文档没有承诺一个稳定的外部 session RPC；`--resume` 是最近 session 语义，不等同于可跨机器传递的公共 session id。

**状态/取消**

- CLI 中断可通过本地进程/AbortSignal 处理；结构化终态主要来自 headless 命令结束和 JSON 输出。
- Continue 的 CLI/IDE/API 入口和输出格式仍在演进；截至 2026-09-01 仓库快照仍有更新，因此不能将旧版文档或旧版 CLI 行为当作永久协议。

**统一适配**：中等。可做 CLI adapter（`cn -p --format json` + `--resume`），但不应把其内部 Redux/IDE 协议作为稳定公共接口；以具体版本的 CLI 文档和输出 schema 为准。

### 3.5 Cline

**官方启动/创建会话**

- 官方仓库现在同时提供 Cline SDK、CLI、IDE 和 Hub；SDK 安装为 `npm install @cline/sdk`。
- 最小 SDK 入口是创建 `Agent`，订阅事件，再 `await agent.run(prompt)`；Core 层支持 session 管理，Hub 层支持 attach/reconnect。
- CLI 支持交互任务、headless `cline "..."`、JSON 输出；`--zen` 将任务交给后台 hub daemon，CLI 立即退出，之后可从 history 读取。

**发送/接收**

- `Agent.run(prompt)` 负责一轮执行；Core session 事件通过 `cline.subscribe(listener, { sessionId })` 观察。
- 官方事件包括 `assistant-text-delta`、`tool-started`、`tool-finished`、`run-finished`、`usage-updated`；Core/host-facing 还有 `content_start/update/end`、`iteration_start/end`、`done`、`error`。
- `snapshot()` 返回当前 status、iteration、usage，适合统一层实现 `status()`。

**状态/取消**

- `run-finished` 的 result status 是一轮明确完成边界；`done/error` 是 session/host 层终态。
- `session.abort` 是官方取消边界；Hub 架构支持另一个客户端连接同一 session 并继续接收事件。SDK/CLI 需要保留“取消请求已发出但仍要 drain 尾部事件”的语义。
- 审批、pending prompt、工具等待应映射为 `WaitingForInput` / `WaitingForApproval`，不能简单标记为 idle。

**统一适配**：高。官方 SDK 是最好的路径；Rust channel 可通过 Node sidecar/CLI/ACP bridge 接入，避免自行猜测 Cline 内部存储。

### 3.6 OpenHands

**官方启动/创建会话**

- CLI：`openhands` 进入交互，`openhands -t "..."` 从任务启动，`--headless` 用于自动化，`--resume [id|--last]` 恢复。
- IDE 集成官方支持 `openhands acp`。
- SDK：`Conversation(agent=..., workspace=...)`；可用 `ConversationState.create()` 创建或从 persistence 恢复；也存在 LocalConversation/RemoteConversation 两种实现。
- Agent Server HTTP API 可通过 `POST /api/conversations/{conversation_id}/events` 发送消息，`run` 字段决定是否自动运行；远程通道以 HTTP + WebSocket 同步状态/事件。

**发送/接收**

- SDK 的核心顺序是 `conversation.send_message("Hello!")` → `conversation.run()`；事件保存于 append-only EventLog，并可通过 callback/visualizer 观察。
- Remote API 的消息是 role + content parts；事件服务将 user message 写入 conversation event log，再按 `run` 启动 agent loop。
- Agent Server/SDK 的事件模型覆盖消息、action/tool、observation、错误、状态和统计；适配器应订阅事件而不是抓取 Rich UI。

**状态/取消**

- 官方 `ConversationExecutionStatus` 包含 `IDLE`、`RUNNING`、`PAUSED`、`WAITING_FOR_CONFIRMATION`、`FINISHED`、`ERROR`、`STUCK`、`DELETING`。
- `EventLog.pause()` 可请求在下一个 agent step 间暂停；LLM 当前调用中不能保证立即生效。CLI 的 `Esc` 也是 pause，`Ctrl+Q`/`/exit` 退出。
- Remote/API 实现应使用 WebSocket 状态更新；本地 SDK 使用 `conversation.state.execution_status` 或事件日志。

**统一适配**：高。它的 Conversation + EventLog + execution status 与标准 Session/Turn/Event 模型高度匹配；要区分 pause、cancel、close，不能把 pause 错标为 cancelled。

### 3.7 SWE-agent

**官方启动/创建会话**

- CLI 使用 `sweagent run` 针对 GitHub issue、本地 repo 或 problem statement 启动一次作业；`run-batch` 批处理，`run-replay` 重放轨迹。
- 官方文档明确 SWE-agent 已被 mini-swe-agent superseded，并处于 maintenance-only；这是**当前状态**，不是待定计划。
- 一次 run 通常创建隔离 Docker/Modal 环境并产生 instance trajectory，而不是创建可供用户持续发送消息的长期 session。

**发送/接收**

- 内部是 problem statement → model query → action → environment observation 的循环；轨迹 `.traj` JSON 持久化 `response/thought/action/observation/state/query` 等字段。
- `Agent.run()` 循环 `step()`，直到 `StepOutput.done`，最终写 trajectory 和 predictions；事件是作业/步骤记录，不是实时统一事件协议。

**状态/取消**

- 终态主要是 `StepOutput.done`、`exit_status`、trajectory 文件和命令退出码；没有官方通用 session status/interrupt RPC。
- 取消应视为 job cancellation：终止 supervisor/容器/子进程，并保留部分 trajectory/log；不要伪装成“当前 turn 可恢复的 cancelled session”。

**统一适配**：低（对长期会话），中高（对一次性 job）。建议在统一库里放到 `JobHarness` 能力，而不是强行实现 `Session::send` 多轮语义。

### 3.8 Roo Code（已归档）

- 仓库 `RooCodeInc/Roo-Code` 在 **2026-05-15** 归档；截至 2026-09-01 属于过去事件，不能描述为仍活跃。
- 存量形态主要是 IDE task/session，适配需依赖扩展内部消息/持久化，不建议新建外部公共 adapter。
- 统一层可保留“archived compatibility”标签，但默认禁用自动发现和新功能承诺。

### 3.9 Kilo Code

- 官方仓库提供 VS Code/JetBrains/CLI/Cloud/Slack 等入口，CLI 安装为 `npm install -g @kilocode/cli`，运行 `kilo` 启动。
- 官方 README 明确 Kilo CLI fork 自 OpenCode，因此其生命周期和协议可能接近 OpenCode，但不能未经版本确认直接复用 OpenCode endpoint/schema。
- 面向统一适配，优先寻找其 CLI 的 headless/JSON/ACP 入口；如果仅有 TUI，则按子进程适配并记录版本。
- 适配结论：中高，但需要以具体版本的官方 CLI/ACP 文档为准；不要把 VS Code 扩展内部状态当作公共 RPC。

### 3.10 Open Interpreter

存在两种应区分的形态：

1. 旧的 Python `openinterpreter/open-interpreter`：
   - `interpreter` 启动交互终端；Python API `interpreter.chat(message)` 发送消息。
   - `interpreter.chat(..., stream=True)` 产生 chunk；模型消息、代码、执行结果以流式对象/终端 Markdown 展现。
   - 多轮由同一 Python `interpreter` 对象保留上下文；停止通常是 Python/进程级中断，没有标准 session RPC。
2. 后续 Rust/类 Codex 的 `openinterpreter/openinterpreter`：
   - 官方仓库页面显示其为面向 Kimi K3 的 coding agent，并支持 harness emulation（native、Claude Code、zcode、qwen-code、deepseek-tui、swe-agent 等）。
   - 这类新版更适合通过 ACP/类 app-server 适配；必须锁定仓库迁移后的具体版本，不能将旧 Python API 与新版命令混用。

统一适配：新版中高、旧版中等；先检测 executable/protocol capability，再选择 ACP 或 Python/CLI adapter。

### 3.11 Tabby

- Tabby 是自托管 coding assistant，官方 README 强调 self-contained、OpenAPI interface、可消费 GPU；运行的是 server/backend + IDE/客户端。
- 主要能力是代码补全、聊天/Answer Engine、repo context；它不是默认意义上的自主工具执行 agent。
- 可创建的“会话”多为聊天 thread/page 或客户端 UI 状态，官方没有与 Codex `turn/start` 等价的通用 agent loop RPC。
- 统一适配：若目标是问答/补全，可做 HTTP/OpenAPI provider adapter；若目标是 agent session/send/wait/cancel，应标记 `Unsupported(Capability::AgentSession)`，不要伪造。

### 3.12 Plandex

- `plandex`/`pdx` 在项目目录启动 REPL；支持 `--chat`、`--tell`、`--no-auto`、`--apply`、`--commit` 等模式。
- CLI 参考包含 `continue`、plan、上下文、apply 等持久工程状态；README 称其为面向大任务的 terminal coding tool。
- Cloud 自 **2025-10-03** 起停止接受新用户；截至 2026-09-01 是已发生的服务状态，仍可讨论 self-host/local mode，但不能假设云端新 session 可用。
- 官方材料没有稳定的通用外部事件流/状态 RPC；适配应为 CLI/REPL，取消采用 Ctrl-C/进程级。
- 统一适配：中低，适合存量 CLI 兼容，不建议作为首批远程 session target。

### 3.13 smol developer

- 官方仓库将其描述为 prototype/junior developer：通过一次 prompt 生成/重写整个 codebase，核心入口 `main.py` / `main_no_modal.py`。
- `modal run main.py --prompt ...` 或本地 `python main_no_modal.py YOUR_PROMPT_HERE` 执行；`prompt.md` 可作为输入。
- 典型行为是每次运行从 prompt 生成代码，且文档说明生成目录可能被删除并重写；不是持久 conversation/session。
- 没有通用 stream event、status 查询或 turn interrupt；只能封装成一次性 job 并观察子进程。
- 统一适配：低，不纳入默认 Session adapter。

### 3.14 GPT Engineer（已归档）

- 官方仓库截至 **2025-05-14** 归档，并将自身定位为 Lovable 的 precursor；这是已发生的归档日期。
- 传统入口是根据 prompt 生成项目，后续通过文件/提示迭代；更像 one-shot project generator，而不是长期流式 coding harness。
- 无必要为其实现新的通用 session adapter；如果需要复现历史行为，按 job runner 记录 prompt、生成目录、stdout/stderr 和退出码即可。

## 4. 适配优先级

### 第一梯队：直接做标准 Session adapter

1. **Goose ACP**：`initialize → session/new/load → session/prompt → notifications → stopReason → session/cancel`。
2. **Cline SDK/Core/Hub**：`create/start → subscribe → run → snapshot → session.abort`。
3. **OpenHands SDK/Agent Server/ACP**：`Conversation.create → send_message → run → EventLog/WebSocket → execution_status → pause/close`。
4. **Gemini CLI**：锁版本后使用 `stream-json`，若启用实验性 ACP 则切到 ACP。
5. **Open Interpreter 新 Rust 形态**：确认版本后复用 ACP/类 Codex 协议。

### 第二梯队：CLI stream adapter

- aider：REPL/`--message` + stdout + Ctrl-C + 本地 history。
- Continue：`cn -p --format json` + `--resume`，仅存量兼容。
- Kilo：按具体版本查 headless/ACP；否则 CLI 子进程。
- Plandex：REPL/CLI + 文件状态。
- Open Interpreter 旧 Python：进程内 Python API + generator stream。

### 第三梯队：Job / 非 agent-session adapter

- SWE-agent：trajectory job。
- smol developer：一次性生成 job。
- GPT Engineer：历史一次性生成 job。
- Tabby：补全/问答 provider，而不是 agent session。
- Roo Code：归档存量兼容。

## 5. 对 channel 统一接口的直接启示

统一实现不应要求每个 harness 都提供同等能力，而应提供“核心接口 + 能力声明”：

```rust
pub enum HarnessStatus {
    Idle,
    Thinking,
    RunningTool,
    WaitingForInput,
    WaitingForApproval,
    Finished,
    Interrupted,
    Failed,
    Closed,
}

pub struct Capabilities {
    pub durable_session: bool,
    pub streaming_events: bool,
    pub status_snapshot: bool,
    pub turn_cancel: bool,
    pub session_resume: bool,
    pub approvals: bool,
}

pub trait Session {
    async fn send(&mut self, text: &str) -> Result<()>;
    async fn next_event(&mut self) -> Result<Option<Event>>;
    async fn wait_finish(&mut self) -> Result<TurnResult>;
    async fn read_all(&mut self) -> Result<String>;
    async fn status(&self) -> Result<HarnessStatus>;
    async fn interrupt(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
}
```

建议的语义：

- `send` 只负责投递 prompt；不要隐式吞掉 stream。
- `next_event` 是统一层的真实底座；`read_all` 只是收集当前 turn 的可见 assistant 文本。
- `wait_finish` 必须等待明确终态事件；CLI adapter 在没有结构化终态时才使用“输出 + REPL 空闲/进程退出”兜底。
- `status` 优先返回 harness snapshot，否则根据最近事件、子进程状态和 exit code 推断，并在结果中标注 capability/可靠性。
- `interrupt` 只取消当前 turn；如果 harness 只有进程级终止，应明确能力降级并将 session 标记为不可继续或需要 resume。
- `read_all` 不返回隐藏思考；reasoning 只能通过公开 `Event::ReasoningDelta` 暴露。
- 等待审批/用户输入是非终态；调用方应能继续响应 approval/input，而不是让 `wait_finish` 永久阻塞。

最短调用仍可保持：

```rust
let mut s = channel::create(Harness::Codex, "你好").await?;
s.wait_finish().await?;
println!("{}", s.read_all().await?);
s.send("你是什么模型？").await?;
s.wait_finish().await?;
println!("{}", s.read_all().await?);
```

但实现上应将该极简 facade 建立在事件驱动的 `Session/Turn/Event` 内核之上，避免为适配 CLI 项目而退化成“sleep + 读最后一行”。

## 6. 来源索引（官方）

- aider README / scripting / options / commands：`https://github.com/Aider-AI/aider`、`https://aider.chat/docs/scripting.html`、`https://aider.chat/docs/config/options.html`、`https://aider.chat/docs/usage/commands.html`
- Goose README、ACP/custom distro、session management、running tasks：`https://github.com/aaif-goose/goose`、`https://github.com/aaif-goose/goose/blob/main/CUSTOM_DISTROS.md`、`https://goose-docs.ai/docs/guides/sessions/session-management/`、`https://goose-docs.ai/docs/guides/running-tasks/`
- Gemini CLI get started/config/CLI/source/changelog：`https://github.com/google-gemini/gemini-cli`、`https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md`、`https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/nonInteractiveCli.ts`
- Continue CLI/docs：`https://docs.continue.dev/cli/quickstart`、`https://docs.continue.dev/cli/headless-mode`、`https://docs.continue.dev/index`（CLI 及输出格式应按 2026-09-01 实际版本复核）
- Cline SDK/events/architecture/CLI：`https://docs.cline.bot/sdk/overview`、`https://docs.cline.bot/sdk/events`、`https://github.com/cline/cline/blob/main/sdk/ARCHITECTURE.md`、`https://github.com/cline/cline/blob/main/apps/cli/README.md`
- OpenHands CLI/SDK/Agent Server：`https://docs.openhands.dev/openhands/usage/cli/quick-start`、`https://docs.openhands.dev/sdk/api-reference/openhands.sdk.conversation`、`https://docs.openhands.dev/sdk/guides/agent-server/api-reference/events/send-message`
- SWE-agent CLI/trajectory/agent：`https://swe-agent.com/latest/usage/cli/`、`https://swe-agent.com/latest/usage/trajectories/`、`https://swe-agent.com/latest/reference/agent/`
- Open Interpreter：`https://github.com/openinterpreter/open-interpreter`、`https://github.com/openinterpreter/openinterpreter`
- Tabby：`https://github.com/TabbyML/tabby`
- Plandex：`https://github.com/plandex-ai/plandex`、`https://github.com/plandex-ai/plandex/blob/main/docs/docs/cli-reference.md`
- smol developer：`https://github.com/smol-ai/developer`
- GPT Engineer：`https://github.com/gpt-engineer-org/gpt-engineer`
- Kilo Code：`https://github.com/Kilo-Org/kilocode`
- Roo Code：`https://github.com/RooCodeInc/Roo-Code`
