# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Registry as an ingest target (core):
  - A put that matches the stored entity is answered before the engine runs.
    `PutOutcome::unchanged` reports it. The returned assessment is then an
    empty placeholder. A dry run and a put with activity events are still
    assessed.
  - `PutOptions::source_seq` and `DeleteOptions::source_seq` carry the
    source's sequence for the entity. A put or delete whose sequence is not
    above the stored one is skipped: nothing is assessed or stored, and
    `skipped_stale` is set on the outcome. A put without a sequence behaves
    as before and keeps the stored sequence.
  - `Registry::delete_with(id, DeleteOptions) -> DeleteOutcome`.
    `Registry::delete` is unchanged. A deletion with a sequence of an id
    that is not stored fails with "not found" and keeps the sequence, so an
    older put that arrives later is skipped. Keeping the sequence is a
    journal write, and the same deletion sent again is answered with
    `skipped_stale` instead of "not found".
  - `Registry::import` / `import_with` apply a batch of `ImportItem`s in
    order and return an `ImportSummary`. New entities are stored without an
    assessment, unchanged ones are skipped, changed ones are assessed like a
    put, and deletions are applied. A failed item is reported and the rest
    still runs. The journal is synced once per import. The items may
    come from a lazy iterator: it is read up to its first `None`, in runs
    of up to 1024 items with no lock held. The registry lock is free
    between two runs, so other callers can get in; the lock is not fair, so
    under load they may wait for more than one run.
  - A new entity gets its first version, its pins and its history entry
    from an import, as from a put. Its history entry has score 0, level
    `none` and no changed fields, because nothing was assessed. It passes
    the same checks as a put (finite numbers, size limits); an entity that
    fails them is listed in `failed`.
  - The journal stores sequences in an optional `source_seq` field and a new
    `seq` line. Journals written by 0.1 and 0.2 still load. 0.2 does not
    know either: see Compatibility.
- Registry analysis cache (core):
  - The registry keeps the term counts of every stored entity's text and
    uses them for the context of later assessments. An entity's text is
    analysed when an assessment first reads it, and again only when its
    title or description changes. The counts of a deleted entity are
    dropped. An import analyses nothing; the first assessment after it
    does.
  - Results do not change, bit for bit. Document frequencies are still
    computed per request over the same corpus.
  - The kept counts are not written to disk. They are built again, on first
    use, after `Registry::open`.
  - `Registry::with_analysis_cache(false)` turns the cache off, and
    `Registry::analysis_cache()` reports the setting. It is on by default.
  - The bench has rows for an import of 1k and 10k new entities and for a
    put with the cache off.
- Registry durability (core):
  - `Registry::compact()` writes the full state to `<journal>.snapshot` and
    starts a fresh journal. The snapshot is written to a temporary file,
    synced and renamed into place. Its header holds a format version, the
    entry count and an FNV-1a 64 checksum. It returns a `Compaction`, or
    `None` for an in-memory registry.
  - `Registry::open` loads the snapshot, if there is one, and replays the
    journal on top of it. A corrupt or cut-off snapshot fails with
    `ErrorKind::Storage`; part of a snapshot is never loaded. So does a
    journal whose snapshot is missing or belongs to another journal.
  - A crash at any point of a compaction leaves files that open to the
    acknowledged state. The registry module docs list each case.
  - `RegistryOptions { fsync, history_limit, compact_after_ops }` and
    `Registry::open_with`. `compact_after_ops` compacts after that many
    journal operations, on the thread of the write that reaches the count.
    A failed automatic compaction does not fail the write;
    `Registry::take_compaction_error` returns the reason.
  - `Registry::journal_ops` and `Registry::compact_after_ops`.
  - A journal that continues a snapshot starts with a `generation` line
    and a `note` line. Journals written by 0.1 and 0.2 still load. 0.1 and
    0.2 refuse to open a compacted journal (they report line 1 as corrupt)
    and leave it as it is.
  - Before `Registry::open` restarts a journal that its snapshot holds
    completely, it syncs the directory, so the snapshot is durable before
    the journal is cut.
