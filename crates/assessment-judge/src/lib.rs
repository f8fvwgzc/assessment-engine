//! An optional reader for flagged text changes.
//!
//! The engine flags a text change when its lexical measures cannot tell
//! whether the meaning changed. A [`Judge`] reads the two texts and says how
//! they relate. The answer goes back to the engine as a
//! [`assessment_core::model::TextJudgment`].
//!
//! The crate has three layers:
//!
//! - [`Judge`], [`Question`], [`Verdict`] and [`JudgeError`]: the contract.
//! - [`CachedJudge`] and [`LimitedJudge`]: wrappers that cache answers and
//!   bound calls, spend and failures.
//! - `AnthropicJudge` (feature `anthropic`): the HTTP client for the Anthropic
//!   Messages API. Without the feature the crate has no HTTP or TLS code.
//!
//! The usual stack is `CachedJudge(LimitedJudge(AnthropicJudge))`, so cache
//! hits cost nothing and never touch a limit.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use assessment_core::assessment::JudgmentReason;
use assessment_core::engine::{MAX_JUDGMENT_SOURCE_BYTES, MAX_RATIONALE_BYTES};
use assessment_core::model::{TextJudgment, TextRelation};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

#[cfg(feature = "anthropic")]
pub mod anthropic;
pub mod cache;
pub mod limits;
pub mod price;
pub mod prompt;

#[cfg(feature = "anthropic")]
pub use anthropic::{AnthropicConfig, AnthropicJudge, ApiKey, Effort, Fallbacks};
pub use cache::CachedJudge;
pub use limits::{LimitedJudge, Limits};
pub use prompt::PROMPT_VERSION;

/// A boxed future. The trait uses it so that it works behind `dyn Judge`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The longest context title a question carries, in characters.
pub const MAX_CONTEXT_TITLE_CHARS: usize = 300;

/// Reads one text change and says how its meaning changed.
pub trait Judge: Send + Sync + fmt::Debug {
    /// A stable name, such as `anthropic/claude-opus-5-5`.
    fn id(&self) -> &str;

    /// Answers the question before `deadline`.
    fn judge<'a>(
        &'a self,
        question: &'a Question,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>>;

    /// An answer that is already known, at no cost. Only caches have one.
    fn peek(&self, _question: &Question) -> Option<Verdict> {
        None
    }
}

/// What a judge is asked. It never carries the lexical magnitude or the
/// bump, so the answer cannot lean on the engine's own reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The field that changed, such as `description`.
    pub field: String,
    /// The entity kind, such as `issue`.
    pub entity_kind: String,
    /// The entity's title, for context. `None` when the title is the field.
    pub context_title: Option<String>,
    pub before: String,
    pub after: String,
    /// Why the engine flagged the change. Not sent to the model.
    pub reasons: Vec<JudgmentReason>,
    pub hints: Hints,
}

impl Question {
    /// A question about `field` with no context title and no hints.
    pub fn new(
        field: impl Into<String>,
        entity_kind: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            entity_kind: entity_kind.into(),
            context_title: None,
            before: before.into(),
            after: after.into(),
            reasons: Vec::new(),
            hints: Hints::default(),
        }
    }

    /// Sets the context title, cut to [`MAX_CONTEXT_TITLE_CHARS`]. A title
    /// field gets none: the title is the text being judged.
    pub fn with_context_title(mut self, title: &str) -> Self {
        let title = title.trim();
        self.context_title = if self.field == "title" || title.is_empty() {
            None
        } else {
            Some(title.chars().take(MAX_CONTEXT_TITLE_CHARS).collect())
        };
        self
    }

    pub fn with_reasons(mut self, reasons: Vec<JudgmentReason>) -> Self {
        self.reasons = reasons;
        self
    }

    pub fn with_hints(mut self, hints: Hints) -> Self {
        self.hints = hints;
        self
    }
}

