//! Apply brief supersession to immutable source messages, without I/O.

use malak::Error;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

pub(super) const KINDS: &[&str] = &[
    "report",
    "decision",
    "spec",
    "plan",
    "learned",
    "handover",
    "exit",
    "other",
    "withdrawn",
];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Link {
    pub board: String,
    pub channel: String,
    pub creation: String,
    pub position: u64,
}

/// An authenticated source and fields accepted by the shared R7 parser.
pub(super) struct Input {
    pub link: Link,
    pub body: String,
    pub fields: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Brief {
    pub link: Link,
    pub body: String,
    pub presentation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<Link>,
}

impl Brief {
    pub(super) fn current(&self) -> bool {
        self.status.is_none_or(|status| status == "current")
    }
}

pub(super) fn replacement(value: &str) -> Option<(String, u64)> {
    let ["briefs", creation, position] = value.split_whitespace().collect::<Vec<_>>()[..] else {
        return None;
    };
    let _: malak::CreationId = creation.parse().ok()?;
    let position = position
        .parse::<u64>()
        .ok()
        .filter(|position| *position > 0)?;
    Some((creation.to_owned(), position))
}

/// Walk in source order. A plain note never changes another brief's status.
pub(super) fn project(mut inputs: Vec<Input>) -> Vec<Brief> {
    inputs.sort_by_key(|input| input.link.position);
    let mut rows: Vec<Brief> = Vec::with_capacity(inputs.len());
    let mut known: HashMap<(String, u64), usize> = HashMap::new();
    for input in inputs {
        let mut row = Brief {
            link: input.link,
            body: input.body,
            presentation: "information",
            title: None,
            kind: None,
            lead: None,
            status: None,
            replaces: None,
            replaced_by: None,
        };
        if let Some(fields) = input.fields {
            let target = fields.get("Replaces").and_then(|value| replacement(value));
            let previous = target.as_ref().and_then(|key| known.get(key)).copied();
            let valid_target = !fields.contains_key("Replaces")
                || target.as_ref().is_some_and(|(creation, position)| {
                    creation == &row.link.creation
                        && *position < row.link.position
                        && previous.is_some()
                });
            if let (Some(title), Some(kind), Some(lead)) =
                (fields.get("Title"), fields.get("Kind"), fields.get("Lead"))
                && !title.trim().is_empty()
                && !lead.trim().is_empty()
                && KINDS.contains(&kind.as_str())
                && valid_target
                && (kind != "withdrawn" || previous.is_some())
            {
                row.title = Some(title.clone());
                row.kind = Some(kind.clone());
                row.lead = Some(lead.clone());
                row.status = Some(if kind == "withdrawn" {
                    "withdrawal"
                } else {
                    "current"
                });
                if let Some(index) = previous {
                    let prior = &mut rows[index];
                    prior.status = Some(if kind == "withdrawn" {
                        "withdrawn"
                    } else {
                        "superseded"
                    });
                    prior.replaced_by = Some(row.link.clone());
                    row.replaces = Some(prior.link.clone());
                }
                known.insert((row.link.creation.clone(), row.link.position), rows.len());
            }
        }
        rows.push(row);
    }
    rows
}

pub(super) fn body(
    title: &str,
    kind: &str,
    lead: &str,
    text: &str,
    replaces: Option<&Link>,
) -> Result<String, Error> {
    for (label, value) in [("Title", title), ("Lead", lead)] {
        if value.trim().is_empty() || value.contains(['\r', '\n', '\0']) {
            return Err(Error::refused(format!("{label} must be one nonempty line")));
        }
    }
    if !KINDS.contains(&kind) {
        return Err(Error::refused(format!("unknown brief kind {kind:?}")));
    }
    if kind == "withdrawn" && replaces.is_none() {
        return Err(Error::refused(
            "a withdrawn brief needs --replaces POSITION",
        ));
    }
    if kind != "withdrawn" && text.trim().is_empty() {
        return Err(Error::refused(
            "give the brief text or artifact reference on standard input",
        ));
    }
    let mut body = format!("Type: brief\nTitle: {title}\nKind: {kind}\nLead: {lead}\n");
    if let Some(link) = replaces {
        body.push_str(&format!(
            "Replaces: briefs {} {}\n",
            link.creation, link.position
        ));
    }
    body.push('\n');
    body.push_str(text);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATION: &str = "11111111111111111111111111111111";

    fn input(position: u64, kind: &str, replaces: Option<u64>) -> Input {
        let mut fields = BTreeMap::from([
            ("Title".into(), format!("Brief {position}")),
            ("Kind".into(), kind.into()),
            ("Lead".into(), "build".into()),
        ]);
        if let Some(position) = replaces {
            fields.insert("Replaces".into(), format!("briefs {CREATION} {position}"));
        }
        Input {
            link: Link {
                board: "build".into(),
                channel: "briefs".into(),
                creation: CREATION.into(),
                position,
            },
            body: format!("Original text {position}"),
            fields: Some(fields),
        }
    }

    #[test]
    fn a_replacement_hides_the_prior_brief_from_listing() {
        let rows = project(vec![input(1, "report", None), input(2, "learned", Some(1))]);
        assert!(!rows[0].current());
        assert!(rows[1].current());
        assert_eq!(rows[0].replaced_by.as_ref().unwrap().position, 2);
    }

    #[test]
    fn a_withdrawal_hides_both_the_target_and_the_withdrawal() {
        let rows = project(vec![
            input(1, "handover", None),
            input(2, "withdrawn", Some(1)),
        ]);
        assert!(rows.iter().all(|row| !row.current()));
        assert_eq!(rows[0].status, Some("withdrawn"));
    }

    #[test]
    fn getting_an_old_brief_keeps_its_original_text() {
        let rows = project(vec![input(1, "exit", None), input(2, "exit", Some(1))]);
        assert_eq!(rows[0].body, "Original text 1");
        assert_eq!(rows[0].status, Some("superseded"));
    }

    #[test]
    fn an_unproven_replacement_stays_a_plain_note() {
        let mut spoof = input(2, "withdrawn", Some(1));
        spoof.fields = None;
        let rows = project(vec![input(1, "report", None), spoof]);
        assert!(rows[0].current());
        assert_eq!(rows[1].body, "Original text 2");
        assert_eq!(rows[1].kind, None);
    }

    #[test]
    fn a_reference_to_another_creation_cannot_replace_a_brief() {
        let mut changed = input(2, "report", Some(1));
        changed.link.creation = "22222222222222222222222222222222".into();
        let rows = project(vec![input(1, "report", None), changed]);
        assert!(rows[0].current());
        assert_eq!(rows[1].kind, None);
    }

    #[test]
    fn a_forward_reference_cannot_replace_a_brief() {
        let rows = project(vec![input(1, "report", Some(2)), input(2, "report", None)]);
        assert_eq!(rows[0].kind, None);
    }

    #[test]
    fn withdrawal_requires_a_target() {
        assert!(body("Withdrawn", "withdrawn", "build", "", None).is_err());
    }

    #[test]
    fn title_cannot_inject_body_fields() {
        assert!(body("Title\nReplaces: 1", "report", "build", "Text", None).is_err());
    }

    #[test]
    fn learned_handover_and_exit_are_valid_kinds() {
        for kind in ["learned", "handover", "exit"] {
            assert!(body("Title", kind, "build", "Text", None).is_ok());
        }
    }
}
