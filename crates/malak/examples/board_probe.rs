//! The smallest in-process board: the program `scripts/board-size` measures.

use malak::{Action, Grant, SendRequest};
use std::time::Duration;

#[cfg(feature = "markdown")]
fn board() -> Result<malak::Owner, malak::Error> {
    malak::create_durable(std::env::temp_dir().join("board-probe"), true)
}

#[cfg(not(feature = "markdown"))]
fn board() -> Result<malak::Owner, malak::Error> {
    malak::in_memory()
}

fn main() -> Result<(), malak::Error> {
    let owner = board()?;
    let mut grant = Grant::default();
    grant.allow("lead".parse()?, [Action::Send]);
    let worker = owner.enrol("worker", grant)?;
    worker.send(SendRequest::note("lead", "run-1", b"checks passed"))?;
    let reply = owner.lead().read(&["lead"], 1, Duration::ZERO)?;
    std::process::exit(i32::from(reply.messages.is_empty()))
}
