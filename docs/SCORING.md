# How the engine scores a change

Every number below is a default from `Policy` (`assessment-engine policy`
prints them all; any of them can be overridden per server or per request).
All scores live in `[0, 1]` and read as *"how likely is it that this needs
someone's attention"*.

## 0. Four operators

| Operator | Formula | Why |
|---|---|---|
| **noisy-OR** | `1 − Π(1 − pᵢ)` | Combines independent evidence: one strong cause dominates, many weak causes accumulate, never exceeds 1. |
| **amplify** | `1 − (1 − s)^γ` | Scales a score by a context factor γ without leaving `[0,1]`; ≈ `γ·s` for small `s`, fixes 0 and 1. |
| **saturate** | `1 − e^{−x/scale}` | Maps an unbounded measure (a ratio, a count) to `[0,1)` with diminishing returns; ≈ 0.63 at `x = scale`. |
| **escalate** | `1 − (1 − base)·(1 − extra)^w` | Raises `base` by a share of `extra`; never below `base`, monotone in both. |

## 1. Field magnitude `m ∈ [0,1]` — how different the two values are

| Type | Magnitude |
|---|---|
| **Number** (estimate) | `saturate(|ln((new+1)/(old+1))|, ln 2)` — doubling ≈ 0.63, ×3 ≈ 0.79; decreases × 0.7. Negative values use the relative change. Set → 0.3, cleared → 0.5. |
| **Status** | Transition-risk matrix `T[from][to]` over backlog / unstarted / started / completed / canceled, e.g. started→canceled 0.8, completed→started (reopen) 0.6, todo→in-progress 0.05; same category, other name 0.05. |
| **Ordinal** (priority) | `|Δrank|/scale`, × 0.6 when de-escalating. The scale must be positive and at least the rank (otherwise the request is invalid). |
| **Timestamp** (due/start) | Compression ratio `ρ = |shift| / window`, `window = max(days left before the old date, 14)`. The shift is the part of the move beyond today (an already missed date counts from today). Pulled in: `saturate(ρ, 1/2)`; pushed out: `0.8 · saturate(ρ, 1)`. Both dates already past: `0.5 ×` the push-out of the whole move (bookkeeping on overdue work). Set 0.15, cleared 0.3. |
| **Reference** (assignee, project, cycle) | reassigned 1.0, assigned 0.25, unassigned 0.6. |
| **Set** (labels) | Jaccard distance; adding or removing a *significant* label (`security`, `breaking-change`, `breaking`, `urgent`, `compliance`), or adding a resolution-type one (`blocked`, `regression`, `incident` — removing those is good news and scored by Jaccard) → 1.0 with severity ≥ 0.5. |
| **Text** | See §2. |
| Type changed / bool flipped | 1.0 |

Synthetic fields: `parent` (re-parenting: moved 1.0, attached 0.5, detached
0.6), `relations` (blocking/related links, as a set), `lifecycle`
(created / deleted), `activity` (comments, §3).

**Breaking** when `m ≥` the field's `breaking_magnitude` (title 0.6,
description 0.5, estimate 0.63, status 0.6, priority 0.75, due date 0.7,
start date 0.8, project 0.9, parent 0.9; never for the others) — and only
for fields whose `max_bump` is MAJOR. The assignee, cycle, labels and team
are capped at MINOR: they never make a change breaking.

## 2. Text

Both texts are analyzed once: Markdown structure (list items, checkboxes,
headings), sentences, lowercase Unicode tokens (`n't` → `not`, `10,000`
and `1.5` kept whole), English stop-words removed, a light suffix stemmer
(`blocked`/`blocking` → `block`), negations and modal verbs read separately.

**Typo alignment.** A word only in the new text that is a likely misspelling
of a word only in the old one (optimal-string-alignment similarity ≥ 0.75
for 4+ letters, or a swap of two adjacent letters in 3-letter words:
`teh` → `the`) is read as that word, so fixing a typo moves only the
character-level distance. Never aligned: `login`→`logout`,
`import`→`export`, `enable`→`disable`; tokens containing a digit
(`200ms`→`500ms`, `oauth1`→`oauth2`, `v2.0`→`v3.0` are real changes); a
one-letter insertion or deletion on a word of ≤ 5 letters (`http`→`https`,
`sync`→`async`); tokens longer than 40 characters; and, for words of 6+
letters, a change of morpheme rather than a slip: another first letter
(`increase`→`decrease`, `internal`→`external`), one run of two or more
letters replaced (`maximize`→`minimize`, `interstate`→`intrastate`,
`hyperthermia`→`hypothermia`), or `micro`→`macro`. Two swapped adjacent
letters are still a typo anywhere in the word. The distance is a banded
OSA with early exit and a global cell budget, so alignment stays linear-ish
on huge inputs.

