// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;
use hotpath::ContentKind;
use serde_json::Value;

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

fn scan_json(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["scan", "--json"], current_dir);
    let value = serde_json::from_str(&stdout).expect("scan JSON should parse");

    (stdout, value)
}

fn parse_json(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["parse", "--json"], current_dir);
    let value = serde_json::from_str(&stdout).expect("parse JSON should parse");

    (stdout, value)
}

fn expected_doctor_stdout() -> &'static str {
    concat!(
        "Hotpath doctor\n",
        "index path: .hotpath/index.db\n",
        "schema version: 2\n",
        "readable: yes\n",
        "health: healthy\n",
    )
}

fn expected_missing_doctor_stdout() -> &'static str {
    concat!(
        "Hotpath doctor\n",
        "index path: .hotpath/index.db\n",
        "schema version: none\n",
        "readable: no\n",
        "health: missing\n",
    )
}

fn file_by_path<'a>(value: &'a Value, path: &str) -> &'a Value {
    value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("expected scan record for {path}"))
}

fn assert_persisted_files_match_scan_json(current_dir: &Path, value: &Value) {
    let store = IndexStore::open(current_dir).expect("index should open");
    let persisted = store
        .latest_scan()
        .expect("latest scan should read")
        .expect("latest scan should exist");
    let json_files = value["files"].as_array().expect("files should be an array");

    assert_eq!(persisted.run.files_observed, Some(json_files.len() as u64));
    assert_eq!(
        persisted.run.warnings_observed,
        value["summary"]["warnings"]["total_warnings"].as_u64()
    );
    assert_eq!(
        persisted
            .warnings
            .iter()
            .map(|warning| (
                warning.code.as_str(),
                warning.path.as_deref(),
                warning.message.as_str()
            ))
            .collect::<Vec<_>>(),
        value["warnings"]
            .as_array()
            .expect("warnings should be an array")
            .iter()
            .map(|warning| (
                warning["code"]
                    .as_str()
                    .expect("warning code should be a string"),
                warning["path"].as_str(),
                warning["message"]
                    .as_str()
                    .expect("warning message should be a string")
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        persisted
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        json_files
            .iter()
            .map(|file| file["path"].as_str().expect("path should be a string"))
            .collect::<Vec<_>>()
    );

    for persisted_file in persisted.files {
        let json_file = file_by_path(value, &persisted_file.path);

        assert_eq!(
            persisted_file.byte_size,
            json_file["byte_size"].as_u64(),
            "byte_size mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.extension.as_deref(),
            json_file["extension"].as_str(),
            "extension mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.language.as_deref(),
            json_file["language"].as_str(),
            "language mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.line_count,
            json_file["line_count"].as_u64(),
            "line_count mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.is_vendor,
            json_file["is_vendor"]
                .as_bool()
                .expect("is_vendor should be bool"),
            "is_vendor mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.is_generated,
            json_file["is_generated"]
                .as_bool()
                .expect("is_generated should be bool"),
            "is_generated mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            content_kind_name(persisted_file.content),
            json_file["content"]
                .as_str()
                .expect("content should be a string"),
            "content mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.is_symlink,
            json_file["is_symlink"]
                .as_bool()
                .expect("is_symlink should be bool"),
            "is_symlink mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file.classification.as_deref(),
            json_file["classification"].as_str(),
            "classification mismatch for {}",
            persisted_file.path
        );
        assert_eq!(
            persisted_file
                .warnings
                .iter()
                .map(|warning| (warning.code.as_str(), warning.message.as_str()))
                .collect::<Vec<_>>(),
            json_file["warnings"]
                .as_array()
                .expect("file warnings should be an array")
                .iter()
                .map(|warning| (
                    warning["code"]
                        .as_str()
                        .expect("file warning code should be a string"),
                    warning["message"]
                        .as_str()
                        .expect("file warning message should be a string")
                ))
                .collect::<Vec<_>>(),
            "warnings mismatch for {}",
            persisted_file.path
        );
    }
}

fn content_kind_name(content: ContentKind) -> &'static str {
    match content {
        ContentKind::Text => "text",
        ContentKind::Binary => "binary",
        ContentKind::Unknown => "unknown",
    }
}

fn assert_json_strings_do_not_contain_path(value: &Value, path: &Path) {
    let mut needles = Vec::new();
    push_path_leak_needles(&mut needles, path);

    if let Ok(canonical_path) = fs::canonicalize(path) {
        push_path_leak_needles(&mut needles, &canonical_path);
    }

    let needles = needles
        .into_iter()
        .map(|needle| comparable_path_string(&needle))
        .collect::<Vec<_>>();
    let mut leaks = Vec::new();

    collect_json_path_leaks(value, "$", &needles, &mut leaks);

    assert!(
        leaks.is_empty(),
        "scan JSON leaked fixture path in string values: {leaks:?}"
    );
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

fn collect_json_path_leaks(
    value: &Value,
    location: &str,
    needles: &[String],
    leaks: &mut Vec<(String, String)>,
) {
    match value {
        Value::String(text) => {
            let text = comparable_path_string(text);

            if needles.iter().any(|needle| text.contains(needle)) {
                leaks.push((location.to_owned(), text));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_json_path_leaks(item, &format!("{location}[{index}]"), needles, leaks);
            }
        }
        Value::Object(entries) => {
            for (key, item) in entries {
                collect_json_path_leaks(item, &format!("{location}.{key}"), needles, leaks);
            }
        }
        Value::Bool(_) | Value::Number(_) | Value::Null => {}
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

#[cfg(unix)]
fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

fn symlink_setup_should_skip(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
    ) || cfg!(windows) && error.raw_os_error() == Some(1314)
}

fn create_symlink_or_skip(
    original: impl AsRef<Path>,
    link: impl AsRef<Path>,
) -> Result<(), std::io::Error> {
    match symlink_file(original, link) {
        Ok(()) => Ok(()),
        Err(error) if symlink_setup_should_skip(&error) => Err(error),
        Err(error) => panic!("unexpected symlink setup error: {error}"),
    }
}

#[test]
fn doctor_reports_missing_index_without_initializing() {
    let fixture = Fixture::new("doctor-missing");

    assert!(!fixture.path.join(".hotpath").exists());

    let stdout = successful_stdout(&["doctor"], &fixture.path);

    assert_eq!(stdout, expected_missing_doctor_stdout());
    assert!(!stdout.contains("health: healthy"));
    assert!(!fixture.path.join(".hotpath").join("index.db").exists());
}

#[test]
fn doctor_reports_healthy_existing_index() {
    let fixture = Fixture::new("doctor-existing");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    successful_stdout(&["scan"], &fixture.path);

    let stdout = successful_stdout(&["doctor"], &fixture.path);

    assert_eq!(stdout, expected_doctor_stdout());
}

#[test]
fn doctor_fails_actionably_for_corrupt_index() {
    let fixture = Fixture::new("doctor-corrupt");
    fixture.write_bytes(".hotpath/index.db", b"not a sqlite database");

    let output = hotpath(&["doctor"], &fixture.path);

    assert!(
        !output.status.success(),
        "doctor unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");

    assert!(stderr.starts_with("hotpath: failed to inspect Hotpath index:"));
    assert!(stderr.contains("index.db"));
    assert!(
        stderr.contains("failed to open Hotpath index") || stderr.contains("unreadable or corrupt"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn scan_json_classifies_generated_fixture_end_to_end() {
    let fixture = Fixture::new("json-classification");
    fixture.write(".gitignore", "ignored/\n*.log\n");
    fixture.write("ignored/secret.rs", "fn ignored() {}\n");
    fixture.write("notes.log", "ignored log\n");
    fixture.write("README.md", "# Fixture\n\nbody\n");
    fixture.write("src/main.rs", "fn main() {}\nprintln!(\"hi\");\n");
    fixture.write("build/app.generated.js", "console.log('built');\n");
    fixture.write("node_modules/pkg/index.js", "module.exports = 1;\n");
    fixture.write_bytes("assets/logo.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);

    let (_, value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &value);
    let paths = value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .map(|file| file["path"].as_str().expect("path should be a string"))
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![
            ".gitignore",
            "README.md",
            "assets/logo.bin",
            "build/app.generated.js",
            "node_modules/pkg/index.js",
            "src/main.rs",
        ]
    );
    assert_json_strings_do_not_contain_path(&value, &fixture.path);
    assert!(paths.iter().all(|path| !path.contains('\\')));

    assert_eq!(value["schema_version"], "hotpath.scan.v1");
    assert_eq!(value["summary"]["total_files"], 6);
    assert_eq!(value["summary"]["content"]["text_files"], 5);
    assert_eq!(value["summary"]["content"]["binary_files"], 1);
    assert_eq!(value["summary"]["content"]["unknown_files"], 0);
    assert_eq!(value["summary"]["flags"]["generated_files"], 1);
    assert_eq!(value["summary"]["flags"]["vendor_files"], 1);
    assert_eq!(value["summary"]["warnings"]["total_warnings"], 0);
    assert_eq!(value["summary"]["languages"]["JavaScript"], 2);
    assert_eq!(value["summary"]["languages"]["Markdown"], 1);
    assert_eq!(value["summary"]["languages"]["Rust"], 1);

    let rust = file_by_path(&value, "src/main.rs");
    assert_eq!(rust["extension"], "rs");
    assert_eq!(rust["language"], "Rust");
    assert_eq!(rust["line_count"], 2);
    assert_eq!(rust["content"], "text");
    assert_eq!(rust["is_vendor"], false);
    assert_eq!(rust["is_generated"], false);
    assert_eq!(rust["warnings"], Value::Array(Vec::new()));

    let binary = file_by_path(&value, "assets/logo.bin");
    assert_eq!(binary["byte_size"], 8);
    assert_eq!(binary["extension"], "bin");
    assert_eq!(binary["language"], Value::Null);
    assert_eq!(binary["line_count"], Value::Null);
    assert_eq!(binary["content"], "binary");

    assert_eq!(
        file_by_path(&value, "build/app.generated.js")["is_generated"],
        true
    );
    assert_eq!(
        file_by_path(&value, "node_modules/pkg/index.js")["is_vendor"],
        true
    );
}

#[test]
fn scan_json_output_is_stable_across_runs() {
    let fixture = Fixture::new("deterministic-json");
    fixture.write("z.rs", "");
    fixture.write(Path::new("nested").join("m.rs"), "");
    fixture.write("docs/.hotpath/config.md", "# Nested config\n");
    fixture.write("a.rs", "");

    let (first_stdout, first_value) = scan_json(&fixture.path);
    assert!(fixture.path.join(".hotpath").join("index.db").is_file());
    let (second_stdout, second_value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &second_value);
    let paths = first_value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .map(|file| file["path"].as_str().expect("path should be a string"))
        .collect::<Vec<_>>();

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(first_value, second_value);
    assert_eq!(
        paths,
        vec!["a.rs", "docs/.hotpath/config.md", "nested/m.rs", "z.rs"]
    );
    assert!(paths.iter().all(|path| !path.starts_with(".hotpath/")));
}

#[test]
fn scan_json_removes_deleted_files_from_persisted_index() {
    let fixture = Fixture::new("delete-between-scans");
    fixture.write("src/keep.rs", "fn keep() {}\n");
    fixture.write("src/delete.rs", "fn delete() {}\n");

    let (_, first_value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &first_value);
    fs::remove_file(fixture.path.join("src").join("delete.rs"))
        .expect("fixture file should be deleted");
    fixture.write("src/keep.rs", "fn keep() {}\nfn current() {}\n");

    let (_, second_value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &second_value);
    let paths = second_value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .map(|file| file["path"].as_str().expect("path should be a string"))
        .collect::<Vec<_>>();
    let store = IndexStore::open(&fixture.path).expect("index should open");
    let persisted = store
        .latest_scan()
        .expect("latest scan should read")
        .expect("latest scan should exist");

    assert_eq!(paths, vec!["src/keep.rs"]);
    assert_eq!(file_by_path(&second_value, "src/keep.rs")["line_count"], 2);
    assert!(paths.iter().all(|path| !path.starts_with(".hotpath/")));
    assert_eq!(
        persisted
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.line_count))
            .collect::<Vec<_>>(),
        vec![("src/keep.rs", Some(2))]
    );
}

#[test]
fn scan_json_reports_scan_warning_fields_end_to_end() {
    let fixture = Fixture::new("json-scan-warning");
    fixture.write(".gitignore", "{foo\n");
    fixture.write("keep.rs", "");

    let (_, value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &value);
    let warnings = value["warnings"]
        .as_array()
        .expect("top-level warnings should be an array");
    let warning = warnings
        .iter()
        .find(|warning| warning["code"] == "ignore_parse_error")
        .expect("malformed .gitignore should produce a scan warning");

    assert_eq!(value["schema_version"], "hotpath.scan.v1");
    assert!(warning["path"].is_string() || warning["path"].is_null());
    assert!(warning["message"]
        .as_str()
        .expect("scan warning message should be a string")
        .contains("glob"));
    assert_eq!(value["summary"]["warnings"]["total_warnings"], 1);
    assert_eq!(value["summary"]["warnings"]["scan_warnings"], 1);
    assert_eq!(value["summary"]["warnings"]["unreadable_warnings"], 0);
    assert_eq!(value["summary"]["warnings"]["skipped_warnings"], 0);
}

#[test]
fn scan_json_persists_file_warning_payloads_end_to_end() {
    let fixture = Fixture::new("json-file-warning");
    fixture.write_bytes("bad.txt", &[b'a', 0xff, b'\n']);

    let (_, value) = scan_json(&fixture.path);
    assert_persisted_files_match_scan_json(&fixture.path, &value);
    let bad = file_by_path(&value, "bad.txt");

    assert_eq!(bad["warnings"][0]["code"], "unsupported_encoding");
    assert_eq!(
        bad["warnings"][0]["message"],
        "file contents are not valid UTF-8"
    );
    assert_eq!(value["summary"]["warnings"]["total_warnings"], 1);
}

#[test]
fn scan_summary_and_default_cli_report_concise_totals() {
    let fixture = Fixture::new("summary");
    fixture.write("src/lib.rs", "fn lib() {}\n");
    fixture.write("dist/app.generated.js", "");
    fixture.write_bytes("vendor/blob.bin", &[0, 1, 2, 3]);

    let summary = successful_stdout(&["scan", "--summary"], &fixture.path);
    let default_summary = successful_stdout(&["scan"], &fixture.path);

    assert_eq!(summary, default_summary);
    assert_eq!(
        summary,
        concat!(
            "Hotpath scan summary\n",
            "total files   3\n",
            "total bytes   16\n",
            "content       text 2, binary 1, unknown 0\n",
            "flags         generated 1, vendor 1\n",
            "languages\n",
            "  JavaScript  1\n",
            "  Rust        1\n",
        )
    );
}

#[test]
fn scan_rejects_conflicting_output_flags_before_persisting() {
    let fixture = Fixture::new("conflicting-flags");
    fixture.write("src/lib.rs", "fn lib() {}\n");

    let stderr = failed_stderr(&["scan", "--summary", "--json"], &fixture.path);

    assert!(stderr.contains("cannot be used with"));
    assert!(stderr.contains("--summary"));
    assert!(stderr.contains("--json"));
    assert!(!fixture.path.join(".hotpath").exists());
}

#[test]
fn parse_summary_command_exists_and_persists_scan() {
    let fixture = Fixture::new("parse-summary");
    fixture.write("README.md", "# Fixture\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stdout = successful_stdout(&["parse"], &fixture.path);

    assert_eq!(
        stdout,
        concat!(
            "Hotpath parse summary\n",
            "total files   2\n",
            "candidates    1\n",
            "pending       1\n",
            "skipped       1\n",
            "symbols       0\n",
            "imports       0\n",
        )
    );
    assert!(fixture.path.join(".hotpath").join("index.db").is_file());
}

#[test]
fn parse_json_reports_schema_and_skips_unsupported_files_end_to_end() {
    let fixture = Fixture::new("parse-json");
    fixture.write("README.md", "# Fixture\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.write_bytes("assets/logo.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);

    let (_, value) = parse_json(&fixture.path);
    let files = value["files"].as_array().expect("files should be an array");

    assert_eq!(value["schema_version"], "hotpath.parse.v1");
    assert_eq!(value["summary"]["total_files"], 3);
    assert_eq!(value["summary"]["candidate_files"], 1);
    assert_eq!(value["summary"]["pending_files"], 1);
    assert_eq!(value["summary"]["skipped_files"], 2);
    assert_eq!(value["summary"]["symbol_count"], 0);
    assert_eq!(value["summary"]["import_count"], 0);
    assert_eq!(value["summary"]["warning_count"], 0);
    assert_eq!(value["warnings"], Value::Array(Vec::new()));
    assert_eq!(value["symbols"], Value::Array(Vec::new()));
    assert_eq!(value["imports"], Value::Array(Vec::new()));
    assert_json_strings_do_not_contain_path(&value, &fixture.path);

    assert_eq!(
        files
            .iter()
            .map(|file| file["path"].as_str().expect("path should be a string"))
            .collect::<Vec<_>>(),
        vec!["README.md", "assets/logo.bin", "src/lib.rs"]
    );

    let rust = file_by_path(&value, "src/lib.rs");
    assert_eq!(rust["language"], "Rust");
    assert_eq!(rust["content"], "text");
    assert_eq!(rust["status"], "pending");
    assert_eq!(rust["reason"], "parser_extraction_pending");
    assert_eq!(rust["symbol_count"], 0);
    assert_eq!(rust["import_count"], 0);

    let markdown = file_by_path(&value, "README.md");
    assert_eq!(markdown["language"], "Markdown");
    assert_eq!(markdown["content"], "text");
    assert_eq!(markdown["status"], "skipped");
    assert_eq!(markdown["reason"], "unsupported_language");

    let binary = file_by_path(&value, "assets/logo.bin");
    assert_eq!(binary["language"], Value::Null);
    assert_eq!(binary["content"], "binary");
    assert_eq!(binary["status"], "skipped");
    assert_eq!(binary["reason"], "unsupported_content");

    let persisted = IndexStore::open(&fixture.path)
        .expect("index should open")
        .latest_scan()
        .expect("latest scan should read")
        .expect("latest scan should exist");

    assert_eq!(persisted.run.files_observed, Some(3));
    assert_eq!(
        persisted
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["README.md", "assets/logo.bin", "src/lib.rs"]
    );
}

#[test]
fn scan_json_records_file_symlinks_when_the_platform_supports_them() {
    let fixture = Fixture::new("symlinks");
    let outside = Fixture::new("outside-symlink-target");
    fixture.write("src/target.rs", "pub fn target() {}\n");
    outside.write("external.rs", "pub fn external() {}\n");

    if create_symlink_or_skip(
        fixture.path.join("src").join("target.rs"),
        fixture.path.join("src").join("linked.rs"),
    )
    .is_err()
    {
        return;
    }

    if create_symlink_or_skip(
        outside.path.join("external.rs"),
        fixture.path.join("outside.rs"),
    )
    .is_err()
    {
        return;
    }

    let (_, value) = scan_json(&fixture.path);
    let linked = file_by_path(&value, "src/linked.rs");
    let outside_link = file_by_path(&value, "outside.rs");

    assert_eq!(linked["is_symlink"], true);
    assert_eq!(linked["language"], "Rust");
    assert_eq!(linked["content"], "text");
    assert_eq!(linked["line_count"], 1);
    assert_eq!(linked["warnings"], Value::Array(Vec::new()));

    assert_eq!(outside_link["is_symlink"], true);
    assert_eq!(outside_link["byte_size"], Value::Null);
    assert_eq!(outside_link["content"], "unknown");
    assert_eq!(outside_link["line_count"], Value::Null);
    assert_eq!(
        outside_link["warnings"][0]["code"],
        "symlink_target_outside_root"
    );
    assert_eq!(value["summary"]["warnings"]["skipped_warnings"], 1);
}
