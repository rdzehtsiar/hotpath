// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
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

fn failed_output(args: &[&str], current_dir: &Path) -> Output {
    let output = hotpath(args, current_dir);

    assert!(
        !output.status.success(),
        "hotpath unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    output
}

fn failed_stderr(args: &[&str], current_dir: &Path) -> String {
    let output = failed_output(args, current_dir);

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

fn successful_json(args: &[&str], current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(args, current_dir);
    let value = serde_json::from_str(&stdout).expect("diff JSON should parse");

    (stdout, value)
}

fn diff_fixture(name: &str) -> GitFixture {
    let fixture = GitFixture::new(name);
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");
    let ben = GitIdentity::new("Ben Bitdiddle", "ben@example.invalid");

    fixture.write("src/risky.rs", &numbered_lines("risky", 20));
    fixture.write("src/stable.rs", "pub fn stable() {}\n");
    fixture.commit(CommitOptions::new(
        "Add base files",
        ada,
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.git_stdout(["checkout", "--quiet", "-b", "feature"]);
    fixture.write("src/risky.rs", &numbered_lines("risky", 80));
    fixture.write("src/new.rs", "pub fn new_file() {}\n");
    fixture.commit(CommitOptions::new(
        "Change risky file and add new file",
        ben,
        "2024-03-01T00:00:00 +0000",
    ));

    fixture
}

#[test]
fn diff_success_persists_scan_git_analysis_and_hotspots() {
    let fixture = diff_fixture("diff-persisted");

    let stdout = successful_stdout(&["diff", "main...HEAD"], fixture.path());

    assert!(stdout.starts_with("Hotpath diff risk\n"));
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
            .map(|file| (file.path.as_str(), file.line_count))
            .collect::<Vec<_>>(),
        vec![
            ("src/new.rs", Some(1)),
            ("src/risky.rs", Some(80)),
            ("src/stable.rs", Some(1)),
        ]
    );

    let persisted_git = IndexStore::open(fixture.path())
        .expect("index should reopen")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(persisted_git.run.head_commit_time, 1709251200);
    assert_eq!(persisted_git.run.recent_window_days, 90);
    assert_eq!(
        persisted_git
            .file_stats
            .iter()
            .map(|stats| (stats.path.as_str(), stats.commits_per_file))
            .collect::<Vec<_>>(),
        vec![("src/new.rs", 1), ("src/risky.rs", 2), ("src/stable.rs", 1),]
    );

    let persisted_hotspots = IndexStore::open(fixture.path())
        .expect("index should reopen for hotspots")
        .latest_hotspots()
        .expect("hotspots should read");
    assert_eq!(
        persisted_hotspots
            .iter()
            .map(|hotspot| (hotspot.rank, hotspot.path.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "src/risky.rs"), (2, "src/stable.rs"), (3, "src/new.rs"),]
    );
    assert!(persisted_hotspots
        .iter()
        .all(|hotspot| hotspot.formula_version == "hotpath.score.v3"));
}

#[test]
fn diff_text_output_contains_required_labels_and_touched_hotspot_details() {
    let fixture = diff_fixture("diff-text");

    let stdout = successful_stdout(&["diff", "main...HEAD"], fixture.path());

    assert!(stdout.starts_with("Hotpath diff risk\n"));
    assert!(stdout.contains("Changed files: 2"));
    assert!(stdout.contains("Touched hotspots: 2"));
    assert!(stdout.contains("Architecture violations: not evaluated"));
    assert!(stdout.contains("Context growth: +"));
    assert!(stdout.contains("\n  #"));
    assert!(stdout.contains("src/risky.rs"));
    assert!(stdout.contains("src/new.rs"));
    assert!(stdout.contains("\nchanged files\n"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn diff_json_output_has_stable_schema_summary_changed_files_and_touched_hotspots() {
    let fixture = diff_fixture("diff-json");

    let (first_stdout, value) = successful_json(&["diff", "main...HEAD", "--json"], fixture.path());
    let (second_stdout, second_value) =
        successful_json(&["diff", "main...HEAD", "--json"], fixture.path());

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(value, second_value);
    assert_eq!(value["schema_version"], "hotpath.diff.v1");
    assert_eq!(value["range"]["requested"], "main...HEAD");
    assert_eq!(value["range"]["base_ref"], "main");
    assert_eq!(value["range"]["head_ref"], "HEAD");
    assert_eq!(value["summary"]["changed_file_count"], 2);
    assert_eq!(value["summary"]["touched_hotspot_count"], 2);
    assert_eq!(value["summary"]["architecture"], "not_evaluated");
    assert_eq!(value["architecture"], "not_evaluated");
    assert!(
        value["summary"]["context_token_delta"]
            .as_i64()
            .expect("context delta should be signed")
            > 0
    );
    assert_eq!(
        changed_paths(&value),
        vec!["src/new.rs".to_owned(), "src/risky.rs".to_owned()]
    );
    assert_eq!(
        touched_hotspot_paths(&value),
        vec!["src/new.rs".to_owned(), "src/risky.rs".to_owned()]
    );
    assert_no_path_leaks_in_json(&value, fixture.path());
}

#[test]
fn pr_json_is_equivalent_to_diff_json_for_matching_refs() {
    let fixture = diff_fixture("diff-pr-equivalence");

    let (_stdout, diff_value) = successful_json(&["diff", "main...HEAD", "--json"], fixture.path());
    let (_stdout, pr_value) = successful_json(
        &["pr", "--base", "main", "--head", "HEAD", "--json"],
        fixture.path(),
    );

    assert_eq!(diff_value["schema_version"], pr_value["schema_version"]);
    assert_eq!(diff_value["range"], pr_value["range"]);
    assert_eq!(diff_value["summary"], pr_value["summary"]);
    assert_eq!(diff_value["changed_files"], pr_value["changed_files"]);
    assert_eq!(diff_value["architecture"], pr_value["architecture"]);
}

#[test]
fn diff_subdirectory_invocation_uses_repository_relative_paths_without_leaks() {
    let fixture = diff_fixture("diff-subdir");
    let stdout = successful_stdout(&["diff", "main...HEAD"], &fixture.path().join("src"));

    assert!(stdout.contains("src/risky.rs"));
    assert!(stdout.contains("src/new.rs"));
    assert!(!stdout.contains("\\src\\risky.rs"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn diff_invalid_range_has_empty_stdout_and_useful_stderr() {
    let fixture = diff_fixture("diff-invalid-range");

    let output = failed_output(&["diff", "main..HEAD"], fixture.path());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");

    assert!(stderr.starts_with("hotpath: diff range must use base...head syntax"));
    assert!(stderr.contains("two-dot ranges are not supported"));
    assert!(!stderr.contains("Hotpath diff risk"));
    assert!(!contains_path(&stderr, fixture.path()));
}

#[test]
fn diff_sanitizes_corrupt_index_errors_without_report_output_or_path_leak() {
    let fixture = diff_fixture("diff-corrupt-index");
    fixture.write(".hotpath/index.db", "not a sqlite database");

    let stderr = failed_stderr(&["diff", "main...HEAD"], fixture.path());

    assert!(stderr.starts_with(
        "hotpath: failed to persist scan results in local Hotpath index (.hotpath/index.db):"
    ));
    assert!(stderr.contains("remove .hotpath/index.db and rerun diff"));
    assert!(!stderr.contains("Hotpath diff risk"));
    assert!(!contains_path(&stderr, fixture.path()));
}

#[test]
fn diff_rejects_non_git_directory_without_report_output_or_index_creation() {
    let fixture = TempDir::new("diff-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["diff", "main...HEAD"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run diff analysis from inside a repository"));
    assert!(!stderr.contains("Hotpath diff risk"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn diff_rejects_missing_head_without_report_output_or_index_creation() {
    let fixture = GitFixture::new("diff-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["diff", "main...HEAD"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit before analyzing a diff"));
    assert!(!stderr.contains("Hotpath diff risk"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn diff_rejects_shallow_repository_without_report_output_or_index_creation() {
    let fixture = GitFixture::new("diff-shallow");
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

    let stderr = failed_stderr(&["diff", "main...HEAD"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository has shallow history"));
    assert!(stderr.contains("fetch complete local history"));
    assert!(!stderr.contains("Hotpath diff risk"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    (0..count)
        .map(|index| format!("pub fn {prefix}_{index}() {{}}\n"))
        .collect()
}

fn changed_paths(value: &Value) -> Vec<String> {
    value["changed_files"]
        .as_array()
        .expect("changed files should be an array")
        .iter()
        .map(|file| {
            file["path"]
                .as_str()
                .expect("changed file path should be a string")
                .to_owned()
        })
        .collect()
}

fn touched_hotspot_paths(value: &Value) -> Vec<String> {
    let mut paths = value["summary"]["touched_hotspots"]
        .as_array()
        .expect("touched hotspots should be an array")
        .iter()
        .map(|hotspot| {
            hotspot["path"]
                .as_str()
                .expect("hotspot path should be a string")
                .to_owned()
        })
        .collect::<Vec<_>>();

    paths.sort();
    paths
}

fn assert_no_path_leaks_in_json(value: &Value, fixture_path: &Path) {
    let mut strings = Vec::new();
    collect_json_strings(value, &mut strings);

    for string in strings {
        assert!(
            !contains_path(string, fixture_path),
            "JSON string leaked fixture path: {string}"
        );
    }
}

fn collect_json_strings<'a>(value: &'a Value, strings: &mut Vec<&'a str>) {
    let mut stack = vec![value];

    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => strings.push(text),
            Value::Array(values) => {
                for value in values.iter().rev() {
                    stack.push(value);
                }
            }
            Value::Object(values) => {
                let values = values.values().collect::<Vec<_>>();
                for value in values.into_iter().rev() {
                    stack.push(value);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

#[test]
fn json_string_collection_handles_deep_json_iteratively() {
    let mut value = Value::String("needle".to_owned());
    for _ in 0..2_000 {
        value = Value::Array(vec![value]);
    }
    let mut strings = Vec::new();

    collect_json_strings(&value, &mut strings);

    assert_eq!(strings, vec!["needle"]);
}

fn contains_path(output: &str, path: &Path) -> bool {
    let mut candidates = Vec::new();
    push_path_leak_needles(&mut candidates, path);

    if let Ok(canonical_path) = fs::canonicalize(path) {
        push_path_leak_needles(&mut candidates, &canonical_path);
    }

    candidates
        .iter()
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| {
            comparable_path_string(output).contains(&comparable_path_string(candidate))
        })
}

fn push_path_leak_needles(needles: &mut Vec<String>, path: &Path) {
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

    for candidate in candidates {
        if !candidate.is_empty() && !needles.contains(&candidate) {
            needles.push(candidate);
        }
    }
}

#[cfg(windows)]
fn comparable_path_string(value: &str) -> String {
    value.replace('\\', "/").to_ascii_lowercase()
}

#[cfg(not(windows))]
fn comparable_path_string(value: &str) -> String {
    value.to_owned()
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
            .join("diff-fixtures")
            .join(format!("{name}-{}-{id}", std::process::id()));

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture root should be created");
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
