use crate::journal_entry::{SavedKeys, StartupEntry};
use crate::{CreationId, Error};
use malak_journal::InspectedJournal;
use std::collections::HashSet;

pub(in crate::board) struct Start {
    pub(super) inspected: InspectedJournal,
    pub(in crate::board) keys: Option<Box<SavedKeys>>,
    lead: Option<Lead>,
    pub(in crate::board) complete: bool,
    pub(in crate::board) run: Option<(u64, [u8; 16])>,
}

enum Lead {
    Direct(CreationId, Option<u64>),
    Local {
        creation: CreationId,
        enrolled_at_ms: Option<u64>,
        uid: u32,
        key: [u8; 32],
        retired: HashSet<[u8; 32]>,
    },
}

impl Start {
    pub(super) fn direct(inspected: InspectedJournal) -> Result<Self, Error> {
        let start = Self::load(inspected)?;
        if start.run.is_some() || matches!(start.lead, Some(Lead::Local { .. })) {
            return Err(existing());
        }
        Ok(start)
    }

    pub(super) fn local(inspected: InspectedJournal, account: u32) -> Result<Self, Error> {
        let start = Self::load(inspected)?;
        match start.lead {
            Some(Lead::Direct(..)) => Err(existing()),
            Some(Lead::Local { uid, .. }) if uid != account => {
                Err(Error::refused("saved lead is under another account"))
            }
            _ => Ok(start),
        }
    }

    pub(super) fn creation(&self) -> Option<CreationId> {
        self.lead.as_ref().map(|lead| match *lead {
            Lead::Direct(creation, _) | Lead::Local { creation, .. } => creation,
        })
    }

    pub(super) fn lead_enrolled_at_ms(&self) -> Option<u64> {
        match self.lead.as_ref()? {
            Lead::Direct(_, at)
            | Lead::Local {
                enrolled_at_ms: at, ..
            } => *at,
        }
    }

    pub(in crate::board) fn lead_uid(&self) -> Option<u32> {
        match self.lead.as_ref()? {
            Lead::Local { uid, .. } => Some(*uid),
            Lead::Direct(..) => None,
        }
    }

    pub(in crate::board) fn current_lead_key(&self) -> Option<&[u8; 32]> {
        match self.lead.as_ref()? {
            Lead::Local { key, .. } => Some(key),
            Lead::Direct(..) => None,
        }
    }

    pub(in crate::board) fn local_key_was_saved(&self, candidate: &[u8; 32]) -> bool {
        matches!(&self.lead, Some(Lead::Local { key, retired, .. })
            if key == candidate || retired.contains(candidate))
    }

    pub(in crate::board) fn retired_lead_keys(&self) -> HashSet<[u8; 32]> {
        match &self.lead {
            Some(Lead::Local { retired, .. }) => retired.clone(),
            _ => HashSet::new(),
        }
    }

    pub(super) fn load(inspected: InspectedJournal) -> Result<Self, Error> {
        let (keys, lead, complete, run) = prefix(&inspected)?;
        Ok(Self {
            inspected,
            keys,
            lead,
            complete,
            run,
        })
    }
}

type InitialRun = Option<(u64, [u8; 16])>;
type Prefix = (Option<Box<SavedKeys>>, Option<Lead>, bool, InitialRun);

fn prefix(inspected: &InspectedJournal) -> Result<Prefix, Error> {
    let mut entries = inspected
        .accepted_entries()
        .map_err(|error| Error::unavailable(format!("board journal: {error}")))?;
    let Some((_, first)) = entries.next() else {
        return Ok((None, None, false, None));
    };
    let StartupEntry::Keys(keys) = StartupEntry::decode(first)? else {
        return Err(order());
    };
    let mut lead = None;
    let mut complete = false;
    for (at, payload) in entries {
        match StartupEntry::decode(payload)? {
            StartupEntry::DirectLead(creation, enrolled_at_ms) if lead.is_none() => {
                lead = Some(Lead::Direct(creation, enrolled_at_ms));
            }
            StartupEntry::LocalLead {
                creation,
                key: new,
                uid,
                enrolled_at_ms,
            } if !complete => {
                if &new == keys.board.public() || &new == keys.owner.public() {
                    return Err(order());
                }
                match &mut lead {
                    None => {
                        lead = Some(Lead::Local {
                            creation,
                            enrolled_at_ms,
                            uid,
                            key: new,
                            retired: HashSet::new(),
                        });
                    }
                    Some(Lead::Local {
                        creation: existing,
                        uid: account,
                        key,
                        retired,
                        ..
                    }) if *existing == creation
                        && *account == uid
                        && *key != new
                        && !retired.contains(&new) =>
                    {
                        retired.insert(std::mem::replace(key, new));
                    }
                    _ => return Err(order()),
                }
            }
            StartupEntry::Complete if !complete && lead.is_some() => complete = true,
            StartupEntry::Run(run) if complete => {
                return Ok((Some(keys), lead, complete, Some((at.sequence, run))));
            }
            _ => return Err(order()),
        }
    }
    Ok((Some(keys), lead, complete, None))
}

fn existing() -> Error {
    Error::refused("the state folder already holds a board")
}

fn order() -> Error {
    Error::unavailable("invalid order of board startup journal entries")
}
