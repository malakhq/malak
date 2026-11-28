//! Open only the caller's lead Seat or its roster-issued read-only seats.

use crate::cli::{
    board_names,
    config::{Config, io_error},
    participant::{self, Session},
};
use malak::Error;
use malak_core::HandoverFile;
use malak_fs::{Directory, check_user_permissions};
use nix::{fcntl::OFlag, unistd::geteuid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::cli) struct Lead {
    pub config: PathBuf,
    pub active: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::cli) struct Seat {
    pub board: String,
    pub config: PathBuf,
    pub id: String,
    pub handover: PathBuf,
    pub top: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::cli) struct Roster {
    pub top: PathBuf,
    pub leads: BTreeMap<String, Lead>,
    pub chief_of_staff: Option<Value>,
    pub candidate: Option<Value>,
    pub seats: BTreeMap<String, Vec<Seat>>,
    pub rail_age_days: u64,
    pub lead_silent_minutes: u64,
    pub work_stale_minutes: u64,
    pub ramp_minutes: u64,
}

pub(in crate::cli) struct Reader {
    pub home: PathBuf,
    pub name: String,
    pub home_board: String,
    pub seat_key: String,
    pub roster: Option<Roster>,
    pub chief: bool,
}

pub(in crate::cli) struct ReadSeat {
    pub board: String,
    pub config: PathBuf,
    pub session: Session,
}

/// A foreign read uses the caller's own recorded reader seat. The selector
/// names the board; it never supplies a handover or another lead's authority.
pub(in crate::cli) fn foreign(config: &Option<PathBuf>) -> Result<Option<ReadSeat>, Error> {
    let Some(path) = config else { return Ok(None) };
    let handover = primary_handover(
        config.clone(),
        std::env::var_os("MALAK_HANDOVER").map(PathBuf::from),
    )?;
    if Config::read(config.clone())?
        .board
        .check_handover(path, &handover)
        .is_ok()
    {
        return Ok(None);
    }
    let reader = Reader::open(config.clone())?;
    let name = board_names::name(&crate::cli::board_group::Entry::read(path.clone()));
    let seat = reader.open_board(Some(&name))?;
    if seat.config.canonicalize().map_err(io_error)? != path.canonicalize().map_err(io_error)? {
        return Err(Error::refused(
            "selected board does not match your recorded reader seat",
        ));
    }
    Ok(Some(seat))
}

fn load(config: &Path) -> Result<Option<Roster>, Error> {
    let board = Config::read(Some(config.to_owned()))?.board;
    let file = match Directory::open(&board.state)
        .and_then(|root| root.child("org".as_ref()))
        .and_then(|root| root.open_file("roster.json", OFlag::O_RDONLY))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    check_user_permissions(&file, geteuid().as_raw(), true).map_err(io_error)?;
    serde_json::from_reader(file)
        .map(Some)
        .map_err(|error| io_error(error.into()))
}

pub(in crate::cli) fn primary_home(
    config: Option<PathBuf>,
) -> Result<(PathBuf, Vec<PathBuf>), Error> {
    // An explicit board is the requested view, while the inherited handover
    // still identifies the reader's home board.
    let handover = primary_handover(
        config.clone(),
        std::env::var_os("MALAK_HANDOVER").map(PathBuf::from),
    )?;
    if !handover.id().is_lead() {
        return Err(Error::refused(
            "organisation views require a lead or chief of staff Seat; subs cannot read them",
        ));
    }
    let mut paths: Vec<PathBuf> = config.into_iter().collect();
    paths.extend(
        board_names::list_quiet()?
            .into_iter()
            .map(|entry| entry.path),
    );
    let home = paths
        .iter()
        .find(|path| {
            Config::read(Some((*path).clone()))
                .is_ok_and(|config| config.board.check_handover(path, &handover).is_ok())
        })
        .cloned()
        .ok_or_else(|| Error::refused("cannot find the reader's home board"))?;
    Ok((home, paths))
}

impl Reader {
    pub fn open(config: Option<PathBuf>) -> Result<Self, Error> {
        let (home, paths) = primary_home(config)?;
        // Validate the caller's session before consulting its roster.
        let mut session = Session::open(Some(home.clone()))?;
        let _ = super::lifecycle::reconcile(&home, &mut session);
        let home_name = board_names::name(&crate::cli::board_group::Entry::read(home.clone()));
        let mut roster = None;
        for path in paths {
            if let Some(found) = load(&path)?
                && (found.top == home || found.leads.values().any(|lead| lead.config == home))
            {
                roster = Some(found);
                break;
            }
        }
        let name = roster
            .as_ref()
            .and_then(|r| {
                if r.top == home {
                    r.chief_of_staff
                        .as_ref()
                        .and_then(|chief| chief["name"].as_str())
                        .map(str::to_owned)
                } else {
                    r.leads
                        .iter()
                        .find(|(_, lead)| lead.config == home)
                        .map(|(name, _)| name.clone())
                }
            })
            .unwrap_or_else(|| home_name.clone());
        let chief = roster
            .as_ref()
            .is_some_and(|r| r.top == home && r.chief_of_staff.is_some());
        let seat_key = format!("{}:{name}", if chief { "chief_of_staff" } else { "lead" });
        Ok(Self {
            seat_key,
            home,
            name,
            home_board: home_name,
            roster,
            chief,
        })
    }

