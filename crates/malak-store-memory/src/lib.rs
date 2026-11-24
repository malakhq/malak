//! A volatile implementation of Malak's store contract.

use malak_layers::{
    Claim, Claimed, Durability, Error, Ownership, Position, ReadLimit, ReadResult, Record, Store,
    StoreInfo, Trim, Trimmed,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

/// Opaque records held for the lifetime of this store instance.
///
/// Construct a new instance for each board run. The store holds no files and
/// cannot be claimed for repair.
#[derive(Default)]
pub struct MemoryStore {
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    store_id: Option<[u8; 16]>,
    generation: u64,
    claimed: bool,
    channels: HashMap<String, BTreeMap<Position, Record>>,
}

impl State {
    fn check_writer(&self, generation: u64) -> Result<(), Error> {
        if self.claimed && self.generation == generation {
            Ok(())
        } else {
            Err(Error::unavailable(
                "memory store claim is no longer current",
            ))
        }
    }

    fn take(&mut self) -> Result<Ownership, Error> {
        if self.claimed {
            return Err(Error::unavailable("memory store is already claimed"));
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::unavailable("memory store writer generations exhausted"))?;
        let store_id = match self.store_id {
            Some(id) => id,
            None => {
                let mut id = [0; 16];
                getrandom::fill(&mut id)
                    .map_err(|_| Error::unavailable("could not draw a memory store ID"))?;
                id
            }
        };

        self.store_id = Some(store_id);
        self.generation = generation;
        self.claimed = true;
        Ok(Ownership {
            store_id,
            generation,
            has_records: !self.channels.is_empty(),
            location: None,
        })
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::unavailable("memory store lock is poisoned")
}

impl Store for MemoryStore {
    fn describe(&self) -> StoreInfo {
        StoreInfo {
            durability: Durability::Memory,
            max_record_bytes: usize::MAX,
            slow_writes: false,
        }
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        match mode {
            Claim::Take => Ok(Claimed::Taken(
                self.state.write().map_err(poisoned)?.take()?,
            )),
            Claim::Repair => Err(Error::not_supported("memory storage cannot be repaired")),
            Claim::Settle { generation, .. } => {
                self.state
                    .read()
                    .map_err(poisoned)?
                    .check_writer(generation)?;
                Ok(Claimed::Done)
            }
            Claim::Release { generation } => {
                let mut state = self.state.write().map_err(poisoned)?;
                state.check_writer(generation)?;
                state.claimed = false;
                Ok(Claimed::Done)
            }
        }
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        let mut state = self.state.write().map_err(poisoned)?;
        state.check_writer(generation)?;

        let Some(first) = records.first() else {
            return Ok(());
        };
        let last = state
            .channels
            .get(channel)
            .and_then(BTreeMap::last_key_value);
        if last.is_some_and(|(last, _)| *last >= first.position)
            || records
                .windows(2)
                .any(|pair| pair[0].position >= pair[1].position)
        {
            return Err(Error::unavailable("record positions must increase"));
        }

        let records = records
            .iter()
            .map(|record| (record.position, record.clone()));
        if let Some(entries) = state.channels.get_mut(channel) {
            entries.extend(records);
        } else {
            state.channels.insert(channel.into(), records.collect());
        }
        Ok(())
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        let state = self.state.read().map_err(poisoned)?;
        let records = state
            .channels
            .get(channel)
            .map_or_else(Vec::new, |entries| {
                entries
                    .range(from..)
                    .take_while(|(position, _)| {
                        limit.before.is_none_or(|before| **position < before)
                    })
                    .take(limit.records)
                    .map(|(_, record)| record.clone())
                    .collect()
            });
        Ok(ReadResult::Records {
            records,
            unparseable: false,
        })
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let mut state = self.state.write().map_err(poisoned)?;
        state.check_writer(generation)?;
        let Trim::Keep(range) = trim else {
            return Err(Error::not_supported("memory storage cannot be repaired"));
        };

        if let Some(entries) = state.channels.get_mut(channel) {
            entries.retain(|position, _| range.contains(&position.get()));
            if entries.is_empty() {
                state.channels.remove(channel);
            }
        }
        Ok(Trimmed::Done)
    }
}
