// Provider switching (cc-switch integration). The heavy dependencies
// (ormer SQLite, potato, toml) are gated behind the `ccswitch` feature;
// without it the session layer degrades: unconfigured sessions run
// as usual, while an explicit switch request fails with
// `Error::UnsupportedCapability` instead of pulling the stack in.

// pub(crate): the catalog enumeration (`crate::catalog`) reads provider keys
// through the same database handle the session layer uses, and the catalog
// tests reuse the cc-switch test database builder.
#[cfg(feature = "ccswitch")]
pub(crate) mod ccswitch;

use crate::protocol::{Error, SessionConfig, SwitchProvider};

#[cfg(feature = "ccswitch")]
impl SwitchProvider {
    pub(crate) async fn available_keys(self) -> Result<Vec<String>, Error> {
        match self {
            SwitchProvider::CcSwitch => {
                ccswitch::CcSwitchDatabase::path()?
                    .available_keys(ccswitch::app_type::CODEX)
                    .await
            }
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
            Self::CcSwitch => {
                let keyed = config.get_switch_key(Self::CcSwitch).is_some();
                match (&config.kind, keyed) {
                    (crate::protocol::HarnessKind::ZCode, _) => {
                        ccswitch::ZcodeResources::prepare(config)
                            .await
                            .map(|resources| resources.map(PreparedResources::Zcode))
                    }
                    (crate::protocol::HarnessKind::Codex, _) => {
                        ccswitch::CodexResources::prepare(config)
                            .await
                            .map(|resources| resources.map(PreparedResources::Codex))
                    }
                    (crate::protocol::HarnessKind::ClaudeCode, _) => {
                        ccswitch::ClaudeResources::prepare(config)
                            .await
                            .map(|resources| resources.map(PreparedResources::Claude))
                    }
                    (crate::protocol::HarnessKind::OpenCode, _) => {
                        ccswitch::OpencodeResources::prepare(config)
                            .await
                            .map(|resources| resources.map(PreparedResources::Opencode))
                    }
                    // 其余 harness 配置了渠道切换时显式报错：静默忽略会让调用方
                    // 以为已按渠道切换、实际一直跑本地默认渠道。
                    (_, true) => Err(Error::UnsupportedCapability(format!(
                        "cc-switch provider switching is not supported for the {:?} harness",
                        config.kind
                    ))),
                    (_, false) => Ok(None),
                }
            }
        }
    }
}

#[cfg(feature = "ccswitch")]
pub(crate) enum PreparedResources {
    Codex(ccswitch::CodexResources),
    Zcode(ccswitch::ZcodeResources),
    Claude(ccswitch::ClaudeResources),
    Opencode(ccswitch::OpencodeResources),
}

#[cfg(feature = "ccswitch")]
impl PreparedResources {
    /// 应注入子进程环境的 (变量名, 值) 列表。持有本枚举即持有会话切换的
    /// 落盘资源（CODEX_HOME / 会话配置副本），进程内不得提前丢弃。
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        match self {
            Self::Codex(resources) => resources
                .env()
                .iter()
                .map(|(name, path)| ((*name).to_owned(), path.to_string_lossy().into_owned()))
                .collect(),
            Self::Zcode(resources) => resources
                .env()
                .iter()
                .map(|(name, path)| ((*name).to_owned(), path.to_string_lossy().into_owned()))
                .collect(),
            Self::Claude(resources) => resources.env().to_vec(),
            Self::Opencode(resources) => resources
                .env()
                .iter()
                .map(|(name, path)| ((*name).to_owned(), path.to_string_lossy().into_owned()))
                .collect(),
        }
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
// list so the spawn call sites in codex/mod.rs and harness/cli.rs compile
// as-is.
#[cfg(not(feature = "ccswitch"))]
pub(crate) struct PreparedResources;

#[cfg(not(feature = "ccswitch"))]
impl PreparedResources {
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}
