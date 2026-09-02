# 主流 AI coding harness 会话生命周期调研

- 调研日期：2026-09-01
- 目标：核实 OpenCode、Zed/ZCode、DeepSeek Harness，以及 GitHub Stars 超过 10k 的主流 coding harness 的会话创建、发送消息、接收输出、状态观察和中断机制。
- 证据优先级：官方文档、官方仓库源码/README、官方仓库 issue/discussion。第三方材料只用于发现线索，不作为关键协议事实的唯一依据。
- 说明：GitHub Stars、CLI 参数和协议均可能变化；以下星数是本次调研页面显示的快照，不是永久属性。所谓“主流”按公开影响力、实际 coding-agent 使用度和本次检索到的官方项目综合判断，不声称穷举所有商业闭源产品。

## 1. 先给结论

### 1.1 实际存在三类会话接口

1. **原生 RPC/服务器型**
   - 代表：OpenCode Server、Codex App Server、ACP Agent。
   - 特征：创建会话返回稳定 ID；消息提交与事件流分离；中断有显式 RPC/notification；生命周期有明确终态。
   - 最适合被 `channel` 适配。

2. **CLI 进程型**
   - 代表：Aider、Goose CLI、Gemini CLI、Continue CLI、Claude Code CLI、SWE-agent。
   - 特征：会话通常由一个前台进程持有；输入来自 stdin/命令参数；输出来自 stdout/stderr；Ctrl-C 或进程终止是最常见的中断手段；部分项目把历史持久化到文件或数据库。
   - 可以适配，但必须由适配器自行实现进程生命周期、输出解析、超时和退出码映射。

3. **IDE/桌面任务型**
   - 代表：Zed Agent、ZCode、Cline、Roo Code、OpenHands Canvas。
   - 特征：用户看到的是 thread/task/conversation；运行时常由 IDE 或本地服务托管；状态和审批事件比纯文本更丰富；对外 API 可能不是稳定公共接口。
   - 若没有公开协议，应优先寻找 ACP、SDK 或本地 server，而不是抓取 UI。

### 1.2 “Thinking”不是跨项目统一状态

各项目普遍能表达“正在处理”，但字段不统一：

- OpenCode：`session.status` 至少有 `idle`、`active`、`error`；`active` 是粗粒度运行状态。
- ACP：通过 `session/update` 的消息/工具/进度通知，以及回合结束时的 `state_update: idle` 和 `stopReason` 判断。
- Codex App Server：turn 状态是 `inProgress`、`completed`、`interrupted`、`failed`。
- DeepSeek Harness：从 `turn/*`、`step/*`、`assistant/chunk` 等事件推导运行中；Web/ACP 表面另有状态通知。
- CLI 类项目通常没有稳定的“thinking”协议字段，只能从进程仍存活、尚未收到最终输出/退出、或 `stream-json` 事件推断。

因此统一接口不应把 `thinking` 当成所有后端都能精确提供的事实，而应定义：

```text
Idle | Running | WaitingForPermission | Completed | Interrupted | Failed | Unknown
```

其中 `Running` 可以包含模型思考、工具执行和等待子任务；后端有更细粒度事件时再暴露原始事件。

### 1.3 “会话”和“回合”必须分开

一个 session/thread/task 可以包含多次 user→agent 交互；每次 `send` 应对应一个 turn/run。中断通常只取消当前 turn，而不是删除整个 session：

```text
Session: 可持续保存上下文的容器
Turn:    一次用户输入及其模型/工具执行过程
Event:   Turn 中的增量输出、工具调用、审批、状态和终态
```

这是 Codex、ACP、OpenCode、DeepSeek Harness 和 Cline 等实现中最稳定的共同点。

### 1.4 正确的接收模型是“事件流 + 聚合结果”

不能只等待一个字符串结果。适配器至少需要：

1. 消费增量文本/推理/工具事件。
2. 持续更新当前 turn 状态。
3. 在终态事件或进程退出时关闭本轮。
4. 把文本事件聚合成 `read_all()` 可返回的最终文本。
5. 保留原始事件，避免工具调用、审批、错误和中断信息丢失。

### 1.5 中断必须有“请求”和“确认终态”两步

发送中断不等于已经停止。可靠流程是：

```text
interrupt() -> 请求后端停止当前 turn
等待终态 -> Completed / Interrupted / Failed
```

Codex 明确要求依赖 `turn/completed` 判断中断完成；ACP 要求原 `session/prompt` 以 `stopReason = cancelled` 结束；OpenCode 提供 `/session/:id/abort`；Cline 的 SDK 也把 `session.abort` 作为取消边界。

## 2. 项目逐项调研

