//! Autostart at login (SPEC R8.5), off by default.
//!
//! The trait is the seam: the real platform backends write the Windows Run key
//! and the Linux XDG autostart entry, and tests supply a fake.

use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Context as _;

use crate::paths::AppPaths;

/// The name the client registers under in the OS autostart mechanism.
pub const AUTOSTART_NAME: &str = "Immich Sync";

/// Turns autostart at login on and off in the real OS mechanism.
pub trait Autostart {
    /// Whether the client starts at login.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS mechanism cannot be read.
    fn is_enabled(&self) -> anyhow::Result<bool>;

    /// Starts or stops the client at login. Enabling twice and disabling twice
    /// are both no-ops.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS mechanism cannot be written.
    fn set_enabled(&self, enabled: bool) -> anyhow::Result<()>;
}

/// The autostart backend for this platform.
///
/// # Errors
///
/// Returns an error when the running executable cannot be found.
#[cfg(target_os = "linux")]
pub fn platform_autostart(paths: &AppPaths) -> anyhow::Result<Rc<dyn Autostart>> {
    let executable = std::env::current_exe().context("find the running executable")?;
    Ok(Rc::new(XdgAutostart::new(
        paths.autostart_dir.clone(),
        executable,
    )))
}

/// The autostart backend for this platform.
///
/// # Errors
///
/// Returns an error when the running executable cannot be found.
#[cfg(windows)]
pub fn platform_autostart(_paths: &AppPaths) -> anyhow::Result<Rc<dyn Autostart>> {
    let executable = std::env::current_exe().context("find the running executable")?;
    Ok(Rc::new(RunKeyAutostart::new(executable)))
}

/// macOS is deferred for v1, so no OS mechanism is wired yet.
///
/// # Errors
///
/// Never fails; the returned backend reports the missing platform.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn platform_autostart(_paths: &AppPaths) -> anyhow::Result<Rc<dyn Autostart>> {
    Ok(Rc::new(UnavailableAutostart::new(
        "autostart is not implemented on this platform yet".to_string(),
    )))
}

/// The Linux XDG autostart entry (SPEC R9.3).
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct XdgAutostart {
    directory: PathBuf,
    executable: PathBuf,
}

#[cfg(target_os = "linux")]
impl XdgAutostart {
    /// An entry in `directory` that starts `executable`.
    #[must_use]
    pub fn new(directory: PathBuf, executable: PathBuf) -> Self {
        Self {
            directory,
            executable,
        }
    }

    /// The `.desktop` file this backend writes.
    #[must_use]
    pub fn entry_path(&self) -> PathBuf {
        self.directory.join("immich-sync.desktop")
    }
}

#[cfg(target_os = "linux")]
impl Autostart for XdgAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(self.entry_path().is_file())
    }

    fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let entry = self.entry_path();
        if enabled {
            std::fs::create_dir_all(&self.directory).with_context(|| {
                format!("create the autostart directory {}", self.directory.display())
            })?;
            std::fs::write(&entry, desktop_entry(&self.executable))
                .with_context(|| format!("write {}", entry.display()))?;
        } else if entry.exists() {
            std::fs::remove_file(&entry).with_context(|| format!("remove {}", entry.display()))?;
        }
        Ok(())
    }
}

/// The desktop entry body, as the Desktop Entry Specification formats it.
#[cfg(target_os = "linux")]
fn desktop_entry(executable: &std::path::Path) -> String {
    let quoted = executable
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={AUTOSTART_NAME}\n\
         Comment=Upload photos and videos to Immich in the background\n\
         Exec=\"{quoted}\"\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// The Windows Run key (SPEC R9.3).
#[cfg(windows)]
#[derive(Clone, Debug)]
pub struct RunKeyAutostart {
    executable: PathBuf,
}

#[cfg(windows)]
impl RunKeyAutostart {
    /// A Run value that starts `executable`.
    #[must_use]
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }

    fn command(&self) -> String {
        self.executable.display().to_string()
    }
}

/// The `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key.
#[cfg(windows)]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

#[cfg(windows)]
impl Autostart for RunKeyAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        // A missing key or value means the client does not start at login.
        let Ok(key) = windows_registry::CURRENT_USER.open(RUN_KEY) else {
            return Ok(false);
        };
        Ok(key
            .get_string(AUTOSTART_NAME)
            .is_ok_and(|value| !value.trim().is_empty()))
    }

    fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let key = windows_registry::CURRENT_USER
            .create(RUN_KEY)
            .map_err(|error| anyhow::anyhow!("open the Windows Run key: {error}"))?;
        if enabled {
            key.set_string(AUTOSTART_NAME, self.command())
                .map_err(|error| anyhow::anyhow!("write the Run value: {error}"))?;
        } else if key.get_string(AUTOSTART_NAME).is_ok() {
            key.remove_value(AUTOSTART_NAME)
                .map_err(|error| anyhow::anyhow!("remove the Run value: {error}"))?;
        }
        Ok(())
    }
}

/// A backend that reports why autostart cannot be applied.
#[derive(Clone, Debug)]
pub struct UnavailableAutostart {
    reason: String,
}

impl UnavailableAutostart {
    /// Reports `reason` whenever the toggle is used.
    #[must_use]
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl Autostart for UnavailableAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn set_enabled(&self, _enabled: bool) -> anyhow::Result<()> {
        anyhow::bail!("{}", self.reason)
    }
}
