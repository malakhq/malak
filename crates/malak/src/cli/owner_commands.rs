use super::{
    Channel,
    board_version::BoardVersion,
    client_transport::{ClientTransport, link_io_error},
    config, output,
};
use malak::{ChannelName, ChannelPattern, CreationId, Error, ParticipantId};
use malak_core::LocalClient;
use malak_proto::{Frame, encode_frame};
use minicbor::{Decoder, Encoder, data::Type};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

pub(super) type Client = LocalClient<ClientTransport>;
const RID: u64 = 1;

pub(super) fn show_status(
    client: &mut Client,
    version: &BoardVersion,
    target: Option<&str>,
    json: bool,
) -> Result<(), Error> {
    let mut reply = match target {
        None => status(client, &[])?,
        Some(target) if target.parse::<ParticipantId>().is_ok() => {
            let by_id = status(client, &[("id", target)])?;
            if by_id["participants"].as_array().is_some_and(Vec::is_empty) {
                status(client, &[("channel", target)])?
            } else {
                by_id
            }
        }
        Some(target) => status(client, &[("channel", target)])?,
    };
    if json {
        reply["board_version"] = Value::from(version.recorded());
        reply["board_build"] = Value::from(version.build());
        print_json(&reply)
    } else {
        print_status(&reply, version, target)
    }
}

pub(super) fn current_run(client: &mut Client) -> Result<String, Error> {
    Ok(required(&status(client, &[])?, "run")?.to_owned())
}

pub(super) fn stop_run(client: &mut Client, run: &str) -> Result<(), Error> {
    request(client, "STOP", &[("run", run)]).map(drop)
}

pub(super) fn post_upgrade_notice(
    client: &mut Client,
    run: &str,
    previous: Option<&str>,
    installed: &str,
    entry: Option<&str>,
) -> Result<(), Error> {
    let mut fields = vec![
        ("run", run),
        ("previous_build", previous.unwrap_or("unknown")),
        ("installed_build", installed),
    ];
    if let Some(entry) = entry {
        fields.push(("previous_entry", entry));
    }
    request(client, "UPGRADE_NOTICE", &fields).map(drop)
}

pub(super) fn ping(client: &mut Client) -> Result<(), Error> {
    request(client, "PING", &[]).map(drop)
}

pub(super) fn revoke(client: &mut Client, target: &str) -> Result<(), Error> {
    let reply = status(client, &[])?;
    let people = reply["participants"].as_array().ok_or_else(invalid)?;
    let id = revoke_id(people, target)?;
    request(client, "REVOKE", &[("id", id.as_str())])?;
    output::result(&format!("Access revoked for participant {id}."))
}

fn revoke_id(people: &[Value], target: &str) -> Result<ParticipantId, Error> {
    let mut matches = Vec::new();
    for person in people {
        let id = required(person, "id")?;
        if id == target {
            return id.parse();
        }
        if required(person, "name")? == target && !boolean(person, "revoked")? {
            matches.push(id);
        }
    }
    match matches.as_slice() {
        [id] => id.parse(),
        [] => Err(Error::refused(format!(
            "no active participant named {target:?} or matching ID"
        ))),
        _ => {
            matches.sort_unstable();
            Err(Error::refused(format!(
                "several active participants are named {target:?}; use one of these IDs: {}",
                matches.join(", ")
            )))
        }
    }
}

pub(super) fn rekey(
    client: &mut Client,
    id: &ParticipantId,
    fingerprint: Option<String>,
    destination: &Path,
) -> Result<(), Error> {
    let fingerprint = match fingerprint {
        Some(fingerprint) => fingerprint,
        None => {
            let status = status(client, &[("id", id.as_str())])?;
            status["participants"][0]["fingerprint"]
                .as_str()
                .ok_or_else(|| Error::refused("participant has no current key"))?
                .to_owned()
        }
    };
    let path = std::path::absolute(destination).map_err(config::io_error)?;
    let path = path
        .to_str()
        .ok_or_else(|| Error::refused("output path must be UTF-8"))?;
    let path = client.rekey(id, &fingerprint, path)?;
    output::details(
        &format!("Credential replaced for {id}."),
        &[("Handover", &path)],
    )
}