## 2.1 OpenCode

### 项目定位和证据

OpenCode 官方仓库为 `anomalyco/opencode`。本次调研确认其提供本地 server/API、Session/Message API 和 SSE 事件流；官方仓库的 v2 session spec 仍在演进。OpenCode GitHub 页面显示其为高关注度项目；本报告不把动态 Stars 数作为协议兼容性的前提。

### 创建会话

Server API：

```http
POST /session
```

请求体可包含 `parentID`、`title` 等字段，返回 `Session`，其中包含稳定 session ID。

### 发送消息

同步发送：

```http
POST /session/:id/message
```

返回本次 assistant message 及其 parts；适合“一次请求等待本轮完成”的调用。

异步发送：

```http
POST /session/:id/prompt_async
```

返回 `204`，调用方随后通过事件流或消息查询获取结果。

### 接收输出

实时事件：

```http
GET /event
```

该接口使用 SSE，转发总线事件。可观察 session 创建/更新/删除、消息增量、工具调用、错误和 `session.status`。

历史消息：

```http
GET /session/:id/message
GET /session/:id/message/:messageID
```

### 状态

OpenCode 的公开状态至少包括：

```text
idle   没有活动执行
active 正在处理
error  会话发生错误
```

`active` 不等价于“模型正在思考”，它也可能表示工具执行、权限等待或其他运行阶段；如需细分，应消费消息/工具事件。

### 中断

```http
POST /session/:id/abort
```

该操作取消 session 的活动处理。调用方仍应等待后续状态/错误/消息事件，不能仅凭 abort HTTP 成功就判定本轮已经结束。

### 其他能力

官方 API 还包括 session fork、diff、revert、summarize、permission response 等。OpenCode 的 session history/event-sourcing v2 规范说明其正在把持久事件、分页历史、回放和实时 tail 作为核心模型。

### 适配评价

OpenCode 是非常适合 `channel` 的后端：

- session ID 明确。
- 同步和异步 prompt 都有。
- 有独立事件流。
- 有状态查询。
- 有显式 abort。

主要风险是 API v2 正在演进，必须将协议版本和原始事件保留在适配器层，不要把某个具体事件字段硬编码成全局语义。

## 2.2 Zed：不是单一 harness，而是三条 Agent Path

Zed 官方文档明确区分：

1. **Zed Agent**：Zed 自己托管的原生 agent。
2. **External Agents**：通过 ACP 集成 Claude、Codex、OpenCode、Copilot、Cursor、Pi 等外部 agent。
3. **Terminal Threads**：在 Zed 的 terminal-backed thread 中运行 CLI/TUI。

因此“Zed harness”如果指编辑器本身，应优先研究 Zed Agent；如果指在 Zed 中运行其他 harness，应研究 ACP，而不是 Zed 私有 UI。

### Zed Agent 原生路径

用户通过 Agent Panel 创建新的 thread，在输入框提交 prompt；消息生成时显示工具调用和流式响应。线程有历史、归档、恢复、排队消息和部分情况下的 steer 行为。

公开文档主要描述 UI/产品行为，没有提供一个供任意外部程序直接创建 Zed Agent thread、发送消息和订阅状态的稳定公共 RPC API。因此不建议 `channel` 直接自动化 Zed 原生 Agent，除非后续找到官方 SDK/server 接口。

### External Agent / ACP 路径

ACP 是 Zed 与外部 coding agent 之间的标准 JSON-RPC 协议，通常通过 stdio 运行 agent 子进程。

初始化：

```text
Client -> Agent: initialize
```

创建/恢复：

```text
Client -> Agent: session/new
Client -> Agent: session/resume   （如果 agent 支持）
```

发送消息：

```text
Client -> Agent: session/prompt
```

该请求被接受后，agent 通过 `session/update` notification 推送：

- assistant 文本增量。
- reasoning/思考增量（若 agent 暴露）。
- tool call 和 tool call update。
- plan/progress/usage 等更新。
- 需要用户决定时的 permission request。

本轮结束时，`session/prompt` 返回 `stopReason`；协议文档描述客户端随后会收到 idle 状态更新。

中断：

```text
Client -> Agent: session/cancel   （notification）
```

协议语义是取消当前 prompt turn：停止模型请求、尽可能终止工具执行、发送待处理更新，并让原 prompt 以 `cancelled` 终态结束。

关闭：

```text
session/close
```

只有 agent 宣布支持该 capability 时才能调用；关闭应取消活动工作并释放资源。

### ACP 对 `channel` 的意义

