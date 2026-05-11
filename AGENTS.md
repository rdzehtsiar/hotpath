# AGENTS

This file gives coding agents the project context and operating rules needed to make useful changes to Hotpath.

## Project Context

Hotpath is an offline, local-first codebase intelligence tool. Its purpose is to help engineers identify risky, expensive, unstable, bloated, or architecturally drifting areas in a repository.

The repository is at the very beginning of development. Treat the current implementation as early and evolving. Verify the actual files, tests, and documentation before assuming any architecture, crate layout, command surface, or data model exists.

The `.plan/` directory is private planning material. Use it only as local context when explicitly asked, do not commit it, and do not copy private goals, business plans, or sensitive roadmap details into public documentation.

## Product Boundaries

Keep the project focused on local codebase intelligence.

In scope:

- repository scanning
- Git history, churn, and ownership analysis
- file size, language, and generated/vendor classification
- symbol, complexity, and coupling analysis
- explainable hotspot scoring
- AI context budgeting
- local indexes and reports
- terminal-native workflows
- CI-friendly output when implemented
- architecture rule checks when implemented

Out of initial scope:

- cloud SaaS
- AI chat assistant behavior
- IDE plugins
- security scanning
- marketplace features
- broad language support before the core is credible
- dashboards that require hosted infrastructure

## Engineering Principles

Preserve these properties unless the user explicitly changes direction:

- offline by default
- no telemetry by default
- no cloud APIs for the primary workflow
- deterministic scans and reports where practical
- transparent, versioned scoring formulas
- explainable metrics and hotspot scores
- advisory-only output
- reproducible benchmarks
- public limitations
- cross-platform behavior
- CI-friendly workflows

## Rust Guidance

Hotpath is expected to use Rust for the core implementation. Follow Rust best practices for ownership, error handling, typed data, dependency use, and concurrency.

When working in Rust:

- Keep CLI entry points thin and move testable behavior into library crates or modules.
- Keep scanning, Git analysis, parsing, metrics, indexing, reporting, TUI, and rules concerns separated.
- Prefer small explicit functions with clear names over large procedural blocks.
- Prefer typed errors and actionable messages over panics for malformed input, unsupported repositories, corrupted indexes, invalid config, or filesystem edge cases.
- Keep public APIs conservative and documented enough for tests or future crates to use safely.
- Avoid hidden side effects and global mutable state.
- Use deterministic data structures and stable ordering where output can be inspected, compared, or snapshot-tested.
- Do not introduce large dependencies, frameworks, generated scaffolding, or broad language support without a clear project need.

## Testing Requirements

Use a test-first pattern whenever practical.

Testing expectations:

- Write or update tests before implementing behavior changes when the desired behavior can be specified up front.
- Cover behavior changes with meaningful tests unless there is a documented reason testing is impractical.
- Prefer focused unit tests for path classification, language detection, scoring formulas, metric normalization, config parsing, and report formatting.
- Prefer fixture-based integration tests for scanner behavior, Git metrics, parser output, index updates, diff analysis, and rule checks.
- Include negative-path tests for binary files, symlinks, ignored files, malformed config, missing Git data, corrupted local indexes, and unsupported encodings.
- Keep tests deterministic, offline, and independent of network access, host-specific absolute paths, wall-clock timestamps, and local machine state.
- When changing existing behavior, add regression tests that would fail without the fix.
- If a change cannot reasonably be tested in the current task, state the gap clearly in the final response.

Common verification commands once Rust code exists:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Determinism Requirements

Hotpath should produce deterministic behavior wherever users can inspect, compare, cache, or gate on output.

- Sort file traversal results and report rows by stable keys.
- Sort diagnostic findings, hotspot lists, rule violations, and JSON output where practical.
- Avoid timestamps in default reports and golden outputs unless time is the behavior under test.
- Avoid absolute paths in portable output unless explicitly requested.
- Keep JSON schemas versioned and output ordering stable where practical.
- Use fixture repositories for exact expected metrics.
- Document approximation boundaries for metrics and scoring formulas.

## Metrics And Scoring

Metrics are decision support, not ground truth.

Every score or finding should be explainable in terms of:

```text
what was measured
where it was measured
which formula or rule was used
which raw metrics contributed
why the result matters
what limitation or approximation applies
```

Do not add opaque scoring, hidden weights, or unverifiable claims. When scoring behavior changes, update tests and documentation.

## SPDX License Header Rule

All coding agents must include SPDX license headers in source code files they create or edit once the project license is defined.

- When creating a source code file, add an SPDX license header before any code.
- When editing an existing source code file, make sure the file already has an SPDX license header; if it does not, add one as part of the edit.
- Use the file's native comment syntax.
- Do not add duplicate SPDX headers.
- Do not add SPDX headers to generated files, vendored third-party files, lockfiles, binary files, or data fixtures unless the project later documents a specific convention.

If the project license has not been defined yet, ask before adding new source files or use the license identifier documented by the user.

## Documentation Guidance

Human-facing documentation should describe what Hotpath does, who it is for, the current project state, and the limits of its metrics. Keep it conservative, public-facing, and appropriate for an infrastructure analysis tool.

- Do not expose private planning details from `.plan/`.
- Do not imply released functionality that does not exist.
- State early-development status clearly.
- Document limitations as first-class project information.
- Keep benchmark and performance claims tied to reproducible evidence.
- Keep scoring claims tied to public formulas and tests.

Useful documentation topics as the project matures include:

- product contract
- privacy model
- scoring formulas
- metric definitions
- limitations
- JSON schema
- CI usage
- benchmark methodology
- architecture rules
- release and binary verification process

## Git Rules

- Work strictly inside this repository unless the user explicitly says otherwise.
- Inspect the existing repository before making structural changes.
- Keep changes scoped to the current request.
- Do not commit `.plan/` or copy private planning details into public files.
- Do not revert user changes unless the user explicitly asks.
- If git reports dubious ownership, do not change global git config unless the user asks or the task requires git operations.
- Use imperative mood for commit messages, such as `Add project documentation` or `Initialize Rust workspace`.
- Before committing, run relevant formatting, linting, and tests when practical.
- Mention any tests that were not run in the final response.

## Working Rules

- Prefer existing project patterns over new abstractions.
- Keep changes small and reviewable.
- Preserve the offline-first posture.
- Prefer deterministic tests over environment-dependent behavior.
- Keep errors explainable and actionable.
- Document unsupported behavior instead of implying it works.
