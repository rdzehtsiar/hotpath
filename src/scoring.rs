// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::git::{GitCoChange, GitFileMetrics};
use crate::FileRecord;

/// Current score formula identifier.
///
/// Formula identifiers are part of score explanations and persisted output.
/// Changing the formula semantics should introduce a new identifier.
pub const CURRENT_SCORE_FORMULA_ID: &str = "hotpath.score.v1";
const SIZE_LINE_COUNT_SATURATION: u64 = 1_000;
const SIZE_BYTE_SATURATION: u64 = 128 * 1024;
const CHURN_LINE_SATURATION: u64 = 2_000;
const RECENT_GROWTH_SATURATION: f64 = 1.0;
const AUTHOR_FRAGMENTATION_SATURATION: u64 = 5;
const CO_CHANGED_FILE_SATURATION: u64 = 25;
const INITIAL_RISK_FORMULA_TERMS: [RiskFormulaTerm; 5] = [
    RiskFormulaTerm {
        name: "churn_score",
        metric: NormalizedMetric::Churn,
        weight: 0.35,
    },
    RiskFormulaTerm {
        name: "size_score",
        metric: NormalizedMetric::Size,
        weight: 0.20,
    },
    RiskFormulaTerm {
        name: "author_fragmentation",
        metric: NormalizedMetric::Ownership,
        weight: 0.20,
    },
    RiskFormulaTerm {
        name: "recent_growth",
        metric: NormalizedMetric::RecentChurn,
        weight: 0.15,
    },
    RiskFormulaTerm {
        name: "cochange_score",
        metric: NormalizedMetric::Coupling,
        weight: 0.10,
    },
];

#[derive(Debug, Clone, Copy)]
struct RiskFormulaTerm {
    name: &'static str,
    metric: NormalizedMetric,
    weight: f64,
}

/// Version identity for a hotspot score formula.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormulaVersion {
    /// Stable machine-readable formula identifier.
    pub id: String,
    /// Major formula version. Increment for meaningfully different scores.
    pub major: u16,
    /// Minor formula version. Increment for compatible explanatory changes.
    pub minor: u16,
}

impl FormulaVersion {
    /// Return the current score formula version without performing scoring.
    pub fn current() -> Self {
        Self {
            id: CURRENT_SCORE_FORMULA_ID.to_owned(),
            major: 1,
            minor: 0,
        }
    }
}

/// Raw repository facts available to a score formula for one path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RawScoreMetrics {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// File size from the scanner, when available.
    pub byte_size: Option<u64>,
    /// Text line count from the scanner, when available.
    pub line_count: Option<u64>,
    /// Number of local Git commits that touched this path.
    pub commits_per_file: Option<u64>,
    /// Added plus deleted lines across observed local history.
    pub total_churn_lines: Option<u64>,
    /// Added plus deleted lines in the current recent-history window.
    pub recent_churn_lines: Option<u64>,
    /// Number of distinct exact author identities that touched this path.
    pub author_count: Option<u64>,
    /// Dominant owner's share of file-touching commits.
    pub dominant_owner_share: Option<f64>,
    /// Number of observed files that co-changed with this path.
    pub co_changed_file_count: Option<u64>,
}

/// Normalized formula inputs for one path.
///
/// Values are intentionally optional because early Hotpath analyses may not
/// have every signal available. This type records normalized inputs only; it
/// does not define how normalization is performed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormalizedScoreMetrics {
    pub size: Option<f64>,
    pub churn: Option<f64>,
    pub recent_churn: Option<f64>,
    pub ownership: Option<f64>,
    pub coupling: Option<f64>,
}

/// Normalized score inputs plus limitations observed while deriving them.
///
/// This is deliberately not a final risk score. It only translates available
/// raw facts into bounded inputs that a later formula can consume.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreNormalization {
    pub normalized_metrics: NormalizedScoreMetrics,
    pub limitations: Vec<ScoreLimitation>,
}

/// One weighted normalized term in a score formula.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeightedTerm {
    /// Stable term name, for example `churn`.
    pub name: String,
    /// Normalized metric consumed by this term.
    pub metric: NormalizedMetric,
    /// Exact formula version that assigned this term weight.
    pub formula_version: FormulaVersion,
    /// Formula weight assigned to this term.
    pub weight: f64,
    /// Normalized input consumed by this term, when available.
    pub normalized_input: Option<f64>,
    /// Weight multiplied by the normalized input, or `0.0` when input is unavailable.
    pub weighted_contribution: f64,
}

/// Named normalized metrics that can participate in a score formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedMetric {
    Size,
    Churn,
    RecentChurn,
    Ownership,
    Coupling,
}

/// Known limitation or approximation attached to a score output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScoreLimitation {
    /// Stable machine-readable limitation code.
    pub code: String,
    /// Human-readable explanation of the limitation.
    pub message: String,
}

