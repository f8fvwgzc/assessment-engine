//! Judges that run an agent command line as a child process.
//!
//! Two programs are known: the Claude Code CLI (`claude`) and the Codex CLI
//! (`codex`). Each call starts the program once, in non-interactive mode,
//! and uses the login the program already has. No API key is read here.
//!
//! The rules of this module:
//!
//! - The texts of a question go to the child on stdin only. They are never
//!   an argument and no shell is involved.
//! - The child runs in an empty private directory, never in the directory
//!   of the caller. Each call makes its own and removes it.
//! - The child gets a short list of environment variables, not the
//!   environment of the caller.
//! - The child and what it started are killed when the deadline passes or
//!   the call is dropped. The kill does not wait for a later task.
//! - An error message is a fixed text. What the program printed decides
//!   which text, but is never copied into an error or a log.
//! - A call is never tried again.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::prompt::{Answer, SYSTEM_PROMPT, schema, single_prompt, user_content};
use crate::{
    BoxFuture, Effort, Judge, JudgeError, JudgeErrorKind, Question, Usage, Verdict, random_u64,
    valid_model,
};

/// The most bytes read from the child's stdout, its stderr or its answer
/// file, unless the configuration says otherwise.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// How much of the child's error text is looked at to name the failure.
const CLASSIFY_BYTES: usize = 16 * 1024;

/// Which agent command line a [`CliJudge`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CliKind {
    /// The Claude Code CLI, `claude -p`. The texts go to Anthropic under
    /// the account the CLI is logged in to.
    ClaudeCode,
    /// The Codex CLI, `codex exec`. The texts go to OpenAI under the
    /// account the CLI is logged in to.
    Codex,
}

impl CliKind {
    /// The provider name, the first part of the judge id: `claude-cli` or
    /// `codex-cli`.
    pub fn provider(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude-cli",
            CliKind::Codex => "codex-cli",
        }
    }

    /// The program that is run when none is configured, found through
    /// `PATH`.
    pub fn default_program(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude",
            CliKind::Codex => "codex",
        }
    }
}

/// How a [`CliJudge`] runs its program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    pub kind: CliKind,
    /// The program. A bare name is looked up in `PATH`. `None` is the
    /// default program of the kind.
    pub program: Option<PathBuf>,
    /// The model. `None` leaves the choice to the program.
    pub model: Option<String>,
    /// `low`, `medium` or `high`. The command lines take no other value.
    pub effort: Effort,
    /// The cap on the child's stdout, its stderr and its answer file.
    pub max_output_bytes: usize,
    /// Codex only: do not load the user's `config.toml`, so a model or a
    /// tool that is configured there does not reach the judge.
    pub ignore_user_config: bool,
}

impl CliConfig {
    /// The defaults of `kind`: the default program, the program's own
    /// model, low effort and a cap of 1 MiB.
    pub fn new(kind: CliKind) -> Self {
        Self {
            kind,
            program: None,
            model: None,
            effort: Effort::Low,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            ignore_user_config: true,
        }
    }
}

