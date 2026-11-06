//! ZCode app-server backend adapter (`zcode app-server`): resident two-way
//! sessions over NDJSON with first-class user-input (AskUserQuestion) and
//! permission support. Structurally mirrors the Codex app-server adapter.
//!
//! # Wire contract (2026-10-30, verified against the local zcode.cjs 0.17.1
//! bundle via live traffic capture + static analysis)
//!
//! - codex-cli-style NDJSON JSON-RPC over stdio, but **without** the
//!   `jsonrpc` envelope field (sending one is rejected with -32600).
//!   Requests are `{id, method, params?}` (numeric id), notifications
//!   `{method, params?}`, responses `{id, result}` / `{id, error}`.
//! - No handshake greeting: the server pushes a `startup/storageState`
//!   notification right after boot (ignored); requests sent before the
//!   server is ready are queued server-side.
//! - Reverse requests (server→client) carry string ids like `"server-N"` and
//!   are re-announced periodically with NEW ids (1s exponential backoff);
//!   answering once by any (latest) id settles the request.
//! - Error codes: -32010 send rejected while a turn is running (→
//!   [`Error::Busy`]), -32020/-32021/-32022 reverse-request anomalies,
//!   -32601 unknown method, -32602 invalid params, -32700 JSON parse error.
//! - Unverified points are marked inline (tool name field, multi-select
//!   array answer shape, model string split convention).

use crate::process::JsonlProcess;
use crate::protocol::{
    Error, Event, Finish, Status, SubagentRunning, SubagentsSnapshot, ToolCall, ToolStatus,
    UserInputRequest, UserQuestion, UserQuestionOption,
};
use crate::runtime::HarnessDiscovery;
use crate::session::{Backend, BackendFuture, PermissionResponse, SendMode};
use crate::utils::json::JsonValueExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Duration;

const COMMAND: &str = "zcode";
const ARGS: &[&str] = &["app-server"];

/// send 被拒（回合运行中）的 wire 错误码，映射 [`Error::Busy`]。
const BUSY_ERROR_CODE: i64 = -32010;

/// Resolves the spawn command/args for the app-server transport: the explicit
/// [`crate::protocol::BackendSpec::ZcodeAppServer`] spec wins, `Auto` falls
/// back to the stock CLI invocation. The `app-server` subcommand is mandatory
/// for this wire protocol, so explicit specs that only carry an interpreter /
/// script path get it appended (multi-segment launches like `node <zcode.cjs>`
/// stay intact).
fn command_and_args(backend: &crate::protocol::BackendSpec) -> Result<(String, Vec<String>), Error> {
    let (command, args) = match backend {
        crate::protocol::BackendSpec::Auto => (
            COMMAND.to_owned(),
            ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
        ),
        crate::protocol::BackendSpec::ZcodeAppServer { command, args } => {
            (command.clone(), args.clone())
        }
        _ => {
            return Err(Error::InvalidConfig(
                "Zcode requires Auto or ZcodeAppServer backend".to_owned(),
            ));
        }
    };
    let mut args = args;
    if !args.iter().any(|arg| arg == "app-server") {
        args.push("app-server".to_owned());
    }
    Ok((command, args))
}

/// 未应答用户提问的簿记条目。键为 wire `requestId`（宿主应答句柄即它，
/// reannounce 稳定），值为最新 wire id（服务端以新 id 周期性重发反向请求，
/// 应答与关闭冲刷一律写最新 id）+ 多选题干集合（应答序列化需要区分
/// 字符串/数组形态）。
struct PendingUserInput {
    wire_id: Value,
    multi_select: BTreeSet<String>,
}

pub(crate) struct ZcodeBackend {
    transport: Box<JsonlProcess>,
    session_id: String,
    next_request_id: u64,
    queued: VecDeque<Event>,
    /// 未应答审批：requestId → 最新 wire id。
    pending_permissions: HashMap<String, Value>,
    /// 未应答用户提问：requestId → 最新 wire id + 多选题干集合。
    pending_user_inputs: HashMap<String, PendingUserInput>,
    /// 在途逻辑请求的 number id → 方法名（单飞诊断簿记，与 codex 同构）。
    pending_responses: HashMap<u64, String>,
    kill_grace_period: Duration,
    /// 渠道切换资源（cc-switch）：ZCode 经 ZCODE_PERSONAL_PROVIDER_CONFIG_FILE
    /// 指向按渠道改写过的 provider 配置副本；持有它保证会话期间副本不被丢弃。
    #[allow(dead_code)]
    resources: Option<crate::switch::PreparedResources>,
}

/// server→client 反向请求的分类。审批判定只认 `interaction/requestPermission`
/// 精确匹配；`interaction/requestUserInput` 拆为一等用户提问流，不混入审批。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReverseRequest {
    UserInput,
    Permission,
    RuntimePreferences,
    McpAuthHeaders,
    Unknown,
}

impl ReverseRequest {
    fn from_method(method: &str) -> Self {
        match method {
            "interaction/requestUserInput" => Self::UserInput,
            "interaction/requestPermission" => Self::Permission,
            "session/requestRuntimePreferences" => Self::RuntimePreferences,
            "interaction/requestOfficialMcpAuthHeaders" => Self::McpAuthHeaders,
            _ => Self::Unknown,
        }
    }
}

impl ZcodeBackend {
    pub(crate) async fn connect_with_config(
        config: &crate::protocol::SessionConfig,
    ) -> Result<Self, Error> {
        let (command, args) = command_and_args(&config.backend)?;
        let command = config
            .runtime
            .process
            .executable
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(command);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // 渠道切换资源（cc-switch）：与 CLI 后端同款，按渠道改写 provider
        // 配置副本并把 env 注入子进程；资源句柄由 backend 持有保活。
        let resources = crate::protocol::SwitchProvider::CcSwitch
            .prepare_resources(config)
            .await?;
        let resource_env = resources.as_ref().map(crate::switch::PreparedResources::env);
        let transport = JsonlProcess::spawn_with_cwd_and_env(
            &command,
            &arg_refs,
            Some(config.workspace_cwd()),
            resource_env.as_deref().unwrap_or(&[]),
        )
        .await?;
        let mut backend = Self {
            transport: Box::new(transport),
            session_id: String::new(),
            next_request_id: 1,
            queued: VecDeque::new(),
            pending_permissions: HashMap::new(),
            pending_user_inputs: HashMap::new(),
            pending_responses: HashMap::new(),
            kill_grace_period: config.runtime.process.kill_grace_period,
            resources,
        };

        // 生命周期入口：既有 provider 会话 id → session/resume，否则
        // session/create。create 过程中服务端必发
        // session/requestRuntimePreferences 反向请求（15s 超时），由 request
        // 读循环内的反向处理自动应答。
        let resume_id = config.conversation_session_id()?;
        let (method, params) = match resume_id {
            Some(id) => ("session/resume", json!({ "sessionId": id })),
            None => ("session/create", session_create_params(config)),
        };
        let result = backend.request(method, params).await?;
        backend.session_id = result
            .pointer("/session/sessionId")
            .and_then(Value::as_str)
            .or_else(|| result.get("sessionId").and_then(Value::as_str))
            .ok_or_else(|| Error::Backend(format!("{method} returned no session id")))?
            .to_owned();

        // 订阅后事件以 session/event 通知单条一帧推送。
        backend
            .request(
                "session/subscribe",
                json!({
                    "sessionId": backend.session_id,
                    "deliveryKind": "desktop-continuous",
                }),
            )
            .await?;

        // channel 模型串约定 "provider/model"，按首个 '/' 拆分（静态推断，
        // 见 `model_reference`）；setModel 失败按创建失败上报，避免静默跑错
        // 模型。
        if let Some(model) = config.model.requested.as_deref() {
            backend
                .request(
                    "session/setModel",
                    json!({
                        "sessionId": backend.session_id,
                        "model": model_reference(model),
                    }),
                )
                .await?;
        }
        Ok(backend)
    }

