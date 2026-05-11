// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use ignore::{DirEntry, Error as IgnoreError, WalkBuilder};
use serde::Serialize;

pub mod git;
pub mod storage;

#[cfg(test)]
const BINARY_SAMPLE_BYTES: usize = 8 * 1024;
const MAX_TEXT_READ_BYTES: u64 = 8 * 1024 * 1024;
pub const SCAN_SCHEMA_VERSION: &str = "hotpath.scan.v1";
const SUMMARY_LABEL_WIDTH: usize = 12;

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
            Self::Index(source) => write!(f, "failed to persist scan results: {source}"),
            Self::Json(source) => write!(f, "failed to render scan JSON: {source}"),
        }
    }
}

impl StdError for ScanError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) | Self::Root { source, .. } => Some(source),
            Self::RootNotDirectory { .. } | Self::RelativePath { .. } => None,
            Self::Index(source) => Some(source),
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

fn render_json(scan: &ScanReport) -> Result<String, ScanError> {
    Ok(serde_json::to_string_pretty(&ScanJsonReport {
        schema_version: SCAN_SCHEMA_VERSION,
        summary: scan.summary(),
        warnings: &scan.warnings,
        files: &scan.files,
    })?)
}

fn render_doctor(index_path: &str, schema_version: &str, readable: &str, health: &str) -> String {
    format!(
        "Hotpath doctor\nindex path: {index_path}\nschema version: {schema_version}\nreadable: {readable}\nhealth: {health}"
    )
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
}
