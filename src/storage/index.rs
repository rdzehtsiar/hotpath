// SPDX-License-Identifier: Apache-2.0

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

const HOTPATH_DIR: &str = ".hotpath";
const INDEX_FILE: &str = "index.db";
const SCHEMA_VERSION_KEY: &str = "schema_version";

#[derive(Debug)]
pub struct IndexStore {
    _connection: Connection,
    path: PathBuf,
    schema_version: u32,
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
            _connection: connection,
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
        }
    }
}

impl StdError for IndexError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateIndexDir { source, .. } => Some(source),
            Self::OpenDatabase { source, .. }
            | Self::CorruptDatabase { source, .. }
            | Self::Migration { source, .. } => Some(source),
            Self::CorruptMetadata { .. }
            | Self::UnsafeIndexDir { .. }
            | Self::IncompatibleFutureSchema { .. } => None,
        }
    }
}

pub fn default_index_path(repo_root: impl AsRef<Path>) -> PathBuf {
    repo_root.as_ref().join(HOTPATH_DIR).join(INDEX_FILE)
}

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

    transaction
        .execute(
            "INSERT INTO hotpath_metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
            params![SCHEMA_VERSION_KEY, CURRENT_SCHEMA_VERSION.to_string()],
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
    verify_metadata_table_shape(connection, path)?;

    let user_version = read_user_version(connection, path)?;
    let metadata_version = read_metadata_schema_version(connection, path)?;

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

fn verify_metadata_table_shape(connection: &Connection, path: &Path) -> Result<(), IndexError> {
    let summary = read_metadata_table_summary(connection, path)?.ok_or_else(|| {
        IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "missing hotpath_metadata table".to_owned(),
        }
    })?;

    if summary.table_type != "table" {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!("hotpath_metadata is a {}, not a table", summary.table_type),
        });
    }

    if summary.column_count != 2 {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "hotpath_metadata has {} columns, expected 2",
                summary.column_count
            ),
        });
    }

    if summary.without_rowid {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "hotpath_metadata must use the default rowid table layout".to_owned(),
        });
    }

    if !summary.strict {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: "hotpath_metadata must be a STRICT table".to_owned(),
        });
    }

    let columns = read_metadata_columns(connection, path)?;
    let expected = [
        ("key", "TEXT", true, None, 1, 0),
        ("value", "TEXT", true, None, 0, 0),
    ];

    if columns.len() != expected.len() {
        return Err(IndexError::CorruptMetadata {
            path: path.to_path_buf(),
            message: format!(
                "hotpath_metadata has {} visible or hidden columns, expected {}",
                columns.len(),
                expected.len()
            ),
        });
    }

    for (column, expected) in columns.iter().zip(expected) {
        let (
            expected_name,
            expected_type,
            expected_not_null,
            expected_default,
            expected_primary_key_position,
            expected_hidden,
        ) = expected;

        if column.name != expected_name
            || !column.data_type.eq_ignore_ascii_case(expected_type)
            || column.not_null != expected_not_null
            || column.default_value.as_deref() != expected_default
            || column.primary_key_position != expected_primary_key_position
            || column.hidden != expected_hidden
        {
            return Err(IndexError::CorruptMetadata {
                path: path.to_path_buf(),
                message: format!(
                    "hotpath_metadata column '{}' does not match expected schema",
                    column.name
                ),
            });
        }
    }

    Ok(())
}

fn read_metadata_table_summary(
    connection: &Connection,
    path: &Path,
) -> Result<Option<MetadataTableSummary>, IndexError> {
    connection
        .query_row("PRAGMA table_list('hotpath_metadata');", [], |row| {
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

fn read_metadata_columns(
    connection: &Connection,
    path: &Path,
) -> Result<Vec<MetadataColumn>, IndexError> {
    let mut statement = connection
        .prepare("PRAGMA table_xinfo('hotpath_metadata');")
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn repeated_open_is_idempotent() {
        let fixture = Fixture::new("idempotent");

        let first = IndexStore::open(&fixture.path).expect("first open should migrate");
        assert_eq!(first.schema_version(), CURRENT_SCHEMA_VERSION);
        drop(first);

        let second = IndexStore::open(&fixture.path).expect("second open should be valid");
        assert_eq!(second.schema_version(), CURRENT_SCHEMA_VERSION);
        let connection = Connection::open(second.path()).expect("index should reopen");
        assert_eq!(
            read_user_version(&connection, second.path())
                .expect("user_version should remain readable"),
            CURRENT_SCHEMA_VERSION
        );
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
