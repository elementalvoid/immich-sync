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
    AnyWindowHandle, AppContext as _, Bounds, Point, TestAppContext, WindowAppearance,
    WindowBounds, WindowOptions, px, size,
};
use immich_sync::dashboard::Dashboard;
use immich_sync::status::{ActivityState, sample};
use immich_sync::theme::{SystemTheme, apply_system_theme, theme_mode};

fn open_dashboard(cx: &mut TestAppContext) -> AnyWindowHandle {
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
            |_, cx| cx.new(|_| Dashboard::new(sample())),
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

#[gpui_kit::test]
fn dashboard_shows_named_controls_and_one_card_per_folder(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx);

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
    let window = open_dashboard(cx);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("dashboard-pause", cx);

        assert_eq!(
            window.find("dashboard-pause").label(),
            Some("Resume syncing"),
            "the pause control renames itself while paused"
        );
    })
    .expect("update the dashboard window");
}
