// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rusqlite::{params, types::Type, Connection, OptionalExtension, Row, Transaction};

use crate::{ContentKind, FileRecord, FileWarning, ScanReport, ScanWarning, SCAN_SCHEMA_VERSION};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

const HOTPATH_DIR: &str = ".hotpath";
const INDEX_FILE: &str = "index.db";
const SCHEMA_IDENTIFIER: &str = "hotpath.index.v1";
const SCHEMA_IDENTIFIER_KEY: &str = "schema_identifier";
const SCHEMA_VERSION_KEY: &str = "schema_version";
const REQUIRED_SCHEMA_TABLES: &[&str] = &[
    "hotpath_metadata",
    "repos",
    "scan_runs",
    "scan_warnings",
    "files",
    "file_warnings",
    "symbols",
    "git_file_stats",
    "dependencies",
    "hotspots",
];

#[derive(Debug)]
pub struct IndexStore {
    connection: Connection,
    path: PathBuf,
    schema_version: u32,
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

impl IndexStore {
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

        persist_scan_warnings(&transaction, &index_path, scan_run_id, &scan.warnings)?;

        for file in &scan.files {
            let file_id = persist_file(&transaction, &index_path, repo_id, scan_run_id, file)?;
            persist_file_warnings(
                &transaction,
                &index_path,
                file_id,
                scan_run_id,
                &file.warnings,
            )?;
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
    ReadIndex {
        path: PathBuf,
        source: rusqlite::Error,
    },
    InvalidScanData {
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
        }
    }
}

impl StdError for IndexError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateIndexDir { source, .. } => Some(source),
            Self::OpenDatabase { source, .. }
            | Self::CorruptDatabase { source, .. }
            | Self::Migration { source, .. }
            | Self::PersistScan { source, .. }
            | Self::ReadIndex { source, .. } => Some(source),
            Self::CorruptMetadata { .. }
            | Self::UnsafeIndexDir { .. }
            | Self::IncompatibleFutureSchema { .. }
            | Self::InvalidScanData { .. } => None,
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

fn persist_file(
    transaction: &Transaction<'_>,
    index_path: &Path,
    repo_id: i64,
    scan_run_id: i64,
    file: &FileRecord,
) -> Result<i64, IndexError> {
    let byte_size = optional_u64_to_i64(file.byte_size, index_path, "byte_size")?;
    let line_count = optional_u64_to_i64(file.line_count, index_path, "line_count")?;

    transaction
        .execute(
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
            params![
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
            ],
        )
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })?;

    transaction
        .query_row(
            "SELECT id FROM files WHERE repo_id = ?1 AND path = ?2;",
            params![repo_id, &file.path],
            |row| row.get(0),
        )
        .map_err(|source| IndexError::PersistScan {
            path: index_path.to_path_buf(),
            source,
        })
}

