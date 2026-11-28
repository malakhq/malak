use super::{Board, Handle};
use crate::channel::Channel;
use crate::journal_entry::Entry;
use crate::storage::STORE_WAIT;
use crate::{
    Action, ChannelName, CreationId, Error, GetReply, Message, Position, ReadChannel, ReadReply,
};
use smallvec::SmallVec;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl Handle {
    /// Receive due messages from the named channels, without acknowledging them.
    ///
    /// Pass a zero wait to return immediately if no messages are available.
    pub fn read(
        &self,
        channels: &[&str],
        max_messages: usize,
        wait: Duration,
    ) -> Result<ReadReply, Error> {
        self.call(
            "read",
            || match channels {
                [channel] => channel.parse().ok(),
                _ => None,
            },
            |_| self.read_counted(channels, max_messages, wait, None),
        )
    }

    pub(super) fn read_counted(
        &self,
        channels: &[&str],
        max_messages: usize,
        wait: Duration,
        sink: Option<&mut dyn ReadSink>,
    ) -> Result<ReadReply, Error> {
        if channels.is_empty() {
            return Err(Error::refused("invalid number of channels in read"));
        }
        if channels.len() > self.board.limits.read_channels {
            return Err(Error::refused("invalid number of channels in read").at_limit());
        }
        let table = self.board.table.check();
        let names = channels
            .iter()
            .map(|name| table.resolve_read_channel(&name.parse()?, Some(&self.id)))
            .collect::<Result<SmallVec<[_; 1]>, _>>()?;
        drop(table);
        self.board.read(self, &names, max_messages, wait, sink)
    }

    /// Acknowledge delivery through a position in the current channel creation.
    pub fn ack(&self, channel: &str, creation: CreationId, position: u64) -> Result<(), Error> {
        self.call(
            "ack",
            || channel.parse().ok(),
            |_| self.ack_counted(channel, creation, position),
        )
    }

    pub(super) fn ack_counted(
        &self,
        channel: &str,
        creation: CreationId,
        position: u64,
    ) -> Result<(), Error> {
        let name = self
            .board
            .table
            .check()
            .resolve_read_channel(&channel.parse()?, Some(&self.id))?;
        let position = Position::try_from(position)?;
        let channel = self
            .allowed_channel(Action::Ack, &name)?
            .filter(|channel| channel.creation == creation)
            .ok_or_else(|| Error::refused("channel creation no longer matches"))?;
        let deadline = Instant::now() + STORE_WAIT;
        let history = channel
            .history
            .try_lock_arc_until(deadline)
            .ok_or_else(|| Error::unavailable("channel write did not settle in time"))?;
        let mut history = self.board.ready_history(&channel, history, deadline)?;

        let mut table = self.board.table.check();
        if !table.is_current(&channel) {
            return Err(Error::refused("channel creation no longer matches"));
        }
        channel.available()?;
        if position.get() >= history.head() {
            return Err(Error::refused("position was not given to this reader"));
        }
        let advanced = table.readers.ack(creation, &self.id, position, || {
            self.board.store.save(Entry::Ack {
                creation,
                participant: &self.id,
                position,
            })
        })?;
        if advanced {
            history.note_ack(&channel, &self.id, position);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            channel.activity.acknowledge(&self.id, position);
        }
        drop(table);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if advanced {
            self.board.events.record(
                malak_layers::EventKind::Acked,
                "ack",
                Some(&self.id),
                &name,
                creation,
                Some(position),
            );
        }
        Ok(())
    }
}

pub(super) trait ReadSink {
    fn open(&self) -> bool;
    fn prepare(&mut self, message: &Message, lost_before: u64) -> Result<(), Error>;
    // Called with the authority lock held, immediately before delivery is marked.
    fn give(&mut self, message: &Message) -> Result<bool, Error>;
}

struct Plan {
    next: usize,
    slots: SmallVec<[Slot; 1]>,
}

struct Slot {
    name: ChannelName,
    channel: Option<Arc<Channel>>,
    saved: u64,
    position: u64,
    previous: u64,
    exhausted: bool,
    trimmed_before: Option<u64>,
    voids: u64,
}

struct Candidate {
    slot: usize,
    message: Message,
    lost_before: u64,
}

struct Scan {
    candidates: SmallVec<[Candidate; 1]>,
    next: usize,
}

