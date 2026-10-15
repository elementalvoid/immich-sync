//! The connection session against the offline Immich test double.
//!
//! These tests own the acceptance criteria of issue #4: password login, API-key
//! mode, the 401 re-login budget, the restart re-login, and the secret rules.

use std::sync::Arc;

use mock_immich::MockImmich;
use sync_core::connection::{
    Attention, AuthMethod, Connection, ConnectionState, Secret, ServerProfile, SignIn, Unauthorized,
};
use sync_core::credential::{CredentialStore, MemoryCredentialStore};

fn profile(server: &MockImmich, method: AuthMethod) -> ServerProfile {
    ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method,
    }
}

#[tokio::test]
async fn signing_in_with_a_password_connects_and_stores_only_the_password() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;

    assert_eq!(state, ConnectionState::Connected);
    let token = connection
        .token()
        .expect("a password login yields a session token")
        .clone();
    assert!(!token.expose().is_empty());

    let writes = store.writes();
    assert_eq!(writes.len(), 1, "the store is written exactly once");
    assert_eq!(writes[0].expose(), "correct horse");
    assert_eq!(
        store.get().expect("the store is readable"),
        Some(Secret::new("correct horse")),
        "the store holds the password"
    );
    assert_ne!(writes[0].expose(), token.expose());
}

#[tokio::test]
async fn a_wrong_password_needs_attention_and_leaves_the_app_usable() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("wrong-password"),
            method: AuthMethod::Password,
        })
        .await;

    assert!(
        matches!(
            state,
            ConnectionState::NeedsAttention(Attention::WrongPassword)
        ),
        "{state:?}"
    );
    assert!(
        store.writes().is_empty(),
        "a rejected password is not persisted"
    );
    assert!(
        state.message().contains("rejected the password"),
        "{state:?}"
    );

    // The app stays usable: the next attempt succeeds.
    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    assert_eq!(state, ConnectionState::Connected, "{state:?}");
}

#[tokio::test]
async fn a_server_with_password_login_disabled_points_at_the_api_key() {
    let server = MockImmich::start().await;
    server.disable_password_login();
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;

    assert!(
        matches!(
            state,
            ConnectionState::NeedsAttention(Attention::PasswordLoginDisabled)
        ),
        "{state:?}"
    );
    assert!(state.message().contains("API key"), "{state:?}");
    assert!(store.writes().is_empty());
}

#[tokio::test]
async fn an_api_key_signs_in_without_a_session_token() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-abc123");
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("api-key-abc123"),
            method: AuthMethod::ApiKey,
        })
        .await;

    assert_eq!(state, ConnectionState::Connected, "{state:?}");
    assert!(
        connection.token().is_none(),
        "API-key mode has no session token"
    );
    assert_eq!(
        store.writes()[0].expose(),
        "api-key-abc123",
        "the key, not a token, is stored"
    );
}

#[tokio::test]
async fn an_invalid_api_key_at_startup_needs_attention() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-abc123");
    let store = Arc::new(MemoryCredentialStore::new());
    store
        .set(&Secret::new("api-key-stale"))
        .expect("seed the store");
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .restore(&profile(&server, AuthMethod::ApiKey))
        .await;

    assert!(
        matches!(
            state,
            ConnectionState::NeedsAttention(Attention::InvalidApiKey)
        ),
        "{state:?}"
    );
}

#[tokio::test]
async fn a_401_relogs_in_once_then_needs_attention() {
    let server = MockImmich::start().await;
    server.require_known_credentials();
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;

    // The server revokes the session: the next authenticated request is a 401.
    server.revoke_tokens();
    assert_eq!(
        connection.handle_unauthorized().await,
        Unauthorized::Retry,
        "the first 401 is answered with one re-login"
    );
    assert!(connection.state().is_connected());
    let refreshed = connection.token().expect("a fresh token").clone();

    // The retried request is refused again before any request succeeded.
    server.revoke_tokens();
    assert_eq!(
        connection.handle_unauthorized().await,
        Unauthorized::Stop,
        "a second 401 needs attention (R1.4)"
    );
    assert!(
        matches!(
            connection.state(),
            ConnectionState::NeedsAttention(Attention::Rejected)
        ),
        "{:?}",
        connection.state()
    );
    assert_ne!(refreshed.expose(), "correct horse");
}