/// Final advisory hotspot score for one path.
///
/// This is an output model only. It records enough context for a caller to
/// explain exactly which formula version and weighted terms produced the score,
/// alongside the raw and normalized inputs used by that formula.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HotspotScore {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Advisory score value produced by the formula.
    pub value: f64,
    /// Exact formula version used to produce this score.
    pub formula_version: FormulaVersion,
    /// Weighted terms that define the formula used for this score.
    pub weighted_terms: Vec<WeightedTerm>,
    /// Raw facts that contributed to this score.
    pub raw_metrics: RawScoreMetrics,
    /// Normalized inputs consumed by the weighted terms.
    pub normalized_metrics: NormalizedScoreMetrics,
    /// Known limitations and approximations for this score.
    pub limitations: Vec<ScoreLimitation>,
}

/// Ranked advisory hotspot score.
///
/// Ranking is intentionally kept separate from score calculation so the score
/// formula remains independent from collection-level ordering.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedHotspotScore {
    /// One-based position after deterministic hotspot ranking.
    pub rank: u64,
    /// Score output for the ranked path.
    #[serde(flatten)]
    pub score: HotspotScore,
}

/// Calculate the initial advisory hotspot risk score for one path.
///
/// Missing normalized inputs contribute `0.0` for their fixed-weight terms.
/// Weights are never redistributed across available inputs; omissions remain
/// visible through normalization limitations.
pub fn calculate_hotspot_score(raw_metrics: RawScoreMetrics) -> HotspotScore {
    let path = raw_metrics.path.clone();
    let normalization = normalize_score_metrics(&raw_metrics);
    let normalized_metrics = normalization.normalized_metrics;
    let formula_version = FormulaVersion::current();
    let weighted_terms = weighted_risk_terms(&formula_version, &normalized_metrics);
    let value = weighted_terms
        .iter()
        .map(|term| term.weighted_contribution)
        .sum();

    HotspotScore {
        path,
        value,
        formula_version,
        weighted_terms,
        raw_metrics,
        normalized_metrics,
        limitations: normalization.limitations,
    }
}

/// Rank hotspot scores deterministically for reports or persisted output.
///
/// Scores are ordered by advisory score descending, then repository-relative
/// path ascending. Ranks are ordinal and one-based after tie-breakers apply.
pub fn rank_hotspot_scores(scores: &[HotspotScore]) -> Vec<RankedHotspotScore> {
    let mut sorted_scores = scores.to_vec();
    sorted_scores.sort_by(compare_hotspot_scores_for_ranking);

    sorted_scores
        .into_iter()
        .enumerate()
        .map(|(index, score)| RankedHotspotScore {
            rank: index as u64 + 1,
            score,
        })
        .collect()
}

/// Assemble score inputs for current scanned files from scanner and Git facts.
///
/// The scanner defines the current file set. Git metrics enrich those files
/// when local history has observed them; files without Git metrics receive
/// explicit zero-valued history inputs rather than being dropped.
pub fn raw_score_metrics_from_scan_and_git(
    files: &[FileRecord],
    git_metrics: &[GitFileMetrics],
    co_changes: &[GitCoChange],
) -> Vec<RawScoreMetrics> {
    let metrics_by_path = git_metrics
        .iter()
        .map(|metric| (metric.path.as_str(), metric))
        .collect::<BTreeMap<_, _>>();
    let co_changed_paths = co_changed_paths_by_file(co_changes);
    let mut files = files.iter().collect::<Vec<_>>();

    files.sort_by(|left, right| left.path.cmp(&right.path));

    files
        .into_iter()
        .map(|file| {
            let metric = metrics_by_path.get(file.path.as_str()).copied();

            RawScoreMetrics {
                path: file.path.clone(),
                byte_size: file.byte_size,
                line_count: file.line_count,
                commits_per_file: Some(metric.map_or(0, |metric| metric.commits_per_file)),
                total_churn_lines: Some(metric.map_or(0, |metric| {
                    metric
                        .total_churn_added
                        .saturating_add(metric.total_churn_deleted)
                })),
                recent_churn_lines: Some(metric.map_or(0, |metric| {
                    metric
                        .recent_churn_added
                        .saturating_add(metric.recent_churn_deleted)
                })),
                author_count: Some(metric.map_or(0, |metric| metric.author_count)),
                dominant_owner_share: metric.and_then(|metric| metric.dominant_owner_share),
                co_changed_file_count: Some(
                    co_changed_paths
                        .get(file.path.as_str())
                        .map_or(0, |paths| paths.len() as u64),
                ),
            }
        })
        .collect()
}

/// Convert raw facts into bounded normalized score inputs.
///
/// Normalized values are always in the inclusive `0.0..=1.0` range. Missing
/// source facts remain `None`; the function records a limitation instead of
/// substituting a guessed value.
pub fn normalize_score_metrics(raw_metrics: &RawScoreMetrics) -> ScoreNormalization {
    let mut limitations = Vec::new();
    let size = normalize_size(raw_metrics, &mut limitations);
    let churn = normalize_count(
        raw_metrics.total_churn_lines,
        CHURN_LINE_SATURATION,
        &mut limitations,
        "missing_total_churn_lines",
        "total churn is unavailable; churn normalization is omitted",
    );
    let recent_churn = normalize_recent_growth(raw_metrics, &mut limitations);
    let ownership = normalize_author_fragmentation(raw_metrics, &mut limitations);
    let coupling = normalize_count(
        raw_metrics.co_changed_file_count,
        CO_CHANGED_FILE_SATURATION,
        &mut limitations,
        "missing_co_changed_file_count",
        "co-change count is unavailable; coupling normalization is omitted",
    );

    ScoreNormalization {
        normalized_metrics: NormalizedScoreMetrics {
            size,
            churn,
            recent_churn,
            ownership,
            coupling,
        },
        limitations,
    }
}

