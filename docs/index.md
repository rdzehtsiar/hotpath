# Local Index

Hotpath is at the beginning of development. The local index exists now, but it
is still an early derived cache, not a stable public database format or
compatibility promise.

The index makes repeated local analysis easier to inspect and extend. It can be
created and populated by `hotpath scan`, `hotpath parse`, `hotpath
complexity`, `hotpath graph`, `hotpath explain-git`, `hotpath hotspots`, and
`hotpath explain` runs. `hotpath diff` and `hotpath pr` persist the same scan,
Git analysis, and hotspot rows used by those workflows; they do not add a
diff-specific table or persist diff report rows in the current schema.
`hotpath context` uses current scan facts for context estimates. When it
persists data, it persists only the current scan facts and does not add
context-specific rows, tables, or schema. The index is not user-authored data
and should be safe to delete and rebuild.

## Location And Scope

The current index location is:

```text
.hotpath/index.db
```

The `.hotpath/` directory is created at the repository root being analyzed when
`hotpath scan` or a successful analysis command persists results. `hotpath
doctor` can inspect this location, but it does not create a missing index.

The `.hotpath/index.db` file is local working data, not a portable report
format. Users should not commit it, share it, or edit it by hand.

## Current Schema

The current SQLite schema version is `2`. The index also stores the schema
identifier `hotpath.index.v2` in metadata. Hotpath rejects indexes with missing,
unknown, malformed, corrupt, or future schema metadata instead of reading them
best-effort.

The schema contains tables for current scanner, parser, dependency, Git, and
hotspot persistence, plus sparse extension points for later analysis. The
presence of a table does not mean every possible related feature is
implemented.

Context estimates currently rely on scanner file facts such as
repository-relative path, byte size, content kind, generated/vendor flags, and
file warnings. There is no context-specific table or context-specific index
schema in the current design, and context estimates do not persist
token-estimate rows.

Diff and PR reports currently rely on scanner facts, local Git analysis, and
hotspot score rows. There is no diff-specific table or diff-specific index
schema in the current design, and diff/PR commands do not persist changed-file
rows, context deltas, or architecture status rows.

Currently populated by `hotpath scan`:

- repository identity for the current working tree, stored with the root key `.`
- scan run metadata such as run key, completed status, scanner version, scan
  JSON schema identifier, observed file count, and observed warning count
- scan warnings, including warning code, optional repository-relative path, and
  message
- scanner file facts such as repository-relative path, byte size, extension,
  language guess, line count, content kind, vendor/generated flags, symlink
  flag, and classification
- per-file warnings, including warning code and message

Currently populated by `hotpath parse`, `hotpath parse --json`, `hotpath
complexity`, and `hotpath graph` after a successful scanner/parser pass:

- current scanner rows, using the same scan persistence behavior as `hotpath
  scan`
- parser-backed symbol rows for supported Rust, Go, TypeScript, and TSX files
- symbol names and kinds for extracted modules, packages, namespaces,
  functions, methods, classes, and types
- repository-relative file identity, line ranges, concise signatures, and
  parent symbol links when a nested parent can be resolved within the same
  parsed file

`hotpath parse --json` prints parse reports with schema identifier
`hotpath.parse.v1`. The command output includes raw import records, symbol
ranges, parent/nesting metadata, and basic function/method complexity
approximations. For parser-derived data, the index currently persists symbol
rows and conservative resolved dependency edges; it does not persist raw imports
or parser-derived complexity metrics as separate metric rows.

Currently populated in `dependencies` by `hotpath parse`, `hotpath
complexity`, and `hotpath graph` when parser-observed relationships can be
resolved conservatively:

- resolved local dependency edges between indexed repository files
- repository-relative source and target file identities
- enough edge metadata for current dependency graph and fan-in/fan-out reports

Current dependency persistence is deliberately conservative. Rust `mod`
declarations and Rust `crate::`/`self::` use paths are stored only when Hotpath
can safely resolve them to local files. TypeScript and TSX relative imports are
stored only when they can be safely resolved to local files. Go dependency
edges, external package imports, grouped imports, glob imports, ambiguous
imports, and unresolved imports are not stored as resolved dependency edges.

`hotpath complexity --json` prints complexity reports with schema identifier
`hotpath.complexity.v1`. `hotpath graph --module <selector> --json` prints
dependency graph reports with schema identifier `hotpath.graph.v1`. Current
complexity and graph flows persist resolved dependency edges and derive
dependency edge counts, per-file fan-in/fan-out, and maximum fan-in/fan-out from
the same fresh parser report used for that command run.

Currently populated by `hotpath explain-git`, `hotpath hotspots`, `hotpath
explain`, `hotpath diff`, and `hotpath pr` after successful local history
analysis:

- Git analysis metadata, including analyzer version, `HEAD` commit id, `HEAD`
  committer timestamp, recent churn window, and observed row counts
