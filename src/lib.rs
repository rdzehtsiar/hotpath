// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, Error as IgnoreError, WalkBuilder};
use serde::Serialize;

pub mod complexity;
pub mod context;
pub mod dependency;
pub mod diff;
pub mod git;
pub mod graph;
pub mod parse;
pub mod scoring;
pub mod storage;

pub use complexity::{
    ComplexityFileRecord, ComplexityReport, ComplexitySummary, ComplexitySymbolRecord,
    COMPLEXITY_SCHEMA_VERSION,
};
pub use context::{
    estimate_context, parse_budget_tokens, BudgetParseError, ContextBudgetStatus, ContextGroupRow,
    ContextOptions, ContextReport, ContextSkippedReason, ContextSkippedRow, ContextSummary,
    CONTEXT_SCHEMA_VERSION,
};
pub use dependency::{
    fan_metrics, resolve_dependencies, DependencyFanMetrics, FileDependencyFan,
    ResolvedDependencyEdge,
};
pub use graph::{GraphReport, GraphSummary, GRAPH_SCHEMA_VERSION};
pub use parse::{
    ParseFileReason, ParseFileRecord, ParseFileStatus, ParseImportRecord, ParseReport,
    ParseSummary, ParseSymbolRecord, ParseWarning,
};

#[cfg(test)]
const BINARY_SAMPLE_BYTES: usize = 8 * 1024;
const MAX_TEXT_READ_BYTES: u64 = 8 * 1024 * 1024;
pub const SCAN_SCHEMA_VERSION: &str = "hotpath.scan.v1";
pub const PARSE_SCHEMA_VERSION: &str = "hotpath.parse.v1";
pub const DEFAULT_HOTSPOTS_LIMIT: usize = 10;
const SUMMARY_LABEL_WIDTH: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotspotsOptions {
    pub limit: usize,
    pub exclude_generated: bool,
    pub exclude_vendor: bool,
}

impl Default for HotspotsOptions {
    fn default() -> Self {
        Self {
            limit: DEFAULT_HOTSPOTS_LIMIT,
            exclude_generated: false,
            exclude_vendor: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedPath {
    value: String,
    used_replacement: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Binary,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileWarning {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanWarning {
    pub code: &'static str,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileRecord {
    pub path: String,
    pub byte_size: Option<u64>,
    pub extension: Option<String>,
    pub language: Option<&'static str>,
    pub line_count: Option<u64>,
    pub is_vendor: bool,
    pub is_generated: bool,
    pub content: ContentKind,
    pub is_symlink: bool,
    pub classification: &'static str,
    pub warnings: Vec<FileWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContentSummary {
    pub text_files: u64,
    pub binary_files: u64,
    pub unknown_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlagSummary {
    pub generated_files: u64,
    pub vendor_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarningSummary {
    pub total_warnings: u64,
    pub scan_warnings: u64,
    pub unreadable_warnings: u64,
    pub skipped_warnings: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    pub total_files: u64,
    pub total_bytes: u64,
    pub content: ContentSummary,
    pub flags: FlagSummary,
    pub warnings: WarningSummary,
    pub languages: BTreeMap<&'static str, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanReport {
    pub status: &'static str,
    pub file_walking: &'static str,
    pub classification: &'static str,
    pub warnings: Vec<ScanWarning>,
    pub files: Vec<FileRecord>,
}

impl ScanReport {
    #[cfg(test)]
    fn from_files(files: Vec<FileRecord>) -> Self {
        Self::from_parts(Vec::new(), files)
    }

    fn from_parts(warnings: Vec<ScanWarning>, files: Vec<FileRecord>) -> Self {
        Self {
            status: "ok",
            file_walking: "implemented",
            classification: "implemented",
            warnings,
            files,
        }
    }

    fn summary(&self) -> ScanSummary {
        let mut summary = initial_scan_summary(self.files.len(), self.warnings.len());

        accumulate_scan_warnings(&mut summary.warnings, &self.warnings);
        for file in &self.files {
            accumulate_file_facts(&mut summary, file);
        }

        summary
    }
}

fn initial_scan_summary(total_files: usize, scan_warnings: usize) -> ScanSummary {
    ScanSummary {
        total_files: total_files as u64,
        total_bytes: 0,
        content: ContentSummary {
            text_files: 0,
            binary_files: 0,
            unknown_files: 0,
        },
        flags: FlagSummary {
            generated_files: 0,
            vendor_files: 0,
        },
        warnings: WarningSummary {
            total_warnings: scan_warnings as u64,
            scan_warnings: scan_warnings as u64,
            unreadable_warnings: 0,
            skipped_warnings: 0,
        },
        languages: BTreeMap::new(),
    }
}

fn accumulate_scan_warnings(summary: &mut WarningSummary, warnings: &[ScanWarning]) {
    for warning in warnings {
        accumulate_warning_counters(summary, warning.code);
    }
}

fn accumulate_file_facts(summary: &mut ScanSummary, file: &FileRecord) {
    summary.total_bytes += file.byte_size.unwrap_or(0);
    accumulate_content_count(&mut summary.content, file.content);
    accumulate_flag_counts(&mut summary.flags, file);

    if let Some(language) = file.language {
        *summary.languages.entry(language).or_insert(0) += 1;
    }

    for warning in &file.warnings {
        summary.warnings.total_warnings += 1;
        accumulate_warning_counters(&mut summary.warnings, warning.code);
    }
}

fn accumulate_content_count(summary: &mut ContentSummary, content: ContentKind) {
    match content {
        ContentKind::Text => summary.text_files += 1,
        ContentKind::Binary => summary.binary_files += 1,
        ContentKind::Unknown => summary.unknown_files += 1,
    }
}

fn accumulate_flag_counts(summary: &mut FlagSummary, file: &FileRecord) {
    if file.is_generated {
        summary.generated_files += 1;
    }

    if file.is_vendor {
        summary.vendor_files += 1;
    }
}

fn accumulate_warning_counters(summary: &mut WarningSummary, code: &str) {
    if is_unreadable_warning(code) {
        summary.unreadable_warnings += 1;
    }

    if is_skipped_warning(code) {
        summary.skipped_warnings += 1;
    }
}

#[derive(Debug, Serialize)]
struct ScanJsonReport<'a> {
    schema_version: &'static str,
    summary: ScanSummary,
    warnings: &'a [ScanWarning],
    files: &'a [FileRecord],
}

#[derive(Debug, Serialize)]
struct ParseJsonReport<'a> {
    schema_version: &'static str,
    summary: ParseSummary,
    warnings: &'a [ParseWarning],
    files: &'a [ParseFileRecord],
    symbols: &'a [ParseSymbolRecord],
    imports: &'a [ParseImportRecord],
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DiffRiskReport {
    schema_version: &'static str,
    range: diff::DiffRangeMetadata,
    summary: DiffRiskSummary,
    changed_files: Vec<diff::DiffChangedFile>,
    architecture: diff::DiffArchitectureStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct DiffRiskSummary {
    changed_file_count: u64,
    touched_hotspot_count: u64,
    touched_hotspots: Vec<TouchedHotspot>,
    context_token_delta: i64,
    architecture: diff::DiffArchitectureStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TouchedHotspot {
    rank: u64,
    path: String,
    score: f64,
}

#[derive(Debug)]
pub enum ScanError {
    CurrentDir(std::io::Error),
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
    RootNotDirectory {
        path: PathBuf,
    },
    RelativePath {
        root: PathBuf,
        path: PathBuf,
    },
    Index(storage::index::IndexError),
    PersistSymbols(storage::index::IndexError),
    Json(serde_json::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Root { path, source } => {
                write!(
                    f,
                    "failed to access scan root '{}': {source}",
                    path.display()
                )
            }
            Self::RootNotDirectory { path } => {
                write!(f, "scan root '{}' is not a directory", path.display())
            }
            Self::RelativePath { root, path } => write!(
                f,
                "failed to make '{}' relative to scan root '{}'",
                path.display(),
                root.display()
            ),
            Self::Index(source) => {
                write_persistence_error(f, "persist scan results", source, "scan")
            }
            Self::PersistSymbols(source) => {
                write_persistence_error(f, "persist parser symbols", source, "parse")
            }
            Self::Json(source) => write!(f, "failed to render scan JSON: {source}"),
        }
    }
}

impl StdError for ScanError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) | Self::Root { source, .. } => Some(source),
            Self::RootNotDirectory { .. } | Self::RelativePath { .. } => None,
            Self::Index(source) | Self::PersistSymbols(source) => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum DoctorError {
    CurrentDir(std::io::Error),
    Index(storage::index::IndexError),
}

impl fmt::Display for DoctorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Index(source) => write!(f, "failed to inspect Hotpath index: {source}"),
        }
    }
}

impl StdError for DoctorError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Index(source) => Some(source),
        }
    }
}

impl From<storage::index::IndexError> for DoctorError {
    fn from(source: storage::index::IndexError) -> Self {
        Self::Index(source)
    }
}

impl From<storage::index::IndexError> for ScanError {
    fn from(source: storage::index::IndexError) -> Self {
        Self::Index(source)
    }
}

impl From<serde_json::Error> for ScanError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

#[derive(Debug)]
pub enum ExplainGitCommandError {
    CurrentDir(std::io::Error),
    Git(git::GitExplainError),
    Index(storage::index::IndexError),
}

impl fmt::Display for ExplainGitCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Git(source) => write!(f, "{source}"),
            Self::Index(source) => write!(f, "failed to persist Git analysis: {source}"),
        }
    }
}

impl StdError for ExplainGitCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Index(source) => Some(source),
        }
    }
}

impl From<git::GitExplainError> for ExplainGitCommandError {
    fn from(source: git::GitExplainError) -> Self {
        Self::Git(source)
    }
}

impl From<storage::index::IndexError> for ExplainGitCommandError {
    fn from(source: storage::index::IndexError) -> Self {
        Self::Index(source)
    }
}

#[derive(Debug)]
pub enum HotspotsCommandError {
    CurrentDir(std::io::Error),
    Git(git::GitHistoryError),
    Scan(ScanError),
    PersistScan(storage::index::IndexError),
    PersistGitAnalysis(storage::index::IndexError),
    PersistHotspots(storage::index::IndexError),
}

#[derive(Debug)]
pub enum DiffCommandError {
    CurrentDir(std::io::Error),
    Diff(diff::DiffError),
    Git(git::GitHistoryError),
    Scan(ScanError),
    Json(serde_json::Error),
}

#[derive(Debug)]
pub enum ContextCommandError {
    CurrentDir(std::io::Error),
    Scan(ScanError),
    PersistScan(storage::index::IndexError),
    Json(serde_json::Error),
}

#[derive(Debug)]
pub enum ExplainCommandError {
    CurrentDir(std::io::Error),
    Git(git::GitHistoryError),
    Path(ExplainPathError),
    Scan(ScanError),
    PersistScan(storage::index::IndexError),
    PersistGitAnalysis(storage::index::IndexError),
    PersistHotspots(storage::index::IndexError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainPathError {
    EmptyPath,
    PathOutsideRepository,
    UnsupportedPathEncoding,
    AmbiguousPath { first: String, second: String },
    NotCurrentFile,
}

impl fmt::Display for ExplainCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Git(source) => write_explain_git_error(f, source),
            Self::Path(source) => write!(f, "{source}"),
            Self::Scan(source) => write!(f, "{source}"),
            Self::PersistScan(source) => {
                write_explain_persistence_error(f, "persist scan results", source)
            }
            Self::PersistGitAnalysis(source) => {
                write_explain_persistence_error(f, "persist Git analysis", source)
            }
            Self::PersistHotspots(source) => {
                write_explain_persistence_error(f, "persist hotspot scores", source)
            }
        }
    }
}

impl fmt::Display for ExplainPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "explain requires a non-empty file path"),
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
            Self::NotCurrentFile => write!(
                f,
                "requested path is not a current scanned file; pass an existing file under the worktree"
            ),
        }
    }
}

impl StdError for ExplainCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Path(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::PersistScan(source)
            | Self::PersistGitAnalysis(source)
            | Self::PersistHotspots(source) => Some(source),
        }
    }
}

impl StdError for ExplainPathError {}

impl From<git::GitHistoryError> for ExplainCommandError {
    fn from(source: git::GitHistoryError) -> Self {
        Self::Git(source)
    }
}

impl From<ScanError> for ExplainCommandError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

impl fmt::Display for HotspotsCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Git(source) => write_hotspots_git_error(f, source),
            Self::Scan(source) => write!(f, "{source}"),
            Self::PersistScan(source) => {
                write_hotspots_persistence_error(f, "persist scan results", source)
            }
            Self::PersistGitAnalysis(source) => {
                write_hotspots_persistence_error(f, "persist Git analysis", source)
            }
            Self::PersistHotspots(source) => {
                write_hotspots_persistence_error(f, "persist hotspot scores", source)
            }
        }
    }
}

impl StdError for HotspotsCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::PersistScan(source)
            | Self::PersistGitAnalysis(source)
            | Self::PersistHotspots(source) => Some(source),
        }
    }
}

impl fmt::Display for DiffCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Diff(source) => write!(f, "{source}"),
            Self::Git(source) => write_diff_git_error(f, source),
            Self::Scan(source) => write!(f, "{source}"),
            Self::Json(source) => write!(f, "failed to render diff JSON: {source}"),
        }
    }
}

impl StdError for DiffCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Diff(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

impl fmt::Display for ContextCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Scan(source) => write!(f, "{source}"),
            Self::PersistScan(source) => {
                write_context_persistence_error(f, "persist scan results", source)
            }
            Self::Json(source) => write!(f, "failed to render context JSON: {source}"),
        }
    }
}

