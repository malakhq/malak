use super::*;

impl Replay {
    pub(in crate::board::durable) fn apply_repair(
        &mut self,
        change: RepairChange,
    ) -> Result<(), Error> {
        match change {
            RepairChange::Accept(plan) => {
                if self.repair.is_some() {
                    return Err(invalid());
                }
                let mut creations = BTreeSet::new();
                for item in &plan.channels {
                    let channel = self.channel(item.creation)?;
                    if item.name != channel.name
                        || channel.available().is_ok()
                        || !creations.insert(item.creation)
                    {
                        return Err(invalid());
                    }
                    channel.history.lock().validate_plan(item)?;
                }
                self.repair = Some(plan);
            }
            RepairChange::Begin(number, creation, attempt) => {
                let item = self.planned(number, creation)?;
                if item.phase != Phase::Pending {
                    return Err(invalid());
                }
                self.channel(creation)?
                    .history
                    .lock()
                    .restore_attempt(item.head, attempt)?;
                self.cuts.remove(&creation);
                self.planned_mut(creation)?.phase = Phase::Begun;
            }
            RepairChange::Finish(number, creation, mut adopted) => {
                let item = self.planned(number, creation)?.clone();
                let channel = self.channel(creation)?.clone();
                if item.phase != Phase::Begun
                    || adopted
                        .iter()
                        .zip(item.head..item.new_head)
                        .any(|(head, expected)| {
                            head.receipt.creation != creation
                                || head.receipt.position.get() != expected
                                || !(self.table.authority.has_id(&head.sender)
                                    || crate::upgrade_notice::sent_by_owner(
                                        &head.sender,
                                        &channel.name,
                                    ))
                                || !self.runs.contains_key(&head.stamp.run)
                        })
                {
                    return Err(invalid());
                }
                for head in &mut adopted {
                    self.restore_lead_session(head);
                }
                self.reservations.remove(&creation);
                channel
                    .history
                    .lock()
                    .finish_repair(&channel, &item, adopted, &self.quota)?;
                self.planned_mut(creation)?.phase = Phase::Finished;
            }
            RepairChange::WithdrawChannel(number, creation) => {
                if !self.planned(number, creation)?.phase.unfinished() {
                    return Err(invalid());
                }
                self.planned_mut(creation)?.phase = Phase::Withdrawn;
            }
            RepairChange::Withdraw(number) => {
                self.require_plan(number)?;
                self.repair = None;
            }
        }
        if self
            .repair
            .as_ref()
            .is_some_and(|plan| plan.channels.iter().all(|item| !item.phase.unfinished()))
        {
            self.repair = None;
        }
        Ok(())
    }

    pub(super) fn require_plan(&self, number: PlanNumber) -> Result<&AcceptedPlan, Error> {
        self.repair
            .as_ref()
            .filter(|plan| plan.number == number)
            .ok_or_else(|| Error::refused("there is no unfinished repair with this plan number"))
    }

    fn planned(&self, number: PlanNumber, creation: CreationId) -> Result<&PlannedChannel, Error> {
        self.require_plan(number)?
            .channels
            .iter()
            .find(|channel| channel.creation == creation)
            .ok_or_else(invalid)
    }

    fn planned_mut(&mut self, creation: CreationId) -> Result<&mut PlannedChannel, Error> {
        self.repair
            .as_mut()
            .and_then(|plan| {
                plan.channels
                    .iter_mut()
                    .find(|channel| channel.creation == creation)
            })
            .ok_or_else(invalid)
    }
}

fn invalid() -> Error {
    Error::unavailable("invalid saved repair progress")
}
