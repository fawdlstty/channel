// Installation detection (`detect_harness`) and channel enumeration
// (`list_channels`) for embedding applications (desktop harness managers,
// launchers). Both APIs are strictly read-only: they never spawn a harness
// session and never mutate on-disk configuration.
//
// - `detect_harness` locates the launcher binary of a harness kind on the
//   machine and asks it for a version string.
// - `list_channels` enumerates the selectable (team/provider, model)
//   combinations of one harness kind. ZCode reads the personal provider
//   config (`~/.zcode/v2/provider_config.json`, same file the session layer
//   switches with, so `CHANNEL_ZCODE_PROVIDER_CONFIG` is honored here too);
//   Codex reads `~/.codex/config.toml` plus the cc-switch key set. Both
//   degrade to a single `default` entry on missing/broken configuration
//   instead of failing the caller — a UI should always get at least one
//   channel per harness.
//
// Codex enumeration needs TOML parsing; the `toml` dependency is
// `ccswitch`-gated (see Cargo.toml), so without that feature `list_channels`
// returns the `default` entry for Codex. ZCode enumeration (serde_json) and
// detection work in every feature combination.
//
// Change note (0.3.11): new public API; no existing signatures were modified.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::protocol::{Error, HarnessKind};
use crate::utils::path::CommandPath;

#[cfg(feature = "ccswitch")]
use crate::switch::ccswitch::app_type;

/// Timeout of a single `--version` probe. Harness CLIs answer in well under a
/// second; anything slower is treated as "binary present, version unknown".
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Headless engine shipped with a desktop-installed ZCode. The `zcode` found
/// on `PATH` is the Electron launcher whose `--version` prints diagnostics
/// instead of a product version, so the version is read through this engine
/// (`node <cjs> --version`) whenever it exists.
const ZCODE_ENGINE_CJS: &str = "/opt/ZCode/resources/glm/zcode.cjs";

/// Failure/absence notes. The default build carries no logging facade
/// (`tracing` is `harness`-gated), and stderr never corrupts the stdout
/// JSON-lines protocols this crate speaks.
macro_rules! catalog_note {
    ($($arg:tt)*) => {
        eprintln!("[channel:catalog] {}", format_args!($($arg)*))
    };
}

/// A detected harness installation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectInfo {
    /// Located launcher binary. For ZCode this is the `zcode` found on
    /// `PATH`, or the bundled headless engine (`zcode.cjs`) when the launcher
    /// is absent.
    pub bin_path: PathBuf,
    /// Version reported by the harness (`<bin> --version`, or
    /// `node <zcode.cjs> --version` for the ZCode engine). `None` when the
    /// binary exists but printed nothing recognizable within
    /// [`VERSION_PROBE_TIMEOUT`].
    pub version: Option<String>,
}

/// One selectable (team/provider, model) channel of a harness kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Display name: ZCode provider display name (falls back to the
    /// prefix-stripped provider id), Codex provider display name or section
    /// key, cc-switch key name.
    pub team: String,
    /// Stable identifier of the team/provider: the full ZCode providerId
    /// (e.g. `account:bigmodel-team-coding-plan`), the Codex
    /// `[model_providers.*]` section key, or the cc-switch key.
    pub team_id: String,
    /// Model identifier as the harness expects it (`GLM-5.3-Flash`,
    /// `glm-5.3-flash`, ...).
    pub model: String,
    /// cc-switch key that restores this channel via
    /// [`crate::SessionConfig::set_switch_provider`]. Filled for channels
    /// backed by a cc-switch entry (Codex, ClaudeCode, OpenCode); ZCode
    /// channels switch through `set_model("{team}/{model}")` instead.
    pub switch_key: Option<String>,
}

/// Locates the launcher binary of a harness kind, probing it for a version.
///
/// Returns `Ok(None)` when the harness is not installed. The probe never
/// fails hard: a binary that exists but does not answer `--version` within
/// five seconds is reported with `version: None`. cc-switch is not a harness
/// kind and is not detected here.
pub async fn detect_harness(kind: HarnessKind) -> Result<Option<DetectInfo>, Error> {
    Ok(match kind {
        HarnessKind::ZCode => detect_zcode().await,
        kind => detect_named(command_of(&kind)).await,
    })
}