impl StdError for ContextCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::PersistScan(source) => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

impl From<git::GitHistoryError> for HotspotsCommandError {
    fn from(source: git::GitHistoryError) -> Self {
        Self::Git(source)
    }
}

impl From<diff::DiffError> for DiffCommandError {
    fn from(source: diff::DiffError) -> Self {
        Self::Diff(source)
    }
}

impl From<git::GitHistoryError> for DiffCommandError {
    fn from(source: git::GitHistoryError) -> Self {
        Self::Git(source)
    }
}

impl From<ScanError> for DiffCommandError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

impl From<serde_json::Error> for DiffCommandError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

impl From<ScanError> for HotspotsCommandError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

impl From<ScanError> for ContextCommandError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

impl From<serde_json::Error> for ContextCommandError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

pub fn scan_current_dir() -> Result<ScanReport, ScanError> {
    let root = env::current_dir().map_err(ScanError::CurrentDir)?;

    scan_repository(root)
}

pub fn scan_repository(root: impl AsRef<Path>) -> Result<ScanReport, ScanError> {
    let requested_root = root.as_ref();
    let root = fs::canonicalize(requested_root).map_err(|source| ScanError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(&root).map_err(|source| ScanError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(ScanError::RootNotDirectory {
            path: requested_root.to_path_buf(),
        });
    }

    fs::read_dir(&root).map_err(|source| ScanError::Root {
        path: requested_root.to_path_buf(),
        source,
    })?;

    let mut warnings = Vec::new();
    let mut files = Vec::new();
    let internal_filter_root = root.clone();

    for entry in WalkBuilder::new(&root)
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(move |entry| !is_internal_entry(&internal_filter_root, entry))
        .build()
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(scan_warning_from_walk_error(&root, &error));
                continue;
            }
        };

        if let Some(error) = entry.error() {
            warnings.push(scan_warning_from_entry_error(&root, &entry, error));
        }

        if !is_walked_file(&entry) {
            if entry.file_type().is_none() {
                warnings.push(scan_warning(
                    "unsupported_file_type",
                    normalized_warning_path(&root, entry.path()),
                    "filesystem entry type is unavailable; entry skipped".to_owned(),
                ));
            }

            continue;
        }

        files.push(classify_file(&root, entry.path())?);
    }

    warnings.sort_by(|left, right| {
        (&left.path, left.code, &left.message).cmp(&(&right.path, right.code, &right.message))
    });
    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(ScanReport::from_parts(warnings, files))
}

pub fn scan_summary() -> Result<String, ScanError> {
    Ok(render_summary(&scan_current_dir_and_persist()?))
}

pub fn scan_json() -> Result<String, ScanError> {
    render_json(&scan_current_dir_and_persist()?)
}

pub fn parse_summary() -> Result<String, ScanError> {
    Ok(render_parse_summary(&parse_current_dir_and_persist()?))
}

pub fn parse_json() -> Result<String, ScanError> {
    render_parse_json(&parse_current_dir_and_persist()?)
}

pub fn complexity_summary() -> Result<String, ScanError> {
    Ok(complexity::render_summary(
        &complexity_current_dir_and_persist()?,
    ))
}

pub fn complexity_json() -> Result<String, ScanError> {
    Ok(complexity::render_json(
        &complexity_current_dir_and_persist()?,
    )?)
}

pub fn graph_summary(selector: &str) -> Result<String, ScanError> {
    Ok(graph::render_summary(&graph_current_dir_and_persist(
        selector,
    )?))
}

pub fn graph_json(selector: &str) -> Result<String, ScanError> {
    Ok(graph::render_json(&graph_current_dir_and_persist(
        selector,
    )?)?)
}

pub fn context(options: ContextOptions, json: bool) -> Result<String, ContextCommandError> {
    let report = context_current_dir_and_persist(options)?;

    if json {
        Ok(serde_json::to_string_pretty(&report)?)
    } else {
        Ok(render_context(&report))
    }
}

pub fn parse_scan_report(scan: &ScanReport) -> ParseReport {
    parse::scaffold_report_from_scan(scan)
}

pub fn hotspots(options: HotspotsOptions) -> Result<String, HotspotsCommandError> {
    let root = env::current_dir().map_err(HotspotsCommandError::CurrentDir)?;
    let analysis = git::analyze_from_head_at(&root)?;
    let scan = scan_repository(&analysis.worktree_root)?;
    let mut index = storage::index::IndexStore::open(&analysis.worktree_root)
        .map_err(HotspotsCommandError::PersistScan)?;

    let scan_run = index
        .persist_scan(&scan)
        .map_err(HotspotsCommandError::PersistScan)?;
    index
        .persist_git_analysis(
            &analysis.worktree_root,
            &analysis.head_commit_id,
            analysis.head_commit_time,
            analysis.recent_window_days as u64,
            &analysis.file_metrics,
            &analysis.co_changes,
        )
        .map_err(HotspotsCommandError::PersistGitAnalysis)?;

    let ranked = ranked_hotspot_scores_from_scan_and_git(
        &scan.files,
        &analysis.file_metrics,
        &analysis.co_changes,
    );
    index
        .persist_hotspots(scan_run.id, &ranked)
        .map_err(HotspotsCommandError::PersistHotspots)?;

    let displayed = select_hotspots_for_output(&ranked, &scan.files, options);

    Ok(render_hotspots(
        &ranked,
        &displayed,
        options,
        analysis.recent_window_days,
    ))
}

pub fn diff_risk(range: &str, json: bool) -> Result<String, DiffCommandError> {
    let report = diff_risk_report(range)?;

    render_diff_risk(&report, json)
}

pub fn pr_risk(base: &str, head: &str, json: bool) -> Result<String, DiffCommandError> {
    diff_risk(&format!("{base}...{head}"), json)
}

pub fn explain(requested_path: impl AsRef<Path>) -> Result<String, ExplainCommandError> {
    let current_dir = env::current_dir().map_err(ExplainCommandError::CurrentDir)?;
    let analysis = git::analyze_from_head_at(&current_dir)?;
    let scan = scan_repository(&analysis.worktree_root)?;
    let path = normalize_explain_file_path(
        &current_dir,
        &analysis.worktree_root,
        requested_path.as_ref(),
        &scan.files,
    )
    .map_err(ExplainCommandError::Path)?;
    let mut index = storage::index::IndexStore::open(&analysis.worktree_root)
        .map_err(ExplainCommandError::PersistScan)?;

    let scan_run = index
        .persist_scan(&scan)
        .map_err(ExplainCommandError::PersistScan)?;
    index
        .persist_git_analysis(
            &analysis.worktree_root,
            &analysis.head_commit_id,
            analysis.head_commit_time,
            analysis.recent_window_days as u64,
            &analysis.file_metrics,
            &analysis.co_changes,
        )
        .map_err(ExplainCommandError::PersistGitAnalysis)?;

    let ranked = ranked_hotspot_scores_from_scan_and_git(
        &scan.files,
        &analysis.file_metrics,
        &analysis.co_changes,
    );
    index
        .persist_hotspots(scan_run.id, &ranked)
        .map_err(ExplainCommandError::PersistHotspots)?;
    let score = ranked
        .iter()
        .find(|ranked_score| ranked_score.score.path == path.as_str())
        .map(|ranked_score| &ranked_score.score)
        .ok_or(ExplainCommandError::Path(ExplainPathError::NotCurrentFile))?;

    Ok(render_explain(score, analysis.recent_window_days))
}

pub fn doctor() -> Result<String, DoctorError> {
    let root = env::current_dir().map_err(DoctorError::CurrentDir)?;

    doctor_repository(root)
}

pub fn explain_git_and_persist(
    requested_path: impl AsRef<Path>,
) -> Result<String, ExplainGitCommandError> {
    let root = env::current_dir().map_err(ExplainGitCommandError::CurrentDir)?;
    let analysis = git::analyze_from_head_at(&root).map_err(git::GitExplainError::from)?;
    let output = git::explain_file_from_analysis_at(&analysis, &root, requested_path)?;
    let mut index = storage::index::IndexStore::open(&analysis.worktree_root)?;

    index.persist_git_analysis(
        &analysis.worktree_root,
        &analysis.head_commit_id,
        analysis.head_commit_time,
        analysis.recent_window_days as u64,
        &analysis.file_metrics,
        &analysis.co_changes,
    )?;

    Ok(output)
}

pub fn doctor_repository(root: impl AsRef<Path>) -> Result<String, DoctorError> {
    let root = root.as_ref();
    let inspection = storage::index::IndexStore::inspect(root)?;
    let index_path = inspection
        .path()
        .strip_prefix(root)
        .unwrap_or_else(|_| inspection.path())
        .to_string_lossy()
        .replace('\\', "/");

    match inspection.schema_version() {
        Some(schema_version) => Ok(render_doctor(
            &index_path,
            &schema_version.to_string(),
            "yes",
            "healthy",
        )),
        None => Ok(render_doctor(&index_path, "none", "no", "missing")),
    }
}

fn scan_current_dir_and_persist() -> Result<ScanReport, ScanError> {
    let root = env::current_dir().map_err(ScanError::CurrentDir)?;
    let report = scan_repository(&root)?;
    let mut index = storage::index::IndexStore::open(&root)?;
    index.persist_scan(&report)?;

    Ok(report)
}

fn parse_current_dir_and_persist() -> Result<ParseReport, ScanError> {
    let root = env::current_dir().map_err(ScanError::CurrentDir)?;
    let scan = scan_repository(&root)?;
    let mut index = storage::index::IndexStore::open(&root)?;
    index.persist_scan(&scan)?;
    let report = parse::report_from_scan(&root, &scan);
    index
        .persist_symbols(&report)
        .map_err(ScanError::PersistSymbols)?;

    Ok(report)
}

fn complexity_current_dir_and_persist() -> Result<ComplexityReport, ScanError> {
    let report = parse_current_dir_and_persist()?;

    Ok(complexity::report_from_parse(&report))
}

fn graph_current_dir_and_persist(selector: &str) -> Result<GraphReport, ScanError> {
    let report = parse_current_dir_and_persist()?;

    Ok(graph::report_from_parse(selector, &report))
}

fn context_current_dir_and_persist(
    options: ContextOptions,
) -> Result<ContextReport, ContextCommandError> {
    let root = env::current_dir().map_err(ContextCommandError::CurrentDir)?;
    let scan = scan_repository(&root).map_err(ContextCommandError::Scan)?;
    let mut index =
        storage::index::IndexStore::open(&root).map_err(ContextCommandError::PersistScan)?;

    index
        .persist_scan(&scan)
        .map_err(ContextCommandError::PersistScan)?;

    Ok(context::estimate_context(&scan.files, options))
}

fn diff_risk_report(range: &str) -> Result<DiffRiskReport, DiffCommandError> {
    let current_dir = env::current_dir().map_err(DiffCommandError::CurrentDir)?;
    let core_report = diff::analyze_committed_tree_diff(&current_dir, range)?;
    let analysis = git::analyze_from_head_at(&current_dir)?;
    let scan = scan_repository(&analysis.worktree_root)?;
    let ranked = ranked_hotspot_scores_from_scan_and_git(
        &scan.files,
        &analysis.file_metrics,
        &analysis.co_changes,
    );
    let touched_hotspots = touched_hotspots(&core_report.changed_files, &scan.files, &ranked);

    Ok(DiffRiskReport {
        schema_version: core_report.schema_version,
        range: core_report.range,
        summary: DiffRiskSummary {
            changed_file_count: core_report.summary.changed_files,
            touched_hotspot_count: touched_hotspots.len() as u64,
            touched_hotspots,
            context_token_delta: core_report.summary.context_token_delta,
            architecture: core_report.architecture,
        },
        changed_files: core_report.changed_files,
        architecture: core_report.architecture,
    })
}

fn touched_hotspots(
    changed_files: &[diff::DiffChangedFile],
    current_files: &[FileRecord],
    ranked_scores: &[scoring::RankedHotspotScore],
) -> Vec<TouchedHotspot> {
    let changed_paths = changed_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let current_paths = current_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();

    ranked_scores
        .iter()
        .take(DEFAULT_HOTSPOTS_LIMIT)
        .filter(|ranked_score| {
            let path = ranked_score.score.path.as_str();

            changed_paths.contains(path) && current_paths.contains(path)
        })
        .map(|ranked_score| TouchedHotspot {
            rank: ranked_score.rank,
            path: ranked_score.score.path.clone(),
            score: ranked_score.score.value,
        })
        .collect()
}

fn ranked_hotspot_scores_from_scan_and_git(
    files: &[FileRecord],
    git_metrics: &[git::GitFileMetrics],
    co_changes: &[git::GitCoChange],
) -> Vec<scoring::RankedHotspotScore> {
    let raw_metrics = scoring::raw_score_metrics_from_scan_and_git(files, git_metrics, co_changes);
    let scores = raw_metrics
        .into_iter()
        .map(scoring::calculate_hotspot_score)
        .collect::<Vec<_>>();

    scoring::rank_hotspot_scores(&scores)
}

fn select_hotspots_for_output<'a>(
    ranked_scores: &'a [scoring::RankedHotspotScore],
    files: &[FileRecord],
    options: HotspotsOptions,
) -> Vec<&'a scoring::RankedHotspotScore> {
    let file_flags = files
        .iter()
        .map(|file| (file.path.as_str(), (file.is_generated, file.is_vendor)))
        .collect::<BTreeMap<_, _>>();

    ranked_scores
        .iter()
        .filter(|ranked_score| {
            let (is_generated, is_vendor) = file_flags
                .get(ranked_score.score.path.as_str())
                .copied()
                .unwrap_or((false, false));

            (!options.exclude_generated || !is_generated) && (!options.exclude_vendor || !is_vendor)
        })
        .take(options.limit)
        .collect()
}

