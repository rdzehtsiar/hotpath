// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

const INDEX_DIR: &str = ".hotpath";
const INDEX_DB: &str = "index.sqlite";
const INDEX_DISPLAY_PATH: &str = ".hotpath/index.sqlite";
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

pub fn render_scan_summary(summary: &ScanSummary) -> String {
    render_scan_summary_with_options(summary, false)
}

pub fn render_scan_summary_with_options(summary: &ScanSummary, verbose: bool) -> String {
    let mut lines = Vec::new();
    lines.push("Hotpath scan complete".to_owned());
    lines.push(String::new());
    lines.extend(render_repository_risk(summary.project.as_ref()));
    lines.push(String::new());
    lines.extend(render_coverage(summary.project.as_ref()));
    lines.push(String::new());
    lines.extend(render_confidence(summary.project.as_ref(), &summary.git));
    lines.push(String::new());
    lines.extend(render_hotspots(&summary.hotspots));
    lines.push(String::new());
    lines.extend(render_limitations(&summary.limitations));
    lines.push(String::new());
    lines.extend(render_index());

    if verbose {
        lines.push(String::new());
        lines.extend(render_verbose(summary));
    }

    lines.join("\n")
}

fn index_path(root: &Path) -> PathBuf {
    root.join(INDEX_DIR).join(INDEX_DB)
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

fn render_repository_risk(project: Option<&ProjectRiskSummary>) -> Vec<String> {
    let (score, spread, driver) = match project {
        Some(project) if project.scored_file_count > 0 && project.risk_band != "unavailable" => (
            format!("{:.1}/10  {}", project.risk_10, project.risk_band),
            hotspot_spread(project),
            project
                .dominant_dimension
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
        ),
        _ => (
            "unavailable".to_owned(),
            "no scored production Go files".to_owned(),
            "unavailable".to_owned(),
        ),
    };

    vec![
        "Repository Risk".to_owned(),
        format!("  Advisory score  {score}"),
        format!("  Hotspot spread  {spread}"),
        format!("  Primary driver  {driver}"),
    ]
}

fn hotspot_spread(project: &ProjectRiskSummary) -> String {
    if project.medium_risk_file_count == 0 {
        return "no medium-or-higher hotspots".to_owned();
    }

    format!(
        "{} high, {} medium-or-higher",
        project.high_risk_file_count, project.medium_risk_file_count
    )
}

fn render_coverage(project: Option<&ProjectRiskSummary>) -> Vec<String> {
    let project = project.cloned().unwrap_or(ProjectRiskSummary {
        risk_10: 0.0,
        risk_band: "unavailable".to_owned(),
        coverage_percent: 0.0,
        scored_file_count: 0,
        active_go_file_count: 0,
        active_file_count: 0,
        confidence: "none".to_owned(),
        high_risk_file_count: 0,
        medium_risk_file_count: 0,
        dominant_dimension: None,
        git_index_status: "unavailable".to_owned(),
    });
    let other_active_files = project
        .active_file_count
        .saturating_sub(project.active_go_file_count);

    vec![
        "Coverage".to_owned(),
        format!(
            "  Files analyzed      {}/{}",
            project.active_file_count, project.active_file_count
        ),
        format!(
            "  Production Go       {}/{} scored  ({:.1}%)",
            project.scored_file_count, project.active_go_file_count, project.coverage_percent
        ),
        format!("  Other active files  {other_active_files} indexed, not scored by Go risk model"),
    ]
}

fn render_confidence(project: Option<&ProjectRiskSummary>, git: &GitSummary) -> Vec<String> {
    vec![
        "Confidence".to_owned(),
        format!("  Git history  {}", git_confidence_sentence(project, git)),
        format!("  Scoring      {}", scoring_confidence_sentence(project)),
    ]
}

fn git_confidence_sentence(project: Option<&ProjectRiskSummary>, git: &GitSummary) -> &'static str {
    if project.is_some_and(|project| project.git_index_status != "available") {
        return "Unavailable; scores use file and parser signals only.";
    }

    match git.confidence.as_deref() {
        Some("full") => "Full Git history analyzed for this repository.",
        Some("bounded") => {
            "Bounded recent history; suitable for hotspot ranking, not full lifetime risk."
        }
        Some("not_git" | "shallow_skipped" | "error_skipped") => {
            "Unavailable; scores use file and parser signals only."
        }
        Some("up_to_date") => "Reused previously indexed Git history for the current HEAD.",
        Some(_) => "Git history quality is known but limited; review limitations before acting.",
        None => "Unknown; review limitations before acting.",
    }
}

