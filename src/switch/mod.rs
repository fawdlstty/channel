// Provider switching (cc-switch integration). The heavy dependencies
// (ormer SQLite, potato, toml) are gated behind the `ccswitch` feature;
// without it the session layer degrades: unconfigured sessions run
// as usual, while an explicit switch request fails with
// `Error::UnsupportedCapability` instead of pulling the stack in.

#[cfg(feature = "ccswitch")]
mod ccswitch;

use std::path::Path;

use crate::protocol::{Error, SessionConfig, SwitchProvider};

#[cfg(feature = "ccswitch")]
impl SwitchProvider {
    pub(crate) async fn available_keys(self) -> Result<Vec<String>, Error> {
        match self {
            SwitchProvider::CcSwitch => ccswitch::CodexDatabase::path()?.available_keys().await,
        }
    }
}

#[cfg(feature = "ccswitch")]
impl SwitchProvider {
    pub(crate) async fn prepare_resources(
        self,
        config: &SessionConfig,
    ) -> Result<Option<PreparedResources>, Error> {
        match self {
            Self::CcSwitch => ccswitch::CodexResources::prepare(config)
                .await
                .map(|resources| resources.map(PreparedResources)),
        }
    }
}

#[cfg(feature = "ccswitch")]
pub(crate) struct PreparedResources(ccswitch::CodexResources);

#[cfg(feature = "ccswitch")]
impl PreparedResources {
    pub(crate) fn env(&self) -> [(&str, &Path); 1] {
        self.0.env()
    }
}

// No-`ccswitch` fallback: keep the session-layer call sites unchanged. A
// configured switch key is a hard error (the caller asked for switching the
// build cannot do); an unconfigured one stays a no-op so plain Codex/ACP/CLI
// sessions never pay for the feature.
#[cfg(not(feature = "ccswitch"))]
impl SwitchProvider {
    const UNSUPPORTED: &'static str = "switch provider support requires the `ccswitch` feature";

    pub(crate) async fn available_keys(self) -> Result<Vec<String>, Error> {
        Err(Error::UnsupportedCapability(Self::UNSUPPORTED.to_owned()))
    }

    pub(crate) async fn prepare_resources(
        self,
        config: &SessionConfig,
    ) -> Result<Option<PreparedResources>, Error> {
        if config.get_switch_key(self).is_some() {
            return Err(Error::UnsupportedCapability(Self::UNSUPPORTED.to_owned()));
        }
        Ok(None)
    }
}

// Shape-compatible stub for the no-`ccswitch` build: `env()` yields an empty
// list so the `PreparedResources::env` slicing in codex/mod.rs compiles
// as-is.
#[cfg(not(feature = "ccswitch"))]
pub(crate) struct PreparedResources;

#[cfg(not(feature = "ccswitch"))]
impl PreparedResources {
    pub(crate) fn env(&self) -> [(&'static str, &'static Path); 0] {
        []
    }
}
