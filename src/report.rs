// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::context::{ContextBudgetStatus, ContextGroupRow, ContextSkippedRow, ContextSummary};
use crate::git;
use crate::scoring::{
    FormulaVersion, NormalizedScoreMetrics, RawScoreMetrics, ScoreLimitation, WeightedTerm,
};
use crate::storage;
use crate::{estimate_context, ContextOptions, ContextReport, ScanError, ScanReport, ScanSummary};

pub const REPORT_SCHEMA_VERSION: &str = "hotpath.report.v1";
const SARIF_HOTSPOT_RULE_ID: &str = "hotpath.hotspot.risk";
const SARIF_ADVISORY_HELP_TEXT: &str = "Hotpath hotspot risk is advisory decision-support output based on local scan and Git history metrics. Review the file and contributing metrics before using the result as a gate.";

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

#[derive(Debug, Clone, PartialEq)]
pub struct CiRiskEvaluation {
    pub threshold: f64,
    pub threshold_breached: bool,
    pub highest_risk: Option<CiRiskHotspot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CiRiskHotspot {
    pub rank: u64,
    pub path: String,
    pub score: f64,
    pub risk: f64,
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
    CreateHtmlOutput(std::io::Error),
    WriteHtml(std::io::Error),
}

pub fn report_markdown() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    Ok(render_markdown(&report))
}

pub fn report_json() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    serde_json::to_string_pretty(&report).map_err(ReportCommandError::Json)
}

pub fn report_sarif() -> Result<String, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    render_sarif(&report)
}

pub fn report_html(output_dir: &Path) -> Result<String, ReportCommandError> {
    let current_dir = env::current_dir().map_err(ReportCommandError::CurrentDir)?;
    let output_index_path = absolute_path(&current_dir, &output_dir.join("index.html"));
    let report = build_report_and_persist(&current_dir, Some(&output_index_path))?;
    let html = render_html(&report);

    fs::create_dir_all(output_dir).map_err(ReportCommandError::CreateHtmlOutput)?;
    fs::write(output_dir.join("index.html"), html).map_err(ReportCommandError::WriteHtml)?;

    Ok("Wrote HTML report to index.html".to_owned())
}

pub fn ci_risk_gate(threshold: f64) -> Result<CiRiskEvaluation, ReportCommandError> {
    let report = build_current_dir_report_and_persist()?;

    Ok(evaluate_ci_risk(&report, threshold))
}

pub fn evaluate_ci_risk(report: &Report, threshold: f64) -> CiRiskEvaluation {
    let highest_risk = report.hotspots.first().map(|hotspot| CiRiskHotspot {
        rank: hotspot.rank,
        path: hotspot.path.clone(),
        score: hotspot.score,
        risk: risk_scale(hotspot.score),
    });
    let threshold_breached = highest_risk
        .as_ref()
        .is_some_and(|hotspot| hotspot.risk >= threshold);

    CiRiskEvaluation {
        threshold,
        threshold_breached,
        highest_risk,
    }
}

pub fn render_ci_risk(evaluation: &CiRiskEvaluation) -> String {
    let mut output = String::new();

    output.push_str("Hotpath CI risk\n");
    output.push_str(&format!(
        "result: {}\n",
        if evaluation.threshold_breached {
            "fail"
        } else {
            "pass"
        }
    ));
    output.push_str(&format!(
        "threshold: {}/10\n",
        format_risk_value(evaluation.threshold)
    ));

    if let Some(hotspot) = &evaluation.highest_risk {
        output.push_str(&format!(
            "max risk: {}/10\n",
            format_risk_value(hotspot.risk)
        ));
        output.push_str(&format!("highest-risk file: {}\n", hotspot.path));
    } else {
        output.push_str("max risk: none\n");
        output.push_str("highest-risk file: none\n");
    }

    output
}

pub fn render_markdown(report: &Report) -> String {
    let mut output = String::new();

    output.push_str("# Hotpath Report\n\n");
    render_markdown_summary(&mut output, report);
    render_markdown_hotspots(&mut output, report);
    render_markdown_context(&mut output, report);
    render_markdown_findings(&mut output, report);
    render_markdown_notes(&mut output, report);

    output
}

fn render_markdown_summary(output: &mut String, report: &Report) {
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
}

