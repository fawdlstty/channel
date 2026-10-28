# design.md

> 维护规则见 [AGENTS.md](AGENTS.md)。每一项内容都标注来源：
> 【用户要求】= 用户明确提出的改动/约束（含原始诉求语义）；
> 【AI 演绎】= AI 为落实用户要求而自行推导补全的细节，尚未逐项确认。
> 最近更新：2026-10-28（新增 Todo 待办一等事件，见 §3.1；
> 此前 2026-10-19：新增 §3.8 历史存储 `history` feature、ormer 依赖
> path 统一，见 §3.8/§4/§5；此前 2026-10-10：Session 附件传递、
> ContextUsage 事件、ZCode CLI
> `--mode`/`--attach`，见 §3.1/§3.2；此前 2026-09-28：ZCode Auto 后端
> ACP → StructuredCli，见 §3.2；
> 同日 atspi 依赖改为直接 git 引用 fawdlstty/atspi（用户要求，见 §3.6）；
> 此前 2026-09-13：CI Windows job 的 Linux 平台隔离哨兵收敛为纯
> `cargo tree` 反查；同日 `full` 聚合可编译性收敛：GPU 卸载后端退出聚合、
> metal 变体按 Apple 目标 gating，用户要求修复
> `cargo clippy --features full --all-targets -- -D warnings`；
> 同日 feature 体系改名 `-all` → `-full`；此前 2026-09-12 记录见 git
> 历史：potato 常驻、local-* 规范命名、harness 平台集成全量编译）。

## 1. 项目定位

- 【AI 演绎】`channel` 是一个面向 AI coding harness 的通道库：把
  Codex app-server、ACP 代理、CLI 型 harness 等后端适配成统一会话模型，
  并附带四种大模型直连 HTTP 协议客户端（可选本地推理/本地服务端）与
  桌面感知 harness（原 charness crate 并入）。库形态供宿主内嵌，服务
  形态经 `harness` bin（JSON-lines 协议）子进程拉起。
- 【AI 演绎】crate 元数据：`channel` 0.3.16（edition 2021，rust 1.90），
  MIT，仓库 github.com/fawdlstty/channel。【用户要求，2026-09-13】
  rust-version 1.85 → 1.88：potato 0.4.1 硬依赖 time ^0.3.55 /
  jsonwebtoken ^11（MSRV 1.88），且用户明确不降 potato 版本，CI 的
  Linux MSRV 档同步升至 1.88。【用户要求（既定策略延续），2026-10-28】
  rust-version 1.88 → 1.90：history 链路锁文件中 uuid 1.28.0
  （MSRV 1.89）与 ordered-float 5.5.0 / roaring 0.11.5（MSRV 1.90）
  抬高下限，按 2026-09-13 确认的「不降级依赖、随依赖树上移」策略升至
  1.90，CI Linux MSRV 档同步更名 msrv-1.90-default。

## 2. 总体结构

- 【AI 演绎】单 crate 布局：
  - `src/lib.rs` 顶层（protocol/session/runtime/utils + 各 feature 模块）；
  - `src/bin/harness.rs`：桌面感知 harness 的 JSON-lines 服务 bin
    （stdout 恒为协议通道，日志进 stderr；【用户要求 F，2026-09-12】
    `[[bin]]` 表项撤销后经 cargo 默认发现，`harness` feature 门控内置于
    文件——未启用时编译出仅打印指引的占位 main）；
  - `src/history.rs`：【用户要求，2026-10-16】AI 会话历史存储（feature
    `history`），自 BlackAgent 宿主 db.rs 整体迁入；`src/lib.rs` 按
    feature 条件导出全部公开面（句柄/行模型/压缩原语/转写辅助），见
    §3.8；
  - `examples/`（basic / llm_chat / local_chat / local_server）经 cargo
    默认发现，feature 门控同样内置于各示例文件（【用户要求 F，
    2026-09-12】`[[example]]` 表项撤销）；`tests/`（集成测试，均带
    feature gate）；
  - `docs/`（文档站源码）。
- 【AI 演绎，2026-09-12 起调整】依赖策略：重型/平台依赖全部 optional，
  按 feature 引入；必选依赖为 tokio、futures-util、serde/serde_json，
  以及常驻传输栈 potato（【用户要求 F，2026-09-12】`potato` feature
  撤销，依赖改为非 optional——见 §3.7/§4）。

## 3. 功能模块（职责 / 启用方式 / 平台）

### 3.1 核心会话层（always compiled）

