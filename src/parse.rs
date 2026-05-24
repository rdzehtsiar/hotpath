// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;

use rayon::prelude::*;
use serde::Serialize;
use serde_json::json;
use tree_sitter::{Language, Node, Parser};

use crate::{operation_log, ContentKind, FileRecord, ScanReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseFileStatus {
    Parsed,
    Pending,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseFileReason {
    ParserExtractionPending,
    UnsupportedContent,
    UnsupportedLanguage,
    UnsupportedEncoding,
    ReadFailed,
    ParseFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseWarning {
    pub code: &'static str,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseFileRecord {
    pub path: String,
    pub language: Option<&'static str>,
    pub content: ContentKind,
    pub status: ParseFileStatus,
    pub reason: Option<ParseFileReason>,
    pub symbol_count: u64,
    pub import_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseSymbolRecord {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: u64,
    pub end_line: u64,
    pub signature: Option<String>,
    pub nesting_depth: u64,
    pub parent: Option<String>,
    pub cyclomatic_complexity: Option<u64>,
    pub max_control_flow_nesting: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseImportRecord {
    pub path: String,
    pub target: String,
    pub kind: String,
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseSummary {
    pub total_files: u64,
    pub candidate_files: u64,
    pub parsed_files: u64,
    pub pending_files: u64,
    pub skipped_files: u64,
    pub symbol_count: u64,
    pub import_count: u64,
    pub warning_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseReport {
    pub warnings: Vec<ParseWarning>,
    pub files: Vec<ParseFileRecord>,
    pub symbols: Vec<ParseSymbolRecord>,
    pub imports: Vec<ParseImportRecord>,
}

impl ParseReport {
    pub(crate) fn summary(&self) -> ParseSummary {
        ParseSummary {
            total_files: self.files.len() as u64,
            candidate_files: self
                .files
                .iter()
                .filter(|file| is_candidate_file_record(file))
                .count() as u64,
            parsed_files: self
                .files
                .iter()
                .filter(|file| file.status == ParseFileStatus::Parsed)
                .count() as u64,
            pending_files: self
                .files
                .iter()
                .filter(|file| file.status == ParseFileStatus::Pending)
                .count() as u64,
            skipped_files: self
                .files
                .iter()
                .filter(|file| file.status == ParseFileStatus::Skipped)
                .count() as u64,
            symbol_count: self.symbols.len() as u64,
            import_count: self.imports.len() as u64,
            warning_count: self.warnings.len() as u64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedLanguage {
    Rust,
    Go,
    TypeScript,
    Tsx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileExtraction {
    parsed: bool,
    warnings: Vec<ParseWarning>,
    symbols: Vec<ParseSymbolRecord>,
    imports: Vec<ParseImportRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseProgress {
    pub completed_files: usize,
    pub total_files: usize,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParentSymbol {
    name: String,
    kind: &'static str,
}

#[derive(Debug)]
struct ExtractionState<'source> {
    path: &'source str,
    source: &'source str,
    language: SupportedLanguage,
    parents: Vec<ParentSymbol>,
    symbols: Vec<ParseSymbolRecord>,
    imports: Vec<ParseImportRecord>,
}

struct FileParseOutput {
    file: ParseFileRecord,
    warnings: Vec<ParseWarning>,
    symbols: Vec<ParseSymbolRecord>,
    imports: Vec<ParseImportRecord>,
    counted_for_progress: bool,
}

pub(crate) fn report_from_scan(root: &Path, scan: &ScanReport) -> ParseReport {
    report_from_scan_with_progress(root, scan, |_| {})
}

pub(crate) fn report_from_scan_with_progress<F>(
    root: &Path,
    scan: &ScanReport,
    mut progress: F,
) -> ParseReport
where
    F: FnMut(ParseProgress),
{
    operation_log::event(
        "parse_started",
        json!({
            "total_files": scan.files.len(),
        }),
    );
    let mut warnings = parse_warnings_from_scan(scan);
    let mut files = Vec::with_capacity(scan.files.len());
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let total_parse_files = scan
        .files
        .iter()
        .filter(|file| supported_language(file).is_some() && file.content == ContentKind::Text)
        .count();
    let mut completed_parse_files = 0;

    let outputs = scan
        .files
        .par_iter()
        .map(|file| parse_file_from_scan(root, file))
        .collect::<Vec<_>>();

    for output in outputs {
        if output.counted_for_progress {
            completed_parse_files += 1;
            progress(ParseProgress {
                completed_files: completed_parse_files,
                total_files: total_parse_files,
                path: output.file.path.clone(),
            });
        }
        warnings.extend(output.warnings);
        symbols.extend(output.symbols);
        imports.extend(output.imports);
        files.push(output.file);
    }

    sort_parse_warnings(&mut warnings);
    sort_symbol_records(&mut symbols);
    sort_import_records(&mut imports);

    ParseReport {
        warnings,
        files,
        symbols,
        imports,
    }
}

fn parse_file_from_scan(root: &Path, file: &FileRecord) -> FileParseOutput {
    let Some(language) = supported_language(file) else {
        return FileParseOutput {
            file: parse_file_record(
                file,
                ParseFileStatus::Skipped,
                Some(unsupported_reason(file)),
                0,
                0,
            ),
            warnings: Vec::new(),
            symbols: Vec::new(),
            imports: Vec::new(),
            counted_for_progress: false,
        };
    };

    if file.content != ContentKind::Text {
        return FileParseOutput {
            file: parse_file_record(
                file,
                ParseFileStatus::Skipped,
                Some(ParseFileReason::UnsupportedContent),
                0,
                0,
            ),
            warnings: Vec::new(),
            symbols: Vec::new(),
            imports: Vec::new(),
            counted_for_progress: false,
        };
    }

    operation_log::event(
        "parse_file_started",
        json!({
            "path": file.path,
            "language": language.name(),
        }),
    );
    let source = match fs::read_to_string(root.join(&file.path)) {
        Ok(source) => source,
        Err(source) if source.kind() == std::io::ErrorKind::InvalidData => {
            operation_log::event(
                "parse_file_failed_or_skipped",
                json!({
                    "path": file.path,
                    "language": language.name(),
                    "reason": "UnsupportedEncoding",
                }),
            );
            return FileParseOutput {
                file: parse_file_record(
                    file,
                    ParseFileStatus::Skipped,
                    Some(ParseFileReason::UnsupportedEncoding),
                    0,
                    0,
                ),
                warnings: vec![parse_warning(
                    "parse_unsupported_encoding",
                    Some(file.path.clone()),
                    "file contents are no longer valid UTF-8; file skipped".to_owned(),
                )],
                symbols: Vec::new(),
                imports: Vec::new(),
                counted_for_progress: true,
            };
        }
        Err(source) => {
            operation_log::event(
                "parse_file_failed_or_skipped",
                json!({
                    "path": file.path,
                    "language": language.name(),
                    "reason": "ReadFailed",
                    "error": source.to_string(),
                }),
            );
            return FileParseOutput {
                file: parse_file_record(
                    file,
                    ParseFileStatus::Skipped,
                    Some(ParseFileReason::ReadFailed),
                    0,
                    0,
                ),
                warnings: vec![parse_warning(
                    "parse_read_failed",
                    Some(file.path.clone()),
                    format!("failed to read file contents for parsing: {source}"),
                )],
                symbols: Vec::new(),
                imports: Vec::new(),
                counted_for_progress: true,
            };
        }
    };

    let extraction = extract_source(&file.path, language, &source);
    let symbol_count = extraction.symbols.len() as u64;
    let import_count = extraction.imports.len() as u64;
    let (status, reason) = if extraction.parsed {
        (ParseFileStatus::Parsed, None)
    } else {
        (ParseFileStatus::Skipped, Some(ParseFileReason::ParseFailed))
    };

    operation_log::event(
        "parse_file_completed",
        json!({
            "path": file.path,
            "language": language.name(),
            "status": format!("{status:?}"),
            "reason": reason.map(|value| format!("{value:?}")),
            "symbol_count": symbol_count,
            "import_count": import_count,
        }),
    );

    FileParseOutput {
        file: parse_file_record(file, status, reason, symbol_count, import_count),
        warnings: extraction.warnings,
        symbols: extraction.symbols,
        imports: extraction.imports,
        counted_for_progress: true,
    }
}

impl SupportedLanguage {
    fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "Tsx",
        }
    }
}

pub(crate) fn sort_symbol_records(symbols: &mut [ParseSymbolRecord]) {
    crate::sort_symbol_records_by_location(symbols);
}

impl crate::SymbolRecordSortFields for ParseSymbolRecord {
    fn symbol_record_sort_key(&self) -> crate::SymbolRecordSortKey<'_> {
        crate::symbol_record_sort_key(
            &self.path,
            self.start_line,
            self.end_line,
            &self.kind,
            &self.name,
        )
    }
}

pub(crate) fn sort_import_records(imports: &mut [ParseImportRecord]) {
    imports.sort_by(|left, right| {
        (
            &left.path,
            left.start_line,
            left.end_line,
            &left.kind,
            &left.target,
        )
            .cmp(&(
                &right.path,
                right.start_line,
                right.end_line,
                &right.kind,
                &right.target,
            ))
    });
}

pub fn scaffold_report_from_scan(scan: &ScanReport) -> ParseReport {
    let mut warnings = parse_warnings_from_scan(scan);

    sort_parse_warnings(&mut warnings);

    ParseReport {
        warnings,
        files: scan.files.iter().map(scaffold_file_record).collect(),
        symbols: Vec::new(),
        imports: Vec::new(),
    }
}

fn is_candidate_file_record(file: &ParseFileRecord) -> bool {
    !matches!(
        file.reason,
        Some(ParseFileReason::UnsupportedContent | ParseFileReason::UnsupportedLanguage)
    )
}

fn parse_warnings_from_scan(scan: &ScanReport) -> Vec<ParseWarning> {
    let scan_warnings = scan.warnings.iter().map(|warning| ParseWarning {
        code: warning.code,
        path: warning.path.clone(),
        message: warning.message.clone(),
    });
    let file_warnings = scan.files.iter().flat_map(|file| {
        file.warnings.iter().map(|warning| ParseWarning {
            code: warning.code,
            path: Some(file.path.clone()),
            message: warning.message.clone(),
        })
    });

    scan_warnings.chain(file_warnings).collect()
}

fn sort_parse_warnings(warnings: &mut [ParseWarning]) {
    warnings.sort_by(|left, right| {
        (&left.path, left.code, &left.message).cmp(&(&right.path, right.code, &right.message))
    });
}

fn scaffold_file_record(file: &FileRecord) -> ParseFileRecord {
    let (status, reason) = if file.content != ContentKind::Text {
        (
            ParseFileStatus::Skipped,
            Some(ParseFileReason::UnsupportedContent),
        )
    } else if supported_language(file).is_some() {
        (
            ParseFileStatus::Pending,
            Some(ParseFileReason::ParserExtractionPending),
        )
    } else {
        (
            ParseFileStatus::Skipped,
            Some(ParseFileReason::UnsupportedLanguage),
        )
    };

    parse_file_record(file, status, reason, 0, 0)
}

fn parse_file_record(
    file: &FileRecord,
    status: ParseFileStatus,
    reason: Option<ParseFileReason>,
    symbol_count: u64,
    import_count: u64,
) -> ParseFileRecord {
    ParseFileRecord {
        path: file.path.clone(),
        language: file.language,
        content: file.content,
        status,
        reason,
        symbol_count,
        import_count,
    }
}

fn unsupported_reason(file: &FileRecord) -> ParseFileReason {
    if file.content != ContentKind::Text {
        ParseFileReason::UnsupportedContent
    } else {
        ParseFileReason::UnsupportedLanguage
    }
}

fn supported_language(file: &FileRecord) -> Option<SupportedLanguage> {
    match file.extension.as_deref() {
        Some("rs") => Some(SupportedLanguage::Rust),
        Some("go") => Some(SupportedLanguage::Go),
        Some("ts") => Some(SupportedLanguage::TypeScript),
        Some("tsx") => Some(SupportedLanguage::Tsx),
        _ => match file.language {
            Some("Rust") => Some(SupportedLanguage::Rust),
            Some("Go") => Some(SupportedLanguage::Go),
            Some("TypeScript") => Some(SupportedLanguage::TypeScript),
            Some("TypeScript JSX") => Some(SupportedLanguage::Tsx),
            _ => None,
        },
    }
}

fn extract_source(path: &str, language: SupportedLanguage, source: &str) -> FileExtraction {
    let mut parser = Parser::new();
    let tree_sitter_language = tree_sitter_language(language);
    if let Err(source) = parser.set_language(&tree_sitter_language) {
        return FileExtraction {
            parsed: false,
            warnings: vec![parse_warning(
                "parse_language_failed",
                Some(path.to_owned()),
                format!("failed to initialize parser grammar: {source}"),
            )],
            symbols: Vec::new(),
            imports: Vec::new(),
        };
    }

    operation_log::event(
        "parse_tree_started",
        json!({
            "path": path,
            "language": language.name(),
            "bytes": source.len(),
        }),
    );
    let Some(tree) = parser.parse(source, None) else {
        return FileExtraction {
            parsed: false,
            warnings: vec![parse_warning(
                "parse_failed",
                Some(path.to_owned()),
                "tree-sitter did not produce a parse tree".to_owned(),
            )],
            symbols: Vec::new(),
            imports: Vec::new(),
        };
    };
    operation_log::event(
        "parse_tree_completed",
        json!({
            "path": path,
            "language": language.name(),
        }),
    );

    let root = tree.root_node();
    let mut warnings = Vec::new();
    operation_log::event(
        "parse_syntax_check_started",
        json!({
            "path": path,
            "language": language.name(),
            "root_kind": root.kind(),
            "has_error": root.has_error(),
        }),
    );
    if root.has_error() {
        let line = first_error_line(root).unwrap_or_else(|| start_line(root));
        warnings.push(parse_warning(
            "syntax_error",
            Some(path.to_owned()),
            format!("syntax errors detected near line {line}; extracted symbols may be incomplete"),
        ));
    }
    operation_log::event(
        "parse_syntax_check_completed",
        json!({
            "path": path,
            "language": language.name(),
            "warning_count": warnings.len(),
        }),
    );

    let mut state = ExtractionState {
        path,
        source,
        language,
        parents: Vec::new(),
        symbols: Vec::new(),
        imports: Vec::new(),
    };

    operation_log::event(
        "parse_walk_started",
        json!({
            "path": path,
            "language": language.name(),
            "root_kind": root.kind(),
        }),
    );
    match language {
        SupportedLanguage::Rust => walk_rust(root, &mut state),
        SupportedLanguage::Go => walk_go(root, &mut state),
        SupportedLanguage::TypeScript | SupportedLanguage::Tsx => {
            walk_typescript(root, &mut state);
        }
    }
    operation_log::event(
        "parse_walk_completed",
        json!({
            "path": path,
            "language": language.name(),
            "symbol_count": state.symbols.len(),
            "import_count": state.imports.len(),
        }),
    );

    FileExtraction {
        parsed: true,
        warnings,
        symbols: state.symbols,
        imports: state.imports,
    }
}

fn tree_sitter_language(language: SupportedLanguage) -> Language {
    match language {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        SupportedLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SupportedLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

enum WalkFrame<'tree> {
    Visit(Node<'tree>),
    ExitParent,
}

fn walk_rust(root: Node<'_>, state: &mut ExtractionState<'_>) {
    let mut stack = vec![WalkFrame::Visit(root)];

    while let Some(frame) = stack.pop() {
        match frame {
            WalkFrame::ExitParent => {
                state.parents.pop();
            }
            WalkFrame::Visit(node) => walk_rust_node(node, state, &mut stack),
        }
    }
}

fn walk_rust_node<'tree>(
    node: Node<'tree>,
    state: &mut ExtractionState<'_>,
    stack: &mut Vec<WalkFrame<'tree>>,
) {
    match node.kind() {
        "use_declaration" => {
            if let Some(target) = rust_use_target(node, state.source) {
                state
                    .imports
                    .push(import_record(state.path, &target, "use", node));
            }
        }
        "mod_item" => {
            let Some(name) = node_name(node, state.source) else {
                push_walk_children(stack, node);
                return;
            };
            if !has_body(node) {
                state
                    .imports
                    .push(import_record(state.path, &name, "mod", node));
            }
            record_symbol_and_push_children(node, state, stack, name, "module", true, false);
        }
        "struct_item" => {
            record_named_symbol_and_push_children(node, state, stack, "struct", false, false);
        }
        "enum_item" => {
            record_named_symbol_and_push_children(node, state, stack, "enum", false, false);
        }
        "trait_item" => {
            record_named_symbol_and_push_children(node, state, stack, "trait", true, false);
        }
        "impl_item" => {
            let name = rust_impl_name(node, state.source).unwrap_or_else(|| "impl".to_owned());
            record_symbol_and_push_children(node, state, stack, name, "impl", true, false);
        }
        "function_item" => {
            let kind = if state
                .parents
                .last()
                .is_some_and(|parent| matches!(parent.kind, "impl" | "trait"))
            {
                "method"
            } else {
                "function"
            };
            record_named_symbol_and_push_children(node, state, stack, kind, true, true);
        }
        "function_signature_item" => {
            let kind = if state
                .parents
                .last()
                .is_some_and(|parent| matches!(parent.kind, "impl" | "trait"))
            {
                "method"
            } else {
                "function"
            };
            record_named_symbol_and_push_children(node, state, stack, kind, false, false);
        }
        _ => push_walk_children(stack, node),
    }
}

fn walk_go(root: Node<'_>, state: &mut ExtractionState<'_>) {
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        match node.kind() {
            "source_file" | "import_declaration" | "import_spec_list" | "type_declaration" => {
                push_node_children(&mut stack, node);
            }
            "package_clause" => {
                if let Some(name) = node_name(node, state.source) {
                    state
                        .symbols
                        .push(symbol_record(state, node, name, "package", false, None));
                }
            }
            "import_spec" => {
                if let Some(target) = string_target(node, state.source) {
                    state
                        .imports
                        .push(import_record(state.path, &target, "import", node));
                }
            }
            "function_declaration" => {
                record_named_go_symbol(node, state, "function", true, None);
            }
            "method_declaration" => {
                let parent = go_receiver_name(node, state.source);
                record_named_go_symbol(node, state, "method", true, parent);
            }
            "type_spec" => {
                if let Some(name) = node_name(node, state.source) {
                    let kind = go_type_kind(node);
                    state
                        .symbols
                        .push(symbol_record(state, node, name, kind, false, None));
                }
            }
            _ => {}
        }
    }
}

fn record_named_go_symbol(
    node: Node<'_>,
    state: &mut ExtractionState<'_>,
    kind: &'static str,
    include_complexity: bool,
    parent: Option<String>,
) {
    if let Some(name) = node_name(node, state.source) {
        state.symbols.push(symbol_record(
            state,
            node,
            name,
            kind,
            include_complexity,
            parent,
        ));
    }
}

fn walk_typescript(root: Node<'_>, state: &mut ExtractionState<'_>) {
    let mut stack = vec![WalkFrame::Visit(root)];

    while let Some(frame) = stack.pop() {
        match frame {
            WalkFrame::ExitParent => {
                state.parents.pop();
            }
            WalkFrame::Visit(node) => walk_typescript_node(node, state, &mut stack),
        }
    }
}

fn walk_typescript_node<'tree>(
    node: Node<'tree>,
    state: &mut ExtractionState<'_>,
    stack: &mut Vec<WalkFrame<'tree>>,
) {
    match node.kind() {
        "import_statement" => {
            if let Some(target) = string_target(node, state.source) {
                state
                    .imports
                    .push(import_record(state.path, &target, "import", node));
            }
        }
        "internal_module" | "module" | "module_declaration" => {
            if let Some(name) = node_name(node, state.source) {
                record_symbol_and_push_children(node, state, stack, name, "namespace", true, false);
            } else {
                push_walk_children(stack, node);
            }
        }
        "function_declaration" => {
            record_named_symbol_and_push_children(node, state, stack, "function", true, true);
        }
        "method_definition" | "method_signature" => {
            record_named_symbol_and_push_children(node, state, stack, "method", true, true);
        }
        "class_declaration" => {
            record_named_symbol_and_push_children(node, state, stack, "class", true, false);
        }
        "interface_declaration" => {
            record_named_symbol_and_push_children(node, state, stack, "interface", true, false);
        }
        "type_alias_declaration" => {
            record_named_symbol_and_push_children(node, state, stack, "type_alias", false, false);
        }
        _ => push_walk_children(stack, node),
    }
}

fn record_named_symbol_and_push_children<'tree>(
    node: Node<'tree>,
    state: &mut ExtractionState<'_>,
    stack: &mut Vec<WalkFrame<'tree>>,
    kind: &'static str,
    push_parent: bool,
    include_complexity: bool,
) {
    let Some(name) = node_name(node, state.source) else {
        push_walk_children(stack, node);
        return;
    };

    record_symbol_and_push_children(
        node,
        state,
        stack,
        name,
        kind,
        push_parent,
        include_complexity,
    );
}

fn record_symbol_and_push_children<'tree>(
    node: Node<'tree>,
    state: &mut ExtractionState<'_>,
    stack: &mut Vec<WalkFrame<'tree>>,
    name: String,
    kind: &'static str,
    push_parent: bool,
    include_complexity: bool,
) {
    let record = symbol_record(state, node, name.clone(), kind, include_complexity, None);
    state.symbols.push(record);

    if push_parent {
        state.parents.push(ParentSymbol { name, kind });
        stack.push(WalkFrame::ExitParent);
    }
    push_walk_children(stack, node);
}

fn push_walk_children<'tree>(stack: &mut Vec<WalkFrame<'tree>>, node: Node<'tree>) {
    for child in named_children(node).into_iter().rev() {
        stack.push(WalkFrame::Visit(child));
    }
}

fn push_node_children<'tree>(stack: &mut Vec<Node<'tree>>, node: Node<'tree>) {
    for child in named_children(node).into_iter().rev() {
        stack.push(child);
    }
}

fn symbol_record(
    state: &ExtractionState<'_>,
    node: Node<'_>,
    name: String,
    kind: &'static str,
    include_complexity: bool,
    parent_override: Option<String>,
) -> ParseSymbolRecord {
    let complexity = include_complexity.then(|| complexity_for(node, state.language));
    let parent = parent_override.or_else(|| parent_path(&state.parents));

    ParseSymbolRecord {
        path: state.path.to_owned(),
        name,
        kind: kind.to_owned(),
        start_line: start_line(node),
        end_line: end_line(node),
        signature: concise_signature(node, state.source),
        nesting_depth: state.parents.len() as u64,
        parent,
        cyclomatic_complexity: complexity.as_ref().map(|value| value.cyclomatic_complexity),
        max_control_flow_nesting: complexity.map(|value| value.max_control_flow_nesting),
    }
}

fn import_record(path: &str, target: &str, kind: &str, node: Node<'_>) -> ParseImportRecord {
    ParseImportRecord {
        path: path.to_owned(),
        target: target.to_owned(),
        kind: kind.to_owned(),
        start_line: start_line(node),
        end_line: end_line(node),
    }
}

#[derive(Debug, Clone, Copy)]
struct Complexity {
    cyclomatic_complexity: u64,
    max_control_flow_nesting: u64,
}

fn complexity_for(node: Node<'_>, language: SupportedLanguage) -> Complexity {
    let mut branch_count = 0;
    let mut max_nesting = 0;

    let mut stack = Vec::new();
    for child in named_children(node).into_iter().rev() {
        stack.push((child, 0));
    }

    while let Some((node, nesting)) = stack.pop() {
        if is_nested_function_boundary(node.kind(), language) {
            continue;
        }

        let child_nesting = if is_control_flow_node(node.kind(), language) {
            branch_count += 1;
            let next_nesting = nesting + 1;
            max_nesting = max_nesting.max(next_nesting);
            next_nesting
        } else {
            nesting
        };

        for child in named_children(node).into_iter().rev() {
            stack.push((child, child_nesting));
        }
    }

    Complexity {
        cyclomatic_complexity: 1 + branch_count,
        max_control_flow_nesting: max_nesting,
    }
}

fn is_nested_function_boundary(kind: &str, language: SupportedLanguage) -> bool {
    match language {
        SupportedLanguage::Rust => matches!(kind, "function_item" | "closure_expression"),
        SupportedLanguage::Go => matches!(kind, "function_declaration" | "method_declaration"),
        SupportedLanguage::TypeScript | SupportedLanguage::Tsx => matches!(
            kind,
            "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
        ),
    }
}

fn is_control_flow_node(kind: &str, language: SupportedLanguage) -> bool {
    match language {
        SupportedLanguage::Rust => matches!(
            kind,
            "if_expression"
                | "match_expression"
                | "while_expression"
                | "loop_expression"
                | "for_expression"
                | "if_let_expression"
                | "while_let_expression"
        ),
        SupportedLanguage::Go => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "switch_statement"
                | "type_switch_statement"
                | "select_statement"
                | "case_clause"
                | "communication_case"
        ),
        SupportedLanguage::TypeScript | SupportedLanguage::Tsx => matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "for_in_statement"
                | "for_of_statement"
                | "while_statement"
                | "do_statement"
                | "switch_statement"
                | "switch_case"
                | "case_clause"
                | "catch_clause"
                | "conditional_expression"
                | "ternary_expression"
        ),
    }
}

fn rust_use_target(node: Node<'_>, source: &str) -> Option<String> {
    let text = node_text(node, source)?;
    let target = text
        .trim()
        .strip_prefix("use")
        .unwrap_or(text.trim())
        .trim()
        .trim_end_matches(';')
        .trim();
    non_empty(compact_text(target))
}

fn rust_impl_name(node: Node<'_>, source: &str) -> Option<String> {
    let signature = concise_signature(node, source)?;
    non_empty(signature)
}

fn go_type_kind(node: Node<'_>) -> &'static str {
    if descendant_kind(node, "struct_type") {
        "struct"
    } else if descendant_kind(node, "interface_type") {
        "interface"
    } else {
        "type"
    }
}

fn go_receiver_name(node: Node<'_>, source: &str) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let text = node_text(receiver, source)?;
    let trimmed = text.trim().trim_start_matches('(').trim_end_matches(')');
    let receiver_type = trimmed.split_whitespace().last()?;
    non_empty(
        receiver_type
            .trim_start_matches('*')
            .trim_start_matches('&')
            .trim()
            .to_owned(),
    )
}

fn string_target(node: Node<'_>, source: &str) -> Option<String> {
    string_literal_text(node, source).and_then(strip_string_quotes)
}

fn string_literal_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut stack = vec![node];

    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "interpreted_string_literal" | "raw_string_literal" | "string"
        ) {
            return node_text(node, source).map(ToOwned::to_owned);
        }

        for child in named_children(node).into_iter().rev() {
            stack.push(child);
        }
    }

    None
}

