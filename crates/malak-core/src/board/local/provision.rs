use super::{LocalConfig, LocalEntrances, Owner};
use crate::board::{
    durable::{LocalReplay, NewBoard, Start, check_local, key_pair, reopen},
    new_participant_id, owner_save_deadline,
};
use crate::handover::{guest_address, local_address};
use crate::journal_entry::Entry;
use crate::{
    CreationId, Error, Grant, HandoverFile, HandoverTarget, HandoverWrite, Limits, OwnerCredential,
    ParticipantId, ParticipantName,
};
use malak_layers::{Enrolment, Handover, Mechanism, Source, Store};
use malak_proto::{Binding, Entrance, KeyPair};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const MECHANISM_WAIT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests;

/// Listener addresses and provisioning settings for a local board.
///
/// Socket paths must be provisioned by the local runner before it accepts links.
pub struct LocalStartup {
    /// Absolute `unix:` address for the owner door.
    pub owner_address: String,
    /// Absolute `unix:` address for the lead.
    pub lead_address: String,
    /// Absolute `unix:` address or guest-visible `vsock:2:PORT` address.
    pub general_address: String,
    /// Destination for the lead's private credentials.
    pub lead_handover: PathBuf,
    /// Mechanism name supplied to participants in their handovers.
    pub mechanism_name: String,
}

