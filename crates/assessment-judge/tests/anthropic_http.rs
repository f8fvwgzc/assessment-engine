//! The Anthropic judge against a local, hand-rolled HTTP/1.1 server.
//!
//! Nothing here leaves the machine: every server listens on 127.0.0.1.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assessment_core::model::TextRelation;
use assessment_judge::anthropic::{API_VERSION, FALLBACKS_BETA, MAX_RESPONSE_BYTES};
use assessment_judge::prompt::{SYSTEM_PROMPT, schema, user_content};
use assessment_judge::{
    AnthropicConfig, AnthropicJudge, ApiKey, Fallbacks, Hints, Judge, JudgeError, JudgeErrorKind,
    Question, ask,
};
use serde_json::{Value, json};
use tokio::time::Instant;

/// A made-up key. It is not a real credential.
const KEY: &str = "sk-ant-test-LOCAL-ONLY-0123456789abcdef";

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

    fn url(&self) -> String {
        format!("http://{}", self.addr)
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
    json!({"type": "error", "error": {"type": kind, "message": message}}).to_string()
}

const ANSWER: &str =
    r#"{"relation":"replaced","confidence":0.93,"rationale":"\"http\" became \"https\"."}"#;

fn message(stop_reason: &str, text: &str) -> String {
    json!({
        "id": "msg_local",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": text}],
        "stop_reason": stop_reason,
        "usage": {"input_tokens": 812, "output_tokens": 37}
    })
    .to_string()
}

fn ok() -> Action {
    respond(200, message("end_turn", ANSWER))
}

fn judge_for(url: &str, model: &str) -> AnthropicJudge {
    let config = AnthropicConfig {
        base_url: url.to_owned(),
        model: model.to_owned(),
        ..AnthropicConfig::default()
    };
    AnthropicJudge::new(ApiKey::new(KEY).expect("key"), config).expect("judge")
}

