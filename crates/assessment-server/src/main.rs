//! `assessment-engine`: the assessment engine's gRPC server and command line.
//!
//! * `assessment-engine [serve] [--addr …]` serves `assessment.v1` with
//!   health and reflection (the default when no subcommand is given).
//! * `assessment-engine assess <file|->`, `policy` and `compare-text` run the
//!   core engine directly, without a network, and print JSON. Only
//!   `assess --judge` uses the network: it asks a model about flagged texts.
//! * `assessment-engine health` asks a running server whether it serves
//!   (for container health checks).

mod commands;
mod health;
mod judge;
mod serve;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use assessment_core::Engine;
use assessment_grpc::service::MAX_SLOTS;
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

/// Scores how much a change to a versioned work item matters, for the item
/// and everything planned around it, and recommends whether to re-plan.
#[derive(Debug, Parser)]
#[command(
    name = "assessment-engine",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Serve options, when no subcommand is given.
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the gRPC API with health and reflection (the default).
    Serve(ServeArgs),
    /// Assess an `AssessmentRequest` (core JSON) and print the `Assessment`.
    Assess(AssessArgs),
    /// Print the effective policy as JSON.
    Policy(PolicyArgs),
    /// Compare two texts and print every measure as JSON.
    CompareText(CompareTextArgs),
    /// Ask a running server whether it serves; exits non-zero otherwise.
    Health(HealthArgs),
}

/// How to serve.
#[derive(Debug, Clone, Args)]
pub(crate) struct ServeArgs {
    /// The address to listen on.
    #[arg(long, env = "ASSESSMENT_ADDR", default_value = "0.0.0.0:50061")]
    pub addr: SocketAddr,

    /// Keep the registry in this append-only journal (replayed on start);
    /// without it the registry lives in memory.
    #[arg(long, env = "ASSESSMENT_JOURNAL", value_name = "PATH")]
    pub journal: Option<PathBuf>,

    /// Sync the journal to disk after every write.
    #[arg(long)]
    pub fsync: bool,

    /// Compact the journal into a snapshot once it holds this many
    /// operations, and on a clean shutdown (0 = never).
    ///
    /// A compaction writes the current state to `<journal>.snapshot` and
    /// starts a new, empty journal, so the next start does not replay the
    /// whole history. Without `--journal` this does nothing.
    #[arg(
        long,
        env = "ASSESSMENT_COMPACT_AFTER_OPS",
        value_name = "N",
        default_value_t = 100_000,
        value_parser = parse_compact_after_ops
    )]
    pub compact_after_ops: u64,

    /// Keep the content of only the N newest versions of each entity, at
    /// least 1 (default: all).
    ///
    /// Older versions stay in the history, but `GetEntity` and
    /// `CompareVersions` no longer answer for them. This bounds memory.
    #[arg(
        long,
        env = "ASSESSMENT_HISTORY_LIMIT",
        value_name = "N",
        value_parser = parse_history_limit
    )]
    pub history_limit: Option<usize>,

    /// A partial policy (JSON) merged over the default policy.
    #[arg(long, env = "ASSESSMENT_POLICY", value_name = "FILE")]
    pub policy: Option<PathBuf>,

    /// Require `authorization: Bearer <token>` on every engine call (health
    /// and reflection stay open).
    #[arg(long, env = "ASSESSMENT_AUTH_TOKEN", hide_env_values = true)]
    pub auth_token: Option<String>,

    /// Largest gRPC message decoded or encoded, in bytes.
    #[arg(long, env = "ASSESSMENT_MAX_MESSAGE_BYTES", default_value_t = 16 * 1024 * 1024)]
    pub max_message_bytes: usize,

    /// Deadline for each call, in seconds (0 = none).
    #[arg(long, env = "ASSESSMENT_REQUEST_TIMEOUT_SECS", default_value_t = 30)]
    pub request_timeout_secs: u64,

    /// Most calls served at once, across all connections; more wait.
    #[arg(long, env = "ASSESSMENT_CONCURRENCY_LIMIT", default_value_t = 256)]
    pub concurrency_limit: usize,

    /// Most requests in one `AssessBatch` call.
    #[arg(long, env = "ASSESSMENT_MAX_BATCH_SIZE", default_value_t = 1000)]
    pub max_batch_size: usize,

    /// Most assessments one batch or stream has started at once; each waits
    /// for its own slot (default: the number of CPUs).
    #[arg(long, env = "ASSESSMENT_MAX_IN_FLIGHT")]
    pub max_in_flight: Option<usize>,

    /// Most stateless assessments (`Assess`, `CompareText`, batch and stream
    /// items) the server runs at once, and separately most registry calls
    /// (`PutEntity`, `DeleteEntity`, `RecordActivity`, `CompareVersions`,
    /// and each chunk of an `Import`); at least 1 (default: the number of CPUs).
    ///
    /// More wait their turn. Unary and batch calls queue within the request
    /// timeout and fail with CANCELLED if it passes while they wait. Stream
    /// items queue with no server-side deadline. Work that has started runs
    /// to completion and holds its slot even if the caller has gone, and
    /// registry writes still commit.
    #[arg(
        long,
        env = "ASSESSMENT_MAX_CONCURRENT_ASSESSMENTS",
        value_name = "N",
        value_parser = parse_slots
    )]
    pub max_concurrent_assessments: Option<usize>,

    /// Log format (the level comes from `RUST_LOG`, default `info`).
    #[arg(long, env = "ASSESSMENT_LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,

    /// The optional judge (off by default).
    #[command(flatten)]
    pub judge: judge::JudgeArgs,
}

