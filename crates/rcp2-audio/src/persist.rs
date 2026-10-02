//! Optional persistence: the PipeWire config file declaring the named outputs,
//! with a snapshot of what was at its path before, so that turning persistence
//! off restores it exactly.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CONFIG_FILE_NAME, GENERATED_MARKER};
use crate::pw::PwError;

/// Directory name used by this project under the XDG base directories.
pub const APP_DIR: &str = "rodecasterpro2-linux";

/// How many same-second temporary names are tried before giving up.
const MAX_NAMES_PER_SECOND: u32 = 100;

/// Marker file recording that no config file existed before persistence.
const ABSENT_MARKER: &str = "was-absent";

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> PwError + use<> {
    let path = path.to_owned();
    move |source| PwError::Io { path, source }
}

/// `$<var>` if set and absolute (the XDG spec ignores relative values), else
/// `$HOME/<fallback>`.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf, PwError> {
    let absolute = |path: &PathBuf| path.is_absolute();
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(absolute)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(absolute)
                .map(|home| home.join(fallback))
        })
        .ok_or(PwError::NoConfigDir)
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
///
/// # Errors
///
/// Returns [`PwError::NoConfigDir`] if neither variable is usable.
pub fn config_home() -> Result<PathBuf, PwError> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
///
/// # Errors
///
/// Returns [`PwError::NoConfigDir`] if neither variable is usable.
pub fn data_home() -> Result<PathBuf, PwError> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Writes `contents` to a new file at `path` and flushes it to disk. Fails if
/// anything, including a symlink, already exists at `path`: nothing is followed.
fn write_synced(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Writes `contents` to a new, never pre-existing file named
/// `<path>.tmp-<unix seconds>[-<n>]` and returns its path. The name does not
/// end in `.conf`, so PipeWire ignores it; concurrent callers never share one.
fn write_temp(path: &Path, contents: &[u8]) -> Result<PathBuf, PwError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut last = None;
    for n in 0..MAX_NAMES_PER_SECOND {
        let mut name = path.as_os_str().to_owned();
        if n == 0 {
            name.push(format!(".tmp-{stamp}"));
        } else {
            name.push(format!(".tmp-{stamp}-{n}"));
        }
        let candidate = PathBuf::from(name);
        match write_synced(&candidate, contents) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = Some((candidate, err)),
            Err(err) => {
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

/// Replaces `path` with `contents` atomically: the new content is written and
/// flushed to a uniquely named temporary file, then renamed over `path`. At no
/// point is `path` missing or partial. Parent directories are created.
///
/// # Errors
///
/// Returns [`PwError::Io`] if a directory or file cannot be created, written or
/// renamed; `path` is then left untouched.
pub fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), PwError> {
    replace_file(path, contents, None)
}

/// [`write_atomically`], with `permissions` applied to the temporary file
/// before the rename: the content is never visible with other permissions.
fn replace_file(
    path: &Path,
    contents: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<(), PwError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let tmp = write_temp(path, contents)?;
    let result = match permissions {
        Some(permissions) => fs::set_permissions(&tmp, permissions.clone()).map_err(io_err(&tmp)),
        None => Ok(()),
    }
    .and_then(|()| fs::rename(&tmp, path).map_err(io_err(path)));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Refuses a symlink at `path`: whoever manages the link (a dotfiles repo, a
/// read-only Nix store…) owns its content.
///
/// # Errors
///
/// Returns [`PwError::Symlink`] if `path` is a symlink.
pub fn refuse_symlink(path: &Path) -> Result<(), PwError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(PwError::Symlink(path.to_owned())),
        _ => Ok(()),
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, PwError> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_err(path)(err)),
    }
}

/// Removes the file at `path` if there is one.
///
/// # Errors
///
/// Returns [`PwError::Io`] (naming the path) if it exists but cannot be removed.
pub fn remove_file_if_present(path: &Path) -> Result<(), PwError> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_err(path)(err)),
        _ => Ok(()),
    }
}

fn is_ours(contents: &[u8]) -> bool {
    contents.starts_with(GENERATED_MARKER.as_bytes())
}

