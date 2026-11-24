//! Fixtures shared by in-process board tests.

#![allow(dead_code)]

use malak_core::{Action, Error, Grant, Handle, Limits, Owner, Position};
use malak_layers::{
    Claim, Claimed, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use malak_store_memory::MemoryStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn grant(pattern: &str, actions: &[Action]) -> Grant {
    let mut grant = Grant::default();
    grant.allow(pattern.parse().unwrap(), actions.iter().copied());
    grant
}

pub fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.minimum_free_disk_bytes = 0;
    limits
}

pub fn board(limits: Limits) -> Owner {
    Owner::in_memory(MemoryStore::default(), limits).unwrap()
}

pub fn publisher(owner: &Owner, name: &str) -> Handle {
    owner
        .enrol(name, grant("**", &[Action::Send, Action::Create]))
        .unwrap()
}

#[derive(Clone, Default)]
pub struct Probe {
    pub memory: Arc<MemoryStore>,
    pub traffic: Arc<AtomicUsize>,
}

impl Probe {
    pub fn calls(&self) -> usize {
        self.traffic.load(Ordering::Relaxed)
    }

    fn count(&self) {
        self.traffic.fetch_add(1, Ordering::Relaxed);
    }
}

impl Store for Probe {
    fn describe(&self) -> StoreInfo {
        self.memory.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        self.memory.claim(mode)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.count();
        self.memory.append(generation, channel, records)
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        self.count();
        self.memory.read(channel, from, limit)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        self.count();
        self.memory.trim(generation, channel, trim)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn copy_journal_prefix(from: &std::path::Path, to: &std::path::Path, entries: usize) {
    use malak_journal::{Save, StateFolder};

    let source = StateFolder::acquire(from).unwrap().inspect().unwrap();
    let mut output = StateFolder::acquire(to)
        .unwrap()
        .inspect()
        .unwrap()
        .activate()
        .unwrap();
    for (_, bytes) in source.entries().take(entries) {
        output.append(bytes, Save::OnDisk).unwrap();
    }
}