pub(super) fn channel(client: &mut Client, command: Channel) -> Result<(), Error> {
    match command {
        Channel::Create { channel } => {
            let reply = fields(&request(
                client,
                "CHANNEL_CREATE",
                &[("channel", channel.as_str())],
            )?)?;
            let creation = required(&reply, "creation")?;
            let action = if boolean(&reply, "created")? {
                "created"
            } else {
                "already exists"
            };
            output::details(
                &format!("Channel {channel} {action}."),
                &[("Creation", creation)],
            )
        }
        Channel::Remove { channel, creation } => {
            let absent = || output::result(&format!("Channel {channel} is already absent."));
            let Some(creation) = resolve(client, &channel, creation)? else {
                return absent();
            };
            let reply = fields(&request(
                client,
                "CHANNEL_REMOVE",
                &[("channel", channel.as_str()), ("creation", &creation)],
            )?)?;
            if boolean(&reply, "removed")? {
                output::details(
                    &format!("Channel {channel} removed."),
                    &[("Creation", &creation)],
                )
            } else if let Some(current) = reply["creation"].as_str() {
                output::details(
                    &format!("Channel {channel} was left in place."),
                    &[("Current creation", current)],
                )
            } else {
                absent()
            }
        }
    }
}

pub(super) fn trim(
    client: &mut Client,
    channel: &ChannelName,
    position: u64,
    creation: Option<CreationId>,
) -> Result<(), Error> {
    let creation = resolve(client, channel, creation)?
        .ok_or_else(|| Error::refused("channel has no current creation"))?;
    let output = encode_frame(
        RID,
        "TRIM",
        3,
        |output| {
            text_fields(
                output,
                &[("channel", channel.as_str()), ("creation", &creation)],
            );
            output.str("position").unwrap().u64(position).unwrap();
        },
        &[],
    )
    .map_err(|_| Error::refused("trim request does not fit"))?;
    let reply = client.owner_call(output, RID)?;
    let floor = fields(&reply)?["floor"].as_u64().ok_or_else(invalid)?;
    output::details(
        &format!("History before position {floor} is no longer retained in {channel}."),
        &[("First kept", &floor.to_string())],
    )
}

pub(super) fn tail(
    client: &mut Client,
    pattern: Option<&ChannelPattern>,
    json: bool,
) -> Result<(), Error> {
    let mut tail = Tail::start(client, pattern)?;
    if !json {
        let selection = pattern.map_or_else(|| "all channels".into(), ToString::to_string);
        output::progress(
            &format!(
                "Following events for {selection} ({}).",
                output::local_zone(output::now_ms())
            ),
            Some("Press Ctrl-C to stop."),
        );
    }
    while let Some(event) = tail.next(client)? {
        if json {
            print_json(&event)?;
        } else {
            print_event(&event)?;
        }
    }
    Ok(())
}

fn print_event(event: &Value) -> Result<(), Error> {
    let name = required(event, "event")?;
    let operation = event["operation"]
        .as_str()
        .unwrap_or("call")
        .to_ascii_lowercase();
    let target = event["channel"]
        .as_str()
        .or_else(|| event["target"].as_str());
    let address = target.map_or_else(String::new, |target| {
        event["position"]
            .as_u64()
            .map_or_else(|| format!(" {target}"), |pos| format!(" {target} #{pos}"))
    });
    let outcome = event["outcome"].as_str().unwrap_or("unknown outcome");
    let description = match name {
        "stored" => {
            let kind = match event["kind"].as_str() {
                Some("task") => "Task",
                Some("note") => "Note",
                Some("report") => "Report",
                _ => "Message",
            };
            format!(
                "{kind} stored in{}",
                if address.is_empty() {
                    " the board"
                } else {
                    &address
                }
            )
        }
        "read" => format!("Read from{address}"),
        "acked" => format!("Acknowledged{address}"),
        "trimmed" => format!("History trimmed in{address}"),
        "denied" | "limit_hit" => format!("{operation} {outcome} in{address}"),
        "owner" => format!("Owner {operation}{address} ({outcome})"),
        "store_damaged" => format!("Store damage in{address}"),
        "tail_cut" => format!("Tail cut in{address}"),
        "tail_tampered" => format!("Tail tampered with in{address}"),
        other => format!("{other}{address}"),
    };
    let format = output::Format::stdout();
    let mut description = description.trim_end().to_owned();
    let actor = event["id"]
        .as_str()
        .map(output::sender_name_or_id)
        .map(|name| {
            if format.interactive() {
                name.to_owned()
            } else {
                output::short_id(name)
            }
        });
    if let Some(actor) = actor {
        description.push_str(&format!(" by {actor}"));
    }
    if let Some(count) = event["count"].as_u64().filter(|&count| count > 1) {
        description.push_str(&format!(" ({count} calls)"));
    }
    let mut block = String::new();
    format.paragraph(
        &mut block,
        "",
        &output::event(event["at_ms"].as_u64(), &description),
    );
    if let Some(reason) = event["reason"].as_str() {
        format.paragraph(
            &mut block,
            "          ",
            &format!("Reason  {}", output::display(reason)),
        );
    }
    if let Some(missed) = event["missed"].as_u64().filter(|&missed| missed > 0) {
        let noun = if missed == 1 { "event" } else { "events" };
        block.push_str(&format!("{missed} {noun} missed.\n"));
    }
    output::write(&block)
}

