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
use smappservice_rs::{AppService, ServiceManagementError, ServiceStatus, ServiceType};

#[cfg(target_os = "macos")]
use crate::BUNDLE_ID;
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
    /// macOS registers an item and then leaves the last step to the user, so
    /// the setting stays on and the row says where to finish it.
    fn approval_needed(&self) -> Option<&'static str> {
        None
    }

    /// Opens the OS page where the user approves the login item.
    ///
    /// The OS reports no result for the open, so a platform with no approval
    /// step simply does nothing.
    fn open_approval_settings(&self) {}
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

/// Shown while macOS holds the login item until the user approves it.
#[cfg(target_os = "macos")]
const APPROVAL_HINT: &str = "macOS needs your approval in System Settings › General › Login Items.";

/// Shown when the user denied the login item in a system prompt.
#[cfg(target_os = "macos")]
const LAUNCH_DENIED_HINT: &str = "macOS is blocking the login item. Open System Settings › General › Login Items and allow Immich Sync.";

/// Shown when macOS refuses the login item for the bundle's signature.
#[cfg(target_os = "macos")]
const INVALID_SIGNATURE_HINT: &str = "macOS refused the login item because the app is not code signed. The LaunchAgent fallback starts the app at login instead.";

/// What macOS reports for the app's login item, as a plain value.
///
/// The crate's `status()` is mapped to this beside the call, so the rule the
/// switch follows is a value the Linux tests can check.
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
    /// Whether the client starts at login. `RequiresApproval` reads as on: the
    /// item is registered, the user has one step left, and the switch must not
    /// flip itself off while macOS holds the registration.
    const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled | Self::RequiresApproval)
    }

    /// Whether macOS holds the item but waits for the user to allow it.
    const fn requires_approval(self) -> bool {
        matches!(self, Self::RequiresApproval)
    }
}

/// Why macOS refused a login-item call, as a plain value.
///
/// It names only the differences the switch acts on: every other documented
/// error keeps the crate's own text and stays an error.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// The bundle's signature does not meet ServiceManagement's requirements.
    InvalidSignature,
    /// ServiceManagement failed internally.
    InternalFailure,
    /// The user denied the login item in a system prompt.
    LaunchDeniedByUser,
    /// Any other documented ServiceManagement error.
    Documented,
    /// A code the crate does not recognize; Apple's public constants start at 2.
    Unknown(u32),
}

#[cfg(any(target_os = "macos", test))]
impl Refusal {
    /// The refusal macOS reported.
    #[cfg(target_os = "macos")]
    fn of(error: &ServiceManagementError) -> Self {
        match error {
            ServiceManagementError::InvalidSignature => Self::InvalidSignature,
            ServiceManagementError::InternalFailure => Self::InternalFailure,
            ServiceManagementError::LaunchDeniedByUser => Self::LaunchDeniedByUser,
            ServiceManagementError::Unknown(code) => Self::Unknown(*code),
            _ => Self::Documented,
        }
    }

    /// Whether the refusal means this bundle cannot be a login item, so the
    /// LaunchAgent starts the app instead (ADR 0008).
    ///
    /// `InternalFailure` and `Unknown` are included on hardware evidence: an
    /// unsigned x86 bundle makes `register()` return code 1, which is neither
    /// the documented `InvalidSignature` (3) nor a public constant. The plist is
    /// the ADR's escape hatch, the same switch removes it, and writing it is
    /// logged, so a false positive costs a warning rather than a stuck setting.
    const fn is_bundle_refusal(self) -> bool {
        matches!(
            self,
            Self::InvalidSignature | Self::InternalFailure | Self::Unknown(_)
        )
    }
}

/// What `enable` does about a refused register call.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Write the LaunchAgent and let it start the app at login.
    LaunchAgent,
    /// Report the refusal; no file is written.
    Report,
}

/// The choice `enable` makes, as a value the Linux tests can check.
#[cfg(any(target_os = "macos", test))]
const fn after_register(refusal: Refusal) -> Action {
    if refusal.is_bundle_refusal() {
        Action::LaunchAgent
    } else {
        Action::Report
    }
}

