# Diff And PR Risk

Hotpath is at the beginning of development. The current `hotpath diff` and
`hotpath pr` commands are early local reports, not a released stable CLI or JSON
contract.

## Command Surface

Current commands:

```powershell
hotpath diff main...HEAD
hotpath diff main...HEAD --json
hotpath pr --base main --head HEAD
hotpath pr --base main --head HEAD --json
```

`hotpath pr` is a convenience form for the same committed-tree comparison as
`hotpath diff <base>...<head>`. It does not contact GitHub, GitLab, a hosted
pull request API, or any network service.

## Scope

Diff and PR risk analysis is offline and local. It reads committed Git objects
from the local worktree and compares the merge-base tree of the requested base
and head refs against the requested head tree.

Current scope and requirements:

- the command must run inside a readable non-bare local Git worktree
- the repository must have a commit at `HEAD`
- shallow repositories are rejected instead of producing partial risk reports
- the range head must resolve to the currently checked-out `HEAD`
- uncommitted working tree and index changes are not part of the comparison
- paths in reports are repository-relative and sorted deterministically where
  practical

The current-head requirement means a user should check out the branch or commit
being analyzed before running `hotpath diff` or `hotpath pr`.

## Output

Text output currently includes:

- requested range
- changed file count
- touched hotspot count
- architecture violation status
- estimated context growth
- touched hotspot rows
- changed file rows with change kind, line counts, and context token delta

JSON output currently uses schema identifier `hotpath.diff.v1`. The current
top-level fields are:

- `schema_version`
- `range`
- `summary`
- `changed_files`
- `architecture`

The `range` object includes the requested range, base ref, head ref, resolved
base commit id, resolved head commit id, and merge-base commit id. The
`summary` object includes changed file count, touched hotspot count, touched
hotspot rows, context token delta, and architecture status. Changed file rows
include repository-relative path, change kind, old/new paths when available,
added/deleted line counts, context token delta, and skipped context reasons.

The JSON shape is versioned for inspection and tests, but it is not a stable
released compatibility promise yet.

## Context Growth

Context growth is an approximation over Git blobs, not a tokenizer-specific
measurement. For each UTF-8 Git blob side that can be read, Hotpath uses:

```text
estimated_tokens = ceil(byte_size / 4)
```

Per-file context delta is calculated as new-side estimated tokens minus
old-side estimated tokens. Binary sides and invalid UTF-8 sides are skipped and
recorded in `skipped_context`; skipped sides contribute `0` to the delta.

This estimate is useful for rough local planning only. It does not use model
tokenizers, external APIs, telemetry, network access, or cloud calls.

## Hotspots And Architecture

Diff and PR reports currently combine changed files with the existing local
hotspot ranking. A touched hotspot means a changed current file is present in
the current top hotspot rows from `hotpath.score.v3`.

Architecture violations are reported as `not_evaluated` until architecture
rules exist. The current command should not be read as enforcing architecture
policy.

## Exit Codes

Current exit-code policy:

- `0` when a text or JSON report is generated
- `1` for invalid arguments, unsupported repositories, Git errors, index
  persistence errors, scan errors, or report rendering errors

Diff and PR commands do not have a fail-on-risk threshold. A report with
touched hotspots, positive context growth, or `not_evaluated` architecture
status still exits `0` when it is generated successfully.

For CI gating, use `hotpath ci --fail-on-risk <threshold>`. That command builds
the current aggregate repository hotspot report and fails when the maximum
hotspot score on the public `0-10` risk scale is greater than or equal to the
threshold. It does not gate this diff/PR report, touched-hotspot count, context
growth, or architecture status.

## Index Persistence

Successful diff and PR runs persist the same derived local data used by current
scan, Git, and hotspot commands:

- current scan rows
- local Git analysis rows
- hotspot score rows

They do not create a diff-specific index table or persist diff report rows in
the current index schema.
