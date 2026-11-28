//! A Seat: the participant's one board connection and the tools that use it.

use super::{Reply, arguments, recipes};
use crate::cli::{
    config::Config,
    mailbox::TickMark,
    participant::{self, Command, Session},
};
use malak::{CreationId, Error, Kind, Limits};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    mem,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const QUESTIONS: &str = "questions";
/// How often `inbox` looks again while it waits and only repeats are unread.
const POLL: Duration = Duration::from_millis(500);

/// What this server process has already returned from `inbox`.
#[derive(Default)]
struct Returned {
    /// Channel, creation and position of each message returned.
    messages: HashSet<(String, String, u64)>,
    /// The text of each ended-session notice returned.
    notices: HashSet<String>,
}

/// `text` with the handover's path replaced, so no output points at the file.
pub(super) fn scrub_handover(text: &str, handover: Option<&str>) -> String {
    match handover {
        Some(path) if !path.is_empty() => text.replace(path, "MALAK_HANDOVER"),
        _ => text.to_owned(),
    }
}

pub(super) struct Seat {
    session: Mutex<Session>,
    /// The participant's inbox, where `inbox` reads.
    inbox: String,
    returned: Mutex<Returned>,
    lead: bool,
    pub(super) config: Option<PathBuf>,
    board: crate::cli::config::Board,
    handover: Option<String>,
}

impl Seat {
    /// Attach with the handover `MALAK_HANDOVER` or the board names, as the CLI does.
    pub(super) fn open(config: Option<PathBuf>) -> Result<Self, Error> {
        let id = participant::preview_handover(config.clone())?.id().clone();
        let session = Session::open(config.clone())?;
        Ok(Self {
            session: Mutex::new(session),
            inbox: id.inbox().to_string(),
            returned: Mutex::default(),
            lead: id.is_lead(),
            board: Config::read(config.clone())?.board,
            config,
            handover: std::env::var("MALAK_HANDOVER").ok(),
        })
    }

    /// Attach again with the handover as the file is now, keeping what this
    /// server already returned when the participant is the same.
    pub(super) fn reattach(&self) -> Result<Self, Error> {
        let fresh = Self::open(self.config.clone())?;
        if fresh.inbox == self.inbox {
            let mut held = self
                .returned
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            *fresh
                .returned
                .lock()
                .unwrap_or_else(|held| held.into_inner()) = mem::take(&mut held);
        }
        Ok(fresh)
    }

    pub(super) fn is_lead(&self) -> bool {
        self.lead
    }

    /// Text that names no handover file, so no tool output points at one.
    pub(super) fn scrub(&self, text: &str) -> String {
        scrub_handover(text, self.handover.as_deref())
    }

    /// Run one tool. `send_id` is the message ID a tool that sends uses unless
    /// the caller gave one, so a retry of the same call stores its message once.
    pub(super) fn call(&self, tool: &str, input: Value, send_id: &str) -> Result<Reply, Error> {
        let notice = self
            .lead
            .then(|| crate::cli::org::lifecycle::on_command(self.config.clone()))
            .flatten();
        let result = match tool {
            "briefs" | "brief" | "brief_view" | "brief_put" => {
                super::briefs::call(self.config.clone(), tool, input, send_id)
            }
            "inbox" => self.inbox(arguments(input)?),
            "ack" => self.ack(arguments(input)?),
            "get" => self.get(arguments(input)?),
            "report" => self.report(arguments(input)?, send_id),
            "ask" => recipes::ask(self, arguments(input)?, false),
            "ask_owner" => recipes::ask(self, arguments(input)?, true),
            "answer" if self.lead => recipes::answer(self, arguments(input)?),
            "delegate" => self.delegate(arguments(input)?, send_id),
            "start_agent" => recipes::start_agent(self, arguments(input)?),
            "agents" => recipes::agents(self),
            "rail" => super::org::rail(self, arguments(input)?),
            "pulse" => super::org::pulse(self, arguments(input)?),
            "rail_wait" => super::org::wait(self, arguments(input)?),
            "rail_handover" => super::org::handover(self, arguments(input)?),
            _ => Err(Error::refused("unknown tool")),
        };
        result.map(|mut reply| {
            if let Some(notice) = notice {
                reply.text = format!("{notice}\n{}", reply.text);
                if reply.value.get("items").is_some() {
                    let next = reply.value["next"].as_str().unwrap_or("");
                    reply.value["next"] = json!(format!("{notice}\n{next}"));
                }
            }
            reply
        })
    }