fn render_markdown_hotspots(output: &mut String, report: &Report) {
    output.push_str("## Top Hotspots\n\n");
    if report.hotspots.is_empty() {
        output.push_str("No current files were ranked as hotspots.\n\n");
        return;
    }

    output.push_str("| Rank | Path | Score | Risk /10 | Commits | Churn lines | Recent churn | Authors | Owners | Co-changed files |\n");
    output.push_str("| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for hotspot in report.hotspots.iter().take(10) {
        output.push_str(&format!(
            "| {} | {} | {:.3} | {:.1} | {} | {} | {} | {} | {} | {} |\n",
            hotspot.rank,
            markdown_code_span(&hotspot.path),
            hotspot.score,
            risk_scale(hotspot.score),
            optional_u64(hotspot.raw_metrics.commits_per_file),
            optional_u64(hotspot.raw_metrics.total_churn_lines),
            optional_u64(hotspot.raw_metrics.recent_churn_lines),
            optional_u64(hotspot.raw_metrics.author_count),
            optional_u64(hotspot.raw_metrics.owner_count),
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

fn render_markdown_context(output: &mut String, report: &Report) {
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
}

fn render_markdown_findings(output: &mut String, report: &Report) {
    output.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        output.push_str("No advisory findings were produced.\n\n");
        return;
    }

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

fn render_markdown_notes(output: &mut String, report: &Report) {
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
}

pub fn render_html(report: &Report) -> String {
    let mut output = String::new();

    output.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    output.push_str("<meta charset=\"utf-8\">\n");
    output.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    output.push_str("<title>Hotpath Report</title>\n");
    output.push_str("<style>\n");
    output.push_str(":root{color-scheme:light;font-family:Inter,Segoe UI,Arial,sans-serif;color:#1f2933;background:#f7f8fa;}\n");
    output.push_str("body{margin:0;padding:32px;}\n");
    output.push_str("main{max-width:1120px;margin:0 auto;}\n");
    output.push_str("h1{font-size:32px;margin:0 0 24px;}\n");
    output.push_str("h2{font-size:20px;margin:32px 0 12px;border-bottom:1px solid #d8dde4;padding-bottom:6px;}\n");
    output.push_str("p,li,td,th{font-size:14px;line-height:1.5;}\n");
    output.push_str("ul{padding-left:20px;}\n");
    output.push_str("table{width:100%;border-collapse:collapse;background:#fff;}\n");
    output.push_str(
        "th,td{border:1px solid #d8dde4;padding:8px;text-align:left;vertical-align:top;}\n",
    );
    output.push_str("th{text-align:left;background:#eef1f4;font-weight:600;}\n");
    output.push_str("td.numeric,th.numeric{text-align:right;font-variant-numeric:tabular-nums;}\n");
    output.push_str(".note{color:#52606d;}\n");
    output.push_str("</style>\n</head>\n<body>\n<main>\n");
    output.push_str("<h1>Hotpath Report</h1>\n");
    render_html_summary(&mut output, report);
    render_html_hotspots(&mut output, report);
    render_html_context(&mut output, report);
    render_html_findings(&mut output, report);
    render_html_notes(&mut output, report);
    output.push_str("</main>\n</body>\n</html>\n");

    output
}

fn render_html_summary(output: &mut String, report: &Report) {
    output.push_str("<h2>Summary</h2>\n<ul>\n");
    output.push_str(&format!(
        "<li>Files scanned: {}</li>\n",
        report.summary.scan.total_files
    ));
    output.push_str(&format!(
        "<li>Text files: {}, binary files: {}, unknown files: {}</li>\n",
        report.summary.scan.content.text_files,
        report.summary.scan.content.binary_files,
        report.summary.scan.content.unknown_files
    ));
    output.push_str(&format!(
        "<li>Generated files: {}, vendor files: {}</li>\n",
        report.summary.scan.flags.generated_files, report.summary.scan.flags.vendor_files
    ));
    output.push_str(&format!(
        "<li>Scan warnings: {}</li>\n",
        report.summary.scan.warnings.total_warnings
    ));
    output.push_str(&format!(
        "<li>Hotspots ranked: {}</li>\n",
        report.summary.hotspot_count
    ));
    output.push_str(&format!(
        "<li>Context estimate: {} tokens across {} included files</li>\n",
        report.summary.context_estimated_tokens, report.context.summary.included_files
    ));
    output.push_str(&format!(
        "<li>Git HEAD: {}; metrics for {} files; {} co-change pairs; recent window {} days</li>\n",
        html_escape(&report.summary.git.head_commit_id),
        report.summary.git.file_metric_count,
        report.summary.git.co_change_count,
        report.summary.git.recent_window_days
    ));
    output.push_str("</ul>\n");
}

fn render_html_hotspots(output: &mut String, report: &Report) {
    output.push_str("<h2>Top Hotspots</h2>\n");
    if report.hotspots.is_empty() {
        output.push_str("<p>No current files were ranked as hotspots.</p>\n");
        return;
    }

    output.push_str("<table>\n<thead><tr><th class=\"numeric\">Rank</th><th>Path</th><th class=\"numeric\">Score</th><th class=\"numeric\">Risk /10</th><th class=\"numeric\">Commits</th><th class=\"numeric\">Churn lines</th><th class=\"numeric\">Recent churn</th><th class=\"numeric\">Authors</th><th class=\"numeric\">Owners</th><th class=\"numeric\">Co-changed files</th></tr></thead>\n<tbody>\n");
    for hotspot in report.hotspots.iter().take(10) {
        output.push_str(&format!(
            "<tr><td class=\"numeric\">{}</td><td>{}</td><td class=\"numeric\">{:.3}</td><td class=\"numeric\">{:.1}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td></tr>\n",
            hotspot.rank,
            html_escape(&hotspot.path),
            hotspot.score,
            risk_scale(hotspot.score),
            optional_u64(hotspot.raw_metrics.commits_per_file),
            optional_u64(hotspot.raw_metrics.total_churn_lines),
            optional_u64(hotspot.raw_metrics.recent_churn_lines),
            optional_u64(hotspot.raw_metrics.author_count),
            optional_u64(hotspot.raw_metrics.owner_count),
            optional_u64(hotspot.raw_metrics.co_changed_file_count)
        ));
    }
    output.push_str("</tbody>\n</table>\n");
    if report.hotspots.len() > 10 {
        output.push_str(&format!(
            "<p class=\"note\">Showing top 10 of {} ranked hotspots. JSON output includes all hotspot rows.</p>\n",
            report.hotspots.len()
        ));
    }
}

fn render_html_context(output: &mut String, report: &Report) {
    output.push_str("<h2>Context Estimate</h2>\n<ul>\n");
    output.push_str(&format!(
        "<li>Estimated tokens: {}</li>\n",
        report.context.summary.estimated_tokens
    ));
    output.push_str(&format!(
        "<li>Included files: {}; skipped files: {}; included bytes: {}</li>\n",
        report.context.summary.included_files,
        report.context.summary.skipped_files,
        report.context.summary.included_bytes
    ));
    output.push_str("</ul>\n");
    if report.context.groups.is_empty() {
        output.push_str("<p>Largest groups: none</p>\n");
    } else {
        output.push_str("<table>\n<thead><tr><th>Group</th><th class=\"numeric\">Tokens</th><th class=\"numeric\">Files</th></tr></thead>\n<tbody>\n");
        for group in report.context.groups.iter().take(5) {
            output.push_str(&format!(
                "<tr><td>{}</td><td class=\"numeric\">{}</td><td class=\"numeric\">{}</td></tr>\n",
                html_escape(&group.path),
                group.estimated_tokens,
                group.file_count
            ));
        }
        output.push_str("</tbody>\n</table>\n");
    }
}

fn render_html_findings(output: &mut String, report: &Report) {
    output.push_str("<h2>Findings</h2>\n");
    if report.findings.is_empty() {
        output.push_str("<p>No advisory findings were produced.</p>\n");
        return;
    }

    output.push_str("<ul>\n");
    for finding in report.findings.iter().take(10) {
        output.push_str(&format!(
            "<li>{}: {}</li>\n",
            html_escape(finding.code),
            html_escape(&finding.message)
        ));
    }
    if report.findings.len() > 10 {
        output.push_str(&format!(
            "<li>{} additional findings are available in JSON output.</li>\n",
            report.findings.len() - 10
        ));
    }
    output.push_str("</ul>\n");
}

fn render_html_notes(output: &mut String, report: &Report) {
    output.push_str("<h2>Calculation Notes</h2>\n<ul>\n");
    output.push_str("<li>Hotpath runs locally and does not require network access or telemetry for this report.</li>\n");
    output.push_str(
        "<li>Hotspot scores are advisory decision-support signals, not proof of defects.</li>\n",
    );
    output.push_str(
        "<li>Risk /10 is the internal 0.0-1.0 score multiplied by 10 for human reading.</li>\n",
    );
    if let Some(hotspot) = report.hotspots.first() {
        output.push_str(&format!(
            "<li>Formula version: {}.</li>\n",
            html_escape(&hotspot.formula_version.id)
        ));
    }
    output.push_str("<li>Scores use the reported formula version and available local scan and Git history metrics.</li>\n");
    output.push_str("<li>Missing or incomplete local history can limit churn, ownership, and co-change signals.</li>\n");
    output.push_str("</ul>\n");
}

pub fn render_sarif(report: &Report) -> Result<String, ReportCommandError> {
    serde_json::to_string_pretty(&sarif_value(report)).map_err(ReportCommandError::Json)
}

fn sarif_value(report: &Report) -> Value {
    let results = report
        .hotspots
        .iter()
        .map(sarif_result_for_hotspot)
        .collect::<Vec<_>>();

    json!({
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "Hotpath",
                        "rules": [
                            {
                                "id": SARIF_HOTSPOT_RULE_ID,
                                "name": "Advisory hotspot risk",
                                "shortDescription": {
                                    "text": "Hotpath ranked a current repository file as a hotspot."
                                },
                                "fullDescription": {
                                    "text": "Hotpath ranks current files using local scan, churn, ownership, recent-growth, and co-change metrics."
                                },
                                "help": {
                                    "text": SARIF_ADVISORY_HELP_TEXT
                                },
                                "properties": {
                                    "schemaVersion": report.schema_version
                                }
                            }
                        ]
                    }
                },
                "results": results
            }
        ]
    })
}

