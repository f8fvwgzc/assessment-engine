//! What the HTTP providers share: the API key, the base URL rules, the
//! client, the capped body read, the error text and the retry loop.
//!
//! The rules are the same for every provider. A client never follows
//! redirects, talks plain HTTP only to a loopback address, reads at most
//! 1 MiB and never puts the API key into an error or a log line.

use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::HeaderValue;
use serde_json::Value;
use tokio::time::Instant;

use crate::{JudgeError, JudgeErrorKind, Verdict, random_u64, retryable_status, truncate_chars};

/// Attempts per question, the first one included.
pub const MAX_ATTEMPTS: u32 = 3;
/// The largest response body that is read.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// The longest wait between two attempts when the API names none.
pub const MAX_BACKOFF: Duration = Duration::from_secs(8);

const BASE_BACKOFF: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_ERROR_DETAIL_CHARS: usize = 200;
const REDACTED: &str = "[redacted]";
/// A key shorter than this has no masked copy worth looking for.
const MIN_MASKED_KEY_LEN: usize = 8;
/// A word that starts with this much of the key is taken as a copy of it.
const KEY_PREFIX_LEN: usize = 6;
/// A masked word that starts with this much of the key is a copy of it.
const MASKED_PREFIX_LEN: usize = 3;
/// A masked word that ends with this much of the key is a copy of it.
const MASKED_SUFFIX_LEN: usize = 4;

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

    fn sensitive(text: &str) -> Result<HeaderValue, JudgeError> {
        let mut value = HeaderValue::from_str(text)
            .map_err(|_| config_error("the API key cannot be sent as an HTTP header"))?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// The key as a header value of its own, such as `x-api-key`.
    #[cfg(feature = "anthropic")]
    pub(crate) fn header(&self) -> Result<HeaderValue, JudgeError> {
        Self::sensitive(&self.0)
    }

    /// The key as the value of an `Authorization: Bearer` header.
    #[cfg(feature = "openai")]
    pub(crate) fn bearer(&self) -> Result<HeaderValue, JudgeError> {
        Self::sensitive(&format!("Bearer {}", self.0))
    }

    /// Takes the key out of a text that came from outside.
    pub(crate) fn redact(&self, text: &str) -> String {
        if text.contains(&self.0) {
            text.replace(&self.0, REDACTED)
        } else {
            text.to_owned()
        }
    }

    /// Takes the key out of an error text from outside, and also every
    /// word that looks like a part of it. An API may repeat the key with
    /// its middle masked, such as `sk-abcde*****wxyz`: that still shows
    /// the start and the end of the key. A word is taken out when it
    /// starts with six characters of the key, or when it is masked (`*`,
    /// `...` or `…`) and starts with three or ends with the last four.
    pub(crate) fn redact_masked(&self, text: &str) -> String {
        let text = self.redact(text);
        if self.0.len() < MIN_MASKED_KEY_LEN {
            return text;
        }
        let is_delimiter = |c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '=' | ',' | ':' | '(' | ')' | '<' | '>'
                )
        };
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let end = rest.find(is_delimiter).unwrap_or(rest.len());
            let (word, tail) = rest.split_at(end);
            out.push_str(if self.is_fragment(word) {
                REDACTED
            } else {
                word
            });
            let mut chars = tail.chars();
            if let Some(delimiter) = chars.next() {
                out.push(delimiter);
            }
            rest = chars.as_str();
        }
        out
    }

    /// Whether `word` looks like a part or a masked copy of the key.
    fn is_fragment(&self, word: &str) -> bool {
        let key = self.0.as_bytes();
        let prefix = word.bytes().zip(key).take_while(|(a, b)| a == *b).count();
        if prefix >= KEY_PREFIX_LEN {
            return true;
        }
        let masked = word.contains('*') || word.contains("...") || word.contains('…');
        if !masked {
            return false;
        }
        // The key is ASCII, so the cut is on a character boundary.
        let end = &self.0[self.0.len() - MASKED_SUFFIX_LEN..];
        let trimmed = word.trim_end_matches(['.', ';', '!', '?']);
        prefix >= MASKED_PREFIX_LEN || trimmed.ends_with(end)
    }

    /// Takes the key out of an answer. An endpoint could repeat the key in
    /// the model name or in the text of the answer, and both go on to the
    /// caller. `id` is the judge id, used when the source holds the key.
    pub(crate) fn scrub(&self, mut verdict: Verdict, id: &str) -> Verdict {
        if verdict.source.contains(&self.0) {
            id.clone_into(&mut verdict.source);
        }
        if verdict.rationale.contains(&self.0) {
            verdict.rationale = self.redact(&verdict.rationale);
        }
        verdict
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

/// A setting is wrong. Nothing was sent.
pub(crate) fn config_error(message: &str) -> JudgeError {
    JudgeError::new(JudgeErrorKind::Transport, message)
}

pub(crate) fn deadline_error() -> JudgeError {
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

/// The client of a provider. It follows no redirect. A loopback address is
/// never reached through a proxy, and any other address only over https.
pub(crate) fn build_client(loopback: bool) -> Result<reqwest::Client, JudgeError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("assessment-judge/", env!("CARGO_PKG_VERSION")));
    builder = if loopback {
        builder.no_proxy()
    } else {
        builder.https_only(true)
    };
    builder
        .build()
        .map_err(|_| config_error("could not set up the HTTP client"))
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

pub(crate) fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

pub(crate) fn count(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// What an API error body says, when it has the usual shape
/// `{"error": {"type": …, "message": …, "param": …}}`.
#[derive(Debug, Default)]
pub(crate) struct ErrorDetail {
    pub(crate) kind: Option<String>,
    pub(crate) message: Option<String>,
    /// The request field the error is about. Only some APIs name it.
    #[cfg_attr(not(feature = "openai"), allow(dead_code))]
    pub(crate) param: Option<String>,
}

pub(crate) fn error_detail(body: Option<&[u8]>) -> ErrorDetail {
    let Some(Ok(value)) = body.map(serde_json::from_slice::<Value>) else {
        return ErrorDetail::default();
    };
    let text = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    ErrorDetail {
        kind: text("/error/type"),
        message: text("/error/message"),
        param: text("/error/param"),
    }
}

/// The error for a status other than 200. The text from the remote side is
/// cut, put on one line and cleared of the key and of masked copies of it.
pub(crate) fn status_error(key: &ApiKey, status: u16, detail: &ErrorDetail) -> JudgeError {
    let mut message = format!("HTTP {status}");
    if (300..400).contains(&status) {
        message.push_str(" (redirects are not followed)");
    }
    if let Some(kind) = detail.kind.as_deref().filter(|k| is_identifier(k)) {
        message.push(' ');
        message.push_str(kind);
    }
    if let Some(detail) = detail.message.as_deref() {
        let detail = clean(&key.redact_masked(detail), MAX_ERROR_DETAIL_CHARS);
        if !detail.is_empty() {
            message.push_str(": ");
            message.push_str(&detail);
        }
    }
    // The error type also comes from the remote side.
    let message = key.redact(&message);
    JudgeError::new(JudgeErrorKind::Api, message).with_http_status(status)
}

/// One failed attempt.
pub(crate) struct Failure {
    pub(crate) error: JudgeError,
    /// The wait the API asked for.
    pub(crate) retry_after: Option<Duration>,
    /// A 400 that names an optional request field which this attempt sent.
    /// The provider then leaves the field out and asks once more.
    pub(crate) rejects_option: bool,
}

impl From<JudgeError> for Failure {
    fn from(error: JudgeError) -> Self {
        Self {
            error,
            retry_after: None,
            rejects_option: false,
        }
    }
}

pub(crate) fn transport_error(error: &reqwest::Error) -> JudgeError {
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

/// A response whose body is not read yet.
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) retry_after: Option<Duration>,
    response: reqwest::Response,
}

impl Reply {
    /// The body of an error response. It only adds detail, so a body that
    /// cannot be read or is too large is left out.
    pub(crate) async fn error_body(self) -> Option<Vec<u8>> {
        read_capped(self.response, MAX_RESPONSE_BYTES)
            .await
            .ok()
            .flatten()
    }

    /// The body of a 200 response, at most 1 MiB.
    pub(crate) async fn body(self) -> Result<Vec<u8>, JudgeError> {
        read_capped(self.response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| transport_error(&e))?
            .ok_or_else(|| {
                JudgeError::new(
                    JudgeErrorKind::InvalidOutput,
                    "the response is larger than 1 MiB",
                )
            })
    }
}

/// Sends one request and reads the status and the retry headers.
pub(crate) async fn send(request: reqwest::RequestBuilder) -> Result<Reply, JudgeError> {
    let response = request.send().await.map_err(|e| transport_error(&e))?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    Ok(Reply {
        status: response.status().as_u16(),
        retry_after: parse_retry_after(header("retry-after-ms"), header("retry-after")),
        response,
    })
}

/// Runs `attempt` until it answers, at most [`MAX_ATTEMPTS`] times and
/// never past `deadline`.
///
/// Connection errors and the statuses 408, 409, 429 and 5xx are tried
/// again, after the wait the API named or a backoff. A wait that does not
/// fit before the deadline is not started. When an attempt reports
/// [`Failure::rejects_option`], `on_rejected_option` runs and one more
/// attempt is made at once, a single time per call.
pub(crate) async fn run_with_retries<A, F>(
    id: &str,
    deadline: Instant,
    attempt: A,
    on_rejected_option: impl Fn(),
) -> Result<Verdict, JudgeError>
where
    A: Fn() -> F,
    F: Future<Output = Result<Verdict, Failure>>,
{
    let mut failed = 0;
    let mut dropped_option = false;
    loop {
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let failure = match tokio::time::timeout_at(deadline, attempt()).await {
            Err(_) => return Err(deadline_error()),
            Ok(Ok(verdict)) => return Ok(verdict),
            Ok(Err(failure)) => failure,
        };
        if failure.rejects_option && !dropped_option {
            dropped_option = true;
            on_rejected_option();
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
            judge = %id,
            status = error.http_status(),
            attempt = failed,
            wait_ms = wait.as_millis() as u64,
            "the judge call failed; trying again"
        );
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-test-0123456789abcdef";

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
    fn the_key_is_trimmed_hidden_and_redacted() {
        let key = ApiKey::new(&format!("  {KEY}\n")).expect("key");
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
        assert_eq!(
            key.redact(&format!("a {KEY} b {KEY}")),
            "a [redacted] b [redacted]"
        );
        assert_eq!(key.redact("nothing here"), "nothing here");
        #[cfg(feature = "anthropic")]
        {
            let header = key.header().expect("header");
            assert!(header.is_sensitive());
            assert_eq!(header.to_str().ok(), Some(KEY));
            assert!(!format!("{header:?}").contains(KEY));
        }
        #[cfg(feature = "openai")]
        {
            let header = key.bearer().expect("header");
            assert!(header.is_sensitive());
            assert_eq!(header.to_str().ok(), Some(format!("Bearer {KEY}").as_str()));
            assert!(!format!("{header:?}").contains(KEY));
        }
    }

    #[test]
    fn the_status_error_is_short_clean_and_without_the_key() {
        let key = ApiKey::new(KEY).expect("key");
        let body = serde_json::json!({
            "error": {
                "type": "authentication_error",
                "message": format!("bad key: {KEY}\nplease check"),
                "param": "model"
            }
        })
        .to_string();
        let detail = error_detail(Some(body.as_bytes()));
        assert_eq!(detail.param.as_deref(), Some("model"));
        let error = status_error(&key, 401, &detail);
        assert_eq!(
            error.message(),
            "HTTP 401 authentication_error: bad key: [redacted] please check"
        );
        assert_eq!(error.http_status(), Some(401));

        // A type that echoes the key is redacted too.
        let body = serde_json::json!({"error": {"type": KEY, "message": "x"}}).to_string();
        let error = status_error(&key, 403, &error_detail(Some(body.as_bytes())));
        assert!(!error.message().contains(KEY));

        let error = status_error(&key, 502, &error_detail(Some(b"<html>")));
        assert_eq!(error.message(), "HTTP 502");
        let error = status_error(&key, 307, &error_detail(None));
        assert_eq!(error.message(), "HTTP 307 (redirects are not followed)");
        assert!(!error.is_retryable());

        let long =
            serde_json::json!({"error": {"type": "bad type!", "message": "a\r\nb".repeat(500)}});
        let error = status_error(&key, 500, &error_detail(Some(long.to_string().as_bytes())));
        assert!(error.message().starts_with("HTTP 500: a"));
        assert!(error.message().chars().count() <= "HTTP 500: ".len() + MAX_ERROR_DETAIL_CHARS);
        assert!(!error.message().chars().any(char::is_control));
    }

    #[test]
    fn a_masked_copy_of_the_key_is_taken_out_of_an_error() {
        let key = ApiKey::new("sk-dummy-0123456789abcdef").expect("key");
        let status = |message: &str| {
            let body = serde_json::json!({
                "error": {"type": "invalid_request_error", "message": message}
            })
            .to_string();
            status_error(&key, 401, &error_detail(Some(body.as_bytes())))
                .message()
                .to_owned()
        };
        assert_eq!(
            status(
                "Incorrect API key provided: sk-dummy*************cdef. You can find your API \
                 key at the dashboard."
            ),
            "HTTP 401 invalid_request_error: Incorrect API key provided: [redacted] You can \
             find your API key at the dashboard."
        );
        for masked in [
            "sk-dummy*************cdef",
            "sk-du***cdef",
            "sk-***",
            "****cdef",
            "*****************cdef.",
            "sk-dummy-0123",
            "sk-dummy...cdef",
            "sk-d…cdef",
            "sk-dummy-0123456789abcde",
            "\"sk-dummy****cdef\"",
            "key=sk-dummy****cdef,",
            "(sk-dummy****cdef)",
        ] {
            let message = status(&format!("bad key {masked} given"));
            for part in ["sk-d", "dummy", "cdef", "0123"] {
                assert!(!message.contains(part), "{masked}: {message}");
            }
            assert!(message.contains("[redacted]"), "{masked}: {message}");
            assert!(message.starts_with("HTTP 401 invalid_request_error: bad key "));
            assert!(message.ends_with(" given"), "{masked}: {message}");
        }
        // Ordinary words stay.
        for plain in [
            "invalid x-api-key",
            "the model `gpt-6.1-sol` does not exist",
            "Rate limit reached... try again in 2 s",
            "sk- is the usual start of a key",
            "wait *** then retry",
        ] {
            assert_eq!(key.redact_masked(plain), plain);
        }
        // A short key has no masked form to look for.
        let short = ApiKey::new("abc").expect("key");
        assert_eq!(
            short.redact_masked("abc*** and a**"),
            "[redacted]*** and a**"
        );
    }

    #[test]
    fn the_key_is_taken_out_of_an_answer() {
        const KEY: &str = "sk-dummy-0123456789abcdef";
        let key = ApiKey::new(KEY).expect("key");
        let verdict = |source: &str, rationale: &str| {
            Verdict::new(
                assessment_core::model::TextRelation::Replaced,
                0.9,
                rationale,
                source,
            )
        };
        let clean = verdict("openai/gpt-6.1-sol", "The host changed.");
        assert_eq!(key.scrub(clean.clone(), "openai/m"), clean);
        let scrubbed = key.scrub(
            verdict(&format!("openai/{KEY}"), &format!("key is {KEY}.")),
            "openai/m",
        );
        assert_eq!(scrubbed.source, "openai/m");
        assert_eq!(scrubbed.rationale, "key is [redacted].");
        assert_eq!(scrubbed.confidence, 0.9);
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
            ok("https://api.openai.com/v1"),
            ("https://api.openai.com/v1".to_owned(), false)
        );
        assert_eq!(
            ok("https://api.openai.com/v1/"),
            ("https://api.openai.com/v1".to_owned(), false)
        );
        assert_eq!(
            ok("https://gateway.example/anthropic/"),
            ("https://gateway.example/anthropic".to_owned(), false)
        );
        assert_eq!(
            ok("http://127.0.0.1:8080"),
            ("http://127.0.0.1:8080".to_owned(), true)
        );
        assert_eq!(
            ok("http://127.0.0.1:8080/v1"),
            ("http://127.0.0.1:8080/v1".to_owned(), true)
        );
        assert!(ok("http://localhost:8080").1);
        assert!(ok("http://[::1]:8080").1);
        assert!(ok("https://localhost").1);
        assert!(ok("http://127.8.9.10").1);

        let secret = "hunter2secret";
        let bad = [
            "http://api.anthropic.com".to_owned(),
            "http://api.openai.com/v1".to_owned(),
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
}
