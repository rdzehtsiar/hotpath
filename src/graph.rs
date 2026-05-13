// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use serde::Serialize;

use crate::{dependency, ParseReport, ResolvedDependencyEdge};

pub const GRAPH_SCHEMA_VERSION: &str = "hotpath.graph.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphSummary {
    pub matched_file_count: u64,
    pub outgoing_edge_count: u64,
    pub incoming_edge_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphReport {
    pub selector: String,
    pub summary: GraphSummary,
    pub matched_files: Vec<String>,
    pub outgoing: Vec<ResolvedDependencyEdge>,
    pub incoming: Vec<ResolvedDependencyEdge>,
}

#[derive(Debug, Serialize)]
struct GraphJsonReport<'a> {
    schema_version: &'static str,
    selector: &'a str,
    summary: &'a GraphSummary,
    matched_files: &'a [String],
    outgoing: &'a [ResolvedDependencyEdge],
    incoming: &'a [ResolvedDependencyEdge],
}

pub fn report_from_parse(selector: &str, report: &ParseReport) -> GraphReport {
    let selector = normalize_selector(selector);
    let matched_files = report
        .files
        .iter()
        .filter(|file| selector_matches_path(&selector, &file.path))
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    let edges = dependency::resolve_dependencies(report);

    let mut outgoing = edges
        .iter()
        .filter(|edge| matched_files.contains(&edge.source_path))
        .cloned()
        .collect::<Vec<_>>();
    let mut incoming = edges
        .iter()
        .filter(|edge| matched_files.contains(&edge.target_path))
        .cloned()
        .collect::<Vec<_>>();
    sort_edges(&mut outgoing);
    sort_edges(&mut incoming);

    GraphReport {
        selector,
        summary: GraphSummary {
            matched_file_count: matched_files.len() as u64,
            outgoing_edge_count: outgoing.len() as u64,
            incoming_edge_count: incoming.len() as u64,
        },
        matched_files: matched_files.into_iter().collect(),
        outgoing,
        incoming,
    }
}

pub fn render_json(report: &GraphReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&GraphJsonReport {
        schema_version: GRAPH_SCHEMA_VERSION,
        selector: &report.selector,
        summary: &report.summary,
        matched_files: &report.matched_files,
        outgoing: &report.outgoing,
        incoming: &report.incoming,
    })
}

pub fn render_summary(report: &GraphReport) -> String {
    let mut output = format!(
        "Hotpath dependency graph\nselector       {}\nmatched files  {}\noutgoing       {}\nincoming       {}",
        report.selector,
        report.summary.matched_file_count,
        report.summary.outgoing_edge_count,
        report.summary.incoming_edge_count
    );

    output.push_str("\n\nmatched files");
    push_paths(&mut output, &report.matched_files);

    output.push_str("\n\noutgoing");
    push_edges(&mut output, &report.outgoing);

    output.push_str("\n\nincoming");
    push_edges(&mut output, &report.incoming);

    output
}

fn push_paths(output: &mut String, paths: &[String]) {
    if paths.is_empty() {
        output.push_str("\n  none");
        return;
    }

    for path in paths {
        output.push_str(&format!("\n  {path}"));
    }
}

fn push_edges(output: &mut String, edges: &[ResolvedDependencyEdge]) {
    if edges.is_empty() {
        output.push_str("\n  none");
        return;
    }

    for edge in edges {
        output.push_str(&format!(
            "\n  {} -> {}  {}",
            edge.source_path, edge.target_path, edge.kind
        ));
    }
}

fn selector_matches_path(selector: &str, path: &str) -> bool {
    let selector = normalize_selector(selector);
    if selector.is_empty() {
        return false;
    }

    let path = normalize_path(path);
    if selector_is_path_prefix(&selector) {
        path_selector_matches_path(&selector, &path)
    } else {
        bare_selector_matches_path(&selector, &path)
    }
}

fn selector_is_path_prefix(selector: &str) -> bool {
    selector.contains('/') || has_known_source_extension(selector)
}

fn path_selector_matches_path(selector: &str, path: &str) -> bool {
    path == selector
        || path
            .strip_prefix(selector)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('.'))
}

fn bare_selector_matches_path(selector: &str, path: &str) -> bool {
    path.split('/').any(|component| component == selector)
        || file_stem(path).is_some_and(|stem| stem == selector)
}

fn normalize_selector(selector: &str) -> String {
    trim_leading_current_dir(&selector.replace('\\', "/")).to_owned()
}

