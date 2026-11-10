//! The command line, run as a process. Nothing here reaches a real model:
//! the judge is pointed at a local address.

// Without the `judge` feature only some of the tests are built.
#![cfg_attr(not(feature = "judge"), allow(dead_code, unused_imports))]

use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};

use assessment_core::model::{AssessmentRequest, ChangeSet, Entity, StatusCategory};
use assessment_core::semver::SemVer;
use serde_json::Value;

/// A local port nothing listens on.
const CLOSED: &str = "http://127.0.0.1:9";

const PARAPHRASE: (&str, &str) = (
    "Users can log in with their email address and password.",
    "People are able to sign in using e-mail and a password.",
);
const TYPO: (&str, &str) = (
    "Users can log in with thier email address and password.",
    "Users can log in with their email address and password.",
);

/// The binary, without any setting of the machine that runs the tests.
fn engine() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_assessment-engine"));
    for (name, _) in std::env::vars_os() {
        let name_text = name.to_string_lossy();
        if name_text.starts_with("ASSESSMENT_") || name_text.starts_with("ANTHROPIC_") {
            command.env_remove(&name);
        }
    }
    command.env_remove("RUST_LOG");
    command
}

fn issue(description: &str) -> Entity {
    Entity::new("P", "issue")
        .with_title("Parent issue")
        .with_description(description)
        .with_status(StatusCategory::Started, "In Progress")
        .with_version(SemVer::new(1, 0, 0))
}

/// The description of one issue goes from `pair.0` to `pair.1`.
fn request(pair: (&str, &str)) -> String {
    let request = AssessmentRequest {
        changes: vec![ChangeSet::update(issue(pair.0), issue(pair.1))],
        ..Default::default()
    };
    let mut request = serde_json::to_value(&request).expect("the request is JSON");
    // A fixed time, so that two runs give the same answer.
    request["now"] = Value::from("2026-11-09T09:00:00Z");
    request.to_string()
}

fn run_with_stdin(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input.as_bytes())
        .expect("the request is written");
    child.wait_with_output().expect("the binary ends")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).expect("stdout is JSON")
}

/// The judgment of the description of the one change.
fn description_judgment(assessment: &Value) -> &Value {
    assessment["changes"][0]["fields"]
        .as_array()
        .expect("the change has fields")
        .iter()
        .find(|f| f["field"] == "description")
        .map(|f| &f["judgment"])
        .expect("the description changed")
}

fn judged_assess(base_url: &str) -> Command {
    let mut command = engine();
    command
        .args(["assess", "-", "--judge"])
        .env("ANTHROPIC_API_KEY", "dummy")
        .env("ASSESSMENT_JUDGE_BASE_URL", base_url)
        .env("ASSESSMENT_JUDGE_TIMEOUT_SECS", "5");
    command
}

#[cfg(feature = "judge")]
#[test]
fn the_shortest_judge_timeout_still_calls_the_judge() {
    let mut command = judged_assess(CLOSED);
    command.env("ASSESSMENT_JUDGE_TIMEOUT_SECS", "1");
    let judged = json(&run_with_stdin(command, &request(PARAPHRASE)));
    let judgment = description_judgment(&judged);
    assert_eq!(judgment["status"], "unavailable", "{judgment}");
    // The call was tried: it was not refused for lack of time.
    assert_ne!(judgment["detail"], "not enough time left", "{judgment}");
}

