# Scoring

Hotpath hotspot scores are advisory signals for investigation. They are not
proof that a file is defective, insecure, unmaintainable, or owned by the wrong
person. Use them alongside repository context, production history, test
coverage, architectural intent, and team knowledge.

Hotpath is early software. This page documents the currently implemented public
formula so output can be explained and compared. It is not a compatibility
promise for future versions.

## Current Formula Version

The implemented hotspot formula is:

```text
id: hotpath.score.v1
major: 1
minor: 0
```

Formula identifiers are part of score explanations and persisted hotspot score
records. Meaningful changes to score semantics should use a new formula
identifier or version.

The final score is the sum of fixed weighted contributions:

```text
score =
  0.35 * churn
+ 0.20 * size
+ 0.20 * ownership
+ 0.15 * recent_churn
+ 0.10 * coupling
```

All normalized inputs are bounded to `0.0..=1.0`, so the final score is also in
the `0.0..=1.0` range when all terms are finite.

The weighted terms exposed by explanations are:

| Term name | Normalized metric | Weight |
| --- | --- | ---: |
| `churn_score` | `churn` | `0.35` |
| `size_score` | `size` | `0.20` |
| `author_fragmentation` | `ownership` | `0.20` |
| `recent_growth` | `recent_churn` | `0.15` |
| `cochange_score` | `coupling` | `0.10` |

Missing normalized inputs contribute `0.0` for their term. Weights are not
redistributed across available metrics. This keeps omissions visible instead of
making a partial score look equivalent to a complete one.

## Raw Metrics

Scores are calculated for repository-relative paths using `/` separators. The
current scanner file set defines the files that can be scored. Git facts enrich
those current files from local history reachable from `HEAD`.

The raw inputs are:

| Raw metric | Definition | Used by v1 formula |
| --- | --- | --- |
| `byte_size` | File size from scanner metadata, when available. | Size fallback |
| `line_count` | Text line count from scanner metadata, when available. | Size and recent churn |
| `commits_per_file` | Number of local Git commits that touched the path. | Context only |
| `total_churn_lines` | Added plus deleted lines across observed local history. | Churn |
| `recent_churn_lines` | Added plus deleted lines in the recent-history window. | Recent churn |
| `author_count` | Number of distinct exact author identities that touched the path. | Ownership |
| `dominant_owner_share` | Highest author's share of file-touching commits. | Ownership |
| `co_changed_file_count` | Number of distinct observed files that co-changed with the path. | Coupling |

The default recent-history window is `90` days before the `HEAD` committer
timestamp. It uses local Git metadata, not the machine's current wall-clock
time.

When score inputs are assembled from scanner and Git data, files without Git
metrics are still scored. Their Git count inputs are recorded as `0`, while
`dominant_owner_share` remains unavailable.

## Normalization

Normalization converts raw metrics into bounded formula inputs. Unless noted
otherwise, normalization uses:

```text
min(raw_value / saturation_threshold, 1.0)
```

### Size

Preferred input:

```text
size = min(line_count / 1000, 1.0)
```

If `line_count` is unavailable but `byte_size` is available:

```text
size = min(byte_size / 131072, 1.0)
```

The byte fallback records the limitation
`size_uses_byte_size_fallback`. If both `line_count` and `byte_size` are
unavailable, size normalization is omitted and records `missing_size_metric`.

### Churn

```text
churn = min(total_churn_lines / 2000, 1.0)
```

If `total_churn_lines` is unavailable, churn normalization is omitted and
records `missing_total_churn_lines`.

### Recent Churn

Recent churn measures recent line movement relative to current line count:

```text
recent_churn = clamp(recent_churn_lines / line_count, 0.0, 1.0)
```

This saturates at `1.0` when recent churn is at least the current line count.

If `recent_churn_lines` is unavailable, recent churn normalization is omitted
and records `missing_recent_churn_lines`. If `line_count` is unavailable, it is
omitted and records `missing_recent_growth_line_count`.

For zero-line files:

- `line_count = 0` and `recent_churn_lines = 0` normalizes to `0.0`.
- `line_count = 0` and nonzero `recent_churn_lines` normalizes to `1.0` and
  records `zero_line_count_recent_growth`.

### Ownership

Ownership is an author-fragmentation signal. Higher values mean authorship is
more dispersed in the observed local history.

The author-count component is:

```text
author_component = min(max(author_count - 1, 0) / 5, 1.0)
```

