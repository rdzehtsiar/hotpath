# Metrics

Hotpath metrics are advisory local signals for codebase investigation. They are
not proof that code is defective, risky, or badly designed. Hotpath is still at
the beginning of development, so the command surface, JSON schemas, formulas,
and local index layout are not compatibility promises.

This page defines the public metrics currently exposed by scanner, parser,
complexity, graph, Git, and hotspot workflows.

## Scope

Current metrics come from local repository files, parser output, conservative
dependency resolution, and local Git history reachable from `HEAD`. The core
workflow should not require network access, telemetry, cloud APIs, or hosted
services.

Current machine-readable schema identifiers include:

| Area | Command surface | Schema identifier |
| --- | --- | --- |
| Scanner report | `hotpath scan --json` | `hotpath.scan.v1` |
| Parser report | `hotpath parse --json` | `hotpath.parse.v1` |
| Complexity report | `hotpath complexity --json` | `hotpath.complexity.v1` |
| Dependency graph report | `hotpath graph --module <selector> --json` | `hotpath.graph.v1` |
| Hotspot scoring | `hotpath hotspots`, `hotpath explain` | `hotpath.score.v1` |

These identifiers describe current output, not a stable released contract.

## Scanner Metrics

Scanner metrics describe repository files without interpreting full language
semantics.

| Metric | Definition | Why it matters | Limitations |
| --- | --- | --- | --- |
| `byte_size` | File size in bytes from local filesystem metadata or file reads. | Large files can be harder to review, load, and reason about. | Generated files, data files, vendored source, and intentional consolidation can make size misleading. |
| `line_count` | Text line count when Hotpath can read the file as supported text. | Line count is a simple size signal and is used by hotspot size and recent-growth scoring. | Unsupported encodings, binary files, minified files, and generated files can limit usefulness. |
| language guess | Best-effort language classification from path and extension. | Language classification controls parser eligibility and report grouping. | It is not a compiler or build-system decision and can be wrong for unusual layouts. |
| content kind | Classification such as text or binary where detectable. | Avoids treating binary content as normal source text. | Detection is conservative and may not capture every encoding or generated artifact. |
| generated/vendor flags | Best-effort path-based generated and vendor classification. | Helps users distinguish authored source from code they may not want to optimize manually. | Current hotspot scoring stores these facts but does not weight or exclude them in `hotpath.score.v1`. |

Current scan summaries expose:

| Summary metric | Definition |
| --- | --- |
| `total_files` | Number of files observed in the current scan. |
| `total_bytes` | Sum of available file byte sizes. |
| `content.text_files`, `content.binary_files`, `content.unknown_files` | Counts by current content classification. |
| `flags.generated_files`, `flags.vendor_files` | Counts of files flagged by generated/vendor path heuristics. |
| `warnings.total_warnings` and warning subcounts | Counts of scan-level and file-level warnings. |
| `languages` | Deterministic map of guessed language name to file count. |

## Parser And Symbol Metrics

Parser output is currently limited to Rust, Go, TypeScript, and TSX. Python and
other languages are skipped as unsupported.

Current parser reports can include modules, packages, namespaces, imports,
functions, methods, classes and types, symbol ranges, parent/nesting metadata,
and concise signatures. Files with syntax errors can still produce partial
output when the parser recovers enough of the tree.

| Metric | Definition | Why it matters | Limitations |
| --- | --- | --- | --- |
| symbol length | Number of source lines covered by an extracted symbol range. | Very large symbols can concentrate review, testing, and change risk. | Range-based length can include comments, blank lines, nested declarations, and parser recovery artifacts. |
| function length | Symbol length for extracted functions and methods. | Long functions and methods are often harder to inspect and test. | It is not a semantic measure of responsibility, branching, or domain complexity. |
| large symbol threshold | A symbol is treated as large when its length is `>= 80` lines. | Provides a simple explainable threshold for highlighting unusually large local units. | The threshold is intentionally rough and may not fit every language, style, or generated file. |
| cyclomatic complexity approximation | A parser-derived branch/control-flow count for extracted functions and methods. Complexity reports expose the maximum observed value. | Higher branch counts can indicate more paths to understand and test. | It is an approximation from parsed syntax, not compiler-level semantic complexity. Macro expansion, dynamic dispatch, generated code, and parser recovery can distort it. |
| control-flow nesting approximation | Maximum nesting depth of parsed control-flow constructs. Complexity reports expose the maximum observed value. | Deep nesting can make local reasoning and review harder. | It is syntax-based and does not understand all language semantics or refactoring intent. |

Current parse summaries expose:

| Summary metric | Definition |
| --- | --- |
| `total_files` | Number of scan file records considered by the parser report. |
| `candidate_files` | Number of files not skipped for unsupported content or unsupported language. |
| `parsed_files` | Number of files with parse status `parsed`. |
| `pending_files` | Number of files with parse status `pending`. |
| `skipped_files` | Number of files with parse status `skipped`. |
| `symbol_count` | Number of extracted parser symbols in the report. |
| `import_count` | Number of raw parser import records in the report, before conservative dependency resolution. |
| `warning_count` | Number of parser report warnings. |

