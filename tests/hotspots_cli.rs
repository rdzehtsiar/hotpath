// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use hotpath::storage::index::IndexStore;

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
    failed_stderr_from_output(hotpath(args, current_dir))
}

fn failed_stderr_from_output(output: Output) -> String {
    assert!(
        !output.status.success(),
        "hotpath unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn hotspots_ranks_current_files_and_persists_scan_and_git_inputs() {
    let fixture = GitFixture::new("hotspots-ranked");
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

    let stdout = successful_stdout(&["hotspots"], fixture.path());
    let repeated_stdout = successful_stdout(&["hotspots"], fixture.path());

    assert_eq!(stdout, repeated_stdout);
    assert!(stdout.starts_with("Hotpath hotspots\n"));
    assert!(
        stdout.contains("scope: current scanned files plus local Git history reachable from HEAD")
    );
    assert!(stdout.contains("formula: hotpath.score.v3"));
    assert!(stdout.contains("\nrank  score  path\n"));
    assert!(stdout.contains("key contributors:"));
    assert!(stdout.contains("why:"));
    assert!(stdout.contains("limitations:"));
    assert!(stdout.contains("\ncalculation notes\n"));
    assert!(stdout.contains("Scores are advisory signals for investigation"));
    assert!(!contains_path(&stdout, fixture.path()));

    assert_eq!(
        ranked_paths(&stdout),
        vec!["src/related.rs", "src/risky.rs", "src/stable.rs"]
    );

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
            ("src/related.rs", Some(2)),
            ("src/risky.rs", Some(100)),
            ("src/stable.rs", Some(1)),
        ]
    );

    let persisted_git = IndexStore::open(fixture.path())
        .expect("index should reopen")
        .latest_git_analysis()
        .expect("Git analysis should read")
        .expect("Git analysis should exist");
    assert_eq!(persisted_git.run.head_commit_time, 1712707200);
    assert_eq!(persisted_git.run.recent_window_days, 90);
    assert_eq!(
        persisted_git
            .file_stats
            .iter()
            .map(|stats| (stats.path.as_str(), stats.commits_per_file))
            .collect::<Vec<_>>(),
        vec![
            ("src/related.rs", 2),
            ("src/risky.rs", 3),
            ("src/stable.rs", 1),
        ]
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
        vec![
            (1, "src/related.rs"),
            (2, "src/risky.rs"),
            (3, "src/stable.rs"),
        ]
    );
    assert!(persisted_hotspots
        .iter()
        .all(|hotspot| hotspot.formula_version == "hotpath.score.v3"));
    assert_eq!(hotspot_row_count(fixture.path()), 3);
    let risky_hotspot = persisted_hotspots
        .iter()
        .find(|hotspot| hotspot.path == "src/risky.rs")
        .expect("risky hotspot should be persisted");
    let risky_raw_metrics = serde_json::from_str::<serde_json::Value>(
        risky_hotspot
            .raw_metrics_json
            .as_deref()
            .expect("raw metrics JSON should be stored"),
    )
    .expect("raw metrics JSON should parse");
    assert_eq!(risky_raw_metrics["path"], "src/risky.rs");
    assert_eq!(risky_raw_metrics["line_count"], 100);
    let risky_explanation = serde_json::from_str::<serde_json::Value>(
        risky_hotspot
            .explanation
            .as_deref()
            .expect("explanation JSON should be stored"),
    )
    .expect("explanation JSON should parse");
    assert_eq!(
        risky_explanation["weighted_terms"][0]["formula_version"]["id"],
        "hotpath.score.v3"
    );
}