/// Parses a slot count: a whole number from 1 to [`MAX_SLOTS`].
fn parse_slots(text: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(n) if n > MAX_SLOTS => Err(format!("must be at most {MAX_SLOTS}")),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("must be a whole number from 1 to {MAX_SLOTS}")),
    }
}

/// Parses `--compact-after-ops`: a whole number, where 0 means never.
fn parse_compact_after_ops(text: &str) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| "must be a whole number (0 = never)".to_owned())
}

/// Parses `--history-limit`: a whole number, at least 1.
fn parse_history_limit(text: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(0) => Err("must be at least 1 (leave it out to keep every version)".to_owned()),
        Ok(n) => Ok(n),
        Err(_) => Err("must be a whole number, at least 1".to_owned()),
    }
}

/// How logs are written (to stderr).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogFormat {
    /// Human-readable lines.
    Pretty,
    /// One JSON object per line.
    Json,
}

#[derive(Debug, Args)]
struct AssessArgs {
    /// The request file (JSON in the core format), or `-` for stdin.
    #[arg(value_name = "FILE|-")]
    input: PathBuf,

    /// A partial policy (JSON) merged over the default policy.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,

    /// Ask a model about the flagged text changes. With the provider
    /// `anthropic` it needs an API key in `ANTHROPIC_API_KEY` or in
    /// `--anthropic-api-key-file`. With `--judge-provider openai` it
    /// needs one in `OPENAI_API_KEY` or in `--openai-api-key-file`. With
    /// `--judge-provider claude-cli` or
    /// `codex-cli` it needs no key, only the program, installed and
    /// logged in. A judge that fails never fails the command: the fields
    /// are then `unavailable`. Ctrl-C stops the judge and its program.
    #[arg(long)]
    judge: bool,

    /// The judge options. They do nothing without `--judge`.
    #[command(flatten)]
    client: judge::JudgeClientArgs,
}

#[derive(Debug, Args)]
struct PolicyArgs {
    /// A partial policy (JSON) merged over the default policy.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CompareTextArgs {
    /// The text before the change.
    #[arg(long)]
    before: String,

    /// The text after the change.
    #[arg(long)]
    after: String,

    /// The field the texts belong to. It decides when `gray_zone` is set.
    #[arg(long, value_enum, default_value_t = TextField::Description)]
    field: TextField,

    /// A partial policy (JSON) merged over the default policy.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
}

