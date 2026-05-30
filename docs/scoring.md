# Scoring

Hotpath hotspot scores are advisory signals for investigation. They are not
proof that a file is defective, insecure, unmaintainable, or owned by the wrong
person. Use them alongside repository context, production history, test
coverage, architectural intent, and team knowledge.

Hotpath is early software. This page documents the currently implemented public
formula so output can be explained and compared. It is not a compatibility
promise for future versions.

## Current Persisted Go Formula

The redesigned scan pipeline currently persists Go file risk scores with:

```text
id: hotpath.score.go.v1
major: 1
minor: 0
```

This formula scores active parsed Go files from the materialized `file_facts`
table. Generated and vendor files are scored, but their flags are preserved so
later reports can filter or group them.

The formula is:

```text
score =
  0.18 * churn
+ 0.14 * recent_churn
+ 0.12 * size
+ 0.14 * ownership_risk
+ 0.10 * cochange_coupling
+ 0.16 * source_coupling
+ 0.16 * cognitive_complexity
```

Additional normalized inputs are:

```text
source_coupling =
  max(
    min(source_coupling_in / 25, 1.0),
    min(source_coupling_out / 15, 1.0)
  )

cognitive_complexity =
  max(
    min(max_function_complexity / 30, 1.0),
    min(cognitive_complexity / 150, 1.0)
  )
```

Persisted rows include the score, public `/10` risk value, rank, weighted terms,
limitations, and concise explanation facts.

## Current Persisted Project Formula

The redesigned scan pipeline also persists a Go-aware project-level risk summary
with:

```text
id: hotpath.project_risk.go.v1
major: 1
minor: 0
```

The project formula aggregates persisted Go file risk rows. It tracks language
coverage and confidence separately, so missing non-Go scoring lowers confidence
instead of directly increasing risk.

```text
project_risk =
  0.35 * max_file_score
+ 0.25 * top_10_mean_score
+ 0.20 * high_risk_share_pressure
+ 0.10 * medium_risk_share_pressure
+ 0.10 * dominant_dimension_pressure
```

The aggregate terms are:

```text
max_file_score = max(file_risk_scores.score)
top_10_mean_score = average(score of top min(10, scored files))
high_risk_share_pressure =
  min((count(score >= 0.70) / scored_file_count) / 0.10, 1.0)
medium_risk_share_pressure =
  min((count(score >= 0.40) / scored_file_count) / 0.30, 1.0)
dominant_dimension_pressure =
  max(mean normalized_value by file-risk term among top 10 files)
```

If no Go files have scores, the project summary is persisted as unavailable
with score `0.0`, confidence `none`, and limitation `no_scored_files`.

## Legacy Hotspot Formula Version

The older documented hotspot formula is:

```text
id: hotpath.score.v3
major: 3
minor: 0
```

Formula identifiers are part of score explanations and persisted score records.
Meaningful changes to score semantics should use a new formula identifier or
version.

The final score is the sum of fixed weighted contributions:

```text
score =
  0.35 * churn
+ 0.20 * size
+ 0.20 * ownership_risk
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
| `ownership_risk` | `ownership_risk` | `0.20` |
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

| Raw metric | Definition | Used by v3 formula |
| --- | --- | --- |
| `byte_size` | File size from scanner metadata, when available. | Size fallback |
| `line_count` | Text line count from scanner metadata, when available. | Size and recent churn |
| `commits_per_file` | Number of local Git commits that touched the path. | Context only |
| `total_churn_lines` | Added plus deleted lines across observed local history. | Churn |
| `recent_churn_lines` | Added plus deleted lines in the recent-history window. | Recent churn |
| `author_count` | Number of distinct exact author identities that touched the path. | Context only |
| `owner_count` | Number of compact displayed operational owners retained for the path. | Ownership risk |
| `dominant_owner_share` | Highest weighted operational owner's share. | Ownership shape and risk |
| `co_changed_file_count` | Number of distinct observed files that co-changed with the path. | Coupling |
| `file_age_days` | Whole days between the file's first observed touch and `HEAD`. | Ownership risk |
| `repository_age_days` | Whole days between the repository's first observed file touch and `HEAD`. | Ownership risk |
| `repository_author_count` | Number of distinct exact author identities in local history. | Ownership risk |
| `repository_file_count` | Number of current scanned files. | Ownership risk |

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

### Ownership Risk

Ownership risk is an interpretive signal. It starts with operational ownership
concentration, then suppresses or amplifies that concentration using repository
maturity and file pressure. Single ownership in a small or new repository can
therefore produce low owner risk.

The owner-count component is:

```text
owner_count_component =
  0.0  when owner_count = 0
  1.0  when owner_count = 1
  0.60 when owner_count = 2
  0.30 when owner_count = 3
  0.0  when owner_count >= 4
