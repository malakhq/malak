use super::board::{GENERAL_FOLDER, LEAD_FOLDER, OWNER_FOLDER, SOCKET_FILE};
use malak_fs::{Directory, check_user_permissions, trusted_parent};
use nix::fcntl::OFlag;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{File, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");
const LEGACY: &str = "unknown (older than 0.1.0)";
pub(super) const RECORD: &str = "version";
const MAX_RECORD_BYTES: u64 = 256;
static WARNED: AtomicBool = AtomicBool::new(false);
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
static EXECUTABLE: OnceLock<Option<(File, String)>> = OnceLock::new();
static BUILD: OnceLock<Option<String>> = OnceLock::new();

pub(super) fn set_config_path(path: PathBuf) {
    let _ = CONFIG_PATH.set(path);
}

#[derive(Default)]
pub(super) enum BoardVersion {
    Recorded {
        version: String,
        build: Option<String>,
        same_executable: bool,
    },
    Legacy,
    #[default]
    Unavailable,
}

/// Publish the version and executable digest before serving, and keep the
/// record locked for the run.
/// The socket directory lock already serializes board startup.
pub(super) fn record(directory: &Directory, uid: u32) -> io::Result<File> {
    let (executable, stamp) = executable()
        .ok_or_else(|| io::Error::other("could not open the running Malak executable"))?;
    let build = digest(executable)?;
    let mut file = directory.open_file(RECORD, OFlag::O_CREAT | OFlag::O_RDWR)?;
    check_user_permissions(&file, uid, true)?;
    file.lock()?;
    file.set_len(0)?;
    writeln!(file, "{} {VERSION} {build} {stamp}", std::process::id())?;
    Ok(file)
}

fn executable() -> Option<&'static (File, String)> {
    EXECUTABLE
        .get_or_init(|| {
            let file = File::open(std::env::current_exe().ok()?).ok()?;
            let meta = file.metadata().ok()?;
            let stamp = format!(
                "{:x}.{:x}.{:x}.{:x}.{:x}.{:x}.{:x}",
                meta.dev(),
                meta.ino(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.ctime(),
                meta.ctime_nsec(),
            );
            Some((file, stamp))
        })
        .as_ref()
}

fn digest(mut file: &File) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let mut build = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(build, "{byte:02x}").expect("writing to a string");
    }
    Ok(build)
}

pub(super) fn build_at(path: &Path) -> io::Result<String> {
    digest(&File::open(path)?)
}

impl BoardVersion {
    pub(super) fn read(address: &str, uid: u32, pid: u32) -> Self {
        Self::read_record(address, uid, pid).unwrap_or_default()
    }

    fn read_record(address: &str, uid: u32, pid: u32) -> io::Result<Self> {
        let socket = Path::new(
            address
                .strip_prefix("unix:")
                .ok_or(io::ErrorKind::InvalidInput)?,
        );
        let door = socket.parent().ok_or(io::ErrorKind::InvalidInput)?;
        if socket.file_name().and_then(|name| name.to_str()) != Some(SOCKET_FILE)
            || !matches!(
                door.file_name().and_then(|name| name.to_str()),
                Some(OWNER_FOLDER | LEAD_FOLDER | GENERAL_FOLDER)
            )
        {
            return Ok(Self::Unavailable);
        }
        let root = door.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let path = root.join(OWNER_FOLDER).join(RECORD);
        let (directory, _) = trusted_parent(&path, uid)?;
        check_user_permissions(directory.file(), uid, true)?;
        let file = match directory.open_file(RECORD, OFlag::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Legacy),
            Err(error) => return Err(error),
        };
        check_user_permissions(&file, uid, true)?;
        match file.try_lock_shared() {
            // An unlocked record can be left behind by a stopped or killed build.
            Ok(()) => return Ok(Self::Legacy),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let mut contents = String::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_string(&mut contents)?;
        if contents.len() as u64 > MAX_RECORD_BYTES {
            return Ok(Self::Unavailable);
        }
        let Some(line) = contents.strip_suffix('\n') else {
            return Ok(Self::Unavailable);
        };
        let mut fields = line.split(' ');
        let (Some(recorded_pid), Some(version)) = (fields.next(), fields.next()) else {
            return Ok(Self::Unavailable);
        };
        if recorded_pid.parse::<u32>() != Ok(pid)
            || version.is_empty()
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
        {
            return Ok(Self::Unavailable);
        }
        let (build, same_executable) = match (fields.next(), fields.next(), fields.next()) {
            (None, None, None) => (None, false),
            (Some(build), Some(stamp), None)
                if build.len() == 64 && build.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                // Avoid reading the whole binary on every call to the same
                // installation. A copied executable can still match by digest.
                (
                    Some(build.into()),
                    executable().is_some_and(|(_, own)| own == stamp),
                )
            }
            _ => return Ok(Self::Unavailable),
        };
        Ok(Self::Recorded {
            version: version.into(),
            build,
            same_executable,
        })
    }

    pub(super) fn recorded(&self) -> Option<&str> {
        match self {
            Self::Recorded { version, .. } => Some(version),
            Self::Legacy | Self::Unavailable => None,
        }
    }

    pub(super) fn build(&self) -> Option<&str> {
        match self {
            Self::Recorded { build, .. } => build.as_deref(),
            Self::Legacy | Self::Unavailable => None,
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Recorded { version, .. } => version,
            Self::Legacy => LEGACY,
            Self::Unavailable => "unknown",
        }
    }

    pub(super) fn warn(&self) {
        if WARNED.load(Ordering::Relaxed) {
            return;
        }
        let Some(mismatch) = self.mismatch() else {
            return;
        };
        if WARNED.swap(true, Ordering::Relaxed) {
            return;
        }
        let config = CONFIG_PATH.get().map_or_else(String::new, |path| {
            format!(
                " --config '{}'",
                path.to_string_lossy().replace('\'', "'\\''")
            )
        });
        let _ = writeln!(
            io::stderr().lock(),
            "Warning: running board {mismatch}. Restart with `malak{config} board stop` then `malak{config} board start`.",
        );
    }

    pub(super) fn mismatch(&self) -> Option<String> {
        if matches!(self, Self::Unavailable) {
            return None;
        }
        let mismatch = if self.recorded() != Some(VERSION) {
            format!(
                "version {} differs from this malak ({VERSION})",
                self.label()
            )
        } else {
            if matches!(
                self,
                Self::Recorded {
                    same_executable: true,
                    ..
                }
            ) {
                return None;
            }
            let local_build = BUILD
                .get_or_init(|| digest(&executable()?.0).ok())
                .as_deref();
            if self.build().is_some() && (local_build.is_none() || self.build() == local_build) {
                return None;
            }
            format!(
                "build {} differs from this malak ({})",
                self.build().unwrap_or("unknown"),
                local_build.unwrap_or("unknown")
            )
        };
        Some(mismatch)
    }
}