/// The launcher command of every harness kind (the same names the harness
/// adapters resolve on `PATH`: `HarnessKind::acp_command`, `cli_command`, and
/// the Codex app-server discovery).
fn command_of(kind: &HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Codex => "codex",
        HarnessKind::ClaudeCode => "claude",
        HarnessKind::OpenCode => "opencode",
        HarnessKind::ZedAcp => "zed",
        HarnessKind::ZCode => "zcode",
        HarnessKind::DeepSeek => "deepseek",
        HarnessKind::Hermes => "hermes",
    }
}

/// Enumerates the selectable (team/provider, model) channels of a harness.
///
/// - **ZCode**: providers from `providerOrder` plus the providerIds of
///   `modelConfigRules.providerModelRules`; display names from
///   `providerConfigRules.providerRules[].providerName` (falling back to the
///   prefix-stripped providerId); models from `providerRules` `modelOrder`,
///   then `providerModelRules` modelIds, then `["default"]`.
/// - **Codex**: one entry per `[model_providers.*]` section of
///   `~/.codex/config.toml` (model = top-level `model`), merged with the
///   cc-switch key set (`app_type = "codex"`) when the cc-switch database
///   exists (those entries carry their own model and `switch_key`). Requires
///   the `ccswitch` feature; without it only the fallback entry is produced.
/// - **ClaudeCode**: the cc-switch key set (`app_type = "claude"`); each key
///   is a full `{"env": {...}}` settings set, so one entry per key with the
///   model from its `ANTHROPIC_MODEL` env when present.
/// - **OpenCode**: one entry per (provider, model) of `opencode.json` /
///   `opencode.jsonc` `provider.<id>` sections, merged with the cc-switch key
///   set (`app_type = "opencode"`, whose `settings_config` is the same
///   provider slice shape).
/// - **Every other kind**: a single `default` entry.
///
/// Missing or broken configuration degrades to a single
/// `ChannelInfo { default, default, default, None }` (the reason is printed
/// to stderr), so callers always receive at least one channel.
pub async fn list_channels(kind: HarnessKind) -> Result<Vec<ChannelInfo>, Error> {
    let channels = match kind {
        HarnessKind::ZCode => zcode_channels_for(zcode_config_text().as_deref()),
        #[cfg(feature = "ccswitch")]
        HarnessKind::Codex => {
            let mut channels = codex_config_channels();
            merge_unique(&mut channels, ccswitch_channels(app_type::CODEX).await);
            channels
        }
        #[cfg(not(feature = "ccswitch"))]
        HarnessKind::Codex => Vec::new(),
        #[cfg(feature = "ccswitch")]
        HarnessKind::ClaudeCode => ccswitch_channels(app_type::CLAUDE).await,
        #[cfg(not(feature = "ccswitch"))]
        HarnessKind::ClaudeCode => Vec::new(),
        #[cfg(feature = "ccswitch")]
        HarnessKind::OpenCode => {
            let mut channels = opencode_config_channels();
            merge_unique(&mut channels, ccswitch_channels(app_type::OPENCODE).await);
            channels
        }
        #[cfg(not(feature = "ccswitch"))]
        HarnessKind::OpenCode => Vec::new(),
        HarnessKind::ZedAcp | HarnessKind::DeepSeek | HarnessKind::Hermes => Vec::new(),
    };
    Ok(if channels.is_empty() {
        vec![default_channel()]
    } else {
        channels
    })
}

// --- installation detection -------------------------------------------------

async fn detect_named(command: &str) -> Option<DetectInfo> {
    let bin_path = command.find_in_path()?;
    let version = probe_version(&bin_path, &["--version"]).await;
    Some(DetectInfo { bin_path, version })
}