fn strip_string_quotes(value: String) -> Option<String> {
    let trimmed = value.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('`')
                .and_then(|value| value.strip_suffix('`'))
        })
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(trimmed);

    non_empty(unquoted.to_owned())
}

fn node_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return node_text(name, source).and_then(|text| non_empty(compact_text(text)));
    }

    for child in named_children(node) {
        if matches!(
            child.kind(),
            "identifier"
                | "field_identifier"
                | "package_identifier"
                | "property_identifier"
                | "type_identifier"
                | "nested_identifier"
        ) {
            return node_text(child, source).and_then(|text| non_empty(compact_text(text)));
        }
    }

    None
}

fn concise_signature(node: Node<'_>, source: &str) -> Option<String> {
    let end = signature_end_byte(node);
    let text = source.get(node.start_byte()..end)?;
    let signature = compact_text(text)
        .trim_end_matches('{')
        .trim_end_matches(';')
        .trim()
        .to_owned();

    non_empty(signature).map(|signature| {
        const MAX_SIGNATURE_BYTES: usize = 180;
        if signature.len() <= MAX_SIGNATURE_BYTES {
            signature
        } else {
            truncate_at_char_boundary(&signature, MAX_SIGNATURE_BYTES)
                .trim_end()
                .to_owned()
        }
    })
}

