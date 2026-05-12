// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;
use serde_json::Value;

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

fn report_json(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["report", "--json"], current_dir);
    let value = serde_json::from_str(&stdout).expect("report JSON should parse");

    (stdout, value)
}

fn report_markdown(current_dir: &Path) -> String {
    successful_stdout(&["report", "--markdown"], current_dir)
}

#[test]
fn report_json_is_deterministic_and_has_expected_schema() {
    let fixture = ranked_fixture("report-json-schema");

    let (first_stdout, value) = report_json(fixture.path());
    let (second_stdout, second_value) = report_json(fixture.path());

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(value, second_value);
    assert!(first_stdout.starts_with("{\n  \"schema_version\": \"hotpath.report.v1\","));
    assert_eq!(value["schema_version"], "hotpath.report.v1");
    assert_eq!(value["summary"]["scan"]["total_files"], 3);
    assert_eq!(value["summary"]["git"]["recent_window_days"], 90);
    assert_eq!(value["summary"]["git"]["file_metric_count"], 3);
    assert_eq!(value["summary"]["hotspot_count"], 3);
    assert_eq!(
        value["summary"]["context_estimated_tokens"],
        value["context"]["summary"]["estimated_tokens"]
    );
    assert_eq!(value["context"]["options"]["exclude_generated"], false);
    assert_eq!(value["context"]["options"]["exclude_vendor"], false);
    assert!(value["context"]["budget"].is_null());
    assert_eq!(
        value["hotspots"].as_array().expect("hotspots array").len(),
        3
    );
    assert_eq!(
        value["findings"].as_array().expect("findings array").len(),
        3
    );
    assert!(!contains_path(&first_stdout, fixture.path()));
}

#[test]
fn report_defaults_to_deterministic_markdown_without_path_leaks() {
    let fixture = ranked_fixture("report-markdown-default");

    let default_stdout = successful_stdout(&["report"], fixture.path());
    let explicit_stdout = report_markdown(fixture.path());
    let repeated_stdout = successful_stdout(&["report"], fixture.path());

    assert_eq!(default_stdout, explicit_stdout);
    assert_eq!(default_stdout, repeated_stdout);
    assert!(default_stdout.starts_with("# Hotpath Report\n\n"));
    assert!(default_stdout.contains("## Summary"));
    assert!(default_stdout.contains("- Files scanned: 3"));
    assert!(default_stdout.contains("- Hotspots ranked: 3"));
    assert!(default_stdout.contains("## Top Hotspots"));
    assert!(default_stdout.contains("| 1 | `src/risky.rs` |"));
    assert!(default_stdout.contains("Risk /10"));
    assert!(default_stdout.contains("## Calculation Notes"));
    assert!(default_stdout.contains("does not require network access or telemetry"));
    assert!(default_stdout.contains("Scores use the reported formula version"));
    assert!(!default_stdout.contains("schema_version"));
    assert!(!contains_path(&default_stdout, fixture.path()));
}

#[test]
fn report_markdown_limits_human_hotspots_while_json_remains_complete() {
    let fixture = many_hotspots_fixture("report-markdown-limit", 12);

    let markdown = report_markdown(fixture.path());
    let (_json_stdout, value) = report_json(fixture.path());

    assert_eq!(
        value["hotspots"].as_array().expect("hotspots array").len(),
        12
    );
    assert!(markdown.contains("| 10 | `src/file09.rs` |"));
    assert!(!markdown.contains("src/file10.rs"));
    assert!(!markdown.contains("src/file11.rs"));
    assert!(markdown.contains("Showing top 10 of 12 ranked hotspots"));
    assert!(markdown.contains("JSON output includes all hotspot rows"));
    assert!(!contains_path(&markdown, fixture.path()));
}

