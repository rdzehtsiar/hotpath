// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;

const INDEX_DB: &str = ".hotpath/index.sqlite";
pub const DEFAULT_HOTSPOT_LIMIT: usize = 5;
const FORMULA_ID: &str = "hotpath.score.go.v1";
const DRIVER_LIMIT: usize = 4;
const HOTSPOTS_JSON_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct HotspotsReport {
    pub include_tests: bool,
    pub rows: Vec<HotspotRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotspotRow {
    pub rank: u64,
    pub score: f64,
    pub relative_path: String,
    pub is_test: bool,
    pub drivers: Vec<String>,
    pub tags: Vec<String>,
    pub confidence: String,
}

#[derive(Debug, Serialize)]
struct HotspotsJsonOutput {
    schema_version: u64,
    command: &'static str,
    include_tests: bool,
    hotspots: Vec<HotspotsJsonHotspot>,
}

#[derive(Debug, Serialize)]
struct HotspotsJsonHotspot {
    rank: u64,
    score: f64,
    path: String,
    is_test: bool,
    tags: Vec<String>,
    drivers: Vec<String>,
    confidence: String,
}

#[derive(Debug, Clone, PartialEq)]
struct HotspotScoreRow {
    score: f64,
    relative_path: String,
    is_generated: bool,
    is_vendor: bool,
    is_test: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotspotTerm {
    pub name: String,
    pub normalized_value: Option<f64>,
}

#[derive(Debug)]
pub enum HotspotsError {
    NoIndex,
    IncompleteIndex,
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    QueryDatabase(rusqlite::Error),
}

impl fmt::Display for HotspotsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoIndex => write!(f, "No Hotpath index found. Run hotpath scan first."),
            Self::IncompleteIndex => {
                write!(f, "Hotpath index is incomplete. Run hotpath scan first.")
            }
            Self::OpenDatabase { path, source } => {
                write!(
                    f,
                    "failed to open Hotpath index '{}': {source}",
                    path.display()
                )
            }
            Self::QueryDatabase(source) => write!(f, "failed to read Hotpath hotspots: {source}"),
        }
    }
}

impl std::error::Error for HotspotsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. } => Some(source),
            Self::QueryDatabase(source) => Some(source),
            Self::NoIndex | Self::IncompleteIndex => None,
        }
    }
}

pub fn load_hotspots_report(
    current_dir: impl AsRef<Path>,
    include_tests: bool,
    limit: Option<usize>,
) -> Result<HotspotsReport, HotspotsError> {
    let index_root = find_index_root(current_dir.as_ref()).ok_or(HotspotsError::NoIndex)?;
    let index_path = index_root.join(INDEX_DB);
    let connection = Connection::open_with_flags(&index_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| HotspotsError::OpenDatabase {
            path: index_path,
            source,
        })?;

    load_hotspots_report_from_connection(&connection, limit, include_tests)
}

fn load_hotspots_report_from_connection(
    connection: &Connection,
    limit: Option<usize>,
    include_tests: bool,
) -> Result<HotspotsReport, HotspotsError> {
    if !index_is_complete(connection)? {
        return Err(HotspotsError::IncompleteIndex);
    }

    let confidence = load_project_confidence(connection)?.unwrap_or_else(|| "none".to_owned());
    let score_rows = load_score_rows(connection, include_tests, limit)?;
    let facts = load_facts(connection)?;
    let terms = load_terms(connection)?;

    let rows = score_rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let row_terms = terms.get(&row.relative_path).cloned().unwrap_or_default();
            HotspotRow {
                rank: index as u64 + 1,
                score: row.score,
                relative_path: row.relative_path.clone(),
                is_test: row.is_test,
                drivers: facts
                    .get(&row.relative_path)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .take(DRIVER_LIMIT)
                    .collect(),
                tags: tags_for_row(&row, &row_terms),
                confidence: confidence.clone(),
            }
        })
        .collect();

    Ok(HotspotsReport { include_tests, rows })
}

