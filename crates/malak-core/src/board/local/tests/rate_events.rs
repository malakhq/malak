use super::*;
use crate::frame_fields::Fields;
use crate::{Action, Outcome, SendRequest};
use malak_layers::EventKind;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn malformed_denied_and_retry_calls_share_one_rate_and_coalesce_events() {
    let limits = Limits {
        minimum_free_disk_bytes: 0,
        calls_per_minute: 4,
        board_channels: 3,
        reserved_channels: 1,
        ..Limits::default()
    };
    let mut fixture = Fixture::with_limits(uid(), Arc::new(Key), limits);
    let (id, opening, mut caller) = fixture.registered("worker");
    let mut grant = Grant::default();
    grant.allow("lead".parse().unwrap(), [Action::Send]);
    fixture.board.replace_grant(&id, grant).unwrap();
    let handle = fixture.board.handle(&id).unwrap();
    let board = fixture.board.board.clone();
    let watch = board.events.watch(None).unwrap();
    fixture.send_protected(&mut caller, &attach_request(1, id.as_str(), &[]).unwrap());
    let attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");
    let lead = fixture.board.lead();

    std::thread::scope(|tasks| {
        tasks.spawn(|| attached.serve());
        let (sender, events) = mpsc::channel();
        let reader = tasks.spawn({
            let watch = watch.clone();
            move || {
                while let Some((event, _)) = watch.next() {
                    if matches!(event.kind, EventKind::Denied | EventKind::LimitHit)
                        && sender.send(event).is_err()
                    {
                        break;
                    }
                }
            }
        });
        let exercise = tasks.spawn(move || {
            let call = SendRequest::note("lead", "message", b"stored");
            let malformed = encode_frame(2, "SEND", 0, |_| {}, &[]).unwrap();
            fixture.send_protected(&mut caller, &malformed);
            let reply = fixture.receive_frame(&mut caller);
            assert_eq!((reply.op(), reply.rid()), ("ERR", 2));
            assert_eq!(
                Fields::new(&reply).unwrap().required("outcome").unwrap(),
                "refused"
            );
            assert_eq!(
                handle.get("lead", 1).unwrap_err().outcome(),
                Outcome::Refused
            );
            assert!(!handle.send(call).unwrap().duplicate);
            assert!(handle.send(call).unwrap().duplicate);

            for rid in 3..=5 {
                let malformed = encode_frame(rid, "GET", 0, |_| {}, &[]).unwrap();
                fixture.send_protected(&mut caller, &malformed);
                let reply = fixture.receive_frame(&mut caller);
                assert_eq!((reply.op(), reply.rid()), ("ERR", rid));
                assert_eq!(
                    Fields::new(&reply).unwrap().required("outcome").unwrap(),
                    "unavailable"
                );
                assert!(handle.send(call).unwrap_err().is_limit());
            }
            assert!(
                lead.send(SendRequest::note("lead-reserve", "lead", b""))
                    .is_ok()
            );
            assert!(lead.get("lead", 1).is_ok());
        });

        let first = events.recv_timeout(Duration::from_secs(5));
        let second = events.recv_timeout(Duration::from_secs(5));
        let extra = events.recv_timeout(Duration::from_millis(100));
        watch.close();
        reader.join().unwrap();
        exercise.join().unwrap();

        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(
            (first.participant, first.kind, first.count),
            (Some(id.clone()), EventKind::Denied, 2)
        );
        assert_eq!(
            (second.participant, second.kind, second.count),
            (Some(id), EventKind::LimitHit, 6)
        );
        assert!(matches!(extra, Err(mpsc::RecvTimeoutError::Timeout)));
    });
}