#[tokio::test]
async fn a_successful_request_resets_the_re_login_budget() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;

    server.revoke_tokens();
    assert_eq!(connection.handle_unauthorized().await, Unauthorized::Retry);

    // The retried request succeeded, so the client has one re-login again.
    connection.note_authenticated();
    server.revoke_tokens();
    assert_eq!(connection.handle_unauthorized().await, Unauthorized::Retry);
}

#[tokio::test]
async fn a_restart_relogs_in_without_user_input() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let saved = profile(&server, AuthMethod::Password);
    let mut first = Connection::new(server.base_url(), store.clone()).expect("a valid server URL");
    first
        .sign_in(SignIn {
            email: saved.email.clone(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    assert_eq!(first.state(), &ConnectionState::Connected);

    // A second process starts with the same store and settings, and no input.
    let mut restarted =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");
    let state = restarted.restore(&saved).await;

    assert_eq!(state, ConnectionState::Connected, "{state:?}");
    assert!(restarted.token().is_some());
}

#[tokio::test]
async fn a_missing_credential_store_is_a_clear_message() {
    let server = MockImmich::start().await;
    let message = "No Secret Service provider was found on this computer. Start GNOME Keyring, \
                   then sign in again.";
    let store = Arc::new(MemoryCredentialStore::unavailable(message));
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .restore(&profile(&server, AuthMethod::Password))
        .await;

    assert!(
        matches!(state, ConnectionState::NeedsAttention(Attention::CredentialStore(ref text)) if text == message),
        "{state:?}"
    );
    assert!(state.message().contains("Secret Service"), "{state:?}");
}

#[tokio::test]
async fn an_unconfigured_profile_is_not_signed_in() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");

    let state = connection
        .restore(&ServerProfile {
            server_url: server.base_url().to_string(),
            email: String::new(),
            method: AuthMethod::Password,
        })
        .await;

    assert_eq!(state, ConnectionState::NotConfigured);
    assert!(connection.credential().is_none());
}

#[tokio::test]
async fn an_unreachable_server_needs_attention_with_the_reason() {
    // A listener that accepts and immediately closes every connection is a
    // server that cannot be reached. Keeping the listener for the whole test
    // also keeps the port reserved, so the ephemeral-port race (a parallel
    // mock re-binding a dropped port) cannot make this flaky.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let port = listener.local_addr().expect("the port is known").port();
    let refusing = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });

    let store = Arc::new(MemoryCredentialStore::new());
    store.set(&Secret::new("correct horse")).expect("seed");
    let mut connection =
        Connection::new(&format!("http://127.0.0.1:{port}"), store.clone()).expect("a valid URL");
    let state = connection
        .restore(&ServerProfile {
            server_url: format!("http://127.0.0.1:{port}"),
            email: "sync@example.com".to_string(),
            method: AuthMethod::Password,
        })
        .await;

    assert!(
        matches!(
            state,
            ConnectionState::NeedsAttention(Attention::Unreachable(_))
        ),
        "{state:?}"
    );
    refusing.abort();
}

#[tokio::test]
async fn the_session_token_never_reaches_the_credential_store() {
    let server = MockImmich::start().await;
    let store = Arc::new(MemoryCredentialStore::new());
    let mut connection =
        Connection::new(server.base_url(), store.clone()).expect("a valid server URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    let token = connection.token().expect("a session token").clone();

    // A re-login must not hand the fresh token to the store either.
    server.revoke_tokens();
    assert_eq!(connection.handle_unauthorized().await, Unauthorized::Retry);
    let refreshed = connection.token().expect("a fresh token").clone();

    for write in store.writes() {
        assert_ne!(write.expose(), token.expose());
        assert_ne!(write.expose(), refreshed.expose());
    }
    assert_eq!(
        store.get().expect("the store is readable"),
        Some(Secret::new("correct horse")),
        "the store still holds the password, never a token"
    );
}
