use super::{
    client_transport::{ClientTransport, link_failed, link_io_error, reconnect},
    config::{Config, START_HINT, io_error},
    examples,
    guest_inbox::{self, GuestInbox},
    mailbox::REFRESH,
    output, owner_commands, owner_for_board_quiet, presentation,
};
use clap::Subcommand;
use malak::{ChannelName, CreationId, Error, Kind, Limits, Outcome, ParticipantId};
use malak_core::{HandoverFile, LocalClient};
use malak_fs::Directory;
use malak_layers::Mechanism;
use malak_mechanism_key::Key;
use malak_proto::{Entrance, encode_frame};
use minicbor::data::Type;
use minicbor::{Decoder, Encoder};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod reply;

use reply::{Output, agent_text};
pub(super) use reply::{json_message, message_block};

const CALL_WAIT: Duration = Duration::from_secs(60);
pub(super) const REPORT_CONNECTION_EXIT: u8 = 75;
pub(super) const EMPTY_BODY: &str = "message body is empty";
/// The board's refusal of an `ack` for a position no `read` has given the reader.
pub(super) const NOT_GIVEN: &str = "position was not given to this reader";

pub(super) fn report_connection_failed(error: &Error) -> bool {
    link_failed(error)
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Send opaque message bytes to a channel.
    #[command(after_help = examples::help(&["send"]))]
    Send {
        channel: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value = "note")]
        kind: Kind,
        #[arg(long, num_args = 3, value_names = ["CHANNEL", "CREATION", "POSITION"])]
        reply_to: Vec<String>,
        #[arg(long = "type")]
        content_type: Option<String>,
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Hexadecimal trace ID, or empty for an exact legacy retry.
        #[arg(long)]
        trace: Option<String>,
        /// Print the send receipt as JSON.
        #[arg(long)]
        json: bool,
        /// Message text, or - to read bytes from standard input.
        body: Option<String>,
    },
    /// Read messages; acknowledge them only with --ack.
    #[command(after_help = examples::help(&["read"]))]
    Read {
        #[arg(required = true, value_name = "CHANNEL")]
        channels: Vec<String>,
        #[arg(long)]
        limit: Option<u64>,
        #[arg(long, default_value_t = 0)]
        wait: u64,
        #[arg(long)]
        json: bool,
        /// Acknowledge exactly the messages printed, after printing them.
        #[arg(long, conflicts_with = "agent")]
        ack: bool,
        /// Bound the output and label messages for an agent.
        #[arg(long)]
        agent: bool,
        /// Start the first displayed message body at this byte offset.
        #[arg(long, requires = "agent")]
        offset: Option<usize>,
    },
    /// Retrieve one position without advancing the reader.
    #[command(after_help = examples::help(&["get"]))]
    Get {
        channel: String,
        position: u64,
        #[arg(long)]
        json: bool,
        /// Bound the output and label the message for an agent.
        #[arg(long)]
        agent: bool,
        /// Start the displayed body at this byte offset.
        #[arg(long, requires = "agent")]
        offset: Option<usize>,
    },
    /// Acknowledge a position already read in a particular channel creation.
    #[command(after_help = examples::help(&["ack"]))]
    Ack {
        channel: String,
        position: u64,
        /// Use this creation directly. Omitting it requires read permission.
        #[arg(long)]
        creation: Option<CreationId>,
    },
}

impl Command {
    pub(super) fn is_launcher_report(&self) -> bool {
        matches!(
            self,
            Self::Send { channel, id: Some(id), kind: Kind::Report, json: true, .. }
                if channel == "lead"
                    && std::env::var("MALAK_RECIPE_REPORT_RETRY").as_deref() == Ok("1")
                    && std::env::var("MALAK_REPORT_ID").as_deref() == Ok(id.as_str())
        )
    }

