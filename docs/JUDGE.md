# The model judge

The engine flags text changes whose meaning its word counts cannot settle
([SCORING.md](SCORING.md) §2a). A *judge* answers how the two texts of such
a field relate. This file describes the judge that ships with the server.
It is optional and off by default. `assessment-core` never calls it: the
server asks the judge and hands the answers to the engine as plain
judgments.

There are three providers. `anthropic` is the default: it asks the
Anthropic Messages API and needs an API key. `claude-cli` and `codex-cli`
run an agent command line that is installed and logged in on the same
machine, and need no key. They are described in
[Command line providers](#command-line-providers). Everything else in this
file holds for all three unless it says otherwise.

Status: the judge is tested against local fake servers and fake programs
only. The paid evaluation against the real API
(`cargo run -p assessment-judge --example judge_cases`, six cases, at most
0.30 US dollars) has not been run yet. The cost and latency numbers of the
API below are estimates. The Claude Code command line has not been run for
real at all. The Codex command line ran the same six cases once on
2026-11-10 (Codex 0.160.0, its default model, a free ChatGPT login): all
six got the expected relation. See
[Command line providers](#command-line-providers).

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

A command line provider gets the same instructions, the same JSON object
and the same schema, and its answer is read by the same strict parser.
`PROMPT_VERSION` is the same for all providers.

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
| `--judge off\|on\|auto` | `ASSESSMENT_JUDGE` | `off` | `on` needs a key, or the program of a command line provider. `auto` runs without a judge when that is missing. |
| `--judge-provider anthropic\|claude-cli\|codex-cli` | `ASSESSMENT_JUDGE_PROVIDER` | `anthropic` | Who answers. See [Command line providers](#command-line-providers). |
| `--judge-cli-path` | `ASSESSMENT_JUDGE_CLI_PATH` | `claude` or `codex` | The program of a command line provider. A bare name is looked up in `PATH`. |
| `--judge-default on\|off` | `ASSESSMENT_JUDGE_DEFAULT` | `on` | Whether a call that does not set `options.judge` is judged. |
| `--judge-model` | `ASSESSMENT_JUDGE_MODEL` | `claude-opus-5-5` | The model. With `codex-cli` the default is the program's own model. |
| `--judge-effort` | `ASSESSMENT_JUDGE_EFFORT` | `low` | `low`, `medium`, `high`, `xhigh` or `max`. A command line provider takes only the first three. |
| `--judge-timeout-secs` | `ASSESSMENT_JUDGE_TIMEOUT_SECS` | 15 | Longest time for one judge call (1 to 600). |
| `--judge-max-fields-per-request` | `ASSESSMENT_JUDGE_MAX_FIELDS_PER_REQUEST` | 4 | Most fields of one request that are asked. |
| `--judge-max-fields-per-batch` | `ASSESSMENT_JUDGE_MAX_FIELDS_PER_BATCH` | 16 | Most fields of one batch that are asked. |
| `--judge-concurrency` | `ASSESSMENT_JUDGE_CONCURRENCY` | 8, or 2 with a command line provider | Judge calls at once. |
| `--judge-max-calls-per-minute` | `ASSESSMENT_JUDGE_MAX_CALLS_PER_MINUTE` | 60 | Rate limit. |
| `--judge-max-usd-per-hour` | `ASSESSMENT_JUDGE_MAX_USD_PER_HOUR` | 10 | Spend cap, from the price table. It has no effect with a command line provider. |
| `--judge-max-text-chars` | `ASSESSMENT_JUDGE_MAX_TEXT_CHARS` | 8000 | Longer texts are not sent. |
| `--judge-max-tokens` | `ASSESSMENT_JUDGE_MAX_TOKENS` | 4096 | Largest answer. Only `anthropic`. |
| `--judge-cache-entries` | `ASSESSMENT_JUDGE_CACHE_ENTRIES` | 4096 | Answers kept in memory. |
| `--judge-fallbacks auto\|on\|off` | `ASSESSMENT_JUDGE_FALLBACKS` | `auto` | Whether the API may answer with another model when the first declines. `auto` is on for the default model only. Only `anthropic`. |
| `--judge-base-url` | `ASSESSMENT_JUDGE_BASE_URL` | `https://api.anthropic.com` | The API root. Only `anthropic`. |
| `--judge-allow-unauthenticated` | `ASSESSMENT_JUDGE_ALLOW_UNAUTHENTICATED` | off | See the threat model. |
| `--anthropic-api-key-file` | `ANTHROPIC_API_KEY_FILE` | none | A file that holds the key. Only `anthropic`. |

The key comes from `ANTHROPIC_API_KEY` or from the key file. The file wins.
A key file that cannot be read or is empty is an error, also with `auto`.
There is no flag that takes the key itself, so it never shows in a process
list.

A flag that is marked "Only `anthropic`" is refused at startup with a
command line provider, also with `--judge auto` and also when it comes
from its environment variable. `--judge-cli-path` is refused with
`anthropic`. Nothing is checked with `--judge off`.

`assess --judge` takes the provider, program path, model, effort, timeout,
fields per request, tokens, fallbacks, base URL and key file flags. It
asks the provider directly, without the cache and the limits, and each
judge call has the whole timeout. It prints one line on stderr:

    judge: 1 flagged, 1 judged (claude-opus-5-5, 2100/180 tokens, ~$0.0120)

The numbers in that line are made up for this example. With a command line
provider the line names the judge id and has no price:

    judge: 1 flagged, 1 judged (codex-cli/default, 14179/52 tokens, cost not measured)

That line is from one real Codex run through `assess --judge`.

## Command line providers

`claude-cli` runs the Claude Code command line (`claude`). `codex-cli`
runs the Codex command line (`codex`). Each call starts the program once,
gives it one question and reads one answer.

`codex-cli` is not a Claude model. It was added because the user of this
project asked for it: they have a Codex login and no Anthropic API key.

```bash
codex login                                   # once, as the user that runs the engine
assessment-engine assess request.json --judge --judge-provider codex-cli
assessment-engine serve --judge on --judge-provider codex-cli --addr 127.0.0.1:50061
```

**Who it is for.** A developer machine, or a host with one tenant where
the operator owns the logged-in account. It is not for the Docker image:
the image holds neither program and no login. `--judge on` does not start
when the program is not found. `--judge auto` then runs without a judge.

**No key.** With a command line provider the engine does not read
`ANTHROPIC_API_KEY` or a key file. The program uses its own login.

**The environment of the program.** The program does not get the
environment of the engine. It starts with an empty one and gets a short
list of variables, when they are set:

* where it finds itself and its login: `HOME`, `PATH`, `USER`, `LOGNAME`,
  `SHELL`, `TMPDIR`, the `XDG_*` directories, and `CLAUDE_CONFIG_DIR`
  (Claude Code only) or `CODEX_HOME` (Codex only);
* language and time: `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`;
* the network: `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` (also
  in lower case), `SSL_CERT_FILE`, `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS`;
* on Windows the variables with the same roles, such as `USERPROFILE`,
  `APPDATA` and `SYSTEMROOT`.

The list is `cli::PASSED_ENV` in the code. Nothing else is passed on. So
`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`,
`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`,
`CLAUDE_CODE_USE_VERTEX`, `OPENAI_API_KEY`, cloud credentials,
`ASSESSMENT_AUTH_TOKEN` and every other secret of the host do not reach
the program. The program must be logged in through its own login store
(`claude` then `/login`, or `codex login`). A login that exists only as a
variable is not used. A proxy address is passed on because the program
needs it to reach its service. If it holds a password, the program sees
it.

Codex also gets `-c shell_environment_policy.inherit=none`, so a command
that Codex runs in its sandbox does not get the variables above either.
This was checked once with Codex 0.160.0: a command in the sandbox had no
`PATH` from the engine. A login shell still reads the profile files of
the engine's user, which are files that user can read.

**What leaves the process, and to whom.** The same content as with the
API: the two texts, the field name, the entity kind, the context title and
the four counts. With `claude-cli` it goes to Anthropic, under the account
that is logged in to Claude Code. With `codex-cli` it goes to OpenAI,
under the account that is logged in to Codex. The terms and the data
handling of that account apply, not those of an API key. The program adds
its own instructions to each call. The engine does not control or see
them.

**How the program is run.**

* The texts go to the program on its standard input only. They are never
  part of the arguments, and no shell is used.
* The program runs in an empty directory that the engine makes for it, with
  access for the owner only. It is never the repository or the server's
  directory. Each call has a new directory, and it is removed when the
  call ends. Nothing of the judge lies in the temporary directory between
  calls, so a cleaner of old temporary files cannot break a server that
  runs for days.
* Claude Code runs with `--tools ""`, without MCP servers, without
  settings files, without slash commands and without a saved session.
* Codex runs with `--sandbox read-only`, `--ephemeral`, `--ignore-rules`
  and `--ignore-user-config`. Codex cannot be run without any tool: it
  keeps a shell, in a sandbox that cannot write. See the threat model.
* The answer must fit the same schema. An answer with another key, an
  unknown relation or a confidence outside 0 to 1 is not used.
* The program is stopped when the deadline passes or the call is dropped.
  The engine stops the whole process group, so helper processes end too.
  This matters for Codex, whose `codex` file is a launcher that starts
  the real program. The signal is sent before the call returns. It does
  not wait for a later task, so it also happens when `assess --judge`
  ends right after a timeout and when the server shuts down.
* Ctrl-C (SIGINT) and SIGTERM during `assess --judge` stop the program
  and its helpers, remove the directory of the call and end the command
  with an error. Nothing is printed on standard output then. The program
  has a process group of its own, so the terminal's Ctrl-C would not
  reach it otherwise.
* Two cases are not covered. When the engine itself is killed with
  SIGKILL, it can do nothing: the program runs until its call ends, and
  the directory of that call stays in the temporary directory. A helper
  that leaves the process group on purpose is not stopped.
* At most 1 MiB of output is read. A program that writes more is stopped.
* A failed call is not tried again.

**Limits.** The cache, the calls at once, the calls per minute and the
breaker work as with the API. The default for calls at once is 2, because
each call is a process. The spend cap does not bind: a call is not billed
per token, so its cost is counted as 0 and the log line shows 0. The rate
limits of the subscription apply and the engine does not know them. When
the program reports that it is not logged in or that a usage limit is
reached, the judge pauses for 300 s at once.

**Time and result.** The six evaluation cases ran once through Codex
0.160.0 on 2026-11-10, with its default model and a free ChatGPT login.
All six got the expected relation, with confidence 0.95 to 0.99. A case
took 3.9 to 5.2 seconds, 4.5 seconds on average, one after another. Codex
reported about 13 800 input tokens per case, most of them its own
instructions. This is one run of six short cases, not a benchmark. The default timeout of 15 s is unchanged. A server that judges
4 fields with 2 calls at once needs two rounds, so send a deadline that
allows for it or raise `--judge-timeout-secs`.

**Not covered.** There is no startup probe for a command line provider: a
probe would start the program and use quota. A login that has expired
shows at the first judged call, as `unavailable` with a detail. The
providers are tested on Unix with fake programs. They are not tested on
Windows.

The evaluation example runs through a command line provider too:

```bash
JUDGE_PROVIDER=codex-cli cargo run -p assessment-judge --example judge_cases
```

It reads no key, prints the time of each case and the totals, and says
that the cost is not measured. It makes six real calls. The run above
printed `6/6 cases passed; 6 answered; 26.8 s in all, 4.5 s per case`.

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
  A command line provider is not tried again.
* Cache: 4096 answers, keyed by the judge id, the prompt version and the
  question. Equal questions that arrive together share one call. A cached
  answer is free and is used even when a limit is reached.
* Rate and spend: 8 calls at once (2 with a command line provider), 60
  per minute, 10 US dollars per hour (no effect with a command line
  provider).
  The cost is reserved before a call and corrected after it. A question
  waits for a free place as long as its call could still start inside its
  deadline, so a batch of 16 fields runs as two rounds of 8.
* Batches: the second passes of one `AssessBatch` run a few at a time,
  inside the same per-call limit as the first passes. The reserve for the
  second pass counts the whole first pass, with the time its items waited.
* Breaker: five failures in a row pause the judge for 30 s. One answer
  401, 403 or 404 (a bad key, an unknown model) pauses it for 300 s at
  once. Three identical 400 or 402 answers in a row do the same. So does
  one command line call that reports a missing login or a usage limit.
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
| `claude CLI not found`, `codex CLI not found` | The program was removed after startup. |
| `… CLI is not logged in` | The program reported that its login is missing or has expired. |
| `… CLI hit a usage limit` | The program reported a rate or usage limit of the account. |
| `… CLI reported an error`, `… CLI exited with status N`, `… CLI was stopped by a signal`, `… CLI could not be started`, `… CLI output could not be read` | The program failed. The text it printed is not copied into the detail. |
| `the … CLI wrote more than N bytes` | The program wrote more than the output cap. |
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
  flag. It is kept out of `Debug` output, errors, logs and details. With
  a command line provider it is not read, and the program does not get
  it: its environment is a short fixed list, not the environment of the
  engine.
* **Who can spend.** Every judged call costs money, or quota of the
  logged-in account with a command line provider. A server with a judge
  on an address that is not loopback must have `--auth-token`, or it does
  not start. `--judge-allow-unauthenticated` lifts this for a network
  that is already closed. The per-request, per-minute and per-hour limits
  bound what one caller can spend. With a command line provider only the
  per-request and per-minute limits bound it.
* **Where the text goes.** With `anthropic`, only to `--judge-base-url`.
  Redirects are not followed. Plain `http://` is refused unless the
  address is loopback. At startup one `GET /v1/models/{model}` checks the
  key and the model. It costs nothing and sends no text. With a command
  line provider the text goes to the program, and the program sends it to
  its own service: Anthropic for `claude-cli`, OpenAI for `codex-cli`.
  The engine does not choose or check that address. It does keep the
  variables that would change the address or the account away from the
  program (see "The environment of the program"). The program's own
  configuration files can still name another endpoint for Claude Code:
  the engine turns off its settings files, not its login store. Codex
  runs with `--ignore-user-config`.
* **An agent reads hostile text.** A command line provider is an agent
  program, not a plain API call. Claude Code is run without tools, so the
  text can change only the answer. Codex cannot be run without tools: it
  keeps a shell in a read-only sandbox. Hostile text that the model obeys
  could make Codex read files that the engine's user can read and put up
  to 400 characters of them into the rationale, which goes back to the
  caller. Those file contents would also reach OpenAI. The sandbox stops
  writes. The environment of the engine is a second place for secrets,
  and an easier one, because no file path has to be guessed. So the
  engine does not pass it on: the program gets a short list of variables
  without keys or tokens, and a command in the Codex sandbox gets none of
  them. A secret in a variable of the server, such as a database
  password, is not there to be read. Files stay readable: this includes
  the shell profile files of the engine's user, and a secret that is
  exported there. The empty working directory, the ignored user
  configuration and the instructions make this harder. They do not make
  it impossible. Run `codex-cli` only where the engine's user can read
  nothing that a person who edits issues must not see. This is the main reason the command line
  providers are for a developer machine or a host with one tenant.
* **The program is trusted.** The engine runs the program that
  `--judge-cli-path` or `PATH` names, as the engine's user. Whoever can
  change that file or `PATH` runs code as that user. The argument list is
  fixed. A model name that starts with `-` is refused, so it cannot be
  read as a flag.
* **Consent.** The server cannot know whether a user agreed to send a
  text to a model. The caller must send `options.judge = false` when they
  did not, or run with `--judge-default off` and opt in per call.
* **Failure.** A judge that is down, slow or wrong never blocks a write
  and never fails a call. The engine's own result is always there.
