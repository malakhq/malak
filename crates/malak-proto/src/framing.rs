use crate::{OPENING_BYTES, REPLY_BYTES, TAG_BYTES};
use bytes::{Buf, Bytes, BytesMut};
use minicbor::{Decoder, data::Type};
use std::collections::HashSet;
use std::fmt;
use std::ops::Range;

/// Largest encoded CBOR header allowed by version 1.
pub const MAX_HEADER: usize = 16 * 1024;
const MAX_DEPTH: u8 = 4;
const FRAME_PREFIX: usize = 6;
const LENGTH_BYTES: usize = 4;

/// Maximum complete frame size for the configured body size.
///
/// Returns `None` if the four-byte length field cannot describe this size.
pub fn max_frame_bytes(max_body: usize) -> Option<usize> {
    let total = max_body.checked_add(MAX_HEADER + FRAME_PREFIX)?;
    u32::try_from(total - LENGTH_BYTES).ok()?;
    Some(total)
}

/// An invalid or disallowed frame or segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramingError {
    /// The announced length or supplied buffer cannot hold a valid item.
    Length,
    /// The CBOR header does not meet the protocol's shape rules.
    Header,
    /// The frame would pass the state-specific byte budget.
    Budget,
    /// An earlier error closed the decoder.
    Closed,
}

impl fmt::Display for FramingError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Length => "invalid wire length",
            Self::Header => "invalid wire header",
            Self::Budget => "wire frame exceeds the byte budget",
            Self::Closed => "wire decoder is closed",
        };
        output.write_str(reason)
    }
}

impl std::error::Error for FramingError {}

/// Size rules for the next encrypted or handshake segment.
#[derive(Clone, Copy, Debug)]
pub enum SegmentKind {
    /// The caller's fixed-size Noise opening.
    Opening,
    /// The board's fixed-size Noise reply.
    Reply,
    /// Nonempty encrypted content and its authentication tag.
    Protected,
}

impl SegmentKind {
    fn accepts(self, length: usize) -> bool {
        match self {
            Self::Opening => length == OPENING_BYTES,
            Self::Reply => length == REPLY_BYTES,
            Self::Protected => length > TAG_BYTES,
        }
    }
}

/// Incrementally extracts segments. The connection supplies the current size rule.
#[derive(Default)]
pub struct SegmentDecoder {
    prefix: [u8; 2],
    prefix_used: usize,
    partial: BytesMut,
    closed: bool,
}

impl SegmentDecoder {
    /// Build the two-byte length prefix for the given segment.
    pub fn prefix(kind: SegmentKind, segment: &[u8]) -> Result<[u8; 2], FramingError> {
        if !kind.accepts(segment.len()) {
            return Err(FramingError::Length);
        }
        Ok(u16::try_from(segment.len())
            .map_err(|_| FramingError::Length)?
            .to_be_bytes())
    }

    /// Consume enough input for one segment, or retain a partial segment.
    ///
    /// Call with the same kind until the partially read segment completes.
    pub fn pull(
        &mut self,
        kind: SegmentKind,
        input: &mut Bytes,
    ) -> Result<Option<Bytes>, FramingError> {
        if self.closed {
            return Err(FramingError::Closed);
        }
        if !take_prefix(&mut self.prefix, &mut self.prefix_used, input) {
            return Ok(None);
        }
        let length = u16::from_be_bytes(self.prefix) as usize;
        if !kind.accepts(length) {
            self.closed = true;
            return Err(FramingError::Length);
        }
        let segment = take_payload(&mut self.partial, length, input);
        if segment.is_some() {
            self.prefix_used = 0;
        }
        Ok(segment)
    }
}

fn take_prefix(prefix: &mut [u8], used: &mut usize, input: &mut Bytes) -> bool {
    let amount = (prefix.len() - *used).min(input.len());
    prefix[*used..*used + amount].copy_from_slice(&input[..amount]);
    *used += amount;
    input.advance(amount);
    *used == prefix.len()
}

