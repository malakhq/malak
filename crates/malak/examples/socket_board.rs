//! Serve a board over Unix sockets, from a few lines.
//!
//! Run it with
//! `cargo run -p malak --example socket_board --no-default-features --features unix,markdown,key -- FOLDER`.
//!
//! The board keeps its state in `FOLDER`, listens on three sockets there, and
//! writes the lead's credentials to `FOLDER/state/lead.handover`. It runs until interrupted. `FOLDER` must sit in a
//! directory that only you can write, with no symbolic link in its path.

use malak::Error;
use malak::mechanism::Key;
use malak::server::{Entrance, LocalStartup, Server};
use malak::stores::MarkdownStore;
use malak::transport::UnixTransport;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> Result<(), Error> {
    let folder = PathBuf::from(std::env::args().nth(1).expect("usage: socket_board FOLDER"));
    std::fs::create_dir_all(&folder).map_err(|error| Error::unavailable(error.to_string()))?;
    let uid = std::fs::metadata(&folder)
        .map_err(|error| Error::unavailable(error.to_string()))?
        .uid();
    let address = |name: &str| format!("unix:{}", folder.join(name).display());
    let (_owner, server) = Server::create(
        MarkdownStore::new(folder.join("messages"), true),
        malak::Limits::default(),
        folder.join("state"),
        LocalStartup {
            owner_address: address("owner"),
            lead_address: address("lead.sock"),
            general_address: address("general"),
            lead_handover: folder.join("state/lead.handover"),
            mechanism_name: "key".into(),
        },
        Arc::new(Key),
        uid,
    )?;
    for (entrance, name) in [
        (Entrance::Owner, "owner"),
        (Entrance::Lead, "lead.sock"),
        (Entrance::General, "general"),
    ] {
        let transport = Arc::new(UnixTransport::bind(&address(name))?);
        server.listen(entrance, transport, address(name), 8)?;
    }
    server.ready();
    println!(
        "serving; lead credentials are in {}",
        folder.join("state/lead.handover").display()
    );
    server.run()
}
