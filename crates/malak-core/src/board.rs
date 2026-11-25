use crate::authority::{Authority, AuthorityLock, Verified};
use crate::changes::Changes;
use crate::channel::{Channel, LockedHistory, Write};
use crate::journal_entry::Entry;
use crate::quota::Quota;
use crate::readers::Readers;
use crate::record_fields::{Fields, RecordSender, RetryInput, trace_header};
use crate::storage::{STORE_WAIT, StoreLease};
use crate::workers::Workers;
use crate::{
    Action, ChannelName, CreationId, Error, GetReply, Grade, Grant, Header, Kind, Limits,
    ParticipantId, ParticipantName, Position, Receipt, SendContent, SendRequest, TraceId, random,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use malak_layers::EventKind;
use malak_layers::{Identity, Mechanism, SenderPeer, Store};
use malak_record::{HashedBody, RecordKey};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use zeroize::Zeroizing;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use events::OwnerTarget;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod durable;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use durable::RecoveryReport;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod events;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod links;
mod local;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use local::LocalStartup;
mod read;
mod remove;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod status;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod stop;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use status::{BoardStatus, ChannelStatus, ParticipantStatus};

pub use local::{LocalConfig, LocalEntrances, LocalOpening, LocalOwner, LocalParticipant};

/// The embedding program's administrative capability for an in-process board.
pub struct Owner {
    board: Arc<Board>,
    lead: ParticipantId,
}

/// An in-process capability for one enrolled participant.
#[derive(Clone)]
pub struct Handle {
    board: Arc<Board>,
    id: ParticipantId,
    proof: Option<CallProof>,
}

#[derive(Clone)]
struct CallProof {
    identity: Arc<Verified>,
    veto: Option<Arc<dyn Mechanism>>,
    peer: Arc<SenderPeer>,
}

struct Board {
    table: AuthorityLock<Table>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    published: arc_swap::ArcSwap<status::Published>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pending_repair: Option<String>,
    limits: Limits,
    run: [u8; 16],
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    started_at_ms: u64,
    key: Arc<RecordKey>,
    quota: Arc<Mutex<Quota>>,
    store: Arc<StoreLease>,
    workers: Arc<Workers>,
    volume: PathBuf,
    changes: Arc<Changes>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    events: Arc<events::Events>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    links: links::Links,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    local_mechanism: std::sync::OnceLock<Arc<dyn Mechanism>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    upgrade_changelog: std::sync::OnceLock<&'static str>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    stopped: std::sync::atomic::AtomicBool,
    retention_thread: Mutex<Option<std::thread::Thread>>,
}

struct Table {
    authority: Authority,
    channels: HashMap<ChannelName, Arc<Channel>>,
    counts: HashMap<ParticipantId, usize>,
    creators: HashMap<CreationId, ParticipantId>,
    removals: HashMap<CreationId, Arc<Channel>>,
    readers: Readers,
}

struct BoardState {
    table: Table,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pending_repair: Option<String>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    lead_session: Option<crate::LeadSession>,
    run: [u8; 16],
    quota: Arc<Mutex<Quota>>,
}

fn owner_save_deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn new_participant_id() -> Result<ParticipantId, Error> {
    Ok(format!("{:032x}", u128::from_be_bytes(random()?))
        .parse()
        .expect("random lowercase hexadecimal participant ID"))
}

fn validate_start(limits: &Limits) -> Result<(), Error> {
    limits.validate()?;
    if limits.board_channels == 0 {
        return Err(Error::refused("the lead needs an inbox"));
    }
    Ok(())
}

impl Owner {
    /// Start an ephemeral board for Direct handles with an empty memory store.
    ///
    /// The board takes exclusive ownership of the supplied store instance.
    pub fn in_memory(store: impl Store + 'static, limits: Limits) -> Result<Self, Error> {
        validate_start(&limits)?;
        let volume = std::env::current_dir()
            .map_err(|_| Error::unavailable("could not find the board's disk volume"))?;
        let workers = Arc::new(Workers::default());
        let (store, _) = StoreLease::take_new(store, workers.clone(), false)?;
        let secret = Zeroizing::new(random::<32>()?);
        let run = random()?;
        let creation = CreationId::from_bytes(random()?);
        let table = Table::first(
            creation,
            run,
            &limits,
            Some(crate::epoch_millis(SystemTime::now())),
        );
        Self::assemble(
            limits,
            store,
            workers,
            Arc::new(RecordKey::new(&secret)),
            volume,
            run,
            table,
        )
    }

    fn assemble(
        limits: Limits,
        store: StoreLease,
        workers: Arc<Workers>,
        key: Arc<RecordKey>,
        volume: PathBuf,
        run: [u8; 16],
        table: Table,
    ) -> Result<Self, Error> {
        let state = BoardState {
            table,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pending_repair: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            lead_session: None,
            run,
            quota: Arc::new(Mutex::new(Quota::default())),
        };
        Self::assemble_state(limits, store, workers, key, volume, state)
    }

    fn assemble_state(
        limits: Limits,
        store: StoreLease,
        workers: Arc<Workers>,
        key: Arc<RecordKey>,
        volume: PathBuf,
        state: BoardState,
    ) -> Result<Self, Error> {
        let BoardState {
            table,
            run,
            quota,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pending_repair,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            lead_session,
        } = state;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        store.lead_session.store(lead_session.map(Arc::new));
        let store = Arc::new(store);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let events = store.events.clone();
        let board = Arc::new(Board {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            published: arc_swap::ArcSwap::from_pointee(table.snapshot()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pending_repair,
            table: AuthorityLock::new(table),
            limits,
            run,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            started_at_ms: crate::epoch_millis(SystemTime::now()),
            key,
            quota,
            store,
            workers,
            volume,
            changes: Arc::new(Changes::default()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            events,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            links: links::Links::default(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            local_mechanism: std::sync::OnceLock::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            upgrade_changelog: std::sync::OnceLock::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            stopped: std::sync::atomic::AtomicBool::new(false),
            retention_thread: Mutex::new(None),
        });
        board.start_retention()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        board.events.start()?;
        Ok(Self {
            lead: ParticipantId::lead(),
            board,
        })
    }

    /// Set the changelog the board quotes in the upgrade notice it posts to the lead.
    ///
    /// The program that starts the board supplies the text, so the board names
    /// no program's commands itself. The first call wins; without one, the
    /// notice lists no changes.
    #[doc(hidden)]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn set_upgrade_changelog(&self, changelog: &'static str) {
        let _ = self.board.upgrade_changelog.set(changelog);
    }

    /// Issue a participant handle for the lead.
    pub fn lead(&self) -> Handle {
        self.issue(self.lead.clone())
    }

    /// Issue a handle for an existing active participant.
    pub fn handle(&self, id: &ParticipantId) -> Result<Handle, Error> {
        self.board.table.check().authority.active(id)?;
        Ok(self.issue(id.clone()))
    }

    /// Enrol a new Direct participant and create its inbox in one change.
    pub fn enrol(&self, name: &str, grant: Grant) -> Result<Handle, Error> {
        let result = self.enrol_direct(name, grant);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "ENROL",
            OwnerTarget::Name(Some(name)),
            result.as_ref().err(),
        );
        result
    }

    fn enrol_direct(&self, name: &str, grant: Grant) -> Result<Handle, Error> {
        let name: ParticipantName = name.parse()?;
        let deadline = owner_save_deadline();
        self.board
            .table
            .save_until(deadline)?
            .authority
            .check_enrol_name(&name)?;
        loop {
            let id = new_participant_id()?;
            let inbox = id.inbox();
            let channel = self.board.channel(inbox.clone())?;
            let mut table = self.board.table.save_until(deadline)?;
            if !table.enrol_ready(&name, &id, &inbox, self.board.limits.board_channels)? {
                continue;
            }
            let enrolled_at_ms = crate::epoch_millis(SystemTime::now());
            self.board.store.save(Entry::Enrol {
                participant: &id,
                name: &name,
                grant: &grant,
                inbox: channel.creation,
                enrolled_at_ms,
            })?;
            table.channels.insert(inbox, channel);
            table
                .authority
                .insert(id.clone(), name, grant, Some(enrolled_at_ms));
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.board.publish(&table);
            drop(table);
            self.board.changes.notify();
            return Ok(self.issue(id));
        }
    }

    /// Replace the entire grant for an active participant.
    pub fn replace_grant(&self, id: &ParticipantId, grant: Grant) -> Result<(), Error> {
        let result = self
            .board
            .table
            .save_until(owner_save_deadline())
            .and_then(|table| self.board.save_grant(table, id, grant));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "GRANT",
            OwnerTarget::Participant(Some(id.as_str())),
            result.as_ref().err(),
        );
        result
    }

    /// Revoke a participant and end its readers and handles.
    pub fn revoke(&self, id: &ParticipantId) -> Result<(), Error> {
        let result = self
            .board
            .table
            .save_until(owner_save_deadline())
            .and_then(|table| self.board.save_revoke(table, id));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "REVOKE",
            OwnerTarget::Participant(Some(id.as_str())),
            result.as_ref().err(),
        );
        result
    }

    /// Create an ordinary channel as the owner, or return its current creation.
    ///
    /// The Boolean reports whether a new creation was saved.
    pub fn create_channel(&self, name: &str) -> Result<(CreationId, bool), Error> {
        let result = self.board.create_channel(name);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "CHANNEL_CREATE",
            OwnerTarget::Channel(Some(name), None),
            result.as_ref().err(),
        );
        result
    }

    /// Trim history up to a position in the specified current creation.
    ///
    /// Returns the new retention boundary. An older creation ID is refused.
    pub fn trim(&self, channel: &str, creation: CreationId, before: u64) -> Result<u64, Error> {
        let result = self.board.trim(channel, creation, before);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "TRIM",
            OwnerTarget::Channel(
                Some(channel),
                Position::try_from(before)
                    .ok()
                    .map(|position| (creation, position)),
            ),
            result.as_ref().err(),
        );
        result
    }

    fn issue(&self, id: ParticipantId) -> Handle {
        Handle {
            board: self.board.clone(),
            id,
            proof: None,
        }
    }
}

