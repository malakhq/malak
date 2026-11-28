//! The brief store and its consumable views, computed by the reading client.

mod model;
mod view;

use super::{
    config::io_error,
    org::{bounded, records, seats::Reader, typed},
    output, participant,
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use malak::{Error, Kind, Limits};
use serde::Deserialize;
use serde_json::Value;
use std::{ffi::OsString, io::Read, path::PathBuf};

/// Text and structured facts shared by the CLI and MCP.
pub(super) struct ResultView {
    pub text: String,
    pub value: Value,
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Get an immutable brief, including supersession or withdrawal status.
    #[command(after_help = super::examples::help(&["brief get"]))]
    Get {
        #[arg(value_name = "BOARD")]
        target: String,
        position: u64,
        /// Start the original brief body at this byte offset.
        #[arg(long)]
        offset: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Show a small linked view of current briefs and board activity.
    #[command(after_help = super::examples::help(&["brief view"]))]
    View {
        #[arg(long, default_value = "24h")]
        since: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutArgs {
    pub title: String,
    pub kind: String,
    pub body: String,
    pub replaces: Option<u64>,
}

#[derive(Parser)]
#[command(name = "malak recipe brief", about = "Put an immutable brief on the board.", after_help = super::examples::help(&["recipe brief"]))]
struct Recipe {
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "board")]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_name = "NAME")]
    board: Option<String>,
    #[command(subcommand)]
    command: RecipeCommand,
}

#[derive(Subcommand)]
enum RecipeCommand {
    /// Store a brief. Read its text or artifact reference from standard input.
    Put {
        #[arg(long)]
        title: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        replaces: Option<u64>,
    },
}