- 【AI 演绎】`protocol`：SessionConfig / BackendSpec / Event / ToolCall /
  CapabilitySet 等协议契约与 wire 形状。
- 【AI 演绎】`session`：Session 状态机（BackendFuture 要求 `+ Send`），
  send/next_event/permission/interrupt/close 统一面。
- 【AI 演绎，2026-10-10】Session 公开面两项扩展：① 附件传递——
  `Session::create_with_attachments` / `send_with_attachments` 以绝对路径
  附件开启/发送回合，后端不支持时经 trait 默认实现静默丢弃、回退纯文本；
  ② 上下文占用事件——`Event`/`SessionEvent` 新增
  `ContextUsage { used, total }`，后端流携带用量投影时透出（ZCode 取自
  result 行的 `projection.contextUsed`/`projection.contextWindow`，缺字段
  或非数值静默跳过），无该数据的后端不产出该事件。
- 【用户要求，2026-10-28】Todo（待办/计划）一等事件归一化层：
  `Event::Todo(TodoSnapshot)` / `SessionEvent::Todo { snapshot }`——
  三个来源统一收敛：① StructuredCli（zcode/claude）的 TodoWrite/
  UpdatePlan 工具调用在 ToolCall 之外按容器键 `todos|plan|steps|items`
  提升快照（TodoRead 不提升）；② ACP `session/update` 的 `plan` 更新
  （原先落未知分支被过滤）；③ Codex app-server `turn/plan/updated`
  通知（`inProgress` 驼峰、explanation 解释字段，原先落 Raw 被过滤）。
  快照语义为全量整表、逐次覆盖，不做增量合并；条目文本字段回退链
  `content/step/title/text/activeForm`，status 归一化为
  pending/in_progress/completed（全量缺省时第 0 项进行中、其余待办），
  priority 仅收 high/medium/low。会话状态聚合忽略该事件（透传事实，
  与 CompactionBoundary 同款）；活动分类新增 `ActivityKind::Todo`，
  TodoWrite 活动行不再误标 WriteFile。
- 【AI 演绎】`runtime`：harness 可执行文件发现（PATH + 平台特化探测）
  与 cwd 解析。

### 3.2 harness 适配器（always compiled）

- 【AI 演绎】`harness::codex`：Codex app-server 适配（stdio JSONL 进程或
  本地 ws endpoint 双传输；turn 生命周期、权限审批、模型清单）。
- 【AI 演绎】`harness::acp` / `harness::cli`：ACP 代理与 CLI 型后端。
- 【用户要求，2026-09-28】ZCode 的 Auto 后端路由由 ACP 改为 StructuredCli
  （`src/session.rs` / `src/harness/mod.rs` / `src/harness/cli.rs`）：
  现行 zcode（桌面 3.14.3 与开源 CLI main）没有 `acp` 子命令，原路径
  spawn `zcode acp` 只会拉起 GUI 并报 JSON 解析错误；改为每回合
  `zcode -p <message> --output-format stream-json`（NDJSON 事件流，
  `model.streaming` 的 `reasoning_delta`/`text_delta` 映射思考与输出增量，
  `turn.failed`/末行 `result` 映射终态，`--resume <sessionId>` 续接）。
  模型选择无 CLI 参数，由 zcode 自身配置（`~/.zcode/v2/provider_config.json`
  的 `defaultModelSelection`）决定，channel 的 `set_model` 不传递（与
  ClaudeCode CLI 后端行为一致）。显式 `BackendSpec::Acp` 仍走 ACP 适配。
  【AI 演绎，2026-10-10】命令面扩展：每回合追加 `--mode yolo|build`——
  「完全访问」配置映射 yolo；zcode 对 `-p` 无头模式默认即 yolo，故常规
  回合显式传 `--mode build` 回到标准权限模式。附件（见 §3.1）以逐文件
  `--attach <path>` 附加在回合参数上。

### 3.3 `llm`（feature `llm`）

- 【AI 演绎】四种直连协议客户端：OpenAI Chat Completions、OpenAI
  Responses、Anthropic Messages、Ollama（HTTP 传输 = potato Session），
  附 SSE/NDJSON 服务端流发射器。
- 【用户要求 U，2026-09-12】BYOK 复用桥（原 `harness::llm_bridge`）随
  harness 瘦身迁出至 WorkRecorder 仓 `wr-harness-ext`（见 §3.6）。

### 3.4 `switch` 模块（feature `ccswitch`；2026-09-12 按【用户要求 F】由 `switch` 改名）

