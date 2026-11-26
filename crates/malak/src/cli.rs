use board_version::BoardVersion;
use clap::{
    CommandFactory, FromArgMatches, Parser, Subcommand,
    error::{ContextKind, ContextValue, ErrorKind},
};
use client_transport::ClientTransport;
use malak::{
    Action, ChannelName, ChannelPattern, CreationId, Error, Grant, Outcome, ParticipantId,
};
use malak_core::{LocalClient, OwnerCredential};
use nix::unistd::User;
use std::path::PathBuf;
use std::process::ExitCode;

mod board;
mod board_group;
mod board_list;
mod board_names;
mod board_version;
mod cargo_install;
mod client_transport;
mod config;
mod examples;
mod guest_inbox;
mod help;
mod hook;
mod mailbox;
#[allow(dead_code, reason = "the stdio tool server ships in v0.2")]
mod mcp;
mod output;
mod owner_commands;
mod participant;
mod presentation;
mod recipe;
mod uninstall;
mod upgrade;
mod worker_launch;

#[derive(Parser)]
#[command(
    name = "malak",
    bin_name = "malak",
    about,
    version,
    after_help = format!(
        "To become or take over as the Malak lead, see malak recipe lead-up --help.\n\n{}",
        examples::help(&["send"])
    )
)]
struct Args {
    /// Use the board configuration file at PATH.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Select a named board. Use either --board or --config.
    #[arg(long, global = true, value_name = "NAME", conflicts_with = "config")]
    board: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Discover, start, inspect or stop local boards.
    #[command(after_help = examples::help(&["board list", "board start"]))]
    Board {
        #[command(subcommand)]
        command: Board,
    },
    /// Stop all boards and remove Malak. Keep board data by default.
    #[command(after_help = examples::help(&["uninstall"]))]
    Uninstall(uninstall::Options),
    /// Install a Malak upgrade, refresh agent integrations and restart running boards.
    #[command(after_help = examples::help(&["upgrade"]))]
    Upgrade(upgrade::Options),
    /// Enrol a participant and write their private handover.
    #[command(after_help = examples::help(&["enrol"]))]
    Enrol {
        name: String,
        #[arg(long = "grant", value_name = "PATTERN:ACTIONS")]
        grants: Vec<String>,
        #[arg(long)]
        only_from: Option<String>,
        #[arg(long)]
        account: Option<String>,
        /// Bind the handover to the session of this running PID on the board host.
        #[arg(long, value_name = "PID", conflicts_with = "unbound")]
        session: Option<u32>,
        /// Allow the handover to be used from different local sessions.
        #[arg(long)]
        unbound: bool,
        #[arg(long, hide = true, requires = "session")]
        launch_program: Option<String>,
        #[arg(long)]
        out: PathBuf,
        /// Print the participant ID and handover path as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Replace a participant's complete grant.
    #[command(after_help = examples::help(&["grant"]))]
    Grant {
        id: ParticipantId,
        #[arg(value_name = "PATTERN:ACTIONS")]
        grants: Vec<String>,
    },
    /// Replace the credential for an enrolled participant or the lead.
    #[command(after_help = examples::help(&["rekey"]))]
    Rekey {
        id: ParticipantId,
        #[arg(long)]
        fingerprint: Option<String>,
        #[arg(long, hide = true)]
        session: Option<u32>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Release the current session claim of a participant.
    #[command(after_help = examples::help(&["release"]))]
    Release {
        #[arg(value_name = "NAME")]
        target: String,
    },
    /// Revoke a participant by ID or active name and close its connections.
    #[command(after_help = examples::help(&["revoke"]))]
    Revoke {
        #[arg(value_name = "ID_OR_NAME")]
        target: String,
    },
    /// Create or remove a channel.
    #[command(after_help = examples::help(&["channel create", "channel remove"]))]
    Channel {
        #[command(subcommand)]
        command: Channel,
    },
    /// Drop a channel's history before a position.
    #[command(after_help = examples::help(&["trim"]))]
    Trim {
        channel: ChannelName,
        position: u64,
        #[arg(long)]
        creation: Option<CreationId>,
    },
    /// Follow board events.
    #[command(after_help = examples::help(&["tail"]))]
    Tail {
        #[arg(long)]
        channel: Option<ChannelPattern>,
        #[arg(long)]
        json: bool,
    },
    /// Watch inbox acknowledgements and live activity at the owner door.
    ///
    /// With --plain --for lead, follow the lead inbox, questions, orders and stuck work.
    #[command(after_help = examples::help(&[
        "mailbox", "mailbox --plain", "mailbox --next", "mailbox --for", "mailbox --who",
        "mailbox --questions", "mailbox --orders", "mailbox --waiting", "mailbox --trace",
    ]))]
    Mailbox {
        /// Show activity involving one enrolled participant by name.
        #[arg(long = "for", value_name = "NAME")]
        name: Option<String>,
        /// Write one live line per event without opening the alternate screen.
        #[arg(long)]
        plain: bool,
        /// Wait for a message in your inbox, or a lead event with --plain --for lead.
        #[arg(long, requires = "name", conflicts_with_all = ["mailbox_list", "who"])]
        next: bool,
        /// Exit with "No events." after this many seconds of waiting with --next.
        #[arg(long, value_name = "SECONDS", requires = "next", value_parser = clap::value_parser!(u32).range(1..))]
        timeout: Option<u32>,
        /// Show participant and grant dates.
        #[arg(long, conflicts_with = "plain")]
        who: bool,
        /// List open questions, with questions for the owner first.
        #[arg(long, group = "mailbox_list", conflicts_with_all = ["who", "name"])]
        questions: bool,
        /// Print open questions, waiting inboxes, or trace history as JSON.
        #[arg(long, requires = "mailbox_list")]
        json: bool,
        /// List orders in force and ended, with the decisions made under each.
        #[arg(long, group = "mailbox_list", conflicts_with_all = ["who", "name", "questions"])]
        orders: bool,
        /// List workers' inboxes with unacknowledged messages from the lead.
        #[arg(long, group = "mailbox_list", conflicts_with_all = ["plain", "orders"])]
        waiting: bool,
        /// Show messages by trace ID (1 to 64 ASCII hexadecimal characters).
        #[arg(long, value_name = "ID", value_parser = mailbox::trace_id, group = "mailbox_list", conflicts_with_all = ["name", "who", "questions", "orders", "waiting"])]
        trace: Option<String>,
    },
    /// Look for damage and print a recovery plan for a stopped board.
    #[command(after_help = examples::help(&["recover"]))]
    Recover {
        /// Maximum candidate bytes to check per byte of channel data.
        #[arg(long, conflicts_with_all = ["apply", "withdraw"])]
        work_factor: Option<u64>,
        /// Apply the exact plan printed by a look, or continue an accepted plan.
        #[arg(long, conflicts_with = "withdraw")]
        apply: Option<String>,
        /// Confirm that the local store volume was neither restored from an image nor copied.
        #[arg(long, requires = "apply")]
        owner_vouches_same_volume: bool,
        /// Withdraw unfinished work from an accepted plan.
        #[arg(long)]
        withdraw: Option<String>,
    },
    /// Deliver inbox messages to an agent tool over stdio.
    #[command(after_help = examples::help(&["hook"]))]
    Hook(hook::Args),
    /// List or run recipes shipped with Malak.
    #[command(
        after_help = examples::help(&["recipe"]),
        after_long_help = format!("{}\n\n{}", examples::help(&["recipe"]), recipe::AGENT_SETUP)
    )]
    Recipe(recipe::Args),
    #[command(flatten)]
    Participant(participant::Command),
}

