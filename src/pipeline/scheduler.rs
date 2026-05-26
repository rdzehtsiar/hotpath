// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::pipeline::enumerator::EnumeratedFile;
use crate::pipeline::events::PipelineEvent;
use crate::pipeline::file_analyzer::{FileAnalysisInput, FileAnalyzer, FileAnalyzerOptions};
use crate::pipeline::git_history_analyzer::{
    GitHistoryAnalyzer, GitHistoryAnalyzerOptions, GitHistoryProgress, GitHistoryScan,
};
use crate::pipeline::store_reducer::StoreReducerHandle;

pub const DEFAULT_QUEUE_CAPACITY: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerOptions {
    pub worker_count: usize,
    pub queue_capacity: usize,
    pub file_analyzer: FileAnalyzerOptions,
    pub git_history_analyzer: GitHistoryAnalyzerOptions,
    pub git_max_concurrency: usize,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            worker_count: default_worker_count(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            file_analyzer: FileAnalyzerOptions::default(),
            git_history_analyzer: GitHistoryAnalyzerOptions::default(),
            git_max_concurrency: default_git_concurrency(),
        }
    }
}

#[derive(Debug)]
pub struct SchedulerRuntimeSettings {
    queue_capacity: AtomicUsize,
}

impl SchedulerRuntimeSettings {
    pub fn new(queue_capacity: usize) -> Self {
        Self {
            queue_capacity: AtomicUsize::new(queue_capacity.max(1)),
        }
    }

    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity.load(Ordering::SeqCst)
    }

    pub fn set_queue_capacity(&self, queue_capacity: usize) {
        self.queue_capacity
            .store(queue_capacity.max(1), Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineTask {
    AnalyzeFile(EnumeratedFile),
    AnalyzeGitHistory(GitHistoryScan),
}

#[derive(Debug)]
pub enum SchedulerError {
    Closed,
    WorkerPanicked,
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "scheduler is closed"),
            Self::WorkerPanicked => write!(f, "scheduler worker thread panicked"),
        }
    }
}

impl StdError for SchedulerError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub submitted_tasks: usize,
    pub processed_tasks: usize,
    pub processed_file_tasks: usize,
    pub processed_git_commits: usize,
}

#[derive(Debug)]
pub struct Scheduler {
    queue: Arc<TaskQueue>,
    workers: Vec<JoinHandle<()>>,
    submitted_tasks: Arc<AtomicUsize>,
    processed_tasks: Arc<AtomicUsize>,
    processed_file_tasks: Arc<AtomicUsize>,
    processed_git_commits: Arc<AtomicUsize>,
}

impl Scheduler {
    pub fn start(options: SchedulerOptions) -> Self {
        Self::start_with_events(options, None, None)
    }

