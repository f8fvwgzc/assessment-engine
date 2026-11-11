//! The OpenAI judge against a local, hand-rolled HTTP/1.1 server.
//!
//! Nothing here leaves the machine: every server listens on 127.0.0.1.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assessment_core::model::TextRelation;
use assessment_judge::openai::{DEFAULT_BASE_URL, MAX_RESPONSE_BYTES};
use assessment_judge::price::{assumed_usage, cost_usd, reserve_usd};
use assessment_judge::prompt::{SYSTEM_PROMPT, schema, user_content};
use assessment_judge::{
    ApiKey, DEFAULT_OPENAI_MODEL, Effort, Hints, Judge, JudgeError, JudgeErrorKind, LimitedJudge,
    Limits, OpenAiConfig, OpenAiJudge, Question, Usage, ask,
};
use serde_json::{Value, json};
use tokio::time::Instant;

/// A made-up key. It is not a real credential.
const KEY: &str = "sk-test-LOCAL-ONLY-0123456789abcdef";
const MODEL: &str = "gpt-6.1-sol";

/// What the server does with one connection.
#[derive(Debug, Clone)]
enum Action {
    Respond {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        /// Without it the body ends when the connection closes.
        content_length: bool,
    },
    /// Read the request and never answer.
    Hang,
    /// Read the request and close the connection without an answer.
    Reset,
}

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON request body")
    }
}

struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    /// Serves one scripted action per connection, in order. Further
    /// connections get a 599.
    fn start(script: Vec<Action>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        std::thread::spawn(move || {
            let mut script: VecDeque<Action> = script.into();
            // Hanging connections are kept open until the test ends.
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                recorded.lock().expect("lock").push(request);
                let action = script
                    .pop_front()
                    .unwrap_or_else(|| respond(599, "{}".to_owned()));
                match action {
                    Action::Hang => held.push(stream),
                    Action::Reset => drop(stream),
                    Action::Respond {
                        status,
                        headers,
                        body,
                        content_length,
                    } => {
                        let mut head = format!("HTTP/1.1 {status} X\r\nconnection: close\r\n");
                        if content_length {
                            head.push_str(&format!("content-length: {}\r\n", body.len()));
                        }
                        for (name, value) in headers {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                        head.push_str("\r\n");
                        // The client may stop reading early; that is fine.
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                        let _ = stream.flush();
                        let _ = stream.shutdown(Shutdown::Write);
                    }
                }
            }
        });
        Self { addr, requests }
    }

    /// The base URL. Like the real one it ends in `/v1`.
    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("lock").clone()
    }

    fn count(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let n = stream.read(&mut buffer).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buffer).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buffer[..n]);
    }
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

fn respond(status: u16, body: String) -> Action {
    Action::Respond {
        status,
        headers: Vec::new(),
        body: body.into_bytes(),
        content_length: true,
    }
}