/// A text field of an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TextField {
    Description,
    Title,
}

impl TextField {
    fn name(self) -> &'static str {
        match self {
            TextField::Description => assessment_core::model::field::DESCRIPTION,
            TextField::Title => assessment_core::model::field::TITLE,
        }
    }
}

#[derive(Debug, Args)]
struct HealthArgs {
    /// The server to ask.
    #[arg(long, default_value = "http://127.0.0.1:50061")]
    endpoint: String,

    /// The service to ask about (`""` asks about the server as a whole).
    #[arg(long, default_value = "assessment.v1.AssessmentEngine")]
    service: String,

    /// How long to wait for the connection and the answer, in seconds.
    #[arg(long, default_value_t = 3)]
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => serve_command(cli.serve).await,
        Some(Command::Serve(args)) => serve_command(args).await,
        Some(Command::Assess(args)) => {
            init_tracing(LogFormat::Pretty, "warn")?;
            let engine = engine_with_policy(args.policy.as_deref())?;
            if args.judge {
                let stack = judge::for_cli(&args.client, judge::key_from_env)?;
                commands::assess_judged(engine, &args.input, stack).await
            } else {
                commands::assess(&engine, &args.input)
            }
        }
        Some(Command::Policy(args)) => {
            init_tracing(LogFormat::Pretty, "warn")?;
            let engine = engine_with_policy(args.policy.as_deref())?;
            commands::policy(&engine)
        }
        Some(Command::CompareText(args)) => {
            init_tracing(LogFormat::Pretty, "warn")?;
            let engine = engine_with_policy(args.policy.as_deref())?;
            commands::compare_text(&engine, args.field.name(), &args.before, &args.after)
        }
        Some(Command::Health(args)) => {
            let timeout = Duration::from_secs(args.timeout_secs.max(1));
            health::check(&args.endpoint, &args.service, timeout).await
        }
    }
}

async fn serve_command(args: ServeArgs) -> Result<()> {
    init_tracing(args.log_format, "info")?;
    let result = serve::run(args).await;
    if let Err(err) = &result {
        let causes: Vec<String> = err.chain().skip(1).map(ToString::to_string).collect();
        tracing::error!(error = %err, ?causes, "the server stopped with an error");
    }
    result
}

/// Logs to stderr, filtered by `RUST_LOG` (else `default_level`).
fn init_tracing(format: LogFormat, default_level: &str) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    let result = match format {
        LogFormat::Pretty => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
    result
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to install the log subscriber")
}

