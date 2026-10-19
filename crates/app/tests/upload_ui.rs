//! Headless tests for the manual upload view (issue #5, R3.1, R3.2, R3.6, R4.5).
//!
//! The view is driven through its real controls: the path field, the button,
//! and the result notice the window renders.

mod common;

use common::mock_server;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, Role, TestAppContext,
    Window, WindowBounds, WindowOptions, px, size,
};
use immich_sync::connection::ConnectionModel;
use immich_sync::paths::AppPaths;
use immich_sync::upload::UploadReport;
use immich_sync::upload_view::UploadView;
use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, Secret, ServerProfile, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::failure::FailureClass;
use sync_core::store::StateStore;

/// The path layout the settings view uses, so the upload uses a temp database.
fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(800.)),
        })),
        ..Default::default()
    }
}

/// A connection model signed in to `server`, with an in-memory credential
/// store. The test waits for the sign-in to finish.
fn connected_model(cx: &mut TestAppContext, server: &MockImmich) -> Entity<ConnectionModel> {
    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        let request = SignIn {
            email: profile.email.clone(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        };
        let connection = cx.new(|_| ConnectionModel::new(profile.clone(), credentials));
        connection.update(cx, |model, cx| model.sign_in(request, profile, cx));
        connection
    })
}

/// Opens the upload view over a model for `paths`, with the shared store the
/// shell installs (N2).
fn open_upload(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    connection: Entity<ConnectionModel>,
) -> (
    AnyWindowHandle,
    Entity<immich_sync::store::StoreModel>,
    Entity<UploadView>,
) {
    let paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let store = cx.new(|cx| immich_sync::store::StoreModel::open(&paths, cx));
        let (window, view) = gpui_kit::open_window(window_options(), cx, |window, cx| {
            let model = cx.new(|cx| {
                immich_sync::upload::UploadModel::new(connection.clone(), store.clone(), cx)
            });
            cx.new(|cx| UploadView::new(model, window, cx))
        })
        .expect("open the upload window");
        (window, store, view)
    })
}

/// Pumps the executor until the shared store has opened.
async fn wait_for_store(cx: &mut TestAppContext, store: &Entity<immich_sync::store::StoreModel>) {
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the state store never opened");
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab(window: &mut Window, cx: &mut App) {
    window.focus_next(cx);
    window.render_frame(cx);
}

/// Tabs until `id` holds the keyboard focus.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..16 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        tab(window, cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
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

#[test]
fn the_report_text_names_the_asset_id_the_duplicate_and_the_class() {
    let created = UploadReport::Created {
        asset_id: "abc-123".to_string(),
    };
    assert_eq!(created.headline(), "Uploaded");
    assert!(
        created.message().contains("abc-123"),
        "{}",
        created.message()
    );

    let duplicate = UploadReport::Duplicate {
        asset_id: "abc-123".to_string(),
    };
    assert_eq!(duplicate.headline(), "Duplicate");
    assert!(duplicate.message().contains("duplicate"));
    assert!(
        duplicate.message().contains("Nothing was transferred"),
        "the duplicate is explicit: {}",
        duplicate.message()
    );

    let permanent = UploadReport::Failed {
        class: Some(FailureClass::UnsupportedFormat),
        permanent: true,
        message: "the server does not accept this file format".to_string(),
    };
    assert_eq!(permanent.headline(), "Permanent error");
    assert!(
        permanent.message().contains("unsupported_format"),
        "the permanent class is shown: {}",
        permanent.message()
    );

    let transient = UploadReport::Failed {
        class: Some(FailureClass::ServerTransient),
        permanent: false,
        message: "the server answered 503".to_string(),
    };
    assert_eq!(transient.headline(), "Temporary error");
    assert!(transient.message().contains("server_transient"));

    assert!(!UploadReport::Idle.is_busy());
    assert!(UploadReport::Running.is_busy());
}

#[gpui_kit::test]
fn the_upload_view_names_its_controls(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = cx.update(|cx| {
        let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), store))
    });
    let (window, _store, _view) = open_upload(cx, &paths, connection);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("upload-path").label(),
            Some("File path"),
            "the row carries the field name"
        );
        assert_eq!(
            row_text_input(window, "upload-path").label(),
            Some("File path"),
            "the text field is named for assistive technology"
        );
        assert_eq!(window.find("upload-button").label(), Some("Upload"));
        let result = window.find("upload-result");
        assert_eq!(result.role(), Some(Role::Alert), "the result is announced");
        assert_eq!(
            result.label(),
            Some(UploadReport::Idle.message().as_str()),
            "the notice states the current outcome"
        );
    })
    .expect("update the upload window");
}

