use crate::protocol::{
    BackendKind, BackendSpec, Error, HarnessInit, HarnessKind, ProtocolInfo, SessionConfig,
    TransportKind,
};
use crate::runtime::HarnessDiscovery;
use crate::session::Session;

mod acp;
mod cli;
mod codex;

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
    ) -> Result<HarnessDiscovery, Error> {
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
    ) -> Result<HarnessDiscovery, Error> {
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
    ) -> Result<Session, Error> {
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
