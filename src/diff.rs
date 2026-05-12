// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str;

use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ErrorClass, ErrorCode, Oid, Patch, Repository,
};
use serde::Serialize;

pub const DIFF_SCHEMA_VERSION: &str = "hotpath.diff.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffReport {
    pub schema_version: &'static str,
    pub range: DiffRangeMetadata,
    pub summary: DiffSummary,
    pub changed_files: Vec<DiffChangedFile>,
    pub architecture: DiffArchitectureStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffRangeMetadata {
    pub requested: String,
    pub base_ref: String,
    pub head_ref: String,
    pub base_commit_id: String,
    pub head_commit_id: String,
    pub merge_base_commit_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffSummary {
    pub changed_files: u64,
    pub added_lines: u64,
    pub deleted_lines: u64,
    pub context_token_delta: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffChangedFile {
    pub path: String,
    pub change_kind: DiffChangeKind,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub added_lines: u64,
    pub deleted_lines: u64,
    pub context_token_delta: i64,
    pub skipped_context: Vec<DiffContextSkip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffContextSkip {
    pub side: DiffContextSide,
    pub reason: DiffContextSkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffContextSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffContextSkipReason {
    Binary,
    InvalidUtf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffArchitectureStatus {
    NotEvaluated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRangeSpec {
    pub requested: String,
    pub base_ref: String,
    pub head_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRangeParseError {
    MissingTripleDot,
    MultipleTripleDot,
    TwoDotRange,
    EmptyBase,
    EmptyHead,
}

#[derive(Debug)]
pub enum DiffError {
    InvalidRange(DiffRangeParseError),
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
    ResolvedHeadNotCurrentHead {
        requested_head: String,
        resolved_head_commit_id: String,
        current_head_commit_id: String,
    },
    UnsupportedPathEncoding {
        commit_id: String,
    },
    Git {
        context: &'static str,
        source: git2::Error,
    },
}

impl fmt::Display for DiffRangeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTripleDot => {
                write!(f, "diff range must use exactly one triple-dot separator")
            }
            Self::MultipleTripleDot => {
                write!(
                    f,
                    "diff range must not contain more than one triple-dot separator"
                )
            }
            Self::TwoDotRange => write!(
                f,
                "diff range must use base...head syntax; two-dot ranges are not supported"
            ),
            Self::EmptyBase => write!(f, "diff range base ref must not be empty"),
            Self::EmptyHead => write!(f, "diff range head ref must not be empty"),
        }
    }
}

impl StdError for DiffRangeParseError {}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange(source) => write!(f, "{source}"),
            Self::NotRepository { .. } => write!(
                f,
                "path is not a readable Git worktree; run diff analysis from inside a repository with local history"
            ),
            Self::OpenRepository { .. } => write!(
                f,
                "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
            ),
            Self::MissingHead { .. } => write!(
                f,
                "Git repository does not have a commit at HEAD; create an initial commit before analyzing a diff"
            ),
            Self::ShallowRepository { .. } => write!(
                f,
                "Git repository has shallow history; fetch complete local history before running diff analysis so metrics are not based on incomplete commits"
            ),
            Self::BareRepository { .. } => write!(
                f,
                "Git repository has no worktree; diff analysis requires a local worktree"
            ),
            Self::HeadNotCommit { source, .. } => {
                write!(f, "Git HEAD does not resolve to a commit: {source}")
            }
            Self::ResolvedHeadNotCurrentHead {
                requested_head,
                resolved_head_commit_id,
                current_head_commit_id,
            } => write!(
                f,
                "diff range head '{requested_head}' resolves to commit {resolved_head_commit_id}, but current HEAD is {current_head_commit_id}; check out the requested head before analyzing the diff"
            ),
            Self::UnsupportedPathEncoding { commit_id } => write!(
                f,
                "commit {commit_id} changed a path that is not valid UTF-8"
            ),
            Self::Git { context, source } => {
                write!(f, "failed to analyze Git diff while {context}: {source}")
            }
        }
    }
}

impl StdError for DiffError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::InvalidRange(source) => Some(source),
            Self::NotRepository { source, .. }
            | Self::OpenRepository { source, .. }
            | Self::HeadNotCommit { source, .. }
            | Self::Git { source, .. } => Some(source),
            Self::MissingHead { .. }
            | Self::ShallowRepository { .. }
            | Self::BareRepository { .. }
            | Self::ResolvedHeadNotCurrentHead { .. }
            | Self::UnsupportedPathEncoding { .. } => None,
        }
    }
}