pub(super) struct Tail {
    pending: VecDeque<Value>,
    ended: bool,
}

pub(super) fn is_stop(operation: Option<&str>, outcome: Option<&str>) -> bool {
    operation == Some("STOP") && outcome == Some("done")
}

impl Tail {
    pub(super) fn start(
        client: &mut Client,
        pattern: Option<&ChannelPattern>,
    ) -> Result<Self, Error> {
        let filter = pattern.map(|pattern| ("pattern", pattern.as_str()));
        let frame = encode_frame(
            RID,
            "TAIL",
            filter.as_slice().len(),
            |output| {
                text_fields(output, filter.as_slice());
            },
            &[],
        )
        .map_err(|_| Error::refused("tail request does not fit"))?;
        client
            .send_frame(frame, Duration::from_secs(5))
            .map_err(link_io_error)?;
        let ping = encode_frame(2, "PING", 0, |_| {}, &[]).map_err(|_| invalid())?;
        client
            .send_frame(ping, Duration::from_secs(5))
            .map_err(link_io_error)?;
        let mut tail = Self {
            pending: VecDeque::new(),
            ended: false,
        };
        loop {
            let frame = client
                .read_frame(Duration::from_secs(5))
                .map_err(link_io_error)?;
            if frame.rid() == 2 {
                Client::owner_reply(frame, 2)?;
                return Ok(tail);
            }
            if let Some(event) = tail.event(frame)? {
                tail.pending.push_back(event);
                if tail.ended {
                    return Ok(tail);
                }
            }
        }
    }

    pub(super) fn next(&mut self, client: &mut Client) -> Result<Option<Value>, Error> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if self.ended {
            return Ok(None);
        }
        let frame = client
            .read_frame(Duration::from_secs(24 * 60 * 60))
            .map_err(link_io_error)?;
        self.event(frame)
    }

    fn event(&mut self, frame: Frame) -> Result<Option<Value>, Error> {
        if frame.op() != "EVENT" {
            Client::owner_reply(frame, RID)?;
            self.ended = true;
            return Ok(None);
        }
        if frame.rid() != RID || !frame.body().is_empty() {
            return Err(Error::unavailable("invalid tail event"));
        }
        let event = fields(&frame)?;
        self.ended = is_stop(event["operation"].as_str(), event["outcome"].as_str());
        Ok(Some(event))
    }
}

fn request(client: &mut Client, op: &str, fields: &[(&str, &str)]) -> Result<Frame, Error> {
    let frame = encode_frame(
        RID,
        op,
        fields.len(),
        |output| text_fields(output, fields),
        &[],
    )
    .map_err(|_| Error::refused("owner request does not fit"))?;
    client.owner_call(frame, RID)
}

fn text_fields(output: &mut Encoder<Vec<u8>>, fields: &[(&str, &str)]) {
    for &(key, value) in fields {
        output.str(key).unwrap().str(value).unwrap();
    }
}

pub(super) fn status(client: &mut Client, selector: &[(&str, &str)]) -> Result<Value, Error> {
    let frame = request(client, "STATUS", selector)?;
    let mut reply = decode(frame.body())?;
    reply["run"] = fields(&frame)?["run"].clone();
    reply["version"] = Value::from(1);
    Ok(reply)
}

fn resolve(
    client: &mut Client,
    name: &ChannelName,
    creation: Option<CreationId>,
) -> Result<Option<String>, Error> {
    if let Some(creation) = creation {
        return Ok(Some(creation.to_string()));
    }
    let current = status(client, &[("channel", name.as_str())])?;
    Ok(current["channels"][0]["creation"]
        .as_str()
        .map(str::to_owned))
}

fn fields(frame: &Frame) -> Result<Value, Error> {
    let mut json = decode(frame.header())?;
    if let Some(map) = json.as_object_mut() {
        map.remove("rid");
        map.remove("op");
    }
    Ok(json)
}

fn decode(bytes: &[u8]) -> Result<Value, Error> {
    let mut input = Decoder::new(bytes);
    let value = item(&mut input, 0).filter(|value| value.is_object());
    if input.position() != bytes.len() {
        return Err(invalid());
    }
    value.ok_or_else(invalid)
}

