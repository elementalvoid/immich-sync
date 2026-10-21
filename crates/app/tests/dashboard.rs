//! Headless tests for the dashboard seams.
//!
//! The view-model seams (card content, theme mapping) are pure functions, but
//! they run here under `#[gpui_kit::test]` with the rest so one command covers
//! the whole dashboard. The rendered-view test opens a real headless window and
//! reads native accessibility properties and layout bounds.

use std::collections::BTreeSet;

use gpui_kit::component::Theme;
use gpui_kit::component::ThemeMode;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Hsla, Point, Role,
    TestAppContext, Window, WindowAppearance, WindowBounds, WindowOptions, px, size,
};
use immich_sync::dashboard::{CardPalette, Dashboard, state_border};
use immich_sync::status::{ActivityState, DashboardModel, StatusCard, sample};
use immich_sync::theme::{SystemTheme, apply_system_theme, theme_mode};

fn open_dashboard(cx: &mut TestAppContext, model: DashboardModel) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1200.), px(800.)),
                })),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(|_| Dashboard::new(model)),
        )
        .expect("open the dashboard window")
        .0
    })
}

#[gpui_kit::test]
fn folder_card_content_maps_the_view_model() {
    let model = sample();
    let attention = model
        .folders
        .iter()
        .find(|folder| folder.activity == ActivityState::NeedsAttention)
        .expect("the sample data has a folder that needs attention");

    let content = attention.content();

    assert_eq!(content.title, attention.title);
    assert_eq!(content.path.as_deref(), attention.path.as_deref());
    assert_eq!(content.state, "Needs attention");
    assert_eq!(content.counts, "11 uploaded · 7 pending · 2 failed");
    assert_eq!(content.progress_percent, "31%");
    assert_eq!(content.error.as_deref(), attention.last_error.as_deref());
    assert!(
        content.error.is_some(),
        "a needs-attention card shows its last error"
    );
}

#[gpui_kit::test]
fn globals_and_idle_folders_map_without_an_error() {
    let model = sample();
    let global = model.global.content();

    assert_eq!(global.title, "All folders");
    assert_eq!(global.path, None);
    assert_eq!(global.state, "Syncing");
    assert_eq!(global.counts, "421 uploaded · 18 pending · 2 failed");
    assert_eq!(global.review.as_deref(), Some("3 missing files to review"));

    let idle = model
        .folders
        .iter()
        .find(|folder| folder.activity == ActivityState::Idle)
        .expect("the sample data has an idle folder")
        .content();
    assert_eq!(idle.state, "Idle");
    assert_eq!(idle.progress_percent, "100%");
    assert_eq!(idle.error, None);
    assert_eq!(idle.review.as_deref(), Some("3 missing files to review"));
}

#[gpui_kit::test]
fn the_global_card_reads_the_upload_rate_and_the_count() {
    use sync_core::meter::UploadTraffic;

    let model = sample();
    let global = model.global.content();
    assert_eq!(
        global.uploads.as_deref(),
        Some("2.4 MiB/s · 3 uploading"),
        "R8.1: the global card shows the rate and the uploads on the wire"
    );
    assert_eq!(
        model.folders[0].content().uploads,
        None,
        "the line belongs to the global card, which is the one that sums the folders"
    );

    // A request that has started and moved no byte yet is still uploading.
    let mut starting = model.global.clone();
    starting.uploads = UploadTraffic {
        bytes: 0,
        bytes_per_second: 0,
        in_flight: 2,
    };
    assert_eq!(
        starting.content().uploads.as_deref(),
        Some("0 B/s · 2 uploading")
    );
}

#[gpui_kit::test]
fn the_global_card_reads_idle_only_when_no_upload_is_moving() {
    use sync_core::meter::UploadTraffic;

    let mut card = sample().global;
    card.uploads = UploadTraffic::default();
    assert_eq!(card.content().uploads.as_deref(), Some("Idle"));

    // The window still holds the last burst after the upload finished: no
    // upload is running, but bytes did move, so the line shows the rate alone
    // instead of hiding a sub-poll burst behind `Idle`.
    card.uploads = UploadTraffic {
        bytes: 1_048_576,
        bytes_per_second: 524_288,
        in_flight: 0,
    };
    assert_eq!(card.content().uploads.as_deref(), Some("512.0 KiB/s"));

    // The burst faded out of the window: nothing is on the wire or moving.
    card.uploads = UploadTraffic {
        bytes: 1_048_576,
        bytes_per_second: 0,
        in_flight: 0,
    };
    assert_eq!(card.content().uploads.as_deref(), Some("Idle"));
}

#[gpui_kit::test]
fn the_global_card_renders_the_upload_line(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx, sample());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window
                .find((ElementId::from("status-global"), "uploads"))
                .label(),
            Some("2.4 MiB/s · 3 uploading"),
            "the line is rendered and readable to assistive technology"
        );
        assert!(
            window
                .try_find(("status-folder", 0_usize))
                .is_some_and(|card| card.visible()),
            "the folder cards still render"
        );
    })
    .expect("update the dashboard window");
}

#[gpui_kit::test]
fn sample_data_covers_several_folders_and_states() {
    use sync_core::meter::UploadTraffic;

    let model = sample();

    assert!(
        model.folders.len() >= 3,
        "the dashboard shows at least three folder cards"
    );
    assert!(
        model
            .folders
            .iter()
            .any(|folder| folder.activity == ActivityState::NeedsAttention),
        "at least one folder needs attention"
    );

    let states: BTreeSet<ActivityState> =
        model.folders.iter().map(|folder| folder.activity).collect();
    assert!(
        states.len() >= 3,
        "the sample data exercises several states"
    );

    let mut summed = UploadTraffic::default();
    for folder in &model.folders {
        summed.add(folder.uploads);
    }
    assert_eq!(
        model.global.uploads, summed,
        "the fixture's global card is the sum of its folders"
    );
}

