use super::recipe;
use clap::{Arg, ArgAction, Command};
use std::ffi::OsString;
use std::fmt::Write as _;

/// Route a bare help word before parsing can treat it as a command argument.
pub(super) fn bare_help(root: &Command, argv: &[OsString]) -> Option<Vec<OsString>> {
    if !argv.iter().skip(1).any(|word| word == "help") {
        return None;
    }
    let (program, mut words) = argv.split_first()?;
    let mut path = vec![program.clone()];
    let mut command = root;
    while let Some((word, rest)) = words.split_first() {
        if word == "help" {
            // Also accept the conventional leading form: malak help board start.
            let subcommand_path = command.has_subcommands();
            for word in rest {
                if let Some(child) = command.find_subcommand(word) {
                    command = child;
                    path.push(word.clone());
                    continue;
                }
                if command.get_name() == "recipe" && recipe::is_shipped(word) {
                    path.push(word.clone());
                } else if subcommand_path
                    && !matches!(word.to_str(), Some("help" | "-h" | "--help"))
                {
                    // Preserve Clap's diagnostics for an unknown help path.
                    return None;
                }
                break;
            }
            path.push("--help".into());
            return Some(path);
        }
        if word == "--"
            || word == "-"
            || word == "-h"
            || word == "--help"
            || word == "--version"
            || word == "-V"
        {
            return None;
        }
        if let Some(long) = word.as_encoded_bytes().strip_prefix(b"--") {
            let mut parts = long.splitn(2, |byte| *byte == b'=');
            let name = parts.next()?;
            let option = command
                .get_arguments()
                .chain(root.get_arguments().filter(|arg| arg.is_global_set()))
                .find(|arg| arg.get_long().is_some_and(|long| long.as_bytes() == name))?;
            let values = option.get_num_args().map_or_else(
                || usize::from(option.get_action().takes_values()),
                |range| range.min_values(),
            );
            words = rest.get(values.saturating_sub(usize::from(parts.next().is_some()))..)?;
            continue;
        }
        command = command.find_subcommand(word)?;
        path.push(word.clone());
        words = rest;
    }
    None
}

pub(super) fn configure(root: Command) -> Command {
    let globals: Vec<_> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(usage_argument)
        .collect();
    configure_command(root, "", &globals)
}

fn configure_command(command: Command, parent: &str, globals: &[String]) -> Command {
    let name = command.get_name();
    let path = if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent} {name}")
    };
    let mut usage = path.clone();
    for flag in globals {
        let _ = write!(usage, " {flag}");
    }
    for flag in command
        .get_arguments()
        .filter(|arg| !arg.is_global_set() && !arg.is_positional() && !arg.is_hide_set())
    {
        let _ = write!(usage, " {}", usage_argument(flag));
    }
    if command.has_subcommands() {
        usage.push_str(" COMMAND [ARGS]");
    } else {
        for arg in command.get_positionals().filter(|arg| !arg.is_hide_set()) {
            let _ = write!(usage, " {}", usage_argument(arg));
        }
    }
    command
        .override_usage(usage)
        .disable_help_flag(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::HelpLong)
                .help("Print help"),
        )
        .mut_subcommands(|child| configure_command(child, &path, globals))
}

fn usage_argument(arg: &Arg) -> String {
    let mut usage = String::new();
    if !arg.is_required_set() {
        usage.push('[');
    }
    if let Some(long) = arg.get_long() {
        let _ = write!(usage, "--{long}");
    } else if let Some(short) = arg.get_short() {
        let _ = write!(usage, "-{short}");
    }
    if arg.is_positional() || arg.get_action().takes_values() {
        if !arg.is_positional() {
            usage.push(' ');
        }
        if let Some(names) = arg.get_value_names() {
            for (index, name) in names.iter().enumerate() {
                if index > 0 {
                    usage.push(' ');
                }
                usage.push_str(name.as_str());
            }
        } else {
            usage.push_str(&arg.get_id().as_str().to_ascii_uppercase());
        }
        if arg.is_positional()
            && !arg.is_trailing_var_arg_set()
            && matches!(arg.get_action(), ArgAction::Append)
        {
            usage.push_str("...");
        }
    }
    if !arg.is_required_set() {
        usage.push(']');
    }
    if !arg.is_positional()
        && matches!(arg.get_action(), ArgAction::Append)
        && arg
            .get_num_args()
            .is_none_or(|range| range.max_values() == 1)
    {
        usage.push_str("...");
    }
    usage
}
