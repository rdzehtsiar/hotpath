// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const INDEX_DIR: &str = ".hotpath";
const LOCK_FILE: &str = "index.lock";

#[derive(Debug)]
pub struct IndexLock {
    path: PathBuf,
}

impl IndexLock {
    pub fn acquire(root: impl AsRef<Path>, command: &str) -> Result<Self, IndexLockError> {
        let index_dir = root.as_ref().join(INDEX_DIR);
        fs::create_dir_all(&index_dir).map_err(|source| IndexLockError::CreateDirectory {
            path: index_dir.clone(),
            source,
        })?;

        let path = index_dir.join(LOCK_FILE);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(IndexLockError::AlreadyLocked { path });
            }
            Err(source) => {
                return Err(IndexLockError::CreateLock { path, source });
            }
        };

        write_lock_payload(&mut file, command).map_err(|source| IndexLockError::WriteLock {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub enum IndexLockError {
    CreateDirectory { path: PathBuf, source: io::Error },
    AlreadyLocked { path: PathBuf },
    CreateLock { path: PathBuf, source: io::Error },
    WriteLock { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for IndexLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateDirectory { path, source } => write!(
                f,
                "failed to create Hotpath index directory '{}': {source}",
                path.display()
            ),
            Self::AlreadyLocked { path } => write!(
                f,
                "another Hotpath process is using the index lock '{}'. Retry after it exits; remove .hotpath/index.lock only if no Hotpath process is running.",
                path.display()
            ),
            Self::CreateLock { path, source } => write!(
                f,
                "failed to create Hotpath index lock '{}': {source}",
                path.display()
            ),
            Self::WriteLock { path, source } => write!(
                f,
                "failed to write Hotpath index lock '{}': {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for IndexLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateDirectory { source, .. }
            | Self::CreateLock { source, .. }
            | Self::WriteLock { source, .. } => Some(source),
            Self::AlreadyLocked { .. } => None,
        }
    }
}

fn write_lock_payload(file: &mut File, command: &str) -> io::Result<()> {
    writeln!(file, "pid={}", std::process::id())?;
    writeln!(file, "command={command}")?;
    file.flush()
}