- 【AI 演绎】cc-switch 供应商切换：ormer(SQLite) 配置库读取 + potato
  本地上游代理（OpenAI/Anthropic 协议伪装转发）+ TOML 配置解析。
  代码模块名（`src/switch/`、`mod switch;`、`SwitchProvider`）保持不变，
  仅 feature 名改为 `ccswitch`；未启用时 `SwitchProvider` 降级为
  unsupported-capability 错误（错误消息同步指向 `ccswitch`）。
- 【用户要求，2026-10-15】cc-switch 的供应商按 `app_type` 分属各开发环境
  （codex/claude/opencode/gemini），渠道枚举与会话切换复用其中 channel
  支持的三类（gemini 无对应 harness，不枚举）：
  - Codex（`app_type=codex`，原有）：config.toml + cc-switch key 合并，
    切换经 `CODEX_HOME` 会话 home（0.3.11 起既有行为不变）；
  - ClaudeCode（`app_type=claude`）：`settings_config` 即 `{"env":{...}}`
    （ANTHROPIC_BASE_URL/AUTH_TOKEN/MODEL 等），切换为逐键注入子进程
    环境变量（claude CLI 对这组变量的响应优先于全局配置），不落盘；
  - OpenCode（`app_type=opencode`）：`settings_config` 是 opencode.json
    `provider.<id>` 切片，切换时挂到会话本地配置的固定 provider id
    `ccswitch` 下（`model` 指向所选模型）经 `OPENCODE_CONFIG` 指给
    `opencode acp`（该变量优先级高于用户全局配置，实测
    `session/new` 的 model `currentValue` 生效）。
- 【AI 演绎，2026-10-15，落实上一条】`CodexDatabase` 更名
  `CcSwitchDatabase`，`available_keys`/`resolve_provider`/`resolve_row`
  带 `app_type` 参数；`PreparedResources::env` 从路径对放宽为
  `(String, String)`；ACP 后端（`harness/acp.rs`）补上 env 注入通道。
  渠道枚举（`catalog::list_channels`）：Codex 静态 config.toml 优先、
  ClaudeCode 全部来自 cc-switch、OpenCode 静态 `opencode.json`/
  `opencode.jsonc`（逐 provider×model 条目）+ cc-switch 合并（同名团队
  静态优先）；claude 条目模型取 `env.ANTHROPIC_MODEL`（缺省 default），
  opencode 条目按 `models` 表逐模型展开。
- 【AI 演绎，2026-10-15】cc-switch 桌面端常驻时持有数据库锁，turso 无法
  直连被外部进程锁定的既有 SQLite 文件（枚举与会话切换解析同时受损）：
  直连失败回退为把 `cc-switch.db`（连同 `-wal`/`-shm`，SQLite 锁为 POSIX
  建议锁、文件字节始终可读）逐字节复制到
  `$TMPDIR/channel-ccswitch-snapshots/` 快照再打开，快照按小时惰性清理；
  读取均为轻量只读查询，可接受秒级陈旧。

### 3.5 `local-safetensors-cpu` 家族（含 `local-gguf-*`，+cuda/metal/vulkan 变体）

- 【用户要求 F，2026-09-12；同日两改定名】local 系 feature 以权重
  格式开头命名：safetensors 系为 `local-safetensors-<底层实现>`
  （如 `local-safetensors-cpu`；safetensors 引擎唯一为 candle，名称不再
  拼写引擎段——初版 `local-safetensors-candle-*` 已按用户要求废弃），
  gguf 系保持
  `local-gguf-<底层实现>`（如 `local-gguf-vulkan`，名称已隐含 llama.cpp
  引擎）；不再有 `local`/`local-gguf`/`local-server`/`local-cuda`/
  `local-candle-*`/`local-safetensors-candle-*` 等旧名。
- 【AI 演绎】权重格式决定 feature 前缀并隐含引擎：safetensors 系（candle
  引擎，纯 Rust）/ gguf 系（GGUF 权重 + llama.cpp 引擎）；底层实现按引擎
  分家——safetensors 系仅 cpu/cuda/metal（candle 无 vulkan 后端，上游仅为
  未实现的 feature request），gguf 系为 cpu/cuda/metal/vulkan。
  `local-safetensors-cpu` 为基座（candle + tokenizers + minijinja 共享栈），
  gguf 系隐含之。