**Lexical similarity `L`** = `w_c·char + w_t·tfidf + w_s·shingle`, with the
weights moving linearly from `(0.4, 0.3, 0.3)` at ≤ 512 characters to
`(0, 0.6, 0.4)` at ≥ 1024 (no cliff at the boundary; character distance is
never computed above 2048 characters). A pure **reorder** — the same
sentences, list items (with their check state) and headings, in another order
or layout — caps the lexical distance at 0.05.

where `char = 1 − levenshtein / longer length`, `tfidf` is the cosine of
sublinear TF-IDF vectors (`(1 + ln tf) · idf`, `idf = ln((1+N)/(1+df)) + 1`
over **the two texts themselves**, so the field's magnitude — and the
version bump — does not depend on how much context the request carries;
`Engine::compare_text` returns the same number), and `shingle` is the
Jaccard similarity of word bigrams (word order).

**Base distance** `b = 1 − L`, or with embeddings on both sides
`0.6 · clamp((1 − cos)/(1 − 0.3)) + 0.4 · (1 − L)`.

**Structural evidence**, each `∈ [0,1]`:

* list items / acceptance criteria: `1 − e^{−0.5·(added + removed + ½·reworded + ½·headings±)}` (ticking a box is progress, not scope), weight 0.8;
* negation flips (`must support` → `must not support`): `1 − e^{−0.7·n}`, weight 0.8;
* requirement-strength shifts (`should` ↔ `must`): `1 − e^{−0.4·n}`, weight 0.5;
* changed numbers (`100 users` → `1000 users`): `1 − e^{−0.5·n}`, weight 0.7.

Negations, modal shifts and numbers are counted **only inside reworded
passages and items** (a sentence that exists on both sides), so a newly
added sentence that happens to contain "no", "without" or a number is an
addition, not a flip; negation and modal scopes stop at sentence ends.

`m = noisy-OR(b, weighted evidence…)` — noisy-OR already discounts each
piece of evidence by what the others explained, so `m` is monotone: more
evidence or lower similarity never lowers it.

**Breaking** when `m ≥ 0.5` (description) / `0.6` (title), or a list item
was removed, or a negation flipped — unless **nothing was lost**: no passage
or list item removed or reworded, no negation or requirement-strength change.
Such a change only adds (however large, a first description included) and
is capped at MINOR. **Additive** when items or headings were added, or
passages were added and nothing was lost (and nothing broke).

**Passages.** For propagation the change is also split into *passages*
(sentences and list items). A passage's identity includes its polarity and
requirement strength, so "must support SSO" → "must not support SSO" is a
*reworded* passage that points at the sub-issue implementing SSO. Unmatched
passages pair as reworded when their term Jaccard is ≥ 0.5, or — for short
items of ≤ 3 terms — when they share a term and their texts are ≥ 0.6 similar
character-wise; the rest are removed or added. Exact matching is hashed and
fuzzy pairing is bounded (top-k candidates through an inverted index), and
overlap queries use an inverted index too, so cost does not multiply with
the number of changed passages. Passages are weighted with an IDF over the
request's texts (shared vocabulary weighs little). `text.idf_scope` decides
which texts:

* `context` (the default) uses **every text in the request**, the changed
  entities and the whole context;
* `reach` uses only the changed entities and what propagation can reach
  (§5): everything within `max_depth` hops along its channels, whether or not
  this request's walk visits it. Text outside that reach is not analysed.
  Context linked to the changes still counts toward breadth and violation
  checks. Only context with no link to the changes cannot move a score.

Either way, only the texts within that reach get document vectors, because
no other text is ever read. Relevance to another entity uses the **best
single passage** (max-passage scoring), so a sub-issue about one changed
sentence is not diluted by the rest of a long rewrite (§5).

## 2a. Gray zone and judgments

