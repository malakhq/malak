//! Shared transport tests exercised against real Unix sockets.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_layers::{Link, Source, Transport};
use malak_testkit::unix_socket::SocketDir;
use malak_transport_unix::{UnixLink, UnixTransport};

fn pair(socket: &SocketDir) -> (UnixTransport, Link<UnixLink>, Link<UnixLink>) {
    let address = socket.address();
    let transport = UnixTransport::bind(&address).unwrap();
    let client = transport.dial(&address).unwrap();
    let server = transport.accept(&address).unwrap();
    (transport, client, server)
}

#[test]
fn bytes_arrive_in_order() {
    let socket = SocketDir::new().unwrap();
    let (transport, client, server) = pair(&socket);
    malak_testkit::transport::bytes_arrive_in_order(&transport, &client.io, &server.io);
}

#[test]
fn close_is_reported_once() {
    let socket = SocketDir::new().unwrap();
    let (transport, client, server) = pair(&socket);
    malak_testkit::transport::close_is_reported_once(&transport, &server.io, &client.io);
}

#[test]
fn a_local_link_reports_the_calling_account_and_process() {
    let socket = SocketDir::new().unwrap();
    let (_, _, server) = pair(&socket);
    assert_eq!(
        server.source,
        Some(Source::LocalUser(nix::unistd::geteuid().as_raw()))
    );
    assert_eq!(server.process_id, Some(std::process::id()));
}

#[test]
fn bind_leaves_an_existing_socket_path_in_place() {
    let socket = SocketDir::new().unwrap();
    let address = socket.address();
    let transport = UnixTransport::bind(&address).unwrap();

    assert!(UnixTransport::bind(&address).is_err());
    let client = transport.dial(&address).unwrap();
    let accepted = transport.accept(&address).unwrap();
    assert_eq!(client.source, accepted.source);
}
