// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::pipeline::enumerator::{
    enumerate_repository_with_callbacks, EnumerationError, EnumerationResult,
};
use crate::pipeline::events::{PipelineEvent, PipelineState};
use crate::pipeline::file_analyzer::FileAnalyzerOptions;
use crate::pipeline::reporter::{NoopReporter, PipelineReporter};
use crate::pipeline::scheduler::{PipelineTask, Scheduler, SchedulerError, SchedulerOptions};

#[derive(Debug, Clone)]
pub struct AnalysisEngine {
    root: PathBuf,
    options: AnalysisEngineOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisEngineOptions {
    pub scheduler: SchedulerOptions,
    pub file_analyzer: FileAnalyzerOptions,
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

    pub fn scan(&self) -> Result<EnumerationResult, AnalysisEngineError> {
        let mut reporter = NoopReporter;
        self.scan_with_reporter(&mut reporter)
    }

    pub fn scan_with_reporter<R>(
        &self,
        reporter: &mut R,
    ) -> Result<EnumerationResult, AnalysisEngineError>
    where
        R: PipelineReporter,
    {
        let mut latest_state = PipelineState::default();
        let result = self.scan_with_event_observer(|state, _event| {
            latest_state = state.clone();
            reporter.update(state);
        });
        reporter.finish(&latest_state);
        result
    }

    pub fn scan_with_event_observer<F>(
        &self,
        observer: F,
    ) -> Result<EnumerationResult, AnalysisEngineError>
    where
        F: FnMut(&PipelineState, &PipelineEvent),
    {
        let mut scheduler_options = self.options.scheduler.clone();
        scheduler_options.file_analyzer = self.options.file_analyzer.clone();
        let (event_sender, event_receiver) = mpsc::channel();
        let scheduler = Scheduler::start_with_events(scheduler_options, Some(event_sender));
        let dispatcher = RefCell::new(EventDispatcher::new(observer));
        dispatcher.borrow_mut().emit(PipelineEvent::ScanStarted);

        let result = enumerate_repository_with_callbacks(
            &self.root,
            |progress| {
                dispatcher
                    .borrow_mut()
                    .emit(PipelineEvent::EnumerationProgress {
                        files_detected: progress.files_detected,
                        entries_walked: progress.entries_walked,
                        elapsed: progress.elapsed,
                    });
                drain_scheduler_events(&event_receiver, &dispatcher);
            },
            |file| {
                dispatcher.borrow_mut().emit(PipelineEvent::FileDiscovered {
                    path: file.path.clone(),
                });
                scheduler
                    .submit(PipelineTask::AnalyzeFile(file))
                    .expect("scheduler should accept tasks while scan is running");
                drain_scheduler_events(&event_receiver, &dispatcher);
            },
        )
        .map_err(AnalysisEngineError::Enumeration);

        if let Ok(result) = &result {
            dispatcher
                .borrow_mut()
                .emit(PipelineEvent::EnumerationCompleted {
                    result: result.clone(),
                });
        }

        let (finish_sender, finish_receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = finish_sender.send(scheduler.finish());
        });
        let stats = loop {
            match finish_receiver.try_recv() {
                Ok(stats) => break stats.map_err(AnalysisEngineError::Scheduler)?,
                Err(mpsc::TryRecvError::Empty) => {
                    receive_scheduler_event(&event_receiver, &dispatcher);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(AnalysisEngineError::Scheduler(
                        SchedulerError::WorkerPanicked,
                    ));
                }
            }
        };
        drain_scheduler_events(&event_receiver, &dispatcher);

        if let Ok(result) = &result {
            dispatcher.borrow_mut().emit(PipelineEvent::ScanCompleted {
                files_detected: result.files_detected,
                analyzed_files: stats.processed_tasks as u64,
            });
        }

        result
    }
}

struct EventDispatcher<F> {
    state: PipelineState,
    observer: F,
}

impl<F> EventDispatcher<F>
where
    F: FnMut(&PipelineState, &PipelineEvent),
{
    fn new(observer: F) -> Self {
        Self {
            state: PipelineState::default(),
            observer,
        }
    }

    fn emit(&mut self, event: PipelineEvent) {
        self.state.apply(&event);
        (self.observer)(&self.state, &event);
    }
}