ACP 是跨 harness 最有价值的标准化入口。Codex、Claude、OpenCode、Goose、Gemini CLI、Cline 等都可以通过 ACP 或 ACP bridge 接入 Zed/其他 client。`channel` 应把 ACP 作为优先级最高的通用传输适配器。

## 2.3 “ZCode”名称核实

### 结论

“ZCode”不是 Zed 的官方别名。当前能核实到的是 **ZCode Agent（zcode.z.ai）**，属于 Z.ai 的独立 coding agent 产品；Zed 是另一个编辑器和 ACP client。搜索到的 `zcode-acp` 是社区 bridge/实现，不应当被误认为 Zed 官方协议。

### ZCode Agent 会话模型

官方文档将会话称为 task/conversation：

- 新建任务时选择 ZCode Agent，并在工作区中提交第一条消息。
- 同一个 task 内可以继续发送消息、引用文件、调用命令、使用 skills、切换执行模式和模型。
- 任务列表支持归档、恢复和查看历史。
- ZCode 会在成功回合后提炼项目记忆，并在同项目的新 session 中自动携带。
- 可以从已完成的 assistant 消息 fork；被中断或失败的半截回复不能 fork。
- fork 只复制对话历史，不回滚磁盘文件，也不复制排队消息、正在执行的工作和后台任务。

### 状态和中断

ZCode 官方产品文档/更新日志能确认 UI 有思考轨迹、回合结束摘要、错误重试和中断任务状态修复，但本次没有找到稳定公开的、面向第三方的 session RPC/API 规范，无法核实：

- 外部程序如何创建一个 ZCode task。
- 如何通过公共接口提交下一条 prompt。
- 是否有 SSE/WebSocket/JSON-RPC 事件协议。
- “thinking、工具执行、权限等待、结束、中断”各自对应什么机器字段。

因此当前只能将 ZCode 适配为：

1. 优先等待官方 API/SDK/ACP 支持。
2. 若 ZCode 提供 ACP adapter，则通过 ACP 适配。
3. 不建议基于桌面 UI、数据库私有表或非官方 bridge 建立稳定生产接口。

## 2.4 DeepSeek Harness（DSH）

### 项目和成熟度

`deepseek-ai/deepseek-harness` 是 DeepSeek AI 官方开源项目，官方 README 将其标记为 developer preview，并明确提示可能存在兼容性破坏变更。本次调研页面显示约 33.6k Stars，但该数字会动态变化。

DSH 的核心设计是“Everything is a Plugin”，底层使用 Cordis。它不是简单的单次 Chat Completions 包装，而是拥有 agent loop、session event log、tool pipeline、插件和多种接入表面的 agent runtime。

### Session 是 append-only event log

官方 session/core 文档确认：

- Session 是 typed `SessionEvent` 的 append-only log。
- 每个事件有单调递增 `seq`、时间和类型化 payload。
- LLM message history 由事件日志 `deriveMessages()` 推导，而不是独立真相源。
- 公开事件包括：
  - `turn/start`
  - `turn/end`
  - `step/start`
  - `step/end`
  - `user/message`
  - `assistant/chunk`
  - `assistant/message`
  - `tool/call`
  - `tool/result`
  - `steering/message`
  - `todo/write`
  - `request/header`

### 一次 turn 的典型流程

```text
user input
  -> inbox / message accepted
  -> turn/start
  -> claim queued input
  -> step/start
  -> user/message
  -> model request and assistant/chunk*
  -> assistant/message
  -> tool/call -> tool/result  （可重复）
  -> step/end
  -> more work? continue step : turn/end
```

一个 step 通常表示一次模型请求及其产生的工具调用；一个 turn 可能包含多个 step，并在 runtime 不再欠用户/系统工作时才结束。

### 接收输出和获知状态

可靠做法是消费 session event stream：

- `assistant/chunk`：增量文本/推理片段。
- `assistant/message`：完整 assistant message。
- `tool/call` / `tool/result`：工具执行生命周期。
- `turn/start` / `step/start`：进入运行阶段。
- `step/end` / `turn/end`：阶段/回合结束。

因此 DSH 的“thinking”应被理解为 `turn` 已开始但尚未产生终态；若要显示更细状态，应根据 `step/start`、`assistant/chunk` 和 tool 事件组合推导，而不是依赖一个单一字段。

### 中断

DSH 不同传输表面的能力不完全一致：

- 官方 ACP surface 支持面向 session 的 `session/cancel`，语义是只取消目标 agent、settle pending prompt，并以 cancelled 结束。
- Web gateway 已有 agent cancel 原语，设计上可保留 inbox，便于用户停止后继续。
- DSH SDK JSON-RPC server 在本次官方仓库讨论记录中仍被指出缺少 cancel/session-close；该表面当前可能需要关闭 runtime process 才能放弃 turn。

