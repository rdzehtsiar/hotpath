// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use crate::pipeline::enumerator::{
    enumerate_repository_with_callbacks, EnumerationError, EnumerationProgress, EnumerationResult,
};
use crate::pipeline::scheduler::{PipelineTask, Scheduler, SchedulerError, SchedulerOptions};

#[derive(Debug, Clone)]
pub struct AnalysisEngine {
    root: PathBuf,
    options: AnalysisEngineOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisEngineOptions {
    pub scheduler: SchedulerOptions,
}

#[derive(Debug)]
pub enum AnalysisEngineError {
    Enumeration(EnumerationError),
    Scheduler(SchedulerError),
}

impl std::fmt::Display for AnalysisEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enumeration(source) => write!(f, "{source}"),
            Self::Scheduler(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for AnalysisEngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Enumeration(source) => Some(source),
            Self::Scheduler(source) => Some(source),
        }
    }
}

impl From<EnumerationError> for AnalysisEngineError {
    fn from(source: EnumerationError) -> Self {
        Self::Enumeration(source)
    }
}

impl From<SchedulerError> for AnalysisEngineError {
    fn from(source: SchedulerError) -> Self {
        Self::Scheduler(source)
    }
}

impl AnalysisEngine {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_options(root, AnalysisEngineOptions::default())
    }

    pub fn with_options(root: impl Into<PathBuf>, options: AnalysisEngineOptions) -> Self {
        Self {
            root: root.into(),
            options,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn options(&self) -> &AnalysisEngineOptions {
        &self.options
    }

    pub fn scan<F>(&self, progress: F) -> Result<EnumerationResult, AnalysisEngineError>
    where
        F: FnMut(EnumerationProgress),
    {
        self.scan_with_observer(progress, |_| {})
    }

    pub fn scan_with_observer<F, G>(
        &self,
        progress: F,
        mut analysis_completed: G,
    ) -> Result<EnumerationResult, AnalysisEngineError>
    where
        F: FnMut(EnumerationProgress),
        G: FnMut(usize),
    {
        let scheduler = Scheduler::start(self.options.scheduler.clone());
        let result = enumerate_repository_with_callbacks(&self.root, progress, |file| {
            scheduler
                .submit(PipelineTask::AnalyzeFile(file))
                .expect("scheduler should accept tasks while scan is running");
        })
        .map_err(AnalysisEngineError::Enumeration);

        let stats = scheduler.finish().map_err(AnalysisEngineError::Scheduler)?;
        analysis_completed(stats.processed_tasks);

        result
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{AnalysisEngine, AnalysisEngineOptions};
    use crate::pipeline::scheduler::SchedulerOptions;

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

    #[test]
    fn scan_waits_until_dummy_file_analysis_processes_detected_files() {
        let fixture = Fixture::new("waits-for-analysis");
        fixture.write("a.go", "package main\n");
        fixture.write("b.go", "package main\n");
        fixture.write("c.go", "package main\n");
        let engine = AnalysisEngine::with_options(
            &fixture.path,
            AnalysisEngineOptions {
                scheduler: SchedulerOptions {
                    worker_count: 2,
                    queue_capacity: 2,
                },
            },
        );
        let mut analyzed_files = None;

        let result = engine
            .scan_with_observer(|_| {}, |processed| analyzed_files = Some(processed))
            .expect("scan should enumerate and analyze files");

        assert_eq!(result.files_detected, 3);
        assert_eq!(analyzed_files, Some(3));
    }

    #[test]
    fn custom_options_configure_scheduler_defaults() {
        let fixture = Fixture::new("custom-options");
        let engine = AnalysisEngine::with_options(
            &fixture.path,
            AnalysisEngineOptions {
                scheduler: SchedulerOptions {
                    worker_count: 3,
                    queue_capacity: 7,
                },
            },
        );

        assert_eq!(engine.options().scheduler.worker_count, 3);
        assert_eq!(engine.options().scheduler.queue_capacity, 7);
    }
}
