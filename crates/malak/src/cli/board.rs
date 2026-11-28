use super::config::{Board, io_error, owned_directory, private_directory};
use super::{board_version, client_transport::ClientTransport, output, owner_commands, user_id};
use malak::Error;
use malak::server::{Entrance, Server};
use malak_core::{LocalClient, LocalStartup, OwnerCredential};
use malak_fs::{
    AccountAccess, Directory, check_user_permissions, share_with, trusted_parent,
    trusted_parent_for_account,
};
use malak_mechanism_key::Key;
use malak_transport_unix::UnixTransport;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
use nix::sys::stat::{FchmodatFlags, Mode, fchmodat};
use nix::unistd::geteuid;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Channels every board starts with. Participants' inbox and relay channels
/// use `to/<id>` and `relay/<name>`.
const STANDARD_CHANNELS: [&str; 6] = ["lead", "questions", "orders", "rail", "briefs", "calls"];
pub(super) const LOG_FILE: &str = "board.log";
const READY_STEP: &str = "ready";
const STARTUP_POLL: Duration = Duration::from_millis(25);
pub(super) const OWNER_FOLDER: &str = "o";
pub(super) const LEAD_FOLDER: &str = "l";
pub(super) const GENERAL_FOLDER: &str = "p";
pub(super) const SOCKET_FILE: &str = "s";
const OWNER_CONNECTIONS: usize = 8;
const OWNER_DESCRIPTORS: usize = 1 + OWNER_CONNECTIONS * 3;
const LEAD_CONNECTIONS: usize = 16 + 16;
const GENERAL_CONNECTIONS: usize = 64 + 1024;
// Remaining listeners, one newly accepted descriptor per door, and board files.
const BOARD_DESCRIPTORS: usize = 32;

/// How long `--detach` waits for the board to be ready, and whether it
/// shows the board's startup steps meanwhile.
pub(super) struct Wait {
    pub limit: Duration,
    pub verbose: bool,
}

/// Start the board in the background and return once it is ready, or at once
/// if the existing board is serving. The board's log is
/// `board.log` in its state folder, and its path is printed.
pub(super) fn detach(
    config_path: &Path,
    config: Board,
    lead_handover: Option<PathBuf>,
    lead_account: Option<String>,
    wait: Wait,
) -> Result<(), Error> {
    let log = config.state.join(LOG_FILE);
    let deadline = Instant::now() + wait.limit;
    if wait_for_existing_board(&config, deadline, &log)? {
        return ready_screen("Board already running.", "Log", &log);
    }
    let state = owned_directory(&config.state)?;
    let output = state
        .open_file(LOG_FILE, OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_APPEND)
        .and_then(|file| {
            check_user_permissions(&file, geteuid().as_raw(), true)?;
            Ok(file)
        })
        .map_err(|error| Error::refused(format!("log {}: {error}", log.display())))?;
    let shown = output.metadata().map_err(io_error)?.len();
    let mut command = Command::new(std::env::current_exe().map_err(io_error)?);
    // Keep the same configuration even when the caller used the default.
    command
        .arg("--config")
        .arg(config_path)
        .args(["board", "start"]);
    if let Some(path) = &lead_handover {
        command.arg("--lead-handover").arg(path);
    }
    if let Some(account) = &lead_account {
        command.arg("--lead-account").arg(account);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(output.try_clone().map_err(io_error)?)
        .process_group(0)
        .spawn()
        .map_err(io_error)?;
    let stdout = child.stdout.take().expect("child stdout is piped");
    let (signal, ready) = mpsc::channel();
    thread::Builder::new()
        .spawn(move || {
            let line = BufReader::new(stdout).lines().next();
            if let Some(Ok(line)) = line
                && line == READY_STEP
            {
                let _ = signal.send(());
            }
        })
        .map_err(io_error)?;
    let mut readiness_closed = false;
    wait_until_ready(
        |pause| {
            let ready = if readiness_closed {
                thread::sleep(pause);
                false
            } else {
                match ready.recv_timeout(pause) {
                    Ok(()) => true,
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        readiness_closed = true;
                        false
                    }
                }
            };
            (ready, child.try_wait())
        },
        &log,
        &output,
        shown,
        deadline,
        wait.verbose,
    )
}