    pub fn boards(&self) -> Vec<String> {
        let mut names = vec![self.home_board.clone()];
        if let Some(roster) = &self.roster
            && let Some(seats) = roster.seats.get(&self.seat_key)
        {
            names.extend(
                seats
                    .iter()
                    .filter(|seat| {
                        seat.top
                            || roster
                                .leads
                                .get(&seat.board)
                                .is_some_and(|lead| lead.active)
                    })
                    .map(|seat| seat.board.clone()),
            );
        }
        names.sort();
        names.dedup();
        names
    }

    pub fn open_board(&self, name: Option<&str>) -> Result<ReadSeat, Error> {
        let name = name.unwrap_or(&self.home_board);
        if name == self.home_board {
            return Ok(ReadSeat {
                board: name.to_owned(),
                config: self.home.clone(),
                session: Session::open(Some(self.home.clone()))?,
            });
        }
        let roster = self.roster.as_ref().ok_or_else(|| {
            Error::refused("no cross-board read seat; ask the owner to run org setup")
        })?;
        if roster
            .leads
            .get(&self.name)
            .is_some_and(|lead| !lead.active)
        {
            return Err(Error::refused(
                "your lead was removed from the organisation",
            ));
        }
        let seat = roster
            .seats
            .get(&self.seat_key)
            .into_iter()
            .flatten()
            .find(|seat| seat.board == name)
            .ok_or_else(|| Error::refused(format!("no read seat on board {name}")))?;
        if !seat.top && !roster.leads.get(name).is_some_and(|lead| lead.active) {
            return Err(Error::refused(format!(
                "board {name} was removed from the organisation"
            )));
        }
        let handover = HandoverFile::read(&seat.handover).map_err(|_| {
            Error::refused(format!(
                "cannot open your read seat on board {name}; ask the owner to run org reseat"
            ))
        })?;
        if handover.id().as_str() != seat.id || handover.id().is_lead() {
            return Err(Error::refused("organisation read seat changed identity"));
        }
        Config::read(Some(seat.config.clone()))?
            .board
            .check_handover(&seat.handover, &handover)
            .map_err(|_| {
                Error::refused(format!(
                    "your read seat no longer matches board {name}; ask the owner to run org reseat"
                ))
            })?;
        Ok(ReadSeat {
            board: name.to_owned(),
            config: seat.config.clone(),
            session: Session::open_guest(handover, Duration::from_secs(2))?,
        })
    }
}

fn primary_handover(
    config: Option<PathBuf>,
    inherited: Option<PathBuf>,
) -> Result<HandoverFile, Error> {
    match inherited {
        // Resolve the home board from this pinned identity, then validate and
        // attach there. An unrelated or absent default config is irrelevant.
        Some(path) => HandoverFile::read(path),
        None => participant::preview_handover(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use malak_core::{HandoverTarget, HandoverWrite};
    use malak_proto::{Binding, Entrance};
    use malak_testkit::unix_socket::SocketDir;

    #[test]
    fn explicit_config_with_inherited_key_does_not_require_a_default_board() {
        let root = SocketDir::new().unwrap();
        let path = root.path().join("lead.handover");
        let key = HandoverFile::new(
            Binding {
                version: 1,
                entrance: Entrance::Lead,
            },
            format!("unix:{}/l/s", root.path().display()),
            [3; 32],
            "lead".parse().unwrap(),
            String::new(),
            Vec::new().into(),
            [7; 32].into(),
        )
        .unwrap();
        HandoverTarget::prepare(&path, geteuid().as_raw(), HandoverWrite::New)
            .unwrap()
            .write(&key)
            .unwrap();
        let config = root.path().join("explicit.toml");
        std::fs::write(
            &config,
            format!(
                "[board]\nstate = {:?}\nsocket_folder = {:?}\n",
                root.path(),
                root.path()
            ),
        )
        .unwrap();
        assert!(!root.path().join("default/board.toml").exists());
        let inherited = primary_handover(Some(config), Some(path)).unwrap();
        assert_eq!(inherited.board_key(), &[3; 32]);
    }

    #[test]
    fn chiefs_home_board_label_is_independent_of_its_reader_name() {
        let reader = Reader {
            home: PathBuf::from("/fixture/top.toml"),
            name: "chief".into(),
            home_board: "top".into(),
            seat_key: "chief_of_staff:chief".into(),
            roster: None,
            chief: true,
        };
        assert_eq!(reader.boards(), vec!["top"]);
    }
}
