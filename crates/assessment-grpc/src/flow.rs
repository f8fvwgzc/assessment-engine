//! The two-pass judged assessment, shared by the gRPC service and the CLI.
//!
//! 1. Pass 1 runs the engine as usual. The engine flags the text changes
//!    whose meaning it doubts.
//! 2. The flagged `title` and `description` changes go to the judge: cached
//!    answers first, then a capped number of questions, all at once and
//!    under one deadline.
//! 3. With at least one answer, pass 2 runs the engine again with the
//!    answers as judgments.
//!
//! The judge is optional help. A judge that fails, is slow or is missing
//! never fails the call: the field keeps its lexical reading and gets the
//! status `unavailable` with a short reason.

use std::sync::Arc;
use std::time::Duration;

use assessment_core::assessment::{Assessment, JudgmentReason, JudgmentStatus};
use assessment_core::model::{AssessmentRequest, TextJudgment, field};
use assessment_core::{Engine, Error as CoreError};
use assessment_judge::{Hints, Judge, JudgeError, JudgeErrorKind, Question, Usage, Verdict};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

/// The only fields a server sends to its judge. No policy and no request
/// can change this list.
pub const JUDGED_FIELDS: [&str; 2] = [field::TITLE, field::DESCRIPTION];

/// The time kept free at the end of a call, so the answer still gets out.
const DEADLINE_MARGIN: Duration = Duration::from_millis(250);

/// The least time kept for the second pass.
const MIN_PASS_2: Duration = Duration::from_secs(1);

/// The longest detail a field carries.
const MAX_DETAIL_CHARS: usize = 200;

/// What the server said when a judge was asked for and it has none.
pub const NO_JUDGE: &str = "this server has no judge";

/// How a server uses its judge. Neither policy nor a request changes these.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JudgeSettings {
    /// Whether a call that does not say (`options.judge` unset) is judged.
    pub default_on: bool,
    /// Most questions one call may ask. Cached answers are free.
    pub max_fields_per_request: usize,
    /// Most questions one `AssessBatch` call may ask, over all its items.
    pub max_fields_per_batch: usize,
    /// The longest the judge is waited for in one call.
    pub timeout: Duration,
    /// A text longer than this, in characters, is not sent.
    pub max_text_chars: usize,
}

impl Default for JudgeSettings {
    fn default() -> Self {
        Self {
            default_on: true,
            max_fields_per_request: 4,
            max_fields_per_batch: 16,
            timeout: Duration::from_secs(15),
            max_text_chars: 8_000,
        }
    }
}

impl JudgeSettings {
    #[must_use]
    pub fn with_default_on(mut self, default_on: bool) -> Self {
        self.default_on = default_on;
        self
    }

    #[must_use]
    pub fn with_max_fields_per_request(mut self, n: usize) -> Self {
        self.max_fields_per_request = n;
        self
    }

    #[must_use]
    pub fn with_max_fields_per_batch(mut self, n: usize) -> Self {
        self.max_fields_per_batch = n;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_max_text_chars(mut self, n: usize) -> Self {
        self.max_text_chars = n;
        self
    }
}

/// A judge with its settings, and what the flow needs around it. Cheap to
/// clone. Without a judge, every assessment is the plain engine result.
#[derive(Debug, Clone, Default)]
pub struct Flow {
    judge: Option<Arc<dyn Judge>>,
    settings: JudgeSettings,
    slots: Option<Arc<Semaphore>>,
    shutdown: Option<watch::Receiver<bool>>,
}

impl Flow {
    /// A flow with no judge.
    pub fn new() -> Self {
        Self::default()
    }

    /// This flow with a judge.
    #[must_use]
    pub fn with_judge(mut self, judge: Arc<dyn Judge>, settings: JudgeSettings) -> Self {
        self.judge = Some(judge);
        self.settings = settings;
        self
    }

    /// This flow with a pool of slots: each engine pass holds one.
    #[must_use]
    pub fn with_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.slots = Some(slots);
        self
    }

    /// This flow with a shutdown signal. Once it is `true`, no judge is
    /// waited for any longer.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Whether there is a judge.
    pub fn has_judge(&self) -> bool {
        self.judge.is_some()
    }

    /// The judge's name, such as `anthropic/claude-opus-5-5`.
    pub fn judge_id(&self) -> Option<&str> {
        self.judge.as_deref().map(Judge::id)
    }

