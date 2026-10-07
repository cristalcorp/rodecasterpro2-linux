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
/// 3: the previous state is kept, marked refreshing, while a new dump is read.
/// 4: a client turned away is told so (`{"error":"busy"}`, no version).
/// 5: `starting`, while the service has not finished its first look at the
///    board.
pub(crate) const PROTOCOL_VERSION: u32 = 5;
/// How long the previous state is still served while a new dump is awaited:
/// a dump takes a moment, one that never comes must not leave old values.
const STALE_FOR: Duration = Duration::from_secs(15);
/// Least time between two requests for a new dump.
const RESYNC_EVERY: Duration = Duration::from_secs(5);
/// A dump awaited with no chunk for this long has stopped (or never
/// started): it is asked for again. A dump (90 to 140 KB depending on the
/// firmware) is a few hundred chunks read as they come; this margin is not
/// measured.
const CHUNK_SILENCE: Duration = Duration::from_secs(2);
/// How often the session looks at the time when the board is quiet.
const TICK: Duration = Duration::from_millis(250);
/// Lock file next to the socket: held by the running instance.
const LOCK_NAME: &str = "board.lock";
/// Notifications kept while waiting for a dump; beyond that, a new dump is
/// needed anyway.
const MAX_PENDING: usize = 1024;
/// Clients served at the same time.
const MAX_CLIENTS: usize = 8;
/// How long a client's whole request may take, from sending it to the end
/// of the answer.
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest answer a client accepts.
const MAX_ANSWER: usize = 1 << 20;
/// The line sent to a client turned away because enough are being served.
const BUSY_ANSWER: &str = "{\"error\":\"busy\"}";
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
        "the board service is older than this rcp2ctl: restart it with this one (`rcp2ctl hid setup` for the systemd service)"
    )]
    ServiceOlder,
    #[error("the board service is newer than this rcp2ctl: use the rcp2ctl it was installed with")]
    ServiceNewer,
    #[error("the board service is busy")]
    Busy,
    #[error("the board service did not answer in time")]
    NoAnswer,
    #[error("the board service closed the connection without answering")]
    Dropped,
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
    pub(crate) code: Option<i32>,
    /// Output muted, if reported.
    pub(crate) muted: Option<bool>,
}

/// The board state, as sent to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of the socket protocol, read by clients one by one"
)]
pub(crate) struct StateDto {
    pub(crate) version: u32,
    /// A session with the board is open.
    pub(crate) connected: bool,
    /// The board's state is known (false while the first dump is read, and
    /// when a new one takes too long).
    pub(crate) state_known: bool,
    /// The state is the previous one: a new dump is being read.
    pub(crate) refreshing: bool,
    /// The service has not finished its first look at the board (searching
    /// it, or reading its first dump); bounded by [`STALE_FOR`].
    pub(crate) starting: bool,
    pub(crate) firmware: Option<String>,
    pub(crate) channels: Vec<ChannelDto>,
    /// One entry per fader; `None` if its level is unreadable.
    pub(crate) faders: Vec<Option<i32>>,
    /// Notifications applied since the last full state dump.
    pub(crate) notifications: u64,
}

impl StateDto {
    fn new(shared: &Shared, now: Instant) -> Self {
        let Shared {
            connected,
            ref tree,
            refreshing_since,
            starting_since,
            notifications,
        } = *shared;
        let recent = |since: Instant| now.saturating_duration_since(since) < STALE_FOR;
        let tree = tree
            .as_ref()
            .filter(|_| refreshing_since.is_none_or(recent));
        let state = tree.map(BoardState::from_tree).unwrap_or_default();
        Self {
            version: PROTOCOL_VERSION,
            connected,
            state_known: tree.is_some(),
            refreshing: tree.is_some() && refreshing_since.is_some(),
            starting: starting_since.is_some_and(recent),
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
    /// The last complete state, kept while a new dump is read.
    tree: Option<Node>,
    /// Since when the tree is not the board's: a new dump was asked for, or
    /// its shape is unsure. Until a new dump is in.
    refreshing_since: Option<Instant>,
    /// When the service started, until its first look at the board is over
    /// (first dump in, board not found, or session ended).
    starting_since: Option<Instant>,
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
    // Held for as long as the service runs.
    let (_lock, listener) = bind(&socket_path()?)?;
    if let Ok(crate::service::UnitState::Outdated) =
        crate::service::unit_path().and_then(|path| crate::service::unit_state(&path))
    {
        let _ = writeln!(
            log,
            "this unit was written by an older version: run `rcp2ctl hid setup` to update it"
        );
    }
    let shared = SharedState::new(Mutex::new(Shared {
        starting_since: Some(Instant::now()),
        ..Shared::default()
    }));
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
                session_ended(&shared);
            }
            Err(err) => {
                board_absent(&shared);
                if !matches!(err, rcp2_hid::HidError::NotFound) {
                    let _ = writeln!(log, "{err}");
                }
            }
        }
        thread::sleep(RECONNECT_EVERY);
    }
}

