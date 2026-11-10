//! Impact propagation: how far, and how strongly, one field change reaches
//! through the graph.
//!
//! Each scored field of each changed entity is a *seed* carrying a risk
//! (severity × magnitude) and a strength per channel. From the seed, impact
//! travels breadth-first:
//!
//! * **down** to children and further descendants, **up** to the parent and
//!   further ancestors, **dependency** to what the entity blocks (and their
//!   children), **upstream** to its blockers; **related** and **sibling** one hop only;
//! * losing `hop_decay` per hop beyond the first, stopping at `max_depth` or
//!   where even the strongest possible contribution falls below
//!   `min_impact`, keeping the strongest path per (entity, channel), within
//!   the seed's own step budget (so one change cannot starve another, and
//!   the result does not depend on the order of the changes);
//! * scaled at each target by a **relevance** that depends on the field:
//!   how much of the added or removed text the target's own text and title
//!   share (TF-IDF cosine, and the embedding change direction when
//!   available) — the child that best matches a removed passage *owns* it —,
//!   what share of the parent's effort an estimate change (or a creation,
//!   deletion, cancellation) moves, whether a moved date now conflicts with
//!   the target's (newly or worse than before), whether a canceled parent
//!   leaves the target active, whether a reassignment hits a sibling with
//!   the same assignee.
//!
//! A contribution is `severity × m_eff × transfer × relevance`, where `m_eff`
//! is the seed's magnitude except when the target redefines it (an estimate
//! change measured against the parent's effort, a removed passage the target
//! owns). Contributions below `min_impact` are dropped (a noise gate: a
//! relevance floor is not evidence), except effort moves, which the engine
//! sums per target before measuring their share, so fifty small
//! re-estimates weigh what one large one would.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::assessment::Channel;
use crate::fields::{Channels, Detail, LifecycleKind, Transition};
use crate::graph::Graph;
use crate::model::{Entity, StatusCategory, field};
use crate::policy::Policy;
use crate::stats::{clamp01, saturate};
use crate::text::{Overlap, SparseVec, TextDelta, direction_alignment};

/// Children × changed passages beyond which ownership is not computed (a safety valve).
const MAX_OWNERSHIP_PAIRS: usize = 200_000;

/// One field change of one changed entity, ready to travel.
#[derive(Debug, Clone)]
pub(crate) struct Seed<'a> {
    pub source: usize,
    pub field: &'a str,
    /// The field's severity, already scaled by its weight.
    pub severity: f64,
    pub magnitude: f64,
    pub channels: Channels,
    pub detail: &'a Detail,
    /// The changed entity's estimate before the change: a cancellation (or
    /// revival) moves it out of (or back into) the parent's effort.
    pub source_estimate: Option<f64>,
    /// Graph steps this seed may take.
    pub budget: u32,
}

/// Effort that moved under `on` (the target itself on the up channel, the
/// shared parent on the sibling channel): `delta` (signed) and the part of
/// the contribution that does not depend on that effort.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rollup {
    pub on: usize,
    pub delta: f64,
    /// `severity × transfer × relevance`: the contribution is `factor · saturate(share)`.
    pub factor: f64,
}

/// Why a hop has the relevance it has (rendered only for the hits shown).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Note {
    None,
    TextRemoved(f64),
    TextRemovedOwner(f64),
    TextReworded(f64),
    TextRewordedOwner(f64),
    TextAdded(f64),
    TextRelevance(f64),
    EffortShare(f64),
    EffortUnknown,
    SizeUnknown,
    StillActive,
    AlreadyClosed,
    Active,
    Closed,
    SharesCapacity,
    CompetesForParent,
    DueAfterParent,
    ChildDueAfter,
    BlockerTooLate,
    BlockedDueBefore,
    SharesReference,
    OldValue,
    MovesAlong,
    Signal,
}

impl Note {
    fn render(self) -> Option<String> {
        Some(match self {
            Note::None => return None,
            Note::TextRemoved(o) => format!("text it is about was removed (overlap {o:.2})"),
            Note::TextRemovedOwner(o) => {
                format!("it is the closest match for text that was removed (overlap {o:.2})")
            }
            Note::TextReworded(o) => format!("text it is about was reworded (overlap {o:.2})"),
            Note::TextRewordedOwner(o) => {
                format!("it is the closest match for text that was reworded (overlap {o:.2})")
            }
            Note::TextAdded(o) => format!("added text concerns it (overlap {o:.2})"),
            Note::TextRelevance(o) => format!("text relevance {o:.2}"),
            Note::EffortShare(s) => format!("{:.0}% of its effort", s * 100.0),
            Note::EffortUnknown => "its effort is unknown".to_owned(),
            Note::SizeUnknown => "scope changed, size unknown".to_owned(),
            Note::StillActive => "it is still active".to_owned(),
            Note::AlreadyClosed => "it is already closed".to_owned(),
            Note::Active => "it is active".to_owned(),
            Note::Closed => "it is closed".to_owned(),
            Note::SharesCapacity => "shares its assignee or cycle".to_owned(),
            Note::CompetesForParent => "competes for the same parent".to_owned(),
            Note::DueAfterParent => "it is now due after its parent".to_owned(),
            Note::ChildDueAfter => "its child is now due after it".to_owned(),
            Note::BlockerTooLate => "its blocker now finishes too late".to_owned(),
            Note::BlockedDueBefore => "what it blocks is now due before it".to_owned(),
            Note::SharesReference => "shares the affected assignee/cycle".to_owned(),
            Note::OldValue => "it still has the old value (it usually moves along)".to_owned(),
            Note::MovesAlong => "it moves along with it".to_owned(),
            Note::Signal => "it reports a blocker or a slip".to_owned(),
        })
    }
}

