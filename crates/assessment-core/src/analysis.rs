//! Text analysis that is kept between assessments.
//!
//! An assessment reads the text of every context entity it can reach: the
//! term counts of its document (title and description) and of its title.
//! The counts depend only on that text and on the text policy, so the
//! [registry](crate::registry) keeps them per stored entity
//! ([`StatsCell`]) and hands them to [`Engine::assess_prepared`]
//! ([`PreparedStats`]). A put on a stored graph then analyses only the text
//! that changed.
//!
//! Only term counts are kept. Document frequencies (IDF) are still counted
//! per request, over the same texts as without the cache, so an assessment
//! is the same with and without it, bit for bit.
//!
//! [`Engine::assess_prepared`]: crate::engine::Engine::assess_prepared

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use crate::model::Entity;
use crate::policy::Policy;
use crate::text::{self, Lexicon};

/// The version of the term analysis ([`text::term_counts`]). Bump it when
/// the same text and policy can give other counts, so stats computed by an
/// older analysis are never used.
pub(crate) const ANALYZER_VERSION: u32 = 1;

/// The term counts of one entity's text, as an assessment needs them for a
/// context node.
#[derive(Debug)]
pub(crate) struct TextStats {
    /// Term counts of [`Entity::document`].
    pub document: BTreeMap<String, u32>,
    /// Term counts of the trimmed title (empty for an empty title).
    pub title: BTreeMap<String, u32>,
    /// The analysis that computed the counts.
    analyzer: u32,
    /// The policy whose `text` section the counts were computed with.
    policy: Arc<Policy>,
}

impl TextStats {
    /// Analyses the entity's document and title.
    pub(crate) fn of(entity: &Entity, policy: &Arc<Policy>, lexicon: &Lexicon<'_>) -> Self {
        let title = entity.title().trim();
        Self {
            document: text::term_counts(&entity.document(), lexicon),
            title: if title.is_empty() {
                BTreeMap::new()
            } else {
                text::term_counts(title, lexicon)
            },
            analyzer: ANALYZER_VERSION,
            policy: Arc::clone(policy),
        }
    }

    /// Whether the counts are those `policy` would give: the same analysis
    /// and the same `text` section.
    fn fits(&self, policy: &Arc<Policy>) -> bool {
        self.analyzer == ANALYZER_VERSION && same_text_policy(&self.policy, policy)
    }
}

/// Whether two policies analyse text the same way.
pub(crate) fn same_text_policy(a: &Arc<Policy>, b: &Arc<Policy>) -> bool {
    Arc::ptr_eq(a, b) || a.text == b.text
}

/// Whether two entities have the same analysed text (title and description).
pub(crate) fn same_text(a: &Entity, b: &Entity) -> bool {
    use crate::model::field::{DESCRIPTION, TITLE};
    a.text(TITLE).unwrap_or("") == b.text(TITLE).unwrap_or("")
        && a.text(DESCRIPTION).unwrap_or("") == b.text(DESCRIPTION).unwrap_or("")
}

/// The place where the stats of one version of an entity's text are kept.
/// It starts empty and is filled on first use. The owner replaces the cell
/// when the text changes; the cell itself checks the analysis version and
/// the text policy.
#[derive(Debug, Default)]
pub(crate) struct StatsCell(Mutex<Option<Arc<TextStats>>>);

impl StatsCell {
    /// The stats that fit `policy`, if they are there.
    fn get(&self, policy: &Arc<Policy>) -> Option<Arc<TextStats>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|s| s.fits(policy))
            .cloned()
    }

    /// The stats of `entity` under `policy`: the kept ones, or computed now
    /// and kept. Stats of another analysis or text policy are replaced. The
    /// analysis runs without the cell's lock.
    pub(crate) fn stats(
        &self,
        entity: &Entity,
        policy: &Arc<Policy>,
        lexicon: &Lexicon<'_>,
    ) -> Arc<TextStats> {
        if let Some(stats) = self.get(policy) {
            return stats;
        }
        let stats = Arc::new(TextStats::of(entity, policy, lexicon));
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&stats));
        stats
    }

    /// Whether stats are kept (of any policy).
    #[cfg(test)]
    pub(crate) fn is_filled(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// The cells that go with one request: where the engine finds, and leaves,
/// the stats of the request's entities.
///
/// The caller promises that every cell belongs to the text of the entity it
/// is given for: a context cell to the context entity of that id, and a
/// change's cells to its `before` and `after` snapshots as sent.
#[derive(Debug)]
pub(crate) struct PreparedStats<'a> {
    /// The policy the cells are kept for. An engine with another `text`
    /// section ignores the cells and leaves them alone.
    policy: &'a Arc<Policy>,
    context: HashMap<&'a str, &'a StatsCell>,
    /// By index of the change in the request.
    before: Vec<Option<&'a StatsCell>>,
    after: Vec<Option<&'a StatsCell>>,
}

