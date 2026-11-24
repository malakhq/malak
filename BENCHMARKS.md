# Malak benchmarks

## v0.1 performance pass

Both binaries were built with Rust 1.99.0 in release mode and run on the same
macOS 15.4.1 arm64 host. The baseline was `2fa7a0c`. Each column uses five
independent runs with the two revisions alternating. Other host activity
continued during the measurements. Quiet runs had no induced load. Loaded
runs started a parallel `cargo build --locked
--workspace --release` in a separate worktree with a fresh target directory.

- CLI calls used fresh local Markdown and SQLite boards with their default
  sync settings. They sent 64 bytes on stdin, read one message, retrieved its
  position and acknowledged it. The detached-start measurement ends when the
  command reports readiness. Stop used the scratch board's run ID.
- The mailbox ran in a 220×45 terminal against fresh copies of the same
  Markdown state with 87 participants, 119 channels and 1,110 retained
  positions. The first frame contains the five-channel summary. The exact
  values were one open question and two orders in force. Peak RSS is the
  macOS `time -l` maximum through that startup.
- The lead rate workload opened two terminal mailboxes and
  `mailbox --plain --for lead` on another fresh copy. It ran `read --wait 1`
  on an empty channel and attempted sends to another channel at up to
  20 per second over 15-second windows. Direct command exits give exact SEND
  and READ refusal counts. Owner-tail rate events coalesce counts with the
  last operation in each batch.
- The recipe trials ran `ask`, `answer`, `forward`, `worker-start` with a
  no-op command and `demo` with five stub workers and a stub lead. The
  recipe subprocesses used the binary under measurement.

The table gives median / worst in milliseconds unless marked MiB or shown
as a refusal count.

| Workload | Before quiet | After quiet | Before loaded | After loaded |
| --- | ---: | ---: | ---: | ---: |
| Send (Markdown) | 17.6 / 53.6 | 19.2 / 101.5 | 20.1 / 37.2 | 28.9 / 51.3 |
| Read (Markdown) | 10.9 / 21.7 | 16.1 / 16.4 | 15.2 / 36.9 | 20.1 / 24.5 |
| Get (Markdown) | 13.7 / 19.1 | 14.7 / 20.7 | 25.5 / 35.9 | 16.7 / 18.3 |
| Ack (Markdown) | 10.2 / 12.6 | 11.5 / 13.1 | 13.2 / 40.1 | 13.7 / 33.4 |
| Start (Markdown) | 106.8 / 206.8 | 101.3 / 189.9 | 116.6 / 200.9 | 136.7 / 171.0 |
| Stop (Markdown) | 17.5 / 24.8 | 16.6 / 18.7 | 11.2 / 20.8 | 20.2 / 29.8 |
| Send (SQLite) | 21.4 / 116.4 | 23.6 / 39.2 | 23.4 / 32.2 | 20.9 / 23.6 |
| Read (SQLite) | 11.3 / 51.8 | 13.7 / 30.0 | 11.7 / 14.0 | 12.3 / 33.4 |
| Get (SQLite) | 20.6 / 29.0 | 21.4 / 24.4 | 15.6 / 20.0 | 17.8 / 22.1 |
| Ack (SQLite) | 16.2 / 409.5 | 15.6 / 44.7 | 16.1 / 21.6 | 15.7 / 17.4 |
| Start (SQLite) | 115.7 / 149.9 | 118.3 / 149.6 | 114.9 / 135.3 | 105.0 / 110.9 |
| Stop (SQLite) | 12.3 / 16.6 | 13.0 / 15.0 | 17.9 / 21.8 | 16.8 / 26.1 |
| Mailbox first full frame | 412.6 / 504.3 | 484.2 / 823.2 | 427.9 / 957.2 | 408.9 / 789.9 |
| Exact question count | 4647.9 / 7312.6 | 4625.0 / 6891.3 | 5390.1 / 9037.1 | 5814.6 / 6894.0 |
| Exact order count | 493.8 / 583.8 | 487.1 / 826.3 | 508.4 / 1038.5 | 412.5 / 793.5 |
| Both counts exact | 4647.9 / 7312.6 | 4625.0 / 6891.3 | 5390.1 / 9037.1 | 5814.6 / 6894.0 |
| Mailbox peak RSS (MiB) | 14.2 / 14.9 | 14.0 / 14.4 | 14.8 / 15.6 | 15.0 / 15.2 |
| Board peak RSS (MiB) | 30.7 / 35.8 | 29.0 / 38.9 | 32.5 / 35.6 | 31.0 / 34.1 |
| ask | 194.1 / 320.7 | 180.9 / 323.4 | 509.6 / 644.5 | 448.2 / 515.5 |
| answer | 280.8 / 289.6 | 311.1 / 467.2 | 424.4 / 599.9 | 436.0 / 749.1 |
| forward | 344.9 / 825.6 | 344.9 / 355.3 | 363.6 / 510.1 | 449.9 / 574.5 |
| worker-start | 799.9 / 1491.2 | 753.6 / 943.1 | 1166.1 / 1390.5 | 1178.3 / 1317.9 |
| demo (five stub workers) | 2400.7 / 2838.1 | 2328.5 / 3148.0 | 2436.2 / 2743.6 | 2480.4 / 2577.0 |
| SEND rate refusals | 243/1418 | 0/1427 | 76/1422 | 0/1416 |
| SEND refusals per window | 52.0 / 81.0 | 0.0 / 0.0 | 9.0 / 49.0 | 0.0 / 0.0 |
| READ rate refusals | 100/163 | 0/75 | 24/96 | 0/75 |
| READ refusals per window | 18.0 / 37.0 | 0.0 / 0.0 | 0.0 / 17.0 | 0.0 / 0.0 |