pub(super) struct Provisioning {
    general_address: String,
    lead_address: String,
    pub(super) mechanism_name: String,
    #[cfg(test)]
    after_rekey_save: parking_lot::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Owner {
    /// Start a durable board and provision its same-account Unix entrances.
    ///
    /// Saves the board and owner keys, then the lead's public key and inbox.
    /// If initialization stopped before a run was saved, reuses the board's
    /// keys and lead inbox. The incomplete lead key is replaced.
    /// An existing run keeps its authority and credentials across restart.
    /// Returns only after the credentials and the completion marker are durable.
    /// The runner is responsible for provisioning the listener directories and
    /// opening sockets according to their access policy.
    pub fn create_local(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        startup: LocalStartup,
        general: Arc<dyn Mechanism>,
    ) -> Result<(Self, LocalEntrances), Error> {
        Self::create_local_for_account(
            store,
            limits,
            state,
            startup,
            general,
            nix::unistd::geteuid().as_raw(),
        )
    }

    /// Start or reopen a local board with a named Unix account at its lead entrance.
    ///
    /// The runner provisions the directory of that entrance for the lead account
    /// and protects the owner entrance separately. The handover's ancestors
    /// must already allow the lead account to traverse them.
    pub fn create_local_for_account(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        startup: LocalStartup,
        general: Arc<dyn Mechanism>,
        lead_uid: u32,
    ) -> Result<(Self, LocalEntrances), Error> {
        let state = state.as_ref();
        if !local_address(&startup.owner_address)
            || !local_address(&startup.lead_address)
            || !(local_address(&startup.general_address) || guest_address(&startup.general_address))
            || startup.owner_address == startup.lead_address
            || startup.owner_address == startup.general_address
            || startup.lead_address == startup.general_address
            || startup.mechanism_name.is_empty()
        {
            return Err(Error::refused("invalid local startup configuration"));
        }
        if !general.describe().key_at_enrolment {
            return Err(Error::not_supported(
                "local provisioning requires keys at enrolment",
            ));
        }
        let uid = nix::unistd::geteuid().as_raw();
        let inspected = check_local(state, &limits, lead_uid)?;
        if inspected.run.is_some() {
            return reopen_local(store, limits, state, startup, general, inspected, uid);
        }
        let complete = inspected.complete;
        let mut retired = inspected.retired_lead_keys();
        let saved_lead = inspected.current_lead_key().copied();
        let (lead_target, owner_target) =
            credential_targets(&inspected, state, &startup, uid, lead_uid)?;
        if lead_target.same_destination(&owner_target)? {
            return Err(Error::refused("lead and owner credential paths coincide"));
        }
        if complete && (!lead_target.exists() || !owner_target.exists()) {
            return Err(Error::refused("a credential saved at startup is missing"));
        }
        let mut provisional = NewBoard::initialize(store, limits, state, inspected)?;

        let lead_key = if complete {
            saved_lead.expect("completed startup has a saved lead")
        } else {
            if let Some(key) = saved_lead {
                retired.insert(key);
            }
            let lead = key_pair()?;
            let key = *lead.public();
            if retired.contains(&key)
                || lead.public() == provisional.board_key.public()
                || lead.public() == provisional.owner_key.public()
            {
                return Err(Error::unavailable(
                    "generated lead key is already on record",
                ));
            }
            let lead_handover = HandoverFile::new(
                binding(Entrance::Lead),
                startup.lead_address.clone(),
                *provisional.board_key.public(),
                ParticipantId::lead(),
                String::new(),
                Zeroizing::new(Vec::new()),
                Zeroizing::new(*lead.private()),
            )?;
            let enrolled_at_ms = provisional.mark_lead_enrolment();
            provisional.save(Entry::LocalLead {
                creation: provisional.creation,
                key: lead.public(),
                uid: lead_uid,
                enrolled_at_ms,
            })?;
            lead_target.write(&lead_handover)?;
            if owner_target.exists() {
                owner_target.confirm()?;
            } else {
                OwnerCredential::save(
                    owner_target,
                    &startup.owner_address,
                    &provisional.board_key,
                    &provisional.owner_key,
                )?;
            }
            key
        };

        let owner_key = *provisional.owner_key.public();
        let (owner, board_key) = provisional.finish()?;
        let entrances = owner.configure_local(
            LocalConfig {
                board_key,
                owner_key,
                lead_key,
                board_user: uid,
                lead_user: lead_uid,
            },
            general,
            Some(Provisioning {
                general_address: startup.general_address,
                lead_address: startup.lead_address,
                mechanism_name: startup.mechanism_name,
                #[cfg(test)]
                after_rekey_save: Default::default(),
            }),
        )?;
        let mut table = owner.board.table.save();
        for key in retired {
            table.authority.retire_key(key)?;
        }
        drop(table);
        Ok((owner, entrances))
    }
}

fn reopen_local(
    store: impl Store + 'static,
    limits: Limits,
    state: &Path,
    startup: LocalStartup,
    general: Arc<dyn Mechanism>,
    inspected: Start,
    uid: u32,
) -> Result<(Owner, LocalEntrances), Error> {
    let keys = inspected
        .keys
        .as_ref()
        .expect("a started board has saved keys");
    let lead_key = *inspected
        .current_lead_key()
        .ok_or_else(|| Error::refused("state has no local lead"))?;
    let lead_uid = inspected
        .lead_uid()
        .expect("a local lead has a saved account");
    let owner_key = *keys.owner.public();
    let target = OwnerCredential::prepare_resume(
        state,
        uid,
        &startup.owner_address,
        &keys.board,
        &keys.owner,
    )?;
    if !target.exists() {
        return Err(Error::refused("the saved owner credential is missing"));
    }
    let (owner, board_key) = reopen(
        store,
        limits,
        state,
        inspected,
        Some(LocalReplay {
            lead_uid,
            mechanism: &startup.mechanism_name,
        }),
    )?;
    let entrances = owner.local_settings(
        LocalConfig {
            board_key,
            owner_key,
            lead_key,
            board_user: uid,
            lead_user: lead_uid,
        },
        general,
        Some(Provisioning {
            general_address: startup.general_address,
            lead_address: startup.lead_address,
            mechanism_name: startup.mechanism_name,
            #[cfg(test)]
            after_rekey_save: Default::default(),
        }),
    );
    Ok((owner, entrances))
}

fn credential_targets(
    start: &Start,
    state: &Path,
    startup: &LocalStartup,
    uid: u32,
    lead_uid: u32,
) -> Result<(HandoverTarget, HandoverTarget), Error> {
    let Some(keys) = &start.keys else {
        return Ok((
            HandoverTarget::prepare(&startup.lead_handover, lead_uid, HandoverWrite::New)?,
            OwnerCredential::prepare(state, uid)?,
        ));
    };
    let lead_target = HandoverTarget::prepare_resume(&startup.lead_handover, lead_uid, &|bytes| {
        let expected = binding(Entrance::Lead);
        let Ok(file) = HandoverFile::decode(bytes) else {
            return false;
        };
        if file.binding().entrance != expected.entrance
            || file.binding().version != expected.version
            || !file.id().is_lead()
            || file.address() != startup.lead_address
            || file.board_key() != keys.board.public()
        {
            return false;
        }
        KeyPair::public_for(file.private_key()).is_ok_and(|public| {
            if start.complete {
                Some(&public) == start.current_lead_key()
            } else {
                start.local_key_was_saved(&public)
            }
        })
    })?;
    let owner_target = OwnerCredential::prepare_resume(
        state,
        uid,
        &startup.owner_address,
        &keys.board,
        &keys.owner,
    )?;
    Ok((lead_target, owner_target))
}

impl LocalEntrances {
    /// Enrol a participant under the configured local mechanism.
    ///
    /// The participant and its inbox are saved before the handover is written.
    /// If the write fails, the error reports the ID of the enrolled participant.
    pub fn enrol(
        &self,
        name: &str,
        grant: Grant,
        path: impl AsRef<Path>,
        only_user: Option<u32>,
    ) -> Result<ParticipantId, Error> {
        self.enrol_for_account(name, grant, path, self.settings.board_user, only_user)
    }

