//! The CLI judges against fake programs. No real CLI is run and nothing
//! leaves the machine: each test writes a small shell script and gives its
//! path to the judge as the program.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use assessment_core::model::TextRelation;
use assessment_judge::{CliConfig, CliJudge, CliKind, Judge, JudgeError, JudgeErrorKind, Question};
use tokio::time::Instant;

const KINDS: [CliKind; 2] = [CliKind::ClaudeCode, CliKind::Codex];
const BEFORE: &str = "MARKER-BEFORE the user can pay by card";
const AFTER: &str = "MARKER-AFTER the user cannot pay by card; $(touch pwned) `id` --tools all";
const ANSWER: &str = r#"{"relation":"reversed","confidence":0.9,"rationale":"can became cannot."}"#;
const CODEX_USAGE: &str = r#"{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":0,"output_tokens":45}}"#;

/// A directory for one test, with the fake program in it. Removed at the
/// end.
struct Fake {
    dir: PathBuf,
    program: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fake {
    /// Writes a fake program. It records its arguments, its directory and
    /// its stdin, then runs `body`. `$dir` is the test directory and `$out`
    /// the path after `-o`, when there is one.
    fn new(body: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "assessment-judge-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("a test directory");
        let program = dir.join("fake-cli");
        let script = format!(
            r#"#!/bin/sh
dir='{dir}'
printf '%s\n' "$@" > "$dir/argv"
env > "$dir/env"
pwd -P > "$dir/cwd"
ls -A > "$dir/listing"
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
cat > "$dir/stdin"
{body}
"#,
            dir = dir.display()
        );
        // The file is written by another process. A file this process had
        // open for writing can fail to start ("text file busy") while
        // another test thread forks.
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

    fn judge(&self, kind: CliKind) -> CliJudge {
        self.judge_with(CliConfig::new(kind))
    }

    fn judge_with(&self, config: CliConfig) -> CliJudge {
        CliJudge::new(CliConfig {
            program: Some(self.program.clone()),
            ..config
        })
        .expect("a judge")
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    /// Waits until the fake has written a process id into `name`.
    async fn pid(&self, name: &str) -> u32 {
        for _ in 0..500 {
            if let Ok(pid) = self.read(name).trim().parse() {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the fake program never wrote {name}");
    }
}

fn question() -> Question {
    Question::new("description", "issue", BEFORE, AFTER).with_context_title("MARKER-TITLE")
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

/// A body that answers well, in the way of `kind`.
fn good(kind: CliKind) -> String {
    answering(kind, ANSWER)
}

/// A body that gives `answer` as the answer of a run that worked.
fn answering(kind: CliKind, answer: &str) -> String {
    match kind {
        CliKind::ClaudeCode => format!(
            r#"cat <<'JSON'
{{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{answer},"usage":{{"input_tokens":1500,"output_tokens":60,"cache_creation_input_tokens":7,"cache_read_input_tokens":8}},"total_cost_usd":0.01}}
JSON"#
        ),
        CliKind::Codex => format!(
            r#"cat > "$out" <<'JSON'
{answer}
JSON
cat <<'JSON'
{{"type":"thread.started","thread_id":"t"}}
{CODEX_USAGE}
JSON"#
        ),
    }
}

async fn fails(kind: CliKind, body: &str) -> JudgeError {
    let fake = Fake::new(body);
    let judge = fake.judge(kind);
    let error = judge
        .judge(&question(), soon())
        .await
        .expect_err("the call fails");
    // No failure carries the texts.
    let shown = format!("{error:?}");
    assert!(!shown.contains("MARKER"), "{shown}");
    assert!(!shown.contains("SECRET"), "{shown}");
    error
}

/// Whether the process still runs. A killed process whose parent is gone
/// can stay in the table as a zombie until the init process collects it,
/// and some init processes (a container, a CI runner) are slow to do so.
/// A zombie runs nothing, so it counts as gone.
fn alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p"])
        .arg(pid.to_string())
        .stderr(Stdio::null())
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

async fn gone(pid: u32) {
    for _ in 0..500 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process {pid} is still there");
}

#[tokio::test]
async fn a_good_answer_is_a_verdict_with_usage() {
    for kind in KINDS {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge(kind);
        let verdict = judge.judge(&question(), soon()).await.expect("a verdict");
        assert_eq!(verdict.relation, TextRelation::Reversed);
        assert_eq!(verdict.confidence, 0.9);
        assert_eq!(verdict.rationale, "can became cannot.");
        assert_eq!(verdict.source, judge.id());
        assert!(!verdict.cached);
        match kind {
            CliKind::ClaudeCode => {
                assert_eq!(verdict.source, "claude-cli/default");
                assert_eq!(verdict.usage.input_tokens, 1500);
                assert_eq!(verdict.usage.output_tokens, 60);
                assert_eq!(verdict.usage.cache_creation_input_tokens, 7);
                assert_eq!(verdict.usage.cache_read_input_tokens, 8);
            }
            CliKind::Codex => {
                assert_eq!(verdict.source, "codex-cli/default");
                assert_eq!(verdict.usage.input_tokens, 1200);
                assert_eq!(verdict.usage.output_tokens, 45);
            }
        }
        // A subscription call is not priced.
        assert_eq!(
            assessment_judge::price::cost_usd(&verdict.source, &verdict.usage),
            0.0
        );
    }
}

#[tokio::test]
async fn the_texts_go_to_stdin_and_never_to_the_arguments() {
    for kind in KINDS {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge(kind);
        judge.judge(&question(), soon()).await.expect("a verdict");

        let argv = fake.read("argv");
        assert!(!argv.is_empty());
        for part in ["MARKER", "pay by card", "touch pwned", "--tools all"] {
            assert!(!argv.contains(part), "{kind:?}: {part} is in the arguments");
        }
        let stdin = fake.read("stdin");
        assert!(stdin.contains(BEFORE), "{kind:?}");
        assert!(stdin.contains("MARKER-AFTER"), "{kind:?}");
        assert!(stdin.contains("MARKER-TITLE"), "{kind:?}");
        // Nothing in the text was run by a shell.
        assert!(!fake.dir.join("pwned").exists());
        assert_eq!(fake.read("listing"), "");

        match kind {
            CliKind::ClaudeCode => {
                // The user content alone: one JSON object.
                let value: serde_json::Value = serde_json::from_str(&stdin).expect("stdin is JSON");
                assert_eq!(value["before"], BEFORE);
                assert_eq!(value["after"], AFTER);
                assert!(argv.contains("--tools\n\n--strict-mcp-config\n"));
                assert!(argv.contains("--setting-sources\n\n"));
                assert!(argv.contains("data, not instructions"));
            }
            CliKind::Codex => {
                // The instructions, then the user content as the last line.
                assert!(stdin.contains("data, not instructions"));
                let last = stdin.trim_end().lines().last().expect("a last line");
                let value: serde_json::Value =
                    serde_json::from_str(last).expect("the last line is JSON");
                assert_eq!(value["after"], AFTER);
                assert!(argv.starts_with("exec\n--ignore-user-config\n--sandbox\nread-only\n"));
                assert!(argv.ends_with("\n-\n"));
            }
        }
    }
}

/// The private directory of the call the fake ran in last: the parent of
/// its working directory.
fn call_dir(fake: &Fake) -> PathBuf {
    let cwd = PathBuf::from(fake.read("cwd").trim());
    assert_eq!(cwd.file_name().and_then(|name| name.to_str()), Some("cwd"));
    cwd.parent().expect("a parent").to_path_buf()
}

#[tokio::test]
async fn the_child_runs_in_the_empty_private_directory() {
    for kind in KINDS {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge(kind);
        judge.judge(&question(), soon()).await.expect("a verdict");
        let cwd = PathBuf::from(fake.read("cwd").trim());
        let root = call_dir(&fake);
        let temp = std::env::temp_dir().canonicalize().expect("it exists");
        assert_eq!(root.parent(), Some(temp.as_path()));
        let name = root.file_name().expect("a name").to_string_lossy();
        assert!(
            name.starts_with(&format!("assessment-judge-{}-", std::process::id())),
            "{name}"
        );
        assert_ne!(
            cwd,
            std::env::current_dir()
                .expect("a directory")
                .canonicalize()
                .expect("it exists")
        );
        assert_eq!(fake.read("listing"), "", "the directory was empty");
        if kind == CliKind::Codex {
            // The schema file and the answer file lie next to the working
            // directory, not in it.
            let argv = fake.read("argv");
            assert!(argv.contains("/schema.json\n"));
            assert!(argv.contains("/answer.json\n"));
        }
        // The directory of the call is gone again, with the answer file.
        assert!(!root.exists());
    }
}

/// The system may clean old entries out of its temporary directory while
/// a server runs. A judge keeps nothing there between calls, so there is
/// nothing a cleaner could take away: each call has a new directory.
#[tokio::test]
async fn a_judge_keeps_nothing_in_the_temporary_directory_between_calls() {
    for kind in KINDS {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge(kind);
        judge.judge(&question(), soon()).await.expect("a verdict");
        let first = call_dir(&fake);
        assert!(
            !first.exists(),
            "{kind:?}: the first call left its directory"
        );
        judge.judge(&question(), soon()).await.expect("a verdict");
        let second = call_dir(&fake);
        assert_ne!(first, second);
        assert!(!second.exists());
    }
}

/// A failed call and a call that ran out of time leave no directory.
#[tokio::test]
async fn a_failed_call_leaves_no_directory() {
    let fake = Fake::new("exit 3");
    let judge = fake.judge(CliKind::Codex);
    judge.judge(&question(), soon()).await.expect_err("fails");
    assert!(!call_dir(&fake).exists());

    let (fake, judge) = warmed_up(CliKind::Codex, "exec sleep 30").await;
    let warm = call_dir(&fake);
    judge
        .judge(&question(), Instant::now() + Duration::from_millis(500))
        .await
        .expect_err("no answer");
    let hung = call_dir(&fake);
    assert_ne!(warm, hung);
    assert!(!hung.exists());
}

/// The child does not get the environment of the engine. A variable that
/// is not on the short list never reaches it, so no key, token or endpoint
/// that happens to be set changes where the texts go, and the agent has
/// no secret to repeat.
#[tokio::test]
async fn the_environment_of_the_engine_does_not_reach_the_cli() {
    // Cargo sets these for the test process. They stand for any variable
    // the engine has that is not on the list.
    let strangers: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| name.starts_with("CARGO_"))
        .collect();
    assert!(!strangers.is_empty(), "the tests run under cargo");
    // What a shell sets for itself is not inherited.
    let own = ["PWD", "OLDPWD", "SHLVL", "_", "__CF_USER_TEXT_ENCODING"];
    for (kind, home) in [
        (CliKind::ClaudeCode, "CLAUDE_CONFIG_DIR"),
        (CliKind::Codex, "CODEX_HOME"),
    ] {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge(kind);
        judge.judge(&question(), soon()).await.expect("a verdict");
        let env = fake.read("env");
        let names: Vec<&str> = env
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        assert!(names.contains(&"PATH"), "{kind:?}: {names:?}");
        for name in &names {
            assert!(
                assessment_judge::cli::PASSED_ENV.contains(name)
                    || *name == home
                    || own.contains(name),
                "{kind:?}: {name} reached the CLI"
            );
        }
        for stranger in &strangers {
            assert!(!names.contains(&stranger.as_str()), "{kind:?}: {stranger}");
        }
        if kind == CliKind::Codex {
            let argv = fake.read("argv");
            assert!(
                argv.contains("-c\nshell_environment_policy.inherit=none\n"),
                "{argv}"
            );
        }
    }
}

#[tokio::test]
async fn calls_at_once_do_not_share_an_answer_file() {
    let fake = Fake::new(&format!("sleep 0.2\n{}", good(CliKind::Codex)));
    let judge = fake.judge(CliKind::Codex);
    let q = question();
    let (a, b, c) = tokio::join!(
        judge.judge(&q, soon()),
        judge.judge(&q, soon()),
        judge.judge(&q, soon())
    );
    for verdict in [a, b, c] {
        assert_eq!(verdict.expect("a verdict").relation, TextRelation::Reversed);
    }
}

#[tokio::test]
async fn a_cli_that_is_not_logged_in_says_so() {
    let claude = r#"cat <<'JSON'
{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate: OAuth session expired and could not be refreshed","usage":{"input_tokens":0,"output_tokens":0}}
JSON
exit 1"#;
    let error = fails(CliKind::ClaudeCode, claude).await;
    assert_eq!(error.message(), "claude CLI is not logged in");
    assert_eq!(error.kind(), JudgeErrorKind::Api);
    assert!(error.is_persistent());
    assert!(!error.is_retryable());

    let codex = "echo 'Not logged in. Run codex login first.' >&2\nexit 1";
    let error = fails(CliKind::Codex, codex).await;
    assert_eq!(error.message(), "codex CLI is not logged in");
    assert!(error.is_persistent());
}

#[tokio::test]
async fn an_error_the_cli_reports_has_a_fixed_message() {
    let claude = r#"echo '{"type":"result","is_error":true,"result":"SECRET went wrong"}'"#;
    let error = fails(CliKind::ClaudeCode, claude).await;
    assert_eq!(error.message(), "claude CLI reported an error");
    assert_eq!(error.kind(), JudgeErrorKind::Transport);

    let codex = r#"echo '{"type":"error","message":"SECRET went wrong"}'
echo '{"type":"turn.failed","error":{"message":"SECRET went wrong"}}'
exit 1"#;
    let error = fails(CliKind::Codex, codex).await;
    assert_eq!(error.message(), "codex CLI reported an error");

    let codex = r#"echo '{"type":"turn.failed","error":{"message":"You have hit your usage limit."}}'
exit 1"#;
    let error = fails(CliKind::Codex, codex).await;
    assert_eq!(error.message(), "codex CLI hit a usage limit");
    assert!(error.is_persistent());
}

#[tokio::test]
async fn a_non_zero_exit_is_reported_with_its_status() {
    for kind in KINDS {
        let error = fails(kind, "echo 'SECRET: unknown option' >&2\nexit 3").await;
        assert_eq!(error.kind(), JudgeErrorKind::Transport);
        assert_eq!(
            error.message(),
            format!("{} CLI exited with status 3", kind.default_program())
        );
        assert!(!error.is_persistent());
    }
    // A good answer of a failed run is not believed.
    let error = fails(CliKind::Codex, &format!("{}\nexit 1", good(CliKind::Codex))).await;
    assert_eq!(error.message(), "codex CLI exited with status 1");
}

#[tokio::test]
async fn output_that_is_not_json_is_not_an_answer() {
    let error = fails(CliKind::ClaudeCode, "echo 'SECRET is not json'").await;
    assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
    assert_eq!(error.message(), "the answer is not a valid verdict");

    let error = fails(CliKind::Codex, r#"echo 'SECRET is not json' > "$out""#).await;
    assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
    // No answer file at all.
    let error = fails(CliKind::Codex, &format!("echo '{CODEX_USAGE}'")).await;
    assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
    assert_eq!(error.usage().map(|usage| usage.output_tokens), Some(45));
}

#[tokio::test]
async fn an_answer_outside_the_schema_is_not_a_verdict() {
    let answers = [
        r#"{"relation":"reversed","confidence":0.9,"rationale":"x","verdict":"SECRET"}"#,
        r#"{"relation":"reversed","confidence":0.9}"#,
        r#"{"relation":"SECRET","confidence":0.9,"rationale":"x"}"#,
        r#"{"relation":"reversed","confidence":1.5,"rationale":"x"}"#,
        r#"{"relation":"reversed","confidence":"0.9","rationale":"x"}"#,
        r#"[{"relation":"reversed","confidence":0.9,"rationale":"x"}]"#,
    ];
    for kind in KINDS {
        for answer in answers {
            let error = fails(kind, &answering(kind, answer)).await;
            assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput, "{answer}");
            assert_eq!(error.message(), "the answer is not a valid verdict");
        }
    }
}

#[tokio::test]
async fn a_long_rationale_is_cut() {
    let long = "r".repeat(2000);
    let answer = format!(r#"{{"relation":"extended","confidence":0.6,"rationale":"{long}"}}"#);
    for kind in KINDS {
        let fake = Fake::new(&answering(kind, &answer));
        let judge = fake.judge(kind);
        let verdict = judge.judge(&question(), soon()).await.expect("a verdict");
        assert_eq!(verdict.rationale.len(), 400);
    }
}

#[tokio::test]
async fn too_much_output_stops_the_call() {
    let small = |kind| CliConfig {
        max_output_bytes: 2048,
        ..CliConfig::new(kind)
    };
    for kind in KINDS {
        // Endless output on stdout: the call must not wait for its end.
        let fake = Fake::new("echo $$ > \"$dir/pid\"\nexec yes SECRET");
        let judge = fake.judge_with(small(kind));
        let started = std::time::Instant::now();
        let error = judge
            .judge(&question(), soon())
            .await
            .expect_err("too much");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(error.kind(), JudgeErrorKind::InvalidOutput);
        assert_eq!(
            error.message(),
            format!(
                "the {} CLI wrote more than 2048 bytes",
                kind.default_program()
            )
        );
        gone(fake.pid("pid").await).await;

        // A flood on stderr is read away and does not block a good answer.
        let fake = Fake::new(&format!(
            "head -c 300000 /dev/zero | tr '\\0' 'e' >&2\n{}",
            good(kind)
        ));
        let judge = fake.judge_with(small(kind));
        judge.judge(&question(), soon()).await.expect("a verdict");
    }

    // An answer file over the cap.
    let fake = Fake::new("head -c 5000 /dev/zero | tr '\\0' 'e' > \"$out\"");
    let judge = fake.judge_with(small(CliKind::Codex));
    let error = judge
        .judge(&question(), soon())
        .await
        .expect_err("too much");
    assert_eq!(error.message(), "the codex CLI wrote more than 2048 bytes");
}

/// A fake that answers well until the file `hang` is there. The first run
/// of a new program file can take seconds (the system checks it), so a
/// test with a short deadline runs the fake once before it counts time.
async fn warmed_up(kind: CliKind, hanging: &str) -> (Fake, CliJudge) {
    let fake = Fake::new(&format!(
        "if [ -e \"$dir/hang\" ]; then\n{hanging}\nfi\n{}",
        good(kind)
    ));
    let judge = fake.judge(kind);
    judge.judge(&question(), soon()).await.expect("a verdict");
    std::fs::write(fake.dir.join("hang"), "").expect("a flag file");
    (fake, judge)
}

#[tokio::test]
async fn a_hanging_cli_is_killed_at_the_deadline() {
    for kind in KINDS {
        let (fake, judge) = warmed_up(kind, "echo $$ > \"$dir/pid\"\nexec sleep 30").await;
        let started = std::time::Instant::now();
        let error = judge
            .judge(&question(), Instant::now() + Duration::from_millis(1000))
            .await
            .expect_err("no answer");
        let took = started.elapsed();
        assert_eq!(error.kind(), JudgeErrorKind::Deadline);
        assert_eq!(error.message(), "judge timed out");
        assert!(took >= Duration::from_millis(950), "{took:?}");
        assert!(took < Duration::from_millis(3000), "{took:?}");
        gone(fake.pid("pid").await).await;
    }
}

#[tokio::test]
async fn a_helper_the_cli_started_is_killed_with_it() {
    // The program starts a helper and waits for it, like a wrapper script
    // around the real binary.
    let (fake, judge) = warmed_up(
        CliKind::Codex,
        "sleep 30 &\necho $! > \"$dir/helper\"\necho $$ > \"$dir/pid\"\nwait",
    )
    .await;
    let error = judge
        .judge(&question(), Instant::now() + Duration::from_millis(1000))
        .await
        .expect_err("no answer");
    assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    gone(fake.pid("pid").await).await;
    gone(fake.pid("helper").await).await;
}

/// A one-shot command ends its runtime right after the call. The kill of
/// the helper must have happened by then: it may not wait for a task.
/// The runtime here is dropped as soon as the call returns, and only then
/// are the processes looked at.
#[test]
fn a_helper_is_killed_before_the_runtime_ends() {
    // `stopped` is the call that sees its own deadline. `dropped` is the
    // call a caller gives up on first, as `LimitedJudge` does.
    for dropped in [false, true] {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let fake = runtime.block_on(async {
            let (fake, judge) = warmed_up(
                CliKind::Codex,
                "sleep 30 &\necho $! > \"$dir/helper\"\necho $$ > \"$dir/pid\"\nwait",
            )
            .await;
            let q = question();
            let call = judge.judge(&q, Instant::now() + Duration::from_millis(1500));
            if dropped {
                let wait = async {
                    fake.pid("helper").await;
                    fake.pid("pid").await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                };
                tokio::select! {
                    _ = call => panic!("the call ended on its own"),
                    () = wait => {}
                }
            } else {
                let error = call.await.expect_err("no answer");
                assert_eq!(error.kind(), JudgeErrorKind::Deadline);
            }
            fake
        });
        drop(runtime);
        let read = |name: &str| -> u32 { fake.read(name).trim().parse().expect("a process id") };
        let (pid, helper) = (read("pid"), read("helper"));
        // The signal was sent before the call returned. The system needs a
        // moment to take the process away, and nothing else is running
        // that could still send it.
        let wait_gone = |pid: u32| {
            for _ in 0..200 {
                if !alive(pid) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        assert!(wait_gone(pid), "dropped={dropped}: the CLI is still there");
        let helper_gone = wait_gone(helper);
        if !helper_gone {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(helper.to_string())
                .status();
        }
        assert!(helper_gone, "dropped={dropped}: the helper is still there");
        assert!(!call_dir(&fake).exists());
    }
}

#[tokio::test]
async fn a_dropped_call_kills_the_cli() {
    for kind in KINDS {
        let fake = Fake::new("echo $$ > \"$dir/pid\"\nexec sleep 30");
        let judge = std::sync::Arc::new(fake.judge(kind));
        let task = tokio::spawn({
            let judge = judge.clone();
            async move { judge.judge(&question(), soon()).await }
        });
        let pid = fake.pid("pid").await;
        assert!(alive(pid));
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        gone(pid).await;
    }
}

#[tokio::test]
async fn a_program_that_is_not_there_is_reported() {
    for kind in KINDS {
        for program in ["/no/such/dir/fake-cli", "no-such-judge-cli-on-the-path"] {
            let judge = CliJudge::new(CliConfig {
                program: Some(PathBuf::from(program)),
                ..CliConfig::new(kind)
            })
            .expect("a judge");
            assert!(!judge.is_installed());
            assert_eq!(judge.program(), None);
            let error = judge
                .judge(&question(), soon())
                .await
                .expect_err("no program");
            assert_eq!(error.kind(), JudgeErrorKind::Transport);
            assert_eq!(
                error.message(),
                format!("{} CLI not found", kind.default_program())
            );
        }
    }
}

#[tokio::test]
async fn a_program_that_appears_later_is_used() {
    let fake = Fake::new(&good(CliKind::Codex));
    let hidden = fake.dir.join("hidden");
    std::fs::rename(&fake.program, &hidden).expect("moved away");
    let judge = fake.judge(CliKind::Codex);
    assert!(!judge.is_installed());
    judge
        .judge(&question(), soon())
        .await
        .expect_err("not there yet");
    std::fs::rename(&hidden, &fake.program).expect("moved back");
    assert!(judge.is_installed());
    assert_eq!(judge.program().as_deref(), Some(Path::new(&fake.program)));
    judge.judge(&question(), soon()).await.expect("a verdict");
}

#[tokio::test]
async fn the_model_and_the_effort_reach_the_cli() {
    for (kind, flag) in [(CliKind::ClaudeCode, "--model"), (CliKind::Codex, "-m")] {
        let fake = Fake::new(&good(kind));
        let judge = fake.judge_with(CliConfig {
            model: Some("some-model-1".to_owned()),
            effort: assessment_judge::Effort::High,
            ..CliConfig::new(kind)
        });
        assert_eq!(judge.id(), format!("{}/some-model-1", kind.provider()));
        let verdict = judge.judge(&question(), soon()).await.expect("a verdict");
        assert_eq!(verdict.source, judge.id());
        let argv = fake.read("argv");
        assert!(argv.contains(&format!("{flag}\nsome-model-1\n")), "{argv}");
        assert!(argv.contains("high"), "{argv}");
    }
}
