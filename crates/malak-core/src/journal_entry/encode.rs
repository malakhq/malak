use super::start::{COMPLETE, DIRECT_LEAD, KEYS, LOCAL_LEAD, RUN, VERSION};
use super::*;
use crate::grant::write_grant;
use malak_journal::Save;
use minicbor::Encoder;
use zeroize::Zeroizing;

impl Entry<'_> {
    pub(crate) fn level(&self, durability: Durability) -> Save {
        match self {
            Self::Ack { .. }
            | Self::Head {
                new_stamp: false, ..
            } if durability == Durability::Restart => Save::Written,
            _ => Save::OnDisk,
        }
    }

    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut encoder = Encoder::new(Vec::new());
        match self {
            Self::Keys {
                record,
                board,
                owner,
                store,
                location,
            } => {
                tag(&mut encoder, KEYS, 6 + u64::from(location.is_some()))
                    .bytes(*record)
                    .unwrap()
                    .bytes(board.private())
                    .unwrap()
                    .bytes(board.public())
                    .unwrap()
                    .bytes(owner.private())
                    .unwrap()
                    .bytes(owner.public())
                    .unwrap()
                    .bytes(store)
                    .unwrap();
                if let Some(location) = location {
                    super::location::encode(&mut encoder, location);
                }
            }
            Self::Lead {
                creation,
                enrolled_at_ms,
            } => {
                tag(
                    &mut encoder,
                    DIRECT_LEAD,
                    1 + u64::from(enrolled_at_ms.is_some()),
                )
                .bytes(creation.as_bytes())
                .unwrap();
                if let Some(at) = enrolled_at_ms {
                    encoder.u64(*at).unwrap();
                }
            }
            Self::LocalLead {
                creation,
                key,
                uid,
                enrolled_at_ms,
            } => {
                tag(
                    &mut encoder,
                    LOCAL_LEAD,
                    3 + u64::from(enrolled_at_ms.is_some()),
                )
                .bytes(creation.as_bytes())
                .unwrap()
                .bytes(*key)
                .unwrap()
                .u32(*uid)
                .unwrap();
                if let Some(at) = enrolled_at_ms {
                    encoder.u64(*at).unwrap();
                }
            }
            Self::LocalEnrol {
                participant,
                name,
                grant,
                inbox,
                key,
                uid,
                place,
                mechanism,
                record,
                enrolled_at_ms,
            } => {
                tag(&mut encoder, 17, 10)
                    .str(participant.as_str())
                    .unwrap()
                    .str(name.as_str())
                    .unwrap()
                    .bytes(inbox.as_bytes())
                    .unwrap()
                    .bytes(*key)
                    .unwrap()
                    .u32(*uid)
                    .unwrap();
                match place {
                    Some(malak_layers::Source::LocalUser(uid)) => {
                        encoder.u32(*uid).unwrap();
                    }
                    Some(malak_layers::Source::VirtualMachine(vm)) => {
                        encoder.bytes(vm.as_bytes()).unwrap();
                    }
                    None => {
                        encoder.null().unwrap();
                    }
                    Some(_) => unreachable!("local enrolment accepts only Unix and VM sources"),
                }
                encoder.str(mechanism).unwrap();
                match record {
                    Some(record) => {
                        encoder.bytes(record).unwrap();
                    }
                    None => {
                        encoder.null().unwrap();
                    }
                }
                write_grant(&mut encoder, grant);
                encoder.u64(*enrolled_at_ms).unwrap();
            }
            Self::SocketUpdate {
                participant,
                generation,
                key,
                record,
            } => {
                tag(&mut encoder, 18, 4)
                    .str(participant.as_str())
                    .unwrap()
                    .u64(*generation)
                    .unwrap();
                match key {
                    Some(key) => {
                        encoder.bytes(key).unwrap();
                    }
                    None => {
                        encoder.null().unwrap();
                    }
                }
                match record {
                    Some(record) => {
                        encoder.bytes(record).unwrap();
                    }
                    None => {
                        encoder.null().unwrap();
                    }
                }
            }
            Self::Complete => {
                tag(&mut encoder, COMPLETE, 0);
            }
            Self::Run(run) => {
                tag(&mut encoder, RUN, 1).bytes(run).unwrap();
            }
            Self::WitnessHanded(at) => {
                tag(&mut encoder, 19, 2)
                    .u64(at.sequence)
                    .unwrap()
                    .bytes(&at.running_hash)
                    .unwrap();
            }
            Self::WitnessPublished { at, position } => {
                tag(&mut encoder, 20, 3)
                    .u64(at.sequence)
                    .unwrap()
                    .bytes(&at.running_hash)
                    .unwrap()
                    .u64(position.get())
                    .unwrap();
            }
            Self::Enrol {
                participant,
                name,
                grant,
                inbox,
                enrolled_at_ms,
            } => {
                tag(&mut encoder, 4, 5)
                    .str(participant.as_str())
                    .unwrap()
                    .str(name.as_str())
                    .unwrap()
                    .bytes(inbox.as_bytes())
                    .unwrap();
                write_grant(&mut encoder, grant);
                encoder.u64(*enrolled_at_ms).unwrap();
            }
            Self::Grant {
                participant,
                grant,
                generation,
                changed_at_ms,
            } => {
                tag(&mut encoder, 5, 4)
                    .str(participant.as_str())
                    .unwrap()
                    .u64(*generation)
                    .unwrap();
                write_grant(&mut encoder, grant);
                encoder.u64(*changed_at_ms).unwrap();
            }
            Self::Revoke {
                participant,
                revoked_at_ms,
            } => {
                tag(&mut encoder, 6, 2)
                    .str(participant.as_str())
                    .unwrap()
                    .u64(*revoked_at_ms)
                    .unwrap();
            }
            Self::Create {
                name,
                creation,
                creator,
            } => {
                tag(&mut encoder, 7, 3)
                    .str(name.as_str())
                    .unwrap()
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .str(creator.as_str())
                    .unwrap();
            }
            Self::Reserve {
                creation,
                sender,
                position,
                day,
                bytes,
            } => {
                tag(&mut encoder, 8, 5)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .str(sender.as_str())
                    .unwrap()
                    .u64(position.get())
                    .unwrap()
                    .u64(*day)
                    .unwrap()
                    .u64(*bytes)
                    .unwrap();
            }
            Self::Head {
                creation,
                position,
                stamp,
                bytes_kept,
                record_bytes,
                day,
                sender,
                id,
                digest,
                durability,
                trace,
                lead_session,
                ..
            } => {
                use crate::record_fields::RetryDigest;
                let (change, count, digest) = match digest {
                    RetryDigest::Legacy(digest) => (9, 11, digest),
                    RetryDigest::TraceAware(digest) => {
                        (29, 12 + u64::from(lead_session.is_some()), digest)
                    }
                };
                tag(&mut encoder, change, count)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .u64(position.get())
                    .unwrap()
                    .bytes(&stamp.run)
                    .unwrap()
                    .u64(stamp.attempt)
                    .unwrap()
                    .u64(*bytes_kept)
                    .unwrap()
                    .u64(*record_bytes)
                    .unwrap()
                    .u64(*day)
                    .unwrap()
                    .str(sender.as_str())
                    .unwrap()
                    .str(id.as_str())
                    .unwrap()
                    .bytes(digest)
                    .unwrap()
                    .u8(durability_code(*durability))
                    .unwrap();
                if change == 29 {
                    match trace {
                        Some(trace) => encoder.str(trace.as_str()).unwrap(),
                        None => encoder.null().unwrap(),
                    };
                    if let Some(session) = lead_session {
                        session.write(&mut encoder);
                    }
                }
            }
            Self::Trim {
                creation,
                floor,
                bytes_kept,
            } => {
                tag(&mut encoder, 10, 3)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .u64(*floor)
                    .unwrap()
                    .u64(*bytes_kept)
                    .unwrap();
            }
            Self::TrimDone(creation) => {
                tag(&mut encoder, 11, 1).bytes(creation.as_bytes()).unwrap();
            }
            Self::Rekey {
                participant,
                generation,
                key,
                record,
            } => {
                tag(&mut encoder, 28, 4)
                    .str(participant.as_str())
                    .unwrap()
                    .u64(*generation)
                    .unwrap()
                    .bytes(key)
                    .unwrap();
                match record {
                    Some(record) => encoder.bytes(record).unwrap(),
                    None => encoder.null().unwrap(),
                };
            }
            Self::Remove(creation) => {
                tag(&mut encoder, 26, 1).bytes(creation.as_bytes()).unwrap();
            }
            Self::RemoveDone(creation) => {
                tag(&mut encoder, 27, 1).bytes(creation.as_bytes()).unwrap();
            }
            Self::Cut {
                creation,
                head,
                attempt,
                tail,
            } => {
                tag(&mut encoder, 12, 3 + u64::from(tail.is_some()))
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .u64(*head)
                    .unwrap()
                    .u64(*attempt)
                    .unwrap();
                if let Some(tail) = tail {
                    encoder
                        .array(3)
                        .unwrap()
                        .u64(tail.last.map_or(0, Position::get))
                        .unwrap()
                        .bool(tail.unparseable)
                        .unwrap()
                        .bool(tail.tampered)
                        .unwrap();
                }
            }
            Self::CutDone { creation, released } => {
                tag(&mut encoder, 13, 2)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .bool(*released)
                    .unwrap();
            }
            Self::Damage { creation, position } => {
                tag(&mut encoder, 14, 2)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .u64(position.get())
                    .unwrap();
            }
            Self::Ack {
                creation,
                participant,
                position,
            } => {
                tag(&mut encoder, 15, 3)
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .str(participant.as_str())
                    .unwrap()
                    .u64(position.get())
                    .unwrap();
            }
        }
        Zeroizing::new(encoder.into_writer())
    }
}

fn tag(encoder: &mut Encoder<Vec<u8>>, change: u8, fields: u64) -> &mut Encoder<Vec<u8>> {
    encoder
        .array(fields + 2)
        .unwrap()
        .u8(VERSION)
        .unwrap()
        .u8(change)
        .unwrap()
}
