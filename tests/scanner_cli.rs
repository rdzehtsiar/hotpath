// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[test]
fn scan_prints_two_line_progress_summary() {
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

    assert_eq!(final_lines.len(), 2);
    assert!(final_lines[0].starts_with("enumerated files 2 | speed "));
    assert!(final_lines[0].ends_with(" files/sec"));
    assert!(final_lines[1].contains("analyzed files"));
    assert!(final_lines[1].contains("2/2"));
    assert!(final_lines[1].contains("remaining 0"));
    assert!(final_lines[1].ends_with(" files/sec"));
}

#[test]
fn scan_respects_ignore_rules_in_file_count() {
    let fixture = Fixture::new("scan-ignore");
    fixture.write(".gitignore", "ignored.go\n");
    fixture.write("kept.go", "package main\n");
    fixture.write("ignored.go", "package ignored\n");

    let output = hotpath(&["scan"], &fixture.path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let final_lines = final_scan_lines(&stdout);
    assert!(final_lines[0].starts_with("enumerated files 2 | speed "));
    assert!(final_lines[1].contains("2/2"));
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

fn final_scan_lines(stdout: &str) -> Vec<String> {
    let sanitized = strip_ansi(stdout);
    let lines: Vec<_> = sanitized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    lines[lines.len().saturating_sub(2)..].to_vec()
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