    /// The settings in force.
    pub fn settings(&self) -> &JudgeSettings {
        &self.settings
    }

    /// Whether a call with this `options.judge` is judged.
    pub fn wants(&self, asked: Option<bool>) -> bool {
        self.judge.is_some() && asked.unwrap_or(self.settings.default_on)
    }

    fn shutting_down(&self) -> bool {
        self.shutdown.as_ref().is_some_and(|s| *s.borrow())
    }
}

/// What the judge did in one call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JudgeReport {
    /// The judge's name; empty when no judge took part.
    pub judge: String,
    /// Flagged fields the judge could be asked about.
    pub flagged: usize,
    /// Fields that got an answer, cached ones included.
    pub judged: usize,
    /// Answers that came from the cache.
    pub cached: usize,
    /// Fields left without an answer.
    pub unavailable: usize,
    /// The tokens of the answers.
    pub usage: Usage,
    /// The estimated cost of the answers, in US dollars.
    pub cost_usd: f64,
}

impl JudgeReport {
    /// Adds another report (of the same judge).
    pub fn add(&mut self, other: &JudgeReport) {
        if self.judge.is_empty() {
            self.judge.clone_from(&other.judge);
        }
        self.flagged += other.flagged;
        self.judged += other.judged;
        self.cached += other.cached;
        self.unavailable += other.unavailable;
        self.usage = self.usage.plus(other.usage);
        self.cost_usd += other.cost_usd;
    }

    /// One log line for a judged call. Nothing when no field was flagged.
    pub fn log(&self, call: &'static str) {
        if self.flagged == 0 {
            return;
        }
        tracing::info!(
            call,
            judge = %self.judge,
            flagged = self.flagged,
            judged = self.judged,
            cached = self.cached,
            unavailable = self.unavailable,
            input_tokens = self.usage.input_tokens,
            output_tokens = self.usage.output_tokens,
            cost_usd = self.cost_usd,
            "judged call"
        );
    }
}

/// A judged assessment.
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub assessment: Assessment,
    pub report: JudgeReport,
}

/// Why a judged assessment failed. A judge never causes one.
#[non_exhaustive]
#[derive(Debug)]
pub enum FlowError {
    /// The engine rejected the request.
    Core(CoreError),
    /// A task of the flow failed or the process is shutting down.
    Internal(String),
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowError::Core(e) => e.fmt(f),
            FlowError::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FlowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FlowError::Core(e) => Some(e),
            FlowError::Internal(_) => None,
        }
    }
}

/// Assesses `request`, asking the flow's judge about flagged text changes.
///
/// `call_deadline` is when the caller stops waiting. The judge is given the
/// time that leaves room for the second pass, and at most
/// [`JudgeSettings::timeout`]. With `options.judge == Some(false)`, or
/// without a judge, the result is the engine's own.
pub async fn assess_judged(
    engine: Arc<Engine>,
    request: AssessmentRequest,
    flow: &Flow,
    call_deadline: Option<Instant>,
) -> Result<Judged, FlowError> {
    let slot = match &flow.slots {
        Some(slots) => Some(
            Arc::clone(slots)
                .acquire_owned()
                .await
                .map_err(|_| FlowError::Internal("the service is shutting down".to_owned()))?,
        ),
        None => None,
    };
    let first_flow = flow.clone();
    let first = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        pass_1(engine, request, &first_flow)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "an assessment task failed");
        FlowError::Internal("the assessment task failed".to_owned())
    })?
    .map_err(FlowError::Core)?;
    let judged = complete(first, flow, call_deadline).await;
    judged.report.log("assess");
    Ok(judged)
}

/// The first pass of one assessment, and what the second one needs.
#[derive(Debug)]
pub(crate) struct Pass1 {
    engine: Arc<Engine>,
    request: AssessmentRequest,
    assessment: Assessment,
    /// The call is judged: the request was run with the texts.
    judged: bool,
    /// The caller asked for the texts itself.
    keep_texts: bool,
    pub(crate) elapsed: Duration,
}

