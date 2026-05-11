# Git Metric Semantics

Hotpath Git analysis is early and not yet a stable report format. This page
defines the default semantics the implementation follows so fixture
repositories, indexes, and explain output can be tested against a public
contract.

Git metrics are advisory signals. They can point to files that may be volatile,
recently active, or ownership-fragmented, but they do not prove that a file is
bad, risky, or owned by the wrong person.

## History Scope

By default, Git metrics use local repository history reachable from `HEAD`.
Hotpath should not call GitHub, another hosted Git provider, telemetry, or cloud
APIs to calculate these metrics.

Repositories with shallow local history are rejected before metrics are
rendered or persisted. The current implementation does not estimate missing
commit history from a shallow clone because that would make churn, ownership,
age, and co-change metrics misleading.

The reference time for time-windowed metrics is the `HEAD` commit's committer
timestamp from the local Git history, not the machine's current wall-clock time.
This keeps results reproducible for the same repository state. Elsewhere on
this page, "commit time" means the committer timestamp.

For each commit in scope:

- normal commits are diffed against their first and only parent
- merge commits are diffed against the first parent
- root commits are diffed against the empty tree

The scope is all commits reachable from `HEAD`, not only the first-parent chain.
First-parent diffing for merge commits defines the merge commit's own file
touches; it does not remove reachable side-branch commits from analysis.

## Path Keys

Metric keys are repository-relative file paths using `/` as the separator.
Portable reports should not expose absolute paths, drive prefixes, UNC prefixes,
home-directory expansions, or paths that escape the repository root.

When metric rows are exposed without a ranking rule, they should be sorted by
path in ascending lexicographic order. When ranked output is needed, rows should
sort by the ranking value first and then by path as a stable tie-breaker.

## File Touches

A file-touching commit is a commit whose selected diff includes that
repository-relative path as an added, modified, deleted, type-changed, renamed,
or copied path.

Multiple hunks or repeated diff records for the same path in one commit count as
one file touch for commit-counting, authorship, ownership, and co-change
purposes.

Line churn uses the added and deleted line counts reported by the selected diff.
For binary changes or file changes where line counts are unavailable, line churn
is `0` for that change and the limitation should be explainable in output that
surfaces the metric.

## Rename Handling

Rename handling is conservative by default. A rename counts as a touch for the
destination path at the rename commit, but Hotpath should not reconstruct the
destination file's full pre-rename history under the old path unless a future
feature explicitly documents and tests that behavior.

This means:

- history before a rename remains associated with the old path
- history after a rename is associated with the new path
- the rename commit itself contributes to the new path
- file age for the new path starts at the first observed commit for that path,
  which may be the rename commit

This can understate age, churn, and authorship continuity for renamed files. The
limitation is intentional until full rename reconstruction is implemented and
tested.

## Commits Per File

`commits_per_file` is the number of distinct file-touching commits for a path
within the history scope.

Formula:

```text
commits_per_file(path) =
  count(distinct commit_id where commit touches path)
```

A commit contributes at most `1` to a path, even when it changes many lines in
that file.

## Recent Churn

The default recent churn window is `90` days relative to the `HEAD` commit time.
A commit is inside the window when its commit time is greater than or equal to:

```text
HEAD commit time - 90 days
```

Recent churn is the sum of added and deleted lines for a path in file-touching
commits inside that window. It is not net line change.

Formula:

```text
recent_churn_added(path) =
  sum(added_lines for path touches in commits inside the recent window)

recent_churn_deleted(path) =
  sum(deleted_lines for path touches in commits inside the recent window)

recent_churn(path) =
  recent_churn_added(path) + recent_churn_deleted(path)
```

Commits after the `HEAD` commit time should not exist in normal reachable
history. If skewed commit metadata creates a negative or future interval,
Hotpath should keep the calculation deterministic and document the limitation
instead of using wall-clock time as a fallback.

## Author Count

`author_count` is the number of distinct authors that have file-touching commits
for a path within the history scope.

Author identity is the exact commit author string:

```text
Name <email>
```

Hotpath should not apply `.mailmap`, case folding, bot detection, domain
normalization, or account merging by default.

Formula:

```text
author_count(path) =
  count(distinct exact_author_identity for commits that touch path)
```

## Dominant Ownership

The dominant owner for a path is the author identity with the highest number of
file-touching commits for that path.

Tie-break rule:

```text
sort by touch_count descending, then exact_author_identity ascending
```

The ascending author string tie-breaker makes results stable when two or more
authors have the same touch count.

Dominant ownership share is:

```text
dominant_owner_share(path) =
  dominant_owner_touch_count(path) / commits_per_file(path)
```

If `commits_per_file` is `0`, dominant owner and dominant owner share are
undefined and should be omitted or reported as unavailable rather than forced to
an arbitrary value.

## Co-Change

Co-change measures unordered file pairs touched in the same commit.

For each commit:

1. Build the set of unique touched paths for that commit.
2. Sort the set lexicographically.
3. Emit every unordered pair `(left_path, right_path)` where `left_path` sorts
   before `right_path`.
4. Count each pair at most once for that commit.

Formula:

```text
co_change_count(left_path, right_path) =
  count(commits where both paths are touched)
```

Single-file commits do not contribute co-change pairs. Commits that touch the
same file through multiple hunks or diff records still contribute at most one
pair count for any related file pair.

Co-change output should be deterministic. Default pair keys should store the
lexicographically smaller path as `left_path` and the larger path as
`right_path`. Ranked co-change output should sort by `co_change_count`
descending, then `left_path` ascending, then `right_path` ascending.

## File Age

File age is measured in whole days between the first observed file-touching
commit for a path and the `HEAD` commit time.

Formula:

```text
file_age_days(path) =
  floor((HEAD commit time - first_observed_commit_time(path)) / 1 day)
```

The first observed commit is path-based. With conservative rename handling, a
renamed file's age under the new path starts at the first commit where the new
path is observed.

If commit timestamps are skewed and the first observed commit time is after the
`HEAD` commit time, Hotpath should report `0` days and surface the timestamp
skew as a limitation where the age is explained.

## Current Implementation Status

These semantics define the target behavior for Git metrics. `hotpath
explain-git` is an early, non-stable command that computes local `HEAD` Git
history and persists the resulting full-repository file metrics and co-change
pairs to the local Hotpath index as derived cache data after successful
analysis. Current Hotpath scans do not compute Git metrics.

Future implementation work should add deterministic fixture repositories and
focused tests that cover commit counting, recent churn windows, exact author
identity, dominant owner tie-breaking, co-change ordering, root commits, merge
commits, and conservative rename behavior.
