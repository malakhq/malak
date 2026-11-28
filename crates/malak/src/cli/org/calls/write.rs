//! Call writers use only the invoking lead's participant connection.

use super::{Link, load, names, top};
use crate::cli::{
    config::io_error,
    org::{records, seats::Reader, typed},
    output,
    participant::{self, Session},
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use malak::{Error, Kind, Limits};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{ffi::OsString, io::Read, path::PathBuf, process::ExitCode};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::cli) enum Action {
    Raise {
        position: u64,
        level: String,
        topic: Option<String>,
    },
    Level {
        question: Link,
        level: String,
        topic: Option<String>,
    },
    Merge {
        questions: Vec<Link>,
    },
    Decide {
        question: Link,
        body: String,
    },
    Relay {
        question: Link,
        body: String,
    },
}

#[derive(Parser)]
#[command(name="malak recipe calls", about="Raise, level, merge and decide calls.", after_help=crate::cli::examples::help(&["recipe calls raise", "recipe calls level", "recipe calls merge", "recipe calls decide", "recipe calls relay"]))]
struct Recipe {
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "board")]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_name = "NAME")]
    board: Option<String>,
    #[command(subcommand)]
    action: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Propose a call's level as the asking board's lead.
    #[command(after_help=crate::cli::examples::help(&["recipe calls raise"]))]
    Raise {
        position: u64,
        /// Level name (default blue, orange, red; configurable in calls.levels).
        #[arg(long)]
        level: String,
        #[arg(long)]
        topic: Option<String>,
    },
    /// Set the effective level as the top board's lead.
    #[command(after_help=crate::cli::examples::help(&["recipe calls level"]))]
    Level {
        target: String,
        position: u64,
        /// Level name (default blue, orange, red; configurable in calls.levels).
        #[arg(long)]
        level: String,
        #[arg(long)]
        topic: Option<String>,
    },
    /// Merge questions. Each LINK is a JSON array of board, channel, creation, position.
    #[command(after_help=crate::cli::examples::help(&["recipe calls merge"]))]
    Merge {
        #[arg(required=true, num_args=2..)]
        links: Vec<String>,
    },
    /// Decide a call as top lead; answer text on standard input.
    #[command(after_help=crate::cli::examples::help(&["recipe calls decide"]))]
    Decide { target: String, position: u64 },
    /// Relay the owner's words as top lead; text on standard input.
    #[command(after_help=crate::cli::examples::help(&["recipe calls relay"]))]
    Relay { target: String, position: u64 },
}

fn parse<P: Parser + CommandFactory + FromArgMatches>(
    args: &[OsString],
    name: &str,
) -> Result<Result<P, ExitCode>, Error> {
    let words: Vec<_> = std::iter::once(OsString::from(name))
        .chain(args.iter().cloned())
        .collect();
    let command = crate::cli::help::configure(P::command());
    let words = crate::cli::help::bare_help(&command, &words).unwrap_or(words);
    match command
        .try_get_matches_from(words)
        .and_then(|matches| P::from_arg_matches(&matches))
    {
        Ok(parsed) => Ok(Ok(parsed)),
        Err(error) => {
            let help = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            error.print().map_err(io_error)?;
            Ok(Err(ExitCode::from(if help { 0 } else { 2 })))
        }
    }
}

pub(in crate::cli) fn recipe(
    args: &[OsString],
    config: Option<PathBuf>,
) -> Result<ExitCode, Error> {
    let parsed = match parse::<Recipe>(args, "malak recipe calls")? {
        Ok(parsed) => parsed,
        Err(exit) => return Ok(exit),
    };
    if config.is_some() && (parsed.config.is_some() || parsed.board.is_some()) {
        return Err(Error::refused("choose a board only once"));
    }
    let config = parsed
        .board
        .map(|board| crate::cli::board_names::resolve(&board))
        .transpose()?
        .or(parsed.config)
        .or(config);
    let reader = writer(config.clone())?;
    let action = match parsed.action {
        Command::Raise {
            position,
            level,
            topic,
        } => Action::Raise {
            position,
            level,
            topic,
        },
        Command::Level {
            target,
            position,
            level,
            topic,
        } => Action::Level {
            question: question(&reader, &target, position)?.0,
            level,
            topic,
        },
        Command::Merge { links } => Action::Merge {
            questions: links
                .iter()
                .map(|link| {
                    Link::parse(link).ok_or_else(|| {
                        Error::refused("each LINK must be [board,channel,creation,position]")
                    })
                })
                .collect::<Result<_, _>>()?,
        },
        Command::Decide { target, position } => Action::Decide {
            question: question(&reader, &target, position)?.0,
            body: stdin()?,
        },
        Command::Relay { target, position } => Action::Relay {
            question: question(&reader, &target, position)?.0,
            body: stdin()?,
        },
    };
    let receipt = write(config, action, None)?;
    output::line(format_args!("{receipt}"))?;
    Ok(ExitCode::SUCCESS)
}