fn take_payload(partial: &mut BytesMut, length: usize, input: &mut Bytes) -> Option<Bytes> {
    if partial.is_empty() && input.len() >= length {
        return Some(input.split_to(length));
    }
    let amount = (length - partial.len()).min(input.len());
    partial.extend_from_slice(&input[..amount]);
    input.advance(amount);
    (partial.len() == length).then(|| partial.split().freeze())
}

/// A complete frame with a validated CBOR header and an opaque body.
#[derive(Debug)]
pub struct Frame {
    data: Bytes,
    header_len: usize,
    op: Range<usize>,
    rid: u64,
}

impl Frame {
    /// Complete CBOR header, including any extension keys.
    pub fn header(&self) -> &[u8] {
        &self.data[..self.header_len]
    }

    /// Opaque body bytes without the lengths or header.
    pub fn body(&self) -> &[u8] {
        &self.data[self.header_len..]
    }

    /// Operation as written by the sender.
    pub fn op(&self) -> &str {
        std::str::from_utf8(&self.data[self.op.clone()]).expect("validated CBOR text")
    }

    /// Connection-local request number.
    pub const fn rid(&self) -> u64 {
        self.rid
    }
}

/// Incrementally extracts frames from ordered, decrypted segment contents.
pub struct FrameDecoder {
    limit: usize,
    frame_limit: usize,
    remaining_budget: Option<usize>,
    prefix: [u8; FRAME_PREFIX],
    prefix_used: usize,
    announced: Option<usize>,
    partial: BytesMut,
    closed: bool,
}

impl FrameDecoder {
    /// Set the body limit and an optional aggregate byte budget, including prefixes.
    ///
    /// The attach state uses a budget. Ordinary attached requests are limited
    /// individually and by their connection's outstanding places.
    pub fn new(max_body: usize, budget: Option<usize>) -> Result<Self, FramingError> {
        let frame_limit = max_frame_bytes(max_body).ok_or(FramingError::Length)?;
        Ok(Self {
            limit: max_body,
            frame_limit,
            remaining_budget: budget,
            prefix: [0; FRAME_PREFIX],
            prefix_used: 0,
            announced: None,
            partial: BytesMut::new(),
            closed: false,
        })
    }

    /// Limit the complete frame, including length and header bytes.
    pub fn with_frame_limit(max_bytes: usize) -> Result<Self, FramingError> {
        if !(FRAME_PREFIX + 1..=u32::MAX as usize).contains(&max_bytes) {
            return Err(FramingError::Length);
        }
        // The complete length check makes this an upper bound for the body.
        let mut decoder = Self::new(max_bytes.saturating_sub(MAX_HEADER + FRAME_PREFIX), None)?;
        decoder.limit = max_bytes - FRAME_PREFIX - 1;
        decoder.frame_limit = max_bytes;
        Ok(decoder)
    }

    /// Consume enough input for one complete frame, leaving later bytes in `input`.
    ///
    /// Lengths and the state budget are checked as their bytes arrive, before
    /// buffering any payload. An invalid frame closes this decoder.
    pub fn pull(&mut self, input: &mut Bytes) -> Result<Option<Frame>, FramingError> {
        if self.closed {
            return Err(FramingError::Closed);
        }
        let result = self.read_frame(input);
        self.closed = result.is_err();
        result
    }

    fn read_frame(&mut self, input: &mut Bytes) -> Result<Option<Frame>, FramingError> {
        let length = match self.announced {
            Some(length) => length,
            None => {
                if !take_prefix(
                    &mut self.prefix[..LENGTH_BYTES],
                    &mut self.prefix_used,
                    input,
                ) {
                    return Ok(None);
                }
                let length =
                    u32::from_be_bytes(self.prefix[..LENGTH_BYTES].try_into().unwrap()) as usize;
                let maximum = self.frame_limit - LENGTH_BYTES;
                if !(3..=maximum).contains(&length) {
                    return Err(FramingError::Length);
                }
                if let Some(budget) = &mut self.remaining_budget {
                    *budget = budget
                        .checked_sub(length + LENGTH_BYTES)
                        .ok_or(FramingError::Budget)?;
                }
                self.announced = Some(length);
                length
            }
        };

        if !take_prefix(&mut self.prefix, &mut self.prefix_used, input) {
            return Ok(None);
        }
        let header_len =
            u16::from_be_bytes(self.prefix[LENGTH_BYTES..].try_into().unwrap()) as usize;
        let payload_len = length - 2;
        if !(1..=MAX_HEADER.min(payload_len)).contains(&header_len)
            || payload_len - header_len > self.limit
        {
            return Err(FramingError::Length);
        }
        let Some(data) = take_payload(&mut self.partial, payload_len, input) else {
            return Ok(None);
        };

        let (op, rid) = parse_header(&data[..header_len])?;
        self.prefix_used = 0;
        self.announced = None;
        Ok(Some(Frame {
            data,
            header_len,
            op,
            rid,
        }))
    }
}

