use super::{EventName, HANDOVER, INBOX, STATE, Tool};
use crate::cli::{output::Format, presentation::BUDGET};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};

#[derive(Serialize)]
struct Description {
    environment: [Environment; 3],
    context_byte_limit: usize,
    events: Vec<Example>,
    monitor: Monitor,
}

#[derive(Serialize)]
struct Environment {
    name: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct Example {
    event: EventName,
    command: String,
    input: Value,
    output: Option<Value>,
}

#[derive(Serialize)]
struct Monitor {
    command: &'static str,
    fallback: &'static str,
}

pub(super) fn write(output: &mut dyn Write, tool: Tool, json: bool) -> io::Result<()> {
    let description = describe(tool);
    if json {
        serde_json::to_writer(&mut *output, &description)?;
        return writeln!(output);
    }

    let render = Format::stdout();
    let name = tool.to_possible_value().expect("every tool has a CLI name");
    write!(
        output,
        "{}",
        render.details(
            &format!("Malak hook contract for the {} tool", name.get_name()),
            &[]
        )
    )?;
    let environment: Vec<_> = description
        .environment
        .iter()
        .map(|item| (item.name, item.description))
        .collect();
    section(output, &render, "Environment", &environment)?;

    if render.interactive() {
        section(output, &render, "Delivery", &[])?;
    }
    prose(
        output,
        &render,
        &format!(
            "\nPut malak on the hook's PATH. Send one JSON object on standard input.\n\
         Use the tool's session ID consistently across all four events.\n\
         Each event acknowledges the previous delivery in that session.\n\
         session_start, prompt and step show one waiting message.\n\
         session_end acknowledges the previous delivery only if a prompt or step\n\
         event came after it in the same session. Otherwise the message stays due.\n\
         session_end shows nothing. Context is limited to {BUDGET} UTF-8 bytes."
        ),
    )?;
    let empty = match tool {
        Tool::Generic => "An event with nothing to show returns an empty context string.",
        Tool::Claude => "An event with nothing to show produces no output.",
        Tool::Codex => {
            "An event with nothing to show produces no output. In hooks.json, set\n\
             additionalContextLimit to 0 for events that show context and timeout to 3\n\
             for SessionEnd. Trust the hooks in Codex's hook review."
        }
    };
    prose(output, &render, empty)?;

    if render.interactive() {
        section(output, &render, "Acknowledgement", &[])?;
    }
    prose(
        output,
        &render,
        "\nThe hook can acknowledge a delivery before you finish handling it.\n\
         If a later read omits the message, use its channel and position with\n\
         malak get --agent CHANNEL POSITION and check the creation in the hook header.",
    )?;

    for example in description.events {
        let event = example.input["event"]
            .as_str()
            .or_else(|| example.input["hook_event_name"].as_str())
            .unwrap_or("Unknown");
        section(
            output,
            &render,
            &format!("Event {event}"),
            &[("Command", &example.command)],
        )?;
        write!(
            output,
            "{}",
            render.screen(
                "  Input (JSON object on stdin)",
                "",
                &fields(&example.input)
            )
        )?;
        if let Some(reply) = example.output {
            let mut reply_title = "  Output (JSON object on stdout)".to_owned();
            let value = if let Some(native) = reply.get("hookSpecificOutput") {
                reply_title.push_str(" / hookSpecificOutput");
                native
            } else {
                &reply
            };
            write!(
                output,
                "{}",
                render.screen(&reply_title, "", &fields(value))
            )?;
        } else {
            writeln!(output, "  No output")?;
        }
    }
    section(
        output,
        &render,
        "Lead monitor",
        &[
            ("Lead read loop", description.monitor.fallback),
            ("Open questions", "malak mailbox --questions"),
            ("Background command", description.monitor.command),
            ("Example payloads", "malak hook --describe --json"),
        ],
    )?;
    prose(
        output,
        &render,
        "The background command follows new messages on lead, questions, and orders. It flags lead work still unacknowledged after 15 minutes.",
    )?;
    let next = "Acknowledge messages after handling them.";
    if render.interactive() {
        let mut footer = String::new();
        render.next(&mut footer, next);
        output.write_all(footer.as_bytes())
    } else {
        writeln!(output, "{next}")
    }
}

fn prose(output: &mut dyn Write, render: &Format, text: &str) -> io::Result<()> {
    if !render.interactive() {
        return writeln!(output, "{text}");
    }
    let mut block = String::new();
    render.paragraph(&mut block, "  ", &text.trim().replace('\n', " "));
    output.write_all(block.as_bytes())
}

fn section(
    output: &mut dyn Write,
    render: &Format,
    title: &str,
    fields: &[(&str, &str)],
) -> io::Result<()> {
    let mut block = String::new();
    render.section(&mut block, title);
    render.screen_fields(&mut block, fields);
    output.write_all(block.as_bytes())
}

fn fields(value: &Value) -> Vec<(&str, &str)> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let value = value.as_str()?;
            Some((
                key.as_str(),
                if value.is_empty() {
                    "Empty string"
                } else {
                    value
                },
            ))
        })
        .collect()
}

fn describe(tool: Tool) -> Description {
    let name = tool.to_possible_value().expect("every tool has a CLI name");
    let command = format!("malak hook --tool {}", name.get_name());
    let format = tool.format();
    Description {
        environment: [
            Environment {
                name: HANDOVER,
                description: "Path to the participant's private handover file.",
            },
            Environment {
                name: INBOX,
                description: "Channel whose waiting messages the hook shows.",
            },
            Environment {
                name: STATE,
                description: "An absolute path to a private directory outside the workspace.",
            },
        ],
        context_byte_limit: BUDGET,
        monitor: Monitor {
            command: "malak mailbox --plain --for lead --next",
            fallback: "malak read --wait 60 lead questions",
        },
        events: EventName::ALL
            .into_iter()
            .map(|event| Example {
                event,
                command: command.clone(),
                input: format.input(event, "<the tool's session ID>"),
                output: format.reply(
                    event,
                    if event == EventName::SessionEnd {
                        ""
                    } else {
                        "<text for the agent>"
                    },
                ),
            })
            .collect(),
    }
}