/// ZCode detection: the `PATH` launcher wins for `bin_path`; the version
/// comes from the bundled headless engine when available (the desktop
/// launcher prints diagnostics instead of a version), otherwise from the
/// launcher itself. Without the launcher, the engine alone is reported.
async fn detect_zcode() -> Option<DetectInfo> {
    let engine = PathBuf::from(ZCODE_ENGINE_CJS);
    let engine_version = || async {
        if engine.is_file() {
            probe_zcode_engine_version(&engine).await
        } else {
            None
        }
    };
    if let Some(bin_path) = "zcode".find_in_path() {
        let version = match engine_version().await {
            Some(version) => Some(version),
            None => probe_version(&bin_path, &["--version"]).await,
        };
        return Some(DetectInfo { bin_path, version });
    }
    if engine.is_file() {
        let version = probe_zcode_engine_version(&engine).await;
        return Some(DetectInfo {
            bin_path: engine,
            version,
        });
    }
    None
}

async fn probe_zcode_engine_version(engine: &Path) -> Option<String> {
    let node = "node".find_in_path()?;
    let engine_arg = engine.to_string_lossy().into_owned();
    probe_version(&node, &[engine_arg.as_str(), "--version"]).await
}

/// Runs `<program> <args>` and extracts a version string from the output.
/// Spawn failures and timeouts yield `None` (the binary still exists — only
/// the version is unknown).
async fn probe_version(program: &Path, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let attempt = async { command.output().await };
    let output = tokio::time::timeout(VERSION_PROBE_TIMEOUT, attempt).await.ok()?.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let output = if stdout.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        stdout
    };
    extract_version(&output)
}

/// A single non-empty output line is used as-is (`codex-cli 0.153.2`);
/// multi-line output is only trusted when it contains a bare version line
/// (`0.16.9`), which keeps diagnostic dumps from masquerading as versions.
fn extract_version(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match lines.as_slice() {
        [only] => Some((*only).to_owned()),
        _ => lines.into_iter().find(|line| is_bare_version(line)).map(str::to_owned),
    }
}

fn is_bare_version(line: &str) -> bool {
    let core = line.split(['-', '+', ' ']).next().unwrap_or_default();
    let core = core.strip_prefix('v').unwrap_or(core);
    let mut parts = core.split('.');
    let first = parts.next().unwrap_or_default();
    core.contains('.')
        && !first.is_empty()
        && first.bytes().all(|byte| byte.is_ascii_digit())
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

// --- ZCode channel enumeration ----------------------------------------------

/// Reads the personal provider config: the session-layer override variable
/// first (channel switching writes session copies of this exact file), the
/// user home otherwise. `None` = file missing/unreadable.
fn zcode_config_text() -> Option<String> {
    if let Some(path) = std::env::var_os("CHANNEL_ZCODE_PROVIDER_CONFIG") {
        return std::fs::read_to_string(path).ok();
    }
    let home = home_dir()?;
    std::fs::read_to_string(home.join(".zcode").join("v2").join("provider_config.json")).ok()
}

/// Enumeration core, split from the file access for direct unit testing.
fn zcode_channels_for(config_text: Option<&str>) -> Vec<ChannelInfo> {
    let Some(config_text) = config_text else {
        catalog_note!(
            "the ZCode provider config was not found; configure providers in ZCode first"
        );
        return vec![default_channel()];
    };
    match zcode_channels_from_config(config_text) {
        Ok(channels) if channels.is_empty() => {
            catalog_note!("the ZCode provider config lists no providers");
            vec![default_channel()]
        }
        Ok(channels) => channels,
        Err(error) => {
            catalog_note!("failed to parse the ZCode provider config: {error}");
            vec![default_channel()]
        }
    }
}

fn zcode_channels_from_config(config_text: &str) -> Result<Vec<ChannelInfo>, String> {
    let config: serde_json::Value =
        serde_json::from_str(config_text).map_err(|error| error.to_string())?;
    let config = config.get("config").ok_or("no top-level config object")?;

    // Display names and per-provider modelOrder from the provider rules.
    let mut display_names: HashMap<String, String> = HashMap::new();
    let mut rule_models: HashMap<String, Vec<String>> = HashMap::new();
    let rules = config
        .pointer("/providerConfigRules/providerRules")
        .and_then(serde_json::Value::as_array);
    for rule in rules.into_iter().flatten() {
        let Some(id) = rule.get("providerId").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(name) = rule
            .get("providerName")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            display_names.insert(id.to_owned(), name.to_owned());
        }
        let models: Vec<String> = rule
            .pointer("/config/modelOrder")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| model.as_str())
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .collect();
        if !models.is_empty() {
            rule_models.insert(id.to_owned(), models);
        }
    }

    // Provider universe: providerOrder first, then the providerIds of the
    // model rules in appearance order (deduplicated).
    let mut provider_ids: Vec<String> = Vec::new();
    let mut push_unique = |id: &str| {
        if !provider_ids.iter().any(|existing| existing == id) {
            provider_ids.push(id.to_owned());
        }
    };
    if let Some(order) = config.get("providerOrder").and_then(serde_json::Value::as_array) {
        for id in order.iter().filter_map(serde_json::Value::as_str) {
            push_unique(id);
        }
    }
    let mut model_rule_models: HashMap<String, Vec<String>> = HashMap::new();
    let model_rules = config
        .pointer("/modelConfigRules/providerModelRules")
        .and_then(serde_json::Value::as_array);
    for rule in model_rules.into_iter().flatten() {
        let Some(id) = rule.get("providerId").and_then(serde_json::Value::as_str) else {
            continue;
        };
        push_unique(id);
        if let Some(model) = rule
            .get("modelId")
            .and_then(serde_json::Value::as_str)
            .filter(|model| !model.is_empty())
        {
            model_rule_models
                .entry(id.to_owned())
                .or_default()
                .push(model.to_owned());
        }
    }

    let mut channels = Vec::new();
    for id in provider_ids {
        let models = rule_models
            .get(&id)
            .cloned()
            .or_else(|| model_rule_models.get(&id).cloned())
            .unwrap_or_else(|| vec!["default".to_owned()]);
        let team = display_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| strip_id_prefix(&id));
        for model in models {
            channels.push(ChannelInfo {
                team: team.clone(),
                team_id: id.clone(),
                model,
                switch_key: None,
            });
        }
    }
    Ok(channels)
}

