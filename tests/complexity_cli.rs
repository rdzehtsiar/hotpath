// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;
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

fn complexity_source() -> String {
    let mut source = String::from(
        "pub struct Widget;\n\
         \n\
         impl Widget {\n\
         \tpub fn render(&self, value: i32) {\n\
         \t\tif value > 0 {\n\
         \t\t\tfor item in 0..value {\n\
         \t\t\t\tmatch item {\n\
         \t\t\t\t\t0 => {}\n\
         \t\t\t\t\t_ => {}\n\
         \t\t\t\t}\n\
         \t\t\t}\n\
         \t\t}\n\
         \t}\n\
         }\n\
         \n\
         pub fn large() {\n",
    );

    for index in 0..78 {
        source.push_str(&format!("\tlet value = {index};\n"));
    }

    source.push_str("}\n");
    source
}

fn symbol_by_name<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["symbols"]
        .as_array()
        .expect("symbols should be an array")
        .iter()
        .find(|symbol| symbol["name"] == name)
        .unwrap_or_else(|| panic!("expected symbol named {name}"))
}

fn file_by_path<'a>(value: &'a Value, path: &str) -> &'a Value {
    value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("expected file record for {path}"))
}

fn assert_no_path_leaks(value: &Value, fixture_path: &Path) {
    let rendered = value.to_string();
    let path = fixture_path.display().to_string();
    let path_forward = path.replace('\\', "/");

    assert!(!rendered.contains(&path), "JSON leaked fixture path");
    assert!(
        !rendered.contains(&path_forward),
        "JSON leaked normalized fixture path"
    );
}

fn assert_text_does_not_contain_path(text: &str, fixture_path: &Path) {
    let path = fixture_path.display().to_string();
    let path_forward = path.replace('\\', "/");

    assert!(!text.contains(&path), "text leaked fixture path");
    assert!(
        !text.contains(&path_forward),
        "text leaked normalized fixture path"
    );
}

#[test]
fn complexity_json_reports_summary_files_symbols_and_persists_parse_symbols() {
    let fixture = Fixture::new("complexity-json");
    fixture.write("src/lib.rs", &complexity_source());

    let first_stdout = successful_stdout(&["complexity", "--json"], &fixture.path);
    let second_stdout = successful_stdout(&["complexity", "--json"], &fixture.path);
    let value: Value = serde_json::from_str(&first_stdout).expect("complexity JSON should parse");

    assert_eq!(first_stdout, second_stdout);
    assert_no_path_leaks(&value, &fixture.path);
    assert_eq!(value["schema_version"], "hotpath.complexity.v1");
    assert_eq!(value["summary"]["total_files"], 1);
    assert_eq!(value["summary"]["parsed_files"], 1);
    assert_eq!(value["summary"]["symbol_count"], 4);
    assert_eq!(value["summary"]["function_method_count"], 2);
    assert_eq!(value["summary"]["large_symbol_count"], 1);
    assert_eq!(value["summary"]["max_cyclomatic_complexity"], 4);
    assert_eq!(value["summary"]["max_nesting_depth"], 3);
    assert_eq!(value["summary"]["dependency_edge_count"], 0);
    assert_eq!(value["summary"]["max_fan_in"], 0);
    assert_eq!(value["summary"]["max_fan_out"], 0);
    assert_eq!(value["files"][0]["path"], "src/lib.rs");
    assert_eq!(value["files"][0]["status"], "parsed");
    assert_eq!(value["files"][0]["function_method_count"], 2);
    assert_eq!(value["files"][0]["large_symbol_count"], 1);
    assert_eq!(value["files"][0]["max_cyclomatic_complexity"], 4);
    assert_eq!(value["files"][0]["max_nesting_depth"], 3);
    assert_eq!(value["files"][0]["fan_in"], 0);
    assert_eq!(value["files"][0]["fan_out"], 0);

    let render = symbol_by_name(&value, "render");
    assert_eq!(render["path"], "src/lib.rs");
    assert_eq!(render["kind"], "method");
    assert_eq!(render["length_lines"], 10);
    assert_eq!(render["function_length_lines"], 10);
    assert_eq!(render["cyclomatic_complexity"], 4);
    assert_eq!(render["max_control_flow_nesting"], 3);
    assert_eq!(render["is_large_symbol"].as_bool(), Some(false));

    let large = symbol_by_name(&value, "large");
    assert_eq!(large["kind"], "function");
    assert_eq!(large["length_lines"], 80);
    assert_eq!(large["function_length_lines"], 80);
    assert_eq!(large["is_large_symbol"].as_bool(), Some(true));

    let widget = symbol_by_name(&value, "Widget");
    assert!(widget["function_length_lines"].is_null());
    assert!(widget["cyclomatic_complexity"].is_null());

    let store = IndexStore::open(&fixture.path).expect("index should open");
    let symbols = store.latest_symbols().expect("symbols should read");
    assert_eq!(symbols.len(), 4);
    assert!(symbols
        .iter()
        .any(|symbol| symbol.path == "src/lib.rs" && symbol.name == "render"));
}