Word counts cannot tell a paraphrase from a new requirement, or a database
swap from a typo fix. The engine marks such a change as being in the
**gray zone** and can use a **judgment** of it: a statement of how the two
texts relate. The engine never asks anyone. A judgment is data in the
request (`ChangeSet.judgments`). A caller can supply it, or a server with a
judge can (see [JUDGE.md](JUDGE.md)).

All thresholds are in `Policy.judge`. `judge.enabled = false` turns the
whole section off: nothing is flagged and supplied judgments are ignored.
The rules read English words. The texts may be in any language, but the
scope words and the negation words are English.

### Which changes are flagged

Only the fields in `judge.fields` (`title` and `description`) are looked at.
`bm` is the field's `breaking_magnitude` and `m` its magnitude from §2. The
reasons are checked in this order, and a field can have several:

| Reason | When |
| --- | --- |
| `negation_changed` | A negation word was added or dropped, the engine did not find a clear flip, and `m < bm`. |
| `scope_qualifier` | A scope word was added or dropped (`except`, `only`, `unless`, `at most`, `instead of`, `no longer`, …, the list is `judge.scope_qualifiers`), and `m < bm`. |
| `term_substitution` | Words were dropped and others added, at most `max_substituted_terms` (3) on each side, and `m < bm`. One word that became a near twin of itself (`increase` → `decrease`) does not count: §2 already handles it. |
| `paraphrase_suspected` | At least one passage was removed and at least one added, so the text may only be reworded. |
| `near_threshold` | `m` is in `[bm − 0.2, bm + 0.1)`, with no list or heading edit, no requirement-word shift and no changed number. |

The last three also need words to be lost: a change that only adds words is
never one of them.

Never flagged: a pure reorder, a text that is empty on one side, a text
longer than `judge.max_text_chars` (20 000 characters), a removed list item
and a clear negation flip (`must support` → `must not support`). The engine
is sure about the last two, and they are breaking already.

### Fingerprint

A judgment names the two texts it is about by a fingerprint, so an answer
for an older edit is never used for a newer one. The fingerprint is the
lowercase hex SHA-256 of

    {byte length of before}:{before},{byte length of after}:{after},

with the lengths in decimal and the texts as UTF-8, exactly as sent. Test
vectors:

| before | after | fingerprint |
| --- | --- | --- |
| (empty) | (empty) | `78f3ba77faa1ffc62bd2dd7cc8de66fb1c9112793b99729582731a64ee7ae8e4` |
| (empty) | `a` | `d476ca72caa7bbdc5b23666b11d6762fa47034529ba497ac557d123407fa8aae` |
| `a` | (empty) | `9388b92a75a2bd819468478a7f33321b47c40cf2837424349f3bea772a70c1ea` |
| `Build the login button` | `Build the logout button` | `2086096a222bb499be60b77fa3ca64d94bf16c2135817a0c5d141cc73e81c86f` |
| `Users can log in with their email address and password.` | `People are able to sign in using e-mail and a password.` | `a5092b8fdb1115a48435979471ef040eb3f920811cb6bfdca108c7393e2d56b5` |

The first input is the 6 bytes `0:,0:,`. The engine prints the fingerprint
of every flagged field, so a caller does not have to compute it.

### Relations and weight

| Relation | Meaning | Direction | Threshold `c0` |
| --- | --- | --- | --- |
| `equivalent` | Same requirement, other words. | lowers | 0.6 |
| `extended` | Everything before still holds, and more is asked. | lowers | 0.6 |
| `narrowed` | Something asked before is no longer asked, or is limited. | raises | 0.5 |
| `replaced` | One thing was swapped for another. | raises | 0.5 |
| `reversed` | The text now asks the opposite. | raises | 0.5 |
| `undetermined` | The judge could not tell. | none | — |

A judgment has a confidence `c ∈ [0,1]` (1 when not given). Its weight is

    w = clamp((c − c0) / (1 − c0))

and 0 for `undetermined`. Lowering needs more confidence than raising,
because a wrong "equivalent" hides a real change. A judgment is
**decisive** when `w ≥ 0.5`, that is `c ≥ 0.8` for a lowering relation and
`c ≥ 0.75` for a raising one.

### Status

Every flagged field, and every field a judgment names, gets one status:

