// SPDX-License-Identifier: Apache-2.0

/// Derives repository-level risk from file scores and repository-wide signals.
#[derive(Debug, Default)]
pub struct RepoRiskAssessor;

impl RepoRiskAssessor {
    pub fn new() -> Self {
        Self
    }

    pub fn assess(&self, input: &RepoRiskInput) -> RepoRiskAssessment {
        let mut files = input.files.clone();
        files.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });

        let scored_file_count = files.len() as u64;
        let scoring_coverage = ratio(scored_file_count, input.active_file_count);
        let go_score_coverage = if input.active_go_file_count == 0 {
            None
        } else {
            Some(scored_file_count as f64 / input.active_go_file_count as f64)
        };
        let confidence = confidence(scoring_coverage);

        let mut limitations = Vec::new();
        if scored_file_count == 0 {
            limitations.push(ProjectRiskLimitation {
                code: "no_scored_files",
                message: "No Go file risk scores are available.",
            });
        }
        if scored_file_count > 0 && matches!(confidence, "low" | "none") {
            limitations.push(ProjectRiskLimitation {
                code: "low_language_coverage",
                message: "Only a small share of active files has Go risk scores.",
            });
        }
        if scored_file_count > 0 && go_score_coverage.is_some_and(|coverage| coverage < 1.0) {
            limitations.push(ProjectRiskLimitation {
                code: "partial_go_score_coverage",
                message: "Some active Go files do not have persisted risk scores.",
            });
        }
        if input.git_index_status != "available" {
            limitations.push(ProjectRiskLimitation {
                code: "git_index_unavailable",
                message: "Git-derived repository context is unavailable.",
            });
        }

        if scored_file_count == 0 {
            return RepoRiskAssessment {
                formula_id: FORMULA_ID,
                score: 0.0,
                risk_10: 0.0,
                risk_band: "unavailable",
                confidence,
                active_file_count: input.active_file_count,
                active_go_file_count: input.active_go_file_count,
                scored_file_count,
                scoring_coverage,
                go_score_coverage,
                max_file_score: 0.0,
                top_10_mean_score: 0.0,
                high_risk_file_count: 0,
                medium_risk_file_count: 0,
                dominant_dimension: None,
                dominant_dimension_pressure: 0.0,
                git_index_status: input.git_index_status.clone(),
                terms: unavailable_terms(),
                limitations,
                facts: vec![ProjectRiskFact {
                    kind: "summary",
                    message: "No Go file risk scores are available for project risk.".to_owned(),
                }],
            };
        }

        let top_files = files.iter().take(10).collect::<Vec<_>>();
        let max_file_score = files.first().map(|file| file.score).unwrap_or_default();
        let top_10_mean_score =
            top_files.iter().map(|file| file.score).sum::<f64>() / top_files.len() as f64;
        let high_risk_file_count = files.iter().filter(|file| file.score >= 0.70).count() as u64;
        let medium_risk_file_count = files.iter().filter(|file| file.score >= 0.40).count() as u64;
        let high_risk_share_pressure =
            ((high_risk_file_count as f64 / scored_file_count as f64) / 0.10).clamp(0.0, 1.0);
        let medium_risk_share_pressure =
            ((medium_risk_file_count as f64 / scored_file_count as f64) / 0.30).clamp(0.0, 1.0);
        let (dominant_dimension, dominant_dimension_pressure) =
            dominant_dimension(&top_files, &input.terms);

        let terms = vec![
            project_term(
                "max_file_score",
                max_file_score,
                max_file_score,
                WEIGHT_MAX_FILE,
            ),
            project_term(
                "top_10_mean_score",
                top_10_mean_score,
                top_10_mean_score,
                WEIGHT_TOP_10_MEAN,
            ),
            project_term(
                "high_risk_share_pressure",
                high_risk_file_count as f64,
                high_risk_share_pressure,
                WEIGHT_HIGH_RISK_SHARE,
            ),
            project_term(
                "medium_risk_share_pressure",
                medium_risk_file_count as f64,
                medium_risk_share_pressure,
                WEIGHT_MEDIUM_RISK_SHARE,
            ),
            project_term(
                "dominant_dimension_pressure",
                dominant_dimension_pressure,
                dominant_dimension_pressure,
                WEIGHT_DOMINANT_DIMENSION,
            ),
        ];
        let score = terms
            .iter()
            .map(|term| term.contribution)
            .sum::<f64>()
            .clamp(0.0, 1.0);

        RepoRiskAssessment {
            formula_id: FORMULA_ID,
            score,
            risk_10: score * 10.0,
            risk_band: risk_band(score),
            confidence,
            active_file_count: input.active_file_count,
            active_go_file_count: input.active_go_file_count,
            scored_file_count,
            scoring_coverage,
            go_score_coverage,
            max_file_score,
            top_10_mean_score,
            high_risk_file_count,
            medium_risk_file_count,
            dominant_dimension: dominant_dimension.clone(),
            dominant_dimension_pressure,
            git_index_status: input.git_index_status.clone(),
            terms,
            limitations,
            facts: project_facts(&files, dominant_dimension.as_deref(), score),
        }
    }
}

