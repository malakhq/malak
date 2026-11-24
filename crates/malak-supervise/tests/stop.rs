//! Operating-system stop checks for the starter's handles.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use malak_supervise::{Report, start_worker};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
                "stop-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            let recipe = path.join("report");
            fs::write(&recipe, "#!/bin/sh\nprintf '%s' \"$1\" > \"$2.status\"\n").unwrap();
            fs::set_permissions(&recipe, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn report(&self) -> Report {
            Report {
                handover: self.0.join("handover"),
                id: "worker-result".into(),
                directory: self.0.clone(),
                recipe: self.0.join("report"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn stopping_the_group_reports_the_workers_signal_exit() {
        let fixture = Fixture::new();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let mut worker = start_worker(command, fixture.report()).unwrap();

        let status = worker.stop().unwrap();

        assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
        assert_eq!(
            fs::read_to_string(fixture.0.join("out.status")).unwrap(),
            "137"
        );
    }

    #[test]
    fn waiting_reports_the_workers_actual_exit_code() {
        let fixture = Fixture::new();
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let mut worker = start_worker(command, fixture.report()).unwrap();

        let status = worker.wait().unwrap();

        assert_eq!(status.code(), Some(7));
        assert_eq!(
            fs::read_to_string(fixture.0.join("out.status")).unwrap(),
            "7"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_vm_stop_signals_the_pinned_host_child() {
    let mut command = Command::new("sleep");
    command.arg("30");
    let mut vm = malak_supervise::start_vm(command, nix::unistd::geteuid().as_raw()).unwrap();

    let status = vm.stop().unwrap();

    assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
}
