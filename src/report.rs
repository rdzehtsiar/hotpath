// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::error::Error as StdError;
use std::fmt;

use serde::Serialize;

use crate::context::{ContextBudgetStatus, ContextGroupRow, ContextSkippedRow, ContextSummary};
use crate::git;
use crate::scoring::{
    FormulaVersion, NormalizedScoreMetrics, RawScoreMetrics, ScoreLimitation, WeightedTerm,
};
use crate::storage;
use crate::{estimate_context, ContextOptions, ScanError, ScanSummary};

pub const REPORT_SCHEMA_VERSION: &str = "hotpath.report.v1";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub schema_version: &'static str,
    pub summary: ReportSummary,
    pub hotspots: Vec<ReportHotspot>,
    pub context: ReportContext,
    pub findings: Vec<ReportFinding>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportSummary {
    pub scan: ScanSummary,
    pub git: ReportGitSummary,
    pub hotspot_count: u64,
    pub context_estimated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportGitSummary {
    pub head_commit_id: String,
    pub recent_window_days: u64,
    pub file_metric_count: u64,
    pub co_change_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportHotspot {
    pub rank: u64,
    pub path: String,
    pub score: f64,
    pub formula_version: FormulaVersion,
    pub raw_metrics: RawScoreMetrics,
    pub normalized_metrics: NormalizedScoreMetrics,
    pub weighted_terms: Vec<WeightedTerm>,
    pub limitations: Vec<ScoreLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportContext {
    pub options: ContextOptions,
    pub summary: ContextSummary,
    pub groups: Vec<ContextGroupRow>,
    pub skipped: Vec<ContextSkippedRow>,
    pub budget: Option<ContextBudgetStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportFinding {
    pub code: &'static str,
    pub level: ReportFindingLevel,
    pub path: Option<String>,
    pub message: String,
    pub rank: Option<u64>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFindingLevel {
    Info,
}

#[derive(Debug)]
pub enum ReportCommandError {
    CurrentDir(std::io::Error),
    Git(git::GitHistoryError),
    Scan(ScanError),
    PersistScan(storage::index::IndexError),
    PersistGitAnalysis(storage::index::IndexError),
    PersistHotspots(storage::index::IndexError),
    Json(serde_json::Error),
}

pub fn report_markdown() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    Ok(render_markdown(&report))
}

pub fn report_json() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    serde_json::to_string_pretty(&report).map_err(ReportCommandError::Json)
}

pub fn render_markdown(report: &Report) -> String {
    let mut output = String::new();

    output.push_str("# Hotpath Report\n\n");
    output.push_str("## Summary\n\n");
    output.push_str(&format!(
        "- Files scanned: {}\n",
        report.summary.scan.total_files
    ));
    output.push_str(&format!(
        "- Text files: {}, binary files: {}, unknown files: {}\n",
        report.summary.scan.content.text_files,
        report.summary.scan.content.binary_files,
        report.summary.scan.content.unknown_files
    ));
    output.push_str(&format!(
        "- Generated files: {}, vendor files: {}\n",
        report.summary.scan.flags.generated_files, report.summary.scan.flags.vendor_files
    ));
    output.push_str(&format!(
        "- Scan warnings: {}\n",
        report.summary.scan.warnings.total_warnings
    ));
    output.push_str(&format!(
        "- Hotspots ranked: {}\n",
        report.summary.hotspot_count
    ));
    output.push_str(&format!(
        "- Context estimate: {} tokens across {} included files\n",
        report.summary.context_estimated_tokens, report.context.summary.included_files
    ));
    output.push_str(&format!(
        "- Git HEAD: `{}`; metrics for {} files; {} co-change pairs; recent window {} days\n\n",
        report.summary.git.head_commit_id,
        report.summary.git.file_metric_count,
        report.summary.git.co_change_count,
        report.summary.git.recent_window_days
    ));

    output.push_str("## Top Hotspots\n\n");
    if report.hotspots.is_empty() {
        output.push_str("No current files were ranked as hotspots.\n\n");
    } else {
        output.push_str("| Rank | Path | Score | Risk /10 | Commits | Churn lines | Recent churn | Authors | Co-changed files |\n");
        output.push_str("| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for hotspot in report.hotspots.iter().take(10) {
            output.push_str(&format!(
                "| {} | {} | {:.3} | {:.1} | {} | {} | {} | {} | {} |\n",
                hotspot.rank,
                markdown_code_span(&hotspot.path),
                hotspot.score,
                risk_scale(hotspot.score),
                optional_u64(hotspot.raw_metrics.commits_per_file),
                optional_u64(hotspot.raw_metrics.total_churn_lines),
                optional_u64(hotspot.raw_metrics.recent_churn_lines),
                optional_u64(hotspot.raw_metrics.author_count),
                optional_u64(hotspot.raw_metrics.co_changed_file_count)
            ));
        }
        if report.hotspots.len() > 10 {
            output.push_str(&format!(
                "\nShowing top 10 of {} ranked hotspots. JSON output includes all hotspot rows.\n\n",
                report.hotspots.len()
            ));
        } else {
            output.push('\n');
        }
    }

    output.push_str("## Context Estimate\n\n");
    output.push_str(&format!(
        "- Estimated tokens: {}\n",
        report.context.summary.estimated_tokens
    ));
    output.push_str(&format!(
        "- Included files: {}; skipped files: {}; included bytes: {}\n",
        report.context.summary.included_files,
        report.context.summary.skipped_files,
        report.context.summary.included_bytes
    ));
    if report.context.groups.is_empty() {
        output.push_str("- Largest groups: none\n\n");
    } else {
        output.push_str("- Largest groups:");
        for group in report.context.groups.iter().take(5) {
            output.push_str(&format!(
                " {} ({} tokens, {} files);",
                markdown_code_span(&group.path),
                group.estimated_tokens,
                group.file_count
            ));
        }
        output.push_str("\n\n");
    }

    output.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        output.push_str("No advisory findings were produced.\n\n");
    } else {
        for finding in report.findings.iter().take(10) {
            output.push_str(&format!(
                "- `{}`: {}\n",
                finding.code,
                markdown_text(&finding.message)
            ));
        }
        if report.findings.len() > 10 {
            output.push_str(&format!(
                "- {} additional findings are available in JSON output.\n",
                report.findings.len() - 10
            ));
        }
        output.push('\n');
    }

    output.push_str("## Calculation Notes\n\n");
    output.push_str("- Hotpath runs locally and does not require network access or telemetry for this report.\n");
    output.push_str(
        "- Hotspot scores are advisory decision-support signals, not proof of defects.\n",
    );
    output
        .push_str("- Risk /10 is the internal 0.0-1.0 score multiplied by 10 for human reading.\n");
    if let Some(hotspot) = report.hotspots.first() {
        output.push_str(&format!(
            "- Formula version: {}.\n",
            markdown_code_span(&hotspot.formula_version.id)
        ));
    }
    output.push_str("- Scores use the reported formula version and available local scan and Git history metrics.\n");
    output.push_str("- Missing or incomplete local history can limit churn, ownership, and co-change signals.\n");

    output
}

pub fn build_current_dir_report_and_persist() -> Result<Report, ReportCommandError> {
    let current_dir = env::current_dir().map_err(ReportCommandError::CurrentDir)?;
    let analysis = git::analyze_from_head_at(&current_dir)?;
    let scan = crate::scan_repository(&analysis.worktree_root)?;
    let mut index = storage::index::IndexStore::open(&analysis.worktree_root)
        .map_err(ReportCommandError::PersistScan)?;

    let scan_run = index
        .persist_scan(&scan)
        .map_err(ReportCommandError::PersistScan)?;
    index
        .persist_git_analysis(
            &analysis.worktree_root,
            &analysis.head_commit_id,
            analysis.head_commit_time,
            analysis.recent_window_days as u64,
            &analysis.file_metrics,
            &analysis.co_changes,
        )
        .map_err(ReportCommandError::PersistGitAnalysis)?;

    let ranked = crate::ranked_hotspot_scores_from_scan_and_git(
        &scan.files,
        &analysis.file_metrics,
        &analysis.co_changes,
    );
    index
        .persist_hotspots(scan_run.id, &ranked)
        .map_err(ReportCommandError::PersistHotspots)?;

    let context = estimate_context(&scan.files, ContextOptions::default());
    let context_estimated_tokens = context.summary.estimated_tokens;
    let hotspots = ranked.iter().map(ReportHotspot::from).collect::<Vec<_>>();
    let findings = hotspots.iter().map(ReportFinding::from).collect::<Vec<_>>();

    Ok(Report {
        schema_version: REPORT_SCHEMA_VERSION,
        summary: ReportSummary {
            scan: scan.summary(),
            git: ReportGitSummary {
                head_commit_id: analysis.head_commit_id,
                recent_window_days: analysis.recent_window_days as u64,
                file_metric_count: analysis.file_metrics.len() as u64,
                co_change_count: analysis.co_changes.len() as u64,
            },
            hotspot_count: hotspots.len() as u64,
            context_estimated_tokens,
        },
        hotspots,
        context: ReportContext {
            options: context.options,
            summary: context.summary,
            groups: context.groups,
            skipped: context.skipped,
            budget: context.budget,
        },
        findings,
    })
}

impl From<&crate::scoring::RankedHotspotScore> for ReportHotspot {
    fn from(ranked: &crate::scoring::RankedHotspotScore) -> Self {
        Self {
            rank: ranked.rank,
            path: ranked.score.path.clone(),
            score: ranked.score.value,
            formula_version: ranked.score.formula_version.clone(),
            raw_metrics: ranked.score.raw_metrics.clone(),
            normalized_metrics: ranked.score.normalized_metrics.clone(),
            weighted_terms: ranked.score.weighted_terms.clone(),
            limitations: ranked.score.limitations.clone(),
        }
    }
}

impl From<&ReportHotspot> for ReportFinding {
    fn from(hotspot: &ReportHotspot) -> Self {
        Self {
            code: "hotpath.hotspot.risk",
            level: ReportFindingLevel::Info,
            path: Some(hotspot.path.clone()),
            message: format!(
                "Ranked hotspot #{} with advisory score {:.3}",
                hotspot.rank, hotspot.score
            ),
            rank: Some(hotspot.rank),
            score: Some(hotspot.score),
        }
    }
}

fn risk_scale(score: f64) -> f64 {
    score * 10.0
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| value.to_string())
}

fn markdown_code_span(value: &str) -> String {
    let value = value.replace('\n', " ").replace('|', "\\|");
    let fence = "`".repeat(max_backtick_run(&value) + 1);

    if value.contains('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn max_backtick_run(value: &str) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;

    for character in value.chars() {
        if character == '`' {
            current_run += 1;
            max_run = max_run.max(current_run);
        } else {
            current_run = 0;
        }
    }

    max_run
}

fn markdown_text(value: &str) -> String {
    value.replace('\n', " ")
}

impl fmt::Display for ReportCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDir(source) => {
                write!(f, "failed to determine the current directory: {source}")
            }
            Self::Git(source) => write_report_git_error(f, source),
            Self::Scan(source) => write_report_scan_error(f, source),
            Self::PersistScan(source) => {
                crate::write_persistence_error(f, "persist scan results", source, "report")
            }
            Self::PersistGitAnalysis(source) => {
                crate::write_persistence_error(f, "persist Git analysis", source, "report")
            }
            Self::PersistHotspots(source) => {
                crate::write_persistence_error(f, "persist hotspot scores", source, "report")
            }
            Self::Json(source) => write!(f, "failed to render report JSON: {source}"),
        }
    }
}

