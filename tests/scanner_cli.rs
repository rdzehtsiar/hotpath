// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

mod support;

use support::git::{CommitOptions, GitFixture, GitIdentity};

static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("hotpath-{name}-{}-{id}", std::process::id()));

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture root should be created");

        Self { path }
    }

    fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(path, contents).expect("fixture file should be written");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn hotpath(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hotpath"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("hotpath binary should run")
}

#[test]
fn scan_prints_file_and_git_progress_summary() {
    let fixture = Fixture::new("scan-summary");
    fixture.write("main.go", "package main\n");
    fixture.write("nested/worker.go", "package nested\n");

    let output = hotpath(&["scan"], &fixture.path);

    assert!(
        output.status.success(),
        "hotpath failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.ends_with('\n'));
    let final_lines = final_scan_lines(&stdout);

    assert_eq!(final_lines.len(), 3);
    assert!(final_lines[0].starts_with("files"));
    assert!(final_lines[0].contains("2/2"));
    assert!(!final_lines[0].contains("remaining"));
    assert!(final_lines[0].ends_with(" files/sec"));
    assert!(final_lines[1].starts_with("git"));
    assert!(final_lines[1].contains("1/1"));
    assert!(final_lines[1].contains("commits/sec"));
    assert!(final_lines[2].starts_with("time"));
    assert!(final_lines[2].contains("elapsed"));
    assert!(stdout.contains("summary"));
    assert!(stdout.contains("top_go_hotspots"));
    assert!(stdout.contains("project_coverage Go 100.0%"));
    assert!(stdout.contains("git confidence not_git"));
    assert!(stdout.contains("git: Git analysis skipped"));
    assert!(stdout.contains(&format!("index {}", index_display(&fixture.path))));

    let connection =
        Connection::open(fixture.path.join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "file_analysis"), 2);
}

#[test]
fn scan_summary_reports_no_go_coverage_and_limitation() {
    let fixture = Fixture::new("scan-no-go-summary");
    fixture.write("README.md", "hello\n");

    let output = hotpath(&["scan"], &fixture.path);

    assert!(
        output.status.success(),
        "hotpath failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("top_go_hotspots none"));
    assert!(stdout.contains(
        "project_coverage Go 0.0%  scored 0/0 Go files  active_files 1  confidence none"
    ));
    assert!(stdout.contains("limitations no_scored_files: No Go file risk scores are available."));
    assert!(stdout.contains(&format!("index {}", index_display(&fixture.path))));
}

#[test]
fn scan_tags_go_test_files_in_index_facts_and_risk_rows() {
    let fixture = Fixture::new("scan-go-test-files");
    fixture.write("service.go", "package main\n\nfunc Service() {}\n");
    fixture.write("service_test.go", "package main\n\nfunc TestService() {}\n");

    let output = hotpath(&["scan"], &fixture.path);

    assert!(
        output.status.success(),
        "hotpath failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let connection =
        Connection::open(fixture.path.join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT is_test FROM file_analysis WHERE relative_path = 'service_test.go'",
        ),
        1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT is_test FROM file_facts WHERE relative_path = 'service_test.go'",
        ),
        1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT is_test FROM file_risk_scores WHERE relative_path = 'service_test.go'",
        ),
        1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT is_test FROM file_facts WHERE relative_path = 'service.go'",
        ),
        0
    );
}

#[test]
fn scan_respects_ignore_rules_in_file_count() {
    let fixture = GitFixture::new("scan-ignore");
    fixture.write(".gitignore", "ignored.go\n");
    fixture.write("kept.go", "package main\n");
    fixture.write("ignored.go", "package ignored\n");

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let final_lines = final_scan_lines(&stdout);
    assert!(final_lines[0].starts_with("files"));
    assert!(final_lines[0].contains("2/2"));
    assert!(final_lines[1].starts_with("git"));
    assert!(final_lines[2].starts_with("time"));
}

#[test]
fn scan_reports_actionable_non_git_diagnostic() {
    let fixture = Fixture::new("scan-non-git-diagnostic");
    fixture.write("main.go", "package main\n");

    let output = hotpath(&["scan"], &fixture.path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("diagnostic not_git"));
    assert!(stdout.contains(
        "git confidence not_git  mode skipped_not_git  collection unavailable  index_action cleared_not_git"
    ));
    assert!(stdout.contains("git: Git analysis skipped"));
    assert!(stdout.contains(&format!("index {}", index_display(&fixture.path))));

    let connection =
        Connection::open(fixture.path.join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_diagnostic'",
        ),
        "not_git"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_action'",
        ),
        "cleared_not_git"
    );
    assert!(scalar_text(
        &connection,
        "SELECT value FROM stage_metadata WHERE key = 'git_diagnostic_message'",
    )
    .contains("not a Git worktree"));
}

