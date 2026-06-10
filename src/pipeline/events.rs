// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use crate::pipeline::enumerator::EnumerationResult;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    pub mode: Option<String>,
    pub confidence: Option<String>,
    pub collection_mode: Option<String>,
    pub max_commits: Option<String>,
    pub max_age_days: Option<String>,
    pub first_parent: Option<bool>,
    pub renames: Option<bool>,
    pub cochange_max_files_per_commit: Option<usize>,
    pub recent_churn_window_days: Option<i64>,
    pub head_timestamp: Option<i64>,
    pub warning: Option<String>,
    pub diagnostic: Option<String>,
    pub index_action: Option<String>,
}

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
    TotalFilesCounted {
        files_detected: u64,
        elapsed: Duration,
    },
    EnumerationCompleted {
        result: EnumerationResult,
    },
    FileAnalysisCompleted {
        analyzed_files: u64,
        elapsed: Duration,
    },
    GitPlanningStarted,
    GitPlanningCompleted {
        total_commits: u64,
        total_chunks: u64,
    },
    GitStatusUpdated {
        status: GitStatus,
    },
    GitHistorySkipped {
        reason: String,
    },
    GitHistoryChunkCompleted {
        processed_commits: u64,
        file_changes: u64,
        elapsed: Duration,
    },
    GitHistoryChunkFailed {
        reason: String,
        elapsed: Duration,
    },
    GitHistoryCompleted {
        processed_commits: u64,
        elapsed: Duration,
    },
    StoreRecordsPlanned {
        total_records: u64,
    },
    StoreBatchFlushed {
        stored_records: u64,
        elapsed: Duration,
    },
    StoreCompleted {
        stored_records: u64,
        elapsed: Duration,
    },
    ScanCompleted {
        files_detected: u64,
        analyzed_files: u64,
        git_commits_processed: u64,
        elapsed: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineState {
    pub enumerated_files: u64,
    pub entries_walked: u64,
    pub total_files: Option<u64>,
    pub analyzed_files: u64,
    pub total_git_commits: Option<u64>,
    pub total_git_chunks: Option<u64>,
    pub git_commits_processed: u64,
    pub git_file_changes: u64,
    pub store_planned_records: Option<u64>,
    pub stored_records: u64,
    pub enumeration_elapsed: Duration,
    pub analysis_elapsed: Duration,
    pub git_elapsed: Duration,
    pub store_elapsed: Duration,
    pub total_elapsed: Duration,
    pub git_completed: bool,
    pub git_skipped: bool,
    pub git_status: GitStatus,
    pub store_completed: bool,
    pub scan_completed: bool,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            enumerated_files: 0,
            entries_walked: 0,
            total_files: None,
            analyzed_files: 0,
            total_git_commits: None,
            total_git_chunks: None,
            git_commits_processed: 0,
            git_file_changes: 0,
            store_planned_records: None,
            stored_records: 0,
            enumeration_elapsed: Duration::ZERO,
            analysis_elapsed: Duration::ZERO,
            git_elapsed: Duration::ZERO,
            store_elapsed: Duration::ZERO,
            total_elapsed: Duration::ZERO,
            git_completed: false,
            git_skipped: false,
            git_status: GitStatus::default(),
            store_completed: false,
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
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::FileDiscovered { .. } => {
                self.enumerated_files += 1;
            }
            PipelineEvent::TotalFilesCounted {
                files_detected,
                elapsed,
            } => {
                self.total_files = Some(*files_detected);
                self.store_planned_records = Some(
                    self.store_planned_records
                        .unwrap_or(0)
                        .max(files_detected.saturating_mul(2)),
                );
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::EnumerationCompleted { result } => {
                self.enumerated_files = result.files_detected;
                self.entries_walked = result.entries_walked;
                self.enumeration_elapsed = result.elapsed;
                self.total_files = Some(result.files_detected);
                self.store_planned_records = Some(
                    self.store_planned_records
                        .unwrap_or(0)
                        .max(result.files_detected.saturating_mul(2)),
                );
                self.total_elapsed = self.total_elapsed.max(result.elapsed);
            }
            PipelineEvent::FileAnalysisCompleted {
                analyzed_files,
                elapsed,
            } => {
                self.analyzed_files = self.analyzed_files.max(*analyzed_files);
                self.analysis_elapsed = self.analysis_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::GitPlanningStarted => {
                self.git_completed = false;
                self.git_skipped = false;
            }
            PipelineEvent::GitPlanningCompleted {
                total_commits,
                total_chunks,
            } => {
                self.total_git_commits = Some(*total_commits);
                self.total_git_chunks = Some(*total_chunks);
                if *total_commits == 0 {
                    self.git_completed = true;
                }
            }
            PipelineEvent::GitStatusUpdated { status } => {
                self.git_status = status.clone();
            }
            PipelineEvent::GitHistorySkipped { .. } => {
                self.total_git_commits = Some(0);
                self.total_git_chunks = Some(0);
                self.git_commits_processed = 0;
                self.git_completed = true;
                self.git_skipped = true;
            }
            PipelineEvent::GitHistoryChunkCompleted {
                processed_commits,
                file_changes,
                elapsed,
            } => {
                self.git_commits_processed += *processed_commits;
                self.git_file_changes += *file_changes;
                self.git_elapsed = self.git_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::GitHistoryChunkFailed { elapsed, .. } => {
                self.git_elapsed = self.git_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::GitHistoryCompleted {
                processed_commits,
                elapsed,
            } => {
                self.git_commits_processed = self.git_commits_processed.max(*processed_commits);
                if self
                    .total_git_commits
                    .is_some_and(|total| self.git_commits_processed != total)
                {
                    self.total_git_commits = Some(self.git_commits_processed);
                }
                self.git_elapsed = self.git_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
                self.git_completed = true;
            }
            PipelineEvent::StoreRecordsPlanned { total_records } => {
                self.store_planned_records =
                    Some(self.store_planned_records.unwrap_or(0).max(*total_records));
            }
            PipelineEvent::StoreBatchFlushed {
                stored_records,
                elapsed,
            } => {
                self.stored_records = self.stored_records.max(*stored_records);
                self.store_elapsed = self.store_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
            PipelineEvent::StoreCompleted {
                stored_records,
                elapsed,
            } => {
                self.stored_records = self.stored_records.max(*stored_records);
                self.store_elapsed = self.store_elapsed.max(*elapsed);
                self.total_elapsed = self.total_elapsed.max(*elapsed);
                self.store_completed = true;
            }
            PipelineEvent::ScanCompleted {
                files_detected,
                analyzed_files,
                git_commits_processed,
                elapsed,
            } => {
                self.total_files = Some(*files_detected);
                self.enumerated_files = *files_detected;
                self.analyzed_files = *analyzed_files;
                self.git_commits_processed = self.git_commits_processed.max(*git_commits_processed);
                if self
                    .total_git_commits
                    .is_some_and(|total| self.git_commits_processed != total)
                {
                    self.total_git_commits = Some(self.git_commits_processed);
                }
                self.git_completed = true;
                self.store_completed = true;
                self.scan_completed = true;
                self.total_elapsed = self.total_elapsed.max(*elapsed);
            }
        }
    }

    pub fn enumeration_files_per_second(&self) -> f64 {
        files_per_second(self.enumerated_files, self.enumeration_elapsed)
    }

    pub fn analysis_files_per_second(&self) -> f64 {
        files_per_second(self.analyzed_files, self.analysis_elapsed)
    }

    pub fn git_commits_per_second(&self) -> f64 {
        files_per_second(self.git_commits_processed, self.git_elapsed)
    }

    pub fn remaining_files(&self) -> Option<u64> {
        self.total_files
            .map(|total| total.saturating_sub(self.analyzed_files))
    }

    pub fn analysis_display_total(&self) -> u64 {
        self.total_files.unwrap_or(0)
    }

    pub fn git_remaining_commits(&self) -> Option<u64> {
        self.total_git_commits
            .map(|total| total.saturating_sub(self.git_commits_processed))
    }

    pub fn git_display_total(&self) -> u64 {
        self.total_git_commits.unwrap_or(0)
    }

    pub fn store_records_per_second(&self) -> f64 {
        files_per_second(self.stored_records, self.store_elapsed)
    }

    pub fn store_remaining_records(&self) -> Option<u64> {
        self.store_total_records()
            .map(|total| total.saturating_sub(self.stored_records))
    }

    pub fn store_display_total(&self) -> u64 {
        self.store_total_records().unwrap_or(0)
    }

    fn store_total_records(&self) -> Option<u64> {
        if let Some(total_records) = self.store_planned_records {
            return Some(total_records);
        }

        match (self.total_files, self.total_git_chunks) {
            (Some(total_files), Some(total_git_chunks)) => Some(total_files + total_git_chunks),
            _ => None,
        }
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
    fn total_file_count_event_sets_total_before_enumeration_completes() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::TotalFilesCounted {
            files_detected: 4,
            elapsed: Duration::from_secs(1),
        });

        assert_eq!(state.total_files, Some(4));
        assert_eq!(state.remaining_files(), Some(4));
        assert_eq!(state.analysis_display_total(), 4);
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
        assert!(state.git_commits_per_second().is_finite());
        assert!(state.git_commits_per_second() >= 0.0);
    }

    #[test]
    fn git_planning_and_chunk_events_update_git_progress() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::GitPlanningCompleted {
            total_commits: 3,
            total_chunks: 2,
        });
        state.apply(&PipelineEvent::GitHistoryChunkCompleted {
            processed_commits: 2,
            file_changes: 5,
            elapsed: Duration::from_secs(1),
        });

        assert_eq!(state.total_git_commits, Some(3));
        assert_eq!(state.git_commits_processed, 2);
        assert_eq!(state.git_file_changes, 5);
        assert_eq!(state.git_remaining_commits(), Some(1));
        assert_eq!(state.git_commits_per_second(), 2.0);
    }

    #[test]
    fn git_skipped_marks_git_complete_with_zero_total() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::GitHistorySkipped {
            reason: "not a git worktree".to_owned(),
        });

        assert_eq!(state.total_git_commits, Some(0));
        assert_eq!(state.total_git_chunks, Some(0));
        assert!(state.git_completed);
        assert!(state.git_skipped);
    }

    #[test]
    fn store_total_is_known_after_file_and_git_chunk_totals_are_known() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::TotalFilesCounted {
            files_detected: 3,
            elapsed: Duration::from_secs(1),
        });
        assert_eq!(state.store_remaining_records(), Some(6));

        state.apply(&PipelineEvent::GitPlanningCompleted {
            total_commits: 5,
            total_chunks: 2,
        });
        state.apply(&PipelineEvent::StoreRecordsPlanned { total_records: 8 });
        state.apply(&PipelineEvent::StoreBatchFlushed {
            stored_records: 4,
            elapsed: Duration::from_secs(2),
        });

        assert_eq!(state.store_display_total(), 8);
        assert_eq!(state.store_remaining_records(), Some(4));
        assert_eq!(state.store_records_per_second(), 2.0);
    }

    #[test]
    fn store_completion_marks_store_complete() {
        let mut state = PipelineState::default();

        state.apply(&PipelineEvent::StoreCompleted {
            stored_records: 2,
            elapsed: Duration::from_secs(1),
        });

        assert_eq!(state.stored_records, 2);
        assert!(state.store_completed);
    }
}
