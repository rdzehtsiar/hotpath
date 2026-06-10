// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use crate::pipeline::events::PipelineEvent;
use crate::pipeline::file_analyzer::{
    ContentKind, FileAnalysisResult, FileDiagnostic, FileParserStatus,
};
use crate::pipeline::file_risk_assessor::{
    FileRiskAssessment, FileRiskAssessor, FileRiskInput, RepositoryRiskContext, FORMULA_ID,
};
use crate::pipeline::git_history_analyzer::{
    GitChunkSummary, GitCochangeDelta, GitFileAuthorDelta, GitFileMetricDelta, GitHistoryError,
    GitHistorySink, GitRepositoryDelta,
};
use crate::pipeline::repo_risk_assessor::{
    ProjectFileRiskInput, ProjectFileRiskTermInput, RepoRiskAssessment, RepoRiskAssessor,
    RepoRiskInput,
};

pub const DEFAULT_STORE_BATCH_SIZE: usize = 1_000;
pub const DEFAULT_STORE_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
pub const DEFAULT_STORE_QUEUE_CAPACITY: usize = 256;

const INDEX_DIR: &str = ".hotpath";
const INDEX_DB: &str = "index.sqlite";
pub const INDEX_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreReducerOptions {
    pub batch_size: usize,
    pub flush_interval: Duration,
    pub queue_capacity: usize,
    pub active_scan_id: i64,
}

impl Default for StoreReducerOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_STORE_BATCH_SIZE,
            flush_interval: DEFAULT_STORE_FLUSH_INTERVAL,
            queue_capacity: DEFAULT_STORE_QUEUE_CAPACITY,
            active_scan_id: 0,
        }
    }
}

#[derive(Debug)]
pub struct StoreReducer {
    handle: StoreReducerHandle,
    join: JoinHandle<Result<StoreReducerStats, StoreReducerError>>,
}

impl StoreReducer {
    pub fn start(
        root: impl AsRef<Path>,
        options: StoreReducerOptions,
        event_sender: mpsc::Sender<PipelineEvent>,
    ) -> Result<Self, StoreReducerError> {
        let db_dir = root.as_ref().join(INDEX_DIR);
        fs::create_dir_all(&db_dir).map_err(|source| StoreReducerError::CreateIndexDirectory {
            path: db_dir.clone(),
            source,
        })?;

        let root = fs::canonicalize(root.as_ref()).unwrap_or_else(|_| root.as_ref().to_path_buf());
        let db_path = db_dir.join(INDEX_DB);
        let (sender, receiver) = mpsc::sync_channel(options.queue_capacity.max(1));
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let handle = StoreReducerHandle { sender };
        let join = thread::spawn(move || {
            reducer_loop(root, db_path, options, receiver, event_sender, ready_sender)
        });

        match ready_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                let _ = join.join();
                return Err(StoreReducerError::Startup(message));
            }
            Err(_) => {
                let _ = join.join();
                return Err(StoreReducerError::ThreadPanicked);
            }
        }

        Ok(Self { handle, join })
    }

    pub fn handle(&self) -> StoreReducerHandle {
        self.handle.clone()
    }

    pub fn finish(self) -> Result<StoreReducerStats, StoreReducerError> {
        let _ = self.handle.finish();
        self.join
            .join()
            .map_err(|_| StoreReducerError::ThreadPanicked)?
    }
}

#[derive(Debug, Clone)]
pub struct StoreReducerHandle {
    sender: SyncSender<StoreMessage>,
}

impl StoreReducerHandle {
    pub fn store_file_analysis(&self, result: FileAnalysisResult) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::FileAnalysis(Box::new(result)))
    }

    pub fn mark_file_reused(&self, path: PathBuf) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::FileReused(path))
    }

    pub fn mark_unseen_files_inactive(&self) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::MarkUnseenFilesInactive)
    }

    pub fn clear_git_data(&self) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::ClearGitData)
    }

    pub fn store_git_chunk_summary(
        &self,
        summary: GitChunkSummary,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitChunkSummary(summary))
    }

    pub fn store_git_file_metrics(
        &self,
        metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitFileMetrics(metrics))
    }

    pub fn store_git_file_authors(
        &self,
        authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitFileAuthors(authors))
    }

    pub fn store_git_cochanges(
        &self,
        cochanges: Vec<GitCochangeDelta>,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitCochanges(cochanges))
    }

    pub fn store_git_repository_delta(
        &self,
        delta: GitRepositoryDelta,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitRepositoryDelta(delta))
    }

    pub fn store_git_repository_summary(
        &self,
        summary: GitRepositorySummaryInput,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::GitRepositorySummary(summary))
    }

    pub fn store_metadata(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::StageMetadata {
            key: key.into(),
            value: value.into(),
        })
    }

    pub fn store_scan_state(
        &self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::ScanState {
            key: key.into(),
            value: value.into(),
        })
    }

    pub fn plan_records(&self, records: u64) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::PlannedRecords(records))
    }

    pub fn plan_file_records(&self, files: u64) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::PlannedFileRecords(files))
    }

    pub fn plan_finalization_records(&self) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::PlanFinalizationRecords)
    }

    fn finish(&self) -> Result<(), StoreReducerError> {
        self.send(StoreMessage::Finish)
    }

    fn send(&self, message: StoreMessage) -> Result<(), StoreReducerError> {
        self.sender
            .send(message)
            .map_err(|_| StoreReducerError::QueueClosed)
    }
}

impl GitHistorySink for StoreReducerHandle {
    fn store_git_chunk_summary(&self, summary: GitChunkSummary) -> Result<(), GitHistoryError> {
        StoreReducerHandle::store_git_chunk_summary(self, summary)
            .map_err(|error| GitHistoryError::Sink(error.to_string()))
    }

    fn store_git_file_metrics(
        &self,
        metrics: Vec<GitFileMetricDelta>,
    ) -> Result<(), GitHistoryError> {
        StoreReducerHandle::store_git_file_metrics(self, metrics)
            .map_err(|error| GitHistoryError::Sink(error.to_string()))
    }

    fn store_git_file_authors(
        &self,
        authors: Vec<GitFileAuthorDelta>,
    ) -> Result<(), GitHistoryError> {
        StoreReducerHandle::store_git_file_authors(self, authors)
            .map_err(|error| GitHistoryError::Sink(error.to_string()))
    }

    fn store_git_cochanges(&self, cochanges: Vec<GitCochangeDelta>) -> Result<(), GitHistoryError> {
        StoreReducerHandle::store_git_cochanges(self, cochanges)
            .map_err(|error| GitHistoryError::Sink(error.to_string()))
    }

    fn store_git_repository_delta(&self, delta: GitRepositoryDelta) -> Result<(), GitHistoryError> {
        StoreReducerHandle::store_git_repository_delta(self, delta)
            .map_err(|error| GitHistoryError::Sink(error.to_string()))
    }
}

#[derive(Debug)]
enum StoreMessage {
    FileAnalysis(Box<FileAnalysisResult>),
    GitChunkSummary(GitChunkSummary),
    GitFileMetrics(Vec<GitFileMetricDelta>),
    GitFileAuthors(Vec<GitFileAuthorDelta>),
    GitCochanges(Vec<GitCochangeDelta>),
    GitRepositoryDelta(GitRepositoryDelta),
    GitRepositorySummary(GitRepositorySummaryInput),
    StageMetadata { key: String, value: String },
    ScanState { key: String, value: String },
    PlannedRecords(u64),
    PlannedFileRecords(u64),
    PlanFinalizationRecords,
    FileReused(PathBuf),
    MarkUnseenFilesInactive,
    ClearGitData,
    Finish,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreReducerStats {
    pub planned_records: u64,
    pub planned_file_fact_rows: u64,
    pub planned_finalization_records: u64,
    pub stored_records: u64,
    pub file_rows: u64,
    pub git_chunk_rows: u64,
    pub git_file_metric_rows: u64,
    pub git_file_owner_rows: u64,
    pub git_cochange_rows: u64,
    pub git_repository_delta_rows: u64,
    pub metadata_rows: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitRepositorySummaryInput {
    pub head_commit: Option<String>,
    pub head_timestamp: Option<i64>,
    pub total_commits: u64,
    pub is_shallow: bool,
    pub is_skipped: bool,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredScanState {
    pub completed: bool,
    pub last_indexed_head: Option<String>,
    pub git_options_signature: Option<String>,
    pub file_analysis_signature: Option<String>,
    pub root: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FileReuseIndex {
    entries: BTreeMap<String, StoredFileIdentity>,
}

impl FileReuseIndex {
    pub fn is_current(&self, path: &Path) -> bool {
        let Some(current) = file_identity(path) else {
            return false;
        };
        self.entries
            .get(&path.to_string_lossy().to_string())
            .is_some_and(|stored| {
                stored.byte_size == current.byte_size && stored.mtime_ms == current.mtime_ms
            })
    }
}

#[derive(Debug, Clone)]
struct StoredFileIdentity {
    byte_size: u64,
    mtime_ms: Option<i64>,
}

pub fn read_scan_state(root: impl AsRef<Path>) -> Result<StoredScanState, StoreReducerError> {
    let db_path = root.as_ref().join(INDEX_DIR).join(INDEX_DB);
    if !db_path.exists() {
        return Ok(StoredScanState::default());
    }
    let connection = open_database(&db_path)?;
    initialize_database(&connection)?;
    Ok(StoredScanState {
        completed: read_scan_state_value(&connection, "last_scan_completed")?
            .is_some_and(|value| value == "1"),
        last_indexed_head: read_scan_state_value(&connection, "last_indexed_head")?,
        git_options_signature: read_scan_state_value(&connection, "git_options_signature")?,
        file_analysis_signature: read_scan_state_value(&connection, "file_analysis_signature")?,
        root: read_scan_state_value(&connection, "root")?,
    })
}

pub fn file_analysis_is_current(
    root: impl AsRef<Path>,
    path: impl AsRef<Path>,
) -> Result<bool, StoreReducerError> {
    let db_path = root.as_ref().join(INDEX_DIR).join(INDEX_DB);
    if !db_path.exists() {
        return Ok(false);
    }
    let connection = open_database(&db_path)?;
    initialize_database(&connection)?;
    if read_scan_state_value(&connection, "last_scan_completed")?.is_none_or(|value| value != "1") {
        return Ok(false);
    }
    let Some(identity) = file_identity(path.as_ref()) else {
        return Ok(false);
    };
    let mut statement = connection
        .prepare(
            "
            SELECT byte_size, mtime_ms
            FROM file_analysis
            WHERE path = ?1 AND is_active = 1
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let row = statement.query_row([path.as_ref().to_string_lossy()], |row| {
        Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
    });
    match row {
        Ok((Some(byte_size), stored_mtime_ms)) => {
            Ok(byte_size as u64 == identity.byte_size && stored_mtime_ms == identity.mtime_ms)
        }
        Ok((None, _)) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(source) => Err(StoreReducerError::WriteDatabase(source)),
    }
}

pub fn load_file_reuse_index(
    root: impl AsRef<Path>,
    expected_file_analysis_signature: &str,
) -> Result<FileReuseIndex, StoreReducerError> {
    let db_path = root.as_ref().join(INDEX_DIR).join(INDEX_DB);
    if !db_path.exists() {
        return Ok(FileReuseIndex::default());
    }
    let connection = open_database(&db_path)?;
    initialize_database(&connection)?;
    if read_scan_state_value(&connection, "last_scan_completed")?.is_none_or(|value| value != "1") {
        return Ok(FileReuseIndex::default());
    }
    if read_scan_state_value(&connection, "file_analysis_signature")?.as_deref()
        != Some(expected_file_analysis_signature)
    {
        return Ok(FileReuseIndex::default());
    }

    let mut statement = connection
        .prepare(
            "
            SELECT path, byte_size, mtime_ms
            FROM file_analysis
            WHERE is_active = 1 AND byte_size IS NOT NULL
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut entries = BTreeMap::new();
    for row in rows {
        let (path, byte_size, mtime_ms) = row.map_err(StoreReducerError::WriteDatabase)?;
        entries.insert(
            path,
            StoredFileIdentity {
                byte_size: byte_size as u64,
                mtime_ms,
            },
        );
    }
    Ok(FileReuseIndex { entries })
}

#[derive(Debug)]
pub enum StoreReducerError {
    CreateIndexDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    WriteDatabase(rusqlite::Error),
    Startup(String),
    QueueClosed,
    ThreadPanicked,
}

impl fmt::Display for StoreReducerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateIndexDirectory { path, source } => {
                write!(
                    f,
                    "failed to create Hotpath index directory '{}': {source}",
                    path.display()
                )
            }
            Self::OpenDatabase { path, source } => {
                write!(
                    f,
                    "failed to open Hotpath SQLite index '{}': {source}",
                    path.display()
                )
            }
            Self::WriteDatabase(source) => {
                write!(f, "failed to write Hotpath SQLite index: {source}")
            }
            Self::Startup(message) => write!(f, "failed to start Hotpath store reducer: {message}"),
            Self::QueueClosed => write!(f, "store reducer queue is closed"),
            Self::ThreadPanicked => write!(f, "store reducer thread panicked"),
        }
    }
}

impl StdError for StoreReducerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateIndexDirectory { source, .. } => Some(source),
            Self::OpenDatabase { source, .. } => Some(source),
            Self::WriteDatabase(source) => Some(source),
            Self::Startup(_) | Self::QueueClosed | Self::ThreadPanicked => None,
        }
    }
}

#[derive(Debug, Default)]
struct StoreBatch {
    file_results: Vec<FileAnalysisResult>,
    git_chunk_summaries: Vec<GitChunkSummary>,
    git_file_metrics: Vec<GitFileMetricDelta>,
    git_file_authors: Vec<GitFileAuthorDelta>,
    git_cochanges: Vec<GitCochangeDelta>,
    git_repository_deltas: Vec<GitRepositoryDelta>,
    git_repository_summary: Option<GitRepositorySummaryInput>,
    metadata: Vec<(String, String)>,
    scan_state: Vec<(String, String)>,
    planned_records: u64,
    planned_file_records: u64,
    plan_finalization_records: bool,
    reused_files: Vec<PathBuf>,
    mark_unseen_files_inactive: bool,
    clear_git_data: bool,
}

