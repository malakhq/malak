//! The brief tools call the same projection and writer as the command line.

use super::{Reply, arguments};
use crate::cli::briefs;
use malak::Error;
use rmcp::model::Tool;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

pub(super) const NAMES: [&str; 4] = ["briefs", "brief", "brief_view", "brief_put"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    board: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    board: String,
    position: u64,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct View {
    since: Option<String>,
}

pub(super) fn call(
    config: Option<PathBuf>,
    name: &str,
    input: Value,
    send_id: &str,
) -> Result<Reply, Error> {
    let result = match name {
        "briefs" => {
            let args: List = arguments(input)?;
            briefs::list(config, args.board.as_deref(), args.kind.as_deref())?
        }
        "brief" => {
            let args: Get = arguments(input)?;
            briefs::get(config, &args.board, args.position, args.offset)?
        }
        "brief_view" => {
            let args: View = arguments(input)?;
            briefs::view(config, args.since.as_deref().unwrap_or("24h"))?
        }
        "brief_put" => briefs::put(config, arguments(input)?, Some(send_id.to_owned()))?,
        _ => return Err(Error::refused("unknown brief tool")),
    };
    Ok(Reply {
        text: result.text,
        value: result.value,
    })
}

pub(super) fn tools() -> Vec<Tool> {
    [
        ("briefs", "List current briefs grouped by lead, using only your existing read seats. Superseded and withdrawn briefs are hidden. Subs receive needed brief text in their tasks.", json!({
            "board": {"type":"string"}, "kind": {"type":"string"}
        }), vec![]),
        ("brief", "Get an original immutable brief and its supersession or withdrawal status. Long sources include paging guidance.", json!({
            "board": {"type":"string"}, "position": {"type":"integer", "minimum":1},
            "offset": {"type":"integer", "minimum":0, "description":"Byte offset from the previous fetch hint."}
        }), vec!["board", "position"]),
        ("brief_view", "Read a linked digest of your own board, or every roster board as chief of staff. It includes briefs, finished work, blockers and open calls when available; unreadable boards are named. The result fits 32 KiB and names the first omitted board.", json!({
            "since": {"type":"string", "description":"Duration such as 24h, 7d or 30m; defaults to 24h."}
        }), vec![]),
        ("brief_put", "Store a brief as this board's lead. Replaces supersedes a prior proven brief; kind withdrawn hides that brief from listings. The original stays retrievable. Put needed brief text directly in a sub's task, never a reference into a brief artifact store.", json!({
            "title":{"type":"string"},
            "kind":{"type":"string", "enum":["report","decision","spec","plan","learned","handover","exit","other","withdrawn"]},
            "body":{"type":"string"},
            "replaces":{"type":"integer", "minimum":1}
        }), vec!["title", "kind", "body"]),
    ].into_iter().map(|(name, description, properties, required)| {
        let Value::Object(schema) = json!({"type":"object", "properties":properties,
            "required":required,"additionalProperties":false}) else { unreachable!("object literal") };
        Tool::new(name, description, schema)
    }).collect()
}
