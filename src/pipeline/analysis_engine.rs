// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::pipeline::enumerator::{
    enumerate_repository, enumerate_repository_with_callbacks, EnumerationError, EnumerationResult,
};
use crate::pipeline::events::{PipelineEvent, PipelineState};
use crate::pipeline::file_analyzer::{file_analyzer_options_signature, FileAnalyzerOptions};
use crate::pipeline::git_history_analyzer::{
    collect_git_plan, git_options_signature, is_ancestor, revision_commit_count,
    GitHistoryAnalyzerOptions, GitHistoryScan,
};
use crate::pipeline::reporter::{NoopReporter, PipelineReporter};
use crate::pipeline::scheduler::{
    PipelineTask, Scheduler, SchedulerError, SchedulerHandle, SchedulerOptions,
};
use crate::pipeline::store_reducer::{
    load_file_reuse_index, read_scan_state, GitRepositorySummaryInput, StoreReducer,
    StoreReducerError, StoreReducerOptions,
};

#[derive(Debug, Clone)]
pub struct AnalysisEngine {
    root: PathBuf,
    options: AnalysisEngineOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisEngineOptions {
    pub scheduler: SchedulerOptions,
    pub file_analyzer: FileAnalyzerOptions,
    pub store_reducer: StoreReducerOptions,
}

#[derive(Debug)]
pub enum AnalysisEngineError {
    Enumeration(EnumerationError),
    Scheduler(SchedulerError),
    Store(StoreReducerError),
}

impl std::fmt::Display for AnalysisEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enumeration(source) => write!(f, "{source}"),
            Self::Scheduler(source) => write!(f, "{source}"),
            Self::Store(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for AnalysisEngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Enumeration(source) => Some(source),
            Self::Scheduler(source) => Some(source),
            Self::Store(source) => Some(source),
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

impl From<StoreReducerError> for AnalysisEngineError {
    fn from(source: StoreReducerError) -> Self {
        Self::Store(source)
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
        let git_history_options = scheduler_options.git_history_analyzer.clone();
        let scan_started = Instant::now();
        let active_scan_id = current_scan_id();
        let (event_sender, event_receiver) = mpsc::channel();
        let total_counter = spawn_total_counter(self.root.clone(), event_sender.clone());
        let mut store_options = self.options.store_reducer.clone();
        store_options.active_scan_id = active_scan_id;
        let store_reducer = StoreReducer::start(&self.root, store_options, event_sender.clone())?;
        let store_handle = store_reducer.handle();
        let file_analysis_signature = file_analyzer_options_signature(&self.options.file_analyzer);
        let file_reuse_index =
            load_file_reuse_index(&self.root, &file_analysis_signature).unwrap_or_default();
        let scheduler = Scheduler::start_with_events(
            scheduler_options,
            Some(event_sender.clone()),
            Some(store_handle.clone()),
        );
        let git_planner = spawn_git_planner(
            self.root.clone(),
            scheduler.handle(),
            store_handle.clone(),
            event_sender.clone(),
            git_history_options,
        );
        let dispatcher = RefCell::new(EventDispatcher::new(observer));
        let reused_file_count = Cell::new(0_u64);
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
                let reused = file_reuse_index.is_current(&file.path);
                if reused {
                    let reused_files = reused_file_count.get() + 1;
                    reused_file_count.set(reused_files);
                    let _ = store_handle.mark_file_reused(file.path);
                    let processed = scheduler.stats().processed_file_tasks as u64 + reused_files;
                    dispatcher
                        .borrow_mut()
                        .emit(PipelineEvent::FileAnalysisCompleted {
                            analyzed_files: processed,
                            elapsed: scan_started.elapsed(),
                        });
                } else {
                    scheduler
                        .submit(PipelineTask::AnalyzeFile(file))
                        .expect("scheduler should accept tasks while scan is running");
                }
                drain_scheduler_events(&event_receiver, &dispatcher);
            },
        )
        .map_err(AnalysisEngineError::Enumeration);

        if let Ok(result) = &result {
            let _ = store_handle.plan_file_records(result.files_detected);
            dispatcher
                .borrow_mut()
                .emit(PipelineEvent::EnumerationCompleted {
                    result: result.clone(),
                });
        }

        let _ = git_planner.join();
        drain_scheduler_events(&event_receiver, &dispatcher);

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
        let _ = total_counter.join();
        drain_scheduler_events(&event_receiver, &dispatcher);
        dispatcher
            .borrow_mut()
            .emit(PipelineEvent::GitHistoryCompleted {
                processed_commits: stats.processed_git_commits as u64,
                elapsed: scan_started.elapsed(),
            });
        let _ = store_handle.store_metadata(
            "files_detected",
            result
                .as_ref()
                .map(|result| result.files_detected)
                .unwrap_or_default()
                .to_string(),
        );
        let _ = store_handle.store_metadata(
            "files_analyzed",
            (stats.processed_file_tasks as u64 + reused_file_count.get()).to_string(),
        );
        let _ = store_handle.store_metadata(
            "git_commits_processed",
            stats.processed_git_commits.to_string(),
        );
        let _ = store_handle.store_metadata(
            "git_scan_commits_processed",
            stats.processed_git_commits.to_string(),
        );
        let _ = store_handle.mark_unseen_files_inactive();
        let _ = store_handle.store_scan_state(
            "schema_version",
            crate::pipeline::store_reducer::INDEX_SCHEMA_VERSION,
        );
        let _ = store_handle.store_scan_state("file_analysis_signature", file_analysis_signature);
        let _ = store_handle.store_scan_state("root", self.root.to_string_lossy());
        let _ = store_handle.store_scan_state("last_scan_completed", "1");
        let _ = store_handle.plan_finalization_records();

        let (store_finish_sender, store_finish_receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = store_finish_sender.send(store_reducer.finish());
        });
        loop {
            match store_finish_receiver.try_recv() {
                Ok(stats) => {
                    let _store_stats = stats.map_err(AnalysisEngineError::Store)?;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    receive_scheduler_event(&event_receiver, &dispatcher);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(AnalysisEngineError::Store(
                        StoreReducerError::ThreadPanicked,
                    ));
                }
            }
        }
        drain_scheduler_events(&event_receiver, &dispatcher);

