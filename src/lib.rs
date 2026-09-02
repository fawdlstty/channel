mod acp;
mod cli;
mod codex;
mod process;
mod protocol;
mod runtime;
mod session;

use protocol::HarnessRuntime;

pub use protocol::{
    AccountRef, ApprovalPolicy, AuditDescriptor, AuditMode, BackendInfo, BackendKind, BackendSpec,
    Capabilities, Capability, CapabilityEvidence, CapabilityReport, CapabilitySet, Confidence,
    ConversationOptions, ConversationSpec, CredentialSource, DecimalAmount, DirectoryRequirement,
    EndpointRequest, EnvValuePolicy, Error, Event, EventEnvelope, EventPolicy, EventVisibility,
    FileChange, FileChangeKind, FileRead, FileReadSource, FileReadStatus, FilesystemAccess,
    FilesystemPermission, Finish, FinishInfo, HarnessType, ModelInfo, ModelOptions, NetworkPolicy,
    Observation, ObservationSource, PathVisibility, PermissionInfo, PermissionLimitation,
    PermissionSpec, ProcessGroupPolicy, ProcessPermission, ProtocolInfo, ProviderOptions,
    RepositoryDetection, RepositoryInfo, RepositoryKind, ResourcePolicy, Resumability,
    ResumeTarget, RetentionPolicy, RuntimeInfo, RuntimeOptions, RuntimeSpec, SecretAccess,
    SecretPermission, SecurityOptions, SessionConfig, SessionConfigBuilder, SessionInfo,
    SessionState, ShareAudience, Shareability, Status, StderrPolicy, TempPolicy, ToolCall,
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
pub struct Harness(HarnessRuntime);

impl Harness {
    pub fn capabilities(&self) -> &CapabilityReport {
        &self.0.capabilities
    }

    pub async fn create_session(&self, first_message: impl Into<String>) -> Result<Session, Error> {
        create_session_with_config(
            self.0.config.clone(),
            first_message.into(),
            Some(self.0.clone()),
        )
        .await
    }
}

/// Checks that a harness is available with default initialization settings.
pub async fn initialize(harness: HarnessType) -> Result<Harness, Error> {
    initialize_with(SessionConfig::for_harness(harness).build()).await
}

/// Initializes a harness from the complete session configuration.
pub async fn initialize_with(config: SessionConfig) -> Result<Harness, Error> {
    Ok(Harness(initialize_runtime(config).await?))
}

async fn initialize_runtime(mut config: SessionConfig) -> Result<HarnessRuntime, Error> {
    prepare_workspace(&mut config)?;
    let init = init_from_config(&config);
    let harness = config.harness.clone();
    let discovery = match &config.backend {
        BackendSpec::CodexAppServer { .. } => initialize_codex(&config.backend, &init).await?,
        BackendSpec::Acp { .. } => initialize_acp(&harness, &config.backend, &init).await?,
        BackendSpec::StructuredCli { .. }
        | BackendSpec::PlainCli { .. }
        | BackendSpec::Custom { .. } => crate::cli::initialize(&harness, &config.backend, &init)?,
        BackendSpec::Auto => match harness {
            HarnessType::Codex => initialize_codex(&config.backend, &init).await?,
            HarnessType::OpenCode
            | HarnessType::ZedAcp
            | HarnessType::ZCode
            | HarnessType::DeepSeek => initialize_acp(&harness, &config.backend, &init).await?,
            _ => crate::cli::initialize(&harness, &config.backend, &init)?,
        },
    };

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
        crate::protocol::BackendKind::Custom => BackendSpec::Custom {
            command: discovery.backend.command.clone().unwrap_or_default(),
            args: discovery.runtime.args.clone(),
        },
    };
    if let Some(executable) = discovery.runtime.executable_path.clone() {
        config.runtime.process.executable = Some(executable);
    }
    config.endpoint = init.endpoint;
    config.port = init.port;
    Ok(HarnessRuntime {
        config,
        backend: discovery.backend,
        runtime: discovery.runtime,
        capabilities: discovery.capabilities,
    })
}

async fn initialize_codex(
    backend: &BackendSpec,
    init: &crate::protocol::HarnessInit,
) -> Result<crate::runtime::HarnessDiscovery, Error> {
    let initialization = crate::codex::initialize(backend, init).await?;
    let protocol = initialization.endpoint.as_ref().map(|_| ProtocolInfo {
        name: "codex-app-server".to_owned(),
        version: None,
        initialized: true,
    });
    let transport = if initialization.endpoint.is_some() {
        crate::protocol::TransportKind::WebSocket
    } else {
        crate::protocol::TransportKind::Stdio
    };
    let harness = HarnessType::Codex;
    Ok(crate::runtime::discovery(
        &harness,
        crate::protocol::BackendKind::CodexAppServer,
        &initialization.command,
        &initialization.args,
        initialization.executable,
        transport,
        protocol,
    ))
}

