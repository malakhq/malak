//! Talk to a board served over Unix sockets, from a few lines.
//!
//! Start the board with `socket_board FOLDER`, then run
//! `cargo run -p malak --example socket_client --no-default-features --features unix -- FOLDER`.
//!
//! The client attaches as the lead with the credentials the board wrote to
//! `FOLDER/state/lead.handover`, sends a note to the lead's own inbox, reads
//! what is due and acknowledges it.

use malak::client::Client;
use malak::transport::UnixTransport;
use malak::{Error, SendRequest};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<(), Error> {
    let folder = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: socket_client FOLDER"),
    );
    let mut client = Client::connect(
        Arc::new(UnixTransport::default()),
        folder.join("state/lead.handover"),
        None,
    )?;

    let id = format!("hello-{}", std::process::id());
    let receipt = client.send(SendRequest::note("lead", &id, b"hello over a socket"))?;
    println!("sent to lead #{}", receipt.position);

    let due = client.read(&["lead"], 10, Duration::from_secs(5))?;
    for message in &due.messages {
        println!(
            "read lead #{}: {}",
            message.receipt.position,
            String::from_utf8_lossy(&message.body)
        );
    }
    if let Some(last) = due.messages.last() {
        client.ack("lead", last.receipt.creation, last.receipt.position.get())?;
        println!("acknowledged through lead #{}", last.receipt.position);
    }
    Ok(())
}