    pub fn start_with_events(
        options: SchedulerOptions,
        event_sender: Option<Sender<PipelineEvent>>,
        store_reducer: Option<StoreReducerHandle>,
    ) -> Self {
        let worker_count = options.worker_count.max(1);
        let file_analyzer_options = options.file_analyzer.clone();
        let git_history_analyzer_options = options.git_history_analyzer.clone();
        let git_concurrency = Arc::new(ConcurrencyLimit::new(options.git_max_concurrency));
        let queue = Arc::new(TaskQueue::new(options.queue_capacity));
        let submitted_tasks = Arc::new(AtomicUsize::new(0));
        let processed_tasks = Arc::new(AtomicUsize::new(0));
        let processed_file_tasks = Arc::new(AtomicUsize::new(0));
        let processed_git_commits = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let workers = (0..worker_count)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let processed_tasks = Arc::clone(&processed_tasks);
                let processed_file_tasks = Arc::clone(&processed_file_tasks);
                let processed_git_commits = Arc::clone(&processed_git_commits);
                let file_analyzer_options = file_analyzer_options.clone();
                let git_history_analyzer_options = git_history_analyzer_options.clone();
                let git_concurrency = Arc::clone(&git_concurrency);
                let event_sender = event_sender.clone();
                let store_reducer = store_reducer.clone();
                thread::spawn(move || {
                    worker_loop(
                        queue,
                        WorkerContext {
                            processed_tasks,
                            processed_file_tasks,
                            processed_git_commits,
                            file_analyzer_options,
                            git_history_analyzer_options,
                            git_concurrency,
                            event_sender,
                            store_reducer,
                            started,
                        },
                    )
                })
            })
            .collect();

        Self {
            queue,
            workers,
            submitted_tasks,
            processed_tasks,
            processed_file_tasks,
            processed_git_commits,
        }
    }

    pub fn submit(&self, task: PipelineTask) -> Result<(), SchedulerError> {
        self.queue.push(task)?;
        self.submitted_tasks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            queue: Arc::clone(&self.queue),
            submitted_tasks: Arc::clone(&self.submitted_tasks),
        }
    }

    pub fn settings(&self) -> Arc<SchedulerRuntimeSettings> {
        Arc::clone(&self.queue.settings)
    }

    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            submitted_tasks: self.submitted_tasks.load(Ordering::SeqCst),
            processed_tasks: self.processed_tasks.load(Ordering::SeqCst),
            processed_file_tasks: self.processed_file_tasks.load(Ordering::SeqCst),
            processed_git_commits: self.processed_git_commits.load(Ordering::SeqCst),
        }
    }

    pub fn finish(self) -> Result<SchedulerStats, SchedulerError> {
        self.queue.close();
        for worker in self.workers {
            worker.join().map_err(|_| SchedulerError::WorkerPanicked)?;
        }

        Ok(SchedulerStats {
            submitted_tasks: self.submitted_tasks.load(Ordering::SeqCst),
            processed_tasks: self.processed_tasks.load(Ordering::SeqCst),
            processed_file_tasks: self.processed_file_tasks.load(Ordering::SeqCst),
            processed_git_commits: self.processed_git_commits.load(Ordering::SeqCst),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerHandle {
    queue: Arc<TaskQueue>,
    submitted_tasks: Arc<AtomicUsize>,
}

impl SchedulerHandle {
    pub fn submit(&self, task: PipelineTask) -> Result<(), SchedulerError> {
        self.queue.push(task)?;
        self.submitted_tasks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct TaskQueue {
    state: Mutex<TaskQueueState>,
    has_tasks: Condvar,
    has_capacity: Condvar,
    settings: Arc<SchedulerRuntimeSettings>,
}

impl TaskQueue {
    fn new(queue_capacity: usize) -> Self {
        Self {
            state: Mutex::new(TaskQueueState::default()),
            has_tasks: Condvar::new(),
            has_capacity: Condvar::new(),
            settings: Arc::new(SchedulerRuntimeSettings::new(queue_capacity)),
        }
    }

    fn push(&self, task: PipelineTask) -> Result<(), SchedulerError> {
        let mut state = self
            .state
            .lock()
            .expect("task queue mutex should not poison");
        while !state.closed && state.tasks.len() >= self.settings.queue_capacity() {
            state = self
                .has_capacity
                .wait(state)
                .expect("task queue mutex should not poison");
        }

        if state.closed {
            return Err(SchedulerError::Closed);
        }

        state.tasks.push_back(task);
        self.has_tasks.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<PipelineTask> {
        let mut state = self
            .state
            .lock()
            .expect("task queue mutex should not poison");
        loop {
            if let Some(task) = state.tasks.pop_front() {
                self.has_capacity.notify_one();
                return Some(task);
            }

            if state.closed {
                return None;
            }

            state = self
                .has_tasks
                .wait(state)
                .expect("task queue mutex should not poison");
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .expect("task queue mutex should not poison");
        state.closed = true;
        self.has_tasks.notify_all();
        self.has_capacity.notify_all();
    }
}

#[derive(Debug, Default)]
struct TaskQueueState {
    tasks: VecDeque<PipelineTask>,
    closed: bool,
}

fn worker_loop(queue: Arc<TaskQueue>, context: WorkerContext) {
    let file_analyzer = FileAnalyzer::with_options(context.file_analyzer_options);
    let git_analyzer = GitHistoryAnalyzer::with_options(context.git_history_analyzer_options);

    while let Some(task) = queue.pop() {
        match task {
            PipelineTask::AnalyzeFile(file) => {
                let result = file_analyzer.analyze(FileAnalysisInput { path: file.path });
                if let Some(store_reducer) = &context.store_reducer {
                    let _ = store_reducer.store_file_analysis(result);
                }
                context.processed_tasks.fetch_add(1, Ordering::SeqCst);
                let analyzed_files =
                    context.processed_file_tasks.fetch_add(1, Ordering::SeqCst) + 1;
                if let Some(event_sender) = &context.event_sender {
                    let _ = event_sender.send(PipelineEvent::FileAnalysisCompleted {
                        analyzed_files: analyzed_files as u64,
                        elapsed: context.started.elapsed(),
                    });
                }
            }
            PipelineTask::AnalyzeGitHistory(scan) => {
                let _permit = context.git_concurrency.acquire();
                let event_sender = context.event_sender.clone();
                let processed_git_commits = Arc::clone(&context.processed_git_commits);
                let started = context.started;
                let mut report_progress = |progress: GitHistoryProgress| {
                    processed_git_commits
                        .fetch_add(progress.commits_processed as usize, Ordering::SeqCst);
                    if let Some(event_sender) = &event_sender {
                        let _ = event_sender.send(PipelineEvent::GitHistoryChunkCompleted {
                            processed_commits: progress.commits_processed,
                            file_changes: progress.file_changes,
                            elapsed: started.elapsed(),
                        });
                    }
                };
                let result = match &context.store_reducer {
                    Some(store_reducer) => git_analyzer.analyze_with_progress(
                        scan,
                        store_reducer,
                        &mut report_progress,
                    ),
                    None => {
                        let sink = crate::pipeline::git_history_analyzer::NoopGitHistorySink;
                        git_analyzer.analyze_with_progress(scan, &sink, &mut report_progress)
                    }
                };
                context.processed_tasks.fetch_add(1, Ordering::SeqCst);
                match result {
                    Ok(_) => {}
                    Err(error) => {
                        if let Some(event_sender) = &context.event_sender {
                            let _ = event_sender.send(PipelineEvent::GitHistoryChunkFailed {
                                reason: error.to_string(),
                                elapsed: context.started.elapsed(),
                            });
                        }
                    }
                }
            }
        }
    }
}

struct WorkerContext {
    processed_tasks: Arc<AtomicUsize>,
    processed_file_tasks: Arc<AtomicUsize>,
    processed_git_commits: Arc<AtomicUsize>,
    file_analyzer_options: FileAnalyzerOptions,
    git_history_analyzer_options: GitHistoryAnalyzerOptions,
    git_concurrency: Arc<ConcurrencyLimit>,
    event_sender: Option<Sender<PipelineEvent>>,
    store_reducer: Option<StoreReducerHandle>,
    started: Instant,
}

#[derive(Debug)]
struct ConcurrencyLimit {
    state: Mutex<ConcurrencyLimitState>,
    has_capacity: Condvar,
}

impl ConcurrencyLimit {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(ConcurrencyLimitState {
                limit: limit.max(1),
                active: 0,
            }),
            has_capacity: Condvar::new(),
        }
    }

    fn acquire(&self) -> ConcurrencyPermit<'_> {
        let mut state = self
            .state
            .lock()
            .expect("concurrency limit mutex should not poison");
        while state.active >= state.limit {
            state = self
                .has_capacity
                .wait(state)
                .expect("concurrency limit mutex should not poison");
        }
        state.active += 1;

        ConcurrencyPermit { limit: self }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .expect("concurrency limit mutex should not poison");
        state.active = state.active.saturating_sub(1);
        self.has_capacity.notify_one();
    }
}

#[derive(Debug)]
struct ConcurrencyLimitState {
    limit: usize,
    active: usize,
}

#[derive(Debug)]
struct ConcurrencyPermit<'a> {
    limit: &'a ConcurrencyLimit,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        self.limit.release();
    }
}

fn default_worker_count() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

fn default_git_concurrency() -> usize {
    default_worker_count().clamp(1, 4)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::{
        PipelineTask, Scheduler, SchedulerOptions, SchedulerRuntimeSettings, TaskQueue,
        DEFAULT_QUEUE_CAPACITY,
    };
    use crate::pipeline::enumerator::EnumeratedFile;
    use crate::pipeline::events::PipelineEvent;
    use crate::pipeline::file_analyzer::FileAnalyzerOptions;
    use crate::pipeline::git_history_analyzer::{GitHistoryAnalyzerOptions, GitHistoryScan};

    #[test]
    fn default_options_use_large_queue_and_available_workers() {
        let options = SchedulerOptions::default();

        assert_eq!(options.queue_capacity, DEFAULT_QUEUE_CAPACITY);
        assert!(options.worker_count >= 1);
        assert!(options.git_max_concurrency >= 1);
        assert!(options.git_max_concurrency <= 4);
    }

    #[test]
    fn runtime_settings_can_update_queue_capacity() {
        let settings = SchedulerRuntimeSettings::new(2);

        assert_eq!(settings.queue_capacity(), 2);
        settings.set_queue_capacity(5);
        assert_eq!(settings.queue_capacity(), 5);
        settings.set_queue_capacity(0);
        assert_eq!(settings.queue_capacity(), 1);
    }

    #[test]
    fn workers_process_all_submitted_file_tasks() {
        let scheduler = Scheduler::start(SchedulerOptions {
            worker_count: 2,
            queue_capacity: 8,
            file_analyzer: FileAnalyzerOptions::default(),
            git_history_analyzer: GitHistoryAnalyzerOptions::default(),
            git_max_concurrency: 1,
        });

        for index in 0..5 {
            scheduler
                .submit(task(index))
                .expect("task should submit successfully");
        }

        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.submitted_tasks, 5);
        assert_eq!(stats.processed_tasks, 5);
        assert_eq!(stats.processed_file_tasks, 5);
    }

    #[test]
    fn workers_wait_on_empty_queue_and_shutdown_cleanly() {
        let scheduler = Scheduler::start(SchedulerOptions {
            worker_count: 2,
            queue_capacity: 2,
            file_analyzer: FileAnalyzerOptions::default(),
            git_history_analyzer: GitHistoryAnalyzerOptions::default(),
            git_max_concurrency: 1,
        });

        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.submitted_tasks, 0);
        assert_eq!(stats.processed_tasks, 0);
        assert_eq!(stats.processed_file_tasks, 0);
    }

    #[test]
    fn queue_blocks_producers_when_full_until_capacity_is_available() {
        let queue = Arc::new(TaskQueue::new(1));
        queue.push(task(0)).expect("first task should push");
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                started_tx.send(()).expect("started signal should send");
                queue
                    .push(task(1))
                    .expect("second task should eventually push");
                done_tx.send(()).expect("done signal should send");
            })
        };

        started_rx.recv().expect("producer should start");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "producer should block while queue capacity is full"
        );

        let first_task = queue.pop().expect("first task should pop");
        assert_eq!(first_task, task(0));
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("producer should unblock after queue capacity is available");
        producer.join().expect("producer should not panic");
        queue.close();
    }

    #[test]
    fn stats_can_be_read_before_finish() {
        let scheduler = Scheduler::start(SchedulerOptions {
            worker_count: 1,
            queue_capacity: 2,
            file_analyzer: FileAnalyzerOptions::default(),
            git_history_analyzer: GitHistoryAnalyzerOptions::default(),
            git_max_concurrency: 1,
        });

        scheduler
            .submit(task(0))
            .expect("task should submit successfully");
        let stats = scheduler.stats();

        assert_eq!(stats.submitted_tasks, 1);
        scheduler.finish().expect("scheduler should finish");
    }

    #[test]
    fn workers_emit_analysis_completion_events() {
        let (event_sender, event_receiver) = mpsc::channel();
        let scheduler = Scheduler::start_with_events(
            SchedulerOptions {
                worker_count: 1,
                queue_capacity: 2,
                file_analyzer: FileAnalyzerOptions::default(),
                git_history_analyzer: GitHistoryAnalyzerOptions::default(),
                git_max_concurrency: 1,
            },
            Some(event_sender),
            None,
        );

        scheduler
            .submit(task(0))
            .expect("task should submit successfully");
        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.processed_tasks, 1);
        let events: Vec<_> = event_receiver.try_iter().collect();
        assert!(events.iter().any(|event| matches!(
            event,
            PipelineEvent::FileAnalysisCompleted {
                analyzed_files: 1,
                ..
            }
        )));
    }

    #[test]
    fn workers_process_git_history_scan_tasks() {
        let (event_sender, event_receiver) = mpsc::channel();
        let scheduler = Scheduler::start_with_events(
            SchedulerOptions {
                worker_count: 1,
                queue_capacity: 2,
                file_analyzer: FileAnalyzerOptions::default(),
                git_history_analyzer: GitHistoryAnalyzerOptions::default(),
                git_max_concurrency: 1,
            },
            Some(event_sender),
            None,
        );

        scheduler
            .submit(PipelineTask::AnalyzeGitHistory(GitHistoryScan {
                root: PathBuf::from("definitely-not-a-git-root"),
                revision: None,
                head_timestamp: 0,
                max_commits: Some(0),
                max_age_days: None,
                cochange_max_files_per_commit: 100,
                delta_batch_size: 10_000,
            }))
            .expect("git scan task should submit successfully");
        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.processed_tasks, 1);
        assert_eq!(stats.processed_git_commits, 0);
        let events: Vec<_> = event_receiver.try_iter().collect();
        assert!(events
            .iter()
            .any(|event| matches!(event, PipelineEvent::GitHistoryChunkFailed { .. })));
    }

    fn task(index: usize) -> PipelineTask {
        PipelineTask::AnalyzeFile(EnumeratedFile {
            path: PathBuf::from(format!("file-{index}.go")),
        })
    }
}
