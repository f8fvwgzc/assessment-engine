//! The optional judge: its flags, the startup checks and the stack that
//! `serve` and `assess --judge` use.
//!
//! The API key comes from `ANTHROPIC_API_KEY` or from a key file. There is
//! no key flag, and the key is never logged or put into an error.

// Without the `judge` feature only the flags and the refusals are used.
#![cfg_attr(not(feature = "judge"), allow(dead_code, unused_imports))]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use assessment_grpc::flow::JudgeSettings;
use assessment_judge::Judge;
use clap::{Args, ValueEnum};

/// The environment variable that holds the API key.
pub(crate) const KEY_ENV: &str = "ANTHROPIC_API_KEY";

const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Whether the server has a judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum JudgeMode {
    /// No judge. Flagged fields are only reported.
    Off,
    /// A judge. The server does not start without an API key.
    On,
    /// A judge when an API key is set, else none.
    Auto,
}

impl JudgeMode {
    fn as_str(self) -> &'static str {
        match self {
            JudgeMode::Off => "off",
            JudgeMode::On => "on",
            JudgeMode::Auto => "auto",
        }
    }
}

/// An on/off value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Switch {
    On,
    Off,
}

/// How much work the model puts into an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum EffortArg {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortArg {
    fn as_str(self) -> &'static str {
        match self {
            EffortArg::Low => "low",
            EffortArg::Medium => "medium",
            EffortArg::High => "high",
            EffortArg::Xhigh => "xhigh",
            EffortArg::Max => "max",
        }
    }
}

/// Whether the API may answer with another model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum FallbacksArg {
    /// On for the default model, off for every other one.
    Auto,
    On,
    Off,
}

/// The judge options that `serve` and `assess --judge` share.
#[derive(Debug, Clone, Args)]
pub(crate) struct JudgeClientArgs {
    /// The model the judge asks.
    #[arg(
        long = "judge-model",
        env = "ASSESSMENT_JUDGE_MODEL",
        value_name = "MODEL",
        default_value = DEFAULT_MODEL
    )]
    pub model: String,

    /// How much work the model puts into an answer.
    #[arg(
        long = "judge-effort",
        env = "ASSESSMENT_JUDGE_EFFORT",
        value_enum,
        default_value_t = EffortArg::Low
    )]
    pub effort: EffortArg,

    /// The longest the judge may take for one call, in seconds (1 to 600).
    #[arg(
        long = "judge-timeout-secs",
        env = "ASSESSMENT_JUDGE_TIMEOUT_SECS",
        value_name = "SECS",
        default_value_t = 15,
        value_parser = clap::value_parser!(u64).range(1..=600)
    )]
    pub timeout_secs: u64,

    /// Most fields of one request the judge is asked about. The rest are
    /// reported as unavailable.
    #[arg(
        long = "judge-max-fields-per-request",
        env = "ASSESSMENT_JUDGE_MAX_FIELDS_PER_REQUEST",
        value_name = "N",
        default_value_t = 4
    )]
    pub max_fields_per_request: usize,

    /// The largest answer the model may write, in tokens.
    #[arg(
        long = "judge-max-tokens",
        env = "ASSESSMENT_JUDGE_MAX_TOKENS",
        value_name = "N",
        default_value_t = 4096,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_tokens: u32,

    /// Whether the API may answer with another model when the first one
    /// declines.
    #[arg(
        long = "judge-fallbacks",
        env = "ASSESSMENT_JUDGE_FALLBACKS",
        value_enum,
        default_value_t = FallbacksArg::Auto
    )]
    pub fallbacks: FallbacksArg,

    /// The API root. It must be https, or http on a loopback address.
    #[arg(
        long = "judge-base-url",
        env = "ASSESSMENT_JUDGE_BASE_URL",
        value_name = "URL",
        default_value = DEFAULT_BASE_URL
    )]
    pub base_url: String,

    /// A file that holds the API key. Without it the key is read from
    /// `ANTHROPIC_API_KEY`. There is no flag that takes the key itself.
    #[arg(
        long = "anthropic-api-key-file",
        env = "ANTHROPIC_API_KEY_FILE",
        value_name = "FILE"
    )]
    pub key_file: Option<PathBuf>,
}

