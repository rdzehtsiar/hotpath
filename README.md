# Hotpath

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

Hotpath is at the very beginning of development.

The repository currently contains private planning material and early project scaffolding only. There is no stable CLI, no released binary, no supported report format, and no compatibility promise yet.

Expect the crate layout, commands, data model, scoring formulas, output formats, and documentation to change as the product contract and first implementation milestones are built.

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

## License

Licensed under the Apache License, Version 2.0.
See [LICENSE](./LICENSE.txt).