关闭整个进程不是等价的中断：它可能丢失进程内 session 状态，并连带影响同一 runtime 中的其他 session。因此 DSH adapter 必须先探测 transport capability：

```text
if supports_session_cancel:
    session/cancel(session_id)
else:
    terminate only the owning runtime process
    mark result as Interrupted or Failed(transport_terminated)
```

### 关键风险

DSH developer preview 的事件日志和恢复语义仍在快速迭代。官方讨论中已经出现中断后 resume、重复 seq、工具调用未闭合、进程级取消导致多 session 连带终止等问题。`channel` 不应只保存最终文本，必须保存：

- 原始事件。
- seq/turn/step/message/call ID。
- transport 类型和版本。
- 中断请求时间、确认终态和进程退出信息。

## 2.5 Aider

- GitHub 页面显示约 46.2k Stars。
- 主要入口是 terminal interactive chat，也支持单次脚本调用。
- 会话由当前 aider 进程维护；可用 `--restore-chat-history` 恢复历史。
- Chat history 默认写入 `.aider.chat.history.md`；另有 LLM raw history 和输入历史文件。
- 默认启用流式输出。
- Ctrl-C 可安全中断当前生成，官方文档说明部分响应会保留在当前 conversation 中；短时间连续 Ctrl-C 会强制退出。
- 未找到一个稳定、官方、面向第三方的 session server API、状态查询 API 或结构化 cancel RPC。

适配方式：启动一个受控 aider 进程，解析 stdout/stderr，使用交互 stdin 发送下一条消息；状态只能从输出、进程存活和退出码推断。该方式能工作，但不如 ACP/Server 可靠。

## 2.6 Goose

- 官方仓库已从 `block/goose` 迁移到 Agentic AI Foundation 组织，页面显示约 44.5k Stars。
- CLI 新 session：`goose session` 或 `goose session -n NAME`。
- 恢复：`--resume`、`--session-id`、`--path`。
- 分叉：`--resume --fork`。
- 从 Goose 1.10 起，session storage 使用 SQLite；旧 `.jsonl` 仍可能留在磁盘但不再由新版本管理。
- headless 入口：`goose run -t TEXT`，可配合 `--interactive`、`--resume`、`--no-session`。
- CLI 的 Ctrl-C 语义：有输入时清除当前行；处理请求时中断当前 request；空行时退出 session。
- 官方材料确认 Goose 支持流式 provider；环境变量可显示 DeepSeek-R1 等模型的 thinking 输出。
- Goose 也提供 ACP/服务器方向的集成，但具体实现和版本能力应通过 ACP capability negotiation 判断。

适配方式：

1. 优先 `goose acp` + ACP。
2. 次选 CLI 进程 + named session/SQLite 持久化。
3. 不直接读 SQLite 作为唯一协议，因为 schema 和版本迁移不属于稳定外部 API。

## 2.7 Cline

- GitHub 页面显示约 67.3k Stars。
- Cline 把一次独立工作单元称为 task。
- 新 task：侧边栏 `+` 或 `/newtask`，第一条 prompt 开始 task。
- 每个 task 有唯一 ID、独立存储目录、完整对话历史、工具执行记录、token/cost/time 统计。
- task 可以被中断，并跨 session resume；还会创建 Git-based checkpoints。
- Cline SDK/core 架构提供 session、event、approval、runtime capability 等服务。
- SDK 事件包含 `run.started`、文本/reasoning delta、最终文本/reasoning、tool start/update/finish 和 agent done。
- 取消边界是 `session.abort`；取消时应先建立 cancel fence，再 abort，避免迟到事件污染新回合；最终状态可标记为 cancelled。
- Cline 还有 API/CLI surface，但“Cline API 的 chat completions”更偏模型网关，不等价于 IDE task session；要控制真实 coding task，应使用其 SDK/core/session surface 或 ACP 集成。

适配评价：Cline 的 session/task 模型比较完整，但公共 SDK 仍可能随版本变化。适配器应以事件和 session ID 为中心，不解析 UI 文本。

## 2.8 Roo Code

- GitHub 页面显示约 24.2k Stars。
- Roo Code 仓库已于 **2026-05-15** 被归档，README 声明 Roo Code 扩展已关闭，并建议使用社区 fork ZooCode 或 Cline。
- 历史模型是 Cline 派生的 IDE task：task 有独立历史，可 resume；运行中可取消；源码可见 `abortReason`、task rehydrate 和恢复任务消息。
- 官方 issue 记录过 streaming API request 中取消导致内容保存竞态的问题，说明取消需要等待持久化完成后再重新加载历史。

