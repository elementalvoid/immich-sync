//! The log file pipeline: the live level, the retention cap, and the tail.
//!
//! The tests install the sink on the test thread only
//! ([`LogSink::thread_default`]), because a process installs one subscriber
//! and the whole test binary shares it.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use immich_sync::logging::{
    LOG_PREFIX, LogLevel, LogPolicy, LogSink, newest_file, record_start, tail,
};
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use tracing_appender::rolling::Rotation;

/// Every log file name in `directory`.
fn log_names(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .expect("read the log directory")
        .map(|entry| {
            entry
                .expect("read a log directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with(LOG_PREFIX))
        .collect()
}

/// The text of the newest log file.
fn newest_text(directory: &Path) -> String {
    let path = newest_file(directory)
        .expect("find the newest log")
        .expect("a log file exists");
    fs::read_to_string(path).expect("read the log file")
}

/// Ages a file so [`newest_file`] has a deterministic order.
fn age(path: &Path, seconds: u64) {
    let old = SystemTime::now() - Duration::from_secs(seconds);
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open the file to age it")
        .set_modified(old)
        .expect("age the file");
}

#[test]
fn changing_the_level_changes_what_is_written_without_a_restart() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let mut sink =
        LogSink::start(temp.path(), LogPolicy::default(), LogLevel::Error).expect("start the log");
    let guard = sink.thread_default();

    tracing::debug!(marker = "hidden", "hidden diagnostic");
    tracing::error!(marker = "visible", "visible failure");

    sink.control()
        .set_level(LogLevel::Debug)
        .expect("raise the level");

    tracing::debug!(marker = "revealed", "revealed diagnostic");

    drop(guard);
    drop(sink);

    let text = newest_text(temp.path());
    assert!(
        !text.contains("hidden diagnostic"),
        "the error level hid a debug line: {text}"
    );
    assert!(
        text.contains("visible failure"),
        "the error line is missing: {text}"
    );
    assert!(
        text.contains("revealed diagnostic"),
        "the raised level applies without a restart: {text}"
    );
}

#[test]
fn the_appender_prunes_old_files_down_to_the_cap() {
    // The appender prunes when it opens the directory and again at a timed
    // rollover. Only the first is deterministic: the rollover prune needs the
    // appender's internal clock to pass a day boundary, so the code path is
    // shared but the timing is not exercised here.
    let temp = tempfile::tempdir().expect("create a temp dir");
    for date in ["2026-01-01", "2026-01-02", "2026-01-03"] {
        fs::write(temp.path().join(format!("{LOG_PREFIX}.{date}")), "old\n")
            .expect("write an old log file");
        // Distinct creation times, so the appender's oldest-first order is the
        // order these files were written.
        thread::sleep(Duration::from_millis(5));
    }
    let policy = LogPolicy {
        rotation: Rotation::DAILY,
        max_files: 2,
    };
    let cap = policy.max_files;

    let sink = LogSink::start(temp.path(), policy, LogLevel::Info).expect("start the log");
    drop(sink);

    let mut names = log_names(temp.path());
    names.sort();
    assert_eq!(names.len(), cap, "the cap pruned the old files: {names:?}");
    assert!(
        !names.iter().any(|name| name.ends_with("2026-01-01")),
        "the oldest file is gone: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.ends_with("2026-01-03")),
        "the newest old file survives: {names:?}"
    );
}

#[test]
fn the_tail_returns_the_latest_lines_of_the_newest_file() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let old = temp.path().join(format!("{LOG_PREFIX}.2026-01-01"));
    let current = temp.path().join(format!("{LOG_PREFIX}.2026-01-02"));
    fs::write(&old, "old line\n").expect("write the old log");
    fs::write(&current, "first\nsecond\nthird\n").expect("write the current log");
    age(&old, 60);

    assert_eq!(
        tail(temp.path(), 2).expect("tail the log directory"),
        ["second", "third"],
        "the tail reads the newest file and the latest lines"
    );
}

#[test]
fn the_start_line_names_the_version_and_the_resolved_paths() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = AppPaths {
        config_file: temp.path().join("config").join("settings.toml"),
        state_db: temp.path().join("data").join("state.db"),
        autostart_dir: temp.path().join("config").join("autostart"),
    };
    let logs = paths.log_directory();
    let mut sink =
        LogSink::start(&logs, LogPolicy::default(), LogLevel::Info).expect("start the log");
    let guard = sink.thread_default();

    record_start(&paths, Some("the platform exposed no home directory"));

    drop(guard);
    drop(sink);

    let text = newest_text(&logs);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "the version is in the start line: {text}"
    );
    assert!(
        text.contains(&paths.config_file.display().to_string()),
        "the config path is in the start line: {text}"
    );
    assert!(
        text.contains(&paths.state_db.display().to_string()),
        "the state path is in the start line: {text}"
    );
    assert!(
        text.contains(&logs.display().to_string()),
        "the log path is in the start line: {text}"
    );
    assert!(
        text.contains("no home directory"),
        "the resolution failure is in the start line: {text}"
    );
}

#[test]
fn a_settings_load_failure_and_its_repair_are_logged() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let config = temp.path().join("config").join("settings.toml");
    let logs = temp.path().join("state");
    let mut sink =
        LogSink::start(&logs, LogPolicy::default(), LogLevel::Info).expect("start the log");
    let guard = sink.thread_default();

    fs::create_dir_all(config.parent().expect("the settings file has a directory"))
        .expect("create the settings directory");
    fs::write(&config, "not toml at all\n").expect("write the corrupt settings file");
    let mut store = SettingsStore::open(config);
    assert!(store.problem().is_some(), "the corrupt file is reported");
    store
        .update(|settings| settings.concurrency = 7)
        .expect("repair the settings file");

    drop(guard);
    drop(sink);

    let text = newest_text(&logs);
    assert!(
        text.contains("could not be used"),
        "the load failure is in the log: {text}"
    );
    assert!(
        text.contains("repaired"),
        "the repair is in the log: {text}"
    );
}
