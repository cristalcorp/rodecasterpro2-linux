//! The application's own settings, re-read at every launch: the only file the
//! application writes on its own behalf.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rcp2_audio::{APP_DIR, PwError, config_home, refuse_symlink, write_atomically};
use serde::{Deserialize, Serialize};

/// Error while reading or writing the settings file.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SettingsError {
    #[error(transparent)]
    Pw(#[from] PwError),
    #[error("{path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not valid (fix or delete it): {source}")]
    Invalid {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot serialise the settings: {0}")]
    Serialise(#[from] toml::ser::Error),
}

/// Settings persisted between launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Create the named outputs at launch when they are missing.
    pub(crate) outputs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { outputs: true }
    }
}

/// `$XDG_CONFIG_HOME/rodecasterpro2-linux/config.toml`.
pub(crate) fn settings_path() -> Result<PathBuf, PwError> {
    Ok(config_home()?.join(APP_DIR).join("config.toml"))
}

/// Loads the settings; defaults if the file does not exist yet. A file that
/// cannot be parsed is an error, never silently replaced.
pub(crate) fn load(path: &Path) -> Result<Settings, SettingsError> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Invalid {
            path: path.to_owned(),
            source,
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(source) => Err(SettingsError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Saves the settings atomically.
pub(crate) fn save(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    // Replacing a symlink with a plain file would detach it from its manager.
    refuse_symlink(path)?;
    let text = toml::to_string(settings)?;
    write_atomically(path, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Settings, SettingsError, load, save};
    use std::fs;

    #[test]
    fn round_trips_and_defaults_to_outputs_on() {
        let dir = std::env::temp_dir().join(format!("rcp2ctl-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        assert_eq!(load(&path).unwrap(), Settings { outputs: true });
        save(&path, &Settings { outputs: false }).unwrap();
        assert_eq!(load(&path).unwrap(), Settings { outputs: false });

        // Unknown keys (from a newer version) and missing keys are tolerated.
        fs::write(&path, "future = 1\n").unwrap();
        assert_eq!(load(&path).unwrap(), Settings { outputs: true });

        fs::write(&path, "outputs = \"maybe\"\n").unwrap();
        assert!(matches!(load(&path), Err(SettingsError::Invalid { .. })));
        fs::remove_dir_all(&dir).unwrap();
    }
}
