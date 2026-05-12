// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;

use serde::Serialize;

use crate::{ContentKind, FileRecord};

pub const CONTEXT_SCHEMA_VERSION: &str = "hotpath.context.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextOptions {
    pub exclude_generated: bool,
    pub exclude_vendor: bool,
    pub budget_tokens: Option<u64>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            exclude_generated: false,
            exclude_vendor: false,
            budget_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextReport {
    pub schema_version: &'static str,
    pub options: ContextOptions,
    pub summary: ContextSummary,
    pub groups: Vec<ContextGroupRow>,
    pub skipped: Vec<ContextSkippedRow>,
    pub budget: Option<ContextBudgetStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextSummary {
    pub total_files: u64,
    pub included_files: u64,
    pub skipped_files: u64,
    pub estimated_tokens: u64,
    pub included_bytes: u64,
    pub filtered_generated_files: u64,
    pub filtered_vendor_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextGroupRow {
    pub path: String,
    pub file_count: u64,
    pub byte_size: u64,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextSkippedRow {
    pub path: String,
    pub reason: ContextSkippedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSkippedReason {
    Binary,
    UnknownContent,
    MissingByteSize,
    Unreadable,
    ExcludedGenerated,
    ExcludedVendor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextBudgetStatus {
    pub budget_tokens: u64,
    pub estimated_tokens: u64,
    pub remaining_tokens: Option<u64>,
    pub over_budget_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetParseError {
    Empty,
    Zero,
    Negative,
    Decimal,
    UnknownSuffix,
    InvalidDigits,
    Overflow,
}

impl fmt::Display for BudgetParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "budget must not be empty"),
            Self::Zero => write!(f, "budget must be a positive integer"),
            Self::Negative => write!(f, "budget must not be negative"),
            Self::Decimal => write!(f, "budget must be a whole number"),
            Self::UnknownSuffix => {
                write!(f, "budget suffix must be omitted or one of k, m")
            }
            Self::InvalidDigits => write!(f, "budget must contain only digits before any suffix"),
            Self::Overflow => write!(f, "budget is too large"),
        }
    }
}

impl StdError for BudgetParseError {}

pub fn estimate_context(files: &[FileRecord], options: ContextOptions) -> ContextReport {
    let mut groups = BTreeMap::<String, ContextGroupRow>::new();
    let mut skipped = Vec::new();
    let mut summary = ContextSummary {
        total_files: files.len() as u64,
        included_files: 0,
        skipped_files: 0,
        estimated_tokens: 0,
        included_bytes: 0,
        filtered_generated_files: 0,
        filtered_vendor_files: 0,
    };

    for file in files {
        if options.exclude_generated && file.is_generated {
            summary.filtered_generated_files += 1;
            push_skipped(&mut skipped, file, ContextSkippedReason::ExcludedGenerated);
            continue;
        }

        if options.exclude_vendor && file.is_vendor {
            summary.filtered_vendor_files += 1;
            push_skipped(&mut skipped, file, ContextSkippedReason::ExcludedVendor);
            continue;
        }

        let byte_size = match included_byte_size(file) {
            Ok(byte_size) => byte_size,
            Err(reason) => {
                push_skipped(&mut skipped, file, reason);
                continue;
            }
        };
        let estimated_tokens = estimate_tokens(byte_size);
        let group_path = context_group_path(&file.path);
        let group = groups.entry(group_path.clone()).or_insert(ContextGroupRow {
            path: group_path,
            file_count: 0,
            byte_size: 0,
            estimated_tokens: 0,
        });

        group.file_count += 1;
        group.byte_size = group.byte_size.saturating_add(byte_size);
        group.estimated_tokens = group.estimated_tokens.saturating_add(estimated_tokens);
        summary.included_files += 1;
        summary.included_bytes = summary.included_bytes.saturating_add(byte_size);
        summary.estimated_tokens = summary.estimated_tokens.saturating_add(estimated_tokens);
    }

    summary.skipped_files = skipped.len() as u64;
    skipped.sort_by(|left, right| left.path.cmp(&right.path));
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .estimated_tokens
            .cmp(&left.estimated_tokens)
            .then_with(|| left.path.cmp(&right.path))
    });

    ContextReport {
        schema_version: CONTEXT_SCHEMA_VERSION,
        options,
        budget: options
            .budget_tokens
            .map(|budget_tokens| budget_status(budget_tokens, summary.estimated_tokens)),
        summary,
        groups,
        skipped,
    }
}