fn respond_with(status: u16, headers: &[(&'static str, &str)], body: String) -> Action {
    Action::Respond {
        status,
        headers: headers.iter().map(|(n, v)| (*n, (*v).to_owned())).collect(),
        body: body.into_bytes(),
        content_length: true,
    }
}

fn api_error(kind: &str, message: &str) -> String {
    json!({"error": {"message": message, "type": kind, "param": null, "code": null}}).to_string()
}

const ANSWER: &str =
    r#"{"relation":"replaced","confidence":0.93,"rationale":"\"http\" became \"https\"."}"#;

fn completion_with(finish_reason: &str, message: Value) -> String {
    json!({
        "id": "chatcmpl-local",
        "object": "chat.completion",
        "created": 1,
        "model": MODEL,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": 812, "completion_tokens": 37, "total_tokens": 849}
    })
    .to_string()
}

fn completion(finish_reason: &str, text: &str) -> String {
    completion_with(
        finish_reason,
        json!({"role": "assistant", "content": text, "refusal": null}),
    )
}

fn ok() -> Action {
    respond(200, completion("stop", ANSWER))
}

fn judge_for(url: &str, model: &str) -> OpenAiJudge {
    let config = OpenAiConfig {
        base_url: url.to_owned(),
        model: model.to_owned(),
        ..OpenAiConfig::default()
    };
    OpenAiJudge::new(ApiKey::new(KEY).expect("key"), config).expect("judge")
}

fn judge(server: &Server) -> OpenAiJudge {
    judge_for(&server.url(), MODEL)
}

fn question() -> Question {
    Question::new(
        "description",
        "issue",
        "Call the API over http",
        "Call the API over https",
    )
    .with_context_title("API client")
    .with_hints(Hints {
        list_items_modified: 1,
        ..Hints::default()
    })
}

fn in_secs(secs: f64) -> Instant {
    Instant::now() + Duration::from_secs_f64(secs)
}

/// The key must never reach an error, in any form.
fn assert_no_key(error: &JudgeError) {
    let shown = format!("{error} {error:?} {}", error.message());
    assert!(!shown.contains(KEY), "{shown}");
    assert!(!shown.contains("LOCAL-ONLY"), "{shown}");
}

#[test]
fn the_defaults_are_the_documented_ones() {
    assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com/v1");
    assert_eq!(DEFAULT_OPENAI_MODEL, "gpt-6.1-sol");
    assert_eq!(MODEL, DEFAULT_OPENAI_MODEL);
}

#[tokio::test]
async fn the_documented_headers_and_body_are_sent() {
    let server = Server::start(vec![ok()]);
    let judge = judge(&server);
    assert_eq!(judge.id(), "openai/gpt-6.1-sol");
    let question = question();
    let verdict = judge
        .judge(&question, in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(verdict.relation, TextRelation::Replaced);
    assert_eq!(verdict.confidence, 0.93);
    assert_eq!(verdict.rationale, "\"http\" became \"https\".");
    assert_eq!(verdict.source, "openai/gpt-6.1-sol");
    assert_eq!(verdict.usage.input_tokens, 812);
    assert_eq!(verdict.usage.output_tokens, 37);
    assert!(!verdict.cached);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    // Nothing of the other provider, and no organization or project.
    for name in [
        "x-api-key",
        "anthropic-version",
        "anthropic-beta",
        "openai-organization",
        "openai-project",
        "cookie",
    ] {
        assert!(request.header(name).is_none(), "{name}");
    }
    assert!(
        request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("assessment-judge/"))
    );

    let expected = json!({
        "model": "gpt-6.1-sol",
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_content(&question)}
        ],
        "max_completion_tokens": 4096,
        "reasoning_effort": "low",
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "verdict", "strict": true, "schema": schema()}
        },
        "store": false
    });
    let body = request.json();
    assert_eq!(body, expected);
    for forbidden in [
        "temperature",
        "top_p",
        "tools",
        "tool_choice",
        "n",
        "stream",
        "user",
        "logprobs",
        "top_logprobs",
        "max_tokens",
        "seed",
        "metadata",
    ] {
        assert!(body.get(forbidden).is_none(), "{forbidden}");
    }
    assert_eq!(body["store"], false);
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);

    // The user content is the question and nothing else.
    let user: Value =
        serde_json::from_str(body["messages"][1]["content"].as_str().expect("text")).expect("json");
    assert_eq!(
        user,
        json!({
            "field": "description",
            "entity_kind": "issue",
            "context_title": "API client",
            "before": "Call the API over http",
            "after": "Call the API over https",
            "hints": {
                "list_items_added": 0,
                "list_items_removed": 0,
                "list_items_modified": 1,
                "negation_changes": 0,
                "requirement_word_changes": 0,
                "number_changes": 0
            }
        })
    );
}

#[tokio::test]
async fn the_model_the_effort_and_the_token_limit_come_from_the_config() {
    let server = Server::start(vec![ok()]);
    let config = OpenAiConfig {
        base_url: format!("{}/", server.url()),
        model: "gpt-6-luna".to_owned(),
        effort: Effort::Xhigh,
        max_tokens: 900,
    };
    let judge = OpenAiJudge::new(ApiKey::new(KEY).expect("key"), config).expect("judge");
    assert_eq!(judge.id(), "openai/gpt-6-luna");
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    let request = &server.requests()[0];
    // A trailing slash on the base URL does not double up.
    assert_eq!(request.path, "/v1/chat/completions");
    let body = request.json();
    assert_eq!(body["model"], "gpt-6-luna");
    assert_eq!(body["reasoning_effort"], "xhigh");
    assert_eq!(body["max_completion_tokens"], 900);
}

