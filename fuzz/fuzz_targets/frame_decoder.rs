#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use malak_proto::{
    Attachment, Connection, ConnectionLimits, FrameDecoder, FramingError, Input, Operation, Phase,
    encode_frame,
};
use std::time::Duration;

type Parts = (u64, String, Vec<u8>, Vec<u8>);

fuzz_target!(|data: &[u8]| {
    let Some((&control, data)) = data.split_first() else {
        return;
    };
    let stream = Bytes::copy_from_slice(data);
    let budget = (control & 1 == 0).then_some(32 * 1024);
    let whole = decode(&stream, stream.len().max(1), budget);
    let fragmented = decode(&stream, usize::from(control / 2) + 1, budget);
    assert_eq!(whole, fragmented);
    drive_connection(stream, control);
});

fn decode(
    stream: &Bytes,
    width: usize,
    budget: Option<usize>,
) -> (Vec<Parts>, Option<FramingError>, usize) {
    let mut decoder = FrameDecoder::new(1024 * 1024, budget).unwrap();
    let mut frames = Vec::new();
    let mut consumed = 0;
    for offset in (0..stream.len()).step_by(width) {
        let mut input = stream.slice(offset..(offset + width).min(stream.len()));
        while !input.is_empty() {
            let before = input.len();
            let result = decoder.pull(&mut input);
            consumed += before - input.len();
            match result {
                Ok(Some(frame)) => {
                    assert!(input.len() < before);
                    frames.push((
                        frame.rid(),
                        frame.op().to_owned(),
                        frame.header().to_vec(),
                        frame.body().to_vec(),
                    ));
                }
                Ok(None) => assert!(input.is_empty()),
                Err(error) => {
                    assert!(matches!(
                        decoder.pull(&mut input),
                        Err(FramingError::Closed)
                    ));
                    return (frames, Some(error), consumed);
                }
            }
        }
    }
    (frames, None, consumed)
}

fn drive_connection(mut stream: Bytes, control: u8) {
    let mut connection = Connection::new(ConnectionLimits::default()).unwrap();
    let now = Duration::ZERO;
    if control & 2 != 0 {
        connection.handshake_reply_sent(&[0; 32], now).unwrap();
        if control & 4 != 0 {
            let mut start = encode_frame(1, "ATTACH", 0, |_| {}, &[]).unwrap();
            let Input::Request(attach) = connection.receive(&mut start, now).unwrap() else {
                panic!("valid attachment request did not reach the mechanism");
            };
            let reply = if control & 8 == 0 {
                Attachment::Accepted
            } else {
                Attachment::More
            };
            connection
                .attachment_reply_sent(attach.ticket, reply, now)
                .unwrap();
        }
    }

    while !stream.is_empty() {
        let phase = connection.phase();
        let before = stream.len();
        let result = connection.receive(&mut stream, now);
        match result {
            Err(_) => {
                assert_eq!(connection.phase(), Phase::Closed);
                assert!(connection.receive(&mut stream, now).is_err());
                return;
            }
            Ok(Input::Partial) => {
                assert!(stream.is_empty());
                assert_eq!(connection.phase(), phase);
            }
            Ok(Input::Paused) => {
                assert_eq!(phase, Phase::Attached);
                assert_eq!(connection.phase(), phase);
                assert!(!connection.can_read());
                return;
            }
            Ok(Input::Request(request)) if phase == Phase::Attaching => {
                assert!(stream.len() < before);
                assert!(matches!(
                    request.operation,
                    Operation::Attach | Operation::Step
                ));
                assert_eq!(connection.phase(), Phase::Attaching);
                let reply = if control & 16 == 0 {
                    Attachment::Accepted
                } else {
                    Attachment::Unavailable
                };
                connection
                    .attachment_reply_sent(request.ticket, reply, now)
                    .unwrap();
            }
            Ok(Input::Request(_) | Input::Unavailable(_)) => {
                assert!(stream.len() < before);
                assert_eq!(phase, Phase::Attached);
                assert_eq!(connection.phase(), phase);
            }
        }
    }
}
