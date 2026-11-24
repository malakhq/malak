use super::*;
use crate::cli::mailbox::{
    model::{
        Body, Preview, Question, QuestionKind, Status,
        tests::{event, status},
    },
    view::{Action, copy_text},
};
use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};

fn fixture() -> Model {
    let mut snapshot = status();
    snapshot.participants[1].name = "a-participant-with-a-name-longer-than-the-screen".into();
    snapshot.channels[1].head = 3;
    snapshot.channels[1].creation = "fedcba9876543210fedcba9876543210".into();
    let mut questions = snapshot.channels[0].clone();
    questions.name = "questions".into();
    snapshot.channels.push(questions);
    let mut model = Model::new(snapshot, None);
    model.set_questions(Some(vec![Question {
        position: 61,
        creation: "questions-creation".into(),
        sender: "lead".into(),
        at_ms: 0,
        sent_at: [0, 0],
        first_line: "Which repository URL?".into(),
        kind: QuestionKind::Owner,
        body: "Which repository URL?".into(),
    }]));
    model.apply(event("stored", "lead", "lead", "first", 7));
    model.apply(event("stored", "lead", "lead", "first", 8));
    model
}

fn ready_view() -> View {
    let mut view = View::default();
    view.startup.history_ready = true;
    view.startup.history_complete = true;
    view
}

fn activity_fixture() -> Model {
    let mut model = fixture();
    for (position, text) in [(7, "First activity body"), (8, "Second activity body")] {
        set_body(&mut model, position, text, None);
    }
    model
}

fn set_body(model: &mut Model, position: u64, text: &str, trace: Option<&str>) {
    let message = event("stored", "lead", "lead", "first", position);
    model.fetch_result(
        message.address().unwrap(),
        Some(Preview {
            sender: "lead".into(),
            sender_trace: None,
            kind: "note".into(),
            sent_at: [0, position * 1_000_000],
            trace: trace.map(str::to_owned),
            text: None,
            cut_bytes: None,
            has_reply_to: false,
            reply_to: None,
            quote: None,
            body: Some(Body { text: text.into() }),
        }),
    );
}

fn paint(model: &Model, view: &mut View, width: u16, height: u16) -> TestBackend {
    paint_color(model, view, width, height, true)
}

fn paint_color(
    model: &Model,
    view: &mut View,
    width: u16,
    height: u16,
    color: bool,
) -> TestBackend {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            draw(
                frame,
                model,
                "lanes",
                23 * 3_600_000 + 19 * 60_000,
                view,
                color,
                None,
            )
        })
        .unwrap();
    terminal.backend().clone()
}

fn focused_activity() -> View {
    ready_view()
}

fn activity_section(screen: &str) -> &str {
    screen
        .split_once("[Activity]")
        .unwrap()
        .1
        .split_once('╞')
        .unwrap()
        .0
}

fn message_start(width: u16) -> u16 {
    if width >= 100 { width / 4 + 1 } else { 0 }
}

fn row_containing(screen: &str, text: &str) -> u16 {
    screen.lines().position(|line| line.contains(text)).unwrap() as u16
}

#[test]
fn activity_previews_wrap_words_and_limit_selected_and_unselected_lines() {
    let mut model = fixture();
    // The words fill both widths and leave a cell for the ellipsis.
    let words = ("abcdefg ".repeat(100)).trim_end().to_owned();
    set_body(&mut model, 8, &words, None);
    set_body(&mut model, 7, &words, None);

    for width in [120, 45] {
        let screen = paint(&model, &mut focused_activity(), width, 30).to_string();
        let activity = activity_section(&screen);
        for (header, count, ends_at) in [("#8", 3, "#7"), ("#7", 2, "\u{0}")] {
            let preview = activity
                .split_once(header)
                .unwrap()
                .1
                .split(ends_at)
                .next()
                .unwrap();
            let lines: Vec<_> = preview
                .lines()
                .filter_map(|line| {
                    line.split_once("┆ ")
                        .map(|(_, body)| body.trim_end_matches([' ', '"']))
                })
                .filter(|line| !line.starts_with("reply"))
                .collect();
            assert_eq!(lines.len(), count, "{screen}");
            assert!(lines.last().unwrap().ends_with("abcdefg…"), "{screen}");
            assert!(
                lines[..count - 1]
                    .iter()
                    .all(|line| line.ends_with("abcdefg")),
                "{screen}"
            );
        }
    }
}

#[test]
fn activity_separates_each_row_with_a_blank_line() {
    let model = activity_fixture();

    for width in [120, 45] {
        let terminal = paint(&model, &mut focused_activity(), width, 30);
        let screen = terminal.to_string();
        let next = row_containing(&screen, "#7");
        assert!(
            (message_start(width)..width).all(|x| terminal.buffer()[(x, next - 1)].symbol() == " "),
            "{screen}"
        );
    }
}

