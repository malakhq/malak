use super::layout::Kind;
use crate::cli::recipe::{self, BUNDLE_FOLDER, BUNDLE_TEMP_PREFIX, WORKER_HOOK_TEMP_PREFIX};
use malak_core::HandoverTarget;
use nix::unistd::geteuid;

#[derive(Clone, Copy)]
pub(super) enum Folder {
    Worker,
    WorkerResult,
    Result,
    Demo,
    DemoBriefs,
    Launches,
    Scratch,
    Extraction,
    Bundle,
    PythonCache,
    WorkerHooks,
    Observe,
}

pub(super) fn state_child(name: &str) -> Option<Kind> {
    // worker.py, demo.py and worker_queue.py write these directly into state.
    match name {
        "launches" => Some(Kind::RecipeFolder(Folder::Launches)),
        // Older worker-start scripts used mktemp instead of Python's tempfile.
        _ if python_temporary(name, "worker") || native_temporary(name, "worker") => {
            Some(Kind::RecipeFolder(Folder::Worker))
        }
        _ if python_temporary(name, "demo") => Some(Kind::RecipeFolder(Folder::Demo)),
        _ if python_temporary(name, "demo-briefs") => Some(Kind::RecipeFolder(Folder::DemoBriefs)),
        _ if HandoverTarget::manages_temporary(name) => Some(Kind::PrivateFile),
        // wrapper.sh can use state as its result directory. TMPDIR can also
        // point directly at state, just as it points at demo scratch below.
        _ if result_folder(name) => Some(Kind::RecipeFolder(Folder::Result)),
        _ => scratch_child(name),
    }
}

impl Folder {
    pub(super) fn child(self, name: &str) -> Option<Kind> {
        match (self, name) {
            (Self::Worker, "handover" | "brief" | "retry-brief")
            | (Self::Result | Self::WorkerResult, "out" | "err")
            | (Self::WorkerResult, "exec-attempt") => Some(Kind::PrivateFile),
            (Self::Worker, "hooks") => Some(Kind::RecipeFolder(Self::WorkerHooks)),
            (Self::Worker, name) if HandoverTarget::manages_temporary(name) => {
                Some(Kind::PrivateFile)
            }
            (Self::Worker, name) if result_folder(name) => {
                Some(Kind::RecipeFolder(Self::WorkerResult))
            }
            (Self::Demo, "lead.log" | "launcher.log" | "outcome.txt" | "launch") => {
                Some(Kind::PrivateFile)
            }
            (Self::Demo, name) if python_temporary(name, "tmp") => {
                Some(Kind::RecipeFolder(Self::Scratch))
            }
            (Self::Demo, name)
                if numbered(name, "worker", "log")
                    || numbered(name, "task", "md")
                    || python_temporary(name, ".outcome") =>
            {
                Some(Kind::PrivateFile)
            }
            (Self::DemoBriefs, "tmp") => Some(Kind::RecipeFolder(Self::Scratch)),
            (Self::DemoBriefs, name)
                if numbered(name, "task", "md")
                    || numbered(name, "report", "expected")
                    || numbered(name, "report", "started") =>
            {
                Some(Kind::PrivateFile)
            }
            (Self::Launches, name) if launch_record(name) => Some(Kind::PrivateFile),
            (Self::Scratch, name) => scratch_child(name),
            (Self::Extraction, BUNDLE_FOLDER) => Some(Kind::RecipeFolder(Self::Bundle)),
            (Self::Bundle, "__pycache__") => Some(Kind::RecipeFolder(Self::PythonCache)),
            (Self::Bundle, name) if recipe::bundle_file(name) => Some(Kind::PrivateFile),
            (Self::PythonCache, name) if bytecode_file(name) => Some(Kind::TrustedFile),
            (Self::WorkerHooks, name) if hook_file(name) => Some(Kind::PrivateFile),
            (Self::Observe, "startup" | "activity" | "reminder") => Some(Kind::PrivateFile),
            (Self::Observe, name)
                if name
                    .strip_prefix("agent-")
                    .or_else(|| name.strip_prefix("reminder-agent-"))
                    .is_some_and(|id| hex(id, 64)) =>
            {
                Some(Kind::PrivateFile)
            }
            _ => None,
        }
    }

