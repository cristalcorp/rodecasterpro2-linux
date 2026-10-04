//! `rcp2ctl`: Linux control tool for the RØDECaster Pro II.

mod daemon;
mod keeper;
mod service;
mod settings;
mod tui;

use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use rcp2_audio::{
    APP_DIR, Channel, DetectError, DisableOutcome, EnableOutcome, Graph, PersistPaths,
    PersistState, PwError, UnsafeNodeName, create_runtime_outputs, data_home, disable_persistence,
    enable_persistence, move_stream, original_recorded, persist_state, pipewire_config,
    refuse_symlink, remove_file_if_present, remove_runtime_outputs, set_default_sink, snapshot,
};
use service::UnitState;
use settings::{Settings, SettingsError};

/// Linux control tool for the RØDECaster Pro II (unofficial).
///
/// At every launch the named outputs ("RØDE Game", "RØDE Music"…) are created
/// if missing, without writing any file, unless turned off with `outputs off`.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Without a command, opens the interactive interface.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the board, the named outputs and how they are kept.
    Status,
    /// Print the PipeWire configuration declaring the named outputs (for packagers).
    Config,
    /// List applications playing audio and where they play.
    ///
    /// Several applications can share a binary (Wine, Electron): route a single
    /// stream by its ID.
    Apps,
    /// Send an application to a named output (remembered for its next runs).
    Route {
        /// Stream ID, process binary or application name, as listed by `rcp2ctl apps`.
        app: String,
        /// Named output (an invalid value lists the valid ones).
        channel: Channel,
    },
    /// Turn the named outputs on or off (remembered for the next launches).
    Outputs {
        /// `on` creates them now and at each launch; `off` removes them.
        state: Switch,
    },
    /// Keep the named outputs after a reboot even without launching rcp2ctl.
    ///
    /// `on` installs a PipeWire config file, after recording what was there;
    /// `off` restores exactly that original state.
    Persist {
        /// `on` or `off`.
        state: Switch,
    },
    /// Make a named output the system's default output (remembered by WirePlumber).
    Default {
        /// Named output.
        channel: Channel,
    },
    /// Show the board's own state (channels, mutes, faders), read from the
    /// board service.
    Board,
    /// Run the board service in the foreground (normally started by systemd,
    /// see `rcp2ctl hid setup`). Keeps the control session read, so the board's
    /// faders keep working, and serves its state to the other commands.
    Daemon,
    /// Board control interface (HID): read-only diagnostics.
    Hid {
        #[command(subcommand)]
        command: HidCommand,
    },
    /// Undo everything rcp2ctl did, before removing the binary.
    Uninstall {
        /// Restore the original PipeWire configuration without asking.
        #[arg(long)]
        yes: bool,
        /// Keep the PipeWire config file without asking.
        #[arg(long, conflicts_with = "yes")]
        keep_pipewire_config: bool,
    },
}