/// Finds a program the way a shell would, without starting it. A bare name
/// is looked up in `PATH`. A path with a directory in it is checked as it
/// is. The result is an absolute path.
pub fn find_program(program: &Path) -> Option<PathBuf> {
    if program.as_os_str().is_empty() {
        return None;
    }
    if program.components().count() > 1 || program.is_absolute() {
        let path = std::path::absolute(program).ok()?;
        return is_executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .filter_map(|dir| std::path::absolute(dir.join(program)).ok())
        .find(|path| is_executable(path))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The environment variables every child gets, when they are set. No
/// other variable of the engine reaches the child: no API key, no token
/// and no variable that points a program at another endpoint or account.
pub const PASSED_ENV: &[&str] = &[
    // Where the program, its runtime and its login are found.
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    // Language and time.
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    // How the network is reached.
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    // The same roles on Windows.
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERNAME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "TEMP",
    "TMP",
];

/// A judge that asks an agent command line.
///
/// Each call makes a private temporary directory. The child runs in an
/// empty directory inside it, and the schema file and the answer file lie
/// next to that. The directory is removed when the call ends, so nothing
/// has to survive in the temporary directory between calls.
#[derive(Debug)]
pub struct CliJudge {
    kind: CliKind,
    id: String,
    given: PathBuf,
    resolved: Option<PathBuf>,
    model: Option<String>,
    effort: Effort,
    max_output_bytes: usize,
    ignore_user_config: bool,
    schema_text: String,
}

impl CliJudge {
    /// Checks the configuration. No process is started and no file is
    /// written. A program that is not installed is not an error
    /// here: see [`CliJudge::is_installed`].
    pub fn new(config: CliConfig) -> Result<Self, JudgeError> {
        if let Some(model) = &config.model
            && !valid_model(model)
        {
            return Err(config_error(
                "the judge model name may hold only letters, digits, '-', '.' and '_'",
            ));
        }
        if !matches!(config.effort, Effort::Low | Effort::Medium | Effort::High) {
            return Err(config_error(
                "a CLI judge takes the effort low, medium or high",
            ));
        }
        if config.max_output_bytes == 0 {
            return Err(config_error(
                "the judge output limit must be at least 1 byte",
            ));
        }
        let given = config
            .program
            .unwrap_or_else(|| PathBuf::from(config.kind.default_program()));
        if given.as_os_str().is_empty() {
            return Err(config_error("the judge CLI path is empty"));
        }
        let resolved = find_program(&given);

        let schema_text = schema().to_string();
        let id = format!(
            "{}/{}",
            config.kind.provider(),
            config.model.as_deref().unwrap_or("default")
        );
        Ok(Self {
            kind: config.kind,
            id,
            given,
            resolved,
            model: config.model,
            effort: config.effort,
            max_output_bytes: config.max_output_bytes,
            ignore_user_config: config.ignore_user_config,
            schema_text,
        })
    }

    pub fn kind(&self) -> CliKind {
        self.kind
    }

    /// Whether the program can be found now. It is not started.
    pub fn is_installed(&self) -> bool {
        self.program().is_some()
    }

    /// The absolute path of the program, when it can be found.
    pub fn program(&self) -> Option<PathBuf> {
        self.resolved.clone().or_else(|| find_program(&self.given))
    }

    /// The environment variable that names the place of the program's own
    /// configuration and login. It is passed on with [`PASSED_ENV`].
    fn home_env(&self) -> &'static str {
        match self.kind {
            CliKind::ClaudeCode => "CLAUDE_CONFIG_DIR",
            CliKind::Codex => "CODEX_HOME",
        }
    }

    /// The command of one call. The texts are not in it: they go to stdin.
    fn command(&self, program: &Path, dir: &CallDir) -> Command {
        let mut command = Command::new(program);
        match self.kind {
            CliKind::ClaudeCode => {
                command.arg("-p");
                if let Some(model) = &self.model {
                    command.arg("--model").arg(model);
                }
                command
                    .arg("--effort")
                    .arg(self.effort.as_str())
                    .args(["--output-format", "json"])
                    .arg("--json-schema")
                    .arg(&self.schema_text)
                    .arg("--system-prompt")
                    .arg(SYSTEM_PROMPT)
                    // No tools, no MCP servers, no settings, no skills and
                    // no saved session.
                    .args(["--tools", ""])
                    .arg("--strict-mcp-config")
                    .args(["--setting-sources", ""])
                    .arg("--disable-slash-commands")
                    .arg("--no-session-persistence");
            }
            CliKind::Codex => {
                command.arg("exec");
                if self.ignore_user_config {
                    command.arg("--ignore-user-config");
                }
                command
                    .args(["--sandbox", "read-only"])
                    .arg("--skip-git-repo-check")
                    .arg("--ephemeral")
                    .arg("--ignore-rules")
                    .arg("--output-schema")
                    .arg(dir.schema_path())
                    .arg("-o")
                    .arg(dir.answer_path())
                    .arg("--json")
                    .arg("-c")
                    .arg(format!("model_reasoning_effort={}", self.effort.as_str()))
                    // A command the agent runs in its sandbox gets no
                    // variable of this process either.
                    .args(["-c", "shell_environment_policy.inherit=none"]);
                if let Some(model) = &self.model {
                    command.arg("-m").arg(model);
                }
                // The prompt is read from stdin.
                command.arg("-");
            }
        }
        // The child starts from an empty environment. With a key, a token
        // or an endpoint variable the program would use another account or
        // another server than its login, and the agent could be talked
        // into repeating a secret it finds there.
        command.env_clear();
        for name in PASSED_ENV.iter().copied().chain([self.home_env()]) {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .current_dir(dir.work_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A group of its own, so the helpers the program starts can be
        // stopped with it.
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    async fn run(&self, question: &Question, deadline: Instant) -> Result<Verdict, JudgeError> {
        if Instant::now() >= deadline {
            return Err(deadline_error());
        }
        let Some(program) = self.program() else {
            return Err(self.transport("CLI not found"));
        };
        // Declared before the child, so it is removed after the child has
        // been stopped.
        let dir = CallDir::new(&self.schema_text)
            .map_err(|_| config_error("could not make a private directory for the judge CLI"))?;
        let user = user_content(question);
        let input = match self.kind {
            CliKind::ClaudeCode => user,
            CliKind::Codex => single_prompt(&user),
        };

        let child =
            spawn(&mut self.command(&program, &dir)).map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => self.transport("CLI not found"),
                _ => self.transport("CLI could not be started"),
            })?;
        // From here on, leaving this function in any way stops the child
        // and its process group: here when the call ends, or in the drop of
        // `running` when the call is dropped while it waits.
        let mut running = Running::new(child);
        let talked = tokio::time::timeout_at(
            deadline,
            talk(running.child(), input.into_bytes(), self.max_output_bytes),
        )
        .await;
        // Whatever happened, nothing of the child is left when this
        // returns: a child that hangs, and a helper a finished child left
        // behind.
        running.stop().await;
        let output = match talked {
            Err(_) => return Err(deadline_error()),
            Ok(Err(Broken::TooLarge)) => return Err(self.too_large()),
            Ok(Err(Broken::Io)) => return Err(self.transport("CLI output could not be read")),
            Ok(Ok(output)) => output,
        };

        match self.kind {
            CliKind::ClaudeCode => self.read_claude(&output),
            CliKind::Codex => {
                let answer = read_file(&dir.answer_path(), self.max_output_bytes).await;
                self.read_codex(&output, answer)
            }
        }
    }

    /// Reads the one JSON object the Claude CLI prints.
    fn read_claude(&self, output: &Output) -> Result<Verdict, JudgeError> {
        let printed = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(text.trim()).ok())
            .filter(Value::is_object);
        let Some(printed) = printed else {
            return Err(self.failure(output.status, &[lossy(&output.stderr)], false));
        };
        let usage = claude_usage(&printed);
        let failed = printed.get("is_error").and_then(Value::as_bool) == Some(true);
        if failed || !output.status.success() {
            let said = printed.get("result").and_then(Value::as_str).unwrap_or("");
            return Err(self
                .failure(output.status, &[cut(said), lossy(&output.stderr)], failed)
                .with_usage(usage));
        }
        let answer = printed
            .get("structured_output")
            .filter(|value| !value.is_null())
            .cloned()
            .and_then(Answer::from_value)
            .or_else(|| {
                printed
                    .get("result")
                    .and_then(Value::as_str)
                    .and_then(Answer::parse)
            });
        match answer {
            Some(answer) => Ok(answer.into_verdict(self.id.as_str()).with_usage(usage)),
            None => Err(invalid_answer().with_usage(usage)),
        }
    }

    /// Reads the event lines the Codex CLI prints and its answer file.
    fn read_codex(
        &self,
        output: &Output,
        answer: Result<Option<Vec<u8>>, Broken>,
    ) -> Result<Verdict, JudgeError> {
        let events = codex_events(&output.stdout);
        if output.status.success() {
            let answer = match answer {
                Err(Broken::TooLarge) => return Err(self.too_large().with_usage(events.usage)),
                Err(Broken::Io) | Ok(None) => None,
                Ok(Some(bytes)) => std::str::from_utf8(&bytes).ok().and_then(Answer::parse),
            };
            if let Some(answer) = answer {
                return Ok(answer
                    .into_verdict(self.id.as_str())
                    .with_usage(events.usage));
            }
        }
        let failed = !events.errors.is_empty();
        if !failed && output.status.success() {
            return Err(invalid_answer().with_usage(events.usage));
        }
        let mut said = events.errors;
        said.push(lossy(&output.stderr));
        Err(self
            .failure(output.status, &said, failed)
            .with_usage(events.usage))
    }

    /// The error of a call the program did not answer. `said` is what the
    /// program printed about it. It only picks the message.
    fn failure(&self, status: ExitStatus, said: &[String], reported: bool) -> JudgeError {
        match classify(said) {
            Some(Trouble::NotLoggedIn) => {
                JudgeError::new(JudgeErrorKind::Api, self.message("CLI is not logged in"))
                    .persistent()
            }
            Some(Trouble::UsageLimit) => {
                JudgeError::new(JudgeErrorKind::Api, self.message("CLI hit a usage limit"))
                    .persistent()
            }
            None if reported => self.transport("CLI reported an error"),
            None => match status.code() {
                Some(0) => invalid_answer(),
                Some(code) => self.transport(&format!("CLI exited with status {code}")),
                None => self.transport("CLI was stopped by a signal"),
            },
        }
    }

    fn name(&self) -> &'static str {
        self.kind.default_program()
    }

    fn message(&self, what: &str) -> String {
        format!("{} {what}", self.name())
    }

    fn transport(&self, what: &str) -> JudgeError {
        JudgeError::new(JudgeErrorKind::Transport, self.message(what))
    }

    fn too_large(&self) -> JudgeError {
        JudgeError::new(
            JudgeErrorKind::InvalidOutput,
            format!(
                "the {} CLI wrote more than {} bytes",
                self.name(),
                self.max_output_bytes
            ),
        )
    }
}

