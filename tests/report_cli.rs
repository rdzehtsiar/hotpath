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

fn assert_exit_code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "expected exit code {expected}, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report_json(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["report", "--json"], current_dir);
    let value = serde_json::from_str(&stdout).expect("report JSON should parse");

    (stdout, value)
}

fn report_sarif(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["report", "--sarif"], current_dir);
    let value = serde_json::from_str(&stdout).expect("SARIF report JSON should parse");

    (stdout, value)
}

fn report_markdown(current_dir: &Path) -> String {
    successful_stdout(&["report", "--markdown"], current_dir)
}

fn report_html(current_dir: &Path, output_dir: &Path) -> String {
    let output_arg = output_dir.to_string_lossy().into_owned();

    successful_stdout(&["report", "--html", &output_arg], current_dir)
}

fn ci_output(current_dir: &Path, threshold: &str) -> Output {
    hotpath(&["ci", "--fail-on-risk", threshold], current_dir)
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
    assert!(default_stdout.contains("| 1 | `src/related.rs` |"));
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
fn report_html_writes_single_self_contained_escaped_file() {
    let fixture = GitFixture::new("report-html-escaped");
    let author = GitIdentity::new("HTML Author", "html@example.invalid");

    fixture.write("src/a&b'quote.rs", "pub fn html_path() {}\n");
    fixture.write("README.md", "# fixture\n");
    fixture.commit(CommitOptions::new(
        "Add escaped HTML paths",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let output_dir = fixture.path().join("target").join("hotpath-html");
    let stdout = report_html(fixture.path(), &output_dir);
    let index_path = output_dir.join("index.html");
    let html = fs::read_to_string(&index_path).expect("HTML report should be written");
    let entries = fs::read_dir(&output_dir)
        .expect("HTML output directory should be readable")
        .collect::<Result<Vec<_>, _>>()
        .expect("HTML output entries should read");

    assert_eq!(stdout, "Wrote HTML report to index.html\n");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_name(), "index.html");
    assert!(html.starts_with("<!doctype html>\n<html lang=\"en\">\n"));
    assert!(html.contains("<title>Hotpath Report</title>"));
    assert!(html.contains("<h1>Hotpath Report</h1>"));
    assert!(html.contains("<h2>Summary</h2>"));
    assert!(html.contains("<h2>Top Hotspots</h2>"));
    assert!(html.contains("<h2>Context Estimate</h2>"));
    assert!(html.contains("<h2>Findings</h2>"));
    assert!(html.contains("<h2>Calculation Notes</h2>"));
    assert!(html.contains("src/a&amp;b&#39;quote.rs"));
    assert!(!html.contains("src/a&b'quote.rs"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("</script>"));
    assert!(!html.contains("http://"));
    assert!(!html.contains("https://"));
    assert!(!html.contains("src=\""));
    assert!(!html.contains("href=\""));
    assert!(!contains_path(&html, fixture.path()));
}

#[test]
fn report_html_is_deterministic_across_runs_and_has_no_path_leaks() {
    let fixture = ranked_fixture("report-html-deterministic");
    let output_dir = fixture.path().join("out").join("report");

    let first_stdout = report_html(fixture.path(), &output_dir);
    let first_html =
        fs::read_to_string(output_dir.join("index.html")).expect("first HTML should read");
    let second_stdout = report_html(fixture.path(), &output_dir);
    let second_html =
        fs::read_to_string(output_dir.join("index.html")).expect("second HTML should read");

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(first_html, second_html);
    assert!(first_html.contains("<td>src/risky.rs</td>"));
    assert!(first_html.contains("Risk /10"));
    assert!(first_html.contains("does not require network access or telemetry"));
    assert!(!contains_path(&first_html, fixture.path()));
}

#[test]
fn report_rejects_html_with_other_format_flags() {
    let fixture = ranked_fixture("report-html-conflict");
    let output_dir = fixture.path().join("html");
    let output_arg = output_dir.to_string_lossy().into_owned();

    let json_stderr = failed_stderr(&["report", "--json", "--html", &output_arg], fixture.path());
    let markdown_stderr = failed_stderr(
        &["report", "--markdown", "--html", &output_arg],
        fixture.path(),
    );

    assert!(json_stderr.contains("--json"));
    assert!(json_stderr.contains("--html"));
    assert!(json_stderr.contains("cannot be used"));
    assert!(markdown_stderr.contains("--markdown"));
    assert!(markdown_stderr.contains("--html"));
    assert!(markdown_stderr.contains("cannot be used"));
}

#[test]
fn report_sarif_is_deterministic_and_has_expected_shape() {
    let fixture = sarif_levels_fixture("report-sarif-shape");

    let (first_stdout, value) = report_sarif(fixture.path());
    let (second_stdout, second_value) = report_sarif(fixture.path());

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(value, second_value);
    assert!(first_stdout.starts_with("{\n"));
    assert!(first_stdout.contains("\"version\": \"2.1.0\""));
    assert_eq!(value["version"], "2.1.0");
    assert_eq!(value["runs"].as_array().expect("runs array").len(), 1);
    assert_eq!(value["runs"][0]["tool"]["driver"]["name"], "Hotpath");
    assert_eq!(
        value["runs"][0]["tool"]["driver"]["rules"][0]["id"],
        "hotpath.hotspot.risk"
    );
    assert!(
        value["runs"][0]["tool"]["driver"]["rules"][0]["help"]["text"]
            .as_str()
            .expect("rule help")
            .contains("advisory decision-support")
    );
    assert_eq!(
        sarif_result_uris(&value),
        vec!["src/high.rs", "src/medium.rs", "src/low.rs"]
    );
    assert_eq!(
        sarif_result_levels(&value),
        vec!["warning", "warning", "note"]
    );
    assert!(first_stdout.contains("hotpath.score.v3"));
    assert!(first_stdout.contains("advisory decision-support"));
    assert!(!first_stdout.contains("pub fn"));
    assert!(!first_stdout.contains("\\src\\"));
    assert!(!contains_path(&first_stdout, fixture.path()));
}

#[test]
fn report_sarif_results_include_expected_properties() {
    let fixture = sarif_levels_fixture("report-sarif-properties");
    let (_stdout, value) = report_sarif(fixture.path());
    let result = &value["runs"][0]["results"][0];
    let score = result["properties"]["score"]
        .as_f64()
        .expect("score property should be numeric");
    let risk = result["properties"]["risk"]
        .as_f64()
        .expect("risk property should be numeric");

    assert_eq!(result["ruleId"], "hotpath.hotspot.risk");
    assert_eq!(result["ruleIndex"], 0);
    assert_eq!(result["properties"]["rank"], 1);
    assert_eq!(result["properties"]["formulaVersion"], "hotpath.score.v3");
    assert_eq!(risk, score * 10.0);
    assert!(result["message"]["text"]
        .as_str()
        .expect("message text")
        .contains("hotspot #1"));
    assert!(result["message"]["text"]
        .as_str()
        .expect("message text")
        .contains("/10"));
    assert!(result["message"]["text"]
        .as_str()
        .expect("message text")
        .contains("hotpath.score.v3"));
    assert!(result["properties"]["advisory"]
        .as_str()
        .expect("advisory property")
        .contains("Review the file and contributing metrics"));
}

#[test]
fn report_sarif_uses_portable_artifact_uris() {
    let fixture = GitFixture::new("report-sarif-uri");
    let author = GitIdentity::new("URI Author", "uri@example.invalid");

    fixture.write("src/weird path`name.rs", "pub fn uri_path() {}\n");
    fixture.commit(CommitOptions::new(
        "Add path needing URI escaping",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let (stdout, value) = report_sarif(fixture.path());

    assert_eq!(
        sarif_result_uris(&value),
        vec!["src/weird%20path%60name.rs"]
    );
    assert!(!stdout.contains("src/weird path`name.rs\""));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn report_sarif_rejects_non_git_directory_without_output_or_path_leak() {
    let fixture = TempDir::new("report-sarif-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["report", "--sarif"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run report from inside a repository"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn report_rejects_sarif_with_other_format_flags() {
    let fixture = ranked_fixture("report-sarif-conflict");
    let output_dir = fixture.path().join("html");
    let output_arg = output_dir.to_string_lossy().into_owned();

    let json_stderr = failed_stderr(&["report", "--sarif", "--json"], fixture.path());
    let markdown_stderr = failed_stderr(&["report", "--sarif", "--markdown"], fixture.path());
    let html_stderr = failed_stderr(
        &["report", "--sarif", "--html", &output_arg],
        fixture.path(),
    );

    assert!(json_stderr.contains("--sarif"));
    assert!(json_stderr.contains("--json"));
    assert!(json_stderr.contains("cannot be used"));
    assert!(markdown_stderr.contains("--sarif"));
    assert!(markdown_stderr.contains("--markdown"));
    assert!(markdown_stderr.contains("cannot be used"));
    assert!(html_stderr.contains("--sarif"));
    assert!(html_stderr.contains("--html"));
    assert!(html_stderr.contains("cannot be used"));
}

#[test]
fn ci_risk_gate_passes_when_max_risk_is_below_threshold_without_path_leaks() {
    let fixture = ranked_fixture("ci-pass");

    let output = ci_output(fixture.path(), "10");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_exit_code(&output, 0);
    assert!(output.stderr.is_empty());
    assert!(stdout.starts_with("Hotpath CI risk\n"));
    assert!(stdout.contains("result: pass\n"));
    assert!(stdout.contains("threshold: 10.000/10\n"));
    assert!(stdout.contains("max risk: "));
    assert!(stdout.contains("highest-risk file: src/related.rs\n"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn ci_risk_gate_fails_when_max_risk_meets_threshold() {
    let fixture = ranked_fixture("ci-fail");

    let output = ci_output(fixture.path(), "0.001");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_exit_code(&output, 1);
    assert!(output.stderr.is_empty());
    assert!(stdout.contains("result: fail\n"));
    assert!(stdout.contains("threshold: 0.001/10\n"));
    assert!(stdout.contains("highest-risk file: src/related.rs\n"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn ci_risk_gate_passes_empty_current_file_sets() {
    let fixture = empty_current_file_set_fixture("ci-empty");

    let output = ci_output(fixture.path(), "0.001");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_exit_code(&output, 0);
    assert!(output.stderr.is_empty());
    assert!(stdout.contains("result: pass\n"));
    assert!(stdout.contains("threshold: 0.001/10\n"));
    assert!(stdout.contains("max risk: none\n"));
    assert!(stdout.contains("highest-risk file: none\n"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn ci_rejects_invalid_thresholds_before_analysis_or_persistence() {
    for threshold in ["0", "-1", "10.1", "NaN", "inf", "not-a-number"] {
        let fixture = TempDir::new(&format!(
            "ci-invalid-{}",
            threshold.replace(['-', '.'], "_")
        ));
        fixture.write("src/lib.rs", "pub fn lib() {}\n");

        let output = ci_output(fixture.path(), threshold);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        assert_exit_code(&output, 2);
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("fail-on-risk"));
        assert!(!contains_path(&stderr, fixture.path()));
        assert!(!fixture.path().join(".hotpath").exists());
    }
}

#[test]
fn ci_operational_errors_exit_two_without_output_or_path_leaks() {
    let fixture = TempDir::new("ci-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let output = ci_output(fixture.path(), "8");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_exit_code(&output, 2);
    assert!(output.stdout.is_empty());
    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run report from inside a repository"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn report_json_preserves_hotspot_ranking_order_and_score_payloads() {
    let fixture = ranked_fixture("report-json-ranking");
    let (_stdout, value) = report_json(fixture.path());

    assert_eq!(
        hotspot_paths(&value),
        vec!["src/related.rs", "src/risky.rs", "src/stable.rs"]
    );
    assert_eq!(value["hotspots"][0]["rank"], 1);
    assert_eq!(value["hotspots"][0]["path"], "src/related.rs");
    assert_eq!(
        value["hotspots"][0]["formula_version"]["id"],
        "hotpath.score.v3"
    );
    assert_eq!(
        value["hotspots"][0]["raw_metrics"]["path"],
        "src/related.rs"
    );
    assert_eq!(value["hotspots"][0]["raw_metrics"]["line_count"], 2);
    assert!(value["hotspots"][0]["weighted_terms"]
        .as_array()
        .expect("weighted terms")
        .iter()
        .any(|term| term["name"] == "churn_score"));
    assert_eq!(value["findings"][0]["code"], "hotpath.hotspot.risk");
    assert_eq!(value["findings"][0]["path"], "src/related.rs");
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
            (1, "src/related.rs"),
            (2, "src/risky.rs"),
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
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn report_json_rejects_missing_head_without_output_or_path_leak() {
    let fixture = GitFixture::new("report-json-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["report", "--json"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
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
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
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

fn sarif_levels_fixture(name: &str) -> GitFixture {
    let fixture = GitFixture::new(name);
    let authors = [
        GitIdentity::new("Ada Levels", "ada-levels@example.invalid"),
        GitIdentity::new("Ben Levels", "ben-levels@example.invalid"),
        GitIdentity::new("Cara Levels", "cara-levels@example.invalid"),
        GitIdentity::new("Dee Levels", "dee-levels@example.invalid"),
        GitIdentity::new("Eli Levels", "eli-levels@example.invalid"),
        GitIdentity::new("Flo Levels", "flo-levels@example.invalid"),
    ];

    fixture.write("src/high.rs", &numbered_lines("high_0", 1_000));
    fixture.write("src/medium.rs", &numbered_lines("medium", 1_000));
    fixture.write("src/low.rs", "pub fn low() {}\n");
    fixture.commit(CommitOptions::new(
        "Add SARIF level files",
        authors[0].clone(),
        "2024-01-01T00:00:00 +0000",
    ));

    fixture.write("src/medium.rs", &numbered_lines("medium_rewrite", 1_000));
    fixture.commit(CommitOptions::new(
        "Rewrite medium risk file",
        authors[0].clone(),
        "2024-01-15T00:00:00 +0000",
    ));

    for (index, author) in authors.iter().enumerate().skip(1) {
        fixture.write(
            "src/high.rs",
            &numbered_lines(&format!("high_{index}"), 1_000),
        );
        fixture.commit(CommitOptions::new(
            &format!("Rewrite high risk file {index}"),
            author.clone(),
            &format!("2024-0{}-01T00:00:00 +0000", index + 1),
        ));
    }

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

fn sarif_results(value: &Value) -> &[Value] {
    value["runs"][0]["results"]
        .as_array()
        .expect("SARIF results should be an array")
}

fn sarif_result_uris(value: &Value) -> Vec<&str> {
    sarif_results(value)
        .iter()
        .map(|result| {
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
                .as_str()
                .expect("artifact URI should be a string")
        })
        .collect()
}

fn sarif_result_levels(value: &Value) -> Vec<&str> {
    sarif_results(value)
        .iter()
        .map(|result| result["level"].as_str().expect("level should be a string"))
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
