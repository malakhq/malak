use crate::{ChannelName, CreationId, Error, Kind, Message, ParticipantId, Position, TraceId};
use std::collections::{HashMap, VecDeque};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Instant;

const LISTS: usize = 16;

#[derive(Default)]
pub(crate) struct Readers {
    by_creation: HashMap<CreationId, HashMap<ParticipantId, Reader>>,
    turns: HashMap<ParticipantId, VecDeque<Turn>>,
    tasks: HashMap<ParticipantId, TaskContext>,
}

struct Reader {
    saved: u64,
    given: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct WorkingTask {
    position: Position,
    pub(crate) trace: Option<TraceId>,
}

impl WorkingTask {
    pub(crate) fn from_message(message: &Message) -> Option<Self> {
        (message.kind == Kind::Task).then_some(Self {
            position: message.receipt.position,
            trace: message.receipt.trace,
        })
    }
}

enum TaskContext {
    Given(CreationId, WorkingTask),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Restoring(CreationId, Instant),
}

impl TaskContext {
    fn creation(&self) -> CreationId {
        match *self {
            Self::Given(creation, _) => creation,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Restoring(creation, _) => creation,
        }
    }
}

struct Turn {
    list: Vec<ChannelName>,
    next: usize,
}

impl Readers {
    pub(crate) fn forget_creation(&mut self, creation: CreationId) {
        self.by_creation.remove(&creation);
        self.tasks.retain(|_, task| task.creation() != creation);
    }

    pub(crate) fn has_space(
        &self,
        creation: CreationId,
        id: &ParticipantId,
        maximum: usize,
    ) -> bool {
        self.by_creation
            .get(&creation)
            .map_or(maximum > 0, |readers| {
                readers.contains_key(id) || readers.len() < maximum
            })
    }

    pub(crate) fn saved(&mut self, creation: CreationId, id: &ParticipantId, floor: u64) -> u64 {
        self.by_creation
            .entry(creation)
            .or_default()
            .entry(id.clone())
            .or_insert(Reader {
                saved: floor - 1,
                given: 0,
            })
            .saved
    }

    pub(crate) fn gave(&mut self, id: &ParticipantId, message: &Message) {
        let crate::Receipt {
            creation, position, ..
        } = message.receipt;
        let reader = self
            .by_creation
            .get_mut(&creation)
            .and_then(|readers| readers.get_mut(id))
            .expect("read plan registered the current reader");
        reader.given = reader.given.max(position.get());

        if let Some(task) = WorkingTask::from_message(message)
            && is_own_inbox(id, &message.channel)
            && !matches!(self.tasks.get(id), Some(TaskContext::Given(_, previous)) if previous.position >= position)
        {
            self.tasks
                .insert(id.clone(), TaskContext::Given(creation, task));
        }
    }

    pub(crate) fn send_task(
        &self,
        sender: &ParticipantId,
        channel: &ChannelName,
    ) -> Result<Option<WorkingTask>, Error> {
        match self.tasks.get(sender) {
            Some(TaskContext::Given(_, task)) => Ok(Some(*task)),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Some(TaskContext::Restoring(_, deadline)) if Instant::now() < *deadline => Err(
                Error::unavailable("working task trace is still being restored"),
            ),
            _ => Ok(self.recipient_task(channel)),
        }
    }

