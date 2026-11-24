//! Frame byte boundaries and strict CBOR header rules.

use bytes::Bytes;
use malak_proto::{
    Frame, FrameDecoder, FramingError, MAX_HEADER, SegmentDecoder, SegmentKind, frame_prefix,
};
use minicbor::Encoder;

fn request_header() -> Vec<u8> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(2)
        .unwrap()
        .str("op")
        .unwrap()
        .str("SEND")
        .unwrap()
        .str("rid")
        .unwrap()
        .u64(7)
        .unwrap();
    encoder.into_writer()
}

fn extended_header(write: impl FnOnce(&mut Encoder<Vec<u8>>)) -> Vec<u8> {
    let mut header = request_header();
    header[0] = 0xa3;
    let mut encoder = Encoder::new(header);
    encoder.str("future").unwrap();
    write(&mut encoder);
    encoder.into_writer()
}

fn frame(header: &[u8], body: &[u8]) -> Bytes {
    let mut bytes = Vec::from(frame_prefix(header.len(), body.len()).unwrap());
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(body);
    bytes.into()
}

fn decode_header(header: &[u8]) -> Result<Option<Frame>, FramingError> {
    FrameDecoder::new(0, None)
        .unwrap()
        .pull(&mut frame(header, b""))
}

#[test]
fn zero_length_segment_is_rejected_at_the_prefix() {
    let mut decoder = SegmentDecoder::default();
    let mut bytes = Bytes::from_static(&[0, 0, 5]);
    assert_eq!(
        decoder.pull(SegmentKind::Opening, &mut bytes),
        Err(FramingError::Length)
    );
    assert_eq!(&bytes[..], &[5]);
}

#[test]
fn opening_segment_is_checked_before_its_payload() {
    let mut decoder = SegmentDecoder::default();
    let mut bytes = Bytes::from_static(&[0, 95, 5]);
    assert_eq!(
        decoder.pull(SegmentKind::Opening, &mut bytes),
        Err(FramingError::Length)
    );
    assert_eq!(&bytes[..], &[5]);
}

#[test]
fn segment_prefix_and_data_can_arrive_in_pieces() {
    let mut decoder = SegmentDecoder::default();
    let kind = SegmentKind::Protected;
    let body = [0xa1; 17];
    for chunk in [&[0u8][..], &[17], &body[..5]] {
        assert_eq!(
            decoder.pull(kind, &mut Bytes::copy_from_slice(chunk)),
            Ok(None)
        );
    }
    let received = decoder
        .pull(kind, &mut Bytes::copy_from_slice(&body[5..]))
        .unwrap()
        .unwrap();
    assert_eq!(&received[..], &body);
}

#[test]
fn oversized_frame_is_rejected_at_the_four_byte_length() {
    let mut decoder = FrameDecoder::new(10, None).unwrap();
    let mut input = Bytes::from_static(&[0xff, 0xff, 0xff, 0xff, 0x00]);
    assert!(matches!(
        decoder.pull(&mut input),
        Err(FramingError::Length)
    ));
    assert_eq!(&input[..], &[0]);
}

#[test]
fn invalid_header_length_is_rejected_before_payload() {
    let mut decoder = FrameDecoder::new(64, None).unwrap();
    let mut input = Bytes::from_static(&[0, 0, 0, 4, 0, 3, 0xab]);
    assert!(matches!(
        decoder.pull(&mut input),
        Err(FramingError::Length)
    ));
    assert_eq!(&input[..], &[0xab]);
}

#[test]
fn body_limit_is_checked_as_soon_as_header_length_arrives() {
    let mut decoder = FrameDecoder::new(2, None).unwrap();
    let mut input = Bytes::from_static(&[0, 0, 0, 6, 0, 1, 0xab]);
    assert!(matches!(
        decoder.pull(&mut input),
        Err(FramingError::Length)
    ));
    assert_eq!(&input[..], &[0xab]);
}

#[test]
fn attaching_budget_charges_the_prefix_and_previous_frames() {
    let message = frame(&request_header(), b"");
    let mut decoder = FrameDecoder::new(0, Some(2 * message.len() - 1)).unwrap();
    assert!(decoder.pull(&mut message.clone()).unwrap().is_some());

    let mut next = message;
    let before = next.len();

    assert!(matches!(decoder.pull(&mut next), Err(FramingError::Budget)));
    assert_eq!(next.len(), before - 4);
}

#[test]
fn a_contiguous_frame_body_is_not_copied() {
    let header = request_header();
    let mut bytes = frame(&header, b"opaque\x00\xff");
    let body_address = bytes.as_ptr().wrapping_add(6 + header.len());
    let mut decoder = FrameDecoder::new(64, None).unwrap();

    let received = decoder.pull(&mut bytes).unwrap().unwrap();

    assert_eq!(received.op(), "SEND");
    assert_eq!(received.rid(), 7);
    assert_eq!(received.body().as_ptr(), body_address);
}

