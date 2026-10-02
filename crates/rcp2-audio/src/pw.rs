//! Side effects: running PipeWire tools.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use crate::config::UnsafeNodeName;
use crate::graph::{Graph, ParseError};

/// Error while talking to PipeWire or touching configuration files.
#[derive(Debug, thiserror::Error)]
pub enum PwError {
    /// A PipeWire tool could not be started.
    #[error("could not run `{tool}` (is it installed? it ships with PipeWire): {source}")]
    Spawn {
        /// Tool name.
        tool: &'static str,
        /// Underlying error.
        source: io::Error,
    },
    /// A PipeWire tool exited with an error.
    #[error("`{tool}` failed: {stderr}")]
    Failed {
        /// Tool name.
        tool: &'static str,
        /// Its standard error, trimmed.
        stderr: String,
    },
    /// `pw-dump` printed something unexpected.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A node name could not be passed safely to PipeWire.
    #[error(transparent)]
    UnsafeNodeName(#[from] UnsafeNodeName),
    /// The PipeWire config file was changed outside `rcp2ctl`: left untouched.
    #[error(
        "{0} was changed outside rcp2ctl, so it was left untouched: review it, \
         then delete it or move it away and rerun"
    )]
    ModifiedOutside(PathBuf),
    /// The config path is a symlink: it is managed elsewhere (dotfiles, Nix…).
    #[error(
        "{0} is a symlink, so it is managed elsewhere: update its target yourself, \
         or remove the link and rerun"
    )]
    Symlink(PathBuf),
    /// Neither `XDG_CONFIG_HOME` nor `HOME` is usable.
    #[error("cannot locate the home directory: set HOME (or the XDG base directory variables)")]
    NoConfigDir,
    /// Reading or writing the configuration file failed.
    #[error("{path}: {source}")]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
}

pub(crate) fn run<S: AsRef<OsStr>>(tool: &'static str, args: &[S]) -> Result<Vec<u8>, PwError> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .map_err(|source| PwError::Spawn { tool, source })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(PwError::Failed {
            tool,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Takes a snapshot of the PipeWire graph with `pw-dump`.
///
/// # Errors
///
/// Returns [`PwError`] if `pw-dump` cannot run, fails, or prints invalid JSON.
pub fn snapshot() -> Result<Graph, PwError> {
    let stdout = run::<&str>("pw-dump", &[])?;
    Ok(Graph::from_pw_dump(&String::from_utf8_lossy(&stdout))?)
}

/// Moves the stream `stream_id` to the node whose `object.serial` is
/// `target_serial`, through the `default` metadata.
///
/// Uses the serial with type `Spa:Id`, like `pactl move-sink-input`: with this
/// form WirePlumber also remembers the target for the application's next runs
/// (a plain node name moves the stream but makes WirePlumber forget it).
///
/// # Errors
///
/// Returns [`PwError`] if `pw-metadata` cannot run or fails.
pub fn move_stream(stream_id: u32, target_serial: u64) -> Result<(), PwError> {
    let stream = stream_id.to_string();
    let target = target_serial.to_string();
    run(
        "pw-metadata",
        &[
            OsStr::new("-n"),
            OsStr::new("default"),
            OsStr::new(&stream),
            OsStr::new("target.object"),
            OsStr::new(&target),
            OsStr::new("Spa:Id"),
        ],
    )
    .map(drop)
}
