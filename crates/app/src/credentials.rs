//! The OS credential store the shell hands to the connection session
//! (SPEC R1.2, R1.5, N3).
//!
//! Windows files the password in the Credential Manager, Linux uses the Secret
//! Service (GNOME Keyring or KWallet), and macOS uses the login Keychain. The
//! credential never enters the settings file and never enters a log.
//!
//! A machine with no usable store must not panic: the platform store becomes a
//! value whose every operation reports a message that names what to install or
//! unlock and what to do instead, and the connection form shows it.

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
/// A failure to install is not cached: the provider can appear later (the user
/// starts GNOME Keyring), and the next call installs it then.
#[must_use]
pub fn platform_credential_store() -> SharedCredentialStore {
    static STORE: OnceLock<SharedCredentialStore> = OnceLock::new();
    cached_platform_store(&STORE, || {
        install_platform_store()?;
        Ok(keyring_store(SERVICE, ACCOUNT))
    })
}

/// Resolves the credential store for one sign-in (SPEC R1.2, R1.5).
///
/// The platform store can appear after the client started (the user starts
/// GNOME Keyring), so the connection model asks through this provider on every
/// sign-in instead of holding the store that existed when the window opened.
#[derive(Clone)]
pub struct CredentialStoreProvider(Arc<dyn Fn() -> SharedCredentialStore + Send + Sync>);

impl CredentialStoreProvider {
    /// A provider that calls `resolve` again for every sign-in.
    #[must_use]
    pub fn new(resolve: impl Fn() -> SharedCredentialStore + Send + Sync + 'static) -> Self {
        Self(Arc::new(resolve))
    }

    /// The store for the sign-in or restore that is starting.
    #[must_use]
    pub fn resolve(&self) -> SharedCredentialStore {
        (self.0)()
    }
}

impl From<SharedCredentialStore> for CredentialStoreProvider {
    /// A provider that always hands back `store`.
    fn from(store: SharedCredentialStore) -> Self {
        Self::new(move || store.clone())
    }
}

/// The store `install` produces, cached in `cache` only after a success.
///
/// A failed install is returned fresh every time, so a later sign-in retries.
/// A retry is safe because `set_default_store` replaces the process-wide store.
fn cached_platform_store(
    cache: &OnceLock<SharedCredentialStore>,
    install: impl FnOnce() -> Result<SharedCredentialStore, String>,
) -> SharedCredentialStore {
    if let Some(store) = cache.get() {
        return store.clone();
    }
    match install() {
        Ok(store) => cache.get_or_init(|| store).clone(),
        Err(message) => Arc::new(UnavailableStore::new(message)),
    }
}

/// A store for one keyring entry, over the platform store
/// [`platform_credential_store`] installed.
///
/// The opt-in integration test uses a test-only service and account, so it
/// never touches the entry the application stores.
#[must_use]
pub fn keyring_store(service: &str, account: &str) -> SharedCredentialStore {
    Arc::new(KeyringStore {
        service: service.to_string(),
        account: account.to_string(),
    })
}

/// The message for a Linux desktop with no Secret Service provider.
#[must_use]
pub fn secret_service_message(reason: &str) -> String {
    format!(
        "No Secret Service provider is available, so the password cannot be stored on this \
         computer. Install or start GNOME Keyring (or another Secret Service provider), then sign \
         in again: the client retries the credential store on every sign-in. ({reason})"
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

/// The message for a macOS login Keychain that could not be used.
///
/// It covers the open and every later operation, so it must not promise a
/// direction: a denied ACL breaks the read as readily as the write.
#[must_use]
pub fn keychain_message(reason: &str) -> String {
    format!(
        "The login Keychain could not be used, so the password cannot be read or saved. Allow \
         the Keychain prompt (or unlock the login Keychain in Keychain Access), then sign in \
         again. ({reason})"
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
    keyring_core::set_default_store(store);
    Ok(())
}

/// Installs the platform store as the keyring default.
#[cfg(target_os = "macos")]
fn install_platform_store() -> Result<(), String> {
    let store = apple_native_keyring_store::keychain::Store::new()
        .map_err(|error| keychain_message(&error.to_string()))?;
    keyring_core::set_default_store(store);
    Ok(())
}

/// Reports the platform as unsupported.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn install_platform_store() -> Result<(), String> {
    Err(unsupported_platform_message(std::env::consts::OS))
}

/// The real store: one entry in the process-wide keyring store.
struct KeyringStore {
    service: String,
    account: String,
}

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
        keyring_core::Entry::new(&self.service, &self.account)
            .map_err(|error| failed(action, error))
    }
}

