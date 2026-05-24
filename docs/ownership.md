# Operational Ownership

Hotpath operational ownership estimates who meaningfully owns and maintains a
file today. It is not a raw list of everyone who ever touched the file, and it
is not by itself a risk verdict.

The model is deterministic, offline, and derived only from local Git history
reachable from `HEAD`. It favors changed lines, recent activity, and sustained
engagement so drive-by edits, historical originators, formatting sweeps, and
one-time changes do not dominate ownership output forever.

## Inputs

Operational ownership uses:

- repository-relative file paths from local Git changes
- exact commit author identity strings
- added plus deleted lines for each file change
- commit timestamps, measured relative to the `HEAD` committer timestamp
- the number of files touched by the same commit
- repeated meaningful commits by the same author on the same file

Author identities are not merged through `.mailmap`, account lookup, bot
detection, case folding, or domain normalization.

## Per-Change Weighting

For each file change:

```text
changed_lines = added_lines + deleted_lines
```

Changes with `changed_lines = 0` do not create ownership weight.

Large multi-file commits are dampened to reduce the effect of broad formatting,
mechanical, dependency, or generated rewrites:

```text
bulk_weight =
  1.0                                      when touched_file_count <= 10
  max(sqrt(10 / touched_file_count), 0.10) when touched_file_count > 10
```

Recency uses a two-year half-life:

```text
age_days = max((HEAD_time - commit_time) / 1 day, 0)
recency_weight = 0.5 ^ (age_days / 730)
```

The per-change base contribution is:

```text
line_recency_score = changed_lines * bulk_weight * recency_weight
```

## Sustained Activity

After per-change scores are accumulated by file and author, Hotpath applies a
sustained-activity multiplier for that author on that file:

```text
sustained_activity_weight =
  0.25 when meaningful_commits = 1
  0.60 when meaningful_commits = 2
  1.00 when meaningful_commits >= 3
```

The final author score is:

```text
ownership_score =
  sum(line_recency_score for author and file) * sustained_activity_weight
```

## Contributor Filtering

An author is eligible for compact ownership display when:

```text
(weighted_share >= 10% OR rank <= 3)
AND
(effective_changed_lines >= line_floor OR meaningful_commits >= 3)
```

The line floor is path-local and capped so small files can still have owners:

```text
line_floor = min(200, max(1, ceil(file_effective_changed_lines * 0.05)))
```

This keeps tiny, stale, or one-time edits out of ownership unless they are among
the only meaningful signals for the file.

## Display

Ownership output is intentionally compact:

```text
Ownership

alice              78%
bob                14%
others              8%
```

Rules:

- show at most the top three meaningful owners
- sort by weighted share descending, then author identity ascending
- collapse remaining positive ownership into `others`
- omit raw commit counts, timestamps, email-only metadata, and Git hashes from
  the compact inspector section

`owner_count` in hotspot scoring is the number of meaningful owners retained in
this compact operational summary, not the historical contributor count.

## Ownership Shape

The inspector classifies the top weighted operational owner share as a
descriptive ownership shape:

```text
> 90%       SINGLE OWNER
70%..90%   CONCENTRATED
40%..70%   SHARED
< 40%      DISTRIBUTED
```

These labels describe distribution only. `SINGLE OWNER` is normal for many
small, young, or intentionally focused projects and should not be read as an
alarm by itself.

## Ownership Risk

Owner risk answers a different question: how dangerous is this concentration in
the current repository context?

Hotpath combines ownership concentration with maturity and file-pressure
signals:

```text
ownership_risk =
  base_concentration
  * repository_maturity_factor
  * file_pressure_factor
```

Repository maturity is higher for older repositories, repositories with more
contributors, larger current file sets, and older files. File pressure is higher
for files with churn, recent churn, co-change breadth, and size pressure.

This means:

- a small new one-maintainer repository can show `SINGLE OWNER` and `LOW` owner
  risk
- a mature high-churn subsystem can show `SINGLE OWNER` or `CONCENTRATED` and
  `HIGH` owner risk
- distributed ownership can still be displayed as a shape without implying that
  ownership is the main risk driver

The inspector renders these separately:

```text
Ownership    SINGLE OWNER
Owner Risk   [bar] LOW
```

Only `Owner Risk` uses a severity bar.

## Limitations

Operational ownership is still a local approximation:

- it does not know code review ownership, team ownership, on-call rotation, or
  production responsibility
- it does not semantically detect formatting-only edits beyond bulk-change
  dampening and zero-line filtering
- it does not merge identities or detect bots
- conservative rename handling can split ownership history across old and new
  paths
- skewed Git timestamps can distort recency
- binary changes or unavailable line statistics do not add ownership weight

The output should answer "who meaningfully owns and maintains this file today?"
better than raw contributor history, but it remains advisory.
