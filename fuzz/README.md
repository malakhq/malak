# Fuzzing Malak

Install [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) and a Rust nightly
toolchain. From the repository root, copy the starting inputs and run the
targets with AddressSanitizer.

```sh
mkdir -p fuzz/corpus/frame_decoder fuzz/corpus/markdown_parser
cp fuzz/seeds/frame_decoder/* fuzz/corpus/frame_decoder/
cp fuzz/seeds/markdown_parser/* fuzz/corpus/markdown_parser/
cargo +nightly fuzz run frame_decoder -- -max_total_time=12 -max_len=65536 -timeout=5
cargo +nightly fuzz run markdown_parser -- -max_total_time=12 -max_len=65536 -timeout=5
```

The frame target compares contiguous and fragmented input, including what the
decoder consumes before closing. It drives the participant protocol in the
opening, attachment and attached states. The Markdown target exercises ordinary
iteration, candidate parsing at independent marker offsets, and round trips
through the stock store's formatter.

A bounded smoke run on arm64 macOS with cargo-fuzz 0.13.2 executed 966,839 frame
inputs and 291,721 Markdown inputs in 13 seconds per target. Both runs completed
without a failure.

The v0.1 run used cargo-fuzz 0.13.2 and Rust 1.102.0-nightly
(`bdf712ee8`) on arm64 macOS 15.4.1 with AddressSanitizer. Each target
started with its checked-in seeds and had a 60-second budget, a maximum
input length of 65,536 bytes, and a 5-second per-input timeout.

| Target | Inputs executed | Elapsed | Failures |
| --- | ---: | ---: | ---: |
| `frame_decoder` | 1,322,166 | 61 s | 0 |
| `markdown_parser` | 494,637 | 61 s | 0 |

To repeat the budget, use the commands above with `-max_total_time=60`.
This exercises both shipped decoder targets. A corpus of complete protected
sessions remains a separate stateful protocol campaign.
