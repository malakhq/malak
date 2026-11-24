use super::*;

const COMMANDS: &[&[&str]] = &[
    &[],
    &["board"],
    &["board", "start"],
    &["board", "list"],
    &["board", "status"],
    &["board", "stop"],
    &["board", "stop", "--all"],
    &["uninstall"],
    &["upgrade"],
    &["enrol"],
    &["grant"],
    &["rekey"],
    &["revoke"],
    &["channel"],
    &["channel", "create"],
    &["channel", "remove"],
    &["trim"],
    &["tail"],
    &["mailbox"],
    &["recover"],
    &["hook"],
    &["recipe"],
    &["send"],
    &["read"],
    &["get"],
    &["ack"],
];

fn help(board: &Fixture, command: &[&str], argument: &str) -> String {
    let output = board
        .without_handover(command)
        .env("HOME", board.socket.path())
        .env_remove("XDG_CONFIG_HOME")
        .arg(argument)
        .output()
        .unwrap();
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{command:?} {argument}: {output:?}"
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn every_command_and_recipe_accepts_the_same_three_help_forms() {
    let board = Fixture::new();
    let before = board.status();
    let recipes = run(&mut board.without_handover(&["recipe"]));
    assert!(!recipes.is_empty());

    for command in COMMANDS
        .iter()
        .map(|path| path.to_vec())
        .chain(recipes.lines().map(|name| vec!["recipe", name]))
    {
        let usage = help(&board, &command, "--help");
        assert_eq!(help(&board, &command, "help"), usage, "{command:?}");
        assert_eq!(help(&board, &command, "-h"), usage, "{command:?}");
        let line = usage
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("usage: "))
            .unwrap_or_else(|| panic!("missing usage for {command:?}: {usage}"));
        assert!(!line.contains("[OPTIONS]"), "{line}");
        if command != ["recipe", "agents"] {
            assert!(line.contains("--board NAME"), "{line}");
        }
        assert!(usage.contains("Example:\n"), "{command:?}: {usage}");
        assert!(
            !usage
                .lines()
                .next()
                .unwrap()
                .to_ascii_lowercase()
                .starts_with("usage:")
        );
    }

    let after = board.status();
    assert_eq!(after["channels"], before["channels"]);
    assert_eq!(after["participants"], before["participants"]);
}

#[test]
fn help_can_precede_a_nested_command() {
    let board = Fixture::new();
    let nested = help(&board, &["board", "start"], "--help");
    assert_eq!(help(&board, &["help", "board"], "start"), nested);
}

#[test]
fn send_help_with_a_body_creates_no_channel() {
    let board = Fixture::new();
    let before = board.status();

    let output = call(&board.lead(), &["send", "--kind", "note", "help", "body"]);

    assert_eq!(output, help(&board, &["send"], "--help"));
    assert_eq!(board.status()["channels"], before["channels"]);
}

#[test]
fn help_is_still_valid_as_an_option_value_or_message_body() {
    let board = Fixture::new();
    call(&board.lead(), &["send", "--id", "help", "lead", "help"]);
    let output = call(&board.lead(), &["get", "--json", "lead", "1"]);
    let message = messages(&output).remove(0);
    assert_eq!(message["id"], "help");
    assert_eq!(message["text"], "help");
}

#[test]
fn a_recipe_recognizes_help_after_its_options() {
    let board = Fixture::new();
    let output =
        run(&mut board.without_handover(&["recipe", "forward", "--board", "missing", "help"]));
    assert_eq!(output, help(&board, &["recipe", "forward"], "--help"));
}

fn reserved_commands() -> impl Iterator<Item = &'static str> {
    COMMANDS
        .iter()
        .filter_map(|path| match *path {
            [name] => Some(*name),
            _ => None,
        })
        .chain(["help"])
}

#[test]
fn reserved_commands_cannot_be_enrolled_as_participants() {
    let board = Fixture::new();
    for name in reserved_commands() {
        let handover = board.socket.path().join(name);
        let output = board
            .owner_command(&["enrol", "--out"])
            .arg(&handover)
            .args(["--", name])
            .output()
            .unwrap();
        assert_reserved("participant", name, &output);
        assert!(!handover.exists());
    }
}

#[test]
fn reserved_commands_cannot_be_created_as_channels() {
    let board = Fixture::new();
    let before = board.status();
    for name in reserved_commands() {
        let output = board
            .owner_command(&["channel", "create", "--", name])
            .output()
            .unwrap();
        assert_reserved("channel", name, &output);
    }
    assert_eq!(board.status()["channels"], before["channels"]);
}

#[test]
fn reserved_commands_cannot_receive_messages() {
    let board = Fixture::new();
    let before = board.status();
    for name in reserved_commands() {
        let output = board
            .without_handover(&["send", "--", name, "body"])
            .output()
            .unwrap();
        assert_reserved("channel", name, &output);
    }
    assert_eq!(board.status()["channels"], before["channels"]);
}

fn assert_reserved(kind: &str, name: &str, output: &std::process::Output) {
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{name}: {output:?}");
    assert!(
        diagnostic.contains(&format!("{kind} name {name} is reserved")),
        "{diagnostic}"
    );
    assert!(output.stdout.is_empty(), "{name}: {output:?}");
}