impl Handle {
    /// The board-assigned identity used for every call through this handle.
    pub fn id(&self) -> &ParticipantId {
        &self.id
    }

    /// Get a message by position without advancing delivery or acknowledgement state.
    pub fn get(&self, channel: &str, position: u64) -> Result<GetReply, Error> {
        self.call(
            "get",
            || channel.parse().ok(),
            |_| self.get_counted(channel, position),
        )
    }

    fn get_counted(&self, channel: &str, position: u64) -> Result<GetReply, Error> {
        let channel: ChannelName = channel.parse()?;
        let position = Position::try_from(position)?;
        let Some(channel) = self.allowed_channel(Action::Get, &channel)? else {
            return Ok(GetReply::Absent);
        };
        self.board.fetch(channel, position).map(|(reply, _)| reply)
    }

    /// Store a message as this participant.
    ///
    /// Use the same message ID and content on a retry to obtain its original receipt.
    pub fn send(&self, request: SendRequest<'_>) -> Result<Receipt, Error> {
        self.call(
            "send",
            || request.channel.parse().ok(),
            |name| self.send_counted(name, request, None),
        )
    }

    fn send_counted(
        &self,
        name: &ParticipantName,
        request: SendRequest<'_>,
        agent_session_id: Option<&str>,
    ) -> Result<Receipt, Error> {
        let board = &self.board;
        let content = request.validate(&board.limits, self.grade())?;
        crate::reserved_names::check_channel_write(&content.channel)?;
        self.authorize(Action::Send, &content.channel)?;
        self.persist_send(name, content, agent_session_id)
    }

