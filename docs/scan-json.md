# `hotpath scan --json` Output

`hotpath scan --json` emits one compact JSON document to stdout after a scan
completes. It suppresses terminal progress output so callers can parse stdout
directly. Use `hotpath scan --json --pretty` to emit the same document with
four-space indentation for human inspection.

The current schema is versioned with `schema_version: 1`. The matching
machine-readable JSON Schema is in
[`schemas/scan.schema.json`](../schemas/scan.schema.json).

This document describes the public command output, not the local SQLite index
stored under `.hotpath/`. The index is cache data and is not a stable public
format.

## Example

```json
{
  "schema_version": 1,
  "hotpath_version": "0.1.0",
  "scanned_at": "2026-06-18T00:00:00Z",
  "assessment": {
    "is_reliable": true,
    "scoring_confidence": "high",
    "reason": "High scoring coverage and repository context are available"
  },
  "risk": {
    "score": 6.8,
    "band": "high",
    "primary_driver": {
      "id": "churn",
      "label": "Churn"
    },
    "files_by_band": {
      "extreme": 0,
      "high": 1,
      "medium": 2,
      "low": 12
    }
  },
  "scan": {
    "type": "full",
    "duration_ms": 143,
    "files_detected": 42,
    "files_analyzed": 42,
    "git_history": "bounded",
    "commits_processed": 128,
    "commits_total": 512
  },
  "top_hotspots": [
    {
      "rank": 1,
      "path": "internal/service/service.go",
      "score": 7.2,
      "band": "high",
      "reason": "High total churn: 2500 changed lines"
    }
  ],
  "limitations": [
    {
      "code": "language_scope",
      "message": "Only production Go files receive risk scores in the default summary"
    }
  ]
}
```

## Stability

`schema_version` identifies the command JSON shape. Consumers should branch on
that value before reading fields.

Within schema version 1:

- fields documented as required are always present on successful command output
- object keys are emitted in the order shown by the Rust serializer, but JSON
  consumers should not depend on object key order
- arrays use deterministic ordering where the underlying report supports it
- scores are advisory and experimental
- `scanned_at` and `scan.duration_ms` vary between runs
- the JSON describes the completed scan summary, not every indexed row

Future incompatible changes should increment `schema_version`.

## Top-Level Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `schema_version` | integer | yes | JSON output schema version. Currently `1`. |
| `hotpath_version` | string | yes | Hotpath package version that produced the output. |
| `scanned_at` | string | yes | UTC timestamp when the JSON document was created, formatted as `YYYY-MM-DDTHH:MM:SSZ`. |
| `assessment` | object | yes | Reliability, scoring confidence, and a short explanation. |
| `risk` | object | yes | Repository-level advisory risk summary. |
| `scan` | object | yes | Scan execution summary. |
| `top_hotspots` | array | yes | Up to five top production Go file hotspots from the scan summary. |
| `limitations` | array | yes | Human-readable limitations and warnings that should be shown with the result. |

## `assessment`

`assessment` summarizes whether the scan result is a good automation signal.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `is_reliable` | boolean | yes | Convenience signal for automation. |
| `scoring_confidence` | string | yes | Repository-level scoring confidence. See [Scoring Confidence](#scoring-confidence). |
| `reason` | string | yes | Short human-readable explanation for the reliability and confidence values. |

## `risk`

`risk` summarizes repository-level advisory risk from the current Go scoring
model.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `score` | number or null | yes | Repository advisory risk score on a `0.0` to `10.0` scale. `null` when no repository score is available. |
| `band` | string | yes | Risk band: `low`, `medium`, `high`, `extreme`, or `unavailable`. |
| `primary_driver` | object or null | yes | Dominant scoring dimension when Hotpath can map it to a public driver. |
| `files_by_band` | object | yes | Count of scored production Go files by risk band. |

### `risk.primary_driver`

`primary_driver` is `null` when no public driver is available.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | string | yes | Stable driver identifier. Current values are `churn`, `complexity`, and `cochange`. |
| `label` | string | yes | Display label for the driver, such as `Churn`, `Recent churn`, `Complexity`, or `Co-change`. |

### `risk.files_by_band`

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `extreme` | integer | yes | Count of scored production Go files in the `extreme` band. |
| `high` | integer | yes | Count of scored production Go files in the `high` band. |
| `medium` | integer | yes | Count of scored production Go files in the `medium` band. |
| `low` | integer | yes | Count of scored production Go files in the `low` band. |

The counts do not include unscored files, unsupported languages, generated
files, vendored files, or Go test files excluded from the production summary.

## `scan`

`scan` describes the completed command execution.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `type` | string | yes | Scan type: `full` or `incremental`. `incremental` is used when Git data was reused or incrementally updated. |
| `duration_ms` | integer | yes | Total scan duration in milliseconds. This is runtime metadata and is not deterministic. |
| `files_detected` | integer | yes | Repository files enumerated after local ignore rules. |
| `files_analyzed` | integer | yes | Files that completed analysis. |
| `git_history` | string | yes | Git history context used by the assessment: `full`, `bounded`, `incremental`, `first_parent_only`, or `absent`. |
| `commits_processed` | integer | yes | Git commits processed during this scan. This is `0` when Git history is unavailable or an incremental scan reuses existing history. |
| `commits_total` | integer or null | yes | Total Git commits planned for this scan context, or `null` when planning did not report a total. For incremental scans, this can be the incremental range rather than the repository's full commit count. |

## `top_hotspots`

`top_hotspots` contains up to five scored production Go files, ordered by risk
score descending with stable path ordering for ties.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `rank` | integer | yes | One-based rank in this result set. |
| `path` | string | yes | Repository-relative path using Hotpath's normalized path form. |
| `score` | number | yes | File advisory risk score on a `0.0` to `10.0` scale. |
| `band` | string | yes | File risk band: `low`, `medium`, `high`, or `extreme`. |
| `reason` | string or null | yes | Short explanatory fact for the hotspot when available. |

The list is empty when no production Go file receives a score.

## `limitations`

`limitations` contains warnings and approximation boundaries that callers
should display near any score or gate decision.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `code` | string | yes | Stable-enough limitation category for grouping. Current examples include `language_scope` and `git`. |
| `message` | string | yes | Human-readable sentence-style text. Messages are normalized to start with a capital letter and omit trailing punctuation. |

Do not treat `limitations` as exhaustive proof that no other approximation
applies. Hotpath is early-stage software and the scoring model is intentionally
advisory.

## Scoring Confidence

`assessment.scoring_confidence` is a repository-level coverage signal:

| Value | Meaning |
| --- | --- |
| `high` | High scoring coverage for production Go files. |
| `medium` | Partial scoring coverage for production Go files. |
| `low` | Low coverage; repository-level risk is directional. |
| `none` | No production Go files were available to score. |

Unknown future values should be handled conservatively.

## Consumer Guidance

- Validate `schema_version` before reading the rest of the document.
- Treat `assessment.is_reliable` as a quick signal, not a final policy decision.
- Show `limitations` with any score in CI or reports.
- Use `risk.score == null` and `risk.band == "unavailable"` as the no-score
  state.
- Do not parse or depend on `.hotpath/index.sqlite` as a public API.
- Do not compare `scanned_at` or `scan.duration_ms` in golden tests.
