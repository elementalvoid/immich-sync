//! Hashing pins against known, independent literals.

use std::path::Path;

use sync_core::hash::{Sha1, sha1, sha1_file};

/// `sha1("hello world")`, from the published test vector.
const HELLO_WORLD_SHA1_HEX: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
/// The same digest, base64.
const HELLO_WORLD_SHA1_BASE64: &str = "Kq5sNclPz7QV2+lfQIuc6R7oRu0=";

#[test]
fn sha1_of_hello_world_matches_the_known_literals() {
    let digest = sha1(b"hello world");

    assert_eq!(digest.hex(), HELLO_WORLD_SHA1_HEX);
    assert_eq!(digest.base64(), HELLO_WORLD_SHA1_BASE64);
    assert_eq!(
        Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a valid hex digest"),
        digest
    );
    assert_eq!(
        format!("{digest:?}"),
        format!("Sha1({HELLO_WORLD_SHA1_HEX})"),
        "Debug is the comparable hex form"
    );
}

#[test]
fn hashing_a_file_matches_hashing_its_bytes() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("hello.txt");
    std::fs::write(&path, b"hello world").expect("write the fixture");

    let digest = sha1_file(&path).expect("hash the fixture");

    assert_eq!(digest.hex(), HELLO_WORLD_SHA1_HEX);
}

#[test]
fn hashing_a_missing_file_is_a_local_read_error() {
    let error = sha1_file(Path::new("/nonexistent/immich-sync/missing.txt"))
        .expect_err("the file does not exist");

    assert!(
        error.to_string().contains("missing.txt"),
        "the error names the file: {error}"
    );
}

#[test]
fn from_hex_refuses_a_wrong_length_or_a_non_hex_character() {
    assert!(Sha1::from_hex("abc").is_none());
    assert!(
        Sha1::from_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_none(),
        "not a hex digest"
    );
}