#[test]
fn scan_reports_actionable_empty_git_repository_diagnostic() {
    let fixture = GitFixture::new("scan-empty-git-diagnostic");
    fixture.write("main.go", "package main\n");

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("diagnostic no_head"));

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_diagnostic'",
        ),
        "no_head"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_action'",
        ),
        "cleared_error"
    );
}

#[test]
fn scan_reports_git_progress_for_git_repository() {
    let fixture = GitFixture::new("scan-git-progress");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("main.go", "package main\n");
    fixture.commit(CommitOptions::new(
        "Add main",
        author.clone(),
        "2025-01-01T00:00:00Z",
    ));
    fixture.write("main.go", "package main\n\nfunc main() {}\n");
    fixture.commit(CommitOptions::new(
        "Update main",
        author,
        "2024-01-02T00:00:00Z",
    ));

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let final_lines = final_scan_lines(&stdout);
    assert!(final_lines[0].contains("1/1"));
    assert!(final_lines[1].starts_with("git"));
    assert!(final_lines[1].contains("2/2"));
    assert!(final_lines[1].contains("commits/sec"));
    assert!(final_lines[2].starts_with("time"));
    assert!(stdout.contains("confidence bounded"));
    assert!(stdout.contains("max_commits 50000"));
    assert!(stdout.contains("max_age_days 730"));
    assert!(stdout.contains("first_parent true"));
    assert!(stdout.contains("renames true"));
    assert!(stdout.contains("cochange_max_files_per_commit 100"));

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "file_analysis"), 1);
    assert_eq!(row_count(&connection, "git_chunks"), 1);
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_confidence'",
        ),
        "bounded"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_first_parent'",
        ),
        "true"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_renames'",
        ),
        "true"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_recent_churn_window_days'",
        ),
        "90"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_action'",
        ),
        "fully_rebuilt"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_recent_churn_reference'",
        ),
        "head_committer_timestamp"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_author_identity_rule'",
        ),
        "exact_author_string_name_email"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_mailmap'",
        ),
        "ignored"
    );
    assert!(scalar_text(
        &connection,
        "SELECT value FROM stage_metadata WHERE key = 'git_ownership_weighting'",
    )
    .contains("bulk_change_dampening"));
}

#[test]
fn scan_bounds_git_history_to_730_days_from_head() {
    let fixture = GitFixture::new("scan-git-old-history-bound");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("legacy.go", "package main\n\nfunc legacy() {}\n");
    fixture.commit(CommitOptions::new(
        "Add legacy file",
        author.clone(),
        "2023-01-01T00:00:00Z",
    ));
    fixture.write("recent.go", "package main\n\nfunc recent() {}\n");
    fixture.commit(CommitOptions::new(
        "Add recent file",
        author.clone(),
        "2025-01-01T00:00:00Z",
    ));
    fixture.write(
        "recent.go",
        "package main\n\nfunc recent() {}\n\nfunc changed() {}\n",
    );
    fixture.commit(CommitOptions::new(
        "Update recent file",
        author,
        "2025-01-02T00:00:00Z",
    ));

    let output = hotpath(&["scan"], fixture.path());

    assert!(
        output.status.success(),
        "hotpath failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("max_age_days 730"));

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_max_age_days'",
        ),
        "730"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_first_parent_commit_count'",
        ),
        "2"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_all_reachable_commit_count'",
        ),
        "2"
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM git_file_metrics WHERE path = 'legacy.go'",
        ),
        0
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT commits_per_file FROM git_file_metrics WHERE path = 'recent.go'",
        ),
        2
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT commits_per_file FROM file_facts WHERE relative_path = 'legacy.go'",
        ),
        0
    );
}

#[test]
fn scan_follows_basic_git_renames_for_file_metrics() {
    let fixture = GitFixture::new("scan-git-rename");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("old.go", "package main\n\nfunc oldName() {}\n");
    fixture.commit(CommitOptions::new(
        "Add old path",
        author.clone(),
        "2024-01-01T00:00:00Z",
    ));
    std::fs::rename(fixture.path().join("old.go"), fixture.path().join("new.go"))
        .expect("fixture file should rename");
    fixture.commit(CommitOptions::new(
        "Rename path",
        author.clone(),
        "2024-01-02T00:00:00Z",
    ));
    fixture.write("new.go", "package main\n\nfunc newName() {}\n");
    fixture.commit(CommitOptions::new(
        "Update new path",
        author,
        "2024-01-03T00:00:00Z",
    ));

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM git_file_metrics WHERE path = 'old.go'",
        ),
        0
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT commits_per_file FROM git_file_metrics WHERE path = 'new.go'",
        ),
        3
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_renames'",
        ),
        "true"
    );
}