/// One contribution of one seed to one entity.
#[derive(Debug, Clone)]
pub(crate) struct Hit {
    pub target: usize,
    pub source: usize,
    pub field: String,
    pub channel: Channel,
    pub path: Vec<usize>,
    pub value: f64,
    pub note: Note,
    pub removed_overlap: f64,
    pub added_overlap: f64,
    pub modified_overlap: f64,
    /// Text evidence (title, description, comments).
    pub text: bool,
    pub transition: Option<Transition>,
    pub lifecycle: Option<LifecycleKind>,
    pub rollup: Option<Rollup>,
    /// A moved date created a conflict with the target, or made one worse.
    pub date_conflict: bool,
    /// A comment reporting a blocker or a slip, reaching what it blocks or its parent.
    pub schedule_signal: bool,
    /// A comment carrying a scope signal.
    pub scope_signal: bool,
    /// The target moves along with the source (its children on a re-parenting
    /// or a project move).
    pub moved_along: bool,
}

#[derive(Debug, Clone)]
struct State {
    node: usize,
    channel: Channel,
    depth: u32,
    transfer: f64,
    path: Vec<usize>,
}

/// Which changed passages of a parent's text (or comment) a child owns.
#[derive(Debug, Clone, Copy, Default)]
struct Owned {
    removed: bool,
    modified: bool,
    added: bool,
}

pub(crate) struct Propagator<'a> {
    graph: &'a Graph,
    policy: &'a Policy,
    docs: &'a [SparseVec],
    titles: &'a [SparseVec],
    /// The nodes whose text was analysed (`engine::text_reach`): every node
    /// the walk visits must be one of them, or its relevance would be read
    /// from an empty vector. Checked in debug builds.
    reach: &'a [bool],
    /// `(own estimate, Σ live children's estimates)` per node.
    efforts: Vec<(f64, f64)>,
}

/// What one hop adds: the effective magnitude, the relevance and the facts behind them.
#[derive(Debug, Clone)]
struct HopValue {
    magnitude: f64,
    relevance: f64,
    note: Note,
    removed_overlap: f64,
    added_overlap: f64,
    modified_overlap: f64,
    text: bool,
    rollup: Option<Rollup>,
    date_conflict: bool,
    schedule_signal: bool,
    scope_signal: bool,
    moved_along: bool,
}

impl HopValue {
    fn plain(magnitude: f64, relevance: f64) -> Self {
        Self {
            magnitude,
            relevance,
            note: Note::None,
            removed_overlap: 0.0,
            added_overlap: 0.0,
            modified_overlap: 0.0,
            text: false,
            rollup: None,
            date_conflict: false,
            schedule_signal: false,
            scope_signal: false,
            moved_along: false,
        }
    }

    fn noted(mut self, note: Note) -> Self {
        self.note = note;
        self
    }
}

/// Cosine of two L2-normalized sparse vectors with non-negative weights.
fn cosine(a: &SparseVec, b: &SparseVec) -> f64 {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    clamp01(
        small
            .iter()
            .filter_map(|(t, w)| large.get(t).map(|x| w * x))
            .sum(),
    )
}

/// The share of `title`'s (L2-normalized) weight whose terms `passage`
/// mentions: 1 when a comment names every word of a title.
fn containment(passage: &SparseVec, title: &SparseVec) -> f64 {
    clamp01(
        title
            .iter()
            .filter(|(t, _)| passage.contains_key(*t))
            .map(|(_, w)| w * w)
            .sum(),
    )
}

fn days(d: chrono::TimeDelta) -> f64 {
    d.num_seconds() as f64 / 86_400.0
}

/// Whether a conflict of `after` days (positive = conflicting) is new or
/// worse than the `before` one.
fn conflict_worse(after: Option<f64>, before: Option<f64>) -> bool {
    match after {
        Some(a) if a > 0.0 => before.is_none_or(|b| b <= 0.0 || a > b),
        _ => false,
    }
}

