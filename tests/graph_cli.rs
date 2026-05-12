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

fn dependency_fixture(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    fixture.write(
        "src/lib.rs",
        concat!(
            "mod auth;\n",
            "use crate::models::User;\n",
            "pub fn lib() {}\n"
        ),
    );
    fixture.write(
        "src/auth.rs",
        concat!("use crate::models::User;\n", "pub fn login() {}\n"),
    );
    fixture.write("src/models.rs", "pub struct User;\n");
    fixture
}

fn assert_no_path_leaks_in_json(value: &Value, fixture_path: &Path) {
    let rendered = value.to_string();
    let path = fixture_path.display().to_string();
    let path_forward = path.replace('\\', "/");

    assert!(!rendered.contains(&path), "JSON leaked fixture path");
    assert!(
        !rendered.contains(&path_forward),
        "JSON leaked normalized fixture path"
    );
}

fn assert_no_path_leaks_in_text(text: &str, fixture_path: &Path) {
    let path = fixture_path.display().to_string();
    let path_forward = path.replace('\\', "/");

    assert!(!text.contains(&path), "text leaked fixture path");
    assert!(
        !text.contains(&path_forward),
        "text leaked normalized fixture path"
    );
}

#[test]
fn graph_summary_reports_one_hop_dependencies_without_git() {
    let fixture = dependency_fixture("graph-summary");

    let stdout = successful_stdout(&["graph", "--module", "auth"], &fixture.path);

    assert!(stdout.contains("Hotpath dependency graph"));
    assert!(stdout.contains("selector       auth"));
    assert!(stdout.contains("matched files  1"));
    assert!(stdout.contains("outgoing       1"));
    assert!(stdout.contains("incoming       1"));
    assert!(stdout.contains("\nmatched files\n  src/auth.rs"));
    assert!(stdout.contains("\noutgoing\n  src/auth.rs -> src/models.rs  use"));
    assert!(stdout.contains("\nincoming\n  src/lib.rs -> src/auth.rs  mod"));
    assert!(!fixture.path.join(".git").exists());
    assert_no_path_leaks_in_text(&stdout, &fixture.path);
}

#[test]
fn graph_json_reports_schema_summary_and_edges() {
    let fixture = dependency_fixture("graph-json");

    let first_stdout = successful_stdout(&["graph", "--module", "auth", "--json"], &fixture.path);
    let second_stdout = successful_stdout(&["graph", "--module", "auth", "--json"], &fixture.path);
    let value: Value = serde_json::from_str(&first_stdout).expect("graph JSON should parse");

    assert_eq!(first_stdout, second_stdout);
    assert_no_path_leaks_in_json(&value, &fixture.path);
    assert_eq!(value["schema_version"], "hotpath.graph.v1");
    assert_eq!(value["selector"], "auth");
    assert_eq!(value["summary"]["matched_file_count"], 1);
    assert_eq!(value["summary"]["outgoing_edge_count"], 1);
    assert_eq!(value["summary"]["incoming_edge_count"], 1);
    assert_eq!(value["matched_files"], serde_json::json!(["src/auth.rs"]));
    assert_eq!(value["outgoing"][0]["source_path"], "src/auth.rs");
    assert_eq!(value["outgoing"][0]["target_path"], "src/models.rs");
    assert_eq!(value["outgoing"][0]["kind"], "use");
    assert_eq!(value["incoming"][0]["source_path"], "src/lib.rs");
    assert_eq!(value["incoming"][0]["target_path"], "src/auth.rs");
    assert_eq!(value["incoming"][0]["kind"], "mod");

    let store = IndexStore::open(&fixture.path).expect("index should open");
    assert_eq!(
        store.dependency_count().expect("dependencies should count"),
        3
    );
}

#[test]
fn graph_path_selector_matches_repository_relative_prefix() {
    let fixture = dependency_fixture("graph-path-selector");

    let stdout = successful_stdout(&["graph", "--module", "src"], &fixture.path);

    assert!(stdout.contains("matched files  3"));
    assert!(stdout.contains("src/auth.rs"));
    assert!(stdout.contains("src/lib.rs"));
    assert!(stdout.contains("src/models.rs"));
    assert_no_path_leaks_in_text(&stdout, &fixture.path);
}

