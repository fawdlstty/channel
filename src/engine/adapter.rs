//! Session assembly for the engine backend: turns a [`SessionConfig`]
//! carrying `BackendSpec::Engine` into a live [`Session`] (the same role
//! `harness::cli` plays for CLI harnesses).
//!
//! Provider options (JSON keys on `SessionConfig.provider_options`):
//! - `engine_client_state` — serialized [`ChatCompletionsClient`] from a
//!   previous session (resume path; restores the full message log).
//! - `engine_system_prompt` — optional system prompt for new sessions.
//! - `engine_context_window` / `engine_max_output_tokens` — compaction
//!   watermark inputs (0 = unknown disables compaction).
//! - `engine_tools` — optional extra tool specs (JSON array of
//!   `{name, description, parameters}`) beyond the built-in surface.

use serde_json::Value;

use crate::llm::ToolSpec;
use crate::protocol::{BackendSpec, Error, SessionConfig};

use super::EngineBackend;

/// Builds the engine session and (when present) opens the first turn.
pub(crate) async fn create_session_with_config(
    config: SessionConfig,
    first_message: String,
    attachments: Vec<String>,
) -> Result<crate::session::Session, Error> {
    let provider = match &config.backend {
        BackendSpec::Engine { provider } => provider.clone(),
        _ => {
            return Err(Error::InvalidConfig(
                "engine backend requires BackendSpec::Engine".to_owned(),
            ))
        }
    };
    let workspace = config.workspace_cwd().to_path_buf();
    let permission_mode = config.permission_mode().map(str::to_owned);
    let options = &config.provider_options;
    let initial_client_state = options
        .get("engine_client_state")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let system_prompt = options
        .get("engine_system_prompt")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let context_window = options
        .get("engine_context_window")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let max_output_tokens = options
        .get("engine_max_output_tokens")
        .and_then(Value::as_u64);
    let available_tools: Vec<ToolSpec> = options
        .get("engine_tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<ToolSpec>(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    let backend = EngineBackend::new(
        config.kind.clone(),
        &provider,
        workspace,
        permission_mode.as_deref(),
        system_prompt,
        initial_client_state,
        super::global_automation_store(),
        available_tools,
        context_window,
        max_output_tokens,
    )
    .await?;

    // New sessions take the requested model; restored ones already carry
    // it inside the serialized state (setting it again is harmless).
    if let Some(model) = config.model.requested.clone() {
        backend.configure_model(model).await?;
    }

    let mut session = crate::session::Session::with_backend_config(config, None, Box::new(backend), None);
    if !first_message.is_empty() {
        session
            .send_with_attachments(first_message, attachments, crate::session::SendMode::Immediate)
            .await?;
    }
    Ok(session)
}
