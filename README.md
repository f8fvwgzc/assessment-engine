# assessment-engine

**Change-impact assessment for versioned work items, in Rust, over gRPC.**

Linear-style trackers (and nexc-engine's issues) have projects, issues and
sub-issues — but no versions. When someone rewrites a parent issue's
description while a sub-issue is already in progress, nobody can say how big
that change is, which sub-issues it touches, or whether the plan still holds.

This engine answers that, for *any* change to *any* field:

| You change… | The engine tells you… |
|---|---|
| title, description (Markdown, acceptance criteria) | how different it is (TF-IDF, shingles, edit distance, embeddings, list-item/negation/number/modal diffs, typo-tolerant) and which children the changed sentences are about |
| estimate | the log-ratio change and the share of the parent's effort it moves |
| status | the transition risk (reopen, cancel, regress…) and who is orphaned |
| priority, assignee, due/start date, labels, project, cycle, parent, blocking links | type-specific magnitudes, schedule compression, date conflicts, capacity effects |
| a comment | blockers, scope changes, risks, schedule slips, questions (no version change) |

…and for each change it returns:

* a **semantic version bump** — MAJOR (breaks plans made against it), MINOR
  (compatible additions), PATCH (cosmetic) — so every entity is versioned, and
  children that *pinned* the parent's version learn their plan is now
  **incompatible** (like a Cargo dependency);
* a **ranked list of affected entities** (parent, children, siblings, blocked
  and blocking items) with a score, a level and an **action** each: *update
  scope*, *obsolete candidate*, *re-estimate*, *reschedule*, *cancel or
  rehome*, *re-verify*, *re-prioritize*, *review*, *notify*;
* the **plan conflicts** the change introduced (orphaned work, child due after
  parent, blocker due too late, estimate overflow, blocking cycles, …);
* a **re-plan score**, a level (none / low / medium / high / critical) and a
  **recommendation** — *no action*, *notify*, *review*, *partial re-plan*,
  *full re-plan* — with the subtree to re-plan, a confidence and the reasons.

The math is documented in [docs/SCORING.md](docs/SCORING.md).

```text
$ cargo run -p assessment-core --example nexc_scenario
critical impact (score 0.83, confidence 0.90); breaking change to "Authentication for the customer portal";
5 other item(s) affected, most: "Email and password sign-up form" (high, obsolete_candidate);
"Password reset email flow" (high, obsolete_candidate); "Session expiry and refresh" (high, update_scope);
recommendation: re-plan everything under "Authentication for the customer portal"
```

## Install

The server, as a container image (linux/amd64 and linux/arm64, signed build
provenance):

```bash
docker run --rm -p 50061:50061 ghcr.io/f8fvwgzc/assessment-engine:0.1.0
```

```yaml
# docker-compose.yml
services:
  assessment-engine:
    image: ghcr.io/f8fvwgzc/assessment-engine:0.1.0
    environment:
      ASSESSMENT_AUTH_TOKEN: ${ASSESSMENT_AUTH_TOKEN:-}
    healthcheck:
      test: ["CMD", "assessment-engine", "health"]
```

The Rust crates, by release tag (they are not on crates.io):

```toml
[dependencies]
# A typed gRPC client over the core types (and the tonic service, to serve it yourself).
assessment-grpc = { git = "https://github.com/f8fvwgzc/assessment-engine", tag = "v0.1.0", default-features = false, features = ["client"] }
# The engine itself, in-process.
assessment-core = { git = "https://github.com/f8fvwgzc/assessment-engine", tag = "v0.1.0" }
```

Any other language: generate a client from
[`proto/assessment/v1/assessment.proto`](proto/assessment/v1/assessment.proto).

## Workspace

| Crate | What | Use it when |
|---|---|---|
| [`assessment-core`](crates/assessment-core) | The engine: pure Rust, no I/O except the optional registry journal. `Engine`, `Registry`, `Policy`. | You are in Rust and want it in-process. |
| [`assessment-grpc`](crates/assessment-grpc) | Generated protobuf types, conversions, the tonic service, and a typed client. | You serve or call it over gRPC from Rust. |
| [`assessment-server`](crates/assessment-server) | The `assessment-engine` binary: gRPC server (health, reflection, auth, limits, graceful shutdown) and an offline CLI. | You run it as a service for any language. |
| [`proto/assessment/v1/assessment.proto`](proto/assessment/v1/assessment.proto) | The contract. | You generate a client in Go, TypeScript, Python, Java, C#… |

## Quick start

### As a service

```bash
cargo run --release -p assessment-server -- serve --addr 127.0.0.1:50061
```

Options (flags or env): `--journal <file>` / `ASSESSMENT_JOURNAL` (persist the
registry as an append-only JSON-lines journal; in memory otherwise),
`--fsync`, `--compact-after-ops <n>` / `ASSESSMENT_COMPACT_AFTER_OPS` (compact
the journal into `<file>.snapshot` after that many operations and on a clean
shutdown; default 100000, 0 = never), `--history-limit <n>` /
`ASSESSMENT_HISTORY_LIMIT` (keep the content of only the newest `n` versions
of each entity; default all), `--policy <patch.json>`, `--auth-token` / `ASSESSMENT_AUTH_TOKEN`
(require `authorization: Bearer <token>`), `--max-message-bytes`,
`--request-timeout-secs`, `--concurrency-limit`,
`--max-concurrent-assessments` (stateless assessments run at once, and
separately registry calls run at once; at least 1, default the number of
CPUs; more wait their turn), `--log-format json`, `RUST_LOG`. Health (`grpc.health.v1`) and reflection are served, so
`grpcurl` works out of the box:

```bash
grpcurl -plaintext -d @ 127.0.0.1:50061 assessment.v1.AssessmentEngine/Assess < examples/requests/parent_rewrite.json
```

#### Durability

Without `--journal` the registry lives in memory and is empty after a
restart. With it, the state on disk is two files: the journal and, after the
first compaction, `<file>.snapshot` next to it.

* **Journal.** Every acknowledged write is appended as one JSON line. On
  start the server loads the snapshot, if there is one, and replays the
  journal over it. A torn last line from a crash is dropped.
* **Snapshot.** A compaction writes the full state to the snapshot and
  starts a new journal, so a start does not replay the whole history. It
  runs once the journal holds `--compact-after-ops` operations (default
  100000) and on a clean shutdown. With `--compact-after-ops 0` the server
  never writes a snapshot and the journal keeps growing. The snapshot has a
  checksum: a corrupt or cut-off snapshot stops the start, and is never
  loaded in part.
* **`--fsync` is off by default.** A write is then handed to the operating
  system before the call is answered, but not forced to disk. It survives a
  crash or a kill of the server. A power cut or a crash of the machine can
  lose the last writes. With `--fsync` every write is synced to disk before
  it is answered, which is slower; an `Import` then syncs once per chunk,
  not once per item. Snapshots are always synced, whatever the flag says.
* **`--history-limit <n>`** keeps the content of only the newest `n`
  versions of each entity, to bound memory. Older versions stay in the
  history with their version and scores, but `GetEntity` and
  `CompareVersions` no longer answer for them. A snapshot holds only what
  was in memory, so a limit that was in force at a compaction is permanent
  for the versions it dropped.

An older server (0.1, 0.2) cannot go back to a compacted journal: it
refuses to start on it. On a journal that was never compacted it starts,
ignores every `source_seq`, and may drop a last `seq` line; see the
changelog.

Back up or copy the journal and its snapshot together. If the engine is fed
from a system of record, its state can also be rebuilt from an empty
journal with `Import`.

### Offline CLI (same engine, JSON in / JSON out)

```bash
cargo run -p assessment-server -- assess examples/requests/parent_rewrite.core.json
cargo run -p assessment-server -- policy                 # every tunable number
cargo run -p assessment-server -- compare-text --before "must support SSO" --after "must not support SSO"
```

### As a Rust library

```toml
[dependencies]
assessment-core = { git = "https://github.com/f8fvwgzc/assessment-engine" }
```

```rust
use assessment_core::prelude::*;

let before = Entity::new("ENG-12", "issue")
    .with_title("Authentication")
    .with_description("- [ ] Email and password login\n- [ ] Password reset by email")
    .with_status(StatusCategory::Started, "In Progress")
    .with_version(SemVer::new(1, 2, 0));
let after = before.clone().with_description("- [ ] Google OAuth login only");
let child = Entity::new("ENG-14", "sub_issue")
    .with_parent("ENG-12")
    .with_title("Password reset email flow")
    .with_status(StatusCategory::Unstarted, "Todo")
    .with_basis("ENG-12", SemVer::new(1, 2, 0));

let a = Engine::default().assess(&AssessmentRequest {
    changes: vec![ChangeSet::update(before, after)],
    context: vec![child],
    ..Default::default()
})?;
assert_eq!(a.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
for impact in &a.impacts {
    println!("{} {:?} {:.2} {:?}", impact.entity_id, impact.level, impact.score, impact.action);
}
```

Stateful, with versions and history kept for you:

```rust
let registry = Registry::new(Engine::default());          // or Registry::open(engine, "journal.jsonl", true)?
registry.put(parent, PutOptions::default())?;             // v1.0.0
registry.put(child, PutOptions::default())?;              // pins the parent's 1.0.0
let out = registry.put(rewritten_parent, PutOptions::default())?;  // assessed, v2.0.0
let stale = registry.stale(Some("ENG-12"))?;              // children planned against 1.x
registry.acknowledge("ENG-14", None)?;                    // re-planned: re-pin to 2.0.0
```

The registry assigns versions itself (a version you send is kept only the
first time an id is recorded, so you can import already-versioned items),
pins children and blocked items to what they depend on — including when the
parent arrives after the child — and keeps history in memory, bounded with
`Registry::with_history_limit(n)` / `open_with_history_limit`. The journal is
append-only JSON lines; a torn last line from a crash is truncated on open.

To follow a system of record, load with `Registry::import(items)`: new
entities are stored at their first version without an assessment, unchanged
ones are skipped and changed ones are assessed like a put. It returns an
`ImportSummary` of counts. `PutOptions::source_seq`,
`DeleteOptions::source_seq` (with `Registry::delete_with`) and
`ImportItem::with_source_seq` carry the source's sequence for the entity; a
change that is not above the stored sequence is skipped. See
[`Import`](#loading-and-syncing-many-entities-import) for the rules.

The journal grows with every write. `Registry::compact()` writes the full
state to `<journal>.snapshot` and starts a fresh journal, so the next open
loads the snapshot and replays only what came after it. The snapshot has a
checksum; a corrupt or cut-off snapshot fails the open and is never loaded
in part. `Registry::open_with` takes `RegistryOptions { fsync, history_limit,
compact_after_ops }`; with `compact_after_ops` the registry compacts on its
own after that many journal operations. `fsync` is off by default: a write
then survives a crash of the process, and a power cut can lose the last
writes. Snapshots are always synced. Keep the journal and its snapshot
together when you copy or back them up.

The registry keeps the text analysis of each stored entity in memory, so a
put analyses only the text that changed. Results are the same with and
without it. `Registry::with_analysis_cache(false)` turns it off.

### From any language over gRPC

Generate a client from `proto/assessment/v1/assessment.proto` (`buf generate`,
`protoc`, `grpc-tools`…) and call `Assess` (stateless: send the change and its
neighbourhood) or `PutEntity` (stateful: the server diffs against what it
stored). Your service then branches on `assessment.recommendation`,
`impacts[].action` and `changes[].bump`.

### Loading and syncing many entities: `Import`

`PutEntity` is for one live change: it assesses the change and answers with
the full assessment. `Import` is for many entities at once: a first load
from your system of record, a re-sync after an outage, or a replay of an
outbox. It is a client stream of `ImportRequest` items and answers once,
with counts instead of assessments. If you need the assessment of each
change, use `PutEntity`.

```bash
grpcurl -plaintext -d @ 127.0.0.1:50061 assessment.v1.AssessmentEngine/Import <<'EOF'
{"entity": {"id": "ENG-12", "kind": "issue", "fields": {"title": {"text": "Authentication for the customer portal"}}}, "sourceSeq": 41}
{"entity": {"id": "ENG-14", "kind": "sub_issue", "parentId": "ENG-12", "fields": {"title": {"text": "Password reset email flow"}}}, "sourceSeq": 7}
{"entity": {"id": "ENG-9"}, "deleted": true, "sourceSeq": 3}
EOF
```

```json
{ "created": "2", "unchanged": "1" }
```

The server handles each item by what it holds for the item's id:

| The item is… | The server… | Counted in |
|---|---|---|
| new | stores it at its first version (1.0.0, or the `version` it carries) **without an assessment**, and sets the pins a put would set | `created` |
| the same as what is stored | does nothing | `unchanged` |
| different from what is stored | assesses and versions it like `PutEntity` | `assessed` |
| `deleted`, and stored | deletes it like `DeleteEntity` | `deleted` |
| `deleted`, and not stored | does nothing | `unchanged` |
| older than what is stored (see `source_seq`) | does nothing | `skipped_stale` |
| not valid (no entity, an empty id, a bad value) | skips it and goes on | `failed`, `failed_total` |

`failed` lists the id and the error of at most the first 1000 failed items;
`failed_total` counts all of them. A listed id is cut to 256 bytes and an
error to 1024 bytes (a cut text ends in `...`), so the list stays small.
Only a message that cannot be decoded or
a journal that cannot be written fails the whole call.

**Ordering.** Items are applied in the order you send them, so two items
for the same id end in the state of the later one. Any order of entities
gives the same versions and pins: a child that arrives before its parent is
pinned when the parent arrives. Parents before children is the cheapest
order. A new entity is not assessed, so its history entry has score 0 and
no changed fields.

**`source_seq`.** Set it on `Import` items, `PutEntity` and `DeleteEntity`
when your source numbers its changes per entity (an outbox id, an update
time in microseconds). The server keeps the highest sequence it has applied
for each id, also after a deletion.

* A change whose sequence is not above the stored one is skipped. Nothing
  is assessed or stored, and `skipped_stale` is set (`PutEntityResponse`,
  `DeleteEntityResponse`) or counted (`ImportResponse`). So a replay, a
  duplicate delivery or a late delivery cannot overwrite newer state.
* A deletion of an id that is not stored still stores its sequence.
  `DeleteEntity` answers `NOT_FOUND` and `Import` counts it in `unchanged`,
  and a put with a lower sequence that arrives later is skipped. The same
  deletion sent again is then a skip like any other (`skipped_stale`, not
  `NOT_FOUND`). A dry run stores no sequence.
* A skipped `PutEntity` returns the stored item in `entity`. If the id is
  not stored (deleted, or only its sequence is known), it returns the item
  as sent: the server does not hold it.
* An equal sequence is also skipped. A replay of a stream with the same
  sequences is counted in `skipped_stale`, not in `unchanged`.
* A change without a sequence is always applied, and the stored sequence
  stays as it was.
* The check comes first: a stale `PutEntity` is skipped even when its
  `expected_version` does not match, and on a dry run.

A `PutEntity` that matches what is stored is not assessed either:
`unchanged` is set. For `unchanged` and `skipped_stale` the `assessment` in
the answer is empty (no changes, no impacts, score 0).

**Chunks, timeout and retries.** The server reads the stream in chunks of
up to 512 items or 4 MiB and applies each chunk as one registry call. Other
calls are served between chunks, and one import holds one chunk in memory.
The whole stream runs within `--request-timeout-secs` (30 by default);
raise it, or set 0, for a long import. If the call ends early (the client
goes away, the deadline passes), no further chunk starts and the chunks
already applied stay applied. Sending the same stream again is safe.

From Rust, `AssessmentClient::import_iter(items)` and `import(stream)` send
`ImportItem`s and return an `ImportReport`; in-process,
`Registry::import(items)` returns an `ImportSummary` with the same counts.

## Wiring it into nexc-engine

| nexc | engine |
|---|---|
| `issues.id` / `projects.id` | `Entity.id`, `kind` = `issue` / `sub_issue` (has `parent_id`) / `project` |
| `issues.parent_id` | `Entity.parent_id` (an issue's parent issue; issues without one use their `project_id`) |
| `title`, `description` | `title`, `description` (text) |
| `state_id` → `issue_states.category`, `.name` | `status` = `Status { category, name }` |
| `priority` 0 none, 1 urgent, 2 high, 3 medium, 4 low | `priority` = `Ordinal { rank: [0,4,3,2,1][p], scale: 4 }` |
| estimate (points) | `estimate` (number) |
| `assignee_id`, `project_id`, `cycle_id`, `team_id` | `assignee`, `project`, `cycle`, `team` (reference) |
| `due_date` | `due_date` (timestamp; a plain `YYYY-MM-DD` is accepted as 00:00 UTC in JSON) |
| `issue_labels` | `labels` (set of label names) |
| `issue_events` kind `state` / `priority` / `assignee` / `title` / `due` | `FieldChange { field, before, after }` (delta mode) — or send before/after snapshots |
| `issue_events` kind `comment` | `ActivityEvent { body }` |

A good integration point is the code path that writes an `issue_events` row:
call `PutEntity` (or `Assess` with the issue, its parent chain, children,
siblings and blockers as context), store `changes[0].next_version` on the
issue, and if `recommendation ≥ REVIEW` notify the owners of `impacts[]`
with their `action`; on `PARTIAL_REPLAN`/`FULL_REPLAN`, open a re-plan for
`replan_root` (for example, by asking the Planner to re-plan that subtree).

## Tuning

Every weight, threshold and lexicon entry is in `Policy`. Override per
server (`--policy patch.json`) or per request (`policy_overrides`, a partial
JSON object merged over the server's), for example:

```json
{
  "fields": { "story_points": { "severity": 0.6, "up": 0.8 } },
  "thresholds": { "high": 0.45 },
  "signals": [{ "category": "blocker", "weight": 0.6, "terms": ["хүлээгдэж байна"] }]
}
```

## Development

```bash
cargo test --all --workspace          # unit, scenario, property and gRPC tests
cargo clippy --all --workspace --all-targets
cargo run -p assessment-core --example nexc_scenario
```

Rust 1.88+ (the toolchain file pins 1.99). The protos compile with
[`protox`](https://crates.io/crates/protox), so no `protoc` is needed.

This workspace uses [Symposium](https://github.com/symposium-dev/symposium)
(`cargo agents`) to give AI coding agents crate-specific skills: run
`cargo agents sync` after changing dependencies to review the plugins they
bring.

## License

MIT