/// Where the persistent config and the snapshot of the original live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistPaths {
    /// The PipeWire config file declaring the named outputs.
    pub config_file: PathBuf,
    /// Directory holding the snapshot of what was at `config_file` before.
    pub snapshot_dir: PathBuf,
}

impl PersistPaths {
    /// Standard locations: `~/.config/pipewire/pipewire.conf.d/` and
    /// `~/.local/share/rodecasterpro2-linux/original/` (XDG variables honoured).
    ///
    /// # Errors
    ///
    /// Returns [`PwError::NoConfigDir`] if no usable base directory is set.
    pub fn from_env() -> Result<Self, PwError> {
        Ok(Self {
            config_file: config_home()?
                .join("pipewire")
                .join("pipewire.conf.d")
                .join(CONFIG_FILE_NAME),
            snapshot_dir: data_home()?.join(APP_DIR).join("original"),
        })
    }

    fn snapshot_file(&self) -> PathBuf {
        self.snapshot_dir.join(CONFIG_FILE_NAME)
    }

    fn absent_marker(&self) -> PathBuf {
        self.snapshot_dir.join(ABSENT_MARKER)
    }
}

/// Whether the persistent config is in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistState {
    /// No file at the config path.
    Off,
    /// Our generated file is in place.
    On,
    /// A file we did not generate (or a symlink) is at the config path.
    Foreign,
}

/// Reads the persistence state from the filesystem (the single source of truth).
///
/// # Errors
///
/// Returns [`PwError::Io`] if the config file exists but cannot be read.
pub fn persist_state(paths: &PersistPaths) -> Result<PersistState, PwError> {
    if refuse_symlink(&paths.config_file).is_err() {
        return Ok(PersistState::Foreign);
    }
    Ok(match read_optional(&paths.config_file)? {
        None => PersistState::Off,
        Some(contents) if is_ours(&contents) => PersistState::On,
        Some(_) => PersistState::Foreign,
    })
}

/// What was at the config path before persistence was first turned on. The
/// snapshot file carries the original's permissions.
enum Original {
    Absent,
    Content {
        contents: Vec<u8>,
        permissions: fs::Permissions,
    },
}

fn read_snapshot(paths: &PersistPaths) -> Result<Option<Original>, PwError> {
    let file = paths.snapshot_file();
    if let Some(contents) = read_optional(&file)? {
        let permissions = fs::metadata(&file).map_err(io_err(&file))?.permissions();
        return Ok(Some(Original::Content {
            contents,
            permissions,
        }));
    }
    if read_optional(&paths.absent_marker())?.is_some() {
        return Ok(Some(Original::Absent));
    }
    Ok(None)
}

fn write_snapshot(paths: &PersistPaths, original: &Original) -> Result<(), PwError> {
    match original {
        Original::Absent => write_atomically(&paths.absent_marker(), b""),
        Original::Content {
            contents,
            permissions,
        } => replace_file(&paths.snapshot_file(), contents, Some(permissions)),
    }
}

/// Puts the original back at the config path; `false` if it was "no file".
fn restore_original(paths: &PersistPaths, original: &Original) -> Result<bool, PwError> {
    match original {
        Original::Content {
            contents,
            permissions,
        } => {
            replace_file(&paths.config_file, contents, Some(permissions))?;
            Ok(true)
        }
        Original::Absent => Ok(false),
    }
}

/// Whether a record of the original state exists (persistence was turned on
/// and not cleanly turned off since).
///
/// # Errors
///
/// Returns [`PwError::Io`] if the record cannot be read.
pub fn original_recorded(paths: &PersistPaths) -> Result<bool, PwError> {
    Ok(read_snapshot(paths)?.is_some())
}

fn clear_snapshot(paths: &PersistPaths) -> Result<(), PwError> {
    remove_file_if_present(&paths.snapshot_file())?;
    remove_file_if_present(&paths.absent_marker())?;
    // Only removes the directory if empty: never anything we did not create.
    let _ = fs::remove_dir(&paths.snapshot_dir);
    Ok(())
}

/// What [`enable_persistence`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableOutcome {
    /// The file was written; the original state was recorded first.
    Enabled,
    /// Our previous file was replaced with new content.
    Updated,
    /// Our file already had this exact content.
    Unchanged,
}

