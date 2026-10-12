//! Resolved application paths (SPEC R9.1 to R9.3).
//!
//! Every path comes from the platform-directory crate, so Windows resolves
//! `%APPDATA%` for config and `%LOCALAPPDATA%` for state, and Linux resolves
//! the XDG directories. No path is hardcoded.

use std::path::PathBuf;

use anyhow::Context as _;
use directories::BaseDirs;

/// The application's directory name, below the platform config and data roots.
pub const APP_DIR: &str = "immich-sync";
/// The TOML settings file name.
pub const SETTINGS_FILE: &str = "settings.toml";
/// The SQLite state database file name.
pub const STATE_FILE: &str = "state.db";

/// The paths the client resolves at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    /// The TOML settings file, in the OS config directory.
    pub config_file: PathBuf,
    /// The state database, in the local data directory. Never a network or
    /// cloud-sync folder (SPEC N4): the local data directory is per-machine.
    pub state_db: PathBuf,
    /// The directory that holds the XDG autostart entry, on Linux.
    pub autostart_dir: PathBuf,
}

impl AppPaths {
    /// Maps the platform base directories to the application's paths.
    #[must_use]
    pub fn from_base_dirs(base: &BaseDirs) -> Self {
        let config_dir = base.config_dir();
        Self {
            config_file: config_dir.join(APP_DIR).join(SETTINGS_FILE),
            state_db: base.data_local_dir().join(APP_DIR).join(STATE_FILE),
            autostart_dir: config_dir.join("autostart"),
        }
    }

    /// Resolves the paths for the current user.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform exposes no home directory.
    pub fn resolve() -> anyhow::Result<Self> {
        let base = BaseDirs::new()
            .context("the platform did not provide a home directory for the client")?;
        Ok(Self::from_base_dirs(&base))
    }

    /// The paths used when [`Self::resolve`] fails: the current directory.
    ///
    /// This keeps the window usable so it can show the failure. The caller
    /// shows the reason through the view's settings problem notice.
    #[must_use]
    pub fn fallback() -> Self {
        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            config_file: current.join(APP_DIR).join(SETTINGS_FILE),
            state_db: current.join(APP_DIR).join(STATE_FILE),
            autostart_dir: current.join("autostart"),
        }
    }
}