pub(super) fn put_recipe(
    args: &[OsString],
    config: Option<PathBuf>,
) -> Result<std::process::ExitCode, Error> {
    let words: Vec<_> = std::iter::once(OsString::from("malak recipe brief"))
        .chain(args.iter().cloned())
        .collect();
    let command = super::help::configure(Recipe::command());
    let words = super::help::bare_help(&command, &words).unwrap_or(words);
    let recipe = match command
        .try_get_matches_from(words)
        .and_then(|matches| Recipe::from_arg_matches(&matches))
    {
        Ok(recipe) => recipe,
        Err(error) => {
            let help = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            error.print().map_err(io_error)?;
            return Ok(std::process::ExitCode::from(if help { 0 } else { 2 }));
        }
    };
    if config.is_some() && (recipe.config.is_some() || recipe.board.is_some()) {
        return Err(Error::refused(
            "choose a board only once with --board or --config",
        ));
    }
    let config = match recipe.board {
        Some(board) => Some(super::board_names::resolve(&board)?),
        None => config.or(recipe.config),
    };
    let RecipeCommand::Put {
        title,
        kind,
        replaces,
    } = recipe.command;
    let mut body = String::new();
    std::io::stdin()
        .take((Limits::default().body_bytes + 1) as u64)
        .read_to_string(&mut body)
        .map_err(io_error)?;
    let result = put(
        config,
        PutArgs {
            title,
            kind,
            replaces,
            body,
        },
        None,
    )?;
    output::line(format_args!("{}", result.text))?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn body(record: &Value) -> Result<String, Error> {
    if let Some(body) = record["body"].as_str() {
        return Ok(body.to_owned());
    }
    let body = record["body"]
        .as_array()
        .ok_or_else(|| Error::unavailable("brief source has no body"))?;
    let bytes = body
        .iter()
        .map(|value| value.as_u64().and_then(|n| u8::try_from(n).ok()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::unavailable("invalid brief source body"))?;
    String::from_utf8(bytes).map_err(|_| Error::refused("brief source is not UTF-8"))
}

fn projection(seat: &mut super::org::seats::ReadSeat) -> Result<Vec<model::Brief>, Error> {
    let messages = records::retained(&seat.config, &mut seat.session, &["briefs"])?;
    project(&seat.board, &messages)
}

fn project(board: &str, messages: &[Value]) -> Result<Vec<model::Brief>, Error> {
    let inputs = messages
        .iter()
        .filter(|record| record["channel"] == "briefs")
        .map(|record| {
            Ok(model::Input {
                link: model::Link {
                    board: board.to_owned(),
                    channel: "briefs".into(),
                    creation: record["creation"]
                        .as_str()
                        .ok_or_else(|| Error::unavailable("brief source has no creation"))?
                        .to_owned(),
                    position: record["position"]
                        .as_u64()
                        .ok_or_else(|| Error::unavailable("brief source has no position"))?,
                },
                body: body(record)?,
                fields: typed::fields(record, "brief", "lead"),
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(model::project(inputs))
}

fn row(brief: &model::Brief, full: bool) -> Result<Value, Error> {
    let mut value = serde_json::to_value(brief).map_err(|error| io_error(error.into()))?;
    if !full && brief.kind.is_some() {
        value
            .as_object_mut()
            .expect("serialized brief is an object")
            .remove("body");
    }
    Ok(value)
}

fn result(items: Vec<Value>, hint: &str) -> ResultView {
    let boards: Vec<_> = items
        .iter()
        .map(|item| {
            item["board"]
                .as_str()
                .or_else(|| item["link"]["board"].as_str())
                .unwrap_or("unknown")
                .to_owned()
        })
        .collect();
    // Reserve the longest possible board label before choosing the page, so
    // naming the actual first omitted board cannot exceed the shared budget.
    let page_hint = |board: &str| format!("First omitted board: {board}. {hint}");
    let longest = boards
        .iter()
        .max_by_key(|board| {
            let value = serde_json::json!({"next":page_hint(board)});
            serde_json::json!({"text":value.to_string(),"structuredContent":value})
                .to_string()
                .len()
        })
        .map_or("unknown", String::as_str);
    let mut value = bounded::result(items, &page_hint(longest));
    if value["omitted"].as_u64().unwrap_or(0) > 0 {
        let shown = value["items"].as_array().map_or(0, Vec::len);
        value["next"] = page_hint(&boards[shown]).into();
    }
    // Both entrances expose exactly the same bounded facts. JSON text also
    // preserves ordinary notes unchanged, including lines that resemble fields.
    ResultView {
        text: value.to_string(),
        value,
    }
}

pub(super) fn list(
    config: Option<PathBuf>,
    board: Option<&str>,
    kind: Option<&str>,
) -> Result<ResultView, Error> {
    if let Some(kind) = kind
        && !model::KINDS.contains(&kind)
    {
        return Err(Error::refused(format!("unknown brief kind {kind:?}")));
    }
    let config = if config.is_none() && std::env::var_os("MALAK_HANDOVER").is_none() {
        board.map(super::board_names::resolve).transpose()?
    } else {
        config
    };
    let reader = Reader::open(config)?;
    let names = board.map_or_else(|| reader.boards(), |board| vec![board.to_owned()]);
    let mut items = Vec::new();
    for name in names {
        let mut seat = reader.open_board(Some(&name))?;
        for brief in projection(&mut seat)?.iter().rev() {
            if brief.current() && kind.is_none_or(|kind| brief.kind.as_deref() == Some(kind)) {
                items.push(row(brief, false)?);
            }
        }
    }
    Ok(result(
        items,
        "Page by board or kind with malak briefs --board BOARD --kind KIND; fetch an original with malak brief get BOARD POSITION and continue with --offset from its fetch hint.",
    ))
}

pub(super) fn get(
    config: Option<PathBuf>,
    board: &str,
    position: u64,
    offset: Option<usize>,
) -> Result<ResultView, Error> {
    let reader = Reader::open(config)?;
    let mut seat = reader.open_board(Some(board))?;
    let brief = projection(&mut seat)?
        .into_iter()
        .find(|brief| brief.link.position == position)
        .ok_or_else(|| Error::refused(format!("no brief at {board} briefs #{position}")))?;
    page(&brief, offset.unwrap_or(0))
}

/// Page the immutable body at UTF-8 boundaries, measuring the shared envelope.
fn page(brief: &model::Brief, offset: usize) -> Result<ResultView, Error> {
    if offset > brief.body.len() || !brief.body.is_char_boundary(offset) {
        return Err(Error::refused(
            "offset must be a UTF-8 byte boundary within the brief body",
        ));
    }
    let mut source = row(brief, true)?;
    source["body"] = "".into();
    let candidate = |source: &Value, end| {
        let mut item = source.clone();
        item["body"] = brief.body[offset..end].into();
        item["body_bytes"] = brief.body.len().into();
        item["offset"] = offset.into();
        item["omitted_bytes"] = (brief.body.len() - end).into();
        let next = (end < brief.body.len()).then(|| {
            format!(
                "malak brief get '{}' {} --offset {end}",
                brief.link.board.replace('\'', "'\\''"),
                brief.link.position,
            )
        });
        if next.is_some() {
            item["fetch"] = serde_json::json!({"tool":"brief", "board":brief.link.board,
                "position":brief.link.position, "offset":end});
        }
        serde_json::json!({"items":[item], "omitted":0, "next":next})
    };
    let metadata = candidate(&source, offset);
    if !bounded::fits(&metadata.to_string(), &metadata) {
        // These fields are already in the immutable body and can themselves
        // be long. Page that original header instead of repeating it on every
        // page. The address, status and replacement link always remain.
        let fields = source
            .as_object_mut()
            .expect("serialized brief is an object");
        fields.remove("title");
        fields.remove("lead");
        source["omitted_fields"] = serde_json::json!(["title", "lead"]);
    }
    let mut low = offset;
    let mut high = brief
        .body
        .len()
        .min(offset.saturating_add(super::presentation::BUDGET));
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let mut end = middle;
        while !brief.body.is_char_boundary(end) {
            end -= 1;
        }
        let value = candidate(&source, end);
        if bounded::fits(&value.to_string(), &value) {
            if end == low {
                let next = brief.body[end..]
                    .chars()
                    .next()
                    .map_or(end, |character| end + character.len_utf8());
                if next <= high {
                    let value = candidate(&source, next);
                    if bounded::fits(&value.to_string(), &value) {
                        low = next;
                        continue;
                    }
                }
                break;
            }
            low = end;
        } else {
            high = end.saturating_sub(1);
        }
    }
    while !brief.body.is_char_boundary(low) {
        low -= 1;
    }
    let value = candidate(&source, low);
    if !bounded::fits(&value.to_string(), &value) || low == offset && offset < brief.body.len() {
        return Ok(result(
            vec![source],
            "Brief metadata does not fit. Read the original message with the existing get tool through your read seat.",
        ));
    }
    Ok(ResultView {
        text: value.to_string(),
        value,
    })
}

pub(super) fn put(
    config: Option<PathBuf>,
    args: PutArgs,
    id: Option<String>,
) -> Result<ResultView, Error> {
    let selected = config.clone();
    let reader = Reader::open(config)?;
    if !participant::preview_handover(Some(selected.unwrap_or_else(|| reader.home.clone())))?
        .id()
        .is_lead()
    {
        return Err(Error::refused(
            "only the board's lead may put a brief; a sub receives needed brief text in its task",
        ));
    }
    let mut seat = reader.open_board(None)?;
    let replaces = if let Some(position) = args.replaces {
        Some(
            projection(&mut seat)?
                .into_iter()
                .find(|brief| brief.link.position == position && brief.kind.is_some())
                .ok_or_else(|| Error::refused("--replaces must name a prior proven brief"))?
                .link,
        )
    } else {
        None
    };
    let body = model::body(
        &args.title,
        &args.kind,
        &reader.name,
        &args.body,
        replaces.as_ref(),
    )?;
    if body.len() > Limits::default().body_bytes {
        return Err(Error::refused("brief body exceeds the message byte limit"));
    }
    let (text, value) = participant::Command::Send {
        channel: "briefs".into(),
        id,
        kind: Kind::Note,
        reply_to: Vec::new(),
        content_type: None,
        max_bytes: None,
        trace: None,
        json: true,
        body: None,
    }
    .execute_reply(&mut seat.session, Some(body.into_bytes()))?;
    Ok(ResultView { text, value })
}

pub(super) fn view(config: Option<PathBuf>, since: &str) -> Result<ResultView, Error> {
    let reader = Reader::open(config)?;
    let home = reader.open_board(None)?.board;
    let now = super::output::now_ms();
    let cutoff = super::org::rail::since_ms(Some(since), now)?;
    let mut names = vec![home.clone()];
    let pulses = if reader.chief {
        if let Some(roster) = &reader.roster {
            names.extend(roster.leads.keys().cloned());
        }
        super::org::pulse::view(&reader, now)?
    } else {
        vec![super::org::pulse::own(&reader, now)?]
    };
    names.sort();
    names.dedup();
    let mut items = Vec::new();
    for name in names {
        let fetched = (|| {
            let mut seat = reader.open_board(Some(&name))?;
            let mut channels = vec![
                "briefs",
                "rail",
                "lead",
                "questions",
                "calls",
                "relay/*",
                "to/*",
            ];
            if name == home && reader.chief {
                channels.push("cos/ramp");
            }
            records::retained(&seat.config, &mut seat.session, &channels)
        })();
        let messages = match fetched {
            Ok(messages) => messages,
            Err(_) if reader.chief => {
                items.push(serde_json::json!({"board":name,"section":"unreadable",
                    "colour":"black","reason":"not reachable","link":{"board":name}}));
                continue;
            }
            Err(error) => return Err(error),
        };
        let briefs = project(&name, &messages)?
            .iter()
            .rev()
            .filter(|brief| brief.current())
            .map(|brief| row(brief, false))
            .collect::<Result<Vec<_>, _>>()?;
        let rails: Vec<_> = messages
            .iter()
            .filter(|message| message["channel"] == "rail")
            .cloned()
            .collect();
        let rail = super::org::rail::project(&name, &rails, |_| None);
        let mut pulse = pulses
            .iter()
            .find(|pulse| pulse["lead"] == name || name == home && pulse["lead"] == reader.name)
            .cloned();
        if pulse.is_none() && name == home {
            pulse = Some(super::org::pulse::own(&reader, now)?);
        }
        if let Some(pulse) = &mut pulse {
            pulse["link"] = serde_json::json!({"board":name});
            let fact = pulse["blocker"]["link"].clone();
            if !fact.is_null() {
                pulse["link"] = fact;
            } else if let Some(message) =
                messages.iter().max_by_key(|message| typed::at_ms(message))
            {
                pulse["link"] = serde_json::json!({"board":name,"channel":message["channel"],
                    "creation":message["creation"],"position":message["position"]});
            }
        }
        // The calls lane supplies validated open calls here when available.
        let mut board_items = view::items(briefs, rail, pulse, None, cutoff);
        for item in &mut board_items {
            item["board"] = name.clone().into();
        }
        items.extend(board_items);
    }
    // Keep every board's overview and reachability ahead of artifact rows, so
    // a busy board cannot hide another board that needs attention.
    items.sort_by_key(|item| match item["section"].as_str() {
        Some("bottom line" | "unreadable") => 0,
        Some("finished") => 1,
        Some("stuck") => 2,
        Some("open calls") => 3,
        _ => 4,
    });
    Ok(result(
        items,
        "Narrow the time range with malak brief view --since 1h; list artifacts with malak briefs --board BOARD, or fetch one with malak brief get BOARD POSITION.",
    ))
}

pub(super) fn show(result: ResultView, json: bool) -> Result<(), Error> {
    if json {
        output::line(format_args!("{}", result.value))
    } else {
        output::write(&view::text(&result.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn a_long_title_is_paged_as_part_of_the_original_body() {
        let title = "Long title ".repeat(3_000);
        let original = model::body(&title, "report", "build", "Original text", None).unwrap();
        let brief = model::project(vec![model::Input {
            link: model::Link {
                board: "build".into(),
                channel: "briefs".into(),
                creation: "11111111111111111111111111111111".into(),
                position: 1,
            },
            body: original.clone(),
            fields: Some(BTreeMap::from([
                ("Title".into(), title),
                ("Kind".into(), "report".into()),
                ("Lead".into(), "build".into()),
            ])),
        }])
        .pop()
        .unwrap();
        let mut reconstructed = String::new();
        let mut offset = 0;
        loop {
            let result = page(&brief, offset).unwrap();
            let item = &result.value["items"][0];
            reconstructed.push_str(item["body"].as_str().unwrap());
            let Some(next) = item["fetch"]["offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn an_omitted_result_names_its_first_board() {
        let result = result(
            vec![
                serde_json::json!({"board":"first","text":"small"}),
                serde_json::json!({"board":"other","text":"large".repeat(10_000)}),
            ],
            "Fetch the source with brief get.",
        );
        assert_eq!(result.value["omitted"], 1);
        assert!(
            result.value["next"]
                .as_str()
                .unwrap()
                .contains("First omitted board: other")
        );
    }
}