/// The effort a roll-up of `moved` (signed, summed over the changes) is
/// measured against: the larger of the target's effort before and after,
/// so an increase and a decrease of the same size weigh the same.
pub(crate) fn rollup_base(own: f64, kids_after: f64, moved: f64) -> f64 {
    let after = own.max(kids_after);
    let before = own.max(kids_after - moved);
    after.max(before)
}

/// Adds the parents a change's first `up` hop goes to, without duplicates.
/// A change of the `parent` field is a move: it touches the parent it left
/// and the one it joined (`moved`, ids the graph does not know are skipped).
/// Any other change goes to the current parent of `source`.
///
/// The walk and the engine's text reach both take their parents from here,
/// so the reach covers every parent the walk can step to.
pub(crate) fn first_parents(
    g: &Graph,
    source: usize,
    moved: Option<[Option<&str>; 2]>,
    out: &mut Vec<usize>,
) {
    let mut add = |p: usize| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    match moved {
        Some(ids) => ids
            .into_iter()
            .flatten()
            .filter_map(|id| g.get(id))
            .for_each(&mut add),
        None => g.parent[source].into_iter().for_each(&mut add),
    }
}

fn enqueue(queue: &mut VecDeque<State>, used: usize, budget: usize, state: State) -> bool {
    if used.saturating_add(queue.len()) < budget {
        queue.push_back(state);
        true
    } else {
        false
    }
}

impl<'a> Propagator<'a> {
    pub(crate) fn new(
        graph: &'a Graph,
        policy: &'a Policy,
        docs: &'a [SparseVec],
        titles: &'a [SparseVec],
        reach: &'a [bool],
    ) -> Self {
        let efforts = (0..graph.len())
            .map(|i| {
                let own = graph.nodes[i].estimate().unwrap_or(0.0);
                let kids: f64 = graph.children[i]
                    .iter()
                    .filter(|&&c| {
                        !graph.deleted[c]
                            && graph.nodes[c].status_category() != Some(StatusCategory::Canceled)
                    })
                    .filter_map(|&c| graph.nodes[c].estimate())
                    .sum();
                (own, kids)
            })
            .collect();
        Self {
            graph,
            policy,
            docs,
            titles,
            reach,
            efforts,
        }
    }

    /// Whether `node`'s text was analysed (see [`Propagator::reach`]).
    fn in_reach(&self, node: usize) -> bool {
        self.reach.get(node).copied().unwrap_or(false)
    }

    /// `(own estimate, Σ live children's estimates)` of a node, after the changes.
    pub(crate) fn effort_parts(&self, i: usize) -> (f64, f64) {
        self.efforts.get(i).copied().unwrap_or((0.0, 0.0))
    }

    fn strength(seed: &Seed<'_>, channel: Channel) -> f64 {
        let c = &seed.channels;
        match channel {
            Channel::SelfChange => 1.0,
            Channel::Down => c.down,
            Channel::Up => c.up,
            Channel::Dependency => c.dependency,
            Channel::Upstream => c.upstream,
            Channel::Related => c.related,
            Channel::Sibling => c.sibling,
        }
    }