    /// 单飞逻辑请求：写 `{id, method, params}`（无 jsonrpc 字段——带上即
    /// -32600），读循环内处理到达的通知/反向请求直至本请求的响应返回。
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.pending_responses.insert(id, method.to_owned());
        self.transport
            .write(&json!({ "id": id, "method": method, "params": params }))
            .await?;

        loop {
            let message = self.transport.read().await?.ok_or_else(|| {
                Error::Backend("Zcode app server ended before completing the request".to_owned())
            })?;

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queued.extend(ZcodeNotification(&message).events());
                }
                continue;
            }

            if message.get("id") == Some(&json!(id)) {
                self.pending_responses.remove(&id);
                if let Some(error) = message.get("error") {
                    if error.get("code").and_then(Value::as_i64) == Some(BUSY_ERROR_CODE) {
                        return Err(Error::Busy);
                    }
                    return Err(Error::Backend(format!(
                        "Zcode RPC {method} failed: {error}"
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            return Err(Error::ProtocolError(format!(
                "Zcode returned an unexpected response while waiting for {method}: {message}"
            )));
        }
    }

    /// server→client 反向请求（id 形如 "server-N"，同逻辑请求以新 id 周期性
    /// 重发——reannounce，1s 起指数退避；簿记按 requestId 键，重发只刷新
    /// 最新 wire id，不重复向宿主发事件）。
    async fn handle_server_request(&mut self, message: &Value, method: &str) -> Result<(), Error> {
        let id = message
            .get("id")
            .cloned()
            .ok_or_else(|| Error::Backend("Zcode server request has no id".to_owned()))?;
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match ReverseRequest::from_method(method) {
            ReverseRequest::UserInput => {
                // 用户提问归一化为一等事件；解析不出问题列表时回落通用拒绝
                // 路径（服务端会停止 reannounce 该请求）。
                if let Some(request) = self.track_user_input(&params, id.clone()) {
                    self.queued.push_back(Event::UserInputRequired(request));
                    return Ok(());
                }
            }
            ReverseRequest::Permission => {
                let (request_id, detail) = permission_required(&params, &id);
                // reannounce 只刷新最新 wire id（簿记键 requestId），不重复
                // 向宿主发事件。
                if register_pending(&mut self.pending_permissions, request_id.clone(), id.clone())
                {
                    self.queued.push_back(Event::PermissionRequired {
                        id: request_id,
                        detail,
                    });
                }
                return Ok(());
            }
            ReverseRequest::RuntimePreferences => {
                return self
                    .transport
                    .write(&json!({ "id": id, "result": runtime_preferences_result() }))
                    .await;
            }
            ReverseRequest::McpAuthHeaders => {
                return self
                    .transport
                    .write(&json!({ "id": id, "result": mcp_auth_headers_result() }))
                    .await;
            }
            ReverseRequest::Unknown => {}
        }
        self.queued.push_back(Event::Raw(message.clone()));
        self.transport
            .write(&json!({
                "id": id,
                "error": { "code": -32601, "message": "request is not supported by channel" },
            }))
            .await
    }

    /// 登记用户提问并归一化：按 requestId 去重（reannounce 只刷新最新
    /// wire id 并返回 `None`，不重复发事件），首次登记返回归一化请求（
    /// `Some`，由调用方入队 [`Event::UserInputRequired`]）。缺 requestId 或
    /// 解析不出问题列表返回 `None`（回落 Raw + -32601 拒绝路径，不登记簿记）。
    fn track_user_input(&mut self, params: &Value, wire_id: Value) -> Option<UserInputRequest> {
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_owned)?;
        let request = user_input_request(params, &request_id)?;
        let multi_select = request
            .questions
            .iter()
            .filter(|question| question.multi_select)
            .map(|question| question.id.clone())
            .collect();
        let duplicate = register_pending(
            &mut self.pending_user_inputs,
            request_id,
            PendingUserInput {
                wire_id,
                multi_select,
            },
        );
        (!duplicate).then_some(request)
    }

    async fn respond_permission(
        &mut self,
        id: &str,
        response: PermissionResponse,
    ) -> Result<(), Error> {
        let wire_id = self
            .pending_permissions
            .remove(id)
            .ok_or_else(|| Error::Backend(format!("unknown permission request id: {id}")))?;
        self.transport
            .write(&json!({ "id": wire_id, "result": permission_result(response) }))
            .await
    }

    async fn respond_user_input(
        &mut self,
        id: &str,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<(), Error> {
        let pending = self
            .pending_user_inputs
            .remove(id)
            .ok_or_else(|| Error::Backend(format!("unknown user input request id: {id}")))?;
        self.transport
            .write(&json!({
                "id": pending.wire_id,
                "result": user_input_response(&answers, &pending.multi_select),
            }))
            .await
    }

    /// 关闭冲刷：未应答审批逐个 deny、未应答提问逐个 decline（zcode 有明确
    /// decline 语义，不再借用 codex 的空答案冲刷先例）。wire id 用簿记里存
    /// 的最新 id（reannounce 可能已刷新）。
    async fn flush_pending(&mut self) {
        let permissions: Vec<Value> = self.pending_permissions.values().cloned().collect();
        self.pending_permissions.clear();
        for wire_id in permissions {
            let _ = self
                .transport
                .write(&json!({
                    "id": wire_id,
                    "result": permission_result(PermissionResponse::Deny),
                }))
                .await;
        }
        let inputs: Vec<Value> = self
            .pending_user_inputs
            .values()
            .map(|pending| pending.wire_id.clone())
            .collect();
        self.pending_user_inputs.clear();
        for wire_id in inputs {
            let _ = self
                .transport
                .write(&json!({ "id": wire_id, "result": { "action": "decline" } }))
                .await;
        }
    }

    async fn next(&mut self) -> Result<Option<Event>, Error> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let message = match self.transport.read().await? {
                Some(message) => message,
                None => return Ok(None),
            };
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if message.get("id").is_some() {
                    self.handle_server_request(&message, method).await?;
                } else {
                    self.queued.extend(ZcodeNotification(&message).events());
                }
                if let Some(event) = self.queued.pop_front() {
                    return Ok(Some(event));
                }
            }
        }
    }
}