impl Board {
    fn read(
        self: &Arc<Self>,
        caller: &Handle,
        names: &[ChannelName],
        maximum: usize,
        wait: Duration,
        mut sink: Option<&mut dyn ReadSink>,
    ) -> Result<ReadReply, Error> {
        if caller.has_veto() {
            {
                let mut table = self.table.check();
                caller.validate(&mut table.authority)?;
                for name in names {
                    table.authority.allow(&caller.id, Action::Read, name)?;
                }
            }
            for name in names {
                caller.veto(Action::Read, name)?;
            }
        }
        let maximum = maximum.min(self.limits.read_messages);
        let deadline = Instant::now()
            .checked_add(wait.min(self.limits.read_wait))
            .ok_or_else(|| Error::refused("read duration is out of range"))?;
        let mut first = true;
        loop {
            let revision = self.changes.revision();
            if sink.as_ref().is_some_and(|sink| !sink.open()) {
                return Err(Error::unavailable("read has ended"));
            }
            let Some(mut plan) = self.plan(caller, names, first)? else {
                return Ok(ReadReply {
                    messages: Vec::new(),
                    channels: Vec::new(),
                    held_back: true,
                });
            };
            let scan = self.scan(&mut plan, maximum)?;
            let reply = self.release(caller, names, plan, scan, &mut sink)?;
            if !reply.messages.is_empty()
                || reply.held_back
                || maximum == 0
                || Instant::now() >= deadline
                || !self.changes.wait(revision, deadline)
            {
                return Ok(reply);
            }
            first = false;
        }
    }

    fn plan(
        &self,
        caller: &Handle,
        names: &[ChannelName],
        first: bool,
    ) -> Result<Option<Plan>, Error> {
        let mut table = self.table.check();
        caller.validate(&mut table.authority)?;
        let id = caller.id();
        for name in names {
            if let Err(error) = table.authority.allow(id, Action::Read, name) {
                return if first { Err(error) } else { Ok(None) };
            }
        }

        let mut slots = SmallVec::<[Slot; 1]>::with_capacity(names.len());
        for name in names {
            if slots.iter().any(|slot| &slot.name == name) {
                continue;
            }
            let channel = table.channels.get(name).cloned();
            if let Some(channel) = &channel {
                channel.available()?;
                if !table
                    .readers
                    .has_space(channel.creation, id, self.limits.channel_readers)
                {
                    return Err(
                        Error::unavailable("channel has no place for another reader").at_limit(),
                    );
                }
            }
            slots.push(Slot {
                name: name.clone(),
                channel,
                saved: 0,
                position: 1,
                previous: 0,
                exhausted: false,
                trimmed_before: None,
                voids: 0,
            });
        }

        for slot in &mut slots {
            if let Some(channel) = &slot.channel {
                let floor = channel.floor();
                slot.saved = table.readers.saved(channel.creation, id, floor);
                slot.previous = slot.saved;
                slot.move_floor(floor);
                slot.position = (slot.saved + 1).max(floor);
            }
        }
        let next = table.readers.turn(id, names);
        Ok(Some(Plan { next, slots }))
    }

    fn scan(self: &Arc<Self>, plan: &mut Plan, maximum: usize) -> Result<Scan, Error> {
        let mut scan = Scan {
            candidates: SmallVec::new(),
            next: plan.next,
        };
        let mut remaining = self.limits.read_reply_bytes;
        let mut current = plan.next;
        let mut empty = 0;
        let count = plan.slots.len();

        while scan.candidates.len() < maximum && empty < count {
            let slot = &mut plan.slots[current];
            let next = (current + 1) % count;
            if slot.exhausted || slot.channel.is_none() || slot.position > Position::MAX {
                empty += 1;
                current = next;
                continue;
            }
            let channel = slot.channel.as_ref().unwrap().clone();
            let position = Position::try_from(slot.position).expect("bounded read position");
            match self.fetch(channel, position)? {
                (GetReply::Message(message), bytes) => {
                    let Some(space) = remaining.checked_sub(bytes) else {
                        scan.next = current;
                        break;
                    };
                    scan.candidates.push(Candidate {
                        slot: current,
                        message,
                        lost_before: slot.position - slot.previous - 1,
                    });
                    slot.previous = slot.position;
                    scan.next = next;
                    slot.position += 1;
                    remaining = space;
                    empty = 0;
                    current = next;
                }
                (GetReply::Trimmed { before, .. }, _) => {
                    slot.move_floor(before);
                    slot.position = before;
                }
                (GetReply::Void { .. }, _) => {
                    slot.voids += 1;
                    slot.position += 1;
                }
                (GetReply::Absent | GetReply::NeverWritten { .. }, _) => {
                    slot.exhausted = true;
                }
            }
        }
        Ok(scan)
    }

