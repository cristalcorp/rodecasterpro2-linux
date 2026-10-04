//! `rcp2ctl daemon`: owns the board's control session and serves its state.
//!
//! Once subscribed, the board freezes its faders if nobody reads its HID
//! interface (I-007), so this service keeps reading for as long as it runs.
//! It also keeps an up-to-date copy of the board's state tree, which other
//! commands read through a private Unix socket. Read-only: it never writes a
//! setting to the board.

use std::fs;
use std::io::{self, BufRead as _, BufReader, Read as _, Write};
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rcp2_proto::{
    BoardState, Change, DumpAssembler, Incoming, InputSource, Node, apply_property, classify,
};
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
const PROTOCOL_VERSION: u32 = 1;

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
    /// Output muted, if reported.
    pub(crate) muted: Option<bool>,
}

/// The board state, as sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StateDto {
    pub(crate) version: u32,
    /// The board is connected and its state is known.
    pub(crate) connected: bool,
    pub(crate) firmware: Option<String>,
    pub(crate) channels: Vec<ChannelDto>,
    pub(crate) faders: Vec<i32>,
    /// Notifications applied since the last full state dump.
    pub(crate) notifications: u64,
}

impl StateDto {
    fn from_tree(tree: Option<&Node>, notifications: u64) -> Self {
        let state = tree.map(BoardState::from_tree).unwrap_or_default();
        Self {
            version: PROTOCOL_VERSION,
            connected: tree.is_some(),
            firmware: state.firmware,
            channels: state
                .channels
                .iter()
                .map(|channel| ChannelDto {
                    index: channel.index,
                    source: source_label(channel.source),
                    muted: channel.muted,
                })
                .collect(),
            faders: state.faders,
            notifications,
        }
    }
}

fn source_label(source: InputSource) -> String {
    source.to_string()
}

/// What the session thread shares with the socket server.
#[derive(Default)]
struct Shared {
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
    let shared = SharedState::default();
    let server_state = Arc::clone(&shared);
    thread::spawn(move || serve(&listener, &server_state));
    let sys_class = Path::new(rcp2_hid::SYS_CLASS_HIDRAW);
    loop {
        match rcp2_hid::find_device(sys_class).and_then(|path| {
            let board = rcp2_hid::Board::open(&path)?;
            Ok((path, board))
        }) {
            Ok((path, board)) => {
                let _ = writeln!(log, "board found at {}: opening a session", path.display());
                let reason = session(board, &shared);
                let _ = writeln!(log, "session ended: {reason}");
                let mut state = lock(&shared);
                state.tree = None;
                state.notifications = 0;
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
fn session(mut board: rcp2_hid::Board, shared: &SharedState) -> String {
    let reports = match board.reports(Instant::now()) {
        Ok(reports) => reports,
        Err(err) => return err.to_string(),
    };
    if let Err(err) = board.handshake() {
        return err.to_string();
    }
    follow(&reports, &mut board, shared)
}

/// Reads reports forever: assembles dumps, applies notifications.
fn follow(
    reports: &Receiver<io::Result<rcp2_hid::Report>>,
    board: &mut rcp2_hid::Board,
    shared: &SharedState,
) -> String {
    let mut assembler = DumpAssembler::default();
    // Notifications that arrive while a dump is being assembled.
    let mut pending: Vec<Change> = Vec::new();
    loop {
        let report = match reports.recv() {
            Ok(Ok(report)) => report,
            Ok(Err(err)) => return format!("read error: {err}"),
            Err(_) => return "reader stopped".to_owned(),
        };
        match classify(&report.bytes) {
            Incoming::DumpChunk(chunk) => match assembler.push(chunk) {
                Ok(Some(mut tree)) => {
                    let mut applied = 0;
                    for change in pending.drain(..) {
                        if apply(&mut tree, change) {
                            applied += 1;
                        }
                    }
                    let mut state = lock(shared);
                    state.tree = Some(tree);
                    state.notifications = applied;
                }
                Ok(None) => {}
                Err(_) => assembler = DumpAssembler::default(),
            },
            Incoming::Change(change) => {
                if assembler.missing().is_some() {
                    pending.push(change);
                    continue;
                }
                let mut state = lock(shared);
                let Some(tree) = state.tree.as_mut() else {
                    pending.push(change);
                    continue;
                };
                if apply(tree, change) {
                    state.notifications += 1;
                } else {
                    // Our copy no longer matches the board: ask for a new dump.
                    state.tree = None;
                    drop(state);
                    if let Err(err) = board.handshake() {
                        return err.to_string();
                    }
                }
            }
            Incoming::Ack | Incoming::Unknown => {}
        }
    }
}

/// Applies a property change; `false` if the copy is out of date.
fn apply(tree: &mut Node, change: Change) -> bool {
    match change {
        Change::PropertyChanged { path, name, value } => {
            apply_property(tree, &path, &name, value).is_ok()
        }
        Change::FullSync(_) | Change::Other { .. } => true,
    }
}

/// Answers clients, one request per connection.
fn serve(listener: &UnixListener, shared: &SharedState) {
    for stream in listener.incoming().flatten() {
        // A misbehaving client only affects its own connection.
        let _ = answer(stream, shared);
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
    let answer = {
        let state = lock(shared);
        StateDto::from_tree(state.tree.as_ref(), state.notifications)
    };
    let json = serde_json::to_string(&answer).map_err(io::Error::other)?;
    writeln!(stream, "{json}")
}

/// Asks the running service for the board state.
///
/// # Errors
///
/// Returns [`DaemonError::NotRunning`] if no service answers, and
/// [`DaemonError::BadAnswer`] if the answer cannot be read.
pub(crate) fn query_state() -> Result<StateDto, DaemonError> {
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
    use super::{StateDto, apply};
    use rcp2_proto::{Change, Node, Var};

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
    fn without_a_tree_the_board_is_reported_disconnected() {
        let state = StateDto::from_tree(None, 0);
        assert!(!state.connected);
        assert!(state.channels.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<StateDto>(&json).unwrap(), state);
    }
}
