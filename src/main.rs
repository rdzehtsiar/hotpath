// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

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

    /// Estimate the current codebase context budget.
    Context(ContextArgs),

    /// Analyze diff risk for a committed base...head range.
    Diff(DiffArgs),

    /// Analyze pull request risk from explicit base and head refs.
    Pr(PrArgs),

    /// Build an aggregate repository risk report.
    Report(ReportArgs),

    /// Fail CI when advisory hotspot risk reaches a threshold.
    Ci(CiArgs),

    /// Check the local Hotpath index health.
    Doctor,

    /// Open the early keyboard-first terminal user interface.
    Tui(TuiArgs),
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Self::Scan(_) => "scan",
            Self::Parse(_) => "parse",
            Self::Complexity(_) => "complexity",
            Self::Graph(_) => "graph",
            Self::Explain(_) => "explain",
            Self::ExplainGit(_) => "explain-git",
            Self::Hotspots(_) => "hotspots",
            Self::Context(_) => "context",
            Self::Diff(_) => "diff",
            Self::Pr(_) => "pr",
            Self::Report(_) => "report",
            Self::Ci(_) => "ci",
            Self::Doctor => "doctor",
            Self::Tui(_) => "tui",
        }
    }

    fn output_mode(&self) -> &'static str {
        match self {
            Self::Scan(args) if args.json => "json",
            Self::Parse(args) if args.json => "json",
            Self::Complexity(args) if args.json => "json",
            Self::Graph(args) if args.json => "json",
            Self::Context(args) if args.json => "json",
            Self::Diff(args) if args.json => "json",
            Self::Pr(args) if args.json => "json",
            Self::Report(args) if args.json => "json",
            Self::Report(args) if args.markdown => "markdown",
            Self::Report(args) if args.sarif => "sarif",
            Self::Report(args) if args.html.is_some() => "html",
            Self::Tui(_) => "tui",
            _ => "text",
        }
    }
}

#[derive(Debug, Args)]
struct CiArgs {
    /// Fail when the maximum hotspot risk is greater than or equal to this 0-10 threshold.
    #[arg(long, value_parser = parse_ci_risk_threshold, allow_hyphen_values = true)]
    fail_on_risk: f64,
}

#[derive(Debug, Args)]
struct DiffArgs {
    /// Triple-dot committed diff range to analyze, for example main...HEAD.
    range: String,

    /// Print a machine-readable JSON diff risk report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PrArgs {
    /// Base ref for the pull request comparison.
    #[arg(long)]
    base: String,

    /// Head ref for the pull request comparison.
    #[arg(long)]
    head: String,

    /// Print a machine-readable JSON diff risk report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Print a machine-readable JSON report.
    #[arg(long, conflicts_with_all = ["markdown", "html", "sarif"])]
    json: bool,

    /// Print a human-readable Markdown report.
    #[arg(long, conflicts_with_all = ["json", "html", "sarif"])]
    markdown: bool,

    /// Print a SARIF 2.1.0 report for CI systems.
    #[arg(long, conflicts_with_all = ["json", "markdown", "html"])]
    sarif: bool,

    /// Write a self-contained static HTML report to the output directory.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = ["json", "markdown", "sarif"]
    )]
    html: Option<std::path::PathBuf>,
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
struct ContextArgs {
    /// Print a machine-readable JSON context report.
    #[arg(long)]
    json: bool,

    /// Token budget to compare against the estimate. Accepts integers with optional k or m suffix.
    #[arg(long, value_parser = parse_budget_tokens_arg)]
    budget: Option<u64>,

    /// Exclude generated files from the estimate.
    #[arg(long)]
    exclude_generated: bool,

    /// Exclude vendor files from the estimate.
    #[arg(long)]
    exclude_vendor: bool,
}

#[derive(Debug, Args)]
struct TuiArgs {
    /// Token budget to compare against the TUI context estimate. Accepts integers with optional k or m suffix.
    #[arg(long, value_parser = parse_budget_tokens_arg)]
    budget: Option<u64>,

    /// Exclude generated files from the TUI context estimate.
    #[arg(long)]
    exclude_generated: bool,

    /// Exclude vendor files from the TUI context estimate.
    #[arg(long)]
    exclude_vendor: bool,

    /// Include generated, vendored, lockfile, and minified files in TUI hotspot rows.
    #[arg(long)]
    include_generated_hotspots: bool,

    /// Use ASCII-only TUI drawing characters.
    #[arg(long)]
    ascii: bool,

    /// Disable semantic TUI colors.
    #[arg(long)]
    no_color: bool,
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
    let command_name = cli.command.name();
    let output_mode = cli.command.output_mode();
    let started = Instant::now();
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let log_root = operation_log_root(&cwd);
    hotpath::operation_log::init(&log_root);
    hotpath::operation_log::event(
        "command_started",
        serde_json::json!({
            "command": command_name,
            "output_mode": output_mode,
            "cwd": cwd.display().to_string(),
            "log_root": log_root.display().to_string(),
        }),
    );

