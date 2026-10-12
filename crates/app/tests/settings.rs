//! Headless tests for the settings seams: platform paths, the TOML store, and
//! autostart.
//!
//! Only tests that resolve real platform directories touch the process
//! environment; [`EnvGuard`] serializes them.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

#[cfg(target_os = "linux")]
use immich_sync::autostart::{Autostart, XdgAutostart, platform_autostart};
use immich_sync::paths::{APP_DIR, AppPaths, SETTINGS_FILE, STATE_FILE};
use immich_sync::settings::{Settings, SettingsStore};

/// Serializes the tests that mutate the process environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets process environment variables for one test, then restores them.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new(changes: &[(&'static str, Option<&Path>)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut saved = Vec::with_capacity(changes.len());
        for (name, value) in changes {
            saved.push((*name, std::env::var_os(name)));
            // SAFETY: every env-mutating test holds ENV_LOCK while it runs.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..).rev() {
            // SAFETY: see EnvGuard::new; the lock is still held here.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_paths_resolve_under_the_xdg_directories() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let home = temp.path().join("home");
    let config = temp.path().join("xdg-config");
    let data = temp.path().join("xdg-data");
    fs::create_dir_all(&home).expect("create the fake home");
    let _env = EnvGuard::new(&[
        ("HOME", Some(&home)),
        ("XDG_CONFIG_HOME", Some(&config)),
        ("XDG_DATA_HOME", Some(&data)),
    ]);

    let paths = AppPaths::resolve().expect("resolve the XDG paths");

    assert_eq!(
        paths.config_file,
        config.join(APP_DIR).join(SETTINGS_FILE),
        "settings live under $XDG_CONFIG_HOME"
    );
    assert_eq!(
        paths.state_db,
        data.join(APP_DIR).join(STATE_FILE),
        "the state database lives under $XDG_DATA_HOME"
    );
    assert_eq!(
        paths.autostart_dir,
        config.join("autostart"),
        "the XDG autostart entry lives under $XDG_CONFIG_HOME/autostart"
    );
    assert!(
        paths.state_db.starts_with(&data),
        "the state database never leaves the local data directory"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_paths_fall_back_to_the_home_directories() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let _env = EnvGuard::new(&[
        ("HOME", Some(&home)),
        ("XDG_CONFIG_HOME", None),
        ("XDG_DATA_HOME", None),
    ]);

    let paths = AppPaths::resolve().expect("resolve the fallback paths");

    assert_eq!(
        paths.config_file,
        home.join(".config").join(APP_DIR).join(SETTINGS_FILE)
    );
    assert_eq!(
        paths.state_db,
        home.join(".local/share").join(APP_DIR).join(STATE_FILE)
    );
}

#[test]
fn crates_sources_have_no_hardcoded_windows_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let sources = rust_sources(root.join("crates"));

    assert!(!sources.is_empty(), "found no crate source files");
    for path in &sources {
        let source = fs::read_to_string(path).expect("read a crate source file");
        for (line_number, line) in source.lines().enumerate() {
            if let Some(drive) = hardcoded_drive_path(line) {
                panic!(
                    "{}:{} hardcodes the Windows path `{drive}`: {line}",
                    path.display(),
                    line_number + 1
                );
            }
        }
    }
}

/// The first drive-letter path literal in `line`, if any.
///
/// A drive letter after another alphanumeric is not a path, as in `http` URLs.
fn hardcoded_drive_path(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    for index in 0..bytes.len().saturating_sub(2) {
        let separator = bytes[index + 2];
        if bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && (separator == b'\\' || separator == b'/')
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
        {
            return Some(&line[index..index + 3]);
        }
    }
    None
}

#[test]
fn default_settings_match_the_spec() {
    let settings = Settings::default();

    assert!(!settings.autostart, "R8.5: autostart defaults to off");
    assert_eq!(
        settings.bandwidth_bytes_per_second, 0,
        "R7.1: zero means unlimited"
    );
    assert_eq!(settings.concurrency, 4, "R7.2: concurrency defaults to 4");
    assert_eq!(
        settings.scan_interval_minutes, 30,
        "R2.3: the scan runs every 30 minutes"
    );
    assert_eq!(settings.attempt_cap, 8, "R6.1: the attempt cap defaults to 8");
}

#[test]
fn a_missing_file_loads_the_defaults_without_a_problem() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");

    let store = SettingsStore::open(path);

    assert_eq!(store.settings(), &Settings::default());
    assert!(store.problem().is_none(), "a first run is not a failure");
}

#[test]
fn settings_round_trip_across_a_restart() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");
    let mut store = SettingsStore::open(path.clone());

    store
        .update(|settings| {
            settings.autostart = true;
            settings.bandwidth_bytes_per_second = 1_500_000;
            settings.concurrency = 6;
            settings.scan_interval_minutes = 45;
            settings.attempt_cap = 3;
        })
        .expect("save the changed settings");

    // A fresh store is exactly what a restart sees.
    let restarted = SettingsStore::open(path.clone());
    let settings = restarted.settings();
    assert!(settings.autostart);
    assert_eq!(settings.bandwidth_bytes_per_second, 1_500_000);
    assert_eq!(settings.concurrency, 6);
    assert_eq!(settings.scan_interval_minutes, 45);
    assert_eq!(settings.attempt_cap, 3);
    assert!(restarted.problem().is_none());

    // The file is plain TOML, so a person can read and edit it.
    let text = fs::read_to_string(&path).expect("read the settings file");
    assert_eq!(
        text,
        "autostart = true\n\
         bandwidth_bytes_per_second = 1500000\n\
         concurrency = 6\n\
         scan_interval_minutes = 45\n\
         attempt_cap = 3\n"
    );
}