All ten loaded measurements ran through an active build. The patched
mailboxes let all 2,843 measured sends and all 150 measured reads complete.
The baseline emitted rate-limit events for GET as well as the measured
SEND and READ refusals. The mailbox reuses the authenticated records from
its plain startup summary for history, saving a repeat GET for each
position the summary covered.

The loaded first-frame worst fell from 0.96 s to 0.79 s. The worst wait
for both exact counts fell from 9.04 s to 6.89 s, with a median of 5.39 s
before and 5.81 s after. The quiet first-frame median was 0.41 s before
and 0.48 s after. With all three viewers and the build active, the terminal
first-frame worst was 1.04 s before and 0.74 s after. Continuous sends kept
changing the retained ranges, so exact-count timings use the static copy.
The CLI, recipes and peak memory showed mixed timing and memory changes.

## Record integrity baseline

2026-11-21, macOS 15.4.1 on arm64, Rust 1.99.0, release profile. Each record carried 128 bytes of metadata. Signing includes allocating the encoded record and copying the body into it. Verification hashes the body, verifies the HMAC and checks the supplied journal history without copying the body.

| Body | Sign median | Sign p95 | Verify median | Verify p95 |
|---|---:|---:|---:|---:|
| 64 B | 646 ns | 997 ns | 423 ns | 544 ns |
| 4 KiB | 3.57 µs | 4.37 µs | 3.03 µs | 3.56 µs |
| 1 MiB | 0.657 ms | 0.753 ms | 0.645 ms | 0.707 ms |

The median and p95 use Criterion's 30 samples, with each sample divided by its iteration count. The p95 is the nearest-rank value of those sample averages.

```sh
cargo bench -p malak-record --bench check -- --warm-up-time 1 --measurement-time 2 --sample-size 30
```

Local socket, CLI and synced storage timings appear below. Network and bucket measurements depend on later milestones.

## Noise IK baseline

Both endpoints ran in one process on the same host. The measurement includes the two cryptographic messages and ephemeral key generation. It excludes sockets, framing, and `ATTACH`.

| Operation | Median wall time | p95 sample average |
|---|---:|---:|
| Both sides of Noise IK | 396 µs | 416 µs |

```sh
cargo bench -p malak-proto --bench handshake -- --warm-up-time 1 --measurement-time 2 --sample-size 30
```


## Frame decoding baseline

The benchmark validates the length fields and a two-key CBOR header. Contiguous input shares its existing body buffer. The split case joins two halves. Neither case decrypts or hashes the body.

| Body | Contiguous median | Contiguous p95 | Split median | Split p95 |
|---|---:|---:|---:|---:|
| 64 B | 46 ns | 53 ns | 70 ns | 72 ns |
| 4 KiB | 46 ns | 57 ns | 117 ns | 120 ns |
| 1 MiB | 44 ns | 57 ns | 17.4 µs | 19.0 µs |

```sh
cargo bench -p malak-proto --bench framing -- --warm-up-time 1 --measurement-time 2 --sample-size 30
```


## Warm Direct calls

Measured on macOS 15.4.1 arm64 with Rust 1.99.0 in release mode. Each send uses a fresh ID in an existing channel. Each read requests one unacknowledged record without waiting. Get requests that same position. The timings include the real memory store, record checks, authority and quota checks, and store worker handoffs. Setup, warmup writes and ID formatting are outside the measured send interval.

| Body | Send median / p95 | Read median / p95 | Get median / p95 |
|---|---:|---:|---:|
| 64 B | 14.2 / 30.1 µs | 18.1 / 37.1 µs | 8.93 / 19.4 µs |
| 4 KiB | 32.8 / 83.8 µs | 21.9 / 50.7 µs | 32.8 / 65.4 µs |
| 1 MiB | 2.08 / 8.34 ms | 2.14 / 6.07 ms | 1.76 / 4.96 ms |

The 64 B results exceed the target of a few microseconds for a warm
in-process call with a memory store, including the record check. The store
worker boundary enforces deadlines even when a layer blocks.

```sh
cargo bench -p malak --bench direct -- --warm-up-time 1 --measurement-time 2 --sample-size 30
```


## Direct 64 B profile and optimization

