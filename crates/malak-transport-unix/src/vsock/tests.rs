use super::*;
use malak_proto::{
    Binding, Entrance, FrameDecoder, Initiator, KeyPair, OPENING_BYTES, Responder, TAG_BYTES,
    encode_frame,
};
use std::io::{Read, Write};

fn pair() -> (VsockLink, VsockLink) {
    let (left, right) = UnixStream::pair().unwrap();
    (
        host_link(left, Evidence::default()).unwrap().io,
        host_link(right, Evidence::default()).unwrap().io,
    )
}

#[test]
fn firecracker_streams_deliver_bytes_in_order() {
    malak_testkit::tasks::spawn(|| {
        let (left, right) = pair();
        malak_testkit::transport::bytes_arrive_in_order(&VsockTransport::default(), &left, &right);
    })
    .wait(std::time::Duration::from_secs(10))
    .expect("Firecracker byte-order check did not finish");
}

#[test]
fn firecracker_streams_report_close_once() {
    let (left, right) = pair();
    malak_testkit::transport::close_is_reported_once(&VsockTransport::default(), &left, &right);
}

#[cfg(target_os = "macos")]
#[test]
fn firecracker_shutdown_wakes_a_reader_draining_the_stream() {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    const TRANSFERS: usize = 512;
    let (completed, progress) = channel();
    let transfers = std::thread::spawn(move || {
        let transport = VsockTransport::default();
        let payload = vec![7; 300_000];
        for attempt in 1..=TRANSFERS {
            let (left, right) = pair();
            let parts = [b"x".as_slice(), payload.as_slice(), b"tail"];
            let mut remaining = parts.iter().map(|part| part.len()).sum::<usize>();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    for mut bytes in parts {
                        while !bytes.is_empty() {
                            bytes = &bytes[transport.send(&left, bytes).unwrap()..];
                        }
                    }
                    transport.send(&left, &[]).unwrap();
                });
                let mut buffer = [0; 7013];
                while let Received::Bytes(count) = transport.receive(&right, &mut buffer).unwrap() {
                    remaining -= count.get();
                }
            });
            assert_eq!(remaining, 0, "premature EOF on transfer {attempt}");
            if completed.send(()).is_err() {
                return;
            }
        }
    });
    // A stalled transfer is a failure. The full batch can take longer under load.
    for attempt in 1..=TRANSFERS {
        match progress.recv_timeout(Duration::from_secs(15)) {
            Ok(()) => {}
            Err(RecvTimeoutError::Disconnected) => {
                transfers.join().unwrap();
                panic!("transfer {attempt} did not report completion");
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("Firecracker reader did not observe shutdown on transfer {attempt}");
            }
        }
    }
    transfers.join().unwrap();
}

#[test]
fn firecracker_connect_leaves_the_first_malak_bytes_unread() {
    let (mut host, mut vmm) = UnixStream::pair().unwrap();
    std::thread::scope(|scope| {
        let (finished, wait) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let mut command = [0; 13];
            vmm.read_exact(&mut command).unwrap();
            assert_eq!(&command, b"CONNECT 4100\n");
            vmm.write_all(b"OK 1073741824\nmalak").unwrap();
            // Keep the VMM socket open while the host completes negotiation.
            // A dropped sender also releases the fixture if the host panics.
            let _ = wait.recv();
        });

        preamble::connect(&host, 4100).unwrap();

        let mut payload = [0; 5];
        host.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"malak");
        finished.send(()).unwrap();
    });
}

#[test]
fn firecracker_connect_rejects_an_overlong_reply() {
    let (host, mut vmm) = UnixStream::pair().unwrap();
    vmm.write_all(b"OK 12345678901234567890\n").unwrap();

    assert!(preamble::connect(&host, 4100).is_err());
}

