use super::*;

pub(super) struct RecoveryPlan {
    pub(super) store_id: [u8; 16],
    pub(super) number: PlanNumber,
    pub(super) place: Option<AcceptedPlace>,
    pub(super) channels: Vec<ChannelReport>,
}

pub(super) struct ChannelReport {
    pub(super) name: ChannelName,
    pub(super) creation: CreationId,
    pub(super) head: u64,
    pub(super) new_head: u64,
    pub(super) keep: Vec<KeptRecord>,
    pub(super) lost: Vec<Loss>,
    pub(super) conflicts: Vec<u64>,
    pub(super) behind: Vec<ParticipantId>,
    pub(super) known_lost: u64,
    pub(super) set_aside: Vec<u64>,
    pub(super) bytes: u64,
    pub(super) input: [u8; 32],
}

pub(super) struct Loss {
    pub(super) range: Range<u64>,
    pub(super) reason: LossReason,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum LossReason {
    Missing,
    Invalid,
    WrongStamp,
}

impl fmt::Display for LossReason {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Missing => "missing",
            Self::Invalid => "record check failed",
            Self::WrongStamp => "wrong stamp",
        })
    }
}

impl RecoveryPlan {
    pub(super) fn new(
        journal: Checkpoint,
        store_id: [u8; 16],
        place: Option<AcceptedPlace>,
        factor: u64,
        channels: Vec<ChannelReport>,
    ) -> Self {
        let mut hash = PlanHash(Sha256::new());
        hash.field(b"malak/recovery-plan/v1");
        hash.number(journal.sequence);
        hash.field(&journal.running_hash);
        hash.field(&store_id);
        match &place {
            Some(AcceptedPlace::Host(place)) => {
                hash.field(&[1]);
                hash.field(place);
            }
            Some(AcceptedPlace::Local(place)) => {
                hash.field(&[2]);
                hash.field(&place.filesystem_id);
                hash.number(place.folder_number);
            }
            None => hash.field(&[0]),
        }
        hash.number(factor);
        hash.number(channels.len() as u64);
        for channel in &channels {
            hash.field(channel.name.as_str().as_bytes());
            hash.field(channel.creation.as_bytes());
            hash.field(&channel.input);
            hash.number(channel.bytes);
            hash.number(channel.head);
            hash.number(channel.new_head);
            hash.number(channel.keep.len() as u64);
            for record in &channel.keep {
                hash.number(record.position.get());
                hash.field(&record.digest);
            }
            hash.number(channel.lost.len() as u64);
            for loss in &channel.lost {
                hash.number(loss.range.start);
                hash.number(loss.range.end);
                hash.field(&[match loss.reason {
                    LossReason::Missing => 0,
                    LossReason::Invalid => 1,
                    LossReason::WrongStamp => 2,
                }]);
            }
            for list in [&channel.conflicts, &channel.set_aside] {
                hash.number(list.len() as u64);
                for position in list {
                    hash.number(*position);
                }
            }
            hash.number(channel.known_lost);
            hash.number(channel.behind.len() as u64);
            for reader in &channel.behind {
                hash.field(reader.as_str().as_bytes());
            }
        }
        Self {
            store_id,
            number: PlanNumber {
                factor,
                digest: hash.0.finalize().into(),
            },
            place,
            channels,
        }
    }
}

struct PlanHash(Sha256);

impl PlanHash {
    fn number(&mut self, value: u64) {
        self.field(&value.to_be_bytes());
    }

    fn field(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }
}

/// A read-only report of the stopped channels and their recovery plan.
pub struct RecoveryReport(Report);

enum Report {
    Proposal(RecoveryPlan),
    Progress(AcceptedPlan),
}

impl RecoveryReport {
    pub(super) fn proposal(plan: RecoveryPlan) -> Self {
        Self(Report::Proposal(plan))
    }

    pub(super) fn progress(plan: AcceptedPlan) -> Self {
        Self(Report::Progress(plan))
    }

    /// Return the plan number, including the work factor used to produce it.
    pub fn plan_number(&self) -> String {
        match &self.0 {
            Report::Proposal(plan) => plan.number.to_string(),
            Report::Progress(plan) => plan.number.to_string(),
        }
    }
}

impl fmt::Display for RecoveryReport {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plan = match &self.0 {
            Report::Proposal(plan) => plan,
            Report::Progress(plan) => {
                for channel in &plan.channels {
                    let phase = match channel.phase {
                        Phase::Pending => "pending",
                        Phase::Begun => "rewrite begun",
                        Phase::Finished => "finished",
                        Phase::Withdrawn => "withdrawn",
                    };
                    writeln!(out, "{} {} {phase}", channel.name, channel.creation)?;
                    if channel.phase != Phase::Pending {
                        writeln!(
                            out,
                            "  Set-aside reference {} {}",
                            plan.number, channel.creation
                        )?;
                    }
                }
                return write!(out, "Plan {}", plan.number);
            }
        };
        if plan.channels.is_empty() {
            writeln!(out, "No stopped channels.")?;
        }
        for channel in &plan.channels {
            writeln!(out, "{} {}", channel.name, channel.creation)?;
            let mut kept: Option<Range<u64>> = None;
            for at in channel
                .keep
                .iter()
                .map(|record| record.position.get())
                .take_while(|&at| at < channel.head)
            {
                match &mut kept {
                    Some(range) if range.end == at => range.end += 1,
                    _ => {
                        if let Some(range) = kept.replace(at..at + 1) {
                            writeln!(out, "  Keep [{}..{})", range.start, range.end)?;
                        }
                    }
                }
            }
            if let Some(range) = kept {
                writeln!(out, "  Keep [{}..{})", range.start, range.end)?;
            }
            for loss in &channel.lost {
                writeln!(
                    out,
                    "  Give up [{}..{}) ({})",
                    loss.range.start, loss.range.end, loss.reason
                )?;
            }
            for at in &channel.conflicts {
                writeln!(out, "  Cannot choose between current records at {at}")?;
            }
            writeln!(
                out,
                "  {} lost messages have retry entries",
                channel.known_lost
            )?;
            if !channel.behind.is_empty() {
                write!(out, "  Readers behind a lost position")?;
                for reader in &channel.behind {
                    write!(out, " {reader}")?;
                }
                out.write_char('\n')?;
            }
            if channel.head < channel.new_head {
                writeln!(out, "  Adopt [{}..{})", channel.head, channel.new_head)?;
            }
            writeln!(out, "  Head {}", channel.new_head)?;
            writeln!(out, "  Set aside {} bytes before a rewrite", channel.bytes)?;
            if !channel.set_aside.is_empty() {
                write!(out, "  Verifying records set aside")?;
                for position in &channel.set_aside {
                    write!(out, " {position}")?;
                }
                out.write_char('\n')?;
            }
        }
        match &plan.place {
            Some(AcceptedPlace::Local(_)) => {
                writeln!(out, "Apply requires --owner-vouches-same-volume.")?;
            }
            Some(AcceptedPlace::Host(_)) => {}
            None => writeln!(out, "Store place is not established.")?,
        }
        write!(out, "Plan {}", plan.number)
    }
}