fn sarif_result_for_hotspot(hotspot: &ReportHotspot) -> Value {
    let risk = risk_scale(hotspot.score);

    json!({
        "ruleId": SARIF_HOTSPOT_RULE_ID,
        "ruleIndex": 0,
        "level": sarif_level(risk),
        "message": {
            "text": format!(
                "Hotpath ranked {} as hotspot #{} with advisory score {:.3} ({:.1}/10) using formula {}. {}",
                hotspot.path,
                hotspot.rank,
                hotspot.score,
                risk,
                hotspot.formula_version.id,
                SARIF_ADVISORY_HELP_TEXT
            )
        },
        "locations": [
            {
                "physicalLocation": {
                    "artifactLocation": {
                        "uri": sarif_artifact_uri(&hotspot.path)
                    }
                }
            }
        ],
        "properties": {
            "rank": hotspot.rank,
            "score": hotspot.score,
            "risk": risk,
            "formulaVersion": hotspot.formula_version.id,
            "advisory": SARIF_ADVISORY_HELP_TEXT
        }
    })
}

fn sarif_level(risk: f64) -> &'static str {
    if risk >= 8.0 {
        "error"
    } else if risk >= 5.0 {
        "warning"
    } else {
        "note"
    }
}

fn sarif_artifact_uri(path: &str) -> String {
    let mut uri = String::with_capacity(path.len());

    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }

    uri
}