    fn persist_send(
        &self,
        name: &ParticipantName,
        content: SendContent<'_>,
        agent_session_id: Option<&str>,
    ) -> Result<Receipt, Error> {
        let sender = RecordSender {
            id: &self.id,
            name,
            grade: self.grade(),
            peer: self.proof.as_ref().map(|proof| &proof.peer),
            agent_session_id,
        };
        self.board.persist_send(content, sender, |name| {
            self.board.find_or_create(self, name)
        })
    }
}

impl Board {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn post_upgrade_notice(
        &self,
        run: CreationId,
        previous: &str,
        installed: &str,
        entry: Option<&str>,
        peer: &Arc<malak_layers::SenderPeer>,
    ) -> Result<(), Error> {
        use crate::upgrade_notice;

        if CreationId::from_bytes(self.run) != run {
            return Err(Error::refused("board run ID does not match"));
        }
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let previous = (previous != "unknown").then_some(previous);
        if !digest(installed) || previous.is_some_and(|old| old == installed || !digest(old)) {
            return Err(Error::refused(
                "upgrade notice requires different build IDs",
            ));
        }
        let changelog = self.upgrade_changelog.get().copied().unwrap_or_default();
        let body = upgrade_notice::message(previous, installed, changelog, entry);
        let id = run.to_string();
        let sender = RecordSender {
            // Enrolments only draw 32-character IDs.
            id: &upgrade_notice::SENDER_ID
                .parse()
                .expect("valid reserved sender ID"),
            name: &"owner".parse().expect("valid sender name"),
            grade: Grade::Proven,
            peer: Some(peer),
            agent_session_id: None,
        };
        let mut request = SendRequest::note("lead", &id, body.as_bytes());
        let headers = [("trace", crate::HeaderValue::Text(&id))];
        request.headers = &headers;
        let content = request.validate(&self.limits, sender.grade)?;
        self.persist_send(content, sender, |name| {
            self.table
                .check()
                .channels
                .get(name)
                .cloned()
                .ok_or_else(|| Error::unavailable("the lead inbox is missing"))
        })?;
        Ok(())
    }