/// S1: no board to read, the first look is over.
fn board_absent(shared: &SharedState) {
    lock(shared).starting_since = None;
}

/// S11: the session is over, and starting with it; a blank state until the
/// board is found again.
fn session_ended(shared: &SharedState) {
    *lock(shared) = Shared::default();
}

/// Takes the instance lock, then binds the socket in a private directory,
/// replacing one left by a killed instance. The lock (released by the
/// kernel when the process ends, however it ends) is what tells a running
/// instance, frozen or not, from a leftover socket; a socket someone listens
/// on is kept too, for an instance older than the lock.
fn bind(path: &Path) -> Result<(fs::File, UnixListener), DaemonError> {
    let dir = path.parent().ok_or_else(|| {
        io_err(path)(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no directory to place the socket in",
        ))
    })?;
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err(dir))?;
    let lock_path = dir.join(LOCK_NAME);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io_err(&lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            return Err(DaemonError::AlreadyRunning(path.to_owned()));
        }
        Err(fs::TryLockError::Error(err)) => return Err(io_err(&lock_path)(err)),
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        // Not ours to remove.
        if !meta.file_type().is_socket() {
            return Err(DaemonError::AlreadyRunning(path.to_owned()));
        }
        // Listened on (a full backlog says so too): an instance without the
        // lock.
        match connect_now(path) {
            Err(err) if err.kind() != io::ErrorKind::WouldBlock => {}
            _ => return Err(DaemonError::AlreadyRunning(path.to_owned())),
        }
        match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(io_err(path)(err)),
            _ => {}
        }
    }
    let listener = UnixListener::bind(path).map_err(io_err(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_err(path))?;
    Ok((lock, listener))
}