fn truncate_at_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }

    &value[..end]
}

fn signature_end_byte(node: Node<'_>) -> usize {
    if let Some(body) = node.child_by_field_name("body") {
        return body.start_byte();
    }

    for child in named_children(node) {
        if matches!(
            child.kind(),
            "block"
                | "statement_block"
                | "declaration_list"
                | "class_body"
                | "object_type"
                | "field_declaration_list"
                | "enum_variant_list"
                | "interface_type"
                | "struct_type"
        ) {
            return child.start_byte();
        }
    }

    node.end_byte()
}

fn parent_path(parents: &[ParentSymbol]) -> Option<String> {
    if parents.is_empty() {
        None
    } else {
        Some(
            parents
                .iter()
                .map(|parent| parent.name.as_str())
                .collect::<Vec<_>>()
                .join("::"),
        )
    }
}

fn first_error_line(node: Node<'_>) -> Option<u64> {
    let mut stack = vec![node];

    while let Some(node) = stack.pop() {
        if node.is_error() || node.kind() == "ERROR" {
            return Some(start_line(node));
        }

        for child in named_children(node).into_iter().rev() {
            stack.push(child);
        }
    }

    None
}

fn descendant_kind(node: Node<'_>, kind: &str) -> bool {
    let mut stack = named_children(node);

    while let Some(node) = stack.pop() {
        if node.kind() == kind {
            return true;
        }

        for child in named_children(node) {
            stack.push(child);
        }
    }

    false
}

