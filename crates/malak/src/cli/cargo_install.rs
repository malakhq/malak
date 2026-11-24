use nix::unistd::{AccessFlags, access};
use std::path::{Path, PathBuf};

pub(super) fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

pub(super) fn cargo_for(exe: &Path, path: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")));
    let (root, bin) = std::env::var_os("CARGO_INSTALL_ROOT")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .chain(cargo_home)
        .find_map(|root| {
            let bin = root.join("bin").canonicalize().ok()?;
            (exe.parent() == Some(bin.as_path())).then_some((root, bin))
        })?;
    // Keep the cargo filename when executing a rustup proxy.
    let cargo = std::iter::once(bin.join("cargo"))
        .chain(path.iter().map(|folder| folder.join("cargo")))
        .find(|path| path.is_file() && access(path, AccessFlags::X_OK).is_ok())?;
    Some((
        std::path::absolute(root).ok()?,
        std::path::absolute(cargo).ok()?,
    ))
}
