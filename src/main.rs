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
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan => run_scan(),
    }
}

fn run_scan() -> ExitCode {
    let mut reporter = StdioReporter::stdout();
    let result = env::current_dir()
        .map_err(hotpath::pipeline::enumerator::EnumerationError::CurrentDir)
        .map_err(hotpath::pipeline::analysis_engine::AnalysisEngineError::Enumeration)
        .and_then(|root| {
            let engine = hotpath::pipeline::analysis_engine::AnalysisEngine::new(root);
            engine.scan_with_reporter(&mut reporter)
        });

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hotpath: {error}");
            ExitCode::FAILURE
        }
    }
}