#[gpui_kit::test]
fn folder_progress_survives_counts_above_a_u16() {
    use sync_core::status::{Activity, FolderStatus, Status};
    use sync_core::store::FolderCounts;

    let counts = FolderCounts {
        uploaded: 100_000,
        pending: 100_000,
        failed: 0,
        review: 0,
    };
    let status = Status {
        activity: Activity::Syncing,
        folders: vec![FolderStatus {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: true,
            album_scope: sync_core::store::AlbumScope::Leaf,
            available: true,
            counts,
            last_error: None,
            uploads: sync_core::meter::UploadTraffic::default(),
        }],
        counts,
        last_error: None,
        uploads: sync_core::meter::UploadTraffic::default(),
    };

    let model = DashboardModel::from_engine(&status, None);

    assert!(
        (model.folders[0].progress - 0.5).abs() < 0.001,
        ">65 535 items must not saturate the percentage: {}",
        model.folders[0].progress
    );
    assert_eq!(model.folders[0].content().progress_percent, "50%");
}

#[gpui_kit::test]
fn the_album_line_reads_the_scope_whether_the_mapping_is_on_or_off() {
    use sync_core::store::AlbumScope;

    let model = sample();
    let mapped = model
        .folders
        .iter()
        .find(|folder| folder.album_mapping && folder.album_scope == AlbumScope::WholeFolder)
        .expect("the sample data has a mapped folder");
    assert_eq!(
        mapped.content().album.as_deref(),
        Some("Albums: One album for this folder")
    );

    let off = model
        .folders
        .iter()
        .find(|folder| !folder.album_mapping)
        .expect("the sample data has an unmapped folder");
    assert_eq!(
        off.content().album.as_deref(),
        Some("Albums: off"),
        "R3.4: the scope radios change what the folder will map to, so the card says which \
         scope is stored even while the mapping is off"
    );

    assert_eq!(
        model.global.content().album,
        None,
        "the all-folders card is not one folder's album"
    );
}

#[gpui_kit::test]
fn system_theme_maps_to_the_kit_theme_mode() {
    assert_eq!(theme_mode(SystemTheme::Light), ThemeMode::Light);
    assert_eq!(theme_mode(SystemTheme::Dark), ThemeMode::Dark);
}

#[gpui_kit::test]
fn window_appearance_maps_to_the_system_theme() {
    assert_eq!(
        SystemTheme::from(WindowAppearance::Light),
        SystemTheme::Light
    );
    assert_eq!(
        SystemTheme::from(WindowAppearance::VibrantLight),
        SystemTheme::Light
    );
    assert_eq!(SystemTheme::from(WindowAppearance::Dark), SystemTheme::Dark);
    assert_eq!(
        SystemTheme::from(WindowAppearance::VibrantDark),
        SystemTheme::Dark
    );
}

#[gpui_kit::test]
fn applying_a_system_theme_updates_the_kit_theme(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);

    cx.update(|cx| apply_system_theme(SystemTheme::Dark, cx));
    assert!(cx.read(|cx| Theme::global(cx).is_dark()));

    cx.update(|cx| apply_system_theme(SystemTheme::Light, cx));
    assert!(!cx.read(|cx| Theme::global(cx).is_dark()));
}

/// WCAG 2.1 relative luminance.
fn luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
}

/// The WCAG 2.1 contrast ratio, 1.0 (identical) to 21.0.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[gpui_kit::test]
fn every_card_color_meets_wcag_aa_in_both_themes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for system in [SystemTheme::Dark, SystemTheme::Light] {
        cx.update(|cx| apply_system_theme(system, cx));
        cx.read(|cx| {
            let theme = Theme::global(cx);
            let palette = CardPalette::new(theme);

            for (what, foreground, background) in [
                ("card text", palette.text, palette.surface),
                ("card meta", palette.meta, palette.surface),
                ("pill text", palette.pill_text, palette.pill_fill),
                ("window label", theme.muted_foreground, theme.background),
            ] {
                let ratio = contrast(foreground, background);
                assert!(
                    ratio >= 4.5,
                    "{system:?}: {what} is {ratio:.2}:1, below the 4.5:1 minimum"
                );
            }

            assert_ne!(
                palette.pill_fill, palette.surface,
                "{system:?}: the pill fill must differ from the card surface"
            );
            for state in [
                ActivityState::Idle,
                ActivityState::Syncing,
                ActivityState::Paused,
                ActivityState::NeedsAttention,
            ] {
                let border = state_border(state, theme);
                let ratio = contrast(border, palette.surface);
                assert!(
                    ratio >= 3.0,
                    "{system:?}: the {state:?} pill border is {ratio:.2}:1 on the card surface, below the WCAG non-text minimum of 3:1"
                );
            }
        });
    }
}

#[gpui_kit::test]
fn folder_cards_in_one_row_share_a_bottom(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx, sample());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let cards: Vec<_> = (0..sample().folders.len())
            .map(|index| {
                let card = window.find(("status-folder", index));
                (index, card.bounds())
            })
            .collect();

        for (index, bounds) in &cards {
            for (other_index, other) in &cards {
                if bounds.top() == other.top() {
                    assert!(
                        (bounds.bottom() - other.bottom()).abs() <= px(0.5),
                        "folder cards {index} and {other_index} share a row but not a bottom"
                    );
                }
            }
        }
    })
    .expect("update the dashboard window");
}

