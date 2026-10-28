// Harness adapters (Codex/ACP/CLI) plus the opt-in desktop-sensing harness
// (feature `harness`, former `charness` crate); see the module docs there.
pub mod harness;
mod catalog;
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
mod llm;
mod process;
mod protocol;
mod runtime;
mod session;
mod switch;
mod utils;

use protocol::{HarnessInit, HarnessState};

// Installation detection and channel enumeration for embedding applications.
pub use catalog::{detect_harness, list_channels, ChannelInfo, DetectInfo};
pub use protocol::{
    AccountRef, ApprovalPolicy, AuditDescriptor, AuditMode, BackendInfo, BackendKind, BackendSpec,
    Capabilities, Capability, CapabilityEvidence, CapabilityReport, CapabilitySet, Confidence,
    ConversationOptions, ConversationSpec, CredentialSource, DecimalAmount, DirectoryRequirement,
    EndpointRequest, EnvValuePolicy, Error, Event, EventEnvelope, EventPolicy, EventVisibility,
    FileChange, FileChangeKind, FileRead, FileReadSource, FileReadStatus, FilesystemAccess,
    FilesystemPermission, Finish, FinishInfo, HarnessKind, ModelInfo, ModelOptions, NetworkPolicy,
    Observation, ObservationSource, PathVisibility, PermissionInfo, PermissionLimitation,
    PermissionSpec, ProcessGroupPolicy, ProcessPermission, ProtocolInfo, ProviderOptions,
    ReasoningEffort, RepositoryDetection, RepositoryInfo, RepositoryKind, ResourcePolicy,
    Resumability, ResumeTarget, RetentionPolicy, RuntimeInfo, RuntimeOptions, RuntimeSpec,
    SecretAccess, SecretPermission, SecurityOptions, SessionConfig, SessionInfo, SessionState,
    ShareAudience, Shareability, Status, StderrPolicy, SwitchProvider, TempPolicy, TodoEntry,
    TodoSnapshot, TodoStatus, ToolCall, ToolStatus, TransportKind, TriState, TurnInfo, Usage,
    VisibilityInfo, VisibilityLimitation, VisibilityRequest, VisibilityState, WorkspaceInfo,
    WorkspaceOptions, WorkspaceRoot,
};
pub use session::{
    ActivityCounts, ActivityEvent, ActivityId, ActivityKind, ActivityStatus, ActivitySummary,
    ActivityView, InterruptAction, InterruptResult, MessageEvent, PermissionEvent,
    PermissionResponse, SendMode, SendResult, Session, SessionEvent,
};

// AI chat-history persistence (`history` feature): an owned SQLite store in
// the ZCode session-store format (`chat_histories.db`), with CRUD, the
// compaction storage primitives and the read-only ZCode engine-database
// helpers for external compaction transcription.
#[cfg(feature = "history")]
mod history;
#[cfg(feature = "history")]
pub use history::{
    assistant_message_data, channel_selection_data, part_data, user_message_data,
    // 压缩（compaction）：存储原语与类型（格式契约见 design.md）
    CompactionBoundary, CompactionBoundaryView, CompactionPreservedSegment,
    CompactionSummaryParams, OPERATION_ID_HOST_PREFIX, PART_TYPE_COMPACTION,
    SEMANTICS_KIND_COMPACT_SUMMARY, SUMMARY_CONTINUATION_PREFIX,
    compaction_marker_part_data, compaction_part_data, compaction_summary_message_data,
    // 外部压缩转写：ZCode 引擎库只读正文获取
    read_zcode_compaction_summary_text, zcode_session_db_path,
    // 实体类型（宿主自行转 DTO）
    ChatHistoryDatabase, MessageRecord, ProjectRecord, SessionRecord,
    // 行模型（存储格式契约，供宿主做遗留数据导入等高级流程）
    LocalSettingRow, MessageRow, PartRow, ProjectRow, SessionEntryRow, SessionRow,
    // ID/工具
    new_event_id, new_session_id, slugify,
    // 常量
    DB_FILE_NAME, DEFAULT_TITLE, ENTRY_SUFFIX_CHANNEL_SELECTION,
    ENTRY_TYPE_CHANNEL_SELECTION, SETTING_SCOPE, SETTING_SCOPE_ID,
    TASK_TYPE_INTERACTIVE, TITLE_SOURCE_CUSTOM, TITLE_SOURCE_DEFAULT,
    TITLE_SOURCE_FIRST_INPUT,
};

