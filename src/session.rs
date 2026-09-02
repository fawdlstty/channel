use crate::protocol::{
    AuthInfo, AuthState, BackendInfo, BackendKind, Capabilities, CapabilityReport, CapabilitySet,
    Confidence, Error, Event, FileChange, FileRead, FileReadSource, Finish, HarnessRuntime,
    HarnessType, ModelInfo, ObservationSource, PermissionInfo, Resumability, RuntimeInfo,
    SessionConfig, SessionInfo, Shareability, Status, ToolCall, ToolStatus, TransportKind,
    TurnInfo, VisibilityInfo, VisibilityState, WorkspaceInfo,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub(crate) type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) trait Backend: Send {
    fn id(&self) -> Option<String> {
        None
    }
    fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()>;
    fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>>;
    fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()>;
    fn close<'a>(&'a mut self) -> BackendFuture<'a, ()>;
}

pub type ActivityId = String;
pub type ActivityStatus = ToolStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Tool,
    ReadFile,
    WriteFile,
    RunCommand,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEvent {
    pub id: ActivityId,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    pub display: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityView {
    pub id: ActivityId,
    pub kind: ActivityKind,
    pub status: ActivityStatus,
    pub display: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityCounts {
    pub total: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivitySummary {
    pub items: Vec<ActivityView>,
    pub counts: ActivityCounts,
    pub display: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageEvent {
    pub text: String,
    pub reasoning: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionEvent {
    pub id: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Activity(ActivityEvent),
    Message(MessageEvent),
    Permission(PermissionEvent),
    Finished(Finish),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMode {
    Immediate,
    Wait,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendResult {
    SendAccepted { turn_id: String },
    Finished(Finish),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptAction {
    Continue,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptResult {
    Continued(Finish),
    Ended(Finish),
}

pub struct Session {
    id: Option<String>,
    status: Status,
    backend: Box<dyn Backend>,
    active_turn: bool,
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCall>,
    files_read: Vec<FileRead>,
    files_changed: Vec<FileChange>,
    capabilities: Capabilities,
    next_sequence: usize,
    finish: Option<Finish>,
    session_info: SessionInfo,
    turns: Vec<TurnInfo>,
}

fn activity_kind(name: &str) -> ActivityKind {
    let name = name.to_ascii_lowercase();
    if name.contains("agent") {
        ActivityKind::Agent
    } else if name.contains("read") || name.contains("cat") || name.contains("list") {
        ActivityKind::ReadFile
    } else if name.contains("write") || name.contains("edit") || name.contains("patch") {
        ActivityKind::WriteFile
    } else if name.contains("command") || name.contains("shell") || name.contains("exec") {
        ActivityKind::RunCommand
    } else {
        ActivityKind::Tool
    }
}

fn activity_display(name: &str, detail: Option<&str>) -> String {
    detail
        .filter(|detail| !detail.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| name.to_owned())
}

fn activity_for_tool(call: &ToolCall) -> ActivityEvent {
    ActivityEvent {
        id: call
            .id
            .clone()
            .unwrap_or_else(|| format!("activity-{}", call.sequence)),
        kind: activity_kind(&call.name),
        status: call.status,
        display: activity_display(&call.name, call.output.as_deref()),
    }
}

fn activity_for_file_read(file: &FileRead) -> ActivityEvent {
    ActivityEvent {
        id: file
            .tool_id
            .clone()
            .unwrap_or_else(|| format!("activity-{}", file.sequence)),
        kind: ActivityKind::ReadFile,
        status: file.status,
        display: format!("Read {}", file.path),
    }
}

fn activity_for_file_change(change: &FileChange) -> ActivityEvent {
    ActivityEvent {
        id: change
            .tool_id
            .clone()
            .unwrap_or_else(|| format!("activity-{}", change.sequence)),
        kind: ActivityKind::WriteFile,
        status: change.status,
        display: format!("Changed {}", change.path),
    }
}

fn backend_kind(harness: &HarnessType, backend: &crate::protocol::BackendSpec) -> BackendKind {
    match backend {
        crate::protocol::BackendSpec::CodexAppServer { .. } => BackendKind::CodexAppServer,
        crate::protocol::BackendSpec::Acp { .. } => BackendKind::Acp,
        crate::protocol::BackendSpec::StructuredCli { .. } => BackendKind::StructuredCli,
        crate::protocol::BackendSpec::PlainCli { .. } => BackendKind::PlainCli,
        crate::protocol::BackendSpec::Custom { .. } => BackendKind::Custom,
        crate::protocol::BackendSpec::Auto => match harness {
            HarnessType::Codex => BackendKind::CodexAppServer,
            HarnessType::OpenCode
            | HarnessType::ZedAcp
            | HarnessType::ZCode
            | HarnessType::DeepSeek => BackendKind::Acp,
            HarnessType::Custom { .. } => BackendKind::Custom,
            _ => BackendKind::PlainCli,
        },
    }
}

fn observed_capabilities(capabilities: Capabilities, reasoning_summary: bool) -> CapabilitySet {
    CapabilitySet {
        streaming_events: capabilities.streaming_events,
        structured_text: capabilities.streaming_events,
        reasoning_summary,
        tool_calls: capabilities.tool_calls,
        tool_inputs: capabilities.tool_inputs,
        tool_outputs: capabilities.tool_outputs,
        file_reads: capabilities.file_reads,
        ..CapabilitySet::default()
    }
}

fn unknown_capabilities(capabilities: Capabilities, reasoning_summary: bool) -> CapabilitySet {
    let observed = observed_capabilities(capabilities, reasoning_summary);
    CapabilitySet {
        streaming_events: !observed.streaming_events,
        structured_text: !observed.structured_text,
        reasoning_summary: !observed.reasoning_summary,
        tool_calls: !observed.tool_calls,
        tool_inputs: !observed.tool_inputs,
        tool_outputs: !observed.tool_outputs,
        file_reads: !observed.file_reads,
        ..CapabilitySet::all()
    }
}

fn backend_command(
    backend: &crate::protocol::BackendSpec,
    harness: &HarnessType,
) -> Option<String> {
    match backend {
        crate::protocol::BackendSpec::CodexAppServer { command, .. }
        | crate::protocol::BackendSpec::Acp { command, .. }
        | crate::protocol::BackendSpec::StructuredCli { command, .. }
        | crate::protocol::BackendSpec::PlainCli { command, .. }
        | crate::protocol::BackendSpec::Custom { command, .. } => Some(command.clone()),
        crate::protocol::BackendSpec::Auto => match harness {
            HarnessType::Codex => Some("codex".to_owned()),
            HarnessType::OpenCode => Some("opencode".to_owned()),
            HarnessType::ZedAcp => Some("zed".to_owned()),
            HarnessType::ZCode => Some("zcode".to_owned()),
            HarnessType::DeepSeek => Some("deepseek".to_owned()),
            HarnessType::ClaudeCode => Some("claude".to_owned()),
            HarnessType::Aider => Some("aider".to_owned()),
            HarnessType::Goose => Some("goose".to_owned()),
            HarnessType::Cline => Some("cline".to_owned()),
            HarnessType::RooCode => Some("roo".to_owned()),
            HarnessType::OpenHands => Some("openhands".to_owned()),
            HarnessType::SweAgent => Some("sweagent".to_owned()),
            HarnessType::GeminiCli => Some("gemini".to_owned()),
            HarnessType::Continue => Some("cn".to_owned()),
            HarnessType::Custom { command, .. } => Some(command.clone()),
        },
    }
}

fn backend_args(backend: &crate::protocol::BackendSpec, harness: &HarnessType) -> Vec<String> {
    match backend {
        crate::protocol::BackendSpec::CodexAppServer { args, .. }
        | crate::protocol::BackendSpec::Acp { args, .. }
        | crate::protocol::BackendSpec::StructuredCli { args, .. }
        | crate::protocol::BackendSpec::PlainCli { args, .. }
        | crate::protocol::BackendSpec::Custom { args, .. } => args.clone(),
        crate::protocol::BackendSpec::Auto => match harness {
            HarnessType::Codex => vec!["app-server".to_owned(), "--stdio".to_owned()],
            HarnessType::OpenCode
            | HarnessType::ZedAcp
            | HarnessType::ZCode
            | HarnessType::DeepSeek => vec!["acp".to_owned()],
            _ => Vec::new(),
        },
    }
}

impl Session {
    pub(crate) fn with_backend_config(
        config: SessionConfig,
        id: Option<String>,
        backend: Box<dyn Backend>,
        initialized: Option<HarnessRuntime>,
    ) -> Self {
        let harness = config.harness.clone();
        let session_id = format!(
            "session-{}",
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        );
        let external_ids = crate::protocol::ExternalSessionIds {
            channel_session_id: session_id.clone(),
            provider_session_id: id.clone(),
            provider_thread_id: None,
            provider_turn_id: None,
            transport_connection_id: None,
            process_id: None,
        };
        let now = SystemTime::now();
        let mut session_info = SessionInfo {
            session_id: session_id.clone(),
            harness: harness.clone(),
            backend: BackendInfo {
                kind: backend_kind(&harness, &config.backend),
                command: backend_command(&config.backend, &harness),
                transport: TransportKind::Stdio,
                protocol: None,
                version: None,
                source: ObservationSource::Unknown,
            },
            state: Status::Idle,
            created_at: now,
            updated_at: now,
            workspace: WorkspaceInfo {
                requested_cwd: config.workspace.cwd.clone(),
                effective_cwd: std::fs::metadata(&config.workspace.cwd)
                    .ok()
                    .filter(|m| m.is_dir())
                    .map(|_| config.workspace.cwd.clone()),
                canonical_cwd: std::fs::canonicalize(&config.workspace.cwd).ok(),
                roots: config
                    .workspace
                    .roots
                    .iter()
                    .map(|root| crate::protocol::EffectiveRoot {
                        path: root.path.clone(),
                        access: root.access,
                        label: root.label.clone(),
                        source: ObservationSource::UserConfig,
                    })
                    .collect(),
                repository: None,
                path_visibility: crate::protocol::PathVisibility::Absolute,
                source: ObservationSource::UserConfig,
            },
            security: config.security.clone(),
            permissions: PermissionInfo {
                requested: config.security.permissions.clone(),
                effective: None,
                confidence: Confidence::Unknown,
                source: ObservationSource::Unknown,
                limitations: Vec::new(),
            },
            visibility: VisibilityInfo {
                requested: config.observability.visibility.clone(),
                channel: VisibilityState::Yes,
                provider: VisibilityState::Unknown,
                native_ui: VisibilityState::Unknown,
                resumability: Resumability::InMemoryOnly,
                shareability: Shareability::No,
                account: None,
                external_ids: external_ids.clone(),
                source: ObservationSource::channelDefault,
                limitations: Vec::new(),
            },
            runtime: RuntimeInfo {
                command: backend_command(&config.backend, &harness),
                args: backend_args(&config.backend, &harness),
                executable_path: config.runtime.process.executable.clone(),
                ..RuntimeInfo::default()
            },
            auth: AuthInfo {
                state: AuthState::Unknown,
                provider: None,
                account: None,
                organization: None,
                profile: None,
                credential_source: crate::protocol::CredentialSource::Unknown,
                source: ObservationSource::Unknown,
            },
            model: ModelInfo {
                requested: config.model.requested.clone(),
                effective: None,
                provider: None,
                context_window: None,
                source: ObservationSource::Unknown,
            },
            capabilities: CapabilityReport::default(),
            external_ids,
            usage: None,
            turns: Vec::new(),
            metadata: config.metadata.clone(),
        };
        if let Some(initialized) = initialized {
            session_info.backend = initialized.backend;
            session_info.runtime = initialized.runtime;
            session_info.capabilities = initialized.capabilities;
        }
        Self {
            id,
            status: Status::Idle,
            backend,
            active_turn: false,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            capabilities: Capabilities::default(),
            next_sequence: 0,
            finish: None,
            session_info,
            turns: Vec::new(),
        }
    }

    pub async fn send(
        &mut self,
        message: impl Into<String>,
        mode: SendMode,
    ) -> Result<SendResult, Error> {
        if self.status == Status::Closed {
            return Err(Error::Closed);
        }
        if self.active_turn {
            return Err(Error::Busy);
        }

        let message = message.into();
        self.text.clear();
        self.reasoning.clear();
        self.tool_calls.clear();
        self.files_read.clear();
        self.files_changed.clear();
        self.capabilities = Capabilities::default();
        self.next_sequence = 0;
        self.finish = None;
        self.active_turn = true;
        self.status = Status::Running;
        let turn_id = format!(
            "{}:turn-{}",
            self.session_info.session_id,
            self.turns.len() + 1
        );
        self.turns.push(TurnInfo {
            turn_id: turn_id.clone(),
            session_id: self.session_info.session_id.clone(),
            user_message: message.clone(),
            status: self.status,
            started_at: SystemTime::now(),
            finished_at: None,
            text: String::new(),
            reasoning_summary: String::new(),
            tool_calls: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            finish: None,
            usage: None,
            termination: None,
        });
        self.touch_info();

        if let Err(error) = self.backend.send(message).await {
            self.fail(error.to_string());
            return Err(error);
        }

        match mode {
            SendMode::Immediate => Ok(SendResult::SendAccepted { turn_id }),
            SendMode::Wait => {
                while self.active_turn {
                    self.wait_event().await?;
                }
                self.finish
                    .clone()
                    .map(SendResult::Finished)
                    .ok_or_else(|| Error::Backend("turn ended without a final status".to_owned()))
            }
        }
    }

    pub async fn wait_event(&mut self) -> Result<Option<SessionEvent>, Error> {
        if self.status == Status::Closed {
            return Ok(None);
        }
        if !self.active_turn {
            if self.finish.is_some() {
                return Ok(None);
            }
            return Err(Error::NoActiveTurn);
        }

        loop {
            let event = match self.backend.next_event().await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    if !self.active_turn {
                        return Ok(None);
                    }
                    let message = "backend ended before a turn completion event".to_owned();
                    self.fail(message.clone());
                    return Err(Error::Backend(message));
                }
                Err(error) => {
                    self.fail(error.to_string());
                    return Err(error);
                }
            };

            self.apply(&event);
            if self.id.is_none() {
                self.id = self.backend.id();
                if self.session_info.external_ids.provider_session_id.is_none() {
                    self.session_info.external_ids.provider_session_id = self.id.clone();
                }
            }
            self.touch_info();

            if let Event::Error(message) = &event {
                return Err(Error::Backend(message.clone()));
            }
            if let Some(event) = self.session_event(&event) {
                return Ok(Some(event));
            }
        }
    }

    pub fn state(&self) -> Status {
        self.status
    }

    pub fn activity(&self) -> ActivitySummary {
        let mut items = Vec::with_capacity(
            self.tool_calls.len() + self.files_read.len() + self.files_changed.len(),
        );
        items.extend(self.tool_calls.iter().map(|call| {
            ActivityView {
                id: call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("activity-{}", call.sequence)),
                kind: ActivityKind::Tool,
                status: call.status,
                display: activity_display(call.name.as_str(), call.output.as_deref()),
            }
        }));
        items.extend(self.files_read.iter().map(|file| {
            ActivityView {
                id: file
                    .tool_id
                    .clone()
                    .unwrap_or_else(|| format!("activity-{}", file.sequence)),
                kind: ActivityKind::ReadFile,
                status: file.status,
                display: format!("Read {}", file.path),
            }
        }));
        items.extend(self.files_changed.iter().map(|change| {
            ActivityView {
                id: change
                    .tool_id
                    .clone()
                    .unwrap_or_else(|| format!("activity-{}", change.sequence)),
                kind: ActivityKind::WriteFile,
                status: change.status,
                display: format!("Changed {}", change.path),
            }
        }));
        items.sort_by_key(|item| item.id.clone());

        let mut counts = ActivityCounts {
            total: items.len(),
            ..ActivityCounts::default()
        };
        for item in &items {
            match item.status {
                ToolStatus::Requested | ToolStatus::Running => counts.running += 1,
                ToolStatus::Completed => counts.completed += 1,
                ToolStatus::Failed => counts.failed += 1,
                ToolStatus::Cancelled => counts.cancelled += 1,
                ToolStatus::Unknown => {}
            }
        }
        let display = items
            .iter()
            .map(|item| item.display.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        ActivitySummary {
            items,
            counts,
            display,
        }
    }

    pub fn result(&self) -> Option<&Finish> {
        self.finish.as_ref()
    }

    pub fn info(&self) -> &SessionInfo {
        &self.session_info
    }

    pub fn capabilities(&self) -> &CapabilityReport {
        &self.session_info.capabilities
    }

    pub async fn refresh_info(&mut self) -> Result<(), Error> {
        if self.status == Status::Closed {
            return Err(Error::Closed);
        }
        if self.id.is_none() {
            self.id = self.backend.id();
            if self.session_info.external_ids.provider_session_id.is_none() {
                self.session_info.external_ids.provider_session_id = self.id.clone();
            }
        }
        self.touch_info();
        Ok(())
    }

    pub async fn interrupt(&mut self, action: InterruptAction) -> Result<InterruptResult, Error> {
        if self.status == Status::Closed {
            return Err(Error::Closed);
        }
        if !self.active_turn {
            return Err(Error::NoActiveTurn);
        }

        if let Err(error) = self.backend.interrupt().await {
            self.fail(error.to_string());
            return Err(error);
        }
        let finish = loop {
            match self.wait_event().await {
                Ok(Some(SessionEvent::Finished(finish))) => break finish,
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(Error::Backend(
                        "interrupt ended without completion".to_owned(),
                    ))
                }
                Err(error) => return Err(error),
            }
        };
        if action == InterruptAction::End {
            self.backend.close().await?;
            self.status = Status::Closed;
            self.active_turn = false;
            self.finish = Some(Finish::new(Status::Closed, finish.text.clone()));
            self.touch_info();
            Ok(InterruptResult::Ended(
                self.finish.clone().expect("finish was just set"),
            ))
        } else {
            Ok(InterruptResult::Continued(finish))
        }
    }

    fn session_event(&self, event: &Event) -> Option<SessionEvent> {
        match event {
            Event::TextDelta(text) => Some(SessionEvent::Message(MessageEvent {
                text: text.clone(),
                reasoning: false,
            })),
            Event::ReasoningDelta(text) => Some(SessionEvent::Message(MessageEvent {
                text: text.clone(),
                reasoning: true,
            })),
            Event::Tool { id, name, detail } => Some(SessionEvent::Activity(ActivityEvent {
                id: id
                    .clone()
                    .unwrap_or_else(|| format!("activity-{}", self.next_sequence)),
                kind: ActivityKind::Tool,
                status: ToolStatus::Requested,
                display: activity_display(name, detail.as_deref()),
            })),
            Event::ToolCall(call) => Some(SessionEvent::Activity(activity_for_tool(call))),
            Event::FileRead(file) => Some(SessionEvent::Activity(activity_for_file_read(file))),
            Event::FileChange(change) => {
                Some(SessionEvent::Activity(activity_for_file_change(change)))
            }
            Event::PermissionRequired { id, detail } => {
                Some(SessionEvent::Permission(PermissionEvent {
                    id: id.clone(),
                    detail: detail.clone(),
                }))
            }
            Event::Status(status) => {
                if status.is_terminal() {
                    self.finish.clone().map(SessionEvent::Finished)
                } else {
                    None
                }
            }
            Event::Finished(finish) => Some(SessionEvent::Finished(
                self.finish.clone().unwrap_or_else(|| finish.clone()),
            )),
            Event::Error(_) => None,
            Event::Raw(_) => None,
        }
    }

    fn apply(&mut self, event: &Event) {
        self.capabilities.streaming_events = true;
        match event {
            Event::TextDelta(delta) => self.text.push_str(delta),
            Event::ReasoningDelta(delta) => self.reasoning.push_str(delta),
            Event::Tool { id, name, detail } => {
                self.capabilities.tool_calls = true;
                self.merge_tool_call(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    status: ToolStatus::Requested,
                    input: None,
                    output: detail.clone(),
                    error: None,
                    sequence: 0,
                });
            }
            Event::ToolCall(call) => {
                self.capabilities.tool_calls = true;
                self.capabilities.tool_inputs |= call.input.is_some();
                self.capabilities.tool_outputs |= call.output.is_some() || call.error.is_some();
                self.merge_tool_call(call.clone());
            }
            Event::FileRead(file) => {
                self.capabilities.file_reads = true;
                self.capabilities.inferred_file_reads |= file.source == FileReadSource::ToolInput
                    || matches!(file.confidence, Confidence::Medium | Confidence::Low);
                self.merge_file_read(file.clone());
            }
            Event::FileChange(change) => {
                self.merge_file_change(change.clone());
            }
            Event::PermissionRequired { .. } => self.status = Status::WaitingForPermission,
            Event::Status(status) => self.set_status(*status),
            Event::Finished(finish) => {
                let finish = if finish.text.is_empty() {
                    Finish::new(finish.status, self.text.clone())
                } else {
                    finish.clone()
                };
                self.finish_turn(finish);
            }
            Event::Error(message) => self.fail(message.clone()),
            Event::Raw(_) => {}
        }
        self.touch_info();
    }

    fn merge_tool_call(&mut self, mut incoming: ToolCall) {
        if let Some(id) = incoming.id.as_deref() {
            if let Some(existing) = self
                .tool_calls
                .iter_mut()
                .find(|call| call.id.as_deref() == Some(id))
            {
                if !incoming.name.is_empty() {
                    existing.name = incoming.name;
                }
                if incoming.status != ToolStatus::Unknown {
                    existing.status = incoming.status;
                }
                if incoming.input.is_some() {
                    existing.input = incoming.input;
                }
                if incoming.output.is_some() {
                    existing.output = incoming.output;
                }
                if incoming.error.is_some() {
                    existing.error = incoming.error;
                }
                self.sync_current_turn();
                return;
            }
        }
        incoming.sequence = self.next_sequence;
        self.next_sequence += 1;
        self.tool_calls.push(incoming);
        self.sync_current_turn();
    }

    fn merge_file_read(&mut self, mut incoming: FileRead) {
        if let Some(tool_id) = incoming.tool_id.as_deref() {
            if let Some(existing) = self
                .files_read
                .iter_mut()
                .find(|file| file.tool_id.as_deref() == Some(tool_id) && file.path == incoming.path)
            {
                if !incoming.path.is_empty() {
                    existing.path = incoming.path;
                }
                if incoming.status != ToolStatus::Unknown {
                    existing.status = incoming.status;
                }
                if incoming.line_start.is_some() {
                    existing.line_start = incoming.line_start;
                }
                if incoming.line_end.is_some() {
                    existing.line_end = incoming.line_end;
                }
                if incoming.summary.is_some() {
                    existing.summary = incoming.summary;
                }
                if incoming.source != FileReadSource::Unknown {
                    existing.source = incoming.source;
                }
                if incoming.confidence != Confidence::Unknown {
                    existing.confidence = incoming.confidence;
                }
                return;
            }
        }
        incoming.sequence = self.next_sequence;
        self.next_sequence += 1;
        self.files_read.push(incoming);
        self.sync_current_turn();
    }

    fn merge_file_change(&mut self, mut incoming: FileChange) {
        if let Some(tool_id) = incoming.tool_id.as_deref() {
            if let Some(existing) = self.files_changed.iter_mut().find(|change| {
                change.tool_id.as_deref() == Some(tool_id) && change.path == incoming.path
            }) {
                if incoming.status != ToolStatus::Unknown {
                    existing.status = incoming.status;
                }
                if incoming.diff_summary.is_some() {
                    existing.diff_summary = incoming.diff_summary;
                }
                if incoming.source != ObservationSource::Unknown {
                    existing.source = incoming.source;
                }
                if incoming.confidence != Confidence::Unknown {
                    existing.confidence = incoming.confidence;
                }
                self.sync_current_turn();
                return;
            }
        }
        incoming.sequence = self.next_sequence;
        self.next_sequence += 1;
        self.files_changed.push(incoming);
        self.sync_current_turn();
    }

    fn set_status(&mut self, status: Status) {
        self.status = status;
        if status.is_terminal() {
            self.finish_turn(Finish::new(status, self.text.clone()));
        }
    }

    fn finish_turn(&mut self, finish: Finish) {
        self.status = finish.status;
        self.active_turn = false;
        self.finish = Some(finish.clone());
        self.sync_current_turn();
        if let Some(turn) = self.turns.last_mut() {
            turn.finished_at = Some(SystemTime::now());
            turn.finish = Some(finish);
        }
        self.touch_info();
    }

    fn sync_current_turn(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            turn.status = self.status;
            turn.text = self.text.clone();
            turn.reasoning_summary = self.reasoning.clone();
            turn.tool_calls = self.tool_calls.clone();
            turn.files_read = self.files_read.clone();
            turn.files_changed = self.files_changed.clone();
        }
    }

    fn touch_info(&mut self) {
        self.session_info.state = self.status;
        self.session_info.capabilities.observed =
            observed_capabilities(self.capabilities, !self.reasoning.is_empty());
        self.session_info.capabilities.unknown =
            unknown_capabilities(self.capabilities, !self.reasoning.is_empty());
        self.session_info.visibility.external_ids = self.session_info.external_ids.clone();
        self.session_info.turns = self.turns.clone();
        self.session_info.updated_at = SystemTime::now();
        self.sync_current_turn();
    }

    fn fail(&mut self, _message: String) {
        self.finish_turn(Finish::new(Status::Failed, self.text.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockBackend {
        events: VecDeque<Event>,
        sent: Vec<String>,
    }

    impl MockBackend {
        fn new(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                events: events.into_iter().collect(),
                sent: Vec::new(),
            }
        }
    }

    impl Backend for MockBackend {
        fn send<'a>(&'a mut self, message: String) -> BackendFuture<'a, ()> {
            self.sent.push(message);
            Box::pin(async { Ok(()) })
        }

        fn next_event<'a>(&'a mut self) -> BackendFuture<'a, Option<Event>> {
            let event = self.events.pop_front();
            Box::pin(async move { Ok(event) })
        }

        fn interrupt<'a>(&'a mut self) -> BackendFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn close<'a>(&'a mut self) -> BackendFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    fn session(events: impl IntoIterator<Item = Event>) -> Session {
        Session::with_backend_config(
            SessionConfig::default_for(HarnessType::Codex),
            Some("test-session".to_owned()),
            Box::new(MockBackend::new(events)),
            None,
        )
    }

    #[tokio::test]
    async fn send_wait_aggregates_a_turn_and_can_send_again() {
        let mut session = session([
            Event::TextDelta("你好".to_owned()),
            Event::TextDelta("，主人".to_owned()),
            Event::ReasoningDelta("摘要".to_owned()),
            Event::Finished(Finish::new(Status::Completed, "你好，主人")),
            Event::TextDelta("第二轮".to_owned()),
            Event::Finished(Finish::new(Status::Completed, "第二轮")),
        ]);

        assert_eq!(
            session.send("第一轮", SendMode::Wait).await.unwrap(),
            SendResult::Finished(Finish::new(Status::Completed, "你好，主人"))
        );
        assert_eq!(session.result().unwrap().text, "你好，主人");
        assert_eq!(
            session.send("第二轮", SendMode::Wait).await.unwrap(),
            SendResult::Finished(Finish::new(Status::Completed, "第二轮"))
        );
        assert_eq!(session.state(), Status::Completed);
        assert_eq!(session.info().turns.len(), 2);
    }

    #[tokio::test]
    async fn wait_event_normalizes_messages_and_activity() {
        let mut call = ToolCall::new(Some("tool-1".to_owned()), "read_file");
        call.status = ToolStatus::Completed;
        let mut session = session([
            Event::ToolCall(call),
            Event::FileRead(FileRead::new("src/lib.rs", Some("tool-1".to_owned()))),
            Event::TextDelta("answer".to_owned()),
            Event::Finished(Finish::new(Status::Completed, "answer")),
        ]);
        assert!(matches!(
            session.wait_event().await,
            Err(Error::NoActiveTurn)
        ));
        session.send("读取", SendMode::Immediate).await.unwrap();
        assert!(matches!(
            session.wait_event().await.unwrap(),
            Some(SessionEvent::Activity(_))
        ));
        assert!(matches!(
            session.wait_event().await.unwrap(),
            Some(SessionEvent::Activity(_))
        ));
        assert!(matches!(
            session.wait_event().await.unwrap(),
            Some(SessionEvent::Message(_))
        ));
        assert!(matches!(
            session.wait_event().await.unwrap(),
            Some(SessionEvent::Finished(_))
        ));
        assert!(matches!(session.wait_event().await, Ok(None)));
        assert_eq!(session.activity().counts.total, 2);
    }

    #[tokio::test]
    async fn status_is_suppressed_and_error_is_returned() {
        let mut session = session([
            Event::Status(Status::Running),
            Event::Error("backend failed".to_owned()),
        ]);
        session.send("请求", SendMode::Immediate).await.unwrap();

        assert_eq!(
            session.wait_event().await,
            Err(Error::Backend("backend failed".to_owned()))
        );
        assert!(matches!(session.wait_event().await, Ok(None)));
    }

    #[tokio::test]
    async fn rejects_send_while_busy_and_interrupt_ends() {
        let mut session = session([Event::Finished(Finish::new(Status::Interrupted, "已停止"))]);
        session.send("停止我", SendMode::Immediate).await.unwrap();
        assert_eq!(
            session.send("第二轮", SendMode::Immediate).await,
            Err(Error::Busy)
        );
        assert!(matches!(
            session.interrupt(InterruptAction::End).await,
            Ok(InterruptResult::Ended(_))
        ));
        assert_eq!(session.state(), Status::Closed);
        assert!(matches!(session.wait_event().await, Ok(None)));
    }

    #[tokio::test]
    async fn eof_is_reported_as_backend_error() {
        let mut session = session([]);
        session.send("请求", SendMode::Immediate).await.unwrap();
        assert!(matches!(session.wait_event().await, Err(Error::Backend(_))));
        assert_eq!(session.state(), Status::Failed);
    }
}