#[gpui_kit::test]
fn long_names_truncate_and_keep_the_row_height(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let mut model = sample();
    let neighbour = model.folders[0].clone();
    model.folders = vec![
        StatusCard {
            title: "A folder name far longer than any card could ever show".to_string(),
            path: Some("Pictures\\A\\very\\deep\\path\\that\\keeps\\going\\and\\going".to_string()),
            ..neighbour.clone()
        },
        neighbour.clone(),
        neighbour.clone(),
        neighbour,
    ];

    let window = open_dashboard(cx, model);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let long = window.find(("status-folder", 0_usize)).bounds();
        let short = window.find(("status-folder", 1_usize)).bounds();
        let last_row = window.find(("status-folder", 3_usize)).bounds();

        assert_eq!(
            long.size.width, short.size.width,
            "both folder cards keep the fixed card width"
        );
        assert!(
            (long.size.height - last_row.size.height).abs() <= px(0.5),
            "a long name or path must not grow its row: {long:?} vs {last_row:?}"
        );
    })
    .expect("update the dashboard window");
}

#[gpui_kit::test]
fn dashboard_shows_named_controls_and_one_card_per_folder(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx, sample());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        assert_eq!(window.find("dashboard-sync-now").label(), Some("Sync now"));
        assert_eq!(
            window.find("dashboard-pause").label(),
            Some("Pause syncing")
        );

        let global = window.find("status-global");
        assert!(global.visible(), "the global card is visible");

        let model = sample();
        for (index, folder) in model.folders.iter().enumerate() {
            let card = window.find(("status-folder", index));
            assert!(card.visible(), "folder card {index} is visible");
            assert!(
                card.bounds().size.width > px(0.) && card.bounds().size.height > px(0.),
                "folder card {index} has a layout size for {}",
                folder.title
            );
            assert!(
                card.bounds().top() >= global.bounds().bottom(),
                "folder card {index} sits below the global card"
            );
        }
    })
    .expect("update the dashboard window");
}

#[gpui_kit::test]
fn pause_control_renames_itself_and_flips_the_global_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx, sample());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status-state-global").label(),
            Some("Syncing"),
            "the global card's pill shows the view-model state"
        );

        window.click("dashboard-pause", cx);

        assert_eq!(
            window.find("dashboard-pause").label(),
            Some("Resume syncing"),
            "the pause control renames itself while paused"
        );
        assert_eq!(
            window.find("status-state-global").label(),
            Some("Paused"),
            "the global card's pill shows the paused state"
        );
    })
    .expect("update the dashboard window");
}

/// The live dashboard over a real (initially disconnected) engine model.
///
/// The view handle comes back too, so a test can drive one of its seams
/// directly when the simulated dialog cannot produce the case (#30).
fn open_live_dashboard(
    cx: &mut TestAppContext,
    temp: &std::path::Path,
) -> (
    AnyWindowHandle,
    Entity<immich_sync::store::StoreModel>,
    Entity<Dashboard>,
) {
    let paths = immich_sync::paths::AppPaths {
        config_file: temp.join("config").join("settings.toml"),
        state_db: temp.join("data").join("state.db"),
        autostart_dir: temp.join("config").join("autostart"),
    };
    cx.update(|cx| {
        gpui_kit::init(cx);
        let credentials: std::sync::Arc<dyn sync_core::credential::CredentialStore> =
            std::sync::Arc::new(sync_core::credential::MemoryCredentialStore::new());
        let connection = cx.new(|_| {
            immich_sync::connection::ConnectionModel::new(
                sync_core::connection::ServerProfile::default(),
                credentials,
            )
        });
        let store = cx.new(|cx| immich_sync::store::StoreModel::open(&paths, cx));
        let engine = cx
            .new(|cx| immich_sync::engine::EngineModel::new(connection, store.clone(), &paths, cx));
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1200.), px(800.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let engine = engine.clone();
                cx.new(|cx| Dashboard::with_engine(engine, window, cx))
            },
        )
        .expect("open the live dashboard window");
        (window, store, view)
    })
}

/// Waits until the shared state store has opened; it opens off the UI thread
/// (N2), so a control driven before that is refused.
async fn wait_for_store(cx: &mut TestAppContext, store: &Entity<immich_sync::store::StoreModel>) {
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            return;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
    panic!("the state store never opened");
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: impl Into<ElementId> + Clone, cx: &mut App) {
    let id = id.into();
    for _ in 0..16 {
        if window.find(id.clone()).focused() == Some(true) {
            return;
        }
        window.focus_next(cx);
        window.render_frame(cx);
    }
    assert_eq!(
        window.find(id.clone()).focused(),
        Some(true),
        "{id:?} is not keyboard reachable"
    );
}

