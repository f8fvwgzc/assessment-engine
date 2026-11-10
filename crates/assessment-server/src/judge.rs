//! The optional judge: its flags, the startup checks and the stack that
//! `serve` and `assess --judge` use.
//!
//! There are two kinds of provider. `anthropic` asks the Messages API with
//! an API key. The key comes from `ANTHROPIC_API_KEY` or from a key file.
//! There is no key flag, and the key is never logged or put into an error.
//! `claude-cli` and `codex-cli` run an agent command line that is installed
//! and logged in on this machine. They need no key, and the key is not
//! read at all when one of them is chosen.

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
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_CONCURRENCY: usize = 8;
const DEFAULT_CLI_CONCURRENCY: usize = 2;

/// Whether the server has a judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum JudgeMode {
    /// No judge. Flagged fields are only reported.
    Off,
    /// A judge. The server does not start when the provider cannot be
    /// used: `anthropic` without an API key, or a command line provider
    /// whose program is not found.
    On,
    /// A judge when the provider can be used, else none: an API key is
    /// set, or the program of a command line provider is found.
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

/// Who answers the judge's questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProviderArg {
    /// The Anthropic Messages API, with an API key.
    Anthropic,
    /// The Claude Code command line, with its own login.
    ClaudeCli,
    /// The Codex command line, with its own login. It is not a Claude
    /// model: the texts go to OpenAI.
    CodexCli,
}

impl ProviderArg {
    fn as_str(self) -> &'static str {
        match self {
            ProviderArg::Anthropic => "anthropic",
            ProviderArg::ClaudeCli => "claude-cli",
            ProviderArg::CodexCli => "codex-cli",
        }
    }

    /// Whether the provider runs a command line program.
    pub(crate) fn is_cli(self) -> bool {
        self != ProviderArg::Anthropic
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
    /// Who answers: the Anthropic API with a key, or a command line
    /// program that is installed and logged in on this machine.
    #[arg(
        long = "judge-provider",
        env = "ASSESSMENT_JUDGE_PROVIDER",
        value_enum,
        value_name = "PROVIDER",
        default_value_t = ProviderArg::Anthropic
    )]
    pub provider: ProviderArg,

    /// The program of a command line provider. A bare name is looked up
    /// in PATH. The default is `claude` or `codex`.
    #[arg(
        long = "judge-cli-path",
        env = "ASSESSMENT_JUDGE_CLI_PATH",
        value_name = "PROGRAM"
    )]
    pub cli_path: Option<PathBuf>,

    /// The model the judge asks [default: claude-opus-5-5, or the
    /// program's own model with codex-cli].
    #[arg(
        long = "judge-model",
        env = "ASSESSMENT_JUDGE_MODEL",
        value_name = "MODEL"
    )]
    pub model: Option<String>,

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

    /// The largest answer the model may write, in tokens [default: 4096].
    /// Only with the anthropic provider.
    #[arg(
        long = "judge-max-tokens",
        env = "ASSESSMENT_JUDGE_MAX_TOKENS",
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_tokens: Option<u32>,

    /// Whether the API may answer with another model when the first one
    /// declines [default: auto]. Only with the anthropic provider.
    #[arg(
        long = "judge-fallbacks",
        env = "ASSESSMENT_JUDGE_FALLBACKS",
        value_enum
    )]
    pub fallbacks: Option<FallbacksArg>,

    /// The API root [default: https://api.anthropic.com]. It must be
    /// https, or http on a loopback address. Only with the anthropic
    /// provider.
    #[arg(
        long = "judge-base-url",
        env = "ASSESSMENT_JUDGE_BASE_URL",
        value_name = "URL"
    )]
    pub base_url: Option<String>,

    /// A file that holds the API key. Without it the key is read from
    /// `ANTHROPIC_API_KEY`. There is no flag that takes the key itself.
    /// Only with the anthropic provider.
    #[arg(
        long = "anthropic-api-key-file",
        env = "ANTHROPIC_API_KEY_FILE",
        value_name = "FILE"
    )]
    pub key_file: Option<PathBuf>,
}

impl JudgeClientArgs {
    /// The model to ask. `None` leaves the choice to the program, which
    /// is the default of `codex-cli`.
    pub(crate) fn model(&self) -> Option<&str> {
        match (&self.model, self.provider) {
            (Some(model), _) => Some(model),
            (None, ProviderArg::CodexCli) => None,
            (None, ProviderArg::Anthropic | ProviderArg::ClaudeCli) => Some(DEFAULT_MODEL),
        }
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }

