// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRecord {
    pub path: String,
    pub classification: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub status: &'static str,
    pub file_walking: &'static str,
    pub classification: &'static str,
    pub files: Vec<FileRecord>,
}

impl ScanReport {
    fn from_files(files: Vec<FileRecord>) -> Self {
        Self {
            status: "ok",
            file_walking: "implemented",
            classification: "not_implemented",
            files,
        }
    }
}

#[derive(Debug)]
pub enum ScanError {
    CurrentDir(std::io::Error),
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
    Walk(ignore::Error),
    RelativePath {
        root: PathBuf,
        path: PathBuf,
    },
    Json(serde_json::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Root { path, source } => {
                write!(f, "failed to read scan root '{}': {source}", path.display())
            }
            Self::Walk(source) => write!(f, "failed while walking repository files: {source}"),
            Self::RelativePath { root, path } => write!(
                f,
                "failed to make '{}' relative to scan root '{}'",
                path.display(),
                root.display()
            ),
            Self::Json(source) => write!(f, "failed to render scan JSON: {source}"),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDir(source) | Self::Root { source, .. } => Some(source),
            Self::Walk(source) => Some(source),
            Self::RelativePath { .. } => None,
            Self::Json(source) => Some(source),
        }
    }
}

impl From<ignore::Error> for ScanError {
    fn from(source: ignore::Error) -> Self {
        Self::Walk(source)
    }
}

impl From<serde_json::Error> for ScanError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

pub fn scan_current_dir() -> Result<ScanReport, ScanError> {
    let root = env::current_dir().map_err(ScanError::CurrentDir)?;

    scan_repository(root)
}

pub fn scan_repository(root: impl AsRef<Path>) -> Result<ScanReport, ScanError> {
    let root = root.as_ref();
    let root = fs::canonicalize(root).map_err(|source| ScanError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let mut files = Vec::new();

    for entry in WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| !is_git_entry(entry))
        .build()
    {
        let entry = entry?;

        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        files.push(FileRecord {
            path: normalized_relative_path(&root, entry.path())?,
            classification: "not_implemented",
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(ScanReport::from_files(files))
}

pub fn scan_summary() -> Result<String, ScanError> {
    Ok(render_summary(&scan_current_dir()?))
}

pub fn scan_json() -> Result<String, ScanError> {
    Ok(serde_json::to_string_pretty(&scan_current_dir()?)?)
}

fn is_git_entry(entry: &DirEntry) -> bool {
    entry.file_name() == ".git"
}

fn render_summary(scan: &ScanReport) -> String {
    let mut summary = format!(
        "Hotpath scan summary\nstatus: {}\nfile walking: {}\nclassification: {}\nfiles: {}",
        scan.status,
        scan.file_walking,
        scan.classification,
        scan.files.len()
    );

    for file in &scan.files {
        summary.push_str("\n- ");
        summary.push_str(&file.path);
    }

    summary
}

fn normalized_relative_path(root: &Path, path: &Path) -> Result<String, ScanError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ScanError::RelativePath {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
        })?;
    let parts = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            Component::CurDir => None,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                unreachable!("stripped repository-relative paths cannot contain root components")
            }
        })
        .collect::<Vec<_>>();

    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::current_dir()
                .expect("test should have a current directory")
                .join("target")
                .join("test-fixtures")
                .join(format!("{name}-{}", std::process::id()));

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

    fn scanned_paths(root: &Path) -> Vec<String> {
        scan_repository(root)
            .expect("fixture scan should succeed")
            .files
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    #[cfg(unix)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(original, link)
    }

    #[test]
    fn scan_records_are_sorted_by_normalized_relative_path() {
        let fixture = Fixture::new("deterministic-ordering");
        fixture.write("z.rs", "");
        fixture.write(Path::new("nested").join("m.rs"), "");
        fixture.write("a.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec!["a.rs", "nested/m.rs", "z.rs"]
        );
    }

    #[test]
    fn scan_respects_gitignore_patterns() {
        let fixture = Fixture::new("gitignore");
        fixture.write(".gitignore", "ignored/\n*.log\n");
        fixture.write("ignored/file.rs", "");
        fixture.write("keep.rs", "");
        fixture.write("notes.log", "");

        assert_eq!(scanned_paths(&fixture.path), vec![".gitignore", "keep.rs"]);
    }

    #[test]
    fn scan_ignores_non_repository_ignore_sources() {
        let fixture = Fixture::new("deterministic-ignore-sources");
        fixture.write(".ignore", "ignored-by-dot-ignore.rs\n");
        fixture.write(".git/info/exclude", "ignored-by-git-exclude.rs\n");
        fixture.write("ignored-by-dot-ignore.rs", "");
        fixture.write("ignored-by-git-exclude.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec![
                ".ignore",
                "ignored-by-dot-ignore.rs",
                "ignored-by-git-exclude.rs"
            ]
        );
    }

    #[test]
    fn scan_excludes_git_entries_that_are_files() {
        let fixture = Fixture::new("git-file");
        fixture.write(".git", "gitdir: ../linked-worktree.git\n");
        fixture.write("keep.rs", "");

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn scan_does_not_descend_into_symlinked_directories() {
        let fixture = Fixture::new("symlinked-directory");
        let linked = Fixture::new("symlink-target");
        linked.write("nested/secret.rs", "");
        fixture.write("keep.rs", "");

        if symlink_dir(&linked.path, fixture.path.join("linked")).is_err() {
            return;
        }

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn summary_reports_current_scan_boundaries() {
        let scan = ScanReport::from_files(vec![FileRecord {
            path: "src/lib.rs".to_owned(),
            classification: "not_implemented",
        }]);
        let summary = render_summary(&scan);

        assert!(summary.contains("status: ok"));
        assert!(summary.contains("file walking: implemented"));
        assert!(summary.contains("classification: not_implemented"));
        assert!(summary.contains("- src/lib.rs"));
    }

    #[test]
    fn json_reports_file_records() {
        let report = ScanReport::from_files(vec![FileRecord {
            path: "src/lib.rs".to_owned(),
            classification: "not_implemented",
        }]);
        let json = serde_json::to_string_pretty(&report).expect("json should render");

        assert_eq!(
            json,
            "{\n  \"status\": \"ok\",\n  \"file_walking\": \"implemented\",\n  \"classification\": \"not_implemented\",\n  \"files\": [\n    {\n      \"path\": \"src/lib.rs\",\n      \"classification\": \"not_implemented\"\n    }\n  ]\n}"
        );
    }
}
