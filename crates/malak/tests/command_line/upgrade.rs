use super::*;
use malak_core::{HandoverFile, LocalClient};
use malak_transport_unix::UnixTransport;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HOOK_REVIEW: &str = "For chats opened before this upgrade";
const INSTALL: &str = r#"
/bin/cp "$MALAK_TEST_REPLACEMENT" "$CARGO_INSTALL_ROOT/bin/malak.new"
/bin/mv "$CARGO_INSTALL_ROOT/bin/malak.new" "$CARGO_INSTALL_ROOT/bin/malak"
"#;

fn checkout(home: &Home, version: &str) -> PathBuf {
    let checkout = home.path().join("source");
    let package = checkout.join("crates/malak");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        checkout.join("Cargo.toml"),
        format!("[workspace.package]\nversion = {version:?}\n"),
    )
    .unwrap();
    fs::write(
        package.join("Cargo.toml"),
        "[package]\nname = \"malak\"\nversion.workspace = true\n",
    )
    .unwrap();
    checkout
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
    fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
}

fn upgrade_fixture() -> CargoInstall {
    let fixture = CargoInstall::new();
    executable(&fixture.tools.join("codex"), "exit 1");
    fixture
}

fn cargo(fixture: &CargoInstall, body: &str) {
    executable(
        &fixture.tools.join("cargo"),
        &format!("printf '%s\\n' \"$@\" > \"$HOME/called\"\n{body}"),
    );
}

fn command(fixture: &CargoInstall, args: &[&str]) -> Command {
    installed_command(fixture, &fixture.exe, args)
}

fn installed_command(fixture: &CargoInstall, exe: &Path, args: &[&str]) -> Command {
    let mut command = fixture.command(exe, args);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let folders = std::iter::once(fixture.tools.clone()).chain(std::env::split_paths(&path));
    command
        .env("PATH", std::env::join_paths(folders).unwrap())
        .env("CODEX_HOME", fixture.home.path().join(".codex"))
        .env("CLAUDE_CONFIG_DIR", fixture.home.path().join(".claude"))
        .env("MALAK_TEST_REPLACEMENT", MALAK);
    command
}

fn change_build(fixture: &CargoInstall) {
    // Mach-O and ELF both permit bytes beyond the loadable executable.
    fs::OpenOptions::new()
        .append(true)
        .open(&fixture.exe)
        .unwrap()
        .write_all(b"old test build")
        .unwrap();
}

fn build(path: &Path) -> String {
    Sha256::digest(fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn old_board(fixture: &CargoInstall) -> Fixture {
    Fixture::start(|path| spawn_board(command(fixture, &[]), path))
}

struct StopDetached(PathBuf);

impl Drop for StopDetached {
    fn drop(&mut self) {
        let _ = Command::new(MALAK)
            .arg("--config")
            .arg(&self.0)
            .args(["board", "stop"])
            .output();
    }
}

struct ReleaseWorker(PathBuf);

impl Drop for ReleaseWorker {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "");
    }
}

#[test]
fn upgrade_stops_early_when_the_latest_release_is_installed() {
    let fixture = upgrade_fixture();
    executable(
        &fixture.tools.join("git"),
        &format!("printf 'ref\\trefs/tags/v{VERSION}\\nref\\trefs/tags/v99.0.0-rc.1\\n'\n"),
    );

    let output = run(&mut command(&fixture, &["upgrade"]));

    assert!(output.contains("already up to date"), "{output}");
    assert!(!fixture.called.exists());
}

#[test]
fn upgrade_selects_the_highest_numeric_release() {
    let fixture = upgrade_fixture();
    executable(
        &fixture.tools.join("git"),
        "printf 'ref\\trefs/tags/v1.9.0\\nref\\trefs/tags/v1.10.0\\n'",
    );

    let output = run(&mut command(&fixture, &["upgrade", "--dry-run"]));

    assert!(output.contains("tag v1.10.0"), "{output}");
    assert!(!fixture.called.exists());
}

