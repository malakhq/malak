//! CPU-side timing of the two Noise messages without transport or attach.

use criterion::{Criterion, criterion_group, criterion_main};
use malak_proto::{Binding, Entrance, Initiator, KeyPair, Responder};
use std::hint::black_box;

fn handshake(criterion: &mut Criterion) {
    let caller = KeyPair::generate().unwrap();
    let board = KeyPair::generate().unwrap();
    let binding = Binding {
        version: 1,
        entrance: Entrance::General,
    };

    criterion.bench_function("noise_ik", |bench| {
        bench.iter(|| {
            let (initiator, opening) =
                Initiator::open(black_box(binding), caller.private(), board.public()).unwrap();
            let responder =
                Responder::read(binding, board.private(), board.public(), &opening).unwrap();
            let (board_cipher, reply) = responder.reply().unwrap();
            let caller_cipher = initiator.finish(&reply).unwrap();
            black_box((caller_cipher, board_cipher));
        });
    });
}

criterion_group!(benches, handshake);
criterion_main!(benches);