    fn persist_send(
        &self,
        mut content: SendContent<'_>,
        sender: RecordSender<'_>,
        resolve_channel: impl FnOnce(&ChannelName) -> Result<Arc<Channel>, Error>,
    ) -> Result<Receipt, Error> {
        let sender_id = sender.id;
        if content
            .minimum_durability
            .is_some_and(|minimum| minimum > self.store.info.durability)
        {
            return Err(Error::not_supported(
                "store cannot reach the requested durability",
            ));
        }
        let trace = trace_header(&content.headers);
        let empty_trace = trace == Some("");
        let no_legacy_retry = || Error::refused("empty trace requires an exact legacy retry");
        let channel = if empty_trace {
            self.table
                .check()
                .channels
                .get(&content.channel)
                .cloned()
                .ok_or_else(no_legacy_retry)?
        } else {
            resolve_channel(&content.channel)?
        };
        let deadline = Instant::now() + STORE_WAIT;
        let mut history = channel
            .history
            .try_lock_arc_until(deadline)
            .ok_or_else(|| Error::unavailable("channel write did not settle in time"))?;
        history = self.ready_history(&channel, history, deadline)?;
        channel.available()?;
        if !empty_trace && (history.pending() || history.trim_needed(&channel)) {
            (history, _) = self.finish_work(channel.clone(), history, deadline, None)?;
        }

        let body = HashedBody::new(content.body);
        let input = RetryInput {
            kind: content.kind,
            headers: &content.headers,
            body: body.digest(),
        };
        if let Some(receipt) = history.retry(sender_id, &content.id, &input)? {
            return Ok(receipt);
        }
        if empty_trace {
            return Err(no_legacy_retry());
        }
        let digest = input.current();
        let mut trace = trace.and_then(TraceId::parse);
        if trace.is_none()
            && !content
                .headers
                .iter()
                .any(|header| matches!(header, Header::ReplyTo { .. }))
        {
            let working_task = self
                .table
                .check()
                .readers
                .send_task(sender_id, &content.channel)?;
            trace = match working_task {
                Some(task) => task.trace,
                None if content.kind == Kind::Task => Some(TraceId::fresh()?),
                None => None,
            };
            if let Some(trace) = trace {
                content.add_trace(trace, &self.limits)?;
            }
        }
        let fields = Fields::encode(
            &content,
            body,
            sender,
            SystemTime::now(),
            self.store.info.durability,
        );
        let size = RecordKey::sealed_size(fields.encoded.bytes().len(), fields.body.bytes().len())?;
        if let Err(error) = history.admit(&channel, sender_id, size as u64) {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.events.mailbox_full(&channel.name, sender_id, &error);
            return Err(error);
        }
        self.check_disk(sender_id)?;
        history.prepare(
            &channel,
            Write {
                sender: sender_id,
                id: content.id,
                digest,
                trace,
                fields,
                size,
            },
            &self.key,
            &self.quota,
            &self.limits,
            &self.store,
        )?;

        let store = self.store.clone();
        let quota = self.quota.clone();
        let changes = self.changes.clone();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let events = self.events.clone();
        self.workers.until(deadline, move || {
            let receipt = history.append(&store, &channel, &quota)?;
            trim_and_notify(
                &mut history,
                &channel,
                &store,
                &quota,
                None,
                &changes,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                &events,
            )?;
            Ok(receipt)
        })
    }
}