#[derive(Subcommand)]
enum Board {
    /// List known local boards, their status and lead inbox counts.
    #[command(after_help = examples::help(&["board list"]))]
    List {
        /// Print the board list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start or reopen a board in the foreground.
    #[command(after_help = examples::help(&["board start"]))]
    Start {
        #[arg(long)]
        lead_handover: Option<PathBuf>,
        #[arg(long)]
        lead_account: Option<String>,
        /// Run in the background; exit once the board is ready, or at once if it is running.
        #[arg(long)]
        detach: bool,
        /// Seconds --detach waits for ready; a board still starting is left running.
        #[arg(long, requires = "detach", default_value_t = 30)]
        wait: u64,
        /// Print the board's startup steps while --detach waits.
        #[arg(long, requires = "detach")]
        verbose: bool,
    },
    /// Show the state of the current board, participant or channel.
    #[command(after_help = examples::help(&["board status"]))]
    Status {
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Stop the selected run or all boards on this account.
    #[command(after_help = examples::help(&["board stop", "board stop --all"]))]
    Stop {
        #[arg(long)]
        run: Option<CreationId>,
        /// Stop every running board found on this account.
        #[arg(long, conflicts_with = "run")]
        all: bool,
    },
}

#[derive(Subcommand)]
enum Channel {
    /// Create this channel, or report the creation that is already there.
    #[command(after_help = examples::help(&["channel create"]))]
    Create { channel: ChannelName },
    /// Remove the named channel's selected creation.
    #[command(after_help = examples::help(&["channel remove"]))]
    Remove {
        channel: ChannelName,
        #[arg(long)]
        creation: Option<CreationId>,
    },
}

#[cfg(test)]
#[test]
fn every_cli_command_has_an_example_in_help() {
    fn check(command: &clap::Command) {
        assert!(
            command.get_after_help().is_some(),
            "{} has no help example",
            command.get_name()
        );
        for subcommand in command.get_subcommands() {
            check(subcommand);
        }
    }

    check(&Args::command());
}

pub(super) fn run() -> ExitCode {
    let argv: Vec<_> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|arg| arg == upgrade::FINISH) {
        return match upgrade::finish(&argv[2..]) {
            Ok(exit) => exit,
            Err(error) => {
                output::error(
                    "finish upgrading Malak",
                    &error.to_string(),
                    "Review the agent and board results above.",
                );
                ExitCode::from(3)
            }
        };
    }
    let command = Args::command();
    let invocation = help::bare_help(&command, &argv);
    let args = help::configure(command)
        .try_get_matches_from(invocation.as_deref().unwrap_or(&argv))
        .and_then(|mut matches| Args::from_arg_matches_mut(&mut matches));
    let args = match args {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.get(ContextKind::InvalidSubcommand),
                Some(ContextValue::String(name)) if name == "mcp"
            ) =>
        {
            output::error(
                "serve the Malak tools",
                "malak mcp ships in v0.2",
                "Use the Malak CLI or malak hook for local agents in v0.1.",
            );
            return ExitCode::from(4);
        }
        Err(error) => {
            let legacy = argv
                .iter()
                .skip(1)
                .take_while(|arg| *arg != "--")
                .any(|arg| arg == "--json" || arg == "--plain" || arg == "--agent");
            if error.use_stderr() {
                output::usage_error(&error, legacy);
                return ExitCode::from(2);
            }
            if error.kind() == ErrorKind::DisplayVersion {
                let _ = error.print();
            } else {
                let _ = output::help(&error);
            }
            return ExitCode::SUCCESS;
        }
    };
    let (operation, next) = args.diagnostic_context();
    let legacy = args.structured_output();
    let launcher_report =
        matches!(&args.command, Command::Participant(command) if command.is_launcher_report());
    let watches_link = matches!(
        &args.command,
        Command::Participant(participant::Command::Read { .. }) | Command::Mailbox { .. }
    );
    let read_to_ack = args.read_before_ack();
    match args.execute() {
        Ok(exit) => exit,
        Err(error) => {
            let mut next: &str = if error == Error::refused(participant::EMPTY_BODY) {
                "Give the message text as an argument or pass - to read it from standard input."
            } else if watches_link && client_transport::link_failed(&error) {
                client_transport::LINK_HINT
            } else {
                next
            };
            let mut reason = error.to_string();
            let read_next;
            if error == Error::refused(participant::NOT_GIVEN)
                && let Some(read) = read_to_ack
            {
                reason.push_str(
                    ". An ack covers only positions a read has given this reader. The mailbox \
                     watcher, --next and get show a message without giving it, and a board \
                     restart forgets what was given",
                );
                read_next = format!("Run `{read}`, then acknowledge the message again.");
                next = &read_next;
            }
            output::operation_error(&operation, &reason, next, legacy);
            ExitCode::from(match error.outcome() {
                Outcome::Done => 0,
                Outcome::Refused => 2,
                Outcome::Unavailable
                    if launcher_report && participant::report_connection_failed(&error) =>
                {
                    participant::REPORT_CONNECTION_EXIT
                }
                Outcome::Unavailable => 3,
                Outcome::NotSupported => 4,
            })
        }
    }
}

