# Report Schema

Hotpath report output is an early local interface, not a released compatibility
contract. The current aggregate report schema identifier is:

```text
hotpath.report.v1
```

`hotpath report` builds the report from the current local repository state. It
uses scanner facts, local Git history reachable from `HEAD`, ranked
`hotpath.score.v1` hotspot rows, and the default context estimate. The command
does not require network access, telemetry, hosted services, or cloud APIs.

## Command Surface

Current report formats:

```powershell
hotpath report
hotpath report --markdown
hotpath report --json
hotpath report --sarif
hotpath report --html out
```

`hotpath report` defaults to Markdown. `--json`, `--markdown`, `--sarif`, and
`--html <dir>` are mutually exclusive. JSON, Markdown, and SARIF are written to
standard output. HTML creates the output directory if needed and writes a
self-contained `index.html` file.

## JSON Shape

`hotpath report --json` prints pretty JSON with stable repository-relative
paths where practical. The top-level fields are:

| Field | Description |
| --- | --- |
| `schema_version` | Always `hotpath.report.v1` for the current JSON report shape. |
| `summary` | Scan summary, local Git summary, hotspot count, and context token total. |
| `hotspots` | Ranked current-file hotspot rows with score inputs and explanations. |
| `context` | Default context estimate options, summary, groups, skipped rows, and optional budget result. |
| `findings` | Advisory finding rows currently derived from ranked hotspots. |

Abbreviated example:

```json
{
  "schema_version": "hotpath.report.v1",
  "summary": {
    "scan": {
      "total_files": 42
    },
    "git": {
      "head_commit_id": "abc123...",
      "recent_window_days": 90,
      "file_metric_count": 40,
      "co_change_count": 12
    },
    "hotspot_count": 40,
    "context_estimated_tokens": 18000
  },
  "hotspots": [
    {
      "rank": 1,
      "path": "src/lib.rs",
      "score": 0.812,
      "formula_version": {
        "id": "hotpath.score.v1"
      },
      "raw_metrics": {},
      "normalized_metrics": {},
      "weighted_terms": [],
      "limitations": []
    }
  ],
  "context": {
    "options": {},
    "summary": {
      "estimated_tokens": 18000
    },
    "groups": [],
    "skipped": [],
    "budget": null
  },
  "findings": [
    {
      "code": "hotpath.hotspot.risk",
      "level": "info",
      "path": "src/lib.rs",
      "message": "Ranked hotspot #1 with advisory score 0.812",
      "rank": 1,
      "score": 0.812
    }
  ]
}
```

The example is intentionally abbreviated. Consumers should treat unknown fields
inside nested scan, context, and scoring objects as part of the current early
surface, not as a long-term compatibility promise.

## Hotspot Rows

Each `hotspots` row represents one current file ranked by the shared hotspot
scorer. Rows are sorted by rank. Paths are repository-relative and use `/` as
the separator where practical.

Important fields:

| Field | Description |
| --- | --- |
| `rank` | One-based hotspot rank. |
| `path` | Repository-relative file path. |
| `score` | Internal advisory score on the `0.0-1.0` scale. |
| `formula_version` | Scoring formula metadata, currently `hotpath.score.v1`. |
| `raw_metrics` | Local scan and Git inputs used by the score when available. |
| `normalized_metrics` | Normalized metric values used by weighted terms. |
| `weighted_terms` | Per-term contribution details for explainability. |
| `limitations` | Known missing or approximate inputs for the row. |

For human-facing risk, Hotpath displays `score * 10.0` as a `/10` value. The
JSON report keeps the raw score and the underlying explanation payloads.

## Context And Findings

The `context` object uses the same default byte-based estimate as `hotpath
context`:

```text
estimated_tokens = ceil(byte_size / 4)
```

The estimate is useful for rough local planning. It is not a model-specific
tokenizer result or billing estimate.

The `findings` array currently contains advisory hotspot findings with code
`hotpath.hotspot.risk`. Findings are decision-support output. They are not
security findings, test failures, architecture violations, or proof that a file
is defective.

## SARIF

`hotpath report --sarif` emits SARIF 2.1.0. It uses one tool driver named
`Hotpath` and one rule ID:

```text
hotpath.hotspot.risk
```

Each ranked hotspot becomes one SARIF result with a repository-relative
artifact URI. SARIF levels are derived from the public `/10` risk value:

| Risk | SARIF level |
| ---: | --- |
| `>= 8.0` | `error` |
| `>= 5.0` and `< 8.0` | `warning` |
| `< 5.0` | `note` |

SARIF is provided for CI systems and code scanning tools that understand SARIF.
It is not a separate Hotpath JSON schema identifier.

## HTML And Markdown

Markdown and HTML are human-readable summaries. They include summary facts, the
top hotspot rows, context estimate information, advisory findings, and
calculation notes. JSON is the complete Hotpath report surface for current
machine consumers.

Static HTML is local and self-contained. It does not use remote assets, scripts,
hosted dashboards, telemetry, or network calls.

## Persistence And Limits

Successful report generation persists the same derived local data used by
scanner, Git, and hotspot workflows:

- current scan rows
- local Git analysis rows
- hotspot score rows

Reports require a readable non-bare local Git worktree with a commit at `HEAD`
and complete local history. Shallow repositories are rejected instead of
producing partial reports. Portable report output should avoid host-specific
absolute paths and source-file contents, but it may still expose sensitive
repository-relative paths and derived metrics.
