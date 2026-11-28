//! The CLI and MCP share the same reading-client projections and budget.

use super::{Reply, seat::Seat};
use crate::cli::{
    org::{bounded, pulse, rail, seats::Reader},
    output,
};
use malak::Error;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RailArgs {
    board: Option<String>,
    since: Option<String>,
}

pub(super) fn rail(seat: &Seat, args: RailArgs) -> Result<Reply, Error> {
    let reader = Reader::open(seat.config.clone())?;
    let items = rail::view(&reader, args.board.as_deref(), args.since.as_deref())?;
    let value = bounded::result(
        items,
        "Use get on the linked channel and position; rail since accepts an age such as 7d or epoch milliseconds.",
    );
    Ok(Reply {
        text: value.to_string(),
        value,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyArgs {}

pub(super) fn pulse(seat: &Seat, _args: EmptyArgs) -> Result<Reply, Error> {
    let reader = Reader::open(seat.config.clone())?;
    let value = bounded::result(
        pulse::view(&reader, output::now_ms())?,
        "Use get on the linked board facts.",
    );
    Ok(Reply {
        text: value.to_string(),
        value,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitArgs {
    on: String,
    subject: String,
    link: Option<(String, String, String, u64)>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HandoverArgs {
    to: String,
    subject: String,
}

pub(super) fn wait(seat: &Seat, args: WaitArgs) -> Result<Reply, Error> {
    write(seat, "wait", "--on", args.on, args.subject, args.link)
}
pub(super) fn handover(seat: &Seat, args: HandoverArgs) -> Result<Reply, Error> {
    write(seat, "handover", "--to", args.to, args.subject, None)
}
fn write(
    seat: &Seat,
    action: &str,
    flag: &str,
    target: String,
    subject: String,
    link: Option<(String, String, String, u64)>,
) -> Result<Reply, Error> {
    let mut words = vec!["rail".into(), action.into(), flag.into(), target, subject];
    if let Some((board, channel, creation, position)) = link {
        words.extend([
            "--link".into(),
            board,
            channel,
            creation,
            position.to_string(),
        ]);
    }
    let text = super::recipes::run(seat, &words, &[])?;
    Ok(Reply {
        value: serde_json::json!({"recorded":true,"receipt":text}),
        text,
    })
}
