use serde::{Deserialize, Serialize};

pub(super) use malak_core::upgrade_notice::message;

/// The lead-facing changelog shipped in this build. The board quotes it in the
/// upgrade notice because `board start` hands it over.
pub(crate) const CHANGELOG: &str = include_str!("../../../lead-changes.md");

#[derive(Deserialize, Serialize)]
pub(super) struct Previous {
    pub(super) build: String,
    pub(super) entry: Option<String>,
}

impl Previous {
    pub(super) fn current(build: String) -> Self {
        Self {
            build,
            entry: malak_core::upgrade_notice::latest_change(CHANGELOG).map(str::to_owned),
        }
    }
}