/// Maps a keyring failure to the store error.
fn failed(action: &'static str, error: keyring_core::Error) -> CredentialError {
    CredentialError::Failed {
        action,
        reason: failure_reason(&error.to_string(), cfg!(target_os = "macos")),
    }
}

/// The reason a failed keyring operation reports, by platform.
///
/// On macOS a denied Keychain ACL prompt arrives here as a platform failure, and
/// the raw `keyring_core` text names no remedy (#62). Other platforms keep that
/// text: their store has no prompt to explain.
fn failure_reason(reason: &str, macos: bool) -> String {
    if macos {
        keychain_message(reason)
    } else {
        reason.to_string()
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
    use std::cell::Cell;
    use sync_core::credential::MemoryCredentialStore;

    #[test]
    fn a_missing_provider_is_retried_until_one_is_installed() {
        let cache: OnceLock<SharedCredentialStore> = OnceLock::new();
        let attempts = Cell::new(0);

        let unavailable = cached_platform_store(&cache, || {
            attempts.set(attempts.get() + 1);
            Err("no provider yet".to_string())
        });
        let error = unavailable.get().expect_err("the provider is missing");
        assert!(
            matches!(error, CredentialError::Unavailable(_)),
            "{error:?}"
        );
        assert_eq!(attempts.get(), 1);

        let installed = cached_platform_store(&cache, || {
            attempts.set(attempts.get() + 1);
            Ok(Arc::new(MemoryCredentialStore::new()) as SharedCredentialStore)
        });
        assert_eq!(attempts.get(), 2, "the failure was not cached");
        assert!(
            installed
                .get()
                .expect("the installed store works")
                .is_none(),
            "the second attempt installed a real store"
        );

        let cached = cached_platform_store(&cache, || {
            attempts.set(attempts.get() + 1);
            Ok(Arc::new(MemoryCredentialStore::new()) as SharedCredentialStore)
        });
        assert_eq!(attempts.get(), 2, "a success is cached");
        assert!(Arc::ptr_eq(&installed, &cached));
    }

    #[test]
    fn the_secret_service_message_names_the_provider_and_the_next_step() {
        let message = secret_service_message("D-Bus was not reachable");

        assert!(message.contains("Secret Service"), "{message}");
        assert!(message.contains("GNOME Keyring"), "{message}");
        assert!(message.contains("D-Bus was not reachable"), "{message}");
        assert!(
            message.contains("retries the credential store on every sign-in"),
            "the recovery step names the retry: {message}"
        );
    }

    #[test]
    fn the_credential_manager_message_names_the_store() {
        let message = credential_manager_message("access denied");

        assert!(message.contains("Credential Manager"), "{message}");
        assert!(message.contains("access denied"), "{message}");
    }

    #[test]
    fn the_keychain_message_names_the_login_keychain_and_the_next_step() {
        let message = keychain_message("the user canceled the operation");

        assert!(message.contains("login Keychain"), "{message}");
        assert!(message.contains("cannot be read or saved"), "{message}");
        assert!(message.contains("Keychain Access"), "{message}");
        assert!(message.contains("Allow the Keychain prompt"), "{message}");
        assert!(
            message.contains("the user canceled the operation"),
            "{message}"
        );
    }

    #[test]
    fn a_failed_keychain_operation_names_the_prompt_and_the_remedy_on_macos() {
        let reason = "the user denied the Keychain prompt";

        assert_eq!(
            failure_reason(reason, false),
            reason,
            "other platforms keep the store's own text"
        );
        let message = failure_reason(reason, true);
        assert!(message.contains("login Keychain"), "{message}");
        assert!(message.contains("Allow the Keychain prompt"), "{message}");
        assert!(message.contains("Keychain Access"), "{message}");
        assert!(
            message.contains(reason),
            "the original reason is kept: {message}"
        );
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
