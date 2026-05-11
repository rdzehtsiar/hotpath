// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::path::{Path, PathBuf};

use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ErrorClass, ErrorCode, Oid, Patch, Repository, Sort,
};

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