        if let Ok(result) = &result {
            dispatcher.borrow_mut().emit(PipelineEvent::ScanCompleted {
                files_detected: result.files_detected,
                analyzed_files: stats.processed_file_tasks as u64 + reused_file_count.get(),
                git_commits_processed: stats.processed_git_commits as u64,
                elapsed: scan_started.elapsed(),
            });
        }

        result
    }
}

fn spawn_git_planner(
    root: PathBuf,
    scheduler: SchedulerHandle,
    store_handle: crate::pipeline::store_reducer::StoreReducerHandle,
    event_sender: mpsc::Sender<PipelineEvent>,
    git_history_options: GitHistoryAnalyzerOptions,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = event_sender.send(PipelineEvent::GitPlanningStarted);
        let previous_state = read_scan_state(&root).unwrap_or_default();
        let options_signature = git_options_signature(&git_history_options);
        match collect_git_plan(&root, &git_history_options) {
            Ok(None) => {
                let _ = store_handle.clear_git_data();
                let _ = store_handle.store_git_repository_summary(GitRepositorySummaryInput {
                    is_skipped: true,
                    skip_reason: Some("not a git worktree".to_owned()),
                    ..GitRepositorySummaryInput::default()
                });
                let _ = store_handle.store_metadata("git_mode", "skipped_not_git");
                let _ = store_handle.store_metadata("git_scan_mode", "skipped_not_git");
                let _ = store_handle.store_metadata("git_collection_mode", "unavailable");
                let _ = store_handle.plan_finalization_records();
                let _ = event_sender.send(PipelineEvent::GitHistorySkipped {
                    reason: "not a git worktree".to_owned(),
                });
            }
            Ok(Some(plan)) if plan.is_shallow => {
                let _ = store_handle.clear_git_data();
                let _ = store_handle.store_git_repository_summary(GitRepositorySummaryInput {
                    head_commit: plan.head_commit,
                    head_timestamp: plan.head_timestamp,
                    total_commits: plan.total_commits.unwrap_or_default(),
                    is_shallow: true,
                    is_skipped: true,
                    skip_reason: Some("shallow repository".to_owned()),
                });
                let _ = store_handle.store_metadata("git_mode", "skipped_shallow");
                let _ = store_handle.store_metadata("git_scan_mode", "skipped_shallow");
                let _ = store_handle.store_metadata("git_collection_mode", "unavailable");
                let _ = store_handle.plan_finalization_records();
                let _ = event_sender.send(PipelineEvent::GitHistorySkipped {
                    reason: "shallow repository".to_owned(),
                });
            }
            Ok(Some(plan)) => {
                let head_timestamp = plan.head_timestamp.unwrap_or_default();
                let current_head = plan.head_commit.clone();
                let mut git_scan_mode = "full".to_owned();
                let mut revision = None;
                let mut planned_git_commits = plan.total_commits;
                let mut should_run_git = true;
                let previous_head = previous_state.last_indexed_head.as_deref();
                let options_match = previous_state.git_options_signature.as_deref()
                    == Some(options_signature.as_str());

                if previous_state.completed && options_match {
                    if let (Some(previous_head), Some(current_head)) =
                        (previous_head, current_head.as_deref())
                    {
                        if previous_head == current_head {
                            git_scan_mode = "up_to_date".to_owned();
                            should_run_git = false;
                        } else if is_ancestor(&root, previous_head, current_head).unwrap_or(false) {
                            git_scan_mode = "incremental".to_owned();
                            let range = format!("{previous_head}..{current_head}");
                            planned_git_commits = revision_commit_count(&root, &range).ok();
                            revision = Some(range);
                        } else {
                            git_scan_mode = "fallback_full".to_owned();
                            let _ = store_handle.clear_git_data();
                        }
                    } else {
                        git_scan_mode = "fallback_full".to_owned();
                        let _ = store_handle.clear_git_data();
                    }
                } else {
                    let _ = store_handle.clear_git_data();
                }

                let _ = store_handle.store_git_repository_summary(GitRepositorySummaryInput {
                    head_commit: current_head.clone(),
                    head_timestamp: plan.head_timestamp,
                    total_commits: plan.total_commits.unwrap_or_default(),
                    is_shallow: false,
                    is_skipped: false,
                    skip_reason: None,
                });
                let _ = store_handle.store_metadata("git_mode", git_scan_mode.clone());
                let _ = store_handle.store_metadata("git_scan_mode", git_scan_mode.clone());
                let _ = store_handle.store_metadata("git_collection_mode", "bounded_recent_stream");
                let _ = store_handle.store_metadata(
                    "git_max_commits",
                    git_history_options
                        .max_commits
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unbounded".to_owned()),
                );
                let _ = store_handle.store_metadata(
                    "git_max_age_days",
                    git_history_options
                        .max_age_days
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unbounded".to_owned()),
                );
                let _ = store_handle.store_metadata(
                    "git_cochange_max_files_per_commit",
                    git_history_options
                        .cochange_max_files_per_commit
                        .to_string(),
                );
                if let Some(total_commits) = planned_git_commits {
                    let _ = event_sender.send(PipelineEvent::GitPlanningCompleted {
                        total_commits,
                        total_chunks: 1,
                    });
                }

                if should_run_git {
                    let scan = GitHistoryScan {
                        root: root.clone(),
                        revision,
                        head_timestamp,
                        max_commits: if git_scan_mode == "incremental" {
                            None
                        } else {
                            git_history_options.max_commits
                        },
                        max_age_days: if git_scan_mode == "incremental" {
                            None
                        } else {
                            git_history_options.max_age_days
                        },
                        cochange_max_files_per_commit: git_history_options
                            .cochange_max_files_per_commit,
                        delta_batch_size: git_history_options.delta_batch_size,
                    };
                    if scheduler
                        .submit(PipelineTask::AnalyzeGitHistory(scan))
                        .is_err()
                    {
                        let _ = event_sender.send(PipelineEvent::GitHistorySkipped {
                            reason: "scheduler closed before Git planning completed".to_owned(),
                        });
                    }
                } else {
                    let _ = store_handle.plan_finalization_records();
                    let _ = event_sender.send(PipelineEvent::GitHistorySkipped {
                        reason: "Git already indexed at current HEAD".to_owned(),
                    });
                }
                if let Some(current_head) = current_head {
                    let _ = store_handle.store_scan_state("last_indexed_head", current_head);
                }
                let _ = store_handle.store_scan_state("git_options_signature", options_signature);
            }
            Err(error) => {
                let _ = store_handle.clear_git_data();
                let _ = store_handle.store_git_repository_summary(GitRepositorySummaryInput {
                    is_skipped: true,
                    skip_reason: Some(error.to_string()),
                    ..GitRepositorySummaryInput::default()
                });
                let _ = store_handle.store_metadata("git_mode", "skipped_error");
                let _ = store_handle.store_metadata("git_scan_mode", "skipped_error");
                let _ = store_handle.store_metadata("git_collection_mode", "unavailable");
                let _ = store_handle.plan_finalization_records();
                let _ = event_sender.send(PipelineEvent::GitHistorySkipped {
                    reason: error.to_string(),
                });
            }
        }
    })
}