    pub(crate) fn fallbacks(&self) -> FallbacksArg {
        self.fallbacks.unwrap_or(FallbacksArg::Auto)
    }

    pub(crate) fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    /// Refuses a flag that the chosen provider cannot honour. A flag that
    /// is silently ignored would look like it worked.
    fn check_provider_flags(&self) -> Result<()> {
        let provider = self.provider.as_str();
        if !self.provider.is_cli() {
            if self.cli_path.is_some() {
                bail!(
                    "--judge-cli-path (ASSESSMENT_JUDGE_CLI_PATH) is only for a command line \
                     provider: set --judge-provider claude-cli or codex-cli, or remove it"
                );
            }
            return Ok(());
        }
        let http_only = [
            (
                "--judge-base-url (ASSESSMENT_JUDGE_BASE_URL)",
                self.base_url.is_some(),
            ),
            (
                "--judge-fallbacks (ASSESSMENT_JUDGE_FALLBACKS)",
                self.fallbacks.is_some(),
            ),
            (
                "--judge-max-tokens (ASSESSMENT_JUDGE_MAX_TOKENS)",
                self.max_tokens.is_some(),
            ),
            (
                "--anthropic-api-key-file (ANTHROPIC_API_KEY_FILE)",
                self.key_file.is_some(),
            ),
        ];
        for (flag, set) in http_only {
            if set {
                bail!(
                    "{flag} is only for the anthropic provider and has no meaning with \
                     --judge-provider {provider}: remove it"
                );
            }
        }
        if matches!(self.effort, EffortArg::Xhigh | EffortArg::Max) {
            bail!(
                "--judge-effort {} is not available with --judge-provider {provider}: \
                 use low, medium or high",
                self.effort.as_str()
            );
        }
        Ok(())
    }
}

/// The judge options of `serve`.
#[derive(Debug, Clone, Args)]
pub(crate) struct JudgeArgs {
    /// Ask a model about text changes whose meaning the word counts cannot
    /// settle. `auto` turns the judge on when an API key is set, or when
    /// the program of a command line provider is installed.
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

    /// Most judge calls that run at once [default: 8, or 2 with a command
    /// line provider].
    #[arg(
        long = "judge-concurrency",
        env = "ASSESSMENT_JUDGE_CONCURRENCY",
        value_name = "N",
        value_parser = parse_at_least_one
    )]
    pub concurrency: Option<usize>,

    /// Most judge calls that start in one minute.
    #[arg(
        long = "judge-max-calls-per-minute",
        env = "ASSESSMENT_JUDGE_MAX_CALLS_PER_MINUTE",
        value_name = "N",
        default_value_t = 60,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub max_calls_per_minute: u32,

    /// Most US dollars the judge may spend in one hour. A command line
    /// provider is not billed per call, so this has no effect with one.
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
    /// that is not loopback. Anyone who reaches it can then spend money
    /// or the quota of the logged-in account.
    #[arg(
        long = "judge-allow-unauthenticated",
        env = "ASSESSMENT_JUDGE_ALLOW_UNAUTHENTICATED"
    )]
    pub allow_unauthenticated: bool,
}

impl JudgeArgs {
    /// Judge calls at once. A command line provider starts one process
    /// per call and runs on a subscription, so its default is lower.
    pub(crate) fn concurrency(&self) -> usize {
        self.concurrency
            .unwrap_or(if self.client.provider.is_cli() {
                DEFAULT_CLI_CONCURRENCY
            } else {
                DEFAULT_CONCURRENCY
            })
    }
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
    /// The name for the report of the command line: the model of the
    /// API, or the judge id of a command line provider.
    pub model: String,
    /// Whether a call is billed per token. It is not with a command line
    /// provider, so no cost is reported for one.
    pub metered: bool,
    /// The API client, for the startup probe. A command line provider
    /// has none.
    #[cfg(feature = "judge")]
    client: Option<Arc<assessment_judge::AnthropicJudge>>,
}