#[derive(Subcommand)]
enum HidCommand {
    /// Give your user access to the board's control interface (installs a udev
    /// rule with sudo; `rcp2ctl uninstall` removes it).
    Setup,
    /// Show which hidraw node is the board's control interface (opens nothing).
    Find,
    /// Handshake with the board and record what it sends (read-only), to study
    /// the protocol. The file contains the board's serial number: keep it private.
    Capture {
        /// Output file, must end in `.rcp2cap` (ignored by git).
        file: PathBuf,
        /// Stop after this many seconds at most (1 to 60).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
        seconds: u64,
        /// Do not ask for confirmation (the faders still freeze until a replug).
        #[arg(long)]
        yes: bool,
    },
    /// Decode a capture file offline and show what it says about the board
    /// (firmware, channels, mutes, faders). Never prints the serial number.
    Decode {
        /// A `.rcp2cap` file written by `rcp2ctl hid capture`.
        file: PathBuf,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Switch {
    On,
    Off,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Pw(#[from] PwError),
    #[error(transparent)]
    Detect(#[from] DetectError),
    #[error(transparent)]
    UnsafeNodeName(#[from] UnsafeNodeName),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("no application matches `{0}` (see `rcp2ctl apps`)")]
    NoSuchApp(String),
    #[error("could not move stream(s) {}: {first}", join_ids(.failed))]
    RouteFailed {
        /// IDs of the streams that were not moved.
        failed: Vec<u32>,
        /// Error of the first failed move.
        first: PwError,
    },
    #[error("the \"{0}\" output does not exist: run `rcp2ctl outputs on`")]
    OutputMissing(&'static str),
    #[error("the \"{0}\" output has no object.serial; cannot route to it")]
    NoSerial(&'static str),
    #[error(
        "the named outputs are kept by the PipeWire config file: run `rcp2ctl persist off` first"
    )]
    PersistentOutputs,
    #[error("uninstall finished with {0} problem(s), reported above")]
    UninstallIncomplete(usize),
    #[error("the interactive interface needs a terminal; see `rcp2ctl --help` for commands")]
    NoTerminal,
    #[error(transparent)]
    Hid(#[from] rcp2_hid::HidError),
    #[error(
        "capture files must end in `.{CAPTURE_EXTENSION}` (git ignores them: they hold the serial number)"
    )]
    CaptureExtension,
    #[error("{path}: {source}")]
    File { path: PathBuf, source: io::Error },
    #[error("capture interrupted after {0} report(s), saved anyway: {1}")]
    CaptureInterrupted(usize, rcp2_hid::HidError),
    #[error("{0}: not a valid capture file (line {1})")]
    BadCapture(PathBuf, usize),
    #[error("the capture holds no complete state dump ({0})")]
    NoDump(String),
    #[error(transparent)]
    Daemon(#[from] daemon::DaemonError),
    #[error("{0} exists but was not written by rcp2ctl: left untouched")]
    ForeignFile(PathBuf),
    #[error("the binary path {0} contains characters systemd cannot take; move the binary")]
    UnsafeExePath(PathBuf),
    #[error("`{0}` failed")]
    CommandFailed(String),
    #[error("cannot write output: {0}")]
    Output(#[from] io::Error),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = io::stdout().lock();
    let result = match cli.command {
        Some(command) => run(command, &mut out),
        None if io::stdin().is_terminal() && io::stdout().is_terminal() => tui::run(),
        None => Err(CliError::NoTerminal),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The reader went away (e.g. `rcp2ctl apps | head -1`): not an error.
        Err(CliError::Output(err)) if err.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            // Nothing sensible is left to do if stderr itself is unwritable.
            let _ = writeln!(io::stderr().lock(), "rcp2ctl: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, out: &mut impl Write) -> Result<(), CliError> {
    // Settings are loaded only by the commands that use them, so `config` and
    // `uninstall` work even when they are unreadable.
    match command {
        Command::Config => config(&snapshot()?, out),
        Command::Uninstall {
            yes,
            keep_pipewire_config,
        } => uninstall(yes, keep_pipewire_config, out),
        Command::Outputs { state } => {
            let path = settings::settings_path()?;
            outputs(
                &path,
                &mut settings::load(&path)?,
                state,
                out,
                &mut notice_to_stderr,
            )
        }
        // Read-only: shows missing outputs instead of creating them.
        Command::Status => status(&snapshot()?, &load_settings()?, out),
        Command::Apps => apps(&prepare(&load_settings()?)?, out),
        Command::Route { app, channel } => route(&prepare(&load_settings()?)?, &app, channel, out),
        Command::Default { channel } => set_default(&prepare(&load_settings()?)?, channel, out),
        Command::Hid { command } => hid(command, out),
        Command::Board => board(out),
        Command::Daemon => daemon::run(&mut io::stderr()).map_err(Into::into),
        Command::Persist { state } => {
            let settings = load_settings()?;
            persist(&prepare(&settings)?, &settings, state, out)
        }
    }
}

fn load_settings() -> Result<Settings, CliError> {
    Ok(settings::load(&settings::settings_path()?)?)
}

fn outputs(
    settings_path: &Path,
    settings: &mut Settings,
    state: Switch,
    out: &mut impl Write,
    notice: &mut dyn FnMut(&str),
) -> Result<(), CliError> {
    if state == Switch::Off && persist_state(&PersistPaths::from_env()?)? == PersistState::On {
        return Err(CliError::PersistentOutputs);
    }
    // Held until the outputs match the saved setting: the board service
    // cannot recreate them in between.
    let _lock = outputs_lock()?;
    // Saved first: the in-memory settings only change once the file has.
    let updated = Settings {
        outputs: state == Switch::On,
    };
    settings::save(settings_path, &updated)?;
    *settings = updated;
    if state == Switch::On {
        for text in create_missing_outputs(settings)?.notices() {
            notice(&text);
        }
        writeln!(out, "Named outputs on (created at each launch).")?;
        return Ok(());
    }
    let removed = remove_runtime_outputs()?;
    writeln!(
        out,
        "Named outputs off ({} removed). Apps that played on them move to the default output.",
        removed.len()
    )?;
    Ok(())
}

/// Prints a notice on stderr, keeping command output data only.
fn notice_to_stderr(text: &str) {
    // Nothing sensible is left to do if stderr itself is unwritable.
    let _ = writeln!(io::stderr().lock(), "rcp2ctl: {text}");
}

/// How long to wait for outputs created through `pactl` to show up in the graph.
const APPEAR_ATTEMPTS: u32 = 20;
const APPEAR_INTERVAL: Duration = Duration::from_millis(50);

/// Restores the application's state at launch: creates the named outputs that
/// are missing if they are on. Returns an up-to-date graph. Notices go to
/// stderr so that command output stays data only.
fn prepare(settings: &Settings) -> Result<Graph, CliError> {
    let restored = restore_outputs(settings)?;
    for notice in restored.notices() {
        notice_to_stderr(&notice);
    }
    Ok(restored.graph)
}

/// Outcome of [`restore_outputs`].
struct Restored {
    graph: Graph,
    created: Vec<Channel>,
    /// Some created outputs were not visible yet when the wait ended.
    pending: bool,
}

impl Restored {
    fn notices(&self) -> Vec<String> {
        let mut notices = Vec::new();
        if !self.created.is_empty() {
            let names: Vec<_> = self.created.iter().map(|channel| channel.label()).collect();
            notices.push(format!("created named outputs: {}", names.join(", ")));
        }
        if self.pending {
            notices.push("the new outputs are not visible yet; retry in a moment".to_owned());
        }
        notices
    }
}

/// Serialises changes to the named outputs between processes (the board
/// service and the commands): otherwise two of them could create the same
/// output twice, or the service could bring them back right after `outputs
/// off`. Released when dropped. Without a runtime directory, nothing is
/// locked.
fn outputs_lock() -> Result<Option<std::fs::File>, CliError> {
    use std::os::unix::fs::DirBuilderExt as _;
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        return Ok(None);
    };
    let dir = runtime.join(APP_DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(file_err(&dir))?;
    let path = dir.join("outputs.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(file_err(&path))?;
    file.lock().map_err(file_err(&path))?;
    Ok(Some(file))
}

/// Creates the missing named outputs if they are on, without printing
/// anything (the TUI owns the terminal).
fn restore_outputs(settings: &Settings) -> Result<Restored, CliError> {
    let _lock = outputs_lock()?;
    create_missing_outputs(settings)
}

/// [`restore_outputs`], for a caller already holding [`outputs_lock`].
fn create_missing_outputs(settings: &Settings) -> Result<Restored, CliError> {
    let graph = snapshot()?;
    let unchanged = |graph| Restored {
        graph,
        created: Vec::new(),
        pending: false,
    };
    if !settings.outputs {
        return Ok(unchanged(graph));
    }
    // Without the board there is nothing to create: commands report it themselves.
    let Ok(rode) = graph.rode() else {
        return Ok(unchanged(graph));
    };
    let created = create_runtime_outputs(&graph, &rode)?;
    if created.is_empty() {
        return Ok(unchanged(graph));
    }
    // pactl returns before the new nodes are visible to other clients.
    let all_visible = |graph: &Graph| {
        created
            .iter()
            .all(|channel| graph.virtual_sink(*channel).is_some())
    };
    let mut graph = snapshot()?;
    for _ in 0..APPEAR_ATTEMPTS {
        if all_visible(&graph) {
            break;
        }
        std::thread::sleep(APPEAR_INTERVAL);
        graph = snapshot()?;
    }
    let pending = !all_visible(&graph);
    Ok(Restored {
        graph,
        created,
        pending,
    })
}

fn status(graph: &Graph, settings: &Settings, out: &mut impl Write) -> Result<(), CliError> {
    // Without the board, the rest is still worth showing.
    let board_present = match graph.rode() {
        Ok(rode) => {
            writeln!(out, "RØDECaster Pro II found")?;
            writeln!(out, "  native stereo sink: {}", rode.stereo_sink)?;
            writeln!(out, "  native multi sink:  {}", rode.multi_sink)?;
            true
        }
        Err(DetectError::NotFound) => {
            writeln!(
                out,
                "RØDECaster Pro II: not found (is it plugged in and powered on?)"
            )?;
            false
        }
        Err(err) => {
            writeln!(out, "RØDECaster Pro II: {err}")?;
            false
        }
    };
    let paths = PersistPaths::from_env()?;
    let persisted = persist_state(&paths)?;
    let mode = match (persisted, settings.outputs) {
        (PersistState::On, _) => {
            "on, kept by the PipeWire config file (`rcp2ctl persist off` to undo)"
        }
        (_, true) => "on, created at each launch, no file written",
        (_, false) => "off (`rcp2ctl outputs on`)",
    };
    writeln!(out, "Named outputs: {mode}")?;
    let mut missing = false;
    for channel in Channel::ALL {
        let state = if graph.virtual_sink(channel).is_some() {
            "present"
        } else {
            missing = true;
            "absent"
        };
        writeln!(
            out,
            "  {:<6} {:<12} {state}",
            channel.id(),
            channel.sink_name()
        )?;
    }
    let unit = service::unit_path().and_then(|path| service::unit_state(&path));
    if missing && settings.outputs {
        let hint = match (board_present, &unit) {
            (true, _) => {
                "Missing outputs come back with `rcp2ctl outputs on` (or any other command)."
            }
            (false, Ok(UnitState::Ours)) => {
                "Missing outputs come back on their own once the board is back (board service)."
            }
            (false, _) => {
                "Missing outputs come back with `rcp2ctl outputs on` once the board is back."
            }
        };
        writeln!(out, "{hint}")?;
    }
    let service = match unit {
        Ok(UnitState::Ours) => "installed, recreates the outputs when the board comes back",
        Ok(UnitState::Outdated) => {
            "installed by an older version, run `rcp2ctl hid setup` to update it"
        }
        Ok(UnitState::Absent) => "not installed (`rcp2ctl hid setup`)",
        Ok(UnitState::Foreign) | Err(_) => "unknown",
    };
    writeln!(out, "Board service: {service}")?;
    if persisted == PersistState::Foreign {
        writeln!(
            out,
            "Note: {} exists but was not written by rcp2ctl; persistence will not touch it.",
            paths.config_file.display()
        )?;
    }
    Ok(())
}

fn config(graph: &Graph, out: &mut impl Write) -> Result<(), CliError> {
    out.write_all(pipewire_config(&graph.rode()?)?.as_bytes())?;
    Ok(())
}

fn persist(
    graph: &Graph,
    settings: &Settings,
    state: Switch,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let paths = PersistPaths::from_env()?;
    let file = paths.config_file.display();
    let after_restart = if settings.outputs {
        "rcp2ctl recreates them at each launch"
    } else {
        "named outputs are off, so rcp2ctl will not recreate them (`rcp2ctl outputs on`)"
    };
    let undo = "Undo: `r` in the TUI or `rcp2ctl persist off` (restores the original exactly).";
    match state {
        Switch::On => {
            let contents = pipewire_config(&graph.rode()?)?;
            // A file we did not write is taken over only after being saved.
            let saved = if persist_state(&paths)? == PersistState::Foreign {
                " (your previous file was saved and comes back on restore)"
            } else {
                ""
            };
            match enable_persistence(&paths, &contents)? {
                EnableOutcome::Unchanged => {
                    writeln!(out, "Outputs already kept after a reboot ({file}).")?;
                }
                EnableOutcome::Updated => writeln!(out, "Updated {file}.")?,
                EnableOutcome::Enabled => writeln!(
                    out,
                    "Outputs now kept after a reboot: wrote {file}{saved}. The current \
                     outputs keep working. {undo}"
                )?,
            }
        }
        Switch::Off => match disable_persistence(&paths)? {
            DisableOutcome::AlreadyOff => {
                writeln!(
                    out,
                    "The original PipeWire configuration is already in place."
                )?;
            }
            DisableOutcome::Restored => writeln!(
                out,
                "Original PipeWire configuration restored ({file}). After the next \
                 PipeWire restart, {after_restart}."
            )?,
            DisableOutcome::Removed => writeln!(
                out,
                "Original PipeWire configuration restored: removed {file} (there was none). \
                 The current outputs keep working until PipeWire restarts; then \
                 {after_restart}."
            )?,
        },
    }
    Ok(())
}

/// Asks a yes/no question, `default` on an empty answer or without a terminal.
fn ask(question: &str, default: bool, out: &mut impl Write) -> Result<bool, CliError> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        writeln!(
            out,
            "{question} {hint} -> {} (no terminal, default)",
            if default { "yes" } else { "no" }
        )?;
        return Ok(default);
    }
    let mut lines = stdin.lock().lines();
    loop {
        write!(out, "{question} {hint} ")?;
        out.flush()?;
        let Some(line) = lines.next().transpose()? else {
            return Ok(default);
        };
        match line.trim().to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" | "o" | "oui" => return Ok(true),
            "n" | "no" | "non" => return Ok(false),
            _ => {}
        }
    }
}

/// Removes the udev rule if `hid setup` installed it. Returns the problem to
/// report, if any; only a terminal write error stops it.
fn remove_udev_rule(out: &mut impl Write) -> Result<Option<String>, CliError> {
    let state = match read_rule() {
        Ok(text) => rcp2_hid::rule_state(text.as_deref()),
        Err(err) => return Ok(Some(format!("could not check the udev rule: {err}"))),
    };
    if !matches!(
        state,
        rcp2_hid::RuleState::UpToDate | rcp2_hid::RuleState::Outdated
    ) {
        return Ok(None);
    }
    writeln!(
        out,
        "Removing {} (sudo may ask for your password).",
        rcp2_hid::UDEV_RULE_PATH
    )?;
    match sudo(&["rm", "-f", rcp2_hid::UDEV_RULE_PATH]).and_then(|()| reload_udev()) {
        Ok(()) => {
            writeln!(
                out,
                "Board access rule removed (fully effective after the next replug or login)."
            )?;
            Ok(None)
        }
        Err(err) => Ok(Some(format!("could not remove the udev rule: {err}"))),
    }
}

fn uninstall(yes: bool, keep_pipewire_config: bool, out: &mut impl Write) -> Result<(), CliError> {
    // Goes as far as possible: each failed step is reported and the rest still runs.
    let mut problems = 0;
    let mut warn = |out: &mut dyn Write, message: String| -> io::Result<()> {
        problems += 1;
        writeln!(out, "Warning: {message}")
    };

    let paths = PersistPaths::from_env()?;
    // Also when our file was edited or deleted by hand: a recorded original
    // must not be left behind silently.
    let recorded = original_recorded(&paths).unwrap_or(true);
    let concerned = recorded || matches!(persist_state(&paths), Ok(PersistState::On));
    if concerned {
        let restore = yes
            || (!keep_pipewire_config
                && ask("Restore the original PipeWire configuration?", true, out)?);
        if restore {
            match disable_persistence(&paths) {
                Ok(DisableOutcome::Restored) => {
                    writeln!(out, "Original PipeWire configuration restored.")?;
                }
                Ok(DisableOutcome::Removed | DisableOutcome::AlreadyOff) => writeln!(
                    out,
                    "PipeWire configuration back to its original state (no file)."
                )?,
                Err(PwError::ModifiedOutside(path) | PwError::Symlink(path)) => writeln!(
                    out,
                    "{} was changed by hand or is managed elsewhere: left untouched. The \
                     original is kept in {}.",
                    path.display(),
                    paths.snapshot_dir.display()
                )?,
                Err(err) => warn(out, format!("could not restore the original: {err}"))?,
            }
        } else if paths.config_file.exists() {
            writeln!(
                out,
                "Kept {} (delete it to remove the outputs for good). The record of the \
                 original state stays in {}.",
                paths.config_file.display(),
                paths.snapshot_dir.display()
            )?;
        } else {
            writeln!(
                out,
                "The original PipeWire configuration was not put back; it stays in {}.",
                paths.snapshot_dir.display()
            )?;
        }
    }

    // The board service: once it stops, nobody reads the board any more, so
    // its faders freeze until it is replugged (I-007).
    match service::remove() {
        Ok(true) => writeln!(
            out,
            "Board service stopped and removed. Unplug and replug the board's USB cable \
             so that its faders work again."
        )?,
        Ok(false) => {}
        Err(err) => warn(out, format!("could not remove the board service: {err}"))?,
    }

    // The udev rule from `rcp2ctl hid setup`, only if it is ours.
    if let Some(problem) = remove_udev_rule(out)? {
        warn(out, problem)?;
    }

    match remove_runtime_outputs() {
        Ok(removed) => writeln!(
            out,
            "Removed {} named output(s) created at runtime.",
            removed.len()
        )?,
        Err(err) => warn(
            out,
            format!(
                "could not remove the runtime named outputs ({err}); they vanish at the \
                 next PipeWire restart anyway"
            ),
        )?,
    }

    // Only what rcp2ctl created: never a recursive delete, never a managed symlink.
    let settings_path = settings::settings_path()?;
    match refuse_symlink(&settings_path).and_then(|()| remove_file_if_present(&settings_path)) {
        Ok(()) => writeln!(out, "rcp2ctl settings removed.")?,
        Err(PwError::Symlink(path)) => writeln!(
            out,
            "Left {} in place (a symlink managed elsewhere).",
            path.display()
        )?,
        Err(err) => warn(out, format!("could not remove the settings: {err}"))?,
    }
    if let Some(dir) = settings_path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    // Non-recursive: a kept record of the original stays.
    let _ = std::fs::remove_dir(data_home()?.join(APP_DIR));

    match std::env::current_exe() {
        Ok(exe) => writeln!(
            out,
            "Last step: remove the binary itself ({}).",
            exe.display()
        )?,
        Err(_) => writeln!(out, "Last step: remove the rcp2ctl binary itself.")?,
    }
    if problems > 0 {
        return Err(CliError::UninstallIncomplete(problems));
    }
    Ok(())
}

/// The named output a node plays on: one of our sinks, or the board's native
/// stereo sink, which is the Chat channel too.
fn channel_of(graph: &Graph, node: &rcp2_audio::Node) -> Option<Channel> {
    Channel::from_sink_name(&node.name).or_else(|| {
        graph
            .rode()
            .ok()
            .filter(|rode| rode.stereo_sink == node.name)
            .map(|_| Channel::Chat)
    })
}

/// Name to show for a node an application plays to: the channel's label for
/// the board's outputs, the node description otherwise.
fn target_label(graph: &Graph, node: &rcp2_audio::Node) -> String {
    channel_of(graph, node).map_or_else(
        || {
            node.description
                .clone()
                .unwrap_or_else(|| node.name.clone())
        },
        Channel::description,
    )
}

/// The OUTPUT column of the applications list, shared by the CLI and the TUI.
fn output_label(graph: &Graph, stream: &rcp2_audio::AppStream<'_>) -> String {
    if stream.targets.is_empty() {
        return "(not connected)".to_owned();
    }
    stream
        .targets
        .iter()
        .map(|node| target_label(graph, node))
        .collect::<Vec<_>>()
        .join(", ")
}

fn apps(graph: &Graph, out: &mut impl Write) -> Result<(), CliError> {
    let streams = graph.app_streams();
    if streams.is_empty() {
        writeln!(out, "No application is playing audio.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:>6}  {:<16}  {:<24}  {:<24}  MEDIA",
        "ID", "BINARY", "APPLICATION", "OUTPUT"
    )?;
    for stream in &streams {
        let output = output_label(graph, stream);
        writeln!(
            out,
            "{:>6}  {:<16}  {:<24}  {:<24}  {}",
            stream.node.id,
            stream.node.process_binary.as_deref().unwrap_or("-"),
            stream.app_label(),
            output,
            stream.node.media_name.as_deref().unwrap_or("")
        )?;
    }
    Ok(())
}

fn route(
    graph: &Graph,
    selector: &str,
    channel: Channel,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let sink = graph
        .virtual_sink(channel)
        .ok_or(CliError::OutputMissing(channel.label()))?;
    let serial = sink.serial.ok_or(CliError::NoSerial(channel.label()))?;
    let streams: Vec<_> = graph
        .app_streams()
        .into_iter()
        .filter(|stream| stream.matches(selector))
        .collect();
    if streams.is_empty() {
        return Err(CliError::NoSuchApp(selector.to_owned()));
    }
    // Try every stream before printing, so neither a failed move nor a closed
    // stdout leaves the others unrouted.
    let mut moved = Vec::with_capacity(streams.len());
    let mut failed = Vec::new();
    let mut first = None;
    for stream in &streams {
        match move_stream(stream.node.id, serial) {
            Ok(()) => moved.push(stream),
            Err(err) => {
                failed.push(stream.node.id);
                first.get_or_insert(err);
            }
        }
    }
    let printed = moved.iter().try_for_each(|stream| {
        writeln!(
            out,
            "{} [{}] (stream {}) -> {}",
            stream.app_label(),
            stream.node.process_binary.as_deref().unwrap_or("-"),
            stream.node.id,
            channel.description()
        )
    });
    // A routing failure outranks an output error (which may be a benign broken pipe).
    if let Some(first) = first {
        return Err(CliError::RouteFailed { failed, first });
    }
    printed?;
    Ok(())
}

fn set_default(graph: &Graph, channel: Channel, out: &mut impl Write) -> Result<(), CliError> {
    let sink = graph
        .virtual_sink(channel)
        .ok_or(CliError::OutputMissing(channel.label()))?;
    set_default_sink(sink.id)?;
    writeln!(
        out,
        "{} is now the default output (remembered by WirePlumber).",
        channel.description()
    )?;
    Ok(())
}

/// Extension of capture files, ignored by git.
const CAPTURE_EXTENSION: &str = "rcp2cap";
/// A capture ends once the board stays quiet this long after the burst.
const CAPTURE_IDLE: Duration = Duration::from_millis(500);

fn hid(command: HidCommand, out: &mut impl Write) -> Result<(), CliError> {
    use std::fmt::Write as _;
    let sys_class = Path::new(rcp2_hid::SYS_CLASS_HIDRAW);
    match command {
        HidCommand::Setup => hid_setup(out),
        HidCommand::Decode { file } => hid_decode(&file, out),
        HidCommand::Find => {
            for path in rcp2_hid::find_devices(sys_class)? {
                writeln!(out, "{}", path.display())?;
            }
            Ok(())
        }
        HidCommand::Capture { file, seconds, yes } => {
            if file.extension().and_then(|ext| ext.to_str()) != Some(CAPTURE_EXTENSION) {
                return Err(CliError::CaptureExtension);
            }
            // Known effect on Linux (I-007): once subscribed, the board's faders
            // stop working when nobody reads its HID interface, until a replug.
            writeln!(
                out,
                "Warning: after this capture the board's faders stop controlling the volume \
                 until you unplug and replug its USB cable (nothing is damaged). Do not run \
                 it during a recording or a live show."
            )?;
            if !yes && !ask("Capture now?", false, out)? {
                writeln!(out, "Nothing sent to the board.")?;
                return Ok(());
            }
            // Create the file first: never handshake for nothing, never overwrite.
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .map_err(file_err(&file))?;
            let captured = rcp2_hid::find_device(sys_class)
                .and_then(|path| rcp2_hid::Board::open(&path))
                .and_then(|mut board| {
                    rcp2_hid::capture(&mut board, Duration::from_secs(seconds), CAPTURE_IDLE)
                });
            let captured = match captured {
                Ok(captured) => captured,
                Err(err) => {
                    // Nothing was recorded: do not leave an empty file behind.
                    drop(output);
                    let _ = std::fs::remove_file(&file);
                    return Err(err.into());
                }
            };
            let reports = &captured.reports;
            let mut text = String::from("# rcp2 capture v1: <microseconds> <report hex>\n");
            for report in reports {
                // Writing to a String cannot fail.
                let _ = write!(text, "{} ", report.at.as_micros());
                for byte in &report.bytes {
                    let _ = write!(text, "{byte:02x}");
                }
                text.push('\n');
            }
            output.write_all(text.as_bytes()).map_err(file_err(&file))?;
            summarise_capture(reports, &file, out)?;
            match captured.error {
                Some(err) => Err(CliError::CaptureInterrupted(reports.len(), err)),
                None => Ok(()),
            }
        }
    }
}

/// Reads the reports of a capture file (`<microseconds> <hex>` per line).
fn read_capture(file: &Path) -> Result<Vec<Vec<u8>>, CliError> {
    let text = std::fs::read_to_string(file).map_err(file_err(file))?;
    let mut reports = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let bad = || CliError::BadCapture(file.to_owned(), number + 1);
        let hex = line.split_whitespace().nth(1).ok_or_else(bad)?;
        if hex.len() % 2 != 0 {
            return Err(bad());
        }
        let report = (0..hex.len())
            .step_by(2)
            .map(|at| {
                hex.get(at..at + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            })
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(bad)?;
        reports.push(report);
    }
    Ok(reports)
}

fn hid_decode(file: &Path, out: &mut impl Write) -> Result<(), CliError> {
    use rcp2_proto::{BoardState, DumpAssembler, Incoming, classify};
    let reports = read_capture(file)?;
    let mut assembler = DumpAssembler::default();
    let (mut acks, mut changes, mut unknown) = (0, 0, 0);
    let mut root = None;
    let mut last_error = None;
    for report in &reports {
        match classify(report) {
            Incoming::Ack => acks += 1,
            Incoming::Change(_) => changes += 1,
            Incoming::Unknown => unknown += 1,
            // A bad dump is skipped: a complete one may follow in the file.
            Incoming::DumpChunk(chunk) => match assembler.push(chunk) {
                Ok(Some(tree)) => {
                    root.get_or_insert(tree);
                }
                Ok(None) => {}
                Err(err) => {
                    assembler = DumpAssembler::default();
                    last_error = Some(err.to_string());
                }
            },
        }
    }
    let root = root.ok_or_else(|| {
        CliError::NoDump(last_error.unwrap_or_else(|| "dump incomplete".to_owned()))
    })?;
    let state = BoardState::from_tree(&root);
    writeln!(
        out,
        "{} report(s): {acks} acknowledgement(s), {changes} notification(s), {unknown} other",
        reports.len()
    )?;
    writeln!(out, "Nodes under the root: {}", root.children.len())?;
    let channels: Vec<_> = state
        .channels
        .iter()
        .map(|channel| (channel.index, channel.source.to_string(), channel.muted))
        .collect();
    print_board(out, state.firmware.as_deref(), &channels, &state.faders)
}

/// Runs `program <args>` in the terminal (`sudo` may ask for the password).
pub(crate) fn run_command(program: &str, args: &[&str]) -> Result<(), CliError> {
    let shown = format!("{program} {}", args.join(" "));
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|_| CliError::CommandFailed(shown.clone()))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::CommandFailed(shown))
    }
}

fn sudo(args: &[&str]) -> Result<(), CliError> {
    run_command("sudo", args)
}

/// The installed rule, `None` if there is none. Any other read error is an
/// error: an unreadable file must not be mistaken for an absent one.
fn read_rule() -> Result<Option<String>, CliError> {
    match std::fs::read_to_string(rcp2_hid::UDEV_RULE_PATH) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CliError::File {
            path: PathBuf::from(rcp2_hid::UDEV_RULE_PATH),
            source,
        }),
    }
}

fn file_err(path: &Path) -> impl FnOnce(io::Error) -> CliError + use<> {
    let path = path.to_owned();
    move |source| CliError::File { path, source }
}

/// Makes udev apply the rules again to the board, without replugging it.
fn reload_udev() -> Result<(), CliError> {
    sudo(&["udevadm", "control", "--reload"])?;
    sudo(&[
        "udevadm",
        "trigger",
        "--subsystem-match=hidraw",
        "--action=change",
    ])
}

/// Installs and starts the board service for this binary.
fn install_service(out: &mut impl Write) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(file_err(Path::new("/proc/self/exe")))?;
    match service::install(&exe) {
        Ok(unit) => writeln!(
            out,
            "Board service running ({}): it keeps the board's faders working and serves its \
             state. `rcp2ctl uninstall` removes it.",
            unit.display()
        )?,
        // Access itself is granted: say what is missing instead of failing.
        Err(err) => writeln!(
            out,
            "Board access is set up, but the board service could not be installed ({err}). \
             Without a systemd user session, run `rcp2ctl daemon` yourself while you use \
             the board."
        )?,
    }
    Ok(())
}

/// How a channel's output state is shown, in the CLI and the TUI.
pub(crate) const fn mute_label(muted: Option<bool>) -> &'static str {
    match muted {
        Some(true) => "muted",
        Some(false) => "on",
        None => "?",
    }
}