impl Judge for CliJudge {
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

/// Starts a child, one at a time.
///
/// Where a pipe cannot be made close-on-exec in one step (macOS), a child
/// that is started at the same moment on another thread can inherit the
/// stdin pipe of this one. This child would then wait for the end of its
/// input until the other child ends. Starting takes well under a
/// millisecond, so the lock costs nothing.
fn spawn(command: &mut Command) -> std::io::Result<Child> {
    let _one = lock_starting();
    command.spawn()
}

fn lock_starting() -> std::sync::MutexGuard<'static, ()> {
    static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    STARTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn config_error(message: &str) -> JudgeError {
    JudgeError::new(JudgeErrorKind::Transport, message)
}

fn deadline_error() -> JudgeError {
    JudgeError::new(JudgeErrorKind::Deadline, "judge timed out")
}

fn invalid_answer() -> JudgeError {
    JudgeError::new(
        JudgeErrorKind::InvalidOutput,
        "the answer is not a valid verdict",
    )
}

/// Makes a new directory only this user can enter, under the system's
/// temporary directory.
fn private_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let mut last = None;
    for _ in 0..8 {
        let path = base.join(format!(
            "assessment-judge-{}-{:016x}",
            std::process::id(),
            random_u64()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Not recursive: a path that exists already is never reused.
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free name")))
}

/// The private directory of one call. It is removed when the call ends,
/// in whatever way.
///
/// A directory that lived as long as the judge would lie in the system's
/// temporary directory for days, where a cleaner may remove it.
struct CallDir {
    root: PathBuf,
}

impl CallDir {
    /// Makes the directory, the empty directory the child runs in and the
    /// schema file.
    fn new(schema_text: &str) -> std::io::Result<Self> {
        let dir = Self {
            root: private_dir()?,
        };
        std::fs::create_dir(dir.work_dir())?;
        std::fs::write(dir.schema_path(), schema_text.as_bytes())?;
        Ok(dir)
    }

    /// The empty directory the child runs in.
    fn work_dir(&self) -> PathBuf {
        self.root.join("cwd")
    }

    fn schema_path(&self) -> PathBuf {
        self.root.join("schema.json")
    }

    /// Where the Codex CLI writes its last message.
    fn answer_path(&self) -> PathBuf {
        self.root.join("answer.json")
    }
}

impl Drop for CallDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A child that is stopped, with its process group, when the call ends.
///
/// [`Running::stop`] is the normal way. The drop is for a call that is
/// dropped while it waits: it sends the signals before it returns and
/// leaves nothing to a task, because the runtime may be about to end.
struct Running {
    child: Child,
    stopped: bool,
    #[cfg(unix)]
    group: Option<u32>,
}

impl Running {
    fn new(child: Child) -> Self {
        Self {
            #[cfg(unix)]
            group: child.id(),
            child,
            stopped: false,
        }
    }

    fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Kills the process group and the child, and waits for the child. A
    /// child that has ended already is not an error.
    async fn stop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            kill_group(group).await;
        }
        let _ = self.child.kill().await;
        self.stopped = true;
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = self.child.start_kill();
        #[cfg(unix)]
        if let Some(group) = self.group {
            kill_group_now(group);
        }
        // `kill_on_drop` lets the runtime reap the child later.
    }
}

/// The `kill` programs that are tried, in this order.
#[cfg(unix)]
const KILL_PROGRAMS: [&str; 2] = ["/bin/kill", "/usr/bin/kill"];

#[cfg(unix)]
fn kill_group_args(group: u32) -> [String; 3] {
    // `--` ends the options: without it the `kill` of procps reads the
    // negative number as an option and reports a failure.
    ["-KILL".to_owned(), "--".to_owned(), format!("-{group}")]
}

/// Stops what is left of the child's process group, such as a helper the
/// program started. `kill` is run as a program because this crate has no
/// unsafe code. A group that is gone already is not an error.
#[cfg(unix)]
async fn kill_group(group: u32) {
    for program in KILL_PROGRAMS {
        let mut command = Command::new(program);
        command
            .args(kill_group_args(group))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut kill) = spawn(&mut command) {
            let _ = kill.wait().await;
            return;
        }
    }
}

