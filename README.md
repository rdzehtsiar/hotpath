# Hotpath

[![Tests](https://github.com/rdzehtsiar/hotpath/actions/workflows/tests.yml/badge.svg)](https://github.com/rdzehtsiar/hotpath/actions/workflows/tests.yml)
[![codecov](https://codecov.io/gh/rdzehtsiar/hotpath/graph/badge.svg)](https://codecov.io/gh/rdzehtsiar/hotpath)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=rdzehtsiar_hotpath&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=rdzehtsiar_hotpath)

Hotpath is an offline, local-first codebase intelligence tool in early
development. Its long-term purpose is to help engineers identify risky,
expensive, unstable, bloated, or architecturally drifting areas in a repository
without uploading source code or depending on hosted services.

The implementation is currently much narrower than that product direction. This
README documents only what exists in the current codebase.

## Current CLI

The CLI currently exposes these subcommands:

```powershell
hotpath scan
hotpath explain <path>
hotpath tui
```

### `hotpath scan`

`hotpath scan` runs from the current working directory. It:

- enumerates repository files with local ignore rules
- reads file metadata and a bounded content window
- classifies basic content kind, generated paths, and vendor paths
- parses supported Go files
- computes approximate Go-derived source metrics where possible
- collects bounded local Git history when the directory is a non-shallow Git
  worktree
- persists derived local data to the Hotpath index
- prints terminal progress for file and Git processing

`hotpath scan --json` writes a compact JSON scan summary. There is no current CI
gate or stable report schema beyond the explicitly versioned command JSON.

### `hotpath explain <path>`

`hotpath explain <path>` reads the nearest existing `.hotpath/index.sqlite` and
prints the indexed context for one file. It is read-only: it does not refresh the
index, rerun analysis, or mutate `.hotpath/`. If the index is missing, stale, or
does not contain the requested file, run `hotpath scan` first.

The path may be repository-relative or an absolute path under the indexed
repository root. Output defaults to terminal text:

```powershell
hotpath explain internal/service/service.go
hotpath explain internal/service/service.go --format text
hotpath explain internal/service/service.go --format json
```

Text output includes file facts, raw metrics, normalized score terms, weights,
score facts, limitations, parser diagnostics, ownership rows, Git collection
context, co-change partners, and source-coupling context. JSON output uses the
versioned `hotpath.explain.v1` schema for automation.

Files that were scanned but not scored still explain their indexed facts and
return success. Their score is reported as unavailable with reasons such as
unsupported language, generated/vendor exclusion, missing parser metrics, or a
missing score row.

### `hotpath tui`

`hotpath tui` opens an early read-only terminal UI over the local Hotpath index.
It is keyboard-first and unstable. It should not be treated as a stable UI
contract.

## Local Index

Scan output is persisted as derived local cache data at:

```text
.hotpath/index.sqlite
```

The index is local working data, not a stable public database format. It may
contain repository-relative paths, file metadata, parser-derived Go facts, Git
metrics, source dependency rows, and Go file, package, and project risk score
rows. It can be deleted and rebuilt from local repository data.

Do not commit `.hotpath/`.

## Language Support

Only Go is currently supported for language-aware processing.

Hotpath still scans files in other languages as files, but the default analyzer
only registers the Go parser. Rust, TypeScript, TSX, Python, and other language
files are not parsed by the current default pipeline and do not receive
language-derived metrics or Go file risk scores.

## Go Processing Limits

Current Go processing is intentionally limited:

- Go recognition is extension-based: only paths ending in `.go` are considered.
- Go files ending in `_test.go` are tagged as test files in the local index so
  their churn, size, and complexity can be interpreted separately from
  production source files.
- Go files must be readable as UTF-8 text.
- Files larger than the active content window are not parsed. The default
  content window is 1 MiB.
- Truncated text files do not receive line counts from the scanner.
- Binary files and invalid UTF-8 files are not parsed as Go.
- The parser is tree-sitter based and may emit a parse-error diagnostic while
  still collecting partial facts from the recovered syntax tree.
- Extracted Go symbols are currently compact facts, not a complete semantic
  model. The parser records top-level functions, methods, type specs, structs,
  interfaces, and imports.
- Symbol output does not currently include source ranges, signatures, receiver
  details, package docs, comments, call sites, or full type information.
- Import extraction records string-literal import targets.
- `source_coupling_pressure` is derived from resolved local Go import edges.
  It is a directional coordination-risk signal, not a complete dependency
  graph, build graph, runtime graph, or call graph.
- Go source dependency resolution is conservative and package-path based. It
  uses the local `go.mod` module prefix when present and active Go file
  directories as known packages. External imports and imports that cannot be
  matched to a known local package are left unresolved.
- `complexity_pressure` is derived from parsed Go control-flow
  syntax. It is a hotspot-ranking signal, not a spec-correct cyclomatic
  complexity implementation or a complete model of Go execution semantics.
- Go file risk scoring is currently limited to active rows whose language is
  `go`.
- Go package risk is an approximate aggregation over scored Go files in the same
  repository-relative directory. It uses package paths derived from file
  locations, not full Go build metadata.
- Generated and vendor Go files are excluded from Go file risk scoring by
  default so generated churn or vendored code does not dominate hotspot ranks.
  Their generated/vendor flags remain stored in file facts for inspection.
- Project risk is Go-aware only. Repositories with little or no Go receive low
  or unavailable scoring coverage rather than broad repository risk analysis.

## Go Metric Formulas

The Go metric formulas below describe the current `hotpath.score.go.v1` inputs.
They are approximate, parser-backed signals for hotspot ranking rather than Go
language specifications or stable public APIs. All normalized values are clamped
to the `0.0..=1.0` range.

### Approximate cognitive complexity

Hotpath computes Go cognitive complexity from the tree-sitter syntax tree after
extracting each parsed top-level function or method body. File cognitive
complexity is the sum of per-function complexity. Maximum function complexity is
the largest per-function value in that file.

For the current Go parser, these syntax constructs add complexity:

| Go syntax matched by the parser | Metric kind | Points added | Nesting effect |
| --- | --- | --- | --- |
| `if` statements | branch | `1 + current_nesting` | Child control flow is nested one level deeper. |
| `for` statements, including range loops | loop | `1 + current_nesting` | Child control flow is nested one level deeper. |
| expression switches, type switches, and `select` statements | switch | `1 + current_nesting` | Child control flow is nested one level deeper. |
| expression cases, type cases, communication cases, and default cases | case | `1 + current_nesting` | Child control flow is nested one level deeper. |
| binary expressions whose source text contains `&&` or `||` | boolean chain | `1` | Child control flow keeps the same nesting level. |
| `break`, `continue`, and `goto` statements | jump | `1` | No child nesting is added. |

The generic complexity reducer can score an `else if` node as `1 +
current_nesting` if a parser emits that metric kind, but the current Go parser
does not emit a distinct `else if` kind. It walks the recovered syntax tree and
scores the nested `if` statements it finds. `return`, `defer`, `go`,
`fallthrough`, calls, boolean expressions without `&&` or `||`, and package
initialization order do not currently add complexity by themselves.

The normalized cognitive-complexity score used by the file risk formula is:

```text
file_complexity_score = min(file_cognitive_complexity / 150, 1.0)
function_complexity_score = min(max_function_complexity / 30, 1.0)
normalized_cognitive_complexity = max(file_complexity_score, function_complexity_score)
```

If both parser-backed complexity values are unavailable, the complexity term is
omitted from that file's risk contribution and a `missing_cognitive_complexity`
limitation is recorded. If one value is unavailable, it is treated as zero while
the other value is still normalized.

### Approximate source coupling

Source coupling is derived from local Go import references that resolve to known
local Go package paths. The parser records string-literal import targets. During
index finalization, Hotpath treats every active Go file directory as a known
package path; files in the repository root use `.` as their package path. Package
paths come from file locations, not from Go package declarations.

Local import resolution is conservative and deterministic:

1. Read the first non-empty `module ...` directive from the repository-root
   `go.mod`, when present.
2. For each Go import target, trim surrounding whitespace.
3. If the import exactly equals the module prefix, try resolving it to `.`.
4. If the import starts with `module_prefix/`, strip that prefix and try the
   remaining path.
5. Also try the import target as written after trimming leading/trailing slashes
   and normalizing backslashes to `/`.
6. Resolve to the first candidate that matches an active Go file directory.
   Imports that do not match a known local package remain unresolved.

External packages, generated build graphs, test-only variants, build tags,
workspace files, vendored module semantics, package declarations, type
information, call sites, and runtime behavior are not used by the current source
coupling calculation.

For each resolved local import edge, Hotpath stores the source file path, the
source file's package path, and the target package path. Duplicate edges from
the same source file to the same target package and reference kind are collapsed.
The materialized coupling values are:

```text
source_coupling_in = count(distinct source files importing this file's package path)
source_coupling_out = count(distinct resolved target package paths imported by this file)
```

Inbound coupling is package-level and then attached to each file in the target
package. Outbound coupling is file-level. The normalized source-coupling score
used by the file risk formula is:

```text
inbound_score = min(source_coupling_in / 25, 1.0)
outbound_score = min(source_coupling_out / 15, 1.0)
normalized_source_coupling = max(inbound_score, outbound_score)
```

The raw source-coupling term shown in score explanations is the larger of
`source_coupling_in` and `source_coupling_out`, while the normalized term uses
the separate inbound and outbound thresholds above.

## Git Processing Limits

Current Git processing is also limited:

- Git processing depends on the local `git` executable.
- Non-Git directories are scanned for files, but Git history is marked
  unavailable.
- Shallow repositories are skipped for Git-derived analysis.
- The default Git history scan is bounded to at most 50,000 commits.
- The default Git history scan is bounded to commits from the last 730 days
  relative to the `HEAD` committer timestamp.
- Scan output and the local index expose the active Git collection bounds:
  `max_commits`, `max_age_days`, `first_parent`, rename detection,
  co-change file limit, and the recent-churn reference window.
- Git confidence metadata is advisory and may be `bounded`, `full`,
  `incremental`, `first_parent_only`, `shallow_skipped`, `not_git`, or
  `error_skipped`.
- Incremental Git scans use the previous indexed `HEAD` when it is an ancestor
  of the current `HEAD`; otherwise Hotpath falls back to a full bounded scan.
- The index records whether Git data was reused, incrementally updated,
  fully rebuilt, cleared because options changed, or cleared because Git
  analysis was unavailable.
- Git log and show commands use `--first-parent`, so side-branch history can be
  missed by the current scan model.
- Hotpath compares bounded first-parent and all-reachable commit counts. If
  side-branch history is much larger, or if many first-parent commits are
  merges, it records a merge-heavy warning because Git metrics may undercount
  side-branch work.
- Git log and show commands use basic Git rename detection. Rename following is
  path-based and heuristic: detected rename rows are attributed to the
  post-rename path, but unusual rename/copy histories may still be approximate.
- Root commits are included.
- Merge handling follows the current first-parent command behavior rather than
  a complete all-parent history analysis.
- Recent churn uses a fixed 90-day window relative to the `HEAD` committer
  timestamp, not the machine wall clock.
- Commit timestamp skew, rebases, imports, rewritten history, and unusual
  committer dates can distort recency and age metrics.
- Author identity is the exact Git author string in the form `Name <email>`.
  `.mailmap`, bot detection, account merging, case folding, and domain
  normalization are not applied. The index explicitly records that `.mailmap`
  is ignored.
- Binary changes and numstat rows without numeric line counts contribute zero
  added and deleted lines.
- Ownership weighting uses changed lines, a 730-day recency half-life,
  bulk-change dampening, sustained-activity weighting, and an `others` grouping
  for low-share authors. It is an operational heuristic, not a code ownership
  policy.
- Commits touching more than 100 files are skipped for co-change pair
  generation, while their churn and authorship rows can still be recorded.
- Hotpath records distortion metadata for broad commits, the broadest commit,
  likely automated authors, and high author concentration. These signals are
  visible limitations; Hotpath does not normalize bot or bulk-change authors in
  the current scoring model.
- Non-Git directories, shallow repositories, empty repositories with no `HEAD`,
  missing `git`, permission failures, and unsupported Git command failures are
  reported as actionable Git diagnostics while file scanning continues.
- Co-change is file-pair breadth from commits, not semantic coupling. Hotpath
  reports co-change pressure and source coupling pressure as advisory signals.
- Git metrics are stored as derived cache data and are not a stable public
  schema.

## Principles

- fully offline by default
- no telemetry by default
- no cloud APIs required
- deterministic results where practical
- advisory-only metrics, not automated truth
- public limitations
- cross-platform behavior

## Who It Is For

Hotpath is intended for:

- staff and principal engineers
- tech leads
- platform and DevOps engineers
- monorepo maintainers
- consultants doing codebase audits
- teams using AI coding tools and watching for code bloat or context growth

## Product Direction

Hotpath is being built toward answering questions such as:

- which files combine high churn, large size, and concentrated operational ownership
- which modules are growing fastest
- where complexity pressure or source coupling pressure is concentrating
- which changes touch known hotspots
- how much of a repo is expensive to load into AI coding context
- whether architecture rules are drifting
- why a hotspot score was assigned

Most of that product direction is not implemented yet.

## What It Is Not

Hotpath is not intended to be:

- a cloud SaaS product
- a security scanner
- an AI chat assistant
- an IDE plugin
- a replacement for human engineering judgment
- a source of hidden or opaque quality scores

Scores and reports should be explainable, reproducible, and treated as decision
support when those surfaces exist.

## Development

Hotpath is expected to use Rust for the core implementation.

Common Rust checks:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Privacy

Hotpath is designed as a local tool. The current workflow should not require
network access, telemetry, cloud APIs, hosted services, or uploading repository
contents.

The local index may contain sensitive derived repository information such as
repository-relative paths, file facts, Git metrics, ownership heuristics,
dependency rows, and risk scores. Treat `.hotpath/` as local cache data.

## License

Licensed under the Apache License, Version 2.0.
See [LICENSE](./LICENSE.txt).
