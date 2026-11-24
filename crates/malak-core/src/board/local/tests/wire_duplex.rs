use super::*;
use crate::{SendRequest, link::Segments};
use malak_proto::MAX_CONTENT;
use malak_testkit::tasks;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_large_unix_send_progresses_while_the_client_reads_a_large_reply() {
    tasks::spawn(exercise_duplex)
        .wait(WAIT * 4)
        .expect("full-duplex scenario did not finish");
}

fn exercise_duplex() {
    let mut fixture = Fixture::new(uid());
    let size = Limits::default().body_bytes;
    fixture
        .board
        .lead()
        .send(SendRequest::note("lead", "outgoing", &vec![b'o'; size]))
        .unwrap();
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    fixture.send_protected(&mut caller, &attach_request(1, "lead", &[]).unwrap());
    let attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");
    let server = tasks::spawn(move || attached.serve());

    let get = encode_frame(
        2,
        "GET",
        2,
        |out| {
            out.str("channel").unwrap().str("lead").unwrap();
            out.str("position").unwrap().u64(1).unwrap();
        },
        &[],
    )
    .unwrap();
    fixture.send_protected(&mut caller, &get);

    let caller = Arc::new(Mutex::new(caller));
    let fixture = Arc::new(fixture);
    let (first_received, continue_sending) = mpsc::channel();
    let (first_written, start_reading) = mpsc::channel();
    let writer = tasks::spawn({
        let fixture = fixture.clone();
        let caller = caller.clone();
        move || {
            let send = encode_frame(
                3,
                "SEND",
                2,
                |out| {
                    out.str("channel").unwrap().str("lead").unwrap();
                    out.str("id").unwrap().str("incoming").unwrap();
                },
                &vec![b'i'; size],
            )
            .unwrap();
            let mut chunks = send.chunks(MAX_CONTENT);
            let send_chunk = |chunk: &[u8]| {
                let mut sealed = vec![0; chunk.len() + TAG_BYTES];
                caller.lock().unwrap().seal(chunk, &mut sealed).unwrap();
                fixture.send(SegmentKind::Protected, &sealed);
            };
            send_chunk(chunks.next().unwrap());
            first_written.send(()).unwrap();
            continue_sending.recv_timeout(WAIT).unwrap();
            for chunk in chunks {
                send_chunk(chunk);
            }
        }
    });
    let reader = tasks::spawn({
        let fixture = fixture.clone();
        move || {
            start_reading.recv_timeout(WAIT).unwrap();
            let mut decoder = FrameDecoder::new(size, None).unwrap();
            let mut first = Some(first_received);
            let mut frames = Vec::new();
            while frames.len() < 3 {
                let cipher = fixture.receive_protected_segment();
                let mut plaintext =
                    Segments::decrypt(&mut caller.lock().unwrap(), &cipher).unwrap();
                if let Some(ready) = first.take() {
                    ready.send(()).unwrap();
                }
                while !plaintext.is_empty() {
                    if let Some(frame) = decoder.pull(&mut plaintext).unwrap() {
                        frames.push(frame);
                    }
                }
            }
            frames
        }
    });

    writer.wait(WAIT * 2).expect("large SEND did not finish");
    let frames = reader.wait(WAIT).expect("large reply did not finish");
    assert_eq!((frames[0].op(), frames[0].rid()), ("MSG", 2));
    assert_eq!(frames[0].body(), &vec![b'o'; size]);
    let mut closings = frames[1..]
        .iter()
        .map(|frame| (frame.op(), frame.rid()))
        .collect::<Vec<_>>();
    closings.sort_unstable();
    assert_eq!(closings, [("OK", 2), ("OK", 3)]);
    let crate::GetReply::Message(stored) = fixture.board.lead().get("lead", 2).unwrap() else {
        panic!("large SEND did not reach the store");
    };
    assert_eq!(stored.body.as_ref(), &vec![b'i'; size]);

    drop(fixture);
    let _ = server
        .wait(WAIT)
        .expect("bidirectional link did not shut down");
}