/// Turns persistence on: records what is at the config path (once, the first
/// time), then writes `contents` there atomically.
///
/// A file we generated earlier without a snapshot (older versions) counts as
/// "no file originally".
///
/// # Errors
///
/// Returns [`PwError::Symlink`] for a symlink, [`PwError::ModifiedOutside`] if
/// a snapshot exists but the file was replaced by someone else since, and
/// [`PwError::Io`] on filesystem errors. Nothing is changed on error.
pub fn enable_persistence(paths: &PersistPaths, contents: &str) -> Result<EnableOutcome, PwError> {
    refuse_symlink(&paths.config_file)?;
    let current = read_optional(&paths.config_file)?;
    let current_is_ours = current.as_deref().is_some_and(is_ours);
    match read_snapshot(paths)? {
        Some(_) if current.is_some() && !current_is_ours => {
            return Err(PwError::ModifiedOutside(paths.config_file.clone()));
        }
        Some(_) => {}
        None => {
            let original = match current.clone() {
                Some(contents) if !is_ours(&contents) => Original::Content {
                    contents,
                    permissions: fs::metadata(&paths.config_file)
                        .map_err(io_err(&paths.config_file))?
                        .permissions(),
                },
                _ => Original::Absent,
            };
            write_snapshot(paths, &original)?;
        }
    }
    if current.as_deref() == Some(contents.as_bytes()) {
        return Ok(EnableOutcome::Unchanged);
    }
    write_atomically(&paths.config_file, contents.as_bytes())?;
    Ok(if current_is_ours {
        EnableOutcome::Updated
    } else {
        EnableOutcome::Enabled
    })
}

/// What [`disable_persistence`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableOutcome {
    /// The original file content was put back.
    Restored,
    /// There was no file originally: ours was removed.
    Removed,
    /// Persistence was already off: nothing to do.
    AlreadyOff,
}