impl StdError for ReportCommandError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CurrentDir(source) => Some(source),
            Self::Git(source) => Some(source),
            Self::Scan(source) => Some(source),
            Self::PersistScan(source)
            | Self::PersistGitAnalysis(source)
            | Self::PersistHotspots(source) => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

impl From<git::GitHistoryError> for ReportCommandError {
    fn from(source: git::GitHistoryError) -> Self {
        Self::Git(source)
    }
}

impl From<ScanError> for ReportCommandError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

fn write_report_git_error(
    f: &mut fmt::Formatter<'_>,
    source: &git::GitHistoryError,
) -> fmt::Result {
    match source {
        git::GitHistoryError::NotRepository { .. } => write!(
            f,
            "path is not a readable Git worktree; run report from inside a repository with local history"
        ),
        git::GitHistoryError::OpenRepository { .. } => write!(
            f,
            "failed to open Git repository from the current worktree; ensure local Git metadata is readable"
        ),
        git::GitHistoryError::MissingHead { .. } => write!(
            f,
            "Git repository does not have a commit at HEAD; create an initial commit before generating a report"
        ),
        git::GitHistoryError::ShallowRepository { .. } => write!(
            f,
            "Git repository has shallow history; fetch complete local history before running report so metrics are not based on incomplete commits"
        ),
        git::GitHistoryError::BareRepository { .. } => write!(
            f,
            "Git repository has no worktree; report generation requires a local worktree"
        ),
        git::GitHistoryError::HeadNotCommit { source, .. } => {
            write!(f, "Git HEAD does not resolve to a commit: {source}")
        }
        git::GitHistoryError::Git { context, source } => {
            write!(f, "failed to traverse Git history while {context}: {source}")
        }
        git::GitHistoryError::UnsupportedAuthorIdentity { commit_id } => write!(
            f,
            "commit {commit_id} has an author name or email that is not valid UTF-8"
        ),
        git::GitHistoryError::UnsupportedPathEncoding { commit_id } => write!(
            f,
            "commit {commit_id} changed a path that is not valid UTF-8"
        ),
    }
}

fn write_report_scan_error(f: &mut fmt::Formatter<'_>, source: &ScanError) -> fmt::Result {
    match source {
        ScanError::Root { source, .. } => {
            write!(f, "failed to access report scan root: {source}")
        }
        ScanError::RootNotDirectory { .. } => write!(f, "report scan root is not a directory"),
        ScanError::RelativePath { .. } => write!(
            f,
            "failed to render a report file path relative to the scan root"
        ),
        ScanError::CurrentDir(source) => {
            write!(f, "failed to determine the current directory: {source}")
        }
        ScanError::Index(source) => {
            crate::write_persistence_error(f, "persist scan results", source, "report")
        }
        ScanError::PersistSymbols(source) => {
            crate::write_persistence_error(f, "persist parser symbols", source, "report")
        }
        ScanError::Json(source) => write!(f, "failed to render scan JSON: {source}"),
    }
}