fn persist_scan_warnings(
    transaction: &Transaction<'_>,
    index_path: &Path,
    scan_run_id: i64,
    warnings: &[ScanWarning],
) -> Result<(), IndexError> {
    for (index, warning) in warnings.iter().enumerate() {
        let warning_order = warning_order_to_i64(index, index_path)?;
        transaction
            .execute(
                "INSERT INTO scan_warnings (
                    scan_run_id,
                    warning_order,
                    code,
                    path,
                    message
                )
                VALUES (?1, ?2, ?3, ?4, ?5);",
                params![
                    scan_run_id,
                    warning_order,
                    warning.code,
                    warning.path.as_deref(),
                    &warning.message,
                ],
            )
            .map_err(|source| IndexError::PersistScan {
                path: index_path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

fn persist_file_warnings(
    transaction: &Transaction<'_>,
    index_path: &Path,
    file_id: i64,
    scan_run_id: i64,
    warnings: &[FileWarning],
) -> Result<(), IndexError> {
    for (index, warning) in warnings.iter().enumerate() {
        let warning_order = warning_order_to_i64(index, index_path)?;
        transaction
            .execute(
                "INSERT INTO file_warnings (
                    file_id,
                    scan_run_id,
                    warning_order,
                    code,
                    message
                )
                VALUES (?1, ?2, ?3, ?4, ?5);",
                params![
                    file_id,
                    scan_run_id,
                    warning_order,
                    warning.code,
                    &warning.message,
                ],
            )
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

    create_core_schema(&transaction, path)?;
    verify_schema_objects(&transaction, path)?;

    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION.to_string()],
        )
        .map_err(|source| migration_error(path, 0, 1, source))?;
    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_IDENTIFIER_KEY, SCHEMA_IDENTIFIER],
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

fn create_core_schema(connection: &Connection, path: &Path) -> Result<(), IndexError> {
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

const GIT_FILE_STATS_COLUMNS: &[ExpectedColumn] = &[
    expected_column("file_id", "INTEGER", false, None, 1),
    expected_column("commit_count", "INTEGER", true, Some("0"), 0),
    expected_column("churn_added", "INTEGER", true, Some("0"), 0),
    expected_column("churn_deleted", "INTEGER", true, Some("0"), 0),
    expected_column("author_count", "INTEGER", true, Some("0"), 0),
    expected_column("primary_author", "TEXT", false, None, 0),
    expected_column("last_commit", "TEXT", false, None, 0),
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

const GIT_FILE_STATS_FOREIGN_KEYS: &[ExpectedForeignKey] =
    &[expected_foreign_key("file_id", "files", "id", "CASCADE")];

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
const GIT_FILE_STATS_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (commit_count >= 0)",
    "CHECK (churn_added >= 0)",
    "CHECK (churn_deleted >= 0)",
    "CHECK (author_count >= 0)",
];
const DEPENDENCIES_CHECK_CONSTRAINTS: &[&str] = &[
    "CHECK (length(target_path) > 0)",
    "CHECK (target_path != '..')",
    "CHECK (target_path NOT LIKE '/%')",
    "CHECK (target_path NOT LIKE './%')",
    "CHECK (target_path NOT LIKE '../%')",
    "CHECK (target_path NOT LIKE '%/../%')",
    "CHECK (target_path NOT LIKE '%/..')",
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
        "git_file_stats" => GIT_FILE_STATS_COLUMNS,
        "dependencies" => DEPENDENCIES_COLUMNS,
        "hotspots" => HOTSPOTS_COLUMNS,
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
        "git_file_stats" => GIT_FILE_STATS_FOREIGN_KEYS,
        "dependencies" => DEPENDENCIES_FOREIGN_KEYS,
        "hotspots" => HOTSPOTS_FOREIGN_KEYS,
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
        "dependencies" => DEPENDENCIES_UNIQUE_CONSTRAINTS,
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
        "git_file_stats" => GIT_FILE_STATS_CHECK_CONSTRAINTS,
        "dependencies" => DEPENDENCIES_CHECK_CONSTRAINTS,
        "hotspots" => HOTSPOTS_CHECK_CONSTRAINTS,
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

    use crate::{FileWarning, ScanWarning};

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
                "INSERT INTO git_file_stats (file_id, commit_count, churn_added, churn_deleted, author_count)
                 VALUES (?1, 1, 2, 3, 1);",
                params![stale_id],
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

        let invalid_second = scan_report(vec![
            scan_file("src/a.rs", Some(10), Some("Rust"), ContentKind::Text),
            scan_file("../invalid.rs", Some(1), Some("Rust"), ContentKind::Text),
        ]);
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
    fn open_rejects_incompatible_future_user_version() {
        let fixture = Fixture::new("future-user-version");
        let index_path = default_index_path(&fixture.path);
        fs::create_dir_all(index_path.parent().expect("index path should have parent"))
            .expect("index directory should be created");
        let connection = Connection::open(&index_path).expect("test database should open");
        connection
            .execute_batch("PRAGMA user_version = 2;")
            .expect("test schema version should be set");
        drop(connection);

        let error = IndexStore::open(&fixture.path).expect_err("future schema should fail");

        assert!(matches!(
            error,
            IndexError::IncompatibleFutureSchema {
                found_version: 2,
                supported_version: CURRENT_SCHEMA_VERSION,
                ..
            }
        ));
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
                VALUES ('schema_version', '2'), ('schema_identifier', 'hotpath.index.v1');",
            )
            .expect("future metadata should be created");
        drop(connection);

        let error =
            IndexStore::open(&fixture.path).expect_err("future metadata schema should be rejected");

        assert!(matches!(
            error,
            IndexError::IncompatibleFutureSchema {
                found_version: 2,
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
                VALUES ('schema_version', '1');
                PRAGMA user_version = 1;",
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
    }
}