// Types shared by the HTTP protocol clients (`llm`) and the local-model
// client (`local-safetensors-cpu`).
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub use llm::{ChatMessage, LlmModelInfo, MessageRole, StreamChunk};
// The four HTTP protocol clients; pure potato transports.
#[cfg(feature = "llm")]
pub use llm::{ChatCompletionsClient, ForwardedCompletion, MessagesClient, OllamaClient, ResponsesClient};
// Server-side relay entry point for OpenAI Chat Completions (arbitrary body passthrough).
#[cfg(feature = "llm")]
pub use llm::forward_chat_completions;
// Server-side stream emitters; usable by the local-model server without `llm`.
#[cfg(any(feature = "llm", feature = "local-safetensors-cpu"))]
pub use llm::{AnthropicSender, OllamaSender, OpenAISender};
// Local-model inference (`local-safetensors-cpu` family).
#[cfg(feature = "local-safetensors-cpu")]
pub use llm::{GenerationParams, LoadOptions, LocalBackendKind, LocalClient, LocalModelMeta};
// One-click HTTP endpoints for local models (ships with `local-safetensors-cpu`).
#[cfg(feature = "local-safetensors-cpu")]
pub use llm::LocalLlmServer;

/// An initialized harness handle. The resolved runtime remains private.
#[derive(Clone, Debug)]
pub struct Harness(HarnessState);

impl Harness {
    pub async fn initialize(kind: HarnessKind) -> Result<Self, Error> {
        Self::initialize_with(SessionConfig::for_kind(kind)).await
    }

    pub async fn initialize_with(config: SessionConfig) -> Result<Self, Error> {
        Ok(Self(Self::initialize_harness(config).await?))
    }

    /// Resumes a provider-side session. The returned `Session` is idle; the
    /// first turn after resuming starts with a regular `send`.
    pub async fn resume(self, target: ResumeTarget) -> Result<Session, Error> {
        if !self.0.capabilities.effective.provider_resume {
            return Err(Error::UnsupportedCapability(
                "provider session resume is not supported by the initialized harness".to_owned(),
            ));
        }
        let mut config = self.0.config.clone();
        config.conversation.mode = ConversationSpec::Resume(target);
        let state = self.0;
        Session::start_session(config, String::new(), Vec::new(), Some(state)).await
    }

    pub fn kind(&self) -> HarnessKind {
        self.0.config.kind.clone()
    }

    pub fn is_full_access(&self) -> bool {
        self.0.config.is_full_access()
    }

    pub fn capabilities(&self) -> &CapabilityReport {
        &self.0.capabilities
    }

    pub fn available_models(&self) -> &[String] {
        &self.0.available_models
    }

    pub async fn available_switch_providers(
        &self,
        source: SwitchProvider,
    ) -> Result<Vec<String>, Error> {
        source.available_keys().await
    }
}

impl Harness {
    async fn initialize_harness(mut config: SessionConfig) -> Result<HarnessState, Error> {
        config.prepare_workspace()?;
        let init = HarnessInit::for_config(&config);
        let discovery = harness::Adapter::for_config(&config)
            .initialize(config.kind.clone(), &config.backend, &init)
            .await?;

        config.backend = match discovery.backend.kind {
            crate::protocol::BackendKind::CodexAppServer => BackendSpec::CodexAppServer {
                command: discovery.backend.command.clone().unwrap_or_default(),
                args: discovery.runtime.args.clone(),
            },
            crate::protocol::BackendKind::Acp => BackendSpec::Acp {
                command: discovery.backend.command.clone().unwrap_or_default(),
                args: discovery.runtime.args.clone(),
            },
            crate::protocol::BackendKind::StructuredCli => BackendSpec::StructuredCli {
                command: discovery.backend.command.clone().unwrap_or_default(),
                args: discovery.runtime.args.clone(),
            },
            crate::protocol::BackendKind::PlainCli => BackendSpec::PlainCli {
                command: discovery.backend.command.clone().unwrap_or_default(),
                args: discovery.runtime.args.clone(),
            },
        };
        if let Some(executable) = discovery.runtime.executable_path.clone() {
            config.runtime.process.executable = Some(executable);
        }
        config.endpoint = init.endpoint;
        config.port = init.port;
        config.model.available_models = discovery.available_models.clone();
        Ok(HarnessState {
            config,
            backend: discovery.backend,
            runtime: discovery.runtime,
            capabilities: discovery.capabilities,
            available_models: discovery.available_models,
        })
    }
}

impl Session {
    /// Creates a session from detailed layered configuration.
    pub async fn create(
        config: SessionConfig,
        first_message: impl Into<String>,
    ) -> Result<Self, Error> {
        Self::create_with_attachments(config, first_message, Vec::new()).await
    }

    /// Creates a session and opens the first turn carrying file attachments
    /// (absolute paths, resolved on the harness host). Backends without
    /// attachment support drop them silently.
    pub async fn create_with_attachments(
        config: SessionConfig,
        first_message: impl Into<String>,
        attachments: Vec<String>,
    ) -> Result<Self, Error> {
        let harness = Harness::initialize_with(config).await?;
        let state = harness.0;
        let initialized_config = state.config.clone();
        Self::start_session(initialized_config, first_message.into(), attachments, Some(state))
            .await
    }
}

impl Session {
    async fn start_session(
        mut config: SessionConfig,
        first_message: String,
        attachments: Vec<String>,
        initialized: Option<HarnessState>,
    ) -> Result<Session, Error> {
        config.prepare_workspace()?;
        harness::Adapter::for_config(&config)
            .create_session_with_config(config, first_message, attachments, initialized)
            .await
    }
}
