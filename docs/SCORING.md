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
`sync`→`async`); tokens longer than 40 characters. The distance is a banded
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
the number of changed passages. Passages are weighted with an IDF over **every text in the request's
neighbourhood** (shared vocabulary weighs little). Relevance to another
entity uses the **best single passage** (max-passage scoring), so a
sub-issue about one changed sentence is not diluted by the rest of a long
rewrite (§5).

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

## Request limits

Up to 10 000 change sets and 200 000 context entities per request; 1 MiB
per text field (and per change set's activity text, and per text given to
`compare_text`); 1 000 events and 1 000 field changes per change set;
100 000 history points; 10 000 fields, relations, basis pins or set items
per entity; embeddings of at most 16 384 finite components; numbers within
`±10¹²`; ordinal scales positive and at least the rank. Field changes are
validated after they are applied, like snapshots. An assessment never
carries a non-finite number.

## Known limits

* Lexical measures do not understand synonyms; send embeddings for semantic
  comparison (the engine is model-agnostic — any sentence-embedding model).
* Passage overlap is bag-of-words (no phrase matching: "credit card" is two
  words); titles are matched separately to make up for long descriptions.
* A date's compression window does not know the entity's planned duration.
* The stemmer and stop-words are English; other scripts are tokenized and
  compared exactly (typo alignment and lexicons still apply).
* Defaults are calibrated on Linear-style trees (project → issue →
  sub-issue); tune severities and thresholds with real outcomes.
