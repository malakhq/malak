//! Names reserved for top-level CLI commands.

use crate::{ChannelName, Error};

const COMMANDS: &[&str] = &[
    "ack",
    "board",
    "channel",
    "enrol",
    "get",
    "grant",
    "help",
    "hook",
    "mailbox",
    "read",
    "recipe",
    "recover",
    "rekey",
    "revoke",
    "send",
    "tail",
    "trim",
    "uninstall",
    "upgrade",
];

pub(crate) fn is_command(name: &str) -> bool {
    COMMANDS.contains(&name)
}

pub(crate) fn check_channel_write(name: &ChannelName) -> Result<(), Error> {
    if is_command(name.as_str()) {
        return Err(Error::refused(format!("channel name {name} is reserved")));
    }
    Ok(())
}
