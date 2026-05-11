// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn parse_json(current_dir: &Path) -> (String, Value) {
    let stdout = successful_stdout(&["parse", "--json"], current_dir);
    let value = serde_json::from_str(&stdout).expect("parse JSON should parse");

    (stdout, value)
}

fn file_paths(value: &Value) -> Vec<&str> {
    value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .map(|file| file["path"].as_str().expect("path should be a string"))
        .collect()
}

fn file_by_path<'a>(value: &'a Value, path: &str) -> &'a Value {
    value["files"]
        .as_array()
        .expect("files should be an array")
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("expected parse file record for {path}"))
}

fn assert_no_path_leaks(value: &Value, fixture_path: &Path) {
    assert_json_strings_do_not_contain_path(value, fixture_path);
    assert!(file_paths(value).iter().all(|path| !path.contains('\\')));
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
        "parse JSON leaked fixture path in string values: {leaks:?}"
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

#[test]
fn parse_json_language_fixture_snapshot_is_stable() {
    let fixture = Fixture::new("parse-language-snapshot");
    fixture.write(
        "src/lib.rs",
        concat!(
            "use crate::models::Widget;\n",
            "mod nested;\n",
            "\n",
            "pub struct Widget {\n",
            "    pub value: i32,\n",
            "}\n",
            "\n",
            "impl Widget {\n",
            "    pub fn render(&self) {\n",
            "        if self.value > 0 {\n",
            "            for item in 0..self.value {\n",
            "                match item {\n",
            "                    0 => {}\n",
            "                    _ => {}\n",
            "                }\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
            "\n",
            "fn helper() {}\n",
        ),
    );
    fixture.write(
        "cmd/main.go",
        concat!(
            "package main\n",
            "\n",
            "import (\n",
            "    \"fmt\"\n",
            "    alias \"net/http\"\n",
            ")\n",
            "\n",
            "type Server struct {\n",
            "    Name string\n",
            "}\n",
            "\n",
            "func NewServer() *Server {\n",
            "    return &Server{}\n",
            "}\n",
            "\n",
            "func (s *Server) Serve() {\n",
            "    if s.Name != \"\" {\n",
            "        for range []int{1} {\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    );
    fixture.write(
        "web/app.ts",
        concat!(
            "import { value } from \"./value\";\n",
            "\n",
            "namespace UI {\n",
            "    export interface Props {\n",
            "        onClick(): void;\n",
            "    }\n",
            "\n",
            "    export type Mode = \"a\" | \"b\";\n",
            "\n",
            "    export class Button {\n",
            "        render() {\n",
            "            if (value) {\n",
            "                while (false) {}\n",
            "            }\n",
            "        }\n",
            "    }\n",
            "}\n",
            "\n",
            "export function helper() {\n",
            "    return value ? 1 : 2;\n",
            "}\n",
        ),
    );
    fixture.write(
        "web/App.tsx",
        concat!(
            "import React from \"react\";\n",
            "\n",
            "type Props = {\n",
            "    title: string;\n",
            "};\n",
            "\n",
            "export function App(props: Props) {\n",
            "    return <section>{props.title ? <h1>{props.title}</h1> : null}</section>;\n",
            "}\n",
        ),
    );

    let (first_stdout, value) = parse_json(&fixture.path);
    let (second_stdout, second_value) = parse_json(&fixture.path);

    assert_eq!(first_stdout, second_stdout);
    assert_eq!(value, second_value);
    assert_no_path_leaks(&value, &fixture.path);
    assert_eq!(value["schema_version"], "hotpath.parse.v1");
    assert_eq!(
        value["summary"],
        json!({
            "total_files": 4,
            "candidate_files": 4,
            "parsed_files": 4,
            "pending_files": 0,
            "skipped_files": 0,
            "symbol_count": 18,
            "import_count": 6,
            "warning_count": 0
        })
    );
    assert_eq!(value["warnings"], json!([]));
    assert_eq!(
        value["files"],
        json!([
            {
                "path": "cmd/main.go",
                "language": "Go",
                "content": "text",
                "status": "parsed",
                "reason": null,
                "symbol_count": 4,
                "import_count": 2
            },
            {
                "path": "src/lib.rs",
                "language": "Rust",
                "content": "text",
                "status": "parsed",
                "reason": null,
                "symbol_count": 5,
                "import_count": 2
            },
            {
                "path": "web/App.tsx",
                "language": "TypeScript JSX",
                "content": "text",
                "status": "parsed",
                "reason": null,
                "symbol_count": 2,
                "import_count": 1
            },
            {
                "path": "web/app.ts",
                "language": "TypeScript",
                "content": "text",
                "status": "parsed",
                "reason": null,
                "symbol_count": 7,
                "import_count": 1
            }
        ])
    );
    assert_eq!(
        value["imports"],
        json!([
            {
                "path": "cmd/main.go",
                "target": "fmt",
                "kind": "import",
                "start_line": 4,
                "end_line": 4
            },
            {
                "path": "cmd/main.go",
                "target": "net/http",
                "kind": "import",
                "start_line": 5,
                "end_line": 5
            },
            {
                "path": "src/lib.rs",
                "target": "crate::models::Widget",
                "kind": "use",
                "start_line": 1,
                "end_line": 1
            },
            {
                "path": "src/lib.rs",
                "target": "nested",
                "kind": "mod",
                "start_line": 2,
                "end_line": 2
            },
            {
                "path": "web/App.tsx",
                "target": "react",
                "kind": "import",
                "start_line": 1,
                "end_line": 1
            },
            {
                "path": "web/app.ts",
                "target": "./value",
                "kind": "import",
                "start_line": 1,
                "end_line": 1
            }
        ])
    );
    assert_eq!(
        value["symbols"],
        json!([
            {
                "path": "cmd/main.go",
                "name": "main",
                "kind": "package",
                "start_line": 1,
                "end_line": 1,
                "signature": "package main",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "cmd/main.go",
                "name": "Server",
                "kind": "struct",
                "start_line": 8,
                "end_line": 10,
                "signature": "Server",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "cmd/main.go",
                "name": "NewServer",
                "kind": "function",
                "start_line": 12,
                "end_line": 14,
                "signature": "func NewServer() *Server",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            },
            {
                "path": "cmd/main.go",
                "name": "Serve",
                "kind": "method",
                "start_line": 16,
                "end_line": 21,
                "signature": "func (s *Server) Serve()",
                "nesting_depth": 0,
                "parent": "Server",
                "cyclomatic_complexity": 3,
                "max_control_flow_nesting": 2
            },
            {
                "path": "src/lib.rs",
                "name": "nested",
                "kind": "module",
                "start_line": 2,
                "end_line": 2,
                "signature": "mod nested",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "src/lib.rs",
                "name": "Widget",
                "kind": "struct",
                "start_line": 4,
                "end_line": 6,
                "signature": "pub struct Widget",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "src/lib.rs",
                "name": "impl Widget",
                "kind": "impl",
                "start_line": 8,
                "end_line": 19,
                "signature": "impl Widget",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "src/lib.rs",
                "name": "render",
                "kind": "method",
                "start_line": 9,
                "end_line": 18,
                "signature": "pub fn render(&self)",
                "nesting_depth": 1,
                "parent": "impl Widget",
                "cyclomatic_complexity": 4,
                "max_control_flow_nesting": 3
            },
            {
                "path": "src/lib.rs",
                "name": "helper",
                "kind": "function",
                "start_line": 21,
                "end_line": 21,
                "signature": "fn helper()",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            },
            {
                "path": "web/App.tsx",
                "name": "Props",
                "kind": "type_alias",
                "start_line": 3,
                "end_line": 5,
                "signature": "type Props =",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "web/App.tsx",
                "name": "App",
                "kind": "function",
                "start_line": 7,
                "end_line": 9,
                "signature": "function App(props: Props)",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 2,
                "max_control_flow_nesting": 1
            },
            {
                "path": "web/app.ts",
                "name": "UI",
                "kind": "namespace",
                "start_line": 3,
                "end_line": 17,
                "signature": "namespace UI",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "web/app.ts",
                "name": "Props",
                "kind": "interface",
                "start_line": 4,
                "end_line": 6,
                "signature": "interface Props",
                "nesting_depth": 1,
                "parent": "UI",
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "web/app.ts",
                "name": "onClick",
                "kind": "method",
                "start_line": 5,
                "end_line": 5,
                "signature": "onClick(): void",
                "nesting_depth": 2,
                "parent": "UI::Props",
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            },
            {
                "path": "web/app.ts",
                "name": "Mode",
                "kind": "type_alias",
                "start_line": 8,
                "end_line": 8,
                "signature": "type Mode = \"a\" | \"b\"",
                "nesting_depth": 1,
                "parent": "UI",
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "web/app.ts",
                "name": "Button",
                "kind": "class",
                "start_line": 10,
                "end_line": 16,
                "signature": "class Button",
                "nesting_depth": 1,
                "parent": "UI",
                "cyclomatic_complexity": null,
                "max_control_flow_nesting": null
            },
            {
                "path": "web/app.ts",
                "name": "render",
                "kind": "method",
                "start_line": 11,
                "end_line": 15,
                "signature": "render()",
                "nesting_depth": 2,
                "parent": "UI::Button",
                "cyclomatic_complexity": 3,
                "max_control_flow_nesting": 2
            },
            {
                "path": "web/app.ts",
                "name": "helper",
                "kind": "function",
                "start_line": 19,
                "end_line": 21,
                "signature": "function helper()",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 2,
                "max_control_flow_nesting": 1
            }
        ])
    );
}

#[test]
fn parse_json_edge_fixture_covers_skips_warnings_ignore_and_flags() {
    let fixture = Fixture::new("parse-edge-snapshot");
    fixture.write(".gitignore", "ignored/\n");
    fixture.write("ignored/secret.ts", "export function ignored() {}\n");
    fixture.write("README.md", "# Fixture\n");
    fixture.write_bytes("assets/logo.rs", &[0, b'R', b'S']);
    fixture.write_bytes("bad/invalid.ts", &[b'e', b'x', b'p', 0xff, b'\n']);
    fixture.write(
        "build/client.generated.ts",
        "export function generated() {}\n",
    );
    fixture.write(
        "node_modules/pkg/index.ts",
        "export function vendored() {}\n",
    );
    fixture.write(
        "src/broken.rs",
        concat!("fn recovered() {}\n", "\n", "fn broken( {\n"),
    );

    let (_, value) = parse_json(&fixture.path);

    assert_no_path_leaks(&value, &fixture.path);
    assert_eq!(value["schema_version"], "hotpath.parse.v1");
    assert_eq!(
        file_paths(&value),
        vec![
            ".gitignore",
            "README.md",
            "assets/logo.rs",
            "bad/invalid.ts",
            "build/client.generated.ts",
            "node_modules/pkg/index.ts",
            "src/broken.rs",
        ]
    );
    assert!(file_paths(&value)
        .iter()
        .all(|path| !path.starts_with("ignored/")));
    assert_eq!(
        value["summary"],
        json!({
            "total_files": 7,
            "candidate_files": 3,
            "parsed_files": 3,
            "pending_files": 0,
            "skipped_files": 4,
            "symbol_count": 3,
            "import_count": 0,
            "warning_count": 2
        })
    );
    assert_eq!(
        value["warnings"],
        json!([
            {
                "code": "unsupported_encoding",
                "path": "bad/invalid.ts",
                "message": "file contents are not valid UTF-8"
            },
            {
                "code": "syntax_error",
                "path": "src/broken.rs",
                "message": "syntax errors detected near line 3; extracted symbols may be incomplete"
            }
        ])
    );
    assert_eq!(
        file_by_path(&value, ".gitignore"),
        &json!({
            "path": ".gitignore",
            "language": null,
            "content": "text",
            "status": "skipped",
            "reason": "unsupported_language",
            "symbol_count": 0,
            "import_count": 0
        })
    );
    assert_eq!(
        file_by_path(&value, "README.md"),
        &json!({
            "path": "README.md",
            "language": "Markdown",
            "content": "text",
            "status": "skipped",
            "reason": "unsupported_language",
            "symbol_count": 0,
            "import_count": 0
        })
    );
    assert_eq!(
        file_by_path(&value, "assets/logo.rs"),
        &json!({
            "path": "assets/logo.rs",
            "language": "Rust",
            "content": "binary",
            "status": "skipped",
            "reason": "unsupported_content",
            "symbol_count": 0,
            "import_count": 0
        })
    );
    assert_eq!(
        file_by_path(&value, "bad/invalid.ts"),
        &json!({
            "path": "bad/invalid.ts",
            "language": "TypeScript",
            "content": "unknown",
            "status": "skipped",
            "reason": "unsupported_content",
            "symbol_count": 0,
            "import_count": 0
        })
    );
    assert_eq!(
        file_by_path(&value, "build/client.generated.ts")["status"],
        "parsed"
    );
    assert_eq!(
        file_by_path(&value, "node_modules/pkg/index.ts")["status"],
        "parsed"
    );
    assert_eq!(file_by_path(&value, "src/broken.rs")["status"], "parsed");
    assert_eq!(
        value["symbols"],
        json!([
            {
                "path": "build/client.generated.ts",
                "name": "generated",
                "kind": "function",
                "start_line": 1,
                "end_line": 1,
                "signature": "function generated()",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            },
            {
                "path": "node_modules/pkg/index.ts",
                "name": "vendored",
                "kind": "function",
                "start_line": 1,
                "end_line": 1,
                "signature": "function vendored()",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            },
            {
                "path": "src/broken.rs",
                "name": "recovered",
                "kind": "function",
                "start_line": 1,
                "end_line": 1,
                "signature": "fn recovered()",
                "nesting_depth": 0,
                "parent": null,
                "cyclomatic_complexity": 1,
                "max_control_flow_nesting": 0
            }
        ])
    );
    assert_eq!(value["imports"], json!([]));
}
