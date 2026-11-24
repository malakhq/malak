use malak_core::{HandoverFile, LocalClient};
use malak_layers::Mechanism;
use malak_mechanism_key::Key;
use malak_proto::encode_frame;
use malak_transport_unix::UnixTransport;
use std::{path::Path, sync::Arc, time::Duration};

const WATCHDOG: Duration = Duration::from_secs(60);

pub(super) fn connect(handover: &Path) -> LocalClient<UnixTransport> {
    let handover = HandoverFile::read(handover).unwrap();
    let mechanism = (handover.id().as_str() != "lead").then(|| Arc::new(Key) as Arc<dyn Mechanism>);
    LocalClient::participant(Arc::new(UnixTransport::default()), handover, mechanism)
        .expect("attach to the test board")
}

pub(super) fn seed(handover: &Path, channel: &str, messages: u64) {
    let mut client = connect(handover);
    for rid in 2..messages + 2 {
        let body = format!("retained-{:03}", rid - 1);
        let frame = encode_frame(
            rid,
            "SEND",
            3,
            |out| {
                out.str("channel").unwrap().str(channel).unwrap();
                out.str("id").unwrap().str(&body).unwrap();
                out.str("kind").unwrap().str("note").unwrap();
            },
            body.as_bytes(),
        )
        .unwrap();
        client.send_frame(frame, WATCHDOG).unwrap();
        assert_eq!(client.read_frame(WATCHDOG).unwrap().op(), "OK", "{body}");
    }
}

pub(super) fn get(client: &mut LocalClient<UnixTransport>, rid: u64, channel: &str) -> String {
    let frame = encode_frame(
        rid,
        "GET",
        2,
        |out| {
            out.str("channel").unwrap().str(channel).unwrap();
            out.str("position").unwrap().u64(1).unwrap();
        },
        &[],
    )
    .unwrap();
    client.send_frame(frame, WATCHDOG).unwrap();
    loop {
        let reply = client.read_frame(WATCHDOG).unwrap();
        assert_eq!(reply.rid(), rid);
        if reply.op() != "MSG" {
            return reply.op().to_owned();
        }
    }
}