impl From<DiffRangeParseError> for DiffError {
    fn from(source: DiffRangeParseError) -> Self {
        Self::InvalidRange(source)
    }
}

pub fn parse_diff_range(range: &str) -> Result<DiffRangeSpec, DiffRangeParseError> {
    let triple_dot_count = range.match_indices("...").count();
    if triple_dot_count == 0 {
        return if range.contains("..") {
            Err(DiffRangeParseError::TwoDotRange)
        } else {
            Err(DiffRangeParseError::MissingTripleDot)
        };
    }
    if triple_dot_count > 1 {
        return Err(DiffRangeParseError::MultipleTripleDot);
    }

    let (base_ref, head_ref) = range
        .split_once("...")
        .expect("triple-dot count was checked before splitting");
    let base_ref = base_ref.trim();
    let head_ref = head_ref.trim();
    if base_ref.is_empty() {
        return Err(DiffRangeParseError::EmptyBase);
    }
    if head_ref.is_empty() {
        return Err(DiffRangeParseError::EmptyHead);
    }
    if base_ref.contains("..")
        || head_ref.contains("..")
        || base_ref.ends_with('.')
        || head_ref.starts_with('.')
    {
        return Err(DiffRangeParseError::TwoDotRange);
    }

    Ok(DiffRangeSpec {
        requested: range.to_owned(),
        base_ref: base_ref.to_owned(),
        head_ref: head_ref.to_owned(),
    })
}

pub fn analyze_committed_tree_diff(
    worktree_path: impl AsRef<Path>,
    range: &str,
) -> Result<DiffReport, DiffError> {
    let range = parse_diff_range(range)?;
    let worktree_path = worktree_path.as_ref();
    let repository = open_repository(worktree_path)?;
    reject_shallow_repository(&repository, worktree_path)?;
    reject_bare_repository(&repository, worktree_path)?;

    let current_head = head_commit(&repository, worktree_path)?;
    let base_commit = resolve_commit(&repository, &range.base_ref, "resolving diff base ref")?;
    let head_commit = resolve_commit(&repository, &range.head_ref, "resolving diff head ref")?;
    if head_commit.id() != current_head.id() {
        return Err(DiffError::ResolvedHeadNotCurrentHead {
            requested_head: range.head_ref,
            resolved_head_commit_id: head_commit.id().to_string(),
            current_head_commit_id: current_head.id().to_string(),
        });
    }

    let merge_base_commit_id = repository
        .merge_base(base_commit.id(), head_commit.id())
        .map_err(|source| DiffError::Git {
            context: "computing the merge base",
            source,
        })?;
    let merge_base_commit = repository
        .find_commit(merge_base_commit_id)
        .map_err(|source| DiffError::Git {
            context: "loading the merge-base commit",
            source,
        })?;
    let merge_base_tree = merge_base_commit.tree().map_err(|source| DiffError::Git {
        context: "loading the merge-base tree",
        source,
    })?;
    let head_tree = head_commit.tree().map_err(|source| DiffError::Git {
        context: "loading the head tree",
        source,
    })?;

    let mut diff_options = DiffOptions::new();
    let mut diff = repository
        .diff_tree_to_tree(
            Some(&merge_base_tree),
            Some(&head_tree),
            Some(&mut diff_options),
        )
        .map_err(|source| DiffError::Git {
            context: "diffing merge-base and head trees",
            source,
        })?;
    let mut find_options = DiffFindOptions::new();
    find_options.renames(true).copies(false);
    diff.find_similar(Some(&mut find_options))
        .map_err(|source| DiffError::Git {
            context: "detecting renamed files",
            source,
        })?;

    let changed_files = changed_files_from_diff(&repository, &diff, head_commit.id())?;
    let summary = summarize(&changed_files);

    Ok(DiffReport {
        schema_version: DIFF_SCHEMA_VERSION,
        range: DiffRangeMetadata {
            requested: range.requested,
            base_ref: range.base_ref,
            head_ref: range.head_ref,
            base_commit_id: base_commit.id().to_string(),
            head_commit_id: head_commit.id().to_string(),
            merge_base_commit_id: merge_base_commit_id.to_string(),
        },
        summary,
        changed_files,
        architecture: DiffArchitectureStatus::NotEvaluated,
    })
}