- per-file Git metrics such as commit count, total churn, recent churn, author
  count, dominant owner/share, first/last observed commits, and file age
- co-change pairs with deterministic left/right repository-relative paths and
  commit counts

These Git rows are derived cache data, not source-of-truth repository metadata.
They inherit the limitations documented in [Git metric semantics](git-metrics.md),
including conservative rename handling, first-parent merge diffs, rejection of
shallow history, exact author identity matching, binary line churn limits, and
timestamp skew. Generated and vendor scanner flags are stored separately from
Git metrics; index v2 does not imply those paths are excluded from Git-derived
rows.

Currently populated by `hotpath hotspots`, `hotpath explain`, `hotpath diff`,
and `hotpath pr` after successful hotspot scoring:

- hotspot score rows for ranked current files
- formula version identifiers
- raw score metrics serialized as JSON
- normalized metric and weighted-term explanation payloads
- score limitation payloads

These hotspot rows are derived cache data. They inherit the limitations
documented in [Scoring](scoring.md), including fixed-weight missing-input
behavior, conservative Git metric inputs, parser data not being used by
hotspot scoring, and advisory-only interpretation.

Current dependency rows are derived cache data, not source-of-truth build
metadata. Documentation and reports should continue to describe the resolver
scope and should not imply complete dependency, package, build-system, or
runtime coupling analysis.

## Path Storage

File identities and warning paths in the index are stored as
repository-root-relative paths. Indexed paths should:

- use `/` as the separator on every platform
- avoid absolute paths, drive prefixes, UNC prefixes, and home-directory
  expansions
- avoid leading `./`, leading `/`, and parent-directory components that escape
  the repository root
- preserve the path spelling Hotpath reports for the scanned file
- sort deterministically by the stored path when reports or diagnostics expose
  ordered file lists

This keeps indexed data portable across machines where practical and avoids
leaking host-specific absolute paths into reports.

## Scan Persistence And Stale Files

Scan persistence is transaction-like: a completed scan updates the index
consistently, or the write fails without publishing a partial scan.

When a scan is successfully persisted, the current scan's file set replaces the
previous active file set for the repository. File rows from older scans that
were not observed in the current scan are deleted. Dependent rows for those
stale files, such as file warnings and derived metric rows tied by foreign
keys, are removed or detached according to the schema.

Removed files should not continue to appear in index-backed reads merely
because they existed in an earlier scan.

## Doctor Behavior

`hotpath doctor` checks the local index at `.hotpath/index.db` for the current
working directory.

If the index is missing, doctor succeeds and reports:

```text
Hotpath doctor
index path: .hotpath/index.db
schema version: none
readable: no
health: missing
```

A missing index is not an error. Doctor does not create `.hotpath/` or
`index.db`.

If the index is present, readable, and matches the supported schema, doctor
succeeds and reports a healthy index:

```text
Hotpath doctor
index path: .hotpath/index.db
schema version: 2
readable: yes
health: healthy
```

If the index cannot be opened as a valid SQLite database, fails integrity or
schema checks, has malformed metadata, or uses an unsupported future schema,
doctor fails with an actionable error. Hotpath should not silently mix corrupt
indexed data with fresh scan data.

## Delete And Rebuild

Because `.hotpath/index.db` is derived local cache data, the supported recovery
model is to rebuild it from the repository.

To delete the index, remove:

```text
.hotpath/index.db
```

Removing the whole `.hotpath/` directory is also safe if it only contains
Hotpath's derived local data. Deleting the index does not affect repository
source files, Git history, project configuration, or report files stored
elsewhere.

To rebuild the index, run a scan from the repository root:

```powershell
hotpath scan
```

`hotpath scan --summary` and `hotpath scan --json` also persist the current scan
before printing output.

## Privacy Posture

The index is local by default. Creating, reading, validating, deleting, or
rebuilding `.hotpath/index.db` does not require network access, telemetry, cloud
APIs, hosted services, or uploading repository contents. Hotpath does not run a
daemon for the index.

The index may contain sensitive repository-derived information, including file
paths, byte sizes, language guesses, line counts, generated/vendor
classification, symlink classification, scan warnings, file warning messages,
parser symbol names, parser symbol ranges, concise symbol signatures,
conservative resolved dependency edges, Git metric rows, co-change pairs, and
hotspot score explanations. Current commands do not store full source-file
contents in the index.

Users should treat `.hotpath/index.db` like any other local cache derived from a
private repository and handle it according to their own security and retention
rules.

## Deterministic Report Implications

Reports generated from indexed data should be deterministic for the same
repository state, Hotpath version, schema version, configuration, and command
arguments where practical.

Index-backed reports should preserve the same public ordering guarantees as
fresh scans. In particular, file lists, warnings, diagnostics, and metric rows
should be sorted by stable keys before they are exposed in human-readable or
machine-readable output.

Default reports should avoid timestamps, host-specific absolute paths, and other
local machine state unless those values are the behavior being requested or
tested.
