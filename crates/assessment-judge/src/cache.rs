//! A cache in front of a judge.
//!
//! Only successful answers are kept. Identical questions asked at the same
//! time make one call: the first caller asks, the others wait for its
//! answer.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;
use tokio::time::Instant;

use crate::prompt::{PROMPT_VERSION, user_content};
use crate::{BoxFuture, Judge, JudgeError, JudgeErrorKind, Question, Verdict};

/// The default number of cached answers.
pub const DEFAULT_CACHE_ENTRIES: usize = 4096;

/// Two 64-bit digests of (judge id, prompt version, user content).
type Key = (u64, u64);

type Outcome = Option<Result<Verdict, JudgeError>>;

/// A judge with a cache of its answers, least recently used first out.
pub struct CachedJudge {
    inner: Arc<dyn Judge>,
    capacity: usize,
    // The keys are random per process, so a caller cannot craft two texts
    // that share a cache entry.
    hashers: (RandomState, RandomState),
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, (Verdict, u64)>,
    /// Use order: the smallest tick is the least recently used entry.
    order: BTreeMap<u64, Key>,
    tick: u64,
    in_flight: HashMap<Key, watch::Receiver<Outcome>>,
}

impl State {
    fn get(&mut self, key: &Key) -> Option<Verdict> {
        self.tick += 1;
        let tick = self.tick;
        let (verdict, used) = self.entries.get_mut(key)?;
        self.order.remove(used);
        *used = tick;
        self.order.insert(tick, *key);
        Some(verdict.clone())
    }

    fn insert(&mut self, key: Key, verdict: Verdict, capacity: usize) {
        self.tick += 1;
        if let Some((_, used)) = self.entries.remove(&key) {
            self.order.remove(&used);
        }
        while self.entries.len() >= capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (verdict, self.tick));
        self.order.insert(self.tick, key);
    }
}

/// Removes the in-flight entry when the asking call ends, also when it is
/// cancelled. Waiting callers then see the channel close and ask again.
struct FlightGuard<'a> {
    judge: &'a CachedJudge,
    key: Key,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.judge.lock().in_flight.remove(&self.key);
    }
}

enum Role {
    Leader(watch::Sender<Outcome>),
    Follower(watch::Receiver<Outcome>),
}

impl CachedJudge {
    /// A cache of [`DEFAULT_CACHE_ENTRIES`] answers.
    pub fn new(inner: Arc<dyn Judge>) -> Self {
        Self::with_capacity(inner, DEFAULT_CACHE_ENTRIES)
    }

    /// A cache of `capacity` answers. With 0 the cache is off and every
    /// question goes to the inner judge.
    pub fn with_capacity(inner: Arc<dyn Judge>, capacity: usize) -> Self {
        Self {
            inner,
            capacity,
            hashers: (RandomState::new(), RandomState::new()),
            state: Mutex::new(State::default()),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of cached answers.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent if a holder panics: every update is
        // complete before the lock is released.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn key(&self, question: &Question) -> Key {
        let content = user_content(question);
        let digest = |state: &RandomState| {
            let mut hasher = state.build_hasher();
            let id = self.inner.id();
            hasher.write_usize(id.len());
            hasher.write(id.as_bytes());
            hasher.write_u32(PROMPT_VERSION);
            hasher.write_usize(content.len());
            hasher.write(content.as_bytes());
            hasher.finish()
        };
        (digest(&self.hashers.0), digest(&self.hashers.1))
    }

    async fn run(&self, question: &Question, deadline: Instant) -> Result<Verdict, JudgeError> {
        if self.capacity == 0 {
            return self.inner.judge(question, deadline).await;
        }
        let key = self.key(question);
        loop {
            let role = {
                let mut state = self.lock();
                if let Some(verdict) = state.get(&key) {
                    return Ok(verdict.cached());
                }
                match state.in_flight.get(&key) {
                    Some(receiver) => Role::Follower(receiver.clone()),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        state.in_flight.insert(key, receiver);
                        Role::Leader(sender)
                    }
                }
            };
            match role {
                Role::Leader(sender) => {
                    let guard = FlightGuard { judge: self, key };
                    let result = self.inner.judge(question, deadline).await;
                    if let Ok(verdict) = &result {
                        self.lock().insert(key, verdict.clone(), self.capacity);
                    }
                    drop(guard);
                    // No waiting caller is not an error.
                    let _ = sender.send(Some(result.clone()));
                    return result;
                }
                Role::Follower(mut receiver) => {
                    let outcome = {
                        let wait = receiver.wait_for(Option::is_some);
                        match tokio::time::timeout_at(deadline, wait).await {
                            Err(_) => {
                                return Err(JudgeError::new(
                                    JudgeErrorKind::Deadline,
                                    "judge timed out",
                                ));
                            }
                            // The asking call was cancelled.
                            Ok(Err(_)) => None,
                            Ok(Ok(value)) => value.clone(),
                        }
                    };
                    match outcome {
                        Some(Ok(verdict)) => return Ok(verdict.cached()),
                        // The asking call ran out of its own time. This
                        // caller may have more, so it asks again.
                        Some(Err(error)) if error.kind() == JudgeErrorKind::Deadline => {}
                        Some(Err(mut error)) => {
                            // The usage belongs to the call that paid.
                            error.usage = None;
                            return Err(error);
                        }
                        None => {}
                    }
                    if Instant::now() >= deadline {
                        return Err(JudgeError::new(JudgeErrorKind::Deadline, "judge timed out"));
                    }
                }
            }
        }
    }
}

