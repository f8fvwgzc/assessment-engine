# The model judge

The engine flags text changes whose meaning its word counts cannot settle
([SCORING.md](SCORING.md) §2a). A *judge* answers how the two texts of such
a field relate. This file describes the judge that ships with the server.
It is optional and off by default. `assessment-core` never calls it: the
server asks the judge and hands the answers to the engine as plain
judgments.

Status: the judge is tested against local fake servers only. The paid
evaluation against the real API
(`cargo run -p assessment-judge --example judge_cases`, six cases, at most
0.30 US dollars) has not been run yet. The cost and latency numbers below
are estimates.

## The prompt

The prompt is code. The source of truth is
[`crates/assessment-judge/src/prompt.rs`](../crates/assessment-judge/src/prompt.rs):
`SYSTEM_PROMPT`, `schema()`, `user_content()` and `PROMPT_VERSION`
(now 1). This file does not copy it. Change the prompt there, and raise
`PROMPT_VERSION` when an answer could change, because cached answers are
keyed by it.

One question is one request to the Anthropic Messages API:

* `system`: the fixed instructions, marked for prompt caching.
* one user message: a JSON object with `field`, `entity_kind`,
  `context_title` (the entity's title, at most 300 characters, left out
  for the title field), `before`, `after` and `hints` (how many list
  items, negations, requirement words and numbers the engine saw change).
  The engine's gray-zone reasons are not sent.
* `output_config`: the effort and a JSON schema, so the answer is one of
  the six relations with a confidence and a short English rationale.

The two texts are sent as data and may be in any language. The rationale
is English.

## How a judged call runs

1. The server assesses the request. This is the engine's own result.
2. It collects the flagged `title` and `description` fields
   (`judgment_requests`) and asks the judge about them, all at once, each
   inside the time that is left.
3. It assesses again with the answers as judgments. This is the result
   that is returned. A field that is first flagged in this second pass
   stays `requested`.

A judge never fails a call. A field without an answer keeps the engine's
result and gets the status `unavailable` with a `detail`. If the second
pass cannot run, the first result is returned.

`Assess`, `AssessBatch`, `AssessStream`, `PutEntity` and `CompareVersions`
are judged. A flagged `PutEntity` is judged before it is stored, also when
the caller sent a judgment that is stale, and the
version record keeps the applied judgments, so `CompareVersions` agrees
with the history. `Import` and `CompareText` never call the judge.

A caller controls it per call with `options.judge`: `false` is never
judged, `true` is judged when the server has a judge, and unset follows
`--judge-default`. Judgments the caller sends are used as they are, and
those fields are not asked again. A judgment that is stale (its
fingerprint does not match the two texts) is not used, so the judge is
asked about that field. When the judge gives no answer for it, the field
is `unavailable` with a `detail`, like any other field.

Each judged call writes one log line, `judged call`, with the counts
(flagged, judged, cached, unavailable), the tokens and the estimated cost.
`GetPolicyResponse.judge` holds the judge id, for example
`anthropic/claude-opus-5-5`.

## Flags

All are flags of `serve`. Each has an environment variable.

