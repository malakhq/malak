//! List the open questions on a board's `questions` channel.
//!
//! Answers are found in the stored messages, so the list holds across restarts.

use super::{
    messages::{RecordBody, read_message, readable_channels},
    model::{Address, Audience, Channel, Preview, Question, QuestionKind},
    now_ms, status, text,
};
use crate::cli::{output, owner, participant::Session};
use malak::Error;
use std::path::PathBuf;
use std::{borrow::Borrow, cmp::Reverse, collections::HashMap};

pub(super) const CHANNEL: &str = "questions";
pub(super) const INBOX_HINT: &str = "Answers arrive in your inbox";
const LEAD: &str = "lead";
pub(super) const OWNER_HEADING: &str = "For the owner";

pub(super) fn run(config: Option<PathBuf>, plain: bool, json: bool) -> Result<(), Error> {
    let status = status(&mut owner(config.clone())?, false)?;
    let Some((mut session, id)) = Session::open_preview(config) else {
        return Err(Error::unavailable("no handover to read the board with"));
    };
    let viewer = status
        .participants
        .iter()
        .find(|person| person.id == id.as_str())
        .ok_or_else(|| Error::refused("the handover does not name an enrolled participant"))?;
    let readable = readable_channels(status.channels.iter(), viewer).collect::<Vec<_>>();
    let body = if plain || json {
        RecordBody::Preview
    } else {
        RecordBody::Full
    };
    let mut asked = open(&readable, &mut session, body)?;
    if json {
        return output::line(format_args!("{}", serde_json::json!(asked)));
    }
    if !plain {
        asked.sort_unstable_by_key(|question| (!question.for_owner(), Reverse(question.order())));
    }
    let names: HashMap<_, _> = status
        .participants
        .iter()
        .map(|person| (person.id.clone(), person.name.clone()))
        .collect();
    print_open(&asked, &names, now_ms(), plain)
}

/// The questions on the `questions` channel that the lead has not answered.
pub(super) fn open(
    readable: &[&Channel],
    session: &mut Session,
    body: RecordBody,
) -> Result<Vec<Question>, Error> {
    collect(readable, |address| {
        let text = if address.channel == CHANNEL {
            body
        } else {
            RecordBody::Omit
        };
        read_message(session, address, text)
    })
}

pub(super) fn collect<T: Borrow<Preview>, E>(
    readable: &[&Channel],
    mut read: impl FnMut(&Address) -> Result<Option<T>, E>,
) -> Result<Vec<Question>, E> {
    let Some(questions) = readable
        .iter()
        .find(|channel| channel.name == CHANNEL && channel.floor < channel.head)
    else {
        return Ok(Vec::new());
    };
    let mut asked = Vec::new();
    let mut answers = HashMap::new();
    for channel in readable {
        for address in channel.addresses() {
            let Some(message) = read(&address)? else {
                continue;
            };
            let message = message.borrow();
            if message.sender == LEAD && message.has_reply_to {
                if let Some(target) = message.reply_to.as_ref().filter(|target| {
                    target.channel == CHANNEL && target.creation == questions.creation
                }) {
                    // Same-channel replies have already been checked for position order.
                    let on_questions = channel.name == CHANNEL;
                    *answers.entry(target.position).or_insert(false) |= on_questions;
                }
            } else if channel.name == CHANNEL {
                let kind = if message.sender == LEAD {
                    QuestionKind::Owner
                } else {
                    QuestionKind::Agent
                };
                asked.push(question(address, message, kind));
            }
        }
    }
    asked.retain(|question| {
        !answers
            .get(&question.position)
            .is_some_and(|&on_questions| question.kind == QuestionKind::Agent || on_questions)
    });
    asked.sort_by_key(|question| !question.for_owner());
    Ok(asked)
}

fn question(address: Address, message: &Preview, kind: QuestionKind) -> Question {
    let first_line = message
        .text
        .as_deref()
        .and_then(|text| text.lines().next())
        .unwrap_or_default()
        .to_owned();
    let body = message
        .body_text()
        .or(message.text.as_deref())
        .map_or_else(String::new, |body| text::preview(body.lines()).join("\n"));
    let named = message.text.as_deref().and_then(Audience::of_body);
    Question {
        position: address.position,
        creation: address.creation,
        sender: message.sender.clone(),
        at_ms: message.at_ms(),
        sent_at: message.sent_at,
        kind,
        // An older question names none and reads as the owner's.
        audience: named.unwrap_or(Audience::Owner),
        named: named.is_some(),
        first_line,
        body,
    }
}

fn print_open(
    questions: &[Question],
    names: &HashMap<String, String>,
    now: u64,
    plain: bool,
) -> Result<(), Error> {
    if questions.is_empty() {
        return output::result("No open questions.");
    }
    let (owner, agent) = questions.split_at(questions.partition_point(|q| q.for_owner()));
    let format = output::Format::stdout();
    let mut block = String::new();
    for (heading, group) in [(OWNER_HEADING, owner), ("Open questions", agent)] {
        if !group.is_empty() {
            block.push_str(&format.details(heading, &[]));
            if plain {
                let rows = question_rows(group, names, now);
                block.push_str(&format.table(&["Position", "Asked", "From", "Question"], &rows));
            } else {
                let width = text::terminal_width();
                for question in group {
                    let sender = sender_name(question, names);
                    let age = output::age(question.at_ms, now);
                    let title = format!("#{} · {sender} · {age}", question.position);
                    let address = text::reply_address(
                        CHANNEL,
                        &question.creation,
                        question.position,
                        text::item_width(width),
                    );
                    let lines = address.lines().chain(question.body.lines());
                    text::item(&mut block, width, &title, lines);
                }
            }
        }
    }
    output::write(&block)
}

fn question_rows(
    questions: &[Question],
    names: &HashMap<String, String>,
    now: u64,
) -> Vec<Vec<String>> {
    questions
        .iter()
        .map(|question| {
            vec![
                format!("#{}", question.position),
                output::age(question.at_ms, now),
                sender_name(question, names),
                question.first_line.clone(),
            ]
        })
        .collect()
}

fn sender_name(question: &Question, names: &HashMap<String, String>) -> String {
    names
        .get(&question.sender)
        .cloned()
        .unwrap_or_else(|| output::short_id(output::sender_name_or_id(&question.sender)))
}