fn has_active_hotspot_output_filter(options: HotspotsOptions) -> bool {
    options.limit != DEFAULT_HOTSPOTS_LIMIT || options.exclude_generated || options.exclude_vendor
}

fn render_hotspot_output_filters(options: HotspotsOptions) -> String {
    let mut filters = Vec::new();

    if options.limit != DEFAULT_HOTSPOTS_LIMIT {
        filters.push(format!("limit {}", options.limit));
    }

    if options.exclude_generated {
        filters.push("exclude generated files".to_owned());
    }

    if options.exclude_vendor {
        filters.push("exclude vendor files".to_owned());
    }

    filters.join(", ")
}

fn render_json(scan: &ScanReport) -> Result<String, ScanError> {
    Ok(serde_json::to_string_pretty(&ScanJsonReport {
        schema_version: SCAN_SCHEMA_VERSION,
        summary: scan.summary(),
        warnings: &scan.warnings,
        files: &scan.files,
    })?)
}

fn render_parse_json(report: &ParseReport) -> Result<String, ScanError> {
    Ok(serde_json::to_string_pretty(&ParseJsonReport {
        schema_version: PARSE_SCHEMA_VERSION,
        summary: report.summary(),
        warnings: &report.warnings,
        files: &report.files,
        symbols: &report.symbols,
        imports: &report.imports,
    })?)
}

fn render_context(report: &ContextReport) -> String {
    let mut output = format!(
        "Hotpath context budget\nscope: current scanned UTF-8 text files from the working directory\nformula: estimated tokens = ceil(byte_size / 4) for UTF-8 text files\n\ntotal estimated tokens  {}\nincluded files          {}\nskipped files           {}\nincluded bytes          {}",
        report.summary.estimated_tokens,
        report.summary.included_files,
        report.summary.skipped_files,
        report.summary.included_bytes
    );

    if has_active_context_output_filter(report.options) {
        output.push_str(&format!(
            "\noutput filters: {}",
            render_context_output_filters(report.options)
        ));
    }

    if let Some(budget) = &report.budget {
        output.push_str(&format!(
            "\nbudget: {}",
            render_context_budget_status(budget)
        ));
    }

    output.push_str("\n\ngroups\n  group path  estimated tokens  bytes  files");

    if report.groups.is_empty() {
        output.push_str("\n  none");
    } else {
        for group in &report.groups {
            output.push_str(&format!(
                "\n  {}  {}  {}  {}",
                group.path, group.estimated_tokens, group.byte_size, group.file_count
            ));
        }
    }

    output.push_str(
        "\n\ncalculation notes\n  - Offline deterministic approximation; no source text leaves the local machine.\n  - Tokenizer-specific counts vary by model and language, so treat this as planning guidance.",
    );

    output
}

fn render_diff_risk(report: &DiffRiskReport, json: bool) -> Result<String, DiffCommandError> {
    if json {
        Ok(serde_json::to_string_pretty(report)?)
    } else {
        Ok(render_diff_risk_text(report))
    }
}

fn render_diff_risk_text(report: &DiffRiskReport) -> String {
    let mut output = format!(
        "Hotpath diff risk\nrange: {}\nChanged files: {}\nTouched hotspots: {}\nArchitecture violations: {}\nContext growth: {:+} tokens",
        report.range.requested,
        report.summary.changed_file_count,
        report.summary.touched_hotspot_count,
        diff_architecture_label(report.summary.architecture),
        report.summary.context_token_delta
    );

    output.push_str("\n\ntouched hotspots");
    if report.summary.touched_hotspots.is_empty() {
        output.push_str("\n  none");
    } else {
        for hotspot in &report.summary.touched_hotspots {
            output.push_str(&format!(
                "\n  #{}  {:.3}  {}",
                hotspot.rank, hotspot.score, hotspot.path
            ));
        }
    }

    output.push_str("\n\nchanged files");
    if report.changed_files.is_empty() {
        output.push_str("\n  none");
    } else {
        for file in &report.changed_files {
            output.push_str(&format!(
                "\n  {}  {}  +{} -{}  {:+} tokens",
                diff_change_kind_label(file.change_kind),
                file.path,
                file.added_lines,
                file.deleted_lines,
                file.context_token_delta
            ));
        }
    }

    output
}

fn diff_architecture_label(status: diff::DiffArchitectureStatus) -> &'static str {
    match status {
        diff::DiffArchitectureStatus::NotEvaluated => "not evaluated",
    }
}

fn diff_change_kind_label(change_kind: diff::DiffChangeKind) -> &'static str {
    match change_kind {
        diff::DiffChangeKind::Added => "added",
        diff::DiffChangeKind::Modified => "modified",
        diff::DiffChangeKind::Deleted => "deleted",
        diff::DiffChangeKind::Renamed => "renamed",
        diff::DiffChangeKind::Copied => "copied",
        diff::DiffChangeKind::TypeChanged => "type_changed",
    }
}

fn has_active_context_output_filter(options: ContextOptions) -> bool {
    options.exclude_generated || options.exclude_vendor
}

fn render_context_output_filters(options: ContextOptions) -> String {
    let mut filters = Vec::new();

    if options.exclude_generated {
        filters.push("exclude generated files");
    }

    if options.exclude_vendor {
        filters.push("exclude vendor files");
    }

    filters.join(", ")
}