#[test]
fn upgrade_preview_lists_connected_workers() {
    let fixture = upgrade_fixture();
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    let (_, handover) = board.enrol("connected-worker");
    let _connected = LocalClient::participant(
        Arc::new(UnixTransport::default()),
        HandoverFile::read(handover).unwrap(),
        Some(Arc::new(malak_mechanism_key::Key)),
    )
    .unwrap();
    let before = board.status()["run"].clone();
    let plan = run(command(&fixture, &["upgrade", "--dry-run", "--path"]).arg(source));

    assert!(
        plan.lines().any(|line| {
            line.split_whitespace()
                .eq(["Connected", "workers", "connected-worker"])
        }),
        "{plan}"
    );
    assert!(!fixture.called.exists());
    assert_eq!(board.status()["run"], before);
}

#[test]
fn upgrade_restarts_with_an_active_detached_worker_without_prompting() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let mut board = old_board(&fixture);
    let _stop = StopDetached(board.config.clone());
    let gate = ReleaseWorker(fixture.home.path().join("release-worker"));
    run(command(&fixture, &["--config"])
        .arg(&board.config)
        .args([
            "recipe",
            "worker-start",
            "detached-worker",
            "--no-watcher",
            "--detach",
            "--",
            "sh",
            "-ec",
            r#"checks=0
            while [ ! -f "$TEST_RELEASE" ]; do
                [ "$checks" -lt 300 ] || exit 1
                checks=$((checks + 1))
                sleep 0.1
            done
            echo finished"#,
        ])
        .env("TEST_RELEASE", &gate.0));
    let before = board.status();
    let worker = before["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|person| person["name"] == "detached-worker")
        .unwrap();
    assert_eq!(worker["connections"], 0);

    let output = run(command(&fixture, &["upgrade", "--path"]).arg(source));

    assert!(
        output.lines().any(|line| {
            line.split_whitespace()
                .eq(["Active", "worker", "launchers", "detached-worker"])
        }),
        "{output}"
    );
    assert!(
        !output.contains("may cost active workers their final report"),
        "{output}"
    );
    assert!(board.board.wait().unwrap().success());
    assert_ne!(board.status()["run"], before["run"]);
    fs::write(&gate.0, "").unwrap();
    let report = run(command(&fixture, &["--config"]).arg(&board.config).args([
        "recipe",
        "await",
        "detached-worker",
        "--timeout",
        "30s",
    ]));
    assert!(report.contains("finished"), "{report}");
}

#[test]
fn unattended_upgrade_without_boards_runs_without_yes() {
    let fixture = upgrade_fixture();
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);

    run(command(&fixture, &["upgrade", "--path"]).arg(source));

    assert!(fixture.called.exists());
}

#[test]
fn an_upgrade_dry_run_never_invokes_cargo() {
    let fixture = upgrade_fixture();
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    let before = board.status()["run"].clone();

    let output = run(command(&fixture, &["upgrade", "--dry-run", "--path"]).arg(source));

    assert!(output.contains("Malak upgrade plan"));
    assert!(output.contains("Will restart"));
    assert!(output.contains("before the local build IDs can be compared"));
    assert!(!fixture.called.exists());
    assert_eq!(board.status()["run"], before);
}

#[test]
fn a_dry_run_previews_new_changelog_lines_and_leaves_the_lead_inbox_empty() {
    let fixture = upgrade_fixture();
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    let docs = source.join("docs");
    fs::create_dir(&docs).unwrap();
    fs::write(
        docs.join("LEAD-CHANGES.md"),
        format!(
            "- 2026-11-25 New lead change.\n{}",
            malak_core::upgrade_notice::CHANGELOG,
        ),
    )
    .unwrap();

    let output = run(command(&fixture, &["upgrade", "--dry-run", "--path"]).arg(source));

    assert!(output.contains("Re-read the malak skill."), "{output}");
    assert!(
        output.contains("\n  - 2026-11-25 New lead change."),
        "{output}"
    );
    assert!(!output.contains("Mailbox previews recover"), "{output}");
    assert!(messages(&call(&board.lead(), &["read", "lead", "--json"])).is_empty());
}

