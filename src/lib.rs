mod harness;
mod process;
mod protocol;
mod runtime;
mod session;
mod switch;
mod utils;

use protocol::{HarnessInit, HarnessState};

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
    ShareAudience, Shareability, Status, StderrPolicy, SwitchProvider, TempPolicy, ToolCall,
    ToolStatus, TransportKind, TriState, TurnInfo, Usage, VisibilityInfo, VisibilityLimitation,
    VisibilityRequest, VisibilityState, WorkspaceInfo, WorkspaceOptions, WorkspaceRoot,
};
pub use session::{
    ActivityCounts, ActivityEvent, ActivityId, ActivityKind, ActivityStatus, ActivitySummary,
    ActivityView, InterruptAction, InterruptResult, MessageEvent, PermissionEvent, SendMode,
    SendResult, Session, SessionEvent,
};

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
        Session::start_session(config, String::new(), Some(state)).await
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

    pub fn available_switch_providers(&self, source: SwitchProvider) -> Result<Vec<String>, Error> {
        source.available_keys()
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
        let harness = Harness::initialize_with(config).await?;
        let state = harness.0;
        let initialized_config = state.config.clone();
        Self::start_session(initialized_config, first_message.into(), Some(state)).await
    }
}

impl Session {
    async fn start_session(
        mut config: SessionConfig,
        first_message: String,
        initialized: Option<HarnessState>,
    ) -> Result<Session, Error> {
        config.prepare_workspace()?;
        harness::Adapter::for_config(&config)
            .create_session_with_config(config, first_message, initialized)
            .await
    }
}