/// As [`kill_group`], without a runtime: it returns when the signal has
/// been sent. It blocks for the few milliseconds `kill` runs.
#[cfg(unix)]
fn kill_group_now(group: u32) {
    for program in KILL_PROGRAMS {
        let mut command = std::process::Command::new(program);
        command
            .args(kill_group_args(group))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let started = {
            let _one = lock_starting();
            command.spawn()
        };
        if let Ok(mut kill) = started {
            let _ = kill.wait();
            return;
        }
    }
}

/// What a finished child left.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Why the child's output could not be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    TooLarge,
    Io,
}

/// Writes the input, reads both outputs and waits for the end.
async fn talk(child: &mut Child, input: Vec<u8>, cap: usize) -> Result<Output, Broken> {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or(Broken::Io)?;
    let stderr = child.stderr.take().ok_or(Broken::Io)?;
    let write = async move {
        if let Some(mut stdin) = stdin {
            // A child that ends before it has read everything is judged by
            // its output, not by this write.
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        }
        Ok::<(), Broken>(())
    };
    let ((), stdout, stderr) =
        tokio::try_join!(write, read_capped(stdout, cap), read_start(stderr, cap))?;
    let status = child.wait().await.map_err(|_| Broken::Io)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Reads everything, or fails when it is more than `cap` bytes.
async fn read_capped(reader: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, Broken> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Broken::Io)?;
    if bytes.len() > cap {
        return Err(Broken::TooLarge);
    }
    Ok(bytes)
}