    pub(super) fn execute(mut self, config: Option<PathBuf>) -> Result<(), Error> {
        let guests = match &self {
            Self::Read {
                channels, limit, ..
            } if *limit != Some(0) && channels.iter().any(|channel| channel == "lead") => {
                guest_inbox::for_lead(&config)
            }
            Self::Ack {
                channel,
                creation: Some(_),
                ..
            } if channel == "lead" => guest_inbox::for_lead(&config),
            _ => Vec::new(),
        };
        let mut session = Session::new(config);
        session.launcher_report = self.is_launcher_report();
        let ack = matches!(self, Self::Read { ack: true, .. });
        let human = matches!(
            self,
            Self::Read {
                json: false,
                agent: false,
                ..
            }
        );
        if let Self::Read { channels, wait, .. } = &self
            && human
            && *wait > 0
        {
            output::progress(
                &format!("Waiting for messages in {}.", channels.join(", ")),
                Some("Press Ctrl-C to stop."),
            );
        }
        if let Self::Ack {
            channel,
            creation: Some(creation),
            ..
        } = &mut self
            && !guests.is_empty()
            && session.current_creation("lead".parse()?).ok() != Some(*creation)
            && let Some((remote, inbox)) = guests
                .iter()
                .find_map(|guest| guest.for_creation(*creation))
        {
            session = remote;
            *channel = inbox.to_string();
        }
        let mut remote = Vec::new();
        let (mut output, header) = if matches!(self, Self::Read { .. }) && !guests.is_empty() {
            self.read_with_guests(&mut session, &guests, &mut remote)?
        } else {
            self.exchange(&mut session, None)
                .map_err(|error| session.launcher_report_error(error))?
        };
        let printed = output.take_printed();
        output.ok(&header, !printed.is_empty())?;
        if human && !ack && !printed.is_empty() {
            let mut footer = String::new();
            output::Format::stdout().next(
                &mut footer,
                "With ack access, acknowledge handled messages.\nmalak ack --creation CREATION CHANNEL POSITION",
            );
            if !footer.is_empty() {
                output::write(&footer)?;
            }
        }
        if ack {
            io::stdout().flush().map_err(io_error)?;
            let mut local = Vec::new();
            for (channel, creation, position) in printed {
                match remote.iter().find(|(_, key)| key == &creation) {
                    Some((guest, _)) => guest.acknowledge(&creation, position, human)?,
                    None => local.push((channel, creation, position)),
                }
            }
            session.ack_printed(local, human)?;
        }
        Ok(())
    }

    fn read_with_guests<'a>(
        &self,
        session: &mut Session,
        guests: &'a [GuestInbox],
        remote: &mut Vec<(&'a GuestInbox, String)>,
    ) -> Result<(Output, Map<String, Value>), Error> {
        let Self::Read {
            channels,
            limit,
            wait,
            json,
            agent,
            offset,
            ..
        } = self
        else {
            unreachable!("only lead reads include guests");
        };
        let deadline = Instant::now() + Duration::from_secs(*wait).min(Limits::default().read_wait);
        let mut interval = Duration::ZERO;
        loop {
            let (mut output, header) = session.read_command(channels, interval, *limit, || {
                Output::messages(*json, *agent, offset.unwrap_or(0))
            })?;
            for guest in guests {
                let remaining =
                    limit.map(|limit| limit.saturating_sub(output.message_count() as u64));
                if remaining == Some(0) {
                    break;
                }
                for mut record in guest.read(remaining) {
                    let (creation, _) = record.address()?;
                    if remote.last().is_none_or(|(_, key)| key != creation) {
                        remote.push((guest, creation.to_owned()));
                    }
                    output.message(&mut record.label, &record.body)?;
                }
            }
            interval = REFRESH.min(deadline.saturating_duration_since(Instant::now()));
            if output.message_count() != 0 || interval.is_zero() {
                return Ok((output, header));
            }
        }
    }

    pub(super) fn execute_tool(
        self,
        session: &mut Session,
        body: Option<Vec<u8>>,
    ) -> Result<Value, Error> {
        let (output, header) = self.exchange(session, body)?;
        output.tool_result(header)
    }