    /// Enrol a participant and share its handover with the named Unix account.
    ///
    /// The account must be able to traverse the parent directories of the
    /// handover and the general entrance. The source lock is independent.
    pub fn enrol_for_account(
        &self,
        name: &str,
        grant: Grant,
        path: impl AsRef<Path>,
        uid: u32,
        only_user: Option<u32>,
    ) -> Result<ParticipantId, Error> {
        let result = self.enrol_local(name, grant, path.as_ref(), uid, only_user);
        self.board.events.owner(
            "ENROL",
            super::super::OwnerTarget::Name(Some(name)),
            result.as_ref().err(),
        );
        result
    }

    pub(super) fn enrol_local(
        &self,
        name: &str,
        grant: Grant,
        path: &Path,
        uid: u32,
        only_user: Option<u32>,
    ) -> Result<ParticipantId, Error> {
        self.enrol_at(name, grant, path, uid, only_user.map(Source::LocalUser))
    }

    /// Enrol a participant whose key is valid only in the named VM launch.
    ///
    /// Configure the general handover address as `vsock:2:PORT`. The runner
    /// provides host-verified VM evidence and delivers the private handover
    /// file to the intended guest.
    pub fn enrol_for_vm(
        &self,
        name: &str,
        grant: Grant,
        path: impl AsRef<Path>,
        vm: CreationId,
    ) -> Result<ParticipantId, Error> {
        let result = self.enrol_at(
            name,
            grant,
            path.as_ref(),
            self.settings.board_user,
            Some(Source::VirtualMachine(vm)),
        );
        self.board.events.owner(
            "ENROL",
            super::super::OwnerTarget::Name(Some(name)),
            result.as_ref().err(),
        );
        result
    }

