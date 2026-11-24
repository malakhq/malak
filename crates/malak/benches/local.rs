//! Local attach, warm GET, CLI GET and CLI sends to synced stores.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use criterion::Criterion;
    use malak::{Limits, Owner, ParticipantId, SendRequest};
    use malak_core::{HandoverFile, LocalClient, LocalConfig};
    use malak_layers::Transport;
    use malak_mechanism_key::Key;
    use malak_proto::{Binding, Entrance, KeyPair, encode_frame};
    use malak_store_memory::MemoryStore;
    use malak_testkit::unix_socket::SocketDir;
    use malak_transport_unix::UnixTransport;
    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeValLike;
    use std::fs::OpenOptions;
    use std::hint::black_box;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const CLI: &str = env!("CARGO_BIN_EXE_malak");
    const WAIT: Duration = Duration::from_secs(5);

    struct Board {
        root: SocketDir,
        process: Child,
    }

    impl Board {
        fn start(persistence: &str) -> Self {
            let root = SocketDir::new().unwrap();
            let config = root.path().join("board.toml");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config)
                .unwrap();
            write!(file, "[board]\nstate = {:?}\nsocket_folder = {:?}\npersistence = {persistence:?}\nsync = true\n", root.path().join("st"), root.path().join("d")).unwrap();
            let mut process = Command::new(CLI)
                .args(["board", "start", "--config"])
                .arg(config)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            BufReader::new(process.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready.trim(), "ready");
            Self { root, process }
        }

        fn handover(&self) -> PathBuf {
            self.root.path().join("st/lead.handover")
        }

        fn run(&self, args: &[&str]) {
            let status = Command::new(CLI)
                .args(args)
                .env("MALAK_HANDOVER", self.handover())
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        }

        fn send(&self, channel: &str, size: usize) {
            let mut child = Command::new(CLI)
                .args(["send", channel, "-"])
                .env("MALAK_HANDOVER", self.handover())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(&vec![b'a'; size])
                .unwrap();
            assert!(child.wait().unwrap().success());
        }
    }

    impl Drop for Board {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    struct Listener {
        owner: Owner,
        transport: Arc<UnixTransport>,
        worker: Option<JoinHandle<()>>,
        stop: Arc<AtomicBool>,
        board_key: [u8; 32],
        lead: KeyPair,
        socket: SocketDir,
    }

    impl Listener {
        fn start() -> Self {
            let socket = SocketDir::new().unwrap();
            let address = socket.address();
            let transport = Arc::new(UnixTransport::bind(&address).unwrap());
            let mut limits = Limits::default();
            limits.lead_calls_per_minute = usize::MAX;
            let owner = Owner::in_memory(MemoryStore::default(), limits).unwrap();
            let board = KeyPair::generate().unwrap();
            let board_key = *board.public();
            let lead = KeyPair::generate().unwrap();
            let uid = nix::unistd::geteuid().as_raw();
            let entrances = owner
                .local_entrances(
                    LocalConfig {
                        board_key: board,
                        board_user: uid,
                        owner_key: *KeyPair::generate().unwrap().public(),
                        lead_user: uid,
                        lead_key: *lead.public(),
                    },
                    Arc::new(Key),
                )
                .unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let halt = stop.clone();
            let server = transport.clone();
            let worker = thread::spawn(move || {
                loop {
                    let incoming = server.accept(&address).unwrap();
                    if halt.load(Ordering::Acquire) {
                        break;
                    }
                    let session = entrances
                        .open(Entrance::Lead, server.clone(), incoming)
                        .unwrap()
                        .attach_participant()
                        .unwrap();
                    let _ = session.serve();
                }
            });
            Self {
                owner,
                transport,
                worker: Some(worker),
                stop,
                board_key,
                lead,
                socket,
            }
        }

        fn handover(&self) -> HandoverFile {
            HandoverFile::new(
                Binding {
                    version: 1,
                    entrance: Entrance::Lead,
                },
                self.socket.address(),
                self.board_key,
                ParticipantId::lead(),
                String::new(),
                Vec::new().into(),
                (*self.lead.private()).into(),
            )
            .unwrap()
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Keep the peer alive through accept so the OS can report its credentials.
            let _wake = self.transport.dial(&self.socket.address()).unwrap();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn measure(label: &str, who: UsageWho, mut operation: impl FnMut()) {
        // Fewer than the 40 connections in the stock opening burst.
        const SAMPLES: usize = 21;
        fn cpu(who: UsageWho) -> i64 {
            let usage = getrusage(who).unwrap();
            (usage.user_time() + usage.system_time()).num_microseconds()
        }
        let mut elapsed = [Duration::ZERO; SAMPLES];
        let mut processor = [0; SAMPLES];
        for (elapsed, processor) in elapsed.iter_mut().zip(&mut processor) {
            let before_cpu = cpu(who);
            let start = Instant::now();
            operation();
            *elapsed = start.elapsed();
            *processor = cpu(who) - before_cpu;
        }
        elapsed.sort_unstable();
        processor.sort_unstable();
        let median = SAMPLES / 2;
        let p95 = (SAMPLES * 95).div_ceil(100) - 1;
        println!(
            "{label} | wall {:.2} / {:.2} µs | CPU {} / {} µs",
            elapsed[median].as_secs_f64() * 1e6,
            elapsed[p95].as_secs_f64() * 1e6,
            processor[median],
            processor[p95],
        );
    }

    pub fn local(criterion: &mut Criterion) {
        let listener = Listener::start();
        let client_transport = Arc::new(UnixTransport::default());
        measure("unix/attach", UsageWho::RUSAGE_SELF, || {
            let handover = listener.handover();
            let client =
                LocalClient::participant(client_transport.clone(), handover, None).unwrap();
            black_box(client);
        });

        for size in [64, 4096, 1024 * 1024] {
            let channel = format!("sample/{size}");
            listener
                .owner
                .lead()
                .send(SendRequest::note(&channel, "seed", &vec![b'a'; size]))
                .unwrap();
            let mut client =
                LocalClient::participant(client_transport.clone(), listener.handover(), None)
                    .unwrap();
            let mut rid = 0;
            criterion.bench_function(&format!("unix/get/{size}"), |bench| {
                bench.iter(|| {
                    rid += 1;
                    let request = encode_frame(
                        rid,
                        "GET",
                        2,
                        |out| {
                            out.str("channel")
                                .unwrap()
                                .str(&channel)
                                .unwrap()
                                .str("position")
                                .unwrap()
                                .u64(1)
                                .unwrap();
                        },
                        &[],
                    )
                    .unwrap();
                    client.send_frame(request, WAIT).unwrap();
                    let message = client.read_frame(WAIT).unwrap();
                    assert_eq!(message.op(), "MSG");
                    assert_eq!(message.body().len(), size);
                    black_box(message);
                    let outcome = client.read_frame(WAIT).unwrap();
                    assert_eq!(outcome.op(), "OK");
                });
            });
            let board = Board::start("sqlite");
            board.send(&channel, size);
            measure(
                &format!("cli/get/{size}"),
                UsageWho::RUSAGE_CHILDREN,
                || {
                    board.run(&["get", &channel, "1", "--json"]);
                },
            );
        }

        for store in ["markdown", "sqlite"] {
            let board = Board::start(store);
            measure(
                &format!("cli/synced_send_64/{store}"),
                UsageWho::RUSAGE_CHILDREN,
                || {
                    board.send("lead", 64);
                },
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
criterion::criterion_group!(benches, unix::local);
#[cfg(any(target_os = "linux", target_os = "macos"))]
criterion::criterion_main!(benches);
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {}