    fn exchange(
        self,
        session: &mut Session,
        body: Option<Vec<u8>>,
    ) -> Result<(Output, Map<String, Value>), Error> {
        let (frame, output, wait) = match self {
            Self::Send {
                channel,
                id,
                kind,
                reply_to,
                content_type,
                max_bytes,
                trace,
                json,
                body: argument,
            } => {
                let id = id.map_or_else(random_id, Ok)?;
                let body = match body {
                    Some(body) => body,
                    None => read_body(argument.as_deref(), max_bytes)?,
                };
                if body.is_empty() {
                    return Err(Error::refused(EMPTY_BODY));
                }
                let kind = match kind {
                    Kind::Note => "note",
                    Kind::Task => "task",
                    Kind::Report => "report",
                };
                let reply = reply_to.as_slice();
                let reply = match reply {
                    [] => None,
                    [channel, creation, position] => {
                        let _: CreationId = creation.parse()?;
                        let position: u64 = position
                            .parse()
                            .map_err(|_| Error::refused("invalid reply position"))?;
                        Some((channel, creation, position))
                    }
                    _ => return Err(Error::refused("invalid reply address")),
                };
                let (trace, warning) = session.trace_for(trace, reply)?;
                if let Some(reason) = warning {
                    output::diagnostic(
                        "Warning: Could not read the message being answered.",
                        &[("Reason", &reason), ("Sending", "Without a trace")],
                    );
                }
                let agent_session = [
                    "CODEX_SESSION_ID",
                    "CODEX_THREAD_ID",
                    "CLAUDE_CODE_SESSION_ID",
                ]
                .into_iter()
                .find_map(|key| std::env::var(key).ok().filter(|id| !id.is_empty()));
                let frame = encode_frame(
                    2,
                    "SEND",
                    4 + usize::from(agent_session.is_some()),
                    |out| {
                        text(out, "channel", &channel);
                        text(out, "id", &id);
                        text(out, "kind", kind);
                        if let Some(id) = &agent_session {
                            text(out, "agent_session_id", id);
                        }
                        out.str("headers")
                            .unwrap()
                            .map(
                                u64::from(reply.is_some())
                                    + u64::from(content_type.is_some())
                                    + u64::from(trace.is_some()),
                            )
                            .unwrap();
                        if let Some(trace) = &trace {
                            text(out, "trace", trace);
                        }
                        if let Some(kind) = &content_type {
                            text(out, "content_type", kind);
                        }
                        if let Some((channel, creation, position)) = reply {
                            out.str("reply_to")
                                .unwrap()
                                .array(3)
                                .unwrap()
                                .str(channel)
                                .unwrap()
                                .str(creation)
                                .unwrap()
                                .u64(position)
                                .unwrap();
                        }
                    },
                    &body,
                );
                (
                    frame,
                    Output::Receipt {
                        id,
                        channel,
                        kind,
                        json,
                    },
                    CALL_WAIT,
                )
            }
            Self::Read {
                channels,
                limit,
                wait,
                json,
                agent,
                offset,
                ..
            } => {
                let wait = Duration::from_secs(wait).min(Limits::default().read_wait);
                let output = || Output::messages(json, agent, offset.unwrap_or(0));
                return session.read_command(&channels, wait, limit, output);
            }
            Self::Get {
                channel,
                position,
                json,
                agent,
                offset,
            } => {
                let frame =
                    encode_frame(2, "GET", 2, |out| get_fields(out, &channel, position), &[]);
                (
                    frame,
                    Output::messages(json, agent, offset.unwrap_or(0)),
                    CALL_WAIT,
                )
            }
            Self::Ack {
                channel,
                position,
                creation,
            } => {
                let creation = match creation {
                    Some(creation) => creation,
                    None => session.current_creation(channel.parse()?)?,
                };
                let creation = creation.to_string();
                let frame = encode_frame(
                    2,
                    "ACK",
                    3,
                    |out| ack_fields(out, &channel, &creation, position),
                    &[],
                );
                (
                    frame,
                    Output::Ack {
                        channel,
                        creation,
                        position,
                    },
                    CALL_WAIT,
                )
            }
        };
        let frame = frame.map_err(|_| Error::refused("request does not fit"))?;
        session.request(output, wait, false, |client| {
            client.send_frame(frame, CALL_WAIT)
        })
    }
}

fn ack_fields(out: &mut Encoder<Vec<u8>>, channel: &str, creation: &str, position: u64) {
    text(out, "channel", channel);
    number(out, "position", position);
    text(out, "creation", creation);
}

pub(super) fn read_for_hook(
    channel: String,
    config: Option<PathBuf>,
) -> Result<Option<(String, presentation::Message)>, Error> {
    let command = Command::Read {
        channels: vec![channel],
        limit: Some(1),
        wait: 0,
        json: false,
        ack: false,
        agent: true,
        offset: None,
    };
    let mut session = Session::new(config);
    let (output, _) = command.exchange(&mut session, None)?;
    let Output::Agent { messages, .. } = output else {
        unreachable!("agent read returns agent messages");
    };
    let shown = agent_text(&messages, 0)?;
    Ok(shown.values.into_iter().zip(messages).next())
}

pub(super) fn wait_for_inbox(
    config: Option<PathBuf>,
    inbox: String,
    timeout: Option<u32>,
) -> Result<(), Error> {
    let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs(seconds.into()));
    let mut session = Session::new(config);
    let channels = [inbox];
    let maximum = Limits::default().read_wait;
    loop {
        let wait = deadline.map_or(maximum, |at| {
            maximum.min(at.saturating_duration_since(Instant::now()))
        });
        if wait.is_zero() {
            return output::line(format_args!("No events."));
        }
        let (mut response, header) = session.read_waiting(&channels, wait, Some(1), || {
            Output::messages(false, false, 0)
        })?;
        if !response.take_printed().is_empty() {
            return response.ok(&header, true);
        }
    }
}

pub(super) struct ParticipantRecord {
    pub label: Map<String, Value>,
    pub preview: Option<String>,
    pub body_bytes: usize,
    pub preview_cut: bool,
}

pub(super) struct GuestRecord {
    pub label: Map<String, Value>,
    pub body: Vec<u8>,
}