impl Args {
    fn structured_output(&self) -> bool {
        matches!(
            &self.command,
            Command::Board {
                command: Board::Status { json: true, .. } | Board::List { json: true }
            } | Command::Enrol { json: true, .. }
                | Command::Tail { json: true, .. }
                | Command::Mailbox { json: true, .. }
                | Command::Mailbox { plain: true, .. }
                | Command::Hook(hook::Args { json: true, .. })
                | Command::Participant(
                    participant::Command::Send { json: true, .. }
                        | participant::Command::Read { json: true, .. }
                        | participant::Command::Read { agent: true, .. }
                        | participant::Command::Get { json: true, .. }
                        | participant::Command::Get { agent: true, .. }
                )
        )
    }

    fn execute(mut self) -> Result<ExitCode, Error> {
        if matches!(&self.command, Command::Recipe(args) if args.is_demo() && !args.shows_help())
            && (self.board.is_some() || self.config.is_some())
        {
            eprintln!("The demo runs on its own board. Omit --board and --config.");
            return Ok(ExitCode::from(2));
        }
        if let Some(name) = self.board.take()
            && !matches!(&self.command, Command::Recipe(args) if args.shows_help())
        {
            if self.config.is_some() {
                return Err(Error::refused("use either --board or --config"));
            }
            self.config = Some(board_names::resolve(&name)?);
        }
        if let Some(path) = &self.config {
            board_version::set_config_path(path.clone());
        }
        let result = match self.command {
            Command::Board {
                command: Board::List { json },
            } => board_list::show(json),
            Command::Board {
                command:
                    Board::Start {
                        lead_handover,
                        lead_account,
                        detach,
                        wait,
                        verbose,
                    },
            } if detach => {
                let (config, path) = board_for_start(self.config)?;
                board::detach(
                    &path,
                    config,
                    lead_handover,
                    lead_account,
                    board::Wait {
                        limit: std::time::Duration::from_secs(wait),
                        verbose,
                    },
                )
            }
            Command::Board {
                command:
                    Board::Start {
                        lead_handover,
                        lead_account,
                        ..
                    },
            } => {
                let (config, _) = board_for_start(self.config)?;
                board::start(config, lead_handover, lead_account)
            }
            Command::Board {
                command: Board::Status { target, json },
            } => {
                let (mut client, version) = owner_with_version(self.config)?;
                owner_commands::show_status(&mut client, &version, target.as_deref(), json)
            }
            Command::Board {
                command: Board::Stop { all: true, .. },
            } => board_group::stop_all(self.config),
            Command::Board {
                command: Board::Stop { run, all: false },
            } => board_group::stop_selected(self.config, run),
            Command::Uninstall(options) => return uninstall::run(self.config, options),
            Command::Upgrade(options) => return upgrade::run(self.config, options),
            Command::Rekey {
                id,
                fingerprint,
                session,
                out,
            } => owner_commands::rekey(&mut owner(self.config)?, &id, fingerprint, &out, session),
            Command::Release { target } => {
                owner_commands::release(&mut owner(self.config)?, &target)
            }
            Command::Revoke { target } => owner_commands::revoke(&mut owner(self.config)?, &target),
            Command::Channel { command } => {
                owner_commands::channel(&mut owner(self.config)?, command)
            }
            Command::Trim {
                channel,
                position,
                creation,
            } => owner_commands::trim(&mut owner(self.config)?, &channel, position, creation),
            Command::Tail { channel, json } => {
                owner_commands::tail(&mut owner(self.config)?, channel.as_ref(), json)
            }
            Command::Mailbox {
                trace: Some(_),
                plain: true,
                json: true,
                ..
            } => Err(Error::refused("--trace accepts either --plain or --json")),
            Command::Mailbox {
                trace: Some(trace),
                plain,
                json,
                ..
            } => mailbox::trace(self.config, trace, plain, json),
            Command::Mailbox {
                orders: true,
                json: true,
                ..
            } => Err(Error::refused(
                "--orders does not support --json. Use malak mailbox --questions --json",
            )),
            Command::Mailbox {
                waiting: true,
                name,
                json,
                ..
            } => mailbox::waiting(self.config, name, json),
            Command::Mailbox {
                next: true,
                name: Some(name),
                plain,
                timeout,
                ..
            } => {
                if name == "lead" {
                    if !plain {
                        return Err(Error::refused("--for lead --next requires --plain"));
                    }
                    mailbox::next(self.config, timeout)
                } else {
                    mailbox::worker_next(self.config, &name, timeout)
                }
            }
            Command::Mailbox {
                next: true,
                name: None,
                ..
            } => Err(Error::refused("--next requires --for NAME")),
            Command::Mailbox {
                name,
                plain,
                who,
                questions,
                json,
                orders,
                waiting: false,
                trace: None,
                next: false,
                ..
            } => mailbox::run(self.config, name, plain, who, questions, json, orders),
            Command::Enrol {
                name,
                grants,
                only_from,
                account,
                session,
                unbound: _,
                launch_program,
                out,
                json,
            } => {
                let account = account.as_deref().map(user_id).transpose()?;
                let only_user = only_from.as_deref().map(user_id).transpose()?;
                let grant = grant(&grants)?;
                let path = std::path::absolute(out).map_err(config::io_error)?;
                let path = path
                    .to_str()
                    .ok_or_else(|| Error::refused("output path must be UTF-8"))?;
                let mut client = owner(self.config)?;
                let (id, path) = if let Some(launch) = session.zip(launch_program.as_deref()) {
                    client
                        .enrol_starting_session(&name, &grant, path, account, only_user, launch)?
                } else {
                    client.enrol_with_session(&name, &grant, path, account, only_user, session)?
                };
                if json {
                    let receipt = serde_json::json!({ "id": id.as_str(), "handover": path });
                    output::line(format_args!("{receipt}"))
                } else {
                    output::details(
                        &format!("Participant {name} enrolled."),
                        &[
                            ("ID", id.as_str()),
                            ("Inbox", &format!("to/{id}")),
                            ("Handover", &path),
                        ],
                    )
                }
            }
            Command::Grant { id, grants } => {
                owner(self.config)?.replace_grant(&id, &grant(&grants)?)?;
                let rules = if grants.is_empty() {
                    "No channel access".into()
                } else {
                    grants.join("  ")
                };
                output::details(
                    &format!("Grant replaced for participant {id}."),
                    &[("Allowed", &rules)],
                )
            }
            Command::Recover {
                work_factor,
                apply,
                owner_vouches_same_volume,
                withdraw,
            } => {
                let settings = config::Config::read(self.config)?.board;
                let limits = settings.limits()?;
                let mechanism = Some(settings.mechanism.as_deref().unwrap_or("key"));
                let (title, report) = if let Some(number) = withdraw {
                    (
                        "Recovery withdrawal",
                        malak::Owner::withdraw_recovery(
                            limits,
                            &settings.state,
                            mechanism,
                            &number,
                        )?,
                    )
                } else if let Some(number) = apply {
                    let apply = if owner_vouches_same_volume {
                        malak::Owner::apply_recovery_with_owner_confirmation
                    } else {
                        malak::Owner::apply_recovery
                    };
                    (
                        "Recovery progress",
                        apply(
                            settings.store(),
                            limits,
                            &settings.state,
                            mechanism,
                            &number,
                        )?,
                    )
                } else {
                    (
                        "Recovery plan",
                        malak::Owner::look_recovery(
                            settings.store(),
                            limits,
                            &settings.state,
                            mechanism,
                            work_factor.unwrap_or(64),
                        )?,
                    )
                };
                let plan = report.plan_number();
                let report = report.to_string();
                let detail = report
                    .strip_suffix(&format!("Plan {plan}"))
                    .unwrap_or(&report)
                    .trim_end();
                let (title, detail) = detail
                    .strip_prefix("No stopped channels.\n")
                    .map_or((title, detail), |remaining| {
                        ("No stopped channels.", remaining)
                    });
                let format = output::Format::stdout();
                let mut result = format.screen(title, "", &[("Plan", &plan)]);
                if !detail.is_empty() {
                    if format.interactive() {
                        format.section(&mut result, "Recovery");
                    } else {
                        result.push('\n');
                    }
                    format.paragraph(&mut result, "  ", detail);
                }
                output::write(&result)
            }
            Command::Hook(args) => return hook::run(args, self.config),
            Command::Recipe(args) => return recipe::run(args, self.config),
            Command::Participant(command) => command.execute(self.config),
        };
        result.map(|()| ExitCode::SUCCESS)
    }
}

