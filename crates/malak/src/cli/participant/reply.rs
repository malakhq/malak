//! Render participant replies for humans, agents and structured callers.

use super::{field, invalid_reply, number_field, random_number, trace_header};
use crate::cli::{
    config::io_error,
    output::{self, line},
    presentation,
};
use malak::Error;
use serde_json::{Map, Value};
use std::io::{self, Write};

pub(super) enum Output {
    Quiet,
    Collected(Vec<super::GuestRecord>),
    Receipt {
        id: String,
        channel: String,
        kind: &'static str,
        json: bool,
    },
    Ack {
        channel: String,
        creation: String,
        position: u64,
    },
    Messages {
        json: bool,
        /// Channel, creation and position of each message printed so far.
        printed: Vec<(String, String, u64)>,
    },
    Agent {
        json: bool,
        offset: usize,
        messages: Vec<presentation::Message>,
    },
}

impl Output {
    pub(super) fn messages(json: bool, agent: bool, offset: usize) -> Self {
        if agent {
            Self::Agent {
                json,
                offset,
                messages: Vec::new(),
            }
        } else {
            Self::Messages {
                json,
                printed: Vec::new(),
            }
        }
    }

    pub(super) fn ok(self, header: &Map<String, Value>, shown: bool) -> Result<(), Error> {
        match self {
            Self::Receipt {
                id,
                channel,
                kind,
                json,
            } => {
                let creation = field(header, "creation")?;
                let position = number_field(header, "position")?;
                let durability = field(header, "durability")?;
                let duplicate = header
                    .get("duplicate")
                    .and_then(Value::as_bool)
                    .ok_or_else(invalid_reply)?;
                let trace = header
                    .get("trace")
                    .map(|trace| trace.as_str().ok_or_else(invalid_reply))
                    .transpose()?;
                if json {
                    let mut receipt = serde_json::json!({ "id": id, "creation": creation,
                        "position": position, "durability": durability, "duplicate": duplicate });
                    if let Some(trace) = trace {
                        receipt["trace"] = Value::String(trace.to_owned());
                    }
                    return line(format_args!("{receipt}"));
                }
                let action = if duplicate {
                    "already recorded in"
                } else {
                    "sent to"
                };
                let mut details = vec![
                    ("Message ID", id.as_str()),
                    ("Creation", creation),
                    ("Durability", output::durability(durability)),
                ];
                if let Some(trace) = trace {
                    details.push(("Trace", trace));
                }
                output::receipt(
                    &format!("{} {action} {channel} #{position}.", kind_label(kind)),
                    &details,
                )
            }
            Self::Ack {
                channel,
                creation,
                position,
            } => output::details(
                &format!("Acknowledged {channel} through #{position}."),
                &[("Creation", &creation)],
            ),
            Self::Messages { json, .. } => {
                if let Some(result) = header.get("result").and_then(Value::as_str) {
                    return if result == "message" {
                        Ok(())
                    } else if json {
                        line(format_args!(
                            "{}",
                            Value::Object(json_reply(header.clone()))
                        ))
                    } else {
                        get_result(header, result)
                    };
                }
                if json {
                    let summary = read_json_summary(header)?;
                    return line(format_args!("{summary}"));
                }
                read_result(header, shown)
            }
            Self::Agent {
                json,
                offset,
                messages,
            } => {
                if json {
                    let mut envelope = match header.get("result").and_then(Value::as_str) {
                        Some("message") => serde_json::json!({}),
                        Some(_) => Value::Object(json_reply(header.clone())),
                        None => read_json_summary(header)?,
                    };
                    envelope["messages"] = Value::Array(Vec::new());
                    let budget = presentation::BUDGET
                        .checked_sub(envelope.to_string().len() + 1)
                        .ok_or_else(invalid_reply)?;
                    let shown = presentation::structured(&messages, offset, budget)?;
                    envelope["messages"] = Value::Array(shown.values);
                    line(format_args!("{envelope}"))
                } else {
                    let shown = agent_text(&messages, offset)?;
                    io::stdout()
                        .lock()
                        .write_all(shown.values.concat().as_bytes())
                        .map_err(io_error)
                }
            }
            Self::Quiet => Ok(()),
            Self::Collected(_) => Err(invalid_reply()),
        }
    }