#[test]
fn one_frame_can_span_any_byte_boundary() {
    let message = frame(&request_header(), b"some bytes");
    for split in 1..message.len() {
        let mut first = message.slice(..split);
        let mut second = message.slice(split..);
        let mut decoder = FrameDecoder::new(64, None).unwrap();

        assert!(
            decoder.pull(&mut first).unwrap().is_none(),
            "split at {split}"
        );
        let received = decoder.pull(&mut second).unwrap().unwrap();
        assert_eq!(received.body(), b"some bytes", "split at {split}");
    }
}

#[test]
fn multiple_frames_can_share_one_segment() {
    let header = request_header();
    let first = frame(&header, b"first");
    let second = frame(&header, b"second");
    let mut input = Bytes::from([first.as_ref(), second.as_ref()].concat());
    let mut decoder = FrameDecoder::new(64, None).unwrap();

    assert_eq!(decoder.pull(&mut input).unwrap().unwrap().body(), b"first");
    assert_eq!(decoder.pull(&mut input).unwrap().unwrap().body(), b"second");
    assert!(input.is_empty());
}

#[test]
fn header_requires_op() {
    assert!(matches!(
        decode_header(b"\xa1\x63rid\x07"),
        Err(FramingError::Header)
    ));
}

#[test]
fn header_requires_rid() {
    assert!(matches!(
        decode_header(b"\xa1\x62op\x64SEND"),
        Err(FramingError::Header)
    ));
}

#[test]
fn duplicate_keys_in_nested_maps_are_rejected() {
    let header = extended_header(|e| {
        e.map(2)
            .unwrap()
            .str("a")
            .unwrap()
            .u8(1)
            .unwrap()
            .str("a")
            .unwrap()
            .u8(2)
            .unwrap();
    });
    assert!(matches!(decode_header(&header), Err(FramingError::Header)));
}

#[test]
fn duplicate_top_level_keys_close_the_decoder() {
    let mut header = request_header();
    header[0] = 0xa3;
    header.extend_from_slice(b"\x62op\x64PING");
    let mut decoder = FrameDecoder::new(0, None).unwrap();
    assert!(matches!(
        decoder.pull(&mut frame(&header, b"")),
        Err(FramingError::Header)
    ));
    assert!(matches!(
        decoder.pull(&mut frame(&request_header(), b"")),
        Err(FramingError::Closed)
    ));
}

#[test]
fn header_ignores_unknown_keys_with_valid_values() {
    let header = extended_header(|e| {
        e.array(3)
            .unwrap()
            .bool(false)
            .unwrap()
            .null()
            .unwrap()
            .undefined()
            .unwrap();
    });
    assert_eq!(decode_header(&header).unwrap().unwrap().op(), "SEND");
}

#[test]
fn headers_forbid_indefinite_values_tags_floats_and_nontext_map_keys() {
    for value in [
        b"\x9f\x01\xff".as_slice(),
        b"\xc0\x61x",
        b"\xf9\x00\x00",
        b"\xa1\x01\x02",
        b"\xf8\x17",
    ] {
        let mut header = extended_header(|_| {});
        header.extend_from_slice(value);
        assert!(
            matches!(decode_header(&header), Err(FramingError::Header)),
            "{value:?}"
        );
    }
}

#[test]
fn header_allows_four_container_levels() {
    let header = extended_header(|e| {
        e.array(1)
            .unwrap()
            .array(1)
            .unwrap()
            .array(1)
            .unwrap()
            .u8(0)
            .unwrap();
    });
    assert!(decode_header(&header).unwrap().is_some());
}

#[test]
fn header_rejects_a_fifth_container_level() {
    let header = extended_header(|e| {
        for _ in 0..4 {
            e.array(1).unwrap();
        }
        e.u8(0).unwrap();
    });
    assert!(matches!(decode_header(&header), Err(FramingError::Header)));
}

#[test]
fn full_header_and_full_body_are_accepted_together() {
    let pad = MAX_HEADER - 64;
    let sample = extended_header(|e| {
        e.bytes(&vec![0; pad]).unwrap();
    });
    let pad = pad + (MAX_HEADER - sample.len());
    let header = extended_header(|e| {
        e.bytes(&vec![0; pad]).unwrap();
    });
    assert_eq!(header.len(), MAX_HEADER);
    let body = vec![0xab; 1024 * 1024];
    let mut decoder = FrameDecoder::new(body.len(), None).unwrap();

    let received = decoder.pull(&mut frame(&header, &body)).unwrap().unwrap();

    assert_eq!(received.header(), header);
    assert_eq!(received.body(), body);
}
