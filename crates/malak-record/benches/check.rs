//! Microbenchmarks for record signing and verification against journal state.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use malak_layers::Position;
use malak_record::{History, RecordKey, Stamp};
use std::hint::black_box;

fn record_check(criterion: &mut Criterion) {
    let key = RecordKey::new(&[1; 32]);
    let position = Position::FIRST;
    let stamp = Stamp {
        run: [2; 16],
        attempt: 0,
    };
    let history = History {
        channel: "jobs/_00000000000000000000000000000001",
        retained: 1..2,
        stamps: &[(position, stamp)],
        voids: &[],
    };
    let metadata = [3; 128];
    let mut group = criterion.benchmark_group("record_check");

    for size in [64, 4096, 1024 * 1024] {
        let body = vec![4; size];
        let record = key
            .seal(history.channel, position, stamp, &metadata, &body)
            .unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::new("sign", size), |bench| {
            bench.iter(|| {
                key.seal(
                    history.channel,
                    position,
                    stamp,
                    &metadata,
                    black_box(&body),
                )
                .unwrap()
            });
        });
        group.bench_function(BenchmarkId::new("verify_current", size), |bench| {
            bench.iter(|| key.current(&history, black_box(&record)).unwrap());
        });
    }
}

criterion_group!(benches, record_check);
criterion_main!(benches);
