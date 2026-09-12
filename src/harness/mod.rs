//! Harness layer: AI coding-harness adapters (always on) plus the opt-in
//! desktop-sensing harness (feature `harness`, folded in from the former
//! `charness` crate).
//!
//! # Coding-harness adapters (always compiled)
//!
//! [`Adapter`] resolves the backend for a [`SessionConfig`] (Codex
//! app-server, ACP agents, structured/plain CLI) and drives initialization
//! and session creation; used by [`crate::Harness`] / [`crate::Session`].
//!
//! # Desktop-sensing harness (feature `harness`)
//!
//! 「极小原子工具面 + 分层关联记忆」的桌面感知底座：把 Windows/Linux 桌面
//! UI 的**感知（Sense）**能力（控件文字/句柄/树/关联关系）做成可复用组件。
//! 库形态供宿主内嵌；服务形态经 `harness` bin（JSON-lines 协议）子进程拉起。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`snapshot`] | UIA 树/控件语义采集 + 识图（`capture_screen` 整屏/区域截图）+ 合成树纯逻辑（测试底座）；Linux 接线层 [`snapshot::linux`] |
//! | [`graph`] | 控件↔窗口↔进程↔步骤 关联图；异常期归属计算接口 |
//! | [`tools`] ＋ [`actuate`] | 感知工具注册表（redact 全感知面收口）＋ 操作类五原语（click/drag/type_text/key/set_value，`--actuation` 授权门；Linux 接线层 [`actuate::linux`]） |
//! | [`serve`] + bin | JSON-lines 请求-响应/订阅（op 执行超时隔离、事件按订阅过滤） |
//! | [`store`] | ControlRef/UiAssociation/SceneSnapshot 契约、脱敏钩子、落盘 |
//!
//! **v1.15 边界（用户要求 U）**：录制事件模型（R1 事件直录）、CDP 调试
//! 浏览器直录（R2）、BYOK LLM 桥与 `relocate_control`（R5）与本产品无关，
//! 已迁至 WorkRecorder 仓的 `wr-harness-ext` crate（E:\Backup\Backup\
//! 08_start_up\35_workrecorder\harness_ext）——channel 只保留通用底座。
//!
//! # 错误与日志
//!
//! 感知面错误收敛为 [`Error`] 枚举（[`Result`] 别名默认承载），Display
//! 输出即 serve wire 错误文案；慢性病现场（轮询停摆/超时线程累积/写失败/
//! probe 初始化失败/工作线程 panic）经 tracing 结构化日志输出——lib 只发
//! 事件不装 subscriber，bin 侧初始化到 **stderr**（stdout 是 JSON-lines
//! 协议通道，绝不能被日志污染）。
//!
//! # 隐私红线
//!
//! 默认只读感知；脱敏经 [`store::Redactor`] 钩子由宿主注入；Scene/关联图
//! 出网前强制敏感扫描（宿主侧职责）；密码框不产 ControlRef 回填。

use crate::protocol::{
    BackendKind, BackendSpec, HarnessInit, HarnessKind, ProtocolInfo, SessionConfig,
    TransportKind,
};
use crate::runtime::HarnessDiscovery;
use crate::session::Session;

mod acp;
mod cli;
mod codex;

/* ---- 桌面感知 harness（原 charness crate；feature `harness`） ----------------- */

/// 操作类工具面（click/drag/type_text/key/set_value；跨平台纯逻辑 +
/// 平台接线层 [`actuate::linux`]）。
#[cfg(feature = "harness")]
pub mod actuate;
/// crate 级错误类型（Display 即 serve wire 文案）。
#[cfg(feature = "harness")]
pub mod error;
/// 控件↔窗口↔进程↔步骤 关联图。
#[cfg(feature = "harness")]
pub mod graph;
/// JSON-lines 请求-响应/订阅服务协议。
#[cfg(feature = "harness")]
pub mod serve;
/// UIA/AT-SPI 树/控件语义采集 + 识图（截图）+ 合成树纯逻辑（测试底座）。
#[cfg(feature = "harness")]
pub mod snapshot;
/// ControlRef/UiAssociation/SceneSnapshot 契约、脱敏钩子、落盘。
#[cfg(feature = "harness")]
pub mod store;
/// 感知工具注册表。
#[cfg(feature = "harness")]
pub mod tools;

#[cfg(feature = "harness")]
pub use error::{Error, Result};

