// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ErrorClass, ErrorCode, Oid, Patch, Repository, Sort,
};

pub const RECENT_CHURN_WINDOW_DAYS: i64 = 90;
const SECONDS_PER_DAY: i64 = 86_400;
const RECENT_CHURN_WINDOW_SECONDS: i64 = RECENT_CHURN_WINDOW_DAYS * SECONDS_PER_DAY;

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// Author identity with the most file-touching commits for this path.
    pub dominant_owner: Option<String>,
    /// Dominant owner's share of file-touching commits for this path.
    pub dominant_owner_share: Option<f64>,
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
        match self {
            Self::NotRepository { path, source } => write!(
                f,
                "failed to open Git repository from '{}': path is not a readable Git worktree ({source})",
                path.display()
            ),
            Self::OpenRepository { path, source } => write!(
                f,
                "failed to open Git repository from '{}': {source}",
                path.display()
            ),
            Self::MissingHead { path } => write!(
                f,
                "Git repository at '{}' does not have a commit at HEAD; create an initial commit before analyzing history",
                path.display()
            ),
            Self::BareRepository { path } => write!(
                f,
                "Git repository at '{}' has no worktree; Git analysis requires a local worktree",
                path.display()
            ),
            Self::HeadNotCommit { path, source } => write!(
                f,
                "Git HEAD for '{}' does not resolve to a commit: {source}",
                path.display()
            ),
            Self::Git { context, source } => {
                write!(f, "failed to traverse Git history while {context}: {source}")
            }
            Self::UnsupportedAuthorIdentity { commit_id } => write!(
                f,
                "commit {commit_id} has an author name or email that is not valid UTF-8"
            ),
            Self::UnsupportedPathEncoding { commit_id } => write!(
                f,
                "commit {commit_id} changed a path that is not valid UTF-8"
            ),
        }
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
            | Self::BareRepository { .. }
            | Self::UnsupportedAuthorIdentity { .. }
            | Self::UnsupportedPathEncoding { .. } => None,
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
    let head_commit = head_commit(&repository, worktree_path)?;

    file_changes_from_repository(&repository, head_commit.id())
}

/// Analyze deterministic local Git history reachable from `HEAD`.
pub fn analyze_from_head_at(
    worktree_path: impl AsRef<Path>,
) -> Result<GitAnalysis, GitHistoryError> {
    let worktree_path = worktree_path.as_ref();
    let repository = open_repository(worktree_path)?;
    let head_commit = head_commit(&repository, worktree_path)?;
    let head_commit_id = head_commit.id();
    let head_commit_time = head_commit.time().seconds();
    let worktree_root = repository
        .workdir()
        .ok_or_else(|| GitHistoryError::BareRepository {
            path: worktree_path.to_path_buf(),
        })?
        .to_path_buf();
    let changes = file_changes_from_repository(&repository, head_commit_id)?;
    let file_metrics = file_metrics_from_changes(&changes, head_commit_time);
    let co_changes = co_changes_from_changes(&changes);

    Ok(GitAnalysis {
        worktree_root,
        head_commit_id: head_commit_id.to_string(),
        head_commit_time,
        recent_window_days: RECENT_CHURN_WINDOW_DAYS,
        changes,
        file_metrics,
        co_changes,
    })
}

fn file_changes_from_repository(
    repository: &Repository,
    head_commit_id: Oid,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    let commits = reachable_commits(repository, head_commit_id)?;
    let mut changes = Vec::new();

    for commit_id in commits {
        let commit = repository
            .find_commit(commit_id)
            .map_err(|source| GitHistoryError::Git {
                context: "loading a reachable commit",
                source,
            })?;
        let commit_id = commit.id().to_string();
        let parent_count = commit.parent_count();
        let author = author_identity(&commit_id, commit.author())?;
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
        let mut diff = repository
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_options))
            .map_err(|source| GitHistoryError::Git {
                context: "diffing commit trees",
                source,
            })?;
        let mut find_options = DiffFindOptions::new();
        find_options.renames(true).copies(false);
        diff.find_similar(Some(&mut find_options))
            .map_err(|source| GitHistoryError::Git {
                context: "detecting renamed files",
                source,
            })?;

        changes.extend(diff_file_changes(
            &diff,
            commit_id,
            parent_count,
            author,
            commit_time,
        )?);
    }

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

    Ok(changes)
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
    let recent_threshold = head_commit_time.saturating_sub(RECENT_CHURN_WINDOW_SECONDS);
    let mut by_path = BTreeMap::<String, FileMetricAccumulator>::new();

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

    by_path
        .into_values()
        .map(|accumulator| accumulator.into_metrics(head_commit_time))
        .collect()
}