fn open_repository(worktree_path: &Path) -> Result<Repository, DiffError> {
    Repository::discover(worktree_path).map_err(|source| {
        if source.code() == ErrorCode::NotFound || source.class() == ErrorClass::Repository {
            DiffError::NotRepository {
                path: worktree_path.to_path_buf(),
                source,
            }
        } else {
            DiffError::OpenRepository {
                path: worktree_path.to_path_buf(),
                source,
            }
        }
    })
}

fn reject_shallow_repository(
    repository: &Repository,
    worktree_path: &Path,
) -> Result<(), DiffError> {
    if repository.is_shallow() {
        Err(DiffError::ShallowRepository {
            path: worktree_path.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

fn reject_bare_repository(repository: &Repository, worktree_path: &Path) -> Result<(), DiffError> {
    if repository.is_bare() {
        Err(DiffError::BareRepository {
            path: worktree_path.to_path_buf(),
        })
    } else {
        Ok(())
    }
}

fn head_commit<'repo>(
    repository: &'repo Repository,
    worktree_path: &Path,
) -> Result<git2::Commit<'repo>, DiffError> {
    let head = repository.head().map_err(|source| {
        if matches!(source.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) {
            DiffError::MissingHead {
                path: worktree_path.to_path_buf(),
            }
        } else {
            DiffError::Git {
                context: "reading HEAD",
                source,
            }
        }
    })?;

    head.peel_to_commit()
        .map_err(|source| DiffError::HeadNotCommit {
            path: worktree_path.to_path_buf(),
            source,
        })
}

fn resolve_commit<'repo>(
    repository: &'repo Repository,
    revision: &str,
    context: &'static str,
) -> Result<git2::Commit<'repo>, DiffError> {
    repository
        .revparse_single(revision)
        .and_then(|object| object.peel_to_commit())
        .map_err(|source| DiffError::Git { context, source })
}

fn changed_files_from_diff(
    repository: &Repository,
    diff: &Diff<'_>,
    commit_id: Oid,
) -> Result<Vec<DiffChangedFile>, DiffError> {
    let mut changed_files = Vec::new();
    let commit_id = commit_id.to_string();

    for (index, delta) in diff.deltas().enumerate() {
        let Some(change_kind) = change_kind(delta.status()) else {
            continue;
        };
        let old_path = path_for_delta_file(&commit_id, delta.old_file())?;
        let new_path = path_for_delta_file(&commit_id, delta.new_file())?;
        let path = if change_kind == DiffChangeKind::Deleted {
            old_path.clone()
        } else {
            new_path.clone()
        }
        .ok_or_else(|| DiffError::UnsupportedPathEncoding {
            commit_id: commit_id.clone(),
        })?;
        let (_context_lines, added_lines, deleted_lines) = Patch::from_diff(diff, index)
            .map_err(|source| DiffError::Git {
                context: "loading a file diff patch",
                source,
            })?
            .map_or(Ok((0, 0, 0)), |patch| {
                patch.line_stats().map_err(|source| DiffError::Git {
                    context: "counting changed lines",
                    source,
                })
            })?;
        let context = context_token_delta(repository, delta)?;

        changed_files.push(DiffChangedFile {
            path,
            change_kind,
            old_path,
            new_path,
            added_lines: added_lines as u64,
            deleted_lines: deleted_lines as u64,
            context_token_delta: context.token_delta,
            skipped_context: context.skipped,
        });
    }

    changed_files.sort_by(|left, right| {
        (
            &left.path,
            left.change_kind,
            left.added_lines,
            left.deleted_lines,
            left.context_token_delta,
        )
            .cmp(&(
                &right.path,
                right.change_kind,
                right.added_lines,
                right.deleted_lines,
                right.context_token_delta,
            ))
    });

    Ok(changed_files)
}