fn scoring_confidence_sentence(project: Option<&ProjectRiskSummary>) -> &'static str {
    match project {
        Some(project) if project.active_go_file_count == 0 => {
            "No production Go files were available to score."
        }
        Some(project) => match project.confidence.as_str() {
            "high" => "High coverage for production Go files.",
            "medium" => "Partial coverage for production Go files.",
            "low" => "Low coverage; treat repository-level risk as directional.",
            "none" => "No production Go files were available to score.",
            _ => "Coverage is available; review limitations before acting.",
        },
        None => "No production Go files were available to score.",
    }
}

fn render_hotspots(hotspots: &[GoHotspot]) -> Vec<String> {
    let mut lines = vec!["Top Hotspots".to_owned()];
    if hotspots.is_empty() {
        lines.push("  none".to_owned());
        return lines;
    }

    lines.push("  #  Risk  Severity  File".to_owned());
    for hotspot in hotspots {
        lines.push(format!(
            "  {:<2} {:<5.1} {:<9} {}",
            hotspot.rank, hotspot.risk_10, hotspot.risk_band, hotspot.relative_path
        ));
        lines.push(format!(
            "     reason: {}",
            hotspot
                .fact
                .as_deref()
                .unwrap_or("No summary fact available.")
        ));
    }
    lines
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
            .map(|limitation| format!("  - {}", limitation.message)),
    );
    lines
}

fn render_index() -> Vec<String> {
    vec!["Index".to_owned(), format!("  {INDEX_DISPLAY_PATH}")]
}

fn render_verbose(summary: &ScanSummary) -> Vec<String> {
    let git = &summary.git;
    vec![
        "Verbose".to_owned(),
        format!(
            "  git mode {}  confidence {}  collection {}",
            display_option(&git.mode),
            display_option(&git.confidence),
            display_option(&git.collection)
        ),
        format!(
            "  git bounds max_commits {}  max_age_days {}  first_parent {}  renames {}",
            display_option(&git.max_commits),
            display_option(&git.max_age_days),
            display_option(&git.first_parent),
            display_option(&git.renames)
        ),
        format!(
            "  git windows recent_churn_window_days {}  cochange_max_files_per_commit {}",
            display_option(&git.recent_churn_window_days),
            display_option(&git.cochange_max_files_per_commit)
        ),
        format!(
            "  git reference head_committer_timestamp {}",
            display_option(&git.head_timestamp)
        ),
        format!("  index action {}", display_option(&git.index_action)),
        format!("  index path {}", summary.index_path.display()),
    ]
}

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
        load_scan_summary_from_connection, render_scan_summary, render_scan_summary_with_options,
        GitSummary, GoHotspot, ProjectRiskSummary, ScanSummary, SummaryLimitation,
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
                message: "Git warning".to_owned(),
            }],
        };

        let rendered = render_scan_summary(&summary);

        assert!(rendered.starts_with("Hotpath scan complete"));
        assert!(rendered.contains("Repository Risk"));
        assert!(rendered.contains("Advisory score  6.8/10  high"));
        assert!(rendered.contains("Hotspot spread  1 high, 2 medium-or-higher"));
        assert!(rendered.contains("Primary driver  churn"));
        assert!(rendered.contains("Production Go       2/2 scored  (100.0%)"));
        assert!(rendered.contains(
            "Git history  Bounded recent history; suitable for hotspot ranking, not full lifetime risk."
        ));
        assert!(rendered.contains("  #  Risk  Severity  File"));
        assert!(rendered.contains("  1  7.2   high      internal/service/a.go"));
        assert!(rendered.contains("     reason: High total churn: 2500 changed lines"));
        assert!(rendered.contains("  - Git warning"));
        assert!(rendered.contains("Index\n  .hotpath/index.sqlite"));
        assert!(!rendered.contains("fully_rebuilt"));
        assert!(!rendered.contains("C:\\repo\\.hotpath\\index.sqlite"));
    }

    #[test]
    fn renders_verbose_appendix_with_raw_git_and_index_details() {
        let summary = ScanSummary {
            index_path: PathBuf::from("C:\\repo\\.hotpath\\index.sqlite"),
            hotspots: Vec::new(),
            project: None,
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
            limitations: Vec::new(),
        };

        let rendered = render_scan_summary_with_options(&summary, true);

        assert!(rendered.contains("Verbose"));
        assert!(rendered
            .contains("git mode full  confidence bounded  collection bounded_recent_stream"));
        assert!(rendered.contains(
            "git bounds max_commits 50000  max_age_days 730  first_parent true  renames false"
        ));
        assert!(rendered.contains("index action fully_rebuilt"));
        assert!(rendered.contains("index path C:\\repo\\.hotpath\\index.sqlite"));
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

        assert!(rendered.contains("Advisory score  unavailable"));
        assert!(rendered.contains("Hotspot spread  no scored production Go files"));
        assert!(rendered.contains("Files analyzed      0/0"));
        assert!(rendered.contains("Production Go       0/0 scored  (0.0%)"));
        assert!(rendered.contains("Top Hotspots\n  none"));
        assert!(rendered.contains("Limitations\n  none"));
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
}