fn wait_for_existing_board(config: &Board, deadline: Instant, log: &Path) -> Result<bool, Error> {
    let folder = owned_directory(&config.socket_folder)?;
    let mut probe: Option<mpsc::Receiver<bool>> = None;
    loop {
        match folder.file().try_lock() {
            Ok(()) => return Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(io_error(error)),
        }
        if let Some(reply) = &probe {
            match reply.try_recv() {
                Ok(true) => return Ok(true),
                Ok(false) | Err(mpsc::TryRecvError::Disconnected) => probe = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::unavailable(format!(
                "the socket folder is still locked and the board did not confirm it is serving. See {} and check with malak board status",
                log.display()
            )));
        }
        if probe.is_none()
            && let Ok(credential) = OwnerCredential::read(&config.state)
        {
            check_owner_address(config, &credential)?;
            let (send, receive) = mpsc::channel();
            thread::Builder::new()
                .spawn(move || {
                    let serving = ClientTransport::connect(|transport| {
                        LocalClient::owner(transport, credential)
                    })
                    .and_then(|(mut client, _)| owner_commands::ping(&mut client))
                    .is_ok();
                    let _ = send.send(serving);
                })
                .map_err(io_error)?;
            probe = Some(receive);
        }
        thread::sleep(STARTUP_POLL.min(left));
    }
}

pub(super) fn check_owner_address(
    config: &Board,
    credential: &OwnerCredential,
) -> Result<(), Error> {
    let socket = config.socket_folder.join(OWNER_FOLDER).join(SOCKET_FILE);
    if credential.address().strip_prefix("unix:") != socket.to_str() {
        return Err(Error::refused(format!(
            "owner credential {} belongs to a different board at {}. The configured owner socket is {}. Use a matching --config. To move or restore, stop the source board and keep its original socket_folder. For a separate board, use a fresh state folder",
            config.state.join(OwnerCredential::FILE_NAME).display(),
            credential.address(),
            socket.display()
        )));
    }
    Ok(())
}

/// Read the child's readiness signal and follow its startup log until the
/// child stops or the wait ends. A child still starting is left running.
fn wait_until_ready(
    mut poll: impl FnMut(Duration) -> (bool, io::Result<Option<ExitStatus>>),
    log: &Path,
    output: &File,
    mut shown: u64,
    deadline: Instant,
    verbose: bool,
) -> Result<(), Error> {
    let mut last_line = None;
    let mut last_reason = None;
    let mut pause = Duration::ZERO;
    loop {
        let (ready, exited) = poll(pause);
        let exited = exited.map_err(io_error)?;
        let new = new_log_lines(output, &mut shown)?;
        if verbose {
            for step in &new {
                output::diagnostic(&format!("Startup: {step}"), &[]);
            }
        }
        for step in new {
            if let Some(reason) = step.strip_prefix("  Reason  ") {
                last_reason = Some(reason.to_owned());
            }
            last_line = Some(step);
        }
        if let Some(status) = exited {
            let reason = last_reason
                .or(last_line)
                .unwrap_or_else(|| "board did not start".into());
            return Err(match status.code() {
                Some(2) => Error::refused(reason),
                Some(4) => Error::not_supported(reason),
                _ => Error::unavailable(reason),
            });
        }
        if ready {
            return ready_screen("Board ready.", "Log", log);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::unavailable(format!(
                "still starting, see {}; check with malak board status",
                log.display()
            )));
        }
        pause = STARTUP_POLL.min(left);
    }
}

/// Complete lines appended to the log since `shown` bytes.
fn new_log_lines(file: &File, shown: &mut u64) -> Result<Vec<String>, Error> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        // The child appends through a duplicate descriptor. Positional reads
        // keep its writes from changing where the parent reads next.
        let read = file
            .read_at(&mut buffer, *shown + bytes.len() as u64)
            .map_err(io_error)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let complete = bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |end| end + 1);
    *shown += complete as u64;
    Ok(String::from_utf8_lossy(&bytes[..complete])
        .lines()
        .map(String::from)
        .collect())
}

/// Record a startup step in the board's log, which is standard error.
fn step(text: &str) {
    if text == READY_STEP {
        eprintln!("{READY_STEP}");
    } else {
        output::diagnostic(text, &[]);
    }
}