/// The message for a register or unregister refusal.
///
/// This is the single place refusal messages are built. The refusals the user
/// can act on name the fix, an unknown code keeps its number so a report is
/// actionable, and the documented errors keep the crate's own text, which is the
/// only place macOS says why.
#[cfg(target_os = "macos")]
fn refusal(error: &ServiceManagementError) -> anyhow::Error {
    match Refusal::of(error) {
        Refusal::LaunchDeniedByUser => anyhow::anyhow!("{LAUNCH_DENIED_HINT}"),
        Refusal::InvalidSignature => anyhow::anyhow!("{INVALID_SIGNATURE_HINT}"),
        Refusal::Unknown(code) => anyhow::anyhow!(
            "macOS refused the login item and reported an unrecognized error code {code}"
        ),
        Refusal::InternalFailure | Refusal::Documented => anyhow::anyhow!("{error}"),
    }
}

/// The LaunchAgent plist that starts `executable` at login (ADR 0008).
///
/// It is plain text so the Linux tests can check it. Only the macOS fallback
/// writes it, and only after macOS refuses the login item (ADR 0008).
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
/// An unsigned bundle is refused (code 1 on x86_64, `InvalidSignature` on a
/// signed host); ADR 0008 then writes a LaunchAgent plist instead, so the setting
/// still works.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
pub struct MacAutostart {
    executable: PathBuf,
    /// The directory the unsigned-bundle fallback writes its plist to.
    launch_agents: PathBuf,
}

#[cfg(target_os = "macos")]
impl MacAutostart {
    /// The app's own login item, and the executable the fallback starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the running executable or the home directory
    /// cannot be found.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            executable: std::env::current_exe().context("find the running executable")?,
            launch_agents: directories::BaseDirs::new()
                .context("find the home directory for a login item file")?
                .home_dir()
                .join("Library/LaunchAgents"),
        })
    }

    /// The LaunchAgent file the unsigned-bundle fallback writes.
    fn plist_path(&self) -> PathBuf {
        self.launch_agents.join(format!("{BUNDLE_ID}.plist"))
    }

    /// Whether the fallback, rather than the login item, starts the app.
    ///
    /// The file counts only when it starts the executable the app runs from:
    /// launchd keeps the absolute path, so a moved or rebuilt app would leave a
    /// file that starts nothing while the switch claimed on.
    fn launch_agent_current(&self) -> bool {
        std::fs::read_to_string(self.plist_path())
            .is_ok_and(|plist| plist == launch_agent_plist(BUNDLE_ID, &self.executable))
    }

    /// Registers the login item, or writes the LaunchAgent when macOS refuses
    /// the bundle.
    fn enable(&self) -> anyhow::Result<()> {
        if self.launch_agent_current() {
            return Ok(());
        }
        match AppService::new(ServiceType::MainApp).register() {
            Ok(()) | Err(ServiceManagementError::AlreadyRegistered) => Ok(()),
            Err(error) => match after_register(Refusal::of(&error)) {
                Action::LaunchAgent => self.write_launch_agent(),
                Action::Report => Err(refusal(&error).context("start the app at login")),
            },
        }
    }

    /// Removes whichever mechanism starts the app at login.
    fn disable(&self) -> anyhow::Result<()> {
        let path = self.plist_path();
        if path.is_file() {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        match AppService::new(ServiceType::MainApp).unregister() {
            Ok(()) | Err(ServiceManagementError::JobNotFound) => Ok(()),
            Err(error) => Err(refusal(&error).context("stop the app starting at login")),
        }
    }

    /// Writes the LaunchAgent that starts the current executable at login.
    ///
    /// This is the ADR 0008 fallback, so it is recorded: the app then starts at
    /// login through a second mechanism that no login-item status shows.
    fn write_launch_agent(&self) -> anyhow::Result<()> {
        let path = self.plist_path();
        std::fs::create_dir_all(&self.launch_agents)
            .with_context(|| format!("create {}", self.launch_agents.display()))?;
        std::fs::write(&path, launch_agent_plist(BUNDLE_ID, &self.executable))
            .with_context(|| format!("write {}", path.display()))?;
        tracing::warn!(
            path = %path.display(),
            "macOS refused the login item for the app's signature; a LaunchAgent now starts it at login"
        );
        Ok(())
    }
}

/// Reads the login item state the settings switch shows.
#[cfg(target_os = "macos")]
fn login_item_status() -> LoginItemStatus {
    match AppService::new(ServiceType::MainApp).status() {
        ServiceStatus::Enabled => LoginItemStatus::Enabled,
        ServiceStatus::RequiresApproval => LoginItemStatus::RequiresApproval,
        ServiceStatus::NotRegistered => LoginItemStatus::NotRegistered,
        ServiceStatus::NotFound => LoginItemStatus::NotFound,
    }
}

