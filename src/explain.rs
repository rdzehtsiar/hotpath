// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags};

const INDEX_DB: &str = ".hotpath/index.sqlite";
const COCHANGE_LIMIT: i64 = 10;

#[derive(Debug)]
pub enum ExplainError {
    NoIndex {
        current_dir: PathBuf,
    },
    OutsideIndexRoot {
        path: PathBuf,
        index_root: PathBuf,
    },
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    QueryDatabase(rusqlite::Error),
    FileNotIndexed {
        relative_path: String,
    },
}

impl fmt::Display for ExplainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoIndex { current_dir } => write!(
                f,
                "no Hotpath index found from '{}'. Run hotpath scan first.",
                current_dir.display()
            ),
            Self::OutsideIndexRoot { path, index_root } => write!(
                f,
                "path '{}' is outside indexed repository '{}'",
                path.display(),
                index_root.display()
            ),
            Self::OpenDatabase { path, source } => write!(
                f,
                "failed to open Hotpath index '{}': {source}",
                path.display()
            ),
            Self::QueryDatabase(source) => write!(f, "failed to read Hotpath explain data: {source}"),
            Self::FileNotIndexed { relative_path } => write!(
                f,
                "file '{relative_path}' is not in the current Hotpath index. Run hotpath scan first."
            ),
        }
    }
}

