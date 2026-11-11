# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- OpenAI judge provider, so that the judge does not depend on one vendor.
  `anthropic` stays the default, and the other providers did not change.
  The provider is tested against a local server only: it has not been run
  against the real OpenAI API, and nothing about its answers, cost or
  latency has been measured.
  - `assessment-judge`: `OpenAiJudge`, `OpenAiConfig` and
    `DEFAULT_OPENAI_MODEL` (feature `openai`, on by default). An
    `OpenAiJudge` (`openai/<model>`) sends one `POST
    {base}/chat/completions` per question with a bearer key, the shared
    prompt, `max_completion_tokens`, `reasoning_effort`, a strict
    `json_schema` answer format and `store: false`. It sends no
    `temperature`, tools or user id. A refusal, a content filter, a cut-off
    answer and an answer that does not fit the schema are errors, never a
    guess. Cached input tokens are counted as cache reads. A 400 that
    names `reasoning_effort` is tried once more without that field, and
    the field is left out from then on. The retries, the 1 MiB answer cap,
    the refusal of redirects and the base URL rule (https, or http on
    loopback) are the ones of the Anthropic client: both now share one
    private HTTP module. `ApiKey` is exported at the crate root.
  - `price`: twelve OpenAI models in the price table. `price_for` takes
    an `openai/` prefix off a name, as it does with `anthropic/`. A name
    has the price of a table name only when it is that name, or that name
    and a date (`-20260101` or `-2026-01-01`). So `gpt-5.4-mini` is never
    priced as `gpt-5.4`, and a tier the table does not know (`gpt-5.5-pro`)
    has the high price of an unknown model, not the price of the cheaper
    model whose name it starts with. `price::assumed_usage` is the usage
    behind a reservation. The prices are estimates for the spend cap.
    They are not a bill.
  - A 200 answer of the OpenAI API that reports no usage (a gateway may
    leave it out) is counted at the assumed usage of the request: the
    estimated input and the whole of `--judge-max-tokens`. So it is never
    free for the spend cap. A warning is logged once.
  - The key is also kept out of what a server sends back. A masked copy
    of the key in an error body (`sk-ab...wxyz`) is replaced with
    `[redacted]`, and a key that a 200 answer echoes in its model name or
    its rationale does not reach the judgment. This holds for the
    Anthropic client as well.
  - Server: `--judge-provider openai` and `--openai-api-key-file`
    (`OPENAI_API_KEY_FILE`), for `serve` and `assess --judge`. The key is
    read only from `OPENAI_API_KEY` or from that file. There is no flag
    that takes the key. Each provider reads only its own variables:
    `ANTHROPIC_API_KEY` and `ANTHROPIC_API_KEY_FILE` are not read with
    `openai`, and `OPENAI_API_KEY` and `OPENAI_API_KEY_FILE` are not read
    with `anthropic`. So a machine that has the variables of both
    providers can change the provider alone. The default model is `gpt-6.1-sol` and the
    default base URL is `https://api.openai.com/v1`. `--judge-base-url`,
    `--judge-max-tokens` and every `--judge-effort` are taken.
    `--judge-fallbacks on|off`, `--judge-cli-path` and
    `--anthropic-api-key-file` are refused with `openai`, and
    `--openai-api-key-file` is refused with every other provider.
    A `--judge-base-url` (`ASSESSMENT_JUDGE_BASE_URL`) on
    `api.anthropic.com` is refused with `openai`, and one on
    `api.openai.com` is refused with `anthropic`: the two providers share
    that setting, and the key must not go to the other company.
    `--judge on` without a key is a startup error that names
    `OPENAI_API_KEY`, and `--judge auto` runs without a judge.
  - `assess --judge` with `openai` names the judge id in its report line:
    `openai/gpt-6.1-sol`.
  - `examples/judge_cases.rs` takes `JUDGE_PROVIDER=openai`. It is a paid
    run, needs `OPENAI_API_KEY` and has not been run.
  - CI lints `assessment-judge` with the `openai` feature alone.