impl GuestRecord {
    pub fn address(&self) -> Result<(&str, u64), Error> {
        Ok((
            field(&self.label, "creation")?,
            number_field(&self.label, "position")?,
        ))
    }
}

impl ParticipantRecord {
    fn new(label: Map<String, Value>, body: &[u8], include_text: bool) -> Self {
        let mut record = Self {
            label,
            preview: None,
            body_bytes: body.len(),
            preview_cut: false,
        };
        if include_text && let Some(text) = text_prefix(body) {
            let mut preview = text.to_owned();
            record.preview_cut = text.len() < record.body_bytes;
            if record.preview_cut {
                preview.push('…');
            }
            record.preview = Some(preview);
        }
        record
    }
}

pub(super) struct Session {
    config: Option<PathBuf>,
    check_board: bool,
    launcher_report: bool,
    client: Option<LocalClient<ClientTransport>>,
}

impl Session {
    fn new(config: Option<PathBuf>) -> Self {
        Self {
            check_board: config.is_some(),
            config,
            launcher_report: false,
            client: None,
        }
    }

    pub(super) fn open(config: Option<PathBuf>) -> Result<Self, Error> {
        let mut session = Self::new(config);
        session.client()?;
        Ok(session)
    }

    pub(super) fn open_guest(handover: HandoverFile, wait: Duration) -> Result<Self, Error> {
        let mechanism = mechanism(&handover)?;
        let (client, _) = ClientTransport::connect(|transport| {
            LocalClient::participant_with_wait(transport, handover, mechanism, wait)
                .map_err(link_io_error)
        })?;
        let mut session = Self::new(None);
        session.client = Some(client);
        Ok(session)
    }

    pub(super) fn read_guest(
        &mut self,
        inbox: &str,
        limit: Option<u64>,
        wait: Duration,
    ) -> Result<Vec<GuestRecord>, Error> {
        let frame = encode_frame(
            2,
            "READ",
            2 + usize::from(limit.is_some()),
            |out| read_fields(out, &[inbox], Duration::ZERO, limit),
            &[],
        )
        .map_err(|_| invalid_reply())?;
        let (output, _) = self.request(Output::Collected(Vec::new()), wait, false, |client| {
            client.send_frame(frame, wait)
        })?;
        let Output::Collected(records) = output else {
            unreachable!("guest reads collect records")
        };
        Ok(records)
    }

    fn client(&mut self) -> Result<&mut LocalClient<ClientTransport>, Error> {
        self.client_with_wait(None)
    }

    fn client_with_wait(
        &mut self,
        remaining: Option<Duration>,
    ) -> Result<&mut LocalClient<ClientTransport>, Error> {
        if self.client.is_none() {
            let path = handover_path(self.config.clone())?;
            let board = self
                .check_board
                .then(|| Config::read(self.config.clone()))
                .transpose()?;
            let select = |handover: &HandoverFile| {
                if let Some(config) = &board {
                    config.board.check_handover(&path, handover)?;
                }
                mechanism(handover)
            };
            let (client, _) = ClientTransport::connect(|transport| {
                if let Some(wait) = remaining.or_else(|| self.launcher_report.then_some(CALL_WAIT))
                {
                    // The waiting read or launcher owns the retry window.
                    let handover = HandoverFile::read(&path)?;
                    let mechanism = select(&handover)?;
                    LocalClient::participant_with_wait(transport, handover, mechanism, wait)
                        .map_err(link_io_error)
                } else {
                    LocalClient::participant_from_file(transport, &path, select)
                }
            })?;
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("participant is attached"))
    }

    fn launcher_report_error(&self, error: Error) -> Error {
        if self.launcher_report
            && report_connection_failed(&error)
            && let Some(reason) = self.confirm_report_refusal()
        {
            return Error::refused(reason);
        }
        error
    }

