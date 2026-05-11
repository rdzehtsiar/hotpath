// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::git::{
    co_changes_from_changes, file_changes_from_head, file_metrics_from_changes, GitChangeKind,
    GitFileChange, GitHistoryError,
};

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

type CoChangeSummary<'a> = (&'a str, &'a str, u64);

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
fn git_file_metrics_report_exact_fixture_values() {
    let fixture = GitFixture::new("file-metrics");
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");
    let ben = GitIdentity::new("Ben Bitdiddle", "ben@example.invalid");
    let cara = GitIdentity::new("Cara Committer", "cara@example.invalid");

    fixture.write("src/lib.rs", "one\ntwo\n");
    let first_src = fixture.commit(CommitOptions::new(
        "Add source",
        ada.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "one\nthree\nfour\n");
    fixture.commit(CommitOptions::new(
        "Rewrite source line",
        ben,
        "2024-01-15T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "one\nthree\nfour\nfive\n");
    let last_src = fixture.commit(CommitOptions::new(
        "Extend source",
        ada,
        "2024-03-01T00:00:00 +0000",
    ));

    fixture.write("README.md", "readme\n");
    let readme = fixture.commit(CommitOptions::new(
        "Add readme",
        cara,
        "2024-04-10T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let metrics = file_metrics_from_changes(&changes, 1_712_707_200);

    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[0].path, "README.md");
    assert_eq!(metrics[0].commits_per_file, 1);
    assert_eq!(metrics[0].total_churn_added, 1);
    assert_eq!(metrics[0].total_churn_deleted, 0);
    assert_eq!(metrics[0].recent_churn_added, 1);
    assert_eq!(metrics[0].recent_churn_deleted, 0);
    assert_eq!(metrics[0].author_count, 1);
    assert_eq!(
        metrics[0].dominant_owner.as_deref(),
        Some("Cara Committer <cara@example.invalid>")
    );
    assert_eq!(metrics[0].dominant_owner_share, Some(1.0));
    assert_eq!(metrics[0].first_commit_id.as_deref(), Some(readme.as_str()));
    assert_eq!(metrics[0].first_commit_time, Some(1_712_707_200));
    assert_eq!(metrics[0].last_commit_id.as_deref(), Some(readme.as_str()));
    assert_eq!(metrics[0].last_commit_time, Some(1_712_707_200));
    assert_eq!(metrics[0].file_age_days, Some(0));

    assert_eq!(metrics[1].path, "src/lib.rs");
    assert_eq!(metrics[1].commits_per_file, 3);
    assert_eq!(metrics[1].total_churn_added, 5);
    assert_eq!(metrics[1].total_churn_deleted, 1);
    assert_eq!(metrics[1].recent_churn_added, 3);
    assert_eq!(metrics[1].recent_churn_deleted, 1);
    assert_eq!(metrics[1].author_count, 2);
    assert_eq!(
        metrics[1].dominant_owner.as_deref(),
        Some("Ada Lovelace <ada@example.invalid>")
    );
    assert_eq!(metrics[1].dominant_owner_share, Some(2.0 / 3.0));
    assert_eq!(
        metrics[1].first_commit_id.as_deref(),
        Some(first_src.as_str())
    );
    assert_eq!(metrics[1].first_commit_time, Some(1_704_067_200));
    assert_eq!(
        metrics[1].last_commit_id.as_deref(),
        Some(last_src.as_str())
    );
    assert_eq!(metrics[1].last_commit_time, Some(1_709_251_200));
    assert_eq!(metrics[1].file_age_days, Some(100));
}

#[test]
fn git_file_metrics_break_dominant_owner_ties_by_author_identity() {
    let changes = vec![
        raw_change(
            "b",
            "Zed Zed <zed@example.invalid>",
            200,
            "src/lib.rs",
            1,
            0,
        ),
        raw_change(
            "a",
            "Ada Ada <ada@example.invalid>",
            100,
            "src/lib.rs",
            1,
            0,
        ),
    ];

    let metrics = file_metrics_from_changes(&changes, 200);

    assert_eq!(metrics.len(), 1);
    assert_eq!(
        metrics[0].dominant_owner.as_deref(),
        Some("Ada Ada <ada@example.invalid>")
    );
    assert_eq!(metrics[0].dominant_owner_share, Some(0.5));
}

#[test]
fn git_file_metrics_clamp_age_when_commit_time_is_after_head_time() {
    let head_time = 1_700_000_000;
    let future_time = head_time + 86_400;
    let changes = vec![raw_change(
        "future",
        "Skewed Author <skew@example.invalid>",
        future_time,
        "future.txt",
        3,
        1,
    )];

    let metrics = file_metrics_from_changes(&changes, head_time);

    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].first_commit_time, Some(future_time));
    assert_eq!(metrics[0].last_commit_time, Some(future_time));
    assert_eq!(metrics[0].file_age_days, Some(0));
    assert_eq!(metrics[0].recent_churn_added, 3);
    assert_eq!(metrics[0].recent_churn_deleted, 1);
}

#[test]
fn git_co_changes_report_pair_counts_from_fixture_history() {
    let fixture = GitFixture::new("co-change-pair-counts");
    let author = GitIdentity::new("Pair Author", "pair@example.invalid");

    fixture.write("a.txt", "a1\n");
    fixture.write("b.txt", "b1\n");
    fixture.commit(CommitOptions::new(
        "Touch a and b",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("b.txt", "b1\nb2\n");
    fixture.write("c.txt", "c1\n");
    fixture.commit(CommitOptions::new(
        "Touch b and c",
        author.clone(),
        "2024-01-02T00:00:00 +0000",
    ));

    fixture.write("a.txt", "a1\na2\n");
    fixture.write("b.txt", "b1\nb2\nb3\n");
    fixture.write("c.txt", "c1\nc2\n");
    fixture.commit(CommitOptions::new(
        "Touch a b and c",
        author,
        "2024-01-03T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let co_changes = co_changes_from_changes(&changes);

    assert_eq!(
        co_change_summary(&co_changes),
        vec![
            ("a.txt", "b.txt", 2),
            ("b.txt", "c.txt", 2),
            ("a.txt", "c.txt", 1),
        ]
    );
}

#[test]
fn git_co_changes_ignore_single_file_commits() {
    let fixture = GitFixture::new("co-change-single-file");
    let author = GitIdentity::new("Single Author", "single@example.invalid");

    fixture.write("a.txt", "a1\n");
    fixture.commit(CommitOptions::new(
        "Touch only a",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("b.txt", "b1\n");
    fixture.commit(CommitOptions::new(
        "Touch only b",
        author,
        "2024-01-02T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let co_changes = co_changes_from_changes(&changes);

    assert!(co_changes.is_empty());
}

#[test]
fn git_co_changes_count_duplicate_path_events_once_per_commit() {
    let fixture = GitFixture::new("co-change-duplicate-path");
    let author = GitIdentity::new("Duplicate Author", "duplicate@example.invalid");

    fixture.write("a.txt", "a1\n");
    fixture.write("b.txt", "b1\n");
    let commit = fixture.commit(CommitOptions::new(
        "Touch a and b",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let mut changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let duplicate = changes
        .iter()
        .find(|change| change.commit_id == commit && change.path == "a.txt")
        .expect("fixture should include a.txt in the commit")
        .clone();
    changes.push(duplicate);

    let co_changes = co_changes_from_changes(&changes);

    assert_eq!(co_change_summary(&co_changes), vec![("a.txt", "b.txt", 1)]);
}

#[test]
fn git_co_changes_are_deterministically_ranked() {
    let fixture = GitFixture::new("co-change-ordering");
    let author = GitIdentity::new("Order Author", "order@example.invalid");

    fixture.write("y.txt", "y1\n");
    fixture.write("z.txt", "z1\n");
    fixture.commit(CommitOptions::new(
        "Touch y and z",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("a.txt", "a1\n");
    fixture.write("z.txt", "z1\nz2\n");
    fixture.commit(CommitOptions::new(
        "Touch a and z",
        author.clone(),
        "2024-01-02T00:00:00 +0000",
    ));

    fixture.write("a.txt", "a1\na2\n");
    fixture.write("b.txt", "b1\n");
    fixture.commit(CommitOptions::new(
        "Touch a and b",
        author,
        "2024-01-03T00:00:00 +0000",
    ));

    let changes = file_changes_from_head(fixture.path()).expect("history should be readable");
    let co_changes = co_changes_from_changes(&changes);

    assert_eq!(
        co_change_summary(&co_changes),
        vec![
            ("a.txt", "b.txt", 1),
            ("a.txt", "z.txt", 1),
            ("y.txt", "z.txt", 1),
        ]
    );
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

fn co_change_summary(co_changes: &[hotpath::git::GitCoChange]) -> Vec<CoChangeSummary<'_>> {
    co_changes
        .iter()
        .map(|co_change| {
            (
                co_change.left_path.as_str(),
                co_change.right_path.as_str(),
                co_change.commit_count,
            )
        })
        .collect()
}

fn raw_change(
    commit_id: &str,
    author: &str,
    commit_time: i64,
    path: &str,
    added_lines: u64,
    deleted_lines: u64,
) -> GitFileChange {
    GitFileChange {
        commit_id: commit_id.to_owned(),
        parent_count: 1,
        is_merge: false,
        author: author.to_owned(),
        commit_time,
        path: path.to_owned(),
        change_kind: GitChangeKind::Modified,
        added_lines,
        deleted_lines,
    }
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