- Command line judge providers, for a machine without an Anthropic API
  key. `codex-cli` is not a Claude model. It was added at the user's
  request.
  - `assessment-judge`: `CliJudge`, `CliConfig`, `CliKind` and
    `cli::find_program` (feature `cli`, on by default). A `CliJudge` runs
    the Claude Code command line (`claude-cli/<model>`) or the Codex
    command line (`codex-cli/<model>`) once per question. The texts go to
    the program on standard input only. The program runs without tools
    (Claude Code) or in a read-only sandbox (Codex), in an empty private
    directory that each call makes and removes, and is stopped with its
    process group at the deadline or when the call is dropped. The signal
    is sent before the call returns, not by a later task. At most 1 MiB
    of output is read. A call is not tried again.
  - The program does not get the environment of the engine. It gets the
    variables in `cli::PASSED_ENV` (such as `HOME`, `PATH` and the proxy
    variables) and `CLAUDE_CONFIG_DIR` or `CODEX_HOME`. No key, token or
    endpoint variable reaches it. Codex also runs with
    `-c shell_environment_policy.inherit=none`.
  - `assess --judge` takes SIGINT and SIGTERM: it stops the judge and its
    program, removes the directory of the call and ends with an error.
  - The prompt and the strict answer parser are shared by all providers:
    `prompt::Answer`, `prompt::single_prompt` and
    `prompt::MAX_RATIONALE_CHARS`. `Effort` and `DEFAULT_MODEL` are now in
    the crate root and no longer need the `anthropic` feature. The old
    paths in `anthropic` still work.
  - `price::is_subscription`, `price::FREE` and
    `price::SUBSCRIPTION_PREFIXES`: a call of a command line provider
    costs 0, so the hourly spend cap does not bind. `JudgeError::persistent`
    marks a failure that will not go away soon (not logged in, usage limit
    reached): `LimitedJudge` pauses for 300 s at once.
  - Server flags `--judge-provider anthropic|claude-cli|codex-cli`
    (`ASSESSMENT_JUDGE_PROVIDER`, default `anthropic`) and
    `--judge-cli-path` (`ASSESSMENT_JUDGE_CLI_PATH`), for `serve` and
    `assess --judge`. A command line provider reads no API key. `--judge
    on` does not start when the program is not found, and `auto` then runs
    without a judge. `--judge-base-url`, `--judge-fallbacks`,
    `--judge-max-tokens`, `--anthropic-api-key-file` and the efforts
    `xhigh` and `max` are refused with a command line provider.
    `--judge-cli-path` is refused with `anthropic`. The default of
    `--judge-concurrency` is 2 with a command line provider.
    `--judge-model` defaults to the program's own model with `codex-cli`.
  - `assess --judge` prints `cost not measured` instead of a price for a
    command line provider.
  - `examples/judge_cases.rs` takes `JUDGE_PROVIDER` and `JUDGE_CLI_PATH`.
    With a command line provider it needs no key and prints the time of
    each case. One run through Codex 0.160.0 (default model, free
    ChatGPT login) passed all six cases at 4.5 s per case.
  - A model name that starts with `-` is refused by every provider.
- Text judgments (core). The engine flags a text change whose meaning its
  word counts cannot settle, and uses a judgment of it when one is supplied.
  The engine never calls a model: a judgment is plain data in the request.
  - `TextDiff.gray_zone` (reasons and fingerprint) and
    `FieldAssessment.judgment` (`FieldJudgment`: status, reasons,
    fingerprint, lexical magnitude, and the supplied relation, confidence,
    weight, rationale and source). Reasons are `negation_changed`,
    `scope_qualifier`, `term_substitution`, `paraphrase_suspected` and
    `near_threshold`. A reorder, a removed list item and a clear negation
    flip are never flagged. The scope words are English.
  - `ChangeSet.judgments` takes `TextJudgment`s (`TextRelation`:
    `equivalent`, `extended`, `narrowed`, `replaced`, `reversed`,
    `undetermined`). A judgment is used only when its fingerprint matches
    the two texts (`text::fingerprint`, SHA-256). Otherwise it is `stale`.
    A decisive `equivalent` lowers a paraphrase to a patch. A decisive
    raising relation makes the change breaking.
  - `Assessment.judgment_requests` lists the flagged fields that still wait,
    in a fixed order. Only the first `judge.max_requests` flagged fields of
    a request can be listed. The rest are `over_budget`, and supplied
    judgments do not free a place.
    A removed list item, a negation flip, a requirement-word shift or a
    changed number blocks every lowering judgment: the magnitude and the
    bump stay. `AssessOptions.judgment_texts` adds the two texts.
    `Assessment.unmatched_judgments` lists judgments of fields that did not
    change. `AssessOptions.judge` is carried for servers. The engine
    ignores it.
  - `Policy.judge` (`JudgePolicy`) holds every threshold.
    `judge.enabled = false` turns the feature off: nothing is flagged,
    supplied judgments are ignored, and every output is byte for byte what
    it was before.
  - `Engine::compare_text_as(field, …)` reports the gray zone for a field.
    `compare_text` reports it for a description.
  - Registry: `PutOptions.judgments`, `PutOptions.hold_if_flagged` (a put
    that would ask for a judgment stores nothing, also when a supplied
    judgment is stale), `VersionRecord.judgments`
    (the applied judgments) and `Registry::compare_with`. `compare` uses the
    stored judgments again, so it agrees with the history. Import never
    uses judgments.
