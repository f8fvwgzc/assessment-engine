//! A judge that asks a model through the OpenAI Chat Completions API.
//!
//! One question is one request with the fixed system prompt and a strict
//! JSON schema for the answer. The HTTP rules are those of every provider:
//! the client never follows redirects, talks plain HTTP only to a loopback
//! address, reads at most 1 MiB and never puts the API key into an error or
//! a log line.
//!
//! The Chat Completions API is the one that most gateways and other
//! vendors copy, so `base_url` may name such an endpoint. Only the request
//! shape of OpenAI itself is built here.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Value, json};
use tokio::time::Instant;

pub use crate::http::{ApiKey, MAX_ATTEMPTS, MAX_BACKOFF, MAX_RESPONSE_BYTES};
use crate::http::{
    Failure, PROBE_TIMEOUT, build_client, config_error, count, deadline_error, error_detail,
    is_identifier, run_with_retries, send, status_error, validate_base_url,
};
use crate::price::assumed_usage;
pub use crate::prompt::MAX_RATIONALE_CHARS;
use crate::prompt::{Answer, SYSTEM_PROMPT, schema, user_content};
use crate::{BoxFuture, Judge, JudgeError, JudgeErrorKind, Question, Usage, Verdict, valid_model};
pub use crate::{DEFAULT_OPENAI_MODEL, Effort};

/// The API root. Unlike the Anthropic one it includes `/v1`.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// The name of the answer schema in `response_format`.
pub const SCHEMA_NAME: &str = "verdict";

/// The settings of an [`OpenAiJudge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiConfig {
    /// The API root, with `/v1`. It must be https, or http on loopback.
    pub base_url: String,
    pub model: String,
    /// Sent as `reasoning_effort`.
    pub effort: Effort,
    /// Sent as `max_completion_tokens`. It bounds the reasoning and the
    /// answer together.
    pub max_tokens: u32,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_OPENAI_MODEL.to_owned(),
            effort: Effort::Low,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

/// A judge backed by the OpenAI Chat Completions API.
pub struct OpenAiJudge {
    client: reqwest::Client,
    key: ApiKey,
    key_header: HeaderValue,
    id: String,
    base: String,
    config: OpenAiConfig,
    /// Set when the API rejected `reasoning_effort`. Stays set.
    effort_rejected: AtomicBool,
    /// Set when a response without a usable `usage` was logged.
    usage_missing_logged: AtomicBool,
}

impl fmt::Debug for OpenAiJudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiJudge")
            .field("id", &self.id)
            .field("base", &self.base)
            .field("key", &self.key)
            .field("effort", &self.config.effort)
            .field("max_tokens", &self.config.max_tokens)
            .field("effort_sent", &self.effort_on())
            .finish()
    }
}

/// The request body. It never holds a sampling setting, `tools`, `n`,
/// `stream`, `user` or `logprobs`, and it asks the API not to store the
/// exchange.
pub(crate) fn build_body(config: &OpenAiConfig, user: &str, effort: bool) -> Value {
    let mut body = json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user}
        ],
        "max_completion_tokens": config.max_tokens,
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": SCHEMA_NAME, "strict": true, "schema": schema()}
        },
        "store": false
    });
    if effort {
        body["reasoning_effort"] = json!(config.effort.as_str());
    }
    body
}

/// The usage of a response, in the crate's terms. `None` when the response
/// reports none that can be used: no `usage` object, a count that is
/// missing or not a number, or no prompt tokens at all. Some compatible
/// endpoints leave the usage out or fill it with zeros.
///
/// `prompt_tokens` already includes the cached tokens. They are taken out
/// of `input_tokens` and reported as `cache_read_input_tokens`, so no token
/// is counted twice. The price module counts cache tokens above the input
/// price, so the cost is never estimated too low. `completion_tokens`
/// includes the reasoning tokens.
fn usage_of(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage?;
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64)?;
    let completion = usage.get("completion_tokens").and_then(Value::as_u64)?;
    // Every request has a prompt, so a zero is not a count.
    if prompt == 0 {
        return None;
    }
    let cached = count(
        usage.get("prompt_tokens_details").unwrap_or(&Value::Null),
        "cached_tokens",
    )
    .min(prompt);
    Some(Usage {
        input_tokens: prompt - cached,
        output_tokens: completion,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    })
}