#[tokio::test]
async fn a_refusal_makes_exactly_one_request() {
    let refusal = completion_with(
        "stop",
        json!({"role": "assistant", "content": null, "refusal": "I cannot help with that."}),
    );
    let filtered = completion_with(
        "content_filter",
        json!({"role": "assistant", "content": ""}),
    );
    let server = Server::start(vec![respond(200, refusal), respond(200, filtered), ok()]);
    let judge = judge(&server);
    for n in 1..=2 {
        let error = judge
            .judge(&question(), in_secs(10.0))
            .await
            .expect_err("refused");
        assert_eq!(error.kind(), JudgeErrorKind::Refused);
        // The text of the refusal is written by the model. It is not copied.
        assert!(!error.message().contains("cannot help"));
        assert!(!error.is_retryable());
        assert_eq!(error.usage().map(|u| u.input_tokens), Some(812));
        assert_eq!(server.count(), n);
    }
}

#[tokio::test]
async fn a_cut_off_or_broken_answer_makes_one_request() {
    let server = Server::start(vec![
        respond(200, completion("length", "{\"relation\":\"repl")),
        respond(200, completion("stop", "I think it changed.")),
        respond(200, completion("tool_calls", ANSWER)),
        respond(200, json!({"choices": []}).to_string()),
        respond(200, "<html>not json</html>".to_owned()),
    ]);
    let judge = judge(&server);
    let kinds = [
        JudgeErrorKind::Truncated,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::InvalidOutput,
    ];
    for (n, kind) in kinds.into_iter().enumerate() {
        let error = judge
            .judge(&question(), in_secs(10.0))
            .await
            .expect_err("fails");
        assert_eq!(error.kind(), kind);
        assert_eq!(server.count(), n + 1);
    }
}

#[tokio::test]
async fn cached_prompt_tokens_are_billed_once_and_not_too_cheap() {
    let mut body: Value = serde_json::from_str(&completion("stop", ANSWER)).expect("json");
    body["usage"] = json!({
        "prompt_tokens": 1000,
        "completion_tokens": 50,
        "total_tokens": 1050,
        "prompt_tokens_details": {"cached_tokens": 600, "audio_tokens": 0},
        "completion_tokens_details": {"reasoning_tokens": 30}
    });
    let server = Server::start(vec![respond(200, body.to_string())]);
    let verdict = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(
        verdict.usage,
        Usage {
            input_tokens: 400,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 600,
        }
    );
    // gpt-6.1-sol is 2 and 10 USD per million tokens. The full price of
    // the request is (1000 * 2 + 50 * 10) / 1e6. The estimate is above it,
    // and below what counting the cached tokens twice would give.
    let cost = cost_usd(&verdict.source, &verdict.usage);
    let full = (1000.0 * 2.0 + 50.0 * 10.0) / 1e6;
    let doubled = (1600.0 * 2.0 + 50.0 * 10.0) / 1e6;
    assert!(cost >= full, "{cost}");
    assert!(cost < doubled, "{cost}");
    assert!((cost - 0.0028).abs() < 1e-12, "{cost}");
}

#[tokio::test]
async fn a_429_with_retry_after_then_200_succeeds() {
    let server = Server::start(vec![
        respond_with(
            429,
            &[("retry-after-ms", "150"), ("retry-after", "30")],
            api_error("rate_limit_exceeded", "slow down"),
        ),
        ok(),
    ]);
    let start = Instant::now();
    let verdict = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(verdict.relation, TextRelation::Replaced);
    assert_eq!(server.count(), 2);
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(start.elapsed() < Duration::from_secs(5));
    // The second request is the same as the first.
    let requests = server.requests();
    assert_eq!(requests[0].json(), requests[1].json());
}

