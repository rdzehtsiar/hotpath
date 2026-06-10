// SPDX-License-Identifier: Apache-2.0

/// Scores file-level risk from merged intelligence facts and limitations.
#[derive(Debug, Default)]
pub struct FileRiskAssessor;

impl FileRiskAssessor {
    pub fn new() -> Self {
        Self
    }

    pub fn assess(
        &self,
        facts: &FileRiskInput,
        repository: &RepositoryRiskContext,
    ) -> FileRiskAssessment {
        let mut limitations = Vec::new();
        let size = normalize_size(facts, &mut limitations);
        let churn = normalized_u64(facts.total_churn_lines, 2_000);
        let recent_churn = normalize_recent_churn(facts, &mut limitations);
        let cochange_pressure = normalized_u64(facts.co_changed_file_count, 25);
        let source_coupling_pressure = normalize_source_coupling_pressure(facts);
        let complexity_pressure = normalize_complexity_pressure(facts, &mut limitations);
        let ownership_risk = normalize_ownership_risk(
            facts,
            repository,
            churn,
            recent_churn,
            cochange_pressure,
            size,
            &mut limitations,
        );

        let terms = vec![
            weighted_term(
                "churn",
                Some(facts.total_churn_lines as f64),
                churn,
                WEIGHT_CHURN,
            ),
            weighted_term(
                "recent_churn",
                Some(facts.recent_churn_lines as f64),
                recent_churn,
                WEIGHT_RECENT_CHURN,
            ),
            weighted_term("size", raw_size(facts), size, WEIGHT_SIZE),
            weighted_term(
                "ownership_risk",
                Some(ownership_risk),
                Some(ownership_risk),
                WEIGHT_OWNERSHIP,
            ),
            weighted_term(
                "cochange_pressure",
                Some(facts.co_changed_file_count as f64),
                cochange_pressure,
                WEIGHT_COCHANGE,
            ),
            weighted_term(
                "source_coupling_pressure",
                Some(
                    facts
                        .source_coupling_pressure_in
                        .unwrap_or_default()
                        .max(facts.source_coupling_pressure_out.unwrap_or_default())
                        as f64,
                ),
                source_coupling_pressure,
                WEIGHT_SOURCE_COUPLING_PRESSURE,
            ),
            weighted_term(
                "complexity_pressure",
                Some(facts.complexity_pressure.unwrap_or_default() as f64),
                complexity_pressure,
                WEIGHT_COMPLEXITY_PRESSURE,
            ),
        ];

        let score = terms
            .iter()
            .map(|term| term.contribution)
            .sum::<f64>()
            .clamp(0.0, 1.0);

        FileRiskAssessment {
            formula_id: FORMULA_ID,
            score,
            risk_10: score * 10.0,
            risk_band: risk_band(score),
            terms,
            limitations,
            facts: driver_facts(facts, score),
        }
    }
}

pub const FORMULA_ID: &str = "hotpath.score.go.v1";

const WEIGHT_CHURN: f64 = 0.18;
const WEIGHT_RECENT_CHURN: f64 = 0.14;
const WEIGHT_SIZE: f64 = 0.12;
const WEIGHT_OWNERSHIP: f64 = 0.14;
const WEIGHT_COCHANGE: f64 = 0.10;
const WEIGHT_SOURCE_COUPLING_PRESSURE: f64 = 0.16;
const WEIGHT_COMPLEXITY_PRESSURE: f64 = 0.16;

