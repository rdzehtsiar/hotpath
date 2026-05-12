# Limitations

Hotpath is at the beginning of development. The repository currently contains
an early Rust CLI with scanner, parser, complexity, dependency graph, index
health, Git metric, hotspot ranking, and hotspot explanation commands. There is
no released binary, stable CLI contract, supported report format, or
compatibility promise yet.

This page documents the limits Hotpath should make explicit as it develops.

## Current Limitations

Hotpath currently provides early repository scanning, scan persistence to a
local index, `hotpath doctor` index health checks, parser-backed symbol
extraction with `hotpath parse`, parser-derived complexity summaries with
`hotpath complexity`, conservative dependency graph output with `hotpath graph
--module <selector>`, local Git history explanation with `hotpath explain-git`,
hotspot ranking with `hotpath hotspots`, and per-file hotspot explanation with
`hotpath explain`. These are not stable interfaces.

Hotpath does not currently provide stable Git analysis or scoring compatibility,
broad parser/language support, complete dependency analysis, CI output,
architecture rules, or a terminal UI.

Future commands, crate layout, data models, scoring formulas, and output formats
may change while the product contract and first implementation milestones are
built.

The current `.hotpath/index.db` behavior is documented separately in
[Local index](index.md), but the index is not yet a stable public database
format. During early development, incompatible or corrupt local indexes may need
to be deleted and rebuilt instead of migrated.

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

## Git Metric Limits

The current `hotpath explain-git` command calculates Git metrics from local
history reachable from `HEAD` and writes derived rows to index v2 after
successful analysis. These metrics are advisory and are not a stable report
contract.

Known Git metric limitations include:

- renamed files are tracked conservatively by path, so pre-rename history stays
  under the old path instead of being reconstructed under the new path
- merge commits are diffed against their first parent, while reachable
  side-branch commits are still analyzed separately
- generated and vendor paths are not excluded from Git metrics merely because a
  scan can classify them as generated or vendor files
- shallow repositories are rejected because missing history would make churn,
  ownership, age, and co-change metrics misleading
- author identity uses the exact commit author string and does not apply
  `.mailmap`, case folding, bot detection, domain normalization, or account
  merging
- binary changes and changes without available line statistics contribute `0`
  line churn even when the file was touched
- recency and file age depend on local Git committer timestamps, so timestamp
  skew from rebases, imports, rewritten history, or unusual commit metadata can
  distort time-based metrics

See [Git metric semantics](git-metrics.md) for the current formulas and
calculation notes.

## Parser Limits

The current parser command surface is `hotpath parse` for a summary and
`hotpath parse --json` for a machine-readable report with schema identifier
`hotpath.parse.v1`. Parser support is currently limited to Rust, Go,
TypeScript, and TSX. Python and other languages are skipped as unsupported.

Parser output can include modules, packages, namespaces, imports, functions,
methods, classes and types, symbol ranges, parent/nesting metadata, and basic
function/method complexity approximations. `hotpath complexity` summarizes
parser-derived symbol length, function length for functions and methods, large
symbols at the current `>= 80` line threshold, maximum cyclomatic complexity,
maximum control-flow nesting, dependency edge counts, maximum fan-in/out, and
per-file fan-in/fan-out. These values are derived from parsed syntax and
resolved local edges, so they should be treated as rough local signals, not full
language-semantic complexity measurements.

Files with syntax errors can still yield partial extraction when the parser
recovers enough of the tree. In those cases Hotpath reports a warning and the
extracted symbols, imports, or complexity values may be incomplete.

Dependency resolution is conservative and intentionally incomplete. Hotpath
currently persists resolved local dependency edges during parse, complexity, and
graph flows only when parser-observed relationships can be safely mapped to
indexed files. Rust `mod` declarations and Rust `crate::`/`self::` use paths
are resolved only where safe. TypeScript and TSX relative imports are resolved
only where safe. Go dependency edges are disabled/unresolved for now. External,
grouped, glob, ambiguous, and unresolved imports are not stored as dependency
edges.

Raw imports reported by `hotpath parse --json` should not be read as complete
dependency graph output. `hotpath graph --module <selector>` exposes current
resolved local edges with schema identifier `hotpath.graph.v1`, but this is not
a complete package graph, build graph, runtime graph, or architecture model.
Hotspot scoring does not yet consume parser symbols, parser-derived complexity,
or resolved dependency fan metrics.

## Hotspot Score Limits

The current `hotpath hotspots` and `hotpath explain` commands combine scanner
facts with local Git metrics to produce advisory hotspot scores using the
documented `hotpath.score.v1` formula. Successful hotspot analysis writes
derived score rows to index v2.

Known hotspot score limitations include:

- scores use current scanned files plus local Git history reachable from
  `HEAD`; they do not query hosted Git providers or cloud APIs
- parser output, including parser-backed symbols and basic complexity
  approximations, is not a formula input
- symbol coupling, resolved dependency edges, parser fan-in/fan-out, test
  coverage, runtime incidents, ownership policy, and architecture rules are not
  formula inputs
- generated and vendor classifications are visible scanner facts but are not
  weighted terms in `hotpath.score.v1`
- missing source facts are not guessed; missing normalized inputs contribute
  `0.0` for their fixed-weight terms and are listed as limitations
- a higher score means "worth investigating sooner," not "bad code"

See [Scoring](scoring.md) for the current formula, normalization rules, ranking
behavior, and limitation codes.

## False Positives And False Negatives

Hotpath may flag files or modules that are not actually problematic. It may also
miss problems that require production knowledge, domain context, code review,
runtime behavior, or team history.

Scores should help prioritize investigation. They should not replace review,
testing, incident analysis, architecture discussion, or maintainer judgment.

## Language And Repository Coverage

Hotpath should avoid implying broad language coverage before the core analysis
is credible. Current parser support is limited to Rust, Go, TypeScript, and
TSX; Python is not supported. Language-aware parsing, symbol extraction,
complexity metrics, dependency edge resolution, and coupling analysis vary by
language and project structure. Current dependency edges are especially limited:
Rust and TypeScript/TSX have conservative local resolution paths, while Go
dependency edges are disabled/unresolved for now.

Repositories with unusual encodings, large generated trees, vendored source,
submodules, symlinks, rewritten history, or nonstandard build layouts may
produce incomplete or approximate results.

Git history metrics require a readable local Git worktree with a commit at
`HEAD` and complete local history. Non-Git directories, repositories without an
initial commit, bare repositories, and shallow clones are reported as
unsupported instead of producing partial metrics.

## Privacy And Environment Limits

The core workflow should be offline and local-first, with no telemetry by
default and no cloud APIs for primary analysis. Generated reports and local
indexes may still contain sensitive derived information from the repository, so
users should handle them according to their own security and retention rules.

Current index data is stored under `.hotpath/` without a daemon or network
service. Hotpath should continue to document local reads and writes clearly as
implementation details become stable.