fn summarize(changed_files: &[DiffChangedFile]) -> DiffSummary {
    DiffSummary {
        changed_files: changed_files.len() as u64,
        added_lines: changed_files.iter().map(|file| file.added_lines).sum(),
        deleted_lines: changed_files.iter().map(|file| file.deleted_lines).sum(),
        context_token_delta: changed_files
            .iter()
            .map(|file| file.context_token_delta)
            .sum(),
    }
}

struct ContextDelta {
    token_delta: i64,
    skipped: Vec<DiffContextSkip>,
}

fn context_token_delta(
    repository: &Repository,
    delta: git2::DiffDelta<'_>,
) -> Result<ContextDelta, DiffError> {
    let mut skipped = Vec::new();
    let old_tokens = tokens_for_side(
        repository,
        delta.old_file(),
        DiffContextSide::Old,
        &mut skipped,
    )?;
    let new_tokens = tokens_for_side(
        repository,
        delta.new_file(),
        DiffContextSide::New,
        &mut skipped,
    )?;

    Ok(ContextDelta {
        token_delta: new_tokens.unwrap_or(0) - old_tokens.unwrap_or(0),
        skipped,
    })
}

fn tokens_for_side(
    repository: &Repository,
    file: git2::DiffFile<'_>,
    side: DiffContextSide,
    skipped: &mut Vec<DiffContextSkip>,
) -> Result<Option<i64>, DiffError> {
    let oid = file.id();
    if oid == Oid::zero() {
        return Ok(None);
    }

    let blob = repository.find_blob(oid).map_err(|source| DiffError::Git {
        context: "loading a blob for context estimation",
        source,
    })?;
    if blob.is_binary() {
        skipped.push(DiffContextSkip {
            side,
            reason: DiffContextSkipReason::Binary,
        });
        return Ok(None);
    }
    let bytes = blob.content();
    if str::from_utf8(bytes).is_err() {
        skipped.push(DiffContextSkip {
            side,
            reason: DiffContextSkipReason::InvalidUtf8,
        });
        return Ok(None);
    }

    Ok(Some(estimate_tokens(bytes.len())))
}

fn estimate_tokens(byte_len: usize) -> i64 {
    byte_len.div_ceil(4) as i64
}

fn path_for_delta_file(
    commit_id: &str,
    file: git2::DiffFile<'_>,
) -> Result<Option<String>, DiffError> {
    if file.id() == Oid::zero() {
        return Ok(None);
    }

    file.path()
        .map(|path| {
            path.to_str()
                .map(|path| path.replace('\\', "/"))
                .ok_or_else(|| DiffError::UnsupportedPathEncoding {
                    commit_id: commit_id.to_owned(),
                })
        })
        .transpose()
}

