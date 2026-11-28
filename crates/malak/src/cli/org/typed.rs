//! Typed conventions count only for the named, proven writer.

use serde_json::Value;
use std::collections::BTreeMap;

pub(in crate::cli) fn text(record: &Value) -> String {
    record["body"]
        .as_array()
        .map(|bytes| {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|v| v.as_u64().and_then(|n| n.try_into().ok()))
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        })
        .unwrap_or_default()
}

pub(in crate::cli) fn fields(
    record: &Value,
    expected_type: &str,
    sender: &str,
) -> Option<BTreeMap<String, String>> {
    if record["sender"] != sender || record["grade"] != "proven" || record["kind"] != "note" {
        return None;
    }
    let text = text(record);
    let mut lines = text.lines();
    if lines.next()? != format!("Type: {expected_type}") {
        return None;
    }
    let mut fields = BTreeMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((key, value)) = line.split_once(": ") else {
            break;
        };
        if fields.insert(key.to_owned(), value.to_owned()).is_some() {
            return None;
        }
    }
    Some(fields)
}

pub(in crate::cli) fn at_ms(record: &Value) -> u64 {
    record["sent_at"][0]
        .as_u64()
        .unwrap_or(0)
        .saturating_mul(1000)
        .saturating_add(record["sent_at"][1].as_u64().unwrap_or(0) / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unproven_type_line_remains_text() {
        let record = json!({"sender":"lead", "grade":"link_only", "kind":"note", "body": b"Type: rail\nEvent: started"});
        assert!(fields(&record, "rail", "lead").is_none());
        assert_eq!(text(&record), "Type: rail\nEvent: started");
    }

    #[test]
    fn a_type_line_from_another_writer_remains_text() {
        let record = json!({"sender":"worker", "grade":"proven", "kind":"note", "body": b"Type: rail\nEvent: started"});
        assert!(fields(&record, "rail", "lead").is_none());
    }
}