#[test]
fn corrupt_settings_fall_back_to_defaults_and_report_the_problem() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");
    fs::write(&path, "concurrency = \"four\"\n").expect("write the corrupt file");

    let store = SettingsStore::open(path.clone());

    assert_eq!(store.settings(), &Settings::default());
    let problem = store.problem().expect("a corrupt file is reported");
    assert_eq!(problem.path, path);
    assert!(
        !problem.reason.is_empty(),
        "the report names the failure: {problem:?}"
    );
    let message = problem.to_string();
    assert!(
        message.contains("settings.toml"),
        "the message names the file: {message}"
    );
}

#[test]
fn saving_repairs_a_corrupt_file() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");
    fs::write(&path, "not toml at all\n").expect("write the corrupt file");
    let mut store = SettingsStore::open(path.clone());

    store
        .update(|settings| settings.concurrency = 5)
        .expect("save over the corrupt file");

    let restarted = SettingsStore::open(path);
    assert_eq!(restarted.settings().concurrency, 5);
    assert!(
        restarted.problem().is_none(),
        "a repaired file loads without a problem"
    );
}

#[test]
fn save_leaves_only_the_settings_file_behind() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");
    let mut store = SettingsStore::open(path.clone());

    store
        .update(|settings| settings.concurrency = 5)
        .expect("save the settings");

    let names: Vec<String> = fs::read_dir(temp.path())
        .expect("read the settings directory")
        .map(|entry| entry.expect("read a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["settings.toml".to_string()],
        "the atomic write cleans up its temporary file"
    );
}

#[test]
fn a_failed_write_keeps_the_previous_file_and_value_intact() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("settings.toml");
    let mut store = SettingsStore::open(path.clone());
    store
        .update(|settings| settings.concurrency = 5)
        .expect("save the first settings");
    let before = fs::read_to_string(&path).expect("read the saved settings");

    // A directory where the sibling temporary file goes makes the write fail.
    fs::create_dir(temp.path().join("settings.toml.tmp")).expect("block the temporary file");

    let error = store
        .update(|settings| settings.concurrency = 9)
        .expect_err("a blocked write reports an error");

    assert!(!error.to_string().is_empty());
    assert_eq!(
        fs::read_to_string(&path).expect("the old file is still there"),
        before,
        "a failed save does not truncate the previous file"
    );
    assert_eq!(
        store.settings().concurrency,
        5,
        "a failed save does not change the in-memory value"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_xdg_backend_writes_and_removes_the_autostart_entry() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let directory = temp.path().join("autostart");
    let executable = PathBuf::from("/opt/immich sync/immich-sync");
    let autostart = XdgAutostart::new(directory.clone(), executable);

    assert!(!autostart.is_enabled().expect("read the initial state"));

    autostart.set_enabled(true).expect("enable autostart");
    let entry = directory.join("immich-sync.desktop");
    let text = fs::read_to_string(&entry).expect("read the desktop entry");
    assert_eq!(
        text,
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Immich Sync\n\
         Comment=Upload photos and videos to Immich in the background\n\
         Exec=\"/opt/immich sync/immich-sync\"\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    assert!(autostart.is_enabled().expect("read the enabled state"));

    // Enabling again keeps the same entry.
    autostart.set_enabled(true).expect("enable autostart again");
    assert_eq!(
        text,
        fs::read_to_string(&entry).expect("read the desktop entry again")
    );

    autostart.set_enabled(false).expect("disable autostart");
    assert!(!entry.exists(), "disabling removes the entry");
    assert!(!autostart.is_enabled().expect("read the disabled state"));
    // Disabling again is not an error.
    autostart.set_enabled(false).expect("disable autostart again");
}

#[cfg(target_os = "linux")]
#[test]
fn the_platform_backend_resolves_the_xdg_autostart_directory() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let home = temp.path().join("home");
    let config = temp.path().join("xdg-config");
    fs::create_dir_all(&home).expect("create the fake home");
    let _env = EnvGuard::new(&[
        ("HOME", Some(&home)),
        ("XDG_CONFIG_HOME", Some(&config)),
        ("XDG_DATA_HOME", Some(&temp.path().join("xdg-data"))),
    ]);
    let paths = AppPaths::resolve().expect("resolve the XDG paths");
    let autostart = platform_autostart(&paths).expect("the Linux backend is available");

    autostart.set_enabled(true).expect("enable autostart");
    let entry = config.join("autostart/immich-sync.desktop");
    assert!(
        entry.is_file(),
        "the entry lands in the XDG autostart directory"
    );
    assert!(autostart.is_enabled().expect("read the enabled state"));

    autostart.set_enabled(false).expect("disable autostart");
    assert!(!entry.exists());
}

/// Every `.rs` file under `root`.
fn rust_sources(root: PathBuf) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read a source directory") {
            let path = entry.expect("read a source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources
}
