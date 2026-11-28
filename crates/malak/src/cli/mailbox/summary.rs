use super::{
    messages::readable_channels,
    model::{Address, Channel, Model, Participant, Preview, Status},
    orders, questions, text,
};
use crate::cli::output::display;
use malak::Error;
use std::collections::{HashMap, hash_map::Entry};

pub(super) const ROWS: u16 = 5;
const ORDERS: &str = "orders";

/// Only channels this participant can get messages from contribute to a review.
struct Sources<'a> {
    questions: Option<Vec<&'a Channel>>,
    orders: Option<&'a Channel>,
}

impl<'a> Sources<'a> {
    fn new(
        channels: impl Iterator<Item = &'a Channel> + Clone,
        viewer: Option<&'a Participant>,
    ) -> Self {
        let readable = viewer
            .into_iter()
            .flat_map(|person| readable_channels(channels.clone(), person))
            .collect::<Vec<_>>();
        let orders = readable
            .iter()
            .copied()
            .find(|channel| channel.name == ORDERS && channel.floor < channel.head);
        let questions = readable
            .iter()
            .any(|channel| channel.name == questions::CHANNEL && channel.floor < channel.head)
            .then_some(readable);
        Self { questions, orders }
    }

    fn question_ranges(&self) -> Option<Vec<Range>> {
        Some(
            self.questions
                .as_ref()?
                .iter()
                .map(|channel| channel.retained_range())
                .collect(),
        )
    }
}

#[derive(Default)]
pub(super) struct Previews(HashMap<Address, Option<Preview>>);

impl Previews {
    pub fn take(
        &mut self,
        address: &Address,
        read: impl FnOnce(&Address) -> Result<Option<Preview>, Error>,
    ) -> Result<Option<Preview>, Error> {
        self.0.remove(address).map_or_else(|| read(address), Ok)
    }
}

/// The plain feed needs its summary before printing history. Keep the
/// authorized records read for that summary to seed the same history.
pub(super) fn load(
    model: &mut Model,
    snapshot: &Status,
    viewer: &str,
    mut read: impl FnMut(&Address) -> Result<Option<Preview>, Error>,
) -> Previews {
    let viewer = snapshot
        .participants
        .iter()
        .find(|person| person.id == viewer);
    let sources = Sources::new(snapshot.channels.iter(), viewer);
    let mut previews = Previews::default();
    let mut scan = |channels: &[&Channel]| {
        channels
            .iter()
            .flat_map(|channel| channel.addresses())
            .try_for_each(|address| {
                if let Entry::Vacant(entry) = previews.0.entry(address) {
                    let preview = read(entry.key())?;
                    entry.insert(preview);
                }
                Ok::<_, Error>(())
            })
            .is_ok()
    };
    let orders = sources.orders.filter(|channel| scan(&[*channel]));
    let questions = sources.questions.filter(|channels| scan(channels));
    let cached = |address: &Address| previews.0.get(address).map(Option::as_ref).ok_or(());
    model.set_order_ends(orders.and_then(|channel| orders::end_times(channel, cached).ok()));
    model.set_questions(questions.and_then(|channels| questions::collect(&channels, cached).ok()));
    previews
}

type Range = (String, String, u64, u64);

pub(super) struct Review<T> {
    viewer: String,
    ranges: Vec<Range>,
    value: Option<T>,
}

impl<T> Review<T> {
    fn new(viewer: &str, ranges: Vec<Range>, value: Option<T>) -> Self {
        Self {
            viewer: viewer.into(),
            ranges,
            value,
        }
    }

    pub fn current(&self, model: &Model, questions: bool) -> Option<&T> {
        let sources = Sources::new(model.channels(), review_viewer(model, &self.viewer));
        let same = if questions {
            sources.questions.as_ref().is_some_and(|channels| {
                channels
                    .iter()
                    .map(|c| c.retained_range())
                    .eq(self.ranges.iter().cloned())
            })
        } else {
            sources
                .orders
                .is_some_and(|channel| self.ranges.as_slice() == [channel.retained_range()])
        };
        same.then_some(self.value.as_ref()).flatten()
    }
}

