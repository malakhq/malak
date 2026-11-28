//! Configurable delivery defaults; authority and link validation are fixed.

use crate::cli::{
    board_names,
    config::{Board, Config},
    org::seats::{self, Reader},
};
use malak::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::cli) struct Options {
    pub levels: Option<Vec<Level>>,
    pub digest: Option<Vec<String>>,
    pub ready_level: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::cli) struct Level {
    pub name: String,
    pub delivery: String,
    pub pulse: String,
}

#[derive(Clone)]
pub(in crate::cli) struct Settings {
    pub levels: Vec<Level>,
    pub digest: Vec<String>,
    pub ready_level: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ready_level: "orange".into(),
            levels: [
                ("blue", "digest", "green"),
                ("orange", "listed", "yellow"),
                ("red", "tick", "red"),
            ]
            .into_iter()
            .map(|(name, delivery, pulse)| Level {
                name: name.into(),
                delivery: delivery.into(),
                pulse: pulse.into(),
            })
            .collect(),
            digest: ["briefs", "finished", "blockers", "calls"]
                .map(str::to_owned)
                .to_vec(),
        }
    }
}

impl Options {
    pub fn validate(&self) -> Result<(), Error> {
        if let Some(levels) = &self.levels {
            if levels.is_empty() {
                return Err(Error::refused(
                    "calls.levels must contain at least one level",
                ));
            }
            let mut names = BTreeSet::new();
            for level in levels {
                if level.name.is_empty()
                    || level.name.contains(['\r', '\n', ','])
                    || !names.insert(&level.name)
                {
                    return Err(Error::refused(
                        "calls.levels names must be unique nonempty single lines without commas",
                    ));
                }
                if !["tick", "listed", "digest"].contains(&level.delivery.as_str()) {
                    return Err(Error::refused(
                        "calls.levels delivery must be tick, listed or digest",
                    ));
                }
                if !["red", "yellow", "green", "black"].contains(&level.pulse.as_str()) {
                    return Err(Error::refused(
                        "calls.levels pulse must be red, yellow, green or black",
                    ));
                }
            }
        }
        if self.digest.as_ref().is_some_and(|digest| {
            digest.iter().any(|section| {
                !["briefs", "finished", "blockers", "calls"].contains(&section.as_str())
            })
        }) {
            return Err(Error::refused(
                "calls.digest entries must be briefs, finished, blockers or calls",
            ));
        }
        Ok(())
    }
}

impl Settings {
    pub fn resolve(roster: Option<&Options>, board: &Options) -> Result<Self, Error> {
        board.validate()?;
        let mut settings = Self::default();
        if let Some(roster) = roster {
            roster.validate()?;
            if let Some(level) = &roster.ready_level {
                settings.ready_level = level.clone();
            }
            if board.ready_level.is_some() {
                return Err(Error::refused(
                    "board calls.ready_level cannot override the roster",
                ));
            }
            if let Some(levels) = &roster.levels {
                settings.levels = levels.clone();
            }
            if let Some(digest) = &roster.digest {
                settings.digest = digest.clone();
            }
            if let Some(overrides) = &board.levels {
                let mut previous = None;
                for level in overrides {
                    let index = settings.levels.iter().position(|known| known.name == level.name).ok_or_else(|| Error::refused("board calls.levels may override delivery and pulse, never roster names"))?;
                    if previous.is_some_and(|previous| index <= previous) {
                        return Err(Error::refused(
                            "board calls.levels must keep the roster severity order",
                        ));
                    }
                    settings.levels[index] = level.clone();
                    previous = Some(index);
                }
            }
        } else if let Some(levels) = &board.levels {
            settings.levels = levels.clone();
        }
        if roster.is_none()
            && let Some(level) = &board.ready_level
        {
            settings.ready_level = level.clone();
        }
        if !settings.known(&settings.ready_level) {
            return Err(Error::refused(
                "calls.ready_level (default orange, configurable) must name a calls.levels entry",
            ));
        }
        if let Some(digest) = &board.digest {
            settings.digest = digest.clone();
        }
        Ok(settings)
    }
    pub fn severity(&self, raw: &str) -> usize {
        self.levels
            .iter()
            .position(|level| level.name == raw)
            .unwrap_or(self.levels.len() - 1)
    }
    pub fn level(&self, raw: &str) -> &Level {
        &self.levels[self.severity(raw)]
    }
    pub fn known(&self, raw: &str) -> bool {
        self.levels.iter().any(|level| level.name == raw)
    }
}

