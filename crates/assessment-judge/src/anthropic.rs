//! A judge that asks a Claude model through the Anthropic Messages API.
//!
//! One question is one request with a fixed system prompt and a JSON schema
//! for the answer. The client never follows redirects, talks plain HTTP
//! only to a loopback address, reads at most 1 MiB and never puts the API
//! key into an error or a log line.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use assessment_core::model::TextRelation;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::prompt::{SYSTEM_PROMPT, schema, user_content};
use crate::{
    BoxFuture, Judge, JudgeError, JudgeErrorKind, Question, Usage, Verdict, random_u64,
    retryable_status, truncate_chars,
};

/// The API version this client speaks.
pub const API_VERSION: &str = "2026-10-21";
/// The beta that goes with the `fallbacks` field.
pub const FALLBACKS_BETA: &str = "server-side-fallback-2026-07-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// Attempts per question, the first one included.
pub const MAX_ATTEMPTS: u32 = 3;
/// The largest response body that is read.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// The longest rationale that is kept, in characters.
pub const MAX_RATIONALE_CHARS: usize = 400;
/// The longest wait between two attempts when the API names none.
pub const MAX_BACKOFF: Duration = Duration::from_secs(8);

const BASE_BACKOFF: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_DETAIL_CHARS: usize = 200;
const REDACTED: &str = "[redacted]";

/// An API key. It is never printed: `Debug` shows a placeholder.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Takes a key, without the white space around it. The key must be
    /// visible ASCII, as an HTTP header needs.
    pub fn new(raw: &str) -> Result<Self, JudgeError> {
        let key = raw.trim();
        if key.is_empty() {
            return Err(config_error("the API key is empty"));
        }
        if !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(config_error(
                "the API key has characters that an HTTP header cannot carry",
            ));
        }
        Ok(Self(key.to_owned()))
    }

    fn header(&self) -> Result<HeaderValue, JudgeError> {
        let mut value = HeaderValue::from_str(&self.0)
            .map_err(|_| config_error("the API key cannot be sent as an HTTP header"))?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// Takes the key out of a text that came from outside.
    fn redact(&self, text: &str) -> String {
        if text.contains(&self.0) {
            text.replace(&self.0, REDACTED)
        } else {
            text.to_owned()
        }
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

/// How much work the model puts into an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    #[default]
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::Xhigh),
            "max" => Ok(Effort::Max),
            _ => Err("expected low, medium, high, xhigh or max".to_owned()),
        }
    }
}

/// Whether the API may answer with another model when the first one
/// declines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallbacks {
    /// On for `claude-opus-5-5`, off for every other model.
    #[default]
    Auto,
    On,
    Off,
}

impl FromStr for Fallbacks {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Fallbacks::Auto),
            "on" => Ok(Fallbacks::On),
            "off" => Ok(Fallbacks::Off),
            _ => Err("expected auto, on or off".to_owned()),
        }
    }
}

/// The settings of an [`AnthropicJudge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicConfig {
    /// The API root, without `/v1`. It must be https, or http on loopback.
    pub base_url: String,
    pub model: String,
    pub effort: Effort,
    pub max_tokens: u32,
    pub fallbacks: Fallbacks,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            effort: Effort::Low,
            max_tokens: DEFAULT_MAX_TOKENS,
            fallbacks: Fallbacks::Auto,
        }
    }
}

/// A judge backed by the Anthropic Messages API.
pub struct AnthropicJudge {
    client: reqwest::Client,
    key: ApiKey,
    key_header: HeaderValue,
    id: String,
    base: String,
    config: AnthropicConfig,
    use_fallbacks: bool,
    /// Set when the API rejected the fallbacks field. Stays set.
    fallbacks_rejected: AtomicBool,
}

impl fmt::Debug for AnthropicJudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicJudge")
            .field("id", &self.id)
            .field("base", &self.base)
            .field("key", &self.key)
            .field("effort", &self.config.effort)
            .field("max_tokens", &self.config.max_tokens)
            .field("fallbacks", &self.fallbacks_on())
            .finish()
    }
}

/// A setting is wrong. Nothing was sent.
fn config_error(message: &str) -> JudgeError {
    JudgeError::new(JudgeErrorKind::Transport, message)
}

fn deadline_error() -> JudgeError {
    JudgeError::new(JudgeErrorKind::Deadline, "judge timed out")
}