/// One session with the board, until it is unplugged or a read fails.
fn session(mut board: rcp2_hid::Board, shared: &SharedState, log: &mut impl Write) -> String {
    let reports = match board.reports(Instant::now()) {
        Ok(reports) => reports,
        Err(err) => return err.to_string(),
    };
    let mut sync = Tracker::default();
    sync.start_request(shared, Instant::now());
    if let Err(err) = board.handshake() {
        return err.to_string();
    }
    lock(shared).connected = true;
    loop {
        match reports.recv_timeout(TICK) {
            Ok(Ok(report)) => sync.handle(&report.bytes, shared, Instant::now()),
            Ok(Err(err)) => return format!("read error: {err}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return "reader stopped".to_owned(),
        }
        let now = Instant::now();
        if sync.due(now) {
            let _ = writeln!(
                log,
                "state copy out of date: asking the board for a new dump"
            );
            sync.start_request(shared, now);
            if let Err(err) = board.handshake() {
                return err.to_string();
            }
        }
    }
}

/// Keeps the copy of the board state in step with the board. Told the time
/// rather than reading it, so its rules are tested without a board.
#[derive(Default)]
struct Tracker {
    assembler: DumpAssembler,
    /// Property changes received while the tree is not the board's, for the
    /// new one.
    pending: Vec<Change>,
    last_request: Option<Instant>,
    /// A new dump is needed: asked for as soon as the rate limit allows,
    /// and once the dump being read is in.
    wanted: bool,
    /// While a dump is awaited: when it last showed progress (asked for, or
    /// a chunk received).
    progress: Option<Instant>,
}

impl Tracker {
    /// Records that a full dump (the handshake) is being asked for,
    /// forgetting partial data; the current tree is still served, as
    /// refreshing, until the dump is in.
    fn start_request(&mut self, shared: &SharedState, now: Instant) {
        self.assembler = DumpAssembler::default();
        self.pending.clear();
        self.last_request = Some(now);
        self.progress = Some(now);
        self.wanted = false;
        lock(shared).refreshing_since.get_or_insert(now);
    }

    /// Whether to ask for a new dump now: one is needed, or the awaited one
    /// has gone silent; never while one is arriving, and at most one request
    /// every [`RESYNC_EVERY`].
    fn due(&self, now: Instant) -> bool {
        let arriving = self
            .progress
            .is_some_and(|at| now.saturating_duration_since(at) < CHUNK_SILENCE);
        let silent = self.progress.is_some() && !arriving;
        let allowed = self
            .last_request
            .is_none_or(|last| now.saturating_duration_since(last) >= RESYNC_EVERY);
        !arriving && (self.wanted || silent) && allowed
    }

    /// The copy is out of date: its shape is unsure, so nothing more is
    /// applied to it (changes wait for a new dump) and it is served as
    /// refreshing until one is in.
    fn need(&mut self, shared: &SharedState, now: Instant) {
        self.wanted = true;
        lock(shared).refreshing_since.get_or_insert(now);
    }

    /// Handles one report.
    fn handle(&mut self, report: &[u8], shared: &SharedState, now: Instant) {
        match classify(report) {
            Incoming::DumpChunk(chunk) => self.chunk(chunk, shared, now),
            Incoming::Change(change) => self.change(change, shared, now),
            Incoming::Ack | Incoming::Unknown => {}
        }
    }

    fn chunk(&mut self, chunk: &[u8], shared: &SharedState, now: Instant) {
        match self.assembler.push(chunk) {
            Ok(Some(mut tree)) => {
                let mut applied = 0;
                let mut fits = true;
                for change in self.pending.drain(..) {
                    // Once one does not fit, the shape is unsure: stop.
                    if fits && apply(&mut tree, change) {
                        applied += 1;
                    } else {
                        fits = false;
                    }
                }
                self.progress = None;
                let mut state = lock(shared);
                state.tree = Some(tree);
                state.refreshing_since = None;
                state.starting_since = None;
                state.notifications = applied;
                drop(state);
                // Changes lost (queue full) are not in it either.
                if !fits || self.wanted {
                    self.need(shared, now);
                }
            }
            // A dump arriving, asked for or not (S13): the tree it replaces
            // is out of date.
            Ok(None) => {
                self.progress = Some(now);
                lock(shared).refreshing_since.get_or_insert(now);
            }
            // A chunk that belongs to no dump: our view of the stream is off.
            // The board is still sending (S14): asked again once it is
            // quiet.
            Err(_) => {
                self.assembler = DumpAssembler::default();
                self.progress = Some(now);
                self.need(shared, now);
            }
        }
    }

    fn change(&mut self, change: Change, shared: &SharedState, now: Instant) {
        let mut state = lock(shared);
        let sure = state.refreshing_since.is_none() && self.assembler.missing().is_none();
        match state.tree.as_mut() {
            Some(tree) if sure => {
                if apply(tree, change) {
                    state.notifications += 1;
                } else {
                    drop(state);
                    self.need(shared, now);
                }
            }
            // The tree being replaced is out of date: keep the change for
            // the new one.
            _ => {
                drop(state);
                if self.pending.len() >= MAX_PENDING {
                    // Lost changes: the dump being read will not be enough.
                    self.pending.clear();
                    self.need(shared, now);
                } else {
                    self.pending.push(change);
                }
            }
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
            turn_away(stream);
            continue;
        }
        let shared = Arc::clone(shared);
        let slot = Slot(Arc::clone(&active));
        thread::spawn(move || {
            // Released even if answering panics.
            let _slot = slot;
            // A misbehaving client only affects its own connection.
            let _ = answer(stream, &shared);
        });
    }
}

/// Tells a client it is turned away rather than hanging up on it, so it
/// knows to ask again; a fresh socket takes one short line without blocking.
fn turn_away(mut stream: UnixStream) {
    let _ = stream
        .set_nonblocking(true)
        .and_then(|()| writeln!(stream, "{BUSY_ANSWER}"));
}

/// One client being served: frees its place when dropped.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn answer(stream: UnixStream, shared: &SharedState) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let mut request = String::new();
    if BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut request)? == 0 {
        // Nothing asked: a client that gave up before it was let in.
        return Ok(());
    }
    let mut stream = stream;
    if request.trim() != "state" {
        return writeln!(stream, "{{\"error\":\"unknown request\"}}");
    }
    // Built under the lock, so every field is of the same moment: it reads
    // a few nodes, cheaper than copying the tree out. The lock is released
    // before the answer is serialized and written.
    let answer = StateDto::new(&lock(shared), Instant::now());
    let json = serde_json::to_string(&answer).map_err(io::Error::other)?;
    writeln!(stream, "{json}")
}

/// Accepts only the protocol version this build speaks.
fn check_version(version: u32) -> Result<(), DaemonError> {
    match version.cmp(&PROTOCOL_VERSION) {
        std::cmp::Ordering::Less => Err(DaemonError::ServiceOlder),
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(DaemonError::ServiceNewer),
    }
}