| Flag | Variable | Default | Meaning |
| --- | --- | --- | --- |
| `--judge off\|on\|auto` | `ASSESSMENT_JUDGE` | `off` | `on` needs a key. `auto` runs without a judge when there is no key. |
| `--judge-default on\|off` | `ASSESSMENT_JUDGE_DEFAULT` | `on` | Whether a call that does not set `options.judge` is judged. |
| `--judge-model` | `ASSESSMENT_JUDGE_MODEL` | `claude-opus-5-5` | The model. |
| `--judge-effort` | `ASSESSMENT_JUDGE_EFFORT` | `low` | `low`, `medium`, `high`, `xhigh` or `max`. |
| `--judge-timeout-secs` | `ASSESSMENT_JUDGE_TIMEOUT_SECS` | 15 | Longest time for one judge call (1 to 600). |
| `--judge-max-fields-per-request` | `ASSESSMENT_JUDGE_MAX_FIELDS_PER_REQUEST` | 4 | Most fields of one request that are asked. |
| `--judge-max-fields-per-batch` | `ASSESSMENT_JUDGE_MAX_FIELDS_PER_BATCH` | 16 | Most fields of one batch that are asked. |
| `--judge-concurrency` | `ASSESSMENT_JUDGE_CONCURRENCY` | 8 | Judge calls at once. |
| `--judge-max-calls-per-minute` | `ASSESSMENT_JUDGE_MAX_CALLS_PER_MINUTE` | 60 | Rate limit. |
| `--judge-max-usd-per-hour` | `ASSESSMENT_JUDGE_MAX_USD_PER_HOUR` | 10 | Spend cap, from the price table. |
| `--judge-max-text-chars` | `ASSESSMENT_JUDGE_MAX_TEXT_CHARS` | 8000 | Longer texts are not sent. |
| `--judge-max-tokens` | `ASSESSMENT_JUDGE_MAX_TOKENS` | 4096 | Largest answer. |
| `--judge-cache-entries` | `ASSESSMENT_JUDGE_CACHE_ENTRIES` | 4096 | Answers kept in memory. |
| `--judge-fallbacks auto\|on\|off` | `ASSESSMENT_JUDGE_FALLBACKS` | `auto` | Whether the API may answer with another model when the first declines. `auto` is on for the default model only. |
| `--judge-base-url` | `ASSESSMENT_JUDGE_BASE_URL` | `https://api.anthropic.com` | The API root. |
| `--judge-allow-unauthenticated` | `ASSESSMENT_JUDGE_ALLOW_UNAUTHENTICATED` | off | See the threat model. |
| `--anthropic-api-key-file` | `ANTHROPIC_API_KEY_FILE` | none | A file that holds the key. |

The key comes from `ANTHROPIC_API_KEY` or from the key file. The file wins.
A key file that cannot be read or is empty is an error, also with `auto`.
There is no flag that takes the key itself, so it never shows in a process
list.

`assess --judge` takes the model, effort, timeout, fields per request,
tokens, fallbacks, base URL and key file flags. It asks the API directly,
without the cache and the limits, and each judge call has the whole
timeout. It prints one line on stderr:

    judge: 1 flagged, 1 judged (claude-opus-5-5, 2100/180 tokens, ~$0.0120)

The numbers in that line are made up for this example.

## Limits

* Fields: 4 per request and 16 per batch are asked, in the engine's
  request order (SCORING.md §2a). The engine lists at most 16 per request
  (`judge.max_requests`), and the rest are `over_budget`.
* Text: a field with a text over 8000 characters is not sent. The engine
  flags nothing over 20 000.
* Time: one judge call gets the time the gRPC deadline leaves, minus a
  reserve for the second pass, and at most 15 s. Send a deadline. A call
  does not start with less time left than 1.2 times the usual latency.
  That bar is at most three quarters of the timeout, so a call with the
  whole timeout ahead of it always starts.
* Retries: timeouts, connection errors and the answers 408, 409, 429 and
  5xx are tried again, at most three attempts, always inside the deadline.
* Cache: 4096 answers, keyed by the judge id, the prompt version and the
  question. Equal questions that arrive together share one call. A cached
  answer is free and is used even when a limit is reached.
* Rate and spend: 8 calls at once, 60 per minute, 10 US dollars per hour.
  The cost is reserved before a call and corrected after it. A question
  waits for a free place as long as its call could still start inside its
  deadline, so a batch of 16 fields runs as two rounds of 8.
* Batches: the second passes of one `AssessBatch` run a few at a time,
  inside the same per-call limit as the first passes. The reserve for the
  second pass counts the whole first pass, with the time its items waited.
