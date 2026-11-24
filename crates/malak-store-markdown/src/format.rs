use data_encoding::{BASE32_NOPAD, BASE64, HEXLOWER};
use malak_layers::{Position, Record};
use std::borrow::Cow;
use std::io::{self, Write};

pub(crate) const MAX_RECORD: usize = 16 * 1024 * 1024;
const MAX_VIEW: usize = MAX_RECORD * 3 + 128;
pub(super) const MARKER: &[u8] = b"<!-- malak 1 ";
const HEADER_END: &[u8] = b" -->\n";
const FOOTER: &[u8] = b"\n\n";

pub(crate) fn file_name(channel: &str) -> Option<String> {
    let mut input = Vec::with_capacity(channel.len() + 1);
    if let Some((name, creation)) = channel.rsplit_once("/_")
        && creation.len() == 32
        && let Ok(id) = HEXLOWER.decode(creation.as_bytes())
    {
        input.push(1);
        input.extend_from_slice(name.as_bytes());
        input.extend_from_slice(&id);
    } else {
        input.push(0);
        input.extend_from_slice(channel.as_bytes());
    }

    let encoded = BASE32_NOPAD.encode(&input);
    let name = format!("{}.md", encoded.to_ascii_lowercase());
    (!channel.is_empty() && name.len() + ".next".len() <= 255).then_some(name)
}

pub(super) fn channel_name(name: &str) -> Option<String> {
    let encoded = name.strip_suffix(".md")?;
    let decoded = BASE32_NOPAD
        .decode(encoded.to_ascii_uppercase().as_bytes())
        .ok()?;
    let channel = match decoded.split_first() {
        Some((0, channel)) => String::from_utf8(channel.to_vec()).ok(),
        Some((1, qualified)) if qualified.len() > 16 => {
            let (channel, id) = qualified.split_at(qualified.len() - 16);
            std::str::from_utf8(channel)
                .ok()
                .map(|channel| format!("{channel}/_{}", HEXLOWER.encode(id)))
        }
        _ => None,
    };
    channel.filter(|channel| file_name(channel).as_deref() == Some(name))
}

pub(crate) fn encode(record: &Record, output: &mut Vec<u8>) {
    let body = malak_record::unverified_body(record).unwrap_or(&record.data);
    let (body, language) = match std::str::from_utf8(body) {
        Ok(text) if !body.windows(MARKER.len()).any(|part| part == MARKER) => {
            (Cow::Borrowed(text), "text")
        }
        _ => (Cow::Owned(BASE64.encode(body)), "base64"),
    };
    let fence_len = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        + 1;
    let fence = "`".repeat(fence_len.max(3));
    let view = format!(
        "## {}\n\n{fence}{language}\n{body}\n{fence}",
        record.position
    );
    output.extend_from_slice(MARKER);
    write!(
        output,
        "{} {} {} ",
        record.position,
        record.data.len(),
        view.len()
    )
    .unwrap();
    output.extend_from_slice(BASE64.encode(&record.data).as_bytes());
    output.extend_from_slice(HEADER_END);
    output.extend_from_slice(view.as_bytes());
    output.extend_from_slice(FOOTER);
}

pub(crate) fn decode(bytes: &[u8]) -> Option<(Record, usize)> {
    let mut input = bytes.strip_prefix(MARKER)?;
    let position: u64 = number(&mut input)?;
    let raw_len: usize = number(&mut input)?;
    let view_len: usize = number(&mut input)?;
    if raw_len > MAX_RECORD || view_len > MAX_VIEW {
        return None;
    }
    let (encoded, rest) = input.split_at_checked(BASE64.encode_len(raw_len))?;
    let rest = rest.strip_prefix(HEADER_END)?;
    let rest = rest.get(view_len..)?.strip_prefix(FOOTER)?;
    let record = Record {
        position: Position::try_from(position).ok()?,
        data: BASE64.decode(encoded).ok()?.into(),
    };
    (record.data.len() == raw_len).then_some((record, bytes.len() - rest.len()))
}

fn number<T: std::str::FromStr>(input: &mut &[u8]) -> Option<T> {
    let end = input.iter().take(21).position(|&byte| byte == b' ')?;
    let (digits, rest) = input.split_at(end);
    *input = &rest[1..];
    if digits.is_empty() || (digits[0] == b'0' && end > 1) || !digits.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

pub(crate) struct Records<'a> {
    bytes: &'a [u8],
    offset: usize,
    last: Option<Position>,
}

impl<'a> Records<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            last: None,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<(Record, std::ops::Range<usize>)>, io::Error> {
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        if let Some((record, len)) = decode(&self.bytes[self.offset..])
            && self.last.is_none_or(|last| record.position > last)
        {
            let range = self.offset..self.offset + len;
            self.offset = range.end;
            self.last = Some(record.position);
            return Ok(Some((record, range)));
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unparseable markdown record",
        ))
    }

    pub(crate) fn last(&self) -> Option<Position> {
        self.last
    }
}