fn change_kind(delta: Delta) -> Option<DiffChangeKind> {
    match delta {
        Delta::Added => Some(DiffChangeKind::Added),
        Delta::Modified => Some(DiffChangeKind::Modified),
        Delta::Deleted => Some(DiffChangeKind::Deleted),
        Delta::Renamed => Some(DiffChangeKind::Renamed),
        Delta::Copied => Some(DiffChangeKind::Copied),
        Delta::Typechange => Some(DiffChangeKind::TypeChanged),
        Delta::Unmodified
        | Delta::Ignored
        | Delta::Untracked
        | Delta::Unreadable
        | Delta::Conflicted => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use git2::{build::CheckoutBuilder, IndexAddOption, ObjectType, Signature};

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_triple_dot_range() {
        let range = parse_diff_range(" main ... feature ").expect("range should parse");

        assert_eq!(
            range,
            DiffRangeSpec {
                requested: " main ... feature ".to_owned(),
                base_ref: "main".to_owned(),
                head_ref: "feature".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_invalid_ranges() {
        assert_eq!(
            parse_diff_range("main").expect_err("missing separator should fail"),
            DiffRangeParseError::MissingTripleDot
        );
        assert_eq!(
            parse_diff_range("main..feature").expect_err("two-dot range should fail"),
            DiffRangeParseError::TwoDotRange
        );
        assert_eq!(
            parse_diff_range("main....feature").expect_err("four-dot range should fail"),
            DiffRangeParseError::TwoDotRange
        );
        assert_eq!(
            parse_diff_range("main...feature...other")
                .expect_err("multiple triple-dot separators should fail"),
            DiffRangeParseError::MultipleTripleDot
        );
        assert_eq!(
            parse_diff_range("...feature").expect_err("empty base should fail"),
            DiffRangeParseError::EmptyBase
        );
        assert_eq!(
            parse_diff_range("main...").expect_err("empty head should fail"),
            DiffRangeParseError::EmptyHead
        );
    }

    #[test]
    fn changed_files_are_sorted_by_stable_repository_relative_path() {
        let fixture = GitFixture::new("diff-ordering");
        fixture.write("zeta.txt", b"base\n");
        fixture.commit_all("base");
        let base = fixture.head_id();

        fixture.write("zeta.txt", b"base\nchanged\n");
        fixture.write("alpha.txt", b"alpha\n");
        fixture.write("dir/beta.txt", b"beta\n");
        fixture.commit_all("head");
        let head = fixture.head_id();

        let report = analyze_committed_tree_diff(fixture.path(), &format!("{base}...{head}"))
            .expect("diff should analyze");
        let paths = report
            .changed_files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["alpha.txt", "dir/beta.txt", "zeta.txt"]);
        assert_eq!(report.schema_version, DIFF_SCHEMA_VERSION);
        assert_eq!(report.architecture, DiffArchitectureStatus::NotEvaluated);
    }

    #[test]
    fn diff_uses_merge_base_instead_of_base_tip() {
        let fixture = GitFixture::new("diff-merge-base");
        fixture.write("shared.txt", b"shared\n");
        fixture.commit_all("root");
        let root_commit = fixture.head_commit();

        fixture.create_branch("feature", &root_commit);
        fixture.checkout_branch("feature");
        fixture.write("feature.txt", b"feature\n");
        fixture.commit_all("feature");
        let feature_head = fixture.head_id();

        fixture.checkout_branch("master");
        fixture.write("base-only.txt", b"base only\n");
        fixture.commit_all("base only");

        fixture.checkout_branch("feature");
        let report = analyze_committed_tree_diff(fixture.path(), "master...HEAD")
            .expect("diff should analyze from merge-base to current head");
        let paths = report
            .changed_files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(report.range.head_commit_id, feature_head.to_string());
        assert_eq!(
            report.range.merge_base_commit_id,
            root_commit.id().to_string()
        );
        assert_eq!(paths, vec!["feature.txt"]);
    }

    #[test]
    fn reports_context_delta_for_add_modify_delete_and_rename() {
        let fixture = GitFixture::new("diff-context");
        fixture.write("delete.txt", sized_text(8).as_bytes());
        fixture.write("modify.txt", sized_text(8).as_bytes());
        fixture.write("rename-old.txt", sized_text(100).as_bytes());
        fixture.commit_all("base");
        let base = fixture.head_id();

        fixture.delete("delete.txt");
        fixture.write("add.txt", sized_text(5).as_bytes());
        fixture.write("modify.txt", sized_text(9).as_bytes());
        fixture.rename("rename-old.txt", "rename-new.txt");
        fixture.write("rename-new.txt", sized_text(104).as_bytes());
        fixture.commit_all("head");
        let head = fixture.head_id();

        let report = analyze_committed_tree_diff(fixture.path(), &format!("{base}...{head}"))
            .expect("diff should analyze");

        let add = changed_file(&report, "add.txt");
        assert_eq!(add.change_kind, DiffChangeKind::Added);
        assert_eq!(add.old_path, None);
        assert_eq!(add.new_path.as_deref(), Some("add.txt"));
        assert_eq!(add.context_token_delta, 2);

        let delete = changed_file(&report, "delete.txt");
        assert_eq!(delete.change_kind, DiffChangeKind::Deleted);
        assert_eq!(delete.old_path.as_deref(), Some("delete.txt"));
        assert_eq!(delete.new_path, None);
        assert_eq!(delete.context_token_delta, -2);

        let modify = changed_file(&report, "modify.txt");
        assert_eq!(modify.change_kind, DiffChangeKind::Modified);
        assert_eq!(modify.context_token_delta, 1);

        let rename = changed_file(&report, "rename-new.txt");
        assert_eq!(rename.change_kind, DiffChangeKind::Renamed);
        assert_eq!(rename.old_path.as_deref(), Some("rename-old.txt"));
        assert_eq!(rename.new_path.as_deref(), Some("rename-new.txt"));
        assert_eq!(rename.context_token_delta, 1);

        assert_eq!(report.summary.changed_files, 4);
        assert_eq!(report.summary.context_token_delta, 2);
        assert_eq!(report.range.base_ref, base.to_string());
        assert_eq!(report.range.head_ref, head.to_string());
        assert_eq!(report.range.base_commit_id, base.to_string());
        assert_eq!(report.range.head_commit_id, head.to_string());
    }

    #[test]
    fn rejects_diff_when_range_head_is_not_current_head() {
        let fixture = GitFixture::new("diff-non-current-head");
        fixture.write("base.txt", b"base\n");
        fixture.commit_all("base");
        let base_branch = fixture.current_branch_name();
        let root_commit = fixture.head_commit();

        fixture.create_branch("feature", &root_commit);
        fixture.checkout_branch("feature");
        fixture.write("feature.txt", b"feature\n");
        fixture.commit_all("feature");
        fixture.checkout_branch(&base_branch);

        let error =
            analyze_committed_tree_diff(fixture.path(), &format!("{base_branch}...feature"))
                .expect_err("non-current range head should be rejected");

        assert!(matches!(
            error,
            DiffError::ResolvedHeadNotCurrentHead {
                requested_head,
                ..
            } if requested_head == "feature"
        ));
    }

    #[test]
    fn records_binary_and_non_utf8_context_skip_reasons() {
        let fixture = GitFixture::new("diff-context-skips");
        fixture.write("base.txt", b"base\n");
        fixture.commit_all("base");
        let base = fixture.head_id();

        fixture.write("binary.bin", &[0, 1, 2, 0, 3]);
        fixture.write("invalid.txt", &[0xC3, 0x28]);
        fixture.commit_all("head");
        let head = fixture.head_id();

        let report = analyze_committed_tree_diff(fixture.path(), &format!("{base}...{head}"))
            .expect("diff should analyze");

        let binary = changed_file(&report, "binary.bin");
        assert_eq!(binary.context_token_delta, 0);
        assert_eq!(
            binary.skipped_context,
            vec![DiffContextSkip {
                side: DiffContextSide::New,
                reason: DiffContextSkipReason::Binary,
            }]
        );

        let invalid = changed_file(&report, "invalid.txt");
        assert_eq!(invalid.context_token_delta, 0);
        assert_eq!(
            invalid.skipped_context,
            vec![DiffContextSkip {
                side: DiffContextSide::New,
                reason: DiffContextSkipReason::InvalidUtf8,
            }]
        );
    }

    fn changed_file<'a>(report: &'a DiffReport, path: &str) -> &'a DiffChangedFile {
        report
            .changed_files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("expected changed file {path}"))
    }

    fn sized_text(byte_len: usize) -> String {
        "a".repeat(byte_len)
    }

    struct GitFixture {
        root: PathBuf,
        repository: Repository,
    }

    impl GitFixture {
        fn new(name: &str) -> Self {
            let root = unique_temp_path(name);
            fs::create_dir_all(&root).expect("fixture directory should be created");
            let repository = Repository::init(&root).expect("repository should initialize");
            Self { root, repository }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative_path: &str, bytes: &[u8]) {
            let path = self.root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent directory should be created");
            }
            fs::write(path, bytes).expect("fixture file should be written");
        }

        fn delete(&self, relative_path: &str) {
            fs::remove_file(self.root.join(relative_path)).expect("fixture file should be deleted");
        }

        fn rename(&self, old_relative_path: &str, new_relative_path: &str) {
            let new_path = self.root.join(new_relative_path);
            if let Some(parent) = new_path.parent() {
                fs::create_dir_all(parent).expect("parent directory should be created");
            }
            fs::rename(self.root.join(old_relative_path), new_path)
                .expect("fixture file should be renamed");
        }

        fn commit_all(&self, message: &str) -> Oid {
            let mut index = self.repository.index().expect("index should load");
            index
                .add_all(["*"], IndexAddOption::DEFAULT, None)
                .expect("files should be added to index");
            index.write().expect("index should be written");
            let tree_id = index.write_tree().expect("index tree should be written");
            let tree = self
                .repository
                .find_tree(tree_id)
                .expect("tree should be found");
            let signature = Signature::new(
                "Hotpath Test",
                "hotpath@example.invalid",
                &git2::Time::new(0, 0),
            )
            .expect("signature should be valid");
            let parent = self.repository.head().ok().and_then(|head| {
                head.target()
                    .and_then(|id| self.repository.find_commit(id).ok())
            });

            match parent.as_ref() {
                Some(parent) => self
                    .repository
                    .commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        message,
                        &tree,
                        &[parent],
                    )
                    .expect("commit should be created"),
                None => self
                    .repository
                    .commit(Some("HEAD"), &signature, &signature, message, &tree, &[])
                    .expect("initial commit should be created"),
            }
        }

        fn head_id(&self) -> Oid {
            self.repository
                .head()
                .expect("HEAD should exist")
                .target()
                .expect("HEAD should be direct")
        }

        fn head_commit(&self) -> git2::Commit<'_> {
            self.repository
                .find_commit(self.head_id())
                .expect("HEAD commit should load")
        }

        fn current_branch_name(&self) -> String {
            self.repository
                .head()
                .expect("HEAD should exist")
                .shorthand()
                .expect("HEAD branch should have shorthand")
                .to_owned()
        }

        fn create_branch(&self, name: &str, commit: &git2::Commit<'_>) {
            self.repository
                .branch(name, commit, false)
                .expect("branch should be created");
        }

        fn checkout_branch(&self, name: &str) {
            let (object, reference) = self
                .repository
                .revparse_ext(name)
                .expect("branch should resolve");
            self.repository
                .checkout_tree(
                    &object,
                    Some(CheckoutBuilder::new().force().remove_untracked(true)),
                )
                .expect("branch tree should be checked out");
            if let Some(reference) = reference {
                self.repository
                    .set_head(reference.name().expect("reference should have name"))
                    .expect("HEAD should be updated");
            } else if object.kind() == Some(ObjectType::Commit) {
                self.repository
                    .set_head_detached(object.id())
                    .expect("HEAD should detach");
            }
        }
    }

    impl Drop for GitFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let counter = FIXTURE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hotpath-{name}-{}-{now}-{counter}",
            std::process::id()
        ))
    }
}
