//! `rcp2ctl`: Linux control tool for the RØDECaster Pro II.

mod settings;
mod tui;

use std::io::{self, BufRead, IsTerminal, Write};
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
    /// Show which hidraw node is the board's control interface (opens nothing).
    Find,
    /// Handshake with the board and record what it sends (read-only), to study
    /// the protocol. The file contains the board's serial number: keep it private.
    Capture {
        /// Output file, must end in `.rcp2cap` (ignored by git).
        file: PathBuf,
        /// Stop after this many seconds at most.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
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
        Command::Status => {
            let settings = load_settings()?;
            status(&prepare(&settings)?, &settings, out)
        }
        Command::Apps => apps(&prepare(&load_settings()?)?, out),
        Command::Route { app, channel } => route(&prepare(&load_settings()?)?, &app, channel, out),
        Command::Default { channel } => set_default(&prepare(&load_settings()?)?, channel, out),
        Command::Hid { command } => hid(command, out),
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
    // Saved first: the in-memory settings only change once the file has.
    let updated = Settings {
        outputs: state == Switch::On,
    };
    settings::save(settings_path, &updated)?;
    *settings = updated;
    if state == Switch::On {
        for text in restore_outputs(settings)?.notices() {
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

/// Creates the missing named outputs if they are on, without printing
/// anything (the TUI owns the terminal).
fn restore_outputs(settings: &Settings) -> Result<Restored, CliError> {
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
    let rode = graph.rode()?;
    writeln!(out, "RØDECaster Pro II found")?;
    writeln!(out, "  native stereo sink: {}", rode.stereo_sink)?;
    writeln!(out, "  native multi sink:  {}", rode.multi_sink)?;
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
    for channel in Channel::ALL {
        let state = if graph.virtual_sink(channel).is_some() {
            "present"
        } else {
            "absent"
        };
        writeln!(
            out,
            "  {:<6} {:<12} {state}",
            channel.id(),
            channel.sink_name()
        )?;
    }
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
        HidCommand::Find => {
            for path in rcp2_hid::find_devices(sys_class)? {
                writeln!(out, "{}", path.display())?;
            }
            Ok(())
        }
        HidCommand::Capture { file, seconds } => {
            if file.extension().and_then(|ext| ext.to_str()) != Some(CAPTURE_EXTENSION) {
                return Err(CliError::CaptureExtension);
            }
            // Create the file first: never handshake for nothing, never overwrite.
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .map_err(|source| PwError::Io {
                    path: file.clone(),
                    source,
                })?;
            let path = rcp2_hid::find_device(sys_class)?;
            let mut board = rcp2_hid::Board::open(&path)?;
            let reports =
                rcp2_hid::capture(&mut board, Duration::from_secs(seconds), CAPTURE_IDLE)?;
            let mut text = String::from("# rcp2 capture v1: <microseconds> <report hex>\n");
            for report in &reports {
                // Writing to a String cannot fail.
                let _ = write!(text, "{} ", report.at.as_micros());
                for byte in &report.bytes {
                    let _ = write!(text, "{byte:02x}");
                }
                text.push('\n');
            }
            output
                .write_all(text.as_bytes())
                .map_err(|source| PwError::Io {
                    path: file.clone(),
                    source,
                })?;
            summarise_capture(&reports, &file, out)
        }
    }
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
