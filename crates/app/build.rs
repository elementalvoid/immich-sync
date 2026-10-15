//! Build glue for the Windows binary (SPEC N1).
//!
//! The MSVC linker embeds [`immich-sync.manifest`], whose `longPathAware`
//! setting lifts the legacy 260-character path limit. Only the MSVC tools
//! accept `/MANIFEST:EMBED` and `/MANIFESTINPUT`, and only the main binary
//! needs the manifest: the test binaries are not shipped.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=immich-sync.manifest");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS");
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV");
    if target_os.as_deref() != Ok("windows") || target_env.as_deref() != Ok("msvc") {
        return;
    }
    let manifest_directory = std::env::var("CARGO_MANIFEST_DIR").expect("the manifest directory");
    let manifest = Path::new(&manifest_directory).join("immich-sync.manifest");
    println!("cargo:rustc-link-arg-bin=immich-sync=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=immich-sync=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