| Status | Meaning |
| --- | --- |
| `requested` | Flagged, no judgment given. It is listed in `judgment_requests`. |
| `over_budget` | Flagged, but ranked past the first `judge.max_requests` (16) flagged fields of this request. Supplying judgments does not free a place: send the fields in a smaller request to have them judged. |
| `applied` | A judgment with `w > 0` was used. |
| `inconclusive` | A judgment was given, but `w = 0`. |
| `stale` | The fingerprint does not match the two texts. The judgment is ignored. A server with a judge asks about the field instead, so there the status becomes `applied`, `inconclusive` or `unavailable`. |
| `not_flagged` | A judgment for a field the engine did not flag. It is ignored unless `judge.apply_unflagged` is set. |
| `unavailable` | Set by a server only: its judge did not answer. `detail` says why. |

Requests are listed in a fixed order: by first reason (the order of the
table above), then by `severity × m` from high to low, then by entity id
and field. With `options.judgment_texts` each request carries the two
texts. A judgment of a field that did not change is listed in
`unmatched_judgments`.

### What an applied judgment changes

`base` is the text-similarity part of §2, before the structure evidence
(lists, negations, requirement words, numbers) is added. A change is
**blocked** when it removed a list item, flipped a negation, shifted a
requirement word or changed a number. It has **soft** evidence when it
added or modified a list item, or added or removed a heading.

A blocked change is never lowered: `equivalent` and `extended` leave its
`m`, its bump and its propagation as they are. The judgment is still
reported as `applied`. Raising relations are not blocked.

* **`equivalent`**, not blocked: `base' = base − w · max(base − 0.05, 0)`,
  and `m` is put together again with the same structure evidence. When
  decisive, the field is not breaking and its bump is at most Patch. It is
  at most Minor with soft evidence, and when the field is additive on its
  own (nothing was lost and a sentence was added), so it stays additive.
* **`extended`**, not blocked: `base' = base + w · (0.25 − base)`. When
  decisive, the field is additive and its bump is at most Minor.
* **`narrowed`, `replaced`, `reversed`**: `m' = 1 − (1 − m)(1 − w · e)`
  with `e` = 0.315, 0.275 and 0.403. When decisive, the field is breaking
  and the bump is Major.

A judgment that is applied but not decisive moves `m` and nothing else: the
field stays breaking or not by the usual rule (`m ≥ bm`).

The paraphrase from the last test vector has `m = 0.802` and is Major on
its own. With `equivalent`:

| `c` | `w` | `m` | Bump |
| --- | --- | --- | --- |
| 0.55 | 0 | 0.802 | Major (`inconclusive`) |
| 0.75 | 0.375 | 0.520 | Major |
| 0.80 | 0.5 | 0.426 | Patch |
| 0.90 | 0.75 | 0.238 | Patch |

`Build the login button` → `Build the logout button` has `m = 0.441` and is
Minor on its own. With `replaced` at `c = 0.9` it has `m = 0.564` and is
Major.

The field reports `metrics.lexical_magnitude` (the `m` before the judgment)
and `metrics.judgment_weight`, and `FieldAssessment.judgment` carries the
relation, confidence, weight, rationale and source.

### Shares in propagation

§5 lets the child that *owns* a removed or reworded passage take the change
at `m_eff ≥ 0.8` or `≥ 0.6`, however small the edit. A judgment scales
these floors, because a passage that only got new words was not removed.
With `left = max(1 − min(2w, 1), 0.25)`:

| Relation | Removed floor × | Reworded floor × |
| --- | --- | --- |
| `equivalent` | `left` | `left` |
| `extended` | `left` | 1 |
| raising | 1 | 1 |

So a decisive `equivalent` leaves a quarter of both floors (0.2 and 0.15).
Relevance is not scaled. A decisive raising judgment also treats a kept
passage whose scope words changed as a reworded one, so the child that is
about it is reached.

### Confidence

A flagged field that has no applied judgment has its confidence (§8)
multiplied by 0.85, and its explanation says `meaning not verified`. With
an applied judgment the field's confidence is

    floor + w · max(c − floor, 0)

where `floor` is that lowered value for a flagged field, and the field's
own confidence for one that was not flagged.

## 3. Activity (comments)

Lexicon matching on stemmed tokens, sentence by sentence (split at
`. ! ? ;` and line breaks), per category — blocker 0.6, scope 0.7, risk 0.5,
schedule 0.55, uncertainty 0.3 (also any `?`, once per change set),
resolution 0.5 — skipping matches negated within the three previous tokens
of the same sentence ("not blocked"; but "Not great. Blocked on the API." is
a blocker). Words that are usually innocent on their own match only as
phrases: `drop it` / `drop support` (not `drop-down`), `rewrite it` /
`full rewrite` (not "rewrite the tests"), `external dependency`.

