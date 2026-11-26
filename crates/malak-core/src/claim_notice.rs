//! The text of the notice that a claimed session ended.
//!
//! The text names no command line tool. A program that shows the notice can
//! find the participant it names with `ended_session_name` and add its own
//! wording for the next step.

const ENDED: &str = "'s session ended. Its handover is unclaimed.";

/// The notice for a participant whose claimed session ended.
pub fn ended(name: &crate::ParticipantName) -> String {
    format!("{name}{ENDED} Check its claim history, then re-issue the handover.")
}

/// Return the participant an ended-session notice names, if `text` contains one.
///
/// The notice can open a message or follow an exit report, so the name is the
/// word before the notice's fixed wording.
pub fn ended_session_name(text: &str) -> Option<crate::ParticipantName> {
    let before = &text[..text.find(ENDED)?];
    before.split_whitespace().next_back()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_tells_the_reader_to_reissue_the_handover() {
        let notice = ended(&"builder".parse().unwrap());

        assert!(notice.starts_with("builder's session ended."), "{notice}");
        assert!(notice.contains("re-issue the handover"), "{notice}");
    }

    #[test]
    fn the_notice_names_no_command_line_tool() {
        let notice = ended(&"builder".parse().unwrap());

        assert!(!notice.contains("malak "), "{notice}");
        assert!(!notice.contains("recipe"), "{notice}");
        assert!(!notice.contains("worker-start"), "{notice}");
    }

    #[test]
    fn the_name_is_found_in_a_notice_that_opens_the_message() {
        let notice = ended(&"builder".parse().unwrap());

        assert_eq!(ended_session_name(&notice).unwrap().as_str(), "builder");
    }

    #[test]
    fn the_name_is_found_in_a_notice_after_an_exit_report() {
        let text = format!(
            "Worker exit status: 0\n\n{}\n\nresult",
            ended(&"builder".parse().unwrap())
        );

        assert_eq!(ended_session_name(&text).unwrap().as_str(), "builder");
    }

    #[test]
    fn other_text_names_no_session() {
        assert!(ended_session_name("builder is on the board.").is_none());
    }
}
