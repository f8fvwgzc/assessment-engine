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

/// A fake Codex command line in a directory of its own. It records its
/// arguments and its stdin, writes one answer and reports its tokens.
#[cfg(all(unix, feature = "judge"))]
struct FakeCodex {
    dir: std::path::PathBuf,
    program: std::path::PathBuf,
}

#[cfg(all(unix, feature = "judge"))]
impl Drop for FakeCodex {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(all(unix, feature = "judge"))]
impl FakeCodex {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "assessment-server-fake-codex-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a test directory");
        let program = dir.join("fake-codex");
        let script = format!(
            r#"#!/bin/sh
dir='{dir}'
printf '%s\n' "$@" > "$dir/argv"
env > "$dir/env"
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
cat > "$dir/stdin"
# With the file `hang` there, the program starts a helper and waits for
# it, like a launcher script around the real binary.
if [ -e "$dir/hang" ]; then
  sleep 40 &
  echo $! > "$dir/helper"
  echo $$ > "$dir/pid"
  wait
fi
printf '%s' '{{"relation":"equivalent","confidence":0.97,"rationale":"Both say the same."}}' > "$out"
echo '{{"type":"turn.completed","usage":{{"input_tokens":321,"output_tokens":45}}}}'
"#,
            dir = dir.display()
        );
        // Written by another process, so that this one never holds the
        // file open for writing while a test starts it.
        let mut writer = Command::new("sh")
            .arg("-c")
            .arg(r#"cat > "$1" && chmod 700 "$1""#)
            .arg("sh")
            .arg(&program)
            .stdin(Stdio::piped())
            .spawn()
            .expect("sh starts");
        writer
            .stdin
            .take()
            .expect("a pipe")
            .write_all(script.as_bytes())
            .expect("the script is written");
        assert!(writer.wait().expect("sh ends").success());
        Self { dir, program }
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    /// `assess --judge` with this program as the Codex command line. The
    /// temporary directory of the engine is a directory of this test, so
    /// the test sees what the judge leaves there.
    fn assess(&self, timeout_secs: &str) -> Command {
        let tmp = self.tmp();
        std::fs::create_dir_all(&tmp).expect("a temporary directory");
        let mut command = engine();
        command
            .args(["assess", "-", "--judge", "--judge-provider", "codex-cli"])
            .arg("--judge-cli-path")
            .arg(&self.program)
            .args(["--judge-timeout-secs", timeout_secs])
            .env("TMPDIR", &tmp);
        command
    }

    fn tmp(&self) -> std::path::PathBuf {
        self.dir.join("tmp")
    }

    /// What the engine left in its temporary directory.
    fn left_in_tmp(&self) -> Vec<String> {
        std::fs::read_dir(self.tmp())
            .expect("readable")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// Runs the program once with a good answer, then makes every later
    /// run hang. The first start of a new script can take seconds, and
    /// the tests below count time.
    fn warm_up_then_hang(&self) {
        let output = run_with_stdin(self.assess("60"), &request(PARAPHRASE));
        assert_eq!(description_judgment(&json(&output))["status"], "applied");
        std::fs::write(self.dir.join("hang"), "").expect("a flag file");
    }

    /// Waits until the hanging program has written a process id.
    fn pid(&self, name: &str) -> u32 {
        for _ in 0..1000 {
            if let Ok(pid) = self.read(name).trim().parse() {
                return pid;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the fake program never wrote {name}");
    }
}

#[cfg(all(unix, feature = "judge"))]
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .status()
        .expect("kill runs")
        .success()
}

/// Whether the process goes away within two seconds. One that stays is
/// killed, so a failed test leaves nothing behind.
#[cfg(all(unix, feature = "judge"))]
fn goes_away(pid: u32) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
    false
}

/// The command ends right after the judge gave up. The program and the
/// helper it started must be gone by then, and the private directory too:
/// nothing may be left to a task that the ending runtime never runs.
#[cfg(all(unix, feature = "judge"))]
#[test]
fn a_judge_that_times_out_leaves_no_process_and_no_directory() {
    let fake = FakeCodex::new("timeout");
    fake.warm_up_then_hang();
    assert_eq!(fake.left_in_tmp(), Vec::<String>::new());

    let output = run_with_stdin(fake.assess("2"), &request(PARAPHRASE));
    let judged = json(&output);
    let judgment = description_judgment(&judged);
    assert_eq!(judgment["status"], "unavailable", "{judgment}");
    assert!(
        stderr(&output).contains("1 flagged, 0 judged (codex-cli/default"),
        "{}",
        stderr(&output)
    );
    let (pid, helper) = (fake.pid("pid"), fake.pid("helper"));
    let (pid_gone, helper_gone) = (goes_away(pid), goes_away(helper));
    assert!(pid_gone, "the program outlived the command");
    assert!(
        helper_gone,
        "the helper of the program outlived the command"
    );
    assert_eq!(fake.left_in_tmp(), Vec::<String>::new());
}

/// Ctrl-C in a terminal does not reach the program, which has a process
/// group of its own. The command takes the signal, stops the program and
/// its helper, removes the private directory and ends with an error.
#[cfg(all(unix, feature = "judge"))]
#[test]
fn an_interrupt_stops_the_program_and_removes_its_directory() {
    for signal in ["-INT", "-TERM"] {
        let fake = FakeCodex::new(&format!("signal{signal}"));
        fake.warm_up_then_hang();
        let _ = std::fs::remove_file(fake.dir.join("pid"));
        let _ = std::fs::remove_file(fake.dir.join("helper"));

        let mut child = fake
            .assess("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary starts");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(request(PARAPHRASE).as_bytes())
            .expect("the request is written");
        let (pid, helper) = (fake.pid("pid"), fake.pid("helper"));
        assert!(alive(pid) && alive(helper));
        assert_eq!(fake.left_in_tmp().len(), 1, "the directory of the call");

        let sent = Command::new("kill")
            .arg(signal)
            .arg(child.id().to_string())
            .status()
            .expect("kill runs");
        assert!(sent.success());
        let output = child.wait_with_output().expect("the binary ends");
        // It ended by its own choice, not by the signal, and printed no
        // assessment.
        assert_eq!(
            output.status.code(),
            Some(1),
            "{signal}: {:?}",
            output.status
        );
        assert!(output.stdout.is_empty(), "{signal}");
        assert!(
            stderr(&output).contains("interrupted"),
            "{signal}: {}",
            stderr(&output)
        );

        let (pid_gone, helper_gone) = (goes_away(pid), goes_away(helper));
        assert!(pid_gone, "{signal}: the program is still running");
        assert!(helper_gone, "{signal}: the helper is still running");
        assert_eq!(fake.left_in_tmp(), Vec::<String>::new(), "{signal}");
    }
}

#[cfg(all(unix, feature = "judge"))]
#[test]
fn assess_with_a_command_line_provider_needs_no_key() {
    let fake = FakeCodex::new("assess");
    let mut command = engine();
    command
        .args(["assess", "-", "--judge", "--judge-provider", "codex-cli"])
        .arg("--judge-cli-path")
        .arg(&fake.program)
        // Not a real key. It and the server's own token must not reach
        // the program.
        .env("ANTHROPIC_API_KEY", "dummy-key-0123456789")
        .env("ASSESSMENT_AUTH_TOKEN", "server-token-0123456789")
        // Not real either. A variable like these would point a program at
        // another account or endpoint, or is a secret of the host.
        .env("ANTHROPIC_AUTH_TOKEN", "other-token-0123456789")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token-0123456789")
        .env("OPENAI_API_KEY", "openai-key-0123456789")
        .env(
            "DATABASE_URL",
            "postgres://user:database-password@127.0.0.1/db",
        )
        // The first start of a new script can be slow.
        .env("ASSESSMENT_JUDGE_TIMEOUT_SECS", "60");
    let output = run_with_stdin(command, &request(PARAPHRASE));
    let judged = json(&output);
    let judgment = description_judgment(&judged);
    assert_eq!(judgment["status"], "applied", "{judgment}");

    let report = stderr(&output);
    assert!(
        report
            .contains("1 flagged, 1 judged (codex-cli/default, 321/45 tokens, cost not measured)"),
        "{report}"
    );
    assert!(!report.contains('$'), "{report}");

    // The texts went to stdin only, and the program ran without the
    // secrets of this process.
    let argv = fake.read("argv");
    assert!(argv.starts_with("exec\n"), "{argv}");
    assert!(argv.contains("read-only"), "{argv}");
    assert!(!argv.contains("log in"), "{argv}");
    assert!(!argv.contains("sign in"), "{argv}");
    let stdin = fake.read("stdin");
    assert!(stdin.contains(PARAPHRASE.0), "{stdin}");
    assert!(stdin.contains(PARAPHRASE.1), "{stdin}");
    let env = fake.read("env");
    assert!(!env.contains("ANTHROPIC_API_KEY"), "{env}");
    assert!(!env.contains("dummy-key-0123456789"), "{env}");
    assert!(!env.contains("server-token-0123456789"), "{env}");
    for name in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "DATABASE_URL",
        "ASSESSMENT_",
        "0123456789",
        "database-password",
    ] {
        assert!(!env.contains(name), "{name} reached the program: {env}");
    }
    assert!(env.contains("PATH="), "{env}");
    assert!(
        argv.contains("-c\nshell_environment_policy.inherit=none\n"),
        "{argv}"
    );
}

