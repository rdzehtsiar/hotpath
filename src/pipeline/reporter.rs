// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::pipeline::events::PipelineState;

const PROGRESS_BAR_WIDTH: usize = 24;
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(100);

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
    last_rendered_at: Option<Instant>,
    min_render_interval: Duration,
    bar_style: ProgressBarStyle,
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
            last_rendered_at: None,
            min_render_interval: MIN_RENDER_INTERVAL,
            bar_style: ProgressBarStyle::detect(),
        }
    }

    #[cfg(test)]
    fn with_min_render_interval(writer: W, min_render_interval: Duration) -> Self {
        Self {
            writer,
            rendered_once: false,
            finished: false,
            last_rendered_at: None,
            min_render_interval,
            bar_style: ProgressBarStyle::Unicode,
        }
    }

    fn should_render(&self) -> bool {
        !self.rendered_once
            || self.last_rendered_at.is_some_and(|last_rendered_at| {
                last_rendered_at.elapsed() >= self.min_render_interval
            })
    }

    fn render_now(&mut self, state: &PipelineState) {
        let line = render_report_line_with_style(state, self.bar_style);
        if self.rendered_once {
            let _ = write!(self.writer, "\r\x1b[2K{line}");
        } else {
            let _ = write!(self.writer, "{line}");
            self.rendered_once = true;
        }
        self.last_rendered_at = Some(Instant::now());
        let _ = self.writer.flush();
    }
}

impl<W: Write> PipelineReporter for StdioReporter<W> {
    fn update(&mut self, state: &PipelineState) {
        if self.finished {
            return;
        }

        if self.should_render() {
            self.render_now(state);
        }
    }

    fn finish(&mut self, state: &PipelineState) {
        if !self.finished {
            self.render_now(state);
        }
        if self.rendered_once && !self.finished {
            let _ = writeln!(self.writer);
            let _ = self.writer.flush();
            self.finished = true;
        }
    }
}

pub fn render_report_line(state: &PipelineState) -> String {
    render_report_line_with_style(state, ProgressBarStyle::Unicode)
}

fn render_report_line_with_style(state: &PipelineState, style: ProgressBarStyle) -> String {
    let analysis_total = state.analysis_display_total();
    let progress_bar = render_progress_bar_with_style(
        state.analyzed_files,
        analysis_total,
        PROGRESS_BAR_WIDTH,
        style,
    );
    match state.remaining_files() {
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
    }
}

pub fn render_progress_bar(done: u64, total: u64, width: usize) -> String {
    render_progress_bar_with_style(done, total, width, ProgressBarStyle::Unicode)
}

fn render_progress_bar_with_style(
    done: u64,
    total: u64,
    width: usize,
    style: ProgressBarStyle,
) -> String {
    let width = width.max(1);
    let filled = if total == 0 {
        0
    } else {
        ((done.min(total) as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width);

    match style {
        ProgressBarStyle::Unicode => {
            format!("│{}{}│", "█".repeat(filled), " ".repeat(width - filled))
        }
        ProgressBarStyle::Ascii => {
            format!("[{}{}]", "#".repeat(filled), " ".repeat(width - filled))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressBarStyle {
    Unicode,
    Ascii,
}

impl ProgressBarStyle {
    fn detect() -> Self {
        if ascii_only_terminal() {
            Self::Ascii
        } else {
            Self::Unicode
        }
    }
}

fn ascii_only_terminal() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return true;
    }

    if std::env::var("TERM")
        .map(|term| term.eq_ignore_ascii_case("dumb"))
        .unwrap_or(false)
    {
        return true;
    }

    if cfg!(windows) {
        return std::env::var("CHCP")
            .or_else(|_| std::env::var("CODEPAGE"))
            .map(|code_page| code_page != "65001")
            .unwrap_or(false);
    }

    false
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        render_progress_bar, render_report_line, render_report_line_with_style, PipelineReporter,
        ProgressBarStyle, StdioReporter,
    };
    use crate::pipeline::events::PipelineState;

    #[test]
    fn render_line_shows_estimating_before_total_is_known() {
        let state = PipelineState {
            enumerated_files: 5,
            analyzed_files: 2,
            analysis_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let line = render_report_line(&state);

        assert!(line.starts_with("analyzed files"));
        assert!(line.contains("│                        │"));
        assert!(line.contains("2/estimating"));
        assert!(line.contains("speed 2.00 files/sec"));
    }

    #[test]
    fn render_line_shows_exact_total_and_remaining_after_total_is_known() {
        let state = PipelineState {
            enumerated_files: 5,
            total_files: Some(5),
            analyzed_files: 2,
            analysis_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let line = render_report_line(&state);

        assert!(line.contains("2/5"));
        assert!(line.contains("remaining 3"));
    }

    #[test]
    fn unicode_progress_bar_handles_zero_partial_and_complete_progress() {
        assert_eq!(render_progress_bar(0, 0, 4), "│    │");
        assert_eq!(render_progress_bar(1, 2, 4), "│██  │");
        assert_eq!(render_progress_bar(4, 4, 4), "│████│");
        assert_eq!(render_progress_bar(5, 4, 4), "│████│");
    }

    #[test]
    fn ascii_progress_bar_is_available_as_fallback() {
        let state = PipelineState {
            total_files: Some(2),
            analyzed_files: 1,
            ..PipelineState::default()
        };

        let line = render_report_line_with_style(&state, ProgressBarStyle::Ascii);

        assert!(line.contains("[############            ]"));
    }

    #[test]
    fn stdio_reporter_writes_one_line_and_final_newline() {
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
        assert_eq!(rendered.matches('\n').count(), 1);
    }

    #[test]
    fn stdio_reporter_throttles_non_final_updates() {
        let mut output = Vec::new();
        let mut reporter =
            StdioReporter::with_min_render_interval(&mut output, Duration::from_secs(60));
        let first = PipelineState {
            enumerated_files: 1,
            ..PipelineState::default()
        };
        let second = PipelineState {
            enumerated_files: 2,
            ..PipelineState::default()
        };

        reporter.update(&first);
        reporter.update(&second);

        let rendered = String::from_utf8(output).expect("reporter output should be UTF-8");
        assert!(rendered.contains("0/estimating"));
        assert!(!rendered.contains("2/estimating"));
    }
}