#[test]
fn local_upgrade_restarts_the_listed_running_board_once_on_the_installed_build() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let mut serving = old_board(&fixture);
    let mut stopped = old_board(&fixture);
    fixture.home.default_alias(&serving);
    let _stop = StopDetached(serving.config.clone());
    let before = serving.status();
    call(&serving.lead(), &["send", "lead", "keep this message"]);
    run(&mut stopped.without_handover(&["board", "stop"]));
    assert!(stopped.board.wait().unwrap().success());
    let listed: Vec<Value> =
        serde_json::from_str(&run(&mut command(&fixture, &["board", "list", "--json"]))).unwrap();
    let statuses: std::collections::BTreeSet<_> = listed
        .iter()
        .map(|board| board["status"].as_str().unwrap())
        .collect();
    assert_eq!(listed.len(), 2);
    assert_eq!(statuses, ["running", "stopped"].into_iter().collect());

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(&source));

    assert!(serving.board.wait().unwrap().success());
    let after = serving.status();
    assert_ne!(after["run"], before["run"]);
    assert_ne!(after["board_build"], before["board_build"]);
    assert_eq!(after["board_build"], build(&fixture.exe));
    assert_eq!(output.matches("Restarted ").count(), 1, "{output}");
    assert!(
        output.contains("Restart each lead chat's watcher"),
        "{output}"
    );
    let found = call(&serving.lead(), &["get", "lead", "1"]);
    assert!(found.contains("keep this message"), "{found}");
    let notice = call(&serving.lead(), &["get", "lead", "2"]);
    assert!(notice.contains("Note from owner in lead #2"), "{notice}");
    assert_eq!(
        stopped
            .without_handover(&["board", "status"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(3),
    );
    let cargo_args = fs::read_to_string(&fixture.called).unwrap();
    assert!(cargo_args.contains("install\n--locked\n--force\n--bin\nmalak\n--path\n"));
    assert!(cargo_args.ends_with(&format!("{}\n", source.join("crates/malak").display())));
}

#[test]
fn upgrade_posts_one_owner_note_with_new_changes_to_each_running_board() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    let previous = build(&fixture.exe);
    let mut first = old_board(&fixture);
    let mut second = old_board(&fixture);
    fixture.home.default_alias(&first);
    let mut stopped = old_board(&fixture);
    let _stop_first = StopDetached(first.config.clone());
    let _stop_second = StopDetached(second.config.clone());
    run(&mut stopped.without_handover(&["board", "stop"]));
    assert!(stopped.board.wait().unwrap().success());
    replace_with_current_build(&fixture);
    let installed = build(&fixture.exe);
    let lines: Vec<_> = malak_core::upgrade_notice::CHANGELOG
        .lines()
        .filter(|line| line.starts_with("- "))
        .collect();
    let snapshot = serde_json::json!({"build": previous, "entry": lines[2]});
    let mut finish = command(
        &fixture,
        &["__upgrade-finish", "skip", &installed, "--previous"],
    );
    finish.arg(snapshot.to_string());
    for board in [&first, &second] {
        let plan = serde_json::json!({
            "config": board.config, "name": "board", "run": board.status()["run"], "workers": []
        });
        finish.arg(plan.to_string());
    }

    run(&mut finish);

    let expected = format!(
        "Malak upgraded (build {previous} -> {installed}). Re-read the malak skill.\n{}\n{}",
        lines[0], lines[1],
    );
    for board in [&mut first, &mut second] {
        assert!(board.board.wait().unwrap().success());
        let notices = messages(&call(&board.lead(), &["read", "lead", "--json"]));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0]["sender"], "malakowner");
        assert_eq!(notices[0]["sender_name"], "owner");
        assert_eq!(notices[0]["kind"], "note");
        assert!(notices[0]["sender_trace"].is_object(), "{notices:?}");
        assert_eq!(notices[0]["body"], serde_json::json!(expected.as_bytes()));
    }
    assert_eq!(
        stopped
            .without_handover(&["board", "status"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(3)
    );
}

#[test]
fn a_stored_upgrade_notice_survives_another_board_restart() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let mut board = old_board(&fixture);
    let _stop = StopDetached(board.config.clone());
    run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));
    assert!(board.board.wait().unwrap().success());
    run(&mut board.without_handover(&["board", "stop"]));

    board.board = spawn_board(command(&fixture, &[]), &board.config);
    board.wait_until_ready();

    assert!(call(&board.lead(), &["get", "lead", "1"]).contains("Note from owner in lead #1"));
}