pub fn render_hotspots_table(report: &HotspotsReport, verbose: bool) -> String {
    if report.rows.is_empty() {
        return if report.include_tests {
            "No scored Go file hotspots found.".to_owned()
        } else {
            "No scored production Go file hotspots found.".to_owned()
        };
    }

    let title_base = if report.include_tests {
        "Go hotspots (production + tests)"
    } else {
        "Production Go hotspots"
    };
    let header = if verbose {
        title_base.to_owned()
    } else {
        let confidence = report
            .rows
            .first()
            .map(|r| r.confidence.as_str())
            .unwrap_or("none");
        format!("{title_base}  [git confidence: {confidence}]")
    };
    let mut lines = vec![header];
    for row in &report.rows {
        lines.push(String::new());
        if verbose {
            lines.push(format!(
                "{:>2}  {:.3}  {}  [{}]",
                row.rank, row.score, row.relative_path, row.confidence
            ));
        } else {
            lines.push(format!("{:>2}  {}", row.rank, row.relative_path));
        }
        if !row.tags.is_empty() {
            lines.push(format!("    {}", row.tags.join(" · ")));
        }
        if verbose {
            for driver in &row.drivers {
                lines.push(format!("    {driver}"));
            }
        }
    }
    lines.join("\n")
}

pub fn render_hotspots_json(report: &HotspotsReport) -> String {
    let output = HotspotsJsonOutput {
        schema_version: HOTSPOTS_JSON_SCHEMA_VERSION,
        command: "hotspots",
        include_tests: report.include_tests,
        hotspots: report
            .rows
            .iter()
            .map(|row| HotspotsJsonHotspot {
                rank: row.rank,
                score: row.score,
                path: row.relative_path.clone(),
                is_test: row.is_test,
                tags: row.tags.clone(),
                drivers: row.drivers.clone(),
                confidence: row.confidence.clone(),
            })
            .collect(),
    };
    serde_json::to_string(&output).expect("hotspots JSON serialization should not fail")
}


fn find_index_root(current_dir: &Path) -> Option<PathBuf> {
    current_dir
        .ancestors()
        .find(|candidate| candidate.join(INDEX_DB).is_file())
        .map(Path::to_path_buf)
}

fn index_is_complete(connection: &Connection) -> Result<bool, HotspotsError> {
    if !table_exists(connection, "scan_state")? {
        return Ok(false);
    }

    let result = connection.query_row(
        "SELECT value FROM scan_state WHERE key = 'last_scan_completed'",
        [],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(value) => Ok(value == "1"),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(source) => Err(HotspotsError::QueryDatabase(source)),
    }
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, HotspotsError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )
        .map_err(HotspotsError::QueryDatabase)
}

