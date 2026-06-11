// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

const INDEX_DIR: &str = ".hotpath";
const INDEX_DB: &str = "index.sqlite";
const TOP_HOTSPOT_LIMIT: u64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct ScanSummary {
    pub index_path: PathBuf,
    pub hotspots: Vec<GoHotspot>,
    pub project: Option<ProjectCoverage>,
    pub git: GitSummary,
    pub limitations: Vec<SummaryLimitation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoHotspot {
    pub rank: u64,
    pub relative_path: String,
    pub risk_10: f64,
    pub risk_band: String,
    pub fact: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectCoverage {
    pub coverage_percent: f64,
    pub scored_file_count: u64,
    pub active_go_file_count: u64,
    pub active_file_count: u64,
    pub confidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitSummary {
    pub confidence: Option<String>,
    pub mode: Option<String>,
    pub collection: Option<String>,
    pub index_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug)]
pub enum ScanSummaryError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    QueryDatabase(rusqlite::Error),
}

impl fmt::Display for ScanSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(
                    f,
                    "failed to open Hotpath summary index '{}': {source}",
                    path.display()
                )
            }
            Self::QueryDatabase(source) => {
                write!(f, "failed to read Hotpath scan summary: {source}")
            }
        }
    }
}

impl std::error::Error for ScanSummaryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. } => Some(source),
            Self::QueryDatabase(source) => Some(source),
        }
    }
}

pub fn load_scan_summary(root: impl AsRef<Path>) -> Result<ScanSummary, ScanSummaryError> {
    let index_path = index_path(root.as_ref());
    let connection =
        Connection::open(&index_path).map_err(|source| ScanSummaryError::OpenDatabase {
            path: index_path.clone(),
            source,
        })?;
    load_scan_summary_from_connection(&connection, index_path)
}

fn load_scan_summary_from_connection(
    connection: &Connection,
    index_path: PathBuf,
) -> Result<ScanSummary, ScanSummaryError> {
    let metadata = load_stage_metadata(connection)?;
    let mut limitations = load_project_limitations(connection)?;
    limitations.extend(git_limitations(&metadata));

    Ok(ScanSummary {
        index_path,
        hotspots: load_hotspots(connection)?,
        project: load_project_coverage(connection)?,
        git: GitSummary {
            confidence: metadata.get("git_confidence").cloned(),
            mode: metadata
                .get("git_scan_mode")
                .or_else(|| metadata.get("git_mode"))
                .cloned(),
            collection: metadata.get("git_collection_mode").cloned(),
            index_action: metadata.get("git_index_action").cloned(),
        },
        limitations,
    })
}

pub fn render_scan_summary(summary: &ScanSummary) -> String {
    let mut lines = vec!["summary".to_owned()];
    if summary.hotspots.is_empty() {
        lines.push("  top_go_hotspots none".to_owned());
    } else {
        lines.push("  top_go_hotspots".to_owned());
        lines.extend(summary.hotspots.iter().map(render_hotspot));
    }

    lines.push(render_project_coverage(summary.project.as_ref()));
    lines.push(render_git_summary(&summary.git));
    lines.push(render_limitations(&summary.limitations));
    lines.push(format!("  index {}", summary.index_path.display()));
    lines.join("\n")
}

fn index_path(root: &Path) -> PathBuf {
    let path = root.join(INDEX_DIR).join(INDEX_DB);
    fs::canonicalize(&path).unwrap_or(path)
}

