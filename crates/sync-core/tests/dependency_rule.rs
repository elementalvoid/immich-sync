//! N6 and the PLAN architecture rule: the engine never imports the shell.

use std::path::{Path, PathBuf};

/// The names that would make the engine depend on the shell.
const SHELL_NAMES: [&str; 2] = ["gpui", "immich_sync"];

#[test]
fn sync_core_does_not_depend_on_the_shell() {
    let manifest = include_str!("../Cargo.toml");
    let dependencies = manifest
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'));

    for line in dependencies {
        assert!(
            !line.contains("gpui"),
            "sync-core must not depend on the GPUI shell: {line}"
        );
    }
}

#[test]
fn sync_core_sources_do_not_import_the_shell() {
    let sources = rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));

    for path in &sources {
        let source = std::fs::read_to_string(path).expect("read a sync-core source file");
        for line in source.lines() {
            // Only code counts: a comment may name the shell.
            let code = line.split("//").next().unwrap_or_default();
            for name in SHELL_NAMES {
                assert!(
                    !code.contains(name),
                    "{} imports the shell (`{name}`): {line}",
                    path.display()
                );
            }
        }
    }

    assert!(!sources.is_empty(), "found no sync-core source files");
}

/// Every `.rs` file under `root`.
fn rust_sources(root: PathBuf) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("read a source directory") {
            let path = entry.expect("read a source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources
}