fn load_project_confidence(connection: &Connection) -> Result<Option<String>, HotspotsError> {
    let result = connection.query_row(
        "
        SELECT confidence
        FROM project_risk_summary
        ORDER BY formula_id
        LIMIT 1
        ",
        [],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(confidence) => Ok(Some(confidence)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(HotspotsError::QueryDatabase(source)),
    }
}

fn load_score_rows(
    connection: &Connection,
    include_tests: bool,
    limit: Option<usize>,
) -> Result<Vec<HotspotScoreRow>, HotspotsError> {
    let sql_limit: i64 = limit.map(|n| n as i64).unwrap_or(-1);
    let mut statement = connection
        .prepare(
            "
            SELECT
                score,
                relative_path,
                is_generated,
                is_vendor,
                is_test
            FROM file_risk_scores
            WHERE formula_id = ?1
                AND (?2 = 1 OR is_test = 0)
            ORDER BY score DESC, relative_path ASC
            LIMIT ?3
            ",
        )
        .map_err(HotspotsError::QueryDatabase)?;
    let rows = statement
        .query_map(
            params![FORMULA_ID, include_tests as i64, sql_limit],
            |row| {
                Ok(HotspotScoreRow {
                    score: row.get(0)?,
                    relative_path: row.get(1)?,
                    is_generated: row.get::<_, i64>(2)? != 0,
                    is_vendor: row.get::<_, i64>(3)? != 0,
                    is_test: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .map_err(HotspotsError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(HotspotsError::QueryDatabase)
}

fn load_facts(connection: &Connection) -> Result<BTreeMap<String, Vec<String>>, HotspotsError> {
    let mut statement = connection
        .prepare(
            "
            SELECT relative_path, message
            FROM file_risk_facts
            WHERE formula_id = ?1
            ORDER BY relative_path ASC, fact_index ASC
            ",
        )
        .map_err(HotspotsError::QueryDatabase)?;
    let rows = statement
        .query_map([FORMULA_ID], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(HotspotsError::QueryDatabase)?;

    let mut facts = BTreeMap::new();
    for row in rows {
        let (path, message) = row.map_err(HotspotsError::QueryDatabase)?;
        facts.entry(path).or_insert_with(Vec::new).push(message);
    }
    Ok(facts)
}

fn load_terms(
    connection: &Connection,
) -> Result<BTreeMap<String, Vec<HotspotTerm>>, HotspotsError> {
    let mut statement = connection
        .prepare(
            "
            SELECT relative_path, term_name, normalized_value
            FROM file_risk_terms
            WHERE formula_id = ?1
            ORDER BY relative_path ASC, term_name ASC
            ",
        )
        .map_err(HotspotsError::QueryDatabase)?;
    let rows = statement
        .query_map([FORMULA_ID], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HotspotTerm {
                    name: row.get(1)?,
                    normalized_value: row.get(2)?,
                },
            ))
        })
        .map_err(HotspotsError::QueryDatabase)?;

    let mut terms = BTreeMap::new();
    for row in rows {
        let (path, term) = row.map_err(HotspotsError::QueryDatabase)?;
        terms.entry(path).or_insert_with(Vec::new).push(term);
    }
    Ok(terms)
}

fn tags_for_row(row: &HotspotScoreRow, terms: &[HotspotTerm]) -> Vec<String> {
    let mut signals = Vec::new();
    if row.is_generated {
        signals.push(("generated", 1.0, 10));
    }
    if row.is_vendor {
        signals.push(("vendor", 1.0, 10));
    }
    if row.is_test {
        signals.push(("test file", 1.0, 20));
    }
    for term in terms {
        let value = term.normalized_value.unwrap_or_default();
        if value < 0.60 {
            continue;
        }
        match term.name.as_str() {
            "churn" => signals.push(("high churn", value, 90)),
            "recent_churn" => signals.push(("recent churn", value, 65)),
            "size" => signals.push(("large file", value, 75)),
            "ownership_risk" => signals.push(("ownership risk", value, 80)),
            "cochange_pressure" | "source_coupling_pressure" => {
                signals.push(("coupling pressure", value, 85));
            }
            "complexity_pressure" => signals.push(("complexity pressure", value, 70)),
            _ => {}
        }
    }

    signals.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| {
                right
                    .1
                    .partial_cmp(&left.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.0.cmp(right.0))
    });

    let mut tags = Vec::new();
    for (label, _, _) in signals {
        if !tags.iter().any(|existing| existing == label) {
            tags.push(label.to_owned());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{
        load_hotspots_report_from_connection, render_hotspots_json, render_hotspots_table,
        tags_for_row, HotspotRow, HotspotScoreRow, HotspotTerm, HotspotsError, HotspotsReport,
    };

    #[test]
    fn renders_hotspots_table_with_requested_columns() {
        let report = HotspotsReport {
            include_tests: false,
            rows: vec![HotspotRow {
                rank: 1,
                score: 0.875,
                relative_path: "internal/service/risky.go".to_owned(),
                is_test: false,
                drivers: vec!["High total churn".to_owned()],
                tags: vec!["high churn".to_owned(), "complexity pressure".to_owned()],
                confidence: "high".to_owned(),
            }],
        };

        let default = render_hotspots_table(&report, false);
        assert!(default.starts_with("Production Go hotspots"));
        assert!(default.contains("git confidence: high"));
        assert!(!default.contains("0.875"));
        assert!(default.contains("internal/service/risky.go"));
        assert!(!default.contains("[high]"));
        assert!(default.contains("high churn · complexity pressure"));
        assert!(!default.contains("High total churn"));

        let verbose = render_hotspots_table(&report, true);
        assert!(verbose.contains("0.875"));
        assert!(verbose.contains("[high]"));
        assert!(verbose.contains("High total churn"));
    }

    #[test]
    fn empty_report_renders_empty_state() {
        let rendered = render_hotspots_table(
            &HotspotsReport {
                include_tests: false,
                rows: Vec::new(),
            },
            false,
        );

        assert_eq!(rendered, "No scored production Go file hotspots found.");
    }

    #[test]
    fn renders_hotspots_json_with_versioned_schema() {
        let report = HotspotsReport {
            include_tests: true,
            rows: vec![HotspotRow {
                rank: 1,
                score: 0.875,
                relative_path: "internal/service/risky_test.go".to_owned(),
                is_test: true,
                drivers: vec!["High total churn".to_owned()],
                tags: vec!["high churn".to_owned(), "test file".to_owned()],
                confidence: "medium".to_owned(),
            }],
        };

        let json = render_hotspots_json(&report);
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("output should be valid JSON");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], "hotspots");
        assert_eq!(value["include_tests"], true);
        let hotspots = value["hotspots"].as_array().expect("hotspots should be array");
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0]["rank"], 1);
        assert_eq!(hotspots[0]["score"], 0.875);
        assert_eq!(hotspots[0]["path"], "internal/service/risky_test.go");
        assert_eq!(hotspots[0]["is_test"], true);
        assert_eq!(hotspots[0]["confidence"], "medium");
    }

    #[test]
    fn tags_follow_tui_signal_priority() {
        let row = HotspotScoreRow {
            score: 0.8,
            relative_path: "service_test.go".to_owned(),
            is_generated: false,
            is_vendor: false,
            is_test: true,
        };
        let tags = tags_for_row(
            &row,
            &[
                HotspotTerm {
                    name: "complexity_pressure".to_owned(),
                    normalized_value: Some(1.0),
                },
                HotspotTerm {
                    name: "churn".to_owned(),
                    normalized_value: Some(0.7),
                },
                HotspotTerm {
                    name: "size".to_owned(),
                    normalized_value: Some(0.3),
                },
            ],
        );

        assert_eq!(tags, vec!["high churn", "complexity pressure", "test file"]);
    }

    #[test]
    fn loader_requires_complete_index() {
        let connection = Connection::open_in_memory().expect("database opens");
        connection
            .execute_batch(
                "
                CREATE TABLE scan_state (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO scan_state VALUES ('last_scan_completed', '0');
                ",
            )
            .expect("schema should be created");

        let error = load_hotspots_report_from_connection(&connection, None, false)
            .expect_err("incomplete index should fail");

        assert!(matches!(error, HotspotsError::IncompleteIndex));
    }

    #[test]
    fn loader_sorts_by_score_then_path_and_limits_rows() {
        let connection = Connection::open_in_memory().expect("database opens");
        create_hotspots_schema(&connection);
        connection
            .execute_batch(
                "
                INSERT INTO scan_state VALUES ('last_scan_completed', '1');
                INSERT INTO project_risk_summary VALUES ('hotpath.project_risk.go.v1', 'bounded');
                INSERT INTO file_risk_scores VALUES
                    ('z.go', 'z.go', 1, 'hotpath.score.go.v1', 3, 0.8, 8.0, 'high', 0, 0, 0),
                    ('a.go', 'a.go', 1, 'hotpath.score.go.v1', 2, 0.8, 8.0, 'high', 0, 0, 0),
                    ('m.go', 'm.go', 1, 'hotpath.score.go.v1', 1, 0.9, 9.0, 'extreme', 0, 0, 0),
                    ('test_test.go', 'test_test.go', 1, 'hotpath.score.go.v1', 4, 1.0, 10.0, 'extreme', 0, 0, 1);
                INSERT INTO file_risk_facts VALUES
                    ('m.go', 'hotpath.score.go.v1', 0, 'summary', 'M fact'),
                    ('a.go', 'hotpath.score.go.v1', 0, 'summary', 'A fact');
                INSERT INTO file_risk_terms VALUES
                    ('m.go', 'hotpath.score.go.v1', 'churn', 1.0),
                    ('a.go', 'hotpath.score.go.v1', 'complexity_pressure', 1.0);
                ",
            )
            .expect("rows should insert");

        let report =
            load_hotspots_report_from_connection(&connection, Some(2), false).expect("hotspots should load");

        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].relative_path, "m.go");
        assert_eq!(report.rows[1].relative_path, "a.go");
        assert_eq!(report.rows[0].rank, 1);
        assert_eq!(report.rows[1].rank, 2);
        assert!(!report.rows[0].is_test);
        assert_eq!(report.rows[0].confidence, "bounded");
        assert_eq!(report.rows[0].drivers, vec!["M fact"]);
        assert_eq!(report.rows[0].tags, vec!["high churn"]);
    }

    #[test]
    fn loader_excludes_test_files_by_default() {
        let connection = Connection::open_in_memory().expect("database opens");
        create_hotspots_schema(&connection);
        connection
            .execute_batch(
                "
                INSERT INTO scan_state VALUES ('last_scan_completed', '1');
                INSERT INTO project_risk_summary VALUES ('hotpath.project_risk.go.v1', 'bounded');
                INSERT INTO file_risk_scores VALUES
                    ('prod.go', 'prod.go', 1, 'hotpath.score.go.v1', 1, 0.9, 9.0, 'extreme', 0, 0, 0),
                    ('prod_test.go', 'prod_test.go', 1, 'hotpath.score.go.v1', 2, 0.8, 8.0, 'high', 0, 0, 1);
                ",
            )
            .expect("rows should insert");

        let report = load_hotspots_report_from_connection(&connection, None, false)
            .expect("hotspots should load");

        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].relative_path, "prod.go");
        assert!(!report.rows[0].is_test);
    }

    #[test]
    fn loader_includes_test_files_when_requested() {
        let connection = Connection::open_in_memory().expect("database opens");
        create_hotspots_schema(&connection);
        connection
            .execute_batch(
                "
                INSERT INTO scan_state VALUES ('last_scan_completed', '1');
                INSERT INTO project_risk_summary VALUES ('hotpath.project_risk.go.v1', 'bounded');
                INSERT INTO file_risk_scores VALUES
                    ('prod.go', 'prod.go', 1, 'hotpath.score.go.v1', 1, 0.9, 9.0, 'extreme', 0, 0, 0),
                    ('prod_test.go', 'prod_test.go', 1, 'hotpath.score.go.v1', 2, 0.8, 8.0, 'high', 0, 0, 1);
                ",
            )
            .expect("rows should insert");

        let report = load_hotspots_report_from_connection(&connection, None, true)
            .expect("hotspots should load");

        assert!(report.include_tests);
        assert_eq!(report.rows.len(), 2);
        assert_eq!(report.rows[0].relative_path, "prod.go");
        assert!(!report.rows[0].is_test);
        assert_eq!(report.rows[1].relative_path, "prod_test.go");
        assert!(report.rows[1].is_test);
        assert!(report.rows[1].tags.contains(&"test file".to_owned()));
    }

    fn create_hotspots_schema(connection: &Connection) {
        connection
            .execute_batch(
                "
                CREATE TABLE scan_state (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                CREATE TABLE project_risk_summary (
                    formula_id TEXT PRIMARY KEY NOT NULL,
                    confidence TEXT NOT NULL
                );
                CREATE TABLE file_risk_scores (
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
                    is_test INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (relative_path, formula_id)
                );
                CREATE TABLE file_risk_facts (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    fact_index INTEGER NOT NULL,
                    fact_kind TEXT NOT NULL,
                    message TEXT NOT NULL,
                    PRIMARY KEY (relative_path, formula_id, fact_index)
                );
                CREATE TABLE file_risk_terms (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    term_name TEXT NOT NULL,
                    normalized_value REAL,
                    PRIMARY KEY (relative_path, formula_id, term_name)
                );
                ",
            )
            .expect("schema should be created");
    }
}
