//! Side effects: running PipeWire tools and installing the configuration.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::CONFIG_FILE_NAME;
use crate::graph::{Graph, ParseError};

/// Error while talking to PipeWire or touching the configuration.
#[derive(Debug, thiserror::Error)]
pub enum PwError {
    /// A PipeWire tool could not be started.
    #[error("could not run `{tool}` (is PipeWire installed?): {source}")]
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
    /// Neither `XDG_CONFIG_HOME` nor `HOME` is usable.
    #[error("cannot locate the config directory: neither XDG_CONFIG_HOME nor HOME is set")]
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

fn run(tool: &'static str, args: &[&OsStr]) -> Result<Vec<u8>, PwError> {
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
    let stdout = run("pw-dump", &[])?;
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

/// Path of the generated configuration:
/// `$XDG_CONFIG_HOME/pipewire/pipewire.conf.d/` (or `~/.config/...`).
///
/// # Errors
///
/// Returns [`PwError::NoConfigDir`] if no usable base directory is set.
pub fn config_path() -> Result<PathBuf, PwError> {
    // Per the XDG spec, a relative XDG_CONFIG_HOME is invalid and ignored.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or(PwError::NoConfigDir)?;
    Ok(base
        .join("pipewire")
        .join("pipewire.conf.d")
        .join(CONFIG_FILE_NAME))
}

/// What [`install_config`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The file already had this exact content: nothing was written.
    Unchanged,
    /// The file did not exist and was created.
    Created,
    /// The file existed with other content, which was moved to `backup`.
    Replaced {
        /// Where the previous content was kept.
        backup: PathBuf,
    },
}

/// Writes `contents` to `path` atomically, keeping any different previous
/// content as `<path>.bak-<unix seconds>` (PipeWire only loads `*.conf`, so the
/// backup is inert).
///
/// Does not restart PipeWire: the caller tells the user how to apply it.
///
/// # Errors
///
/// Returns [`PwError::Io`] if a directory or file cannot be read, created,
/// renamed or written.
pub fn install_config(path: &Path, contents: &str) -> Result<InstallOutcome, PwError> {
    let io_err = |path: &Path| {
        let path = path.to_owned();
        move |source| PwError::Io { path, source }
    };

    let previous = match fs::read_to_string(path) {
        Ok(previous) => Some(previous),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_err(path)(err)),
    };
    if previous.as_deref() == Some(contents) {
        return Ok(InstallOutcome::Unchanged);
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let tmp = path.with_extension("conf.tmp");
    fs::write(&tmp, contents).map_err(io_err(&tmp))?;

    let outcome = if previous.is_some() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut backup = path.as_os_str().to_owned();
        backup.push(format!(".bak-{stamp}"));
        let backup = PathBuf::from(backup);
        fs::rename(path, &backup).map_err(io_err(path))?;
        InstallOutcome::Replaced { backup }
    } else {
        InstallOutcome::Created
    };
    fs::rename(&tmp, path).map_err(io_err(path))?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::{InstallOutcome, install_config};
    use std::fs;
    use std::path::PathBuf;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rcp2-audio-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_then_is_idempotent_then_backs_up() {
        let dir = scratch_dir("install");
        let path = dir.join("pipewire.conf.d").join("50-test.conf");

        assert_eq!(
            install_config(&path, "one").unwrap(),
            InstallOutcome::Created
        );
        assert_eq!(
            install_config(&path, "one").unwrap(),
            InstallOutcome::Unchanged
        );

        let InstallOutcome::Replaced { backup } = install_config(&path, "two").unwrap() else {
            panic!("expected a backup of the previous content");
        };
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "one");
        assert!(!path.with_extension("conf.tmp").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