impl std::error::Error for ExplainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. } => Some(source),
            Self::QueryDatabase(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainReport {
    pub file: ExplainFile,
    pub score_unavailable_reasons: Vec<ExplainLimitation>,
    pub score: Option<ExplainScore>,
    pub terms: Vec<ExplainTerm>,
    pub raw_metrics: RawMetrics,
    pub facts: Vec<ExplainFact>,
    pub limitations: Vec<ExplainLimitation>,
    pub parser_diagnostics: Vec<ExplainLimitation>,
    pub owners: Vec<ExplainOwner>,
    pub git_context: GitContext,
    pub source_coupling: SourceCoupling,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainFile {
    pub relative_path: String,
    pub absolute_path: String,
    pub active_scan_id: u64,
    pub language_id: Option<String>,
    pub content_kind: String,
    pub extension: Option<String>,
    pub is_generated: bool,
    pub is_vendor: bool,
    pub is_test: bool,
    pub parser_status: String,
    pub parser_recognition_attempts: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainScore {
    pub formula_id: String,
    pub rank: u64,
    pub score: f64,
    pub risk_10: f64,
    pub risk_band: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainTerm {
    pub name: String,
    pub raw_value: Option<f64>,
    pub normalized_value: Option<f64>,
    pub weight: f64,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawMetrics {
    pub byte_size: Option<u64>,
    pub mtime_ms: Option<u64>,
    pub line_count: Option<u64>,
    pub symbol_count: u64,
    pub function_count: u64,
    pub method_count: u64,
    pub type_count: u64,
    pub import_count: u64,
    pub complexity_pressure: Option<u64>,
    pub max_function_complexity_pressure: Option<u64>,
    pub commits_per_file: u64,
    pub total_added_lines: u64,
    pub total_deleted_lines: u64,
    pub total_churn_lines: u64,
    pub recent_added_lines: u64,
    pub recent_deleted_lines: u64,
    pub recent_churn_lines: u64,
    pub author_count: u64,
    pub first_touch_timestamp: Option<u64>,
    pub last_touch_timestamp: Option<u64>,
    pub file_age_days: Option<u64>,
    pub owner_count: Option<u64>,
    pub dominant_owner: Option<String>,
    pub dominant_owner_share: Option<f64>,
    pub co_changed_file_count: u64,
    pub source_coupling_pressure_in: Option<u64>,
    pub source_coupling_pressure_out: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainFact {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainOwner {
    pub rank: u64,
    pub author: String,
    pub ownership_score: f64,
    pub ownership_share: f64,
    pub touch_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    pub metadata: BTreeMap<String, String>,
    pub limitations: Vec<ExplainLimitation>,
    pub cochanges: Vec<CochangePartner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CochangePartner {
    pub path: String,
    pub co_change_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCoupling {
    pub package_path: Option<String>,
    pub inbound_count: Option<u64>,
    pub outbound_count: Option<u64>,
    pub outbound_references: Vec<SourceReference>,
    pub outbound_edges: Vec<SourceEdge>,
    pub inbound_sources: Vec<SourceEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReference {
    pub reference_index: u64,
    pub reference_kind: String,
    pub raw_target: String,
    pub resolved_package: Option<String>,
    pub is_resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEdge {
    pub source_path: String,
    pub source_package: String,
    pub target_package: String,
    pub reference_kind: String,
}

#[derive(Debug)]
struct FileFactsRow {
    absolute_path: String,
    relative_path: String,
    active_scan_id: u64,
    byte_size: Option<u64>,
    mtime_ms: Option<u64>,
    extension: Option<String>,
    content_kind: String,
    line_count: Option<u64>,
    is_generated: bool,
    is_vendor: bool,
    is_test: bool,
    parser_status: String,
    parser_recognition_attempts: u64,
    language_id: Option<String>,
    symbol_count: u64,
    function_count: u64,
    method_count: u64,
    type_count: u64,
    import_count: u64,
    complexity_pressure: Option<u64>,
    max_function_complexity_pressure: Option<u64>,
    diagnostics: String,
    commits_per_file: u64,
    total_added_lines: u64,
    total_deleted_lines: u64,
    total_churn_lines: u64,
    recent_added_lines: u64,
    recent_deleted_lines: u64,
    recent_churn_lines: u64,
    author_count: u64,
    first_touch_timestamp: Option<u64>,
    last_touch_timestamp: Option<u64>,
    file_age_days: Option<u64>,
    owner_count: Option<u64>,
    dominant_owner: Option<String>,
    dominant_owner_share: Option<f64>,
    co_changed_file_count: u64,
    source_coupling_pressure_in: Option<u64>,
    source_coupling_pressure_out: Option<u64>,
}

pub fn load_explain_report(
    current_dir: impl AsRef<Path>,
    input_path: impl AsRef<Path>,
) -> Result<ExplainReport, ExplainError> {
    let current_dir = current_dir.as_ref();
    let index_root = find_index_root(current_dir).ok_or_else(|| ExplainError::NoIndex {
        current_dir: current_dir.to_path_buf(),
    })?;
    let relative_path =
        normalize_index_relative_path(&index_root, current_dir, input_path.as_ref())?;
    let index_path = index_root.join(INDEX_DB);
    let connection = Connection::open_with_flags(&index_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| ExplainError::OpenDatabase {
            path: index_path.clone(),
            source,
        })?;
    load_explain_report_from_connection(&connection, &relative_path)
}

fn load_explain_report_from_connection(
    connection: &Connection,
    relative_path: &str,
) -> Result<ExplainReport, ExplainError> {
    let metadata = load_stage_metadata(connection)?;
    let file = load_file_facts(connection, relative_path)?.ok_or_else(|| {
        ExplainError::FileNotIndexed {
            relative_path: relative_path.to_owned(),
        }
    })?;
    let score = load_score(
        connection,
        relative_path,
        metadata.get("file_risk_formula_id"),
    )?;
    let formula_id = score.as_ref().map(|score| score.formula_id.as_str());
    let terms = load_terms(connection, relative_path, formula_id)?;
    let facts = load_facts(connection, relative_path, formula_id)?;
    let limitations = load_limitations(connection, relative_path, formula_id)?;
    let owners = load_owners(connection, relative_path)?;
    let source_coupling = load_source_coupling(connection, &file)?;
    let git_context = GitContext {
        metadata: metadata
            .iter()
            .filter(|(key, _)| key.starts_with("git_") || key.starts_with("source_dependency_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        limitations: git_limitations(&metadata),
        cochanges: load_cochanges(connection, relative_path)?,
    };
    let score_unavailable_reasons = if score.is_some() {
        Vec::new()
    } else {
        score_unavailable_reasons(&file)
    };

    Ok(ExplainReport {
        file: ExplainFile {
            relative_path: file.relative_path.clone(),
            absolute_path: file.absolute_path.clone(),
            active_scan_id: file.active_scan_id,
            language_id: file.language_id.clone(),
            content_kind: file.content_kind.clone(),
            extension: file.extension.clone(),
            is_generated: file.is_generated,
            is_vendor: file.is_vendor,
            is_test: file.is_test,
            parser_status: file.parser_status.clone(),
            parser_recognition_attempts: file.parser_recognition_attempts,
        },
        score_unavailable_reasons,
        score,
        terms,
        raw_metrics: RawMetrics {
            byte_size: file.byte_size,
            mtime_ms: file.mtime_ms,
            line_count: file.line_count,
            symbol_count: file.symbol_count,
            function_count: file.function_count,
            method_count: file.method_count,
            type_count: file.type_count,
            import_count: file.import_count,
            complexity_pressure: file.complexity_pressure,
            max_function_complexity_pressure: file.max_function_complexity_pressure,
            commits_per_file: file.commits_per_file,
            total_added_lines: file.total_added_lines,
            total_deleted_lines: file.total_deleted_lines,
            total_churn_lines: file.total_churn_lines,
            recent_added_lines: file.recent_added_lines,
            recent_deleted_lines: file.recent_deleted_lines,
            recent_churn_lines: file.recent_churn_lines,
            author_count: file.author_count,
            first_touch_timestamp: file.first_touch_timestamp,
            last_touch_timestamp: file.last_touch_timestamp,
            file_age_days: file.file_age_days,
            owner_count: file.owner_count,
            dominant_owner: file.dominant_owner.clone(),
            dominant_owner_share: file.dominant_owner_share,
            co_changed_file_count: file.co_changed_file_count,
            source_coupling_pressure_in: file.source_coupling_pressure_in,
            source_coupling_pressure_out: file.source_coupling_pressure_out,
        },
        facts,
        limitations,
        parser_diagnostics: parse_diagnostics_json(&file.diagnostics),
        owners,
        git_context,
        source_coupling,
    })
}

pub fn render_explain_text(report: &ExplainReport) -> String {
    let mut lines = Vec::new();
    lines.push("explain".to_owned());
    lines.push("File".to_owned());
    lines.push(format!("  path {}", report.file.relative_path));
    lines.push(format!("  absolute_path {}", report.file.absolute_path));
    lines.push(format!(
        "  language {}  content_kind {}  extension {}",
        display_opt(report.file.language_id.as_deref()),
        report.file.content_kind,
        display_opt(report.file.extension.as_deref())
    ));
    lines.push(format!(
        "  generated {}  vendor {}  test {}",
        report.file.is_generated, report.file.is_vendor, report.file.is_test
    ));
    lines.push(format!(
        "  parser_status {}  parser_recognition_attempts {}",
        report.file.parser_status, report.file.parser_recognition_attempts
    ));

    lines.push("Score".to_owned());
    match &report.score {
        Some(score) => {
            lines.push(format!(
                "  formula {}  rank {}  score {:.3}  risk {:.1}/10  band {}",
                score.formula_id, score.rank, score.score, score.risk_10, score.risk_band
            ));
        }
        None => {
            lines.push("  unavailable".to_owned());
            lines.extend(
                report
                    .score_unavailable_reasons
                    .iter()
                    .map(|reason| format!("  reason {}: {}", reason.code, reason.message)),
            );
        }
    }

    lines.push("Terms".to_owned());
    if report.terms.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(report.terms.iter().map(|term| {
            format!(
                "  {} raw {} normalized {} weight {:.3} contribution {:.3}",
                term.name,
                display_f64(term.raw_value),
                display_f64(term.normalized_value),
                term.weight,
                term.contribution
            )
        }));
    }

    lines.push("Raw Metrics".to_owned());
    lines.push(format!(
        "  size lines {} bytes {} mtime_ms {}",
        display_u64(report.raw_metrics.line_count),
        display_u64(report.raw_metrics.byte_size),
        display_u64(report.raw_metrics.mtime_ms)
    ));
    lines.push(format!(
        "  symbols total {} functions {} methods {} types {} imports {}",
        report.raw_metrics.symbol_count,
        report.raw_metrics.function_count,
        report.raw_metrics.method_count,
        report.raw_metrics.type_count,
        report.raw_metrics.import_count
    ));
    lines.push(format!(
        "  complexity file {} max_function {}",
        display_u64(report.raw_metrics.complexity_pressure),
        display_u64(report.raw_metrics.max_function_complexity_pressure)
    ));
    lines.push(format!(
        "  git commits {} churn total {} recent {} added {} deleted {} authors {}",
        report.raw_metrics.commits_per_file,
        report.raw_metrics.total_churn_lines,
        report.raw_metrics.recent_churn_lines,
        report.raw_metrics.total_added_lines,
        report.raw_metrics.total_deleted_lines,
        report.raw_metrics.author_count
    ));
    lines.push(format!(
        "  ownership owner_count {} dominant_owner {} dominant_share {}",
        display_u64(report.raw_metrics.owner_count),
        display_opt(report.raw_metrics.dominant_owner.as_deref()),
        display_f64(report.raw_metrics.dominant_owner_share)
    ));
    lines.push(format!(
        "  coupling source_in {} source_out {} co_changed_files {}",
        display_u64(report.raw_metrics.source_coupling_pressure_in),
        display_u64(report.raw_metrics.source_coupling_pressure_out),
        report.raw_metrics.co_changed_file_count
    ));

    render_facts_section(&mut lines, "Facts", &report.facts);
    render_limitations_section(&mut lines, "Limitations", &report.limitations);
    render_limitations_section(&mut lines, "Parser Diagnostics", &report.parser_diagnostics);

    lines.push("Owners".to_owned());
    if report.owners.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(report.owners.iter().map(|owner| {
            format!(
                "  {}. {} share {:.3} score {:.3} touches {}",
                owner.rank,
                owner.author,
                owner.ownership_share,
                owner.ownership_score,
                owner.touch_count
            )
        }));
    }

    lines.push("Git Context".to_owned());
    if report.git_context.metadata.is_empty() {
        lines.push("  metadata none".to_owned());
    } else {
        for (key, value) in &report.git_context.metadata {
            lines.push(format!("  {key} {value}"));
        }
    }
    if report.git_context.limitations.is_empty() {
        lines.push("  limitations none".to_owned());
    } else {
        lines.extend(
            report.git_context.limitations.iter().map(|limitation| {
                format!("  limitation {}: {}", limitation.code, limitation.message)
            }),
        );
    }
    if report.git_context.cochanges.is_empty() {
        lines.push("  cochanges none".to_owned());
    } else {
        lines.extend(report.git_context.cochanges.iter().map(|cochange| {
            format!(
                "  cochange {} count {}",
                cochange.path, cochange.co_change_count
            )
        }));
    }

    lines.push("Source Coupling".to_owned());
    lines.push(format!(
        "  package {}  inbound {}  outbound {}",
        display_opt(report.source_coupling.package_path.as_deref()),
        display_u64(report.source_coupling.inbound_count),
        display_u64(report.source_coupling.outbound_count)
    ));
    render_source_references(
        &mut lines,
        "  outbound_references",
        &report.source_coupling.outbound_references,
    );
    render_source_edges(
        &mut lines,
        "  outbound_edges",
        &report.source_coupling.outbound_edges,
    );
    render_source_edges(
        &mut lines,
        "  inbound_sources",
        &report.source_coupling.inbound_sources,
    );

    lines.join("\n")
}

fn render_facts_section(lines: &mut Vec<String>, title: &str, facts: &[ExplainFact]) {
    lines.push(title.to_owned());
    if facts.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(
            facts
                .iter()
                .map(|fact| format!("  {}: {}", fact.kind, fact.message)),
        );
    }
}

fn render_limitations_section(
    lines: &mut Vec<String>,
    title: &str,
    limitations: &[ExplainLimitation],
) {
    lines.push(title.to_owned());
    if limitations.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(
            limitations
                .iter()
                .map(|limitation| format!("  {}: {}", limitation.code, limitation.message)),
        );
    }
}

fn render_source_references(lines: &mut Vec<String>, title: &str, references: &[SourceReference]) {
    if references.is_empty() {
        lines.push(format!("{title} none"));
    } else {
        lines.push(title.to_owned());
        lines.extend(references.iter().map(|reference| {
            format!(
                "    {} {} raw {} resolved {}",
                reference.reference_index,
                reference.reference_kind,
                reference.raw_target,
                display_opt(reference.resolved_package.as_deref())
            )
        }));
    }
}

fn render_source_edges(lines: &mut Vec<String>, title: &str, edges: &[SourceEdge]) {
    if edges.is_empty() {
        lines.push(format!("{title} none"));
    } else {
        lines.push(title.to_owned());
        lines.extend(edges.iter().map(|edge| {
            format!(
                "    {} ({}) -> {} [{}]",
                edge.source_path, edge.source_package, edge.target_package, edge.reference_kind
            )
        }));
    }
}

fn load_file_facts(
    connection: &Connection,
    relative_path: &str,
) -> Result<Option<FileFactsRow>, ExplainError> {
    let result = connection.query_row(
        "
        SELECT
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
            is_test,
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
        FROM file_facts
        WHERE relative_path = ?1
        ",
        [relative_path],
        |row| {
            Ok(FileFactsRow {
                absolute_path: row.get(0)?,
                relative_path: row.get(1)?,
                active_scan_id: i64_to_u64(row.get::<_, i64>(2)?),
                byte_size: optional_i64_to_u64(row.get::<_, Option<i64>>(3)?),
                mtime_ms: optional_i64_to_u64(row.get::<_, Option<i64>>(4)?),
                extension: row.get(5)?,
                content_kind: row.get(6)?,
                line_count: optional_i64_to_u64(row.get::<_, Option<i64>>(7)?),
                is_generated: row.get::<_, i64>(8)? != 0,
                is_vendor: row.get::<_, i64>(9)? != 0,
                is_test: row.get::<_, i64>(10)? != 0,
                parser_status: row.get(11)?,
                parser_recognition_attempts: i64_to_u64(row.get::<_, i64>(12)?),
                language_id: row.get(13)?,
                symbol_count: i64_to_u64(row.get::<_, i64>(14)?),
                function_count: i64_to_u64(row.get::<_, i64>(15)?),
                method_count: i64_to_u64(row.get::<_, i64>(16)?),
                type_count: i64_to_u64(row.get::<_, i64>(17)?),
                import_count: i64_to_u64(row.get::<_, i64>(18)?),
                complexity_pressure: optional_i64_to_u64(row.get::<_, Option<i64>>(19)?),
                max_function_complexity_pressure: optional_i64_to_u64(
                    row.get::<_, Option<i64>>(20)?,
                ),
                diagnostics: row.get(21)?,
                commits_per_file: i64_to_u64(row.get::<_, i64>(22)?),
                total_added_lines: i64_to_u64(row.get::<_, i64>(23)?),
                total_deleted_lines: i64_to_u64(row.get::<_, i64>(24)?),
                total_churn_lines: i64_to_u64(row.get::<_, i64>(25)?),
                recent_added_lines: i64_to_u64(row.get::<_, i64>(26)?),
                recent_deleted_lines: i64_to_u64(row.get::<_, i64>(27)?),
                recent_churn_lines: i64_to_u64(row.get::<_, i64>(28)?),
                author_count: i64_to_u64(row.get::<_, i64>(29)?),
                first_touch_timestamp: optional_i64_to_u64(row.get::<_, Option<i64>>(30)?),
                last_touch_timestamp: optional_i64_to_u64(row.get::<_, Option<i64>>(31)?),
                file_age_days: optional_i64_to_u64(row.get::<_, Option<i64>>(32)?),
                owner_count: optional_i64_to_u64(row.get::<_, Option<i64>>(33)?),
                dominant_owner: row.get(34)?,
                dominant_owner_share: row.get(35)?,
                co_changed_file_count: i64_to_u64(row.get::<_, i64>(36)?),
                source_coupling_pressure_in: optional_i64_to_u64(row.get::<_, Option<i64>>(37)?),
                source_coupling_pressure_out: optional_i64_to_u64(row.get::<_, Option<i64>>(38)?),
            })
        },
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(ExplainError::QueryDatabase(source)),
    }
}

fn load_score(
    connection: &Connection,
    relative_path: &str,
    preferred_formula_id: Option<&String>,
) -> Result<Option<ExplainScore>, ExplainError> {
    let result = if let Some(formula_id) = preferred_formula_id {
        connection.query_row(
            "
            SELECT formula_id, rank, score, risk_10, risk_band
            FROM file_risk_scores
            WHERE relative_path = ?1 AND formula_id = ?2
            ",
            params![relative_path, formula_id],
            score_from_row,
        )
    } else {
        connection.query_row(
            "
            SELECT formula_id, rank, score, risk_10, risk_band
            FROM file_risk_scores
            WHERE relative_path = ?1
            ORDER BY formula_id
            LIMIT 1
            ",
            [relative_path],
            score_from_row,
        )
    };
    match result {
        Ok(score) => Ok(Some(score)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(ExplainError::QueryDatabase(source)),
    }
}

fn score_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExplainScore> {
    Ok(ExplainScore {
        formula_id: row.get(0)?,
        rank: i64_to_u64(row.get::<_, i64>(1)?),
        score: row.get(2)?,
        risk_10: row.get(3)?,
        risk_band: row.get(4)?,
    })
}

fn load_terms(
    connection: &Connection,
    relative_path: &str,
    formula_id: Option<&str>,
) -> Result<Vec<ExplainTerm>, ExplainError> {
    if let Some(formula_id) = formula_id {
        query_terms(
            connection,
            "
            SELECT term_name, raw_value, normalized_value, weight, contribution
            FROM file_risk_terms
            WHERE relative_path = ?1 AND formula_id = ?2
            ORDER BY term_name
            ",
            params![relative_path, formula_id],
        )
    } else {
        Ok(Vec::new())
    }
}

fn query_terms<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<ExplainTerm>, ExplainError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map(params, |row| {
            Ok(ExplainTerm {
                name: row.get(0)?,
                raw_value: row.get(1)?,
                normalized_value: row.get(2)?,
                weight: row.get(3)?,
                contribution: row.get(4)?,
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_facts(
    connection: &Connection,
    relative_path: &str,
    formula_id: Option<&str>,
) -> Result<Vec<ExplainFact>, ExplainError> {
    let Some(formula_id) = formula_id else {
        return Ok(Vec::new());
    };
    let mut statement = connection
        .prepare(
            "
            SELECT fact_kind, message
            FROM file_risk_facts
            WHERE relative_path = ?1 AND formula_id = ?2
            ORDER BY fact_index
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map(params![relative_path, formula_id], |row| {
            Ok(ExplainFact {
                kind: row.get(0)?,
                message: row.get(1)?,
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_limitations(
    connection: &Connection,
    relative_path: &str,
    formula_id: Option<&str>,
) -> Result<Vec<ExplainLimitation>, ExplainError> {
    let Some(formula_id) = formula_id else {
        return Ok(Vec::new());
    };
    let mut statement = connection
        .prepare(
            "
            SELECT code, message
            FROM file_risk_limitations
            WHERE relative_path = ?1 AND formula_id = ?2
            ORDER BY limitation_index
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map(params![relative_path, formula_id], |row| {
            Ok(ExplainLimitation {
                code: row.get(0)?,
                message: row.get(1)?,
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_owners(
    connection: &Connection,
    relative_path: &str,
) -> Result<Vec<ExplainOwner>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT owner_rank, author, ownership_score, ownership_share, touch_count
            FROM git_file_owners
            WHERE path = ?1
            ORDER BY owner_rank
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map([relative_path], |row| {
            Ok(ExplainOwner {
                rank: i64_to_u64(row.get::<_, i64>(0)?) + 1,
                author: row.get(1)?,
                ownership_score: row.get(2)?,
                ownership_share: row.get(3)?,
                touch_count: i64_to_u64(row.get::<_, i64>(4)?),
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_stage_metadata(connection: &Connection) -> Result<BTreeMap<String, String>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT key, value
            FROM stage_metadata
            ORDER BY key
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(ExplainError::QueryDatabase)?;
    let mut metadata = BTreeMap::new();
    for row in rows {
        let (key, value) = row.map_err(ExplainError::QueryDatabase)?;
        metadata.insert(key, value);
    }
    Ok(metadata)
}

fn load_source_coupling(
    connection: &Connection,
    file: &FileFactsRow,
) -> Result<SourceCoupling, ExplainError> {
    let package_path = load_package_path(connection, &file.relative_path)?;
    let outbound_references = load_outbound_references(connection, &file.relative_path)?;
    let outbound_edges = load_outbound_edges(connection, &file.relative_path)?;
    let inbound_sources = if let Some(package_path) = &package_path {
        load_inbound_sources(connection, &file.relative_path, package_path)?
    } else {
        Vec::new()
    };
    Ok(SourceCoupling {
        package_path,
        inbound_count: file.source_coupling_pressure_in,
        outbound_count: file.source_coupling_pressure_out,
        outbound_references,
        outbound_edges,
        inbound_sources,
    })
}

fn load_package_path(
    connection: &Connection,
    relative_path: &str,
) -> Result<Option<String>, ExplainError> {
    let result = connection.query_row(
        "
        SELECT package_path
        FROM source_file_packages
        WHERE file_path = ?1
        ",
        [relative_path],
        |row| row.get(0),
    );
    match result {
        Ok(package_path) => Ok(Some(package_path)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(ExplainError::QueryDatabase(source)),
    }
}

fn load_outbound_references(
    connection: &Connection,
    relative_path: &str,
) -> Result<Vec<SourceReference>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT reference_index, reference_kind, raw_target, resolved_package, is_resolved
            FROM source_dependency_references
            WHERE source_path = ?1 AND is_active = 1
            ORDER BY reference_index
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map([relative_path], |row| {
            Ok(SourceReference {
                reference_index: i64_to_u64(row.get::<_, i64>(0)?),
                reference_kind: row.get(1)?,
                raw_target: row.get(2)?,
                resolved_package: row.get(3)?,
                is_resolved: row.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_outbound_edges(
    connection: &Connection,
    relative_path: &str,
) -> Result<Vec<SourceEdge>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT source_path, source_package, target_package, reference_kind
            FROM source_dependency_edges
            WHERE source_path = ?1
            ORDER BY target_package, reference_kind
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map([relative_path], source_edge_from_row)
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn load_inbound_sources(
    connection: &Connection,
    relative_path: &str,
    package_path: &str,
) -> Result<Vec<SourceEdge>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT source_path, source_package, target_package, reference_kind
            FROM source_dependency_edges
            WHERE target_package = ?1 AND source_path != ?2
            ORDER BY source_path, reference_kind
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map(params![package_path, relative_path], source_edge_from_row)
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn source_edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceEdge> {
    Ok(SourceEdge {
        source_path: row.get(0)?,
        source_package: row.get(1)?,
        target_package: row.get(2)?,
        reference_kind: row.get(3)?,
    })
}

fn load_cochanges(
    connection: &Connection,
    relative_path: &str,
) -> Result<Vec<CochangePartner>, ExplainError> {
    let mut statement = connection
        .prepare(
            "
            SELECT path, co_change_count
            FROM (
                SELECT right_path AS path, co_change_count
                FROM git_cochanges
                WHERE left_path = ?1
                UNION ALL
                SELECT left_path AS path, co_change_count
                FROM git_cochanges
                WHERE right_path = ?1
            )
            ORDER BY co_change_count DESC, path ASC
            LIMIT ?2
            ",
        )
        .map_err(ExplainError::QueryDatabase)?;
    let rows = statement
        .query_map(params![relative_path, COCHANGE_LIMIT], |row| {
            Ok(CochangePartner {
                path: row.get(0)?,
                co_change_count: i64_to_u64(row.get::<_, i64>(1)?),
            })
        })
        .map_err(ExplainError::QueryDatabase)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(ExplainError::QueryDatabase)
}

fn score_unavailable_reasons(file: &FileFactsRow) -> Vec<ExplainLimitation> {
    let mut reasons = Vec::new();
    if file.language_id.as_deref() != Some("go") {
        reasons.push(ExplainLimitation {
            code: "unsupported_language".to_owned(),
            message: "Only active Go files currently receive Go file risk scores.".to_owned(),
        });
    }
    if file.is_generated || file.is_vendor {
        reasons.push(ExplainLimitation {
            code: "generated_or_vendor_excluded".to_owned(),
            message: "Generated and vendor Go files are excluded from file risk scoring."
                .to_owned(),
        });
    }
    if file.complexity_pressure.is_none() && file.max_function_complexity_pressure.is_none() {
        reasons.push(ExplainLimitation {
            code: "missing_parser_metrics".to_owned(),
            message: "Parser-derived Go complexity metrics are unavailable.".to_owned(),
        });
    }
    if reasons.is_empty() {
        reasons.push(ExplainLimitation {
            code: "missing_score_row".to_owned(),
            message: "No file risk score row exists for this indexed file.".to_owned(),
        });
    }
    reasons
}

fn git_limitations(metadata: &BTreeMap<String, String>) -> Vec<ExplainLimitation> {
    [
        ("git_diagnostic_message", "git_diagnostic"),
        ("git_merge_heavy_warning", "git_merge_heavy"),
        ("git_broad_commit_warning", "git_broad_commit"),
        (
            "git_author_concentration_warning",
            "git_author_concentration",
        ),
    ]
    .into_iter()
    .filter_map(|(key, code)| {
        metadata.get(key).map(|message| ExplainLimitation {
            code: code.to_owned(),
            message: message.clone(),
        })
    })
    .collect()
}

pub(crate) fn parse_diagnostics_json(value: &str) -> Vec<ExplainLimitation> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(value) else {
        return vec![ExplainLimitation {
            code: "invalid_diagnostics_json".to_owned(),
            message: "Stored parser diagnostics JSON could not be decoded.".to_owned(),
        }];
    };

    items
        .into_iter()
        .filter_map(|item| {
            Some(ExplainLimitation {
                code: item.get("code")?.as_str()?.to_owned(),
                message: item.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

fn find_index_root(current_dir: &Path) -> Option<PathBuf> {
    current_dir
        .ancestors()
        .find(|candidate| candidate.join(INDEX_DB).is_file())
        .map(Path::to_path_buf)
}

pub(crate) fn normalize_index_relative_path(
    index_root: &Path,
    current_dir: &Path,
    input_path: &Path,
) -> Result<String, ExplainError> {
    let root = canonical_or_lexical(index_root);
    let absolute = if input_path.is_absolute() {
        canonical_or_lexical(input_path)
    } else {
        normalize_lexical(&canonical_or_lexical(current_dir).join(input_path))
    };
    let relative = absolute
        .strip_prefix(&root)
        .map_err(|_| ExplainError::OutsideIndexRoot {
            path: input_path.to_path_buf(),
            index_root: index_root.to_path_buf(),
        })?;
    let normalized = path_to_index_string(relative);
    if normalized.is_empty() {
        return Err(ExplainError::OutsideIndexRoot {
            path: input_path.to_path_buf(),
            index_root: index_root.to_path_buf(),
        });
    }
    Ok(normalized)
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical(path))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn path_to_index_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn display_opt(value: Option<&str>) -> &str {
    value.unwrap_or("none")
}

fn display_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_owned())
}

fn display_f64(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "none".to_owned())
}

fn optional_i64_to_u64(value: Option<i64>) -> Option<u64> {
    value.map(i64_to_u64)
}

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_index_relative_path, parse_diagnostics_json, render_explain_text,
        CochangePartner, ExplainFact, ExplainFile, ExplainLimitation, ExplainOwner, ExplainReport,
        ExplainScore, ExplainTerm, GitContext, RawMetrics, SourceCoupling, SourceEdge,
        SourceReference,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn normalizes_relative_path_from_current_directory_to_index_root() {
        let root = Path::new("C:/repo");
        let current = Path::new("C:/repo/internal");

        let normalized =
            normalize_index_relative_path(root, current, Path::new("../cmd/app/main.go"))
                .expect("path should normalize");

        assert_eq!(normalized, "cmd/app/main.go");
    }

    #[test]
    fn rejects_paths_outside_index_root() {
        let error = normalize_index_relative_path(
            Path::new("C:/repo"),
            Path::new("C:/repo"),
            Path::new("../other/file.go"),
        )
        .expect_err("outside path should fail");

        assert!(error.to_string().contains("outside indexed repository"));
    }

    #[test]
    fn parses_diagnostics_json_in_order() {
        let diagnostics = parse_diagnostics_json(
            r#"[{"code":"parse_error","message":"bad syntax"},{"code":"truncated","message":"window"}]"#,
        );

        assert_eq!(diagnostics[0].code, "parse_error");
        assert_eq!(diagnostics[1].message, "window");
    }

    #[test]
    fn invalid_diagnostics_json_is_reported() {
        let diagnostics = parse_diagnostics_json("not json");

        assert_eq!(diagnostics[0].code, "invalid_diagnostics_json");
    }

    #[test]
    fn renders_text_with_required_sections() {
        let mut metadata = BTreeMap::new();
        metadata.insert("git_confidence".to_owned(), "bounded".to_owned());
        let report = ExplainReport {
            file: ExplainFile {
                relative_path: "src/risky.go".to_owned(),
                absolute_path: "C:/repo/src/risky.go".to_owned(),
                active_scan_id: 1,
                language_id: Some("go".to_owned()),
                content_kind: "text".to_owned(),
                extension: Some("go".to_owned()),
                is_generated: false,
                is_vendor: false,
                is_test: false,
                parser_status: "parsed".to_owned(),
                parser_recognition_attempts: 1,
            },
            score_unavailable_reasons: Vec::new(),
            score: Some(ExplainScore {
                formula_id: "hotpath.score.go.v1".to_owned(),
                rank: 1,
                score: 0.75,
                risk_10: 7.5,
                risk_band: "high".to_owned(),
            }),
            terms: vec![ExplainTerm {
                name: "churn".to_owned(),
                raw_value: Some(2000.0),
                normalized_value: Some(1.0),
                weight: 0.18,
                contribution: 0.18,
            }],
            raw_metrics: RawMetrics {
                byte_size: Some(100),
                mtime_ms: Some(1),
                line_count: Some(10),
                symbol_count: 1,
                function_count: 1,
                method_count: 0,
                type_count: 0,
                import_count: 1,
                complexity_pressure: Some(2),
                max_function_complexity_pressure: Some(2),
                commits_per_file: 2,
                total_added_lines: 10,
                total_deleted_lines: 3,
                total_churn_lines: 13,
                recent_added_lines: 8,
                recent_deleted_lines: 2,
                recent_churn_lines: 10,
                author_count: 1,
                first_touch_timestamp: Some(1),
                last_touch_timestamp: Some(2),
                file_age_days: Some(1),
                owner_count: Some(1),
                dominant_owner: Some("Alice <a@example.invalid>".to_owned()),
                dominant_owner_share: Some(1.0),
                co_changed_file_count: 1,
                source_coupling_pressure_in: Some(2),
                source_coupling_pressure_out: Some(1),
            },
            facts: vec![ExplainFact {
                kind: "summary".to_owned(),
                message: "summary fact".to_owned(),
            }],
            limitations: vec![ExplainLimitation {
                code: "approx".to_owned(),
                message: "approximate".to_owned(),
            }],
            parser_diagnostics: vec![ExplainLimitation {
                code: "parse_error".to_owned(),
                message: "syntax".to_owned(),
            }],
            owners: vec![ExplainOwner {
                rank: 1,
                author: "Alice <a@example.invalid>".to_owned(),
                ownership_score: 10.0,
                ownership_share: 1.0,
                touch_count: 2,
            }],
            git_context: GitContext {
                metadata,
                limitations: Vec::new(),
                cochanges: vec![CochangePartner {
                    path: "src/peer.go".to_owned(),
                    co_change_count: 1,
                }],
            },
            source_coupling: SourceCoupling {
                package_path: Some("src".to_owned()),
                inbound_count: Some(2),
                outbound_count: Some(1),
                outbound_references: vec![SourceReference {
                    reference_index: 0,
                    reference_kind: "import".to_owned(),
                    raw_target: "example.test/pkg".to_owned(),
                    resolved_package: Some("pkg".to_owned()),
                    is_resolved: true,
                }],
                outbound_edges: vec![SourceEdge {
                    source_path: "src/risky.go".to_owned(),
                    source_package: "src".to_owned(),
                    target_package: "pkg".to_owned(),
                    reference_kind: "import".to_owned(),
                }],
                inbound_sources: Vec::new(),
            },
        };

        let rendered = render_explain_text(&report);

        for section in [
            "File",
            "Score",
            "Terms",
            "Raw Metrics",
            "Facts",
            "Limitations",
            "Parser Diagnostics",
            "Owners",
            "Git Context",
            "Source Coupling",
        ] {
            assert!(rendered.contains(section), "missing {section}");
        }
        assert!(rendered.contains("churn raw 2000.000 normalized 1.000 weight 0.180"));
        assert!(rendered.contains("cochange src/peer.go count 1"));
    }
}