    fn confirm_report_refusal(&self) -> Option<&'static str> {
        // Authentication failures deliberately close the link without a
        // reason. The stock launcher can check its enrolment as the owner.
        let path = handover_path(self.config.clone()).ok()?;
        let handover = HandoverFile::read(&path).ok()?;
        let config = Config::read(self.config.clone()).ok()?;
        config.board.check_handover(&path, &handover).ok()?;
        let (mut owner, _, _) = owner_for_board_quiet(&config.board).ok()?;
        let status = owner_commands::status(&mut owner, &[("id", handover.id().as_str())]).ok()?;
        let person = status["participants"]
            .as_array()?
            .iter()
            .find(|person| person["id"] == handover.id().as_str())?;
        if person["revoked"].as_bool() == Some(true) {
            return Some("the worker enrolment was revoked");
        }
        if owner_commands::grant_end(person).is_some_and(|end| end <= output::now_ms()) {
            return Some("the worker grant expired");
        }
        let fingerprint = person["fingerprint"].as_str()?;
        let public = malak_proto::KeyPair::public_for(handover.private_key()).ok()?;
        let mut key_hash = String::with_capacity(64);
        for byte in Sha256::digest(public) {
            write!(key_hash, "{byte:02x}").expect("writing to a string");
        }
        (key_hash != fingerprint).then_some("the worker credential was replaced")
    }

    /// Acknowledge the highest printed position of each channel creation.
    pub(super) fn ack_printed(
        &mut self,
        printed: Vec<(String, String, u64)>,
        feedback: bool,
    ) -> Result<(), Error> {
        for (channel, creation, position) in printed {
            let frame = encode_frame(
                2,
                "ACK",
                3,
                |out| ack_fields(out, &channel, &creation, position),
                &[],
            )
            .map_err(|_| Error::refused("request does not fit"))?;
            self.request(Output::Quiet, CALL_WAIT, false, |client| {
                client.send_frame(frame, CALL_WAIT)
            })
            .map_err(|error| {
                let reason = format!(
                    "messages were shown but not acknowledged in {channel} through #{position}: {error}"
                );
                match error.outcome() {
                    Outcome::Refused => Error::refused(reason),
                    Outcome::NotSupported => Error::not_supported(reason),
                    _ => Error::unavailable(reason),
                }
            })?;
            if feedback {
                output::progress(
                    &format!("Acknowledged messages in {channel} through #{position}."),
                    None,
                );
            }
        }
        Ok(())
    }

    /// Supply an explicit trace or inherit it from the addressed message.
    /// The board selects the trace for unaddressed sends without a trace.
    fn trace_for(
        &mut self,
        explicit: Option<String>,
        reply: Option<(&String, &String, u64)>,
    ) -> Result<(Option<String>, Option<String>), Error> {
        if let Some(trace) = explicit {
            if trace.len() > 64 || !trace.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::refused(
                    "trace must be 1 to 64 hexadecimal characters",
                ));
            }
            return Ok((Some(trace), None));
        }
        let Some((channel, creation, position)) = reply else {
            return Ok((None, None));
        };
        self.client()?;
        match self.get_record(channel, creation, position, false) {
            Ok(Some(record)) => Ok((
                trace_header(&record.label).filter(|trace| !trace.is_empty()),
                None,
            )),
            Ok(None) => Ok((None, None)),
            Err(error) => {
                self.client = None;
                Ok((None, Some(error.to_string())))
            }
        }
    }

    fn current_creation(&mut self, channel: ChannelName) -> Result<CreationId, Error> {
        self.creation_within(channel, CALL_WAIT)
    }

    pub(super) fn creation_within(
        &mut self,
        channel: ChannelName,
        wait: Duration,
    ) -> Result<CreationId, Error> {
        // A zero-message read discovers the creation without giving the reader
        // a position it could then acknowledge.
        let frame = encode_frame(
            2,
            "READ",
            3,
            |out| read_fields(out, &[channel.as_str()], Duration::ZERO, Some(0)),
            &[],
        )
        .map_err(|_| Error::refused("request does not fit"))?;
        self.client()?;
        let (_, header) = self
            .request(Output::Quiet, wait, false, |client| {
                client.send_frame(frame, wait)
            })
            .map_err(|error| match error.outcome() {
                Outcome::Refused => {
                    Error::refused(format!("{error}. Pass --creation to ack without a read."))
                }
                _ => error,
            })?;
        let Some([entry]) = header
            .get("channels")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        else {
            return Err(invalid_reply());
        };
        if entry["channel"].as_str() != Some(channel.as_str()) {
            return Err(invalid_reply());
        }
        let Some(creation) = entry.get("creation") else {
            return Err(Error::refused("channel is absent"));
        };
        creation
            .as_str()
            .ok_or_else(invalid_reply)?
            .parse()
            .map_err(|_| invalid_reply())
    }

    fn request(
        &mut self,
        mut output: Output,
        wait: Duration,
        defer_messages: bool,
        send: impl FnOnce(&mut LocalClient<ClientTransport>) -> Result<(), Error>,
    ) -> Result<(Output, Map<String, Value>), Error> {
        let client = self.client()?;
        let mut pending = Vec::new();
        let reply = (|| {
            send(client).map_err(link_io_error)?;
            loop {
                let frame = client.read_frame(wait).map_err(link_io_error)?;
                if frame.rid() != 2 {
                    return Err(invalid_reply());
                }
                let Value::Object(mut header) = value(&mut Decoder::new(frame.header()))? else {
                    return Err(invalid_reply());
                };
                match frame.op() {
                    "MSG" if defer_messages => pending.push((header, frame)),
                    "MSG" => output.message(&mut header, frame.body())?,
                    "OK" | "ERR" if frame.body().is_empty() => {
                        return Ok((frame.op() == "OK", header));
                    }
                    _ => return Err(invalid_reply()),
                }
            }
        })();
        match reply {
            Ok((true, header)) => {
                for (mut label, frame) in pending {
                    output.message(&mut label, frame.body())?;
                }
                Ok((output, header))
            }
            Ok((false, header)) => Err(remote_error(&header)),
            Err(error) => {
                self.client = None;
                Err(error)
            }
        }
    }

    fn read_waiting(
        &mut self,
        channels: &[String],
        wait: Duration,
        limit: Option<u64>,
        output: impl Fn() -> Output,
    ) -> Result<(Output, Map<String, Value>), Error> {
        self.client()?;
        let deadline = Instant::now() + wait;
        let read = |session: &mut Self| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = encode_frame(
                2,
                "READ",
                2 + usize::from(limit.is_some()),
                |out| read_fields(out, channels, remaining, limit),
                &[],
            )
            .map_err(|_| Error::refused("request does not fit"))?;
            // A dropped attempt may have sent MSG frames but no closing reply.
            // Publish only the completed attempt so a retry cannot print twice.
            session.request(output(), remaining + CALL_WAIT, true, |client| {
                client.send_frame(frame, CALL_WAIT)
            })
        };
        match read(self) {
            Err(error) if link_failed(&error) => reconnect(deadline, |remaining| {
                self.client = None;
                self.client_with_wait(Some(remaining))?;
                read(self)
            }),
            result => result,
        }
    }

    fn read_command(
        &mut self,
        channels: &[String],
        wait: Duration,
        limit: Option<u64>,
        output: impl Fn() -> Output,
    ) -> Result<(Output, Map<String, Value>), Error> {
        if !wait.is_zero() {
            return self.read_waiting(channels, wait, limit, output);
        }
        let frame = encode_frame(
            2,
            "READ",
            2 + usize::from(limit.is_some()),
            |out| read_fields(out, channels, wait, limit),
            &[],
        )
        .map_err(|_| Error::refused("request does not fit"))?;
        self.request(output(), CALL_WAIT, false, |client| {
            client.send_frame(frame, CALL_WAIT)
        })
    }

    pub(super) fn open_preview(config: Option<PathBuf>) -> Option<(Self, ParticipantId)> {
        let handover = preview_handover(config.clone()).ok()?;
        let id = handover.id().clone();
        let client = attach_preview(handover).ok()?;
        Some((
            Self {
                config,
                check_board: true,
                launcher_report: false,
                client: Some(client),
            },
            id,
        ))
    }

    /// Open previews for a board in the owner's multi-board mailbox.
    pub(super) fn open_mailbox_preview(config: PathBuf) -> Option<(Self, ParticipantId)> {
        let board = Config::read(Some(config.clone())).ok()?.board;
        // The environment can name a participant of one board only. Use
        // that identity where it matches and each other board's own lead.
        let handover = if let Ok(handover) = preview_handover(Some(config.clone())) {
            handover
        } else {
            let path = board.lead_handover();
            let handover = HandoverFile::read(&path).ok()?;
            board.check_handover(&path, &handover).ok()?;
            handover
        };
        let id = handover.id().clone();
        let mechanism = mechanism(&handover).ok()?;
        let (client, _, _) = ClientTransport::connect_quiet(|transport| {
            LocalClient::participant(transport, handover, mechanism)
        })
        .ok()?;
        Some((
            Self {
                config: Some(config),
                check_board: true,
                launcher_report: false,
                client: Some(client),
            },
            id,
        ))
    }

    pub(super) fn get_record(
        &mut self,
        channel: &str,
        creation: &str,
        position: u64,
        include_text: bool,
    ) -> Result<Option<ParticipantRecord>, Error> {
        self.get_record_with_body(channel, creation, position, include_text, false)
            .map(|result| result.map(|(record, _)| record))
    }

    // Use the same authenticated GET and address check for callers that need
    // opaque bytes as well as the label. Metadata-only callers skip the copy.
    pub(super) fn get_record_with_body(
        &mut self,
        channel: &str,
        creation: &str,
        position: u64,
        include_text: bool,
        include_body: bool,
    ) -> Result<Option<(ParticipantRecord, Vec<u8>)>, Error> {
        self.get_record_matching(
            channel,
            creation,
            position,
            include_text,
            include_body,
            |_| true,
        )
    }

    pub(super) fn get_record_matching(
        &mut self,
        channel: &str,
        creation: &str,
        position: u64,
        include_text: bool,
        include_body: bool,
        matches: impl Fn(&Map<String, Value>) -> bool,
    ) -> Result<Option<(ParticipantRecord, Vec<u8>)>, Error> {
        let client = self.client.as_mut().ok_or_else(invalid_reply)?;
        let frame = encode_frame(2, "GET", 2, |out| get_fields(out, channel, position), &[])
            .map_err(|_| invalid_reply())?;
        let wait = Duration::from_secs(2);
        let reply = (|| {
            client.send_frame(frame, wait).map_err(link_io_error)?;
            let mut record = None;
            loop {
                let frame = client.read_frame(wait).map_err(link_io_error)?;
                if frame.rid() != 2 {
                    return Err(invalid_reply());
                }
                match frame.op() {
                    "MSG" => {
                        let Value::Object(label) = value(&mut Decoder::new(frame.header()))? else {
                            return Err(invalid_reply());
                        };
                        if label["creation"].as_str() == Some(creation)
                            && label["channel"].as_str() == Some(channel)
                            && label["position"].as_u64() == Some(position)
                            && matches(&label)
                        {
                            let body = if include_body {
                                frame.body().to_vec()
                            } else {
                                Vec::new()
                            };
                            record = Some((
                                ParticipantRecord::new(label, frame.body(), include_text),
                                body,
                            ));
                        }
                    }
                    "OK" if frame.body().is_empty() => return Ok(Ok(record)),
                    "ERR" if frame.body().is_empty() => {
                        let Value::Object(header) = value(&mut Decoder::new(frame.header()))?
                        else {
                            return Err(invalid_reply());
                        };
                        return Ok(Err(remote_error(&header)));
                    }
                    _ => return Err(invalid_reply()),
                }
            }
        })();
        match reply {
            Ok(result) => result,
            Err(error) => {
                self.client = None;
                Err(error)
            }
        }
    }

    pub(super) fn is_connected(&self) -> bool {
        self.client.is_some()
    }
}

