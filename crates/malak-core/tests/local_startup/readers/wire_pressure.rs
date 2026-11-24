use super::*;
use malak_proto::{Frame, TAG_BYTES};
use malak_testkit::tasks::{self, Task};
use std::time::Duration;

#[test]
fn cancel_closes_a_ready_read_before_the_full_queue_can_drain() {
    tasks::spawn(run_cancel_with_full_queue)
        .wait(TIMEOUT * 4)
        .expect("full-queue cancellation timed out");
}

fn run_cancel_with_full_queue() {
    let fixture = FullQueue::new();
    let paused = &fixture.writer;
    fixture.with_client(|reader| {
        let (entered, release) = paused.pause();
        reader
            .send_frame(read_channels(2, &["lead"], Some(1), 0), TIMEOUT)
            .unwrap();
        entered.recv_timeout(TIMEOUT).unwrap();
        let ready = read_channels(3, &["next"], Some(1), 60_000);
        reader.send_frame(ready, TIMEOUT).unwrap();
        fixture.wait_for_deferred_read();
        let cancel = encode_frame(
            4,
            "CANCEL",
            1,
            |header| {
                header.str("target").unwrap().u64(3).unwrap();
            },
            &[],
        )
        .unwrap();
        let ping = encode_frame(5, "PING", 0, |_| {}, &[]).unwrap();
        let expected = paused.received_bytes() + wire_len(&cancel) + wire_len(&ping);
        for request in [cancel, ping] {
            reader.send_frame(request, TIMEOUT).unwrap();
        }
        // The receiver reads the PING only after handling CANCEL. The writer has
        // been blocked since taking the first message and still cannot drain it.
        paused.wait_for_bytes(expected);
        release.send(()).unwrap();

        let first = reader.read_frame(TIMEOUT).unwrap();
        assert_eq!((first.op(), first.rid()), ("MSG", 2));
        let mut replies = (0..4)
            .map(|_| reader.read_frame(TIMEOUT).unwrap())
            .collect::<Vec<_>>();
        replies.sort_unstable_by_key(Frame::rid);
        assert_eq!(
            replies
                .iter()
                .map(|reply| (reply.op(), reply.rid()))
                .collect::<Vec<_>>(),
            [("OK", 2), ("OK", 3), ("OK", 4), ("OK", 5)]
        );
        assert!(field(&replies[1], "cancelled").bool().unwrap());
        assert!(field(&replies[2], "found").bool().unwrap());

        let unreleased = ack_frame(6, "next", fixture.next);
        reader.send_frame(unreleased, TIMEOUT).unwrap();
        let reply = reader.read_frame(TIMEOUT).unwrap();
        assert_eq!((reply.op(), reply.rid()), ("ERR", 6));

        reader
            .send_frame(read_channels(7, &["next"], Some(1), 0), TIMEOUT)
            .unwrap();
        let message = reader.read_frame(TIMEOUT).unwrap();
        assert_eq!(
            (message.op(), message.rid(), message.body()[0]),
            ("MSG", 7, 2)
        );
        assert_eq!(
            field(&message, "position").u64().unwrap(),
            fixture.next.position.get()
        );
        assert_eq!(reader.read_frame(TIMEOUT).unwrap().op(), "OK");
        acknowledge(reader, 8, "next", fixture.next);
    });
}

#[test]
fn a_full_message_queue_keeps_its_closing_reply_bound_until_progress() {
    tasks::spawn(run_closing_budget)
        .wait(TIMEOUT * 4)
        .expect("closing reply budget scenario timed out");
}

fn run_closing_budget() {
    let fixture = FullQueue::new();
    let paused = &fixture.writer;
    fixture.with_client(|reader| {
        let (entered, release) = paused.pause();
        reader
            .send_frame(read_channels(2, &["lead"], Some(1), 0), TIMEOUT)
            .unwrap();
        entered.recv_timeout(TIMEOUT).unwrap();
        let mut allowed = paused.received_bytes();
        for rid in 3..=19 {
            let ping = encode_frame(rid, "PING", 0, |_| {}, &[]).unwrap();
            if rid <= 17 {
                allowed += wire_len(&ping);
            }
            reader.send_frame(ping, TIMEOUT).unwrap();
        }
        paused.wait_for_bytes(allowed);
        // The first READ occupies one of 16 closing places. The decoder has
        // consumed fifteen PINGs, leaving the remaining two in the socket.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(paused.received_bytes(), allowed);
        release.send(()).unwrap();

        let first = reader.read_frame(TIMEOUT).unwrap();
        assert_eq!((first.op(), first.rid()), ("MSG", 2));
        let mut replies = (2..=19)
            .map(|_| {
                let reply = reader.read_frame(TIMEOUT).unwrap();
                assert_eq!(reply.op(), "OK");
                reply.rid()
            })
            .collect::<Vec<_>>();
        replies.sort_unstable();
        assert_eq!(replies, (2..=19).collect::<Vec<_>>());
    });
}