- 【用户要求 F，2026-09-12】`local-server` 撤销为独立 feature：本地模型
  HTTP 服务（`LocalLlmServer`，OpenAI/Responses/Anthropic/Ollama 兼容
  端点）随 `local-safetensors-cpu` 直接提供。
- 【AI 演绎】本地推理：candle 与 llama-cpp-2 双引擎、聊天模板渲染
  （minijinja）；`local-gguf-cuda`/`local-gguf-metal`/`local-gguf-vulkan`
  仅切换 llama.cpp 的 GPU 卸载后端，`local-safetensors-cuda`/`local-safetensors-metal`
  仅切换 candle 的设备。
- 【用户要求 F，2026-09-12】两份 README（EN/中文）在本地模型章节写明
  两引擎支持的模型架构：safetensors（candle）为固定白名单
  `llama`/`qwen2`/`qwen3`/`phi3`/`gemma`（candle-transformers 0.11，
  其他架构报错并提示改用 GGUF）；GGUF（llama.cpp）无自有白名单，
  覆盖内置 llama.cpp（llama-cpp-2 0.1.156）支持的全部架构（约 140 种：
  llama/llama4、qwen2/qwen3 含 MoE/VL 变体、gemma 系、phi2/phi3、
  deepseek 系、GLM 系、mistral3/mistral4、gpt-oss 等）。
- 【用户要求 F，2026-09-12；2026-09-13 用户要求改名为 full】聚合
  feature：`local-safetensors-full` 与 `local-gguf-full` 启用各权重
  格式的后端集，`full` 启用本 crate 的 llm、ccswitch、两个 `-full`
  聚合与 harness。
- 【用户要求，2026-09-13】`cargo clippy --features full --all-targets
  -- -D warnings` 须能在无 GPU SDK 的开发机（Windows）上执行成功
  （本次修复请求的原始诉求）。
- 【AI 演绎，2026-09-13，落实上一条】聚合 feature 的边界收敛为
  「**无外部 GPU SDK 且无目标平台硬限制即可编译**的后端全集」：
  `local-safetensors-full = cpu`、`local-gguf-full = cpu + metal`；
  cuda（candle-kernels 构建期需要 nvcc，llama.cpp 构建期需要 CUDA
  Toolkit）与 vulkan（Windows 构建期强制 `VULKAN_SDK`）有编译期 SDK
  硬依赖，保持独立 feature 供具备 SDK 的环境显式启用。candle 的
  metal 变体仅 Apple 目标可编译（candle 上游把 objc2 系 metal 依赖
  声明为无平台 gating 的普通 optional 依赖，非 Apple 目标启用直接
  compile_error；cargo 的 resolve 不按目标段过滤，同包同名依赖亦无法
  双名并存做平台化 feature），故不进入任何聚合；llama 侧 metal 为
  无副作用标记 feature（GGML_METAL 由 CMake 在 Apple 目标自动开启），
  随 `local-gguf-full` 保留。Cargo.toml 内有同语义注释。

### 3.6 桌面感知 harness（feature `harness`；v1.15 瘦身，用户要求 U）

- 【用户要求 U，2026-09-12】channel harness 收敛为**通用底座**，只保留
  基本感知/操作能力、识图（截图）能力与自进化（关联记忆）能力：
  `snapshot`（UIA/AT-SPI 采集 + `capture_screen` 截图 + 合成树）、
  `graph`（控件↔窗口↔进程关联图）、`tools`/`actuate`（只读工具面 +
  操作五原语，`--actuation` 授权门）、`serve`（JSON-lines op 隔离执行）、
  `store`（契约 + 脱敏钩子）。
- 【用户要求 U，2026-09-12】迁出至 WorkRecorder 仓 `wr-harness-ext`
  crate（E:\Backup\Backup\08_start_up\35_workrecorder\harness_ext）：
  `record`（RecordEvent/合帧器/R1 事件直录生产者）、`browser`（CDP 调试
  浏览器直录七 op）、`llm_bridge`（BYOK LLM 桥 + `relocate_control`）；
  `harness-browser` feature 撤销，`harness` 不再含 potato；bin 移除
  `--llm-*` 参数与 ui/browser 事件装配；serve 白名单收窄为
  `focus`/`scene`；error 枚举移除 browser/llm/record 十个变体（wire 文案
  与机器码由 wr-harness-ext 的自有错误枚举逐字节承接）。golden 契约的
  `recordEvent*`/`relocation` 段 serde 断言随迁（fixture 本体仍双轨镜像）。
