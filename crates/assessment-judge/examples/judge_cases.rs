//! The evaluation of the model judge against a real provider.
//!
//! It sends six flagged text changes to a judge and compares the answers
//! with the expected relations. It never runs in tests or CI.
//!
//! `JUDGE_PROVIDER` picks the provider. The default is `anthropic`.
//!
//! * `anthropic`: the paid Anthropic API. It costs real money (a few
//!   cents; it stops at 0.30 US dollars). Without a key it refuses to run.
//!
//!   ```text
//!   ANTHROPIC_API_KEY=... cargo run -p assessment-judge --example judge_cases
//!   ```
//!
//! * `claude-cli` or `codex-cli`: the Claude Code or the Codex command
//!   line that is installed and logged in on this machine. No key is read.
//!   The calls use the quota of the logged-in account. The cost is not
//!   measured. The time of each call is printed.
//!
//!   ```text
//!   JUDGE_PROVIDER=codex-cli cargo run -p assessment-judge --example judge_cases
//!   ```
//!
//!   `codex-cli` sends the texts to OpenAI, not to a Claude model.
//!   `JUDGE_CLI_PATH` names the program when it is not in `PATH`.
//!
//! `JUDGE_MODEL` picks another model. The default is `claude-opus-5-5`,
//! and the program's own model with `codex-cli`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use assessment_core::model::TextRelation;
use assessment_judge::price::cost_usd;
use assessment_judge::{
    AnthropicConfig, AnthropicJudge, ApiKey, CliConfig, CliJudge, CliKind, DEFAULT_MODEL, Judge,
    LimitedJudge, Limits, Question, ask,
};
use tokio::time::Instant;

/// The most this run may spend, in US dollars.
const BUDGET_USD: f64 = 0.30;
const TIMEOUT: Duration = Duration::from_secs(60);

struct Case {
    name: &'static str,
    field: &'static str,
    before: &'static str,
    after: &'static str,
    expected: TextRelation,
}

const CASES: [Case; 6] = [
    Case {
        name: "paraphrase",
        field: "description",
        before: "Users can log in with their email address and password.",
        after: "People are able to sign in using e-mail and a password.",
        expected: TextRelation::Equivalent,
    },
    Case {
        name: "postgres_mongo",
        field: "description",
        before: "Store user sessions in PostgreSQL with a 30 day expiry.",
        after: "Store user sessions in MongoDB with a 30 day expiry.",
        expected: TextRelation::Replaced,
    },
    Case {
        name: "login_logout",
        field: "title",
        before: "Build the login button",
        after: "Build the logout button",
        expected: TextRelation::Replaced,
    },
    Case {
        name: "flexible_fixed",
        field: "description",
        before: "The launch date is flexible and can move if needed.",
        after: "The launch date is fixed and can move if needed.",
        expected: TextRelation::Reversed,
    },
    Case {
        name: "except_paypal",
        field: "description",
        before: "Support all payment methods at checkout.",
        after: "Support all payment methods at checkout except PayPal.",
        expected: TextRelation::Narrowed,
    },
    Case {
        name: "http_https",
        field: "title",
        before: "Call the API over http",
        after: "Call the API over https",
        expected: TextRelation::Replaced,
    },
];

#[tokio::main]
async fn main() -> ExitCode {
    let provider = std::env::var("JUDGE_PROVIDER").unwrap_or_else(|_| "anthropic".to_owned());
    match provider.as_str() {
        "anthropic" => run_api().await,
        "claude-cli" => run_cli(CliKind::ClaudeCode).await,
        "codex-cli" => run_cli(CliKind::Codex).await,
        _ => {
            eprintln!("JUDGE_PROVIDER must be anthropic, claude-cli or codex-cli.");
            ExitCode::from(2)
        }
    }
}

