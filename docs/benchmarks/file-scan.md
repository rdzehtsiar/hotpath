# File Scan Benchmark Methodology

Hotpath is at the beginning of development. This document defines a public,
repeatable methodology for benchmarking file scanning. It does not publish a
baseline result, performance target, or compatibility promise.

Use this methodology when measuring the scan command against real repositories
or fixtures. Do not report performance numbers unless the command, build,
hardware, repository revisions, and run protocol are documented with the result.

## Scope

The file scan benchmark measures the local repository walk and file
classification path exposed by:

```powershell
cargo run --release -- scan --json
```

For the current implementation, the scan includes repository-relative file
walking, `.gitignore` handling, file size collection, conservative language
guessing, generated and vendor path flags, binary and UTF-8 text
classification, line counting for text files within the read limit, symlink
handling, deterministic file ordering, and JSON rendering.

The benchmark should not include network access, dependency installation,
repository cloning time, Git history analysis, scoring, indexing, report
post-processing, terminal rendering, or unrelated shell startup work unless a
specific result clearly says those costs are included.

## Run Protocol

Use a release build and run from the root of the benchmarked repository.

Record at least:

- Hotpath version, commit, or release artifact
- Rust toolchain version
- build command and profile
- exact scan command
- repository name and exact revision
- operating system and filesystem
- number of warmup runs
- number of measured runs
- timing tool and version
- whether output was written to disk or discarded
- whether the benchmark represents warm filesystem cache, cold filesystem
  cache, or both

Prefer separate warm-cache and cold-cache results. Warm-cache results are easier
to reproduce across ordinary developer machines. Cold-cache results are useful
only when the cache-clearing procedure is documented and can be repeated without
administrator-only or host-specific assumptions.

Use a minimum of 10 measured runs for local comparisons. Report median time,
minimum time, maximum time, and a spread measure such as standard deviation or
median absolute deviation when the timing tool provides it.

## Hardware And Environment Fields

Include the following fields with every published benchmark result:

| Field | Value |
| --- | --- |
| Machine identifier | Example: `local workstation`, `CI runner type`, or anonymized host label |
| CPU model |  |
| Physical cores / logical threads |  |
| Memory |  |
| Storage device type | Example: `NVMe SSD`, `SATA SSD`, `network disk` |
| Filesystem | Example: `NTFS`, `APFS`, `ext4` |
| Operating system |  |
| Kernel or OS build |  |
| Rust toolchain | Output from `rustc --version` |
| Cargo version | Output from `cargo --version` |
| Hotpath version or commit |  |
| Build command |  |
| Power mode | Example: `balanced`, `performance`, `battery` |
| Virtualization or containerization | Example: `none`, `WSL2`, `Docker`, `CI VM` |
| Antivirus or indexing notes | Example: `default Windows Defender`, `disabled by policy`, `unknown` |
| Thermal or load notes | Example: `idle before run`, `shared CI host`, `unknown` |

Do not publish private hostnames, usernames, absolute local paths, or sensitive
repository locations.

## Repository Selection

Benchmark repositories should be selected before running comparisons. Avoid
changing the sample after seeing results.

Prefer a small public suite that covers different repository shapes:

- a small Rust repository
- a medium application repository
- a repository with many small files
- a repository with generated or vendored paths
- a repository with binary assets
- a repository with enough `.gitignore` rules to exercise ignore handling

For each repository, record:

| Field | Value |
| --- | --- |
| Repository name |  |
| Public URL or fixture description |  |
| License |  |
| Commit SHA or immutable archive identifier |  |
| Submodules included | `yes` / `no` / `not applicable` |
| Git LFS files included | `yes` / `no` / `not applicable` |
| Benchmark root | Repository-relative path, usually `.` |
| Total files reported by Hotpath |  |
| Total bytes reported by Hotpath |  |
| Text / binary / unknown files |  |
| Generated files reported by Hotpath |  |
| Vendor files reported by Hotpath |  |
| Warning count |  |
| Notes |  |

If a repository cannot be public, describe it as a private corpus and omit
results from public comparisons unless the repository shape can be disclosed
without revealing sensitive information.

## Benchmark Commands

Build Hotpath once before measuring:

```powershell
cargo build --release
```

Record toolchain versions:

```powershell
rustc --version
cargo --version
target\release\hotpath.exe --help
target\release\hotpath.exe scan --help
```

Capture the scan summary for repository shape metadata:

```powershell
target\release\hotpath.exe scan --summary
```

Capture JSON when validating output or recording scan metadata:

```powershell
target\release\hotpath.exe scan --json > hotpath-scan.json
```

Measure a quick local run with PowerShell:

```powershell
Measure-Command { target\release\hotpath.exe scan --json *> $null }
```

For publishable timing, prefer a dedicated timing tool such as `hyperfine`:

```powershell
hyperfine --warmup 3 --runs 10 --export-json hotpath-file-scan.hyperfine.json "target\release\hotpath.exe scan --json"
```

When benchmarking on Unix-like shells, use the platform path for the release
binary:

```sh
cargo build --release
./target/release/hotpath scan --summary
hyperfine --warmup 3 --runs 10 --export-json hotpath-file-scan.hyperfine.json './target/release/hotpath scan --json'
```

If redirecting output, keep the redirection consistent across all compared
runs. JSON rendering is part of the current command path, but writing large
outputs to disk may add storage noise.

## Result Table Template

Use one row per repository, command, environment, and cache mode.

| Date | Hotpath commit | Repository | Repo commit | OS / filesystem | CPU | Storage | Cache mode | Command | Runs | Median | Min | Max | Std dev or MAD | Total files | Total bytes | Warnings | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| YYYY-MM-DD |  |  |  |  |  |  | warm | `target\release\hotpath.exe scan --json` | 10 |  |  |  |  |  |  |  |  |

Only fill timing columns with values produced by the documented run protocol.
Do not mix debug and release builds, different output modes, different cache
modes, or different repository revisions in the same comparison row.

## Reproduction Steps

1. Install the recorded Rust toolchain.
2. Check out the recorded Hotpath commit.
3. Build Hotpath with the recorded build command.
4. Check out each benchmark repository at the recorded commit or archive
   identifier.
5. Confirm submodule and Git LFS handling matches the recorded repository
   fields.
6. From the benchmark root, run `hotpath scan --summary` and record repository
   shape fields.
7. Run the warmup and measured commands exactly as recorded.
8. Store raw timing output next to the summarized result.
9. Re-run any surprising result before publishing it.
10. Publish the result table, raw timing output, environment fields, repository
    fields, and any deviations from this methodology.

## Limitations

File scan benchmarks are sensitive to filesystem cache state, storage hardware,
antivirus and indexing software, virtualization, background load, repository
layout, file sizes, ignored paths, binary assets, and generated or vendored
trees.

Current scan results are early and may change as Hotpath evolves. A benchmark
from one commit may not be comparable to another commit if file walking,
classification, output fields, read limits, ignore behavior, or JSON rendering
changes.

Elapsed time alone does not explain scan quality. Benchmark results should be
read alongside correctness tests, deterministic output checks, documented
limitations, and the repository shape fields reported with the result.

Do not generalize a result from one machine or one repository to all Hotpath
users. Treat benchmark numbers as local evidence for a specific version,
environment, repository, command, and methodology.