* Breaker: five failures in a row pause the judge for 30 s. One answer
  401, 403 or 404 (a bad key, an unknown model) pauses it for 300 s at
  once. Three identical 400 or 402 answers in a row do the same.
* Answer size: at most 1 MiB is read.

The spend cap uses the price table in
[`crates/assessment-judge/src/price.rs`](../crates/assessment-judge/src/price.rs).
The table is an estimate for the cap. It is not a bill, and an unknown
model is counted at a high price on purpose.

## When the judge does not answer

The field's status is `unavailable`, the result is the engine's own, and
`detail` is one of:

| `detail` | Cause |
| --- | --- |
| `this server has no judge` | The call set `options.judge = true` on a server without a judge. |
| `text longer than N characters` | Over `--judge-max-text-chars`. |
| `over the server's cap of N fields` | Over the per-request or per-batch cap. |
| `not enough time left` | The deadline leaves too little for a call. |
| `judge timed out after N s` | No answer in time. |
| `spend cap reached` | Over `--judge-max-usd-per-hour`. |
| `refused (…)` | The model declined to answer. |
| `HTTP <status> …` | The API answered with an error. |
| `could not connect to the API` | No network, DNS or TLS failure. |
| `server shutting down` | The server is stopping. |
| `the judge task failed`, `the second pass failed` | An internal error. The first result is returned. |
| `too many judge calls are running` | No place under `--judge-concurrency` came free while the call could still start inside its deadline. |
| `the judge call rate limit is reached` | Over the calls per minute. |
| `the judge is paused after repeated failures` | The breaker is open. |
| `the answer was cut off at the token limit` | The model ran out of tokens. |
| `the answer is not a valid verdict`, `unexpected stop reason: …` | The answer cannot be used. |

A failure without a message of its own reads `the judge failed (<kind>)`.
A detail is at most 200 characters and never holds the key.

You can see this without a network or a real key. The request goes to a
closed local port:

```bash
jq '.options.judge = true' examples/requests/paraphrase.core.json \
  | ANTHROPIC_API_KEY=dummy ASSESSMENT_JUDGE_BASE_URL=http://127.0.0.1:9 \
    cargo run -p assessment-server -- assess --judge -
#   status "unavailable", detail "could not connect to the API", bump "major", exit code 0
```

CI runs the same check in the container image with `--network none`.

## Threat model

* **The judged text is hostile input.** Whoever edits an issue controls
  the text the model reads, and can write instructions into it. The
  prompt marks both texts as data, the answer must fit a schema with six
  relations, and nothing in an answer is executed. What an attacker can
  gain is a wrong relation for their own change: a real change passed off
  as `equivalent`. That is why lowering needs more confidence than
  raising, why a removed list item, a negation flip, a requirement-word
  shift and a changed number block lowering, and why the field always
  reports `lexical_magnitude` and the judgment next to the result.
* **The rationale is untrusted text.** It is written by a model that read
  hostile input. It is cut to 2048 bytes. Show it as plain text. Do not
  render it as HTML or Markdown, and do not feed it to another model as
  instructions.
* **The key.** It is read from the environment or a file, never from a
  flag. It is kept out of `Debug` output, errors, logs and details.
* **Who can spend.** Every judged call costs money. A server with a judge
  on an address that is not loopback must have `--auth-token`, or it does
  not start. `--judge-allow-unauthenticated` lifts this for a network
  that is already closed. The per-request, per-minute and per-hour limits
  bound what one caller can spend.
* **Where the text goes.** Only to `--judge-base-url`. Redirects are not
  followed. Plain `http://` is refused unless the address is loopback.
  At startup one `GET /v1/models/{model}` checks the key and the model.
  It costs nothing and sends no text.
* **Consent.** The server cannot know whether a user agreed to send a
  text to a model. The caller must send `options.judge = false` when they
  did not, or run with `--judge-default off` and opt in per call.
* **Failure.** A judge that is down, slow or wrong never blocks a write
  and never fails a call. The engine's own result is always there.