The owner-share component is:

```text
owner_component = 1.0 - clamp(dominant_owner_share, 0.0, 1.0)
```

When both components are available:

```text
ownership = (author_component + owner_component) / 2
```

When only one component is available, that component is used by itself and a
limitation is recorded. Missing `dominant_owner_share` records
`author_fragmentation_missing_owner_share`; missing `author_count` records
`author_fragmentation_missing_author_count`. If both are unavailable, ownership
normalization is omitted and records `missing_author_fragmentation_metrics`.

If `dominant_owner_share` is outside `0.0..=1.0`, it is clamped and records
`dominant_owner_share_out_of_range`. If it is not finite, the owner-share
component is omitted and records `invalid_dominant_owner_share`.

### Coupling

Coupling uses the number of distinct files observed to co-change with the path:

```text
coupling = min(co_changed_file_count / 25, 1.0)
```

This is a breadth signal, not a weighted co-change strength. A file that
co-changed with one other file many times and a file that co-changed with that
file once both count that related file once for this normalized input.

If `co_changed_file_count` is unavailable, coupling normalization is omitted
and records `missing_co_changed_file_count`.

## Ranking

Hotspot ranking is deterministic:

1. Sort by advisory score descending.
2. Break score ties by repository-relative path ascending.
3. Assign one-based ordinal ranks after sorting.

Equal scores do not share a rank after the path tie-breaker is applied.

## Missing Inputs And Limitations

Missing source facts are not guessed during normalization. The normalized value
is omitted, a limitation code is attached, and the fixed weighted contribution
for that term is `0.0`.

Current limitation codes produced by scoring normalization are:

| Code | Meaning |
| --- | --- |
| `size_uses_byte_size_fallback` | Line count is unavailable, so byte size is used for size normalization. |
| `missing_size_metric` | Neither line count nor byte size is available. |
| `missing_total_churn_lines` | Total churn is unavailable. |
| `missing_recent_churn_lines` | Recent churn is unavailable. |
| `missing_recent_growth_line_count` | Line count is unavailable for recent churn normalization. |
| `zero_line_count_recent_growth` | Recent churn exists for a zero-line file, so recent churn saturates. |
| `author_fragmentation_missing_owner_share` | Ownership uses author count only. |
| `author_fragmentation_missing_author_count` | Ownership uses dominant owner share only, or is omitted if owner share is also unusable. |
| `missing_author_fragmentation_metrics` | Neither author count nor dominant owner share is available. |
| `invalid_dominant_owner_share` | Dominant owner share is not finite and is omitted. |
| `dominant_owner_share_out_of_range` | Dominant owner share is clamped to `0.0..=1.0`. |
| `missing_co_changed_file_count` | Co-change breadth is unavailable. |

## Limitations

The v1 score intentionally uses a small set of local, explainable signals. Known
limitations include:

- Scores are based on scanner facts and local Git history reachable from
  `HEAD`; they do not query hosted Git providers, telemetry, or cloud APIs.
- Recent churn depends on commit timestamps in local Git metadata. Rewritten,
  imported, rebased, or otherwise skewed timestamps can distort recency.
- Rename handling is conservative in Git metrics. Earlier history can remain
  associated with the old path.
- Merge commits are diffed against their first parent for the merge commit's own
  file touches.
- Author identity is the exact commit author string. `.mailmap`, bot detection,
  case folding, domain normalization, and account merging are not applied.
- Binary changes or unavailable Git line statistics contribute `0` line churn.
  Scanner byte size can still provide a size fallback when line count is absent.
- `commits_per_file` is preserved as raw context but is not a weighted term in
  `hotpath.score.v1`.
- Although `hotpath parse` can extract parser-backed symbols and basic
  function/method complexity approximations for Rust, Go, TypeScript, and TSX,
  `hotpath.score.v1` does not consume parser data.
- The formula does not include symbol coupling, dependency analysis, test
  coverage, runtime incidents, ownership policy, or architectural rule checks.
- Generated and vendor classifications are not part of the v1 weighted formula.

These general limitations are interpretation guidance for v1 scores. The CLI
prints normalization limitations and calculation notes for specific score
records, but a higher score still means "worth looking at sooner," not "bad
code."

## Changing Scores

When scoring behavior changes, tests and documentation should change with it.
Formula versions should make it possible to tell whether a score changed because
the repository changed or because Hotpath changed.