fn item(input: &mut Decoder<'_>, depth: usize) -> Option<Value> {
    if depth >= 8 {
        return None;
    }
    Some(match input.datatype().ok()? {
        Type::Null => {
            input.null().ok()?;
            Value::Null
        }
        Type::String => Value::from(input.str().ok()?),
        Type::Bool => Value::from(input.bool().ok()?),
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => Value::from(input.u64().ok()?),
        Type::Array => {
            let mut list = Vec::new();
            for _ in 0..input.array().ok()?? {
                list.push(item(input, depth + 1)?);
            }
            Value::Array(list)
        }
        Type::Map => {
            let mut map = Map::new();
            for _ in 0..input.map().ok()?? {
                let key = input.str().ok()?.to_owned();
                if map.insert(key, item(input, depth + 1)?).is_some() {
                    return None;
                }
            }
            Value::Object(map)
        }
        _ => return None,
    })
}

fn print_json(value: &Value) -> Result<(), Error> {
    let json = serde_json::to_string(value).map_err(|_| invalid())?;
    output::line(format_args!("{json}"))
}

fn boolean(value: &Value, key: &str) -> Result<bool, Error> {
    value[key].as_bool().ok_or_else(invalid)
}

fn required<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value[name].as_str().ok_or_else(invalid)
}

fn invalid() -> Error {
    Error::unavailable("invalid owner response")
}

fn print_status(status: &Value, version: &BoardVersion, target: Option<&str>) -> Result<(), Error> {
    let format = output::Format::stdout();
    let people = status["participants"].as_array().ok_or_else(invalid)?;
    let channels = status["channels"].as_array().ok_or_else(invalid)?;
    let now = output::now_ms();
    if let Some(target) = target {
        let block = if let Some(person) = people.first() {
            participant_details(&format, person, now)?
        } else if let Some(channel) = channels.first() {
            channel_details(&format, channel)?
        } else {
            format.details(
                &format!("No current channel or participant matches {target}."),
                &[],
            )
        };
        return output::write(&block);
    }
    let run = required(status, "run")?;
    let run = if format.interactive() {
        run.to_owned()
    } else {
        output::short_id(run)
    };
    let started = output::timestamp(status["started_at_ms"].as_u64(), now);
    let layers = &status["layers"];
    let mut block = format.screen(
        "Board running",
        "",
        &[
            ("Run", &run),
            ("Version", version.label()),
            ("Build", version.build().unwrap_or("unknown")),
            ("Started", &started),
            (
                "Entrance",
                layers["entrance"].as_str().unwrap_or("Unavailable"),
            ),
            (
                "Mechanism",
                layers["mechanism"].as_str().unwrap_or("Unavailable"),
            ),
            (
                "Durability",
                output::durability(layers["store_durability"].as_str().unwrap_or("Unavailable")),
            ),
            (
                "Repair",
                status["repair"].as_str().unwrap_or("No unfinished plan"),
            ),
        ],
    );

    let mut person_rows = Vec::with_capacity(people.len());
    let ids = people.iter().filter_map(|person| person["id"].as_str());
    for person in people {
        let name = required(person, "name")?.to_owned();
        let id = required(person, "id")?;
        let state = person_state(person, now).to_owned();
        let connections = number(person, "connections")?;
        person_rows.push(if format.interactive() {
            vec![name, state, connections, id.to_owned()]
        } else {
            vec![name, output::unique_id(id, ids.clone()), state, connections]
        });
    }

    let mut channel_rows = Vec::with_capacity(channels.len());
    for channel in channels {
        channel_rows.push(vec![
            required(channel, "name")?.into(),
            channel_state(channel)?.into(),
            number(channel, "floor")?,
            number(channel, "head")?,
        ]);
    }
    for (title, empty, headers, rows) in [
        (
            "Participants",
            "No participants.",
            if format.interactive() {
                &["Name", "State", "Connections", "ID"][..]
            } else {
                &["Name", "ID", "State", "Connections"][..]
            },
            person_rows,
        ),
        (
            "Channels",
            "No channels.",
            if format.interactive() {
                &["Name", "State", "First kept", "Next position"][..]
            } else {
                &["Channel", "State", "First kept", "Next position"][..]
            },
            channel_rows,
        ),
    ] {
        if format.interactive() {
            format.section(&mut block, title);
            if rows.is_empty() {
                format.paragraph(&mut block, "  ", empty);
            }
        } else {
            block.push('\n');
            block.push_str(&format.details(if rows.is_empty() { empty } else { title }, &[]));
        }
        if !rows.is_empty() {
            block.push_str(&format.name_table(headers, &rows, |name| {
                title == "Channels"
                    && name
                        .strip_prefix("to/")
                        .is_some_and(|id| id.parse::<ParticipantId>().is_ok())
            }));
        }
    }
    output::write(&block)
}

