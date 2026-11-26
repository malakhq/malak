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
use std::{path::PathBuf, sync::Mutex};

pub(super) struct Seat {
    session: Mutex<Session>,
    /// The participant's inbox, where `inbox` reads.
    inbox: String,
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
            lead: id.is_lead(),
            board: Config::read(config.clone())?.board,
            config,
            handover: std::env::var("MALAK_HANDOVER").ok(),
        })
    }

    pub(super) fn is_lead(&self) -> bool {
        self.lead
    }

    /// Text that names no handover file, so no tool output points at one.
    pub(super) fn scrub(&self, text: &str) -> String {
        match &self.handover {
            Some(path) if !path.is_empty() => text.replace(path.as_str(), "MALAK_HANDOVER"),
            _ => text.to_owned(),
        }
    }

    pub(super) fn call(&self, tool: &str, input: Value) -> Result<Reply, Error> {
        match tool {
            "inbox" => self.inbox(arguments(input)?),
            "ack" => self.ack(arguments(input)?),
            "get" => self.get(arguments(input)?),
            "report" => self.report(arguments(input)?),
            "ask_owner" => recipes::ask_owner(self, arguments(input)?),
            "delegate" => self.delegate(arguments(input)?),
            "start_agent" => recipes::start_agent(self, arguments(input)?),
            "agents" => recipes::agents(self),
            _ => Err(Error::refused("unknown tool")),
        }
    }

    fn run(&self, command: Command, body: Option<Vec<u8>>) -> Result<(String, Value), Error> {
        let mut session = self.session.lock().unwrap_or_else(|held| held.into_inner());
        command.execute_reply(&mut session, body)
    }

    fn inbox(&self, args: InboxArgs) -> Result<Reply, Error> {
        // A lead that waits on its inbox counts as watching, as a `mailbox --next` tick does.
        let _tick = (self.lead && args.wait > 0).then(|| TickMark::start(&self.board));
        let (mut text, mut value) = self.run(
            Command::Read {
                channels: vec![self.inbox.clone()],
                limit: args.limit,
                wait: args.wait,
                json: true,
                ack: false,
                agent: true,
                offset: None,
            },
            None,
        )?;
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
        let (text, value) = self.run(
            Command::Get {
                channel: args.channel,
                position: args.position,
                json: true,
                agent: true,
                offset: args.offset,
            },
            None,
        )?;
        Ok(Reply { text, value })
    }

    fn report(&self, args: ReportArgs) -> Result<Reply, Error> {
        self.send("lead", Kind::Report, args.id, args.body)
    }

    fn delegate(&self, args: DelegateArgs) -> Result<Reply, Error> {
        let inbox = recipes::inbox_of(self, &args.agent)?;
        self.send(&inbox, Kind::Task, None, args.task)
    }

    fn send(
        &self,
        channel: &str,
        kind: Kind,
        id: Option<String>,
        body: String,
    ) -> Result<Reply, Error> {
        if body.len() > Limits::default().body_bytes {
            return Err(Error::refused("message body exceeds the byte limit"));
        }
        let (text, value) = self.run(
            Command::Send {
                channel: channel.to_owned(),
                id,
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
}
