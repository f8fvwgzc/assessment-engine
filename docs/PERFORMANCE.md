# Performance

How fast the engine is, where the time goes, and what each roadmap step is
expected to change. All numbers come from the bench example, which builds
deterministic synthetic graphs (`crates/assessment-core/tests/support/mod.rs`):

```bash
cargo run --release -p assessment-core --example bench             # full table
cargo run --release -p assessment-core --example bench -- --quick  # up to ~1k entities
```

Each row times `Engine::assess` on one request: median and minimum of 3–7 runs
after one warm-up. The JSON parse of the same request is timed separately. The
registry rows store a project tree put by put (timed once), then time puts that
rewrite the changed issue against the stored graph, with and without the
analysis cache. The import rows time one `Registry::import` of new entities
(timed once), the first put after it (timed once) and later puts. All registry
rows use an in-memory registry, so they hold no journal or disk time.

## Baseline: 0.1.0 (2026-11-10, Apple M2 Pro, 10 cores, release build)

| scenario | entities | JSON MB | parse ms | assess median ms | assess min ms |
|---|---:|---:|---:|---:|---:|
| siblings x100 (268 chars) | 106 | 0.06 | 0.2 | 3.3 | 3.3 |
| siblings x1000 (268 chars) | 1006 | 0.59 | 1.0 | 28.5 | 27.8 |
| siblings x5000 (268 chars) | 5006 | 2.92 | 5.4 | 144.2 | 139.9 |
| siblings x10000 (268 chars) | 10006 | 5.85 | 9.5 | 292.5 | 289.9 |
| siblings x1000 (2000 chars) | 1006 | 2.32 | 1.3 | 103.6 | 102.4 |
| cousins 2500x3 (268 chars) | 10006 | 5.91 | 9.6 | 287.1 | 280.7 |
| fan-out tree x10000 | 10006 | 5.91 | 8.9 | 279.2 | 263.8 |
| deep chain x10000 | 10006 | 5.92 | 9.3 | 260.6 | 259.8 |
| unrelated project x10000 | 10007 | 5.85 | 8.9 | 264.5 | 258.5 |
| blocking chain x200 | 206 | 0.13 | 0.2 | 6.0 | 6.0 |
| registry cousins 250x3: bulk load (put by put) | 1006 | — | — | 13947.1 | 13947.1 |
| registry cousins 250x3: put on the stored graph | 1006 | — | — | 28.7 | 28.0 |

What these numbers show:

- **The time does not depend on reach.** A context that the walk never
  reaches still costs about as much as one it reaches. Compare the 10k
  unrelated project (264 ms) and the 10k deep chain (261 ms, only a few links
  reached) with 10k siblings (292 ms). Every context entity's text is analysed
  on every request.
- **Text length dominates.** 1k siblings with 2 000-character descriptions
  take 3.6× as long as with 268 characters.
- **Parsing is cheap.** The JSON parse is 3–4 % of the assessment.
- **Bulk loading is quadratic.** Each put assesses against the stored
  neighbourhood, so loading 1k entities put by put takes ~14 s.

A profile of the debug build (three independent measurements, 10k siblings)
broke one assessment down as:

- **Text work: ~82 %.**
  - ~25 % is rebuilding the 137-word lexicon on every `text::analyze` call.
  - ~57 % is tokenising, stemming and TF-IDF for every context entity.
- **~11 %:** propagation, violation checks and impacts.
- **3–5 %:** JSON parse.
- **~2 %:** validation, clones and graph builds.

## M1: hot path (2026-11-10, same machine and build)

What changed:

- **Lexicon built once per assessment.** One word lexicon is shared by
  context documents, titles, changed fields, typo alignment and activity
  text, instead of one per text.
- **Text work limited to what is read.** Only entities within propagation's
  reach get document vectors. Context documents and titles are counted, not
  fully analysed: no passages, segments or list items.
- **Registry assesses outside its lock.** Puts, deletions, activity and
  version comparisons gather their inputs under the shared lock, assess with
  no lock, and commit under the exclusive lock. If the entity changed in
  between (another put or deletion, an acknowledge, or a pin backfilled by a
  new neighbour), the commit gathers and assesses again under the exclusive
  lock.
- **Server-wide caps.** `--max-concurrent-assessments` sizes two slot pools:
  one for stateless assessments and one for registry calls. So up to twice
  that many CPU jobs can run at once.

`text.idf_scope` (policy) picks which texts make up the IDF corpus:

- **`context`** (the default): every entity in the request, exactly as in
  0.1.0. Every golden output is byte-identical to 0.1.0. Unreached context is
  still counted for its document frequencies.
- **`reach`**: only the changed entities and everything within
  `propagation.max_depth` hops along propagation's channels, including a
  moved entity's former siblings. Text outside that reach is never analysed
  and is not in the IDF corpus. Context linked to the changes still counts
  toward breadth (structural dependents) and violation checks, even outside
  the reach. Only context with no link to the changes leaves the result
  unchanged. On the
  synthetic graphs, relevance to reached entities rose by up to 0.08, and
  three more low-level impacts crossed the reporting floor. Level,
  recommendation, bumps, violations and the other actions were unchanged.