/// A 200 response, read.
pub(crate) struct Parsed {
    pub(crate) result: Result<Verdict, JudgeError>,
    /// The response reported its usage. When it did not, the usage of the
    /// result is `assumed`.
    pub(crate) usage_reported: bool,
}

/// Reads a 200 response. A refusal and the finish reason are checked
/// before the content.
///
/// `assumed` is the usage to take when the response reports none. It must
/// be an upper estimate: the spend limit settles a call at its usage, and a
/// call that looks free would never count against the limit.
pub(crate) fn parse_response(body: &[u8], default_model: &str, assumed: Usage) -> Parsed {
    let Ok(response) = serde_json::from_slice::<Value>(body) else {
        return Parsed {
            result: Err(JudgeError::new(
                JudgeErrorKind::InvalidOutput,
                "the response is not JSON",
            )),
            usage_reported: true,
        };
    };
    let reported = usage_of(response.get("usage"));
    Parsed {
        result: read_response(&response, default_model, reported.unwrap_or(assumed)),
        usage_reported: reported.is_some(),
    }
}

fn read_response(
    response: &Value,
    default_model: &str,
    usage: Usage,
) -> Result<Verdict, JudgeError> {
    let fail = |kind, message: String| Err(JudgeError::new(kind, message).with_usage(usage));

    let Some(choice) = response.pointer("/choices/0").filter(|c| c.is_object()) else {
        return fail(
            JudgeErrorKind::InvalidOutput,
            "the response has no choice".to_owned(),
        );
    };
    let finish = choice.get("finish_reason").and_then(Value::as_str);
    // The text of a refusal is written by the model. It is not copied.
    let refused = choice
        .pointer("/message/refusal")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if refused {
        return fail(
            JudgeErrorKind::Refused,
            "the model refused to answer".to_owned(),
        );
    }
    match finish {
        Some("stop") => {}
        Some("content_filter") => {
            return fail(
                JudgeErrorKind::Refused,
                "the model refused to answer (content filter)".to_owned(),
            );
        }
        Some("length") => {
            return fail(
                JudgeErrorKind::Truncated,
                "the answer was cut off at the token limit".to_owned(),
            );
        }
        other => {
            let message = match other.filter(|r| is_identifier(r)) {
                Some(reason) => format!("unexpected finish reason: {reason}"),
                None => "the response has no usable finish reason".to_owned(),
            };
            return fail(JudgeErrorKind::InvalidOutput, message);
        }
    }

    let raw = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .and_then(Answer::parse);
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
    Ok(raw
        .into_verdict(format!("openai/{model}"))
        .with_usage(usage))
}

impl OpenAiJudge {
    /// Builds the client. Nothing is sent yet.
    pub fn new(key: ApiKey, config: OpenAiConfig) -> Result<Self, JudgeError> {
        let (base, loopback) = validate_base_url(&config.base_url)?;
        if !valid_model(&config.model) {
            return Err(config_error(
                "the judge model name may hold only letters, digits, '-', '.' and '_'",
            ));
        }
        if config.max_tokens == 0 {
            return Err(config_error("the judge max_tokens must be at least 1"));
        }
        let client = build_client(loopback)?;
        Ok(Self {
            client,
            key_header: key.bearer()?,
            key,
            id: format!("openai/{}", config.model),
            base,
            config,
            effort_rejected: AtomicBool::new(false),
            usage_missing_logged: AtomicBool::new(false),
        })
    }

    pub fn config(&self) -> &OpenAiConfig {
        &self.config
    }

    /// The `reasoning_effort` field is sent with the next request.
    pub fn effort_on(&self) -> bool {
        !self.effort_rejected.load(Ordering::Relaxed)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header(AUTHORIZATION, self.key_header.clone())
    }

