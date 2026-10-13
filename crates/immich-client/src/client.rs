//! The hand-written Immich HTTP client (ADR 0006).

use std::time::Duration;

use reqwest::{Client, Response, StatusCode, Url};
use serde::Serialize;

use crate::error::ClientError;
use crate::model::{AuthStatus, LoginResponse, ServerAbout, SessionToken};

/// The longest error message body kept from the server. A reverse proxy can
/// answer with an HTML page; the client keeps a short, useful prefix.
const MESSAGE_LIMIT: usize = 200;

/// A request that has not finished within this window is unreachable.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A typed client for the Immich Stable operations the client uses.
///
/// The client is cheap to clone and holds no credential. The session owns the
/// credential; the client only sends what the caller passes in.
#[derive(Clone, Debug)]
pub struct ImmichClient {
    http: Client,
    base: Url,
}

/// The login request body. It does not derive `Debug`, so a password cannot
/// reach a log through a stray `{:?}`.
#[derive(Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

impl ImmichClient {
    /// Builds a client for one server. TLS verification stays on (N3).
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidUrl`] for a URL that cannot be parsed, and
    /// [`ClientError::HttpClient`] when the TLS backend cannot start.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let mut base = Url::parse(base_url.trim())
            .map_err(|error| ClientError::InvalidUrl(error.to_string()))?;
        // A base with a path prefix (`https://host/immich`) must keep the
        // prefix when an endpoint is joined to it.
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ClientError::HttpClient)?;
        Ok(Self { http, base })
    }

    /// Logs in with an email address and password (R1.3).
    ///
    /// # Errors
    ///
    /// [`ClientError::WrongCredentials`] when the server rejects the password,
    /// [`ClientError::PasswordLoginDisabled`] when the server has password
    /// login turned off (R1.6), and the transport and protocol variants.
    pub async fn login(&self, email: &str, password: &str) -> Result<LoginResponse, ClientError> {
        let response = self
            .http
            .post(self.endpoint("auth/login"))
            .json(&LoginRequest { email, password })
            .send()
            .await
            .map_err(ClientError::Unreachable)?;
        let result = self.read_login(response).await;
        log_outcome("login", &result);
        result
    }

    /// Confirms that a session token is still valid (R6.6).
    ///
    /// # Errors
    ///
    /// [`ClientError::InvalidToken`] when the server refuses the token, plus the
    /// transport and protocol variants.
    pub async fn validate_token(&self, token: &SessionToken) -> Result<(), ClientError> {
        let response = self
            .http
            .post(self.endpoint("auth/validateToken"))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(ClientError::Unreachable)?;
        let result = self
            .read_auth_status(response, ClientError::InvalidToken)
            .await;
        log_outcome("validateToken", &result);
        result
    }

    /// Confirms that an API key is still valid (R1.5).
    ///
    /// # Errors
    ///
    /// [`ClientError::InvalidApiKey`] when the server refuses the key, plus the
    /// transport and protocol variants.
    pub async fn validate_api_key(&self, key: &str) -> Result<(), ClientError> {
        let response = self
            .http
            .post(self.endpoint("auth/validateToken"))
            .header("x-api-key", key)
            .send()
            .await
            .map_err(ClientError::Unreachable)?;
        let result = self
            .read_auth_status(response, ClientError::InvalidApiKey)
            .await;
        log_outcome("validateApiKey", &result);
        result
    }

    /// Reads the server version.
    ///
    /// # Errors
    ///
    /// Returns the transport and protocol variants.
    pub async fn about(&self) -> Result<ServerAbout, ClientError> {
        let response = self
            .http
            .get(self.endpoint("server/about"))
            .send()
            .await
            .map_err(ClientError::Unreachable)?;
        let status = response.status();
        let result = if status.is_success() {
            response
                .json::<ServerAbout>()
                .await
                .map_err(|error| ClientError::Protocol {
                    status: status.as_u16(),
                    message: error.to_string(),
                })
        } else {
            Err(ClientError::Protocol {
                status: status.as_u16(),
                message: server_message(response).await,
            })
        };
        log_outcome("about", &result);
        result
    }

    /// The absolute URL of one endpoint.
    fn endpoint(&self, path: &str) -> Url {
        self.base
            .join(path)
            .expect("the base URL ends in a slash before a relative path")
    }

    /// Maps a login response to its result.
    async fn read_login(&self, response: Response) -> Result<LoginResponse, ClientError> {
        let status = response.status();
        if status.is_success() {
            return response
                .json::<LoginResponse>()
                .await
                .map_err(|error| ClientError::Protocol {
                    status: status.as_u16(),
                    message: error.to_string(),
                });
        }
        let message = server_message(response).await;
        match status {
            // The server answers 401 for both a wrong password and a disabled
            // password login; only the message separates them (R1.6).
            StatusCode::UNAUTHORIZED if message.contains("disabled") => {
                Err(ClientError::PasswordLoginDisabled)
            }
            StatusCode::UNAUTHORIZED => Err(ClientError::WrongCredentials),
            _ => Err(ClientError::Protocol {
                status: status.as_u16(),
                message,
            }),
        }
    }

    /// Maps a `validateToken` response to its result.
    async fn read_auth_status(
        &self,
        response: Response,
        refusal: ClientError,
    ) -> Result<(), ClientError> {
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(refusal);
        }
        if !status.is_success() {
            return Err(ClientError::Protocol {
                status: status.as_u16(),
                message: server_message(response).await,
            });
        }
        let body = response
            .json::<AuthStatus>()
            .await
            .map_err(|error| ClientError::Protocol {
                status: status.as_u16(),
                message: error.to_string(),
            })?;
        if body.auth_status {
            Ok(())
        } else {
            Err(refusal)
        }
    }
}

/// The server's message from an error response, shortened and without secrets.
async fn server_message(response: Response) -> String {
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|body| {
            body.get("message")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or(text);
    message.chars().take(MESSAGE_LIMIT).collect()
}

/// Records the outcome of one request as structured, secret-free fields.
fn log_outcome<T>(operation: &'static str, result: &Result<T, ClientError>) {
    match result {
        Ok(_) => tracing::debug!(operation, outcome = "ok", "Immich request finished"),
        Err(error) => {
            tracing::debug!(
                operation,
                outcome = "error",
                error = %error,
                "Immich request finished"
            );
        }
    }
}
