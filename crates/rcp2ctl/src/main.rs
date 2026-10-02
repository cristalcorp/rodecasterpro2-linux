//! `rcp2ctl`: Linux control tool for the RØDECaster Pro II.

mod settings;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use rcp2_audio::{
    APP_DIR, Channel, DetectError, DisableOutcome, EnableOutcome, Graph, PersistPaths,
    PersistState, PwError, UnsafeNodeName, create_runtime_outputs, data_home, disable_persistence,
    enable_persistence, move_stream, persist_state, pipewire_config, remove_runtime_outputs,
    snapshot,
};
use settings::{Settings, SettingsError};

/// Linux control tool for the RØDECaster Pro II (unofficial).
///
/// At every launch the named outputs ("RØDE Game", "RØDE Music"…) are created
/// if missing, without writing any file, unless turned off with `outputs off`.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    #[error("cannot write output: {0}")]
    Output(#[from] io::Error),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut out = io::stdout().lock();
    match run(cli.command, &mut out) {
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
    let settings_path = settings::settings_path()?;
    let mut settings = settings::load(&settings_path)?;
    match command {
        Command::Config => config(&snapshot()?, out),
        Command::Uninstall {
            yes,
            keep_pipewire_config,
        } => uninstall(&settings_path, yes, keep_pipewire_config, out),
        Command::Outputs { state } => {
            settings.outputs = state == Switch::On;
            if state == Switch::Off
                && persist_state(&PersistPaths::from_env()?)? == PersistState::On
            {
                return Err(CliError::PersistentOutputs);
            }
            settings::save(&settings_path, &settings)?;
            if state == Switch::On {
                prepare(&settings)?;
                writeln!(out, "Named outputs on (created at each launch).")?;
            } else {
                let removed = remove_runtime_outputs()?;
                writeln!(
                    out,
                    "Named outputs off ({} removed). Apps that played on them move to the \
                     default output.",
                    removed.len()
                )?;
            }
            Ok(())
        }
        Command::Status => status(&prepare(&settings)?, &settings, out),
        Command::Apps => apps(&prepare(&settings)?, out),
        Command::Route { app, channel } => route(&prepare(&settings)?, &app, channel, out),
        Command::Persist { state } => persist(&prepare(&settings)?, state, out),
    }
}

/// Restores the application's state at launch: creates the named outputs that
/// are missing if they are on. Returns an up-to-date graph. Notices go to
/// stderr so that command output stays data only.
fn prepare(settings: &Settings) -> Result<Graph, CliError> {
    let graph = snapshot()?;
    if !settings.outputs {
        return Ok(graph);
    }
    // Without the board there is nothing to create: commands report it themselves.
    let Ok(rode) = graph.rode() else {
        return Ok(graph);
    };
    let created = create_runtime_outputs(&graph, &rode)?;
    if created.is_empty() {
        return Ok(graph);
    }
    let names: Vec<_> = created.iter().map(|channel| channel.label()).collect();
    let _ = writeln!(
        io::stderr().lock(),
        "rcp2ctl: created named outputs: {}",
        names.join(", ")
    );
    Ok(snapshot()?)
}

fn status(graph: &Graph, settings: &Settings, out: &mut impl Write) -> Result<(), CliError> {
    let rode = graph.rode()?;
    writeln!(out, "RØDECaster Pro II found")?;
    writeln!(out, "  native stereo sink: {}", rode.stereo_sink)?;
    writeln!(out, "  native multi sink:  {}", rode.multi_sink)?;
    let paths = PersistPaths::from_env()?;
    let mode = match (persist_state(&paths)?, settings.outputs) {
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
    if persist_state(&paths)? == PersistState::Foreign {
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

fn persist(graph: &Graph, state: Switch, out: &mut impl Write) -> Result<(), CliError> {
    let paths = PersistPaths::from_env()?;
    let file = paths.config_file.display();
    match state {
        Switch::On => {
            let contents = pipewire_config(&graph.rode()?)?;
            match enable_persistence(&paths, &contents)? {
                EnableOutcome::Unchanged => writeln!(out, "Persistence already on ({file}).")?,
                EnableOutcome::Updated => writeln!(out, "Persistence on: updated {file}.")?,
                EnableOutcome::Enabled => writeln!(
                    out,
                    "Persistence on: wrote {file}. The current outputs keep working; the \
                     file takes over at the next PipeWire start. `rcp2ctl persist off` \
                     restores the original state."
                )?,
            }
        }
        Switch::Off => match disable_persistence(&paths)? {
            DisableOutcome::AlreadyOff => writeln!(out, "Persistence already off.")?,
            DisableOutcome::Restored => {
                writeln!(out, "Persistence off: original {file} restored.")?;
            }
            DisableOutcome::Removed => writeln!(
                out,
                "Persistence off: removed {file} (there was none originally). The current \
                 outputs keep working until PipeWire restarts; rcp2ctl recreates them at \
                 launch."
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

fn uninstall(
    settings_path: &Path,
    yes: bool,
    keep_pipewire_config: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let paths = PersistPaths::from_env()?;
    if persist_state(&paths)? == PersistState::On {
        let restore = yes
            || (!keep_pipewire_config
                && ask("Restore the original PipeWire configuration?", true, out)?);
        if restore {
            disable_persistence(&paths)?;
            writeln!(out, "Original PipeWire configuration restored.")?;
        } else {
            writeln!(
                out,
                "Kept {} (delete it to remove the outputs for good). The record of the \
                 original state stays in {}.",
                paths.config_file.display(),
                paths.snapshot_dir.display()
            )?;
        }
    }
    let removed = remove_runtime_outputs()?;
    writeln!(
        out,
        "Removed {} named output(s) created at runtime.",
        removed.len()
    )?;

    // Only what rcp2ctl created: never a recursive delete of a shared directory.
    remove_if_present(settings_path)?;
    if let Some(dir) = settings_path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    // Non-recursive: a kept snapshot (restoration declined) or anything else
    // in our data directory stays.
    let _ = std::fs::remove_dir(data_home()?.join(APP_DIR));
    writeln!(out, "rcp2ctl settings removed.")?;
    match std::env::current_exe() {
        Ok(exe) => writeln!(
            out,
            "Last step: remove the binary itself ({}).",
            exe.display()
        )?,
        Err(_) => writeln!(out, "Last step: remove the rcp2ctl binary itself.")?,
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<(), CliError> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

/// Name to show for a node an application plays to: our label for our own
/// outputs, the node description otherwise.
fn target_label(node: &rcp2_audio::Node) -> String {
    Channel::from_sink_name(&node.name).map_or_else(
        || {
            node.description
                .clone()
                .unwrap_or_else(|| node.name.clone())
        },
        Channel::description,
    )
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
        let output = if stream.targets.is_empty() {
            "(not connected)".to_owned()
        } else {
            stream
                .targets
                .iter()
                .map(|node| target_label(node))
                .collect::<Vec<_>>()
                .join(", ")
        };
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

fn join_ids(ids: &[u32]) -> String {
    ids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