pub(super) fn start(
    config: Board,
    lead_handover: Option<PathBuf>,
    lead_account: Option<String>,
) -> Result<(), Error> {
    super::org::calls::settings::validate_start(&config)?;
    let board_uid = geteuid().as_raw();
    let lead_uid = lead_account
        .as_deref()
        .or(config.lead_account.as_deref())
        .map(user_id)
        .transpose()?
        .unwrap_or(board_uid);
    let limits = config.limits()?;
    let capacities = capacities(limits.open_data_requests)?;
    trusted_parent(&config.state, board_uid).map_err(io_error)?;
    step("state folder checked");
    let socket_folder = owned_directory(&config.socket_folder)?;
    match socket_folder.file().try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(Error::refused("socket folder is in use"));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(io_error(error)),
    }
    step("socket folder locked");
    public_search(&socket_folder)?;
    let owner = Door::prepare(
        &config.socket_folder,
        OWNER_FOLDER,
        Entrance::Owner,
        OWNER_CONNECTIONS,
        Access::Private,
    )?;
    let lead_access = if board_uid == lead_uid {
        Access::Private
    } else {
        Access::Named(lead_uid)
    };
    let lead = Door::prepare(
        &config.socket_folder,
        LEAD_FOLDER,
        Entrance::Lead,
        capacities.0,
        lead_access,
    )?;
    let general = Door::prepare(
        &config.socket_folder,
        GENERAL_FOLDER,
        Entrance::General,
        capacities.1,
        Access::Public,
    )?;
    step("entrance sockets prepared");
    let lead_handover = lead_handover.unwrap_or_else(|| config.lead_handover());
    let (board_owner, server) = Server::create(
        config.store(),
        limits,
        &config.state,
        LocalStartup {
            owner_address: owner.address.clone(),
            lead_address: lead.address.clone(),
            general_address: general.address.clone(),
            lead_handover: lead_handover.clone(),
            mechanism_name: "key".into(),
        },
        Arc::new(Key),
        lead_uid,
    )?;
    board_owner.set_upgrade_changelog(super::upgrade::CHANGELOG);
    step("store opened");
    for name in STANDARD_CHANNELS {
        board_owner.create_channel(name)?;
    }
    let launcher_active = |id: &malak::ParticipantId| {
        super::worker_launch::open(&config.state)
            .ok()
            .flatten()
            .is_some_and(|dir| super::worker_launch::live_claim(&dir, id).unwrap_or(false))
    };
    let _ = server.entrances().refresh_claims(launcher_active);
    let _version_record = board_version::record(&owner.directory, board_uid).map_err(io_error)?;
    let _owner_socket = owner.listen(&server)?;
    let _lead_socket = lead.listen(&server)?;
    let _general_socket = general.listen(&server)?;
    step("entrances listening");
    server.ready();
    step(READY_STEP);
    if io::stdout().is_terminal() {
        ready_screen("Board ready.", "Lead handover", &lead_handover)?;
    } else if let Err(error) = writeln!(io::stdout().lock(), "{READY_STEP}")
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        // The detached launcher may close its pipe when --wait runs out.
        return Err(io_error(error));
    }
    server.run_with(|entrances| {
        let _ = entrances.refresh_claims(launcher_active);
    })
}

fn ready_screen(title: &str, label: &str, path: &Path) -> Result<(), Error> {
    let render = output::Format::stdout();
    let mut block = render.screen(title, "", &[(label, &path.to_string_lossy())]);
    render.next(
        &mut block,
        "Run malak board status with the same configuration.",
    );
    output::write(&block)
}

fn public_search(directory: &Directory) -> Result<(), Error> {
    directory
        .file()
        .set_permissions(std::fs::Permissions::from_mode(0o711))
        .map_err(io_error)
}

fn capacities(requests: usize) -> Result<(usize, usize), Error> {
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)
        .map_err(|error| Error::unavailable(format!("descriptor limit: {error}")))?;
    let descriptor_directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let open = std::fs::read_dir(descriptor_directory)
        .map_err(io_error)?
        .try_fold(0, |count, entry| entry.map(|_| count + 1))
        .map_err(io_error)?;
    let per_connection = descriptors_per_connection(requests);
    let needed = (LEAD_CONNECTIONS + GENERAL_CONNECTIONS)
        .saturating_mul(per_connection)
        .saturating_add(OWNER_DESCRIPTORS + BOARD_DESCRIPTORS)
        .saturating_add(open);
    let target = hard.min(needed as _);
    // Darwin's OPEN_MAX from sys/syslimits.h bounds the portable soft limit.
    #[cfg(target_os = "macos")]
    let target = target.min(10_240);
    let limit = if target > soft && setrlimit(Resource::RLIMIT_NOFILE, target, hard).is_ok() {
        target
    } else {
        soft
    };
    let available = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_sub(open);
    let (lead, general) = split_capacity(available, requests)?;
    if lead < LEAD_CONNECTIONS {
        output::diagnostic(
            "Warning: The descriptor limit caps the lead entrance.",
            &[("Connections", &lead.to_string())],
        );
    }
    if general < GENERAL_CONNECTIONS {
        output::diagnostic(
            "Warning: The descriptor limit caps the general entrance.",
            &[("Connections", &general.to_string())],
        );
    }
    Ok((lead, general))
}

fn split_capacity(available: usize, requests: usize) -> Result<(usize, usize), Error> {
    if available < OWNER_DESCRIPTORS {
        return Err(Error::unavailable(
            "descriptor limit cannot reserve the owner entrance",
        ));
    }
    let per_connection = descriptors_per_connection(requests);
    let places = available.saturating_sub(OWNER_DESCRIPTORS + BOARD_DESCRIPTORS) / per_connection;
    if places < 2 {
        return Err(Error::unavailable(
            "descriptor limit cannot serve all three entrances",
        ));
    }
    let lead = LEAD_CONNECTIONS.min(places - 1);
    let general = GENERAL_CONNECTIONS.min(places - lead);
    Ok((lead, general))
}