fn normalize_size(
    raw_metrics: &RawScoreMetrics,
    limitations: &mut Vec<ScoreLimitation>,
) -> Option<f64> {
    if let Some(line_count) = raw_metrics.line_count {
        return Some(saturating_ratio(line_count, SIZE_LINE_COUNT_SATURATION));
    }

    if let Some(byte_size) = raw_metrics.byte_size {
        limitations.push(score_limitation(
            "size_uses_byte_size_fallback",
            "line count is unavailable; size normalization uses byte size",
        ));
        return Some(saturating_ratio(byte_size, SIZE_BYTE_SATURATION));
    }

    limitations.push(score_limitation(
        "missing_size_metric",
        "line count and byte size are unavailable; size normalization is omitted",
    ));
    None
}

fn normalize_recent_growth(
    raw_metrics: &RawScoreMetrics,
    limitations: &mut Vec<ScoreLimitation>,
) -> Option<f64> {
    let recent_churn_lines = match raw_metrics.recent_churn_lines {
        Some(recent_churn_lines) => recent_churn_lines,
        None => {
            limitations.push(score_limitation(
                "missing_recent_churn_lines",
                "recent churn is unavailable; recent growth normalization is omitted",
            ));
            return None;
        }
    };

    let line_count = match raw_metrics.line_count {
        Some(line_count) => line_count,
        None => {
            limitations.push(score_limitation(
                "missing_recent_growth_line_count",
                "line count is unavailable; recent growth normalization is omitted",
            ));
            return None;
        }
    };

    if line_count == 0 {
        if recent_churn_lines == 0 {
            return Some(0.0);
        }

        limitations.push(score_limitation(
            "zero_line_count_recent_growth",
            "line count is zero but recent churn is nonzero; recent growth is saturated",
        ));
        return Some(1.0);
    }

    Some(clamp_unit(
        recent_churn_lines as f64 / line_count as f64 / RECENT_GROWTH_SATURATION,
    ))
}

fn normalize_author_fragmentation(
    raw_metrics: &RawScoreMetrics,
    limitations: &mut Vec<ScoreLimitation>,
) -> Option<f64> {
    let owner_share_was_missing = raw_metrics.dominant_owner_share.is_none();
    let author_component = raw_metrics
        .author_count
        .map(saturating_author_fragmentation);
    let owner_component = normalize_owner_dispersion(raw_metrics.dominant_owner_share, limitations);

    match (author_component, owner_component) {
        (Some(author_component), Some(owner_component)) => {
            Some((author_component + owner_component) / 2.0)
        }
        (Some(author_component), None) => {
            if owner_share_was_missing {
                limitations.push(score_limitation(
                    "author_fragmentation_missing_owner_share",
                    "dominant owner share is unavailable; author fragmentation uses author count only",
                ));
            }
            Some(author_component)
        }
        (None, Some(owner_component)) => {
            limitations.push(score_limitation(
                "author_fragmentation_missing_author_count",
                "author count is unavailable; author fragmentation uses dominant owner share only",
            ));
            Some(owner_component)
        }
        (None, None) if owner_share_was_missing => {
            limitations.push(score_limitation(
                "missing_author_fragmentation_metrics",
                "author count and dominant owner share are unavailable; author fragmentation is omitted",
            ));
            None
        }
        (None, None) => {
            limitations.push(score_limitation(
                "author_fragmentation_missing_author_count",
                "author count is unavailable; author fragmentation is omitted",
            ));
            None
        }
    }
}

fn normalize_owner_dispersion(
    dominant_owner_share: Option<f64>,
    limitations: &mut Vec<ScoreLimitation>,
) -> Option<f64> {
    let dominant_owner_share = dominant_owner_share?;

    if !dominant_owner_share.is_finite() {
        limitations.push(score_limitation(
            "invalid_dominant_owner_share",
            "dominant owner share is not finite; owner-share normalization is omitted",
        ));
        return None;
    }

    if !(0.0..=1.0).contains(&dominant_owner_share) {
        limitations.push(score_limitation(
            "dominant_owner_share_out_of_range",
            "dominant owner share is outside 0.0..=1.0; value is clamped",
        ));
    }

    Some(1.0 - clamp_unit(dominant_owner_share))
}

fn normalize_count(
    value: Option<u64>,
    saturation: u64,
    limitations: &mut Vec<ScoreLimitation>,
    missing_code: &'static str,
    missing_message: &'static str,
) -> Option<f64> {
    match value {
        Some(value) => Some(saturating_ratio(value, saturation)),
        None => {
            limitations.push(score_limitation(missing_code, missing_message));
            None
        }
    }
}

