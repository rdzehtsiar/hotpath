# Context Estimates

Hotpath is at the beginning of development. `hotpath context` is an early
offline command for estimating how expensive scanned repository files may be to
load into AI coding context. It is planning guidance, not an exact tokenizer or
billing calculation.

## Scope

`hotpath context` estimates token cost from scanner facts already available to
Hotpath. It does not call external APIs, run model-specific tokenizers, collect
telemetry, use network access, or contact cloud services.

The machine-readable report schema identifier is:

```text
hotpath.context.v1
```

The schema identifier describes current output, not a stable released contract.

## Estimate Formula

For each scanned UTF-8 text file included in the context report, Hotpath uses:

```text
estimated_tokens = ceil(byte_size / 4)
```

The estimate is intentionally simple and explainable. It uses the scanned byte
size for supported UTF-8 text files, then rounds up so non-empty partial groups
are not rounded down.

This approximation is useful for planning context size, comparing repository
areas, and deciding what to exclude. It is not tokenizer-specific model billing,
an exact prompt token count, or a guarantee for any particular AI model.

## Skipped Files

Files are skipped from token totals when Hotpath cannot use them as scanned
UTF-8 text inputs for the estimate. Skipped files are counted and reported with
reasons.

Current skipped categories include:

- binary files
- unknown or unreadable files
- files with unsupported encodings
- files missing usable byte-size facts

Generated and vendor files are included by default. Users can exclude them with:

```powershell
hotpath context --exclude-generated
hotpath context --exclude-vendor
```

When those flags are used, matching files are removed from the estimate and
reported as skipped for the corresponding reason.

## Budgets

`hotpath context --budget <tokens>` compares the estimate with a user-provided
budget and reports whether the result is within or over budget.

The budget value accepts integers and `k` or `m` suffixes, for example:

```powershell
hotpath context --budget 128000
hotpath context --budget 128k
hotpath context --budget 1m
```

Budget reporting is advisory. It helps with local planning before copying,
prompting, or selecting repository slices for AI-assisted work.

## Privacy

Context reports are derived from local scanner facts. They may contain
repository-relative paths, grouping information, skipped-file counts, skipped
reasons, byte totals, and estimated token counts. They do not include source
file contents and do not require network calls.
