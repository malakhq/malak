//! Byte delivery checks shared by transport implementations.

use malak_layers::{Received, Transport};

/// Check that tiny and large writes arrive in order, once, with their exact bytes.
///
/// Supply a fresh connected pair. A writer can block until the reader takes
/// bytes, so the test drives the two ends on different threads.
pub fn bytes_arrive_in_order<T: Transport>(
    transport: &T,
    left: &T::Connection,
    right: &T::Connection,
) {
    let payload: Vec<u8> = (0..300_000_u32)
        .map(|index| index.wrapping_add(index >> 8) as u8)
        .collect();
    let tail = b"tail";
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for part in [b"x".as_slice(), &payload, tail] {
                let mut remaining = part;
                while !remaining.is_empty() {
                    let written = transport.send(left, remaining).unwrap();
                    assert!(written > 0);
                    remaining = &remaining[written..];
                }
            }
            transport.send(left, &[]).unwrap();
        });

        let mut received = Vec::new();
        let mut buffer = [0; 7013];
        while let Received::Bytes(count) = transport.receive(right, &mut buffer).unwrap() {
            received.extend_from_slice(&buffer[..count.get()]);
        }
        assert_eq!(received, [b"x".as_slice(), &payload, tail].concat());
    });
}

/// Check that the peer reports closure just once.
pub fn close_is_reported_once<T: Transport>(
    transport: &T,
    left: &T::Connection,
    right: &T::Connection,
) {
    assert_eq!(transport.send(left, &[]).unwrap(), 0);
    let mut buffer = [0; 1];
    assert_eq!(
        transport.receive(right, &mut buffer).unwrap(),
        Received::Closed
    );
    assert!(transport.receive(right, &mut buffer).is_err());
}
