# Hotpath

[![Tests](https://github.com/rdzehtsiar/hotpath/actions/workflows/tests.yml/badge.svg)](https://github.com/rdzehtsiar/hotpath/actions/workflows/tests.yml)
[![codecov](https://codecov.io/gh/rdzehtsiar/hotpath/graph/badge.svg)](https://codecov.io/gh/rdzehtsiar/hotpath)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=rdzehtsiar_hotpath&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=rdzehtsiar_hotpath)

Hotpath is an offline, local-first codebase intelligence tool for engineers who need to find risky, expensive, unstable, bloated, or architecturally drifting parts of a repository.

The intended experience is simple: install one binary, run one command in a repo, and get useful codebase intelligence in minutes without sending code anywhere.

## Vision

Hotpath is built around a practical question:

```text
Where is this repo likely to hurt us next?
```

The product direction is a terminal-native engine that combines local repository signals such as:

- file structure
- Git history
- churn and ownership
- size and growth
- symbols and language-aware structure
- complexity and coupling
- AI context cost
- architecture rule violations

into explainable hotspot reports that help engineers decide where to investigate, refactor, test, or constrain change.

## Current State

Hotpath is at the beginning of development.

The repository currently contains an early Rust CLI with `hotpath scan`,
`hotpath parse`, `hotpath complexity`, `hotpath graph`, `hotpath doctor`,
`hotpath explain-git`, `hotpath hotspots`, `hotpath explain`, and
`hotpath context`, `hotpath report`, and `hotpath ci`, plus early `hotpath diff`
and `hotpath pr` commands for committed-tree diff risk reports. It also
contains an early `hotpath tui` terminal UI for local, offline exploration of
the same repository facts. The TUI is terminal-native and keyboard-first, with
no mouse required, but it is unstable and not a stable UI contract. The scanner
reports local file facts and warnings, scan and analysis commands persist
derived local SQLite index data at
`.hotpath/index.db`, Git analysis explains local history for requested paths,
hotspot commands rank and explain current files with the documented
`hotpath.score.v3` formula, parse commands print an early parser report for
supported source files, complexity commands summarize parser-derived symbol
complexity and fan metrics, graph commands expose conservative resolved local
dependency edges for a selected module scope, and context commands estimate AI
context cost offline from scanner facts. Repository reports aggregate scan
summary, local Git analysis, hotspot ranking, and context estimates into
Markdown, JSON, SARIF, or static HTML output. The CI command can fail a local or
hosted CI job when the current repository hotspot risk reaches a supplied
threshold. Diff and PR reports compare committed Git trees locally, use the
merge base of the requested base and head refs, and do not require GitHub API or
network access.

Parser support is currently limited to Rust, Go, TypeScript, and TSX. There is
no Python parser support yet. `hotpath parse` prints a summary, while
`hotpath parse --json` prints a machine-readable report with schema identifier
`hotpath.parse.v1`. Parser output includes modules, packages, namespaces,
imports, functions, methods, classes and types, symbol ranges, parent/nesting
metadata, and basic parser-derived function/method complexity approximations.
`hotpath complexity --json` currently uses schema identifier
`hotpath.complexity.v1`, and `hotpath graph --module <selector> --json`
currently uses schema identifier `hotpath.graph.v1`. `hotpath context --json`
currently uses schema identifier `hotpath.context.v1`. `hotpath diff
<base>...<head> --json` and `hotpath pr --base <base> --head <head> --json`
currently use schema identifier `hotpath.diff.v1`. `hotpath report --json`
currently uses schema identifier `hotpath.report.v1`; `hotpath report --sarif`
emits SARIF 2.1.0 for hotspot findings.

There is no released binary, stable CLI contract, stable TUI contract, stable
index format, stable report compatibility promise, stable Git analysis
compatibility promise, broad parser/language support, complete dependency
analysis, or architecture rules yet.

Expect the crate layout, commands, data model, scoring formulas, output formats, and documentation to change as the product contract and first implementation milestones are built.

## Product Contract

The public contract for Hotpath is documented in:

- [Product contract](docs/product-contract.md)
- [Privacy](docs/privacy.md)
- [Metrics](docs/metrics.md)
- [Scoring principles](docs/scoring.md)
- [Git metric semantics](docs/git-metrics.md)
- [Local index](docs/index.md)
- [Context estimates](docs/context.md)
- [Report schema](docs/json-schema.md)
- [CI usage](docs/ci.md)
- [Diff and PR risk](docs/diff-risk.md)
- [Limitations](docs/limitations.md)

## Product Principles

- fully offline by default
- no telemetry by default
- no cloud APIs required
- deterministic results where practical
- transparent and versioned scoring formulas
- advisory-only metrics, not automated truth
- reproducible benchmarks
- public limitations
- CI-friendly output

## Who It Is For

Hotpath is intended for:

- staff and principal engineers
- tech leads
- platform and DevOps engineers
- monorepo maintainers
- consultants doing codebase audits
- teams using AI coding tools and watching for code bloat or context growth

## What Hotpath Should Help With

Hotpath should make it easier to answer questions such as:

- which files combine high churn, large size, and concentrated operational ownership
- which modules are growing fastest
- where complexity or coupling is concentrating
- which changes touch known hotspots
- how much of a repo is expensive to load into AI coding context
- whether architecture rules are drifting
- why a hotspot score was assigned

## What It Is Not

Hotpath is not intended to be:

- a cloud SaaS product
- a security scanner
- an AI chat assistant
- an IDE plugin
- a replacement for human engineering judgment
- a source of hidden or opaque quality scores

Scores and reports should be explainable, reproducible, and treated as decision support.

## Development

Hotpath is expected to use Rust for the core implementation.

Common Rust checks once the project is initialized:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Privacy

Hotpath is designed as a local tool. The core workflow should not require network access, telemetry, cloud APIs, hosted services, or uploading repository contents.

Current scans and analysis commands write derived local cache data under
`.hotpath/`, including `.hotpath/index.db`. The index stores scanner file
facts, scan run metadata, scan/file warnings, parser-backed symbol rows, Git
metrics, co-change rows, conservative resolved dependency edges, and hotspot
score rows using repository-relative paths. Context estimates reuse current scan
facts and do not add a context-specific index table or schema. Diff and PR
reports persist the same scan, Git analysis, and hotspot rows used by existing
commands; they do not add a diff-specific index table. Repository reports and
CI risk gates persist the same scan, Git analysis, and hotspot rows used by
existing commands; static HTML reports are written only when requested with
`hotpath report --html <dir>`. The index does not require a daemon or network
access, and it can be deleted and rebuilt from local repository data. See
[Local index](docs/index.md).

## License

Licensed under the Apache License, Version 2.0.
See [LICENSE](./LICENSE.txt).