impl Handle {
    fn call<T>(
        &self,
        _operation: &'static str,
        _channel: impl FnOnce() -> Option<ChannelName>,
        work: impl FnOnce(&ParticipantName) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let result = self.begin().and_then(|name| work(&name));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Err(error) = &result {
            self.board
                .events
                .failure(&self.id, _operation, _channel, error);
        }
        result
    }

    fn begin(&self) -> Result<ParticipantName, Error> {
        self.board.store.running()?;
        let mut table = self.board.table.check();
        if let Some(proof) = &self.proof {
            table.authority.validate_attached(&proof.identity)?;
        }
        table.authority.start_call(&self.id, &self.board.limits)
    }

    fn validate(&self, authority: &mut Authority) -> Result<(), Error> {
        self.board.store.running()?;
        match &self.proof {
            Some(proof) => authority.validate_attached(&proof.identity),
            None => authority.valid(&self.id),
        }
    }

    fn grade(&self) -> Grade {
        self.proof
            .as_ref()
            .map_or(Grade::Proven, |proof| proof.identity.grade)
    }

    fn authorize(&self, action: Action, channel: &ChannelName) -> Result<(), Error> {
        self.board
            .table
            .check()
            .authority
            .allow(&self.id, action, channel)?;
        self.veto(action, channel)
    }

    fn veto(&self, action: Action, channel: &ChannelName) -> Result<(), Error> {
        let Some(proof) = self.proof.as_ref() else {
            return Ok(());
        };
        let Some(veto) = proof.veto.clone() else {
            return Ok(());
        };
        let ends = self
            .board
            .table
            .check()
            .authority
            .verified_end(&proof.identity);
        let participant = self.id.clone();
        let channel = channel.clone();
        let grade = self.grade();
        let allowed = self
            .board
            .workers
            .until(Instant::now() + STORE_WAIT, move || {
                veto.allow(
                    &Identity {
                        participant: &participant,
                        grade,
                        ends,
                    },
                    action,
                    &channel,
                )
                .map_err(|_| Error::unavailable("mechanism veto failed"))
            })?;
        if allowed {
            Ok(())
        } else {
            Err(Error::refused("mechanism vetoed the call"))
        }
    }

    fn has_veto(&self) -> bool {
        self.proof
            .as_ref()
            .is_some_and(|proof| proof.veto.is_some())
    }

    fn allowed_channel(
        &self,
        action: Action,
        channel: &ChannelName,
    ) -> Result<Option<Arc<Channel>>, Error> {
        let table = self.board.table.check();
        table.authority.allow(&self.id, action, channel)?;
        if !self.has_veto() {
            return Ok(table.channels.get(channel).cloned());
        }
        drop(table);
        self.veto(action, channel)?;
        Ok(self.board.table.check().channels.get(channel).cloned())
    }
}

impl Board {
    fn save_grant(
        &self,
        mut table: parking_lot::MutexGuard<'_, Table>,
        id: &ParticipantId,
        grant: Grant,
    ) -> Result<(), Error> {
        let changed_at_ms = crate::epoch_millis(SystemTime::now());
        table
            .authority
            .replace_grant(id, grant, changed_at_ms, |grant, generation| {
                self.store.save(Entry::Grant {
                    participant: id,
                    grant,
                    generation,
                    changed_at_ms,
                })
            })?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.publish(&table);
        drop(table);
        self.changes.notify();
        Ok(())
    }