fn stdin() -> Result<String, Error> {
    let mut text = String::new();
    std::io::stdin()
        .take((Limits::default().body_bytes + 1) as u64)
        .read_to_string(&mut text)
        .map_err(io_error)?;
    if text.trim().is_empty() {
        return Err(Error::refused("give the decision text on standard input"));
    }
    Ok(text)
}

fn writer(config: Option<PathBuf>) -> Result<Reader, Error> {
    if !participant::preview_handover(config.clone())?
        .id()
        .is_lead()
    {
        return Err(Error::refused("only the board's lead may write calls"));
    }
    // Opening the selected board first prevents a foreign selector from
    // silently turning a write into a write on the caller's home board.
    Session::open(config.clone())?;
    Reader::open(config)
}

fn chief(reader: &Reader) -> Result<(), Error> {
    if !reader.chief {
        return Err(Error::refused(
            "only the top board's lead may set an effective level or decide a call",
        ));
    }
    Ok(())
}

fn question(reader: &Reader, board: &str, position: u64) -> Result<(Link, Value), Error> {
    let mut seat = reader.open_board(Some(board))?;
    let record = records::at(&seat.config, &mut seat.session, "questions", position)?
        .filter(|record| {
            record["grade"] == "proven"
                && record["kind"] == "note"
                && record["headers"]["reply_to"][0] != "questions"
        })
        .ok_or_else(|| Error::refused("the link must name a proven question"))?;
    Ok((
        Link::of(board, &record).expect("retained record has an address"),
        record,
    ))
}

fn checked(reader: &Reader, link: &Link) -> Result<Value, Error> {
    if !link.question() {
        return Err(Error::refused("calls must link questions"));
    }
    let (actual, record) = question(reader, &link.0, link.3)?;
    if actual != *link {
        return Err(Error::refused("the question creation changed"));
    }
    Ok(record)
}

pub(in crate::cli) fn send(
    session: &mut Session,
    channel: &str,
    body: String,
    reply: Option<&Link>,
    id: Option<String>,
) -> Result<Value, Error> {
    if body.len() > Limits::default().body_bytes {
        return Err(Error::refused("call body exceeds the message byte limit"));
    }
    let (_, value) = participant::Command::Send {
        channel: channel.into(),
        id,
        kind: Kind::Note,
        reply_to: reply.map_or_else(Vec::new, |link| {
            vec![link.1.clone(), link.2.clone(), link.3.to_string()]
        }),
        content_type: None,
        max_bytes: None,
        trace: None,
        json: true,
        body: None,
    }
    .execute_reply(session, Some(body.into_bytes()))?;
    Ok(value)
}

fn encoded(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("links serialize")
}

pub(in crate::cli) fn rail(
    session: &mut Session,
    event: &str,
    subject: &str,
    link: &Link,
    id: Option<&str>,
) -> Result<(), Error> {
    let subject = subject.lines().next().unwrap_or("Question");
    send(
        session,
        "rail",
        format!(
            "Type: rail\nEvent: {event}\nSubject: {subject}\nLink: {}\n",
            encoded(link)
        ),
        None,
        id.map(|id| format!("{id}-{event}")),
    )?;
    Ok(())
}

pub(in crate::cli) fn write(
    config: Option<PathBuf>,
    action: Action,
    id: Option<String>,
) -> Result<Value, Error> {
    let reader = writer(config)?;
    let mut seat = reader.open_board(None)?;
    let mut blocked = None;
    let body = match action {
        Action::Raise {
            position,
            level,
            topic,
        } => {
            let (link, record) = question(&reader, &reader.home_board, position)?;
            blocked = Some((link.clone(), typed::text(&record)));
            level_body(
                &link,
                &level,
                topic.as_deref(),
                &super::settings::for_board(&reader, &link.0)?,
            )?
        }
        Action::Level {
            question,
            level,
            topic,
        } => {
            chief(&reader)?;
            checked(&reader, &question)?;
            level_body(
                &question,
                &level,
                topic.as_deref(),
                &super::settings::for_board(&reader, &question.0)?,
            )?
        }
        Action::Merge { questions } => {
            if questions.len() < 2 {
                return Err(Error::refused("merge needs at least two question links"));
            }
            for link in &questions {
                if !reader.chief && link.0 != reader.home_board {
                    return Err(Error::refused(
                        "a lead may merge only its own board's calls",
                    ));
                }
                checked(&reader, link)?;
            }
            format!("Type: {}\nLinks: {}\n", names::MERGE, encoded(&questions))
        }
        Action::Decide { question, body } => {
            decision_body(&reader, &question, names::DECISION, names::TOP, &body)?
        }
        Action::Relay { question, body } => decision_body(
            &reader,
            &question,
            names::DECISION,
            names::OWNER_RELAY,
            &body,
        )?,
    };
    let receipt = send(&mut seat.session, names::CHANNEL, body, None, id.clone())?;
    if let Some((link, subject)) = blocked {
        rail(&mut seat.session, "blocked", &subject, &link, id.as_deref())?;
    }
    Ok(receipt)
}

