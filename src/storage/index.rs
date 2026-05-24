// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{
    params, params_from_iter, types::Type, Connection, OpenFlags, OptionalExtension, Row,
    Statement, ToSql, Transaction,
};
use serde::Serialize;
use serde_json::json;

use crate::git::{GitCoChange, GitFileChange, GitFileMetrics};
use crate::operation_log;
use crate::scoring::{NormalizedScoreMetrics, RankedHotspotScore, ScoreLimitation, WeightedTerm};
use crate::{
    dependency, ContentKind, FileRecord, FileWarning, ParseReport, ParseSymbolRecord, ScanReport,
    ScanWarning, SCAN_SCHEMA_VERSION,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 4;

const HOTPATH_DIR: &str = ".hotpath";
const INDEX_FILE: &str = "index.db";
const SCHEMA_IDENTIFIER_V1: &str = "hotpath.index.v1";
const SCHEMA_IDENTIFIER_V2: &str = "hotpath.index.v2";
const SCHEMA_IDENTIFIER_V3: &str = "hotpath.index.v3";
const SCHEMA_IDENTIFIER: &str = "hotpath.index.v4";
const SCHEMA_IDENTIFIER_KEY: &str = "schema_identifier";
const SCHEMA_VERSION_KEY: &str = "schema_version";
const GIT_ANALYSIS_KEY: &str = "git-analysis-current";
const REQUIRED_SCHEMA_TABLES: &[&str] = &[
    "hotpath_metadata",
    "repos",
    "scan_runs",
    "scan_warnings",
    "files",
    "file_warnings",
    "symbols",
    "git_analysis_runs",
    "git_file_stats",
    "git_co_changes",
    "dependencies",
    "hotspots",
    "git_commit_diffs",
];

#[derive(Debug)]
pub struct IndexStore {
    connection: Connection,
    path: PathBuf,
    schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexInspection {
    Missing { path: PathBuf },
    Healthy { path: PathBuf, schema_version: u32 },
}

impl IndexInspection {
    pub fn path(&self) -> &Path {
        match self {
            Self::Missing { path } | Self::Healthy { path, .. } => path,
        }
    }

    pub fn schema_version(&self) -> Option<u32> {
        match self {
            Self::Missing { .. } => None,
            Self::Healthy { schema_version, .. } => Some(*schema_version),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedScan {
    pub run: PersistedScanRun,
    pub warnings: Vec<PersistedScanWarning>,
    pub files: Vec<PersistedFileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedScanRun {
    pub id: i64,
    pub run_key: String,
    pub status: String,
    pub scanner_version: Option<String>,
    pub scan_schema_identifier: Option<String>,
    pub files_observed: Option<u64>,
    pub warnings_observed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedFileRecord {
    pub path: String,
    pub byte_size: Option<u64>,
    pub extension: Option<String>,
    pub language: Option<String>,
    pub line_count: Option<u64>,
    pub is_vendor: bool,
    pub is_generated: bool,
    pub content: ContentKind,
    pub is_symlink: bool,
    pub classification: Option<String>,
    pub warnings: Vec<PersistedFileWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedScanWarning {
    pub code: String,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedFileWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistedGitAnalysis {
    pub run: PersistedGitAnalysisRun,
    pub file_stats: Vec<PersistedGitFileStats>,
    pub co_changes: Vec<PersistedGitCoChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedGitAnalysisRun {
    pub id: i64,
    pub analysis_key: String,
    pub status: String,
    pub analyzer_version: Option<String>,
    pub git_head: String,
    pub head_commit_time: i64,
    pub recent_window_days: u64,
    pub metrics_observed: u64,
    pub co_changes_observed: u64,
}

pub type PersistedGitFileStats = GitFileMetrics;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedGitCoChange {
    pub left_path: String,
    pub right_path: String,
    pub commit_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistedHotspot {
    pub path: String,
    pub score: f64,
    pub rank: u64,
    pub formula_version: String,
    pub raw_metrics_json: Option<String>,
    pub explanation: Option<String>,
    pub limitation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSymbol {
    pub id: i64,
    pub path: String,
    pub parent_symbol_id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub line_start: Option<u64>,
    pub line_end: Option<u64>,
    pub signature: Option<String>,
}

#[derive(Serialize)]
struct HotspotExplanationPayload<'a> {
    normalized_metrics: &'a NormalizedScoreMetrics,
    weighted_terms: &'a [WeightedTerm],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InsertedSymbol {
    id: i64,
    file_path: String,
    parent_path: Option<String>,
    line_start: u64,
    line_end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SymbolLookup {
    id: i64,
    line_start: u64,
    line_end: u64,
}

#[derive(Serialize)]
struct HotspotLimitationsPayload<'a> {
    limitations: &'a [ScoreLimitation],
}

impl IndexStore {
    pub fn inspect(repo_root: impl AsRef<Path>) -> Result<IndexInspection, IndexError> {
        let path = default_index_path(repo_root);
        let parent = path
            .parent()
            .expect("default index path should always have a parent");

        if !existing_index_dir_is_safe(parent)? {
            return Ok(IndexInspection::Missing { path });
        }

        match fs::metadata(&path) {
            Ok(_) => {}
            Err(source) if source.kind() == ErrorKind::NotFound => {
                return Ok(IndexInspection::Missing { path });
            }
            Err(source) => {
                return Err(IndexError::AccessIndex { path, source });
            }
        }

        let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| IndexError::OpenDatabase {
                path: path.clone(),
                source,
            })?;
        verify_database_integrity(&connection, &path)?;
        let schema_version = read_user_version(&connection, &path)?;

        if schema_version > CURRENT_SCHEMA_VERSION {
            return Err(IndexError::IncompatibleFutureSchema {
                path,
                found_version: schema_version,
                supported_version: CURRENT_SCHEMA_VERSION,
            });
        }

        if schema_version != CURRENT_SCHEMA_VERSION {
            return Err(IndexError::CorruptMetadata {
                path,
                message: format!(
                    "schema version {schema_version} is not initialized for this binary; run 'hotpath scan' to create or migrate the index"
                ),
            });
        }

        verify_metadata(&connection, &path)?;

        Ok(IndexInspection::Healthy {
            path,
            schema_version,
        })
    }

    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self, IndexError> {
        let path = default_index_path(repo_root);
        let parent = path
            .parent()
            .expect("default index path should always have a parent");

        ensure_index_dir(parent)?;

        let mut connection =
            Connection::open(&path).map_err(|source| IndexError::OpenDatabase {
                path: path.clone(),
                source,
            })?;
        enable_foreign_keys(&connection, &path)?;
        verify_database_integrity(&connection, &path)?;
        let schema_version = read_user_version(&connection, &path)?;

        if schema_version > CURRENT_SCHEMA_VERSION {
            return Err(IndexError::IncompatibleFutureSchema {
                path,
                found_version: schema_version,
                supported_version: CURRENT_SCHEMA_VERSION,
            });
        }

        migrate_to_current(&mut connection, &path, schema_version)?;
        verify_metadata(&connection, &path)?;

        Ok(Self {
            connection,
            path,
            schema_version: CURRENT_SCHEMA_VERSION,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn persist_scan(&mut self, scan: &ScanReport) -> Result<PersistedScanRun, IndexError> {
        let index_path = self.path.clone();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistScan {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo(&transaction, &index_path)?;
        let run_key = next_run_key(&transaction, &index_path, repo_id)?;
        let summary = scan.summary();
        let files_observed = u64_to_i64(summary.total_files, &index_path, "files_observed")?;
        let warnings_observed = u64_to_i64(
            summary.warnings.total_warnings,
            &index_path,
            "warnings_observed",
        )?;

        transaction
            .execute(
                "INSERT INTO scan_runs (
                    repo_id,
                    run_key,
                    status,
                    scanner_version,
                    scan_schema_identifier,
                    files_observed,
                    warnings_observed
                )
                VALUES (?1, ?2, 'completed', ?3, ?4, ?5, ?6);",
                params![
                    repo_id,
                    run_key,
                    env!("CARGO_PKG_VERSION"),
                    SCAN_SCHEMA_VERSION,
                    files_observed,
                    warnings_observed,
                ],
            )
            .map_err(|source| IndexError::PersistScan {
                path: index_path.clone(),
                source,
            })?;
        let scan_run_id = transaction.last_insert_rowid();

        {
            let mut scan_warning_insert = prepare_statement(
                &transaction,
                &index_path,
                "INSERT INTO scan_warnings (
                    scan_run_id,
                    warning_order,
                    code,
                    path,
                    message
                )
                VALUES (?1, ?2, ?3, ?4, ?5);",
            )?;
            persist_scan_warnings(
                &mut scan_warning_insert,
                &index_path,
                scan_run_id,
                &scan.warnings,
            )?;
        }

        {
            let mut file_upsert = prepare_statement(
                &transaction,
                &index_path,
                "INSERT INTO files (
                    repo_id,
                    path,
                    byte_size,
                    extension,
                    language,
                    line_count,
                    content_kind,
                    is_vendor,
                    is_generated,
                    is_symlink,
                    classification,
                    scan_run_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(repo_id, path) DO UPDATE SET
                    byte_size = excluded.byte_size,
                    extension = excluded.extension,
                    language = excluded.language,
                    line_count = excluded.line_count,
                    content_kind = excluded.content_kind,
                    is_vendor = excluded.is_vendor,
                    is_generated = excluded.is_generated,
                    is_symlink = excluded.is_symlink,
                    classification = excluded.classification,
                    scan_run_id = excluded.scan_run_id;",
            )?;
            let mut file_id_query = prepare_statement(
                &transaction,
                &index_path,
                "SELECT id FROM files WHERE repo_id = ?1 AND path = ?2;",
            )?;
            let mut file_warning_insert = prepare_statement(
                &transaction,
                &index_path,
                "INSERT INTO file_warnings (
                    file_id,
                    scan_run_id,
                    warning_order,
                    code,
                    message
                )
                VALUES (?1, ?2, ?3, ?4, ?5);",
            )?;

            for file in &scan.files {
                let file_id = persist_file(
                    &mut file_upsert,
                    &mut file_id_query,
                    &index_path,
                    repo_id,
                    scan_run_id,
                    file,
                )?;
                persist_file_warnings(
                    &mut file_warning_insert,
                    &index_path,
                    file_id,
                    scan_run_id,
                    &file.warnings,
                )?;
            }
        }

        delete_stale_files(&transaction, &index_path, repo_id, scan_run_id)?;

        transaction
            .commit()
            .map_err(|source| IndexError::PersistScan {
                path: index_path,
                source,
            })?;

        Ok(PersistedScanRun {
            id: scan_run_id,
            run_key,
            status: "completed".to_owned(),
            scanner_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            scan_schema_identifier: Some(SCAN_SCHEMA_VERSION.to_owned()),
            files_observed: Some(summary.total_files),
            warnings_observed: Some(summary.warnings.total_warnings),
        })
    }

    pub fn persist_git_analysis(
        &mut self,
        _repo_root: impl AsRef<Path>,
        git_head: &str,
        head_commit_time: i64,
        recent_window_days: u64,
        metrics: &[GitFileMetrics],
        co_changes: &[GitCoChange],
    ) -> Result<PersistedGitAnalysisRun, IndexError> {
        let index_path = self.path.clone();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistGitAnalysis {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo_for_git(&transaction, &index_path)?;
        let recent_window_days =
            u64_to_i64_for_git(recent_window_days, &index_path, "recent_window_days")?;
        let metrics_observed =
            usize_to_i64_for_git(metrics.len(), &index_path, "metrics_observed")?;
        let co_changes_observed =
            usize_to_i64_for_git(co_changes.len(), &index_path, "co_changes_observed")?;

        transaction
            .execute(
                "DELETE FROM git_analysis_runs WHERE repo_id = ?1;",
                params![repo_id],
            )
            .map_err(|source| IndexError::PersistGitAnalysis {
                path: index_path.clone(),
                source,
            })?;
        transaction
            .execute(
                "INSERT INTO git_analysis_runs (
                    repo_id,
                    analysis_key,
                    status,
                    analyzer_version,
                    git_head,
                    head_commit_time,
                    recent_window_days,
                    metrics_observed,
                    co_changes_observed
                )
                VALUES (?1, ?2, 'completed', ?3, ?4, ?5, ?6, ?7, ?8);",
                params![
                    repo_id,
                    GIT_ANALYSIS_KEY,
                    env!("CARGO_PKG_VERSION"),
                    git_head,
                    head_commit_time,
                    recent_window_days,
                    metrics_observed,
                    co_changes_observed,
                ],
            )
            .map_err(|source| IndexError::PersistGitAnalysis {
                path: index_path.clone(),
                source,
            })?;
        let analysis_run_id = transaction.last_insert_rowid();

        let mut file_ids = BTreeMap::new();
        for metric in metrics {
            ensure_git_path_file(
                &transaction,
                &index_path,
                repo_id,
                &metric.path,
                &mut file_ids,
            )?;
        }
        for co_change in co_changes {
            ensure_git_path_file(
                &transaction,
                &index_path,
                repo_id,
                &co_change.left_path,
                &mut file_ids,
            )?;
            ensure_git_path_file(
                &transaction,
                &index_path,
                repo_id,
                &co_change.right_path,
                &mut file_ids,
            )?;
        }

        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO git_file_stats (
                        file_id,
                        repo_id,
                        analysis_run_id,
                        commits_per_file,
                        total_churn_added,
                        total_churn_deleted,
                        recent_churn_added,
                        recent_churn_deleted,
                        author_count,
                        owner_count,
                        dominant_owner,
                        dominant_owner_share,
                        first_commit_id,
                        first_commit_time,
                        last_commit_id,
                        last_commit_time,
                        file_age_days
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17);",
                )
                .map_err(|source| IndexError::PersistGitAnalysis {
                    path: index_path.clone(),
                    source,
                })?;

            for metric in metrics {
                let file_id = file_ids[&metric.path];
                insert
                    .execute(params![
                        file_id,
                        repo_id,
                        analysis_run_id,
                        u64_to_i64_for_git(
                            metric.commits_per_file,
                            &index_path,
                            "commits_per_file"
                        )?,
                        u64_to_i64_for_git(
                            metric.total_churn_added,
                            &index_path,
                            "total_churn_added"
                        )?,
                        u64_to_i64_for_git(
                            metric.total_churn_deleted,
                            &index_path,
                            "total_churn_deleted"
                        )?,
                        u64_to_i64_for_git(
                            metric.recent_churn_added,
                            &index_path,
                            "recent_churn_added"
                        )?,
                        u64_to_i64_for_git(
                            metric.recent_churn_deleted,
                            &index_path,
                            "recent_churn_deleted"
                        )?,
                        u64_to_i64_for_git(metric.author_count, &index_path, "author_count")?,
                        u64_to_i64_for_git(metric.owner_count, &index_path, "owner_count")?,
                        metric.dominant_owner.as_deref(),
                        metric.dominant_owner_share,
                        metric.first_commit_id.as_deref(),
                        metric.first_commit_time,
                        metric.last_commit_id.as_deref(),
                        metric.last_commit_time,
                        optional_u64_to_i64_for_git(
                            metric.file_age_days,
                            &index_path,
                            "file_age_days"
                        )?,
                    ])
                    .map_err(|source| IndexError::PersistGitAnalysis {
                        path: index_path.clone(),
                        source,
                    })?;
            }
        }

        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO git_co_changes (
                        repo_id,
                        analysis_run_id,
                        left_file_id,
                        right_file_id,
                        left_path,
                        right_path,
                        commit_count
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
                )
                .map_err(|source| IndexError::PersistGitAnalysis {
                    path: index_path.clone(),
                    source,
                })?;

            for co_change in co_changes {
                insert
                    .execute(params![
                        repo_id,
                        analysis_run_id,
                        file_ids[&co_change.left_path],
                        file_ids[&co_change.right_path],
                        &co_change.left_path,
                        &co_change.right_path,
                        u64_to_i64_for_git(co_change.commit_count, &index_path, "commit_count")?,
                    ])
                    .map_err(|source| IndexError::PersistGitAnalysis {
                        path: index_path.clone(),
                        source,
                    })?;
            }
        }

        transaction
            .commit()
            .map_err(|source| IndexError::PersistGitAnalysis {
                path: index_path,
                source,
            })?;

        Ok(PersistedGitAnalysisRun {
            id: analysis_run_id,
            analysis_key: GIT_ANALYSIS_KEY.to_owned(),
            status: "completed".to_owned(),
            analyzer_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            git_head: git_head.to_owned(),
            head_commit_time,
            recent_window_days: recent_window_days as u64,
            metrics_observed: metrics_observed as u64,
            co_changes_observed: co_changes_observed as u64,
        })
    }

    pub fn persist_hotspots(
        &mut self,
        scan_run_id: i64,
        ranked_scores: &[RankedHotspotScore],
    ) -> Result<(), IndexError> {
        let index_path = self.path.clone();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistHotspots {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo_for_hotspots(&transaction, &index_path)?;

        transaction
            .execute("DELETE FROM hotspots WHERE repo_id = ?1;", params![repo_id])
            .map_err(|source| IndexError::PersistHotspots {
                path: index_path.clone(),
                source,
            })?;

        {
            let mut file_id_query = transaction
                .prepare("SELECT id FROM files WHERE repo_id = ?1 AND path = ?2;")
                .map_err(|source| IndexError::PersistHotspots {
                    path: index_path.clone(),
                    source,
                })?;
            let mut insert = transaction
                .prepare(
                    "INSERT INTO hotspots (
                        file_id,
                        repo_id,
                        scan_run_id,
                        score,
                        rank,
                        formula_version,
                        raw_metrics_json,
                        explanation,
                        limitation
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9);",
                )
                .map_err(|source| IndexError::PersistHotspots {
                    path: index_path.clone(),
                    source,
                })?;

            for ranked_score in ranked_scores {
                persist_hotspot(
                    &mut file_id_query,
                    &mut insert,
                    &index_path,
                    repo_id,
                    scan_run_id,
                    ranked_score,
                )?;
            }
        }

        transaction
            .commit()
            .map_err(|source| IndexError::PersistHotspots {
                path: index_path,
                source,
            })?;

        Ok(())
    }

    pub fn persist_symbols(&mut self, report: &ParseReport) -> Result<(), IndexError> {
        let index_path = self.path.clone();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo_for_symbols(&transaction, &index_path)?;
        let file_ids =
            current_file_ids_for_parse_report(&transaction, &index_path, repo_id, report)?;

        {
            let mut delete = transaction
                .prepare("DELETE FROM symbols WHERE file_id = ?1;")
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.clone(),
                    source,
                })?;

            for file_id in file_ids.values() {
                delete
                    .execute(params![file_id])
                    .map_err(|source| IndexError::PersistSymbols {
                        path: index_path.clone(),
                        source,
                    })?;
            }
        }

        let mut sorted_symbols = report.symbols.iter().collect::<Vec<_>>();
        sorted_symbols.sort_by(|left, right| symbol_sort_key(left).cmp(&symbol_sort_key(right)));

        let mut inserted_symbols = Vec::with_capacity(sorted_symbols.len());
        let mut symbol_paths: BTreeMap<(String, String), Vec<SymbolLookup>> = BTreeMap::new();

        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO symbols (
                        file_id,
                        parent_symbol_id,
                        name,
                        kind,
                        line_start,
                        line_end,
                        signature
                    )
                    VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6);",
                )
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.clone(),
                    source,
                })?;

            for symbol in sorted_symbols {
                let file_id = symbol_file_id(&file_ids, &index_path, symbol)?;
                let line_start = symbol_line_to_i64(symbol.start_line, &index_path, symbol)?;
                let line_end = symbol_line_to_i64(symbol.end_line, &index_path, symbol)?;

                if symbol.end_line < symbol.start_line {
                    return Err(IndexError::InvalidSymbolData {
                        path: index_path.clone(),
                        message: format!(
                            "symbol '{}' in '{}' has end line before start line",
                            symbol.name, symbol.path
                        ),
                    });
                }

                if symbol.name.is_empty() {
                    return Err(IndexError::InvalidSymbolData {
                        path: index_path.clone(),
                        message: format!("symbol in '{}' has an empty name", symbol.path),
                    });
                }

                if symbol.kind.is_empty() {
                    return Err(IndexError::InvalidSymbolData {
                        path: index_path.clone(),
                        message: format!(
                            "symbol '{}' in '{}' has an empty kind",
                            symbol.name, symbol.path
                        ),
                    });
                }

                insert
                    .execute(params![
                        file_id,
                        &symbol.name,
                        &symbol.kind,
                        line_start,
                        line_end,
                        symbol.signature.as_deref(),
                    ])
                    .map_err(|source| IndexError::PersistSymbols {
                        path: index_path.clone(),
                        source,
                    })?;
                let id = transaction.last_insert_rowid();
                let symbol_path = persisted_symbol_path(symbol);

                symbol_paths
                    .entry((symbol.path.clone(), symbol_path))
                    .or_default()
                    .push(SymbolLookup {
                        id,
                        line_start: symbol.start_line,
                        line_end: symbol.end_line,
                    });
                inserted_symbols.push(InsertedSymbol {
                    id,
                    file_path: symbol.path.clone(),
                    parent_path: symbol.parent.clone(),
                    line_start: symbol.start_line,
                    line_end: symbol.end_line,
                });
            }
        }

        {
            let mut update = transaction
                .prepare("UPDATE symbols SET parent_symbol_id = ?1 WHERE id = ?2;")
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.clone(),
                    source,
                })?;

            for symbol in &inserted_symbols {
                let Some(parent_path) = &symbol.parent_path else {
                    continue;
                };
                let Some(parent_id) = resolved_parent_symbol_id(
                    &symbol_paths,
                    &symbol.file_path,
                    parent_path,
                    symbol.id,
                    symbol.line_start,
                    symbol.line_end,
                ) else {
                    continue;
                };

                update
                    .execute(params![parent_id, symbol.id])
                    .map_err(|source| IndexError::PersistSymbols {
                        path: index_path.clone(),
                        source,
                    })?;
            }
        }

        replace_dependencies_for_report(&transaction, &index_path, repo_id, &file_ids, report)?;

        transaction
            .commit()
            .map_err(|source| IndexError::PersistSymbols {
                path: index_path,
                source,
            })?;

        Ok(())
    }

    pub fn persist_dependencies(&mut self, report: &ParseReport) -> Result<(), IndexError> {
        let index_path = self.path.clone();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo_for_symbols(&transaction, &index_path)?;
        let file_ids =
            current_file_ids_for_parse_report(&transaction, &index_path, repo_id, report)?;

        replace_dependencies_for_report(&transaction, &index_path, repo_id, &file_ids, report)?;

        transaction
            .commit()
            .map_err(|source| IndexError::PersistSymbols {
                path: index_path,
                source,
            })?;

        Ok(())
    }

    pub fn latest_scan(&self) -> Result<Option<PersistedScan>, IndexError> {
        let Some(run) = self
            .connection
            .query_row(
                "SELECT
                    id,
                    run_key,
                    status,
                    scanner_version,
                    scan_schema_identifier,
                    files_observed,
                    warnings_observed
                 FROM scan_runs
                 ORDER BY id DESC
                 LIMIT 1;",
                [],
                read_persisted_scan_run,
            )
            .optional()
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?
        else {
            return Ok(None);
        };

        let warnings = read_scan_warnings(&self.connection, &self.path, run.id)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT
                    id,
                    path,
                    byte_size,
                    extension,
                    language,
                    line_count,
                    is_vendor,
                    is_generated,
                    content_kind,
                    is_symlink,
                    classification
                 FROM files
                 WHERE scan_run_id = ?1
                 ORDER BY path;",
            )
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;
        let rows = statement
            .query_map(params![run.id], read_persisted_file_record)
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;
        let mut files = Vec::new();

        for row in rows {
            let (file_id, mut file) = row.map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;
            file.warnings = read_file_warnings(&self.connection, &self.path, run.id, file_id)?;
            files.push(file);
        }

        Ok(Some(PersistedScan {
            run,
            warnings,
            files,
        }))
    }

    pub fn latest_git_analysis(&self) -> Result<Option<PersistedGitAnalysis>, IndexError> {
        let Some(run) = self
            .connection
            .query_row(
                "SELECT
                    id,
                    analysis_key,
                    status,
                    analyzer_version,
                    git_head,
                    head_commit_time,
                    recent_window_days,
                    metrics_observed,
                    co_changes_observed
                 FROM git_analysis_runs
                 ORDER BY id DESC
                 LIMIT 1;",
                [],
                read_persisted_git_analysis_run,
            )
            .optional()
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?
        else {
            return Ok(None);
        };

        let file_stats = read_git_file_stats(&self.connection, &self.path, run.id)?;
        let co_changes = read_git_co_changes(&self.connection, &self.path, run.id)?;

        Ok(Some(PersistedGitAnalysis {
            run,
            file_stats,
            co_changes,
        }))
    }

    pub fn latest_hotspots(&self) -> Result<Vec<PersistedHotspot>, IndexError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT
                    files.path,
                    hotspots.score,
                    hotspots.rank,
                    hotspots.formula_version,
                    hotspots.raw_metrics_json,
                    hotspots.explanation,
                    hotspots.limitation
                 FROM hotspots
                 INNER JOIN files ON files.id = hotspots.file_id
                 INNER JOIN repos ON repos.id = hotspots.repo_id
                 WHERE repos.root_key = '.'
                 ORDER BY hotspots.rank, files.path;",
            )
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;
        let rows = statement
            .query_map([], read_persisted_hotspot)
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })
    }

    pub fn latest_symbols(&self) -> Result<Vec<PersistedSymbol>, IndexError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT
                    symbols.id,
                    files.path,
                    symbols.parent_symbol_id,
                    symbols.name,
                    symbols.kind,
                    symbols.line_start,
                    symbols.line_end,
                    symbols.signature
                 FROM symbols
                 INNER JOIN files ON files.id = symbols.file_id
                 INNER JOIN repos ON repos.id = files.repo_id
                 WHERE repos.root_key = '.'
                 ORDER BY files.path, symbols.line_start, symbols.line_end, symbols.kind, symbols.name, symbols.id;",
            )
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;
        let rows = statement
            .query_map([], read_persisted_symbol)
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })
    }

    pub fn dependency_count(&self) -> Result<u64, IndexError> {
        let count = self
            .connection
            .query_row(
                "SELECT COUNT(*)
                 FROM dependencies
                 INNER JOIN repos ON repos.id = dependencies.repo_id
                 WHERE repos.root_key = '.';",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;

        i64_to_u64(count, 0).map_err(|source| IndexError::ReadIndex {
            path: self.path.clone(),
            source,
        })
    }

    pub fn cached_git_commit_changes(
        &self,
        commit_id: &str,
        analyzer_version: &str,
    ) -> Result<Option<Vec<GitFileChange>>, IndexError> {
        let json = self
            .connection
            .query_row(
                "SELECT changes_json
                 FROM git_commit_diffs
                 INNER JOIN repos ON repos.id = git_commit_diffs.repo_id
                 WHERE repos.root_key = '.'
                   AND commit_id = ?1
                   AND analyzer_version = ?2;",
                params![commit_id, analyzer_version],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|source| IndexError::ReadIndex {
                path: self.path.clone(),
                source,
            })?;

        json.map(|json| {
            serde_json::from_str::<Vec<GitFileChange>>(&json).map_err(|source| {
                IndexError::InvalidGitAnalysisData {
                    path: self.path.clone(),
                    message: format!(
                        "cached Git commit diff for {commit_id} is not valid JSON: {source}"
                    ),
                }
            })
        })
        .transpose()
    }

    pub fn cached_git_commit_changes_batch(
        &self,
        commit_ids: &[String],
        analyzer_version: &str,
    ) -> Result<BTreeMap<String, Vec<GitFileChange>>, IndexError> {
        let mut changes_by_commit = BTreeMap::new();
        if commit_ids.is_empty() {
            return Ok(changes_by_commit);
        }

        for chunk in commit_ids.chunks(900) {
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT commit_id, changes_json
                 FROM git_commit_diffs
                 INNER JOIN repos ON repos.id = git_commit_diffs.repo_id
                 WHERE repos.root_key = '.'
                   AND analyzer_version = ?
                   AND commit_id IN ({placeholders});"
            );
            let mut statement =
                self.connection
                    .prepare(&sql)
                    .map_err(|source| IndexError::ReadIndex {
                        path: self.path.clone(),
                        source,
                    })?;
            let mut query_params = Vec::<&dyn ToSql>::with_capacity(chunk.len() + 1);
            query_params.push(&analyzer_version);
            for commit_id in chunk {
                query_params.push(commit_id);
            }
            let rows = statement
                .query_map(params_from_iter(query_params), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|source| IndexError::ReadIndex {
                    path: self.path.clone(),
                    source,
                })?;
            for row in rows {
                let (commit_id, json) = row.map_err(|source| IndexError::ReadIndex {
                    path: self.path.clone(),
                    source,
                })?;
                let changes =
                    serde_json::from_str::<Vec<GitFileChange>>(&json).map_err(|source| {
                        IndexError::InvalidGitAnalysisData {
                            path: self.path.clone(),
                            message: format!(
                                "cached Git commit diff for {commit_id} is not valid JSON: {source}"
                            ),
                        }
                    })?;
                changes_by_commit.insert(commit_id, changes);
            }
        }

        Ok(changes_by_commit)
    }

    pub fn persist_git_commit_changes(
        &mut self,
        commit_id: &str,
        analyzer_version: &str,
        changes: &[GitFileChange],
    ) -> Result<(), IndexError> {
        self.persist_git_commit_changes_batch(
            analyzer_version,
            &[(commit_id.to_owned(), changes.to_vec())],
        )
    }

    pub fn persist_git_commit_changes_batch(
        &mut self,
        analyzer_version: &str,
        commits: &[(String, Vec<GitFileChange>)],
    ) -> Result<(), IndexError> {
        if commits.is_empty() {
            return Ok(());
        }

        let index_path = self.path.clone();
        let mut payloads = Vec::with_capacity(commits.len());
        let serialize_started = Instant::now();
        for (commit_id, changes) in commits {
            let changes_json = serde_json::to_string(changes).map_err(|source| {
                IndexError::InvalidGitAnalysisData {
                    path: index_path.clone(),
                    message: format!("failed to serialize cached Git commit diff: {source}"),
                }
            })?;
            payloads.push((commit_id, changes_json));
        }
        let serialize_ms = elapsed_ms(serialize_started.elapsed());

        let write_started = Instant::now();
        let transaction =
            self.connection
                .transaction()
                .map_err(|source| IndexError::PersistGitAnalysis {
                    path: index_path.clone(),
                    source,
                })?;
        let repo_id = ensure_repo_for_git(&transaction, &index_path)?;

        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO git_commit_diffs (
                    repo_id,
                    commit_id,
                    analyzer_version,
                    changes_json
                )
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(repo_id, commit_id, analyzer_version) DO UPDATE SET
                    changes_json = excluded.changes_json;",
                )
                .map_err(|source| IndexError::PersistGitAnalysis {
                    path: index_path.clone(),
                    source,
                })?;

            for (commit_id, changes_json) in payloads {
                insert
                    .execute(params![repo_id, commit_id, analyzer_version, changes_json])
                    .map_err(|source| IndexError::PersistGitAnalysis {
                        path: index_path.clone(),
                        source,
                    })?;
            }
        }

        transaction
            .commit()
            .map_err(|source| IndexError::PersistGitAnalysis {
                path: index_path.clone(),
                source,
            })?;
        if git_perf_enabled() {
            operation_log::event(
                "hotpath.git_cache_write",
                json!({
                    "commits": commits.len(),
                    "serialize_ms": serialize_ms,
                    "sqlite_write_ms": elapsed_ms(write_started.elapsed()),
                }),
            );
        }

        Ok(())
    }
}