结论：Roo Code 适合做兼容性/迁移参考，不适合作为新适配器的主要目标。新实现应优先 Cline 或其仍受支持的后继项目。

## 2.9 OpenHands

- 官方仓库已转向 `OpenHands/OpenHands`，页面显示约 85.8k Stars。
- 当前 Agent Canvas 是 self-hosted developer control center，支持 OpenHands、Claude Code、Codex、Gemini 和 ACP-compatible agent。
- 官方 README 明确把 `OpenHands/software-agent-sdk` 作为 Python SDK、Agent Server、agents、tools、conversations、workspaces、events 和 canonical server API 的归属仓库。
- Agent Server 是用于一台机器运行多个 agent 的 REST API；Agent Canvas 可以连接多个 server/backend。
- 这意味着 OpenHands 的稳定集成边界应是 Agent Server/SDK 或 ACP，而不是 Canvas UI。

本次检索没有在已获取的官方页面中核实完整、当前版本的 create-conversation/send-message/abort endpoint 名称，因此不把猜测的 URL 写入适配协议。实现时应锁定对应 SDK/API 版本并从其 OpenAPI/类型定义生成 client。

## 2.10 SWE-agent

- GitHub 页面显示约 20.2k Stars。
- 官方 README 已说明当前主要开发转向 mini-SWE-agent；SWE-agent 仍是研究型、配置驱动的 issue-to-patch harness。
- 标准流程是 `sweagent run`：读取问题描述，初始化环境，循环执行 model step、解析 action、运行工具、记录 observation，直到提交/失败/预算终止。
- 每个实例生成 `.traj` JSON，保存 thought/action/observation/state/query，以及 config/log/exit status 等。
- trajectory 是持久审计/重放记录，但不是 live session resume：未完成 trajectory 通常被视为不可复用，replay 会重新运行动作，不会恢复原进程并继续模型上下文。
- 没有面向交互式多轮会话的稳定公共消息 server 或 cancel API；中断通常是停止进程/任务执行。

结论：SWE-agent 应在 `channel` 中被建模为“job/trajectory harness”，不能假设它具备通用 chat session 语义。其 `wait_finish` 可映射为等待 job terminal state，`read_all` 可映射为 trajectory/最终 patch/日志聚合。

## 2.11 Gemini CLI

- GitHub 页面显示约 106.8k Stars。
- 新会话：在项目目录运行 `gemini`，或用 `gemini -p PROMPT` 进行 headless 单次执行。
- 交互模式默认持续接收后续输入；`-i/--prompt-interactive` 用初始 prompt 启动交互 session。
- 自动保存完整历史，包括 prompts、assistant responses、工具执行、token usage 和可用的 thoughts/reasoning summaries。
- session 按项目目录隔离，默认位于 `~/.gemini/tmp/<project_hash>/chats/`。
- 恢复：`gemini --resume`、`gemini -r latest`、`gemini -r SESSION_ID`。
- 列举/删除：`--list-sessions`、`--delete-session`。
- 自动化输出：`--output-format json` 或 `stream-json`；后者用于实时事件。
- 中断后，官方文档说明后台保存机制用于保留工作，因此可恢复已保存内容；但没有发现独立公共 RPC 的 session status/cancel 方法。

适配方式：优先使用 `stream-json` + `--resume SESSION_ID`；状态从结构化事件、进程退出和最终事件推断。不要假定“进程被 Ctrl-C 杀掉”一定等价于一个已经写入的、干净的 cancelled turn。

## 2.12 Continue CLI

- GitHub 页面显示约 33.7k Stars。
- `cn` 是 Continue 的 terminal coding agent；直接运行 `cn` 进入 TUI session，`cn -p PROMPT` 运行 headless 单次任务。
- `--resume` 恢复最近 session；TUI 中 `/resume`、`/fork`、`/compact` 等命令管理会话。
- 内部 session 类型包含 `sessionId`、title、workspaceDirectory、history、mode 和 usage。
- Continue SDK 使用 `AbortController` 作为当前请求的取消控制点。
- 工具权限、Agent/Plan/Chat/Background 模式属于 session/runtime 配置的一部分。
- 没有在官方文档中找到对外稳定的远程 session RPC 或标准事件协议；CLI/TUI 和内部 SDK 是主要控制面。

适配方式：若有可调用 SDK，则传递 abort signal；否则使用 `cn -p`/TUI 进程，并把 `--resume` 作为恢复参数。输出结构和状态需要锁定 Continue CLI 版本。

## 2.13 Codex App Server（相关基线）