#[test]
fn scan_warns_when_first_parent_history_hides_side_branch_work() {
    let fixture = GitFixture::new("scan-merge-heavy");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("base.go", "package main\n");
    fixture.commit(CommitOptions::new(
        "Add base",
        author.clone(),
        "2025-01-01T00:00:00Z",
    ));

    fixture.git_ok(["checkout", "--quiet", "-b", "side"]);
    for index in 0..3 {
        fixture.write(
            format!("side-{index}.go"),
            &format!("package main\n\nfunc side{index}() {{}}\n"),
        );
        fixture.commit(CommitOptions::new(
            &format!("Add side {index}"),
            author.clone(),
            &format!("2025-01-0{}T00:00:00Z", index + 2),
        ));
    }

    fixture.git_ok(["checkout", "--quiet", "main"]);
    fixture.write("main.go", "package main\n\nfunc mainPath() {}\n");
    fixture.commit(CommitOptions::new(
        "Add main path",
        author,
        "2025-01-06T00:00:00Z",
    ));
    fixture.git_ok(["merge", "--quiet", "--no-ff", "side", "-m", "Merge side"]);

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("warning all reachable history is much larger"));

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_first_parent_commit_count'",
        ),
        "3"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_all_reachable_commit_count'",
        ),
        "6"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_merge_commit_count'",
        ),
        "1"
    );
    assert!(scalar_text(
        &connection,
        "SELECT value FROM stage_metadata WHERE key = 'git_merge_heavy_warning'",
    )
    .contains("undercount side-branch work"));
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT commits_per_file FROM git_file_metrics WHERE path = 'side-0.go'",
        ),
        1
    );
}

#[test]
fn broad_commits_skip_cochange_but_keep_churn_and_ownership() {
    let fixture = GitFixture::new("scan-broad-commit");
    let author = GitIdentity::new("Build Bot", "build.bot@example.invalid");
    for index in 0..101 {
        fixture.write(
            format!("file-{index}.go"),
            &format!("package main\n\nfunc file{index}() {{}}\n"),
        );
    }
    fixture.commit(CommitOptions::new(
        "Add broad generated surface",
        author,
        "2025-01-01T00:00:00Z",
    ));

    let output = hotpath(&["scan"], fixture.path());

    assert!(output.status.success());
    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "git_cochanges"), 0);
    assert_eq!(row_count(&connection, "git_file_metrics"), 101);
    assert_eq!(row_count(&connection, "git_file_owners"), 101);
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_broad_commits_skipped_for_cochange'",
        ),
        "1"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_max_touched_files_in_commit'",
        ),
        "101"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_likely_automated_author_count'",
        ),
        "1"
    );
    assert!(scalar_text(
        &connection,
        "SELECT value FROM stage_metadata WHERE key = 'git_broad_commit_warning'",
    )
    .contains("still counted churn and ownership"));
}

#[test]
fn scan_materializes_go_package_risk_scores() {
    let fixture = Fixture::new("scan-package-risk");
    fixture.write("go.mod", "module example.test/hotpath\n");
    fixture.write(
        "cmd/app/main.go",
        r#"package main

import "example.test/hotpath/internal/service"

func main() {
	service.Run(true)
}
"#,
    );
    fixture.write(
        "internal/service/a.go",
        r#"package service

func Run(enabled bool) int {
	if enabled {
		return 1
	}
	return 0
}
"#,
    );
    fixture.write(
        "internal/service/b.go",
        r#"package service

func Stop(enabled bool) int {
	if enabled {
		return 2
	}
	return 0
}
"#,
    );

    let output = hotpath(&["scan"], &fixture.path);

    assert!(
        output.status.success(),
        "hotpath failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let connection =
        Connection::open(fixture.path.join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "package_risk_scores"), 2);
    assert_eq!(row_count(&connection, "package_risk_terms"), 10);
    assert_eq!(row_count(&connection, "package_risk_facts"), 4);
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'package_risk_formula_id'",
        ),
        "hotpath.package_risk.go.v1"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'package_risk_scores_materialized'",
        ),
        "2"
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT file_count FROM package_risk_scores WHERE package_path = 'internal/service'",
        ),
        2
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT source_coupling_in FROM package_risk_scores WHERE package_path = 'internal/service'",
        ),
        2
    );
    assert!(
        scalar_f64(
            &connection,
            "SELECT score FROM package_risk_scores WHERE package_path = 'internal/service'",
        ) > 0.0
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("summary"));
    assert!(stdout.contains("top_go_hotspots"));
    assert!(stdout.contains("cmd/app/main.go"));
    assert!(stdout.contains("internal/service/a.go"));
    assert!(stdout.contains("project_coverage Go 100.0%  scored 3/3 Go files"));
    assert!(stdout.contains("git confidence not_git"));
    assert!(stdout.contains(&format!("index {}", index_display(&fixture.path))));
}