fn git_perf_enabled() -> bool {
    env::var("HOTPATH_PERF").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn elapsed_ms(duration: Duration) -> u128 {
    duration.as_millis()
}

#[derive(Debug)]
pub enum IndexError {
    CreateIndexDir {
        path: PathBuf,
        source: std::io::Error,
    },
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    AccessIndex {
        path: PathBuf,
        source: std::io::Error,
    },
    CorruptDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    CorruptMetadata {
        path: PathBuf,
        message: String,
    },
    UnsafeIndexDir {
        path: PathBuf,
        message: String,
    },
    IncompatibleFutureSchema {
        path: PathBuf,
        found_version: u32,
        supported_version: u32,
    },
    Migration {
        path: PathBuf,
        from_version: u32,
        to_version: u32,
        source: rusqlite::Error,
    },
    PersistScan {
        path: PathBuf,
        source: rusqlite::Error,
    },
    PersistGitAnalysis {
        path: PathBuf,
        source: rusqlite::Error,
    },
    PersistSymbols {
        path: PathBuf,
        source: rusqlite::Error,
    },
    PersistHotspots {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ReadIndex {
        path: PathBuf,
        source: rusqlite::Error,
    },
    InvalidScanData {
        path: PathBuf,
        message: String,
    },
    InvalidGitAnalysisData {
        path: PathBuf,
        message: String,
    },
    InvalidSymbolData {
        path: PathBuf,
        message: String,
    },
    InvalidHotspotData {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateIndexDir { path, source } => write!(
                f,
                "failed to create Hotpath index directory '{}': {source}",
                path.display()
            ),
            Self::OpenDatabase { path, source } => {
                write!(f, "failed to open Hotpath index '{}': {source}", path.display())
            }
            Self::AccessIndex { path, source } => {
                write!(f, "failed to access Hotpath index '{}': {source}", path.display())
            }
            Self::CorruptDatabase { path, source } => write!(
                f,
                "Hotpath index '{}' is unreadable or corrupt: {source}",
                path.display()
            ),
            Self::CorruptMetadata { path, message } => write!(
                f,
                "Hotpath index '{}' has invalid schema metadata: {message}",
                path.display()
            ),
            Self::UnsafeIndexDir { path, message } => write!(
                f,
                "refusing to use Hotpath index directory '{}': {message}",
                path.display()
            ),
            Self::IncompatibleFutureSchema {
                path,
                found_version,
                supported_version,
            } => write!(
                f,
                "Hotpath index '{}' uses schema version {found_version}, but this binary supports up to version {supported_version}",
                path.display()
            ),
            Self::Migration {
                path,
                from_version,
                to_version,
                source,
            } => write!(
                f,
                "failed to migrate Hotpath index '{}' from schema version {from_version} to {to_version}: {source}",
                path.display()
            ),
            Self::PersistScan { path, source } => write!(
                f,
                "failed to persist scan results to Hotpath index '{}': {source}",
                path.display()
            ),
            Self::PersistGitAnalysis { path, source } => write!(
                f,
                "failed to persist Git analysis to Hotpath index '{}': {source}",
                path.display()
            ),
            Self::PersistSymbols { path, source } => write!(
                f,
                "failed to persist parser symbols to Hotpath index '{}': {source}",
                path.display()
            ),
            Self::PersistHotspots { path, source } => write!(
                f,
                "failed to persist hotspot scores to Hotpath index '{}': {source}",
                path.display()
            ),
            Self::ReadIndex { path, source } => write!(
                f,
                "failed to read Hotpath index '{}': {source}",
                path.display()
            ),
            Self::InvalidScanData { path, message } => write!(
                f,
                "scan results cannot be persisted to Hotpath index '{}': {message}",
                path.display()
            ),
            Self::InvalidGitAnalysisData { path, message } => write!(
                f,
                "Git analysis cannot be persisted to Hotpath index '{}': {message}",
                path.display()
            ),
            Self::InvalidSymbolData { path, message } => write!(
                f,
                "parser symbols cannot be persisted to Hotpath index '{}': {message}",
                path.display()
            ),
            Self::InvalidHotspotData { path, message } => write!(
                f,
                "hotspot scores cannot be persisted to Hotpath index '{}': {message}",
                path.display()
            ),
        }
    }
}

impl StdError for IndexError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateIndexDir { source, .. } => Some(source),
            Self::AccessIndex { source, .. } => Some(source),
            Self::OpenDatabase { source, .. }
            | Self::CorruptDatabase { source, .. }
            | Self::Migration { source, .. }
            | Self::PersistScan { source, .. }
            | Self::PersistGitAnalysis { source, .. }
            | Self::PersistSymbols { source, .. }
            | Self::PersistHotspots { source, .. }
            | Self::ReadIndex { source, .. } => Some(source),
            Self::CorruptMetadata { .. }
            | Self::UnsafeIndexDir { .. }
            | Self::IncompatibleFutureSchema { .. }
            | Self::InvalidScanData { .. }
            | Self::InvalidGitAnalysisData { .. }
            | Self::InvalidSymbolData { .. }
            | Self::InvalidHotspotData { .. } => None,
        }
    }
}

pub fn default_index_path(repo_root: impl AsRef<Path>) -> PathBuf {
    repo_root.as_ref().join(HOTPATH_DIR).join(INDEX_FILE)
}

fn ensure_repo(transaction: &Transaction<'_>, path: &Path) -> Result<i64, IndexError> {
    transaction
        .execute(
            "INSERT INTO repos (root_key)
             VALUES ('.')
             ON CONFLICT(root_key) DO NOTHING;",
            [],
        )
        .map_err(|source| IndexError::PersistScan {
            path: path.to_path_buf(),
            source,
        })?;
    transaction
        .query_row("SELECT id FROM repos WHERE root_key = '.';", [], |row| {
            row.get(0)
        })
        .map_err(|source| IndexError::PersistScan {
            path: path.to_path_buf(),
            source,
        })
}

fn ensure_repo_for_git(transaction: &Transaction<'_>, path: &Path) -> Result<i64, IndexError> {
    transaction
        .execute(
            "INSERT INTO repos (root_key)
             VALUES ('.')
             ON CONFLICT(root_key) DO NOTHING;",
            [],
        )
        .map_err(|source| IndexError::PersistGitAnalysis {
            path: path.to_path_buf(),
            source,
        })?;
    transaction
        .query_row("SELECT id FROM repos WHERE root_key = '.';", [], |row| {
            row.get(0)
        })
        .map_err(|source| IndexError::PersistGitAnalysis {
            path: path.to_path_buf(),
            source,
        })
}

fn ensure_repo_for_hotspots(transaction: &Transaction<'_>, path: &Path) -> Result<i64, IndexError> {
    transaction
        .execute(
            "INSERT INTO repos (root_key)
             VALUES ('.')
             ON CONFLICT(root_key) DO NOTHING;",
            [],
        )
        .map_err(|source| IndexError::PersistHotspots {
            path: path.to_path_buf(),
            source,
        })?;
    transaction
        .query_row("SELECT id FROM repos WHERE root_key = '.';", [], |row| {
            row.get(0)
        })
        .map_err(|source| IndexError::PersistHotspots {
            path: path.to_path_buf(),
            source,
        })
}

fn ensure_repo_for_symbols(transaction: &Transaction<'_>, path: &Path) -> Result<i64, IndexError> {
    transaction
        .execute(
            "INSERT INTO repos (root_key)
             VALUES ('.')
             ON CONFLICT(root_key) DO NOTHING;",
            [],
        )
        .map_err(|source| IndexError::PersistSymbols {
            path: path.to_path_buf(),
            source,
        })?;
    transaction
        .query_row("SELECT id FROM repos WHERE root_key = '.';", [], |row| {
            row.get(0)
        })
        .map_err(|source| IndexError::PersistSymbols {
            path: path.to_path_buf(),
            source,
        })
}

fn ensure_git_path_file(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    file_path: &str,
    file_ids: &mut BTreeMap<String, i64>,
) -> Result<(), IndexError> {
    if file_ids.contains_key(file_path) {
        return Ok(());
    }

    transaction
        .execute(
            "INSERT INTO files (repo_id, path)
             VALUES (?1, ?2)
             ON CONFLICT(repo_id, path) DO NOTHING;",
            params![repo_id, file_path],
        )
        .map_err(|source| IndexError::PersistGitAnalysis {
            path: index_path.to_path_buf(),
            source,
        })?;

    let file_id = transaction
        .query_row(
            "SELECT id FROM files WHERE repo_id = ?1 AND path = ?2;",
            params![repo_id, file_path],
            |row| row.get(0),
        )
        .map_err(|source| IndexError::PersistGitAnalysis {
            path: index_path.to_path_buf(),
            source,
        })?;
    file_ids.insert(file_path.to_owned(), file_id);

    Ok(())
}

