// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

mod support;

use support::git::{CommitOptions, GitFixture, GitIdentity};

#[test]
fn git_fixture_creates_reproducible_commits_with_fixed_identities_and_dates() {
    let first = author_history("authors-a");
    let second = author_history("authors-b");

    assert_eq!(first.commit_ids, second.commit_ids);
    assert_eq!(first.audit_log, second.audit_log);
    assert_eq!(
        first.audit_log,
        concat!(
            "Ada Lovelace <ada@example.invalid>|Ada Lovelace <ada@example.invalid>|",
            "2024-01-01T00:00:00+00:00|2024-01-01T00:00:00+00:00|Add library\n",
            "Grace Hopper <grace@example.invalid>|Release Bot <release@example.invalid>|",
            "2024-01-02T03:04:05+00:00|2024-01-02T04:04:05+00:00|Update library\n",
        )
    );
}

#[test]
fn git_fixture_records_delete_commits_with_repository_relative_paths() {
    let fixture = GitFixture::new("delete-history");
    let author = GitIdentity::new("Delete Author", "delete@example.invalid");

    fixture.write("src/keep.rs", "pub fn keep() {}\n");
    fixture.write("src/remove.rs", "pub fn remove() {}\n");
    fixture.commit(CommitOptions::new(
        "Add files",
        author.clone(),
        "2024-02-01T00:00:00 +0000",
    ));
    fixture.delete("src/remove.rs");
    let delete_commit = fixture.commit(CommitOptions::new(
        "Delete removed file",
        author,
        "2024-02-02T00:00:00 +0000",
    ));

    let name_status = fixture.git_stdout([
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-status",
        "-r",
        delete_commit.as_str(),
    ]);
    let log_with_paths = fixture.git_stdout(["log", "--reverse", "--name-status", "--format=%s"]);

    assert_eq!(name_status, "D\tsrc/remove.rs\n");
    assert!(!fixture.path().join("src/remove.rs").exists());
    assert!(fixture.path().join("src/keep.rs").is_file());
    assert!(!contains_fixture_path(&log_with_paths, fixture.path()));
}

#[test]
fn git_fixture_creates_co_change_shaped_commits() {
    let fixture = GitFixture::new("co-change-history");
    let author = GitIdentity::new("Pair Author", "pair@example.invalid");

    fixture.write("src/a.rs", "pub fn a() {}\n");
    fixture.write("src/b.rs", "pub fn b() {}\n");
    let ab_commit = fixture.commit(CommitOptions::new(
        "Touch a and b",
        author.clone(),
        "2024-03-01T00:00:00 +0000",
    ));

    fixture.write("src/b.rs", "pub fn b() {}\npub fn b2() {}\n");
    fixture.write("src/c.rs", "pub fn c() {}\n");
    let bc_commit = fixture.commit(CommitOptions::new(
        "Touch b and c",
        author.clone(),
        "2024-03-02T00:00:00 +0000",
    ));

    fixture.write("src/a.rs", "pub fn a() {}\npub fn a2() {}\n");
    fixture.write("src/c.rs", "pub fn c() {}\npub fn c2() {}\n");
    let ac_commit = fixture.commit(CommitOptions::new(
        "Touch a and c",
        author,
        "2024-03-03T00:00:00 +0000",
    ));

    assert_eq!(
        changed_paths(&fixture, &ab_commit),
        vec!["src/a.rs", "src/b.rs"]
    );
    assert_eq!(
        changed_paths(&fixture, &bc_commit),
        vec!["src/b.rs", "src/c.rs"]
    );
    assert_eq!(
        changed_paths(&fixture, &ac_commit),
        vec!["src/a.rs", "src/c.rs"]
    );
    assert_eq!(
        fixture.git_stdout(["status", "--porcelain=v1"]),
        "",
        "fixture repository should be clean after commits"
    );
}

struct AuthorHistory {
    commit_ids: String,
    audit_log: String,
}

fn author_history(name: &str) -> AuthorHistory {
    let fixture = GitFixture::new(name);

    fixture.write("src/lib.rs", "pub fn first() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        GitIdentity::new("Ada Lovelace", "ada@example.invalid"),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/lib.rs", "pub fn first() {}\npub fn second() {}\n");
    fixture.commit(
        CommitOptions::new(
            "Update library",
            GitIdentity::new("Grace Hopper", "grace@example.invalid"),
            "2024-01-02T03:04:05 +0000",
        )
        .committer(GitIdentity::new("Release Bot", "release@example.invalid"))
        .committer_date("2024-01-02T04:04:05 +0000"),
    );

    AuthorHistory {
        commit_ids: fixture.git_stdout(["rev-list", "--reverse", "HEAD"]),
        audit_log: fixture.git_stdout([
            "log",
            "--reverse",
            "--format=%an <%ae>|%cn <%ce>|%aI|%cI|%s",
        ]),
    }
}

fn changed_paths(fixture: &GitFixture, commit: &str) -> Vec<String> {
    fixture
        .git_stdout([
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit,
        ])
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn contains_fixture_path(output: &str, path: &Path) -> bool {
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
        .any(|candidate| output.contains(candidate))
}