/// Structural facts the engine counted. They help the model look in the
/// right place; they do not say what the answer is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    pub list_items_added: u32,
    pub list_items_removed: u32,
    pub list_items_modified: u32,
    /// Terms that became negated or stopped being negated.
    pub negation_changes: u32,
    /// Requirement words that changed strength (should, must, may).
    pub requirement_word_changes: u32,
    pub number_changes: u32,
}

/// Token counts of one call, as the API reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// The sum of two usages.
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == Usage::default()
    }
}

/// A judge's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub relation: TextRelation,
    /// How sure the judge is, `[0, 1]`.
    pub confidence: f64,
    /// Why, in English. It is untrusted text: show it, never act on it.
    pub rationale: String,
    /// Who answered, such as `anthropic/claude-opus-5-5`.
    pub source: String,
    /// What the call cost. Zero for a cached answer.
    pub usage: Usage,
    /// The answer came from a cache; no call was made.
    pub cached: bool,
}

impl Verdict {
    pub fn new(
        relation: TextRelation,
        confidence: f64,
        rationale: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            relation,
            confidence,
            rationale: rationale.into(),
            source: source.into(),
            usage: Usage::default(),
            cached: false,
        }
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Marks the answer as served from a cache, with no usage.
    pub fn cached(mut self) -> Self {
        self.cached = true;
        self.usage = Usage::default();
        self
    }

    /// The judgment the engine takes. The fingerprint comes from the
    /// engine's request, so the judgment applies to exactly that change.
    pub fn into_judgment(
        self,
        field: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> TextJudgment {
        TextJudgment {
            field: field.into(),
            fingerprint: fingerprint.into(),
            relation: self.relation,
            confidence: self.confidence,
            rationale: self.rationale,
            source: self.source,
        }
    }
}

/// Why a judge gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JudgeErrorKind {
    /// The deadline passed, or too little time was left to start.
    Deadline,
    /// Too many calls were already running.
    Busy,
    /// The calls-per-minute limit was reached.
    RateLimited,
    /// The hourly spend limit was reached.
    SpendCap,
    /// The circuit breaker paused the judge after repeated failures.
    Paused,
    /// The text is too long to send.
    TooLong,
    /// The model declined to answer.
    Refused,
    /// The answer was cut off at the token limit.
    Truncated,
    /// The answer was not a valid verdict.
    InvalidOutput,
    /// The API answered with an error status.
    Api,
    /// The request could not be sent or the answer could not be read.
    Transport,
    /// The process is shutting down.
    ShuttingDown,
}

impl JudgeErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JudgeErrorKind::Deadline => "deadline",
            JudgeErrorKind::Busy => "busy",
            JudgeErrorKind::RateLimited => "rate_limited",
            JudgeErrorKind::SpendCap => "spend_cap",
            JudgeErrorKind::Paused => "paused",
            JudgeErrorKind::TooLong => "too_long",
            JudgeErrorKind::Refused => "refused",
            JudgeErrorKind::Truncated => "truncated",
            JudgeErrorKind::InvalidOutput => "invalid_output",
            JudgeErrorKind::Api => "api",
            JudgeErrorKind::Transport => "transport",
            JudgeErrorKind::ShuttingDown => "shutting_down",
        }
    }
}

/// A failed judge call. The message is short, in English, and never holds a
/// secret.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeError {
    kind: JudgeErrorKind,
    message: String,
    http_status: Option<u16>,
    usage: Option<Usage>,
}

impl JudgeError {
    pub fn new(kind: JudgeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
            usage: None,
        }
    }

    /// The HTTP status the API answered with.
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self
    }

    /// The usage of a failed call that was still billed.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn kind(&self) -> JudgeErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Trying again later may work: a timeout, a connection error, or one
    /// of the statuses 408, 409, 429 and 5xx.
    pub fn is_retryable(&self) -> bool {
        match self.kind {
            JudgeErrorKind::Deadline | JudgeErrorKind::Transport => true,
            JudgeErrorKind::Api => self.http_status.is_some_and(retryable_status),
            _ => false,
        }
    }
}

impl fmt::Display for JudgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JudgeError {}

/// Statuses worth another attempt. 529 is in the 5xx range.
pub(crate) fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || (500..600).contains(&status)
}