pub fn parse_budget_tokens(input: &str) -> Result<u64, BudgetParseError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(BudgetParseError::Empty);
    }

    if input.starts_with('-') {
        return Err(BudgetParseError::Negative);
    }

    if input.contains('.') {
        return Err(BudgetParseError::Decimal);
    }

    let (digits, multiplier) = match input.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&input[..input.len() - 1], 1_000_u64),
        Some(b'm' | b'M') => (&input[..input.len() - 1], 1_000_000_u64),
        Some(byte) if byte.is_ascii_digit() => (input, 1_u64),
        Some(_) => return Err(BudgetParseError::UnknownSuffix),
        None => return Err(BudgetParseError::Empty),
    };

    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BudgetParseError::InvalidDigits);
    }

    let value = digits
        .parse::<u64>()
        .map_err(|_| BudgetParseError::Overflow)?;
    if value == 0 {
        return Err(BudgetParseError::Zero);
    }

    value
        .checked_mul(multiplier)
        .ok_or(BudgetParseError::Overflow)
}

fn included_byte_size(file: &FileRecord) -> Result<u64, ContextSkippedReason> {
    if has_unreadable_warning(file) {
        return Err(ContextSkippedReason::Unreadable);
    }

    match file.content {
        ContentKind::Text => file.byte_size.ok_or(ContextSkippedReason::MissingByteSize),
        ContentKind::Binary => Err(ContextSkippedReason::Binary),
        ContentKind::Unknown => Err(ContextSkippedReason::UnknownContent),
    }
}

fn estimate_tokens(byte_size: u64) -> u64 {
    byte_size / 4 + u64::from(byte_size % 4 != 0)
}

fn context_group_path(path: &str) -> String {
    path.split('/')
        .next()
        .filter(|component| !component.is_empty() && *component != path)
        .unwrap_or(".")
        .to_owned()
}

fn push_skipped(
    skipped: &mut Vec<ContextSkippedRow>,
    file: &FileRecord,
    reason: ContextSkippedReason,
) {
    skipped.push(ContextSkippedRow {
        path: file.path.clone(),
        reason,
    });
}

fn budget_status(budget_tokens: u64, estimated_tokens: u64) -> ContextBudgetStatus {
    ContextBudgetStatus {
        budget_tokens,
        estimated_tokens,
        remaining_tokens: budget_tokens.checked_sub(estimated_tokens),
        over_budget_tokens: estimated_tokens.checked_sub(budget_tokens),
    }
}