impl Backend for ZcodeBackend {
    fn id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }

    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            // 异步回合：accepted 即返回，回合结果走 session/event
            // （turn.completed / turn.failed）。回合运行中再发 → -32010 →
            // [`Error::Busy`]。
            self.request(
                "session/send",
                json!({ "sessionId": self.session_id, "content": message }),
            )
            .await?;
            Ok(())
        })
    }

    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
        Box::pin(async move { self.next().await })
    }

    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            // session/stop 是唯一中断方法；回合随后以 turn.completed
            // (resultType=cancelled) 收场，经事件流归一为 Interrupted。
            self.request("session/stop", json!({ "sessionId": self.session_id }))
                .await?;
            Ok(())
        })
    }

    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.flush_pending().await;
            let _ = self
                .request("session/close", json!({ "sessionId": self.session_id }))
                .await;
            self.transport.close(self.kill_grace_period).await
        })
    }

    fn respond_permission<'a>(
        &'a mut self,
        id: &'a str,
        response: PermissionResponse,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.respond_permission(id, response).await })
    }

    fn respond_user_input<'a>(
        &'a mut self,
        id: &'a str,
        answers: BTreeMap<String, Vec<String>>,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.respond_user_input(id, answers).await })
    }
}

pub(crate) struct ZcodeInitialization {
    pub command: String,
    pub args: Vec<String>,
    pub executable: std::path::PathBuf,
}

impl ZcodeBackend {
    /// Discovery-only initialization (no probe spawn, structured-CLI style):
    /// resolve the launch command/args and report the declared capabilities.
    /// The verified wire surface has no model-listing method, so
    /// `available_models` stays empty and model selection is applied per
    /// session via `session/setModel`.
    pub(crate) fn initialize(
        backend: &crate::protocol::BackendSpec,
        init: &crate::protocol::HarnessInit,
    ) -> Result<ZcodeInitialization, Error> {
        if init.port.is_some() || !matches!(init.endpoint, crate::protocol::EndpointRequest::Auto) {
            return Err(Error::InvalidConfig(
                "the Zcode app-server backend only supports managed stdio connections".to_owned(),
            ));
        }
        let cwd = HarnessDiscovery::resolve_cwd(init.cwd.as_deref())?;
        let (command, args) = command_and_args(backend)?;
        let executable =
            HarnessDiscovery::resolve_executable(init.executable.as_deref(), &command, Some(&cwd))?;
        Ok(ZcodeInitialization {
            command,
            args,
            executable,
        })
    }

    pub(crate) fn supported_capabilities() -> crate::protocol::CapabilitySet {
        crate::protocol::CapabilitySet {
            streaming_events: true,
            structured_text: true,
            reasoning_summary: true,
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            command_execution: true,
            permission_requests: true,
            permission_responses: true,
            user_input_requests: true,
            user_input_responses: true,
            turn_cancel: true,
            session_close: true,
            provider_persistence: true,
            provider_resume: true,
            raw_events: true,
            // tool.updated 未投射文件读取证据，file_reads 保持 false。
            ..crate::protocol::CapabilitySet::default()
        }
    }

    pub(crate) async fn create_session_with_config(
        config: crate::protocol::SessionConfig,
        first_message: String,
        initialized: Option<crate::protocol::HarnessState>,
    ) -> Result<crate::session::Session, Error> {
        let backend = Self::connect_with_config(&config).await?;
        let id = backend.session_id.clone();
        let mut session = crate::session::Session::with_backend_config(
            config,
            Some(id),
            Box::new(backend),
            initialized,
        );
        if !first_message.is_empty() {
            session.send(first_message, SendMode::Immediate).await?;
        }
        Ok(session)
    }
}

impl crate::protocol::SessionConfig {
    /// 会话生命周期入口：New → session/create；Resume(provider id) →
    /// session/resume；native UI 句柄与 fork/attach 不支持。
    fn conversation_session_id(&self) -> Result<Option<String>, Error> {
        match &self.conversation.mode {
            crate::protocol::ConversationSpec::New => Ok(None),
            crate::protocol::ConversationSpec::Resume(target) => match target.provider_id() {
                Some(id) if !id.is_empty() => Ok(Some(id.to_owned())),
                _ => Err(Error::UnsupportedCapability(
                    "native UI handles cannot be resumed by the Zcode backend".to_owned(),
                )),
            },
            crate::protocol::ConversationSpec::Fork(_)
            | crate::protocol::ConversationSpec::Attach(_) => Err(Error::UnsupportedCapability(
                "conversation fork and attach are not supported by the Zcode backend".to_owned(),
            )),
        }
    }
}

/// `session/create` params：workspace 必填（workspacePath/workspaceKey 同取
/// cwd）。mode 按渠道侧权限语义映射——permission_mode 覆盖原样透传
/// （build/edit/plan/yolo，另支持 guarded/auto），否则完全访问→yolo、常规
/// →build（对齐 StructuredCli 的 `--mode` 映射规则；app-server 缺省 mode 未
/// 核实，显式传保证与 CLI 行为一致）。
fn session_create_params(config: &crate::protocol::SessionConfig) -> Value {
    let cwd = config.workspace_cwd().to_string_lossy().into_owned();
    let mode = config
        .permission_mode()
        .map(str::to_owned)
        .unwrap_or_else(|| (if config.is_full_access() { "yolo" } else { "build" }).to_owned());
    json!({
        "workspace": { "workspacePath": cwd, "workspaceKey": cwd },
        "mode": mode,
    })
}

/// Session 的模型串 → setModel 的 `{providerId, modelId}`。channel 模型串
/// 约定 "provider/model"（如 "zai/glm-4.6"），按首个 '/' 拆分；无 '/' 时只
/// 传 modelId。拆分约定为静态推断（0.17.1 bundle），标注备查。
fn model_reference(model: &str) -> Value {
    match model.split_once('/') {
        Some((provider_id, model_id)) => json!({
            "providerId": provider_id,
            "modelId": model_id,
        }),
        None => json!({ "modelId": model }),
    }
}

