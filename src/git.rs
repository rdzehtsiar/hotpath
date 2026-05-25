// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use git2::{ErrorClass, ErrorCode, Repository};
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
const GIT_COMMIT_CACHE_WRITE_BATCH_SIZE: usize = 512;
const GIT_PERF_ENV: &str = "HOTPATH_PERF";
const GIT_STREAM_WORKERS_ENV: &str = "HOTPATH_GIT_WORKERS";
const GIT_HISTORY_BACKEND: &str = "cli-stream";
pub const GIT_HISTORY_CACHE_VERSION: &str = "hotpath.git-history-cli.v1";
const GIT_STREAM_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const GIT_STREAM_PERF_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
const GIT_RECORD_SEPARATOR: u8 = 0x1e;

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

#[derive(Debug)]
struct GitStreamPerf {
    enabled: bool,
    started_at: Instant,
    last_progress_at: Instant,
    last_snapshot_at: Instant,
    total_commits: usize,
    cached_commits: u64,
    missing_commits: u64,
    stream_workers: u64,
    completed_commits: u64,
    cache_batches: u64,
    cache_lookup_ms: u128,
    cache_hits: u64,
    cache_misses: u64,
    rev_list_ms: u128,
    spawn_ms: u128,
    stdin_write_ms: u128,
    stdout_read_ms: u128,
    stdout_bytes: u64,
    parsed_commits: u64,
    raw_records: u64,
    numstat_records: u64,
    parsed_changes: u64,
    cache_flushes: u64,
    final_sort_ms: u128,
    aggregation_ms: u128,
}

impl GitStreamPerf {
    fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            started_at: now,
            last_progress_at: now,
            last_snapshot_at: now,
            total_commits: 0,
            cached_commits: 0,
            missing_commits: 0,
            stream_workers: 0,
            completed_commits: 0,
            cache_batches: 0,
            cache_lookup_ms: 0,
            cache_hits: 0,
            cache_misses: 0,
            rev_list_ms: 0,
            spawn_ms: 0,
            stdin_write_ms: 0,
            stdout_read_ms: 0,
            stdout_bytes: 0,
            parsed_commits: 0,
            raw_records: 0,
            numstat_records: 0,
            parsed_changes: 0,
            cache_flushes: 0,
            final_sort_ms: 0,
            aggregation_ms: 0,
        }
    }

    fn disabled() -> Self {
        Self::new(false)
    }

    fn emit_started(&self) {
        operation_log::event(
            "hotpath.git_analysis_started",
            json!({
                "backend": GIT_HISTORY_BACKEND,
                "total_commits": self.total_commits,
                "cached_commits": self.cached_commits,
                "missing_commits": self.missing_commits,
                "stream_workers": self.stream_workers,
                "cache_key": GIT_HISTORY_CACHE_VERSION,
            }),
        );
    }

    fn maybe_emit_progress(&mut self) {
        if self.last_progress_at.elapsed() < GIT_STREAM_PROGRESS_INTERVAL {
            return;
        }
        self.last_progress_at = Instant::now();
        operation_log::event("hotpath.git_analysis_progress", self.progress_payload());
    }

    fn maybe_emit_perf_snapshot(&mut self) {
        if !self.enabled || self.last_snapshot_at.elapsed() < GIT_STREAM_PERF_SNAPSHOT_INTERVAL {
            return;
        }
        self.last_snapshot_at = Instant::now();
        operation_log::event("hotpath.git_stream_perf_snapshot", self.summary_payload());
    }

    fn emit_completed(&self, changes: usize, file_metrics: usize, co_changes: usize) {
        operation_log::event(
            "hotpath.git_analysis_completed",
            json!({
                "backend": GIT_HISTORY_BACKEND,
                "elapsed_ms": elapsed_ms(self.started_at.elapsed()),
                "commits_per_second": commits_per_second(self.completed_commits, self.started_at.elapsed()),
                "completed_commits": self.completed_commits,
                "total_commits": self.total_commits,
                "changes": changes,
                "file_metrics": file_metrics,
                "co_changes": co_changes,
                "cache_hits": self.cache_hits,
                "cache_misses": self.cache_misses,
            }),
        );
    }

    fn emit_failed(&self, phase: &'static str, error: &str) {
        operation_log::event(
            "hotpath.git_analysis_failed",
            json!({
                "backend": GIT_HISTORY_BACKEND,
                "phase": phase,
                "elapsed_ms": elapsed_ms(self.started_at.elapsed()),
                "completed_commits": self.completed_commits,
                "total_commits": self.total_commits,
                "error": error,
            }),
        );
    }

    fn emit_perf_summary(&self) {
        if self.enabled {
            operation_log::event("hotpath.git_stream_perf_summary", self.summary_payload());
        }
    }

    fn progress_payload(&self) -> serde_json::Value {
        json!({
            "backend": GIT_HISTORY_BACKEND,
            "elapsed_ms": elapsed_ms(self.started_at.elapsed()),
            "completed_commits": self.completed_commits,
            "total_commits": self.total_commits,
            "commits_per_second": commits_per_second(self.completed_commits, self.started_at.elapsed()),
            "cache_hits": self.cache_hits,
            "cache_misses": self.cache_misses,
            "parsed_commits": self.parsed_commits,
        })
    }

    fn summary_payload(&self) -> serde_json::Value {
        json!({
            "backend": GIT_HISTORY_BACKEND,
            "elapsed_ms": elapsed_ms(self.started_at.elapsed()),
            "total_commits": self.total_commits,
            "cached_commits": self.cached_commits,
            "missing_commits": self.missing_commits,
            "stream_workers": self.stream_workers,
            "completed_commits": self.completed_commits,
            "commits_per_second": commits_per_second(self.completed_commits, self.started_at.elapsed()),
            "rev_list_ms": self.rev_list_ms,
            "cache_batches": self.cache_batches,
            "cache_lookup_ms": self.cache_lookup_ms,
            "cache_hits": self.cache_hits,
            "cache_misses": self.cache_misses,
            "spawn_ms": self.spawn_ms,
            "stdin_write_ms": self.stdin_write_ms,
            "stdout_read_ms": self.stdout_read_ms,
            "stdout_bytes": self.stdout_bytes,
            "parsed_commits": self.parsed_commits,
            "raw_records": self.raw_records,
            "numstat_records": self.numstat_records,
            "parsed_changes": self.parsed_changes,
            "cache_flushes": self.cache_flushes,
            "final_sort_ms": self.final_sort_ms,
            "aggregation_ms": self.aggregation_ms,
        })
    }
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