#[test]
fn the_older_upgrade_handoff_uses_the_previous_served_build_for_the_notice() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    let before = build(&fixture.exe);
    let mut board = old_board(&fixture);
    let _stop = StopDetached(board.config.clone());
    replace_with_current_build(&fixture);
    let installed = build(&fixture.exe);
    let plan = serde_json::json!({
        "config": board.config, "name": "board", "run": board.status()["run"], "workers": []
    });

    run(command(&fixture, &["__upgrade-finish", "skip", &installed]).arg(plan.to_string()));

    assert!(board.board.wait().unwrap().success());
    let notices = messages(&call(&board.lead(), &["read", "lead", "--json"]));
    assert_eq!(notices.len(), 1, "{notices:?}");
    let expected =
        format!("Malak upgraded (build {before} -> {installed}). Re-read the malak skill.");
    let body = notices[0]["text"].as_str().unwrap();
    assert!(body.starts_with(&expected), "{body}");
    assert_eq!(body.lines().count(), 6, "{body}");
}

#[test]
fn an_identical_local_build_does_not_restart_the_board() {
    let fixture = upgrade_fixture();
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    let before = board.status();

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(fixture.called.exists());
    assert!(output.contains("already up to date"));
    assert_eq!(board.status()["run"], before["run"]);
}

#[test]
fn an_identical_local_build_leaves_the_lead_inbox_empty() {
    let fixture = upgrade_fixture();
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);

    run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(messages(&call(&board.lead(), &["read", "lead", "--json"])).is_empty());
}

fn replace_with_current_build(fixture: &CargoInstall) {
    let next = fixture.exe.with_extension("next");
    fs::copy(MALAK, &next).unwrap();
    fs::rename(next, &fixture.exe).unwrap();
}

#[test]
fn an_installed_release_finishes_pending_restarts_without_cargo() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    let mut older = old_board(&fixture);
    let _stop = StopDetached(older.config.clone());
    let before = older.status();
    replace_with_current_build(&fixture);
    let current = old_board(&fixture);
    let already_current = current.status()["run"].clone();

    let output = run(&mut command(
        &fixture,
        &["upgrade", "--version", VERSION, "--yes"],
    ));

    assert!(!fixture.called.exists());
    assert!(output.contains("Finishing pending work"), "{output}");
    assert_eq!(output.matches("Restarted ").count(), 1, "{output}");
    assert!(older.board.wait().unwrap().success());
    let after = older.status();
    assert_ne!(after["run"], before["run"]);
    assert_eq!(after["board_build"], build(&fixture.exe));
    assert_eq!(current.status()["run"], already_current);
}

#[test]
fn an_identical_local_install_finishes_pending_restarts() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    let mut older = old_board(&fixture);
    let _stop = StopDetached(older.config.clone());
    replace_with_current_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);

    let output = run(command(&fixture, &["upgrade", "--path"])
        .arg(source)
        .arg("--yes"));

    assert!(fixture.called.exists());
    assert!(output.contains("Finishing pending work"), "{output}");
    assert!(older.board.wait().unwrap().success());
    assert_eq!(older.status()["board_build"], build(&fixture.exe));
}

