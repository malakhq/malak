use crate::{FramingError, frame_prefix};
use bytes::Bytes;
use minicbor::Encoder;

pub(crate) enum Field<'a> {
    Text(&'a str),
    Number(u64),
}

pub(crate) fn encode(
    rid: u64,
    operation: &str,
    fields: &[(&str, Field<'_>)],
    body: &[u8],
) -> Result<Bytes, FramingError> {
    encode_frame(
        rid,
        operation,
        fields.len(),
        |header| {
            for (name, field) in fields {
                header.str(name).unwrap();
                match field {
                    Field::Text(text) => header.str(text).unwrap(),
                    Field::Number(number) => header.u64(*number).unwrap(),
                };
            }
        },
        body,
    )
}

/// Build a frame with its operation, request number, and caller-supplied header fields.
///
/// The field encoder must write exactly `additional` text-keyed values. The
/// protocol frame decoder checks all fields and the lengths at the receiver.
pub fn encode_frame(
    rid: u64,
    operation: &str,
    additional: usize,
    fields: impl FnOnce(&mut Encoder<Vec<u8>>),
    body: &[u8],
) -> Result<Bytes, FramingError> {
    let mut header = Encoder::new(Vec::new());
    header
        .map(2 + additional as u64)
        .unwrap()
        .str("op")
        .unwrap()
        .str(operation)
        .unwrap()
        .str("rid")
        .unwrap()
        .u64(rid)
        .unwrap();
    fields(&mut header);
    let header = header.into_writer();
    let prefix = frame_prefix(header.len(), body.len())?;
    let mut frame = Vec::with_capacity(prefix.len() + header.len() + body.len());
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(body);
    Ok(frame.into())
}