/// Keeps the first `cap` bytes and reads the rest away, so a child that
/// writes a lot of diagnostics is not blocked.
async fn read_start(mut reader: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, Broken> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX);
    (&mut reader)
        .take(limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Broken::Io)?;
    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    Ok(bytes)
}

/// Reads a file of at most `cap` bytes. `None` when there is no file.
async fn read_file(path: &Path, cap: usize) -> Result<Option<Vec<u8>>, Broken> {
    match tokio::fs::File::open(path).await {
        Ok(file) => read_capped(file, cap).await.map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Broken::Io),
    }
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The usage the Claude CLI reports. It has the names of the API.
fn claude_usage(printed: &Value) -> Usage {
    let Some(usage) = printed.get("usage") else {
        return Usage::default();
    };
    Usage {
        input_tokens: count(usage, "input_tokens"),
        output_tokens: count(usage, "output_tokens"),
        cache_creation_input_tokens: count(usage, "cache_creation_input_tokens"),
        cache_read_input_tokens: count(usage, "cache_read_input_tokens"),
    }
}

/// What the event lines of the Codex CLI say.
#[derive(Debug, Default, PartialEq)]
struct CodexEvents {
    usage: Usage,
    errors: Vec<String>,
}

/// Reads the JSON lines of `codex exec --json`. A line that is not an
/// event is skipped.
fn codex_events(stdout: &[u8]) -> CodexEvents {
    let mut events = CodexEvents::default();
    for line in stdout.split(|byte| *byte == b'\n') {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("turn.completed") => {
                if let Some(usage) = event.get("usage") {
                    events.usage = events.usage.plus(Usage {
                        input_tokens: count(usage, "input_tokens"),
                        output_tokens: count(usage, "output_tokens"),
                        ..Usage::default()
                    });
                }
            }
            Some("error") => {
                let message = event.get("message").and_then(Value::as_str);
                events.errors.push(cut(message.unwrap_or("")));
            }
            Some("turn.failed") => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str);
                events.errors.push(cut(message.unwrap_or("")));
            }
            _ => {}
        }
    }
    events
}

/// A failure that will not go away by asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    NotLoggedIn,
    UsageLimit,
}

const LOGIN_SIGNS: [&str; 12] = [
    "not logged in",
    "not authenticated",
    "failed to authenticate",
    "authentication",
    "unauthorized",
    "oauth",
    "/login",
    "codex login",
    "please log in",
    "please login",
    "sign in",
    "invalid api key",
];

const LIMIT_SIGNS: [&str; 7] = [
    "usage limit",
    "rate limit",
    "rate_limit",
    "quota",
    "too many requests",
    "hit your limit",
    "limit reached",
];

/// Names the failure from what the program printed. The text is only
/// searched, never kept.
fn classify(said: &[String]) -> Option<Trouble> {
    let text = said
        .iter()
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    if LOGIN_SIGNS.iter().any(|sign| text.contains(sign)) {
        Some(Trouble::NotLoggedIn)
    } else if LIMIT_SIGNS.iter().any(|sign| text.contains(sign)) {
        Some(Trouble::UsageLimit)
    } else {
        None
    }
}

/// The start of `bytes` as text, for [`classify`].
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(CLASSIFY_BYTES)]).into_owned()
}

