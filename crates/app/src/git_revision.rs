// The `git rev-parse` lookup the build uses to embed the short revision.
// `build.rs` includes this file, so the tests below and the build run the same
// code; keep it dependent on std only.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::{revision_label, watch_paths};

    #[test]
    fn a_repository_reports_its_head() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        fs::write(temp.path().join("content.txt"), "content").expect("write a file");
        commit(temp.path());

        let head = Command::new("git")
            .current_dir(temp.path())
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .expect("run git");
        let expected = String::from_utf8(head.stdout)
            .expect("git prints utf-8")
            .trim()
            .to_string();

        assert_eq!(
            revision_label(temp.path()),
            expected,
            "the lookup reports the repository's own HEAD"
        );

        let head_file = temp.path().join(".git").join("HEAD");
        assert!(head_file.exists(), "git init wrote {}", head_file.display());
        assert!(
            watch_paths(temp.path()).contains(&head_file),
            "a commit moves the git dir's HEAD, so the build must watch it"
        );
    }

    #[test]
    fn a_directory_without_a_repository_reports_unknown() {
        let temp = tempfile::tempdir().expect("create a temp dir");

        assert_eq!(
            revision_label(temp.path()),
            "unknown",
            "a tree with no repository still builds, with no revision to name"
        );
        assert!(
            watch_paths(temp.path()).is_empty(),
            "with no repository there is no revision to watch"
        );
    }

    /// Makes `directory` a repository with one commit, configured so the
    /// machine's global git config cannot change what the test does.
    fn commit(directory: &Path) {
        git(directory, &["init", "--quiet"]);
        git(directory, &["add", "content.txt"]);
        git(
            directory,
            &[
                "-c",
                "user.name=immich-sync test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "content",
            ],
        );
    }

    fn git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
