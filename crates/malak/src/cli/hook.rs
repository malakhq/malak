use super::{config::io_error, output, participant, presentation};
use malak::{Error, Outcome};
use nix::sys::stat::{Mode, umask};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod check;
mod describe;
mod format;
mod state;
use format::{Format, Tool};
use state::Pending;

const CALL_TIMEOUT: Duration = Duration::from_secs(40);
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(2);
const HANDOVER: &str = "MALAK_HANDOVER";
const INBOX: &str = "MALAK_HOOK_INBOX";
const STATE: &str = "MALAK_HOOK_STATE";

#[derive(clap::Args)]
pub(super) struct Args {
    /// Select the tool format. Inspect and check the generic format by default.
    #[arg(long, value_enum, required_unless_present_any = ["describe", "check"])]
    tool: Option<Tool>,
    /// Print the stdin, stdout and event contract for a tool format.
    #[arg(long, conflicts_with = "check")]
    describe: bool,
    /// Send and receive a probe through the configured hook.
    #[arg(long)]
    check: bool,
    /// Print the hook description as JSON.
    #[arg(long, requires = "describe", conflicts_with = "check")]
    pub(super) json: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EventName {
    SessionStart,
    Prompt,
    Step,
    SessionEnd,
}

impl EventName {
    const ALL: [Self; 4] = [
        Self::SessionStart,
        Self::Prompt,
        Self::Step,
        Self::SessionEnd,
    ];
}

struct Event {
    session: String,
    name: EventName,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    channel: String,
    position: String,
    creation: String,
    /// Set once a `prompt` or `step` event followed the delivery, which shows the agent ran.
    #[serde(default)]
    ran: bool,
}

impl Receipt {
    fn acknowledge(self, config: Option<PathBuf>) -> Result<(), Error> {
        participant::Command::Ack {
            channel: self.channel,
            position: self
                .position
                .parse()
                .map_err(|_| Error::refused("invalid hook receipt position"))?,
            creation: Some(self.creation.parse()?),
        }
        .execute_tool(&mut participant::Session::open(config)?, None)
        .map(|_| ())
    }
}

pub(super) fn run(args: Args, config: Option<PathBuf>) -> Result<ExitCode, Error> {
    let tool = args.tool.unwrap_or(Tool::Generic);
    if args.describe {
        describe::write(&mut io::stdout().lock(), tool, args.json).map_err(io_error)?;
        return Ok(ExitCode::SUCCESS);
    }
    umask(Mode::S_IRWXG | Mode::S_IRWXO);
    if args.check {
        check::run(tool.format(), config)?;
        return Ok(ExitCode::SUCCESS);
    }
    if env::var_os("MALAK_RECIPE_NATIVE_ONLY").as_deref() == Some("1".as_ref())
        && env::var_os("MALAK_RECIPE_NATIVE_HOOK").as_deref() != Some("1".as_ref())
    {
        return Ok(ExitCode::SUCCESS);
    }
    Ok(
        match deliver(
            tool.format(),
            config,
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::error(
                    "deliver the hook event",
                    &error.to_string(),
                    "Run malak hook --describe to check the input and environment.",
                );
                ExitCode::from(1)
            }
        },
    )
}