pub const FORMULA_ID: &str = "hotpath.project_risk.go.v1";

const WEIGHT_MAX_FILE: f64 = 0.35;
const WEIGHT_TOP_10_MEAN: f64 = 0.25;
const WEIGHT_HIGH_RISK_SHARE: f64 = 0.20;
const WEIGHT_MEDIUM_RISK_SHARE: f64 = 0.10;
const WEIGHT_DOMINANT_DIMENSION: f64 = 0.10;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoRiskInput {
    pub active_file_count: u64,
    pub active_go_file_count: u64,
    pub git_index_status: String,
    pub files: Vec<ProjectFileRiskInput>,
    pub terms: Vec<ProjectFileRiskTermInput>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFileRiskInput {
    pub relative_path: String,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFileRiskTermInput {
    pub relative_path: String,
    pub term_name: String,
    pub normalized_value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRiskAssessment {
    pub formula_id: &'static str,
    pub score: f64,
    pub risk_10: f64,
    pub risk_band: &'static str,
    pub confidence: &'static str,
    pub active_file_count: u64,
    pub active_go_file_count: u64,
    pub scored_file_count: u64,
    pub scoring_coverage: f64,
    pub go_score_coverage: Option<f64>,
    pub max_file_score: f64,
    pub top_10_mean_score: f64,
    pub high_risk_file_count: u64,
    pub medium_risk_file_count: u64,
    pub dominant_dimension: Option<String>,
    pub dominant_dimension_pressure: f64,
    pub git_index_status: String,
    pub terms: Vec<ProjectRiskTerm>,
    pub limitations: Vec<ProjectRiskLimitation>,
    pub facts: Vec<ProjectRiskFact>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRiskTerm {
    pub name: &'static str,
    pub raw_value: f64,
    pub normalized_value: f64,
    pub weight: f64,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRiskLimitation {
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRiskFact {
    pub kind: &'static str,
    pub message: String,
}

fn project_term(
    name: &'static str,
    raw_value: f64,
    normalized_value: f64,
    weight: f64,
) -> ProjectRiskTerm {
    ProjectRiskTerm {
        name,
        raw_value,
        normalized_value: normalized_value.clamp(0.0, 1.0),
        weight,
        contribution: normalized_value.clamp(0.0, 1.0) * weight,
    }
}

fn unavailable_terms() -> Vec<ProjectRiskTerm> {
    vec![
        project_term("max_file_score", 0.0, 0.0, WEIGHT_MAX_FILE),
        project_term("top_10_mean_score", 0.0, 0.0, WEIGHT_TOP_10_MEAN),
        project_term("high_risk_share_pressure", 0.0, 0.0, WEIGHT_HIGH_RISK_SHARE),
        project_term(
            "medium_risk_share_pressure",
            0.0,
            0.0,
            WEIGHT_MEDIUM_RISK_SHARE,
        ),
        project_term(
            "dominant_dimension_pressure",
            0.0,
            0.0,
            WEIGHT_DOMINANT_DIMENSION,
        ),
    ]
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64).clamp(0.0, 1.0)
    }
}

fn confidence(scoring_coverage: f64) -> &'static str {
    if scoring_coverage >= 0.80 {
        "high"
    } else if scoring_coverage >= 0.30 {
        "medium"
    } else if scoring_coverage > 0.0 {
        "low"
    } else {
        "none"
    }
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

fn dominant_dimension(
    top_files: &[&ProjectFileRiskInput],
    terms: &[ProjectFileRiskTermInput],
) -> (Option<String>, f64) {
    let top_paths = top_files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut by_term = std::collections::BTreeMap::<String, (f64, u64)>::new();
    for term in terms
        .iter()
        .filter(|term| top_paths.contains(term.relative_path.as_str()))
    {
        let entry = by_term.entry(term.term_name.clone()).or_default();
        entry.0 += term.normalized_value.unwrap_or_default().clamp(0.0, 1.0);
        entry.1 += 1;
    }

    by_term
        .into_iter()
        .filter_map(|(term, (sum, count))| {
            if count == 0 {
                None
            } else {
                Some((term, sum / count as f64))
            }
        })
        .max_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.0.cmp(&left.0))
        })
        .map(|(term, value)| (Some(term), value.clamp(0.0, 1.0)))
        .unwrap_or((None, 0.0))
}

fn project_facts(
    files: &[ProjectFileRiskInput],
    dominant_dimension: Option<&str>,
    score: f64,
) -> Vec<ProjectRiskFact> {
    let mut facts = Vec::new();
    if let Some(top) = files.first() {
        facts.push(ProjectRiskFact {
            kind: "top_file",
            message: format!(
                "Highest-risk file is {} at {:.3}",
                top.relative_path, top.score
            ),
        });
    }
    if let Some(dominant_dimension) = dominant_dimension {
        facts.push(ProjectRiskFact {
            kind: "dominant_dimension",
            message: format!("Dominant risk dimension among top files is {dominant_dimension}"),
        });
    }
    facts.push(ProjectRiskFact {
        kind: "summary",
        message: format!("Project advisory risk score is {score:.3}"),
    });
    facts
}

#[cfg(test)]
mod tests {
    use super::{
        ProjectFileRiskInput, ProjectFileRiskTermInput, RepoRiskAssessor, RepoRiskInput, FORMULA_ID,
    };

    #[test]
    fn empty_input_returns_unavailable_summary() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 10,
            active_go_file_count: 0,
            git_index_status: "unavailable".to_owned(),
            files: Vec::new(),
            terms: Vec::new(),
        });

        assert_eq!(assessment.formula_id, FORMULA_ID);
        assert_eq!(assessment.score, 0.0);
        assert_eq!(assessment.risk_band, "unavailable");
        assert_eq!(assessment.confidence, "none");
        assert!(assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "no_scored_files"));
        assert!(!assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "low_language_coverage"));
    }

    #[test]
    fn hybrid_terms_are_normalized_and_bounded() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 10,
            active_go_file_count: 3,
            git_index_status: "available".to_owned(),
            files: vec![file("b.go", 0.8), file("a.go", 0.8), file("c.go", 0.2)],
            terms: vec![
                term("a.go", "churn", 0.5),
                term("b.go", "churn", 1.0),
                term("c.go", "size", 0.2),
            ],
        });

        assert!((0.0..=1.0).contains(&assessment.score));
        assert_eq!(assessment.max_file_score, 0.8);
        assert_eq!(assessment.high_risk_file_count, 2);
        assert_eq!(assessment.medium_risk_file_count, 2);
        assert_eq!(assessment.confidence, "medium");
        assert_eq!(
            assessment.facts[0].message,
            "Highest-risk file is a.go at 0.800"
        );
    }

    #[test]
    fn confidence_tracks_scoring_coverage() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 100,
            active_go_file_count: 1,
            git_index_status: "available".to_owned(),
            files: vec![file("a.go", 0.3)],
            terms: Vec::new(),
        });

        assert_eq!(assessment.confidence, "low");
        assert_eq!(assessment.scored_file_count, 1);
        assert_eq!(assessment.go_score_coverage, Some(1.0));
        assert!(assessment
            .limitations
            .iter()
            .any(|limitation| limitation.code == "low_language_coverage"));
    }

    #[test]
    fn limitations_are_direct_for_zero_scored_files() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 3,
            active_go_file_count: 2,
            git_index_status: "available".to_owned(),
            files: Vec::new(),
            terms: Vec::new(),
        });

        assert_eq!(limitation_codes(&assessment), vec!["no_scored_files"]);
        assert_eq!(assessment.confidence, "none");
    }

    #[test]
    fn limitations_warn_for_partial_scored_files() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 10,
            active_go_file_count: 2,
            git_index_status: "available".to_owned(),
            files: vec![file("a.go", 0.3)],
            terms: Vec::new(),
        });

        assert_eq!(
            limitation_codes(&assessment),
            vec!["low_language_coverage", "partial_go_score_coverage"]
        );
        assert_eq!(assessment.confidence, "low");
        assert_eq!(assessment.go_score_coverage, Some(0.5));
    }

    #[test]
    fn limitations_are_empty_for_healthy_go_scoring_coverage() {
        let assessment = RepoRiskAssessor::new().assess(&RepoRiskInput {
            active_file_count: 3,
            active_go_file_count: 3,
            git_index_status: "available".to_owned(),
            files: vec![file("a.go", 0.3), file("b.go", 0.4), file("c.go", 0.5)],
            terms: Vec::new(),
        });

        assert_eq!(limitation_codes(&assessment), Vec::<&str>::new());
        assert_eq!(assessment.confidence, "high");
        assert_eq!(assessment.go_score_coverage, Some(1.0));
    }

    fn limitation_codes(assessment: &super::RepoRiskAssessment) -> Vec<&'static str> {
        assessment
            .limitations
            .iter()
            .map(|limitation| limitation.code)
            .collect()
    }

    fn file(relative_path: &str, score: f64) -> ProjectFileRiskInput {
        ProjectFileRiskInput {
            relative_path: relative_path.to_owned(),
            score,
        }
    }

    fn term(
        relative_path: &str,
        term_name: &str,
        normalized_value: f64,
    ) -> ProjectFileRiskTermInput {
        ProjectFileRiskTermInput {
            relative_path: relative_path.to_owned(),
            term_name: term_name.to_owned(),
            normalized_value: Some(normalized_value),
        }
    }
}
