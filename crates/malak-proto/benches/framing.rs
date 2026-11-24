//! Framing time for contiguous and fragmented plaintext inputs.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use malak_proto::{FrameDecoder, frame_prefix};
use std::hint::black_box;

fn framing(criterion: &mut Criterion) {
    let header = b"\xa2\x62op\x64SEND\x63rid\x07";
    let mut group = criterion.benchmark_group("frame_decode");

    for size in [64, 4096, 1024 * 1024] {
        let mut message = Vec::from(frame_prefix(header.len(), size).unwrap());
        message.extend_from_slice(header);
        message.resize(message.len() + size, 42);
        let encoded = Bytes::from(message);
        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_function(BenchmarkId::new("contiguous", size), |bench| {
            let mut frames = FrameDecoder::new(size, None).unwrap();
            bench.iter(|| {
                frames
                    .pull(&mut black_box(encoded.clone()))
                    .unwrap()
                    .unwrap()
            });
        });
        group.bench_function(BenchmarkId::new("split", size), |bench| {
            let mut frames = FrameDecoder::new(size, None).unwrap();
            let first = encoded.slice(..encoded.len() / 2);
            let second = encoded.slice(encoded.len() / 2..);
            bench.iter(|| {
                assert!(
                    frames
                        .pull(&mut black_box(first.clone()))
                        .unwrap()
                        .is_none()
                );
                frames
                    .pull(&mut black_box(second.clone()))
                    .unwrap()
                    .unwrap()
            });
        });
    }
}

criterion_group!(benches, framing);
criterion_main!(benches);