#[test]
fn activity_status_aligns_on_the_right_and_only_status_has_delivery_color() {
    let mut model = activity_fixture();
    model.enable_activity();
    let mut ack = event("acked", "lead", "lead", "first", 7);
    ack.at_ms = Some(0);
    model.apply(ack);

    for width in [120, 45] {
        for color in [true, false] {
            let terminal = paint_color(&model, &mut focused_activity(), width, 30, color);
            let screen = terminal.to_string();
            for (state, hue) in [("waiting", Color::Yellow), ("acknowledged", Color::Green)] {
                let row = screen
                    .lines()
                    .position(|line| {
                        line.contains(state) && line.contains('#') && line.contains('▾')
                    })
                    .unwrap() as u16;
                let cells = (0..width)
                    .map(|x| &terminal.buffer()[(x, row)])
                    .collect::<Vec<_>>();
                let status = if state == "acknowledged" {
                    "acknowledged 23h"
                } else {
                    state
                };
                let start = usize::from(width) - 2 - status.len();
                let status_cells = &cells[start..usize::from(width) - 2];
                assert_eq!(
                    status_cells
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>(),
                    status,
                    "{screen}"
                );
                assert!(
                    status_cells
                        .iter()
                        .all(|cell| cell.fg == if color { hue } else { Color::Reset }),
                    "{screen}"
                );
                let heading = usize::from(message_start(width)) + 2;
                assert_eq!(cells[heading].fg, Color::Reset, "{screen}");
                assert!(cells[heading].modifier.contains(Modifier::BOLD), "{screen}");
            }
        }
    }
}

#[test]
fn a_live_revocation_marks_the_tasks_status_red_at_both_widths() {
    let mut model = Model::new(status(), None);
    model.enable_activity();
    let mut task = event("stored", "lead", "to/a0123456", "second", 1);
    task.kind = Some("task".into());
    model.apply(task);
    let mut revoke = event("owner", "a0123456", "to/a0123456", "second", 1);
    revoke.operation = Some("REVOKE".into());
    revoke.outcome = Some("done".into());
    model.apply(revoke);

    for width in [120, 45] {
        for color in [true, false] {
            let terminal = paint_color(&model, &mut focused_activity(), width, 30, color);
            let screen = terminal.to_string();
            let y = row_containing(&screen, "task #1");
            let row = screen.lines().nth(usize::from(y)).unwrap();
            assert!(row.contains("revoked"), "{screen}");
            assert_eq!(
                terminal.buffer()[(width - 9, y)].fg,
                if color { Color::Red } else { Color::Reset },
                "{screen}"
            );
        }
    }
}

#[test]
fn live_names_prefix_people_and_leave_channel_names_unprefixed() {
    let mut model = Model::new(status(), None);
    for (id, channel, creation, position, kind) in [
        ("lead", "to/a0123456", "second", 1, "task"),
        ("a0123456", "lead", "first", 10, "report"),
        ("lead", "questions", "third", 1, "note"),
    ] {
        let mut message = event("stored", id, channel, creation, position);
        message.kind = Some(kind.into());
        model.apply(message);
    }

    for width in [120, 45] {
        let screen = paint(&model, &mut focused_activity(), width, 40).to_string();
        let activity = activity_section(&screen);
        assert!(activity.contains("@lead → questions"), "{screen}");
        assert!(activity.contains("@worker → @lead"), "{screen}");
        assert!(activity.contains("@lead → @worker"), "{screen}");
        assert!(screen.contains("PARTICIPANTS"), "{screen}");
        assert_eq!(screen.matches("@worker").count(), 3, "{screen}");
    }
}

#[test]
fn activity_exports_only_the_selected_messages_trace_and_reply() {
    let mut model = activity_fixture();
    set_body(&mut model, 8, "Traced", Some("abcc01230123456789"));
    let mut view = focused_activity();

    for width in [120, 45] {
        let screen = paint(&model, &mut view, width, 30).to_string();
        let activity = activity_section(&screen);
        assert_eq!(activity.matches("reply lead").count(), 1, "{screen}");
        assert!(activity.contains("trace abcc0123…"), "{screen}");
        assert!(screen.contains("t trace"), "{screen}");
        assert_eq!(
            view.key(KeyCode::Char('c')),
            Action::Copy("lead first 8".into())
        );
        assert_eq!(
            view.key(KeyCode::Char('y')),
            Action::CopyTrace("abcc01230123456789".into())
        );
        view.key(KeyCode::Down);
        let screen = paint(&model, &mut view, width, 30).to_string();
        assert!(!screen.contains("t trace"), "{screen}");
        assert_eq!(view.key(KeyCode::Char('t')), Action::None);
        view.key(KeyCode::Up);
    }
}

