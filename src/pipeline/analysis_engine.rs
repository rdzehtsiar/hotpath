// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use crate::pipeline::enumerator::{
    enumerate_repository_with_progress, EnumerationError, EnumerationProgress, EnumerationResult,
};

#[derive(Debug, Clone)]
pub struct AnalysisEngine {
    root: PathBuf,
}

impl AnalysisEngine {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scan<F>(&self, progress: F) -> Result<EnumerationResult, EnumerationError>
    where
        F: FnMut(EnumerationProgress),
    {
        enumerate_repository_with_progress(&self.root, progress)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::AnalysisEngine;

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
                .join("analysis-engine-fixtures")
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

    #[test]
    fn scan_delegates_to_enumerator_with_progress() {
        let fixture = Fixture::new("scan");
        fixture.write("main.go", "package main\n");
        fixture.write("nested/worker.go", "package nested\n");
        let engine = AnalysisEngine::new(&fixture.path);
        let mut progress = Vec::new();

        let result = engine
            .scan(|update| progress.push(update))
            .expect("scan should enumerate files");

        assert_eq!(result.files_detected, 2);
        assert_eq!(engine.root(), fixture.path.as_path());
        assert!(progress.len() >= 2);
        assert_eq!(
            progress
                .last()
                .expect("final progress should be emitted")
                .files_detected,
            2
        );
    }
}
