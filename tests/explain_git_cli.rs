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
fn explain_git_reports_file_metrics_and_ranked_co_changes() {
    let fixture = GitFixture::new("explain-git-metrics");
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");
    let ben = GitIdentity::new("Ben Bitdiddle", "ben@example.invalid");

    fixture.write("src/lib.rs", "one\ntwo\n");
    fixture.write("src/alpha.rs", "alpha\n");
    fixture.commit(CommitOptions::new(
        "Add library and alpha",
        ada.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "one\nthree\nfour\n");
    fixture.write("src/beta.rs", "beta\n");
    fixture.commit(CommitOptions::new(
        "Update library and beta",
        ben,
        "2024-01-15T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "one\nthree\nfour\nfive\n");
    fixture.write("src/alpha.rs", "alpha\nalpha2\n");
    fixture.write("src/beta.rs", "beta\nbeta2\n");
    fixture.commit(CommitOptions::new(
        "Extend library with related files",
        ada,
        "2024-04-10T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["explain-git", "src/lib.rs"], fixture.path());

    assert!(stdout.starts_with("Hotpath Git explanation\npath: src/lib.rs\n"));
    assert!(stdout.contains("history scope: local commits reachable from HEAD"));
    assert!(stdout.contains("HEAD committer timestamp: 1712707200 (Unix seconds)"));
    assert!(stdout.contains("\nraw changes\n"));
    assert!(stdout.contains("added  +2 -0  Ada Lovelace <ada@example.invalid>"));
    assert!(stdout.contains("modified  +2 -1  Ben Bitdiddle <ben@example.invalid>"));
    assert!(stdout.contains("modified  +1 -0  Ada Lovelace <ada@example.invalid>"));
    assert!(stdout.contains("\nraw metrics\n"));
    assert!(stdout.contains("  commits per file: 3"));
    assert!(stdout.contains("  total churn: 5 added, 1 deleted, 6 combined"));
    assert!(stdout.contains("  recent churn (90 days): 3 added, 1 deleted, 4 combined"));
    assert!(stdout.contains("  author count: 2"));
    assert!(stdout.contains(
        "  dominant owner: Ada Lovelace <ada@example.invalid> (66.67% of file-touching commits)"
    ));
    assert!(stdout.contains("  file age: 100 days"));
    assert!(stdout.contains("\nco-changes\n  2  src/alpha.rs\n  2  src/beta.rs"));
    assert!(stdout.contains("\ncalculation notes\n"));
    assert!(
        stdout.contains("Recent churn uses the 90-day window before the HEAD committer timestamp.")
    );
    assert!(stdout.contains("\nlimitations\n"));
    assert!(stdout.contains(
        "Results are advisory and should be treated as local derived cache data when persisted."
    ));
    assert!(!contains_path(&stdout, fixture.path()));

    let persisted = IndexStore::open(fixture.path())
        .expect("index should open")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(persisted.run.git_head.len(), 40);
    assert_eq!(persisted.run.head_commit_time, 1712707200);
    assert_eq!(persisted.run.recent_window_days, 90);
    assert_eq!(persisted.run.metrics_observed, 3);
    assert_eq!(persisted.run.co_changes_observed, 3);
    assert_eq!(
        persisted
            .file_stats
            .iter()
            .map(|stats| (stats.path.as_str(), stats.commits_per_file))
            .collect::<Vec<_>>(),
        vec![("src/alpha.rs", 2), ("src/beta.rs", 2), ("src/lib.rs", 3)]
    );
    assert_eq!(
        persisted
            .co_changes
            .iter()
            .map(|co_change| {
                (
                    co_change.left_path.as_str(),
                    co_change.right_path.as_str(),
                    co_change.commit_count,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("src/alpha.rs", "src/lib.rs", 2),
            ("src/beta.rs", "src/lib.rs", 2),
            ("src/alpha.rs", "src/beta.rs", 1),
        ]
    );
    assert!(!persisted_git_analysis_contains_path(
        &persisted,
        fixture.path()
    ));

    fixture.write("src/lib.rs", "one\nthree\nfour\nfive\nsix\n");
    let latest_commit = fixture.commit(CommitOptions::new(
        "Update library alone",
        GitIdentity::new("Cara Compiler", "cara@example.invalid"),
        "2024-04-20T00:00:00 +0000",
    ));

    successful_stdout(&["explain-git", "src/lib.rs"], fixture.path());

    let updated = IndexStore::open(fixture.path())
        .expect("index should reopen")
        .latest_git_analysis()
        .expect("updated Git analysis should read")
        .expect("updated Git analysis should exist");
    let lib_stats = updated
        .file_stats
        .iter()
        .find(|stats| stats.path == "src/lib.rs")
        .expect("library stats should persist");

    assert_eq!(updated.run.git_head, latest_commit);
    assert_eq!(updated.run.head_commit_time, 1713571200);
    assert_eq!(updated.run.metrics_observed, 3);
    assert_eq!(updated.run.co_changes_observed, 3);
    assert_eq!(lib_stats.commits_per_file, 4);
    assert_eq!(
        lib_stats.recent_churn_added + lib_stats.recent_churn_deleted,
        2
    );
    assert!(!persisted_git_analysis_contains_path(
        &updated,
        fixture.path()
    ));
}

#[test]
fn explain_git_accepts_cwd_relative_and_repository_relative_paths_from_subdirectories() {
    let fixture = GitFixture::new("explain-git-path-normalization");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let src_dir = fixture.path().join("src");
    let cwd_relative = successful_stdout(&["explain-git", "lib.rs"], &src_dir);
    let repo_separator_path = PathBuf::from("src").join("lib.rs");
    let repo_separator_path = repo_separator_path
        .to_str()
        .expect("test path should be valid UTF-8");
    let repo_relative = successful_stdout(&["explain-git", repo_separator_path], &src_dir);

    assert!(cwd_relative.starts_with("Hotpath Git explanation\npath: src/lib.rs\n"));
    assert!(repo_relative.starts_with("Hotpath Git explanation\npath: src/lib.rs\n"));
    assert!(cwd_relative.contains("  commits per file: 1"));
    assert!(repo_relative.contains("  commits per file: 1"));
    assert!(!contains_path(&cwd_relative, fixture.path()));
    assert!(!contains_path(&repo_relative, fixture.path()));
}

#[test]
fn explain_git_rejects_ambiguous_subdirectory_paths_without_persisting() {
    let fixture = GitFixture::new("explain-git-ambiguous-path");
    let author = GitIdentity::new("Ambiguous Author", "ambiguous@example.invalid");

    fixture.write("lib.rs", "pub fn root() {}\n");
    fixture.write("src/lib.rs", "pub fn nested() {}\n");
    fixture.commit(CommitOptions::new(
        "Add ambiguous library paths",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let stderr = failed_stderr(&["explain-git", "lib.rs"], &fixture.path().join("src"));

    assert!(stderr.starts_with("hotpath: requested path is ambiguous inside this worktree"));
    assert!(stderr.contains("'lib.rs'"));
    assert!(stderr.contains("'src/lib.rs'"));
    assert!(!stderr.contains("raw metrics"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_git_reports_zero_metrics_for_existing_file_without_history() {
    let fixture = GitFixture::new("explain-git-untracked-file");
    let author = GitIdentity::new("Tracked Author", "tracked@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add tracked source",
        author,
        "2024-02-01T00:00:00 +0000",
    ));
    fixture.write("notes/todo.md", "- local note\n");

    let stdout = successful_stdout(&["explain-git", "notes/todo.md"], fixture.path());

    assert!(stdout.starts_with("Hotpath Git explanation\npath: notes/todo.md\n"));
    assert!(stdout.contains("\nraw changes\n  none\n"));
    assert!(stdout.contains("  commits per file: 0"));
    assert!(stdout.contains("  total churn: 0 added, 0 deleted, 0 combined"));
    assert!(stdout.contains("  recent churn (90 days): 0 added, 0 deleted, 0 combined"));
    assert!(stdout.contains("  author count: 0"));
    assert!(stdout.contains("  dominant owner: unavailable"));
    assert!(stdout.contains("  first observed commit: unavailable"));
    assert!(stdout.contains("  last observed commit: unavailable"));
    assert!(stdout.contains("  file age: unavailable"));
    assert!(stdout.contains("\nco-changes\n  none\n"));
    assert!(!contains_path(&stdout, fixture.path()));

    let persisted = IndexStore::open(fixture.path())
        .expect("index should open")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");

    assert_eq!(
        persisted
            .file_stats
            .iter()
            .map(|stats| stats.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
}

#[test]
fn explain_git_rejects_paths_outside_worktree_without_leaking_absolute_paths() {
    let fixture = GitFixture::new("explain-git-outside-path");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let output = hotpath(&["explain-git", ".."], fixture.path());

    assert!(
        !output.status.success(),
        "explain-git unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");

    assert!(stderr.starts_with("hotpath: requested path is outside the Git worktree"));
    assert!(!contains_path(&stderr, fixture.path()));
}

#[test]
fn explain_git_rejects_non_git_directory_without_metric_output_or_path_leak() {
    let fixture = TempDir::new("explain-git-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["explain-git", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(!stderr.contains("raw metrics"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_git_rejects_missing_head_without_metric_output_or_path_leak() {
    let fixture = GitFixture::new("explain-git-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["explain-git", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit before analyzing history"));
    assert!(!stderr.contains("raw metrics"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn explain_git_rejects_shallow_repository_before_output_or_persistence() {
    let fixture = GitFixture::new("explain-git-shallow");
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

    let stderr = failed_stderr(&["explain-git", "src/lib.rs"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository has shallow history"));
    assert!(stderr.contains("fetch complete local history"));
    assert!(!stderr.contains("Hotpath Git explanation"));
    assert!(!stderr.contains("raw metrics"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
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
            .join("explain-git-cli-tests")
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

fn persisted_git_analysis_contains_path(
    analysis: &hotpath::storage::index::PersistedGitAnalysis,
    path: &Path,
) -> bool {
    let strings =
        analysis
            .file_stats
            .iter()
            .flat_map(|stats| {
                [
                    stats.path.as_str(),
                    stats.dominant_owner.as_deref().unwrap_or_default(),
                    stats.first_commit_id.as_deref().unwrap_or_default(),
                    stats.last_commit_id.as_deref().unwrap_or_default(),
                ]
            })
            .chain(analysis.co_changes.iter().flat_map(|co_change| {
                [co_change.left_path.as_str(), co_change.right_path.as_str()]
            }))
            .collect::<Vec<_>>();

    strings.iter().any(|value| contains_path(value, path))
}