    /// Runs one seed within its own step budget and appends its hits.
    /// Returns whether the budget ran out before the walk was complete.
    pub(crate) fn run(&self, seed: &Seed<'_>, out: &mut Vec<Hit>) -> bool {
        let g = self.graph;
        let pp = &self.policy.propagation;
        if pp.max_depth == 0 || seed.severity * seed.magnitude <= 0.0 {
            return false;
        }
        let budget = usize::try_from(seed.budget.max(1)).unwrap_or(usize::MAX);
        let s = seed.source;
        let owners = match seed.detail {
            Detail::Text(delta) => self.ownership(s, delta, false),
            // A scope comment that names a child is about that child.
            Detail::Activity {
                delta, scope: true, ..
            } => self.ownership(s, delta, true),
            _ => HashMap::new(),
        };

        // The first hop: structural edges first, then siblings (those sharing
        // the assignee or cycle first). Nothing beyond the budget is queued.
        let mut parents: Vec<usize> = Vec::new();
        let moved = match seed.detail {
            _ if seed.field != field::PARENT => None,
            Detail::Reference { before, after } => Some([before.as_deref(), after.as_deref()]),
            _ => Some([None, None]),
        };
        first_parents(g, s, moved, &mut parents);
        let source_entity = &g.nodes[s];
        let siblings: &[usize] = match g.parent[s] {
            Some(p) if Self::strength(seed, Channel::Sibling) > 0.0 => &g.children[p],
            _ => &[],
        };
        let shared = siblings
            .iter()
            .copied()
            .filter(|&x| x != s && self.shared_capacity(source_entity, &g.nodes[x]));
        let others = siblings
            .iter()
            .copied()
            .filter(|&x| x != s && !self.shared_capacity(source_entity, &g.nodes[x]));
        let first = g.children[s]
            .iter()
            .map(|&c| (c, Channel::Down))
            .chain(parents.iter().map(|&p| (p, Channel::Up)))
            .chain(g.blocks[s].iter().map(|&b| (b, Channel::Dependency)))
            .chain(g.blocked_by[s].iter().map(|&b| (b, Channel::Upstream)))
            .chain(g.related[s].iter().map(|&r| (r, Channel::Related)))
            .chain(shared.chain(others).map(|x| (x, Channel::Sibling)));

        let mut truncated = false;
        let mut queue: VecDeque<State> = VecDeque::new();
        for (node, channel) in first {
            if node == s {
                continue;
            }
            let transfer = Self::strength(seed, channel);
            if transfer <= 0.0 || seed.severity * transfer < pp.min_impact {
                continue;
            }
            let state = State {
                node,
                channel,
                depth: 1,
                transfer,
                path: vec![s, node],
            };
            if !enqueue(&mut queue, 0, budget, state) {
                truncated = true;
                break;
            }
        }

        let mut best: HashMap<(usize, Channel), f64> = HashMap::new();
        let mut steps = 0usize;
        while let Some(st) = queue.pop_front() {
            steps += 1;
            debug_assert!(
                self.in_reach(st.node),
                "the walk visited `{}`, outside the analysed text reach",
                g.nodes[st.node].id
            );
            if best
                .get(&(st.node, st.channel))
                .is_some_and(|b| *b >= st.transfer)
            {
                continue;
            }
            best.insert((st.node, st.channel), st.transfer);

            let hop = self.hop(seed, &st, &owners);
            let value = clamp01(seed.severity * hop.magnitude * st.transfer * hop.relevance);
            if value >= pp.min_impact || (value > 0.0 && hop.rollup.is_some()) {
                out.push(Hit {
                    target: st.node,
                    source: s,
                    field: seed.field.to_owned(),
                    channel: st.channel,
                    path: st.path.clone(),
                    value,
                    note: hop.note,
                    removed_overlap: hop.removed_overlap,
                    added_overlap: hop.added_overlap,
                    modified_overlap: hop.modified_overlap,
                    text: hop.text,
                    transition: match seed.detail {
                        Detail::Status(t) => Some(*t),
                        _ => None,
                    },
                    lifecycle: match seed.detail {
                        Detail::Lifecycle { kind, .. } => Some(*kind),
                        _ => None,
                    },
                    rollup: hop.rollup,
                    date_conflict: hop.date_conflict,
                    schedule_signal: hop.schedule_signal,
                    scope_signal: hop.scope_signal,
                    moved_along: hop.moved_along,
                });
            }

            if st.depth >= pp.max_depth {
                continue;
            }
            let next_transfer = st.transfer * pp.hop_decay;
            if seed.severity * next_transfer < pp.min_impact {
                continue;
            }
            let mut next: Vec<(usize, Channel)> = Vec::new();
            match st.channel {
                Channel::Down => {
                    next.extend(g.children[st.node].iter().map(|&c| (c, Channel::Down)))
                }
                Channel::Up => next.extend(g.parent[st.node].map(|p| (p, Channel::Up))),
                Channel::Dependency => {
                    next.extend(g.blocks[st.node].iter().map(|&b| (b, Channel::Dependency)));
                    next.extend(
                        g.children[st.node]
                            .iter()
                            .map(|&c| (c, Channel::Dependency)),
                    );
                }
                Channel::Upstream => {
                    next.extend(
                        g.blocked_by[st.node]
                            .iter()
                            .map(|&b| (b, Channel::Upstream)),
                    );
                }
                Channel::Related | Channel::Sibling | Channel::SelfChange => {}
            }
            for (node, channel) in next {
                if node == s || st.path.contains(&node) {
                    continue;
                }
                let mut path = st.path.clone();
                path.push(node);
                let state = State {
                    node,
                    channel,
                    depth: st.depth + 1,
                    transfer: next_transfer,
                    path,
                };
                if !enqueue(&mut queue, steps, budget, state) {
                    truncated = true;
                    break;
                }
            }
        }
        truncated
    }

    fn shared_capacity(&self, source: &Entity, target: &Entity) -> bool {
        let same = |name: &str| matches!((source.reference(name), target.reference(name)), (Some(a), Some(b)) if a == b);
        same(field::ASSIGNEE) || same(field::CYCLE)
    }

