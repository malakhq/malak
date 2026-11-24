use crate::encoding::{Field, encode};
use crate::{Frame, FramingError};
use bytes::Bytes;
use minicbor::Decoder;

/// Payload of a reply to a participant proof round.
pub enum AttachResponse<'a> {
    /// The mechanism needs a further round.
    Step(&'a [u8]),
    /// Attachment succeeded. Carries the configured silence time in seconds.
    Accepted(u64),
    /// Proof succeeded and every attachment place is occupied.
    Unavailable,
}

/// Extract the ID claimed in a well-formed attachment frame.
///
/// Identity and grant checks remain the board's responsibility.
pub fn attach_id(frame: &Frame) -> Result<&str, FramingError> {
    if frame.op() != "ATTACH" {
        return Err(FramingError::Header);
    }
    let mut decoder = Decoder::new(frame.header());
    for _ in 0..decoder.map()?.expect("validated header map") {
        if decoder.str()? == "id" {
            return Ok(decoder.str()?);
        }
        decoder.skip()?;
    }
    Err(FramingError::Header)
}

/// Encode the first proof request and its optional mechanism bytes.
pub fn attach_request(rid: u64, id: &str, body: &[u8]) -> Result<Bytes, FramingError> {
    encode(rid, "ATTACH", &[("id", Field::Text(id))], body)
}

/// Encode the caller's next proof request after a STEP reply.
pub fn attach_step(rid: u64, body: &[u8]) -> Result<Bytes, FramingError> {
    encode(rid, "STEP", &[], body)
}

/// Encode a board reply. A capacity refusal includes no reason text.
pub fn attach_reply(rid: u64, response: AttachResponse<'_>) -> Result<Bytes, FramingError> {
    match response {
        AttachResponse::Step(body) => encode(rid, "STEP", &[], body),
        AttachResponse::Accepted(silence) => {
            encode(rid, "OK", &[("silence", Field::Number(silence))], &[])
        }
        AttachResponse::Unavailable => {
            encode(rid, "ERR", &[("outcome", Field::Text("unavailable"))], &[])
        }
    }
}
