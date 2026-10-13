//! The OS credential store the shell hands to the connection session
//! (SPEC R1.2, R1.5, N3).
//!
//! Windows files the password in the Credential Manager; Linux uses the Secret
//! Service (GNOME Keyring or KWallet). The credential never enters the settings
//! file and never enters a log.
//!
//! A machine with no Secret Service provider must not panic: the platform store
//! becomes a value whose every operation reports a message that names what to
//! install and what to do instead, and the connection form shows it.

use std::sync::{Arc, OnceLock};

use sync_core::credential::{CredentialError, CredentialStore, Secret, SharedCredentialStore};

/// The service every entry is filed under.
pub const SERVICE: &str = "immich-sync";
/// The account every entry is filed under.
///
/// SPEC scope is one server and one account per install, so one entry holds the
/// current credential and a new sign-in replaces it.
pub const ACCOUNT: &str = "immich-sync";

/// The process-wide platform credential store.
///
/// The keyring's default store is a process-global, so it is installed once.
#[must_use]
pub fn platform_credential_store() -> SharedCredentialStore {
    static STORE: OnceLock<SharedCredentialStore> = OnceLock::new();
    STORE
        .get_or_init(|| match install_platform_store() {
            Ok(()) => Arc::new(KeyringStore),
            Err(message) => Arc::new(UnavailableStore::new(message)),
        })
        .clone()
}

/// The message for a Linux desktop with no Secret Service provider.
#[must_use]
pub fn secret_service_message(reason: &str) -> String {
    format!(
        "No Secret Service provider is available, so the password cannot be stored on this \
         computer. Install or start GNOME Keyring (or another Secret Service provider), then sign \
         in again. ({reason})"
    )
}

/// The message for a Windows Credential Manager that could not be opened.
#[must_use]
pub fn credential_manager_message(reason: &str) -> String {
    format!(
        "The Windows Credential Manager could not be opened, so the password cannot be stored. \
         Sign in again, or check the credential store. ({reason})"
    )
}

/// The message for a platform this build has no store for.
#[must_use]
pub fn unsupported_platform_message(platform: &str) -> String {
    format!(
        "This build has no OS credential store for {platform}, so the password cannot be stored."
    )
}

/// Installs the platform store as the keyring default.
#[cfg(target_os = "linux")]
fn install_platform_store() -> Result<(), String> {
    let store = dbus_secret_service_keyring_store::Store::new()
        .map_err(|error| secret_service_message(&error.to_string()))?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// Installs the platform store as the keyring default.
#[cfg(target_os = "windows")]
fn install_platform_store() -> Result<(), String> {
    let store = windows_native_keyring_store::Store::new()
        .map_err(|error| credential_manager_message(&error.to_string()))?;
    keyring_core::set_default_store(Arc::new(store));
    Ok(())
}

/// Reports the platform as unsupported. macOS is deferred (SPEC N1).
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn install_platform_store() -> Result<(), String> {
    Err(unsupported_platform_message(std::env::consts::OS))
}

/// The real store: one entry in the process-wide keyring store.
struct KeyringStore;

impl CredentialStore for KeyringStore {
    fn get(&self) -> Result<Option<Secret>, CredentialError> {
        match self.entry("read the credential")?.get_password() {
            Ok(secret) => Ok(Some(Secret::new(secret))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(failed("read the credential", error)),
        }
    }

    fn set(&self, secret: &Secret) -> Result<(), CredentialError> {
        self.entry("save the credential")?
            .set_password(secret.expose())
            .map_err(|error| failed("save the credential", error))
    }

    fn delete(&self) -> Result<(), CredentialError> {
        match self.entry("remove the credential")?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(failed("remove the credential", error)),
        }
    }
}

impl KeyringStore {
    /// The one keyring entry this install uses.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::Failed`] when the keyring rejects the entry
    /// specifier.
    fn entry(&self, action: &'static str) -> Result<keyring_core::Entry, CredentialError> {
        keyring_core::Entry::new(SERVICE, ACCOUNT).map_err(|error| failed(action, error))
    }
}

/// Maps a keyring failure to the store error.
fn failed(action: &'static str, error: keyring_core::Error) -> CredentialError {
    CredentialError::Failed {
        action,
        reason: error.to_string(),
    }
}

/// A store standing in for a missing provider. Every operation reports the
/// message, so the connection form shows it instead of the app failing.
#[derive(Debug)]
pub struct UnavailableStore {
    message: String,
}

impl UnavailableStore {
    /// A store whose every operation reports `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn error(&self) -> CredentialError {
        CredentialError::Unavailable(self.message.clone())
    }
}

impl CredentialStore for UnavailableStore {
    fn get(&self) -> Result<Option<Secret>, CredentialError> {
        Err(self.error())
    }

    fn set(&self, _secret: &Secret) -> Result<(), CredentialError> {
        Err(self.error())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        Err(self.error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_service_message_names_the_provider_and_the_next_step() {
        let message = secret_service_message("D-Bus was not reachable");

        assert!(message.contains("Secret Service"), "{message}");
        assert!(message.contains("GNOME Keyring"), "{message}");
        assert!(message.contains("D-Bus was not reachable"), "{message}");
    }

    #[test]
    fn the_credential_manager_message_names_the_store() {
        let message = credential_manager_message("access denied");

        assert!(message.contains("Credential Manager"), "{message}");
        assert!(message.contains("access denied"), "{message}");
    }

    #[test]
    fn an_unavailable_store_reports_the_message_instead_of_panicking() {
        let store = UnavailableStore::new("No Secret Service provider is available.");

        for result in [
            store.get().map(|_| ()),
            store.set(&Secret::new("secret")),
            store.delete(),
        ] {
            let error = result.expect_err("the store is unavailable");
            assert!(
                matches!(error, CredentialError::Unavailable(ref message) if message.contains("Secret Service")),
                "{error:?}"
            );
        }
    }
}
