// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::path::{Path, PathBuf};

use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ErrorClass, ErrorCode, Oid, Patch, Repository, Sort,
};

const RECENT_CHURN_WINDOW_DAYS: i64 = 90;
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
            | Self::UnsupportedAuthorIdentity { .. }
            | Self::UnsupportedPathEncoding { .. } => None,
        }
    }
}

/// Return deterministic raw file change events for commits reachable from `HEAD`.
pub fn file_changes_from_head(
    worktree_path: impl AsRef<Path>,
) -> Result<Vec<GitFileChange>, GitHistoryError> {
    let worktree_path = worktree_path.as_ref();
    let repository = Repository::discover(worktree_path).map_err(|source| {
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
    })?;
    let head_commit = head_commit(&repository, worktree_path)?;
    let commits = reachable_commits(&repository, head_commit.id())?;
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