#[gpui_kit::test]
async fn uploading_a_file_shows_the_asset_id_and_a_repeat_shows_the_duplicate(
    cx: &mut TestAppContext,
) {
    let (_runtime, server) = mock_server();
    // The sign-in and the upload results arrive from the HTTP worker thread.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connected_model(cx, &server);
    let (window, store, _view) = open_upload(cx, &paths, connection.clone());
    wait_for_store(cx, &store).await;
    cx.wait_for(window, Duration::from_secs(60), |_, cx| {
        connection.read(cx).state().is_connected()
    })
    .await;

    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello world").expect("write the fixture");
    let path_text = file.display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "upload-path", cx);
        window.input(&path_text, cx);
        assert_eq!(
            row_text_input(window, "upload-path").value(),
            Some(path_text.as_str()),
            "the path field holds the typed path"
        );
        window.click("upload-button", cx);
    })
    .expect("fill in the path and click Upload");
    cx.wait_for(window, Duration::from_secs(60), |window, _| {
        window.find("upload-result").label().is_some_and(|label| {
            !label.starts_with("Uploading") && label != "No file has been uploaded yet."
        })
    })
    .await;
    let first_label = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window
                .find("upload-result")
                .label()
                .expect("the result")
                .to_string()
        })
        .expect("read the result");
    assert!(
        first_label.starts_with("Uploaded"),
        "the upload finished: {first_label}"
    );

    // The row the pipeline wrote holds the asset id the notice shows.
    let store = StateStore::open(paths.state_db.clone()).expect("open the state database");
    let row = store
        .item(&path_text)
        .expect("read the row")
        .expect("the upload was recorded");
    let asset_id = row.asset_id.clone().expect("the row holds the asset id");
    assert!(
        first_label.contains(&asset_id),
        "the window shows the asset id {asset_id}: {first_label}"
    );
    assert_eq!(server.upload_requests(), 1);

    // The same file again: an explicit duplicate, and no second transfer.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("upload-button", cx);
    })
    .expect("click Upload again");
    cx.wait_for(window, Duration::from_secs(60), |window, _| {
        window
            .find("upload-result")
            .label()
            .is_some_and(|label| label.starts_with("Already on the server (duplicate)"))
    })
    .await;
    assert_eq!(
        server.upload_requests(),
        1,
        "R4.5: the unchanged SHA-1 transferred no body"
    );
}

#[gpui_kit::test]
async fn a_second_enter_during_an_upload_starts_nothing(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connected_model(cx, &server);
    let (window, store, _view) = open_upload(cx, &paths, connection.clone());
    wait_for_store(cx, &store).await;
    cx.wait_for(window, Duration::from_secs(60), |_, cx| {
        connection.read(cx).state().is_connected()
    })
    .await;

    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello world").expect("write the fixture");
    let path_text = file.display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "upload-path", cx);
        window.input(&path_text, cx);
        window.press("enter", cx);
        // A second Enter while the first pipeline runs must be a no-op, not a
        // second upload of the same file.
        window.press("enter", cx);
    })
    .expect("submit twice");
    cx.wait_for(window, Duration::from_secs(60), |window, _| {
        window.find("upload-result").label().is_some_and(|label| {
            !label.starts_with("Uploading") && label != "No file has been uploaded yet."
        })
    })
    .await;

    assert_eq!(
        server.upload_requests(),
        1,
        "a second Enter during an in-flight upload must not start a second pipeline"
    );
}

#[gpui_kit::test]
async fn an_upload_without_a_session_is_unavailable_not_a_crash(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = cx.update(|cx| {
        let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), store))
    });
    let (window, store, _view) = open_upload(cx, &paths, connection);
    wait_for_store(cx, &store).await;
    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello world").expect("write the fixture");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "upload-path", cx);
        window.input(file.to_str().expect("a UTF-8 temp path"), cx);
        window.click("upload-button", cx);
    })
    .expect("click Upload with no session");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("upload-result").label(),
            Some(
                UploadReport::CannotRun {
                    message: "Sign in to a server before uploading a file.".to_string()
                }
                .message()
                .as_str()
            ),
            "the window says what to do instead of failing silently"
        );
    })
    .expect("read the result");
}