/// The judge options of `serve`.
#[derive(Debug, Clone, Args)]
pub(crate) struct JudgeArgs {
    /// Ask a model about text changes whose meaning the word counts cannot
    /// settle. `auto` turns the judge on when an API key is set.
    #[arg(
        long = "judge",
        env = "ASSESSMENT_JUDGE",
        value_enum,
        value_name = "MODE",
        default_value_t = JudgeMode::Off
    )]
    pub mode: JudgeMode,

    /// Whether a call that does not say is judged.
    #[arg(
        long = "judge-default",
        env = "ASSESSMENT_JUDGE_DEFAULT",
        value_enum,
        default_value_t = Switch::On
    )]
    pub default: Switch,

    #[command(flatten)]
    pub client: JudgeClientArgs,

    /// Most fields of one batch the judge is asked about.
    #[arg(
        long = "judge-max-fields-per-batch",
        env = "ASSESSMENT_JUDGE_MAX_FIELDS_PER_BATCH",
        value_name = "N",
        default_value_t = 16
    )]
    pub max_fields_per_batch: usize,

    /// Most judge calls that run at once.
    #[arg(
        long = "judge-concurrency",
        env = "ASSESSMENT_JUDGE_CONCURRENCY",
        value_name = "N",
        default_value_t = 8,
        value_parser = parse_at_least_one
    )]
    pub concurrency: usize,

    /// Most judge calls that start in one minute.
    #[arg(
        long = "judge-max-calls-per-minute",
        env = "ASSESSMENT_JUDGE_MAX_CALLS_PER_MINUTE",
        value_name = "N",
        default_value_t = 60,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_calls_per_minute: u32,

    /// Most US dollars the judge may spend in one hour.
    #[arg(
        long = "judge-max-usd-per-hour",
        env = "ASSESSMENT_JUDGE_MAX_USD_PER_HOUR",
        value_name = "USD",
        default_value_t = 10.0,
        value_parser = parse_usd
    )]
    pub max_usd_per_hour: f64,

    /// The longest text the judge is sent, in characters. A longer one is
    /// reported as unavailable.
    #[arg(
        long = "judge-max-text-chars",
        env = "ASSESSMENT_JUDGE_MAX_TEXT_CHARS",
        value_name = "N",
        default_value_t = 8000
    )]
    pub max_text_chars: usize,

    /// Most answers the judge cache keeps.
    #[arg(
        long = "judge-cache-entries",
        env = "ASSESSMENT_JUDGE_CACHE_ENTRIES",
        value_name = "N",
        default_value_t = 4096,
        value_parser = parse_at_least_one
    )]
    pub cache_entries: usize,

    /// Let a server without `--auth-token` use the judge on an address
    /// that is not loopback. Anyone who reaches it can then spend money.
    #[arg(
        long = "judge-allow-unauthenticated",
        env = "ASSESSMENT_JUDGE_ALLOW_UNAUTHENTICATED"
    )]
    pub allow_unauthenticated: bool,
}

fn parse_at_least_one(text: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(n) => Ok(n),
        Err(_) => Err("must be a whole number, at least 1".to_owned()),
    }
}

fn parse_usd(text: &str) -> Result<f64, String> {
    match text.trim().parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd >= 0.0 => Ok(usd),
        _ => Err("must be a number of US dollars, 0 or more".to_owned()),
    }
}

/// An API key as it was read. It is never printed.
pub(crate) struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// The key in the environment, when one is set.
pub(crate) fn key_from_env() -> Option<Secret> {
    std::env::var(KEY_ENV).ok().map(Secret)
}

/// The key to use: the key file when one is given, else the environment.
/// White space around the key is dropped. An error never holds the key.
fn resolve_key(file: Option<&Path>, env: Option<Secret>) -> Result<Option<Secret>> {
    if let Some(path) = file {
        // The error of the read names the path only.
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}", e.kind()))
            .with_context(|| format!("failed to read the API key file `{}`", path.display()))?;
        let key = text.trim();
        if key.is_empty() {
            bail!("the API key file `{}` is empty", path.display());
        }
        return Ok(Some(Secret(key.to_owned())));
    }
    Ok(env
        .map(|key| Secret(key.0.trim().to_owned()))
        .filter(|key| !key.0.is_empty()))
}