fn participant_details(format: &output::Format, person: &Value, now: u64) -> Result<String, Error> {
    let connections = number(person, "connections")?;
    let grant = &person["grant"];
    let rules = grant[1].as_array().map_or_else(String::new, |rules| {
        rules
            .iter()
            .filter_map(|rule| {
                let pattern = rule[0].as_str()?;
                let mask = rule[1].as_u64()?;
                let actions = ["send", "read", "get", "ack", "create"]
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, action)| action)
                    .collect::<Vec<_>>()
                    .join(",");
                Some(format!("{pattern}:{actions}"))
            })
            .collect::<Vec<_>>()
            .join("  ")
    });
    let enrolled = output::timestamp(person["enrolled_at_ms"].as_u64(), now);
    let changed = output::timestamp(person["grant_changed_at_ms"].as_u64(), now);
    let mut fields = vec![
        ("ID", required(person, "id")?),
        ("State", person_state(person, now)),
        ("Connections", &connections),
        (
            "Grant",
            if rules.is_empty() {
                "No channel access"
            } else {
                &rules
            },
        ),
        ("Enrolled", &enrolled),
        ("Grant changed", &changed),
    ];
    let revoked = output::timestamp(person["revoked_at_ms"].as_u64(), now);
    if !person["revoked_at_ms"].is_null() {
        fields.push(("Revoked", &revoked));
    }
    let ends = output::timestamp(grant_end(person), now);
    if !grant[0].is_null() {
        fields.push(("Grant ends", &ends));
    }
    if let Some(fingerprint) = person["fingerprint"].as_str() {
        fields.push(("Fingerprint", fingerprint));
    }
    Ok(format.screen(
        &format!("Participant {}", required(person, "name")?),
        "",
        &fields,
    ))
}

pub(super) fn grant_end(person: &Value) -> Option<u64> {
    let stamp = &person["grant"][0];
    if stamp[0].as_bool()? {
        return Some(0);
    }
    stamp[1]
        .as_u64()?
        .checked_mul(1000)?
        .checked_add(stamp[2].as_u64()? / 1_000_000)
}

fn person_state(person: &Value, now: u64) -> &'static str {
    if person["revoked"].as_bool() == Some(true) {
        "Revoked"
    } else if grant_end(person).is_some_and(|end| end <= now) {
        "Grant expired"
    } else {
        "Active"
    }
}

fn channel_state(channel: &Value) -> Result<&'static str, Error> {
    Ok(if boolean(channel, "stopped")? {
        "Stopped (damage)"
    } else {
        "Open"
    })
}

fn channel_details(format: &output::Format, channel: &Value) -> Result<String, Error> {
    let floor = number(channel, "floor")?;
    let head = number(channel, "head")?;
    let mut block = format.screen(
        &format!("Channel {}", required(channel, "name")?),
        "",
        &[
            ("State", channel_state(channel)?),
            ("Creation", required(channel, "creation")?),
            ("First kept", &floor),
            ("Next position", &head),
        ],
    );
    if let Some(readers) = channel["readers"]
        .as_object()
        .filter(|readers| !readers.is_empty())
    {
        let rows = readers
            .iter()
            .map(|(id, position)| vec![id.clone(), position.to_string()])
            .collect::<Vec<_>>();
        if format.interactive() {
            format.section(&mut block, "Readers");
        } else {
            block.push('\n');
        }
        block.push_str(&format.name_table(&["Reader", "Acknowledged through"], &rows, |_| true));
    }
    Ok(block)
}

fn number(value: &Value, key: &str) -> Result<String, Error> {
    value[key]
        .as_u64()
        .map(|number| number.to_string())
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn revoke_refuses_an_ambiguous_name_and_lists_the_active_ids() {
        let people = [
            json!({"name": "worker", "id": "previous", "revoked": true}),
            json!({"name": "worker", "id": "active02", "revoked": false}),
            json!({"name": "worker", "id": "active01", "revoked": false}),
        ];

        let error = revoke_id(&people, "worker").unwrap_err();

        assert_eq!(error.outcome(), malak::Outcome::Refused);
        assert!(error.to_string().contains("active01, active02"));
        assert!(!error.to_string().contains("previous"));
    }
}
