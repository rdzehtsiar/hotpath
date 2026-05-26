// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};

use crate::pipeline::events::PipelineState;

const PROGRESS_BAR_WIDTH: usize = 24;

pub trait PipelineReporter {
    fn update(&mut self, state: &PipelineState);

    fn finish(&mut self, state: &PipelineState) {
        self.update(state);
    }
}

#[derive(Debug)]
pub struct NoopReporter;

impl PipelineReporter for NoopReporter {
    fn update(&mut self, _state: &PipelineState) {}
}

#[derive(Debug)]
pub struct StdioReporter<W: Write> {
    writer: W,
    rendered_once: bool,
    finished: bool,
}

impl StdioReporter<io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(io::stdout())
    }
}

impl<W: Write> StdioReporter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            rendered_once: false,
            finished: false,
        }
    }
}

impl<W: Write> PipelineReporter for StdioReporter<W> {
    fn update(&mut self, state: &PipelineState) {
        if self.finished {
            return;
        }

        let [enumeration_line, analysis_line] = render_report_lines(state);
        if self.rendered_once {
            let _ = write!(
                self.writer,
                "\x1b[1A\r\x1b[2K{enumeration_line}\n\r\x1b[2K{analysis_line}"
            );
        } else {
            let _ = write!(self.writer, "{enumeration_line}\n{analysis_line}");
            self.rendered_once = true;
        }
        let _ = self.writer.flush();
    }

    fn finish(&mut self, state: &PipelineState) {
        self.update(state);
        if self.rendered_once && !self.finished {
            let _ = writeln!(self.writer);
            let _ = self.writer.flush();
            self.finished = true;
        }
    }
}

pub fn render_report_lines(state: &PipelineState) -> [String; 2] {
    let enumeration_line = format!(
        "enumerated files {} | speed {:.2} files/sec",
        state.enumerated_files,
        state.enumeration_files_per_second()
    );
    let analysis_total = state.analysis_display_total();
    let progress_bar =
        render_progress_bar(state.analyzed_files, analysis_total, PROGRESS_BAR_WIDTH);
    let analysis_line = match state.remaining_files() {
        Some(remaining) => format!(
            "analyzed files   {progress_bar} {}/{} | remaining {} | speed {:.2} files/sec",
            state.analyzed_files,
            analysis_total,
            remaining,
            state.analysis_files_per_second()
        ),
        None => format!(
            "analyzed files   {progress_bar} {}/estimating | speed {:.2} files/sec",
            state.analyzed_files,
            state.analysis_files_per_second()
        ),
    };

    [enumeration_line, analysis_line]
}

pub fn render_progress_bar(done: u64, total: u64, width: usize) -> String {
    let width = width.max(1);
    let filled = if total == 0 {
        0
    } else {
        ((done.min(total) as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width);

    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{render_progress_bar, render_report_lines, PipelineReporter, StdioReporter};
    use crate::pipeline::events::PipelineState;

    #[test]
    fn render_lines_show_estimating_before_total_is_known() {
        let state = PipelineState {
            enumerated_files: 5,
            analyzed_files: 2,
            analysis_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let lines = render_report_lines(&state);

        assert!(lines[0].starts_with("enumerated files 5 | speed "));
        assert!(lines[1].contains("2/estimating"));
        assert!(lines[1].contains("speed 2.00 files/sec"));
    }

    #[test]
    fn render_lines_show_exact_total_and_remaining_after_total_is_known() {
        let state = PipelineState {
            enumerated_files: 5,
            total_files: Some(5),
            analyzed_files: 2,
            analysis_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let lines = render_report_lines(&state);

        assert!(lines[1].contains("2/5"));
        assert!(lines[1].contains("remaining 3"));
    }

    #[test]
    fn progress_bar_handles_zero_partial_and_complete_progress() {
        assert_eq!(render_progress_bar(0, 0, 4), "[----]");
        assert_eq!(render_progress_bar(1, 2, 4), "[##--]");
        assert_eq!(render_progress_bar(4, 4, 4), "[####]");
        assert_eq!(render_progress_bar(5, 4, 4), "[####]");
    }

    #[test]
    fn stdio_reporter_writes_two_lines_and_final_newline() {
        let mut output = Vec::new();
        let mut reporter = StdioReporter::new(&mut output);
        let state = PipelineState {
            enumerated_files: 1,
            total_files: Some(1),
            analyzed_files: 1,
            ..PipelineState::default()
        };

        reporter.finish(&state);

        let rendered = String::from_utf8(output).expect("reporter output should be UTF-8");
        assert!(rendered.ends_with('\n'));
        assert_eq!(rendered.matches('\n').count(), 2);
    }
}