/// Readable team name: drop a `prefix:` qualifier
/// (`account:bigmodel-team-coding-plan` -> `bigmodel-team-coding-plan`).
fn strip_id_prefix(id: &str) -> String {
    match id.split_once(':') {
        Some((_, rest)) if !rest.is_empty() => rest.to_owned(),
        _ => id.to_owned(),
    }
}

// --- Codex channel enumeration (`ccswitch` feature) --------------------------

#[cfg(feature = "ccswitch")]
fn codex_config_channels() -> Vec<ChannelInfo> {
    let Some(text) = home_dir()
        .and_then(|home| std::fs::read_to_string(home.join(".codex").join("config.toml")).ok())
    else {
        catalog_note!("the Codex config.toml was not found; enumerating cc-switch keys only");
        return Vec::new();
    };
    match codex_channels_from_config(&text) {
        Ok(channels) => channels,
        Err(error) => {
            catalog_note!("failed to parse the Codex config.toml: {error}");
            Vec::new()
        }
    }
}

#[cfg(feature = "ccswitch")]
fn codex_channels_from_config(config_text: &str) -> Result<Vec<ChannelInfo>, String> {
    let config: toml::Value = toml::from_str(config_text).map_err(|error| error.to_string())?;
    let model = config
        .get("model")
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "default".to_owned());
    let providers = config
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .ok_or("no [model_providers] sections")?;
    let mut channels = Vec::new();
    for (key, section) in providers {
        let Some(section) = section.as_table() else {
            continue;
        };
        let display = section
            .get("name")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(key);
        channels.push(ChannelInfo {
            team: display.to_owned(),
            team_id: key.clone(),
            model: model.clone(),
            switch_key: None,
        });
    }
    // The toml map iterates by key, not by document position; sort anyway so
    // the enumeration order stays deterministic across toml versions.
    channels.sort_by(|left, right| left.team_id.cmp(&right.team_id));
    Ok(channels)
}