- 【用户要求 F，2026-09-12】`harness-linux-*` 子 feature（x11/atspi/
  portal/uinput）全部撤销：启用 `harness` 时各平台集成（Windows UIA/
  SendInput/GDI+WIC，Linux X11/AT-SPI/portal/uinput）**全量编译进**，
  实际走哪条路由目标平台（`cfg(target_os)`）+ 运行环境探测
  （`DISPLAY`/`WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`/`/dev/uinput`、
  `HARNESS_ALLOW_UINPUT` 双闸）在运行时决定，不做编译期裁剪。
- 【AI 演绎，已被上一条取代后收敛】Linux 侧依赖（x11rb+png、atspi+
  futures-lite、ashpd）保持 optional，统一由 `harness` feature 以
  `dep:` 引用启用（非 Linux 目标下这些引用为空操作）。
- 【用户要求，2026-09-28】`atspi` 依赖改为直接 git 引用
  `https://github.com/fawdlstty/atspi`（不再走 registry 版本号）：
  atspi 0.31 未发布 registry（crates.io 止步 0.30.0），而 WorkRecorder
  消费仓的 third_party/atspi 补丁基线即 0.31.0/atspi-common 0.15.0；
  registry `^0.30` 会解析出旧版，依赖图中并存两套互不兼容的
  atspi-common 类型。
- 【AI 演绎，2026-09-28，落实上一条】feature 面维持
  `proxies`/`connection`/`zbus` 不变（0.31 的 features 表未显式声明
  `zbus`，但 zbus 为 optional 依赖且无 `dep:` 引用，隐式 feature 仍在，
  `atspi::zbus` 类型 re-export 路径不变，zbus 5.5）；
  `role_to_control_type` 补 `Role::Switch → Button` 映射（atspi-common
  0.15 新增变体，穷尽 match 编译期强制补表）。
- 【用户要求，2026-10-11】恢复 `cargo publish` 发布路径（0.3.10 发布被
  "dependency `atspi` does not specify a version" 阻断）。【AI 演绎，
  落实上一条，保留 git 引用前提】atspi 依赖补版本要求
  `>=0.30, <0.32`：publish 时 git 段被剥除，crates.io 下游按范围解析到
  上游 0.30.0；本地 git fork（0.31.0）与 WorkRecorder third_party 补丁
  （0.31.0）同样命中该范围，依赖图仍统一在单一 atspi-common 版本。
  【AI 演绎，落实上一条】`role_to_control_type` 撤下 `Role::Switch →
  Button` 分支（registry 0.30/atspi-common 0.14 无此变体，下游编译不过），
  表尾改 `_ => "Custom"` 兜底并加 `#[allow(unreachable_patterns)]`
  （0.30 全覆盖 match 下 `_` 必触发 unreachable 警告，CI `-D warnings`
  会挂；fork 本地构建 Switch 经兜底落 Custom）。上游合并 PR #384 并
  发布 0.31 后：版本要求收紧为 `0.31`，删除 `_` 与 allow、恢复
  `Role::Switch => "Button"` 分支。

### 3.7 ws 传输层（2026-09-12 重构，本次变更）

- 【用户要求】移除 `tungstenite` 与 `tokio-tungstenite` 依赖，WebSocket
  改为直接使用 potato 库已有功能实现。
- 【AI 演绎】两条 ws 路径与 potato API 的对应：
  - **CDP 泵线程**（同步模型，要求 50ms 空转 tick + 帧内不丢半帧）：
    用 `potato::wslite` 泛型客户端（`WebsocketIo` trait + `Websocket<S,R>`
    + `client_handshake_with_rng` + `parse_ws_url`），`std::net::TcpStream`
    经带读缓冲的 `SyncTcp` 适配器实现 `WebsocketIo`（帧读到一半遇读超时
    在适配器内消化，缓冲见底才放行 WouldBlock 驱动合帧 tick），
    `potato::block_on` 驱动异步 API（future 全部即时完成，不引入 async
    runtime）；Close/超时错误按 potato 固定错误文案（自家库约定）判定。
  - **Codex WebSocket endpoint**（异步串行模型）：用 `potato::Websocket::
    connect`（HTTP 栈升级握手）+ `send_text`/`recv`/`send_close`；对端
    Close 按 potato 固定文案 `"close frame"` 归一为会话结束。
- 【AI 演绎】potato 侧配套（fawdlstty/potato 0.4.1）：
  - lib.rs 在 std 构建下以独立命名空间 `wslite` 导出既有 lite 泛型 ws
    实现（规避与 full 版 `Websocket`/`WsFrame` 同名冲突），并导出
    `block_on`；
  - full 版 `Websocket` 补 `send_close()` 公开方法；
  - 版本 bump 0.4.0 → 0.4.1（发布后 channel 移除 `[patch.crates-io]`）。
