//! The typed client against the offline Immich test double.

use immich_client::{ClientError, ImmichClient};
use mock_immich::{Fault, MockImmich, Operation};

fn client(server: &MockImmich) -> ImmichClient {
    ImmichClient::new(server.base_url()).expect("the mock URL is valid")
}

#[tokio::test]
async fn login_returns_the_session_token_and_the_user() {
    let server = MockImmich::start().await;
    let client = client(&server);

    let login = client
        .login("sync@example.com", "correct horse")
        .await
        .expect("the mock accepts these credentials");

    assert_eq!(login.user_email, "sync@example.com");
    assert!(!login.access_token.expose().is_empty());
    assert!(login.is_onboarded);
}

#[tokio::test]
async fn a_wrong_password_is_its_own_error() {
    let server = MockImmich::start().await;
    let client = client(&server);

    let error = client
        .login("sync@example.com", "wrong")
        .await
        .expect_err("the mock rejects a wrong password");

    assert!(matches!(error, ClientError::WrongCredentials), "{error:?}");
}

#[tokio::test]
async fn a_server_with_password_login_disabled_is_its_own_error() {
    let server = MockImmich::start().await;
    server.disable_password_login();
    let client = client(&server);

    let error = client
        .login("sync@example.com", "correct horse")
        .await
        .expect_err("password login is disabled");

    assert!(
        matches!(error, ClientError::PasswordLoginDisabled),
        "the 401 must be told apart from a wrong password (R1.6): {error:?}"
    );
}

#[tokio::test]
async fn an_api_key_is_validated_and_a_wrong_key_is_rejected() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-1234");
    let client = client(&server);

    client
        .validate_api_key("api-key-1234")
        .await
        .expect("the configured key is valid");

    let error = client
        .validate_api_key("api-key-5678")
        .await
        .expect_err("the mock rejects an unknown key");
    assert!(matches!(error, ClientError::InvalidApiKey), "{error:?}");
}

#[tokio::test]
async fn a_revoked_session_token_is_rejected() {
    let server = MockImmich::start().await;
    server.require_known_credentials();
    let client = client(&server);

    let login = client
        .login("sync@example.com", "correct horse")
        .await
        .expect("login succeeds before the revocation");
    client
        .validate_token(&login.access_token)
        .await
        .expect("the issued token is valid");

    server.revoke_tokens();
    let error = client
        .validate_token(&login.access_token)
        .await
        .expect_err("the revoked token is refused");
    assert!(matches!(error, ClientError::InvalidToken), "{error:?}");
}

#[tokio::test]
async fn an_unreachable_server_is_its_own_error() {
    // Bind a port, then release it: the connection is refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let port = listener.local_addr().expect("the port is known").port();
    drop(listener);

    let client = ImmichClient::new(&format!("http://127.0.0.1:{port}")).expect("a valid URL");
    let error = client
        .about()
        .await
        .expect_err("nothing listens on the released port");

    assert!(matches!(error, ClientError::Unreachable(_)), "{error:?}");
}

#[tokio::test]
async fn an_empty_success_body_is_a_transient_failure() {
    let server = MockImmich::start().await;
    let client = client(&server);

    // A 201 whose body never arrived is transport noise: the status alone does
    // not describe the login result, so a later attempt may succeed.
    server.fail(
        Operation::Login,
        Fault::Status {
            code: 201,
            retry_after: None,
        },
    );
    let error = client
        .login("sync@example.com", "correct horse")
        .await
        .expect_err("the body is not a login response");
    assert!(matches!(error, ClientError::Unreachable(_)), "{error:?}");
}

#[tokio::test]
async fn a_500_stays_a_protocol_error_with_its_status() {
    let server = MockImmich::start().await;
    let client = client(&server);

    server.fail(
        Operation::About,
        Fault::Status {
            code: 500,
            retry_after: None,
        },
    );
    let error = client.about().await.expect_err("the server failed");
    assert!(
        matches!(error, ClientError::Protocol { status: 500, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn about_reports_the_server_version() {
    let server = MockImmich::start().await;
    let client = client(&server);

    let about = client
        .about()
        .await
        .expect("the mock answers /server/about");

    assert_eq!(about.version, "v3.3.1");
}

#[test]
fn an_invalid_server_url_is_rejected() {
    let error = ImmichClient::new("not a url").expect_err("not a URL");
    assert!(matches!(error, ClientError::InvalidUrl(_)), "{error:?}");
}

#[test]
fn a_url_with_embedded_credentials_is_rejected() {
    // The password in `user:password@` would reach the settings file, a log
    // line, and a request error, so the client refuses it up front.
    for url in [
        "https://alice:hunter2@immich.example.com",
        "https://alice@immich.example.com",
    ] {
        let error = ImmichClient::new(url).expect_err("credentials belong in the credential field");
        assert!(
            matches!(error, ClientError::InvalidUrl(_)),
            "{url}: {error:?}"
        );
    }
}

#[test]
fn a_url_that_is_not_http_is_rejected_instead_of_panicking() {
    // `Url::parse` accepts all of these, but no endpoint can be joined to
    // them: the server URL field and a hand-edited settings file can hold one.
    for url in [
        "mailto:foo@bar",
        "data:text/plain,hello",
        "tel:+15551234",
        "urn:isbn:0451450523",
    ] {
        let error = ImmichClient::new(url).expect_err("only http and https are usable");
        assert!(
            matches!(error, ClientError::InvalidUrl(_)),
            "{url}: {error:?}"
        );
    }
}