impl fmt::Debug for CachedJudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The texts are not kept, and the answers are not printed.
        f.debug_struct("CachedJudge")
            .field("inner", &self.inner)
            .field("capacity", &self.capacity)
            .field("len", &self.len())
            .finish()
    }
}

impl Judge for CachedJudge {
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
        if self.capacity == 0 {
            return None;
        }
        let key = self.key(question);
        self.lock().get(&key).map(Verdict::cached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;
    use crate::testing::*;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn cached(inner: ScriptedJudge, capacity: usize) -> (Arc<ScriptedJudge>, Arc<CachedJudge>) {
        let inner = Arc::new(inner);
        let judge = Arc::new(CachedJudge::with_capacity(inner.clone(), capacity));
        (inner, judge)
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeated_question_is_answered_from_the_cache() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        inner.push(Step {
            delay: Some(Duration::ZERO),
            result: Ok(verdict().with_usage(Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            })),
        });
        assert!(judge.peek(&question(1)).is_none());
        let first = judge.judge(&question(1), soon()).await.expect("first");
        assert!(!first.cached);
        assert_eq!(first.usage.input_tokens, 100);
        let second = judge.judge(&question(1), soon()).await.expect("second");
        assert!(second.cached);
        assert!(second.usage.is_zero());
        assert_eq!(second.relation, first.relation);
        assert_eq!(second.rationale, first.rationale);
        assert_eq!(inner.calls(), 1);
        let peeked = judge.peek(&question(1)).expect("peek");
        assert!(peeked.cached);
        assert_eq!(judge.len(), 1);
        assert_eq!(judge.id(), "fake/model");
    }