Each category counts **once per change set**, however many comments repeat
it: its weight × `1 + 0.15·min(distinct terms − 1, 2)` (at most × 1.3).
`m = noisy-OR(risk categories) · (1 − ½·resolution)`. Severity 0.4 (0.6
when a scope signal is present). A blocker or schedule signal travels with
the `activity_schedule` strengths (up 0.5, dependency 0.8) and reaches the
parent and what the entity blocks at full relevance, whatever words they
share. Activity never changes the version. The lexicon is policy data: add
your team's terms and languages (Mongolian terms ship by default).

## 4. Local score and version

* field risk `rᶠ = severityᶠ · mᶠ` (severity caps: description 0.85, status
  0.9, parent 0.7, estimate 0.6, due date 0.6, project 0.6, title 0.55,
  team 0.5, assignee 0.4, relations 0.4, start date 0.4, priority 0.35,
  cycle 0.35, labels 0.2, unknown fields 0.25);
* **counted once**: each field has a weight `wᶠ` (1) — when the same fact is
  in two fields, the weaker counts only for what the other does not already
  say: a project move that is also the re-parenting (an issue whose parent
  is its project) counts once, the weaker field with weight 0 (and it does
  not travel); a rename mirrored in title and description counts the weaker
  field at `1 −` the share of its changed terms the other also changes;