    /// How much a passage is about an entity: the better of its cosine with
    /// the entity's whole text and its match with the title (cosine; for a
    /// comment, how much of the title it names).
    fn passage_overlap(&self, passage: &SparseVec, node: usize, comment: bool) -> f64 {
        let doc = self.docs.get(node).map_or(0.0, |d| cosine(passage, d));
        let title = self.titles.get(node).map_or(0.0, |t| {
            if comment {
                containment(passage, t)
            } else {
                cosine(passage, t)
            }
        });
        doc.max(title)
    }

    /// The children of `s` that own each removed or reworded passage (each
    /// passage of a scope comment): the best match among the siblings (ties
    /// within `owner_tie_ratio`), when that match is at least
    /// `owner_min_overlap`. A passage nobody matches well has no owner.
    /// Only active children take the owner's weight (see [`Self::hop`]), but
    /// closed ones compete: a passage a done child implemented is its.
    fn ownership(&self, s: usize, delta: &TextDelta, comment: bool) -> HashMap<usize, Owned> {
        let g = self.graph;
        let pp = &self.policy.propagation;
        let mut out: HashMap<usize, Owned> = HashMap::new();
        let kids: Vec<usize> = g.children[s]
            .iter()
            .copied()
            .filter(|&c| !g.deleted[c])
            .collect();
        debug_assert!(
            kids.iter().all(|&c| self.in_reach(c)),
            "a child of `{}` is outside the analysed text reach",
            g.nodes[s].id
        );
        let lists: [(&[SparseVec], u8); 3] = if comment {
            [(&delta.added, 2), (&[], 0), (&[], 1)]
        } else {
            [(&delta.removed, 0), (&delta.modified, 1), (&[], 2)]
        };
        let passages: usize = lists.iter().map(|(l, _)| l.len()).sum();
        if kids.is_empty()
            || passages == 0
            || kids.len().saturating_mul(passages) > MAX_OWNERSHIP_PAIRS
        {
            return out;
        }
        for (list, kind) in lists {
            for p in list {
                let overlaps: Vec<f64> = kids
                    .iter()
                    .map(|&c| self.passage_overlap(p, c, comment))
                    .collect();
                let best = overlaps.iter().copied().fold(0.0, f64::max);
                if best < pp.owner_min_overlap {
                    continue;
                }
                for (&c, &o) in kids.iter().zip(&overlaps) {
                    if o >= pp.owner_min_overlap && o >= pp.owner_tie_ratio * best {
                        let e = out.entry(c).or_default();
                        match kind {
                            0 => e.removed = true,
                            1 => e.modified = true,
                            _ => e.added = true,
                        }
                    }
                }
            }
        }
        out
    }

