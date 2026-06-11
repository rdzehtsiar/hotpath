// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use hotpath::pipeline::events::PipelineState;
use hotpath::pipeline::reporter::StdioReporter;
use serde::Serialize;

const SCAN_JSON_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Parser)]
#[command(name = "hotpath")]
#[command(about = "Offline local-first codebase intelligence CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Enumerate repository files and print scan throughput.
    Scan(ScanArgs),
    /// Explain indexed metrics and score context for one file.
    Explain(ExplainArgs),
    /// Show ranked Go file hotspots from the latest complete local index.
    Hotspots,
    /// Open the read-only Hotpath terminal UI for the current index.
    Tui,
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Write a stable JSON scan summary instead of terminal progress.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    /// Repository-relative path, or an absolute path under the indexed repository root.
    path: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ExplainFormat::Text)]
    format: ExplainFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExplainFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan(args) => run_scan(args),
        Commands::Explain(args) => run_explain(args),
        Commands::Hotspots => run_hotspots(),
        Commands::Tui => run_tui(),
    }
}

fn run_tui() -> ExitCode {
    match hotpath::tui::run_tui() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hotpath: {error}");
            ExitCode::FAILURE
        }
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

    match args.format {
        ExplainFormat::Text => {
            println!("{}", hotpath::explain::render_explain_text(&report));
            ExitCode::SUCCESS
        }
        ExplainFormat::Json => match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("hotpath: failed to serialize explain JSON: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run_hotspots() -> ExitCode {
    let current_dir = match env::current_dir() {
        Ok(current_dir) => current_dir,
        Err(error) => {
            eprintln!("hotpath: failed to read current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let report = match hotpath::hotspots::load_hotspots_report(&current_dir) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("hotpath: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}", hotpath::hotspots::render_hotspots_table(&report));
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
                diagnostic: state.git_status.diagnostic.clone(),
                index_action: state.git_status.index_action.clone(),
            },
            index: ScanJsonIndex {
                records_stored: state.stored_records,
            },
        }
    }
}