#[test]
fn upgrade_leaves_a_replacement_run_started_during_installation_alone() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    let _stop = StopDetached(board.config.clone());
    let before = board.status();
    cargo(
        &fixture,
        &format!(
            r#"
old="$CARGO_INSTALL_ROOT/bin/malak"
"$old" --config "$MALAK_TEST_BOARD" board stop
"$old" --config "$MALAK_TEST_BOARD" board start --detach
{INSTALL}
"#,
        ),
    );

    let result = command(&fixture, &["upgrade", "--yes", "--path"])
        .arg(source)
        .env("MALAK_TEST_BOARD", &board.config)
        .output()
        .unwrap();

    let output = String::from_utf8_lossy(&result.stdout);
    assert_eq!(result.status.code(), Some(3), "{result:?}");
    assert!(
        output.contains("the board run changed after the plan"),
        "{output}"
    );
    let replacement = board.status();
    assert_ne!(replacement["run"], before["run"]);
    assert_eq!(replacement["board_build"], before["board_build"]);
}

#[test]
fn a_forced_older_target_uses_the_fallback_and_checks_the_served_build() {
    let fixture = upgrade_fixture();
    cargo(&fixture, INSTALL);
    let older = fixture.tools.join("older");
    executable(
        &older,
        r#"
if [ "$1" = "__upgrade-finish" ]; then exit 2; fi
exec "$MALAK_TEST_BOARD_EXE" "$@"
"#,
    );
    let board = old_board(&fixture);
    let _stop = StopDetached(board.config.clone());
    let before = board.status();

    let result = command(
        &fixture,
        &["upgrade", "--version", "0.0.0", "--force", "--yes"],
    )
    .env("MALAK_TEST_REPLACEMENT", older)
    .env("MALAK_TEST_BOARD_EXE", MALAK)
    .output()
    .unwrap();

    let output = String::from_utf8_lossy(&result.stdout);
    assert_eq!(result.status.code(), Some(3), "{result:?}");
    assert!(output.contains("previous process to finish"), "{output}");
    assert!(
        output.contains("the board did not report a new run on the installed build"),
        "{output}"
    );
    assert_ne!(board.status()["run"], before["run"]);
    let cargo_args = fs::read_to_string(&fixture.called).unwrap();
    assert!(
        cargo_args.ends_with("--tag\nv0.0.0\nmalak\n"),
        "{cargo_args}"
    );
}

#[test]
fn cargo_failure_leaves_the_integration_and_board_run_untouched() {
    let fixture = upgrade_fixture();
    let source = checkout(&fixture.home, VERSION);
    let board = old_board(&fixture);
    fs::create_dir(fixture.home.path().join(".codex")).unwrap();
    run(&mut command(&fixture, &["recipe", "agents", "--yes"]));
    let manifest = fixture.home.path().join(".malak/agents.json");
    let before = fs::read(&manifest).unwrap();
    let previous_run = board.status()["run"].clone();
    let previous_build = build(&fixture.exe);
    cargo(&fixture, "echo 'stub cargo build error' >&2\nexit 17");

    let result = command(&fixture, &["upgrade", "--yes", "--path"])
        .arg(source)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(3));
    assert!(stderr.contains("stub cargo build error"), "{stderr}");
    assert!(
        stderr.contains("cargo install failed (exit status: 17)"),
        "{stderr}"
    );
    assert_eq!(build(&fixture.exe), previous_build);
    assert_eq!(fs::read(manifest).unwrap(), before);
    assert_eq!(board.status()["run"], previous_run);
}