fn normalize_path(path: &str) -> String {
    trim_leading_current_dir(&path.replace('\\', "/")).to_owned()
}

fn trim_leading_current_dir(path: &str) -> &str {
    let mut trimmed = path;
    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }
    trimmed
}

fn file_stem(path: &str) -> Option<&str> {
    let file_name = path.rsplit('/').next()?;
    file_name
        .rsplit_once('.')
        .map_or(Some(file_name), |(stem, _)| {
            if stem.is_empty() {
                None
            } else {
                Some(stem)
            }
        })
}

fn has_known_source_extension(selector: &str) -> bool {
    let Some((_, extension)) = selector.rsplit_once('.') else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "bash"
            | "c"
            | "cc"
            | "cpp"
            | "cs"
            | "cxx"
            | "go"
            | "h"
            | "hh"
            | "hpp"
            | "hxx"
            | "java"
            | "js"
            | "jsx"
            | "kt"
            | "kts"
            | "mjs"
            | "cjs"
            | "php"
            | "proto"
            | "ps1"
            | "py"
            | "rb"
            | "rs"
            | "scala"
            | "sh"
            | "swift"
            | "ts"
            | "tsx"
            | "zsh"
    )
}

fn sort_edges(edges: &mut [ResolvedDependencyEdge]) {
    edges.sort_by(|left, right| {
        (&left.source_path, &left.target_path, &left.kind).cmp(&(
            &right.source_path,
            &right.target_path,
            &right.kind,
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_support::{parse_import as import, parsed_text_file as file},
        ParseFileRecord, ParseImportRecord,
    };

    fn report(files: Vec<ParseFileRecord>, imports: Vec<ParseImportRecord>) -> ParseReport {
        ParseReport {
            warnings: Vec::new(),
            files,
            symbols: Vec::new(),
            imports,
        }
    }

    #[test]
    fn bare_selector_matches_components_and_file_stems() {
        assert!(selector_matches_path("auth", "src/auth.rs"));
        assert!(selector_matches_path("auth", "src/auth/mod.rs"));
        assert!(selector_matches_path("auth", "src/auth/login.rs"));
        assert!(selector_matches_path("auth", "web/auth.ts"));
        assert!(!selector_matches_path("auth", "web/authorize.ts"));
    }

    #[test]
    fn path_selector_matches_normalized_repository_relative_prefixes() {
        assert!(selector_matches_path("src/auth", "src/auth.rs"));
        assert!(selector_matches_path("./src\\auth", "src/auth/login.rs"));
        assert!(selector_matches_path("auth.rs", "auth.rs"));
        assert!(!selector_matches_path("src/auth", "src/authz.rs"));
        assert!(!selector_matches_path(
            "src/auth",
            "src/authentication/mod.rs"
        ));
        assert!(!selector_matches_path("auth.rs", "src/auth.rs"));
        assert!(!selector_matches_path("", "src/auth.rs"));
    }

    #[test]
    fn report_includes_matched_files_and_one_hop_edges() {
        let report = report(
            vec![
                file("src/lib.rs", "Rust"),
                file("src/auth.rs", "Rust"),
                file("src/models.rs", "Rust"),
            ],
            vec![
                import("src/lib.rs", "auth", "mod"),
                import("src/auth.rs", "crate::models::User", "use"),
            ],
        );

        let graph = report_from_parse("auth", &report);

        assert_eq!(graph.selector, "auth");
        assert_eq!(graph.matched_files, vec!["src/auth.rs"]);
        assert_eq!(graph.summary.matched_file_count, 1);
        assert_eq!(graph.summary.outgoing_edge_count, 1);
        assert_eq!(graph.summary.incoming_edge_count, 1);
        assert_eq!(graph.outgoing[0].source_path, "src/auth.rs");
        assert_eq!(graph.outgoing[0].target_path, "src/models.rs");
        assert_eq!(graph.outgoing[0].kind, "use");
        assert_eq!(graph.incoming[0].source_path, "src/lib.rs");
        assert_eq!(graph.incoming[0].target_path, "src/auth.rs");
        assert_eq!(graph.incoming[0].kind, "mod");
    }

    #[test]
    fn report_shape_is_empty_for_no_matches() {
        let graph = report_from_parse("missing", &report(vec![file("src/lib.rs", "Rust")], vec![]));

        assert_eq!(graph.summary.matched_file_count, 0);
        assert!(graph.matched_files.is_empty());
        assert!(graph.outgoing.is_empty());
        assert!(graph.incoming.is_empty());
        assert!(render_summary(&graph).contains("  none"));
    }
}