impl<'a> PreparedStats<'a> {
    /// No cells yet, for a registry that keeps its stats under `policy`.
    pub(crate) fn new(policy: &'a Arc<Policy>) -> Self {
        Self {
            policy,
            context: HashMap::new(),
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    /// The cell of the context entity `id`.
    pub(crate) fn context(&mut self, id: &'a str, cell: &'a StatsCell) {
        self.context.insert(id, cell);
    }

    /// The cells of the snapshots of change number `index`.
    pub(crate) fn change(
        &mut self,
        index: usize,
        before: Option<&'a StatsCell>,
        after: Option<&'a StatsCell>,
    ) {
        if self.before.len() <= index {
            self.before.resize(index + 1, None);
            self.after.resize(index + 1, None);
        }
        self.before[index] = before;
        self.after[index] = after;
    }

    /// Whether an engine with `policy` can use these cells.
    pub(crate) fn usable_by(&self, policy: &Arc<Policy>) -> bool {
        same_text_policy(self.policy, policy)
    }

    pub(crate) fn policy(&self) -> &'a Arc<Policy> {
        self.policy
    }

    pub(crate) fn context_cell(&self, id: &str) -> Option<&'a StatsCell> {
        self.context.get(id).copied()
    }

    pub(crate) fn before_cell(&self, index: usize) -> Option<&'a StatsCell> {
        self.before.get(index).copied().flatten()
    }

    pub(crate) fn after_cell(&self, index: usize) -> Option<&'a StatsCell> {
        self.after.get(index).copied().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::model::{AssessmentRequest, ChangeSet};
    use crate::text::analysed;

    fn child(i: usize) -> Entity {
        Entity::new(format!("ENG-{i}"), "sub_issue")
            .with_parent("ENG-0")
            .with_title(format!("Password reset email flow {i}"))
            .with_description("Send the reset link by email.\n- [ ] The link expires after a day")
    }

    /// A rewrite of the parent `ENG-0`, with eight children as context.
    fn request() -> AssessmentRequest {
        let before = Entity::new("ENG-0", "issue")
            .with_title("Authentication")
            .with_description("- [ ] Email and password login\n- [ ] Password reset by email");
        let after = before
            .clone()
            .with_description("- [ ] Google OAuth login only\nPassword reset is not needed.");
        AssessmentRequest {
            changes: vec![ChangeSet::update(before, after)],
            context: (1..=8).map(child).collect(),
            now: Some(chrono::DateTime::UNIX_EPOCH + chrono::Duration::days(20_000)),
            ..AssessmentRequest::default()
        }
    }

    fn cells(n: usize) -> Vec<StatsCell> {
        (0..n).map(|_| StatsCell::default()).collect()
    }

    /// The request's cells: one per context entity, and the two snapshots'.
    fn prepared<'a>(
        policy: &'a Arc<Policy>,
        request: &'a AssessmentRequest,
        context: &'a [StatsCell],
        change: &'a [StatsCell],
    ) -> PreparedStats<'a> {
        let mut p = PreparedStats::new(policy);
        for (e, cell) in request.context.iter().zip(context) {
            p.context(&e.id, cell);
        }
        p.change(0, change.first(), change.get(1));
        p
    }

    /// The texts this thread analyses while `f` runs.
    fn analysed_by<R>(f: impl FnOnce() -> R) -> (R, u64) {
        let before = analysed::texts();
        let out = f();
        (out, analysed::texts() - before)
    }

    #[test]
    fn kept_stats_give_the_same_assessment_and_save_the_analysis() {
        let engine = Engine::default();
        let request = request();
        let (plain, plain_texts) = analysed_by(|| engine.assess(&request).expect("assesses"));
        let (context, change) = (cells(8), cells(2));
        let p = prepared(engine.shared_policy(), &request, &context, &change);

        // The first run fills the cells. It analyses what a plain run does,
        // and the title of the `before` snapshot, which a plain run skips.
        let (first, first_texts) =
            analysed_by(|| engine.assess_prepared(&request, &p).expect("assesses"));
        assert_eq!(first, plain);
        assert_eq!(first_texts, plain_texts + 1);
        assert!(context.iter().chain(&change).all(StatsCell::is_filled));

        // The second run analyses no document and no title again: two
        // texts per context entity and three for the change are saved.
        let (second, second_texts) =
            analysed_by(|| engine.assess_prepared(&request, &p).expect("assesses"));
        assert_eq!(second, plain);
        assert_eq!(second_texts + 2 * 8 + 3, plain_texts);
    }

    #[test]
    fn an_engine_with_another_text_policy_ignores_the_kept_stats() {
        let base = Engine::default();
        let request = request();
        let (context, change) = (cells(8), cells(2));
        let p = prepared(base.shared_policy(), &request, &context, &change);
        base.assess_prepared(&request, &p).expect("assesses");
        let kept: Vec<_> = context
            .iter()
            .map(|c| c.get(base.shared_policy()))
            .collect();

        // A request-level override of the text policy: the cells are
        // bypassed and stay as they were.
        let other = base
            .with_policy_patch(&serde_json::json!({ "text": { "stopwords": false } }))
            .expect("a valid patch");
        let (plain, plain_texts) = analysed_by(|| other.assess(&request).expect("assesses"));
        let (bypassed, texts) =
            analysed_by(|| other.assess_prepared(&request, &p).expect("assesses"));
        assert_eq!(bypassed, plain);
        assert_eq!(texts, plain_texts, "everything was analysed again");
        assert_ne!(
            plain,
            base.assess(&request).expect("assesses"),
            "the override changes the result, so stale stats would show"
        );
        for (cell, before) in context.iter().zip(&kept) {
            let now = cell.get(base.shared_policy());
            assert!(matches!((&now, before), (Some(a), Some(b)) if Arc::ptr_eq(a, b)));
            assert!(cell.get(other.shared_policy()).is_none());
        }

        // An override that leaves the text policy alone still uses them.
        let strict = base
            .with_policy_patch(&serde_json::json!({ "thresholds": { "high": 0.4 } }))
            .expect("a valid patch");
        let plain = strict.assess(&request).expect("assesses");
        let (cached, texts) =
            analysed_by(|| strict.assess_prepared(&request, &p).expect("assesses"));
        assert_eq!(cached, plain);
        assert!(texts < plain_texts);
    }

    #[test]
    fn stats_of_an_old_base_policy_are_computed_again() {
        let old = Engine::default();
        let request = request();
        let (context, change) = (cells(8), cells(2));
        old.assess_prepared(
            &request,
            &prepared(old.shared_policy(), &request, &context, &change),
        )
        .expect("assesses");

        // The owner of the cells moves to another text policy.
        let new = old
            .with_policy_patch(&serde_json::json!({ "text": { "stopwords": false } }))
            .expect("a valid patch");
        let p = prepared(new.shared_policy(), &request, &context, &change);
        let plain = new.assess(&request).expect("assesses");
        assert_eq!(new.assess_prepared(&request, &p).expect("assesses"), plain);
        for cell in context.iter().chain(&change) {
            assert!(cell.get(new.shared_policy()).is_some());
            assert!(cell.get(old.shared_policy()).is_none());
        }
        let (again, texts) = analysed_by(|| new.assess_prepared(&request, &p).expect("assesses"));
        assert_eq!(again, plain);
        let (_, plain_texts) = analysed_by(|| new.assess(&request).expect("assesses"));
        assert_eq!(texts + 2 * 8 + 3, plain_texts);
    }

    #[test]
    fn snapshots_derived_from_field_changes_are_analysed() {
        use crate::model::{FieldChange, FieldValue, field};
        let engine = Engine::default();
        let mut request = request();
        let parent = request.changes[0].before.clone().expect("a before");
        request.changes = vec![ChangeSet {
            entity_id: "ENG-0".to_owned(),
            field_changes: vec![FieldChange {
                field: field::DESCRIPTION.to_owned(),
                before: Some(FieldValue::Text("- [ ] Email and password login".into())),
                after: Some(FieldValue::Text("- [ ] Google OAuth login only".into())),
            }],
            ..ChangeSet::default()
        }];
        request.context.push(parent.clone());
        let plain = engine.assess(&request).expect("assesses");

        // Cells that hold the stored parent's stats: neither derived
        // snapshot is that entity, so they must not be read for the change.
        let (context, change) = (cells(9), cells(2));
        let lexicon = Lexicon::new(&engine.policy().text);
        for cell in context.iter().chain(&change) {
            cell.stats(&child(1), engine.shared_policy(), &lexicon);
        }
        let mut p = PreparedStats::new(engine.shared_policy());
        p.change(0, change.first(), change.get(1));
        p.context("ENG-0", &context[8]);
        assert_eq!(
            engine.assess_prepared(&request, &p).expect("assesses"),
            plain
        );
    }
}