impl StoreBatch {
    fn push(&mut self, message: StoreMessage) {
        match message {
            StoreMessage::FileAnalysis(result) => self.file_results.push(*result),
            StoreMessage::GitChunkSummary(summary) => self.git_chunk_summaries.push(summary),
            StoreMessage::GitFileMetrics(metrics) => self.git_file_metrics.extend(metrics),
            StoreMessage::GitFileAuthors(authors) => self.git_file_authors.extend(authors),
            StoreMessage::GitCochanges(cochanges) => self.git_cochanges.extend(cochanges),
            StoreMessage::GitRepositoryDelta(delta) => self.git_repository_deltas.push(delta),
            StoreMessage::GitRepositorySummary(summary) => {
                self.git_repository_summary = Some(summary)
            }
            StoreMessage::StageMetadata { key, value } => self.metadata.push((key, value)),
            StoreMessage::ScanState { key, value } => self.scan_state.push((key, value)),
            StoreMessage::PlannedRecords(records) => self.planned_records += records,
            StoreMessage::PlannedFileRecords(files) => {
                self.planned_file_records += files;
            }
            StoreMessage::PlanFinalizationRecords => self.plan_finalization_records = true,
            StoreMessage::FileReused(path) => self.reused_files.push(path),
            StoreMessage::MarkUnseenFilesInactive => self.mark_unseen_files_inactive = true,
            StoreMessage::ClearGitData => self.clear_git_data = true,
            StoreMessage::Finish => {}
        }
    }

    fn progress_record_count(&self) -> u64 {
        self.file_results.len() as u64
            + self.git_chunk_summaries.len() as u64
            + self.git_file_metrics.len() as u64
            + self.git_file_authors.len() as u64
            + self.git_cochanges.len() as u64
            + self.git_repository_deltas.len() as u64
            + u64::from(self.git_repository_summary.is_some())
            + self.reused_files.len() as u64
    }

    fn dynamic_planned_record_count(&self) -> u64 {
        self.git_chunk_summaries.len() as u64
            + self.git_file_metrics.len() as u64
            + self.git_file_authors.len() as u64
            + self.git_cochanges.len() as u64
            + self.git_repository_deltas.len() as u64
            + u64::from(self.git_repository_summary.is_some())
    }

    fn total_len(&self) -> usize {
        self.file_results.len()
            + self.git_chunk_summaries.len()
            + self.git_file_metrics.len()
            + self.git_file_authors.len()
            + self.git_cochanges.len()
            + self.git_repository_deltas.len()
            + self.metadata.len()
            + self.scan_state.len()
            + usize::from(self.planned_records > 0)
            + usize::from(self.planned_file_records > 0)
            + usize::from(self.plan_finalization_records)
            + self.reused_files.len()
            + usize::from(self.git_repository_summary.is_some())
            + usize::from(self.mark_unseen_files_inactive)
            + usize::from(self.clear_git_data)
    }

    fn is_empty(&self) -> bool {
        self.total_len() == 0
    }
}