/// cc-switch keys as channels of one app (`app_type`). A missing/unreadable
/// cc-switch database is a normal "not installed" state, not an enumeration
/// failure. Codex keys yield one entry each (the model their config pins);
/// Claude keys yield one entry each (the model from `ANTHROPIC_MODEL` when
/// present); OpenCode keys yield one entry per model of their `models` map.
#[cfg(feature = "ccswitch")]
async fn ccswitch_channels(app: &str) -> Vec<ChannelInfo> {
    let database = match crate::switch::ccswitch::CcSwitchDatabase::path() {
        Ok(database) => database,
        Err(error) => {
            catalog_note!("cannot locate the cc-switch database: {error}");
            return Vec::new();
        }
    };
    if !database.exists() {
        return Vec::new();
    }
    let keys = match database.available_keys(app).await {
        Ok(keys) => keys,
        Err(error) => {
            catalog_note!("failed to read cc-switch keys: {error}");
            return Vec::new();
        }
    };
    let mut channels = Vec::new();
    for key in keys {
        let models = match database.resolve_row(app, &key).await {
            Ok(row) => ccswitch_entry_models(app, &key, &row.settings_config, &row.meta),
            Err(error) => {
                catalog_note!("failed to read cc-switch provider {key}: {error}");
                vec!["default".to_owned()]
            }
        };
        for model in models {
            channels.push(ChannelInfo {
                team: key.clone(),
                team_id: key.clone(),
                model,
                switch_key: Some(key.clone()),
            });
        }
    }
    channels
}

/// The model ids a cc-switch entry enumerates as, per app shape.
#[cfg(feature = "ccswitch")]
fn ccswitch_entry_models(
    app: &str,
    key: &str,
    settings_config: &str,
    meta: &str,
) -> Vec<String> {
    let settings: serde_json::Value = match serde_json::from_str(settings_config) {
        Ok(settings) => settings,
        Err(error) => {
            catalog_note!("invalid cc-switch settings for {key}: {error}");
            return vec!["default".to_owned()];
        }
    };
    match app {
        app_type::CODEX => {
            // Codex 条目解析失败（如 OAuth-only）时不整条丢弃，仍给 default。
            match crate::switch::ccswitch::ResolvedProvider::parse(key, key, settings_config, meta)
            {
                Ok(provider) => vec![provider.model],
                Err(_) => vec!["default".to_owned()],
            }
        }
        app_type::CLAUDE => vec![settings
            .pointer("/env/ANTHROPIC_MODEL")
            .and_then(serde_json::Value::as_str)
            .filter(|model| !model.is_empty())
            .unwrap_or("default")
            .to_owned()],
        app_type::OPENCODE => {
            let models: Vec<String> = settings
                .pointer("/models")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(model, _)| !model.is_empty())
                .map(|(model, _)| model.clone())
                .collect();
            if models.is_empty() {
                vec!["default".to_owned()]
            } else {
                models
            }
        }
        _ => vec!["default".to_owned()],
    }
}

/// One entry per (provider, model) of the user's OpenCode provider sections.
/// Both `opencode.json` and `opencode.jsonc` are read (the latter only when
/// it parses as plain JSON); a section without a `models` map falls back to
/// a single `default` entry.
#[cfg(feature = "ccswitch")]
fn opencode_config_channels() -> Vec<ChannelInfo> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut channels = Vec::new();
    for file in ["opencode.json", "opencode.jsonc"] {
        let Some(text) = std::fs::read_to_string(home.join(".config").join("opencode").join(file))
            .ok()
            .filter(|text| !text.trim().is_empty())
        else {
            continue;
        };
        let config: serde_json::Value = match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                catalog_note!("failed to parse the OpenCode config {file}: {error}");
                continue;
            }
        };
        let Some(providers) = config.get("provider").and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (key, section) in providers {
            let Some(section) = section.as_object() else {
                continue;
            };
            let models: Vec<String> = section
                .get("models")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(model, _)| !model.is_empty())
                .map(|(model, _)| model.clone())
                .collect();
            let models = if models.is_empty() {
                vec!["default".to_owned()]
            } else {
                models
            };
            for model in models {
                channels.push(ChannelInfo {
                    team: key.clone(),
                    team_id: key.clone(),
                    model,
                    switch_key: None,
                });
            }
        }
    }
    channels
}

