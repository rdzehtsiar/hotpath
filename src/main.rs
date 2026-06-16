// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::styling;
use clap::{Args, Parser, Subcommand};

const CLI_STYLES: styling::Styles = styling::Styles::styled()
    .header(styling::Style::new().bold())
    .usage(styling::Style::new().bold());
use hotpath::pipeline::events::PipelineState;
use hotpath::pipeline::reporter::StdioReporter;
use serde::Serialize;

const SCAN_JSON_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Parser)]
#[command(name = "hotpath")]
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
    /// Include raw Git collection and local index details in text output.
    #[arg(long)]
    verbose: bool,
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
    let cli = Cli::parse();

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

    let engine = hotpath::pipeline::analysis_engine::AnalysisEngine::new(&root);
    if args.json {
        let outcome = match engine.scan_with_state() {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("hotpath: {error}");
                return ExitCode::FAILURE;
            }
        };

        match serde_json::to_string(&ScanJsonOutput::from_state(&outcome.state)) {
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

    let mut reporter = StdioReporter::stdout().with_final_git_status(args.verbose);
    if let Err(error) = engine.scan_with_reporter(&mut reporter) {
        eprintln!("hotpath: {error}");
        return ExitCode::FAILURE;
    }

    match hotpath::pipeline::scan_summary::load_scan_summary(&root) {
        Ok(summary) => {
            println!(
                "{}",
                hotpath::pipeline::scan_summary::render_scan_summary_with_options(
                    &summary,
                    args.verbose
                )
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
    command: &'static str,
    files: ScanJsonFiles,
    git: ScanJsonGit,
    index: ScanJsonIndex,
}

#[derive(Debug, Serialize)]
struct ScanJsonFiles {
    detected: u64,
    analyzed: u64,
}

#[derive(Debug, Serialize)]
struct ScanJsonGit {
    skipped: bool,
    mode: Option<String>,
    confidence: Option<String>,
    commits_total: Option<u64>,
    commits_processed: u64,
    diagnostic: Option<String>,
    index_action: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScanJsonIndex {
    records_stored: u64,
}

impl ScanJsonOutput {
    fn from_state(state: &PipelineState) -> Self {
        Self {
            schema_version: SCAN_JSON_SCHEMA_VERSION,
            command: "scan",
            files: ScanJsonFiles {
                detected: state.total_files.unwrap_or(state.enumerated_files),
                analyzed: state.analyzed_files,
            },
            git: ScanJsonGit {
                skipped: state.git_skipped,
                mode: state.git_status.mode.clone(),
                confidence: state.git_status.confidence.clone(),
                commits_total: state.total_git_commits,
                commits_processed: state.git_commits_processed,
                diagnostic: state.git_status.diagnostic.as_deref().map(diagnostic_code),
                index_action: state.git_status.index_action.clone(),
            },
            index: ScanJsonIndex {
                records_stored: state.stored_records,
            },
        }
    }
}

fn diagnostic_code(diagnostic: &str) -> String {
    diagnostic
        .split_once(':')
        .map(|(code, _)| code)
        .unwrap_or(diagnostic)
        .to_owned()
}
