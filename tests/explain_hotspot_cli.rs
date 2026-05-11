// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;

mod support;

use support::git::{CommitOptions, GitFixture, GitIdentity};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

fn hotpath(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hotpath"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("hotpath binary should run")
}

fn successful_stdout(args: &[&str], current_dir: &Path) -> String {
    let output = hotpath(args, current_dir);

    assert!(
        output.status.success(),
        "hotpath failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("stdout should be UTF-8")
}

fn failed_stderr(args: &[&str], current_dir: &Path) -> String {
    let output = hotpath(args, current_dir);

    assert!(
        !output.status.success(),
        "hotpath unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn explain_reports_score_inputs_formula_contributions_and_persists_inputs() {
    let fixture = GitFixture::new("explain-score");
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");
    let ben = GitIdentity::new("Ben Bitdiddle", "ben@example.invalid");

    fixture.write("src/risky.rs", "one\ntwo\n");
    fixture.write("src/stable.rs", "pub fn stable() {}\n");
    fixture.commit(CommitOptions::new(
        "Add risky and stable files",
        ada,
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/risky.rs", "one\ntwo\nthree\nfour\n");
    fixture.commit(CommitOptions::new(
        "Grow risky file",
        ben,
        "2024-04-10T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["explain", "src/risky.rs"], fixture.path());

    assert!(stdout.starts_with("Hotpath score explanation\npath: src/risky.rs\n"));
    assert!(
        stdout.contains("scope: current scanned file plus local Git history reachable from HEAD")
    );
    assert!(stdout.contains("formula version: hotpath.score.v1 (major 1, minor 0)"));
    assert!(stdout.contains("final score:"));
    assert!(stdout.contains("\nraw metrics\n"));
    assert!(stdout.contains("  line count: 4"));
    assert!(stdout.contains("  commits per file: 2"));
    assert!(stdout.contains("  total churn lines: 4"));
    assert!(stdout.contains("  recent churn lines (90 days): 2"));
    assert!(stdout.contains("  author count: 2"));
    assert!(stdout.contains("  dominant owner share: 50.00%"));
    assert!(stdout.contains("  co-changed file count: 1"));
    assert!(stdout.contains("\nnormalized metrics\n"));
    assert!(stdout.contains("  size:"));
    assert!(stdout.contains("  churn:"));
    assert!(stdout.contains("  recent_churn:"));
    assert!(stdout.contains("  ownership:"));
    assert!(stdout.contains("  coupling:"));
    assert!(stdout.contains("\nweighted contributions\n"));
    assert!(stdout.contains("churn_score: weight 0.350 * churn"));
    assert!(stdout.contains("size_score: weight 0.200 * size"));
    assert!(stdout.contains("author_fragmentation: weight 0.200 * ownership"));
    assert!(stdout.contains("recent_growth: weight 0.150 * recent_churn"));
    assert!(stdout.contains("cochange_score: weight 0.100 * coupling"));
    assert!(stdout.contains("\nlimitations\n"));
    assert!(stdout.contains("Scores are advisory signals for investigation"));
    assert!(!contains_path(&stdout, fixture.path()));

    let persisted = IndexStore::open(fixture.path())
        .expect("index should open")
        .latest_scan()
        .expect("scan should read")
        .expect("scan should exist");
    assert_eq!(
        persisted
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/risky.rs", "src/stable.rs"]
    );

    let persisted_git = IndexStore::open(fixture.path())
        .expect("index should reopen")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(persisted_git.run.head_commit_time, 1712707200);
    assert_eq!(persisted_git.run.recent_window_days, 90);
    assert_eq!(
        persisted_git
            .file_stats
            .iter()
            .map(|stats| (stats.path.as_str(), stats.commits_per_file))
            .collect::<Vec<_>>(),
        vec![("src/risky.rs", 2), ("src/stable.rs", 1)]
    );
}

#[test]
fn explain_reports_zero_git_metrics_for_untracked_existing_file() {
    let fixture = GitFixture::new("explain-untracked");
    let author = GitIdentity::new("Tracked Author", "tracked@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add tracked file",
        author,
        "2024-02-01T00:00:00 +0000",
    ));
    fixture.write("notes/todo.md", "- local note\n");

    let stdout = successful_stdout(&["explain", "notes/todo.md"], fixture.path());

    assert!(stdout.starts_with("Hotpath score explanation\npath: notes/todo.md\n"));
    assert!(stdout.contains("  commits per file: 0"));
    assert!(stdout.contains("  total churn lines: 0"));
    assert!(stdout.contains("  recent churn lines (90 days): 0"));
    assert!(stdout.contains("  author count: 0"));
    assert!(stdout.contains("  dominant owner share: unavailable"));
    assert!(stdout.contains("  co-changed file count: 0"));
    assert!(stdout.contains(
        "dominant owner share is unavailable; author fragmentation uses author count only"
    ));
    assert!(!contains_path(&stdout, fixture.path()));

    let persisted = IndexStore::open(fixture.path())
        .expect("index should open")
        .latest_scan()
        .expect("scan should read")
        .expect("scan should exist");
    assert_eq!(
        persisted
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["notes/todo.md", "src/lib.rs"]
    );

    let persisted_git = IndexStore::open(fixture.path())
        .expect("index should reopen")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(
        persisted_git
            .file_stats
            .iter()
            .map(|stats| stats.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
}

#[test]
fn explain_accepts_cwd_relative_and_repository_relative_paths_from_subdirectories() {
    let fixture = GitFixture::new("explain-path-normalization");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("README.md", "# fixture\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add fixture files",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let src_dir = fixture.path().join("src");
    let cwd_relative = successful_stdout(&["explain", "lib.rs"], &src_dir);
    let repo_separator_path = PathBuf::from("src").join("lib.rs");
    let repo_separator_path = repo_separator_path
        .to_str()
        .expect("test path should be valid UTF-8");
    let repo_relative = successful_stdout(&["explain", repo_separator_path], &src_dir);

    assert!(cwd_relative.starts_with("Hotpath score explanation\npath: src/lib.rs\n"));
    assert!(repo_relative.starts_with("Hotpath score explanation\npath: src/lib.rs\n"));
    assert!(cwd_relative.contains("  commits per file: 1"));
    assert!(repo_relative.contains("  commits per file: 1"));
    assert!(!contains_path(&cwd_relative, fixture.path()));
    assert!(!contains_path(&repo_relative, fixture.path()));
}

#[test]
fn explain_rejects_ambiguous_subdirectory_paths_without_persisting() {
    let fixture = GitFixture::new("explain-ambiguous-path");
    let author = GitIdentity::new("Ambiguous Author", "ambiguous@example.invalid");

    fixture.write("lib.rs", "pub fn root() {}\n");
    fixture.write("src/lib.rs", "pub fn nested() {}\n");
    fixture.commit(CommitOptions::new(
        "Add ambiguous files",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let stderr = failed_stderr(&["explain", "lib.rs"], &fixture.path().join("src"));

    assert!(stderr.starts_with("hotpath: requested path is ambiguous inside this worktree"));
    assert!(stderr.contains("'lib.rs'"));
    assert!(stderr.contains("'src/lib.rs'"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_rejects_paths_outside_worktree_without_path_leaks() {
    let fixture = GitFixture::new("explain-outside-path");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let stderr = failed_stderr(&["explain", ".."], fixture.path());

    assert!(stderr.starts_with("hotpath: requested path is outside the Git worktree"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_rejects_non_current_file_without_output_or_persistence() {
    let fixture = GitFixture::new("explain-not-current-file");
    let author = GitIdentity::new("Delete Author", "delete@example.invalid");

    fixture.write("src/deleted.rs", "pub fn deleted() {}\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add files",
        author.clone(),
        "2024-02-01T00:00:00 +0000",
    ));
    fs::remove_file(fixture.path().join("src").join("deleted.rs"))
        .expect("tracked file should be deleted");
    fixture.commit(CommitOptions::new(
        "Delete old file",
        author,
        "2024-02-02T00:00:00 +0000",
    ));

    let stderr = failed_stderr(&["explain", "src/deleted.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: requested path is not a current scanned file"));
    assert!(stderr.contains("pass an existing file under the worktree"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_rejects_non_git_directory_without_output_or_path_leak() {
    let fixture = TempDir::new("explain-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["explain", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run explain from inside a repository"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_rejects_missing_head_without_output_or_path_leak() {
    let fixture = GitFixture::new("explain-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["explain", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit before explaining hotspot scores"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_rejects_shallow_repository_without_output_or_path_leak() {
    let fixture = GitFixture::new("explain-shallow");
    let author = GitIdentity::new("Shallow Author", "shallow@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    let commit = fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-02-01T00:00:00 +0000",
    ));
    fs::write(
        fixture.path().join(".git").join("shallow"),
        format!("{commit}\n"),
    )
    .expect("shallow marker should be written");

    let stderr = failed_stderr(&["explain", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository has shallow history"));
    assert!(stderr.contains("fetch complete local history"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_sanitizes_persistence_errors_without_fixture_path_leak() {
    let fixture = GitFixture::new("explain-corrupt-index");
    let author = GitIdentity::new("Index Author", "index@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-07-01T00:00:00 +0000",
    ));
    fixture.write(".hotpath/index.db", "not a sqlite database");

    let stderr = failed_stderr(&["explain", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with(
        "hotpath: failed to persist scan results in local Hotpath index (.hotpath/index.db):"
    ));
    assert!(stderr.contains("remove .hotpath/index.db and rerun explain"));
    assert!(!stderr.contains("Hotpath score explanation"));
    assert!(!contains_path(&stderr, fixture.path()));
}

fn contains_path(output: &str, path: &Path) -> bool {
    let path = path.display().to_string();
    let without_verbatim_prefix = path
        .strip_prefix("\\\\?\\")
        .map_or_else(|| path.clone(), ToOwned::to_owned);
    let candidates = [
        path.clone(),
        path.replace('\\', "/"),
        without_verbatim_prefix.clone(),
        without_verbatim_prefix.replace('\\', "/"),
    ];

    candidates
        .iter()
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| output.contains(candidate))
}

#[derive(Debug)]
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::current_dir()
            .expect("test should have a current directory")
            .join("target")
            .join("explain-hotspot-cli-tests")
            .join(format!("{name}-{}-{id}", std::process::id()));

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temporary directory should be created");
        fs::write(path.join(".git"), "not a gitdir\n")
            .expect("invalid git marker should be written");

        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
        let path = self.path.join(relative_path);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("temporary parent should be created");
        }

        fs::write(path, contents).expect("temporary file should be written");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
