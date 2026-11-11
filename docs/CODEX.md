# Working with Codex

This guide is for a person who clones this repository on a new computer and
works on it with the Codex command line. It covers two different uses of
Codex. Do not mix them up:

1. **Codex as your coding agent.** You talk to Codex, and Codex reads and
   changes this repository. Its rules are in [AGENTS.md](../AGENTS.md).
2. **Codex as the judge of the engine.** The engine starts the `codex`
   program by itself to ask how two texts relate
   (`--judge-provider codex-cli`). You do not talk to it.

There is a third way to use OpenAI models as the judge: the OpenAI API with
a key (`--judge-provider openai`). It does not use the Codex program at
all. It is at the end of this guide.

What was checked, and how, is in [What was tested](#what-was-tested).

## 1. Set up the computer

You need:

- **git**.
- **Rust**, installed with [rustup](https://rustup.rs). The file
  `rust-toolchain.toml` picks the compiler version by itself on the first
  build.
- **A C compiler** (`cc`, `clang` or `gcc`; on Windows the MSVC build
  tools). The TLS crate of the judge needs it.
- **Network access to crates.io** for the first build. Behind a company
  proxy set `HTTPS_PROXY` before you build.
- **The Codex command line**, logged in.

```bash
git clone https://github.com/f8fvwgzc/assessment-engine
cd assessment-engine
cargo fetch --locked
cargo build --release -p assessment-server
```

`cargo fetch` downloads every crate, also the ones only the tests need, so
that later builds and tests need no network. The binary is `target/release/assessment-engine`. Check it without any
model:

```bash
target/release/assessment-engine assess examples/requests/parent_rewrite.core.json
```

It prints one JSON document with `summary`, `changes`, `impacts` and
`recommendation`.

Then check Codex:

```bash
codex --version
codex login status
```

If you are not logged in, run `codex login` and follow the browser. Use the
account that your company allows. Do this yourself: an agent must not log
in for you.

## 2. Configure Codex as your coding agent

Codex reads [AGENTS.md](../AGENTS.md) at the root of the repository when it
starts there. That file holds the commands and the rules of this project.
You do not need to paste them.

Start Codex in the repository:

```bash
codex --sandbox workspace-write --ask-for-approval on-request
```

With these two options Codex may change files in the repository and asks
you before it does anything outside its sandbox.

### The sandbox and the tests

The sandbox of Codex blocks the network. Many tests in this project start a
small server on `127.0.0.1`, and the sandbox refuses that too. Inside the
sandbox:

- `cargo test -p assessment-core` passes.
- Tests of `assessment-judge`, `assessment-grpc` and `assessment-server`
  that open a port fail with `Operation not permitted`.
- Seven tests that check that a child process was stopped fail with the
  same message, because they run `ps` and the sandbox refuses it: five in
  `crates/assessment-judge/tests/cli_process.rs` and two in
  `crates/assessment-server/tests/cli.rs`.

This is not a bug in the code. You have two choices.

**Choice A: approve the test run when Codex asks.** Nothing to configure.
Codex asks to run `cargo test` outside the sandbox, and you say yes.

**Choice B: allow the network in the sandbox.** Put this in
`~/.codex/config.toml`:

```toml
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
network_access = true
```

Then the tests that open a port pass inside the sandbox. The seven `ps`
tests still fail there, so the full test run still needs choice A. Know
what you allow: this opens the
whole network to the commands Codex runs, not only `127.0.0.1`. On a
company computer ask whether that is permitted. If it is not, use choice A.

A build downloads crates the first time. Run `cargo fetch --locked`
yourself once before you start Codex (step 1 does this), so that Codex
never needs the network to build or to test.

### What Codex must not do here

These are in `AGENTS.md`, and they matter most on a company computer:

- It must not read or print an API key or the server token.
- It must not call a paid API unless you ask for that run.
- It must not tag, release or change the version unless you ask.

## 3. Use Codex as the judge

The engine decides most changes by itself. Some text changes it cannot
decide: a paraphrase, one word replaced by another, a new exception. It
flags them. With a judge on, the engine asks a model how the two texts
relate and assesses again with the answer.

With `codex-cli` the engine starts the `codex` program that is installed
and logged in on this computer. No API key is read.

```bash
target/release/assessment-engine assess examples/requests/paraphrase.core.json --judge --judge-provider codex-cli
```

The JSON goes to standard output. One line about the judge goes to standard
error, like this one from a real run:

```text
judge: 1 flagged, 1 judged (codex-cli/default, 14179/52 tokens, cost not measured)
```

As a server:

```bash
target/release/assessment-engine serve --judge on --judge-provider codex-cli --addr 127.0.0.1:50061
```

Options that matter with this provider:

| Flag | Environment variable | Meaning |
|---|---|---|
| `--judge-provider codex-cli` | `ASSESSMENT_JUDGE_PROVIDER` | Use the Codex program. |
| `--judge-cli-path <program>` | `ASSESSMENT_JUDGE_CLI_PATH` | Where `codex` is, when it is not in `PATH`. |
| `--judge-model <model>` | `ASSESSMENT_JUDGE_MODEL` | Another model. The default is the model Codex picks itself. |
| `--judge-effort <level>` | `ASSESSMENT_JUDGE_EFFORT` | `low` (default), `medium` or `high`. `xhigh` and `max` are refused with this provider. |
| `--judge-timeout-secs <n>` | `ASSESSMENT_JUDGE_TIMEOUT_SECS` | Longest time for one call. The default is 15. A Codex call takes several seconds, so raise it if fields come back `unavailable` with "timed out". |

Read these limits before you use it at work:

- **The texts go to OpenAI**, under the account that is logged in to
  Codex. For each flagged field that is: the two texts, the field name,
  the kind of the item, its title, and six counts of what changed (list items added, removed and
  modified; negation, requirement word and number changes). No ids, no other
  fields, no comments. Check that your company allows this for the data
  you assess. Send `options.judge = false` in a request to keep one call
  away from the model.
- **Your own Codex configuration is not used.** The engine starts Codex
  with `--ignore-user-config`, `--ignore-rules`, `--ephemeral` and
  `--sandbox read-only`, in an empty directory. `~/.codex/config.toml`
  is not loaded, and the `AGENTS.md` of a repository is not read, because
  the program runs in an empty directory. A global `AGENTS.md` in
  `~/.codex` was not checked: do not keep instructions there that should
  not reach the judge.
- **The login must be in the login store of Codex** (`~/.codex`, or the
  directory in `CODEX_HOME`). The engine does not pass `OPENAI_API_KEY`
  to the program.
- **A proxy is passed on.** `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`,
  `NO_PROXY`, `SSL_CERT_FILE`, `SSL_CERT_DIR` and `NODE_EXTRA_CA_CERTS`
  reach the program, so it works behind a company proxy with its own
  certificate.
- **No cost is measured** and the hourly spend cap has no effect. The
  limits of the account apply.
- **For a developer computer or a host with one tenant.** Codex cannot run
  without a shell tool. The engine gives it a sandbox that cannot write,
  but a text written to trick the model could still make it read files.
  Do not use this provider on a shared server. The Docker image does not
  hold the program.
- **A judge that does not answer never fails a call.** When the program
  is slow or logged out, the field keeps the engine's own result and gets
  the status `unavailable` with a short reason. When the program is not
  found at the start, `assess --judge` and `serve --judge on` stop with an
  error, and `serve --judge auto` runs without a judge.

Do not run the judge from inside a Codex session. The inner Codex needs the
network to reach OpenAI and the sandbox of the outer one blocks it. Run
judge commands in your own terminal.

The full description is in
[Command line providers](JUDGE.md#command-line-providers) and the
[threat model](JUDGE.md#threat-model).

### Check the judge with the six evaluation cases

```bash
JUDGE_PROVIDER=codex-cli cargo run -p assessment-judge --example judge_cases
```

It asks six questions with a known answer and prints how many came back
right. It uses the quota of your Codex login. `JUDGE_CLI_PATH` names the
program when it is not in `PATH`, and `JUDGE_MODEL` picks another model.

## 4. Prompts for Codex

Copy one of these into Codex, started in the repository. Each one says
what to do and when to stop. Text in `<...>` is yours to fill in.

### Get to know the project

```text
Read AGENTS.md, README.md and docs/SCORING.md. Do not change any file.
Then tell me in at most 20 lines: what this engine takes as input, what it
returns, which crate does what, and which three files I should read first
to understand how a text change becomes a version bump.
```

### Check that the clone works

```text
Do not change any file. Run, in this order:
  cargo fmt --all -- --check
  cargo clippy --locked --all --workspace --all-targets -- -D warnings
  cargo test --locked --all --workspace
If a test fails with "Operation not permitted", that is your sandbox
blocking a local port or the ps command: ask me for approval to run the tests outside the
sandbox, and do not report it as a bug. Report for each command: passed or
failed, and for a failure the test name and the first error line.
```

### Explain one assessment

```text
Run: cargo run -q -p assessment-server -- assess <path to a .core.json file>
Do not change any file. Explain the result to a project manager in plain
English: what changed, why the engine chose this version bump, which items
are affected and what should be done about each, and what the
recommendation is. Then name the functions in crates/assessment-core that
produced the bump and the recommendation.
```

### Compare two texts

```text
Run: cargo run -q -p assessment-server -- compare-text --before "<old text>" --after "<new text>"
Explain every measure in the output with docs/SCORING.md, and say whether
the result has a gray_zone and why (the key is missing when the change is
not in the gray zone, and --field decides when it is set). Do not change
any file.
```

### Write a request for my own data

```text
Read examples/requests/parent_rewrite.core.json and the types it maps to in
crates/assessment-core. Write a new file examples/requests/<name>.core.json
for this case: <describe the parent, its children, and the change>. Use
only fields that exist. Run it with
  cargo run -q -p assessment-server -- assess examples/requests/<name>.core.json
and show me the summary line and the recommendation. Change no other file.
```

### Fix a bug

```text
Bug: <what you did, what you got, what you expected>.
Follow AGENTS.md. First write a test that fails because of this bug and
show me that it fails. Then fix the code. Then run the three commands from
AGENTS.md. Do not set GOLDEN_BLESS. If a golden test changes, stop and
show me the diff. Add one line to CHANGELOG.md under [Unreleased]. Do not
commit.
```

### Add a feature

```text
Feature: <what it should do>.
Follow AGENTS.md. Before you write code, tell me your plan in at most 10
lines: which crate, which files, which tests, and whether the gRPC
contract or a golden file changes. Wait for my yes. Then implement it with
tests, update README.md, the matching file in docs/ and CHANGELOG.md, and
run the three commands from AGENTS.md. assessment-core must keep exactly
its four dependencies. Do not commit, tag or change the version.
```

### Change a number of the policy

```text
I want <for example: a change is "critical" only from score 0.9>.
Run: cargo run -q -p assessment-server -- policy
Find the field for this in the output, in README.md section "Tuning" and
in docs/SCORING.md. Do not change the code. Write a file
policy.patch.json with only that field, and show me the result of
  cargo run -q -p assessment-server -- assess examples/requests/parent_rewrite.core.json --policy policy.patch.json
next to the result without the patch. If the two results are equal, say
so and say why. Delete policy.patch.json at the end unless I ask to keep
it.
```

### Connect my tracker to the engine

```text
Read the section "Wiring it into nexc-engine" in README.md,
proto/assessment/v1/assessment.proto and
crates/assessment-grpc/examples/nexc_client.rs. Do not change this
repository. My system is <language, framework, how issues are stored>.
Write a short design: which of my fields map to which Entity fields, when
I call PutEntity, Assess and Import, what I store from the answer, and
what I show to the user for each recommendation. List what you are not
sure about as questions.
```

### Find out why a judged field is "unavailable"

```text
Do not change any file and do not read or print any API key.
Read docs/JUDGE.md, section "When the judge does not answer". I ran
<the command> and the field has the status "unavailable" with the detail
"<detail>". Tell me the most likely cause, and the one command I should
run myself in my own terminal to confirm it.
```

### Review before I commit

```text
Review the uncommitted changes against AGENTS.md: git status --short, git
diff, and the content of every new file that git does not track yet. Do not change
any file. Check: does assessment-core still have only its four
dependencies, could a secret be printed or logged, is a golden file
changed, is a proto field number reused, are README.md, docs/ and
CHANGELOG.md updated, is anything claimed as tested that was not. List
the findings with file and line, the worst first.
```

## 5. The OpenAI API as the judge

This is separate from the Codex program. It needs an API key from the
OpenAI platform. A ChatGPT login, which Codex uses, does not include API
access: API use is billed on its own.

Set the key in your own shell or in a file that only you can read. Never
give it to an agent, and never put it in a flag, a prompt or the
repository.

```bash
export OPENAI_API_KEY=...             # or --openai-api-key-file <file>, or OPENAI_API_KEY_FILE
target/release/assessment-engine assess examples/requests/paraphrase.core.json --judge --judge-provider openai
```

| Flag | Environment variable | Default |
|---|---|---|
| `--judge-provider openai` | `ASSESSMENT_JUDGE_PROVIDER` | `anthropic` |
| `--judge-model <model>` | `ASSESSMENT_JUDGE_MODEL` | `gpt-6.1-sol` |
| `--judge-base-url <url>` | `ASSESSMENT_JUDGE_BASE_URL` | `https://api.openai.com/v1` (the URL includes `/v1`) |
| `--openai-api-key-file <file>` | `OPENAI_API_KEY_FILE` | none; `OPENAI_API_KEY` is read without it |
| `--judge-max-usd-per-hour <usd>` | `ASSESSMENT_JUDGE_MAX_USD_PER_HOUR` | 10 (server only) |

`--judge-base-url` lets you point the judge at a company gateway or
another vendor that speaks the Chat Completions API. It must be `https`,
or `http` on a loopback address.

The details are in [The OpenAI provider](JUDGE.md#the-openai-provider).

## What was tested

Say this to your team as it is. Nothing here is better tested than this
list says.

| What | State |
|---|---|
| Engine, gRPC service, server, offline commands | Automated tests, run in CI on every change. |
| Judge through the Codex program | Six evaluation cases ran once, on 2026-11-10, with Codex 0.160.0 and a free ChatGPT login on macOS: six of six right, 4.5 seconds per case on average. One run, not a benchmark. Not run on Linux or Windows. |
| Judge through the OpenAI API | Tested against a local stand-in server only. Never run against the real API. Cost and latency are estimates. |
| Judge through the Anthropic API | Tested against a local stand-in server only. The paid evaluation has not been run. |
| Judge through the Claude Code program | Never run for real. |
| Tests inside the Codex sandbox | Checked once with `codex sandbox` 0.160.0 on macOS: the tests of `assessment-core` pass; the local-server tests fail without network access and pass with `network_access = true`; seven tests that run `ps` fail in both cases. |
| Codex as a coding agent on this repository | Not tried. `AGENTS.md` and the prompts above are written for it but no Codex session has used them yet. |
