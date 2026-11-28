//! Fold public owner data and authorized previews into mailbox state.

use super::{activity, questions, sender::Sender, summary::Review};
use crate::cli::output::sender_name_or_id;
use malak::{ChannelName, ChannelPattern, ParticipantId};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;

mod inboxes;

pub(super) use inboxes::Lane;

pub(super) type EventKey = (u64, EventOrder);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum EventOrder {
    Retained(Address),
    Live(u64),
}

#[derive(Clone, Deserialize)]
pub(super) struct Status {
    pub run: String,
    pub started_at_ms: Option<u64>,
    pub participants: Vec<Participant>,
    pub channels: Vec<Channel>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Participant {
    pub id: String,
    pub name: String,
    pub revoked: bool,
    pub enrolled_at_ms: Option<u64>,
    pub grant_changed_at_ms: Option<u64>,
    pub revoked_at_ms: Option<u64>,
    #[serde(skip)]
    pub launcher_state: Option<&'static str>,
    pub claim: Option<ClaimStatus>,
    #[serde(default)]
    pub grant: Grant,
}

#[derive(Clone, Deserialize)]
pub(super) struct ClaimStatus {
    pub state: String,
    pub session: ClaimProcess,
    pub issuer: String,
    pub refused: u64,
    pub last_refused_at_ms: Option<u64>,
    pub history: Vec<ClaimEvent>,
}

#[derive(Clone, Deserialize)]
pub(super) struct ClaimProcess {
    pub pid: Option<u32>,
    pub program: Option<String>,
    #[serde(alias = "cwd")]
    pub folder: Option<String>,
}

#[derive(Clone, Deserialize)]
pub(super) struct ClaimEvent {
    pub event: String,
    pub at_ms: u64,
    #[serde(alias = "peer")]
    pub session: Option<ClaimProcess>,
}

impl Participant {
    pub fn can_get(&self, channel: &str) -> bool {
        self.allows(GrantAction::Get, channel)
    }

    fn allows(&self, action: GrantAction, channel: &str) -> bool {
        !self.revoked
            && self
                .grant
                .ends_at_ms()
                .is_none_or(|end| end > i128::from(super::now_ms()))
            && channel
                .parse::<ChannelName>()
                .is_ok_and(|name| self.grant.covers(action, &name))
    }

    fn inbox(&self) -> String {
        if self.id == "lead" {
            self.id.clone()
        } else {
            format!("to/{}", self.id)
        }
    }
}

pub(super) fn inbox_recipient<'a>(
    participants: &'a [Participant],
    channel: &str,
) -> Option<&'a Participant> {
    let id = channel.strip_prefix("to/")?;
    participants.iter().find(|p| p.id == id && id != "lead")
}

#[derive(Clone, Deserialize)]
pub(super) struct Channel {
    pub name: String,
    pub creation: String,
    pub floor: u64,
    pub head: u64,
    readers: BTreeMap<String, u64>,
}

impl Channel {
    fn first_due(&self, acked: u64) -> u64 {
        self.floor.max(acked.saturating_add(1))
    }

    pub fn waiting(&self, acked: u64) -> u64 {
        self.head.saturating_sub(self.first_due(acked))
    }

    fn acked_by(&self, reader: &str) -> u64 {
        self.readers.get(reader).copied().unwrap_or(0)
    }

    pub fn due_positions(&self, reader: &str) -> Range<u64> {
        self.first_due(self.acked_by(reader))..self.head
    }

    pub fn retained_range(&self) -> (String, String, u64, u64) {
        (
            self.name.clone(),
            self.creation.clone(),
            self.floor,
            self.head,
        )
    }

    pub fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        (self.floor..self.head).map(|position| self.address(position))
    }

    pub fn address(&self, position: u64) -> Address {
        Address {
            channel: self.name.clone(),
            creation: self.creation.clone(),
            position,
        }
    }

    pub fn slowest_ack(&self) -> u64 {
        self.readers.values().copied().min().unwrap_or(0)
    }

    pub fn readers(&self) -> impl Iterator<Item = (&str, u64)> {
        self.readers.iter().map(|(id, &ack)| (id.as_str(), ack))
    }
}

