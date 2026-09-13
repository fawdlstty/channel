# design.md

> 维护规则见 [AGENTS.md](AGENTS.md)。每一项内容都标注来源：
> 【用户要求】= 用户明确提出的改动/约束（含原始诉求语义）；
> 【AI 演绎】= AI 为落实用户要求而自行推导补全的细节，尚未逐项确认。
> 最近更新：2026-09-13（CI Windows job 的 Linux 平台隔离哨兵收敛为纯
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
- 【AI 演绎】crate 元数据：`channel` 0.3.5（edition 2021，rust 1.88），
  MIT，仓库 github.com/fawdlstty/channel。【用户要求，2026-09-13】
  rust-version 1.85 → 1.88：potato 0.4.1 硬依赖 time ^0.3.55 /
  jsonwebtoken ^11（MSRV 1.88），且用户明确不降 potato 版本，CI 的
  Linux MSRV 档同步升至 1.88。

## 2. 总体结构

- 【AI 演绎】单 crate 布局：
  - `src/lib.rs` 顶层（protocol/session/runtime/utils + 各 feature 模块）；
  - `src/bin/harness.rs`：桌面感知 harness 的 JSON-lines 服务 bin
    （stdout 恒为协议通道，日志进 stderr；【用户要求 F，2026-09-12】
    `[[bin]]` 表项撤销后经 cargo 默认发现，`harness` feature 门控内置于
    文件——未启用时编译出仅打印指引的占位 main）；
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
- 【AI 演绎】`runtime`：harness 可执行文件发现（PATH + 平台特化探测）
  与 cwd 解析。

### 3.2 harness 适配器（always compiled）

- 【AI 演绎】`harness::codex`：Codex app-server 适配（stdio JSONL 进程或
  本地 ws endpoint 双传输；turn 生命周期、权限审批、模型清单）。
- 【AI 演绎】`harness::acp` / `harness::cli`：ACP 代理与 CLI 型后端。

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

## 4. 构建与发布

- 【AI 演绎，2026-09-12 按用户要求 F 重组】feature 体系见 Cargo.toml：
  `default = []`；特性面收敛为 `llm`、`ccswitch`（2026-09-12 按【用户要求
  F】由 `switch` 改名，旧名不再保留）、`local-safetensors-cpu`（+
  `local-safetensors-cuda/metal`、`local-gguf-cpu/cuda/metal/vulkan`）、
  `harness`（含全部平台集成），及【用户要求 F，2026-09-12】聚合 feature
  `local-safetensors-all`/`local-gguf-all`/`all`，共 13 项；`potato`/
  `local`/`local-gguf`/`local-server`/`local-cuda`/`local-metal`/
  `local-vulkan`/`harness-linux-*` 均已撤销。
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

## 5. 与其他系统的关系

- 【AI 演绎】potato（fawdlstty/potato ≥ 0.4.1）：HTTP 传输（Session/
  HttpServer/SSE）+ WebSocket 客户端（wslite 泛型客户端 / full
  `Websocket::connect`）+ 最小 `block_on` 执行器。
- 【AI 演绎】ormer(SQLite)：cc-switch 配置库读取（`switch` feature）。
- 【AI 演绎】Codex app-server：stdio JSONL / 本地 ws endpoint 双契约
  （initialize/thread.start/turn.* / 审批 accept/decline 枚举）。
- 【AI 演绎】CC-Shift/宿主 App（Tauri）：harness bin JSON-lines 协议、
  windows crate 版本族对齐。