#[cfg(target_os = "macos")]
impl Autostart for MacAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        if self.launch_agent_current() {
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
        // The fallback already starts the app, so a leftover registration must
        // not ask for an approval that changes nothing.
        (!self.launch_agent_current() && login_item_status().requires_approval())
            .then_some(APPROVAL_HINT)
    }

    fn open_approval_settings(&self) {
        AppService::open_system_settings_login_items();
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

    use crate::BUNDLE_ID;

    use super::{Action, LoginItemStatus, Refusal, after_register, launch_agent_plist};

    #[test]
    fn a_registered_login_item_reads_as_on() {
        assert!(LoginItemStatus::Enabled.is_enabled());
        assert!(
            LoginItemStatus::RequiresApproval.is_enabled(),
            "the registration exists, so the switch must not flip itself off"
        );
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
    fn a_refused_bundle_takes_the_launch_agent_fallback() {
        // 3 is the documented InvalidSignature, 2 the documented InternalFailure,
        // 1 is what an unsigned x86 bundle actually reports, and 4242 stands in
        // for any other undocumented code.
        for refusal in [
            Refusal::InvalidSignature,
            Refusal::InternalFailure,
            Refusal::Unknown(1),
            Refusal::Unknown(4242),
        ] {
            assert!(refusal.is_bundle_refusal(), "{refusal:?}");
            assert_eq!(after_register(refusal), Action::LaunchAgent, "{refusal:?}");
        }
    }

    #[test]
    fn a_user_denied_login_item_writes_no_launch_agent() {
        assert_eq!(
            after_register(Refusal::LaunchDeniedByUser),
            Action::Report,
            "a plist behind the user's denial would start the app anyway"
        );
    }

    #[test]
    fn the_other_documented_refusals_stay_errors() {
        assert!(!Refusal::Documented.is_bundle_refusal());
        assert_eq!(after_register(Refusal::Documented), Action::Report);
    }

    #[test]
    fn the_launch_agent_label_is_the_packager_identifier() {
        assert!(
            include_str!("../Cargo.toml").contains(&format!("identifier = \"{BUNDLE_ID}\"")),
            "the LaunchAgent label must stay the bundle identifier the package is built with"
        );
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

/// The message mapping needs the macOS-only crate, so macOS CI runs these; the
/// probe type-checks them, and the Linux tests above keep the decisions they do
/// not reach.
#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::{
        Action, INVALID_SIGNATURE_HINT, LAUNCH_DENIED_HINT, Refusal, after_register, refusal,
    };
    use smappservice_rs::ServiceManagementError;

    #[test]
    fn a_refusal_the_user_can_act_on_names_the_fix() {
        assert_eq!(
            format!("{:#}", refusal(&ServiceManagementError::LaunchDeniedByUser)),
            LAUNCH_DENIED_HINT
        );
        assert_eq!(
            format!("{:#}", refusal(&ServiceManagementError::InvalidSignature)),
            INVALID_SIGNATURE_HINT
        );
    }

    #[test]
    fn any_other_refusal_keeps_the_os_text() {
        let known = format!(
            "{:#}",
            refusal(&ServiceManagementError::AuthorizationFailure)
        );
        assert!(known.contains("authorization"), "{known}");
    }

    #[test]
    fn an_unknown_refusal_names_the_login_item_and_keeps_the_code() {
        let unknown = format!("{:#}", refusal(&ServiceManagementError::Unknown(99)));
        assert!(unknown.contains("refused the login item"), "{unknown}");
        assert!(unknown.contains("99"), "{unknown}");
    }

    #[test]
    fn only_the_bundle_refusals_take_the_launch_agent_fallback() {
        for error in [
            ServiceManagementError::InvalidSignature,
            ServiceManagementError::InternalFailure,
            ServiceManagementError::Unknown(1),
            ServiceManagementError::Unknown(4242),
        ] {
            assert_eq!(
                after_register(Refusal::of(&error)),
                Action::LaunchAgent,
                "{error:?}"
            );
        }
        for error in [
            ServiceManagementError::LaunchDeniedByUser,
            ServiceManagementError::AuthorizationFailure,
            ServiceManagementError::ToolNotValid,
            ServiceManagementError::ServiceUnavailable,
            ServiceManagementError::JobPlistNotFound,
            ServiceManagementError::JobMustBeEnabled,
            ServiceManagementError::InvalidPlist,
        ] {
            assert_eq!(
                after_register(Refusal::of(&error)),
                Action::Report,
                "{error:?}"
            );
        }
    }
}