pub(super) fn check_preview_board(config: Option<PathBuf>) -> Result<(), Error> {
    if std::env::var_os("MALAK_HANDOVER").is_some() {
        preview_handover(config)?;
    }
    Ok(())
}

pub(super) fn check_recipe_handover(config: Option<PathBuf>) -> Result<(), Error> {
    let mut session = Session::new(config.clone());
    session.check_board = true;
    let result = session.client().map(|_| ());
    if result
        .as_ref()
        .is_err_and(|error| error.outcome() == Outcome::Unavailable)
    {
        // Keep the specific diagnostic when the board itself is unavailable.
        super::owner(config)?;
    }
    result
}

fn attach_preview(handover: HandoverFile) -> Result<LocalClient<ClientTransport>, Error> {
    let mechanism = mechanism(&handover)?;
    ClientTransport::connect(|transport| LocalClient::participant(transport, handover, mechanism))
        .map(|(client, _)| client)
}

pub(super) fn preview_handover(config: Option<PathBuf>) -> Result<HandoverFile, Error> {
    let path = handover_path(config.clone())?;
    let handover = HandoverFile::read(&path)?;
    Config::read(config)?
        .board
        .check_handover(&path, &handover)?;
    Ok(handover)
}

fn handover_path(config: Option<PathBuf>) -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os("MALAK_HANDOVER") {
        return found(
            path.into(),
            "check that MALAK_HANDOVER names your handover file",
        );
    }
    let unset = |reason: &str| Error::refused(format!("MALAK_HANDOVER is unset and {reason}"));
    let board = Config::read(config)
        .map_err(|error| unset(&error.to_string()))?
        .board;
    // OS read access to board state defines the owner's zone. Opening the
    // lead handover still checks the private file's permissions.
    Directory::open(&board.state).map_err(|_| {
        unset(&format!(
            "the board state {} cannot be read; {}",
            board.state.display(),
            START_HINT
        ))
    })?;
    found(board.lead_handover(), START_HINT)
}