fn level_body(
    link: &Link,
    level: &str,
    topic: Option<&str>,
    settings: &super::settings::Settings,
) -> Result<String, Error> {
    if !settings.known(level) {
        return Err(Error::refused(
            "level must be a name in calls.levels (default blue, orange, red; configurable)",
        ));
    }
    if topic.is_some_and(|topic| topic.is_empty() || topic.contains(['\r', '\n'])) {
        return Err(Error::refused("topic must be one nonempty line"));
    }
    Ok(format!(
        "Type: {}\nLink: {}\nLevel: {level}\n{}",
        names::LEVEL,
        encoded(link),
        topic.map_or_else(String::new, |topic| format!("Topic: {topic}\n"))
    ))
}

fn decision_body(
    reader: &Reader,
    question: &Link,
    kind: &str,
    by: &str,
    body: &str,
) -> Result<String, Error> {
    chief(reader)?;
    checked(reader, question)?;
    if body.trim().is_empty() {
        return Err(Error::refused("decision text is empty"));
    }
    Ok(format!(
        "Type: {kind}\nLink: {}\nDecided by: {by}\n\n{body}",
        encoded(question)
    ))
}

#[derive(Parser)]
struct RecordRecipe {
    #[arg(long, num_args = 4, required = true)]
    decision: Vec<String>,
    #[arg(num_args=1..=2,required=true)]
    address: Vec<String>,
}

pub(in crate::cli) fn record_recipe(
    args: &[OsString],
    config: Option<PathBuf>,
) -> Result<ExitCode, Error> {
    let parsed = match parse::<RecordRecipe>(args, "malak recipe answer")? {
        Ok(parsed) => parsed,
        Err(exit) => return Ok(exit),
    };
    let position = parsed
        .address
        .last()
        .and_then(|position| position.parse::<u64>().ok())
        .ok_or_else(|| Error::refused("give the question position"))?;
    if parsed.address.len() == 2 && parsed.address[0] != "questions" {
        return Err(Error::refused(
            "a decision may be recorded only for questions",
        ));
    }
    let link = Link(
        parsed.decision[0].clone(),
        parsed.decision[1].clone(),
        parsed.decision[2].clone(),
        parsed.decision[3]
            .parse()
            .map_err(|_| Error::refused("decision position must be positive"))?,
    );
    let receipt = record(config, position, &link, None)?;
    output::line(format_args!("Answered questions #{position}.\n{receipt}"))?;
    Ok(ExitCode::SUCCESS)
}

pub(in crate::cli) fn record(
    config: Option<PathBuf>,
    position: u64,
    decision: &Link,
    id: Option<String>,
) -> Result<Value, Error> {
    let reader = writer(config)?;
    if top(&reader).as_deref() != Some(&decision.0) || decision.1 != "calls" {
        return Err(Error::refused("decision must link the top board's calls"));
    }
    let (question, original) = question(&reader, &reader.home_board, position)?;
    let projection = load(&reader)?;
    let note = projection
        .decision_for(&question, decision)
        .ok_or_else(|| {
            Error::refused(
                "decision is not a proven top-board decision for this question or a proven merge",
            )
        })?;
    let by = if note["decided_by"] == names::TOP {
        names::TOP_RECORDED
    } else {
        names::OWNER_RECORDED
    };
    let body = format!(
        "Decided by: {by}\nDecision: {}\n\n{}",
        encoded(decision),
        note["body"].as_str().unwrap_or("")
    );
    let mut seat = reader.open_board(None)?;
    let receipt = send(
        &mut seat.session,
        "questions",
        body.clone(),
        Some(&question),
        id.clone(),
    )?;
    let sender = original["sender"].as_str().unwrap_or("lead");
    if sender != "lead" {
        send(
            &mut seat.session,
            &format!("to/{sender}"),
            body,
            Some(&question),
            id.clone(),
        )?;
    }
    let answer = Link(
        reader.home_board,
        "questions".into(),
        receipt["creation"].as_str().unwrap_or("").into(),
        receipt["position"].as_u64().unwrap_or(0),
    );
    rail(
        &mut seat.session,
        "decided",
        &typed::text(&original),
        &answer,
        id.as_deref(),
    )?;
    rail(
        &mut seat.session,
        "unblocked",
        &typed::text(&original),
        &answer,
        id.as_deref(),
    )?;
    Ok(json!({"recorded":receipt,"decision":decision}))
}