/// The six cases through a command line that is logged in on this
/// machine. No key is read and no cost is measured.
async fn run_cli(kind: CliKind) -> ExitCode {
    let mut config = CliConfig::new(kind);
    config.program = std::env::var_os("JUDGE_CLI_PATH").map(Into::into);
    config.model = match (std::env::var("JUDGE_MODEL"), kind) {
        (Ok(model), _) => Some(model),
        (Err(_), CliKind::ClaudeCode) => Some(DEFAULT_MODEL.to_owned()),
        (Err(_), CliKind::Codex) => None,
    };
    let judge = match CliJudge::new(config) {
        Ok(judge) => judge,
        Err(error) => {
            eprintln!("cannot set up the judge: {error}");
            return ExitCode::from(2);
        }
    };
    let Some(program) = judge.program() else {
        eprintln!(
            "The `{}` program was not found. Install it and log in, or set JUDGE_CLI_PATH.",
            kind.default_program()
        );
        return ExitCode::from(2);
    };
    println!(
        "judge: {}; program: {}; cost: not measured (the calls use the logged-in account)",
        judge.id(),
        program.display()
    );
    let mut passed = 0;
    let mut answered = 0_u32;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let started = Instant::now();
    for case in &CASES {
        let question = Question::new(case.field, "issue", case.before, case.after);
        let asked = Instant::now();
        let result = ask(&judge, &question, asked + TIMEOUT).await;
        let secs = asked.elapsed().as_secs_f64();
        match result {
            Ok(verdict) => {
                answered += 1;
                input_tokens += verdict.usage.input_tokens;
                output_tokens += verdict.usage.output_tokens;
                let ok = verdict.relation == case.expected;
                passed += usize::from(ok);
                println!(
                    "{} {:<15} expected {:<12} got {:<12} confidence {:.2} \
                     tokens {}+{} {secs:.1} s [{}]\n     {}",
                    if ok { "ok  " } else { "FAIL" },
                    case.name,
                    case.expected.as_str(),
                    verdict.relation.as_str(),
                    verdict.confidence,
                    verdict.usage.input_tokens,
                    verdict.usage.output_tokens,
                    verdict.source,
                    verdict.rationale,
                );
            }
            Err(error) => println!(
                "FAIL {:<15} expected {:<12} error after {secs:.1} s: {error}",
                case.name,
                case.expected.as_str()
            ),
        }
    }
    let total = started.elapsed().as_secs_f64();
    println!(
        "{passed}/{} cases passed; {answered} answered; {total:.1} s in all, {:.1} s per case; \
         tokens {input_tokens}+{output_tokens}; cost not measured",
        CASES.len(),
        total / CASES.len() as f64
    );
    if passed == CASES.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The six cases through the paid Anthropic API.
async fn run_api() -> ExitCode {
    let Ok(raw_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!(
            "ANTHROPIC_API_KEY is not set. This example calls the paid Anthropic API \
             and does not run without a key."
        );
        return ExitCode::from(2);
    };
    let key = match ApiKey::new(&raw_key) {
        Ok(key) => key,
        Err(error) => {
            eprintln!("ANTHROPIC_API_KEY cannot be used: {error}");
            return ExitCode::from(2);
        }
    };
    drop(raw_key);

    let mut config = AnthropicConfig::default();
    if let Ok(model) = std::env::var("JUDGE_MODEL") {
        config.model = model;
    }
    let limits = Limits {
        usd_per_hour: BUDGET_USD,
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        timeout: TIMEOUT,
        ..Limits::default()
    };
    let model = config.model.clone();
    let anthropic = match AnthropicJudge::new(key, config) {
        Ok(judge) => Arc::new(judge),
        Err(error) => {
            eprintln!("cannot set up the judge: {error}");
            return ExitCode::from(2);
        }
    };
    let judge = LimitedJudge::new(anthropic, limits);

    println!("model: {model}; budget: ${BUDGET_USD:.2}");
    let mut passed = 0;
    let mut spent = 0.0;
    for case in &CASES {
        let question = Question::new(case.field, "issue", case.before, case.after);
        match ask(&judge, &question, Instant::now() + TIMEOUT).await {
            Ok(verdict) => {
                let cost = cost_usd(&verdict.source, &verdict.usage);
                spent += cost;
                let ok = verdict.relation == case.expected;
                passed += usize::from(ok);
                println!(
                    "{} {:<15} expected {:<12} got {:<12} confidence {:.2} \
                     tokens {}+{} ${cost:.4} [{}]\n     {}",
                    if ok { "ok  " } else { "FAIL" },
                    case.name,
                    case.expected.as_str(),
                    verdict.relation.as_str(),
                    verdict.confidence,
                    verdict.usage.input_tokens,
                    verdict.usage.output_tokens,
                    verdict.source,
                    verdict.rationale,
                );
            }
            Err(error) => {
                if let Some(usage) = error.usage() {
                    spent += cost_usd(&model, &usage);
                }
                println!(
                    "FAIL {:<15} expected {:<12} error: {error}",
                    case.name,
                    case.expected.as_str()
                );
            }
        }
    }
    println!(
        "{passed}/{} cases passed; about ${spent:.4} spent",
        CASES.len()
    );
    if passed == CASES.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