fn weighted_risk_terms(
    formula_version: &FormulaVersion,
    normalized_metrics: &NormalizedScoreMetrics,
) -> Vec<WeightedTerm> {
    INITIAL_RISK_FORMULA_TERMS
        .iter()
        .map(|term| {
            let normalized_input = normalized_metric_value(normalized_metrics, term.metric);

            WeightedTerm {
                name: term.name.to_owned(),
                metric: term.metric,
                formula_version: formula_version.clone(),
                weight: term.weight,
                normalized_input,
                weighted_contribution: normalized_input.unwrap_or(0.0) * term.weight,
            }
        })
        .collect()
}

fn normalized_metric_value(
    normalized_metrics: &NormalizedScoreMetrics,
    metric: NormalizedMetric,
) -> Option<f64> {
    match metric {
        NormalizedMetric::Size => normalized_metrics.size,
        NormalizedMetric::Churn => normalized_metrics.churn,
        NormalizedMetric::RecentChurn => normalized_metrics.recent_churn,
        NormalizedMetric::Ownership => normalized_metrics.ownership,
        NormalizedMetric::Coupling => normalized_metrics.coupling,
    }
}

fn compare_hotspot_scores_for_ranking(
    left: &HotspotScore,
    right: &HotspotScore,
) -> std::cmp::Ordering {
    right
        .value
        .total_cmp(&left.value)
        .then_with(|| left.path.cmp(&right.path))
}

fn co_changed_paths_by_file(co_changes: &[GitCoChange]) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut paths_by_file = BTreeMap::<&str, BTreeSet<&str>>::new();

    for co_change in co_changes {
        paths_by_file
            .entry(co_change.left_path.as_str())
            .or_default()
            .insert(co_change.right_path.as_str());
        paths_by_file
            .entry(co_change.right_path.as_str())
            .or_default()
            .insert(co_change.left_path.as_str());
    }

    paths_by_file
}

fn saturating_author_fragmentation(author_count: u64) -> f64 {
    saturating_ratio(
        author_count.saturating_sub(1),
        AUTHOR_FRAGMENTATION_SATURATION,
    )
}

fn saturating_ratio(value: u64, saturation: u64) -> f64 {
    clamp_unit(value as f64 / saturation as f64)
}