#[cfg(feature = "judge")]
#[test]
fn a_judge_that_cannot_be_reached_never_fails_assess() {
    let mut plain = engine();
    plain.args(["assess", "-"]);
    let lexical = json(&run_with_stdin(plain, &request(PARAPHRASE)));
    assert_eq!(description_judgment(&lexical)["status"], "requested");

    let output = run_with_stdin(judged_assess(CLOSED), &request(PARAPHRASE));
    let judged = json(&output);
    let judgment = description_judgment(&judged);
    assert_eq!(judgment["status"], "unavailable", "{judgment}");
    let detail = judgment["detail"].as_str().expect("a detail");
    assert!(!detail.is_empty());
    assert!(!detail.contains("dummy"), "{detail}");

    // The result is the engine's own.
    assert_eq!(judged["changes"][0]["bump"], lexical["changes"][0]["bump"]);
    assert_eq!(
        judged["changes"][0]["to_version"],
        lexical["changes"][0]["to_version"]
    );
    assert_eq!(judged["score"], lexical["score"]);
    assert_eq!(judged["level"], lexical["level"]);

    let report = stderr(&output);
    assert!(
        report.contains("judge: 1 flagged, 0 judged (claude-opus-5-5, 0/0 tokens, ~$0.0000)"),
        "{report}"
    );
    assert!(!report.contains("dummy"), "{report}");
}

#[cfg(feature = "judge")]
#[test]
fn an_unflagged_change_never_connects_to_the_judge() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    listener
        .set_nonblocking(true)
        .expect("the listener does not block");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("the listener has an address")
    );

    let output = run_with_stdin(judged_assess(&base_url), &request(TYPO));
    let judged = json(&output);
    assert!(description_judgment(&judged).is_null(), "{judged}");
    assert!(
        stderr(&output).contains("judge: 0 flagged, 0 judged"),
        "{}",
        stderr(&output)
    );
    let connection = listener.accept();
    assert!(
        matches!(&connection, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "{connection:?}"
    );

    let mut plain = engine();
    plain.args(["assess", "-"]);
    assert_eq!(judged, json(&run_with_stdin(plain, &request(TYPO))));
}

#[cfg(feature = "judge")]
#[test]
fn assess_with_the_judge_needs_a_key() {
    let mut command = engine();
    command
        .args(["assess", "-", "--judge"])
        .env("ASSESSMENT_JUDGE_BASE_URL", CLOSED);
    let output = run_with_stdin(command, &request(PARAPHRASE));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("needs an API key"),
        "{}",
        stderr(&output)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn the_judge_mode_is_read_from_the_environment() {
    // `on` without a key: the server refuses to start.
    let output = engine()
        .args(["serve", "--addr", "127.0.0.1:0"])
        .env("ASSESSMENT_JUDGE", "on")
        .env("ASSESSMENT_JUDGE_BASE_URL", CLOSED)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("needs an API key"),
        "{}",
        stderr(&output)
    );

    // A value that is not a mode is a usage error.
    let output = engine()
        .args(["serve", "--addr", "127.0.0.1:0"])
        .env("ASSESSMENT_JUDGE", "maybe")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("'maybe'"), "{}", stderr(&output));

    // So is a bad value of a judge setting.
    let output = engine()
        .args(["serve", "--addr", "127.0.0.1:0"])
        .env("ASSESSMENT_JUDGE_EFFORT", "extreme")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
}

#[test]
fn compare_text_reports_the_gray_zone_of_a_field() {
    for field in ["description", "title"] {
        let output = engine()
            .args([
                "compare-text",
                "--field",
                field,
                "--before",
                PARAPHRASE.0,
                "--after",
                PARAPHRASE.1,
            ])
            .output()
            .expect("the binary runs");
        let diff = json(&output);
        assert!(diff.get("magnitude").is_some(), "{diff}");
        if field == "description" {
            let zone = &diff["gray_zone"];
            assert!(zone["fingerprint"].is_string(), "{diff}");
            assert!(
                zone["reasons"].as_array().is_some_and(|r| !r.is_empty()),
                "{diff}"
            );
        }
    }
    let output = engine()
        .args([
            "compare-text",
            "--before",
            "Add a login page",
            "--after",
            "Add a login page",
        ])
        .output()
        .expect("the binary runs");
    assert!(json(&output).get("gray_zone").is_none_or(Value::is_null));
}
