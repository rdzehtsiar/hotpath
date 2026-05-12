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

    /// Parse supported scanned source files and print an early symbol report.
    Parse(ParseArgs),

    /// Analyze parsed symbols and print a complexity report.
    Complexity(ComplexityArgs),

    /// Show one-hop internal dependencies for a selected module.
    Graph(GraphArgs),

    /// Explain hotspot scoring for one current file.
    Explain(ExplainArgs),

    /// Explain local Git history metrics for one file.
    ExplainGit(ExplainGitArgs),

    /// Rank current files by advisory hotspot risk.
    Hotspots(HotspotsArgs),

    /// Check the local Hotpath index health.
    Doctor,
}

#[derive(Debug, Args)]
struct ExplainGitArgs {
    /// Repository-relative or worktree-relative file path to explain.
    path: std::path::PathBuf,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    /// Repository-relative or worktree-relative file path to explain.
    path: std::path::PathBuf,
}

#[derive(Debug, Args)]
struct HotspotsArgs {
    /// Maximum number of ranked rows to display.
    #[arg(long, default_value_t = hotpath::DEFAULT_HOTSPOTS_LIMIT, value_parser = parse_positive_limit, allow_hyphen_values = true)]
    limit: usize,

    /// Hide generated files from the displayed rows.
    #[arg(long)]
    exclude_generated: bool,

    /// Hide vendor files from the displayed rows.
    #[arg(long)]
    exclude_vendor: bool,
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

#[derive(Debug, Args)]
struct ParseArgs {
    /// Print a machine-readable JSON parse report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ComplexityArgs {
    /// Print a machine-readable JSON complexity report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct GraphArgs {
    /// Repository-relative prefix or bare module name to graph.
    #[arg(long)]
    module: String,

    /// Print a machine-readable JSON graph report.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: Result<String, Box<dyn std::error::Error>> = match cli.command {
        Commands::Scan(args) if args.json => hotpath::scan_json().map_err(Into::into),
        Commands::Scan(args) if args.summary => hotpath::scan_summary().map_err(Into::into),
        Commands::Scan(_) => hotpath::scan_summary().map_err(Into::into),
        Commands::Parse(args) if args.json => hotpath::parse_json().map_err(Into::into),
        Commands::Parse(_) => hotpath::parse_summary().map_err(Into::into),
        Commands::Complexity(args) if args.json => hotpath::complexity_json().map_err(Into::into),
        Commands::Complexity(_) => hotpath::complexity_summary().map_err(Into::into),
        Commands::Graph(args) if args.json => hotpath::graph_json(&args.module).map_err(Into::into),
        Commands::Graph(args) => hotpath::graph_summary(&args.module).map_err(Into::into),
        Commands::Explain(args) => hotpath::explain(&args.path).map_err(Into::into),
        Commands::ExplainGit(args) => {
            hotpath::explain_git_and_persist(&args.path).map_err(Into::into)
        }
        Commands::Hotspots(args) => hotpath::hotspots(hotpath::HotspotsOptions {
            limit: args.limit,
            exclude_generated: args.exclude_generated,
            exclude_vendor: args.exclude_vendor,
        })
        .map_err(Into::into),
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

fn parse_positive_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<i128>()
        .map_err(|_| "limit must be a positive integer".to_owned())?;

    if limit <= 0 {
        Err("limit must be greater than 0".to_owned())
    } else {
        usize::try_from(limit).map_err(|_| "limit is too large".to_owned())
    }
}
