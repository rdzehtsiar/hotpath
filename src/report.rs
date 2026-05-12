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

pub fn report_json() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    serde_json::to_string_pretty(&report).map_err(ReportCommandError::Json)
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