#[test]
fn complexity_summary_is_concise_ranked_and_does_not_require_git() {
    let fixture = Fixture::new("complexity-summary");
    fixture.write("src/lib.rs", &complexity_source());

    let stdout = successful_stdout(&["complexity"], &fixture.path);

    assert!(stdout.contains("Hotpath complexity summary"));
    assert!(stdout.contains("total files        1"));
    assert!(stdout.contains("parsed files       1"));
    assert!(stdout.contains("functions/methods  2"));
    assert!(stdout.contains("large symbols      1"));
    assert!(stdout.contains("max cyclomatic     4"));
    assert!(stdout.contains("max nesting        3"));
    assert!(stdout.contains("dependency edges   0"));
    assert!(stdout.contains("max fan-in         0"));
    assert!(stdout.contains("max fan-out        0"));
    assert!(stdout.contains("most complex function/method symbols"));
    assert!(stdout.contains("src/lib.rs:4  method  render"));
    assert!(stdout.contains("src/lib.rs:16  function  large"));
    assert!(
        stdout
            .find("src/lib.rs:4  method  render")
            .expect("render should be ranked")
            < stdout
                .find("src/lib.rs:16  function  large")
                .expect("large should be ranked")
    );
}

#[test]
fn complexity_json_reports_file_fan_metrics_and_persists_dependencies() {
    let fixture = Fixture::new("complexity-dependencies");
    fixture.write(
        "src/lib.rs",
        concat!(
            "mod child;\n",
            "use crate::models::Widget;\n",
            "pub fn lib() {}\n"
        ),
    );
    fixture.write("src/child.rs", "pub fn child() {}\n");
    fixture.write("src/models/mod.rs", "pub struct Widget;\n");

    let stdout = successful_stdout(&["complexity", "--json"], &fixture.path);
    let value: Value = serde_json::from_str(&stdout).expect("complexity JSON should parse");

    assert_eq!(value["summary"]["dependency_edge_count"], 2);
    assert_eq!(value["summary"]["max_fan_in"], 1);
    assert_eq!(value["summary"]["max_fan_out"], 2);
    assert_eq!(file_by_path(&value, "src/lib.rs")["fan_in"], 0);
    assert_eq!(file_by_path(&value, "src/lib.rs")["fan_out"], 2);
    assert_eq!(file_by_path(&value, "src/child.rs")["fan_in"], 1);
    assert_eq!(file_by_path(&value, "src/child.rs")["fan_out"], 0);
    assert_eq!(file_by_path(&value, "src/models/mod.rs")["fan_in"], 1);
    assert_eq!(file_by_path(&value, "src/models/mod.rs")["fan_out"], 0);

    let store = IndexStore::open(&fixture.path).expect("index should open");
    assert_eq!(
        store.dependency_count().expect("dependencies should count"),
        2
    );
}

#[test]
fn complexity_sanitizes_persistence_errors_without_fixture_path_leak() {
    let fixture = Fixture::new("complexity-corrupt-index");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.write(".hotpath/index.db", "not a sqlite database");

    let stderr = failed_stderr(&["complexity"], &fixture.path);

    assert!(stderr.starts_with(
        "hotpath: failed to persist scan results in local Hotpath index (.hotpath/index.db):"
    ));
    assert!(stderr.contains("remove .hotpath/index.db"));
    assert!(!stderr.contains("Hotpath complexity summary"));
    assert_text_does_not_contain_path(&stderr, &fixture.path);
}
