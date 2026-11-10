//! Limits around a judge: concurrency, calls per minute, spend per hour and
//! a circuit breaker.
//!
//! Every wait is bounded. A call waits for a free slot only as long as it
//! could still start inside its deadline, and for the rate window only a
//! moment. A call that cannot start fails with a clear error, so a request
//! never hangs on the judge.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::price::{cost_usd, reserve_usd};
use crate::prompt::{SYSTEM_PROMPT, user_content};
use crate::{BoxFuture, Judge, JudgeError, JudgeErrorKind, Question, Verdict};

/// The longest a call waits for the rate window.
pub const MAX_WAIT: Duration = Duration::from_millis(500);
/// The window of the call rate limit.
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
/// The window of the spend limit.
pub const SPEND_WINDOW: Duration = Duration::from_secs(3600);
/// Retryable failures in a row that pause the judge.
pub const FAILURES_TO_PAUSE: u32 = 5;
/// The pause after repeated retryable failures.
pub const SHORT_PAUSE: Duration = Duration::from_secs(30);
/// The pause after a failure that will not fix itself: a bad key, a
/// missing model or a request the API keeps rejecting.
pub const LONG_PAUSE: Duration = Duration::from_secs(300);
/// Identical 400 or 402 answers in a row that pause the judge.
pub const BAD_REQUESTS_TO_PAUSE: u32 = 3;
/// The first guess of how long a call takes.
pub const INITIAL_LATENCY: Duration = Duration::from_secs(6);
/// The smallest time budget a call may start with.
pub const MIN_BUDGET_FLOOR: Duration = Duration::from_secs(2);

const BUDGET_FACTOR: f64 = 1.2;
/// The largest share of the timeout the minimum budget may take. It stays
/// below 1, so a call with the whole timeout ahead of it always starts.
const BUDGET_CEILING: f64 = 0.75;
const LATENCY_WEIGHT: f64 = 0.2;
/// Bytes of a request besides the system prompt and the user content.
const REQUEST_OVERHEAD_BYTES: usize = 512;

/// The limits of a [`LimitedJudge`].
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// Calls that may run at the same time.
    pub concurrency: usize,
    /// Calls that may start per minute.
    pub calls_per_minute: u32,
    /// US dollars that may be spent per hour.
    pub usd_per_hour: f64,
    /// The model whose price the reservation uses.
    pub model: String,
    /// The answer size the reservation assumes.
    pub max_tokens: u32,
    /// The longest a call is given. It caps the minimum budget.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 8,
            calls_per_minute: 60,
            usd_per_hour: 10.0,
            model: "claude-opus-5-5".to_owned(),
            max_tokens: 4096,
            timeout: Duration::from_secs(15),
        }
    }
}

/// A judge behind limits. Put it inside a [`crate::CachedJudge`], so cache
/// hits never count.
pub struct LimitedJudge {
    inner: Arc<dyn Judge>,
    limits: Limits,
    slots: Semaphore,
    state: Mutex<State>,
}

struct Spend {
    id: u64,
    at: Instant,
    usd: f64,
}

struct State {
    calls: VecDeque<Instant>,
    spend: VecDeque<Spend>,
    next_id: u64,
    paused_until: Option<Instant>,
    failures: u32,
    bad_request: Option<(u16, String)>,
    bad_requests: u32,
    latency_secs: f64,
}

impl State {
    fn prune(&mut self, now: Instant) {
        while self
            .calls
            .front()
            .is_some_and(|at| now.duration_since(*at) >= RATE_WINDOW)
        {
            self.calls.pop_front();
        }
        while self
            .spend
            .front()
            .is_some_and(|entry| now.duration_since(entry.at) >= SPEND_WINDOW)
        {
            self.spend.pop_front();
        }
        if self.paused_until.is_some_and(|until| now >= until) {
            self.paused_until = None;
        }
    }

    fn spent(&self) -> f64 {
        self.spend.iter().map(|entry| entry.usd).sum()
    }

    /// Sets what a call cost. `None` gives the reservation back.
    fn settle(&mut self, id: u64, usd: Option<f64>) {
        match usd {
            Some(usd) => {
                if let Some(entry) = self.spend.iter_mut().find(|entry| entry.id == id) {
                    entry.usd = usd;
                }
            }
            None => self.spend.retain(|entry| entry.id != id),
        }
    }
}

