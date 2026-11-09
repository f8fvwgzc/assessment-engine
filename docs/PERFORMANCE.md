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
rewrite the changed issue against the stored graph.

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

## Targets

- **M3 (ingest).** Import stores without assessing, and analyses are cached
  per entity version. Expected: bulk load is linear (~1k entities in well under
  a second), and a put on the stored graph analyses only the changed entity's
  text.