#[gpui_kit::test]
fn engine_status_maps_to_the_global_card_and_one_card_per_folder() {
    use sync_core::status::{Activity, FolderStatus, Status};
    use sync_core::store::FolderCounts;

    let status = Status {
        activity: Activity::Syncing,
        folders: vec![
            FolderStatus {
                path: "/home/you/Pictures".to_string(),
                recursive: true,
                album_mapping: true,
                album_scope: sync_core::store::AlbumScope::Leaf,
                available: true,
                counts: FolderCounts {
                    uploaded: 7,
                    pending: 3,
                    failed: 1,
                    review: 2,
                },
                last_error: None,
                uploads: sync_core::meter::UploadTraffic::default(),
            },
            FolderStatus {
                path: "/mnt/unplugged".to_string(),
                recursive: false,
                album_mapping: true,
                album_scope: sync_core::store::AlbumScope::Leaf,
                available: false,
                counts: FolderCounts::default(),
                last_error: None,
                uploads: sync_core::meter::UploadTraffic::default(),
            },
        ],
        counts: FolderCounts {
            uploaded: 7,
            pending: 3,
            failed: 1,
            review: 2,
        },
        last_error: Some("unsupported_format: the file type".to_string()),
        uploads: sync_core::meter::UploadTraffic {
            bytes: 41_943_040,
            bytes_per_second: 2_516_582,
            in_flight: 2,
        },
    };

    let model = DashboardModel::from_engine(&status, Some("Connected".to_string()));

    assert_eq!(model.global.title, "All folders");
    assert_eq!(model.global.activity, ActivityState::Syncing);
    assert_eq!(model.global.counts.uploaded, 7);
    assert_eq!(model.global.connection.as_deref(), Some("Connected"));
    assert_eq!(
        model.global.last_error.as_deref(),
        Some("unsupported_format: the file type")
    );
    assert!((model.global.progress - 0.7).abs() < 0.01);
    assert_eq!(
        model.global.review_count, 2,
        "R5.4: the global card shows the review count"
    );
    assert_eq!(
        model.global.content().review.as_deref(),
        Some("2 missing files to review")
    );
    assert_eq!(
        model.global.content().uploads.as_deref(),
        Some("2.4 MiB/s · 2 uploading"),
        "R8.1: the engine's reading reaches the global card's line"
    );
    assert_eq!(model.folders.len(), 2, "one card per source folder");

    let pictures = &model.folders[0];
    assert_eq!(pictures.title, "Pictures");
    assert_eq!(pictures.path.as_deref(), Some("/home/you/Pictures"));
    assert_eq!(pictures.activity, ActivityState::Syncing);
    assert_eq!(
        pictures.content().counts,
        "7 uploaded · 3 pending · 1 failed"
    );
    assert_eq!(
        pictures.content().uploads,
        None,
        "the upload line is the global card's"
    );
    assert_eq!(
        pictures.review_count, 2,
        "R5.4: a folder card shows its own review count"
    );

    // R2.5: a folder that disappeared is a folder status, not per-file errors.
    let missing = &model.folders[1];
    assert_eq!(missing.activity, ActivityState::NeedsAttention);
    assert!(!missing.available);
    assert_eq!(missing.content().counts, "Folder unavailable");
}

#[gpui_kit::test]
async fn folder_add_and_remove_controls_reach_the_store(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("dashboard-add-folder").label(),
            Some("Add folder"),
            "the add control is named for assistive technology"
        );
        assert_eq!(
            window.find("dashboard-folder-path").label(),
            Some("Source folder path"),
            "the path field is named for assistive technology"
        );
        tab_to(window, "dashboard-folder-path", cx);
        window.input(photos.to_str().expect("a UTF-8 path"), cx);
        window.click("dashboard-add-folder", cx);
        window.render_frame(cx);
        assert!(
            window.find("folder-panel").visible(),
            "#40: Add folder opens the setup dialog instead of adding"
        );
        window.click("folder-panel-add", cx);
    })
    .expect("add the folder through the dashboard");

    // The add runs on the worker runtime now (N2), so wait for the store.
    let mut added = Vec::new();
    for _ in 0..600 {
        added = cx.read(|cx| store.read(cx).folders().to_vec());
        if !added.is_empty() {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
    assert_eq!(
        added,
        vec![sync_core::store::SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
        }],
        "R2.1: the add control persisted the folder"
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let card = window.find(("status-folder", 0_usize));
        assert!(card.visible(), "the new folder has a card");
        assert_eq!(
            window.find(("folder-remove", 0_usize)).label(),
            Some("Remove photos"),
            "the remove control is named for assistive technology"
        );
        window.click(("folder-remove", 0_usize), cx);
    })
    .expect("remove the folder through the dashboard");

    let mut removed = false;
    for _ in 0..600 {
        removed = cx.read(|cx| store.read(cx).folders().is_empty());
        if removed {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
    assert!(removed, "R2.1: the remove control deleted the folder");
}

/// Adds one source folder through the dashboard controls and waits until the
/// store holds it, so a test can act on the card the user sees.
async fn add_folder_through_ui(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    store: &Entity<immich_sync::store::StoreModel>,
    path: &std::path::Path,
) {
    wait_for_store(cx, store).await;
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "dashboard-folder-path", cx);
        window.input(path.to_str().expect("a UTF-8 path"), cx);
        window.click("dashboard-add-folder", cx);
        window.render_frame(cx);
        // #40: the field opens the setup dialog; Add confirms its defaults.
        window.click("folder-panel-add", cx);
    })
    .expect("add the folder through the dashboard");
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).folders().len() == 1) {
            return;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
    panic!("the folder did not reach the store");
}