fn reducer_loop(
    root: PathBuf,
    db_path: PathBuf,
    options: StoreReducerOptions,
    receiver: mpsc::Receiver<StoreMessage>,
    event_sender: mpsc::Sender<PipelineEvent>,
    ready_sender: mpsc::SyncSender<Result<(), String>>,
) -> Result<StoreReducerStats, StoreReducerError> {
    let started = Instant::now();
    let mut connection = match open_database(&db_path).and_then(|connection| {
        initialize_database(&connection)?;
        Ok(connection)
    }) {
        Ok(connection) => {
            let _ = ready_sender.send(Ok(()));
            connection
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready_sender.send(Err(message.clone()));
            return Err(StoreReducerError::Startup(message));
        }
    };

    let mut batch = StoreBatch::default();
    let mut stats = StoreReducerStats::default();
    let batch_size = options.batch_size.max(1);
    let flush_interval = options.flush_interval;

    loop {
        match receiver.recv_timeout(flush_interval) {
            Ok(StoreMessage::Finish) => {
                flush_batch(
                    &root,
                    &mut connection,
                    &mut batch,
                    &mut stats,
                    &event_sender,
                    started,
                    options.active_scan_id,
                )?;
                finalize_git_tables(&root, &mut connection, &mut stats, &event_sender, started)?;
                let _ = event_sender.send(PipelineEvent::StoreCompleted {
                    stored_records: stats.stored_records,
                    elapsed: started.elapsed(),
                });
                return Ok(stats);
            }
            Ok(message) => {
                batch.push(message);
                if batch.total_len() >= batch_size {
                    flush_batch(
                        &root,
                        &mut connection,
                        &mut batch,
                        &mut stats,
                        &event_sender,
                        started,
                        options.active_scan_id,
                    )?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_batch(
                    &root,
                    &mut connection,
                    &mut batch,
                    &mut stats,
                    &event_sender,
                    started,
                    options.active_scan_id,
                )?;
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_batch(
                    &root,
                    &mut connection,
                    &mut batch,
                    &mut stats,
                    &event_sender,
                    started,
                    options.active_scan_id,
                )?;
                finalize_git_tables(&root, &mut connection, &mut stats, &event_sender, started)?;
                let _ = event_sender.send(PipelineEvent::StoreCompleted {
                    stored_records: stats.stored_records,
                    elapsed: started.elapsed(),
                });
                return Ok(stats);
            }
        }
    }
}

fn open_database(path: &Path) -> Result<Connection, StoreReducerError> {
    let connection = Connection::open(path).map_err(|source| StoreReducerError::OpenDatabase {
        path: path.to_path_buf(),
        source,
    })?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(StoreReducerError::WriteDatabase)?;
    Ok(connection)
}

fn read_scan_state_value(
    connection: &Connection,
    key: &str,
) -> Result<Option<String>, StoreReducerError> {
    match connection.query_row(
        "SELECT value FROM scan_state WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(StoreReducerError::WriteDatabase(source)),
    }
}

fn initialize_database(connection: &Connection) -> Result<(), StoreReducerError> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS file_analysis (
                path TEXT PRIMARY KEY NOT NULL,
                relative_path TEXT,
                byte_size INTEGER,
                mtime_ms INTEGER,
                active_scan_id INTEGER NOT NULL DEFAULT 0,
                is_active INTEGER NOT NULL DEFAULT 1,
                extension TEXT,
                content_kind TEXT NOT NULL,
                line_count INTEGER,
                is_generated INTEGER NOT NULL,
                is_vendor INTEGER NOT NULL,
                parser_status TEXT NOT NULL,
                parser_recognition_attempts INTEGER NOT NULL,
                language_id TEXT,
                symbol_count INTEGER NOT NULL DEFAULT 0,
                function_count INTEGER NOT NULL DEFAULT 0,
                method_count INTEGER NOT NULL DEFAULT 0,
                type_count INTEGER NOT NULL DEFAULT 0,
                import_count INTEGER NOT NULL DEFAULT 0,
                complexity_pressure INTEGER,
                max_function_complexity_pressure INTEGER,
                diagnostics TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS git_chunks (
                chunk_index INTEGER PRIMARY KEY NOT NULL,
                commits_processed INTEGER NOT NULL,
                file_changes INTEGER NOT NULL,
                cochange_pairs INTEGER NOT NULL DEFAULT 0,
                broad_commits_skipped_for_cochange INTEGER NOT NULL DEFAULT 0,
                max_touched_files INTEGER NOT NULL DEFAULT 0,
                broadest_commit TEXT
            );

            CREATE TABLE IF NOT EXISTS git_repository_summary (
                id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
                head_commit TEXT,
                head_timestamp INTEGER,
                total_commits INTEGER NOT NULL DEFAULT 0,
                first_commit_timestamp INTEGER,
                last_commit_timestamp INTEGER,
                repository_age_days INTEGER,
                repository_author_count INTEGER NOT NULL DEFAULT 0,
                is_shallow INTEGER NOT NULL DEFAULT 0,
                is_skipped INTEGER NOT NULL DEFAULT 0,
                skip_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS git_file_metrics (
                path TEXT PRIMARY KEY NOT NULL,
                commits_per_file INTEGER NOT NULL DEFAULT 0,
                total_added_lines INTEGER NOT NULL DEFAULT 0,
                total_deleted_lines INTEGER NOT NULL DEFAULT 0,
                total_churn_lines INTEGER NOT NULL DEFAULT 0,
                recent_added_lines INTEGER NOT NULL DEFAULT 0,
                recent_deleted_lines INTEGER NOT NULL DEFAULT 0,
                recent_churn_lines INTEGER NOT NULL DEFAULT 0,
                author_count INTEGER NOT NULL DEFAULT 0,
                first_touch_timestamp INTEGER,
                last_touch_timestamp INTEGER,
                file_age_days INTEGER,
                owner_count INTEGER,
                dominant_owner TEXT,
                dominant_owner_share REAL,
                co_changed_file_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS git_file_owners (
                path TEXT NOT NULL,
                owner_rank INTEGER NOT NULL,
                author TEXT NOT NULL,
                ownership_score REAL NOT NULL,
                ownership_share REAL NOT NULL,
                touch_count INTEGER NOT NULL,
                PRIMARY KEY (path, owner_rank)
            );

            CREATE TABLE IF NOT EXISTS git_cochanges (
                left_path TEXT NOT NULL,
                right_path TEXT NOT NULL,
                co_change_count INTEGER NOT NULL,
                PRIMARY KEY (left_path, right_path)
            );

            CREATE INDEX IF NOT EXISTS idx_git_cochanges_left_path
                ON git_cochanges(left_path);
            CREATE INDEX IF NOT EXISTS idx_git_cochanges_right_path
                ON git_cochanges(right_path);

            CREATE TABLE IF NOT EXISTS git_file_cochange_counts (
                path TEXT PRIMARY KEY NOT NULL,
                co_changed_file_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS git_file_author_accumulators (
                path TEXT NOT NULL,
                author TEXT NOT NULL,
                touch_count INTEGER NOT NULL DEFAULT 0,
                meaningful_commit_count INTEGER NOT NULL DEFAULT 0,
                effective_changed_lines INTEGER NOT NULL DEFAULT 0,
                ownership_line_recency_score REAL NOT NULL DEFAULT 0.0,
                PRIMARY KEY (path, author)
            );

            CREATE INDEX IF NOT EXISTS idx_git_file_author_accumulators_path
                ON git_file_author_accumulators(path);

            CREATE TABLE IF NOT EXISTS git_repository_authors (
                author TEXT PRIMARY KEY NOT NULL
            );

            CREATE TABLE IF NOT EXISTS stage_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS scan_state (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS source_dependency_references (
                source_path TEXT NOT NULL,
                source_package TEXT NOT NULL,
                language_id TEXT,
                reference_index INTEGER NOT NULL,
                reference_kind TEXT NOT NULL,
                raw_target TEXT NOT NULL,
                resolved_package TEXT,
                is_resolved INTEGER NOT NULL DEFAULT 0,
                active_scan_id INTEGER NOT NULL,
                is_active INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (source_path, reference_index)
            );

            CREATE INDEX IF NOT EXISTS idx_source_dependency_references_active
                ON source_dependency_references(active_scan_id, is_active);
            CREATE INDEX IF NOT EXISTS idx_source_dependency_references_resolved
                ON source_dependency_references(resolved_package);

            CREATE TABLE IF NOT EXISTS source_file_packages (
                file_path TEXT PRIMARY KEY NOT NULL,
                package_path TEXT NOT NULL,
                language_id TEXT,
                active_scan_id INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_source_file_packages_package
                ON source_file_packages(package_path);

            CREATE TABLE IF NOT EXISTS source_dependency_edges (
                source_path TEXT NOT NULL,
                source_package TEXT NOT NULL,
                target_package TEXT NOT NULL,
                reference_kind TEXT NOT NULL,
                active_scan_id INTEGER NOT NULL,
                PRIMARY KEY (source_path, target_package, reference_kind)
            );

            CREATE INDEX IF NOT EXISTS idx_source_dependency_edges_source_path
                ON source_dependency_edges(source_path);
            CREATE INDEX IF NOT EXISTS idx_source_dependency_edges_target_package
                ON source_dependency_edges(target_package);

            CREATE TABLE IF NOT EXISTS file_facts (
                path TEXT PRIMARY KEY NOT NULL,
                relative_path TEXT,
                active_scan_id INTEGER NOT NULL,
                byte_size INTEGER,
                mtime_ms INTEGER,
                extension TEXT,
                content_kind TEXT NOT NULL,
                line_count INTEGER,
                is_generated INTEGER NOT NULL,
                is_vendor INTEGER NOT NULL,
                parser_status TEXT NOT NULL,
                parser_recognition_attempts INTEGER NOT NULL,
                language_id TEXT,
                symbol_count INTEGER NOT NULL DEFAULT 0,
                function_count INTEGER NOT NULL DEFAULT 0,
                method_count INTEGER NOT NULL DEFAULT 0,
                type_count INTEGER NOT NULL DEFAULT 0,
                import_count INTEGER NOT NULL DEFAULT 0,
                complexity_pressure INTEGER,
                max_function_complexity_pressure INTEGER,
                diagnostics TEXT NOT NULL,
                commits_per_file INTEGER NOT NULL DEFAULT 0,
                total_added_lines INTEGER NOT NULL DEFAULT 0,
                total_deleted_lines INTEGER NOT NULL DEFAULT 0,
                total_churn_lines INTEGER NOT NULL DEFAULT 0,
                recent_added_lines INTEGER NOT NULL DEFAULT 0,
                recent_deleted_lines INTEGER NOT NULL DEFAULT 0,
                recent_churn_lines INTEGER NOT NULL DEFAULT 0,
                author_count INTEGER NOT NULL DEFAULT 0,
                first_touch_timestamp INTEGER,
                last_touch_timestamp INTEGER,
                file_age_days INTEGER,
                owner_count INTEGER,
                dominant_owner TEXT,
                dominant_owner_share REAL,
                co_changed_file_count INTEGER NOT NULL DEFAULT 0,
                source_coupling_pressure_in INTEGER,
                source_coupling_pressure_out INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_file_facts_relative_path
                ON file_facts(relative_path);
            CREATE INDEX IF NOT EXISTS idx_file_facts_generated_vendor
                ON file_facts(is_generated, is_vendor);
            CREATE INDEX IF NOT EXISTS idx_file_facts_recent_churn
                ON file_facts(recent_churn_lines);
            CREATE INDEX IF NOT EXISTS idx_file_facts_cochanged
                ON file_facts(co_changed_file_count);

            CREATE TABLE IF NOT EXISTS file_risk_scores (
                relative_path TEXT NOT NULL,
                path TEXT NOT NULL,
                active_scan_id INTEGER NOT NULL,
                formula_id TEXT NOT NULL,
                rank INTEGER NOT NULL,
                score REAL NOT NULL,
                risk_10 REAL NOT NULL,
                risk_band TEXT NOT NULL,
                is_generated INTEGER NOT NULL,
                is_vendor INTEGER NOT NULL,
                PRIMARY KEY (relative_path, formula_id)
            );

            CREATE INDEX IF NOT EXISTS idx_file_risk_scores_rank
                ON file_risk_scores(formula_id, rank);
            CREATE INDEX IF NOT EXISTS idx_file_risk_scores_score
                ON file_risk_scores(formula_id, score);
            CREATE INDEX IF NOT EXISTS idx_file_risk_scores_generated_vendor
                ON file_risk_scores(is_generated, is_vendor);
            CREATE INDEX IF NOT EXISTS idx_file_risk_scores_relative_path
                ON file_risk_scores(relative_path);

            CREATE TABLE IF NOT EXISTS file_risk_terms (
                relative_path TEXT NOT NULL,
                formula_id TEXT NOT NULL,
                term_name TEXT NOT NULL,
                raw_value REAL,
                normalized_value REAL,
                weight REAL NOT NULL,
                contribution REAL NOT NULL,
                PRIMARY KEY (relative_path, formula_id, term_name)
            );

            CREATE INDEX IF NOT EXISTS idx_file_risk_terms_relative_path
                ON file_risk_terms(relative_path);

            CREATE TABLE IF NOT EXISTS file_risk_limitations (
                relative_path TEXT NOT NULL,
                formula_id TEXT NOT NULL,
                limitation_index INTEGER NOT NULL,
                code TEXT NOT NULL,
                message TEXT NOT NULL,
                PRIMARY KEY (relative_path, formula_id, limitation_index)
            );

            CREATE INDEX IF NOT EXISTS idx_file_risk_limitations_relative_path
                ON file_risk_limitations(relative_path);

            CREATE TABLE IF NOT EXISTS file_risk_facts (
                relative_path TEXT NOT NULL,
                formula_id TEXT NOT NULL,
                fact_index INTEGER NOT NULL,
                fact_kind TEXT NOT NULL,
                message TEXT NOT NULL,
                PRIMARY KEY (relative_path, formula_id, fact_index)
            );

            CREATE INDEX IF NOT EXISTS idx_file_risk_facts_relative_path
                ON file_risk_facts(relative_path);

            CREATE TABLE IF NOT EXISTS project_risk_summary (
                formula_id TEXT PRIMARY KEY NOT NULL,
                active_scan_id INTEGER NOT NULL,
                score REAL NOT NULL,
                risk_10 REAL NOT NULL,
                risk_band TEXT NOT NULL,
                confidence TEXT NOT NULL,
                active_file_count INTEGER NOT NULL,
                active_go_file_count INTEGER NOT NULL,
                scored_file_count INTEGER NOT NULL,
                scoring_coverage REAL NOT NULL,
                go_score_coverage REAL,
                max_file_score REAL NOT NULL,
                top_10_mean_score REAL NOT NULL,
                high_risk_file_count INTEGER NOT NULL,
                medium_risk_file_count INTEGER NOT NULL,
                dominant_dimension TEXT,
                dominant_dimension_pressure REAL NOT NULL,
                git_index_status TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_project_risk_summary_score
                ON project_risk_summary(score);
            CREATE INDEX IF NOT EXISTS idx_project_risk_summary_band
                ON project_risk_summary(risk_band);

            CREATE TABLE IF NOT EXISTS project_risk_terms (
                formula_id TEXT NOT NULL,
                term_name TEXT NOT NULL,
                raw_value REAL NOT NULL,
                normalized_value REAL NOT NULL,
                weight REAL NOT NULL,
                contribution REAL NOT NULL,
                PRIMARY KEY (formula_id, term_name)
            );

            CREATE TABLE IF NOT EXISTS project_risk_facts (
                formula_id TEXT NOT NULL,
                fact_index INTEGER NOT NULL,
                fact_kind TEXT NOT NULL,
                message TEXT NOT NULL,
                PRIMARY KEY (formula_id, fact_index)
            );

            CREATE TABLE IF NOT EXISTS project_risk_limitations (
                formula_id TEXT NOT NULL,
                limitation_index INTEGER NOT NULL,
                code TEXT NOT NULL,
                message TEXT NOT NULL,
                PRIMARY KEY (formula_id, limitation_index)
            );
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    add_column_if_missing(connection, "file_analysis", "relative_path", "TEXT")?;
    add_column_if_missing(connection, "file_analysis", "mtime_ms", "INTEGER")?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "active_scan_id",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "is_active",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(connection, "file_analysis", "language_id", "TEXT")?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "symbol_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "function_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "method_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "type_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "import_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "complexity_pressure",
        "INTEGER",
    )?;
    add_column_if_missing(
        connection,
        "file_analysis",
        "max_function_complexity_pressure",
        "INTEGER",
    )?;
    add_column_if_missing(connection, "file_facts", "language_id", "TEXT")?;
    add_column_if_missing(
        connection,
        "file_facts",
        "symbol_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "function_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "method_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "type_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "import_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(connection, "file_facts", "complexity_pressure", "INTEGER")?;
    add_column_if_missing(
        connection,
        "file_facts",
        "max_function_complexity_pressure",
        "INTEGER",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "source_coupling_pressure_in",
        "INTEGER",
    )?;
    add_column_if_missing(
        connection,
        "file_facts",
        "source_coupling_pressure_out",
        "INTEGER",
    )?;
    add_column_if_missing(
        connection,
        "git_chunks",
        "cochange_pairs",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "git_chunks",
        "broad_commits_skipped_for_cochange",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        connection,
        "git_chunks",
        "max_touched_files",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(connection, "git_chunks", "broadest_commit", "TEXT")?;
    Ok(())
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StoreReducerError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(StoreReducerError::WriteDatabase)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(StoreReducerError::WriteDatabase)?;
    for existing in columns {
        if existing
            .map_err(StoreReducerError::WriteDatabase)?
            .eq_ignore_ascii_case(column)
        {
            return Ok(());
        }
    }

    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn clear_git_tables(transaction: &rusqlite::Transaction<'_>) -> Result<(), StoreReducerError> {
    transaction
        .execute_batch(
            "
            DELETE FROM git_chunks;
            DELETE FROM git_repository_summary;
            DELETE FROM git_file_metrics;
            DELETE FROM git_file_owners;
            DELETE FROM git_cochanges;
            DELETE FROM git_file_cochange_counts;
            DELETE FROM git_file_author_accumulators;
            DELETE FROM git_repository_authors;
            DELETE FROM stage_metadata WHERE key LIKE 'git_%';
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)
}

#[derive(Debug)]
struct FileIdentity {
    byte_size: u64,
    mtime_ms: Option<i64>,
}

fn file_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = fs::metadata(path).ok()?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64);
    Some(FileIdentity {
        byte_size: metadata.len(),
        mtime_ms,
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.strip_prefix(&canonical_root)
        .unwrap_or(&path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn package_path(relative_path: &str) -> String {
    Path::new(relative_path)
        .parent()
        .map(|parent| {
            parent
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|package| !package.is_empty())
        .unwrap_or_else(|| ".".to_owned())
}

fn normalize_package_path(value: &str) -> String {
    let normalized = value.trim().trim_matches('/').replace('\\', "/");
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}

fn flush_batch(
    root: &Path,
    connection: &mut Connection,
    batch: &mut StoreBatch,
    stats: &mut StoreReducerStats,
    event_sender: &mpsc::Sender<PipelineEvent>,
    started: Instant,
    active_scan_id: i64,
) -> Result<(), StoreReducerError> {
    if batch.is_empty() {
        return Ok(());
    }

    let progress_records = batch.progress_record_count();
    let planned_file_fact_rows = batch.planned_file_records;
    let planned_records = batch.planned_records
        + planned_file_fact_rows.saturating_mul(2)
        + batch.dynamic_planned_record_count();
    let file_rows = batch.file_results.len() as u64;
    let git_chunk_rows = batch.git_chunk_summaries.len() as u64;
    let git_file_metric_rows = batch.git_file_metrics.len() as u64;
    let git_file_author_rows = batch.git_file_authors.len() as u64;
    let git_cochange_rows = batch.git_cochanges.len() as u64;
    let git_repository_delta_rows = batch.git_repository_deltas.len() as u64;
    let metadata_rows = batch.metadata.len() as u64;
    let reused_file_rows = batch.reused_files.len() as u64;
    stats.planned_records += planned_records;
    stats.planned_file_fact_rows += planned_file_fact_rows;
    if planned_records > 0 {
        let _ = event_sender.send(PipelineEvent::StoreRecordsPlanned {
            total_records: stats.planned_records,
        });
    }
    let transaction = connection
        .transaction()
        .map_err(StoreReducerError::WriteDatabase)?;

    if batch.clear_git_data {
        clear_git_tables(&transaction)?;
    }

    {
        let mut file_statement = transaction
            .prepare(
                "
                INSERT OR REPLACE INTO file_analysis (
                    path,
                    relative_path,
                    byte_size,
                    mtime_ms,
                    active_scan_id,
                    is_active,
                    extension,
                    content_kind,
                    line_count,
                    is_generated,
                    is_vendor,
                    parser_status,
                    parser_recognition_attempts,
                    language_id,
                    symbol_count,
                    function_count,
                    method_count,
                    type_count,
                    import_count,
                    complexity_pressure,
                    max_function_complexity_pressure,
                    diagnostics
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for result in &batch.file_results {
            let metadata = file_identity(&result.path);
            let result_relative_path = relative_path(root, &result.path);
            file_statement
                .execute(params![
                    result.path.to_string_lossy(),
                    result_relative_path.as_str(),
                    metadata
                        .as_ref()
                        .map(|identity| identity.byte_size as i64)
                        .or_else(|| result.byte_size.map(|value| value as i64)),
                    metadata.as_ref().and_then(|identity| identity.mtime_ms),
                    active_scan_id,
                    1_i64,
                    result.extension.as_deref(),
                    content_kind_name(result.content_kind),
                    result.line_count.map(|value| value as i64),
                    bool_to_i64(result.is_generated),
                    bool_to_i64(result.is_vendor),
                    parser_status_name(result.parser_status),
                    result.parser_recognition_attempts as i64,
                    result.language_id.as_deref(),
                    result.symbol_count as i64,
                    result.function_count as i64,
                    result.method_count as i64,
                    result.type_count as i64,
                    result.import_count as i64,
                    result.complexity_pressure.map(|value| value as i64),
                    result
                        .max_function_complexity_pressure
                        .map(|value| value as i64),
                    diagnostics_json(&result.diagnostics),
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    write_source_dependency_references(&transaction, root, &batch.file_results, active_scan_id)?;

    {
        let mut reused_statement = transaction
            .prepare(
                "
                UPDATE file_analysis
                SET active_scan_id = ?2, is_active = 1
                WHERE path = ?1
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        for path in &batch.reused_files {
            let reused_relative_path = relative_path(root, path);
            reused_statement
                .execute(params![path.to_string_lossy(), active_scan_id])
                .map_err(StoreReducerError::WriteDatabase)?;
            transaction
                .execute(
                    "
                    UPDATE source_dependency_references
                    SET active_scan_id = ?2, is_active = 1
                    WHERE source_path = ?1
                    ",
                    params![reused_relative_path, active_scan_id],
                )
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    {
        let mut git_statement = transaction
            .prepare(
                "
                INSERT OR REPLACE INTO git_chunks (
                    chunk_index,
                    commits_processed,
                    file_changes,
                    cochange_pairs,
                    broad_commits_skipped_for_cochange,
                    max_touched_files,
                    broadest_commit
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for result in &batch.git_chunk_summaries {
            git_statement
                .execute(params![
                    result.chunk_index as i64,
                    result.commits_processed as i64,
                    result.file_changes as i64,
                    result.cochange_pairs as i64,
                    result.broad_commits_skipped_for_cochange as i64,
                    result.max_touched_files as i64,
                    result.broadest_commit.as_deref(),
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    write_git_repository_summary(&transaction, batch.git_repository_summary.as_ref())?;
    write_git_file_metrics(&transaction, &batch.git_file_metrics)?;
    write_git_file_authors(&transaction, &batch.git_file_authors)?;
    write_git_cochanges(&transaction, &batch.git_cochanges)?;
    write_git_repository_deltas(&transaction, &batch.git_repository_deltas)?;

    {
        let mut metadata_statement = transaction
            .prepare(
                "
                INSERT OR REPLACE INTO stage_metadata (
                    key,
                    value
                ) VALUES (?1, ?2)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for (key, value) in &batch.metadata {
            metadata_statement
                .execute(params![key, value])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    {
        let mut scan_state_statement = transaction
            .prepare(
                "
                INSERT OR REPLACE INTO scan_state (
                    key,
                    value
                ) VALUES (?1, ?2)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for (key, value) in &batch.scan_state {
            scan_state_statement
                .execute(params![key, value])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    if batch.mark_unseen_files_inactive {
        transaction
            .execute(
                "
                UPDATE file_analysis
                SET is_active = 0
                WHERE active_scan_id <> ?1
                ",
                params![active_scan_id],
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        transaction
            .execute(
                "
                UPDATE source_dependency_references
                SET is_active = 0
                WHERE active_scan_id <> ?1
                ",
                params![active_scan_id],
            )
            .map_err(StoreReducerError::WriteDatabase)?;
    }

    transaction
        .commit()
        .map_err(StoreReducerError::WriteDatabase)?;
    stats.stored_records += progress_records;
    stats.file_rows += file_rows;
    stats.git_chunk_rows += git_chunk_rows;
    stats.git_file_metric_rows += git_file_metric_rows;
    stats.git_file_owner_rows += git_file_author_rows;
    stats.git_cochange_rows += git_cochange_rows;
    stats.git_repository_delta_rows += git_repository_delta_rows;
    stats.metadata_rows += metadata_rows;
    stats.file_rows += reused_file_rows;

    if batch.plan_finalization_records {
        plan_finalization_records(connection, stats, event_sender)?;
    }

    *batch = StoreBatch::default();

    if progress_records > 0 {
        let _ = event_sender.send(PipelineEvent::StoreBatchFlushed {
            stored_records: stats.stored_records,
            elapsed: started.elapsed(),
        });
    }

    Ok(())
}

fn write_source_dependency_references(
    transaction: &rusqlite::Transaction<'_>,
    root: &Path,
    file_results: &[FileAnalysisResult],
    active_scan_id: i64,
) -> Result<(), StoreReducerError> {
    let mut delete_statement = transaction
        .prepare(
            "
            DELETE FROM source_dependency_references
            WHERE source_path = ?1
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut insert_statement = transaction
        .prepare(
            "
            INSERT INTO source_dependency_references (
                source_path,
                source_package,
                language_id,
                reference_index,
                reference_kind,
                raw_target,
                resolved_package,
                is_resolved,
                active_scan_id,
                is_active
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, ?7, 1)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for result in file_results {
        let source_path = relative_path(root, &result.path);
        delete_statement
            .execute(params![source_path])
            .map_err(StoreReducerError::WriteDatabase)?;

        let Some(parser_output) = result.parser_output.as_ref() else {
            continue;
        };
        let source_package = package_path(&source_path);
        for (index, reference) in parser_output.references.iter().enumerate() {
            insert_statement
                .execute(params![
                    source_path,
                    source_package,
                    parser_output.language_id.as_str(),
                    index as i64,
                    reference.kind.as_str(),
                    reference.target.as_str(),
                    active_scan_id,
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    Ok(())
}

fn write_git_repository_summary(
    transaction: &rusqlite::Transaction<'_>,
    summary: Option<&GitRepositorySummaryInput>,
) -> Result<(), StoreReducerError> {
    if let Some(summary) = summary {
        transaction
            .execute(
                "
                INSERT INTO git_repository_summary (
                    id,
                    head_commit,
                    head_timestamp,
                    total_commits,
                    is_shallow,
                    is_skipped,
                    skip_reason
                ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(id) DO UPDATE SET
                    head_commit = excluded.head_commit,
                    head_timestamp = excluded.head_timestamp,
                    total_commits = excluded.total_commits,
                    is_shallow = excluded.is_shallow,
                    is_skipped = excluded.is_skipped,
                    skip_reason = excluded.skip_reason
                ",
                params![
                    summary.head_commit.as_deref(),
                    summary.head_timestamp,
                    summary.total_commits as i64,
                    bool_to_i64(summary.is_shallow),
                    bool_to_i64(summary.is_skipped),
                    summary.skip_reason.as_deref(),
                ],
            )
            .map_err(StoreReducerError::WriteDatabase)?;
    }

    Ok(())
}

fn write_git_file_metrics(
    transaction: &rusqlite::Transaction<'_>,
    metrics: &[GitFileMetricDelta],
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO git_file_metrics (
                path,
                commits_per_file,
                total_added_lines,
                total_deleted_lines,
                total_churn_lines,
                recent_added_lines,
                recent_deleted_lines,
                recent_churn_lines,
                first_touch_timestamp,
                last_touch_timestamp
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(path) DO UPDATE SET
                commits_per_file = git_file_metrics.commits_per_file + excluded.commits_per_file,
                total_added_lines = git_file_metrics.total_added_lines + excluded.total_added_lines,
                total_deleted_lines = git_file_metrics.total_deleted_lines + excluded.total_deleted_lines,
                total_churn_lines = git_file_metrics.total_churn_lines + excluded.total_churn_lines,
                recent_added_lines = git_file_metrics.recent_added_lines + excluded.recent_added_lines,
                recent_deleted_lines = git_file_metrics.recent_deleted_lines + excluded.recent_deleted_lines,
                recent_churn_lines = git_file_metrics.recent_churn_lines + excluded.recent_churn_lines,
                first_touch_timestamp = min(git_file_metrics.first_touch_timestamp, excluded.first_touch_timestamp),
                last_touch_timestamp = max(git_file_metrics.last_touch_timestamp, excluded.last_touch_timestamp)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for metric in metrics {
        insert_git_file_metric(&mut statement, metric)?;
    }

    Ok(())
}

fn insert_git_file_metric(
    statement: &mut rusqlite::Statement<'_>,
    metric: &GitFileMetricDelta,
) -> Result<(), StoreReducerError> {
    statement
        .execute(params![
            metric.path,
            metric.commits as i64,
            metric.total_added_lines as i64,
            metric.total_deleted_lines as i64,
            (metric.total_added_lines + metric.total_deleted_lines) as i64,
            metric.recent_added_lines as i64,
            metric.recent_deleted_lines as i64,
            (metric.recent_added_lines + metric.recent_deleted_lines) as i64,
            metric.first_touch_timestamp,
            metric.last_touch_timestamp,
        ])
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn write_git_file_authors(
    transaction: &rusqlite::Transaction<'_>,
    authors: &[GitFileAuthorDelta],
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO git_file_author_accumulators (
                path,
                author,
                touch_count,
                meaningful_commit_count,
                effective_changed_lines,
                ownership_line_recency_score
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(path, author) DO UPDATE SET
                touch_count = git_file_author_accumulators.touch_count + excluded.touch_count,
                meaningful_commit_count = git_file_author_accumulators.meaningful_commit_count + excluded.meaningful_commit_count,
                effective_changed_lines = git_file_author_accumulators.effective_changed_lines + excluded.effective_changed_lines,
                ownership_line_recency_score = git_file_author_accumulators.ownership_line_recency_score + excluded.ownership_line_recency_score
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for author in authors {
        insert_git_file_author(&mut statement, author)?;
    }

    Ok(())
}

fn insert_git_file_author(
    statement: &mut rusqlite::Statement<'_>,
    author: &GitFileAuthorDelta,
) -> Result<(), StoreReducerError> {
    statement
        .execute(params![
            author.path,
            author.author,
            author.touch_count as i64,
            author.meaningful_commit_count as i64,
            author.effective_changed_lines as i64,
            author.ownership_line_recency_score,
        ])
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn write_git_cochanges(
    transaction: &rusqlite::Transaction<'_>,
    cochanges: &[GitCochangeDelta],
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO git_cochanges (
                left_path,
                right_path,
                co_change_count
            ) VALUES (?1, ?2, ?3)
            ON CONFLICT(left_path, right_path) DO UPDATE SET
                co_change_count = git_cochanges.co_change_count + excluded.co_change_count
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for cochange in cochanges {
        insert_git_cochange(&mut statement, cochange)?;
    }

    Ok(())
}

fn insert_git_cochange(
    statement: &mut rusqlite::Statement<'_>,
    cochange: &GitCochangeDelta,
) -> Result<(), StoreReducerError> {
    statement
        .execute(params![
            cochange.left_path,
            cochange.right_path,
            cochange.count as i64,
        ])
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn write_git_repository_deltas(
    transaction: &rusqlite::Transaction<'_>,
    deltas: &[GitRepositoryDelta],
) -> Result<(), StoreReducerError> {
    let mut author_statement = transaction
        .prepare("INSERT OR IGNORE INTO git_repository_authors (author) VALUES (?1)")
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut summary_statement = transaction
        .prepare(
            "
            INSERT INTO git_repository_summary (
                id,
                first_commit_timestamp,
                last_commit_timestamp
            ) VALUES (1, ?1, ?2)
            ON CONFLICT(id) DO UPDATE SET
                first_commit_timestamp = CASE
                    WHEN git_repository_summary.first_commit_timestamp IS NULL THEN excluded.first_commit_timestamp
                    WHEN excluded.first_commit_timestamp IS NULL THEN git_repository_summary.first_commit_timestamp
                    ELSE min(git_repository_summary.first_commit_timestamp, excluded.first_commit_timestamp)
                END,
                last_commit_timestamp = CASE
                    WHEN git_repository_summary.last_commit_timestamp IS NULL THEN excluded.last_commit_timestamp
                    WHEN excluded.last_commit_timestamp IS NULL THEN git_repository_summary.last_commit_timestamp
                    ELSE max(git_repository_summary.last_commit_timestamp, excluded.last_commit_timestamp)
                END
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for delta in deltas {
        for author in &delta.authors {
            author_statement
                .execute(params![author])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
        if delta.first_commit_timestamp.is_none() && delta.last_commit_timestamp.is_none() {
            continue;
        }
        summary_statement
            .execute(params![
                delta.first_commit_timestamp,
                delta.last_commit_timestamp,
            ])
            .map_err(StoreReducerError::WriteDatabase)?;
    }

    Ok(())
}

fn finalize_git_tables(
    root: &Path,
    connection: &mut Connection,
    stats: &mut StoreReducerStats,
    event_sender: &mpsc::Sender<PipelineEvent>,
    started: Instant,
) -> Result<(), StoreReducerError> {
    let finalization_plan = finalization_record_plan(connection, stats)?;
    let git_final_records = finalization_plan.git_final_records;
    let materialized_file_records = finalization_plan.materialized_file_records;
    let unplanned_materialized_file_records = finalization_plan.unplanned_materialized_file_records;
    let final_records = git_final_records + materialized_file_records;
    let final_planned_records = git_final_records
        .saturating_sub(stats.planned_finalization_records)
        + unplanned_materialized_file_records;
    if final_records == 0 {
        return Ok(());
    }

    stats.planned_records += final_planned_records;
    stats.planned_finalization_records +=
        git_final_records.saturating_sub(stats.planned_finalization_records);
    if final_planned_records > 0 {
        let _ = event_sender.send(PipelineEvent::StoreRecordsPlanned {
            total_records: stats.planned_records,
        });
    }

    let transaction = connection
        .transaction()
        .map_err(StoreReducerError::WriteDatabase)?;
    finalize_git_repository_summary(&transaction)?;
    finalize_git_cochange_counts(&transaction)?;
    finalize_git_file_metrics(&transaction)?;
    finalize_git_file_owners(&transaction)?;
    finalize_git_broad_commit_metadata(&transaction)?;
    finalize_source_dependencies(root, &transaction)?;
    materialize_file_facts(&transaction)?;
    materialize_file_risk_scores(&transaction)?;
    materialize_project_risk_summary(&transaction)?;
    transaction
        .commit()
        .map_err(StoreReducerError::WriteDatabase)?;

    stats.stored_records += final_records;
    let _ = event_sender.send(PipelineEvent::StoreBatchFlushed {
        stored_records: stats.stored_records,
        elapsed: started.elapsed(),
    });

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FinalizationRecordPlan {
    git_final_records: u64,
    materialized_file_records: u64,
    unplanned_materialized_file_records: u64,
}

fn finalization_record_plan(
    connection: &Connection,
    stats: &StoreReducerStats,
) -> Result<FinalizationRecordPlan, StoreReducerError> {
    let git_final_records = count_git_file_metric_rows(connection)?
        + count_git_owner_rows_to_write(connection)?
        + count_git_summary_rows(connection)?
        + count_git_broad_metadata_rows(connection)?;
    let materialized_file_records = count_active_file_analysis_rows(connection)?;
    let unplanned_materialized_file_records =
        materialized_file_records.saturating_sub(stats.planned_file_fact_rows);

    Ok(FinalizationRecordPlan {
        git_final_records,
        materialized_file_records,
        unplanned_materialized_file_records,
    })
}

fn plan_finalization_records(
    connection: &Connection,
    stats: &mut StoreReducerStats,
    event_sender: &mpsc::Sender<PipelineEvent>,
) -> Result<(), StoreReducerError> {
    let finalization_plan = finalization_record_plan(connection, stats)?;
    let planned_records = finalization_plan
        .git_final_records
        .saturating_sub(stats.planned_finalization_records);

    if planned_records == 0 {
        return Ok(());
    }

    stats.planned_records += planned_records;
    stats.planned_finalization_records += finalization_plan
        .git_final_records
        .saturating_sub(stats.planned_finalization_records);
    let _ = event_sender.send(PipelineEvent::StoreRecordsPlanned {
        total_records: stats.planned_records,
    });
    Ok(())
}

fn finalize_git_broad_commit_metadata(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute(
            "
            INSERT OR REPLACE INTO stage_metadata (key, value)
            SELECT
                'git_broad_commits_skipped_for_cochange',
                CAST(COALESCE(SUM(broad_commits_skipped_for_cochange), 0) AS TEXT)
            FROM git_chunks
            ",
            [],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn finalize_source_dependencies(
    root: &Path,
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    let module_prefix = read_go_module_prefix(root);
    let mut package_statement = transaction
        .prepare(
            "
            SELECT relative_path, language_id, active_scan_id
            FROM file_analysis
            WHERE is_active = 1
                AND language_id = 'go'
                AND relative_path IS NOT NULL
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let package_rows = package_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut file_packages = Vec::new();
    let mut known_packages = BTreeSet::new();
    for row in package_rows {
        let (file_path, language_id, active_scan_id) =
            row.map_err(StoreReducerError::WriteDatabase)?;
        let package_path = package_path(&file_path);
        known_packages.insert(package_path.clone());
        file_packages.push((file_path, package_path, language_id, active_scan_id));
    }

    transaction
        .execute_batch(
            "
            DELETE FROM source_file_packages;
            DELETE FROM source_dependency_edges;
            UPDATE source_dependency_references
            SET resolved_package = NULL, is_resolved = 0
            WHERE is_active = 1;
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    {
        let mut statement = transaction
            .prepare(
                "
                INSERT INTO source_file_packages (
                    file_path,
                    package_path,
                    language_id,
                    active_scan_id
                ) VALUES (?1, ?2, ?3, ?4)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        for (file_path, package_path, language_id, active_scan_id) in &file_packages {
            statement
                .execute(params![
                    file_path,
                    package_path,
                    language_id.as_deref(),
                    active_scan_id,
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    let mut reference_statement = transaction
        .prepare(
            "
            SELECT
                source_path,
                source_package,
                reference_index,
                reference_kind,
                raw_target,
                active_scan_id
            FROM source_dependency_references
            WHERE is_active = 1
                AND language_id = 'go'
                AND reference_kind = 'import'
            ORDER BY source_path, reference_index
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let references = reference_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut resolved_references = Vec::new();
    for reference in references {
        let (source_path, source_package, reference_index, reference_kind, raw_target, scan_id) =
            reference.map_err(StoreReducerError::WriteDatabase)?;
        if let Some(target_package) =
            resolve_go_import(&raw_target, module_prefix.as_deref(), &known_packages)
        {
            resolved_references.push((
                source_path,
                source_package,
                reference_index,
                reference_kind,
                target_package,
                scan_id,
            ));
        }
    }

    {
        let mut update_statement = transaction
            .prepare(
                "
                UPDATE source_dependency_references
                SET resolved_package = ?3, is_resolved = 1
                WHERE source_path = ?1 AND reference_index = ?2
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        let mut edge_statement = transaction
            .prepare(
                "
                INSERT OR IGNORE INTO source_dependency_edges (
                    source_path,
                    source_package,
                    target_package,
                    reference_kind,
                    active_scan_id
                ) VALUES (?1, ?2, ?3, ?4, ?5)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for (
            source_path,
            source_package,
            reference_index,
            reference_kind,
            target_package,
            scan_id,
        ) in resolved_references
        {
            update_statement
                .execute(params![source_path, reference_index, target_package])
                .map_err(StoreReducerError::WriteDatabase)?;
            edge_statement
                .execute(params![
                    source_path,
                    source_package,
                    target_package,
                    reference_kind,
                    scan_id,
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

    transaction
        .execute(
            "
            INSERT OR REPLACE INTO stage_metadata (key, value)
            VALUES
                ('source_dependency_edges_materialized', CAST((SELECT COUNT(*) FROM source_dependency_edges) AS TEXT)),
                ('source_dependency_resolver_version', '1')
            ",
            [],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn read_go_module_prefix(root: &Path) -> Option<String> {
    let contents = fs::read_to_string(root.join("go.mod")).ok()?;
    contents.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("module ")
            .map(str::trim)
            .filter(|module| !module.is_empty())
            .map(str::to_owned)
    })
}

fn resolve_go_import(
    raw_target: &str,
    module_prefix: Option<&str>,
    known_packages: &BTreeSet<String>,
) -> Option<String> {
    let mut candidates = Vec::new();
    let raw_target = raw_target.trim();

    if let Some(module_prefix) = module_prefix {
        if raw_target == module_prefix {
            candidates.push(".".to_owned());
        } else if let Some(stripped) = raw_target
            .strip_prefix(module_prefix)
            .and_then(|value| value.strip_prefix('/'))
        {
            candidates.push(normalize_package_path(stripped));
        }
    }

    candidates.push(normalize_package_path(raw_target));
    candidates
        .into_iter()
        .find(|candidate| known_packages.contains(candidate))
}

fn materialize_file_facts(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute_batch(
            "
            DELETE FROM file_facts;

            INSERT INTO file_facts (
                path,
                relative_path,
                active_scan_id,
                byte_size,
                mtime_ms,
                extension,
                content_kind,
                line_count,
                is_generated,
                is_vendor,
                parser_status,
                parser_recognition_attempts,
                language_id,
                symbol_count,
                function_count,
                method_count,
                type_count,
                import_count,
                complexity_pressure,
                max_function_complexity_pressure,
                diagnostics,
                commits_per_file,
                total_added_lines,
                total_deleted_lines,
                total_churn_lines,
                recent_added_lines,
                recent_deleted_lines,
                recent_churn_lines,
                author_count,
                first_touch_timestamp,
                last_touch_timestamp,
                file_age_days,
                owner_count,
                dominant_owner,
                dominant_owner_share,
                co_changed_file_count,
                source_coupling_pressure_in,
                source_coupling_pressure_out
            )
            SELECT
                file_analysis.path,
                file_analysis.relative_path,
                file_analysis.active_scan_id,
                file_analysis.byte_size,
                file_analysis.mtime_ms,
                file_analysis.extension,
                file_analysis.content_kind,
                file_analysis.line_count,
                file_analysis.is_generated,
                file_analysis.is_vendor,
                file_analysis.parser_status,
                file_analysis.parser_recognition_attempts,
                file_analysis.language_id,
                file_analysis.symbol_count,
                file_analysis.function_count,
                file_analysis.method_count,
                file_analysis.type_count,
                file_analysis.import_count,
                file_analysis.complexity_pressure,
                file_analysis.max_function_complexity_pressure,
                file_analysis.diagnostics,
                COALESCE(git_file_metrics.commits_per_file, 0),
                COALESCE(git_file_metrics.total_added_lines, 0),
                COALESCE(git_file_metrics.total_deleted_lines, 0),
                COALESCE(git_file_metrics.total_churn_lines, 0),
                COALESCE(git_file_metrics.recent_added_lines, 0),
                COALESCE(git_file_metrics.recent_deleted_lines, 0),
                COALESCE(git_file_metrics.recent_churn_lines, 0),
                COALESCE(git_file_metrics.author_count, 0),
                git_file_metrics.first_touch_timestamp,
                git_file_metrics.last_touch_timestamp,
                git_file_metrics.file_age_days,
                git_file_metrics.owner_count,
                git_file_metrics.dominant_owner,
                git_file_metrics.dominant_owner_share,
                COALESCE(git_file_metrics.co_changed_file_count, 0),
                COALESCE(source_in.source_coupling_pressure_in, 0),
                COALESCE(source_out.source_coupling_pressure_out, 0)
            FROM file_analysis
            LEFT JOIN git_file_metrics
                ON file_analysis.relative_path = git_file_metrics.path
            LEFT JOIN source_file_packages
                ON file_analysis.relative_path = source_file_packages.file_path
            LEFT JOIN (
                SELECT
                    target_package,
                    COUNT(DISTINCT source_path) AS source_coupling_pressure_in
                FROM source_dependency_edges
                GROUP BY target_package
            ) source_in
                ON source_file_packages.package_path = source_in.target_package
            LEFT JOIN (
                SELECT
                    source_path,
                    COUNT(DISTINCT target_package) AS source_coupling_pressure_out
                FROM source_dependency_edges
                GROUP BY source_path
            ) source_out
                ON file_analysis.relative_path = source_out.source_path
            WHERE file_analysis.is_active = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            VALUES
                ('file_facts_materialized', CAST((SELECT COUNT(*) FROM file_facts) AS TEXT)),
                ('file_facts_materializer_version', '1');
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)
}

#[derive(Debug)]
struct FileRiskRow {
    path: String,
    relative_path: String,
    active_scan_id: i64,
    is_generated: bool,
    is_vendor: bool,
    input: FileRiskInput,
    assessment: FileRiskAssessment,
}

fn materialize_file_risk_scores(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    let repository = repository_risk_context(transaction)?;
    let assessor = FileRiskAssessor::new();
    let mut statement = transaction
        .prepare(
            "
            SELECT
                path,
                relative_path,
                active_scan_id,
                byte_size,
                line_count,
                is_generated,
                is_vendor,
                total_churn_lines,
                recent_churn_lines,
                owner_count,
                dominant_owner_share,
                co_changed_file_count,
                file_age_days,
                source_coupling_pressure_in,
                source_coupling_pressure_out,
                complexity_pressure,
                max_function_complexity_pressure
            FROM file_facts
            WHERE language_id = 'go'
                AND relative_path IS NOT NULL
            ORDER BY relative_path
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let rows = statement
        .query_map([], |row| {
            let relative_path = row.get::<_, String>(1)?;
            let input = FileRiskInput {
                relative_path: relative_path.clone(),
                line_count: optional_i64_to_u64(row.get::<_, Option<i64>>(4)?),
                byte_size: optional_i64_to_u64(row.get::<_, Option<i64>>(3)?),
                total_churn_lines: i64_to_u64(row.get::<_, i64>(7)?),
                recent_churn_lines: i64_to_u64(row.get::<_, i64>(8)?),
                owner_count: optional_i64_to_u64(row.get::<_, Option<i64>>(9)?),
                dominant_owner_share: row.get::<_, Option<f64>>(10)?,
                co_changed_file_count: i64_to_u64(row.get::<_, i64>(11)?),
                file_age_days: optional_i64_to_u64(row.get::<_, Option<i64>>(12)?),
                source_coupling_pressure_in: optional_i64_to_u64(row.get::<_, Option<i64>>(13)?),
                source_coupling_pressure_out: optional_i64_to_u64(row.get::<_, Option<i64>>(14)?),
                complexity_pressure: optional_i64_to_u64(row.get::<_, Option<i64>>(15)?),
                max_function_complexity_pressure: optional_i64_to_u64(
                    row.get::<_, Option<i64>>(16)?,
                ),
            };
            Ok(FileRiskRow {
                path: row.get(0)?,
                relative_path,
                active_scan_id: row.get(2)?,
                is_generated: row.get::<_, i64>(5)? != 0,
                is_vendor: row.get::<_, i64>(6)? != 0,
                input,
                assessment: FileRiskAssessment {
                    formula_id: FORMULA_ID,
                    score: 0.0,
                    risk_10: 0.0,
                    risk_band: "low",
                    terms: Vec::new(),
                    limitations: Vec::new(),
                    facts: Vec::new(),
                },
            })
        })
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut risk_rows = Vec::new();
    for row in rows {
        let mut risk_row = row.map_err(StoreReducerError::WriteDatabase)?;
        risk_row.assessment = assessor.assess(&risk_row.input, &repository);
        risk_rows.push(risk_row);
    }
    risk_rows.sort_by(|left, right| {
        right
            .assessment
            .score
            .total_cmp(&left.assessment.score)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });

    transaction
        .execute_batch(
            "
            DELETE FROM file_risk_scores;
            DELETE FROM file_risk_terms;
            DELETE FROM file_risk_limitations;
            DELETE FROM file_risk_facts;
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    {
        let mut score_statement = transaction
            .prepare(
                "
                INSERT INTO file_risk_scores (
                    relative_path,
                    path,
                    active_scan_id,
                    formula_id,
                    rank,
                    score,
                    risk_10,
                    risk_band,
                    is_generated,
                    is_vendor
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        let mut term_statement = transaction
            .prepare(
                "
                INSERT INTO file_risk_terms (
                    relative_path,
                    formula_id,
                    term_name,
                    raw_value,
                    normalized_value,
                    weight,
                    contribution
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        let mut limitation_statement = transaction
            .prepare(
                "
                INSERT INTO file_risk_limitations (
                    relative_path,
                    formula_id,
                    limitation_index,
                    code,
                    message
                ) VALUES (?1, ?2, ?3, ?4, ?5)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;
        let mut fact_statement = transaction
            .prepare(
                "
                INSERT INTO file_risk_facts (
                    relative_path,
                    formula_id,
                    fact_index,
                    fact_kind,
                    message
                ) VALUES (?1, ?2, ?3, ?4, ?5)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for (index, risk_row) in risk_rows.iter().enumerate() {
            let rank = index as i64 + 1;
            score_statement
                .execute(params![
                    risk_row.relative_path,
                    risk_row.path,
                    risk_row.active_scan_id,
                    risk_row.assessment.formula_id,
                    rank,
                    risk_row.assessment.score,
                    risk_row.assessment.risk_10,
                    risk_row.assessment.risk_band,
                    bool_to_i64(risk_row.is_generated),
                    bool_to_i64(risk_row.is_vendor),
                ])
                .map_err(StoreReducerError::WriteDatabase)?;

            for term in &risk_row.assessment.terms {
                term_statement
                    .execute(params![
                        risk_row.relative_path,
                        risk_row.assessment.formula_id,
                        term.name,
                        term.raw_value,
                        term.normalized_value,
                        term.weight,
                        term.contribution,
                    ])
                    .map_err(StoreReducerError::WriteDatabase)?;
            }

            for (limitation_index, limitation) in risk_row.assessment.limitations.iter().enumerate()
            {
                limitation_statement
                    .execute(params![
                        risk_row.relative_path,
                        risk_row.assessment.formula_id,
                        limitation_index as i64,
                        limitation.code,
                        limitation.message,
                    ])
                    .map_err(StoreReducerError::WriteDatabase)?;
            }

            for (fact_index, fact) in risk_row.assessment.facts.iter().enumerate() {
                fact_statement
                    .execute(params![
                        risk_row.relative_path,
                        risk_row.assessment.formula_id,
                        fact_index as i64,
                        fact.kind,
                        fact.message,
                    ])
                    .map_err(StoreReducerError::WriteDatabase)?;
            }
        }
    }

    transaction
        .execute(
            "
            INSERT OR REPLACE INTO stage_metadata (key, value)
            VALUES
                ('file_risk_formula_id', ?1),
                ('file_risk_scores_materialized', ?2),
                ('file_risk_scorer_version', '1')
            ",
            params![FORMULA_ID, risk_rows.len().to_string()],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn materialize_project_risk_summary(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    let input = project_risk_input(transaction)?;
    let assessment = RepoRiskAssessor::new().assess(&input);

    transaction
        .execute_batch(
            "
            DELETE FROM project_risk_summary;
            DELETE FROM project_risk_terms;
            DELETE FROM project_risk_facts;
            DELETE FROM project_risk_limitations;
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    write_project_risk_summary(transaction, &assessment)?;
    write_project_risk_terms(transaction, &assessment)?;
    write_project_risk_facts(transaction, &assessment)?;
    write_project_risk_limitations(transaction, &assessment)?;

    transaction
        .execute(
            "
            INSERT OR REPLACE INTO stage_metadata (key, value)
            VALUES
                ('project_risk_formula_id', ?1),
                ('project_risk_score', ?2),
                ('project_risk_band', ?3),
                ('project_risk_confidence', ?4),
                ('project_risk_scorer_version', '1')
            ",
            params![
                assessment.formula_id,
                assessment.score.to_string(),
                assessment.risk_band,
                assessment.confidence,
            ],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn project_risk_input(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<RepoRiskInput, StoreReducerError> {
    let active_file_count = transaction
        .query_row("SELECT COUNT(*) FROM file_facts", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(i64_to_u64)
        .map_err(StoreReducerError::WriteDatabase)?;
    let active_go_file_count = transaction
        .query_row(
            "SELECT COUNT(*) FROM file_facts WHERE language_id = 'go'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(i64_to_u64)
        .map_err(StoreReducerError::WriteDatabase)?;
    let git_index_status =
        read_stage_metadata(transaction, "git_index_status")?.unwrap_or_else(|| {
            git_index_status_from_summary(transaction)
                .unwrap_or("unavailable")
                .to_owned()
        });

    let mut file_statement = transaction
        .prepare(
            "
            SELECT relative_path, score
            FROM file_risk_scores
            WHERE formula_id = ?1
            ORDER BY score DESC, relative_path ASC
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let file_rows = file_statement
        .query_map([FORMULA_ID], |row| {
            Ok(ProjectFileRiskInput {
                relative_path: row.get(0)?,
                score: row.get(1)?,
            })
        })
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut files = Vec::new();
    for row in file_rows {
        files.push(row.map_err(StoreReducerError::WriteDatabase)?);
    }

    let mut term_statement = transaction
        .prepare(
            "
            SELECT relative_path, term_name, normalized_value
            FROM file_risk_terms
            WHERE formula_id = ?1
            ORDER BY relative_path, term_name
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let term_rows = term_statement
        .query_map([FORMULA_ID], |row| {
            Ok(ProjectFileRiskTermInput {
                relative_path: row.get(0)?,
                term_name: row.get(1)?,
                normalized_value: row.get(2)?,
            })
        })
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut terms = Vec::new();
    for row in term_rows {
        terms.push(row.map_err(StoreReducerError::WriteDatabase)?);
    }

    Ok(RepoRiskInput {
        active_file_count,
        active_go_file_count,
        git_index_status,
        files,
        terms,
    })
}

fn write_project_risk_summary(
    transaction: &rusqlite::Transaction<'_>,
    assessment: &RepoRiskAssessment,
) -> Result<(), StoreReducerError> {
    let active_scan_id = transaction
        .query_row(
            "SELECT COALESCE(MAX(active_scan_id), 0) FROM file_facts",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    transaction
        .execute(
            "
            INSERT INTO project_risk_summary (
                formula_id,
                active_scan_id,
                score,
                risk_10,
                risk_band,
                confidence,
                active_file_count,
                active_go_file_count,
                scored_file_count,
                scoring_coverage,
                go_score_coverage,
                max_file_score,
                top_10_mean_score,
                high_risk_file_count,
                medium_risk_file_count,
                dominant_dimension,
                dominant_dimension_pressure,
                git_index_status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            ",
            params![
                assessment.formula_id,
                active_scan_id,
                assessment.score,
                assessment.risk_10,
                assessment.risk_band,
                assessment.confidence,
                assessment.active_file_count as i64,
                assessment.active_go_file_count as i64,
                assessment.scored_file_count as i64,
                assessment.scoring_coverage,
                assessment.go_score_coverage,
                assessment.max_file_score,
                assessment.top_10_mean_score,
                assessment.high_risk_file_count as i64,
                assessment.medium_risk_file_count as i64,
                assessment.dominant_dimension.as_deref(),
                assessment.dominant_dimension_pressure,
                assessment.git_index_status,
            ],
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn write_project_risk_terms(
    transaction: &rusqlite::Transaction<'_>,
    assessment: &RepoRiskAssessment,
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO project_risk_terms (
                formula_id,
                term_name,
                raw_value,
                normalized_value,
                weight,
                contribution
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    for term in &assessment.terms {
        statement
            .execute(params![
                assessment.formula_id,
                term.name,
                term.raw_value,
                term.normalized_value,
                term.weight,
                term.contribution,
            ])
            .map_err(StoreReducerError::WriteDatabase)?;
    }
    Ok(())
}

fn write_project_risk_facts(
    transaction: &rusqlite::Transaction<'_>,
    assessment: &RepoRiskAssessment,
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO project_risk_facts (
                formula_id,
                fact_index,
                fact_kind,
                message
            ) VALUES (?1, ?2, ?3, ?4)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    for (index, fact) in assessment.facts.iter().enumerate() {
        statement
            .execute(params![
                assessment.formula_id,
                index as i64,
                fact.kind,
                fact.message,
            ])
            .map_err(StoreReducerError::WriteDatabase)?;
    }
    Ok(())
}

fn write_project_risk_limitations(
    transaction: &rusqlite::Transaction<'_>,
    assessment: &RepoRiskAssessment,
) -> Result<(), StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            INSERT INTO project_risk_limitations (
                formula_id,
                limitation_index,
                code,
                message
            ) VALUES (?1, ?2, ?3, ?4)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    for (index, limitation) in assessment.limitations.iter().enumerate() {
        statement
            .execute(params![
                assessment.formula_id,
                index as i64,
                limitation.code,
                limitation.message,
            ])
            .map_err(StoreReducerError::WriteDatabase)?;
    }
    Ok(())
}

fn read_stage_metadata(
    transaction: &rusqlite::Transaction<'_>,
    key: &str,
) -> Result<Option<String>, StoreReducerError> {
    match transaction.query_row(
        "SELECT value FROM stage_metadata WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(StoreReducerError::WriteDatabase(source)),
    }
}

fn git_index_status_from_summary(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<&'static str, StoreReducerError> {
    let is_skipped = transaction.query_row(
        "SELECT is_skipped FROM git_repository_summary WHERE id = 1",
        [],
        |row| row.get::<_, i64>(0),
    );
    match is_skipped {
        Ok(0) => Ok("available"),
        Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok("unavailable"),
        Err(source) => Err(StoreReducerError::WriteDatabase(source)),
    }
}

fn repository_risk_context(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<RepositoryRiskContext, StoreReducerError> {
    let repository_file_count = transaction
        .query_row("SELECT COUNT(*) FROM file_facts", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(i64_to_u64)
        .map_err(StoreReducerError::WriteDatabase)?;

    let summary = transaction.query_row(
        "
        SELECT repository_age_days, repository_author_count, is_skipped
        FROM git_repository_summary
        WHERE id = 1
        ",
        [],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)? != 0,
            ))
        },
    );

    match summary {
        Ok((repository_age_days, repository_author_count, false)) => Ok(RepositoryRiskContext {
            repository_age_days: optional_i64_to_u64(repository_age_days),
            repository_author_count: Some(i64_to_u64(repository_author_count)),
            repository_file_count,
        }),
        Ok((_, _, true)) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(RepositoryRiskContext {
            repository_age_days: None,
            repository_author_count: None,
            repository_file_count,
        }),
        Err(source) => Err(StoreReducerError::WriteDatabase(source)),
    }
}

fn optional_i64_to_u64(value: Option<i64>) -> Option<u64> {
    value.map(i64_to_u64)
}

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn finalize_git_repository_summary(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute_batch(
            "
            UPDATE git_repository_summary
            SET
                repository_author_count = (SELECT COUNT(*) FROM git_repository_authors),
                repository_age_days = CASE
                    WHEN head_timestamp IS NOT NULL AND first_commit_timestamp IS NOT NULL
                    THEN max((head_timestamp - first_commit_timestamp) / 86400, 0)
                    ELSE NULL
                END
            WHERE id = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            SELECT 'git_index_status',
                CASE
                    WHEN is_skipped = 1 THEN 'unavailable'
                    ELSE 'available'
                END
            FROM git_repository_summary
            WHERE id = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            SELECT 'git_indexed_commits', CAST(total_commits AS TEXT)
            FROM git_repository_summary
            WHERE id = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            SELECT 'git_index_head', COALESCE(head_commit, '')
            FROM git_repository_summary
            WHERE id = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            SELECT 'git_index_skip_reason', COALESCE(skip_reason, '')
            FROM git_repository_summary
            WHERE id = 1;
            ",
        )
        .map(|_| ())
        .map_err(StoreReducerError::WriteDatabase)
}

fn finalize_git_file_metrics(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute_batch(
            "
            UPDATE git_file_metrics
            SET
                author_count = (
                    SELECT COUNT(*)
                    FROM git_file_author_accumulators author
                    WHERE author.path = git_file_metrics.path
                ),
                file_age_days = (
                    SELECT CASE
                        WHEN summary.head_timestamp IS NOT NULL AND git_file_metrics.first_touch_timestamp IS NOT NULL
                        THEN max((summary.head_timestamp - git_file_metrics.first_touch_timestamp) / 86400, 0)
                        ELSE NULL
                    END
                    FROM git_repository_summary summary
                    WHERE summary.id = 1
                ),
                co_changed_file_count = COALESCE((
                    SELECT COALESCE(counts.co_changed_file_count, 0)
                    FROM git_file_cochange_counts counts
                    WHERE counts.path = git_file_metrics.path
                ), 0);
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)
}

fn finalize_git_cochange_counts(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute_batch(
            "
            DELETE FROM git_file_cochange_counts;

            INSERT INTO git_file_cochange_counts (path, co_changed_file_count)
            SELECT path, COUNT(DISTINCT other_path)
            FROM (
                SELECT left_path AS path, right_path AS other_path FROM git_cochanges
                UNION ALL
                SELECT right_path AS path, left_path AS other_path FROM git_cochanges
            )
            GROUP BY path;
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)
}

fn finalize_git_file_owners(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    let owner_inputs = load_owner_inputs(transaction)?;
    transaction
        .execute("DELETE FROM git_file_owners", [])
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut owner_statement = transaction
        .prepare(
            "
            INSERT INTO git_file_owners (
                path,
                owner_rank,
                author,
                ownership_score,
                ownership_share,
                touch_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut metric_statement = transaction
        .prepare(
            "
            UPDATE git_file_metrics
            SET owner_count = ?2,
                dominant_owner = ?3,
                dominant_owner_share = ?4
            WHERE path = ?1
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;

    for (path, authors) in owner_inputs {
        let owners = compact_owners(&authors);
        for (index, owner) in owners.iter().enumerate() {
            owner_statement
                .execute(params![
                    path,
                    (index + 1) as i64,
                    owner.author,
                    owner.score,
                    owner.share,
                    owner.touch_count as i64,
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }

        let dominant = owners.first();
        metric_statement
            .execute(params![
                path,
                owners
                    .iter()
                    .filter(|owner| owner.author != "others")
                    .count() as i64,
                dominant.map(|owner| owner.author.as_str()),
                dominant.map(|owner| owner.share),
            ])
            .map_err(StoreReducerError::WriteDatabase)?;
    }

    Ok(())
}

#[derive(Debug)]
struct OwnerInput {
    author: String,
    touch_count: u64,
    meaningful_commit_count: u64,
    effective_changed_lines: u64,
    base_score: f64,
}

#[derive(Debug)]
struct CompactOwner {
    author: String,
    score: f64,
    share: f64,
    touch_count: u64,
}

fn load_owner_inputs(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<std::collections::BTreeMap<String, Vec<OwnerInput>>, StoreReducerError> {
    let mut statement = transaction
        .prepare(
            "
            SELECT path,
                   author,
                   touch_count,
                   meaningful_commit_count,
                   effective_changed_lines,
                   ownership_line_recency_score
            FROM git_file_author_accumulators
            ORDER BY path ASC, author ASC
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                OwnerInput {
                    author: row.get(1)?,
                    touch_count: row.get::<_, i64>(2)? as u64,
                    meaningful_commit_count: row.get::<_, i64>(3)? as u64,
                    effective_changed_lines: row.get::<_, i64>(4)? as u64,
                    base_score: row.get(5)?,
                },
            ))
        })
        .map_err(StoreReducerError::WriteDatabase)?;

    let mut inputs = std::collections::BTreeMap::<String, Vec<OwnerInput>>::new();
    for row in rows {
        let (path, input) = row.map_err(StoreReducerError::WriteDatabase)?;
        inputs.entry(path).or_default().push(input);
    }

    Ok(inputs)
}

fn compact_owners(inputs: &[OwnerInput]) -> Vec<CompactOwner> {
    let total_effective_lines = inputs
        .iter()
        .map(|input| input.effective_changed_lines)
        .sum::<u64>();
    let line_floor = 200_u64.min(1_u64.max(div_ceil(total_effective_lines * 5, 100)));
    let mut scored: Vec<_> = inputs
        .iter()
        .filter_map(|input| {
            let score = input.base_score * sustained_activity_weight(input.meaningful_commit_count);
            if score <= 0.0 {
                return None;
            }
            Some(CompactOwner {
                author: input.author.clone(),
                score,
                share: 0.0,
                touch_count: input.touch_count,
            })
        })
        .collect();

    scored.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.author.cmp(&right.author))
    });

    let total_score = scored.iter().map(|owner| owner.score).sum::<f64>();
    if total_score <= f64::EPSILON {
        return Vec::new();
    }

    for owner in &mut scored {
        owner.share = owner.score / total_score;
    }

    let mut retained = Vec::new();
    let mut others_score = 0.0;
    let mut others_touch_count = 0;
    for (index, owner) in scored.into_iter().enumerate() {
        let input = inputs
            .iter()
            .find(|input| input.author == owner.author)
            .expect("owner input should exist");
        let eligible = (owner.share >= 0.10 || index < 3)
            && (input.effective_changed_lines >= line_floor || input.meaningful_commit_count >= 3);
        if eligible && retained.len() < 3 {
            retained.push(owner);
        } else {
            others_score += owner.score;
            others_touch_count += owner.touch_count;
        }
    }

    if others_score > 0.0 {
        retained.push(CompactOwner {
            author: "others".to_owned(),
            score: others_score,
            share: others_score / total_score,
            touch_count: others_touch_count,
        });
    }

    retained
}

fn sustained_activity_weight(meaningful_commits: u64) -> f64 {
    match meaningful_commits {
        0 => 0.0,
        1 => 0.25,
        2 => 0.60,
        _ => 1.0,
    }
}

fn div_ceil(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        0
    } else {
        ((value - 1) / divisor) + 1
    }
}

fn count_git_file_metric_rows(connection: &Connection) -> Result<u64, StoreReducerError> {
    count_rows(connection, "git_file_metrics")
}

fn count_git_owner_rows_to_write(connection: &Connection) -> Result<u64, StoreReducerError> {
    let mut statement = connection
        .prepare("SELECT path, effective_changed_lines FROM git_file_author_accumulators")
        .map_err(StoreReducerError::WriteDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(StoreReducerError::WriteDatabase)?;
    let mut paths = std::collections::BTreeSet::new();
    for row in rows {
        let (path, effective_lines) = row.map_err(StoreReducerError::WriteDatabase)?;
        if effective_lines > 0 {
            paths.insert(path);
        }
    }
    Ok(paths.len() as u64)
}

fn count_git_summary_rows(connection: &Connection) -> Result<u64, StoreReducerError> {
    count_rows(connection, "git_repository_summary")
}

fn count_git_broad_metadata_rows(connection: &Connection) -> Result<u64, StoreReducerError> {
    count_rows(connection, "git_chunks").map(|count| u64::from(count > 0))
}

fn count_active_file_analysis_rows(connection: &Connection) -> Result<u64, StoreReducerError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM file_analysis WHERE is_active = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count as u64)
        .map_err(StoreReducerError::WriteDatabase)
}

fn count_rows(connection: &Connection, table: &str) -> Result<u64, StoreReducerError> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    connection
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count as u64)
        .map_err(StoreReducerError::WriteDatabase)
}

fn content_kind_name(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Text => "text",
        ContentKind::Binary => "binary",
        ContentKind::Unknown => "unknown",
    }
}

fn parser_status_name(status: FileParserStatus) -> &'static str {
    match status {
        FileParserStatus::Unsupported => "unsupported",
        FileParserStatus::Parsed => "parsed",
    }
}

fn diagnostics_json(diagnostics: &[FileDiagnostic]) -> String {
    let diagnostics: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| {
            json!({
                "code": diagnostic.code,
                "message": diagnostic.message,
            })
        })
        .collect();
    serde_json::to_string(&diagnostics).unwrap_or_else(|_| "[]".to_owned())
}

fn bool_to_i64(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use rusqlite::Connection;

    use super::{
        GitRepositorySummaryInput, StoreReducer, StoreReducerOptions, DEFAULT_STORE_QUEUE_CAPACITY,
    };
    use crate::languages::{ParserOutput, UniversalCodeMetricsInput, UniversalReference};
    use crate::pipeline::events::PipelineEvent;
    use crate::pipeline::file_analyzer::{ContentKind, FileAnalysisResult, FileParserStatus};
    use crate::pipeline::git_history_analyzer::{
        GitChunkSummary, GitCochangeDelta, GitFileAuthorDelta, GitFileMetricDelta,
        GitRepositoryDelta,
    };

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::current_dir()
                .expect("test should have current directory")
                .join("target")
                .join("store-reducer-fixtures")
                .join(format!("{name}-{}-{id}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("fixture root should be created");

            Self { path }
        }

        fn db_path(&self) -> PathBuf {
            self.path.join(".hotpath").join("index.sqlite")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn default_options_use_bounded_queue_capacity() {
        assert_eq!(
            StoreReducerOptions::default().queue_capacity,
            DEFAULT_STORE_QUEUE_CAPACITY
        );
    }

    #[test]
    fn creates_database_and_expected_tables() {
        let fixture = Fixture::new("schema");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");

        reducer.finish().expect("reducer should finish");

        assert!(fixture.db_path().exists());
        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(table_count(&connection, "file_analysis"), 1);
        assert_eq!(table_count(&connection, "git_chunks"), 1);
        assert_eq!(table_count(&connection, "stage_metadata"), 1);
        assert_eq!(table_count(&connection, "source_dependency_references"), 1);
        assert_eq!(table_count(&connection, "source_dependency_edges"), 1);
        assert_eq!(table_count(&connection, "source_file_packages"), 1);
        assert_eq!(table_count(&connection, "file_facts"), 1);
        assert_eq!(table_count(&connection, "file_risk_scores"), 1);
        assert_eq!(table_count(&connection, "file_risk_terms"), 1);
        assert_eq!(table_count(&connection, "file_risk_limitations"), 1);
        assert_eq!(table_count(&connection, "file_risk_facts"), 1);
        assert_eq!(table_count(&connection, "project_risk_summary"), 1);
        assert_eq!(table_count(&connection, "project_risk_terms"), 1);
        assert_eq!(table_count(&connection, "project_risk_limitations"), 1);
        assert_eq!(table_count(&connection, "project_risk_facts"), 1);
    }

    #[test]
    fn writes_file_and_git_rows_in_batches() {
        let fixture = Fixture::new("batch");
        let (event_sender, event_receiver) = mpsc::channel();
        let reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions {
                batch_size: 2,
                ..StoreReducerOptions::default()
            },
            event_sender,
        )
        .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result("a.go"))
            .expect("file result should enqueue");
        handle
            .store_git_chunk_summary(GitChunkSummary {
                chunk_index: 0,
                commits_processed: 3,
                file_changes: 7,
                cochange_pairs: 0,
                broad_commits_skipped_for_cochange: 0,
                max_touched_files: 0,
                broadest_commit: None,
            })
            .expect("git result should enqueue");

        let stats = reducer.finish().expect("reducer should finish");

        assert_eq!(stats.stored_records, 4);
        assert_eq!(stats.file_rows, 1);
        assert_eq!(stats.git_chunk_rows, 1);
        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_analysis"), 1);
        assert_eq!(row_count(&connection, "git_chunks"), 1);
        let events: Vec<_> = event_receiver.try_iter().collect();
        assert!(events.iter().any(|event| matches!(
            event,
            PipelineEvent::StoreBatchFlushed {
                stored_records: 2,
                ..
            }
        )));
    }

    #[test]
    fn finish_flushes_partial_batch() {
        let fixture = Fixture::new("finish-flush");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions {
                batch_size: 1_000,
                ..StoreReducerOptions::default()
            },
            event_sender,
        )
        .expect("reducer should start");

        reducer
            .handle()
            .store_file_analysis(file_result("a.go"))
            .expect("file result should enqueue");
        let stats = reducer.finish().expect("reducer should finish");

        assert_eq!(stats.stored_records, 2);
        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_analysis"), 1);
    }

    #[test]
    fn writes_and_finalizes_git_derived_tables() {
        let fixture = Fixture::new("git-derived");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_git_repository_summary(GitRepositorySummaryInput {
                head_commit: Some("head".to_owned()),
                head_timestamp: Some(1_000),
                total_commits: 2,
                is_shallow: false,
                is_skipped: false,
                skip_reason: None,
            })
            .expect("repository summary should enqueue");
        handle
            .store_git_chunk_summary(GitChunkSummary {
                chunk_index: 0,
                commits_processed: 2,
                file_changes: 3,
                cochange_pairs: 1,
                broad_commits_skipped_for_cochange: 1,
                max_touched_files: 2,
                broadest_commit: Some("abc".to_owned()),
            })
            .expect("git chunk should enqueue");
        handle
            .store_git_file_metrics(vec![
                GitFileMetricDelta {
                    path: "src/a.rs".to_owned(),
                    commits: 2,
                    total_added_lines: 12,
                    total_deleted_lines: 3,
                    recent_added_lines: 5,
                    recent_deleted_lines: 1,
                    first_touch_timestamp: 100,
                    last_touch_timestamp: 900,
                },
                GitFileMetricDelta {
                    path: "src/b.rs".to_owned(),
                    commits: 1,
                    total_added_lines: 2,
                    total_deleted_lines: 0,
                    recent_added_lines: 2,
                    recent_deleted_lines: 0,
                    first_touch_timestamp: 900,
                    last_touch_timestamp: 900,
                },
            ])
            .expect("git metrics should enqueue");
        handle
            .store_git_file_authors(vec![
                GitFileAuthorDelta {
                    path: "src/a.rs".to_owned(),
                    author: "Alice <alice@example.invalid>".to_owned(),
                    touch_count: 2,
                    meaningful_commit_count: 2,
                    effective_changed_lines: 15,
                    ownership_line_recency_score: 15.0,
                },
                GitFileAuthorDelta {
                    path: "src/b.rs".to_owned(),
                    author: "Bob <bob@example.invalid>".to_owned(),
                    touch_count: 1,
                    meaningful_commit_count: 1,
                    effective_changed_lines: 2,
                    ownership_line_recency_score: 2.0,
                },
            ])
            .expect("git authors should enqueue");
        handle
            .store_git_cochanges(vec![GitCochangeDelta {
                left_path: "src/a.rs".to_owned(),
                right_path: "src/b.rs".to_owned(),
                count: 1,
            }])
            .expect("cochanges should enqueue");
        handle
            .store_git_repository_delta(GitRepositoryDelta {
                authors: vec![
                    "Alice <alice@example.invalid>".to_owned(),
                    "Bob <bob@example.invalid>".to_owned(),
                ],
                first_commit_timestamp: Some(100),
                last_commit_timestamp: Some(900),
            })
            .expect("repository delta should enqueue");

        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "git_file_metrics"), 2);
        assert_eq!(row_count(&connection, "git_file_owners"), 2);
        assert_eq!(row_count(&connection, "git_cochanges"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT total_churn_lines FROM git_file_metrics WHERE path = 'src/a.rs'",
            ),
            15
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT co_changed_file_count FROM git_file_metrics WHERE path = 'src/a.rs'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT repository_author_count FROM git_repository_summary WHERE id = 1",
            ),
            2
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT CAST(value AS INTEGER) FROM stage_metadata WHERE key = 'git_broad_commits_skipped_for_cochange'",
            ),
            1
        );
    }

    #[test]
    fn materializes_file_facts_with_git_metrics_joined_by_relative_path() {
        let fixture = Fixture::new("file-facts-git");
        fs::create_dir_all(fixture.path.join("src")).expect("src directory should be created");
        fs::write(fixture.path.join("src/a.rs"), "fn main() {}\n")
            .expect("fixture file should be written");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result(fixture.path.join("src/a.rs")))
            .expect("file result should enqueue");
        handle
            .store_git_file_metrics(vec![GitFileMetricDelta {
                path: "src/a.rs".to_owned(),
                commits: 2,
                total_added_lines: 10,
                total_deleted_lines: 4,
                recent_added_lines: 3,
                recent_deleted_lines: 1,
                first_touch_timestamp: 100,
                last_touch_timestamp: 200,
            }])
            .expect("git metric should enqueue");
        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_facts"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT commits_per_file FROM file_facts WHERE relative_path = 'src/a.rs'",
            ),
            2
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT total_churn_lines FROM file_facts WHERE relative_path = 'src/a.rs'",
            ),
            14
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT CAST(value AS INTEGER) FROM stage_metadata WHERE key = 'file_facts_materialized'",
            ),
            1
        );
    }

    #[test]
    fn materializes_file_facts_without_git_as_zero_metrics() {
        let fixture = Fixture::new("file-facts-no-git");
        fs::write(fixture.path.join("a.go"), "package main\n")
            .expect("fixture file should be written");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");

        reducer
            .handle()
            .store_file_analysis(file_result(fixture.path.join("a.go")))
            .expect("file result should enqueue");
        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_facts"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT commits_per_file FROM file_facts WHERE relative_path = 'a.go'",
            ),
            0
        );
        assert_eq!(
            nullable_i64(
                &connection,
                "SELECT first_touch_timestamp FROM file_facts WHERE relative_path = 'a.go'",
            ),
            None
        );
    }

    #[test]
    fn resolves_local_go_imports_and_materializes_source_coupling_pressure() {
        let fixture = Fixture::new("source-coupling");
        fs::write(fixture.path.join("go.mod"), "module example.com/app\n")
            .expect("go.mod should be written");
        write_fixture_file(&fixture.path, "src/a.go");
        write_fixture_file(&fixture.path, "pkg/service/service.go");
        write_fixture_file(&fixture.path, "pkg/direct/direct.go");

        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("src/a.go"),
                &["example.com/app/pkg/service", "pkg/direct", "fmt"],
            ))
            .expect("source file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/service/service.go"),
                &[],
            ))
            .expect("service file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/direct/direct.go"),
                &[],
            ))
            .expect("direct file should enqueue");

        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "source_dependency_references"), 3);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE is_resolved = 1",
            ),
            2
        );
        assert_eq!(row_count(&connection, "source_dependency_edges"), 2);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_out FROM file_facts WHERE relative_path = 'src/a.go'",
            ),
            2
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_in FROM file_facts WHERE relative_path = 'pkg/service/service.go'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_in FROM file_facts WHERE relative_path = 'pkg/direct/direct.go'",
            ),
            1
        );
    }

    #[test]
    fn resolves_go_import_variants_with_deduplicated_source_coupling_pressure() {
        let fixture = Fixture::new("source-coupling-variants");
        fs::write(fixture.path.join("go.mod"), "module example.com/app\n")
            .expect("go.mod should be written");
        write_fixture_file(&fixture.path, "cmd/app/main.go");
        write_fixture_file(&fixture.path, "pkg/service/service.go");
        write_fixture_file(&fixture.path, "pkg/service/extra.go");
        write_fixture_file(&fixture.path, "internal/util/util.go");

        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("cmd/app/main.go"),
                &[
                    "example.com/app/pkg/service",
                    "example.com/app/pkg/service",
                    "internal/util",
                    "github.com/acme/external",
                ],
            ))
            .expect("source file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/service/service.go"),
                &[],
            ))
            .expect("first service file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/service/extra.go"),
                &[],
            ))
            .expect("second service file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("internal/util/util.go"),
                &[],
            ))
            .expect("util file should enqueue");

        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "source_dependency_references"), 4);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE is_resolved = 1",
            ),
            3
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE raw_target = 'github.com/acme/external' AND is_resolved = 0",
            ),
            1
        );
        assert_eq!(row_count(&connection, "source_dependency_edges"), 2);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_out FROM file_facts WHERE relative_path = 'cmd/app/main.go'",
            ),
            2
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_in FROM file_facts WHERE relative_path = 'pkg/service/service.go'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_in FROM file_facts WHERE relative_path = 'pkg/service/extra.go'",
            ),
            1
        );
    }

    #[test]
    fn resolves_package_like_go_imports_without_go_mod() {
        let fixture = Fixture::new("source-coupling-no-module");
        write_fixture_file(&fixture.path, "src/a.go");
        write_fixture_file(&fixture.path, "pkg/direct/direct.go");

        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("src/a.go"),
                &["pkg/direct", "example.com/missing/pkg/direct"],
            ))
            .expect("source file should enqueue");
        handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/direct/direct.go"),
                &[],
            ))
            .expect("direct file should enqueue");

        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "source_dependency_references"), 2);
        assert_eq!(row_count(&connection, "source_dependency_edges"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE raw_target = 'pkg/direct' AND resolved_package = 'pkg/direct'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE raw_target = 'example.com/missing/pkg/direct' AND is_resolved = 0",
            ),
            1
        );
    }

    #[test]
    fn reanalyzed_files_replace_previous_source_references() {
        let fixture = Fixture::new("source-reference-replace");
        write_fixture_file(&fixture.path, "src/a.go");
        write_fixture_file(&fixture.path, "pkg/old/old.go");
        write_fixture_file(&fixture.path, "pkg/new/new.go");

        let (first_event_sender, _first_event_receiver) = mpsc::channel();
        let first_reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions {
                active_scan_id: 1,
                ..StoreReducerOptions::default()
            },
            first_event_sender,
        )
        .expect("first reducer should start");
        first_reducer
            .handle()
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("src/a.go"),
                &["pkg/old"],
            ))
            .expect("first source should enqueue");
        first_reducer
            .handle()
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/old/old.go"),
                &[],
            ))
            .expect("old package should enqueue");
        first_reducer.finish().expect("first reducer should finish");

        let (second_event_sender, _second_event_receiver) = mpsc::channel();
        let second_reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions {
                active_scan_id: 2,
                ..StoreReducerOptions::default()
            },
            second_event_sender,
        )
        .expect("second reducer should start");
        let second_handle = second_reducer.handle();
        second_handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("src/a.go"),
                &["pkg/new"],
            ))
            .expect("second source should enqueue");
        second_handle
            .store_file_analysis(file_result_with_imports(
                fixture.path.join("pkg/new/new.go"),
                &[],
            ))
            .expect("new package should enqueue");
        second_handle
            .mark_unseen_files_inactive()
            .expect("inactive marker should enqueue");
        second_reducer
            .finish()
            .expect("second reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_references WHERE raw_target = 'pkg/old'",
            ),
            0
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM source_dependency_edges WHERE target_package = 'pkg/new'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT source_coupling_pressure_out FROM file_facts WHERE relative_path = 'src/a.go'",
            ),
            1
        );
    }

    #[test]
    fn materialized_file_facts_exclude_inactive_files() {
        let fixture = Fixture::new("file-facts-active");
        fs::write(fixture.path.join("active.go"), "package main\n")
            .expect("fixture file should be written");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions {
                active_scan_id: 7,
                ..StoreReducerOptions::default()
            },
            event_sender,
        )
        .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(file_result(fixture.path.join("active.go")))
            .expect("file result should enqueue");
        handle
            .mark_unseen_files_inactive()
            .expect("inactive marker should enqueue");
        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_facts"), 1);
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM file_facts WHERE relative_path = 'active.go'",
            ),
            1
        );
    }

    #[test]
    fn materializes_go_file_risk_scores_with_terms_facts_and_flags() {
        let fixture = Fixture::new("file-risk");
        write_fixture_file(&fixture.path, "risky.go");
        write_fixture_file(&fixture.path, "simple.go");
        write_fixture_file(&fixture.path, "README.md");

        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");
        let handle = reducer.handle();

        handle
            .store_file_analysis(go_result_with_metrics(
                fixture.path.join("risky.go"),
                1_200,
                220,
                45,
                true,
                false,
            ))
            .expect("risky go file should enqueue");
        handle
            .store_file_analysis(go_result_with_metrics(
                fixture.path.join("simple.go"),
                10,
                1,
                1,
                false,
                false,
            ))
            .expect("simple go file should enqueue");
        handle
            .store_file_analysis(file_result(fixture.path.join("README.md")))
            .expect("non-go file should enqueue");
        handle
            .store_git_repository_summary(GitRepositorySummaryInput {
                head_commit: Some("head".to_owned()),
                head_timestamp: Some(2_000),
                total_commits: 3,
                is_shallow: false,
                is_skipped: false,
                skip_reason: None,
            })
            .expect("repository summary should enqueue");
        handle
            .store_git_repository_delta(GitRepositoryDelta {
                authors: vec!["Alice <alice@example.invalid>".to_owned()],
                first_commit_timestamp: Some(1_000),
                last_commit_timestamp: Some(2_000),
            })
            .expect("repository delta should enqueue");
        handle
            .store_git_file_metrics(vec![
                GitFileMetricDelta {
                    path: "risky.go".to_owned(),
                    commits: 3,
                    total_added_lines: 2_000,
                    total_deleted_lines: 300,
                    recent_added_lines: 900,
                    recent_deleted_lines: 100,
                    first_touch_timestamp: 1_000,
                    last_touch_timestamp: 2_000,
                },
                GitFileMetricDelta {
                    path: "simple.go".to_owned(),
                    commits: 1,
                    total_added_lines: 1,
                    total_deleted_lines: 0,
                    recent_added_lines: 0,
                    recent_deleted_lines: 0,
                    first_touch_timestamp: 2_000,
                    last_touch_timestamp: 2_000,
                },
            ])
            .expect("git metrics should enqueue");
        handle
            .store_git_file_authors(vec![GitFileAuthorDelta {
                path: "risky.go".to_owned(),
                author: "Alice <alice@example.invalid>".to_owned(),
                touch_count: 3,
                meaningful_commit_count: 3,
                effective_changed_lines: 2_300,
                ownership_line_recency_score: 2_300.0,
            }])
            .expect("git authors should enqueue");

        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_risk_scores"), 2);
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT relative_path FROM file_risk_scores WHERE rank = 1",
            ),
            "risky.go"
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT is_generated FROM file_risk_scores WHERE relative_path = 'risky.go'",
            ),
            1
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM file_risk_terms WHERE relative_path = 'risky.go'",
            ),
            7
        );
        assert!(
            scalar_f64(
                &connection,
                "SELECT score FROM file_risk_scores WHERE relative_path = 'risky.go'",
            ) > scalar_f64(
                &connection,
                "SELECT score FROM file_risk_scores WHERE relative_path = 'simple.go'",
            )
        );
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT value FROM stage_metadata WHERE key = 'file_risk_formula_id'",
            ),
            "hotpath.score.go.v1"
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT CAST(value AS INTEGER) FROM stage_metadata WHERE key = 'file_risk_scores_materialized'",
            ),
            2
        );
        assert!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM file_risk_facts WHERE relative_path = 'risky.go'",
            ) >= 3
        );
        assert_eq!(row_count(&connection, "project_risk_summary"), 1);
        assert!(
            scalar_f64(
                &connection,
                "SELECT score FROM project_risk_summary WHERE formula_id = 'hotpath.project_risk.go.v1'",
            ) > 0.0
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT scored_file_count FROM project_risk_summary WHERE formula_id = 'hotpath.project_risk.go.v1'",
            ),
            2
        );
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT value FROM stage_metadata WHERE key = 'project_risk_formula_id'",
            ),
            "hotpath.project_risk.go.v1"
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM project_risk_terms WHERE formula_id = 'hotpath.project_risk.go.v1'",
            ),
            5
        );
    }

    #[test]
    fn materializes_unavailable_project_risk_without_scored_go_files() {
        let fixture = Fixture::new("project-risk-no-go");
        fs::write(fixture.path.join("README.md"), "hello\n").expect("fixture file should write");
        let (event_sender, _event_receiver) = mpsc::channel();
        let reducer =
            StoreReducer::start(&fixture.path, StoreReducerOptions::default(), event_sender)
                .expect("reducer should start");

        reducer
            .handle()
            .store_file_analysis(file_result(fixture.path.join("README.md")))
            .expect("file should enqueue");
        reducer.finish().expect("reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT risk_band FROM project_risk_summary WHERE formula_id = 'hotpath.project_risk.go.v1'",
            ),
            "unavailable"
        );
        assert_eq!(
            scalar_text(
                &connection,
                "SELECT confidence FROM project_risk_summary WHERE formula_id = 'hotpath.project_risk.go.v1'",
            ),
            "none"
        );
        assert_eq!(
            scalar_i64(
                &connection,
                "SELECT COUNT(*) FROM project_risk_limitations WHERE code = 'no_scored_files'",
            ),
            1
        );
    }

    #[test]
    fn scan_start_preserves_rows_for_incremental_reuse() {
        let fixture = Fixture::new("preserve");
        let (first_event_sender, _first_event_receiver) = mpsc::channel();
        let first_reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions::default(),
            first_event_sender,
        )
        .expect("first reducer should start");
        let first_handle = first_reducer.handle();

        first_handle
            .store_file_analysis(file_result("stale.go"))
            .expect("stale file result should enqueue");
        first_handle
            .store_git_cochanges(vec![GitCochangeDelta {
                left_path: "stale/a.go".to_owned(),
                right_path: "stale/b.go".to_owned(),
                count: 1,
            }])
            .expect("stale cochange should enqueue");
        first_reducer.finish().expect("first reducer should finish");

        let (second_event_sender, _second_event_receiver) = mpsc::channel();
        let second_reducer = StoreReducer::start(
            &fixture.path,
            StoreReducerOptions::default(),
            second_event_sender,
        )
        .expect("second reducer should start");
        second_reducer
            .finish()
            .expect("second reducer should finish");

        let connection = Connection::open(fixture.db_path()).expect("db should open");
        assert_eq!(row_count(&connection, "file_analysis"), 1);
        assert_eq!(row_count(&connection, "git_cochanges"), 1);
    }

    fn file_result(path: impl AsRef<Path>) -> FileAnalysisResult {
        FileAnalysisResult {
            path: path.as_ref().to_path_buf(),
            byte_size: Some(12),
            extension: Some("go".to_owned()),
            content_kind: ContentKind::Text,
            line_count: Some(1),
            is_generated: false,
            is_vendor: false,
            diagnostics: Vec::new(),
            parser_status: FileParserStatus::Unsupported,
            parser_output: None,
            parser_recognition_attempts: 0,
            language_id: None,
            symbol_count: 0,
            function_count: 0,
            method_count: 0,
            type_count: 0,
            import_count: 0,
            complexity_pressure: None,
            max_function_complexity_pressure: None,
        }
    }

    fn file_result_with_imports(path: impl AsRef<Path>, imports: &[&str]) -> FileAnalysisResult {
        let mut result = file_result(path);
        result.parser_status = FileParserStatus::Parsed;
        result.language_id = Some("go".to_owned());
        result.import_count = imports.len() as u64;
        result.parser_output = Some(ParserOutput {
            language_id: "go".to_owned(),
            symbols: Vec::new(),
            references: imports
                .iter()
                .map(|target| UniversalReference {
                    target: (*target).to_owned(),
                    kind: "import".to_owned(),
                })
                .collect(),
            metrics_input: UniversalCodeMetricsInput::default(),
            diagnostics: Vec::new(),
            limitations: Vec::new(),
        });
        result
    }

    fn go_result_with_metrics(
        path: impl AsRef<Path>,
        line_count: u64,
        complexity_pressure: u64,
        max_function_complexity_pressure: u64,
        is_generated: bool,
        is_vendor: bool,
    ) -> FileAnalysisResult {
        let mut result = file_result_with_imports(path, &[]);
        result.line_count = Some(line_count);
        result.byte_size = Some(line_count * 40);
        result.is_generated = is_generated;
        result.is_vendor = is_vendor;
        result.complexity_pressure = Some(complexity_pressure);
        result.max_function_complexity_pressure = Some(max_function_complexity_pressure);
        result
    }

    fn write_fixture_file(root: &Path, relative_path: &str) {
        let path = root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(path, "package fixture\n").expect("fixture file should be written");
    }

    fn table_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("table count should query")
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        connection
            .query_row(&sql, [], |row| row.get(0))
            .expect("row count should query")
    }

    fn scalar_i64(connection: &Connection, sql: &str) -> i64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }

    fn scalar_f64(connection: &Connection, sql: &str) -> f64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }

    fn scalar_text(connection: &Connection, sql: &str) -> String {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }

    fn nullable_i64(connection: &Connection, sql: &str) -> Option<i64> {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }
}
