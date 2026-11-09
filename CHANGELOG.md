# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `text.idf_scope` policy setting:
  - `context` is the default and the 0.1.0 behaviour: the IDF corpus is every text in the request.
  - `reach` limits the corpus to the changed entities and what propagation
    can reach. Text outside that reach is not analysed. Context with no link
    to the changes then costs nothing and cannot move a score. Context linked
    to the changes still counts toward breadth and violation checks.
- `ServiceLimits::max_concurrent_assessments` and the server flag
  `--max-concurrent-assessments` / `ASSESSMENT_MAX_CONCURRENT_ASSESSMENTS` set
  the size of two slot pools:
  - stateless work: Assess, CompareText, and each batch or stream item;
  - registry calls: PutEntity, DeleteEntity, RecordActivity, CompareVersions.

  The default is the number of CPUs. The flag must be at least 1, and the
  server rejects 0. `with_limits` brings out-of-range limits into range.
- Calls beyond the cap wait for a slot:
  - A unary call or a batch waits within the request timeout and then fails
    with `CANCELLED`.
  - A stream item waits with no server-side deadline.
  - Work that has started runs to completion and holds its slot until then. A
    registry write that has started still commits.
- A batch or stream queues up to `max_in_flight` items for slots at once.
- `crates/assessment-core/examples/bench.rs` (performance table, see
  `docs/PERFORMANCE.md`) and golden tests that pin full outputs
  (`tests/golden.rs`, `GOLDEN_BLESS=1` to rewrite).

### Changed

- Faster assessments with identical results (every 0.1.0 golden output is
  byte-identical under the default policy):
  - The word lexicon is built once per assessment and once per
    `compare_text` call, not once per text.
  - Context documents and titles are counted rather than fully analysed.
  - Only entities propagation can reach get document vectors.
  - Release build: 1.6–2.6× faster depending on the graph (`docs/PERFORMANCE.md`).
- The registry gathers a put's, deletion's, activity's or comparison's inputs
  under its lock and assesses without it. Reads and other puts no longer wait
  for a long assessment, or for a writer queued behind one. One case is left:
  if the entity itself changed during the assessment, the commit gathers and
  assesses it again under the exclusive lock, as in 0.1.0, and reads and
  writes wait for that second assessment.
- Opposite English words are no longer read as typos of each other:
  `increase`→`decrease`, `internal`→`external`, `encryption`→`decryption`,
  `maximize`→`minimize`, `interstate`→`intrastate`, `micro…`→`macro…` now
  score as a replaced word (magnitude about 0.25 in a one-sentence text,
  was 0.02). Real typos (`recieve`, `enviroment`, `deploymnet`) still align.

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