fn board_for_start(config_path: Option<PathBuf>) -> Result<(config::Board, PathBuf), Error> {
    let (config, path, created) = config::read_for_start(config_path)?;
    if let Some(text) = created {
        output::diagnostic(&text, &[]);
    }
    if let Ok(credential) = OwnerCredential::read(&config.board.state) {
        board::check_owner_address(&config.board, &credential)?;
    }
    board_list::record(&path);
    Ok((config.board, path))
}

fn owner(config_path: Option<PathBuf>) -> Result<LocalClient<ClientTransport>, Error> {
    owner_with_version(config_path).map(|(client, _)| client)
}

fn owner_with_version(
    config_path: Option<PathBuf>,
) -> Result<(LocalClient<ClientTransport>, BoardVersion), Error> {
    let settings = config::Config::read(config_path)?;
    owner_for_board(&settings.board)
}

fn owner_for_board(
    settings: &config::Board,
) -> Result<(LocalClient<ClientTransport>, BoardVersion), Error> {
    let (client, version, _) = owner_for_board_quiet(settings)?;
    version.warn();
    Ok((client, version))
}

fn owner_for_board_quiet(
    settings: &config::Board,
) -> Result<
    (
        LocalClient<ClientTransport>,
        BoardVersion,
        std::sync::Arc<ClientTransport>,
    ),
    Error,
