use crate::protocol::{
    BackendInfo, BackendKind, Capability, CapabilityEvidence, CapabilityReport, CapabilitySet,
    Confidence, EndpointRequest, Error, HarnessType, ObservationSource, ProtocolInfo, RuntimeInfo,
    TransportKind,
};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::SystemTime;

pub(crate) struct HarnessDiscovery {
    pub backend: BackendInfo,
    pub runtime: RuntimeInfo,
    pub capabilities: CapabilityReport,
}

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
        if let Some(path) = find_in_path(&requested.to_string_lossy()) {
            return Ok(path);
        }
        return Err(Error::Initialization(format!(
            "configured executable was not found on PATH: {}",
            requested.display()
        )));
    }

    #[cfg(windows)]
    if command.eq_ignore_ascii_case("codex") {
        if let Some(path) = find_chatgpt_codex() {
            return Ok(path);
        }
    }

    find_in_path(command).ok_or_else(|| {
        Error::Initialization(format!(
            "harness executable was not found on PATH: {command}"
        ))
    })
}

#[cfg(windows)]
fn find_chatgpt_codex() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path().join("codex.exe");
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, path));
    }
    candidates.sort_by(|(left_time, _), (right_time, _)| right_time.cmp(left_time));
    candidates.into_iter().map(|(_, path)| path).find(|path| {
        std::process::Command::new(path)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return path.is_file().then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|directory| directory.join(command))
        .find(|path| path.is_file())
}

pub(crate) fn discovery(
    _harness: &HarnessType,
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
    let declared = match backend_kind {
        BackendKind::Acp => crate::acp::supported_capabilities(),
        BackendKind::CodexAppServer => crate::codex::supported_capabilities(),
        _ => crate::cli::supported_capabilities(),
    };
    HarnessDiscovery {
        capabilities: capability_report(declared, source),
        backend,
        runtime,
    }
}

pub(crate) fn capability_report(
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
        unknown: complement(declared),
        evidence,
    }
}

fn complement(capabilities: CapabilitySet) -> CapabilitySet {
    let all = CapabilitySet::all();
    let mut unknown = CapabilitySet::default();
    macro_rules! complement_field {
        ($field:ident) => {
            unknown.$field = all.$field && !capabilities.$field;
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

pub(crate) fn codex_endpoint(
    endpoint: &EndpointRequest,
    port: Option<u16>,
) -> Result<Option<String>, Error> {
    if port.is_some() && !matches!(endpoint, EndpointRequest::Auto) {
        return Err(Error::InvalidConfig(
            "endpoint and port cannot both be specified".to_owned(),
        ));
    }
    match (endpoint, port) {
        (EndpointRequest::Explicit(endpoint), _) => normalize_local_websocket(endpoint)
            .map(Some)
            .map_err(Error::InvalidConfig),
        (EndpointRequest::Auto, Some(port)) => {
            normalize_local_websocket(&format!("ws://127.0.0.1:{port}"))
                .map(Some)
                .map_err(Error::InvalidConfig)
        }
        (EndpointRequest::Auto, None) => Ok(None),
    }
}

pub(crate) fn normalize_local_websocket(endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    let authority = endpoint
        .strip_prefix("ws://")
        .ok_or_else(|| "only local ws:// endpoints are supported".to_owned())?;
    if authority.contains(['@', '/', '?', '#']) {
        return Err("endpoint credentials, paths, and queries are not supported".to_owned());
    }
    let host = if authority == "::1" {
        "[::1]".to_owned()
    } else {
        authority.to_owned()
    };
    let (hostname, port) = if let Some(rest) = host.strip_prefix("[::1]") {
        if rest.is_empty() {
            ("[::1]", None)
        } else {
            let port = rest
                .strip_prefix(':')
                .ok_or_else(|| "invalid endpoint authority".to_owned())?;
            ("[::1]", Some(port))
        }
    } else if let Some((candidate, port)) = host.rsplit_once(':') {
        (candidate, Some(port))
    } else {
        (host.as_str(), None)
    };
    if hostname != "127.0.0.1" && hostname != "[::1]" && hostname != "localhost" {
        return Err("only 127.0.0.1, ::1, and localhost are allowed".to_owned());
    }
    if let Some(port) = port {
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid endpoint port".to_owned())?;
        if port == 0 {
            return Err("endpoint port must be greater than zero".to_owned());
        }
    }
    Ok(format!("ws://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_only_local_websocket_endpoints() {
        assert_eq!(
            normalize_local_websocket(" ws://localhost:4500 ").unwrap(),
            "ws://localhost:4500"
        );
        assert_eq!(normalize_local_websocket("ws://::1").unwrap(), "ws://[::1]");
        assert_eq!(
            normalize_local_websocket("ws://[::1]:4501").unwrap(),
            "ws://[::1]:4501"
        );
    }

    #[test]
    fn rejects_non_local_or_invalid_websocket_endpoints() {
        for endpoint in [
            "http://localhost:4500",
            "ws://example.invalid:4500",
            "ws://localhost:4500/path",
            "ws://user@localhost:4500",
            "ws://localhost:0",
            "ws://localhost:bad",
        ] {
            assert!(normalize_local_websocket(endpoint).is_err());
        }
    }

    #[test]
    fn codex_endpoint_rejects_conflicting_connection_inputs() {
        assert!(codex_endpoint(
            &crate::protocol::EndpointRequest::Explicit("ws://localhost:4500".to_owned()),
            Some(4501)
        )
        .is_err());
    }
}
