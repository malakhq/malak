//! List the orders on a board's `orders` channel, in force and ended, with
//! the decisions made under each.
//!
//! An order is a message from the lead on `orders`. Its `Until:` line says
//! when it ends, and one without it ends 8 hours after it was posted. A
//! decision is a message from the lead whose first line is
//! `Under order: CREATION POSITION`. Both are found in the stored messages, so
//! the review holds across restarts.

use super::{
    messages::{RecordBody, read_message, readable_channels},
    model::{Address, Channel, Model, Participant, Preview, inbox_recipient},
    now_ms, status, text,
};
use crate::cli::{output, owner, participant::Session};
use malak::Error;
use std::{collections::BTreeMap, path::PathBuf};
use time::{Date, Month, PrimitiveDateTime, Time};

const CHANNEL: &str = "orders";
const LEAD: &str = "lead";
const UNTIL: &str = "Until: ";
const MARK: &str = "Under order: ";
const DEFAULT_LENGTH_MS: u64 = 8 * 60 * 60 * 1000;

struct Order {
    address: Address,
    sent_at: [u64; 2],
    at_ms: u64,
    ends_ms: u64,
    scope: String,
    decisions: Vec<Decision>,
}

struct Decision {
    address: Address,
    sent_at: [u64; 2],
    recipient: String,
    first_line: String,
}

pub(super) fn run(config: Option<PathBuf>, plain: bool) -> Result<(), Error> {
    let status = status(&mut owner(config.clone())?, false)?;
    let Some((mut session, id)) = Session::open_preview(config) else {
        return Err(Error::unavailable("no handover to read the board with"));
    };
    let viewer = status
        .participants
        .iter()
        .find(|person| person.id == id.as_str())
        .ok_or_else(|| Error::refused("the handover does not name an enrolled participant"))?;
    let readable = readable_channels(status.channels.iter(), viewer);
    let Some(orders) = readable.clone().find(|channel| channel.name == CHANNEL) else {
        return output::result("No orders recorded.");
    };
    let message_text = if plain {
        RecordBody::Preview
    } else {
        RecordBody::Full
    };
    let mut found = posted(orders, &mut session, message_text)
        .map(|result| result.map(|(address, message)| order(address, &message)))
        .collect::<Result<Vec<_>, _>>()?;
    if found.is_empty() {
        return output::result("No orders recorded.");
    }
    for channel in readable.filter(|channel| channel.name != CHANNEL) {
        for address in channel.addresses() {
            let Some(message) = read_message(&mut session, &address, RecordBody::Preview)? else {
                continue;
            };
            if let Some(position) = decision_under(&message, orders)
                && let Some(order) = found
                    .iter_mut()
                    .find(|order| order.address.position == position)
            {
                let message = if plain {
                    message
                } else {
                    let Some(full) = read_message(&mut session, &address, RecordBody::Full)? else {
                        continue;
                    };
                    full
                };
                order
                    .decisions
                    .push(decision(address, &message, &status.participants));
            }
        }
    }
    if !plain {
        found.sort_unstable_by(|a, b| {
            b.address
                .message_order(b.sent_at)
                .cmp(&a.address.message_order(a.sent_at))
        });
        for order in &mut found {
            order.decisions.sort_unstable_by(|a, b| {
                b.address
                    .message_order(b.sent_at)
                    .cmp(&a.address.message_order(a.sent_at))
            });
        }
    }
    let from = status
        .participants
        .iter()
        .find(|person| person.id == LEAD)
        .map_or(LEAD, |person| person.name.as_str());
    output::write(&render(&found, from, now_ms(), plain))
}

pub(super) fn end_times<'a, E>(
    channel: &Channel,
    mut read: impl FnMut(&Address) -> Result<Option<&'a Preview>, E>,
) -> Result<Vec<u64>, E> {
    let mut ends = Vec::new();
    for address in channel.addresses() {
        if let Some(message) = read(&address)?
            && message.sender == LEAD
        {
            ends.push(order_end(message));
        }
    }
    Ok(ends)
}

pub(super) struct LiveOrder<'a> {
    pub address: Address,
    pub message: &'a Preview,
    pub decisions: Vec<(Address, &'a Preview)>,
}