- Text judgments (gRPC contract and client). Every change is additive: an
  old client and an old server keep working.
  - Proto: enums `TextRelation` and `JudgmentStatus`, messages
    `TextJudgment`, `FieldJudgment`, `JudgmentRequest`, `JudgmentKey` and
    `GrayZone`. New fields: `AssessOptions.judge` and `.judgment_texts`,
    `ChangeSet.judgments`, `FieldAssessment.judgment`,
    `Assessment.judgment_requests` and `.unmatched_judgments`,
    `CompareTextRequest.field`, `CompareTextResponse.gray_zone`,
    `GetPolicyResponse.judge`, `VersionRecord.judgments`,
    `PutEntityRequest.judgments` and `.hold_if_flagged`,
    `CompareVersionsRequest.options` and `.judgments`.
  - A request judgment is checked strictly. An unspecified or unknown
    relation, or a confidence that is not a finite number in [0, 1], is
    `INVALID_ARGUMENT` with the field path, for example
    `changes[0].judgments[1].relation`. An unset confidence is 1.
  - A response is read leniently. An unknown judgment status reads as
    `unavailable` with a note in `detail`. An unknown relation reads as
    none. Unknown reasons are dropped.
  - `CompareText` never calls a model. `GetPolicyResponse.judge` is empty on
    a server without a judge.
  - Client: `AssessmentClient::with_timeout` (sent as `grpc-timeout` on
    every call), `compare_text_as`, `compare_versions_with` and
    `judge_info`.
- Model judge (new crate `assessment-judge`). `assessment-core` is unchanged
  and still makes no calls.
  - `Judge` (a trait that works behind `dyn`), `Question`, `Hints`,
    `Verdict`, `Usage`, `JudgeError`, `JudgeErrorKind`, and `ask`, which
    enforces the deadline and checks the answer.
  - `AnthropicJudge` (feature `anthropic`, on by default) asks the Anthropic
    Messages API for one of the six relations as schema-checked JSON. The
    two texts are sent as untrusted data and may be in any language. The
    rationale is English. It retries only timeouts, connection errors and
    408, 409, 429 and 5xx answers, at most three attempts, always inside the
    deadline. It follows no redirect, allows plain http for loopback only,
    reads at most 1 MiB, and keeps the API key out of `Debug` output, errors
    and logs. A background probe checks the key and the model at startup.
  - `CachedJudge` keeps 4096 successful verdicts and shares one call among
    equal questions that arrive together.
  - `LimitedJudge` allows 8 calls at once, 60 per minute and 10 US dollars
    per hour (reserved before the call, corrected after it), and pauses
    after repeated failures. A question waits for a free place as long as
    its call could still start inside its deadline, so 16 questions asked
    together run as two rounds of 8.
  - Without the `anthropic` feature the crate pulls in no HTTP or TLS crate.
  - `examples/judge_cases.rs` is a paid evaluation of six cases. It does
    not run without `ANTHROPIC_API_KEY` and is not part of the tests.
- Judged assessments (server, off by default). A server with a judge asks
  it about the flagged `title` and `description` changes and assesses again
  with the answers. A judge never fails a call: a field it did not answer
  keeps the engine's result and gets the status `unavailable` with a short
  detail, such as `judge timed out after 15 s` or `spend cap reached`.
  - `assessment_grpc::flow`: `assess_judged`, `Flow`, `JudgeSettings`,
    `JudgeReport`, `Judged` and `FlowError`. The service and the command
    line share it.
  - `AssessmentService::with_judge`, `with_call_timeout` and
    `with_shutdown`. `Assess`, `AssessBatch`, `AssessStream`, `PutEntity`
    and `CompareVersions` are judged. `Import` and `CompareText` never are.
    `options.judge = false` gives the engine's own result. A call with
    `options.judge = true` on a server without a judge gets `unavailable`
    with `this server has no judge`.
  - One call asks about at most 4 fields and one batch about at most 16.
    Texts longer than 8000 characters are not sent. The judge gets the time
    the call's deadline leaves, and at most 15 s. The second passes of a
    batch run inside the same per-call limit as the first passes.
  - A stale judgment from the caller is not used: the judge is asked
    about that field, and without an answer the field is `unavailable`
    with a detail.
  - A flagged `PutEntity` is judged before it is stored, and the version
    record keeps the judgments. A put is stored when the judge fails.
  - `GetPolicy` returns the judge id. Each judged call writes one log line
    with the counts, the tokens and the estimated cost.
  - Server flags `--judge off|on|auto` (`ASSESSMENT_JUDGE`, default `off`),
    `--judge-default`, `--judge-model`, `--judge-effort`,
    `--judge-timeout-secs`, `--judge-max-fields-per-request`,
    `--judge-max-fields-per-batch`, `--judge-concurrency`,
    `--judge-max-calls-per-minute`, `--judge-max-usd-per-hour`,
    `--judge-max-text-chars`, `--judge-max-tokens`, `--judge-cache-entries`,
    `--judge-fallbacks`, `--judge-base-url` and
    `--judge-allow-unauthenticated`, each with an `ASSESSMENT_JUDGE_*`
    variable. The API key is read only from `ANTHROPIC_API_KEY` or from
    `--anthropic-api-key-file` (`ANTHROPIC_API_KEY_FILE`). There is no key
    flag.
  - Startup checks: `--judge on` needs a key, and `auto` runs without a
    judge when there is none. A judge without `--auth-token` on an address
    that is not loopback is refused unless
    `--judge-allow-unauthenticated` is set. A plain `http://` base URL is
    refused unless it is loopback.
  - `assessment-engine assess FILE|- --judge` asks the judge from the
    command line and prints one summary line on stderr.
    `compare-text --field description|title` prints `gray_zone` for that
    field.
  - Cargo feature `judge` of `assessment-server` (on by default). A binary
    built without it has no HTTP client and refuses `--judge on`.
  - The container image now has `ca-certificates`.
