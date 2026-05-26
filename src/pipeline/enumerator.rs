// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::{DirEntry, Error as IgnoreError, WalkBuilder};

const PROGRESS_ENTRY_INTERVAL: u64 = 512;
const PROGRESS_TIME_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq)]
pub struct EnumerationProgress {
    pub files_detected: u64,
    pub entries_walked: u64,
    pub elapsed: Duration,
}

impl EnumerationProgress {
    pub fn files_per_second(&self) -> f64 {
        files_per_second(self.files_detected, self.elapsed)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumerationResult {
    pub root: PathBuf,
    pub files_detected: u64,
    pub entries_walked: u64,
    pub elapsed: Duration,
}

impl EnumerationResult {
    pub fn files_per_second(&self) -> f64 {
        files_per_second(self.files_detected, self.elapsed)
    }
}

#[derive(Debug)]
pub enum EnumerationError {
    CurrentDir(std::io::Error),
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
    RootNotDirectory {
        path: PathBuf,
    },
    Walk {
        path: Option<PathBuf>,
        source: IgnoreError,
    },
}

impl fmt::Display for EnumerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Root { path, source } => {
                write!(
                    f,
                    "failed to access scan root '{}': {source}",
                    path.display()
                )
            }
            Self::RootNotDirectory { path } => {
                write!(f, "scan root '{}' is not a directory", path.display())
            }
            Self::Walk {
                path: Some(path),
                source,
            } => {
                write!(
                    f,
                    "failed while walking repository entry '{}': {source}",
                    path.display()
                )
            }
            Self::Walk { path: None, source } => {
                write!(f, "failed while walking repository entries: {source}")
            }
        }
    }
}

impl StdError for EnumerationError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) | Self::Root { source, .. } => Some(source),
            Self::RootNotDirectory { .. } => None,
            Self::Walk { source, .. } => Some(source),
        }
    }
}

pub fn enumerate_repository(root: impl AsRef<Path>) -> Result<EnumerationResult, EnumerationError> {
    enumerate_repository_with_progress(root, |_| {})
}

pub fn enumerate_repository_with_progress<F>(
    root: impl AsRef<Path>,
    mut progress: F,
) -> Result<EnumerationResult, EnumerationError>
where
    F: FnMut(EnumerationProgress),
{
    let started = Instant::now();
    let requested_root = root.as_ref();
    let root = fs::canonicalize(requested_root).map_err(|source| EnumerationError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(&root).map_err(|source| EnumerationError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(EnumerationError::RootNotDirectory {
            path: requested_root.to_path_buf(),
        });
    }

    fs::read_dir(&root).map_err(|source| EnumerationError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;

    let internal_filter_root = root.clone();
    let mut files_detected = 0;
    let mut entries_walked = 0;
    let mut last_progress = Instant::now();

    progress(EnumerationProgress {
        files_detected,
        entries_walked,
        elapsed: started.elapsed(),
    });

    for entry in WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(move |entry| !is_internal_entry(&internal_filter_root, entry))
        .build()
    {
        let entry = entry.map_err(|source| EnumerationError::Walk {
            path: ignore_error_path(&source).map(Path::to_path_buf),
            source,
        })?;
        entries_walked += 1;

        if is_walked_file(&entry) {
            files_detected += 1;
        }

        if entries_walked.is_multiple_of(PROGRESS_ENTRY_INTERVAL)
            || last_progress.elapsed() >= PROGRESS_TIME_INTERVAL
        {
            progress(EnumerationProgress {
                files_detected,
                entries_walked,
                elapsed: started.elapsed(),
            });
            last_progress = Instant::now();
        }
    }

    let result = EnumerationResult {
        root,
        files_detected,
        entries_walked,
        elapsed: started.elapsed(),
    };
    progress(EnumerationProgress {
        files_detected: result.files_detected,
        entries_walked: result.entries_walked,
        elapsed: result.elapsed,
    });

    Ok(result)
}

fn files_per_second(files_detected: u64, elapsed: Duration) -> f64 {
    let elapsed_seconds = elapsed.as_secs_f64();
    if elapsed_seconds <= f64::EPSILON {
        return files_detected as f64;
    }

    files_detected as f64 / elapsed_seconds
}

fn is_internal_entry(root: &Path, entry: &DirEntry) -> bool {
    match entry.file_name().to_str() {
        Some(".git") => true,
        Some(".hotpath") => entry.path() == root.join(".hotpath"),
        _ => false,
    }
}

fn is_walked_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|file_type| {
        file_type.is_file()
            || (file_type.is_symlink()
                && fs::metadata(entry.path())
                    .map(|metadata| metadata.is_file())
                    .unwrap_or(true))
    })
}