/// Turns persistence off: restores exactly what was at the config path before
/// persistence was first turned on, then forgets the snapshot.
///
/// # Errors
///
/// Returns [`PwError::Symlink`] for a symlink, [`PwError::ModifiedOutside`] if
/// the file at the config path is not ours (it is left untouched), and
/// [`PwError::Io`] on filesystem errors.
pub fn disable_persistence(paths: &PersistPaths) -> Result<DisableOutcome, PwError> {
    refuse_symlink(&paths.config_file)?;
    let current = read_optional(&paths.config_file)?;
    if current
        .as_deref()
        .is_some_and(|contents| !is_ours(contents))
    {
        return Err(PwError::ModifiedOutside(paths.config_file.clone()));
    }
    // Also when our file was removed by hand: a recorded original goes back.
    let restored = match read_snapshot(paths)? {
        Some(original) => restore_original(paths, &original)?,
        None => false,
    };
    let outcome = if restored {
        DisableOutcome::Restored
    } else if current.is_some() {
        remove_file_if_present(&paths.config_file)?;
        DisableOutcome::Removed
    } else {
        DisableOutcome::AlreadyOff
    };
    clear_snapshot(paths)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::{
        DisableOutcome, EnableOutcome, PersistPaths, PersistState, disable_persistence,
        enable_persistence, original_recorded, persist_state, write_atomically,
    };
    use crate::pw::PwError;
    use std::fs;
    use std::path::PathBuf;

    const OURS: &str = "# Generated by rcp2ctl (test)\ncontext.modules = []\n";
    const OURS_V2: &str = "# Generated by rcp2ctl (test v2)\ncontext.modules = []\n";

    fn paths(name: &str) -> (PathBuf, PersistPaths) {
        let dir =
            std::env::temp_dir().join(format!("rcp2-audio-persist-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = PersistPaths {
            config_file: dir.join("pipewire.conf.d").join("50-test.conf"),
            snapshot_dir: dir.join("data").join("original"),
        };
        (dir, paths)
    }

    #[test]
    fn toggling_with_no_original_file_leaves_nothing_behind() {
        let (dir, paths) = paths("absent");
        assert_eq!(persist_state(&paths).unwrap(), PersistState::Off);

        assert_eq!(
            enable_persistence(&paths, OURS).unwrap(),
            EnableOutcome::Enabled
        );
        assert_eq!(persist_state(&paths).unwrap(), PersistState::On);
        assert_eq!(
            enable_persistence(&paths, OURS).unwrap(),
            EnableOutcome::Unchanged
        );
        assert_eq!(
            enable_persistence(&paths, OURS_V2).unwrap(),
            EnableOutcome::Updated
        );

        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::Removed
        );
        assert_eq!(persist_state(&paths).unwrap(), PersistState::Off);
        assert!(!paths.snapshot_dir.exists());
        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::AlreadyOff
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggling_restores_an_original_file_byte_for_byte() {
        let (dir, paths) = paths("original");
        let original = b"# someone's own config\n\xe9\n";
        write_atomically(&paths.config_file, original).unwrap();
        assert_eq!(persist_state(&paths).unwrap(), PersistState::Foreign);

        enable_persistence(&paths, OURS).unwrap();
        enable_persistence(&paths, OURS_V2).unwrap();
        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::Restored
        );

        assert_eq!(fs::read(&paths.config_file).unwrap(), original);
        assert!(!paths.snapshot_dir.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restores_the_original_even_if_our_file_was_deleted_by_hand() {
        let (dir, paths) = paths("deleted");
        write_atomically(&paths.config_file, b"mine").unwrap();
        enable_persistence(&paths, OURS).unwrap();
        fs::remove_file(&paths.config_file).unwrap();

        assert_eq!(persist_state(&paths).unwrap(), PersistState::Off);
        assert!(original_recorded(&paths).unwrap());
        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::Restored
        );
        assert_eq!(fs::read(&paths.config_file).unwrap(), b"mine");
        assert!(!original_recorded(&paths).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restores_the_original_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, paths) = paths("mode");
        write_atomically(&paths.config_file, b"private").unwrap();
        fs::set_permissions(&paths.config_file, fs::Permissions::from_mode(0o600)).unwrap();

        enable_persistence(&paths, OURS).unwrap();
        disable_persistence(&paths).unwrap();

        let mode = fs::metadata(&paths.config_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_from_an_older_version_counts_as_no_original() {
        let (dir, paths) = paths("legacy");
        write_atomically(&paths.config_file, OURS.as_bytes()).unwrap();

        enable_persistence(&paths, OURS_V2).unwrap();
        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::Removed
        );
        assert!(!paths.config_file.exists());

        // Same without enabling first: our file alone is simply removed.
        write_atomically(&paths.config_file, OURS.as_bytes()).unwrap();
        assert_eq!(
            disable_persistence(&paths).unwrap(),
            DisableOutcome::Removed
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn never_overwrites_a_file_changed_outside() {
        let (dir, paths) = paths("outside");
        enable_persistence(&paths, OURS).unwrap();
        write_atomically(&paths.config_file, b"hand edited").unwrap();

        assert!(matches!(
            enable_persistence(&paths, OURS),
            Err(PwError::ModifiedOutside(_))
        ));
        assert!(matches!(
            disable_persistence(&paths),
            Err(PwError::ModifiedOutside(_))
        ));
        assert_eq!(fs::read(&paths.config_file).unwrap(), b"hand edited");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_symlinks_and_touches_nothing() {
        let (dir, paths) = paths("symlink");
        let real = dir.join("dotfiles.conf");
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &paths.config_file).unwrap();

        assert_eq!(persist_state(&paths).unwrap(), PersistState::Foreign);
        assert!(matches!(
            enable_persistence(&paths, OURS),
            Err(PwError::Symlink(_))
        ));
        assert!(matches!(
            disable_persistence(&paths),
            Err(PwError::Symlink(_))
        ));
        assert_eq!(fs::read_to_string(&real).unwrap(), "old");
        assert!(!paths.snapshot_dir.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_writes_leave_no_temporary_file() {
        let (dir, paths) = paths("atomic");
        write_atomically(&paths.config_file, b"one").unwrap();
        write_atomically(&paths.config_file, b"two").unwrap();
        assert_eq!(fs::read(&paths.config_file).unwrap(), b"two");
        let parent = paths.config_file.parent().unwrap();
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
