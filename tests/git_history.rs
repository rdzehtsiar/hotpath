// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::git::{file_changes_from_head, GitChangeKind, GitFileChange, GitHistoryError};

mod support;

use support::git::{CommitOptions, GitFixture, GitIdentity};

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

type ChangeSummary<'a> = (
    &'a str,
    usize,
    bool,
    &'a str,
    i64,
    &'a str,
    GitChangeKind,
    u64,
    u64,
);

#[test]
fn git_history_reports_root_modify_delete_facts() {
    let fixture = GitFixture::new("linear-history");
    let author = GitIdentity::new("Ada Lovelace", "ada@example.invalid");

    fixture.write("src/lib.rs", "pub fn one() {}\n");
    let root = fixture.commit(CommitOptions::new(
        "Add library",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n");
    let modified = fixture.commit(
        CommitOptions::new(
            "Update library",
            author.clone(),
            "2024-01-02T00:00:00 +0000",
        )
        .committer(GitIdentity::new("Commit Bot", "bot@example.invalid"))
        .committer_date("2024-01-02T01:00:00 +0000"),
    );

    fixture.delete("src/lib.rs");
    let deleted = fixture.commit(CommitOptions::new(
        "Delete library",
        author,
        "2024-01-03T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");

    assert_eq!(
        change_summary(&changes),
        vec![
            (
                root.as_str(),
                0,
                false,
                "Ada Lovelace <ada@example.invalid>",
                1_704_067_200,
                "src/lib.rs",
                GitChangeKind::Added,
                1,
                0,
            ),
            (
                modified.as_str(),
                1,
                false,
                "Ada Lovelace <ada@example.invalid>",
                1_704_157_200,
                "src/lib.rs",
                GitChangeKind::Modified,
                1,
                0,
            ),
            (
                deleted.as_str(),
                1,
                false,
                "Ada Lovelace <ada@example.invalid>",
                1_704_240_000,
                "src/lib.rs",
                GitChangeKind::Deleted,
                0,
                2,
            ),
        ]
    );
}

#[test]
fn git_history_walks_side_branch_commits_and_diffs_merge_against_first_parent() {
    let fixture = GitFixture::new("merge-history");
    let author = GitIdentity::new("Merge Author", "merge@example.invalid");

    fixture.write("base.txt", "base\n");
    fixture.commit(CommitOptions::new(
        "Add base",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.git_stdout(["checkout", "--quiet", "-b", "feature"]);
    fixture.write("side.txt", "side\n");
    let side = fixture.commit(CommitOptions::new(
        "Add side",
        author.clone(),
        "2024-01-02T00:00:00 +0000",
    ));

    fixture.git_stdout(["checkout", "--quiet", "main"]);
    fixture.write("main.txt", "main\n");
    fixture.commit(CommitOptions::new(
        "Add main",
        author.clone(),
        "2024-01-03T00:00:00 +0000",
    ));

    fixture.git_stdout(["merge", "--no-ff", "--no-commit", "feature"]);
    let merge = fixture.commit(CommitOptions::new(
        "Merge feature",
        author,
        "2024-01-04T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let side_changes = changes
        .iter()
        .filter(|change| change.path == "side.txt")
        .collect::<Vec<_>>();

    assert_eq!(side_changes.len(), 2);
    assert_eq!(side_changes[0].commit_id, side);
    assert_eq!(side_changes[0].parent_count, 1);
    assert!(!side_changes[0].is_merge);
    assert_eq!(side_changes[0].change_kind, GitChangeKind::Added);
    assert_eq!(side_changes[0].added_lines, 1);

    assert_eq!(side_changes[1].commit_id, merge);
    assert_eq!(side_changes[1].parent_count, 2);
    assert!(side_changes[1].is_merge);
    assert_eq!(side_changes[1].change_kind, GitChangeKind::Added);
    assert_eq!(side_changes[1].added_lines, 1);
}

#[test]
fn git_history_reports_rename_destination_path_conservatively() {
    let fixture = GitFixture::new("rename-history");
    let author = GitIdentity::new("Rename Author", "rename@example.invalid");

    fixture.write("old.txt", "same\n");
    fixture.commit(CommitOptions::new(
        "Add old path",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));
    fixture.git_stdout(["mv", "old.txt", "new.txt"]);
    let rename = fixture.commit(CommitOptions::new(
        "Rename path",
        author,
        "2024-01-02T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let rename_change = changes
        .iter()
        .find(|change| change.commit_id == rename)
        .expect("rename commit should produce a file change");

    assert_eq!(rename_change.path, "new.txt");
    assert_eq!(rename_change.change_kind, GitChangeKind::Renamed);
    assert_eq!(rename_change.added_lines, 0);
    assert_eq!(rename_change.deleted_lines, 0);
}

#[test]
fn git_history_errors_when_path_is_not_a_git_repository() {
    let fixture = TempDir::new("not-git");

    let error = file_changes_from_head(fixture.path()).expect_err("non-Git path should fail");

    assert!(matches!(error, GitHistoryError::NotRepository { .. }));
    assert!(error.to_string().contains("not a readable Git worktree"));
}

#[test]
fn git_history_errors_when_head_has_no_commit() {
    let fixture = GitFixture::new("missing-head");

    let error = file_changes_from_head(fixture.path()).expect_err("unborn HEAD should fail");

    assert!(matches!(error, GitHistoryError::MissingHead { .. }));
    assert!(error.to_string().contains("does not have a commit at HEAD"));
}

fn change_summary(changes: &[GitFileChange]) -> Vec<ChangeSummary<'_>> {
    changes
        .iter()
        .map(|change| {
            (
                change.commit_id.as_str(),
                change.parent_count,
                change.is_merge,
                change.author.as_str(),
                change.commit_time,
                change.path.as_str(),
                change.change_kind,
                change.added_lines,
                change.deleted_lines,
            )
        })
        .collect()
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
            .join("git-history-tests")
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
