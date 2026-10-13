//! Headless tests for the dashboard seams.
//!
//! The view-model seams (card content, theme mapping) are pure functions, but
//! they run here under `#[gpui_kit::test]` with the rest so one command covers
//! the whole dashboard. The rendered-view test opens a real headless window and
//! reads native accessibility properties and layout bounds.

use std::collections::BTreeSet;

use gpui_kit::component::Theme;
use gpui_kit::component::ThemeMode;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Hsla, Point, TestAppContext, Window,
    WindowAppearance, WindowBounds, WindowOptions, px, size,
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
fn sample_data_covers_several_folders_and_states() {
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
fn open_live_dashboard(
    cx: &mut TestAppContext,
    temp: &std::path::Path,
) -> (AnyWindowHandle, Entity<immich_sync::store::StoreModel>) {
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
        let window = gpui_kit::open_window(
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
        .expect("open the live dashboard window")
        .0;
        (window, store)
    })
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..16 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        window.focus_next(cx);
        window.render_frame(cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

#[gpui_kit::test]
fn engine_status_maps_to_the_global_card_and_one_card_per_folder() {
    use sync_core::engine::{Activity, EngineStatus, FolderStatus};
    use sync_core::store::FolderCounts;

    let status = EngineStatus {
        activity: Activity::Syncing,
        folders: vec![
            FolderStatus {
                path: "/home/you/Pictures".to_string(),
                recursive: true,
                available: true,
                counts: FolderCounts {
                    uploaded: 7,
                    pending: 3,
                    failed: 1,
                },
                last_error: None,
            },
            FolderStatus {
                path: "/mnt/unplugged".to_string(),
                recursive: false,
                available: false,
                counts: FolderCounts::default(),
                last_error: None,
            },
        ],
        counts: FolderCounts {
            uploaded: 7,
            pending: 3,
            failed: 1,
        },
        last_error: Some("unsupported_format: the file type".to_string()),
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
    assert_eq!(model.folders.len(), 2, "one card per source folder");

    let pictures = &model.folders[0];
    assert_eq!(pictures.title, "Pictures");
    assert_eq!(pictures.path.as_deref(), Some("/home/you/Pictures"));
    assert_eq!(pictures.activity, ActivityState::Syncing);
    assert_eq!(
        pictures.content().counts,
        "7 uploaded · 3 pending · 1 failed"
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
    let (window, store) = open_live_dashboard(cx, temp.path());
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            break;
        }
        cx.executor()
            .timer(std::time::Duration::from_millis(10))
            .await;
        cx.run_until_parked();
    }

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
    })
    .expect("add the folder through the dashboard");

    let added = cx.read(|cx| store.read(cx).folders().to_vec());
    assert_eq!(
        added,
        vec![sync_core::store::SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
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

    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "R2.1: the remove control deleted the folder"
    );
}
