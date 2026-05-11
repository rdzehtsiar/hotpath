// SPDX-License-Identifier: Apache-2.0

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "hotpath")]
#[command(about = "Offline local-first codebase intelligence CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scan a repository and print an early placeholder report.
    Scan(ScanArgs),

    /// Explain local Git history metrics for one file.
    ExplainGit(ExplainGitArgs),

    /// Rank current files by advisory hotspot risk.
    Hotspots,

    /// Check the local Hotpath index health.
    Doctor,
}

#[derive(Debug, Args)]
struct ExplainGitArgs {
    /// Repository-relative or worktree-relative file path to explain.
    path: std::path::PathBuf,
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Print a human-readable scan summary.
    #[arg(long, conflicts_with = "json")]
    summary: bool,

    /// Print a machine-readable JSON scan summary.
    #[arg(long, conflicts_with = "summary")]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: Result<String, Box<dyn std::error::Error>> = match cli.command {
        Commands::Scan(args) if args.json => hotpath::scan_json().map_err(Into::into),
        Commands::Scan(args) if args.summary => hotpath::scan_summary().map_err(Into::into),
        Commands::Scan(_) => hotpath::scan_summary().map_err(Into::into),
        Commands::ExplainGit(args) => {
            hotpath::explain_git_and_persist(&args.path).map_err(Into::into)
        }
        Commands::Hotspots => hotpath::hotspots().map_err(Into::into),
        Commands::Doctor => hotpath::doctor().map_err(Into::into),
    };

    match result {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hotpath: {error}");
            ExitCode::FAILURE
        }
    }
}
