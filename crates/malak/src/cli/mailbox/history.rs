//! Load a recent page before walking the remaining retained history.

use super::{
    PreviewDoor, Update, activity,
    messages::{fetch, readable_channels},
    model::{Address, Channel, Preview, Status},
};
use malak::Error;
use std::collections::{BinaryHeap, HashSet};
use std::ops::Range;
use std::sync::mpsc::SyncSender;

const RECENT_ROWS: usize = 24;

pub(super) type Record = (Address, Option<Preview>);

type ReadPreview<'a> = dyn FnMut(&Address) -> Result<Option<Preview>, Error> + 'a;

struct History<'a> {
    channel: &'a Channel,
    remaining: Range<u64>,
}

pub(super) fn retained<'a>(status: &'a Status, viewer: &str) -> impl Iterator<Item = &'a Channel> {
    status
        .participants
        .iter()
        .find(|person| person.id == viewer)
        .into_iter()
        .flat_map(|person| readable_channels(status.channels.iter(), person))
        .filter(|channel| channel.floor < channel.head)
}

impl History<'_> {
    fn read(&mut self, position: u64, get: &mut ReadPreview<'_>) -> Option<Record> {
        let address = self.channel.address(position);
        match get(&address) {
            Ok(preview) => Some((address, preview)),
            Err(_) => {
                self.remaining = 0..0;
                None
            }
        }
    }
}

pub(super) fn backfill(
    status: Status,
    waiting: Vec<Address>,
    door: PreviewDoor,
    tx: &SyncSender<Update>,
) {
    if let Some((mut session, id)) = door.open()
        && let Some(viewer) = status
            .participants
            .iter()
            .find(|person| person.id == id.as_str())
    {
        let mut channels: Vec<_> = retained(&status, id.as_str())
            .map(|channel| History {
                channel,
                remaining: channel.floor..channel.head,
            })
            .collect();
        let get = &mut |address: &Address| fetch(&mut session, viewer, address);
        let recent = first_page(&mut channels, &waiting, get);
        let prefetched: HashSet<_> = recent.iter().map(|(address, _)| address.clone()).collect();
        if tx
            .send(Update::Recent(Some(id.to_string()), recent))
            .is_err()
        {
            return;
        }
        channels.sort_by_key(|history| history.channel.name != "orders");
        for channel in &mut channels {
            while let Some(position) = channel.remaining.next_back() {
                if prefetched.contains(&channel.channel.address(position)) {
                    continue;
                }
                if let Some((address, preview)) = channel.read(position, get)
                    && tx.send(Update::Backfill(address, preview)).is_err()
                {
                    return;
                }
            }
        }
    } else if tx.send(Update::Recent(None, Vec::new())).is_err() {
        return;
    }
    let _ = tx.send(Update::BackfillDone);
}

fn first_page(
    channels: &mut [History<'_>],
    waiting: &[Address],
    get: &mut ReadPreview<'_>,
) -> Vec<Record> {
    let mut recent = Vec::new();
    let mut heads = BinaryHeap::new();
    let mut pull = |index: usize, channel: &mut History<'_>, heads: &mut BinaryHeap<_>| {
        if let Some(position) = channel.remaining.next_back()
            && let Some((address, preview)) = channel.read(position, get)
        {
            if let Some(preview) = &preview {
                heads.push((preview.at_ms(), index));
            }
            recent.push((address, preview));
        }
    };
    for (index, channel) in channels.iter_mut().enumerate() {
        // Hook probes remain in backfill but cannot contribute to the first Activity page.
        if !activity::is_self_test(&channel.channel.name) {
            pull(index, channel, &mut heads);
        }
    }
    // Each head is a candidate for the newest row. Refill the most recent
    // channel first so a busy channel can supply the rest of the first page.
    for _ in 0..RECENT_ROWS {
        let Some((_, index)) = heads.pop() else { break };
        pull(index, &mut channels[index], &mut heads);
    }
    for address in waiting {
        if let Some(channel) = channels.iter_mut().find(|channel| {
            channel.channel.name == address.channel
                && channel.channel.creation == address.creation
                && channel.remaining.contains(&address.position)
        }) && let Some(record) = channel.read(address.position, get)
        {
            recent.push(record);
        }
    }
    recent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::mailbox::model::tests::status;

    #[test]
    fn first_page_reads_a_bounded_number_of_retained_positions() {
        let mut snapshot = status();
        let channel = &mut snapshot.channels[0];
        channel.floor = 1;
        channel.head = 1_001;
        let mut histories = [History {
            channel,
            remaining: channel.floor..channel.head,
        }];
        let waiting = [channel.address(1)];
        let mut reads = Vec::new();
        let page = first_page(&mut histories, &waiting, &mut |address| {
            reads.push(address.position);
            Ok(Some(Preview {
                sender: "lead".into(),
                sender_trace: None,
                kind: "note".into(),
                sent_at: [0, address.position * 1_000_000],
                trace: None,
                text: None,
                cut_bytes: None,
                has_reply_to: false,
                reply_to: None,
                quote: None,
                body: None,
            }))
        });
        assert!(reads.len() <= 26, "first page made {} GETs", reads.len());
        assert_eq!(reads.first(), Some(&1_000));
        assert_eq!(reads.last(), Some(&1));
        assert_eq!(page.len(), reads.len());
    }
}
