//! The tools that are existing recipes: `ask`, `worker-start` and `workers`.
//!
//! Each runs `malak recipe` as a child of the server, so a recipe behaves as it
//! does when the agent runs it: same handover, same board, same refusals.

use super::{Reply, seat::Seat};
use malak::Error;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    process::{Command, Stdio},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AskArgs {
    question: String,
    options: Vec<AskOption>,
    recommendation: String,
    blocks: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskOption {
    option: String,
    pros: String,
    cons: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartArgs {
    name: String,
    brief: String,
    tool: Tool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tool {
    Claude,
    Codex,
}

impl Tool {
    /// The tool's non-interactive command. It adds no permission flags, so the
    /// worker runs under the tool's own settings.
    fn command(self) -> [&'static str; 3] {
        let prompt =
            "Your task from the lead is in your Malak inbox. Do it, then report to the lead.";
        match self {
            Self::Claude => ["claude", "-p", prompt],
            Self::Codex => ["codex", "exec", prompt],
        }
    }
}

pub(super) fn ask_owner(seat: &Seat, args: AskArgs) -> Result<Reply, Error> {
    let mut recipe = vec![
        "ask".to_owned(),
        "--blocks".to_owned(),
        args.blocks,
        "--recommend".to_owned(),
        args.recommendation,
    ];
    for option in args.options {
        recipe.push("--option".to_owned());
        recipe.push(format!(
            "{} Pros: {} Cons: {}",
            option.option, option.pros, option.cons
        ));
    }
    let text = run(seat, &recipe, args.question.as_bytes())?;
    Ok(Reply {
        value: json!({ "posted": true, "receipt": text }),
        text,
    })
}

pub(super) fn start_agent(seat: &Seat, args: StartArgs) -> Result<Reply, Error> {
    let brief = BriefFile::write(&args.brief)?;
    let mut recipe = vec![
        "worker-start".to_owned(),
        args.name.clone(),
        "--brief".to_owned(),
        brief.path.clone(),
        "--detach".to_owned(),
        "--".to_owned(),
    ];
    recipe.extend(args.tool.command().map(str::to_owned));
    let text = run(seat, &recipe, &[])?;
    Ok(Reply {
        value: json!({ "started": args.name, "output": text }),
        text,
    })
}

pub(super) fn agents(seat: &Seat) -> Result<Reply, Error> {
    let listing = workers(seat, None)?;
    let mut text = String::new();
    for row in &listing {
        text.push_str(&format!(
            "{} · {} · inbox {} · {}{}\n",
            row["name"].as_str().unwrap_or("?"),
            row["state"].as_str().unwrap_or("?"),
            row["inbox"].as_str().unwrap_or("?"),
            row["enrolment"].as_str().unwrap_or("?"),
            row["unread"]
                .as_u64()
                .map_or_else(String::new, |unread| format!(" · {unread} unread")),
        ));
    }
    if text.is_empty() {
        text.push_str("No agents.\n");
    }
    Ok(Reply {
        text,
        value: json!({ "agents": listing }),
    })
}

/// The inbox of the agent the lead names, as `workers` lists it.
pub(super) fn inbox_of(seat: &Seat, name: &str) -> Result<String, Error> {
    let found = workers(seat, Some(name))?
        .into_iter()
        .find(|row| row["active"] == true && row["enrolment"] == "active")
        .ok_or_else(|| Error::refused(format!("no active agent named {name} on the board")))?;
    found["inbox"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::unavailable("the agent list names no inbox"))
}

fn workers(seat: &Seat, name: Option<&str>) -> Result<Vec<Value>, Error> {
    let mut recipe = vec!["workers".to_owned()];
    recipe.extend(name.map(str::to_owned));
    recipe.push("--json".to_owned());
    let output = run(seat, &recipe, &[])?;
    let listing: Value = serde_json::from_str(&output)
        .map_err(|_| Error::unavailable("the agent list was not understood"))?;
    Ok(listing["workers"].as_array().cloned().unwrap_or_default())
}

/// Run `malak recipe ...` on the Seat's board and return what it printed.
fn run(seat: &Seat, recipe: &[String], input: &[u8]) -> Result<String, Error> {
    let executable = std::env::current_exe().map_err(super::super::config::io_error)?;
    let mut command = Command::new(executable);
    if let Some(config) = &seat.config {
        command.arg("--config").arg(config);
    }
    let mut child = command
        .arg("recipe")
        .args(recipe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(super::super::config::io_error)?;
    let mut stdin = child.stdin.take().expect("piped standard input");
    // A recipe that needs no input exits without reading it.
    let _ = stdin.write_all(input);
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(super::super::config::io_error)?;
    let printed = |bytes: &[u8]| seat.scrub(String::from_utf8_lossy(bytes).trim());
    if output.status.success() {
        return Ok(printed(&output.stdout));
    }
    let reason = printed(&output.stderr);
    Err(Error::refused(if reason.is_empty() {
        format!("the recipe failed ({})", output.status)
    } else {
        reason
    }))
}

/// The brief on disk for `worker-start --brief`, private to this account and
/// removed once the recipe has posted it.
struct BriefFile {
    path: String,
}

impl BriefFile {
    fn write(text: &str) -> Result<Self, Error> {
        let path = std::env::temp_dir().join(format!(
            "malak-brief-{}-{}",
            std::process::id(),
            crate::cli::participant::random_id()?
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(super::super::config::io_error)?;
        file.write_all(text.as_bytes())
            .map_err(super::super::config::io_error)?;
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
        })
    }
}

impl Drop for BriefFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