These are report counters, not claims of complete semantic coverage.

Current complexity summaries expose:

| Summary metric | Definition |
| --- | --- |
| `total_files` | Number of parse file records considered by the complexity report. |
| `parsed_files` | Number of files successfully parsed by the current parser flow. |
| `symbol_count` | Number of extracted parser symbols in the report. |
| `function_method_count` | Number of extracted symbols whose kind is `function` or `method`. |
| `large_symbol_count` | Number of extracted symbols with `length_lines >= 80`. |
| `max_cyclomatic_complexity` | Highest parser-derived cyclomatic complexity approximation among functions and methods, or `0` when none exist. |
| `max_nesting_depth` | Highest parser-derived control-flow nesting approximation among functions and methods, or `0` when none exist. |
| `dependency_edge_count` | Number of conservatively resolved local dependency edges in the current report. |
| `max_fan_in` | Highest per-file fan-in in the current report. |
| `max_fan_out` | Highest per-file fan-out in the current report. |

## Dependency And Coupling Metrics

Hotpath now persists conservative local dependency edges during
parse/complexity/graph flows. These edges are derived only when Hotpath can
safely resolve a parser-observed relationship to another indexed repository
file.

Currently resolved dependency scope:

- Rust `mod` declarations where the local module file can be resolved safely.
- Rust `crate::` and `self::` use paths where the local target can be resolved
  safely.
- TypeScript and TSX relative imports where the local target can be resolved
  safely.

Currently unresolved or disabled dependency scope:

- Go dependency edges are disabled/unresolved for now.
- External package imports are not stored as local dependency edges.
- Group, glob, ambiguous, or otherwise unsafe imports are not stored as resolved
  edges.

| Metric | Definition | Formula or derivation | Why it matters | Limitations |
| --- | --- | --- | --- | --- |
| dependency edge | A resolved local file-to-file relationship from a source file to a target file. | Persisted only when resolver rules safely map the parser-observed relationship to indexed repository files. | Shows direct local code relationships that may matter during changes. | Missing edges are expected for unsupported languages, external packages, ambiguous imports, generated code, and resolver gaps. |
| `dependency_edge_count` | Count of resolved local dependency edges in the current report scope. | `count(resolved_edges)` after conservative resolution. | Gives a small coupling-size signal for the analyzed scope. | It is not a complete build graph, package graph, or runtime dependency graph. |
| per-file `fan_out` | Number of distinct local files a file depends on through resolved edges. | `count(distinct target_path where source_path = file_path)`. | High fan-out can indicate broad local knowledge required to change a file. | Conservative resolver scope can undercount dependencies. |
| per-file `fan_in` | Number of distinct local files that depend on a file through resolved edges. | `count(distinct source_path where target_path = file_path)`. | High fan-in can indicate changes may affect more local callers or modules. | It only counts resolved local file edges, not package users, dynamic references, or external consumers. |
| max fan-in/out | Highest per-file fan-in or fan-out observed in the report scope. | `max(file.fan_in)` and `max(file.fan_out)`. | Highlights the strongest local coupling concentration in a scope. | The maximum can be dominated by generated, barrel, facade, or central module files. |

`hotpath graph --module <selector>` exposes the current resolved dependency
graph for the selected module scope. The graph report is a local, conservative
view of indexed dependency edges, not a complete architectural model.

Current graph summaries expose:

| Summary metric | Definition |
| --- | --- |
| `matched_file_count` | Number of current parsed files matched by the `--module` selector. |
| `outgoing_edge_count` | Number of one-hop resolved local edges from matched files to other files. |
| `incoming_edge_count` | Number of one-hop resolved local edges from other files to matched files. |

Graph selectors are path-prefix or bare-module filters over repository-relative
file paths. A no-match selector succeeds with empty sections so scripts can
handle absent modules deterministically.

## Git Metrics

Git metrics are derived from local history reachable from `HEAD`. They include
per-file commit counts, total churn, recent churn, author count, dominant owner
share, file age, and co-change pairs. The default recent-history window is `90`
days before the `HEAD` committer timestamp.

See [Git metric semantics](git-metrics.md) for formulas, ordering rules, and
known limitations.

## Hotspot Scores

Hotspot scores currently combine scanner facts with local Git metrics using the
documented `hotpath.score.v1` formula:

```text
score =
  0.35 * churn
+ 0.20 * size
+ 0.20 * ownership
+ 0.15 * recent_churn
+ 0.10 * coupling
```

The current hotspot formula does not consume parser-derived complexity metrics,
symbol metrics, or resolved dependency edges yet. Its `coupling` term is based
on Git co-change breadth, not parser dependency fan-in or fan-out.

See [Scoring](scoring.md) for normalization, ranking, limitation codes, and
formula details.

## Determinism And Interpretation

Where practical, metric rows and reports should use repository-relative paths,
stable sorting, deterministic formulas, and no host-specific absolute paths in
portable output.

Every metric should be read with its scope and limitations. A high value means
"worth investigating," not "wrong." A low or missing value can mean the relevant
signal is absent, unsupported, unresolved, or outside Hotpath's current local
analysis scope.