#[test]
fn upgrade_refuses_a_downgrade_before_cargo_runs() {
    let fixture = upgrade_fixture();
    let result = command(&fixture, &["upgrade", "--version", "0.0.0", "--yes"])
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("refusing to downgrade"));
    assert!(!fixture.called.exists());
}

#[test]
fn an_upgrade_does_not_create_an_agents_manifest() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    fs::create_dir(fixture.home.path().join(".codex")).unwrap();

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(output.contains("No installed skill and reminders to refresh."));
    assert!(!fixture.home.path().join(".malak/agents.json").exists());
    assert!(!fixture.home.path().join(".codex/hooks.json").exists());
    assert!(!output.contains("Codex /hooks"));
}

#[test]
fn an_upgrade_keeps_matching_hooks_and_skips_the_codex_trust_prompt() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let root = fixture.home.path();
    fs::create_dir(root.join(".codex")).unwrap();
    run(&mut command(&fixture, &["recipe", "agents", "--yes"]));
    let manifest = root.join(".malak/agents.json");
    let before = fs::read(&manifest).unwrap();

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(output.contains("No installed skill or reminders changed."));
    assert!(!output.contains(HOOK_REVIEW), "{output}");
    assert_eq!(fs::read(manifest).unwrap(), before);
}

#[test]
fn an_installed_release_finishes_pending_agent_setup_without_cargo() {
    let fixture = upgrade_fixture();
    let home = fixture.home.path();
    fs::create_dir(home.join(".codex")).unwrap();
    run(&mut command(&fixture, &["recipe", "agents", "--yes"]));
    let manifest = home.join(".malak/agents.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    state["complete"] = Value::from(false);
    let before = serde_json::to_vec(&state).unwrap();
    fs::write(&manifest, &before).unwrap();
    let hooks = home.join(".codex/hooks.json");
    fs::remove_file(&hooks).unwrap();

    let preview = run(&mut command(
        &fixture,
        &["upgrade", "--version", VERSION, "--dry-run"],
    ));
    assert!(
        preview.contains("Refresh the installed skill and reminders"),
        "{preview}"
    );
    assert_eq!(fs::read(&manifest).unwrap(), before);
    assert!(!hooks.exists());

    let output = run(&mut command(
        &fixture,
        &["upgrade", "--version", VERSION, "--yes"],
    ));

    assert!(!fixture.called.exists());
    assert!(output.contains("Finishing pending work"), "{output}");
    assert!(output.contains("Codex /hooks"), "{output}");
    assert!(hooks.is_file());
    let state: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(state["complete"], true);
}

#[test]
fn an_installed_release_finishes_a_pending_managed_skill_refresh() {
    let fixture = upgrade_fixture();
    let home = fixture.home.path();
    fs::create_dir(home.join(".codex")).unwrap();
    run(&mut command(&fixture, &["recipe", "agents", "--yes"]));
    let manifest = home.join(".malak/agents.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    state["skills"][0]["data"] = Value::from("b2xk");
    fs::write(&manifest, serde_json::to_vec(&state).unwrap()).unwrap();
    let skill = home.join(".codex/skills/malak/SKILL.md");
    let expected = fs::read(&skill).unwrap();
    fs::write(&skill, b"old").unwrap();

    let output = run(&mut command(
        &fixture,
        &["upgrade", "--version", VERSION, "--yes"],
    ));

    assert!(!fixture.called.exists());
    assert!(output.contains("Refreshed skill"), "{output}");
    assert!(!output.contains(HOOK_REVIEW), "{output}");
    assert_eq!(fs::read(skill).unwrap(), expected);
}

#[test]
fn upgrade_refreshes_tracked_skills_and_hooks_and_preserves_notification_history() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let root = fixture.home.path();
    let previous = fixture.tools.join("malak-before-upgrade");
    fs::copy(&fixture.exe, &previous).unwrap();
    fs::create_dir(root.join(".codex")).unwrap();
    fs::create_dir(root.join(".claude")).unwrap();
    run(&mut installed_command(
        &fixture,
        &previous,
        &["recipe", "agents", "--yes"],
    ));
    let skill = root.join(".codex/skills/malak/SKILL.md");
    let claude_skill = root.join(".claude/skills/malak/SKILL.md");
    let fresh_skill = fs::read(&skill).unwrap();
    let manifest = root.join(".malak/agents.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    fs::write(&skill, b"old").unwrap();
    fs::write(&claude_skill, b"edited").unwrap();
    for tracked in state["skills"].as_array_mut().unwrap() {
        tracked["data"] = Value::from("b2xk"); // base64 for the previous skill.
    }
    state["notified"] = serde_json::json!(["thread-kept"]);
    fs::write(&manifest, state.to_string()).unwrap();
    let hooks = root.join(".codex/hooks.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&hooks).unwrap()).unwrap();
    settings["user_setting"] = Value::from(true);
    fs::write(&hooks, settings.to_string()).unwrap();

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(output.contains("Refreshed skill"), "{output}");
    assert!(output.contains("Keeping edited skill"), "{output}");
    assert_eq!(output.matches(HOOK_REVIEW).count(), 1, "{output}");
    assert_eq!(fs::read(&skill).unwrap(), fresh_skill);
    assert_eq!(fs::read(&claude_skill).unwrap(), b"edited");
    let settings: Value = serde_json::from_slice(&fs::read(&hooks).unwrap()).unwrap();
    assert_eq!(settings["user_setting"], true);
    let groups = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let updated = groups[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(updated.contains(fixture.exe.to_str().unwrap()), "{updated}");
    let state: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(state["notified"], serde_json::json!(["thread-kept"]));
    run(&mut command(
        &fixture,
        &["recipe", "agents", "--remove", "--yes"],
    ));
    let remaining: Value = serde_json::from_slice(&fs::read(hooks).unwrap()).unwrap();
    assert_eq!(remaining, serde_json::json!({"user_setting": true}));
}

#[test]
fn upgrade_reports_changed_hooks_in_a_claude_only_installation() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let previous = fixture.tools.join("malak-before-upgrade");
    fs::copy(&fixture.exe, &previous).unwrap();
    fs::create_dir(fixture.home.path().join(".claude")).unwrap();
    run(&mut installed_command(
        &fixture,
        &previous,
        &["recipe", "agents", "--yes"],
    ));

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert_eq!(output.matches(HOOK_REVIEW).count(), 1, "{output}");
    assert!(output.contains("restart Claude Code"), "{output}");
    assert!(output.contains("Codex /hooks"), "{output}");
}

#[test]
fn upgrade_notifies_a_codex_thread_that_already_received_the_installation_notice() {
    let fixture = upgrade_fixture();
    change_build(&fixture);
    cargo(&fixture, INSTALL);
    let source = checkout(&fixture.home, VERSION);
    let home = fixture.home.path();
    fs::create_dir(home.join(".codex")).unwrap();
    let codex = fixture.tools.join("codex");
    fs::write(&codex, include_str!("../fixtures/codex_notice.py")).unwrap();
    fs::set_permissions(&codex, Permissions::from_mode(0o700)).unwrap();
    run(&mut command(
        &fixture,
        &["recipe", "agents", "--yes", "--notify-open"],
    ));
    let previous = build(&fixture.exe);

    let output = run(command(&fixture, &["upgrade", "--yes", "--path"]).arg(source));

    assert!(
        output.contains("Notified 1 open Codex thread(s)."),
        "{output}"
    );
    let notices: Vec<_> = fs::read_to_string(home.join("injected.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[1]["threadId"], "opened-thread");
    let installed = build(&fixture.exe);
    assert_eq!(
        notices[1]["items"][0]["content"][0]["text"],
        format!("Malak upgraded (build {previous} -> {installed}). Re-read the malak skill.")
    );
}