/// Runs the first pass (blocking).
///
/// A call that is not judged is run exactly as given. A judged one is run
/// with the texts in its judgment requests, and with `now` fixed so that
/// both passes see the same time.
pub(crate) fn pass_1(
    engine: Arc<Engine>,
    mut request: AssessmentRequest,
    flow: &Flow,
) -> Result<Pass1, CoreError> {
    let asked = request.options.judge;
    let judged = flow.wants(asked);
    let keep_texts = request.options.judgment_texts;
    if judged {
        request.options.judgment_texts = true;
        if request.now.is_none() {
            request.now = Some(chrono::Utc::now());
        }
    }
    let started = std::time::Instant::now();
    let mut assessment = engine.assess(&request)?;
    if asked == Some(true) && !flow.has_judge() {
        mark_no_judge(&mut assessment);
    }
    Ok(Pass1 {
        engine,
        request,
        assessment,
        judged,
        keep_texts,
        elapsed: started.elapsed(),
    })
}

impl Pass1 {
    /// The flagged fields of this assessment that the judge may be asked
    /// about. `item` is the place of the assessment in its batch.
    pub(crate) fn candidates(&self, item: usize) -> Vec<Candidate> {
        if !self.judged {
            return Vec::new();
        }
        candidates(&self.assessment, item, |change_index| {
            let change = self.request.changes.get(change_index)?;
            let entity = change.after.as_ref().or(change.before.as_ref())?;
            Some(entity.title().to_owned())
        })
    }

    /// Whether the judge may be asked about a field of this assessment.
    pub(crate) fn flagged(&self) -> bool {
        self.judged
            && self.assessment.judgment_requests.iter().any(|asked| {
                JUDGED_FIELDS.contains(&asked.field.as_str())
                    && asked.before.is_some()
                    && asked.after.is_some()
            })
    }

    /// The first pass as the answer, with no judge involved.
    pub(crate) fn into_assessment(mut self) -> Assessment {
        if !self.keep_texts {
            strip_texts(&mut self.assessment);
        }
        self.assessment
    }
}

/// Asks the judge about the flagged fields of `first` and finishes it.
pub(crate) async fn complete(first: Pass1, flow: &Flow, call_deadline: Option<Instant>) -> Judged {
    let candidates = first.candidates(0);
    if candidates.is_empty() {
        return Judged {
            assessment: first.into_assessment(),
            report: JudgeReport::default(),
        };
    }
    let cap = flow.settings.max_fields_per_request;
    let answers = ask_all(flow, &candidates, cap, call_deadline, first.elapsed).await;
    finish(first, candidates, answers, flow, call_deadline).await
}

/// Runs the second pass with the answers, and marks the fields that got
/// none. If the second pass fails or is too late, the first one is the
/// answer.
pub(crate) async fn finish(
    first: Pass1,
    candidates: Vec<Candidate>,
    mut answers: Vec<Answer>,
    flow: &Flow,
    call_deadline: Option<Instant>,
) -> Judged {
    let Pass1 {
        engine,
        mut request,
        mut assessment,
        keep_texts,
        ..
    } = first;
    let verdicts = judgments(&candidates, &answers);
    if !verdicts.is_empty() {
        for (change_index, judgment) in verdicts {
            if let Some(change) = request.changes.get_mut(change_index) {
                merge_judgment(&mut change.judgments, judgment);
            }
        }
        match pass_2(engine, request, flow, call_deadline).await {
            Ok(second) => assessment = second,
            Err(detail) => void_verdicts(&mut answers, detail),
        }
    }
    let report = settle(&mut assessment, &candidates, &answers, keep_texts, flow);
    Judged { assessment, report }
}

/// The second pass, under the watchdog. The error is the detail for the
/// fields whose answers could not be used.
async fn pass_2(
    engine: Arc<Engine>,
    request: AssessmentRequest,
    flow: &Flow,
    call_deadline: Option<Instant>,
) -> Result<Assessment, &'static str> {
    const FAILED: &str = PASS_2_FAILED;
    let slots = flow.slots.clone();
    let work = async move {
        let slot = match slots {
            Some(slots) => Some(slots.acquire_owned().await.map_err(|_| FAILED)?),
            None => None,
        };
        let done = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            engine.assess(&request)
        })
        .await;
        match done {
            Ok(Ok(assessment)) => Ok(assessment),
            Ok(Err(err)) => {
                // Pass 1 took the same request, so only a judgment can be
                // at fault here.
                tracing::warn!(error = %err, "the second pass was rejected; using the first");
                Err(FAILED)
            }
            Err(err) => {
                tracing::error!(error = %err, "the second pass task failed");
                Err(FAILED)
            }
        }
    };
    match watchdog(call_deadline) {
        Some(at) => match tokio::time::timeout_at(at, work).await {
            Ok(result) => result,
            Err(_) => Err(LATE),
        },
        None => work.await,
    }
}

