//! The OS credential store (SPEC R1.2, R1.5, N3).
//!
//! The password or the API key lives in the operating-system credential store.
//! The engine reaches it through [`CredentialStore`] so tests use a
//! deterministic in-memory store and the shell supplies the platform store.
//! The value never enters the settings file and never enters a log:
//! [`Secret`] redacts its `Debug` output and only [`Secret::expose`] reveals it.

use std::fmt;
use std::sync::{Arc, Mutex};

use thiserror::Error;

/// The one secret this install stores: the password or the API key.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value, for the credential store or an authentication call.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// Why the credential store could not be used.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialError {
    /// No usable credential store exists. The message is already written for
    /// the user and names what to do (SPEC R1.2).
    #[error("{0}")]
    Unavailable(String),
    /// The store exists but the operation failed.
    #[error("the OS credential store could not {action}: {reason}")]
    Failed {
        /// What the store was asked to do, for example `read the credential`.
        action: &'static str,
        /// The platform's description of the failure.
        reason: String,
    },
}

/// The operating-system credential store, behind a seam for tests.
pub trait CredentialStore: Send + Sync {
    /// The stored secret, or `None` on a first run.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when the store is unavailable or fails.
    fn get(&self) -> Result<Option<Secret>, CredentialError>;

    /// Stores the secret, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when the store is unavailable or fails.
    fn set(&self, secret: &Secret) -> Result<(), CredentialError>;

    /// Removes the stored secret. A missing secret is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError`] when the store is unavailable or fails.
    fn delete(&self) -> Result<(), CredentialError>;
}

/// An in-memory [`CredentialStore`] for tests and for a headless build.
///
/// It also records every value written, so a test can prove what the session
/// persisted without reaching into the store.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    secret: Mutex<Option<Secret>>,
    writes: Mutex<Vec<Secret>>,
    unavailable: Option<String>,
}

impl MemoryCredentialStore {
    /// An empty store that accepts writes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose every operation fails with `message`, standing in for a
    /// missing Secret Service provider.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            unavailable: Some(message.into()),
            ..Self::default()
        }
    }

    /// Every value written through [`CredentialStore::set`], in order.
    #[must_use]
    pub fn writes(&self) -> Vec<Secret> {
        self.writes
            .lock()
            .expect("the write log is not poisoned")
            .clone()
    }

    fn unavailable_error(&self) -> Option<CredentialError> {
        self.unavailable.clone().map(CredentialError::Unavailable)
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self) -> Result<Option<Secret>, CredentialError> {
        if let Some(error) = self.unavailable_error() {
            return Err(error);
        }
        Ok(self
            .secret
            .lock()
            .expect("the store is not poisoned")
            .clone())
    }

    fn set(&self, secret: &Secret) -> Result<(), CredentialError> {
        if let Some(error) = self.unavailable_error() {
            return Err(error);
        }
        *self.secret.lock().expect("the store is not poisoned") = Some(secret.clone());
        self.writes
            .lock()
            .expect("the write log is not poisoned")
            .push(secret.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        if let Some(error) = self.unavailable_error() {
            return Err(error);
        }
        *self.secret.lock().expect("the store is not poisoned") = None;
        Ok(())
    }
}

/// A shared credential store, as the shell and the engine hand it around.
pub type SharedCredentialStore = Arc<dyn CredentialStore>;
