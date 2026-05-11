# Limitations

Hotpath is at the beginning of development. The repository currently contains
early project scaffolding and documentation. There is no stable CLI, no released
binary, no supported report format, and no compatibility promise yet.

This page documents the limits Hotpath should make explicit as it develops.

## Current Limitations

Hotpath does not currently provide a stable implementation of repository
scanning, indexing, Git analysis, scoring, reports, CI output, architecture
rules, or a terminal UI.

Future commands, crate layout, data models, scoring formulas, and output formats
may change while the product contract and first implementation milestones are
built.

## Product Boundaries

Hotpath is not intended to be:

- a cloud SaaS product
- an AI chat assistant
- an IDE plugin
- a security scanner
- a broad multi-language analysis suite before the core is credible
- a hosted dashboard
- a replacement for human engineering judgment

Hotpath findings should be treated as advisory. They can point to areas worth
investigating, but they do not prove that code is defective, risky, insecure, or
unimportant.

## Metric Limits

Repository metrics are approximations of engineering risk and cost. They can be
useful signals, but they are not complete explanations.

Expected limitations include:

- churn can reflect healthy active development, not only instability
- file size can reflect generated code, data, or deliberate consolidation
- ownership metrics can be distorted by bots, bulk rewrites, pair programming,
  imports, or history rewrites
- complexity metrics can miss domain context and intentional tradeoffs
- coupling metrics can be incomplete when language support is partial
- AI context estimates are approximations, not tokenizer-specific guarantees

Each metric should document what it measures, why it may matter, and where it
can mislead.

## False Positives And False Negatives

Hotpath may flag files or modules that are not actually problematic. It may also
miss problems that require production knowledge, domain context, code review,
runtime behavior, or team history.

Scores should help prioritize investigation. They should not replace review,
testing, incident analysis, architecture discussion, or maintainer judgment.

## Language And Repository Coverage

Hotpath should avoid implying broad language coverage before the core analysis
is credible. Language-aware parsing, symbol extraction, complexity metrics, and
coupling analysis may vary by language and project structure.

Repositories with unusual encodings, large generated trees, vendored source,
submodules, symlinks, rewritten history, or nonstandard build layouts may
produce incomplete or approximate results.

## Privacy And Environment Limits

The core workflow should be offline and local-first, with no telemetry by
default and no cloud APIs for primary analysis. Generated reports and local
indexes may still contain sensitive derived information from the repository, so
users should handle them according to their own security and retention rules.

Hotpath should document local reads and writes clearly as implementation
details become stable.