    fn save_revoke(
        &self,
        mut table: parking_lot::MutexGuard<'_, Table>,
        id: &ParticipantId,
    ) -> Result<(), Error> {
        let revoked_at_ms = crate::epoch_millis(SystemTime::now());
        if !table.authority.revoke(id, Some(revoked_at_ms), || {
            self.store.save(Entry::Revoke {
                participant: id,
                revoked_at_ms,
            })
        })? {
            return Ok(());
        }
        table.readers.revoke(id);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        for channel in table.channels.values() {
            channel.activity.forget_reader(id);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let links = self.links.participant(id);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.publish(&table);
        drop(table);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        for link in links {
            link.close();
        }
        self.changes.notify();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(mechanism) = self.local_mechanism.get().cloned() {
            let id = id.clone();
            let _ = self.workers.dispatch(move || {
                let _ = mechanism.revoke(&id);
            });
        }
        Ok(())
    }

    fn create_channel(&self, name: &str) -> Result<(CreationId, bool), Error> {
        let name: ChannelName = name.parse()?;
        crate::reserved_names::check_channel_write(&name)?;
        let candidate = self.channel(name)?;
        let mut table = self.table.save_until(owner_save_deadline())?;
        if let Some(current) = table.channels.get(&candidate.name) {
            return Ok((current.creation, false));
        }
        let creation = candidate.creation;
        self.save_create(&mut table, &ParticipantId::lead(), candidate)?;
        Ok((creation, true))
    }

    fn trim(&self, channel: &str, creation: CreationId, before: u64) -> Result<u64, Error> {
        let name: ChannelName = channel.parse()?;
        if !(1..=Position::MAX + 1).contains(&before) {
            return Err(Error::refused("invalid retention position"));
        }
        let authority_deadline = owner_save_deadline();
        let channel = self
            .table
            .save_until(authority_deadline)?
            .channels
            .get(&name)
            .cloned()
            .filter(|channel| channel.creation == creation)
            .ok_or_else(|| Error::refused("channel creation no longer matches"))?;
        let history = channel
            .history
            .try_lock_arc_until(authority_deadline)
            .ok_or_else(|| Error::unavailable("channel write did not settle in time"))?;
        let deadline = Instant::now() + STORE_WAIT;
        let history = self.ready_history(&channel, history, deadline)?;
        channel.available()?;
        let (_, floor) = self.finish_work(channel, history, deadline, Some(before))?;
        Ok(floor)
    }

    fn fetch(
        self: &Arc<Self>,
        channel: Arc<Channel>,
        position: Position,
    ) -> Result<(GetReply, usize), Error> {
        let deadline = Instant::now() + STORE_WAIT;
        let mut history = channel
            .history
            .try_lock_arc_until(deadline)
            .ok_or_else(|| Error::unavailable("channel write did not settle in time"))?;
        history = self.ready_history(&channel, history, deadline)?;
        channel.available()?;
        if history.pending() || history.trim_needed(&channel) {
            (history, _) = self.finish_work(channel.clone(), history, deadline, None)?;
        }
        if let Some(answer) = history.locate(channel.creation, position) {
            return Ok((answer, 0));
        }

        let board = self.clone();
        #[allow(unused_mut)]
        let (mut message, bytes) = self.workers.until(deadline, move || {
            history.load(&channel, &board.store, &board.key, position)
        })?;
        self.store.running()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if message.record_run == self.run {
            message.earlier_session = self.events.earlier_session(
                &message.sender,
                message
                    .sender_trace
                    .as_ref()
                    .and_then(|trace| trace.session.as_ref()),
            );
        }
        Ok((GetReply::Message(message), bytes))
    }

    fn ready_history(
        &self,
        _channel: &Arc<Channel>,
        history: LockedHistory,
        _deadline: Instant,
    ) -> Result<LockedHistory, Error> {
        self.store.running()?;
        _channel.present()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if history.needs_reopen() {
            let channel = _channel.clone();
            let store = self.store.clone();
            let key = self.key.clone();
            let quota = self.quota.clone();
            let mut history = history;
            return self.workers.until(_deadline, move || {
                history.check_reopen(&channel, &store, &key, &quota, None)?;
                Ok(history)
            });
        }
        Ok(history)
    }

    fn finish_work(
        &self,
        channel: Arc<Channel>,
        mut history: LockedHistory,
        deadline: Instant,
        before: Option<u64>,
    ) -> Result<(LockedHistory, u64), Error> {
        let store = self.store.clone();
        let quota = self.quota.clone();
        let changes = self.changes.clone();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let events = self.events.clone();
        self.workers.until(deadline, move || {
            if history.pending() {
                history.settle(&store, &channel, &quota)?;
            }
            let floor = trim_and_notify(
                &mut history,
                &channel,
                &store,
                &quota,
                before,
                &changes,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                &events,
            )?;
            Ok((history, floor))
        })
    }

    fn start_retention(self: &Arc<Self>) -> Result<(), Error> {
        let board = Arc::downgrade(self);
        let worker = std::thread::Builder::new()
            .name("malak-retention".into())
            .spawn(move || {
                loop {
                    std::thread::park_timeout(Duration::from_secs(60));
                    let Some(board) = board.upgrade() else { break };
                    if board.store.running().is_err() {
                        break;
                    }
                    board.sweep_retention();
                }
            })
            .map_err(|_| Error::unavailable("could not start the retention task"))?;
        *self.retention_thread.lock() = Some(worker.thread().clone());
        Ok(())
    }

    fn sweep_retention(self: &Arc<Self>) {
        self.sweep_removals();
        let channels: Vec<_> = self.table.check().channels.values().cloned().collect();
        for channel in channels {
            let Some(mut history) = channel.history.try_lock_arc() else {
                continue;
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if history.needs_reopen() {
                let store = self.store.clone();
                let key = self.key.clone();
                let quota = self.quota.clone();
                let changes = self.changes.clone();
                let task_restore = history.take_task_restore();
                let board = task_restore.as_ref().map(|_| Arc::downgrade(self));
                let _ = self.workers.dispatch(move || {
                    let through = task_restore.as_ref().map(|(_, position)| *position);
                    let result = history.check_reopen(&channel, &store, &key, &quota, through);
                    if let Some((id, _)) = task_restore
                        && let Some(board) = board.and_then(|board| board.upgrade())
                    {
                        board.table.check().readers.finish_task_restore(
                            id,
                            channel.creation,
                            result.ok().flatten(),
                        );
                    }
                    changes.notify();
                });
                continue;
            }
            if channel.available().is_err() || history.pending() || !history.trim_needed(&channel) {
                continue;
            }

            let store = self.store.clone();
            let quota = self.quota.clone();
            let changes = self.changes.clone();
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let events = self.events.clone();
            let _ = self.workers.dispatch(move || {
                let _ = trim_and_notify(
                    &mut history,
                    &channel,
                    &store,
                    &quota,
                    None,
                    &changes,
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    &events,
                );
            });
        }
    }

    fn channel(&self, name: ChannelName) -> Result<Arc<Channel>, Error> {
        Ok(Arc::new(Channel::new(
            name,
            CreationId::from_bytes(random()?),
            self.run,
            &self.limits,
        )))
    }

    fn find_or_create(&self, caller: &Handle, name: &ChannelName) -> Result<Arc<Channel>, Error> {
        let id = caller.id();
        {
            let table = self.table.check();
            if let Some(channel) = table.channels.get(name) {
                return Ok(channel.clone());
            }
            table.authority.allow(id, Action::Create, name)?;
        }

        caller.veto(Action::Create, name)?;
        let channel = self.channel(name.clone())?;
        let mut table = self.table.check();
        if let Some(current) = table.channels.get(name) {
            return Ok(current.clone());
        }
        self.save_create(&mut table, id, channel.clone())?;
        Ok(channel)
    }

    fn save_create(
        &self,
        table: &mut Table,
        id: &ParticipantId,
        channel: Arc<Channel>,
    ) -> Result<(), Error> {
        let limit = self.limits.board_channels;
        let limit = if id.is_lead() {
            limit
        } else {
            limit - self.limits.reserved_channels
        };
        let count = table.counts.get(id).copied().unwrap_or(0);
        if table.channels.len() >= limit || count >= self.limits.participant_channels {
            return Err(Error::refused("channel allowance exceeded").at_limit());
        }
        self.store.save(Entry::Create {
            name: &channel.name,
            creation: channel.creation,
            creator: id,
        })?;
        table.creators.insert(channel.creation, id.clone());
        table.channels.insert(channel.name.clone(), channel);
        table.counts.insert(id.clone(), count + 1);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.publish(table);
        Ok(())
    }

    fn check_disk(&self, id: &ParticipantId) -> Result<(), Error> {
        let minimum = self.limits.minimum_free_disk_bytes;
        if id.is_lead() || minimum == 0 {
            return Ok(());
        }
        match fs4::available_space(&self.volume) {
            Ok(available) if available >= minimum => Ok(()),
            Ok(_) => Err(Error::unavailable("insufficient free disk space").at_limit()),
            Err(_) => Err(Error::unavailable("insufficient free disk space")),
        }
    }
}

fn trim_and_notify(
    history: &mut LockedHistory,
    channel: &Channel,
    store: &StoreLease,
    quota: &Mutex<Quota>,
    before: Option<u64>,
    changes: &Changes,
    #[cfg(any(target_os = "linux", target_os = "macos"))] events: &events::Events,
) -> Result<u64, Error> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let original = channel.floor();
    let trimmed = history.trim(channel, store, quota, before);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if channel.floor() != original {
        events.record(
            EventKind::Trimmed,
            "trim",
            None,
            &channel.name,
            channel.creation,
            Position::try_from(channel.floor()).ok(),
        );
    }
    changes.notify();
    trimmed
}

impl Table {
    fn first(
        creation: CreationId,
        run: [u8; 16],
        limits: &Limits,
        enrolled_at_ms: Option<u64>,
    ) -> Self {
        let lead = ParticipantId::lead();
        let name = lead.inbox();
        let inbox = Arc::new(Channel::new(name.clone(), creation, run, limits));
        Self {
            authority: Authority::new(lead, enrolled_at_ms),
            channels: HashMap::from([(name, inbox)]),
            counts: HashMap::new(),
            creators: HashMap::new(),
            removals: HashMap::new(),
            readers: Readers::default(),
        }
    }