/// Appends `extra` entries whose team is not enumerated yet (the static
/// config.toml wins over a cc-switch entry with the same display name).
#[cfg(feature = "ccswitch")]
fn merge_unique(base: &mut Vec<ChannelInfo>, extra: Vec<ChannelInfo>) {
    for channel in extra {
        if !base.iter().any(|existing| existing.team == channel.team) {
            base.push(channel);
        }
    }
}

// --- shared helpers -----------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn default_channel() -> ChannelInfo {
    ChannelInfo {
        team: "default".to_owned(),
        team_id: "default".to_owned(),
        model: "default".to_owned(),
        switch_key: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-mutating tests must not race each other (nor leave a modified
    // HOME/CHANNEL_* behind for the parallel tests).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        home: Option<std::ffi::OsString>,
        zcode_config: Option<std::ffi::OsString>,
        cc_switch_db: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::remove_var("CHANNEL_ZCODE_PROVIDER_CONFIG");
            std::env::remove_var("CHANNEL_CC_SWITCH_DB");
            Self {
                home: std::env::var_os("HOME"),
                zcode_config: None,
                cc_switch_db: None,
                _lock: lock,
            }
        }

        fn set_home(&mut self, path: &Path) {
            std::env::set_var("HOME", path);
            self.home = Some(path.as_os_str().to_owned());
        }

        fn set_zcode_config(&mut self, path: &Path) {
            std::env::set_var("CHANNEL_ZCODE_PROVIDER_CONFIG", path);
            self.zcode_config = Some(path.as_os_str().to_owned());
        }

        #[cfg(feature = "ccswitch")]
        fn set_cc_switch_db(&mut self, path: &Path) {
            std::env::set_var("CHANNEL_CC_SWITCH_DB", path);
            self.cc_switch_db = Some(path.as_os_str().to_owned());
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore("HOME", self.home.take());
            restore("CHANNEL_ZCODE_PROVIDER_CONFIG", self.zcode_config.take());
            restore("CHANNEL_CC_SWITCH_DB", self.cc_switch_db.take());
        }
    }

    fn restore(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    const ZCODE_FIXTURE: &str = r#"{
        "schemaVersion": 1,
        "config": {
            "providerOrder": ["prov-1", "prov-2"],
            "providerConfigRules": { "providerRules": [
                {
                    "providerId": "prov-1",
                    "providerName": "glm团队1",
                    "config": { "modelOrder": ["M-A", "M-B"] }
                },
                {
                    "providerId": "account:prov-team",
                    "config": { "modelOrder": ["T-1"] }
                }
            ]},
            "modelConfigRules": { "providerModelRules": [
                { "providerId": "prov-2", "modelId": "P2-M1" },
                { "providerId": "prov-2", "modelId": "P2-M2" },
                { "providerId": "account:prov-team", "modelId": "SHOULD-NOT-APPEAR" }
            ]},
            "defaultModelSelection": { "providerId": "prov-1", "modelId": "M-A" }
        }
    }"#;

    fn channel(team: &str, team_id: &str, model: &str) -> ChannelInfo {
        ChannelInfo {
            team: team.to_owned(),
            team_id: team_id.to_owned(),
            model: model.to_owned(),
            switch_key: None,
        }
    }

    #[test]
    fn zcode_parser_prefers_rule_names_and_model_order() {
        let channels = zcode_channels_from_config(ZCODE_FIXTURE).unwrap();
        assert_eq!(
            channels,
            vec![
                channel("glm团队1", "prov-1", "M-A"),
                channel("glm团队1", "prov-1", "M-B"),
                channel("prov-2", "prov-2", "P2-M1"),
                channel("prov-2", "prov-2", "P2-M2"),
                // modelOrder wins over the providerModelRules entry, and the
                // account: prefix is stripped from the display name.
                channel("prov-team", "account:prov-team", "T-1"),
            ]
        );
    }

    #[test]
    fn zcode_parser_falls_back_through_model_sources() {
        let config = r#"{
            "config": {
                "providerOrder": ["solo"],
                "modelConfigRules": { "providerModelRules": [
                    { "providerId": "solo", "modelId": "SOLO-M" }
                ]}
            }
        }"#;
        assert_eq!(
            zcode_channels_from_config(config).unwrap(),
            vec![channel("solo", "solo", "SOLO-M")],
            "providerModelRules modelIds are used without providerRules modelOrder"
        );

        let config = r#"{ "config": { "providerOrder": ["bare"] } }"#;
        assert_eq!(
            zcode_channels_from_config(config).unwrap(),
            vec![channel("bare", "bare", "default")],
            "a provider without any model source falls back to [default]"
        );

        let config = r#"{ "config": { "providerOrder": [], "modelConfigRules": {} } }"#;
        assert!(zcode_channels_from_config(config).unwrap().is_empty());
    }

    #[test]
    fn zcode_missing_or_broken_config_yields_default_entry() {
        assert_eq!(zcode_channels_for(None), vec![default_channel()]);
        assert_eq!(zcode_channels_for(Some("not json")), vec![default_channel()]);
        assert_eq!(zcode_channels_for(Some(r#"{"other": 1}"#)), vec![default_channel()]);
    }

    #[tokio::test]
    async fn zcode_reads_home_and_env_override() {
        let mut guard = EnvGuard::acquire();

        // Missing home: the fallback entry.
        let home = tempfile::tempdir().unwrap();
        guard.set_home(home.path());
        assert_eq!(list_channels(HarnessKind::ZCode).await.unwrap(), vec![default_channel()]);

        // A broken config file in the home: still the fallback entry.
        let zcode_dir = home.path().join(".zcode").join("v2");
        std::fs::create_dir_all(&zcode_dir).unwrap();
        std::fs::write(zcode_dir.join("provider_config.json"), "{broken").unwrap();
        assert_eq!(list_channels(HarnessKind::ZCode).await.unwrap(), vec![default_channel()]);

        // A valid config file in the home: parsed.
        std::fs::write(zcode_dir.join("provider_config.json"), ZCODE_FIXTURE).unwrap();
        assert_eq!(
            list_channels(HarnessKind::ZCode).await.unwrap().len(),
            5,
            "fixture enumerates 5 (provider, model) channels"
        );

        // The session-layer env override points at the same parse result.
        let override_file = home.path().join("override.json");
        std::fs::write(&override_file, ZCODE_FIXTURE).unwrap();
        guard.set_zcode_config(&override_file);
        assert_eq!(list_channels(HarnessKind::ZCode).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn other_kinds_yield_the_default_entry() {
        // No file system or environment access on this branch, so this is
        // hermetic despite calling the public API.
        assert_eq!(
            list_channels(HarnessKind::DeepSeek).await.unwrap(),
            vec![default_channel()]
        );
        assert_eq!(
            list_channels(HarnessKind::Hermes).await.unwrap(),
            vec![default_channel()]
        );
    }

    #[cfg(feature = "ccswitch")]
    #[test]
    fn codex_parser_reads_sections_names_and_model() {
        let config = r#"
model = "glm-5.3-flash"

[model_providers.custom]
name = "zhipu_glm"
base_url = "http://127.0.0.1:1/v1"

[model_providers.bare]
base_url = "http://127.0.0.1:2/v1"
"#;
        assert_eq!(
            codex_channels_from_config(config).unwrap(),
            vec![
                channel("bare", "bare", "glm-5.3-flash"),
                channel("zhipu_glm", "custom", "glm-5.3-flash"),
            ],
            "sections are enumerated in team_id order"
        );

        assert!(
            codex_channels_from_config("model = \"m\"\n").is_err(),
            "a config without provider sections is a parse failure"
        );
        assert!(
            codex_channels_from_config("broken toml").is_err(),
            "parse failures surface as Err (the wrapper degrades to the default entry)"
        );
    }

    #[cfg(feature = "ccswitch")]
    #[tokio::test]
    async fn codex_merges_config_toml_with_cc_switch_keys() {
        let mut guard = EnvGuard::acquire();

        let home = tempfile::tempdir().unwrap();
        guard.set_home(home.path());
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "model = \"glm-5.3-flash\"\n\n[model_providers.custom]\nname = \"zhipu_glm\"\n",
        )
        .unwrap();

        // No cc-switch database: config.toml entries only.
        guard.set_cc_switch_db(Path::new("/nonexistent/cc-switch.db"));
        assert_eq!(
            list_channels(HarnessKind::Codex).await.unwrap(),
            vec![channel("zhipu_glm", "custom", "glm-5.3-flash")]
        );

        // With a database: keys are appended with their own model and the
        // switch_key; teams already enumerated are not duplicated.
        let database = crate::switch::ccswitch::test_support::TestDatabase::create().await;
        database.insert_provider("alpha", "alpha-model").await;
        database.insert_provider("zhipu_glm", "ignored-model").await;
        guard.set_cc_switch_db(database.path());
        assert_eq!(
            list_channels(HarnessKind::Codex).await.unwrap(),
            vec![
                channel("zhipu_glm", "custom", "glm-5.3-flash"),
                ChannelInfo {
                    switch_key: Some("alpha".to_owned()),
                    ..channel("alpha", "alpha", "alpha-model")
                },
            ]
        );

        // A broken config.toml still lets the cc-switch keys through.
        std::fs::write(codex_dir.join("config.toml"), "broken toml").unwrap();
        let channels = list_channels(HarnessKind::Codex).await.unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].team, "alpha");
        assert_eq!(channels[0].switch_key.as_deref(), Some("alpha"));
    }

    #[test]
    fn command_names_match_the_harness_adapters() {
        assert_eq!(super::command_of(&HarnessKind::Codex), "codex");
        assert_eq!(super::command_of(&HarnessKind::ClaudeCode), "claude");
        assert_eq!(super::command_of(&HarnessKind::OpenCode), "opencode");
        assert_eq!(super::command_of(&HarnessKind::ZedAcp), "zed");
        assert_eq!(super::command_of(&HarnessKind::ZCode), "zcode");
        assert_eq!(super::command_of(&HarnessKind::DeepSeek), "deepseek");
        assert_eq!(super::command_of(&HarnessKind::Hermes), "hermes");
    }

    #[tokio::test]
    async fn detection_misses_are_none_and_hits_carry_versions() {
        assert_eq!(detect_named("channel-no-such-tool-xyz").await, None);

        // The platform shell itself answers --version.
        let (shell, args): (&str, &[&str]) = if cfg!(windows) {
            (r"C:\Windows\System32\cmd.exe", &["/c", "ver"])
        } else {
            ("/bin/sh", &["--version"])
        };
        assert!(
            probe_version(Path::new(shell), args).await.is_some(),
            "the platform shell reports a version"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // /tmp is frequently mounted noexec, so probe from the crate
            // target directory instead.
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!(
                    "catalog-probe-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("probe");
            std::fs::write(&script, "#!/bin/sh\necho 9.9.9-custom\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let version = probe_version(&script, &["--version"]).await;
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(version.as_deref(), Some("9.9.9-custom"));
        }
    }

    #[test]
    fn version_extraction_rejects_diagnostic_dumps() {
        assert_eq!(extract_version("codex-cli 0.153.2\n"), Some("codex-cli 0.153.2".to_owned()));
        assert_eq!(extract_version("0.16.9\n"), Some("0.16.9".to_owned()));
        assert_eq!(extract_version("v1.2.3\n"), Some("v1.2.3".to_owned()));
        assert_eq!(extract_version("\n"), None);
        // Desktop launcher logs: multi-line without a bare version line.
        let dump = "[2026-10-12 10:00:00.000] [pid:1] [main] started\n  args: []\n}\n";
        assert_eq!(extract_version(dump), None);
        // Multi-line with one bare version line.
        let dump = "loading...\n1.2.4\nfooter\n";
        assert_eq!(extract_version(dump), Some("1.2.4".to_owned()));
    }

    #[test]
    fn id_prefix_stripping() {
        assert_eq!(strip_id_prefix("account:bigmodel-team-coding-plan"), "bigmodel-team-coding-plan");
        assert_eq!(strip_id_prefix("custom:bigmodel-api-2"), "bigmodel-api-2");
        assert_eq!(strip_id_prefix("bigmodel-api-2"), "bigmodel-api-2");
        assert_eq!(strip_id_prefix("account:"), "account:");
    }
}