fn error(kind: JudgeErrorKind, message: &str) -> JudgeError {
    JudgeError::new(kind, message)
}

impl LimitedJudge {
    pub fn new(inner: Arc<dyn Judge>, limits: Limits) -> Self {
        let slots = Semaphore::new(limits.concurrency.max(1));
        Self {
            inner,
            limits,
            slots,
            state: Mutex::new(State {
                calls: VecDeque::new(),
                spend: VecDeque::new(),
                next_id: 0,
                paused_until: None,
                failures: 0,
                bad_request: None,
                bad_requests: 0,
                latency_secs: INITIAL_LATENCY.as_secs_f64(),
            }),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The dollars spent and reserved in the last hour.
    pub fn spent_last_hour(&self) -> f64 {
        let mut state = self.lock();
        state.prune(Instant::now());
        state.spent()
    }

    /// The circuit breaker is open: calls fail with `Paused`.
    pub fn paused(&self) -> bool {
        let mut state = self.lock();
        state.prune(Instant::now());
        state.paused_until.is_some()
    }

    /// The least time a call must have left to start: 1.2 times the usual
    /// latency, at most three quarters of the timeout, and at least 2 s
    /// when that ceiling allows it.
    pub fn min_budget(&self) -> Duration {
        let latency = self.lock().latency_secs;
        let ceiling = self.limits.timeout.mul_f64(BUDGET_CEILING);
        let floor = MIN_BUDGET_FLOOR.min(ceiling);
        Duration::from_secs_f64(latency * BUDGET_FACTOR).clamp(floor, ceiling)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update is complete before the lock is released, so the
        // state is still good after a panic elsewhere.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn check_start(&self, deadline: Instant) -> Result<(), JudgeError> {
        let now = Instant::now();
        {
            let mut state = self.lock();
            state.prune(now);
            if state.paused_until.is_some() {
                return Err(error(
                    JudgeErrorKind::Paused,
                    "the judge is paused after repeated failures",
                ));
            }
        }
        if deadline.saturating_duration_since(now) < self.min_budget() {
            return Err(error(JudgeErrorKind::Deadline, "not enough time left"));
        }
        Ok(())
    }

    async fn run(&self, question: &Question, deadline: Instant) -> Result<Verdict, JudgeError> {
        self.check_start(deadline)?;

        // A call waits for a slot as long as it could still start: callers
        // ask more questions at once than there are slots (a batch asks 16,
        // and calls overlap), and a judge call takes seconds. The slots are
        // handed out in the order of arrival.
        let last_start = deadline
            .checked_sub(self.min_budget())
            .unwrap_or_else(Instant::now);
        let _slot = match tokio::time::timeout_at(last_start, self.slots.acquire()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => {
                return Err(error(
                    JudgeErrorKind::ShuttingDown,
                    "the judge is shut down",
                ));
            }
            Err(_) => {
                return Err(error(
                    JudgeErrorKind::Busy,
                    "too many judge calls are running",
                ));
            }
        };

        // Wait once for the rate window, when a place frees up soon.
        let rate_limit = self.limits.calls_per_minute.max(1) as usize;
        let free_at = {
            let mut state = self.lock();
            state.prune(Instant::now());
            if state.calls.len() >= rate_limit {
                state
                    .calls
                    .get(state.calls.len() - rate_limit)
                    .map(|at| *at + RATE_WINDOW)
            } else {
                None
            }
        };
        if let Some(free_at) = free_at {
            if free_at.saturating_duration_since(Instant::now()) > MAX_WAIT {
                return Err(error(
                    JudgeErrorKind::RateLimited,
                    "the judge call rate limit is reached",
                ));
            }
            tokio::time::sleep_until(free_at).await;
        }

        // The waits used time, so check the breaker and the budget again.
        self.check_start(deadline)?;

        let request_bytes =
            SYSTEM_PROMPT.len() + user_content(question).len() + REQUEST_OVERHEAD_BYTES;
        let reserve = reserve_usd(&self.limits.model, request_bytes, self.limits.max_tokens);
        let started = Instant::now();
        let id = {
            let mut state = self.lock();
            state.prune(started);
            if state.calls.len() >= rate_limit {
                return Err(error(
                    JudgeErrorKind::RateLimited,
                    "the judge call rate limit is reached",
                ));
            }
            if state.spent() + reserve > self.limits.usd_per_hour {
                return Err(error(
                    JudgeErrorKind::SpendCap,
                    "the judge spend limit for this hour is reached",
                ));
            }
            state.calls.push_back(started);
            let id = state.next_id;
            state.next_id += 1;
            state.spend.push_back(Spend {
                id,
                at: started,
                usd: reserve,
            });
            id
        };

        // If this future is dropped here, the reservation stays: the call
        // may still be billed.
        let result =
            match tokio::time::timeout_at(deadline, self.inner.judge(question, deadline)).await {
                Ok(result) => result,
                Err(_) => Err(error(JudgeErrorKind::Deadline, "judge timed out")),
            };
        self.record(id, started.elapsed(), &result);
        result
    }

    fn record(&self, id: u64, latency: Duration, result: &Result<Verdict, JudgeError>) {
        let now = Instant::now();
        let mut state = self.lock();
        match result {
            Ok(verdict) => {
                let model = verdict
                    .source
                    .strip_prefix("anthropic/")
                    .unwrap_or(&self.limits.model);
                state.settle(id, Some(cost_usd(model, &verdict.usage)));
                state.failures = 0;
                state.bad_request = None;
                state.bad_requests = 0;
                state.latency_secs = state.latency_secs * (1.0 - LATENCY_WEIGHT)
                    + latency.as_secs_f64() * LATENCY_WEIGHT;
            }
            Err(failure) => {
                match (failure.usage(), failure.kind()) {
                    // A billed failure: a refusal, a cut-off or a bad answer.
                    (Some(usage), _) => {
                        state.settle(id, Some(cost_usd(&self.limits.model, &usage)));
                    }
                    // The call may have run to the end on the other side.
                    (None, JudgeErrorKind::Deadline | JudgeErrorKind::Transport) => {}
                    (None, _) => state.settle(id, None),
                }
                self.count_failure(&mut state, now, failure);
            }
        }
    }

    fn count_failure(&self, state: &mut State, now: Instant, failure: &JudgeError) {
        let status = failure.http_status();
        if failure.kind() == JudgeErrorKind::Api && matches!(status, Some(401 | 403 | 404)) {
            state.paused_until = Some(now + LONG_PAUSE);
            state.failures = 0;
            tracing::error!(
                judge = self.inner.id(),
                status,
                pause_secs = LONG_PAUSE.as_secs(),
                "the judge is paused: the API rejected the key or the model"
            );
            return;
        }
        if let (JudgeErrorKind::Api, Some(status @ (400 | 402))) = (failure.kind(), status) {
            let signature = (status, failure.message().to_owned());
            if state.bad_request.as_ref() == Some(&signature) {
                state.bad_requests += 1;
            } else {
                state.bad_request = Some(signature);
                state.bad_requests = 1;
            }
            if state.bad_requests >= BAD_REQUESTS_TO_PAUSE {
                state.paused_until = Some(now + LONG_PAUSE);
                state.bad_request = None;
                state.bad_requests = 0;
                tracing::error!(
                    judge = self.inner.id(),
                    status,
                    error = failure.message(),
                    pause_secs = LONG_PAUSE.as_secs(),
                    "the judge is paused: the API rejected the same request again and again"
                );
            }
            return;
        }
        state.bad_request = None;
        state.bad_requests = 0;
        if failure.is_retryable() {
            // A deadline counts: no call starts without the full minimum
            // budget, so a late answer here is the service being slow.
            state.failures += 1;
            if state.failures >= FAILURES_TO_PAUSE {
                state.paused_until = Some(now + SHORT_PAUSE);
                state.failures = 0;
                tracing::warn!(
                    judge = self.inner.id(),
                    pause_secs = SHORT_PAUSE.as_secs(),
                    "the judge is paused after repeated failures"
                );
            }
        } else {
            // The API answered, so it is up.
            state.failures = 0;
        }
    }
}

impl fmt::Debug for LimitedJudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LimitedJudge")
            .field("inner", &self.inner)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Judge for LimitedJudge {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn judge<'a>(
        &'a self,
        question: &'a Question,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Verdict, JudgeError>> {
        Box::pin(self.run(question, deadline))
    }

    fn peek(&self, question: &Question) -> Option<Verdict> {
        self.inner.peek(question)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;
    use crate::testing::*;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn limited(inner: ScriptedJudge, limits: Limits) -> (Arc<ScriptedJudge>, Arc<LimitedJudge>) {
        let inner = Arc::new(inner);
        let judge = Arc::new(LimitedJudge::new(inner.clone(), limits));
        (inner, judge)
    }

    fn api(status: u16, message: &str) -> JudgeError {
        JudgeError::new(JudgeErrorKind::Api, message).with_http_status(status)
    }

    fn fail(error: JudgeError) -> Step {
        Step {
            delay: Some(Duration::ZERO),
            result: Err(error),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn reserve_for(q: &Question, limits: &Limits) -> f64 {
        let bytes = SYSTEM_PROMPT.len() + user_content(q).len() + REQUEST_OVERHEAD_BYTES;
        reserve_usd(&limits.model, bytes, limits.max_tokens)
    }

    #[test]
    fn the_defaults_match_the_design() {
        let limits = Limits::default();
        assert_eq!(limits.concurrency, 8);
        assert_eq!(limits.calls_per_minute, 60);
        assert_eq!(limits.usd_per_hour, 10.0);
        assert_eq!(limits.max_tokens, 4096);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_house_gives_busy_after_a_bounded_wait() {
        let limits = Limits {
            concurrency: 2,
            ..Limits::default()
        };
        let (inner, judge) = limited(ScriptedJudge::slow(Duration::from_secs(10)), limits);
        let mut running = Vec::new();
        for n in 0..2 {
            let judge = judge.clone();
            running.push(tokio::spawn(async move {
                judge.judge(&question(n), soon()).await
            }));
        }
        tokio::task::yield_now().await;
        // The wait ends when the call could no longer start in time: its
        // deadline minus the minimum budget.
        let start = Instant::now();
        let deadline = start + Duration::from_secs(9);
        let wait = Duration::from_secs(9) - judge.min_budget();
        assert!(wait > Duration::ZERO && wait < Duration::from_secs(9));
        let error = judge.judge(&question(9), deadline).await.expect_err("busy");
        assert_eq!(error.kind(), JudgeErrorKind::Busy);
        assert_eq!(error.message(), "too many judge calls are running");
        assert_eq!(start.elapsed(), wait);
        assert_eq!(inner.calls(), 2);
        for task in running {
            task.await.expect("join").expect("verdict");
        }
        // The slots are free again.
        judge.judge(&question(9), soon()).await.expect("verdict");
    }

    /// The server defaults: a batch asks 16 questions at once and 8 run
    /// at a time. The second eight wait for the first (they used to be
    /// refused after 500 ms).
    #[tokio::test(start_paused = true)]
    async fn a_full_batch_of_questions_waits_for_the_slots() {
        let limits = Limits::default();
        assert_eq!(limits.concurrency, 8);
        let timeout = limits.timeout;
        let (inner, judge) = limited(ScriptedJudge::slow(INITIAL_LATENCY), limits);
        let start = Instant::now();
        let deadline = start + timeout;
        let mut asked = tokio::task::JoinSet::new();
        for n in 0..16 {
            let judge = judge.clone();
            asked.spawn(async move { crate::ask(&*judge, &question(n), deadline).await });
        }
        let mut answered = 0;
        while let Some(joined) = asked.join_next().await {
            joined.expect("join").expect("verdict");
            answered += 1;
        }
        assert_eq!(answered, 16);
        assert_eq!(inner.calls(), 16);
        // Two waves of eight.
        assert_eq!(start.elapsed(), INITIAL_LATENCY * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_that_frees_up_soon_is_waited_for() {
        let limits = Limits {
            concurrency: 1,
            ..Limits::default()
        };
        let (inner, judge) = limited(ScriptedJudge::slow(Duration::from_millis(300)), limits);
        let first = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        judge.judge(&question(2), soon()).await.expect("second");
        first.await.expect("join").expect("first");
        assert_eq!(inner.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_call_rate_is_limited_per_minute() {
        let limits = Limits {
            calls_per_minute: 3,
            ..Limits::default()
        };
        let (inner, judge) = limited(ScriptedJudge::ok(), limits);
        for n in 0..3 {
            judge.judge(&question(n), soon()).await.expect("verdict");
        }
        let start = Instant::now();
        let error = judge.judge(&question(3), soon()).await.expect_err("limit");
        assert_eq!(error.kind(), JudgeErrorKind::RateLimited);
        assert!(start.elapsed() <= MAX_WAIT);
        assert_eq!(inner.calls(), 3);

        // 300 ms before the window moves on, the call waits and runs.
        tokio::time::advance(RATE_WINDOW - Duration::from_millis(300)).await;
        let start = Instant::now();
        judge.judge(&question(3), soon()).await.expect("verdict");
        assert_eq!(start.elapsed(), Duration::from_millis(300));
        assert_eq!(inner.calls(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_reservation_becomes_the_actual_cost() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            ..Usage::default()
        };
        inner.push(Step {
            delay: Some(Duration::from_secs(1)),
            result: Ok(Verdict::new(
                assessment_core::model::TextRelation::Replaced,
                0.9,
                "x",
                "anthropic/claude-opus-5-5",
            )
            .with_usage(usage)),
        });
        let call = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        // While the call runs, the full reservation counts.
        let reserve = reserve_for(&question(1), judge.limits());
        assert!(close(judge.spent_last_hour(), reserve));
        assert!(reserve > 4096.0 * 20.0 / 1e6);
        call.await.expect("join").expect("verdict");
        // 1000 * $4 + 100 * $20, per million tokens.
        assert!(close(judge.spent_last_hour(), 0.006));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fallback_model_is_charged_at_the_unknown_price() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            ..Usage::default()
        };
        inner.push(Step {
            delay: Some(Duration::ZERO),
            result: Ok(Verdict::new(
                assessment_core::model::TextRelation::Replaced,
                0.9,
                "x",
                "anthropic/claude-other-9",
            )
            .with_usage(usage)),
        });
        judge.judge(&question(1), soon()).await.expect("verdict");
        // 1000 * $10 + 100 * $50, per million tokens.
        assert!(close(judge.spent_last_hour(), 0.015));
    }

    #[tokio::test(start_paused = true)]
    async fn billed_failures_count_and_unbilled_ones_are_given_back() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        let usage = Usage {
            input_tokens: 2000,
            output_tokens: 0,
            ..Usage::default()
        };
        for kind in [
            JudgeErrorKind::Refused,
            JudgeErrorKind::Truncated,
            JudgeErrorKind::InvalidOutput,
        ] {
            inner.push(fail(JudgeError::new(kind, "x").with_usage(usage)));
        }
        for n in 0..3 {
            judge.judge(&question(n), soon()).await.expect_err("fails");
        }
        // 3 * 2000 * $4 per million tokens.
        assert!(close(judge.spent_last_hour(), 0.024));

        // An error status with no usage was not billed.
        inner.push(fail(api(400, "HTTP 400 invalid_request_error")));
        inner.push(fail(api(500, "HTTP 500 api_error")));
        for n in 0..2 {
            judge.judge(&question(n), soon()).await.expect_err("fails");
        }
        assert!(close(judge.spent_last_hour(), 0.024));
    }

    #[tokio::test(start_paused = true)]
    async fn the_reservation_stays_after_a_timeout_or_a_cancel() {
        let limits = Limits::default();
        let reserve = reserve_for(&question(1), &limits);
        let (inner, judge) = limited(ScriptedJudge::ok(), limits);

        // A call that never answers runs into its deadline.
        inner.push(Step {
            delay: None,
            result: Ok(verdict()),
        });
        let start = Instant::now();
        let error = judge
            .judge(&question(1), start + Duration::from_secs(10))
            .await
            .expect_err("late");
        assert_eq!(error.kind(), JudgeErrorKind::Deadline);
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert!(close(judge.spent_last_hour(), reserve));

        // A connection error may also have been billed.
        inner.push(fail(JudgeError::new(JudgeErrorKind::Transport, "reset")));
        judge.judge(&question(1), soon()).await.expect_err("reset");
        assert!(close(judge.spent_last_hour(), 2.0 * reserve));

        // A caller that goes away leaves its reservation too.
        inner.push(Step {
            delay: None,
            result: Ok(verdict()),
        });
        let call = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        call.abort();
        assert!(call.await.expect_err("aborted").is_cancelled());
        assert!(close(judge.spent_last_hour(), 3.0 * reserve));

        // After an hour the window is empty again.
        tokio::time::advance(SPEND_WINDOW).await;
        assert!(close(judge.spent_last_hour(), 0.0));
    }

    #[tokio::test(start_paused = true)]
    async fn calls_are_refused_when_the_hour_is_spent() {
        let reserve = reserve_for(&question(1), &Limits::default());
        let limits = Limits {
            // Room for two reservations, not for three.
            usd_per_hour: reserve * 2.5,
            ..Limits::default()
        };
        let (inner, judge) = limited(ScriptedJudge::ok(), limits);
        for _ in 0..2 {
            inner.push(fail(JudgeError::new(JudgeErrorKind::Transport, "reset")));
        }
        for _ in 0..2 {
            judge.judge(&question(1), soon()).await.expect_err("reset");
        }
        let error = judge.judge(&question(1), soon()).await.expect_err("cap");
        assert_eq!(error.kind(), JudgeErrorKind::SpendCap);
        assert_eq!(inner.calls(), 2);
        // A refused call reserves nothing and does not use the rate window.
        assert!(close(judge.spent_last_hour(), 2.0 * reserve));

        tokio::time::advance(SPEND_WINDOW).await;
        judge.judge(&question(1), soon()).await.expect("verdict");
        assert_eq!(inner.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn five_retryable_failures_in_a_row_pause_for_30_seconds() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        let failures = [
            api(500, "HTTP 500"),
            api(529, "HTTP 529"),
            api(429, "HTTP 429"),
            JudgeError::new(JudgeErrorKind::Transport, "reset"),
            JudgeError::new(JudgeErrorKind::Deadline, "judge timed out"),
        ];
        for failure in failures {
            assert!(!judge.paused());
            inner.push(fail(failure));
            judge.judge(&question(1), soon()).await.expect_err("fails");
        }
        assert!(judge.paused());
        let error = judge.judge(&question(1), soon()).await.expect_err("paused");
        assert_eq!(error.kind(), JudgeErrorKind::Paused);
        assert_eq!(inner.calls(), 5);

        tokio::time::advance(SHORT_PAUSE - Duration::from_millis(1)).await;
        assert!(judge.paused());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!judge.paused());
        judge.judge(&question(1), soon()).await.expect("verdict");
        assert_eq!(inner.calls(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_resets_the_failure_count() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        for round in 0..3 {
            for _ in 0..4 {
                inner.push(fail(api(503, "HTTP 503")));
                judge.judge(&question(1), soon()).await.expect_err("fails");
            }
            judge.judge(&question(1), soon()).await.expect("verdict");
            assert!(!judge.paused(), "round {round}");
        }
        // A refusal is an answer too: the API is up.
        for _ in 0..4 {
            inner.push(fail(api(503, "HTTP 503")));
            judge.judge(&question(1), soon()).await.expect_err("fails");
        }
        inner.push(fail(JudgeError::new(JudgeErrorKind::Refused, "refused")));
        judge
            .judge(&question(1), soon())
            .await
            .expect_err("refused");
        inner.push(fail(api(503, "HTTP 503")));
        judge.judge(&question(1), soon()).await.expect_err("fails");
        assert!(!judge.paused());
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_key_or_model_pauses_for_300_seconds() {
        for status in [401, 403, 404] {
            let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
            inner.push(fail(api(status, "HTTP error")));
            judge.judge(&question(1), soon()).await.expect_err("fails");
            assert!(judge.paused(), "{status}");
            tokio::time::advance(LONG_PAUSE - Duration::from_millis(1)).await;
            assert!(judge.paused(), "{status}");
            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(!judge.paused(), "{status}");
            judge.judge(&question(1), soon()).await.expect("verdict");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn three_identical_bad_requests_pause_for_300_seconds() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        // Different messages do not add up.
        for message in ["HTTP 400 a", "HTTP 400 b", "HTTP 400 a", "HTTP 400 b"] {
            inner.push(fail(api(400, message)));
            judge.judge(&question(1), soon()).await.expect_err("fails");
        }
        assert!(!judge.paused());
        // The same message with another status does not add up either.
        inner.push(fail(api(402, "HTTP 400 b")));
        judge.judge(&question(1), soon()).await.expect_err("fails");
        assert!(!judge.paused());
        for _ in 0..2 {
            inner.push(fail(api(402, "HTTP 400 b")));
            judge.judge(&question(1), soon()).await.expect_err("fails");
        }
        assert!(judge.paused());
        tokio::time::advance(LONG_PAUSE).await;
        assert!(!judge.paused());
    }

    #[tokio::test(start_paused = true)]
    async fn no_call_starts_without_the_minimum_budget() {
        let (inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        // 1.2 * 6 s to begin with.
        assert_eq!(judge.min_budget(), Duration::from_secs_f64(7.2));
        let error = judge
            .judge(&question(1), Instant::now() + Duration::from_secs(7))
            .await
            .expect_err("too little time");
        assert_eq!(error.kind(), JudgeErrorKind::Deadline);
        assert_eq!(error.message(), "not enough time left");
        assert_eq!(inner.calls(), 0);
        assert!(close(judge.spent_last_hour(), 0.0));

        // Such refusals are not failures of the service.
        for _ in 0..10 {
            judge
                .judge(&question(1), Instant::now() + Duration::from_secs(1))
                .await
                .expect_err("too little time");
        }
        assert!(!judge.paused());

        // Fast answers bring the budget down, but not below 2 s.
        for _ in 0..40 {
            judge.judge(&question(1), soon()).await.expect("verdict");
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        assert_eq!(judge.min_budget(), MIN_BUDGET_FLOOR);
        judge
            .judge(&question(1), Instant::now() + Duration::from_secs(3))
            .await
            .expect("verdict");
    }

    #[tokio::test(start_paused = true)]
    async fn the_minimum_budget_stays_below_the_timeout() {
        let limits = Limits {
            timeout: Duration::from_secs(10),
            ..Limits::default()
        };
        let (_inner, judge) = limited(ScriptedJudge::slow(Duration::from_secs(20)), limits);
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_secs(60);
            judge.judge(&question(1), deadline).await.expect("verdict");
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert_eq!(judge.min_budget(), Duration::from_secs_f64(7.5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_with_the_whole_timeout_ahead_always_starts() {
        // Short timeouts, with the first latency guess above them.
        for secs in [1, 5, 7, 8, 15] {
            let timeout = Duration::from_secs(secs);
            let limits = Limits {
                timeout,
                ..Limits::default()
            };
            let (inner, judge) = limited(ScriptedJudge::ok(), limits);
            let deadline = Instant::now() + timeout;
            // The caller reads the clock before the call starts.
            tokio::time::advance(timeout / 10).await;
            judge
                .judge(&question(1), deadline)
                .await
                .expect("the call starts");
            assert_eq!(inner.calls(), 1, "timeout {secs} s");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_latency_near_the_timeout_does_not_lock_the_judge_out() {
        let timeout = Duration::from_secs(8);
        let limits = Limits {
            timeout,
            ..Limits::default()
        };
        let (inner, judge) = limited(ScriptedJudge::slow(Duration::from_millis(7500)), limits);
        for i in 0..30 {
            let deadline = Instant::now() + timeout;
            tokio::time::advance(Duration::from_millis(100)).await;
            judge
                .judge(&question(1), deadline)
                .await
                .expect("the call starts and answers");
            assert_eq!(inner.calls(), i + 1);
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert!(judge.min_budget() < timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn peek_and_id_come_from_the_inner_judge() {
        let (_inner, judge) = limited(ScriptedJudge::ok(), Limits::default());
        assert_eq!(judge.id(), "fake/model");
        assert!(judge.peek(&question(1)).is_none());
        assert!(format!("{judge:?}").contains("LimitedJudge"));
    }
}