    fn run(&self, command: Command, body: Option<Vec<u8>>) -> Result<(String, Value), Error> {
        let mut session = self.session.lock().unwrap_or_else(|held| held.into_inner());
        command.execute_reply(&mut session, body)
    }

    /// The channels `inbox` reads. The lead's owner answers arrive on
    /// `questions`; a worker's arrive in its own inbox.
    fn inbox_channels(&self) -> Vec<String> {
        let mut channels = vec![self.inbox.clone()];
        if self.lead {
            channels.push(QUESTIONS.to_owned());
        }
        channels
    }

    /// One read of the inbox: the messages not yet returned by this process,
    /// the notes and answers that count as new, and how many repeats it left out.
    fn read_new(&self, args: &InboxArgs, wait: u64) -> Result<(String, Value, usize), Error> {
        let returned = self
            .returned
            .lock()
            .unwrap_or_else(|held| held.into_inner());
        let mut taken = 0;
        let mut batch_notices = HashSet::new();
        let mut repeats = 0;
        let command = Command::Read {
            channels: self.inbox_channels(),
            // The limit counts what is returned, not repeats it skips.
            limit: args.all.then_some(args.limit).flatten(),
            wait,
            json: true,
            ack: false,
            agent: true,
            offset: None,
        };
        let mut session = self.session.lock().unwrap_or_else(|held| held.into_inner());
        let (mut text, mut value, _) = command.execute_reply_keeping(&mut session, |message| {
            message.mark_owner_answer();
            let (channel, creation, position) = message.address();
            if channel == QUESTIONS
                && !message.answers_a_question()
                && !message.is_worker_question()
            {
                return false;
            }
            if !args.all {
                let key = (channel.to_owned(), creation.to_owned(), position);
                let notice = message.ended_session_notice();
                if returned.messages.contains(&key)
                    || notice.is_some_and(|notice| {
                        returned.notices.contains(notice)
                            || !batch_notices.insert(notice.to_owned())
                    })
                {
                    repeats += 1;
                    return false;
                }
                if args.limit.is_some_and(|limit| taken >= limit) {
                    return false;
                }
                taken += 1;
            }
            true
        })?;
        if repeats > 0 {
            text.push_str(&format!(
                "\n{repeats} earlier message(s) already returned by this server are not repeated; \
                 inbox with all: true returns them again.\n"
            ));
            value["already_returned"] = json!(repeats);
        }
        Ok((text, value, repeats))
    }

    /// Remember what a reply showed in full, so the next `inbox` does not repeat it.
    fn remember(&self, value: &Value) {
        let mut returned = self
            .returned
            .lock()
            .unwrap_or_else(|held| held.into_inner());
        for message in value["messages"].as_array().map_or(&[][..], Vec::as_slice) {
            let (Some(channel), Some(creation), Some(position)) = (
                message["channel"].as_str(),
                message["creation"].as_str(),
                message["position"].as_u64(),
            ) else {
                continue;
            };
            if message.get("fetch").is_none() {
                returned
                    .messages
                    .insert((channel.to_owned(), creation.to_owned(), position));
            }
            if message.get("reissue").is_some()
                && let Some(body) = message["body"].as_str()
            {
                returned.notices.insert(body.to_owned());
            }
        }
    }

    fn inbox(&self, args: InboxArgs) -> Result<Reply, Error> {
        // A lead that waits on its inbox counts as watching, as a `mailbox --next` tick does.
        let _tick = (self.lead && args.wait > 0).then(|| TickMark::start(&self.board));
        let deadline = Instant::now() + Duration::from_secs(args.wait);
        let mut wait = args.wait;
        let (mut text, mut value) = loop {
            let (text, value, repeats) = self.read_new(&args, wait)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let empty = value["messages"].as_array().is_none_or(Vec::is_empty);
            // Messages already returned end a read's wait at once; look again until the deadline.
            if !empty || repeats == 0 || remaining.is_zero() {
                break (text, value);
            }
            std::thread::sleep(POLL.min(remaining));
            wait = deadline.saturating_duration_since(Instant::now()).as_secs();
        };
        self.remember(&value);
        if args.ack {
            let through = acknowledgeable(&value);
            let mut session = self.session.lock().unwrap_or_else(|held| held.into_inner());
            match session.ack_printed(through.clone(), false) {
                Ok(()) => {
                    for (channel, _, position) in &through {
                        text.push_str(&format!("Acknowledged {channel} through #{position}.\n"));
                    }
                    value["acknowledged"] = through
                        .into_iter()
                        .map(|(channel, creation, position)| {
                            json!({ "channel": channel, "creation": creation, "position": position })
                        })
                        .collect();
                }
                Err(error) => {
                    text.push_str(&format!("Not acknowledged: {error}\n"));
                    value["ack_error"] = Value::String(error.to_string());
                }
            }
        }
        Ok(Reply { text, value })
    }

