use crate::protocol::{
    BackendInfo, BackendKind, Capability, CapabilityEvidence, CapabilityReport, CapabilitySet,
    Confidence, EndpointRequest, Error, HarnessKind, ObservationSource, ProtocolInfo, RuntimeInfo,
    TransportKind,
};
use crate::utils::path::CommandPath;
use crate::utils::websocket::LocalWebSocket;
use std::path::{Path, PathBuf};

pub(crate) struct HarnessDiscovery {
    pub backend: BackendInfo,
    pub runtime: RuntimeInfo,
    pub capabilities: CapabilityReport,
    pub available_models: Vec<String>,
}

impl HarnessDiscovery {
    pub(crate) fn resolve_cwd(cwd: Option<&Path>) -> Result<PathBuf, Error> {
        let cwd = match cwd {
            Some(cwd) => cwd.to_path_buf(),
            None => {
                std::env::current_dir().map_err(|error| Error::Initialization(error.to_string()))?
            }
        };
        if !cwd.is_dir() {
            return Err(Error::Initialization(format!(
                "workspace is not a directory: {}",
                cwd.display()
            )));
        }
        Ok(cwd)
    }

    pub(crate) fn resolve_executable(
        requested: Option<&Path>,
        command: &str,
        cwd: Option<&Path>,
    ) -> Result<PathBuf, Error> {
        if let Some(requested) = requested {
            if requested.is_absolute() || requested.components().count() > 1 {
                let path = match cwd {
                    Some(cwd) if requested.is_relative() => cwd.join(requested),
                    _ => requested.to_path_buf(),
                };
                if path.is_file() {
                    return Ok(path);
                }
                return Err(Error::Initialization(format!(
                    "configured executable is unavailable: {}",
                    path.display()
                )));
            }
            if let Some(path) = requested.to_string_lossy().as_ref().find_in_path() {
                return Ok(path);
            }
            return Err(Error::Initialization(format!(
                "configured executable was not found on PATH: {}",
                requested.display()
            )));
        }

        command.find_in_path().ok_or_else(|| {
            Error::Initialization(format!(
                "harness executable was not found on PATH: {command}"
            ))
        })
    }

    pub(crate) fn discover(
        kind: &HarnessKind,
        backend_kind: BackendKind,
        command: &str,
        args: &[String],
        executable: PathBuf,
        transport: TransportKind,
        protocol: Option<ProtocolInfo>,
    ) -> HarnessDiscovery {
        let source = if transport == TransportKind::Stdio {
            ObservationSource::ProcessInspection
        } else {
            ObservationSource::ProviderProtocol
        };
        let backend = BackendInfo {
            kind: backend_kind,
            command: Some(command.to_owned()),
            transport,
            protocol: protocol.clone(),
            version: None,
            source,
        };
        let runtime = RuntimeInfo {
            command: Some(command.to_owned()),
            args: args.to_vec(),
            executable_path: Some(executable),
            protocol,
            host_os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            source,
            ..RuntimeInfo::default()
        };
        let declared = backend_kind.supported_capabilities(kind);
        HarnessDiscovery {
            capabilities: CapabilityReport::discovered(declared, source),
            backend,
            runtime,
            available_models: Vec::new(),
        }
    }
}

impl CapabilityReport {
    pub(crate) fn discovered(
        declared: CapabilitySet,
        source: ObservationSource,
    ) -> CapabilityReport {
        let evidence = [
            (Capability::StreamingEvents, declared.streaming_events),
            (Capability::StructuredText, declared.structured_text),
            (Capability::ReasoningSummary, declared.reasoning_summary),
            (Capability::ToolCalls, declared.tool_calls),
            (Capability::ToolInputs, declared.tool_inputs),
            (Capability::ToolOutputs, declared.tool_outputs),
            (Capability::FileReads, declared.file_reads),
            (Capability::FileWrites, declared.file_writes),
            (Capability::FileDeletes, declared.file_deletes),
            (Capability::CommandExecution, declared.command_execution),
            (
                Capability::NetworkAccessControl,
                declared.network_access_control,
            ),
            (Capability::PermissionRequests, declared.permission_requests),
            (
                Capability::PermissionResponses,
                declared.permission_responses,
            ),
            (Capability::TurnCancel, declared.turn_cancel),
            (Capability::SessionClose, declared.session_close),
            (
                Capability::ProviderPersistence,
                declared.provider_persistence,
            ),
            (Capability::ProviderResume, declared.provider_resume),
            (Capability::NativeUiDiscovery, declared.native_ui_discovery),
            (Capability::NativeUiAttach, declared.native_ui_attach),
            (Capability::SessionHistory, declared.session_history),
            (Capability::RawEvents, declared.raw_events),
        ]
        .into_iter()
        .filter(|(_, supported)| *supported)
        .map(|(capability, _)| CapabilityEvidence {
            capability,
            source,
            confidence: Confidence::High,
            detail: Some("reported during harness initialization".to_owned()),
        })
        .collect();

        CapabilityReport {
            declared,
            effective: declared,
            observed: CapabilitySet::default(),
            unsupported: CapabilitySet::default(),
            unknown: declared.complement(),
            evidence,
        }
    }
}

impl CapabilitySet {
    fn complement(self) -> CapabilitySet {
        let all = CapabilitySet::all();
        let mut unknown = CapabilitySet::default();
        macro_rules! complement_field {
            ($field:ident) => {
                unknown.$field = all.$field && !self.$field;
            };
        }
        complement_field!(streaming_events);
        complement_field!(structured_text);
        complement_field!(reasoning_summary);
        complement_field!(tool_calls);
        complement_field!(tool_inputs);
        complement_field!(tool_outputs);
        complement_field!(file_reads);
        complement_field!(file_writes);
        complement_field!(file_deletes);
        complement_field!(command_execution);
        complement_field!(network_access_control);
        complement_field!(permission_requests);
        complement_field!(permission_responses);
        complement_field!(turn_cancel);
        complement_field!(session_close);
        complement_field!(provider_persistence);
        complement_field!(provider_resume);
        complement_field!(native_ui_discovery);
        complement_field!(native_ui_attach);
        complement_field!(session_history);
        complement_field!(raw_events);
        unknown
    }
}

impl EndpointRequest {
    pub(crate) fn codex_endpoint(&self, port: Option<u16>) -> Result<Option<String>, Error> {
        if port.is_some() && !matches!(self, EndpointRequest::Auto) {
            return Err(Error::InvalidConfig(
                "endpoint and port cannot both be specified".to_owned(),
            ));
        }
        match (self, port) {
            (EndpointRequest::Explicit(endpoint), _) => endpoint
                .normalize_local_websocket()
                .map(Some)
                .map_err(Error::InvalidConfig),
            (EndpointRequest::Auto, Some(port)) => format!("ws://127.0.0.1:{port}")
                .normalize_local_websocket()
                .map(Some)
                .map_err(Error::InvalidConfig),
            (EndpointRequest::Auto, None) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn codex_endpoint_rejects_conflicting_connection_inputs() {
        assert!(
            crate::protocol::EndpointRequest::Explicit("ws://localhost:4500".to_owned())
                .codex_endpoint(Some(4501))
                .is_err()
        );
    }
}