/// Shown while the board service has no state to give.
pub(crate) const BOARD_NOT_CONNECTED: &str = "{BOARD_NOT_CONNECTED}";
/// Shown while the board service reads the board's state.
pub(crate) const BOARD_BEING_READ: &str = "Board connected; its state is being read.";

/// Prints a board state: shared by `board` (from the service) and `hid decode`.
fn print_board(
    out: &mut impl Write,
    firmware: Option<&str>,
    channels: &[(usize, String, Option<bool>)],
    faders: &[Option<i32>],
) -> Result<(), CliError> {
    writeln!(out, "Firmware: {}", firmware.unwrap_or("(not reported)"))?;
    writeln!(out, "Channels (tree index, source, output):")?;
    for (index, source, muted) in channels {
        writeln!(out, "  0x{index:03x}  {source:<10}  {}", mute_label(*muted))?;
    }
    let faders: Vec<String> = faders
        .iter()
        .map(|level| level.map_or_else(|| "?".to_owned(), |level| level.to_string()))
        .collect();
    writeln!(out, "Faders (0-127): {}", faders.join(" "))?;
    Ok(())
}

fn board(out: &mut impl Write) -> Result<(), CliError> {
    let state = daemon::query_state()?;
    if !state.connected {
        writeln!(
            out,
            "Board service running, but the board is not connected."
        )?;
        return Ok(());
    }
    if !state.state_known {
        writeln!(out, "{BOARD_BEING_READ} Try again in a moment.")?;
        return Ok(());
    }
    let channels: Vec<_> = state
        .channels
        .iter()
        .map(|channel| (channel.index, channel.source.clone(), channel.muted))
        .collect();
    print_board(out, state.firmware.as_deref(), &channels, &state.faders)?;
    writeln!(
        out,
        "Changes applied since the last dump: {}",
        state.notifications
    )?;
    Ok(())
}