/// The handover file, or an error that names it and the next step.
fn found(path: PathBuf, next: &str) -> Result<PathBuf, Error> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::refused(format!(
            "no handover file at {}; {next}",
            path.display()
        ))),
        _ => Ok(path),
    }
}

fn text_prefix(body: &[u8]) -> Option<&str> {
    let prefix = &body[..body.len().min(128)];
    match std::str::from_utf8(prefix) {
        Ok(text) => Some(text),
        Err(error) if error.error_len().is_none() && prefix.len() < body.len() => {
            std::str::from_utf8(&prefix[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn mechanism(handover: &HandoverFile) -> Result<Option<Arc<dyn Mechanism>>, Error> {
    match handover.binding().entrance {
        Entrance::Lead => Ok(None),
        Entrance::General if handover.mechanism() == "key" => Ok(Some(Arc::new(Key))),
        _ => Err(Error::not_supported("handover mechanism is unavailable")),
    }
}

fn get_fields(out: &mut Encoder<Vec<u8>>, channel: &str, position: u64) {
    text(out, "channel", channel);
    number(out, "position", position);
}

fn read_fields(
    out: &mut Encoder<Vec<u8>>,
    channels: &[impl AsRef<str>],
    wait: Duration,
    limit: Option<u64>,
) {
    out.str("channels")
        .unwrap()
        .array(channels.len() as u64)
        .unwrap();
    for channel in channels {
        out.str(channel.as_ref()).unwrap();
    }
    number(out, "wait_ms", wait.as_millis() as u64);
    if let Some(limit) = limit {
        number(out, "limit", limit);
    }
}

fn field<'a>(header: &'a Map<String, Value>, name: &str) -> Result<&'a str, Error> {
    header
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(invalid_reply)
}

fn number_field(header: &Map<String, Value>, name: &str) -> Result<u64, Error> {
    header
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(invalid_reply)
}

fn remote_error(header: &Map<String, Value>) -> Error {
    let reason = header
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("board refused the call")
        .to_owned();
    match header.get("outcome").and_then(Value::as_str) {
        Some("refused") => Error::refused(reason),
        Some("unavailable") => Error::unavailable(reason),
        Some("not_supported") => Error::not_supported(reason),
        _ => invalid_reply(),
    }
}

fn invalid_reply() -> Error {
    Error::unavailable("invalid reply from the board")
}

fn value(input: &mut Decoder<'_>) -> Result<Value, Error> {
    let invalid = |_| invalid_reply();
    match input.datatype().map_err(invalid)? {
        Type::String => Ok(Value::from(input.str().map_err(invalid)?)),
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => {
            Ok(Value::from(input.u64().map_err(invalid)?))
        }
        Type::Bool => Ok(Value::from(input.bool().map_err(invalid)?)),
        Type::Null => {
            input.null().map_err(invalid)?;
            Ok(Value::Null)
        }
        Type::Array => {
            let count = input.array().map_err(invalid)?.ok_or_else(invalid_reply)?;
            let items = (0..count)
                .map(|_| value(input))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Array(items))
        }
        Type::Map => {
            let count = input.map().map_err(invalid)?.ok_or_else(invalid_reply)?;
            let mut fields = Map::new();
            for _ in 0..count {
                let name = input.str().map_err(invalid)?.to_owned();
                fields.insert(name, value(input)?);
            }
            Ok(Value::Object(fields))
        }
        _ => Err(invalid_reply()),
    }
}

/// The `trace` header of a message frame, if it carries one.
pub(super) fn trace_header(frame: &Map<String, Value>) -> Option<String> {
    let trace = frame.get("headers")?.get("trace")?;
    trace.as_str().map(str::to_owned)
}

pub(super) fn random_id() -> Result<String, Error> {
    random_number().map(|number| format!("{number:032x}"))
}

fn random_number() -> Result<u128, Error> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::unavailable("could not make a message ID"))?;
    Ok(u128::from_be_bytes(bytes))
}

