// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use crate::git::GitFileChange;

const HALF_LIFE_DAYS: f64 = 730.0;
const FULL_BULK_FILE_LIMIT: usize = 10;
const MIN_BULK_WEIGHT: f64 = 0.10;
const OWNER_SHARE_THRESHOLD: f64 = 0.10;
const OWNER_DISPLAY_LIMIT: usize = 3;
const MEANINGFUL_LINE_FLOOR_SHARE: f64 = 0.05;
const MEANINGFUL_LINE_FLOOR_MAX: f64 = 200.0;
const MEANINGFUL_COMMIT_FLOOR: u64 = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OperationalOwnershipSnapshot {
    pub by_file: Vec<OperationalFileOwnership>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OperationalFileOwnership {
    pub path: String,
    pub owners: Vec<OperationalOwnerShare>,
    pub others_share: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OperationalOwnerShare {
    pub author: String,
    pub meaningful_commits: u64,
    pub effective_lines: f64,
    pub score: f64,
    pub share: f64,
}

#[derive(Debug, Default)]
struct ContributorAccumulator {
    commits: BTreeSet<String>,
    meaningful_commits: BTreeSet<String>,
    effective_lines: f64,
    weighted_lines: f64,
}

pub fn operational_ownership_from_changes(
    changes: &[GitFileChange],
    head_commit_time: i64,
) -> OperationalOwnershipSnapshot {
    let touched_paths_by_commit = touched_paths_by_commit(changes);
    let mut by_path = BTreeMap::<String, BTreeMap<String, ContributorAccumulator>>::new();

    for change in changes {
        let changed_lines = change.added_lines.saturating_add(change.deleted_lines);
        if changed_lines == 0 {
            continue;
        }

        let touched_file_count = touched_paths_by_commit
            .get(change.commit_id.as_str())
            .map_or(1, BTreeSet::len);
        let bulk_weight = bulk_change_weight(touched_file_count);
        let recency_weight = recency_weight(head_commit_time, change.commit_time);
        let effective_lines = changed_lines as f64 * bulk_weight;
        let accumulator = by_path
            .entry(change.path.clone())
            .or_default()
            .entry(change.author.clone())
            .or_default();

        accumulator.commits.insert(change.commit_id.clone());
        accumulator
            .meaningful_commits
            .insert(change.commit_id.clone());
        accumulator.effective_lines += effective_lines;
        accumulator.weighted_lines += effective_lines * recency_weight;
    }

    let by_file = by_path
        .into_iter()
        .map(|(path, by_author)| ownership_for_path(path, by_author))
        .collect();

    OperationalOwnershipSnapshot { by_file }
}

fn touched_paths_by_commit(changes: &[GitFileChange]) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut by_commit = BTreeMap::<&str, BTreeSet<&str>>::new();

    for change in changes {
        by_commit
            .entry(change.commit_id.as_str())
            .or_default()
            .insert(change.path.as_str());
    }

    by_commit
}

fn ownership_for_path(
    path: String,
    by_author: BTreeMap<String, ContributorAccumulator>,
) -> OperationalFileOwnership {
    let total_effective_lines = by_author
        .values()
        .map(|contributor| contributor.effective_lines)
        .sum::<f64>();
    let mut owners = by_author
        .into_iter()
        .filter_map(|(author, contributor)| {
            let meaningful_commits = contributor.meaningful_commits.len() as u64;
            let activity_weight = sustained_activity_weight(meaningful_commits);
            let score = contributor.weighted_lines * activity_weight;

            (score > 0.0).then_some(OperationalOwnerShare {
                author,
                meaningful_commits,
                effective_lines: contributor.effective_lines,
                score,
                share: 0.0,
            })
        })
        .collect::<Vec<_>>();
    let total_score = owners.iter().map(|owner| owner.score).sum::<f64>();

    if total_score > 0.0 {
        for owner in &mut owners {
            owner.share = owner.score / total_score;
        }
    }

    owners.sort_by(compare_owner_shares);

    let line_floor = meaningful_line_floor(total_effective_lines);
    let mut visible = Vec::new();
    let mut others_share = 0.0;
    for (index, owner) in owners.into_iter().enumerate() {
        let eligible_by_rank_or_share =
            index < OWNER_DISPLAY_LIMIT || owner.share >= OWNER_SHARE_THRESHOLD;
        let meaningful_enough = owner.effective_lines >= line_floor
            || owner.meaningful_commits >= MEANINGFUL_COMMIT_FLOOR;

        if visible.len() < OWNER_DISPLAY_LIMIT && eligible_by_rank_or_share && meaningful_enough {
            visible.push(owner);
        } else {
            others_share += owner.share;
        }
    }

    OperationalFileOwnership {
        path,
        owners: visible,
        others_share,
    }
}

fn compare_owner_shares(
    left: &OperationalOwnerShare,
    right: &OperationalOwnerShare,
) -> std::cmp::Ordering {
    right
        .share
        .total_cmp(&left.share)
        .then_with(|| right.score.total_cmp(&left.score))
        .then_with(|| left.author.cmp(&right.author))
}

fn meaningful_line_floor(total_effective_lines: f64) -> f64 {
    (total_effective_lines * MEANINGFUL_LINE_FLOOR_SHARE)
        .ceil()
        .clamp(1.0, MEANINGFUL_LINE_FLOOR_MAX)
}

fn bulk_change_weight(touched_file_count: usize) -> f64 {
    if touched_file_count <= FULL_BULK_FILE_LIMIT {
        1.0
    } else {
        (FULL_BULK_FILE_LIMIT as f64 / touched_file_count as f64)
            .sqrt()
            .max(MIN_BULK_WEIGHT)
    }
}

fn recency_weight(head_commit_time: i64, commit_time: i64) -> f64 {
    let age_days = head_commit_time.saturating_sub(commit_time).max(0) as f64 / 86_400.0;

    0.5_f64.powf(age_days / HALF_LIFE_DAYS)
}

fn sustained_activity_weight(meaningful_commits: u64) -> f64 {
    match meaningful_commits {
        0 => 0.0,
        1 => 0.25,
        2 => 0.60,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitChangeKind;

    #[test]
    fn repeated_recent_maintainer_beats_stale_originator() {
        let changes = vec![
            change("old", "Alice <alice@example.invalid>", 0, 900, 100),
            change(
                "new-1",
                "Bob <bob@example.invalid>",
                6 * 365 * 86_400,
                120,
                0,
            ),
            change(
                "new-2",
                "Bob <bob@example.invalid>",
                6 * 365 * 86_400 + 10,
                100,
                20,
            ),
            change(
                "new-3",
                "Bob <bob@example.invalid>",
                6 * 365 * 86_400 + 20,
                100,
                10,
            ),
        ];

        let ownership = operational_ownership_from_changes(&changes, 6 * 365 * 86_400 + 20);
        let file = &ownership.by_file[0];

        assert_eq!(file.owners[0].author, "Bob <bob@example.invalid>");
        assert!(file.owners[0].share > 0.70);
    }

    #[test]
    fn one_time_small_edits_collapse_into_others() {
        let changes = vec![
            change("a1", "Alice <alice@example.invalid>", 100, 100, 0),
            change("a2", "Alice <alice@example.invalid>", 200, 100, 0),
            change("a3", "Alice <alice@example.invalid>", 300, 100, 0),
            change("b1", "Bob <bob@example.invalid>", 300, 2, 0),
            change("c1", "Cara <cara@example.invalid>", 300, 2, 0),
        ];

        let ownership = operational_ownership_from_changes(&changes, 300);
        let file = &ownership.by_file[0];

        assert_eq!(file.owners.len(), 1);
        assert_eq!(file.owners[0].author, "Alice <alice@example.invalid>");
        assert!(file.others_share > 0.0);
    }

    #[test]
    fn ownership_is_deterministic_for_equal_scores() {
        let changes = vec![
            change("b", "Zed <zed@example.invalid>", 100, 10, 0),
            change("a", "Ada <ada@example.invalid>", 100, 10, 0),
        ];

        let ownership = operational_ownership_from_changes(&changes, 100);
        let file = &ownership.by_file[0];

        assert_eq!(file.owners[0].author, "Ada <ada@example.invalid>");
        assert_eq!(file.owners[1].author, "Zed <zed@example.invalid>");
    }

    fn change(
        commit_id: &str,
        author: &str,
        commit_time: i64,
        added_lines: u64,
        deleted_lines: u64,
    ) -> GitFileChange {
        GitFileChange {
            commit_id: commit_id.to_owned(),
            parent_count: 1,
            is_merge: false,
            author: author.to_owned(),
            commit_time,
            path: "src/lib.rs".to_owned(),
            change_kind: GitChangeKind::Modified,
            added_lines,
            deleted_lines,
        }
    }
}