```

The concentration component is:

```text
concentration_component = clamp(dominant_owner_share, 0.0, 1.0)
```

When both components are available:

```text
base_concentration = (owner_count_component + concentration_component) / 2
```

When only one component is available, that component is used by itself and a
limitation is recorded. Missing `dominant_owner_share` records
`ownership_risk_missing_owner_share`; missing `owner_count` records
`ownership_risk_missing_owner_count`. If both are unavailable, ownership
risk normalization is omitted and records `missing_ownership_risk_metrics`.

Repository maturity uses local history age, repository contributor breadth,
current file count, and file age. Missing maturity context contributes `0.0`
for that maturity component and records a limitation when the repository-level
fact is unavailable.

```text
repository_maturity =
  0.35 * min(repository_age_days / 730, 1.0)
+ 0.35 * clamp((repository_author_count - 1) / 9, 0.0, 1.0)
+ 0.15 * min(repository_file_count / 200, 1.0)
+ 0.15 * min(file_age_days / 365, 1.0)
```

File pressure uses non-ownership work and coordination signals:

```text
file_pressure =
  0.35 * churn
+ 0.25 * recent_churn
+ 0.25 * coupling
+ 0.15 * size
```

The final normalized ownership-risk input is:

```text
ownership_risk =
  base_concentration
  * (0.20 + 0.80 * repository_maturity)
  * (0.30 + 0.70 * file_pressure)
```

If `dominant_owner_share` is outside `0.0..=1.0`, it is clamped and records
`dominant_owner_share_out_of_range`. If it is not finite, the owner-share
component is omitted and records `invalid_dominant_owner_share`.

See [Operational ownership](ownership.md) for the changed-line, recency, and
sustained-activity model that produces `owner_count` and
`dominant_owner_share`.

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
| `ownership_risk_missing_owner_share` | Owner risk uses owner count only. |
| `ownership_risk_missing_owner_count` | Owner risk uses dominant owner share only, or is omitted if owner share is also unusable. |
| `missing_ownership_risk_metrics` | Neither owner count nor dominant owner share is available. |
| `ownership_risk_missing_repository_age` | Repository age is unavailable, so maturity is lower. |
| `ownership_risk_missing_repository_author_count` | Repository author count is unavailable, so maturity is lower. |
| `ownership_risk_missing_repository_file_count` | Repository file count is unavailable, so maturity is lower. |
| `invalid_dominant_owner_share` | Dominant owner share is not finite and is omitted. |
| `dominant_owner_share_out_of_range` | Dominant owner share is clamped to `0.0..=1.0`. |
| `missing_co_changed_file_count` | Co-change breadth is unavailable. |

## Limitations

The v3 score intentionally uses a small set of local, explainable signals. Known
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
  Operational ownership filters drive-by history, but it does not infer teams or
  review responsibility.
- Binary changes or unavailable Git line statistics contribute `0` line churn.
  Scanner byte size can still provide a size fallback when line count is absent.
- `commits_per_file` is preserved as raw context but is not a weighted term in
  `hotpath.score.v3`.
- Although `hotpath parse` can extract parser-backed symbols and basic
  function/method complexity approximations for Rust, Go, TypeScript, and TSX,
  `hotpath.score.v3` does not consume parser data.
- The formula does not include parser-derived symbol coupling, resolved
  dependency edges, dependency fan-in/fan-out, test coverage, runtime
  incidents, ownership policy, or architectural rule checks.
- Generated and vendor classifications are not part of the v3 weighted formula.

These general limitations are interpretation guidance for v3 scores. The CLI
prints normalization limitations and calculation notes for specific score
records, but a higher score still means "worth looking at sooner," not "bad
code."

## Changing Scores

When scoring behavior changes, tests and documentation should change with it.
Formula versions should make it possible to tell whether a score changed because
the repository changed or because Hotpath changed.