#[test]
fn an_applied_ack_can_be_retried_after_losing_its_unwritten_reply() {
    tasks::spawn(run_lost_ack)
        .wait(TIMEOUT * 4)
        .expect("lost acknowledgement scenario timed out");
}

fn run_lost_ack() {
    let fixture = FullQueue::new();
    let (server, mut reader) = fixture.connect();
    reader
        .send_frame(read_channels(2, &["lead"], Some(1), 0), TIMEOUT)
        .unwrap();
    let first = reader.read_frame(TIMEOUT).unwrap();
    assert_eq!((first.op(), first.rid()), ("MSG", 2));
    assert_eq!(
        field(&first, "position").u64().unwrap(),
        fixture.first.position.get()
    );
    assert_eq!(reader.read_frame(TIMEOUT).unwrap().op(), "OK");
    let (entered, release) = fixture.writer.pause();
    reader
        .send_frame(ack_frame(3, "lead", fixture.first), TIMEOUT)
        .unwrap();
    // The writer can reach this gate only after the journal has saved the ACK
    // and queued its reply. No byte of that reply has reached the peer.
    entered.recv_timeout(TIMEOUT).unwrap();
    drop(reader);
    release.send(()).unwrap();
    let _ = server
        .wait(TIMEOUT)
        .expect("first reader link did not shut down");

    fixture.with_client(|reader| {
        acknowledge(reader, 2, "lead", fixture.first);
        reader
            .send_frame(read_channels(3, &["lead"], Some(1), 0), TIMEOUT)
            .unwrap();
        let second = reader.read_frame(TIMEOUT).unwrap();
        assert_eq!((second.op(), second.rid(), second.body()[0]), ("MSG", 3, 3));
        assert_eq!(field(&second, "position").u64().unwrap(), 2);
        assert_eq!(reader.read_frame(TIMEOUT).unwrap().op(), "OK");
    });
}

fn wire_len(frame: &[u8]) -> usize {
    assert!(frame.len() <= malak_proto::MAX_CONTENT);
    frame.len() + TAG_BYTES + 2
}

struct FullQueue {
    _fixture: Fixture,
    board: malak_core::Owner,
    next_checks: mpsc::Receiver<()>,
    entrances: malak_core::LocalEntrances,
    address: String,
    handover: std::path::PathBuf,
    unix: Arc<UnixTransport>,
    writer: Arc<PauseWrite>,
    first: malak_core::Receipt,
    next: malak_core::Receipt,
}

impl FullQueue {
    fn new() -> Self {
        const BODY: usize = 32 * 1024;
        let fixture = Fixture::new();
        let address = fixture.root.address();
        let (checked, next_checks) = mpsc::channel();
        let probe = Arc::new(ReadVeto(move |channel: &ChannelName| {
            if channel.as_str() == "next" {
                let _ = checked.send(());
            }
            true
        }));
        let (board, entrances) = fixture
            .start_settings_with_limits(
                &fixture.state,
                |role, fallback| address_for(role, fallback, &address),
                probe,
                one_message_limits(BODY),
            )
            .unwrap();
        let handover = fixture.root.path().join("reader");
        enrol(&fixture, &entrances, "reader", &["lead", "next"]);
        let lead = board.lead();
        let first = lead
            .send(SendRequest::note("lead", "held", &[1; BODY]))
            .unwrap();
        lead.send(SendRequest::note("lead", "following", &[3; BODY]))
            .unwrap();
        let next = lead
            .send(SendRequest::note("next", "ready", &[2; BODY]))
            .unwrap();
        let unix = Arc::new(UnixTransport::bind(&address).unwrap());
        let writer = PauseWrite::new(&unix);
        Self {
            _fixture: fixture,
            board,
            next_checks,
            entrances,
            address,
            handover,
            unix,
            writer,
            first,
            next,
        }
    }

    fn wait_for_deferred_read(&self) {
        // Wait for preflight and the first delivery attempt. The read already
        // took its change revision, so this append wakes it even if it has not
        // parked yet.
        self.next_checks.recv_timeout(TIMEOUT).unwrap();
        self.next_checks.recv_timeout(TIMEOUT).unwrap();
        self.board
            .lead()
            .send(SendRequest::note("ticks", "wake-reader", b""))
            .unwrap();
        self.next_checks
            .recv_timeout(TIMEOUT)
            .expect("read did not revisit its ready message");
    }

    fn connect(&self) -> (Task<Result<(), Error>>, LocalClient<UnixTransport>) {
        let server = tasks::spawn({
            let entrances = self.entrances.clone();
            let address = self.address.clone();
            let writer = self.writer.clone();
            move || serve_participant(&entrances, &address, writer)
        });
        let reader = connect(&self.unix, HandoverFile::read(&self.handover).unwrap());
        (server, reader)
    }

    fn with_client(&self, exercise: impl FnOnce(&mut LocalClient<UnixTransport>)) {
        let (server, mut reader) = self.connect();
        exercise(&mut reader);
        drop(reader);
        let _ = server.wait(TIMEOUT).expect("reader link did not shut down");
    }
}