pub(in crate::cli) fn for_board(reader: &Reader, board: &str) -> Result<Settings, Error> {
    let seat = reader.open_board(Some(board))?;
    let config = Config::read(Some(seat.config))?;
    Settings::resolve(
        reader.roster.as_ref().map(|roster| &roster.calls),
        &config.board.calls,
    )
}

/// Validate both local settings and the organisation's level vocabulary before
/// opening the board. This reads owner state only, never participant messages.
pub(in crate::cli) fn validate_start(board: &Board) -> Result<(), Error> {
    board.calls.validate()?;
    for entry in board_names::list_quiet()? {
        if let Some(roster) = seats::load(&entry.path)? {
            let belongs = Config::read(Some(roster.top.clone()))?.board.state == board.state
                || roster.leads.values().any(|lead| {
                    Config::read(Some(lead.config.clone()))
                        .is_ok_and(|config| config.board.state == board.state)
                });
            if belongs {
                Settings::resolve(Some(&roster.calls), &board.calls)?;
                return Ok(());
            }
        }
    }
    Settings::resolve(None, &board.calls)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(levels: &[(&str, &str, &str)]) -> Options {
        Options {
            levels: Some(
                levels
                    .iter()
                    .map(|(name, delivery, pulse)| Level {
                        name: (*name).into(),
                        delivery: (*delivery).into(),
                        pulse: (*pulse).into(),
                    })
                    .collect(),
            ),
            digest: None,
            ready_level: None,
        }
    }
    #[test]
    fn duplicate_level_names_are_refused() {
        assert!(
            options(&[("one", "tick", "red"), ("one", "digest", "green")])
                .validate()
                .is_err()
        );
    }
    #[test]
    fn an_empty_level_map_is_refused() {
        assert!(options(&[]).validate().is_err());
    }
    #[test]
    fn unknown_delivery_is_refused() {
        assert!(options(&[("one", "interrupt", "red")]).validate().is_err());
    }
    #[test]
    fn unknown_pulse_is_refused() {
        assert!(options(&[("one", "tick", "purple")]).validate().is_err());
    }
    #[test]
    fn a_board_cannot_rename_a_roster_level() {
        assert!(
            Settings::resolve(
                Some(&Options::default()),
                &options(&[("urgent", "tick", "red")])
            )
            .is_err()
        );
    }
    #[test]
    fn a_board_cannot_reorder_roster_levels() {
        assert!(
            Settings::resolve(
                Some(&Options::default()),
                &options(&[("red", "tick", "red"), ("blue", "digest", "green")])
            )
            .is_err()
        );
    }
    #[test]
    fn a_board_can_override_delivery_and_pulse() {
        let settings = Settings::resolve(
            Some(&Options::default()),
            &options(&[("blue", "listed", "yellow")]),
        )
        .unwrap();
        assert_eq!(settings.level("blue").delivery, "listed");
        assert_eq!(settings.level("blue").pulse, "yellow");
    }
    #[test]
    fn an_unknown_stored_level_uses_the_most_severe_current_level() {
        let mut options = options(&[("later", "digest", "green"), ("now", "tick", "red")]);
        options.ready_level = Some("later".into());
        let settings = Settings::resolve(None, &options).unwrap();
        assert_eq!(settings.level("removed").name, "now");
    }
    #[test]
    fn the_ready_level_must_name_the_current_map() {
        let options = Options {
            ready_level: Some("removed".into()),
            ..Options::default()
        };
        assert!(Settings::resolve(None, &options).is_err());
    }
    #[test]
    fn a_board_cannot_override_the_rosters_ready_level() {
        let options = Options {
            ready_level: Some("red".into()),
            ..Options::default()
        };
        assert!(Settings::resolve(Some(&Options::default()), &options).is_err());
    }
}
