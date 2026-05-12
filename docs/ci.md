# CI Usage

Hotpath can run as an offline CI risk gate for the current repository checkout.
The current command is:

```powershell
hotpath ci --fail-on-risk 8
```

The gate builds the same aggregate report model used by `hotpath report`, then
checks the highest ranked hotspot score on a public `0-10` scale. It does not
call hosted Git providers, cloud APIs, telemetry services, or external
tokenizers.

## Threshold Semantics

`--fail-on-risk <threshold>` accepts finite numeric values greater than `0` and
at most `10`.

Hotpath keeps hotspot scores internally on a `0.0-1.0` scale. CI converts the
highest hotspot score to a public risk value by multiplying by `10.0`:

```text
risk = score * 10.0
```

The command fails when:

```text
max_risk >= threshold
```

For example, `--fail-on-risk 8` fails when the highest current hotspot risk is
`8.0/10` or higher.

## Output

Passing example:

```text
Hotpath CI risk
result: pass
threshold: 8.000/10
max risk: 6.420/10
highest-risk file: src/lib.rs
```

Failing example:

```text
Hotpath CI risk
result: fail
threshold: 8.000/10
max risk: 8.250/10
highest-risk file: src/lib.rs
```

If no current files can be ranked as hotspots, the command reports no maximum
risk and passes because no score reaches the threshold:

```text
Hotpath CI risk
result: pass
threshold: 8.000/10
max risk: none
highest-risk file: none
```

Output is intentionally concise so CI logs stay readable. Use `hotpath report
--json`, `hotpath report --markdown`, `hotpath report --sarif`, or `hotpath
report --html <dir>` when the job should also publish a report artifact.

## Exit Codes

Current exit-code behavior:

| Exit code | Meaning |
| ---: | --- |
| `0` | Analysis succeeded and the maximum hotspot risk is below the threshold. |
| `1` | Analysis succeeded and the maximum hotspot risk is greater than or equal to the threshold. |
| `2` | Hotpath could not run the analysis, such as unsupported repository state, Git errors, scan errors, or index persistence errors. |

Invalid CLI arguments, including invalid threshold values, are rejected before
analysis and before `.hotpath` persistence. The CLI parser may also use exit
code `2` for argument errors.

## Repository Requirements

The CI gate currently requires:

- a readable non-bare local Git worktree
- a commit at `HEAD`
- complete local history, not a shallow clone
- repository files readable by the current process
- permission to create or update Hotpath's local `.hotpath/index.db` cache

The command scans the current worktree for file facts and combines those facts
with local Git history reachable from `HEAD`. Uncommitted file additions,
deletions, size changes, and generated/vendor classifications can affect the
current scan side of the report, but Git churn and ownership metrics still come
from committed local history. The command does not fetch remote refs or infer
hosted pull request metadata.

## GitHub Actions Example

Example job step:

```yaml
- name: Run Hotpath risk gate
  run: hotpath ci --fail-on-risk 8
```

To keep a report artifact, generate it in a separate step:

```yaml
- name: Write Hotpath HTML report
  run: hotpath report --html hotpath-report
```

The HTML report is a local static artifact. Publishing it is a CI-system choice,
not something Hotpath does automatically.

## Current Limits

The CI gate is advisory. It should be treated as a deterministic local signal
for investigation, not a complete release-quality decision.

Current limitations:

- the gate evaluates the current aggregate repository hotspot report, not
  diff-specific touched-hotspot risk
- architecture rules are not evaluated or enforced yet
- scores do not include test coverage, runtime incidents, hosted ownership
  policy, security findings, or production reliability signals
- shallow repositories and unsupported Git states fail as operational errors
  rather than producing partial risk scores
- generated reports and CI logs can reveal repository-relative paths and
  derived metrics, so users should handle artifacts according to their own
  retention and security rules
