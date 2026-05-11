// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

/// Current score formula identifier.
///
/// Formula identifiers are part of score explanations and persisted output.
/// Changing the formula semantics should introduce a new identifier.
pub const CURRENT_SCORE_FORMULA_ID: &str = "hotpath.score.v1";

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

/// One weighted normalized term in a score formula.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeightedTerm {
    /// Stable term name, for example `churn`.
    pub name: String,
    /// Normalized metric consumed by this term.
    pub metric: NormalizedMetric,
    /// Formula weight assigned to this term.
    pub weight: f64,
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
                weight: 0.4,
            },
            WeightedTerm {
                name: "coupling".to_owned(),
                metric: NormalizedMetric::Coupling,
                weight: 0.2,
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
}
