use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarnessType {
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
    Custom { command: String, args: Vec<String> },
}

/// A requested provider endpoint. `Auto` deliberately does not imply a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointRequest {
    Auto,
    Explicit(String),
}

impl Default for EndpointRequest {
    fn default() -> Self {
        Self::Auto
    }
}

impl From<String> for EndpointRequest {
    fn from(endpoint: String) -> Self {
        Self::Explicit(endpoint)
    }
}

impl From<&str> for EndpointRequest {
    fn from(endpoint: &str) -> Self {
        Self::Explicit(endpoint.to_owned())
    }
}

/// Inputs used to discover or start a harness runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessInit {
    pub endpoint: EndpointRequest,
    pub executable: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub port: Option<u16>,
}

impl Default for HarnessInit {
    fn default() -> Self {
        Self {
            endpoint: EndpointRequest::Auto,
            executable: None,
            cwd: None,
            port: None,
        }
    }
}

/// Resolved initialization metadata retained for internal session creation.
#[derive(Clone, Debug)]
pub(crate) struct HarnessRuntime {
    pub(crate) config: SessionConfig,
    pub(crate) backend: BackendInfo,
    pub(crate) runtime: RuntimeInfo,
    pub(crate) capabilities: CapabilityReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityOptions {
    pub permissions: PermissionSpec,
    pub network: NetworkPolicy,
    pub approval: ApprovalPolicy,
}

impl Default for SecurityOptions {
    fn default() -> Self {
        Self {
            permissions: PermissionSpec::workspace_read_write(),
            network: NetworkPolicy::ProviderDefault,
            approval: ApprovalPolicy::ProviderDefault,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelOptions {
    pub requested: Option<String>,
    pub provider: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
}

impl Default for ModelOptions {
    fn default() -> Self {
        Self {
            requested: None,
            provider: None,
            reasoning_effort: None,
            temperature: None,
            max_output_tokens: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeOptions {
    pub process: RuntimeSpec,
    pub resources: ResourcePolicy,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            process: RuntimeSpec::default(),
            resources: ResourcePolicy::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationOptions {
    pub mode: ConversationSpec,
}

impl Default for ConversationOptions {
    fn default() -> Self {
        Self {
            mode: ConversationSpec::New,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservabilityOptions {
    pub visibility: VisibilityRequest,
    pub events: EventPolicy,
}

impl Default for ObservabilityOptions {
    fn default() -> Self {
        Self {
            visibility: VisibilityRequest::ProviderDefault,
            events: EventPolicy::default(),
        }
    }
}

pub type ProviderOptions = serde_json::Map<String, serde_json::Value>;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    pub(crate) harness: HarnessType,
    pub(crate) workspace: WorkspaceOptions,
    pub(crate) security: SecurityOptions,
    pub(crate) model: ModelOptions,
    pub(crate) runtime: RuntimeOptions,
    pub(crate) conversation: ConversationOptions,
    pub(crate) observability: ObservabilityOptions,
    pub(crate) metadata: BTreeMap<String, String>,
    pub(crate) endpoint: EndpointRequest,
    pub(crate) port: Option<u16>,
    provider_options: ProviderOptions,
    pub(crate) backend: BackendSpec,
}

impl SessionConfig {
    pub fn for_harness(harness: HarnessType) -> SessionConfigBuilder {
        SessionConfigBuilder::new(harness)
    }

    pub fn harness(&self) -> &HarnessType {
        &self.harness
    }

    pub fn workspace(&self) -> &WorkspaceOptions {
        &self.workspace
    }

    pub fn security(&self) -> &SecurityOptions {
        &self.security
    }

    pub fn model(&self) -> &ModelOptions {
        &self.model
    }

    pub fn runtime(&self) -> &RuntimeOptions {
        &self.runtime
    }

    pub fn conversation(&self) -> &ConversationOptions {
        &self.conversation
    }

    pub fn observability(&self) -> &ObservabilityOptions {
        &self.observability
    }

    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    pub fn provider_options(&self) -> &ProviderOptions {
        &self.provider_options
    }

    pub(crate) fn default_for(harness: HarnessType) -> Self {
        Self {
            harness,
            workspace: WorkspaceOptions::default(),
            security: SecurityOptions::default(),
            model: ModelOptions::default(),
            runtime: RuntimeOptions::default(),
            conversation: ConversationOptions::default(),
            observability: ObservabilityOptions::default(),
            metadata: BTreeMap::new(),
            endpoint: EndpointRequest::Auto,
            port: None,
            provider_options: ProviderOptions::new(),
            backend: BackendSpec::Auto,
        }
    }
}

pub struct SessionConfigBuilder {
    config: SessionConfig,
}

impl SessionConfigBuilder {
    fn new(harness: HarnessType) -> Self {
        Self {
            config: SessionConfig::default_for(harness),
        }
    }

    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.workspace.cwd = path.into();
        self
    }

    pub fn model(mut self, name: impl Into<String>) -> Self {
        self.config.model.requested = Some(name.into());
        self
    }

    pub fn network(mut self, network: NetworkPolicy) -> Self {
        self.config.security.network = network;
        self
    }

    pub fn approval(mut self, approval: ApprovalPolicy) -> Self {
        self.config.security.approval = approval;
        self
    }

    pub fn permissions(mut self, permissions: PermissionSpec) -> Self {
        self.config.security.permissions = permissions;
        self
    }

    pub fn runtime(mut self, runtime: RuntimeOptions) -> Self {
        self.config.runtime = runtime;
        self
    }

    pub fn conversation(mut self, conversation: ConversationOptions) -> Self {
        self.config.conversation = conversation;
        self
    }

    pub fn observability(mut self, observability: ObservabilityOptions) -> Self {
        self.config.observability = observability;
        self
    }

    pub fn endpoint(mut self, endpoint: impl Into<EndpointRequest>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    pub fn executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.runtime.process.executable = Some(path.into());
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.config.port = Some(port);
        self
    }

    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.metadata.insert(key.into(), value.into());
        self
    }

    pub fn provider_option(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.config.provider_options.insert(key.into(), value);
        self
    }

    pub fn codex_app_server(
        mut self,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.config.backend = BackendSpec::CodexAppServer {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn acp(
        mut self,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.config.backend = BackendSpec::Acp {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn structured_cli(
        mut self,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.config.backend = BackendSpec::StructuredCli {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn plain_cli(
        mut self,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.config.backend = BackendSpec::PlainCli {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn custom_command(
        mut self,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.config.backend = BackendSpec::Custom {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn build(self) -> SessionConfig {
        self.config
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceOptions {
    /// The requested working directory passed to the harness.
    pub cwd: PathBuf,
    /// Allowed path ranges; an empty list does not mean full host access.
    pub roots: Vec<WorkspaceRoot>,
    pub existence: DirectoryRequirement,
    pub repository_detection: RepositoryDetection,
    pub temp_policy: TempPolicy,
}

impl WorkspaceOptions {
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        Self {
            cwd: path.into(),
            ..Self::default()
        }
    }
}

impl Default for WorkspaceOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            roots: Vec::new(),
            existence: DirectoryRequirement::MustExist,
            repository_detection: RepositoryDetection::BestEffort,
            temp_policy: TempPolicy::WorkspaceOnly,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRoot {
    pub path: PathBuf,
    pub access: FilesystemAccess,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub requested_cwd: PathBuf,
    pub effective_cwd: Option<PathBuf>,
    pub canonical_cwd: Option<PathBuf>,
    pub roots: Vec<EffectiveRoot>,
    pub repository: Option<RepositoryInfo>,
    pub path_visibility: PathVisibility,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveRoot {
    pub path: PathBuf,
    pub access: FilesystemAccess,
    pub label: Option<String>,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub kind: RepositoryKind,
    pub root: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: TriState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryKind {
    Git,
    Mercurial,
    Other(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryRequirement {
    #[default]
    MustExist,
    CreateIfMissing,
    MayBeMissing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepositoryDetection {
    Disabled,
    #[default]
    BestEffort,
    Required,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TempPolicy {
    Deny,
    #[default]
    WorkspaceOnly,
    ProviderDefault,
    SystemTemp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilesystemAccess {
    None,
    ReadOnly,
    WorkspaceReadWrite,
    SelectedRoots,
    FullHost,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionSpec {
    pub filesystem: FilesystemPermission,
    pub process: ProcessPermission,
    pub environment: EnvironmentPermission,
    pub secrets: SecretPermission,
}

impl PermissionSpec {
    pub fn workspace_read_write() -> Self {
        Self {
            filesystem: FilesystemPermission {
                mode: FilesystemAccess::WorkspaceReadWrite,
                roots: Vec::new(),
                follow_symlinks: false,
            },
            process: ProcessPermission::default(),
            environment: EnvironmentPermission::default(),
            secrets: SecretPermission::default(),
        }
    }
}

impl Default for PermissionSpec {
    fn default() -> Self {
        Self::workspace_read_write()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesystemPermission {
    pub mode: FilesystemAccess,
    pub roots: Vec<PathBuf>,
    pub follow_symlinks: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessPermission {
    pub execute: TriState,
    pub terminate: TriState,
    pub control: TriState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentPermission {
    pub policy: EnvValuePolicy,
    pub allowed_variables: Vec<String>,
}

impl Default for EnvironmentPermission {
    fn default() -> Self {
        Self {
            policy: EnvValuePolicy::Redact,
            allowed_variables: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretPermission {
    pub access: SecretAccess,
    pub sources: Vec<String>,
}

impl Default for SecretPermission {
    fn default() -> Self {
        Self {
            access: SecretAccess::Deny,
            sources: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecretAccess {
    #[default]
    Deny,
    Allow,
    ProviderDefault,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionInfo {
    pub requested: PermissionSpec,
    pub effective: Option<PermissionSpec>,
    pub confidence: Confidence,
    pub source: ObservationSource,
    pub limitations: Vec<PermissionLimitation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionLimitation {
    pub area: String,
    pub reason: String,
    pub source: ObservationSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalPolicy {
    RequireHandler,
    AutoApprove,
    AutoDeny,
    #[default]
    ProviderDefault,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendSpec {
    Auto,
    CodexAppServer { command: String, args: Vec<String> },
    Acp { command: String, args: Vec<String> },
    StructuredCli { command: String, args: Vec<String> },
    PlainCli { command: String, args: Vec<String> },
    Custom { command: String, args: Vec<String> },
}

impl Default for BackendSpec {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub command: Option<String>,
    pub transport: TransportKind,
    pub protocol: Option<ProtocolInfo>,
    pub version: Option<String>,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolInfo {
    pub name: String,
    pub version: Option<String>,
    pub initialized: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportKind {
    InProcess,
    #[default]
    Stdio,
    Http,
    WebSocket,
    Sdk,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    CodexAppServer,
    Acp,
    StructuredCli,
    PlainCli,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TriState {
    Yes,
    No,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSpec {
    pub executable: Option<PathBuf>,
    pub env: BTreeMap<String, EnvValuePolicy>,
    pub inherit_environment: bool,
    pub timeout: Option<Duration>,
    pub kill_grace_period: Duration,
    pub stderr: StderrPolicy,
    pub process_group: ProcessGroupPolicy,
}

impl Default for RuntimeSpec {
    fn default() -> Self {
        Self {
            executable: None,
            env: BTreeMap::new(),
            inherit_environment: true,
            timeout: None,
            kill_grace_period: Duration::from_secs(2),
            stderr: StderrPolicy::Diagnostic,
            process_group: ProcessGroupPolicy::ProviderDefault,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub executable_path: Option<PathBuf>,
    pub pid: Option<u32>,
    pub parent_pid: Option<u32>,
    pub process_started_at: Option<SystemTime>,
    pub version: Option<String>,
    pub protocol: Option<ProtocolInfo>,
    pub host_os: String,
    pub architecture: String,
    pub containerized: TriState,
    pub source: ObservationSource,
}

impl Default for RuntimeInfo {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            executable_path: None,
            pid: None,
            parent_pid: None,
            process_started_at: None,
            version: None,
            protocol: None,
            host_os: String::new(),
            architecture: String::new(),
            containerized: TriState::Unknown,
            source: ObservationSource::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub requested: Option<String>,
    pub effective: Option<String>,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisibilityRequest {
    Private,
    channelOnly,
    ProviderSession {
        discoverable: bool,
        resumable: bool,
    },
    NativeUi {
        discoverable: bool,
        attachable: bool,
    },
    Shared {
        audience: ShareAudience,
    },
    ProviderDefault,
}

impl Default for VisibilityRequest {
    fn default() -> Self {
        Self::ProviderDefault
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibilityInfo {
    pub requested: VisibilityRequest,
    pub channel: VisibilityState,
    pub provider: VisibilityState,
    pub native_ui: VisibilityState,
    pub resumability: Resumability,
    pub shareability: Shareability,
    pub account: Option<AccountRef>,
    pub external_ids: ExternalSessionIds,
    pub source: ObservationSource,
    pub limitations: Vec<VisibilityLimitation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisibilityState {
    Yes,
    No,
    #[default]
    Unknown,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resumability {
    InMemoryOnly,
    channelResume,
    ProviderResume,
    NativeUiResume,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shareability {
    No,
    Yes,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibilityLimitation {
    pub area: String,
    pub reason: String,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSessionIds {
    pub channel_session_id: String,
    pub provider_session_id: Option<String>,
    pub provider_thread_id: Option<String>,
    pub provider_turn_id: Option<String>,
    pub transport_connection_id: Option<String>,
    pub process_id: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationSpec {
    New,
    Resume(ResumeTarget),
    Fork(ResumeTarget),
    Attach(ResumeTarget),
}

impl Default for ConversationSpec {
    fn default() -> Self {
        Self::New
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeTarget {
    channelSession(String),
    ProviderSession { id: String },
    ProviderThread { id: String },
    NativeUiHandle { id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventPolicy {
    pub include_reasoning: bool,
    pub include_tool_inputs: bool,
    pub include_tool_outputs: bool,
    pub include_raw: bool,
    pub max_raw_bytes: Option<u64>,
}

impl Default for EventPolicy {
    fn default() -> Self {
        Self {
            include_reasoning: true,
            include_tool_inputs: true,
            include_tool_outputs: true,
            include_raw: false,
            max_raw_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventVisibility {
    pub channel: bool,
    pub harness: bool,
    pub native_ui: TriState,
    pub user: bool,
    pub other_clients: TriState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventEnvelope {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub sequence: usize,
    pub observed_at: SystemTime,
    pub source: ObservationSource,
    pub confidence: Confidence,
    pub visibility: EventVisibility,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourcePolicy {
    pub turn_timeout: Option<Duration>,
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_output_bytes: Option<u64>,
    pub max_event_bytes: Option<u64>,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            turn_timeout: None,
            max_turns: None,
            max_tool_calls: None,
            max_output_bytes: None,
            max_event_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub harness: HarnessType,
    pub backend: BackendInfo,
    pub state: SessionState,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
    pub workspace: WorkspaceInfo,
    pub security: SecurityOptions,
    pub permissions: PermissionInfo,
    pub visibility: VisibilityInfo,
    pub runtime: RuntimeInfo,
    pub auth: AuthInfo,
    pub model: ModelInfo,
    pub capabilities: CapabilityReport,
    pub external_ids: ExternalSessionIds,
    pub usage: Option<Usage>,
    pub turns: Vec<TurnInfo>,
    pub metadata: BTreeMap<String, String>,
}

pub type SessionState = Status;
pub type FinishInfo = Finish;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthInfo {
    pub state: AuthState,
    pub provider: Option<String>,
    pub account: Option<AccountRef>,
    pub organization: Option<String>,
    pub profile: Option<String>,
    pub credential_source: CredentialSource,
    pub source: ObservationSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthState {
    Authenticated,
    Unauthenticated,
    Expired,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub provider: String,
    pub subject: Option<String>,
    pub display_name: Option<String>,
    pub redacted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CredentialSource {
    Environment,
    File,
    Agent,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub estimated_cost: Option<DecimalAmount>,
    pub source: ObservationSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalAmount {
    pub value: String,
    pub currency: String,
    pub estimated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EnvValuePolicy {
    Inherit,
    Allow,
    #[default]
    Redact,
    Deny,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    Disabled,
    Restricted,
    Unrestricted,
    #[default]
    ProviderDefault,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StderrPolicy {
    Ignore,
    #[default]
    Diagnostic,
    Event,
    Inherit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessGroupPolicy {
    Isolated,
    Inherit,
    #[default]
    ProviderDefault,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PathVisibility {
    Absolute,
    WorkspaceRelative,
    Redacted,
    #[default]
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareAudience {
    #[default]
    None,
    CurrentUser,
    WorkspaceUsers,
    ExplicitUsers,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditDescriptor {
    pub enabled: bool,
    pub mode: AuditMode,
    pub correlation_id: Option<String>,
    pub event_sequence_complete: TriState,
    pub redaction_ruleset: Option<String>,
    pub retention: RetentionPolicy,
}

impl Default for AuditDescriptor {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: AuditMode::None,
            correlation_id: None,
            event_sequence_complete: TriState::Unknown,
            redaction_ruleset: None,
            retention: RetentionPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuditMode {
    #[default]
    None,
    EventsOnly,
    DecisionsAndEvents,
    AppendOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RetentionPolicy {
    #[default]
    None,
    Session,
    Indefinite,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObservationSource {
    UserConfig,
    ProviderProtocol,
    ProcessInspection,
    ToolExecution,
    ToolInputInference,
    TextInference,
    channelDefault,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub source: ObservationSource,
    pub confidence: Confidence,
    pub observed_at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityReport {
    pub declared: CapabilitySet,
    pub effective: CapabilitySet,
    pub observed: CapabilitySet,
    pub unsupported: CapabilitySet,
    pub unknown: CapabilitySet,
    pub evidence: Vec<CapabilityEvidence>,
}

impl Default for CapabilityReport {
    fn default() -> Self {
        Self {
            declared: CapabilitySet::default(),
            effective: CapabilitySet::default(),
            observed: CapabilitySet::default(),
            unsupported: CapabilitySet::default(),
            unknown: CapabilitySet::all(),
            evidence: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    pub streaming_events: bool,
    pub structured_text: bool,
    pub reasoning_summary: bool,
    pub tool_calls: bool,
    pub tool_inputs: bool,
    pub tool_outputs: bool,
    pub file_reads: bool,
    pub file_writes: bool,
    pub file_deletes: bool,
    pub command_execution: bool,
    pub network_access_control: bool,
    pub permission_requests: bool,
    pub permission_responses: bool,
    pub turn_cancel: bool,
    pub session_close: bool,
    pub provider_persistence: bool,
    pub provider_resume: bool,
    pub native_ui_discovery: bool,
    pub native_ui_attach: bool,
    pub session_history: bool,
    pub raw_events: bool,
}

impl CapabilitySet {
    pub const fn all() -> Self {
        Self {
            streaming_events: true,
            structured_text: true,
            reasoning_summary: true,
            tool_calls: true,
            tool_inputs: true,
            tool_outputs: true,
            file_reads: true,
            file_writes: true,
            file_deletes: true,
            command_execution: true,
            network_access_control: true,
            permission_requests: true,
            permission_responses: true,
            turn_cancel: true,
            session_close: true,
            provider_persistence: true,
            provider_resume: true,
            native_ui_discovery: true,
            native_ui_attach: true,
            session_history: true,
            raw_events: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityEvidence {
    pub capability: Capability,
    pub source: ObservationSource,
    pub confidence: Confidence,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    StreamingEvents,
    StructuredText,
    ReasoningSummary,
    ToolCalls,
    ToolInputs,
    ToolOutputs,
    FileReads,
    FileWrites,
    FileDeletes,
    CommandExecution,
    NetworkAccessControl,
    PermissionRequests,
    PermissionResponses,
    TurnCancel,
    SessionClose,
    ProviderPersistence,
    ProviderResume,
    NativeUiDiscovery,
    NativeUiAttach,
    SessionHistory,
    RawEvents,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnInfo {
    pub turn_id: String,
    pub session_id: String,
    pub user_message: String,
    pub status: Status,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub text: String,
    pub reasoning_summary: String,
    pub tool_calls: Vec<ToolCall>,
    pub files_read: Vec<FileRead>,
    pub files_changed: Vec<FileChange>,
    pub finish: Option<FinishInfo>,
    pub usage: Option<Usage>,
    pub termination: Option<TerminationInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub tool_id: Option<String>,
    pub status: ToolStatus,
    pub diff_summary: Option<String>,
    pub source: ObservationSource,
    pub confidence: Confidence,
    pub sequence: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminationInfo {
    pub reason: TerminationReason,
    pub provider_reason: Option<String>,
    pub error: Option<StructuredError>,
    pub source: ObservationSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationReason {
    Completed,
    Interrupted,
    PermissionDenied,
    ProviderError,
    ProcessExited,
    Timeout,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuredError {
    pub kind: String,
    pub message: String,
    pub retryable: bool,
    pub provider_code: Option<String>,
}

/// Lifecycle state of a session or turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Starting,
    Idle,
    Running,
    WaitingForPermission,
    Completed,
    Interrupted,
    Failed,
    Closed,
    #[default]
    Unknown,
}

impl Status {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Failed | Self::Closed
        )
    }
}

/// Lifecycle state of a tool call or file read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Requested,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

/// Status alias for callers that want to name the file-read state explicitly.
pub type FileReadStatus = ToolStatus;

/// Where a file path came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileReadSource {
    Protocol,
    ToolInput,
    Unknown,
}

/// Capabilities observed or declared by a session's current turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub streaming_events: bool,
    pub tool_calls: bool,
    pub tool_inputs: bool,
    pub tool_outputs: bool,
    pub file_reads: bool,
    pub inferred_file_reads: bool,
}

impl Capabilities {
    pub fn supports_streaming_events(self) -> bool {
        self.streaming_events
    }

    pub fn supports_tool_calls(self) -> bool {
        self.tool_calls
    }

    pub fn supports_tool_inputs(self) -> bool {
        self.tool_inputs
    }

    pub fn supports_tool_outputs(self) -> bool {
        self.tool_outputs
    }

    pub fn supports_file_reads(self) -> bool {
        self.file_reads
    }

    pub fn supports_inferred_file_reads(self) -> bool {
        self.inferred_file_reads
    }
}

/// A normalized tool invocation. Updates for the same non-empty ID are merged by Session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub status: ToolStatus,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub sequence: usize,
}

impl ToolCall {
    pub fn new(id: Option<String>, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            status: ToolStatus::Requested,
            input: None,
            output: None,
            error: None,
            sequence: 0,
        }
    }
}

/// A normalized observation that a tool read a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRead {
    pub path: String,
    pub tool_id: Option<String>,
    pub status: FileReadStatus,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub summary: Option<String>,
    pub source: FileReadSource,
    pub confidence: Confidence,
    pub sequence: usize,
}

impl FileRead {
    pub fn new(path: impl Into<String>, tool_id: Option<String>) -> Self {
        Self {
            path: path.into(),
            tool_id,
            status: ToolStatus::Requested,
            line_start: None,
            line_end: None,
            summary: None,
            source: FileReadSource::Unknown,
            confidence: Confidence::Unknown,
            sequence: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    /// Legacy coarse-grained tool event retained for API compatibility.
    Tool {
        id: Option<String>,
        name: String,
        detail: Option<String>,
    },
    ToolCall(ToolCall),
    FileRead(FileRead),
    FileChange(FileChange),
    PermissionRequired {
        id: String,
        detail: String,
    },
    Status(Status),
    Error(String),
    Finished(Finish),
    Raw(serde_json::Value),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finish {
    pub status: Status,
    pub text: String,
}

impl Finish {
    pub(crate) fn new(status: Status, text: impl Into<String>) -> Self {
        Self {
            status,
            text: text.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    UnsupportedHarness(HarnessType),
    Initialization(String),
    InvalidConfig(String),
    PermissionDenied(String),
    UnsupportedCapability(String),
    ProviderRejected(String),
    ProtocolError(String),
    ProcessExited(String),
    Timeout,
    NotVisible(String),
    Busy,
    Closed,
    NoActiveTurn,
    Backend(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedHarness(harness) => write!(f, "unsupported harness: {harness:?}"),
            Self::Initialization(message) => write!(f, "harness initialization failed: {message}"),
            Self::InvalidConfig(message) => write!(f, "invalid configuration: {message}"),
            Self::PermissionDenied(message) => write!(f, "permission denied: {message}"),
            Self::UnsupportedCapability(capability) => {
                write!(f, "unsupported capability: {capability}")
            }
            Self::ProviderRejected(message) => write!(f, "provider rejected request: {message}"),
            Self::ProtocolError(message) => write!(f, "protocol error: {message}"),
            Self::ProcessExited(message) => write!(f, "process exited: {message}"),
            Self::Timeout => f.write_str("operation timed out"),
            Self::NotVisible(message) => write!(f, "session is not visible: {message}"),
            Self::Busy => f.write_str("the current turn is still running"),
            Self::Closed => f.write_str("the session is closed"),
            Self::NoActiveTurn => f.write_str("the session has no active turn"),
            Self::Backend(message) => write!(f, "backend error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
