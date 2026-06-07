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

The CLI currently exposes two subcommands:

```powershell
hotpath scan
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

There is no current `scan --json`, `scan --summary`, report output, CI gate, or
stable machine-readable scan schema.

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
metrics, source dependency rows, and Go risk score rows. It can be deleted and
rebuilt from local repository data.

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
- Approximate source coupling is derived from resolved local Go import edges.
  It is a directional coordination-risk signal, not a complete dependency
  graph, build graph, runtime graph, or call graph.
- Go source dependency resolution is conservative and package-path based. It
  uses the local `go.mod` module prefix when present and active Go file
  directories as known packages. External imports and imports that cannot be
  matched to a known local package are left unresolved.
- Approximate cognitive complexity is derived from parsed Go control-flow
  syntax. It is a hotspot-ranking signal, not a spec-correct cyclomatic
  complexity implementation or a complete model of Go execution semantics.
- Go file risk scoring is currently limited to active rows whose language is
  `go`.
- Generated and vendor Go files can still be scored. Their flags are preserved,
  but they are not excluded by default.
- Project risk is Go-aware only. Repositories with little or no Go receive low
  or unavailable scoring coverage rather than broad repository risk analysis.

## Git Processing Limits

Current Git processing is also limited:

- Git processing depends on the local `git` executable.
- Non-Git directories are scanned for files, but Git history is marked
  unavailable.
- Shallow repositories are skipped for Git-derived analysis.
- The default Git history scan is bounded to at most 50,000 commits.
- The default Git history scan is bounded to commits from the last 730 days
  relative to the `HEAD` committer timestamp.
- Incremental Git scans use the previous indexed `HEAD` when it is an ancestor
  of the current `HEAD`; otherwise Hotpath falls back to a full bounded scan.
- Git log and show commands use `--first-parent`, so side-branch history can be
  missed by the current scan model.
- Git log and show commands use `--no-renames`, so rename history is not
  reconstructed across paths.
- Root commits are included.
- Merge handling follows the current first-parent command behavior rather than
  a complete all-parent history analysis.
- Recent churn uses a fixed 90-day window relative to the `HEAD` committer
  timestamp, not the machine wall clock.
- Commit timestamp skew, rebases, imports, rewritten history, and unusual
  committer dates can distort recency and age metrics.
- Author identity is the exact Git author string in the form `Name <email>`.
  `.mailmap`, bot detection, account merging, case folding, and domain
  normalization are not applied.
- Binary changes and numstat rows without numeric line counts contribute zero
  added and deleted lines.
- Ownership weighting uses changed lines, a bulk-change dampening factor, and a
  recency half-life. It is an operational heuristic, not a code ownership
  policy.
- Commits touching more than 100 files are skipped for co-change pair
  generation, while their churn and authorship rows can still be recorded.
- Co-change is file-pair breadth from commits, not semantic coupling.
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
- where complexity or coupling is concentrating
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