/// One flagged field the judge may be asked about.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    /// The place of the assessment in its batch.
    pub(crate) item: usize,
    pub(crate) change_index: usize,
    pub(crate) entity_id: String,
    pub(crate) field: String,
    fingerprint: String,
    /// The most important reason the field is flagged.
    pub(crate) reason: Option<JudgmentReason>,
    pub(crate) lexical_magnitude: f64,
    question: Question,
}

impl Candidate {
    /// The order in which a batch spends its questions: the most important
    /// reason first, then the larger lexical change, then the place in the
    /// batch.
    pub(crate) fn batch_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
        // A field with no reason goes last.
        let reason = |c: &Candidate| (c.reason.is_none(), c.reason);
        reason(a)
            .cmp(&reason(b))
            .then_with(|| b.lexical_magnitude.total_cmp(&a.lexical_magnitude))
            .then_with(|| a.item.cmp(&b.item))
            .then_with(|| a.change_index.cmp(&b.change_index))
            .then_with(|| a.field.cmp(&b.field))
    }
}

/// What became of one candidate.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    Verdict(Verdict),
    /// No answer, and why.
    Unavailable(String),
}

/// The candidates of `assessment`, in the engine's order. `title` gives the
/// title of the entity of a change, by the index of its change set.
///
/// Only the fields in [`JUDGED_FIELDS`] with both texts are candidates.
pub(crate) fn candidates(
    assessment: &Assessment,
    item: usize,
    title: impl Fn(usize) -> Option<String>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for asked in &assessment.judgment_requests {
        if !JUDGED_FIELDS.contains(&asked.field.as_str()) {
            continue;
        }
        let (Some(before), Some(after)) = (&asked.before, &asked.after) else {
            continue;
        };
        let change = assessment
            .changes
            .iter()
            .find(|c| c.entity_id == asked.entity_id);
        let kind = change.map_or("", |c| c.kind.as_str());
        let metrics = change
            .and_then(|c| c.fields.iter().find(|f| f.field == asked.field))
            .map(|f| &f.metrics);
        let count = |name: &str| {
            let value = metrics.and_then(|m| m.get(name)).copied().unwrap_or(0.0);
            if value.is_finite() && value > 0.0 {
                // A count: small and whole.
                value.min(f64::from(u32::MAX)) as u32
            } else {
                0
            }
        };
        let hints = Hints {
            list_items_added: count("items_added"),
            list_items_removed: count("items_removed"),
            list_items_modified: count("items_modified"),
            negation_changes: count("negation_flips"),
            requirement_word_changes: count("modal_shifts"),
            number_changes: count("numbers_changed"),
        };
        let mut question = Question::new(&asked.field, kind, before, after)
            .with_reasons(asked.reasons.clone())
            .with_hints(hints);
        if let Some(title) = title(asked.change_index) {
            question = question.with_context_title(&title);
        }
        out.push(Candidate {
            item,
            change_index: asked.change_index,
            entity_id: asked.entity_id.to_string(),
            field: asked.field.clone(),
            fingerprint: asked.fingerprint.clone(),
            reason: asked.reasons.iter().min().copied(),
            lexical_magnitude: asked.lexical_magnitude,
            question,
        });
    }
    out
}