fn spawn_total_counter(
    root: PathBuf,
    event_sender: mpsc::Sender<PipelineEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Ok(result) = enumerate_repository(root) {
            let _ = event_sender.send(PipelineEvent::TotalFilesCounted {
                files_detected: result.files_detected,
                elapsed: result.elapsed,
            });
        }
    })
}

fn current_scan_id() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
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

    use rusqlite::Connection;

    use super::{AnalysisEngine, AnalysisEngineOptions};
    use crate::pipeline::events::PipelineEvent;
    use crate::pipeline::file_analyzer::FileAnalyzerOptions;
    use crate::pipeline::git_history_analyzer::GitHistoryAnalyzerOptions;
    use crate::pipeline::scheduler::SchedulerOptions;
    use crate::pipeline::store_reducer::StoreReducerOptions;

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
            .any(|(_, event)| matches!(event, PipelineEvent::TotalFilesCounted { .. })));
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
                    git_history_analyzer: GitHistoryAnalyzerOptions::default(),
                    git_max_concurrency: 1,
                },
                file_analyzer: FileAnalyzerOptions::default(),
                store_reducer: StoreReducerOptions::default(),
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
        let connection = Connection::open(fixture.path.join(".hotpath").join("index.sqlite"))
            .expect("db should open");
        assert_eq!(row_count(&connection, "file_analysis"), 3);
        assert_eq!(row_count(&connection, "file_facts"), 3);
    }

    #[test]
    fn second_scan_reuses_unchanged_file_rows() {
        let fixture = Fixture::new("incremental-file-reuse");
        fixture.write("a.go", "package main\n");
        let engine = AnalysisEngine::new(&fixture.path);

        engine.scan().expect("first scan should complete");
        let connection = Connection::open(fixture.path.join(".hotpath").join("index.sqlite"))
            .expect("db should open");
        let first_scan_id = scalar_i64(
            &connection,
            "SELECT active_scan_id FROM file_analysis WHERE path LIKE '%a.go'",
        );
        drop(connection);

        engine.scan().expect("second scan should complete");

        let connection = Connection::open(fixture.path.join(".hotpath").join("index.sqlite"))
            .expect("db should open");
        assert_eq!(row_count(&connection, "file_analysis"), 1);
        assert_eq!(row_count(&connection, "file_facts"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT is_active FROM file_analysis WHERE path LIKE '%a.go'",
            ),
            1
        );
        assert!(
            scalar_i64(
                &connection,
                "SELECT active_scan_id FROM file_analysis WHERE path LIKE '%a.go'",
            ) >= first_scan_id
        );
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT value FROM scan_state WHERE key = 'last_scan_completed'",
            ),
            "1"
        );
    }

    #[test]
    fn second_scan_marks_deleted_files_inactive() {
        let fixture = Fixture::new("incremental-delete");
        fixture.write("a.go", "package main\n");
        let engine = AnalysisEngine::new(&fixture.path);

        engine.scan().expect("first scan should complete");
        fs::remove_file(fixture.path.join("a.go")).expect("fixture file should be deleted");
        engine.scan().expect("second scan should complete");

        let connection = Connection::open(fixture.path.join(".hotpath").join("index.sqlite"))
            .expect("db should open");
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT is_active FROM file_analysis WHERE path LIKE '%a.go'",
            ),
            0
        );
        assert_eq!(row_count(&connection, "file_facts"), 0);
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
                    git_history_analyzer: GitHistoryAnalyzerOptions::default(),
                    git_max_concurrency: 1,
                },
                file_analyzer: FileAnalyzerOptions::default(),
                store_reducer: StoreReducerOptions::default(),
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
        assert!(completed_state.store_completed);
        assert_eq!(completed_state.store_remaining_records(), Some(0));
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
                    git_history_analyzer: GitHistoryAnalyzerOptions::default(),
                    git_max_concurrency: 1,
                },
                file_analyzer: FileAnalyzerOptions {
                    content_window_bytes: 128,
                    parsers: Vec::new(),
                },
                store_reducer: StoreReducerOptions::default(),
            },
        );

        assert_eq!(engine.options().scheduler.worker_count, 3);
        assert_eq!(engine.options().scheduler.queue_capacity, 7);
        assert_eq!(engine.options().file_analyzer.content_window_bytes, 128);
    }

    #[test]
    fn scan_emits_git_skipped_for_non_git_directory() {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("hotpath-non-git-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("non-git fixture should be created");
        fs::write(path.join("a.go"), "package main\n").expect("fixture file should be written");
        let engine = AnalysisEngine::new(&path);
        let mut saw_git_skipped = false;

        let result = engine
            .scan_with_event_observer(|state, event| {
                if matches!(event, PipelineEvent::GitHistorySkipped { .. }) {
                    saw_git_skipped = true;
                    assert_eq!(state.total_git_commits, Some(0));
                    assert!(state.git_completed);
                }
            })
            .expect("scan should succeed without git");
        let _ = fs::remove_dir_all(&path);

        assert_eq!(result.files_detected, 1);
        assert!(saw_git_skipped);
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        connection
            .query_row(&sql, [], |row| row.get(0))
            .expect("row count should query")
    }

    fn scalar_i64(connection: &Connection, sql: &str) -> i64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }

    fn scalar_text(connection: &Connection, sql: &str) -> String {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }
}