- 【用户要求 F，2026-09-12】`potato` feature 撤销：potato（0.4.1）改为
  非 optional 常驻依赖，`cfg(feature = "potato")` 门控与无 potato 降级
  分支删除——HTTP/ws 传输（四种协议客户端、cc-switch 本地代理、本地
  模型 HTTP 服务、Codex ws endpoint）一律可用；此前「显式 potato
  feature + 按 feature 启用」的形态废止。
- 【AI 演绎】掩码随机源 `utils::websocket::ws_mask_rng`（std RandomState
  的 SipHash 混合，不引入 rand）。
- 【AI 演绎】回归：`tests/ws_smoke.rs` 双路径回环冒烟（wslite 同步客户端
  + full `Websocket::connect`，手写最小 ws echo 服务端）。

### 3.8 历史存储（feature `history`；2026-10-16 自 BlackAgent db.rs 迁入）

- 【用户要求，2026-10-16】AI 会话历史持久化自 BlackAgent 宿主 db.rs
  整体迁入 channel（迁移本体的接口细节按源码为准，权威 =
  `src/history.rs` + `tests/history*.rs` 四件套）；存储为宿主自有
  SQLite 库（ormer/turso 后端，纯 Rust，默认构建无 C 工具链要求），
  格式与 ZCode session-store 兼容。
- 【AI 演绎，落实上一条】职责与公开面（经 `src/lib.rs`
  `#[cfg(feature = "history")] pub use` 导出）：
  - 句柄 `ChatHistoryDatabase`：`open()`（兜底路径 HOME/USERPROFILE +
    `chat_histories.db`，不走环境变量覆盖——存储位置由宿主经
    `open_at(path)` 显式指定，2026-10-16 拍板）与六域方法（settings /
    projects / sessions / channel-selection / messages / compaction）；
  - 行模型（存储格式契约，供宿主做遗留数据导入等高级流程）：
    `ProjectRow` / `SessionRow` / `MessageRow` / `PartRow` /
    `SessionEntryRow` / `LocalSettingRow`；宿主投影类型
    `ProjectRecord` / `SessionRecord` / `MessageRecord`（serde
    camelCase，宿主自行转 DTO）；
  - data 列合成函数（ZCode 键名照抄：`sessionID`/`modelSelection` 及
    遗留 `model.providerID`/`modelID` 等）：`user_message_data` /
    `assistant_message_data` / `part_data` / `channel_selection_data`；
  - ID/工具：`new_session_id`（`sess_` + uuid v4）、`new_event_id`
    （`msg_`/`part_` + 毫秒时间戳 base36 + uuid v4）、`slugify`。
- 【AI 演绎】ZCode session-store 格式兼容契约：六表
  `session`/`message`/`part`/`session_entry`/`local_setting` + channel
  扩展表 `project`；ZCode 有而 channel 不写的列照建并保持 NULL（同一
  库文件可继续被 ZCode 打开）；时间戳统一 epoch 毫秒（行内 `i64`、
  记录层 `u64`）；ID 形状与 ZCode 一致。
- 【用户要求，2026-10-16 制定；2026-10-19 修订第①条】turso 红线
  （原文以注释存于 src/history.rs，宿主侧必须遵守）：① 数据库访问
  一律经 ormer——自有库（`chat_histories.db` 由本模块创建/打开）直连
  读写；外部标准 SQLite（如 ZCode 引擎库）同样经 ormer 只读访问
  （单次调用内短连接打开、仅发 SELECT、用完即弃，不写不迁移；原
  "外部库走独立只读连接"的约束由用户要求取消）；② `journal_mode`
  是查询型
  PRAGMA（execute 遇结果行报 unexpected row 属误报、副作用已生效），
  `open_at` 内已按查询通道处理，宿主不得再对句柄库直发 PRAGMA；
  ③ 模型不声明 `#[foreign]`（SQLite 不能 ALTER ADD CONSTRAINT，不声明
  则保留存量外键），级联删除走显式先子后父事务；④ 单连接并发
  forbidden——句柄内部 tokio Mutex 串行化，唯一豁口 `lock()`（tokio
  Mutex 不可重入，持有守卫期间再经 `&self` 调任何句柄方法即死锁）。
