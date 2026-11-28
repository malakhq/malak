//! Read-side role coverage using owner-created, read-only organisation seats.

use super::*;

struct Organisation {
    lead: Fixture,
    other: Fixture,
    chief: Fixture,
    guest: PathBuf,
}

impl Organisation {
    fn new() -> Self {
        let lead = Fixture::new();
        let other = lead.sibling("other", "o");
        let chief = lead.sibling("top", "t");
        for board in [&lead, &other, &chief] {
            board.start();
        }
        other.lead(&[
            "send",
            "briefs",
            "Type: brief\nTitle: Other brief\nKind: learned\nLead: other\n\nOriginal text",
        ]);
        let guest = other.enrol("reader-lead", &["briefs:read,get"]);
        let chief_guest = other.enrol("reader-chief", &["briefs:read,get"]);
        let seat = |path: &Path| {
            json!({
                "board":"other", "config":other.config,
                "id":malak_core::HandoverFile::read(path).unwrap().id().to_string(),
                "handover":path, "top":false,
            })
        };
        let roster = json!({
            "top":chief.config, "leads":{
                "malak":{"config":lead.config,"active":true},
                "other":{"config":other.config,"active":true},
            },
            "chief_of_staff":{"name":"chief"}, "candidate":null,
            "seats":{"malak":[seat(&guest)],"chief":[seat(&chief_guest)]},
            "rail_age_days":30,"lead_silent_minutes":60,"work_stale_minutes":30,"ramp_minutes":10,
        });
        let folder = chief.state.join("org");
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        let path = folder.join("roster.json");
        fs::write(&path, roster.to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            lead,
            other,
            chief,
            guest,
        }
    }

    fn command(&self, reader: &Fixture, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        reader.recipe_environment(&mut command);
        command.args(words);
        command
    }

    fn read(&self, reader: &Fixture, words: &[&str]) -> Value {
        serde_json::from_str(&run(self.command(reader, words))).unwrap()
    }
}

#[test]
fn another_lead_lists_briefs_through_its_existing_read_only_seat() {
    let org = Organisation::new();

    let result = org.read(&org.lead, &["briefs", "--board", "other", "--json"]);

    assert_eq!(result["items"][0]["title"], "Other brief");
    assert_eq!(result["items"][0]["link"]["board"], "other");
}

#[test]
fn the_chief_of_staff_gets_a_brief_through_its_read_only_seat() {
    let org = Organisation::new();

    let result = org.read(&org.chief, &["brief", "get", "other", "1", "--json"]);

    assert!(
        result["items"][0]["body"]
            .as_str()
            .unwrap()
            .ends_with("Original text")
    );
}

#[test]
fn the_chiefs_own_brief_links_use_the_top_board_name() {
    let org = Organisation::new();
    org.chief.lead(&[
        "send",
        "briefs",
        "Type: brief\nTitle: Exit\nKind: exit\nLead: chief\n\nExit text",
    ]);

    let result = org.read(&org.chief, &["brief", "get", "top", "1", "--json"]);

    assert_eq!(result["items"][0]["link"]["board"], "top");
}

#[test]
fn a_brief_read_seat_cannot_send_to_the_other_board() {
    let org = Organisation::new();

    let output = output(participant_command(
        &org.guest,
        &["send", "briefs", "Forged"],
    ));

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        org.other.status()["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["name"] == "briefs")
            .unwrap()["head"],
        2
    );
}

#[test]
fn the_chiefs_brief_view_includes_other_roster_boards() {
    let org = Organisation::new();
    let result = org.read(&org.chief, &["brief", "view", "--json"]);
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["board"] == "other" && item["title"] == "Other brief")
    );
}

#[test]
fn a_leads_brief_view_stays_on_its_own_board() {
    let org = Organisation::new();
    let result = org.read(&org.lead, &["brief", "view", "--json"]);
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["board"] == "malak")
    );
}

#[test]
fn putting_a_brief_never_falls_back_from_another_selected_board() {
    let org = Organisation::new();
    let mut command = org.lead.recipe("brief");
    command
        .arg("--config")
        .arg(&org.other.config)
        .args(["put", "--title", "Wrong board", "--kind", "report"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Do not misroute this brief")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(
        !org.lead.status()["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|channel| channel["name"] == "briefs")
    );
}

#[test]
fn an_unreadable_roster_board_is_shown_in_the_chiefs_view() {
    let org = Organisation::new();
    let id = participant(&org.other.status(), "reader-chief")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    run(org.other.owner(&["revoke", &id]));
    let result = org.read(&org.chief, &["brief", "view", "--json"]);
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["board"] == "other" && item["section"] == "unreadable")
    );
}
