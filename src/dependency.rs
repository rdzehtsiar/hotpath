// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{ParseFileRecord, ParseFileStatus, ParseImportRecord, ParseReport, ParseSymbolRecord};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ResolvedDependencyEdge {
    pub source_path: String,
    pub target_path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FileDependencyFan {
    pub fan_in: u64,
    pub fan_out: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFanMetrics {
    pub by_path: BTreeMap<String, FileDependencyFan>,
    pub dependency_edge_count: u64,
    pub max_fan_in: u64,
    pub max_fan_out: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyLanguage {
    Rust,
    Go,
    TypeScript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedFile<'a> {
    path: &'a str,
    language: DependencyLanguage,
}

pub fn resolve_dependencies(report: &ParseReport) -> Vec<ResolvedDependencyEdge> {
    let parsed_files = report
        .files
        .iter()
        .filter_map(parsed_dependency_file)
        .collect::<Vec<_>>();
    let parsed_paths = parsed_files
        .iter()
        .map(|file| file.path.to_owned())
        .collect::<BTreeSet<_>>();
    let parsed_by_path = parsed_files
        .iter()
        .map(|file| (file.path, file.language))
        .collect::<BTreeMap<_, _>>();

    let mut edges = BTreeSet::new();
    for import in &report.imports {
        let Some(language) = parsed_by_path.get(import.path.as_str()).copied() else {
            continue;
        };
        if language == DependencyLanguage::Rust
            && import.kind == "mod"
            && rust_mod_import_is_nested_in_module(import, &report.symbols)
        {
            continue;
        }
        let Some(target_path) = resolve_import(import, language, &parsed_paths) else {
            continue;
        };

        edges.insert(ResolvedDependencyEdge {
            source_path: import.path.clone(),
            target_path,
            kind: import.kind.clone(),
        });
    }

    edges.into_iter().collect()
}

fn rust_mod_import_is_nested_in_module(
    import: &ParseImportRecord,
    symbols: &[ParseSymbolRecord],
) -> bool {
    symbols.iter().any(|symbol| {
        symbol.path == import.path
            && symbol.kind == "module"
            && symbol.start_line < import.start_line
            && symbol.end_line >= import.end_line
    })
}

pub fn fan_metrics(
    files: &[ParseFileRecord],
    edges: &[ResolvedDependencyEdge],
) -> DependencyFanMetrics {
    let mut outgoing = BTreeMap::<String, BTreeSet<String>>::new();
    let mut incoming = BTreeMap::<String, BTreeSet<String>>::new();

    for file in files {
        outgoing.entry(file.path.clone()).or_default();
        incoming.entry(file.path.clone()).or_default();
    }

    for edge in edges {
        outgoing
            .entry(edge.source_path.clone())
            .or_default()
            .insert(edge.target_path.clone());
        incoming
            .entry(edge.target_path.clone())
            .or_default()
            .insert(edge.source_path.clone());
    }

    let paths = outgoing
        .keys()
        .chain(incoming.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut by_path = BTreeMap::new();
    let mut max_fan_in = 0;
    let mut max_fan_out = 0;

    for path in paths {
        let fan_in = incoming.get(&path).map_or(0, BTreeSet::len) as u64;
        let fan_out = outgoing.get(&path).map_or(0, BTreeSet::len) as u64;
        max_fan_in = max_fan_in.max(fan_in);
        max_fan_out = max_fan_out.max(fan_out);
        by_path.insert(path, FileDependencyFan { fan_in, fan_out });
    }

    DependencyFanMetrics {
        by_path,
        dependency_edge_count: edges.len() as u64,
        max_fan_in,
        max_fan_out,
    }
}

fn parsed_dependency_file(file: &ParseFileRecord) -> Option<ParsedFile<'_>> {
    if file.status != ParseFileStatus::Parsed || !is_safe_repo_path(&file.path) {
        return None;
    }

    Some(ParsedFile {
        path: &file.path,
        language: dependency_language(file)?,
    })
}

fn dependency_language(file: &ParseFileRecord) -> Option<DependencyLanguage> {
    if file.path.ends_with(".rs") || file.language == Some("Rust") {
        Some(DependencyLanguage::Rust)
    } else if file.path.ends_with(".go") || file.language == Some("Go") {
        Some(DependencyLanguage::Go)
    } else if file.path.ends_with(".ts")
        || file.path.ends_with(".tsx")
        || matches!(file.language, Some("TypeScript" | "TypeScript JSX"))
    {
        Some(DependencyLanguage::TypeScript)
    } else {
        None
    }
}

fn resolve_import(
    import: &ParseImportRecord,
    language: DependencyLanguage,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    match language {
        DependencyLanguage::Rust => resolve_rust_import(import, parsed_paths),
        DependencyLanguage::Go => None,
        DependencyLanguage::TypeScript => resolve_typescript_import(import, parsed_paths),
    }
    .filter(|path| is_safe_repo_path(path))
}

fn resolve_rust_import(
    import: &ParseImportRecord,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    match import.kind.as_str() {
        "mod" => resolve_rust_mod(&import.path, &import.target, parsed_paths),
        "use" => resolve_rust_use(&import.path, &import.target, parsed_paths),
        _ => None,
    }
}

fn resolve_rust_mod(
    source_path: &str,
    target: &str,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    if !is_rust_identifier(target) {
        return None;
    }

    let base = rust_child_module_base(source_path)?;
    let module_path = append_path(&base, target);
    exactly_one_existing(module_file_candidates(&module_path), parsed_paths)
}

fn resolve_rust_use(
    source_path: &str,
    target: &str,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    if target.contains('{')
        || target.contains('}')
        || target.contains('*')
        || target.contains(" as ")
    {
        return None;
    }

    let (base, rest) = if let Some(rest) = target.strip_prefix("crate::") {
        (rust_crate_root(source_path)?, rest)
    } else if let Some(rest) = target.strip_prefix("self::") {
        (rust_child_module_base(source_path)?, rest)
    } else {
        return None;
    };
    let segments = rest
        .split("::")
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() || segments.iter().any(|segment| !is_rust_identifier(segment)) {
        return None;
    }

    let mut candidates = Vec::new();
    for index in 1..=segments.len() {
        let module_path = append_segments(&base, &segments[..index]);
        candidates.extend(module_file_candidates(&module_path));
    }

    exactly_one_existing(candidates, parsed_paths)
}

fn resolve_typescript_import(
    import: &ParseImportRecord,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    if !(import.target.starts_with("./") || import.target.starts_with("../")) {
        return None;
    }
    if import.target.contains('\\') || import.target.contains('\0') {
        return None;
    }

    let source_dir = parent_dir(&import.path);
    let base = normalize_relative_path(source_dir.as_deref(), &import.target)?;
    let candidates = if base.ends_with(".ts") || base.ends_with(".tsx") {
        vec![base]
    } else {
        vec![
            format!("{base}.ts"),
            format!("{base}.tsx"),
            format!("{base}/index.ts"),
            format!("{base}/index.tsx"),
        ]
    };

    exactly_one_existing(candidates, parsed_paths)
}

fn exactly_one_existing(
    candidates: Vec<String>,
    parsed_paths: &BTreeSet<String>,
) -> Option<String> {
    let existing = candidates
        .into_iter()
        .filter(|candidate| parsed_paths.contains(candidate))
        .collect::<BTreeSet<_>>();

    if existing.len() == 1 {
        existing.into_iter().next()
    } else {
        None
    }
}

fn module_file_candidates(module_path: &str) -> Vec<String> {
    vec![format!("{module_path}.rs"), format!("{module_path}/mod.rs")]
}

fn rust_crate_root(source_path: &str) -> Option<String> {
    let parts = source_path.split('/').collect::<Vec<_>>();
    let src_index = parts.iter().rposition(|part| *part == "src")?;

    Some(parts[..=src_index].join("/"))
}

fn rust_child_module_base(source_path: &str) -> Option<String> {
    let parent = parent_dir(source_path);
    let file_name = source_path.rsplit('/').next()?;

    if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        return Some(parent.unwrap_or_default());
    }

    let stem = file_name.strip_suffix(".rs")?;
    Some(append_path(parent.as_deref().unwrap_or_default(), stem))
}

fn normalize_relative_path(base_dir: Option<&str>, target: &str) -> Option<String> {
    let mut parts = base_dir
        .unwrap_or_default()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn parent_dir(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(parent, _)| parent.to_owned())
}

fn append_segments(base: &str, segments: &[&str]) -> String {
    segments
        .iter()
        .fold(base.to_owned(), |path, segment| append_path(&path, segment))
}

fn append_path(base: &str, child: &str) -> String {
    if base.is_empty() {
        child.to_owned()
    } else {
        format!("{base}/{child}")
    }
}

fn is_rust_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_safe_repo_path(path: &str) -> bool {
    !path.is_empty()
        && path != ".."
        && !path.starts_with('/')
        && !path.starts_with("./")
        && !path.starts_with("../")
        && !path.starts_with('~')
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.contains("/../")
        && !path.ends_with("/..")
        && !looks_like_windows_absolute_path(path)
}

fn looks_like_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();

    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_support, ContentKind};

    fn file(path: &str, language: &'static str) -> ParseFileRecord {
        ParseFileRecord {
            path: path.to_owned(),
            language: Some(language),
            content: ContentKind::Text,
            status: ParseFileStatus::Parsed,
            reason: None,
            symbol_count: 0,
            import_count: 0,
        }
    }

    fn report(files: Vec<ParseFileRecord>, imports: Vec<ParseImportRecord>) -> ParseReport {
        ParseReport {
            warnings: Vec::new(),
            files,
            symbols: Vec::new(),
            imports,
        }
    }

    fn edge_paths(edges: &[ResolvedDependencyEdge]) -> Vec<(&str, &str, &str)> {
        edges
            .iter()
            .map(|edge| {
                (
                    edge.source_path.as_str(),
                    edge.target_path.as_str(),
                    edge.kind.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn resolves_conservative_rust_module_and_use_edges() {
        let report = report(
            vec![
                file("src/lib.rs", "Rust"),
                file("src/child.rs", "Rust"),
                file("src/models/mod.rs", "Rust"),
            ],
            vec![
                test_support::parse_import("src/lib.rs", "child", "mod"),
                test_support::parse_import("src/lib.rs", "crate::models::Widget", "use"),
                test_support::parse_import("src/lib.rs", "std::fmt", "use"),
                test_support::parse_import("src/lib.rs", "crate::{fmt, io}", "use"),
            ],
        );

        let edges = resolve_dependencies(&report);

        assert_eq!(
            edge_paths(&edges),
            vec![
                ("src/lib.rs", "src/child.rs", "mod"),
                ("src/lib.rs", "src/models/mod.rs", "use"),
            ]
        );
    }

    #[test]
    fn leaves_nested_rust_mod_declarations_unresolved() {
        let mut report = report(
            vec![file("src/lib.rs", "Rust"), file("src/child.rs", "Rust")],
            vec![test_support::parse_import("src/lib.rs", "child", "mod")],
        );
        report.imports[0].start_line = 2;
        report.imports[0].end_line = 2;
        report.symbols.push(ParseSymbolRecord {
            path: "src/lib.rs".to_owned(),
            name: "outer".to_owned(),
            kind: "module".to_owned(),
            start_line: 1,
            end_line: 3,
            signature: Some("mod outer".to_owned()),
            nesting_depth: 0,
            parent: None,
            cyclomatic_complexity: None,
            max_control_flow_nesting: None,
        });

        assert!(resolve_dependencies(&report).is_empty());
    }

    #[test]
    fn leaves_ambiguous_rust_and_typescript_imports_unresolved() {
        let report = report(
            vec![
                file("src/lib.rs", "Rust"),
                file("src/child.rs", "Rust"),
                file("src/child/mod.rs", "Rust"),
                file("web/app.ts", "TypeScript"),
                file("web/value.ts", "TypeScript"),
                file("web/value.tsx", "TypeScript JSX"),
            ],
            vec![
                test_support::parse_import("src/lib.rs", "child", "mod"),
                test_support::parse_import("web/app.ts", "./value", "import"),
            ],
        );

        assert!(resolve_dependencies(&report).is_empty());
    }

    #[test]
    fn resolves_typescript_relative_imports_and_rejects_escapes() {
        let report = report(
            vec![
                file("web/app.ts", "TypeScript"),
                file("web/components/index.tsx", "TypeScript JSX"),
                file("shared/util.ts", "TypeScript"),
            ],
            vec![
                test_support::parse_import("web/app.ts", "./components", "import"),
                test_support::parse_import("web/app.ts", "../shared/util", "import"),
                test_support::parse_import("web/app.ts", "../../outside", "import"),
                test_support::parse_import("web/app.ts", "react", "import"),
            ],
        );

        let edges = resolve_dependencies(&report);

        assert_eq!(
            edge_paths(&edges),
            vec![
                ("web/app.ts", "shared/util.ts", "import"),
                ("web/app.ts", "web/components/index.tsx", "import"),
            ]
        );
    }

    #[test]
    fn fan_metrics_count_distinct_sources_and_targets() {
        let files = vec![
            file("src/a.rs", "Rust"),
            file("src/b.rs", "Rust"),
            file("src/c.rs", "Rust"),
        ];
        let edges = vec![
            ResolvedDependencyEdge {
                source_path: "src/a.rs".to_owned(),
                target_path: "src/b.rs".to_owned(),
                kind: "use".to_owned(),
            },
            ResolvedDependencyEdge {
                source_path: "src/a.rs".to_owned(),
                target_path: "src/b.rs".to_owned(),
                kind: "mod".to_owned(),
            },
            ResolvedDependencyEdge {
                source_path: "src/c.rs".to_owned(),
                target_path: "src/b.rs".to_owned(),
                kind: "use".to_owned(),
            },
        ];

        let fan = fan_metrics(&files, &edges);

        assert_eq!(fan.dependency_edge_count, 3);
        assert_eq!(
            fan.by_path["src/a.rs"],
            FileDependencyFan {
                fan_in: 0,
                fan_out: 1
            }
        );
        assert_eq!(
            fan.by_path["src/b.rs"],
            FileDependencyFan {
                fan_in: 2,
                fan_out: 0
            }
        );
        assert_eq!(fan.max_fan_in, 2);
        assert_eq!(fan.max_fan_out, 1);
    }
}
