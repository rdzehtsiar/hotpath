// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::pipeline::enumerator::EnumeratedFile;
use crate::pipeline::file_analyzer::{FileAnalysisInput, FileAnalyzer};

pub const DEFAULT_QUEUE_CAPACITY: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerOptions {
    pub worker_count: usize,
    pub queue_capacity: usize,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            worker_count: default_worker_count(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
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
}

#[derive(Debug)]
pub struct Scheduler {
    queue: Arc<TaskQueue>,
    workers: Vec<JoinHandle<()>>,
    submitted_tasks: Arc<AtomicUsize>,
    processed_tasks: Arc<AtomicUsize>,
}

impl Scheduler {
    pub fn start(options: SchedulerOptions) -> Self {
        let worker_count = options.worker_count.max(1);
        let queue = Arc::new(TaskQueue::new(options.queue_capacity));
        let submitted_tasks = Arc::new(AtomicUsize::new(0));
        let processed_tasks = Arc::new(AtomicUsize::new(0));
        let workers = (0..worker_count)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let processed_tasks = Arc::clone(&processed_tasks);
                thread::spawn(move || worker_loop(queue, processed_tasks))
            })
            .collect();

        Self {
            queue,
            workers,
            submitted_tasks,
            processed_tasks,
        }
    }

    pub fn submit(&self, task: PipelineTask) -> Result<(), SchedulerError> {
        self.queue.push(task)?;
        self.submitted_tasks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub fn settings(&self) -> Arc<SchedulerRuntimeSettings> {
        Arc::clone(&self.queue.settings)
    }

    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            submitted_tasks: self.submitted_tasks.load(Ordering::SeqCst),
            processed_tasks: self.processed_tasks.load(Ordering::SeqCst),
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
        })
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

fn worker_loop(queue: Arc<TaskQueue>, processed_tasks: Arc<AtomicUsize>) {
    let analyzer = FileAnalyzer::new();

    while let Some(task) = queue.pop() {
        match task {
            PipelineTask::AnalyzeFile(file) => {
                let _result = analyzer.analyze(FileAnalysisInput { path: file.path });
                processed_tasks.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

fn default_worker_count() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
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

    #[test]
    fn default_options_use_large_queue_and_available_workers() {
        let options = SchedulerOptions::default();

        assert_eq!(options.queue_capacity, DEFAULT_QUEUE_CAPACITY);
        assert!(options.worker_count >= 1);
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
        });

        for index in 0..5 {
            scheduler
                .submit(task(index))
                .expect("task should submit successfully");
        }

        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.submitted_tasks, 5);
        assert_eq!(stats.processed_tasks, 5);
    }

    #[test]
    fn workers_wait_on_empty_queue_and_shutdown_cleanly() {
        let scheduler = Scheduler::start(SchedulerOptions {
            worker_count: 2,
            queue_capacity: 2,
        });

        let stats = scheduler.finish().expect("scheduler should finish");

        assert_eq!(stats.submitted_tasks, 0);
        assert_eq!(stats.processed_tasks, 0);
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
        });

        scheduler
            .submit(task(0))
            .expect("task should submit successfully");
        let stats = scheduler.stats();

        assert_eq!(stats.submitted_tasks, 1);
        scheduler.finish().expect("scheduler should finish");
    }

    fn task(index: usize) -> PipelineTask {
        PipelineTask::AnalyzeFile(EnumeratedFile {
            path: PathBuf::from(format!("file-{index}.go")),
        })
    }
}