pub(super) fn live(model: &Model) -> Vec<LiveOrder<'_>> {
    let Some(orders) = model.channels().find(|channel| channel.name == CHANNEL) else {
        return Vec::new();
    };
    let mut entries: BTreeMap<_, _> = orders
        .addresses()
        .filter_map(|address| {
            let message = model.resolved_preview(&address).flatten()?;
            (message.sender == LEAD).then_some((
                address.position,
                LiveOrder {
                    address,
                    message,
                    decisions: Vec::new(),
                },
            ))
        })
        .collect();
    if !entries.is_empty() {
        for address in model
            .channels()
            .filter(|c| c.name != CHANNEL)
            .flat_map(Channel::addresses)
        {
            if let Some(message) = model.resolved_preview(&address).flatten()
                && let Some(position) = decision_under(message, orders)
                && let Some(order) = entries.get_mut(&position)
            {
                order.decisions.push((address, message));
            }
        }
    }
    entries.into_values().collect()
}

fn decision_under(message: &Preview, orders: &Channel) -> Option<u64> {
    if message.sender != LEAD {
        return None;
    }
    let (creation, position) = marked_order(message.text.as_deref()?)?;
    (creation == orders.creation).then_some(position)
}

fn posted<'a>(
    channel: &'a Channel,
    session: &'a mut Session,
    text: RecordBody,
) -> impl Iterator<Item = Result<(Address, Preview), Error>> + 'a {
    channel
        .addresses()
        .filter_map(move |address| match read_message(session, &address, text) {
            Ok(Some(message)) if message.sender == LEAD => Some(Ok((address, message))),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
}

pub(super) fn order_end(message: &Preview) -> u64 {
    ends_at(message.at_ms(), message.text.as_deref().unwrap_or_default())
}

fn order(address: Address, message: &Preview) -> Order {
    let text = message
        .body_text()
        .or(message.text.as_deref())
        .unwrap_or_default();
    Order {
        address,
        sent_at: message.sent_at,
        at_ms: message.at_ms(),
        ends_ms: order_end(message),
        scope: scope_lines(text).collect::<Vec<_>>().join("\n"),
        decisions: Vec::new(),
    }
}

fn decision(address: Address, message: &Preview, participants: &[Participant]) -> Decision {
    let text = message
        .body_text()
        .or(message.text.as_deref())
        .unwrap_or_default();
    let channel = &address.channel;
    let recipient = inbox_recipient(participants, channel).map_or_else(
        || {
            channel
                .strip_prefix("to/")
                .map_or_else(|| channel.clone(), output::short_id)
        },
        |person| person.name.clone(),
    );
    Decision {
        address,
        sent_at: message.sent_at,
        recipient,
        first_line: decision_lines(text).next().unwrap_or_default().to_owned(),
    }
}

pub(super) fn scope_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| !line.starts_with(UNTIL))
        .skip_while(|line| line.trim().is_empty())
}

pub(super) fn decision_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .skip(1)
        .skip_while(|line| line.trim().is_empty())
}

/// When an order posted at `posted_ms` with this text stops applying.
fn ends_at(posted_ms: u64, text: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(UNTIL))
        .and_then(|value| parse_utc(value.trim()))
        .unwrap_or_else(|| posted_ms.saturating_add(DEFAULT_LENGTH_MS))
}

/// Read `YYYY-MM-DDTHH:MM:SSZ` as milliseconds since the Unix epoch.
fn parse_utc(value: &str) -> Option<u64> {
    let digits = |range: std::ops::Range<usize>| value.get(range)?.parse::<u32>().ok();
    if value.len() != 20
        || !value.is_ascii()
        || [4, 7, 10, 13, 16, 19]
            .iter()
            .zip(['-', '-', 'T', ':', ':', 'Z'])
            .any(|(at, expected)| value.as_bytes()[*at] != expected as u8)
    {
        return None;
    }
    let date = Date::from_calendar_date(
        i32::try_from(digits(0..4)?).ok()?,
        Month::try_from(u8::try_from(digits(5..7)?).ok()?).ok()?,
        u8::try_from(digits(8..10)?).ok()?,
    )
    .ok()?;
    let time = Time::from_hms(
        u8::try_from(digits(11..13)?).ok()?,
        u8::try_from(digits(14..16)?).ok()?,
        u8::try_from(digits(17..19)?).ok()?,
    )
    .ok()?;
    let seconds = PrimitiveDateTime::new(date, time)
        .assume_utc()
        .unix_timestamp();
    u64::try_from(seconds).ok()?.checked_mul(1000)
}