/// The host of a base URL, for the log. The URL itself may carry a path.
fn base_host(base_url: &str) -> &str {
    let rest = base_url
        .trim()
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// A judge that is ready to use, and how the flow uses it.
#[derive(Debug)]
pub(crate) struct JudgeStack {
    pub judge: Arc<dyn Judge>,
    pub settings: JudgeSettings,
    /// The model name, for the report of the command line.
    pub model: String,
    #[cfg(feature = "judge")]
    client: Arc<assessment_judge::AnthropicJudge>,
}

impl JudgeStack {
    /// Checks the key, the model and TLS in the background and logs the
    /// result. Startup does not wait for it.
    pub(crate) fn spawn_startup_probe(&self) {
        #[cfg(feature = "judge")]
        drop(self.client.spawn_startup_probe());
    }
}

fn settings(client: &JudgeClientArgs) -> JudgeSettings {
    JudgeSettings::default()
        .with_max_fields_per_request(client.max_fields_per_request)
        .with_timeout(Duration::from_secs(client.timeout_secs))
}

#[cfg(feature = "judge")]
fn anthropic(
    client: &JudgeClientArgs,
    key: &Secret,
) -> Result<Arc<assessment_judge::AnthropicJudge>> {
    use assessment_judge::{AnthropicConfig, AnthropicJudge, ApiKey, Effort, Fallbacks};

    // The messages of these errors are fixed texts: they never hold the key.
    let key = ApiKey::new(&key.0).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let config = AnthropicConfig {
        base_url: client.base_url.clone(),
        model: client.model.clone(),
        effort: match client.effort {
            EffortArg::Low => Effort::Low,
            EffortArg::Medium => Effort::Medium,
            EffortArg::High => Effort::High,
            EffortArg::Xhigh => Effort::Xhigh,
            EffortArg::Max => Effort::Max,
        },
        max_tokens: client.max_tokens,
        fallbacks: match client.fallbacks {
            FallbacksArg::Auto => Fallbacks::Auto,
            FallbacksArg::On => Fallbacks::On,
            FallbacksArg::Off => Fallbacks::Off,
        },
    };
    let judge = AnthropicJudge::new(key, config).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    Ok(Arc::new(judge))
}

/// The judge of `serve`, or `None` when the server runs without one.
///
/// `env_key` is the value of `ANTHROPIC_API_KEY`. `addr` and `auth_enabled`
/// say who can reach the server.
pub(crate) fn from_args(
    args: &JudgeArgs,
    env_key: Option<Secret>,
    addr: SocketAddr,
    auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    if args.mode == JudgeMode::Off {
        return Ok(None);
    }
    let key = resolve_key(args.client.key_file.as_deref(), env_key)?;
    let Some(key) = key else {
        if args.mode == JudgeMode::On {
            bail!(
                "--judge on needs an API key: set {KEY_ENV} or give --anthropic-api-key-file \
                 (use --judge auto to run without a judge when there is no key)"
            );
        }
        tracing::info!("judge off: --judge auto and no API key is set");
        return Ok(None);
    };
    build(args, &key, addr, auth_enabled)
}

#[cfg(not(feature = "judge"))]
fn build(
    args: &JudgeArgs,
    _key: &Secret,
    _addr: SocketAddr,
    _auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    if args.mode == JudgeMode::On {
        bail!("--judge on is not available: this binary was built without the `judge` feature");
    }
    tracing::info!("judge off: this binary was built without the `judge` feature");
    Ok(None)
}

#[cfg(feature = "judge")]
fn build(
    args: &JudgeArgs,
    key: &Secret,
    addr: SocketAddr,
    auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    use assessment_judge::{CachedJudge, LimitedJudge, Limits};

    if !auth_enabled && !addr.ip().is_loopback() && !args.allow_unauthenticated {
        bail!(
            "the judge spends money and {addr} is not a loopback address: set --auth-token, \
             listen on a loopback address, or pass --judge-allow-unauthenticated"
        );
    }
    let client = anthropic(&args.client, key).context("the judge cannot be set up")?;
    let timeout = Duration::from_secs(args.client.timeout_secs);
    let limits = Limits {
        concurrency: args.concurrency,
        calls_per_minute: args.max_calls_per_minute,
        usd_per_hour: args.max_usd_per_hour,
        model: args.client.model.clone(),
        max_tokens: args.client.max_tokens,
        timeout,
    };
    let inner: Arc<dyn Judge> = Arc::clone(&client) as Arc<dyn Judge>;
    let limited: Arc<dyn Judge> = Arc::new(LimitedJudge::new(inner, limits));
    let judge: Arc<dyn Judge> = Arc::new(CachedJudge::with_capacity(limited, args.cache_entries));
    let settings = settings(&args.client)
        .with_default_on(args.default == Switch::On)
        .with_max_fields_per_batch(args.max_fields_per_batch)
        .with_max_text_chars(args.max_text_chars);
    tracing::info!(
        mode = args.mode.as_str(),
        model = %args.client.model,
        effort = args.client.effort.as_str(),
        fallbacks = client.fallbacks_on(),
        default_on = settings.default_on,
        timeout_secs = args.client.timeout_secs,
        max_fields_per_request = settings.max_fields_per_request,
        max_fields_per_batch = settings.max_fields_per_batch,
        max_text_chars = settings.max_text_chars,
        max_tokens = args.client.max_tokens,
        concurrency = args.concurrency,
        max_calls_per_minute = args.max_calls_per_minute,
        max_usd_per_hour = args.max_usd_per_hour,
        cache_entries = args.cache_entries,
        base_host = base_host(&args.client.base_url),
        "judge on"
    );
    Ok(Some(JudgeStack {
        judge,
        settings,
        model: args.client.model.clone(),
        client,
    }))
}

/// The judge of `assess --judge`. The command asked for one, so a missing
/// key is an error.
#[cfg(feature = "judge")]
pub(crate) fn for_cli(client: &JudgeClientArgs, env_key: Option<Secret>) -> Result<JudgeStack> {
    let Some(key) = resolve_key(client.key_file.as_deref(), env_key)? else {
        bail!("--judge needs an API key: set {KEY_ENV} or give --anthropic-api-key-file");
    };
    let judge = anthropic(client, &key).context("the judge cannot be set up")?;
    Ok(JudgeStack {
        judge: Arc::clone(&judge) as Arc<dyn Judge>,
        settings: settings(client),
        model: client.model.clone(),
        client: judge,
    })
}

#[cfg(not(feature = "judge"))]
pub(crate) fn for_cli(_client: &JudgeClientArgs, _env_key: Option<Secret>) -> Result<JudgeStack> {
    bail!("--judge is not available: this binary was built without the `judge` feature")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{Cli, Command};

    const LOOPBACK: &str = "127.0.0.1:50061";
    const PUBLIC: &str = "0.0.0.0:50061";
    /// A local port nothing listens on. No test connects to it.
    const LOCAL_BASE: &str = "http://127.0.0.1:9";
    const KEY: &str = "test-key-0123456789";

    fn judge_args(extra: &[&str]) -> JudgeArgs {
        let mut argv = vec!["assessment-engine"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).expect("parses").serve.judge
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("an address")
    }

    fn key() -> Option<Secret> {
        Some(Secret(KEY.to_owned()))
    }

    fn key_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "assessment-server-judge-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temporary directory is created");
        let path = dir.join("key");
        std::fs::write(&path, content).expect("the key file is written");
        path
    }

    #[test]
    fn the_judge_flags_have_their_defaults() {
        let args = judge_args(&[]);
        // `ASSESSMENT_JUDGE` may be set where the tests run.
        if std::env::var_os("ASSESSMENT_JUDGE").is_none() {
            assert_eq!(args.mode, JudgeMode::Off);
        }
        let args = judge_args(&["--judge", "off"]);
        assert_eq!(args.mode, JudgeMode::Off);
        if std::env::vars_os().any(|(name, _)| {
            name.to_string_lossy().starts_with("ASSESSMENT_JUDGE_")
                || name.to_string_lossy() == "ANTHROPIC_API_KEY_FILE"
        }) {
            return;
        }
        assert_eq!(args.default, Switch::On);
        assert_eq!(args.client.model, "claude-opus-5-5");
        assert_eq!(args.client.effort, EffortArg::Low);
        assert_eq!(args.client.timeout_secs, 15);
        assert_eq!(args.client.max_fields_per_request, 4);
        assert_eq!(args.max_fields_per_batch, 16);
        assert_eq!(args.concurrency, 8);
        assert_eq!(args.max_calls_per_minute, 60);
        assert!((args.max_usd_per_hour - 10.0).abs() < f64::EPSILON);
        assert_eq!(args.max_text_chars, 8000);
        assert_eq!(args.client.max_tokens, 4096);
        assert_eq!(args.cache_entries, 4096);
        assert_eq!(args.client.fallbacks, FallbacksArg::Auto);
        assert_eq!(args.client.base_url, "https://api.anthropic.com");
        assert_eq!(args.client.key_file, None);
        assert!(!args.allow_unauthenticated);
    }

    #[test]
    fn the_judge_flags_parse() {
        let args = judge_args(&[
            "--judge",
            "auto",
            "--judge-default",
            "off",
            "--judge-model",
            "claude-haiku-4-5",
            "--judge-effort",
            "xhigh",
            "--judge-timeout-secs",
            "7",
            "--judge-max-fields-per-request",
            "2",
            "--judge-max-fields-per-batch",
            "9",
            "--judge-concurrency",
            "3",
            "--judge-max-calls-per-minute",
            "30",
            "--judge-max-usd-per-hour",
            "2.5",
            "--judge-max-text-chars",
            "500",
            "--judge-max-tokens",
            "1024",
            "--judge-cache-entries",
            "64",
            "--judge-fallbacks",
            "off",
            "--judge-base-url",
            LOCAL_BASE,
            "--anthropic-api-key-file",
            "/run/secrets/key",
            "--judge-allow-unauthenticated",
        ]);
        assert_eq!(args.mode, JudgeMode::Auto);
        assert_eq!(args.default, Switch::Off);
        assert_eq!(args.client.model, "claude-haiku-4-5");
        assert_eq!(args.client.effort, EffortArg::Xhigh);
        assert_eq!(args.client.timeout_secs, 7);
        assert_eq!(args.client.max_fields_per_request, 2);
        assert_eq!(args.max_fields_per_batch, 9);
        assert_eq!(args.concurrency, 3);
        assert_eq!(args.max_calls_per_minute, 30);
        assert!((args.max_usd_per_hour - 2.5).abs() < f64::EPSILON);
        assert_eq!(args.max_text_chars, 500);
        assert_eq!(args.client.max_tokens, 1024);
        assert_eq!(args.cache_entries, 64);
        assert_eq!(args.client.fallbacks, FallbacksArg::Off);
        assert_eq!(args.client.base_url, LOCAL_BASE);
        assert_eq!(
            args.client.key_file.as_deref(),
            Some(Path::new("/run/secrets/key"))
        );
        assert!(args.allow_unauthenticated);
    }

    #[test]
    fn bad_judge_values_are_rejected() {
        let bad: [&[&str]; 11] = [
            &["--judge", "maybe"],
            &["--judge-default", "auto"],
            &["--judge-effort", "extreme"],
            &["--judge-fallbacks", "sometimes"],
            &["--judge-timeout-secs", "0"],
            &["--judge-concurrency", "0"],
            &["--judge-max-calls-per-minute", "0"],
            // `=` so that a leading `-` is read as the value.
            &["--judge-max-usd-per-hour=-1"],
            &["--judge-max-usd-per-hour", "NaN"],
            &["--judge-max-tokens", "0"],
            &["--judge-cache-entries", "0"],
        ];
        for bad in bad {
            let mut argv = vec!["assessment-engine"];
            argv.extend_from_slice(bad);
            assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
        }
        // There is no flag that takes the key.
        for flag in ["--anthropic-api-key", "--judge-api-key", "--api-key"] {
            assert!(
                Cli::try_parse_from(["assessment-engine", flag, KEY]).is_err(),
                "{flag}"
            );
        }
    }

    #[test]
    fn assess_takes_a_bare_judge_switch_and_the_shared_flags() {
        let cli = Cli::try_parse_from(["assessment-engine", "assess", "-"]).expect("parses");
        let Some(Command::Assess(args)) = cli.command else {
            panic!("expected the assess subcommand");
        };
        assert!(!args.judge);

        let cli = Cli::try_parse_from([
            "assessment-engine",
            "assess",
            "-",
            "--judge",
            "--judge-model",
            "claude-haiku-4-5",
            "--judge-max-fields-per-request",
            "1",
        ])
        .expect("parses");
        let Some(Command::Assess(args)) = cli.command else {
            panic!("expected the assess subcommand");
        };
        assert!(args.judge);
        assert_eq!(args.client.model, "claude-haiku-4-5");
        assert_eq!(args.client.max_fields_per_request, 1);
        // The serve-only flags are not part of `assess`.
        assert!(
            Cli::try_parse_from([
                "assessment-engine",
                "assess",
                "-",
                "--judge-concurrency",
                "2"
            ])
            .is_err()
        );
    }

    #[test]
    fn off_needs_no_key_and_builds_nothing() {
        let args = judge_args(&["--judge", "off"]);
        let stack = from_args(&args, None, addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
        // A key that is set is not even looked at.
        let stack = from_args(&args, key(), addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
    }

    #[test]
    fn on_without_a_key_is_an_error_and_auto_is_off() {
        let on = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
        let err = from_args(&on, None, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");
        // A key of white space only is no key.
        let blank = Some(Secret("  \n".to_owned()));
        let err = from_args(&on, blank, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");

        let auto = judge_args(&["--judge", "auto", "--judge-base-url", LOCAL_BASE]);
        let stack = from_args(&auto, None, addr(LOOPBACK), true).expect("starts");
        assert!(stack.is_none());
    }

    #[test]
    fn a_missing_or_empty_key_file_is_an_error_without_the_key() {
        let missing = std::env::temp_dir().join("assessment-server-judge-no-such-key-file");
        let missing = missing.to_str().expect("the path is UTF-8");
        let args = judge_args(&[
            "--judge",
            "auto",
            "--judge-base-url",
            LOCAL_BASE,
            "--anthropic-api-key-file",
            missing,
        ]);
        // A key file that was asked for must be readable, in `auto` too.
        let err = from_args(&args, key(), addr(LOOPBACK), true).expect_err("no file");
        let text = format!("{err:#}");
        assert!(text.contains("failed to read the API key file"), "{text}");
        assert!(!text.contains(KEY), "{text}");

        let empty = key_file("empty", " \n");
        let args = judge_args(&[
            "--judge",
            "on",
            "--judge-base-url",
            LOCAL_BASE,
            "--anthropic-api-key-file",
            empty.to_str().expect("the path is UTF-8"),
        ]);
        let err = from_args(&args, None, addr(LOOPBACK), true).expect_err("empty file");
        assert!(err.to_string().contains("is empty"), "{err}");
        let _ = std::fs::remove_dir_all(empty.parent().expect("the file has a directory"));
    }

    #[test]
    fn secrets_are_not_printed() {
        assert_eq!(
            format!("{:?}", Secret(KEY.to_owned())),
            "Secret([redacted])"
        );
    }

    #[test]
    fn the_base_host_is_read_from_the_url() {
        assert_eq!(base_host("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(base_host("http://127.0.0.1:9/v1/"), "127.0.0.1:9");
        assert_eq!(base_host("https://user:pw@example.com/x"), "example.com");
    }

    #[cfg(not(feature = "judge"))]
    #[test]
    fn a_binary_without_the_judge_feature_refuses_on() {
        let on = judge_args(&["--judge", "on"]);
        let err = from_args(&on, key(), addr(LOOPBACK), true).expect_err("no judge");
        assert!(err.to_string().contains("built without"), "{err}");
        let auto = judge_args(&["--judge", "auto"]);
        assert!(
            from_args(&auto, key(), addr(LOOPBACK), true)
                .expect("starts")
                .is_none()
        );
    }

    #[cfg(feature = "judge")]
    mod with_the_judge {
        use super::*;

        #[test]
        fn a_key_file_is_trimmed_and_wins_over_the_environment() {
            let path = key_file("trimmed", &format!("\n  {KEY}  \n\n"));
            let secret = resolve_key(Some(&path), Some(Secret("other".to_owned())))
                .expect("reads")
                .expect("a key");
            assert_eq!(secret.0, KEY);

            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-base-url",
                LOCAL_BASE,
                "--anthropic-api-key-file",
                path.to_str().expect("the path is UTF-8"),
            ]);
            let stack = from_args(&args, None, addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert_eq!(stack.judge.id(), "anthropic/claude-opus-5-5");
            assert_eq!(stack.model, "claude-opus-5-5");
            assert!(!format!("{stack:?}").contains(KEY));
            let _ = std::fs::remove_dir_all(path.parent().expect("the file has a directory"));
        }

        #[test]
        fn the_flags_reach_the_settings() {
            let args = judge_args(&[
                "--judge",
                "auto",
                "--judge-default",
                "off",
                "--judge-timeout-secs",
                "7",
                "--judge-max-fields-per-request",
                "2",
                "--judge-max-fields-per-batch",
                "9",
                "--judge-max-text-chars",
                "500",
                "--judge-model",
                "claude-haiku-4-5",
                "--judge-base-url",
                LOCAL_BASE,
            ]);
            let stack = from_args(&args, key(), addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert!(!stack.settings.default_on);
            assert_eq!(stack.settings.timeout, Duration::from_secs(7));
            assert_eq!(stack.settings.max_fields_per_request, 2);
            assert_eq!(stack.settings.max_fields_per_batch, 9);
            assert_eq!(stack.settings.max_text_chars, 500);
            assert_eq!(stack.judge.id(), "anthropic/claude-haiku-4-5");
        }

        #[test]
        fn a_judge_without_auth_must_stay_on_loopback() {
            let args = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
            let err = from_args(&args, key(), addr(PUBLIC), false).expect_err("open to anyone");
            assert!(
                err.to_string().contains("--judge-allow-unauthenticated"),
                "{err}"
            );
            assert!(!format!("{err:#}").contains(KEY));

            // Each of the three ways out works.
            assert!(from_args(&args, key(), addr(PUBLIC), true).is_ok());
            assert!(from_args(&args, key(), addr(LOOPBACK), false).is_ok());
            assert!(from_args(&args, key(), addr("[::1]:50061"), false).is_ok());
            let allowed = judge_args(&[
                "--judge",
                "on",
                "--judge-base-url",
                LOCAL_BASE,
                "--judge-allow-unauthenticated",
            ]);
            assert!(from_args(&allowed, key(), addr(PUBLIC), false).is_ok());
        }

        #[test]
        fn a_plain_http_base_url_must_be_loopback() {
            for bad in [
                "http://api.example.com",
                "http://10.0.0.5:8080",
                "ftp://127.0.0.1",
                "https://user:pw@api.example.com",
                "not a url",
            ] {
                let args = judge_args(&["--judge", "on", "--judge-base-url", bad]);
                let err = from_args(&args, key(), addr(LOOPBACK), true).expect_err("refused");
                let text = format!("{err:#}");
                assert!(text.contains("base URL"), "{bad}: {text}");
                assert!(!text.contains(KEY), "{bad}: {text}");
            }
            for good in [
                "https://api.example.com",
                "http://localhost:8080",
                "http://[::1]:8080",
            ] {
                let args = judge_args(&["--judge", "on", "--judge-base-url", good]);
                assert!(
                    from_args(&args, key(), addr(LOOPBACK), true).is_ok(),
                    "{good}"
                );
            }
        }

        #[test]
        fn a_bad_key_or_model_is_an_error_without_the_key() {
            let args = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
            let bad_key = "bad key\twith spaces";
            let err = from_args(
                &args,
                Some(Secret(bad_key.to_owned())),
                addr(LOOPBACK),
                true,
            )
            .expect_err("a bad key");
            let text = format!("{err:#}");
            assert!(text.contains("API key"), "{text}");
            assert!(!text.contains(bad_key), "{text}");

            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-base-url",
                LOCAL_BASE,
                "--judge-model",
                "bad model",
            ]);
            let err = from_args(&args, key(), addr(LOOPBACK), true).expect_err("a bad model");
            assert!(format!("{err:#}").contains("model"), "{err:#}");
        }

        #[test]
        fn the_command_line_judge_needs_a_key() {
            let args = judge_args(&["--judge-base-url", LOCAL_BASE]);
            let err = for_cli(&args.client, None).expect_err("no key");
            assert!(err.to_string().contains("needs an API key"), "{err}");
            let stack = for_cli(&args.client, key()).expect("a judge");
            assert_eq!(stack.judge.id(), "anthropic/claude-opus-5-5");
            assert_eq!(stack.settings.max_fields_per_request, 4);
        }
    }
}