Re-measured the unchanged warm Direct workload on the same macOS arm64 host. Built the optimized benchmark with symbols and sampled each operation for four seconds at 1 ms intervals using macOS `sample`. The hot caller stacks for both verbs were dominated by store-worker dispatch and reply waiting. For example, a main-thread read stack reported 1,706 samples in the kernel's timed semaphore wait. Allocation and SHA-256 appeared in the stacks, but far less often than the handoff paths. The body was already shared on read.

The new handoff reuses a one-slot worker mailbox, stores the work and typed reply in one allocation, and returns the worker to the idle pool before waking its caller. Small metadata and the single-channel scan stay inline. Retry matching and record authentication share the body SHA-256 on send. Every store operation still runs with the same deadline, and the read path still authenticates the stored bytes before release.

| 64 B Direct call | Before median / p95 | After median / p95 |
|---|---:|---:|
| Send | 30.9 / 78.4 µs | 16.2 / 43.6 µs |
| Read | 19.6 / 40.9 µs | 14.4 / 32.0 µs |
| Get | 13.5 / 30.8 µs | 10.7 / 31.9 µs |

The earlier baseline above used a shorter measurement window and shows run-to-run variation. In this run the matched workload improved, while the medians remain above the few-microsecond target. The p95 uses Criterion sample averages.

For later: a store that declares it never blocks, such as the memory store, could be called directly without a worker.

```sh
CARGO_PROFILE_BENCH_DEBUG=1 cargo bench -p malak --bench direct -- '/64$' --warm-up-time 2 --measurement-time 4 --sample-size 30
```

## Mailbox owner metadata check

The existing 64 B warm Direct workload ran on macOS 15.4.1 arm64 with
Rust 1.99.0 after adding owner event metadata. Criterion point estimates
were 37.48 µs for send, 21.09 µs for read and 31.44 µs for get. The run
uses 20 samples. The estimates exceed the target of a few microseconds
for a warm in-process call with a memory store, including the record check.

```sh
cargo bench -p malak --bench direct -- '/64$' --warm-up-time 1 --measurement-time 2 --sample-size 20
```

## Unix sockets, CLI and synced stores

The v0.1 local benchmark uses Rust 1.99.0 in release mode on macOS 15.4.1
arm64 and Ubuntu 24.04.5 arm64 in the `malak-linux` Lima VM. The guest uses
four virtual CPUs and its own virtual disk. The runs were sequential after the platform checks finished. The Linux VM
figures include guest and host scheduling and the virtual disk.

- **Unix attach** opens the lead socket, completes Noise IK and `ATTACH`,
  and drops the connection. The benchmark builds the handover in memory.
- **Warm Unix GET** reuses an attached connection to an in-process board
  with the memory store. It times request framing, record verification,
  the message and the closing reply.
- **CLI GET** spawns `malak get --json`. The board uses SQLite. The CLI
  reads its handover, attaches, retrieves the record and serializes it.
  The child sends stdout to the null device.
- **CLI synced send** spawns `malak send` and writes 64 bytes on stdin.
  The Markdown board has syncing enabled. SQLite uses its stock synced mode.
  Both times include CLI overhead and the durable board write.

All values are milliseconds, shown as median / p95. Warm Unix GET uses
20 Criterion samples divided by their iteration counts. The other rows
use 21 individual calls, below the stock opening burst. CPU time includes
both endpoints for in-process Unix attach and the CLI child alone for CLI
operations.

| Operation | macOS wall | macOS CPU | Linux VM wall | Linux VM CPU |
| --- | ---: | ---: | ---: | ---: |
| Unix attach | 0.633 / 0.665 | 0.749 / 0.820 | 0.866 / 1.055 | 0.924 / 1.402 |
| Warm Unix GET, 64 B | 0.120 / 0.219 | — | 0.343 / 0.402 | — |
| Warm Unix GET, 4 KiB | 0.134 / 0.167 | — | 0.384 / 0.518 | — |
| Warm Unix GET, 1 MiB | 5.248 / 7.662 | — | 7.439 / 16.149 | — |
| CLI GET, 64 B | 4.135 / 5.059 | 3.201 / 3.788 | 2.243 / 3.141 | 1.482 / 2.097 |
| CLI GET, 4 KiB | 4.306 / 6.159 | 3.331 / 4.272 | 3.228 / 5.474 | 1.997 / 3.632 |
| CLI GET, 1 MiB | 42.696 / 53.927 | 39.573 / 42.690 | 143.818 / 336.622 | 121.660 / 259.787 |
| CLI synced Markdown send, 64 B | 10.967 / 23.979 | 3.171 / 3.565 | 10.886 / 18.388 | 3.658 / 6.566 |
| CLI synced SQLite send, 64 B | 12.672 / 13.371 | 3.113 / 4.520 | 17.232 / 46.008 | 3.447 / 12.546 |

On both systems, the 64 B warm socket median exceeds the target of tens
of microseconds and the cold attachment median exceeds the estimated
0.2 to 0.5 ms for the handshake and attachment. The CLI figures include
process startup and the stock JSON encoder. The synced figures include
the guest's virtual disk flush latency on Linux.

```sh
cargo bench -p malak --bench local -- --warm-up-time 0.5 --measurement-time 2 --sample-size 20
```