    /// An error for a status other than 200. `rejects_option` is set for a
    /// 400 that names the `reasoning_effort` field.
    fn status_failure(
        &self,
        status: u16,
        body: Option<&[u8]>,
        retry_after: Option<Duration>,
    ) -> Failure {
        let detail = error_detail(body);
        let names = |text: &Option<String>| {
            text.as_deref()
                .is_some_and(|t| t.contains("reasoning_effort"))
        };
        let names_effort = status == 400 && (names(&detail.param) || names(&detail.message));
        Failure {
            error: status_error(&self.key, status, &detail),
            retry_after,
            rejects_option: names_effort,
        }
    }

    async fn attempt(&self, user: &str) -> Result<Verdict, Failure> {
        let effort = self.effort_on();
        let request_body = serde_json::to_vec(&build_body(&self.config, user, effort))
            .map_err(|_| config_error("could not build the request"))?;
        // What the call is taken to have used when the response does not say.
        let assumed = assumed_usage(request_body.len(), self.config.max_tokens);
        let request = self
            .request(reqwest::Method::POST, "/chat/completions")
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(request_body);
        let reply = send(request).await?;
        if reply.status != 200 {
            let (status, retry_after) = (reply.status, reply.retry_after);
            let body = reply.error_body().await;
            let mut failure = self.status_failure(status, body.as_deref(), retry_after);
            // Only a request that carried the field can drop it.
            failure.rejects_option &= effort;
            return Err(failure);
        }
        let body = reply.body().await?;
        let parsed = parse_response(&body, &self.config.model, assumed);
        if !parsed.usage_reported && !self.usage_missing_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                judge = %self.id,
                "the API response reports no usage; such a call is counted at an upper estimate (the request size and a full answer)"
            );
        }
        // A remote text could repeat the key, so it is taken out here too:
        // of an error, and of the model name and the rationale of an answer.
        parsed
            .result
            .map(|verdict| self.key.scrub(verdict, &self.id))
            .map_err(|mut error| {
                error.message = self.key.redact(&error.message);
                Failure::from(error)
            })
    }

    async fn run(&self, question: &Question, deadline: Instant) -> Result<Verdict, JudgeError> {
        let user = user_content(question);
        run_with_retries(
            &self.id,
            deadline,
            || self.attempt(&user),
            || {
                // The model or the endpoint does not take the field. It is
                // left out from now on.
                self.effort_rejected.store(true, Ordering::Relaxed);
                tracing::warn!(
                    judge = %self.id,
                    "the API rejected the reasoning_effort field; it is left out from now on"
                );
            },
        )
        .await
    }

    /// Asks the API for the model. It shows whether the key, the model
    /// name and TLS work. It costs nothing and sends no text.
    pub async fn probe(&self, deadline: Instant) -> Result<(), JudgeError> {
        let path = format!("/models/{}", self.config.model);
        let call = async {
            let reply = send(self.request(reqwest::Method::GET, &path)).await?;
            if reply.status == 200 {
                return Ok(());
            }
            let status = reply.status;
            let body = reply.error_body().await;
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
                Err(error) if error.http_status() == Some(401) => tracing::warn!(
                    judge = %judge.id,
                    status = error.http_status(),
                    error = error.message(),
                    "judge startup check failed: the API rejected the key; judged assessments will be unavailable until it works"
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

impl Judge for OpenAiJudge {
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
    use crate::price::{cost_usd, price_for};
    use assessment_core::model::TextRelation;

    const KEY: &str = "sk-test-0123456789abcdef";

    fn judge(config: OpenAiConfig) -> OpenAiJudge {
        OpenAiJudge::new(ApiKey::new(KEY).expect("key"), config).expect("judge")
    }

    fn response(finish_reason: &str, message: Value) -> Vec<u8> {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-6.1-sol",
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 700, "completion_tokens": 40, "total_tokens": 740}
        })
        .to_string()
        .into_bytes()
    }

    fn content(text: &str) -> Value {
        json!({"role": "assistant", "content": text, "refusal": null})
    }

    /// The usage a test takes for a response that reports none.
    const ASSUMED: Usage = Usage {
        input_tokens: 5000,
        output_tokens: 4096,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };

    fn parse(body: &[u8], default_model: &str) -> Result<Verdict, JudgeError> {
        parse_response(body, default_model, ASSUMED).result
    }

    const ANSWER: &str =
        r#"{"relation":"replaced","confidence":0.93,"rationale":"\"http\" became \"https\"."}"#;

    #[test]
    fn the_body_is_exactly_the_documented_shape() {
        let body = build_body(&OpenAiConfig::default(), "{\"x\":1}", true);
        let keys: Vec<&str> = body
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "max_completion_tokens",
                "messages",
                "model",
                "reasoning_effort",
                "response_format",
                "store"
            ]
        );
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["max_completion_tokens"], 4096);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["store"], false);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": "{\"x\":1}"}
            ])
        );
        assert_eq!(
            body["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": {"name": "verdict", "strict": true, "schema": schema()}
            })
        );
        let without = build_body(&OpenAiConfig::default(), "x", false);
        assert!(without.get("reasoning_effort").is_none());
        assert_eq!(without.as_object().map(|o| o.len()), Some(5));
    }

    /// Strict mode of the API takes a schema only when every object closes
    /// itself and requires each of its properties.
    #[test]
    fn the_schema_fits_strict_mode() {
        fn check(node: &Value) {
            if node.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(node["additionalProperties"], false, "{node}");
                let mut names: Vec<&str> = node["properties"]
                    .as_object()
                    .expect("properties")
                    .keys()
                    .map(String::as_str)
                    .collect();
                let mut required: Vec<&str> = node["required"]
                    .as_array()
                    .expect("required")
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                names.sort_unstable();
                required.sort_unstable();
                assert_eq!(names, required, "{node}");
            }
            match node {
                Value::Object(map) => map.values().for_each(check),
                Value::Array(items) => items.iter().for_each(check),
                _ => {}
            }
        }
        let schema = schema();
        assert_eq!(schema["type"], "object");
        check(&schema);
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
            let config = OpenAiConfig {
                effort,
                ..OpenAiConfig::default()
            };
            assert_eq!(build_body(&config, "x", true)["reasoning_effort"], name);
        }
    }

    #[test]
    fn a_good_answer_is_read_with_its_source_and_usage() {
        let verdict = parse(&response("stop", content(ANSWER)), "m").expect("verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);
        assert_eq!(verdict.confidence, 0.93);
        assert_eq!(verdict.rationale, "\"http\" became \"https\".");
        assert_eq!(verdict.source, "openai/gpt-6.1-sol");
        assert_eq!(verdict.usage.input_tokens, 700);
        assert_eq!(verdict.usage.output_tokens, 40);
        assert!(!verdict.cached);

        // The source names the model that answered, when its name is sane.
        let mut body: Value =
            serde_json::from_slice(&response("stop", content(ANSWER))).expect("json");
        body["model"] = json!("gpt-6.1-sol-2026-09-01");
        let verdict = parse(body.to_string().as_bytes(), "m").expect("verdict");
        assert_eq!(verdict.source, "openai/gpt-6.1-sol-2026-09-01");
        body["model"] = json!("x y\nz");
        let verdict = parse(body.to_string().as_bytes(), "gpt-6.1-sol").expect("verdict");
        assert_eq!(verdict.source, "openai/gpt-6.1-sol");
    }

    #[test]
    fn every_relation_name_is_read() {
        for name in crate::prompt::RELATIONS {
            let answer = format!(r#"{{"relation":"{name}","confidence":0.5,"rationale":""}}"#);
            let verdict = parse(&response("stop", content(&answer)), "m").expect(name);
            assert_eq!(verdict.relation.as_str(), name);
        }
    }

    #[test]
    fn a_refusal_and_the_finish_reason_are_checked_before_the_content() {
        // A refusal next to a perfectly good answer.
        let refusal = json!({"role": "assistant", "content": ANSWER, "refusal": "I cannot do it."});
        let error = parse(&response("stop", refusal), "m").expect_err("refused");
        assert_eq!(error.kind(), JudgeErrorKind::Refused);
        assert_eq!(error.message(), "the model refused to answer");
        assert_eq!(error.usage().map(|u| u.input_tokens), Some(700));

        // An empty refusal text is none.
        for empty in [json!(""), json!("  "), Value::Null, json!(7)] {
            let message = json!({"role": "assistant", "content": ANSWER, "refusal": empty});
            assert!(parse(&response("stop", message), "m").is_ok());
        }

        let error = parse(&response("content_filter", content(ANSWER)), "m").expect_err("filter");
        assert_eq!(error.kind(), JudgeErrorKind::Refused);
        assert!(error.usage().is_some());

        let error = parse(&response("length", content(ANSWER)), "m").expect_err("cut off");
        assert_eq!(error.kind(), JudgeErrorKind::Truncated);
        assert!(error.usage().is_some());

        for reason in ["tool_calls", "function_call", "something new", ""] {
            let error = parse(&response(reason, content(ANSWER)), "m").expect_err("unexpected");
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{reason}");
            assert!(error.usage().is_some());
        }
        let error = parse(&response("tool_calls", content(ANSWER)), "m").expect_err("x");
        assert_eq!(error.message(), "unexpected finish reason: tool_calls");

        for body in [
            json!({"choices": []}),
            json!({"choices": "none"}),
            json!({"choices": [7]}),
            json!({"usage": {"prompt_tokens": 1}}),
            json!({"choices": [{"message": {"content": ANSWER}}]}),
            json!({"choices": [{"message": {"content": ANSWER}, "finish_reason": null}]}),
        ] {
            let error = parse(body.to_string().as_bytes(), "m").expect_err("unusable");
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{body}");
        }
    }

    #[test]
    fn only_the_first_choice_counts() {
        let body = json!({
            "choices": [
                {"message": {"content": "no"}, "finish_reason": "stop"},
                {"message": {"content": ANSWER}, "finish_reason": "stop"}
            ]
        });
        let error = parse(body.to_string().as_bytes(), "m").expect_err("first");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
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
            r#"{"relation":"replaced","confidence":"0.9","rationale":"x"}"#,
            r#"[{"relation":"replaced","confidence":0.9,"rationale":"x"}]"#,
        ];
        for answer in bad {
            let error = parse(&response("stop", content(answer)), "m").expect_err(answer);
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{answer}");
            assert_eq!(error.usage().map(|u| u.output_tokens), Some(40), "{answer}");
        }
        // No content, or content that is not a string.
        for message in [
            json!({"role": "assistant", "content": null}),
            json!({"role": "assistant"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": ANSWER}]}),
        ] {
            let error = parse(&response("stop", message), "m").expect_err("no text");
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        }
        // A body that is not JSON has no usage to bill.
        let error = parse(b"<html>", "m").expect_err("html");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        assert_eq!(error.usage(), None);
    }

    #[test]
    fn a_long_rationale_is_cut_on_a_character_boundary() {
        let long = "Утга өөрчлөгдсөн. ".repeat(60);
        let answer = json!({"relation": "narrowed", "confidence": 0.8, "rationale": long});
        let verdict = parse(&response("stop", content(&answer.to_string())), "m").expect("verdict");
        assert_eq!(verdict.rationale.chars().count(), MAX_RATIONALE_CHARS);
        assert!(long.starts_with(&verdict.rationale));
    }

    #[test]
    fn cached_prompt_tokens_are_counted_once_and_never_too_cheap() {
        let usage = usage_of(Some(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_tokens_details": {"cached_tokens": 600},
            "completion_tokens_details": {"reasoning_tokens": 30}
        })))
        .expect("usage");
        assert_eq!(
            usage,
            Usage {
                input_tokens: 400,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 600,
            }
        );
        // Every prompt token is there exactly once.
        assert_eq!(usage.input_tokens + usage.cache_read_input_tokens, 1000);
        // The estimate is at least the full price of all prompt tokens.
        let price = price_for("openai/gpt-6.1-sol");
        let full = (1000.0 * price.input_per_mtok + 50.0 * price.output_per_mtok) / 1e6;
        let cost = cost_usd("openai/gpt-6.1-sol", &usage);
        assert!(cost >= full, "{cost} {full}");
        // 400 * 2 + 600 * 1.25 * 2 + 50 * 10, per million.
        assert!((cost - (800.0 + 1500.0 + 500.0) / 1e6).abs() < 1e-12);

        // Without cache details all prompt tokens are plain input.
        let plain = usage_of(Some(
            &json!({"prompt_tokens": 1000, "completion_tokens": 50}),
        ))
        .expect("usage");
        assert_eq!(plain.input_tokens, 1000);
        assert_eq!(plain.cache_read_input_tokens, 0);
        // A cached count above the prompt count cannot go below zero.
        let odd = usage_of(Some(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": 99}
        })))
        .expect("usage");
        assert_eq!(odd.input_tokens, 0);
        assert_eq!(odd.cache_read_input_tokens, 10);
        assert_eq!(odd.output_tokens, 0);
    }

    #[test]
    fn a_usage_that_says_nothing_is_not_read_as_zero() {
        for usage in [
            Value::Null,
            json!("x"),
            json!(7),
            json!([]),
            json!({}),
            json!({"prompt_tokens": 10}),
            json!({"completion_tokens": 10}),
            json!({"prompt_tokens": "10", "completion_tokens": 5}),
            json!({"prompt_tokens": 10, "completion_tokens": null}),
            json!({"prompt_tokens": -1, "completion_tokens": 5}),
            json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
            json!({"prompt_tokens": 0, "completion_tokens": 40}),
        ] {
            assert_eq!(usage_of(Some(&usage)), None, "{usage}");
        }
        assert_eq!(usage_of(None), None);
    }

    #[test]
    fn a_response_without_usage_is_costed_at_the_assumed_usage() {
        let without = |finish_reason: &str, message: Value| {
            json!({
                "model": "gpt-6.1-sol",
                "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}]
            })
            .to_string()
            .into_bytes()
        };
        // A good answer.
        let parsed = parse_response(&without("stop", content(ANSWER)), "m", ASSUMED);
        assert!(!parsed.usage_reported);
        let verdict = parsed.result.expect("verdict");
        assert_eq!(verdict.usage, ASSUMED);
        assert!(cost_usd("openai/gpt-6.1-sol", &verdict.usage) > 0.0);

        // A billed failure.
        let refusal = json!({"role": "assistant", "content": null, "refusal": "No."});
        for (finish, message, kind) in [
            ("stop", refusal, JudgeErrorKind::Refused),
            ("content_filter", content(ANSWER), JudgeErrorKind::Refused),
            ("length", content(ANSWER), JudgeErrorKind::Truncated),
            ("stop", content("not json"), JudgeErrorKind::InvalidOutput),
        ] {
            let parsed = parse_response(&without(finish, message), "m", ASSUMED);
            assert!(!parsed.usage_reported);
            let error = parsed.result.expect_err("fails");
            assert_eq!(error.kind(), kind);
            assert_eq!(error.usage(), Some(ASSUMED), "{finish}");
        }

        // Zeros and a null are no usage either.
        for usage in [
            Value::Null,
            json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
        ] {
            let mut body: Value =
                serde_json::from_slice(&without("stop", content(ANSWER))).expect("json");
            body["usage"] = usage;
            let parsed = parse_response(body.to_string().as_bytes(), "m", ASSUMED);
            assert!(!parsed.usage_reported);
            assert_eq!(parsed.result.expect("verdict").usage, ASSUMED);
        }

        // A reported usage is taken as it is.
        let parsed = parse_response(&response("stop", content(ANSWER)), "m", ASSUMED);
        assert!(parsed.usage_reported);
        assert_eq!(parsed.result.expect("verdict").usage.input_tokens, 700);
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let judge = judge(OpenAiConfig::default());
        let shown = format!("{judge:?} {:?} {:#?}", judge.config(), judge.key_header);
        assert!(!shown.contains(KEY), "{shown}");
        assert!(!shown.contains("0123456789"), "{shown}");
        assert!(shown.contains("openai/gpt-6.1-sol"));
    }

    #[test]
    fn the_key_is_taken_out_of_error_text() {
        let judge = judge(OpenAiConfig::default());
        let body = json!({
            "error": {
                "message": format!("Incorrect API key provided: {KEY}\nplease check"),
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_api_key"
            }
        })
        .to_string();
        let failure = judge.status_failure(401, Some(body.as_bytes()), None);
        assert_eq!(
            failure.error.message(),
            "HTTP 401 invalid_request_error: Incorrect API key provided: [redacted] please check"
        );
        assert!(!format!("{:?} {}", failure.error, failure.error).contains(KEY));
        assert_eq!(failure.error.http_status(), Some(401));
        assert!(!failure.rejects_option);
    }

    #[test]
    fn only_a_400_that_names_the_field_drops_the_effort() {
        let judge = judge(OpenAiConfig::default());
        let names = |status, message: &str, param: Value| {
            let body = json!({
                "error": {"type": "invalid_request_error", "message": message, "param": param}
            })
            .to_string();
            judge
                .status_failure(status, Some(body.as_bytes()), None)
                .rejects_option
        };
        assert!(names(
            400,
            "Unsupported parameter: 'reasoning_effort' is not supported with this model.",
            json!("reasoning_effort")
        ));
        assert!(names(400, "Unsupported value", json!("reasoning_effort")));
        assert!(names(
            400,
            "Unrecognized request argument: reasoning_effort",
            Value::Null
        ));
        assert!(!names(
            400,
            "max_completion_tokens is too large",
            json!("max_completion_tokens")
        ));
        assert!(!names(
            400,
            "Invalid schema for response_format",
            json!("response_format")
        ));
        assert!(!names(500, "reasoning_effort", json!("reasoning_effort")));
        assert!(!names(404, "reasoning_effort", json!("reasoning_effort")));
        assert!(!judge.status_failure(400, None, None).rejects_option);
    }

    #[test]
    fn a_bad_config_is_refused() {
        let key = || ApiKey::new(KEY).expect("key");
        for model in ["", "gpt 6", "../chat/completions", "gpt/6", "a?b=c", "-x"] {
            let config = OpenAiConfig {
                model: model.to_owned(),
                ..OpenAiConfig::default()
            };
            let error = OpenAiJudge::new(key(), config).expect_err("bad model");
            assert!(error.message().contains("model"), "{model}");
        }
        let bad_url = OpenAiConfig {
            base_url: "http://example.com/v1".to_owned(),
            ..OpenAiConfig::default()
        };
        assert!(OpenAiJudge::new(key(), bad_url).is_err());
        let no_tokens = OpenAiConfig {
            max_tokens: 0,
            ..OpenAiConfig::default()
        };
        assert!(OpenAiJudge::new(key(), no_tokens).is_err());
    }

    #[test]
    fn the_defaults_and_the_id_are_the_documented_ones() {
        let config = OpenAiConfig::default();
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-6.1-sol");
        assert_eq!(config.effort, Effort::Low);
        assert_eq!(config.max_tokens, 4096);
        let judge = judge(OpenAiConfig {
            model: "gpt-6-luna".to_owned(),
            ..config
        });
        assert_eq!(judge.id(), "openai/gpt-6-luna");
        assert!(judge.effort_on());
        let judge: Arc<dyn Judge> = Arc::new(judge);
        assert!(
            judge
                .peek(&Question::new("title", "issue", "a", "b"))
                .is_none()
        );
    }
}