fn read_body(argument: Option<&str>, maximum: Option<usize>) -> Result<Vec<u8>, Error> {
    let maximum = maximum
        .unwrap_or(usize::MAX)
        .min(Limits::default().body_bytes);
    let bytes = match argument {
        Some("-") => {
            let mut bytes = Vec::new();
            io::stdin()
                .lock()
                .take(maximum as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io_error)?;
            bytes
        }
        argument => argument.unwrap_or_default().as_bytes().to_vec(),
    };
    if bytes.len() > maximum {
        return Err(Error::refused("message body exceeds the byte limit"));
    }
    Ok(bytes)
}

fn text(out: &mut Encoder<Vec<u8>>, key: &str, value: &str) {
    out.str(key).unwrap().str(value).unwrap();
}

fn number(out: &mut Encoder<Vec<u8>>, key: &str, value: u64) {
    out.str(key).unwrap().u64(value).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_preview_retains_the_original_byte_length() {
        let prefix = "x".repeat(127);
        let body = format!("{prefix}é");

        let record = ParticipantRecord::new(Map::new(), body.as_bytes(), true);

        assert_eq!(record.body_bytes, 129);
        assert_eq!(record.preview, Some(format!("{prefix}…")));
    }

    #[test]
    fn the_cut_flag_distinguishes_a_shortened_preview_from_a_literal_ellipsis() {
        let body = "x".repeat(129);
        let cut = ParticipantRecord::new(Map::new(), body.as_bytes(), true);
        let literal = ParticipantRecord::new(Map::new(), "ends with …".as_bytes(), true);

        assert!(cut.preview_cut);
        assert!(!literal.preview_cut);
    }
}