> {
    let credential = owner_credential(settings)?;
    board::check_owner_address(settings, &credential)?;
    ClientTransport::connect_quiet(|transport| LocalClient::owner(transport, credential))
}

fn owner_credential(board: &config::Board) -> Result<OwnerCredential, Error> {
    OwnerCredential::read(&board.state).map_err(|_| {
        Error::refused(format!(
            "no owner credential in {}; {}",
            board.state.display(),
            config::START_HINT
        ))
    })
}

fn user_id(value: &str) -> Result<u32, Error> {
    if let Ok(uid) = value.parse() {
        return Ok(uid);
    }
    User::from_name(value)
        .map_err(|error| Error::unavailable(format!("could not look up account {value}: {error}")))?
        .map(|user| user.uid.as_raw())
        .ok_or_else(|| Error::refused(format!("local account {value} does not exist")))
}

fn grant(rules: &[String]) -> Result<Grant, Error> {
    rules
        .iter()
        .map(String::as_str)
        .try_fold(Grant::default(), add_rule)
}

fn add_rule(mut grant: Grant, rule: &str) -> Result<Grant, Error> {
    let (pattern, actions) = rule
        .split_once(':')
        .ok_or_else(|| Error::refused("grant needs PATTERN:ACTIONS"))?;
    let actions = actions
        .split(',')
        .map(|action| match action {
            "send" => Ok(Action::Send),
            "read" => Ok(Action::Read),
            "get" => Ok(Action::Get),
            "ack" => Ok(Action::Ack),
            "create" => Ok(Action::Create),
            _ => Err(Error::refused("invalid grant action")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    grant.allow(pattern.parse()?, actions);
    Ok(grant)
}

impl Args {
    /// The read that gives the reader the position an `ack` named.
    fn read_before_ack(&self) -> Option<String> {
        let Command::Participant(participant::Command::Ack { channel, .. }) = &self.command else {
            return None;
        };
        let quoted = |text: &str| {
            let plain = |c: char| c.is_ascii_alphanumeric() || "-_./:@".contains(c);
            if !text.is_empty() && text.chars().all(plain) {
                text.to_owned()
            } else {
                format!("'{}'", text.replace('\'', "'\\''"))
            }
        };
        let board = match (&self.board, &self.config) {
            (Some(name), _) => format!("--board {} ", quoted(name)),
            (None, Some(path)) => format!("--config {} ", quoted(&path.display().to_string())),
            (None, None) => String::new(),
        };
        Some(format!("malak {board}read {channel}"))
    }

    fn diagnostic_context(&self) -> (String, &'static str) {
        let board = self
            .config
            .as_ref()
            .map_or_else(|| "the board".to_owned(), |path| path.display().to_string());
        let check_board = "Use malak board status with the same configuration to check the board.";
        let check_participant =
            "Use malak board status with the participant ID to check its current enrolment.";
        match &self.command {
            Command::Board { command } => match command {
                Board::List { .. } => (
                    "list local boards".to_owned(),
                    "Check access to your home folder and board configurations.",
                ),
                Board::Start { .. } => (
                    format!("start {board}"),
                    "Check the board configuration and any existing board.log.",
                ),
                Board::Stop { all: true, .. } => (
                    "stop all boards".to_owned(),
                    "Check the reason shown for each running board.",
                ),
                Board::Stop { run, all: false } => (
                    run.map_or_else(
                        || format!("stop {board}"),
                        |run| format!("stop board run {run}"),
                    ),
                    check_board,
                ),
                Board::Status { target, .. } => (
                    target.as_ref().map_or_else(
                        || format!("show status for {board}"),
                        |target| format!("show status for {target}"),
                    ),
                    "Check the board configuration and its owner access.",
                ),
            },
            Command::Uninstall(_) => (
                "uninstall Malak".to_owned(),
                "Review the output or run malak uninstall --help.",
            ),
            Command::Upgrade(_) => (
                "upgrade Malak".to_owned(),
                "Review the output or run malak upgrade --help.",
            ),
            Command::Enrol { name, out, .. } => (
                format!("enrol {name} with handover at {}", out.display()),
                "Check the name, grant and destination. Use a new private handover path.",
            ),
            Command::Grant { id, .. } => (
                format!("replace the grant for {id}"),
                "Check the participant ID and use PATTERN:ACTIONS with send, read, get, ack or create.",
            ),
            Command::Rekey { id, out, .. } => (
                format!("replace the credential for {id} at {}", out.display()),
                check_participant,
            ),
            Command::Release { target } => (
                format!("release the session claim for {target}"),
                "Check the participant's claim with malak recipe workers NAME.",
            ),
            Command::Revoke { target } => (
                format!("revoke access for {target}"),
                "Check the participant ID or active name with malak board status.",
            ),
            Command::Channel { command } => match command {
                Channel::Create { channel } => (format!("create channel {channel}"), check_board),
                Channel::Remove { channel, .. } => (
                    format!("remove channel {channel}"),
                    "Run malak board status with this channel to check its current creation.",
                ),
            },
            Command::Trim {
                channel, position, ..
            } => (
                format!("trim {channel} before #{position}"),
                "Check the channel creation and positions with malak board status.",
            ),
            Command::Tail { channel, .. } => (
                channel.as_ref().map_or_else(
                    || format!("follow events on {board}"),
                    |channel| format!("follow events for {channel}"),
                ),
                check_board,
            ),
            Command::Mailbox {
                name,
                questions,
                orders,
                who,
                waiting,
                trace,
                ..
            } => {
                let view = if trace.is_some() {
                    "trace history"
                } else if *questions {
                    "open questions"
                } else if *orders {
                    "orders"
                } else if *who {
                    "participants"
                } else if *waiting {
                    "waiting inboxes"
                } else {
                    "mailbox activity"
                };
                let target = name.as_deref().unwrap_or(&board);
                (
                    format!("show {view} for {target}"),
                    "Check the board and participant with malak board status, then reopen the view.",
                )
            }
            Command::Recover {
                apply, withdraw, ..
            } => {
                let action = if let Some(number) = apply {
                    format!("apply recovery plan {number}")
                } else if let Some(number) = withdraw {
                    format!("withdraw recovery plan {number}")
                } else {
                    format!("look for damage on {board}")
                };
                (
                    action,
                    "For a Markdown board, stop the board before using malak recover.",
                )
            }
            Command::Hook(_) => (
                "run the Malak hook".into(),
                "Run malak hook --describe to check the hook setup.",
            ),
            Command::Recipe(args) => (
                args.operation(),
                "Run malak recipe to list the shipped recipes.",
            ),
            Command::Participant(command) => match command {
                participant::Command::Send { channel, .. } => (
                    format!("send to {channel}"),
                    "Check the channel, message arguments and send access. Reuse the same message ID if you retry an uncertain send.",
                ),
                participant::Command::Read { channels, .. } => (
                    format!("read {}", channels.join(", ")),
                    "Check the handover and ask the lead to verify your read access.",
                ),
                participant::Command::Get {
                    channel,
                    position,
                    offset,
                    ..
                } => {
                    if let Some(offset) = offset {
                        (
                            format!("display {channel} #{position} at byte {offset}"),
                            "Use the fetch offset from the previous result, or omit --offset.",
                        )
                    } else {
                        (
                            format!("get {channel} #{position}"),
                            "Check the channel, position and get access.",
                        )
                    }
                }
                participant::Command::Ack {
                    channel, position, ..
                } => (
                    format!("acknowledge {channel} #{position}"),
                    "Check the creation and position on the original message and confirm your acknowledge access.",
                ),
            },
        }
    }
}
