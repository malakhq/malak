use super::super::{cargo_install, output};
use malak::Error;
use malak_fs::trusted_parent;
use nix::errno::Errno;
use nix::fcntl::readlinkat;
use nix::unistd::{UnlinkatFlags, geteuid, unlinkat};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub(super) fn remove(exe: &Path, dry_run: bool) -> Result<ExitCode, Error> {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let path = cargo_install::search_path();
    let links = path_links(&exe, &path);
    let Some((root, cargo)) = cargo_install::cargo_for(&exe, &path) else {
        manual(Some(&exe), &links)?;
        return Ok(ExitCode::SUCCESS);
    };
    output::details(
        if dry_run {
            "Would run cargo uninstall malak."
        } else {
            "Running cargo uninstall malak."
        },
        &[("Binary", &exe.to_string_lossy())],
    )?;
    if dry_run {
        if !links.is_empty() {
            output::result("Would try to remove these PATH symlinks after removing the binary.")?;
            for link in links {
                output::line(format_args!(
                    "  {}",
                    output::display(&link.path.to_string_lossy())
                ))?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let failed = match Command::new(cargo)
        .args(["uninstall", "malak"])
        .env("CARGO_INSTALL_ROOT", root)
        .status()
    {
        Ok(status) if status.success() => false,
        Ok(status) => {
            output::details(
                "Cargo uninstall failed.",
                &[("Reason", &status.to_string())],
            )?;
            true
        }
        Err(error) => {
            output::details(
                "Could not run Cargo uninstall.",
                &[("Reason", &error.to_string())],
            )?;
            true
        }
    };

    if !exe
        .symlink_metadata()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        if !failed {
            output::result("Cargo left the Malak binary in place.")?;
        }
        manual(Some(&exe), &links)?;
        return Ok(ExitCode::from(3));
    }

    output::result("Malak binary removed.")?;
    let left: Vec<_> = links
        .into_iter()
        .filter(|link| {
            link.remove()
                .is_err_and(|error| error.kind() != io::ErrorKind::NotFound)
        })
        .collect();
    if !left.is_empty() {
        manual(None, &left)?;
    }
    Ok(if failed {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    })
}

struct PathLink {
    path: PathBuf,
    target: OsString,
}

impl PathLink {
    fn remove(&self) -> io::Result<()> {
        let (parent, name) = trusted_parent(&self.path, geteuid().as_raw())?;
        match readlinkat(&parent, name) {
            Ok(target) if target == self.target => {
                unlinkat(&parent, name, UnlinkatFlags::NoRemoveDir)?
            }
            Ok(_) | Err(Errno::EINVAL | Errno::ENOENT) => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

fn path_links(exe: &Path, path: &[PathBuf]) -> Vec<PathLink> {
    let folders: BTreeSet<_> = path
        .iter()
        .filter_map(|folder| folder.join(".").canonicalize().ok())
        .collect();
    let mut links = Vec::new();
    for folder in folders {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            let path = entry.path();
            if let Ok(target) = path.read_link()
                && path.canonicalize().is_ok_and(|target| target == exe)
            {
                links.push(PathLink {
                    path,
                    target: target.into_os_string(),
                });
            }
        }
    }
    links.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    links
}

fn manual(exe: Option<&Path>, links: &[PathLink]) -> Result<(), Error> {
    output::line(format_args!(""))?;
    output::result(match (exe.is_some(), links.is_empty()) {
        (true, true) => "Remove the Malak binary by hand.",
        (true, false) => {
            "Remove the Malak binary by hand. Its PATH symlinks will be broken after removal."
        }
        (false, _) => "Remove these broken Malak PATH symlinks by hand.",
    })?;
    let arguments = exe
        .into_iter()
        .chain(links.iter().map(|link| link.path.as_path()))
        .map(shell_word)
        .collect::<Vec<_>>();
    output::line(format_args!("rm -- {}", arguments.join(" ")))
}

fn shell_word(path: &Path) -> String {
    if let Some(text) = path.to_str()
        && output::display(text) == text
    {
        return format!("'{}'", text.replace('\'', "'\\''"));
    }
    // Quote unusual Unix filenames byte for byte without printing control sequences.
    use std::fmt::Write as _;
    let mut word = String::from("$'");
    for byte in path.as_os_str().as_bytes() {
        write!(word, "\\{byte:03o}").unwrap();
    }
    word.push('\'');
    word
}