fn current_file_id_for_symbols(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    file_path: &str,
    label: &str,
) -> Result<i64, IndexError> {
    transaction
        .query_row(
            "SELECT id
             FROM files
             WHERE repo_id = ?1
               AND path = ?2
               AND scan_run_id IS NOT NULL;",
            params![repo_id, file_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| IndexError::PersistSymbols {
            path: index_path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| IndexError::InvalidSymbolData {
            path: index_path.to_path_buf(),
            message: format!(
                "{label} '{file_path}' is not present in the current scan index; persist the scan before persisting parser symbols"
            ),
        })
}

fn current_file_ids_for_parse_report(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    report: &ParseReport,
) -> Result<BTreeMap<String, i64>, IndexError> {
    let mut file_ids = BTreeMap::new();

    for file in &report.files {
        if file_ids.contains_key(&file.path) {
            continue;
        }

        let file_id = current_file_id_for_symbols(
            transaction,
            index_path,
            repo_id,
            &file.path,
            "parse file",
        )?;
        file_ids.insert(file.path.clone(), file_id);
    }

    Ok(file_ids)
}

fn symbol_file_id(
    file_ids: &BTreeMap<String, i64>,
    index_path: &Path,
    symbol: &ParseSymbolRecord,
) -> Result<i64, IndexError> {
    file_ids
        .get(&symbol.path)
        .copied()
        .ok_or_else(|| IndexError::InvalidSymbolData {
            path: index_path.to_path_buf(),
            message: format!(
                "symbol '{}' refers to '{}', which is not in the current parse file scope",
                symbol.name, symbol.path
            ),
        })
}

fn replace_dependencies_for_report(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    file_ids: &BTreeMap<String, i64>,
    report: &ParseReport,
) -> Result<(), IndexError> {
    {
        let mut delete = transaction
            .prepare("DELETE FROM dependencies WHERE source_file_id = ?1;")
            .map_err(|source| IndexError::PersistSymbols {
                path: index_path.to_path_buf(),
                source,
            })?;

        for file_id in file_ids.values() {
            delete
                .execute(params![file_id])
                .map_err(|source| IndexError::PersistSymbols {
                    path: index_path.to_path_buf(),
                    source,
                })?;
        }
    }

    let edges = dependency::resolve_dependencies(report);
    if edges.is_empty() {
        return Ok(());
    }

    let mut insert = transaction
        .prepare(
            "INSERT INTO dependencies (
                repo_id,
                source_file_id,
                target_file_id,
                target_path,
                kind,
                symbol_name,
                weight
            )
            VALUES (?1, ?2, ?3, ?4, ?5, NULL, 1.0);",
        )
        .map_err(|source| IndexError::PersistSymbols {
            path: index_path.to_path_buf(),
            source,
        })?;

    for edge in edges {
        let source_file_id = dependency_file_id(file_ids, index_path, "source", &edge.source_path)?;
        let target_file_id = dependency_file_id(file_ids, index_path, "target", &edge.target_path)?;

        insert
            .execute(params![
                repo_id,
                source_file_id,
                target_file_id,
                &edge.target_path,
                &edge.kind,
            ])
            .map_err(|source| IndexError::PersistSymbols {
                path: index_path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

fn dependency_file_id(
    file_ids: &BTreeMap<String, i64>,
    index_path: &Path,
    label: &str,
    path: &str,
) -> Result<i64, IndexError> {
    file_ids
        .get(path)
        .copied()
        .ok_or_else(|| IndexError::InvalidSymbolData {
            path: index_path.to_path_buf(),
            message: format!(
                "parser dependency {label} '{path}' is not in the current parse file scope"
            ),
        })
}

fn symbol_line_to_i64(
    value: u64,
    index_path: &Path,
    symbol: &ParseSymbolRecord,
) -> Result<i64, IndexError> {
    if value == 0 {
        return Err(IndexError::InvalidSymbolData {
            path: index_path.to_path_buf(),
            message: format!(
                "symbol '{}' in '{}' has line 0; parser symbol lines must be 1-based",
                symbol.name, symbol.path
            ),
        });
    }

    u64_to_i64_for_symbol(value, index_path, "symbol line")
}

fn symbol_sort_key(symbol: &ParseSymbolRecord) -> (&str, u64, u64, &str, &str) {
    (
        symbol.path.as_str(),
        symbol.start_line,
        symbol.end_line,
        symbol.kind.as_str(),
        symbol.name.as_str(),
    )
}

fn persisted_symbol_path(symbol: &ParseSymbolRecord) -> String {
    symbol.parent.as_ref().map_or_else(
        || symbol.name.clone(),
        |parent| format!("{parent}::{}", symbol.name),
    )
}

fn resolved_parent_symbol_id(
    symbol_paths: &BTreeMap<(String, String), Vec<SymbolLookup>>,
    file_path: &str,
    parent_path: &str,
    child_id: i64,
    child_start_line: u64,
    child_end_line: u64,
) -> Option<i64> {
    let candidates = symbol_paths
        .get(&(file_path.to_owned(), parent_path.to_owned()))?
        .iter()
        .copied()
        .filter(|candidate| candidate.id != child_id)
        .collect::<Vec<_>>();

    if candidates.len() == 1 {
        return Some(candidates[0].id);
    }

    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.line_start <= child_start_line && candidate.line_end >= child_end_line
        })
        .min_by_key(|candidate| {
            (
                candidate.line_end.saturating_sub(candidate.line_start),
                candidate.line_start,
                candidate.id,
            )
        })
        .map(|candidate| candidate.id)
}

fn persist_hotspot(
    file_id_query: &mut Statement<'_>,
    insert: &mut Statement<'_>,
    index_path: &Path,
    repo_id: i64,
    scan_run_id: i64,
    ranked_score: &RankedHotspotScore,
) -> Result<(), IndexError> {
    let score = &ranked_score.score;

    if !score.value.is_finite() || score.value < 0.0 {
        return Err(IndexError::InvalidHotspotData {
            path: index_path.to_path_buf(),
            message: format!(
                "hotspot score for '{}' must be a finite non-negative number",
                score.path
            ),
        });
    }

    let rank = u64_to_i64_for_hotspot(ranked_score.rank, index_path, "rank")?;
    let raw_metrics_json = hotspot_json(&score.raw_metrics, index_path, "raw_metrics_json")?;
    let explanation = hotspot_json(
        &HotspotExplanationPayload {
            normalized_metrics: &score.normalized_metrics,
            weighted_terms: &score.weighted_terms,
        },
        index_path,
        "explanation",
    )?;
    let limitation = hotspot_json(
        &HotspotLimitationsPayload {
            limitations: &score.limitations,
        },
        index_path,
        "limitation",
    )?;
    let file_id = file_id_query
        .query_row(params![repo_id, &score.path], |row| row.get::<_, i64>(0))
        .map_err(|source| IndexError::PersistHotspots {
            path: index_path.to_path_buf(),
            source,
        })?;

    insert
        .execute(params![
            file_id,
            repo_id,
            scan_run_id,
            score.value,
            rank,
            &score.formula_version.id,
            raw_metrics_json,
            explanation,
            limitation,
        ])
        .map_err(|source| IndexError::PersistHotspots {
            path: index_path.to_path_buf(),
            source,
        })?;

    Ok(())
}

fn next_run_key(
    transaction: &Transaction<'_>,
    path: &Path,
    repo_id: i64,
) -> Result<String, IndexError> {
    let next_run_number = transaction
        .query_row(
            "SELECT COUNT(*) + 1 FROM scan_runs WHERE repo_id = ?1;",
            params![repo_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|source| IndexError::PersistScan {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(format!("scan-{next_run_number:016}"))
}

fn prepare_statement<'transaction>(
    transaction: &'transaction Transaction<'_>,
    index_path: &Path,
    sql: &str,
) -> Result<Statement<'transaction>, IndexError> {
    transaction
        .prepare(sql)
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })
}

fn persist_file(
    file_upsert: &mut Statement<'_>,
    file_id_query: &mut Statement<'_>,
    index_path: &Path,
    repo_id: i64,
    scan_run_id: i64,
    file: &FileRecord,
) -> Result<i64, IndexError> {
    let byte_size = optional_u64_to_i64(file.byte_size, index_path, "byte_size")?;
    let line_count = optional_u64_to_i64(file.line_count, index_path, "line_count")?;

    file_upsert
        .execute(params![
            repo_id,
            &file.path,
            byte_size,
            file.extension.as_deref(),
            file.language,
            line_count,
            content_kind_to_index(file.content),
            file.is_vendor,
            file.is_generated,
            file.is_symlink,
            file.classification,
            scan_run_id,
        ])
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })?;

    file_id_query
        .query_row(params![repo_id, &file.path], |row| row.get(0))
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })
}

fn persist_scan_warnings(
    scan_warning_insert: &mut Statement<'_>,
    index_path: &Path,
    scan_run_id: i64,
    warnings: &[ScanWarning],
) -> Result<(), IndexError> {
    for (index, warning) in warnings.iter().enumerate() {
        let warning_order = warning_order_to_i64(index, index_path)?;
        scan_warning_insert
            .execute(params![
                scan_run_id,
                warning_order,
                warning.code,
                warning.path.as_deref(),
                &warning.message,
            ])
            .map_err(|source| IndexError::PersistScan {
                path: index_path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

fn persist_file_warnings(
    file_warning_insert: &mut Statement<'_>,
    index_path: &Path,
    file_id: i64,
    scan_run_id: i64,
    warnings: &[FileWarning],
) -> Result<(), IndexError> {
    for (index, warning) in warnings.iter().enumerate() {
        let warning_order = warning_order_to_i64(index, index_path)?;
        file_warning_insert
            .execute(params![
                file_id,
                scan_run_id,
                warning_order,
                warning.code,
                &warning.message,
            ])
            .map_err(|source| IndexError::PersistScan {
                path: index_path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

fn delete_stale_files(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    scan_run_id: i64,
) -> Result<(), IndexError> {
    transaction
        .execute(
            "DELETE FROM dependencies
             WHERE repo_id = ?1
               AND target_file_id IN (
                SELECT id
                FROM files
                WHERE repo_id = ?1
                  AND (scan_run_id IS NULL OR scan_run_id <> ?2)
             );",
            params![repo_id, scan_run_id],
        )
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })?;
    transaction
        .execute(
            "DELETE FROM files
             WHERE repo_id = ?1
               AND (scan_run_id IS NULL OR scan_run_id <> ?2);",
            params![repo_id, scan_run_id],
        )
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })?;

    Ok(())
}

fn read_persisted_scan_run(row: &Row<'_>) -> rusqlite::Result<PersistedScanRun> {
    Ok(PersistedScanRun {
        id: row.get(0)?,
        run_key: row.get(1)?,
        status: row.get(2)?,
        scanner_version: row.get(3)?,
        scan_schema_identifier: row.get(4)?,
        files_observed: optional_i64_to_u64(row.get(5)?, 5)?,
        warnings_observed: optional_i64_to_u64(row.get(6)?, 6)?,
    })
}

fn read_persisted_file_record(row: &Row<'_>) -> rusqlite::Result<(i64, PersistedFileRecord)> {
    let content_kind: String = row.get(8)?;

    Ok((
        row.get(0)?,
        PersistedFileRecord {
            path: row.get(1)?,
            byte_size: optional_i64_to_u64(row.get(2)?, 2)?,
            extension: row.get(3)?,
            language: row.get(4)?,
            line_count: optional_i64_to_u64(row.get(5)?, 5)?,
            is_vendor: row.get(6)?,
            is_generated: row.get(7)?,
            content: content_kind_from_index(&content_kind).map_err(|source| {
                rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(source))
            })?,
            is_symlink: row.get(9)?,
            classification: row.get(10)?,
            warnings: Vec::new(),
        },
    ))
}

fn read_scan_warnings(
    connection: &Connection,
    index_path: &Path,
    scan_run_id: i64,
) -> Result<Vec<PersistedScanWarning>, IndexError> {
    let mut statement = connection
        .prepare(
            "SELECT code, path, message
             FROM scan_warnings
             WHERE scan_run_id = ?1
             ORDER BY warning_order, code, path, message;",
        )
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map(params![scan_run_id], read_persisted_scan_warning)
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let mut warnings = Vec::new();

    for row in rows {
        warnings.push(row.map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?);
    }

    Ok(warnings)
}

fn read_file_warnings(
    connection: &Connection,
    index_path: &Path,
    scan_run_id: i64,
    file_id: i64,
) -> Result<Vec<PersistedFileWarning>, IndexError> {
    let mut statement = connection
        .prepare(
            "SELECT code, message
             FROM file_warnings
             WHERE scan_run_id = ?1 AND file_id = ?2
             ORDER BY warning_order, code, message;",
        )
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map(params![scan_run_id, file_id], read_persisted_file_warning)
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let mut warnings = Vec::new();

    for row in rows {
        warnings.push(row.map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?);
    }

    Ok(warnings)
}

fn read_persisted_git_analysis_run(row: &Row<'_>) -> rusqlite::Result<PersistedGitAnalysisRun> {
    Ok(PersistedGitAnalysisRun {
        id: row.get(0)?,
        analysis_key: row.get(1)?,
        status: row.get(2)?,
        analyzer_version: row.get(3)?,
        git_head: row.get(4)?,
        head_commit_time: row.get(5)?,
        recent_window_days: i64_to_u64(row.get(6)?, 6)?,
        metrics_observed: i64_to_u64(row.get(7)?, 7)?,
        co_changes_observed: i64_to_u64(row.get(8)?, 8)?,
    })
}

fn read_git_file_stats(
    connection: &Connection,
    index_path: &Path,
    analysis_run_id: i64,
) -> Result<Vec<PersistedGitFileStats>, IndexError> {
    let mut statement = connection
        .prepare(
            "SELECT
                files.path,
                git_file_stats.commits_per_file,
                git_file_stats.total_churn_added,
                git_file_stats.total_churn_deleted,
                git_file_stats.recent_churn_added,
                git_file_stats.recent_churn_deleted,
                git_file_stats.author_count,
                git_file_stats.owner_count,
                git_file_stats.dominant_owner,
                git_file_stats.dominant_owner_share,
                git_file_stats.first_commit_id,
                git_file_stats.first_commit_time,
                git_file_stats.last_commit_id,
                git_file_stats.last_commit_time,
                git_file_stats.file_age_days
             FROM git_file_stats
             INNER JOIN files ON files.id = git_file_stats.file_id
             WHERE git_file_stats.analysis_run_id = ?1
             ORDER BY files.path;",
        )
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map(params![analysis_run_id], read_persisted_git_file_stats)
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })
}

fn read_persisted_git_file_stats(row: &Row<'_>) -> rusqlite::Result<PersistedGitFileStats> {
    Ok(GitFileMetrics {
        path: row.get(0)?,
        commits_per_file: i64_to_u64(row.get(1)?, 1)?,
        total_churn_added: i64_to_u64(row.get(2)?, 2)?,
        total_churn_deleted: i64_to_u64(row.get(3)?, 3)?,
        recent_churn_added: i64_to_u64(row.get(4)?, 4)?,
        recent_churn_deleted: i64_to_u64(row.get(5)?, 5)?,
        author_count: i64_to_u64(row.get(6)?, 6)?,
        owner_count: i64_to_u64(row.get(7)?, 7)?,
        dominant_owner: row.get(8)?,
        dominant_owner_share: row.get(9)?,
        co_changed_file_count: 0,
        first_commit_id: row.get(10)?,
        first_commit_time: row.get(11)?,
        last_commit_id: row.get(12)?,
        last_commit_time: row.get(13)?,
        file_age_days: optional_i64_to_u64(row.get(14)?, 14)?,
    })
}

fn read_git_co_changes(
    connection: &Connection,
    index_path: &Path,
    analysis_run_id: i64,
) -> Result<Vec<PersistedGitCoChange>, IndexError> {
    let mut statement = connection
        .prepare(
            "SELECT left_path, right_path, commit_count
             FROM git_co_changes
             WHERE analysis_run_id = ?1
             ORDER BY commit_count DESC, left_path, right_path;",
        )
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map(params![analysis_run_id], read_persisted_git_co_change)
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::ReadIndex {
            path: index_path.to_path_buf(),
            source,
        })
}

fn read_persisted_git_co_change(row: &Row<'_>) -> rusqlite::Result<PersistedGitCoChange> {
    Ok(PersistedGitCoChange {
        left_path: row.get(0)?,
        right_path: row.get(1)?,
        commit_count: i64_to_u64(row.get(2)?, 2)?,
    })
}

fn read_persisted_hotspot(row: &Row<'_>) -> rusqlite::Result<PersistedHotspot> {
    Ok(PersistedHotspot {
        path: row.get(0)?,
        score: row.get(1)?,
        rank: i64_to_u64(row.get(2)?, 2)?,
        formula_version: row.get(3)?,
        raw_metrics_json: row.get(4)?,
        explanation: row.get(5)?,
        limitation: row.get(6)?,
    })
}

fn read_persisted_symbol(row: &Row<'_>) -> rusqlite::Result<PersistedSymbol> {
    Ok(PersistedSymbol {
        id: row.get(0)?,
        path: row.get(1)?,
        parent_symbol_id: row.get(2)?,
        name: row.get(3)?,
        kind: row.get(4)?,
        line_start: optional_i64_to_u64(row.get(5)?, 5)?,
        line_end: optional_i64_to_u64(row.get(6)?, 6)?,
        signature: row.get(7)?,
    })
}

fn read_persisted_scan_warning(row: &Row<'_>) -> rusqlite::Result<PersistedScanWarning> {
    Ok(PersistedScanWarning {
        code: row.get(0)?,
        path: row.get(1)?,
        message: row.get(2)?,
    })
}

fn read_persisted_file_warning(row: &Row<'_>) -> rusqlite::Result<PersistedFileWarning> {
    Ok(PersistedFileWarning {
        code: row.get(0)?,
        message: row.get(1)?,
    })
}

fn optional_u64_to_i64(
    value: Option<u64>,
    path: &Path,
    field_name: &'static str,
) -> Result<Option<i64>, IndexError> {
    value
        .map(|value| u64_to_i64(value, path, field_name))
        .transpose()
}

fn u64_to_i64(value: u64, path: &Path, field_name: &'static str) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidScanData {
        path: path.to_path_buf(),
        message: format!("{field_name} value {value} exceeds SQLite INTEGER range"),
    })
}

fn optional_u64_to_i64_for_git(
    value: Option<u64>,
    path: &Path,
    field_name: &'static str,
) -> Result<Option<i64>, IndexError> {
    value
        .map(|value| u64_to_i64_for_git(value, path, field_name))
        .transpose()
}

fn u64_to_i64_for_git(
    value: u64,
    path: &Path,
    field_name: &'static str,
) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidGitAnalysisData {
        path: path.to_path_buf(),
        message: format!("{field_name} value {value} exceeds SQLite INTEGER range"),
    })
}

fn u64_to_i64_for_symbol(
    value: u64,
    path: &Path,
    field_name: &'static str,
) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidSymbolData {
        path: path.to_path_buf(),
        message: format!("{field_name} value {value} exceeds SQLite INTEGER range"),
    })
}

fn u64_to_i64_for_hotspot(
    value: u64,
    path: &Path,
    field_name: &'static str,
) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidHotspotData {
        path: path.to_path_buf(),
        message: format!("{field_name} value {value} exceeds SQLite INTEGER range"),
    })
}

fn hotspot_json<T: Serialize>(
    value: &T,
    path: &Path,
    field_name: &'static str,
) -> Result<String, IndexError> {
    serde_json::to_string(value).map_err(|source| IndexError::InvalidHotspotData {
        path: path.to_path_buf(),
        message: format!("failed to serialize {field_name} as JSON: {source}"),
    })
}

fn usize_to_i64_for_git(
    value: usize,
    path: &Path,
    field_name: &'static str,
) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidGitAnalysisData {
        path: path.to_path_buf(),
        message: format!("{field_name} value {value} exceeds SQLite INTEGER range"),
    })
}

fn warning_order_to_i64(value: usize, path: &Path) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| IndexError::InvalidScanData {
        path: path.to_path_buf(),
        message: format!("warning_order value {value} exceeds SQLite INTEGER range"),
    })
}

fn optional_i64_to_u64(value: Option<i64>, column: usize) -> rusqlite::Result<Option<u64>> {
    value.map(|value| i64_to_u64(value, column)).transpose()
}

fn i64_to_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Integer, Box::new(source))
    })
}

fn content_kind_to_index(content: ContentKind) -> &'static str {
    match content {
        ContentKind::Text => "text",
        ContentKind::Binary => "binary",
        ContentKind::Unknown => "unknown",
    }
}

fn content_kind_from_index(value: &str) -> Result<ContentKind, InvalidContentKind> {
    match value {
        "text" => Ok(ContentKind::Text),
        "binary" => Ok(ContentKind::Binary),
        "unknown" => Ok(ContentKind::Unknown),
        _ => Err(InvalidContentKind {
            value: value.to_owned(),
        }),
    }
}

#[derive(Debug)]
struct InvalidContentKind {
    value: String,
}

impl fmt::Display for InvalidContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid indexed content kind '{}'", self.value)
    }
}

impl StdError for InvalidContentKind {}

fn ensure_index_dir(path: &Path) -> Result<(), IndexError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == ErrorKind::AlreadyExists => {
            ensure_existing_index_dir(path, source)
        }
        Err(source) => Err(IndexError::CreateIndexDir {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn existing_index_dir_is_safe(path: &Path) -> Result<bool, IndexError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(IndexError::AccessIndex {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if is_redirecting_path(&metadata) {
        return Err(IndexError::UnsafeIndexDir {
            path: path.to_path_buf(),
            message: "existing .hotpath directory is a symlink or filesystem reparse point"
                .to_owned(),
        });
    }

    if metadata.file_type().is_dir() {
        Ok(true)
    } else {
        Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "existing .hotpath path is not a directory".to_owned(),
        })
    }
}

fn ensure_existing_index_dir(path: &Path, source: std::io::Error) -> Result<(), IndexError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| IndexError::CreateIndexDir {
        path: path.to_path_buf(),
        source,
    })?;

    if is_redirecting_path(&metadata) {
        return Err(IndexError::UnsafeIndexDir {
            path: path.to_path_buf(),
            message: "existing .hotpath directory is a symlink or filesystem reparse point"
                .to_owned(),
        });
    }

    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(IndexError::CreateIndexDir {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn is_redirecting_path(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

fn enable_foreign_keys(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

fn migrate_to_current(
    connection: &mut Connection,
    path: &Path,
    starting_version: u32,
) -> Result<(), IndexError> {
    let mut version = starting_version;

    while version < CURRENT_SCHEMA_VERSION {
        match version {
            0 => {
                migrate_0_to_1(connection, path)?;
                version = 1;
            }
            1 => {
                migrate_1_to_2(connection, path)?;
                version = 2;
            }
            2 => {
                migrate_2_to_3(connection, path)?;
                version = 3;
            }
            3 => {
                migrate_3_to_4(connection, path)?;
                version = 4;
            }
            _ => {
                return Err(IndexError::CorruptMetadata {
                    path: path.to_path_buf(),
                    message: format!("unsupported historical schema version {version}"),
                });
            }
        }
    }

    Ok(())
}

fn migrate_2_to_3(connection: &mut Connection, path: &Path) -> Result<(), IndexError> {
    let transaction = connection
        .transaction()
        .map_err(|source| migration_error(path, 2, 3, source))?;

    transaction
        .execute_batch(
            "
            ALTER TABLE git_file_stats
                ADD COLUMN owner_count INTEGER NOT NULL DEFAULT 0;
            ",
        )
        .map_err(|source| migration_error(path, 2, 3, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION.to_string()],
        )
        .map_err(|source| migration_error(path, 2, 3, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER_V3],
        )
        .map_err(|source| migration_error(path, 2, 3, source))?;
    transaction
        .execute_batch("PRAGMA user_version = 3;")
        .map_err(|source| migration_error(path, 2, 3, source))?;
    transaction
        .commit()
        .map_err(|source| migration_error(path, 2, 3, source))?;

    Ok(())
}

fn migrate_3_to_4(connection: &mut Connection, path: &Path) -> Result<(), IndexError> {
    let transaction = connection
        .transaction()
        .map_err(|source| migration_error(path, 3, 4, source))?;

    create_cache_schema_v4(&transaction, path).map_err(|error| match error {
        IndexError::Migration {
            from_version: _,
            to_version: _,
            source,
            ..
        } => migration_error(path, 3, 4, source),
        other => other,
    })?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION.to_string()],
        )
        .map_err(|source| migration_error(path, 3, 4, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER],
        )
        .map_err(|source| migration_error(path, 3, 4, source))?;
    transaction
        .execute_batch("PRAGMA user_version = 4;")
        .map_err(|source| migration_error(path, 3, 4, source))?;
    transaction
        .commit()
        .map_err(|source| migration_error(path, 3, 4, source))?;

    Ok(())
}

fn migrate_0_to_1(connection: &mut Connection, path: &Path) -> Result<(), IndexError> {
    let transaction = connection
        .transaction()
        .map_err(|source| migration_error(path, 0, 1, source))?;

    if metadata_object_exists(&transaction, path)? {
        verify_metadata_table_shape(&transaction, path)?;
        verify_existing_metadata_before_initial_migration(&transaction, path)?;
    } else {
        transaction
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            ) STRICT;",
            )
            .map_err(|source| migration_error(path, 0, 1, source))?;
    }

    create_core_schema_v1(&transaction, path)?;

    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, "1"],
        )
        .map_err(|source| migration_error(path, 0, 1, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER_V1],
        )
        .map_err(|source| migration_error(path, 0, 1, source))?;
    transaction
        .execute_batch("PRAGMA user_version = 1;")
        .map_err(|source| migration_error(path, 0, 1, source))?;
    transaction
        .commit()
        .map_err(|source| migration_error(path, 0, 1, source))?;

    Ok(())
}