/// Polls the one folder the store holds until `check` accepts it.
async fn wait_for_folder(
    cx: &mut TestAppContext,
    store: &Entity<immich_sync::store::StoreModel>,
    check: impl Fn(&sync_core::store::SourceFolder) -> bool,
) -> sync_core::store::SourceFolder {
    for _ in 0..600 {
        let folders = cx.read(|cx| store.read(cx).folders().to_vec());
        if let Some(folder) = folders.first()
            && check(folder)
        {
            return folder.clone();
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }
    panic!("the folder never reached the wanted state");
}

#[gpui_kit::test]
async fn a_folder_card_names_the_album_mapping_and_its_scope(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    add_folder_through_ui(cx, window, &store, &photos).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // N5: every control carries an accessible name, and the album scope
        // says which folder it belongs to.
        assert_eq!(
            window.find(("folder-album", 0_usize)).label(),
            Some("Album mapping for photos")
        );
        assert_eq!(
            window.find(("folder-album-leaf", 0_usize)).label(),
            Some("Album scope for photos: one album per folder")
        );
        assert_eq!(
            window.find(("folder-album-whole", 0_usize)).label(),
            Some("Album scope for photos: one album for this folder")
        );
        // R3.4: the mapping is opt-in, so it starts off and the scope it would
        // use is not yet in force.
        assert_eq!(
            window.find(("folder-album", 0_usize)).checked(),
            Some(false),
            "album mapping starts off"
        );
        assert_eq!(
            window.find(("folder-album-leaf", 0_usize)).selected(),
            Some(true),
            "the leaf scope is the default"
        );
        // The scope stays choosable before the mapping is on: it is the
        // folder's stored scope and applies once the mapping is on (R3.4).
        assert_eq!(
            window.find(("folder-album-whole", 0_usize)).checked(),
            Some(false),
            "the other scope is not chosen"
        );
    })
    .expect("read the album controls");
}

#[gpui_kit::test]
async fn the_album_controls_reach_the_store_by_pointer_and_by_keyboard(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    add_folder_through_ui(cx, window, &store, &photos).await;

    // The switch is a tab stop that activates with Space (N5).
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, ("folder-album", 0_usize), cx);
        window.press("space", cx);
    })
    .expect("turn album mapping on with the keyboard");
    let mapped = wait_for_folder(cx, &store, |folder| folder.album_mapping).await;
    assert_eq!(
        mapped.album_scope,
        sync_core::store::AlbumScope::Leaf,
        "R3.4: the scope is per folder and starts at one album per folder"
    );

    // With mapping on, the scope radios take focus and answer the keyboard.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("folder-album", 0_usize)).checked(),
            Some(true),
            "the switch shows the mapping the store holds"
        );
        tab_to(window, ("folder-album-whole", 0_usize), cx);
        window.press("enter", cx);
    })
    .expect("choose the whole-folder scope with the keyboard");
    let whole = wait_for_folder(cx, &store, |folder| {
        folder.album_scope == sync_core::store::AlbumScope::WholeFolder
    })
    .await;
    assert!(whole.album_mapping, "the mapping is still on");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("folder-album-whole", 0_usize)).checked(),
            Some(true),
            "the radio shows the scope the store holds"
        );
        window.click(("folder-album-leaf", 0_usize), cx);
    })
    .expect("choose the leaf scope with the pointer");
    let leaf = wait_for_folder(cx, &store, |folder| {
        folder.album_scope == sync_core::store::AlbumScope::Leaf
    })
    .await;
    assert!(leaf.album_mapping);
}

#[gpui_kit::test]
async fn the_recursive_switch_reaches_the_store_by_pointer_and_by_keyboard(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    add_folder_through_ui(cx, window, &store, &photos).await;

    // R2.1: the field adds a folder recursively, and the card's switch shows
    // and changes the flag the store holds.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("folder-recursive", 0_usize)).label(),
            Some("Sync photos recursively")
        );
        assert_eq!(
            window.find(("folder-recursive", 0_usize)).checked(),
            Some(true),
            "a folder added through the field starts recursive"
        );
    })
    .expect("read the recursion switch");

    // Turn album mapping on first: the recursion toggle must not disturb it.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("folder-album", 0_usize), cx);
    })
    .expect("turn album mapping on");
    wait_for_folder(cx, &store, |folder| folder.album_mapping).await;

    // The switch is a tab stop that activates with Space (N5).
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, ("folder-recursive", 0_usize), cx);
        window.press("space", cx);
    })
    .expect("turn recursion off with the keyboard");
    let flat = wait_for_folder(cx, &store, |folder| !folder.recursive).await;
    assert!(
        flat.album_mapping,
        "R2.1: the recursion toggle keeps the album mapping"
    );

    // And back on by pointer, still preserving the other per-folder settings.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("folder-recursive", 0_usize), cx);
    })
    .expect("turn recursion on with the pointer");
    let recursive = wait_for_folder(cx, &store, |folder| folder.recursive).await;
    assert!(
        recursive.album_mapping,
        "R2.1: the switch back keeps the album mapping"
    );
}

/// The text input the kit nests inside the row `id`.
fn row_text_input(
    window: &Window,
    id: &'static str,
) -> gpui_kit::base::test_support::ElementSnapshot {
    gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .find(|snapshot| {
            snapshot.role() == Some(Role::TextInput)
                && snapshot.path().contains(&ElementId::Name(id.into()))
        })
        .expect("a text input inside the row")
}

