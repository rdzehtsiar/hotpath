// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;
use serde_json::{json, Value};

static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::current_dir()
            .expect("test should have a current directory")
            .join("target")
            .join("integration-fixtures")
            .join(format!("{name}-{}-{id}", std::process::id()));

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("fixture root should be created");

        Self { path }
    }

    fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
        self.write_bytes(relative_path, contents.as_bytes());
    }

    fn write_bytes(&self, relative_path: impl AsRef<Path>, contents: &[u8]) {
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

fn context_json(args: &[&str], current_dir: &Path) -> (String, Value) {
    let mut context_args = vec!["context", "--json"];
    context_args.extend_from_slice(args);
    let stdout = successful_stdout(&context_args, current_dir);
    let value = serde_json::from_str(&stdout).expect("context JSON should parse");

    (stdout, value)
}

fn context_fixture(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    fixture.write("src/lib.rs", "12345678");
    fixture.write("src/tiny.rs", "abcd");
    fixture.write("alpha/tie.rs", "abcdefgh");
    fixture.write("zeta/tie.rs", "ijklmnop");
    fixture.write("README.md", "abcde");
    fixture.write("dist/client.gen.js", "generated!");
    fixture.write("node_modules/pkg/index.js", "vendor!!");
    fixture.write_bytes("assets/logo.bin", b"\x00PNG");
    fixture.write_bytes("data/latin1.txt", b"\xff\xfe\xfd");
    fixture
}

fn assert_no_path_leaks_in_text(text: &str, fixture_path: &Path) {
    let mut needles = Vec::new();
    push_path_leak_needles(&mut needles, fixture_path);

    if let Ok(canonical_path) = fs::canonicalize(fixture_path) {
        push_path_leak_needles(&mut needles, &canonical_path);
    }

    let text = comparable_path_string(text);
    for needle in needles {
        assert!(
            !text.contains(&comparable_path_string(&needle)),
            "text leaked fixture path {needle:?}"
        );
    }
}

fn assert_no_path_leaks_in_json(value: &Value, fixture_path: &Path) {
    let mut strings = Vec::new();
    collect_json_strings(value, &mut strings);

    for string in strings {
        assert_no_path_leaks_in_text(string, fixture_path);
    }
}

fn collect_json_strings<'a>(value: &'a Value, strings: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => strings.push(text),
        Value::Array(values) => {
            for value in values {
                collect_json_strings(value, strings);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_json_strings(value, strings);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
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

fn group_paths(value: &Value) -> Vec<&str> {
    value["groups"]
        .as_array()
        .expect("groups should be an array")
        .iter()
        .map(|group| {
            group["path"]
                .as_str()
                .expect("group path should be a string")
        })
        .collect()
}

fn skipped_reason<'a>(value: &'a Value, path: &str) -> &'a str {
    value["skipped"]
        .as_array()
        .expect("skipped should be an array")
        .iter()
        .find(|row| row["path"] == path)
        .unwrap_or_else(|| panic!("expected skipped row for {path}"))["reason"]
        .as_str()
        .expect("skipped reason should be a string")
}

#[test]
fn context_summary_works_without_git_persists_index_and_uses_relative_paths() {
    let fixture = context_fixture("context-summary");

    let stdout = successful_stdout(&["context"], &fixture.path);

    assert!(stdout.contains("Hotpath context budget"));
    assert!(stdout.contains("total estimated tokens  14"));
    assert!(stdout.contains("included files          7"));
    assert!(stdout.contains("skipped files           2"));
    assert!(stdout.contains("included bytes          51"));
    assert!(stdout.contains("\n  src  3  12  2"));
    assert!(stdout.contains("\n  .  2  5  1"));
    assert!(!fixture.path.join(".git").exists());
    assert!(fixture.path.join(".hotpath").join("index.db").is_file());
    assert_no_path_leaks_in_text(&stdout, &fixture.path);

    let store = IndexStore::open(&fixture.path).expect("index should open");
    let persisted = store
        .latest_scan()
        .expect("latest scan should read")
        .expect("latest scan should exist");
    assert_eq!(persisted.run.files_observed, Some(9));
}

#[test]
fn context_json_is_deterministic_and_reports_schema_options_summary_groups_skips_and_budget() {
    let fixture = context_fixture("context-json");

    let (first_stdout, value) = context_json(&["--budget", "20"], &fixture.path);
    let (second_stdout, second_value) = context_json(&["--budget", "20"], &fixture.path);

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(value, second_value);
    assert_no_path_leaks_in_json(&value, &fixture.path);
    assert_eq!(value["schema_version"], "hotpath.context.v1");
    assert_eq!(
        value["options"],
        json!({
            "exclude_generated": false,
            "exclude_vendor": false,
            "budget_tokens": 20
        })
    );
    assert_eq!(
        value["summary"],
        json!({
            "total_files": 9,
            "included_files": 7,
            "skipped_files": 2,
            "estimated_tokens": 14,
            "included_bytes": 51,
            "filtered_generated_files": 0,
            "filtered_vendor_files": 0
        })
    );
    assert_eq!(
        value["budget"],
        json!({
            "budget_tokens": 20,
            "estimated_tokens": 14,
            "remaining_tokens": 6,
            "over_budget_tokens": null
        })
    );
    assert_eq!(
        group_paths(&value),
        vec!["dist", "src", ".", "alpha", "node_modules", "zeta"]
    );
    assert_eq!(value["groups"][0]["estimated_tokens"], 3);
    assert_eq!(value["groups"][1]["estimated_tokens"], 3);
    assert_eq!(value["groups"][2]["path"], ".");
    assert_eq!(value["groups"][2]["estimated_tokens"], 2);
    assert_eq!(value["groups"][2]["file_count"], 1);
    assert_eq!(
        value["skipped"]
            .as_array()
            .expect("skipped should be an array")
            .len(),
        2
    );
    assert_eq!(skipped_reason(&value, "assets/logo.bin"), "binary");
    assert_eq!(skipped_reason(&value, "data/latin1.txt"), "unknown_content");
}

#[test]
fn context_token_math_uses_ceiling_byte_size_divided_by_four_for_utf8_text_only() {
    let fixture = Fixture::new("context-token-math");
    fixture.write("one.txt", "a");
    fixture.write("four.txt", "abcd");
    fixture.write("five.txt", "abcde");
    fixture.write("unicode.txt", "ééé");
    fixture.write_bytes("invalid.txt", b"\xff\xfe\xfd\xfc\xfb");
    fixture.write_bytes("binary.dat", b"\x00abcdefghi");

    let (_stdout, value) = context_json(&[], &fixture.path);

    assert_eq!(value["summary"]["included_bytes"], 16);
    assert_eq!(value["summary"]["estimated_tokens"], 6);
    assert_eq!(
        value["groups"],
        json!([{
            "path": ".",
            "file_count": 4,
            "byte_size": 16,
            "estimated_tokens": 6
        }])
    );
    assert_eq!(skipped_reason(&value, "binary.dat"), "binary");
    assert_eq!(skipped_reason(&value, "invalid.txt"), "unknown_content");
}

#[test]
fn context_budget_accepts_plain_k_and_m_suffixes_and_reports_status() {
    let fixture = context_fixture("context-budget");

    let stdout = successful_stdout(&["context", "--budget", "14"], &fixture.path);
    assert!(stdout.contains("budget: within budget by 0 tokens (budget 14, estimated 14)"));

    let (_stdout, value) = context_json(&["--budget", "1k"], &fixture.path);
    assert_eq!(value["options"]["budget_tokens"], 1_000);
    assert_eq!(value["budget"]["remaining_tokens"], 986);
    assert_eq!(value["budget"]["over_budget_tokens"], Value::Null);

    let (_stdout, value) = context_json(&["--budget", "1m"], &fixture.path);
    assert_eq!(value["options"]["budget_tokens"], 1_000_000);
    assert_eq!(value["budget"]["remaining_tokens"], 999_986);

    let (_stdout, value) = context_json(&["--budget", "13"], &fixture.path);
    assert_eq!(
        value["budget"],
        json!({
            "budget_tokens": 13,
            "estimated_tokens": 14,
            "remaining_tokens": null,
            "over_budget_tokens": 1
        })
    );
}

#[test]
fn context_rejects_invalid_budget_before_scan_or_persistence() {
    for (index, invalid) in ["", "0", "-1", "1.5k", "10g", "k"].iter().enumerate() {
        let fixture = Fixture::new(&format!("context-invalid-budget-{index}"));
        fixture.write("src/lib.rs", "pub fn lib() {}\n");

        let stderr = failed_stderr(&["context", "--budget", invalid], &fixture.path);

        assert!(
            stderr.contains("invalid value") || stderr.contains("unexpected argument"),
            "stderr was:\n{stderr}"
        );
        assert!(!stderr.contains("Hotpath context budget"));
        assert!(!fixture.path.join(".hotpath").exists());
    }
}

#[test]
fn context_exclude_generated_and_vendor_removes_files_from_estimate_and_reports_skips() {
    let fixture = context_fixture("context-excludes");

    let (_stdout, value) = context_json(
        &["--exclude-generated", "--exclude-vendor", "--budget", "10"],
        &fixture.path,
    );

    assert_eq!(value["options"]["exclude_generated"], true);
    assert_eq!(value["options"]["exclude_vendor"], true);
    assert_eq!(value["summary"]["included_files"], 5);
    assert_eq!(value["summary"]["included_bytes"], 33);
    assert_eq!(value["summary"]["estimated_tokens"], 9);
    assert_eq!(value["summary"]["filtered_generated_files"], 1);
    assert_eq!(value["summary"]["filtered_vendor_files"], 1);
    assert_eq!(group_paths(&value), vec!["src", ".", "alpha", "zeta"]);
    assert_eq!(
        skipped_reason(&value, "dist/client.gen.js"),
        "excluded_generated"
    );
    assert_eq!(
        skipped_reason(&value, "node_modules/pkg/index.js"),
        "excluded_vendor"
    );
    assert_eq!(skipped_reason(&value, "assets/logo.bin"), "binary");
    assert_eq!(skipped_reason(&value, "data/latin1.txt"), "unknown_content");
}

#[test]
fn context_sanitizes_corrupt_index_errors_and_does_not_render_report() {
    let fixture = Fixture::new("context-corrupt-index");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.write_bytes(".hotpath/index.db", b"not a sqlite database");

    let stderr = failed_stderr(&["context"], &fixture.path);

    assert!(stderr.starts_with(
        "hotpath: failed to persist scan results in local Hotpath index (.hotpath/index.db):"
    ));
    assert!(stderr.contains("remove .hotpath/index.db and rerun context"));
    assert!(!stderr.contains("Hotpath context budget"));
    assert_no_path_leaks_in_text(&stderr, &fixture.path);
}