#[cfg(feature = "judge")]
#[test]
fn a_command_line_provider_whose_program_is_missing_is_an_error() {
    const MISSING: &str = "/nonexistent/assessment-judge-cli";
    for provider in ["claude-cli", "codex-cli"] {
        let mut command = engine();
        command.args([
            "assess",
            "-",
            "--judge",
            "--judge-provider",
            provider,
            "--judge-cli-path",
            MISSING,
        ]);
        let output = run_with_stdin(command, &request(PARAPHRASE));
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("was not found"),
            "{}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty());

        // `serve --judge on` refuses to start. The provider and the path
        // are read from the environment here.
        let output = engine()
            .args(["serve", "--addr", "127.0.0.1:0", "--judge", "on"])
            .env("ASSESSMENT_JUDGE_PROVIDER", provider)
            .env("ASSESSMENT_JUDGE_CLI_PATH", MISSING)
            .stdin(Stdio::null())
            .output()
            .expect("the binary runs");
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("was not found"),
            "{}",
            stderr(&output)
        );
    }

    // A flag of the API has no meaning with a command line provider.
    let mut command = engine();
    command
        .args(["assess", "-", "--judge", "--judge-provider", "codex-cli"])
        .env("ASSESSMENT_JUDGE_BASE_URL", CLOSED);
    let output = run_with_stdin(command, &request(PARAPHRASE));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--judge-base-url"),
        "{}",
        stderr(&output)
    );

    // A provider that does not exist is a usage error.
    let output = engine()
        .args([
            "serve",
            "--addr",
            "127.0.0.1:0",
            "--judge-provider",
            "openai",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
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
