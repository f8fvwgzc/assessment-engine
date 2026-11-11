//! The optional judge: its flags, the startup checks and the stack that
//! `serve` and `assess --judge` use.
//!
//! There are two kinds of provider. `anthropic` asks the Anthropic
//! Messages API and `openai` asks the OpenAI Chat Completions API, each
//! with an API key. The key comes from the variable of the provider
//! (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`) or from its key file. There is
//! no key flag, and a key is never logged or put into an error. The key of
//! the provider that was not chosen is never read, and neither is the
//! variable that names its key file. `claude-cli` and
//! `codex-cli` run an agent command line that is installed and logged in
//! on this machine. They need no key, and no key is read at all when one
//! of them is chosen.

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

/// The environment variable that holds the Anthropic API key.
pub(crate) const KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// The environment variable that holds the OpenAI API key.
pub(crate) const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";

const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-6.1-sol";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The OpenAI API root. Unlike the Anthropic one it includes `/v1`.
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// The environment variable that names the Anthropic key file.
pub(crate) const KEY_FILE_ENV: &str = "ANTHROPIC_API_KEY_FILE";
/// The environment variable that names the OpenAI key file.
pub(crate) const OPENAI_KEY_FILE_ENV: &str = "OPENAI_API_KEY_FILE";
const ANTHROPIC_KEY_FILE_FLAG: &str = "--anthropic-api-key-file";
const OPENAI_KEY_FILE_FLAG: &str = "--openai-api-key-file";
/// The host of each API. A base URL on the host of the other provider is
/// a setting that was left over from that provider.
const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
const OPENAI_API_HOST: &str = "api.openai.com";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_CONCURRENCY: usize = 8;
const DEFAULT_CLI_CONCURRENCY: usize = 2;

/// Whether the server has a judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum JudgeMode {
    /// No judge. Flagged fields are only reported.
    Off,
    /// A judge. The server does not start when the provider cannot be
    /// used: `anthropic` or `openai` without an API key, or a command
    /// line provider whose program is not found.
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
    /// The OpenAI Chat Completions API, with an API key. It is not a
    /// Claude model: the texts go to OpenAI, or to the host of
    /// `--judge-base-url`.
    Openai,
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
            ProviderArg::Openai => "openai",
            ProviderArg::ClaudeCli => "claude-cli",
            ProviderArg::CodexCli => "codex-cli",
        }
    }

    /// Whether the provider runs a command line program.
    pub(crate) fn is_cli(self) -> bool {
        matches!(self, ProviderArg::ClaudeCli | ProviderArg::CodexCli)
    }

    /// The environment variable that holds the API key of the provider.
    /// A command line provider has none.
    pub(crate) fn key_env(self) -> Option<&'static str> {
        match self {
            ProviderArg::Anthropic => Some(KEY_ENV),
            ProviderArg::Openai => Some(OPENAI_KEY_ENV),
            ProviderArg::ClaudeCli | ProviderArg::CodexCli => None,
        }
    }

    /// The environment variable that names the key file of the provider.
    /// A command line provider has none.
    pub(crate) fn key_file_env(self) -> Option<&'static str> {
        match self {
            ProviderArg::Anthropic => Some(KEY_FILE_ENV),
            ProviderArg::Openai => Some(OPENAI_KEY_FILE_ENV),
            ProviderArg::ClaudeCli | ProviderArg::CodexCli => None,
        }
    }

    /// The flag of the key file of the provider.
    fn key_file_flag(self) -> &'static str {
        match self {
            ProviderArg::Openai => OPENAI_KEY_FILE_FLAG,
            _ => ANTHROPIC_KEY_FILE_FLAG,
        }
    }

    /// The API host of the other API provider.
    fn other_api_host(self) -> Option<&'static str> {
        match self {
            ProviderArg::Anthropic => Some(OPENAI_API_HOST),
            ProviderArg::Openai => Some(ANTHROPIC_API_HOST),
            ProviderArg::ClaudeCli | ProviderArg::CodexCli => None,
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
    /// Who answers: the Anthropic or the OpenAI API with a key, or a
    /// command line program that is installed and logged in on this
    /// machine.
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

    /// The model the judge asks [default: claude-opus-5-5, gpt-6.1-sol
    /// with openai, or the program's own model with codex-cli].
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
    /// Only with the anthropic or the openai provider.
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

    /// The API root [default: https://api.anthropic.com, or
    /// https://api.openai.com/v1 with openai, where it includes `/v1`].
    /// It must be https, or http on a loopback address. Only with the
    /// anthropic or the openai provider. The two share this flag and its
    /// variable: change it when the provider changes.
    #[arg(
        long = "judge-base-url",
        env = "ASSESSMENT_JUDGE_BASE_URL",
        value_name = "URL"
    )]
    pub base_url: Option<String>,

    /// A file that holds the Anthropic API key. Without the flag the
    /// file that `ANTHROPIC_API_KEY_FILE` names is read, and without both
    /// the key is read from `ANTHROPIC_API_KEY`. There is no flag that
    /// takes the key itself. Only with the anthropic provider.
    // The variable is not a clap `env`: it is read only when the
    // anthropic provider is chosen, so it does not disturb another one.
    #[arg(long = "anthropic-api-key-file", value_name = "FILE")]
    pub key_file: Option<PathBuf>,

    /// A file that holds the OpenAI API key. Without the flag the file
    /// that `OPENAI_API_KEY_FILE` names is read, and without both the key
    /// is read from `OPENAI_API_KEY`. There is no flag that takes the key
    /// itself. Only with the openai provider.
    // Not a clap `env`, like the Anthropic one.
    #[arg(long = "openai-api-key-file", value_name = "FILE")]
    pub openai_key_file: Option<PathBuf>,
}