    pub(super) fn enrol_at(
        &self,
        name: &str,
        grant: Grant,
        path: &Path,
        uid: u32,
        place: Option<Source>,
    ) -> Result<ParticipantId, Error> {
        let provision = self.provisioning()?;
        let guest = guest_address(&provision.general_address);
        match (&place, guest) {
            (None, _)
            | (Some(Source::LocalUser(_)), false)
            | (Some(Source::VirtualMachine(_)), true) => {}
            _ => {
                return Err(Error::refused(
                    "source does not match the general transport",
                ));
            }
        }
        if !guest && uid != self.settings.board_user {
            let socket = Path::new(
                provision
                    .general_address
                    .strip_prefix("unix:")
                    .expect("validated Unix address"),
            );
            malak_fs::trusted_parent_for_account(socket, self.settings.board_user, uid)
                .map_err(|error| Error::refused(format!("general entrance: {error}")))?;
        }
        let name: ParticipantName = name.parse()?;
        let deadline = owner_save_deadline();
        self.board
            .table
            .save_until(deadline)?
            .authority
            .check_enrol_name(&name)?;
        let target = HandoverTarget::prepare(path, uid, HandoverWrite::New)?;
        #[cfg(test)]
        crate::enrol_kill::checkpoint(path, "preflight");

        loop {
            let id = new_participant_id()?;
            let inbox = id.inbox();
            let channel = self.board.channel(inbox.clone())?;
            let PreparedHandover {
                key,
                credentials,
                record,
            } = self.make_handover(&id, uid, place.clone())?;
            #[cfg(test)]
            crate::enrol_kill::checkpoint(path, "keys");
            let mut table = self.board.table.save_until(deadline)?;
            if !table.enrol_ready(&name, &id, &inbox, self.board.limits.board_channels)? {
                continue;
            }
            table.authority.require_new_key(&key)?;
            let enrolled_at_ms = crate::epoch_millis(std::time::SystemTime::now());
            self.board.store.save(Entry::LocalEnrol {
                participant: &id,
                name: &name,
                grant: &grant,
                inbox: channel.creation,
                key: &key,
                uid,
                place: place.as_ref(),
                mechanism: &provision.mechanism_name,
                record: record.as_deref(),
                enrolled_at_ms,
            })?;
            table.channels.insert(inbox, channel);
            table
                .authority
                .insert(id.clone(), name, grant, Some(enrolled_at_ms));
            table
                .authority
                .register_socket(&id, Some(key), place, Some(uid), record);
            self.board.publish(&table);
            drop(table);
            self.board.changes.notify();
            #[cfg(test)]
            crate::enrol_kill::checkpoint(path, "authority");
            target.write(&credentials).map_err(|error| {
                Error::unavailable(format!(
                    "participant {id} was enrolled but its handover failed: {error}"
                ))
            })?;
            return Ok(id);
        }
    }

    /// Replace a local participant's key if its current fingerprint matches.
    ///
    /// A successful replacement invalidates the old key before writing the
    /// handover. If the write fails, retry with the newly saved fingerprint.
    pub fn rekey(
        &self,
        id: &ParticipantId,
        expected: &str,
        path: impl AsRef<Path>,
    ) -> Result<PathBuf, Error> {
        let result = self.rekey_local(id, expected, path.as_ref());
        self.board.events.owner(
            "REKEY",
            super::super::OwnerTarget::Participant(Some(id.as_str())),
            result.as_ref().err(),
        );
        result
    }

    pub(super) fn rekey_local(
        &self,
        id: &ParticipantId,
        expected: &str,
        path: &Path,
    ) -> Result<PathBuf, Error> {
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::refused("invalid key fingerprint"));
        }

        let provision = self.provisioning()?;
        let deadline = owner_save_deadline();
        let gate = self
            .board
            .table
            .save_until(deadline)?
            .authority
            .rekey_gate(id)?;
        let _replacement = gate.try_lock_until(deadline).ok_or_else(|| {
            Error::unavailable("participant key replacement is still in progress")
        })?;
        let (account, place) = self
            .board
            .table
            .save_until(deadline)?
            .authority
            .rekey_target(id, expected)?;
        let account =
            account.ok_or_else(|| Error::refused("participant has no saved handover account"))?;
        let (entrance, address, _) = provision.handover(id);
        let target = HandoverTarget::prepare_resume(path, account, &|bytes| {
            HandoverFile::decode(bytes).is_ok_and(|file| {
                file.id() == id
                    && file.binding().version == binding(entrance).version
                    && file.binding().entrance == entrance
                    && file.address() == address
                    && file.board_key() == self.settings.board_key.public()
            })
        })?;
        #[cfg(test)]
        crate::durable_kill::handover(path, "prepared");

