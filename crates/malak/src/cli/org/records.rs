//! Read retained facts with the reader's participant connection, without ACK.

use crate::cli::{
    owner, owner_commands,
    participant::{self, Session},
};
use malak::Error;
use serde_json::Value;
use std::path::Path;

fn status(config: &Path) -> Result<Value, Error> {
    owner_commands::status(&mut owner(Some(config.to_owned()))?, &[])
}

/// Fetch one original through the caller's own Seat. The owner supplies only
/// the channel address; it never supplies a body or participant authority.
pub(in crate::cli) fn at(
    config: &Path,
    session: &mut Session,
    name: &str,
    position: u64,
) -> Result<Option<Value>, Error> {
    let status = status(config)?;
    let Some(channel) = status["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|channel| channel["name"] == name)
    else {
        return Ok(None);
    };
    if position < channel["floor"].as_u64().unwrap_or(1)
        || position >= channel["head"].as_u64().unwrap_or(1)
    {
        return Ok(None);
    }
    let creation = channel["creation"]
        .as_str()
        .ok_or_else(|| Error::unavailable("channel has no creation"))?;
    Ok(session
        .get_record_with_body(name, creation, position, false, true)?
        .map(|(record, body)| participant::json_message(record.label, &body)))
}

pub(in crate::cli) fn retained(
    config: &Path,
    session: &mut Session,
    channels: &[&str],
) -> Result<Vec<Value>, Error> {
    // The owner door supplies channel addresses only; every body is a normal
    // authenticated GET through the caller's own Seat and its grant.
    let status = status(config)?;
    let mut records = Vec::new();
    for channel in status["channels"].as_array().into_iter().flatten() {
        let name = channel["name"].as_str().unwrap_or("");
        if !channels.iter().any(|pattern| {
            *pattern == name
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| name.starts_with(prefix))
        }) {
            continue;
        }
        let creation = channel["creation"]
            .as_str()
            .ok_or_else(|| Error::unavailable("channel has no creation"))?;
        for position in
            channel["floor"].as_u64().unwrap_or(1)..channel["head"].as_u64().unwrap_or(1)
        {
            if let Some((record, body)) =
                session.get_record_with_body(name, creation, position, false, true)?
            {
                records.push(participant::json_message(record.label, &body));
            }
        }
    }
    records.sort_by_key(|record| {
        (
            super::typed::at_ms(record),
            record["channel"].as_str().unwrap_or("").to_owned(),
            record["position"].as_u64().unwrap_or(0),
        )
    });
    Ok(records)
}