/// Encode the six length bytes without copying a header or body.
pub fn frame_prefix(header_len: usize, body_len: usize) -> Result<[u8; 6], FramingError> {
    if !(1..=MAX_HEADER).contains(&header_len) {
        return Err(FramingError::Length);
    }
    let total = (header_len + 2)
        .checked_add(body_len)
        .ok_or(FramingError::Length)?;
    let length = u32::try_from(total).map_err(|_| FramingError::Length)?;
    let mut prefix = [0; FRAME_PREFIX];
    prefix[..LENGTH_BYTES].copy_from_slice(&length.to_be_bytes());
    prefix[LENGTH_BYTES..].copy_from_slice(&(header_len as u16).to_be_bytes());
    Ok(prefix)
}

fn parse_header(input: &[u8]) -> Result<(Range<usize>, u64), FramingError> {
    let mut decoder = Decoder::new(input);
    let mut op = None;
    let mut rid = None;
    visit_map(&mut decoder, |key, decoder| {
        match key {
            "op" => {
                let text = decoder.str()?.len();
                let end = decoder.position();
                op = Some(end - text..end);
            }
            "rid" => rid = Some(decoder.u64()?),
            _ => visit_value(decoder, 2)?,
        }
        Ok(())
    })?;
    if decoder.position() != input.len() {
        return Err(FramingError::Header);
    }
    op.zip(rid).ok_or(FramingError::Header)
}

impl From<minicbor::decode::Error> for FramingError {
    fn from(_: minicbor::decode::Error) -> Self {
        Self::Header
    }
}

fn visit_map<'a>(
    decoder: &mut Decoder<'a>,
    mut visit: impl FnMut(&'a str, &mut Decoder<'a>) -> Result<(), FramingError>,
) -> Result<(), FramingError> {
    let count = decoder.map()?.ok_or(FramingError::Header)?;
    const INLINE_KEYS: usize = 12;
    let mut inline = [""; INLINE_KEYS];
    let mut many = (count > INLINE_KEYS as u64).then(HashSet::new);

    for index in 0..count {
        let key = decoder.str()?;
        if let Some(keys) = &mut many {
            if !keys.insert(key) {
                return Err(FramingError::Header);
            }
        } else {
            let index = index as usize;
            if inline[..index].contains(&key) {
                return Err(FramingError::Header);
            }
            inline[index] = key;
        }
        visit(key, decoder)?;
    }
    Ok(())
}

fn visit_value(decoder: &mut Decoder<'_>, depth: u8) -> Result<(), FramingError> {
    match decoder.datatype()? {
        Type::Map if depth <= MAX_DEPTH => {
            visit_map(decoder, |_, decoder| visit_value(decoder, depth + 1))?;
        }
        Type::Array if depth <= MAX_DEPTH => {
            for _ in 0..decoder.array()?.ok_or(FramingError::Header)? {
                visit_value(decoder, depth + 1)?;
            }
        }
        Type::Bytes => {
            decoder.bytes()?;
        }
        Type::String => {
            decoder.str()?;
        }
        Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::Int => {
            decoder.int()?;
        }
        Type::Bool => {
            decoder.bool()?;
        }
        Type::Null => {
            decoder.null()?;
        }
        Type::Undefined => {
            decoder.undefined()?;
        }
        Type::Simple => {
            let start = decoder.position();
            let value = decoder.simple()?;
            if value < 32 && decoder.position() - start > 1 {
                return Err(FramingError::Header);
            }
        }
        _ => return Err(FramingError::Header),
    }
    Ok(())
}
