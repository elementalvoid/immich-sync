//! N6 and the PLAN architecture rule: the engine never imports the shell.

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