#[test]
fn graph_path_selector_does_not_match_similar_prefixes() {
    let fixture = Fixture::new("graph-path-selector-boundary");
    fixture.write("src/auth/mod.rs", "pub fn auth() {}\n");
    fixture.write("src/auth/login.rs", "pub fn login() {}\n");
    fixture.write("src/authz.rs", "pub fn authz() {}\n");
    fixture.write("src/authentication/mod.rs", "pub fn authentication() {}\n");

    let stdout = successful_stdout(&["graph", "--module", "src/auth"], &fixture.path);

    assert!(stdout.contains("selector       src/auth"));
    assert!(stdout.contains("matched files  2"));
    assert!(stdout.contains("\n  src/auth/login.rs"));
    assert!(stdout.contains("\n  src/auth/mod.rs"));
    assert!(!stdout.contains("src/authz.rs"));
    assert!(!stdout.contains("src/authentication/mod.rs"));
    assert_no_path_leaks_in_text(&stdout, &fixture.path);
}

#[test]
fn graph_no_match_succeeds_with_empty_sections() {
    let fixture = dependency_fixture("graph-no-match");

    let stdout = successful_stdout(&["graph", "--module", "missing"], &fixture.path);

    assert!(stdout.contains("selector       missing"));
    assert!(stdout.contains("matched files  0"));
    assert!(stdout.contains("outgoing       0"));
    assert!(stdout.contains("incoming       0"));
    assert!(stdout.contains("\nmatched files\n  none"));
    assert!(stdout.contains("\noutgoing\n  none"));
    assert!(stdout.contains("\nincoming\n  none"));
    assert_no_path_leaks_in_text(&stdout, &fixture.path);
}

#[test]
fn graph_json_leaves_unresolved_imports_unpersisted() {
    let fixture = Fixture::new("graph-unresolved-imports");
    fixture.write(
        "src/lib.rs",
        concat!(
            "mod child;\n",
            "use crate::auth::User;\n",
            "use crate::{fmt, io};\n",
            "use std::fmt;\n"
        ),
    );
    fixture.write("src/child.rs", "pub fn child() {}\n");
    fixture.write("src/child/mod.rs", "pub fn child_mod() {}\n");
    fixture.write("src/auth.rs", "pub struct User;\n");
    fixture.write("src/auth/mod.rs", "pub struct User;\n");
    fixture.write(
        "web/app.ts",
        concat!(
            "import { value } from \"./value\";\n",
            "import { outside } from \"../outside\";\n",
            "import React from \"react\";\n"
        ),
    );
    fixture.write("web/value.ts", "export const value = 1;\n");
    fixture.write("web/value.tsx", "export const value = 1;\n");
    fixture.write("README.md", "# Unsupported\n");

    let parse_stdout = successful_stdout(&["parse", "--json"], &fixture.path);
    let parse_value: Value = serde_json::from_str(&parse_stdout).expect("parse JSON should parse");
    assert_eq!(parse_value["summary"]["import_count"], 7);

    let stdout = successful_stdout(&["graph", "--module", "src", "--json"], &fixture.path);
    let value: Value = serde_json::from_str(&stdout).expect("graph JSON should parse");

    assert_eq!(value["summary"]["outgoing_edge_count"], 0);
    assert_eq!(value["summary"]["incoming_edge_count"], 0);
    assert_eq!(value["outgoing"], serde_json::json!([]));
    assert_eq!(value["incoming"], serde_json::json!([]));

    let store = IndexStore::open(&fixture.path).expect("index should open");
    assert_eq!(
        store.dependency_count().expect("dependencies should count"),
        0
    );
}

#[test]
fn graph_requires_module_before_creating_index() {
    let fixture = Fixture::new("graph-missing-module");

    let stderr = failed_stderr(&["graph"], &fixture.path);

    assert!(stderr.contains("--module"));
    assert!(!fixture.path.join(".hotpath").exists());
}