#[test]
fn report_markdown_reports_empty_hotspot_sets() {
    let fixture = empty_current_file_set_fixture("report-markdown-empty");

    let markdown = report_markdown(fixture.path());

    assert!(markdown.contains("- Files scanned: 0"));
    assert!(markdown.contains("- Hotspots ranked: 0"));
    assert!(markdown.contains("No current files were ranked as hotspots."));
    assert!(markdown.contains("No advisory findings were produced."));
    assert!(!contains_path(&markdown, fixture.path()));
}

#[test]
fn report_markdown_uses_safe_code_spans_for_paths_with_backticks() {
    let fixture = GitFixture::new("report-markdown-backtick-path");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("src/weird`name.rs", "pub fn weird_name() {}\n");
    fixture.commit(CommitOptions::new(
        "Add path with backtick",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let markdown = report_markdown(fixture.path());

    assert!(markdown.contains("| 1 | `` src/weird`name.rs `` |"));
    assert!(!contains_path(&markdown, fixture.path()));
}

#[test]
fn report_rejects_json_and_markdown_flags_together() {
    let fixture = ranked_fixture("report-markdown-conflict");

    let stderr = failed_stderr(&["report", "--json", "--markdown"], fixture.path());

    assert!(stderr.contains("--json"));
    assert!(stderr.contains("--markdown"));
    assert!(stderr.contains("cannot be used"));
}

#[test]
fn report_json_preserves_hotspot_ranking_order_and_score_payloads() {
    let fixture = ranked_fixture("report-json-ranking");
    let (_stdout, value) = report_json(fixture.path());

    assert_eq!(
        hotspot_paths(&value),
        vec!["src/risky.rs", "src/related.rs", "src/stable.rs"]
    );
    assert_eq!(value["hotspots"][0]["rank"], 1);
    assert_eq!(value["hotspots"][0]["path"], "src/risky.rs");
    assert_eq!(
        value["hotspots"][0]["formula_version"]["id"],
        "hotpath.score.v1"
    );
    assert_eq!(value["hotspots"][0]["raw_metrics"]["path"], "src/risky.rs");
    assert_eq!(value["hotspots"][0]["raw_metrics"]["line_count"], 100);
    assert!(value["hotspots"][0]["weighted_terms"]
        .as_array()
        .expect("weighted terms")
        .iter()
        .any(|term| term["name"] == "churn_score"));
    assert_eq!(value["findings"][0]["code"], "hotpath.hotspot.risk");
    assert_eq!(value["findings"][0]["path"], "src/risky.rs");
    assert_eq!(value["findings"][0]["rank"], 1);
}

#[test]
fn report_json_persists_scan_git_analysis_and_hotspots() {
    let fixture = ranked_fixture("report-json-persistence");
    let (_stdout, value) = report_json(fixture.path());

    let store = IndexStore::open(fixture.path()).expect("index should open");
    let persisted_scan = store
        .latest_scan()
        .expect("scan should read")
        .expect("scan should exist");
    assert_eq!(
        persisted_scan
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/related.rs", "src/risky.rs", "src/stable.rs"]
    );

    let persisted_git = IndexStore::open(fixture.path())
        .expect("index should reopen for Git")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(persisted_git.run.recent_window_days, 90);
    assert_eq!(persisted_git.run.metrics_observed, 3);

    let persisted_hotspots = IndexStore::open(fixture.path())
        .expect("index should reopen for hotspots")
        .latest_hotspots()
        .expect("hotspots should read");
    assert_eq!(
        persisted_hotspots
            .iter()
            .map(|hotspot| (hotspot.rank, hotspot.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "src/risky.rs"),
            (2, "src/related.rs"),
            (3, "src/stable.rs"),
        ]
    );
    assert_eq!(
        persisted_hotspots.len(),
        value["hotspots"].as_array().unwrap().len()
    );
}

#[test]
fn report_json_uses_repository_root_from_subdirectories_without_path_leaks() {
    let fixture = GitFixture::new("report-json-subdir");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("README.md", "# fixture\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add fixture files",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let subdir = fixture.path().join("src");
    let (stdout, value) = report_json(&subdir);

    assert_eq!(hotspot_paths(&value), vec!["README.md", "src/lib.rs"]);
    assert!(!stdout.contains("\\src\\lib.rs"));
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
        vec!["README.md", "src/lib.rs"]
    );
}

#[test]
fn report_json_rejects_non_git_directory_without_output_or_path_leak() {
    let fixture = TempDir::new("report-json-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["report", "--json"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run report from inside a repository"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn report_json_rejects_missing_head_without_output_or_path_leak() {
    let fixture = GitFixture::new("report-json-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["report", "--json"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn report_json_rejects_shallow_repository_without_output_or_path_leak() {
    let fixture = GitFixture::new("report-json-shallow");
    let author = GitIdentity::new("Shallow Author", "shallow@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    let commit = fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-06-01T00:00:00 +0000",
    ));
    fs::write(
        fixture.path().join(".git").join("shallow"),
        format!("{commit}\n"),
    )
    .expect("shallow marker should be written");

    let stderr = failed_stderr(&["report", "--json"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository has shallow history"));
    assert!(stderr.contains("fetch complete local history"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

fn ranked_fixture(name: &str) -> GitFixture {
    let fixture = GitFixture::new(name);
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");
    let ben = GitIdentity::new("Ben Bitdiddle", "ben@example.invalid");
    let cara = GitIdentity::new("Cara Compiler", "cara@example.invalid");

    fixture.write("src/risky.rs", &numbered_lines("risky", 50));
    fixture.write("src/stable.rs", "pub fn stable() {}\n");
    fixture.commit(CommitOptions::new(
        "Add risky and stable files",
        ada,
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/risky.rs", &numbered_lines("risky", 80));
    fixture.write("src/related.rs", "pub fn related() {}\n");
    fixture.commit(CommitOptions::new(
        "Update risky with related file",
        ben,
        "2024-03-20T00:00:00 +0000",
    ));

    fixture.write("src/risky.rs", &numbered_lines("risky", 100));
    fixture.write(
        "src/related.rs",
        "pub fn related() {}\npub fn more_related() {}\n",
    );
    fixture.commit(CommitOptions::new(
        "Grow risky with related file",
        cara,
        "2024-04-10T00:00:00 +0000",
    ));

    fixture
}

fn many_hotspots_fixture(name: &str, count: usize) -> GitFixture {
    let fixture = GitFixture::new(name);
    let author = GitIdentity::new("Many Author", "many@example.invalid");

    for index in 0..count {
        fixture.write(
            format!("src/file{index:02}.rs"),
            &format!("pub fn file_{index}() {{}}\n"),
        );
    }
    fixture.commit(CommitOptions::new(
        "Add many files",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    fixture
}

fn empty_current_file_set_fixture(name: &str) -> GitFixture {
    let fixture = GitFixture::new(name);
    let author = GitIdentity::new("Empty Author", "empty@example.invalid");

    fixture.write("src/removed.rs", "pub fn removed() {}\n");
    fixture.commit(CommitOptions::new(
        "Add removed file",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));
    fixture.delete("src/removed.rs");
    fixture.commit(CommitOptions::new(
        "Remove current files",
        author,
        "2024-01-02T00:00:00 +0000",
    ));

    fixture
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    (0..count)
        .map(|index| format!("pub fn {prefix}_{index}() {{}}\n"))
        .collect()
}

fn hotspot_paths(value: &Value) -> Vec<&str> {
    value["hotspots"]
        .as_array()
        .expect("hotspots should be an array")
        .iter()
        .map(|hotspot| hotspot["path"].as_str().expect("path should be a string"))
        .collect()
}

fn contains_path(output: &str, path: &Path) -> bool {
    let display = path.to_string_lossy();
    output.contains(display.as_ref()) || output.contains(&display.replace('\\', "/"))
}

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join("hotpath-report-fixtures")
            .join(format!("{name}-{}-{id}", std::process::id()));

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture root should be created");

        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
        let path = self.path.join(relative_path);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }

        fs::write(path, contents).expect("fixture file should be written");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
