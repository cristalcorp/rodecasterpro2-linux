//! `rcp2ctl daemon`: owns the board's control session and serves its state.
//!
//! Once subscribed, the board freezes its faders if nobody reads its HID
//! interface (I-007), so this service keeps reading for as long as it runs.
//! It also keeps an up-to-date copy of the board's state tree, which other
//! commands read through a private Unix socket. Read-only: it never writes a
//! setting to the board. Alongside, it recreates the named outputs when the
//! board's sound card comes back (see [`crate::keeper`]).

use std::fs;
use std::io::{self, BufRead as _, BufReader, Read as _, Write};
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rcp2_proto::{BoardState, Change, DumpAssembler, Incoming, Node, apply_property, classify};
use serde::{Deserialize, Serialize};

/// Socket file name inside the runtime directory.
const SOCKET_NAME: &str = "board.sock";
/// How often to look for the board while it is absent.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);
/// Largest request accepted from a client.
const MAX_REQUEST: u64 = 256;
/// How long a client may take to send its request.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
/// Version of the socket protocol.
/// 2: channels carry their source code, fader levels may be unreadable.
/// 3: each board session has its own number.
pub(crate) const PROTOCOL_VERSION: u32 = 3;
/// Least time between two requests for a new dump.
const RESYNC_EVERY: Duration = Duration::from_secs(5);
/// Notifications kept while waiting for a dump; beyond that, a new dump is
/// needed anyway.
const MAX_PENDING: usize = 1024;
/// Clients served at the same time.
const MAX_CLIENTS: usize = 8;
/// Pause after a failed `accept`, so a persistent error cannot spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

/// Error of the service or of a client of it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DaemonError {
    #[error("XDG_RUNTIME_DIR is not set or not absolute: cannot place the private socket")]
    NoRuntimeDir,
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("another board service is already running ({0})")]
    AlreadyRunning(PathBuf),
    #[error("the board service is not running (`rcp2ctl hid setup` installs and starts it)")]
    NotRunning,
    #[error("the board service sent an invalid answer: {0}")]
    BadAnswer(String),
    #[error(
        "the board service is older than this rcp2ctl: run `hid setup` with this rcp2ctl to update it"
    )]
    ServiceOlder,
    #[error("the board service is newer than this rcp2ctl: use the rcp2ctl it was installed with")]
    ServiceNewer,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> DaemonError + use<> {
    let path = path.to_owned();
    move |source| DaemonError::Io { path, source }
}

/// `$XDG_RUNTIME_DIR/rodecasterpro2-linux/board.sock`: the runtime directory
/// belongs to the user alone (mode 0700, created by systemd-logind).
pub(crate) fn socket_path() -> Result<PathBuf, DaemonError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(DaemonError::NoRuntimeDir)?;
    Ok(runtime.join(rcp2_audio::APP_DIR).join(SOCKET_NAME))
}

/// One channel strip, as sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChannelDto {
    /// Position under the root of the board's state tree.
    pub(crate) index: usize,
    /// Human-readable source.
    pub(crate) source: String,
    /// The board's source code (`channelInputSource`), if readable.
    #[serde(default)]
    pub(crate) code: Option<i32>,
    /// Output muted, if reported.
    pub(crate) muted: Option<bool>,
}

/// The board state, as sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateDto {
    pub(crate) version: u32,
    /// A session with the board is open.
    pub(crate) connected: bool,
    /// Number of the board session, new after every unplug: a state from
    /// another session no longer describes the board.
    #[serde(default)]
    pub(crate) session: u64,
    /// The board's state is known (false while the first or a new dump is read).
    pub(crate) state_known: bool,
    pub(crate) firmware: Option<String>,
    pub(crate) channels: Vec<ChannelDto>,
    /// One entry per fader; `None` if its level is unreadable.
    pub(crate) faders: Vec<Option<i32>>,
    /// Notifications applied since the last full state dump.
    pub(crate) notifications: u64,
}

impl StateDto {
    fn new(shared: &Shared) -> Self {
        let Shared {
            connected,
            session,
            ref tree,
            notifications,
        } = *shared;
        let state = tree.as_ref().map(BoardState::from_tree).unwrap_or_default();
        Self {
            version: PROTOCOL_VERSION,
            connected,
            session,
            state_known: tree.is_some(),
            firmware: state.firmware,
            channels: state
                .channels
                .iter()
                .map(|channel| ChannelDto {
                    index: channel.index,
                    source: channel.source.to_string(),
                    code: channel.source.code(),
                    muted: channel.muted,
                })
                .collect(),
            faders: state.faders,
            notifications,
        }
    }
}

/// What the session thread shares with the socket server.
#[derive(Default)]
struct Shared {
    connected: bool,
    /// Incremented each time a session opens; kept across sessions.
    session: u64,
    tree: Option<Node>,
    notifications: u64,
}

type SharedState = Arc<Mutex<Shared>>;