fn ignore_error_path(error: &IgnoreError) -> Option<&Path> {
    let mut stack = vec![error];

    while let Some(error) = stack.pop() {
        match error {
            IgnoreError::Partial(errors) => {
                for error in errors.iter().rev() {
                    stack.push(error);
                }
            }
            IgnoreError::WithLineNumber { err, .. } | IgnoreError::WithDepth { err, .. } => {
                stack.push(err);
            }
            IgnoreError::WithPath { path, .. } => return Some(path),
            IgnoreError::Loop { child, .. } => return Some(child),
            IgnoreError::Io(_)
            | IgnoreError::Glob { .. }
            | IgnoreError::UnrecognizedFileType(_)
            | IgnoreError::InvalidDefinition => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{enumerate_repository, enumerate_repository_with_progress};

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::current_dir()
                .expect("test should have current directory")
                .join("target")
                .join("enumerator-fixtures")
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

        fn mkdir(&self, relative_path: impl AsRef<Path>) {
            fs::create_dir_all(self.path.join(relative_path))
                .expect("fixture directory should be created");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn counts_normal_and_nested_files() {
        let fixture = Fixture::new("normal-files");
        fixture.write("a.go", "package main\n");
        fixture.write("nested/b.go", "package nested\n");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert_eq!(result.files_detected, 2);
        assert!(result.entries_walked >= 3);
    }

    #[test]
    fn respects_gitignore() {
        let fixture = Fixture::new("gitignore");
        fixture.write(".gitignore", "ignored.txt\n");
        fixture.write("kept.go", "package main\n");
        fixture.write("ignored.txt", "ignored\n");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert_eq!(result.files_detected, 2);
    }

    #[test]
    fn skips_root_git_and_hotpath_directories() {
        let fixture = Fixture::new("internal-dirs");
        fixture.write("src/main.go", "package main\n");
        fixture.write(".git/config", "ignored\n");
        fixture.write(".hotpath/index.db", "ignored\n");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert_eq!(result.files_detected, 1);
    }

    #[test]
    fn includes_hidden_files_outside_internal_directories() {
        let fixture = Fixture::new("hidden-files");
        fixture.write(".env", "KEY=value\n");
        fixture.write(".config/settings.toml", "");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert_eq!(result.files_detected, 2);
    }

    #[test]
    fn ignores_directories_without_counting_them_as_files() {
        let fixture = Fixture::new("directories");
        fixture.mkdir("empty");
        fixture.write("file.go", "package main\n");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert_eq!(result.files_detected, 1);
    }

    #[test]
    fn reports_files_per_second() {
        let fixture = Fixture::new("speed");
        fixture.write("a.go", "package main\n");

        let result = enumerate_repository(&fixture.path).expect("enumeration should succeed");

        assert!(result.files_per_second().is_finite());
        assert!(result.files_per_second() >= 0.0);
    }

    #[test]
    fn reports_initial_and_final_progress() {
        let fixture = Fixture::new("progress");
        fixture.write("a.go", "package main\n");
        fixture.write("b.go", "package main\n");
        let mut progress = Vec::new();

        let result = enumerate_repository_with_progress(&fixture.path, |update| {
            progress.push(update);
        })
        .expect("enumeration should succeed");

        assert!(progress.len() >= 2);
        assert_eq!(progress[0].files_detected, 0);
        assert_eq!(progress[0].entries_walked, 0);
        let final_progress = progress.last().expect("final progress should be emitted");
        assert_eq!(final_progress.files_detected, result.files_detected);
        assert_eq!(final_progress.entries_walked, result.entries_walked);
    }
}
