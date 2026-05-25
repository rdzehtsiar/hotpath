// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::{Command, Output};

fn hotpath(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hotpath"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("hotpath binary should run")
}

#[test]
fn top_level_help_lists_analyze_and_tui_commands() {
    let output = hotpath(&["--help"], Path::new(env!("CARGO_MANIFEST_DIR")));

    assert!(
        output.status.success(),
        "hotpath --help failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");

    assert!(stdout.contains("analyze"));
    assert!(stdout.contains("Build or refresh the local analysis index"));
    assert!(stdout.contains("tui"));
    assert!(stdout.contains("Open the read-only terminal viewer for an existing local index"));
}

#[test]
fn tui_help_describes_read_only_viewer() {
    let output = hotpath(&["tui", "--help"], Path::new(env!("CARGO_MANIFEST_DIR")));

    assert!(
        output.status.success(),
        "hotpath tui --help failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");

    assert!(stdout.contains("Open the read-only terminal viewer for an existing local index"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("tui"));
    assert!(stdout.contains("--help"));
}

#[test]
fn analyze_help_describes_index_refresh() {
    let output = hotpath(
        &["analyze", "--help"],
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );

    assert!(
        output.status.success(),
        "hotpath analyze --help failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");

    assert!(stdout.contains("Build or refresh the local analysis index"));
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("analyze"));
}