    fn enrol_ready(
        &self,
        name: &ParticipantName,
        id: &ParticipantId,
        inbox: &ChannelName,
        limit: usize,
    ) -> Result<bool, Error> {
        self.authority.check_name(name)?;
        if self.authority.has_id(id) || self.channels.contains_key(inbox) {
            return Ok(false);
        }
        if self.channels.len() >= limit {
            return Err(Error::refused("board channel allowance exceeded"));
        }
        Ok(true)
    }

    fn is_current(&self, channel: &Channel) -> bool {
        self.channels
            .get(&channel.name)
            .is_some_and(|current| current.creation == channel.creation)
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        if let Some(worker) = self.retention_thread.get_mut().take() {
            worker.unpark();
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::Outcome;
    use malak_store_memory::MemoryStore;

    #[test]
    fn a_legacy_channel_with_a_reserved_name_is_readable_after_restart() {
        use malak_store_markdown::MarkdownStore;
        use malak_testkit::unix_socket::SocketDir;

        let dir = SocketDir::new().unwrap();
        let state = dir.path().join("state");
        let store = || MarkdownStore::new(dir.path().join("records"), false);
        let limits = Limits::default();
        let note = SendRequest::note("help", "legacy", b"old message");
        let receipt = {
            let owner = Owner::create_durable(store(), limits.clone(), &state).unwrap();
            let lead = owner.lead();
            // Seed the same journal and message the board wrote before this rule.
            let content = note.validate(&limits, lead.grade()).unwrap();
            lead.persist_send(&"lead".parse().unwrap(), content, None)
                .unwrap()
        };

        let owner = Owner::restart_durable(store(), limits, &state).unwrap();
        let lead = owner.lead();
        let read = lead.read(&["help"], 1, Duration::ZERO).unwrap();
        assert_eq!(&read.messages[0].body[..], b"old message");
        assert_eq!(
            lead.get("help", 1).unwrap(),
            GetReply::Message(read.messages[0].clone())
        );
        lead.ack("help", receipt.creation, 1).unwrap();
        assert!(
            lead.read(&["help"], 1, Duration::ZERO)
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn sends_do_not_wait_for_the_recipients_working_task_restore() {
        let board = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let mut grant = Grant::default();
        grant.allow("lead".parse().unwrap(), [Action::Send]);
        let worker = board.enrol("worker", grant).unwrap();
        let inbox = worker.id().inbox();
        let lead = board.lead();
        let original = lead
            .send(SendRequest {
                kind: Kind::Task,
                ..SendRequest::note(inbox.as_str(), "original", b"work")
            })
            .unwrap();
        {
            // Hold the restore pending with a writable inbox to isolate trace
            // selection from the channel's independent startup checks.
            let readers = &mut board.board.table.check().readers;
            readers
                .restore(original.creation, worker.id().clone(), original.position)
                .unwrap();
            readers
                .begin_task_restore(
                    worker.id(),
                    original.creation,
                    1,
                    Instant::now() + Duration::from_secs(30),
                )
                .unwrap();
        }

        let note = lead
            .send(SendRequest::note(inbox.as_str(), "note", b""))
            .unwrap();
        let task = lead
            .send(SendRequest {
                kind: Kind::Task,
                ..SendRequest::note(inbox.as_str(), "next", b"more work")
            })
            .unwrap();

        assert_eq!(note.trace, None);
        assert!(task.trace.is_some());
        assert_ne!(task.trace, original.trace);
        let error = worker
            .send(SendRequest::note("lead", "report", b""))
            .unwrap_err();
        assert_eq!(error.outcome(), Outcome::Unavailable);
        assert!(
            error
                .to_string()
                .contains("working task trace is still being restored")
        );
    }
}