fn render_context_budget_status(budget: &ContextBudgetStatus) -> String {
    match (budget.remaining_tokens, budget.over_budget_tokens) {
        (Some(remaining), _) => format!(
            "within budget by {remaining} tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
        (_, Some(over)) => format!(
            "over budget by {over} tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
        (None, None) => format!(
            "within budget by 0 tokens (budget {}, estimated {})",
            budget.budget_tokens, budget.estimated_tokens
        ),
    }
}

fn render_hotspots(
    ranked_scores: &[scoring::RankedHotspotScore],
    displayed_scores: &[&scoring::RankedHotspotScore],
    options: HotspotsOptions,
    recent_window_days: i64,
) -> String {
    let mut output = format!(
        "Hotpath hotspots\nscope: current scanned files plus local Git history reachable from HEAD\nformula: {}\nfiles ranked: {} (showing {})",
        scoring::CURRENT_SCORE_FORMULA_ID,
        ranked_scores.len(),
        displayed_scores.len()
    );

    if has_active_hotspot_output_filter(options) {
        output.push_str(&format!(
            "\noutput filters: {}",
            render_hotspot_output_filters(options)
        ));
    }

    output.push_str("\n\nrank  score  path");

    if displayed_scores.is_empty() {
        output.push_str("\n  none");
    }

    for ranked_score in displayed_scores {
        let score = &ranked_score.score;

        output.push_str(&format!(
            "\n{:>4}  {:.3}  {}",
            ranked_score.rank, score.value, score.path
        ));
        output.push_str(&format!(
            "\n      key contributors: {}",
            render_key_contributors(score)
        ));
        output.push_str(&format!(
            "\n      why: {}",
            render_hotspot_raw_summary(&score.raw_metrics)
        ));
        output.push_str(&format!(
            "\n      limitations: {}",
            render_hotspot_limitations(score)
        ));
    }

    output.push_str(&format!(
        "\n\ncalculation notes\n  - Scores are advisory signals for investigation, not proof that a file is defective.\n  - Inputs come from scanner facts and local Git history reachable from HEAD only.\n  - Recent churn uses the {recent_window_days}-day window before the HEAD committer timestamp.\n  - Missing normalized inputs contribute 0.0; formula weights are not redistributed."
    ));
    if has_active_hotspot_output_filter(options) {
        output.push_str(
            "\n  - Output filters affect displayed rows only; persisted hotspot scores keep the full ranked set and original ranks.",
        );
    }

    output
}

fn render_explain(score: &scoring::HotspotScore, recent_window_days: i64) -> String {
    let mut output = format!(
        "Hotpath score explanation\npath: {}\nscope: current scanned file plus local Git history reachable from HEAD\nformula version: {} (major {}, minor {})\nfinal score: {:.3}\n\nraw metrics",
        score.path,
        score.formula_version.id,
        score.formula_version.major,
        score.formula_version.minor,
        score.value
    );

    output.push_str(&format!(
        "\n  byte size: {}\n  line count: {}\n  commits per file: {}\n  total churn lines: {}\n  recent churn lines ({} days): {}\n  author count: {}\n  dominant owner share: {}\n  co-changed file count: {}",
        render_optional_count(score.raw_metrics.byte_size),
        render_optional_count(score.raw_metrics.line_count),
        render_optional_count(score.raw_metrics.commits_per_file),
        render_optional_count(score.raw_metrics.total_churn_lines),
        recent_window_days,
        render_optional_count(score.raw_metrics.recent_churn_lines),
        render_optional_count(score.raw_metrics.author_count),
        render_optional_share(score.raw_metrics.dominant_owner_share),
        render_optional_count(score.raw_metrics.co_changed_file_count),
    ));

    output.push_str("\n\nnormalized metrics");
    for (name, value) in normalized_metric_rows(&score.normalized_metrics) {
        output.push_str(&format!("\n  {name}: {}", render_optional_score(value)));
    }

    output.push_str("\n\nweighted contributions");
    for term in &score.weighted_terms {
        output.push_str(&format!(
            "\n  {}: weight {:.3} * {} {} = {:.3}",
            term.name,
            term.weight,
            normalized_metric_name(term.metric),
            render_optional_score(term.normalized_input),
            term.weighted_contribution
        ));
    }

    output.push_str("\n\nlimitations");
    if score.limitations.is_empty() {
        output.push_str("\n  - Uses local Git history only; advisory score.");
    } else {
        for limitation in &score.limitations {
            output.push_str(&format!("\n  - {}", limitation.message));
        }
    }

    output.push_str(&format!(
        "\n\ncalculation notes\n  - Scores are advisory signals for investigation, not proof that a file is defective.\n  - Inputs come from scanner facts and local Git history reachable from HEAD only.\n  - Recent churn uses the {recent_window_days}-day window before the HEAD committer timestamp.\n  - Missing normalized inputs contribute 0.0; formula weights are not redistributed."
    ));

    output
}

fn render_doctor(index_path: &str, schema_version: &str, readable: &str, health: &str) -> String {
    format!(
        "Hotpath doctor\nindex path: {index_path}\nschema version: {schema_version}\nreadable: {readable}\nhealth: {health}"
    )
}

fn render_key_contributors(score: &scoring::HotspotScore) -> String {
    let mut terms = score
        .weighted_terms
        .iter()
        .filter(|term| term.weighted_contribution > 0.0)
        .collect::<Vec<_>>();

    terms.sort_by(|left, right| {
        right
            .weighted_contribution
            .total_cmp(&left.weighted_contribution)
            .then_with(|| left.name.cmp(&right.name))
    });

    if terms.is_empty() {
        return "none observed".to_owned();
    }

    terms
        .into_iter()
        .take(3)
        .map(|term| format!("{} {:.3}", term.name, term.weighted_contribution))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_hotspot_raw_summary(raw_metrics: &scoring::RawScoreMetrics) -> String {
    format!(
        "{} commits, {} churn lines, {} recent churn lines, {} authors, {} co-changed files, {}",
        render_optional_count(raw_metrics.commits_per_file),
        render_optional_count(raw_metrics.total_churn_lines),
        render_optional_count(raw_metrics.recent_churn_lines),
        render_optional_count(raw_metrics.author_count),
        render_optional_count(raw_metrics.co_changed_file_count),
        render_size_summary(raw_metrics)
    )
}

fn render_hotspot_limitations(score: &scoring::HotspotScore) -> String {
    if score.limitations.is_empty() {
        return "uses local Git history only; advisory score".to_owned();
    }

    score
        .limitations
        .iter()
        .take(2)
        .map(|limitation| limitation.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn render_optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn render_optional_score(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.3}"))
}

fn render_optional_share(value: Option<f64>) -> String {
    value.map_or_else(
        || "unavailable".to_owned(),
        |value| format!("{:.2}%", value * 100.0),
    )
}

fn render_size_summary(raw_metrics: &scoring::RawScoreMetrics) -> String {
    match (raw_metrics.line_count, raw_metrics.byte_size) {
        (Some(line_count), _) => format!("{line_count} lines"),
        (None, Some(byte_size)) => format!("{byte_size} bytes"),
        (None, None) => "size unavailable".to_owned(),
    }
}

fn normalized_metric_rows(
    metrics: &scoring::NormalizedScoreMetrics,
) -> [(&'static str, Option<f64>); 5] {
    [
        ("size", metrics.size),
        ("churn", metrics.churn),
        ("recent_churn", metrics.recent_churn),
        ("ownership", metrics.ownership),
        ("coupling", metrics.coupling),
    ]
}

fn normalized_metric_name(metric: scoring::NormalizedMetric) -> &'static str {
    match metric {
        scoring::NormalizedMetric::Size => "size",
        scoring::NormalizedMetric::Churn => "churn",
        scoring::NormalizedMetric::RecentChurn => "recent_churn",
        scoring::NormalizedMetric::Ownership => "ownership",
        scoring::NormalizedMetric::Coupling => "coupling",
    }
}

fn normalize_explain_file_path(
    current_dir: &Path,
    worktree_root: &Path,
    requested_path: &Path,
    files: &[FileRecord],
) -> Result<String, ExplainPathError> {
    if requested_path.as_os_str().is_empty() {
        return Err(ExplainPathError::EmptyPath);
    }

    let current_dir =
        fs::canonicalize(current_dir).map_err(|_| ExplainPathError::PathOutsideRepository)?;
    let worktree_root =
        fs::canonicalize(worktree_root).map_err(|_| ExplainPathError::PathOutsideRepository)?;
    let scanned_paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = Vec::new();

    if requested_path.is_absolute() {
        push_explain_relative_candidate(&mut candidates, &worktree_root, requested_path)?;
    } else {
        push_explain_relative_candidate(
            &mut candidates,
            &worktree_root,
            &current_dir.join(requested_path),
        )?;
        push_explain_relative_candidate(
            &mut candidates,
            &worktree_root,
            &worktree_root.join(requested_path),
        )?;
    }

    candidates.sort();
    candidates.dedup();

    if candidates.is_empty() {
        return Err(ExplainPathError::PathOutsideRepository);
    }

    choose_explain_candidate(&scanned_paths, &candidates)
}

fn push_explain_relative_candidate(
    candidates: &mut Vec<String>,
    worktree_root: &Path,
    candidate: &Path,
) -> Result<(), ExplainPathError> {
    let candidate = lexically_normalize(candidate);
    let Ok(relative) = candidate.strip_prefix(worktree_root) else {
        return Ok(());
    };
    let relative = portable_relative_path(relative)?;

    if relative.is_empty() {
        return Err(ExplainPathError::EmptyPath);
    }

    candidates.push(relative);
    Ok(())
}

fn choose_explain_candidate(
    scanned_paths: &BTreeSet<&str>,
    candidates: &[String],
) -> Result<String, ExplainPathError> {
    let scanned_matches = candidates
        .iter()
        .filter(|candidate| scanned_paths.contains(candidate.as_str()))
        .collect::<Vec<_>>();

    match scanned_matches.as_slice() {
        [candidate] => Ok((*candidate).clone()),
        [first, second, ..] => Err(ExplainPathError::AmbiguousPath {
            first: (*first).clone(),
            second: (*second).clone(),
        }),
        [] => Err(ExplainPathError::NotCurrentFile),
    }
}

fn portable_relative_path(path: &Path) -> Result<String, ExplainPathError> {
    let mut parts = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or(ExplainPathError::UnsupportedPathEncoding)?;
                parts.push(part.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(ExplainPathError::PathOutsideRepository);
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

fn write_explain_git_error(
    f: &mut fmt::Formatter<'_>,
    source: &git::GitHistoryError,
) -> fmt::Result {
    match source {
        git::GitHistoryError::NotRepository { .. } => write!(
            f,
            "path is not a readable Git worktree; run explain from inside a repository with local history"
        ),
        git::GitHistoryError::OpenRepository { .. } => write!(
            f,
            "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
        ),
        git::GitHistoryError::MissingHead { .. } => write!(
            f,
            "Git repository does not have a commit at HEAD; create an initial commit before explaining hotspot scores"
        ),
        git::GitHistoryError::ShallowRepository { .. } => write!(
            f,
            "Git repository has shallow history; fetch complete local history before running explain so metrics are not based on incomplete commits"
        ),
        git::GitHistoryError::BareRepository { .. } => write!(
            f,
            "Git repository has no worktree; hotspot score explanation requires a local worktree"
        ),
        git::GitHistoryError::HeadNotCommit { source, .. } => {
            write!(f, "Git HEAD does not resolve to a commit: {source}")
        }
        git::GitHistoryError::Git { context, source } => {
            write!(f, "failed to traverse Git history while {context}: {source}")
        }
        git::GitHistoryError::UnsupportedAuthorIdentity { commit_id } => write!(
            f,
            "commit {commit_id} has an author name or email that is not valid UTF-8"
        ),
        git::GitHistoryError::UnsupportedPathEncoding { commit_id } => write!(
            f,
            "commit {commit_id} changed a path that is not valid UTF-8"
        ),
    }
}

fn write_explain_persistence_error(
    f: &mut fmt::Formatter<'_>,
    action: &str,
    source: &storage::index::IndexError,
) -> fmt::Result {
    write_persistence_error(f, action, source, "explain")
}

fn write_hotspots_git_error(
    f: &mut fmt::Formatter<'_>,
    source: &git::GitHistoryError,
) -> fmt::Result {
    match source {
        git::GitHistoryError::NotRepository { .. } => write!(
            f,
            "path is not a readable Git worktree; run hotspots from inside a repository with local history"
        ),
        git::GitHistoryError::OpenRepository { .. } => write!(
            f,
            "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
        ),
        git::GitHistoryError::MissingHead { .. } => write!(
            f,
            "Git repository does not have a commit at HEAD; create an initial commit before analyzing hotspots"
        ),
        git::GitHistoryError::ShallowRepository { .. } => write!(
            f,
            "Git repository has shallow history; fetch complete local history before running hotspots so metrics are not based on incomplete commits"
        ),
        git::GitHistoryError::BareRepository { .. } => write!(
            f,
            "Git repository has no worktree; hotspot analysis requires a local worktree"
        ),
        git::GitHistoryError::HeadNotCommit { source, .. } => {
            write!(f, "Git HEAD does not resolve to a commit: {source}")
        }
        git::GitHistoryError::Git { context, source } => {
            write!(f, "failed to traverse Git history while {context}: {source}")
        }
        git::GitHistoryError::UnsupportedAuthorIdentity { commit_id } => write!(
            f,
            "commit {commit_id} has an author name or email that is not valid UTF-8"
        ),
        git::GitHistoryError::UnsupportedPathEncoding { commit_id } => write!(
            f,
            "commit {commit_id} changed a path that is not valid UTF-8"
        ),
    }
}

fn write_diff_git_error(f: &mut fmt::Formatter<'_>, source: &git::GitHistoryError) -> fmt::Result {
    match source {
        git::GitHistoryError::NotRepository { .. } => write!(
            f,
            "path is not a readable Git worktree; run diff from inside a repository with local history"
        ),
        git::GitHistoryError::OpenRepository { .. } => write!(
            f,
            "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
        ),
        git::GitHistoryError::MissingHead { .. } => write!(
            f,
            "Git repository does not have a commit at HEAD; create an initial commit before analyzing a diff"
        ),
        git::GitHistoryError::ShallowRepository { .. } => write!(
            f,
            "Git repository has shallow history; fetch complete local history before running diff so metrics are not based on incomplete commits"
        ),
        git::GitHistoryError::BareRepository { .. } => write!(
            f,
            "Git repository has no worktree; diff analysis requires a local worktree"
        ),
        git::GitHistoryError::HeadNotCommit { source, .. } => {
            write!(f, "Git HEAD does not resolve to a commit: {source}")
        }
        git::GitHistoryError::Git { context, source } => {
            write!(f, "failed to traverse Git history while {context}: {source}")
        }
        git::GitHistoryError::UnsupportedAuthorIdentity { commit_id } => write!(
            f,
            "commit {commit_id} has an author name or email that is not valid UTF-8"
        ),
        git::GitHistoryError::UnsupportedPathEncoding { commit_id } => write!(
            f,
            "commit {commit_id} changed a path that is not valid UTF-8"
        ),
    }
}

fn write_hotspots_persistence_error(
    f: &mut fmt::Formatter<'_>,
    action: &str,
    source: &storage::index::IndexError,
) -> fmt::Result {
    write_persistence_error(f, action, source, "hotspots")
}

fn write_context_persistence_error(
    f: &mut fmt::Formatter<'_>,
    action: &str,
    source: &storage::index::IndexError,
) -> fmt::Result {
    write_persistence_error(f, action, source, "context")
}

fn write_persistence_error(
    f: &mut fmt::Formatter<'_>,
    action: &str,
    source: &storage::index::IndexError,
    rerun_command: &str,
) -> fmt::Result {
    write!(
        f,
        "failed to {action} in local Hotpath index (.hotpath/index.db): "
    )?;

    match source {
        storage::index::IndexError::CreateIndexDir { .. } => write!(
            f,
            "could not create .hotpath; ensure the repository directory is writable"
        ),
        storage::index::IndexError::AccessIndex { .. } => {
            write!(f, "could not access .hotpath; ensure it is readable")
        }
        storage::index::IndexError::OpenDatabase { .. } => write!(
            f,
            "could not open .hotpath/index.db; ensure it is readable or remove it to rebuild"
        ),
        storage::index::IndexError::CorruptDatabase { .. } => write!(
            f,
            "the index is unreadable or corrupt; remove .hotpath/index.db and rerun {rerun_command}"
        ),
        storage::index::IndexError::CorruptMetadata { .. } => write!(
            f,
            "the index schema metadata is invalid; remove .hotpath/index.db and rerun {rerun_command}"
        ),
        storage::index::IndexError::UnsafeIndexDir { .. } => write!(
            f,
            "refusing to use unsafe .hotpath directory; replace it with a regular directory"
        ),
        storage::index::IndexError::IncompatibleFutureSchema {
            found_version,
            supported_version,
            ..
        } => write!(
            f,
            "index schema version {found_version} is newer than supported version {supported_version}; use a newer hotpath binary or remove .hotpath/index.db to rebuild"
        ),
        storage::index::IndexError::Migration {
            from_version,
            to_version,
            ..
        } => write!(
            f,
            "could not migrate index schema from version {from_version} to {to_version}; remove .hotpath/index.db to rebuild if the index can be discarded"
        ),
        storage::index::IndexError::PersistScan { .. }
        | storage::index::IndexError::PersistGitAnalysis { .. }
        | storage::index::IndexError::PersistSymbols { .. }
        | storage::index::IndexError::PersistHotspots { .. } => write!(
            f,
            "could not update .hotpath/index.db; ensure the index is writable"
        ),
        storage::index::IndexError::ReadIndex { .. } => write!(
            f,
            "could not read .hotpath/index.db; inspect the index with `hotpath doctor`"
        ),
        storage::index::IndexError::InvalidScanData { message, .. }
        | storage::index::IndexError::InvalidGitAnalysisData { message, .. }
        | storage::index::IndexError::InvalidSymbolData { message, .. }
        | storage::index::IndexError::InvalidHotspotData { message, .. } => {
            write!(f, "{message}")
        }
    }
}

fn is_internal_entry(root: &Path, entry: &DirEntry) -> bool {
    match entry.file_name().to_str() {
        Some(".git") => true,
        Some(".hotpath") => entry.path() == root.join(".hotpath"),
        _ => false,
    }
}

fn is_walked_file(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|file_type| {
        file_type.is_file()
            || (file_type.is_symlink()
                && fs::metadata(entry.path())
                    .map(|metadata| metadata.is_file())
                    .unwrap_or(true))
    })
}

fn scan_warning(code: &'static str, path: Option<String>, message: String) -> ScanWarning {
    ScanWarning {
        code,
        path,
        message,
    }
}

fn scan_warning_from_walk_error(root: &Path, error: &IgnoreError) -> ScanWarning {
    let code = if error.is_io() {
        "walk_io_error"
    } else {
        "walk_error"
    };

    scan_warning(
        code,
        ignore_error_path(error).and_then(|path| normalized_warning_path(root, path)),
        format!(
            "failed while walking repository entry: {}",
            ignore_error_message(error)
        ),
    )
}

fn scan_warning_from_entry_error(
    root: &Path,
    entry: &DirEntry,
    error: &IgnoreError,
) -> ScanWarning {
    scan_warning(
        "ignore_parse_error",
        ignore_error_path(error)
            .and_then(|path| normalized_warning_path(root, path))
            .or_else(|| normalized_warning_path(root, entry.path())),
        format!(
            "failed to apply ignore rules: {}",
            ignore_error_message(error)
        ),
    )
}

fn ignore_error_path(error: &IgnoreError) -> Option<&Path> {
    match error {
        IgnoreError::Partial(errors) => errors.iter().find_map(ignore_error_path),
        IgnoreError::WithLineNumber { err, .. } | IgnoreError::WithDepth { err, .. } => {
            ignore_error_path(err)
        }
        IgnoreError::WithPath { path, .. } => Some(path),
        IgnoreError::Loop { child, .. } => Some(child),
        IgnoreError::Io(_)
        | IgnoreError::Glob { .. }
        | IgnoreError::UnrecognizedFileType(_)
        | IgnoreError::InvalidDefinition => None,
    }
}

fn ignore_error_message(error: &IgnoreError) -> String {
    match error {
        IgnoreError::Partial(errors) => match errors.as_slice() {
            [] => "unknown partial error".to_owned(),
            [error] => ignore_error_message(error),
            errors => format!("multiple errors occurred ({} errors)", errors.len()),
        },
        IgnoreError::WithLineNumber { line, err } => {
            format!("line {line}: {}", ignore_error_message(err))
        }
        IgnoreError::WithPath { err, .. } | IgnoreError::WithDepth { err, .. } => {
            ignore_error_message(err)
        }
        IgnoreError::Loop { .. } => "filesystem loop detected".to_owned(),
        IgnoreError::Io(error) => io_error_message(error),
        IgnoreError::Glob {
            glob: Some(glob),
            err,
        } => {
            format!("error parsing glob '{glob}': {err}")
        }
        IgnoreError::Glob { glob: _, err } => err.to_owned(),
        IgnoreError::UnrecognizedFileType(file_type) => {
            format!("unrecognized file type: {file_type}")
        }
        IgnoreError::InvalidDefinition => {
            "invalid file type definition; expected type:glob".to_owned()
        }
    }
}

fn io_error_message(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("{:?} (os error {code})", error.kind()),
        None => format!("{:?}", error.kind()),
    }
}

fn normalized_warning_path(root: &Path, path: &Path) -> Option<String> {
    normalized_relative_path(root, path).ok().and_then(|path| {
        if path.value.is_empty() {
            None
        } else {
            Some(path.value)
        }
    })
}

fn classify_file(root: &Path, path: &Path) -> Result<FileRecord, ScanError> {
    let relative_path = normalized_relative_path(root, path)?;
    let mut record = FileRecord {
        byte_size: None,
        extension: file_extension(&relative_path.value),
        language: language_guess(&relative_path.value),
        line_count: None,
        is_vendor: is_vendor_path(&relative_path.value),
        is_generated: is_generated_path(&relative_path.value),
        content: ContentKind::Unknown,
        is_symlink: fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false),
        classification: "implemented",
        warnings: Vec::new(),
        path: relative_path.value,
    };

    if relative_path.used_replacement {
        record.warnings.push(file_warning(
            "unsupported_path_encoding",
            "file path is not valid UTF-8; replacement characters were used in portable output"
                .to_owned(),
        ));
    }

    if record.is_symlink {
        let target = match fs::canonicalize(path) {
            Ok(target) => target,
            Err(source) => {
                record.warnings.push(file_warning(
                    "symlink_target_unreadable",
                    format!("failed to canonicalize symlink target: {source}"),
                ));
                return Ok(record);
            }
        };

        if !target.starts_with(root) {
            record.warnings.push(file_warning(
                "symlink_target_outside_root",
                "symlink target is outside the scan root".to_owned(),
            ));
            return Ok(record);
        }
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) => {
            record.warnings.push(file_warning(
                "metadata_failed",
                format!("failed to read file metadata: {source}"),
            ));
            return Ok(record);
        }
    };

    record.byte_size = Some(metadata.len());

    classify_content(path, &mut record);

    Ok(record)
}

fn classify_content(path: &Path, record: &mut FileRecord) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) => {
            record.warnings.push(file_warning(
                "read_failed",
                format!("failed to open file contents: {source}"),
            ));
            return;
        }
    };

    let mut bytes = Vec::new();
    let mut bounded = file.take(MAX_TEXT_READ_BYTES.saturating_add(1));
    if let Err(source) = bounded.read_to_end(&mut bytes) {
        record.warnings.push(file_warning(
            "read_failed",
            format!("failed to read file contents: {source}"),
        ));
        return;
    }

    if bytes.contains(&0) {
        record.content = ContentKind::Binary;
        return;
    }

    let exceeded_read_limit = bytes.len() as u64 > MAX_TEXT_READ_BYTES;
    if exceeded_read_limit {
        bytes.truncate(MAX_TEXT_READ_BYTES as usize);
    }

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            record.warnings.push(file_warning(
                "unsupported_encoding",
                "file contents are not valid UTF-8".to_owned(),
            ));
            return;
        }
    };

    record.content = ContentKind::Text;

    if exceeded_read_limit {
        record.warnings.push(file_warning(
            "line_count_skipped",
            format!("file is larger than the safe text read limit of {MAX_TEXT_READ_BYTES} bytes"),
        ));
        return;
    }

    record.line_count = Some(count_lines(&text));
}

