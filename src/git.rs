// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, TrySendError};
use git2::{Delta, Diff, DiffLineType, DiffOptions, ErrorClass, ErrorCode, Oid, Repository, Sort};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::operation_log;
use crate::ownership::{operational_ownership_from_changes, OperationalOwnershipSnapshot};

pub const RECENT_CHURN_WINDOW_DAYS: i64 = 90;
const SECONDS_PER_DAY: i64 = 86_400;
const RECENT_CHURN_WINDOW_SECONDS: i64 = RECENT_CHURN_WINDOW_DAYS * SECONDS_PER_DAY;
const CO_CHANGED_FILE_COUNT_SATURATION: u64 = 25;
const MAX_PAIRWISE_CO_CHANGE_PATHS: usize = 256;
const GIT_CACHE_LOOKUP_BATCH_SIZE: usize = 1024;
const GIT_DIFF_JOB_COMMIT_CHUNK_SIZE: usize = 64;
const GIT_COMMIT_CACHE_WRITE_BATCH_SIZE: usize = 512;
const GIT_DIFF_QUEUE_FACTOR: usize = 4;
const GIT_RESULT_QUEUE_FACTOR: usize = 8;
const MAX_DEFAULT_GIT_WORKERS: usize = 16;
const GIT_PERF_ENV: &str = "HOTPATH_PERF";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A raw repository-relative file change observed in one reachable commit.
pub struct GitFileChange {
    /// Full hexadecimal commit object id.
    pub commit_id: String,
    /// Number of parents on the commit.
    pub parent_count: usize,
    /// Whether the commit has more than one parent.
    pub is_merge: bool,
    /// Exact commit author identity in `Name <email>` form.
    pub author: String,
    /// Committer timestamp as seconds since the Unix epoch.
    pub commit_time: i64,
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Git file-level change kind for this path.
    pub change_kind: GitChangeKind,
    /// Added line count reported by the selected diff.
    pub added_lines: u64,
    /// Deleted line count reported by the selected diff.
    pub deleted_lines: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
/// File-level change kind reported by Git for a selected commit diff.
pub enum GitChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq)]
/// Aggregated churn and ownership metrics for one repository-relative path.
pub struct GitFileMetrics {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Number of distinct commits that touched this path.
    pub commits_per_file: u64,
    /// Total added lines across all observed file changes for this path.
    pub total_churn_added: u64,
    /// Total deleted lines across all observed file changes for this path.
    pub total_churn_deleted: u64,
    /// Added lines in the 90-day window relative to the HEAD commit time.
    pub recent_churn_added: u64,
    /// Deleted lines in the 90-day window relative to the HEAD commit time.
    pub recent_churn_deleted: u64,
    /// Number of distinct exact author identities that touched this path.
    pub author_count: u64,
    /// Number of meaningful operational owners identified for this path.
    pub owner_count: u64,
    /// Operational owner with the highest weighted ownership share for this path.
    pub dominant_owner: Option<String>,
    /// Dominant owner's weighted operational ownership share for this path.
    pub dominant_owner_share: Option<f64>,
    /// Distinct files observed in commits that also touched this path.
    ///
    /// This count is saturated at the score formula's current co-change
    /// normalization ceiling so large mechanical commits cannot dominate
    /// runtime or memory use.
    pub co_changed_file_count: u64,
    /// First observed commit id for this path by commit time, then commit id.
    pub first_commit_id: Option<String>,
    /// First observed committer timestamp for this path.
    pub first_commit_time: Option<i64>,
    /// Last observed commit id for this path by commit time, then commit id.
    pub last_commit_id: Option<String>,
    /// Last observed committer timestamp for this path.
    pub last_commit_time: Option<i64>,
    /// Whole days between first observed commit time and HEAD commit time.
    pub file_age_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Aggregated co-change count for one unordered repository-relative path pair.
pub struct GitCoChange {
    /// Lexicographically smaller repository-relative path using `/` separators.
    pub left_path: String,
    /// Lexicographically larger repository-relative path using `/` separators.
    pub right_path: String,
    /// Number of distinct commits that touched both paths.
    pub commit_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Progress while walking reachable Git history.
pub struct GitHistoryProgress {
    pub completed_commits: usize,
    pub total_commits: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPipelineOptions {
    pub git_jobs: usize,
    pub job_commit_chunk_size: usize,
    pub diff_queue_capacity: usize,
    pub result_queue_capacity: usize,
}

#[derive(Debug)]
struct GitPerf {
    enabled: bool,
    started_at: Instant,
    total_commits: usize,
    git_jobs: usize,
    job_commit_chunk_size: usize,
    diff_queue_capacity: usize,
    result_queue_capacity: usize,
    revwalk_ms: u128,
    cache_batches: u64,
    cache_lookup_ms: u128,
    cache_hits: u64,
    cache_misses: u64,
    jobs_sent: u64,
    queue_full_count: u64,
    enqueue_wait_ms: u128,
    result_receive_wait_ms: u128,
    result_batches: u64,
    reducer_handle_ms: u128,
    reducer_commits: u64,
    reducer_changes: u64,
    reducer_deltas: u64,
    reducer_changed_lines: u64,
    cache_flushes: u64,
    final_sort_ms: u128,
    aggregation_ms: u128,
    workers: Arc<Mutex<Vec<GitWorkerPerf>>>,
}

#[derive(Debug, Clone, Default)]
struct GitWorkerPerf {
    worker_id: usize,
    jobs: u64,
    commits: u64,
    changes: u64,
    deltas: u64,
    changed_lines: u64,
    active_ms: u128,
    send_wait_ms: u128,
    errors: u64,
}

impl GitPerf {
    fn new(enabled: bool, options: GitPipelineOptions) -> Self {
        Self {
            enabled,
            started_at: Instant::now(),
            total_commits: 0,
            git_jobs: options.git_jobs,
            job_commit_chunk_size: options.job_commit_chunk_size,
            diff_queue_capacity: options.diff_queue_capacity,
            result_queue_capacity: options.result_queue_capacity,
            revwalk_ms: 0,
            cache_batches: 0,
            cache_lookup_ms: 0,
            cache_hits: 0,
            cache_misses: 0,
            jobs_sent: 0,
            queue_full_count: 0,
            enqueue_wait_ms: 0,
            result_receive_wait_ms: 0,
            result_batches: 0,
            reducer_handle_ms: 0,
            reducer_commits: 0,
            reducer_changes: 0,
            reducer_deltas: 0,
            reducer_changed_lines: 0,
            cache_flushes: 0,
            final_sort_ms: 0,
            aggregation_ms: 0,
            workers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn disabled() -> Self {
        Self::new(false, GitPipelineOptions::default())
    }

    fn emit_summary(&self) {
        if !self.enabled {
            return;
        }

        let workers = self
            .workers
            .lock()
            .map(|workers| workers.clone())
            .unwrap_or_default();
        operation_log::event(
            "hotpath.git_perf_summary",
            json!({
                "elapsed_ms": elapsed_ms(self.started_at.elapsed()),
                "total_commits": self.total_commits,
                "git_jobs": self.git_jobs,
                "job_commit_chunk_size": self.job_commit_chunk_size,
                "diff_queue_capacity": self.diff_queue_capacity,
                "result_queue_capacity": self.result_queue_capacity,
                "revwalk_ms": self.revwalk_ms,
                "cache_batches": self.cache_batches,
                "cache_lookup_ms": self.cache_lookup_ms,
                "cache_hits": self.cache_hits,
                "cache_misses": self.cache_misses,
                "jobs_sent": self.jobs_sent,
                "queue_full_count": self.queue_full_count,
                "enqueue_wait_ms": self.enqueue_wait_ms,
                "result_receive_wait_ms": self.result_receive_wait_ms,
                "result_batches": self.result_batches,
                "reducer_handle_ms": self.reducer_handle_ms,
                "reducer_commits": self.reducer_commits,
                "reducer_changes": self.reducer_changes,
                "reducer_deltas": self.reducer_deltas,
                "reducer_changed_lines": self.reducer_changed_lines,
                "cache_flushes": self.cache_flushes,
                "final_sort_ms": self.final_sort_ms,
                "aggregation_ms": self.aggregation_ms,
                "workers": workers.iter().map(|worker| {
                    json!({
                        "worker_id": worker.worker_id,
                        "jobs": worker.jobs,
                        "commits": worker.commits,
                        "changes": worker.changes,
                        "deltas": worker.deltas,
                        "changed_lines": worker.changed_lines,
                        "active_ms": worker.active_ms,
                        "send_wait_ms": worker.send_wait_ms,
                        "errors": worker.errors,
                    })
                }).collect::<Vec<_>>(),
            }),
        );
    }

    fn record_send_stats(&mut self, stats: SendDiffJobStats) {
        self.jobs_sent += 1;
        self.queue_full_count += stats.queue_full_count;
        self.enqueue_wait_ms += stats.enqueue_wait_ms;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SendDiffJobStats {
    queue_full_count: u64,
    enqueue_wait_ms: u128,
}

struct GitCacheCallbacks<L, S> {
    load: L,
    store: S,
}

fn git_perf_enabled() -> bool {
    env::var(GIT_PERF_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn elapsed_ms(duration: Duration) -> u128 {
    duration.as_millis()
}

impl Default for GitPipelineOptions {
    fn default() -> Self {
        let parallelism = thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        let git_jobs = env::var("HOTPATH_GIT_JOBS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or_else(|| parallelism.clamp(1, MAX_DEFAULT_GIT_WORKERS));
        let job_commit_chunk_size = env::var("HOTPATH_GIT_CHUNK_SIZE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(GIT_DIFF_JOB_COMMIT_CHUNK_SIZE);

        Self {
            git_jobs,
            job_commit_chunk_size,
            diff_queue_capacity: git_jobs.saturating_mul(GIT_DIFF_QUEUE_FACTOR).max(1),
            result_queue_capacity: git_jobs.saturating_mul(GIT_RESULT_QUEUE_FACTOR).max(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Full local Git analysis for a worktree at `HEAD`.
pub struct GitAnalysis {
    /// Canonical Git worktree root used for index persistence.
    pub worktree_root: PathBuf,
    /// Full hexadecimal object id for `HEAD`.
    pub head_commit_id: String,
    /// `HEAD` committer timestamp as seconds since the Unix epoch.
    pub head_commit_time: i64,
    /// Recent churn window in days.
    pub recent_window_days: i64,
    /// Deterministic raw file change events.
    pub changes: Vec<GitFileChange>,
    /// Aggregated file metrics sorted by path.
    pub file_metrics: Vec<GitFileMetrics>,
    /// Aggregated co-change pairs ranked by count, then paths.
    pub co_changes: Vec<GitCoChange>,
    /// Weighted operational ownership by current file path.
    pub ownership: OperationalOwnershipSnapshot,
}

#[derive(Debug)]
/// Errors that can occur while opening or traversing local Git history.
pub enum GitHistoryError {
    NotRepository {
        path: PathBuf,
        source: git2::Error,
    },
    OpenRepository {
        path: PathBuf,
        source: git2::Error,
    },
    MissingHead {
        path: PathBuf,
    },
    ShallowRepository {
        path: PathBuf,
    },
    BareRepository {
        path: PathBuf,
    },
    HeadNotCommit {
        path: PathBuf,
        source: git2::Error,
    },
    Git {
        context: &'static str,
        source: git2::Error,
    },
    UnsupportedAuthorIdentity {
        commit_id: String,
    },
    UnsupportedPathEncoding {
        commit_id: String,
    },
    WorkerFailed {
        context: &'static str,
    },
}

#[derive(Debug)]
/// Errors that can occur while explaining Git metrics for one file.
pub enum GitExplainError {
    History(GitHistoryError),
    BareRepository,
    EmptyPath,
    PathOutsideRepository,
    UnsupportedPathEncoding,
    AmbiguousPath { first: String, second: String },
}

impl fmt::Display for GitHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_git_history_error(f, self, GitHistoryUsage::ExplainGit)
    }
}

impl StdError for GitHistoryError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::NotRepository { source, .. }
            | Self::OpenRepository { source, .. }
            | Self::HeadNotCommit { source, .. }
            | Self::Git { source, .. } => Some(source),
            Self::MissingHead { .. }
            | Self::ShallowRepository { .. }
            | Self::BareRepository { .. }
            | Self::UnsupportedAuthorIdentity { .. }
            | Self::UnsupportedPathEncoding { .. }
            | Self::WorkerFailed { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHistoryUsage {
    ExplainGit,
    Report,
    ExplainHotspot,
    Hotspots,
    Diff,
}

pub(crate) fn write_git_history_error(
    f: &mut fmt::Formatter<'_>,
    source: &GitHistoryError,
    usage: GitHistoryUsage,
) -> fmt::Result {
    match source {
        GitHistoryError::NotRepository { .. } => write!(
            f,
            "path is not a readable Git worktree; run {} from inside a repository with local history",
            usage.command()
        ),
        GitHistoryError::OpenRepository { .. } => write!(
            f,
            "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
        ),
        GitHistoryError::MissingHead { .. } => write!(
            f,
            "Git repository does not have a commit at HEAD; create an initial commit before {}",
            usage.missing_head_action()
        ),
        GitHistoryError::ShallowRepository { .. } => write!(
            f,
            "Git repository has shallow history; fetch complete local history before running {} so metrics are not based on incomplete commits",
            usage.command()
        ),
        GitHistoryError::BareRepository { .. } => write!(
            f,
            "Git repository has no worktree; {} requires a local worktree",
            usage.worktree_subject()
        ),
        GitHistoryError::HeadNotCommit { source, .. } => {
            write!(f, "Git HEAD does not resolve to a commit: {source}")
        }
        GitHistoryError::Git { context, source } => {
            write!(f, "failed to traverse Git history while {context}: {source}")
        }
        GitHistoryError::UnsupportedAuthorIdentity { commit_id } => write!(
            f,
            "commit {commit_id} has an author name or email that is not valid UTF-8"
        ),
        GitHistoryError::UnsupportedPathEncoding { commit_id } => write!(
            f,
            "commit {commit_id} changed a path that is not valid UTF-8"
        ),
        GitHistoryError::WorkerFailed { context } => {
            write!(f, "failed to traverse Git history while {context}")
        }
    }
}

impl GitHistoryUsage {
    fn command(self) -> &'static str {
        match self {
            Self::ExplainGit => "explain-git",
            Self::Report => "report",
            Self::ExplainHotspot => "explain",
            Self::Hotspots => "hotspots",
            Self::Diff => "diff",
        }
    }

    fn missing_head_action(self) -> &'static str {
        match self {
            Self::ExplainGit => "analyzing history",
            Self::Report => "generating a report",
            Self::ExplainHotspot => "explaining hotspot scores",
            Self::Hotspots => "analyzing hotspots",
            Self::Diff => "analyzing a diff",
        }
    }

    fn worktree_subject(self) -> &'static str {
        match self {
            Self::ExplainGit => "Git analysis",
            Self::Report => "report generation",
            Self::ExplainHotspot => "hotspot score explanation",
            Self::Hotspots => "hotspot analysis",
            Self::Diff => "diff analysis",
        }
    }
}

impl fmt::Display for GitExplainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::History(source) => write!(f, "{source}"),
            Self::BareRepository => write!(
                f,
                "Git repository has no worktree; explain-git requires a local worktree"
            ),
            Self::EmptyPath => write!(f, "explain-git requires a non-empty file path"),
            Self::PathOutsideRepository => write!(
                f,
                "requested path is outside the Git worktree; pass a repository-relative path or a path under the current worktree"
            ),
            Self::UnsupportedPathEncoding => write!(
                f,
                "requested path is not valid UTF-8 and cannot be rendered as a portable repository-relative path"
            ),
            Self::AmbiguousPath { first, second } => write!(
                f,
                "requested path is ambiguous inside this worktree; it could refer to '{first}' or '{second}'"
            ),
        }
    }
}

impl StdError for GitExplainError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::History(source) => Some(source),
            Self::BareRepository
            | Self::EmptyPath
            | Self::PathOutsideRepository
            | Self::UnsupportedPathEncoding
            | Self::AmbiguousPath { .. } => None,
        }
    }
}

impl From<GitHistoryError> for GitExplainError {
    fn from(source: GitHistoryError) -> Self {
        Self::History(source)
    }
}

/// Return deterministic raw file change events for commits reachable from `HEAD`.
pub fn file_changes_from_head(
    worktree_path: impl AsRef<Path>,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    let worktree_path = worktree_path.as_ref();
    let repository = open_repository(worktree_path)?;
    reject_shallow_repository(&repository, worktree_path)?;
    let head_commit = head_commit(&repository, worktree_path)?;

    file_changes_from_repository(&repository, head_commit.id())
}

/// Return the canonical Git worktree root discovered from a path inside a
/// non-bare repository.
pub fn worktree_root_at(worktree_path: impl AsRef<Path>) -> Result<PathBuf, GitHistoryError> {
    let worktree_path = worktree_path.as_ref();
    let repository = open_repository(worktree_path)?;
    reject_shallow_repository(&repository, worktree_path)?;
    head_commit(&repository, worktree_path)?;
    repository
        .workdir()
        .ok_or_else(|| GitHistoryError::BareRepository {
            path: worktree_path.to_path_buf(),
        })
        .map(Path::to_path_buf)
}

/// Analyze deterministic local Git history reachable from `HEAD`.
pub fn analyze_from_head_at(
    worktree_path: impl AsRef<Path>,
) -> Result<GitAnalysis, GitHistoryError> {
    analyze_from_head_at_with_progress(worktree_path, |_| {})
}

/// Analyze deterministic local Git history reachable from `HEAD`, reporting
/// coarse commit progress to interactive callers.
pub fn analyze_from_head_at_with_progress<F>(
    worktree_path: impl AsRef<Path>,
    progress: F,
) -> Result<GitAnalysis, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
{
    analyze_from_head_at_with_progress_and_cache(worktree_path, progress, |_| None, |_, _| {})
}

/// Analyze deterministic local Git history reachable from `HEAD`, reusing and
/// recording per-commit file changes through caller-provided cache callbacks.
pub fn analyze_from_head_at_with_progress_and_cache<F, L, S>(
    worktree_path: impl AsRef<Path>,
    progress: F,
    mut load_cached_commit: L,
    mut store_commit: S,
) -> Result<GitAnalysis, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
    L: FnMut(&str) -> Option<Vec<GitFileChange>>,
    S: FnMut(&str, &[GitFileChange]),
{
    analyze_from_head_at_with_progress_and_cache_batches(
        worktree_path,
        progress,
        |commit_ids| {
            commit_ids
                .iter()
                .filter_map(|commit_id| {
                    load_cached_commit(commit_id).map(|changes| (commit_id.clone(), changes))
                })
                .collect()
        },
        |commits| {
            for (commit_id, changes) in commits {
                store_commit(commit_id, changes);
            }
        },
    )
}

/// Analyze deterministic local Git history reachable from `HEAD`, reusing and
/// recording per-commit file changes through batch cache callbacks.
pub fn analyze_from_head_at_with_progress_and_cache_batches<F, L, S>(
    worktree_path: impl AsRef<Path>,
    progress: F,
    load_cached_commits: L,
    store_commits: S,
) -> Result<GitAnalysis, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
    L: FnMut(&[String]) -> BTreeMap<String, Vec<GitFileChange>>,
    S: FnMut(&[(String, Vec<GitFileChange>)]),
{
    let worktree_path = worktree_path.as_ref();
    let repository = open_repository(worktree_path)?;
    reject_shallow_repository(&repository, worktree_path)?;
    let head_commit = head_commit(&repository, worktree_path)?;
    let head_commit_id = head_commit.id();
    let head_commit_time = head_commit.time().seconds();
    let worktree_root = repository
        .workdir()
        .ok_or_else(|| GitHistoryError::BareRepository {
            path: worktree_path.to_path_buf(),
        })?
        .to_path_buf();
    let options = GitPipelineOptions::default();
    let mut perf = GitPerf::new(git_perf_enabled(), options);
    let changes = file_changes_from_repository_with_progress_and_cache(
        &repository,
        &worktree_root,
        head_commit_id,
        progress,
        GitCacheCallbacks {
            load: load_cached_commits,
            store: store_commits,
        },
        options,
        &mut perf,
    )?;
    let aggregation_started = Instant::now();
    let ownership = operational_ownership_from_changes(&changes, head_commit_time);
    let file_metrics =
        file_metrics_from_changes_with_ownership(&changes, head_commit_time, &ownership);
    let co_changes = co_changes_from_changes(&changes);
    perf.aggregation_ms = elapsed_ms(aggregation_started.elapsed());
    perf.emit_summary();

    Ok(GitAnalysis {
        worktree_root,
        head_commit_id: head_commit_id.to_string(),
        head_commit_time,
        recent_window_days: RECENT_CHURN_WINDOW_DAYS,
        changes,
        file_metrics,
        co_changes,
        ownership,
    })
}

fn file_changes_from_repository(
    repository: &Repository,
    head_commit_id: Oid,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    file_changes_from_repository_with_progress(repository, head_commit_id, |_| {})
}

fn file_changes_from_repository_with_progress<F>(
    repository: &Repository,
    head_commit_id: Oid,
    progress: F,
) -> Result<Vec<GitFileChange>, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
{
    let worktree_root = repository
        .workdir()
        .ok_or_else(|| GitHistoryError::BareRepository {
            path: PathBuf::from("."),
        })?
        .to_path_buf();
    let mut perf = GitPerf::disabled();
    file_changes_from_repository_with_progress_and_cache(
        repository,
        &worktree_root,
        head_commit_id,
        progress,
        GitCacheCallbacks {
            load: no_cached_commits,
            store: ignore_cached_commits,
        },
        GitPipelineOptions::default(),
        &mut perf,
    )
}

fn no_cached_commits(_: &[String]) -> BTreeMap<String, Vec<GitFileChange>> {
    BTreeMap::new()
}

fn ignore_cached_commits(_: &[(String, Vec<GitFileChange>)]) {}

fn file_changes_from_repository_with_progress_and_cache<F, L, S>(
    repository: &Repository,
    worktree_root: &Path,
    head_commit_id: Oid,
    mut progress: F,
    mut cache: GitCacheCallbacks<L, S>,
    options: GitPipelineOptions,
    perf: &mut GitPerf,
) -> Result<Vec<GitFileChange>, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
    L: FnMut(&[String]) -> BTreeMap<String, Vec<GitFileChange>>,
    S: FnMut(&[(String, Vec<GitFileChange>)]),
{
    let revwalk_started = Instant::now();
    let commits = reachable_commits(repository, head_commit_id)?;
    perf.revwalk_ms = elapsed_ms(revwalk_started.elapsed());
    let mut changes = Vec::new();
    let total_commits = commits.len();
    perf.total_commits = total_commits;
    perf.git_jobs = options.git_jobs;
    perf.job_commit_chunk_size = options.job_commit_chunk_size;
    perf.diff_queue_capacity = options.diff_queue_capacity;
    perf.result_queue_capacity = options.result_queue_capacity;
    if total_commits == 0 {
        perf.emit_summary();
        return Ok(changes);
    }
    let git_jobs = options.git_jobs.clamp(1, total_commits.max(1));
    let job_commit_chunk_size = options.job_commit_chunk_size.max(1);
    let (job_sender, job_receiver) = bounded::<DiffJob>(options.diff_queue_capacity.max(git_jobs));
    let (result_sender, result_receiver) =
        bounded::<DiffWorkerResult>(options.result_queue_capacity.max(git_jobs));
    let mut workers = Vec::with_capacity(git_jobs);
    for worker_id in 0..git_jobs {
        let worktree_root = worktree_root.to_path_buf();
        let job_receiver = job_receiver.clone();
        let result_sender = result_sender.clone();
        let worker_perf = perf.workers.clone();
        let enabled = perf.enabled;
        workers.push(thread::spawn(move || {
            diff_worker(
                worker_id,
                &worktree_root,
                job_receiver,
                result_sender,
                worker_perf,
                enabled,
            );
        }));
    }
    drop(result_sender);

    let mut completed_commits = 0usize;
    let mut fresh_cache_writes = Vec::<(String, Vec<GitFileChange>)>::new();
    let mut diff_batch = Vec::<Oid>::with_capacity(job_commit_chunk_size);

    for commit_batch in commits.chunks(GIT_CACHE_LOOKUP_BATCH_SIZE) {
        let commit_ids = commit_batch
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let cache_lookup_started = Instant::now();
        let mut cached_changes = (cache.load)(&commit_ids);
        perf.cache_batches += 1;
        perf.cache_lookup_ms += elapsed_ms(cache_lookup_started.elapsed());
        for commit_id in commit_batch {
            let commit_id_string = commit_id.to_string();
            if let Some(cached) = cached_changes.remove(&commit_id_string) {
                perf.cache_hits += 1;
                changes.extend(cached);
                completed_commits += 1;
                progress(GitHistoryProgress {
                    completed_commits,
                    total_commits,
                });
            } else {
                perf.cache_misses += 1;
                diff_batch.push(*commit_id);
                if diff_batch.len() < job_commit_chunk_size {
                    continue;
                }
                send_diff_job(
                    std::mem::take(&mut diff_batch),
                    &job_sender,
                    &result_receiver,
                    &mut |result| {
                        DiffReducer {
                            changes: &mut changes,
                            fresh_cache_writes: &mut fresh_cache_writes,
                            store_commits: &mut cache.store,
                            completed_commits: &mut completed_commits,
                            total_commits,
                            progress: &mut progress,
                            perf,
                        }
                        .handle(result)
                    },
                )
                .map(|stats| perf.record_send_stats(stats))?;
            }
        }
        if !diff_batch.is_empty() {
            send_diff_job(
                std::mem::take(&mut diff_batch),
                &job_sender,
                &result_receiver,
                &mut |result| {
                    DiffReducer {
                        changes: &mut changes,
                        fresh_cache_writes: &mut fresh_cache_writes,
                        store_commits: &mut cache.store,
                        completed_commits: &mut completed_commits,
                        total_commits,
                        progress: &mut progress,
                        perf,
                    }
                    .handle(result)
                },
            )
            .map(|stats| perf.record_send_stats(stats))?;
        }
    }
    drop(job_sender);

    while completed_commits < total_commits {
        let receive_started = Instant::now();
        let result = result_receiver
            .recv()
            .map_err(|_| GitHistoryError::WorkerFailed {
                context: "receiving Git diff worker result",
            })?;
        perf.result_receive_wait_ms += elapsed_ms(receive_started.elapsed());
        DiffReducer {
            changes: &mut changes,
            fresh_cache_writes: &mut fresh_cache_writes,
            store_commits: &mut cache.store,
            completed_commits: &mut completed_commits,
            total_commits,
            progress: &mut progress,
            perf,
        }
        .handle(result)?;
    }

    if completed_commits != total_commits {
        return Err(GitHistoryError::WorkerFailed {
            context: "collecting Git diff worker results",
        });
    }
    if !fresh_cache_writes.is_empty() {
        (cache.store)(&fresh_cache_writes);
        perf.cache_flushes += 1;
    }
    fresh_cache_writes.clear();
    for worker in workers {
        if worker.join().is_err() {
            return Err(GitHistoryError::WorkerFailed {
                context: "joining Git diff worker",
            });
        }
    }

    let sort_started = Instant::now();
    changes.sort_by(|left, right| {
        (
            left.commit_time,
            &left.commit_id,
            &left.path,
            left.change_kind,
            left.added_lines,
            left.deleted_lines,
        )
            .cmp(&(
                right.commit_time,
                &right.commit_id,
                &right.path,
                right.change_kind,
                right.added_lines,
                right.deleted_lines,
            ))
    });
    perf.final_sort_ms = elapsed_ms(sort_started.elapsed());

    Ok(changes)
}

type DiffJob = Vec<Oid>;
type DiffWorkerResult = Result<DiffBatch, GitHistoryError>;

#[derive(Debug)]
struct DiffBatch {
    commits: Vec<(String, Vec<GitFileChange>)>,
    change_count: u64,
    delta_count: u64,
    changed_lines: u64,
}

#[derive(Debug)]
struct DiffCommitOutput {
    commit_id: String,
    changes: Vec<GitFileChange>,
    delta_count: u64,
    changed_lines: u64,
}

fn send_diff_job<F>(
    job: DiffJob,
    sender: &crossbeam_channel::Sender<DiffJob>,
    result_receiver: &Receiver<DiffWorkerResult>,
    handle_result: &mut F,
) -> Result<SendDiffJobStats, GitHistoryError>
where
    F: FnMut(DiffWorkerResult) -> Result<(), GitHistoryError>,
{
    let started = Instant::now();
    let mut stats = SendDiffJobStats::default();
    let mut pending = job;
    loop {
        match sender.try_send(pending) {
            Ok(()) => {
                stats.enqueue_wait_ms = elapsed_ms(started.elapsed());
                return Ok(stats);
            }
            Err(TrySendError::Full(job)) => {
                stats.queue_full_count += 1;
                pending = job;
                let result = result_receiver
                    .recv()
                    .map_err(|_| GitHistoryError::WorkerFailed {
                        context: "receiving Git diff worker result",
                    })?;
                handle_result(result)?;
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(GitHistoryError::WorkerFailed {
                    context: "sending Git diff worker job",
                });
            }
        }
    }
}

struct DiffReducer<'a, F, S> {
    changes: &'a mut Vec<GitFileChange>,
    fresh_cache_writes: &'a mut Vec<(String, Vec<GitFileChange>)>,
    store_commits: &'a mut S,
    completed_commits: &'a mut usize,
    total_commits: usize,
    progress: &'a mut F,
    perf: &'a mut GitPerf,
}

impl<F, S> DiffReducer<'_, F, S>
where
    F: FnMut(GitHistoryProgress),
    S: FnMut(&[(String, Vec<GitFileChange>)]),
{
    fn handle(self, result: DiffWorkerResult) -> Result<(), GitHistoryError> {
        let started = Instant::now();
        let batch = result?;
        self.perf.result_batches += 1;
        self.perf.reducer_commits += batch.commits.len() as u64;
        self.perf.reducer_changes += batch.change_count;
        self.perf.reducer_deltas += batch.delta_count;
        self.perf.reducer_changed_lines += batch.changed_lines;
        for (commit_id, commit_changes) in batch.commits {
            self.changes.extend(commit_changes.clone());
            self.fresh_cache_writes.push((commit_id, commit_changes));
            if self.fresh_cache_writes.len() >= GIT_COMMIT_CACHE_WRITE_BATCH_SIZE {
                (self.store_commits)(self.fresh_cache_writes);
                self.perf.cache_flushes += 1;
                self.fresh_cache_writes.clear();
            }
            *self.completed_commits += 1;
            (self.progress)(GitHistoryProgress {
                completed_commits: *self.completed_commits,
                total_commits: self.total_commits,
            });
        }
        self.perf.reducer_handle_ms += elapsed_ms(started.elapsed());
        Ok(())
    }
}

fn diff_worker(
    worker_id: usize,
    worktree_root: &Path,
    receiver: Receiver<DiffJob>,
    sender: crossbeam_channel::Sender<DiffWorkerResult>,
    worker_perf: Arc<Mutex<Vec<GitWorkerPerf>>>,
    perf_enabled: bool,
) {
    let mut local_perf = GitWorkerPerf {
        worker_id,
        ..GitWorkerPerf::default()
    };
    let repository = match open_repository(worktree_root) {
        Ok(repository) => repository,
        Err(error) => {
            local_perf.errors += 1;
            record_worker_perf(perf_enabled, worker_perf, local_perf);
            let _ = sender.send(Err(error));
            return;
        }
    };

    for job in receiver {
        let active_started = Instant::now();
        let mut commits = Vec::with_capacity(job.len());
        let mut change_count = 0u64;
        let mut delta_count = 0u64;
        let mut changed_lines = 0u64;
        for commit_id in job {
            match diff_commit_file_changes(&repository, commit_id) {
                Ok(output) => {
                    change_count += output.changes.len() as u64;
                    delta_count += output.delta_count;
                    changed_lines += output.changed_lines;
                    commits.push((output.commit_id, output.changes));
                }
                Err(error) => {
                    local_perf.errors += 1;
                    record_worker_perf(perf_enabled, worker_perf, local_perf);
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
        local_perf.active_ms += elapsed_ms(active_started.elapsed());
        local_perf.jobs += 1;
        local_perf.commits += commits.len() as u64;
        local_perf.changes += change_count;
        local_perf.deltas += delta_count;
        local_perf.changed_lines += changed_lines;

        let send_started = Instant::now();
        if sender
            .send(Ok(DiffBatch {
                commits,
                change_count,
                delta_count,
                changed_lines,
            }))
            .is_err()
        {
            local_perf.send_wait_ms += elapsed_ms(send_started.elapsed());
            record_worker_perf(perf_enabled, worker_perf, local_perf);
            return;
        }
        local_perf.send_wait_ms += elapsed_ms(send_started.elapsed());
    }
    record_worker_perf(perf_enabled, worker_perf, local_perf);
}

fn record_worker_perf(
    enabled: bool,
    worker_perf: Arc<Mutex<Vec<GitWorkerPerf>>>,
    local_perf: GitWorkerPerf,
) {
    if enabled {
        if let Ok(mut workers) = worker_perf.lock() {
            workers.push(local_perf);
        }
    }
}

fn diff_commit_file_changes(
    repository: &Repository,
    commit_id: Oid,
) -> Result<DiffCommitOutput, GitHistoryError> {
    let commit = repository
        .find_commit(commit_id)
        .map_err(|source| GitHistoryError::Git {
            context: "loading a reachable commit",
            source,
        })?;
    let commit_id_string = commit.id().to_string();
    let parent_count = commit.parent_count();
    let author = author_identity(&commit_id_string, commit.author())?;
    let commit_time = commit.time().seconds();
    let tree = commit.tree().map_err(|source| GitHistoryError::Git {
        context: "loading a commit tree",
        source,
    })?;
    let parent_tree = if parent_count == 0 {
        None
    } else {
        Some(
            commit
                .parent(0)
                .and_then(|parent| parent.tree())
                .map_err(|source| GitHistoryError::Git {
                    context: "loading the first parent tree",
                    source,
                })?,
        )
    };
    let mut diff_options = DiffOptions::new();
    diff_options.context_lines(0).interhunk_lines(0);
    let diff = repository
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_options))
        .map_err(|source| GitHistoryError::Git {
            context: "diffing commit trees",
            source,
        })?;
    let changes = diff_file_changes(
        &diff,
        commit_id_string.clone(),
        parent_count,
        author,
        commit_time,
    )?;

    let delta_count = changes.len() as u64;
    let changed_lines = changes
        .iter()
        .map(|change| change.added_lines.saturating_add(change.deleted_lines))
        .sum();

    Ok(DiffCommitOutput {
        commit_id: commit_id_string,
        changes,
        delta_count,
        changed_lines,
    })
}

/// Explain local Git history metrics and co-changes for one requested file.
///
/// The requested path may be repository-relative or relative to the current
/// process directory. Output is deterministic and uses repository-relative
/// paths with `/` separators.
pub fn explain_file_from_head(requested_path: impl AsRef<Path>) -> Result<String, GitExplainError> {
    explain_file_from_head_at(Path::new("."), requested_path)
}

/// Explain local Git history metrics and co-changes from a specific worktree
/// context. This is useful for tests and for callers running from subdirs.
pub fn explain_file_from_head_at(
    worktree_path: impl AsRef<Path>,
    requested_path: impl AsRef<Path>,
) -> Result<String, GitExplainError> {
    let worktree_path = worktree_path.as_ref();
    let analysis = analyze_from_head_at(worktree_path)?;

    explain_file_from_analysis_at(&analysis, worktree_path, requested_path)
}

/// Render an explanation from an already-computed repository Git analysis.
pub fn explain_file_from_analysis_at(
    analysis: &GitAnalysis,
    worktree_path: impl AsRef<Path>,
    requested_path: impl AsRef<Path>,
) -> Result<String, GitExplainError> {
    let worktree_path = worktree_path.as_ref();
    let requested_path = requested_path.as_ref();
    let metric_paths = analysis
        .file_metrics
        .iter()
        .map(|metric| metric.path.as_str())
        .collect::<BTreeSet<_>>();
    let path = normalize_requested_file_path(
        worktree_path,
        &analysis.worktree_root,
        requested_path,
        &metric_paths,
    )?;
    let metric = analysis
        .file_metrics
        .iter()
        .find(|metric| metric.path == path);
    let file_changes = analysis
        .changes
        .iter()
        .filter(|change| change.path == path)
        .collect::<Vec<_>>();
    let co_changes = co_changes_for_path(&path, &analysis.co_changes);

    Ok(render_file_explanation(
        &path,
        analysis.head_commit_time,
        metric,
        &file_changes,
        &co_changes,
    ))
}

/// Aggregate raw file changes into deterministic per-file Git metrics.
///
/// `head_commit_time` is the committer timestamp of `HEAD` in seconds since the
/// Unix epoch. Recent churn uses the documented 90-day window relative to that
/// timestamp rather than the machine clock.
pub fn file_metrics_from_changes(
    changes: &[GitFileChange],
    head_commit_time: i64,
) -> Vec<GitFileMetrics> {
    let ownership = operational_ownership_from_changes(changes, head_commit_time);

    file_metrics_from_changes_with_ownership(changes, head_commit_time, &ownership)
}

fn file_metrics_from_changes_with_ownership(
    changes: &[GitFileChange],
    head_commit_time: i64,
    ownership: &OperationalOwnershipSnapshot,
) -> Vec<GitFileMetrics> {
    let recent_threshold = head_commit_time.saturating_sub(RECENT_CHURN_WINDOW_SECONDS);
    let mut by_path = HashMap::<String, FileMetricAccumulator>::new();
    let co_changed_file_counts = co_changed_file_counts_from_changes(changes);
    let ownership_by_path = ownership
        .by_file
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<HashMap<_, _>>();

    for change in changes {
        let accumulator = by_path
            .entry(change.path.clone())
            .or_insert_with(|| FileMetricAccumulator::new(change.path.clone()));

        accumulator.total_churn_added += change.added_lines;
        accumulator.total_churn_deleted += change.deleted_lines;

        if change.commit_time >= recent_threshold {
            accumulator.recent_churn_added += change.added_lines;
            accumulator.recent_churn_deleted += change.deleted_lines;
        }

        accumulator.record_touch(change);
    }

    let mut metrics = by_path
        .into_values()
        .map(|accumulator| {
            let file_ownership = ownership_by_path.get(accumulator.path.as_str()).copied();
            let co_changed_file_count = co_changed_file_counts
                .get(accumulator.path.as_str())
                .copied()
                .unwrap_or(0);

            accumulator.into_metrics(head_commit_time, file_ownership, co_changed_file_count)
        })
        .collect::<Vec<_>>();
    metrics.sort_by(|left, right| left.path.cmp(&right.path));
    metrics
}

/// Aggregate raw file changes into deterministic co-change counts.
///
/// Each commit contributes at most one count for any unordered pair of touched
/// paths. Returned pairs are ranked by count descending, then left path
/// ascending, then right path ascending.
pub fn co_changes_from_changes(changes: &[GitFileChange]) -> Vec<GitCoChange> {
    let mut paths_by_commit = HashMap::<&str, HashSet<&str>>::new();

    for change in changes {
        paths_by_commit
            .entry(change.commit_id.as_str())
            .or_default()
            .insert(change.path.as_str());
    }

    let mut pair_counts = HashMap::<(String, String), u64>::new();

    for paths in paths_by_commit.into_values() {
        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort_unstable();
        if paths.len() > MAX_PAIRWISE_CO_CHANGE_PATHS {
            continue;
        }

        for (left_index, left_path) in paths.iter().enumerate() {
            for right_path in paths.iter().skip(left_index + 1) {
                *pair_counts
                    .entry(((*left_path).to_owned(), (*right_path).to_owned()))
                    .or_insert(0) += 1;
            }
        }
    }

    let mut co_changes = pair_counts
        .into_iter()
        .map(|((left_path, right_path), commit_count)| GitCoChange {
            left_path,
            right_path,
            commit_count,
        })
        .collect::<Vec<_>>();

    co_changes.sort_by(|left, right| {
        right
            .commit_count
            .cmp(&left.commit_count)
            .then_with(|| left.left_path.cmp(&right.left_path))
            .then_with(|| left.right_path.cmp(&right.right_path))
    });

    co_changes
}

fn co_changed_file_counts_from_changes(changes: &[GitFileChange]) -> BTreeMap<String, u64> {
    let mut paths_by_commit = HashMap::<&str, HashSet<&str>>::new();

    for change in changes {
        paths_by_commit
            .entry(change.commit_id.as_str())
            .or_default()
            .insert(change.path.as_str());
    }

    let mut related_by_path = HashMap::<String, HashSet<String>>::new();
    let mut saturated_paths = HashSet::<String>::new();

    for paths in paths_by_commit.into_values() {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if paths.len() <= 1 {
            continue;
        }

        if paths.len().saturating_sub(1) as u64 >= CO_CHANGED_FILE_COUNT_SATURATION {
            for path in paths {
                saturated_paths.insert(path.to_owned());
            }
            continue;
        }

        for (left_index, left_path) in paths.iter().enumerate() {
            if saturated_paths.contains(*left_path) {
                continue;
            }

            let related = related_by_path.entry((*left_path).to_owned()).or_default();
            for (right_index, right_path) in paths.iter().enumerate() {
                if left_index != right_index {
                    related.insert((*right_path).to_owned());
                    if related.len() as u64 >= CO_CHANGED_FILE_COUNT_SATURATION {
                        saturated_paths.insert((*left_path).to_owned());
                        related.clear();
                        break;
                    }
                }
            }
        }
    }

    let mut counts = related_by_path
        .into_iter()
        .map(|(path, related)| (path, related.len() as u64))
        .collect::<BTreeMap<_, _>>();
    for path in saturated_paths {
        counts.insert(path, CO_CHANGED_FILE_COUNT_SATURATION);
    }

    counts
}

fn open_repository(worktree_path: &Path) -> Result<Repository, GitHistoryError> {
    Repository::discover(worktree_path).map_err(|source| {
        if source.code() == ErrorCode::NotFound || source.class() == ErrorClass::Repository {
            GitHistoryError::NotRepository {
                path: worktree_path.to_path_buf(),
                source,
            }
        } else {
            GitHistoryError::OpenRepository {
                path: worktree_path.to_path_buf(),
                source,
            }
        }
    })
}

fn reject_shallow_repository(
    repository: &Repository,
    worktree_path: &Path,
) -> Result<(), GitHistoryError> {
    if repository.is_shallow() {
        Err(GitHistoryError::ShallowRepository {
            path: worktree_path.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

fn head_commit<'repo>(
    repository: &'repo Repository,
    worktree_path: &Path,
) -> Result<git2::Commit<'repo>, GitHistoryError> {
    let head = repository.head().map_err(|source| {
        if matches!(source.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) {
            GitHistoryError::MissingHead {
                path: worktree_path.to_path_buf(),
            }
        } else {
            GitHistoryError::Git {
                context: "reading HEAD",
                source,
            }
        }
    })?;

    head.peel_to_commit()
        .map_err(|source| GitHistoryError::HeadNotCommit {
            path: worktree_path.to_path_buf(),
            source,
        })
}

fn reachable_commits(repository: &Repository, head: Oid) -> Result<Vec<Oid>, GitHistoryError> {
    let mut revwalk = repository
        .revwalk()
        .map_err(|source| GitHistoryError::Git {
            context: "creating the revision walk",
            source,
        })?;
    revwalk
        .set_sorting(Sort::TIME)
        .map_err(|source| GitHistoryError::Git {
            context: "configuring the revision walk",
            source,
        })?;
    revwalk.push(head).map_err(|source| GitHistoryError::Git {
        context: "starting the revision walk at HEAD",
        source,
    })?;

    revwalk
        .map(|commit| {
            commit.map_err(|source| GitHistoryError::Git {
                context: "walking commits reachable from HEAD",
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()
}

fn diff_file_changes(
    diff: &Diff<'_>,
    commit_id: String,
    parent_count: usize,
    author: String,
    commit_time: i64,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    #[derive(Debug)]
    struct PendingChange {
        path: String,
        change_kind: GitChangeKind,
        added_lines: u64,
        deleted_lines: u64,
    }

    let pending_changes = RefCell::new(Vec::<PendingChange>::new());
    let current_change_index = Cell::new(None::<usize>);
    let callback_error = RefCell::new(None::<GitHistoryError>);

    let mut file_cb = |delta: git2::DiffDelta<'_>, _progress: f32| {
        let Some(change_kind) = change_kind(delta.status()) else {
            current_change_index.set(None);
            return true;
        };
        let path = match delta_path(&commit_id, delta) {
            Ok(path) => path,
            Err(error) => {
                *callback_error.borrow_mut() = Some(error);
                return false;
            }
        };
        if is_internal_analysis_path(&path) {
            current_change_index.set(None);
            return true;
        }

        let mut changes = pending_changes.borrow_mut();
        changes.push(PendingChange {
            path,
            change_kind,
            added_lines: 0,
            deleted_lines: 0,
        });
        current_change_index.set(Some(changes.len() - 1));
        true
    };
    let mut line_cb = |_delta: git2::DiffDelta<'_>,
                       _hunk: Option<git2::DiffHunk<'_>>,
                       line: git2::DiffLine<'_>| {
        let Some(index) = current_change_index.get() else {
            return true;
        };
        let mut changes = pending_changes.borrow_mut();
        let Some(change) = changes.get_mut(index) else {
            return true;
        };
        match line.origin_value() {
            DiffLineType::Addition | DiffLineType::AddEOFNL => {
                change.added_lines += 1;
            }
            DiffLineType::Deletion | DiffLineType::DeleteEOFNL => {
                change.deleted_lines += 1;
            }
            _ => {}
        }
        true
    };

    diff.foreach(&mut file_cb, None, None, Some(&mut line_cb))
        .map_err(|source| {
            callback_error
                .borrow_mut()
                .take()
                .unwrap_or(GitHistoryError::Git {
                    context: "counting changed lines",
                    source,
                })
        })?;

    if let Some(error) = callback_error.into_inner() {
        return Err(error);
    }

    let mut changes = pending_changes
        .into_inner()
        .into_iter()
        .map(|change| GitFileChange {
            commit_id: commit_id.clone(),
            parent_count,
            is_merge: parent_count > 1,
            author: author.clone(),
            commit_time,
            path: change.path,
            change_kind: change.change_kind,
            added_lines: change.added_lines,
            deleted_lines: change.deleted_lines,
        })
        .collect::<Vec<_>>();

    changes.sort_by(|left, right| {
        (
            &left.path,
            left.change_kind,
            left.added_lines,
            left.deleted_lines,
        )
            .cmp(&(
                &right.path,
                right.change_kind,
                right.added_lines,
                right.deleted_lines,
            ))
    });

    Ok(changes)
}

fn is_internal_analysis_path(path: &str) -> bool {
    path == ".hotpath" || path.starts_with(".hotpath/")
}

fn author_identity(
    commit_id: &str,
    author: git2::Signature<'_>,
) -> Result<String, GitHistoryError> {
    let name = author
        .name()
        .ok_or_else(|| GitHistoryError::UnsupportedAuthorIdentity {
            commit_id: commit_id.to_owned(),
        })?;
    let email = author
        .email()
        .ok_or_else(|| GitHistoryError::UnsupportedAuthorIdentity {
            commit_id: commit_id.to_owned(),
        })?;

    Ok(format!("{name} <{email}>"))
}

fn delta_path(commit_id: &str, delta: git2::DiffDelta<'_>) -> Result<String, GitHistoryError> {
    let file = if delta.status() == Delta::Deleted {
        delta.old_file()
    } else {
        delta.new_file()
    };
    let path = file.path().and_then(Path::to_str).ok_or_else(|| {
        GitHistoryError::UnsupportedPathEncoding {
            commit_id: commit_id.to_owned(),
        }
    })?;

    Ok(path.replace('\\', "/"))
}

fn change_kind(delta: Delta) -> Option<GitChangeKind> {
    match delta {
        Delta::Added => Some(GitChangeKind::Added),
        Delta::Modified => Some(GitChangeKind::Modified),
        Delta::Deleted => Some(GitChangeKind::Deleted),
        Delta::Renamed => Some(GitChangeKind::Renamed),
        Delta::Copied => Some(GitChangeKind::Copied),
        Delta::Typechange => Some(GitChangeKind::TypeChanged),
        Delta::Unmodified
        | Delta::Ignored
        | Delta::Untracked
        | Delta::Unreadable
        | Delta::Conflicted => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelatedCoChange {
    path: String,
    commit_count: u64,
}

fn co_changes_for_path(path: &str, co_changes: &[GitCoChange]) -> Vec<RelatedCoChange> {
    let mut related = co_changes
        .iter()
        .filter_map(|co_change| {
            if co_change.left_path == path {
                Some(RelatedCoChange {
                    path: co_change.right_path.clone(),
                    commit_count: co_change.commit_count,
                })
            } else if co_change.right_path == path {
                Some(RelatedCoChange {
                    path: co_change.left_path.clone(),
                    commit_count: co_change.commit_count,
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    related.sort_by(|left, right| {
        right
            .commit_count
            .cmp(&left.commit_count)
            .then_with(|| left.path.cmp(&right.path))
    });

    related
}

fn render_file_explanation(
    path: &str,
    head_commit_time: i64,
    metric: Option<&GitFileMetrics>,
    file_changes: &[&GitFileChange],
    co_changes: &[RelatedCoChange],
) -> String {
    let mut output = format!(
        "Hotpath Git explanation\npath: {path}\nhistory scope: local commits reachable from HEAD\nHEAD committer timestamp: {head_commit_time} (Unix seconds)"
    );

    output.push_str("\n\nraw changes");
    if file_changes.is_empty() {
        output.push_str("\n  none");
    } else {
        for change in file_changes {
            output.push_str(&format!(
                "\n  {}  {}  +{} -{}  {}  commit_time {}",
                short_commit_id(&change.commit_id),
                change_kind_label(change.change_kind),
                change.added_lines,
                change.deleted_lines,
                change.author,
                change.commit_time
            ));
        }
    }

    output.push_str("\n\nraw metrics");
    if let Some(metric) = metric {
        let recent_churn = metric.recent_churn_added + metric.recent_churn_deleted;
        let total_churn = metric.total_churn_added + metric.total_churn_deleted;

        output.push_str(&format!(
            "\n  commits per file: {}\n  total churn: {} added, {} deleted, {} combined\n  recent churn (90 days): {} added, {} deleted, {} combined\n  author count: {}\n  owner count: {}",
            metric.commits_per_file,
            metric.total_churn_added,
            metric.total_churn_deleted,
            total_churn,
            metric.recent_churn_added,
            metric.recent_churn_deleted,
            recent_churn,
            metric.author_count,
            metric.owner_count
        ));

        if let (Some(owner), Some(share)) = (&metric.dominant_owner, metric.dominant_owner_share) {
            output.push_str(&format!(
                "\n  dominant owner: {owner} ({:.2}% weighted operational ownership)",
                share * 100.0
            ));
        } else {
            output.push_str("\n  dominant owner: unavailable");
        }

        output.push_str(&format!(
            "\n  first observed commit: {}\n  last observed commit: {}\n  file age: {}",
            observed_commit(metric.first_commit_id.as_deref(), metric.first_commit_time),
            observed_commit(metric.last_commit_id.as_deref(), metric.last_commit_time),
            metric
                .file_age_days
                .map(|days| format!("{days} days"))
                .unwrap_or_else(|| "unavailable".to_owned())
        ));
    } else {
        output.push_str(
            "\n  commits per file: 0\n  total churn: 0 added, 0 deleted, 0 combined\n  recent churn (90 days): 0 added, 0 deleted, 0 combined\n  author count: 0\n  owner count: 0\n  dominant owner: unavailable\n  first observed commit: unavailable\n  last observed commit: unavailable\n  file age: unavailable",
        );
    }

    output.push_str("\n\nco-changes");
    if co_changes.is_empty() {
        output.push_str("\n  none");
    } else {
        for co_change in co_changes {
            output.push_str(&format!(
                "\n  {}  {}",
                co_change.commit_count, co_change.path
            ));
        }
    }

    output.push_str(
        "\n\ncalculation notes\n  - Uses local Git history reachable from HEAD only.\n  - Root commits are diffed against the empty tree; merge commits are diffed against their first parent.\n  - Recent churn uses the 90-day window before the HEAD committer timestamp.\n  - Operational ownership weights changed lines, recency, and sustained file activity.\n  - Co-change rows count commits that touched the requested path and the listed path.",
    );
    output.push_str(
        "\n\nlimitations\n  - Rename handling is conservative: the rename commit counts for the destination path, but earlier history remains under the old path.\n  - Binary changes or unavailable line statistics contribute 0 line churn.\n  - Author identity is the exact commit author string; .mailmap, bot detection, and identity merging are not applied.\n  - File age is clamped to 0 days if commit timestamps place the first observed file touch after HEAD.\n  - Results are advisory and should be treated as local derived cache data when persisted.",
    );

    output
}

fn observed_commit(commit_id: Option<&str>, commit_time: Option<i64>) -> String {
    match (commit_id, commit_time) {
        (Some(commit_id), Some(commit_time)) => {
            format!(
                "{} at commit_time {commit_time}",
                short_commit_id(commit_id)
            )
        }
        _ => "unavailable".to_owned(),
    }
}

fn short_commit_id(commit_id: &str) -> &str {
    commit_id.get(..12).unwrap_or(commit_id)
}

fn change_kind_label(change_kind: GitChangeKind) -> &'static str {
    match change_kind {
        GitChangeKind::Added => "added",
        GitChangeKind::Modified => "modified",
        GitChangeKind::Deleted => "deleted",
        GitChangeKind::Renamed => "renamed",
        GitChangeKind::Copied => "copied",
        GitChangeKind::TypeChanged => "type_changed",
    }
}

fn normalize_requested_file_path(
    worktree_path: &Path,
    workdir: &Path,
    requested_path: &Path,
    metric_paths: &BTreeSet<&str>,
) -> Result<String, GitExplainError> {
    if requested_path.as_os_str().is_empty() {
        return Err(GitExplainError::EmptyPath);
    }

    let workdir = fs::canonicalize(workdir).map_err(|_| GitExplainError::PathOutsideRepository)?;
    let worktree_path =
        fs::canonicalize(worktree_path).map_err(|_| GitExplainError::PathOutsideRepository)?;
    let mut candidates = Vec::new();

    if requested_path.is_absolute() {
        push_relative_candidate(&mut candidates, &workdir, requested_path)?;
    } else {
        push_relative_candidate(
            &mut candidates,
            &workdir,
            &worktree_path.join(requested_path),
        )?;
        push_relative_candidate(&mut candidates, &workdir, &workdir.join(requested_path))?;
    }

    candidates.sort();
    candidates.dedup();

    if candidates.is_empty() {
        return Err(GitExplainError::PathOutsideRepository);
    }

    choose_requested_candidate(&workdir, &candidates, metric_paths)
}

fn push_relative_candidate(
    candidates: &mut Vec<String>,
    workdir: &Path,
    candidate: &Path,
) -> Result<(), GitExplainError> {
    let candidate = lexically_normalize(candidate);
    let Ok(relative) = candidate.strip_prefix(workdir) else {
        return Ok(());
    };
    let relative = portable_relative_path(relative)?;

    if relative.is_empty() {
        return Err(GitExplainError::EmptyPath);
    }

    candidates.push(relative);
    Ok(())
}

fn choose_requested_candidate(
    workdir: &Path,
    candidates: &[String],
    metric_paths: &BTreeSet<&str>,
) -> Result<String, GitExplainError> {
    match crate::select_unique_candidate(candidates, |candidate| metric_paths.contains(candidate)) {
        crate::CandidateSelection::One(candidate) => return Ok(candidate),
        crate::CandidateSelection::Ambiguous { first, second } => {
            return Err(GitExplainError::AmbiguousPath { first, second });
        }
        crate::CandidateSelection::None => {}
    }

    match crate::select_unique_candidate(candidates, |candidate| workdir.join(candidate).exists()) {
        crate::CandidateSelection::One(candidate) => Ok(candidate),
        crate::CandidateSelection::Ambiguous { first, second } => {
            Err(GitExplainError::AmbiguousPath { first, second })
        }
        crate::CandidateSelection::None => candidates
            .first()
            .cloned()
            .ok_or(GitExplainError::PathOutsideRepository),
    }
}

fn portable_relative_path(path: &Path) -> Result<String, GitExplainError> {
    let mut parts = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or(GitExplainError::UnsupportedPathEncoding)?;
                parts.push(part.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(GitExplainError::PathOutsideRepository);
            }
        }
    }

    Ok(parts.join("/"))
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(_) | Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

#[derive(Debug)]
struct FileMetricAccumulator {
    path: String,
    total_churn_added: u64,
    total_churn_deleted: u64,
    recent_churn_added: u64,
    recent_churn_deleted: u64,
    commits: HashSet<String>,
    author_touch_counts: HashMap<String, u64>,
    first_commit: Option<CommitPoint>,
    last_commit: Option<CommitPoint>,
}

impl FileMetricAccumulator {
    fn new(path: String) -> Self {
        Self {
            path,
            total_churn_added: 0,
            total_churn_deleted: 0,
            recent_churn_added: 0,
            recent_churn_deleted: 0,
            commits: HashSet::new(),
            author_touch_counts: HashMap::new(),
            first_commit: None,
            last_commit: None,
        }
    }

    fn record_touch(&mut self, change: &GitFileChange) {
        let point = CommitPoint {
            id: change.commit_id.clone(),
            time: change.commit_time,
        };

        if self.commits.insert(change.commit_id.clone()) {
            *self
                .author_touch_counts
                .entry(change.author.clone())
                .or_insert(0) += 1;
            self.record_commit_point(point);
        }
    }

    fn record_commit_point(&mut self, point: CommitPoint) {
        if self
            .first_commit
            .as_ref()
            .is_none_or(|first| point.sort_key() < first.sort_key())
        {
            self.first_commit = Some(point.clone());
        }

        if self
            .last_commit
            .as_ref()
            .is_none_or(|last| point.sort_key() > last.sort_key())
        {
            self.last_commit = Some(point);
        }
    }

    fn into_metrics(
        self,
        head_commit_time: i64,
        ownership: Option<&crate::ownership::OperationalFileOwnership>,
        co_changed_file_count: u64,
    ) -> GitFileMetrics {
        let commits_per_file = self.commits.len() as u64;
        let author_count = self.author_touch_counts.len() as u64;
        let owner_count = ownership.map_or(0, |ownership| ownership.owners.len() as u64);
        let dominant = ownership.and_then(|ownership| ownership.owners.first());
        let dominant_owner = dominant.map(|owner| owner.author.clone());
        let dominant_owner_share = dominant.map(|owner| owner.share);
        let first_commit = self.first_commit;
        let last_commit = self.last_commit;
        let file_age_days = first_commit.as_ref().map(|commit| {
            if commit.time >= head_commit_time {
                0
            } else {
                ((head_commit_time - commit.time) / SECONDS_PER_DAY) as u64
            }
        });

        GitFileMetrics {
            path: self.path,
            commits_per_file,
            total_churn_added: self.total_churn_added,
            total_churn_deleted: self.total_churn_deleted,
            recent_churn_added: self.recent_churn_added,
            recent_churn_deleted: self.recent_churn_deleted,
            author_count,
            owner_count,
            dominant_owner,
            dominant_owner_share,
            co_changed_file_count,
            first_commit_id: first_commit.as_ref().map(|commit| commit.id.clone()),
            first_commit_time: first_commit.as_ref().map(|commit| commit.time),
            last_commit_id: last_commit.as_ref().map(|commit| commit.id.clone()),
            last_commit_time: last_commit.as_ref().map(|commit| commit.time),
            file_age_days,
        }
    }
}

#[derive(Debug, Clone)]
struct CommitPoint {
    id: String,
    time: i64,
}

impl CommitPoint {
    fn sort_key(&self) -> (i64, &str) {
        (self.time, self.id.as_str())
    }
}