* **content score** `raw = noisy-OR(wᶠ·rᶠ)` over content fields;
* **local score** `amplify(noisy-OR(raw, r_activity), γ_phase · γ_deadline · γ_churn)`:
  * phase of the **work the change disturbs**: for an update the larger of
    the phase before and after (backlog 0.5, unstarted 0.75, started 1.0,
    completed 1.15 — rework —, canceled 0.15, no status 0.8), so canceling
    started work weighs as started work; completing work is not rework (the
    completed factor is capped at started's for that move); a creation uses
    the new phase; deleting closed work is cleanup (0.15);
  * deadline: `1 + 0.5·e^{−days_left/14}` (1.5 when overdue), for active
    entities, the larger of before and after;
  * churn: `1 + 0.3·tanh(I/1.5)`, `I = Σ past magnitudes · 2^{−age/7 days}`
    (history magnitudes must be in `[0,1]`);
* **anomaly**: z-score of `raw` against the entity's past change magnitudes
  (≥ 5 samples; none when not finite).

**SemVer bump** (from content, not context):
created → `initial` (1.0.0) · deleted → MAJOR · any breaking field, or
`raw_major ≥ 0.55` → MAJOR · any additive field, or `raw_minor ≥ 0.2` →
MINOR · any other field change → PATCH · only activity → none, where
`raw_major` / `raw_minor` is the noisy-OR of `wᶠ·rᶠ` over the fields whose
`max_bump` allows that level (so a reassignment plus a cycle move, 0.4 and
0.35, is MINOR, not MAJOR). The bump never exceeds the largest `max_bump`
of the changed fields.

**Basis pins.** An entity planned against version `v` of another pins it.
Each pin is compared, with Cargo's caret rules, with the version before and
after the change: same → *current*, patch → *compatible*, minor → *review
suggested*, major (or backwards) → *incompatible*, deleted → *removed*.
`basis_status` is the worst; `basis_introduced` says whether this change
produced it. Only a pin the change made worse counts: an active, unchanged
entity it made incompatible lifts the overall score to at least *medium*
and asks for a review. A pin that was already stale is reported but
neither raises the score nor lists the entity on its own.

**Versions in the registry.** The stateless engine only proposes
`next_version`. The registry (`PutEntity`, `Import`) applies it and keeps
the history:

* **First version.** A new id starts at 1.0.0, or at the version it
  carries. Later the registry assigns versions itself and ignores a sent
  version.
* **A put that changes the entity** is assessed against the stored version
  and its stored neighbourhood, and gets the bump above.
* **A put that matches the stored entity** is not assessed. The version
  stays, and the answer has `unchanged` set and an empty assessment. A dry
  run and a put with activity events are still assessed.
* **Ordering guard.** A put or delete may carry `source_seq`, the position
  of the change in the source's own order for that entity. The registry
  keeps the highest sequence it has applied per id, also after a deletion.
  A change whose sequence is equal or lower is skipped as stale: nothing is
  assessed, stored or versioned, and `skipped_stale` is set. The guard is
  checked before `expected_version`, and on dry runs too. A change without
  a sequence is always applied and leaves the stored sequence as it was.
  A duplicate that carries a higher sequence moves the stored sequence and
  nothing else. So does a deletion of an id that is not stored: a delete
  call fails with "not found", an import counts it as unchanged, and both
  keep the sequence (a dry run keeps nothing).
* **Import.** A new entity gets its first version, its basis pins and the
  pins other entities hold on it exactly as from a put, but **without an
  assessment**: its history entry has bump `initial`, score 0, level
  *none* and no changed fields. Unchanged items are skipped, changed items
  are assessed and versioned like a put, and deletions are assessed like a
  delete (a deletion of an id that is not stored keeps only its sequence). Items are
  applied in input order, and any order of entities gives the same versions
  and pins. Churn and anomaly read only the history of updates, never the
  creation entry, so later changes are assessed the same after an import as
  after puts.

## 5. Propagation

Every scored field is a *seed* with risk `wᶠ·rᶠ` and a strength per channel:

| | down (children…) | up (parent…) | dependency (blocked) | upstream (blockers) | related | sibling |
|---|---|---|---|---|---|---|
| description | 0.9 | 0.3 | 0.3 | 0.15 | 0.2 | 0.15 |
| title | 0.6 | 0.2 | 0.2 | 0.05 | 0.15 | 0.1 |
| estimate | 0.2 | 0.8 | 0.4 | 0.1 | 0 | 0.15 |
| status | 1.0 | 0.6 | 0.8 | 0.2 | 0.1 | 0 |
| due date | 0.7 | 0.5 | 0.9 | 0.6 | 0.05 | 0.1 |
| priority | 0.4 | 0.15 | 0.3 | 0.5 | 0.05 | 0.25 |
| assignee | 0.1 | 0.05 | 0.15 | 0 | 0 | 0.2 |
| parent | 0.3 | 0.7 | 0.2 | 0 | 0 | 0.2 |
| created / deleted | 0 / 1.0 | 0.6 / 0.6 | 0.3 / 0.7 | 0 / 0.1 | 0.05 / 0.1 | 0.15 / 0.1 |
| comment (blocker or slip) | 0.3 | 0.25 (0.5) | 0.35 (0.8) | 0.2 | 0.1 | 0.05 |

Status strengths are further scaled by the kind of transition (down / up /
dependency): cancel 1.0/0.8/0.8, reopen 0.6/0.7/0.8, regress 0.7/0.4/0.6,
revive 0.5/0.4/0.4, complete 0.2/0.15/0.1, progress 0.1/0.05/0.05, lateral
0.05/0.02/0.02.

Breadth-first from the changed entity: down continues to descendants, up to
ancestors, dependency to transitively blocked entities and their children,
upstream to transitive blockers; related and sibling stop after one hop.
Each hop beyond the first multiplies by 0.6; depth ≤ 4; a path whose
strongest possible contribution (`severity · transfer`) is below 0.01 is not
followed; the strongest path per (entity, channel) wins. Contributions below
0.01 are dropped (a relevance floor is not evidence), except effort moves,
which are summed at their target first (below).

**Step budget.** Each seed walks within its own budget — `max(64, 500 000 ×`
its share of the request's total seed risk`)` steps — and nothing beyond the
budget is queued, so one change cannot starve another and the result does
not depend on the order of the changes in the request. A change whose walk
ran out reports `propagation_truncated`.

At each target, a contribution is `wᶠ·severity · m_eff · transfer · relevance`:

* **text**: overlap `o` = the best passage's match with the target: its
  cosine with the target's text or with its title alone (for a comment, the
  share of the title's TF-IDF weight it names), whichever is higher — and,
  with embeddings, at least `clamp(2·|cos(e_after − e_before, e_target)|)`
  (embeddings only add evidence). **Ownership:** among the changed entity's
  children, the best match for a removed or reworded passage (overlap ≥ 0.2,
  ties within 90 %) — for a scope comment, for the comment — *owns* it; an
  active owner takes it at `o = 1` with `m_eff ≥ 0.8` (removed: its planned
  scope is gone, however small the parent's edit) or `≥ 0.6` (reworded).
  Relevance `0.15 + 0.85·o` down/related, `0.5 + 0.5·o` up/dependency/upstream;
  siblings `(0.15 + 0.85·o) · max(capacity, o)` (capacity 1.0 when sharing
  the assignee or cycle, else 0.3);
* **effort moves** — an estimate change, a creation or deletion with an
  estimate, a cancellation (or revival) of an estimated entity: up,
  `m_eff = saturate(|Δ| / base, 0.3)` with `base = max(effort before, effort
  after)` of the target (effort = `max(own estimate, Σ estimates of children
  neither deleted nor canceled)`), so an increase and a decrease of the same
  size weigh the same; siblings, the share of the parent they share, ×
  capacity. Several effort moves reaching one target (for one base) count
  as one: `saturate(Σ|Δ| / base(ΣΔ), 0.3)` × the `|Δ|`-weighted mean of
  their `severity · transfer · relevance`, so fifty small re-estimates
  weigh what one large one would;
* **status down**: active children 1.0 (cancel), 0.8 (regress), 0.4 (reopen),
  0.5 otherwise; closed children 1.0 (reopen: re-verify), 0.1 (cancel), 0.2;
  dependency: active 1.0, closed 0.1;
* **dates**: 1.0 (and `m_eff ≥ 0.6`) when the move **creates a conflict or
  makes one worse** (due dates: child now due after parent, child's new due
  after the parent's, blocker now due after the blocked entity's due/start;
  any date of a blocked entity now before its blocker's due); a move that
  relieves a conflict is not one. Otherwise: down 0.5 pulled in / 0.2
  pushed out (0.4 / 0.15 when the child has no due date), up 0.3,
  dependency 0.1 / 0.4 (a blocker finishing earlier relieves), upstream
  0.5 / 0.1, other channels 0.3;
* **references**: siblings sharing the old or new assignee/cycle 1.0, else
  0.09; children still carrying the old project/team/cycle 0.5 (they usually
  move along), other children 0.3; children of a re-parented entity 0.5;
* **lifecycle**: deleting a parent → active children 1.0, closed 0.2; a
  created or deleted blocker → active dependents 1.0, closed 0.1;
* **comments**: a blocker or slip reaches the parent and blocked entities at
  relevance 1.0; otherwise as text.

**Combination at a target.** The strongest contribution per (source,
field) — one piece of evidence counted once —, effort moves summed per base,
then `noisy-OR` of `amplify(·, γ_phase · γ_deadline)` of the **target** per
contribution: disturbing in-progress or completed work weighs more than
backlog. The completed-phase rework boost (γ > 1) applies only to evidence
that changes what was done: not to removed text (the work is moot, not
redone) nor to text below the 0.3 overlap flag. For a changed entity the
result is noisy-OR'd with its own local score.

## 6. Plan-consistency violations

Checked on the neighbourhood before and after; only those the change
**introduced** count: orphaned work under a canceled/deleted parent (0.8),
blocking cycle (0.7), child due after parent (0.6), blocker due after the
blocked entity's due/start (0.6), blocked by a canceled/deleted entity
(0.55), parent completed with active children (0.5), children's estimates
over the parent's by more than 10 % (0.45), overdue (0.4). An estimate
overflow is the parent's, whichever children make it up: adding, canceling
or deleting a child does not make it new. A violation that was there before
and got worse (more days late, more points over) is reported with "got
worse", not as introduced.

Only the entity that **needs action** — the orphaned child, the blocked
entity, the completed parent, the overflowing parent, the late child, the
late blocker, the overdue entity, the changed member of a cycle — is
floored at `0.9 × severity` and gets the violation's action; the others are
merely involved (the violation is one of their reasons, nothing more). The
overall score is floored at the same `0.9 × severity`.

## 7. Aggregation and decision

* **severity** = the highest entity score (changed entities included);
* **affected** = the changed entities, what structurally depends on them
  within 4 hops (descendants, transitively blocked entities), every entity
  the change reached (combined score ≥ 0.01), and the entities that need
  action for an introduced violation; canceled bystanders excluded.
  Unrelated context the caller sends does not dilute anything;
* **effort_total** = `Σ effort`, **effort_at_risk** = `Σ score·effort` over
  the affected (effort = estimate, else 1);
* **breadth** = `(effort_at_risk − score·effort of the most affected entity) / effort_total`;
* **score** = `escalate(severity, breadth, 0.4)` =
  `1 − (1 − severity)·(1 − breadth)^0.4` (the exponent is
  `breadth_weight / severity_weight`), floored by introduced violations
  (§6) and at *medium* by a pin this change made incompatible. It is never
  below the worst single impact;
* **level**: none < 0.05 ≤ low < 0.25 ≤ medium < 0.5 ≤ high < 0.75 ≤ critical;
* **recommendation**: no action · notify · review · partial re-plan · full
  re-plan; a re-plan always names a root (with nothing but deleted entities
  left to re-plan, it is a review);
* **re-plan root**: the lowest common ancestor of the candidates of every
  entity scoring ≥ high (of the most affected one when none does) and of
  the change behind each one's strongest contribution: a deleted entity
  stands for its parent, a changed entity without children for its parent
  (a moved one for the parents it left and joined); lifted above deleted
  ancestors; when they share no ancestor, the most affected candidate;
* **actions** per entity:
  * a violation it needs to act on: orphaned → *cancel or rehome*; blocked
    by a closed entity, cycle → *resolve conflict*; schedule, dependency,
    overdue → *reschedule*; estimate overflow → *re-estimate*; premature
    close → *review* — except that removed text it is about (*obsolete
    candidate*) wins over rescheduling, re-estimating or reviewing;
  * below *low*: *review* if this change made one of its pins review
    suggested or worse, else none;
  * a changed entity whose own change dominates → *replan* / *review* /
    *notify* by score;
  * closed or deleted work → *notify* (*re-verify* for completed children
    of a reopened parent): it is never re-estimated, rescheduled, rescoped
    or obsoleted;
  * otherwise from its strongest contribution: parent canceled or deleted →
    *cancel or rehome*; blocker canceled or deleted → *resolve conflict*;
    effort moved under it (a child re-estimated, created, deleted, canceled)
    → *re-estimate*; a new blocker, or a blocker reopened, moved back or
    revived → *reschedule*; moving along with a moved parent → *notify*; a
    blocker or slip comment → *reschedule*; text it is about (down and
    related only) removed → *obsolete candidate*, reworded or added
    (comments: with a scope signal) → *update scope*, other text →
    *review* / *notify* as below; an estimate change of a blocker →
    *reschedule*, of a parent or sibling → *notify*; a creation reaching a
    sibling or related item → *notify*; a date conflict (or a date change
    at ≥ medium) → *reschedule*, else *notify*; priority → *re-prioritize*;
    anything else *review* at ≥ medium (or with an introduced incompatible
    pin), *notify* below;
  * never what the entity's own change just did (re-estimate, reschedule,
    cancel or rehome): *review* instead.

## 8. Confidence

`0.9^(missing referenced entities)` × `0.7` if activity targeted an entity the
request did not include × the mean field confidence (text: 1.0 with
embeddings, 0.9 lexical, 0.7 when a side has fewer than 4 terms) × 0.8 if
any change's propagation ran out of its step budget; clamped to `[0.3, 1]`.
A text field in the gray zone counts with a lower confidence until a
judgment is applied (§2a).

## Request limits

Up to 10 000 change sets and 200 000 context entities per request; 1 MiB
per text field (and per change set's activity text, and per text given to
`compare_text`); 1 000 events and 1 000 field changes per change set;
100 000 history points; 10 000 fields, relations, basis pins or set items
per entity; embeddings of at most 16 384 finite components; numbers within
`±10¹²`; ordinal scales positive and at least the rank. Field changes are
validated after they are applied, like snapshots. An assessment never
carries a non-finite number.

Up to 64 judgments per change set, each with a rationale of at most 2048
bytes and a source of at most 256 bytes. The `field` of `CompareText` is at
most 256 bytes.

## Known limits

* Lexical measures do not understand synonyms; send embeddings for semantic
  comparison (the engine is model-agnostic — any sentence-embedding model).
* Passage overlap is bag-of-words (no phrase matching: "credit card" is two
  words); titles are matched separately to make up for long descriptions.
* The gray-zone rules (§2a) read English scope and negation words. A
  change in another language is still flagged as a suspected paraphrase, a
  term substitution or a near-threshold change, and a judge reads any
  language.
* A date's compression window does not know the entity's planned duration.
* The stemmer and stop-words are English; other scripts are tokenized and
  compared exactly (typo alignment and lexicons still apply).
* Defaults are calibrated on Linear-style trees (project → issue →
  sub-issue); tune severities and thresholds with real outcomes.