fn deliver(
    format: &dyn Format,
    config: Option<PathBuf>,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> io::Result<()> {
    let Some(event) = format.read(input)? else {
        return Ok(());
    };
    let configured = Configured::read(config).map_err(io::Error::other)?;
    let inbox = &configured.inbox;
    let pending = configured.pending(inbox, &event.session)?;

    let ending = event.name == EventName::SessionEnd;
    let timeout = if ending {
        SESSION_END_TIMEOUT
    } else {
        CALL_TIMEOUT
    };
    if !ending {
        // A failed acknowledgement must not hide that the agent ran.
        pending.mark_ran()?;
    }
    if let Err(error) = acknowledge_pending(&configured, &pending, inbox, timeout, ending)? {
        output::error(
            &format!("deliver hook messages from {inbox}"),
            &error.to_string(),
            "Run malak hook --check to verify the configured hook.",
        );
        if error.outcome() != Outcome::Refused {
            return format.write(output, event.name, "");
        }
        // The receipt can belong to a previous run or channel creation.
        pending.remove()?;
    }
    if ending {
        return format.write(output, event.name, "");
    }

    match offer(&configured, inbox, &pending, |_, context| {
        format.write(output, event.name, context)
    })? {
        Ok(Some(())) => Ok(()),
        Ok(None) => format.write(output, event.name, ""),
        Err(error) => {
            output::error(
                &format!("deliver hook messages from {inbox}"),
                &error.to_string(),
                "Run malak hook --check to verify the configured hook.",
            );
            format.write(output, event.name, "")
        }
    }
}

struct Configured {
    config: Option<PathBuf>,
    handover: PathBuf,
    inbox: String,
    state: PathBuf,
}

impl Configured {
    fn read(config: Option<PathBuf>) -> Result<Self, Error> {
        if config.is_some() {
            // Refuse a mismatched handover before touching the pending receipt.
            participant::preview_handover(config.clone())?;
        }
        Ok(Self {
            config,
            inbox: env::var(INBOX)
                .map_err(|_| Error::refused(format!("{INBOX} is absent or invalid")))?,
            state: required_path(STATE).map_err(io_error)?,
            handover: required_path(HANDOVER).map_err(io_error)?,
        })
    }

    fn pending(&self, channel: &str, session: &str) -> io::Result<Pending> {
        Pending::lock(&self.state, &self.handover, channel, session)
    }
}

fn acknowledge_pending(
    configured: &Configured,
    pending: &Pending,
    inbox: &str,
    timeout: Duration,
    only_if_ran: bool,
) -> io::Result<Result<(), Error>> {
    let Some(receipt) = pending.load()? else {
        return Ok(Ok(()));
    };
    if only_if_ran && !receipt.ran {
        // No prompt or step followed the delivery, so the message stays due.
        return Ok(Ok(()));
    }
    if receipt.channel != inbox {
        return Err(invalid("hook receipt belongs to a different channel"));
    }
    let config = configured.config.clone();
    let result = board_call(timeout, move || receipt.acknowledge(config))?;
    if result.is_ok() {
        pending.remove()?;
    }
    Ok(result)
}

fn offer<T>(
    configured: &Configured,
    inbox: &str,
    pending: &Pending,
    write: impl FnOnce(&presentation::Message, &str) -> io::Result<T>,
) -> io::Result<Result<Option<T>, Error>> {
    let requested = inbox.to_owned();
    let config = configured.config.clone();
    match board_call(CALL_TIMEOUT, move || {
        participant::read_for_hook(requested, config)
    })? {
        Ok(Some((context, message))) => {
            let (channel, creation, position) = message.address();
            if channel != inbox {
                return Err(invalid("unexpected Malak presentation"));
            }
            let shown = write(&message, &context)?;
            // A failed write or a kill before this point leaves the message due.
            pending.save(&Receipt {
                channel: channel.to_owned(),
                creation: creation.to_owned(),
                position: position.to_string(),
                ran: false,
            })?;
            Ok(Ok(Some(shown)))
        }
        Ok(None) => Ok(Ok(None)),
        Err(error) => Ok(Err(error)),
    }
}

fn required_path(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| invalid(format!("{name} is unset")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn board_call<T: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> io::Result<Result<T, Error>> {
    // Include attachment and the request in the deadline. A timed-out worker
    // owns only the board call and the hook process exits without saving state.
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new().spawn(move || {
        let _ = send.send(call());
    })?;
    receive.recv_timeout(timeout).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => {
            io::Error::new(io::ErrorKind::TimedOut, "board call timed out")
        }
        mpsc::RecvTimeoutError::Disconnected => {
            io::Error::other("board call ended without a result")
        }
    })
}
