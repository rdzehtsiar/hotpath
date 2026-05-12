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
`hotpath explain-git`, `hotpath hotspots`, and `hotpath explain`. The scanner
reports local file facts and warnings, scan and analysis commands persist
derived local SQLite index data at `.hotpath/index.db`, Git analysis explains
local history for requested paths, hotspot commands rank and explain current
files with the documented `hotpath.score.v1` formula, parse commands print an
early parser report for supported source files, complexity commands summarize
parser-derived symbol complexity and fan metrics, and graph commands expose
conservative resolved local dependency edges for a selected module scope.

Parser support is currently limited to Rust, Go, TypeScript, and TSX. There is
no Python parser support yet. `hotpath parse` prints a summary, while
`hotpath parse --json` prints a machine-readable report with schema identifier
`hotpath.parse.v1`. Parser output includes modules, packages, namespaces,
imports, functions, methods, classes and types, symbol ranges, parent/nesting
metadata, and basic parser-derived function/method complexity approximations.
`hotpath complexity --json` currently uses schema identifier
`hotpath.complexity.v1`, and `hotpath graph --module <selector> --json`
currently uses schema identifier `hotpath.graph.v1`.

There is no released binary, stable CLI contract, stable index format,
supported report format, stable Git analysis compatibility promise, broad
parser/language support, complete dependency analysis, CI output, architecture
rules, or terminal UI yet.

Expect the crate layout, commands, data model, scoring formulas, output formats, and documentation to change as the product contract and first implementation milestones are built.

## Product Contract

The public contract for Hotpath is documented in:

- [Product contract](docs/product-contract.md)
- [Privacy](docs/privacy.md)
- [Metrics](docs/metrics.md)
- [Scoring principles](docs/scoring.md)
- [Git metric semantics](docs/git-metrics.md)
- [Local index](docs/index.md)
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
- CI-friendly output once implemented

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

- which files combine high churn, large size, and fragmented ownership
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
score rows using repository-relative paths. It does not require a daemon or
network access, and it can be deleted and rebuilt from local repository data.
See [Local index](docs/index.md).

## License

Licensed under the Apache License, Version 2.0.
See [LICENSE](./LICENSE.txt).