/// serve JSON-lines 协议版本（不兼容变更时递增；`version` op 回带）。
#[cfg(feature = "harness")]
pub const PROTOCOL_VERSION: u32 = 1;

/// 包语义版本（`version` op 回带；与 Cargo.toml 同步维护）。
#[cfg(feature = "harness")]
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/* ---- AI coding-harness 适配器（Codex/ACP/CLI；常驻编译） ---------------------- */

#[derive(Clone, Copy)]
pub(crate) enum Adapter {
    Codex,
    Acp,
    Cli,
}

impl Adapter {
    pub(crate) fn for_config(config: &SessionConfig) -> Self {
        match &config.backend {
            BackendSpec::CodexAppServer { .. } => Adapter::Codex,
            BackendSpec::Acp { .. } => Adapter::Acp,
            BackendSpec::Auto => match config.kind {
                HarnessKind::Codex => Adapter::Codex,
                HarnessKind::OpenCode
                | HarnessKind::ZedAcp
                | HarnessKind::ZCode
                | HarnessKind::DeepSeek
                | HarnessKind::Hermes => Adapter::Acp,
                HarnessKind::ClaudeCode => Adapter::Cli,
            },
            BackendSpec::StructuredCli { .. } | BackendSpec::PlainCli { .. } => Adapter::Cli,
        }
    }

    pub(crate) async fn initialize(
        self,
        kind: HarnessKind,
        backend: &BackendSpec,
        init: &HarnessInit,
    ) -> Result<HarnessDiscovery, crate::protocol::Error> {
        match self {
            Self::Codex => self.initialize_codex(backend, init).await,
            Self::Acp => {
                let initialization = acp::AcpBackend::initialize(&kind, backend, init).await?;
                Ok(HarnessDiscovery::discover(
                    &kind,
                    BackendKind::Acp,
                    &initialization.command,
                    &initialization.args,
                    initialization.executable,
                    TransportKind::Stdio,
                    Some(ProtocolInfo {
                        name: initialization
                            .server_name
                            .clone()
                            .unwrap_or_else(|| "agent-client-protocol".to_owned()),
                        version: initialization.protocol_version,
                        initialized: true,
                    }),
                ))
            }
            Self::Cli => cli::CliBackend::initialize(&kind, backend, init),
        }
    }

    async fn initialize_codex(
        self,
        backend: &BackendSpec,
        init: &HarnessInit,
    ) -> Result<HarnessDiscovery, crate::protocol::Error> {
        let initialization = codex::CodexBackend::initialize(backend, init).await?;
        let protocol = initialization.endpoint.as_ref().map(|_| ProtocolInfo {
            name: "codex-app-server".to_owned(),
            version: None,
            initialized: true,
        });
        let transport = if initialization.endpoint.is_some() {
            TransportKind::WebSocket
        } else {
            TransportKind::Stdio
        };
        let kind = HarnessKind::Codex;
        let mut discovered = HarnessDiscovery::discover(
            &kind,
            BackendKind::CodexAppServer,
            &initialization.command,
            &initialization.args,
            initialization.executable,
            transport,
            protocol,
        );
        discovered.available_models = initialization.models;
        Ok(discovered)
    }

    pub(crate) async fn create_session_with_config(
        self,
        config: SessionConfig,
        first_message: String,
        initialized: Option<crate::protocol::HarnessState>,
    ) -> Result<Session, crate::protocol::Error> {
        match self {
            Self::Codex => {
                codex::CodexBackend::create_session_with_config(config, first_message, initialized)
                    .await
            }
            Self::Acp => {
                acp::AcpBackend::create_session_with_config(config, first_message, initialized)
                    .await
            }
            Self::Cli => {
                cli::CliBackend::create_session_with_config(config, first_message, initialized)
                    .await
            }
        }
    }
}

impl BackendKind {
    pub(crate) fn supported_capabilities(
        self,
        kind: &HarnessKind,
    ) -> crate::protocol::CapabilitySet {
        match self {
            BackendKind::Acp => acp::AcpBackend::supported_capabilities(),
            BackendKind::CodexAppServer => codex::CodexBackend::supported_capabilities(),
            BackendKind::StructuredCli => cli::CliBackend::structured_capabilities(kind),
            BackendKind::PlainCli => cli::CliBackend::plain_capabilities(),
        }
    }
}