fn lock(shared: &SharedState) -> std::sync::MutexGuard<'_, Shared> {
    // A panic elsewhere cannot leave this plain data half-updated in a
    // harmful way: keep serving it.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the service until killed: binds the socket, then keeps a session
/// with the board, reconnecting after unplugs.
///
/// # Errors
///
/// Returns [`DaemonError`] if the socket cannot be set up or another
/// instance is running.
pub(crate) fn run(log: &mut impl Write) -> Result<(), DaemonError> {
    let listener = bind(&socket_path()?)?;
    if let Ok(crate::service::UnitState::Outdated) =
        crate::service::unit_path().and_then(|path| crate::service::unit_state(&path))
    {
        let _ = writeln!(
            log,
            "this unit was written by an older version: run `rcp2ctl hid setup` to update it"
        );
    }
    let shared = SharedState::default();
    let server_state = Arc::clone(&shared);
    thread::spawn(move || serve(&listener, &server_state));
    thread::spawn(|| crate::keeper::run(&mut io::stderr()));
    let sys_class = Path::new(rcp2_hid::SYS_CLASS_HIDRAW);
    loop {
        match rcp2_hid::find_device(sys_class).and_then(|path| {
            let board = rcp2_hid::Board::open(&path)?;
            Ok((path, board))
        }) {
            Ok((path, board)) => {
                let _ = writeln!(log, "board found at {}: opening a session", path.display());
                let reason = session(board, &shared, log);
                let _ = writeln!(log, "session ended: {reason}");
                let mut state = lock(&shared);
                *state = Shared {
                    session: state.session,
                    ..Shared::default()
                };
            }
            Err(rcp2_hid::HidError::NotFound) => {}
            Err(err) => {
                let _ = writeln!(log, "{err}");
            }
        }
        thread::sleep(RECONNECT_EVERY);
    }
}

/// Binds the socket in a private directory, replacing a stale socket but
/// refusing to start next to a running instance.
fn bind(path: &Path) -> Result<UnixListener, DaemonError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err(dir))?;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() || UnixStream::connect(path).is_ok() {
            return Err(DaemonError::AlreadyRunning(path.to_owned()));
        }
        // A socket nobody listens on: left over by a killed instance.
        fs::remove_file(path).map_err(io_err(path))?;
    }
    let listener = UnixListener::bind(path).map_err(io_err(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_err(path))?;
    Ok(listener)
}

/// One session with the board, until it is unplugged or a read fails.
fn session(mut board: rcp2_hid::Board, shared: &SharedState, log: &mut impl Write) -> String {
    let reports = match board.reports(Instant::now()) {
        Ok(reports) => reports,
        Err(err) => return err.to_string(),
    };
    let mut sync = Tracker::default();
    if let Err(err) = sync.request(&mut board) {
        return err.to_string();
    }
    {
        let mut state = lock(shared);
        state.connected = true;
        state.session = state.session.wrapping_add(1);
    }
    loop {
        let report = match reports.recv_timeout(RESYNC_EVERY) {
            Ok(Ok(report)) => Some(report),
            Ok(Err(err)) => return format!("read error: {err}"),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return "reader stopped".to_owned(),
        };
        let needs_resync = match report {
            Some(report) => sync.handle(&report.bytes, shared),
            // Quiet board: only a problem if a dump was asked for and never came.
            None => lock(shared).tree.is_none(),
        };
        if needs_resync && sync.may_request() {
            let _ = writeln!(
                log,
                "state copy out of date: asking the board for a new dump"
            );
            lock(shared).tree = None;
            if let Err(err) = sync.request(&mut board) {
                return err.to_string();
            }
        }
    }
}

/// Keeps the copy of the board state in step with the board.
#[derive(Default)]
struct Tracker {
    assembler: DumpAssembler,
    /// Property changes received while no complete dump is available.
    pending: Vec<Change>,
    last_request: Option<Instant>,
}

impl Tracker {
    /// Asks for a full dump (the handshake), forgetting partial data.
    fn request(&mut self, board: &mut rcp2_hid::Board) -> Result<(), rcp2_hid::HidError> {
        self.assembler = DumpAssembler::default();
        self.pending.clear();
        self.last_request = Some(Instant::now());
        board.handshake()
    }

    /// Rate limit: at most one request every [`RESYNC_EVERY`].
    fn may_request(&self) -> bool {
        self.last_request
            .is_none_or(|last| last.elapsed() >= RESYNC_EVERY)
    }