    let command = match cli.command {
        Commands::Ci(args) => return run_ci(args, command_name, output_mode, started),
        Commands::Tui(args) => return run_tui(args, command_name, output_mode, started),
        command => command,
    };

    let result: Result<String, Box<dyn std::error::Error>> = match command {
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
        Commands::Context(args) => hotpath::context(
            hotpath::ContextOptions {
                exclude_generated: args.exclude_generated,
                exclude_vendor: args.exclude_vendor,
                budget_tokens: args.budget,
            },
            args.json,
        )
        .map_err(Into::into),
        Commands::Diff(args) => hotpath::diff_risk(&args.range, args.json).map_err(Into::into),
        Commands::Pr(args) => {
            hotpath::pr_risk(&args.base, &args.head, args.json).map_err(Into::into)
        }
        Commands::Report(args) if args.json => hotpath::report_json().map_err(Into::into),
        Commands::Report(args) if args.markdown => {
            hotpath::report::report_markdown().map_err(Into::into)
        }
        Commands::Report(args) if args.sarif => hotpath::report::report_sarif().map_err(Into::into),
        Commands::Report(args) if args.html.is_some() => {
            let output_dir = args.html.expect("html path should be present");

            hotpath::report::report_html(&output_dir).map_err(Into::into)
        }
        Commands::Report(_) => hotpath::report::report_markdown().map_err(Into::into),
        Commands::Ci(_) => unreachable!("CI command is handled before generic command dispatch"),
        Commands::Tui(_) => unreachable!("TUI command is handled before generic command dispatch"),
        Commands::Doctor => hotpath::doctor().map_err(Into::into),
    };

    match result {
        Ok(output) => {
            println!("{output}");
            log_command_completed(command_name, output_mode, started);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hotpath: {error}");
            log_command_failed(command_name, output_mode, started, &error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn operation_log_root(cwd: &Path) -> PathBuf {
    git2::Repository::discover(cwd)
        .ok()
        .and_then(|repository| repository.workdir().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn run_tui(
    args: TuiArgs,
    command_name: &'static str,
    output_mode: &'static str,
    started: Instant,
) -> ExitCode {
    match hotpath::run_tui_with_options(hotpath::TuiOptions {
        context: hotpath::ContextOptions {
            exclude_generated: args.exclude_generated,
            exclude_vendor: args.exclude_vendor,
            budget_tokens: args.budget,
        },
        include_generated_hotspots: args.include_generated_hotspots,
        ascii: args.ascii,
        no_color: args.no_color,
    }) {
        Ok(()) => {
            log_command_completed(command_name, output_mode, started);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hotpath: {error}");
            log_command_failed(command_name, output_mode, started, &error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn run_ci(
    args: CiArgs,
    command_name: &'static str,
    output_mode: &'static str,
    started: Instant,
) -> ExitCode {
    match hotpath::report::ci_risk_gate(args.fail_on_risk) {
        Ok(evaluation) => {
            print!("{}", hotpath::report::render_ci_risk(&evaluation));
            log_command_completed(command_name, output_mode, started);
            if evaluation.threshold_breached {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("hotpath: {error}");
            log_command_failed(command_name, output_mode, started, &error.to_string());
            ExitCode::from(2)
        }
    }
}

fn log_command_completed(command_name: &'static str, output_mode: &'static str, started: Instant) {
    hotpath::operation_log::event(
        "command_completed",
        serde_json::json!({
            "command": command_name,
            "output_mode": output_mode,
            "elapsed_ms": started.elapsed().as_millis(),
        }),
    );
}

fn log_command_failed(
    command_name: &'static str,
    output_mode: &'static str,
    started: Instant,
    error: &str,
) {
    hotpath::operation_log::event(
        "command_failed",
        serde_json::json!({
            "command": command_name,
            "output_mode": output_mode,
            "elapsed_ms": started.elapsed().as_millis(),
            "error": error,
        }),
    );
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

fn parse_ci_risk_threshold(value: &str) -> Result<f64, String> {
    let threshold = value
        .parse::<f64>()
        .map_err(|_| "fail-on-risk must be a number greater than 0 and at most 10".to_owned())?;

    if !threshold.is_finite() {
        Err("fail-on-risk must be finite".to_owned())
    } else if threshold <= 0.0 {
        Err("fail-on-risk must be greater than 0".to_owned())
    } else if threshold > 10.0 {
        Err("fail-on-risk must be at most 10".to_owned())
    } else {
        Ok(threshold)
    }
}

fn parse_budget_tokens_arg(value: &str) -> Result<u64, String> {
    hotpath::parse_budget_tokens(value).map_err(|error| error.to_string())
}
