//! Autostart at login (SPEC R8.5), off by default.
//!
//! The trait is the seam: the real platform backends write the Windows Run key,
//! the Linux XDG autostart entry, and the macOS login item, and tests supply a
//! fake.

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::path::PathBuf;
use std::rc::Rc;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use anyhow::Context as _;

#[cfg(target_os = "macos")]
use objc2_service_management::{
    SMAppService, SMAppServiceStatus, kSMErrorAlreadyRegistered, kSMErrorInvalidSignature,
    kSMErrorJobNotFound,
};

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

    /// The text to show when the OS holds the login item but waits for the user
    /// to allow it, or `None` when no approval is pending.
    ///
    /// macOS registers an item and then leaves it to the user; the setting reads
    /// as off until they approve it, so the row has to say where.
    fn approval_needed(&self) -> Option<&'static str> {
        None
    }

    /// Opens the OS page where the user approves the login item.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform has no approval step or the page
    /// cannot be opened.
    fn open_approval_settings(&self) -> anyhow::Result<()> {
        anyhow::bail!("this platform has no login-item approval step")
    }
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

/// The autostart backend for this platform.
///
/// `AppPaths` is ignored: the login item is addressed by bundle identity, not by
/// a path (the LaunchAgent fallback resolves its own directory).
///
/// # Errors
///
/// Returns an error when the running executable cannot be found.
#[cfg(target_os = "macos")]
pub fn platform_autostart(_paths: &AppPaths) -> anyhow::Result<Rc<dyn Autostart>> {
    Ok(Rc::new(MacAutostart::new()?))
}

/// No autostart mechanism is wired for this platform.
///
/// # Errors
///
/// Never fails; the returned backend reports the missing platform.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
                format!(
                    "create the autostart directory {}",
                    self.directory.display()
                )
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

/// The bundle identifier from the packager metadata, and so the label of the
/// login item and of the LaunchAgent file the fallback writes. One label per
/// app, whichever mechanism carries it.
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_ID: &str = "com.elementalvoid.immich-sync";

/// Shown while macOS holds the login item until the user approves it.
#[cfg(any(target_os = "macos", test))]
const APPROVAL_HINT: &str = "macOS needs your approval in System Settings › General › Login Items.";

/// `kSMErrorInvalidSignature`: the bundle is not signed, so macOS refuses the
/// login item and ADR 0008 falls back to a LaunchAgent.
#[cfg(any(target_os = "macos", test))]
const SM_ERROR_INVALID_SIGNATURE: isize = 3;
/// `kSMErrorJobNotFound`: nothing is registered, so disabling is already done.
#[cfg(any(target_os = "macos", test))]
const SM_ERROR_JOB_NOT_FOUND: isize = 6;
/// `kSMErrorAlreadyRegistered`: something is registered, so enabling is already
/// done.
#[cfg(any(target_os = "macos", test))]
const SM_ERROR_ALREADY_REGISTERED: isize = 12;

/// The pinned crate's constants these copy, checked when the probe type-checks
/// the macOS arm, so a pin that moves the numbers fails there.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(SM_ERROR_INVALID_SIGNATURE == kSMErrorInvalidSignature as isize);
    assert!(SM_ERROR_JOB_NOT_FOUND == kSMErrorJobNotFound as isize);
    assert!(SM_ERROR_ALREADY_REGISTERED == kSMErrorAlreadyRegistered as isize);
};

/// What macOS reports for the app's login item, as a plain value.
///
/// The unsafe `status()` call is mapped to this beside the call, so the rule
/// the switch follows is a value the Linux tests can check.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginItemStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
}

#[cfg(any(target_os = "macos", test))]
impl LoginItemStatus {
    /// Whether the client starts at login. `RequiresApproval` reads as off: the
    /// item exists, but the user has not allowed it.
    const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Whether macOS holds the item but waits for the user to allow it.
    const fn requires_approval(self) -> bool {
        matches!(self, Self::RequiresApproval)
    }
}

/// The result of a register call, once the item's new status is known.
///
/// A registration that waits for the user is not success: the setting stays
/// off and the row says where to approve it.
#[cfg(any(target_os = "macos", test))]
fn registration_result(status: LoginItemStatus) -> anyhow::Result<()> {
    if status.requires_approval() {
        anyhow::bail!("{APPROVAL_HINT}");
    }
    Ok(())
}

