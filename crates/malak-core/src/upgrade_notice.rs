//! Build the fixed upgrade note for the owner door and the CLI preview.

/// The lead-facing changelog shipped in this build.
pub const CHANGELOG: &str = include_str!("../lead-changes.md");
const FALLBACK_ENTRIES: usize = 5;
/// The reserved record sender ID for notices sent by the owner door.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub const SENDER_ID: &str = "malakowner";

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn sent_by_owner(sender: &crate::ParticipantId, channel: &crate::ChannelName) -> bool {
    sender.as_str() == SENDER_ID && channel.as_str() == "lead"
}

/// Return the newest entry in this build, to mark what the older build knew.
pub fn latest_change() -> Option<&'static str> {
    entries(CHANGELOG).next()
}

/// Render a notice using the changes listed ahead of a known previous entry.
/// If the previous entry is unavailable, use the latest five entries.
pub fn message(
    previous_build: Option<&str>,
    installed_build: &str,
    changelog: &str,
    previous_entry: Option<&str>,
) -> String {
    let old = previous_build.unwrap_or("unknown");
    let mut notice =
        format!("Malak upgraded (build {old} -> {installed_build}). Re-read the malak skill.");
    let count = previous_entry
        .and_then(|previous| entries(changelog).position(|line| line == previous))
        .unwrap_or(FALLBACK_ENTRIES);
    for line in entries(changelog).take(count) {
        notice.push('\n');
        notice.push_str(line);
    }
    notice
}

fn entries(changelog: &str) -> impl Iterator<Item = &str> {
    changelog.lines().filter(|line| line.starts_with("- "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_lists_entries_newer_than_the_previous_build() {
        let changelog = "# Changes\n\n- 2026-11-24 new\n- 2026-11-24 old\n- 2026-11-23 older";
        assert_eq!(
            message(Some("abc"), "def", changelog, Some("- 2026-11-24 old")),
            "Malak upgraded (build abc -> def). Re-read the malak skill.\n- 2026-11-24 new",
        );
    }

    #[test]
    fn the_notice_limits_the_list_when_the_previous_entry_is_unknown() {
        let log = "- one\n- two\n- three\n- four\n- five\n- six";
        let notice = message(None, "def", log, Some("- missing"));
        assert!(notice.starts_with("Malak upgraded (build unknown -> def)."));
        assert_eq!(
            notice.lines().skip(1).collect::<Vec<_>>(),
            ["- one", "- two", "- three", "- four", "- five"]
        );
    }
}