#[test]
fn hotspots_default_output_remains_top_ten() {
    let fixture = GitFixture::new("hotspots-default-top-ten");
    let author = GitIdentity::new("Default Author", "default@example.invalid");

    for index in 0..12 {
        fixture.write(
            format!("src/file{index:02}.rs"),
            &numbered_lines(&format!("file{index:02}"), 1),
        );
    }
    fixture.commit(CommitOptions::new(
        "Add twelve files",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["hotspots"], fixture.path());

    assert!(stdout.contains("files ranked: 12 (showing 10)"));
    assert_eq!(ranked_paths(&stdout).len(), 10);
    assert!(!stdout.contains("output filters:"));
    assert!(!contains_path(&stdout, fixture.path()));

    assert_eq!(hotspot_row_count(fixture.path()), 12);
}

#[test]
fn hotspots_limit_truncates_displayed_rows_only() {
    let fixture = GitFixture::new("hotspots-limit");
    let author = GitIdentity::new("Limit Author", "limit@example.invalid");

    for index in 0..12 {
        fixture.write(
            format!("src/file{index:02}.rs"),
            &numbered_lines(&format!("file{index:02}"), 1),
        );
    }
    fixture.commit(CommitOptions::new(
        "Add twelve files",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["hotspots", "--limit", "3"], fixture.path());

    assert!(stdout.contains("files ranked: 12 (showing 3)"));
    assert!(stdout.contains("output filters: limit 3"));
    assert_eq!(ranked_paths(&stdout).len(), 3);
    assert!(!contains_path(&stdout, fixture.path()));

    assert_eq!(hotspot_row_count(fixture.path()), 12);
}

#[test]
fn hotspots_excludes_generated_and_vendor_from_output_only() {
    let fixture = GitFixture::new("hotspots-exclude-generated-vendor");
    let author = GitIdentity::new("Filter Author", "filter@example.invalid");

    fixture.write("dist/generated.rs", &numbered_lines("generated", 1000));
    fixture.write("vendor/pkg.rs", &numbered_lines("vendor", 800));
    fixture.write("src/app.rs", "pub fn app() {}\n");
    fixture.commit(CommitOptions::new(
        "Add generated vendor and app files",
        author,
        "2024-01-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(
        &["hotspots", "--exclude-generated", "--exclude-vendor"],
        fixture.path(),
    );
    let rows = ranked_rows(&stdout);

    assert!(stdout.contains("files ranked: 3 (showing 1)"));
    assert!(stdout.contains("output filters: exclude generated files, exclude vendor files"));
    assert!(stdout.contains("Output filters affect displayed rows only"));
    assert!(!stdout.contains("dist/generated.rs"));
    assert!(!stdout.contains("vendor/pkg.rs"));
    assert_eq!(rows, vec![(3, "src/app.rs")]);
    assert!(!contains_path(&stdout, fixture.path()));

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
            (1, "dist/generated.rs"),
            (2, "vendor/pkg.rs"),
            (3, "src/app.rs"),
        ]
    );
}

#[test]
fn hotspots_rejects_zero_limit_before_analysis_or_persistence() {
    let fixture = GitFixture::new("hotspots-zero-limit");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["hotspots", "--limit", "0"], fixture.path());

    assert!(stderr.contains("--limit"));
    assert!(stderr.contains("limit must be greater than 0"));
    assert!(!stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());

    let negative_stderr = failed_stderr(&["hotspots", "--limit", "-1"], fixture.path());

    assert!(negative_stderr.contains("--limit"));
    assert!(negative_stderr.contains("limit must be greater than 0"));
    assert!(!negative_stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&negative_stderr, fixture.path()));
    assert!(!fixture.path().join(".hotpath").exists());
}

