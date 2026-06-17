// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::builder::styling;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

const CLI_STYLES: styling::Styles = styling::Styles::styled()
    .header(styling::Style::new().bold())
    .usage(styling::Style::new().bold());
use hotpath::pipeline::events::PipelineState;
use hotpath::pipeline::reporter::StdioReporter;
use serde::Serialize;

const SCAN_JSON_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Parser)]
#[command(name = "hotpath")]
#[command(version)]
#[command(disable_version_flag = true)]
#[command(override_usage = "hotpath <COMMAND> [OPTIONS]")]
#[command(about = "Find risky files in your Go codebase using local Git and parser signals.")]
#[command(styles = CLI_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Analyze files and Git history; reruns are incremental.
    Scan(ScanArgs),
    /// Show why a specific file scored as a hotspot.
    Explain(ExplainArgs),
    /// List the top Go file hotspots by risk score.
    Hotspots(HotspotsArgs),
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Write a stable JSON scan summary instead of terminal progress.
    #[arg(long)]
    json: bool,
    /// Force a complete scan, ignoring any prior results.
    #[arg(long)]
    full: bool,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    /// Repository-relative path, or an absolute path under the indexed repository root.
    path: PathBuf,
}

#[derive(Debug, Args)]
struct HotspotsArgs {
    /// Include Go test-file hotspots alongside production Go hotspots.
    #[arg(long)]
    include_tests: bool,
    /// Write a JSON hotspot summary to stdout instead of a text table.
    #[arg(long)]
    json: bool,
    /// Include scores, confidence, and driver details in text output.
    #[arg(long)]
    verbose: bool,
    /// Show top N hotspots (default: 5).
    #[arg(long, conflicts_with = "all")]
    top: Option<usize>,
    /// Show all scored hotspots without a limit.
    #[arg(long, conflicts_with = "top")]
    all: bool,
}

fn main() -> ExitCode {
    let matches = Cli::command()
        .arg(
            clap::Arg::new("version")
                .short('v')
                .long("version")
                .action(clap::ArgAction::Version)
                .help("Print version"),
        )
        .get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };

    match cli.command {
        Commands::Scan(args) => run_scan(args),
        Commands::Explain(args) => run_explain(args),
        Commands::Hotspots(args) => run_hotspots(args),
    }
}