/// The order a message says it was decided under: its creation and position.
fn marked_order(text: &str) -> Option<(String, u64)> {
    let mark = text.lines().next()?.strip_prefix(MARK)?;
    let (creation, position) = mark.split_once(' ')?;
    Some((creation.to_owned(), position.trim().parse().ok()?))
}

fn render(orders: &[Order], from: &str, now: u64, plain: bool) -> String {
    let format = output::Format::stdout();
    if orders.is_empty() {
        return format.details("No orders recorded.", &[]);
    }
    let mut out = format.details("Orders", &[]);
    let width = text::terminal_width();
    for order in orders {
        let position = order.address.position;
        let state = if now < order.ends_ms {
            "in force"
        } else {
            "ended"
        };
        let ends = output::timestamp(Some(order.ends_ms), now);
        if !plain {
            let age = output::age(order.at_ms, now);
            let title = format!("Order #{position} · {from} · {age}");
            let until = format!("{state} · until {ends}");
            let address = &order.address;
            let columns = text::item_width(width);
            let reply = text::reply_address(&address.channel, &address.creation, position, columns);
            let mut lines = vec![until];
            lines.extend(reply.lines().map(str::to_owned));
            lines.push(String::new());
            lines.extend(
                text::preview(order.scope.lines())
                    .into_iter()
                    .map(str::to_owned),
            );
            for decision in &order.decisions {
                let address = &decision.address;
                let title = format!("Decision  {} #{}", decision.recipient, address.position);
                let reply = text::reply_address(
                    &address.channel,
                    &address.creation,
                    address.position,
                    columns,
                );
                lines.push(title);
                lines.extend(reply.lines().map(str::to_owned));
                lines.push(decision.first_line.clone());
            }
            text::item(&mut out, width, &title, lines.iter().map(String::as_str));
            continue;
        }
        // answer --under-order uses the unstyled position and state prefix.
        out.push_str(&format!("order #{position}  {state}  until {ends}\n"));
        format.fields(
            &mut out,
            &[("Scope", order.scope.lines().next().unwrap_or_default())],
        );
        for decision in &order.decisions {
            let address = &decision.address;
            let label = format!("{} #{}", address.channel, address.position);
            format.fields(&mut out, &[("Decision", &label)]);
            format.paragraph(&mut out, "            ", &decision.first_line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    #[test]
    fn an_order_with_no_end_time_ends_8_hours_after_it_was_posted() {
        assert_eq!(ends_at(5 * HOUR, "Fix lint failures."), 13 * HOUR);
    }

    #[test]
    fn an_order_ends_at_its_until_line() {
        let text = "Until: 1970-01-02T00:00:00Z\n\nFix lint failures.";
        assert_eq!(ends_at(0, text), 24 * HOUR);
    }

    #[test]
    fn an_unreadable_until_line_falls_back_to_8_hours() {
        assert_eq!(ends_at(0, "Until: tomorrow"), 8 * HOUR);
    }

    #[test]
    fn a_mark_names_the_order_by_creation_and_position() {
        assert_eq!(
            marked_order("Under order: abc 3\n\nUse the older API."),
            Some(("abc".into(), 3))
        );
        assert_eq!(marked_order("Use the older API."), None);
    }

    #[test]
    fn an_order_ends_when_its_end_time_is_reached() {
        let order = |ends_ms| Order {
            address: Address {
                channel: CHANNEL.into(),
                creation: "creation".into(),
                position: 1,
            },
            sent_at: [0, 0],
            at_ms: 0,
            ends_ms,
            scope: "Fix lint failures.".into(),
            decisions: Vec::new(),
        };
        assert!(render(&[order(HOUR)], LEAD, HOUR - 1, true).contains("in force"));
        assert!(render(&[order(HOUR)], LEAD, HOUR, true).contains("ended"));
    }
}
