use crate::journal_entry::Entry;
use crate::{Durability, Error, Position};
use malak_journal::{Checkpoint, Journal, Save};

pub(super) struct Writer {
    journal: Journal,
    changed: Checkpoint,
    synced: Checkpoint,
    confirmed: Checkpoint,
}

impl Writer {
    pub(super) fn new(journal: Journal, confirmed: Checkpoint) -> Self {
        let head = journal.head();
        Self {
            journal,
            changed: head,
            synced: head,
            confirmed,
        }
    }

    pub(super) fn writable(&self) -> bool {
        self.journal.writable()
    }

    pub(super) fn save(&mut self, entry: Entry<'_>, durability: Durability) -> Result<(), Error> {
        self.changed = self.append(&entry, entry.level(durability))?;
        self.note_sync();
        Ok(())
    }

    pub(super) fn hand_witness(&mut self) -> Result<Option<Checkpoint>, Error> {
        let at = self.synced;
        if at.sequence <= self.confirmed.sequence {
            return Ok(None);
        }
        #[cfg(test)]
        crate::durable_kill::hit("witness-before-handing");
        self.append(&Entry::WitnessHanded(at), Save::OnDisk)?;
        #[cfg(test)]
        crate::durable_kill::hit("witness-handed");
        self.note_sync();
        Ok(Some(at))
    }

    pub(super) fn confirm_witness(
        &mut self,
        at: Checkpoint,
        position: Position,
    ) -> Result<(), Error> {
        self.append(&Entry::WitnessPublished { at, position }, Save::Written)?;
        #[cfg(test)]
        crate::durable_kill::hit("witness-confirmed");
        self.confirmed = at;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn confirmed_witness(&self) -> Checkpoint {
        self.confirmed
    }

    fn append(&mut self, entry: &Entry<'_>, save: Save) -> Result<Checkpoint, Error> {
        self.journal
            .append(&entry.encode(), save)
            .map_err(|error| Error::unavailable(format!("board journal: {error}")))
    }

    fn note_sync(&mut self) {
        if self.changed.sequence <= self.journal.synced_through() {
            self.synced = self.changed;
        }
    }
}