/// Asks the judge about `candidates`, in their order.
///
/// A text that is too long is not sent. A cached answer is free. The first
/// `cap` other questions are asked at the same time, under one deadline;
/// the rest get no answer. The answers come back in the candidates' order.
pub(crate) async fn ask_all(
    flow: &Flow,
    candidates: &[Candidate],
    cap: usize,
    call_deadline: Option<Instant>,
    pass_1_time: Duration,
) -> Vec<Answer> {
    let Some(judge) = &flow.judge else {
        return vec![Answer::Unavailable(NO_JUDGE.to_owned()); candidates.len()];
    };
    let settings = &flow.settings;
    let now = Instant::now();
    let by_timeout = now + settings.timeout;
    // Room for the second pass: twice the first, and at least a second.
    let reserve = pass_1_time.saturating_mul(2).max(MIN_PASS_2) + DEADLINE_MARGIN;
    let (deadline, timeout_is_the_limit) = match call_deadline {
        Some(call) => match call.checked_sub(reserve) {
            Some(by_call) if by_call < by_timeout => (by_call, false),
            Some(_) => (by_timeout, true),
            None => (now, false),
        },
        None => (by_timeout, true),
    };

    let mut answers: Vec<Option<Answer>> = vec![None; candidates.len()];
    let mut asked = JoinSet::new();
    let mut misses = 0usize;
    let too_long = |text: &str| text.chars().count() > settings.max_text_chars;
    for (i, candidate) in candidates.iter().enumerate() {
        let question = &candidate.question;
        if too_long(&question.before) || too_long(&question.after) {
            answers[i] = Some(Answer::Unavailable(format!(
                "text longer than {} characters",
                settings.max_text_chars
            )));
        } else if let Some(verdict) = judge.peek(question) {
            answers[i] = Some(Answer::Verdict(verdict));
        } else if misses >= cap {
            answers[i] = Some(Answer::Unavailable(format!(
                "over the server's cap of {cap} fields"
            )));
        } else if flow.shutting_down() {
            answers[i] = Some(Answer::Unavailable("server shutting down".to_owned()));
        } else {
            misses += 1;
            let judge = Arc::clone(judge);
            let question = question.clone();
            asked.spawn(async move {
                let result = assessment_judge::ask(&*judge, &question, deadline).await;
                (i, result)
            });
        }
    }

    // Dropping this future drops `asked`, which cancels the open questions.
    let mut shutdown = flow.shutdown.clone();
    let mut stopped = false;
    loop {
        tokio::select! {
            biased;
            () = shut_down(&mut shutdown) => {
                stopped = true;
                asked.abort_all();
                break;
            }
            joined = asked.join_next() => match joined {
                None => break,
                Some(Ok((i, Ok(verdict)))) => answers[i] = Some(Answer::Verdict(verdict)),
                Some(Ok((i, Err(error)))) => {
                    let detail = error_detail(&error, settings, timeout_is_the_limit);
                    answers[i] = Some(Answer::Unavailable(detail));
                }
                Some(Err(error)) => {
                    tracing::error!(%error, "a judge task failed");
                }
            },
        }
    }
    let missing = if stopped {
        "server shutting down"
    } else {
        "the judge task failed"
    };
    answers
        .into_iter()
        .map(|a| a.unwrap_or_else(|| Answer::Unavailable(missing.to_owned())))
        .collect()
}

