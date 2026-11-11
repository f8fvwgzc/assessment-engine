# AGENTS.md

Instructions for coding agents (Codex, Claude Code and others) that work in
this repository. People should start with [README.md](README.md). The setup
of Codex and ready prompts are in [docs/CODEX.md](docs/CODEX.md).

## What this project is

`assessment-engine` scores how much a change to a work item matters. A work
item is a project, an issue or a sub-issue from a tracker such as nexc. For
each change the engine returns a semantic version bump (major, minor,
patch or none), the other items the change affects with an action for each, the
plan conflicts, and a recommendation from "no action" to "full re-plan".

It is a Rust workspace. The engine is deterministic: the same request and
the same policy give the same answer. An optional judge asks a language
model about text changes that the engine cannot decide by itself. The judge
is off by default.

## Where things are

| Path | What |
|---|---|
| `crates/assessment-core` | The engine. No network, no async. `Engine`, `Registry`, `Policy`. |
| `crates/assessment-grpc` | Protobuf types, conversions, the tonic service, a typed client. |
| `crates/assessment-judge` | The optional model judge: Anthropic API, OpenAI API, Claude Code command line, Codex command line. |
| `crates/assessment-server` | The `assessment-engine` binary: gRPC server and offline commands. |
| `proto/assessment/v1/assessment.proto` | The gRPC contract. |
| `examples/requests/` | Example requests. `*.core.json` is for the `assess` command, the others are for `grpcurl`. |
| `docs/SCORING.md` | The math. |
| `docs/JUDGE.md` | The judge: prompt, flags, limits, threat model. |
| `docs/PERFORMANCE.md` | Measured timings. |

## Commands

Run these before you say a change is done. All three must pass.

```bash
cargo fmt --all
cargo clippy --locked --all --workspace --all-targets -- -D warnings
cargo test --locked --all --workspace
```

Other useful commands:

```bash
cargo run -p assessment-core --example nexc_scenario
cargo run -p assessment-server -- assess examples/requests/parent_rewrite.core.json
cargo run -p assessment-server -- compare-text --before "must support SSO" --after "must not support SSO"
cargo run -p assessment-server -- policy
cargo run --release -p assessment-core --example bench -- --quick
```

Tests in `assessment-judge`, `assessment-grpc` and `assessment-server` open
ports on `127.0.0.1`. A sandbox without network access refuses that, and
those tests then fail with `Operation not permitted`. Seven tests that
check that a child process was stopped run `ps`, and some sandboxes refuse
that too, with the same message (`assessment-judge` `tests/cli_process.rs`,
`assessment-server` `tests/cli.rs`). That is the sandbox, not the code. Run the tests with network access allowed, or ask the user to
run them. The tests of `assessment-core` need no network.

The first build downloads crates and needs a C compiler (for the TLS crate
of the judge). `cargo build -p assessment-server --no-default-features`
needs neither a C compiler nor TLS, and has no judge: the `--judge` flags
are still listed, and are refused when used.

## Rules

1. **`assessment-core` stays pure.** Its normal dependencies are exactly
   `chrono`, `serde`, `serde_json` and `thiserror` (`proptest` is a
   dev-dependency for tests). No network, no async runtime. CI fails when
   the normal dependencies change. Code that talks to a network or starts
   a program belongs in `assessment-judge`.
2. **Never read, print or log a secret.** This covers `ANTHROPIC_API_KEY`,
   `OPENAI_API_KEY`, the files that `ANTHROPIC_API_KEY_FILE` and
   `OPENAI_API_KEY_FILE` name, and `ASSESSMENT_AUTH_TOKEN`. Do not run
   `env` or `printenv` to look at them. Do not ask the user to paste a key
   into the chat. No flag takes an API key as its value, and none may be added. The
   server token has the flag `--auth-token`; prefer the variable
   `ASSESSMENT_AUTH_TOKEN`, so that the token is not in the process list.
3. **Do not call a paid API by yourself.** Do not run the evaluation
   (`--example judge_cases`) or `--judge` with the provider `anthropic` or
   `openai` unless the user asks for that run. Tests use a local server
   and never the real API.
4. **The golden files are a contract.** `crates/assessment-core/tests/golden/`
   holds exact outputs. Do not set `GOLDEN_BLESS` to make a test pass. If
   a change is meant to move a score, say so, bless, show the diff to the
   user and list it in `CHANGELOG.md`.
5. **English rules only.** Do not add word lists, tokenizers or rules for
   another language. Input in any language is accepted. All output text is
   English. The default policy already holds a few Mongolian terms (in
   `crates/assessment-core/src/policy.rs`). Leave them as they are and add
   no more.
6. **Add dependencies with `cargo add`**, not by editing `Cargo.toml` by
   hand. Keep `Cargo.lock` committed. Every cargo build, clippy, test, check
   and tree command in CI uses `--locked`.
7. **The minimum Rust version is 1.88.** `rust-toolchain.toml` pins a newer
   compiler for development. Do not use a language or library feature that
   is newer than 1.88.
8. **The gRPC contract only grows.** Add fields and messages with new
   numbers. Never change or reuse a field number.
9. **No version bump, no tag and no release** unless the user asks.
10. **Keep the documents true.** A change to a flag, a default or a
    behaviour also changes `README.md`, the matching file in `docs/` and
    `CHANGELOG.md` (section `[Unreleased]`). Do not write that something
    was measured or tested when it was not.
11. **Out of scope.** No external graph database. No second process. No
    decision modelling: that lives in other projects.

## How to work

- Read the code before you change it. Each crate has its tests next to it
  in `tests/`.
- Write a test that fails first for a bug fix.
- Keep a change small and say what you did not do.
- Write documents and comments in plain, short English sentences.
- Commit messages follow `feat: ...`, `fix: ...`, `docs: ...`.