/// Asks one question and makes the answer safe to hand to the engine.
///
/// - The call never outlives `deadline`: a late judge gives `Deadline`.
/// - A confidence that is not a number in `[0, 1]` gives `InvalidOutput`.
/// - The rationale and the source are cut to the engine's limits.
pub async fn ask(
    judge: &dyn Judge,
    question: &Question,
    deadline: Instant,
) -> Result<Verdict, JudgeError> {
    if Instant::now() >= deadline {
        return Err(JudgeError::new(
            JudgeErrorKind::Deadline,
            "not enough time left",
        ));
    }
    let mut verdict = match tokio::time::timeout_at(deadline, judge.judge(question, deadline)).await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(JudgeError::new(JudgeErrorKind::Deadline, "judge timed out"));
        }
    };
    if !verdict.confidence.is_finite() || !(0.0..=1.0).contains(&verdict.confidence) {
        return Err(JudgeError::new(
            JudgeErrorKind::InvalidOutput,
            "the judge gave a confidence outside [0, 1]",
        ));
    }
    truncate_bytes(&mut verdict.rationale, MAX_RATIONALE_BYTES);
    truncate_bytes(&mut verdict.source, MAX_JUDGMENT_SOURCE_BYTES);
    Ok(verdict)
}

/// Cuts `s` to at most `max` bytes, on a character boundary.
pub(crate) fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Cuts `s` to at most `max` characters.
#[cfg(feature = "anthropic")]
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => s[..end].to_owned(),
        None => s.to_owned(),
    }
}