fn drain_scheduler_events<F>(
    event_receiver: &Receiver<PipelineEvent>,
    dispatcher: &RefCell<EventDispatcher<F>>,
) where
    F: FnMut(&PipelineState, &PipelineEvent),
{
    for event in event_receiver.try_iter() {
        dispatcher.borrow_mut().emit(event);
    }
}

fn receive_scheduler_event<F>(
    event_receiver: &Receiver<PipelineEvent>,
    dispatcher: &RefCell<EventDispatcher<F>>,
) where
    F: FnMut(&PipelineState, &PipelineEvent),
{
    match event_receiver.recv_timeout(Duration::from_millis(50)) {
        Ok(event) => dispatcher.borrow_mut().emit(event),
        Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{AnalysisEngine, AnalysisEngineOptions};
    use crate::pipeline::events::PipelineEvent;
    use crate::pipeline::file_analyzer::FileAnalyzerOptions;
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
    fn scan_emits_enumeration_events() {
        let fixture = Fixture::new("scan");
        fixture.write("main.go", "package main\n");
        fixture.write("nested/worker.go", "package nested\n");
        let engine = AnalysisEngine::new(&fixture.path);
        let mut events = Vec::new();

        let result = engine
            .scan_with_event_observer(|state, event| {
                events.push((state.clone(), event.clone()));
            })
            .expect("scan should enumerate files");

        assert_eq!(result.files_detected, 2);
        assert_eq!(engine.root(), fixture.path.as_path());
        assert!(events
            .iter()
            .any(|(_, event)| matches!(event, PipelineEvent::EnumerationProgress { .. })));
        assert!(events
            .iter()
            .any(|(_, event)| matches!(event, PipelineEvent::EnumerationCompleted { .. })));
        assert_eq!(
            events
                .last()
                .expect("scan completion should be emitted")
                .0
                .enumerated_files,
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
                    file_analyzer: FileAnalyzerOptions::default(),
                },
                file_analyzer: FileAnalyzerOptions::default(),
            },
        );
        let mut final_analyzed_files = None;

        let result = engine
            .scan_with_event_observer(|state, event| {
                if matches!(event, PipelineEvent::ScanCompleted { .. }) {
                    final_analyzed_files = Some(state.analyzed_files);
                }
            })
            .expect("scan should enumerate and analyze files");

        assert_eq!(result.files_detected, 3);
        assert_eq!(final_analyzed_files, Some(3));
    }

    #[test]
    fn scan_completion_is_emitted_after_analysis_finishes() {
        let fixture = Fixture::new("completion-order");
        fixture.write("a.go", "package main\n");
        fixture.write("b.go", "package main\n");
        let engine = AnalysisEngine::with_options(
            &fixture.path,
            AnalysisEngineOptions {
                scheduler: SchedulerOptions {
                    worker_count: 1,
                    queue_capacity: 1,
                    file_analyzer: FileAnalyzerOptions::default(),
                },
                file_analyzer: FileAnalyzerOptions::default(),
            },
        );
        let mut completed_state = None;

        engine
            .scan_with_event_observer(|state, event| {
                if matches!(event, PipelineEvent::ScanCompleted { .. }) {
                    completed_state = Some(state.clone());
                }
            })
            .expect("scan should complete");

        let completed_state = completed_state.expect("scan completion should be emitted");
        assert_eq!(completed_state.total_files, Some(2));
        assert_eq!(completed_state.analyzed_files, 2);
        assert_eq!(completed_state.remaining_files(), Some(0));
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
                    file_analyzer: FileAnalyzerOptions::default(),
                },
                file_analyzer: FileAnalyzerOptions {
                    content_window_bytes: 128,
                    parsers: Vec::new(),
                },
            },
        );

        assert_eq!(engine.options().scheduler.worker_count, 3);
        assert_eq!(engine.options().scheduler.queue_capacity, 7);
        assert_eq!(engine.options().file_analyzer.content_window_bytes, 128);
    }
}