impl JudgeStack {
    /// Checks the key, the model and TLS in the background and logs the
    /// result. Startup does not wait for it. A command line provider is
    /// not probed: a probe would start the program and use its quota.
    pub(crate) fn spawn_startup_probe(&self) {
        #[cfg(feature = "judge")]
        if let Some(client) = &self.client {
            drop(client.spawn_startup_probe());
        }
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
        base_url: client.base_url().to_owned(),
        model: client.model().unwrap_or(DEFAULT_MODEL).to_owned(),
        effort: match client.effort {
            EffortArg::Low => Effort::Low,
            EffortArg::Medium => Effort::Medium,
            EffortArg::High => Effort::High,
            EffortArg::Xhigh => Effort::Xhigh,
            EffortArg::Max => Effort::Max,
        },
        max_tokens: client.max_tokens(),
        fallbacks: match client.fallbacks() {
            FallbacksArg::Auto => Fallbacks::Auto,
            FallbacksArg::On => Fallbacks::On,
            FallbacksArg::Off => Fallbacks::Off,
        },
    };
    let judge = AnthropicJudge::new(key, config).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    Ok(Arc::new(judge))
}

/// The judge of a command line provider. No process is started here.
#[cfg(feature = "judge")]
fn cli_judge(client: &JudgeClientArgs) -> Result<Arc<assessment_judge::CliJudge>> {
    use assessment_judge::{CliConfig, CliJudge, CliKind, Effort};

    let kind = match client.provider {
        ProviderArg::ClaudeCli => CliKind::ClaudeCode,
        ProviderArg::CodexCli => CliKind::Codex,
        ProviderArg::Anthropic => bail!("the anthropic provider is not a command line"),
    };
    let mut config = CliConfig::new(kind);
    config.program = client.cli_path.clone();
    config.model = client.model().map(str::to_owned);
    config.effort = match client.effort {
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
        EffortArg::Xhigh => Effort::Xhigh,
        EffortArg::Max => Effort::Max,
    };
    let judge = CliJudge::new(config).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    Ok(Arc::new(judge))
}

/// The program a command line provider would run, for a message.
fn cli_program(client: &JudgeClientArgs) -> String {
    match (&client.cli_path, client.provider) {
        (Some(path), _) => path.display().to_string(),
        (None, ProviderArg::CodexCli) => "codex".to_owned(),
        (None, _) => "claude".to_owned(),
    }
}

/// A server with a judge must not be open to everyone.
fn check_reach(args: &JudgeArgs, addr: SocketAddr, auth_enabled: bool) -> Result<()> {
    if !auth_enabled && !addr.ip().is_loopback() && !args.allow_unauthenticated {
        bail!(
            "the judge spends money or the quota of a logged-in account and {addr} is not a \
             loopback address: set --auth-token, listen on a loopback address, or pass \
             --judge-allow-unauthenticated"
        );
    }
    Ok(())
}