fn migrate_1_to_2(connection: &mut Connection, path: &Path) -> Result<(), IndexError> {
    let transaction = connection
        .transaction()
        .map_err(|source| migration_error(path, 1, 2, source))?;

    create_git_schema_v2(&transaction, path)?;

    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, "2"],
        )
        .map_err(|source| migration_error(path, 1, 2, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER_V2],
        )
        .map_err(|source| migration_error(path, 1, 2, source))?;
    transaction
        .execute_batch("PRAGMA user_version = 2;")
        .map_err(|source| migration_error(path, 1, 2, source))?;
    transaction
        .commit()
        .map_err(|source| migration_error(path, 1, 2, source))?;

    Ok(())
}

fn create_core_schema_v1(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS repos (
                id INTEGER PRIMARY KEY,
                root_key TEXT NOT NULL UNIQUE,
                display_name TEXT,
                default_branch TEXT,
                head_commit TEXT,
                CHECK (length(root_key) > 0)
            ) STRICT;

            CREATE TABLE IF NOT EXISTS scan_runs (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                run_key TEXT NOT NULL,
                status TEXT NOT NULL,
                scanner_version TEXT,
                scan_schema_identifier TEXT,
                config_hash TEXT,
                git_head TEXT,
                files_observed INTEGER,
                warnings_observed INTEGER,
                UNIQUE (repo_id, run_key),
                CHECK (length(run_key) > 0),
                CHECK (status IN ('started', 'completed', 'failed')),
                CHECK (files_observed IS NULL OR files_observed >= 0),
                CHECK (warnings_observed IS NULL OR warnings_observed >= 0),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS scan_warnings (
                id INTEGER PRIMARY KEY,
                scan_run_id INTEGER NOT NULL,
                warning_order INTEGER NOT NULL,
                code TEXT NOT NULL,
                path TEXT,
                message TEXT NOT NULL,
                UNIQUE (scan_run_id, warning_order),
                CHECK (warning_order >= 0),
                CHECK (length(code) > 0),
                CHECK (path IS NULL OR length(path) > 0),
                CHECK (path IS NULL OR path != '..'),
                CHECK (path IS NULL OR path NOT LIKE '/%'),
                CHECK (path IS NULL OR path NOT LIKE './%'),
                CHECK (path IS NULL OR path NOT LIKE '../%'),
                CHECK (path IS NULL OR path NOT LIKE '%/../%'),
                CHECK (path IS NULL OR path NOT LIKE '%/..'),
                CHECK (path IS NULL OR path NOT LIKE '~%'),
                CHECK (path IS NULL OR path NOT GLOB '[A-Za-z]:*'),
                CHECK (path IS NULL OR instr(path, '\\') = 0),
                CHECK (path IS NULL OR instr(path, char(0)) = 0),
                CHECK (length(message) > 0),
                FOREIGN KEY (scan_run_id) REFERENCES scan_runs(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                byte_size INTEGER,
                extension TEXT,
                language TEXT,
                line_count INTEGER,
                content_kind TEXT,
                is_vendor INTEGER NOT NULL DEFAULT 0,
                is_generated INTEGER NOT NULL DEFAULT 0,
                is_symlink INTEGER NOT NULL DEFAULT 0,
                classification TEXT,
                scan_run_id INTEGER,
                UNIQUE (repo_id, path),
                CHECK (length(path) > 0),
                CHECK (path != '..'),
                CHECK (path NOT LIKE '/%'),
                CHECK (path NOT LIKE './%'),
                CHECK (path NOT LIKE '../%'),
                CHECK (path NOT LIKE '%/../%'),
                CHECK (path NOT LIKE '%/..'),
                CHECK (path NOT LIKE '~%'),
                CHECK (path NOT GLOB '[A-Za-z]:*'),
                CHECK (instr(path, '\\') = 0),
                CHECK (instr(path, char(0)) = 0),
                CHECK (byte_size IS NULL OR byte_size >= 0),
                CHECK (line_count IS NULL OR line_count >= 0),
                CHECK (content_kind IS NULL OR content_kind IN ('text', 'binary', 'unknown')),
                CHECK (is_vendor IN (0, 1)),
                CHECK (is_generated IN (0, 1)),
                CHECK (is_symlink IN (0, 1)),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (scan_run_id) REFERENCES scan_runs(id) ON DELETE SET NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS file_warnings (
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL,
                scan_run_id INTEGER NOT NULL,
                warning_order INTEGER NOT NULL,
                code TEXT NOT NULL,
                message TEXT NOT NULL,
                UNIQUE (file_id, scan_run_id, warning_order),
                CHECK (warning_order >= 0),
                CHECK (length(code) > 0),
                CHECK (length(message) > 0),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (scan_run_id) REFERENCES scan_runs(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL,
                parent_symbol_id INTEGER,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                line_start INTEGER,
                line_end INTEGER,
                signature TEXT,
                UNIQUE (file_id, kind, name, line_start, line_end),
                CHECK (length(name) > 0),
                CHECK (length(kind) > 0),
                CHECK (line_start IS NULL OR line_start >= 1),
                CHECK (line_end IS NULL OR line_end >= line_start),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (parent_symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS git_file_stats (
                file_id INTEGER PRIMARY KEY,
                commit_count INTEGER NOT NULL DEFAULT 0,
                churn_added INTEGER NOT NULL DEFAULT 0,
                churn_deleted INTEGER NOT NULL DEFAULT 0,
                author_count INTEGER NOT NULL DEFAULT 0,
                primary_author TEXT,
                last_commit TEXT,
                CHECK (commit_count >= 0),
                CHECK (churn_added >= 0),
                CHECK (churn_deleted >= 0),
                CHECK (author_count >= 0),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS dependencies (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                source_file_id INTEGER NOT NULL,
                target_file_id INTEGER,
                target_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                symbol_name TEXT,
                weight REAL,
                UNIQUE (source_file_id, target_path, kind, symbol_name),
                CHECK (length(target_path) > 0),
                CHECK (target_path != '..'),
                CHECK (target_path NOT LIKE '/%'),
                CHECK (target_path NOT LIKE './%'),
                CHECK (target_path NOT LIKE '../%'),
                CHECK (target_path NOT LIKE '%/../%'),
                CHECK (target_path NOT LIKE '%/..'),
                CHECK (target_path NOT LIKE '~%'),
                CHECK (target_path NOT GLOB '[A-Za-z]:*'),
                CHECK (instr(target_path, '\\') = 0),
                CHECK (instr(target_path, char(0)) = 0),
                CHECK (length(kind) > 0),
                CHECK (weight IS NULL OR weight >= 0.0),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (source_file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (target_file_id) REFERENCES files(id) ON DELETE SET NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS hotspots (
                file_id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                scan_run_id INTEGER,
                score REAL NOT NULL,
                rank INTEGER,
                formula_version TEXT NOT NULL,
                raw_metrics_json TEXT,
                explanation TEXT,
                limitation TEXT,
                CHECK (score >= 0.0),
                CHECK (rank IS NULL OR rank >= 1),
                CHECK (length(formula_version) > 0),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (scan_run_id) REFERENCES scan_runs(id) ON DELETE SET NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS scan_runs_by_repo_run_key
                ON scan_runs (repo_id, run_key);
            CREATE INDEX IF NOT EXISTS scan_warnings_by_scan_order
                ON scan_warnings (scan_run_id, warning_order);
            CREATE INDEX IF NOT EXISTS files_by_repo_path
                ON files (repo_id, path);
            CREATE INDEX IF NOT EXISTS file_warnings_by_scan_file_order
                ON file_warnings (scan_run_id, file_id, warning_order);
            CREATE INDEX IF NOT EXISTS symbols_by_file_order
                ON symbols (file_id, line_start, line_end, kind, name);
            CREATE INDEX IF NOT EXISTS dependencies_by_source
                ON dependencies (source_file_id, kind, target_path);
            CREATE INDEX IF NOT EXISTS dependencies_by_target
                ON dependencies (target_file_id, kind, source_file_id);
            CREATE INDEX IF NOT EXISTS hotspots_by_repo_rank
                ON hotspots (repo_id, rank, file_id);
            CREATE INDEX IF NOT EXISTS hotspots_by_repo_score
                ON hotspots (repo_id, score DESC, file_id);
            ",
        )
        .map_err(|source| migration_error(path, 0, 1, source))
}

fn create_git_schema_v2(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    connection
        .execute_batch(
            "
            DROP TABLE IF EXISTS git_file_stats;

            CREATE TABLE IF NOT EXISTS git_analysis_runs (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                analysis_key TEXT NOT NULL,
                status TEXT NOT NULL,
                analyzer_version TEXT,
                git_head TEXT NOT NULL,
                head_commit_time INTEGER NOT NULL,
                recent_window_days INTEGER NOT NULL,
                metrics_observed INTEGER NOT NULL,
                co_changes_observed INTEGER NOT NULL,
                UNIQUE (repo_id, analysis_key),
                CHECK (length(analysis_key) > 0),
                CHECK (status IN ('completed')),
                CHECK (length(git_head) > 0),
                CHECK (recent_window_days >= 0),
                CHECK (metrics_observed >= 0),
                CHECK (co_changes_observed >= 0),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS git_file_stats (
                file_id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                analysis_run_id INTEGER NOT NULL,
                commits_per_file INTEGER NOT NULL DEFAULT 0,
                total_churn_added INTEGER NOT NULL DEFAULT 0,
                total_churn_deleted INTEGER NOT NULL DEFAULT 0,
                recent_churn_added INTEGER NOT NULL DEFAULT 0,
                recent_churn_deleted INTEGER NOT NULL DEFAULT 0,
                author_count INTEGER NOT NULL DEFAULT 0,
                dominant_owner TEXT,
                dominant_owner_share REAL,
                first_commit_id TEXT,
                first_commit_time INTEGER,
                last_commit_id TEXT,
                last_commit_time INTEGER,
                file_age_days INTEGER,
                CHECK (commits_per_file >= 0),
                CHECK (total_churn_added >= 0),
                CHECK (total_churn_deleted >= 0),
                CHECK (recent_churn_added >= 0),
                CHECK (recent_churn_deleted >= 0),
                CHECK (author_count >= 0),
                CHECK (dominant_owner_share IS NULL OR (dominant_owner_share >= 0.0 AND dominant_owner_share <= 1.0)),
                CHECK (file_age_days IS NULL OR file_age_days >= 0),
                FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (analysis_run_id) REFERENCES git_analysis_runs(id) ON DELETE CASCADE
            ) STRICT;

            CREATE TABLE IF NOT EXISTS git_co_changes (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                analysis_run_id INTEGER NOT NULL,
                left_file_id INTEGER NOT NULL,
                right_file_id INTEGER NOT NULL,
                left_path TEXT NOT NULL,
                right_path TEXT NOT NULL,
                commit_count INTEGER NOT NULL,
                UNIQUE (repo_id, left_path, right_path),
                CHECK (length(left_path) > 0),
                CHECK (left_path != '..'),
                CHECK (left_path NOT LIKE '/%'),
                CHECK (left_path NOT LIKE './%'),
                CHECK (left_path NOT LIKE '../%'),
                CHECK (left_path NOT LIKE '%/../%'),
                CHECK (left_path NOT LIKE '%/..'),
                CHECK (left_path NOT LIKE '~%'),
                CHECK (left_path NOT GLOB '[A-Za-z]:*'),
                CHECK (instr(left_path, '\\') = 0),
                CHECK (instr(left_path, char(0)) = 0),
                CHECK (length(right_path) > 0),
                CHECK (right_path != '..'),
                CHECK (right_path NOT LIKE '/%'),
                CHECK (right_path NOT LIKE './%'),
                CHECK (right_path NOT LIKE '../%'),
                CHECK (right_path NOT LIKE '%/../%'),
                CHECK (right_path NOT LIKE '%/..'),
                CHECK (right_path NOT LIKE '~%'),
                CHECK (right_path NOT GLOB '[A-Za-z]:*'),
                CHECK (instr(right_path, '\\') = 0),
                CHECK (instr(right_path, char(0)) = 0),
                CHECK (left_path < right_path),
                CHECK (commit_count >= 0),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (analysis_run_id) REFERENCES git_analysis_runs(id) ON DELETE CASCADE,
                FOREIGN KEY (left_file_id) REFERENCES files(id) ON DELETE CASCADE,
                FOREIGN KEY (right_file_id) REFERENCES files(id) ON DELETE CASCADE
            ) STRICT;

            CREATE INDEX IF NOT EXISTS git_analysis_runs_by_repo_key
                ON git_analysis_runs (repo_id, analysis_key);
            CREATE INDEX IF NOT EXISTS git_file_stats_by_repo_analysis
                ON git_file_stats (repo_id, analysis_run_id, file_id);
            CREATE INDEX IF NOT EXISTS git_co_changes_by_repo_rank
                ON git_co_changes (repo_id, commit_count DESC, left_path, right_path);
            ",
        )
        .map_err(|source| migration_error(path, 1, 2, source))
}

fn create_cache_schema_v4(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS git_commit_diffs (
                repo_id INTEGER NOT NULL,
                commit_id TEXT NOT NULL,
                analyzer_version TEXT NOT NULL,
                changes_json TEXT NOT NULL,
                PRIMARY KEY (repo_id, commit_id, analyzer_version),
                CHECK (length(commit_id) > 0),
                CHECK (length(analyzer_version) > 0),
                CHECK (length(changes_json) > 0),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE
            ) STRICT;

            CREATE INDEX IF NOT EXISTS git_commit_diffs_by_repo_commit
                ON git_commit_diffs (repo_id, commit_id);
            ",
        )
        .map_err(|source| migration_error(path, 3, 4, source))
}

fn migration_error(
    path: &Path,
    from_version: u32,
    to_version: u32,
    source: rusqlite::Error,
) -> IndexError {
    IndexError::Migration {
        path: path.to_path_buf(),
        from_version,
        to_version,
        source,
    }
}

fn read_user_version(connection: &Connection, path: &Path) -> Result<u32, IndexError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    u32::try_from(version).map_err(|_| IndexError::CorruptMetadata {
        path: path.to_path_buf(),
        message: format!("schema version {version} is outside the supported range"),
    })
}

fn verify_database_integrity(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    let result: String = connection
        .query_row("PRAGMA quick_check;", [], |row| row.get(0))
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    if result == "ok" {
        Ok(())
    } else {
        Err(IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!("SQLite quick_check failed: {result}")),
            ),
        })
    }
}

fn verify_metadata(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    verify_database_integrity(connection, path)?;
    verify_schema_objects(connection, path)?;

    let user_version = read_user_version(connection, path)?;
    let metadata_version = read_metadata_schema_version(connection, path)?;
    let metadata_identifier = read_metadata_schema_identifier(connection, path)?;

    if metadata_version > CURRENT_SCHEMA_VERSION {
        return Err(IndexError::IncompatibleFutureSchema {
            path: path.to_path_buf(),
            found_version: metadata_version,
            supported_version: CURRENT_SCHEMA_VERSION,
        });
    }

    if metadata_version != user_version {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "metadata schema version {metadata_version} does not match SQLite user_version {user_version}"
            ),
        });
    }

    if metadata_version != CURRENT_SCHEMA_VERSION {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "schema version {metadata_version} was not migrated to {CURRENT_SCHEMA_VERSION}"
            ),
        });
    }

    if metadata_identifier != SCHEMA_IDENTIFIER {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "metadata schema identifier '{metadata_identifier}' does not match expected {SCHEMA_IDENTIFIER}"
            ),
        });
    }

    Ok(())
}

fn verify_schema_objects(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    for table_name in REQUIRED_SCHEMA_TABLES {
        verify_table_shape(connection, path, table_name, expected_columns(table_name))?;
        verify_required_check_constraints(
            connection,
            path,
            table_name,
            expected_check_constraints(table_name),
        )?;
        verify_unique_constraints(
            connection,
            path,
            table_name,
            expected_unique_constraints(table_name),
        )?;
        verify_foreign_keys(
            connection,
            path,
            table_name,
            expected_foreign_keys(table_name),
        )?;
    }

    verify_required_indexes(connection, path)?;

    Ok(())
}

fn verify_existing_metadata_before_initial_migration(
    connection: &Connection,
    path: &Path,
) -> Result<(), IndexError> {
    let metadata_version = read_metadata_schema_version(connection, path)?;
    let metadata_identifier = read_metadata_schema_identifier(connection, path)?;

    if metadata_version > CURRENT_SCHEMA_VERSION {
        return Err(IndexError::IncompatibleFutureSchema {
            path: path.to_path_buf(),
            found_version: metadata_version,
            supported_version: CURRENT_SCHEMA_VERSION,
        });
    }

    if metadata_identifier != SCHEMA_IDENTIFIER_V1
        && metadata_identifier != SCHEMA_IDENTIFIER_V2
        && metadata_identifier != SCHEMA_IDENTIFIER_V3
        && metadata_identifier != SCHEMA_IDENTIFIER
    {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "metadata schema identifier '{metadata_identifier}' does not match expected {SCHEMA_IDENTIFIER_V1}"
            ),
        });
    }

    Ok(())
}

fn metadata_object_exists(connection: &Connection, path: &Path) -> Result<bool, IndexError> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE name = ?1;",
            params!["hotpath_metadata"],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug)]
struct MetadataTableSummary {
    table_type: String,
    column_count: i64,
    without_rowid: bool,
    strict: bool,
}

#[derive(Debug)]
struct MetadataColumn {
    name: String,
    data_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
    hidden: i64,
}

#[derive(Debug)]
struct ForeignKeyColumn {
    target_table: String,
    from_column: String,
    target_column: String,
    on_delete: String,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedColumn {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
    hidden: i64,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedForeignKey {
    from_column: &'static str,
    target_table: &'static str,
    target_column: &'static str,
    on_delete: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedUniqueConstraint {
    columns: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
struct ExpectedIndex {
    name: &'static str,
    table_name: &'static str,
    columns: &'static [&'static str],
}

const HOTPATH_METADATA_COLUMNS: &[ExpectedColumn] = &[
    expected_column("key", "TEXT", true, None, 1),
    expected_column("value", "TEXT", true, None, 0),
];

const REPOS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("root_key", "TEXT", true, None, 0),
    expected_column("display_name", "TEXT", false, None, 0),
    expected_column("default_branch", "TEXT", false, None, 0),
    expected_column("head_commit", "TEXT", false, None, 0),
];

const SCAN_RUNS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("run_key", "TEXT", true, None, 0),
    expected_column("status", "TEXT", true, None, 0),
    expected_column("scanner_version", "TEXT", false, None, 0),
    expected_column("scan_schema_identifier", "TEXT", false, None, 0),
    expected_column("config_hash", "TEXT", false, None, 0),
    expected_column("git_head", "TEXT", false, None, 0),
    expected_column("files_observed", "INTEGER", false, None, 0),
    expected_column("warnings_observed", "INTEGER", false, None, 0),
];

const SCAN_WARNINGS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("scan_run_id", "INTEGER", true, None, 0),
    expected_column("warning_order", "INTEGER", true, None, 0),
    expected_column("code", "TEXT", true, None, 0),
    expected_column("path", "TEXT", false, None, 0),
    expected_column("message", "TEXT", true, None, 0),
];

const FILES_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("path", "TEXT", true, None, 0),
    expected_column("byte_size", "INTEGER", false, None, 0),
    expected_column("extension", "TEXT", false, None, 0),
    expected_column("language", "TEXT", false, None, 0),
    expected_column("line_count", "INTEGER", false, None, 0),
    expected_column("content_kind", "TEXT", false, None, 0),
    expected_column("is_vendor", "INTEGER", true, Some("0"), 0),
    expected_column("is_generated", "INTEGER", true, Some("0"), 0),
    expected_column("is_symlink", "INTEGER", true, Some("0"), 0),
    expected_column("classification", "TEXT", false, None, 0),
    expected_column("scan_run_id", "INTEGER", false, None, 0),
];

const FILE_WARNINGS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("file_id", "INTEGER", true, None, 0),
    expected_column("scan_run_id", "INTEGER", true, None, 0),
    expected_column("warning_order", "INTEGER", true, None, 0),
    expected_column("code", "TEXT", true, None, 0),
    expected_column("message", "TEXT", true, None, 0),
];

const SYMBOLS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("file_id", "INTEGER", true, None, 0),
    expected_column("parent_symbol_id", "INTEGER", false, None, 0),
    expected_column("name", "TEXT", true, None, 0),
    expected_column("kind", "TEXT", true, None, 0),
    expected_column("line_start", "INTEGER", false, None, 0),
    expected_column("line_end", "INTEGER", false, None, 0),
    expected_column("signature", "TEXT", false, None, 0),
];

const GIT_ANALYSIS_RUNS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("analysis_key", "TEXT", true, None, 0),
    expected_column("status", "TEXT", true, None, 0),
    expected_column("analyzer_version", "TEXT", false, None, 0),
    expected_column("git_head", "TEXT", true, None, 0),
    expected_column("head_commit_time", "INTEGER", true, None, 0),
    expected_column("recent_window_days", "INTEGER", true, None, 0),
    expected_column("metrics_observed", "INTEGER", true, None, 0),
    expected_column("co_changes_observed", "INTEGER", true, None, 0),
];

const GIT_FILE_STATS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("file_id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("analysis_run_id", "INTEGER", true, None, 0),
    expected_column("commits_per_file", "INTEGER", true, Some("0"), 0),
    expected_column("total_churn_added", "INTEGER", true, Some("0"), 0),
    expected_column("total_churn_deleted", "INTEGER", true, Some("0"), 0),
    expected_column("recent_churn_added", "INTEGER", true, Some("0"), 0),
    expected_column("recent_churn_deleted", "INTEGER", true, Some("0"), 0),
    expected_column("author_count", "INTEGER", true, Some("0"), 0),
    expected_column("dominant_owner", "TEXT", false, None, 0),
    expected_column("dominant_owner_share", "REAL", false, None, 0),
    expected_column("first_commit_id", "TEXT", false, None, 0),
    expected_column("first_commit_time", "INTEGER", false, None, 0),
    expected_column("last_commit_id", "TEXT", false, None, 0),
    expected_column("last_commit_time", "INTEGER", false, None, 0),
    expected_column("file_age_days", "INTEGER", false, None, 0),
    expected_column("owner_count", "INTEGER", true, Some("0"), 0),
];