fn clamp_unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn score_limitation(code: &'static str, message: &'static str) -> ScoreLimitation {
    ScoreLimitation {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_formula_version_is_stable_and_explicit() {
        let version = FormulaVersion::current();

        assert_eq!(version.id, "hotpath.score.v1");
        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 0);
    }

    #[test]
    fn hotspot_score_carries_formula_terms_inputs_and_limitations() {
        let raw_metrics = RawScoreMetrics {
            path: "src/lib.rs".to_owned(),
            byte_size: Some(120),
            line_count: Some(10),
            commits_per_file: Some(3),
            total_churn_lines: Some(42),
            recent_churn_lines: Some(12),
            author_count: Some(2),
            dominant_owner_share: Some(0.75),
            co_changed_file_count: Some(4),
        };
        let normalized_metrics = NormalizedScoreMetrics {
            size: Some(0.2),
            churn: Some(0.7),
            recent_churn: Some(0.4),
            ownership: Some(0.3),
            coupling: Some(0.5),
        };
        let weighted_terms = vec![
            WeightedTerm {
                name: "churn".to_owned(),
                metric: NormalizedMetric::Churn,
                formula_version: FormulaVersion::current(),
                weight: 0.4,
                normalized_input: Some(0.7),
                weighted_contribution: 0.28,
            },
            WeightedTerm {
                name: "coupling".to_owned(),
                metric: NormalizedMetric::Coupling,
                formula_version: FormulaVersion::current(),
                weight: 0.2,
                normalized_input: Some(0.5),
                weighted_contribution: 0.1,
            },
        ];
        let limitations = vec![ScoreLimitation {
            code: "local_git_only".to_owned(),
            message: "score uses local Git history only".to_owned(),
        }];

        let score = HotspotScore {
            path: raw_metrics.path.clone(),
            value: 0.62,
            formula_version: FormulaVersion::current(),
            weighted_terms: weighted_terms.clone(),
            raw_metrics: raw_metrics.clone(),
            normalized_metrics: normalized_metrics.clone(),
            limitations: limitations.clone(),
        };

        assert_eq!(score.formula_version.id, CURRENT_SCORE_FORMULA_ID);
        assert_eq!(score.weighted_terms, weighted_terms);
        assert_eq!(score.raw_metrics, raw_metrics);
        assert_eq!(score.normalized_metrics, normalized_metrics);
        assert_eq!(score.limitations, limitations);
    }

    #[test]
    fn calculates_initial_risk_formula_and_preserves_contributions() {
        let raw_metrics = RawScoreMetrics {
            path: "src/risk.rs".to_owned(),
            byte_size: Some(20_000),
            line_count: Some(400),
            commits_per_file: Some(10),
            total_churn_lines: Some(1_000),
            recent_churn_lines: Some(200),
            author_count: Some(3),
            dominant_owner_share: Some(0.5),
            co_changed_file_count: Some(10),
        };

        let score = calculate_hotspot_score(raw_metrics.clone());

        assert_eq!(score.path, "src/risk.rs");
        assert_eq!(score.formula_version, FormulaVersion::current());
        assert_eq!(score.raw_metrics, raw_metrics);
        assert_eq!(score.limitations, Vec::new());
        assert_eq!(
            score.normalized_metrics,
            NormalizedScoreMetrics {
                size: Some(0.4),
                churn: Some(0.5),
                recent_churn: Some(0.5),
                ownership: Some(0.45),
                coupling: Some(0.4),
            }
        );
        assert_f64_near(score.value, 0.46);

        let expected_terms = [
            (
                "churn_score",
                NormalizedMetric::Churn,
                0.35,
                Some(0.5),
                0.175,
            ),
            ("size_score", NormalizedMetric::Size, 0.20, Some(0.4), 0.08),
            (
                "author_fragmentation",
                NormalizedMetric::Ownership,
                0.20,
                Some(0.45),
                0.09,
            ),
            (
                "recent_growth",
                NormalizedMetric::RecentChurn,
                0.15,
                Some(0.5),
                0.075,
            ),
            (
                "cochange_score",
                NormalizedMetric::Coupling,
                0.10,
                Some(0.4),
                0.04,
            ),
        ];

        assert_eq!(score.weighted_terms.len(), expected_terms.len());
        for (term, (name, metric, weight, normalized_input, weighted_contribution)) in
            score.weighted_terms.iter().zip(expected_terms)
        {
            assert_eq!(term.name, name);
            assert_eq!(term.metric, metric);
            assert_eq!(term.formula_version, score.formula_version);
            assert_f64_near(term.weight, weight);
            assert_f64_near(
                term.normalized_input
                    .expect("formula term should preserve its normalized input"),
                normalized_input.expect("expected formula term should include a normalized input"),
            );
            assert_f64_near(term.weighted_contribution, weighted_contribution);
        }
    }

    #[test]
    fn missing_score_inputs_keep_fixed_weights_and_existing_limitations() {
        let score = calculate_hotspot_score(RawScoreMetrics {
            path: "src/missing.rs".to_owned(),
            byte_size: None,
            line_count: None,
            commits_per_file: None,
            total_churn_lines: None,
            recent_churn_lines: None,
            author_count: None,
            dominant_owner_share: None,
            co_changed_file_count: None,
        });

        assert_eq!(score.value, 0.0);
        assert_eq!(
            limitation_codes(&score.limitations),
            vec![
                "missing_size_metric",
                "missing_total_churn_lines",
                "missing_recent_churn_lines",
                "missing_author_fragmentation_metrics",
                "missing_co_changed_file_count",
            ]
        );
        assert!(score
            .weighted_terms
            .iter()
            .all(|term| term.normalized_input.is_none()));
        assert!(score
            .weighted_terms
            .iter()
            .all(|term| term.weighted_contribution == 0.0));

        let weights = score
            .weighted_terms
            .iter()
            .map(|term| term.weight)
            .collect::<Vec<_>>();
        assert_eq!(weights, vec![0.35, 0.20, 0.20, 0.15, 0.10]);
    }

    #[test]
    fn ranks_hotspot_scores_by_value_descending_then_path_ascending() {
        let scores = vec![
            score_with_value("src/zeta.rs", 0.80),
            score_with_value("src/top.rs", 0.95),
            score_with_value("src/alpha.rs", 0.80),
            score_with_value("src/bottom.rs", 0.10),
        ];

        let ranked = rank_hotspot_scores(&scores);

        assert_eq!(
            ranked
                .iter()
                .map(|score| (score.rank, score.score.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "src/top.rs"),
                (2, "src/alpha.rs"),
                (3, "src/zeta.rs"),
                (4, "src/bottom.rs"),
            ]
        );
    }

    #[test]
    fn ranking_is_stable_across_reordered_equal_score_inputs() {
        let first_run = vec![
            score_with_value("src/b.rs", 0.50),
            score_with_value("src/a.rs", 0.50),
            score_with_value("src/c.rs", 0.50),
        ];
        let second_run = vec![
            score_with_value("src/c.rs", 0.50),
            score_with_value("src/b.rs", 0.50),
            score_with_value("src/a.rs", 0.50),
        ];

        let first_paths = ranked_paths(&rank_hotspot_scores(&first_run));
        let second_paths = ranked_paths(&rank_hotspot_scores(&second_run));

        assert_eq!(first_paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert_eq!(second_paths, first_paths);
    }

    #[test]
    fn assembles_raw_score_metrics_for_current_scanned_files() {
        let files = vec![
            file_record("src/stable.rs", Some(25), Some(2)),
            file_record("src/risky.rs", Some(200), Some(20)),
        ];
        let git_metrics = vec![GitFileMetrics {
            path: "src/risky.rs".to_owned(),
            commits_per_file: 3,
            total_churn_added: 30,
            total_churn_deleted: 5,
            recent_churn_added: 10,
            recent_churn_deleted: 1,
            author_count: 2,
            dominant_owner: Some("Ada Lovelace <ada@example.invalid>".to_owned()),
            dominant_owner_share: Some(2.0 / 3.0),
            first_commit_id: Some("a".repeat(40)),
            first_commit_time: Some(1),
            last_commit_id: Some("b".repeat(40)),
            last_commit_time: Some(2),
            file_age_days: Some(10),
        }];
        let co_changes = vec![
            GitCoChange {
                left_path: "src/risky.rs".to_owned(),
                right_path: "src/stable.rs".to_owned(),
                commit_count: 1,
            },
            GitCoChange {
                left_path: "src/old.rs".to_owned(),
                right_path: "src/risky.rs".to_owned(),
                commit_count: 2,
            },
        ];

        let raw_metrics = raw_score_metrics_from_scan_and_git(&files, &git_metrics, &co_changes);

        assert_eq!(
            raw_metrics,
            vec![
                RawScoreMetrics {
                    path: "src/risky.rs".to_owned(),
                    byte_size: Some(200),
                    line_count: Some(20),
                    commits_per_file: Some(3),
                    total_churn_lines: Some(35),
                    recent_churn_lines: Some(11),
                    author_count: Some(2),
                    dominant_owner_share: Some(2.0 / 3.0),
                    co_changed_file_count: Some(2),
                },
                RawScoreMetrics {
                    path: "src/stable.rs".to_owned(),
                    byte_size: Some(25),
                    line_count: Some(2),
                    commits_per_file: Some(0),
                    total_churn_lines: Some(0),
                    recent_churn_lines: Some(0),
                    author_count: Some(0),
                    dominant_owner_share: None,
                    co_changed_file_count: Some(1),
                },
            ]
        );
    }

    #[test]
    fn normalization_maps_raw_metrics_to_bounded_inputs() {
        let raw_metrics = RawScoreMetrics {
            total_churn_lines: Some(1_000),
            recent_churn_lines: Some(250),
            author_count: Some(3),
            dominant_owner_share: Some(0.75),
            co_changed_file_count: Some(5),
            ..raw_metrics()
        };

        let normalization = normalize_score_metrics(&raw_metrics);

        assert_eq!(normalization.normalized_metrics.size, Some(0.5));
        assert_eq!(normalization.normalized_metrics.churn, Some(0.5));
        assert_eq!(normalization.normalized_metrics.recent_churn, Some(0.5));
        assert_near(normalization.normalized_metrics.ownership, 0.325);
        assert_eq!(normalization.normalized_metrics.coupling, Some(0.2));
        assert!(normalization.limitations.is_empty());
    }

    #[test]
    fn normalization_saturates_at_lower_and_upper_boundaries() {
        let minimum = normalize_score_metrics(&RawScoreMetrics {
            byte_size: Some(0),
            line_count: Some(0),
            commits_per_file: Some(0),
            total_churn_lines: Some(0),
            recent_churn_lines: Some(0),
            author_count: Some(1),
            dominant_owner_share: Some(1.0),
            co_changed_file_count: Some(0),
            path: "src/min.rs".to_owned(),
        });

        assert_eq!(minimum.normalized_metrics.size, Some(0.0));
        assert_eq!(minimum.normalized_metrics.churn, Some(0.0));
        assert_eq!(minimum.normalized_metrics.recent_churn, Some(0.0));
        assert_eq!(minimum.normalized_metrics.ownership, Some(0.0));
        assert_eq!(minimum.normalized_metrics.coupling, Some(0.0));
        assert!(minimum.limitations.is_empty());

        let maximum = normalize_score_metrics(&RawScoreMetrics {
            byte_size: Some(usize::MAX as u64),
            line_count: Some(u64::MAX),
            commits_per_file: Some(u64::MAX),
            total_churn_lines: Some(u64::MAX),
            recent_churn_lines: Some(u64::MAX),
            author_count: Some(u64::MAX),
            dominant_owner_share: Some(0.0),
            co_changed_file_count: Some(u64::MAX),
            path: "src/max.rs".to_owned(),
        });

        assert_eq!(maximum.normalized_metrics.size, Some(1.0));
        assert_eq!(maximum.normalized_metrics.churn, Some(1.0));
        assert_eq!(maximum.normalized_metrics.recent_churn, Some(1.0));
        assert_eq!(maximum.normalized_metrics.ownership, Some(1.0));
        assert_eq!(maximum.normalized_metrics.coupling, Some(1.0));
        assert!(maximum.limitations.is_empty());
    }

    #[test]
    fn normalization_hits_each_metric_saturation_threshold() {
        let normalization = normalize_score_metrics(&RawScoreMetrics {
            byte_size: Some(42),
            line_count: Some(SIZE_LINE_COUNT_SATURATION),
            commits_per_file: Some(10),
            total_churn_lines: Some(CHURN_LINE_SATURATION),
            recent_churn_lines: Some(SIZE_LINE_COUNT_SATURATION),
            author_count: Some(AUTHOR_FRAGMENTATION_SATURATION + 1),
            dominant_owner_share: Some(0.0),
            co_changed_file_count: Some(CO_CHANGED_FILE_SATURATION),
            path: "src/saturated.rs".to_owned(),
        });

        assert_eq!(
            normalization.normalized_metrics,
            NormalizedScoreMetrics {
                size: Some(1.0),
                churn: Some(1.0),
                recent_churn: Some(1.0),
                ownership: Some(1.0),
                coupling: Some(1.0),
            }
        );
        assert!(normalization.limitations.is_empty());
    }

    #[test]
    fn missing_inputs_are_omitted_and_recorded_as_limitations() {
        let normalization = normalize_score_metrics(&RawScoreMetrics {
            path: "src/missing.rs".to_owned(),
            byte_size: None,
            line_count: None,
            commits_per_file: None,
            total_churn_lines: None,
            recent_churn_lines: None,
            author_count: None,
            dominant_owner_share: None,
            co_changed_file_count: None,
        });

        assert_eq!(
            normalization.normalized_metrics,
            NormalizedScoreMetrics {
                size: None,
                churn: None,
                recent_churn: None,
                ownership: None,
                coupling: None,
            }
        );
        assert_eq!(
            limitation_codes(&normalization.limitations),
            vec![
                "missing_size_metric",
                "missing_total_churn_lines",
                "missing_recent_churn_lines",
                "missing_author_fragmentation_metrics",
                "missing_co_changed_file_count",
            ]
        );
    }

    #[test]
    fn size_uses_byte_fallback_when_line_count_is_missing() {
        let raw_metrics = RawScoreMetrics {
            line_count: None,
            byte_size: Some(64 * 1024),
            ..raw_metrics()
        };

        let normalization = normalize_score_metrics(&raw_metrics);

        assert_eq!(normalization.normalized_metrics.size, Some(0.5));
        assert!(
            limitation_codes(&normalization.limitations).contains(&"size_uses_byte_size_fallback")
        );
    }

    #[test]
    fn recent_growth_requires_recent_churn_and_line_count() {
        let missing_line_count = normalize_score_metrics(&RawScoreMetrics {
            line_count: None,
            byte_size: Some(10),
            recent_churn_lines: Some(10),
            ..raw_metrics()
        });

        assert_eq!(missing_line_count.normalized_metrics.recent_churn, None);
        assert!(limitation_codes(&missing_line_count.limitations)
            .contains(&"missing_recent_growth_line_count"));

        let zero_line_count = normalize_score_metrics(&RawScoreMetrics {
            line_count: Some(0),
            recent_churn_lines: Some(1),
            ..raw_metrics()
        });

        assert_eq!(zero_line_count.normalized_metrics.recent_churn, Some(1.0));
        assert!(limitation_codes(&zero_line_count.limitations)
            .contains(&"zero_line_count_recent_growth"));
    }

    #[test]
    fn author_fragmentation_records_partial_and_invalid_inputs() {
        let missing_owner_share = normalize_score_metrics(&RawScoreMetrics {
            author_count: Some(3),
            dominant_owner_share: None,
            ..raw_metrics()
        });
        assert_eq!(missing_owner_share.normalized_metrics.ownership, Some(0.4));
        assert!(limitation_codes(&missing_owner_share.limitations)
            .contains(&"author_fragmentation_missing_owner_share"));

        let missing_author_count = normalize_score_metrics(&RawScoreMetrics {
            author_count: None,
            dominant_owner_share: Some(0.25),
            ..raw_metrics()
        });
        assert_eq!(
            missing_author_count.normalized_metrics.ownership,
            Some(0.75)
        );
        assert!(limitation_codes(&missing_author_count.limitations)
            .contains(&"author_fragmentation_missing_author_count"));

        let out_of_range_owner_share = normalize_score_metrics(&RawScoreMetrics {
            author_count: Some(1),
            dominant_owner_share: Some(1.5),
            ..raw_metrics()
        });
        assert_eq!(
            out_of_range_owner_share.normalized_metrics.ownership,
            Some(0.0)
        );
        assert!(limitation_codes(&out_of_range_owner_share.limitations)
            .contains(&"dominant_owner_share_out_of_range"));

        let non_finite_owner_share = normalize_score_metrics(&RawScoreMetrics {
            author_count: None,
            dominant_owner_share: Some(f64::NAN),
            ..raw_metrics()
        });
        assert_eq!(non_finite_owner_share.normalized_metrics.ownership, None);
        assert_eq!(
            limitation_codes(&non_finite_owner_share.limitations),
            vec![
                "invalid_dominant_owner_share",
                "author_fragmentation_missing_author_count",
            ]
        );

        let author_count_with_non_finite_owner_share = normalize_score_metrics(&RawScoreMetrics {
            author_count: Some(3),
            dominant_owner_share: Some(f64::NAN),
            ..raw_metrics()
        });
        assert_eq!(
            author_count_with_non_finite_owner_share
                .normalized_metrics
                .ownership,
            Some(0.4)
        );
        assert_eq!(
            limitation_codes(&author_count_with_non_finite_owner_share.limitations),
            vec!["invalid_dominant_owner_share"]
        );
    }

    #[test]
    fn fixture_derived_scores_match_expected_formula_math_and_ordering() {
        let files = vec![
            file_record("src/stable.rs", Some(19), Some(1)),
            file_record("src/risky.rs", Some(1_980), Some(100)),
            file_record("src/related.rs", Some(42), Some(2)),
        ];
        let git_metrics = vec![
            GitFileMetrics {
                path: "src/related.rs".to_owned(),
                commits_per_file: 2,
                total_churn_added: 2,
                total_churn_deleted: 0,
                recent_churn_added: 2,
                recent_churn_deleted: 0,
                author_count: 2,
                dominant_owner: Some("Ben Bitdiddle <ben@example.invalid>".to_owned()),
                dominant_owner_share: Some(0.5),
                first_commit_id: Some("b".repeat(40)),
                first_commit_time: Some(1_710_892_800),
                last_commit_id: Some("c".repeat(40)),
                last_commit_time: Some(1_712_707_200),
                file_age_days: Some(21),
            },
            GitFileMetrics {
                path: "src/risky.rs".to_owned(),
                commits_per_file: 3,
                total_churn_added: 100,
                total_churn_deleted: 0,
                recent_churn_added: 50,
                recent_churn_deleted: 0,
                author_count: 3,
                dominant_owner: Some("Ada Lovelace <ada@example.invalid>".to_owned()),
                dominant_owner_share: Some(1.0 / 3.0),
                first_commit_id: Some("a".repeat(40)),
                first_commit_time: Some(1_704_067_200),
                last_commit_id: Some("c".repeat(40)),
                last_commit_time: Some(1_712_707_200),
                file_age_days: Some(100),
            },
            GitFileMetrics {
                path: "src/stable.rs".to_owned(),
                commits_per_file: 1,
                total_churn_added: 1,
                total_churn_deleted: 0,
                recent_churn_added: 0,
                recent_churn_deleted: 0,
                author_count: 1,
                dominant_owner: Some("Ada Lovelace <ada@example.invalid>".to_owned()),
                dominant_owner_share: Some(1.0),
                first_commit_id: Some("a".repeat(40)),
                first_commit_time: Some(1_704_067_200),
                last_commit_id: Some("a".repeat(40)),
                last_commit_time: Some(1_704_067_200),
                file_age_days: Some(100),
            },
        ];
        let co_changes = vec![
            GitCoChange {
                left_path: "src/risky.rs".to_owned(),
                right_path: "src/stable.rs".to_owned(),
                commit_count: 1,
            },
            GitCoChange {
                left_path: "src/related.rs".to_owned(),
                right_path: "src/risky.rs".to_owned(),
                commit_count: 2,
            },
        ];

        let scores = raw_score_metrics_from_scan_and_git(&files, &git_metrics, &co_changes)
            .into_iter()
            .map(calculate_hotspot_score)
            .collect::<Vec<_>>();
        let related = score_for_path(&scores, "src/related.rs");
        let risky = score_for_path(&scores, "src/risky.rs");
        let stable = score_for_path(&scores, "src/stable.rs");

        assert_f64_near(related.value, 0.22475);
        assert_eq!(
            related.normalized_metrics,
            NormalizedScoreMetrics {
                size: Some(0.002),
                churn: Some(0.001),
                recent_churn: Some(1.0),
                ownership: Some(0.35),
                coupling: Some(0.04),
            }
        );
        assert_f64_near(risky.value, 0.22716666666666668);
        assert_eq!(risky.normalized_metrics.size, Some(0.1));
        assert_eq!(risky.normalized_metrics.churn, Some(0.05));
        assert_eq!(risky.normalized_metrics.recent_churn, Some(0.5));
        assert_near(risky.normalized_metrics.ownership, 0.5333333333333333);
        assert_eq!(risky.normalized_metrics.coupling, Some(0.08));
        assert_f64_near(stable.value, 0.004375);

        assert_eq!(
            ranked_paths(&rank_hotspot_scores(&scores)),
            vec!["src/risky.rs", "src/related.rs", "src/stable.rs"]
        );
    }

    fn score_with_value(path: &str, value: f64) -> HotspotScore {
        let mut score = calculate_hotspot_score(RawScoreMetrics {
            path: path.to_owned(),
            ..raw_metrics()
        });
        score.value = value;
        score
    }

    fn file_record(path: &str, byte_size: Option<u64>, line_count: Option<u64>) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size,
            extension: None,
            language: None,
            line_count,
            is_vendor: false,
            is_generated: false,
            content: crate::ContentKind::Text,
            is_symlink: false,
            classification: "implemented",
            warnings: Vec::new(),
        }
    }

    fn ranked_paths(ranked_scores: &[RankedHotspotScore]) -> Vec<String> {
        ranked_scores
            .iter()
            .map(|ranked_score| ranked_score.score.path.clone())
            .collect()
    }

    fn score_for_path<'a>(scores: &'a [HotspotScore], path: &str) -> &'a HotspotScore {
        scores
            .iter()
            .find(|score| score.path == path)
            .unwrap_or_else(|| panic!("expected score for {path}"))
    }

    fn raw_metrics() -> RawScoreMetrics {
        RawScoreMetrics {
            path: "src/lib.rs".to_owned(),
            byte_size: Some(120),
            line_count: Some(500),
            commits_per_file: Some(3),
            total_churn_lines: Some(42),
            recent_churn_lines: Some(12),
            author_count: Some(2),
            dominant_owner_share: Some(0.75),
            co_changed_file_count: Some(4),
        }
    }

    fn limitation_codes(limitations: &[ScoreLimitation]) -> Vec<&str> {
        limitations
            .iter()
            .map(|limitation| limitation.code.as_str())
            .collect()
    }

    fn assert_near(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("normalized metric should be present");
        assert_f64_near(actual, expected);
    }

    fn assert_f64_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {actual} to be near {expected}"
        );
    }
}