fn file_warning(code: &'static str, message: String) -> FileWarning {
    FileWarning { code, message }
}

fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
}

fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

fn language_guess(path: &str) -> Option<&'static str> {
    let extension = file_extension(path);
    let file_name = Path::new(path)
        .file_name()
        .and_then(|file_name| file_name.to_str())?;

    match file_name {
        "Dockerfile" | "Containerfile" => return Some("Dockerfile"),
        "Makefile" => return Some("Makefile"),
        _ => {}
    }

    match extension.as_deref()? {
        "bash" | "sh" | "zsh" => Some("Shell"),
        "c" => Some("C"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some("C++"),
        "cs" => Some("C#"),
        "css" => Some("CSS"),
        "go" => Some("Go"),
        "h" => Some("C/C++ Header"),
        "htm" | "html" => Some("HTML"),
        "java" => Some("Java"),
        "js" | "mjs" | "cjs" => Some("JavaScript"),
        "json" => Some("JSON"),
        "jsx" => Some("JavaScript JSX"),
        "kt" | "kts" => Some("Kotlin"),
        "md" | "markdown" => Some("Markdown"),
        "php" => Some("PHP"),
        "proto" => Some("Protocol Buffers"),
        "ps1" => Some("PowerShell"),
        "py" => Some("Python"),
        "rb" => Some("Ruby"),
        "rs" => Some("Rust"),
        "scala" => Some("Scala"),
        "scss" => Some("Sass"),
        "sql" => Some("SQL"),
        "swift" => Some("Swift"),
        "toml" => Some("TOML"),
        "ts" => Some("TypeScript"),
        "tsx" => Some("TypeScript JSX"),
        "xml" => Some("XML"),
        "yaml" | "yml" => Some("YAML"),
        _ => None,
    }
}

fn is_vendor_path(path: &str) -> bool {
    normalized_components(path).any(|component| {
        matches_case_insensitive(
            component,
            &[
                "node_modules",
                "vendor",
                "third_party",
                "third-party",
                "external",
            ],
        )
    })
}

fn is_generated_path(path: &str) -> bool {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    normalized_components(path).any(|component| {
        matches_case_insensitive(component, &["generated", "gen", "codegen", "dist", "build"])
    }) || file_name.contains(".generated.")
        || file_name.contains(".gen.")
        || file_name.ends_with(".pb.go")
        || file_name.ends_with(".pb.rs")
        || file_name.ends_with(".g.cs")
}

fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn normalized_components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter(|component| !component.is_empty())
        .map(|component| component.trim())
}

fn render_summary(scan: &ScanReport) -> String {
    let scan_summary = scan.summary();

    let mut summary = format!(
        "Hotpath scan summary\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  text {}, binary {}, unknown {}\n{:<SUMMARY_LABEL_WIDTH$}  generated {}, vendor {}",
        "total files",
        scan_summary.total_files,
        "total bytes",
        scan_summary.total_bytes,
        "content",
        scan_summary.content.text_files,
        scan_summary.content.binary_files,
        scan_summary.content.unknown_files,
        "flags",
        scan_summary.flags.generated_files,
        scan_summary.flags.vendor_files
    );

    if scan_summary.warnings.total_warnings > 0 {
        if scan_summary.warnings.scan_warnings > 0 {
            summary.push_str(&format!(
                "\n{:<SUMMARY_LABEL_WIDTH$}  {} (scan {}, unreadable {}, skipped {})",
                "warnings",
                scan_summary.warnings.total_warnings,
                scan_summary.warnings.scan_warnings,
                scan_summary.warnings.unreadable_warnings,
                scan_summary.warnings.skipped_warnings
            ));
        } else {
            summary.push_str(&format!(
                "\n{:<SUMMARY_LABEL_WIDTH$}  {} (unreadable {}, skipped {})",
                "warnings",
                scan_summary.warnings.total_warnings,
                scan_summary.warnings.unreadable_warnings,
                scan_summary.warnings.skipped_warnings
            ));
        }
    }

    summary.push_str("\nlanguages");

    if scan_summary.languages.is_empty() {
        summary.push_str("\n  none");
    } else {
        let language_width = scan_summary
            .languages
            .keys()
            .map(|language| language.len())
            .max()
            .unwrap_or(0);

        for (language, count) in scan_summary.languages {
            summary.push_str(&format!("\n  {language:<language_width$}  {count}"));
        }
    }

    summary
}

fn render_parse_summary(report: &ParseReport) -> String {
    let parse_summary = report.summary();

    let mut summary = format!(
        "Hotpath parse summary\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}\n{:<SUMMARY_LABEL_WIDTH$}  {}",
        "total files",
        parse_summary.total_files,
        "candidates",
        parse_summary.candidate_files,
        "parsed",
        parse_summary.parsed_files,
        "pending",
        parse_summary.pending_files,
        "skipped",
        parse_summary.skipped_files,
        "symbols",
        parse_summary.symbol_count,
        "imports",
        parse_summary.import_count
    );

    if parse_summary.warning_count > 0 {
        summary.push_str(&format!(
            "\n{:<SUMMARY_LABEL_WIDTH$}  {}",
            "warnings", parse_summary.warning_count
        ));
    }

    summary
}

fn is_unreadable_warning(code: &str) -> bool {
    matches!(
        code,
        "metadata_failed" | "read_failed" | "symlink_target_unreadable" | "walk_io_error"
    )
}

fn is_skipped_warning(code: &str) -> bool {
    matches!(
        code,
        "line_count_skipped"
            | "symlink_target_outside_root"
            | "unsupported_file_type"
            | "walk_error"
            | "walk_io_error"
    )
}