fn hid_setup(out: &mut impl Write) -> Result<(), CliError> {
    let dest = rcp2_hid::UDEV_RULE_PATH;
    match rcp2_hid::rule_state(read_rule()?.as_deref()) {
        rcp2_hid::RuleState::UpToDate => {
            writeln!(out, "Access already set up ({dest}).")?;
            return install_service(out);
        }
        rcp2_hid::RuleState::Foreign => return Err(CliError::ForeignFile(PathBuf::from(dest))),
        rcp2_hid::RuleState::Absent | rcp2_hid::RuleState::Outdated => {}
    }
    writeln!(
        out,
        "Installing {dest} so that your user can talk to the board (sudo may ask for your password)."
    )?;
    // A private temporary copy (mode 0600), then `install` sets owner and mode
    // in one step. Removed on every path.
    let tmp = std::env::temp_dir().join(format!("rcp2ctl-udev-{}.rules", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(file_err(&tmp))
        .and_then(|mut file| {
            file.write_all(rcp2_hid::UDEV_RULE.as_bytes())
                .map_err(file_err(&tmp))
        });
    let installed = written.and_then(|()| {
        let tmp_text = tmp.to_string_lossy().into_owned();
        sudo(&["install", "-m", "644", &tmp_text, dest])
    });
    let _ = std::fs::remove_file(&tmp);
    installed?;
    reload_udev()?;
    writeln!(out, "Access granted: no replug needed.")?;
    install_service(out)
}

/// Counts only: the content holds the serial number and stays in the file.
fn summarise_capture(
    reports: &[rcp2_hid::Report],
    file: &Path,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let bytes: usize = reports.iter().map(|report| report.bytes.len()).sum();
    writeln!(
        out,
        "Captured {} report(s), {bytes} bytes, into {}.",
        reports.len(),
        file.display()
    )?;
    let mut ids: Vec<(u8, usize)> = Vec::new();
    for id in reports
        .iter()
        .filter_map(|report| report.bytes.first().copied())
    {
        match ids.iter_mut().find(|(known, _)| *known == id) {
            Some((_, count)) => *count += 1,
            None => ids.push((id, 1)),
        }
    }
    for (id, count) in ids {
        writeln!(out, "  report {id}: {count}")?;
    }
    if let Some(last) = reports.last() {
        writeln!(out, "  last report after {} ms", last.at.as_millis())?;
    }
    Ok(())
}

fn join_ids(ids: &[u32]) -> String {
    ids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