Codex 虽然不属于本次“Stars >10k”筛选的必要条件，但它是本项目原始目标，且提供了最完整的官方可编程控制面：

```text
initialize
  -> thread/start 或 thread/resume/fork
  -> turn/start
  -> thread/turn/item notifications
  -> turn/completed
```

- `thread/start` 创建新会话。
- `thread/resume` 恢复已有会话。
- `turn/start` 发送用户输入，返回 turn 对象并异步推送事件。
- `item/agentMessage/delta` 等事件提供流式输出。
- `turn/completed` 的状态为 `completed`、`interrupted` 或 `failed`。
- `turn/interrupt(threadId, turnId)` 中断当前回合；调用方应等待 `turn/completed`。
- thread history 可通过 `thread/list`、`thread/read` 等 API 读取。

Codex 的模型非常适合作为 `channel` 首个强类型适配器，也可以作为统一抽象的参考实现。

## 2.14 Claude Code（相关基线）

Claude Code 也属于本项目原始目标。官方 CLI 支持：

- `claude -p PROMPT` 非交互执行。
- `--output-format text|json|stream-json`。
- `--continue` 继续当前目录最近 conversation。
- `--resume SESSION_ID PROMPT` 恢复指定 session 并发送新消息。
- `--max-turns` 限制非交互 agentic turns。

Claude Code 的 CLI 是很好的进程型适配目标，但本次没有把其完整内部事件 schema 和 application-level cancel RPC 核实到足够稳定的程度；实现应优先使用其官方 SDK/ACP 集成，或将 CLI 进程适配器明确标注为版本绑定。

## 3. 跨项目通用流程

## 3.1 初始化/连接

```text
启动或连接 backend
  -> capability/version negotiation（若协议支持）
  -> 认证/读取配置
  -> 设置 cwd/workspace、模型、权限策略、sandbox
```

要求：

- 能区分“后端未启动”“认证失败”“session 创建失败”。
- 保存 backend 类型、版本、transport 和 capability。
- 不把 cwd、模型和权限策略藏在全局单例中；它们属于 session 配置。

## 3.2 创建或恢复 session

```text
create({ harness, cwd, options }) -> SessionHandle
resume({ harness, session_id }) -> SessionHandle
```

优先顺序：

1. 原生 server/RPC。
2. ACP。
3. 官方 SDK。
4. 官方 CLI 参数。
5. 最后才是进程 stdin/stdout 的兼容适配。

## 3.3 发送一个 turn

```text
send(session, input)
  -> 创建 request/turn/run id
  -> 将 user input 写入后端
  -> 持续消费事件
  -> 更新状态和输出缓冲
  -> 等待终态
```

应允许一轮中出现：

- assistant text delta。
- reasoning/thinking delta。
- tool call/update/result。
- permission request。
- plan/progress/usage。
- warning/error。
- queued/steering input。

## 3.4 状态机

统一状态建议：

```text
Created
  -> Idle
  -> Running
  -> WaitingForPermission
  -> Completed
  -> Interrupted
  -> Failed
  -> Closed
```

约束：

- `Running` 可以覆盖 thinking、工具调用、子 agent 工作和模型重试。
- `WaitingForPermission` 只有后端明确发出审批请求时才使用，否则仍是 `Running`。
- `Completed`、`Interrupted`、`Failed` 都是当前 turn 的终态。
- Session 本身在一个 turn 终止后通常回到 `Idle`，而不是变成 `Closed`。
- `Closed` 只表示 session/runtime 被关闭，不能用于表示单轮完成。
- 如果只有进程退出而无明确终态，状态应为 `Failed` 或 `Interrupted`，并附带 `termination = signal/exit_code/transport_closed`。

## 3.5 接收和读取

应同时提供两种模式：

1. **实时订阅**：调用方逐事件消费。
2. **最终聚合**：调用方等待本轮终态后调用 `read_all()`，获取 assistant 最终文本。

建议保留：

```text
text_output
reasoning_output（可选）
tool_events
permission_events
raw_events
usage
final_status
error
```

## 3.6 中断

```text
interrupt()
  -> 向后端发送 cancel/abort 或终止拥有该 session 的进程
  -> 继续消费事件
  -> 等待明确终态
  -> 返回 Interrupted 或 backend error
```

必须避免：

- abort 请求返回成功后立即创建下一轮，导致旧事件污染新轮次。
- 关闭共享 runtime，连带杀死其他 session。
- 丢弃中断前已收到的 assistant delta/tool result。
- 重新加载历史时覆盖尚未持久化的中断内容。

## 3.7 关闭和恢复

`close()` 的语义是释放 transport/runtime；不是删除历史。恢复时：