/// Checks a base URL. Returns it without a trailing slash, and whether the
/// host is a loopback address. Errors never repeat the URL: it may hold a
/// password.
pub(crate) fn validate_base_url(raw: &str) -> Result<(String, bool), JudgeError> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|_| config_error("the judge base URL is not a valid URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(config_error(
            "the judge base URL must not carry a user name or a password",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(config_error(
            "the judge base URL must not carry a query or a fragment",
        ));
    }
    let Some(host) = url.host_str() else {
        return Err(config_error("the judge base URL has no host"));
    };
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => {
            return Err(config_error(
                "the judge base URL must use https (http is allowed for loopback only)",
            ));
        }
    }
    Ok((url.as_str().trim_end_matches('/').to_owned(), loopback))
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

/// The request body. It never holds `thinking`, a sampling setting,
/// `tool_choice`, `stream` or an assistant turn.
pub(crate) fn build_body(config: &AnthropicConfig, user: &str, fallbacks: bool) -> Value {
    let mut body = json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "system": [{
            "type": "text",
            "text": SYSTEM_PROMPT,
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": user}],
        "output_config": {
            "effort": config.effort.as_str(),
            "format": {"type": "json_schema", "schema": schema()}
        }
    });
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    body
}

/// The wait the API asked for. `retry-after-ms` wins over `retry-after`.
/// Only plain numbers count; a date is ignored.
pub(crate) fn parse_retry_after(millis: Option<&str>, seconds: Option<&str>) -> Option<Duration> {
    let number = |text: &str| {
        text.trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    if let Some(ms) = millis.and_then(number) {
        return Some(Duration::from_secs_f64((ms / 1000.0).min(86_400.0)));
    }
    seconds
        .and_then(number)
        .map(|s| Duration::from_secs_f64(s.min(86_400.0)))
}

/// The wait before the next attempt, after `failed` failed attempts:
/// 0.5 s doubled each time, at most 8 s, shortened by up to a quarter.
pub(crate) fn backoff(failed: u32, random: u64) -> Duration {
    let doubled = BASE_BACKOFF.saturating_mul(2u32.saturating_pow(failed.saturating_sub(1)));
    let base = doubled.min(MAX_BACKOFF);
    let jitter = (random % 1024) as f64 / 1024.0;
    base.mul_f64(1.0 - 0.25 * jitter)
}

/// Makes a text from outside safe for one log line.
fn clean(text: &str, max_chars: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate_chars(flat.trim(), max_chars)
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

fn count(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// The usage of a response. With `iterations` it is the sum over them:
/// the top-level numbers cover only the last attempt.
fn usage_of(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let one = |value: &Value| Usage {
        input_tokens: count(value, "input_tokens"),
        output_tokens: count(value, "output_tokens"),
        cache_creation_input_tokens: count(value, "cache_creation_input_tokens"),
        cache_read_input_tokens: count(value, "cache_read_input_tokens"),
    };
    match usage.get("iterations").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => {
            items.iter().map(one).fold(Usage::default(), Usage::plus)
        }
        _ => one(usage),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVerdict {
    relation: TextRelation,
    confidence: f64,
    rationale: String,
}

/// Reads one answer text. `None` when it is not exactly a verdict.
fn read_verdict(text: &str) -> Option<RawVerdict> {
    let raw: RawVerdict = serde_json::from_str(text.trim()).ok()?;
    (raw.confidence.is_finite() && (0.0..=1.0).contains(&raw.confidence)).then_some(raw)
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn block_text(block: &Value) -> Option<&str> {
    (block_type(block) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// Reads a 200 response. The stop reason is checked before the content.
pub(crate) fn parse_response(body: &[u8], default_model: &str) -> Result<Verdict, JudgeError> {
    let response: Value = serde_json::from_slice(body)
        .map_err(|_| JudgeError::new(JudgeErrorKind::InvalidOutput, "the response is not JSON"))?;
    let usage = usage_of(response.get("usage"));
    let fail = |kind, message: String| Err(JudgeError::new(kind, message).with_usage(usage));

    match response.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") => {}
        Some("refusal") => {
            let category = response
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .filter(|c| is_identifier(c));
            let message = match category {
                Some(category) => format!("the model refused to answer (category: {category})"),
                None => "the model refused to answer".to_owned(),
            };
            return fail(JudgeErrorKind::Refused, message);
        }
        Some("max_tokens") => {
            return fail(
                JudgeErrorKind::Truncated,
                "the answer was cut off at the token limit".to_owned(),
            );
        }
        other => {
            let message = match other.filter(|r| is_identifier(r)) {
                Some(reason) => format!("unexpected stop reason: {reason}"),
                None => "the response has no usable stop reason".to_owned(),
            };
            return fail(JudgeErrorKind::InvalidOutput, message);
        }
    }

    let empty = Vec::new();
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // After a fallback, only the blocks after the last marker are the answer.
    let start = content
        .iter()
        .rposition(|block| block_type(block) == Some("fallback"))
        .map_or(0, |at| at + 1);
    let answer: String = content[start..].iter().filter_map(block_text).collect();
    let raw = read_verdict(&answer).or_else(|| {
        content[start..]
            .iter()
            .rev()
            .filter_map(block_text)
            .find_map(read_verdict)
    });
    let Some(raw) = raw else {
        return fail(
            JudgeErrorKind::InvalidOutput,
            "the answer is not a valid verdict".to_owned(),
        );
    };

    let model = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| valid_model(m))
        .unwrap_or(default_model);
    Ok(Verdict::new(
        raw.relation,
        raw.confidence,
        truncate_chars(raw.rationale.trim(), MAX_RATIONALE_CHARS),
        format!("anthropic/{model}"),
    )
    .with_usage(usage))
}

/// The type and the message of an API error body, when it has them.
fn error_detail(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let text = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    (text("/error/type"), text("/error/message"))
}

/// One failed attempt.
struct Failure {
    error: JudgeError,
    /// The wait the API asked for.
    retry_after: Option<Duration>,
    /// A 400 that names the fallbacks field or its beta header.
    names_fallbacks: bool,
}

impl From<JudgeError> for Failure {
    fn from(error: JudgeError) -> Self {
        Self {
            error,
            retry_after: None,
            names_fallbacks: false,
        }
    }
}

fn transport_error(error: &reqwest::Error) -> JudgeError {
    // The text is fixed: a library message could repeat a URL or a header.
    let message = if error.is_connect() {
        "could not connect to the API"
    } else if error.is_timeout() {
        "the connection to the API timed out"
    } else if error.is_body() || error.is_decode() {
        "could not read the API response"
    } else {
        "could not send the request to the API"
    };
    JudgeError::new(JudgeErrorKind::Transport, message)
}

/// Reads a body up to `limit` bytes. `Ok(None)` when it is larger.
async fn read_capped(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

impl AnthropicJudge {
    /// Builds the client. Nothing is sent yet.
    pub fn new(key: ApiKey, config: AnthropicConfig) -> Result<Self, JudgeError> {
        let (base, loopback) = validate_base_url(&config.base_url)?;
        if !valid_model(&config.model) {
            return Err(config_error(
                "the judge model name may hold only letters, digits, '-', '.' and '_'",
            ));
        }
        if config.max_tokens == 0 {
            return Err(config_error("the judge max_tokens must be at least 1"));
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("assessment-judge/", env!("CARGO_PKG_VERSION")));
        builder = if loopback {
            // A local address is never reached through a proxy.
            builder.no_proxy()
        } else {
            builder.https_only(true)
        };
        let client = builder
            .build()
            .map_err(|_| config_error("could not set up the HTTP client"))?;
        let use_fallbacks = match config.fallbacks {
            Fallbacks::Auto => config.model == DEFAULT_MODEL,
            Fallbacks::On => true,
            Fallbacks::Off => false,
        };
        Ok(Self {
            client,
            key_header: key.header()?,
            key,
            id: format!("anthropic/{}", config.model),
            base,
            config,
            use_fallbacks,
            fallbacks_rejected: AtomicBool::new(false),
        })
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// The `fallbacks` field is sent with the next request.
    pub fn fallbacks_on(&self) -> bool {
        self.use_fallbacks && !self.fallbacks_rejected.load(Ordering::Relaxed)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("x-api-key", self.key_header.clone())
            .header("anthropic-version", API_VERSION)
    }

    /// An error for a status other than 200.
    fn status_failure(
        &self,
        status: u16,
        body: Option<&[u8]>,
        retry_after: Option<Duration>,
    ) -> Failure {
        let (kind, detail) = body.map(error_detail).unwrap_or_default();
        let names_fallbacks = status == 400
            && detail
                .as_deref()
                .is_some_and(|d| d.contains("fallbacks") || d.contains("anthropic-beta"));
        let mut message = format!("HTTP {status}");
        if (300..400).contains(&status) {
            message.push_str(" (redirects are not followed)");
        }
        if let Some(kind) = kind.filter(|k| is_identifier(k)) {
            message.push(' ');
            message.push_str(&kind);
        }
        if let Some(detail) = detail {
            let detail = clean(&self.key.redact(&detail), MAX_ERROR_DETAIL_CHARS);
            if !detail.is_empty() {
                message.push_str(": ");
                message.push_str(&detail);
            }
        }
        // The error type also comes from the remote side.
        let message = self.key.redact(&message);
        Failure {
            error: JudgeError::new(JudgeErrorKind::Api, message).with_http_status(status),
            retry_after,
            names_fallbacks,
        }
    }

    async fn attempt(&self, user: &str, fallbacks: bool) -> Result<Verdict, Failure> {
        let mut request = self.request(reqwest::Method::POST, "/v1/messages");
        if fallbacks {
            request = request.header("anthropic-beta", FALLBACKS_BETA);
        }
        let response = request
            .json(&build_body(&self.config, user, fallbacks))
            .send()
            .await
            .map_err(|e| transport_error(&e))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        let retry_after = parse_retry_after(header("retry-after-ms"), header("retry-after"));

        if status != 200 {
            // The body only adds detail, so a body that cannot be read is
            // left out.
            let body = read_capped(response, MAX_RESPONSE_BYTES)
                .await
                .ok()
                .flatten();
            return Err(self.status_failure(status, body.as_deref(), retry_after));
        }
        let body = read_capped(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| transport_error(&e))?;
        let Some(body) = body else {
            return Err(JudgeError::new(
                JudgeErrorKind::InvalidOutput,
                "the response is larger than 1 MiB",
            )
            .into());
        };
        // A remote text could repeat the key, so it is taken out here too.
        parse_response(&body, &self.config.model).map_err(|mut error| {
            error.message = self.key.redact(&error.message);
            Failure::from(error)
        })
    }

    async fn run(&self, question: &Question, deadline: Instant) -> Result<Verdict, JudgeError> {
        let user = user_content(question);
        let mut failed = 0;
        let mut dropped_fallbacks = false;
        loop {
            if Instant::now() >= deadline {
                return Err(deadline_error());
            }
            let fallbacks = self.fallbacks_on();
            let failure =
                match tokio::time::timeout_at(deadline, self.attempt(&user, fallbacks)).await {
                    Err(_) => return Err(deadline_error()),
                    Ok(Ok(verdict)) => return Ok(verdict),
                    Ok(Err(failure)) => failure,
                };
            if fallbacks && failure.names_fallbacks && !dropped_fallbacks {
                // The API does not take the field here. Ask once more
                // without it, and leave it out from now on.
                dropped_fallbacks = true;
                self.fallbacks_rejected.store(true, Ordering::Relaxed);
                tracing::warn!(
                    judge = %self.id,
                    "the API rejected the fallbacks field; it is off from now on"
                );
                continue;
            }
            failed += 1;
            let error = failure.error;
            let retryable = match error.kind() {
                JudgeErrorKind::Transport => true,
                JudgeErrorKind::Api => error.http_status().is_some_and(retryable_status),
                _ => false,
            };
            if !retryable || failed >= MAX_ATTEMPTS {
                return Err(error);
            }
            let wait = failure
                .retry_after
                .unwrap_or_else(|| backoff(failed, random_u64()));
            // A wait that does not fit before the deadline is not started.
            if Instant::now() + wait >= deadline {
                return Err(error);
            }
            tracing::debug!(
                judge = %self.id,
                status = error.http_status(),
                attempt = failed,
                wait_ms = wait.as_millis() as u64,
                "the judge call failed; trying again"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// Asks the API for the model. It shows whether the key, the model
    /// name and TLS work. It costs nothing.
    pub async fn probe(&self, deadline: Instant) -> Result<(), JudgeError> {
        let path = format!("/v1/models/{}", self.config.model);
        let call = async {
            let response = self
                .request(reqwest::Method::GET, &path)
                .send()
                .await
                .map_err(|e| transport_error(&e))?;
            let status = response.status().as_u16();
            if status == 200 {
                return Ok(());
            }
            let body = read_capped(response, MAX_RESPONSE_BYTES)
                .await
                .ok()
                .flatten();
            Err(self.status_failure(status, body.as_deref(), None).error)
        };
        match tokio::time::timeout_at(deadline, call).await {
            Ok(result) => result,
            Err(_) => Err(deadline_error()),
        }
    }

    /// Runs [`Self::probe`] in the background and logs the result. Startup
    /// does not wait for it, and a failure does not stop the server.
    pub fn spawn_startup_probe(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let judge = Arc::clone(self);
        tokio::spawn(async move {
            match judge.probe(Instant::now() + PROBE_TIMEOUT).await {
                Ok(()) => tracing::info!(
                    judge = %judge.id,
                    "judge startup check passed: the key, the model and TLS work"
                ),
                Err(error) => tracing::warn!(
                    judge = %judge.id,
                    status = error.http_status(),
                    error = error.message(),
                    "judge startup check failed; judged assessments will be unavailable until it works"
                ),
            }
        })
    }
}

impl Judge for AnthropicJudge {
    fn id(&self) -> &str {
        &self.id
    }

    fn judge<'a>(
        &'a self,
        question: &'a Question,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(self.run(question, deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-test-0123456789abcdef";

    fn config(model: &str, fallbacks: Fallbacks) -> AnthropicConfig {
        AnthropicConfig {
            model: model.to_owned(),
            fallbacks,
            ..AnthropicConfig::default()
        }
    }

    fn judge(config: AnthropicConfig) -> AnthropicJudge {
        AnthropicJudge::new(ApiKey::new(KEY).expect("key"), config).expect("judge")
    }

    fn response(stop_reason: &str, content: Value) -> Vec<u8> {
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "stop_reason": stop_reason,
            "content": content,
            "usage": {"input_tokens": 700, "output_tokens": 40}
        })
        .to_string()
        .into_bytes()
    }

    fn text(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    const ANSWER: &str =
        r#"{"relation":"replaced","confidence":0.93,"rationale":"\"http\" became \"https\"."}"#;

    #[test]
    fn the_body_is_exactly_the_documented_shape() {
        let body = build_body(&AnthropicConfig::default(), "{\"x\":1}", true);
        let keys: Vec<&str> = body
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "fallbacks",
                "max_tokens",
                "messages",
                "model",
                "output_config",
                "system"
            ]
        );
        for absent in [
            "thinking",
            "temperature",
            "top_p",
            "top_k",
            "tool_choice",
            "tools",
            "stream",
        ] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": SYSTEM_PROMPT,
                "cache_control": {"type": "ephemeral"}
            }])
        );
        // One user turn and no assistant prefill.
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "{\"x\":1}"}])
        );
        assert_eq!(
            body["output_config"],
            json!({"effort": "low", "format": {"type": "json_schema", "schema": schema()}})
        );
    }

    #[test]
    fn the_effort_is_set_from_the_config() {
        for (effort, name) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            let config = AnthropicConfig {
                effort,
                ..AnthropicConfig::default()
            };
            assert_eq!(
                build_body(&config, "x", false)["output_config"]["effort"],
                name
            );
            assert_eq!(name.parse::<Effort>(), Ok(effort));
        }
        assert!("none".parse::<Effort>().is_err());
        assert_eq!("auto".parse::<Fallbacks>(), Ok(Fallbacks::Auto));
        assert_eq!("on".parse::<Fallbacks>(), Ok(Fallbacks::On));
        assert_eq!("off".parse::<Fallbacks>(), Ok(Fallbacks::Off));
        assert!("yes".parse::<Fallbacks>().is_err());
    }

    #[test]
    fn fallbacks_are_sent_only_for_opus_unless_forced() {
        assert!(judge(config("claude-opus-5-5", Fallbacks::Auto)).fallbacks_on());
        assert!(!judge(config("claude-sonnet-5-5", Fallbacks::Auto)).fallbacks_on());
        assert!(!judge(config("claude-haiku-5-5", Fallbacks::Auto)).fallbacks_on());
        assert!(!judge(config("claude-opus-5-5", Fallbacks::Off)).fallbacks_on());
        assert!(judge(config("claude-sonnet-5-5", Fallbacks::On)).fallbacks_on());
        let body = build_body(&config("claude-sonnet-5-5", Fallbacks::Auto), "x", false);
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn a_good_answer_is_read_with_its_source_and_usage() {
        let verdict =
            parse_response(&response("end_turn", json!([text(ANSWER)])), "m").expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);
        assert_eq!(verdict.confidence, 0.93);
        assert_eq!(verdict.rationale, "\"http\" became \"https\".");
        assert_eq!(verdict.source, "anthropic/claude-opus-5-5");
        assert_eq!(verdict.usage.input_tokens, 700);
        assert_eq!(verdict.usage.output_tokens, 40);
        assert!(!verdict.cached);
    }

    #[test]
    fn every_relation_name_is_read() {
        for name in crate::prompt::RELATIONS {
            let answer = format!(r#"{{"relation":"{name}","confidence":0.5,"rationale":""}}"#);
            let verdict =
                parse_response(&response("end_turn", json!([text(&answer)])), "m").expect(name);
            assert_eq!(verdict.relation.as_str(), name);
        }
    }

    #[test]
    fn the_stop_reason_is_checked_before_the_content() {
        // A refusal with a perfectly good answer in the content.
        let mut refusal: Value =
            serde_json::from_slice(&response("refusal", json!([text(ANSWER)]))).expect("json");
        refusal["stop_details"] = json!({"type": "refusal", "category": "cyber"});
        let error = parse_response(refusal.to_string().as_bytes(), "m").expect_err("refused");
        assert_eq!(error.kind(), JudgeErrorKind::Refused);
        assert!(error.message().contains("cyber"));
        assert_eq!(error.usage().map(|u| u.input_tokens), Some(700));

        let error = parse_response(&response("refusal", json!([])), "m").expect_err("no category");
        assert_eq!(error.kind(), JudgeErrorKind::Refused);

        let error = parse_response(&response("max_tokens", json!([text(ANSWER)])), "m")
            .expect_err("cut off");
        assert_eq!(error.kind(), JudgeErrorKind::Truncated);
        assert!(error.usage().is_some());

        for reason in ["tool_use", "pause_turn", "stop_sequence", "something new"] {
            let error = parse_response(&response(reason, json!([text(ANSWER)])), "m")
                .expect_err("unexpected");
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{reason}");
            assert!(error.usage().is_some());
        }
        let error = parse_response(br#"{"content":[]}"#, "m").expect_err("no stop reason");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
    }

    #[test]
    fn a_verdict_from_before_a_fallback_is_never_used() {
        // The first model wrote a verdict, the fallback model did not.
        let content = json!([
            text(ANSWER),
            {"type": "fallback"},
            text("I can't help with that.")
        ]);
        let error = parse_response(&response("end_turn", content), "m").expect_err("no verdict");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);

        // After the marker the last block that is a verdict still counts.
        let content = json!([text("x"), {"type": "fallback"}, text(ANSWER), text("")]);
        let verdict = parse_response(&response("end_turn", content), "m").expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);
    }

    #[test]
    fn after_a_fallback_the_last_answer_counts() {
        let partial = r#"{"relation":"equivalent","confidence":0.9,"rationale":"first"}"#;
        let content = json!([
            text(partial),
            {"type": "fallback", "model": "claude-fable-5-1"},
            {"type": "thinking", "thinking": "", "signature": "x"},
            text(ANSWER)
        ]);
        let verdict = parse_response(&response("end_turn", content), "m").expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);

        // A partial text before the marker does not spoil the answer.
        let content = json!([
            text(r#"{"relation":"equi"#),
            {"type": "fallback"},
            text(ANSWER)
        ]);
        let verdict = parse_response(&response("end_turn", content), "m").expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);

        // An answer split over two text blocks is joined.
        let (head, tail) = ANSWER.split_at(20);
        let verdict = parse_response(&response("end_turn", json!([text(head), text(tail)])), "m")
            .expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);

        // When the joined text is not a verdict, the last block that is one
        // counts.
        let content = json!([text(ANSWER), text("")]);
        assert!(parse_response(&response("end_turn", content), "m").is_ok());
        let content = json!([text("Here is the answer."), text(ANSWER)]);
        assert!(parse_response(&response("end_turn", content), "m").is_ok());
    }

    #[test]
    fn answers_that_are_not_a_strict_verdict_are_invalid() {
        let bad = [
            "not json",
            "",
            r#"{"relation":"replaced","confidence":0.9}"#,
            r#"{"relation":"replaced","confidence":0.9,"rationale":"x","extra":1}"#,
            r#"{"relation":"renamed","confidence":0.9,"rationale":"x"}"#,
            r#"{"relation":"Replaced","confidence":0.9,"rationale":"x"}"#,
            r#"{"relation":"replaced","confidence":1.2,"rationale":"x"}"#,
            r#"{"relation":"replaced","confidence":-0.1,"rationale":"x"}"#,
            r#"{"relation":"replaced","confidence":NaN,"rationale":"x"}"#,
            r#"{"relation":"replaced","confidence":"0.9","rationale":"x"}"#,
            r#"{"relation":"replaced","confidence":0.9,"rationale":7}"#,
            r#"[{"relation":"replaced","confidence":0.9,"rationale":"x"}]"#,
        ];
        for answer in bad {
            let error = parse_response(&response("end_turn", json!([text(answer)])), "m")
                .expect_err(answer);
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{answer}");
            assert_eq!(error.usage().map(|u| u.output_tokens), Some(40), "{answer}");
        }
        // No text block at all.
        let content = json!([{"type": "thinking", "thinking": ""}]);
        let error = parse_response(&response("end_turn", content), "m").expect_err("no text");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        // A body that is not JSON has no usage to bill.
        let error = parse_response(b"<html>", "m").expect_err("html");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        assert_eq!(error.usage(), None);
    }

    #[test]
    fn a_long_rationale_is_cut_on_a_character_boundary() {
        let long = "Утга өөрчлөгдсөн. ".repeat(60);
        assert!(long.chars().count() > 1000);
        let answer = json!({"relation": "narrowed", "confidence": 0.8, "rationale": long});
        let verdict = parse_response(
            &response("end_turn", json!([text(&answer.to_string())])),
            "m",
        )
        .expect("verdict");
        assert_eq!(verdict.rationale.chars().count(), MAX_RATIONALE_CHARS);
        assert!(long.starts_with(&verdict.rationale));
    }

    #[test]
    fn usage_is_summed_over_the_iterations() {
        let mut body: Value =
            serde_json::from_slice(&response("end_turn", json!([text(ANSWER)]))).expect("json");
        body["model"] = json!("claude-fable-5-1");
        body["usage"] = json!({
            "input_tokens": 1,
            "output_tokens": 1,
            "iterations": [
                {"input_tokens": 700, "output_tokens": 5, "cache_read_input_tokens": 10},
                {"input_tokens": 710, "output_tokens": 40, "cache_creation_input_tokens": 20}
            ]
        });
        let verdict = parse_response(body.to_string().as_bytes(), "m").expect("verdict");
        assert_eq!(
            verdict.usage,
            Usage {
                input_tokens: 1410,
                output_tokens: 45,
                cache_creation_input_tokens: 20,
                cache_read_input_tokens: 10,
            }
        );
        // The source names the model that answered.
        assert_eq!(verdict.source, "anthropic/claude-fable-5-1");
    }

    #[test]
    fn a_strange_model_name_is_not_copied_into_the_source() {
        let mut body: Value =
            serde_json::from_slice(&response("end_turn", json!([text(ANSWER)]))).expect("json");
        body["model"] = json!("x y\nz");
        let verdict =
            parse_response(body.to_string().as_bytes(), "claude-opus-5-5").expect("verdict");
        assert_eq!(verdict.source, "anthropic/claude-opus-5-5");
    }

    #[test]
    fn retry_after_is_read_from_either_header() {
        let ms = Duration::from_millis;
        assert_eq!(parse_retry_after(Some("250"), None), Some(ms(250)));
        assert_eq!(parse_retry_after(Some("250"), Some("9")), Some(ms(250)));
        assert_eq!(parse_retry_after(None, Some("2")), Some(ms(2000)));
        assert_eq!(parse_retry_after(None, Some(" 1.5 ")), Some(ms(1500)));
        assert_eq!(parse_retry_after(None, Some("0")), Some(ms(0)));
        assert_eq!(parse_retry_after(Some("soon"), Some("3")), Some(ms(3000)));
        assert_eq!(parse_retry_after(None, None), None);
        assert_eq!(parse_retry_after(None, Some("-1")), None);
        assert_eq!(parse_retry_after(None, Some("NaN")), None);
        assert_eq!(parse_retry_after(None, Some("inf")), None);
        assert_eq!(
            parse_retry_after(None, Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        // A huge number does not overflow.
        assert!(parse_retry_after(Some("1e300"), None).is_some());
    }

    #[test]
    fn the_backoff_doubles_and_never_exceeds_8_seconds() {
        let ms = Duration::from_millis;
        assert_eq!(backoff(1, 0), ms(500));
        assert_eq!(backoff(2, 0), ms(1000));
        assert_eq!(backoff(3, 0), ms(2000));
        assert_eq!(backoff(5, 0), ms(8000));
        for failed in 0..80 {
            for random in [0, 1, 511, 1023, u64::MAX] {
                let wait = backoff(failed, random);
                assert!(wait <= MAX_BACKOFF, "{failed} {random}");
                assert!(wait >= ms(375), "{failed} {random}");
            }
        }
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let key = ApiKey::new(&format!("  {KEY}\n")).expect("key");
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        let judge = judge(AnthropicConfig::default());
        let shown = format!("{judge:?} {:?} {:#?}", judge.config(), judge.key_header);
        assert!(!shown.contains(KEY), "{shown}");
        assert!(!shown.contains("0123456789"), "{shown}");
        assert!(shown.contains("anthropic/claude-opus-5-5"));
    }

    #[test]
    fn a_bad_key_is_refused_without_repeating_it() {
        for bad in [
            "",
            "   ",
            "sk-ant\nsecret-part",
            "sk ant secret-part",
            "sk-ключ",
        ] {
            let error = ApiKey::new(bad).expect_err("bad key");
            if !bad.trim().is_empty() {
                assert!(!error.message().contains("secret-part"));
                assert!(!error.message().contains(bad));
            }
        }
    }

    #[test]
    fn the_key_is_taken_out_of_error_text() {
        let judge = judge(AnthropicConfig::default());
        let body = json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": format!("invalid x-api-key: {KEY}\nplease check")
            }
        })
        .to_string();
        let failure = judge.status_failure(401, Some(body.as_bytes()), None);
        let message = failure.error.message();
        assert_eq!(
            message,
            "HTTP 401 authentication_error: invalid x-api-key: [redacted] please check"
        );
        assert!(!format!("{:?} {}", failure.error, failure.error).contains(KEY));
        assert_eq!(failure.error.http_status(), Some(401));
    }

    #[test]
    fn error_text_is_short_and_on_one_line() {
        let judge = judge(AnthropicConfig::default());
        let body = json!({"error": {"type": "bad type!", "message": "a\r\nb".repeat(500)}});
        let failure = judge.status_failure(500, Some(body.to_string().as_bytes()), None);
        let message = failure.error.message();
        assert!(message.starts_with("HTTP 500: a"));
        assert!(message.chars().count() <= "HTTP 500: ".len() + MAX_ERROR_DETAIL_CHARS);
        assert!(!message.chars().any(char::is_control));

        let failure = judge.status_failure(502, Some(b"<html>bad gateway</html>"), None);
        assert_eq!(failure.error.message(), "HTTP 502");
        let failure = judge.status_failure(307, None, None);
        assert_eq!(
            failure.error.message(),
            "HTTP 307 (redirects are not followed)"
        );
        assert!(!failure.error.is_retryable());
    }

    #[test]
    fn only_a_400_that_names_the_field_drops_the_fallbacks() {
        let judge = judge(AnthropicConfig::default());
        let body = |message: &str| {
            json!({"error": {"type": "invalid_request_error", "message": message}}).to_string()
        };
        let names = |status, message: &str| {
            judge
                .status_failure(status, Some(body(message).as_bytes()), None)
                .names_fallbacks
        };
        assert!(names(400, "fallbacks: Extra inputs are not permitted"));
        assert!(names(
            400,
            "Unexpected value(s) for the `anthropic-beta` header"
        ));
        assert!(!names(400, "max_tokens: must be positive"));
        assert!(!names(500, "fallbacks"));
        assert!(!judge.status_failure(400, None, None).names_fallbacks);
    }

    #[test]
    fn the_base_url_rules_hold() {
        let ok = |url: &str| validate_base_url(url).expect(url);
        assert_eq!(
            ok("https://api.anthropic.com"),
            ("https://api.anthropic.com".to_owned(), false)
        );
        assert_eq!(
            ok("https://api.anthropic.com/"),
            ("https://api.anthropic.com".to_owned(), false)
        );
        assert_eq!(
            ok("https://gateway.example/anthropic/"),
            ("https://gateway.example/anthropic".to_owned(), false)
        );
        assert_eq!(
            ok("http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080".to_owned(), true)
        );
        assert!(ok("http://localhost:8080").1);
        assert!(ok("http://[::1]:8080").1);
        assert!(ok("https://localhost").1);
        assert!(ok("http://127.8.9.10").1);

        let secret = "hunter2secret";
        let bad = [
            "http://api.anthropic.com".to_owned(),
            "http://10.0.0.5:8080".to_owned(),
            "http://localhost.example.com".to_owned(),
            "http://0.0.0.0:8080".to_owned(),
            "ftp://127.0.0.1".to_owned(),
            format!("https://user:{secret}@api.anthropic.com"),
            format!("https://{secret}@api.anthropic.com"),
            format!("https://api.anthropic.com/?key={secret}"),
            format!("https://api.anthropic.com/#{secret}"),
            format!("{secret} not a url"),
            "api.anthropic.com".to_owned(),
            String::new(),
        ];
        for url in &bad {
            let error = validate_base_url(url).expect_err(url);
            assert!(!error.message().contains(secret), "{error}");
        }
    }

    #[test]
    fn a_bad_config_is_refused() {
        let key = || ApiKey::new(KEY).expect("key");
        for model in ["", "claude opus", "../v1/messages", "claude/opus", "a?b=c"] {
            let error =
                AnthropicJudge::new(key(), config(model, Fallbacks::Auto)).expect_err("bad model");
            assert!(error.message().contains("model"), "{model}");
        }
        let bad_url = AnthropicConfig {
            base_url: "http://example.com".to_owned(),
            ..AnthropicConfig::default()
        };
        assert!(AnthropicJudge::new(key(), bad_url).is_err());
        let no_tokens = AnthropicConfig {
            max_tokens: 0,
            ..AnthropicConfig::default()
        };
        assert!(AnthropicJudge::new(key(), no_tokens).is_err());
    }

    #[test]
    fn the_id_names_the_configured_model() {
        let judge = judge(config("claude-haiku-5-5", Fallbacks::Auto));
        assert_eq!(judge.id(), "anthropic/claude-haiku-5-5");
        let judge: Arc<dyn Judge> = Arc::new(judge);
        assert!(
            judge
                .peek(&Question::new("title", "issue", "a", "b"))
                .is_none()
        );
    }
}
