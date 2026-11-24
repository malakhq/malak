//! Tests for the caller-supplied part of a message.

use malak_core::{CreationId, Grade, HeaderValue, Kind, Limits, Outcome, SendRequest};

fn note_with_headers<'a>(headers: &'a [(&'a str, HeaderValue<'a>)]) -> SendRequest<'a> {
    SendRequest {
        headers,
        ..SendRequest::note("jobs", "message-1", b"")
    }
}

#[test]
fn sender_only_supplies_message_content() {
    let body = b"\0\xff\r\n";
    let request = SendRequest::note("jobs", "message-1", body);
    let checked = request.validate(&Limits::default(), Grade::Proven).unwrap();
    assert_eq!(checked.kind, Kind::Note);
    assert!(std::ptr::eq(checked.body, body.as_slice()));
    assert!(checked.headers.is_empty());
}

#[test]
fn link_only_participant_cannot_send_a_task() {
    let request = SendRequest {
        kind: Kind::Task,
        ..SendRequest::note("jobs", "message-1", b"")
    };
    let error = request
        .validate(&Limits::default(), Grade::LinkOnly)
        .unwrap_err();
    assert_eq!(error.outcome(), Outcome::Refused);
}

#[test]
fn body_beyond_the_limit_is_refused() {
    let mut limits = Limits::default();
    limits.body_bytes = 3;
    let request = SendRequest::note("jobs", "message-1", b"ABCD");
    assert_eq!(
        request
            .validate(&limits, Grade::Proven)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn header_count_beyond_the_limit_is_refused() {
    let mut limits = Limits::default();
    limits.header_entries = 1;
    let input = [
        ("x-a", HeaderValue::Text("ok")),
        ("x-b", HeaderValue::Text("ok")),
    ];
    assert_eq!(
        note_with_headers(&input)
            .validate(&limits, Grade::Proven)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn header_bytes_count_keys_and_values() {
    let mut limits = Limits::default();
    limits.header_bytes = 4;
    let input = [("x-a", HeaderValue::Text("ok"))];
    assert_eq!(
        note_with_headers(&input)
            .validate(&limits, Grade::Proven)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn header_keys_are_unique() {
    let input = [
        ("x-a", HeaderValue::Text("one")),
        ("x-a", HeaderValue::Text("two")),
    ];
    assert!(
        note_with_headers(&input)
            .validate(&Limits::default(), Grade::Proven)
            .is_err()
    );
}

#[test]
fn header_order_is_independent_of_input_order() {
    let input = [
        ("x-z", HeaderValue::Text("one")),
        ("x-a", HeaderValue::Text("two")),
    ];
    let content = note_with_headers(&input)
        .validate(&Limits::default(), Grade::Proven)
        .unwrap();
    assert_eq!(content.headers[0].key(), "x-a");
}

#[test]
fn extension_headers_require_printable_text() {
    for text in ["\u{1b}", "x\r\ny", "x\u{200b}y"] {
        let input = [("x-label", HeaderValue::Text(text))];
        assert!(
            note_with_headers(&input)
                .validate(&Limits::default(), Grade::Proven)
                .is_err(),
            "{text:?}"
        );
    }
}

#[test]
fn extension_headers_accept_printable_unicode() {
    let input = [("x-label", HeaderValue::Text("café \\u{literal}"))];
    assert!(
        note_with_headers(&input)
            .validate(&Limits::default(), Grade::Proven)
            .is_ok()
    );
}

#[test]
fn content_type_requires_a_media_type() {
    let valid = [(
        "content_type",
        HeaderValue::Text("text/plain; charset=utf-8"),
    )];
    assert!(
        note_with_headers(&valid)
            .validate(&Limits::default(), Grade::Proven)
            .is_ok()
    );
    for text in ["plain", "text/*", "text/plain\n"] {
        let input = [("content_type", HeaderValue::Text(text))];
        assert!(
            note_with_headers(&input)
                .validate(&Limits::default(), Grade::Proven)
                .is_err(),
            "{text:?}"
        );
    }
}

#[test]
fn trace_accepts_only_bounded_hexadecimal() {
    for text in ["zz", &"a".repeat(65)] {
        let input = [("trace", HeaderValue::Text(text))];
        assert!(
            note_with_headers(&input)
                .validate(&Limits::default(), Grade::Proven)
                .is_err(),
            "{text:?}"
        );
    }
}

#[test]
fn reply_to_requires_a_valid_channel_and_position() {
    let creation = CreationId::from_bytes([1; 16]);
    for (channel, position) in [("../jobs", 1), ("jobs", 0)] {
        let input = [(
            "reply_to",
            HeaderValue::ReplyTo {
                channel,
                creation,
                position,
            },
        )];
        assert!(
            note_with_headers(&input)
                .validate(&Limits::default(), Grade::Proven)
                .is_err()
        );
    }
}
