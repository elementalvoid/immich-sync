//! Opt-in integration test for the real OS credential store.
//!
//! It is ignored by default: CI has no unlocked Secret Service, and a developer
//! chooses when their real credential store is touched. The test uses a
//! test-only service and account, so the entry the application stores is never
//! read, overwritten, or deleted.
//!
//! Linux, with a session bus and an unlocked keyring:
//!
//! ```sh
//! cd <checkout>
//! dbus-run-session -- bash -c '
//!   eval "$(printf test | gnome-keyring-daemon --unlock --components=secrets)"
//!   cargo test -p immich-sync --test keyring -- --ignored --nocapture
//! '
//! ```
//!
//! Windows, against the Credential Manager:
//!
//! ```sh
//! cargo test -p immich-sync --test keyring -- --ignored --nocapture
//! ```
//!
//! macOS, against the login Keychain (it prompts for the test entry once per
//! ad-hoc rebuild, which is the unsigned-build cost in ADR 0008):
//!
//! ```sh
//! cargo test -p immich-sync --test keyring -- --ignored --nocapture
//! ```

use immich_sync::credentials::{keyring_store, platform_credential_store};
use sync_core::credential::Secret;

#[test]
#[ignore = "needs a real OS credential store; see the module docs for the command"]
fn the_real_credential_store_round_trips_a_secret() {
    // Installs the platform store and holds the message when there is none.
    let platform = platform_credential_store();
    // A test-only entry, so the application's own credential stays untouched.
    let store = keyring_store("immich-sync-test", "round-trip");
    let secret = Secret::new(format!("immich-sync-test-{}", std::process::id()));

    store.set(&secret).expect("save to the real store");
    assert_eq!(
        store.get().expect("read the real store"),
        Some(secret.clone()),
        "the real store returned what was written"
    );
    store.delete().expect("remove from the real store");
    assert_eq!(
        store.get().expect("read after the delete"),
        None,
        "the test entry is gone again"
    );

    drop(platform);
}
