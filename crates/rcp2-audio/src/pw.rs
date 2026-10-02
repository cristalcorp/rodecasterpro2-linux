//! Side effects: running PipeWire tools and installing the configuration.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
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
    /// The config path is a symlink: it is managed elsewhere (dotfiles, Nix…).
    #[error(
        "{0} is a symlink, so it is managed elsewhere: update its target yourself, \
         or remove the link and rerun"
    )]
    Symlink(PathBuf),
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
    /// The file existed with other content, which was copied to `backup`.
    Replaced {
        /// Where the previous content was kept.
        backup: PathBuf,
    },
}

/// How many same-second names are tried before giving up.
const MAX_NAMES_PER_SECOND: u32 = 100;

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> PwError + use<> {
    let path = path.to_owned();
    move |source| PwError::Io { path, source }
}

/// Refuses a symlink at `path`: whoever manages the link (a dotfiles repo, a
/// read-only Nix store…) owns its content, and neither following nor replacing
/// it is safe in every setup.
fn refuse_symlink(path: &Path) -> Result<(), PwError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(PwError::Symlink(path.to_owned())),
        _ => Ok(()),
    }
}

/// Writes `contents` to a new file at `path` and flushes it to disk. Fails if
/// anything, including a symlink, already exists at `path`: nothing is followed.
fn write_synced(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Writes `contents` to a new, never pre-existing file named
/// `<path>.<tag>-<unix seconds>[-<n>]` and returns its path. The name does not
/// end in `.conf`, so PipeWire ignores it; concurrent callers never share one.
fn write_unique(path: &Path, tag: &str, contents: &[u8]) -> Result<PathBuf, PwError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut last = None;
    for n in 0..MAX_NAMES_PER_SECOND {
        let mut name = path.as_os_str().to_owned();
        if n == 0 {
            name.push(format!(".{tag}-{stamp}"));
        } else {
            name.push(format!(".{tag}-{stamp}-{n}"));
        }
        let candidate = PathBuf::from(name);
        match write_synced(&candidate, contents) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = Some((candidate, err)),
            Err(err) => {
                // A partial file would look valid: remove it (best effort).
                let _ = fs::remove_file(&candidate);
                return Err(io_err(&candidate)(err));
            }
        }
    }
    Err(match last {
        Some((candidate, err)) => io_err(&candidate)(err),
        None => io_err(path)(io::Error::from(io::ErrorKind::AlreadyExists)),
    })
}

/// Writes `contents` to `path`, keeping any different previous content as
/// `<path>.bak-<unix seconds>` (PipeWire only loads `*.conf`, so the backup is
/// inert).
///
/// The new content is first written and flushed to a uniquely named temporary
/// file, the previous content is backed up, then a single atomic rename puts
/// the new file in place: at no point is the config missing or partial.
///
/// Does not restart PipeWire: the caller tells the user how to apply it.
///
/// # Errors
///
/// Returns [`PwError::Symlink`] if `path` is a symlink, and [`PwError::Io`] if
/// a directory or file cannot be read, created, renamed or written. On error
/// the existing file is left untouched and no temporary file or backup is left
/// behind (best effort).
pub fn install_config(path: &Path, contents: &str) -> Result<InstallOutcome, PwError> {
    refuse_symlink(path)?;
    let previous = match fs::read(path) {
        Ok(previous) => Some(previous),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_err(path)(err)),
    };
    if previous.as_deref() == Some(contents.as_bytes()) {
        return Ok(InstallOutcome::Unchanged);
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let tmp = write_unique(path, "tmp", contents.as_bytes())?;
    let outcome = match previous.map(|previous| write_backup_then_rename(path, &tmp, &previous)) {
        Some(Ok(backup)) => Ok(InstallOutcome::Replaced { backup }),
        Some(Err(err)) => Err(err),
        None => fs::rename(&tmp, path)
            .map(|()| InstallOutcome::Created)
            .map_err(io_err(path)),
    };
    if outcome.is_err() {
        // Best effort cleanup: the error being returned matters more.
        let _ = fs::remove_file(&tmp);
    }
    outcome
}

/// Backs up `previous`, then renames `tmp` over `path`. If the rename fails,
/// the backup of the still-unchanged config is removed.
fn write_backup_then_rename(path: &Path, tmp: &Path, previous: &[u8]) -> Result<PathBuf, PwError> {
    let backup = write_unique(path, "bak", previous)?;
    if let Err(err) = fs::rename(tmp, path) {
        let _ = fs::remove_file(&backup);
        return Err(io_err(path)(err));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::{InstallOutcome, PwError, install_config};
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
        // Only the config and its backup remain: no temporary file.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn back_to_back_installs_keep_every_backup() {
        let dir = scratch_dir("same-second");
        let path = dir.join("50-test.conf");
        install_config(&path, "one").unwrap();
        let backups: Vec<PathBuf> = ["two", "three", "four"]
            .into_iter()
            .map(|contents| match install_config(&path, contents).unwrap() {
                InstallOutcome::Replaced { backup } => backup,
                other => panic!("expected a backup, got {other:?}"),
            })
            .collect();
        let saved: Vec<String> = backups
            .iter()
            .map(|backup| fs::read_to_string(backup).unwrap())
            .collect();
        assert_eq!(saved, ["one", "two", "three"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaces_a_previous_file_that_is_not_utf8() {
        let dir = scratch_dir("latin1");
        let path = dir.join("50-test.conf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, b"caf\xe9").unwrap();
        let InstallOutcome::Replaced { backup } = install_config(&path, "new").unwrap() else {
            panic!("expected a backup of the previous content");
        };
        assert_eq!(fs::read(&backup).unwrap(), b"caf\xe9");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_symlinks_and_touches_nothing() {
        let dir = scratch_dir("symlink");
        let real = dir.join("dotfiles").join("50-test.conf");
        let link = dir.join("pipewire.conf.d").join("50-test.conf");
        let dangling = dir.join("pipewire.conf.d").join("51-test.conf");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::os::unix::fs::symlink(dir.join("missing.conf"), &dangling).unwrap();

        assert!(matches!(
            install_config(&link, "new"),
            Err(PwError::Symlink(_))
        ));
        assert!(matches!(
            install_config(&dangling, "new"),
            Err(PwError::Symlink(_))
        ));

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "old");
        assert_eq!(fs::read_dir(link.parent().unwrap()).unwrap().count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }
}