    /// Best overlap per kind of changed passage with the target's text and title.
    /// Comments match titles by containment (a comment naming a child is about it).
    fn text_overlap(&self, delta: &TextDelta, node: usize, comment: bool) -> Overlap {
        let empty = SparseVec::new();
        let doc = self.docs.get(node).unwrap_or(&empty);
        let title = self.titles.get(node).unwrap_or(&empty);
        let o = delta.overlap(doc);
        let by_title = |passages: &[SparseVec]| {
            passages
                .iter()
                .map(|p| {
                    if comment {
                        containment(p, title)
                    } else {
                        cosine(p, title)
                    }
                })
                .fold(0.0, f64::max)
        };
        Overlap {
            removed: o.removed.max(by_title(&delta.removed)),
            added: o.added.max(by_title(&delta.added)),
            modified: o.modified.max(by_title(&delta.modified)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text_hop(
        &self,
        delta: &TextDelta,
        st: &State,
        capacity: f64,
        owned: Owned,
        comment: bool,
        m: f64,
    ) -> HopValue {
        let pp = &self.policy.propagation;
        let target = &self.graph.nodes[st.node];
        let abs = self.text_overlap(delta, st.node, comment);
        let (removed_abs, added, modified_abs) = (abs.removed, abs.added, abs.modified);
        let effective = Overlap {
            removed: if owned.removed { 1.0 } else { removed_abs },
            added: if owned.added { 1.0 } else { added },
            modified: if owned.modified { 1.0 } else { modified_abs },
        };
        let mut overlap = effective.max();
        if let (Some(dir), Some(emb)) = (&delta.embedding_delta, &target.embedding)
            && let Some(align) = direction_alignment(dir, emb)
        {
            // Embeddings only add evidence: they never lower a lexical match.
            overlap = overlap.max(clamp01(2.0 * align));
        }
        let relevance = match st.channel {
            Channel::Up | Channel::Dependency | Channel::Upstream => 0.5 + 0.5 * overlap,
            Channel::Sibling => {
                (pp.relevance_floor + (1.0 - pp.relevance_floor) * overlap) * capacity.max(overlap)
            }
            _ => pp.relevance_floor + (1.0 - pp.relevance_floor) * overlap,
        };
        // An applied judgment leaves only a share of the owner floors and
        // of the overlaps the notes and the hit report. Relevance is not
        // scaled. Shares of `(1, 1)` change nothing.
        let (rs, cs) = delta.shares();
        let (removed_abs, modified_abs) = (removed_abs * rs, modified_abs * cs);
        let (removed, added, modified) = (
            effective.removed * rs,
            effective.added * cs,
            effective.modified * cs,
        );
        let magnitude = if owned.removed {
            m.max(rs * pp.removed_owner_magnitude)
        } else if owned.modified {
            m.max(cs * pp.reworded_owner_magnitude)
        } else {
            m
        };
        let changed = added.max(modified);
        let note = if removed >= pp.removed_overlap_flag && removed > changed {
            if owned.removed {
                Note::TextRemovedOwner(removed_abs)
            } else {
                Note::TextRemoved(removed)
            }
        } else if modified >= pp.added_overlap_flag && modified >= added {
            if owned.modified {
                Note::TextRewordedOwner(modified_abs)
            } else {
                Note::TextReworded(modified)
            }
        } else if added >= pp.added_overlap_flag {
            Note::TextAdded(added)
        } else {
            Note::TextRelevance(overlap)
        };
        HopValue {
            removed_overlap: removed,
            added_overlap: added,
            modified_overlap: modified,
            text: true,
            ..HopValue::plain(magnitude, relevance).noted(note)
        }
    }

    /// Effort moved under the target (up) or under the parent it shares
    /// with the source (sibling): its share of that effort.
    fn rollup(&self, seed: &Seed<'_>, st: &State, delta: f64, m: f64, relevance: f64) -> HopValue {
        let on = match st.channel {
            Channel::Sibling => self.graph.parent[st.node],
            _ => Some(st.node),
        };
        let d = delta.abs();
        let Some(on) = on.filter(|_| d > 0.0) else {
            return HopValue::plain(m.min(0.5), relevance).noted(Note::SizeUnknown);
        };
        let (own, kids) = self.effort_parts(on);
        let base = rollup_base(own, kids, delta);
        if base.is_nan() || base <= 0.0 {
            return HopValue::plain(m.min(0.5), relevance).noted(Note::EffortUnknown);
        }
        let share = (d / base).min(1.0);
        let note = match st.channel {
            Channel::Sibling if relevance >= self.policy.propagation.sibling_shared => {
                Note::SharesCapacity
            }
            Channel::Sibling => Note::CompetesForParent,
            _ => Note::EffortShare(share),
        };
        HopValue {
            rollup: Some(Rollup {
                on,
                delta,
                factor: seed.severity * st.transfer * relevance,
            }),
            ..HopValue::plain(saturate(share, self.policy.number.rollup_scale), relevance)
                .noted(note)
        }
    }

    /// The effort an estimate-like change moves: an estimate's delta, a
    /// creation's or deletion's estimate, a cancellation's (or revival's)
    /// estimate. `None` for changes that move no effort.
    fn effort_moved(seed: &Seed<'_>) -> Option<f64> {
        match seed.detail {
            Detail::Estimate { delta } if seed.field == field::ESTIMATE => Some(*delta),
            Detail::Lifecycle { kind, estimate } => Some(match (kind, estimate) {
                (_, None) => 0.0,
                (LifecycleKind::Created, Some(e)) => *e,
                (LifecycleKind::Deleted, Some(e)) => -*e,
            }),
            Detail::Status(t @ (Transition::Cancel | Transition::Revive)) => {
                let e = seed.source_estimate.filter(|e| *e > 0.0)?;
                Some(if *t == Transition::Cancel { -e } else { e })
            }
            _ => None,
        }
    }

    fn date_hop(
        &self,
        seed: &Seed<'_>,
        target: &Entity,
        (before, after): (Option<DateTime<Utc>>, Option<DateTime<Utc>>),
        channel: Channel,
        m: f64,
    ) -> HopValue {
        let pulled_in = matches!((before, after), (Some(b), Some(a)) if a < b);
        let due = seed.field == field::DUE_DATE;
        let conflict = |note| HopValue {
            date_conflict: true,
            ..HopValue::plain(m.max(0.6), 1.0).noted(note)
        };
        match channel {
            Channel::Down => match target.due() {
                Some(td) if due => {
                    let late = |pd: Option<DateTime<Utc>>| pd.map(|pd| days(td - pd));
                    if conflict_worse(late(after), late(before)) {
                        conflict(Note::DueAfterParent)
                    } else {
                        HopValue::plain(m, if pulled_in { 0.5 } else { 0.2 })
                    }
                }
                Some(_) => HopValue::plain(m, if pulled_in { 0.5 } else { 0.2 }),
                None => HopValue::plain(m, if pulled_in { 0.4 } else { 0.15 }),
            },
            Channel::Up => match target.due() {
                Some(pd) if due => {
                    let late = |sd: Option<DateTime<Utc>>| sd.map(|sd| days(sd - pd));
                    if conflict_worse(late(after), late(before)) {
                        conflict(Note::ChildDueAfter)
                    } else {
                        HopValue::plain(m, 0.3)
                    }
                }
                _ => HopValue::plain(m, 0.3),
            },
            Channel::Dependency => {
                let late = |sd: Option<DateTime<Utc>>| {
                    let sd = sd?;
                    [target.due(), target.start()]
                        .into_iter()
                        .flatten()
                        .map(|d| days(sd - d))
                        .reduce(f64::max)
                };
                if due && conflict_worse(late(after), late(before)) {
                    conflict(Note::BlockerTooLate)
                } else if pulled_in {
                    HopValue::plain(m, 0.1)
                } else {
                    HopValue::plain(m, 0.4)
                }
            }
            Channel::Upstream => {
                let late = |sd: Option<DateTime<Utc>>| {
                    let bd = target.due()?;
                    sd.map(|sd| days(bd - sd))
                };
                if conflict_worse(late(after), late(before)) {
                    conflict(Note::BlockedDueBefore)
                } else if pulled_in {
                    HopValue::plain(m, 0.5)
                } else {
                    HopValue::plain(m, 0.1)
                }
            }
            _ => HopValue::plain(m, 0.3),
        }
    }

    /// What an active child owns of its parent's change (closed work keeps
    /// its plain overlap: there is nothing left to stop or rescope).
    fn owned(&self, owners: &HashMap<usize, Owned>, st: &State, active: bool) -> Owned {
        if active && st.channel == Channel::Down && st.depth == 1 {
            owners.get(&st.node).copied().unwrap_or_default()
        } else {
            Owned::default()
        }
    }

    fn hop(&self, seed: &Seed<'_>, st: &State, owners: &HashMap<usize, Owned>) -> HopValue {
        let g = self.graph;
        let pp = &self.policy.propagation;
        let source = &g.nodes[seed.source];
        let target = &g.nodes[st.node];
        let target_active = !g.deleted[st.node] && target.is_active();
        let m = seed.magnitude;
        let capacity = if self.shared_capacity(source, target) {
            pp.sibling_shared
        } else {
            pp.sibling_default
        };
        let active_note = |active: bool| {
            if active {
                Note::StillActive
            } else {
                Note::AlreadyClosed
            }
        };

        match (seed.detail, st.channel) {
            (Detail::Text(delta), _) => {
                let owned = self.owned(owners, st, target_active);
                self.text_hop(delta, st, capacity, owned, false, m)
            }
            (
                Detail::Activity {
                    delta,
                    schedule,
                    scope,
                },
                channel,
            ) => {
                if *schedule && matches!(channel, Channel::Dependency | Channel::Up) {
                    // A blocker or a slip matters to what it blocks and to its
                    // parent whatever words they share.
                    HopValue {
                        schedule_signal: true,
                        scope_signal: *scope,
                        ..HopValue::plain(m, 1.0).noted(Note::Signal)
                    }
                } else {
                    let owned = self.owned(owners, st, target_active);
                    let mut h = self.text_hop(delta, st, capacity, owned, true, m);
                    h.scope_signal = *scope;
                    h
                }
            }
            // Effort moved under the parent: its share of the parent's effort
            // (canceling takes it out like a deletion; reviving puts it back),
            // and, for siblings, the share of the parent they compete for.
            (_, Channel::Up) if Self::effort_moved(seed).is_some() => {
                let delta = Self::effort_moved(seed).unwrap_or(0.0);
                self.rollup(seed, st, delta, m, 1.0)
            }
            (_, Channel::Sibling) if Self::effort_moved(seed).is_some() => {
                let delta = Self::effort_moved(seed).unwrap_or(0.0);
                self.rollup(seed, st, delta, m, capacity)
            }
            (
                Detail::Lifecycle {
                    kind: LifecycleKind::Deleted,
                    ..
                },
                Channel::Down,
            ) => HopValue::plain(m, if target_active { 1.0 } else { 0.2 })
                .noted(active_note(target_active)),
            (Detail::Lifecycle { .. }, Channel::Dependency) => {
                HopValue::plain(m, if target_active { 1.0 } else { 0.1 })
            }
            (Detail::Estimate { .. }, Channel::Sibling) => {
                HopValue::plain(m, capacity).noted(if capacity >= pp.sibling_shared {
                    Note::SharesCapacity
                } else {
                    Note::CompetesForParent
                })
            }
            (Detail::Status(t), Channel::Down) => {
                let r = if target_active {
                    match t {
                        Transition::Cancel => 1.0,
                        Transition::Regress => 0.8,
                        Transition::Reopen => 0.4,
                        _ => 0.5,
                    }
                } else {
                    match t {
                        Transition::Reopen => 1.0,
                        Transition::Cancel => 0.1,
                        _ => 0.2,
                    }
                };
                HopValue::plain(m, r).noted(if target_active {
                    Note::Active
                } else {
                    Note::Closed
                })
            }
            (Detail::Status(_), Channel::Dependency) => {
                HopValue::plain(m, if target_active { 1.0 } else { 0.1 })
            }
            (Detail::Date { before, after }, channel) => {
                self.date_hop(seed, target, (*before, *after), channel, m)
            }
            (Detail::Reference { before, after }, Channel::Sibling) => {
                let hit = [before, after]
                    .into_iter()
                    .flatten()
                    .any(|v| target.reference(seed.field) == Some(v.as_str()));
                if hit {
                    HopValue::plain(m, pp.sibling_shared).noted(Note::SharesReference)
                } else {
                    HopValue::plain(m, 0.3 * pp.sibling_default)
                }
            }
            (Detail::Reference { before, .. }, Channel::Down) if seed.field != field::PARENT => {
                // Children usually move with their parent (to the new project,
                // team, cycle): worth a notification, not a re-plan.
                let moved_together = before
                    .as_deref()
                    .is_some_and(|b| target.reference(seed.field) == Some(b));
                if moved_together {
                    HopValue {
                        moved_along: true,
                        ..HopValue::plain(m, 0.5).noted(Note::OldValue)
                    }
                } else {
                    HopValue::plain(m, 0.3)
                }
            }
            (Detail::Reference { .. }, Channel::Down) => HopValue {
                moved_along: true,
                ..HopValue::plain(m, 0.5).noted(Note::MovesAlong)
            },
            (_, Channel::Sibling) => HopValue::plain(m, capacity),
            _ => HopValue::plain(m, 1.0),
        }
    }
}

/// A hit, explained: who passed it on, what changed, and why it matters here.
pub(crate) fn describe(g: &Graph, h: &Hit) -> String {
    let source = &g.nodes[h.source];
    let depth = h.path.len().saturating_sub(1);
    let role = match (h.channel, depth) {
        (Channel::Down, 1) => "parent",
        (Channel::Down, _) => "ancestor",
        (Channel::Up, 1) => "child",
        (Channel::Up, _) => "descendant",
        (Channel::Dependency, _) => "blocker",
        (Channel::Upstream, _) => "dependent",
        (Channel::Related, _) => "related item",
        (Channel::Sibling, _) => "sibling",
        (Channel::SelfChange, _) => "itself",
    };
    let what = match (h.lifecycle, h.transition, h.field.as_str()) {
        (Some(kind), _, _) => kind.as_str().to_owned(),
        (_, _, field::ACTIVITY) => "got a signal-bearing comment".to_owned(),
        (_, _, field::PARENT) => "moved to another parent".to_owned(),
        (_, Some(t), _) => format!("was {}", t.as_str()),
        (_, _, f) => format!("{f} changed"),
    };
    match h.note.render() {
        Some(note) => format!("{role} {} {what}; {note}", source.label()),
        None => format!("{role} {} {what}", source.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_base_is_symmetric() {
        // Parent without an estimate, children k and o = 2: k 2 → 8 and 8 → 2
        // move the same 6 points of a 10-point effort.
        assert_eq!(rollup_base(0.0, 10.0, 6.0), 10.0);
        assert_eq!(rollup_base(0.0, 4.0, -6.0), 10.0);
        // An own estimate above the children's sum is the base either way.
        assert_eq!(rollup_base(20.0, 8.0, -6.0), 20.0);
    }

    #[test]
    fn conflicts_count_only_when_new_or_worse() {
        assert!(conflict_worse(Some(2.0), None));
        assert!(conflict_worse(Some(2.0), Some(-1.0)));
        assert!(conflict_worse(Some(3.0), Some(2.0)));
        assert!(!conflict_worse(Some(2.0), Some(3.0)), "relief");
        assert!(!conflict_worse(Some(-1.0), Some(3.0)));
        assert!(!conflict_worse(None, Some(3.0)));
    }

    #[test]
    fn containment_measures_how_much_of_a_title_is_named() {
        let v = |pairs: &[(&str, f64)]| -> SparseVec {
            let norm = pairs.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
            pairs
                .iter()
                .map(|(t, w)| ((*t).to_owned(), w / norm))
                .collect()
        };
        let title = v(&[("paypal", 2.0), ("checkout", 1.0)]);
        let comment = v(&[
            ("paypal", 1.0),
            ("client", 1.0),
            ("drop", 1.0),
            ("need", 1.0),
        ]);
        assert!((containment(&comment, &title) - 0.8).abs() < 1e-9);
        assert!(cosine(&comment, &title) < 0.5);
    }
}
