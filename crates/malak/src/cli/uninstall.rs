use super::{board_group, board_list, board_names, config, output, recipe};
use clap::Args;
use malak::Error;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

mod binary;
mod data;
mod inventory;
mod layout;
mod recipe_data;

#[derive(Args)]
pub(super) struct Options {
    /// Delete board data without a terminal, or preview its deletion with --dry-run.
    #[arg(long)]
    delete_data: bool,
    /// Confirm uninstall when running without a terminal.
    #[arg(long)]
    yes: bool,
    /// Show what would happen without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// List individual handovers and managed paths in the deletion plan.
    #[arg(long)]
    verbose: bool,
}

pub(super) fn run(explicit: Option<PathBuf>, options: Options) -> Result<ExitCode, Error> {
    let interactive = !options.dry_run && io::stdin().is_terminal();
    if interactive {
        if !confirm("Uninstall Malak?")? {
            output::result("Uninstall cancelled.")?;
            return Ok(ExitCode::SUCCESS);
        }
    } else if !options.dry_run && !options.yes {
        return Err(Error::refused(
            "uninstall requires --yes without a terminal. Run malak uninstall --yes to keep the data, or add --delete-data to remove it",
        ));
    }
    let root = board_list::directory()?;
    let exe = std::env::current_exe().map_err(config::io_error)?;
    let list = if interactive || options.delete_data {
        board_list::lock_for_deletion(&root, options.dry_run).map_err(config::io_error)
    } else {
        Ok(None)
    };
    let boards = board_names::discover(&root, explicit);
    let configs = || boards.iter().flat_map(board_names::LocalBoard::configs);
    let external = inventory::external_handover(&boards);
    let external = external.as_deref();
    let delete_data = if interactive {
        for board in &boards {
            inventory::show(board, options.verbose, None, None)?;
        }
        let preview = list
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|list| data::Plan::prepare(configs(), &root, &exe, list.is_some(), true));
        match preview {
            Ok(plan) => plan.show(options.verbose, external)?,
            Err(error) => output::details(
                "Could not prepare data deletion.",
                &[("Reason", &error.to_string())],
            )?,
        }
        confirm("Also delete board data?")?
    } else {
        options.delete_data
    };
    board_group::stop(&boards, options.dry_run, |board, status, detail| {
        if interactive {
            let name = board_names::name(board.primary());
            if let Some(("Reason", reason)) = detail {
                output::details(&format!("{status} {name}"), &[("Reason", reason)])
            } else {
                output::result(&format!("{status} {name}"))
            }
        } else {
            inventory::show(board, options.verbose, Some(status), detail)
        }
    })?;
    if let Some(home) = std::env::home_dir() {
        recipe::remove_agents(&home, options.dry_run)?;
    }

    if delete_data {
        let list = list?;
        let data = data::Plan::prepare(configs(), &root, &exe, list.is_some(), options.dry_run)?;
        if options.dry_run || !interactive {
            data.show(options.verbose, external)?;
        }
        if !options.dry_run {
            data.delete(options.verbose, external)?;
        }
    } else {
        let list_path = board_list::path(&root);
        if list_path.symlink_metadata().is_ok() {
            output::details("Board list", &[("Path", &list_path.to_string_lossy())])?;
        }
        if let Some(path) = external {
            output::result("Will keep")?;
            data::show_external(path)?;
        }
        output::result(if options.dry_run {
            "Would keep the data. To delete it, answer yes to the data prompt or use malak uninstall --yes --delete-data."
        } else {
            "Data kept. To delete it later, answer yes to the data prompt, run malak uninstall --yes --delete-data without a terminal, or remove the listed paths by hand."
        })?;
    }
    if !boards.is_empty() {
        output::result("Find and remove any other handovers written with --out by hand.")?;
    }
    binary::remove(&exe, options.dry_run)
}

fn confirm(question: &str) -> Result<bool, Error> {
    output::write(&format!("{question} [y/N] "))?;
    io::stdout().flush().map_err(config::io_error)?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(config::io_error)?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}
