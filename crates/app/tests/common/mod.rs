//! Helpers shared by the app's integration tests.
//!
//! Cargo compiles this module into every test binary that declares
//! `mod common;`, so it only uses the dev-dependencies those binaries
//! already enable, and each binary uses a part of it. The per-item
//! `dead_code` allows say which parts that binary left out.

use gpui_kit::{App, ElementId, Window};
use mock_immich::MockImmich;

/// Starts the offline Immich double on its own runtime: a GPUI test runs on
/// the GPUI executor, which is not a Tokio runtime.
#[allow(dead_code)]
pub fn mock_server() -> (tokio::runtime::Runtime, MockImmich) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("start the mock runtime");
    let server = runtime.block_on(MockImmich::start());
    (runtime, server)
}

/// The WCAG 2.1 contrast ratio, 1.0 (identical) to 21.0.
///
/// The headless harness cannot read pixels, so a surface's ink and its
/// background are measured as theme tokens.
#[allow(dead_code)]
pub fn contrast(a: gpui_kit::Hsla, b: gpui_kit::Hsla) -> f32 {
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = |color: gpui_kit::Hsla| {
        let rgb = color.to_rgb();
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Scrolls the dashboard until the element with `id` is on screen.
///
/// #56: the global card carries the reconciliation scan line, so on a short
/// window a card's footer sits below the fold; a control is clicked only after
/// it is really visible.
#[allow(dead_code)]
pub fn scroll_dashboard_into_view(window: &mut Window, cx: &mut App, id: impl Into<ElementId>) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{ScrollDelta, point, px};

    let id = id.into();
    window.render_frame(cx);
    for _ in 0..60 {
        if window
            .try_find(id.clone())
            .is_some_and(|target| target.visible())
        {
            return;
        }
        window.scroll(
            "dashboard-scroll",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
    }
    panic!("{id:?} never became visible on the dashboard");
}

/// Waits for an alert dialog's entrance to settle before pressing it.
///
/// The surface slides in for 250 ms, and a press is dispatched from the bounds
/// read a moment earlier: mid-flight the control has moved on, so under load the
/// press lands beside it and the button looks dead. The entrance runs on the
/// wall clock, which the test executor's `advance_clock` does not move, hence
/// the sleep. A person aims at the control they can see, so this is the
/// harness's problem, not the dialog's (#73).
#[allow(dead_code)]
pub fn settle_alert(window: &mut Window, cx: &mut App) {
    use gpui_kit::test::TestWindowExt as _;

    std::thread::sleep(std::time::Duration::from_millis(350));
    window.render_frame(cx);
}

/// The select-all chord the GPUI text input binds on this platform.
///
/// `ctrl-a` is select-all on Linux and Windows, but macOS binds `ctrl-a` to
/// "move to the start of the line" and select-all to `cmd-a` (gpui-base's
/// `input/base/state.rs`), so a test that hardcodes `ctrl-a` types over the
/// start of the old value on macOS instead of replacing it.
#[allow(dead_code)]
pub fn select_all_chord() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    }
}

/// One row #89 moved onto the row contract: the id its surface finds it by, the
/// name it must announce, and the help sentence announced after that name.
pub struct MigratedRow {
    /// The surface that draws it: `logs` or `diagnostics`.
    pub surface: &'static str,
    pub id: &'static str,
    pub label: String,
    /// `None` is a claim too: #80's read-only pair names its value and announces
    /// no description, because its help is plain text beside the value.
    pub description: Option<String>,
}

/// The ten rows the Logs and Diagnostics cards migrated onto the row contract
/// (#89), with the words each one announces.
///
/// `directory` is the run's log directory, which is also the debug bundle's: the
/// log folder row names it, the current file row names the newest log file in
/// it, and the `Collect` help quotes it.
#[allow(dead_code)]
pub fn migrated_rows(directory: &str) -> Vec<MigratedRow> {
    let current = immich_sync::logging::newest_file(std::path::Path::new(directory))
        .ok()
        .flatten()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "No log file yet.".to_string());
    let collect_help = format!(
        "Writes one .zip into {directory}: the newest {} log files, {} each at most, \
         plus the versions, database, configuration, sizing, and runtime sections.",
        immich_sync::debug_bundle::LOG_FILES,
        immich_sync::diagnostics::format_bytes(immich_sync::debug_bundle::LOG_BYTES),
    );
    let row = |surface: &'static str, id: &'static str, label: &str, description: Option<&str>| {
        MigratedRow {
            surface,
            id,
            label: label.to_string(),
            description: description.map(str::to_string),
        }
    };
    vec![
        // The two log paths are read-only pairs, not label-and-control rows:
        // the value names the path and no description is announced (#80).
        row("logs", "logs-path-directory-value", directory, None::<&str>),
        row("logs", "logs-path-current-value", &current, None::<&str>),
        row(
            "logs",
            "logs-refresh-row",
            "Refresh",
            Some("Re-reads the tail of the newest log file."),
        ),
        row(
            "logs",
            "logs-copy-row",
            "Copy all",
            Some("Copies every line shown below, in order."),
        ),
        row(
            "logs",
            "logs-paused",
            "Live tail paused",
            Some("New lines keep arriving; the tail stays where you left it."),
        ),
        row(
            "diagnostics",
            "diagnostics-refresh-row",
            "Refresh",
            Some("Re-reads the state database and re-checks the server."),
        ),
        row(
            "diagnostics",
            "diagnostics-rebuild-row",
            "Rebuild",
            Some("Starts the state over and rescans every folder. The server is re-checked."),
        ),
        row(
            "diagnostics",
            "diagnostics-bundle-collect-row",
            "Collect",
            Some(&collect_help),
        ),
        row(
            "diagnostics",
            "diagnostics-bundle-copy-row",
            "Copy path",
            Some(
                "Puts the archive's full path on the clipboard, so it can be pasted into a \
                 message or a file manager.",
            ),
        ),
        row(
            "diagnostics",
            "diagnostics-bundle-reveal-row",
            "Show in folder",
            Some(
                "Opens the folder that holds the archive in your file manager, with the archive \
                 selected where the platform allows it.",
            ),
        ),
    ]
}

/// Asserts every row one surface draws is on screen with the name and the help
/// sentence the table gives it, so a migration that loses a word or an id fails
/// here instead of quietly changing what a screen reader reads (#89).
#[allow(dead_code)]
pub fn assert_migrated_rows(window: &Window, table: &[MigratedRow], surface: &str) {
    use gpui_kit::test::TestWindowExt as _;

    let mut seen = 0;
    for row in table.iter().filter(|row| row.surface == surface) {
        let found = window
            .try_find(row.id)
            .unwrap_or_else(|| panic!("{} is no longer drawn on {surface}", row.id));
        assert_eq!(
            found.label(),
            Some(row.label.as_str()),
            "{} keeps its accessible name",
            row.id
        );
        assert_eq!(
            found.description(),
            row.description.as_deref(),
            "{} keeps its help sentence",
            row.id
        );
        seen += 1;
    }
    assert_eq!(
        seen, 5,
        "the {surface} surface draws its five migrated rows"
    );
}