    /// The tool reply: the text a model reads, and the same facts as JSON.
    pub(super) fn tool_result(self, header: Map<String, Value>) -> Result<(String, Value), Error> {
        let mut header = json_reply(header);
        match self {
            Self::Quiet => Ok(("Done.".to_owned(), serde_json::json!({ "done": true }))),
            Self::Ack {
                channel, position, ..
            } => Ok((
                format!("Acknowledged {channel} through #{position}."),
                serde_json::json!({ "done": true }),
            )),
            Self::Receipt {
                id, channel, kind, ..
            } => {
                let position = number_field(&header, "position")?;
                let text = format!("{} sent to {channel} #{position}.", kind_label(kind));
                header.insert("id".into(), Value::String(id));
                Ok((text, Value::Object(header)))
            }
            Self::Agent {
                offset, messages, ..
            } => {
                // The text is what `read --agent` prints. The JSON carries the same
                // messages, cut to the ones the text also shows.
                let text = agent_text(&messages, offset)?.values;
                let shown =
                    presentation::structured(&messages, offset, presentation::BUDGET / 2 - 512)?;
                let count = text.len().min(shown.values.len());
                header.remove("channels");
                header.insert(
                    "messages".into(),
                    Value::Array(shown.values.into_iter().take(count).collect()),
                );
                let mut text = if count == 0 {
                    "No messages.".to_owned()
                } else {
                    text[..count].concat()
                };
                // The board gave more than fit in the reply. They stay due.
                let cut = messages.len() - count;
                if cut > 0 {
                    text.push_str(&format!(
                        "\n{cut} more message(s) did not fit in this reply; read again for them.\n"
                    ));
                    header.insert("held_back".into(), Value::Bool(true));
                }
                Ok((text, Value::Object(header)))
            }
            Self::Messages { .. } | Self::Collected(_) => Err(invalid_reply()),
        }
    }

    /// The highest printed position of each channel creation.
    pub(super) fn take_printed(&mut self) -> Vec<(String, String, u64)> {
        let Self::Messages { printed, .. } = self else {
            return Vec::new();
        };
        let mut highest: Vec<(String, String, u64)> = Vec::new();
        for (channel, creation, position) in std::mem::take(printed) {
            match highest
                .iter_mut()
                .find(|(c, k, _)| *c == channel && *k == creation)
            {
                Some(entry) => entry.2 = entry.2.max(position),
                None => highest.push((channel, creation, position)),
            }
        }
        highest
    }

    pub(super) fn message_count(&self) -> usize {
        match self {
            Self::Messages { printed, .. } => printed.len(),
            Self::Agent { messages, .. } => messages.len(),
            _ => 0,
        }
    }

    pub(super) fn message(
        &mut self,
        header: &mut Map<String, Value>,
        body: &[u8],
    ) -> Result<(), Error> {
        if let Self::Messages { printed, .. } = self {
            let address = (
                field(header, "channel")?,
                field(header, "creation")?,
                header.get("position").and_then(Value::as_u64),
            );
            if let (channel, creation, Some(position)) = address {
                printed.push((channel.to_owned(), creation.to_owned(), position));
            }
        }
        match self {
            Self::Messages { json: true, .. } => line(format_args!(
                "{}",
                json_message(std::mem::take(header), body)
            )),
            Self::Messages { json: false, .. } => output::write(&message_block(header, body)?),
            Self::Agent { messages, .. } => {
                messages.push(presentation::Message::from_frame(
                    std::mem::take(header),
                    body.to_vec(),
                )?);
                Ok(())
            }
            Self::Collected(records) => {
                records.push(super::GuestRecord {
                    label: std::mem::take(header),
                    body: body.to_vec(),
                });
                Ok(())
            }
            _ => Err(invalid_reply()),
        }
    }
}

pub(super) fn agent_text(
    messages: &[presentation::Message],
    offset: usize,
) -> Result<presentation::Shown<String>, Error> {
    presentation::text(messages, offset, random_number()?, presentation::BUDGET)
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "note" => "Note",
        "task" => "Task",
        "report" => "Report",
        _ => "Message",
    }
}

fn get_result(header: &Map<String, Value>, result: &str) -> Result<(), Error> {
    let channel = field(header, "channel")?;
    if result == "absent" {
        return output::result(&format!("Channel {channel} is absent."));
    }
    let position = number_field(header, "position")?;
    match result {
        "trimmed" => output::details(
            &format!("Message {channel} #{position} was trimmed."),
            &[(
                "First kept",
                &number_field(header, "trimmed_before")?.to_string(),
            )],
        ),
        "void" => output::result(&format!("Message {channel} #{position} was lost.")),
        "never_written" => output::result(&format!(
            "No message has been written at {channel} #{position}."
        )),
        _ => Err(invalid_reply()),
    }
}