/// Asks the running service for the board state.
///
/// # Errors
///
/// Returns [`DaemonError::NotRunning`] if no service answers,
/// [`DaemonError::Busy`] or [`DaemonError::NoAnswer`] if it cannot answer
/// now (within [`QUERY_TIMEOUT`]), [`DaemonError::BadAnswer`] if the answer
/// cannot be read, and [`DaemonError::ServiceOlder`] or
/// [`DaemonError::ServiceNewer`] if it speaks another protocol version.
pub(crate) fn query_state() -> Result<StateDto, DaemonError> {
    let path = socket_path()?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut stream = connect(&path)?;
    let line = exchange(&mut stream, deadline).map_err(|err| answer_err(&path, err))?;
    parse_answer(&line)
}

/// Sends the request and reads the answer line, all by `deadline`.
fn exchange(stream: &mut UnixStream, deadline: Instant) -> io::Result<String> {
    stream.set_write_timeout(Some(time_left(deadline)?))?;
    // A busy service answers and hangs up without reading: the request may
    // then fail to go out, but its answer is there to read.
    if let Err(err) = writeln!(stream, "state")
        && !matches!(
            err.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        return Err(err);
    }
    read_answer(stream, deadline)
}

/// Time left before `deadline`; none is a timeout.
fn time_left(deadline: Instant) -> io::Result<Duration> {
    Some(deadline.saturating_duration_since(Instant::now()))
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}

/// Connects without waiting: a service that no longer accepts (frozen, its
/// backlog full) would make a blocking `connect` wait without limit.
fn connect(path: &Path) -> Result<UnixStream, DaemonError> {
    match connect_now(path) {
        Ok(stream) => Ok(stream),
        // No socket, or nobody listening on it.
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(DaemonError::NotRunning)
        }
        // Someone listens but does not take connections now.
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Err(DaemonError::NoAnswer),
        Err(err) => Err(io_err(path)(err)),
    }
}

/// A Unix socket `connect` that fails with `WouldBlock` rather than waits
/// when the listener's backlog is full; the stream is blocking again after.
fn connect_now(path: &Path) -> io::Result<UnixStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    socket.connect(&socket2::SockAddr::unix(path)?)?;
    socket.set_nonblocking(false)?;
    Ok(UnixStream::from(std::os::fd::OwnedFd::from(socket)))
}

/// A failed exchange: a timeout means the service could not answer now
/// (it says so itself when busy); anything else is a broken answer or
/// reported as is.
fn answer_err(path: &Path, err: io::Error) -> DaemonError {
    match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => DaemonError::NoAnswer,
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof => DaemonError::Dropped,
        io::ErrorKind::InvalidData => DaemonError::BadAnswer(err.to_string()),
        _ => io_err(path)(err),
    }
}

/// Reads one answer line, all of it by `deadline` (a per-read timeout
/// alone lets a service that trickles bytes take forever).
fn read_answer(stream: &mut UnixStream, deadline: Instant) -> io::Result<String> {
    let mut answer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut complete = false;
    while !complete {
        stream.set_read_timeout(Some(time_left(deadline)?))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                let new = chunk.get(..read).unwrap_or_default();
                // One line: whatever follows it is not part of the answer.
                let line = new
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(new, |end| new.get(..=end).unwrap_or(new));
                complete = line.ends_with(b"\n");
                answer.extend_from_slice(line);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
        if answer.len() > MAX_ANSWER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "answer too long",
            ));
        }
    }
    String::from_utf8(answer).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