const GIT_CO_CHANGES_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("analysis_run_id", "INTEGER", true, None, 0),
    expected_column("left_file_id", "INTEGER", true, None, 0),
    expected_column("right_file_id", "INTEGER", true, None, 0),
    expected_column("left_path", "TEXT", true, None, 0),
    expected_column("right_path", "TEXT", true, None, 0),
    expected_column("commit_count", "INTEGER", true, None, 0),
];

const DEPENDENCIES_COLUMNS: &[ExpectedColumn] = &[
    expected_column("id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("source_file_id", "INTEGER", true, None, 0),
    expected_column("target_file_id", "INTEGER", false, None, 0),
    expected_column("target_path", "TEXT", true, None, 0),
    expected_column("kind", "TEXT", true, None, 0),
    expected_column("symbol_name", "TEXT", false, None, 0),
    expected_column("weight", "REAL", false, None, 0),
];

const HOTSPOTS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("file_id", "INTEGER", false, None, 1),
    expected_column("repo_id", "INTEGER", true, None, 0),
    expected_column("scan_run_id", "INTEGER", false, None, 0),
    expected_column("score", "REAL", true, None, 0),
    expected_column("rank", "INTEGER", false, None, 0),
    expected_column("formula_version", "TEXT", true, None, 0),
    expected_column("raw_metrics_json", "TEXT", false, None, 0),
    expected_column("explanation", "TEXT", false, None, 0),
    expected_column("limitation", "TEXT", false, None, 0),
];

const GIT_COMMIT_DIFFS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("repo_id", "INTEGER", true, None, 1),
    expected_column("commit_id", "TEXT", true, None, 2),
    expected_column("analyzer_version", "TEXT", true, None, 3),
    expected_column("changes_json", "TEXT", true, None, 0),
];

const NO_FOREIGN_KEYS: &[ExpectedForeignKey] = &[];
const NO_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] = &[];
const NO_CHECK_CONSTRAINTS: &[&str] = &[];

const SCAN_RUNS_FOREIGN_KEYS: &[ExpectedForeignKey] =
    &[expected_foreign_key("repo_id", "repos", "id", "CASCADE")];
const SCAN_WARNINGS_FOREIGN_KEYS: &[ExpectedForeignKey] = &[expected_foreign_key(
    "scan_run_id",
    "scan_runs",
    "id",
    "CASCADE",
)];

const FILES_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("repo_id", "repos", "id", "CASCADE"),
    expected_foreign_key("scan_run_id", "scan_runs", "id", "SET NULL"),
];
const FILE_WARNINGS_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("file_id", "files", "id", "CASCADE"),
    expected_foreign_key("scan_run_id", "scan_runs", "id", "CASCADE"),
];

const SYMBOLS_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("file_id", "files", "id", "CASCADE"),
    expected_foreign_key("parent_symbol_id", "symbols", "id", "CASCADE"),
];

const GIT_ANALYSIS_RUNS_FOREIGN_KEYS: &[ExpectedForeignKey] =
    &[expected_foreign_key("repo_id", "repos", "id", "CASCADE")];

const GIT_FILE_STATS_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("file_id", "files", "id", "CASCADE"),
    expected_foreign_key("repo_id", "repos", "id", "CASCADE"),
    expected_foreign_key("analysis_run_id", "git_analysis_runs", "id", "CASCADE"),
];

const GIT_CO_CHANGES_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("repo_id", "repos", "id", "CASCADE"),
    expected_foreign_key("analysis_run_id", "git_analysis_runs", "id", "CASCADE"),
    expected_foreign_key("left_file_id", "files", "id", "CASCADE"),
    expected_foreign_key("right_file_id", "files", "id", "CASCADE"),
];

const DEPENDENCIES_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("repo_id", "repos", "id", "CASCADE"),
    expected_foreign_key("source_file_id", "files", "id", "CASCADE"),
    expected_foreign_key("target_file_id", "files", "id", "SET NULL"),
];

const HOTSPOTS_FOREIGN_KEYS: &[ExpectedForeignKey] = &[
    expected_foreign_key("file_id", "files", "id", "CASCADE"),
    expected_foreign_key("repo_id", "repos", "id", "CASCADE"),
    expected_foreign_key("scan_run_id", "scan_runs", "id", "SET NULL"),
];
const GIT_COMMIT_DIFFS_FOREIGN_KEYS: &[ExpectedForeignKey] =
    &[expected_foreign_key("repo_id", "repos", "id", "CASCADE")];

const REPOS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] =
    &[expected_unique_constraint(&["root_key"])];
const SCAN_RUNS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] =
    &[expected_unique_constraint(&["repo_id", "run_key"])];
const SCAN_WARNINGS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] = &[
    expected_unique_constraint(&["scan_run_id", "warning_order"]),
];
const FILES_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] =
    &[expected_unique_constraint(&["repo_id", "path"])];
const FILE_WARNINGS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] = &[
    expected_unique_constraint(&["file_id", "scan_run_id", "warning_order"]),
];
const SYMBOLS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] = &[expected_unique_constraint(&[
    "file_id",
    "kind",
    "name",
    "line_start",
    "line_end",
])];
const GIT_ANALYSIS_RUNS_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] =
    &[expected_unique_constraint(&["repo_id", "analysis_key"])];
const GIT_CO_CHANGES_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] = &[
    expected_unique_constraint(&["repo_id", "left_path", "right_path"]),
];
const DEPENDENCIES_UNIQUE_CONSTRAINTS: &[ExpectedUniqueConstraint] =
    &[expected_unique_constraint(&[
        "source_file_id",
        "target_path",
        "kind",
        "symbol_name",
    ])];

const REPOS_CHECK_CONSTRAINTS: &[&str] = &["CHECK (length(root_key) > 0)"];
const SCAN_RUNS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(run_key) > 0)",
    "CHECK (status IN ('started', 'completed', 'failed'))",
    "CHECK (files_observed IS NULL OR files_observed >= 0)",
    "CHECK (warnings_observed IS NULL OR warnings_observed >= 0)",
];
const SCAN_WARNINGS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (warning_order >= 0)",
    "CHECK (length(code) > 0)",
    "CHECK (path IS NULL OR length(path) > 0)",
    "CHECK (path IS NULL OR path != '..')",
    "CHECK (path IS NULL OR path NOT LIKE '/%')",
    "CHECK (path IS NULL OR path NOT LIKE './%')",
    "CHECK (path IS NULL OR path NOT LIKE '../%')",
    "CHECK (path IS NULL OR path NOT LIKE '%/../%')",
    "CHECK (path IS NULL OR path NOT LIKE '%/..')",
    "CHECK (path IS NULL OR path NOT LIKE '~%')",
    "CHECK (path IS NULL OR path NOT GLOB '[A-Za-z]:*')",
    "CHECK (path IS NULL OR instr(path, '\\') = 0)",
    "CHECK (path IS NULL OR instr(path, char(0)) = 0)",
    "CHECK (length(message) > 0)",
];
const FILES_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(path) > 0)",
    "CHECK (path != '..')",
    "CHECK (path NOT LIKE '/%')",
    "CHECK (path NOT LIKE './%')",
    "CHECK (path NOT LIKE '../%')",
    "CHECK (path NOT LIKE '%/../%')",
    "CHECK (path NOT LIKE '%/..')",
    "CHECK (path NOT LIKE '~%')",
    "CHECK (path NOT GLOB '[A-Za-z]:*')",
    "CHECK (instr(path, '\\') = 0)",
    "CHECK (instr(path, char(0)) = 0)",
    "CHECK (byte_size IS NULL OR byte_size >= 0)",
    "CHECK (line_count IS NULL OR line_count >= 0)",
    "CHECK (content_kind IS NULL OR content_kind IN ('text', 'binary', 'unknown'))",
    "CHECK (is_vendor IN (0, 1))",
    "CHECK (is_generated IN (0, 1))",
    "CHECK (is_symlink IN (0, 1))",
];
const FILE_WARNINGS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (warning_order >= 0)",
    "CHECK (length(code) > 0)",
    "CHECK (length(message) > 0)",
];
const SYMBOLS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(name) > 0)",
    "CHECK (length(kind) > 0)",
    "CHECK (line_start IS NULL OR line_start >= 1)",
    "CHECK (line_end IS NULL OR line_end >= line_start)",
];
const GIT_ANALYSIS_RUNS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(analysis_key) > 0)",
    "CHECK (status IN ('completed'))",
    "CHECK (length(git_head) > 0)",
    "CHECK (recent_window_days >= 0)",
    "CHECK (metrics_observed >= 0)",
    "CHECK (co_changes_observed >= 0)",
];
const GIT_FILE_STATS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (commits_per_file >= 0)",
    "CHECK (total_churn_added >= 0)",
    "CHECK (total_churn_deleted >= 0)",
    "CHECK (recent_churn_added >= 0)",
    "CHECK (recent_churn_deleted >= 0)",
    "CHECK (author_count >= 0)",
    "CHECK (dominant_owner_share IS NULL OR (dominant_owner_share >= 0.0 AND dominant_owner_share <= 1.0))",
    "CHECK (file_age_days IS NULL OR file_age_days >= 0)",
];
const GIT_CO_CHANGES_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(left_path) > 0)",
    "CHECK (left_path != '..')",
    "CHECK (left_path NOT LIKE '/%')",
    "CHECK (left_path NOT LIKE './%')",
    "CHECK (left_path NOT LIKE '../%')",
    "CHECK (left_path NOT LIKE '%/../%')",
    "CHECK (left_path NOT LIKE '%/..')",
    "CHECK (left_path NOT LIKE '~%')",
    "CHECK (left_path NOT GLOB '[A-Za-z]:*')",
    "CHECK (instr(left_path, '\\') = 0)",
    "CHECK (instr(left_path, char(0)) = 0)",
    "CHECK (length(right_path) > 0)",
    "CHECK (right_path != '..')",
    "CHECK (right_path NOT LIKE '/%')",
    "CHECK (right_path NOT LIKE './%')",
    "CHECK (right_path NOT LIKE '../%')",
    "CHECK (right_path NOT LIKE '%/../%')",
    "CHECK (right_path NOT LIKE '%/..')",
    "CHECK (right_path NOT LIKE '~%')",
    "CHECK (right_path NOT GLOB '[A-Za-z]:*')",
    "CHECK (instr(right_path, '\\') = 0)",
    "CHECK (instr(right_path, char(0)) = 0)",
    "CHECK (left_path < right_path)",
    "CHECK (commit_count >= 0)",
];
const DEPENDENCIES_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(target_path) > 0)",
    "CHECK (target_path != '..')",
    "CHECK (target_path NOT LIKE '/%')",
    "CHECK (target_path NOT LIKE './%')",
    "CHECK (target_path NOT LIKE '../%')",
    "CHECK (target_path NOT LIKE '%/../%')",
    "CHECK (target_path NOT LIKE '%/..')",
    "CHECK (target_path NOT LIKE '~%')",
    "CHECK (target_path NOT GLOB '[A-Za-z]:*')",
    "CHECK (instr(target_path, '\\') = 0)",
    "CHECK (instr(target_path, char(0)) = 0)",
    "CHECK (length(kind) > 0)",
    "CHECK (weight IS NULL OR weight >= 0.0)",
];
const HOTSPOTS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (score >= 0.0)",
    "CHECK (rank IS NULL OR rank >= 1)",
    "CHECK (length(formula_version) > 0)",
];
const GIT_COMMIT_DIFFS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(commit_id) > 0)",
    "CHECK (length(analyzer_version) > 0)",
    "CHECK (length(changes_json) > 0)",
];

const REQUIRED_INDEXES: &[ExpectedIndex] = &[
    expected_index(
        "scan_runs_by_repo_run_key",
        "scan_runs",
        &["repo_id", "run_key"],
    ),
    expected_index(
        "scan_warnings_by_scan_order",
        "scan_warnings",
        &["scan_run_id", "warning_order"],
    ),
    expected_index("files_by_repo_path", "files", &["repo_id", "path"]),
    expected_index(
        "file_warnings_by_scan_file_order",
        "file_warnings",
        &["scan_run_id", "file_id", "warning_order"],
    ),
    expected_index(
        "symbols_by_file_order",
        "symbols",
        &["file_id", "line_start", "line_end", "kind", "name"],
    ),
    expected_index(
        "git_analysis_runs_by_repo_key",
        "git_analysis_runs",
        &["repo_id", "analysis_key"],
    ),
    expected_index(
        "git_file_stats_by_repo_analysis",
        "git_file_stats",
        &["repo_id", "analysis_run_id", "file_id"],
    ),
    expected_index(
        "git_co_changes_by_repo_rank",
        "git_co_changes",
        &["repo_id", "commit_count", "left_path", "right_path"],
    ),
    expected_index(
        "dependencies_by_source",
        "dependencies",
        &["source_file_id", "kind", "target_path"],
    ),
    expected_index(
        "dependencies_by_target",
        "dependencies",
        &["target_file_id", "kind", "source_file_id"],
    ),
    expected_index(
        "hotspots_by_repo_rank",
        "hotspots",
        &["repo_id", "rank", "file_id"],
    ),
    expected_index(
        "hotspots_by_repo_score",
        "hotspots",
        &["repo_id", "score", "file_id"],
    ),
    expected_index(
        "git_commit_diffs_by_repo_commit",
        "git_commit_diffs",
        &["repo_id", "commit_id"],
    ),
];

const fn expected_column(
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
) -> ExpectedColumn {
    ExpectedColumn {
        name,
        data_type,
        not_null,
        default_value,
        primary_key_position,
        hidden: 0,
    }
}

const fn expected_foreign_key(
    from_column: &'static str,
    target_table: &'static str,
    target_column: &'static str,
    on_delete: &'static str,
) -> ExpectedForeignKey {
    ExpectedForeignKey {
        from_column,
        target_table,
        target_column,
        on_delete,
    }
}

const fn expected_unique_constraint(columns: &'static [&'static str]) -> ExpectedUniqueConstraint {
    ExpectedUniqueConstraint { columns }
}

const fn expected_index(
    name: &'static str,
    table_name: &'static str,
    columns: &'static [&'static str],
) -> ExpectedIndex {
    ExpectedIndex {
        name,
        table_name,
        columns,
    }
}

fn verify_metadata_table_shape(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    verify_table_shape(
        connection,
        path,
        "hotpath_metadata",
        HOTPATH_METADATA_COLUMNS,
    )
}

fn verify_table_shape(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected: &[ExpectedColumn],
) -> Result<(), IndexError> {
    let summary = read_table_summary(connection, path, table_name)?.ok_or_else(|| {
        IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("missing required table {table_name}"),
        }
    })?;

    if summary.table_type != "table" {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("{table_name} is a {}, not a table", summary.table_type),
        });
    }

    if summary.column_count != expected.len() as i64 {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "{table_name} has {} columns, expected {}",
                summary.column_count,
                expected.len()
            ),
        });
    }

    if summary.without_rowid {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("{table_name} must use the default rowid table layout"),
        });
    }

    if !summary.strict {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("{table_name} must be a STRICT table"),
        });
    }

    let columns = read_table_columns(connection, path, table_name)?;

    if columns.len() != expected.len() {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "{table_name} has {} visible or hidden columns, expected {}",
                columns.len(),
                expected.len()
            ),
        });
    }

    for (column, expected) in columns.iter().zip(expected) {
        if column.name != expected.name
            || !column.data_type.eq_ignore_ascii_case(expected.data_type)
            || column.not_null != expected.not_null
            || column.default_value.as_deref() != expected.default_value
            || column.primary_key_position != expected.primary_key_position
            || column.hidden != expected.hidden
        {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "{table_name} column '{}' does not match expected schema",
                    column.name
                ),
            });
        }
    }

    Ok(())
}

fn expected_columns(table_name: &str) -> &'static [ExpectedColumn] {
    match table_name {
        "hotpath_metadata" => HOTPATH_METADATA_COLUMNS,
        "repos" => REPOS_COLUMNS,
        "scan_runs" => SCAN_RUNS_COLUMNS,
        "scan_warnings" => SCAN_WARNINGS_COLUMNS,
        "files" => FILES_COLUMNS,
        "file_warnings" => FILE_WARNINGS_COLUMNS,
        "symbols" => SYMBOLS_COLUMNS,
        "git_analysis_runs" => GIT_ANALYSIS_RUNS_COLUMNS,
        "git_file_stats" => GIT_FILE_STATS_COLUMNS,
        "git_co_changes" => GIT_CO_CHANGES_COLUMNS,
        "dependencies" => DEPENDENCIES_COLUMNS,
        "hotspots" => HOTSPOTS_COLUMNS,
        "git_commit_diffs" => GIT_COMMIT_DIFFS_COLUMNS,
        _ => &[],
    }
}

fn expected_foreign_keys(table_name: &str) -> &'static [ExpectedForeignKey] {
    match table_name {
        "hotpath_metadata" | "repos" => NO_FOREIGN_KEYS,
        "scan_runs" => SCAN_RUNS_FOREIGN_KEYS,
        "scan_warnings" => SCAN_WARNINGS_FOREIGN_KEYS,
        "files" => FILES_FOREIGN_KEYS,
        "file_warnings" => FILE_WARNINGS_FOREIGN_KEYS,
        "symbols" => SYMBOLS_FOREIGN_KEYS,
        "git_analysis_runs" => GIT_ANALYSIS_RUNS_FOREIGN_KEYS,
        "git_file_stats" => GIT_FILE_STATS_FOREIGN_KEYS,
        "git_co_changes" => GIT_CO_CHANGES_FOREIGN_KEYS,
        "dependencies" => DEPENDENCIES_FOREIGN_KEYS,
        "hotspots" => HOTSPOTS_FOREIGN_KEYS,
        "git_commit_diffs" => GIT_COMMIT_DIFFS_FOREIGN_KEYS,
        _ => NO_FOREIGN_KEYS,
    }
}

fn expected_unique_constraints(table_name: &str) -> &'static [ExpectedUniqueConstraint] {
    match table_name {
        "repos" => REPOS_UNIQUE_CONSTRAINTS,
        "scan_runs" => SCAN_RUNS_UNIQUE_CONSTRAINTS,
        "scan_warnings" => SCAN_WARNINGS_UNIQUE_CONSTRAINTS,
        "files" => FILES_UNIQUE_CONSTRAINTS,
        "file_warnings" => FILE_WARNINGS_UNIQUE_CONSTRAINTS,
        "symbols" => SYMBOLS_UNIQUE_CONSTRAINTS,
        "git_analysis_runs" => GIT_ANALYSIS_RUNS_UNIQUE_CONSTRAINTS,
        "git_co_changes" => GIT_CO_CHANGES_UNIQUE_CONSTRAINTS,
        "dependencies" => DEPENDENCIES_UNIQUE_CONSTRAINTS,
        "git_commit_diffs" => NO_UNIQUE_CONSTRAINTS,
        _ => NO_UNIQUE_CONSTRAINTS,
    }
}

fn expected_check_constraints(table_name: &str) -> &'static [&'static str] {
    match table_name {
        "repos" => REPOS_CHECK_CONSTRAINTS,
        "scan_runs" => SCAN_RUNS_CHECK_CONSTRAINTS,
        "scan_warnings" => SCAN_WARNINGS_CHECK_CONSTRAINTS,
        "files" => FILES_CHECK_CONSTRAINTS,
        "file_warnings" => FILE_WARNINGS_CHECK_CONSTRAINTS,
        "symbols" => SYMBOLS_CHECK_CONSTRAINTS,
        "git_analysis_runs" => GIT_ANALYSIS_RUNS_CHECK_CONSTRAINTS,
        "git_file_stats" => GIT_FILE_STATS_CHECK_CONSTRAINTS,
        "git_co_changes" => GIT_CO_CHANGES_CHECK_CONSTRAINTS,
        "dependencies" => DEPENDENCIES_CHECK_CONSTRAINTS,
        "hotspots" => HOTSPOTS_CHECK_CONSTRAINTS,
        "git_commit_diffs" => GIT_COMMIT_DIFFS_CHECK_CONSTRAINTS,
        _ => NO_CHECK_CONSTRAINTS,
    }
}

fn verify_required_check_constraints(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected: &[&str],
) -> Result<(), IndexError> {
    if expected.is_empty() {
        return Ok(());
    }

    let table_sql = read_table_sql(connection, path, table_name)?;
    let normalized_table_sql = normalize_schema_sql(&table_sql);

    for expected_check in expected {
        let normalized_check = normalize_schema_sql(expected_check);

        if !normalized_table_sql.contains(&normalized_check) {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!("{table_name} is missing required constraint {expected_check}"),
            });
        }
    }

    Ok(())
}

fn verify_unique_constraints(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected: &[ExpectedUniqueConstraint],
) -> Result<(), IndexError> {
    for expected_constraint in expected {
        if !table_has_unique_constraint(connection, path, table_name, expected_constraint.columns)?
        {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "{table_name} is missing required UNIQUE constraint on ({})",
                    expected_constraint.columns.join(", ")
                ),
            });
        }
    }

    Ok(())
}

fn verify_foreign_keys(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected: &[ExpectedForeignKey],
) -> Result<(), IndexError> {
    let foreign_keys = read_foreign_keys(connection, path, table_name)?;

    if foreign_keys.len() != expected.len() {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "{table_name} has {} foreign key columns, expected {}",
                foreign_keys.len(),
                expected.len()
            ),
        });
    }

    for expected_key in expected {
        let found = foreign_keys.iter().any(|foreign_key| {
            foreign_key.from_column == expected_key.from_column
                && foreign_key.target_table == expected_key.target_table
                && foreign_key.target_column == expected_key.target_column
                && foreign_key
                    .on_delete
                    .eq_ignore_ascii_case(expected_key.on_delete)
        });

        if !found {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "{table_name} foreign key {} -> {}.{} ON DELETE {} is missing",
                    expected_key.from_column,
                    expected_key.target_table,
                    expected_key.target_column,
                    expected_key.on_delete
                ),
            });
        }
    }

    Ok(())
}