    fn recipient_task(&self, channel: &ChannelName) -> Option<WorkingTask> {
        let id = participant_inbox_id(channel)?;
        match self.tasks.get(id) {
            Some(TaskContext::Given(_, task)) => Some(*task),
            _ => None,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn begin_task_restore(
        &mut self,
        id: &ParticipantId,
        creation: CreationId,
        floor: u64,
        deadline: Instant,
    ) -> Option<u64> {
        let saved = self.by_creation.get(&creation)?.get(id)?.saved;
        if saved < floor {
            return None;
        }
        self.tasks
            .insert(id.clone(), TaskContext::Restoring(creation, deadline));
        Some(saved)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn finish_task_restore(
        &mut self,
        id: ParticipantId,
        creation: CreationId,
        task: Option<WorkingTask>,
    ) {
        let Some(TaskContext::Restoring(pending_creation, deadline)) = self.tasks.get(&id) else {
            return;
        };
        if *pending_creation != creation {
            return;
        }
        if let Some(task) = task.filter(|_| Instant::now() < *deadline) {
            self.tasks.insert(id, TaskContext::Given(creation, task));
        } else {
            self.tasks.remove(&id);
        }
    }

    pub(crate) fn ack(
        &mut self,
        creation: CreationId,
        id: &ParticipantId,
        position: Position,
        save: impl FnOnce() -> Result<(), Error>,
    ) -> Result<bool, Error> {
        let reader = self
            .by_creation
            .get_mut(&creation)
            .and_then(|readers| readers.get_mut(id))
            .filter(|reader| position.get() <= reader.given)
            .ok_or_else(|| Error::refused("position was not given to this reader"))?;
        if position.get() > reader.saved {
            save()?;
            reader.saved = position.get();
            return Ok(true);
        }
        Ok(false)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore(
        &mut self,
        creation: CreationId,
        id: ParticipantId,
        position: Position,
    ) -> Result<(), Error> {
        let reader = self
            .by_creation
            .entry(creation)
            .or_default()
            .entry(id)
            .or_insert(Reader { saved: 0, given: 0 });
        if position.get() <= reader.saved {
            return Err(Error::unavailable("invalid restored reader position"));
        }
        reader.saved = position.get();
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn positions(
        &self,
        creation: CreationId,
    ) -> std::collections::BTreeMap<ParticipantId, u64> {
        self.by_creation
            .get(&creation)
            .into_iter()
            .flat_map(|readers| {
                readers
                    .iter()
                    .map(|(id, reader)| (id.clone(), reader.saved))
            })
            .collect()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn behind(&self, creation: CreationId, position: u64) -> Vec<ParticipantId> {
        let mut readers: Vec<_> = self
            .by_creation
            .get(&creation)
            .into_iter()
            .flat_map(|readers| readers.iter())
            .filter(|(_, reader)| reader.saved < position)
            .map(|(id, _)| id.clone())
            .collect();
        readers.sort_unstable();
        readers
    }

    pub(crate) fn revoke(&mut self, id: &ParticipantId) {
        self.turns.remove(id);
        self.tasks.remove(id);
        self.by_creation.retain(|_, readers| {
            readers.remove(id);
            !readers.is_empty()
        });
    }

    pub(crate) fn turn(&mut self, id: &ParticipantId, list: &[ChannelName]) -> usize {
        let lists = self.turns.entry(id.clone()).or_default();
        let turn = lists
            .iter()
            .position(|turn| turn.list == list)
            .and_then(|found| lists.remove(found))
            .unwrap_or_else(|| Turn {
                list: list.to_vec(),
                next: 0,
            });
        let next = turn.next;
        lists.push_front(turn);
        lists.truncate(LISTS);
        next
    }

    pub(crate) fn remember(&mut self, id: &ParticipantId, list: &[ChannelName], next: usize) {
        self.turn(id, list);
        self.turns.get_mut(id).unwrap().front_mut().unwrap().next = next;
    }
}

fn is_own_inbox(id: &ParticipantId, channel: &ChannelName) -> bool {
    if id.is_lead() {
        return channel.as_str() == "lead";
    }
    participant_inbox_id(channel) == Some(id.as_str())
}

fn participant_inbox_id(channel: &ChannelName) -> Option<&str> {
    channel
        .as_str()
        .strip_prefix("to/")
        .filter(|id| *id != "lead")
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{Durability, Grade};
    use std::time::{Duration, SystemTime};

    #[test]
    fn a_live_task_keeps_precedence_over_a_pending_restore() {
        let mut readers = Readers::default();
        let lead = ParticipantId::lead();
        let creation = CreationId::from_bytes([1; 16]);
        let first = Position::try_from(1).unwrap();
        let second = Position::try_from(2).unwrap();
        let older = TraceId::parse("AB");
        let live = TraceId::parse("CD");
        readers.restore(creation, lead.clone(), first).unwrap();
        readers
            .begin_task_restore(&lead, creation, 1, Instant::now() + Duration::from_secs(5))
            .unwrap();

        readers.gave(
            &lead,
            &Message {
                channel: lead.inbox(),
                id: "live".parse().unwrap(),
                kind: Kind::Task,
                headers: Vec::new(),
                body: Vec::new().into(),
                sender: lead.clone(),
                sender_name: "lead".parse().unwrap(),
                sender_trace: None,
                agent_session_id: None,
                earlier_session: None,
                record_run: [0; 16],
                grade: Grade::Proven,
                sent_at: SystemTime::UNIX_EPOCH,
                receipt: crate::Receipt {
                    position: second,
                    creation,
                    durability: Durability::Synced,
                    trace: live,
                    duplicate: false,
                },
            },
        );
        readers.finish_task_restore(
            lead.clone(),
            creation,
            Some(WorkingTask {
                position: first,
                trace: older,
            }),
        );

        let task = readers.send_task(&lead, &lead.inbox()).unwrap().unwrap();
        assert_eq!(task.trace, live);
    }
}