    #[tokio::test(start_paused = true)]
    async fn the_least_recently_used_answer_is_dropped_first() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 2);
        judge.judge(&question(1), soon()).await.expect("1");
        judge.judge(&question(2), soon()).await.expect("2");
        // Use 1, so 2 is now the oldest.
        judge.judge(&question(1), soon()).await.expect("1 again");
        judge.judge(&question(3), soon()).await.expect("3");
        assert_eq!(inner.calls(), 3);
        assert_eq!(judge.len(), 2);
        assert!(judge.peek(&question(1)).is_some());
        assert!(judge.peek(&question(2)).is_none());
        assert!(judge.peek(&question(3)).is_some());
        judge.judge(&question(2), soon()).await.expect("2 again");
        assert_eq!(inner.calls(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn ten_identical_questions_make_one_call() {
        let (inner, judge) = cached(ScriptedJudge::slow(Duration::from_secs(1)), 8);
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let judge = judge.clone();
            tasks.push(tokio::spawn(async move {
                judge.judge(&question(1), soon()).await
            }));
        }
        let mut fresh = 0;
        for task in tasks {
            let verdict = task.await.expect("join").expect("verdict");
            if !verdict.cached {
                fresh += 1;
            }
        }
        assert_eq!(inner.calls(), 1);
        assert_eq!(fresh, 1);
        assert!(judge.lock().in_flight.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn different_questions_do_not_share_a_call() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        judge.judge(&question(1), soon()).await.expect("1");
        judge.judge(&question(2), soon()).await.expect("2");
        let mut titled = question(1);
        titled.context_title = Some("Checkout".to_owned());
        judge.judge(&titled, soon()).await.expect("titled");
        assert_eq!(inner.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_not_cached() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        inner.push(Step {
            delay: Some(Duration::ZERO),
            result: Err(JudgeError::new(JudgeErrorKind::Api, "HTTP 500").with_http_status(500)),
        });
        let error = judge.judge(&question(1), soon()).await.expect_err("fails");
        assert_eq!(error.http_status(), Some(500));
        assert!(judge.is_empty());
        assert!(judge.peek(&question(1)).is_none());
        let verdict = judge.judge(&question(1), soon()).await.expect("works");
        assert!(!verdict.cached);
        assert_eq!(inner.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_callers_get_the_failure_without_its_usage() {
        let usage = Usage {
            input_tokens: 50,
            ..Usage::default()
        };
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        inner.push(Step {
            delay: Some(Duration::from_secs(1)),
            result: Err(JudgeError::new(JudgeErrorKind::Refused, "refused").with_usage(usage)),
        });
        let leader = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        let follower = judge.judge(&question(1), soon()).await.expect_err("fails");
        assert_eq!(follower.kind(), JudgeErrorKind::Refused);
        assert_eq!(follower.usage(), None);
        let leader = leader.await.expect("join").expect_err("fails");
        assert_eq!(leader.usage(), Some(usage));
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_call_leaves_nothing_behind() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        inner.push(Step {
            delay: None,
            result: Ok(verdict()),
        });
        let leader = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(judge.lock().in_flight.len(), 1);
        // A second caller waits for the first one.
        let follower = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(inner.calls(), 1);
        leader.abort();
        assert!(leader.await.expect_err("aborted").is_cancelled());
        // The waiting caller takes over and asks itself.
        let verdict = follower.await.expect("join").expect("verdict");
        assert!(!verdict.cached);
        assert_eq!(inner.calls(), 2);
        assert!(judge.lock().in_flight.is_empty());
        assert_eq!(judge.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_caller_keeps_its_own_deadline() {
        let (inner, judge) = cached(ScriptedJudge::slow(Duration::from_secs(30)), 8);
        let leader = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        let start = Instant::now();
        let error = judge
            .judge(&question(1), start + Duration::from_secs(2))
            .await
            .expect_err("late");
        assert_eq!(error.kind(), JudgeErrorKind::Deadline);
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        assert!(!leader.await.expect("join").expect("verdict").cached);
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_caller_asks_again_when_the_first_ran_out_of_time() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 8);
        inner.push(Step {
            delay: Some(Duration::from_secs(1)),
            result: Err(JudgeError::new(JudgeErrorKind::Deadline, "judge timed out")),
        });
        let leader = {
            let judge = judge.clone();
            tokio::spawn(async move { judge.judge(&question(1), soon()).await })
        };
        tokio::task::yield_now().await;
        let verdict = judge.judge(&question(1), soon()).await.expect("verdict");
        assert!(!verdict.cached);
        assert_eq!(inner.calls(), 2);
        assert_eq!(
            leader.await.expect("join").expect_err("late").kind(),
            JudgeErrorKind::Deadline
        );
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_zero_turns_the_cache_off() {
        let (inner, judge) = cached(ScriptedJudge::ok(), 0);
        judge.judge(&question(1), soon()).await.expect("1");
        let again = judge.judge(&question(1), soon()).await.expect("1 again");
        assert!(!again.cached);
        assert_eq!(inner.calls(), 2);
        assert!(judge.peek(&question(1)).is_none());
        assert!(judge.is_empty());
    }

    #[test]
    fn the_default_capacity_is_4096() {
        let judge = CachedJudge::new(Arc::new(ScriptedJudge::ok()));
        assert_eq!(judge.capacity(), 4096);
        assert!(format!("{judge:?}").contains("CachedJudge"));
    }
}