fn has_body(node: Node<'_>) -> bool {
    node.child_by_field_name("body").is_some()
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn node_text<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    source.get(node.start_byte()..node.end_byte())
}

fn compact_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn start_line(node: Node<'_>) -> u64 {
    node.start_position().row as u64 + 1
}

fn end_line(node: Node<'_>) -> u64 {
    node.end_position().row as u64 + 1
}

fn parse_warning(code: &'static str, path: Option<String>, message: String) -> ParseWarning {
    ParseWarning {
        code,
        path,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extraction(language: SupportedLanguage, source: &str) -> FileExtraction {
        extract_source("src/sample", language, source)
    }

    fn symbol<'a>(extraction: &'a FileExtraction, name: &str, kind: &str) -> &'a ParseSymbolRecord {
        extraction
            .symbols
            .iter()
            .find(|symbol| symbol.name == name && symbol.kind == kind)
            .unwrap_or_else(|| panic!("expected {kind} symbol named {name}"))
    }

    fn import_targets(extraction: &FileExtraction) -> Vec<&str> {
        extraction
            .imports
            .iter()
            .map(|import| import.target.as_str())
            .collect()
    }

    #[test]
    fn rust_extraction_records_symbols_imports_and_complexity() {
        let source = r#"
use std::{fmt, io};
mod child;

pub struct Widget {
    value: i32,
}

trait Draw {
    fn draw(&self);
}

impl Widget {
    pub fn render(&self) {
        if self.value > 0 {
            for item in 0..self.value {
                match item {
                    0 => {}
                    _ => {}
                }
            }
        }
    }
}

fn free() {}
"#;

        let extraction = extraction(SupportedLanguage::Rust, source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), vec!["std::{fmt, io}", "child"]);
        assert_eq!(symbol(&extraction, "child", "module").start_line, 3);
        assert_eq!(symbol(&extraction, "Widget", "struct").start_line, 5);
        assert_eq!(symbol(&extraction, "Draw", "trait").start_line, 9);
        assert_eq!(symbol(&extraction, "free", "function").start_line, 26);

        let render = symbol(&extraction, "render", "method");
        assert_eq!(render.parent.as_deref(), Some("impl Widget"));
        assert_eq!(render.nesting_depth, 1);
        assert_eq!(render.cyclomatic_complexity, Some(4));
        assert_eq!(render.max_control_flow_nesting, Some(3));
    }

    #[test]
    fn rust_inline_modules_are_symbols_not_dependency_imports() {
        let source = r#"
mod inline {
    pub fn child() {}
}

mod external;
"#;

        let extraction = extraction(SupportedLanguage::Rust, source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), vec!["external"]);
        assert_eq!(symbol(&extraction, "inline", "module").start_line, 2);
        assert_eq!(
            symbol(&extraction, "child", "function").parent.as_deref(),
            Some("inline")
        );
    }

    #[test]
    fn go_extraction_records_package_types_methods_and_imports() {
        let source = r#"
package api

import (
    "fmt"
    alias "net/http"
)

type Server struct {
    Name string
}

type Runner interface {
    Run() error
}

func New() *Server {
    return &Server{}
}

func (s *Server) Serve() {
    if s.Name != "" {
        for range []int{1} {
        }
    }
}
"#;

        let extraction = extraction(SupportedLanguage::Go, source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), vec!["fmt", "net/http"]);
        assert_eq!(symbol(&extraction, "api", "package").start_line, 2);
        assert_eq!(symbol(&extraction, "Server", "struct").start_line, 9);
        assert_eq!(symbol(&extraction, "Runner", "interface").start_line, 13);
        assert_eq!(symbol(&extraction, "New", "function").start_line, 17);

        let serve = symbol(&extraction, "Serve", "method");
        assert_eq!(serve.parent.as_deref(), Some("Server"));
        assert_eq!(serve.cyclomatic_complexity, Some(3));
        assert_eq!(serve.max_control_flow_nesting, Some(2));
    }

    #[test]
    fn go_extraction_does_not_walk_deep_generated_literal_trees() {
        let depth = 2_000;
        let source = format!(
            "package p\nvar x = {}0{}\nfunc done() {{}}\n",
            "[]any{".repeat(depth),
            "}".repeat(depth)
        );

        let extraction = extraction(SupportedLanguage::Go, &source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), Vec::<&str>::new());
        assert_eq!(symbol(&extraction, "p", "package").start_line, 1);
        assert_eq!(symbol(&extraction, "done", "function").start_line, 3);
        assert!(!extraction.symbols.iter().any(|symbol| symbol.name == "x"));
    }

    #[test]
    fn rust_extraction_handles_deep_symbol_nesting_iteratively() {
        let depth = 1_500;
        let mut source = String::new();
        for index in 0..depth {
            source.push_str(&format!("mod m{index} {{\n"));
        }
        source.push_str("fn leaf() {}\n");
        source.push_str(&"}\n".repeat(depth));

        let extraction = extraction(SupportedLanguage::Rust, &source);

        assert!(extraction.parsed);
        let leaf = symbol(&extraction, "leaf", "function");
        assert_eq!(leaf.nesting_depth, depth as u64);
        assert!(leaf
            .parent
            .as_deref()
            .is_some_and(|parent| parent.starts_with("m0::m1::m2")));
        assert!(leaf
            .parent
            .as_deref()
            .is_some_and(|parent| parent.ends_with(&format!("m{}", depth - 1))));
    }

    #[test]
    fn typescript_extraction_handles_deep_symbol_nesting_iteratively() {
        let depth = 1_000;
        let mut source = String::new();
        for index in 0..depth {
            source.push_str(&format!("namespace n{index} {{\n"));
        }
        source.push_str("function leaf() { return true; }\n");
        source.push_str(&"}\n".repeat(depth));

        let extraction = extraction(SupportedLanguage::TypeScript, &source);

        assert!(extraction.parsed);
        let leaf = symbol(&extraction, "leaf", "function");
        assert_eq!(leaf.nesting_depth, depth as u64);
        assert!(leaf
            .parent
            .as_deref()
            .is_some_and(|parent| parent.starts_with("n0::n1::n2")));
        assert!(leaf
            .parent
            .as_deref()
            .is_some_and(|parent| parent.ends_with(&format!("n{}", depth - 1))));
    }

    #[test]
    fn complexity_handles_deep_control_flow_iteratively() {
        let depth = 1_000;
        let mut source = "fn deep() {\n".to_owned();
        for _ in 0..depth {
            source.push_str("if true {\n");
        }
        source.push_str(&"}\n".repeat(depth));
        source.push_str("}\n");

        let extraction = extraction(SupportedLanguage::Rust, &source);

        assert!(extraction.parsed);
        let deep = symbol(&extraction, "deep", "function");
        assert_eq!(deep.cyclomatic_complexity, Some(depth as u64 + 1));
        assert_eq!(deep.max_control_flow_nesting, Some(depth as u64));
    }

    #[test]
    fn typescript_extraction_records_namespaces_types_methods_and_complexity() {
        let source = r#"
import React from "react";
import { x } from "./x";

namespace UI {
    export interface Props {
        onClick(): void;
    }

    export type Mode = "a" | "b";

    export class Button {
        render() {
            if (true) {
                while (false) {}
            }
        }
    }
}

function helper() {
    return true ? 1 : 2;
}
"#;

        let extraction = extraction(SupportedLanguage::TypeScript, source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), vec!["react", "./x"]);
        assert_eq!(symbol(&extraction, "UI", "namespace").start_line, 5);
        assert_eq!(symbol(&extraction, "Props", "interface").start_line, 6);
        assert_eq!(symbol(&extraction, "Mode", "type_alias").start_line, 10);
        assert_eq!(symbol(&extraction, "Button", "class").start_line, 12);

        let render = symbol(&extraction, "render", "method");
        assert_eq!(render.parent.as_deref(), Some("UI::Button"));
        assert_eq!(render.cyclomatic_complexity, Some(3));
        assert_eq!(render.max_control_flow_nesting, Some(2));

        let helper = symbol(&extraction, "helper", "function");
        assert_eq!(helper.cyclomatic_complexity, Some(2));
        assert_eq!(helper.max_control_flow_nesting, Some(1));
    }

    #[test]
    fn tsx_extraction_uses_tsx_grammar() {
        let source = r#"
import React from "react";

export function App() {
    return <div>{true ? <span /> : null}</div>;
}
"#;

        let extraction = extraction(SupportedLanguage::Tsx, source);

        assert!(extraction.parsed);
        assert_eq!(import_targets(&extraction), vec!["react"]);
        let app = symbol(&extraction, "App", "function");
        assert_eq!(app.cyclomatic_complexity, Some(2));
        assert_eq!(app.max_control_flow_nesting, Some(1));
    }

    #[test]
    fn syntax_errors_are_warnings_without_discarding_tree() {
        let extraction = extraction(SupportedLanguage::Rust, "fn broken( {\n");

        assert!(extraction.parsed);
        assert_eq!(extraction.warnings.len(), 1);
        assert_eq!(extraction.warnings[0].code, "syntax_error");
        assert_eq!(extraction.warnings[0].path.as_deref(), Some("src/sample"));
    }

    #[test]
    fn signature_truncation_handles_utf8_boundaries() {
        let source = format!("fn long_{}() {{}}\n", "é".repeat(200));

        let extraction = extraction(SupportedLanguage::Rust, &source);

        assert!(extraction.parsed);
        assert!(symbol(
            &extraction,
            &format!("long_{}", "é".repeat(200)),
            "function"
        )
        .signature
        .as_deref()
        .is_some_and(|signature| signature.len() <= 180));
    }
}