fn normalized_relative_path(root: &Path, path: &Path) -> Result<NormalizedPath, ScanError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ScanError::RelativePath {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
        })?;
    let mut used_replacement = false;
    let parts = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => {
                let text = part.to_string_lossy();
                used_replacement |= text.contains('\u{FFFD}');
                Some(text.into_owned())
            }
            Component::CurDir => None,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                unreachable!("stripped repository-relative paths cannot contain root components")
            }
        })
        .collect::<Vec<_>>();

    Ok(NormalizedPath {
        value: parts.join("/"),
        used_replacement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = env::current_dir()
                .expect("test should have a current directory")
                .join("target")
                .join("test-fixtures")
                .join(format!("{name}-{}", std::process::id()));

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

        fn write_bytes(&self, relative_path: impl AsRef<Path>, contents: &[u8]) {
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

    fn scanned_paths(root: &Path) -> Vec<String> {
        scan_repository(root)
            .expect("fixture scan should succeed")
            .files
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    fn scanned_records(root: &Path) -> Vec<FileRecord> {
        scan_repository(root)
            .expect("fixture scan should succeed")
            .files
    }

    fn scanned_record(root: &Path, path: &str) -> FileRecord {
        scanned_records(root)
            .into_iter()
            .find(|record| record.path == path)
            .unwrap_or_else(|| panic!("expected scan record for {path}"))
    }

    fn json_value(scan: &ScanReport) -> serde_json::Value {
        let json = render_json(scan).expect("json should render");

        serde_json::from_str(&json).expect("json should parse")
    }

    fn parse_json_value(report: &ParseReport) -> serde_json::Value {
        let json = render_parse_json(report).expect("parse json should render");

        serde_json::from_str(&json).expect("parse json should parse")
    }

    fn raw_score_metrics_with_size(
        line_count: Option<u64>,
        byte_size: Option<u64>,
    ) -> scoring::RawScoreMetrics {
        scoring::RawScoreMetrics {
            path: "src/lib.rs".to_owned(),
            byte_size,
            line_count,
            commits_per_file: Some(1),
            total_churn_lines: Some(2),
            recent_churn_lines: Some(3),
            author_count: Some(1),
            dominant_owner_share: Some(1.0),
            co_changed_file_count: Some(0),
        }
    }

    fn git_error(message: &str) -> git2::Error {
        git2::Error::new(
            git2::ErrorCode::GenericError,
            git2::ErrorClass::Repository,
            message,
        )
    }

    fn sqlite_error() -> rusqlite::Error {
        rusqlite::Error::ExecuteReturnedResults
    }

    fn absolute_index_path() -> PathBuf {
        env::current_dir()
            .expect("test should have a current directory")
            .join("target")
            .join("private-repo")
            .join(".hotpath")
            .join("index.db")
    }

    fn assert_sanitized_actionable_hotspots_error(
        error: HotspotsCommandError,
        absolute_path: &Path,
        expected: &str,
    ) {
        let message = error.to_string();

        assert_eq!(message, expected);
        assert!(
            !message.contains(&absolute_path.display().to_string()),
            "hotspots persistence error leaked absolute path: {message}"
        );
    }

    fn assert_sanitized_actionable_explain_error(
        error: ExplainCommandError,
        absolute_path: &Path,
        expected: &str,
    ) {
        let message = error.to_string();

        assert_eq!(message, expected);
        assert!(
            !message.contains(&absolute_path.display().to_string()),
            "explain persistence error leaked absolute path: {message}"
        );
    }

    fn assert_sanitized_actionable_scan_error(
        error: ScanError,
        absolute_path: &Path,
        expected: &str,
    ) {
        let message = error.to_string();

        assert_eq!(message, expected);
        assert!(
            !message.contains(&absolute_path.display().to_string()),
            "scan persistence error leaked absolute path: {message}"
        );
    }

    fn assert_sanitized_actionable_context_error(
        error: ContextCommandError,
        absolute_path: &Path,
        expected: &str,
    ) {
        let message = error.to_string();

        assert_eq!(message, expected);
        assert!(
            !message.contains(&absolute_path.display().to_string()),
            "context persistence error leaked absolute path: {message}"
        );
    }

    fn scan_warning_record(code: &'static str, path: Option<&str>) -> ScanWarning {
        scan_warning(
            code,
            path.map(ToOwned::to_owned),
            "test scan warning".to_owned(),
        )
    }

    fn record(
        path: &str,
        byte_size: Option<u64>,
        language: Option<&'static str>,
        content: ContentKind,
    ) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size,
            extension: file_extension(path),
            language,
            line_count: None,
            is_vendor: false,
            is_generated: false,
            content,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }
    }

    fn context_text(path: &str, byte_size: u64) -> FileRecord {
        record(path, Some(byte_size), None, ContentKind::Text)
    }

    #[cfg(unix)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn symlink_dir(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(original, link)
    }

    #[cfg(unix)]
    fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn symlink_file(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        std::os::windows::fs::symlink_file(original, link)
    }

    fn symlink_setup_should_skip(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) || cfg!(windows) && error.raw_os_error() == Some(1314)
    }

    fn create_dir_symlink_or_skip(
        original: impl AsRef<Path>,
        link: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        match symlink_dir(original, link) {
            Ok(()) => Ok(()),
            Err(error) if symlink_setup_should_skip(&error) => Err(error),
            Err(error) => panic!("unexpected symlink setup error: {error}"),
        }
    }

    fn create_file_symlink_or_skip(
        original: impl AsRef<Path>,
        link: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        match symlink_file(original, link) {
            Ok(()) => Ok(()),
            Err(error) if symlink_setup_should_skip(&error) => Err(error),
            Err(error) => panic!("unexpected symlink setup error: {error}"),
        }
    }

    #[cfg(unix)]
    fn non_utf8_path_component() -> PathBuf {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(vec![0xff]))
    }

    #[cfg(windows)]
    fn non_utf8_path_component() -> PathBuf {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        PathBuf::from(OsString::from_wide(&[0xd800]))
    }

    #[test]
    fn hotspots_persistence_error_messages_are_sanitized_and_actionable() {
        let path = absolute_index_path();
        let cases = vec![
            (
                storage::index::IndexError::CreateIndexDir {
                    path: path.clone(),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "absolute path denied"),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not create .hotpath; ensure the repository directory is writable",
            ),
            (
                storage::index::IndexError::AccessIndex {
                    path: path.clone(),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "absolute path denied"),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not access .hotpath; ensure it is readable",
            ),
            (
                storage::index::IndexError::OpenDatabase {
                    path: path.clone(),
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not open .hotpath/index.db; ensure it is readable or remove it to rebuild",
            ),
            (
                storage::index::IndexError::CorruptDatabase {
                    path: path.clone(),
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index is unreadable or corrupt; remove .hotpath/index.db and rerun hotspots",
            ),
            (
                storage::index::IndexError::CorruptMetadata {
                    path: path.clone(),
                    message: "schema metadata row is malformed".to_owned(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index schema metadata is invalid; remove .hotpath/index.db and rerun hotspots",
            ),
            (
                storage::index::IndexError::UnsafeIndexDir {
                    path: path.clone(),
                    message: "index path resolves to a symlink".to_owned(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): refusing to use unsafe .hotpath directory; replace it with a regular directory",
            ),
            (
                storage::index::IndexError::IncompatibleFutureSchema {
                    path: path.clone(),
                    found_version: 7,
                    supported_version: 2,
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): index schema version 7 is newer than supported version 2; use a newer hotpath binary or remove .hotpath/index.db to rebuild",
            ),
            (
                storage::index::IndexError::Migration {
                    path: path.clone(),
                    from_version: 1,
                    to_version: 2,
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not migrate index schema from version 1 to 2; remove .hotpath/index.db to rebuild if the index can be discarded",
            ),
            (
                storage::index::IndexError::PersistScan {
                    path: path.clone(),
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not update .hotpath/index.db; ensure the index is writable",
            ),
            (
                storage::index::IndexError::PersistGitAnalysis {
                    path: path.clone(),
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not update .hotpath/index.db; ensure the index is writable",
            ),
            (
                storage::index::IndexError::ReadIndex {
                    path: path.clone(),
                    source: sqlite_error(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): could not read .hotpath/index.db; inspect the index with `hotpath doctor`",
            ),
            (
                storage::index::IndexError::InvalidScanData {
                    path: path.clone(),
                    message: "scan file path must be repository-relative; fix the input and rerun hotspots"
                        .to_owned(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): scan file path must be repository-relative; fix the input and rerun hotspots",
            ),
            (
                storage::index::IndexError::InvalidGitAnalysisData {
                    path: path.clone(),
                    message:
                        "Git analysis path must be repository-relative; fix the input and rerun hotspots"
                            .to_owned(),
                },
                "failed to persist scan results in local Hotpath index (.hotpath/index.db): Git analysis path must be repository-relative; fix the input and rerun hotspots",
            ),
        ];

        for (source, expected) in cases {
            assert_sanitized_actionable_hotspots_error(
                HotspotsCommandError::PersistScan(source),
                &path,
                expected,
            );
        }
    }

    #[test]
    fn hotspots_git_analysis_persistence_error_uses_git_analysis_action() {
        let path = absolute_index_path();

        assert_sanitized_actionable_hotspots_error(
            HotspotsCommandError::PersistGitAnalysis(
                storage::index::IndexError::PersistGitAnalysis {
                    path: path.clone(),
                    source: sqlite_error(),
                },
            ),
            &path,
            "failed to persist Git analysis in local Hotpath index (.hotpath/index.db): could not update .hotpath/index.db; ensure the index is writable",
        );
    }

    #[test]
    fn parse_symbol_persistence_error_uses_symbol_action() {
        let path = absolute_index_path();

        assert_sanitized_actionable_scan_error(
            ScanError::PersistSymbols(storage::index::IndexError::PersistSymbols {
                path: path.clone(),
                source: sqlite_error(),
            }),
            &path,
            "failed to persist parser symbols in local Hotpath index (.hotpath/index.db): could not update .hotpath/index.db; ensure the index is writable",
        );
        assert_sanitized_actionable_scan_error(
            ScanError::PersistSymbols(storage::index::IndexError::InvalidSymbolData {
                path: path.clone(),
                message: "symbol path must be repository-relative; fix the input and rerun parse"
                    .to_owned(),
            }),
            &path,
            "failed to persist parser symbols in local Hotpath index (.hotpath/index.db): symbol path must be repository-relative; fix the input and rerun parse",
        );
    }

    #[test]
    fn context_persistence_error_messages_are_sanitized_and_actionable() {
        let path = absolute_index_path();

        assert_sanitized_actionable_context_error(
            ContextCommandError::PersistScan(storage::index::IndexError::CorruptDatabase {
                path: path.clone(),
                source: sqlite_error(),
            }),
            &path,
            "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index is unreadable or corrupt; remove .hotpath/index.db and rerun context",
        );
        assert_sanitized_actionable_context_error(
            ContextCommandError::PersistScan(storage::index::IndexError::CorruptMetadata {
                path: path.clone(),
                message: "schema metadata row is malformed".to_owned(),
            }),
            &path,
            "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index schema metadata is invalid; remove .hotpath/index.db and rerun context",
        );
    }

    #[test]
    fn context_rendering_reports_summary_filters_budget_and_groups() {
        let mut generated = context_text("dist/client.js", 8);
        generated.is_generated = true;
        let mut vendor = context_text("vendor/lib.rs", 12);
        vendor.is_vendor = true;
        let report = context::estimate_context(
            &[context_text("src/lib.rs", 12), generated, vendor],
            ContextOptions {
                exclude_generated: true,
                exclude_vendor: true,
                budget_tokens: Some(5),
            },
        );

        assert_eq!(
            render_context(&report),
            "Hotpath context budget\nscope: current scanned UTF-8 text files from the working directory\nformula: estimated tokens = ceil(byte_size / 4) for UTF-8 text files\n\ntotal estimated tokens  3\nincluded files          1\nskipped files           2\nincluded bytes          12\noutput filters: exclude generated files, exclude vendor files\nbudget: within budget by 2 tokens (budget 5, estimated 3)\n\ngroups\n  group path  estimated tokens  bytes  files\n  src  3  12  1\n\ncalculation notes\n  - Offline deterministic approximation; no source text leaves the local machine.\n  - Tokenizer-specific counts vary by model and language, so treat this as planning guidance."
        );
    }

    #[test]
    fn context_rendering_reports_over_budget_and_empty_groups() {
        let report = context::estimate_context(
            &[record("image.png", Some(10), None, ContentKind::Binary)],
            ContextOptions {
                exclude_generated: false,
                exclude_vendor: false,
                budget_tokens: Some(1),
            },
        );

        assert!(render_context(&report).contains(
            "budget: within budget by 1 tokens (budget 1, estimated 0)\n\ngroups\n  group path  estimated tokens  bytes  files\n  none"
        ));

        let report = context::estimate_context(
            &[context_text("src/lib.rs", 8)],
            ContextOptions {
                exclude_generated: false,
                exclude_vendor: false,
                budget_tokens: Some(1),
            },
        );

        assert!(render_context(&report)
            .contains("budget: over budget by 1 tokens (budget 1, estimated 2)"));
    }

    #[test]
    fn context_json_serializes_report_schema_pretty_printed() {
        let report =
            context::estimate_context(&[context_text("src/lib.rs", 4)], ContextOptions::default());
        let json = serde_json::to_string_pretty(&report).expect("context JSON should render");

        assert!(json.starts_with("{\n  \"schema_version\": \"hotpath.context.v1\","));
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("context JSON should parse");
        assert_eq!(value["schema_version"], CONTEXT_SCHEMA_VERSION);
        assert_eq!(value["summary"]["estimated_tokens"], 1);
        assert_eq!(value["groups"][0]["path"], "src");
    }

    #[test]
    fn explain_path_error_messages_cover_empty_and_unsupported_encoding() {
        let fixture = Fixture::new("explain-empty-path");
        fixture.write("src/lib.rs", "pub fn lib() {}\n");
        let files = scanned_records(&fixture.path);

        let empty_error =
            normalize_explain_file_path(&fixture.path, &fixture.path, Path::new(""), &files)
                .expect_err("empty explain path should fail before matching scan records");

        assert_eq!(empty_error, ExplainPathError::EmptyPath);
        assert_eq!(
            empty_error.to_string(),
            "explain requires a non-empty file path"
        );

        let encoding_error = portable_relative_path(&non_utf8_path_component())
            .expect_err("non-UTF-8 path component should fail");

        assert_eq!(encoding_error, ExplainPathError::UnsupportedPathEncoding);
        assert_eq!(
            encoding_error.to_string(),
            "requested path is not valid UTF-8 and cannot be rendered as a portable repository-relative path"
        );
    }

    #[test]
    fn explain_persistence_error_messages_are_sanitized_and_actionable() {
        let path = absolute_index_path();

        assert_sanitized_actionable_explain_error(
            ExplainCommandError::PersistScan(storage::index::IndexError::CorruptDatabase {
                path: path.clone(),
                source: sqlite_error(),
            }),
            &path,
            "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index is unreadable or corrupt; remove .hotpath/index.db and rerun explain",
        );
        assert_sanitized_actionable_explain_error(
            ExplainCommandError::PersistScan(storage::index::IndexError::CorruptMetadata {
                path: path.clone(),
                message: "schema metadata row is malformed".to_owned(),
            }),
            &path,
            "failed to persist scan results in local Hotpath index (.hotpath/index.db): the index schema metadata is invalid; remove .hotpath/index.db and rerun explain",
        );
        assert_sanitized_actionable_explain_error(
            ExplainCommandError::PersistGitAnalysis(
                storage::index::IndexError::PersistGitAnalysis {
                    path: path.clone(),
                    source: sqlite_error(),
                },
            ),
            &path,
            "failed to persist Git analysis in local Hotpath index (.hotpath/index.db): could not update .hotpath/index.db; ensure the index is writable",
        );
    }

    #[test]
    fn explain_git_error_messages_cover_non_cli_history_failures() {
        let cases = [
            (
                git::GitHistoryError::OpenRepository {
                    path: PathBuf::from("repo"),
                    source: git_error("config is unreadable"),
                },
                "failed to open Git repository from the current worktree; ensure local Git metadata is readable",
            ),
            (
                git::GitHistoryError::BareRepository {
                    path: PathBuf::from("repo.git"),
                },
                "Git repository has no worktree; hotspot score explanation requires a local worktree",
            ),
            (
                git::GitHistoryError::HeadNotCommit {
                    path: PathBuf::from("repo"),
                    source: git_error("object is a tree"),
                },
                "Git HEAD does not resolve to a commit: object is a tree",
            ),
            (
                git::GitHistoryError::Git {
                    context: "walking reachable commits",
                    source: git_error("revwalk failed"),
                },
                "failed to traverse Git history while walking reachable commits: revwalk failed",
            ),
            (
                git::GitHistoryError::UnsupportedAuthorIdentity {
                    commit_id: "abc123".to_owned(),
                },
                "commit abc123 has an author name or email that is not valid UTF-8",
            ),
            (
                git::GitHistoryError::UnsupportedPathEncoding {
                    commit_id: "def456".to_owned(),
                },
                "commit def456 changed a path that is not valid UTF-8",
            ),
        ];

        for (source, expected) in cases {
            assert!(
                ExplainCommandError::from(source)
                    .to_string()
                    .starts_with(expected),
                "expected explain Git error to start with '{expected}'"
            );
        }
    }

    #[test]
    fn render_size_summary_reports_unavailable_when_line_count_and_byte_size_are_missing() {
        let raw_metrics = raw_score_metrics_with_size(None, None);

        assert_eq!(render_size_summary(&raw_metrics), "size unavailable");
    }

    #[test]
    fn hotspots_git_error_messages_cover_non_cli_history_failures() {
        let cases = [
            (
                git::GitHistoryError::OpenRepository {
                    path: PathBuf::from("repo"),
                    source: git_error("config is unreadable"),
                },
                "failed to open Git repository from the current worktree; ensure local Git metadata is readable",
            ),
            (
                git::GitHistoryError::BareRepository {
                    path: PathBuf::from("repo.git"),
                },
                "Git repository has no worktree; hotspot analysis requires a local worktree",
            ),
            (
                git::GitHistoryError::HeadNotCommit {
                    path: PathBuf::from("repo"),
                    source: git_error("object is a tree"),
                },
                "Git HEAD does not resolve to a commit: object is a tree",
            ),
            (
                git::GitHistoryError::Git {
                    context: "walking reachable commits",
                    source: git_error("revwalk failed"),
                },
                "failed to traverse Git history while walking reachable commits: revwalk failed",
            ),
            (
                git::GitHistoryError::UnsupportedAuthorIdentity {
                    commit_id: "abc123".to_owned(),
                },
                "commit abc123 has an author name or email that is not valid UTF-8",
            ),
            (
                git::GitHistoryError::UnsupportedPathEncoding {
                    commit_id: "def456".to_owned(),
                },
                "commit def456 changed a path that is not valid UTF-8",
            ),
        ];

        for (source, expected) in cases {
            assert!(
                HotspotsCommandError::from(source)
                    .to_string()
                    .starts_with(expected),
                "expected hotspots Git error to start with '{expected}'"
            );
        }
    }

    #[test]
    fn scan_records_are_sorted_by_normalized_relative_path() {
        let fixture = Fixture::new("deterministic-ordering");
        fixture.write("z.rs", "");
        fixture.write(Path::new("nested").join("m.rs"), "");
        fixture.write("a.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec!["a.rs", "nested/m.rs", "z.rs"]
        );
    }

    #[test]
    fn scan_rejects_missing_roots_with_actionable_error() {
        let fixture = Fixture::new("missing-root");
        let missing = fixture.path.join("missing");
        let error = scan_repository(&missing).expect_err("missing root should fail");

        match error {
            ScanError::Root { path, source } => {
                assert_eq!(path, missing);
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            error => panic!("expected root access error, got {error:?}"),
        }
    }

    #[test]
    fn scan_rejects_non_directory_roots() {
        let fixture = Fixture::new("file-root");
        fixture.write("not-a-directory.rs", "");
        let root_file = fixture.path.join("not-a-directory.rs");
        let error = scan_repository(&root_file).expect_err("file root should fail");

        match error {
            ScanError::RootNotDirectory { path } => assert_eq!(path, root_file),
            error => panic!("expected non-directory root error, got {error:?}"),
        }
    }

    #[test]
    fn scan_respects_gitignore_patterns() {
        let fixture = Fixture::new("gitignore");
        fixture.write(".gitignore", "ignored/\n*.log\n");
        fixture.write("ignored/file.rs", "");
        fixture.write("keep.rs", "");
        fixture.write("notes.log", "");

        assert_eq!(scanned_paths(&fixture.path), vec![".gitignore", "keep.rs"]);
    }

    #[test]
    fn ignore_parse_errors_are_scan_warnings_without_aborting() {
        let fixture = Fixture::new("bad-gitignore");
        fixture.write(".gitignore", "{foo\n");
        fixture.write("keep.rs", "");

        let report = scan_repository(&fixture.path).expect("scan should return partial results");
        let warning = report
            .warnings
            .iter()
            .find(|warning| warning.code == "ignore_parse_error" || warning.code == "walk_error")
            .expect("scan should report malformed ignore file");

        assert!(report.files.iter().any(|file| file.path == "keep.rs"));
        assert!(warning.message.contains("glob") || warning.message.contains("ignore"));
        assert!(warning
            .path
            .as_deref()
            .is_none_or(|path| { !path.contains(&fixture.path.display().to_string()) }));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directories_are_scan_warnings_without_aborting() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("unreadable-directory");
        fixture.write("keep.rs", "");
        fixture.write("blocked/secret.rs", "");
        let blocked = fixture.path.join("blocked");
        let original_permissions = fs::metadata(&blocked)
            .expect("blocked directory metadata should be readable")
            .permissions();
        let mut denied_permissions = original_permissions.clone();
        denied_permissions.set_mode(0o0);
        fs::set_permissions(&blocked, denied_permissions)
            .expect("blocked directory permissions should be changed");

        let report = scan_repository(&fixture.path).expect("scan should return partial results");

        fs::set_permissions(&blocked, original_permissions)
            .expect("blocked directory permissions should be restored");

        assert!(report.files.iter().any(|file| file.path == "keep.rs"));
        assert!(!report
            .files
            .iter()
            .any(|file| file.path == "blocked/secret.rs"));
        assert!(report.warnings.iter().any(|warning| {
            warning.code == "walk_io_error" && warning.path.as_deref() == Some("blocked")
        }));
    }

    #[test]
    fn scan_ignores_non_repository_ignore_sources() {
        let fixture = Fixture::new("deterministic-ignore-sources");
        fixture.write(".ignore", "ignored-by-dot-ignore.rs\n");
        fixture.write(".git/info/exclude", "ignored-by-git-exclude.rs\n");
        fixture.write("ignored-by-dot-ignore.rs", "");
        fixture.write("ignored-by-git-exclude.rs", "");

        assert_eq!(
            scanned_paths(&fixture.path),
            vec![
                ".ignore",
                "ignored-by-dot-ignore.rs",
                "ignored-by-git-exclude.rs"
            ]
        );
    }

    #[test]
    fn scan_excludes_git_entries_that_are_files() {
        let fixture = Fixture::new("git-file");
        fixture.write(".git", "gitdir: ../linked-worktree.git\n");
        fixture.write("keep.rs", "");

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn scan_does_not_descend_into_symlinked_directories() {
        let fixture = Fixture::new("symlinked-directory");
        let linked = Fixture::new("symlink-target");
        linked.write("nested/secret.rs", "");
        fixture.write("keep.rs", "");

        if create_dir_symlink_or_skip(&linked.path, fixture.path.join("linked")).is_err() {
            return;
        }

        assert_eq!(scanned_paths(&fixture.path), vec!["keep.rs"]);
    }

    #[test]
    fn binary_files_are_classified_without_line_counts() {
        let fixture = Fixture::new("binary-file");
        fixture.write_bytes("image.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);

        let record = scanned_record(&fixture.path, "image.bin");

        assert_eq!(record.byte_size, Some(8));
        assert_eq!(record.extension, Some("bin".to_owned()));
        assert_eq!(record.content, ContentKind::Binary);
        assert_eq!(record.line_count, None);
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn nul_after_initial_sample_classifies_file_as_binary() {
        let fixture = Fixture::new("delayed-nul");
        let mut contents = vec![b'a'; BINARY_SAMPLE_BYTES + 1];
        contents.push(0);
        fixture.write_bytes("delayed.bin", &contents);

        let record = scanned_record(&fixture.path, "delayed.bin");

        assert_eq!(record.content, ContentKind::Binary);
        assert_eq!(record.line_count, None);
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn text_larger_than_read_limit_skips_line_count() {
        let fixture = Fixture::new("large-text");
        let contents = vec![b'a'; MAX_TEXT_READ_BYTES as usize + 1];
        fixture.write_bytes("large.txt", &contents);

        let record = scanned_record(&fixture.path, "large.txt");

        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "line_count_skipped");
    }

    #[test]
    fn utf8_text_files_record_line_counts_and_sizes() {
        let fixture = Fixture::new("utf8-line-count");
        fixture.write("src/lib.rs", "fn main() {}\n\n// done\n");

        let record = scanned_record(&fixture.path, "src/lib.rs");

        assert_eq!(record.byte_size, Some(22));
        assert_eq!(record.extension, Some("rs".to_owned()));
        assert_eq!(record.language, Some("Rust"));
        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, Some(3));
    }

    #[test]
    fn invalid_utf8_fallback_does_not_panic_or_count_lines() {
        let fixture = Fixture::new("invalid-utf8");
        fixture.write_bytes("bad.txt", &[b'a', 0xff, b'\n']);

        let record = scanned_record(&fixture.path, "bad.txt");

        assert_eq!(record.byte_size, Some(3));
        assert_eq!(record.extension, Some("txt".to_owned()));
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "unsupported_encoding");
    }

    #[test]
    fn vendor_and_generated_paths_are_flagged() {
        let fixture = Fixture::new("vendor-generated");
        fixture.write("node_modules/pkg/index.js", "");
        fixture.write("src/api.generated.ts", "");
        fixture.write("src/handwritten.ts", "");

        let vendor = scanned_record(&fixture.path, "node_modules/pkg/index.js");
        let generated = scanned_record(&fixture.path, "src/api.generated.ts");
        let handwritten = scanned_record(&fixture.path, "src/handwritten.ts");

        assert!(vendor.is_vendor);
        assert!(!vendor.is_generated);
        assert!(generated.is_generated);
        assert!(!generated.is_vendor);
        assert!(!handwritten.is_vendor);
        assert!(!handwritten.is_generated);
    }

    #[test]
    fn vendor_and_generated_component_matching_is_case_insensitive() {
        let fixture = Fixture::new("cased-vendor-generated");
        fixture.write("Node_Modules/pkg/index.js", "");
        fixture.write("Src/CodeGen/api.ts", "");

        let vendor = scanned_record(&fixture.path, "Node_Modules/pkg/index.js");
        let generated = scanned_record(&fixture.path, "Src/CodeGen/api.ts");

        assert!(vendor.is_vendor);
        assert!(generated.is_generated);
    }

    #[test]
    fn extension_and_language_guesses_are_conservative() {
        let fixture = Fixture::new("language-guesses");
        fixture.write("README.md", "");
        fixture.write("Dockerfile", "");
        fixture.write("src/view.tsx", "");
        fixture.write("unknown.hotpath", "");

        let markdown = scanned_record(&fixture.path, "README.md");
        let dockerfile = scanned_record(&fixture.path, "Dockerfile");
        let tsx = scanned_record(&fixture.path, "src/view.tsx");
        let unknown = scanned_record(&fixture.path, "unknown.hotpath");

        assert_eq!(markdown.extension, Some("md".to_owned()));
        assert_eq!(markdown.language, Some("Markdown"));
        assert_eq!(dockerfile.extension, None);
        assert_eq!(dockerfile.language, Some("Dockerfile"));
        assert_eq!(tsx.extension, Some("tsx".to_owned()));
        assert_eq!(tsx.language, Some("TypeScript JSX"));
        assert_eq!(unknown.extension, Some("hotpath".to_owned()));
        assert_eq!(unknown.language, None);
    }

    #[test]
    fn language_guesses_cover_supported_extensions_and_special_file_names() {
        let cases = [
            ("script.bash", Some("Shell")),
            ("script.sh", Some("Shell")),
            ("script.zsh", Some("Shell")),
            ("main.c", Some("C")),
            ("main.cc", Some("C++")),
            ("main.cpp", Some("C++")),
            ("main.cxx", Some("C++")),
            ("include.hpp", Some("C++")),
            ("include.hh", Some("C++")),
            ("include.hxx", Some("C++")),
            ("Program.cs", Some("C#")),
            ("style.css", Some("CSS")),
            ("main.go", Some("Go")),
            ("include.h", Some("C/C++ Header")),
            ("index.htm", Some("HTML")),
            ("index.html", Some("HTML")),
            ("Main.java", Some("Java")),
            ("index.js", Some("JavaScript")),
            ("index.mjs", Some("JavaScript")),
            ("index.cjs", Some("JavaScript")),
            ("data.json", Some("JSON")),
            ("view.jsx", Some("JavaScript JSX")),
            ("Main.kt", Some("Kotlin")),
            ("build.gradle.kts", Some("Kotlin")),
            ("README.markdown", Some("Markdown")),
            ("index.php", Some("PHP")),
            ("schema.proto", Some("Protocol Buffers")),
            ("build.ps1", Some("PowerShell")),
            ("tool.py", Some("Python")),
            ("tool.rb", Some("Ruby")),
            ("lib.rs", Some("Rust")),
            ("Job.scala", Some("Scala")),
            ("style.scss", Some("Sass")),
            ("query.sql", Some("SQL")),
            ("App.swift", Some("Swift")),
            ("Cargo.toml", Some("TOML")),
            ("index.ts", Some("TypeScript")),
            ("index.tsx", Some("TypeScript JSX")),
            ("document.xml", Some("XML")),
            ("config.yaml", Some("YAML")),
            ("config.yml", Some("YAML")),
            ("Dockerfile", Some("Dockerfile")),
            ("Containerfile", Some("Dockerfile")),
            ("Makefile", Some("Makefile")),
            ("unknown.hotpath", None),
        ];

        for (path, expected) in cases {
            assert_eq!(language_guess(path), expected, "language guess for {path}");
        }
    }

    #[test]
    fn path_classification_covers_generated_suffixes_and_vendor_components() {
        for path in [
            "src/schema.pb.go",
            "src/schema.pb.rs",
            "src/messages.g.cs",
            "src/api.gen.ts",
            "generated/client.ts",
            "build/client.ts",
            "dist/client.js",
        ] {
            assert!(is_generated_path(path), "{path} should be generated");
        }

        for path in [
            "third_party/pkg/lib.rs",
            "third-party/pkg/lib.rs",
            "external/pkg/lib.rs",
            "vendor/pkg/lib.rs",
            "node_modules/pkg/index.js",
        ] {
            assert!(is_vendor_path(path), "{path} should be vendor");
        }

        assert!(!is_generated_path("src/generation_notes.rs"));
        assert!(!is_vendor_path("src/vendor_notes.rs"));
    }

    #[test]
    fn symlinked_files_inside_scan_root_are_classified() {
        let fixture = Fixture::new("symlinked-file");
        fixture.write("target.rs", "fn linked() {}\n");

        if create_file_symlink_or_skip(
            fixture.path.join("target.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.language, Some("Rust"));
        assert_eq!(record.content, ContentKind::Text);
        assert_eq!(record.line_count, Some(1));
        assert!(record.warnings.is_empty());
    }

    #[test]
    fn symlinked_files_outside_scan_root_are_recorded_without_content() {
        let fixture = Fixture::new("outside-symlink");
        let target = Fixture::new("outside-symlink-target");
        target.write("target.rs", "fn linked() {}\n");

        if create_file_symlink_or_skip(
            target.path.join("target.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.byte_size, None);
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "symlink_target_outside_root");
    }

    #[test]
    fn unreadable_symlink_targets_are_recorded_without_content() {
        let fixture = Fixture::new("broken-symlink");

        if create_file_symlink_or_skip(
            fixture.path.join("missing.rs"),
            fixture.path.join("linked.rs"),
        )
        .is_err()
        {
            return;
        }

        let record = scanned_record(&fixture.path, "linked.rs");

        assert!(record.is_symlink);
        assert_eq!(record.byte_size, None);
        assert_eq!(record.content, ContentKind::Unknown);
        assert_eq!(record.line_count, None);
        assert_eq!(record.warnings.len(), 1);
        assert_eq!(record.warnings[0].code, "symlink_target_unreadable");
    }

    #[test]
    fn summary_reports_concise_totals() {
        let mut generated = record(
            "dist/app.generated.js",
            Some(30),
            Some("JavaScript"),
            ContentKind::Text,
        );
        generated.is_generated = true;
        let mut vendor = record("vendor/blob.bin", Some(5), None, ContentKind::Binary);
        vendor.is_vendor = true;
        let mut unknown = record("notes.txt", None, None, ContentKind::Unknown);
        unknown.warnings.push(file_warning(
            "read_failed",
            "failed to open file contents: denied".to_owned(),
        ));

        let scan = ScanReport::from_files(vec![
            record("src/lib.rs", Some(10), Some("Rust"), ContentKind::Text),
            generated,
            vendor,
            unknown,
        ]);
        let summary = render_summary(&scan);

        assert_eq!(
            summary,
            "Hotpath scan summary\ntotal files   4\ntotal bytes   45\ncontent       text 2, binary 1, unknown 1\nflags         generated 1, vendor 1\nwarnings      1 (unreadable 1, skipped 0)\nlanguages\n  JavaScript  1\n  Rust        1"
        );
    }

    #[test]
    fn summary_omits_warning_line_when_no_warnings_are_present() {
        let scan = ScanReport::from_files(vec![record(
            "src/lib.rs",
            Some(10),
            Some("Rust"),
            ContentKind::Text,
        )]);

        let summary = render_summary(&scan);

        assert!(!summary.contains("warnings:"));
    }

    #[test]
    fn summary_reports_empty_language_counts_explicitly() {
        let scan = ScanReport::from_files(vec![record(
            "blob.bin",
            Some(10),
            None,
            ContentKind::Binary,
        )]);

        let summary = render_summary(&scan);

        assert_eq!(
            summary,
            "Hotpath scan summary\ntotal files   1\ntotal bytes   10\ncontent       text 0, binary 1, unknown 0\nflags         generated 0, vendor 0\nlanguages\n  none"
        );
    }

    #[test]
    fn summary_reports_skipped_warning_counts() {
        let mut skipped = record("large.txt", Some(10), None, ContentKind::Text);
        skipped.warnings.push(file_warning(
            "line_count_skipped",
            "file is larger than the safe text read limit".to_owned(),
        ));

        let summary = render_summary(&ScanReport::from_files(vec![skipped]));

        assert_eq!(
            summary,
            "Hotpath scan summary\ntotal files   1\ntotal bytes   10\ncontent       text 1, binary 0, unknown 0\nflags         generated 0, vendor 0\nwarnings      1 (unreadable 0, skipped 1)\nlanguages\n  none"
        );
    }

    #[test]
    fn summary_reports_scan_warning_counts() {
        let scan = ScanReport::from_parts(
            vec![scan_warning_record("walk_io_error", Some("blocked"))],
            vec![record(
                "src/lib.rs",
                Some(10),
                Some("Rust"),
                ContentKind::Text,
            )],
        );

        let summary = render_summary(&scan);

        assert_eq!(
            summary,
            "Hotpath scan summary\ntotal files   1\ntotal bytes   10\ncontent       text 1, binary 0, unknown 0\nflags         generated 0, vendor 0\nwarnings      1 (scan 1, unreadable 1, skipped 1)\nlanguages\n  Rust  1"
        );
    }

    #[test]
    fn summary_counts_unsupported_file_type_scan_warning_as_skipped() {
        let scan = ScanReport::from_parts(
            vec![scan_warning_record(
                "unsupported_file_type",
                Some("unknown-entry"),
            )],
            Vec::new(),
        );

        let summary = scan.summary();

        assert_eq!(summary.warnings.total_warnings, 1);
        assert_eq!(summary.warnings.scan_warnings, 1);
        assert_eq!(summary.warnings.unreadable_warnings, 0);
        assert_eq!(summary.warnings.skipped_warnings, 1);
    }

    #[test]
    fn json_reports_schema_version_and_summary_totals() {
        let mut generated = record(
            "dist/app.generated.js",
            Some(30),
            Some("JavaScript"),
            ContentKind::Text,
        );
        generated.is_generated = true;
        let mut vendor = record("vendor/blob.bin", Some(5), None, ContentKind::Binary);
        vendor.is_vendor = true;
        let mut unknown = record("notes.txt", None, None, ContentKind::Unknown);
        unknown.warnings.push(file_warning(
            "read_failed",
            "failed to open file contents: denied".to_owned(),
        ));

        let value = json_value(&ScanReport::from_files(vec![
            record("src/lib.rs", Some(10), Some("Rust"), ContentKind::Text),
            generated,
            vendor,
            unknown,
        ]));

        assert_eq!(value["schema_version"], "hotpath.scan.v1");
        assert_eq!(value["summary"]["total_files"], 4);
        assert_eq!(value["summary"]["total_bytes"], 45);
        assert_eq!(value["summary"]["content"]["text_files"], 2);
        assert_eq!(value["summary"]["content"]["binary_files"], 1);
        assert_eq!(value["summary"]["content"]["unknown_files"], 1);
        assert_eq!(value["summary"]["flags"]["generated_files"], 1);
        assert_eq!(value["summary"]["flags"]["vendor_files"], 1);
        assert_eq!(value["summary"]["warnings"]["total_warnings"], 1);
        assert_eq!(value["summary"]["warnings"]["scan_warnings"], 0);
        assert_eq!(value["summary"]["warnings"]["unreadable_warnings"], 1);
        assert_eq!(value["summary"]["warnings"]["skipped_warnings"], 0);
        assert_eq!(value["summary"]["languages"]["JavaScript"], 1);
        assert_eq!(value["summary"]["languages"]["Rust"], 1);
    }

    #[test]
    fn json_reports_scan_warnings_without_absolute_paths() {
        let scan = ScanReport::from_parts(
            vec![scan_warning_record("walk_io_error", Some("blocked"))],
            Vec::new(),
        );

        let value = json_value(&scan);

        assert_eq!(value["schema_version"], "hotpath.scan.v1");
        assert!(value
            .as_object()
            .expect("scan JSON should be an object")
            .contains_key("warnings"));
        assert!(value["summary"]["warnings"]
            .as_object()
            .expect("summary warnings should be an object")
            .contains_key("scan_warnings"));
        assert_eq!(value["summary"]["warnings"]["total_warnings"], 1);
        assert_eq!(value["summary"]["warnings"]["scan_warnings"], 1);
        assert_eq!(value["summary"]["warnings"]["unreadable_warnings"], 1);
        assert_eq!(value["summary"]["warnings"]["skipped_warnings"], 1);
        assert_eq!(value["warnings"][0]["code"], "walk_io_error");
        assert_eq!(value["warnings"][0]["path"], "blocked");
        assert_eq!(value["warnings"][0]["message"], "test scan warning");
    }

    #[test]
    fn json_preserves_stable_file_order_from_scan() {
        let fixture = Fixture::new("json-file-order");
        fixture.write("z.rs", "");
        fixture.write(Path::new("nested").join("m.rs"), "");
        fixture.write("a.rs", "");

        let report = scan_repository(&fixture.path).expect("fixture scan should succeed");
        let value = json_value(&report);
        let paths = value["files"]
            .as_array()
            .expect("files should be an array")
            .iter()
            .map(|file| file["path"].as_str().expect("path should be a string"))
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["a.rs", "nested/m.rs", "z.rs"]);
    }

    #[test]
    fn json_reports_stable_file_fields() {
        let report = ScanReport::from_files(vec![FileRecord {
            path: "src/lib.rs".to_owned(),
            byte_size: Some(10),
            extension: Some("rs".to_owned()),
            language: Some("Rust"),
            line_count: Some(1),
            is_vendor: false,
            is_generated: false,
            content: ContentKind::Text,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }]);
        let json = render_json(&report).expect("json should render");

        assert_eq!(
            json,
            "{\n  \"schema_version\": \"hotpath.scan.v1\",\n  \"summary\": {\n    \"total_files\": 1,\n    \"total_bytes\": 10,\n    \"content\": {\n      \"text_files\": 1,\n      \"binary_files\": 0,\n      \"unknown_files\": 0\n    },\n    \"flags\": {\n      \"generated_files\": 0,\n      \"vendor_files\": 0\n    },\n    \"warnings\": {\n      \"total_warnings\": 0,\n      \"scan_warnings\": 0,\n      \"unreadable_warnings\": 0,\n      \"skipped_warnings\": 0\n    },\n    \"languages\": {\n      \"Rust\": 1\n    }\n  },\n  \"warnings\": [],\n  \"files\": [\n    {\n      \"path\": \"src/lib.rs\",\n      \"byte_size\": 10,\n      \"extension\": \"rs\",\n      \"language\": \"Rust\",\n      \"line_count\": 1,\n      \"is_vendor\": false,\n      \"is_generated\": false,\n      \"content\": \"text\",\n      \"is_symlink\": false,\n      \"classification\": \"implemented\",\n      \"warnings\": []\n    }\n  ]\n}"
        );
    }

    #[test]
    fn json_reports_stable_warning_fields() {
        let mut unreadable = record("notes.txt", None, None, ContentKind::Unknown);
        unreadable.warnings.push(file_warning(
            "read_failed",
            "failed to open file contents: denied".to_owned(),
        ));

        let value = json_value(&ScanReport::from_files(vec![unreadable]));
        let warning = &value["files"][0]["warnings"][0];

        assert_eq!(warning["code"], "read_failed");
        assert_eq!(warning["message"], "failed to open file contents: denied");
    }

    #[test]
    fn parse_json_reports_stable_fields() {
        let report = parse_scan_report(&ScanReport::from_files(vec![FileRecord {
            path: "src/lib.rs".to_owned(),
            byte_size: Some(10),
            extension: Some("rs".to_owned()),
            language: Some("Rust"),
            line_count: Some(1),
            is_vendor: false,
            is_generated: false,
            content: ContentKind::Text,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }]));
        let json = render_parse_json(&report).expect("parse json should render");

        assert_eq!(
            json,
            "{\n  \"schema_version\": \"hotpath.parse.v1\",\n  \"summary\": {\n    \"total_files\": 1,\n    \"candidate_files\": 1,\n    \"parsed_files\": 0,\n    \"pending_files\": 1,\n    \"skipped_files\": 0,\n    \"symbol_count\": 0,\n    \"import_count\": 0,\n    \"warning_count\": 0\n  },\n  \"warnings\": [],\n  \"files\": [\n    {\n      \"path\": \"src/lib.rs\",\n      \"language\": \"Rust\",\n      \"content\": \"text\",\n      \"status\": \"pending\",\n      \"reason\": \"parser_extraction_pending\",\n      \"symbol_count\": 0,\n      \"import_count\": 0\n    }\n  ],\n  \"symbols\": [],\n  \"imports\": []\n}"
        );
    }

    #[test]
    fn parse_report_marks_unsupported_files_as_skipped() {
        let report = parse_scan_report(&ScanReport::from_files(vec![
            record("README.md", Some(20), Some("Markdown"), ContentKind::Text),
            record("assets/logo.bin", Some(8), None, ContentKind::Binary),
            record("src/lib.rs", Some(10), Some("Rust"), ContentKind::Text),
        ]));
        let value = parse_json_value(&report);
        let files = value["files"].as_array().expect("files should be an array");

        assert_eq!(value["summary"]["total_files"], 3);
        assert_eq!(value["summary"]["candidate_files"], 1);
        assert_eq!(value["summary"]["parsed_files"], 0);
        assert_eq!(value["summary"]["pending_files"], 1);
        assert_eq!(value["summary"]["skipped_files"], 2);
        assert_eq!(value["symbols"], serde_json::Value::Array(Vec::new()));
        assert_eq!(value["imports"], serde_json::Value::Array(Vec::new()));
        assert_eq!(files[0]["path"], "README.md");
        assert_eq!(files[0]["status"], "skipped");
        assert_eq!(files[0]["reason"], "unsupported_language");
        assert_eq!(files[1]["path"], "assets/logo.bin");
        assert_eq!(files[1]["status"], "skipped");
        assert_eq!(files[1]["reason"], "unsupported_content");
        assert_eq!(files[2]["path"], "src/lib.rs");
        assert_eq!(files[2]["status"], "pending");
        assert_eq!(files[2]["reason"], "parser_extraction_pending");
    }
}