pub fn build_current_dir_report_and_persist() -> Result<Report, ReportCommandError> {
    let current_dir = env::current_dir().map_err(ReportCommandError::CurrentDir)?;

    build_report_and_persist(&current_dir, None)
}

fn build_report_and_persist(
    current_dir: &Path,
    excluded_report_path: Option<&Path>,
) -> Result<Report, ReportCommandError> {
    let analysis = crate::analyze_git_cached_at(current_dir)?;
    let excluded_report_path = excluded_report_path
        .and_then(|path| repository_relative_path(&analysis.worktree_root, path));
    let mut scan = crate::scan_repository(&analysis.worktree_root)?;

    if let Some(excluded_report_path) = excluded_report_path.as_deref() {
        scan.files.retain(|file| file.path != excluded_report_path);
    }

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

    let ranked = crate::ranked_hotspot_scores_from_scan_and_git(&scan.files, &analysis);
    index
        .persist_hotspots(scan_run.id, &ranked)
        .map_err(ReportCommandError::PersistHotspots)?;

    let context = estimate_context(&scan.files, ContextOptions::default());
    let hotspots = ranked.iter().map(ReportHotspot::from).collect::<Vec<_>>();
    let findings = hotspots.iter().map(ReportFinding::from).collect::<Vec<_>>();

    Ok(report_from_scan_analysis(
        &scan, &analysis, context, hotspots, findings,
    ))
}

pub(crate) fn report_from_scan_analysis(
    scan: &ScanReport,
    analysis: &git::GitAnalysis,
    context: ContextReport,
    hotspots: Vec<ReportHotspot>,
    findings: Vec<ReportFinding>,
) -> Report {
    let context_estimated_tokens = context.summary.estimated_tokens;

    Report {
        schema_version: REPORT_SCHEMA_VERSION,
        summary: ReportSummary {
            scan: scan.summary(),
            git: ReportGitSummary {
                head_commit_id: analysis.head_commit_id.clone(),
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
    }
}

fn absolute_path(current_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        lexically_normalize(path)
    } else {
        lexically_normalize(&current_dir.join(path))
    }
}

fn repository_relative_path(root: &Path, path: &Path) -> Option<String> {
    let root = lexically_normalize(root);
    let path = lexically_normalize(path);
    let relative = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();

    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    Some(parts.join("/"))
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    normalized
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

fn format_risk_value(value: f64) -> String {
    format!("{value:.3}")
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

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(character),
        }
    }

    escaped
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
            Self::CreateHtmlOutput(source) => {
                write!(f, "failed to create HTML report output directory: {source}")
            }
            Self::WriteHtml(source) => write!(f, "failed to write HTML report: {source}"),
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
            Self::CreateHtmlOutput(source) | Self::WriteHtml(source) => Some(source),
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
    git::write_git_history_error(f, source, git::GitHistoryUsage::Report)
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
