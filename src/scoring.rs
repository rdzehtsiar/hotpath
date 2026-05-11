// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

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
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {actual} to be near {expected}"
        );
    }
}