- Judgment docs, examples and CI checks.
  - `docs/SCORING.md` §2a describes the gray zone, the fingerprint (with
    test vectors), the relations, the formulas, the propagation shares and
    the confidence. `docs/JUDGE.md` describes the server judge: where the
    prompt lives, the flags, the limits, the failure details and the threat
    model. The README has a "Judgments" section.
  - `examples/requests/paraphrase.core.json` and
    `paraphrase_judged.core.json`, with the proto JSON twins
    `paraphrase.json` and `paraphrase_judged.json`. A test checks the
    fingerprint in them and that the judged one is a patch.
  - CI job `features`: clippy for `assessment-grpc` without features, with
    the client only and with the server only, for `assessment-judge`
    without the Anthropic client and for `assessment-server` without the
    judge. It also checks that `assessment-core` depends on `chrono`,
    `serde`, `serde_json` and `thiserror` only, and that a client-only
    build and a judge-less server pull in no HTTP client or TLS crate.
  - The Docker job checks that the image has a CA bundle and that
    `assess --judge` without a network returns the engine's own result
    with the status `unavailable`.
  - The paid evaluation of the judge against the real API has not been run.
    The cost and latency figures in the README are estimates.
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

- A flagged text change that has no judgment keeps its bump and its score.
  Its confidence is multiplied by `judge.unjudged_confidence` (0.85), and the
  explanation and the summary say the meaning was not verified. Seven of
  the 24 golden files were re-blessed, each because its description is now
  flagged. In each the bump, the score and the impacts are the same. The
  confidence is lower, the field has a `judgment` block with the status
  `requested`, the texts say "meaning not verified", and there is one
  `judgment_requests` entry.
  - `text_paraphrase`: `paraphrase_suspected`. Confidence 0.9 → 0.765.
  - `text_postgres_mongo`: `term_substitution` (PostgreSQL → MongoDB).
    Confidence 0.9 → 0.765.
  - `text_flexible_fixed`: `term_substitution` (flexible → fixed).
    Confidence 0.9 → 0.765.
  - `text_except_paypal`: `scope_qualifier` (added "except").
    Confidence 0.9 → 0.765.
  - `text_login_logout`: `term_substitution` and `near_threshold`.
    Confidence 0.7 → 0.595.
  - `text_http_https`: `term_substitution`. Confidence 0.7 → 0.595.
  - `text_mongolian_negation`: `term_substitution`. Confidence 0.9 → 0.765.
    The rules do not read Mongolian: the changed word is seen as a swapped
    term.

  The other 17 files are byte-identical. With `judge.enabled = false` all
  24 match the outputs from before judgments, which are kept in
  `tests/golden_pre_m4/` and checked by a test.
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

- Rust struct literals of `ChangeSet`, `AssessOptions`, `PutOptions`,
  `FieldAssessment`, `Assessment`, `TextDiff`, `VersionRecord` and `Policy`
  need the new fields (or `..Default::default()` where there is one). The
  new JSON fields are optional, so journals and snapshots written by 0.1.0
  and later still load.
- Text judgments change no bump, score or impact unless a judgment is
  supplied or a server judge is on. A flagged text field has a lower
  confidence and new output fields (see Changed). `judge.enabled = false`
  in the policy gives the earlier outputs byte for byte.
- A server started without `--judge` behaves as before and makes no
  outgoing call. Callers that never set `options.judge` or send judgments
  need no change.
- The engine version is still 0.1.0. To find out whether a server has a
  judge, read `GetPolicyResponse.judge`, not the version.
- The default build of `assessment-server` now needs a C compiler (for
  the judge's TLS stack). `--no-default-features` builds without it.
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
