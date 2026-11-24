//! Restore terminal modes when leaving the mailbox.

use ratatui::crossterm::{ExecutableCommand, event::DisableMouseCapture, terminal::EnableLineWrap};
use std::io;

pub(super) struct RestoreTerminal {
    pub(super) live_view: bool,
}

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        if self.live_view {
            let mut stdout = io::stdout().lock();
            let _ = stdout.execute(DisableMouseCapture);
            let _ = stdout.execute(EnableLineWrap);
        }
        let _ = ratatui::try_restore();
    }
}
