use bytes::{Buf, Bytes};
use std::collections::VecDeque;
use std::time::Duration;

pub(crate) struct Replies {
    frames: VecDeque<Queued>,
    message_bytes: usize,
    maximum: usize,
    closings: usize,
    pressure: bool,
    last_progress: Option<Duration>,
}

struct Queued {
    bytes: Bytes,
    closing: bool,
}

impl Replies {
    pub(crate) fn new(maximum: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            message_bytes: 0,
            maximum,
            closings: 0,
            pressure: false,
            last_progress: None,
        }
    }

    pub(crate) fn pressure(&self) -> bool {
        self.pressure
    }

    pub(crate) fn closings(&self) -> usize {
        self.closings
    }

    pub(crate) fn message_bytes(&self) -> usize {
        self.message_bytes
    }

    pub(crate) fn stalled(&self, elapsed: Duration, timeout: Duration) -> bool {
        self.deadline(timeout).is_some_and(|at| elapsed >= at)
    }

    pub(crate) fn deadline(&self, timeout: Duration) -> Option<Duration> {
        self.last_progress.map(|last| last.saturating_add(timeout))
    }

    pub(crate) fn message(&mut self, bytes: Bytes, elapsed: Duration) -> bool {
        if self.pressure || bytes.len() > self.maximum - self.message_bytes {
            self.pressure = self.message_bytes > self.maximum / 2;
            return false;
        }
        self.message_bytes += bytes.len();
        self.push(bytes, false, elapsed);
        true
    }

    pub(crate) fn closing(&mut self, bytes: Bytes, elapsed: Duration) {
        self.closings += 1;
        self.push(bytes, true, elapsed);
    }

    fn push(&mut self, bytes: Bytes, closing: bool, elapsed: Duration) {
        if self.frames.is_empty() {
            self.last_progress = Some(elapsed);
        }
        self.frames.push_back(Queued { bytes, closing });
    }

    pub(crate) fn front(&self) -> Option<&[u8]> {
        self.frames.front().map(|frame| frame.bytes.as_ref())
    }

    pub(crate) fn progress(
        &mut self,
        frame_bytes: usize,
        wire_bytes: usize,
        elapsed: Duration,
    ) -> bool {
        let Some(front) = self.frames.front_mut() else {
            return frame_bytes == 0 && wire_bytes == 0;
        };
        if frame_bytes > front.bytes.len() || frame_bytes > wire_bytes {
            return false;
        }
        if wire_bytes > 0 {
            self.last_progress = Some(elapsed);
        }
        if !front.closing {
            self.message_bytes -= frame_bytes;
            if self.message_bytes <= self.maximum / 2 {
                self.pressure = false;
            }
        }
        front.bytes.advance(frame_bytes);
        if front.bytes.is_empty() {
            self.closings -= usize::from(front.closing);
            self.frames.pop_front();
            if self.frames.is_empty() {
                self.last_progress = None;
            }
        }
        true
    }
}
