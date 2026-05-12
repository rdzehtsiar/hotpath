// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{ParseFileStatus, ParseReport};

pub const COMPLEXITY_SCHEMA_VERSION: &str = "hotpath.complexity.v1";
const LARGE_SYMBOL_LINE_THRESHOLD: u64 = 80;
const DEFAULT_RANKED_SYMBOL_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComplexitySummary {
    pub total_files: u64,
    pub parsed_files: u64,
    pub symbol_count: u64,
    pub function_method_count: u64,
    pub large_symbol_count: u64,
    pub max_cyclomatic_complexity: u64,
    pub max_nesting_depth: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComplexityFileRecord {
    pub path: String,
    pub language: Option<&'static str>,
    pub status: ParseFileStatus,
    pub symbol_count: u64,
    pub function_method_count: u64,
    pub large_symbol_count: u64,
    pub max_cyclomatic_complexity: Option<u64>,
    pub max_nesting_depth: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComplexitySymbolRecord {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: u64,
    pub end_line: u64,
    pub length_lines: u64,
    pub function_length_lines: Option<u64>,
    pub nesting_depth: u64,
    pub cyclomatic_complexity: Option<u64>,
    pub max_control_flow_nesting: Option<u64>,
    pub is_large_symbol: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComplexityReport {
    pub summary: ComplexitySummary,
    pub files: Vec<ComplexityFileRecord>,
    pub symbols: Vec<ComplexitySymbolRecord>,
}

#[derive(Debug, Serialize)]
struct ComplexityJsonReport<'a> {
    schema_version: &'static str,
    summary: &'a ComplexitySummary,
    files: &'a [ComplexityFileRecord],
    symbols: &'a [ComplexitySymbolRecord],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RankedSymbolKey<'a> {
    cyclomatic_complexity: u64,
    max_control_flow_nesting: u64,
    length_lines: u64,
    path: &'a str,
    start_line: u64,
    kind: &'a str,
    name: &'a str,
}

pub fn report_from_parse(report: &ParseReport) -> ComplexityReport {
    let parse_summary = report.summary();
    let mut files = report
        .files
        .iter()
        .map(|file| ComplexityFileRecord {
            path: file.path.clone(),
            language: file.language,
            status: file.status,
            symbol_count: file.symbol_count,
            function_method_count: 0,
            large_symbol_count: 0,
            max_cyclomatic_complexity: None,
            max_nesting_depth: None,
        })
        .collect::<Vec<_>>();
    let file_positions = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let symbols = report
        .symbols
        .iter()
        .map(|symbol| {
            let length_lines = symbol.end_line - symbol.start_line + 1;
            let function_length_lines = is_function_or_method(&symbol.kind).then_some(length_lines);
            let max_control_flow_nesting = symbol.max_control_flow_nesting;
            let cyclomatic_complexity = symbol.cyclomatic_complexity;
            let is_large_symbol = length_lines >= LARGE_SYMBOL_LINE_THRESHOLD;

            if let Some(file_index) = file_positions.get(symbol.path.as_str()).copied() {
                let file = &mut files[file_index];
                file.large_symbol_count += u64::from(is_large_symbol);

                if function_length_lines.is_some() {
                    file.function_method_count += 1;
                    file.max_cyclomatic_complexity =
                        max_option(file.max_cyclomatic_complexity, cyclomatic_complexity);
                    file.max_nesting_depth =
                        max_option(file.max_nesting_depth, max_control_flow_nesting);
                }
            }

            ComplexitySymbolRecord {
                path: symbol.path.clone(),
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
                length_lines,
                function_length_lines,
                nesting_depth: symbol.nesting_depth,
                cyclomatic_complexity,
                max_control_flow_nesting,
                is_large_symbol,
            }
        })
        .collect::<Vec<_>>();

    let function_method_count = symbols
        .iter()
        .filter(|symbol| symbol.function_length_lines.is_some())
        .count() as u64;
    let large_symbol_count = symbols
        .iter()
        .filter(|symbol| symbol.is_large_symbol)
        .count() as u64;
    let max_cyclomatic_complexity = symbols
        .iter()
        .filter_map(|symbol| symbol.cyclomatic_complexity)
        .max()
        .unwrap_or(0);
    let max_nesting_depth = symbols
        .iter()
        .filter_map(|symbol| symbol.max_control_flow_nesting)
        .max()
        .unwrap_or(0);

    ComplexityReport {
        summary: ComplexitySummary {
            total_files: parse_summary.total_files,
            parsed_files: parse_summary.parsed_files,
            symbol_count: parse_summary.symbol_count,
            function_method_count,
            large_symbol_count,
            max_cyclomatic_complexity,
            max_nesting_depth,
        },
        files,
        symbols,
    }
}

pub fn render_json(report: &ComplexityReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&ComplexityJsonReport {
        schema_version: COMPLEXITY_SCHEMA_VERSION,
        summary: &report.summary,
        files: &report.files,
        symbols: &report.symbols,
    })
}

pub fn render_summary(report: &ComplexityReport) -> String {
    let mut summary = format!(
        "Hotpath complexity summary\n{:<17}  {}\n{:<17}  {}\n{:<17}  {}\n{:<17}  {}\n{:<17}  {}\n{:<17}  {}\n{:<17}  {}",
        "total files",
        report.summary.total_files,
        "parsed files",
        report.summary.parsed_files,
        "symbols",
        report.summary.symbol_count,
        "functions/methods",
        report.summary.function_method_count,
        "large symbols",
        report.summary.large_symbol_count,
        "max cyclomatic",
        report.summary.max_cyclomatic_complexity,
        "max nesting",
        report.summary.max_nesting_depth
    );

    summary.push_str("\n\nmost complex function/method symbols");
    summary.push_str("\nrank  cyclo  nesting  lines  location  kind  name");

    let ranked = ranked_function_symbols(&report.symbols);
    if ranked.is_empty() {
        summary.push_str("\n  none");
        return summary;
    }

    for (index, symbol) in ranked
        .into_iter()
        .take(DEFAULT_RANKED_SYMBOL_LIMIT)
        .enumerate()
    {
        summary.push_str(&format!(
            "\n{:>4}  {:>5}  {:>7}  {:>5}  {}:{}  {}  {}",
            index + 1,
            symbol.cyclomatic_complexity.unwrap_or(0),
            symbol.max_control_flow_nesting.unwrap_or(0),
            symbol.function_length_lines.unwrap_or(symbol.length_lines),
            symbol.path,
            symbol.start_line,
            symbol.kind,
            symbol.name
        ));
    }

    summary
}

fn ranked_function_symbols(symbols: &[ComplexitySymbolRecord]) -> Vec<&ComplexitySymbolRecord> {
    let mut ranked = symbols
        .iter()
        .filter(|symbol| symbol.function_length_lines.is_some())
        .collect::<Vec<_>>();

    ranked.sort_by(|left, right| {
        ranked_symbol_key(right)
            .cyclomatic_complexity
            .cmp(&ranked_symbol_key(left).cyclomatic_complexity)
            .then_with(|| {
                ranked_symbol_key(right)
                    .max_control_flow_nesting
                    .cmp(&ranked_symbol_key(left).max_control_flow_nesting)
            })
            .then_with(|| {
                ranked_symbol_key(right)
                    .length_lines
                    .cmp(&ranked_symbol_key(left).length_lines)
            })
            .then_with(|| {
                ranked_symbol_key(left)
                    .path
                    .cmp(ranked_symbol_key(right).path)
            })
            .then_with(|| {
                ranked_symbol_key(left)
                    .start_line
                    .cmp(&ranked_symbol_key(right).start_line)
            })
            .then_with(|| {
                ranked_symbol_key(left)
                    .kind
                    .cmp(ranked_symbol_key(right).kind)
            })
            .then_with(|| {
                ranked_symbol_key(left)
                    .name
                    .cmp(ranked_symbol_key(right).name)
            })
    });

    ranked
}

fn ranked_symbol_key(symbol: &ComplexitySymbolRecord) -> RankedSymbolKey<'_> {
    RankedSymbolKey {
        cyclomatic_complexity: symbol.cyclomatic_complexity.unwrap_or(0),
        max_control_flow_nesting: symbol.max_control_flow_nesting.unwrap_or(0),
        length_lines: symbol.function_length_lines.unwrap_or(symbol.length_lines),
        path: &symbol.path,
        start_line: symbol.start_line,
        kind: &symbol.kind,
        name: &symbol.name,
    }
}

fn is_function_or_method(kind: &str) -> bool {
    matches!(kind, "function" | "method")
}

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentKind, ParseFileRecord, ParseSymbolRecord};

    fn parse_report(symbols: Vec<ParseSymbolRecord>) -> ParseReport {
        ParseReport {
            warnings: Vec::new(),
            files: vec![ParseFileRecord {
                path: "src/lib.rs".to_owned(),
                language: Some("Rust"),
                content: ContentKind::Text,
                status: ParseFileStatus::Parsed,
                reason: None,
                symbol_count: symbols.len() as u64,
                import_count: 0,
            }],
            symbols,
            imports: Vec::new(),
        }
    }

    fn symbol(
        name: &str,
        kind: &str,
        start_line: u64,
        end_line: u64,
        cyclomatic_complexity: Option<u64>,
        max_control_flow_nesting: Option<u64>,
    ) -> ParseSymbolRecord {
        ParseSymbolRecord {
            path: "src/lib.rs".to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            start_line,
            end_line,
            signature: None,
            nesting_depth: 0,
            parent: None,
            cyclomatic_complexity,
            max_control_flow_nesting,
        }
    }

    #[test]
    fn report_derives_lengths_large_symbols_and_summary() {
        let report = report_from_parse(&parse_report(vec![
            symbol("Widget", "struct", 1, 90, None, None),
            symbol("small", "function", 4, 8, Some(2), Some(1)),
            symbol("large", "method", 10, 89, Some(7), Some(3)),
        ]));

        assert_eq!(report.summary.total_files, 1);
        assert_eq!(report.summary.parsed_files, 1);
        assert_eq!(report.summary.symbol_count, 3);
        assert_eq!(report.summary.function_method_count, 2);
        assert_eq!(report.summary.large_symbol_count, 2);
        assert_eq!(report.summary.max_cyclomatic_complexity, 7);
        assert_eq!(report.summary.max_nesting_depth, 3);
        assert_eq!(report.files[0].function_method_count, 2);
        assert_eq!(report.files[0].large_symbol_count, 2);
        assert_eq!(report.files[0].max_cyclomatic_complexity, Some(7));
        assert_eq!(report.files[0].max_nesting_depth, Some(3));
        assert_eq!(report.symbols[0].length_lines, 90);
        assert_eq!(report.symbols[0].function_length_lines, None);
        assert_eq!(report.symbols[1].length_lines, 5);
        assert_eq!(report.symbols[1].function_length_lines, Some(5));
        assert!(report.symbols[2].is_large_symbol);
    }

    #[test]
    fn summary_ranks_functions_by_complexity_then_stable_keys() {
        let report = report_from_parse(&parse_report(vec![
            symbol("later", "function", 20, 22, Some(4), Some(1)),
            symbol("first", "function", 10, 11, Some(4), Some(2)),
            symbol("not_ranked", "struct", 1, 1, None, None),
        ]));

        let summary = render_summary(&report);

        assert!(summary.contains("functions/methods  2"));
        assert!(
            summary.find("first").expect("first should be ranked")
                < summary.find("later").expect("later should be ranked")
        );
        assert!(!summary.contains("not_ranked"));
    }

    #[test]
    fn json_uses_public_schema_identifier() {
        let report = report_from_parse(&parse_report(Vec::new()));
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&report).expect("json should render"))
                .expect("json should parse");

        assert_eq!(value["schema_version"], COMPLEXITY_SCHEMA_VERSION);
        assert!(value["summary"].is_object());
        assert!(value["files"].is_array());
        assert!(value["symbols"].is_array());
    }
}
