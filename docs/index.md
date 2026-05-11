# Index Invariants

Hotpath is at the beginning of development. This document chooses the intended
invariants for the future local index; it does not describe a stable database
format, released CLI behavior, or compatibility promise.

The index exists to make repeated local analysis faster and more explainable.
It is derived from repository files, Git metadata, local configuration, and
Hotpath's own analysis rules. It is not user-authored data and should be safe to
delete and rebuild.

## Location And Scope

The planned index location is:

```text
.hotpath/index.db
```

The path is relative to the repository root being analyzed. A repository should
have at most one active Hotpath index at that location for a given working tree.

The `.hotpath/index.db` file is local working data, not a portable report
format. Users should not need to commit it, share it, or edit it by hand.

## Schema Versioning

The index schema version is separate from report schema versions such as the
current scan JSON schema, `hotpath.scan.v1`.

The initial index schema identifier should be:

```text
hotpath.index.v1
```

The index must store its schema version in database metadata before Hotpath
reads analysis data from it. Implementations should reject unknown, missing, or
incompatible schema versions instead of attempting best-effort reads.

During early development, Hotpath may rebuild an incompatible index instead of
migrating it. Once migrations are supported, each migration should be explicit,
tested, and documented with the schema versions it accepts and produces.

## Path Storage

File identities in the index must be stored as repository-root-relative paths.
Portable indexed paths should:

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

Scan persistence should be transaction-like: a completed scan either updates
the index consistently or leaves the previous committed index state available.

When a scan is successfully persisted, the current scan's file set replaces the
previous active file set for that repository root. File rows and per-file metric
rows that were present in an older scan but not observed in the current scan
should be deleted during persistence. They should not remain visible as active
files in later reports.

Failed, interrupted, or cancelled scans should not partially delete old file
rows or partially publish new file rows. If Hotpath cannot confidently persist a
complete scan, it should leave the last valid committed index state intact and
surface an actionable error or warning.

## Corruption Handling

Hotpath must not produce reports from an index it knows is corrupt. Corruption
includes database open failures, malformed required metadata, failed integrity
checks, missing required schema objects, or values that violate documented index
invariants.

Because `.hotpath/index.db` is derived local cache data, the supported recovery
model is rebuild from repository source data. A corrupt index should lead to one
of these outcomes:

- an explicit rebuild path creates a fresh index without relying on corrupt
  records
- Hotpath fails with an actionable message explaining that deleting
  `.hotpath/index.db` is safe and that the next index build can recreate it

Hotpath should not silently mix data from a corrupt index with fresh scan data.

## Rebuild And Delete Behavior

Deleting `.hotpath/index.db` should remove only Hotpath's local derived index.
It should not affect repository source files, Git history, project
configuration, or user-requested report files stored elsewhere.

A future rebuild command should be equivalent to discarding the old index and
creating a new one from the current repository state. Rebuild behavior should
not depend on old indexed metrics being readable.

If a scan needs an index and no index exists, Hotpath should create a new index
using the current schema. If Hotpath encounters an incompatible schema, it
should either rebuild explicitly or stop with instructions that make the delete
and rebuild behavior clear.

## Privacy Posture

The index is local by default. Creating, reading, validating, deleting, or
rebuilding `.hotpath/index.db` should not require network access, telemetry,
cloud APIs, hosted services, or uploading repository contents.

The index may contain sensitive repository-derived information, including file
paths, sizes, language classifications, generated or vendor flags, Git-derived
metrics, ownership summaries, and future analysis metrics. It should avoid full
source-file contents by default unless a future feature explicitly documents why
content storage is needed and how users control it.

Portable reports should continue to avoid host-specific absolute paths unless a
user explicitly requests them. The local index should follow the same path
normalization rule where practical.

## Deterministic Report Implications

Reports generated from an index should be deterministic for the same repository
state, Hotpath version, schema version, configuration, and command arguments
where practical.

Index-backed reports should preserve the same public ordering guarantees as
fresh scans. In particular, file lists, warnings, diagnostics, and metric rows
should be sorted by stable keys before they are exposed in human-readable or
machine-readable output.

Stale-row deletion is part of the determinism contract: removed files should not
continue to appear in reports merely because they existed in a previous scan.

Default reports should avoid timestamps, host-specific absolute paths, and other
local machine state unless those values are the behavior being requested or
tested.