#[tokio::test]
async fn a_retry_after_beyond_the_deadline_fails_at_once() {
    let server = Server::start(vec![
        respond_with(
            429,
            &[("retry-after", "30")],
            api_error("rate_limit_exceeded", "slow down"),
        ),
        ok(),
    ]);
    let start = Instant::now();
    let error = judge(&server)
        .judge(&question(), in_secs(5.0))
        .await
        .expect_err("no time to wait");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(429));
    assert_eq!(error.message(), "HTTP 429 rate_limit_exceeded: slow down");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn server_errors_are_tried_three_times() {
    let server = Server::start(vec![
        respond(500, api_error("server_error", "internal")),
        respond(503, api_error("server_error", "overloaded")),
        respond(500, api_error("server_error", "internal again")),
        ok(),
    ]);
    let error = judge(&server)
        .judge(&question(), in_secs(20.0))
        .await
        .expect_err("fails");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(500));
    assert_eq!(error.message(), "HTTP 500 server_error: internal again");
    assert!(error.is_retryable());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn retries_stop_at_the_deadline() {
    let server = Server::start(vec![
        respond(503, api_error("server_error", "down")),
        respond(503, api_error("server_error", "down")),
        ok(),
    ]);
    let start = Instant::now();
    // The first wait (about 0.5 s) fits; the second (about 1 s) does not.
    let error = judge(&server)
        .judge(&question(), in_secs(1.0))
        .await
        .expect_err("fails");
    assert_eq!(error.http_status(), Some(503));
    assert!(start.elapsed() < Duration::from_millis(1200));
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    for status in [400, 401, 403, 404, 413] {
        let server = Server::start(vec![
            respond_with(
                status,
                &[("retry-after", "0")],
                api_error("invalid_request_error", "no"),
            ),
            ok(),
        ]);
        let judge = judge(&server);
        let error = judge
            .judge(&question(), in_secs(10.0))
            .await
            .expect_err("fails");
        assert_eq!(error.kind(), JudgeErrorKind::Api);
        assert_eq!(error.http_status(), Some(status));
        assert_eq!(
            error.message(),
            format!("HTTP {status} invalid_request_error: no")
        );
        assert_eq!(server.count(), 1, "{status}");
        // None of them is about the effort field.
        assert!(judge.effort_on(), "{status}");
    }
}

/// Some compatible endpoints leave the usage out, set it to null or fill
/// it with zeros. Such a call must not look free: the spend limit settles
/// a call at its usage.
#[tokio::test]
async fn a_response_without_usage_is_not_free() {
    let without_usage = |finish_reason: &str, text: &str, usage: Option<Value>| {
        let mut body: Value = serde_json::from_str(&completion(finish_reason, text)).expect("json");
        match usage {
            Some(usage) => body["usage"] = usage,
            None => {
                body.as_object_mut().expect("object").remove("usage");
            }
        }
        respond(200, body.to_string())
    };
    let zeros = json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});
    let server = Server::start(vec![
        without_usage("stop", ANSWER, None),
        without_usage("stop", ANSWER, Some(Value::Null)),
        without_usage("stop", ANSWER, Some(zeros)),
        without_usage("length", ANSWER, None),
        without_usage("stop", "not a verdict", None),
    ]);
    let inner = Arc::new(judge(&server));
    let limits = Limits {
        model: MODEL.to_owned(),
        usd_per_hour: 1000.0,
        ..Limits::default()
    };
    let limited = LimitedJudge::new(inner as Arc<dyn Judge>, limits);
    let mut spent = 0.0;
    for call in 0..5 {
        let result = limited.judge(&question(), in_secs(10.0)).await;
        let usage = match &result {
            Ok(verdict) => verdict.usage,
            Err(error) => error.usage().expect("a billed failure has a usage"),
        };
        assert_eq!(result.is_ok(), call < 3, "{call}");

        // The usage is the upper estimate for the request that was sent.
        let request_bytes = server.requests()[call].body.len();
        assert_eq!(usage, assumed_usage(request_bytes, 4096), "{call}");
        assert!(usage.input_tokens > 0 && usage.output_tokens == 4096);
        let cost = cost_usd(MODEL, &usage);
        assert!(cost >= reserve_usd(MODEL, request_bytes, 4096) - 1e-12);
        // A full answer alone is 4096 tokens at 10 USD per million.
        assert!(cost > 4096.0 * 10.0 / 1e6, "{cost}");

        // The limit counts it.
        spent += cost;
        assert!(
            (limited.spent_last_hour() - spent).abs() < 1e-9,
            "{call}: {} {spent}",
            limited.spent_last_hour()
        );
    }
    assert_eq!(server.count(), 5);
}

