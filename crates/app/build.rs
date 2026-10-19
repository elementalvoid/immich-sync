//! Embeds the short git revision this binary was built from (SPEC R10.2) and,
//! for Windows targets, the app icon the shell reads off the executable
//! (SPEC R11.1).
//!
//! The revision lookup is `src/git_revision.rs`, included here and compiled
//! into the app's tests as well, so the fallback the build takes is the one the
//! tests exercise. A tree with no repository (or no git installed) still
//! builds: the revision is `unknown`.

mod git_revision {
    include!("src/git_revision.rs");
}

use std::path::PathBuf;

fn main() {
    let package_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/git_revision.rs");
    println!(
        "cargo:rustc-env=IMMICH_SYNC_GIT_REVISION={}",
        git_revision::revision_label(&package_dir)
    );
    for path in git_revision::watch_paths(&package_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    embed_windows_icon();
}

/// Puts `icons/icon.ico` into the Windows binaries as icon resource 1.
///
/// Resource 1 is the id GPUI's Windows backend loads for the window class
/// (`LoadImageW(module, MAKEINTRESOURCE(1), IMAGE_ICON, ..)`), and Explorer,
/// the taskbar, and Settings → Apps read the same resource, so one resource
/// covers every shell surface.
///
/// The gate reads `CARGO_CFG_TARGET_OS` rather than `#[cfg(windows)]`: a build
/// script is compiled for the *host*, so `#[cfg(windows)]` is false when
/// cross-compiling to Windows and true when building on Windows for another
/// target. For the same reason `embed-resource` sits in the plain
/// `[build-dependencies]` table, not `[target.'cfg(windows)'.build-dependencies]`,
/// which cargo resolves against the host too and would leave this file with no
/// crate to call when a Linux host builds for Windows.
fn embed_windows_icon() {
    // Unconditional, so swapping the artwork rebuilds the executable on every
    // platform rather than only where the resource is embedded. `embed-resource`
    // emits no `rerun-if-changed` of its own (it cannot trace what a resource
    // file includes), so the two files it reads are named here.
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // A Windows build that cannot produce the icon is a broken artifact, so this
    // fails the build rather than shipping an executable with the default icon.
    // `manifest_required` still returns Ok for a non-Windows target, so the gate
    // above is the only thing keeping this inert elsewhere.
    embed_resource::compile("icon.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|reason| {
            panic!("cannot embed icons/icon.ico into the Windows binary: {reason}")
        });
}
