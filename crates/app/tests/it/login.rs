//! Login against the offline Immich double, through the real settings file.
//!
//! This test owns the secrecy acceptance criteria of issue #4: the password or
//! the API key never reaches the TOML file, and the session token never reaches
//! any file at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use immich_sync::connection::{apply_profile, profile_from_settings};
use immich_sync::settings::SettingsStore;
use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, Connection, ConnectionState, Secret, SignIn};
use sync_core::credential::{CredentialStore as _, MemoryCredentialStore};

/// Every regular file under `root`.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read a directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Fails when any file under `root` contains `secret`.
fn assert_no_file_contains(root: &Path, secret: &str, what: &str) {
    for file in files(root) {
        let bytes = fs::read(&file).expect("read a file the login wrote");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(secret),
            "{} contains the {what}",
            file.display()
        );
    }
}

#[tokio::test]
async fn a_password_login_keeps_the_password_and_the_token_out_of_every_file() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let config = temp.path().join("config").join("settings.toml");
    let mut settings = SettingsStore::open(config.clone());

    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection = Connection::new(server.base_url(), store.clone()).expect("a valid URL");

    let password = "correct horse";
    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(password),
            method: AuthMethod::Password,
        })
        .await;
    assert_eq!(state, ConnectionState::Connected, "{state:?}");
    let token = connection
        .token()
        .expect("a password login yields a session token")
        .expose()
        .to_string();

    // What the settings form does with an accepted sign-in.
    let profile = connection.profile().clone();
    settings
        .update(|settings| apply_profile(settings, &profile))
        .expect("save the profile");

    let text = fs::read_to_string(&config).expect("read the settings file");
    assert!(text.contains("server_url"), "{text}");
    assert!(text.contains("sync@example.com"), "{text}");
    assert!(!text.contains(password), "the TOML holds the password");
    assert!(!text.contains(&token), "the TOML holds the session token");
    assert_no_file_contains(temp.path(), password, "password");
    assert_no_file_contains(temp.path(), &token, "session token");

    // The credential store holds the password and never the token.
    assert_eq!(
        store
            .get()
            .expect("read the store")
            .expect("a stored secret")
            .expose(),
        password
    );

    // A restart relogs in with no user input (acceptance criterion).
    let restored_profile = profile_from_settings(SettingsStore::open(config.clone()).settings());
    assert_eq!(restored_profile, profile, "the saved profile round-tripped");
    let mut restarted = Connection::new(server.base_url(), store.clone()).expect("a valid URL");
    let state = restarted.restore(&restored_profile).await;
    assert_eq!(state, ConnectionState::Connected, "{state:?}");
    let fresh = restarted
        .token()
        .expect("the restart has its own session")
        .expose()
        .to_string();
    assert_ne!(fresh, token, "the restart got a new session token");
    assert_no_file_contains(temp.path(), &fresh, "session token");
}

#[tokio::test]
async fn an_api_key_login_saves_the_method_and_keeps_the_key_out_of_the_file() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let config = temp.path().join("config").join("settings.toml");
    let mut settings = SettingsStore::open(config.clone());

    let server = MockImmich::start().await;
    let key = "api-key-abc123";
    server.set_api_key(key);
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection = Connection::new(server.base_url(), store.clone()).expect("a valid URL");

    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(key),
            method: AuthMethod::ApiKey,
        })
        .await;
    assert_eq!(state, ConnectionState::Connected, "{state:?}");
    assert!(connection.token().is_none(), "API-key mode has no token");

    let profile = connection.profile().clone();
    settings
        .update(|settings| apply_profile(settings, &profile))
        .expect("save the profile");

    let text = fs::read_to_string(&config).expect("read the settings file");
    assert!(text.contains("auth_method = \"api_key\""), "{text}");
    assert!(!text.contains(key), "the TOML holds the API key");
    assert_no_file_contains(temp.path(), key, "API key");
    assert_eq!(
        store
            .get()
            .expect("read the store")
            .expect("a stored secret")
            .expose(),
        key
    );
}