#[gpui_kit::test]
async fn browsing_for_a_folder_fills_the_field_and_add_folder_adds_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("dashboard-browse-folder").label(),
            Some("Browse for a source folder"),
            "the browse control is named for assistive technology"
        );
        // The control is a tab stop, like the field and the add button (N5).
        tab_to(window, "dashboard-browse-folder", cx);
        window.click("dashboard-browse-folder", cx);
    })
    .expect("open the folder picker");

    assert!(
        cx.did_prompt_for_paths(),
        "R2.1: Browse… opens the OS folder picker"
    );
    cx.simulate_path_prompt_response(|options| {
        assert!(
            options.directories,
            "R2.1: the source folder picker chooses directories"
        );
        assert!(!options.files, "R2.1: it does not choose files");
        assert!(!options.multiple, "#30: one source folder at a time");
        Some(vec![photos.clone()])
    });
    cx.run_until_parked();

    let path_text = photos.display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(path_text.as_str()),
            "the chosen folder fills the existing field"
        );
        assert!(
            window.try_find("dashboard-browse-notice").is_none(),
            "a chosen folder is not a notice"
        );
        assert!(
            window.find("folder-panel").visible(),
            "#40: Browse opens the setup dialog instead of adding"
        );
        window.click("folder-panel-add", cx);
    })
    .expect("add the chosen folder");

    let added = wait_for_folder(cx, &store, |_| true).await;
    assert_eq!(
        added.path, path_text,
        "R2.1: Add folder adds the browsed folder"
    );
}

#[gpui_kit::test]
async fn cancelling_the_folder_picker_changes_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let typed = temp
        .path()
        .join("typed but not chosen")
        .display()
        .to_string();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "dashboard-folder-path", cx);
        window.input(&typed, cx);
        window.click("dashboard-browse-folder", cx);
    })
    .expect("open the picker with the field already typed in");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(typed.as_str()),
            "#30: a cancel leaves the typed path alone"
        );
        assert!(
            window.try_find("dashboard-browse-notice").is_none(),
            "a cancel is not an error"
        );
        assert!(
            window.try_find("folder-panel").is_none(),
            "#40: a cancelled pick opens no setup dialog"
        );
    })
    .expect("read the field after the cancel");
    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#30: a cancel adds nothing"
    );
}

#[gpui_kit::test]
async fn a_picker_that_cannot_open_keeps_the_typed_path_usable(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let path_text = photos.display().to_string();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "dashboard-folder-path", cx);
        window.input(&path_text, cx);
        // The harness answers a prompt with a choice or a cancel and nothing
        // else, so the portal-less failure comes in through the seam the click
        // handler uses (#30).
        view.update(cx, |view, cx| {
            view.apply_pick(
                Err(anyhow::anyhow!(
                    "Couldn't open file picker due to missing xdg-desktop-portal implementation."
                )),
                window,
                cx,
            );
        });
    })
    .expect("apply a picker that could not open");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window.find("dashboard-browse-notice");
        assert_eq!(
            notice.role(),
            Some(Role::Alert),
            "the failure notice is announced"
        );
        let label = notice
            .label()
            .expect("the notice has a message")
            .to_string();
        assert!(
            label.contains("xdg-desktop-portal"),
            "the notice keeps the reason the dialog gave: {label}"
        );
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(path_text.as_str()),
            "the typed path survives a failure"
        );
        assert!(
            window.try_find("folder-panel").is_none(),
            "a picker that could not open starts no setup"
        );
        // And it is still usable: Add folder opens the setup with it.
        window.click("dashboard-add-folder", cx);
        window.render_frame(cx);
        window.click("folder-panel-add", cx);
    })
    .expect("use the typed path after the failure");

    let added = wait_for_folder(cx, &store, |_| true).await;
    assert_eq!(
        added.path, path_text,
        "#30: the typed path still reaches Add folder after a failed picker"
    );
}

/// Opens the setup dialog for the field's path with Enter (#40).
fn open_setup_through_the_field(window: &mut Window, path: &str, cx: &mut App) {
    window.render_frame(cx);
    tab_to(window, "dashboard-folder-path", cx);
    window.input(path, cx);
    window.press("enter", cx);
    window.render_frame(cx);
    assert!(
        window.find("folder-panel").visible(),
        "#40: Enter opens the folder setup dialog"
    );
}

/// Presses Add in the open setup dialog and renders the result (#40).
fn add_in_setup(window: &mut Window, cx: &mut App) {
    window.render_frame(cx);
    window.click("folder-panel-add", cx);
    window.render_frame(cx);
}

/// Waits for the setup dialog's Add request to come back with a reason.
async fn wait_for_setup_problem(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.wait_for(window, std::time::Duration::from_secs(30), |window, _| {
        window.try_find("folder-panel-problem").is_some()
    })
    .await;
}

/// The reason the open setup dialog shows.
fn setup_problem(window: &Window) -> String {
    window
        .find("folder-panel-problem")
        .label()
        .expect("the setup dialog shows a reason")
        .to_string()
}

#[gpui_kit::test]
async fn enter_opens_the_setup_dialog_without_writing_anything(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let path = photos.display().to_string();

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &path, cx);
        assert_eq!(
            window.find("folder-panel").role(),
            Some(Role::Dialog),
            "N5: the setup is one named dialog"
        );
        assert_eq!(
            window.find("folder-panel").label(),
            Some("Folder setup"),
            "N5: the dialog has an accessible name"
        );
        // R2.1: the dialog shows the path it will record.
        assert_eq!(
            window.find("folder-panel-path").label(),
            Some(format!("Folder path: {path}").as_str())
        );
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(path.as_str()),
            "opening the dialog leaves the typed path alone"
        );
        // R2.1, R3.4: the three settings start at the documented defaults.
        assert_eq!(
            window.find("folder-panel-recursive").checked(),
            Some(true),
            "Recursive defaults on"
        );
        assert_eq!(
            window.find("folder-panel-album").checked(),
            Some(false),
            "Album mapping defaults off"
        );
        assert_eq!(
            window.find("folder-panel-album-leaf").selected(),
            Some(true),
            "Album scope defaults to one album per folder"
        );
    })
    .expect("open the setup dialog");

    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#40: opening the dialog starts no scan and records nothing"
    );
}