    /// Handles one report; `true` if the copy is out of date.
    fn handle(&mut self, report: &[u8], shared: &SharedState) -> bool {
        match classify(report) {
            Incoming::DumpChunk(chunk) => match self.assembler.push(chunk) {
                Ok(Some(mut tree)) => {
                    let mut applied = 0;
                    let mut fits = true;
                    for change in self.pending.drain(..) {
                        if apply(&mut tree, change) {
                            applied += 1;
                        } else {
                            fits = false;
                        }
                    }
                    let mut state = lock(shared);
                    state.tree = Some(tree);
                    state.notifications = applied;
                    !fits
                }
                Ok(None) => false,
                // A chunk that belongs to no dump: our view of the stream is off.
                Err(_) => {
                    self.assembler = DumpAssembler::default();
                    true
                }
            },
            Incoming::Change(change) => {
                let mut state = lock(shared);
                match state.tree.as_mut() {
                    Some(tree) if self.assembler.missing().is_none() => {
                        if apply(tree, change) {
                            state.notifications += 1;
                            false
                        } else {
                            true
                        }
                    }
                    _ => {
                        drop(state);
                        if self.pending.len() >= MAX_PENDING {
                            self.pending.clear();
                            return true;
                        }
                        self.pending.push(change);
                        false
                    }
                }
            }
            Incoming::Ack | Incoming::Unknown => false,
        }
    }
}

/// Applies a change; `false` if the copy is out of date and needs a new dump
/// (changes to the tree's shape are not applied, they always need one).
fn apply(tree: &mut Node, change: Change) -> bool {
    match change {
        Change::PropertyChanged { path, name, value } => {
            apply_property(tree, &path, &name, value).is_ok()
        }
        Change::FullSync(new_tree) => {
            *tree = new_tree;
            true
        }
        Change::Structural { .. } => false,
    }
}

/// Answers clients, one request per connection, each on its own thread (up
/// to [`MAX_CLIENTS`] at once), so a slow client cannot block the others.
fn serve(listener: &UnixListener, shared: &SharedState) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            thread::sleep(ACCEPT_BACKOFF);
            continue;
        };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let shared = Arc::clone(shared);
        let active = Arc::clone(&active);
        thread::spawn(move || {
            // A misbehaving client only affects its own connection.
            let _ = answer(stream, &shared);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn answer(stream: UnixStream, shared: &SharedState) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let mut request = String::new();
    BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut request)?;
    let mut stream = stream;
    if request.trim() != "state" {
        return writeln!(stream, "{{\"error\":\"unknown request\"}}");
    }
    let answer = StateDto::new(&lock(shared));
    let json = serde_json::to_string(&answer).map_err(io::Error::other)?;
    writeln!(stream, "{json}")
}

/// Asks the running service for the board state.
///
/// # Errors
///
/// Returns [`DaemonError::NotRunning`] if no service answers,
/// [`DaemonError::BadAnswer`] if the answer cannot be read, and
/// [`DaemonError::ServiceOlder`] or [`DaemonError::ServiceNewer`] if the
/// service speaks another protocol version.
pub(crate) fn query_state() -> Result<StateDto, DaemonError> {
    let state = query_any_version()?;
    check_version(state.version)?;
    Ok(state)
}

/// Accepts only the protocol version this build speaks.
fn check_version(version: u32) -> Result<(), DaemonError> {
    match version.cmp(&PROTOCOL_VERSION) {
        std::cmp::Ordering::Less => Err(DaemonError::ServiceOlder),
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(DaemonError::ServiceNewer),
    }
}

fn query_any_version() -> Result<StateDto, DaemonError> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).map_err(|_| DaemonError::NotRunning)?;
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(io_err(&path))?;
    writeln!(stream, "state").map_err(io_err(&path))?;
    let mut line = String::new();
    BufReader::new(stream.take(1 << 20))
        .read_line(&mut line)
        .map_err(io_err(&path))?;
    serde_json::from_str(&line).map_err(|err| DaemonError::BadAnswer(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{DaemonError, PROTOCOL_VERSION, Shared, StateDto, apply, check_version};
    use rcp2_proto::{Change, Node, Var};

    #[test]
    fn shape_changes_always_need_a_new_dump() {
        let mut tree = Node::default();
        let change = Change::Structural {
            kind: 3,
            path: vec![],
        };
        assert!(!apply(&mut tree, change));
    }

    #[test]
    fn a_change_outside_the_tree_marks_the_copy_out_of_date() {
        let mut tree = Node::default();
        let change = Change::PropertyChanged {
            path: vec![3],
            name: "x".to_owned(),
            value: Var::Int(1),
        };
        assert!(!apply(&mut tree, change));
    }

    #[test]
    fn a_session_without_a_tree_is_connected_but_unknown() {
        let state = StateDto::new(&Shared {
            connected: true,
            session: 4,
            ..Shared::default()
        });
        assert!(state.connected);
        assert_eq!(state.session, 4);
        assert!(!state.state_known);
        assert!(state.channels.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<StateDto>(&json).unwrap(), state);
    }

    #[test]
    fn another_protocol_version_says_which_side_is_behind() {
        assert!(check_version(PROTOCOL_VERSION).is_ok());
        assert!(matches!(
            check_version(PROTOCOL_VERSION - 1),
            Err(DaemonError::ServiceOlder)
        ));
        assert!(matches!(
            check_version(PROTOCOL_VERSION + 1),
            Err(DaemonError::ServiceNewer)
        ));
    }
}