/// Review only complete ranges from the same previews the activity view loaded.
pub(super) fn update(model: &mut Model) {
    let Some(viewer) = model.viewer.as_deref() else {
        return;
    };
    let sources = Sources::new(model.channels(), review_viewer(model, viewer));
    let orders = if model.order_ends().is_none() {
        sources.orders.and_then(|channel| {
            let ends =
                orders::end_times(channel, |address| model.resolved_preview(address).ok_or(()))
                    .ok()?;
            Some(Review::new(
                viewer,
                vec![channel.retained_range()],
                Some(ends),
            ))
        })
    } else {
        None
    };
    let questions = if model.open_questions().is_none() {
        sources.questions.as_ref().and_then(|readable| {
            let open = questions::collect(readable, |address| {
                model.resolved_preview(address).ok_or(())
            })
            .ok()?;
            Some(Review::new(viewer, sources.question_ranges()?, Some(open)))
        })
    } else {
        None
    };
    if let Some(orders) = orders {
        model.review_orders(orders);
    }
    if let Some(questions) = questions {
        model.review_questions(questions);
    }
}

/// After initial history, a new range can appear before its tail event or
/// outside the selected participant's activity. Fill only missing previews.
pub(super) fn missing(model: &Model) -> Option<Address> {
    let viewer = model.viewer.as_deref()?;
    let sources = Sources::new(model.channels(), review_viewer(model, viewer));
    let orders = sources.orders.filter(|_| model.order_ends().is_none());
    let questions = sources
        .questions
        .filter(|_| model.open_questions().is_none());
    orders
        .into_iter()
        .chain(questions.into_iter().flatten())
        .flat_map(Channel::addresses)
        .find(|address| !model.requested(address))
}

fn review_viewer<'a>(model: &'a Model, id: &str) -> Option<&'a Participant> {
    model.participant(id).filter(|person| {
        !person.revoked
            && person
                .grant
                .ends_at_ms()
                .is_none_or(|end| end > i128::from(super::now_ms()))
    })
}

fn question_counts(model: &Model) -> Option<(usize, usize)> {
    if !model.has_retained(questions::CHANNEL) {
        return Some((0, 0));
    }
    let open = model.open_questions()?;
    let owner = open.iter().filter(|q| q.for_owner()).count();
    Some((open.len(), owner))
}

fn order_count(model: &Model, now: u64) -> Option<usize> {
    if model.has_retained(ORDERS) {
        model.orders_in_force(now)
    } else {
        Some(0)
    }
}

pub(super) fn compact(
    model: &Model,
    now: u64,
    waiting_workers: usize,
    stopped: bool,
) -> [String; ROWS as usize] {
    let pending = if stopped { "unavailable" } else { "loading…" };
    let questions = match question_counts(model) {
        _ if model.question_inbox_viewer().is_some() => inbox_label(),
        Some((count, 0)) => format!("questions {count}"),
        Some((count, owner)) => format!("questions {count} ({owner} you)"),
        None => format!("questions {pending}"),
    };
    let orders = order_count(model, now).map_or_else(
        || format!("orders {pending}"),
        |count| format!("orders {count}"),
    );
    let relay = match model.relay_waiting() {
        0 => "relay –".into(),
        count => format!("relay {count}"),
    };
    [
        format!("lead {}", model.lead_waiting()),
        questions,
        orders,
        format!("inboxes {waiting_workers} waiting"),
        relay,
    ]
}

pub(super) fn lines(model: &Model, now: u64, columns: usize) -> [String; ROWS as usize] {
    let questions = match question_counts(model) {
        _ if model.question_inbox_viewer().is_some() => inbox_label(),
        None => "questions unavailable".into(),
        Some((count, 0)) => format!("questions {count} open"),
        Some((count, owner)) => {
            let full = format!("questions {count} open ({owner} for the owner)");
            if text::width(&full) <= columns {
                full
            } else {
                format!("questions {count} open ({owner} owner)")
            }
        }
    };
    let orders = order_count(model, now).map_or_else(
        || "orders unavailable".into(),
        |count| format!("orders {count} in force"),
    );
    let relay = match model.relay_waiting() {
        0 => "relay none".into(),
        count => format!("relay {count} waiting"),
    };
    [
        format!("lead {} waiting", model.lead_waiting()),
        questions,
        orders,
        inboxes(model, columns),
        relay,
    ]
}

pub(super) fn inbox_label() -> String {
    format!("questions · {}", questions::INBOX_HINT)
}