/// Completes once the shutdown signal is `true`. Never completes without a
/// signal.
async fn shut_down(shutdown: &mut Option<watch::Receiver<bool>>) {
    match shutdown {
        Some(signal) => {
            if signal.wait_for(|stop| *stop).await.is_err() {
                // The sender is gone: no shutdown will be announced.
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// The short reason shown on a field the judge did not answer.
fn error_detail(
    error: &JudgeError,
    settings: &JudgeSettings,
    timeout_is_the_limit: bool,
) -> String {
    let message = error.message();
    let detail = match error.kind() {
        JudgeErrorKind::Deadline if message == "not enough time left" || !timeout_is_the_limit => {
            "not enough time left".to_owned()
        }
        JudgeErrorKind::Deadline => {
            format!("judge timed out after {} s", settings.timeout.as_secs())
        }
        JudgeErrorKind::SpendCap => "spend cap reached".to_owned(),
        JudgeErrorKind::ShuttingDown => "server shutting down".to_owned(),
        JudgeErrorKind::Refused if !message.starts_with("refused") => {
            format!("refused ({message})")
        }
        _ => match error.http_status() {
            Some(status) if !message.contains(&status.to_string()) => {
                format!("HTTP {status} {message}")
            }
            _ => message.to_owned(),
        },
    };
    let detail = detail.trim();
    if detail.is_empty() {
        return format!("the judge failed ({})", error.kind().as_str());
    }
    detail.chars().take(MAX_DETAIL_CHARS).collect()
}

/// The judgments of the answered candidates, each with the index of its
/// change set. The fingerprint is the engine's own, so a judgment applies
/// to exactly the texts that were asked about.
pub(crate) fn judgments(
    candidates: &[Candidate],
    answers: &[Answer],
) -> Vec<(usize, TextJudgment)> {
    candidates
        .iter()
        .zip(answers)
        .filter_map(|(candidate, answer)| match answer {
            Answer::Verdict(verdict) => Some((
                candidate.change_index,
                verdict
                    .clone()
                    .into_judgment(&candidate.field, &candidate.fingerprint),
            )),
            Answer::Unavailable(_) => None,
        })
        .collect()
}

/// Whether any of `answers` is a verdict: only then is there a second pass.
pub(crate) fn has_verdict(answers: &[Answer]) -> bool {
    answers.iter().any(|a| matches!(a, Answer::Verdict(_)))
}

/// Turns every verdict into "no answer": the pass that would have used
/// them did not happen.
pub(crate) fn void_verdicts(answers: &mut [Answer], detail: &str) {
    for answer in answers {
        if matches!(answer, Answer::Verdict(_)) {
            *answer = Answer::Unavailable(detail.to_owned());
        }
    }
}

/// The detail of a field whose answer came too late to be used.
pub(crate) const LATE: &str = "not enough time left";

/// The detail of a field whose answer the engine could not use.
pub(crate) const PASS_2_FAILED: &str = "the second pass failed";

/// When the second pass of a call with this deadline must be done.
pub(crate) fn watchdog(call_deadline: Option<Instant>) -> Option<Instant> {
    call_deadline.map(|deadline| deadline.checked_sub(DEADLINE_MARGIN).unwrap_or(deadline))
}

/// Puts `judgment` in `list`, in place of any other judgment of its field.
pub(crate) fn merge_judgment(list: &mut Vec<TextJudgment>, judgment: TextJudgment) {
    list.retain(|j| j.field != judgment.field);
    list.push(judgment);
}

/// Finishes a judged assessment: marks the fields that got no answer
/// `unavailable`, removes the texts the caller did not ask for, and counts
/// what the judge did.
pub(crate) fn settle(
    assessment: &mut Assessment,
    candidates: &[Candidate],
    answers: &[Answer],
    keep_texts: bool,
    flow: &Flow,
) -> JudgeReport {
    let mut report = JudgeReport {
        judge: flow.judge_id().unwrap_or_default().to_owned(),
        flagged: candidates.len(),
        ..JudgeReport::default()
    };
    for (candidate, answer) in candidates.iter().zip(answers) {
        match answer {
            Answer::Verdict(verdict) => {
                report.judged += 1;
                if verdict.cached {
                    report.cached += 1;
                }
                report.usage = report.usage.plus(verdict.usage);
            }
            Answer::Unavailable(detail) => {
                report.unavailable += 1;
                mark_unavailable(assessment, &candidate.entity_id, &candidate.field, detail);
            }
        }
    }
    report.cost_usd = assessment_judge::price::cost_usd(&report.judge, &report.usage);
    if !keep_texts {
        strip_texts(assessment);
    }
    report
}

/// Whether a field with this status waits for a judgment. A field whose
/// supplied judgment is stale is asked about again, like one with none.
fn waiting(status: JudgmentStatus) -> bool {
    matches!(status, JudgmentStatus::Requested | JudgmentStatus::Stale)
}

/// Sets a field that still waits for a judgment to `unavailable`.
fn mark_unavailable(assessment: &mut Assessment, entity_id: &str, field: &str, detail: &str) {
    for change in &mut assessment.changes {
        if change.entity_id.as_str() != entity_id {
            continue;
        }
        for f in &mut change.fields {
            if f.field != field {
                continue;
            }
            if let Some(judgment) = &mut f.judgment
                && waiting(judgment.status)
            {
                judgment.status = JudgmentStatus::Unavailable;
                judgment.detail = detail.to_owned();
            }
        }
    }
}

/// Marks the flagged judged fields of an assessment made for a caller that
/// asked for a judge (`options.judge == true`) on a server without one.
pub(crate) fn mark_no_judge(assessment: &mut Assessment) {
    for change in &mut assessment.changes {
        for f in &mut change.fields {
            if !JUDGED_FIELDS.contains(&f.field.as_str()) {
                continue;
            }
            if let Some(judgment) = &mut f.judgment
                && waiting(judgment.status)
            {
                judgment.status = JudgmentStatus::Unavailable;
                judgment.detail = NO_JUDGE.to_owned();
            }
        }
    }
}

/// Removes the texts from the judgment requests.
pub(crate) fn strip_texts(assessment: &mut Assessment) {
    for asked in &mut assessment.judgment_requests {
        asked.before = None;
        asked.after = None;
    }
}