| scenario | 0.1.0 ms | M1, `context` ms | M1, `reach` ms |
|---|---:|---:|---:|
| siblings x100 (268 chars) | 3.3 | 3.5 | 3.3 |
| siblings x1000 (268 chars) | 28.5 | 16.8 | 17.1 |
| siblings x5000 (268 chars) | 144.2 | 85.9 | 87.9 |
| siblings x10000 (268 chars) | 292.5 | 178.2 | 176.5 |
| siblings x1000 (2000 chars) | 103.6 | 55.5 | 54.4 |
| cousins 2500x3 (268 chars) | 287.1 | 134.2 | 65.1 |
| fan-out tree x10000 | 279.2 | 114.0 | 20.0 |
| deep chain x10000 | 260.6 | 105.6 | 12.1 |
| unrelated project x10000 | 264.5 | 105.4 | 11.3 |
| blocking chain x200 | 6.0 | 3.8 | 3.8 |
| registry cousins 250x3: bulk load (put by put) | 13 947 | 6 492 | 2 107 |
| registry cousins 250x3: put on the stored graph | 28.7 | 14.1 | 7.4 |

How the gains break down:

- **Fully reached graphs** (siblings): 1.6–1.9× faster. This comes from the
  lexicon and counts-only analysis.
- **Mostly unreached context** (cousins, deep chain, unrelated, fan-out
  beyond 4 levels): 2–2.6× faster with `context`, and 4–23× faster with
  `reach`.
- **Registry puts:** 2× faster with `context` and 3.9× with `reach`. Bulk
  loading is still quadratic until M3.

Measured and dropped: building impact reasons and contributions only for the
returned `top_k` impacts. Changing `top_k` (0 / 100 / 5) and `explain` moved
no timing beyond noise, even with 346 impacts, so the engine still builds them
for every impact.

## M3: ingest and analysis cache (2026-11-10, same machine and build, `context`)

What changed:

- **Import stores without assessing.** `Registry::import` (the `Import` RPC)
  gives a new entity its first version, its pins and its history entry
  without running the engine. Only an item that changes a stored entity, or
  deletes one, is assessed.
- **Duplicates are answered before the engine.** A put or an import item
  that matches the stored entity is compared and skipped. So is one whose
  `source_seq` is not above the stored sequence.
- **Analysis cache.** The registry keeps the term counts of each stored
  entity's title and description in memory. They are computed when an
  assessment first reads the entity, and again only when that text changes.
  They are dropped when the entity is deleted.
  An import analyses nothing; the first put after it analyses the texts it
  reads, once.
- **Snapshots.** A compaction folds the journal into a snapshot, so a start
  replays only what was written after it. This is not in the bench: the
  registry rows run in memory.

| scenario | entities | M1, `context` ms | M3 median ms | M3 min ms |
|---|---:|---:|---:|---:|
| registry cousins 250x3: bulk load (put by put) | 1006 | 6 492 | 2 757.7 | 2 757.7 |
| registry cousins 250x3: put on the stored graph | 1006 | 14.1 | 7.9 | 6.9 |
| registry cousins 250x3: put on the stored graph, analysis cache off | 1006 | — | 14.2 | 14.0 |
| registry: import 1k new entities | 1006 | — | 1.0 | 1.0 |
| registry: first put after the 1k import | 1006 | — | 17.7 | 17.7 |
| registry: put after the 1k import | 1006 | — | 8.9 | 8.9 |
| registry: import 10k new entities | 10006 | — | 12.8 | 12.8 |
| registry: first put after the 10k import | 10006 | — | 190.1 | 190.1 |
| registry: put after the 10k import | 10006 | — | 93.8 | 93.6 |

The import rows load the `siblings` graph, where the put that follows
reaches every entity. A dash means the row did not exist in M1.

What the numbers show:

- **Import is linear.** 1k new entities take 1.0 ms and 10k take 12.8 ms,
  about 1 µs per entity. A put-by-put load of a 1k graph takes 2.8 s,
  because every put is assessed.
- **A put on a stored graph is about 2× faster.** 14.1 ms in M1 and 14.2 ms
  with the cache off, 7.9 ms with it on.
- **The first put after an import pays for the analysis once.** 17.7 ms on
  1k entities and 190.1 ms on 10k; later puts take 8.9 ms and 93.8 ms.
- **A put-by-put load is 2.4× faster than in M1,** but it is still not
  linear. Use an import to load.

What did not change:

- **Assessment results.** Every golden output is byte-identical to M2. Only
  term counts are kept; document frequencies and TF-IDF are computed per
  request over the same corpus as before.
- **Stateless `Engine::assess`.** The rows above the registry rows in the
  bench table are within noise of M1 (for example 10k siblings: 178.2 ms in
  M1, 174.9 ms now).
- **The per-request work on a large reached graph.** A put on a fully
  reached graph of 10k entities still takes about 94 ms. The analysis is
  gone from it; what is left is document frequencies, TF-IDF vectors,
  similarity and propagation over every reached node.
- **`reach` scope** was not measured again for M3.

## Targets

- **M3 (ingest): measured.** The targets were a linear bulk load, with 1k
  entities in well under a second, and a put that analyses only the changed
  entity's text. An import of 1k entities takes 1.0 ms and one of 10k takes
  12.8 ms. A put on the stored 1k graph went from 14.1 ms to 7.9 ms. A unit
  test checks that such a put analyses the same few texts (at most 12) for
  10 and for 300 stored children.