/// The judge of `serve`, or `None` when the server runs without one.
///
/// `env_key` reads `ANTHROPIC_API_KEY`. It is called only with the
/// anthropic provider, so a command line provider never reads the key.
/// `addr` and `auth_enabled` say who can reach the server.
pub(crate) fn from_args(
    args: &JudgeArgs,
    env_key: impl FnOnce() -> Option<Secret>,
    addr: SocketAddr,
    auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    if args.mode == JudgeMode::Off {
        return Ok(None);
    }
    args.client.check_provider_flags()?;
    if args.client.provider.is_cli() {
        return build_cli(args, addr, auth_enabled);
    }
    let key = resolve_key(args.client.key_file.as_deref(), env_key())?;
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

#[cfg(not(feature = "judge"))]
fn build_cli(
    args: &JudgeArgs,
    _addr: SocketAddr,
    _auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    if args.mode == JudgeMode::On {
        bail!("--judge on is not available: this binary was built without the `judge` feature");
    }
    tracing::info!("judge off: this binary was built without the `judge` feature");
    Ok(None)
}

/// The cache and the limits around a judge, and the flow settings.
#[cfg(feature = "judge")]
fn wrap(
    args: &JudgeArgs,
    inner: Arc<dyn Judge>,
    price_model: String,
) -> (Arc<dyn Judge>, JudgeSettings) {
    use assessment_judge::{CachedJudge, LimitedJudge, Limits};

    let limits = Limits {
        concurrency: args.concurrency(),
        calls_per_minute: args.max_calls_per_minute,
        usd_per_hour: args.max_usd_per_hour,
        model: price_model,
        max_tokens: args.client.max_tokens(),
        timeout: Duration::from_secs(args.client.timeout_secs),
    };
    let limited: Arc<dyn Judge> = Arc::new(LimitedJudge::new(inner, limits));
    let judge: Arc<dyn Judge> = Arc::new(CachedJudge::with_capacity(limited, args.cache_entries));
    let settings = settings(&args.client)
        .with_default_on(args.default == Switch::On)
        .with_max_fields_per_batch(args.max_fields_per_batch)
        .with_max_text_chars(args.max_text_chars);
    (judge, settings)
}

/// The judge of a command line provider for `serve`. A program that is
/// not installed is an error with `on` and no judge with `auto`.
#[cfg(feature = "judge")]
fn build_cli(args: &JudgeArgs, addr: SocketAddr, auth_enabled: bool) -> Result<Option<JudgeStack>> {
    let provider = args.client.provider.as_str();
    let client = cli_judge(&args.client).context("the judge cannot be set up")?;
    let Some(program) = client.program() else {
        let program = cli_program(&args.client);
        if args.mode == JudgeMode::On {
            bail!(
                "--judge on with --judge-provider {provider} needs the program `{program}`, and \
                 it was not found: install it, give --judge-cli-path, or use --judge auto to \
                 run without a judge"
            );
        }
        tracing::info!(
            provider,
            "judge off: --judge auto and the program of the provider was not found"
        );
        return Ok(None);
    };
    check_reach(args, addr, auth_enabled)?;
    let id = client.id().to_owned();
    // The judge id is the price name: a call of it costs nothing.
    let (judge, settings) = wrap(args, client as Arc<dyn Judge>, id.clone());
    tracing::info!(
        mode = args.mode.as_str(),
        provider,
        judge = %id,
        program = %program.display(),
        effort = args.client.effort.as_str(),
        default_on = settings.default_on,
        timeout_secs = args.client.timeout_secs,
        max_fields_per_request = settings.max_fields_per_request,
        max_fields_per_batch = settings.max_fields_per_batch,
        max_text_chars = settings.max_text_chars,
        concurrency = args.concurrency(),
        max_calls_per_minute = args.max_calls_per_minute,
        cache_entries = args.cache_entries,
        "judge on"
    );
    Ok(Some(JudgeStack {
        judge,
        settings,
        model: id,
        metered: false,
        client: None,
    }))
}

#[cfg(feature = "judge")]
fn build(
    args: &JudgeArgs,
    key: &Secret,
    addr: SocketAddr,
    auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    check_reach(args, addr, auth_enabled)?;
    let client = anthropic(&args.client, key).context("the judge cannot be set up")?;
    let model = args.client.model().unwrap_or(DEFAULT_MODEL).to_owned();
    let inner: Arc<dyn Judge> = Arc::clone(&client) as Arc<dyn Judge>;
    let (judge, settings) = wrap(args, inner, model.clone());
    tracing::info!(
        mode = args.mode.as_str(),
        provider = args.client.provider.as_str(),
        model = %model,
        effort = args.client.effort.as_str(),
        fallbacks = client.fallbacks_on(),
        default_on = settings.default_on,
        timeout_secs = args.client.timeout_secs,
        max_fields_per_request = settings.max_fields_per_request,
        max_fields_per_batch = settings.max_fields_per_batch,
        max_text_chars = settings.max_text_chars,
        max_tokens = args.client.max_tokens(),
        concurrency = args.concurrency(),
        max_calls_per_minute = args.max_calls_per_minute,
        max_usd_per_hour = args.max_usd_per_hour,
        cache_entries = args.cache_entries,
        base_host = base_host(args.client.base_url()),
        "judge on"
    );
    Ok(Some(JudgeStack {
        judge,
        settings,
        model,
        metered: true,
        client: Some(client),
    }))
}

/// The judge of `assess --judge`. The command asked for one, so a missing
/// key or a missing program is an error. `env_key` is called only with the
/// anthropic provider.
#[cfg(feature = "judge")]
pub(crate) fn for_cli(
    client: &JudgeClientArgs,
    env_key: impl FnOnce() -> Option<Secret>,
) -> Result<JudgeStack> {
    client.check_provider_flags()?;
    if client.provider.is_cli() {
        let judge = cli_judge(client).context("the judge cannot be set up")?;
        if !judge.is_installed() {
            bail!(
                "--judge with --judge-provider {} needs the program `{}`, and it was not found: \
                 install it or give --judge-cli-path",
                client.provider.as_str(),
                cli_program(client)
            );
        }
        let id = judge.id().to_owned();
        return Ok(JudgeStack {
            judge: judge as Arc<dyn Judge>,
            settings: settings(client),
            model: id,
            metered: false,
            client: None,
        });
    }
    let Some(key) = resolve_key(client.key_file.as_deref(), env_key())? else {
        bail!("--judge needs an API key: set {KEY_ENV} or give --anthropic-api-key-file");
    };
    let judge = anthropic(client, &key).context("the judge cannot be set up")?;
    Ok(JudgeStack {
        judge: Arc::clone(&judge) as Arc<dyn Judge>,
        settings: settings(client),
        model: client.model().unwrap_or(DEFAULT_MODEL).to_owned(),
        metered: true,
        client: Some(judge),
    })
}

#[cfg(not(feature = "judge"))]
pub(crate) fn for_cli(
    _client: &JudgeClientArgs,
    _env_key: impl FnOnce() -> Option<Secret>,
) -> Result<JudgeStack> {
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

    /// The arguments of a test. clap also reads the environment of the
    /// test process, so a developer who has `ASSESSMENT_JUDGE_PROVIDER`
    /// set would get another provider than the test means. A test that
    /// names no provider gets the default one, by a flag: a flag wins
    /// over the variable.
    fn with_provider<'a>(extra: &[&'a str]) -> Vec<&'a str> {
        let mut argv = vec!["assessment-engine"];
        if !extra.contains(&"--judge-provider") {
            argv.extend_from_slice(&["--judge-provider", "anthropic"]);
        }
        argv.extend_from_slice(extra);
        argv
    }

    fn judge_args(extra: &[&str]) -> JudgeArgs {
        Cli::try_parse_from(with_provider(extra))
            .expect("parses")
            .serve
            .judge
    }

    #[test]
    fn a_test_names_its_provider_whatever_the_environment_says() {
        // The flag is on the command line of every test that names no
        // provider, so `ASSESSMENT_JUDGE_PROVIDER` cannot change it.
        assert_eq!(
            with_provider(&["--judge", "on"]),
            [
                "assessment-engine",
                "--judge-provider",
                "anthropic",
                "--judge",
                "on"
            ]
        );
        assert_eq!(
            with_provider(&["--judge-provider", "codex-cli"]),
            ["assessment-engine", "--judge-provider", "codex-cli"]
        );
        assert_eq!(judge_args(&[]).client.provider, ProviderArg::Anthropic);
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
        assert_eq!(args.concurrency, None);
        assert_eq!(args.client.model, None);
        assert_eq!(args.default, Switch::On);
        assert_eq!(args.client.provider, ProviderArg::Anthropic);
        assert_eq!(args.client.cli_path, None);
        assert_eq!(args.client.model(), Some("claude-opus-5-5"));
        assert_eq!(args.client.effort, EffortArg::Low);
        assert_eq!(args.client.timeout_secs, 15);
        assert_eq!(args.client.max_fields_per_request, 4);
        assert_eq!(args.max_fields_per_batch, 16);
        assert_eq!(args.concurrency(), 8);
        assert_eq!(args.max_calls_per_minute, 60);
        assert!((args.max_usd_per_hour - 10.0).abs() < f64::EPSILON);
        assert_eq!(args.max_text_chars, 8000);
        assert_eq!(args.client.max_tokens(), 4096);
        assert_eq!(args.cache_entries, 4096);
        assert_eq!(args.client.fallbacks(), FallbacksArg::Auto);
        assert_eq!(args.client.base_url(), "https://api.anthropic.com");
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
        assert_eq!(args.client.model(), Some("claude-haiku-4-5"));
        assert_eq!(args.client.effort, EffortArg::Xhigh);
        assert_eq!(args.client.timeout_secs, 7);
        assert_eq!(args.client.max_fields_per_request, 2);
        assert_eq!(args.max_fields_per_batch, 9);
        assert_eq!(args.concurrency(), 3);
        assert_eq!(args.max_calls_per_minute, 30);
        assert!((args.max_usd_per_hour - 2.5).abs() < f64::EPSILON);
        assert_eq!(args.max_text_chars, 500);
        assert_eq!(args.client.max_tokens(), 1024);
        assert_eq!(args.cache_entries, 64);
        assert_eq!(args.client.fallbacks(), FallbacksArg::Off);
        assert_eq!(args.client.base_url(), LOCAL_BASE);
        assert_eq!(
            args.client.key_file.as_deref(),
            Some(Path::new("/run/secrets/key"))
        );
        assert!(args.allow_unauthenticated);
    }

    #[test]
    fn bad_judge_values_are_rejected() {
        let bad: [&[&str]; 13] = [
            &["--judge", "maybe"],
            &["--judge-provider", "openai"],
            &["--judge-provider", "codex"],
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
        assert_eq!(args.client.model(), Some("claude-haiku-4-5"));
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
        let stack = from_args(&args, || None, addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
        // A key that is set is not even looked at.
        let stack = from_args(&args, key, addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
    }

    #[test]
    fn on_without_a_key_is_an_error_and_auto_is_off() {
        let on = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
        let err = from_args(&on, || None, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");
        // A key of white space only is no key.
        let blank = Some(Secret("  \n".to_owned()));
        let err = from_args(&on, move || blank, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");

        let auto = judge_args(&["--judge", "auto", "--judge-base-url", LOCAL_BASE]);
        let stack = from_args(&auto, || None, addr(LOOPBACK), true).expect("starts");
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
        let err = from_args(&args, key, addr(LOOPBACK), true).expect_err("no file");
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
        let err = from_args(&args, || None, addr(LOOPBACK), true).expect_err("empty file");
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
        let err = from_args(&on, key, addr(LOOPBACK), true).expect_err("no judge");
        assert!(err.to_string().contains("built without"), "{err}");
        let auto = judge_args(&["--judge", "auto"]);
        assert!(
            from_args(&auto, key, addr(LOOPBACK), true)
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
            let stack = from_args(&args, || None, addr(LOOPBACK), false)
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
            let stack = from_args(&args, key, addr(LOOPBACK), false)
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
            let err = from_args(&args, key, addr(PUBLIC), false).expect_err("open to anyone");
            assert!(
                err.to_string().contains("--judge-allow-unauthenticated"),
                "{err}"
            );
            assert!(!format!("{err:#}").contains(KEY));

            // Each of the three ways out works.
            assert!(from_args(&args, key, addr(PUBLIC), true).is_ok());
            assert!(from_args(&args, key, addr(LOOPBACK), false).is_ok());
            assert!(from_args(&args, key, addr("[::1]:50061"), false).is_ok());
            let allowed = judge_args(&[
                "--judge",
                "on",
                "--judge-base-url",
                LOCAL_BASE,
                "--judge-allow-unauthenticated",
            ]);
            assert!(from_args(&allowed, key, addr(PUBLIC), false).is_ok());
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
                let err = from_args(&args, key, addr(LOOPBACK), true).expect_err("refused");
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
                    from_args(&args, key, addr(LOOPBACK), true).is_ok(),
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
                || Some(Secret(bad_key.to_owned())),
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
            let err = from_args(&args, key, addr(LOOPBACK), true).expect_err("a bad model");
            assert!(format!("{err:#}").contains("model"), "{err:#}");
        }

        /// A program that exists. No test in this file starts it.
        fn installed() -> String {
            std::env::current_exe()
                .expect("the test binary has a path")
                .to_str()
                .expect("the path is UTF-8")
                .to_owned()
        }

        /// A key reader for a provider that must not read the key.
        fn no_key_is_read() -> Option<Secret> {
            panic!("a command line provider read the API key")
        }

        const MISSING: &str = "/nonexistent/assessment-judge-cli";

        #[test]
        fn the_provider_flags_parse_and_change_the_defaults() {
            let args = judge_args(&["--judge-provider", "claude-cli"]);
            assert_eq!(args.client.provider, ProviderArg::ClaudeCli);
            let args = judge_args(&[
                "--judge-provider",
                "codex-cli",
                "--judge-cli-path",
                "/opt/bin/codex",
            ]);
            assert_eq!(args.client.provider, ProviderArg::CodexCli);
            assert_eq!(
                args.client.cli_path.as_deref(),
                Some(Path::new("/opt/bin/codex"))
            );
            if std::env::vars_os()
                .any(|(name, _)| name.to_string_lossy().starts_with("ASSESSMENT_JUDGE_"))
            {
                return;
            }
            // The model: the default for Claude, the program's own for Codex.
            assert_eq!(args.client.model(), None);
            let claude = judge_args(&["--judge-provider", "claude-cli"]);
            assert_eq!(claude.client.model(), Some("claude-opus-5-5"));
            let named = judge_args(&["--judge-provider", "codex-cli", "--judge-model", "m-1"]);
            assert_eq!(named.client.model(), Some("m-1"));
            // Calls at once: 2 for a command line, unless the operator says.
            assert_eq!(args.concurrency(), 2);
            assert_eq!(claude.concurrency(), 2);
            assert_eq!(judge_args(&[]).concurrency(), 8);
            let set = judge_args(&["--judge-provider", "codex-cli", "--judge-concurrency", "5"]);
            assert_eq!(set.concurrency(), 5);
        }

        #[test]
        fn a_command_line_provider_needs_no_key_and_reads_none() {
            let program = installed();
            for (provider, model, id) in [
                ("codex-cli", None, "codex-cli/default"),
                ("codex-cli", Some("m-1"), "codex-cli/m-1"),
                ("claude-cli", None, "claude-cli/claude-opus-5-5"),
                (
                    "claude-cli",
                    Some("claude-haiku-4-5"),
                    "claude-cli/claude-haiku-4-5",
                ),
            ] {
                let mut flags = vec![
                    "--judge",
                    "on",
                    "--judge-provider",
                    provider,
                    "--judge-cli-path",
                    &program,
                ];
                if let Some(model) = model {
                    flags.extend(["--judge-model", model]);
                }
                let args = judge_args(&flags);
                let stack = from_args(&args, no_key_is_read, addr(LOOPBACK), false)
                    .expect("starts")
                    .expect("a judge");
                assert_eq!(stack.judge.id(), id);
                assert_eq!(stack.model, id);
                assert!(!stack.metered);
                // Nothing to probe: a probe would start the program.
                stack.spawn_startup_probe();

                let stack = for_cli(&args.client, no_key_is_read).expect("a judge");
                assert_eq!(stack.judge.id(), id);
                assert!(!stack.metered);
            }
            // The API provider is billed.
            let args = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
            let stack = from_args(&args, key, addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert!(stack.metered);
        }

        #[test]
        fn a_missing_program_is_an_error_with_on_and_no_judge_with_auto() {
            for provider in ["claude-cli", "codex-cli"] {
                let on = judge_args(&[
                    "--judge",
                    "on",
                    "--judge-provider",
                    provider,
                    "--judge-cli-path",
                    MISSING,
                ]);
                let err =
                    from_args(&on, no_key_is_read, addr(LOOPBACK), true).expect_err("no program");
                let text = err.to_string();
                assert!(text.contains("was not found"), "{text}");
                assert!(text.contains(MISSING), "{text}");
                assert!(text.contains(provider), "{text}");

                let auto = judge_args(&[
                    "--judge",
                    "auto",
                    "--judge-provider",
                    provider,
                    "--judge-cli-path",
                    MISSING,
                ]);
                let stack = from_args(&auto, no_key_is_read, addr(LOOPBACK), true).expect("starts");
                assert!(stack.is_none());

                let err = for_cli(&on.client, no_key_is_read).expect_err("no program");
                assert!(err.to_string().contains("was not found"), "{err}");
            }
        }

        #[test]
        fn the_api_only_flags_are_refused_with_a_command_line_provider() {
            let program = installed();
            let key_path = key_file("cli-provider", KEY);
            let key_path_text = key_path.to_str().expect("the path is UTF-8").to_owned();
            let refused: [(&[&str], &str); 6] = [
                (&["--judge-base-url", LOCAL_BASE], "--judge-base-url"),
                (&["--judge-fallbacks", "off"], "--judge-fallbacks"),
                (&["--judge-max-tokens", "1024"], "--judge-max-tokens"),
                (
                    &["--anthropic-api-key-file", &key_path_text],
                    "--anthropic-api-key-file",
                ),
                (&["--judge-effort", "xhigh"], "--judge-effort xhigh"),
                (&["--judge-effort", "max"], "--judge-effort max"),
            ];
            for provider in ["claude-cli", "codex-cli"] {
                for (extra, name) in refused {
                    // Also with `auto`: a flag that cannot work is a mistake.
                    for mode in ["on", "auto"] {
                        let mut flags = vec![
                            "--judge",
                            mode,
                            "--judge-provider",
                            provider,
                            "--judge-cli-path",
                            &program,
                        ];
                        flags.extend_from_slice(extra);
                        let args = judge_args(&flags);
                        let err = from_args(&args, no_key_is_read, addr(LOOPBACK), true)
                            .expect_err("refused");
                        let text = format!("{err:#}");
                        assert!(text.contains(name), "{provider} {mode}: {text}");
                        assert!(text.contains(provider), "{provider} {mode}: {text}");
                        assert!(!text.contains(KEY), "{text}");
                        assert!(for_cli(&args.client, no_key_is_read).is_err());
                    }
                }
                // The other efforts and the spend cap are accepted.
                let args = judge_args(&[
                    "--judge",
                    "on",
                    "--judge-provider",
                    provider,
                    "--judge-cli-path",
                    &program,
                    "--judge-effort",
                    "high",
                    "--judge-max-usd-per-hour",
                    "0",
                ]);
                assert!(from_args(&args, no_key_is_read, addr(LOOPBACK), true).is_ok());
            }
            // With `off` nothing is checked.
            let off = judge_args(&[
                "--judge",
                "off",
                "--judge-provider",
                "codex-cli",
                "--judge-base-url",
                LOCAL_BASE,
            ]);
            assert!(
                from_args(&off, no_key_is_read, addr(LOOPBACK), true)
                    .expect("starts")
                    .is_none()
            );
            let _ = std::fs::remove_dir_all(key_path.parent().expect("the file has a directory"));
        }

        #[test]
        fn the_program_path_is_refused_with_the_api_provider() {
            let program = installed();
            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-base-url",
                LOCAL_BASE,
                "--judge-cli-path",
                &program,
            ]);
            let err = from_args(&args, key, addr(LOOPBACK), true).expect_err("refused");
            assert!(err.to_string().contains("--judge-cli-path"), "{err}");
            assert!(for_cli(&args.client, key).is_err());
        }

        #[test]
        fn a_command_line_judge_without_auth_must_stay_on_loopback() {
            let program = installed();
            let flags = [
                "--judge",
                "on",
                "--judge-provider",
                "codex-cli",
                "--judge-cli-path",
                &program,
            ];
            let args = judge_args(&flags);
            let err =
                from_args(&args, no_key_is_read, addr(PUBLIC), false).expect_err("open to anyone");
            assert!(
                err.to_string().contains("--judge-allow-unauthenticated"),
                "{err}"
            );
            assert!(from_args(&args, no_key_is_read, addr(PUBLIC), true).is_ok());
            assert!(from_args(&args, no_key_is_read, addr(LOOPBACK), false).is_ok());
            let mut allowed = flags.to_vec();
            allowed.push("--judge-allow-unauthenticated");
            let args = judge_args(&allowed);
            assert!(from_args(&args, no_key_is_read, addr(PUBLIC), false).is_ok());
        }

        #[test]
        fn a_bad_model_of_a_command_line_provider_is_an_error() {
            let program = installed();
            for model in ["bad model", "--dangerous"] {
                let args = judge_args(&[
                    "--judge",
                    "on",
                    "--judge-provider",
                    "codex-cli",
                    "--judge-cli-path",
                    &program,
                    &format!("--judge-model={model}"),
                ]);
                let err = from_args(&args, no_key_is_read, addr(LOOPBACK), true)
                    .expect_err("a bad model");
                assert!(format!("{err:#}").contains("model"), "{err:#}");
            }
        }

        #[test]
        fn the_command_line_judge_needs_a_key() {
            let args = judge_args(&["--judge-base-url", LOCAL_BASE]);
            let err = for_cli(&args.client, || None).expect_err("no key");
            assert!(err.to_string().contains("needs an API key"), "{err}");
            let stack = for_cli(&args.client, key).expect("a judge");
            assert_eq!(stack.judge.id(), "anthropic/claude-opus-5-5");
            assert_eq!(stack.settings.max_fields_per_request, 4);
        }
    }
}
