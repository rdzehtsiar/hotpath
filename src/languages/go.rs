// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use tree_sitter::Node;

use super::{
    LanguageParser, ParserDiagnostic, ParserLimitation, ParserOutput, ParserRecognition,
    UniversalCodeMetricsInput, UniversalControlFlowKind, UniversalControlFlowNode,
    UniversalFunction, UniversalFunctionKind, UniversalReference, UniversalSymbol,
};
use crate::pipeline::file_analyzer::{AnalyzedFile, ContentKind};

#[derive(Debug, Default)]
/// Go source parser adapter for the universal source model.
pub struct GoParser;

impl GoParser {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageParser for GoParser {
    fn language_id(&self) -> &'static str {
        "go"
    }

    fn recognize(&self, file: &AnalyzedFile) -> ParserRecognition {
        if !has_go_extension(file.path()) {
            return ParserRecognition::NotRecognized;
        }

        let window = file.first_content_window();
        if window.content_kind == ContentKind::Text && !window.truncated {
            ParserRecognition::Recognized
        } else {
            ParserRecognition::NotRecognized
        }
    }

    fn parse(&self, file: &AnalyzedFile) -> ParserOutput {
        let mut output = empty_output(self.language_id());
        let window = file.first_content_window();

        if window.truncated {
            output.limitations.push(ParserLimitation {
                code: "truncated_source".to_owned(),
                message: "Go parser skipped content beyond the active file window".to_owned(),
            });
            return output;
        }

        let source = match std::str::from_utf8(&window.bytes) {
            Ok(source) => source,
            Err(source) => {
                output.diagnostics.push(ParserDiagnostic {
                    code: "invalid_utf8".to_owned(),
                    message: format!("Go parser requires UTF-8 source: {source}"),
                });
                return output;
            }
        };

        let mut parser = tree_sitter::Parser::new();
        if let Err(source) = parser.set_language(&tree_sitter_go::LANGUAGE.into()) {
            output.diagnostics.push(ParserDiagnostic {
                code: "parser_language_failed".to_owned(),
                message: format!("failed to initialize Go parser: {source}"),
            });
            return output;
        }

        let Some(tree) = parser.parse(source, None) else {
            output.diagnostics.push(ParserDiagnostic {
                code: "parse_failed".to_owned(),
                message: "tree-sitter returned no Go syntax tree".to_owned(),
            });
            return output;
        };

        let root = tree.root_node();
        if root.has_error() {
            output.diagnostics.push(ParserDiagnostic {
                code: "parse_error".to_owned(),
                message: "Go source contains syntax errors".to_owned(),
            });
        }

        collect_go_facts(root, source, &mut output);
        output
    }
}

fn has_go_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("go"))
}

fn empty_output(language_id: &str) -> ParserOutput {
    ParserOutput {
        language_id: language_id.to_owned(),
        symbols: Vec::new(),
        references: Vec::new(),
        metrics_input: UniversalCodeMetricsInput::default(),
        diagnostics: Vec::new(),
        limitations: Vec::new(),
    }
}

fn collect_go_facts(root: Node<'_>, source: &str, output: &mut ParserOutput) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        collect_top_level_node(child, source, output);
    }
}

fn collect_top_level_node(node: Node<'_>, source: &str, output: &mut ParserOutput) {
    match node.kind() {
        "import_declaration" => collect_imports(node, source, output),
        "function_declaration" => {
            collect_function(node, source, output, UniversalFunctionKind::Function)
        }
        "method_declaration" => {
            collect_function(node, source, output, UniversalFunctionKind::Method)
        }
        "type_declaration" => collect_types(node, source, output),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_top_level_node(child, source, output);
            }
        }
    }
}

fn collect_imports(node: Node<'_>, source: &str, output: &mut ParserOutput) {
    if node.kind() == "import_spec" {
        if let Some(target) = import_target(node, source) {
            output.references.push(UniversalReference {
                target,
                kind: "import".to_owned(),
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_imports(child, source, output);
    }
}

fn import_target(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let target = node
        .children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "interpreted_string_literal" | "raw_string_literal"
            )
        })
        .and_then(|literal| node_text(literal, source))
        .map(strip_go_string_literal);
    target
}

fn strip_go_string_literal(value: String) -> String {
    value.trim_matches('"').trim_matches('`').to_owned()
}

fn collect_types(node: Node<'_>, source: &str, output: &mut ParserOutput) {
    if node.kind() == "type_spec" {
        if let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| node_text(name, source))
        {
            output.symbols.push(UniversalSymbol {
                name,
                kind: type_kind(node),
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_types(child, source, output);
    }
}

fn type_kind(node: Node<'_>) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_type" => return "struct".to_owned(),
            "interface_type" => return "interface".to_owned(),
            _ => {}
        }
    }
    "type".to_owned()
}

