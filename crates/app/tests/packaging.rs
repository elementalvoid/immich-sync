//! The macOS packaging metadata (#67). Nothing here runs `cargo packager`: no
//! macOS host exists in this checkout, so the build that consumes these files
//! only ever runs in the release job. These tests read the files the packager
//! and the compiler read and assert the contract its 0.11.8 source defines —
//! `MacOsConfig` is `#[serde(deny_unknown_fields)]`, so any other key in the
//! table fails the artifact build, and `infoPlistPath` is merged over the
//! generated plist with a last-writer-wins `insert`, so a repeated key silently
//! wins over the manifest instead of being ignored.

use std::path::Path;

/// The usage strings macOS shows a TCC prompt for: the three folders a source
/// folder commonly lives in, and the two volume kinds it can be on.
const USAGE_KEYS: [&str; 5] = [
    "NSDesktopFolderUsageDescription",
    "NSDocumentsFolderUsageDescription",
    "NSDownloadsFolderUsageDescription",
    "NSNetworkVolumesUsageDescription",
    "NSRemovableVolumesUsageDescription",
];

#[test]
fn macos_config_sets_the_support_floor_and_names_the_plist() {
    let manifest = read("Cargo.toml");
    let manifest: toml::Value = toml::from_str(&manifest).expect("crates/app/Cargo.toml parses");
    let table = manifest["package"]["metadata"]["packager"]["macos"]
        .as_table()
        .expect("[package.metadata.packager.macos] exists");

    // Equality, not `contains`: the table must hold exactly these three keys.
    // `deny_unknown_fields` turns a misspelled or extra key — a `signingIdentity`
    // added "just in case", say — into a failed build on the macOS host, which
    // is the one host this checkout cannot try it on.
    let expected: toml::Value = toml::from_str(
        r#"
        minimumSystemVersion = "13.0"
        backgroundApp = false
        infoPlistPath = "macos/Info.plist"
        "#,
    )
    .expect("the expected table parses");
    assert_eq!(toml::Value::Table(table.clone()), expected);
}

#[test]
fn info_plist_carries_only_the_five_file_access_usage_strings() {
    let plist = read("macos/Info.plist");
    let mut keys = elements(&plist, "key");
    let values = elements(&plist, "string");
    keys.sort_unstable();

    // The counts must match: this file is a flat dictionary of strings, so a
    // boolean, an array, or a nested dictionary would change one count without
    // the other — and such a value would merge into the bundle unread here.
    assert_eq!(
        keys, USAGE_KEYS,
        "the plist holds exactly the five file-access usage strings, and nothing else"
    );
    assert_eq!(
        keys.len(),
        values.len(),
        "every key in the plist maps to one <string>"
    );
    for value in &values {
        assert!(
            !value.is_empty() && value.ends_with('.'),
            "{value:?} is an honest one-sentence reason"
        );
    }
}

#[test]
fn deployment_target_is_set_for_every_build() {
    // The packager writes LSMinimumSystemVersion from `minimumSystemVersion` but
    // does not set the compiler's deployment target; rustc's own arm64 default is
    // 11.0. Without this the binary floor and the plist disagree.
    let config: toml::Value =
        toml::from_str(&read("../../.cargo/config.toml")).expect(".cargo/config.toml parses");
    assert_eq!(
        config["env"]["MACOSX_DEPLOYMENT_TARGET"].as_str(),
        Some("13.0")
    );
}

/// A file next to this crate's manifest, read as UTF-8.
fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The bodies of every `<tag>…</tag>` in the document, in order. Enough for a
/// hand-written flat plist and free of an XML dependency the crate does not
/// otherwise need.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut bodies = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let end = after
            .find(&close)
            .unwrap_or_else(|| panic!("<{tag}> without {close}"));
        bodies.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    bodies
}