/// The default engine, with the policy patch in `path` merged over it.
pub(crate) fn engine_with_policy(path: Option<&Path>) -> Result<Engine> {
    let Some(path) = path else {
        return Ok(Engine::default());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read the policy patch `{}`", path.display()))?;
    let patch: serde_json::Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse the policy patch `{}` as JSON",
            path.display()
        )
    })?;
    Engine::default()
        .with_policy_patch(&patch)
        .with_context(|| format!("failed to apply the policy patch `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_is_the_default() {
        let cli =
            Cli::try_parse_from(["assessment-engine", "--addr", "127.0.0.1:1234"]).expect("parses");
        assert!(cli.command.is_none());
        assert_eq!(cli.serve.addr.port(), 1234);
        assert_eq!(cli.serve.max_message_bytes, 16 * 1024 * 1024);
        assert_eq!(cli.serve.request_timeout_secs, 30);
        assert_eq!(cli.serve.concurrency_limit, 256);
    }

    #[test]
    fn max_concurrent_assessments_is_optional_and_ranged() {
        const FLAG: &str = "--max-concurrent-assessments";
        let cli = Cli::try_parse_from(["assessment-engine"]).expect("parses");
        assert_eq!(cli.serve.max_concurrent_assessments, None);
        let cli = Cli::try_parse_from(["assessment-engine", FLAG, "3"]).expect("parses");
        assert_eq!(cli.serve.max_concurrent_assessments, Some(3));
        let cli = Cli::try_parse_from(["assessment-engine", "serve", FLAG, "3"]).expect("parses");
        let Some(Command::Serve(args)) = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(args.max_concurrent_assessments, Some(3));

        let too_many = usize::MAX.to_string();
        for bad in ["0", too_many.as_str(), "many"] {
            let err =
                Cli::try_parse_from(["assessment-engine", FLAG, bad]).expect_err("out of range");
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{bad}");
        }
        let err = Cli::try_parse_from(["assessment-engine", FLAG, "0"]).expect_err("zero");
        assert!(err.to_string().contains("must be at least 1"), "{err}");
    }

    #[test]
    fn compact_after_ops_defaults_to_one_hundred_thousand_and_zero_means_never() {
        const FLAG: &str = "--compact-after-ops";
        let cli = Cli::try_parse_from(["assessment-engine"]).expect("parses");
        assert_eq!(cli.serve.compact_after_ops, 100_000);
        let cli = Cli::try_parse_from(["assessment-engine", FLAG, "0"]).expect("parses");
        assert_eq!(cli.serve.compact_after_ops, 0);
        let cli = Cli::try_parse_from(["assessment-engine", "serve", FLAG, "250"]).expect("parses");
        let Some(Command::Serve(args)) = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(args.compact_after_ops, 250);

        for bad in ["-1", "1.5", "many", "99999999999999999999999"] {
            // `=` so that a leading `-` is read as the value.
            let err = Cli::try_parse_from(["assessment-engine", &format!("{FLAG}={bad}")])
                .expect_err("not a count");
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{bad}");
            assert!(err.to_string().contains("must be a whole number"), "{err}");
        }
    }

    #[test]
    fn history_limit_is_optional_and_at_least_one() {
        const FLAG: &str = "--history-limit";
        let cli = Cli::try_parse_from(["assessment-engine"]).expect("parses");
        assert_eq!(cli.serve.history_limit, None);
        let cli = Cli::try_parse_from(["assessment-engine", FLAG, "1"]).expect("parses");
        assert_eq!(cli.serve.history_limit, Some(1));
        let cli = Cli::try_parse_from(["assessment-engine", "serve", FLAG, "20"]).expect("parses");
        let Some(Command::Serve(args)) = cli.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(args.history_limit, Some(20));

        for bad in ["0", "-3", "all"] {
            let err = Cli::try_parse_from(["assessment-engine", &format!("{FLAG}={bad}")])
                .expect_err("out of range");
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{bad}");
        }
        let err = Cli::try_parse_from(["assessment-engine", FLAG, "0"]).expect_err("zero");
        assert!(err.to_string().contains("must be at least 1"), "{err}");
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from([
            "assessment-engine",
            "compare-text",
            "--before",
            "a",
            "--after",
            "b",
        ])
        .expect("parses");
        let Some(Command::CompareText(args)) = cli.command else {
            panic!("expected the compare-text subcommand");
        };
        assert_eq!(args.field, TextField::Description);
        let cli = Cli::try_parse_from([
            "assessment-engine",
            "compare-text",
            "--before",
            "a",
            "--after",
            "b",
            "--field",
            "title",
        ])
        .expect("parses");
        let Some(Command::CompareText(args)) = cli.command else {
            panic!("expected the compare-text subcommand");
        };
        assert_eq!(args.field.name(), "title");
        assert!(
            Cli::try_parse_from([
                "assessment-engine",
                "compare-text",
                "--before",
                "a",
                "--after",
                "b",
                "--field",
                "labels",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["assessment-engine", "assess", "-"]).expect("parses");
        assert!(matches!(cli.command, Some(Command::Assess(_))));
        let cli = Cli::try_parse_from(["assessment-engine", "health"]).expect("parses");
        let Some(Command::Health(args)) = cli.command else {
            panic!("expected the health subcommand");
        };
        assert_eq!(args.endpoint, "http://127.0.0.1:50061");
        assert_eq!(args.service, "assessment.v1.AssessmentEngine");
    }
}
