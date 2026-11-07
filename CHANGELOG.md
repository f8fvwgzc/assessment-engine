# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-08

First release.

### Added

- `assessment-core`: the change-impact engine. It assesses any change to a
  versioned work item (project, issue, sub-issue): a SemVer bump per entity,
  a ranked list of affected entities with an action each, the plan conflicts
  the change introduced, and a re-plan score, level and recommendation with
  the subtree to re-plan. Includes a registry with an optional append-only
  journal, and a tunable policy.
- `assessment-grpc`: the `assessment.v1` protobuf contract, conversions, the
  tonic service with bearer-token auth, and a typed client.
- `assessment-server`: the `assessment-engine` binary. It serves gRPC with
  health, reflection, limits and graceful shutdown, has an offline CLI
  (`assess`, `policy`, `compare-text`), and a `health` check for containers.
- Container image `ghcr.io/f8fvwgzc/assessment-engine` (linux/amd64,
  linux/arm64) with SBOM and signed build provenance.

[Unreleased]: https://github.com/f8fvwgzc/assessment-engine/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/f8fvwgzc/assessment-engine/releases/tag/v0.1.0
