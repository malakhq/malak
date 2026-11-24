use super::{
    model::{Address, Event, Model, Preview, PreviewState},
    text,
};
use crate::cli::{
    output::{self, clock, display},
    presentation,
};
use std::fmt::Write as _;

pub(super) const HISTORY_LABEL: &str =
    "Acknowledgements and refusals shown since this view opened.";

pub(super) fn event_line(model: &Model, event: &Event) -> String {
    event_label(model, event, false)
}

pub(super) fn participant(name: &str) -> String {
    format!("@{}", display(name))
}

pub(super) fn activity_event(model: &Model, event: &Event) -> String {
    event_label(model, event, true)
}

fn event_label(model: &Model, event: &Event, live: bool) -> String {
    let sender = event.id.as_deref().map(|id| {
        if live {
            participant(model.name(id))
        } else {
            model.plain_name(id).to_owned()
        }
    });
    let actor = sender.as_deref().unwrap_or("board");
    let channel = event.channel.as_deref().map(|channel| {
        if live {
            model
                .inbox_recipient(channel)
                .map_or_else(|| channel.to_owned(), |person| participant(&person.name))
        } else {
            model.plain_recipient(channel).to_owned()
        }
    });
    let target = channel.as_deref().unwrap_or("the board");
    let position = position(event.position);
    let reason = event
        .reason
        .as_deref()
        .map_or_else(String::new, |reason| format!("  Reason  {reason}"));
    let mut line = match event.event.as_str() {
        "stored" => {
            let kind = event.kind.as_deref().unwrap_or("message");
            let details = event.address().map_or_else(String::new, |address| {
                header_details(model, &address, model.preview(&address), live)
            });
            format!("{actor} sent a {kind} to {target} {position}{details}")
        }
        "read" => format!("{actor} read {target} {position}"),
        "acked" => format!("{actor} acknowledged {target} through {position}"),
        "denied" | "limit_hit" => {
            let operation = event.operation.as_deref().unwrap_or("call");
            let outcome = if live {
                String::new()
            } else {
                format!(" {}", event.outcome.as_deref().unwrap_or("limit hit"))
            };
            let count = event
                .count
                .filter(|&n| n > 1)
                .map_or_else(String::new, |n| format!(" ({n} calls)"));
            format!("{operation}{outcome} for {actor} in {target}{count}{reason}")
        }
        "owner" => {
            let operation = event.operation.as_deref().unwrap_or("call");
            let target = channel
                .as_deref()
                .or(event.target.as_deref())
                .or(sender.as_deref())
                .unwrap_or("the board");
            let outcome = if live {
                String::new()
            } else {
                format!(
                    " ({})",
                    event.outcome.as_deref().unwrap_or("unknown outcome")
                )
            };
            format!("Owner {operation} for {target} {position}{outcome}{reason}")
        }
        kind => format!("{kind} for {target} {position} by {actor}"),
    };
    if event.missed > 0 {
        line.push_str(&format!("  [{} events missed]", event.missed));
    }
    output::event(event.at_ms, line.trim_end())
}

pub(super) fn plain_event(model: &Model, event: &Event) -> String {
    let mut block = event_line(model, event);
    let Some(address) = event.address() else {
        if event.event == "owner"
            && let Some(sender) = &event.sender_trace
        {
            write!(block, "\n  {}", sender.line("Owner", usize::MAX)).unwrap();
        }
        block.push('\n');
        return block;
    };
    let preview = model.preview(&address);
    if let PreviewState::Ready(message) = preview
        && model.answered_target(&address, message).is_some()
    {
        write!(block, "  {}", answered_by(model, &address)).unwrap();
    }
    if let Some(sender) = event
        .sender_trace
        .as_ref()
        .or_else(|| preview.sender_trace())
    {
        let actor = event
            .id
            .as_deref()
            .map_or("unknown sender", |id| model.plain_name(id));
        write!(block, "\n  {}", sender.line(actor, usize::MAX)).unwrap();
    }
    writeln!(block, "\n  {}", reply_address(&address, usize::MAX)).unwrap();
    if let Some(inbox) = event
        .id
        .as_deref()
        .and_then(|id| presentation::reply_inbox(&address.channel, id))
    {
        writeln!(block, "  Reply inbox  {inbox}").unwrap();
    }
    if let PreviewState::Ready(preview) = preview
        && let Some(quote) = quote_line(model, preview)
    {
        writeln!(block, "{quote}").unwrap();
    }
    message_body(&mut block, &address, preview);
    block
}

