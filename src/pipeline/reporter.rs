// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::pipeline::events::PipelineState;

const PROGRESS_BAR_WIDTH: usize = 24;
const COUNT_VALUE_WIDTH: usize = 9;
const TOTAL_VALUE_WIDTH: usize = 10;
const SPEED_VALUE_WIDTH: usize = 10;
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(windows)]
const UTF8_CODE_PAGE: u32 = 65001;

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
        let [files_line, git_line, elapsed_line] =
            render_report_lines_with_style(state, self.bar_style);
        if self.rendered_once {
            let _ = write!(
                self.writer,
                "\x1b[2A\r\x1b[2K{files_line}\n\r\x1b[2K{git_line}\n\r\x1b[2K{elapsed_line}"
            );
        } else {
            let _ = write!(self.writer, "{files_line}\n{git_line}\n{elapsed_line}");
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

pub fn render_report_lines(state: &PipelineState) -> [String; 3] {
    render_report_lines_with_style(state, ProgressBarStyle::Unicode)
}

fn render_report_lines_with_style(state: &PipelineState, style: ProgressBarStyle) -> [String; 3] {
    let files_line = render_progress_row(
        "files",
        state.analyzed_files,
        state.total_files,
        state.analysis_files_per_second(),
        "files/sec",
        style,
    );
    let (git_processed, git_total) = if state.git_skipped {
        (1, Some(1))
    } else {
        (state.git_commits_processed, state.total_git_commits)
    };
    let git_line = render_progress_row(
        "git",
        git_processed,
        git_total,
        state.git_commits_per_second(),
        "commits/sec",
        style,
    );
    let elapsed_line = render_elapsed_row(state.total_elapsed);

    [files_line, git_line, elapsed_line]
}

fn render_progress_row(
    label: &str,
    processed: u64,
    total: Option<u64>,
    speed: f64,
    speed_unit: &str,
    style: ProgressBarStyle,
) -> String {
    let progress_bar = render_progress_bar_with_style(processed, total, PROGRESS_BAR_WIDTH, style);
    let total_text = total
        .map(|total| total.to_string())
        .unwrap_or_else(|| "estimating".to_owned());

    format!(
        "{label:<5} {progress_bar} {processed:>COUNT_VALUE_WIDTH$}/{total_text:<TOTAL_VALUE_WIDTH$} | speed {speed:>SPEED_VALUE_WIDTH$.2} {speed_unit}"
    )
}

fn render_elapsed_row(elapsed: Duration) -> String {
    format!("time  elapsed {}", format_elapsed(elapsed))
}

fn format_elapsed(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let centiseconds = elapsed.subsec_millis() / 10;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{centiseconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}.{centiseconds:02}")
    }
}

pub fn render_progress_bar(done: u64, total: u64, width: usize) -> String {
    render_progress_bar_with_style(done, Some(total), width, ProgressBarStyle::Unicode)
}

fn render_progress_bar_with_style(
    done: u64,
    total: Option<u64>,
    width: usize,
    style: ProgressBarStyle,
) -> String {
    let width = width.max(1);
    let total = total.unwrap_or(0);
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
    let term = std::env::var("TERM").ok();
    ascii_only_terminal_for(
        std::env::var_os("NO_COLOR").is_some(),
        term.as_deref(),
        terminal_platform(),
    )
}

