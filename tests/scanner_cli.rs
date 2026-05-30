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

    let connection =
        Connection::open(fixture.path.join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "file_analysis"), 2);
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
fn scan_reports_git_progress_for_git_repository() {
    let fixture = GitFixture::new("scan-git-progress");
    let author = GitIdentity::new("Hotpath Test", "hotpath.test@example.invalid");
    fixture.write("main.go", "package main\n");
    fixture.commit(CommitOptions::new(
        "Add main",
        author.clone(),
        "2024-01-01T00:00:00Z",
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

    let connection =
        Connection::open(fixture.path().join(".hotpath").join("index.sqlite")).expect("db opens");
    assert_eq!(row_count(&connection, "file_analysis"), 1);
    assert_eq!(row_count(&connection, "git_chunks"), 1);
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

    lines[lines.len().saturating_sub(3)..].to_vec()
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
