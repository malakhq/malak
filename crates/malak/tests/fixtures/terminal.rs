use std::fs::File;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const WATCHDOG: Duration = Duration::from_secs(60);

#[allow(unsafe_code)]
pub(super) fn terminal_pair(rows: u16, columns: u16) -> (File, File) {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use nix::libc::{openpty, winsize};
    use std::os::fd::FromRawFd;

    let mut size = winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // The outputs point to valid descriptors only on a successful openpty.
    let (master, slave) = unsafe {
        assert_eq!(
            openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size as *mut winsize,
            ),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        (File::from_raw_fd(master), File::from_raw_fd(slave))
    };
    for file in [&master, &slave] {
        fcntl(file, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap();
    }
    (master, slave)
}

pub(super) struct TerminalOutput {
    pub(super) input: File,
    pub(super) chunks: Receiver<Vec<u8>>,
    pub(super) bytes: Vec<u8>,
}

impl TerminalOutput {
    pub(super) fn new(master: File) -> Self {
        let mut output = master.try_clone().unwrap();
        let (send, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = output.read(&mut buf) {
                if n == 0 || send.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            input: master,
            chunks,
            bytes: Vec::new(),
        }
    }

    pub(super) fn press(&mut self, keys: &[u8]) {
        self.input.write_all(keys).unwrap();
    }

    pub(super) fn wait_for(&mut self, expected: &str) {
        let wanted: String = expected.split_whitespace().collect();
        self.wait_for_output(expected, |bytes| terminal_text(bytes).contains(&wanted));
    }

    pub(super) fn wait_for_output(&mut self, expected: &str, seen: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + WATCHDOG;
        while !seen(&self.bytes) {
            let chunk = self
                .chunks
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "expected {expected:?} ({error}): {:?}",
                        terminal_text(&self.bytes)
                    )
                });
            self.bytes.extend(chunk);
        }
    }
}

// Crossterm advances the cursor over blank cells, so skip both CSI sequences
// and whitespace when looking for visible text in its output stream.
pub(super) fn terminal_text(bytes: &[u8]) -> String {
    let mut bytes = bytes.iter().copied().peekable();
    let mut text = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte == 0x1b && bytes.next_if_eq(&b'[').is_some() {
            for control in bytes.by_ref() {
                if (0x40..=0x7e).contains(&control) {
                    break;
                }
            }
        } else if !byte.is_ascii_whitespace() {
            text.push(byte);
        }
    }
    String::from_utf8_lossy(&text).into_owned()
}
