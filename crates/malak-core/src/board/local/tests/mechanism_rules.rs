use super::*;
use crate::{Action, ChannelName, frame_fields::Fields};
use malak_layers::{Enrolment, Handover, Identity, Proves, Session, Step};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct FailingVeto(AtomicUsize);

impl Mechanism for FailingVeto {
    fn describe(&self) -> Proves {
        Proves {
            has_veto: true,
            ..Key.describe()
        }
    }

    fn enrol(&self, id: &ParticipantId, hints: &Enrolment<'_>) -> Result<Handover, Error> {
        Key.enrol(id, hints)
    }

    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error> {
        Key.step(session, incoming)
    }

    fn allow(&self, _: &Identity<'_>, _: Action, _: &ChannelName) -> Result<bool, Error> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Err(Error::unavailable("test veto failure"))
    }

    fn revoke(&self, id: &ParticipantId) -> Result<(), Error> {
        Key.revoke(id)
    }
}

fn get_frame() -> Bytes {
    encode_frame(
        2,
        "GET",
        2,
        |out| {
            out.str("channel").unwrap().str("lead").unwrap();
            out.str("position").unwrap().u64(1).unwrap();
        },
        &[],
    )
    .unwrap()
}

fn exchange(
    fixture: Fixture,
    caller: Encrypted,
    attached: super::super::LocalParticipant<UnixTransport>,
) -> Frame {
    std::thread::scope(|scope| {
        scope.spawn(move || attached.serve());
        // The fixture closes the client on return or panic, unblocking serve.
        scope
            .spawn(move || {
                let mut caller = caller;
                fixture.send_protected(&mut caller, &get_frame());
                fixture.receive_frame(&mut caller)
            })
            .join()
            .unwrap()
    })
}

#[test]
fn an_error_in_an_allowed_calls_veto_fails_closed() {
    let veto = Arc::new(FailingVeto::default());
    let mut fixture = Fixture::with_mechanism(uid(), veto.clone());
    let (id, opening, mut caller) = fixture.registered("member");
    let mut grant = Grant::default();
    grant.allow("lead".parse().unwrap(), [Action::Get]);
    fixture.board.replace_grant(&id, grant).unwrap();
    fixture.send_protected(&mut caller, &attach_request(1, id.as_str(), &[]).unwrap());
    let attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");

    let reply = exchange(fixture, caller, attached);

    assert_eq!(reply.op(), "ERR");
    assert_eq!(
        Fields::new(&reply).unwrap().required("outcome").unwrap(),
        "unavailable"
    );
    assert_eq!(veto.0.load(Ordering::Relaxed), 1);
}

#[test]
fn a_mechanisms_veto_does_not_run_for_the_lead() {
    let veto = Arc::new(FailingVeto::default());
    let mut fixture = Fixture::with_mechanism(uid(), veto.clone());
    let (opening, mut caller) = fixture.protected(Entrance::Lead);
    fixture.send_protected(&mut caller, &attach_request(1, "lead", &[]).unwrap());
    let attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");

    let reply = exchange(fixture, caller, attached);

    assert_eq!(reply.op(), "OK");
    assert_eq!(veto.0.load(Ordering::Relaxed), 0);
}

#[test]
fn a_denied_grant_never_reaches_the_veto() {
    let veto = Arc::new(FailingVeto::default());
    let mut fixture = Fixture::with_mechanism(uid(), veto.clone());
    let (id, opening, mut caller) = fixture.registered("member");
    fixture.send_protected(&mut caller, &attach_request(1, id.as_str(), &[]).unwrap());
    let attached = opening.attach_participant().unwrap();
    assert_eq!(fixture.receive_frame(&mut caller).op(), "OK");

    let reply = exchange(fixture, caller, attached);

    assert_eq!(reply.op(), "ERR");
    assert_eq!(
        Fields::new(&reply).unwrap().required("outcome").unwrap(),
        "refused"
    );
    assert_eq!(veto.0.load(Ordering::Relaxed), 0);
}