#[derive(Clone, Deserialize)]
pub(super) struct Event {
    pub event: String,
    pub at_ms: Option<u64>,
    pub operation: Option<String>,
    pub id: Option<String>,
    pub sender_trace: Option<Sender>,
    pub target: Option<String>,
    pub channel: Option<String>,
    pub creation: Option<String>,
    pub position: Option<u64>,
    pub kind: Option<String>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub count: Option<u64>,
    pub missed: u64,
    #[serde(skip)]
    historical: bool,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct Address {
    pub channel: String,
    pub creation: String,
    pub position: u64,
}

pub(super) type MessageOrder<'a> = ([u64; 2], &'a str, u64);

impl Address {
    pub fn message_order(&self, sent_at: [u64; 2]) -> MessageOrder<'_> {
        (sent_at, &self.channel, self.position)
    }
}

impl Event {
    pub fn address(&self) -> Option<Address> {
        if self.event == "stored" {
            self.record_address()
        } else {
            None
        }
    }

    pub fn is_stop(&self) -> bool {
        super::owner_commands::is_stop(self.operation.as_deref(), self.outcome.as_deref())
    }

    pub fn record_address(&self) -> Option<Address> {
        Some(Address {
            channel: self.channel.clone()?,
            creation: self.creation.clone()?,
            position: self.position?,
        })
    }

    pub(super) fn historical(address: &Address, preview: &Preview) -> Self {
        Self {
            event: "stored".into(),
            at_ms: Some(preview.at_ms()),
            operation: None,
            id: Some(preview.sender.clone()),
            sender_trace: preview.sender_trace.clone(),
            target: None,
            channel: Some(address.channel.clone()),
            creation: Some(address.creation.clone()),
            position: Some(address.position),
            kind: Some(preview.kind.clone()),
            outcome: Some("done".into()),
            reason: None,
            count: None,
            missed: 0,
            historical: true,
        }
    }
}

pub(super) struct Preview {
    pub sender: String,
    pub sender_trace: Option<Sender>,
    pub kind: String,
    pub sent_at: [u64; 2],
    pub trace: Option<String>,
    pub text: Option<String>,
    pub cut_bytes: Option<usize>,
    /// Whether the stored message carries a reply address, including an invalid target.
    pub has_reply_to: bool,
    pub reply_to: Option<Address>,
    pub quote: Option<Quote>,
    pub body: Option<Body>,
}

pub(super) struct Body {
    pub text: String,
}

impl Preview {
    pub fn at_ms(&self) -> u64 {
        let [seconds, nanos] = self.sent_at;
        seconds
            .saturating_mul(1000)
            .saturating_add(nanos / 1_000_000)
    }

    pub fn body_text(&self) -> Option<&str> {
        self.text.as_ref()?;
        self.body.as_ref().map(|body| body.text.as_str())
    }
}

pub(super) struct Quote {
    pub sender: String,
    pub kind: String,
    pub at_ms: u64,
    pub excerpt: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) enum PreviewState<'a> {
    Pending,
    Unavailable,
    AnswersInInbox,
    Ready(&'a Preview),
}

impl<'a> PreviewState<'a> {
    pub fn sender_trace(self) -> Option<&'a Sender> {
        match self {
            Self::Ready(preview) => preview.sender_trace.as_ref(),
            _ => None,
        }
    }
}

/// An unanswered message on the `questions` channel.
#[derive(Clone, Serialize)]
pub(super) struct Question {
    pub position: u64,
    #[serde(skip)]
    pub creation: String,
    pub sender: String,
    pub at_ms: u64,
    #[serde(skip)]
    pub sent_at: [u64; 2],
    pub first_line: String,
    pub kind: QuestionKind,
    /// Who the question is for, from its `For:` line; none reads as the owner.
    pub audience: Audience,
    /// Whether the body has a `For:` line. An older question has none, and
    /// stays among the agents' questions as it did before audiences.
    #[serde(skip)]
    pub named: bool,
    #[serde(skip)]
    pub body: String,
}

impl Question {
    pub fn order(&self) -> ([u64; 2], u64) {
        (self.sent_at, self.position)
    }

    /// Whether the lead has to take the question to the owner.
    pub fn for_owner(&self) -> bool {
        self.kind == QuestionKind::Owner || (self.named && self.audience != Audience::Lead)
    }
}

