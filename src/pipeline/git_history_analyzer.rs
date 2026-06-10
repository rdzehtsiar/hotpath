// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_GIT_MAX_COMMITS: usize = 50_000;
pub const DEFAULT_GIT_MAX_AGE_DAYS: i64 = 730;
pub const DEFAULT_GIT_COCHANGE_MAX_FILES_PER_COMMIT: usize = 100;
pub const DEFAULT_GIT_DELTA_BATCH_SIZE: usize = 10_000;
pub const RECENT_CHURN_WINDOW_DAYS: i64 = 90;
pub const OWNERSHIP_HALF_LIFE_DAYS: f64 = 730.0;

const RECORD_SEPARATOR: char = '\x1e';
const FIELD_SEPARATOR: char = '\x1f';
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHistoryAnalyzerOptions {
    pub max_commits: Option<usize>,
    pub max_age_days: Option<i64>,
    pub detect_renames: bool,
    pub cochange_max_files_per_commit: usize,
    pub delta_batch_size: usize,
}

impl Default for GitHistoryAnalyzerOptions {
    fn default() -> Self {
        Self {
            max_commits: Some(DEFAULT_GIT_MAX_COMMITS),
            max_age_days: Some(DEFAULT_GIT_MAX_AGE_DAYS),
            detect_renames: true,
            cochange_max_files_per_commit: DEFAULT_GIT_COCHANGE_MAX_FILES_PER_COMMIT,
            delta_batch_size: DEFAULT_GIT_DELTA_BATCH_SIZE,
        }
    }
}

/// Reduces local Git history into file-level and repository-level Git facts.
#[derive(Debug, Clone)]
pub struct GitHistoryAnalyzer {
    options: GitHistoryAnalyzerOptions,
}

impl GitHistoryAnalyzer {
    pub fn new() -> Self {
        Self::with_options(GitHistoryAnalyzerOptions::default())
    }

