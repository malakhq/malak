use crate::stream::io_error;
use malak_layers::Error;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub(super) fn connect(mut socket: &UnixStream, port: u32) -> Result<(), Error> {
    let timeout = Some(Duration::from_secs(5));
    (|| {
        socket.set_read_timeout(timeout)?;
        socket.set_write_timeout(timeout)?;
        writeln!(socket, "CONNECT {port}")?;
        let mut reply = [0u8; 14]; // "OK " plus a u32 and a newline.
        for index in 0..reply.len() {
            socket.read_exact(&mut reply[index..index + 1])?;
            if reply[index] == b'\n' {
                if valid_reply(&reply[..index]) {
                    socket.set_read_timeout(None)?;
                    return socket.set_write_timeout(None);
                }
                break;
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Firecracker connect reply",
        ))
    })()
    .map_err(|error| io_error("negotiate Firecracker link", &error))
}

fn valid_reply(reply: &[u8]) -> bool {
    let Some(port) = reply.strip_prefix(b"OK ") else {
        return false;
    };
    !port.is_empty()
        && port.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(port).is_ok_and(|port| port.parse::<u32>().is_ok())
}