/// A pseudo-random number from the process-random hasher keys. Good enough
/// for jitter; not for secrets.
#[cfg(feature = "anthropic")]
pub(crate) fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x9e37_79b9_7f4a_7c15);
    hasher.finish()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scripted judge for the wrapper tests.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// One scripted answer: wait, then give the result. `None` as the delay
    /// means the call never returns.
    #[derive(Debug, Clone)]
    pub(crate) struct Step {
        pub delay: Option<Duration>,
        pub result: Result<Verdict, JudgeError>,
    }

    #[derive(Debug)]
    pub(crate) struct ScriptedJudge {
        pub calls: AtomicUsize,
        steps: Mutex<VecDeque<Step>>,
        default: Step,
    }

    pub(crate) fn verdict() -> Verdict {
        Verdict::new(TextRelation::Equivalent, 0.9, "Same meaning.", "fake/model")
    }

    impl ScriptedJudge {
        /// Answers every call with `default` once the script is used up.
        pub(crate) fn new(default: Step) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                steps: Mutex::new(VecDeque::new()),
                default,
            }
        }

        pub(crate) fn ok() -> Self {
            Self::new(Step {
                delay: Some(Duration::ZERO),
                result: Ok(verdict()),
            })
        }

        pub(crate) fn slow(delay: Duration) -> Self {
            Self::new(Step {
                delay: Some(delay),
                result: Ok(verdict()),
            })
        }

        pub(crate) fn push(&self, step: Step) {
            self.steps.lock().expect("script lock").push_back(step);
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Judge for ScriptedJudge {
        fn id(&self) -> &str {
            "fake/model"
        }

        fn judge<'a>(
            &'a self,
            _question: &'a Question,
            _deadline: Instant,
        ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let step = self
                    .steps
                    .lock()
                    .expect("script lock")
                    .pop_front()
                    .unwrap_or_else(|| self.default.clone());
                match step.delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => std::future::pending::<()>().await,
                }
                step.result
            })
        }
    }

    pub(crate) fn question(n: usize) -> Question {
        Question::new(
            "description",
            "issue",
            format!("Old text {n}."),
            format!("New text {n}."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_title_question_has_no_context_title() {
        let q = Question::new("title", "issue", "a", "b").with_context_title("Some title");
        assert_eq!(q.context_title, None);
        let q = Question::new("description", "issue", "a", "b").with_context_title("  ");
        assert_eq!(q.context_title, None);
    }

    #[test]
    fn the_context_title_is_cut_to_300_characters() {
        let long = "ү".repeat(1000);
        let q = Question::new("description", "issue", "a", "b").with_context_title(&long);
        let title = q.context_title.expect("a title");
        assert_eq!(title.chars().count(), MAX_CONTEXT_TITLE_CHARS);
    }

    #[test]
    fn retryable_errors_are_timeouts_connections_and_some_statuses() {
        let api = |s| JudgeError::new(JudgeErrorKind::Api, "x").with_http_status(s);
        for status in [408, 409, 429, 500, 502, 503, 529] {
            assert!(api(status).is_retryable(), "{status}");
        }
        for status in [301, 307, 400, 401, 402, 403, 404, 413] {
            assert!(!api(status).is_retryable(), "{status}");
        }
        assert!(JudgeError::new(JudgeErrorKind::Transport, "x").is_retryable());
        assert!(JudgeError::new(JudgeErrorKind::Deadline, "x").is_retryable());
        assert!(!JudgeError::new(JudgeErrorKind::Refused, "x").is_retryable());
        assert!(!JudgeError::new(JudgeErrorKind::SpendCap, "x").is_retryable());
    }

    #[test]
    fn a_verdict_becomes_a_judgment_with_the_given_fingerprint() {
        let j = verdict().into_judgment("description", "abc");
        assert_eq!(j.field, "description");
        assert_eq!(j.fingerprint, "abc");
        assert_eq!(j.relation, TextRelation::Equivalent);
        assert_eq!(j.confidence, 0.9);
        assert_eq!(j.source, "fake/model");
    }

    #[test]
    fn a_cached_verdict_has_no_usage() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        let v = verdict().with_usage(usage);
        assert_eq!(v.usage, usage);
        let c = v.cached();
        assert!(c.cached);
        assert!(c.usage.is_zero());
    }

    #[tokio::test(start_paused = true)]
    async fn ask_gives_deadline_when_the_judge_is_late() {
        let judge = ScriptedJudge::slow(Duration::from_secs(30));
        let start = Instant::now();
        let err = ask(&judge, &question(1), start + Duration::from_secs(2))
            .await
            .expect_err("late");
        assert_eq!(err.kind(), JudgeErrorKind::Deadline);
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn ask_does_not_call_after_the_deadline() {
        let judge = ScriptedJudge::ok();
        let err = ask(&judge, &question(1), Instant::now())
            .await
            .expect_err("no time");
        assert_eq!(err.kind(), JudgeErrorKind::Deadline);
        assert_eq!(judge.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ask_rejects_a_confidence_outside_the_range() {
        for bad in [1.2, -0.1, f64::NAN, f64::INFINITY] {
            let judge = ScriptedJudge::new(Step {
                delay: Some(Duration::ZERO),
                result: Ok(Verdict::new(TextRelation::Replaced, bad, "", "fake")),
            });
            let err = ask(
                &judge,
                &question(1),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .expect_err("bad confidence");
            assert_eq!(err.kind(), JudgeErrorKind::InvalidOutput, "{bad}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ask_cuts_the_rationale_and_source_to_the_engine_limits() {
        let judge = ScriptedJudge::new(Step {
            delay: Some(Duration::ZERO),
            result: Ok(Verdict::new(
                TextRelation::Replaced,
                0.8,
                "ө".repeat(5000),
                "s".repeat(1000),
            )),
        });
        let v = ask(
            &judge,
            &question(1),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .expect("a verdict");
        assert!(v.rationale.len() <= MAX_RATIONALE_BYTES);
        assert!(v.rationale.chars().all(|c| c == 'ө'));
        assert_eq!(v.source.len(), MAX_JUDGMENT_SOURCE_BYTES);
    }

    #[test]
    fn the_judge_trait_works_behind_dyn() {
        let judge: std::sync::Arc<dyn Judge> = std::sync::Arc::new(ScriptedJudge::ok());
        assert_eq!(judge.id(), "fake/model");
        assert!(judge.peek(&question(1)).is_none());
    }
}