    pub fn with_options(options: GitHistoryAnalyzerOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> &GitHistoryAnalyzerOptions {
        &self.options
    }

    pub fn analyze<S>(
        &self,
        input: GitHistoryScan,
        sink: &S,
    ) -> Result<GitChunkSummary, GitHistoryError>
    where
        S: GitHistorySink,
    {
        self.analyze_with_progress(input, sink, &mut |_| {})
    }

    pub fn analyze_with_progress<S>(
        &self,
        input: GitHistoryScan,
        sink: &S,
        progress: &mut dyn FnMut(GitHistoryProgress),
    ) -> Result<GitChunkSummary, GitHistoryError>
    where
        S: GitHistorySink,
    {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&input.root)
            .arg("log")
            .arg("--reverse")
            .arg("--format=format:%x1e%H%x1f%P%x1f%an <%ae>%x1f%ct")
            .arg("--numstat")
            .arg("--no-ext-diff")
            .arg("--first-parent")
            .arg("--root");
        if self.options.detect_renames {
            command.arg("--find-renames");
        } else {
            command.arg("--no-renames");
        }
        if let Some(revision) = &input.revision {
            command.arg(revision);
        }
        if let Some(max_commits) = input.max_commits {
            command.arg(format!("-n{max_commits}"));
        }
        if let Some(max_age_days) = input.max_age_days {
            let cutoff = input.head_timestamp - max_age_days.max(0) * SECONDS_PER_DAY;
            command.arg(format!("--since=@{cutoff}"));
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|source| GitHistoryError::CommandStart {
                root: input.root.clone(),
                source,
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitHistoryError::MissingPipe {
                root: input.root.clone(),
                stream: "stdout",
            })?;
        let batching_sink = GitBatchingSink::new(sink, input.delta_batch_size);
        let summary = parse_git_history_reader_with_progress(
            0,
            input.head_timestamp,
            input.cochange_max_files_per_commit,
            BufReader::new(stdout),
            &batching_sink,
            progress,
        )?;
        batching_sink.flush_all()?;

        let output = child
            .wait_with_output()
            .map_err(|source| GitHistoryError::CommandStart {
                root: input.root.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(GitHistoryError::CommandFailed {
                root: input.root,
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        sink.store_git_chunk_summary(summary.clone())?;
        Ok(summary)
    }

    pub fn analyze_chunk<S>(
        &self,
        input: GitHistoryChunk,
        sink: &S,
    ) -> Result<GitChunkSummary, GitHistoryError>
    where
        S: GitHistorySink,
    {
        if input.commit_ids.is_empty() {
            let summary = GitChunkSummary {
                chunk_index: input.chunk_index,
                commits_processed: 0,
                file_changes: 0,
                cochange_pairs: 0,
                broad_commits_skipped_for_cochange: 0,
                max_touched_files: 0,
                broadest_commit: None,
            };
            sink.store_git_chunk_summary(summary.clone())?;
            return Ok(summary);
        }

        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&input.root)
            .arg("show")
            .arg("--stdin")
            .arg("--format=format:%x1e%H%x1f%P%x1f%an <%ae>%x1f%ct")
            .arg("--numstat")
            .arg("--no-ext-diff")
            .arg("--first-parent")
            .arg("--root")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.options.detect_renames {
            command.arg("--find-renames");
        } else {
            command.arg("--no-renames");
        }
        let mut child = command
            .spawn()
            .map_err(|source| GitHistoryError::CommandStart {
                root: input.root.clone(),
                source,
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            for commit_id in &input.commit_ids {
                writeln!(stdin, "{commit_id}").map_err(|source| GitHistoryError::CommandStart {
                    root: input.root.clone(),
                    source,
                })?;
            }
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitHistoryError::MissingPipe {
                root: input.root.clone(),
                stream: "stdout",
            })?;
        let batching_sink = GitBatchingSink::new(sink, self.options.delta_batch_size);
        let summary = parse_git_history_reader(
            input.chunk_index,
            input.head_timestamp,
            self.options.cochange_max_files_per_commit,
            BufReader::new(stdout),
            &batching_sink,
        )?;
        batching_sink.flush_all()?;

        let output = child
            .wait_with_output()
            .map_err(|source| GitHistoryError::CommandStart {
                root: input.root.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(GitHistoryError::CommandFailed {
                root: input.root,
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        sink.store_git_chunk_summary(summary.clone())?;
        Ok(summary)
    }
}

impl Default for GitHistoryAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHistoryChunk {
    pub root: PathBuf,
    pub chunk_index: usize,
    pub commit_ids: Vec<String>,
    pub head_timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHistoryScan {
    pub root: PathBuf,
    pub revision: Option<String>,
    pub head_timestamp: i64,
    pub max_commits: Option<usize>,
    pub max_age_days: Option<i64>,
    pub detect_renames: bool,
    pub cochange_max_files_per_commit: usize,
    pub delta_batch_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChunkSummary {
    pub chunk_index: usize,
    pub commits_processed: u64,
    pub file_changes: u64,
    pub cochange_pairs: u64,
    pub broad_commits_skipped_for_cochange: u64,
    pub max_touched_files: u64,
    pub broadest_commit: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHistoryProgress {
    pub commits_processed: u64,
    pub file_changes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitHistoryChunkResult {
    pub chunk_index: usize,
    pub commits_processed: u64,
    pub file_changes: u64,
    pub file_metrics: Vec<GitFileMetricDelta>,
    pub file_authors: Vec<GitFileAuthorDelta>,
    pub cochanges: Vec<GitCochangeDelta>,
    pub repository: GitRepositoryDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileMetricDelta {
    pub path: String,
    pub commits: u64,
    pub total_added_lines: u64,
    pub total_deleted_lines: u64,
    pub recent_added_lines: u64,
    pub recent_deleted_lines: u64,
    pub first_touch_timestamp: i64,
    pub last_touch_timestamp: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitFileAuthorDelta {
    pub path: String,
    pub author: String,
    pub touch_count: u64,
    pub meaningful_commit_count: u64,
    pub effective_changed_lines: u64,
    pub ownership_line_recency_score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCochangeDelta {
    pub left_path: String,
    pub right_path: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitRepositoryDelta {
    pub authors: Vec<String>,
    pub first_commit_timestamp: Option<i64>,
    pub last_commit_timestamp: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepositoryPlan {
    pub total_commits: Option<u64>,
    pub head_commit: Option<String>,
    pub head_timestamp: Option<i64>,
    pub is_shallow: bool,
    pub first_parent_commit_count: Option<u64>,
    pub all_reachable_commit_count: Option<u64>,
    pub merge_commit_count: Option<u64>,
}

pub fn git_options_signature(options: &GitHistoryAnalyzerOptions) -> String {
    format!(
        "max_commits={:?};max_age_days={:?};detect_renames={};cochange_max_files_per_commit={};delta_batch_size={}",
        options.max_commits,
        options.max_age_days,
        options.detect_renames,
        options.cochange_max_files_per_commit,
        options.delta_batch_size
    )
}

pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, GitHistoryError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor)
        .arg(descendant)
        .output()
        .map_err(|source| GitHistoryError::CommandStart {
            root: root.to_path_buf(),
            source,
        })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        status => Err(GitHistoryError::CommandFailed {
            root: root.to_path_buf(),
            status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }),
    }
}

pub trait GitHistorySink {
    fn store_git_chunk_summary(&self, summary: GitChunkSummary) -> Result<(), GitHistoryError>;
    fn store_git_file_metrics(
        &self,
        metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), GitHistoryError>;
    fn store_git_file_authors(
        &self,
        authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), GitHistoryError>;
    fn store_git_cochanges(&self, cochanges: Vec<GitCochangeDelta>) -> Result<(), GitHistoryError>;
    fn store_git_repository_delta(&self, delta: GitRepositoryDelta) -> Result<(), GitHistoryError>;
}

#[derive(Debug, Clone, Copy)]
pub struct NoopGitHistorySink;

impl GitHistorySink for NoopGitHistorySink {
    fn store_git_chunk_summary(&self, _summary: GitChunkSummary) -> Result<(), GitHistoryError> {
        Ok(())
    }

    fn store_git_file_metrics(
        &self,
        _metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), GitHistoryError> {
        Ok(())
    }

    fn store_git_file_authors(
        &self,
        _authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), GitHistoryError> {
        Ok(())
    }

    fn store_git_cochanges(
        &self,
        _cochanges: Vec<GitCochangeDelta>,
    ) -> Result<(), GitHistoryError> {
        Ok(())
    }

    fn store_git_repository_delta(
        &self,
        _delta: GitRepositoryDelta,
    ) -> Result<(), GitHistoryError> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum GitHistoryError {
    CommandStart {
        root: PathBuf,
        source: std::io::Error,
    },
    CommandFailed {
        root: PathBuf,
        status: Option<i32>,
        stderr: String,
    },
    MissingPipe {
        root: PathBuf,
        stream: &'static str,
    },
    ReadStream(std::io::Error),
    Sink(String),
}

impl fmt::Display for GitHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandStart { root, source } => {
                write!(
                    f,
                    "failed to start git history command in '{}': {source}",
                    root.display()
                )
            }
            Self::CommandFailed {
                root,
                status,
                stderr,
            } => {
                write!(
                    f,
                    "git history command failed in '{}' with status {:?}: {}",
                    root.display(),
                    status,
                    stderr
                )
            }
            Self::MissingPipe { root, stream } => {
                write!(
                    f,
                    "git history command in '{}' did not provide {stream}",
                    root.display()
                )
            }
            Self::ReadStream(source) => write!(f, "failed to read git history stream: {source}"),
            Self::Sink(source) => write!(f, "failed to send git history result: {source}"),
        }
    }
}

impl StdError for GitHistoryError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CommandStart { source, .. } => Some(source),
            Self::ReadStream(source) => Some(source),
            Self::CommandFailed { .. } | Self::MissingPipe { .. } | Self::Sink(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedGitHistoryStream {
    pub commits: u64,
    pub file_changes: u64,
}

pub fn collect_commit_ids(root: &Path) -> Result<Option<Vec<String>>, GitHistoryError> {
    if !is_inside_worktree(root)? {
        return Ok(None);
    }

    let commit_ids = git_stdout(root, ["rev-list", "--reverse", "HEAD"])?;
    Ok(Some(
        commit_ids
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

pub fn collect_git_plan(
    root: &Path,
    options: &GitHistoryAnalyzerOptions,
) -> Result<Option<GitRepositoryPlan>, GitHistoryError> {
    if !is_inside_worktree(root)? {
        return Ok(None);
    }

    let is_shallow = is_shallow_repository(root)?;
    let head_commit = git_stdout(root, ["rev-parse", "HEAD"])?;
    let head_timestamp = git_stdout(root, ["show", "-s", "--format=%ct", "HEAD"])?;
    let parsed_head_timestamp =
        non_empty_trimmed(head_timestamp).and_then(|value| value.parse().ok());
    let first_parent_commit_count = match parsed_head_timestamp {
        Some(head_timestamp) => bounded_commit_count(root, head_timestamp, options).ok(),
        None => None,
    };
    let all_reachable_commit_count = match parsed_head_timestamp {
        Some(head_timestamp) => {
            bounded_all_reachable_commit_count(root, head_timestamp, options).ok()
        }
        None => None,
    };
    let merge_commit_count = match parsed_head_timestamp {
        Some(head_timestamp) => bounded_merge_commit_count(root, head_timestamp, options).ok(),
        None => None,
    };

    Ok(Some(GitRepositoryPlan {
        head_commit: non_empty_trimmed(head_commit),
        head_timestamp: parsed_head_timestamp,
        total_commits: first_parent_commit_count,
        is_shallow,
        first_parent_commit_count,
        all_reachable_commit_count,
        merge_commit_count,
    }))
}

pub fn bounded_commit_count(
    root: &Path,
    head_timestamp: i64,
    options: &GitHistoryAnalyzerOptions,
) -> Result<u64, GitHistoryError> {
    let mut args = vec![
        "rev-list".to_owned(),
        "--count".to_owned(),
        "--first-parent".to_owned(),
    ];
    if let Some(max_age_days) = options.max_age_days {
        let cutoff = head_timestamp - max_age_days.max(0) * SECONDS_PER_DAY;
        args.push(format!("--since=@{cutoff}"));
    }
    args.push("HEAD".to_owned());
    let count = git_stdout(root, args)?;
    let mut total = count.trim().parse::<u64>().unwrap_or(0);
    if let Some(max_commits) = options.max_commits {
        total = total.min(max_commits as u64);
    }
    Ok(total)
}

pub fn bounded_all_reachable_commit_count(
    root: &Path,
    head_timestamp: i64,
    options: &GitHistoryAnalyzerOptions,
) -> Result<u64, GitHistoryError> {
    bounded_rev_list_count(root, head_timestamp, options, &[])
}

pub fn bounded_merge_commit_count(
    root: &Path,
    head_timestamp: i64,
    options: &GitHistoryAnalyzerOptions,
) -> Result<u64, GitHistoryError> {
    bounded_rev_list_count(
        root,
        head_timestamp,
        options,
        &["--first-parent", "--merges"],
    )
}

fn bounded_rev_list_count(
    root: &Path,
    head_timestamp: i64,
    options: &GitHistoryAnalyzerOptions,
    extra_args: &[&str],
) -> Result<u64, GitHistoryError> {
    let mut args = vec!["rev-list".to_owned(), "--count".to_owned()];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    if let Some(max_age_days) = options.max_age_days {
        let cutoff = head_timestamp - max_age_days.max(0) * SECONDS_PER_DAY;
        args.push(format!("--since=@{cutoff}"));
    }
    args.push("HEAD".to_owned());
    let count = git_stdout(root, args)?;
    let mut total = count.trim().parse::<u64>().unwrap_or(0);
    if let Some(max_commits) = options.max_commits {
        total = total.min(max_commits as u64);
    }
    Ok(total)
}

pub fn revision_commit_count(root: &Path, revision: &str) -> Result<u64, GitHistoryError> {
    let count = git_stdout(root, ["rev-list", "--count", "--first-parent", revision])?;
    Ok(count.trim().parse::<u64>().unwrap_or(0))
}

pub fn chunk_commit_ids(
    root: &Path,
    commit_ids: &[String],
    chunk_size: usize,
    head_timestamp: i64,
) -> Vec<GitHistoryChunk> {
    let chunk_size = chunk_size.max(1);
    commit_ids
        .chunks(chunk_size)
        .enumerate()
        .map(|(chunk_index, commit_ids)| GitHistoryChunk {
            root: root.to_path_buf(),
            chunk_index,
            commit_ids: commit_ids.to_vec(),
            head_timestamp,
        })
        .collect()
}

pub fn parse_git_log_name_status(output: &str) -> ParsedGitHistoryStream {
    let parsed = parse_git_history_stream(0, 0, output);
    ParsedGitHistoryStream {
        commits: parsed.commits_processed,
        file_changes: parsed.file_changes,
    }
}

pub fn parse_git_history_stream(
    chunk_index: usize,
    head_timestamp: i64,
    output: &str,
) -> GitHistoryChunkResult {
    parse_git_history_stream_with_cochange_limit(chunk_index, head_timestamp, usize::MAX, output)
}

pub fn parse_git_history_stream_with_cochange_limit(
    chunk_index: usize,
    head_timestamp: i64,
    cochange_max_files_per_commit: usize,
    output: &str,
) -> GitHistoryChunkResult {
    let sink = InMemoryGitSink::default();
    let summary = parse_git_history_reader(
        chunk_index,
        head_timestamp,
        cochange_max_files_per_commit,
        output.as_bytes(),
        &sink,
    )
    .expect("in-memory parsing should not fail");
    sink.into_result(summary)
}

pub fn parse_git_history_reader<R, S>(
    chunk_index: usize,
    head_timestamp: i64,
    cochange_max_files_per_commit: usize,
    reader: R,
    sink: &S,
) -> Result<GitChunkSummary, GitHistoryError>
where
    R: BufRead,
    S: GitHistorySink,
{
    parse_git_history_reader_with_progress(
        chunk_index,
        head_timestamp,
        cochange_max_files_per_commit,
        reader,
        sink,
        &mut |_| {},
    )
}

pub fn parse_git_history_reader_with_progress<R, S>(
    chunk_index: usize,
    head_timestamp: i64,
    cochange_max_files_per_commit: usize,
    reader: R,
    sink: &S,
    progress: &mut dyn FnMut(GitHistoryProgress),
) -> Result<GitChunkSummary, GitHistoryError>
where
    R: BufRead,
    S: GitHistorySink,
{
    let mut summary = GitChunkSummary {
        chunk_index,
        commits_processed: 0,
        file_changes: 0,
        cochange_pairs: 0,
        broad_commits_skipped_for_cochange: 0,
        max_touched_files: 0,
        broadest_commit: None,
    };
    let mut current = None;
    let mut commits = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(GitHistoryError::ReadStream)?;
        if let Some(header) = line.strip_prefix(RECORD_SEPARATOR) {
            if let Some(commit) = current.take() {
                commits.push(commit);
            }
            current = parse_commit_header(header);
        } else if let Some(commit) = &mut current {
            if let Some(change) = parse_numstat_line(line.trim()) {
                commit.add_change(change);
            }
        }
    }

    if let Some(commit) = current.take() {
        commits.push(commit);
    }

    let rename_aliases = rename_aliases(&commits);
    for mut commit in commits {
        commit.apply_rename_aliases(&rename_aliases);
        let delta = process_commit(
            commit,
            head_timestamp,
            cochange_max_files_per_commit,
            sink,
            &mut summary,
        )?;
        progress(delta);
    }

    Ok(summary)
}

fn parse_commit_header(header: &str) -> Option<ParsedCommit> {
    let mut fields = header.trim().split(FIELD_SEPARATOR);
    let hash = fields.next()?.trim();
    let parents = fields.next().unwrap_or_default().trim();
    let author = fields.next()?.trim();
    let timestamp = fields.next()?.trim().parse().ok()?;

    Some(ParsedCommit {
        hash: hash.to_owned(),
        parent_count: parents.split_whitespace().count(),
        author: author.to_owned(),
        timestamp,
        changes_by_path: BTreeMap::new(),
    })
}

fn process_commit<S>(
    commit: ParsedCommit,
    head_timestamp: i64,
    cochange_max_files_per_commit: usize,
    sink: &S,
    summary: &mut GitChunkSummary,
) -> Result<GitHistoryProgress, GitHistoryError>
where
    S: GitHistorySink,
{
    let _is_merge = commit.parent_count > 1;
    let changes: Vec<_> = commit.changes_by_path.into_values().collect();
    let touched_file_count = changes.len();
    summary.commits_processed += 1;
    summary.file_changes += touched_file_count as u64;
    if touched_file_count as u64 > summary.max_touched_files {
        summary.max_touched_files = touched_file_count as u64;
        summary.broadest_commit = Some(commit.hash.clone());
    }

    sink.store_git_repository_delta(GitRepositoryDelta {
        authors: vec![commit.author.clone()],
        first_commit_timestamp: Some(commit.timestamp),
        last_commit_timestamp: Some(commit.timestamp),
    })?;

    let bulk_weight = bulk_weight(touched_file_count);
    let recency_weight = recency_weight(head_timestamp, commit.timestamp);
    let recent_cutoff = head_timestamp - RECENT_CHURN_WINDOW_DAYS * SECONDS_PER_DAY;
    let is_recent = commit.timestamp >= recent_cutoff;
    let mut metrics = Vec::with_capacity(changes.len());
    let mut authors = Vec::with_capacity(changes.len());

    for change in &changes {
        metrics.push(GitFileMetricDelta {
            path: change.path.clone(),
            commits: 1,
            total_added_lines: change.added_lines,
            total_deleted_lines: change.deleted_lines,
            recent_added_lines: if is_recent { change.added_lines } else { 0 },
            recent_deleted_lines: if is_recent { change.deleted_lines } else { 0 },
            first_touch_timestamp: commit.timestamp,
            last_touch_timestamp: commit.timestamp,
        });

        let changed_lines = change.added_lines + change.deleted_lines;
        authors.push(GitFileAuthorDelta {
            path: change.path.clone(),
            author: commit.author.clone(),
            touch_count: 1,
            meaningful_commit_count: u64::from(changed_lines > 0),
            effective_changed_lines: changed_lines,
            ownership_line_recency_score: changed_lines as f64 * bulk_weight * recency_weight,
        });
    }

    if !metrics.is_empty() {
        sink.store_git_file_metrics(metrics)?;
        sink.store_git_file_authors(authors)?;
    }

    if touched_file_count > cochange_max_files_per_commit {
        summary.broad_commits_skipped_for_cochange += 1;
    } else {
        let paths: Vec<_> = changes.iter().map(|change| change.path.as_str()).collect();
        for (left_index, left_path) in paths.iter().enumerate() {
            for right_path in paths.iter().skip(left_index + 1) {
                let (left_path, right_path) = ordered_pair(left_path, right_path);
                summary.cochange_pairs += 1;
                sink.store_git_cochanges(vec![GitCochangeDelta {
                    left_path,
                    right_path,
                    count: 1,
                }])?;
            }
        }
    }

    Ok(GitHistoryProgress {
        commits_processed: 1,
        file_changes: touched_file_count as u64,
    })
}

fn parse_numstat_line(line: &str) -> Option<ParsedFileChange> {
    let mut fields = line.split('\t');
    let added = fields.next()?.trim();
    let deleted = fields.next()?.trim();
    let raw_path = fields.next()?.trim();
    let (old_path, path) = parse_numstat_path(raw_path)?;

    Some(ParsedFileChange {
        path,
        old_path,
        added_lines: parse_numstat_count(added),
        deleted_lines: parse_numstat_count(deleted),
    })
}

fn parse_numstat_path(raw_path: &str) -> Option<(Option<String>, String)> {
    if let Some((old_path, new_path)) = parse_rename_path(raw_path) {
        return Some((
            Some(normalize_git_path(&old_path)?),
            normalize_git_path(&new_path)?,
        ));
    }

    Some((None, normalize_git_path(raw_path)?))
}

fn parse_rename_path(raw_path: &str) -> Option<(String, String)> {
    let (before, after) = raw_path.split_once(" => ")?;
    if let Some(open_index) = before.rfind('{') {
        let prefix = &before[..open_index];
        let old_part = &before[open_index + 1..];
        let close_index = after.find('}')?;
        let new_part = &after[..close_index];
        let suffix = &after[close_index + 1..];
        return Some((
            format!("{prefix}{old_part}{suffix}"),
            format!("{prefix}{new_part}{suffix}"),
        ));
    }

    Some((before.to_owned(), after.to_owned()))
}

fn parse_numstat_count(value: &str) -> u64 {
    value.parse().unwrap_or(0)
}

fn normalize_git_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

#[derive(Debug)]
struct ParsedCommit {
    hash: String,
    parent_count: usize,
    author: String,
    timestamp: i64,
    changes_by_path: BTreeMap<String, ParsedFileChange>,
}

impl ParsedCommit {
    fn add_change(&mut self, change: ParsedFileChange) {
        self.changes_by_path
            .entry(change.path.clone())
            .and_modify(|existing| {
                existing.added_lines += change.added_lines;
                existing.deleted_lines += change.deleted_lines;
            })
            .or_insert(change);
    }

    fn apply_rename_aliases(&mut self, aliases: &BTreeMap<String, String>) {
        let mut rekeyed = BTreeMap::new();
        for mut change in std::mem::take(&mut self.changes_by_path).into_values() {
            if let Some(new_path) = resolve_rename_alias(aliases, &change.path) {
                change.path = new_path;
            }
            rekeyed
                .entry(change.path.clone())
                .and_modify(|existing: &mut ParsedFileChange| {
                    existing.added_lines += change.added_lines;
                    existing.deleted_lines += change.deleted_lines;
                })
                .or_insert(change);
        }
        self.changes_by_path = rekeyed;
    }
}

#[derive(Debug)]
struct ParsedFileChange {
    path: String,
    old_path: Option<String>,
    added_lines: u64,
    deleted_lines: u64,
}

fn rename_aliases(commits: &[ParsedCommit]) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    for commit in commits {
        for change in commit.changes_by_path.values() {
            if let Some(old_path) = &change.old_path {
                let new_path = resolve_rename_alias(&aliases, &change.path)
                    .unwrap_or_else(|| change.path.clone());
                aliases.insert(old_path.clone(), new_path.clone());
                for aliased_path in aliases.values_mut() {
                    if *aliased_path == *old_path {
                        *aliased_path = new_path.clone();
                    }
                }
            }
        }
    }
    aliases
}

fn resolve_rename_alias(aliases: &BTreeMap<String, String>, path: &str) -> Option<String> {
    let mut current = path;
    let mut resolved = None;
    for _ in 0..aliases.len() {
        let Some(next) = aliases.get(current) else {
            break;
        };
        resolved = Some(next.clone());
        current = next;
    }
    resolved
}

struct GitBatchingSink<'a, S> {
    inner: &'a S,
    batch_size: usize,
    file_metrics: RefCell<Vec<GitFileMetricDelta>>,
    file_authors: RefCell<Vec<GitFileAuthorDelta>>,
    cochanges: RefCell<Vec<GitCochangeDelta>>,
    repository: RefCell<GitRepositoryAccumulator>,
}

impl<'a, S> GitBatchingSink<'a, S>
where
    S: GitHistorySink,
{
    fn new(inner: &'a S, batch_size: usize) -> Self {
        Self {
            inner,
            batch_size: batch_size.max(1),
            file_metrics: RefCell::new(Vec::new()),
            file_authors: RefCell::new(Vec::new()),
            cochanges: RefCell::new(Vec::new()),
            repository: RefCell::new(GitRepositoryAccumulator::default()),
        }
    }

    fn flush_all(&self) -> Result<(), GitHistoryError> {
        self.flush_file_metrics()?;
        self.flush_file_authors()?;
        self.flush_cochanges()?;
        self.flush_repository()
    }

    fn flush_file_metrics(&self) -> Result<(), GitHistoryError> {
        let mut file_metrics = self.file_metrics.borrow_mut();
        if !file_metrics.is_empty() {
            self.inner
                .store_git_file_metrics(std::mem::take(&mut *file_metrics))?;
        }
        Ok(())
    }

    fn flush_file_authors(&self) -> Result<(), GitHistoryError> {
        let mut file_authors = self.file_authors.borrow_mut();
        if !file_authors.is_empty() {
            self.inner
                .store_git_file_authors(std::mem::take(&mut *file_authors))?;
        }
        Ok(())
    }

    fn flush_cochanges(&self) -> Result<(), GitHistoryError> {
        let mut cochanges = self.cochanges.borrow_mut();
        if !cochanges.is_empty() {
            self.inner
                .store_git_cochanges(std::mem::take(&mut *cochanges))?;
        }
        Ok(())
    }

    fn flush_repository(&self) -> Result<(), GitHistoryError> {
        let mut repository = self.repository.borrow_mut();
        if !repository.is_empty() {
            self.inner.store_git_repository_delta(repository.drain())?;
        }
        Ok(())
    }
}

impl<S> GitHistorySink for GitBatchingSink<'_, S>
where
    S: GitHistorySink,
{
    fn store_git_chunk_summary(&self, summary: GitChunkSummary) -> Result<(), GitHistoryError> {
        self.inner.store_git_chunk_summary(summary)
    }

    fn store_git_file_metrics(
        &self,
        metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), GitHistoryError> {
        let should_flush = {
            let mut file_metrics = self.file_metrics.borrow_mut();
            file_metrics.extend(metrics);
            file_metrics.len() >= self.batch_size
        };
        if should_flush {
            self.flush_file_metrics()?;
        }
        Ok(())
    }

    fn store_git_file_authors(
        &self,
        authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), GitHistoryError> {
        let should_flush = {
            let mut file_authors = self.file_authors.borrow_mut();
            file_authors.extend(authors);
            file_authors.len() >= self.batch_size
        };
        if should_flush {
            self.flush_file_authors()?;
        }
        Ok(())
    }

    fn store_git_cochanges(&self, cochanges: Vec<GitCochangeDelta>) -> Result<(), GitHistoryError> {
        let should_flush = {
            let mut buffered = self.cochanges.borrow_mut();
            buffered.extend(cochanges);
            buffered.len() >= self.batch_size
        };
        if should_flush {
            self.flush_cochanges()?;
        }
        Ok(())
    }

    fn store_git_repository_delta(&self, delta: GitRepositoryDelta) -> Result<(), GitHistoryError> {
        self.repository.borrow_mut().add(delta);
        Ok(())
    }
}

#[derive(Default)]
struct GitRepositoryAccumulator {
    authors: BTreeSet<String>,
    first_commit_timestamp: Option<i64>,
    last_commit_timestamp: Option<i64>,
}

impl GitRepositoryAccumulator {
    fn add(&mut self, delta: GitRepositoryDelta) {
        self.authors.extend(delta.authors);
        if let Some(timestamp) = delta.first_commit_timestamp {
            self.first_commit_timestamp = min_timestamp(self.first_commit_timestamp, timestamp);
        }
        if let Some(timestamp) = delta.last_commit_timestamp {
            self.last_commit_timestamp = max_timestamp(self.last_commit_timestamp, timestamp);
        }
    }

    fn is_empty(&self) -> bool {
        self.authors.is_empty()
            && self.first_commit_timestamp.is_none()
            && self.last_commit_timestamp.is_none()
    }

    fn drain(&mut self) -> GitRepositoryDelta {
        GitRepositoryDelta {
            authors: std::mem::take(&mut self.authors).into_iter().collect(),
            first_commit_timestamp: self.first_commit_timestamp.take(),
            last_commit_timestamp: self.last_commit_timestamp.take(),
        }
    }
}

#[derive(Default)]
struct InMemoryGitSink {
    file_metrics: RefCell<Vec<GitFileMetricDelta>>,
    file_authors: RefCell<Vec<GitFileAuthorDelta>>,
    cochanges: RefCell<Vec<GitCochangeDelta>>,
    repository: RefCell<GitRepositoryAccumulator>,
}

impl InMemoryGitSink {
    fn into_result(self, summary: GitChunkSummary) -> GitHistoryChunkResult {
        GitHistoryChunkResult {
            chunk_index: summary.chunk_index,
            commits_processed: summary.commits_processed,
            file_changes: summary.file_changes,
            file_metrics: self.file_metrics.into_inner(),
            file_authors: self.file_authors.into_inner(),
            cochanges: self.cochanges.into_inner(),
            repository: self.repository.into_inner().drain(),
        }
    }
}

impl GitHistorySink for InMemoryGitSink {
    fn store_git_chunk_summary(&self, _summary: GitChunkSummary) -> Result<(), GitHistoryError> {
        Ok(())
    }

    fn store_git_file_metrics(
        &self,
        metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), GitHistoryError> {
        self.file_metrics.borrow_mut().extend(metrics);
        Ok(())
    }

    fn store_git_file_authors(
        &self,
        authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), GitHistoryError> {
        self.file_authors.borrow_mut().extend(authors);
        Ok(())
    }

    fn store_git_cochanges(&self, cochanges: Vec<GitCochangeDelta>) -> Result<(), GitHistoryError> {
        self.cochanges.borrow_mut().extend(cochanges);
        Ok(())
    }

    fn store_git_repository_delta(&self, delta: GitRepositoryDelta) -> Result<(), GitHistoryError> {
        self.repository.borrow_mut().add(delta);
        Ok(())
    }
}

fn bulk_weight(touched_file_count: usize) -> f64 {
    if touched_file_count <= 10 {
        1.0
    } else {
        (10.0 / touched_file_count as f64).sqrt().max(0.10)
    }
}

fn recency_weight(head_timestamp: i64, commit_timestamp: i64) -> f64 {
    let age_days = ((head_timestamp - commit_timestamp).max(0) as f64) / SECONDS_PER_DAY as f64;
    0.5_f64.powf(age_days / OWNERSHIP_HALF_LIFE_DAYS)
}

fn ordered_pair(left_path: &str, right_path: &str) -> (String, String) {
    if left_path <= right_path {
        (left_path.to_owned(), right_path.to_owned())
    } else {
        (right_path.to_owned(), left_path.to_owned())
    }
}

fn min_timestamp(current: Option<i64>, candidate: i64) -> Option<i64> {
    Some(current.map_or(candidate, |current| current.min(candidate)))
}

fn max_timestamp(current: Option<i64>, candidate: i64) -> Option<i64> {
    Some(current.map_or(candidate, |current| current.max(candidate)))
}

fn git_stdout<I, S>(root: &Path, args: I) -> Result<String, GitHistoryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|source| GitHistoryError::CommandStart {
            root: root.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(GitHistoryError::CommandFailed {
            root: root.to_path_buf(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn non_empty_trimmed(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn is_inside_worktree(root: &Path) -> Result<bool, GitHistoryError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .output()
        .map_err(|source| GitHistoryError::CommandStart {
            root: root.to_path_buf(),
            source,
        })?;

    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn is_shallow_repository(root: &Path) -> Result<bool, GitHistoryError> {
    Ok(git_stdout(root, ["rev-parse", "--is-shallow-repository"])?
        .trim()
        .eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        chunk_commit_ids, parse_git_history_reader, parse_git_history_reader_with_progress,
        parse_git_history_stream, parse_git_history_stream_with_cochange_limit,
        parse_git_log_name_status, GitBatchingSink, GitHistoryAnalyzer, GitHistoryAnalyzerOptions,
        GitHistoryChunk, GitHistoryProgress, InMemoryGitSink,
        DEFAULT_GIT_COCHANGE_MAX_FILES_PER_COMMIT, DEFAULT_GIT_DELTA_BATCH_SIZE,
        DEFAULT_GIT_MAX_AGE_DAYS, DEFAULT_GIT_MAX_COMMITS, SECONDS_PER_DAY,
    };

    #[test]
    fn default_options_use_bounded_recent_stream_defaults() {
        let options = GitHistoryAnalyzerOptions::default();

        assert_eq!(options.max_commits, Some(DEFAULT_GIT_MAX_COMMITS));
        assert_eq!(options.max_age_days, Some(DEFAULT_GIT_MAX_AGE_DAYS));
        assert!(options.detect_renames);
        assert_eq!(
            options.cochange_max_files_per_commit,
            DEFAULT_GIT_COCHANGE_MAX_FILES_PER_COMMIT
        );
        assert_eq!(options.delta_batch_size, DEFAULT_GIT_DELTA_BATCH_SIZE);
    }

    #[test]
    fn chunking_uses_configured_size_and_preserves_order() {
        let commits = ["a", "b", "c", "d", "e"].map(str::to_owned);

        let chunks = chunk_commit_ids(&PathBuf::from("."), &commits, 2, 123);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].commit_ids, vec!["a", "b"]);
        assert_eq!(chunks[1].commit_ids, vec!["c", "d"]);
        assert_eq!(chunks[2].commit_ids, vec!["e"]);
        assert_eq!(chunks[0].head_timestamp, 123);
    }

    #[test]
    fn parses_streamed_metadata_and_numstat_output() {
        let parsed = parse_git_history_stream(
            7,
            1_000_000,
            "\x1eabc\x1fparent\x1fAlice <alice@example.invalid>\x1f999000\n\
             3\t2\tsrc/main.rs\n\
             -\t-\tassets/logo.png\n\
             \x1edef\x1fabc\x1fBob <bob@example.invalid>\x1f1000000\n\
             1\t0\tsrc/main.rs\n\
             4\t1\tsrc/lib.rs\n",
        );

        assert_eq!(parsed.chunk_index, 7);
        assert_eq!(parsed.commits_processed, 2);
        assert_eq!(parsed.file_changes, 4);
        assert_eq!(parsed.file_metrics.len(), 4);
        let main_added = parsed
            .file_metrics
            .iter()
            .filter(|metric| metric.path == "src/main.rs")
            .map(|metric| metric.total_added_lines)
            .sum::<u64>();
        assert_eq!(main_added, 4);
        let binary = parsed
            .file_metrics
            .iter()
            .find(|metric| metric.path == "assets/logo.png")
            .expect("binary metric should exist");
        assert_eq!(binary.total_added_lines, 0);
        assert_eq!(binary.total_deleted_lines, 0);
        assert!(parsed
            .file_authors
            .iter()
            .any(|author| author.path == "src/main.rs"
                && author.author == "Alice <alice@example.invalid>"));
    }

    #[test]
    fn broad_commits_skip_cochange_but_keep_churn_and_ownership() {
        let output = "\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f100\n\
             1\t0\ta.rs\n\
             1\t0\tb.rs\n\
             1\t0\tc.rs\n";
        let parsed = parse_git_history_stream_with_cochange_limit(0, 100, 2, output);

        assert_eq!(parsed.file_metrics.len(), 3);
        assert_eq!(parsed.file_authors.len(), 3);
        assert!(parsed.cochanges.is_empty());
        assert_eq!(parsed.commits_processed, 1);

        let sink = InMemoryGitSink::default();
        let summary = parse_git_history_reader(0, 100, 2, output.as_bytes(), &sink)
            .expect("stream should parse");
        assert_eq!(summary.broad_commits_skipped_for_cochange, 1);
        assert_eq!(summary.max_touched_files, 3);
    }

    #[test]
    fn legacy_name_status_counter_counts_commits_and_file_rows() {
        let parsed = parse_git_log_name_status(
            "\x1eabc\x1fp\x1fAlice <alice@example.invalid>\x1f1\n\
             1\t0\tsrc/main.rs\n\
             2\t0\tsrc/lib.rs\n\
             \x1edef\x1fabc\x1fBob <bob@example.invalid>\x1f2\n\
             1\t0\told.rs\n",
        );

        assert_eq!(parsed.commits, 2);
        assert_eq!(parsed.file_changes, 3);
    }

    #[test]
    fn cochange_pairs_are_deterministic_and_skip_single_file_commits() {
        let parsed = parse_git_history_stream(
            0,
            100,
            "\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f100\n\
             1\t0\tb.rs\n\
             1\t0\ta.rs\n\
             1\t0\tc.rs\n\
             \x1eb\x1fa\x1fAlice <alice@example.invalid>\x1f100\n\
             1\t0\tsingle.rs\n",
        );

        assert_eq!(parsed.cochanges.len(), 3);
        assert_eq!(parsed.cochanges[0].left_path, "a.rs");
        assert_eq!(parsed.cochanges[0].right_path, "b.rs");
        assert!(parsed
            .cochanges
            .iter()
            .all(|cochange| cochange.left_path < cochange.right_path));
    }

    #[test]
    fn recent_churn_uses_head_timestamp_window() {
        let head = 1_000 * SECONDS_PER_DAY;
        let old = head - 91 * SECONDS_PER_DAY;
        let recent = head - 90 * SECONDS_PER_DAY;
        let parsed = parse_git_history_stream(
            0,
            head,
            &format!(
                "\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f{old}\n1\t0\told.rs\n\
                 \x1eb\x1fa\x1fAlice <alice@example.invalid>\x1f{recent}\n2\t3\told.rs\n"
            ),
        );

        let total_added = parsed
            .file_metrics
            .iter()
            .map(|metric| metric.total_added_lines)
            .sum::<u64>();
        let recent_added = parsed
            .file_metrics
            .iter()
            .map(|metric| metric.recent_added_lines)
            .sum::<u64>();
        assert_eq!(total_added, 3);
        assert_eq!(recent_added, 2);
    }

    #[test]
    fn parses_git_rename_numstat_paths_to_new_path() {
        let simple =
            super::parse_numstat_line("0\t0\told.go => new.go").expect("rename row should parse");
        assert_eq!(simple.old_path.as_deref(), Some("old.go"));
        assert_eq!(simple.path, "new.go");

        let braced = super::parse_numstat_line("0\t0\tsrc/{old.go => new.go}")
            .expect("braced rename row should parse");
        assert_eq!(braced.old_path.as_deref(), Some("src/old.go"));
        assert_eq!(braced.path, "src/new.go");
    }

    #[test]
    fn rename_aliases_attribute_pre_rename_metrics_to_new_path() {
        let parsed = parse_git_history_stream(
            0,
            300,
            "\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f100\n\
             5\t1\told.go\n\
             \x1eb\x1fa\x1fAlice <alice@example.invalid>\x1f200\n\
             0\t0\told.go => new.go\n\
             \x1ec\x1fb\x1fAlice <alice@example.invalid>\x1f300\n\
             2\t0\tnew.go\n",
        );

        assert!(parsed
            .file_metrics
            .iter()
            .all(|metric| metric.path == "new.go"));
        assert_eq!(
            parsed
                .file_metrics
                .iter()
                .map(|metric| metric.total_added_lines)
                .sum::<u64>(),
            7
        );
        assert_eq!(
            parsed
                .file_metrics
                .iter()
                .map(|metric| metric.commits)
                .sum::<u64>(),
            3
        );
    }

    #[test]
    fn parser_reports_progress_per_commit_while_streaming() {
        let sink = InMemoryGitSink::default();
        let mut progress = Vec::new();
        let summary = parse_git_history_reader_with_progress(
            0,
            100,
            usize::MAX,
            "\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f90\n\
             1\t0\ta.rs\n\
             \x1eb\x1fa\x1fBob <bob@example.invalid>\x1f100\n\
             1\t0\tb.rs\n\
             1\t0\tc.rs\n"
                .as_bytes(),
            &sink,
            &mut |delta: GitHistoryProgress| progress.push(delta),
        )
        .expect("stream should parse");

        assert_eq!(summary.commits_processed, 2);
        assert_eq!(
            progress,
            vec![
                GitHistoryProgress {
                    commits_processed: 1,
                    file_changes: 1
                },
                GitHistoryProgress {
                    commits_processed: 1,
                    file_changes: 2
                },
            ]
        );
    }

    #[test]
    fn ownership_inputs_apply_bulk_and_recency_weights() {
        let head = 1_000 * SECONDS_PER_DAY;
        let mut rows = String::from("\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f");
        rows.push_str(&head.to_string());
        rows.push('\n');
        for index in 0..25 {
            rows.push_str(&format!("10\t0\tfile-{index}.rs\n"));
        }

        let parsed = parse_git_history_stream(0, head, &rows);
        let author = parsed
            .file_authors
            .iter()
            .find(|author| author.path == "file-0.rs")
            .expect("author should exist");

        assert_eq!(author.meaningful_commit_count, 1);
        assert_eq!(author.effective_changed_lines, 10);
        assert!(
            author.ownership_line_recency_score < 10.0,
            "bulk commit should be dampened"
        );
    }

    #[test]
    fn batching_sink_flushes_large_cochange_batches() {
        let inner = InMemoryGitSink::default();
        let batching = GitBatchingSink::new(&inner, 10);
        let mut rows = String::from("\x1ea\x1f\x1fAlice <alice@example.invalid>\x1f100\n");
        for index in 0..6 {
            rows.push_str(&format!("1\t0\tfile-{index}.rs\n"));
        }

        let summary = parse_git_history_reader(0, 100, usize::MAX, rows.as_bytes(), &batching)
            .expect("stream should parse");
        batching.flush_all().expect("batches should flush");
        let result = inner.into_result(summary);

        assert_eq!(result.cochanges.len(), 15);
    }

    #[test]
    fn empty_chunk_succeeds_without_git_subprocess() {
        let analyzer = GitHistoryAnalyzer::with_options(GitHistoryAnalyzerOptions::default());
        let sink = InMemoryGitSink::default();

        let result = analyzer
            .analyze_chunk(
                GitHistoryChunk {
                    root: PathBuf::from("."),
                    chunk_index: 3,
                    commit_ids: Vec::new(),
                    head_timestamp: 0,
                },
                &sink,
            )
            .expect("empty chunk should succeed");

        assert_eq!(result.chunk_index, 3);
        assert_eq!(result.commits_processed, 0);
        assert_eq!(result.file_changes, 0);
    }
}