fn load_hotspots(connection: &Connection) -> Result<Vec<GoHotspot>, ScanSummaryError> {
    let mut statement = connection
        .prepare(
            "
            SELECT
                score.relative_path,
                score.risk_10,
                score.risk_band,
                fact.message
            FROM file_risk_scores score
            LEFT JOIN file_risk_facts fact
                ON fact.relative_path = score.relative_path
                AND fact.formula_id = score.formula_id
                AND fact.fact_index = 0
            WHERE score.is_test = 0
            ORDER BY score.score DESC, score.relative_path ASC
            LIMIT ?1
            ",
        )
        .map_err(ScanSummaryError::QueryDatabase)?;
    let rows = statement
        .query_map([TOP_HOTSPOT_LIMIT], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(ScanSummaryError::QueryDatabase)?;
    rows.enumerate()
        .map(|(index, row)| {
            let (relative_path, risk_10, risk_band, fact) =
                row.map_err(ScanSummaryError::QueryDatabase)?;
            Ok(GoHotspot {
                rank: index as u64 + 1,
                relative_path,
                risk_10,
                risk_band,
                fact,
            })
        })
        .collect()
}

fn load_project_coverage(
    connection: &Connection,
) -> Result<Option<ProjectCoverage>, ScanSummaryError> {
    let result = connection.query_row(
        "
        SELECT
            scoring_coverage,
            go_score_coverage,
            scored_file_count,
            active_go_file_count,
            active_file_count,
            confidence
        FROM project_risk_summary
        ORDER BY formula_id
        LIMIT 1
        ",
        [],
        |row| {
            let scoring_coverage = row.get::<_, f64>(0)?;
            let go_score_coverage = row.get::<_, Option<f64>>(1)?;
            Ok(ProjectCoverage {
                coverage_percent: go_score_coverage
                    .unwrap_or(scoring_coverage)
                    .clamp(0.0, 1.0)
                    * 100.0,
                scored_file_count: i64_to_u64(row.get::<_, i64>(2)?),
                active_go_file_count: i64_to_u64(row.get::<_, i64>(3)?),
                active_file_count: i64_to_u64(row.get::<_, i64>(4)?),
                confidence: row.get(5)?,
            })
        },
    );
    match result {
        Ok(project) => Ok(Some(project)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(ScanSummaryError::QueryDatabase(source)),
    }
}

fn load_project_limitations(
    connection: &Connection,
) -> Result<Vec<SummaryLimitation>, ScanSummaryError> {
    let mut statement = connection
        .prepare(
            "
            SELECT code, message
            FROM project_risk_limitations
            ORDER BY formula_id, limitation_index
            ",
        )
        .map_err(ScanSummaryError::QueryDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok(SummaryLimitation {
                code: row.get(0)?,
                message: row.get(1)?,
            })
        })
        .map_err(ScanSummaryError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ScanSummaryError::QueryDatabase)
}

fn load_stage_metadata(
    connection: &Connection,
) -> Result<BTreeMap<String, String>, ScanSummaryError> {
    let mut statement = connection
        .prepare(
            "
            SELECT key, value
            FROM stage_metadata
            WHERE key LIKE 'git_%'
            ORDER BY key ASC
            ",
        )
        .map_err(ScanSummaryError::QueryDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(ScanSummaryError::QueryDatabase)?;
    let mut metadata = BTreeMap::new();
    for row in rows {
        let (key, value) = row.map_err(ScanSummaryError::QueryDatabase)?;
        metadata.insert(key, value);
    }
    Ok(metadata)
}

fn git_limitations(metadata: &BTreeMap<String, String>) -> Vec<SummaryLimitation> {
    [
        "git_diagnostic_message",
        "git_merge_heavy_warning",
        "git_broad_commit_warning",
        "git_author_concentration_warning",
    ]
    .into_iter()
    .filter_map(|key| {
        metadata.get(key).map(|message| SummaryLimitation {
            code: "git".to_owned(),
            message: message.clone(),
        })
    })
    .collect()
}

fn render_hotspot(hotspot: &GoHotspot) -> String {
    let fact = hotspot
        .fact
        .as_deref()
        .unwrap_or("No summary fact available.");
    format!(
        "    {}. {}  risk {:.1}/10 {}  {}",
        hotspot.rank, hotspot.relative_path, hotspot.risk_10, hotspot.risk_band, fact
    )
}

fn render_project_coverage(project: Option<&ProjectCoverage>) -> String {
    match project {
        Some(project) => format!(
            "  project_coverage production_go {:.1}%  scored {}/{} production Go files  active_files {}  confidence {}",
            project.coverage_percent,
            project.scored_file_count,
            project.active_go_file_count,
            project.active_file_count,
            project.confidence
        ),
        None => "  project_coverage production_go 0.0%  scored 0/0 production Go files  active_files 0  confidence none"
            .to_owned(),
    }
}

fn render_git_summary(git: &GitSummary) -> String {
    format!(
        "  git confidence {}  mode {}  collection {}  index_action {}",
        git.confidence.as_deref().unwrap_or("unknown"),
        git.mode.as_deref().unwrap_or("unknown"),
        git.collection.as_deref().unwrap_or("unknown"),
        git.index_action.as_deref().unwrap_or("unknown")
    )
}

fn render_limitations(limitations: &[SummaryLimitation]) -> String {
    if limitations.is_empty() {
        return "  limitations none".to_owned();
    }

    let text = limitations
        .iter()
        .map(|limitation| format!("{}: {}", limitation.code, limitation.message))
        .collect::<Vec<_>>()
        .join("; ");
    format!("  limitations {text}")
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::{
        load_scan_summary_from_connection, render_scan_summary, GitSummary, GoHotspot,
        ProjectCoverage, ScanSummary, SummaryLimitation,
    };

    #[test]
    fn renders_compact_summary_with_hotspots_and_limitations() {
        let summary = ScanSummary {
            index_path: PathBuf::from("C:\\repo\\.hotpath\\index.sqlite"),
            hotspots: vec![GoHotspot {
                rank: 1,
                relative_path: "internal/service/a.go".to_owned(),
                risk_10: 7.24,
                risk_band: "high".to_owned(),
                fact: Some("High total churn: 2500 changed lines".to_owned()),
            }],
            project: Some(ProjectCoverage {
                coverage_percent: 100.0,
                scored_file_count: 2,
                active_go_file_count: 2,
                active_file_count: 3,
                confidence: "high".to_owned(),
            }),
            git: GitSummary {
                confidence: Some("bounded".to_owned()),
                mode: Some("full".to_owned()),
                collection: Some("bounded_recent_stream".to_owned()),
                index_action: Some("fully_rebuilt".to_owned()),
            },
            limitations: vec![SummaryLimitation {
                code: "git".to_owned(),
                message: "Git warning".to_owned(),
            }],
        };

        let rendered = render_scan_summary(&summary);

        assert!(rendered.contains("summary"));
        assert!(rendered.contains("1. internal/service/a.go  risk 7.2/10 high"));
        assert!(rendered.contains(
            "project_coverage production_go 100.0%  scored 2/2 production Go files  active_files 3  confidence high"
        ));
        assert!(rendered.contains(
            "git confidence bounded  mode full  collection bounded_recent_stream  index_action fully_rebuilt"
        ));
        assert!(rendered.contains("limitations git: Git warning"));
        assert!(rendered.contains("index C:\\repo\\.hotpath\\index.sqlite"));
    }

    #[test]
    fn renders_stable_fallbacks_without_hotspots_or_project() {
        let summary = ScanSummary {
            index_path: PathBuf::from("index.sqlite"),
            hotspots: Vec::new(),
            project: None,
            git: GitSummary::default(),
            limitations: Vec::new(),
        };

        let rendered = render_scan_summary(&summary);

        assert!(rendered.contains("top_go_hotspots none"));
        assert!(rendered.contains(
            "project_coverage production_go 0.0%  scored 0/0 production Go files  active_files 0  confidence none"
        ));
        assert!(rendered.contains(
            "git confidence unknown  mode unknown  collection unknown  index_action unknown"
        ));
        assert!(rendered.contains("limitations none"));
    }

    #[test]
    fn loads_summary_in_deterministic_order() {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection
            .execute_batch(
                "
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
                CREATE TABLE project_risk_summary (
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
                CREATE TABLE project_risk_limitations (
                    formula_id TEXT NOT NULL,
                    limitation_index INTEGER NOT NULL,
                    code TEXT NOT NULL,
                    message TEXT NOT NULL,
                    PRIMARY KEY (formula_id, limitation_index)
                );
                CREATE TABLE stage_metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO file_risk_scores VALUES
                    ('b.go', 'b.go', 1, 'hotpath.score.go.v1', 2, 0.4, 4.0, 'medium', 0, 0, 0),
                    ('a.go', 'a.go', 1, 'hotpath.score.go.v1', 1, 0.7, 7.0, 'high', 0, 0, 0),
                    ('a_test.go', 'a_test.go', 1, 'hotpath.score.go.v1', 3, 0.9, 9.0, 'extreme', 0, 0, 1);
                INSERT INTO file_risk_facts VALUES
                    ('a.go', 'hotpath.score.go.v1', 0, 'summary', 'A fact'),
                    ('b.go', 'hotpath.score.go.v1', 0, 'summary', 'B fact');
                INSERT INTO project_risk_summary VALUES
                    ('hotpath.project_risk.go.v1', 1, 0.7, 7.0, 'high', 'high', 3, 2, 2, 0.66, 1.0, 0.7, 0.55, 1, 2, 'churn', 0.8, 'available');
                INSERT INTO project_risk_limitations VALUES
                    ('hotpath.project_risk.go.v1', 0, 'example', 'Example limitation');
                INSERT INTO stage_metadata VALUES
                    ('git_confidence', 'bounded'),
                    ('git_scan_mode', 'full'),
                    ('git_collection_mode', 'bounded_recent_stream'),
                    ('git_index_action', 'fully_rebuilt'),
                    ('git_diagnostic_message', 'Git diagnostic');
                ",
            )
            .expect("schema and rows should insert");

        let summary = load_scan_summary_from_connection(&connection, PathBuf::from("index.sqlite"))
            .expect("summary should load");

        assert_eq!(summary.hotspots[0].relative_path, "a.go");
        assert_eq!(summary.hotspots[1].relative_path, "b.go");
        assert_eq!(summary.hotspots.len(), 2);
        assert_eq!(
            summary
                .project
                .as_ref()
                .map(|project| project.coverage_percent),
            Some(100.0)
        );
        assert_eq!(summary.limitations[0].code, "example");
        assert_eq!(summary.limitations[1].message, "Git diagnostic");
    }
}