/// An endpoint could repeat the key in a 200 response: as the model name
/// and inside the answer. Both go on to the caller, so the key is taken
/// out of them.
#[tokio::test]
async fn a_key_echoed_in_an_answer_never_reaches_the_verdict() {
    let answer = json!({
        "relation": "replaced",
        "confidence": 0.9,
        "rationale": format!("key is {KEY} and again {KEY}.")
    })
    .to_string();
    let mut body: Value = serde_json::from_str(&completion("stop", &answer)).expect("json");
    body["model"] = json!(KEY);
    let server = Server::start(vec![respond(200, body.to_string())]);
    let verdict = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    let shown = format!("{verdict:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(!shown.contains("LOCAL-ONLY"), "{shown}");
    // The source falls back to the judge id.
    assert_eq!(verdict.source, format!("openai/{MODEL}"));
    assert_eq!(verdict.rationale, "key is [redacted] and again [redacted].");
    assert_eq!(verdict.relation, TextRelation::Replaced);
}

/// The API answers a wrong key with a masked copy of it, which still shows
/// its start and its end. Neither may reach an error or the probe.
#[tokio::test]
async fn a_masked_copy_of_the_key_is_taken_out_of_the_error() {
    let start = &KEY[..8];
    let end = &KEY[KEY.len() - 4..];
    let masked = format!(
        "Incorrect API key provided: {start}{}{end}. You can find your API key in the dashboard.",
        "*".repeat(KEY.len() - 12)
    );
    let server = Server::start(vec![
        respond(401, api_error("invalid_request_error", &masked)),
        respond(401, api_error("invalid_request_error", &masked)),
    ]);
    let judge = judge(&server);
    let call = judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("rejected");
    let probe = judge.probe(in_secs(10.0)).await.expect_err("rejected");
    for error in [call, probe] {
        assert_eq!(error.http_status(), Some(401));
        assert_eq!(
            error.message(),
            "HTTP 401 invalid_request_error: Incorrect API key provided: [redacted] You can \
             find your API key in the dashboard."
        );
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(start), "{shown}");
        assert!(!shown.contains(end), "{shown}");
        assert!(!shown.contains('*'), "{shown}");
        assert_no_key(&error);
    }
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn a_rejected_key_is_reported_without_it() {
    let echo = format!("Incorrect API key provided: {KEY}. Find yours in the dashboard.");
    let server = Server::start(vec![
        respond(401, api_error("invalid_request_error", &echo)),
        ok(),
    ]);
    let error = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("rejected");
    assert_eq!(error.http_status(), Some(401));
    assert!(!error.is_retryable());
    assert!(error.message().contains("[redacted]"));
    assert_no_key(&error);
    assert_eq!(server.count(), 1);
}

fn effort_rejected() -> Action {
    let body = json!({
        "error": {
            "message": "Unsupported parameter: 'reasoning_effort' is not supported with this model.",
            "type": "invalid_request_error",
            "param": "reasoning_effort",
            "code": "unsupported_parameter"
        }
    });
    respond(400, body.to_string())
}

#[tokio::test]
async fn a_400_that_names_the_effort_drops_it_for_good() {
    let server = Server::start(vec![effort_rejected(), ok(), ok()]);
    let judge = judge(&server);
    assert!(judge.effort_on());
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert!(!judge.effort_on());
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].json()["reasoning_effort"], "low");
    let mut first = requests[0].json();
    first
        .as_object_mut()
        .expect("object")
        .remove("reasoning_effort");
    for request in &requests[1..] {
        // Only that one field is gone.
        assert!(request.json().get("reasoning_effort").is_none());
        assert_eq!(request.json(), first);
    }
}