fn ascii_only_terminal_for(no_color: bool, term: Option<&str>, platform: TerminalPlatform) -> bool {
    if no_color {
        return true;
    }

    if term.is_some_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return true;
    }

    if let TerminalPlatform::Windows { output_utf8 } = platform {
        return !output_utf8;
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalPlatform {
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows { output_utf8: bool },
    #[cfg_attr(windows, allow(dead_code))]
    Other,
}

#[cfg(windows)]
fn terminal_platform() -> TerminalPlatform {
    TerminalPlatform::Windows {
        output_utf8: windows_console_output_is_utf8(),
    }
}

#[cfg(not(windows))]
fn terminal_platform() -> TerminalPlatform {
    TerminalPlatform::Other
}

#[cfg(windows)]
fn windows_console_output_is_utf8() -> bool {
    unsafe { windows_sys::Win32::System::Console::GetConsoleOutputCP() == UTF8_CODE_PAGE }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ascii_only_terminal_for, render_progress_bar, render_progress_row, render_report_lines,
        render_report_lines_with_style, PipelineReporter, ProgressBarStyle, StdioReporter,
        TerminalPlatform,
    };
    use crate::pipeline::events::PipelineState;

    #[test]
    fn render_lines_show_estimating_before_totals_are_known() {
        let state = PipelineState {
            enumerated_files: 5,
            analyzed_files: 2,
            analysis_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let lines = render_report_lines(&state);

        assert!(lines[0].starts_with("files"));
        assert!(lines[0].contains("│                        │"));
        assert!(lines[0].contains("        2/estimating"));
        assert!(lines[0].contains("speed       2.00 files/sec"));
        assert!(lines[1].starts_with("git"));
        assert!(lines[1].contains("        0/estimating"));
        assert!(lines[2].starts_with("time"));
    }

    #[test]
    fn render_lines_show_exact_totals_without_remaining_after_totals_are_known() {
        let state = PipelineState {
            enumerated_files: 5,
            total_files: Some(5),
            analyzed_files: 2,
            total_git_commits: Some(4),
            total_git_chunks: Some(2),
            git_commits_processed: 1,
            analysis_elapsed: Duration::from_secs(1),
            git_elapsed: Duration::from_secs(1),
            ..PipelineState::default()
        };

        let lines = render_report_lines(&state);

        assert!(lines[0].contains("        2/5"));
        assert!(lines[1].contains("        1/4"));
        assert!(lines[2].starts_with("time"));
        assert!(!lines.iter().any(|line| line.contains("remaining")));
        assert!(lines[1].contains("speed       1.00 commits/sec"));
        let speed_columns: Vec<_> = lines
            .iter()
            .take(2)
            .map(|line| {
                line[..line.find("| speed").expect("speed column should exist")]
                    .chars()
                    .count()
            })
            .collect();
        assert_eq!(speed_columns[0], speed_columns[1]);
    }

    #[test]
    fn skipped_git_renders_as_complete() {
        let state = PipelineState {
            git_skipped: true,
            git_completed: true,
            ..PipelineState::default()
        };

        let lines = render_report_lines(&state);

        assert!(lines[1].contains("        1/1"));
        assert_eq!(
            lines[1],
            render_progress_row(
                "git",
                1,
                Some(1),
                0.0,
                "commits/sec",
                ProgressBarStyle::Unicode
            )
        );
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

        let lines = render_report_lines_with_style(&state, ProgressBarStyle::Ascii);

        assert!(lines[0].contains("[############            ]"));
    }

    #[test]
    fn style_detection_uses_ascii_when_no_color_is_set() {
        assert!(ascii_only_terminal_for(true, None, TerminalPlatform::Other));
    }

    #[test]
    fn style_detection_uses_ascii_when_term_is_dumb() {
        assert!(ascii_only_terminal_for(
            false,
            Some("dumb"),
            TerminalPlatform::Other
        ));
    }

    #[test]
    fn style_detection_allows_unicode_on_non_windows_without_ascii_signal() {
        assert!(!ascii_only_terminal_for(
            false,
            None,
            TerminalPlatform::Other
        ));
    }

    #[test]
    fn style_detection_requires_confirmed_utf8_output_on_windows() {
        assert!(!ascii_only_terminal_for(
            false,
            None,
            TerminalPlatform::Windows { output_utf8: true }
        ));
        assert!(ascii_only_terminal_for(
            false,
            None,
            TerminalPlatform::Windows { output_utf8: false }
        ));
    }

    #[test]
    fn stdio_reporter_writes_three_lines_and_final_newline_without_git_status() {
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
        assert_eq!(rendered.matches('\n').count(), 3);
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