/// `session/requestRuntimePreferences` 的自动应答（create 过程中必发，15s
/// 超时）。channel 由宿主应答提问、不委托服务端自动化：`toolSearchMode:
/// "off"` 关工具搜索、`askUserQuestionAutoResolutionEnabled: false` 关提问
/// 自动解析、`modelContextBudgetStrategy: "preflight-v1"` 取保守预检策略。
/// 语义为静态推断（0.17.1 bundle），wire 形状实测有效。
fn runtime_preferences_result() -> Value {
    json!({
        "nativeSearchEnhancementsEnabled": false,
        "toolSearchMode": "off",
        "memoryEnabled": false,
        "askUserQuestionAutoResolutionEnabled": false,
        "modelContextBudgetStrategy": "preflight-v1",
    })
}

/// `interaction/requestOfficialMcpAuthHeaders` 的自动应答：channel 不代管
/// 官方 MCP 鉴权头。
fn mcp_auth_headers_result() -> Value {
    json!({ "headersApplied": false })
}

/// 审批应答 → zcode wire 的 result 形状（decision 枚举
/// allow/deny/escalate/modify；channel 的两值映射：Approve→allow、
/// Deny→deny）。
fn permission_result(response: PermissionResponse) -> Value {
    let decision = match response {
        PermissionResponse::Approve => "allow",
        PermissionResponse::Deny => "deny",
    };
    json!({ "decision": decision })
}

/// wire id（"server-N" 字符串）→ 应答/展示用字符串；非字符串 id（防御）
/// 按 JSON 序列化。
fn wire_id_string(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

/// 反向请求簿记登记（requestId 键）：同 requestId 重发只刷新最新 wire id
/// （返回 `false`，不重复发事件），首次登记返回 `true`。
fn register_pending<V>(pending: &mut HashMap<String, V>, request_id: String, entry: V) -> bool {
    pending.insert(request_id, entry).is_none()
}

/// 归一化审批请求 → (应答句柄, detail)：句柄取 wire `requestId`（缺省回落
/// wire id 字符串，防御）；detail = params 原文 JSON 字符串（对齐现有
/// permission 流的 detail 约定）。
fn permission_required(params: &Value, wire_id: &Value) -> (String, String) {
    let id = params
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| wire_id_string(wire_id));
    (id, params.to_string())
}