#[derive(Debug, Clone, PartialEq)]
pub struct FileRiskInput {
    pub relative_path: String,
    pub line_count: Option<u64>,
    pub byte_size: Option<u64>,
    pub total_churn_lines: u64,
    pub recent_churn_lines: u64,
    pub owner_count: Option<u64>,
    pub dominant_owner_share: Option<f64>,
    pub co_changed_file_count: u64,
    pub file_age_days: Option<u64>,
    pub source_coupling_pressure_in: Option<u64>,
    pub source_coupling_pressure_out: Option<u64>,
    pub complexity_pressure: Option<u64>,
    pub max_function_complexity_pressure: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryRiskContext {
    pub repository_age_days: Option<u64>,
    pub repository_author_count: Option<u64>,
    pub repository_file_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRiskAssessment {
    pub formula_id: &'static str,
    pub score: f64,
    pub risk_10: f64,
    pub risk_band: &'static str,
    pub terms: Vec<FileRiskTerm>,
    pub limitations: Vec<FileRiskLimitation>,
    pub facts: Vec<FileRiskFact>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRiskTerm {
    pub name: &'static str,
    pub raw_value: Option<f64>,
    pub normalized_value: Option<f64>,
    pub weight: f64,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRiskLimitation {
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRiskFact {
    pub kind: &'static str,
    pub message: String,
}

fn weighted_term(
    name: &'static str,
    raw_value: Option<f64>,
    normalized_value: Option<f64>,
    weight: f64,
) -> FileRiskTerm {
    FileRiskTerm {
        name,
        raw_value,
        normalized_value,
        weight,
        contribution: normalized_value.unwrap_or_default() * weight,
    }
}

fn normalize_size(facts: &FileRiskInput, limitations: &mut Vec<FileRiskLimitation>) -> Option<f64> {
    if let Some(line_count) = facts.line_count {
        return Some(normalized_u64(line_count, 1_000).unwrap_or_default());
    }
    if let Some(byte_size) = facts.byte_size {
        limitations.push(FileRiskLimitation {
            code: "size_uses_byte_size_fallback",
            message: "Line count is unavailable, so byte size is used for size normalization.",
        });
        return Some(normalized_u64(byte_size, 131_072).unwrap_or_default());
    }
    limitations.push(FileRiskLimitation {
        code: "missing_size_metric",
        message: "Neither line count nor byte size is available.",
    });
    None
}

fn normalize_recent_churn(
    facts: &FileRiskInput,
    limitations: &mut Vec<FileRiskLimitation>,
) -> Option<f64> {
    let Some(line_count) = facts.line_count else {
        limitations.push(FileRiskLimitation {
            code: "missing_recent_growth_line_count",
            message: "Line count is unavailable for recent churn normalization.",
        });
        return None;
    };
    if line_count == 0 {
        if facts.recent_churn_lines == 0 {
            return Some(0.0);
        }
        limitations.push(FileRiskLimitation {
            code: "zero_line_count_recent_growth",
            message: "Recent churn exists for a zero-line file, so recent churn saturates.",
        });
        return Some(1.0);
    }
    Some((facts.recent_churn_lines as f64 / line_count as f64).clamp(0.0, 1.0))
}

fn normalize_source_coupling_pressure(facts: &FileRiskInput) -> Option<f64> {
    let inbound = normalized_u64(facts.source_coupling_pressure_in.unwrap_or_default(), 25)?;
    let outbound = normalized_u64(facts.source_coupling_pressure_out.unwrap_or_default(), 15)?;
    Some(inbound.max(outbound))
}

fn normalize_complexity_pressure(
    facts: &FileRiskInput,
    limitations: &mut Vec<FileRiskLimitation>,
) -> Option<f64> {
    let file_complexity = facts.complexity_pressure;
    let function_complexity = facts.max_function_complexity_pressure;
    if file_complexity.is_none() && function_complexity.is_none() {
        limitations.push(FileRiskLimitation {
            code: "missing_complexity_pressure",
            message: "Parser-backed approximate complexity pressure is unavailable.",
        });
        return None;
    }
    let file_score = normalized_u64(file_complexity.unwrap_or_default(), 150).unwrap_or_default();
    let function_score =
        normalized_u64(function_complexity.unwrap_or_default(), 30).unwrap_or_default();
    Some(file_score.max(function_score))
}

fn normalize_ownership_risk(
    facts: &FileRiskInput,
    repository: &RepositoryRiskContext,
    churn: Option<f64>,
    recent_churn: Option<f64>,
    cochange_pressure: Option<f64>,
    size: Option<f64>,
    limitations: &mut Vec<FileRiskLimitation>,
) -> f64 {
    let owner_component = facts.owner_count.map(owner_count_component);
    let share_component = facts.dominant_owner_share.and_then(|share| {
        if !share.is_finite() {
            limitations.push(FileRiskLimitation {
                code: "invalid_dominant_owner_share",
                message: "Dominant owner share is not finite and is omitted.",
            });
            return None;
        }
        if !(0.0..=1.0).contains(&share) {
            limitations.push(FileRiskLimitation {
                code: "dominant_owner_share_out_of_range",
                message: "Dominant owner share is clamped to 0.0..=1.0.",
            });
        }
        Some(share.clamp(0.0, 1.0))
    });

    let base_concentration = match (owner_component, share_component) {
        (Some(owner), Some(share)) => (owner + share) / 2.0,
        (Some(owner), None) => {
            limitations.push(FileRiskLimitation {
                code: "ownership_risk_missing_owner_share",
                message: "Owner risk uses owner count only.",
            });
            owner
        }
        (None, Some(share)) => {
            limitations.push(FileRiskLimitation {
                code: "ownership_risk_missing_owner_count",
                message: "Owner risk uses dominant owner share only.",
            });
            share
        }
        (None, None) => {
            limitations.push(FileRiskLimitation {
                code: "missing_ownership_risk_metrics",
                message: "Neither owner count nor dominant owner share is available.",
            });
            return 0.0;
        }
    };

    let repository_maturity = repository_maturity(facts, repository, limitations);
    let file_pressure = 0.35 * churn.unwrap_or_default()
        + 0.25 * recent_churn.unwrap_or_default()
        + 0.25 * cochange_pressure.unwrap_or_default()
        + 0.15 * size.unwrap_or_default();

    (base_concentration * (0.20 + 0.80 * repository_maturity) * (0.30 + 0.70 * file_pressure))
        .clamp(0.0, 1.0)
}

fn repository_maturity(
    facts: &FileRiskInput,
    repository: &RepositoryRiskContext,
    limitations: &mut Vec<FileRiskLimitation>,
) -> f64 {
    let repository_age = match repository.repository_age_days {
        Some(days) => normalized_u64(days, 730).unwrap_or_default(),
        None => {
            limitations.push(FileRiskLimitation {
                code: "ownership_risk_missing_repository_age",
                message: "Repository age is unavailable, so maturity is lower.",
            });
            0.0
        }
    };
    let repository_authors = match repository.repository_author_count {
        Some(count) => ((count.saturating_sub(1)) as f64 / 9.0).clamp(0.0, 1.0),
        None => {
            limitations.push(FileRiskLimitation {
                code: "ownership_risk_missing_repository_author_count",
                message: "Repository author count is unavailable, so maturity is lower.",
            });
            0.0
        }
    };
    let repository_files =
        normalized_u64(repository.repository_file_count, 200).unwrap_or_default();
    let file_age = facts
        .file_age_days
        .and_then(|days| normalized_u64(days, 365))
        .unwrap_or_default();

    0.35 * repository_age + 0.35 * repository_authors + 0.15 * repository_files + 0.15 * file_age
}

fn owner_count_component(owner_count: u64) -> f64 {
    match owner_count {
        0 => 0.0,
        1 => 1.0,
        2 => 0.60,
        3 => 0.30,
        _ => 0.0,
    }
}

fn normalized_u64(value: u64, saturation: u64) -> Option<f64> {
    if saturation == 0 {
        return None;
    }
    Some((value as f64 / saturation as f64).clamp(0.0, 1.0))
}

fn raw_size(facts: &FileRiskInput) -> Option<f64> {
    facts
        .line_count
        .or(facts.byte_size)
        .map(|value| value as f64)
}

fn risk_band(score: f64) -> &'static str {
    if score >= 0.85 {
        "extreme"
    } else if score >= 0.70 {
        "high"
    } else if score >= 0.40 {
        "medium"
    } else {
        "low"
    }
}

fn driver_facts(facts: &FileRiskInput, score: f64) -> Vec<FileRiskFact> {
    let mut drivers = Vec::new();
    if facts.total_churn_lines >= 1_000 {
        drivers.push(FileRiskFact {
            kind: "high_churn",
            message: format!(
                "High total churn: {} changed lines",
                facts.total_churn_lines
            ),
        });
    }
    if facts.recent_churn_lines >= facts.line_count.unwrap_or(u64::MAX).max(1) {
        drivers.push(FileRiskFact {
            kind: "recent_churn",
            message: format!(
                "Recent churn is high: {} changed lines",
                facts.recent_churn_lines
            ),
        });
    }
    if facts
        .max_function_complexity_pressure
        .is_some_and(|complexity| complexity >= 20)
    {
        drivers.push(FileRiskFact {
            kind: "high_complexity_pressure",
            message: format!(
                "High approximate cognitive complexity pressure: max function {}",
                facts.max_function_complexity_pressure.unwrap_or_default()
            ),
        });
    }
    if facts.source_coupling_pressure_in.unwrap_or_default() >= 10
        || facts.source_coupling_pressure_out.unwrap_or_default() >= 10
    {
        drivers.push(FileRiskFact {
            kind: "source_coupling_pressure",
            message: format!(
                "High source coupling pressure: {} inbound resolved local imports, {} outbound resolved local imports",
                facts.source_coupling_pressure_in.unwrap_or_default(),
                facts.source_coupling_pressure_out.unwrap_or_default()
            ),
        });
    }
    if facts.co_changed_file_count >= 15 {
        drivers.push(FileRiskFact {
            kind: "cochange_pressure",
            message: format!(
                "High co-change pressure: {} files",
                facts.co_changed_file_count
            ),
        });
    }
    if facts
        .dominant_owner_share
        .is_some_and(|share| share >= 0.75)
    {
        drivers.push(FileRiskFact {
            kind: "ownership",
            message: format!(
                "High ownership concentration: dominant owner share {:.2}",
                facts.dominant_owner_share.unwrap_or_default()
            ),
        });
    }
    if drivers.is_empty() {
        drivers.push(FileRiskFact {
            kind: "summary",
            message: format!(
                "Advisory risk score {:.3} from local file, Git, source coupling pressure, and complexity pressure signals",
                score
            ),
        });
    }
    drivers
}

#[cfg(test)]
mod tests {
    use super::{FileRiskAssessor, FileRiskInput, RepositoryRiskContext, FORMULA_ID};

    #[test]
    fn high_metrics_produce_bounded_score() {
        let assessment = FileRiskAssessor::new().assess(
            &FileRiskInput {
                relative_path: "src/risky.go".to_owned(),
                line_count: Some(2_000),
                byte_size: Some(200_000),
                total_churn_lines: 10_000,
                recent_churn_lines: 3_000,
                owner_count: Some(1),
                dominant_owner_share: Some(0.95),
                co_changed_file_count: 100,
                file_age_days: Some(500),
                source_coupling_pressure_in: Some(40),
                source_coupling_pressure_out: Some(20),
                complexity_pressure: Some(500),
                max_function_complexity_pressure: Some(80),
            },
            &RepositoryRiskContext {
                repository_age_days: Some(1_000),
                repository_author_count: Some(20),
                repository_file_count: 500,
            },
        );

        assert_eq!(assessment.formula_id, FORMULA_ID);
        assert!((0.0..=1.0).contains(&assessment.score));
        assert_eq!(assessment.risk_band, "extreme");
        assert!(assessment
            .facts
            .iter()
            .any(|fact| fact.kind == "high_complexity_pressure"));
    }

    #[test]
    fn risky_go_file_explains_approximation_drivers() {
        let assessment = FileRiskAssessor::new().assess(
            &FileRiskInput {
                relative_path: "src/risky.go".to_owned(),
                line_count: Some(1_200),
                byte_size: Some(120_000),
                total_churn_lines: 2_500,
                recent_churn_lines: 1_300,
                owner_count: Some(1),
                dominant_owner_share: Some(0.82),
                co_changed_file_count: 4,
                file_age_days: Some(400),
                source_coupling_pressure_in: Some(12),
                source_coupling_pressure_out: Some(3),
                complexity_pressure: Some(180),
                max_function_complexity_pressure: Some(24),
            },
            &RepositoryRiskContext {
                repository_age_days: Some(600),
                repository_author_count: Some(8),
                repository_file_count: 120,
            },
        );

        let messages = assessment
            .facts
            .iter()
            .map(|fact| fact.message.as_str())
            .collect::<Vec<_>>();
        assert!(messages
            .iter()
            .any(|message| message.contains("High total churn")));
        assert!(messages
            .iter()
            .any(|message| message.contains("Recent churn is high")));
        assert!(messages
            .iter()
            .any(|message| message.contains("High approximate cognitive complexity pressure")));
        assert!(messages
            .iter()
            .any(|message| message.contains("resolved local imports")));
        assert!(messages
            .iter()
            .any(|message| message.contains("High ownership concentration")));
        assert!(assessment
            .terms
            .iter()
            .any(|term| term.name == "complexity_pressure"));
        assert!(assessment
            .terms
            .iter()
            .any(|term| term.name == "source_coupling_pressure"));
    }

    #[test]
    fn missing_inputs_create_limitations_and_zero_contributions() {
        let assessment = FileRiskAssessor::new().assess(
            &FileRiskInput {
                relative_path: "src/simple.go".to_owned(),
                line_count: None,
                byte_size: None,
                total_churn_lines: 0,
                recent_churn_lines: 0,
                owner_count: None,
                dominant_owner_share: None,
                co_changed_file_count: 0,
                file_age_days: None,
                source_coupling_pressure_in: None,
                source_coupling_pressure_out: None,
                complexity_pressure: None,
                max_function_complexity_pressure: None,
            },
            &RepositoryRiskContext::default(),
        );

        assert!(assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "missing_size_metric"));
        assert!(assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "missing_complexity_pressure"));
        assert!(assessment
            .terms
            .iter()
            .any(|term| term.name == "size" && term.contribution == 0.0));
    }

    #[test]
    fn byte_size_is_used_when_line_count_is_missing() {
        let assessment = FileRiskAssessor::new().assess(
            &FileRiskInput {
                relative_path: "src/binaryish.go".to_owned(),
                line_count: None,
                byte_size: Some(65_536),
                total_churn_lines: 0,
                recent_churn_lines: 0,
                owner_count: Some(2),
                dominant_owner_share: Some(0.5),
                co_changed_file_count: 0,
                file_age_days: Some(10),
                source_coupling_pressure_in: Some(0),
                source_coupling_pressure_out: Some(0),
                complexity_pressure: Some(0),
                max_function_complexity_pressure: Some(0),
            },
            &RepositoryRiskContext::default(),
        );

        let size = assessment
            .terms
            .iter()
            .find(|term| term.name == "size")
            .expect("size term should exist");
        assert_eq!(size.normalized_value, Some(0.5));
        assert!(assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "size_uses_byte_size_fallback"));
    }
}