- 【AI 演绎】压缩存储原语（自建压缩与外部转写共用）：
  `CompactionBoundary`（serde camelCase 键组与 ZCode zod schema
  一一对应，未建模键经 flatten 兜底无损往返）、`CompactionSummaryParams`
  + `insert_compaction_summary`（单事务写 summary message + text part +
  compaction part，同 `boundary_id` 幂等跳过）、
  `record_external_compaction_marker`（无 boundary 的水位观察标记，
  读侧不认、永不参与切片）、`latest_compaction_boundary`（宿主自压
  选材锚点）、`get_context_messages`（切片还原：最后一个 active
  boundary 起返回，preservedSegment 区间插回 summary 之后；无 boundary
  等价全量投影 `get_messages`）。
- 【AI 演绎，2026-10-19 按用户要求改经 ormer】外部转写辅助（ormer
  短连接只读，仅 SELECT，连接用完即弃）：
  `zcode_session_db_path()`（`~/.zcode/cli/db/db.sqlite`，环境变量
  `CHANNEL_ZCODE_SESSION_DB` 覆盖——读外部库的场景与"自有库路径不走
  环境变量"的拍板不冲突，后者只约束 `chat_histories.db`）与
  `read_zcode_compaction_summary_text(session_id, message_id)`
  （优先 message.data 的 `summary.body`，缺失
  回落 text part 并剥除 ZCode 续接前导；库不存在/查无此行返回
  `Ok(None)`，打开/查询失败返回 `Err` 供宿主降级占位转写）。
- 【用户要求，2026-10-16】CompactionBoundary 一等事件感知通道：
  `Event::CompactionBoundary { payload }` /
  `SessionEvent::CompactionBoundary { payload }`——payload 为 ZCode
  `CompactBoundaryPayload` JSON 原样透传，channel 故意不维护引擎事件面
  的 typed copy（仍在演化；转写层再解析为 `channel::CompactionBoundary`）；
  会话状态聚合忽略该事件（透传事实，不改状态机），由宿主在事件消费侧
  处理转写。
- 【AI 演绎】回归：`tests/history.rs`（CRUD/投影/级联/标题/存档）、
  `tests/history_compaction.rs`（自压落库键名快照、幂等、切片语义、
  观察标记、外部转写真实 payload fixture、ormer 建库的引擎库只读
  fixture）、
  `tests/history_concurrency.rs`（句柄串行化并发炮轰 + 病灶复现
  ignored 用例）、`tests/history_pragma.rs`（PRAGMA 执行/查询通道 +
  重复打开幂等）；全部 `:memory:`/临时文件，不碰真实 `~/chat_histories.db`
  与 `~/.zcode`。

## 4. 构建与发布

- 【AI 演绎，2026-09-12 按用户要求 F 重组；2026-09-13 聚合改名 `-full`
  （见 §3.5，本条旧 `-all` 名顺带更正）；2026-10-16 新增 `history`】
  feature 体系见 Cargo.toml：`default = []`；特性面为 `llm`、
  `ccswitch`（2026-09-12 按【用户要求 F】由 `switch` 改名，旧名不再
  保留）、`history`（AI 会话历史存储，见 §3.8；与 `ccswitch` 相互
  独立——只要历史存储的宿主不拉 TOML，反之亦然）、
  `local-safetensors-cpu`（+ `local-safetensors-cuda/metal`、
  `local-gguf-cpu/cuda/metal/vulkan`）、`harness`（含全部平台集成），
  及【用户要求 F，2026-09-12】聚合 feature
  `local-safetensors-full`/`local-gguf-full`/`full`，共 14 项；
  `potato`/`local`/`local-gguf`/`local-server`/`local-cuda`/
  `local-metal`/`local-vulkan`/`harness-linux-*` 均已撤销。
- 【用户要求，2026-10-19，迁移拍板纠偏；AI 演绎落实】ormer 依赖
  （`ccswitch` 与 `history` 两 feature 共用）由 registry `0.2` 改为
  path 指向本地 checkout `/home/fawdlstty/workspace/github/_/ormer/ormer`
  （0.2.23，主依赖 optional + default-features = false + sqlite，dev
  依赖同源），与消费仓 BlackAgent 的 path 依赖统一为同一 crate——
  双版本并存时 `ormer::Database` 与行模型类型不互通；Cargo.toml 注明
  发布 crates.io 时需切回 registry 版本。0.2.22 → 0.2.23 无公开 API
  变化（sqlite 后端内部并发排队），channel 调用点零适配。
  【AI 演绎】聚合 feature `full` 同步扩为 `["llm", "ccswitch",
  "history", "local-safetensors-full", "local-gguf-full", "harness"]`。