    pub(super) fn private(self) -> bool {
        // Python can create its bytecode cache with group read access.
        !matches!(self, Self::PythonCache)
    }
}

fn scratch_child(name: &str) -> Option<Kind> {
    if native_temporary(name, BUNDLE_TEMP_PREFIX) {
        Some(Kind::RecipeFolder(Folder::Extraction))
    } else if native_temporary(name, WORKER_HOOK_TEMP_PREFIX) {
        Some(Kind::RecipeFolder(Folder::WorkerHooks))
    } else {
        let (uid, session) = name.strip_prefix("malak-observe-")?.split_once('-')?;
        (uid.parse() == Ok(geteuid().as_raw()) && hex(session, 64))
            .then_some(Kind::RecipeFolder(Folder::Observe))
    }
}

fn launch_record(name: &str) -> bool {
    if let Some(worker) = name
        .strip_prefix("name.")
        .and_then(|name| name.strip_suffix(".lock"))
    {
        return worker.parse::<malak::ParticipantName>().is_ok();
    }
    name.split_once('.')
        .is_some_and(|(id, extension)| matches!(extension, "json" | "lock" | "new") && hex(id, 32))
}

fn hook_file(name: &str) -> bool {
    let Some((session, suffix)) = name.split_once('.') else {
        return false;
    };
    hex(session, 64)
        && (matches!(suffix, "json" | "lock")
            || suffix
                .strip_prefix("json.")
                .and_then(|rest| rest.strip_suffix(".tmp"))
                .is_some_and(|nonce| hex(nonce, 32)))
}

// Extracted Python modules can leave bytecode and its atomic staging file
// if a launcher was interrupted before removing its temporary bundle.
fn bytecode_file(name: &str) -> bool {
    let Some((module, suffix)) = name.split_once(".cpython-") else {
        return false;
    };
    let Some((tag, tail)) = suffix.split_once(".pyc") else {
        return false;
    };
    let version = tag.split_once(".opt-").map_or(tag, |(version, opt)| {
        if matches!(opt, "1" | "2") {
            version
        } else {
            ""
        }
    });
    decimal(version)
        && (tail.is_empty() || tail.strip_prefix('.').is_some_and(decimal))
        && recipe::bundle_file(&format!("{module}.py"))
}

fn numbered(name: &str, prefix: &str, extension: &str) -> bool {
    name.split_once('-').is_some_and(|(stem, rest)| {
        stem == prefix
            && rest.split_once('.').is_some_and(|(number, suffix)| {
                matches!(number, "1" | "2" | "3" | "4" | "5") && suffix == extension
            })
    })
}

// tempfile.mkdtemp and NamedTemporaryFile use eight lower-case letters,
// digits or underscores. The shell wrapper and Rust launcher use mkdtemp's
// alphanumeric names. Match the entire component in either case.
fn python_temporary(name: &str, prefix: &str) -> bool {
    temporary(name, prefix, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
    })
}

fn native_temporary(name: &str, prefix: &str) -> bool {
    temporary(name, prefix, |byte| byte.is_ascii_alphanumeric())
}

fn result_folder(name: &str) -> bool {
    temporary(name, "result", |byte| {
        byte.is_ascii_alphanumeric() || byte == b'_'
    })
}

fn temporary(name: &str, prefix: &str, valid: impl Fn(u8) -> bool) -> bool {
    name.strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('.'))
        .is_some_and(|suffix| suffix.len() == 8 && suffix.bytes().all(valid))
}

fn hex(name: &str, length: usize) -> bool {
    name.len() == length
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn decimal(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
}