/// 从 `interaction/requestUserInput` params 归一化出 [`UserInputRequest`]：
/// `id` 取 wire `requestId`（宿主应答句柄，reannounce 稳定；不用会漂移的
/// server request id），`source` 取 `toolName`（缺省回落 `schema.toolName`，
/// 再缺为 `None`），`payload` 原样透传。题干即应答键（[`UserQuestion::id`] =
/// question 文本，服务端按题干收答案），`multiSelect` 缺省单选，options 只
/// 保留 label/description（`preview` 是宿主渲染物料，归一化层丢弃）。缺
/// questions 数组或解析不出任何问题时返回 `None`，调用方回落 Raw + -32601
/// 拒绝路径。
fn user_input_request(params: &Value, request_id: &str) -> Option<UserInputRequest> {
    let questions = params.get("questions")?.as_array()?;
    let questions: Vec<UserQuestion> = questions
        .iter()
        .filter_map(|question| {
            let text = question.get("question")?.as_str()?;
            Some(UserQuestion {
                id: text.to_owned(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                question: text.to_owned(),
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| {
                                Some(UserQuestionOption {
                                    label: option.get("label")?.as_str()?.to_owned(),
                                    description: option
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect();
    if questions.is_empty() {
        return None;
    }
    Some(UserInputRequest {
        id: request_id.to_owned(),
        source: params
            .get("toolName")
            .and_then(Value::as_str)
            .or_else(|| params.pointer("/schema/toolName").and_then(Value::as_str))
            .map(str::to_owned),
        questions,
        payload: params.clone(),
    })
}

/// 归一化应答 → zcode wire 的 result 形状：
/// `{"action":"accept","content":{"answers":{"<题干>": <label>}}}`。
/// 单选题取首选项字符串，多选题（登记自请求时的 `multiSelect` 题干集合）
/// 回传完整数组（数组形态为静态推断，0.17.1 bundle）；空 Vec 视为该题未答，
/// 键不写入；全部未答 → `{"action":"decline"}`（zcode 的显式拒答语义，
/// close 冲刷也走它，回合得以收场而非悬挂在等待应答上）。
fn user_input_response(
    answers: &BTreeMap<String, Vec<String>>,
    multi_select: &BTreeSet<String>,
) -> Value {
    let answered: serde_json::Map<String, Value> = answers
        .iter()
        .filter(|(_, picked)| !picked.is_empty())
        .map(|(question, picked)| {
            let value = if multi_select.contains(question) {
                json!(picked)
            } else {
                // 单选题丢弃多余项（正常流程只有一项）。
                json!(picked.first())
            };
            (question.clone(), value)
        })
        .collect();
    if answered.is_empty() {
        json!({ "action": "decline" })
    } else {
        json!({ "action": "accept", "content": { "answers": answered } })
    }
}

struct ZcodeNotification<'a>(&'a Value);

impl<'a> ZcodeNotification<'a> {
    fn events(&self) -> Vec<Event> {
        let Some(method) = self.0.get("method").and_then(Value::as_str) else {
            return Vec::new();
        };
        match method {
            // 订阅后事件单条一帧：params {type, payload, eventId, sessionId,
            // turnId?, seq, traceId, timestamp, deliveryKind}，按 type 分派。
            "session/event" => Self::session_events(self.0),
            // 会话状态补丁：引擎以独立 method 推（wire 实测 2026-11-06，不经
            // session/event 封装），params {type:"state.updated", scope,
            // revision, reason?, patch}。patch.subagents 归一为一等子代理
            // 名单事件，其余 patch 键（status/mode/model/todo/goal/plan/
            // workflowRuns 等）各有消费者，channel 不关心。
            "state.updated" => subagents_events(self.0.get("params").unwrap_or(&Value::Null)),
            // 启动即推的存储状态噪声，契约注明可忽略。
            "startup/storageState" => Vec::new(),
            _ => vec![Event::Raw(self.0.clone())],
        }
    }

    fn session_events(message: &Value) -> Vec<Event> {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(event_type) = params.get("type").and_then(Value::as_str) else {
            // 缺 type 的畸形帧无法分类，保留为 Raw 供排查。
            return vec![Event::Raw(message.clone())];
        };
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        match event_type {
            "model.streaming" => streaming_events(&payload),
            "tool.updated" => tool_events(&payload),
            // 回合成功边界：resultType 定终态，response 为完整回答文本。
            "turn.completed" => vec![Event::Finished(turn_completed_finish(&payload))],
            // 失败终态镜像 codex 的产出方式：Event::Error 触发 Session 的
            // fail()（终态 Failed + termination 记录），不追加 Finished。
            "turn.failed" => vec![Event::Error(turn_failed_message(&payload))],
            // 会话状态补丁：patch.subagents 是子代理（worker）名单全量投影
            // （名单变化才推），归一为一等 Event::Subagents。
            "state.updated" => subagents_events(&payload),
            // 服务端反向请求的事件镜像（permission/userInput requested/
            // resolved）：真实交互走反向请求通道，忽略防止重复上报。
            // turn.started 不映射（send 已同步置 Running）；message.upserted/
            // part.* 是整条消息/部件落地，忽略避免与增量重复；session.*、
            // checkpoint、rewind、streamRecovery、steer 暂无归一化目标。
            "turn.started"
            | "message.upserted"
            | "permission.requested"
            | "permission.resolved"
            | "userInput.requested"
            | "userInput.resolved"
            | "checkpoint"
            | "rewind"
            | "streamRecovery"
            | "steer" => Vec::new(),
            kind if kind.starts_with("session.") || kind.starts_with("part.") => Vec::new(),
            _ => vec![Event::Raw(message.clone())],
        }
    }
}

/// `model.streaming` payload（{kind, delta?}）分派：text/reasoning 两类
/// delta 提升，空 delta 静默丢弃；tool_input_delta 等其余 kind 忽略（工具
/// 面走 `tool.updated`）。
fn streaming_events(payload: &Value) -> Vec<Event> {
    let delta = payload
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())
        .map(str::to_owned);
    match payload.get("kind").and_then(Value::as_str) {
        Some("text_delta") => delta.map(Event::TextDelta).into_iter().collect(),
        Some("reasoning_delta") => delta.map(Event::ReasoningDelta).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `tool.updated` payload（discriminatedUnion("kind")）：scheduled→Requested、
/// started/progress→Running、result→Completed（带 result 输出）、error→
/// Failed，未知 kind 归 Unknown。工具名字段未在实测中核实，按
/// name|toolName|title|displayName 回退链取，全缺用 toolCallId 兜底，再缺
/// 落 "tool"（学 acp.rs 的防御式回退风格）；input/output 能取则取。
fn tool_events(payload: &Value) -> Vec<Event> {
    let id = payload
        .get("toolCallId")
        .or_else(|| payload.get("tool_call_id"))
        .and_then(JsonValueExt::string_value);
    let name = ["name", "toolName", "title", "displayName"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| id.clone())
        .unwrap_or_else(|| "tool".to_owned());
    let status = match payload.get("kind").and_then(Value::as_str) {
        Some("scheduled") => ToolStatus::Requested,
        Some("started") | Some("progress") => ToolStatus::Running,
        Some("result") => ToolStatus::Completed,
        Some("error") => ToolStatus::Failed,
        _ => ToolStatus::Unknown,
    };
    let input = ["input", "arguments", "args"]
        .iter()
        .find_map(|key| payload.get(*key))
        .filter(|input| !input.is_null());
    let output = ["result", "output"]
        .iter()
        .find_map(|key| payload.get(*key))
        .filter(|output| !output.is_null());
    let error = payload.get("error").filter(|error| !error.is_null());
    vec![Event::ToolCall(ToolCall {
        id,
        name,
        status,
        input: input.map(JsonValueExt::value_text),
        output: output.map(JsonValueExt::value_text),
        error: error.map(JsonValueExt::value_text),
        sequence: 0,
    })]
}

/// state.updated → patch.subagents 子代理名单全量快照。两个入口共用：独立
/// method（wire 实测主路径，入参为 params 本体）与 session/event 封装形态
/// （入参为 params.payload，防御性保留）。subagents 形状经引擎 bundle
/// schema 核实（2026-11-06）：{revision, childSessionIds[], running[]
/// [{childSessionId, agentId?, toolCallId?, subagentType, title, summary?,
/// status, startedAt?}], endedTotal}。patch 缺 subagents 键（status/mode/
/// model/todo/goal/plan/workflowRuns 等其余状态各有消费者）或字段畸形时
/// 静默忽略，不产出事件。
fn subagents_events(payload: &Value) -> Vec<Event> {
    let subagents = match payload.get("patch").and_then(|patch| patch.get("subagents")) {
        Some(subagents) => subagents,
        None => return Vec::new(),
    };
    let revision = subagents.get("revision").and_then(Value::as_u64);
    let ended_total = subagents.get("endedTotal").and_then(Value::as_u64);
    let child_ids: Option<Vec<String>> = subagents
        .get("childSessionIds")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        });
    let running: Option<Vec<SubagentRunning>> = subagents
        .get("running")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    // childSessionId/subagentType/title 是名单条目的最小可
                    // 显示集，缺一放弃该条目；status 缺省按 running 处理。
                    let child = entry.get("childSessionId").and_then(Value::as_str)?;
                    let kind = entry.get("subagentType").and_then(Value::as_str)?;
                    let title = entry.get("title").and_then(Value::as_str)?;
                    Some(SubagentRunning {
                        child_session_id: child.to_owned(),
                        agent_id: entry.get("agentId").and_then(Value::as_str).map(str::to_owned),
                        subagent_type: kind.to_owned(),
                        title: title.to_owned(),
                        status: entry
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("running")
                            .to_owned(),
                    })
                })
                .collect()
        });
    match (revision, ended_total, child_ids, running) {
        (Some(revision), Some(ended_total), Some(child_session_ids), Some(running)) => {
            vec![Event::Subagents(SubagentsSnapshot {
                revision,
                child_session_ids,
                running,
                ended_total,
            })]
        }
        _ => Vec::new(),
    }
}

/// `turn.completed` payload {response, tokenCount, toolCallCount, duration,
/// resultType} → 终态：success→Completed、cancelled→Interrupted、error_*
/// （max_turns/max_budget/during_execution/max_tool_calls）→Failed；未知
/// resultType 保守按 Failed（镜像 codex turn/completed 的兜底）。
fn turn_completed_finish(payload: &Value) -> Finish {
    let status = match payload.get("resultType").and_then(Value::as_str) {
        Some("success") => Status::Completed,
        Some("cancelled") => Status::Interrupted,
        _ => Status::Failed,
    };
    Finish::new(
        status,
        payload
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
}

/// `turn.failed` payload {error:{type,message,...}, turnPhase} → 错误文案：
/// error.message 优先，回退 error 整体 / 顶层 message 文本。
fn turn_failed_message(payload: &Value) -> String {
    payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| payload.get("error").map(Value::to_string))
        .or_else(|| {
            payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Zcode turn failed without an error message".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HarnessKind;

    fn session_event(event_type: &str, payload: Value) -> Value {
        json!({
            "method": "session/event",
            "params": {
                "type": event_type,
                "payload": payload,
                "eventId": "evt-1",
                "sessionId": "sess-1",
                "seq": 1,
                "deliveryKind": "desktop-continuous",
            }
        })
    }

    fn user_input_params() -> Value {
        json!({
            "input": "用户输入上下文",
            "prompt": "选择一个方向",
            "questions": [
                {
                    "header": "方向",
                    "question": "先做哪个?",
                    "multiSelect": false,
                    "options": [
                        { "label": "调研", "description": "先摸清现场" },
                        { "label": "直接修", "preview": "diff 预览物料" }
                    ]
                },
                {
                    "header": "范围",
                    "question": "涉及哪些模块?",
                    "multiSelect": true,
                    "options": [
                        { "label": "前端" },
                        { "label": "后端", "description": null }
                    ]
                },
                {
                    "header": "备注",
                    "question": "补充说明?",
                    "multiSelect": false,
                    "options": null
                }
            ],
            "requestId": "req-1",
            "sessionId": "sess-1",
            "schema": { "toolName": "AskUserQuestion" },
            "toolCallId": "tc-1",
            "toolName": "AskUserQuestion",
            "turnId": "turn-1"
        })
    }

    /// (a) requestUserInput 归一化：题干即应答键、options 只留
    /// label/description、multiSelect 透传、payload 原样、source 取
    /// toolName；缺/空 questions 返回 None（回落拒绝路径）。
    #[test]
    fn normalizes_user_input_requests() {
        let params = user_input_params();
        let request = user_input_request(&params, "req-1").expect("valid user input request");
        assert_eq!(request.id, "req-1");
        assert_eq!(request.source.as_deref(), Some("AskUserQuestion"));
        assert_eq!(request.payload, params);
        assert_eq!(request.questions.len(), 3);

        let single = &request.questions[0];
        assert_eq!(single.id, "先做哪个?", "题干即应答键");
        assert_eq!(single.header.as_deref(), Some("方向"));
        assert_eq!(single.question, "先做哪个?");
        assert_eq!(single.options.len(), 2);
        assert_eq!(single.options[0].label, "调研");
        assert_eq!(single.options[0].description.as_deref(), Some("先摸清现场"));
        assert_eq!(single.options[1].description, None);
        assert!(!single.multi_select);

        let multi = &request.questions[1];
        assert!(multi.multi_select);
        assert_eq!(multi.options[1].label, "后端");

        let free_text = &request.questions[2];
        assert_eq!(free_text.options, Vec::new(), "options null → 空 Vec");

        // source 回退 schema.toolName；缺 questions / 空 questions → None。
        let schema_only = json!({
            "requestId": "r",
            "schema": { "toolName": "Ask" },
            "questions": [{ "question": "确认?" }]
        });
        let request = user_input_request(&schema_only, "r").unwrap();
        assert_eq!(request.source.as_deref(), Some("Ask"));
        assert_eq!(user_input_request(&json!({}), "r"), None);
        assert_eq!(
            user_input_request(&json!({ "questions": [] }), "r"),
            None
        );
    }

    /// (b) 应答 payload 构造：单选字符串、多选数组、空 Vec 跳过、全空
    /// decline。
    #[test]
    fn user_input_responses_map_single_and_multi_select() {
        let mut multi_select = BTreeSet::new();
        multi_select.insert("涉及哪些模块?".to_owned());

        let mut answers = BTreeMap::new();
        answers.insert("先做哪个?".to_owned(), vec!["调研".to_owned()]);
        answers.insert(
            "涉及哪些模块?".to_owned(),
            vec!["前端".to_owned(), "后端".to_owned()],
        );
        answers.insert("补充说明?".to_owned(), Vec::new());
        assert_eq!(
            user_input_response(&answers, &multi_select),
            json!({
                "action": "accept",
                "content": {
                    "answers": {
                        "先做哪个?": "调研",
                        "涉及哪些模块?": ["前端", "后端"]
                    }
                }
            })
        );

        // 全空 → 显式 decline（close 冲刷同款）。
        assert_eq!(
            user_input_response(&BTreeMap::new(), &BTreeSet::new()),
            json!({ "action": "decline" })
        );
        let mut empty_pick = BTreeMap::new();
        empty_pick.insert("先做哪个?".to_owned(), Vec::new());
        assert_eq!(
            user_input_response(&empty_pick, &BTreeSet::new()),
            json!({ "action": "decline" })
        );
    }

    /// (c) reannounce 去重：同 requestId 第二次登记只刷新最新 wire id，
    /// 返回 false（不重复发事件）；不同 requestId 各自独立登记。
    #[test]
    fn reannounce_updates_wire_id_without_duplicates() {
        let mut pending: HashMap<String, PendingUserInput> = HashMap::new();
        assert!(register_pending(
            &mut pending,
            "req-1".to_owned(),
            PendingUserInput {
                wire_id: json!("server-1"),
                multi_select: BTreeSet::new(),
            }
        ));
        assert!(!register_pending(
            &mut pending,
            "req-1".to_owned(),
            PendingUserInput {
                wire_id: json!("server-7"),
                multi_select: BTreeSet::new(),
            }
        ));
        assert_eq!(pending.len(), 1, "重发不产生新条目");
        assert_eq!(pending["req-1"].wire_id, json!("server-7"), "用最新 id");

        let mut permissions: HashMap<String, Value> = HashMap::new();
        assert!(register_pending(
            &mut permissions,
            "perm-1".to_owned(),
            json!("server-2")
        ));
        assert!(!register_pending(
            &mut permissions,
            "perm-1".to_owned(),
            json!("server-8")
        ));
        assert_eq!(permissions["perm-1"], json!("server-8"));
    }

    /// (d) requestPermission 归一化（requestId 句柄、detail 原文约定）与
    /// allow/deny 应答形状。
    #[test]
    fn permission_requests_normalize_and_answer() {
        assert_eq!(
            permission_result(PermissionResponse::Approve),
            json!({ "decision": "allow" })
        );
        assert_eq!(
            permission_result(PermissionResponse::Deny),
            json!({ "decision": "deny" })
        );

        let params = json!({
            "input": { "command": "rm -rf /" },
            "reason": "高风险命令",
            "requestId": "perm-1",
            "riskLevel": "high",
            "sessionId": "sess-1",
            "toolCallId": "tc-2",
            "toolName": "Bash"
        });
        // 宿主句柄 = requestId；detail = params 原文 JSON 字符串。
        let (id, detail) = permission_required(&params, &json!("server-3"));
        assert_eq!(id, "perm-1");
        assert_eq!(detail, params.to_string());

        // requestId 缺失回落 wire id 字符串（防御）。
        let (id, _) = permission_required(&json!({}), &json!("server-4"));
        assert_eq!(id, "server-4");
    }

    /// (e) runtime preferences / mcp auth headers 自动应答 payload：自动化
    /// 全关 + 预检预算策略；MCP 鉴权头不代管。
    #[test]
    fn auto_answers_carry_closed_automation_payloads() {
        let prefs = runtime_preferences_result();
        assert_eq!(prefs["nativeSearchEnhancementsEnabled"], json!(false));
        assert_eq!(prefs["toolSearchMode"], json!("off"));
        assert_eq!(prefs["memoryEnabled"], json!(false));
        assert_eq!(
            prefs["askUserQuestionAutoResolutionEnabled"],
            json!(false),
            "提问由宿主应答，关闭服务端自动解析"
        );
        assert_eq!(
            prefs["modelContextBudgetStrategy"],
            json!("preflight-v1")
        );
        assert_eq!(
            mcp_auth_headers_result(),
            json!({ "headersApplied": false })
        );
    }

    /// (f-1) turn.completed 三类 resultType：success→Completed、
    /// cancelled→Interrupted、error_*→Failed；response 进终态文本。
    #[test]
    fn maps_turn_completed_result_types() {
        let events = ZcodeNotification(&session_event(
            "turn.completed",
            json!({
                "response": "完整回答",
                "tokenCount": 42,
                "toolCallCount": 2,
                "duration": 1200,
                "resultType": "success"
            }),
        ))
        .events();
        assert_eq!(
            events,
            vec![Event::Finished(Finish::new(Status::Completed, "完整回答"))]
        );

        for result_type in [
            "cancelled",
            "error_max_turns",
            "error_max_budget",
            "error_during_execution",
            "error_max_tool_calls",
        ] {
            let events = ZcodeNotification(&session_event(
                "turn.completed",
                json!({ "response": "", "resultType": result_type }),
            ))
            .events();
            let expected = match result_type {
                "cancelled" => Status::Interrupted,
                _ => Status::Failed,
            };
            assert_eq!(
                events,
                vec![Event::Finished(Finish::new(expected, ""))],
                "resultType={result_type}"
            );
        }
    }

    /// (f-2) tool.updated 四 kind：scheduled/started/progress/result/error
    /// 的状态与 input/output 提取；名字段回退链与 toolCallId 兜底。
    #[test]
    fn maps_tool_updated_kinds() {
        let events = ZcodeNotification(&session_event(
            "tool.updated",
            json!({ "kind": "scheduled", "toolCallId": "tc-1", "toolName": "Bash" }),
        ))
        .events();
        assert!(matches!(&events[0], Event::ToolCall(ToolCall {
            id: Some(id), name, status: ToolStatus::Requested, ..
        }) if id == "tc-1" && name == "Bash"));

        for kind in ["started", "progress"] {
            let events = ZcodeNotification(&session_event(
                "tool.updated",
                json!({
                    "kind": kind,
                    "toolCallId": "tc-1",
                    "input": { "command": "ls" }
                }),
            ))
            .events();
            assert!(matches!(&events[0], Event::ToolCall(ToolCall {
                status: ToolStatus::Running, input: Some(input), ..
            }) if input.contains("ls")),
            "kind={kind}");
        }

        let events = ZcodeNotification(&session_event(
            "tool.updated",
            json!({
                "kind": "result",
                "toolCallId": "tc-1",
                "result": "file contents",
                "duration": 88
            }),
        ))
        .events();
        assert!(matches!(&events[0], Event::ToolCall(ToolCall {
            status: ToolStatus::Completed, output: Some(output), ..
        }) if output == "file contents"));

        let events = ZcodeNotification(&session_event(
            "tool.updated",
            json!({
                "kind": "error",
                "toolCallId": "tc-1",
                "error": { "message": "boom" }
            }),
        ))
        .events();
        assert!(matches!(&events[0], Event::ToolCall(ToolCall {
            status: ToolStatus::Failed, error: Some(error), ..
        }) if error.contains("boom")));

        // 名字全缺时 toolCallId 兜底；kind 未知归 Unknown。
        let events = ZcodeNotification(&session_event(
            "tool.updated",
            json!({ "kind": "mystery", "toolCallId": "tc-9" }),
        ))
        .events();
        assert!(matches!(&events[0], Event::ToolCall(ToolCall {
            id: Some(id), name, status: ToolStatus::Unknown, ..
        }) if id == "tc-9" && name == "tc-9"));
    }

    /// (f-3) model.streaming 两类 delta：text/reasoning 提升；空 delta 与
    /// 其余 kind（tool_input_delta 等）忽略。
    #[test]
    fn maps_streaming_deltas() {
        let events = ZcodeNotification(&session_event(
            "model.streaming",
            json!({ "kind": "text_delta", "delta": "你好" }),
        ))
        .events();
        assert_eq!(events, vec![Event::TextDelta("你好".to_owned())]);

        let events = ZcodeNotification(&session_event(
            "model.streaming",
            json!({ "kind": "reasoning_delta", "delta": "先思考" }),
        ))
        .events();
        assert_eq!(events, vec![Event::ReasoningDelta("先思考".to_owned())]);

        for payload in [
            json!({ "kind": "text_delta", "delta": "" }),
            json!({ "kind": "reasoning_delta" }),
            json!({ "kind": "tool_input_delta", "partial": "{}" }),
            json!({ "kind": "start" }),
        ] {
            assert_eq!(
                ZcodeNotification(&session_event("model.streaming", payload)).events(),
                Vec::new()
            );
        }
    }

    /// (f-4) 未知 type → Raw；已知但忽略的 type（事件镜像、生命周期、
    /// session.*/part.* 前缀）静默丢弃。
    #[test]
    fn unknown_event_types_stay_raw_and_mirrors_are_ignored() {
        let unknown = session_event("brandNew.event", json!({ "x": 1 }));
        assert_eq!(
            ZcodeNotification(&unknown).events(),
            vec![Event::Raw(unknown.clone())]
        );

        for event_type in [
            "turn.started",
            "message.upserted",
            "permission.requested",
            "permission.resolved",
            "userInput.requested",
            "userInput.resolved",
            "state.updated",
            "checkpoint",
            "rewind",
            "streamRecovery",
            "steer",
            "session.created",
            "part.updated",
        ] {
            assert_eq!(
                ZcodeNotification(&session_event(event_type, json!({}))).events(),
                Vec::new(),
                "event_type={event_type} 应被忽略"
            );
        }

        // 非 session/event 的直接通知：startup/storageState 忽略，其余 Raw。
        assert_eq!(
            ZcodeNotification(&json!({
                "method": "startup/storageState",
                "params": {}
            }))
            .events(),
            Vec::new()
        );
        let stray = json!({ "method": "future/notification", "params": {} });
        assert_eq!(
            ZcodeNotification(&stray).events(),
            vec![Event::Raw(stray)]
        );
    }

    /// (f-5) turn.failed → Event::Error（镜像 codex 失败终态产出），文案
    /// 回退链 error.message → error 整体 → 顶层 message。
    #[test]
    fn turn_failed_surfaces_error_message() {
        let events = ZcodeNotification(&session_event(
            "turn.failed",
            json!({
                "error": { "type": "provider", "message": "[1309] 套餐到期" },
                "turnPhase": "processing_input"
            }),
        ))
        .events();
        assert_eq!(
            events,
            vec![Event::Error("[1309] 套餐到期".to_owned())]
        );

        let events = ZcodeNotification(&session_event(
            "turn.failed",
            json!({ "error": { "type": "unknown" } }),
        ))
        .events();
        assert!(matches!(&events[0], Event::Error(message) if message.contains("unknown")));

        let events = ZcodeNotification(&session_event(
            "turn.failed",
            json!({ "message": "直接文案" }),
        ))
        .events();
        assert_eq!(events, vec![Event::Error("直接文案".to_owned())]);
    }

    /// (g) 审批判定只认 interaction/requestPermission 精确匹配，不受
    /// requestUserInput 污染；其余反向请求各自归类。
    #[test]
    fn user_input_is_not_classified_as_permission() {
        assert_eq!(
            ReverseRequest::from_method("interaction/requestUserInput"),
            ReverseRequest::UserInput
        );
        assert_eq!(
            ReverseRequest::from_method("interaction/requestPermission"),
            ReverseRequest::Permission
        );
        assert_ne!(
            ReverseRequest::from_method("interaction/requestUserInput"),
            ReverseRequest::Permission
        );
        assert_eq!(
            ReverseRequest::from_method("session/requestRuntimePreferences"),
            ReverseRequest::RuntimePreferences
        );
        assert_eq!(
            ReverseRequest::from_method("interaction/requestOfficialMcpAuthHeaders"),
            ReverseRequest::McpAuthHeaders
        );
        assert_eq!(
            ReverseRequest::from_method("browserList"),
            ReverseRequest::Unknown
        );
        // 模糊前缀不放宽判定（防 zcode 后续新增 interaction/* 误入审批流）。
        assert_eq!(
            ReverseRequest::from_method("interaction/requestPermissionX"),
            ReverseRequest::Unknown
        );
    }

    /// create params：workspace 双键同取 cwd；mode 映射——常规→build、
    /// 完全访问→yolo、permission_mode 覆盖优先。
    #[test]
    fn create_params_map_workspace_and_mode() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::ZCode);
        config.set_workspace(Some("/tmp/channel".into()));
        assert_eq!(
            session_create_params(&config),
            json!({
                "workspace": {
                    "workspacePath": "/tmp/channel",
                    "workspaceKey": "/tmp/channel"
                },
                "mode": "build",
            })
        );

        config.set_full_access(true);
        assert_eq!(session_create_params(&config)["mode"], json!("yolo"));

        config.set_permission_mode(Some("edit".to_owned()));
        assert_eq!(session_create_params(&config)["mode"], json!("edit"));
    }

    /// 会话生命周期分发：New→create、Resume(provider id)→resume、native UI
    /// 句柄与 fork/attach 拒绝。
    #[test]
    fn conversation_modes_map_to_session_lifecycle() {
        let mut config = crate::protocol::SessionConfig::default_for(HarnessKind::ZCode);
        assert_eq!(config.conversation_session_id().unwrap(), None);

        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::ProviderSession {
                id: "sess_1".to_owned(),
            },
        );
        assert_eq!(
            config.conversation_session_id().unwrap(),
            Some("sess_1".to_owned())
        );

        config.conversation.mode = crate::protocol::ConversationSpec::Resume(
            crate::protocol::ResumeTarget::NativeUiHandle {
                id: "ui-1".to_owned(),
            },
        );
        assert!(matches!(
            config.conversation_session_id(),
            Err(Error::UnsupportedCapability(_))
        ));

        config.conversation.mode = crate::protocol::ConversationSpec::Fork(
            crate::protocol::ResumeTarget::ProviderSession {
                id: "sess_2".to_owned(),
            },
        );
        assert!(matches!(
            config.conversation_session_id(),
            Err(Error::UnsupportedCapability(_))
        ));
    }

    /// 模型串拆分："provider/model" 双字段；无 '/' 只传 modelId。
    #[test]
    fn model_strings_split_into_provider_and_id() {
        assert_eq!(
            model_reference("zai/glm-4.6"),
            json!({ "providerId": "zai", "modelId": "glm-4.6" })
        );
        assert_eq!(model_reference("glm-4.6"), json!({ "modelId": "glm-4.6" }));
    }

    /// argv 兜底：显式 spec 缺 app-server 子命令时追加（多段启动形态保留
    /// 原有参数），Auto 用默认 command+args；其他 backend spec 拒绝。
    #[test]
    fn command_resolution_appends_app_server_subcommand() {
        let (command, args) = command_and_args(&crate::protocol::BackendSpec::Auto).unwrap();
        assert_eq!(command, "zcode");
        assert_eq!(args, vec!["app-server".to_owned()]);

        let (command, args) = command_and_args(&crate::protocol::BackendSpec::ZcodeAppServer {
            command: "node".to_owned(),
            args: vec!["/opt/ZCode/resources/glm/zcode.cjs".to_owned()],
        })
        .unwrap();
        assert_eq!(command, "node");
        assert_eq!(
            args,
            vec![
                "/opt/ZCode/resources/glm/zcode.cjs".to_owned(),
                "app-server".to_owned()
            ]
        );

        let (command, args) = command_and_args(&crate::protocol::BackendSpec::ZcodeAppServer {
            command: "zcode".to_owned(),
            args: vec!["app-server".to_owned(), "--verbose".to_owned()],
        })
        .unwrap();
        assert_eq!(
            args,
            vec!["app-server".to_owned(), "--verbose".to_owned()],
            "已有 app-server 不重复追加"
        );
        assert_eq!(command, "zcode");

        assert!(matches!(
            command_and_args(&crate::protocol::BackendSpec::Acp {
                command: "opencode".to_owned(),
                args: Vec::new(),
            }),
            Err(Error::InvalidConfig(_))
        ));
    }
}