#[test]
fn hotspots_uses_repository_root_from_subdirectories_without_path_leaks() {
    let fixture = GitFixture::new("hotspots-subdir");
    let author = GitIdentity::new("Path Author", "path@example.invalid");

    fixture.write("README.md", "# fixture\n");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add fixture files",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["hotspots"], &fixture.path().join("src"));

    assert!(stdout.contains("README.md"));
    assert!(stdout.contains("src/lib.rs"));
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
fn hotspots_reports_empty_current_file_set_after_all_files_are_deleted() {
    let fixture = GitFixture::new("hotspots-empty-current-files");
    let author = GitIdentity::new("Empty Author", "empty@example.invalid");

    fixture.write("src/removed.rs", "pub fn removed() {}\n");
    fixture.commit(CommitOptions::new(
        "Add file",
        author.clone(),
        "2024-01-01T00:00:00 +0000",
    ));
    fixture.delete("src/removed.rs");
    fixture.commit(CommitOptions::new(
        "Remove file",
        author,
        "2024-02-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["hotspots"], fixture.path());

    assert!(stdout.contains("files ranked: 0 (showing 0)"));
    assert!(stdout.contains("\nrank  score  path\n  none\n\ncalculation notes"));
    assert!(!stdout.contains("src/removed.rs"));
    assert!(!contains_path(&stdout, fixture.path()));

    let persisted = IndexStore::open(fixture.path())
        .expect("index should open")
        .latest_scan()
        .expect("scan should read")
        .expect("scan should exist");
    assert!(persisted.files.is_empty());
    let persisted_hotspots = IndexStore::open(fixture.path())
        .expect("index should reopen for hotspots")
        .latest_hotspots()
        .expect("hotspots should read");
    assert!(persisted_hotspots.is_empty());
    assert_eq!(hotspot_row_count(fixture.path()), 0);
}

#[test]
fn hotspots_renders_zero_contributors_for_current_file_without_git_history() {
    let fixture = GitFixture::new("hotspots-zero-contributors");
    let author = GitIdentity::new("Tracked Author", "tracked@example.invalid");

    fixture.write("src/tracked.rs", "pub fn tracked() {}\n");
    fixture.commit(CommitOptions::new(
        "Add tracked file",
        author,
        "2024-03-01T00:00:00 +0000",
    ));
    fixture.write("src/untracked_empty.rs", "");

    let stdout = successful_stdout(&["hotspots"], fixture.path());
    let section = row_section(&stdout, "src/untracked_empty.rs");

    assert!(section.contains("key contributors: none observed"));
    assert!(section.contains(
        "why: 0 commits, 0 churn lines, 0 recent churn lines, 0 authors, 0 owners, 0 co-changed files, 0 lines"
    ));
    assert!(section.contains(
        "limitations: dominant operational owner share is unavailable; owner risk uses owner count only"
    ));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn hotspots_renders_byte_size_summary_and_limitations_for_binary_file() {
    let fixture = GitFixture::new("hotspots-byte-size-summary");
    let author = GitIdentity::new("Binary Author", "binary@example.invalid");

    fixture.write("assets/blob.bin", "abc\0def\n");
    fixture.commit(CommitOptions::new(
        "Add binary blob",
        author,
        "2024-04-01T00:00:00 +0000",
    ));

    let stdout = successful_stdout(&["hotspots"], fixture.path());
    let section = row_section(&stdout, "assets/blob.bin");

    assert!(section.contains("why:"));
    assert!(section.contains("8 bytes"));
    assert!(section
        .contains("limitations: line count is unavailable; size normalization uses byte size"));
    assert!(section.contains("recent growth normalization is omitted"));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn hotspots_renders_unknown_size_summary_when_current_file_has_no_size() {
    let fixture = GitFixture::new("hotspots-unknown-size-summary");
    let author = GitIdentity::new("Symlink Author", "symlink@example.invalid");

    fixture.write("src/tracked.rs", "pub fn tracked() {}\n");
    fixture.commit(CommitOptions::new(
        "Add tracked file",
        author,
        "2024-05-01T00:00:00 +0000",
    ));

    if create_symlink_or_skip(
        fixture.path().join("missing-target.rs"),
        fixture.path().join("src").join("missing_link.rs"),
    )
    .is_err()
    {
        return;
    }

    let stdout = successful_stdout(&["hotspots"], fixture.path());
    let section = row_section(&stdout, "src/missing_link.rs");

    assert!(section.contains("size unavailable"));
    assert!(section.contains(
        "limitations: line count and byte size are unavailable; size normalization is omitted"
    ));
    assert!(!contains_path(&stdout, fixture.path()));
}

#[test]
fn hotspots_rejects_non_git_directory_without_report_output_or_path_leak() {
    let fixture = TempDir::new("hotspots-non-git");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["hotspots"], fixture.path());

    assert!(stderr.starts_with("hotpath: path is not a readable Git worktree"));
    assert!(stderr.contains("run hotspots from inside a repository"));
    assert!(!stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn hotspots_rejects_missing_head_without_report_output_or_path_leak() {
    let fixture = GitFixture::new("hotspots-missing-head");
    fixture.write("src/lib.rs", "pub fn lib() {}\n");

    let stderr = failed_stderr(&["hotspots"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository does not have a commit at HEAD"));
    assert!(stderr.contains("create an initial commit before analyzing hotspots"));
    assert!(!stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn hotspots_rejects_shallow_repository_without_report_output_or_path_leak() {
    let fixture = GitFixture::new("hotspots-shallow");
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

    let stderr = failed_stderr(&["hotspots"], fixture.path());

    assert!(stderr.starts_with("hotpath: Git repository has shallow history"));
    assert!(stderr.contains("fetch complete local history"));
    assert!(!stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&stderr, fixture.path()));
    assert!(fixture.path().join(".hotpath").join("logs").exists());
    assert!(!fixture.path().join(".hotpath").join("index.db").exists());
}

#[test]
fn hotspots_sanitizes_persistence_errors_without_fixture_path_leak() {
    let fixture = GitFixture::new("hotspots-corrupt-index");
    let author = GitIdentity::new("Index Author", "index@example.invalid");

    fixture.write("src/lib.rs", "pub fn lib() {}\n");
    fixture.commit(CommitOptions::new(
        "Add library",
        author,
        "2024-07-01T00:00:00 +0000",
    ));
    fixture.write(".hotpath/index.db", "not a sqlite database");

    let stderr = failed_stderr(&["hotspots"], fixture.path());

    assert!(stderr.starts_with(
        "hotpath: failed to persist scan results in local Hotpath index (.hotpath/index.db):"
    ));
    assert!(stderr.contains("remove .hotpath/index.db"));
    assert!(!stderr.contains("Hotpath hotspots"));
    assert!(!contains_path(&stderr, fixture.path()));
}

fn numbered_lines(prefix: &str, count: usize) -> String {
    (0..count)
        .map(|index| format!("pub fn {prefix}_{index}() {{}}\n"))
        .collect()
}

fn ranked_paths(stdout: &str) -> Vec<&str> {
    ranked_rows(stdout)
        .into_iter()
        .map(|(_rank, path)| path)
        .collect()
}

fn ranked_rows(stdout: &str) -> Vec<(u64, &str)> {
    stdout
        .lines()
        .filter_map(|line| {
            let parts = line.split_whitespace().collect::<Vec<_>>();

            if parts.len() >= 3
                && parts[0].parse::<u64>().is_ok()
                && parts[1].parse::<f64>().is_ok()
            {
                Some((
                    parts[0].parse::<u64>().expect("rank should parse"),
                    parts[2],
                ))
            } else {
                None
            }
        })
        .collect()
}

fn row_section<'a>(stdout: &'a str, path: &str) -> &'a str {
    let lines = stdout.lines().collect::<Vec<_>>();
    let row_index = lines
        .iter()
        .position(|line| line.split_whitespace().nth(2) == Some(path))
        .unwrap_or_else(|| panic!("expected hotspot row for {path}\n{stdout}"));
    let end_index = lines
        .iter()
        .enumerate()
        .skip(row_index + 1)
        .find(|(_index, line)| {
            line.split_whitespace()
                .next()
                .and_then(|part| part.parse::<u64>().ok())
                .is_some()
                || line.starts_with("calculation notes")
        })
        .map_or(lines.len(), |(index, _line)| index);

    &stdout[byte_offset_for_line(stdout, row_index)..byte_offset_for_line(stdout, end_index)]
}

fn byte_offset_for_line(text: &str, line_index: usize) -> usize {
    if line_index == 0 {
        return 0;
    }

    text.match_indices('\n')
        .nth(line_index - 1)
        .map_or(text.len(), |(index, _newline)| index + 1)
}

fn hotspot_row_count(root: &Path) -> i64 {
    let connection = rusqlite::Connection::open(root.join(".hotpath").join("index.db"))
        .expect("index database should open");

    connection
        .query_row("SELECT COUNT(*) FROM hotspots;", [], |row| row.get(0))
        .expect("hotspot row count should read")
}

fn contains_path(output: &str, path: &Path) -> bool {
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

    candidates
        .iter()
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| output.contains(candidate))
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
    ) || error.raw_os_error() == Some(1314)
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
            .join("hotspots-fixtures")
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
