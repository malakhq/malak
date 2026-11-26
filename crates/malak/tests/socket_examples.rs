//! The socket client example talks to the socket board example, over real sockets.

#![cfg(all(
    feature = "unix",
    feature = "markdown",
    feature = "key",
    any(target_os = "linux", target_os = "macos")
))]

use malak_testkit::unix_socket::SocketDir;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

fn example(name: &str) -> Command {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    Command::new(path.join("examples").join(name))
}

/// A running `socket_board` that is killed when dropped.
struct Board {
    child: Child,
    folder: PathBuf,
    _root: SocketDir,
}

impl Board {
    fn start() -> Self {
        let root = SocketDir::new().unwrap();
        let folder = root.path().join("board");
        let mut child = example("socket_board")
            .arg(&folder)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("serving;"), "{line}");
        Self {
            child,
            folder,
            _root: root,
        }
    }

    fn client(&self) -> Output {
        example("socket_client").arg(&self.folder).output().unwrap()
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_socket_client_sends_reads_and_acknowledges_on_the_socket_board() {
    let board = Board::start();

    let output = board.client();

    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert_eq!(
        text,
        "sent to lead #1\nread lead #1: hello over a socket\nacknowledged through lead #1\n"
    );
}

#[test]
fn a_second_client_run_reads_only_what_the_first_acknowledged_past() {
    let board = Board::start();
    board.client();

    let text = String::from_utf8(board.client().stdout).unwrap();

    assert_eq!(
        text,
        "sent to lead #2\nread lead #2: hello over a socket\nacknowledged through lead #2\n"
    );
}
