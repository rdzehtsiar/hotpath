// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

const LOG_DIR: &str = ".hotpath/logs";
const LOG_PREFIX: &str = "hotpath-";
const LOG_SUFFIX: &str = ".jsonl";
const DEFAULT_RETAIN_LOGS: usize = 20;
const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

static LOGGER: OnceLock<Mutex<Option<OperationLogger>>> = OnceLock::new();

pub fn init(root: &Path) {
    let logger = OperationLogger::open(root, DEFAULT_MAX_BYTES, DEFAULT_RETAIN_LOGS).ok();
    let handle = LOGGER.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = handle.lock() {
        *guard = logger;
    }
}

pub fn event(event: &'static str, fields: Value) {
    let Some(handle) = LOGGER.get() else {
        return;
    };
    let Ok(mut guard) = handle.lock() else {
        return;
    };
    let Some(logger) = guard.as_mut() else {
        return;
    };
    let _ = logger.write_event(event, fields);
}

pub fn logs_dir(root: &Path) -> PathBuf {
    root.join(LOG_DIR)
}

struct OperationLogger {
    file: File,
    path: PathBuf,
    bytes_written: u64,
    max_bytes: u64,
    truncated: bool,
}

impl OperationLogger {
    fn open(root: &Path, max_bytes: u64, retain_logs: usize) -> io::Result<Self> {
        let logs_dir = logs_dir(root);
        fs::create_dir_all(&logs_dir)?;
        prune_logs(&logs_dir, retain_logs.saturating_sub(1))?;

        let path = logs_dir.join(log_file_name(now_millis(), process::id()));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        prune_logs(&logs_dir, retain_logs)?;

        Ok(Self {
            file,
            path,
            bytes_written: 0,
            max_bytes,
            truncated: false,
        })
    }

    fn write_event(&mut self, event: &'static str, fields: Value) -> io::Result<()> {
        if self.bytes_written >= self.max_bytes {
            return self.write_truncated_once();
        }

        let record = operation_record(event, fields);
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');

        if self.bytes_written + line.len() as u64 > self.max_bytes {
            self.write_truncated_once()?;
            return Ok(());
        }

        self.file.write_all(&line)?;
        self.bytes_written += line.len() as u64;

        Ok(())
    }

    fn write_truncated_once(&mut self) -> io::Result<()> {
        if self.truncated {
            return Ok(());
        }
        self.truncated = true;

        let record = operation_record(
            "log_truncated",
            json!({
                "path": self.path.display().to_string(),
                "max_bytes": self.max_bytes,
            }),
        );
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        if self.bytes_written + line.len() as u64 <= self.max_bytes {
            self.file.write_all(&line)?;
            self.bytes_written += line.len() as u64;
        }

        Ok(())
    }
}

fn operation_record(event: &'static str, fields: Value) -> Value {
    let fields = match fields {
        Value::Object(fields) => fields,
        other => {
            let mut wrapped = Map::new();
            wrapped.insert("value".to_owned(), other);
            wrapped
        }
    };

    json!({
        "schema_version": "hotpath.operation_log.v1",
        "timestamp_unix_ms": now_millis(),
        "pid": process::id(),
        "event": event,
        "fields": fields,
    })
}

fn prune_logs(logs_dir: &Path, retain_logs: usize) -> io::Result<()> {
    let mut logs = fs::read_dir(logs_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with(LOG_PREFIX) && name.ends_with(LOG_SUFFIX) {
                Some((name, entry.path()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    logs.sort_by(|left, right| left.0.cmp(&right.0));

    let remove_count = logs.len().saturating_sub(retain_logs);
    for (_, path) in logs.into_iter().take(remove_count) {
        fs::remove_file(path)?;
    }

    Ok(())
}

fn log_file_name(timestamp_unix_ms: u128, pid: u32) -> String {
    format!("{LOG_PREFIX}{timestamp_unix_ms:013}-{pid}{LOG_SUFFIX}")
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::Value;

    use super::{log_file_name, logs_dir, prune_logs, OperationLogger};

    #[test]
    fn logger_creates_jsonl_operation_log() {
        let root = temp_root("operation-log-create");
        let mut logger = OperationLogger::open(&root, 1024 * 1024, 20).expect("logger should open");

        logger
            .write_event("command_started", serde_json::json!({"command": "parse"}))
            .expect("event should write");

        let entries = fs::read_dir(logs_dir(&root))
            .expect("logs dir should exist")
            .collect::<Result<Vec<_>, _>>()
            .expect("logs should read");
        assert_eq!(entries.len(), 1);
        let contents =
            fs::read_to_string(entries[0].path()).expect("operation log should be readable");
        let value: Value = serde_json::from_str(contents.trim()).expect("line should be JSON");

        assert_eq!(value["schema_version"], "hotpath.operation_log.v1");
        assert_eq!(value["event"], "command_started");
        assert_eq!(value["fields"]["command"], "parse");
    }

    #[test]
    fn prune_logs_keeps_latest_stable_names() {
        let root = temp_root("operation-log-prune");
        let logs = logs_dir(&root);
        fs::create_dir_all(&logs).expect("logs dir should create");
        for index in 0..25 {
            fs::write(logs.join(log_file_name(index, 1)), "").expect("log should write");
        }

        prune_logs(&logs, 20).expect("prune should succeed");

        let mut names = fs::read_dir(logs)
            .expect("logs should read")
            .map(|entry| {
                entry
                    .expect("entry should read")
                    .file_name()
                    .into_string()
                    .expect("name should be UTF-8")
            })
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names.len(), 20);
        assert_eq!(names[0], log_file_name(5, 1));
        assert_eq!(names[19], log_file_name(24, 1));
    }

    #[test]
    fn logger_caps_file_size_without_panicking() {
        let root = temp_root("operation-log-cap");
        let mut logger = OperationLogger::open(&root, 256, 20).expect("logger should open");

        for _ in 0..20 {
            logger
                .write_event(
                    "large_event",
                    serde_json::json!({"message": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}),
                )
                .expect("cap should be non-fatal");
        }

        let size = fs::metadata(&logger.path)
            .expect("log metadata should read")
            .len();
        assert!(size <= 256, "log exceeded cap: {size}");
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("hotpath-{name}-{unique}"));
        fs::create_dir_all(&path).expect("temp root should create");
        path
    }
}
