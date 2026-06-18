// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::hotspots::{phrase_for_tags, tags_for_score_signals, HotspotTerm};

const INDEX_DIR: &str = ".hotpath";
const INDEX_DB: &str = "index.sqlite";
const TOP_HOTSPOT_LIMIT: u64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct ScanSummary {
    pub index_path: PathBuf,
    pub hotspots: Vec<GoHotspot>,
    pub project: Option<ProjectRiskSummary>,
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
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRiskSummary {
    pub risk_10: f64,
    pub risk_band: String,
    pub coverage_percent: f64,
    pub scored_file_count: u64,
    pub active_go_file_count: u64,
    pub active_file_count: u64,
    pub confidence: String,
    pub high_risk_file_count: u64,
    pub medium_risk_file_count: u64,
    pub dominant_dimension: Option<String>,
    pub git_index_status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitSummary {
    pub confidence: Option<String>,
    pub mode: Option<String>,
    pub collection: Option<String>,
    pub index_action: Option<String>,
    pub max_commits: Option<String>,
    pub max_age_days: Option<String>,
    pub first_parent: Option<String>,
    pub renames: Option<String>,
    pub cochange_max_files_per_commit: Option<String>,
    pub recent_churn_window_days: Option<String>,
    pub head_timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandCounts {
    pub extreme: u64,
    pub high: u64,
    pub medium: u64,
    pub low: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryDriverSummary {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskSummary {
    pub score: Option<f64>,
    pub band: String,
    pub primary_driver: Option<PrimaryDriverSummary>,
    pub files_by_band: BandCounts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRunInfo {
    pub scan_type: String,
    pub duration_ms: u64,
    pub files_detected: u64,
    pub files_analyzed: u64,
    pub git_history: String,
    pub commits_processed: u64,
    pub commits_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanRunSummary {
    pub assessment_reliable: bool,
    pub scoring_confidence: String,
    pub risk: RiskSummary,
    pub scan: ScanRunInfo,
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

pub fn load_band_counts(root: impl AsRef<Path>) -> Result<BandCounts, ScanSummaryError> {
    let index_path = index_path(root.as_ref());
    let connection =
        Connection::open(&index_path).map_err(|source| ScanSummaryError::OpenDatabase {
            path: index_path.clone(),
            source,
        })?;
    load_band_counts_from_connection(&connection)
}

fn load_band_counts_from_connection(
    connection: &Connection,
) -> Result<BandCounts, ScanSummaryError> {
    let mut statement = connection
        .prepare(
            "
            SELECT risk_band, COUNT(*)
            FROM file_risk_scores
            WHERE is_test = 0
            GROUP BY risk_band
            ",
        )
        .map_err(ScanSummaryError::QueryDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(ScanSummaryError::QueryDatabase)?;
    let mut counts = BandCounts {
        extreme: 0,
        high: 0,
        medium: 0,
        low: 0,
    };
    for row in rows {
        let (band, count) = row.map_err(ScanSummaryError::QueryDatabase)?;
        let count = u64::try_from(count).unwrap_or_default();
        match band.as_str() {
            "extreme" => counts.extreme = count,
            "high" => counts.high = count,
            "medium" => counts.medium = count,
            "low" => counts.low = count,
            _ => {}
        }
    }
    Ok(counts)
}

fn load_scan_summary_from_connection(
    connection: &Connection,
    index_path: PathBuf,
) -> Result<ScanSummary, ScanSummaryError> {
    let metadata = load_stage_metadata(connection)?;
    let project = load_project_risk_summary(connection)?;
    let mut limitations = load_project_limitations(connection)?;
    limitations.extend(derived_limitations(project.as_ref()));
    limitations.extend(git_limitations(&metadata));
    let limitations = deduplicate_limitations(limitations);

    Ok(ScanSummary {
        index_path,
        hotspots: load_hotspots(connection)?,
        project,
        git: GitSummary {
            confidence: metadata.get("git_confidence").cloned(),
            mode: metadata
                .get("git_scan_mode")
                .or_else(|| metadata.get("git_mode"))
                .cloned(),
            collection: metadata.get("git_collection_mode").cloned(),
            index_action: metadata.get("git_index_action").cloned(),
            max_commits: metadata.get("git_max_commits").cloned(),
            max_age_days: metadata.get("git_max_age_days").cloned(),
            first_parent: metadata.get("git_first_parent").cloned(),
            renames: metadata.get("git_renames").cloned(),
            cochange_max_files_per_commit: metadata
                .get("git_cochange_max_files_per_commit")
                .cloned(),
            recent_churn_window_days: metadata.get("git_recent_churn_window_days").cloned(),
            head_timestamp: metadata.get("git_head_timestamp").cloned(),
        },
        limitations,
    })
}

pub fn render_scan_summary(summary: &ScanSummary, run: &ScanRunSummary) -> String {
    let mut lines = Vec::new();
    lines.push("Hotpath scan complete".to_owned());
    lines.push(String::new());
    lines.extend(render_assessment(run));
    lines.push(String::new());
    lines.extend(render_scan(&run.scan));
    lines.push(String::new());
    lines.extend(render_risk(&run.risk));
    lines.push(String::new());
    lines.extend(render_hotspots(&summary.hotspots));
    lines.push(String::new());
    lines.extend(render_limitations(&summary.limitations));
    lines.join("\n")
}

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_DB)
}

fn load_hotspots(connection: &Connection) -> Result<Vec<GoHotspot>, ScanSummaryError> {
    let terms = load_hotspot_terms(connection)?;
    let mut statement = connection
        .prepare(
            "
            SELECT
                score.relative_path,
                score.risk_10,
                score.risk_band,
                fact.message,
                score.is_generated,
                score.is_vendor,
                score.is_test
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
                row.get::<_, i64>(4)? != 0,
                row.get::<_, i64>(5)? != 0,
                row.get::<_, i64>(6)? != 0,
            ))
        })
        .map_err(ScanSummaryError::QueryDatabase)?;
    rows.enumerate()
        .map(|(index, row)| {
            let (relative_path, risk_10, risk_band, fact, is_generated, is_vendor, is_test) =
                row.map_err(ScanSummaryError::QueryDatabase)?;
            let row_terms = terms.get(&relative_path).cloned().unwrap_or_default();
            Ok(GoHotspot {
                rank: index as u64 + 1,
                relative_path,
                risk_10,
                risk_band,
                fact,
                tags: tags_for_score_signals(is_generated, is_vendor, is_test, &row_terms),
            })
        })
        .collect()
}

fn load_hotspot_terms(
    connection: &Connection,
) -> Result<BTreeMap<String, Vec<HotspotTerm>>, ScanSummaryError> {
    let mut statement = connection
        .prepare(
            "
            SELECT relative_path, term_name, normalized_value
            FROM file_risk_terms
            WHERE formula_id = 'hotpath.score.go.v1'
            ORDER BY relative_path ASC, term_name ASC
            ",
        )
        .map_err(ScanSummaryError::QueryDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HotspotTerm {
                    name: row.get(1)?,
                    normalized_value: row.get(2)?,
                },
            ))
        })
        .map_err(ScanSummaryError::QueryDatabase)?;

    let mut terms = BTreeMap::new();
    for row in rows {
        let (path, term) = row.map_err(ScanSummaryError::QueryDatabase)?;
        terms.entry(path).or_insert_with(Vec::new).push(term);
    }
    Ok(terms)
}

fn load_project_risk_summary(
    connection: &Connection,
) -> Result<Option<ProjectRiskSummary>, ScanSummaryError> {
    let result = connection.query_row(
        "
        SELECT
            risk_10,
            risk_band,
            scoring_coverage,
            go_score_coverage,
            scored_file_count,
            active_go_file_count,
            active_file_count,
            confidence,
            high_risk_file_count,
            medium_risk_file_count,
            dominant_dimension,
            git_index_status
        FROM project_risk_summary
        ORDER BY formula_id
        LIMIT 1
        ",
        [],
        |row| {
            let scoring_coverage = row.get::<_, f64>(2)?;
            let go_score_coverage = row.get::<_, Option<f64>>(3)?;
            Ok(ProjectRiskSummary {
                risk_10: row.get(0)?,
                risk_band: row.get(1)?,
                coverage_percent: go_score_coverage
                    .unwrap_or(scoring_coverage)
                    .clamp(0.0, 1.0)
                    * 100.0,
                scored_file_count: i64_to_u64(row.get::<_, i64>(4)?),
                active_go_file_count: i64_to_u64(row.get::<_, i64>(5)?),
                active_file_count: i64_to_u64(row.get::<_, i64>(6)?),
                confidence: row.get(7)?,
                high_risk_file_count: i64_to_u64(row.get::<_, i64>(8)?),
                medium_risk_file_count: i64_to_u64(row.get::<_, i64>(9)?),
                dominant_dimension: row.get(10)?,
                git_index_status: row.get(11)?,
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

fn derived_limitations(project: Option<&ProjectRiskSummary>) -> Vec<SummaryLimitation> {
    let Some(project) = project else {
        return Vec::new();
    };

    if project.active_go_file_count > 0 && project.active_file_count > project.active_go_file_count
    {
        return vec![SummaryLimitation {
            code: "language_scope".to_owned(),
            message: "Only production Go files receive risk scores in the default summary."
                .to_owned(),
        }];
    }

    Vec::new()
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

fn deduplicate_limitations(limitations: Vec<SummaryLimitation>) -> Vec<SummaryLimitation> {
    let mut seen = BTreeSet::new();
    limitations
        .into_iter()
        .filter(|limitation| seen.insert(limitation.message.clone()))
        .collect()
}

fn render_assessment(run: &ScanRunSummary) -> Vec<String> {
    vec![
        "Assessment".to_owned(),
        format!("  Reliable: {}", run.assessment_reliable),
        format!("  Scoring confidence: {}", run.scoring_confidence),
        format!("  Reason: {}", assessment_reason(run)),
    ]
}

fn assessment_reason(run: &ScanRunSummary) -> &'static str {
    match (
        run.assessment_reliable,
        run.scoring_confidence.as_str(),
        run.scan.git_history.as_str(),
    ) {
        (true, "high", _) => "High scoring coverage and repository context are available.",
        (true, "medium", _) => "Medium scoring coverage and repository context are available.",
        (false, "none", _) => "No production Go files were scored.",
        (false, "low", _) => "Scoring coverage is low.",
        (false, "high", "absent") => {
            "High scoring coverage, but repository context is unavailable."
        }
        (false, "medium", "absent") => {
            "Medium scoring coverage, but repository context is unavailable."
        }
        (true, _, _) => "Scoring coverage and repository context are available.",
        (false, _, _) => "Assessment reliability is limited by incomplete scoring context.",
    }
}

fn render_scan(scan: &ScanRunInfo) -> Vec<String> {
    let commits = scan
        .commits_total
        .map(|total| format!("{} of {total}", scan.commits_processed))
        .unwrap_or_else(|| scan.commits_processed.to_string());

    vec![
        "Scan".to_owned(),
        format!("  Type: {}", scan.scan_type),
        format!("  Duration: {} ms", scan.duration_ms),
        format!(
            "  Files: {} detected, {} analyzed",
            scan.files_detected, scan.files_analyzed
        ),
        format!("  Git history: {}", scan.git_history),
        format!("  Commits processed: {commits}"),
    ]
}

fn render_risk(risk: &RiskSummary) -> Vec<String> {
    let score = risk
        .score
        .map(|score| format!("{score:.1}"))
        .unwrap_or_else(|| "unavailable".to_owned());
    let primary_driver = risk
        .primary_driver
        .as_ref()
        .map(|driver| driver.label.clone())
        .unwrap_or_else(|| "none".to_owned());

    vec![
        "Risk".to_owned(),
        format!("  Score: {score}"),
        format!("  Band: {}", risk.band),
        format!("  Primary driver: {primary_driver}"),
        format!(
            "  Files by band: extreme {}  high {}  medium {}  low {}",
            risk.files_by_band.extreme,
            risk.files_by_band.high,
            risk.files_by_band.medium,
            risk.files_by_band.low
        ),
    ]
}

fn render_hotspots(hotspots: &[GoHotspot]) -> Vec<String> {
    let mut lines = vec!["Top Hotspots".to_owned()];
    if hotspots.is_empty() {
        lines.push("  none".to_owned());
        return lines;
    }

    for hotspot in hotspots {
        lines.push(String::new());
        lines.push(format!("{:>2}  {}", hotspot.rank, hotspot.relative_path));
        if let Some(reason) = hotspot
            .fact
            .as_deref()
            .filter(|reason| !reason.trim().is_empty())
        {
            lines.push(format!("    {}", normalize_hotspot_reason(reason)));
        } else if let Some(phrase) = phrase_for_tags(&hotspot.tags) {
            lines.push(format!("    {phrase}"));
        }
    }
    lines
}

fn normalize_hotspot_reason(reason: &str) -> String {
    capitalize_first_letter(reason.trim_end_matches(['.', '!', '?']).trim_end())
}

fn render_limitations(limitations: &[SummaryLimitation]) -> Vec<String> {
    let mut lines = vec!["Limitations".to_owned()];
    if limitations.is_empty() {
        lines.push("  none".to_owned());
        return lines;
    }

    lines.extend(
        limitations
            .iter()
            .map(|limitation| format!("  - {}", normalize_limitation_message(&limitation.message))),
    );
    lines
}

fn normalize_limitation_message(message: &str) -> String {
    let trimmed = message.trim_end_matches(['.', '!', '?']).trim_end();
    capitalize_first_letter(trimmed)
}

fn capitalize_first_letter(message: &str) -> String {
    let mut chars = message.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };

    if first.is_ascii_lowercase() {
        format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
    } else {
        message.to_owned()
    }
}

#[allow(dead_code)]
fn display_option(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::{
        load_scan_summary_from_connection, render_scan_summary, BandCounts, GitSummary, GoHotspot,
        PrimaryDriverSummary, ProjectRiskSummary, RiskSummary, ScanRunInfo, ScanRunSummary,
        ScanSummary, SummaryLimitation,
    };

    #[test]
    fn renders_user_oriented_summary_with_hotspots_and_limitations() {
        let summary = ScanSummary {
            index_path: PathBuf::from("C:\\repo\\.hotpath\\index.sqlite"),
            hotspots: vec![GoHotspot {
                rank: 1,
                relative_path: "internal/service/a.go".to_owned(),
                risk_10: 7.24,
                risk_band: "high".to_owned(),
                fact: Some("High total churn: 2500 changed lines".to_owned()),
                tags: vec!["high churn".to_owned(), "complexity pressure".to_owned()],
            }],
            project: Some(ProjectRiskSummary {
                risk_10: 6.82,
                risk_band: "high".to_owned(),
                coverage_percent: 100.0,
                scored_file_count: 2,
                active_go_file_count: 2,
                active_file_count: 3,
                confidence: "high".to_owned(),
                high_risk_file_count: 1,
                medium_risk_file_count: 2,
                dominant_dimension: Some("churn".to_owned()),
                git_index_status: "available".to_owned(),
            }),
            git: GitSummary {
                confidence: Some("bounded".to_owned()),
                mode: Some("full".to_owned()),
                collection: Some("bounded_recent_stream".to_owned()),
                index_action: Some("fully_rebuilt".to_owned()),
                max_commits: Some("50000".to_owned()),
                max_age_days: Some("730".to_owned()),
                first_parent: Some("true".to_owned()),
                renames: Some("false".to_owned()),
                cochange_max_files_per_commit: Some("100".to_owned()),
                recent_churn_window_days: Some("90".to_owned()),
                head_timestamp: Some("1700000000".to_owned()),
            },
            limitations: vec![SummaryLimitation {
                code: "git".to_owned(),
                message: "git warning".to_owned(),
            }],
        };

        let rendered = render_scan_summary(&summary, &example_run_summary());

        assert!(rendered.starts_with("Hotpath scan complete"));
        assert!(rendered.contains("Assessment"));
        assert!(rendered.contains("  Reliable: true"));
        assert!(rendered.contains("  Scoring confidence: high"));
        assert!(rendered.contains("Risk"));
        assert!(rendered.contains("  Score: 6.8"));
        assert!(rendered.contains("  Band: high"));
        assert!(rendered.contains("  Primary driver: Churn"));
        assert!(!rendered.contains("  Primary driver: churn (Churn)"));
        assert!(rendered.contains("  Files by band: extreme 0  high 1  medium 1  low 0"));
        assert!(rendered.contains("\nScan\n"));
        assert!(rendered.contains("  Type: full"));
        assert!(rendered.contains("  Files: 3 detected, 3 analyzed"));
        assert!(!rendered.contains("duration_ms"));
        assert!(!rendered.contains("files_detected"));
        assert!(rendered.contains("Top Hotspots\n\n 1  internal/service/a.go"));
        assert!(rendered.contains("    High total churn: 2500 changed lines"));
        assert!(!rendered.contains("  #  Risk  Severity  File"));
        assert!(!rendered.contains("    Frequently changed, high-complexity file"));
        assert!(rendered.contains("  - Git warning"));
        assert!(!rendered.contains("  - git warning"));
        assert!(!rendered.contains("Index\n  .hotpath/index.sqlite"));
        assert!(!rendered.contains("fully_rebuilt"));
        assert!(!rendered.contains("C:\\repo\\.hotpath\\index.sqlite"));
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

        let rendered = render_scan_summary(&summary, &unavailable_run_summary());

        assert!(rendered.contains("  Reliable: false"));
        assert!(rendered.contains("  Scoring confidence: none"));
        assert!(rendered.contains("  Score: unavailable"));
        assert!(rendered.contains("  Band: unavailable"));
        assert!(rendered.contains("  Primary driver: none"));
        assert!(rendered.contains("\nScan\n"));
        assert!(rendered.contains("  Files: 0 detected, 0 analyzed"));
        assert!(rendered.contains("Top Hotspots\n  none"));
        assert!(rendered.contains("Limitations\n  none"));
    }

    #[test]
    fn renders_hotspot_tag_phrase_when_fact_is_missing() {
        let summary = ScanSummary {
            index_path: PathBuf::from("index.sqlite"),
            hotspots: vec![GoHotspot {
                rank: 1,
                relative_path: "internal/service/a.go".to_owned(),
                risk_10: 7.24,
                risk_band: "high".to_owned(),
                fact: None,
                tags: vec!["high churn".to_owned(), "complexity pressure".to_owned()],
            }],
            project: None,
            git: GitSummary::default(),
            limitations: Vec::new(),
        };

        let rendered = render_scan_summary(&summary, &unavailable_run_summary());

        assert!(rendered.contains("    Frequently changed, high-complexity file"));
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
                CREATE TABLE file_risk_terms (
                    relative_path TEXT NOT NULL,
                    formula_id TEXT NOT NULL,
                    term_name TEXT NOT NULL,
                    normalized_value REAL,
                    PRIMARY KEY (relative_path, formula_id, term_name)
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
                INSERT INTO file_risk_terms VALUES
                    ('a.go', 'hotpath.score.go.v1', 'churn', 1.0),
                    ('b.go', 'hotpath.score.go.v1', 'complexity_pressure', 1.0);
                INSERT INTO project_risk_summary VALUES
                    ('hotpath.project_risk.go.v1', 1, 0.7, 7.0, 'high', 'high', 3, 2, 2, 0.66, 1.0, 0.7, 0.55, 1, 2, 'churn', 0.8, 'available');
                INSERT INTO project_risk_limitations VALUES
                    ('hotpath.project_risk.go.v1', 0, 'example', 'Example limitation');
                INSERT INTO stage_metadata VALUES
                    ('git_confidence', 'bounded'),
                    ('git_scan_mode', 'full'),
                    ('git_collection_mode', 'bounded_recent_stream'),
                    ('git_index_action', 'fully_rebuilt'),
                    ('git_max_commits', '50000'),
                    ('git_max_age_days', '730'),
                    ('git_first_parent', 'true'),
                    ('git_renames', 'false'),
                    ('git_cochange_max_files_per_commit', '100'),
                    ('git_recent_churn_window_days', '90'),
                    ('git_head_timestamp', '1700000000'),
                    ('git_diagnostic_message', 'Git diagnostic'),
                    ('git_broad_commit_warning', 'Example limitation');
                ",
            )
            .expect("schema and rows should insert");

        let summary = load_scan_summary_from_connection(&connection, PathBuf::from("index.sqlite"))
            .expect("summary should load");

        assert_eq!(summary.hotspots[0].relative_path, "a.go");
        assert_eq!(summary.hotspots[1].relative_path, "b.go");
        assert_eq!(summary.hotspots.len(), 2);
        assert_eq!(summary.hotspots[0].tags, vec!["high churn"]);
        assert_eq!(summary.hotspots[1].tags, vec!["complexity pressure"]);
        let project = summary.project.as_ref().expect("project summary loads");
        assert_eq!(project.coverage_percent, 100.0);
        assert_eq!(project.risk_10, 7.0);
        assert_eq!(project.risk_band, "high");
        assert_eq!(project.high_risk_file_count, 1);
        assert_eq!(project.medium_risk_file_count, 2);
        assert_eq!(project.dominant_dimension.as_deref(), Some("churn"));
        assert_eq!(summary.git.max_commits.as_deref(), Some("50000"));
        assert_eq!(summary.limitations[0].message, "Example limitation");
        assert_eq!(
            summary.limitations[1].message,
            "Only production Go files receive risk scores in the default summary."
        );
        assert_eq!(summary.limitations[2].message, "Git diagnostic");
        assert_eq!(summary.limitations.len(), 3);
    }

    fn example_run_summary() -> ScanRunSummary {
        ScanRunSummary {
            assessment_reliable: true,
            scoring_confidence: "high".to_owned(),
            risk: RiskSummary {
                score: Some(6.82),
                band: "high".to_owned(),
                primary_driver: Some(PrimaryDriverSummary {
                    id: "churn".to_owned(),
                    label: "Churn".to_owned(),
                }),
                files_by_band: BandCounts {
                    extreme: 0,
                    high: 1,
                    medium: 1,
                    low: 0,
                },
            },
            scan: ScanRunInfo {
                scan_type: "full".to_owned(),
                duration_ms: 42,
                files_detected: 3,
                files_analyzed: 3,
                git_history: "bounded".to_owned(),
                commits_processed: 2,
                commits_total: Some(2),
            },
        }
    }

    fn unavailable_run_summary() -> ScanRunSummary {
        ScanRunSummary {
            assessment_reliable: false,
            scoring_confidence: "none".to_owned(),
            risk: RiskSummary {
                score: None,
                band: "unavailable".to_owned(),
                primary_driver: None,
                files_by_band: BandCounts {
                    extreme: 0,
                    high: 0,
                    medium: 0,
                    low: 0,
                },
            },
            scan: ScanRunInfo {
                scan_type: "full".to_owned(),
                duration_ms: 0,
                files_detected: 0,
                files_analyzed: 0,
                git_history: "absent".to_owned(),
                commits_processed: 0,
                commits_total: None,
            },
        }
    }
}
