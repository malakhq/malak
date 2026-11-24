//! Warm Direct calls through the same handles as embedding programs use.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use malak::{Handle, Limits, Owner, SendRequest};
use malak_store_memory::MemoryStore;
use std::hint::black_box;
use std::time::{Duration, Instant};

fn lead() -> Handle {
    let mut settings = Limits::default();
    settings.lead_calls_per_minute = usize::MAX;
    settings.daily_storage_bytes = u64::MAX;
    Owner::in_memory(MemoryStore::default(), settings)
        .unwrap()
        .lead()
}

fn direct(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("direct");
    for size in [64, 4096, 1024 * 1024] {
        let body = vec![42; size];
        group.bench_function(BenchmarkId::new("send", size), |bench| {
            bench.iter_custom(|iterations| {
                let chunk = (16 * 1024 * 1024 / size).min(1024) as u64;
                let mut measured = Duration::ZERO;
                let mut remaining = iterations;
                while remaining > 0 {
                    let lead = lead();
                    lead.send(SendRequest::note("lead", "warmup", &body))
                        .unwrap();
                    let batch = remaining.min(chunk);
                    let ids: Vec<_> = (0..batch).map(|index| index.to_string()).collect();
                    let start = Instant::now();
                    for id in &ids {
                        black_box(lead.send(SendRequest::note("lead", id, &body)).unwrap());
                    }
                    measured += start.elapsed();
                    remaining -= batch;
                }
                measured
            });
        });
        let lead = lead();
        lead.send(SendRequest::note("lead", "retained", &body))
            .unwrap();
        group.bench_function(BenchmarkId::new("get", size), |bench| {
            bench.iter(|| black_box(lead.get("lead", 1).unwrap()));
        });
        group.bench_function(BenchmarkId::new("read", size), |bench| {
            bench.iter(|| black_box(lead.read(&["lead"], 1, Duration::ZERO).unwrap()));
        });
    }
}

criterion_group!(benches, direct);
criterion_main!(benches);
