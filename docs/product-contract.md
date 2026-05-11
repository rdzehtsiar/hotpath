# Product Contract

Hotpath is intended to be an offline, local-first, terminal-native codebase
intelligence tool. Its purpose is to help engineers find parts of a repository
that may be risky, expensive, unstable, bloated, or architecturally drifting.

Hotpath is at the beginning of development. This document describes the product
contract the project is being built toward. It does not describe a released CLI,
stable output format, or compatibility guarantee.

## Core Promise

Hotpath should help engineers inspect a local repository and understand where
engineering attention may be needed. The primary workflow should be simple:
run Hotpath in a repository and receive explainable, local codebase intelligence
without uploading code or depending on hosted services.

Hotpath should prioritize:

- offline operation by default
- no telemetry by default
- no network calls for primary workflows
- no cloud APIs for scanning, scoring, indexing, or reports
- deterministic results where practical
- explainable, formula-based metrics and scores
- advisory findings that support human judgment
- clear documentation of limitations and approximations

## Intended Users

Hotpath is intended for engineers who need to reason about repository health and
change risk, including:

- staff and principal engineers
- tech leads
- platform and DevOps engineers
- monorepo maintainers
- consultants doing codebase audits
- teams watching code growth and AI context cost

## Intended Workflows

Hotpath should eventually help answer questions such as:

- Which files combine high churn, large size, or fragmented ownership?
- Which modules appear to be growing fastest?
- Where are complexity and coupling concentrating?
- Which changes touch known hotspots?
- How expensive is a repository or directory to load into AI coding context?
- Why did a file, module, or change receive a risk score?

The intended interface is terminal-native. Future CI output should be
machine-readable and deterministic where practical, but hosted infrastructure
should not be required for the primary workflow.

## Non-Goals

Hotpath is not intended to be:

- a cloud SaaS product
- an AI chat assistant
- an IDE plugin
- a security scanner
- a broad multi-language analysis suite before the core is credible
- a hosted dashboard
- a replacement for human engineering judgment

Hotpath should not present scores as objective truth. Scores and findings should
remain explainable decision support.

## Contract Boundaries

Hotpath may inspect repository files, Git metadata, local configuration, and
local indexes as needed for codebase intelligence. The core workflow should keep
that data on the user's machine.

When Hotpath produces metrics or findings, the output should make clear what was
measured, where it was measured, which formula or rule was used, which raw
metrics contributed, why the result may matter, and what limitations apply.

When behavior is approximate, unsupported, experimental, or language-dependent,
the project should document that directly instead of implying complete coverage.