fn descriptors_per_connection(requests: usize) -> usize {
    // A request can hold a Markdown file and a journal file at the same time.
    1 + 2 * requests
}

struct Door {
    entrance: Entrance,
    address: String,
    directory: Directory,
    stale: bool,
    capacity: usize,
    access: Access,
}

#[derive(Clone, Copy)]
enum Access {
    Private,
    Named(u32),
    Public,
}

impl Door {
    fn prepare(
        root: &Path,
        name: &str,
        entrance: Entrance,
        capacity: usize,
        access: Access,
    ) -> Result<Self, Error> {
        let folder = root.join(name);
        let path = folder.join(SOCKET_FILE);
        let directory = match access {
            Access::Private => private_directory(&folder)?,
            Access::Public => {
                let directory = owned_directory(&folder)?;
                public_search(&directory)?;
                directory
            }
            Access::Named(uid) => {
                let directory = owned_directory(&folder)?;
                directory
                    .file()
                    .set_permissions(std::fs::Permissions::from_mode(0o700))
                    .map_err(io_error)?;
                share_with(directory.file(), uid, AccountAccess::Search).map_err(io_error)?;
                trusted_parent_for_account(&path, geteuid().as_raw(), uid).map_err(io_error)?;
                directory
            }
        };
        SocketAddr::from_pathname(&path).map_err(io_error)?;
        let stale = match path.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(io_error(error)),
            Ok(metadata)
                if metadata.file_type().is_socket() && metadata.uid() == geteuid().as_raw() =>
            {
                confirm_stale(&path)?;
                true
            }
            Ok(_) => return Err(Error::refused("socket pathname is already occupied")),
        };
        let utf8 = path
            .to_str()
            .ok_or_else(|| Error::refused("socket path must be UTF-8"))?;
        Ok(Self {
            entrance,
            address: format!("unix:{utf8}"),
            directory,
            stale,
            capacity,
            access,
        })
    }

    fn listen(self, server: &Server) -> Result<BoundSocket, Error> {
        if self.stale {
            self.directory.remove(SOCKET_FILE).map_err(io_error)?;
        }
        let transport = Arc::new(UnixTransport::bind(&self.address)?);
        let socket = BoundSocket(self.directory);
        if !matches!(self.access, Access::Private) {
            // The checked parent remains writable only by the board and root.
            fchmodat(
                &socket.0,
                SOCKET_FILE,
                Mode::from_bits_truncate(0o666),
                FchmodatFlags::FollowSymlink,
            )
            .map_err(|error| io_error(error.into()))?;
        }
        server.listen(self.entrance, transport, self.address, self.capacity)?;
        Ok(socket)
    }
}

fn confirm_stale(path: &Path) -> Result<(), Error> {
    let socket = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|error| io_error(error.into()))?;
    fcntl(&socket, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|error| io_error(error.into()))?;
    let address = UnixAddr::new(path).map_err(|error| io_error(error.into()))?;
    match connect(socket.as_raw_fd(), &address) {
        Err(Errno::ECONNREFUSED) => Ok(()),
        Ok(()) | Err(Errno::EAGAIN | Errno::EINPROGRESS) => {
            Err(Error::refused("socket pathname has a listener"))
        }
        Err(error) => Err(io_error(error.into())),
    }
}

struct BoundSocket(Directory);

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = self.0.remove(SOCKET_FILE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_keeps_the_reason_read_before_the_child_exits() {
        use std::os::unix::process::ExitStatusExt;

        let root = malak_testkit::unix_socket::SocketDir::new().unwrap();
        let log = root.path().join("board.log");
        let reason = "socket pathname is already occupied";
        std::fs::write(&log, format!("{reason}\n")).unwrap();
        let mut polls = [None, Some(ExitStatus::from_raw(2 << 8))].into_iter();

        let error = wait_until_ready(
            |_| {
                (
                    false,
                    Ok(polls.next().expect("unexpected poll after child exit")),
                )
            },
            &log,
            &File::open(&log).unwrap(),
            0,
            Instant::now() + Duration::from_secs(5),
            false,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), reason);
    }

    #[test]
    fn a_small_descriptor_budget_leaves_both_participant_entrances_available() {
        let (lead, general) = split_capacity(256, 8).unwrap();

        assert!(lead > 0);
        assert_eq!(general, 1);
        assert!(OWNER_DESCRIPTORS + BOARD_DESCRIPTORS + (lead + general) * 17 <= 256);
    }
}
