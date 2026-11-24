#![no_main]

use libfuzzer_sys::fuzz_target;
use malak_layers::{Position, Record};

#[path = "../../crates/malak-store-markdown/src/format.rs"]
mod format;

fuzz_target!(|data: &[u8]| {
    let mut records = format::Records::new(data);
    let mut end = 0;
    let mut last = None;
    while let Ok(Some((record, range))) = records.next() {
        assert_eq!(range.start, end);
        assert!(range.start < range.end && range.end <= data.len());
        assert!(last.is_none_or(|previous| previous < record.position));
        let (whole, len) = format::decode(&data[range.clone()]).unwrap();
        assert_eq!(len, range.len());
        assert_eq!(whole.position, record.position);
        assert_eq!(whole.data, record.data);
        end = range.end;
        last = Some(record.position);
    }
    assert_eq!(records.last(), last);

    for (offset, word) in data.windows(format::MARKER.len()).enumerate() {
        if word == format::MARKER
            && let Some((record, len)) = format::decode(&data[offset..])
        {
            assert!(len >= format::MARKER.len() && len <= data.len() - offset);
            round_trip(record);
        }
    }
    round_trip(Record {
        position: Position::FIRST,
        data: data.to_vec().into(),
    });

    if let Ok(name) = std::str::from_utf8(data)
        && let Some(encoded) = format::file_name(name)
    {
        assert_eq!(format::channel_name(&encoded).as_deref(), Some(name));
    }
});

fn round_trip(record: Record) {
    let mut encoded = Vec::new();
    format::encode(&record, &mut encoded);
    let (decoded, consumed) = format::decode(&encoded).unwrap();
    assert_eq!(record.position, decoded.position);
    assert_eq!(record.data, decoded.data);
    assert_eq!(encoded.len(), consumed);
}