    fn release(
        &self,
        caller: &Handle,
        names: &[ChannelName],
        plan: Plan,
        scan: Scan,
        sink: &mut Option<&mut dyn ReadSink>,
    ) -> Result<ReadReply, Error> {
        let id = caller.id();
        let connected = sink.is_some();
        let mut direct = (!connected).then(|| self.table.check());
        {
            let mut locked;
            let table = if let Some(table) = &mut direct {
                &mut **table
            } else {
                locked = self.table.check();
                &mut *locked
            };
            caller.validate(&mut table.authority)?;
            for slot in &plan.slots {
                if let Some(channel) = &slot.channel {
                    channel.available()?;
                }
            }
        }

        let mut next = scan.next;
        let mut held_back = false;
        let mut messages = Vec::with_capacity(scan.candidates.len());
        for Candidate {
            slot,
            message,
            lost_before,
        } in scan.candidates
        {
            let permitted = if connected {
                match caller.veto(Action::Read, &message.channel) {
                    Ok(()) => true,
                    Err(error) if error.outcome() == crate::Outcome::Refused => false,
                    Err(error) => return Err(error),
                }
            } else {
                true
            };
            if let Some(sink) = sink {
                sink.prepare(&message, lost_before)?;
            }
            let mut locked;
            let table = if let Some(table) = &mut direct {
                &mut **table
            } else {
                locked = self.table.check();
                &mut *locked
            };
            caller.validate(&mut table.authority)?;
            if !permitted
                || table
                    .authority
                    .allow(id, Action::Read, &message.channel)
                    .is_err()
            {
                held_back = true;
                next = (slot + 1) % plan.slots.len();
                break;
            }
            let channel = plan.slots[slot]
                .channel
                .as_ref()
                .expect("message has a channel");
            if !table.is_current(channel) {
                break;
            }
            if let Some(sink) = sink {
                if !sink.give(&message)? {
                    next = slot;
                    break;
                }
                table.readers.gave(id, &message);
            }
            messages.push(message);
        }
        let mut table = direct.unwrap_or_else(|| self.table.check());
        if messages.is_empty() && !held_back {
            caller.validate(&mut table.authority)?;
            held_back = names
                .iter()
                .any(|name| table.authority.allow(id, Action::Read, name).is_err());
        }
        if !connected {
            for message in &messages {
                table.readers.gave(id, message);
            }
        }
        table.readers.remember(id, names, next);
        drop(table);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        for message in &messages {
            self.events.record(
                malak_layers::EventKind::Read,
                "read",
                Some(id),
                &message.channel,
                message.receipt.creation,
                Some(message.receipt.position),
            );
        }

        let channels = plan
            .slots
            .into_iter()
            .map(|slot| ReadChannel {
                channel: slot.name,
                creation: slot.channel.map(|channel| channel.creation),
                trimmed_before: slot.trimmed_before,
                voids: slot.voids,
            })
            .collect();
        Ok(ReadReply {
            messages,
            channels,
            held_back,
        })
    }
}

impl Slot {
    fn move_floor(&mut self, floor: u64) {
        if floor > self.saved + 1 {
            self.trimmed_before = Some(floor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Limits, Owner, SendRequest};
    use malak_store_memory::MemoryStore;

    struct FullQueue;

    impl ReadSink for FullQueue {
        fn open(&self) -> bool {
            true
        }

        fn prepare(&mut self, _: &Message, _: u64) -> Result<(), Error> {
            Ok(())
        }

        fn give(&mut self, _: &Message) -> Result<bool, Error> {
            Ok(false)
        }
    }

    #[test]
    fn a_task_that_could_not_be_queued_is_not_given() {
        let board = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let lead = board.lead();
        let task = lead
            .send(SendRequest {
                kind: Kind::Task,
                ..SendRequest::note("lead", "task", b"work")
            })
            .unwrap();
        let report = SendRequest::note("lead", "report", b"progress");

        let blocked = lead
            .read_counted(&["lead"], 1, Duration::ZERO, Some(&mut FullQueue))
            .unwrap();
        assert!(blocked.messages.is_empty());
        assert_eq!(lead.send(report).unwrap().trace, None);

        lead.read(&["lead"], 1, Duration::ZERO).unwrap();
        assert_eq!(
            lead.send(SendRequest {
                id: "after-delivery",
                ..report
            })
            .unwrap()
            .trace,
            task.trace
        );
    }
}