#[gpui_kit::test]
async fn add_writes_the_path_and_all_three_settings_in_one_request(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let path = photos.display().to_string();

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &path, cx);
        // R2.1, R3.4: change every setting from its default before Add.
        window.click("folder-panel-recursive", cx);
        window.click("folder-panel-album", cx);
        window.render_frame(cx);
        window.click("folder-panel-album-whole", cx);
        window.render_frame(cx);
        assert_eq!(window.find("folder-panel-recursive").checked(), Some(false));
        assert_eq!(window.find("folder-panel-album").checked(), Some(true));
        assert_eq!(
            window.find("folder-panel-album-whole").selected(),
            Some(true)
        );
        add_in_setup(window, cx);
    })
    .expect("add the folder with the confirmed settings");

    let stored = wait_for_folder(cx, &store, |_| true).await;
    assert_eq!(
        stored,
        sync_core::store::SourceFolder {
            path: path.clone(),
            recursive: false,
            album_mapping: true,
            album_scope: sync_core::store::AlbumScope::WholeFolder,
        },
        "#40: one write records the path and all three settings"
    );

    // R8.1: the card appears with the values the user chose, and the dialog
    // that recorded it has closed.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("folder-panel").is_none(),
            "#40: a recorded folder closes the dialog"
        );
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(""),
            "#40: a recorded folder clears the field"
        );
        assert!(
            window.find(("status-folder", 0_usize)).visible(),
            "the new folder has a card"
        );
        assert_eq!(
            window.find(("folder-recursive", 0_usize)).checked(),
            Some(false)
        );
        assert_eq!(window.find(("folder-album", 0_usize)).checked(), Some(true));
        assert_eq!(
            window.find(("folder-album-whole", 0_usize)).selected(),
            Some(true)
        );
    })
    .expect("read the new folder's card");
}

#[gpui_kit::test]
async fn cancel_and_escape_close_the_setup_and_write_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let path = photos.display().to_string();

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &path, cx);
        // N5: Cancel answers the keyboard.
        tab_to(window, "folder-panel-cancel", cx);
        window.press("enter", cx);
    })
    .expect("press Cancel");

    // The panel reports its event, and the dashboard closes it, in the next
    // update (the same cadence a frame has).
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("folder-panel").is_none(),
            "#40: Cancel closes the dialog"
        );
        assert_eq!(
            window.find("dashboard-folder-path").focused(),
            Some(true),
            "#40: Cancel returns focus to the add field"
        );
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(path.as_str()),
            "#30: Cancel leaves the typed path usable"
        );

        // Escape does the same.
        window.click("dashboard-add-folder", cx);
        window.render_frame(cx);
        assert!(window.find("folder-panel").visible());
        window.press("escape", cx);
    })
    .expect("press Escape");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("folder-panel").is_none(),
            "#40: Escape cancels the dialog"
        );
        assert_eq!(
            window.find("dashboard-folder-path").focused(),
            Some(true),
            "#40: Escape returns focus to the add field"
        );
    })
    .expect("read the dialog after Escape");

    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#40: Cancel writes nothing"
    );
}

/// #40: Add is one in-flight write. While it runs, Cancel must not abandon
/// the dialog: the write still lands, so the dialog waits for its own result.
#[gpui_kit::test]
async fn cancel_while_an_add_is_pending_leaves_the_dialog_and_the_write(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;
    let path = photos.display().to_string();

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &path, cx);
        // Focus Cancel, then press Add with the mouse: Cancel is disabled once
        // the write runs, and the modal must keep the keyboard (Escape and the
        // focus trap) instead of dropping focus to the window root.
        tab_to(window, "folder-panel-cancel", cx);
        add_in_setup(window, cx);
        assert_eq!(
            window.find("folder-panel-trap").focused(),
            Some(true),
            "#40: a pending Add leaves focus on the modal host, which owns Escape"
        );
        window.click("folder-panel-cancel", cx);
        window.press("escape", cx);
    })
    .expect("press Add then Cancel");

    // The dashboard closes a cancelled panel in the next update, so the check
    // that it stayed open belongs in an update of its own: an inert Cancel is
    // the only thing that keeps it there.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("folder-panel").is_some(),
            "#40: Cancel during a pending Add keeps the dialog open"
        );
    })
    .expect("read the dialog after Cancel");

    let added = wait_for_folder(cx, &store, |_| true).await;
    assert_eq!(added.path, path, "#40: the pending write still lands");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("folder-panel").is_none(),
            "#40: the result closes the dialog the Cancel could not"
        );
        assert_eq!(
            row_text_input(window, "dashboard-folder-path").value(),
            Some(""),
            "#40: the recorded folder clears the field"
        );
        assert!(
            window.find(("status-folder", 0_usize)).visible(),
            "#40: the recorded folder has its card"
        );
    })
    .expect("read the dialog after the result");
}