fn verify_required_indexes(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    for expected in REQUIRED_INDEXES {
        let columns = read_index_columns(connection, path, expected.name)?;
        let expected_columns = expected
            .columns
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        if columns != expected_columns {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "index {} has columns ({}), expected ({})",
                    expected.name,
                    columns.join(", "),
                    expected_columns.join(", ")
                ),
            });
        }

        let table_name = read_index_table_name(connection, path, expected.name)?;
        if table_name != expected.table_name {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "index {} belongs to table {}, expected {}",
                    expected.name, table_name, expected.table_name
                ),
            });
        }
    }

    Ok(())
}

fn read_table_summary(
    connection: &Connection,
    path: &Path,
    table_name: &str,
) -> Result<Option<MetadataTableSummary>, IndexError> {
    let sql = format!("PRAGMA table_list('{table_name}');");

    connection
        .query_row(&sql, [], |row| {
            Ok(MetadataTableSummary {
                table_type: row.get(2)?,
                column_count: row.get(3)?,
                without_rowid: row.get::<_, i64>(4)? != 0,
                strict: row.get::<_, i64>(5)? != 0,
            })
        })
        .optional()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

fn read_table_columns(
    connection: &Connection,
    path: &Path,
    table_name: &str,
) -> Result<Vec<MetadataColumn>, IndexError> {
    let sql = format!("PRAGMA table_xinfo('{table_name}');");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    let columns = statement
        .query_map([], |row| {
            Ok(MetadataColumn {
                name: row.get(1)?,
                data_type: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
                primary_key_position: row.get(5)?,
                hidden: row.get(6)?,
            })
        })
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    columns
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

fn read_table_sql(
    connection: &Connection,
    path: &Path,
    table_name: &str,
) -> Result<String, IndexError> {
    connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1;",
            params![table_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("missing required table {table_name}"),
        })
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn table_has_unique_constraint(
    connection: &Connection,
    path: &Path,
    table_name: &str,
    expected_columns: &[&str],
) -> Result<bool, IndexError> {
    let expected_columns = expected_columns
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let sql = format!("PRAGMA index_list('{table_name}');");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    let indexes = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    for index in indexes {
        let (index_name, is_unique, origin) =
            index.map_err(|source| IndexError::CorruptDatabase {
                path: path.to_path_buf(),
                source,
            })?;

        if !is_unique || origin != "u" {
            continue;
        }

        if read_index_columns(connection, path, &index_name)? == expected_columns {
            return Ok(true);
        }
    }

    Ok(false)
}

fn read_index_table_name(
    connection: &Connection,
    path: &Path,
    index_name: &str,
) -> Result<String, IndexError> {
    connection
        .query_row(
            "SELECT tbl_name FROM sqlite_schema WHERE type = 'index' AND name = ?1;",
            params![index_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("missing required index {index_name}"),
        })
}

fn read_index_columns(
    connection: &Connection,
    path: &Path,
    index_name: &str,
) -> Result<Vec<String>, IndexError> {
    read_index_table_name(connection, path, index_name)?;

    let sql = format!("PRAGMA index_info('{index_name}');");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    let columns = statement
        .query_map([], |row| row.get::<_, String>(2))
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    columns
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

fn read_foreign_keys(
    connection: &Connection,
    path: &Path,
    table_name: &str,
) -> Result<Vec<ForeignKeyColumn>, IndexError> {
    let sql = format!("PRAGMA foreign_key_list('{table_name}');");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    let foreign_keys = statement
        .query_map([], |row| {
            Ok(ForeignKeyColumn {
                target_table: row.get(2)?,
                from_column: row.get(3)?,
                target_column: row.get(4)?,
                on_delete: row.get(6)?,
            })
        })
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?;

    foreign_keys
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })
}

fn read_metadata_schema_version(connection: &Connection, path: &Path) -> Result<u32, IndexError> {
    let value = connection
        .query_row(
            "SELECT value FROM hotpath_metadata WHERE key = ?1;",
            params![SCHEMA_VERSION_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "missing schema_version metadata row".to_owned(),
        })?;

    value
        .parse::<u32>()
        .map_err(|source| IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("schema_version metadata value '{value}' is not a number: {source}"),
        })
}