/// The start of `text`, for [`classify`].
fn cut(text: &str) -> String {
    let mut text = text.to_owned();
    crate::truncate_bytes(&mut text, CLASSIFY_BYTES);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use assessment_core::model::TextRelation;

    fn judge(kind: CliKind) -> CliJudge {
        CliJudge::new(CliConfig::new(kind)).expect("a judge")
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The status of a program that ended with `code`. Each system has
    /// its own way to make one by hand.
    #[cfg(unix)]
    fn status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(u32::try_from(code).expect("a small exit code"))
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    const ANSWER: &str = r#"{"relation":"replaced","confidence":0.75,"rationale":"Other work."}"#;

    #[test]
    fn the_id_names_the_provider_and_the_model() {
        assert_eq!(judge(CliKind::ClaudeCode).id(), "claude-cli/default");
        assert_eq!(judge(CliKind::Codex).id(), "codex-cli/default");
        let config = CliConfig {
            model: Some("claude-opus-5-5".to_owned()),
            ..CliConfig::new(CliKind::ClaudeCode)
        };
        let judge = CliJudge::new(config).expect("a judge");
        assert_eq!(judge.id(), "claude-cli/claude-opus-5-5");
        assert!(crate::price::is_subscription(judge.id()));
    }

    #[test]
    fn a_bad_configuration_is_refused() {
        for model in ["", "-m", "a b", "x;y", "$(id)", "a/b"] {
            let config = CliConfig {
                model: Some(model.to_owned()),
                ..CliConfig::new(CliKind::Codex)
            };
            assert!(CliJudge::new(config).is_err(), "{model}");
        }
        for effort in [Effort::Xhigh, Effort::Max] {
            let config = CliConfig {
                effort,
                ..CliConfig::new(CliKind::ClaudeCode)
            };
            assert!(CliJudge::new(config).is_err(), "{effort:?}");
        }
        let config = CliConfig {
            max_output_bytes: 0,
            ..CliConfig::new(CliKind::Codex)
        };
        assert!(CliJudge::new(config).is_err());
        let config = CliConfig {
            program: Some(PathBuf::new()),
            ..CliConfig::new(CliKind::Codex)
        };
        assert!(CliJudge::new(config).is_err());
    }

    #[test]
    fn the_private_directory_is_empty_and_goes_away() {
        let dir = CallDir::new("{}").expect("a directory");
        let root = dir.root.clone();
        assert!(root.starts_with(std::env::temp_dir()));
        assert_eq!(dir.work_dir().parent(), Some(root.as_path()));
        assert_eq!(
            std::fs::read_dir(dir.work_dir()).expect("readable").count(),
            0,
            "the child's directory is empty"
        );
        assert_eq!(
            std::fs::read_to_string(dir.schema_path()).expect("a schema"),
            "{}"
        );
        assert!(!dir.answer_path().exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&root)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let other = CallDir::new("{}").expect("a directory");
        assert_ne!(other.root, root);
        // An answer file that was written goes away with the rest.
        std::fs::write(dir.answer_path(), "x").expect("written");
        drop(dir);
        assert!(!root.exists());
    }

    #[test]
    fn the_claude_command_has_no_tools_and_no_settings() {
        let config = CliConfig {
            model: Some("claude-opus-5-5".to_owned()),
            effort: Effort::Medium,
            ..CliConfig::new(CliKind::ClaudeCode)
        };
        let judge = CliJudge::new(config).expect("a judge");
        let dir = CallDir::new(&judge.schema_text).expect("a directory");
        let command = judge.command(Path::new("/bin/claude"), &dir);
        let schema = schema().to_string();
        assert_eq!(
            args(&command),
            [
                "-p",
                "--model",
                "claude-opus-5-5",
                "--effort",
                "medium",
                "--output-format",
                "json",
                "--json-schema",
                schema.as_str(),
                "--system-prompt",
                SYSTEM_PROMPT,
                "--tools",
                "",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--disable-slash-commands",
                "--no-session-persistence",
            ]
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(dir.work_dir().as_path())
        );
        assert_eq!(command.as_std().get_program(), "/bin/claude");
    }

    #[test]
    fn the_codex_command_is_read_only_and_ephemeral() {
        let judge = judge(CliKind::Codex);
        let dir = CallDir::new(&judge.schema_text).expect("a directory");
        let command = judge.command(Path::new("/bin/codex"), &dir);
        let schema_path = dir.schema_path().to_string_lossy().into_owned();
        let answer_path = dir.answer_path().to_string_lossy().into_owned();
        assert_eq!(
            args(&command),
            [
                "exec",
                "--ignore-user-config",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--ephemeral",
                "--ignore-rules",
                "--output-schema",
                schema_path.as_str(),
                "-o",
                answer_path.as_str(),
                "--json",
                "-c",
                "model_reasoning_effort=low",
                "-c",
                "shell_environment_policy.inherit=none",
                "-",
            ]
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(dir.work_dir().as_path())
        );
        assert_eq!(
            std::fs::read_to_string(dir.schema_path()).ok(),
            Some(schema().to_string())
        );

        let config = CliConfig {
            model: Some("some-model".to_owned()),
            effort: Effort::High,
            ignore_user_config: false,
            ..CliConfig::new(CliKind::Codex)
        };
        let judge = CliJudge::new(config).expect("a judge");
        let args = args(&judge.command(Path::new("/bin/codex"), &dir));
        assert!(!args.contains(&"--ignore-user-config".to_owned()));
        assert!(args.contains(&"model_reasoning_effort=high".to_owned()));
        assert_eq!(args[args.len() - 3..], ["-m", "some-model", "-"]);
    }

    #[test]
    fn the_child_gets_a_short_list_of_variables_and_nothing_else() {
        // Names that would change the account or the endpoint, and secrets
        // of the engine. None is ever on the list.
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "AWS_SECRET_ACCESS_KEY",
            "ASSESSMENT_AUTH_TOKEN",
            "CARGO_MANIFEST_DIR",
        ] {
            assert!(!PASSED_ENV.contains(&name), "{name}");
        }
        for (kind, own, other) in [
            (CliKind::ClaudeCode, "CLAUDE_CONFIG_DIR", "CODEX_HOME"),
            (CliKind::Codex, "CODEX_HOME", "CLAUDE_CONFIG_DIR"),
        ] {
            let judge = judge(kind);
            let dir = CallDir::new(&judge.schema_text).expect("a directory");
            let command = judge.command(Path::new("/bin/x"), &dir);
            // Every variable the command names is on the list and has the
            // value of this process. Nothing is inherited beyond them:
            // `env_clear` is on, which `get_envs` shows as no removals.
            let mut passed = 0;
            for (name, value) in command.as_std().get_envs() {
                let name = name.to_str().expect("a plain name");
                assert!(PASSED_ENV.contains(&name) || name == own, "{name}");
                assert_ne!(name, other);
                assert_eq!(value, std::env::var_os(name).as_deref(), "{name}");
                assert!(value.is_some(), "{name} is set, not removed");
                passed += 1;
            }
            let expected = PASSED_ENV
                .iter()
                .copied()
                .chain([own])
                .filter(|name| std::env::var_os(name).is_some())
                .count();
            assert_eq!(passed, expected);
            assert!(expected > 0, "PATH at least is set where the tests run");
            assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        }
    }

    #[test]
    fn a_claude_answer_is_read_with_its_usage() {
        let judge = judge(CliKind::ClaudeCode);
        let printed = format!(
            r#"{{"type":"result","is_error":false,"result":"","structured_output":{ANSWER},"usage":{{"input_tokens":12,"output_tokens":34,"cache_creation_input_tokens":5,"cache_read_input_tokens":6}},"total_cost_usd":0.5}}"#
        );
        let verdict = judge
            .read_claude(&output(0, &printed, ""))
            .expect("a verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);
        assert_eq!(verdict.confidence, 0.75);
        assert_eq!(verdict.source, "claude-cli/default");
        assert_eq!(
            verdict.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 34,
                cache_creation_input_tokens: 5,
                cache_read_input_tokens: 6,
            }
        );
        assert_eq!(crate::price::cost_usd(&verdict.source, &verdict.usage), 0.0);

        // Without structured output the result text is read.
        let printed = serde_json::json!({"type": "result", "is_error": false, "result": ANSWER});
        let verdict = judge
            .read_claude(&output(0, &printed.to_string(), ""))
            .expect("a verdict");
        assert_eq!(verdict.relation, TextRelation::Replaced);
    }

    #[test]
    fn a_claude_failure_has_a_fixed_message() {
        let judge = judge(CliKind::ClaudeCode);
        let cases = [
            (
                1,
                r#"{"type":"result","is_error":true,"result":"Failed to authenticate: OAuth session expired and could not be refreshed"}"#,
                "",
                "claude CLI is not logged in",
            ),
            (
                0,
                r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#,
                "",
                "claude CLI is not logged in",
            ),
            (
                1,
                r#"{"type":"result","is_error":true,"result":"You have hit your limit for this week"}"#,
                "",
                "claude CLI hit a usage limit",
            ),
            (
                1,
                r#"{"type":"result","is_error":true,"result":"SECRET-TEXT went wrong"}"#,
                "",
                "claude CLI reported an error",
            ),
            (
                2,
                "",
                "SECRET-TEXT: unknown option",
                "claude CLI exited with status 2",
            ),
            (
                1,
                "not json SECRET-TEXT",
                "",
                "claude CLI exited with status 1",
            ),
            (
                0,
                "not json SECRET-TEXT",
                "",
                "the answer is not a valid verdict",
            ),
            (0, "[1, 2]", "", "the answer is not a valid verdict"),
            (
                0,
                r#"{"type":"result","is_error":false,"result":"SECRET-TEXT"}"#,
                "",
                "the answer is not a valid verdict",
            ),
            (
                0,
                r#"{"type":"result","is_error":false,"structured_output":{"relation":"replaced","confidence":0.5,"rationale":"x","more":1}}"#,
                "",
                "the answer is not a valid verdict",
            ),
            (
                0,
                r#"{"type":"result","is_error":false,"structured_output":{"relation":"replaced","confidence":7,"rationale":"x"}}"#,
                "",
                "the answer is not a valid verdict",
            ),
        ];
        for (code, stdout, stderr, message) in cases {
            let error = judge
                .read_claude(&output(code, stdout, stderr))
                .expect_err("a failure");
            assert_eq!(error.message(), message, "{stdout}");
            assert!(!format!("{error:?}").contains("SECRET-TEXT"));
        }
    }

    #[test]
    fn the_kind_of_a_failure_drives_the_breaker() {
        let judge = judge(CliKind::ClaudeCode);
        let login = judge.failure(status(1), &["Please run /login".to_owned()], true);
        assert_eq!(login.kind(), JudgeErrorKind::Api);
        assert!(login.is_persistent());
        assert!(!login.is_retryable());
        let limit = judge.failure(status(1), &["usage limit reached".to_owned()], true);
        assert!(limit.is_persistent());
        let other = judge.failure(status(1), &["boom".to_owned()], true);
        assert_eq!(other.kind(), JudgeErrorKind::Transport);
        assert!(!other.is_persistent());
        let exit = judge.failure(status(3), &[], false);
        assert_eq!(exit.kind(), JudgeErrorKind::Transport);
        assert_eq!(exit.message(), "claude CLI exited with status 3");
    }

    #[test]
    fn codex_events_give_the_usage_and_the_errors() {
        let stdout = concat!(
            r#"{"type":"thread.started","thread_id":"t"}"#,
            "\n",
            "not json\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}"#,
            "\n",
            r#"{"type":"error","message":"first"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"second"}}"#,
            "\n",
        );
        let events = codex_events(stdout.as_bytes());
        assert_eq!(events.usage.input_tokens, 101);
        assert_eq!(events.usage.output_tokens, 22);
        assert_eq!(events.errors, ["first", "second"]);
    }

    #[test]
    fn a_codex_answer_is_read_from_the_answer_file() {
        let judge = judge(CliKind::Codex);
        let stdout = r#"{"type":"turn.completed","usage":{"input_tokens":9,"output_tokens":3}}"#;
        let verdict = judge
            .read_codex(
                &output(0, stdout, "noise"),
                Ok(Some(ANSWER.as_bytes().to_vec())),
            )
            .expect("a verdict");
        assert_eq!(verdict.source, "codex-cli/default");
        assert_eq!(verdict.usage.input_tokens, 9);
        assert_eq!(verdict.usage.output_tokens, 3);

        // An error event that the program got over does not spoil a good
        // answer.
        let stdout = r#"{"type":"error","message":"Reconnecting... 1/5"}"#;
        judge
            .read_codex(&output(0, stdout, ""), Ok(Some(ANSWER.as_bytes().to_vec())))
            .expect("a verdict");
    }

    #[test]
    fn a_codex_failure_has_a_fixed_message() {
        let judge = judge(CliKind::Codex);
        let good = || Ok(Some(ANSWER.as_bytes().to_vec()));
        let bad = |text: &str| Ok(Some(text.as_bytes().to_vec()));
        let cases = [
            (0, "", "", Ok(None), "the answer is not a valid verdict"),
            (
                0,
                "",
                "",
                bad("SECRET-TEXT"),
                "the answer is not a valid verdict",
            ),
            (
                0,
                "",
                "",
                bad(r#"{"relation":"replaced","confidence":0.5,"rationale":"x","more":1}"#),
                "the answer is not a valid verdict",
            ),
            (
                0,
                "",
                "",
                Err(Broken::TooLarge),
                "the codex CLI wrote more than 1048576 bytes",
            ),
            // A failed run is not believed, even with an answer file.
            (
                1,
                "",
                "SECRET-TEXT",
                good(),
                "codex CLI exited with status 1",
            ),
            (
                1,
                r#"{"type":"error","message":"SECRET-TEXT"}"#,
                "",
                Ok(None),
                "codex CLI reported an error",
            ),
            (
                1,
                r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#,
                "",
                Ok(None),
                "codex CLI hit a usage limit",
            ),
            (
                1,
                "",
                "Not logged in. Run `codex login`.",
                Ok(None),
                "codex CLI is not logged in",
            ),
            (
                1,
                r#"{"type":"error","message":"unexpected status 401 Unauthorized"}"#,
                "",
                Ok(None),
                "codex CLI is not logged in",
            ),
        ];
        for (code, stdout, stderr, answer, message) in cases {
            let error = judge
                .read_codex(&output(code, stdout, stderr), answer)
                .expect_err("a failure");
            assert_eq!(error.message(), message, "{stdout} {stderr}");
            assert!(!format!("{error:?}").contains("SECRET-TEXT"));
        }
    }

    #[test]
    fn a_program_is_found_like_a_shell_finds_it() {
        assert_eq!(find_program(Path::new("")), None);
        assert_eq!(
            find_program(Path::new("no-such-program-for-the-judge-tests")),
            None
        );
        assert_eq!(find_program(Path::new("/no/such/dir/program")), None);
        #[cfg(unix)]
        {
            let sh = find_program(Path::new("sh")).expect("sh is on the PATH");
            assert!(sh.is_absolute());
            assert_eq!(find_program(&sh), Some(sh));
            // A directory and a file that is not executable do not count.
            assert_eq!(find_program(Path::new("/tmp/")), None);
            assert_eq!(find_program(Path::new("/etc/hosts")), None);
        }
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_without_a_process() {
        for kind in [CliKind::ClaudeCode, CliKind::Codex] {
            let config = CliConfig {
                program: Some(PathBuf::from("/no/such/dir/program")),
                ..CliConfig::new(kind)
            };
            let judge = CliJudge::new(config).expect("a judge");
            assert!(!judge.is_installed());
            let error = judge
                .judge(
                    &Question::new("title", "issue", "a", "b"),
                    Instant::now() + std::time::Duration::from_secs(5),
                )
                .await
                .expect_err("no program");
            assert_eq!(error.kind(), JudgeErrorKind::Transport);
            assert_eq!(
                error.message(),
                format!("{} CLI not found", kind.default_program())
            );
        }
    }

    #[tokio::test]
    async fn a_passed_deadline_starts_nothing() {
        let judge = judge(CliKind::Codex);
        let error = judge
            .judge(&Question::new("title", "issue", "a", "b"), Instant::now())
            .await
            .expect_err("too late");
        assert_eq!(error.kind(), JudgeErrorKind::Deadline);
    }
}