#[test]
fn scrolling_a_full_activity_body_keeps_the_row_selected_until_preview_returns() {
    let mut model = fixture();
    let body = (1..=50)
        .map(|n| format!("body line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    set_body(&mut model, 8, &body, None);

    for width in [120, 45] {
        let mut view = focused_activity();
        paint(&model, &mut view, width, 24);
        view.key(KeyCode::Enter);
        let first = paint(&model, &mut view, width, 24).to_string();
        assert!(first.contains("lines 1–"), "{first}");
        for _ in 0..50 {
            view.key(KeyCode::Down);
        }
        let end = paint(&model, &mut view, width, 24).to_string();
        assert!(end.contains("body line 50"), "{end}");
        assert!(end.contains("50 of 50"), "{end}");
        view.key(KeyCode::Esc);
        let preview = paint(&model, &mut view, width, 24).to_string();
        assert!(preview.contains("⏎ open"), "{preview}");
        assert!(!preview.contains("body line 50"), "{preview}");
        assert_eq!(
            view.key(KeyCode::Char('c')),
            Action::Copy("lead first 8".into())
        );
    }
}

#[test]
fn activity_keeps_a_refused_call_as_a_row() {
    let mut model = fixture();
    let mut refusal = event("denied", "a0123456", "lead", "first", 1);
    refusal.operation = Some("send".into());
    refusal.outcome = Some("refused".into());
    model.apply(refusal);

    let screen = paint(&model, &mut ready_view(), 120, 30).to_string();

    assert!(screen.contains("send for @"), "{screen}");
    assert!(screen.contains("refused"), "{screen}");
}

#[test]
fn activity_keeps_a_revoke_as_a_row() {
    let mut model = fixture();
    let mut revoke = event("owner", "a0123456", "lead", "first", 1);
    revoke.channel = None;
    revoke.operation = Some("REVOKE".into());
    revoke.outcome = Some("done".into());
    model.apply(revoke);

    let screen = paint(&model, &mut ready_view(), 120, 30).to_string();

    assert!(screen.contains("Owner REVOKE"), "{screen}");
}

#[test]
fn activity_hides_self_test_messages_kept_in_history() {
    let mut model = fixture();
    let probe = event("stored", "lead", "selftest/lead-hook", "probe", 1);
    let address = probe.address().unwrap();
    model.apply(probe);

    let screen = paint(&model, &mut ready_view(), 120, 30).to_string();

    assert!(!screen.contains("selftest/lead-hook"), "{screen}");
    assert!(model.stored(&address).is_some());
}

#[test]
fn activity_puts_receipt_ages_on_the_message_row() {
    let mut model = fixture();
    model.enable_activity();
    let now = 23 * 3_600_000 + 19 * 60_000;
    let mut read = event("read", "lead", "lead", "first", 8);
    read.at_ms = Some(now - 4 * 60_000);
    model.apply(read);
    let read = paint(&model, &mut ready_view(), 45, 24).to_string();

    let mut ack = event("acked", "lead", "lead", "first", 8);
    ack.at_ms = Some(now - 3 * 60_000);
    model.apply(ack);
    let ack = paint(&model, &mut ready_view(), 45, 24).to_string();

    let read_row = read.lines().find(|row| row.contains("message #8")).unwrap();
    let ack_row = ack.lines().find(|row| row.contains("message #8")).unwrap();
    assert!(read_row.contains("read 4m"), "{read}");
    assert!(ack_row.contains("acknowledged 3m"), "{ack}");
    assert_eq!(model.activity().count(), 2);
}

#[test]
fn a_stopped_board_displays_its_stop_time_in_the_header() {
    let mut view = ready_view();
    view.stopped_at = Some(23 * 3_600_000 + 19 * 60_000);

    let screen = paint(&fixture(), &mut view, 45, 24).to_string();
    let header = screen.split('└').next().unwrap();

    assert!(header.contains("board stopped 23:19 UTC"), "{screen}");
}

#[test]
fn the_box_contains_the_board_clock_and_all_five_channels() {
    for width in [30, 45, 80, 120] {
        let screen = paint(&fixture(), &mut ready_view(), width, 24).to_string();
        let header = screen.split('└').next().unwrap();
        for label in [
            "Malak · lanes",
            "23:19 UTC",
            "lead 5",
            "questions 1 (1 you)",
            "orders 0",
            "inboxes 1 waiting",
            "relay –",
        ] {
            assert!(header.contains(label), "{label} at {width}: {screen}");
        }
        assert!(screen.find('└').unwrap() < screen.find("1 for you · #61").unwrap());
    }
}

#[test]
fn the_footer_stays_at_the_bottom_when_details_take_up_space() {
    let model = fixture();
    let mut view = ready_view();
    for width in [30, 45, 80] {
        let closed = paint(&model, &mut view, width, 24).to_string();
        view.key(KeyCode::Enter);
        let open = paint(&model, &mut view, width, 24).to_string();
        for screen in [&closed, &open] {
            let lines = screen.lines().collect::<Vec<_>>();
            let rule = lines.iter().position(|line| line.contains('╞')).unwrap();
            assert!(lines[rule + 1..].iter().all(|line| !line.contains('┆')));
            assert!(
                lines[rule + 1..].iter().any(|line| line.contains("q quit")),
                "{screen}"
            );
        }
        view.key(KeyCode::Enter);
    }
}

#[test]
fn the_header_and_footer_stay_at_the_edges_on_redraw_and_resize() {
    let model = fixture();
    let mut view = ready_view();
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    for (width, height) in [(120, 24), (120, 24), (45, 18), (45, 2), (45, 1), (120, 24)] {
        terminal.backend_mut().resize(width, height);
        view.key(KeyCode::Enter);
        let frame = terminal
            .draw(|frame| draw(frame, &model, "lanes", 0, &mut view, false, None))
            .unwrap();
        assert_eq!(frame.area.height, height);
        let screen = terminal.backend().to_string();
        let rows: Vec<_> = screen.lines().collect();
        assert_eq!(rows.len(), usize::from(height), "{screen}");
        assert!(rows[0].contains("Malak · lanes"), "{screen}");
        if height > 1 {
            assert!(
                rows.iter()
                    .rev()
                    .take(4)
                    .any(|line| line.contains("q quit")),
                "{screen}"
            );
        }
    }
}

#[test]
fn participant_columns_keep_last_activity_only_when_it_fits() {
    for (width, last) in [
        (160, true),
        (120, true),
        (100, false),
        (80, true),
        (45, true),
    ] {
        let screen = paint(&fixture(), &mut ready_view(), width, 24).to_string();
        let heading = screen
            .lines()
            .find(|line| line.contains("PARTICIPANTS"))
            .unwrap();
        let heading = heading.split('│').next().unwrap();
        assert_eq!(heading.contains("last"), last, "{screen}");
        assert!(
            heading.trim_end_matches([' ', '"']).ends_with("waiting"),
            "{screen}"
        );
        let worker = screen
            .lines()
            .find(|line| line.contains("@a-part"))
            .unwrap();
        let worker = worker.split('│').next().unwrap();
        assert!(
            worker.trim_end_matches([' ', '"']).ends_with("2"),
            "{screen}"
        );
    }
}

#[test]
fn a_long_participant_name_keeps_its_prefix_and_ends_in_an_ellipsis() {
    let mut snapshot = status();
    snapshot.participants[1].name = "界".repeat(50);
    let model = Model::new(snapshot, None);
    for width in [160, 120, 100, 80, 45] {
        let screen = paint(&model, &mut ready_view(), width, 24).to_string();
        let row = screen.lines().find(|row| row.contains("@界")).unwrap();
        let name = row
            .split_whitespace()
            .find(|word| word.starts_with('@'))
            .unwrap();
        assert!(
            name.ends_with('…') && name[1..].trim_end_matches('…').chars().all(|c| c == '界'),
            "{screen}"
        );
    }
}

#[test]
fn the_participant_filter_shows_last_activity_when_the_column_is_hidden() {
    let mut model = fixture();
    let mut sent = event("stored", "a0123456", "lead", "first", 9);
    sent.at_ms = Some(23 * 3_600_000 + 6 * 60_000);
    model.apply(sent);
    let mut view = ready_view();
    view.focus = Pane::Participants;
    paint(&model, &mut view, 100, 24);
    view.key(KeyCode::Down);
    paint(&model, &mut view, 100, 24);

    view.key(KeyCode::Enter);

    let screen = paint(&model, &mut view, 100, 24).to_string();
    assert!(
        screen.contains("Activity · @a-participant-with-a-name-longer-than-the-screen · last 13m"),
        "{screen}"
    );
}

#[test]
fn participant_rows_hide_inbox_addresses_at_every_width() {
    let model = fixture();
    for width in [45, 80, 120] {
        let screen = paint(&model, &mut ready_view(), width, 24).to_string();
        let participant = screen
            .lines()
            .find(|line| line.contains("@a-part"))
            .unwrap();
        assert!(
            !participant
                .split('│')
                .next()
                .unwrap()
                .contains("to/a0123456"),
            "{screen}"
        );
    }
}

fn reused_name_status() -> Status {
    let mut snapshot = status();
    let mut previous = snapshot.participants[1].clone();
    previous.id = "e1234567".into();
    previous.enrolled_at_ms = Some(1);
    previous.revoked = true;
    snapshot.participants[1].enrolled_at_ms = Some(2);
    snapshot.participants.push(previous);
    snapshot
}

#[test]
fn reused_names_keep_enrolment_numbers_across_the_archive() {
    let model = Model::new(reused_name_status(), None);
    let mut view = ready_view();
    paint(&model, &mut view, 120, 24);
    view.key(KeyCode::Char('a'));

    let screen = paint(&model, &mut view, 120, 24).to_string();
    let archive = row_containing(&screen, "Archived (1)");
    assert!(row_containing(&screen, "@worker (2)") < archive, "{screen}");
    assert!(archive < row_containing(&screen, "@worker (1)"), "{screen}");
}

#[test]
fn the_archive_counts_only_revoked_participants() {
    let mut snapshot = reused_name_status();
    let mut retired = snapshot.participants.last().unwrap().clone();
    retired.name = "retired".into();
    retired.id = "f1234567".into();
    snapshot.participants.push(retired);
    let model = Model::new(snapshot, None);

    let screen = paint(&model, &mut ready_view(), 120, 24).to_string();

    assert_eq!(screen.matches("Archived (2)").count(), 1, "{screen}");
}

#[test]
fn the_archive_starts_closed() {
    let model = Model::new(reused_name_status(), None);

    let screen = paint(&model, &mut ready_view(), 120, 24).to_string();

    assert!(screen.contains("▸ Archived (1)"), "{screen}");
    assert!(!screen.contains("@worker (1)"), "{screen}");
}

#[test]
fn a_opens_and_closes_the_archive() {
    let model = Model::new(reused_name_status(), None);
    let mut view = ready_view();
    let collapsed = paint(&model, &mut view, 120, 24).to_string();
    assert!(collapsed.contains("a show archive"), "{collapsed}");

    view.key(KeyCode::Char('a'));

    let expanded = paint(&model, &mut view, 120, 24).to_string();
    assert!(expanded.contains("a hide archive"), "{expanded}");
    assert!(expanded.contains("▾ Archived (1)"), "{expanded}");
    assert!(expanded.contains("@worker (1)"), "{expanded}");

    view.key(KeyCode::Char('a'));

    let collapsed = paint(&model, &mut view, 120, 24).to_string();
    assert!(collapsed.contains("▸ Archived (1)"), "{collapsed}");
    assert!(!collapsed.contains("@worker (1)"), "{collapsed}");
}

#[test]
fn archived_participants_keep_their_activity_order() {
    let mut snapshot = reused_name_status();
    let mut recent = snapshot.participants.last().unwrap().clone();
    recent.id = "f1234567".into();
    recent.name = "retired".into();
    snapshot.participants.insert(1, recent);
    let mut model = Model::new(snapshot, None);
    let mut newest = event("stored", "e1234567", "lead", "first", 8);
    newest.at_ms = Some(20);
    let mut older = event("stored", "f1234567", "lead", "first", 7);
    older.at_ms = Some(10);
    model.apply(newest);
    model.apply(older);
    let lanes = model.lanes_by_activity();

    let (archive, _) = participant_rows(&lanes, true);
    let names: Vec<_> = archive
        .iter()
        .filter_map(ParticipantRow::lane)
        .filter(|lane| lane.participant.revoked)
        .map(|lane| lane.participant.id.as_str())
        .collect();

    assert_eq!(names, ["e1234567", "f1234567"]);
}

#[test]
fn selecting_an_archived_participant_filters_by_its_enrolment() {
    let mut model = Model::new(reused_name_status(), None);
    model.apply(event("stored", "a0123456", "lead", "first", 7));
    model.apply(event("stored", "e1234567", "lead", "first", 8));
    model.apply(event("stored", "lead", "to/e1234567", "archived", 2));
    let mut view = ready_view();
    view.key(KeyCode::Tab);
    paint(&model, &mut view, 120, 24);
    view.key(KeyCode::Char('a'));
    paint(&model, &mut view, 120, 24);
    view.key(KeyCode::End);
    paint(&model, &mut view, 120, 24);

    view.key(KeyCode::Enter);

    let screen = paint(&model, &mut view, 120, 24).to_string();
    assert_eq!(view.filter.as_deref(), Some("e1234567"));
    assert!(screen.contains("message #8"), "{screen}");
    assert!(screen.contains("message #2"), "{screen}");
    assert!(!screen.contains("message #7"), "{screen}");
}

#[test]
fn the_expanded_archive_stays_above_activity_in_the_stacked_layout() {
    let model = Model::new(reused_name_status(), None);
    for width in [45, 80, 99] {
        let mut view = ready_view();
        view.key(KeyCode::Tab);
        paint(&model, &mut view, width, 24);
        view.key(KeyCode::Char('a'));

        let screen = paint(&model, &mut view, width, 24).to_string();
        let archive = row_containing(&screen, "Archived (1)");
        let archived = row_containing(&screen, "@worker (1)");
        assert!(archive < archived, "{screen}");
        assert!(archived < row_containing(&screen, "[Activity]"), "{screen}");
    }
}

#[test]
fn the_selected_participant_copies_its_inbox_channel() {
    let model = fixture();
    let mut view = ready_view();
    view.focus = Pane::Participants;
    paint(&model, &mut view, 45, 24);
    view.key(KeyCode::Down);
    paint(&model, &mut view, 45, 24);

    assert_eq!(
        view.key(KeyCode::Char('c')),
        Action::Copy("to/a0123456".into())
    );
}

#[test]
fn startup_shows_rows_while_review_counts_are_loading() {
    let mut model = fixture();
    let mut orders = status().channels.remove(0);
    orders.name = "orders".into();
    let mut snapshot = status();
    let mut questions = orders.clone();
    questions.name = "questions".into();
    snapshot.channels.extend([questions, orders]);
    model.update(snapshot);
    model.set_questions(None);
    let mut view = View::default();

    let loading = paint(&model, &mut view, 120, 24).to_string();
    assert!(loading.contains("orders loading…"), "{loading}");
    assert!(loading.contains("[Activity]"), "{loading}");
    assert!(loading.contains("Malak · lanes"), "{loading}");
    assert!(loading.contains('╞'), "{loading}");

    view.startup.history_ready = true;
    let ready = paint(&model, &mut view, 120, 24).to_string();
    for text in [
        "questions loading…",
        "orders loading…",
        "PARTICIPANTS",
        "[Activity]",
    ] {
        assert!(ready.contains(text), "{ready}");
    }
}

#[test]
fn the_split_uses_one_quarter_for_participants_and_stacks_under_100() {
    for (width, divider) in [
        (160, Some(40)),
        (120, Some(30)),
        (100, Some(25)),
        (80, None),
        (45, None),
    ] {
        let terminal = paint(&fixture(), &mut ready_view(), width, 24);
        let screen = terminal.to_string();
        let participant = row_containing(&screen, "PARTICIPANTS");
        let activity = row_containing(&screen, "[Activity]");
        if let Some(divider) = divider {
            assert_eq!(participant, activity, "{screen}");
            assert_eq!(terminal.buffer()[(divider, activity)].symbol(), "│");
        } else {
            assert!(participant < activity, "{screen}");
        }
    }
}

#[test]
fn arrow_keys_move_the_participant_highlight() {
    let model = fixture();
    let mut view = ready_view();
    view.focus = Pane::Participants;
    assert!(
        paint(&model, &mut view, 45, 24)
            .to_string()
            .contains("▶ @lead")
    );

    view.key(KeyCode::Down);

    let terminal = paint(&model, &mut view, 45, 24);
    let screen = terminal.to_string();
    let row = row_containing(&screen, "▶ @a-participant");
    assert!(
        terminal.buffer()[(0, row)]
            .modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn enter_filters_to_messages_sent_or_received_by_the_participant() {
    let mut model = activity_fixture();
    model.apply(event("stored", "a0123456", "lead", "first", 9));
    model.apply(event("stored", "lead", "to/a0123456", "second", 1));
    for width in [160, 120, 80, 45] {
        let mut view = ready_view();
        view.key(KeyCode::Tab);
        paint(&model, &mut view, width, 30);
        view.key(KeyCode::Down);
        paint(&model, &mut view, width, 30);

        view.key(KeyCode::Enter);

        let filtered = paint(&model, &mut view, width, 30).to_string();
        let messages = activity_section(&filtered);
        assert!(messages.contains("Activity · @a-participant"), "{filtered}");
        assert!(messages.contains("message #9"), "{filtered}");
        assert!(messages.contains("message #1"), "{filtered}");
        assert!(!messages.contains("Second activity body"), "{filtered}");

        view.key(KeyCode::Esc);
        let restored = paint(&model, &mut view, width, 30).to_string();
        assert!(restored.contains("Second activity body"), "{restored}");
    }
}

#[test]
fn activity_shows_each_visible_message_body_by_default() {
    let model = activity_fixture();
    for width in [120, 45] {
        let screen = paint(&model, &mut ready_view(), width, 24).to_string();
        let activity = screen.split_once("[Activity]").unwrap().1;
        for (position, text) in [(8, "Second activity body"), (7, "First activity body")] {
            let lead = activity
                .lines()
                .position(|line| line.contains(&format!("#{position}")) && line.contains('▾'))
                .unwrap();
            assert!(
                activity.lines().nth(lead + 1).unwrap().contains(text),
                "{screen}"
            );
        }
    }
}

#[test]
fn e_toggles_activity_previews_and_remembers_the_choice_on_tab_changes() {
    let model = activity_fixture();
    for width in [160, 120, 80, 45] {
        let mut view = ready_view();
        let open = paint(&model, &mut view, width, 24).to_string();
        assert!(open.contains("e collapse all"), "{open}");

        view.key(KeyCode::Char('e'));
        view.key(KeyCode::Char('2'));
        view.key(KeyCode::Char('1'));
        let closed = paint(&model, &mut view, width, 24).to_string();
        assert!(closed.contains("e expand all"), "{closed}");
        assert!(!closed.contains("activity body"), "{closed}");

        view.key(KeyCode::Char('e'));
        let reopened = paint(&model, &mut view, width, 24).to_string();
        assert_eq!(reopened.matches("activity body").count(), 2, "{reopened}");
    }
}

#[test]
fn tab_moves_focus_and_the_footer_follows_the_pane() {
    let model = fixture();
    let mut view = ready_view();
    let messages = paint(&model, &mut view, 80, 24).to_string();
    assert!(messages.contains("⏎ open"), "{messages}");
    assert!(messages.contains("e collapse all"), "{messages}");

    view.key(KeyCode::Tab);

    let people = paint(&model, &mut view, 80, 24).to_string();
    assert_eq!(view.focus, Pane::Participants);
    assert!(people.contains("⏎ filter"), "{people}");
    assert!(!people.contains("e collapse all"), "{people}");
}

#[test]
fn quick_jump_selects_the_labelled_row_in_either_pane() {
    let model = fixture();
    for focus in [Pane::Participants, Pane::Messages] {
        let mut view = ready_view();
        view.focus = focus;
        paint(&model, &mut view, 45, 30);
        view.key(KeyCode::Char('f'));
        let labels = paint(&model, &mut view, 45, 30).to_string();
        assert!(labels.contains("Esc/f cancel"), "{labels}");
        let (letter, _) = view.visible.jumps[1];

        view.key(KeyCode::Char(letter));

        paint(&model, &mut view, 45, 30);
        assert_eq!(view.focus, focus);
        assert!(!view.jumping);
        assert_eq!(view.cursor(focus).index, 1);
    }
}

#[test]
fn escape_and_f_both_clear_quick_jump_labels() {
    let model = fixture();
    let mut view = ready_view();
    paint(&model, &mut view, 45, 24);
    for cancel in [KeyCode::Esc, KeyCode::Char('f')] {
        view.key(KeyCode::Char('f'));
        let jumping = paint(&model, &mut view, 45, 24).to_string();
        assert!(jumping.contains("letter jump"), "{jumping}");

        assert_eq!(view.key(cancel), Action::Redraw);
        let normal = paint(&model, &mut view, 45, 24).to_string();
        assert!(!normal.contains("letter jump"), "{normal}");
    }
}

#[test]
fn copy_uses_the_complete_reply_address_even_on_a_narrow_screen() {
    let mut model = fixture();
    let creation = "fedcba9876543210fedcba9876543210";
    model.apply(event("stored", "lead", "to/a0123456", creation, 1));
    let mut view = ready_view();
    paint(&model, &mut view, 45, 24);

    let expected = "to/a0123456 fedcba9876543210fedcba9876543210 1";
    assert_eq!(view.key(KeyCode::Char('c')), Action::Copy(expected.into()));
    let mut wire = Vec::new();
    copy_text(&mut wire, expected).unwrap();
    assert_eq!(
        wire,
        b"\x1b]52;c;dG8vYTAxMjM0NTYgZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTAgMQ==\x07"
    );
}

#[test]
fn a_live_insert_preserves_the_selected_activity_message() {
    let mut model = fixture();
    let mut view = ready_view();
    paint(&model, &mut view, 80, 24);
    view.key(KeyCode::Down);
    paint(&model, &mut view, 80, 24);
    model.apply(event("stored", "lead", "lead", "first", 9));

    let screen = paint(&model, &mut view, 80, 24).to_string();
    let active = screen
        .lines()
        .find(|row| row.contains("▶ ") && row.contains("waiting"))
        .unwrap();
    assert!(active.contains("#7"), "{screen}");
}

#[test]
fn scrolling_away_holds_the_page_and_counts_new_rows_above_it() {
    for width in [160, 120, 100, 80, 45] {
        let mut model = activity_fixture();
        let mut view = ready_view();
        paint(&model, &mut view, width, 24);
        view.key(KeyCode::Down);
        paint(&model, &mut view, width, 24);

        for position in 9..=11 {
            model.apply(event("stored", "lead", "lead", "first", position));
            set_body(&mut model, position, "New activity above", None);
        }
        let paused = paint(&model, &mut view, width, 24).to_string();

        assert!(paused.contains("3 new · g live"), "{paused}");
        assert!(
            paused.split_once('╧').map_or_else(
                || paused.rsplit_once('╞').unwrap().1.contains("g live"),
                |(_, footer)| footer.contains("g live")
            ),
            "{paused}"
        );
        assert!(!paused.contains("New activity above"), "{paused}");
        assert!(
            paused
                .lines()
                .any(|line| line.contains("▶ ") && line.contains("#7")),
            "{paused}"
        );
    }
}

#[test]
fn a_late_arrival_above_the_paused_page_counts_as_new() {
    let mut model = activity_fixture();
    for position in 9..=30 {
        model.apply(event("stored", "lead", "lead", "first", position));
        set_body(&mut model, position, "Retained activity", None);
    }
    let mut view = ready_view();
    paint(&model, &mut view, 120, 24);
    view.key(KeyCode::End);
    paint(&model, &mut view, 120, 24);

    let mut late = event("stored", "lead", "lead", "first", 31);
    late.at_ms = Some(15);
    model.apply(late);

    let screen = paint(&model, &mut view, 120, 24).to_string();
    assert!(screen.contains("1 new · g live"), "{screen}");
}

#[test]
fn each_live_jump_binding_selects_the_requested_end_of_the_list() {
    for width in [160, 120, 100, 80, 45] {
        for (oldest, newest) in [
            (KeyCode::Char('G'), KeyCode::Char('g')),
            (KeyCode::End, KeyCode::Home),
        ] {
            let mut model = activity_fixture();
            let mut view = ready_view();
            paint(&model, &mut view, width, 24);

            view.key(oldest);
            let bottom = paint(&model, &mut view, width, 24).to_string();
            assert!(
                bottom
                    .lines()
                    .any(|line| line.contains("▶ ") && line.contains("#7")),
                "{bottom}"
            );
            view.key(newest);
            let top = paint(&model, &mut view, width, 24).to_string();
            assert!(
                top.lines()
                    .any(|line| line.contains("▶ ") && line.contains("#8")),
                "{top}"
            );
            assert!(!top.contains("g live"), "{top}");

            model.apply(event("stored", "lead", "lead", "first", 9));
            set_body(&mut model, 9, "New top item", None);
            let latest = paint(&model, &mut view, width, 24).to_string();
            assert!(
                latest
                    .lines()
                    .any(|line| line.contains("▶ ") && line.contains("#9")),
                "{latest}"
            );
        }
    }
}

#[test]
fn each_tab_highlights_its_name_without_replacing_the_frame() {
    for width in [160, 120, 80, 45] {
        let model = fixture();
        let mut view = ready_view();
        for (index, tab) in Tab::ALL.into_iter().enumerate() {
            view.key(KeyCode::Char(char::from(b'1' + index as u8)));
            let screen = paint_color(&model, &mut view, width, 24, false).to_string();
            assert!(screen.contains("Malak · lanes"), "{screen}");
            assert!(screen.contains("1 for you · #61"), "{screen}");
            assert!(screen.contains("PARTICIPANTS"), "{screen}");
            assert!(screen.contains(&format!("[{}]", tab.title())), "{screen}");
            assert!(
                screen.contains('╞') && screen.contains("q quit"),
                "{screen}"
            );
        }
    }
}

#[test]
fn the_questions_tab_displays_how_long_an_open_question_has_waited() {
    let mut view = ready_view();
    view.key(KeyCode::Char('2'));

    let screen = paint(&fixture(), &mut view, 45, 24).to_string();
    assert!(screen.contains("owner question #61 · 23h"), "{screen}");
}

#[test]
fn brackets_cycle_through_the_tabs() {
    let mut view = ready_view();
    view.key(KeyCode::Char('['));
    assert_eq!(view.tab, Tab::Trace);
    view.key(KeyCode::Char(']'));
    assert_eq!(view.tab, Tab::Activity);
}

#[test]
fn help_is_a_box_inside_the_mailbox_frame() {
    let model = fixture();
    let mut view = ready_view();
    view.key(KeyCode::Char('?'));
    let help = paint(&model, &mut view, 45, 24).to_string();
    assert!(help.contains("┌ Help ─"), "{help}");
    assert!(help.contains("Malak · lanes"), "{help}");
    assert!(help.contains("?/Esc close"), "{help}");
    assert!(
        help.contains("a  Show or hide archived participants"),
        "{help}"
    );

    view.key(KeyCode::Esc);

    let normal = paint(&model, &mut view, 45, 24).to_string();
    assert!(!normal.contains("┌ Help ─"), "{normal}");
}

#[test]
fn no_color_preserves_the_active_tab_and_selection_without_cell_colors() {
    for width in [160, 120, 80, 45] {
        let terminal = paint_color(&activity_fixture(), &mut ready_view(), width, 24, false);
        let screen = terminal.to_string();
        assert!(
            screen.contains("[Activity]") && screen.contains('▶'),
            "{screen}"
        );
        assert!(
            terminal
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
    }
}

mod multiboard {
    use super::*;

    fn paint_boards(entries: &[&BoardEntry], view: &mut View, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal
            .draw(|frame| draw_boards(frame, entries, 5_000, view, false, None))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn the_stacked_board_and_participants_align_their_waiting_counts() {
        let a = BoardEntry::ready_fixture("alpha", activity_fixture());
        let mut view = View::for_boards();
        paint_boards(&[&a], &mut view, 80);
        view.key(KeyCode::Tab);
        assert_eq!(view.board_key(KeyCode::Right, true), Some(Action::Redraw));

        let screen = paint_boards(&[&a], &mut view, 80);

        let column = |label, count| {
            let line = screen.lines().find(|line| line.contains(label)).unwrap();
            line.chars().position(|ch| ch == count).unwrap()
        };
        assert_eq!(column("alpha ●", '7'), column("@lead", '5'), "{screen}");
    }

    #[test]
    fn the_multi_board_footer_labels_enter_for_the_focused_pane() {
        let a = BoardEntry::ready_fixture("alpha", activity_fixture());
        let b = BoardEntry::ready_fixture("bravo", activity_fixture());
        let mut view = View::for_boards();
        for (focus, expected) in [
            (Pane::Messages, "⏎ open message"),
            (Pane::Participants, "⏎/→ open board"),
        ] {
            view.focus = focus;
            let screen = paint_boards(&[&a, &b], &mut view, 120);
            let (_, footer) = screen.rsplit_once('╞').unwrap();
            assert!(footer.contains(expected), "{footer}");
            assert_eq!(footer.matches('⏎').count(), 1, "{footer}");
        }
    }

    #[test]
    fn combined_rows_keep_board_tags_but_selected_board_rows_do_not() {
        let a = BoardEntry::ready_fixture("alpha", activity_fixture());
        let b = BoardEntry::ready_fixture("bravo", activity_fixture());
        let boards = [&a, &b];
        for width in [120, 80] {
            let mut view = View::for_boards();
            let all = paint_boards(&boards, &mut view, width);
            assert!(all.contains("#8 · [BOARD: ALPHA]"), "{all}");
            assert!(all.contains("#8 · [BOARD: BRAVO]"), "{all}");
            view.key(KeyCode::Tab);
            view.key(KeyCode::Down);
            let one = paint_boards(&boards, &mut view, width);
            assert!(one.contains("Malak · alpha"), "{one}");
            assert!(!one.contains("[BOARD:"), "{one}");
        }
    }

    #[test]
    fn combined_rows_show_a_message_from_a_different_session() {
        let mut model = activity_fixture();
        let mut stored = event("stored", "lead", "lead", "first", 9);
        stored.sender_trace = Some(
            serde_json::from_value(serde_json::json!({
                "transport": "unix", "cwd": "/test/project",
                "session": {"program": "agent", "pid": 42},
                "earlier_session": {"program": "agent", "pid": 41},
            }))
            .unwrap(),
        );
        model.apply(stored);
        let a = BoardEntry::ready_fixture("alpha", model);
        let b = BoardEntry::ready_fixture("bravo", activity_fixture());

        let screen = paint_boards(&[&a, &b], &mut View::for_boards(), 120);

        assert!(
            screen.contains("lead sent from agent 42 in /test/project (earlier: agent 41)"),
            "{screen}"
        );
    }

    #[test]
    fn combined_questions_group_owner_questions_before_newer_agent_questions() {
        let mut a = fixture();
        a.set_questions(Some(vec![Question {
            position: 42,
            creation: "first".into(),
            sender: "lead".into(),
            at_ms: 1_000,
            sent_at: [1, 0],
            first_line: "Owner question".into(),
            kind: QuestionKind::Owner,
            body: "Owner question".into(),
        }]));
        let mut b = fixture();
        b.set_questions(Some(vec![Question {
            position: 99,
            creation: "second".into(),
            sender: "worker".into(),
            at_ms: 3_000,
            sent_at: [3, 0],
            first_line: "Agent question".into(),
            kind: QuestionKind::Agent,
            body: "Agent question".into(),
        }]));
        let a = BoardEntry::ready_fixture("alpha", a);
        let b = BoardEntry::ready_fixture("bravo", b);
        let mut view = View::for_boards();
        view.tab = Tab::Questions;

        let screen = paint_boards(&[&a, &b], &mut view, 120);

        assert!(
            row_containing(&screen, "#42 · [BOARD: ALPHA]")
                < row_containing(&screen, "#99 · [BOARD: BRAVO]"),
            "{screen}",
        );
    }

    #[test]
    fn combined_clipboard_actions_include_the_selected_boards_name() {
        let mut model = fixture();
        set_body(&mut model, 8, "trace body", Some("a1b2"));
        let a = BoardEntry::ready_fixture("alpha", model);
        let b = BoardEntry::ready_fixture("bravo", Model::new(status(), None));
        let mut view = View::for_boards();
        paint_boards(&[&a, &b], &mut view, 120);

        assert_eq!(
            view.key(KeyCode::Char('c')),
            Action::Copy("--board alpha --reply-to lead first 8".into())
        );
        assert_eq!(
            view.key(KeyCode::Char('y')),
            Action::CopyTrace("malak --board alpha mailbox --trace a1b2".into())
        );
    }

    #[test]
    fn the_owner_question_heading_covers_both_boards_while_one_is_selected() {
        let a = BoardEntry::ready_fixture("alpha", fixture());
        let mut model = fixture();
        let mut questions = model.open_questions().unwrap().to_vec();
        questions[0].sent_at = [1, 0];
        questions[0].body = "Ship now?".into();
        model.set_questions(Some(questions));
        let b = BoardEntry::ready_fixture("bravo", model);
        let boards = [&a, &b];
        let mut view = View::for_boards();
        paint_boards(&boards, &mut view, 120);
        view.key(KeyCode::Tab);
        view.key(KeyCode::Down);

        let screen = paint_boards(&boards, &mut view, 120);

        assert!(screen.contains("Malak · alpha"), "{screen}");
        assert!(
            screen.contains(r#"⚑ 2 owner questions · newest on bravo: "Ship now?""#),
            "{screen}"
        );
    }
}