/// Aggregate raw file changes into deterministic co-change counts.
///
/// Each commit contributes at most one count for any unordered pair of touched
/// paths. Returned pairs are ranked by count descending, then left path
/// ascending, then right path ascending.
pub fn co_changes_from_changes(changes: &[GitFileChange]) -> Vec<GitCoChange> {
    let mut paths_by_commit = BTreeMap::<&str, BTreeSet<&str>>::new();

    for change in changes {
        paths_by_commit
            .entry(change.commit_id.as_str())
            .or_default()
            .insert(change.path.as_str());
    }

    let mut pair_counts = BTreeMap::<(String, String), u64>::new();

    for paths in paths_by_commit.into_values() {
        let paths = paths.into_iter().collect::<Vec<_>>();

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

    let mut commits = revwalk
        .map(|commit| {
            commit.map_err(|source| GitHistoryError::Git {
                context: "walking commits reachable from HEAD",
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    commits.sort();

    Ok(commits)
}

fn diff_file_changes(
    diff: &Diff<'_>,
    commit_id: String,
    parent_count: usize,
    author: String,
    commit_time: i64,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    let mut changes = Vec::new();

    for (index, delta) in diff.deltas().enumerate() {
        let Some(change_kind) = change_kind(delta.status()) else {
            continue;
        };
        let path = delta_path(&commit_id, delta)?;
        if is_internal_analysis_path(&path) {
            continue;
        }
        let (_context, added_lines, deleted_lines) = Patch::from_diff(diff, index)
            .map_err(|source| GitHistoryError::Git {
                context: "loading a file diff patch",
                source,
            })?
            .map_or(Ok((0, 0, 0)), |patch| {
                patch.line_stats().map_err(|source| GitHistoryError::Git {
                    context: "counting changed lines",
                    source,
                })
            })?;

        changes.push(GitFileChange {
            commit_id: commit_id.clone(),
            parent_count,
            is_merge: parent_count > 1,
            author: author.clone(),
            commit_time,
            path,
            change_kind,
            added_lines: added_lines as u64,
            deleted_lines: deleted_lines as u64,
        });
    }

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
            "\n  commits per file: {}\n  total churn: {} added, {} deleted, {} combined\n  recent churn (90 days): {} added, {} deleted, {} combined\n  author count: {}",
            metric.commits_per_file,
            metric.total_churn_added,
            metric.total_churn_deleted,
            total_churn,
            metric.recent_churn_added,
            metric.recent_churn_deleted,
            recent_churn,
            metric.author_count
        ));

        if let (Some(owner), Some(share)) = (&metric.dominant_owner, metric.dominant_owner_share) {
            output.push_str(&format!(
                "\n  dominant owner: {owner} ({:.2}% of file-touching commits)",
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
            "\n  commits per file: 0\n  total churn: 0 added, 0 deleted, 0 combined\n  recent churn (90 days): 0 added, 0 deleted, 0 combined\n  author count: 0\n  dominant owner: unavailable\n  first observed commit: unavailable\n  last observed commit: unavailable\n  file age: unavailable",
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
        "\n\ncalculation notes\n  - Uses local Git history reachable from HEAD only.\n  - Root commits are diffed against the empty tree; merge commits are diffed against their first parent.\n  - Recent churn uses the 90-day window before the HEAD committer timestamp.\n  - A commit counts once per file for commit counts, authorship, ownership, and co-change pairs.\n  - Co-change rows count commits that touched the requested path and the listed path.",
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
    let metric_matches = candidates
        .iter()
        .filter(|candidate| metric_paths.contains(candidate.as_str()))
        .collect::<Vec<_>>();

    match metric_matches.as_slice() {
        [candidate] => return Ok((*candidate).clone()),
        [first, second, ..] => {
            return Err(GitExplainError::AmbiguousPath {
                first: (*first).clone(),
                second: (*second).clone(),
            });
        }
        [] => {}
    }

    let existing_matches = candidates
        .iter()
        .filter(|candidate| workdir.join(candidate).exists())
        .collect::<Vec<_>>();

    match existing_matches.as_slice() {
        [candidate] => Ok((*candidate).clone()),
        [first, second, ..] => Err(GitExplainError::AmbiguousPath {
            first: (*first).clone(),
            second: (*second).clone(),
        }),
        [] => candidates
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
    commits: BTreeSet<String>,
    author_touch_counts: BTreeMap<String, u64>,
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
            commits: BTreeSet::new(),
            author_touch_counts: BTreeMap::new(),
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

    fn into_metrics(self, head_commit_time: i64) -> GitFileMetrics {
        let commits_per_file = self.commits.len() as u64;
        let author_count = self.author_touch_counts.len() as u64;
        let dominant = self.author_touch_counts.iter().max_by(
            |(left_author, left_count), (right_author, right_count)| {
                left_count
                    .cmp(right_count)
                    .then_with(|| right_author.cmp(left_author))
            },
        );
        let dominant_owner = dominant.map(|(author, _count)| author.clone());
        let dominant_owner_share = dominant.map(|(_author, count)| {
            let count = *count as f64;

            count / commits_per_file as f64
        });
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
            dominant_owner,
            dominant_owner_share,
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
