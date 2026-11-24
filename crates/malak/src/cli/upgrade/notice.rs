use serde::{Deserialize, Serialize};

pub(super) use malak_core::upgrade_notice::{CHANGELOG, message};

#[derive(Deserialize, Serialize)]
pub(super) struct Previous {
    pub(super) build: String,
    pub(super) entry: Option<String>,
}

impl Previous {
    pub(super) fn current(build: String) -> Self {
        Self {
            build,
            entry: malak_core::upgrade_notice::latest_change().map(str::to_owned),
        }
    }
}
