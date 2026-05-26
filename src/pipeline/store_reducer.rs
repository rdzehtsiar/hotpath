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
use std::collections::BTreeMap;

use crate::pipeline::events::PipelineEvent;
use crate::pipeline::file_analyzer::{
    ContentKind, FileAnalysisResult, FileDiagnostic, FileParserStatus,
};
use crate::pipeline::git_history_analyzer::{
    GitChunkSummary, GitCochangeDelta, GitFileAuthorDelta, GitFileMetricDelta, GitHistoryError,
    GitHistorySink, GitRepositoryDelta,
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
        self.send(StoreMessage::FileAnalysis(result))
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
    FileAnalysis(FileAnalysisResult),
    GitChunkSummary(GitChunkSummary),
    GitFileMetrics(Vec<GitFileMetricDelta>),
    GitFileAuthors(Vec<GitFileAuthorDelta>),
    GitCochanges(Vec<GitCochangeDelta>),
    GitRepositoryDelta(GitRepositoryDelta),
    GitRepositorySummary(GitRepositorySummaryInput),
    StageMetadata { key: String, value: String },
    ScanState { key: String, value: String },
    FileReused(PathBuf),
    MarkUnseenFilesInactive,
    ClearGitData,
    Finish,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoreReducerStats {
    pub planned_records: u64,
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

pub fn load_file_reuse_index(root: impl AsRef<Path>) -> Result<FileReuseIndex, StoreReducerError> {
    let db_path = root.as_ref().join(INDEX_DIR).join(INDEX_DB);
    if !db_path.exists() {
        return Ok(FileReuseIndex::default());
    }
    let connection = open_database(&db_path)?;
    initialize_database(&connection)?;
    if read_scan_state_value(&connection, "last_scan_completed")?.is_none_or(|value| value != "1") {
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
    reused_files: Vec<PathBuf>,
    mark_unseen_files_inactive: bool,
    clear_git_data: bool,
}

impl StoreBatch {
    fn push(&mut self, message: StoreMessage) {
        match message {
            StoreMessage::FileAnalysis(result) => self.file_results.push(result),
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

    fn total_len(&self) -> usize {
        self.file_results.len()
            + self.git_chunk_summaries.len()
            + self.git_file_metrics.len()
            + self.git_file_authors.len()
            + self.git_cochanges.len()
            + self.git_repository_deltas.len()
            + self.metadata.len()
            + self.scan_state.len()
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
                finalize_git_tables(&mut connection, &mut stats, &event_sender, started)?;
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
                finalize_git_tables(&mut connection, &mut stats, &event_sender, started)?;
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
                cognitive_complexity INTEGER,
                source_coupling_in INTEGER,
                source_coupling_out INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_file_facts_relative_path
                ON file_facts(relative_path);
            CREATE INDEX IF NOT EXISTS idx_file_facts_generated_vendor
                ON file_facts(is_generated, is_vendor);
            CREATE INDEX IF NOT EXISTS idx_file_facts_recent_churn
                ON file_facts(recent_churn_lines);
            CREATE INDEX IF NOT EXISTS idx_file_facts_cochanged
                ON file_facts(co_changed_file_count);
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
    let file_rows = batch.file_results.len() as u64;
    let git_chunk_rows = batch.git_chunk_summaries.len() as u64;
    let git_file_metric_rows = batch.git_file_metrics.len() as u64;
    let git_file_author_rows = batch.git_file_authors.len() as u64;
    let git_cochange_rows = batch.git_cochanges.len() as u64;
    let git_repository_delta_rows = batch.git_repository_deltas.len() as u64;
    let metadata_rows = batch.metadata.len() as u64;
    let reused_file_rows = batch.reused_files.len() as u64;
    stats.planned_records += progress_records;
    if progress_records > 0 {
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
                    diagnostics
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                ",
            )
            .map_err(StoreReducerError::WriteDatabase)?;

        for result in &batch.file_results {
            let metadata = file_identity(&result.path);
            file_statement
                .execute(params![
                    result.path.to_string_lossy(),
                    relative_path(root, &result.path),
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
                    diagnostics_json(&result.diagnostics),
                ])
                .map_err(StoreReducerError::WriteDatabase)?;
        }
    }

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
            reused_statement
                .execute(params![path.to_string_lossy(), active_scan_id])
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
    *batch = StoreBatch::default();

    if progress_records > 0 {
        let _ = event_sender.send(PipelineEvent::StoreBatchFlushed {
            stored_records: stats.stored_records,
            elapsed: started.elapsed(),
        });
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
    connection: &mut Connection,
    stats: &mut StoreReducerStats,
    event_sender: &mpsc::Sender<PipelineEvent>,
    started: Instant,
) -> Result<(), StoreReducerError> {
    let final_records = count_git_file_metric_rows(connection)?
        + count_git_owner_rows_to_write(connection)?
        + count_git_summary_rows(connection)?
        + count_git_broad_metadata_rows(connection)?
        + count_active_file_analysis_rows(connection)?;
    if final_records == 0 {
        return Ok(());
    }

    stats.planned_records += final_records;
    let _ = event_sender.send(PipelineEvent::StoreRecordsPlanned {
        total_records: stats.planned_records,
    });

    let transaction = connection
        .transaction()
        .map_err(StoreReducerError::WriteDatabase)?;
    finalize_git_repository_summary(&transaction)?;
    finalize_git_cochange_counts(&transaction)?;
    finalize_git_file_metrics(&transaction)?;
    finalize_git_file_owners(&transaction)?;
    finalize_git_broad_commit_metadata(&transaction)?;
    materialize_file_facts(&transaction)?;
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
                cognitive_complexity,
                source_coupling_in,
                source_coupling_out
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
                NULL,
                NULL,
                NULL
            FROM file_analysis
            LEFT JOIN git_file_metrics
                ON file_analysis.relative_path = git_file_metrics.path
            WHERE file_analysis.is_active = 1;

            INSERT OR REPLACE INTO stage_metadata (key, value)
            VALUES
                ('file_facts_materialized', CAST((SELECT COUNT(*) FROM file_facts) AS TEXT)),
                ('file_facts_materializer_version', '1');
            ",
        )
        .map_err(StoreReducerError::WriteDatabase)
}

fn finalize_git_repository_summary(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StoreReducerError> {
    transaction
        .execute(
            "
            UPDATE git_repository_summary
            SET
                repository_author_count = (SELECT COUNT(*) FROM git_repository_authors),
                repository_age_days = CASE
                    WHEN head_timestamp IS NOT NULL AND first_commit_timestamp IS NOT NULL
                    THEN max((head_timestamp - first_commit_timestamp) / 86400, 0)
                    ELSE NULL
                END
            WHERE id = 1
            ",
            [],
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
        assert_eq!(table_count(&connection, "file_facts"), 1);
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
        }
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

    fn nullable_i64(connection: &Connection, sql: &str) -> Option<i64> {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar query should run")
    }
}
