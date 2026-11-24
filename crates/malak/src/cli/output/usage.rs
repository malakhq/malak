//! Command help and diagnostic formatting.

use super::{Format, write, write_diagnostic};
use crate::cli::config::{START_HINT, io_error};
use malak::Error;

pub(in crate::cli) fn error(operation: &str, reason: &str, next: &str) {
    operation_error(operation, reason, next, false);
}

pub(in crate::cli) fn operation_error(operation: &str, reason: &str, next: &str, legacy: bool) {
    let mut detail = [("Reason", reason), ("Next", next)];
    if let Some((reason, hint)) = existing_next_step(reason) {
        detail = [("Reason", reason), ("Next", hint)];
    }
    let title = format!("Error: Could not {operation}.");
    let format = Format::stderr(legacy);
    if legacy {
        write_diagnostic(&format.details(&title, &detail));
    } else {
        write_diagnostic(&format.screen(&title, "Details", &detail));
    }
}

fn existing_next_step(reason: &str) -> Option<(&str, &'static str)> {
    let hints = [
        (START_HINT, "Run malak board start to start the board."),
        (
            "check the --config path",
            "Check that --config names an existing configuration file.",
        ),
        (
            "check that MALAK_HANDOVER names your handover file",
            "Check that MALAK_HANDOVER names your handover file.",
        ),
        (
            "check with malak board status",
            "Run malak board status to check whether the board is ready.",
        ),
        (
            "Pass --creation to ack without a read.",
            "Pass --creation with the creation ID from the original message.",
        ),
    ];
    for (suffix, hint) in hints {
        if let Some((reason, _)) = reason.rsplit_once("; ").filter(|(_, last)| *last == suffix) {
            return Some((reason, hint));
        }
        if let Some(reason) = reason
            .strip_suffix(suffix)
            .and_then(|part| part.strip_suffix(". "))
        {
            return Some((reason, hint));
        }
    }
    None
}

pub(in crate::cli) fn usage_error(error: &clap::Error, legacy: bool) {
    use clap::error::ContextKind as Key;
    if error.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
        let format = Format::stderr(legacy);
        let mut block = format.details("Error: Choose a Malak command.", &[]);
        let help = error.render().to_string();
        if !legacy && format.interactive {
            format.help_sections(&mut block, help.trim());
            format.next(&mut block, "malak COMMAND --help");
        } else {
            format.paragraph(&mut block, "  ", help.trim());
        }
        write_diagnostic(&block);
        return;
    }
    let mut values = Vec::new();
    for (label, key) in [
        ("Command", Key::InvalidSubcommand),
        ("Argument", Key::InvalidArg),
        ("Value", Key::InvalidValue),
        ("Conflicts with", Key::PriorArg),
        ("Choices", Key::ValidValue),
        ("Expected values", Key::ExpectedNumValues),
        ("Suggestion", Key::SuggestedSubcommand),
        ("Suggested argument", Key::SuggestedArg),
        ("Suggested value", Key::SuggestedValue),
        ("Usage", Key::Usage),
    ] {
        if let Some(value) = error.get(key) {
            let value = value.to_string();
            values.push((
                label,
                value
                    .trim()
                    .strip_prefix("Usage: ")
                    .unwrap_or(value.trim())
                    .to_owned(),
            ));
        }
    }
    let mut fields = vec![(
        "Reason",
        error.kind().as_str().unwrap_or("invalid command line"),
    )];
    fields.extend(values.iter().map(|(label, value)| (*label, value.as_str())));
    fields.push(("Next", "Use --help with this command for usage."));
    let format = Format::stderr(legacy);
    let title = "Error: Could not run the command.";
    if legacy {
        write_diagnostic(&format.details(title, &fields));
    } else {
        write_diagnostic(&format.screen(title, "Details", &fields));
    }
}

pub(in crate::cli) fn help(help: &clap::Error) -> Result<(), Error> {
    let format = Format::stdout();
    if !format.interactive {
        return help.print().map_err(io_error);
    }
    let mut block = format.details("Malak help", &[]);
    let text = help.render().to_string();
    let text = text.trim();
    if let Some((usage, example)) = text.split_once("\nExample:\n") {
        format.help_sections(&mut block, usage);
        format.section(&mut block, "Example");
        block.push_str(example);
        block.push('\n');
    } else {
        format.help_sections(&mut block, text);
    }
    write(&block)
}

impl Format {
    fn help_sections(&self, out: &mut String, help: &str) {
        for line in help.lines().filter(|line| !line.is_empty()) {
            if let Some(usage) = line.strip_prefix("Usage: ") {
                self.section(out, "Usage");
                self.paragraph(out, "  ", usage);
            } else if let Some(section @ ("Commands" | "Arguments" | "Options")) =
                line.strip_suffix(':')
            {
                self.section(out, section);
            } else {
                self.paragraph(out, "  ", line.trim_start());
            }
        }
    }
}
