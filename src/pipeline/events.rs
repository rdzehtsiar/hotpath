// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use crate::pipeline::enumerator::EnumerationResult;

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    ScanStarted,
    EnumerationProgress {
        files_detected: u64,
        entries_walked: u64,
        elapsed: Duration,
    },
    FileDiscovered {
        path: PathBuf,
    },
    EnumerationCompleted {
        result: EnumerationResult,
    },
    FileAnalysisCompleted {
        analyzed_files: u64,
        elapsed: Duration,
    },
    ScanCompleted {
        files_detected: u64,
        analyzed_files: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineState {
    pub enumerated_files: u64,
    pub entries_walked: u64,
    pub total_files: Option<u64>,
    pub analyzed_files: u64,
    pub enumeration_elapsed: Duration,
    pub analysis_elapsed: Duration,
    pub scan_completed: bool,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            enumerated_files: 0,
            entries_walked: 0,
            total_files: None,
            analyzed_files: 0,
            enumeration_elapsed: Duration::ZERO,
            analysis_elapsed: Duration::ZERO,
            scan_completed: false,
        }
    }
}

impl PipelineState {
    pub fn apply(&mut self, event: &PipelineEvent) {
        match event {
            PipelineEvent::ScanStarted => {
                *self = Self::default();
            }
            PipelineEvent::EnumerationProgress {
                files_detected,
                entries_walked,
                elapsed,
            } => {
                self.enumerated_files = *files_detected;
                self.entries_walked = *entries_walked;
                self.enumeration_elapsed = *elapsed;
            }
            PipelineEvent::FileDiscovered { .. } => {
                self.enumerated_files += 1;
            }
            PipelineEvent::EnumerationCompleted { result } => {
                self.enumerated_files = result.files_detected;
                self.entries_walked = result.entries_walked;
                self.enumeration_elapsed = result.elapsed;
                self.total_files = Some(result.files_detected);
            }
            PipelineEvent::FileAnalysisCompleted {
                analyzed_files,
                elapsed,
            } => {
                self.analyzed_files = self.analyzed_files.max(*analyzed_files);
                self.analysis_elapsed = self.analysis_elapsed.max(*elapsed);
            }
            PipelineEvent::ScanCompleted {
                files_detected,
                analyzed_files,
            } => {
                self.total_files = Some(*files_detected);
                self.enumerated_files = *files_detected;
                self.analyzed_files = *analyzed_files;
                self.scan_completed = true;
            }
        }
    }

    pub fn enumeration_files_per_second(&self) -> f64 {
        files_per_second(self.enumerated_files, self.enumeration_elapsed)
    }

    pub fn analysis_files_per_second(&self) -> f64 {
        files_per_second(self.analyzed_files, self.analysis_elapsed)
    }

    pub fn remaining_files(&self) -> Option<u64> {
        self.total_files
            .map(|total| total.saturating_sub(self.analyzed_files))
    }

    pub fn analysis_display_total(&self) -> u64 {
        self.total_files
            .unwrap_or_else(|| self.enumerated_files.max(self.analyzed_files))
    }
}

fn files_per_second(files: u64, elapsed: Duration) -> f64 {
    let elapsed_seconds = elapsed.as_secs_f64();
    if elapsed_seconds <= f64::EPSILON {
        return files as f64;
    }

    files as f64 / elapsed_seconds
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{PipelineEvent, PipelineState};
    use crate::pipeline::enumerator::EnumerationResult;

    #[test]
    fn reducer_starts_with_unknown_total() {
        let state = PipelineState::default();

        assert_eq!(state.total_files, None);
        assert_eq!(state.remaining_files(), None);
    }

    #[test]
    fn file_discovery_increments_enumerated_count() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::FileDiscovered {
            path: PathBuf::from("main.go"),
        });

        assert_eq!(state.enumerated_files, 1);
        assert_eq!(state.total_files, None);
    }

    #[test]
    fn enumeration_completion_locks_exact_total() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::EnumerationCompleted {
            result: EnumerationResult {
                root: PathBuf::from("."),
                files_detected: 7,
                entries_walked: 9,
                elapsed: Duration::from_secs(2),
            },
        });

        assert_eq!(state.enumerated_files, 7);
        assert_eq!(state.total_files, Some(7));
        assert_eq!(state.remaining_files(), Some(7));
        assert_eq!(state.enumeration_files_per_second(), 3.5);
    }

    #[test]
    fn analysis_completion_updates_remaining_when_total_is_known() {
        let mut state = PipelineState {
            total_files: Some(5),
            ..PipelineState::default()
        };

        state.apply(&PipelineEvent::FileAnalysisCompleted {
            analyzed_files: 2,
            elapsed: Duration::from_secs(1),
        });

        assert_eq!(state.analyzed_files, 2);
        assert_eq!(state.remaining_files(), Some(3));
        assert_eq!(state.analysis_files_per_second(), 2.0);
    }

    #[test]
    fn speeds_are_finite_and_non_negative() {
        let state = PipelineState {
            enumerated_files: 3,
            analyzed_files: 2,
            ..PipelineState::default()
        };

        assert!(state.enumeration_files_per_second().is_finite());
        assert!(state.enumeration_files_per_second() >= 0.0);
        assert!(state.analysis_files_per_second().is_finite());
        assert!(state.analysis_files_per_second() >= 0.0);
    }
}
