use crate::cli::{config, owner_commands, worker_launch};
use malak::{Error, ParticipantId};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
pub(super) struct Worker {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub launching: bool,
}

#[derive(Deserialize)]
struct Status {
    run: String,
    participants: Vec<Participant>,
}

#[derive(Deserialize)]
struct Participant {
    id: String,
    name: String,
    connections: u64,
}

pub(super) fn observe(
    client: &mut owner_commands::Client,
    settings: &config::Board,
) -> Result<(String, Vec<Worker>), Error> {
    let status: Status = serde_json::from_value(owner_commands::status(client, &[])?)
        .map_err(|_| Error::unavailable("invalid owner response"))?;
    let launches = worker_launch::open(&settings.state).map_err(config::io_error)?;
    let mut workers = Vec::new();
    for participant in status.participants {
        if participant.id == "lead" {
            continue;
        }
        let id: ParticipantId = participant.id.parse()?;
        let launching = launches.as_ref().map_or(Ok(false), |directory| {
            worker_launch::live_claim(directory, &id).map_err(config::io_error)
        })?;
        if participant.connections > 0 || launching {
            workers.push(Worker {
                id: participant.id,
                name: participant.name,
                connected: participant.connections > 0,
                launching,
            });
        }
    }
    Ok((status.run, workers))
}