        let PreparedHandover {
            key,
            credentials,
            record,
        } = self.make_handover(id, account, place)?;
        #[cfg(test)]
        crate::durable_kill::handover(path, "keys");
        let links = {
            let mut table = self.board.table.save_until(owner_save_deadline())?;
            table
                .authority
                .replace_key(id, expected, key, record, |entry| {
                    self.board.store.save(entry)
                })?;
            self.board.publish(&table);
            self.board.links.participant(id)
        };
        #[cfg(test)]
        {
            let pause = provision.after_rekey_save.lock().take();
            if let Some(pause) = pause {
                pause();
            }
        }
        for link in links {
            link.close();
        }
        self.board.changes.notify();
        target.write(&credentials).map_err(|error| {
            Error::unavailable(format!(
                "participant {id} has a new key but its handover failed: {error}"
            ))
        })
    }

    fn provisioning(&self) -> Result<&Provisioning, Error> {
        self.settings
            .provisioning
            .as_ref()
            .ok_or_else(|| Error::not_supported("the board has no durable local provisioning"))
    }

    fn make_handover(
        &self,
        id: &ParticipantId,
        uid: u32,
        place: Option<Source>,
    ) -> Result<PreparedHandover, Error> {
        let (entrance, address, mechanism_name) = self.provisioning()?.handover(id);
        let mut keys = if id.is_lead() {
            let key = key_pair()?;
            Handover {
                private_key: Some(*key.private()),
                public_key: Some(*key.public()),
                contents: Vec::new(),
                record: None,
            }
        } else {
            let mechanism = self.settings.mechanism.clone();
            let claimant = id.clone();
            self.settings
                .owner
                .workers
                .until(Instant::now() + MECHANISM_WAIT, move || {
                    mechanism
                        .enrol(
                            &claimant,
                            &Enrolment {
                                account: Some(uid),
                                allowed_place: place.as_ref(),
                            },
                        )
                        .map_err(|_| {
                            Error::unavailable("mechanism could not prepare participant keys")
                        })
                })?
        };
        let key = keys
            .public_key
            .take()
            .ok_or_else(|| Error::unavailable("mechanism supplied no public key"))?;
        let private = Zeroizing::new(
            keys.private_key
                .take()
                .ok_or_else(|| Error::unavailable("mechanism supplied no private key"))?,
        );
        if KeyPair::public_for(&private).ok() != Some(key) {
            return Err(Error::unavailable(
                "mechanism supplied a mismatched key pair",
            ));
        }
        if keys
            .record
            .as_ref()
            .is_some_and(|record| record.len() > crate::authority::MAX_RECORD)
        {
            return Err(Error::unavailable(
                "mechanism enrolment record is too large",
            ));
        }
        let handover = HandoverFile::new(
            binding(entrance),
            address.to_owned(),
            *self.settings.board_key.public(),
            id.clone(),
            mechanism_name.to_owned(),
            Zeroizing::new(std::mem::take(&mut keys.contents)),
            private,
        )?;
        Ok(PreparedHandover {
            key,
            credentials: handover,
            record: keys.record.take(),
        })
    }
}

struct PreparedHandover {
    key: [u8; 32],
    credentials: HandoverFile,
    record: Option<Vec<u8>>,
}

impl Provisioning {
    fn handover(&self, id: &ParticipantId) -> (Entrance, &str, &str) {
        if id.is_lead() {
            (Entrance::Lead, &self.lead_address, "")
        } else {
            (
                Entrance::General,
                &self.general_address,
                &self.mechanism_name,
            )
        }
    }
}

const fn binding(entrance: Entrance) -> Binding {
    Binding {
        version: 1,
        entrance,
    }
}