fn run_scan(args: ScanArgs) -> ExitCode {
    let root = match env::current_dir()
        .map_err(hotpath::pipeline::enumerator::EnumerationError::CurrentDir)
        .map_err(hotpath::pipeline::analysis_engine::AnalysisEngineError::Enumeration)
    {
        Ok(root) => root,
        Err(error) => {
            eprintln!("hotpath: {error}");
            return ExitCode::FAILURE;
        }
    };

    if args.full {
        let index_path = root.join(".hotpath").join("index.sqlite");
        if let Err(e) = std::fs::remove_file(&index_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("hotpath: failed to remove index: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let engine = hotpath::pipeline::analysis_engine::AnalysisEngine::new(&root);
    if args.json {
        let outcome = match engine.scan_with_state() {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("hotpath: {error}");
                return ExitCode::FAILURE;
            }
        };

        let summary = match hotpath::pipeline::scan_summary::load_scan_summary(&root) {
            Ok(summary) => Some(summary),
            Err(error) => {
                eprintln!("hotpath: {error}");
                return ExitCode::FAILURE;
            }
        };

        let band_counts = match hotpath::pipeline::scan_summary::load_band_counts(&root) {
            Ok(counts) => counts,
            Err(error) => {
                eprintln!("hotpath: {error}");
                return ExitCode::FAILURE;
            }
        };

        let output = ScanJsonOutput::build(&outcome.state, summary.as_ref(), &band_counts);

        match serde_json::to_string(&output) {
            Ok(json) => {
                println!("{json}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("hotpath: failed to serialize scan JSON: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut reporter = StdioReporter::stdout();
    if let Err(error) = engine.scan_with_reporter(&mut reporter) {
        eprintln!("hotpath: {error}");
        return ExitCode::FAILURE;
    }

    match hotpath::pipeline::scan_summary::load_scan_summary(&root) {
        Ok(summary) => {
            println!(
                "{}",
                hotpath::pipeline::scan_summary::render_scan_summary(&summary)
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hotpath: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_explain(args: ExplainArgs) -> ExitCode {
    let current_dir = match env::current_dir() {
        Ok(current_dir) => current_dir,
        Err(error) => {
            eprintln!("hotpath: failed to read current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let report = match hotpath::explain::load_explain_report(&current_dir, &args.path) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("hotpath: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}", hotpath::explain::render_explain_text(&report));
    ExitCode::SUCCESS
}

fn run_hotspots(args: HotspotsArgs) -> ExitCode {
    let current_dir = match env::current_dir() {
        Ok(current_dir) => current_dir,
        Err(error) => {
            eprintln!("hotpath: failed to read current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let limit = if args.all {
        None
    } else {
        Some(args.top.unwrap_or(hotpath::hotspots::DEFAULT_HOTSPOT_LIMIT))
    };
    let report =
        match hotpath::hotspots::load_hotspots_report(&current_dir, args.include_tests, limit) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("hotpath: {error}");
                return ExitCode::FAILURE;
            }
        };

    if args.json {
        println!("{}", hotpath::hotspots::render_hotspots_json(&report));
    } else {
        println!(
            "{}",
            hotpath::hotspots::render_hotspots_table(&report, args.verbose)
        );
    }
    ExitCode::SUCCESS
}

#[derive(Debug, Serialize)]
struct ScanJsonOutput {
    schema_version: u64,
    hotpath_version: String,
    scanned_at: String,
    assessment_reliable: bool,
    scoring_confidence: String,
    risk: ScanJsonRisk,
    scan: ScanJsonScanInfo,
    top_hotspots: Vec<ScanJsonHotspot>,
    limitations: Vec<ScanJsonLimitation>,
}

#[derive(Debug, Serialize)]
struct ScanJsonRisk {
    score: Option<f64>,
    band: String,
    primary_driver: Option<ScanJsonPrimaryDriver>,
    files_by_band: ScanJsonFilesByBand,
}

#[derive(Debug, Serialize)]
struct ScanJsonPrimaryDriver {
    id: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct ScanJsonFilesByBand {
    extreme: u64,
    high: u64,
    medium: u64,
    low: u64,
}

#[derive(Debug, Serialize)]
struct ScanJsonScanInfo {
    #[serde(rename = "type")]
    scan_type: String,
    duration_ms: u64,
    files_detected: u64,
    files_analyzed: u64,
    git_history: String,
    commits_processed: u64,
    commits_total: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ScanJsonHotspot {
    rank: u64,
    path: String,
    score: f64,
    band: String,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScanJsonLimitation {
    code: String,
    message: String,
}

impl ScanJsonOutput {
    fn build(
        state: &PipelineState,
        summary: Option<&hotpath::pipeline::scan_summary::ScanSummary>,
        band_counts: &hotpath::pipeline::scan_summary::BandCounts,
    ) -> Self {
        let scoring_confidence = summary
            .and_then(|s| s.project.as_ref())
            .map(|p| p.confidence.as_str())
            .unwrap_or("none")
            .to_owned();

        let git_history = derive_git_history(state).to_owned();
        let assessment_reliable =
            matches!(scoring_confidence.as_str(), "high" | "medium") && git_history != "absent";

        let project = summary.and_then(|s| s.project.as_ref());

        let (risk_score, risk_band) = match project {
            Some(p) if scoring_confidence != "none" => (Some(p.risk_10), p.risk_band.clone()),
            _ => (None, "unavailable".to_owned()),
        };

        let primary_driver =
            project.and_then(|p| map_primary_driver(p.dominant_dimension.as_deref()));

        let risk = ScanJsonRisk {
            score: risk_score,
            band: risk_band,
            primary_driver,
            files_by_band: ScanJsonFilesByBand {
                extreme: band_counts.extreme,
                high: band_counts.high,
                medium: band_counts.medium,
                low: band_counts.low,
            },
        };

        let scan = ScanJsonScanInfo {
            scan_type: derive_scan_type(state).to_owned(),
            duration_ms: state.total_elapsed.as_millis() as u64,
            files_detected: state.total_files.unwrap_or(state.enumerated_files),
            files_analyzed: state.analyzed_files,
            git_history,
            commits_processed: state.git_commits_processed,
            commits_total: state.total_git_commits,
        };

        let top_hotspots = summary
            .map(|s| {
                s.hotspots
                    .iter()
                    .map(|h| ScanJsonHotspot {
                        rank: h.rank,
                        path: h.relative_path.clone(),
                        score: h.risk_10,
                        band: h.risk_band.clone(),
                        reason: h.fact.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let limitations = summary
            .map(|s| {
                s.limitations
                    .iter()
                    .map(|l| ScanJsonLimitation {
                        code: l.code.clone(),
                        message: normalize_json_limitation_message(&l.message),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let scanned_at = format_utc_iso8601(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );

        Self {
            schema_version: SCAN_JSON_SCHEMA_VERSION,
            hotpath_version: env!("CARGO_PKG_VERSION").to_owned(),
            scanned_at,
            assessment_reliable,
            scoring_confidence,
            risk,
            scan,
            top_hotspots,
            limitations,
        }
    }
}

fn normalize_json_limitation_message(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return "Limitation details are unavailable.".to_owned();
    }

    let mut normalized = String::new();
    let mut capitalized = false;
    for character in trimmed.chars() {
        if !capitalized && character.is_ascii_alphabetic() {
            normalized.push(character.to_ascii_uppercase());
            capitalized = true;
        } else {
            normalized.push(character);
        }
    }

    while normalized.ends_with(['.', '!', '?']) {
        normalized.pop();
    }
    normalized.push('.');
    normalized
}

fn format_utc_iso8601(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let time_of_day = epoch_secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z",
        year = y,
        month = m,
        day = d,
        hours = hours,
        minutes = minutes,
        seconds = seconds,
    )
}

fn derive_git_history(state: &PipelineState) -> &'static str {
    if state.git_skipped {
        return "absent";
    }
    match state.git_status.confidence.as_deref() {
        Some("full") | Some("up_to_date") => "full",
        Some("bounded") => "bounded",
        _ => "absent",
    }
}

fn derive_scan_type(state: &PipelineState) -> &'static str {
    match state.git_status.index_action.as_deref() {
        Some("reused") | Some("incrementally_updated") => "incremental",
        _ => "full",
    }
}

fn map_primary_driver(dominant_dimension: Option<&str>) -> Option<ScanJsonPrimaryDriver> {
    let dimension = dominant_dimension?;
    let (id, label) = match dimension {
        "churn" => ("churn", "Churn"),
        "recent_churn" => ("churn", "Recent churn"),
        "complexity_pressure" => ("complexity", "Complexity"),
        "cochange_pressure" => ("cochange", "Co-change"),
        _ => return None,
    };
    Some(ScanJsonPrimaryDriver {
        id: id.to_owned(),
        label: label.to_owned(),
    })
}