```text
close transport
  -> 保留 session_id 和持久化位置
  -> 新建连接
  -> resume(session_id)
  -> 重新订阅事件
```

如果后端只支持 CLI 文件恢复，则将恢复参数和工作目录一并保存。

## 4. 统一接口设计建议

以下是对 Rust 用户最小化、但仍覆盖通用能力的建议形状。具体名称可按项目现有风格调整：

```rust
let mut s = channel::create(Harness::Codex, "你好").await?;
s.wait_finish().await?;
let answer = s.read_all().await?;

s.send("你是什么模型？").await?;
s.wait_finish().await?;
let answer = s.read_all().await?;
```

建议公开的最小接口：

```rust
pub async fn create(
    harness: Harness,
    first_message: impl Into<String>,
) -> Result<Session>;

impl Session {
    pub async fn send(&mut self, message: impl Into<String>) -> Result<()>;
    pub async fn wait_finish(&mut self) -> Result<Finish>;
    pub async fn read_all(&mut self) -> Result<String>;
    pub async fn status(&self) -> Result<Status>;
    pub async fn interrupt(&mut self) -> Result<()>;
}
```

必要的类型：

```rust
pub enum Harness {
    Codex,
    ClaudeCode,
    OpenCode,
    ZedAcp,
    ZCode,
    DeepSeek,
    Aider,
    Goose,
    Cline,
    RooCode,
    OpenHands,
    SweAgent,
    GeminiCli,
    Continue,
    Custom(String),
}

pub enum Status {
    Idle,
    Running,
    WaitingForPermission,
    Completed,
    Interrupted,
    Failed,
    Closed,
    Unknown,
}

pub struct Finish {
    pub status: Status,
    pub text: String,
}
```

实现层建议再提供但不强迫普通用户使用的扩展：

```rust
impl Session {
    pub fn id(&self) -> &str;
    pub fn harness(&self) -> Harness;
    pub async fn subscribe(&mut self) -> Result<EventStream>;
    pub async fn send_with(&mut self, message: Message, options: SendOptions) -> Result<()>;
    pub async fn respond_permission(&mut self, request_id: &str, response: Permission) -> Result<()>;
    pub async fn close(&mut self) -> Result<()>;
}
```

### 4.1 为什么 `send()` 不直接返回字符串

因为真实 harness 的一次 send 可能持续数分钟，期间会发生工具调用、审批、重试和增量输出。`send()` 只负责提交 turn；`wait_finish()` 负责等待终态；`read_all()` 负责读取已聚合结果，正好匹配用户给出的极简调用。

### 4.2 为什么 `wait_finish()` 必须独立

- 允许实时 `subscribe()` 和阻塞等待两种消费者。
- 允许调用者在等待期间查询状态、展示进度或调用 interrupt。
- 统一 Server、ACP 和 CLI 进程三类后端。
- 避免把“HTTP 请求返回”“prompt 被接受”“模型真正结束”“进程退出”混为一谈。

### 4.3 适配器内部统一结构

```text
HarnessAdapter
  - create/resume
  - send
  - next_event
  - status
  - interrupt
  - close

SessionRuntime
  - session_id
  - current_turn_id
  - event receiver
  - text/reasoning/tool buffers
  - status machine
  - raw event log
  - child process/server handle
```

每个适配器只负责把后端协议转换为统一 `Event`：

```rust
pub enum Event {
    UserMessage(String),
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted { id: String, name: String },
    ToolUpdated { id: String, detail: String },
    ToolFinished { id: String, output: String },
    PermissionRequired(PermissionRequest),
    Status(Status),
    Usage(Usage),
    Error(Error),
    Finished(Finish),
    Raw { kind: String, payload: serde_json::Value },
}
```

### 4.4 能力探测

统一接口必须允许后端能力缺失：

```rust
pub struct Capabilities {
    pub streaming: bool,
    pub status_query: bool,
    pub interrupt: bool,
    pub resume: bool,
    pub permission_requests: bool,
    pub structured_events: bool,
    pub fork: bool,
}
```

例如：

- OpenCode：上述能力大多存在。
- ACP：能力由 initialize/session capabilities 协商。
- DSH SDK JSON-RPC 某些版本：可能没有 session cancel/close。
- Aider/SWE-agent：通常没有独立状态查询或结构化中断。
- ZCode：当前未核实公开第三方 API，能力应报告为 Unknown，而不是假设支持。

## 5. 对当前项目实现的直接建议