async fn initialize_acp(
    harness: &HarnessType,
    backend: &BackendSpec,
    init: &crate::protocol::HarnessInit,
) -> Result<crate::runtime::HarnessDiscovery, Error> {
    let initialization = crate::acp::initialize(harness, backend, init).await?;
    Ok(crate::runtime::discovery(
        harness,
        crate::protocol::BackendKind::Acp,
        &initialization.command,
        &initialization.args,
        initialization.executable,
        crate::protocol::TransportKind::Stdio,
        Some(crate::protocol::ProtocolInfo {
            name: initialization
                .server_name
                .clone()
                .unwrap_or_else(|| "agent-client-protocol".to_owned()),
            version: initialization.protocol_version,
            initialized: true,
        }),
    ))
}

fn init_from_config(config: &SessionConfig) -> crate::protocol::HarnessInit {
    crate::protocol::HarnessInit {
        endpoint: config.endpoint.clone(),
        executable: config.runtime.process.executable.clone(),
        cwd: Some(config.workspace.cwd.clone()),
        port: config.port,
    }
}

/// Creates a session through the simple harness-first entry point.
pub async fn create_session(
    harness: HarnessType,
    cwd: Option<String>,
    first_message: impl Into<String>,
) -> Result<Session, Error> {
    CreateSession(harness, cwd, first_message).await
}

/// Creates a session from a harness selection with an optional working directory.
#[allow(non_snake_case)]
pub async fn CreateSession(
    harness: HarnessType,
    cwd: Option<String>,
    first_message: impl Into<String>,
) -> Result<Session, Error> {
    let mut config = SessionConfig::for_harness(harness).build();
    if let Some(cwd) = cwd {
        config.workspace.cwd = cwd.into();
    }
    initialize_with(config)
        .await?
        .create_session(first_message)
        .await
}

/// Creates a session from detailed layered configuration.
#[allow(non_snake_case)]
pub async fn CreateSessionWithConfig(
    config: SessionConfig,
    first_message: impl Into<String>,
) -> Result<Session, Error> {
    initialize_with(config)
        .await?
        .create_session(first_message)
        .await
}

async fn create_session_with_config(
    mut config: SessionConfig,
    first_message: String,
    initialized: Option<HarnessRuntime>,
) -> Result<Session, Error> {
    prepare_workspace(&mut config)?;
    let harness = config.harness.clone();
    match &config.backend {
        BackendSpec::CodexAppServer { .. } => {
            codex::create_session_with_config(config, first_message, initialized).await
        }
        BackendSpec::Acp { .. } => {
            acp::create_session_with_config(config, first_message, initialized).await
        }
        BackendSpec::StructuredCli { .. }
        | BackendSpec::PlainCli { .. }
        | BackendSpec::Custom { .. } => {
            cli::create_session_with_config(config, first_message, initialized).await
        }
        BackendSpec::Auto => match harness {
            HarnessType::Codex => {
                codex::create_session_with_config(config, first_message, initialized).await
            }
            HarnessType::OpenCode
            | HarnessType::ZedAcp
            | HarnessType::ZCode
            | HarnessType::DeepSeek => {
                acp::create_session_with_config(config, first_message, initialized).await
            }
            _ => cli::create_session_with_config(config, first_message, initialized).await,
        },
    }
}

fn prepare_workspace(config: &mut SessionConfig) -> Result<(), Error> {
    let cwd = &config.workspace.cwd;
    if !cwd.exists() {
        match config.workspace.existence {
            DirectoryRequirement::CreateIfMissing => {
                std::fs::create_dir_all(cwd).map_err(|error| {
                    Error::InvalidConfig(format!(
                        "failed to create workspace {}: {error}",
                        cwd.display()
                    ))
                })?
            }
            DirectoryRequirement::MustExist => {
                return Err(Error::InvalidConfig(format!(
                    "workspace does not exist: {}",
                    cwd.display()
                )))
            }
            DirectoryRequirement::MayBeMissing => {}
        }
    }
    if cwd.exists() && !cwd.is_dir() {
        return Err(Error::InvalidConfig(format!(
            "workspace is not a directory: {}",
            cwd.display()
        )));
    }
    Ok(())
}

pub async fn resume_session(harness: Harness, target: ResumeTarget) -> Result<Session, Error> {
    if !harness.0.capabilities.effective.provider_resume {
        return Err(Error::UnsupportedCapability(
            "provider session resume is not supported by the initialized harness".to_owned(),
        ));
    }
    let mut config = harness.0.config.clone();
    config.conversation.mode = ConversationSpec::Resume(target);
    Err(Error::UnsupportedCapability(
        "provider session resume is not implemented".to_owned(),
    ))
}
