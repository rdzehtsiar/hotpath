# Scoring Principles

Hotpath scores are intended to be decision support, not ground truth. A score
should help an engineer decide where to investigate next; it should not make an
automated claim that a file, module, or change is objectively good or bad.

Hotpath is at the beginning of development. This document defines the scoring
standard the project is being built toward. It does not define a released
formula or stable schema.

## Required Properties

Hotpath scoring should be:

- explainable
- formula-based
- versioned
- deterministic where practical
- reproducible from local repository data
- advisory-only
- documented with known limitations and approximations

Scoring should not depend on telemetry, hosted services, or cloud APIs.

## Explainability

Every score or finding should make clear:

- what was measured
- where it was measured
- which formula, rule, or scoring version was used
- which raw metrics contributed
- how metrics were normalized
- how weighted contributions affected the result
- why the result may matter
- what limitation or approximation applies

Hotpath should avoid opaque scores, hidden weights, and unverifiable claims.

## Raw And Normalized Metrics

When a score combines multiple signals, Hotpath should preserve enough detail
for users to inspect both the raw inputs and the normalized values used by the
formula.

For example, a future hotspot score may include signals such as churn, file
size, ownership fragmentation, recent growth, or co-change activity. The exact
formula should be public, versioned, and covered by tests before it is treated
as part of a stable contract.

## Advisory Output

Scores should indicate where attention may be useful. They should not be used as
standalone evidence that code is defective, insecure, unmaintainable, or owned
by the wrong team.

Users should interpret scores alongside repository context, team knowledge,
production history, test coverage, and architectural intent.

## Determinism

Where output can be inspected, compared, cached, or used in CI, Hotpath should
prefer deterministic behavior. Reports should use stable ordering where
practical and should avoid timestamps, host-specific absolute paths, and local
machine state in portable output unless those values are explicitly requested.

Some metrics may remain approximate because source code, Git history, generated
files, vendored code, and language parsing all have practical limits. Those
approximation boundaries should be documented with the metrics they affect.

## Changing Scores

When scoring behavior changes, the project should update tests and
documentation. Formula versions should make it possible to understand whether a
score changed because the repository changed or because Hotpath changed.