#[gpui_kit::test]
async fn browsing_for_a_file_fills_the_field_and_upload_sends_it(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connected_model(cx, &server);
    let (window, store, _view) = open_upload(cx, &paths, connection.clone());
    wait_for_store(cx, &store).await;
    cx.wait_for(window, Duration::from_secs(60), |_, cx| {
        connection.read(cx).state().is_connected()
    })
    .await;

    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello world").expect("write the fixture");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("upload-browse").label(),
            Some("Browse for the file to upload"),
            "the browse control is named for assistive technology"
        );
        // The control is a tab stop, like the path field and Upload (N5).
        tab_to(window, "upload-browse", cx);
        window.click("upload-browse", cx);
    })
    .expect("open the file picker");

    assert!(
        cx.did_prompt_for_paths(),
        "the manual upload offers the OS file picker"
    );
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files, "the upload picker chooses files");
        assert!(
            !options.directories,
            "the upload picker does not choose directories"
        );
        assert!(!options.multiple, "#30: one file at a time");
        Some(vec![file.clone()])
    });
    cx.run_until_parked();

    let path_text = file.display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            row_text_input(window, "upload-path").value(),
            Some(path_text.as_str()),
            "the chosen file fills the existing field"
        );
        assert_eq!(
            window.find("upload-result").label(),
            Some(UploadReport::Idle.message().as_str()),
            "picking a file is not an outcome"
        );
        window.click("upload-button", cx);
    })
    .expect("upload the chosen file");
    cx.wait_for(window, Duration::from_secs(60), |window, _| {
        window.find("upload-result").label().is_some_and(|label| {
            !label.starts_with("Uploading") && label != "No file has been uploaded yet."
        })
    })
    .await;

    let label = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.find("upload-result").label().unwrap_or_default().to_string()
        })
        .expect("read the result");
    assert!(
        label.starts_with("Uploaded"),
        "the browsed file was uploaded: {label}"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "the browsed path reached the upload handler"
    );
}

#[gpui_kit::test]
fn cancelling_the_file_picker_changes_nothing(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = cx.update(|cx| {
        let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), store))
    });
    let (window, _store, _view) = open_upload(cx, &paths, connection);
    let typed = temp.path().join("typed but not chosen.txt").display().to_string();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "upload-path", cx);
        window.input(&typed, cx);
        window.click("upload-browse", cx);
    })
    .expect("open the picker with the field already typed in");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            row_text_input(window, "upload-path").value(),
            Some(typed.as_str()),
            "#30: a cancel leaves the typed path alone"
        );
        assert!(
            window.try_find("upload-browse-notice").is_none(),
            "a cancel is not an error"
        );
        assert_eq!(
            window.find("upload-result").label(),
            Some(UploadReport::Idle.message().as_str()),
            "#30: a cancel uploads nothing"
        );
    })
    .expect("read the field after the cancel");
}

#[gpui_kit::test]
async fn a_file_picker_that_cannot_open_keeps_the_typed_path_usable(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = cx.update(|cx| {
        let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), store))
    });
    let (window, store, view) = open_upload(cx, &paths, connection);
    wait_for_store(cx, &store).await;
    let file = temp.path().join("hello.txt");
    std::fs::write(&file, b"hello world").expect("write the fixture");
    let typed = file.display().to_string();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "upload-path", cx);
        window.input(&typed, cx);
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
        let notice = window.find("upload-browse-notice");
        assert_eq!(
            notice.role(),
            Some(Role::Alert),
            "the failure notice is announced"
        );
        let label = notice.label().expect("the notice has a message").to_string();
        assert!(
            label.contains("xdg-desktop-portal"),
            "the notice keeps the reason the dialog gave: {label}"
        );
        assert_eq!(
            row_text_input(window, "upload-path").value(),
            Some(typed.as_str()),
            "the typed path survives a failure"
        );
        // And it is still usable: Upload submits it (there is no session here,
        // so the model's answer proves the typed path reached the handler).
        window.click("upload-button", cx);
    })
    .expect("use the typed path after the failure");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("upload-result").label(),
            Some(
                UploadReport::CannotRun {
                    message: "Sign in to a server before uploading a file.".to_string()
                }
                .message()
                .as_str()
            ),
            "#30: the typed path still reaches the upload handler after a failed picker"
        );
    })
    .expect("read the result");
}
