use malak::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const PACKAGE: &str = "malak";

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Version(u64, u64, u64);

impl Version {
    pub(super) fn parse(text: &str) -> Result<Self, Error> {
        Self::exact(text).ok_or_else(|| {
            Error::refused(format!("expected a version in X.Y.Z form, found {text:?}"))
        })
    }

    fn exact(text: &str) -> Option<Self> {
        fn number(text: &str) -> Option<u64> {
            if text.is_empty()
                || (text.starts_with('0') && text.len() > 1)
                || !text.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            text.parse().ok()
        }
        let mut parts = text.split('.');
        let version = Self(
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        );
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub(super) enum Source {
    Release(Version),
    Local { path: PathBuf, version: Version },
}

impl Source {
    pub(super) fn find(path: Option<PathBuf>, version: Option<String>) -> Result<Self, Error> {
        if let Some(path) = path {
            let checkout = path.canonicalize().map_err(|error| {
                Error::refused(format!("local checkout {}: {error}", path.display()))
            })?;
            let package = manifest(&checkout.join("crates/malak"))?;
            let package = package
                .get("package")
                .filter(|package| {
                    package.get("name").and_then(toml::Value::as_str) == Some(PACKAGE)
                })
                .ok_or_else(|| Error::refused("the local checkout has no Malak package"))?;
            let version = package
                .get("version")
                .ok_or_else(|| Error::refused("the local Malak package has no version"))?;
            let inherited;
            let version =
                if version.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    inherited = manifest(&checkout)?;
                    inherited
                        .get("workspace")
                        .and_then(|workspace| workspace.get("package"))
                        .and_then(|package| package.get("version"))
                        .and_then(toml::Value::as_str)
                } else {
                    version.as_str()
                }
                .ok_or_else(|| {
                    Error::refused("the local Malak package needs a version in X.Y.Z form")
                })?;
            return Ok(Self::Local {
                path: checkout,
                version: Version::parse(version)?,
            });
        }
        version
            .map_or_else(latest_release, |version| Version::parse(&version))
            .map(Self::Release)
    }

    pub(super) fn version(&self) -> Version {
        match self {
            Self::Release(version) | Self::Local { version, .. } => *version,
        }
    }

    pub(super) fn local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    pub(super) fn lead_changes(&self) -> Result<Option<String>, Error> {
        let Self::Local { path, .. } = self else {
            return Ok(None);
        };
        let path = path.join("docs/LEAD-CHANGES.md");
        match fs::read_to_string(&path) {
            Ok(changes) => Ok(Some(changes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::unavailable(format!(
                "could not read {}: {error}",
                path.display()
            ))),
        }
    }

    pub(super) fn description(&self) -> String {
        match self {
            Self::Release(version) => format!("{REPOSITORY} (tag v{version})"),
            Self::Local { path, .. } => format!("local checkout at {}", path.display()),
        }
    }

    pub(super) fn install(&self, root: &Path, cargo: &Path) -> Result<(), Error> {
        let mut command = Command::new(cargo);
        command
            .env("CARGO_INSTALL_ROOT", root)
            .args(["install", "--locked", "--force", "--bin", PACKAGE]);
        match self {
            Self::Release(version) => {
                command.args([
                    "--git",
                    REPOSITORY,
                    "--tag",
                    &format!("v{version}"),
                    PACKAGE,
                ]);
            }
            Self::Local { path, .. } => {
                command.arg("--path").arg(path.join("crates/malak"));
            }
        }
        let status = command.status().map_err(|error| {
            Error::unavailable(format!(
                "could not run Cargo at {}: {error}",
                cargo.display()
            ))
        })?;
        if !status.success() {
            return Err(Error::unavailable(format!(
                "cargo install failed ({status})"
            )));
        }
        Ok(())
    }
}

fn manifest(folder: &Path) -> Result<toml::Value, Error> {
    let path = folder.join("Cargo.toml");
    let contents = fs::read_to_string(&path)
        .map_err(|error| Error::refused(format!("could not read {}: {error}", path.display())))?;
    toml::from_str(&contents)
        .map_err(|error| Error::refused(format!("could not parse {}: {error}", path.display())))
}

fn latest_release() -> Result<Version, Error> {
    let result = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", REPOSITORY, "v*"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| Error::unavailable(format!("could not run git ls-remote: {error}")))?;
    if !result.status.success() {
        return Err(Error::unavailable(format!(
            "git ls-remote failed ({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim(),
        )));
    }
    let refs = String::from_utf8_lossy(&result.stdout);
    refs.lines()
        .filter_map(|line| Version::exact(line.split_once("\trefs/tags/v")?.1))
        .max()
        .ok_or_else(|| Error::unavailable(format!("no vX.Y.Z release tags found at {REPOSITORY}")))
}
