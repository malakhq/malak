use super::{Board, Table};
use crate::{ChannelName, Error, Header, ParticipantId, SendContent};

impl Board {
    pub(super) fn resolve_reply_channels(
        &self,
        content: &mut SendContent<'_>,
        sender: &ParticipantId,
    ) -> Result<(), Error> {
        if !content
            .headers
            .iter()
            .any(|header| matches!(header, Header::ReplyTo { .. }))
        {
            return Ok(());
        }
        let table = self.table.check();
        content.resolve_reply_channels(|channel| {
            table.authority.resolve_channel(channel, Some(sender))
        })
    }
}

impl Table {
    // Old stray channels remain readable under their existing grants. An active
    // name or exact participant ID always wins over a literal stray channel.
    pub(super) fn resolve_read_channel(
        &self,
        name: &ChannelName,
        viewer: Option<&ParticipantId>,
    ) -> Result<ChannelName, Error> {
        match self.authority.resolve_channel(name, viewer) {
            Err(_) if self.channels.contains_key(name) => Ok(name.clone()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use malak_store_memory::MemoryStore;

    fn store_old_reply(owner: &Owner, request: SendRequest<'_>) -> Receipt {
        let board = &owner.board;
        let content = request.validate(&board.limits, Grade::Proven).unwrap();
        let channel = board.table.check().channels[content.channel.as_str()].clone();
        let mut history = channel.history.lock_arc();
        let body = HashedBody::new(content.body);
        let digest = RetryInput {
            kind: content.kind,
            headers: &content.headers,
            body: body.digest(),
        }
        .current();
        let id = ParticipantId::lead();
        let name = "lead".parse().unwrap();
        let fields = Fields::encode(
            &content,
            body,
            RecordSender {
                id: &id,
                name: &name,
                grade: Grade::Proven,
                peer: None,
                agent_session_id: None,
            },
            SystemTime::now(),
            board.store.info.durability,
        );
        let size = RecordKey::sealed_size(fields.encoded.bytes().len(), fields.body.bytes().len())
            .unwrap();
        history
            .prepare(
                &channel,
                Write {
                    sender: &id,
                    id: content.id,
                    digest,
                    trace: None,
                    fields,
                    size,
                },
                &board.key,
                &board.quota,
                &board.limits,
                &board.store,
            )
            .unwrap();
        history
            .append(&board.store, &channel, &board.quota)
            .unwrap()
    }

    fn old_reply_headers(owner: &Owner) -> [(&'static str, crate::HeaderValue<'static>); 1] {
        let worker = owner.enrol("reviewer", Grant::default()).unwrap();
        let (creation, _) = owner.create_channel(worker.id().inbox().as_str()).unwrap();
        [(
            "reply_to",
            crate::HeaderValue::ReplyTo {
                channel: "to/reviewer",
                creation,
                position: 1,
            },
        )]
    }

    #[test]
    fn an_exact_legacy_name_reply_retry_keeps_the_original_receipt_and_record() {
        let owner = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let headers = old_reply_headers(&owner);
        let request = SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "old-reply", b"old reply")
        };
        let original = store_old_reply(&owner, request);

        let retry = owner.lead().send(request).unwrap();
        assert!(retry.duplicate);
        assert_eq!(retry.creation, original.creation);
        assert_eq!(retry.position, original.position);
        let GetReply::Message(message) = owner.lead().get("lead", 1).unwrap() else {
            panic!("missing original reply");
        };
        assert!(
            matches!(&message.headers[0], Header::ReplyTo { channel, .. } if channel.as_str() == "to/reviewer")
        );
        assert!(matches!(
            owner.lead().get("lead", 2).unwrap(),
            GetReply::NeverWritten { .. }
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_exact_legacy_name_reply_retry_survives_a_durable_restart() {
        use malak_store_markdown::MarkdownStore;
        let root = malak_testkit::unix_socket::SocketDir::new().unwrap();
        let state = root.path().join("state");
        let records = root.path().join("records");
        let owner = Owner::create_durable(
            MarkdownStore::new(&records, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        let headers = old_reply_headers(&owner);
        let request = SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "old-reply", b"old reply")
        };
        let original = store_old_reply(&owner, request);
        drop(owner);

        let owner = Owner::restart_durable(
            MarkdownStore::new(&records, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        let retry = owner.lead().send(request).unwrap();
        assert!(retry.duplicate);
        assert_eq!(retry.creation, original.creation);
        assert_eq!(retry.position, original.position);
    }

    #[test]
    fn an_exact_legacy_name_reply_retry_still_requires_the_destination_grant() {
        let owner = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let headers = old_reply_headers(&owner);
        let request = SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "old-reply", b"old reply")
        };
        store_old_reply(&owner, request);
        owner
            .replace_grant(&ParticipantId::lead(), Grant::default())
            .unwrap();

        assert_eq!(
            owner.lead().send(request).unwrap_err().outcome(),
            crate::Outcome::Refused
        );
        assert_eq!(owner.board.table.check().channels["lead"].status().1, 2);
    }

    fn legacy_stray() -> Owner {
        let owner = Owner::in_memory(MemoryStore::default(), Limits::default()).unwrap();
        let channel = owner.board.channel("to/reviewer".parse().unwrap()).unwrap();
        // Seed a pre-decision-152 channel without weakening the public write path.
        owner
            .board
            .table
            .check()
            .channels
            .insert(channel.name.clone(), channel.clone());
        let id = ParticipantId::lead();
        let name = "lead".parse().unwrap();
        let content = SendRequest::note("to/reviewer", "old", b"old note")
            .validate(&owner.board.limits, Grade::Proven)
            .unwrap();
        owner
            .board
            .persist_send(
                content,
                RecordSender {
                    id: &id,
                    name: &name,
                    grade: Grade::Proven,
                    peer: None,
                    agent_session_id: None,
                },
                |_| Ok(channel),
            )
            .unwrap();
        owner
    }

    #[test]
    fn a_legacy_stray_can_be_read_gotten_and_acknowledged() {
        let owner = legacy_stray();
        let lead = owner.lead();
        assert!(matches!(
            lead.get("to/reviewer", 1).unwrap(),
            GetReply::Message(_)
        ));
        let reply = lead.read(&["to/reviewer"], 1, Duration::ZERO).unwrap();
        let receipt = reply.messages[0].receipt;
        lead.ack("to/reviewer", receipt.creation, 1).unwrap();
        assert!(
            lead.read(&["to/reviewer"], 1, Duration::ZERO)
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn a_legacy_stray_cannot_receive_new_messages() {
        let owner = legacy_stray();
        assert!(
            owner
                .lead()
                .send(SendRequest::note("to/reviewer", "new", b"new note"))
                .is_err()
        );
        assert_eq!(
            owner.board.table.check().channels["to/reviewer"].status().1,
            2
        );
    }

    #[test]
    fn a_legacy_stray_still_requires_the_existing_grant() {
        let owner = legacy_stray();
        let worker = owner.enrol("other", Grant::default()).unwrap();
        assert!(worker.get("to/reviewer", 1).is_err());
    }

    #[test]
    fn an_active_name_shadows_a_stray_without_changing_its_records() {
        let owner = legacy_stray();
        let worker = owner.enrol("reviewer", Grant::default()).unwrap();
        owner
            .lead()
            .send(SendRequest::note("to/reviewer", "new", b"new note"))
            .unwrap();
        let GetReply::Message(message) = owner.lead().get("to/reviewer", 1).unwrap() else {
            panic!("missing new message");
        };
        assert_eq!(message.channel, worker.id().inbox());
        assert_eq!(&message.body[..], b"new note");
        let old = owner.board.table.check().channels["to/reviewer"].clone();
        let (GetReply::Message(message), _) = owner
            .board
            .fetch(old, Position::try_from(1).unwrap())
            .unwrap()
        else {
            panic!("missing stray history");
        };
        assert_eq!(&message.body[..], b"old note");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_grandfathered_enrolment_resolves_after_a_durable_restart() {
        use malak_store_markdown::MarkdownStore;
        let root = malak_testkit::unix_socket::SocketDir::new().unwrap();
        let state = root.path().join("state");
        let records = root.path().join("records");
        let owner = Owner::create_durable(
            MarkdownStore::new(&records, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        let id = new_participant_id().unwrap();
        let name: ParticipantName = "old_reviewer_with_a_long_legacy_name".parse().unwrap();
        let inbox = id.inbox();
        let channel = owner.board.channel(inbox.clone()).unwrap();
        let grant = Grant::default();
        // Replay an old, valid enrolment entry instead of calling today's enrol.
        owner
            .board
            .store
            .save(Entry::Enrol {
                participant: &id,
                name: &name,
                grant: &grant,
                inbox: channel.creation,
                enrolled_at_ms: 0,
            })
            .unwrap();
        {
            let mut table = owner.board.table.check();
            table.channels.insert(inbox.clone(), channel);
            table.authority.insert(id, name.clone(), grant, None);
        }
        drop(owner);

        let owner = Owner::restart_durable(
            MarkdownStore::new(&records, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        let address = format!("to/{name}");
        owner
            .lead()
            .send(SendRequest::note(&address, "one", b"legacy recipient"))
            .unwrap();
        let GetReply::Message(message) = owner.lead().get(&address, 1).unwrap() else {
            panic!("legacy inbox missing");
        };
        assert_eq!(message.channel, inbox);
    }
}
