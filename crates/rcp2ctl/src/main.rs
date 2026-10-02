//! `rcp2ctl`: Linux control tool for the RØDECaster Pro II.

use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rcp2_audio::{
    Channel, DetectError, Graph, InstallOutcome, PwError, UnsafeNodeName, config_path,
    install_config, move_stream, pipewire_config, snapshot,
};

/// Linux control tool for the RØDECaster Pro II (unofficial).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the board and which named outputs are installed.
    Status,
    /// Print the PipeWire configuration declaring the named outputs.
    Config {
        /// Write it to ~/.config/pipewire/pipewire.conf.d/ instead of printing it.
        #[arg(long)]
        install: bool,
    },
    /// List applications playing audio and where they play.
    Apps,
    /// Send an application to a named output (remembered for its next runs).
    Route {
        /// Stream ID, process binary or application name, as listed by `rcp2ctl apps`.
        app: String,
        /// Named output (an invalid value lists the valid ones).
        channel: Channel,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Pw(#[from] PwError),
    #[error(transparent)]
    Detect(#[from] DetectError),
    #[error(transparent)]
    UnsafeNodeName(#[from] UnsafeNodeName),
    #[error("no application matches `{0}` (see `rcp2ctl apps`)")]
    NoSuchApp(String),
    #[error(
        "the \"{0}\" output is not installed: run `rcp2ctl config --install`, then restart PipeWire"
    )]
    OutputMissing(&'static str),
    #[error("the \"{0}\" output has no object.serial; cannot route to it")]
    NoSerial(&'static str),
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
    match command {
        Command::Status => status(&snapshot()?, out),
        Command::Config { install } => config(&snapshot()?, install, out),
        Command::Apps => apps(&snapshot()?, out),
        Command::Route { app, channel } => route(&snapshot()?, &app, channel, out),
    }
}

fn status(graph: &Graph, out: &mut impl Write) -> Result<(), CliError> {
    let rode = graph.rode()?;
    writeln!(out, "RØDECaster Pro II found")?;
    writeln!(out, "  native stereo sink: {}", rode.stereo_sink)?;
    writeln!(out, "  native multi sink:  {}", rode.multi_sink)?;
    writeln!(out, "Named outputs:")?;
    let mut missing = false;
    for channel in Channel::ALL {
        let state = if graph.virtual_sink(channel).is_some() {
            "installed"
        } else {
            missing = true;
            "MISSING"
        };
        writeln!(
            out,
            "  {:<6} {:<12} {state}",
            channel.id(),
            channel.sink_name()
        )?;
    }
    if missing {
        writeln!(
            out,
            "Run `rcp2ctl config --install`, then restart PipeWire, to add the missing outputs."
        )?;
    }
    Ok(())
}

fn config(graph: &Graph, install: bool, out: &mut impl Write) -> Result<(), CliError> {
    let contents = pipewire_config(&graph.rode()?)?;
    if !install {
        out.write_all(contents.as_bytes())?;
        return Ok(());
    }
    let path = config_path()?;
    match install_config(&path, &contents)? {
        InstallOutcome::Unchanged => {
            writeln!(out, "{} is already up to date.", path.display())?;
            return Ok(());
        }
        InstallOutcome::Created => writeln!(out, "Created {}", path.display())?,
        InstallOutcome::Replaced { backup } => writeln!(
            out,
            "Updated {} (previous version kept as {})",
            path.display(),
            backup.display()
        )?,
    }
    writeln!(
        out,
        "Apply with: systemctl --user restart pipewire pipewire-pulse wireplumber \
         (audio drops for a second or two)."
    )?;
    Ok(())
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
        "{:>6}  {:<16}  {:<24}  {:<12}  MEDIA",
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
            "{:>6}  {:<16}  {:<24}  {:<12}  {}",
            stream.node.id,
            stream.node.process_binary.as_deref().unwrap_or("-"),
            stream.app_label(),
            output,
            stream.node.media_name.as_deref().unwrap_or("")
        )?;
    }
    writeln!(
        out,
        "Route by ID, binary or application name; an ID picks a single stream."
    )?;
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
    // Move every stream before printing: a closed stdout must not leave the
    // routing half done.
    for stream in &streams {
        move_stream(stream.node.id, serial)?;
    }
    for stream in &streams {
        writeln!(
            out,
            "{} (stream {}) -> {}",
            stream.app_label(),
            stream.node.id,
            channel.description()
        )?;
    }
    Ok(())
}