#[tokio::test]
async fn a_second_400_about_the_effort_is_not_retried_again() {
    let server = Server::start(vec![effort_rejected(), effort_rejected(), ok()]);
    let error = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("fails");
    assert_eq!(error.http_status(), Some(400));
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn no_other_400_changes_the_request() {
    let other = || {
        let body = json!({
            "error": {
                "message": "Invalid schema for response_format 'verdict'.",
                "type": "invalid_request_error",
                "param": "response_format",
                "code": null
            }
        });
        respond(400, body.to_string())
    };
    let server = Server::start(vec![other(), ok()]);
    let judge = judge(&server);
    let error = judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("fails");
    assert_eq!(error.http_status(), Some(400));
    assert!(judge.effort_on());
    assert_eq!(server.count(), 1);
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(server.requests()[1].json()["reasoning_effort"], "low");
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let target = Server::start(vec![ok()]);
    let server = Server::start(vec![
        respond_with(
            307,
            &[("location", &format!("{}/chat/completions", target.url()))],
            String::new(),
        ),
        ok(),
    ]);
    let error = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("redirect");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(307));
    assert!(!error.is_retryable());
    assert_eq!(server.count(), 1);
    // The key was not carried anywhere else.
    assert_eq!(target.count(), 0);
}

#[tokio::test]
async fn a_hanging_server_gives_deadline_in_time() {
    let server = Server::start(vec![Action::Hang, ok()]);
    let start = Instant::now();
    let error = judge(&server)
        .judge(&question(), in_secs(0.5))
        .await
        .expect_err("hangs");
    assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    assert!(start.elapsed() >= Duration::from_millis(500));
    assert!(start.elapsed() < Duration::from_millis(700));
    assert_eq!(server.count(), 1);

    // The same through `ask`.
    let server = Server::start(vec![Action::Hang]);
    let judge = judge(&server);
    let start = Instant::now();
    let error = ask(&judge, &question(), in_secs(0.3))
        .await
        .expect_err("hangs");
    assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn a_dropped_connection_then_200_succeeds() {
    let server = Server::start(vec![Action::Reset, ok()]);
    let verdict = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(verdict.relation, TextRelation::Replaced);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn a_closed_port_gives_a_transport_error() {
    // Bind and free a port, so nothing listens on it.
    let addr = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr");
    let judge = judge_for(&format!("http://{addr}/v1"), MODEL);
    let error = judge
        .judge(&question(), in_secs(1.0))
        .await
        .expect_err("nothing listens");
    assert_eq!(error.kind(), JudgeErrorKind::Transport);
    assert!(error.is_retryable());
    assert_no_key(&error);
    assert!(!error.message().contains(&addr.port().to_string()));
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let huge = vec![b' '; MAX_RESPONSE_BYTES + 1];
    for content_length in [true, false] {
        let server = Server::start(vec![
            Action::Respond {
                status: 200,
                headers: Vec::new(),
                body: huge.clone(),
                content_length,
            },
            ok(),
        ]);
        let error = judge(&server)
            .judge(&question(), in_secs(10.0))
            .await
            .expect_err("too large");
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        assert!(error.message().contains("1 MiB"));
        assert_eq!(server.count(), 1, "content-length: {content_length}");
    }
}

#[tokio::test]
async fn a_body_of_exactly_the_limit_is_read() {
    let mut body = completion("stop", ANSWER).into_bytes();
    body.resize(MAX_RESPONSE_BYTES, b' ');
    let server = Server::start(vec![Action::Respond {
        status: 200,
        headers: Vec::new(),
        body,
        content_length: false,
    }]);
    judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
}

#[tokio::test]
async fn the_key_never_shows_in_any_error_text() {
    let echo = format!("Incorrect API key provided: {KEY}; got header authorization: Bearer {KEY}");
    let refusal = completion_with("stop", json!({"role": "assistant", "refusal": KEY}));
    let odd_finish = json!({"choices": [{"message": {"content": KEY}, "finish_reason": KEY}]});
    let key_param = json!({"error": {"message": echo, "type": KEY, "param": KEY, "code": KEY}});
    let script = vec![
        respond(401, api_error("invalid_request_error", &echo)),
        respond(403, key_param.to_string()),
        respond(400, api_error("invalid_request_error", &echo)),
        respond(404, echo.clone()),
        respond(200, refusal),
        respond(200, odd_finish.to_string()),
        respond(200, completion("length", KEY)),
        respond(200, completion("stop", KEY)),
        respond(200, completion("content_filter", KEY)),
        respond(200, echo.clone()),
        respond_with(307, &[("location", &format!("/{KEY}"))], echo.clone()),
        Action::Hang,
        // Three server errors that echo the key, to cover the retry path.
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("server_error", &echo),
        ),
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("server_error", &echo),
        ),
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("server_error", &echo),
        ),
    ];
    let kinds = [
        JudgeErrorKind::Api,
        JudgeErrorKind::Api,
        JudgeErrorKind::Api,
        JudgeErrorKind::Api,
        JudgeErrorKind::Refused,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::Truncated,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::Refused,
        JudgeErrorKind::InvalidOutput,
        JudgeErrorKind::Api,
        JudgeErrorKind::Deadline,
        JudgeErrorKind::Api,
    ];
    let server = Server::start(script);
    let judge = judge(&server);
    for kind in kinds {
        let error = judge
            .judge(&question(), in_secs(0.6))
            .await
            .expect_err("fails");
        assert_eq!(error.kind(), kind, "{error}");
        assert_no_key(&error);
    }
    assert_eq!(server.count(), 15);
    let shown = format!("{judge:?} {:?}", judge.config());
    assert!(!shown.contains(KEY), "{shown}");
    assert!(!shown.contains("LOCAL-ONLY"), "{shown}");
    assert!(!format!("{:?}", ApiKey::new(KEY).expect("key")).contains("LOCAL-ONLY"));

    // The probe reports errors the same way.
    let server = Server::start(vec![respond(
        401,
        api_error("invalid_request_error", &echo),
    )]);
    let error = judge_for(&server.url(), MODEL)
        .probe(in_secs(5.0))
        .await
        .expect_err("fails");
    assert_eq!(error.http_status(), Some(401));
    assert!(error.message().contains("[redacted]"));
    assert_no_key(&error);
}

