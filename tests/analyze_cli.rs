// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::{Command, Output};

use hotpath::storage::index::IndexStore;

mod support;

use support::git::{CommitOptions, GitFixture, GitIdentity};

fn hotpath(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hotpath"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("hotpath binary should run")
}

#[test]
fn analyze_persists_complete_observation_index() {
    let fixture = GitFixture::new("analyze-index");
    let ada = GitIdentity::new("Ada Lovelace", "ada@example.invalid");

    fixture.write("src/lib.rs", "pub fn answer() -> u32 { 42 }\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        ada,
        "2024-01-01T00:00:00 +0000",
    ));

    let output = hotpath(&["analyze"], fixture.path());

    assert!(
        output.status.success(),
        "hotpath analyze failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("Hotpath analysis complete"));
    assert!(stdout.contains("Index: .hotpath/index.db"));

    let store = IndexStore::open_read_only(fixture.path()).expect("index should open read-only");
    let scan = store
        .latest_scan()
        .expect("scan should read")
        .expect("scan should exist");
    assert_eq!(scan.files.len(), 1);
    assert_eq!(scan.files[0].path, "src/lib.rs");
    assert_eq!(
        store
            .latest_hotspot_page(0, 10, false, None)
            .expect("hotspots should read")
            .total,
        1
    );
    assert!(!store
        .latest_symbols()
        .expect("symbols should read")
        .is_empty());
}