/// Who decides a question.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Audience {
    Lead,
    Owner,
    Both,
}

impl Audience {
    /// The audience a question body names on a line `For: lead|owner|both`.
    pub fn of_body(text: &str) -> Option<Self> {
        text.lines()
            .find_map(|line| match line.strip_prefix("For: ")?.trim() {
                "lead" => Some(Self::Lead),
                "owner" => Some(Self::Owner),
                "both" => Some(Self::Both),
                _ => None,
            })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum QuestionKind {
    Owner,
    Agent,
}

pub(super) struct Model {
    pub run: String,
    pub started_at_ms: Option<u64>,
    participants: Vec<Participant>,
    channels: BTreeMap<String, Channel>,
    selection: Selection,
    recent_messages: BTreeMap<String, u64>,
    historical_messages: BTreeMap<String, u64>,
    history: BTreeMap<EventKey, Event>,
    activity: Option<activity::Receipts>,
    refusals: activity::Refusals,
    lead_watch: bool,
    positions: HashMap<Address, EventKey>,
    sequence: u64,
    previews: HashMap<Address, Option<Preview>>,
    failed_previews: HashSet<Address>,
    pending: VecDeque<Address>,
    answers: HashMap<Address, Address>,
    open_questions: Option<Vec<Question>>,
    order_ends: Option<Vec<u64>>,
    reviewed_questions: Option<Review<Vec<Question>>>,
    reviewed_orders: Option<Review<Vec<u64>>>,
    pub viewer: Option<String>,
    pub previews_enabled: bool,
    pub missed: u64,
}

enum Selection {
    All,
    Participant(String),
    Missing,
}

impl Selection {
    fn from_name(name: Option<&str>, participants: &[Participant]) -> Self {
        let Some(name) = name else { return Self::All };
        let mut matches = participants.iter().filter(|p| p.name == name);
        if let Some(active) = matches.clone().find(|p| !p.revoked) {
            return Self::Participant(active.id.clone());
        }
        match (matches.next(), matches.next()) {
            (Some(one), None) => Self::Participant(one.id.clone()),
            _ => Self::Missing,
        }
    }

    fn includes(&self, person: &Participant) -> bool {
        match self {
            Self::All => true,
            Self::Participant(id) => person.id == *id,
            Self::Missing => false,
        }
    }
}

#[derive(Clone, Default, Deserialize)]
pub(super) struct Grant(Option<(bool, u64, u32)>, Vec<(String, u8)>);

#[derive(Clone, Copy)]
pub(super) enum GrantAction {
    Send = 1,
    Get = 4,
}

impl Grant {
    pub fn ends_at_ms(&self) -> Option<i128> {
        self.0.map(|(before_epoch, seconds, nanos)| {
            let millis = i128::from(seconds) * 1000 + i128::from(nanos / 1_000_000);
            if before_epoch { -millis } else { millis }
        })
    }

    pub fn covers(&self, action: GrantAction, channel: &ChannelName) -> bool {
        self.1.iter().any(|(pattern, mask)| {
            mask & action as u8 != 0
                && pattern
                    .parse::<ChannelPattern>()
                    .is_ok_and(|rule| rule.matches(channel))
        })
    }
}

impl Model {
    pub fn new(status: Status, selected: Option<String>) -> Self {
        let mut model = Self {
            run: status.run.clone(),
            started_at_ms: status.started_at_ms,
            participants: Vec::new(),
            channels: BTreeMap::new(),
            selection: Selection::from_name(selected.as_deref(), &status.participants),
            recent_messages: BTreeMap::new(),
            historical_messages: BTreeMap::new(),
            history: BTreeMap::new(),
            activity: None,
            refusals: activity::Refusals::default(),
            lead_watch: false,
            positions: HashMap::new(),
            sequence: 0,
            previews: HashMap::new(),
            failed_previews: HashSet::new(),
            pending: VecDeque::new(),
            answers: HashMap::new(),
            open_questions: None,
            order_ends: None,
            reviewed_questions: None,
            reviewed_orders: None,
            viewer: None,
            previews_enabled: false,
            missed: 0,
        };
        model.update(status);
        model
    }

    pub fn update(&mut self, status: Status) {
        if let Some(activity) = &mut self.activity {
            activity.refresh(status.channels.iter());
        }
        self.participants = status.participants;
        self.channels = status
            .channels
            .into_iter()
            .map(|mut channel| {
                if let Some(current) = self.channels.get(&channel.name)
                    && current.creation == channel.creation
                {
                    channel.head = channel.head.max(current.head);
                    channel.floor = channel.floor.max(current.floor);
                    for (id, &at) in &current.readers {
                        if self
                            .participants
                            .iter()
                            .any(|p| p.id == *id && (!p.revoked || self.lead_watch))
                        {
                            let acked = channel.readers.entry(id.clone()).or_default();
                            *acked = (*acked).max(at);
                        }
                    }
                }
                (channel.name.clone(), channel)
            })
            .collect();
    }

    pub fn has_selection(&self) -> bool {
        !matches!(self.selection, Selection::Missing)
    }

    pub fn selected_id(&self) -> Option<&str> {
        match &self.selection {
            Selection::Participant(id) => Some(id),
            _ => None,
        }
    }

    pub fn take_selection(&mut self) -> Option<String> {
        match std::mem::replace(&mut self.selection, Selection::All) {
            Selection::Participant(id) => Some(id),
            _ => None,
        }
    }

    pub fn people(&self) -> Vec<&Participant> {
        let mut participants = self
            .participants
            .iter()
            .filter(|p| self.selection.includes(p))
            .collect::<Vec<_>>();
        participants.sort_by_key(|p| (p.id != "lead", p.name.as_str(), p.id.as_str()));
        participants
    }

    pub fn enable_lead_watch(&mut self) {
        self.lead_watch = true;
    }

    pub fn last_message(&self, id: &str) -> Option<u64> {
        self.recent_messages
            .get(id)
            .or_else(|| self.historical_messages.get(id))
            .copied()
    }

    pub fn open_questions(&self) -> Option<&[Question]> {
        match &self.reviewed_questions {
            Some(review) => review.current(self, true).map(Vec::as_slice),
            None => self.open_questions.as_deref(),
        }
    }

    pub fn set_questions(&mut self, questions: Option<Vec<Question>>) {
        self.open_questions = questions;
    }

    pub fn set_order_ends(&mut self, ends: Option<Vec<u64>>) {
        self.order_ends = ends;
    }

    pub fn review_questions(&mut self, review: Review<Vec<Question>>) {
        self.reviewed_questions = Some(review);
    }

    pub fn review_orders(&mut self, review: Review<Vec<u64>>) {
        self.reviewed_orders = Some(review);
    }

    pub fn order_ends(&self) -> Option<&[u64]> {
        match &self.reviewed_orders {
            Some(review) => review.current(self, false),
            None => self.order_ends.as_ref(),
        }
        .map(Vec::as_slice)
    }

    pub fn orders_in_force(&self, now: u64) -> Option<usize> {
        Some(self.order_ends()?.iter().filter(|&&end| now < end).count())
    }

    pub fn participant(&self, id: &str) -> Option<&Participant> {
        self.participants.iter().find(|p| p.id == id)
    }

    pub fn question_inbox_viewer(&self) -> Option<&str> {
        let viewer = self.participant(self.viewer.as_deref()?)?;
        (viewer.allows(GrantAction::Send, questions::CHANNEL)
            && !viewer.can_get(questions::CHANNEL))
        .then_some(viewer.id.as_str())
    }

    pub fn worker_inbox(&self, name: &str) -> Option<(&Participant, &Channel)> {
        Some((
            inbox_recipient(&self.participants, name)?,
            self.channels.get(name)?,
        ))
    }

    pub fn selected_worker_inboxes(&self) -> impl Iterator<Item = (&Participant, &Channel)> {
        self.channels
            .keys()
            .filter_map(|name| self.worker_inbox(name))
            .filter(|(person, _)| !person.revoked && self.selection.includes(person))
    }

    pub fn channels(&self) -> impl Iterator<Item = &Channel> + Clone {
        self.channels.values()
    }

    pub fn has_retained(&self, name: &str) -> bool {
        self.channels
            .get(name)
            .is_some_and(|channel| channel.floor < channel.head)
    }

    pub fn lead_waiting(&self) -> u64 {
        self.channels
            .get("lead")
            .map_or(0, |channel| channel.waiting(channel.acked_by("lead")))
    }

    pub fn relay_waiting(&self) -> u64 {
        self.channels
            .values()
            .filter(|channel| channel.name.starts_with("relay/"))
            .map(|channel| channel.waiting(channel.acked_by("lead")))
            .fold(0, u64::saturating_add)
    }

    pub fn waiting_inboxes(&self) -> impl Iterator<Item = (&str, u64, Option<&'static str>)> {
        self.channels.values().filter_map(|channel| {
            let id = channel.name.strip_prefix("to/")?;
            let recipient = inbox_recipient(&self.participants, &channel.name);
            if recipient.is_some_and(|person| person.revoked) {
                return None;
            }
            let (name, acked) = recipient.map_or_else(
                || (channel.name.as_str(), channel.slowest_ack()),
                |person| (person.name.as_str(), channel.acked_by(id)),
            );
            let waiting = channel.waiting(acked);
            (waiting > 0).then_some((
                name,
                waiting,
                recipient.map_or(Some("stray"), |p| p.launcher_state),
            ))
        })
    }

    pub fn stray_channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.values().filter(|channel| {
            channel.name.starts_with("to/") && self.inbox_recipient(&channel.name).is_none()
        })
    }

    pub fn set_history(&mut self, messages: BTreeMap<String, u64>) {
        self.historical_messages = messages;
    }

    pub fn recipient<'a>(&'a self, channel: &'a str) -> &'a str {
        inbox_recipient(&self.participants, channel).map_or(channel, |person| person.name.as_str())
    }

    pub fn inbox_recipient(&self, channel: &str) -> Option<&Participant> {
        inbox_recipient(&self.participants, channel)
    }

    pub fn message_recipient(&self, channel: &str) -> Option<&Participant> {
        if channel == "lead" {
            self.participant("lead")
        } else {
            self.inbox_recipient(channel)
        }
    }

    pub fn message_involves(&self, address: &Address, sender: &str, id: &str) -> bool {
        sender == id
            || self
                .message_recipient(&address.channel)
                .is_some_and(|p| p.id == id)
    }

    pub fn name<'a>(&'a self, id: &'a str) -> &'a str {
        self.participant(id)
            .map_or_else(|| sender_name_or_id(id), |person| person.name.as_str())
    }

    pub fn plain_name<'a>(&'a self, id: &'a str) -> Cow<'a, str> {
        self.participant(id).map_or_else(
            || Cow::Borrowed(sender_name_or_id(id)),
            |person| self.plain_label(person),
        )
    }

    pub fn plain_recipient<'a>(&'a self, channel: &'a str) -> Cow<'a, str> {
        if channel.starts_with("to/") && self.inbox_recipient(channel).is_none() {
            return Cow::Owned(format!("stray {channel}"));
        }
        self.inbox_recipient(channel)
            .map_or(Cow::Borrowed(channel), |person| self.plain_label(person))
    }

    /// The name, numbered like the live view only when another live
    /// enrolment shares it. A revoked enrolment never causes a clash.
    fn plain_label<'a>(&'a self, person: &'a Participant) -> Cow<'a, str> {
        let clash = self
            .participants
            .iter()
            .any(|other| other.name == person.name && other.id != person.id && !other.revoked);
        match self.name_number(person).filter(|_| clash) {
            Some(number) => Cow::Owned(format!("{}#{number}", person.name)),
            None => Cow::Borrowed(&person.name),
        }
    }

    pub fn missing_event_participant(&self, event: &Event) -> bool {
        let recipient = event
            .channel
            .as_deref()
            .and_then(|channel| channel.strip_prefix("to/"));
        event
            .id
            .as_deref()
            .into_iter()
            .chain(recipient)
            .any(|id| self.participant(id).is_none() && id.parse::<ParticipantId>().is_ok())
    }

    pub fn name_number(&self, person: &Participant) -> Option<usize> {
        let mut count = 0;
        let mut earlier = 0;
        for other in self
            .participants
            .iter()
            .filter(|other| other.name == person.name)
        {
            count += 1;
            if (other.enrolled_at_ms, &other.id) < (person.enrolled_at_ms, &person.id) {
                earlier += 1;
            }
        }
        (count > 1).then_some(earlier + 1)
    }

    pub fn observe(&mut self, event: &Event) {
        self.missed = self.missed.saturating_add(event.missed);
        if event.event == "stored"
            && let Some(id) = &event.id
            && let Some(at) = event.at_ms
        {
            self.recent_messages.insert(id.clone(), at);
        }
    }

    pub fn apply(&mut self, event: Event) -> bool {
        self.observe(&event);
        if let Some(activity) = &mut self.activity {
            activity.observe(&event);
        }
        self.apply_cursors(&event);
        self.record(event, false)
    }

    pub fn apply_cursors(&mut self, event: &Event) {
        if let Some(name) = &event.channel
            && let Some(creation) = &event.creation
            && let Some(position) = event.position
            && let Some(channel) = self.channels.get_mut(name)
            && channel.creation == *creation
        {
            match event.event.as_str() {
                "stored" => channel.head = channel.head.max(position.saturating_add(1)),
                "acked" => {
                    if let Some(id) = &event.id
                        && self
                            .participants
                            .iter()
                            .any(|person| person.id == *id && (!person.revoked || self.lead_watch))
                    {
                        let acked = channel.readers.entry(id.clone()).or_default();
                        *acked = (*acked).max(position);
                    }
                }
                "trimmed" => channel.floor = channel.floor.max(position),
                _ => {}
            }
        }
    }

    fn record(&mut self, mut event: Event, ordered_backfill: bool) -> bool {
        let address = event.address();
        let order = if ordered_backfill {
            EventOrder::Retained(address.as_ref().expect("retained message address").clone())
        } else {
            self.sequence += 1;
            EventOrder::Live(self.sequence)
        };
        let key = (event.at_ms.unwrap_or_default(), order);
        let fetch_pending = !event.historical
            && self.previews_enabled
            && activity::shows(&event)
            && self.visible(&event);
        let mut is_new = true;
        if let Some(address) = &address {
            if let Some(previous) = self.positions.get(address) {
                if event.historical {
                    return false;
                }
                self.history.remove(previous);
                is_new = false;
            }
            self.positions.insert(address.clone(), key.clone());
        }
        if self.activity.is_some()
            && let Some(previous) = self.refusals.fold(&key, &mut event)
        {
            self.history.remove(&previous);
        }
        self.history.insert(key, event);
        if let Some(address) = address {
            self.remember_answer(&address);
            if fetch_pending && !self.requested(&address) {
                self.pending.push_back(address);
            }
        }
        is_new
    }

    pub fn backfill(&mut self, address: Address, preview: Preview, ordered: bool) {
        self.record(Event::historical(&address, &preview), ordered);
        self.fetch_result(address, Some(preview));
    }

    pub fn stored(&self, address: &Address) -> Option<&Event> {
        self.positions
            .get(address)
            .and_then(|key| self.history.get(key))
    }

    pub fn visible(&self, event: &Event) -> bool {
        // Interactive tail events can arrive before the preview viewer is known.
        if event.channel.as_deref() == Some(questions::CHANNEL)
            && matches!(event.event.as_str(), "stored" | "read" | "acked")
            && ((self.previews_enabled && self.viewer.is_none())
                || self
                    .question_inbox_viewer()
                    .is_some_and(|viewer| event.id.as_deref() != Some(viewer)))
        {
            return false;
        }
        if self.lead_watch {
            let channel = event.channel.as_deref();
            if event.id.as_deref() == Some("lead") {
                match event.event.as_str() {
                    "read" | "acked" => return false,
                    "stored" if channel == Some("lead") => return false,
                    _ => (),
                }
            }
            if event.event == "stored" && matches!(channel, Some("questions" | "orders")) {
                return true;
            }
        }
        match &self.selection {
            Selection::All => true,
            Selection::Missing => false,
            Selection::Participant(id) => self
                .participants
                .iter()
                .any(|person| person.id == *id && involves(event, person, &person.inbox())),
        }
    }

    pub fn feed(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.history
            .values()
            .rev()
            .filter(|event| self.visible(event))
    }

    pub fn activity(&self) -> impl DoubleEndedIterator<Item = (&EventKey, &Event)> {
        self.history
            .iter()
            .rev()
            .filter(|(_, event)| activity::shows(event) && (event.is_stop() || self.visible(event)))
    }

    pub fn enable_activity(&mut self) {
        self.activity = Some(activity::Receipts::new(self.channels.values()));
    }

    pub fn delivery(&self, address: &Address) -> activity::Delivery {
        if self
            .inbox_recipient(&address.channel)
            .is_some_and(|p| p.revoked)
        {
            return activity::Delivery::Revoked;
        }
        self.activity
            .as_ref()
            .map_or(activity::Delivery::Waiting, |activity| {
                activity.delivery(address)
            })
    }

    pub fn requested(&self, address: &Address) -> bool {
        self.previews.contains_key(address) || self.failed_previews.contains(address)
    }

    pub fn failed_preview(&mut self, address: Address) {
        self.failed_previews.insert(address);
    }

    // An absent record (for example a void position) still resolves coverage.
    // A request that failed or has not run does not.
    pub fn resolved_preview(&self, address: &Address) -> Option<Option<&Preview>> {
        self.previews.get(address).map(Option::as_ref)
    }

    pub fn next_preview(&mut self) -> Option<Address> {
        while let Some(address) = self.pending.pop_front() {
            if !self.requested(&address) {
                return Some(address);
            }
        }
        None
    }

    pub fn fetch_result(&mut self, address: Address, preview: Option<Preview>) {
        self.failed_previews.remove(&address);
        if preview.is_some() || !self.previews.contains_key(&address) {
            self.previews.insert(address.clone(), preview);
            self.remember_answer(&address);
        }
    }

    fn remember_answer(&mut self, reply: &Address) {
        if let Some(target) = self
            .fetched(reply)
            .and_then(|preview| self.answered_target(reply, preview))
        {
            self.answers.insert(target.clone(), reply.clone());
        }
    }

    pub fn reply_reference<'a>(&self, preview: &'a Preview) -> Option<&'a Address> {
        preview.reply_to.as_ref().filter(|target| {
            self.channels
                .get(&target.channel)
                .is_none_or(|channel| channel.creation == target.creation)
        })
    }

    pub fn answered_target<'a>(
        &self,
        reply: &Address,
        preview: &'a Preview,
    ) -> Option<&'a Address> {
        let target = self
            .reply_reference(preview)
            .filter(|target| target.channel == "lead")?;
        let reply_order = &self.positions.get(reply)?.1;
        if self.stored(reply)?.historical {
            return None;
        }
        if let Some(event) = self.stored(target) {
            let earlier = event.historical || &self.positions.get(target)?.1 < reply_order;
            return (earlier && matches!(event.kind.as_deref(), Some("note" | "report")))
                .then_some(target);
        }
        preview
            .quote
            .as_ref()
            .is_some_and(|quote| matches!(quote.kind.as_str(), "note" | "report"))
            .then_some(target)
    }

    pub fn answered_by(&self, address: &Address) -> Option<&Address> {
        self.answers.get(address)
    }

    pub fn preview(&self, address: &Address) -> PreviewState<'_> {
        if address.channel == questions::CHANNEL && self.question_inbox_viewer().is_some() {
            return PreviewState::AnswersInInbox;
        }
        match self.previews.get(address) {
            Some(Some(preview)) => PreviewState::Ready(preview),
            Some(None) => PreviewState::Unavailable,
            None if self.previews_enabled && !self.failed_previews.contains(address) => {
                PreviewState::Pending
            }
            None => PreviewState::Unavailable,
        }
    }

    fn fetched(&self, address: &Address) -> Option<&Preview> {
        if let PreviewState::Ready(preview) = self.preview(address) {
            Some(preview)
        } else {
            None
        }
    }
}

fn involves(event: &Event, person: &Participant, inbox: &str) -> bool {
    event.id.as_deref() == Some(person.id.as_str())
        || event.channel.as_deref() == Some(inbox)
        || event.target.as_deref() == Some(person.name.as_str())
}

#[cfg(test)]
pub(super) mod tests;
