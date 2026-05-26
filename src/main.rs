// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
    let mut renderer = ScanLineRenderer::default();
    let result = env::current_dir()
        .map_err(hotpath::pipeline::enumerator::EnumerationError::CurrentDir)
        .and_then(|root| {
            let engine = hotpath::pipeline::analysis_engine::AnalysisEngine::new(root);
            engine.scan(|progress| {
                renderer.render_progress(&progress);
            })
        });

    match result {
        Ok(result) => {
            renderer.finish_result(&result);
            ExitCode::SUCCESS
        }
        Err(error) => {
            renderer.finish_line();
            eprintln!("hotpath: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Default)]
struct ScanLineRenderer {
    last_width: usize,
    line_finished: bool,
}

impl ScanLineRenderer {
    fn render_progress(&mut self, progress: &hotpath::pipeline::enumerator::EnumerationProgress) {
        self.render_line(progress.files_detected, progress.files_per_second(), false);
    }

    fn finish_result(&mut self, result: &hotpath::pipeline::enumerator::EnumerationResult) {
        if self.last_width == 0 {
            self.render_line(result.files_detected, result.files_per_second(), false);
        }
        self.finish_line();
    }

    fn finish_line(&mut self) {
        if self.last_width > 0 && !self.line_finished {
            println!();
            self.line_finished = true;
        }
    }

    fn render_line(&mut self, files_detected: u64, files_per_second: f64, newline: bool) {
        let line = render_scan_line(files_detected, files_per_second);
        let padding = self.last_width.saturating_sub(line.len());

        print!("\r{line}{}", " ".repeat(padding));
        if newline {
            println!();
            self.line_finished = true;
        } else {
            self.line_finished = false;
        }
        let _ = io::stdout().flush();
        self.last_width = line.len();
    }
}

fn render_scan_line(files_detected: u64, files_per_second: f64) -> String {
    format!("files detected {files_detected} | speed {files_per_second:.2} files/sec")
}