pub(super) fn message_body(block: &mut String, address: &Address, preview: PreviewState<'_>) {
    if let PreviewState::Ready(preview) = preview {
        if let Some(body) = &preview.body {
            body_block(block, &body.text);
        } else if let Some(bytes) = preview.cut_bytes {
            writeln!(block, "  Body preview  {}", fetch_hint(address, bytes)).unwrap();
        } else {
            block.push_str("  Body preview unavailable.\n");
        }
    } else {
        block.push_str("  Body preview unavailable.\n");
    }
    block.push('\n');
}

pub(super) fn body_block(block: &mut String, text: &str) {
    block.push_str("  Body\n");
    output::Format::stdout().paragraph(block, "    ", text);
}

pub(super) fn header_details(
    model: &Model,
    address: &Address,
    preview: PreviewState<'_>,
    live: bool,
) -> String {
    let mut details = String::new();
    if let PreviewState::Ready(preview) = preview
        && let Some(reply) = model.reply_reference(preview)
    {
        write!(details, "  Reply to #{}", reply.position).unwrap();
        if reply.channel != address.channel {
            let channel = if live {
                &reply.channel
            } else {
                model.plain_recipient(&reply.channel)
            };
            details.push(' ');
            details.push_str(&display(channel));
        }
    }
    if let Some(reply) = model.answered_by(address) {
        write!(details, "  {}", answered_by(model, reply)).unwrap();
    }
    details
}

pub(super) fn fetch_hint(address: &Address, bytes: usize) -> String {
    format!(
        "({bytes} bytes · malak get {} {})",
        display(&address.channel),
        address.position,
    )
}

pub(super) fn reply_address(address: &Address, columns: usize) -> String {
    text::reply_address(
        &address.channel,
        &address.creation,
        address.position,
        columns,
    )
}

pub(super) fn quote_line(model: &Model, preview: &Preview) -> Option<String> {
    let target = model.reply_reference(preview)?;
    let (sender, kind, at_ms, excerpt) = match model.preview(target) {
        PreviewState::Ready(original) => (
            &original.sender,
            &original.kind,
            original.at_ms(),
            original.body_text().map(text::quote),
        ),
        _ => {
            let quote = preview.quote.as_ref()?;
            (
                &quote.sender,
                &quote.kind,
                quote.at_ms,
                quote.excerpt.clone(),
            )
        }
    };
    let excerpt = excerpt.as_deref().unwrap_or("—");
    Some(format!(
        "  Quote  {} #{} {} {}  {excerpt}",
        display(model.name(sender)),
        target.position,
        display(kind),
        clock(Some(at_ms)),
    ))
}

fn answered_by(model: &Model, reply: &Address) -> String {
    format!(
        "Answered by #{} to {}",
        reply.position,
        display(model.recipient(&reply.channel)),
    )
}

fn position(at: Option<u64>) -> String {
    at.map_or_else(String::new, |at| format!("#{at}"))
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::{event, status};
    use super::*;

    #[test]
    fn a_send_line_uses_the_human_inbox_name() {
        let model = Model::new(status(), None);
        let stored = event("stored", "lead", "to/a0123456", "second", 1);

        let line = event_line(&model, &stored);

        assert!(line.contains("lead sent a message to worker #1"), "{line}");
    }

    #[test]
    fn a_message_without_sender_trace_still_renders() {
        let model = Model::new(status(), None);
        let stored = event("stored", "lead", "lead", "first", 3);

        let block = plain_event(&model, &stored);

        assert!(block.contains("lead sent a message to lead #3"), "{block}");
        assert!(block.contains("Reply address"), "{block}");
    }

    #[test]
    fn a_read_line_uses_the_action_word() {
        let model = Model::new(status(), None);
        let read = event("read", "a0123456", "to/a0123456", "second", 1);

        let line = event_line(&model, &read);

        assert!(line.contains("worker read worker #1"), "{line}");
    }
}