fn inboxes(model: &Model, columns: usize) -> String {
    let mut count = 0u64;
    let mut names = model
        .waiting_inboxes()
        .map(|(name, waiting, state)| {
            count = count.saturating_add(waiting);
            state.map_or_else(
                || display(name),
                |state| format!("{} {state}", display(name)),
            )
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    let label = format!("inboxes {count} waiting");
    for shown in (0..=names.len().min(3)).rev() {
        if names.is_empty() {
            break;
        }
        let suffix = if shown == 0 {
            "…"
        } else if shown < names.len() {
            ", …"
        } else {
            ""
        };
        let line = format!("{label} ({}{suffix})", names[..shown].join(", "));
        if text::width(&line) <= columns {
            return line;
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::mailbox::model::{
        Preview,
        tests::{event, status},
    };
    use serde_json::json;

    fn preview(sender: &str, reply_to: Option<Address>) -> Preview {
        Preview {
            sender: sender.to_owned(),
            sender_trace: None,
            kind: "note".into(),
            sent_at: [1, 0],
            trace: None,
            text: Some("What is ready?".into()),
            cut_bytes: None,
            has_reply_to: reply_to.is_some(),
            reply_to,
            quote: None,
            body: None,
        }
    }

    #[test]
    fn plain_startup_gets_each_summary_and_history_position_once() {
        let mut snapshot = status();
        snapshot.participants[0].grant =
            serde_json::from_value(json!([null, [["questions", 4], ["orders", 4]]])).unwrap();
        snapshot.channels[0].name = "questions".into();
        snapshot.channels[1].name = "orders".into();
        for channel in &mut snapshot.channels {
            channel.floor = 1;
            channel.head = 3;
        }
        let mut model = Model::new(snapshot.clone(), None);
        let mut gets = Vec::new();
        let mut read = |address: &Address| {
            gets.push(address.clone());
            Ok((address.position == 1).then(|| preview("lead", None)))
        };

        let mut cached = load(&mut model, &snapshot, "lead", &mut read);
        for address in snapshot.channels.iter().flat_map(Channel::addresses) {
            cached.take(&address, &mut read).unwrap();
        }

        assert_eq!(gets.len(), 4, "two channels with two positions each");
    }

    #[test]
    fn question_counts_wait_for_the_cached_answer_channels() {
        let mut snapshot = status();
        snapshot.participants[0].grant =
            serde_json::from_value(json!([null, [["questions", 4], ["to/*", 4]]])).unwrap();
        snapshot.channels[0].name = "questions".into();
        snapshot.channels[0].floor = 1;
        snapshot.channels[0].head = 2;
        snapshot.channels[1].head = 2;
        let question = snapshot.channels[0].address(1);
        let answer = snapshot.channels[1].address(1);
        let mut model = Model::new(snapshot, None);
        model.viewer = Some("lead".into());

        model.backfill(question.clone(), preview("a0123456", None), true);
        update(&mut model);
        assert_eq!(question_counts(&model), None);

        model.backfill(answer, preview("lead", Some(question)), true);
        update(&mut model);
        assert_eq!(question_counts(&model), Some((0, 0)));
    }

    #[test]
    fn order_counts_can_finish_before_the_other_history() {
        let mut snapshot = status();
        snapshot.participants[0].grant =
            serde_json::from_value(json!([null, [["questions", 4], ["orders", 4]]])).unwrap();
        let mut orders = snapshot.channels[0].clone();
        orders.name = "orders".into();
        orders.floor = 1;
        orders.head = 2;
        let order = orders.address(1);
        snapshot.channels[0].name = "questions".into();
        snapshot.channels.push(orders);
        let mut model = Model::new(snapshot, None);
        model.viewer = Some("lead".into());

        model.backfill(order, preview("lead", None), true);
        update(&mut model);

        assert_eq!(order_count(&model, 1_001), Some(1));
        assert_eq!(question_counts(&model), None);
    }

    #[test]
    fn a_late_question_review_cannot_restore_a_stale_count() {
        let mut snapshot = status();
        snapshot.participants[0].grant =
            serde_json::from_value(json!([null, [["questions", 4]]])).unwrap();
        let channel = &mut snapshot.channels[0];
        channel.name = "questions".into();
        let range = channel.retained_range();
        let mut model = Model::new(snapshot, None);
        model.review_questions(Review::new("lead", vec![range.clone()], Some(Vec::new())));
        assert_eq!(question_counts(&model), Some((0, 0)));

        model.apply(event("stored", "lead", "questions", "first", 10));
        model.review_questions(Review::new("lead", vec![range], Some(Vec::new())));

        assert_eq!(question_counts(&model), None);
    }
}
