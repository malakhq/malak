use super::{
    CALL_TIMEOUT, Configured, EventName, Format, acknowledge_pending, board_call, invalid, offer,
};
use crate::cli::{config::io_error, output, participant, presentation};
use malak::{ChannelName, Error, Kind, Outcome};
use malak_core::HandoverFile;
use std::io;
use std::path::PathBuf;

const PROBE_SESSION: &str = "malak-hook-check";
const PROBE_PREFIX: &str = "Malak hook self-test ";

pub(super) fn run(format: &dyn Format, config: Option<PathBuf>) -> Result<(), Error> {
    let configured = Configured::read(config)?;
    let inbox: ChannelName = configured.inbox.parse()?;
    let handover = HandoverFile::read(&configured.handover)?;
    let id = handover.id();
    let channel = if id.is_lead() {
        "selftest/lead-hook".to_owned()
    } else {
        format!("selftest/{id}")
    };
    if channel == inbox.as_str() {
        return Err(Error::refused(
            "MALAK_HOOK_INBOX must be separate from the hook self-test channel",
        ));
    }
    let pending = configured
        .pending(&channel, PROBE_SESSION)
        .map_err(io_error)?;
    // Restore a check interrupted after showing, but before acknowledging, a
    // probe. A refused receipt can refer to an older creation of this channel.
    if let Err(error) = acknowledge_pending(&configured, &pending, &channel, CALL_TIMEOUT, false)
        .map_err(io_error)?
    {
        if error.outcome() != Outcome::Refused {
            return Err(error);
        }
        pending.remove().map_err(io_error)?;
    }

    let nonce = participant::random_id()?;
    let body = format!("{PROBE_PREFIX}{nonce}");
    let result = board_call(CALL_TIMEOUT, {
        let channel = channel.clone();
        let body = body.clone();
        let config = configured.config.clone();
        move || {
            let mut session = participant::Session::open(config)?;
            participant::Command::Send {
                channel,
                id: Some(nonce),
                kind: Kind::Note,
                reply_to: Vec::new(),
                content_type: None,
                max_bytes: None,
                trace: None,
                json: false,
                body: None,
            }
            .execute_tool(&mut session, Some(body.into_bytes()))
        }
    })
    .map_err(io_error)?;
    let sent = result.map_err(|error| {
        if error.outcome() == Outcome::Refused {
            let mut reason = format!(
                "Could not send the hook probe to {channel} ({error}). The owner must provision it with `malak channel create {channel}`."
            );
            if !id.is_lead() {
                reason.push_str(&format!(
                    " Include `{channel}:send,read,ack` when running `malak grant {id} <existing grants> {channel}:send,read,ack`."
                ));
            }
            Error::refused(reason)
        } else {
            error
        }
    })?;
    let creation = sent["creation"].as_str().ok_or_else(missing_probe)?;
    let position = sent["position"].as_u64().ok_or_else(missing_probe)?;

    let input = format.input(EventName::Step, PROBE_SESSION).to_string();
    let event = format
        .read(&mut input.as_bytes())
        .map_err(io_error)?
        .ok_or_else(|| Error::refused("the hook format ignored the probe event"))?;
    loop {
        let shown = offer(&configured, &channel, &pending, |message, context| {
            let (_, actual_creation, actual_position) = message.address();
            let probe = probe_body(message, id.as_str(), body.len())?;
            if actual_creation != creation
                || actual_position > position
                || (actual_position == position && probe != body)
            {
                return Err(invalid("unexpected message on the hook self-test channel"));
            }
            let mut output = Vec::new();
            format.write(&mut output, event.name, context)?;
            if !output
                .windows(probe.len())
                .any(|part| part == probe.as_bytes())
            {
                return Err(invalid("the hook output did not contain the probe"));
            }
            Ok(actual_position == position)
        })
        .map_err(io_error)??;
        let arrived = shown.ok_or_else(missing_probe)?;
        acknowledge_pending(&configured, &pending, &channel, CALL_TIMEOUT, false)
            .map_err(io_error)??;
        if arrived {
            break;
        }
    }
    output::details(
        "Hook probe received and acknowledged.",
        &[
            ("Channel", &channel),
            (
                "Next",
                "Trigger a hook event in your agent tool to verify its configuration.",
            ),
        ],
    )
}

fn probe_body(
    message: &presentation::Message,
    id: &str,
    expected_len: usize,
) -> io::Result<String> {
    let view = presentation::structured(std::slice::from_ref(message), 0, presentation::BUDGET)
        .map_err(io::Error::other)?;
    let body = view.values.first().and_then(|value| {
        let body = value["body"].as_str()?;
        let nonce = body.strip_prefix(PROBE_PREFIX)?;
        (value["sender"] == id
            && value["grade"] == "proven"
            && value["kind"] == "note"
            && value["body_bytes"].as_u64() == Some(expected_len as u64)
            && body.len() == expected_len
            && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| body.to_owned())
    });
    body.ok_or_else(|| invalid("unexpected message on the hook self-test channel"))
}

fn missing_probe() -> Error {
    Error::unavailable("the hook did not receive the probe it sent")
}
