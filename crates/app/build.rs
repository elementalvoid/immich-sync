//! Embeds the short git revision this binary was built from (SPEC R10.2).
//!
//! The lookup is `src/git_revision.rs`, included here and compiled into the
//! app's tests as well, so the fallback the build takes is the one the tests
//! exercise. A tree with no repository (or no git installed) still builds:
//! the revision is `unknown`.

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
}
