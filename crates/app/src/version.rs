//! The client version (SPEC R10.2): the package semver and the short revision
//! the binary was built from.

/// The package version on its own, for example `0.1.0`.
///
/// The update check compares a release tag with this, not with
/// [`client_version`]: the revision would never compare against a tag (R12).
#[must_use]
pub const fn package_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The client version string, for example `0.1.0 (abc1234)`.
///
/// The revision is embedded by `build.rs`, and `unknown` outside a git
/// repository. Diagnostics and the log's start line both read this, so the two
/// cannot drift.
#[must_use]
pub const fn client_version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("IMMICH_SYNC_GIT_REVISION"),
        ")"
    )
}
