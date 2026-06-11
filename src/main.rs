// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hotpath::pipeline::reporter::StdioReporter;

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
    Scan,
    /// Open the read-only Hotpath terminal UI for the current index.
    Tui,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan => run_scan(),
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

fn run_scan() -> ExitCode {
    let mut reporter = StdioReporter::stdout();
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