#[test]
fn protected_malak_frames_pass_through_the_firecracker_stream() {
    let (left, right) = pair();
    let transport = VsockTransport::default();
    let binding = Binding {
        version: 1,
        entrance: Entrance::General,
    };
    let participant = KeyPair::generate().unwrap();
    let board = KeyPair::generate().unwrap();
    let (initiator, opening) =
        Initiator::open(binding, participant.private(), board.public()).unwrap();
    let mut bytes = [0; OPENING_BYTES];

    transfer(&transport, &left, &right, &opening, &mut bytes);
    let responder = Responder::read(binding, board.private(), board.public(), &bytes).unwrap();
    let (mut receiver, reply) = responder.reply().unwrap();
    let mut bytes = vec![0; reply.len()];
    transfer(&transport, &right, &left, &reply, &mut bytes);
    let mut sender = initiator.finish(&bytes).unwrap();

    let plain = encode_frame(41, "PING", 0, |_| {}, b"").unwrap();
    let mut encrypted = vec![0; plain.len() + TAG_BYTES];
    let length = sender.seal(&plain, &mut encrypted).unwrap();
    encrypted.truncate(length);
    let mut bytes = vec![0; length];
    transfer(&transport, &left, &right, &encrypted, &mut bytes);
    let mut decoded = vec![0; plain.len()];
    let length = receiver.open(&bytes, &mut decoded).unwrap();
    assert_eq!(length, plain.len());
    let frame = FrameDecoder::new(1024, None)
        .unwrap()
        .pull(&mut decoded.into())
        .unwrap()
        .unwrap();
    assert_eq!(frame.op(), "PING");
    assert_eq!(frame.rid(), 41);
}

fn transfer(
    transport: &VsockTransport,
    from: &VsockLink,
    to: &VsockLink,
    mut bytes: &[u8],
    mut output: &mut [u8],
) {
    while !bytes.is_empty() {
        let count = transport.send(from, bytes).unwrap();
        bytes = &bytes[count..];
    }
    while !output.is_empty() {
        let Received::Bytes(count) = transport.receive(to, output).unwrap() else {
            panic!("link closed mid-transfer");
        };
        output = &mut output[count.get()..];
    }
}

#[cfg(target_os = "linux")]
mod verified {
    use super::*;
    use malak_layers::Source;
    use malak_testkit::unix_socket::SocketDir;
    use nix::unistd::geteuid;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;

    struct ChildProcess(Child);

    impl Drop for ChildProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_verified_route_skips_a_socket_from_the_wrong_host_process() {
        let root = SocketDir::new().unwrap();
        let uds_path = root.path().join("vmm");
        let route = root.path().join("vmm_4100");
        let mut peer = ChildProcess(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "vsock::tests::verified::unix_peer", "--ignored"])
                .env("MALAK_TEST_VMM_ROUTE", &route)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let vmm = Arc::new(Vmm::capture(&mut peer.0, geteuid().as_raw()).unwrap());
        let transport = VsockTransport::bind_verified(vmm.clone(), &uds_path, 4100).unwrap();
        let _wrong_process = UnixStream::connect(route).unwrap();
        peer.0.stdin.as_mut().unwrap().write_all(b"1").unwrap();

        let accepted = transport
            .accept(&format!("vsock:{}:4100", vmm.id()))
            .unwrap();

        assert_eq!(accepted.process_id, Some(peer.0.id()));
        assert_eq!(accepted.source, Some(Source::VirtualMachine(vmm.id())));
    }

    #[test]
    #[ignore = "runs as the isolated Unix peer subprocess"]
    fn unix_peer() {
        let mut stdin = std::io::stdin().lock();
        let mut trigger = [0; 1];
        stdin.read_exact(&mut trigger).unwrap();
        let path = PathBuf::from(std::env::var_os("MALAK_TEST_VMM_ROUTE").unwrap());
        let _connection = UnixStream::connect(path).unwrap();
        let _ = stdin.read(&mut trigger);
    }
}