/// Reads the version alone first: another version may have another shape.
fn parse_answer(line: &str) -> Result<StateDto, DaemonError> {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    #[derive(Deserialize)]
    struct Refused {
        error: String,
    }
    let bad = |err: serde_json::Error| DaemonError::BadAnswer(err.to_string());
    match serde_json::from_str::<Versioned>(line) {
        Ok(Versioned { version }) => {
            check_version(version)?;
            serde_json::from_str(line).map_err(bad)
        }
        // No version: a refusal, which has no state to give.
        Err(err) => match serde_json::from_str::<Refused>(line) {
            Ok(Refused { error }) if error == "busy" => Err(DaemonError::Busy),
            Ok(Refused { error }) => Err(DaemonError::BadAnswer(error)),
            Err(_) => Err(bad(err)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUSY_ANSWER, CHUNK_SILENCE, DaemonError, MAX_PENDING, PROTOCOL_VERSION, QUERY_TIMEOUT,
        RESYNC_EVERY, STALE_FOR, Shared, SharedState, StateDto, Tracker, answer_err, apply, bind,
        board_absent, check_version, connect, exchange, lock, parse_answer, read_answer,
        session_ended, turn_away,
    };
    use rcp2_proto::{Change, Node, Var};
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::{Duration, Instant};

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
        let state = StateDto::new(
            &Shared {
                connected: true,
                ..Shared::default()
            },
            Instant::now(),
        );
        assert!(state.connected);
        assert!(!state.refreshing);
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

    #[test]
    fn a_newer_answer_of_another_shape_still_says_newer() {
        let line = format!(
            "{{\"version\":{},\"channels\":\"renamed\"}}",
            PROTOCOL_VERSION + 1
        );
        assert!(matches!(
            parse_answer(&line),
            Err(DaemonError::ServiceNewer)
        ));
        assert!(matches!(
            parse_answer("not json"),
            Err(DaemonError::BadAnswer(_))
        ));
    }

    #[test]
    fn the_previous_state_is_served_while_a_new_dump_is_read() {
        let asked = Instant::now();
        let shared = Shared {
            connected: true,
            tree: Some(Node::default()),
            refreshing_since: Some(asked),
            starting_since: None,
            notifications: 0,
        };
        let soon = StateDto::new(&shared, asked + Duration::from_secs(1));
        assert!(soon.state_known);
        assert!(soon.refreshing);
        // A dump that never comes: the old state is no longer served.
        let late = StateDto::new(&shared, asked + STALE_FOR);
        assert!(!late.state_known);
        assert!(!late.refreshing);
        let done = StateDto::new(
            &Shared {
                refreshing_since: None,
                ..shared
            },
            asked + STALE_FOR,
        );
        assert!(done.state_known);
        assert!(!done.refreshing);
    }

    #[test]
    fn a_turned_away_client_is_told_to_ask_again() {
        assert!(matches!(parse_answer(BUSY_ANSWER), Err(DaemonError::Busy)));
        // A state answer with an `error` field is still read by its version.
        let newer = format!(
            "{{\"version\":{},\"error\":\"busy\"}}",
            PROTOCOL_VERSION + 1
        );
        assert!(matches!(
            parse_answer(&newer),
            Err(DaemonError::ServiceNewer)
        ));
        assert!(matches!(
            parse_answer("{\"error\":\"unknown request\"}"),
            Err(DaemonError::BadAnswer(_))
        ));
    }

    /// Reads from one end of a socket pair, the other end fed by `feed`.
    fn read_from(feed: impl FnOnce(UnixStream), within: Duration) -> Result<String, DaemonError> {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        feed(theirs);
        read_answer(&mut ours, Instant::now() + within)
            .map_err(|err| answer_err(Path::new("test.sock"), err))
    }

    #[test]
    fn the_whole_answer_must_come_in_time() {
        let started = Instant::now();
        // Part of a line, then nothing: the deadline ends the wait.
        let kept_open = std::sync::Mutex::new(None);
        let slow = read_from(
            |mut theirs| {
                theirs.write_all(b"{\"version\"").unwrap();
                *kept_open.lock().unwrap() = Some(theirs);
            },
            Duration::from_millis(200),
        );
        assert!(matches!(slow, Err(DaemonError::NoAnswer)), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        // Hung up without a word (a busy service says so): most often a
        // service restarting.
        let dropped = read_from(drop, Duration::from_secs(1));
        assert!(matches!(dropped, Err(DaemonError::Dropped)), "{dropped:?}");
        let garbled = read_from(
            |mut theirs| theirs.write_all(b"\xff\n").unwrap(),
            Duration::from_secs(1),
        );
        assert!(
            matches!(garbled, Err(DaemonError::BadAnswer(_))),
            "{garbled:?}"
        );
        let whole = read_from(
            |mut theirs| theirs.write_all(b"{}\nmore\n").unwrap(),
            Duration::from_secs(1),
        );
        assert_eq!(whole.unwrap(), "{}\n");
    }

    #[test]
    fn a_turned_away_client_reads_busy_even_if_its_request_fails() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        // The service answers busy and hangs up before the request is sent.
        turn_away(theirs);
        let answer = exchange(&mut ours, Instant::now() + Duration::from_secs(1))
            .map_err(|err| answer_err(Path::new("test.sock"), err))
            .and_then(|line| parse_answer(&line));
        assert!(matches!(answer, Err(DaemonError::Busy)), "{answer:?}");
    }

    /// A directory of its own for a test, removed even if the test fails.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rcp2ctl-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn connecting_never_waits_for_a_service_that_does_not_accept() {
        let scratch = Scratch::new("connect");
        let path = scratch.0.join("board.sock");
        assert!(matches!(connect(&path), Err(DaemonError::NotRunning)));
        // A frozen service: it listens but never accepts, so its backlog
        // (one connection here) fills; then an attempt fails at once.
        let listener =
            socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        listener
            .bind(&socket2::SockAddr::unix(&path).unwrap())
            .unwrap();
        listener.listen(1).unwrap();
        let started = Instant::now();
        let mut held = Vec::new();
        let full = loop {
            match connect(&path) {
                Ok(stream) => held.push(stream),
                Err(err) => break err,
            }
            assert!(held.len() < 16, "the backlog never filled");
        };
        assert!(matches!(full, DaemonError::NoAnswer), "{full:?}");
        // A blocking connect would wait without limit: any bound shows it
        // did not, and a loose one keeps a loaded test machine from failing.
        assert!(started.elapsed() < QUERY_TIMEOUT);
        // Left behind by a killed service: nobody listens on it.
        drop(listener);
        assert!(matches!(connect(&path), Err(DaemonError::NotRunning)));
    }

    // Table S of `05-Etats-et-flux`: the service's rules, with the time given.

    fn secs(value: f64) -> Duration {
        Duration::from_secs_f64(value)
    }

    fn just_before(limit: Duration) -> Duration {
        limit.saturating_sub(secs(0.1))
    }

    /// A tree whose dump takes several reports, with one channel-like child.
    fn board_tree() -> Node {
        Node {
            name: "ROOT".to_owned(),
            properties: vec![("blob".to_owned(), Var::String("x".repeat(600)))],
            children: vec![Node {
                name: "CHANNEL".to_owned(),
                properties: vec![("level".to_owned(), Var::Int(1))],
                children: vec![],
            }],
        }
    }

    /// The reports of a dump, as the board sends them.
    fn dump_reports(root: &Node) -> Vec<Vec<u8>> {
        let mut message = vec![2];
        message.extend(rcp2_proto::encode_tree(root));
        let mut stream = u32::try_from(message.len()).unwrap().to_le_bytes().to_vec();
        stream.extend(message);
        stream
            .chunks(255)
            .map(|chunk| {
                let mut report = vec![rcp2_proto::PROPERTY_IN_REPORT_ID];
                report.extend_from_slice(chunk);
                report.resize(256, 0);
                report
            })
            .collect()
    }

    fn level(value: i32) -> Change {
        Change::PropertyChanged {
            path: vec![0],
            name: "level".to_owned(),
            value: Var::Int(value),
        }
    }

    fn served_level(shared: &SharedState) -> Option<Var> {
        let state = lock(shared);
        let tree = state.tree.as_ref()?;
        tree.children
            .first()?
            .properties
            .iter()
            .find(|(name, _)| name == "level")
            .map(|(_, value)| value.clone())
    }

    fn refreshing(shared: &SharedState, now: Instant) -> bool {
        StateDto::new(&lock(shared), now).refreshing
    }

    /// A session whose first dump was asked for at `t0` and is fully in at
    /// `t0` + 1 s.
    fn synced(t0: Instant) -> (Tracker, SharedState) {
        let shared = SharedState::new(std::sync::Mutex::new(Shared {
            connected: true,
            starting_since: Some(t0),
            ..Shared::default()
        }));
        let mut sync = Tracker::default();
        sync.start_request(&shared, t0);
        for report in dump_reports(&board_tree()) {
            sync.handle(&report, &shared, t0 + secs(1.0));
        }
        (sync, shared)
    }

    #[test]
    fn s2_s3_the_first_dump_ends_starting_and_is_served() {
        let t0 = Instant::now();
        let shared = SharedState::new(std::sync::Mutex::new(Shared {
            connected: true,
            starting_since: Some(t0),
            ..Shared::default()
        }));
        let mut sync = Tracker::default();
        sync.start_request(&shared, t0);
        let reading = StateDto::new(&lock(&shared), t0);
        assert!(reading.starting && !reading.state_known);
        for report in dump_reports(&board_tree()) {
            sync.handle(&report, &shared, t0 + secs(0.5));
        }
        let read = StateDto::new(&lock(&shared), t0 + secs(0.5));
        assert!(read.state_known && !read.refreshing && !read.starting);
        assert!(!sync.due(t0 + secs(30.0)));
    }

    #[test]
    fn s4_a_change_that_fits_is_applied() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        sync.change(level(7), &shared, t0 + secs(2.0));
        assert_eq!(served_level(&shared), Some(Var::Int(7)));
        assert!(!refreshing(&shared, t0 + secs(2.0)));
        assert!(!sync.due(t0 + secs(30.0)));
    }

    #[test]
    fn s5_a_change_that_does_not_fit_makes_the_shape_unsure() {
        let t0 = Instant::now();
        let unfit = [
            Change::Structural {
                kind: 3,
                path: vec![],
            },
            Change::PropertyChanged {
                path: vec![5],
                name: "level".to_owned(),
                value: Var::Int(3),
            },
        ];
        for change in unfit {
            let (mut sync, shared) = synced(t0);
            sync.change(change, &shared, t0 + secs(6.0));
            assert!(refreshing(&shared, t0 + secs(6.0)));
            // Nothing more is applied to a tree of unsure shape.
            sync.change(level(9), &shared, t0 + secs(6.0));
            assert_eq!(served_level(&shared), Some(Var::Int(1)));
            assert!(sync.due(t0 + secs(6.0)));
        }
        // A chunk that belongs to no dump: asked again once the board is
        // quiet (S14).
        let (mut sync, shared) = synced(t0);
        sync.handle(&stray(), &shared, t0 + secs(6.0));
        assert!(refreshing(&shared, t0 + secs(6.0)));
        assert!(sync.due(t0 + secs(6.0) + CHUNK_SILENCE));
    }

    /// A chunk that belongs to no dump.
    fn stray() -> Vec<u8> {
        let mut report = vec![rcp2_proto::PROPERTY_IN_REPORT_ID];
        report.extend([0xFF; 255]);
        report
    }

    #[test]
    fn s6_a_request_held_by_the_rate_limit_is_kept() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        let unfit = Change::Structural {
            kind: 3,
            path: vec![],
        };
        sync.change(unfit, &shared, t0 + secs(2.0));
        assert!(!sync.due(t0 + secs(2.0)));
        assert!(!sync.due(t0 + just_before(RESYNC_EVERY)));
        // Not forgotten: asked for once the limit allows.
        assert!(sync.due(t0 + RESYNC_EVERY));
    }

    #[test]
    fn s7_never_asked_again_while_chunks_arrive() {
        let t0 = Instant::now();
        let shared = SharedState::default();
        let mut sync = Tracker::default();
        sync.start_request(&shared, t0);
        let reports = dump_reports(&board_tree());
        assert!(reports.len() >= 3);
        // Slow but steady: each chunk well within the silence limit, the
        // whole dump past the rate limit.
        let step = CHUNK_SILENCE.mul_f64(0.9);
        let mut now = t0;
        for (position, report) in reports.iter().enumerate() {
            now += step;
            if position == 1 {
                // Changes lost mid-transfer: another dump is needed, but
                // only once this one is in.
                for value in 0..=i32::try_from(MAX_PENDING).unwrap() {
                    sync.change(level(value), &shared, now);
                }
            }
            assert!(!sync.due(now), "asked again mid-transfer");
            sync.handle(report, &shared, now);
        }
        assert!(now > t0 + RESYNC_EVERY);
        assert!(StateDto::new(&lock(&shared), now).state_known);
        assert!(sync.due(now));
    }

    #[test]
    fn s8_a_silent_dump_is_asked_again() {
        let t0 = Instant::now();
        let shared = SharedState::default();
        let mut sync = Tracker::default();
        sync.start_request(&shared, t0);
        // Nothing at all: asked again, within the rate limit.
        assert!(!sync.due(t0 + just_before(CHUNK_SILENCE)));
        assert!(!sync.due(t0 + just_before(RESYNC_EVERY)));
        assert!(sync.due(t0 + RESYNC_EVERY));
        // Chunks that stop.
        let reports = dump_reports(&board_tree());
        let first = reports.first().unwrap();
        let last_chunk = t0 + secs(4.5);
        sync.handle(first, &shared, last_chunk);
        assert!(!sync.due(last_chunk + just_before(CHUNK_SILENCE)));
        assert!(sync.due(last_chunk + CHUNK_SILENCE));
    }

    #[test]
    fn s9_changes_during_a_dump_go_to_the_new_tree() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        sync.start_request(&shared, t0 + secs(10.0));
        sync.change(level(4), &shared, t0 + secs(10.0));
        // Not applied to the tree being replaced.
        assert_eq!(served_level(&shared), Some(Var::Int(1)));
        for report in dump_reports(&board_tree()) {
            sync.handle(&report, &shared, t0 + secs(11.0));
        }
        assert_eq!(served_level(&shared), Some(Var::Int(4)));
        assert!(!refreshing(&shared, t0 + secs(11.0)));
        // One that does not fit the new tree: S5 on it.
        sync.start_request(&shared, t0 + secs(20.0));
        let unfit = Change::Structural {
            kind: 4,
            path: vec![],
        };
        sync.change(unfit, &shared, t0 + secs(20.0));
        sync.change(level(8), &shared, t0 + secs(20.0));
        for report in dump_reports(&board_tree()) {
            sync.handle(&report, &shared, t0 + secs(21.0));
        }
        assert!(refreshing(&shared, t0 + secs(21.0)));
        // Past the one that did not fit, the shape is unsure: none applied.
        assert_eq!(served_level(&shared), Some(Var::Int(1)));
        assert!(sync.due(t0 + secs(25.0)));
    }

    #[test]
    fn s5_s9_changes_lost_while_a_dump_arrives_need_another() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        sync.start_request(&shared, t0 + secs(10.0));
        for value in 0..=i32::try_from(MAX_PENDING).unwrap() {
            sync.change(level(value), &shared, t0 + secs(10.0));
        }
        for report in dump_reports(&board_tree()) {
            sync.handle(&report, &shared, t0 + secs(11.0));
        }
        assert!(refreshing(&shared, t0 + secs(11.0)));
        assert!(sync.due(t0 + secs(15.0)));
    }

    #[test]
    fn s1_starting_is_over_once_the_board_is_not_found() {
        let t0 = Instant::now();
        let shared = SharedState::new(std::sync::Mutex::new(Shared {
            starting_since: Some(t0),
            ..Shared::default()
        }));
        board_absent(&shared);
        let state = StateDto::new(&lock(&shared), t0);
        assert!(!state.connected && !state.starting);
    }

    #[test]
    fn s11_an_ended_session_serves_a_blank_state() {
        let t0 = Instant::now();
        let (_sync, shared) = synced(t0);
        lock(&shared).starting_since = Some(t0);
        session_ended(&shared);
        let state = StateDto::new(&lock(&shared), t0 + secs(1.0));
        assert!(!state.connected && !state.starting && !state.state_known);
    }

    #[test]
    fn s13_a_dump_not_asked_for_is_read_as_one_asked_for() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        let reports = dump_reports(&board_tree());
        let first = t0 + secs(10.0);
        sync.handle(reports.first().unwrap(), &shared, first);
        assert!(refreshing(&shared, first));
        // S8: it stops, it is asked for.
        assert!(!sync.due(first + just_before(CHUNK_SILENCE)));
        assert!(sync.due(first + CHUNK_SILENCE));
        // S7: changes lost while it arrives; asked again only once it is in.
        let (mut sync, shared) = synced(t0);
        let step = CHUNK_SILENCE.mul_f64(0.9);
        let mut now = first;
        for (position, report) in reports.iter().enumerate() {
            if position == 1 {
                for value in 0..=i32::try_from(MAX_PENDING).unwrap() {
                    sync.change(level(value), &shared, now);
                }
            }
            sync.handle(report, &shared, now);
            now += step;
            if position + 1 < reports.len() {
                assert!(!sync.due(now), "asked again mid-transfer");
            }
        }
        assert!(sync.due(now));
    }

    #[test]
    fn s14_stray_chunks_mean_the_board_is_still_sending() {
        let t0 = Instant::now();
        let (mut sync, shared) = synced(t0);
        let first = t0 + secs(10.0);
        let step = CHUNK_SILENCE.mul_f64(0.9);
        let mut now = first;
        for _ in 0..3 {
            sync.handle(&stray(), &shared, now);
            assert!(refreshing(&shared, now));
            assert!(!sync.due(now + just_before(CHUNK_SILENCE)));
            now += step;
        }
        now -= step;
        assert!(sync.due(now + CHUNK_SILENCE));
    }

    #[test]
    fn s10_s11_starting_and_refreshing_are_bounded() {
        let t0 = Instant::now();
        let shared = Shared {
            starting_since: Some(t0),
            ..Shared::default()
        };
        assert!(StateDto::new(&shared, t0 + secs(1.0)).starting);
        assert!(!StateDto::new(&shared, t0 + STALE_FOR).starting);
        // A session ended: the service starts again from a blank state.
        let ended = StateDto::new(&Shared::default(), t0);
        assert!(!ended.connected && !ended.starting && !ended.state_known);
    }

    #[test]
    fn s12_a_second_instance_refuses_to_start() {
        let scratch = Scratch::new("bind");
        let path = scratch.0.join("board.sock");
        let first = bind(&path).unwrap();
        assert!(matches!(bind(&path), Err(DaemonError::AlreadyRunning(_))));
        // The first one is untouched.
        assert!(connect(&path).is_ok());
        // Killed: its socket stays, its lock goes; the next one starts.
        drop(first);
        assert!(path.exists());
        let _again = bind(&path).unwrap();
        // An instance older than the lock: it holds none, but listens.
        let old = scratch.0.join("old").join("board.sock");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        let _old_instance = std::os::unix::net::UnixListener::bind(&old).unwrap();
        assert!(matches!(bind(&old), Err(DaemonError::AlreadyRunning(_))));
        assert!(connect(&old).is_ok());
        // No directory to place it in: refused, nothing made at the root.
        assert!(matches!(
            bind(Path::new("/")),
            Err(DaemonError::Io { ref source, .. }) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        // A file that is not a socket is not ours to remove.
        let other = scratch.0.join("other").join("board.sock");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "x").unwrap();
        assert!(matches!(bind(&other), Err(DaemonError::AlreadyRunning(_))));
    }
}
