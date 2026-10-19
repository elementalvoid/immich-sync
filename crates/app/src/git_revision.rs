// The `git rev-parse` lookup the build uses to embed the short revision.
// `build.rs` includes this file, so the tests below and the build run the same
// code; keep it dependent on std only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the build embeds: the short revision of `directory`'s repository, or
/// `unknown` when it is not a repository, has no commit, or git is not
/// installed. A tree with no repository still builds.
#[must_use]
pub fn revision_label(directory: &Path) -> String {
    short_revision(directory).unwrap_or_else(|| "unknown".to_string())
}

/// The files whose change moves the revision `directory` reports.
///
/// A commit moves the branch ref and `HEAD`'s reflog; a packed ref instead
/// lands in `packed-refs`. Only paths that exist are reported, because cargo
/// re-runs a build script on every build when a `rerun-if-changed` path is
/// missing. `build.rs` lists them as `rerun-if-changed`, so a commit re-runs
/// the build script and re-embeds the revision.
///
/// Empty outside a repository, where there is no revision to move.
#[must_use]
pub fn watch_paths(directory: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = git_dir(directory) else {
        return Vec::new();
    };
    watch_paths_in(directory, &git_dir)
}

/// The watch paths of a git directory `directory` belongs to.
///
/// Split from `watch_paths` so a test can pass a git directory without
/// `HEAD`, which `git rev-parse` refuses to resolve.
fn watch_paths_in(directory: &Path, git_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let head_path = git_dir.join("HEAD");
    paths.extend(head_path.exists().then_some(head_path));
    let head = fs::read_to_string(git_dir.join("HEAD")).unwrap_or_default();
    for name in ["logs/HEAD", "packed-refs"] {
        paths.extend(git_path(directory, name));
    }
    if let Some(reference) = head.trim().strip_prefix("ref: ") {
        paths.extend(git_path(directory, reference));
    }
    paths
}

/// The repository's git directory, when `directory` is in a working tree.
fn git_dir(directory: &Path) -> Option<PathBuf> {
    let output = git_stdout(directory, &["rev-parse", "--absolute-git-dir"])?;
    Some(PathBuf::from(output.trim()))
}

/// One git path, when it exists.
fn git_path(directory: &Path, name: &str) -> Option<PathBuf> {
    let output = git_stdout(directory, &["rev-parse", "--git-path", name])?;
    let path = resolve(directory, output.trim());
    path.exists().then_some(path)
}

/// The short revision, when git resolves one.
fn short_revision(directory: &Path) -> Option<String> {
    let revision = git_stdout(directory, &["rev-parse", "--short", "HEAD"])?;
    let revision = revision.trim();
    (!revision.is_empty()).then(|| revision.to_string())
}

/// One `git` command's stdout, or `None` when git is missing or fails.
fn git_stdout(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// An absolute path for git's output, which can be relative to `directory`.
fn resolve(directory: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory.join(path)
    }
}

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
        let paths = watch_paths(temp.path());
        assert!(
            paths.contains(&head_file),
            "a checkout moves the git dir's HEAD, so the build must watch it"
        );

        let head = fs::read_to_string(&head_file).expect("read HEAD");
        let reference = head
            .trim()
            .strip_prefix("ref: ")
            .expect("HEAD names a branch after a commit");
        let ref_file = temp.path().join(".git").join(reference);
        assert!(ref_file.exists(), "the commit wrote {}", ref_file.display());
        assert!(
            paths.contains(&ref_file),
            "a commit moves the branch ref, so the build must watch it"
        );
        assert!(
            paths.iter().all(|path| path.exists()),
            "a missing rerun-if-changed path re-runs the build script on every build: {paths:?}"
        );
    }

    #[test]
    fn a_git_dir_without_head_is_not_watched() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let git_dir = temp.path().join("git-dir");
        fs::create_dir(&git_dir).expect("create a fake git dir");

        let paths = super::watch_paths_in(temp.path(), &git_dir);

        assert!(
            !paths.contains(&git_dir.join("HEAD")),
            "HEAD is missing, so watching it would re-run the build script on every build: {paths:?}"
        );
        assert!(
            paths.iter().all(|path| path.exists()),
            "a missing rerun-if-changed path re-runs the build script on every build: {paths:?}"
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