1. **第一层实现 ACP client**：覆盖 Codex、Claude Code、OpenCode、Goose、Gemini CLI、Cline 等可通过 ACP 暴露的 agent。
2. **第二层实现 Codex App Server adapter**：作为本项目最初示例和强类型参考。
3. **第三层实现 OpenCode Server adapter**：HTTP + SSE，验证 Session/Message/Status/Abort 的完整映射。
4. **第四层实现通用 CLI adapter**：支持 stdin/stdout、`stream-json`、resume 参数、退出码和 Ctrl-C；用于 Aider、Gemini CLI、Continue、Claude Code fallback。
5. **DSH 单独做 capability-aware adapter**：不要把 ACP、Web gateway、SDK JSON-RPC 的取消能力混写。
6. **ZCode 先只登记为待实现 backend**：除非官方发布 API/ACP，否则不做 UI/私有数据库抓取。
7. **Roo Code 标记 archived/deprecated**：不作为新功能验收目标。
8. **SWE-agent 建模为 job/trajectory**：不要强行伪装成可无限多轮的 interactive session。
9. **所有适配器保留 raw event**：统一接口返回简化文本，但 `.ref`/调试日志应能追溯原始生命周期。
10. **用终态驱动 `wait_finish()`**：不可使用固定 sleep、空闲超时或“收到一段文本”作为完成判据。

## 6. 不确定项和版本风险

### 已明确但版本敏感

- OpenCode v2 session/event API 正在演进，当前 server 文档和 v2 spec 可能不完全一致。
- DeepSeek Harness 是 developer preview，事件日志、resume、repair 和 transport API 可能发生 breaking change。
- Goose 已迁移组织且 session storage 在 1.10 发生迁移，CLI 参数和数据库实现需按版本锁定。
- Roo Code 已于 2026-05-15 归档，后续行为不应视为受支持协议。
- Codex App Server 的 v2 thread/turn API 是当前推荐方向，但具体 schema 仍随 Codex 版本演进。

### 本次无法从稳定官方公开接口核实

- Zed 原生 Zed Agent 是否提供可供任意外部程序使用的稳定 session RPC。
- ZCode Agent 的公开创建 task、发送 prompt、事件订阅和 cancel API 的确切协议。
- OpenHands 当前 SDK/Agent Server 的完整 endpoint 名称及版本化 schema。
- Aider 的 application-level cancel/status RPC；目前主要是 CLI/进程控制。
- Continue 的对外远程 session RPC；目前已核实 CLI/TUI/SDK 能力，但没有稳定公共 wire protocol。
- SWE-agent 是否会在未来版本提供交互式、可 resume 的长期 session；当前官方文档仍将 trajectory/replay 作为主要持久边界。
- Gemini CLI `stream-json` 的全部事件枚举和未来兼容保证；已核实其存在和用途，但未把每个事件字段纳入本报告。

## 7. 官方来源

### OpenCode

- https://github.com/anomalyco/opencode
- https://github.com/anomalyco/opencode/blob/dev/specs/v2/session.md
- https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/server/routes/session.ts

### Zed / ACP

- https://zed.dev/docs/ai/agents
- https://zed.dev/docs/ai/external-agents
- https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v2/overview.mdx
- https://github.com/agentclientprotocol/agent-client-protocol/tree/main/schema

### ZCode

- https://zcode.z.ai/en/docs/agents
- https://zcode.z.ai/cn/changelog

### DeepSeek Harness

- https://github.com/deepseek-ai/deepseek-harness
- https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/core.md
- https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/session.md
- https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/core/agent/README.md

### Aider / Goose / Cline / Roo Code

- https://github.com/Aider-AI/aider
- https://github.com/Aider-AI/aider/blob/main/aider/website/docs/usage/commands.md
- https://github.com/aaif-goose/goose
- https://github.com/aaif-goose/goose/blob/main/documentation/docs/guides/goose-cli-commands.md
- https://github.com/cline/cline
- https://github.com/cline/cline/blob/main/docs/core-workflows/task-management.mdx
- https://github.com/cline/cline/blob/main/sdk/ARCHITECTURE.md
- https://github.com/RooCodeInc/Roo-Code

### OpenHands / SWE-agent / Gemini CLI / Continue

- https://github.com/OpenHands/OpenHands
- https://github.com/SWE-agent/SWE-agent
- https://github.com/SWE-agent/SWE-agent/blob/main/docs/usage/trajectories.md
- https://github.com/google-gemini/gemini-cli
- https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md
- https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md
- https://github.com/continuedev/continue
- https://docs.continue.dev/cli/quickstart
- https://docs.continue.dev/cli/tui-mode

### Codex / Claude Code

- https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- https://github.com/openai/codex/blob/main/codex-rs/docs/codex_mcp_interface.md
- https://docs.anthropic.com/en/docs/claude-code/cli-usage