fn git_stream_worker_count() -> usize {
    env::var(GIT_STREAM_WORKERS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|workers| *workers > 0)
        .unwrap_or_else(|| {
            thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        })
}

fn elapsed_ms(duration: Duration) -> u128 {
    duration.as_millis()
}

fn commits_per_second(completed_commits: u64, duration: Duration) -> u64 {
    let elapsed = duration.as_secs_f64();
    if elapsed <= 0.0 {
        0
    } else {
        (completed_commits as f64 / elapsed).round() as u64
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
    GitCommandSpawn {
        context: &'static str,
        source: io::Error,
    },
    GitCommandFailed {
        context: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    MalformedGitStream {
        context: &'static str,
        detail: String,
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
            Self::GitCommandSpawn { source, .. } => Some(source),
            Self::MissingHead { .. }
            | Self::ShallowRepository { .. }
            | Self::BareRepository { .. }
            | Self::GitCommandFailed { .. }
            | Self::MalformedGitStream { .. }
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
        GitHistoryError::GitCommandSpawn { context, source } => write!(
            f,
            "failed to run Git while {context}: {source}; ensure the git executable is available on PATH"
        ),
        GitHistoryError::GitCommandFailed {
            context,
            status,
            stderr,
        } => write!(
            f,
            "Git command failed while {context} with status {}: {}",
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            stderr
        ),
        GitHistoryError::MalformedGitStream { context, detail } => {
            write!(f, "failed to parse Git history while {context}: {detail}")
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

    file_changes_from_repository(&repository, &head_commit.id().to_string())
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
    let mut perf = GitStreamPerf::new(git_perf_enabled());
    let changes = file_changes_from_repository_with_progress_and_cache(
        &worktree_root,
        &head_commit_id.to_string(),
        progress,
        GitCacheCallbacks {
            load: load_cached_commits,
            store: store_commits,
        },
        &mut perf,
    )?;
    let aggregation_started = Instant::now();
    let ownership = operational_ownership_from_changes(&changes, head_commit_time);
    let file_metrics =
        file_metrics_from_changes_with_ownership(&changes, head_commit_time, &ownership);
    let co_changes = co_changes_from_changes(&changes);
    perf.aggregation_ms = elapsed_ms(aggregation_started.elapsed());
    perf.emit_completed(changes.len(), file_metrics.len(), co_changes.len());
    perf.emit_perf_summary();

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
    head_commit_id: &str,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    file_changes_from_repository_with_progress(repository, head_commit_id, |_| {})
}

fn file_changes_from_repository_with_progress<F>(
    repository: &Repository,
    head_commit_id: &str,
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
    let mut perf = GitStreamPerf::disabled();
    file_changes_from_repository_with_progress_and_cache(
        &worktree_root,
        head_commit_id,
        progress,
        GitCacheCallbacks {
            load: no_cached_commits,
            store: ignore_cached_commits,
        },
        &mut perf,
    )
}

fn no_cached_commits(_: &[String]) -> BTreeMap<String, Vec<GitFileChange>> {
    BTreeMap::new()
}

fn ignore_cached_commits(_: &[(String, Vec<GitFileChange>)]) {}

fn file_changes_from_repository_with_progress_and_cache<F, L, S>(
    worktree_root: &Path,
    head_commit_id: &str,
    mut progress: F,
    mut cache: GitCacheCallbacks<L, S>,
    perf: &mut GitStreamPerf,
) -> Result<Vec<GitFileChange>, GitHistoryError>
where
    F: FnMut(GitHistoryProgress),
    L: FnMut(&[String]) -> BTreeMap<String, Vec<GitFileChange>>,
    S: FnMut(&[(String, Vec<GitFileChange>)]),
{
    let rev_list_started = Instant::now();
    let commits = reachable_commit_ids(worktree_root, head_commit_id)?;
    perf.rev_list_ms = elapsed_ms(rev_list_started.elapsed());
    let mut changes = Vec::new();
    let total_commits = commits.len();
    perf.total_commits = total_commits;
    if total_commits == 0 {
        perf.emit_started();
        return Ok(changes);
    }

    let mut completed_commits = 0usize;
    let mut fresh_cache_writes = Vec::<(String, Vec<GitFileChange>)>::new();
    let mut missing_commits = Vec::<String>::new();

    for commit_batch in commits.chunks(GIT_CACHE_LOOKUP_BATCH_SIZE) {
        let commit_ids = commit_batch.to_vec();
        let cache_lookup_started = Instant::now();
        let mut cached_changes = (cache.load)(&commit_ids);
        perf.cache_batches += 1;
        perf.cache_lookup_ms += elapsed_ms(cache_lookup_started.elapsed());
        for commit_id in commit_batch {
            if let Some(cached) = cached_changes.remove(commit_id) {
                perf.cache_hits += 1;
                perf.cached_commits += 1;
                changes.extend(cached);
                completed_commits += 1;
                perf.completed_commits = completed_commits as u64;
                progress(GitHistoryProgress {
                    completed_commits,
                    total_commits,
                });
                perf.maybe_emit_progress();
            } else {
                perf.cache_misses += 1;
                missing_commits.push(commit_id.clone());
            }
        }
    }
    perf.missing_commits = missing_commits.len() as u64;
    perf.emit_started();

    if !missing_commits.is_empty() {
        stream_missing_commits(
            worktree_root,
            &missing_commits,
            |commit_id, commit_changes, perf| {
                changes.extend(commit_changes.iter().cloned());
                fresh_cache_writes.push((commit_id.to_owned(), commit_changes.to_vec()));
                if fresh_cache_writes.len() >= GIT_COMMIT_CACHE_WRITE_BATCH_SIZE {
                    (cache.store)(&fresh_cache_writes);
                    perf.cache_flushes += 1;
                    fresh_cache_writes.clear();
                }
                completed_commits += 1;
                perf.completed_commits = completed_commits as u64;
                progress(GitHistoryProgress {
                    completed_commits,
                    total_commits,
                });
                perf.maybe_emit_progress();
            },
            perf,
        )
        .inspect_err(|error| {
            perf.emit_failed("streaming missing commits", &error.to_string());
        })?;
    }

    if !fresh_cache_writes.is_empty() {
        (cache.store)(&fresh_cache_writes);
        perf.cache_flushes += 1;
    }
    fresh_cache_writes.clear();

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

fn stream_missing_commits<F>(
    worktree_root: &Path,
    missing_commits: &[String],
    mut handle_commit: F,
    perf: &mut GitStreamPerf,
) -> Result<(), GitHistoryError>
where
    F: FnMut(&str, &[GitFileChange], &mut GitStreamPerf),
{
    let worker_count = git_stream_worker_count().min(missing_commits.len()).max(1);
    perf.stream_workers = worker_count as u64;
    if worker_count == 1 {
        return stream_missing_commits_worker(worktree_root, missing_commits, handle_commit, perf);
    }

    let (sender, receiver) = mpsc::channel::<GitStreamWorkerMessage>();
    let chunks = split_commit_ids_for_workers(missing_commits, worker_count);
    let mut workers = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let worker_root = worktree_root.to_path_buf();
        let worker_sender = sender.clone();
        workers.push(thread::spawn(move || {
            let mut worker_perf = GitStreamPerf::disabled();
            let result = stream_missing_commits_worker(
                &worker_root,
                &chunk,
                |commit_id, changes, _| {
                    let _ = worker_sender.send(GitStreamWorkerMessage::Commit(StreamedGitCommit {
                        commit_id: commit_id.to_owned(),
                        changes: changes.to_vec(),
                    }));
                },
                &mut worker_perf,
            );
            match result {
                Ok(()) => {
                    let _ = worker_sender.send(GitStreamWorkerMessage::Perf(
                        GitStreamWorkerPerf::from(&worker_perf),
                    ));
                }
                Err(error) => {
                    let _ = worker_sender.send(GitStreamWorkerMessage::Error(error));
                }
            }
        }));
    }
    drop(sender);

    let mut first_error = None;
    for message in receiver {
        match message {
            GitStreamWorkerMessage::Commit(commit) => {
                handle_commit(&commit.commit_id, &commit.changes, perf);
                perf.maybe_emit_perf_snapshot();
            }
            GitStreamWorkerMessage::Perf(worker_perf) => {
                worker_perf.add_to(perf);
            }
            GitStreamWorkerMessage::Error(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    let mut worker_failed = false;
    for worker in workers {
        if worker.join().is_err() {
            worker_failed = true;
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if worker_failed {
        return Err(GitHistoryError::WorkerFailed {
            context: "streaming Git history",
        });
    }

    Ok(())
}

fn stream_missing_commits_worker<F>(
    worktree_root: &Path,
    missing_commits: &[String],
    mut handle_commit: F,
    perf: &mut GitStreamPerf,
) -> Result<(), GitHistoryError>
where
    F: FnMut(&str, &[GitFileChange], &mut GitStreamPerf),
{
    let spawn_started = Instant::now();
    let mut child = Command::new("git")
        .args([
            "show",
            "--stdin",
            "--raw",
            "--numstat",
            "-z",
            "--no-renames",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-color",
            "--format=%x1e%H%x00%ct%x00%an%x00%ae%x00%P%x00",
        ])
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|source| GitHistoryError::GitCommandSpawn {
            context: "starting Git history stream",
            source,
        })?;
    perf.spawn_ms = elapsed_ms(spawn_started.elapsed());

    let mut stdin = child
        .stdin
        .take()
        .expect("piped stdin should be available for git show");
    let ids = missing_commits.to_vec();
    let stdin_writer = thread::spawn(move || -> io::Result<u128> {
        let started = Instant::now();
        for commit_id in ids {
            stdin.write_all(commit_id.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        Ok(elapsed_ms(started.elapsed()))
    });

    let mut stderr = child
        .stderr
        .take()
        .expect("piped stderr should be available for git show");
    let stderr_reader = thread::spawn(move || {
        let mut stderr_bytes = Vec::new();
        let _ = stderr.read_to_end(&mut stderr_bytes);
        String::from_utf8_lossy(&stderr_bytes).trim().to_owned()
    });

    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout should be available for git show");
    let read_started = Instant::now();
    let mut buffer = Vec::<u8>::new();
    let mut read_buffer = [0u8; 64 * 1024];
    loop {
        let read =
            stdout
                .read(&mut read_buffer)
                .map_err(|source| GitHistoryError::GitCommandSpawn {
                    context: "reading Git history stream",
                    source,
                })?;
        if read == 0 {
            break;
        }
        perf.stdout_bytes += read as u64;
        buffer.extend_from_slice(&read_buffer[..read]);
        drain_complete_git_records(&mut buffer, &mut handle_commit, perf)?;
        perf.maybe_emit_perf_snapshot();
    }
    perf.stdout_read_ms = elapsed_ms(read_started.elapsed());
    drain_final_git_record(&mut buffer, &mut handle_commit, perf)?;

    perf.stdin_write_ms = stdin_writer.join().ok().and_then(Result::ok).unwrap_or(0);
    let stderr = stderr_reader.join().unwrap_or_default();
    let status = child
        .wait()
        .map_err(|source| GitHistoryError::GitCommandSpawn {
            context: "waiting for Git history stream",
            source,
        })?;
    if !status.success() {
        return Err(GitHistoryError::GitCommandFailed {
            context: "streaming Git history",
            status: status.code(),
            stderr,
        });
    }

    Ok(())
}

#[derive(Debug)]
struct StreamedGitCommit {
    commit_id: String,
    changes: Vec<GitFileChange>,
}

#[derive(Debug)]
enum GitStreamWorkerMessage {
    Commit(StreamedGitCommit),
    Perf(GitStreamWorkerPerf),
    Error(GitHistoryError),
}

#[derive(Debug, Clone, Copy, Default)]
struct GitStreamWorkerPerf {
    spawn_ms: u128,
    stdin_write_ms: u128,
    stdout_read_ms: u128,
    stdout_bytes: u64,
    parsed_commits: u64,
    raw_records: u64,
    numstat_records: u64,
    parsed_changes: u64,
}

impl GitStreamWorkerPerf {
    fn from(perf: &GitStreamPerf) -> Self {
        Self {
            spawn_ms: perf.spawn_ms,
            stdin_write_ms: perf.stdin_write_ms,
            stdout_read_ms: perf.stdout_read_ms,
            stdout_bytes: perf.stdout_bytes,
            parsed_commits: perf.parsed_commits,
            raw_records: perf.raw_records,
            numstat_records: perf.numstat_records,
            parsed_changes: perf.parsed_changes,
        }
    }

    fn add_to(self, perf: &mut GitStreamPerf) {
        perf.spawn_ms += self.spawn_ms;
        perf.stdin_write_ms += self.stdin_write_ms;
        perf.stdout_read_ms += self.stdout_read_ms;
        perf.stdout_bytes += self.stdout_bytes;
        perf.parsed_commits += self.parsed_commits;
        perf.raw_records += self.raw_records;
        perf.numstat_records += self.numstat_records;
        perf.parsed_changes += self.parsed_changes;
    }
}

fn split_commit_ids_for_workers(commit_ids: &[String], worker_count: usize) -> Vec<Vec<String>> {
    let worker_count = worker_count.min(commit_ids.len()).max(1);
    let base = commit_ids.len() / worker_count;
    let extra = commit_ids.len() % worker_count;
    let mut chunks = Vec::with_capacity(worker_count);
    let mut start = 0usize;
    for worker_index in 0..worker_count {
        let len = base + usize::from(worker_index < extra);
        let end = start + len;
        chunks.push(commit_ids[start..end].to_vec());
        start = end;
    }
    chunks
}

fn drain_complete_git_records<F>(
    buffer: &mut Vec<u8>,
    handle_commit: &mut F,
    perf: &mut GitStreamPerf,
) -> Result<(), GitHistoryError>
where
    F: FnMut(&str, &[GitFileChange], &mut GitStreamPerf),
{
    discard_before_record_separator(buffer);
    while buffer.first() == Some(&GIT_RECORD_SEPARATOR) {
        let Some(next_separator) = buffer
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, byte)| (*byte == GIT_RECORD_SEPARATOR).then_some(index))
        else {
            break;
        };
        let record = buffer[..next_separator].to_vec();
        buffer.drain(..next_separator);
        handle_git_record(&record, handle_commit, perf)?;
    }
    Ok(())
}

fn drain_final_git_record<F>(
    buffer: &mut Vec<u8>,
    handle_commit: &mut F,
    perf: &mut GitStreamPerf,
) -> Result<(), GitHistoryError>
where
    F: FnMut(&str, &[GitFileChange], &mut GitStreamPerf),
{
    discard_before_record_separator(buffer);
    if buffer.first() == Some(&GIT_RECORD_SEPARATOR) && buffer.len() > 1 {
        let record = std::mem::take(buffer);
        handle_git_record(&record, handle_commit, perf)?;
    }
    Ok(())
}

fn discard_before_record_separator(buffer: &mut Vec<u8>) {
    if buffer.first() == Some(&GIT_RECORD_SEPARATOR) {
        return;
    }
    if let Some(separator) = buffer.iter().position(|byte| *byte == GIT_RECORD_SEPARATOR) {
        buffer.drain(..separator);
    } else {
        buffer.clear();
    }
}

fn handle_git_record<F>(
    record: &[u8],
    handle_commit: &mut F,
    perf: &mut GitStreamPerf,
) -> Result<(), GitHistoryError>
where
    F: FnMut(&str, &[GitFileChange], &mut GitStreamPerf),
{
    let parsed = parse_git_stream_record(record)?;
    perf.parsed_commits += 1;
    perf.raw_records += parsed.raw_records;
    perf.numstat_records += parsed.numstat_records;
    perf.parsed_changes += parsed.changes.len() as u64;
    handle_commit(&parsed.commit_id, &parsed.changes, perf);
    Ok(())
}

#[derive(Debug)]
struct ParsedGitRecord {
    commit_id: String,
    changes: Vec<GitFileChange>,
    raw_records: u64,
    numstat_records: u64,
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

fn reachable_commit_ids(worktree_root: &Path, head: &str) -> Result<Vec<String>, GitHistoryError> {
    let output = Command::new("git")
        .args(["rev-list", "--date-order", head])
        .current_dir(worktree_root)
        .env("LC_ALL", "C")
        .output()
        .map_err(|source| GitHistoryError::GitCommandSpawn {
            context: "listing reachable commits",
            source,
        })?;

    if !output.status.success() {
        return Err(GitHistoryError::GitCommandFailed {
            context: "listing reachable commits",
            status: output.status.code(),
            stderr: sanitize_git_stderr(&output.stderr),
        });
    }

    String::from_utf8(output.stdout)
        .map_err(|source| GitHistoryError::MalformedGitStream {
            context: "listing reachable commits",
            detail: format!("rev-list output is not UTF-8: {source}"),
        })
        .map(|stdout| {
            stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
}

fn parse_git_stream_record(record: &[u8]) -> Result<ParsedGitRecord, GitHistoryError> {
    let record = record
        .strip_prefix(&[GIT_RECORD_SEPARATOR])
        .ok_or_else(|| malformed_stream("commit record", "missing record separator"))?;
    let mut fields = record.split(|byte| *byte == 0);
    let commit_id = utf8_field(fields.next(), "commit id", "")?;
    let commit_time = parse_i64_field(fields.next(), "commit time", &commit_id)?;
    let author_name = utf8_field(fields.next(), "author name", &commit_id)?;
    let author_email = utf8_field(fields.next(), "author email", &commit_id)?;
    let parents = utf8_field(fields.next(), "parent ids", &commit_id)?;
    let terminator = fields
        .next()
        .ok_or_else(|| malformed_stream("commit header", "missing header terminator"))?;
    if !terminator.is_empty() {
        return Err(malformed_stream(
            "commit header",
            "header terminator was not empty",
        ));
    }

    let parent_count = parents.split_whitespace().count();
    let author = format!("{author_name} <{author_email}>");
    let mut kinds_by_path = BTreeMap::<String, GitChangeKind>::new();
    let mut numstats = Vec::<(String, u64, u64)>::new();
    let mut raw_records = 0u64;
    let mut numstat_records = 0u64;
    let tokens = fields.collect::<Vec<_>>();
    let mut index = 0usize;
    while index < tokens.len() {
        let token = trim_record_token(tokens[index]);
        index += 1;
        if token.is_empty() {
            continue;
        }
        if token.starts_with(b":") {
            raw_records += 1;
            let Some(change_kind) = raw_change_kind(token) else {
                continue;
            };
            let path_token = tokens
                .get(index)
                .copied()
                .ok_or_else(|| malformed_stream("raw record", "missing raw path"))?;
            index += 1;
            let mut path = utf8_bytes(path_token, "raw path", &commit_id)?;
            if matches!(change_kind, GitChangeKind::Renamed | GitChangeKind::Copied) {
                if let Some(destination) = tokens.get(index).copied() {
                    path = utf8_bytes(destination, "raw destination path", &commit_id)?;
                    index += 1;
                }
            }
            let path = normalize_git_path(path);
            if !is_internal_analysis_path(&path) {
                kinds_by_path.insert(path, change_kind);
            }
            continue;
        }

        if let Some((path, added, deleted)) = parse_numstat_token(token, &commit_id)? {
            numstat_records += 1;
            if !is_internal_analysis_path(&path) {
                numstats.push((path, added, deleted));
            }
        }
    }

    let mut changes = numstats
        .into_iter()
        .map(|(path, added_lines, deleted_lines)| {
            let change_kind = kinds_by_path
                .get(&path)
                .copied()
                .unwrap_or(GitChangeKind::Modified);
            GitFileChange {
                commit_id: commit_id.clone(),
                parent_count,
                is_merge: parent_count > 1,
                author: author.clone(),
                commit_time,
                path,
                change_kind,
                added_lines,
                deleted_lines,
            }
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

    Ok(ParsedGitRecord {
        commit_id,
        changes,
        raw_records,
        numstat_records,
    })
}

fn trim_record_token(token: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while token
        .get(start)
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        start += 1;
    }
    &token[start..]
}

fn raw_change_kind(token: &[u8]) -> Option<GitChangeKind> {
    let status = token.rsplit(|byte| *byte == b' ').next()?;
    match status.first().copied()? {
        b'A' => Some(GitChangeKind::Added),
        b'M' => Some(GitChangeKind::Modified),
        b'D' => Some(GitChangeKind::Deleted),
        b'R' => Some(GitChangeKind::Renamed),
        b'C' => Some(GitChangeKind::Copied),
        b'T' => Some(GitChangeKind::TypeChanged),
        _ => None,
    }
}

fn parse_numstat_token(
    token: &[u8],
    commit_id: &str,
) -> Result<Option<(String, u64, u64)>, GitHistoryError> {
    let mut fields = token.splitn(3, |byte| *byte == b'\t');
    let Some(added) = fields.next() else {
        return Ok(None);
    };
    let Some(deleted) = fields.next() else {
        return Ok(None);
    };
    let Some(path) = fields.next() else {
        return Ok(None);
    };

    let added = parse_numstat_count(added, "added line count", commit_id)?;
    let deleted = parse_numstat_count(deleted, "deleted line count", commit_id)?;
    let path = normalize_git_path(utf8_bytes(path, "numstat path", commit_id)?);

    Ok(Some((path, added, deleted)))
}

fn parse_numstat_count(
    value: &[u8],
    field: &'static str,
    commit_id: &str,
) -> Result<u64, GitHistoryError> {
    if value == b"-" {
        return Ok(0);
    }
    let value = utf8_bytes(value, field, commit_id)?;
    value.parse::<u64>().map_err(|source| {
        malformed_stream(field, format!("invalid count in {commit_id}: {source}"))
    })
}

fn is_internal_analysis_path(path: &str) -> bool {
    path == ".hotpath" || path.starts_with(".hotpath/")
}

fn utf8_field(
    field: Option<&[u8]>,
    name: &'static str,
    commit_id: &str,
) -> Result<String, GitHistoryError> {
    let field = field.ok_or_else(|| malformed_stream(name, "missing field"))?;
    utf8_bytes(field, name, commit_id)
}

fn utf8_bytes(
    bytes: &[u8],
    name: &'static str,
    commit_id: &str,
) -> Result<String, GitHistoryError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        if matches!(name, "author name" | "author email") {
            GitHistoryError::UnsupportedAuthorIdentity {
                commit_id: commit_id.to_owned(),
            }
        } else {
            GitHistoryError::UnsupportedPathEncoding {
                commit_id: commit_id.to_owned(),
            }
        }
    })
}

fn parse_i64_field(
    field: Option<&[u8]>,
    name: &'static str,
    commit_id: &str,
) -> Result<i64, GitHistoryError> {
    let value = utf8_field(field, name, commit_id)?;
    value.parse::<i64>().map_err(|source| {
        malformed_stream(name, format!("invalid integer in {commit_id}: {source}"))
    })
}

fn normalize_git_path(path: String) -> String {
    path.replace('\\', "/")
}

fn malformed_stream(context: &'static str, detail: impl Into<String>) -> GitHistoryError {
    GitHistoryError::MalformedGitStream {
        context,
        detail: detail.into(),
    }
}

fn sanitize_git_stderr(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "no stderr output".to_owned()
    } else {
        trimmed.lines().take(5).collect::<Vec<_>>().join(" ")
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

#[cfg(test)]
mod tests {
    use super::{
        parse_git_stream_record, split_commit_ids_for_workers, GitChangeKind, GitHistoryError,
        GIT_RECORD_SEPARATOR,
    };

    #[test]
    fn stream_parser_joins_raw_status_and_numstat_rows() {
        let record = stream_record(
            "abc",
            "100",
            "Ada",
            "ada@example.invalid",
            "parent",
            [
                b":000000 100644 0000000 1111111 A".as_slice(),
                b"src/lib.rs",
                b"3\t0\tsrc/lib.rs",
            ],
        );

        let parsed = parse_git_stream_record(&record).expect("record should parse");

        assert_eq!(parsed.commit_id, "abc");
        assert_eq!(parsed.raw_records, 1);
        assert_eq!(parsed.numstat_records, 1);
        assert_eq!(parsed.changes.len(), 1);
        assert_eq!(parsed.changes[0].change_kind, GitChangeKind::Added);
        assert_eq!(parsed.changes[0].added_lines, 3);
        assert_eq!(parsed.changes[0].deleted_lines, 0);
        assert_eq!(parsed.changes[0].author, "Ada <ada@example.invalid>");
    }

    #[test]
    fn stream_parser_treats_binary_numstat_counts_as_zero() {
        let record = stream_record(
            "bin",
            "100",
            "Binary",
            "binary@example.invalid",
            "parent",
            [
                b":100644 100644 1111111 2222222 M".as_slice(),
                b"image.bin",
                b"-\t-\timage.bin",
            ],
        );

        let parsed = parse_git_stream_record(&record).expect("record should parse");

        assert_eq!(parsed.changes[0].path, "image.bin");
        assert_eq!(parsed.changes[0].added_lines, 0);
        assert_eq!(parsed.changes[0].deleted_lines, 0);
    }

    #[test]
    fn stream_parser_keeps_tabs_inside_paths_after_numstat_counts() {
        let record = stream_record(
            "tab",
            "100",
            "Tab",
            "tab@example.invalid",
            "parent",
            [
                b":100644 100644 1111111 2222222 M".as_slice(),
                b"src/has\ttab.rs",
                b"1\t2\tsrc/has\ttab.rs",
            ],
        );

        let parsed = parse_git_stream_record(&record).expect("record should parse");

        assert_eq!(parsed.changes[0].path, "src/has\ttab.rs");
        assert_eq!(parsed.changes[0].added_lines, 1);
        assert_eq!(parsed.changes[0].deleted_lines, 2);
    }

    #[test]
    fn stream_parser_rejects_malformed_headers() {
        let error = parse_git_stream_record(&[GIT_RECORD_SEPARATOR, b'a', 0])
            .expect_err("malformed record should fail");

        assert!(matches!(error, GitHistoryError::MalformedGitStream { .. }));
    }

    #[test]
    fn stream_worker_chunks_are_contiguous_and_balanced() {
        let commits = (0..10)
            .map(|index| format!("commit-{index}"))
            .collect::<Vec<_>>();

        let chunks = split_commit_ids_for_workers(&commits, 3);

        assert_eq!(
            chunks[0],
            vec!["commit-0", "commit-1", "commit-2", "commit-3"]
        );
        assert_eq!(chunks[1], vec!["commit-4", "commit-5", "commit-6"]);
        assert_eq!(chunks[2], vec!["commit-7", "commit-8", "commit-9"]);
        assert_eq!(chunks.into_iter().flatten().collect::<Vec<_>>(), commits);
    }

    fn stream_record<const N: usize>(
        commit_id: &str,
        commit_time: &str,
        author_name: &str,
        author_email: &str,
        parents: &str,
        body_tokens: [&[u8]; N],
    ) -> Vec<u8> {
        let mut record = Vec::new();
        record.push(GIT_RECORD_SEPARATOR);
        for field in [commit_id, commit_time, author_name, author_email, parents] {
            record.extend_from_slice(field.as_bytes());
            record.push(0);
        }
        record.push(0);
        for token in body_tokens {
            record.extend_from_slice(token);
            record.push(0);
        }
        record
    }
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