#[test]
fn second_scan_skips_git_when_head_is_unchanged() {
    let fixture = GitFixture::new("scan-git-incremental-skip");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("main.go", "package main\n");
    fixture.commit(CommitOptions::new(
        "Add main",
        author,
        "2024-01-01T00:00:00Z",
    ));

    let first = hotpath(&["scan"], fixture.path());
    assert!(first.status.success());
    let second = hotpath(&["scan"], fixture.path());
    assert!(second.status.success());

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_mode'",
        ),
        "up_to_date"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_scan_mode'",
        ),
        "up_to_date"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_scan_commits_processed'",
        ),
        "0"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_status'",
        ),
        "available"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_indexed_commits'",
        ),
        "1"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_action'",
        ),
        "reused"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM scan_state WHERE key = 'last_scan_completed'",
        ),
        "1"
    );
}

#[test]
fn second_scan_processes_only_new_git_commits_when_head_advances() {
    let fixture = GitFixture::new("scan-git-incremental-range");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("main.go", "package main\n");
    fixture.commit(CommitOptions::new(
        "Add main",
        author.clone(),
        "2024-01-01T00:00:00Z",
    ));

    let first = hotpath(&["scan"], fixture.path());
    assert!(first.status.success());

    fixture.write("main.go", "package main\n\nfunc main() {}\n");
    fixture.commit(CommitOptions::new(
        "Update main",
        author,
        "2024-01-02T00:00:00Z",
    ));
    let second = hotpath(&["scan"], fixture.path());
    assert!(second.status.success());

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_mode'",
        ),
        "incremental"
    );
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT value FROM stage_metadata WHERE key = 'git_index_action'",
        ),
        "incrementally_updated"
    );
    assert_eq!(
        row_count_where(
            &connection,
            "git_chunks",
            "commits_processed = 1 AND chunk_index = 0",
        ),
        1
    );
}

#[test]
fn removed_commands_fail_as_unknown_commands() {
    let fixture = Fixture::new("unknown-command");

    let output = hotpath(&["parse"], &fixture.path);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("unrecognized subcommand 'parse'")
            || stderr.contains("unexpected argument 'parse'")
    );
}

#[test]
fn tui_command_is_recognized_by_clap() {
    let fixture = Fixture::new("tui-help");

    let output = hotpath(&["tui", "--help"], &fixture.path);

    assert!(
        output.status.success(),
        "hotpath tui --help failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("tui"));
}

fn final_scan_lines(stdout: &str) -> Vec<String> {
    let sanitized = strip_ansi(stdout);
    let lines: Vec<_> = sanitized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    let progress_lines: Vec<_> = lines
        .iter()
        .filter(|line| {
            line.starts_with("files")
                || line.starts_with("time")
                || (line.starts_with("git") && line.contains("| speed"))
        })
        .cloned()
        .collect();

    progress_lines[progress_lines.len().saturating_sub(3)..].to_vec()
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for character in chars.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if character == '\r' {
            output.push('\n');
        } else {
            output.push(character);
        }
    }

    output
}

fn row_count(connection: &Connection, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    connection
        .query_row(&sql, [], |row| row.get(0))
        .expect("row count should query")
}

fn row_count_where(connection: &Connection, table: &str, condition: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
    connection
        .query_row(&sql, [], |row| row.get(0))
        .expect("row count should query")
}

fn scalar_text(connection: &Connection, sql: &str) -> String {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("scalar query should run")
}

fn scalar_i64(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("scalar query should run")
}

fn scalar_f64(connection: &Connection, sql: &str) -> f64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("scalar query should run")
}

fn index_display(root: &Path) -> String {
    let index = root.join(".hotpath").join("index.sqlite");
    fs::canonicalize(&index)
        .unwrap_or(index)
        .display()
        .to_string()
}