/// Whether an SMError means the requested state already holds.
#[cfg(any(target_os = "macos", test))]
fn is_benign(error_code: isize) -> bool {
    matches!(
        error_code,
        SM_ERROR_ALREADY_REGISTERED | SM_ERROR_JOB_NOT_FOUND
    )
}

/// The error shown for an SMAppService refusal, with the OS message and code.
/// The message is never dropped: it is the only place macOS says why.
#[cfg(any(target_os = "macos", test))]
fn os_error(action: &str, error_code: isize, message: &str) -> anyhow::Error {
    anyhow::anyhow!("{action}: {message} (SMError {error_code})")
}

/// The LaunchAgent plist that starts `executable` at login (ADR 0008).
///
/// It is plain text so the Linux tests can check it. Only the macOS fallback
/// writes it, and only after macOS refuses the login item for its signature.
#[cfg(any(target_os = "macos", test))]
fn launch_agent_plist(label: &str, executable: &std::path::Path) -> String {
    let label = xml_escape(label);
    let executable = xml_escape(&executable.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{executable}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Escapes the XML characters a label or a path may contain.
#[cfg(any(target_os = "macos", test))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The macOS login item (SMAppService, macOS 13+).
///
/// An unsigned bundle is refused with `kSMErrorInvalidSignature`; ADR 0008 then
/// writes a LaunchAgent plist instead, so the setting still works.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
pub struct MacAutostart {
    executable: PathBuf,
    /// `None` when the platform exposes no home directory; the login item still
    /// works, only the fallback cannot be written.
    launch_agents: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
impl MacAutostart {
    /// The app's own login item, and the executable the fallback starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the running executable cannot be found.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            executable: std::env::current_exe().context("find the running executable")?,
            launch_agents: directories::BaseDirs::new()
                .map(|base| base.home_dir().join("Library/LaunchAgents")),
        })
    }

    /// The LaunchAgent file the unsigned-bundle fallback writes.
    fn plist_path(&self) -> Option<PathBuf> {
        self.launch_agents
            .as_ref()
            .map(|directory| directory.join(format!("{MACOS_BUNDLE_ID}.plist")))
    }

    /// Whether the fallback, rather than the login item, starts the app.
    fn launch_agent_installed(&self) -> bool {
        self.plist_path().is_some_and(|path| path.is_file())
    }

    /// Registers the login item, or writes the LaunchAgent when the bundle is
    /// unsigned.
    fn enable(&self) -> anyhow::Result<()> {
        if self.launch_agent_installed() {
            return Ok(());
        }
        // SAFETY: `mainAppService` takes no arguments and has no precondition;
        // SMAppService is not main-thread-only, so any thread may call it.
        let service = unsafe { SMAppService::mainAppService() };
        // SAFETY: registration only asks the OS to add the app's login item.
        match unsafe { service.registerAndReturnError() } {
            Ok(()) => registration_result(login_item_status()),
            Err(error) => {
                let code = error.code();
                if code == SM_ERROR_INVALID_SIGNATURE {
                    self.write_launch_agent()
                } else if is_benign(code) {
                    // Already registered: succeed, unless the user still has to
                    // approve the item.
                    registration_result(login_item_status())
                } else {
                    Err(os_error(
                        "start the app at login",
                        code,
                        &error.localizedDescription().to_string(),
                    ))
                }
            }
        }
    }

    /// Removes whichever mechanism starts the app at login.
    fn disable(&self) -> anyhow::Result<()> {
        if let Some(path) = self.plist_path().filter(|path| path.is_file()) {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        // SAFETY: `mainAppService` takes no arguments and has no precondition.
        let service = unsafe { SMAppService::mainAppService() };
        // SAFETY: unregistration only asks the OS to remove the login item.
        match unsafe { service.unregisterAndReturnError() } {
            Ok(()) => Ok(()),
            Err(error) => {
                let code = error.code();
                if is_benign(code) {
                    Ok(())
                } else {
                    Err(os_error(
                        "stop the app starting at login",
                        code,
                        &error.localizedDescription().to_string(),
                    ))
                }
            }
        }
    }

    /// Writes the LaunchAgent that starts the same executable at login.
    fn write_launch_agent(&self) -> anyhow::Result<()> {
        let Some(directory) = &self.launch_agents else {
            anyhow::bail!("no home directory for a login item file");
        };
        let path = directory.join(format!("{MACOS_BUNDLE_ID}.plist"));
        std::fs::create_dir_all(directory)
            .with_context(|| format!("create {}", directory.display()))?;
        std::fs::write(&path, launch_agent_plist(MACOS_BUNDLE_ID, &self.executable))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

/// Reads the login item state the settings switch shows.
#[cfg(target_os = "macos")]
fn login_item_status() -> LoginItemStatus {
    // SAFETY: `mainAppService` takes no arguments and has no precondition.
    let service = unsafe { SMAppService::mainAppService() };
    // SAFETY: `status` only reads the current registration.
    match unsafe { service.status() } {
        SMAppServiceStatus::Enabled => LoginItemStatus::Enabled,
        SMAppServiceStatus::RequiresApproval => LoginItemStatus::RequiresApproval,
        SMAppServiceStatus::NotRegistered => LoginItemStatus::NotRegistered,
        _ => LoginItemStatus::NotFound,
    }
}

#[cfg(target_os = "macos")]
impl Autostart for MacAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        if self.launch_agent_installed() {
            return Ok(true);
        }
        Ok(login_item_status().is_enabled())
    }

    fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            self.enable()
        } else {
            self.disable()
        }
    }

    fn approval_needed(&self) -> Option<&'static str> {
        login_item_status()
            .requires_approval()
            .then_some(APPROVAL_HINT)
    }

    fn open_approval_settings(&self) -> anyhow::Result<()> {
        // SAFETY: opens the Login Items pane; no inputs and no precondition.
        unsafe { SMAppService::openSystemSettingsLoginItems() };
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        LoginItemStatus, SM_ERROR_ALREADY_REGISTERED, SM_ERROR_INVALID_SIGNATURE,
        SM_ERROR_JOB_NOT_FOUND, is_benign, launch_agent_plist, os_error, registration_result,
    };

    #[test]
    fn only_an_enabled_login_item_reads_as_on() {
        assert!(LoginItemStatus::Enabled.is_enabled());
        assert!(!LoginItemStatus::RequiresApproval.is_enabled());
        assert!(!LoginItemStatus::NotRegistered.is_enabled());
        assert!(!LoginItemStatus::NotFound.is_enabled());
    }

    #[test]
    fn only_requires_approval_asks_the_user_to_approve() {
        assert!(LoginItemStatus::RequiresApproval.requires_approval());
        for status in [
            LoginItemStatus::Enabled,
            LoginItemStatus::NotRegistered,
            LoginItemStatus::NotFound,
        ] {
            assert!(!status.requires_approval(), "{status:?}");
        }
    }

    #[test]
    fn a_registration_that_waits_for_approval_is_not_success() {
        assert!(registration_result(LoginItemStatus::Enabled).is_ok());
        let error = registration_result(LoginItemStatus::RequiresApproval)
            .expect_err("a pending approval must not claim success");
        assert!(
            format!("{error:#}").contains("System Settings"),
            "the failure says where to approve it"
        );
    }

    #[test]
    fn the_benign_sm_errors_mean_the_state_already_holds() {
        assert!(is_benign(SM_ERROR_ALREADY_REGISTERED));
        assert!(is_benign(SM_ERROR_JOB_NOT_FOUND));
        assert!(!is_benign(SM_ERROR_INVALID_SIGNATURE));
        assert!(!is_benign(11), "kSMErrorLaunchDeniedByUser is a refusal");
        assert!(!is_benign(0));
    }

    #[test]
    fn an_os_refusal_keeps_the_os_message_and_code() {
        let message = format!(
            "{:#}",
            os_error("start the app at login", 11, "The operation was denied.")
        );
        assert!(message.contains("start the app at login"), "{message}");
        assert!(message.contains("The operation was denied."), "{message}");
        assert!(message.contains("11"), "{message}");
    }

    #[test]
    fn the_launch_agent_plist_starts_the_executable_at_login() {
        let plist = launch_agent_plist(
            "com.example.immich-sync",
            Path::new("/Applications/Immich & Sync.app/Contents/MacOS/immich-sync"),
        );
        assert!(
            plist.contains("<string>com.example.immich-sync</string>"),
            "{plist}"
        );
        assert!(
            plist.contains(
                "<string>/Applications/Immich &amp; Sync.app/Contents/MacOS/immich-sync</string>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains("<key>ProgramArguments</key>\n\t<array>"),
            "{plist}"
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"), "{plist}");
    }
}