    fn ack(&self, args: AckArgs) -> Result<Reply, Error> {
        let creation: CreationId = args.creation.parse()?;
        let (text, value) = self.run(
            Command::Ack {
                channel: args.channel,
                position: args.position,
                creation: Some(creation),
            },
            None,
        )?;
        Ok(Reply { text, value })
    }

    fn get(&self, args: GetArgs) -> Result<Reply, Error> {
        let command = Command::Get {
            channel: args.channel,
            position: args.position,
            creation: args.creation.map(|creation| creation.parse()).transpose()?,
            json: true,
            agent: true,
            offset: args.offset,
        };
        let (text, value) = if let Some(board) = args.board {
            if !self.lead {
                return Err(Error::refused(
                    "subs cannot select an organisation reader seat",
                ));
            }
            let reader = crate::cli::org::seats::Reader::open(self.config.clone())?;
            let mut source = reader.open_board(Some(&board))?;
            if source.config == reader.home {
                self.run(command, None)?
            } else {
                command.foreign_reply(&mut source.session, &source.board)?
            }
        } else {
            self.run(command, None)?
        };
        Ok(Reply { text, value })
    }

    fn report(&self, args: ReportArgs, send_id: &str) -> Result<Reply, Error> {
        let id = args.id.unwrap_or_else(|| send_id.to_owned());
        self.send("lead", Kind::Report, id, args.body)
    }

    fn delegate(&self, args: DelegateArgs, send_id: &str) -> Result<Reply, Error> {
        let inbox = recipes::inbox_of(self, &args.agent)?;
        let id = args.id.unwrap_or_else(|| send_id.to_owned());
        self.send(&inbox, Kind::Task, id, args.task)
    }

    fn send(&self, channel: &str, kind: Kind, id: String, body: String) -> Result<Reply, Error> {
        if body.len() > Limits::default().body_bytes {
            return Err(Error::refused("message body exceeds the byte limit"));
        }
        let (text, value) = self.run(
            Command::Send {
                channel: channel.to_owned(),
                id: Some(id),
                kind,
                reply_to: Vec::new(),
                content_type: None,
                max_bytes: None,
                trace: None,
                json: true,
                body: None,
            },
            Some(body.into_bytes()),
        )?;
        Ok(Reply { text, value })
    }
}

/// The highest position to acknowledge in each channel creation: only messages
/// the reply shows in full, and none from the first task onward, because a task
/// is acknowledged after its work.
fn acknowledgeable(value: &Value) -> Vec<(String, String, u64)> {
    let mut through: Vec<(String, String, u64)> = Vec::new();
    let messages = value["messages"].as_array().map_or(&[][..], Vec::as_slice);
    for message in messages {
        if message["presentation"] == "task" || message.get("fetch").is_some() {
            break;
        }
        let (Some(channel), Some(creation), Some(position)) = (
            message["channel"].as_str(),
            message["creation"].as_str(),
            message["position"].as_u64(),
        ) else {
            break;
        };
        match through
            .iter_mut()
            .find(|(c, k, _)| c == channel && k == creation)
        {
            Some(entry) => entry.2 = entry.2.max(position),
            None => through.push((channel.to_owned(), creation.to_owned(), position)),
        }
    }
    through
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxArgs {
    #[serde(default)]
    wait: u64,
    limit: Option<u64>,
    #[serde(default)]
    ack: bool,
    /// Also return messages this server already returned.
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckArgs {
    channel: String,
    position: u64,
    creation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    board: Option<String>,
    creation: Option<String>,
    channel: String,
    position: u64,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    body: String,
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    agent: String,
    task: String,
    id: Option<String>,
}
