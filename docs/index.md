# Local Index

Hotpath is at the beginning of development. The local index exists now, but it
is still an early derived cache, not a stable public database format or
compatibility promise.

The index makes repeated local analysis easier to inspect and extend. It is
created from repository files and Hotpath's scanner results. It is not
user-authored data and should be safe to delete and rebuild.

## Location And Scope

The current index location is:

```text
.hotpath/index.db
```

The `.hotpath/` directory is created at the repository root being analyzed when
`hotpath scan` persists results. `hotpath doctor` can inspect this location, but
it does not create a missing index.

The `.hotpath/index.db` file is local working data, not a portable report
format. Users should not commit it, share it, or edit it by hand.

## Current Schema

The current SQLite schema version is `2`. The index also stores the schema
identifier `hotpath.index.v2` in metadata. Hotpath rejects indexes with missing,
unknown, malformed, corrupt, or future schema metadata instead of reading them
best-effort.

The schema contains tables for current scanner persistence and sparse extension
points for later analysis. The presence of a table does not mean the related
feature is implemented.

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

Currently populated by `hotpath explain-git` after successful local history
analysis:

- Git analysis metadata, including analyzer version, `HEAD` commit id, `HEAD`
  committer timestamp, recent churn window, and observed row counts
- per-file Git metrics such as commit count, total churn, recent churn, author
  count, dominant owner/share, first/last observed commits, and file age
- co-change pairs with deterministic left/right repository-relative paths and
  commit counts

Reserved as schema extension points but not populated by current scans:

- `symbols` for future parser output
- `dependencies` for future coupling or dependency analysis
- `hotspots` for future scoring output

Current scans do not populate parser symbols, dependency edges, or hotspot
scores. Documentation and reports should not imply those analyses are available
until implementation and tests exist.

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
stale files, such as file warnings and reserved future metrics tied by foreign
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
classification, symlink classification, scan warnings, and file warning
messages. Current scans do not store full source-file contents in the index.

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
