//! `assessment-engine`: the assessment engine's gRPC server and command line.
//!
//! * `assessment-engine [serve] [--addr …]` serves `assessment.v1` with
//!   health and reflection (the default when no subcommand is given).
//! * `assessment-engine assess <file|->`, `policy` and `compare-text` run the
//!   core engine directly, without a network, and print JSON.
//! * `assessment-engine health` asks a running server whether it serves
//!   (for container health checks).

mod commands;
mod health;
mod serve;

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use assessment_core::Engine;
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

    /// Most assessments one batch or stream runs at once (default: the
    /// number of CPUs).
    #[arg(long, env = "ASSESSMENT_MAX_IN_FLIGHT")]
    pub max_in_flight: Option<usize>,

    /// Log format (the level comes from `RUST_LOG`, default `info`).
    #[arg(long, env = "ASSESSMENT_LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
    pub log_format: LogFormat,
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

    /// A partial policy (JSON) merged over the default policy.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
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
            commands::assess(&engine, &args.input)
        }
        Some(Command::Policy(args)) => {
            init_tracing(LogFormat::Pretty, "warn")?;
            let engine = engine_with_policy(args.policy.as_deref())?;
            commands::policy(&engine)
        }
        Some(Command::CompareText(args)) => {
            init_tracing(LogFormat::Pretty, "warn")?;
            let engine = engine_with_policy(args.policy.as_deref())?;
            commands::compare_text(&engine, &args.before, &args.after)
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
        assert!(matches!(cli.command, Some(Command::CompareText(_))));
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