#[tokio::test]
async fn the_probe_asks_for_the_model() {
    let server = Server::start(vec![
        respond(200, json!({"id": MODEL, "object": "model"}).to_string()),
        respond(
            404,
            api_error("invalid_request_error", "The model does not exist"),
        ),
        Action::Hang,
    ]);
    let judge = Arc::new(judge(&server));
    judge.probe(in_secs(5.0)).await.expect("the probe passes");
    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v1/models/gpt-6.1-sol");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(request.header("x-api-key").is_none());
    assert!(request.body.is_empty());

    let error = judge.probe(in_secs(5.0)).await.expect_err("no model");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(404));

    let error = judge.probe(in_secs(0.3)).await.expect_err("hangs");
    assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    assert_eq!(server.count(), 3);
    // A probe never changes what the judge sends.
    assert!(judge.effort_on());
}

#[tokio::test]
async fn the_startup_probe_runs_in_the_background() {
    for status in [200, 401, 500] {
        let server = Server::start(vec![respond(status, "{}".to_owned())]);
        let judge = Arc::new(judge(&server));
        judge.spawn_startup_probe().await.expect("the task ends");
        assert_eq!(server.count(), 1, "{status}");
        assert_eq!(server.requests()[0].path, "/v1/models/gpt-6.1-sol");
    }
}

#[test]
fn only_loopback_may_use_plain_http() {
    let key = || ApiKey::new(KEY).expect("key");
    let config = |url: &str| OpenAiConfig {
        base_url: url.to_owned(),
        ..OpenAiConfig::default()
    };
    assert!(OpenAiJudge::new(key(), config("http://127.0.0.1:1/v1")).is_ok());
    assert!(OpenAiJudge::new(key(), config("http://localhost:1/v1")).is_ok());
    assert!(OpenAiJudge::new(key(), config("https://api.openai.com/v1")).is_ok());
    assert!(OpenAiJudge::new(key(), config("https://gateway.example.com/openai/v1")).is_ok());
    for url in [
        "http://api.openai.com/v1",
        "http://192.168.1.10:8080/v1",
        "https://user:pass@api.openai.com/v1",
        "https://api.openai.com/v1?x=1",
        "ftp://api.openai.com/v1",
        "not a url",
    ] {
        let error = OpenAiJudge::new(key(), config(url)).expect_err(url);
        assert_eq!(error.kind(), JudgeErrorKind::Transport, "{url}");
        assert_no_key(&error);
        assert!(!error.message().contains("user:pass"));
    }
}
