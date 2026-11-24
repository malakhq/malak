//! Process-kill test checkpoints.

const CHECKPOINT: &str = "malak test checkpoint reached";

/// Run a test subprocess, wait for its checkpoint, then kill and reap it.
pub fn kill_after_checkpoint(command: &mut std::process::Command, checkpoint: &std::path::Path) {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Stdio};

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Process(command.stdout(Stdio::piped()).spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    assert!(
        BufReader::new(stdout)
            .lines()
            .any(|line| line.unwrap() == CHECKPOINT),
        "child exited before {checkpoint:?}"
    );
    assert!(checkpoint.is_file());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

/// Mark a checkpoint and suspend this subprocess until it is killed.
pub fn pause_at_checkpoint(checkpoint: &std::path::Path) -> ! {
    use std::io::{self, Write};

    std::fs::write(checkpoint, b"ready").unwrap();
    // Write directly to the pipe so libtest's print capture cannot buffer it.
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "\n{CHECKPOINT}").unwrap();
    stdout.flush().unwrap();
    drop(stdout);
    loop {
        std::thread::park();
    }
}