fn json_reply(mut header: Map<String, Value>) -> Map<String, Value> {
    header.remove("op");
    header.remove("rid");
    header
}

pub(in crate::cli) fn json_message(header: Map<String, Value>, body: &[u8]) -> Value {
    let mut record = json_reply(header);
    record.insert("body".into(), serde_json::json!(body));
    if let Ok(text) = std::str::from_utf8(body) {
        record.insert("text".into(), Value::String(text.into()));
    }
    Value::Object(record)
}

fn read_channels(header: &Map<String, Value>) -> Result<&[Value], Error> {
    header
        .get("channels")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(invalid_reply)
}

fn read_json_summary(header: &Map<String, Value>) -> Result<Value, Error> {
    Ok(serde_json::json!({ "channels": read_channels(header)? }))
}

fn read_result(header: &Map<String, Value>, shown: bool) -> Result<(), Error> {
    let channels = read_channels(header)?;
    let mut present = Vec::new();
    for entry in channels {
        let channel = entry["channel"].as_str().ok_or_else(invalid_reply)?;
        if entry["creation"].is_null() {
            output::result(&format!("Channel {channel} is absent."))?;
        } else {
            present.push(channel);
        }
        if let Some(floor) = entry["trimmed_before"].as_u64() {
            output::result(&format!(
                "History in {channel} before #{floor} was trimmed."
            ))?;
        }
        let lost = entry["voids"].as_u64().unwrap_or(0);
        if lost > 0 {
            output::result(&format!("{lost} lost positions passed in {channel}."))?;
        }
    }
    if header.get("held_back").and_then(Value::as_bool) == Some(true) {
        output::result("The board held back some messages.")?;
    } else if !shown && !present.is_empty() {
        output::result(&format!(
            "No messages returned from {}.",
            present.join(", ")
        ))?;
    }
    Ok(())
}

pub(in crate::cli) fn message_block(
    header: &Map<String, Value>,
    body: &[u8],
) -> Result<String, Error> {
    let channel = field(header, "channel")?;
    let position = number_field(header, "position")?;
    let creation = field(header, "creation")?;
    let sender = field(header, "sender")?;
    let sender_name = header
        .get("sender_name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| output::sender_name_or_id(sender));
    let grade = header.get("grade").and_then(Value::as_str);
    let kind = field(header, "kind")?;
    let guest = header.get("guest_board").and_then(Value::as_str);
    let label =
        if kind == "task" && (guest.is_some() || sender != "lead" || grade != Some("proven")) {
            "Information (task label)"
        } else {
            kind_label(kind)
        };
    let sent = header.get("sent_at").and_then(sent_ms);
    let timestamp = output::timestamp(sent, output::now_ms());
    let mut fields = vec![("Sent", timestamp.as_str()), ("Creation", creation)];
    if grade == Some("link_only") {
        fields.push(("Identity", "Link only"));
    }
    let trace = trace_header(header);
    if let Some(trace) = &trace {
        fields.push(("Trace", trace));
    }
    let reply = header.get("headers").and_then(|headers| {
        let reply = &headers["reply_to"];
        Some(format!(
            "{} #{} (creation {})",
            reply[0].as_str()?,
            reply[2].as_u64()?,
            reply[1].as_str()?
        ))
    });
    if let Some(reply) = &reply {
        fields.push(("Reply to", reply));
    }
    let reply_inbox = presentation::reply_inbox(channel, sender);
    if let Some(inbox) = &reply_inbox {
        fields.push(("Reply inbox", inbox.as_str()));
    }
    let lost = header
        .get("lost_before")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .map(|n| n.to_string());
    if let Some(lost) = &lost {
        fields.push(("Lost before this message", lost));
    }
    let title = match guest {
        Some(board) => format!("{label} from {sender_name} in guest · {board} #{position}"),
        None => format!("{label} from {sender_name} in {channel} #{position}"),
    };
    let mut block = output::Format::stdout().message(&title, &fields, body);
    block.push('\n');
    Ok(block)
}

fn sent_ms(stamp: &Value) -> Option<u64> {
    stamp[0]
        .as_u64()?
        .checked_mul(1000)?
        .checked_add(stamp[1].as_u64()? / 1_000_000)
}
