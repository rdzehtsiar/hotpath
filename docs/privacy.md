# Privacy

Hotpath is designed as a local tool. The core workflow should not require
network access, telemetry, cloud APIs, hosted services, or uploading repository
contents.

Hotpath is at the beginning of development. This document defines the privacy
posture the project is being built toward; it does not describe a released
binary or complete implementation.

## Default Posture

Hotpath's primary workflows should provide:

- no telemetry by default
- no network calls for scanning, scoring, indexing, or reports
- no cloud APIs for repository analysis
- no hosted service dependency
- local storage for indexes, caches, and reports
- transparent documentation for any future behavior that could access external
  systems

The default expectation is that repository contents and derived metrics stay on
the user's machine.

## Local Data Hotpath May Read

To provide codebase intelligence, Hotpath may read local data such as:

- repository files and directories
- Git history and Git metadata
- ignore files and local configuration
- dependency manifests and project metadata
- previously created local Hotpath indexes or reports

Hotpath should read only what is needed for local analysis and should document
unsupported file types, encodings, generated files, vendor directories, and
other classification limits.

## Local Data Hotpath May Write

Future implementations may write local data such as:

- local indexes
- scan metadata
- report files requested by the user
- cache files needed to make repeated scans faster

Local writes should be documented, deterministic where practical, and avoid
including host-specific absolute paths in portable output unless the user asks
for them.

## Network And Cloud Boundaries

Primary Hotpath workflows should not make network calls. Scanning, scoring,
indexing, and report generation should not depend on cloud APIs.

If future optional features ever need network access, they should be explicit,
documented, opt-in, and separate from the core local workflow. They should not
change the default no-telemetry posture.

## Telemetry

Hotpath should not collect telemetry by default. The project should not add
silent analytics, background reporting, usage tracking, or automatic upload of
repository contents or derived metrics.

## User Responsibility

Hotpath is intended to help users inspect their own repositories locally. Users
remain responsible for deciding where to store generated reports and whether
those reports contain sensitive repository information.