- 【AI 演绎】常用命令：`cargo check --features harness`、
  `cargo test --features harness`、【用户要求，2026-09-13】
  `cargo clippy --features full --all-targets -- -D warnings`（全量
  lint 门禁，Cargo.toml 尾注）、
  `cargo publish --allow-dirty --registry crates-io`（Cargo.toml 尾注）；
  文档站 `cd docs && npm run docs:build`。
- 【AI 演绎，2026-09-13】本地构建工具链要求：`local-gguf-*` 系需要
  cmake + C++ 工具链，bindgen 需 libclang（本机约定
  `LIBCLANG_PATH=D:\Software\Program\LLVM\bin`）；`local-*-cuda` 需
  CUDA Toolkit（candle-kernels 构建期跑 nvcc），`local-gguf-vulkan`
  需 Vulkan SDK（Windows 构建期强制 `VULKAN_SDK` 环境变量）。
- 【AI 演绎】CI：`.github/workflows/rust.yml`（多平台矩阵，具体策略以
  workflow 文件为准）。【用户要求，2026-09-13】修复 Windows job 的 CI
  编译报错（cc-rs 找不到 `x86_64-linux-gnu-gcc`）；【AI 演绎，
  2026-09-13，落实上一条】平台隔离哨兵收敛为纯 `cargo tree -i` 反查，
  不再做 x86_64-unknown-linux-gnu 交叉 cargo check：常驻 potato TLS 栈
  经 rustls 引入 ring，其 build script 经 cc-rs 编译 C 代码（cargo check
  也会执行 build script），Windows runner 上无 Linux 交叉 C 工具链必然
  失败；Linux 依赖图的编译正确性由 linux job 的 harness/extended 档
  原生编译覆盖。
- 【AI 演绎】发布顺序约束（2026-09-12 起）：先 `cargo publish` potato
  0.4.1，channel 的 `[patch.crates-io]`（本地桥接）随后移除。
- 【用户要求，2026-10-11，2026-09-28 条目已解决】`cargo publish` 曾被
  atspi git 依赖阻断（crates.io 不接受无版本号的 git 依赖），已按
  3.6 节 2026-10-11 条目的版本范围方案恢复；待 fawdlstty/atspi 0.31 系
  发布到 registry 后可收紧为版本号引用。

## 5. 与其他系统的关系

- 【AI 演绎，落实用户要求 git 引用】atspi（fawdlstty/atspi，odilia-app/
  atspi 的 0.31 基线 fork，zbus 5.5）：Linux AT-SPI2 控件树感知的协议
  实现，channel 经 git 依赖 + 版本范围 `>=0.30, <0.32` 引用——git 构建
  （channel 本地、WorkRecorder 补丁仓）落在 fork 0.31.0，crates.io 发布
  形态剥除 git 段落解析到上游 0.30.0（2026-10-11 起）；WorkRecorder 仓的
  third_party/atspi 补丁与其同源同版本（0.31.0/0.15.0），保证两侧
  atspi-common 类型一致。
- 【AI 演绎】potato（fawdlstty/potato ≥ 0.4.1）：HTTP 传输（Session/
  HttpServer/SSE）+ WebSocket 客户端（wslite 泛型客户端 / full
  `Websocket::connect`）+ 最小 `block_on` 执行器。
- 【AI 演绎，2026-10-19 更新】ormer（SQLite ORM，turso 后端）：
  cc-switch 配置库读取（`ccswitch` feature）与 AI 会话历史存储
  （`history` feature）共用同一依赖。【用户要求，2026-10-19】两处均
  为 path 依赖本地 checkout（0.2.23），与 BlackAgent 统一单版本。
- 【用户要求，2026-10-19】依赖收口：项目内数据库访问一律经 ormer，
  独立 SQLite 驱动依赖已移除（`history` feature 只剩 `dep:ormer` +
  `dep:uuid`）；外部标准 SQLite 只读访问同样走 ormer 短连接，见
  §3.8 turso 红线第①条修订。
- 【AI 演绎】Codex app-server：stdio JSONL / 本地 ws endpoint 双契约
  （initialize/thread.start/turn.* / 审批 accept/decline 枚举）。
- 【AI 演绎】CC-Shift/宿主 App（Tauri）：harness bin JSON-lines 协议、
  windows crate 版本族对齐。