#[gpui_kit::test]
async fn an_empty_path_keeps_the_setup_open_with_its_reason(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("dashboard-add-folder", cx);
        window.render_frame(cx);
        assert!(
            window.find("folder-panel").visible(),
            "#40: the dialog opens even with an empty path"
        );
        add_in_setup(window, cx);
    })
    .expect("open the dialog with an empty path");
    wait_for_setup_problem(cx, window).await;
    cx.update_window(window, |_, window, _| {
        assert_eq!(setup_problem(window), "Enter the path of a folder to add.");
        assert!(
            window.find("folder-panel").visible(),
            "#40: a refused Add keeps the dialog open"
        );
    })
    .expect("read the empty-path reason");

    assert!(cx.read(|cx| store.read(cx).folders().is_empty()));
}

#[gpui_kit::test]
async fn a_missing_path_keeps_the_setup_open_with_its_reason_and_writes_nothing(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let missing = temp.path().join("not there");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &missing.display().to_string(), cx);
        add_in_setup(window, cx);
    })
    .expect("open the dialog for a missing path");
    wait_for_setup_problem(cx, window).await;
    cx.update_window(window, |_, window, _| {
        assert_eq!(setup_problem(window), "That path does not exist.");
        assert!(
            window.find("folder-panel").visible(),
            "#40: a missing path keeps the dialog open"
        );
    })
    .expect("read the missing-path reason");

    assert!(cx.read(|cx| store.read(cx).folders().is_empty()));
}

#[gpui_kit::test]
async fn a_file_path_keeps_the_setup_open_with_its_reason_and_writes_nothing(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let file = temp.path().join("photo.jpg");
    std::fs::write(&file, b"not a folder").expect("create the fixture file");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &file.display().to_string(), cx);
        add_in_setup(window, cx);
    })
    .expect("open the dialog for a file path");
    wait_for_setup_problem(cx, window).await;
    cx.update_window(window, |_, window, _| {
        assert_eq!(setup_problem(window), "That path is a file, not a folder.");
        assert!(
            window.find("folder-panel").visible(),
            "#40: a file path keeps the dialog open"
        );
    })
    .expect("read the file-path reason");

    assert!(cx.read(|cx| store.read(cx).folders().is_empty()));
}

/// #40: the dialog must not upsert a folder the client already tracks.
#[gpui_kit::test]
async fn adding_a_tracked_folder_says_already_added_and_changes_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let seeded = sync_core::store::SourceFolder {
        path: photos.display().to_string(),
        recursive: false,
        album_mapping: true,
        album_scope: sync_core::store::AlbumScope::WholeFolder,
    };
    let paths = immich_sync::paths::AppPaths {
        config_file: temp.path().join("config").join("settings.toml"),
        state_db: temp.path().join("data").join("state.db"),
        autostart_dir: temp.path().join("config").join("autostart"),
    };
    {
        let store = sync_core::store::StateStore::open(&paths.state_db).expect("seed the store");
        store.add_folder(&seeded).expect("seed the folder");
    }
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &seeded.path, cx);
        add_in_setup(window, cx);
    })
    .expect("open the dialog for an already-added folder");
    wait_for_setup_problem(cx, window).await;
    cx.update_window(window, |_, window, _| {
        assert_eq!(setup_problem(window), "Already added.");
        assert!(
            window.find("folder-panel").visible(),
            "#40: an already-added folder keeps the dialog open"
        );
    })
    .expect("read the already-added reason");

    assert_eq!(
        cx.read(|cx| store.read(cx).folders().to_vec()),
        vec![seeded],
        "#40: the tracked folder keeps the settings it already had"
    );
}

#[gpui_kit::test]
async fn the_setup_dialog_names_its_controls_and_traps_focus(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    let (window, store, _view) = open_live_dashboard(cx, temp.path());
    wait_for_store(cx, &store).await;

    cx.update_window(window, |_, window, cx| {
        open_setup_through_the_field(window, &photos.display().to_string(), cx);
        // N5: every control has an accessible name.
        assert_eq!(
            window.find("folder-panel-recursive").label(),
            Some("Sync this folder recursively")
        );
        assert_eq!(
            window.find("folder-panel-album").label(),
            Some("Album mapping for this folder")
        );
        assert_eq!(
            window.find("folder-panel-album-leaf").label(),
            Some("Album scope for this folder: one album per folder")
        );
        assert_eq!(
            window.find("folder-panel-album-whole").label(),
            Some("Album scope for this folder: one album for this folder")
        );
        assert_eq!(
            window.find("folder-panel-cancel").label(),
            Some("Cancel adding the folder")
        );
        assert_eq!(
            window.find("folder-panel-add").label(),
            Some("Add the folder")
        );

        // N5: Tab stays inside the dialog and never reaches the dashboard
        // controls behind the backdrop.
        for _ in 0..12 {
            window.press("tab", cx);
            for behind in [
                "dashboard-folder-path",
                "dashboard-add-folder",
                "dashboard-browse-folder",
            ] {
                assert_ne!(
                    window.find(behind).focused(),
                    Some(true),
                    "focus escaped the dialog to {behind}"
                );
            }
        }

        // N5: the settings are keyboard reachable and answer the keyboard.
        tab_to(window, "folder-panel-recursive", cx);
        window.press("space", cx);
        assert_eq!(
            window.find("folder-panel-recursive").checked(),
            Some(false),
            "Space changes the focused switch"
        );
        tab_to(window, "folder-panel-add", cx);
        // N5: Add answers the keyboard too.
        window.press("space", cx);
    })
    .expect("read the setup dialog's names and focus");

    let added = wait_for_folder(cx, &store, |_| true).await;
    assert_eq!(
        added.path,
        photos.display().to_string(),
        "N5: Space on the focused Add records the folder"
    );
}