- Ingest over gRPC (additive; existing clients keep working):
  - `rpc Import(stream ImportRequest) returns (ImportResponse)`. The server
    applies the stream in order, in chunks of
    `ServiceLimits::max_import_chunk` items (default 512) or 4 MiB of
    messages. Each chunk is one registry call under the registry slot pool,
    so other calls are served between chunks and one import holds one chunk
    in memory. An item that is not valid is listed in `failed` with its id
    and the rest still runs. `failed` lists at most the first 1000;
    `failed_total` counts all. A listed id is cut to 256 bytes and an error
    to 1024 bytes. If the call is cancelled or passes the
    deadline, no further chunk starts and the applied chunks stay.
  - `PutEntityRequest.source_seq` and `DeleteEntityRequest.source_seq`;
    `PutEntityResponse.skipped_stale` and `.unchanged`;
    `DeleteEntityResponse.skipped_stale`. `DeleteEntityResponse.deleted` is
    now false for a delete that was skipped as stale.
  - Client: `AssessmentClient::import` (a stream) and `import_iter`, both
    returning an `ImportReport`; `delete_entity_with(id, DeleteOptions)`.
    `put_entity` sends `PutOptions::source_seq`.
  - `convert`: `delete_request_to_pb` / `delete_request_from_pb` and
    conversions for `DeleteOutcome`, `ImportItem`, `ImportFailure` and
    `ImportSummary`.
  - The service logs a failed automatic compaction.
- Server flags:
  - `--compact-after-ops` / `ASSESSMENT_COMPACT_AFTER_OPS` (default 100000,
    0 = never). The journal is compacted after that many operations and on
    a clean shutdown. With 0 the server never writes a snapshot.
  - `--history-limit` / `ASSESSMENT_HISTORY_LIMIT` (at least 1; default
    all) keeps the content of only that many newest versions per entity.
    The library already had this as `Registry::with_history_limit`.
  - The journal is opened on the blocking pool.
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

- A put that matches the stored entity is no longer assessed. Before, the
  engine ran and the answer held one change with no field changes. Now
  `PutOutcome::unchanged` (`PutEntityResponse.unchanged`) is set and the
  assessment is empty: `changes` has no entry, so do not read `changes[0]`
  without a check. `stored` is false and the version does not move, as
  before. A dry run and a put with activity events are still assessed.
- The storage error for a journal that a failed write left out of step now
  says "no longer matches the registry after a failed write; reopen the
  registry".
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

### Compatibility

- Assessment results are unchanged. Every golden output is byte-identical
  under the default policy.
- The gRPC contract only grows: one new RPC, new messages and new fields
  with new numbers. Nothing is renamed or renumbered. A 0.1 client keeps
  working. It cannot see `unchanged` or `skipped_stale`, and it gets an
  empty assessment for a duplicate put.
- Journals written by 0.1 and 0.2 load as they are. The new journal fields
  are optional and default when absent.
- Going back is limited, and not always refused.
  - After a compaction, 0.1 and 0.2 fail to open the journal ("line 1 is
    corrupt") and change nothing. The state is in the snapshot, which they
    cannot read.
  - Before the first compaction, 0.1 and 0.2 open the journal. They ignore
    every `source_seq` field. They fail on a `seq` line that other lines
    follow, but a `seq` line at the very end is dropped without an error,
    like a torn write. The ordering guard is then lost for the ids
    concerned: this version, opening the journal again, applies a change
    that it would have skipped as stale.
  - To keep a journal fully readable by 0.2, serve with
    `--compact-after-ops 0` (which also turns off the compaction on
    shutdown) and send no `source_seq`.
- Rust API: `PutOptions` and `PutOutcome` have new public fields. Code that
  builds them with a struct literal and no `..` needs the new fields.

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