impl JudgeClientArgs {
    /// The model to ask. `None` leaves the choice to the program, which
    /// is the default of `codex-cli`.
    pub(crate) fn model(&self) -> Option<&str> {
        match (&self.model, self.provider) {
            (Some(model), _) => Some(model),
            (None, ProviderArg::CodexCli) => None,
            (None, ProviderArg::Openai) => Some(DEFAULT_OPENAI_MODEL),
            (None, ProviderArg::Anthropic | ProviderArg::ClaudeCli) => Some(DEFAULT_MODEL),
        }
    }

    /// The model of an API provider, which always has one.
    fn api_model(&self) -> &str {
        self.model().unwrap_or(match self.provider {
            ProviderArg::Openai => DEFAULT_OPENAI_MODEL,
            _ => DEFAULT_MODEL,
        })
    }

    /// The key file flag of the chosen provider.
    fn provider_key_file(&self) -> Option<&Path> {
        match self.provider {
            ProviderArg::Openai => self.openai_key_file.as_deref(),
            _ => self.key_file.as_deref(),
        }
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }

    pub(crate) fn fallbacks(&self) -> FallbacksArg {
        self.fallbacks.unwrap_or(FallbacksArg::Auto)
    }

    pub(crate) fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(match self.provider {
            ProviderArg::Openai => DEFAULT_OPENAI_BASE_URL,
            _ => DEFAULT_BASE_URL,
        })
    }

    /// Refuses a flag that the chosen provider cannot honour. A flag that
    /// is silently ignored would look like it worked.
    fn check_provider_flags(&self) -> Result<()> {
        let provider = self.provider.as_str();
        // The key file flag of another provider. Its key would not be
        // used. The variable of such a file is not looked at: it may be
        // set for every provider at once.
        let key_files = [
            (
                ANTHROPIC_KEY_FILE_FLAG,
                "anthropic",
                self.key_file.is_some() && self.provider != ProviderArg::Anthropic,
            ),
            (
                OPENAI_KEY_FILE_FLAG,
                "openai",
                self.openai_key_file.is_some() && self.provider != ProviderArg::Openai,
            ),
        ];
        for (flag, owner, set) in key_files {
            if set {
                bail!(
                    "{flag} is only for the {owner} provider and has no meaning with \
                     --judge-provider {provider}: remove it"
                );
            }
        }
        // A base URL on the API host of the other provider. The two share
        // one flag and one variable, so it is what stays behind when only
        // the provider is changed. The key would go to the wrong company.
        if let (Some(base_url), Some(other)) = (&self.base_url, self.provider.other_api_host())
            && host_name(base_url).eq_ignore_ascii_case(other)
        {
            bail!(
                "--judge-base-url (ASSESSMENT_JUDGE_BASE_URL) is `{other}`, the API of another \
                 provider than --judge-provider {provider}: change it or remove it"
            );
        }
        if !self.provider.is_cli() {
            if self.cli_path.is_some() {
                bail!(
                    "--judge-cli-path (ASSESSMENT_JUDGE_CLI_PATH) is only for a command line \
                     provider: set --judge-provider claude-cli or codex-cli, or remove it"
                );
            }
            // `auto` is the default. It turns fallbacks on for the default
            // Anthropic model only, so it asks nothing of another provider.
            let fallbacks_set =
                matches!(self.fallbacks, Some(FallbacksArg::On | FallbacksArg::Off));
            if self.provider == ProviderArg::Openai && fallbacks_set {
                bail!(
                    "--judge-fallbacks (ASSESSMENT_JUDGE_FALLBACKS) is only for the anthropic \
                     provider and has no meaning with --judge-provider {provider}: remove it"
                );
            }
            return Ok(());
        }
        let anthropic_only = [(
            "--judge-fallbacks (ASSESSMENT_JUDGE_FALLBACKS)",
            self.fallbacks.is_some(),
        )];
        for (flag, set) in anthropic_only {
            if set {
                bail!(
                    "{flag} is only for the anthropic provider and has no meaning with \
                     --judge-provider {provider}: remove it"
                );
            }
        }
        let api_only = [
            (
                "--judge-base-url (ASSESSMENT_JUDGE_BASE_URL)",
                self.base_url.is_some(),
            ),
            (
                "--judge-max-tokens (ASSESSMENT_JUDGE_MAX_TOKENS)",
                self.max_tokens.is_some(),
            ),
        ];
        for (flag, set) in api_only {
            if set {
                bail!(
                    "{flag} is only for the anthropic and openai providers and has no meaning \
                     with --judge-provider {provider}: remove it"
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

/// The value of the environment variable `name`, when it is set. `name`
/// is a variable of the chosen provider: its key, or the path of its key
/// file. So nothing of another provider is ever read.
pub(crate) fn key_from_env(name: &'static str) -> Option<Secret> {
    std::env::var(name).ok().map(Secret)
}

/// The key of an API provider: the file of its flag, else the file that
/// its key file variable names, else its key variable. `env` is called
/// with these two variables only. An empty key file variable is not set.
fn provider_key(
    client: &JudgeClientArgs,
    key_env: &'static str,
    env: impl Fn(&'static str) -> Option<Secret>,
) -> Result<Option<Secret>> {
    if let Some(path) = client.provider_key_file() {
        return resolve_key(Some(path), None);
    }
    let named = client
        .provider
        .key_file_env()
        .and_then(&env)
        .map(|path| PathBuf::from(path.0))
        .filter(|path| !path.as_os_str().is_empty());
    match named {
        Some(path) => resolve_key(Some(&path), None),
        None => resolve_key(None, env(key_env)),
    }
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

/// The host of a base URL without its port.
fn host_name(base_url: &str) -> &str {
    let host = base_host(base_url);
    if host.starts_with('[') {
        // An IPv6 address: the colons are its own.
        return host;
    }
    host.split_once(':').map_or(host, |(name, _)| name)
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
    /// Anthropic API, or the judge id of every other provider.
    pub model: String,
    /// Whether a call is billed per token. It is not with a command line
    /// provider, so no cost is reported for one.
    pub metered: bool,
    /// The API client, for the startup probe. A command line provider
    /// has none.
    #[cfg(feature = "judge")]
    client: Option<ApiClient>,
}

/// The client of an API provider.
#[cfg(feature = "judge")]
#[derive(Debug, Clone)]
enum ApiClient {
    Anthropic(Arc<assessment_judge::AnthropicJudge>),
    Openai(Arc<assessment_judge::OpenAiJudge>),
}

#[cfg(feature = "judge")]
impl ApiClient {
    fn judge(&self) -> Arc<dyn Judge> {
        match self {
            ApiClient::Anthropic(judge) => Arc::clone(judge) as Arc<dyn Judge>,
            ApiClient::Openai(judge) => Arc::clone(judge) as Arc<dyn Judge>,
        }
    }

    /// Whether the API may answer with another model. Only Anthropic has
    /// that.
    fn fallbacks_on(&self) -> bool {
        match self {
            ApiClient::Anthropic(judge) => judge.fallbacks_on(),
            ApiClient::Openai(_) => false,
        }
    }

    fn spawn_startup_probe(&self) {
        match self {
            ApiClient::Anthropic(judge) => drop(judge.spawn_startup_probe()),
            ApiClient::Openai(judge) => drop(judge.spawn_startup_probe()),
        }
    }
}

impl JudgeStack {
    /// Checks the key, the model and TLS in the background and logs the
    /// result. Startup does not wait for it. A command line provider is
    /// not probed: a probe would start the program and use its quota.
    pub(crate) fn spawn_startup_probe(&self) {
        #[cfg(feature = "judge")]
        if let Some(client) = &self.client {
            client.spawn_startup_probe();
        }
    }
}

fn settings(client: &JudgeClientArgs) -> JudgeSettings {
    JudgeSettings::default()
        .with_max_fields_per_request(client.max_fields_per_request)
        .with_timeout(Duration::from_secs(client.timeout_secs))
}

#[cfg(feature = "judge")]
fn effort(arg: EffortArg) -> assessment_judge::Effort {
    use assessment_judge::Effort;

    match arg {
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
        EffortArg::Xhigh => Effort::Xhigh,
        EffortArg::Max => Effort::Max,
    }
}

/// The client of the chosen API provider, and the name for the report.
#[cfg(feature = "judge")]
fn api_client(client: &JudgeClientArgs, key: &Secret) -> Result<(ApiClient, String)> {
    match client.provider {
        ProviderArg::Openai => {
            let judge = openai(client, key)?;
            // The id says which provider answered: `openai/<model>`.
            let name = judge.id().to_owned();
            Ok((ApiClient::Openai(judge), name))
        }
        _ => {
            let judge = anthropic(client, key)?;
            Ok((ApiClient::Anthropic(judge), client.api_model().to_owned()))
        }
    }
}

#[cfg(feature = "judge")]
fn openai(client: &JudgeClientArgs, key: &Secret) -> Result<Arc<assessment_judge::OpenAiJudge>> {
    use assessment_judge::{ApiKey, OpenAiConfig, OpenAiJudge};

    // The messages of these errors are fixed texts: they never hold the key.
    let key = ApiKey::new(&key.0).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let config = OpenAiConfig {
        base_url: client.base_url().to_owned(),
        model: client.api_model().to_owned(),
        effort: effort(client.effort),
        max_tokens: client.max_tokens(),
    };
    let judge = OpenAiJudge::new(key, config).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    Ok(Arc::new(judge))
}

#[cfg(feature = "judge")]
fn anthropic(
    client: &JudgeClientArgs,
    key: &Secret,
) -> Result<Arc<assessment_judge::AnthropicJudge>> {
    use assessment_judge::{AnthropicConfig, AnthropicJudge, ApiKey, Fallbacks};

    // The messages of these errors are fixed texts: they never hold the key.
    let key = ApiKey::new(&key.0).map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let config = AnthropicConfig {
        base_url: client.base_url().to_owned(),
        model: client.api_model().to_owned(),
        effort: effort(client.effort),
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
    use assessment_judge::{CliConfig, CliJudge, CliKind};

    let kind = match client.provider {
        ProviderArg::ClaudeCli => CliKind::ClaudeCode,
        ProviderArg::CodexCli => CliKind::Codex,
        ProviderArg::Anthropic | ProviderArg::Openai => bail!(
            "the {} provider is not a command line",
            client.provider.as_str()
        ),
    };
    let mut config = CliConfig::new(kind);
    config.program = client.cli_path.clone();
    config.model = client.model().map(str::to_owned);
    config.effort = effort(client.effort);
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
/// `env_key` reads the environment variable it is given. It is called only
/// with an API provider, and only with the variables of that provider: the
/// one that names its key file and the one that holds its key. So a
/// command line provider reads no key, and nothing of the other API
/// provider is ever read.
/// `addr` and `auth_enabled` say who can reach the server.
pub(crate) fn from_args(
    args: &JudgeArgs,
    env_key: impl Fn(&'static str) -> Option<Secret>,
    addr: SocketAddr,
    auth_enabled: bool,
) -> Result<Option<JudgeStack>> {
    if args.mode == JudgeMode::Off {
        return Ok(None);
    }
    args.client.check_provider_flags()?;
    let Some(key_env) = args.client.provider.key_env() else {
        return build_cli(args, addr, auth_enabled);
    };
    let key = provider_key(&args.client, key_env, env_key)?;
    let Some(key) = key else {
        if args.mode == JudgeMode::On {
            bail!(
                "--judge on with --judge-provider {} needs an API key: set {key_env} or give {} \
                 (use --judge auto to run without a judge when there is no key)",
                args.client.provider.as_str(),
                args.client.provider.key_file_flag()
            );
        }
        tracing::info!(
            provider = args.client.provider.as_str(),
            "judge off: --judge auto and no API key is set"
        );
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
    let (client, name) = api_client(&args.client, key).context("the judge cannot be set up")?;
    let model = args.client.api_model().to_owned();
    let (judge, settings) = wrap(args, client.judge(), model.clone());
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
        model: name,
        metered: true,
        client: Some(client),
    }))
}

/// The judge of `assess --judge`. The command asked for one, so a missing
/// key or a missing program is an error. `env_key` is called only with an
/// API provider, with the variables of that provider.
#[cfg(feature = "judge")]
pub(crate) fn for_cli(
    client: &JudgeClientArgs,
    env_key: impl Fn(&'static str) -> Option<Secret>,
) -> Result<JudgeStack> {
    client.check_provider_flags()?;
    let Some(key_env) = client.provider.key_env() else {
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
    };
    let Some(key) = provider_key(client, key_env, env_key)? else {
        bail!(
            "--judge with --judge-provider {} needs an API key: set {key_env} or give {}",
            client.provider.as_str(),
            client.provider.key_file_flag()
        );
    };
    let (api, name) = api_client(client, &key).context("the judge cannot be set up")?;
    Ok(JudgeStack {
        judge: api.judge(),
        settings: settings(client),
        model: name,
        metered: true,
        client: Some(api),
    })
}

#[cfg(not(feature = "judge"))]
pub(crate) fn for_cli(
    _client: &JudgeClientArgs,
    _env_key: impl Fn(&'static str) -> Option<Secret>,
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

    /// The environment of a test: the variable `key_env` holds `value`
    /// and the key file variable `file_env` is not set. A test fails when
    /// any other variable is asked for.
    fn env_of(
        key_env: &'static str,
        file_env: &'static str,
        value: &'static str,
    ) -> impl Fn(&'static str) -> Option<Secret> {
        move |name| {
            if name == file_env {
                return None;
            }
            assert_eq!(name, key_env, "the variable of another provider was read");
            Some(Secret(value.to_owned()))
        }
    }

    /// The key of the anthropic provider. It is there only when that
    /// provider asks for its own variable.
    fn key(name: &'static str) -> Option<Secret> {
        env_of(KEY_ENV, KEY_FILE_ENV, KEY)(name)
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
        // The key file variables are not read by clap, so they change
        // nothing here.
        assert_eq!(args.client.key_file, None);
        assert_eq!(args.client.openai_key_file, None);
        if std::env::vars_os()
            .any(|(name, _)| name.to_string_lossy().starts_with("ASSESSMENT_JUDGE_"))
        {
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
        assert_eq!(args.client.openai_key_file, None);
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
        let bad: [&[&str]; 14] = [
            &["--judge", "maybe"],
            &["--judge-provider", "gemini"],
            &["--judge-provider", "OpenAI"],
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
        for flag in [
            "--anthropic-api-key",
            "--openai-api-key",
            "--judge-api-key",
            "--api-key",
        ] {
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
        let stack = from_args(&args, |_| None, addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
        // A key that is set is not even looked at.
        let stack = from_args(&args, key, addr(PUBLIC), false).expect("starts");
        assert!(stack.is_none());
    }

    #[test]
    fn on_without_a_key_is_an_error_and_auto_is_off() {
        let on = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
        let err = from_args(&on, |_| None, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");
        // A key of white space only is no key.
        let blank = env_of(KEY_ENV, KEY_FILE_ENV, "  \n");
        let err = from_args(&on, blank, addr(LOOPBACK), true).expect_err("no key");
        assert!(err.to_string().contains("needs an API key"), "{err}");

        let auto = judge_args(&["--judge", "auto", "--judge-base-url", LOCAL_BASE]);
        let stack = from_args(&auto, |_| None, addr(LOOPBACK), true).expect("starts");
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
        let err = from_args(&args, |_| None, addr(LOOPBACK), true).expect_err("empty file");
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
        assert_eq!(host_name("http://127.0.0.1:9/v1/"), "127.0.0.1");
        assert_eq!(host_name("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(host_name("http://[::1]:9/v1"), "[::1]:9");
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
            let stack = from_args(&args, |_| None, addr(LOOPBACK), false)
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
                env_of(KEY_ENV, KEY_FILE_ENV, bad_key),
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
        fn no_key_is_read(name: &str) -> Option<Secret> {
            panic!("a command line provider read the API key in {name}")
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
            let openai_key_path = key_file("cli-provider-openai", KEY);
            let openai_key_path_text = openai_key_path
                .to_str()
                .expect("the path is UTF-8")
                .to_owned();
            let refused: [(&[&str], &str); 7] = [
                (&["--judge-base-url", LOCAL_BASE], "--judge-base-url"),
                (&["--judge-fallbacks", "off"], "--judge-fallbacks"),
                (&["--judge-max-tokens", "1024"], "--judge-max-tokens"),
                (
                    &["--anthropic-api-key-file", &key_path_text],
                    "--anthropic-api-key-file",
                ),
                (
                    &["--openai-api-key-file", &openai_key_path_text],
                    "--openai-api-key-file",
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
            let _ = std::fs::remove_dir_all(
                openai_key_path.parent().expect("the file has a directory"),
            );
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
            let err = for_cli(&args.client, |_| None).expect_err("no key");
            assert!(err.to_string().contains("needs an API key"), "{err}");
            let stack = for_cli(&args.client, key).expect("a judge");
            assert_eq!(stack.judge.id(), "anthropic/claude-opus-5-5");
            assert_eq!(stack.settings.max_fields_per_request, 4);
        }

        /// The key of the openai provider. It is there only when that
        /// provider asks for its own variable.
        fn openai_key(name: &'static str) -> Option<Secret> {
            env_of(OPENAI_KEY_ENV, OPENAI_KEY_FILE_ENV, KEY)(name)
        }

        const OPENAI_LOCAL_BASE: &str = "http://127.0.0.1:9/v1";

        #[test]
        fn the_openai_provider_parses_and_has_its_own_defaults() {
            let args = judge_args(&["--judge-provider", "openai"]);
            assert_eq!(args.client.provider, ProviderArg::Openai);
            assert_eq!(args.client.provider.as_str(), "openai");
            assert!(!args.client.provider.is_cli());
            assert_eq!(args.client.provider.key_env(), Some("OPENAI_API_KEY"));
            assert_eq!(ProviderArg::Anthropic.key_env(), Some("ANTHROPIC_API_KEY"));
            assert_eq!(ProviderArg::ClaudeCli.key_env(), None);
            assert_eq!(ProviderArg::CodexCli.key_env(), None);
            assert_eq!(
                args.client.provider.key_file_env(),
                Some("OPENAI_API_KEY_FILE")
            );
            assert_eq!(
                ProviderArg::Anthropic.key_file_env(),
                Some("ANTHROPIC_API_KEY_FILE")
            );
            assert_eq!(ProviderArg::ClaudeCli.key_file_env(), None);
            assert_eq!(ProviderArg::CodexCli.key_file_env(), None);
            if std::env::vars_os()
                .any(|(name, _)| name.to_string_lossy().starts_with("ASSESSMENT_JUDGE_"))
            {
                return;
            }
            assert_eq!(args.client.model(), Some("gpt-6.1-sol"));
            assert_eq!(args.client.base_url(), "https://api.openai.com/v1");
            assert_eq!(args.client.max_tokens(), 4096);
            assert_eq!(args.client.effort, EffortArg::Low);
            // An API provider: 8 calls at once, like anthropic.
            assert_eq!(args.concurrency(), 8);
            let named = judge_args(&["--judge-provider", "openai", "--judge-model", "gpt-6-luna"]);
            assert_eq!(named.client.model(), Some("gpt-6-luna"));
            // The default provider did not change.
            assert_eq!(judge_args(&[]).client.provider, ProviderArg::Anthropic);
            assert_eq!(judge_args(&[]).client.model(), Some("claude-opus-5-5"));
        }

        #[test]
        fn the_openai_judge_is_built_with_its_own_key_only() {
            let flags = [
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ];
            let args = judge_args(&flags);
            // `openai_key` fails the test when another variable is asked for.
            let stack = from_args(&args, openai_key, addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert_eq!(stack.judge.id(), "openai/gpt-6.1-sol");
            assert_eq!(stack.model, "openai/gpt-6.1-sol");
            assert!(stack.metered);
            assert!(!format!("{stack:?}").contains(KEY));

            let stack = for_cli(&args.client, openai_key).expect("a judge");
            assert_eq!(stack.judge.id(), "openai/gpt-6.1-sol");
            assert_eq!(stack.model, "openai/gpt-6.1-sol");
            assert!(stack.metered);
            assert_eq!(stack.settings.max_fields_per_request, 4);

            // Every effort, the token limit and another model are taken.
            for effort in ["low", "medium", "high", "xhigh", "max"] {
                let mut flags = flags.to_vec();
                flags.extend([
                    "--judge-effort",
                    effort,
                    "--judge-max-tokens",
                    "1024",
                    "--judge-model",
                    "gpt-6-luna",
                    "--judge-fallbacks",
                    "auto",
                ]);
                let args = judge_args(&flags);
                let stack = from_args(&args, openai_key, addr(LOOPBACK), true)
                    .expect("starts")
                    .expect("a judge");
                assert_eq!(stack.judge.id(), "openai/gpt-6-luna", "{effort}");
            }
        }

        #[test]
        fn the_anthropic_provider_never_reads_the_openai_key() {
            let args = judge_args(&["--judge", "on", "--judge-base-url", LOCAL_BASE]);
            // `key` fails the test when another variable is asked for.
            let stack = from_args(&args, key, addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert_eq!(stack.judge.id(), "anthropic/claude-opus-5-5");
            assert_eq!(stack.model, "claude-opus-5-5");
            // With only the key of the other provider there is no key.
            // Every variable of the other provider is set, and none of its own.
            let only_other = |name: &'static str| {
                let value = match name {
                    OPENAI_KEY_ENV => KEY,
                    OPENAI_KEY_FILE_ENV => "/nonexistent/openai-key",
                    _ => return None,
                };
                Some(Secret(value.to_owned()))
            };
            let err = from_args(&args, only_other, addr(LOOPBACK), true).expect_err("no key");
            assert!(err.to_string().contains("ANTHROPIC_API_KEY"), "{err}");

            let openai = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ]);
            let only_other = |name: &'static str| {
                let value = match name {
                    KEY_ENV => KEY,
                    KEY_FILE_ENV => "/nonexistent/anthropic-key",
                    _ => return None,
                };
                Some(Secret(value.to_owned()))
            };
            let err = from_args(&openai, only_other, addr(LOOPBACK), true).expect_err("no key");
            assert!(err.to_string().contains("OPENAI_API_KEY"), "{err}");
        }

        #[test]
        fn openai_on_without_a_key_is_an_error_and_auto_is_off() {
            let on = judge_args(&["--judge", "on", "--judge-provider", "openai"]);
            let err = from_args(&on, |_| None, addr(LOOPBACK), true).expect_err("no key");
            let text = err.to_string();
            assert!(text.contains("needs an API key"), "{text}");
            assert!(text.contains("OPENAI_API_KEY"), "{text}");
            assert!(text.contains("--openai-api-key-file"), "{text}");
            assert!(!text.contains("ANTHROPIC"), "{text}");
            assert!(!text.contains("--anthropic-api-key-file"), "{text}");
            let blank = env_of(OPENAI_KEY_ENV, OPENAI_KEY_FILE_ENV, " \n");
            assert!(from_args(&on, blank, addr(LOOPBACK), true).is_err());

            let auto = judge_args(&["--judge", "auto", "--judge-provider", "openai"]);
            let stack = from_args(&auto, |_| None, addr(LOOPBACK), true).expect("starts");
            assert!(stack.is_none());

            let err = for_cli(&on.client, |_| None).expect_err("no key");
            let text = err.to_string();
            assert!(text.contains("OPENAI_API_KEY"), "{text}");
            assert!(text.contains("--openai-api-key-file"), "{text}");
            assert!(!text.contains("ANTHROPIC"), "{text}");
            // The message of the default provider still names its own.
            let anthropic = judge_args(&["--judge", "on"]);
            let err = from_args(&anthropic, |_| None, addr(LOOPBACK), true).expect_err("no key");
            let text = err.to_string();
            assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
            assert!(text.contains("--anthropic-api-key-file"), "{text}");
            assert!(!text.contains("OPENAI"), "{text}");
        }

        #[test]
        fn the_openai_key_file_is_trimmed_and_wins_over_the_environment() {
            let path = key_file("openai-trimmed", &format!("\n  {KEY}  \n\n"));
            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
                "--openai-api-key-file",
                path.to_str().expect("the path is UTF-8"),
            ]);
            assert_eq!(args.client.provider_key_file(), Some(path.as_path()));
            let stack = from_args(&args, |_| None, addr(LOOPBACK), false)
                .expect("starts")
                .expect("a judge");
            assert_eq!(stack.judge.id(), "openai/gpt-6.1-sol");
            assert!(!format!("{stack:?}").contains(KEY));
            let _ = std::fs::remove_dir_all(path.parent().expect("the file has a directory"));

            // A key file that was asked for must be readable, in `auto` too.
            let missing = std::env::temp_dir().join("assessment-server-judge-no-openai-key-file");
            let args = judge_args(&[
                "--judge",
                "auto",
                "--judge-provider",
                "openai",
                "--openai-api-key-file",
                missing.to_str().expect("the path is UTF-8"),
            ]);
            let err = from_args(&args, openai_key, addr(LOOPBACK), true).expect_err("no file");
            let text = format!("{err:#}");
            assert!(text.contains("failed to read the API key file"), "{text}");
            assert!(!text.contains(KEY), "{text}");
        }

        #[test]
        fn a_key_file_of_another_provider_is_refused() {
            let path = key_file("other-provider", KEY);
            let path_text = path.to_str().expect("the path is UTF-8").to_owned();
            let cases = [
                ("openai", "--anthropic-api-key-file", "anthropic"),
                ("anthropic", "--openai-api-key-file", "openai"),
            ];
            for (provider, flag, owner) in cases {
                for mode in ["on", "auto"] {
                    let args = judge_args(&[
                        "--judge",
                        mode,
                        "--judge-provider",
                        provider,
                        flag,
                        &path_text,
                    ]);
                    // No key is read before the flags are checked.
                    let err = from_args(&args, no_key_is_read, addr(LOOPBACK), true)
                        .expect_err("refused");
                    let text = format!("{err:#}");
                    assert!(text.contains(flag), "{provider} {mode}: {text}");
                    assert!(
                        text.contains(&format!("is only for the {owner} provider")),
                        "{text}"
                    );
                    assert!(
                        text.contains(&format!("--judge-provider {provider}: remove it")),
                        "{text}"
                    );
                    assert!(!text.contains(KEY), "{text}");
                    assert!(for_cli(&args.client, no_key_is_read).is_err());
                }
            }
            let _ = std::fs::remove_dir_all(path.parent().expect("the file has a directory"));
        }

        /// An environment where both providers have a key and a key file
        /// variable, as a container that is set up for both has. It notes
        /// the names that were read.
        fn both_providers<'a>(
            anthropic_file: &'a str,
            openai_file: &'a str,
            read: &'a std::sync::Mutex<Vec<&'static str>>,
        ) -> impl Fn(&'static str) -> Option<Secret> + 'a {
            move |name| {
                read.lock().expect("the lock").push(name);
                let value = match name {
                    KEY_ENV | OPENAI_KEY_ENV => "the-key-of-the-variable",
                    KEY_FILE_ENV => anthropic_file,
                    OPENAI_KEY_FILE_ENV => openai_file,
                    other => panic!("an unknown variable was read: {other}"),
                };
                Some(Secret(value.to_owned()))
            }
        }

        #[test]
        fn a_key_file_variable_of_another_provider_is_not_read() {
            // The file of the other provider does not exist. A provider
            // that looked at it would fail to start.
            const MISSING_FILE: &str = "/nonexistent/assessment-judge-other-key";
            let path = key_file("own-variable", &format!(" {KEY}\n"));
            let own = path.to_str().expect("the path is UTF-8");
            let cases = [
                (
                    "anthropic",
                    LOCAL_BASE,
                    own,
                    MISSING_FILE,
                    [KEY_FILE_ENV, KEY_ENV],
                    "anthropic/claude-opus-5-5",
                ),
                (
                    "openai",
                    OPENAI_LOCAL_BASE,
                    MISSING_FILE,
                    own,
                    [OPENAI_KEY_FILE_ENV, OPENAI_KEY_ENV],
                    "openai/gpt-6.1-sol",
                ),
            ];
            for (provider, base, anthropic_file, openai_file, own_names, id) in cases {
                let args = judge_args(&[
                    "--judge",
                    "on",
                    "--judge-provider",
                    provider,
                    "--judge-base-url",
                    base,
                ]);
                // clap did not read the variables into the flags.
                assert_eq!(args.client.provider_key_file(), None);
                let read = std::sync::Mutex::new(Vec::new());
                let env = both_providers(anthropic_file, openai_file, &read);
                let stack = from_args(&args, &env, addr(LOOPBACK), false)
                    .expect("starts")
                    .expect("a judge");
                assert_eq!(stack.judge.id(), id);
                assert!(for_cli(&args.client, &env).is_ok());
                let read = read.lock().expect("the lock");
                assert!(!read.is_empty());
                assert!(
                    read.iter().all(|name| own_names.contains(name)),
                    "{provider}: {read:?}"
                );
                // The file wins: the key variable was not needed.
                assert!(read.iter().all(|name| *name == own_names[0]), "{read:?}");
            }
            let _ = std::fs::remove_dir_all(path.parent().expect("the file has a directory"));
        }

        #[test]
        fn a_key_file_variable_is_read_like_the_flag() {
            let missing = |name: &'static str| match name {
                OPENAI_KEY_FILE_ENV => Some(Secret("/nonexistent/openai-key-file".to_owned())),
                OPENAI_KEY_ENV => Some(Secret(KEY.to_owned())),
                other => panic!("the variable of another provider was read: {other}"),
            };
            // A file that was asked for must be readable, in `auto` too.
            for mode in ["on", "auto"] {
                let args = judge_args(&["--judge", mode, "--judge-provider", "openai"]);
                let err = from_args(&args, missing, addr(LOOPBACK), true).expect_err("no file");
                let text = format!("{err:#}");
                assert!(text.contains("failed to read the API key file"), "{text}");
                assert!(text.contains("/nonexistent/openai-key-file"), "{text}");
                assert!(!text.contains(KEY), "{text}");
            }
            // The flag wins over the variable, and an empty variable is not set.
            let path = key_file("flag-wins", KEY);
            let flagged = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
                "--openai-api-key-file",
                path.to_str().expect("the path is UTF-8"),
            ]);
            assert!(from_args(&flagged, missing, addr(LOOPBACK), false).is_ok());
            let empty = |name: &'static str| match name {
                OPENAI_KEY_FILE_ENV => Some(Secret(String::new())),
                _ => Some(Secret(KEY.to_owned())),
            };
            let plain = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ]);
            assert!(from_args(&plain, empty, addr(LOOPBACK), false).is_ok());
            let _ = std::fs::remove_dir_all(path.parent().expect("the file has a directory"));
        }

        #[test]
        fn a_base_url_on_the_host_of_the_other_provider_is_refused() {
            let cases = [
                ("openai", "https://api.anthropic.com", "api.anthropic.com"),
                (
                    "openai",
                    "https://API.Anthropic.com:443/v1/",
                    "api.anthropic.com",
                ),
                ("anthropic", "https://api.openai.com/v1", "api.openai.com"),
                ("anthropic", "https://user@api.openai.com", "api.openai.com"),
            ];
            for (provider, base, host) in cases {
                for mode in ["on", "auto"] {
                    let args = judge_args(&[
                        "--judge",
                        mode,
                        "--judge-provider",
                        provider,
                        "--judge-base-url",
                        base,
                    ]);
                    // It is refused before a key is read.
                    let err = from_args(&args, no_key_is_read, addr(LOOPBACK), true)
                        .expect_err("refused");
                    assert_eq!(
                        err.to_string(),
                        format!(
                            "--judge-base-url (ASSESSMENT_JUDGE_BASE_URL) is `{host}`, the API \
                             of another provider than --judge-provider {provider}: change it \
                             or remove it"
                        )
                    );
                    assert!(for_cli(&args.client, no_key_is_read).is_err());
                }
            }
            // The own host, a gateway and a name that only looks alike pass.
            let allowed = [
                ("openai", "https://api.openai.com/v1"),
                ("anthropic", "https://api.anthropic.com"),
                ("openai", "https://gateway.example.com/anthropic/v1"),
                ("anthropic", "https://api.openai.com.example.com"),
            ];
            for (provider, base) in allowed {
                let args = judge_args(&["--judge-provider", provider, "--judge-base-url", base]);
                assert!(
                    args.client.check_provider_flags().is_ok(),
                    "{provider} {base}"
                );
            }
        }

        #[test]
        fn openai_refuses_the_flags_it_cannot_honour() {
            let program = installed();
            let base = [
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ];
            let refused: [(&[&str], &str); 3] = [
                (&["--judge-fallbacks", "on"], "--judge-fallbacks"),
                (&["--judge-fallbacks", "off"], "--judge-fallbacks"),
                (&["--judge-cli-path", &program], "--judge-cli-path"),
            ];
            for (extra, name) in refused {
                let mut flags = base.to_vec();
                flags.extend_from_slice(extra);
                let args = judge_args(&flags);
                let err =
                    from_args(&args, no_key_is_read, addr(LOOPBACK), true).expect_err("refused");
                let text = format!("{err:#}");
                assert!(text.contains(name), "{text}");
                assert!(for_cli(&args.client, no_key_is_read).is_err());
            }
            let mut flags = base.to_vec();
            flags.extend(["--judge-fallbacks", "on"]);
            let err = from_args(&judge_args(&flags), no_key_is_read, addr(LOOPBACK), true)
                .expect_err("refused");
            assert_eq!(
                err.to_string(),
                "--judge-fallbacks (ASSESSMENT_JUDGE_FALLBACKS) is only for the anthropic \
                 provider and has no meaning with --judge-provider openai: remove it"
            );
        }

        #[test]
        fn an_openai_judge_without_auth_must_stay_on_loopback() {
            let flags = [
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ];
            let args = judge_args(&flags);
            let err = from_args(&args, openai_key, addr(PUBLIC), false).expect_err("open");
            assert!(
                err.to_string().contains("--judge-allow-unauthenticated"),
                "{err}"
            );
            assert!(!format!("{err:#}").contains(KEY));
            assert!(from_args(&args, openai_key, addr(PUBLIC), true).is_ok());
            assert!(from_args(&args, openai_key, addr(LOOPBACK), false).is_ok());
            let mut allowed = flags.to_vec();
            allowed.push("--judge-allow-unauthenticated");
            let args = judge_args(&allowed);
            assert!(from_args(&args, openai_key, addr(PUBLIC), false).is_ok());
        }

        #[test]
        fn a_bad_openai_base_url_key_or_model_is_an_error_without_the_key() {
            for bad in [
                "http://api.example.com/v1",
                "http://10.0.0.5:8080/v1",
                "https://user:pw@api.example.com/v1",
                "not a url",
            ] {
                let args = judge_args(&[
                    "--judge",
                    "on",
                    "--judge-provider",
                    "openai",
                    "--judge-base-url",
                    bad,
                ]);
                let err = from_args(&args, openai_key, addr(LOOPBACK), true).expect_err("refused");
                let text = format!("{err:#}");
                assert!(text.contains("base URL"), "{bad}: {text}");
                assert!(!text.contains(KEY), "{bad}: {text}");
            }
            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
                "--judge-model",
                "bad model",
            ]);
            let err = from_args(&args, openai_key, addr(LOOPBACK), true).expect_err("a bad model");
            assert!(format!("{err:#}").contains("model"), "{err:#}");
            let bad_key = "bad key\twith spaces";
            let args = judge_args(&[
                "--judge",
                "on",
                "--judge-provider",
                "openai",
                "--judge-base-url",
                OPENAI_LOCAL_BASE,
            ]);
            let err = from_args(
                &args,
                env_of(OPENAI_KEY_ENV, OPENAI_KEY_FILE_ENV, bad_key),
                addr(LOOPBACK),
                true,
            )
            .expect_err("a bad key");
            let text = format!("{err:#}");
            assert!(text.contains("API key"), "{text}");
            assert!(!text.contains(bad_key), "{text}");
        }
    }
}