fn judge(server: &Server) -> AnthropicJudge {
    judge_for(&server.url(), "claude-opus-5-5")
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

#[tokio::test]
async fn the_documented_headers_and_body_are_sent() {
    let server = Server::start(vec![ok()]);
    let judge = judge(&server);
    let question = question();
    let verdict = judge
        .judge(&question, in_secs(10.0))
        .await
        .expect("verdict");
    assert_eq!(verdict.relation, TextRelation::Replaced);
    assert_eq!(verdict.confidence, 0.93);
    assert_eq!(verdict.rationale, "\"http\" became \"https\".");
    assert_eq!(verdict.source, "anthropic/claude-opus-5-5");
    assert_eq!(verdict.usage.input_tokens, 812);
    assert_eq!(verdict.usage.output_tokens, 37);
    assert!(!verdict.cached);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("x-api-key"), Some(KEY));
    assert_eq!(request.header("anthropic-version"), Some(API_VERSION));
    assert_eq!(request.header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(request.header("anthropic-beta"), Some(FALLBACKS_BETA));
    assert_eq!(
        request.header("anthropic-beta"),
        Some("server-side-fallback-2026-07-01")
    );
    assert!(request.header("authorization").is_none());
    assert!(
        request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("assessment-judge/"))
    );

    let expected = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 4096,
        "system": [{
            "type": "text",
            "text": SYSTEM_PROMPT,
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": user_content(&question)}],
        "output_config": {
            "effort": "low",
            "format": {"type": "json_schema", "schema": schema()}
        },
        "fallbacks": "default"
    });
    assert_eq!(request.json(), expected);

    // The user content is the question and nothing else.
    let user: Value = serde_json::from_str(
        request.json()["messages"][0]["content"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
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
async fn other_models_get_no_fallbacks_and_no_beta_header() {
    let server = Server::start(vec![ok()]);
    let judge = judge_for(&server.url(), "claude-sonnet-5-5");
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    let request = &server.requests()[0];
    assert!(request.header("anthropic-beta").is_none());
    let body = request.json();
    assert_eq!(body["model"], "claude-sonnet-5-5");
    assert!(body.get("fallbacks").is_none());
}

#[tokio::test]
async fn a_refusal_makes_exactly_one_request() {
    let body = json!({
        "model": "claude-opus-5-5",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "cyber"},
        "usage": {"input_tokens": 800, "output_tokens": 3}
    });
    let server = Server::start(vec![respond(200, body.to_string()), ok()]);
    let error = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("refused");
    assert_eq!(error.kind(), JudgeErrorKind::Refused);
    assert!(error.message().contains("cyber"));
    assert_eq!(error.usage().map(|u| u.input_tokens), Some(800));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn a_cut_off_or_broken_answer_makes_one_request() {
    let server = Server::start(vec![
        respond(200, message("max_tokens", "{\"relation\":\"repl")),
        respond(200, message("end_turn", "I think it changed.")),
        respond(200, "<html>not json</html>".to_owned()),
    ]);
    let judge = judge(&server);
    let kinds = [
        JudgeErrorKind::Truncated,
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
async fn a_429_with_retry_after_ms_then_200_succeeds() {
    let server = Server::start(vec![
        respond_with(
            429,
            &[("retry-after-ms", "150"), ("retry-after", "30")],
            api_error("rate_limit_error", "slow down"),
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
}

#[tokio::test]
async fn a_retry_after_beyond_the_deadline_fails_at_once() {
    let server = Server::start(vec![
        respond_with(
            429,
            &[("retry-after", "30")],
            api_error("rate_limit_error", "slow down"),
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
    assert_eq!(error.message(), "HTTP 429 rate_limit_error: slow down");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn server_errors_are_tried_three_times() {
    let server = Server::start(vec![
        respond(500, api_error("api_error", "internal")),
        respond(529, api_error("overloaded_error", "overloaded")),
        respond(500, api_error("api_error", "internal again")),
        ok(),
    ]);
    let error = judge(&server)
        .judge(&question(), in_secs(20.0))
        .await
        .expect_err("fails");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(500));
    assert_eq!(error.message(), "HTTP 500 api_error: internal again");
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn retries_stop_at_the_deadline() {
    let server = Server::start(vec![
        respond(503, api_error("api_error", "down")),
        respond(503, api_error("api_error", "down")),
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
        let error = judge(&server)
            .judge(&question(), in_secs(10.0))
            .await
            .expect_err("fails");
        assert_eq!(error.kind(), JudgeErrorKind::Api);
        assert_eq!(error.http_status(), Some(status));
        assert_eq!(server.count(), 1, "{status}");
    }
}

#[tokio::test]
async fn a_400_that_names_fallbacks_turns_them_off_for_good() {
    let server = Server::start(vec![
        respond(
            400,
            api_error(
                "invalid_request_error",
                "fallbacks: Extra inputs are not permitted",
            ),
        ),
        ok(),
        ok(),
    ]);
    let judge = judge(&server);
    assert!(judge.fallbacks_on());
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    assert!(!judge.fallbacks_on());
    judge
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].header("anthropic-beta").is_some());
    assert_eq!(requests[0].json()["fallbacks"], "default");
    for request in &requests[1..] {
        assert!(request.header("anthropic-beta").is_none());
        assert!(request.json().get("fallbacks").is_none());
    }
}

#[tokio::test]
async fn a_second_400_about_fallbacks_is_not_retried_again() {
    let rejected = || {
        respond(
            400,
            api_error("invalid_request_error", "fallbacks is not supported"),
        )
    };
    let server = Server::start(vec![rejected(), rejected(), ok()]);
    let error = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect_err("fails");
    assert_eq!(error.http_status(), Some(400));
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let target = Server::start(vec![ok()]);
    let server = Server::start(vec![
        respond_with(
            307,
            &[("location", &format!("{}/v1/messages", target.url()))],
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
    let judge = judge_for(&format!("http://{addr}"), "claude-opus-5-5");
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
    let mut body = message("end_turn", ANSWER).into_bytes();
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

/// An endpoint could repeat the key in a 200 response: as the model name
/// and inside the answer. Both go on to the caller, so the key is taken
/// out of them.
#[tokio::test]
async fn a_key_echoed_in_an_answer_never_reaches_the_verdict() {
    let answer = json!({
        "relation": "replaced",
        "confidence": 0.9,
        "rationale": format!("key is {KEY}.")
    })
    .to_string();
    let mut body: Value = serde_json::from_str(&message("end_turn", &answer)).expect("json");
    body["model"] = json!(KEY);
    let server = Server::start(vec![respond(200, body.to_string())]);
    let verdict = judge(&server)
        .judge(&question(), in_secs(10.0))
        .await
        .expect("verdict");
    let shown = format!("{verdict:?}");
    assert!(!shown.contains(KEY), "{shown}");
    assert!(!shown.contains("LOCAL-ONLY"), "{shown}");
    assert_eq!(verdict.source, "anthropic/claude-opus-5-5");
    assert_eq!(verdict.rationale, "key is [redacted].");
}

#[tokio::test]
async fn the_key_never_shows_in_any_error_text() {
    let echo = format!("invalid x-api-key {KEY}; got header x-api-key: {KEY}");
    let refusal = json!({
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"category": KEY},
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let odd_stop = json!({"content": [], "stop_reason": KEY});
    let script = vec![
        respond(401, api_error("authentication_error", &echo)),
        respond(403, api_error(KEY, &echo)),
        respond(400, api_error("invalid_request_error", &echo)),
        respond(404, echo.clone()),
        respond(200, refusal.to_string()),
        respond(200, odd_stop.to_string()),
        respond(200, message("max_tokens", KEY)),
        respond(200, message("end_turn", KEY)),
        respond(200, echo.clone()),
        respond_with(307, &[("location", &format!("/{KEY}"))], echo.clone()),
        Action::Hang,
        // Three server errors that echo the key, to cover the retry path.
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("api_error", &echo),
        ),
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("api_error", &echo),
        ),
        respond_with(
            500,
            &[("retry-after-ms", "1")],
            api_error("api_error", &echo),
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
    assert_eq!(server.count(), 14);
    assert!(!format!("{judge:?}").contains(KEY));

    // The probe reports errors the same way.
    let server = Server::start(vec![respond(401, api_error("authentication_error", &echo))]);
    let error = judge_for(&server.url(), "claude-opus-5-5")
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
        respond(200, json!({"id": "claude-opus-5-5"}).to_string()),
        respond(404, api_error("not_found_error", "model: claude-opus-5-5")),
        Action::Hang,
    ]);
    let judge = Arc::new(judge(&server));
    judge.probe(in_secs(5.0)).await.expect("the probe passes");
    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v1/models/claude-opus-5-5");
    assert_eq!(request.header("x-api-key"), Some(KEY));
    assert_eq!(request.header("anthropic-version"), Some(API_VERSION));
    assert!(request.header("anthropic-beta").is_none());
    assert!(request.body.is_empty());

    let error = judge.probe(in_secs(5.0)).await.expect_err("no model");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert_eq!(error.http_status(), Some(404));

    let error = judge.probe(in_secs(0.3)).await.expect_err("hangs");
    assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn the_startup_probe_runs_in_the_background() {
    let server = Server::start(vec![respond(200, "{}".to_owned())]);
    let judge = Arc::new(judge(&server));
    judge.spawn_startup_probe().await.expect("the task ends");
    assert_eq!(server.count(), 1);
    assert_eq!(server.requests()[0].path, "/v1/models/claude-opus-5-5");
}

#[test]
fn only_loopback_may_use_plain_http() {
    let key = || ApiKey::new(KEY).expect("key");
    let config = |url: &str| AnthropicConfig {
        base_url: url.to_owned(),
        fallbacks: Fallbacks::Off,
        ..AnthropicConfig::default()
    };
    assert!(AnthropicJudge::new(key(), config("http://127.0.0.1:1")).is_ok());
    assert!(AnthropicJudge::new(key(), config("https://api.anthropic.com")).is_ok());
    for url in [
        "http://api.anthropic.com",
        "http://192.168.1.10:8080",
        "https://user:pass@api.anthropic.com",
        "https://api.anthropic.com?x=1",
    ] {
        let error = AnthropicJudge::new(key(), config(url)).expect_err(url);
        assert_no_key(&error);
        assert!(!error.message().contains("user:pass"));
    }
}
