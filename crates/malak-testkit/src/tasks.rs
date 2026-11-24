//! Deadline-bounded waits for test threads that own their fixtures.

use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

/// A test thread whose completion can be observed without blocking indefinitely.
pub struct Task<T>(Receiver<std::thread::Result<T>>);

/// Run work with owned inputs, allowing the surrounding test to leave on timeout.
pub fn spawn<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Task<T> {
    let (done, completion) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(panic::catch_unwind(AssertUnwindSafe(work)));
    });
    Task(completion)
}

impl<T> Task<T> {
    /// Wait up to the supplied duration, propagating a panic from the worker.
    pub fn wait(self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match self.0.recv_timeout(timeout)? {
            Ok(result) => Ok(result),
            Err(panic) => panic::resume_unwind(panic),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_task_does_not_hold_up_the_test() {
        let (release, gate) = mpsc::channel::<()>();
        let worker = spawn(move || gate.recv());

        assert_eq!(
            worker.wait(Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        );
        release.send(()).unwrap();
    }
}