fn has_unreadable_warning(file: &FileRecord) -> bool {
    file.warnings
        .iter()
        .any(|warning| matches!(warning.code, "read_failed" | "metadata_failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileWarning;

    fn text(path: &str, byte_size: Option<u64>) -> FileRecord {
        record(path, byte_size, ContentKind::Text)
    }

    fn record(path: &str, byte_size: Option<u64>, content: ContentKind) -> FileRecord {
        FileRecord {
            path: path.to_owned(),
            byte_size,
            extension: None,
            language: None,
            line_count: None,
            is_vendor: false,
            is_generated: false,
            content,
            is_symlink: false,
            classification: "test",
            warnings: Vec::new(),
        }
    }

    fn group_paths(report: &ContextReport) -> Vec<&str> {
        report
            .groups
            .iter()
            .map(|group| group.path.as_str())
            .collect()
    }

    #[test]
    fn estimator_uses_ceiling_byte_size_divided_by_four() {
        let report = estimate_context(
            &[
                text("one.txt", Some(1)),
                text("four.txt", Some(4)),
                text("five.txt", Some(5)),
            ],
            ContextOptions::default(),
        );

        assert_eq!(report.schema_version, CONTEXT_SCHEMA_VERSION);
        assert_eq!(report.summary.included_files, 3);
        assert_eq!(report.summary.included_bytes, 10);
        assert_eq!(report.summary.estimated_tokens, 4);
        assert_eq!(report.groups[0].path, ".");
        assert_eq!(report.groups[0].estimated_tokens, 4);
    }

    #[test]
    fn groups_by_first_path_component_and_sorts_deterministically() {
        let report = estimate_context(
            &[
                text("zeta/file.rs", Some(8)),
                text("alpha/file.rs", Some(8)),
                text("src/compiler/lib.rs", Some(12)),
                text("README.md", Some(4)),
            ],
            ContextOptions::default(),
        );

        assert_eq!(group_paths(&report), vec!["src", "alpha", "zeta", "."]);
        assert_eq!(report.groups[0].estimated_tokens, 3);
        assert_eq!(report.groups[1].estimated_tokens, 2);
        assert_eq!(report.groups[2].estimated_tokens, 2);
        assert_eq!(report.groups[3].estimated_tokens, 1);
    }

    #[test]
    fn generated_and_vendor_files_are_included_by_default() {
        let mut generated = text("dist/client.js", Some(8));
        generated.is_generated = true;
        let mut vendor = text("vendor/lib.rs", Some(12));
        vendor.is_vendor = true;

        let report = estimate_context(&[generated, vendor], ContextOptions::default());

        assert_eq!(report.summary.included_files, 2);
        assert_eq!(report.summary.skipped_files, 0);
        assert_eq!(report.summary.estimated_tokens, 5);
        assert_eq!(group_paths(&report), vec!["vendor", "dist"]);
    }

    #[test]
    fn generated_and_vendor_filters_skip_matching_files() {
        let mut generated = text("dist/client.js", Some(8));
        generated.is_generated = true;
        let mut vendor = text("vendor/lib.rs", Some(12));
        vendor.is_vendor = true;
        let options = ContextOptions {
            exclude_generated: true,
            exclude_vendor: true,
            budget_tokens: None,
        };

        let report = estimate_context(&[generated, vendor, text("src/lib.rs", Some(4))], options);

        assert_eq!(report.summary.included_files, 1);
        assert_eq!(report.summary.skipped_files, 2);
        assert_eq!(report.summary.filtered_generated_files, 1);
        assert_eq!(report.summary.filtered_vendor_files, 1);
        assert_eq!(report.summary.estimated_tokens, 1);
        assert_eq!(
            report.skipped,
            vec![
                ContextSkippedRow {
                    path: "dist/client.js".to_owned(),
                    reason: ContextSkippedReason::ExcludedGenerated,
                },
                ContextSkippedRow {
                    path: "vendor/lib.rs".to_owned(),
                    reason: ContextSkippedReason::ExcludedVendor,
                },
            ]
        );
    }

    #[test]
    fn skips_binary_unknown_unreadable_and_missing_byte_size_files() {
        let mut unreadable = text("blocked.txt", Some(16));
        unreadable.warnings.push(FileWarning {
            code: "read_failed",
            message: "denied".to_owned(),
        });

        let report = estimate_context(
            &[
                record("assets/logo.bin", Some(10), ContentKind::Binary),
                text("missing.txt", None),
                record("unknown.dat", None, ContentKind::Unknown),
                unreadable,
                text("src/lib.rs", Some(4)),
            ],
            ContextOptions::default(),
        );

        assert_eq!(report.summary.included_files, 1);
        assert_eq!(report.summary.skipped_files, 4);
        assert_eq!(report.summary.estimated_tokens, 1);
        assert_eq!(
            report.skipped,
            vec![
                ContextSkippedRow {
                    path: "assets/logo.bin".to_owned(),
                    reason: ContextSkippedReason::Binary,
                },
                ContextSkippedRow {
                    path: "blocked.txt".to_owned(),
                    reason: ContextSkippedReason::Unreadable,
                },
                ContextSkippedRow {
                    path: "missing.txt".to_owned(),
                    reason: ContextSkippedReason::MissingByteSize,
                },
                ContextSkippedRow {
                    path: "unknown.dat".to_owned(),
                    reason: ContextSkippedReason::UnknownContent,
                },
            ]
        );
    }

    #[test]
    fn reports_budget_status_when_budget_is_provided() {
        let report = estimate_context(
            &[text("src/lib.rs", Some(12))],
            ContextOptions {
                exclude_generated: false,
                exclude_vendor: false,
                budget_tokens: Some(2),
            },
        );

        assert_eq!(
            report.budget,
            Some(ContextBudgetStatus {
                budget_tokens: 2,
                estimated_tokens: 3,
                remaining_tokens: None,
                over_budget_tokens: Some(1),
            })
        );
    }

    #[test]
    fn parses_plain_and_suffixed_positive_integer_budgets() {
        assert_eq!(parse_budget_tokens("1"), Ok(1));
        assert_eq!(parse_budget_tokens("200k"), Ok(200_000));
        assert_eq!(parse_budget_tokens("3K"), Ok(3_000));
        assert_eq!(parse_budget_tokens("2m"), Ok(2_000_000));
        assert_eq!(parse_budget_tokens("4M"), Ok(4_000_000));
    }

    #[test]
    fn rejects_invalid_budgets() {
        assert_eq!(parse_budget_tokens(""), Err(BudgetParseError::Empty));
        assert_eq!(parse_budget_tokens("0"), Err(BudgetParseError::Zero));
        assert_eq!(parse_budget_tokens("-1"), Err(BudgetParseError::Negative));
        assert_eq!(parse_budget_tokens("1.5k"), Err(BudgetParseError::Decimal));
        assert_eq!(
            parse_budget_tokens("10g"),
            Err(BudgetParseError::UnknownSuffix)
        );
        assert_eq!(
            parse_budget_tokens("abc"),
            Err(BudgetParseError::UnknownSuffix)
        );
        assert_eq!(
            parse_budget_tokens("k"),
            Err(BudgetParseError::InvalidDigits)
        );
        assert_eq!(
            parse_budget_tokens("18446744073709551616"),
            Err(BudgetParseError::Overflow)
        );
        assert_eq!(
            parse_budget_tokens("18446744073709552k"),
            Err(BudgetParseError::Overflow)
        );
    }
}
