//! The TOML settings store (SPEC R9.1).
//!
//! The file lives in the OS config directory and holds every value the spec
//! makes user-settable and persistent. Saves are atomic, and a file that cannot
//! be parsed falls back to the defaults with a reported problem, so the client
//! stays usable and the user learns what happened.

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The user-settable, persistent settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Start the client at login (SPEC R8.5).
    pub autostart: bool,
    /// The bandwidth limit in bytes per second; 0 means unlimited (SPEC R7.1).
    pub bandwidth_bytes_per_second: u64,
    /// The number of parallel uploads (SPEC R7.2).
    pub concurrency: u32,
    /// The reconciliation scan interval in minutes (SPEC R2.3).
    pub scan_interval_minutes: u32,
    /// The retry attempt cap (SPEC R6.1).
    pub attempt_cap: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: false,
            bandwidth_bytes_per_second: 0,
            concurrency: 4,
            scan_interval_minutes: 30,
            attempt_cap: 8,
        }
    }
}

/// Why a settings file could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadProblem {
    /// The file that failed.
    pub path: PathBuf,
    /// The parse or read failure, as the reader reports it.
    pub reason: String,
}

impl fmt::Display for LoadProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} could not be read, so the defaults are in use: {}",
            self.path.display(),
            self.reason
        )
    }
}

/// The settings file, the values read from it, and any load problem.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
    problem: Option<LoadProblem>,
}

impl SettingsStore {
    /// Reads the settings file. A missing file is a first run, not a problem.
    ///
    /// A file that cannot be read or parsed yields the defaults and records the
    /// problem in [`Self::problem`].
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        match fs::read_to_string(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                path,
                settings: Settings::default(),
                problem: None,
            },
            Err(error) => Self::broken(path, format!("could not read it ({error})")),
            Ok(text) => match toml::from_str::<Settings>(&text) {
                Ok(settings) => Self {
                    path,
                    settings,
                    problem: None,
                },
                Err(error) => Self::broken(path, error.to_string()),
            },
        }
    }

    fn broken(path: PathBuf, reason: String) -> Self {
        Self {
            problem: Some(LoadProblem {
                path: path.clone(),
                reason,
            }),
            path,
            settings: Settings::default(),
        }
    }

    /// The values loaded from the file, or the defaults.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The load problem, when the file was unreadable or unparsable.
    #[must_use]
    pub fn problem(&self) -> Option<&LoadProblem> {
        self.problem.as_ref()
    }

    /// Applies `change` and saves the result. A no-op change writes nothing.
    ///
    /// The in-memory value changes only after the file was written, so a failed
    /// save leaves memory and disk agreeing on the previous values.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings directory cannot be created or the
    /// file cannot be written.
    pub fn update(&mut self, change: impl FnOnce(&mut Settings)) -> anyhow::Result<()> {
        let mut next = self.settings;
        change(&mut next);
        if next == self.settings {
            return Ok(());
        }
        self.write(&next)?;
        self.settings = next;
        // The file is valid again, so a load problem no longer describes it.
        self.problem = None;
        Ok(())
    }

    /// Writes `settings` beside the settings file, then renames it over the old
    /// file. A crash leaves either the old file or the new one, never a partial
    /// file.
    fn write(&self, settings: &Settings) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(settings).context("serialize the settings")?;
        let directory = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory)
            .with_context(|| format!("create the settings directory {}", directory.display()))?;

        let temporary = temporary_path(&self.path);
        let result = (|| -> anyhow::Result<()> {
            let mut file = fs::File::create(&temporary)
                .with_context(|| format!("create {}", temporary.display()))?;
            file.write_all(text.as_bytes())
                .with_context(|| format!("write {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", temporary.display()))?;
            fs::rename(&temporary, &self.path)
                .with_context(|| format!("replace {}", self.path.display()))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// The temporary file an atomic save writes before renaming it into place.
fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("toml.tmp")
}