fn collect_function(
    node: Node<'_>,
    source: &str,
    output: &mut ParserOutput,
    function_kind: UniversalFunctionKind,
) {
    let name = node
        .child_by_field_name("name")
        .and_then(|name| node_text(name, source))
        .unwrap_or_else(|| "<anonymous>".to_owned());
    let symbol_kind = match function_kind {
        UniversalFunctionKind::Function => "function",
        UniversalFunctionKind::Method => "method",
    };

    output.symbols.push(UniversalSymbol {
        name: name.clone(),
        kind: symbol_kind.to_owned(),
    });

    let control_flow = node
        .child_by_field_name("body")
        .map(|body| collect_control_flow(body, source))
        .unwrap_or_default();

    output.metrics_input.functions.push(UniversalFunction {
        name,
        kind: function_kind,
        control_flow,
    });
}

fn collect_control_flow(node: Node<'_>, source: &str) -> Vec<UniversalControlFlowNode> {
    match node.kind() {
        "if_statement" => vec![control_node(
            UniversalControlFlowKind::Branch,
            collect_control_flow_children(node, source),
        )],
        "for_statement" => vec![control_node(
            UniversalControlFlowKind::Loop,
            collect_control_flow_children(node, source),
        )],
        "expression_switch_statement" | "type_switch_statement" | "select_statement" => {
            vec![control_node(
                UniversalControlFlowKind::Switch,
                collect_control_flow_children(node, source),
            )]
        }
        "expression_case" | "type_case" | "communication_case" | "default_case" => {
            vec![control_node(
                UniversalControlFlowKind::Case,
                collect_control_flow_children(node, source),
            )]
        }
        "binary_expression" if is_boolean_chain(node, source) => vec![control_node(
            UniversalControlFlowKind::BooleanChain,
            collect_control_flow_children(node, source),
        )],
        "break_statement" | "continue_statement" | "goto_statement" => {
            vec![control_node(UniversalControlFlowKind::Jump, Vec::new())]
        }
        _ => collect_control_flow_children(node, source),
    }
}

fn collect_control_flow_children(node: Node<'_>, source: &str) -> Vec<UniversalControlFlowNode> {
    let mut nodes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        nodes.extend(collect_control_flow(child, source));
    }
    nodes
}

fn is_boolean_chain(node: Node<'_>, source: &str) -> bool {
    node_text(node, source).is_some_and(|text| text.contains("&&") || text.contains("||"))
}

fn control_node(
    kind: UniversalControlFlowKind,
    children: Vec<UniversalControlFlowNode>,
) -> UniversalControlFlowNode {
    UniversalControlFlowNode { kind, children }
}

fn node_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::pipeline::code_metrics_analyzer::CodeMetricsAnalyzer;
    use crate::pipeline::file_analyzer::{AnalyzedFile, FileAnalyzerOptions};

    static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::current_dir()
                .expect("test should have current directory")
                .join("target")
                .join("go-parser-fixtures")
                .join(format!("{name}-{}-{id}", std::process::id()));

            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("fixture root should be created");

            Self { path }
        }

        fn write(&self, relative_path: impl AsRef<Path>, contents: &str) -> PathBuf {
            let path = self.path.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("fixture parent should be created");
            }
            fs::write(&path, contents).expect("fixture file should be written");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn recognizes_go_files_only() {
        let fixture = Fixture::new("recognize");
        let go = AnalyzedFile::new(
            fixture.write("main.go", "package main\n"),
            FileAnalyzerOptions {
                content_window_bytes: 1024,
                parsers: Vec::new(),
            },
        );
        let rust = AnalyzedFile::new(
            fixture.write("main.rs", "fn main() {}\n"),
            FileAnalyzerOptions {
                content_window_bytes: 1024,
                parsers: Vec::new(),
            },
        );
        let parser = GoParser::new();

        assert_eq!(parser.recognize(&go), ParserRecognition::Recognized);
        assert_eq!(parser.recognize(&rust), ParserRecognition::NotRecognized);
    }

    #[test]
    fn parses_go_symbols_imports_and_metric_input() {
        let fixture = Fixture::new("parse");
        let path = fixture.write(
            "main.go",
            r#"
package main

import (
    "fmt"
    alias "strings"
)

type Service struct{}
type Runner interface { Run() }

func main() {
    if true && false {
        for i := 0; i < 1; i++ {
            continue
        }
    }
}

func (s Service) Run() {
    switch 1 {
    case 1:
        fmt.Println(alias.TrimSpace("x"))
    default:
    }
}
"#,
        );
        let file = AnalyzedFile::new(
            path,
            FileAnalyzerOptions {
                content_window_bytes: 4096,
                parsers: Vec::new(),
            },
        );

        let output = GoParser::new().parse(&file);
        let complexity = CodeMetricsAnalyzer::new().analyze(&output.metrics_input);

        assert_eq!(output.language_id, "go");
        assert_eq!(output.references.len(), 2);
        assert_eq!(output.metrics_input.functions.len(), 2);
        assert!(output
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "function" && symbol.name == "main"));
        assert!(output
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "method" && symbol.name == "Run"));
        assert!(output
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "struct" && symbol.name == "Service"));
        assert!(output
            .symbols
            .iter()
            .any(|symbol| symbol.kind == "interface" && symbol.name == "Runner"));
        assert!(complexity.cognitive_complexity > 0);
    }
}