fn read_metadata_schema_identifier(
    connection: &Connection,
    path: &Path,
) -> Result<String, IndexError> {
    connection
        .query_row(
            "SELECT value FROM hotpath_metadata WHERE key = ?1;",
            params![SCHEMA_IDENTIFIER_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|source| IndexError::CorruptDatabase {
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "missing schema_identifier metadata row".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::scoring::{calculate_hotspot_score, rank_hotspot_scores, RawScoreMetrics};
    use crate::{
        FileWarning, ParseFileRecord, ParseFileStatus, ParseImportRecord, ParseReport,
        ParseSymbolRecord, ScanWarning,
    };

    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::current_dir()
                .expect("test should have a current directory")
                .join("target")
                .join("storage-fixtures")
                .join(format!("{name}-{}-{id}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("fixture root should be created");

            Self { path }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct CleanupDir {
        path: PathBuf,
    }

    impl Drop for CleanupDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    fn user_table_names(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare(
                "SELECT name
                 FROM sqlite_schema
                 WHERE type = 'table'
                   AND name NOT LIKE 'sqlite_%'
                 ORDER BY name;",
            )
            .expect("table query should prepare");

        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("table names should read")
    }

    fn table_columns(connection: &Connection, table_name: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_xinfo('{table_name}');"))
            .expect("column query should prepare");

        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names should read")
    }

    fn is_strict_table(connection: &Connection, table_name: &str) -> bool {
        connection
            .query_row(&format!("PRAGMA table_list('{table_name}');"), [], |row| {
                Ok(row.get::<_, i64>(5)? != 0)
            })
            .expect("table strict flag should read")
    }

    fn table_has_foreign_key(
        connection: &Connection,
        table_name: &str,
        target_table: &str,
    ) -> bool {
        let mut statement = connection
            .prepare(&format!("PRAGMA foreign_key_list('{table_name}');"))
            .expect("foreign key query should prepare");
        let targets = statement
            .query_map([], |row| row.get::<_, String>(2))
            .expect("foreign key query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("foreign key targets should read");

        targets.iter().any(|target| target == target_table)
    }

    fn metadata_rows(connection: &Connection) -> Vec<(String, String)> {
        let mut statement = connection
            .prepare("SELECT key, value FROM hotpath_metadata ORDER BY key;")
            .expect("metadata query should prepare");

        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("metadata query should run")
            .collect::<Result<Vec<_>, _>>()
            .expect("metadata rows should read")
    }

    fn indexed_file_id(connection: &Connection, path: &str) -> i64 {
        connection
            .query_row(
                "SELECT id FROM files WHERE path = ?1;",
                params![path],
                |row| row.get(0),
            )
            .expect("indexed file id should read")
    }

    fn table_count(connection: &Connection, table_name: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table_name};"), [], |row| {
                row.get(0)
            })
            .expect("table count should read")
    }

    fn assert_sqlite_check_constraint(error: rusqlite::Error) {
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(error, _)
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ));
    }

    fn assert_scan_persist_check_constraint(error: IndexError) {
        match error {
            IndexError::PersistScan { source, .. } => assert_sqlite_check_constraint(source),
            other => panic!("expected scan persistence CHECK constraint failure, got {other:?}"),
        }
    }

    fn assert_git_persist_check_constraint(error: IndexError) {
        match error {
            IndexError::PersistGitAnalysis { source, .. } => {
                assert_sqlite_check_constraint(source);
            }
            other => panic!("expected Git persistence CHECK constraint failure, got {other:?}"),
        }
    }

    fn unsafe_path_values() -> [&'static str; 3] {
        ["~/repo/file.rs", "C:/repo/file.rs", r"C:\repo\file.rs"]
    }

    fn create_legacy_metadata_only_index(index_path: &Path, metadata_rows: &[(&str, &str)]) {
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                ) STRICT;",
            )
            .expect("legacy metadata table should be created");

        for (key, value) in metadata_rows {
            connection
                .execute(
                    "INSERT INTO hotpath_metadata (key, value) VALUES (?1, ?2);",
                    params![key, value],
                )
                .expect("legacy metadata row should be inserted");
        }
    }

    fn scan_report(files: Vec<FileRecord>) -> ScanReport {
        ScanReport {
            status: "ok",
            file_walking: "implemented",
            classification: "implemented",
            warnings: Vec::new(),
            files,
        }
    }

    fn scan_file(
        path: &str,
        byte_size: Option<u64>,
        language: Option<&'static str>,
        content: ContentKind,
    ) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size,
            extension: Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_owned),
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

    fn parse_report(files: Vec<ParseFileRecord>, symbols: Vec<ParseSymbolRecord>) -> ParseReport {
        parse_report_with_imports(files, symbols, Vec::new())
    }

    fn parse_report_with_imports(
        files: Vec<ParseFileRecord>,
        symbols: Vec<ParseSymbolRecord>,
        imports: Vec<ParseImportRecord>,
    ) -> ParseReport {
        ParseReport {
            warnings: Vec::new(),
            files,
            symbols,
            imports,
        }
    }

    fn parse_file(path: &str) -> ParseFileRecord {
        ParseFileRecord {
            path: path.to_owned(),
            language: Some("Rust"),
            content: ContentKind::Text,
            status: ParseFileStatus::Parsed,
            reason: None,
            symbol_count: 0,
            import_count: 0,
        }
    }

    fn parse_symbol(
        path: &str,
        name: &str,
        kind: &str,
        start_line: u64,
        end_line: u64,
        parent: Option<&str>,
    ) -> ParseSymbolRecord {
        ParseSymbolRecord {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            start_line,
            end_line,
            signature: Some(format!("{kind} {name}")),
            nesting_depth: u64::from(parent.is_some()),
            parent: parent.map(ToOwned::to_owned),
            cyclomatic_complexity: Some(1),
            max_control_flow_nesting: Some(0),
        }
    }

    fn parse_import(path: &str, target: &str, kind: &str) -> ParseImportRecord {
        ParseImportRecord {
            path: path.to_owned(),
            target: target.to_owned(),
            kind: kind.to_owned(),
            start_line: 1,
            end_line: 1,
        }
    }

    fn files_table_sql(include_unique: bool, include_bare_dotdot_check: bool) -> String {
        format!(
            "CREATE TABLE files (
                id INTEGER PRIMARY KEY,
                repo_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                byte_size INTEGER,
                extension TEXT,
                language TEXT,
                line_count INTEGER,
                content_kind TEXT,
                is_vendor INTEGER NOT NULL DEFAULT 0,
                is_generated INTEGER NOT NULL DEFAULT 0,
                is_symlink INTEGER NOT NULL DEFAULT 0,
                classification TEXT,
                scan_run_id INTEGER,
                {unique}
                CHECK (length(path) > 0),
                {bare_dotdot_check}
                CHECK (path NOT LIKE '/%'),
                CHECK (path NOT LIKE './%'),
                CHECK (path NOT LIKE '../%'),
                CHECK (path NOT LIKE '%/../%'),
                CHECK (path NOT LIKE '%/..'),
                CHECK (path NOT LIKE '~%'),
                CHECK (path NOT GLOB '[A-Za-z]:*'),
                CHECK (instr(path, '\\') = 0),
                CHECK (instr(path, char(0)) = 0),
                CHECK (byte_size IS NULL OR byte_size >= 0),
                CHECK (line_count IS NULL OR line_count >= 0),
                CHECK (content_kind IS NULL OR content_kind IN ('text', 'binary', 'unknown')),
                CHECK (is_vendor IN (0, 1)),
                CHECK (is_generated IN (0, 1)),
                CHECK (is_symlink IN (0, 1)),
                FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                FOREIGN KEY (scan_run_id) REFERENCES scan_runs(id) ON DELETE SET NULL
            ) STRICT;",
            unique = if include_unique {
                "UNIQUE (repo_id, path),"
            } else {
                ""
            },
            bare_dotdot_check = if include_bare_dotdot_check {
                "CHECK (path != '..'),"
            } else {
                ""
            }
        )
    }

    fn recreate_files_table(
        connection: &Connection,
        include_unique: bool,
        include_bare_dotdot_check: bool,
    ) {
        connection
            .execute_batch("DROP TABLE files;")
            .expect("files table should be dropped");
        connection
            .execute_batch(&files_table_sql(include_unique, include_bare_dotdot_check))
            .expect("files table should be recreated");
        connection
            .execute_batch(
                "CREATE INDEX files_by_repo_path
                    ON files (repo_id, path);",
            )
            .expect("files index should be recreated");
    }

    fn recreate_dependencies_table_with_target_delete_action(
        connection: &Connection,
        target_delete_action: &str,
    ) {
        connection
            .execute_batch("DROP TABLE dependencies;")
            .expect("dependencies table should be dropped");
        connection
            .execute_batch(&format!(
                "CREATE TABLE dependencies (
                    id INTEGER PRIMARY KEY,
                    repo_id INTEGER NOT NULL,
                    source_file_id INTEGER NOT NULL,
                    target_file_id INTEGER,
                    target_path TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    symbol_name TEXT,
                    weight REAL,
                    UNIQUE (source_file_id, target_path, kind, symbol_name),
                    CHECK (length(target_path) > 0),
                    CHECK (target_path != '..'),
                    CHECK (target_path NOT LIKE '/%'),
                    CHECK (target_path NOT LIKE './%'),
                    CHECK (target_path NOT LIKE '../%'),
                    CHECK (target_path NOT LIKE '%/../%'),
                    CHECK (target_path NOT LIKE '%/..'),
                    CHECK (target_path NOT LIKE '~%'),
                    CHECK (target_path NOT GLOB '[A-Za-z]:*'),
                    CHECK (instr(target_path, '\\') = 0),
                    CHECK (instr(target_path, char(0)) = 0),
                    CHECK (length(kind) > 0),
                    CHECK (weight IS NULL OR weight >= 0.0),
                    FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
                    FOREIGN KEY (source_file_id) REFERENCES files(id) ON DELETE CASCADE,
                    FOREIGN KEY (target_file_id) REFERENCES files(id) ON DELETE {target_delete_action}
                ) STRICT;"
            ))
            .expect("dependencies table should be recreated");
        connection
            .execute_batch(
                "CREATE INDEX dependencies_by_source
                    ON dependencies (source_file_id, kind, target_path);
                CREATE INDEX dependencies_by_target
                    ON dependencies (target_file_id, kind, source_file_id);",
            )
            .expect("dependencies indexes should be recreated");
    }

    #[test]
    fn persist_scan_records_completed_run_and_file_facts() {
        let fixture = Fixture::new("persist-scan");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let mut generated = scan_file(
            "dist/app.generated.js",
            Some(21),
            Some("JavaScript"),
            ContentKind::Text,
        );
        generated.line_count = Some(1);
        generated.is_generated = true;
        generated.warnings.push(FileWarning {
            code: "line_count_skipped",
            message: "test warning".to_owned(),
        });
        let mut vendor = scan_file("vendor/blob.bin", Some(4), None, ContentKind::Binary);
        vendor.is_vendor = true;
        vendor.is_symlink = true;
        let mut scan = scan_report(vec![vendor, generated]);
        scan.warnings.push(ScanWarning {
            code: "walk_error",
            path: Some("blocked".to_owned()),
            message: "test scan warning".to_owned(),
        });

        let run = store
            .persist_scan(&scan)
            .expect("scan should persist successfully");
        let persisted = store
            .latest_scan()
            .expect("latest scan should read")
            .expect("latest scan should exist");

        assert_eq!(run.run_key, "scan-0000000000000001");
        assert_eq!(run.status, "completed");
        assert_eq!(
            run.scanner_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            run.scan_schema_identifier.as_deref(),
            Some(SCAN_SCHEMA_VERSION)
        );
        assert_eq!(run.files_observed, Some(2));
        assert_eq!(run.warnings_observed, Some(2));
        assert_eq!(persisted.run, run);
        assert_eq!(
            persisted.warnings,
            vec![PersistedScanWarning {
                code: "walk_error".to_owned(),
                path: Some("blocked".to_owned()),
                message: "test scan warning".to_owned(),
            }]
        );
        assert_eq!(
            persisted
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["dist/app.generated.js", "vendor/blob.bin"]
        );
        assert_eq!(
            persisted.files[0],
            PersistedFileRecord {
                path: "dist/app.generated.js".to_owned(),
                byte_size: Some(21),
                extension: Some("js".to_owned()),
                language: Some("JavaScript".to_owned()),
                line_count: Some(1),
                is_vendor: false,
                is_generated: true,
                content: ContentKind::Text,
                is_symlink: false,
                classification: Some("implemented".to_owned()),
                warnings: vec![PersistedFileWarning {
                    code: "line_count_skipped".to_owned(),
                    message: "test warning".to_owned(),
                }],
            }
        );
        assert_eq!(
            persisted.files[1],
            PersistedFileRecord {
                path: "vendor/blob.bin".to_owned(),
                byte_size: Some(4),
                extension: Some("bin".to_owned()),
                language: None,
                line_count: None,
                is_vendor: true,
                is_generated: false,
                content: ContentKind::Binary,
                is_symlink: true,
                classification: Some("implemented".to_owned()),
                warnings: Vec::new(),
            }
        );
    }

    #[test]
    fn persist_scan_appends_one_run_per_successful_scan() {
        let fixture = Fixture::new("persist-scan-runs");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let first = scan_report(vec![scan_file(
            "a.rs",
            Some(0),
            Some("Rust"),
            ContentKind::Text,
        )]);
        let second = scan_report(vec![scan_file(
            "b.rs",
            Some(1),
            Some("Rust"),
            ContentKind::Text,
        )]);

        let first_run = store
            .persist_scan(&first)
            .expect("first scan should persist");
        let second_run = store
            .persist_scan(&second)
            .expect("second scan should persist");
        let latest = store
            .latest_scan()
            .expect("latest scan should read")
            .expect("latest scan should exist");
        let connection = Connection::open(store.path()).expect("index should reopen");
        let run_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM scan_runs;", [], |row| row.get(0))
            .expect("run count should read");

        assert_eq!(first_run.run_key, "scan-0000000000000001");
        assert_eq!(second_run.run_key, "scan-0000000000000002");
        assert_eq!(run_count, 2);
        assert_eq!(latest.run, second_run);
        assert_eq!(
            latest
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["b.rs"]
        );
    }

    #[test]
    fn persist_scan_deletes_stale_files_and_dependent_rows() {
        let fixture = Fixture::new("persist-stale-files");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let mut stale = scan_file("src/stale.rs", Some(8), Some("Rust"), ContentKind::Text);
        stale.warnings.push(FileWarning {
            code: "unsupported_encoding",
            message: "test warning".to_owned(),
        });
        let first = scan_report(vec![
            scan_file("src/keep.rs", Some(3), Some("Rust"), ContentKind::Text),
            stale,
        ]);

        store
            .persist_scan(&first)
            .expect("first scan should persist");

        let index_path = store.path().to_path_buf();
        let connection = Connection::open(&index_path).expect("index should reopen");
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("foreign keys should enable");
        let keep_id = indexed_file_id(&connection, "src/keep.rs");
        let stale_id = indexed_file_id(&connection, "src/stale.rs");
        connection
            .execute(
                "INSERT INTO symbols (file_id, name, kind, line_start, line_end)
                 VALUES (?1, 'stale_symbol', 'function', 1, 1);",
                params![stale_id],
            )
            .expect("symbol should insert");
        connection
            .execute(
                "INSERT INTO git_analysis_runs (
                    repo_id,
                    analysis_key,
                    status,
                    git_head,
                    head_commit_time,
                    recent_window_days,
                    metrics_observed,
                    co_changes_observed
                )
                VALUES (1, 'test', 'completed', 'abc123', 1, 90, 1, 0);",
                [],
            )
            .expect("Git analysis run should insert");
        let git_analysis_run_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO git_file_stats (
                    file_id,
                    repo_id,
                    analysis_run_id,
                    commits_per_file,
                    total_churn_added,
                    total_churn_deleted,
                    author_count
                )
                 VALUES (?1, 1, ?2, 1, 2, 3, 1);",
                params![stale_id, git_analysis_run_id],
            )
            .expect("git stats should insert");
        connection
            .execute(
                "INSERT INTO dependencies (repo_id, source_file_id, target_file_id, target_path, kind)
                 VALUES (1, ?1, ?2, 'src/stale.rs', 'import');",
                params![keep_id, stale_id],
            )
            .expect("dependency should insert");
        connection
            .execute("INSERT INTO repos (root_key) VALUES ('other-repo');", [])
            .expect("second repo should insert");
        let other_repo_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, 'src/other.rs');",
                params![other_repo_id],
            )
            .expect("second repo file should insert");
        let other_file_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO dependencies (repo_id, source_file_id, target_file_id, target_path, kind)
                 VALUES (?1, ?2, ?3, 'src/stale.rs', 'import');",
                params![other_repo_id, other_file_id, stale_id],
            )
            .expect("cross-repo dependency should insert");
        connection
            .execute(
                "INSERT INTO hotspots (file_id, repo_id, scan_run_id, score, rank, formula_version)
                 VALUES (?1, 1, 1, 1.0, 1, 'test');",
                params![stale_id],
            )
            .expect("hotspot should insert");
        drop(connection);

        let mut keep = scan_file("src/keep.rs", Some(12), Some("Rust"), ContentKind::Text);
        keep.line_count = Some(2);
        let second = scan_report(vec![keep]);

        store
            .persist_scan(&second)
            .expect("second scan should persist");

        let latest = store
            .latest_scan()
            .expect("latest scan should read")
            .expect("latest scan should exist");
        assert_eq!(latest.files.len(), 1);
        assert_eq!(latest.files[0].path, "src/keep.rs");
        assert_eq!(latest.files[0].byte_size, Some(12));
        assert_eq!(latest.files[0].line_count, Some(2));

        let connection = Connection::open(&index_path).expect("index should reopen");
        let surviving_dependency = connection
            .query_row(
                "SELECT COUNT(*)
                 FROM dependencies
                 WHERE repo_id = ?1
                   AND source_file_id = ?2
                   AND target_file_id IS NULL
                   AND target_path = 'src/stale.rs';",
                params![other_repo_id, other_file_id],
                |row| row.get::<_, i64>(0),
            )
            .expect("surviving dependency should count");

        assert_eq!(table_count(&connection, "files"), 2);
        assert_eq!(table_count(&connection, "file_warnings"), 0);
        assert_eq!(table_count(&connection, "symbols"), 0);
        assert_eq!(table_count(&connection, "git_file_stats"), 0);
        assert_eq!(table_count(&connection, "dependencies"), 1);
        assert_eq!(surviving_dependency, 1);
        assert_eq!(table_count(&connection, "hotspots"), 0);
    }

    #[test]
    fn persist_symbols_records_parent_relationships_and_reads_back_sorted_rows() {
        let fixture = Fixture::new("persist-symbols");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        store
            .persist_scan(&scan_report(vec![
                scan_file("src/b.rs", Some(20), Some("Rust"), ContentKind::Text),
                scan_file("src/a.rs", Some(10), Some("Rust"), ContentKind::Text),
            ]))
            .expect("scan should persist");

        let report = parse_report(
            vec![parse_file("src/b.rs"), parse_file("src/a.rs")],
            vec![
                parse_symbol("src/b.rs", "child", "function", 12, 12, Some("missing")),
                parse_symbol("src/a.rs", "child", "method", 4, 5, Some("Parent")),
                parse_symbol("src/a.rs", "Parent", "impl", 2, 7, None),
                parse_symbol("src/a.rs", "top", "function", 9, 9, None),
            ],
        );

        store
            .persist_symbols(&report)
            .expect("symbols should persist");

        let symbols = store.latest_symbols().expect("symbols should read");
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (
                    symbol.path.as_str(),
                    symbol.name.as_str(),
                    symbol.kind.as_str(),
                    symbol.line_start,
                    symbol.line_end,
                    symbol.signature.as_deref(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "src/a.rs",
                    "Parent",
                    "impl",
                    Some(2),
                    Some(7),
                    Some("impl Parent"),
                ),
                (
                    "src/a.rs",
                    "child",
                    "method",
                    Some(4),
                    Some(5),
                    Some("method child"),
                ),
                (
                    "src/a.rs",
                    "top",
                    "function",
                    Some(9),
                    Some(9),
                    Some("function top"),
                ),
                (
                    "src/b.rs",
                    "child",
                    "function",
                    Some(12),
                    Some(12),
                    Some("function child"),
                ),
            ]
        );

        let parent = symbols
            .iter()
            .find(|symbol| symbol.name == "Parent")
            .expect("parent symbol should exist");
        let child = symbols
            .iter()
            .find(|symbol| symbol.name == "child" && symbol.path == "src/a.rs")
            .expect("child symbol should exist");
        let unresolved = symbols
            .iter()
            .find(|symbol| symbol.path == "src/b.rs")
            .expect("unresolved child should exist");

        assert_eq!(child.parent_symbol_id, Some(parent.id));
        assert_eq!(unresolved.parent_symbol_id, None);
        assert_eq!(
            store.dependency_count().expect("dependencies should count"),
            0
        );
    }

    #[test]
    fn persist_symbols_resolves_duplicate_parent_names_by_line_containment() {
        let fixture = Fixture::new("persist-symbol-duplicate-parents");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        store
            .persist_scan(&scan_report(vec![scan_file(
                "src/lib.rs",
                Some(40),
                Some("Rust"),
                ContentKind::Text,
            )]))
            .expect("scan should persist");

        let report = parse_report(
            vec![parse_file("src/lib.rs")],
            vec![
                parse_symbol("src/lib.rs", "impl Widget", "impl", 1, 5, None),
                parse_symbol("src/lib.rs", "first", "method", 2, 4, Some("impl Widget")),
                parse_symbol("src/lib.rs", "impl Widget", "impl", 7, 11, None),
                parse_symbol("src/lib.rs", "second", "method", 8, 10, Some("impl Widget")),
            ],
        );

        store
            .persist_symbols(&report)
            .expect("symbols should persist");

        let symbols = store.latest_symbols().expect("symbols should read");
        let first_parent = symbols
            .iter()
            .find(|symbol| symbol.name == "impl Widget" && symbol.line_start == Some(1))
            .expect("first impl parent should exist");
        let second_parent = symbols
            .iter()
            .find(|symbol| symbol.name == "impl Widget" && symbol.line_start == Some(7))
            .expect("second impl parent should exist");
        let first_method = symbols
            .iter()
            .find(|symbol| symbol.name == "first")
            .expect("first method should exist");
        let second_method = symbols
            .iter()
            .find(|symbol| symbol.name == "second")
            .expect("second method should exist");

        assert_eq!(first_method.parent_symbol_id, Some(first_parent.id));
        assert_eq!(second_method.parent_symbol_id, Some(second_parent.id));
    }

    #[test]
    fn persist_symbols_replaces_rows_for_current_parse_scope() {
        let fixture = Fixture::new("persist-symbol-replacement");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        store
            .persist_scan(&scan_report(vec![
                scan_file("src/a.rs", Some(10), Some("Rust"), ContentKind::Text),
                scan_file("src/b.rs", Some(20), Some("Rust"), ContentKind::Text),
            ]))
            .expect("scan should persist");

        let first = parse_report(
            vec![parse_file("src/a.rs"), parse_file("src/b.rs")],
            vec![
                parse_symbol("src/a.rs", "stale", "function", 1, 1, None),
                parse_symbol("src/b.rs", "old", "function", 1, 1, None),
            ],
        );
        store
            .persist_symbols(&first)
            .expect("first symbols should persist");

        let second = parse_report(
            vec![parse_file("src/a.rs"), parse_file("src/b.rs")],
            vec![parse_symbol("src/b.rs", "new", "function", 3, 3, None)],
        );
        store
            .persist_symbols(&second)
            .expect("replacement symbols should persist");

        let symbols = store.latest_symbols().expect("symbols should read");
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.path.as_str(), symbol.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/b.rs", "new")]
        );
        assert_eq!(
            store.dependency_count().expect("dependencies should count"),
            0
        );
    }

    #[test]
    fn persist_symbols_replaces_resolved_dependency_edges() {
        let fixture = Fixture::new("persist-dependencies");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        store
            .persist_scan(&scan_report(vec![
                scan_file("src/lib.rs", Some(10), Some("Rust"), ContentKind::Text),
                scan_file("src/child.rs", Some(20), Some("Rust"), ContentKind::Text),
                scan_file("src/old.rs", Some(30), Some("Rust"), ContentKind::Text),
            ]))
            .expect("scan should persist");

        let first = parse_report_with_imports(
            vec![
                parse_file("src/lib.rs"),
                parse_file("src/child.rs"),
                parse_file("src/old.rs"),
            ],
            Vec::new(),
            vec![
                parse_import("src/lib.rs", "child", "mod"),
                parse_import("src/lib.rs", "std::fmt", "use"),
                parse_import("src/lib.rs", "../unsafe", "use"),
            ],
        );
        store
            .persist_symbols(&first)
            .expect("first dependencies should persist");

        assert_eq!(
            store.dependency_count().expect("dependencies should count"),
            1
        );

        let second = parse_report_with_imports(
            vec![parse_file("src/lib.rs"), parse_file("src/child.rs")],
            Vec::new(),
            Vec::new(),
        );
        store
            .persist_symbols(&second)
            .expect("replacement dependencies should persist");

        assert_eq!(
            store.dependency_count().expect("dependencies should count"),
            0
        );
    }

    #[test]
    fn persist_git_analysis_records_metadata_file_stats_and_co_changes() {
        let fixture = Fixture::new("persist-git-analysis");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let metrics = vec![
            GitFileMetrics {
                path: "src/a.rs".to_owned(),
                commits_per_file: 2,
                total_churn_added: 10,
                total_churn_deleted: 3,
                recent_churn_added: 4,
                recent_churn_deleted: 1,
                author_count: 2,
                owner_count: 2,
                dominant_owner: Some("Ada <ada@example.invalid>".to_owned()),
                dominant_owner_share: Some(0.5),
                co_changed_file_count: 1,
                first_commit_id: Some("1111111111111111111111111111111111111111".to_owned()),
                first_commit_time: Some(1_700_000_000),
                last_commit_id: Some("2222222222222222222222222222222222222222".to_owned()),
                last_commit_time: Some(1_700_086_400),
                file_age_days: Some(1),
            },
            GitFileMetrics {
                path: "src/b.rs".to_owned(),
                commits_per_file: 1,
                total_churn_added: 5,
                total_churn_deleted: 0,
                recent_churn_added: 5,
                recent_churn_deleted: 0,
                author_count: 1,
                owner_count: 1,
                dominant_owner: Some("Ben <ben@example.invalid>".to_owned()),
                dominant_owner_share: Some(1.0),
                co_changed_file_count: 1,
                first_commit_id: Some("3333333333333333333333333333333333333333".to_owned()),
                first_commit_time: Some(1_700_086_400),
                last_commit_id: Some("3333333333333333333333333333333333333333".to_owned()),
                last_commit_time: Some(1_700_086_400),
                file_age_days: Some(0),
            },
        ];
        let co_changes = vec![GitCoChange {
            left_path: "src/a.rs".to_owned(),
            right_path: "src/b.rs".to_owned(),
            commit_count: 1,
        }];

        let run = store
            .persist_git_analysis(
                &fixture.path,
                "2222222222222222222222222222222222222222",
                1_700_086_400,
                90,
                &metrics,
                &co_changes,
            )
            .expect("Git analysis should persist");
        let persisted = store
            .latest_git_analysis()
            .expect("Git analysis should read")
            .expect("Git analysis should exist");

        assert_eq!(run.analysis_key, "git-analysis-current");
        assert_eq!(run.status, "completed");
        assert_eq!(run.git_head, "2222222222222222222222222222222222222222");
        assert_eq!(run.head_commit_time, 1_700_086_400);
        assert_eq!(run.recent_window_days, 90);
        assert_eq!(run.metrics_observed, 2);
        assert_eq!(run.co_changes_observed, 1);
        assert_eq!(persisted.run, run);
        assert_eq!(
            persisted
                .file_stats
                .iter()
                .map(|stats| (
                    stats.path.as_str(),
                    stats.commits_per_file,
                    stats.author_count
                ))
                .collect::<Vec<_>>(),
            vec![("src/a.rs", 2, 2), ("src/b.rs", 1, 1)]
        );
        assert_eq!(persisted.file_stats[0].dominant_owner_share, Some(0.5));
        assert_eq!(
            persisted.co_changes,
            vec![PersistedGitCoChange {
                left_path: "src/a.rs".to_owned(),
                right_path: "src/b.rs".to_owned(),
                commit_count: 1,
            }]
        );
        assert!(persisted
            .file_stats
            .iter()
            .all(|stats| !stats.path.contains(&fixture.path.display().to_string())));
    }

    #[test]
    fn persist_git_commit_changes_round_trips_cached_diff_artifacts() {
        let fixture = Fixture::new("persist-git-commit-cache");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let changes = vec![GitFileChange {
            commit_id: "1111111111111111111111111111111111111111".to_owned(),
            parent_count: 1,
            is_merge: false,
            author: "Ada <ada@example.invalid>".to_owned(),
            commit_time: 1_700_000_000,
            path: "src/lib.rs".to_owned(),
            change_kind: crate::git::GitChangeKind::Modified,
            added_lines: 3,
            deleted_lines: 1,
        }];

        store
            .persist_git_commit_changes(
                "1111111111111111111111111111111111111111",
                "test-version",
                &changes,
            )
            .expect("commit cache should persist");

        assert_eq!(
            store
                .cached_git_commit_changes(
                    "1111111111111111111111111111111111111111",
                    "test-version",
                )
                .expect("commit cache should read"),
            Some(changes.clone())
        );
        assert_eq!(
            store
                .cached_git_commit_changes(
                    "1111111111111111111111111111111111111111",
                    "other-version",
                )
                .expect("commit cache miss should read"),
            None
        );
        assert_eq!(
            store
                .cached_git_commit_changes_batch(
                    &[
                        "1111111111111111111111111111111111111111".to_owned(),
                        "2222222222222222222222222222222222222222".to_owned(),
                    ],
                    "test-version",
                )
                .expect("commit cache batch should read"),
            BTreeMap::from([(
                "1111111111111111111111111111111111111111".to_owned(),
                changes,
            )])
        );
    }

    #[test]
    fn persist_hotspots_records_ranked_scores_json_payloads_and_replaces_rows() {
        let fixture = Fixture::new("persist-hotspots");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let mut risky_file = scan_file(
            "src/risky.rs",
            Some(131_072),
            Some("Rust"),
            ContentKind::Binary,
        );
        risky_file.line_count = None;
        let mut stable_file = scan_file("src/stable.rs", Some(20), Some("Rust"), ContentKind::Text);
        stable_file.line_count = Some(1);
        let scan_run = store
            .persist_scan(&scan_report(vec![risky_file, stable_file]))
            .expect("scan should persist");
        let scores = vec![
            calculate_hotspot_score(RawScoreMetrics {
                path: "src/stable.rs".to_owned(),
                byte_size: Some(20),
                line_count: Some(1),
                commits_per_file: Some(1),
                total_churn_lines: Some(1),
                recent_churn_lines: Some(0),
                author_count: Some(1),
                owner_count: Some(1),
                dominant_owner_share: Some(1.0),
                co_changed_file_count: Some(0),
                file_age_days: Some(1),
                repository_age_days: Some(1),
                repository_author_count: Some(1),
                repository_file_count: Some(2),
            }),
            calculate_hotspot_score(RawScoreMetrics {
                path: "src/risky.rs".to_owned(),
                byte_size: Some(131_072),
                line_count: None,
                commits_per_file: Some(4),
                total_churn_lines: Some(2_000),
                recent_churn_lines: Some(200),
                author_count: Some(4),
                owner_count: Some(4),
                dominant_owner_share: Some(0.25),
                co_changed_file_count: Some(3),
                file_age_days: Some(365),
                repository_age_days: Some(730),
                repository_author_count: Some(10),
                repository_file_count: Some(200),
            }),
        ];
        let ranked = rank_hotspot_scores(&scores);

        store
            .persist_hotspots(scan_run.id, &ranked)
            .expect("hotspots should persist");

        let persisted = store.latest_hotspots().expect("hotspots should read");
        assert_eq!(
            persisted
                .iter()
                .map(|hotspot| (hotspot.rank, hotspot.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "src/risky.rs"), (2, "src/stable.rs")]
        );
        assert_eq!(persisted[0].formula_version, "hotpath.score.v3");
        assert!(persisted[0].score > persisted[1].score);

        let raw_metrics = serde_json::from_str::<serde_json::Value>(
            persisted[0]
                .raw_metrics_json
                .as_deref()
                .expect("raw metrics JSON should be stored"),
        )
        .expect("raw metrics JSON should parse");
        assert_eq!(raw_metrics["path"], "src/risky.rs");
        assert_eq!(raw_metrics["byte_size"], 131_072);
        assert!(raw_metrics["line_count"].is_null());

        let explanation = serde_json::from_str::<serde_json::Value>(
            persisted[0]
                .explanation
                .as_deref()
                .expect("explanation JSON should be stored"),
        )
        .expect("explanation JSON should parse");
        assert_eq!(explanation["normalized_metrics"]["size"], 1.0);
        assert_eq!(explanation["weighted_terms"][0]["name"], "churn_score");
        assert_eq!(
            explanation["weighted_terms"][0]["formula_version"]["id"],
            "hotpath.score.v3"
        );

        let limitation = serde_json::from_str::<serde_json::Value>(
            persisted[0]
                .limitation
                .as_deref()
                .expect("limitation JSON should be stored"),
        )
        .expect("limitation JSON should parse");
        assert_eq!(
            limitation["limitations"][0]["code"],
            "size_uses_byte_size_fallback"
        );

        let replacement = rank_hotspot_scores(&[scores[0].clone()]);
        store
            .persist_hotspots(scan_run.id, &replacement)
            .expect("replacement hotspots should persist");

        let replaced = store
            .latest_hotspots()
            .expect("replacement hotspots should read");
        assert_eq!(
            replaced
                .iter()
                .map(|hotspot| (hotspot.rank, hotspot.path.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "src/stable.rs")]
        );
    }

    #[test]
    fn failed_persist_does_not_delete_previous_file_rows() {
        let fixture = Fixture::new("failed-persist-keeps-files");
        let mut store = IndexStore::open(&fixture.path).expect("index should open");
        let first = scan_report(vec![
            scan_file("src/a.rs", Some(1), Some("Rust"), ContentKind::Text),
            scan_file("src/b.rs", Some(2), Some("Rust"), ContentKind::Text),
        ]);

        store
            .persist_scan(&first)
            .expect("first scan should persist");

        let mut updated = scan_file("src/a.rs", Some(10), Some("Rust"), ContentKind::Text);
        updated.warnings.push(FileWarning {
            code: "line_count_skipped",
            message: "test warning".to_owned(),
        });
        let mut invalid_second = scan_report(vec![
            updated,
            scan_file("../invalid.rs", Some(1), Some("Rust"), ContentKind::Text),
        ]);
        invalid_second.warnings.push(ScanWarning {
            code: "walk_error",
            path: Some("blocked".to_owned()),
            message: "test scan warning".to_owned(),
        });
        let error = store
            .persist_scan(&invalid_second)
            .expect_err("invalid second scan should fail");

        assert!(matches!(
            error,
            IndexError::PersistScan { .. } | IndexError::InvalidScanData { .. }
        ));

        let latest = store
            .latest_scan()
            .expect("latest scan should read")
            .expect("latest scan should exist");
        assert_eq!(latest.run.run_key, "scan-0000000000000001");
        assert_eq!(
            latest
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.byte_size))
                .collect::<Vec<_>>(),
            vec![("src/a.rs", Some(1)), ("src/b.rs", Some(2))]
        );

        let connection = Connection::open(store.path()).expect("index should reopen");
        assert_eq!(table_count(&connection, "scan_runs"), 1);
        assert_eq!(table_count(&connection, "files"), 2);
        assert_eq!(table_count(&connection, "scan_warnings"), 0);
        assert_eq!(table_count(&connection, "file_warnings"), 0);
    }

    #[test]
    fn default_index_path_uses_hotpath_directory() {
        assert_eq!(
            default_index_path(Path::new("repo")),
            Path::new("repo").join(".hotpath").join("index.db")
        );
    }

    #[test]
    fn open_creates_index_directory_and_metadata() {
        let fixture = Fixture::new("open-create");

        let store = IndexStore::open(&fixture.path).expect("index should open");

        assert_eq!(store.path(), default_index_path(&fixture.path));
        assert_eq!(store.schema_version(), CURRENT_SCHEMA_VERSION);
        assert!(fixture.path.join(".hotpath").is_dir());
        assert!(store.path().is_file());
        let connection = Connection::open(store.path()).expect("index should reopen");
        assert_eq!(
            read_metadata_schema_version(&connection, store.path())
                .expect("metadata version should read"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(
            read_metadata_schema_identifier(&connection, store.path())
                .expect("metadata identifier should read"),
            SCHEMA_IDENTIFIER
        );
    }

    #[test]
    fn fresh_migration_creates_initial_schema_tables() {
        let fixture = Fixture::new("initial-schema");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let connection = Connection::open(store.path()).expect("index should reopen");

        let table_names = user_table_names(&connection);
        let mut expected_table_names = REQUIRED_SCHEMA_TABLES
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        expected_table_names.sort();

        assert_eq!(table_names, expected_table_names);
        assert_eq!(
            table_columns(&connection, "files"),
            [
                "id",
                "repo_id",
                "path",
                "byte_size",
                "extension",
                "language",
                "line_count",
                "content_kind",
                "is_vendor",
                "is_generated",
                "is_symlink",
                "classification",
                "scan_run_id",
            ]
        );
        assert_eq!(
            table_columns(&connection, "scan_warnings"),
            [
                "id",
                "scan_run_id",
                "warning_order",
                "code",
                "path",
                "message"
            ]
        );
        assert_eq!(
            table_columns(&connection, "file_warnings"),
            [
                "id",
                "file_id",
                "scan_run_id",
                "warning_order",
                "code",
                "message",
            ]
        );
        assert_eq!(
            table_columns(&connection, "git_analysis_runs"),
            [
                "id",
                "repo_id",
                "analysis_key",
                "status",
                "analyzer_version",
                "git_head",
                "head_commit_time",
                "recent_window_days",
                "metrics_observed",
                "co_changes_observed",
            ]
        );
        assert_eq!(
            table_columns(&connection, "git_file_stats"),
            [
                "file_id",
                "repo_id",
                "analysis_run_id",
                "commits_per_file",
                "total_churn_added",
                "total_churn_deleted",
                "recent_churn_added",
                "recent_churn_deleted",
                "author_count",
                "dominant_owner",
                "dominant_owner_share",
                "first_commit_id",
                "first_commit_time",
                "last_commit_id",
                "last_commit_time",
                "file_age_days",
                "owner_count",
            ]
        );
        assert_eq!(
            table_columns(&connection, "git_co_changes"),
            [
                "id",
                "repo_id",
                "analysis_run_id",
                "left_file_id",
                "right_file_id",
                "left_path",
                "right_path",
                "commit_count",
            ]
        );
        assert_eq!(
            table_columns(&connection, "hotspots"),
            [
                "file_id",
                "repo_id",
                "scan_run_id",
                "score",
                "rank",
                "formula_version",
                "raw_metrics_json",
                "explanation",
                "limitation",
            ]
        );
        assert!(is_strict_table(&connection, "repos"));
        assert!(is_strict_table(&connection, "scan_warnings"));
        assert!(is_strict_table(&connection, "files"));
        assert!(is_strict_table(&connection, "file_warnings"));
        assert!(is_strict_table(&connection, "git_analysis_runs"));
        assert!(is_strict_table(&connection, "git_file_stats"));
        assert!(is_strict_table(&connection, "git_co_changes"));
        assert!(is_strict_table(&connection, "git_commit_diffs"));
        assert!(is_strict_table(&connection, "hotspots"));
        assert!(table_has_foreign_key(&connection, "files", "repos"));
        assert!(table_has_foreign_key(
            &connection,
            "scan_warnings",
            "scan_runs"
        ));
        assert!(table_has_foreign_key(&connection, "file_warnings", "files"));
        assert!(table_has_foreign_key(
            &connection,
            "file_warnings",
            "scan_runs"
        ));
        assert!(table_has_foreign_key(&connection, "symbols", "files"));
        assert!(table_has_foreign_key(
            &connection,
            "git_analysis_runs",
            "repos"
        ));
        assert!(table_has_foreign_key(
            &connection,
            "git_file_stats",
            "files"
        ));
        assert!(table_has_foreign_key(
            &connection,
            "git_file_stats",
            "git_analysis_runs"
        ));
        assert!(table_has_foreign_key(
            &connection,
            "git_co_changes",
            "files"
        ));
        assert!(table_has_foreign_key(
            &connection,
            "git_co_changes",
            "git_analysis_runs"
        ));
        assert!(table_has_foreign_key(
            &connection,
            "git_commit_diffs",
            "repos"
        ));
        assert!(table_has_foreign_key(&connection, "dependencies", "files"));
        assert!(table_has_foreign_key(&connection, "hotspots", "files"));
    }

    #[test]
    fn schema_rejects_bare_dotdot_file_path() {
        let fixture = Fixture::new("reject-file-dotdot");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let connection = Connection::open(store.path()).expect("index should reopen");
        connection
            .execute("INSERT INTO repos (root_key) VALUES (?1);", params!["repo"])
            .expect("repo should insert");

        let error = connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, ?2);",
                params![1, ".."],
            )
            .expect_err("bare dotdot file path should be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(error, _)
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ));
    }

    #[test]
    fn schema_rejects_bare_dotdot_dependency_target_path() {
        let fixture = Fixture::new("reject-dependency-dotdot");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let connection = Connection::open(store.path()).expect("index should reopen");
        connection
            .execute("INSERT INTO repos (root_key) VALUES (?1);", params!["repo"])
            .expect("repo should insert");
        connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, ?2);",
                params![1, "src/lib.rs"],
            )
            .expect("source file should insert");

        let error = connection
            .execute(
                "INSERT INTO dependencies (repo_id, source_file_id, target_path, kind)
                 VALUES (?1, ?2, ?3, ?4);",
                params![1, 1, "..", "import"],
            )
            .expect_err("bare dotdot dependency target path should be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(error, _)
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ));
    }

    #[test]
    fn persist_scan_rejects_unsafe_file_paths() {
        for (index, unsafe_path) in unsafe_path_values().iter().enumerate() {
            let fixture = Fixture::new(&format!("reject-scan-file-path-{index}"));
            let mut store = IndexStore::open(&fixture.path).expect("index should open");
            let scan = scan_report(vec![scan_file(
                unsafe_path,
                Some(1),
                Some("Rust"),
                ContentKind::Text,
            )]);

            let error = store
                .persist_scan(&scan)
                .expect_err("unsafe file path should be rejected");

            assert_scan_persist_check_constraint(error);
        }
    }

    #[test]
    fn persist_scan_rejects_unsafe_scan_warning_paths() {
        for (index, unsafe_path) in unsafe_path_values().iter().enumerate() {
            let fixture = Fixture::new(&format!("reject-scan-warning-path-{index}"));
            let mut store = IndexStore::open(&fixture.path).expect("index should open");
            let mut scan = scan_report(Vec::new());
            scan.warnings.push(ScanWarning {
                code: "walk_error",
                path: Some((*unsafe_path).to_owned()),
                message: "test warning".to_owned(),
            });

            let error = store
                .persist_scan(&scan)
                .expect_err("unsafe scan warning path should be rejected");

            assert_scan_persist_check_constraint(error);
        }
    }

    #[test]
    fn persist_git_analysis_rejects_unsafe_paths() {
        for (index, unsafe_path) in unsafe_path_values().iter().enumerate() {
            let fixture = Fixture::new(&format!("reject-git-analysis-path-{index}"));
            let mut store = IndexStore::open(&fixture.path).expect("index should open");
            let co_changes = vec![GitCoChange {
                left_path: "src/a.rs".to_owned(),
                right_path: (*unsafe_path).to_owned(),
                commit_count: 1,
            }];

            let error = store
                .persist_git_analysis(
                    &fixture.path,
                    "2222222222222222222222222222222222222222",
                    1_700_086_400,
                    90,
                    &[],
                    &co_changes,
                )
                .expect_err("unsafe Git analysis path should be rejected");

            assert_git_persist_check_constraint(error);
        }
    }

    #[test]
    fn schema_rejects_unsafe_dependency_target_paths() {
        let fixture = Fixture::new("reject-dependency-unsafe-paths");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let connection = Connection::open(store.path()).expect("index should reopen");
        connection
            .execute("INSERT INTO repos (root_key) VALUES (?1);", params!["repo"])
            .expect("repo should insert");
        connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, ?2);",
                params![1, "src/lib.rs"],
            )
            .expect("source file should insert");

        for unsafe_path in unsafe_path_values() {
            let error = connection
                .execute(
                    "INSERT INTO dependencies (repo_id, source_file_id, target_path, kind)
                     VALUES (?1, ?2, ?3, ?4);",
                    params![1, 1, unsafe_path, "import"],
                )
                .expect_err("unsafe dependency target path should be rejected");

            assert_sqlite_check_constraint(error);
        }
    }

    #[test]
    fn schema_rejects_unsafe_git_co_change_paths() {
        let fixture = Fixture::new("reject-git-co-change-unsafe-paths");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let connection = Connection::open(store.path()).expect("index should reopen");
        connection
            .execute("INSERT INTO repos (root_key) VALUES (?1);", params!["repo"])
            .expect("repo should insert");
        connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, ?2);",
                params![1, "src/left.rs"],
            )
            .expect("left file should insert");
        let left_file_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO files (repo_id, path) VALUES (?1, ?2);",
                params![1, "src/right.rs"],
            )
            .expect("right file should insert");
        let right_file_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO git_analysis_runs (
                    repo_id,
                    analysis_key,
                    status,
                    git_head,
                    head_commit_time,
                    recent_window_days,
                    metrics_observed,
                    co_changes_observed
                )
                VALUES (1, 'test', 'completed', 'abc123', 1, 90, 0, 1);",
                [],
            )
            .expect("Git analysis run should insert");
        let analysis_run_id = connection.last_insert_rowid();

        for unsafe_path in unsafe_path_values() {
            let error = connection
                .execute(
                    "INSERT INTO git_co_changes (
                        repo_id,
                        analysis_run_id,
                        left_file_id,
                        right_file_id,
                        left_path,
                        right_path,
                        commit_count
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
                    params![
                        1,
                        analysis_run_id,
                        left_file_id,
                        right_file_id,
                        "0.rs",
                        unsafe_path,
                        1
                    ],
                )
                .expect_err("unsafe git co-change right path should be rejected");

            assert_sqlite_check_constraint(error);
        }

        let error = connection
            .execute(
                "INSERT INTO git_co_changes (
                    repo_id,
                    analysis_run_id,
                    left_file_id,
                    right_file_id,
                    left_path,
                    right_path,
                    commit_count
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
                params![
                    1,
                    analysis_run_id,
                    left_file_id,
                    right_file_id,
                    "C:/repo/left.rs",
                    "zz.rs",
                    1
                ],
            )
            .expect_err("unsafe git co-change left path should be rejected");

        assert_sqlite_check_constraint(error);
    }

    #[test]
    fn repeated_open_is_idempotent() {
        let fixture = Fixture::new("idempotent");

        let first = IndexStore::open(&fixture.path).expect("first open should migrate");
        assert_eq!(first.schema_version(), CURRENT_SCHEMA_VERSION);
        let index_path = first.path().to_path_buf();
        let first_connection = Connection::open(&index_path).expect("index should reopen");
        let first_tables = user_table_names(&first_connection);
        let first_metadata = metadata_rows(&first_connection);
        drop(first_connection);
        drop(first);

        let second = IndexStore::open(&fixture.path).expect("second open should be valid");
        assert_eq!(second.schema_version(), CURRENT_SCHEMA_VERSION);
        let connection = Connection::open(second.path()).expect("index should reopen");
        assert_eq!(
            read_user_version(&connection, second.path())
                .expect("user_version should remain readable"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(user_table_names(&connection), first_tables);
        assert_eq!(metadata_rows(&connection), first_metadata);
    }

    #[test]
    fn repeated_open_after_version_zero_metadata_migration_is_idempotent() {
        let fixture = Fixture::new("idempotent-legacy-metadata");
        let index_path = default_index_path(&fixture.path);
        create_legacy_metadata_only_index(
            &index_path,
            &[
                (SCHEMA_VERSION_KEY, "0"),
                (SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER),
            ],
        );

        let first = IndexStore::open(&fixture.path).expect("legacy metadata should migrate");
        assert_eq!(first.schema_version(), CURRENT_SCHEMA_VERSION);
        let first_connection = Connection::open(&index_path).expect("index should reopen");
        assert_eq!(
            read_user_version(&first_connection, &index_path)
                .expect("user_version should read after migration"),
            CURRENT_SCHEMA_VERSION
        );
        let first_tables = user_table_names(&first_connection);
        let first_metadata = metadata_rows(&first_connection);
        drop(first_connection);
        drop(first);

        let second = IndexStore::open(&fixture.path).expect("current schema should reopen");
        assert_eq!(second.schema_version(), CURRENT_SCHEMA_VERSION);
        let second_connection = Connection::open(&index_path).expect("index should reopen");

        assert_eq!(user_table_names(&second_connection), first_tables);
        assert_eq!(metadata_rows(&second_connection), first_metadata);
    }

    #[test]
    fn open_rejects_incompatible_future_user_version() {
        let fixture = Fixture::new("future-user-version");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch("PRAGMA user_version = 5;")
            .expect("test schema version should be set");
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("future schema should fail");

        assert!(matches!(
            error,
            IndexError::IncompatibleFutureSchema {
                found_version: 5,
                supported_version: CURRENT_SCHEMA_VERSION,
                ..
            }
        ));
    }

    #[test]
    fn open_rejects_current_schema_missing_schema_version_metadata() {
        let fixture = Fixture::new("missing-current-schema-version");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute(
                "DELETE FROM hotpath_metadata WHERE key = ?1;",
                params![SCHEMA_VERSION_KEY],
            )
            .expect("schema_version metadata row should be removed");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("missing schema_version should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        let message = error.to_string();
        assert!(message.contains(index_path.to_string_lossy().as_ref()));
        assert!(message.contains("missing schema_version metadata row"));
    }

    #[test]
    fn open_rejects_current_schema_missing_schema_identifier_metadata() {
        let fixture = Fixture::new("missing-current-schema-identifier");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute(
                "DELETE FROM hotpath_metadata WHERE key = ?1;",
                params![SCHEMA_IDENTIFIER_KEY],
            )
            .expect("schema_identifier metadata row should be removed");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("missing schema_identifier should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        let message = error.to_string();
        assert!(message.contains(index_path.to_string_lossy().as_ref()));
        assert!(message.contains("missing schema_identifier metadata row"));
    }

    #[test]
    fn open_reports_corrupt_metadata() {
        let fixture = Fixture::new("corrupt-metadata");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                ) STRICT;
                PRAGMA user_version = 1;",
            )
            .expect("test metadata should be created");
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("metadata should be invalid");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
    }

    #[test]
    fn migration_rejects_malformed_preexisting_metadata_table() {
        let fixture = Fixture::new("malformed-migration-metadata");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT
                );",
            )
            .expect("malformed metadata table should be created");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("malformed metadata should be rejected");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        let connection = Connection::open(&index_path).expect("test database should reopen");
        assert_eq!(
            read_user_version(&connection, &index_path)
                .expect("user_version should remain readable"),
            0
        );
    }

    #[test]
    fn migration_rejects_future_metadata_schema_version_with_zero_user_version() {
        let fixture = Fixture::new("future-metadata-zero-user-version");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                ) STRICT;
                INSERT INTO hotpath_metadata (key, value)
                VALUES ('schema_version', '5'), ('schema_identifier', 'hotpath.index.v4');",
            )
            .expect("future metadata should be created");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("future metadata schema should be rejected");

        assert!(matches!(
            error,
            IndexError::IncompatibleFutureSchema {
                found_version: 5,
                supported_version: CURRENT_SCHEMA_VERSION,
                ..
            }
        ));
        let connection = Connection::open(&index_path).expect("test database should reopen");
        assert_eq!(
            read_user_version(&connection, &index_path)
                .expect("user_version should remain readable"),
            0
        );
    }

    #[test]
    fn migration_rejects_missing_metadata_schema_version_with_zero_user_version() {
        let fixture = Fixture::new("missing-migration-schema-version");
        let index_path = default_index_path(&fixture.path);
        create_legacy_metadata_only_index(
            &index_path,
            &[(SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER)],
        );

        let error =
            IndexStore::open(&fixture.path).expect_err("missing schema_version should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        let message = error.to_string();
        assert!(message.contains("missing schema_version metadata row"));
        let connection = Connection::open(&index_path).expect("test database should reopen");
        assert_eq!(
            read_user_version(&connection, &index_path)
                .expect("user_version should remain readable"),
            0
        );
    }

    #[test]
    fn migration_rejects_missing_metadata_schema_identifier_with_zero_user_version() {
        let fixture = Fixture::new("missing-migration-schema-identifier");
        let index_path = default_index_path(&fixture.path);
        create_legacy_metadata_only_index(&index_path, &[(SCHEMA_VERSION_KEY, "0")]);

        let error =
            IndexStore::open(&fixture.path).expect_err("missing schema_identifier should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        let message = error.to_string();
        assert!(message.contains("missing schema_identifier metadata row"));
        let connection = Connection::open(&index_path).expect("test database should reopen");
        assert_eq!(
            read_user_version(&connection, &index_path)
                .expect("user_version should remain readable"),
            0
        );
    }

    #[test]
    fn migration_rejects_wrong_metadata_identifier_with_zero_user_version() {
        let fixture = Fixture::new("wrong-identifier-zero-user-version");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                ) STRICT;
                INSERT INTO hotpath_metadata (key, value)
                VALUES ('schema_version', '0'), ('schema_identifier', 'other.index');",
            )
            .expect("wrong metadata identifier should be created");
        drop(connection);

        let error = IndexStore::open(&fixture.path)
            .expect_err("wrong metadata identifier should be rejected");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error.to_string().contains("schema identifier"));
        let connection = Connection::open(&index_path).expect("test database should reopen");
        assert_eq!(
            read_user_version(&connection, &index_path)
                .expect("user_version should remain readable"),
            0
        );
    }

    #[test]
    fn open_rejects_current_user_version_with_future_metadata_schema_version() {
        let fixture = Fixture::new("future-current-metadata");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute(
                "UPDATE hotpath_metadata SET value = '5' WHERE key = ?1;",
                params![SCHEMA_VERSION_KEY],
            )
            .expect("metadata schema version should be updated");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("future metadata schema should be rejected");

        assert!(matches!(
            error,
            IndexError::IncompatibleFutureSchema {
                found_version: 5,
                supported_version: CURRENT_SCHEMA_VERSION,
                ..
            }
        ));
        let message = error.to_string();
        assert!(message.contains(index_path.to_string_lossy().as_ref()));
        assert!(message.contains("supports up to version 4"));
    }

    #[test]
    fn open_rejects_current_schema_with_malformed_metadata_table() {
        let fixture = Fixture::new("malformed-current-metadata");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch(
                "CREATE TABLE hotpath_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    extra TEXT
                ) STRICT;
                INSERT INTO hotpath_metadata (key, value)
                VALUES ('schema_version', '3');
                PRAGMA user_version = 3;",
            )
            .expect("malformed current metadata should be created");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("malformed metadata should be rejected");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
    }

    #[test]
    fn open_rejects_current_schema_missing_required_table() {
        let fixture = Fixture::new("missing-required-table");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute_batch("DROP TABLE files;")
            .expect("required table should be removed");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("missing required table should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error.to_string().contains("missing required table files"));
    }

    #[test]
    fn open_rejects_current_schema_with_malformed_required_table() {
        let fixture = Fixture::new("malformed-required-table");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute_batch(
                "DROP TABLE hotspots;
                CREATE TABLE hotspots (
                    file_id INTEGER PRIMARY KEY,
                    repo_id INTEGER NOT NULL,
                    scan_run_id INTEGER,
                    score REAL NOT NULL,
                    rank INTEGER,
                    formula_version TEXT NOT NULL,
                    raw_metrics_json TEXT,
                    explanation TEXT,
                    limitation TEXT
                ) STRICT;",
            )
            .expect("required table should be malformed");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("malformed required table should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error
            .to_string()
            .contains("hotspots is missing required constraint"));
    }

    #[test]
    fn open_rejects_current_schema_missing_files_unique_constraint() {
        let fixture = Fixture::new("missing-files-unique");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        recreate_files_table(&connection, false, true);
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("missing files unique should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error
            .to_string()
            .contains("files is missing required UNIQUE constraint"));
    }

    #[test]
    fn open_rejects_current_schema_missing_path_safety_check() {
        let fixture = Fixture::new("missing-path-safety-check");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        recreate_files_table(&connection, true, false);
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("missing path check should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error.to_string().contains("CHECK (path != '..')"));
    }

    #[test]
    fn open_rejects_current_schema_with_wrong_foreign_key_delete_action() {
        let fixture = Fixture::new("wrong-fk-delete-action");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        recreate_dependencies_table_with_target_delete_action(&connection, "CASCADE");
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("wrong delete action should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error
            .to_string()
            .contains("target_file_id -> files.id ON DELETE SET NULL"));
    }

    #[test]
    fn open_rejects_current_schema_missing_required_index() {
        let fixture = Fixture::new("missing-required-index");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute_batch("DROP INDEX files_by_repo_path;")
            .expect("required index should be dropped");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("missing required index should fail");

        assert!(matches!(error, IndexError::CorruptMetadata { .. }));
        assert!(error
            .to_string()
            .contains("missing required index files_by_repo_path"));
    }

    #[test]
    fn open_reports_failed_integrity_check_as_corrupt_database() {
        let fixture = Fixture::new("integrity-check");
        let store = IndexStore::open(&fixture.path).expect("index should open");
        let index_path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&index_path).expect("test database should reopen");
        connection
            .execute_batch(
                "PRAGMA writable_schema = ON;
                UPDATE sqlite_schema
                SET sql = 'CREATE TABLE hotpath_metadata ('
                WHERE name = 'hotpath_metadata';
                PRAGMA writable_schema = OFF;",
            )
            .expect("test schema should be corrupted");
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("corrupt index should fail");

        assert!(matches!(error, IndexError::CorruptDatabase { .. }));
        assert!(error.to_string().contains("corrupt"));
    }

    #[test]
    fn open_rejects_symlinked_index_directory() {
        let fixture = Fixture::new("symlinked-index-dir");
        let redirect_path = fixture.path.with_file_name(format!(
            "{}-redirect",
            fixture
                .path
                .file_name()
                .expect("fixture should have a file name")
                .to_string_lossy()
        ));
        let _cleanup = CleanupDir {
            path: redirect_path.clone(),
        };
        let _ = fs::remove_dir_all(&redirect_path);
        fs::create_dir_all(&redirect_path).expect("redirect target should be created");

        let hotpath_link = fixture.path.join(".hotpath");
        if let Err(source) = create_directory_symlink(&redirect_path, &hotpath_link) {
            if source.kind() == ErrorKind::PermissionDenied || source.raw_os_error() == Some(1314) {
                return;
            }

            panic!("directory symlink should be created or skipped for permissions: {source}");
        }

        let error = IndexStore::open(&fixture.path).expect_err("symlinked .hotpath should fail");

        assert!(matches!(error, IndexError::UnsafeIndexDir { .. }));
        assert!(!redirect_path.join(INDEX_FILE).exists());
    }

    #[test]
    fn open_reports_create_directory_failure() {
        let fixture = Fixture::new("directory-failure");
        let file_root = fixture.path.join("not-a-directory");
        fs::write(&file_root, b"not a directory").expect("fixture file should be written");

        let error = IndexStore::open(&file_root).expect_err("directory creation should fail");

        assert!(matches!(error, IndexError::CreateIndexDir { .. }));
    }

    #[test]
    fn open_does_not_create_missing_repository_root() {
        let fixture = Fixture::new("missing-root");
        let missing_root = fixture.path.join("missing");

        let error = IndexStore::open(&missing_root).expect_err("missing root should fail");

        assert!(matches!(error, IndexError::CreateIndexDir { .. }));
        assert!(!missing_root.exists());
    }

    #[test]
    fn open_reports_corrupt_database_file() {
        let fixture = Fixture::new("corrupt-database");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        fs::write(&index_path, b"not a sqlite database").expect("corrupt index should be written");

        let error = IndexStore::open(&fixture.path).expect_err("corrupt index should fail");

        assert!(matches!(
            error,
            IndexError::OpenDatabase { .. } | IndexError::CorruptDatabase { .. }
        ));
        let message = error.to_string();
        assert!(message.contains(index_path.to_string_lossy().as_ref()));
        assert!(
            message.contains("failed to open Hotpath index")
                || message.contains("unreadable or corrupt")
        );
    }

    #[test]
    fn open_reports_database_open_failure_with_index_path() {
        let fixture = Fixture::new("database-open-failure");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(&index_path).expect("index path directory should be created");

        let error = IndexStore::open(&fixture.path).expect_err("directory index should fail");

        assert!(matches!(error, IndexError::OpenDatabase { .. }));
        let message = error.to_string();
        assert!(message.contains(index_path.to_string_lossy().as_ref()));
        assert!(message.contains("failed to open Hotpath index"));
    }
}
