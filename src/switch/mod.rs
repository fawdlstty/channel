mod ccswitch;

use std::path::Path;

use crate::protocol::{Error, SessionConfig, SwitchProvider};

impl SwitchProvider {
    pub(crate) fn available_keys(self) -> Result<Vec<String>, Error> {
        match self {
            SwitchProvider::CcSwitch => ccswitch::CodexDatabase::path()?.available_keys(),
        }
    }
}

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

pub(crate) struct PreparedResources(ccswitch::CodexResources);

impl PreparedResources {
    pub(crate) fn env(&self) -> [(&str, &Path); 1] {
        self.0.env()
    }
}
