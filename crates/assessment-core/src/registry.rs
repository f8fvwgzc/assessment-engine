//! A versioned entity registry: the stateful way to use the engine.
//!
//! Put entities in as they change; the registry diffs each against its
//! stored version, builds the neighbourhood from what it holds, assesses
//! the change, assigns the next semantic version, keeps the history, and
//! manages *basis pins* (the versions a child or a blocked item was planned
//! against). [`Registry::stale`] then lists every plan that is behind a
//! breaking change, and [`Registry::acknowledge`] re-pins once it has been
//! re-planned.
//!
//! In memory, optionally backed by an append-only JSON-lines journal that is
//! streamed and replayed on open. A torn last line from a crash (or a last
//! record whose newline was lost) is repaired before the journal is written
//! again, and a write that fails half-way is rolled back, so the journal
//! always holds exactly what the registry acknowledged.
//! [`Registry::compact`] folds the journal into a snapshot file; see
//! *Durability* below.
//!
//! **Ingest.** The registry can follow a source of truth. A put that matches
//! what is stored never reaches the engine. A put or delete can carry the
//! source's sequence for the entity ([`PutOptions::source_seq`]); one that is
//! not above the stored sequence is skipped as stale. [`Registry::import`]
//! applies a batch: new entities are stored without an assessment, unchanged
//! ones are skipped, changed ones are assessed like a put, and the journal is
//! synced once per batch.
//!
//! **Analysis cache.** The registry keeps the term counts of each stored
//! entity's title and description, next to the entity. They are computed
//! with no lock held, when an assessment first reads the entity, and dropped
//! when its title or description changes. A put on a stored graph then
//! analyses only the changed text, and an import analyses nothing. Document
//! frequencies are still computed per request, so every result is the same,
//! bit for bit, as without the cache. The counts are never written to disk;
//! they are built again on first use after an open.
//! [`Registry::with_analysis_cache`] turns the cache off.
//!
//! **History.** Every version keeps its metadata ([`VersionRecord`]) for
//! good. Snapshot bodies are kept for every version by default; bound the
//! memory they take with [`Registry::with_history_limit`] (or
//! [`Registry::open_with_history_limit`], which applies the limit while
//! replaying). A snapshot written by [`Registry::compact`] holds only the
//! bodies that were in memory, so a limit that was in force at a compaction
//! is permanent for the versions it trimmed.
//!
//! **Concurrency.** A put or delete gathers its inputs under the shared lock
//! and is assessed with no lock held, so reads, dry runs and other writes
//! proceed meanwhile. It then commits under the exclusive lock if the entity
//! has not changed, or is gathered and assessed again under that lock if it
//! has (reads and writes wait for that second assessment). Activity and
//! version comparisons are assessed with no lock held too.
//! Neighbours that change meanwhile do not cause a retry: the
//! assessment, and the versions auto-pinned for the entity, are those of the
//! neighbourhood it was assessed against (so a pin can only be older than
//! the neighbour, which [`Registry::stale`] then reports).
//!
//! **Durability.** The state on disk is two files: the journal and, once
//! [`Registry::compact`] has run, `<journal>.snapshot`.
//!
//! The snapshot is JSON lines. Its first line is a header: the format
//! version, the journal *generation* and byte length it covers, the number
//! of entries, and an FNV-1a 64 checksum over those header values and the
//! whole body. The body is one line per entity (sorted by id) and one per
//! stored source sequence. A journal of generation 1 or more starts with a
//! line that names its generation; a journal without that line (every
//! journal before the first compaction, and every 0.1/0.2 journal) is
//! generation 0. The generation line is followed by a `note` line that is
//! ignored on load. It keeps 0.1 and 0.2 from opening a compacted store as
//! an empty one: they drop one unknown last line as a torn write, and fail
//! on an unknown line that anything follows.
//!
//! A compaction of a journal at generation `G` runs in three steps:
//!
//! 1. Under the shared lock, the state is serialised to memory and the
//!    journal length `L` is noted. Readers go on; writers wait for this step
//!    only.
//! 2. With no lock held, the snapshot `{generation: G, journal_len: L}` is
//!    written to `<journal>.snapshot.tmp`, synced, renamed over
//!    `<journal>.snapshot`, and the directory is synced (on Unix).
//! 3. Under the journal lock, a new journal is written to `<journal>.tmp`:
//!    the generation line `G + 1` and its note, then whatever was appended after `L`
//!    while step 2 ran. It is synced and renamed over the journal, and the
//!    directory is synced. Writers wait for this short step.
//!
//! [`Registry::open`] loads the snapshot (if there is one), then reads the
//! journal's generation `g` and decides:
//!
//! - no snapshot, `g = 0`: replay everything (also every old journal);
//! - `g = G + 1`: the journal continues the snapshot; replay everything;
//! - `g = G`: the snapshot was taken from this journal; skip its first `L`
//!   bytes and replay the rest. If nothing is left, the journal is restarted
//!   at `G + 1`;
//! - an empty journal next to a snapshot is restarted at `G + 1`;
//! - before either restart the directory is synced, so the snapshot's
//!   rename is on disk before the journal is cut (the compaction that wrote
//!   the snapshot may have been killed before its own directory sync);
//! - anything else (a journal of generation 1 or more without its snapshot,
//!   or a pair that does not belong together) fails with
//!   [`ErrorKind::Storage`].
//!
//! What a crash leaves behind, by where it hits:
//!
//! - *During step 1 or 2, before the snapshot is renamed.* The old snapshot
//!   (or none) and the full journal are untouched and load as before. The
//!   half-written `.snapshot.tmp` is never read; `open` removes it.
//! - *After the snapshot is renamed, before the journal is renamed.* The new
//!   snapshot `{G, L}` sits next to the old journal of generation `G`. The
//!   first `L` bytes are skipped, so nothing is applied twice, and writes
//!   that were appended during step 2 are replayed from byte `L` on.
//! - *The snapshot rename is lost in a power cut, after the journal was
//!   renamed or restarted.* This cannot happen on Unix: the directory is
//!   synced before step 3 starts, and before `open` restarts a journal.
//!   Other platforms have no directory sync, and rely on the file system
//!   keeping renames in order.
//! - *After the journal is renamed.* The snapshot `{G, L}` and the journal
//!   `G + 1` are the normal state.
//! - *A power cut without `fsync` cuts the old journal below `L`.* The
//!   snapshot is always synced, so it holds everything up to `L`; `open`
//!   restarts the journal.
//! - *A snapshot that is cut short, has a wrong checksum, a wrong entry
//!   count or an unknown format.* `open` fails with [`ErrorKind::Storage`];
//!   it never loads part of a snapshot. Restore the file from a backup, or
//!   remove both files to start empty.
//! - *A torn last journal line.* Dropped and truncated, as without a
//!   snapshot.
//!
//! A failure inside a running process is handled the same way. If step 2
//! fails, or step 3 fails before its rename, the registry carries on with
//! the journal it had, and the next compaction starts over. If step 3 fails
//! after its rename, the journal refuses further writes until the registry
//! is reopened.
//!
//! [`RegistryOptions::compact_after_ops`] compacts on its own, on the thread
//! of the write that crosses the threshold, after that write has been
//! acknowledged. A failed automatic compaction never fails the write; see
//! [`Registry::take_compaction_error`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::analysis::{PreparedStats, StatsCell, same_text};
use crate::assessment::{
    Assessment, BasisStatus, ChangeAssessment, ChangeType, ImpactLevel, JudgmentStatus,
    Recommendation,
};
use crate::engine::Engine;
use crate::error::{Error, ErrorKind, Result};
use crate::model::{
    ActivityEvent, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity, EntityId,
    EntityKind, HistoryPoint, RelationKind, TextJudgment,
};
use crate::semver::{Compatibility, SemVer, VersionBump};

/// Most entities gathered as context for one change.
const MAX_CONTEXT: usize = 50_000;

/// One version of an entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub version: SemVer,
    pub bump: VersionBump,
    pub at: DateTime<Utc>,
    pub change_type: ChangeType,
    /// The content score of the change that produced this version.
    pub raw_score: f64,
    /// The level of the assessment of that change.
    pub level: ImpactLevel,
    pub changed_fields: Vec<String>,
    /// The entity at this version: `None` for a deletion, and for a version
    /// older than the registry's [history limit](Registry::with_history_limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Entity>,
    /// The text judgments that were applied to the change that produced this
    /// version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judgments: Vec<TextJudgment>,
}

/// How to put an entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PutOptions {
    /// Assess without storing anything (a what-if).
    pub dry_run: bool,
    /// Fail with [`ErrorKind::Conflict`] unless the stored version is this one.
    pub expected_version: Option<SemVer>,
    /// Activity that accompanies the change.
    pub events: Vec<ActivityEvent>,
    /// The reference time; defaults to now.
    pub now: Option<DateTime<Utc>>,
    /// When the entity carries no explicit basis: pin its parent's and
    /// blockers' current versions on creation, re-pin the new parent on
    /// re-parenting, and add any missing pin on its parent or blockers on
    /// update. Creating an entity also pins it, at its first version, on
    /// stored entities that were put before it and depend on it (its
    /// children, and what it blocks) and hold no pin on it yet.
    pub auto_pin: bool,
    pub assess: AssessOptions,
    /// The position of this change in the source's own order for the entity
    /// (an outbox id, an update time in microseconds, …). A put or delete
    /// whose sequence is not above the one stored for the entity is skipped
    /// as stale: nothing is assessed and nothing is stored. Without a
    /// sequence the put is always applied, and the stored sequence stays.
    pub source_seq: Option<u64>,
    /// Judgments of the text changes of this put (see
    /// [`ChangeSet::judgments`]). The applied ones are kept with the version.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judgments: Vec<TextJudgment>,
    /// Store nothing if the assessment asks for a judgment of a field (none
    /// was supplied, or the supplied one is stale): the
    /// outcome then has `stored` false and the requests in its assessment.
    /// Put again with the judgments to store the change.
    #[serde(skip_serializing_if = "is_false")]
    pub hold_if_flagged: bool,
}

impl Default for PutOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            expected_version: None,
            events: Vec::new(),
            now: None,
            auto_pin: true,
            assess: AssessOptions::default(),
            source_seq: None,
            judgments: Vec::new(),
            hold_if_flagged: false,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What a put did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PutOutcome {
    /// The entity as stored (or as it would be, on a dry run), with its version.
    pub entity: Entity,
    pub created: bool,
    /// Whether anything was written (false on a dry run).
    pub stored: bool,
    pub bump: VersionBump,
    /// The assessment of the change. When `unchanged` or `skipped_stale` is
    /// set, the engine did not run and this is an empty placeholder: no
    /// changes, no impacts, score 0.
    pub assessment: Assessment,
    /// The entity matches what is stored, so nothing was assessed or stored.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unchanged: bool,
    /// The put carried a [`PutOptions::source_seq`] that is not above the
    /// stored one, so nothing was assessed or stored. `entity` is then the
    /// stored entity. If the id is not stored (it was deleted, or only its
    /// sequence is known), `entity` is the given one, normalised: the
    /// registry does not hold it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skipped_stale: bool,
}

/// How to delete an entity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeleteOptions {
    /// Assess without deleting anything (a what-if).
    pub dry_run: bool,
    /// Fail with [`ErrorKind::Conflict`] unless the stored version is this one.
    pub expected_version: Option<SemVer>,
    /// The reference time; defaults to now.
    pub now: Option<DateTime<Utc>>,
    /// The position of the deletion in the source's order for the entity;
    /// see [`PutOptions::source_seq`].
    pub source_seq: Option<u64>,
}

/// What a delete did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteOutcome {
    /// Whether the entity was deleted (false on a dry run or a stale skip).
    pub deleted: bool,
    /// The assessment of the deletion; an empty placeholder when
    /// `skipped_stale` is set.
    pub assessment: Assessment,
    /// The delete carried a [`DeleteOptions::source_seq`] that is not above
    /// the stored one, so nothing was assessed or deleted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skipped_stale: bool,
}

/// One entry of a [`Registry::import`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportItem {
    /// The entity as the source holds it. For a deletion only its id is read.
    pub entity: Entity,
    /// See [`PutOptions::source_seq`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_seq: Option<u64>,
    /// The source deleted the entity.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

impl ImportItem {
    /// The entity as it is at the source.
    pub fn upsert(entity: Entity) -> Self {
        Self {
            entity,
            source_seq: None,
            deleted: false,
        }
    }

    /// The deletion of `id` at the source.
    pub fn delete(id: impl Into<EntityId>) -> Self {
        Self {
            entity: Entity::new(id, EntityKind::from("")),
            source_seq: None,
            deleted: true,
        }
    }

    pub fn with_source_seq(mut self, seq: u64) -> Self {
        self.source_seq = Some(seq);
        self
    }
}

/// How to import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportOptions {
    /// The reference time for every item; defaults to now.
    pub now: Option<DateTime<Utc>>,
    /// See [`PutOptions::auto_pin`].
    pub auto_pin: bool,
    /// Options for the assessments of changed entities.
    pub assess: AssessOptions,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            now: None,
            auto_pin: true,
            assess: AssessOptions::default(),
        }
    }
}

/// An item of an import that could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportFailure {
    pub id: EntityId,
    pub error: String,
}

/// What an import did, in numbers of items.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// New entities, stored at their first version without an assessment.
    pub created: u64,
    /// Entities that match what is stored, and deletions of entities that
    /// are not stored.
    pub unchanged: u64,
    /// Changed entities, assessed and stored like a put.
    pub assessed: u64,
    /// Items whose sequence is not above the stored one.
    pub skipped_stale: u64,
    /// Stored entities that were deleted.
    pub deleted: u64,
    /// Items that failed, in input order. The other items were applied.
    pub failed: Vec<ImportFailure>,
}

/// A plan that is behind what it was planned against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleEntry {
    pub entity_id: EntityId,
    pub basis_entity_id: EntityId,
    pub pinned: SemVer,
    /// `None` when the pinned entity was deleted.
    pub current: Option<SemVer>,
    pub status: BasisStatus,
}

#[derive(Debug, Clone)]
struct Record {
    current: Entity,
    deleted: bool,
    history: Vec<VersionRecord>,
    /// Bumped by every journal operation on this entity, so a commit can tell
    /// whether the entity changed since it was assessed.
    revision: u64,
    /// `history[..trimmed]` no longer hold snapshots.
    trimmed: usize,
    /// The kept analysis of `current`'s text (see [`crate::analysis`]):
    /// filled when an assessment first reads it, replaced by an empty cell
    /// when the text changes. Never written to disk.
    stats: Arc<StatsCell>,
}

impl Record {
    /// Drops the snapshots of all but the newest `keep` versions.
    fn retain_snapshots(&mut self, keep: Option<usize>) {
        let Some(keep) = keep else { return };
        let cut = self.history.len().saturating_sub(keep);
        if cut > self.trimmed {
            for h in &mut self.history[self.trimmed..cut] {
                h.snapshot = None;
            }
            self.trimmed = cut;
        }
    }
}

/// Who points at whom, for building neighbourhoods.
#[derive(Debug, Default)]
struct Links {
    children: HashMap<EntityId, BTreeSet<EntityId>>,
    referrers: HashMap<EntityId, BTreeSet<EntityId>>,
}

impl Links {
    fn unlink(&mut self, e: &Entity) {
        if let Some(p) = &e.parent_id
            && let Some(set) = self.children.get_mut(p)
        {
            set.remove(&e.id);
        }
        for r in &e.relations {
            if let Some(set) = self.referrers.get_mut(&r.target) {
                set.remove(&e.id);
            }
        }
    }

    fn link(&mut self, e: &Entity) {
        if let Some(p) = &e.parent_id {
            self.children
                .entry(p.clone())
                .or_default()
                .insert(e.id.clone());
        }
        for r in &e.relations {
            self.referrers
                .entry(r.target.clone())
                .or_default()
                .insert(e.id.clone());
        }
    }
}

#[derive(Debug, Default)]
struct State {
    records: HashMap<EntityId, Record>,
    links: Links,
    /// Keep snapshots of only this many newest versions per entity (`None` = all).
    history_limit: Option<usize>,
    /// The highest source sequence seen per entity id (live, deleted, or
    /// never stored: a deletion can arrive before the entity does).
    seqs: HashMap<EntityId, u64>,
}

impl State {
    /// Whether a change of `id` at `seq` is behind what is stored.
    fn is_stale(&self, id: &str, seq: Option<u64>) -> bool {
        match (seq, self.seqs.get(id)) {
            (Some(seq), Some(stored)) => seq <= *stored,
            _ => false,
        }
    }

    /// Remembers `seq` for `id` if it is above the stored one. No sequence
    /// leaves the stored one alone.
    fn note_seq(&mut self, id: &str, seq: Option<u64>) {
        let Some(seq) = seq else { return };
        match self.seqs.get_mut(id) {
            Some(stored) => *stored = (*stored).max(seq),
            None => {
                self.seqs.insert(id.to_owned(), seq);
            }
        }
    }

    /// The journal entry that remembers `seq` for `id`, if it is news.
    fn seq_op(&self, id: &str, seq: Option<u64>) -> Option<JournalOp> {
        let seq = seq?;
        (self.seqs.get(id).is_none_or(|stored| seq > *stored)).then(|| JournalOp::Seq {
            id: id.to_owned(),
            source_seq: seq,
        })
    }

    fn live(&self, id: &str) -> Option<&Entity> {
        self.records
            .get(id)
            .filter(|r| !r.deleted)
            .map(|r| &r.current)
    }

    /// The entity's revision, or `None` if it was never stored.
    fn revision(&self, id: &str) -> Option<u64> {
        self.records.get(id).map(|r| r.revision)
    }

    fn set_history_limit(&mut self, keep: Option<usize>) {
        self.history_limit = keep;
        for rec in self.records.values_mut() {
            rec.retain_snapshots(keep);
        }
    }

    /// The body of a snapshot of this state, with its hash and its number
    /// of lines: every record and then every source sequence, each sorted
    /// by id, so the same state always gives the same bytes.
    fn snapshot_body(&self) -> Result<(Vec<u8>, Fnv1a, u64)> {
        fn push(body: &mut Vec<u8>, line: &SnapshotLine<'_>) -> Result<()> {
            serde_json::to_writer(&mut *body, line).map_err(|e| {
                Error::with_source(
                    ErrorKind::Storage,
                    "a snapshot entry could not be encoded",
                    e,
                )
            })?;
            body.push(b'\n');
            Ok(())
        }
        let mut body = Vec::new();
        let mut entries: u64 = 0;
        let mut ids: Vec<&EntityId> = self.records.keys().collect();
        ids.sort();
        for id in ids {
            let Some(rec) = self.records.get(id) else {
                continue;
            };
            let (history, head) = match rec.history.split_last() {
                Some((last, rest)) if last.snapshot.as_ref() == Some(&rec.current) => (
                    rest,
                    Some(Box::new(VersionRecord {
                        version: last.version,
                        bump: last.bump,
                        at: last.at,
                        change_type: last.change_type,
                        raw_score: last.raw_score,
                        level: last.level,
                        changed_fields: last.changed_fields.clone(),
                        snapshot: None,
                        judgments: last.judgments.clone(),
                    })),
                ),
                _ => (rec.history.as_slice(), None),
            };
            push(
                &mut body,
                &SnapshotLine::Record {
                    entity: &rec.current,
                    deleted: rec.deleted,
                    history,
                    head,
                },
            )?;
            entries += 1;
        }
        let mut seqs: Vec<(&EntityId, &u64)> = self.seqs.iter().collect();
        seqs.sort();
        for (id, seq) in seqs {
            push(
                &mut body,
                &SnapshotLine::Seq {
                    id,
                    source_seq: *seq,
                },
            )?;
            entries += 1;
        }
        let mut hash = Fnv1a::new();
        hash.update(&body);
        Ok((body, hash, entries))
    }

    /// Puts one snapshot entry back. `false` if its id is already there.
    fn restore(&mut self, entry: SnapshotEntry) -> bool {
        match entry {
            SnapshotEntry::Seq { id, source_seq } => self.seqs.insert(id, source_seq).is_none(),
            SnapshotEntry::Record {
                entity,
                deleted,
                mut history,
                head,
            } => {
                let entity = *entity;
                if self.records.contains_key(&entity.id) {
                    return false;
                }
                if let Some(head) = head {
                    history.push(VersionRecord {
                        snapshot: Some(entity.clone()),
                        ..*head
                    });
                }
                if !deleted {
                    self.links.link(&entity);
                }
                let mut rec = Record {
                    current: entity,
                    deleted,
                    history,
                    revision: 0,
                    trimmed: 0,
                    stats: Arc::default(),
                };
                rec.retain_snapshots(self.history_limit);
                self.records.insert(rec.current.id.clone(), rec);
                true
            }
        }
    }

    fn apply(&mut self, op: JournalOp) {
        let keep = self.history_limit;
        match op {
            JournalOp::Seq { id, source_seq } => self.note_seq(&id, Some(source_seq)),
            // Read by `Journal::open`; they change nothing.
            JournalOp::Generation { .. } | JournalOp::Note { .. } => {}
            JournalOp::Put {
                entity,
                record,
                source_seq,
            } => {
                self.note_seq(&entity.id, source_seq);
                // The journal does not repeat the entity as the record's
                // snapshot: the snapshot of a version is the entity it stored.
                let record = record.map(|r| VersionRecord {
                    snapshot: Some(entity.clone()),
                    ..r
                });
                match self.records.get_mut(&entity.id) {
                    Some(rec) => {
                        self.links.unlink(&rec.current);
                        self.links.link(&entity);
                        rec.deleted = false;
                        rec.revision = rec.revision.wrapping_add(1);
                        match record {
                            Some(r) => {
                                rec.history.push(r);
                                rec.retain_snapshots(keep);
                            }
                            None => {
                                // Not a new version (a re-pin, an unversioned
                                // edit): keep the current version's snapshot
                                // equal to the live entity.
                                if let Some(last) = rec.history.last_mut()
                                    && last.change_type != ChangeType::Deleted
                                    && Some(last.version) == entity.version
                                {
                                    last.snapshot = Some(entity.clone());
                                }
                            }
                        }
                        // The kept analysis goes with the text it was made of.
                        if !same_text(&rec.current, &entity) {
                            rec.stats = Arc::default();
                        }
                        rec.current = entity;
                    }
                    None => {
                        self.links.link(&entity);
                        let mut rec = Record {
                            current: entity,
                            deleted: false,
                            history: record.into_iter().collect(),
                            revision: 0,
                            trimmed: 0,
                            stats: Arc::default(),
                        };
                        rec.retain_snapshots(keep);
                        self.records.insert(rec.current.id.clone(), rec);
                    }
                }
            }
            JournalOp::Delete {
                id,
                record,
                source_seq,
            } => {
                self.note_seq(&id, source_seq);
                if let Some(rec) = self.records.get_mut(&id) {
                    self.links.unlink(&rec.current);
                    rec.deleted = true;
                    // Nothing reads the text of a tombstone again.
                    rec.stats = Arc::default();
                    rec.revision = rec.revision.wrapping_add(1);
                    rec.history.push(VersionRecord {
                        snapshot: None,
                        ..record
                    });
                    rec.retain_snapshots(keep);
                }
            }
        }
    }

    /// The live neighbourhood of `seeds` within `depth` hops, without
    /// `exclude`: every entity at most once, sorted by id. The second list
    /// holds each entity's analysis cell, in the same order.
    fn context(
        &self,
        seeds: &[&str],
        exclude: &str,
        depth: u32,
    ) -> (Vec<Entity>, Vec<Arc<StatsCell>>) {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        for s in seeds {
            if seen.insert((*s).to_owned()) {
                queue.push_back(((*s).to_owned(), 0));
            }
        }
        let mut out: Vec<(Entity, Arc<StatsCell>)> = Vec::new();
        while let Some((id, d)) = queue.pop_front() {
            let mut next: Vec<String> = Vec::new();
            if let Some(rec) = self.records.get(&id).filter(|r| !r.deleted) {
                let e = &rec.current;
                if id != exclude {
                    out.push((e.clone(), Arc::clone(&rec.stats)));
                    if out.len() >= MAX_CONTEXT {
                        break;
                    }
                }
                if d >= depth {
                    continue;
                }
                next.extend(e.parent_id.iter().cloned());
                next.extend(e.relations.iter().map(|r| r.target.clone()));
            } else if d != 0 || d >= depth {
                continue;
            }
            // A seed that is not stored (a new entity) still expands from its links.
            if let Some(c) = self.links.children.get(&id) {
                next.extend(c.iter().cloned());
            }
            if let Some(r) = self.links.referrers.get(&id) {
                next.extend(r.iter().cloned());
            }
            for n in next {
                if seen.insert(n.clone()) {
                    queue.push_back((n, d + 1));
                }
            }
        }
        out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        out.into_iter().unzip()
    }

    fn history_points(&self, id: &str) -> Vec<HistoryPoint> {
        self.records
            .get(id)
            .map(|r| {
                r.history
                    .iter()
                    .filter(|h| h.change_type == ChangeType::Updated)
                    .map(|h| HistoryPoint {
                        entity_id: id.to_owned(),
                        at: h.at,
                        magnitude: h.raw_score,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Pins for `e` on what it depends on: its parent, its blockers (and
    /// whatever says it blocks `e`), at their current versions. Never on `e`.
    fn auto_pins(&self, e: &Entity) -> Vec<BasisPin> {
        let mut targets: BTreeSet<&str> = BTreeSet::new();
        if let Some(p) = &e.parent_id {
            targets.insert(p);
        }
        for r in &e.relations {
            if r.kind == RelationKind::BlockedBy {
                targets.insert(&r.target);
            }
        }
        if let Some(refs) = self.links.referrers.get(&e.id) {
            for src in refs {
                if let Some(other) = self.live(src)
                    && other
                        .relations
                        .iter()
                        .any(|r| r.kind == RelationKind::Blocks && r.target == e.id)
                {
                    targets.insert(src);
                }
            }
        }
        targets.remove(e.id.as_str());
        targets
            .into_iter()
            .filter_map(|t| {
                self.live(t).and_then(|x| x.version).map(|v| BasisPin {
                    entity_id: t.to_owned(),
                    version: v,
                })
            })
            .collect()
    }

    /// The stored entities that depend on `e` (its children, what it blocks,
    /// what says it is blocked by `e`) and hold no pin on it, each with a pin
    /// on `e` at its version added: the other half of [`State::auto_pins`],
    /// for dependents that were put before `e`.
    fn backfill_pins(&self, e: &Entity) -> Vec<JournalOp> {
        let Some(version) = e.version else {
            return Vec::new();
        };
        let mut dependents: BTreeSet<&str> = BTreeSet::new();
        if let Some(kids) = self.links.children.get(&e.id) {
            dependents.extend(kids.iter().map(String::as_str));
        }
        if let Some(refs) = self.links.referrers.get(&e.id) {
            for src in refs {
                if self.live(src).is_some_and(|other| {
                    other
                        .relations
                        .iter()
                        .any(|r| r.kind == RelationKind::BlockedBy && r.target == e.id)
                }) {
                    dependents.insert(src);
                }
            }
        }
        for r in &e.relations {
            if r.kind == RelationKind::Blocks {
                dependents.insert(&r.target);
            }
        }
        dependents.remove(e.id.as_str());
        dependents
            .into_iter()
            .filter_map(|id| self.live(id))
            .filter(|d| d.pin_for(&e.id).is_none())
            .map(|d| {
                let mut updated = d.clone();
                updated.basis.push(BasisPin {
                    entity_id: e.id.clone(),
                    version,
                });
                normalize(&mut updated);
                JournalOp::Put {
                    entity: updated,
                    record: None,
                    source_seq: None,
                }
            })
            .collect()
    }
}

/// Puts an entity in the canonical form the registry stores: a well-known
/// kind as its variant, relations sorted and de-duplicated (they are a set),
/// the basis sorted by entity with one pin per entity (the first given) and
/// no pin on the entity itself.
fn normalize(e: &mut Entity) {
    if matches!(e.kind, EntityKind::Custom(_)) {
        e.kind = EntityKind::from(e.kind.as_str());
    }
    e.relations.sort();
    e.relations.dedup();
    let id = &e.id;
    e.basis.retain(|p| &p.entity_id != id);
    e.basis.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
    e.basis
        .dedup_by(|later, earlier| later.entity_id == earlier.entity_id);
}

/// Whether `after` (canonical) stores the same thing as `stored`, ignoring
/// `updated_at`.
fn same_content(stored: &Entity, after: &Entity) -> bool {
    let Entity {
        id,
        kind,
        parent_id,
        version,
        fields,
        relations,
        basis,
        embedding,
        updated_at: _,
    } = stored;
    if *id != after.id
        || *kind != after.kind
        || *parent_id != after.parent_id
        || *version != after.version
        || *fields != after.fields
        || *embedding != after.embedding
    {
        return false;
    }
    // Entities stored before canonicalisation may hold these in another order.
    let mut relations = relations.clone();
    relations.sort();
    relations.dedup();
    let mut pins = Entity::new(id.clone(), kind.clone());
    pins.basis = basis.clone();
    normalize(&mut pins);
    relations == after.relations && pins.basis == after.basis
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum JournalOp {
    Put {
        entity: Entity,
        /// Written without its snapshot (the entity is the snapshot); a
        /// snapshot in older journals is ignored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<VersionRecord>,
        /// Absent in journals written before 0.3, and for writes without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_seq: Option<u64>,
    },
    Delete {
        id: EntityId,
        record: VersionRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_seq: Option<u64>,
    },
    /// A newer source sequence for an id whose content did not change (a
    /// duplicate put, or the deletion of something that is not stored).
    Seq { id: EntityId, source_seq: u64 },
    /// The first line of a journal that continues a snapshot: the journal's
    /// generation (see the module docs). Never anywhere else.
    Generation { generation: u64 },
    /// The second line of such a journal: a remark for whoever reads the
    /// file, ignored on load. It is there so that the generation line is
    /// never the last line: 0.1 and 0.2 drop an unknown last line as a torn
    /// write, and would open a freshly compacted store as an empty one. Two
    /// unknown lines make them fail instead.
    Note { note: String },
}

/// The text of the [`JournalOp::Note`] after a generation line.
const JOURNAL_NOTE: &str = "this journal continues the .snapshot file next to it; versions without snapshots (0.1, 0.2) cannot read it";

#[derive(Debug)]
struct Journal {
    file: File,
    path: PathBuf,
    fsync: bool,
    /// The length of the file up to the last acknowledged write.
    len: u64,
    /// A write failed and could not be undone: the file no longer matches memory.
    failed: bool,
    /// Something was written since the last sync (only tracked with `fsync`).
    unsynced: bool,
    /// Which journal this is: 0 until the first compaction, then one more
    /// with every compaction.
    generation: u64,
    /// The operations this journal holds on top of the snapshot.
    ops: u64,
    /// After a failed automatic compaction: no new attempt below this many
    /// operations.
    auto_floor: u64,
    /// Test-only: how often the file was synced to disk.
    #[cfg(test)]
    syncs: usize,
}

/// What a loaded snapshot says about the journal it was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Covered {
    generation: u64,
    journal_len: u64,
}

/// FNV-1a, 64 bit: the checksum of a snapshot.
#[derive(Debug, Clone, Copy)]
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// The format version this build writes and reads.
const SNAPSHOT_FORMAT: u32 = 1;
/// The `snapshot` value of every header: what kind of file this is.
const SNAPSHOT_MAGIC: &str = "assessment-engine-registry";

/// The first line of a snapshot file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotHeader {
    snapshot: String,
    format: u32,
    /// The generation of the journal the snapshot was taken from.
    generation: u64,
    /// How many bytes of that journal the snapshot holds.
    journal_len: u64,
    /// The number of body lines.
    entries: u64,
    /// `fnv1a64:` and 16 hex digits, over the four numbers above and the body.
    checksum: String,
}

impl SnapshotHeader {
    fn new(covered: Covered, entries: u64, body: Fnv1a) -> Self {
        let mut header = Self {
            snapshot: SNAPSHOT_MAGIC.to_owned(),
            format: SNAPSHOT_FORMAT,
            generation: covered.generation,
            journal_len: covered.journal_len,
            entries,
            checksum: String::new(),
        };
        header.checksum = header.checksum_of(body);
        header
    }

    /// The checksum of this header's numbers and a body with the hash `body`.
    /// The numbers go in after the body, so the body can be hashed first.
    fn checksum_of(&self, mut body: Fnv1a) -> String {
        body.update(
            format!(
                "\n{} {} {} {}",
                self.format, self.generation, self.journal_len, self.entries
            )
            .as_bytes(),
        );
        format!("fnv1a64:{:016x}", body.0)
    }
}

/// One body line of a snapshot, as written.
#[derive(Serialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum SnapshotLine<'a> {
    Record {
        entity: &'a Entity,
        #[serde(skip_serializing_if = "is_false")]
        deleted: bool,
        history: &'a [VersionRecord],
        /// The newest version when its snapshot is `entity` itself, written
        /// without the snapshot so the entity is stored once.
        #[serde(skip_serializing_if = "Option::is_none")]
        head: Option<Box<VersionRecord>>,
    },
    Seq {
        id: &'a str,
        source_seq: u64,
    },
}

/// One body line of a snapshot, as read. Mirrors [`SnapshotLine`].
#[derive(Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum SnapshotEntry {
    Record {
        entity: Box<Entity>,
        #[serde(default)]
        deleted: bool,
        #[serde(default)]
        history: Vec<VersionRecord>,
        #[serde(default)]
        head: Option<Box<VersionRecord>>,
    },
    Seq {
        id: EntityId,
        source_seq: u64,
    },
}

/// `<journal><suffix>`: the files that live next to a journal.
fn sibling(journal: &Path, suffix: &str) -> PathBuf {
    let mut name = journal.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Syncs the directory that holds `path`, so a rename in it survives a
/// power cut. Only Unix can sync a directory.
#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    DIR_SYNCS.with(|n| n.set(n.get() + 1));
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

#[cfg(test)]
thread_local! {
    /// Test-only: how often this thread synced a directory.
    static DIR_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    DIR_SYNCS.with(|n| n.set(n.get() + 1));
    Ok(())
}

fn snapshot_error(path: &Path, what: &str) -> Error {
    Error::new(
        ErrorKind::Storage,
        format!("the snapshot `{}` {what}", path.display()),
    )
}

/// Loads the snapshot of the journal at `journal` into `state` (which must
/// be empty). `None` if there is no snapshot. Any defect fails: the caller
/// must drop `state` then, it may hold part of the snapshot.
fn load_snapshot(journal: &Path, state: &mut State) -> Result<Option<Covered>> {
    let path = sibling(journal, ".snapshot");
    let io = |what: &str, e: std::io::Error| {
        Error::with_source(
            ErrorKind::Storage,
            format!("the snapshot `{}` could not be {what}", path.display()),
            e,
        )
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io("opened", e)),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| io("read", e))?;
    if line.last() != Some(&b'\n') {
        return Err(snapshot_error(
            &path,
            "is cut short: it has no complete header",
        ));
    }
    let header: SnapshotHeader = serde_json::from_slice(&line).map_err(|e| {
        Error::with_source(
            ErrorKind::Storage,
            format!("the snapshot `{}` has an unreadable header", path.display()),
            e,
        )
    })?;
    if header.snapshot != SNAPSHOT_MAGIC {
        return Err(snapshot_error(&path, "is not a registry snapshot"));
    }
    if header.format != SNAPSHOT_FORMAT {
        return Err(snapshot_error(
            &path,
            &format!(
                "has format {}, and this version reads format {SNAPSHOT_FORMAT}",
                header.format
            ),
        ));
    }
    let mut hash = Fnv1a::new();
    let mut entries: u64 = 0;
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| io("read", e))?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return Err(snapshot_error(
                &path,
                "is cut short: its last line is incomplete",
            ));
        }
        hash.update(&line);
        entries += 1;
        if entries > header.entries {
            // The count is wrong already; do not parse what should not be there.
            continue;
        }
        let entry: SnapshotEntry = serde_json::from_slice(&line).map_err(|e| {
            Error::with_source(
                ErrorKind::Storage,
                format!(
                    "entry {entries} of the snapshot `{}` is corrupt",
                    path.display()
                ),
                e,
            )
        })?;
        if !state.restore(entry) {
            return Err(snapshot_error(
                &path,
                &format!("is corrupt: entry {entries} repeats an id"),
            ));
        }
    }
    if entries != header.entries {
        return Err(snapshot_error(
            &path,
            &format!(
                "is corrupt or cut short: it holds {entries} entries, and its header counts {}",
                header.entries
            ),
        ));
    }
    if header.checksum_of(hash) != header.checksum {
        return Err(snapshot_error(
            &path,
            "is corrupt: its checksum does not match",
        ));
    }
    Ok(Some(Covered {
        generation: header.generation,
        journal_len: header.journal_len,
    }))
}

/// Writes the snapshot of the journal at `journal`: a temporary file next to
/// it, synced, then renamed into place, then the directory synced. Returns
/// the size of the file.
fn write_snapshot(journal: &Path, header: &SnapshotHeader, body: &[u8]) -> Result<u64> {
    let path = sibling(journal, ".snapshot");
    let tmp = sibling(journal, ".snapshot.tmp");
    let io = |e: std::io::Error| {
        Error::with_source(
            ErrorKind::Storage,
            format!("the snapshot `{}` could not be written", path.display()),
            e,
        )
    };
    let mut head = serde_json::to_vec(header).map_err(|e| {
        Error::with_source(
            ErrorKind::Storage,
            "a snapshot header could not be encoded",
            e,
        )
    })?;
    head.push(b'\n');
    let written = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&head)?;
        file.write_all(body)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        sync_dir(&path)
    })();
    if let Err(e) = written {
        // If the rename did not happen, the old snapshot is still in place.
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok((head.len() + body.len()) as u64)
}

impl Journal {
    /// Opens the journal at `path` (creating it if missing), streams it into
    /// `state`, and repairs its tail so that the next append starts a line:
    /// an unreadable last line (a torn write: cut JSON, a cut UTF-8
    /// character) is truncated, and a last record without its newline gets one.
    ///
    /// `snapshot` is what the snapshot already loaded into `state` covers.
    /// The part of the journal it holds is skipped, and a journal that it
    /// holds completely is restarted (see the module docs).
    fn open(
        path: PathBuf,
        fsync: bool,
        state: &mut State,
        snapshot: Option<Covered>,
    ) -> Result<Self> {
        let io = |what: &str, e: std::io::Error| {
            Error::with_source(
                ErrorKind::Storage,
                format!("the journal `{}` could not be {what}", path.display()),
                e,
            )
        };
        let mismatch = |what: String| {
            Error::new(
                ErrorKind::Storage,
                format!(
                    "the journal `{}` and its snapshot do not belong together: {what}",
                    path.display()
                ),
            )
        };
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| io("opened", e))?;
        let mut torn_at = None;
        let mut missing_newline = false;
        // Known once the first record is read.
        let mut generation: Option<u64> = None;
        // The journal up to here is in the snapshot.
        let mut skip: u64 = 0;
        let mut ops: u64 = 0;
        // The generation line is the last line: its note is missing.
        let mut bare_marker = false;
        {
            let mut reader = BufReader::new(&file);
            let mut line = Vec::new();
            let mut offset: u64 = 0;
            let mut number: usize = 0;
            // A line that does not parse: fatal unless nothing but blank lines follows it.
            let mut unreadable: Option<(usize, u64, serde_json::Error)> = None;
            loop {
                line.clear();
                let n = reader
                    .read_until(b'\n', &mut line)
                    .map_err(|e| io("read", e))?;
                if n == 0 {
                    break;
                }
                number += 1;
                let start = offset;
                offset += n as u64;
                let body = line.trim_ascii();
                if body.is_empty() {
                    continue;
                }
                if let Some((at, _, e)) = unreadable.take() {
                    return Err(Error::with_source(
                        ErrorKind::Storage,
                        format!("line {at} of the journal `{}` is corrupt", path.display()),
                        e,
                    ));
                }
                if generation.is_some() && start < skip {
                    if offset > skip {
                        return Err(mismatch(format!(
                            "the snapshot covers {skip} bytes, which ends inside line {number}"
                        )));
                    }
                    // In the snapshot already.
                    continue;
                }
                let op = match serde_json::from_slice::<JournalOp>(body) {
                    Ok(op) => op,
                    Err(e) => {
                        unreadable = Some((number, start, e));
                        continue;
                    }
                };
                missing_newline = line.last() != Some(&b'\n');
                let marker = match &op {
                    JournalOp::Generation { generation } => Some(*generation),
                    _ => None,
                };
                if generation.is_none() {
                    let g = marker.unwrap_or(0);
                    skip = match (snapshot, g) {
                        (None, 0) => 0,
                        (None, g) => {
                            return Err(Error::new(
                                ErrorKind::Storage,
                                format!(
                                    "the journal `{}` continues a snapshot (generation {}), and `{}` is missing",
                                    path.display(),
                                    g - 1,
                                    sibling(&path, ".snapshot").display()
                                ),
                            ));
                        }
                        (Some(s), g) if Some(g) == s.generation.checked_add(1) => 0,
                        (Some(s), g) if g == s.generation => s.journal_len,
                        (Some(s), g) => {
                            return Err(mismatch(format!(
                                "the journal is generation {g}, and the snapshot was taken from generation {}",
                                s.generation
                            )));
                        }
                    };
                    generation = Some(g);
                } else if marker.is_some() {
                    return Err(Error::new(
                        ErrorKind::Storage,
                        format!(
                            "line {number} of the journal `{}` is corrupt: a generation line that is not the first line",
                            path.display()
                        ),
                    ));
                }
                bare_marker = marker.is_some();
                if marker.is_some() || matches!(op, JournalOp::Note { .. }) {
                    continue;
                }
                if start < skip {
                    if offset > skip {
                        return Err(mismatch(format!(
                            "the snapshot covers {skip} bytes, which ends inside line {number}"
                        )));
                    }
                    continue;
                }
                state.apply(op);
                ops += 1;
            }
            if let Some((_, start, _)) = unreadable {
                torn_at = Some(start);
            }
        }
        let repaired = if let Some(start) = torn_at {
            // A torn final write (crash mid-append) is dropped.
            file.set_len(start).map_err(|e| io("repaired", e))?;
            true
        } else if missing_newline {
            file.write_all(b"\n").map_err(|e| io("repaired", e))?;
            true
        } else {
            false
        };
        if repaired && fsync {
            file.sync_data().map_err(|e| io("repaired", e))?;
        }
        let mut len = file.metadata().map_err(|e| io("read", e))?.len();
        // A journal that the snapshot holds completely (a compaction that
        // was cut off before its last step, or a journal that lost its
        // unsynced tail), and an empty journal next to a snapshot, start
        // over as the next generation.
        let restart = match (generation, snapshot) {
            (Some(g), Some(s)) if g == s.generation && len <= s.journal_len => {
                s.generation.checked_add(1)
            }
            (None, Some(s)) => s.generation.checked_add(1),
            _ => None,
        };
        if let Some(next) = restart {
            let head = Self::head(next)?;
            // The snapshot is all there is once the journal is cut, so its
            // rename must be on disk first: the compaction that wrote it may
            // have been killed before it synced the directory.
            sync_dir(&path)
                .and_then(|()| file.set_len(0))
                .and_then(|()| file.write_all(&head))
                .and_then(|()| file.sync_data())
                .map_err(|e| io("restarted", e))?;
            len = head.len() as u64;
            ops = 0;
        } else if bare_marker {
            // A restart that was cut off after its first line.
            let note = Self::note()?;
            file.write_all(&note)
                .and_then(|()| file.sync_data())
                .map_err(|e| io("repaired", e))?;
            len += note.len() as u64;
        }
        Ok(Self {
            file,
            path,
            fsync,
            len,
            failed: false,
            unsynced: false,
            generation: restart.or(generation).unwrap_or(0),
            ops,
            auto_floor: 0,
            #[cfg(test)]
            syncs: 0,
        })
    }

    fn line(op: &JournalOp) -> Result<Vec<u8>> {
        let mut line = serde_json::to_vec(op).map_err(|e| {
            Error::with_source(
                ErrorKind::Storage,
                "a journal entry could not be encoded",
                e,
            )
        })?;
        line.push(b'\n');
        Ok(line)
    }

    /// The note that follows a generation line.
    fn note() -> Result<Vec<u8>> {
        Self::line(&JournalOp::Note {
            note: JOURNAL_NOTE.to_owned(),
        })
    }

    /// The first two lines of a journal of `generation`: the generation
    /// line and its note.
    fn head(generation: u64) -> Result<Vec<u8>> {
        let mut head = Self::line(&JournalOp::Generation { generation })?;
        head.extend(Self::note()?);
        Ok(head)
    }

    /// The last step of a compaction: replaces the journal by one of the
    /// next generation that holds only what was appended after `covered`
    /// bytes (what the snapshot does not have). Returns the number of
    /// operations carried over.
    ///
    /// The new journal is written next to the old one, synced and renamed
    /// over it. A failure before the rename leaves the journal as it was. A
    /// failure after it leaves this handle on a file that is no longer the
    /// journal, so further writes are refused.
    fn restart(&mut self, covered: u64) -> Result<u64> {
        let path = self.path.clone();
        let io = |e: std::io::Error| {
            Error::with_source(
                ErrorKind::Storage,
                format!("the journal `{}` could not be restarted", path.display()),
                e,
            )
        };
        let next = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorKind::Storage, "the journal generation is exhausted"))?;
        let mut bytes = Self::head(next)?;
        let marker_len = bytes.len();
        let expected = self.len.saturating_sub(covered);
        (|| {
            let mut old = File::open(&path)?;
            old.seek(SeekFrom::Start(covered))?;
            old.take(expected).read_to_end(&mut bytes)?;
            Ok(())
        })()
        .map_err(io)?;
        let tail = &bytes[marker_len..];
        if tail.len() as u64 != expected || tail.last().is_some_and(|b| *b != b'\n') {
            return Err(Error::new(
                ErrorKind::Storage,
                format!(
                    "the journal `{}` changed on disk while it was compacted",
                    path.display()
                ),
            ));
        }
        let carried = tail.iter().filter(|b| **b == b'\n').count() as u64;
        let tmp = sibling(&path, ".tmp");
        let prepared = (|| {
            let mut file = File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = prepared {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        let swapped =
            sync_dir(&path).and_then(|()| OpenOptions::new().read(true).append(true).open(&path));
        match swapped {
            Ok(file) => {
                self.file = file;
                self.len = bytes.len() as u64;
                self.generation = next;
                self.ops = carried;
                self.auto_floor = 0;
                self.unsynced = false;
                Ok(carried)
            }
            Err(e) => {
                self.failed = true;
                Err(io(e))
            }
        }
    }

    /// Appends `ops`, all or nothing: a write that fails part-way is
    /// truncated away again, so the file never holds what memory does not.
    /// With `sync` false the write is not synced to disk even when the
    /// journal syncs: the caller batches writes and calls [`Journal::sync`].
    fn append(&mut self, ops: &[JournalOp], sync: bool) -> Result<()> {
        if self.failed {
            return Err(Error::new(
                ErrorKind::Storage,
                format!(
                    "the journal `{}` no longer matches the registry after a failed write; reopen the registry",
                    self.path.display()
                ),
            ));
        }
        let mut bytes = Vec::new();
        for op in ops {
            serde_json::to_writer(&mut bytes, op).map_err(|e| {
                Error::with_source(
                    ErrorKind::Storage,
                    "a journal entry could not be encoded",
                    e,
                )
            })?;
            bytes.push(b'\n');
        }
        if bytes.is_empty() {
            return Ok(());
        }
        match self.write(&bytes, sync) {
            Ok(()) => {
                self.len += bytes.len() as u64;
                self.ops += ops.len() as u64;
                Ok(())
            }
            Err(e) => {
                let undone = self.file.set_len(self.len).and_then(|()| {
                    if self.fsync && sync {
                        self.file.sync_data()
                    } else {
                        Ok(())
                    }
                });
                if undone.is_err() {
                    self.failed = true;
                }
                Err(Error::with_source(
                    ErrorKind::Storage,
                    format!("the journal `{}` could not be written", self.path.display()),
                    e,
                ))
            }
        }
    }

    fn write(&mut self, bytes: &[u8], sync: bool) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.file.flush()?;
        if self.fsync {
            if sync {
                self.sync_data()?;
            } else {
                self.unsynced = true;
            }
        }
        Ok(())
    }

    fn sync_data(&mut self) -> std::io::Result<()> {
        self.file.sync_data()?;
        self.unsynced = false;
        #[cfg(test)]
        {
            self.syncs += 1;
        }
        Ok(())
    }

    /// Syncs what [`Journal::append`] wrote without syncing. If that fails,
    /// memory is ahead of what is safely on disk and cannot be taken back,
    /// so the journal refuses further writes.
    fn sync(&mut self) -> Result<()> {
        if !self.fsync || !self.unsynced || self.failed {
            return Ok(());
        }
        self.sync_data().map_err(|e| {
            self.failed = true;
            Error::with_source(
                ErrorKind::Storage,
                format!("the journal `{}` could not be synced", self.path.display()),
                e,
            )
        })
    }
}

/// A thread-safe, versioned entity registry.
#[derive(Debug)]
pub struct Registry {
    engine: Engine,
    state: RwLock<State>,
    journal: Option<Mutex<Journal>>,
    /// Compact once the journal holds this many operations.
    compact_after: Option<u64>,
    /// Held by the one compaction that runs.
    compacting: Mutex<()>,
    /// Why the last automatic compaction failed, until it is taken.
    compaction_error: Mutex<Option<Error>>,
    /// Keep the text analysis of stored entities between assessments.
    analysis_cache: bool,
    /// Test-only: holds the next assessment of this registry (see [`gate`]).
    #[cfg(test)]
    gate: gate::Gate,
}

/// How to open a journal-backed registry ([`Registry::open_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegistryOptions {
    /// Sync every write to disk. Off by default: a write then survives a
    /// crash of the process, and a power cut can lose the last writes.
    /// Snapshots are always synced.
    pub fsync: bool,
    /// Keep the snapshot of only this many newest versions per entity
    /// (`None` = all; at least 1). See [`Registry::with_history_limit`]. It
    /// is applied while loading, so the full history is never held in memory.
    pub history_limit: Option<usize>,
    /// Compact (see [`Registry::compact`]) when the journal holds this many
    /// operations on top of the snapshot (`None` = never; at least 1). The
    /// count includes what was replayed on open. The compaction runs on the
    /// thread of the write that reaches the count, after that write is
    /// acknowledged; other writers wait only while the state is copied, and
    /// readers do not wait for the file to be written.
    pub compact_after_ops: Option<u64>,
}

/// What a compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    /// The generation of the journal that was started.
    pub generation: u64,
    /// The lines in the snapshot: one per entity that was ever stored, and
    /// one per stored source sequence.
    pub entries: u64,
    /// The size of the snapshot file.
    pub snapshot_bytes: u64,
    /// Operations written while the snapshot was being saved; they were
    /// carried into the new journal.
    pub carried_ops: u64,
}

impl Registry {
    /// An in-memory registry.
    pub fn new(engine: Engine) -> Self {
        Self::assemble(engine, State::default(), None, None)
    }

    fn assemble(
        engine: Engine,
        state: State,
        journal: Option<Journal>,
        compact_after: Option<u64>,
    ) -> Self {
        Self {
            engine,
            state: RwLock::new(state),
            journal: journal.map(Mutex::new),
            compact_after: compact_after.map(|n| n.max(1)),
            compacting: Mutex::new(()),
            compaction_error: Mutex::new(None),
            analysis_cache: true,
            #[cfg(test)]
            gate: gate::Gate::default(),
        }
    }

    /// A registry backed by the journal at `path` (created if missing,
    /// replayed if present). With `fsync`, every write is synced to disk.
    ///
    /// If `<path>.snapshot` exists (see [`Registry::compact`]), it is loaded
    /// first and the journal is replayed on top of it. A snapshot that is
    /// corrupt or cut short, or that does not belong to the journal, fails
    /// with [`ErrorKind::Storage`]; part of a snapshot is never loaded.
    ///
    /// A torn last line (a crash mid-write) is dropped and truncated, and a
    /// last record that lost its newline gets it back, before anything new
    /// is appended; corruption anywhere else fails with [`ErrorKind::Storage`].
    pub fn open(engine: Engine, path: impl AsRef<Path>, fsync: bool) -> Result<Self> {
        Self::open_with(
            engine,
            path,
            RegistryOptions {
                fsync,
                ..RegistryOptions::default()
            },
        )
    }

    /// Like [`Registry::open`], with every option.
    pub fn open_with(
        engine: Engine,
        path: impl AsRef<Path>,
        options: RegistryOptions,
    ) -> Result<Self> {
        let path = path.as_ref();
        let mut state = State {
            history_limit: options.history_limit.map(|n| n.max(1)),
            ..State::default()
        };
        // What an interrupted compaction left behind; it was never read.
        let _ = std::fs::remove_file(sibling(path, ".snapshot.tmp"));
        let _ = std::fs::remove_file(sibling(path, ".tmp"));
        let snapshot = load_snapshot(path, &mut state)?;
        let journal = Journal::open(path.to_path_buf(), options.fsync, &mut state, snapshot)?;
        Ok(Self::assemble(
            engine,
            state,
            Some(journal),
            options.compact_after_ops,
        ))
    }

    /// Like [`Registry::open`], with [`Registry::with_history_limit`]
    /// applied while replaying, so the full history is never held in memory.
    pub fn open_with_history_limit(
        engine: Engine,
        path: impl AsRef<Path>,
        fsync: bool,
        snapshots: usize,
    ) -> Result<Self> {
        Self::open_with(
            engine,
            path,
            RegistryOptions {
                fsync,
                history_limit: Some(snapshots),
                ..RegistryOptions::default()
            },
        )
    }

    /// Keeps the snapshot of only the newest `snapshots` versions of each
    /// entity (at least 1, so the current version is always there); older
    /// versions keep their [`VersionRecord`] without its snapshot, and
    /// [`Registry::get`] / [`Registry::compare`] on them fail with
    /// [`ErrorKind::NotFound`]. By default every snapshot is kept, so memory
    /// grows with the number of versions. Applies to what is already loaded too.
    pub fn with_history_limit(mut self, snapshots: usize) -> Self {
        self.state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .set_history_limit(Some(snapshots.max(1)));
        self
    }

    /// Turns the analysis cache on or off (on by default).
    ///
    /// With the cache, the registry keeps the term counts of every stored
    /// entity's title and description once an assessment has read them, so
    /// a later assessment analyses only the text that changed. The counts
    /// are computed with no registry lock held, are dropped when the
    /// entity's text changes or the entity is deleted, and are not written
    /// to disk: after
    /// [`Registry::open`] they are rebuilt as assessments read them.
    ///
    /// Assessments are the same with and without the cache, bit for bit.
    /// Turning it off saves the memory the counts take.
    pub fn with_analysis_cache(mut self, enabled: bool) -> Self {
        self.analysis_cache = enabled;
        self
    }

    /// Whether the analysis cache is on.
    pub fn analysis_cache(&self) -> bool {
        self.analysis_cache
    }

    /// The number of newest versions per entity whose snapshots are kept
    /// (`None` = all).
    pub fn history_limit(&self) -> Option<usize> {
        self.read().history_limit
    }

    /// The number of journal operations after which the registry compacts
    /// on its own (`None` = never). See [`RegistryOptions::compact_after_ops`].
    pub fn compact_after_ops(&self) -> Option<u64> {
        self.compact_after
    }

    /// The number of operations the journal holds on top of the snapshot
    /// (0 for an in-memory registry).
    pub fn journal_ops(&self) -> u64 {
        match &self.journal {
            Some(j) => j.lock().unwrap_or_else(PoisonError::into_inner).ops,
            None => 0,
        }
    }

    /// Folds the journal into `<journal>.snapshot` and starts a fresh
    /// journal, so the next [`Registry::open`] loads the snapshot and replays
    /// only what was written since. Returns `None`, and does nothing, for an
    /// in-memory registry.
    ///
    /// The snapshot holds the full state: every entity (live or deleted)
    /// with its history, and every stored source sequence. It is written to
    /// a temporary file, synced and renamed into place, whatever the `fsync`
    /// option says. Readers are not held up. Writers wait while the state is
    /// serialised to memory, and again briefly when the journal is swapped;
    /// they do not wait for the snapshot file to be written. What they write
    /// meanwhile is carried into the new journal.
    ///
    /// A crash at any point leaves files that open to the acknowledged
    /// state; the module docs go through each case. If this fails, nothing
    /// is lost: the registry keeps the journal it had, or (if the journal
    /// swap failed half-way) refuses writes until it is reopened.
    pub fn compact(&self) -> Result<Option<Compaction>> {
        let Some(journal) = &self.journal else {
            return Ok(None);
        };
        let _one = self
            .compacting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.compact_journal(journal).map(Some)
    }

    /// One compaction; the caller holds `compacting`.
    fn compact_journal(&self, journal: &Mutex<Journal>) -> Result<Compaction> {
        // Writers hold the exclusive lock while they append, so under the
        // shared lock the journal length and the state belong together.
        let (path, covered, body, hash, entries) = {
            let state = self.read();
            let (path, covered) = {
                let j = journal.lock().unwrap_or_else(PoisonError::into_inner);
                if j.failed {
                    return Err(Error::new(
                        ErrorKind::Storage,
                        format!(
                            "the journal `{}` no longer matches the registry after a failed write; reopen the registry",
                            j.path.display()
                        ),
                    ));
                }
                (
                    j.path.clone(),
                    Covered {
                        generation: j.generation,
                        journal_len: j.len,
                    },
                )
            };
            let (body, hash, entries) = state.snapshot_body()?;
            (path, covered, body, hash, entries)
        };
        let header = SnapshotHeader::new(covered, entries, hash);
        let snapshot_bytes = write_snapshot(&path, &header, &body)?;
        drop(body);
        let mut j = journal.lock().unwrap_or_else(PoisonError::into_inner);
        let carried_ops = j.restart(covered.journal_len)?;
        Ok(Compaction {
            generation: j.generation,
            entries,
            snapshot_bytes,
            carried_ops,
        })
    }

    /// Compacts if [`RegistryOptions::compact_after_ops`] is reached. Called
    /// after a write, with no lock held. A failure is kept for
    /// [`Registry::take_compaction_error`], and the next attempt waits for
    /// as many operations again.
    fn compact_if_due(&self) {
        let (Some(after), Some(journal)) = (self.compact_after, &self.journal) else {
            return;
        };
        let due = |j: &Journal| !j.failed && j.ops >= after.max(j.auto_floor);
        if !due(&journal.lock().unwrap_or_else(PoisonError::into_inner)) {
            return;
        }
        // Another thread is compacting already: nothing to do.
        let _one = match self.compacting.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        if !due(&journal.lock().unwrap_or_else(PoisonError::into_inner)) {
            return;
        }
        if let Err(e) = self.compact_journal(journal) {
            let mut j = journal.lock().unwrap_or_else(PoisonError::into_inner);
            j.auto_floor = j.ops.saturating_add(after);
            drop(j);
            *self
                .compaction_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(e);
        }
    }

    /// Why the last automatic compaction failed, if one did since this was
    /// last called. The write that set it off was stored all the same.
    pub fn take_compaction_error(&self) -> Option<Error> {
        self.compaction_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Number of live (not deleted) entities.
    pub fn len(&self) -> usize {
        self.read().records.values().filter(|r| !r.deleted).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `ops` to the journal, if there is one. `sync` false leaves
    /// the sync to a later [`Registry::sync_journal`] (an import's batch).
    fn persist(&self, ops: &[JournalOp], sync: bool) -> Result<()> {
        match &self.journal {
            Some(j) => j
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .append(ops, sync),
            None => Ok(()),
        }
    }

    fn sync_journal(&self) -> Result<()> {
        match &self.journal {
            Some(j) => j.lock().unwrap_or_else(PoisonError::into_inner).sync(),
            None => Ok(()),
        }
    }

    /// The stand-in assessment of a put or delete the engine never saw.
    fn not_assessed(&self, summary: &str, now: DateTime<Utc>) -> Assessment {
        Assessment {
            score: 0.0,
            level: ImpactLevel::None,
            recommendation: Recommendation::NoAction,
            confidence: 1.0,
            severity: 0.0,
            breadth: 0.0,
            effort_at_risk: 0.0,
            effort_total: 0.0,
            replan_root: None,
            changes: Vec::new(),
            impacts: Vec::new(),
            impacts_total: 0,
            violations: Vec::new(),
            summary: summary.to_owned(),
            engine_version: crate::ENGINE_VERSION.to_owned(),
            assessed_at: now,
            judgment_requests: Vec::new(),
            unmatched_judgments: Vec::new(),
        }
    }

    /// Runs the engine. Every assessment of the registry goes through here,
    /// so the tests can hold one mid-way and check that no lock is held.
    ///
    /// With the analysis cache on, the engine reads the term counts of the
    /// stored entities from `cells` and fills the cells that are empty.
    fn assess(&self, request: &AssessmentRequest, cells: &Cells) -> Result<Assessment> {
        #[cfg(test)]
        self.gate.pass();
        if !self.analysis_cache {
            return self.engine.assess(request);
        }
        let mut prepared = PreparedStats::new(self.engine.shared_policy());
        for (entity, cell) in request.context.iter().zip(&cells.context) {
            prepared.context(&entity.id, cell);
        }
        prepared.change(0, cells.before.as_deref(), cells.after.as_deref());
        self.engine.assess_prepared(request, &prepared)
    }

    fn depth(&self) -> u32 {
        self.engine.policy().propagation.max_depth.max(1) + 1
    }

    /// Creates or updates an entity: assesses the change against the stored
    /// version and, unless `dry_run`, stores it under its next version.
    ///
    /// The entity is stored in a canonical form: a well-known kind as its
    /// variant, relations sorted and de-duplicated, the basis sorted by
    /// entity, one pin per entity, and no pin on itself. The registry assigns
    /// versions: `entity.version` is honoured only when the id is first
    /// recorded (to import an already versioned item; a re-created id takes
    /// the larger of it and the next major after its tombstone) and ignored
    /// on updates. A change that bumps nothing but still changes what is
    /// stored (the kind, the embedding, the basis) keeps the version and
    /// updates its snapshot.
    ///
    /// A put that matches what is stored is answered without running the
    /// engine ([`PutOutcome::unchanged`]); so is one whose
    /// [`PutOptions::source_seq`] is not above the stored sequence
    /// ([`PutOutcome::skipped_stale`]). A dry run and a put with activity
    /// events are always assessed, unless stale.
    pub fn put(&self, entity: Entity, opts: PutOptions) -> Result<PutOutcome> {
        let outcome = self.put_inner(entity, &opts, true)?;
        self.compact_if_due();
        Ok(outcome)
    }

    /// A put; with `sync` false its journal write is left for the caller to
    /// sync (see [`Registry::sync_journal`]).
    fn put_inner(&self, entity: Entity, opts: &PutOptions, sync: bool) -> Result<PutOutcome> {
        if entity.id.trim().is_empty() {
            return Err(Error::invalid("an entity has an empty id"));
        }
        let now = opts.now.unwrap_or_else(Utc::now);
        if opts.dry_run {
            let gathered = self.gather_put(&self.read(), entity, opts, now)?;
            return Ok(self.evaluate_put(gathered)?.into_outcome(false));
        }
        let assessed = self.assess_put(entity.clone(), opts, now)?;
        self.commit_put(entity, opts, now, assessed, sync)
    }

    /// The expensive half of a put: its inputs are gathered under the shared
    /// lock, the assessment runs without any lock (so a long assessment never
    /// holds up other readers, or a writer they would queue behind), and the
    /// entity's revision it was made against is returned for the commit.
    fn assess_put(
        &self,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
    ) -> Result<(Prepared, Option<u64>)> {
        let (gathered, revision) = {
            let state = self.read();
            let revision = state.revision(&entity.id);
            (self.gather_put(&state, entity, opts, now)?, revision)
        };
        Ok((self.evaluate_put(gathered)?, revision))
    }

    /// The write half of a put, under the exclusive lock. If the entity
    /// changed since it was assessed, it is assessed again (under this lock,
    /// so this time nothing can move).
    fn commit_put(
        &self,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
        (assessed, revision): (Prepared, Option<u64>),
        sync: bool,
    ) -> Result<PutOutcome> {
        let mut state = self.write();
        // The stored sequence can move without the revision moving (a
        // duplicate put with a newer sequence), so it is checked again.
        let p = if state.revision(&entity.id) == revision
            && state.is_stale(&entity.id, opts.source_seq) == assessed.skipped_stale
        {
            assessed
        } else {
            self.prepare_put(&state, entity, opts, now)?
        };
        if p.held {
            // A judgment is asked for first: nothing is stored, and no
            // sequence is remembered, so the second put is not stale.
            return Ok(p.into_outcome(false));
        }
        if p.noop {
            // Nothing to store, but a newer sequence is remembered, so an
            // older change that arrives late is still recognised.
            if !p.skipped_stale
                && let Some(op) = state.seq_op(&p.entity.id, opts.source_seq)
            {
                self.persist(std::slice::from_ref(&op), sync)?;
                state.apply(op);
            }
            return Ok(p.into_outcome(false));
        }
        let mut ops = vec![JournalOp::Put {
            entity: p.entity.clone(),
            record: p.record.clone(),
            source_seq: opts.source_seq,
        }];
        if p.created && opts.auto_pin {
            ops.extend(state.backfill_pins(&p.entity));
        }
        self.persist(&ops, sync)?;
        for op in ops {
            state.apply(op);
        }
        // The text was analysed for the assessment; the record keeps that.
        if let Some(cell) = &p.stats
            && let Some(rec) = state.records.get_mut(&p.entity.id)
        {
            rec.stats = Arc::clone(cell);
        }
        Ok(p.into_outcome(true))
    }

    /// A whole put under one lock (the commit's fallback when the entity
    /// changed after it was assessed).
    fn prepare_put(
        &self,
        state: &State,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
    ) -> Result<Prepared> {
        self.evaluate_put(self.gather_put(state, entity, opts, now)?)
    }

    /// A put that is answered without the engine: a stale one, or one that
    /// matches what is stored.
    fn settled(&self, entity: Entity, stale: bool, now: DateTime<Utc>) -> Prepared {
        let summary = if stale {
            "Skipped: a newer change of this entity is already stored."
        } else {
            "Nothing changed: the entity matches the stored version."
        };
        Prepared {
            entity,
            created: false,
            bump: VersionBump::None,
            assessment: self.not_assessed(summary, now),
            record: None,
            noop: true,
            held: false,
            unchanged: !stale,
            skipped_stale: stale,
            stats: None,
        }
    }

    /// The part of a put that reads the registry: the entity as it will be
    /// stored (basis, pins, version to assess against) and the request for
    /// the engine (its neighbourhood and history). A put that is stale, or
    /// that stores nothing new, is settled here and never reaches the engine.
    fn gather_put(
        &self,
        state: &State,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
    ) -> Result<Gathered> {
        let existing = state.records.get(&entity.id);
        let live = existing.filter(|r| !r.deleted);
        if state.is_stale(&entity.id, opts.source_seq) {
            let shown = match live {
                Some(rec) => rec.current.clone(),
                None => {
                    let mut given = entity;
                    normalize(&mut given);
                    given
                }
            };
            return Ok(Gathered::Settled(self.settled(shown, true, now)));
        }
        check_expected(&entity.id, live, opts.expected_version)?;

        let Staged {
            after,
            created,
            previous_version,
        } = stage(state, entity, opts.auto_pin, now)?;
        // The same content check as after an assessment, made before it: a
        // duplicate costs a comparison. Activity is always assessed, and a
        // dry run asks for an assessment.
        if !opts.dry_run
            && opts.events.is_empty()
            && let Some(rec) = live
            && same_content(&rec.current, &after)
        {
            return Ok(Gathered::Settled(self.settled(
                rec.current.clone(),
                false,
                now,
            )));
        }

        let mut seeds: BTreeSet<&str> = BTreeSet::from([after.id.as_str()]);
        seeds.extend(after.parent_id.as_deref());
        seeds.extend(live.and_then(|r| r.current.parent_id.as_deref()));
        seeds.extend(after.relations.iter().map(|r| r.target.as_str()));
        let seeds: Vec<&str> = seeds.into_iter().collect();
        let (context, context_cells) = state.context(&seeds, &after.id, self.depth());
        let cells = Cells {
            context: context_cells,
            before: live.map(|r| Arc::clone(&r.stats)),
            // The stored cell if the text stays, else a new one; the commit
            // hands it to the record, filled by this put's assessment.
            after: Some(match live {
                Some(r) if same_text(&r.current, &after) => Arc::clone(&r.stats),
                _ => Arc::default(),
            }),
        };
        let change = match live {
            Some(rec) => ChangeSet::update(rec.current.clone(), after.clone()),
            None => ChangeSet::create(after.clone()),
        }
        .with_events(opts.events.clone())
        .with_judgments(opts.judgments.clone());
        let request = AssessmentRequest {
            changes: vec![change],
            context,
            history: state.history_points(&after.id),
            now: Some(now),
            options: opts.assess.clone(),
        };
        Ok(Gathered::Assess(PutInput {
            after,
            created,
            previous_version,
            current: live.map(|r| r.current.clone()),
            request,
            cells,
            now,
            hold_if_flagged: opts.hold_if_flagged,
        }))
    }

    /// The part of a put that needs no lock: the assessment, the version it
    /// leads to, and whether the put changes anything at all.
    fn evaluate_put(&self, gathered: Gathered) -> Result<Prepared> {
        let input = match gathered {
            Gathered::Settled(p) => return Ok(p),
            Gathered::Assess(input) => input,
        };
        let PutInput {
            mut after,
            created,
            previous_version,
            current,
            request,
            cells,
            now,
            hold_if_flagged,
        } = input;
        let assessment = self.assess(&request, &cells)?;
        let ca = first_change(&assessment)?;
        let bump = if created {
            VersionBump::Initial
        } else {
            ca.bump
        };
        let next_version = match (created, after.version, previous_version) {
            (true, Some(v), _) => v,
            (false, _, Some(p)) => p.checked_bump(bump).ok_or_else(|| {
                Error::invalid(format!(
                    "`{}` is at version {p}, which a {} bump would overflow",
                    after.id,
                    bump.as_str()
                ))
            })?,
            _ => SemVer::INITIAL,
        };
        after.version = Some(next_version);
        let changed_fields: Vec<String> = ca
            .fields
            .iter()
            .filter(|f| f.field != crate::model::field::ACTIVITY)
            .map(|f| f.field.clone())
            .collect();
        let record = (created || bump != VersionBump::None).then_some(VersionRecord {
            version: next_version,
            bump,
            at: now,
            change_type: if created {
                ChangeType::Created
            } else {
                ChangeType::Updated
            },
            raw_score: ca.raw_score,
            level: assessment.level,
            changed_fields,
            // Filled from the entity when applied; not repeated in the journal.
            snapshot: None,
            judgments: applied_judgments(&request, ca),
        });
        // A stale judgment is asked for again, so it holds the put too.
        let held = hold_if_flagged && !assessment.judgment_requests.is_empty();
        let noop = !created
            && bump == VersionBump::None
            && current.as_ref().is_some_and(|c| same_content(c, &after));
        if noop && let Some(c) = current {
            after = c;
        }
        Ok(Prepared {
            entity: after,
            created,
            bump,
            assessment,
            record,
            noop,
            held,
            unchanged: false,
            skipped_stale: false,
            stats: cells.after,
        })
    }

    /// Deletes an entity (a tombstone with a final major version is kept in
    /// its history) and returns the assessment of the deletion.
    pub fn delete(
        &self,
        id: &str,
        dry_run: bool,
        expected_version: Option<SemVer>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment> {
        self.delete_with(
            id,
            DeleteOptions {
                dry_run,
                expected_version,
                now,
                source_seq: None,
            },
        )
        .map(|o| o.assessment)
    }

    /// [`Registry::delete`] with options: the same deletion, and with a
    /// [`DeleteOptions::source_seq`] that is not above the stored sequence,
    /// a skip that assesses and deletes nothing.
    ///
    /// Deleting an id that is not stored (never stored, or already deleted)
    /// fails with [`ErrorKind::NotFound`]. A `source_seq` above the stored
    /// one is still remembered for the id (not on a dry run), so an older
    /// put that arrives later is skipped as stale. That is a write: it goes
    /// to the journal, and the same call sent again is answered as a skip
    /// ([`DeleteOutcome::skipped_stale`]), not with `NotFound`.
    pub fn delete_with(&self, id: &str, opts: DeleteOptions) -> Result<DeleteOutcome> {
        let outcome = self.delete_inner(id, &opts, true);
        // Also after an error: a "not found" may have stored a sequence.
        self.compact_if_due();
        outcome
    }

    fn delete_inner(&self, id: &str, opts: &DeleteOptions, sync: bool) -> Result<DeleteOutcome> {
        let now = opts.now.unwrap_or_else(Utc::now);
        let expected_version = opts.expected_version;
        let seq = opts.source_seq;
        let stale = |reg: &Self| DeleteOutcome {
            deleted: false,
            assessment: reg.not_assessed(
                "Skipped: a newer change of this entity is already stored.",
                now,
            ),
            skipped_stale: true,
        };
        // Gather under the shared lock and assess without a lock; commit
        // under the exclusive one, or assess again under it if the entity
        // changed meanwhile.
        let gathered = {
            let state = self.read();
            if state.is_stale(id, seq) {
                return Ok(stale(self));
            }
            match state.live(id) {
                Some(_) => Some((
                    self.gather_delete(&state, id, expected_version, now)?,
                    state.revision(id),
                )),
                // Nothing to delete. A what-if, and a deletion without a
                // sequence, end here; a sequence is remembered below.
                None if opts.dry_run || seq.is_none() => return Err(missing(id)),
                None => None,
            }
        };
        let assessed = match gathered {
            Some(((before, request, cells), revision)) => Some((
                self.evaluate_delete(id, before, &request, &cells, now, seq)?,
                revision,
            )),
            None => None,
        };
        if opts.dry_run {
            let (assessed, _) = assessed.ok_or_else(|| missing(id))?;
            return Ok(DeleteOutcome {
                deleted: false,
                assessment: assessed.0,
                skipped_stale: false,
            });
        }
        let mut state = self.write();
        if state.is_stale(id, seq) {
            return Ok(stale(self));
        }
        if state.live(id).is_none() {
            // The source deleted an item that is not stored here (the
            // deletion came before the item, or twice). Its sequence is
            // kept, so that an older put that arrives late is skipped.
            if let Some(op) = state.seq_op(id, seq) {
                self.persist(std::slice::from_ref(&op), sync)?;
                state.apply(op);
            }
            return Err(missing(id));
        }
        let (assessment, op) = match assessed {
            Some((assessed, revision)) if state.revision(id) == revision => assessed,
            _ => self.prepare_delete(&state, id, expected_version, now, seq)?,
        };
        self.persist(std::slice::from_ref(&op), sync)?;
        state.apply(op);
        Ok(DeleteOutcome {
            deleted: true,
            assessment,
            skipped_stale: false,
        })
    }

    /// Applies a batch from the source of truth, in input order. Each item
    /// is handled by what the registry holds for its id:
    ///
    /// - not stored (or deleted): stored at its first version **without an
    ///   assessment**, with the same version, pins and back-filled pins a
    ///   put would give it. Its [`VersionRecord`] has score 0, level `none`
    ///   and no changed fields.
    /// - stored with the same content: nothing happens.
    /// - stored with other content: assessed and versioned like
    ///   [`Registry::put`].
    /// - `deleted`: deleted like [`Registry::delete`]; nothing happens if it
    ///   is not stored.
    /// - a [`source_seq`](ImportItem::source_seq) that is not above the
    ///   stored one: skipped.
    ///
    /// An item that fails is listed in [`ImportSummary::failed`] and the
    /// rest still runs. The journal is synced once, at the end (when the
    /// registry syncs at all). A journal that cannot be written or synced
    /// stops the import with [`ErrorKind::Storage`]: items before the failed
    /// write stay applied, and importing the same batch again is safe.
    ///
    /// New entities cost time in proportion to their own links, so loading
    /// a whole graph is linear in its size. Parents before children is the
    /// cheapest order (children then pin the parent as they arrive).
    ///
    /// `items` may be lazy. It is read in runs of up to 1024 items with no
    /// registry lock held, so it may read a file or this registry. The lock
    /// is free between two runs, so other readers and writers can get in;
    /// the lock is not fair, so under load they may wait for more than one
    /// run. The import ends at the first `None` of the iterator.
    pub fn import(&self, items: impl IntoIterator<Item = ImportItem>) -> Result<ImportSummary> {
        self.import_with(items, &ImportOptions::default())
    }

    /// [`Registry::import`] with options.
    pub fn import_with(
        &self,
        items: impl IntoIterator<Item = ImportItem>,
        opts: &ImportOptions,
    ) -> Result<ImportSummary> {
        let now = opts.now.unwrap_or_else(Utc::now);
        let mut summary = ImportSummary::default();
        let outcome = self.import_items(items, opts, now, &mut summary);
        // Whatever was written is synced, also when the import stops early.
        let synced = self.sync_journal();
        outcome.and(synced)?;
        self.compact_if_due();
        Ok(summary)
    }

    fn import_items(
        &self,
        items: impl IntoIterator<Item = ImportItem>,
        opts: &ImportOptions,
        now: DateTime<Utc>,
        summary: &mut ImportSummary,
    ) -> Result<()> {
        /// Most items handled under one hold of the exclusive lock.
        const LOCK_RUN: usize = 1024;
        // Fused: the import ends at the first `None`, like a `for` loop,
        // whatever the iterator would return after it.
        let mut items = items.into_iter().fuse();
        let mut run: Vec<ImportItem> = Vec::new();
        loop {
            // The caller's iterator runs with no lock held: it may be slow
            // (a file, a channel) and it may read this registry.
            run.clear();
            run.extend(items.by_ref().take(LOCK_RUN));
            if run.is_empty() {
                return Ok(());
            }
            self.import_run(&mut run, opts, now, summary)?;
            // The lock is free between two runs. One yield gives a waiting
            // reader or writer the chance to take it; std's lock is not
            // fair, so this is a chance and not a hand-over.
            std::thread::yield_now();
        }
    }

    /// Applies one run of an import under one hold of the exclusive lock
    /// (given up around each assessment).
    fn import_run(
        &self,
        run: &mut Vec<ImportItem>,
        opts: &ImportOptions,
        now: DateTime<Utc>,
        summary: &mut ImportSummary,
    ) -> Result<()> {
        let mut guard = None;
        for item in run.drain(..) {
            let id = item.entity.id.clone();
            let state = guard.get_or_insert_with(|| self.write());
            let step = self.import_stored(state, item, opts, now).and_then(|s| {
                let item = match s {
                    Step::Done(done) => return Ok(done),
                    Step::Assess(item) => item,
                };
                // A change to assess: the lock is released, as for a put.
                guard = None;
                self.import_assessed(*item, opts, now)
            });
            match step {
                Ok(Done::Created) => summary.created += 1,
                Ok(Done::Unchanged) => summary.unchanged += 1,
                Ok(Done::Assessed) => summary.assessed += 1,
                Ok(Done::SkippedStale) => summary.skipped_stale += 1,
                Ok(Done::Deleted) => summary.deleted += 1,
                Err(e) if e.kind() == ErrorKind::Storage => return Err(e),
                Err(e) => summary.failed.push(ImportFailure {
                    id,
                    error: e.to_string(),
                }),
            }
        }
        Ok(())
    }

    /// The part of an import that needs no assessment, under the exclusive
    /// lock: stale and unchanged items, and new entities.
    fn import_stored(
        &self,
        state: &mut State,
        item: ImportItem,
        opts: &ImportOptions,
        now: DateTime<Utc>,
    ) -> Result<Step> {
        let id = item.entity.id.as_str();
        if id.trim().is_empty() {
            return Err(Error::invalid("an entity has an empty id"));
        }
        if state.is_stale(id, item.source_seq) {
            return Ok(Step::Done(Done::SkippedStale));
        }
        let live = state.live(id);
        let same = match live {
            Some(_) if item.deleted => false,
            Some(current) => {
                let staged = stage(state, item.entity.clone(), opts.auto_pin, now)?;
                same_content(current, &staged.after)
            }
            // The deletion of something that is not stored changes nothing.
            None => item.deleted,
        };
        if same {
            if let Some(op) = state.seq_op(id, item.source_seq) {
                self.persist(std::slice::from_ref(&op), false)?;
                state.apply(op);
            }
            return Ok(Step::Done(Done::Unchanged));
        }
        if live.is_some() {
            return Ok(Step::Assess(Box::new(item)));
        }
        let Staged { after, .. } = stage(state, item.entity, opts.auto_pin, now)?;
        // No engine run checks this entity, so check it here: what a put
        // would reject must not get into the registry through an import.
        crate::engine::validate_entity(&after)?;
        let version = after.version.unwrap_or(SemVer::INITIAL);
        let mut ops = vec![JournalOp::Put {
            record: Some(VersionRecord {
                version,
                bump: VersionBump::Initial,
                at: now,
                change_type: ChangeType::Created,
                raw_score: 0.0,
                level: ImpactLevel::None,
                changed_fields: Vec::new(),
                snapshot: None,
                judgments: Vec::new(),
            }),
            entity: after,
            source_seq: item.source_seq,
        }];
        if opts.auto_pin
            && let Some(JournalOp::Put { entity, .. }) = ops.first()
        {
            let backfilled = state.backfill_pins(entity);
            ops.extend(backfilled);
        }
        self.persist(&ops, false)?;
        for op in ops {
            state.apply(op);
        }
        Ok(Step::Done(Done::Created))
    }

    /// The part of an import that assesses: a changed entity or a deletion,
    /// through the same path as [`Registry::put`] and [`Registry::delete`],
    /// with the journal sync left to the end of the import.
    fn import_assessed(
        &self,
        item: ImportItem,
        opts: &ImportOptions,
        now: DateTime<Utc>,
    ) -> Result<Done> {
        if item.deleted {
            let delete = DeleteOptions {
                now: Some(now),
                source_seq: item.source_seq,
                ..DeleteOptions::default()
            };
            return match self.delete_inner(&item.entity.id, &delete, false) {
                Ok(o) if o.skipped_stale => Ok(Done::SkippedStale),
                Ok(_) => Ok(Done::Deleted),
                // Deleted by someone else since the lock was released.
                // The delete has kept this item's sequence.
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(Done::Unchanged),
                Err(e) => Err(e),
            };
        }
        let put = PutOptions {
            now: Some(now),
            auto_pin: opts.auto_pin,
            assess: opts.assess.clone(),
            source_seq: item.source_seq,
            ..PutOptions::default()
        };
        let o = self.put_inner(item.entity, &put, false)?;
        Ok(if o.skipped_stale {
            Done::SkippedStale
        } else if o.unchanged {
            Done::Unchanged
        } else if o.created {
            Done::Created
        } else {
            Done::Assessed
        })
    }

    /// A whole deletion under one lock (the commit's fallback).
    fn prepare_delete(
        &self,
        state: &State,
        id: &str,
        expected_version: Option<SemVer>,
        now: DateTime<Utc>,
        seq: Option<u64>,
    ) -> Result<(Assessment, JournalOp)> {
        let (before, request, cells) = self.gather_delete(state, id, expected_version, now)?;
        self.evaluate_delete(id, before, &request, &cells, now, seq)
    }

    /// The part of a deletion that reads the registry: the entity and the
    /// request assessing its removal.
    fn gather_delete(
        &self,
        state: &State,
        id: &str,
        expected_version: Option<SemVer>,
        now: DateTime<Utc>,
    ) -> Result<(Entity, AssessmentRequest, Cells)> {
        let rec = state
            .records
            .get(id)
            .filter(|r| !r.deleted)
            .ok_or_else(|| missing(id))?;
        check_expected(id, Some(rec), expected_version)?;
        let before = rec.current.clone();
        let mut seeds: BTreeSet<&str> = BTreeSet::from([id]);
        seeds.extend(before.parent_id.as_deref());
        let seeds: Vec<&str> = seeds.into_iter().collect();
        let (context, context_cells) = state.context(&seeds, id, self.depth());
        let cells = Cells {
            context: context_cells,
            before: Some(Arc::clone(&rec.stats)),
            after: None,
        };
        let request = AssessmentRequest {
            changes: vec![ChangeSet::delete(before.clone())],
            context,
            history: state.history_points(id),
            now: Some(now),
            options: AssessOptions::default(),
        };
        Ok((before, request, cells))
    }

    /// The part of a deletion that needs no lock: the assessment and the
    /// tombstone's journal entry.
    fn evaluate_delete(
        &self,
        id: &str,
        before: Entity,
        request: &AssessmentRequest,
        cells: &Cells,
        now: DateTime<Utc>,
        seq: Option<u64>,
    ) -> Result<(Assessment, JournalOp)> {
        let assessment = self.assess(request, cells)?;
        let current = before.version.unwrap_or(SemVer::INITIAL);
        let version = current.checked_bump(VersionBump::Major).ok_or_else(|| {
            Error::invalid(format!(
                "`{id}` is at version {current}, which a deletion's major bump would overflow"
            ))
        })?;
        let raw_score = first_change(&assessment)?.raw_score;
        let op = JournalOp::Delete {
            id: id.to_owned(),
            record: VersionRecord {
                version,
                bump: VersionBump::Major,
                at: now,
                change_type: ChangeType::Deleted,
                raw_score,
                level: assessment.level,
                changed_fields: vec![crate::model::field::LIFECYCLE.to_owned()],
                snapshot: None,
                judgments: Vec::new(),
            },
            source_seq: seq,
        };
        Ok((assessment, op))
    }

    /// Assesses activity (comments, …) on a stored entity. Nothing is versioned.
    pub fn record_activity(
        &self,
        id: &str,
        events: Vec<ActivityEvent>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment> {
        if events.is_empty() {
            return Err(Error::invalid("no activity to record"));
        }
        let (request, cells) = {
            let state = self.read();
            let rec = state
                .records
                .get(id)
                .filter(|r| !r.deleted)
                .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
            let current = &rec.current;
            let mut seeds: Vec<&str> = vec![id];
            if let Some(p) = &current.parent_id {
                seeds.push(p);
            }
            let (mut context, mut context_cells) = state.context(&seeds, "", self.depth());
            if !context.iter().any(|e| e.id == id) {
                context.push(current.clone());
                context_cells.push(Arc::clone(&rec.stats));
            }
            let request = AssessmentRequest {
                changes: vec![ChangeSet::activity(id, events)],
                context,
                history: state.history_points(id),
                now,
                options: AssessOptions::default(),
            };
            let cells = Cells {
                context: context_cells,
                ..Cells::default()
            };
            (request, cells)
        };
        // No lock is held here: the guard was dropped with the block above.
        self.assess(&request, &cells)
    }

    /// The live entity, or the snapshot at `version`.
    pub fn get(&self, id: &str, version: Option<SemVer>) -> Result<Entity> {
        let state = self.read();
        let rec = state
            .records
            .get(id)
            .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
        match version {
            None if rec.deleted => Err(Error::not_found(format!("entity `{id}` was deleted"))),
            None => Ok(rec.current.clone()),
            Some(v) => {
                let h = rec
                    .history
                    .iter()
                    .rev()
                    .find(|h| h.version == v)
                    .ok_or_else(|| Error::not_found(format!("entity `{id}` has no version {v}")))?;
                h.snapshot.clone().ok_or_else(|| {
                    Error::not_found(if h.change_type == ChangeType::Deleted {
                        format!("version {v} of `{id}` is its deletion")
                    } else {
                        format!("the snapshot of version {v} of `{id}` is no longer kept")
                    })
                })
            }
        }
    }

    /// The version history, oldest first; with `limit > 0`, only the newest `limit` records.
    pub fn history(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>> {
        let state = self.read();
        let rec = state
            .records
            .get(id)
            .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
        let h = &rec.history;
        let start = if limit > 0 {
            h.len().saturating_sub(limit)
        } else {
            0
        };
        Ok(h[start..].to_vec())
    }

    /// Assesses the difference between two stored versions of an entity,
    /// against its current neighbourhood.
    ///
    /// The judgments that were applied when `to` was stored are used again.
    /// They fit only the change from the version just before it, so the
    /// comparison of two consecutive versions agrees with the history.
    pub fn compare(
        &self,
        id: &str,
        from: SemVer,
        to: SemVer,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment> {
        self.compare_with(id, from, to, now, &AssessOptions::default(), &[])
    }

    /// [`Registry::compare`] with assessment options and more judgments.
    /// A given judgment of a field goes before the stored one.
    pub fn compare_with(
        &self,
        id: &str,
        from: SemVer,
        to: SemVer,
        now: Option<DateTime<Utc>>,
        options: &AssessOptions,
        judgments: &[TextJudgment],
    ) -> Result<Assessment> {
        let before = self.get(id, Some(from))?;
        let after = self.get(id, Some(to))?;
        let mut seeds: Vec<&str> = vec![id];
        if let Some(p) = &after.parent_id {
            seeds.push(p);
        }
        // The guard is dropped at the end of this statement: no lock is held
        // while the comparison is assessed.
        let (context, context_cells) = self.read().context(&seeds, id, self.depth());
        // The two versions are history, not the stored entity: only the
        // neighbourhood has kept analysis.
        let cells = Cells {
            context: context_cells,
            ..Cells::default()
        };
        let mut all = judgments.to_vec();
        let stored = self.read().records.get(id).and_then(|rec| {
            rec.history
                .iter()
                .rev()
                .find(|r| r.version == to)
                .map(|r| r.judgments.clone())
        });
        for j in stored.unwrap_or_default() {
            if !all.iter().any(|g| g.field == j.field) {
                all.push(j);
            }
        }
        let request = AssessmentRequest {
            changes: vec![ChangeSet::update(before, after).with_judgments(all)],
            context,
            history: Vec::new(),
            now,
            options: options.clone(),
        };
        self.assess(&request, &cells)
    }

    /// Every pin that is behind a compatible addition, a breaking change or a
    /// deletion — among the descendants of `root` (and `root` itself), or everywhere.
    /// A pin an entity holds on itself is ignored, as the engine does.
    pub fn stale(&self, root: Option<&str>) -> Result<Vec<StaleEntry>> {
        let state = self.read();
        let ids: Vec<String> = match root {
            Some(r) => {
                if state.live(r).is_none() {
                    return Err(Error::not_found(format!("entity `{r}` does not exist")));
                }
                let mut out = vec![r.to_owned()];
                let mut queue = VecDeque::from([r.to_owned()]);
                let mut seen = BTreeSet::from([r.to_owned()]);
                while let Some(id) = queue.pop_front() {
                    if let Some(kids) = state.links.children.get(&id) {
                        for k in kids {
                            if state.live(k).is_some() && seen.insert(k.clone()) {
                                out.push(k.clone());
                                queue.push_back(k.clone());
                            }
                        }
                    }
                }
                out
            }
            None => {
                let mut v: Vec<String> = state
                    .records
                    .iter()
                    .filter(|(_, r)| !r.deleted)
                    .map(|(id, _)| id.clone())
                    .collect();
                v.sort();
                v
            }
        };
        let mut out = Vec::new();
        for id in ids {
            let Some(e) = state.live(&id) else { continue };
            for pin in e.basis.iter().filter(|p| p.entity_id != id) {
                let current = state.live(&pin.entity_id).and_then(|x| x.version);
                let status = match current {
                    None => BasisStatus::Removed,
                    Some(c) => match pin.version.compatibility(c) {
                        Compatibility::Identical => BasisStatus::Current,
                        Compatibility::Patch => BasisStatus::Compatible,
                        Compatibility::Additive => BasisStatus::ReviewSuggested,
                        Compatibility::Breaking => BasisStatus::Incompatible,
                    },
                };
                if status >= BasisStatus::ReviewSuggested {
                    out.push(StaleEntry {
                        entity_id: id.clone(),
                        basis_entity_id: pin.entity_id.clone(),
                        pinned: pin.version,
                        current,
                        status,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            b.status
                .cmp(&a.status)
                .then(a.entity_id.cmp(&b.entity_id))
                .then(a.basis_entity_id.cmp(&b.basis_entity_id))
        });
        Ok(out)
    }

    /// Re-pins an entity to the current versions of what it was planned
    /// against (dropping pins on deleted entities and on itself, adding its
    /// parent and blockers if unpinned). Call after re-planning it. Not a
    /// version change.
    pub fn acknowledge(&self, id: &str, now: Option<DateTime<Utc>>) -> Result<Entity> {
        let now = now.unwrap_or_else(Utc::now);
        let mut state = self.write();
        let current = state
            .live(id)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
        let mut pins: BTreeMap<String, SemVer> = BTreeMap::new();
        for pin in current.basis.iter().filter(|p| p.entity_id != id) {
            if let Some(v) = state.live(&pin.entity_id).and_then(|e| e.version) {
                pins.insert(pin.entity_id.clone(), v);
            }
        }
        for pin in state.auto_pins(&current) {
            pins.entry(pin.entity_id).or_insert(pin.version);
        }
        let mut updated = current.clone();
        updated.basis = pins
            .into_iter()
            .map(|(entity_id, version)| BasisPin { entity_id, version })
            .collect();
        if updated.basis == current.basis {
            return Ok(current);
        }
        updated.updated_at = Some(now);
        let op = JournalOp::Put {
            entity: updated.clone(),
            record: None,
            source_seq: None,
        };
        self.persist(std::slice::from_ref(&op), true)?;
        state.apply(op);
        drop(state);
        self.compact_if_due();
        Ok(updated)
    }
}

/// How an import handled one item.
enum Done {
    Created,
    Unchanged,
    Assessed,
    SkippedStale,
    Deleted,
}

/// Where an import item goes after the part under the lock.
enum Step {
    Done(Done),
    /// A change or a deletion of a stored entity: to assess without the lock.
    Assess(Box<ImportItem>),
}

/// What a put gathered from the registry, ready to assess without a lock.
struct PutInput {
    /// The entity as it will be stored, at the version it is assessed at.
    after: Entity,
    created: bool,
    previous_version: Option<SemVer>,
    /// The live stored entity, if any.
    current: Option<Entity>,
    request: AssessmentRequest,
    cells: Cells,
    now: DateTime<Utc>,
    hold_if_flagged: bool,
}

/// The analysis cells that go with a request the registry built: one per
/// context entity, in the order of the context, and those of the one
/// change's snapshots where they are stored entities.
#[derive(Debug, Default)]
struct Cells {
    context: Vec<Arc<StatsCell>>,
    before: Option<Arc<StatsCell>>,
    after: Option<Arc<StatsCell>>,
}

struct Prepared {
    entity: Entity,
    created: bool,
    bump: VersionBump,
    assessment: Assessment,
    record: Option<VersionRecord>,
    noop: bool,
    /// Held back by [`PutOptions::hold_if_flagged`]: nothing is stored.
    held: bool,
    /// Settled before the engine ran: the entity matches what is stored.
    unchanged: bool,
    /// Settled before the engine ran: the source sequence is stale.
    skipped_stale: bool,
    /// The analysis cell of `entity`'s text, for the record that stores it.
    stats: Option<Arc<StatsCell>>,
}

/// What [`Registry::gather_put`] found.
enum Gathered {
    /// A change to assess.
    Assess(PutInput),
    /// Nothing to assess: stale, or the same as what is stored.
    Settled(Prepared),
}

/// An entity as a put would store it, before any assessment.
struct Staged {
    after: Entity,
    created: bool,
    previous_version: Option<SemVer>,
}

/// Puts `entity` in the form it will be stored in: canonical, with the basis
/// it keeps or gets pinned, at the version it is assessed against (its stored
/// version for an update, its first version for a creation).
fn stage(state: &State, entity: Entity, auto_pin: bool, now: DateTime<Utc>) -> Result<Staged> {
    let existing = state.records.get(&entity.id);
    let live = existing.filter(|r| !r.deleted);
    let mut after = entity;
    normalize(&mut after);
    let explicit_basis = !after.basis.is_empty();
    let created = live.is_none();
    let previous_version = live.and_then(|r| r.current.version);
    match live {
        Some(rec) => {
            if !explicit_basis {
                after.basis = rec.current.basis.clone();
                if auto_pin {
                    if after.parent_id != rec.current.parent_id {
                        // Re-parented: the plan is now against the new
                        // parent, at its current version.
                        let old = rec.current.parent_id.as_deref();
                        let new = after.parent_id.as_deref();
                        after.basis.retain(|p| {
                            let p = Some(p.entity_id.as_str());
                            p != old && p != new
                        });
                    }
                    for pin in state.auto_pins(&after) {
                        if after.pin_for(&pin.entity_id).is_none() {
                            after.basis.push(pin);
                        }
                    }
                }
            }
            after.version = previous_version;
        }
        None => {
            if auto_pin && !explicit_basis {
                after.basis = state.auto_pins(&after);
            }
            // Decided before assessing, so the assessment reports, and
            // checks pins against, the version that is stored.
            after.version = Some(creation_version(&after.id, existing, after.version)?);
        }
    }
    normalize(&mut after);
    if after.updated_at.is_none() {
        after.updated_at = Some(now);
    }
    Ok(Staged {
        after,
        created,
        previous_version,
    })
}

impl Prepared {
    fn into_outcome(self, stored: bool) -> PutOutcome {
        PutOutcome {
            entity: self.entity,
            created: self.created,
            stored,
            bump: self.bump,
            assessment: self.assessment,
            unchanged: self.unchanged,
            skipped_stale: self.skipped_stale,
        }
    }
}

/// The supplied judgments the assessment applied, in field order.
fn applied_judgments(request: &AssessmentRequest, ca: &ChangeAssessment) -> Vec<TextJudgment> {
    let Some(change) = request.changes.first() else {
        return Vec::new();
    };
    ca.fields
        .iter()
        .filter(|f| {
            f.judgment
                .as_ref()
                .is_some_and(|j| j.status == JudgmentStatus::Applied)
        })
        .filter_map(|f| change.judgments.iter().find(|j| j.field == f.field))
        .cloned()
        .collect()
}

/// The assessment of the one change a registry request carries.
fn first_change(assessment: &Assessment) -> Result<&ChangeAssessment> {
    assessment
        .changes
        .first()
        .ok_or_else(|| Error::invalid("the engine returned no assessment for the change"))
}

/// The version an entity gets when it is (re-)created: the caller's, if it
/// supplies one, for a first creation (an import); for a re-created id, the
/// next major after its tombstone, or the caller's if that is higher.
fn creation_version(
    id: &str,
    existing: Option<&Record>,
    supplied: Option<SemVer>,
) -> Result<SemVer> {
    match existing.and_then(|r| r.history.last()) {
        Some(last) => {
            let next = last
                .version
                .checked_bump(VersionBump::Major)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{id}` was deleted at version {}, which a re-creation's major bump would overflow",
                        last.version
                    ))
                })?;
            Ok(supplied.map_or(next, |s| s.max(next)))
        }
        None => Ok(supplied.unwrap_or(SemVer::INITIAL)),
    }
}

fn missing(id: &str) -> Error {
    Error::not_found(format!("entity `{id}` does not exist"))
}

fn check_expected(id: &str, live: Option<&Record>, expected: Option<SemVer>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match live.and_then(|r| r.current.version) {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(Error::conflict(format!(
            "`{id}` is at version {v}, not the expected {expected}"
        ))),
        None => Err(Error::conflict(format!(
            "`{id}` does not exist, so it is not at the expected version {expected}"
        ))),
    }
}

/// Test-only: holds the next assessment of one registry just before the
/// engine runs, so a test can check what else can run meanwhile. The gate
/// belongs to the registry under test, so tests running in parallel on other
/// registries never meet it.
#[cfg(test)]
mod gate {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    /// The longest a test waits for the other side. A test that would
    /// deadlock fails after this long instead of hanging.
    pub(super) const TIMEOUT: Duration = Duration::from_secs(10);

    /// The longest an assessment stays held. Longer than [`TIMEOUT`], so a
    /// test that waits on something the held assessment blocks times out
    /// (and fails) before the assessment lets go by itself.
    const HOLD: Duration = Duration::from_secs(30);

    /// What an armed gate holds: where to report that the assessment was
    /// reached, and where to wait for the go-ahead.
    type Armed = (Sender<()>, Receiver<()>);

    #[derive(Debug, Default)]
    pub(super) struct Gate(Mutex<Option<Armed>>, AtomicUsize);

    /// The test's side of an armed gate. Dropping it disarms the gate and
    /// releases a held assessment, so a failing test leaves nothing blocked.
    #[derive(Debug)]
    pub(super) struct Held<'a> {
        gate: &'a Gate,
        entered: Receiver<()>,
        resume: Sender<()>,
    }

    impl Gate {
        /// How many assessments this registry has run.
        pub(super) fn assessments(&self) -> usize {
            self.1.load(Ordering::Relaxed)
        }

        /// Arms the gate for the next assessment of this registry.
        pub(super) fn arm(&self) -> Held<'_> {
            let (entered_tx, entered) = channel();
            let (resume, resume_rx) = channel();
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((entered_tx, resume_rx));
            Held {
                gate: self,
                entered,
                resume,
            }
        }

        /// Called before every assessment. If the gate is armed, disarms it,
        /// reports in, and waits for the go-ahead (or for the test to give
        /// up: a dropped [`Held`] or the timeout both let it go on).
        pub(super) fn pass(&self) {
            self.1.fetch_add(1, Ordering::Relaxed);
            let taken = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((entered, resume)) = taken {
                // The test may be gone already; then there is nobody to tell.
                entered.send(()).ok();
                resume.recv_timeout(HOLD).ok();
            }
        }
    }

    impl Held<'_> {
        /// Waits until an assessment is held at the gate.
        pub(super) fn wait_entered(&self) {
            self.entered
                .recv_timeout(TIMEOUT)
                .expect("the operation reaches its assessment in time");
        }

        /// Lets the held assessment go on.
        pub(super) fn resume(self) {
            self.resume.send(()).ok();
        }
    }

    impl Drop for Held<'_> {
        fn drop(&mut self) {
            *self.gate.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StatusCategory;
    use chrono::TimeZone;

    /// Runs `op` on a worker thread and holds it just before its assessment.
    /// While it is held, checks that a read and a put of an unrelated entity
    /// (whose commit needs the exclusive lock) finish, then runs `meanwhile`,
    /// lets `op` go on and returns its result.
    ///
    /// If `op` held a lock during its assessment, the unrelated put would
    /// wait for it: the check then fails after a timeout instead of hanging.
    fn hold_mid_assessment<R: Send>(
        reg: &Registry,
        op: impl FnOnce() -> R + Send,
        meanwhile: impl FnOnce(),
    ) -> R {
        std::thread::scope(|s| {
            // Declared in the scope: if a check below panics, the gate is
            // released before the scope joins the worker.
            let held = reg.gate.arm();
            let worker = s.spawn(op);
            held.wait_entered();
            let (done, others) = std::sync::mpsc::channel();
            s.spawn(move || {
                let got = reg.get("ENG-1", None).map(|e| e.id);
                let put = reg
                    .put(
                        Entity::new("ENG-9", "issue").with_title("Invoices"),
                        opts(2),
                    )
                    .map(|o| o.stored);
                done.send((got, put)).ok();
            });
            let (got, put) = others
                .recv_timeout(gate::TIMEOUT)
                .expect("a get and an unrelated put finish while the assessment is held");
            assert_eq!(got.expect("the get succeeds"), "ENG-1");
            assert!(put.expect("the unrelated put succeeds"));
            meanwhile();
            held.resume();
            worker.join().expect("the worker does not panic")
        })
    }

    /// A put assesses without holding the registry's lock: while one is in
    /// the middle of its assessment, reads and other puts go ahead.
    #[test]
    fn reads_and_other_puts_proceed_while_a_put_is_assessing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        let outcome = hold_mid_assessment(
            &reg,
            || {
                reg.put(
                    parent().with_description("- [ ] Card payments only"),
                    opts(2),
                )
            },
            || {},
        )
        .expect("the held put commits");
        assert!(outcome.stored);
        assert_eq!(outcome.bump, VersionBump::Major);
        assert_eq!(reg.len(), 2);
    }

    /// Dry runs assess without a lock as well: a preview never blocks a
    /// commit.
    #[test]
    fn reads_and_puts_proceed_while_a_dry_run_is_assessing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        let preview = hold_mid_assessment(
            &reg,
            || {
                reg.put(
                    parent().with_description("- [ ] Card payments only"),
                    PutOptions {
                        dry_run: true,
                        ..opts(2)
                    },
                )
            },
            || {},
        )
        .expect("the dry-run put assesses");
        assert!(!preview.stored);
    }

    #[test]
    fn reads_and_puts_proceed_while_a_dry_run_delete_is_assessing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        hold_mid_assessment(&reg, || reg.delete("ENG-1", true, None, Some(t(3))), || {})
            .expect("the dry-run delete assesses");
        assert!(reg.get("ENG-1", None).is_ok());
    }

    /// A deletion assesses without a lock too. If the entity changes during
    /// that assessment, the commit gathers and assesses again under the
    /// exclusive lock, so the tombstone follows the newer version.
    #[test]
    fn a_delete_held_mid_assessment_blocks_nobody_and_follows_a_newer_version() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        let assessment = hold_mid_assessment(
            &reg,
            || reg.delete("ENG-1", false, None, Some(t(3))),
            || {
                let theirs = reg
                    .put(parent().with_description("- [ ] Card payments"), opts(2))
                    .expect("a put of the same entity commits meanwhile");
                assert_eq!(theirs.entity.version, Some(SemVer::new(2, 0, 0)));
            },
        )
        .expect("the held delete commits");
        assert_eq!(
            assessment.changes[0].previous_version,
            Some(SemVer::new(2, 0, 0)),
            "assessed again against the newer version"
        );
        assert!(reg.get("ENG-1", None).is_err());
        let history = reg.history("ENG-1", 0).expect("history");
        let versions: Vec<SemVer> = history.iter().map(|h| h.version).collect();
        let tombstone = history.last().expect("a tombstone");
        assert_eq!(tombstone.change_type, ChangeType::Deleted);
        assert!(tombstone.version > SemVer::new(2, 0, 0), "{versions:?}");
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    }

    /// Activity is assessed without a lock.
    #[test]
    fn reads_and_puts_proceed_while_activity_is_assessing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        let assessment = hold_mid_assessment(
            &reg,
            || {
                reg.record_activity(
                    "ENG-1",
                    vec![ActivityEvent::comment(
                        "ENG-1",
                        "blocked by the PSP sandbox",
                    )],
                    Some(t(2)),
                )
            },
            || {},
        )
        .expect("the held activity is assessed");
        assert_eq!(assessment.changes.len(), 1);
    }

    /// A version comparison is assessed without a lock.
    #[test]
    fn reads_and_puts_proceed_while_a_comparison_is_assessing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        reg.put(parent().with_description("- [ ] Card payments"), opts(2))
            .expect("the breaking change stores");
        let assessment = hold_mid_assessment(
            &reg,
            || {
                reg.compare(
                    "ENG-1",
                    SemVer::new(1, 0, 0),
                    SemVer::new(2, 0, 0),
                    Some(t(3)),
                )
            },
            || {},
        )
        .expect("the held comparison is assessed");
        assert_eq!(assessment.changes[0].bump, VersionBump::Major);
    }

    fn t(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, 9, 0, 0)
            .single()
            .expect("valid")
    }

    fn opts(day: u32) -> PutOptions {
        PutOptions {
            now: Some(t(day)),
            ..PutOptions::default()
        }
    }

    fn parent() -> Entity {
        Entity::new("ENG-1", "issue")
            .with_title("Checkout")
            .with_description("- [ ] Card payments\n- [ ] PayPal")
            .with_status(StatusCategory::Started, "In Progress")
    }

    fn kid() -> Entity {
        Entity::new("ENG-2", "sub_issue")
            .with_parent("ENG-1")
            .with_title("PayPal button")
            .with_status(StatusCategory::Unstarted, "Todo")
    }

    #[test]
    fn versions_history_pins_and_staleness() {
        let reg = Registry::new(Engine::default());
        let p = reg.put(parent(), opts(1)).expect("creates");
        assert!(p.created && p.stored);
        assert_eq!(p.entity.version, Some(SemVer::INITIAL));
        let k = reg.put(kid(), opts(1)).expect("creates");
        assert_eq!(
            k.entity.basis,
            vec![BasisPin {
                entity_id: "ENG-1".into(),
                version: SemVer::INITIAL
            }]
        );

        // A typo: patch.
        let typo = parent().with_title("Checkuot");
        let out = reg.put(typo, opts(2)).expect("updates");
        assert_eq!(out.bump, VersionBump::Patch);
        assert_eq!(out.entity.version, Some(SemVer::new(1, 0, 1)));
        assert!(reg.stale(None).expect("lists").is_empty());

        // Drop PayPal: major; the child's plan is now incompatible.
        let cut = parent()
            .with_title("Checkuot")
            .with_description("- [ ] Card payments");
        let out = reg.put(cut, opts(3)).expect("updates");
        assert_eq!(out.bump, VersionBump::Major);
        assert_eq!(out.entity.version, Some(SemVer::new(2, 0, 0)));
        let stale = reg.stale(Some("ENG-1")).expect("lists");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].entity_id, "ENG-2");
        assert_eq!(stale[0].status, BasisStatus::Incompatible);
        let child = out
            .assessment
            .impacts
            .iter()
            .find(|i| i.entity_id == "ENG-2")
            .expect("child impacted");
        assert_eq!(child.basis_status, BasisStatus::Incompatible);

        // Re-planned: acknowledge re-pins.
        let acked = reg.acknowledge("ENG-2", Some(t(4))).expect("acks");
        assert_eq!(acked.basis[0].version, SemVer::new(2, 0, 0));
        assert!(reg.stale(None).expect("lists").is_empty());

        let hist = reg.history("ENG-1", 0).expect("history");
        assert_eq!(
            hist.iter().map(|h| h.version).collect::<Vec<_>>(),
            vec![
                SemVer::new(1, 0, 0),
                SemVer::new(1, 0, 1),
                SemVer::new(2, 0, 0)
            ]
        );
        assert_eq!(
            reg.get("ENG-1", Some(SemVer::new(1, 0, 0)))
                .expect("old")
                .title(),
            "Checkout"
        );
        let cmp = reg
            .compare(
                "ENG-1",
                SemVer::new(1, 0, 0),
                SemVer::new(2, 0, 0),
                Some(t(5)),
            )
            .expect("compares");
        assert_eq!(cmp.changes[0].bump, VersionBump::Major);
    }

    #[test]
    fn conflicts_dry_runs_and_noops() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates");
        let err = reg
            .put(
                parent().with_title("X"),
                PutOptions {
                    expected_version: Some(SemVer::new(9, 0, 0)),
                    ..opts(2)
                },
            )
            .expect_err("wrong version");
        assert_eq!(err.kind(), ErrorKind::Conflict);
        let dry = reg
            .put(
                parent().with_title("Totally different"),
                PutOptions {
                    dry_run: true,
                    ..opts(2)
                },
            )
            .expect("assesses");
        assert!(!dry.stored);
        assert_eq!(reg.get("ENG-1", None).expect("live").title(), "Checkout");
        let same = reg.put(parent(), opts(3)).expect("no-op");
        assert!(!same.stored);
        assert_eq!(same.bump, VersionBump::None);
        assert_eq!(reg.history("ENG-1", 0).expect("history").len(), 1);
    }

    #[test]
    fn delete_and_activity() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates");
        reg.put(kid(), opts(1)).expect("creates");
        let a = reg
            .record_activity(
                "ENG-2",
                vec![ActivityEvent::comment(
                    "ENG-2",
                    "blocked by the PSP sandbox",
                )],
                Some(t(2)),
            )
            .expect("assesses");
        assert!(!a.changes[0].signals.is_empty());
        let del = reg
            .delete("ENG-1", false, None, Some(t(3)))
            .expect("deletes");
        assert!(
            del.violations
                .iter()
                .any(|v| v.kind == crate::assessment::ViolationKind::OrphanedWork)
        );
        assert!(reg.get("ENG-1", None).is_err());
        assert_eq!(
            reg.history("ENG-1", 1).expect("history")[0].change_type,
            ChangeType::Deleted
        );
        let stale = reg.stale(None).expect("lists");
        assert_eq!(stale[0].status, BasisStatus::Removed);
        assert_eq!(
            reg.delete("ENG-1", false, None, None)
                .expect_err("gone")
                .kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn journal_replays() {
        let dir =
            std::env::temp_dir().join(format!("assessment-registry-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), opts(1)).expect("creates");
            reg.put(kid(), opts(1)).expect("creates");
            reg.put(parent().with_description("- [ ] Card payments"), opts(2))
                .expect("updates");
        }
        // A torn final line is tolerated.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("appends");
            f.write_all(b"{\"op\":\"put\",\"enti").expect("writes");
        }
        let reg = Registry::open(Engine::default(), &path, false).expect("replays");
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.get("ENG-1", None).expect("live").version,
            Some(SemVer::new(2, 0, 0))
        );
        assert_eq!(reg.stale(None).expect("lists").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_journal(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("assessment-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("journal.jsonl");
        (dir, path)
    }

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new().append(true).open(path).expect("appends");
        f.write_all(bytes).expect("writes");
    }

    fn pin(id: &str, version: SemVer) -> BasisPin {
        BasisPin {
            entity_id: id.into(),
            version,
        }
    }

    /// Findings 1 and 2: every update of an entity with a parent used to fail
    /// with "entity `ENG-1` appears twice in the context".
    #[test]
    fn updates_entities_that_have_a_parent() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates the parent");
        reg.put(kid(), opts(1)).expect("creates the child");

        let started = kid().with_status(StatusCategory::Started, "In Progress");
        let dry = reg
            .put(
                started.clone(),
                PutOptions {
                    dry_run: true,
                    ..opts(2)
                },
            )
            .expect("dry-runs a sub-issue edit");
        assert!(!dry.stored);
        let out = reg
            .put(started.clone(), opts(2))
            .expect("updates the sub-issue");
        assert!(out.stored && !out.created);
        assert_ne!(out.bump, VersionBump::None);
        assert!(out.entity.version > Some(SemVer::INITIAL));
        assert_eq!(out.entity.version, out.assessment.changes[0].next_version);
        let same = reg
            .put(started, opts(3))
            .expect("re-puts the unchanged sub-issue");
        assert!(!same.stored);
        assert_eq!(same.bump, VersionBump::None);

        let grandchild = Entity::new("ENG-3", "sub_issue")
            .with_parent("ENG-2")
            .with_title("Sandbox keys");
        reg.put(grandchild.clone(), opts(3)).expect("creates");
        reg.put(grandchild.with_estimate(2.0), opts(4))
            .expect("updates a grandchild");

        reg.put(
            Entity::new("X", "issue").with_title("PSP contract"),
            opts(4),
        )
        .expect("creates");
        reg.put(
            Entity::new("Y", "issue")
                .with_title("Go live")
                .with_relation(RelationKind::BlockedBy, "X")
                .with_relation(RelationKind::RelatesTo, "X"),
            opts(4),
        )
        .expect("two relations to one target");
        let blocked_by_parent = Entity::new("ENG-4", "sub_issue")
            .with_parent("ENG-1")
            .with_title("Refunds")
            .with_relation(RelationKind::BlockedBy, "ENG-1");
        reg.put(blocked_by_parent.clone(), opts(4))
            .expect("a parent that is also a blocker");
        reg.put(blocked_by_parent.with_estimate(5.0), opts(5))
            .expect("and its update");

        let state = reg.read();
        let (ctx, _) = state.context(&["ENG-2", "ENG-1", "ENG-1", "ENG-2"], "", 3);
        let ids: BTreeSet<&str> = ctx.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), ctx.len(), "no entity twice in a context");
    }

    /// Findings 3 and 11 (a): a torn tail is truncated, so what is appended
    /// after it survives the next open, and so does the journal.
    #[test]
    fn appends_after_a_torn_tail_survive_reopening() {
        let (dir, path) = temp_journal("torn");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), opts(1)).expect("creates");
        }
        append_raw(&path, b"{\"op\":\"put\",\"enti");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("drops the torn line");
            assert_eq!(reg.len(), 1);
            assert!(reg.put(kid(), opts(2)).expect("creates").stored);
        }
        {
            // Before the fix the child was glued onto the torn bytes and lost here.
            let reg = Registry::open(Engine::default(), &path, false).expect("reopens");
            assert_eq!(reg.len(), 2);
            assert!(reg.get("ENG-2", None).is_ok());
            reg.put(parent().with_title("Checkout v2"), opts(3))
                .expect("updates");
            reg.put(parent().with_title("Checkout v3"), opts(4))
                .expect("updates");
        }
        // ... and, two writes later, the journal no longer opened at all.
        let reg = Registry::open(Engine::default(), &path, false).expect("still opens");
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get("ENG-1", None).expect("live").title(), "Checkout v3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finding 11 (c): a last record that lost only its newline is kept, and
    /// the next append does not merge with it.
    #[test]
    fn a_last_record_without_its_newline_is_kept() {
        let (dir, path) = temp_journal("newline");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), opts(1)).expect("creates");
        }
        let bytes = std::fs::read(&path).expect("reads");
        assert_eq!(bytes.last(), Some(&b'\n'));
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("drops the newline");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            assert_eq!(reg.len(), 1);
            reg.put(kid(), opts(2)).expect("creates");
        }
        // Before the fix both records were dropped here as one torn line.
        let reg = Registry::open(Engine::default(), &path, false).expect("reopens");
        assert_eq!(reg.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finding 11 (b): a crash inside a multi-byte character of the last line
    /// is a torn line like any other, not an unreadable journal.
    #[test]
    fn a_tail_torn_inside_a_utf8_character_is_dropped() {
        let (dir, path) = temp_journal("utf8");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), opts(1)).expect("creates");
            reg.put(
                Entity::new("ENG-7", "issue").with_title("Нэвтрэх хуудас"),
                opts(1),
            )
            .expect("creates");
        }
        let bytes = std::fs::read(&path).expect("reads");
        let last_line = bytes[..bytes.len() - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .expect("two lines")
            + 1;
        let lead = last_line
            + bytes[last_line..]
                .iter()
                .position(|b| *b >= 0xC0)
                .expect("a multi-byte character");
        std::fs::write(&path, &bytes[..=lead]).expect("cuts inside the character");
        {
            let reg =
                Registry::open(Engine::default(), &path, false).expect("opens despite the cut");
            assert_eq!(reg.len(), 1);
            reg.put(
                Entity::new("ENG-7", "issue").with_title("Нэвтрэх хуудас"),
                opts(2),
            )
            .expect("creates again");
        }
        let reg = Registry::open(Engine::default(), &path, false).expect("reopens");
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.get("ENG-7", None).expect("live").title(),
            "Нэвтрэх хуудас"
        );

        // Damage anywhere but the tail is still an error.
        let mut bytes = std::fs::read(&path).expect("reads");
        bytes.splice(0..0, b"{\"op\":\"pu\n".iter().copied());
        std::fs::write(&path, &bytes).expect("corrupts the first line");
        let err = Registry::open(Engine::default(), &path, false).expect_err("corrupt");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(err.message().contains("line 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finding 11 (append): a write that fails leaves memory untouched, and a
    /// journal that cannot be restored refuses further writes.
    #[test]
    fn a_failed_write_changes_nothing_and_stops_further_writes() {
        let (dir, path) = temp_journal("failed");
        std::fs::write(&path, b"").expect("creates");
        // A read-only handle: writing fails, and so does truncating it back.
        let file = File::open(&path).expect("opens read-only");
        let reg = Registry::assemble(
            Engine::default(),
            State::default(),
            Some(Journal {
                file,
                path: path.clone(),
                fsync: false,
                len: 0,
                failed: false,
                unsynced: false,
                generation: 0,
                ops: 0,
                auto_floor: 0,
                syncs: 0,
            }),
            None,
        );
        let err = reg.put(parent(), opts(1)).expect_err("cannot write");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(reg.get("ENG-1", None).is_err(), "memory is not updated");
        let err = reg.put(parent(), opts(2)).expect_err("refuses to write");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(err.message().contains("reopen the registry"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Findings 13 and 36: a re-created id is assessed at, and checks pins
    /// against, the version it is stored under.
    #[test]
    fn a_recreated_id_is_assessed_at_its_stored_version() {
        let reg = Registry::new(Engine::default());
        let fresh = reg.put(parent(), opts(1)).expect("creates");
        assert_eq!(
            fresh.entity.version,
            fresh.assessment.changes[0].next_version
        );
        reg.put(
            kid().with_status(StatusCategory::Started, "In Progress"),
            opts(1),
        )
        .expect("creates the child, pinned at 1.0.0");
        reg.delete("ENG-1", false, None, Some(t(2)))
            .expect("deletes");
        let again = reg
            .put(
                parent().with_description("- [ ] Crypto payments only"),
                opts(3),
            )
            .expect("re-creates");
        assert_eq!(again.entity.version, Some(SemVer::new(3, 0, 0)));
        // Before the fix: next_version 1.0.0, and the child was not impacted.
        assert_eq!(
            again.assessment.changes[0].next_version,
            again.entity.version
        );
        let child = again
            .assessment
            .impacts
            .iter()
            .find(|i| i.entity_id == "ENG-2")
            .expect("the child is impacted");
        assert_eq!(child.basis_status, BasisStatus::Incompatible);
        let stale = reg.stale(None).expect("lists");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].status, BasisStatus::Incompatible);
        assert_eq!(stale[0].current, Some(SemVer::new(3, 0, 0)));
    }

    /// Finding 15: an entity put before its parent or blocker is pinned when
    /// that arrives, so stale() does not depend on import order.
    #[test]
    fn pins_are_backfilled_when_what_they_depend_on_arrives_later() {
        let reg = Registry::new(Engine::default());
        let k = reg.put(kid(), opts(1)).expect("the child arrives first");
        assert!(k.entity.basis.is_empty());
        reg.put(parent(), opts(1)).expect("then its parent");
        let child = reg.get("ENG-2", None).expect("child");
        assert_eq!(child.basis, vec![pin("ENG-1", SemVer::INITIAL)]);
        assert_eq!(
            reg.get("ENG-2", child.version).expect("its snapshot"),
            child,
            "the current snapshot follows the re-pin"
        );

        reg.put(
            Entity::new("B-2", "issue")
                .with_title("Launch")
                .with_relation(RelationKind::BlockedBy, "B-1"),
            opts(1),
        )
        .expect("the blocked item first");
        reg.put(
            Entity::new("B-1", "issue").with_title("Security review"),
            opts(1),
        )
        .expect("then its blocker");
        assert_eq!(
            reg.get("B-2", None).expect("blocked").basis,
            vec![pin("B-1", SemVer::INITIAL)]
        );
        reg.put(Entity::new("C-2", "issue").with_title("Docs"), opts(1))
            .expect("the blocked item first");
        reg.put(
            Entity::new("C-1", "issue")
                .with_title("API")
                .with_relation(RelationKind::Blocks, "C-2"),
            opts(1),
        )
        .expect("then what blocks it");
        assert_eq!(
            reg.get("C-2", None).expect("blocked").basis,
            vec![pin("C-1", SemVer::INITIAL)]
        );

        let out = reg
            .put(parent().with_description("- [ ] Card payments"), opts(2))
            .expect("a breaking change");
        assert_eq!(out.bump, VersionBump::Major);
        let stale = reg.stale(Some("ENG-1")).expect("lists");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].entity_id, "ENG-2");
        assert_eq!(stale[0].status, BasisStatus::Incompatible);

        // A child stored without pins gets its missing pin on its next update.
        let receipts = Entity::new("ENG-5", "sub_issue")
            .with_parent("ENG-1")
            .with_title("Receipts");
        let bare = reg
            .put(
                receipts.clone(),
                PutOptions {
                    auto_pin: false,
                    ..opts(3)
                },
            )
            .expect("creates without pins");
        assert!(bare.entity.basis.is_empty());
        let pinned = reg
            .put(receipts.with_estimate(2.0), opts(4))
            .expect("updates");
        assert_eq!(
            pinned.entity.basis,
            vec![pin("ENG-1", SemVer::new(2, 0, 0))]
        );
    }

    /// Finding 34: the assessment runs with no lock held, so the entity can
    /// change meanwhile. A commit that finds it changed gathers and assesses
    /// again under the exclusive lock instead of overwriting the newer version.
    #[test]
    fn a_put_assessed_against_an_older_state_is_assessed_again() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates");

        // No interference: the optimistic assessment is committed as is.
        let typo = parent().with_title("Checkuot");
        let o = opts(2);
        let assessed = reg.assess_put(typo.clone(), &o, t(2)).expect("assesses");
        let expected = assessed.0.assessment.clone();
        let out = reg
            .commit_put(typo, &o, t(2), assessed, true)
            .expect("commits");
        assert_eq!(out.assessment, expected);
        assert_eq!(out.entity.version, Some(SemVer::new(1, 0, 1)));

        // Interference: another writer commits between assessment and commit.
        let mine = parent().with_title("Checkout (cards)");
        let o = opts(3);
        let assessed = reg.assess_put(mine.clone(), &o, t(3)).expect("assesses");
        assert_eq!(
            assessed.0.assessment.changes[0].previous_version,
            Some(SemVer::new(1, 0, 1))
        );
        let theirs = reg
            .put(
                parent()
                    .with_title("Checkuot")
                    .with_description("- [ ] Card payments"),
                opts(3),
            )
            .expect("their breaking change");
        assert_eq!(theirs.entity.version, Some(SemVer::new(2, 0, 0)));
        let out = reg
            .commit_put(mine, &o, t(3), assessed, true)
            .expect("commits");
        assert_eq!(
            out.assessment.changes[0].previous_version,
            Some(SemVer::new(2, 0, 0)),
            "assessed again against the newer version"
        );
        assert!(out.entity.version > Some(SemVer::new(2, 0, 0)));
        assert_eq!(out.entity.version, out.assessment.changes[0].next_version);
        let versions: Vec<SemVer> = reg
            .history("ENG-1", 0)
            .expect("history")
            .iter()
            .map(|h| h.version)
            .collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    }

    #[test]
    fn concurrent_puts_lose_nothing() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates");
        std::thread::scope(|s| {
            for n in 0..4 {
                let reg = &reg;
                s.spawn(move || {
                    for v in 0..4 {
                        reg.put(
                            Entity::new(format!("K-{n}"), "sub_issue")
                                .with_parent("ENG-1")
                                .with_title(format!("Child {n}"))
                                .with_estimate(f64::from(1_u32 << v)),
                            opts(2),
                        )
                        .expect("puts");
                    }
                });
            }
        });
        for n in 0..4 {
            let id = format!("K-{n}");
            assert_eq!(reg.history(&id, 0).expect("history").len(), 4);
            assert_eq!(reg.get(&id, None).expect("live").estimate(), Some(8.0));
        }
    }

    /// Finding 38: a version the caller already tracks is kept on import.
    #[test]
    fn imported_versions_are_kept() {
        let reg = Registry::new(Engine::default());
        let p = reg
            .put(parent().with_version(SemVer::new(3, 2, 0)), opts(1))
            .expect("imports");
        assert_eq!(p.entity.version, Some(SemVer::new(3, 2, 0)));
        assert_eq!(p.assessment.changes[0].next_version, p.entity.version);
        let k = reg
            .put(
                kid()
                    .with_version(SemVer::new(1, 4, 0))
                    .with_basis("ENG-1", SemVer::new(3, 2, 0)),
                opts(1),
            )
            .expect("imports");
        assert_eq!(k.entity.version, Some(SemVer::new(1, 4, 0)));
        // Before the fix: [ENG-2 → ENG-1 @3.2.0, now 1.0.0, Incompatible].
        assert!(reg.stale(None).expect("lists").is_empty());
        let typo = reg
            .put(parent().with_title("Checkuot"), opts(2))
            .expect("updates");
        assert_eq!(typo.entity.version, Some(SemVer::new(3, 2, 1)));
        assert!(typo.assessment.level < ImpactLevel::Medium);
        // Later puts cannot move the version: the registry assigns it.
        let ignored = reg
            .put(
                parent()
                    .with_title("Checkuot")
                    .with_version(SemVer::new(9, 0, 0)),
                opts(3),
            )
            .expect("no-op");
        assert!(!ignored.stored);
        assert_eq!(ignored.entity.version, Some(SemVer::new(3, 2, 1)));
        // A re-created id takes the larger of the caller's version and the next major.
        reg.delete("ENG-1", false, None, Some(t(4)))
            .expect("deletes at 4.0.0");
        let low = reg
            .put(
                parent()
                    .with_title("Checkuot")
                    .with_version(SemVer::INITIAL),
                PutOptions {
                    dry_run: true,
                    ..opts(5)
                },
            )
            .expect("dry-runs");
        assert_eq!(low.entity.version, Some(SemVer::new(5, 0, 0)));
        let high = reg
            .put(
                parent()
                    .with_title("Checkuot")
                    .with_version(SemVer::new(7, 1, 0)),
                opts(5),
            )
            .expect("re-creates");
        assert_eq!(high.entity.version, Some(SemVer::new(7, 1, 0)));
    }

    /// Finding 58: a version that cannot be bumped is refused, not repeated.
    #[test]
    fn a_version_that_cannot_be_bumped_is_refused() {
        let reg = Registry::new(Engine::default());
        let top = SemVer::new(1, 2, u64::MAX);
        reg.put(parent().with_version(top), opts(1))
            .expect("imports");
        let err = reg
            .put(parent().with_title("Checkuot"), opts(2))
            .expect_err("a patch would overflow");
        assert_eq!(err.kind(), ErrorKind::InvalidRequest);
        assert_eq!(
            reg.get("ENG-1", None).expect("unchanged").version,
            Some(top)
        );
        reg.delete("ENG-1", false, None, Some(t(3)))
            .expect("deletes at 2.0.0");
    }

    /// Finding 39: below 1.0.0, versions bump the Cargo way, so a pin is
    /// broken by a breaking change and nothing else.
    #[test]
    fn zero_versions_bump_the_cargo_way() {
        let reg = Registry::new(Engine::default());
        reg.put(parent().with_version(SemVer::new(0, 3, 0)), opts(1))
            .expect("imports");
        let k = reg.put(kid(), opts(1)).expect("creates");
        assert_eq!(k.entity.basis, vec![pin("ENG-1", SemVer::new(0, 3, 0))]);
        let typo = reg
            .put(parent().with_title("Checkuot"), opts(2))
            .expect("updates");
        assert_eq!(typo.bump, VersionBump::Patch);
        assert_eq!(typo.entity.version, Some(SemVer::new(0, 3, 1)));
        assert!(reg.stale(None).expect("lists").is_empty());
        let cut = reg
            .put(
                parent()
                    .with_title("Checkuot")
                    .with_description("- [ ] Card payments"),
                opts(3),
            )
            .expect("updates");
        assert_eq!(cut.bump, VersionBump::Major);
        assert_eq!(cut.entity.version, Some(SemVer::new(0, 4, 0)));
        let stale = reg.stale(None).expect("lists");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].status, BasisStatus::Incompatible);
    }

    /// Finding 40: re-parenting keeps an explicit basis.
    #[test]
    fn an_explicit_basis_survives_reparenting() {
        let reg = Registry::new(Engine::default());
        let payments = Entity::new("ENG-9", "issue")
            .with_title("Payments")
            .with_description("- [ ] Cards\n- [ ] PayPal");
        reg.put(payments.clone(), opts(1)).expect("creates");
        let cut = reg
            .put(payments.with_description("- [ ] Cards"), opts(2))
            .expect("a breaking change");
        assert_eq!(cut.entity.version, Some(SemVer::new(2, 0, 0)));

        let refunds = Entity::new("ENG-5", "issue").with_title("Refund flow");
        reg.put(refunds.clone(), opts(1)).expect("creates");
        let moved = reg
            .put(
                refunds
                    .with_parent("ENG-9")
                    .with_basis("ENG-9", SemVer::INITIAL),
                opts(3),
            )
            .expect("moves");
        // Before the fix: ["ENG-9@2.0.0"], and nothing was stale.
        assert_eq!(moved.entity.basis, vec![pin("ENG-9", SemVer::INITIAL)]);
        let stale = reg.stale(None).expect("lists");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].entity_id, "ENG-5");

        let chargebacks = Entity::new("ENG-6", "issue").with_title("Chargebacks");
        reg.put(chargebacks.clone(), opts(1)).expect("creates");
        let moved = reg
            .put(chargebacks.with_parent("ENG-9"), opts(3))
            .expect("moves");
        assert_eq!(moved.entity.basis, vec![pin("ENG-9", SemVer::new(2, 0, 0))]);
    }

    /// Finding 41: a change that bumps nothing keeps the current version's
    /// snapshot equal to the live entity; a reordering is no change at all.
    #[test]
    fn unversioned_changes_keep_the_current_snapshot_in_step() {
        let reg = Registry::new(Engine::default());
        let e = Entity::new("ENG-7", "sub_issue")
            .with_title("Webhooks")
            .with_relation(RelationKind::RelatesTo, "A")
            .with_relation(RelationKind::RelatesTo, "B");
        reg.put(e.clone(), opts(1)).expect("creates");

        let mut issue = e.clone();
        issue.kind = EntityKind::Issue;
        let out = reg.put(issue.clone(), opts(2)).expect("changes the kind");
        assert!(out.stored);
        assert_eq!(out.bump, VersionBump::None);
        assert_eq!(out.entity.version, Some(SemVer::INITIAL));
        let live = reg.get("ENG-7", None).expect("live");
        assert_eq!(live.kind, EntityKind::Issue);
        assert_eq!(
            reg.get("ENG-7", Some(SemVer::INITIAL)).expect("snapshot"),
            live
        );
        assert_eq!(reg.history("ENG-7", 0).expect("history").len(), 1);

        let mut embedded = issue.clone();
        embedded.embedding = Some(vec![0.1, 0.2, 0.3]);
        let out = reg.put(embedded, opts(3)).expect("adds an embedding");
        assert_eq!(out.bump, VersionBump::None);
        assert_eq!(
            reg.get("ENG-7", Some(SemVer::INITIAL)).expect("snapshot"),
            reg.get("ENG-7", None).expect("live")
        );

        let mut reordered = reg.get("ENG-7", None).expect("live");
        reordered.relations.reverse();
        reordered.relations.push(reordered.relations[0].clone());
        let out = reg.put(reordered, opts(4)).expect("reorders");
        assert!(!out.stored, "relations are a set");
    }

    /// Finding 42: a `Custom` kind with a well-known name is stored as the
    /// variant, so replay rebuilds exactly the live state.
    #[test]
    fn a_custom_kind_with_a_known_name_replays_identically() {
        let (dir, path) = temp_journal("kind");
        let mut e = Entity::new("ENG-9", "issue").with_title("Payments");
        e.kind = EntityKind::Custom("issue".to_owned());
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            let out = reg.put(e.clone(), opts(1)).expect("creates");
            assert!(matches!(out.entity.kind, EntityKind::Issue));
            assert!(!reg.put(e.clone(), opts(2)).expect("re-puts").stored);
        }
        let reg = Registry::open(Engine::default(), &path, false).expect("reopens");
        // Before the fix: stored=true, bump=None after the restart.
        let out = reg.put(e, opts(3)).expect("re-puts after replay");
        assert!(!out.stored);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finding 43: a pin an entity holds on itself is ignored, as the engine does.
    #[test]
    fn a_pin_on_itself_is_ignored() {
        let reg = Registry::new(Engine::default());
        let e = Entity::new("ENG-2", "issue")
            .with_title("PayPal")
            .with_description("- [ ] Button\n- [ ] Webhook")
            .with_basis("ENG-2", SemVer::INITIAL);
        let out = reg.put(e.clone(), opts(1)).expect("creates");
        assert!(out.entity.basis.is_empty());
        let out = reg
            .put(e.with_description("- [ ] Button"), opts(2))
            .expect("a breaking self-edit");
        assert_eq!(out.bump, VersionBump::Major);
        assert!(reg.stale(None).expect("lists").is_empty());

        // Even a self-pin that reached the store some other way.
        reg.write()
            .records
            .get_mut("ENG-2")
            .expect("stored")
            .current
            .basis
            .push(pin("ENG-2", SemVer::INITIAL));
        assert!(reg.stale(None).expect("lists").is_empty());
        let acked = reg.acknowledge("ENG-2", Some(t(3))).expect("acks");
        assert!(acked.basis.is_empty());

        let looped = Entity::new("Z", "issue")
            .with_title("Loop")
            .with_parent("Z");
        reg.put(looped.clone(), opts(1)).expect("creates");
        let out = reg
            .put(looped.with_estimate(1.0), opts(2))
            .expect("updates");
        assert!(out.entity.basis.is_empty(), "no auto-pin on itself");
    }

    /// Findings 46 and 59: the journal holds each entity once, replay
    /// restores snapshots from it, and snapshot retention can be bounded.
    #[test]
    fn the_journal_stores_each_entity_once_and_history_can_be_bounded() {
        let (dir, path) = temp_journal("history");
        let description = "Accept card payments through the PSP. ".repeat(1_000);
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent().with_description(description.clone()), opts(1))
                .expect("creates");
        }
        let journal = std::fs::metadata(&path).expect("exists").len();
        // Before the fix: about twice the description.
        assert!(
            journal < description.len() as u64 * 3 / 2,
            "{journal} bytes for a {}-byte description",
            description.len()
        );
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("replays");
            assert_eq!(
                reg.get("ENG-1", Some(SemVer::INITIAL)).expect("snapshot"),
                reg.get("ENG-1", None).expect("live")
            );
            for (day, estimate) in [(2, 1.0), (3, 2.0), (4, 4.0), (5, 8.0)] {
                reg.put(
                    parent()
                        .with_description(description.clone())
                        .with_estimate(estimate),
                    opts(day),
                )
                .expect("updates");
            }
            assert_eq!(reg.history_limit(), None);
            assert!(
                reg.get("ENG-1", Some(SemVer::INITIAL)).is_ok(),
                "all kept by default"
            );
        }

        // A journal written before the fix (snapshot repeated) still replays.
        let reg = Registry::new(Engine::default());
        let mut old = Vec::new();
        let entity = parent().with_version(SemVer::INITIAL);
        let op = JournalOp::Put {
            entity: entity.clone(),
            record: Some(VersionRecord {
                version: SemVer::INITIAL,
                bump: VersionBump::Initial,
                at: t(1),
                change_type: ChangeType::Created,
                raw_score: 0.5,
                level: ImpactLevel::Low,
                changed_fields: vec![],
                snapshot: Some(entity.clone()),
                judgments: Vec::new(),
            }),
            source_seq: None,
        };
        serde_json::to_writer(&mut old, &op).expect("encodes");
        let old_path = dir.join("old.jsonl");
        std::fs::write(&old_path, &old).expect("writes");
        drop(reg);
        let reg = Registry::open(Engine::default(), &old_path, false).expect("replays");
        assert_eq!(
            reg.get("ENG-1", Some(SemVer::INITIAL)).expect("snapshot"),
            entity
        );

        // Bounded while replaying.
        let reg = Registry::open_with_history_limit(Engine::default(), &path, false, 2)
            .expect("replays with a limit");
        let history = reg.history("ENG-1", 0).expect("history");
        assert_eq!(history.len(), 5, "every version keeps its record");
        assert_eq!(
            history.iter().filter(|h| h.snapshot.is_some()).count(),
            2,
            "only the newest two keep their snapshot"
        );
        let err = reg
            .get("ENG-1", Some(SemVer::INITIAL))
            .expect_err("trimmed");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.message().contains("no longer kept"));
        assert!(reg.get("ENG-1", history[4].version.into()).is_ok());
        assert!(reg.get("ENG-1", history[3].version.into()).is_ok());

        // Bounded after the fact, and for later versions.
        let reg = Registry::open(Engine::default(), &path, false)
            .expect("replays")
            .with_history_limit(0);
        assert_eq!(reg.history_limit(), Some(1), "at least the current version");
        reg.put(
            parent().with_description(description).with_estimate(16.0),
            opts(6),
        )
        .expect("updates");
        let history = reg.history("ENG-1", 0).expect("history");
        assert_eq!(history.len(), 6);
        assert_eq!(history.iter().filter(|h| h.snapshot.is_some()).count(), 1);
        assert_eq!(
            reg.get("ENG-1", history[5].version.into())
                .expect("current"),
            reg.get("ENG-1", None).expect("live")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seq(day: u32, seq: u64) -> PutOptions {
        PutOptions {
            source_seq: Some(seq),
            ..opts(day)
        }
    }

    fn import_opts(day: u32) -> ImportOptions {
        ImportOptions {
            now: Some(t(day)),
            ..ImportOptions::default()
        }
    }

    fn syncs(reg: &Registry) -> usize {
        reg.journal
            .as_ref()
            .expect("a journal")
            .lock()
            .expect("the journal lock")
            .syncs
    }

    /// Everything the registry holds for `ids`: the live entity and the
    /// history. The scores of a creation are left out: an import does not
    /// assess what it creates.
    fn stored(reg: &Registry, ids: &[&str]) -> Vec<(Option<Entity>, Vec<VersionRecord>)> {
        ids.iter()
            .map(|id| {
                let mut history = reg.history(id, 0).unwrap_or_default();
                for h in &mut history {
                    if h.change_type == ChangeType::Created {
                        h.raw_score = 0.0;
                        h.level = ImpactLevel::None;
                        h.changed_fields.clear();
                    }
                }
                (reg.get(id, None).ok(), history)
            })
            .collect()
    }

    #[test]
    fn a_duplicate_put_never_reaches_the_engine() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("the parent stores");
        reg.put(kid(), opts(1)).expect("the child stores");
        let before = reg.gate.assessments();
        assert_eq!(before, 2);

        // The same content again, relations and time aside.
        let mut again = kid();
        again.updated_at = Some(t(5));
        let same = reg.put(again, opts(2)).expect("a no-op");
        assert!(same.unchanged && !same.stored && !same.created && !same.skipped_stale);
        assert_eq!(same.bump, VersionBump::None);
        assert_eq!(same.entity, reg.get("ENG-2", None).expect("live"));
        assert!(same.assessment.changes.is_empty());
        assert_eq!(same.assessment.level, ImpactLevel::None);
        assert_eq!(same.assessment.assessed_at, t(2));
        assert_eq!(reg.gate.assessments(), before, "the engine did not run");
        assert_eq!(reg.history("ENG-2", 0).expect("history").len(), 1);

        // A real change, a dry run and activity are still assessed.
        reg.put(
            kid(),
            PutOptions {
                dry_run: true,
                ..opts(3)
            },
        )
        .expect("a dry run");
        assert_eq!(reg.gate.assessments(), before + 1);
        let changed = reg
            .put(kid().with_title("PayPal checkout button"), opts(3))
            .expect("an update");
        assert!(changed.stored && !changed.unchanged);
        assert_eq!(reg.gate.assessments(), before + 2);
    }

    #[test]
    fn a_stale_sequence_is_skipped_without_an_assessment() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), seq(1, 10)).expect("the parent stores");
        let assessed = reg.gate.assessments();

        for stale in [10, 9, 0] {
            let out = reg
                .put(parent().with_title("An older title"), seq(2, stale))
                .expect("a skip");
            assert!(
                out.skipped_stale && !out.stored && !out.unchanged,
                "{stale}"
            );
            assert_eq!(out.entity.title(), "Checkout", "the stored entity");
            assert!(out.assessment.changes.is_empty());
        }
        // A stale put is skipped before the version check and on a dry run.
        let out = reg
            .put(
                parent().with_title("An older title"),
                PutOptions {
                    dry_run: true,
                    expected_version: Some(SemVer::new(9, 0, 0)),
                    ..seq(2, 3)
                },
            )
            .expect("a skip");
        assert!(out.skipped_stale);
        let out = reg
            .delete_with(
                "ENG-1",
                DeleteOptions {
                    now: Some(t(2)),
                    source_seq: Some(10),
                    ..DeleteOptions::default()
                },
            )
            .expect("a skip");
        assert!(out.skipped_stale && !out.deleted);
        assert_eq!(reg.gate.assessments(), assessed, "nothing was assessed");
        assert_eq!(reg.get("ENG-1", None).expect("live").title(), "Checkout");
        assert_eq!(reg.history("ENG-1", 0).expect("history").len(), 1);

        // A put without a sequence is applied and leaves the stored one.
        let out = reg
            .put(parent().with_title("Checkout flow"), opts(3))
            .expect("an update");
        assert!(out.stored);
        let out = reg
            .put(parent().with_title("An older title"), seq(4, 10))
            .expect("a skip");
        assert!(out.skipped_stale, "the sequence is still 10");
        let out = reg
            .put(parent().with_title("Checkout page"), seq(4, 11))
            .expect("an update");
        assert!(out.stored && !out.skipped_stale);

        // A deletion carries a sequence too, and an older put that arrives
        // after it does not bring the entity back.
        let out = reg
            .delete_with(
                "ENG-1",
                DeleteOptions {
                    now: Some(t(5)),
                    source_seq: Some(20),
                    ..DeleteOptions::default()
                },
            )
            .expect("a deletion");
        assert!(out.deleted && !out.skipped_stale);
        let assessed = reg.gate.assessments();
        let out = reg.put(parent(), seq(6, 15)).expect("a skip");
        assert!(out.skipped_stale && !out.stored);
        assert_eq!(out.entity.id, "ENG-1");
        assert!(reg.get("ENG-1", None).is_err(), "still deleted");
        assert_eq!(reg.gate.assessments(), assessed);
        let out = reg.put(parent(), seq(6, 21)).expect("a re-creation");
        assert!(out.created && out.stored);
    }

    #[test]
    fn a_duplicate_with_a_newer_sequence_moves_the_sequence() {
        let (dir, path) = temp_journal("seq");
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), seq(1, 5)).expect("the parent stores");
            let same = reg.put(parent(), seq(2, 9)).expect("a no-op");
            assert!(same.unchanged && !same.stored);
            assert_eq!(reg.gate.assessments(), 1);
            let late = reg
                .put(parent().with_title("An older title"), seq(3, 7))
                .expect("a skip");
            assert!(late.skipped_stale);
            reg.delete_with(
                "ENG-1",
                DeleteOptions {
                    now: Some(t(4)),
                    source_seq: Some(12),
                    ..DeleteOptions::default()
                },
            )
            .expect("a deletion");
        }
        // The sequences are in the journal: of the put, of the duplicate and
        // of the deletion.
        let text = std::fs::read_to_string(&path).expect("reads");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains(r#""source_seq":5"#), "{text}");
        assert!(lines[1].starts_with(r#"{"op":"seq""#), "{text}");
        assert!(lines[2].contains(r#""source_seq":12"#), "{text}");
        let reg = Registry::open(Engine::default(), &path, false).expect("replays");
        let late = reg.put(parent(), seq(5, 12)).expect("a skip");
        assert!(late.skipped_stale, "the deletion's sequence was replayed");
        assert!(reg.put(parent(), seq(5, 13)).expect("re-creates").created);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_lines_without_a_source_sequence_still_load() {
        let (dir, path) = temp_journal("old-lines");
        // The lines a 0.2 registry writes: no `source_seq` anywhere.
        {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            reg.put(parent(), opts(1)).expect("the parent stores");
            reg.put(kid(), opts(1)).expect("the child stores");
            reg.delete("ENG-2", false, None, Some(t(2)))
                .expect("deletes");
        }
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(!text.contains("source_seq"), "{text}");
        assert!(text.contains(r#""op":"put""#) && text.contains(r#""op":"delete""#));

        let reg = Registry::open(Engine::default(), &path, false).expect("replays");
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.history("ENG-2", 0).expect("history").len(), 2);
        // Nothing has a sequence yet, so any sequence is news.
        let out = reg
            .put(parent().with_title("Checkout flow"), seq(3, 0))
            .expect("an update");
        assert!(out.stored && !out.skipped_stale);
        let out = reg.put(parent(), seq(3, 0)).expect("a skip");
        assert!(out.skipped_stale);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn blocked() -> Entity {
        Entity::new("ENG-3", "issue")
            .with_title("Receipts")
            .with_relation(RelationKind::BlockedBy, "ENG-1")
    }

    #[test]
    fn an_import_of_new_entities_matches_putting_them_one_by_one() {
        let ids = ["ENG-1", "ENG-2", "ENG-3", "ENG-4"];
        let imported_version = Entity::new("ENG-4", "issue")
            .with_title("Invoices")
            .with_version(SemVer::new(3, 1, 0));
        // Children before the parent, so pins are back-filled as well.
        let order = [kid(), blocked(), parent(), imported_version];

        let put = Registry::new(Engine::default());
        for e in order.clone() {
            put.put(e, opts(1)).expect("stores");
        }
        let (dir, path) = temp_journal("import-equivalence");
        let imported = Registry::open(Engine::default(), &path, false).expect("opens");
        let summary = imported
            .import_with(order.into_iter().map(ImportItem::upsert), &import_opts(1))
            .expect("imports");
        assert_eq!(
            summary,
            ImportSummary {
                created: 4,
                ..ImportSummary::default()
            }
        );
        assert_eq!(imported.gate.assessments(), 0, "nothing was assessed");
        assert_eq!(stored(&imported, &ids), stored(&put, &ids));
        assert_eq!(
            imported.get("ENG-2", None).expect("live").basis,
            vec![pin("ENG-1", SemVer::INITIAL)],
            "the pin on the parent was back-filled"
        );
        assert_eq!(imported.stale(None).expect("stale"), vec![]);

        drop(imported);
        let reopened = Registry::open(Engine::default(), &path, false).expect("replays");
        assert_eq!(stored(&reopened, &ids), stored(&put, &ids));

        // A later change is assessed the same way in both.
        let change = parent().with_description("- [ ] Card payments only");
        let a = put.put(change.clone(), opts(2)).expect("updates");
        let b = reopened.put(change, opts(2)).expect("updates");
        assert_eq!(a, b);
        assert_eq!(
            reopened.stale(None).expect("stale"),
            put.stale(None).expect("stale")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_assesses_only_what_changed_and_keeps_the_input_order() {
        let reg = Registry::new(Engine::default());
        reg.import_with([parent(), kid()].map(ImportItem::upsert), &import_opts(1))
            .expect("imports");
        assert_eq!(reg.gate.assessments(), 0);

        let narrowed = parent().with_description("- [ ] Card payments only");
        let renamed = narrowed.clone().with_title("Checkout flow");
        let summary = reg
            .import_with(
                [
                    ImportItem::upsert(kid()),
                    ImportItem::upsert(narrowed.clone()),
                    ImportItem::upsert(blocked()),
                    ImportItem::upsert(renamed.clone()),
                    ImportItem::upsert(renamed),
                    ImportItem::delete("ENG-2"),
                    ImportItem::delete("ENG-2"),
                    ImportItem::delete("never-stored"),
                ],
                &import_opts(2),
            )
            .expect("imports");
        assert_eq!(
            summary,
            ImportSummary {
                created: 1,
                unchanged: 4,
                assessed: 2,
                skipped_stale: 0,
                deleted: 1,
                failed: vec![],
            }
        );
        assert_eq!(reg.gate.assessments(), 3, "two changes and a deletion");

        // The same as the same puts in the same order.
        let put = Registry::new(Engine::default());
        put.put(parent(), opts(1)).expect("stores");
        put.put(kid(), opts(1)).expect("stores");
        put.put(narrowed.clone(), opts(2)).expect("updates");
        put.put(blocked(), opts(2)).expect("stores");
        put.put(narrowed.with_title("Checkout flow"), opts(2))
            .expect("updates");
        put.delete("ENG-2", false, None, Some(t(2)))
            .expect("deletes");
        let ids = ["ENG-1", "ENG-2", "ENG-3"];
        assert_eq!(stored(&reg, &ids), stored(&put, &ids));
        let versions: Vec<SemVer> = reg
            .history("ENG-1", 0)
            .expect("history")
            .iter()
            .map(|h| h.version)
            .collect();
        assert_eq!(
            versions,
            vec![SemVer::INITIAL, SemVer::new(2, 0, 0), SemVer::new(2, 1, 0)]
        );
    }

    #[test]
    fn an_import_skips_stale_items_and_remembers_sequences() {
        let reg = Registry::new(Engine::default());
        let summary = reg
            .import_with(
                [
                    // The deletion arrives before the entity it deletes.
                    ImportItem::delete("ENG-2").with_source_seq(8),
                    ImportItem::upsert(kid()).with_source_seq(7),
                    ImportItem::upsert(parent()).with_source_seq(3),
                    ImportItem::upsert(parent().with_title("An older title")).with_source_seq(2),
                    ImportItem::upsert(parent()).with_source_seq(4),
                    ImportItem::upsert(parent().with_title("An older title")).with_source_seq(4),
                    ImportItem::delete("ENG-1").with_source_seq(1),
                ],
                &import_opts(1),
            )
            .expect("imports");
        assert_eq!(
            summary,
            ImportSummary {
                created: 1,
                unchanged: 2,
                skipped_stale: 4,
                ..ImportSummary::default()
            }
        );
        assert_eq!(reg.gate.assessments(), 0);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("ENG-1", None).expect("live").title(), "Checkout");
        assert!(reg.get("ENG-2", None).is_err(), "never came back");
    }

    #[test]
    fn an_import_rejects_a_new_entity_that_a_put_rejects() {
        let invalid = || {
            Entity::new("ENG-9", "issue")
                .with_title("Estimate")
                .with_field("estimate", crate::model::FieldValue::Number(f64::NAN))
        };
        let by_put = Registry::new(Engine::default());
        let put_error = by_put
            .put(invalid(), opts(1))
            .expect_err("a put rejects it");

        let reg = Registry::new(Engine::default());
        let summary = reg
            .import_with(
                [ImportItem::upsert(invalid()), ImportItem::upsert(parent())],
                &import_opts(1),
            )
            .expect("imports");
        assert_eq!(summary.created, 1);
        assert_eq!(summary.failed.len(), 1, "{:?}", summary.failed);
        assert_eq!(summary.failed[0].id, "ENG-9");
        assert_eq!(summary.failed[0].error, put_error.to_string());
        assert!(reg.get("ENG-9", None).is_err(), "nothing was stored");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn a_failed_import_item_does_not_stop_the_rest() {
        let (dir, path) = temp_journal("import-failures");
        let reg = Registry::open(Engine::default(), &path, false).expect("opens");
        reg.put(
            Entity::new("ENG-7", "issue")
                .with_title("At the last version")
                .with_version(SemVer::new(1, 2, u64::MAX)),
            opts(1),
        )
        .expect("stores");
        let summary = reg
            .import_with(
                [
                    ImportItem::upsert(parent()),
                    ImportItem::upsert(Entity::new("  ", "issue").with_title("No id")),
                    // A patch of a version that cannot be bumped.
                    ImportItem::upsert(
                        Entity::new("ENG-7", "issue").with_title("At the last verison"),
                    ),
                    ImportItem::upsert(kid()),
                ],
                &import_opts(2),
            )
            .expect("imports");
        assert_eq!(summary.created, 2);
        assert_eq!(summary.failed.len(), 2, "{:?}", summary.failed);
        assert_eq!(summary.failed[0].id, "  ");
        assert!(summary.failed[0].error.contains("empty id"));
        assert_eq!(summary.failed[1].id, "ENG-7");
        assert!(summary.failed[1].error.contains("overflow"));
        assert!(
            reg.get("ENG-2", None).is_ok(),
            "the item after the failures"
        );

        drop(reg);
        let reg = Registry::open(Engine::default(), &path, false).expect("replays");
        assert_eq!(reg.len(), 3);
        assert_eq!(
            reg.get("ENG-7", None).expect("live").title(),
            "At the last version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_syncs_the_journal_once() {
        let (dir, path) = temp_journal("import-sync");
        let reg = Registry::open(Engine::default(), &path, true).expect("opens");
        reg.put(parent(), opts(1)).expect("stores");
        assert_eq!(syncs(&reg), 1, "a put syncs");
        let summary = reg
            .import_with(
                [
                    ImportItem::upsert(kid()),
                    ImportItem::upsert(blocked()),
                    ImportItem::upsert(parent().with_description("- [ ] Card payments only")),
                    ImportItem::upsert(parent().with_description("- [ ] Card payments only"))
                        .with_source_seq(4),
                    ImportItem::delete("ENG-3"),
                ],
                &import_opts(2),
            )
            .expect("imports");
        assert_eq!(
            (
                summary.created,
                summary.assessed,
                summary.unchanged,
                summary.deleted
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(syncs(&reg), 2, "one sync for the whole import");
        // An import that writes nothing does not sync.
        reg.import_with([ImportItem::upsert(kid())], &import_opts(3))
            .expect("imports");
        assert_eq!(syncs(&reg), 2);
        reg.put(kid().with_title("PayPal checkout button"), opts(3))
            .expect("updates");
        assert_eq!(syncs(&reg), 3, "puts sync again");

        drop(reg);
        let reg = Registry::open(Engine::default(), &path, true).expect("replays");
        assert_eq!(reg.len(), 2);
        assert!(reg.get("ENG-3", None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_stops_when_the_journal_cannot_be_written() {
        let (dir, path) = temp_journal("import-storage");
        std::fs::write(&path, b"").expect("creates");
        let file = File::open(&path).expect("opens read-only");
        let reg = Registry::assemble(
            Engine::default(),
            State::default(),
            Some(Journal {
                file,
                path: path.clone(),
                fsync: false,
                len: 0,
                failed: false,
                unsynced: false,
                generation: 0,
                ops: 0,
                auto_floor: 0,
                syncs: 0,
            }),
            None,
        );
        let err = reg
            .import([parent(), kid()].map(ImportItem::upsert))
            .expect_err("cannot write");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(reg.is_empty(), "memory is not ahead of the journal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Snapshots and compaction ----

    fn snapshot_path(journal: &Path) -> PathBuf {
        sibling(journal, ".snapshot")
    }

    /// Everything the registry stores, in the canonical snapshot form:
    /// every record with its history, and every source sequence.
    fn dump(reg: &Registry) -> String {
        let (body, _, _) = reg.read().snapshot_body().expect("serialises");
        String::from_utf8(body).expect("utf-8")
    }

    /// Checks that `other` answers like `live`: the stored state, the stale
    /// plans, old versions, activity, and the assessment of a next change
    /// (which is built from the links between the entities).
    fn assert_same_state(live: &Registry, other: &Registry) {
        assert_eq!(dump(live), dump(other));
        assert_eq!(live.len(), other.len());
        assert_eq!(
            live.stale(None).expect("lists"),
            other.stale(None).expect("lists")
        );
        let ids: Vec<EntityId> = {
            let state = live.read();
            let mut ids: Vec<EntityId> = state.records.keys().cloned().collect();
            ids.sort();
            ids
        };
        for id in &ids {
            assert_eq!(live.get(id, None).ok(), other.get(id, None).ok(), "{id}");
            let history = live.history(id, 0).expect("history");
            assert_eq!(history, other.history(id, 0).expect("history"), "{id}");
            for h in &history {
                assert_eq!(
                    live.get(id, Some(h.version)).ok(),
                    other.get(id, Some(h.version)).ok(),
                    "{id} at {:?}",
                    h.version
                );
            }
            if live.get(id, None).is_ok() {
                let events = vec![ActivityEvent::comment(id.clone(), "Is this still planned?")];
                assert_eq!(
                    live.record_activity(id, events.clone(), Some(t(20)))
                        .expect("assesses"),
                    other
                        .record_activity(id, events, Some(t(20)))
                        .expect("assesses"),
                    "{id}"
                );
            }
        }
        if live.get("ENG-1", None).is_ok() {
            let change = parent().with_description("- [ ] Bank transfer only");
            let dry = PutOptions {
                dry_run: true,
                ..opts(21)
            };
            assert_eq!(
                live.put(change.clone(), dry.clone()).expect("assesses"),
                other.put(change, dry).expect("assesses")
            );
        }
    }

    /// A registry with a bit of everything: versions, a stale pin, a
    /// deletion, source sequences (also for an id that was never stored),
    /// and an unversioned re-pin.
    fn populate(reg: &Registry) {
        reg.put(parent(), seq(1, 5)).expect("creates");
        reg.put(kid(), seq(1, 7)).expect("creates");
        reg.put(blocked(), opts(1)).expect("creates");
        reg.put(
            Entity::new("ENG-4", "sub_issue")
                .with_parent("ENG-1")
                .with_title("Refunds"),
            opts(1),
        )
        .expect("creates");
        reg.put(parent().with_description("- [ ] Card payments"), seq(2, 6))
            .expect("a breaking update");
        reg.acknowledge("ENG-3", Some(t(3))).expect("re-pins");
        reg.delete("ENG-4", false, None, Some(t(4)))
            .expect("deletes");
        let summary = reg
            .import_with(
                [ImportItem::delete("GHOST").with_source_seq(9)],
                &import_opts(4),
            )
            .expect("imports");
        assert_eq!(summary.unchanged, 1);
    }

    fn more_writes(reg: &Registry) {
        reg.put(kid().with_title("PayPal checkout button"), seq(5, 8))
            .expect("updates");
        reg.put(
            Entity::new("ENG-5", "issue").with_title("Invoices"),
            seq(5, 1),
        )
        .expect("creates");
        reg.delete_with(
            "ENG-3",
            DeleteOptions {
                now: Some(t(6)),
                source_seq: Some(3),
                ..DeleteOptions::default()
            },
        )
        .expect("deletes");
    }

    fn reopen(path: &Path) -> Registry {
        Registry::open(Engine::default(), path, false).expect("reopens")
    }

    fn journal_generation(reg: &Registry) -> u64 {
        reg.journal
            .as_ref()
            .expect("a journal")
            .lock()
            .expect("the journal lock")
            .generation
    }

    /// A journal of `generation` that holds nothing yet.
    fn fresh_journal(generation: u64) -> Vec<u8> {
        Journal::head(generation).expect("encodes")
    }

    /// What 0.1 and 0.2 do with a journal: they know `put` and `delete`
    /// lines only, drop an unknown last line as a torn write, and fail on
    /// an unknown line that something follows. `Ok` is the number of lines
    /// they would load, `Err` the line they would call corrupt.
    fn read_like_0_2(journal: &Path) -> std::result::Result<usize, usize> {
        let text = std::fs::read_to_string(journal).expect("reads");
        let mut loaded = 0;
        let mut unreadable = None;
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(at) = unreadable {
                return Err(at);
            }
            let op: serde_json::Value = serde_json::from_str(line).expect("json");
            match op["op"].as_str() {
                Some("put" | "delete") => loaded += 1,
                _ => unreadable = Some(i + 1),
            }
        }
        Ok(loaded)
    }

    /// 0.1 and 0.2 take an unknown last line for a torn write and drop it.
    /// A journal that held only its generation line would open there as an
    /// empty store, take writes, and lose them (and hide the snapshot's
    /// state) on the way back. The note after the generation line makes the
    /// old versions fail instead.
    #[test]
    fn an_older_version_cannot_open_a_compacted_journal_as_an_empty_store() {
        let (dir, path) = temp_journal("compact-old-reader");
        let live = reopen(&path);
        populate(&live);
        // Before the first compaction nothing can be done for the old
        // versions: `populate` ends with a `seq` line, which they drop like
        // a torn write (and they ignore every `source_seq` field).
        let lines = std::fs::read_to_string(&path)
            .expect("reads")
            .lines()
            .count();
        assert_eq!(read_like_0_2(&path), Ok(lines - 1));
        live.compact().expect("compacts");
        assert_eq!(read_like_0_2(&path), Err(1), "a fresh journal");
        let text = std::fs::read_to_string(&path).expect("reads");
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            text.lines()
                .nth(1)
                .expect("a note")
                .contains("\"op\":\"note\"")
        );
        // The note is no operation, here or after a reopen.
        assert_eq!(live.journal_ops(), 0);
        drop(live);
        let reopened = reopen(&path);
        assert_eq!(reopened.journal_ops(), 0);
        assert_eq!(std::fs::read(&path).expect("reads"), fresh_journal(1));
        more_writes(&reopened);
        assert_eq!(read_like_0_2(&path), Err(1));
        let ops = reopened.journal_ops();
        assert_eq!(reopen(&path).journal_ops(), ops);

        // A restart on open (the journal is all in the snapshot) writes the
        // note too.
        let (dir2, path2) = temp_journal("compact-old-reader-2");
        let live = reopen(&path2);
        populate(&live);
        let old_journal = std::fs::read(&path2).expect("reads");
        live.compact().expect("compacts");
        std::fs::write(&path2, &old_journal).expect("puts the old journal back");
        let reopened = reopen(&path2);
        assert_eq!(std::fs::read(&path2).expect("reads"), fresh_journal(1));
        assert_eq!(read_like_0_2(&path2), Err(1));
        assert_same_state(&live, &reopened);
        drop(reopened);

        // A journal that holds only its generation line (a restart that a
        // power cut tore after the first line) gets its note back.
        std::fs::write(&path2, "{\"op\":\"generation\",\"generation\":1}\n").expect("writes");
        let reopened = reopen(&path2);
        assert_eq!(std::fs::read(&path2).expect("reads"), fresh_journal(1));
        assert_same_state(&live, &reopened);
        reopened
            .put(Entity::new("ENG-9", "issue"), opts(8))
            .expect("appends after the note");
        assert_eq!(reopened.journal_ops(), 1);
        assert_eq!(reopen(&path2).journal_ops(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// A compaction that was killed right after it renamed the snapshot has
    /// not synced the directory. `open` then cuts the journal, so it must
    /// make the snapshot's rename durable first.
    #[test]
    fn open_syncs_the_directory_before_it_restarts_the_journal() {
        let (dir, path) = temp_journal("open-syncs-dir");
        let live = reopen(&path);
        populate(&live);
        let old_journal = std::fs::read(&path).expect("reads");
        live.compact().expect("compacts");
        drop(live);
        // No restart: nothing to make durable.
        let before = DIR_SYNCS.with(std::cell::Cell::get);
        drop(reopen(&path));
        assert_eq!(DIR_SYNCS.with(std::cell::Cell::get), before);
        // The snapshot next to the journal it was taken from.
        std::fs::write(&path, &old_journal).expect("puts the old journal back");
        drop(reopen(&path));
        assert_eq!(DIR_SYNCS.with(std::cell::Cell::get), before + 1);
        assert_eq!(std::fs::read(&path).expect("reads"), fresh_journal(1));
        // An empty journal next to a snapshot.
        std::fs::write(&path, b"").expect("empties");
        drop(reopen(&path));
        assert_eq!(DIR_SYNCS.with(std::cell::Cell::get), before + 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The deletion of an id that is not stored keeps its sequence, which
    /// is a journal line like any other: it counts toward the automatic
    /// compaction although the call fails with "not found".
    #[test]
    fn a_remembered_sequence_of_a_failed_delete_counts_toward_compaction() {
        let (dir, path) = temp_journal("compact-auto-ghost");
        let options = RegistryOptions {
            compact_after_ops: Some(3),
            ..RegistryOptions::default()
        };
        let live = Registry::open_with(Engine::default(), &path, options).expect("opens");
        let ghost = |id: &str, source_seq: u64| {
            live.delete_with(
                id,
                DeleteOptions {
                    now: Some(t(1)),
                    source_seq: Some(source_seq),
                    ..DeleteOptions::default()
                },
            )
        };
        for (i, id) in ["G-1", "G-2"].into_iter().enumerate() {
            let err = ghost(id, 5).expect_err("nothing to delete");
            assert_eq!(err.kind(), ErrorKind::NotFound);
            assert_eq!(live.journal_ops(), i as u64 + 1);
        }
        // The same call again stores nothing and is answered as a skip.
        let again = ghost("G-2", 5).expect("a skip");
        assert!(again.skipped_stale && !again.deleted);
        assert_eq!(live.journal_ops(), 2);
        assert_eq!(journal_generation(&live), 0);
        let err = ghost("G-3", 5).expect_err("nothing to delete");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(journal_generation(&live), 1, "the third line compacts");
        assert_eq!(live.journal_ops(), 0);
        assert!(live.take_compaction_error().is_none());
        assert_same_state(&live, &reopen(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_compacted_registry_reopens_to_the_live_state() {
        let (dir, path) = temp_journal("compact");
        let live = reopen(&path);
        populate(&live);
        let before = std::fs::read_to_string(&path).expect("reads");
        assert!(!before.contains("generation"), "{before}");
        assert!(!snapshot_path(&path).exists());
        let ops = live.journal_ops();
        assert_eq!(ops, before.lines().count() as u64);

        let done = live.compact().expect("compacts").expect("has a journal");
        assert_eq!(done.generation, 1);
        assert_eq!(done.carried_ops, 0);
        // Five entities were stored at some point; three ids have a sequence.
        assert_eq!(done.entries, 4 + 3);
        assert_eq!(
            done.snapshot_bytes,
            std::fs::metadata(snapshot_path(&path))
                .expect("exists")
                .len()
        );
        assert_eq!(live.journal_ops(), 0);
        assert_eq!(std::fs::read(&path).expect("reads"), fresh_journal(1));
        assert!(!sibling(&path, ".snapshot.tmp").exists());
        assert!(!sibling(&path, ".tmp").exists());

        let reopened = reopen(&path);
        assert_eq!(reopened.journal_ops(), 0);
        assert_eq!(journal_generation(&reopened), 1);
        assert_same_state(&live, &reopened);
        // Sequences survive: an older change is still stale.
        assert!(
            reopened
                .put(kid().with_title("Late"), seq(9, 7))
                .expect("a skip")
                .skipped_stale
        );
        assert!(
            reopened
                .put(Entity::new("GHOST", "issue"), seq(9, 9))
                .expect("a skip")
                .skipped_stale
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_state_always_gives_the_same_snapshot() {
        let (dir, path) = temp_journal("compact-bytes");
        let (dir2, path2) = temp_journal("compact-bytes-2");
        let a = reopen(&path);
        let b = reopen(&path2);
        populate(&a);
        populate(&b);
        a.compact().expect("compacts");
        b.compact().expect("compacts");
        let bytes = std::fs::read(snapshot_path(&path)).expect("reads");
        assert_eq!(bytes, std::fs::read(snapshot_path(&path2)).expect("reads"));
        // Compacting what was loaded from a snapshot writes the same body.
        let reopened = reopen(&path);
        reopened.compact().expect("compacts");
        let again = std::fs::read(snapshot_path(&path)).expect("reads");
        let body = |bytes: &[u8]| {
            let at = bytes.iter().position(|b| *b == b'\n').expect("a header");
            bytes[at..].to_vec()
        };
        assert_eq!(body(&bytes), body(&again));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn writes_after_a_compaction_are_replayed_over_the_snapshot() {
        let (dir, path) = temp_journal("compact-writes");
        let live = reopen(&path);
        populate(&live);
        live.compact().expect("compacts");
        more_writes(&live);
        assert_eq!(live.journal_ops(), 3);
        {
            let reopened = reopen(&path);
            assert_eq!(reopened.journal_ops(), 3);
            assert_same_state(&live, &reopened);
        }
        // A second compaction, then more writes again.
        let done = live.compact().expect("compacts").expect("has a journal");
        assert_eq!(done.generation, 2);
        live.put(parent().with_title("Checkout v2"), seq(7, 10))
            .expect("updates");
        let reopened = reopen(&path);
        assert_eq!(journal_generation(&reopened), 2);
        assert_same_state(&live, &reopened);
        // The reopened registry writes to the same journal.
        drop(live);
        reopened
            .put(Entity::new("ENG-6", "issue").with_title("Taxes"), opts(8))
            .expect("creates");
        let third = reopen(&path);
        assert_same_state(&reopened, &third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crash while the snapshot is being written: the temporary file is
    /// there, the snapshot is not (or is still the old one).
    #[test]
    fn a_crash_before_the_snapshot_is_renamed_keeps_the_old_files() {
        for compacted_before in [false, true] {
            let (dir, path) = temp_journal("crash-before-rename");
            let live = reopen(&path);
            populate(&live);
            if compacted_before {
                live.compact().expect("compacts");
            }
            more_writes(&live);
            let journal = std::fs::read(&path).expect("reads");
            let snapshot = std::fs::read(snapshot_path(&path)).ok();
            std::fs::write(
                sibling(&path, ".snapshot.tmp"),
                b"{\"snapshot\":\"assessment-engine-registry\",\"format\":1,\"gener",
            )
            .expect("writes");
            std::fs::write(sibling(&path, ".tmp"), b"{\"op\":\"gen").expect("writes");

            let reopened = reopen(&path);
            assert_same_state(&live, &reopened);
            assert!(!sibling(&path, ".snapshot.tmp").exists());
            assert!(!sibling(&path, ".tmp").exists());
            assert_eq!(std::fs::read(&path).expect("reads"), journal);
            assert_eq!(std::fs::read(snapshot_path(&path)).ok(), snapshot);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A crash after the snapshot is in place and before the journal is
    /// swapped: the snapshot sits next to the journal it was taken from.
    #[test]
    fn a_crash_after_the_snapshot_is_renamed_replays_nothing_twice() {
        for compacted_before in [false, true] {
            let (dir, path) = temp_journal("crash-after-rename");
            let live = reopen(&path);
            populate(&live);
            if compacted_before {
                live.compact().expect("compacts");
                more_writes(&live);
            }
            let old_journal = std::fs::read(&path).expect("reads");
            let done = live.compact().expect("compacts").expect("has a journal");
            // The crash: the old journal is still there.
            std::fs::write(&path, &old_journal).expect("restores");

            let reopened = reopen(&path);
            assert_same_state(&live, &reopened);
            // The journal was all in the snapshot, so it was restarted.
            assert_eq!(journal_generation(&reopened), done.generation);
            assert_eq!(reopened.journal_ops(), 0);
            assert_eq!(
                std::fs::read(&path).expect("reads"),
                fresh_journal(done.generation)
            );
            // Writes after the recovery survive another reopen.
            reopened
                .put(Entity::new("ENG-7", "issue").with_title("Coupons"), opts(9))
                .expect("creates");
            assert_same_state(&reopened, &reopen(&path));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The same crash, with writes that reached the old journal while the
    /// snapshot was being saved: they are behind the covered bytes and must
    /// be replayed, once.
    #[test]
    fn a_crash_after_the_snapshot_is_renamed_keeps_later_writes() {
        for compacted_before in [false, true] {
            let (dir, path) = temp_journal("crash-after-rename-tail");
            let live = reopen(&path);
            populate(&live);
            if compacted_before {
                live.compact().expect("compacts");
                live.put(
                    Entity::new("ENG-8", "issue").with_title("Gift cards"),
                    opts(5),
                )
                .expect("creates");
            }
            let old_journal = std::fs::read(&path).expect("reads");
            let before = journal_generation(&live);
            live.compact().expect("compacts");
            more_writes(&live);
            // The new journal is its generation line and the later writes.
            let new_journal = std::fs::read_to_string(&path).expect("reads");
            let (_, tail) = new_journal.split_once('\n').expect("a generation line");
            let mut crashed = old_journal.clone();
            crashed.extend_from_slice(tail.as_bytes());
            std::fs::write(&path, &crashed).expect("restores");

            let reopened = reopen(&path);
            assert_same_state(&live, &reopened);
            // The journal still holds the later writes, so it is kept.
            assert_eq!(journal_generation(&reopened), before);
            assert_eq!(reopened.journal_ops(), 3);
            assert_eq!(std::fs::read(&path).expect("reads"), crashed);
            // Appending to it and compacting it both work.
            reopened
                .put(Entity::new("ENG-7", "issue").with_title("Coupons"), opts(9))
                .expect("creates");
            assert_same_state(&reopened, &reopen(&path));
            let done = reopened
                .compact()
                .expect("compacts")
                .expect("has a journal");
            assert_eq!(done.generation, before + 1);
            assert_same_state(&reopened, &reopen(&path));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Without `fsync`, a power cut can cut the journal below what the
    /// (always synced) snapshot covers.
    #[test]
    fn a_journal_that_is_shorter_than_its_snapshot_covers_is_restarted() {
        for keep in [0usize, 40] {
            let (dir, path) = temp_journal("crash-short-journal");
            let live = reopen(&path);
            populate(&live);
            let old_journal = std::fs::read(&path).expect("reads");
            live.compact().expect("compacts");
            std::fs::write(&path, &old_journal[..keep]).expect("cuts");

            let reopened = reopen(&path);
            assert_same_state(&live, &reopened);
            assert_eq!(journal_generation(&reopened), 1);
            assert_eq!(std::fs::read(&path).expect("reads"), fresh_journal(1));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_torn_last_journal_line_after_a_snapshot_is_dropped() {
        let (dir, path) = temp_journal("compact-torn");
        let live = reopen(&path);
        populate(&live);
        live.compact().expect("compacts");
        more_writes(&live);
        append_raw(&path, b"{\"op\":\"put\",\"enti");
        let reopened = reopen(&path);
        assert_same_state(&live, &reopened);
        reopened
            .put(Entity::new("ENG-7", "issue").with_title("Coupons"), opts(9))
            .expect("creates");
        assert_same_state(&reopened, &reopen(&path));

        // A torn generation line (the only line) is dropped too.
        let (dir2, path2) = temp_journal("compact-torn-marker");
        let live = reopen(&path2);
        populate(&live);
        live.compact().expect("compacts");
        std::fs::write(&path2, b"{\"op\":\"generation\",\"gen").expect("tears");
        let reopened = reopen(&path2);
        assert_same_state(&live, &reopened);
        assert_eq!(journal_generation(&reopened), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    fn open_error(path: &Path) -> Error {
        let err = Registry::open(Engine::default(), path, false).expect_err("must not open");
        assert_eq!(err.kind(), ErrorKind::Storage, "{err}");
        err
    }

    #[test]
    fn a_corrupt_or_cut_snapshot_is_rejected() {
        let (dir, path) = temp_journal("snapshot-corrupt");
        let snap = snapshot_path(&path);
        {
            let live = reopen(&path);
            populate(&live);
            live.compact().expect("compacts");
            more_writes(&live);
        }
        let good = std::fs::read(&snap).expect("reads");
        let journal = std::fs::read(&path).expect("reads");
        let text = String::from_utf8(good.clone()).expect("utf-8");
        let (header, body) = text.split_once('\n').expect("a header");
        let check = |bytes: &[u8], expect: &str| {
            std::fs::write(&snap, bytes).expect("writes");
            let err = open_error(&path);
            assert!(err.message().contains(expect), "{err} (wanted `{expect}`)");
            // Nothing was repaired, restarted or removed.
            assert_eq!(std::fs::read(&path).expect("reads"), journal);
            assert_eq!(std::fs::read(&snap).expect("reads"), bytes);
        };

        // One changed character in the body that still parses.
        assert!(body.contains("Checkout"));
        check(
            text.replacen("Checkout", "Checkuot", 1).as_bytes(),
            "checksum does not match",
        );
        // A body line that no longer parses.
        check(
            format!("{header}\n{}", body.replacen("{\"entry\"", "{\"entry", 1)).as_bytes(),
            "is corrupt",
        );
        // Cut in the middle of a line, and cut after a whole line.
        check(&good[..good.len() - 10], "cut short");
        let last_line = body.trim_end().rfind('\n').expect("several lines") + 1;
        check(
            format!("{header}\n{}", &body[..last_line]).as_bytes(),
            "its header counts",
        );
        // Cut inside the header, and an empty file.
        check(&good[..20], "no complete header");
        check(b"", "no complete header");
        // One line too many.
        check(
            format!("{text}{{\"entry\":\"seq\",\"id\":\"X\",\"source_seq\":1}}\n").as_bytes(),
            "its header counts",
        );
        // A header value that changed: the checksum covers the header too.
        assert!(header.contains("\"generation\":0"));
        check(
            text.replacen("\"generation\":0", "\"generation\":1", 1)
                .as_bytes(),
            "checksum does not match",
        );
        // A format this version does not know, and a file of another kind.
        check(
            text.replacen("\"format\":1", "\"format\":2", 1).as_bytes(),
            "has format 2",
        );
        check(b"not a snapshot\n", "unreadable header");
        check(
            text.replacen(SNAPSHOT_MAGIC, "something-else", 1)
                .as_bytes(),
            "is not a registry snapshot",
        );
        // A repeated entry with a matching count is still refused.
        let first_line = body.lines().next().expect("a line");
        let doubled = format!("{first_line}\n{body}");
        let mut hash = Fnv1a::new();
        hash.update(doubled.as_bytes());
        let mut forged: SnapshotHeader = serde_json::from_str(header).expect("parses");
        forged.entries += 1;
        forged.checksum = forged.checksum_of(hash);
        check(
            format!(
                "{}\n{doubled}",
                serde_json::to_string(&forged).expect("encodes")
            )
            .as_bytes(),
            "repeats an id",
        );

        // The good file still opens.
        std::fs::write(&snap, &good).expect("writes");
        assert_eq!(reopen(&path).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_and_a_snapshot_that_do_not_belong_together_are_rejected() {
        let (dir, path) = temp_journal("snapshot-mismatch");
        let snap = snapshot_path(&path);
        let live = reopen(&path);
        populate(&live);
        let journal0 = std::fs::read(&path).expect("reads");
        live.compact().expect("compacts");
        let snapshot0 = std::fs::read(&snap).expect("reads");
        more_writes(&live);
        let journal1 = std::fs::read(&path).expect("reads");
        live.compact().expect("compacts");
        let snapshot1 = std::fs::read(&snap).expect("reads");
        live.put(parent().with_title("Checkout v2"), opts(7))
            .expect("updates");
        let journal2 = std::fs::read(&path).expect("reads");
        drop(live);

        // The snapshot is gone: the journal alone is not the whole state.
        std::fs::remove_file(&snap).expect("removes");
        let err = open_error(&path);
        assert!(err.message().contains("is missing"), "{err}");
        // A journal that is two generations ahead of the snapshot.
        std::fs::write(&snap, &snapshot0).expect("writes");
        let err = open_error(&path);
        assert!(err.message().contains("do not belong together"), "{err}");
        // A journal that is older than the one the snapshot was taken from.
        std::fs::write(&snap, &snapshot1).expect("writes");
        std::fs::write(&path, &journal0).expect("writes");
        let err = open_error(&path);
        assert!(err.message().contains("do not belong together"), "{err}");
        // A snapshot that covers a length that is not a line end.
        let text = String::from_utf8(snapshot1.clone()).expect("utf-8");
        let (header, body) = text.split_once('\n').expect("a header");
        let mut forged: SnapshotHeader = serde_json::from_str(header).expect("parses");
        forged.journal_len -= 5;
        let mut hash = Fnv1a::new();
        hash.update(body.as_bytes());
        forged.checksum = forged.checksum_of(hash);
        std::fs::write(
            &snap,
            format!(
                "{}\n{body}",
                serde_json::to_string(&forged).expect("encodes")
            ),
        )
        .expect("writes");
        let mut longer = journal1.clone();
        longer.extend_from_slice(journal1.split(|b| *b == b'\n').nth(2).expect("an op"));
        longer.push(b'\n');
        std::fs::write(&path, &longer).expect("writes");
        let err = open_error(&path);
        assert!(err.message().contains("ends inside line"), "{err}");
        // A generation line in the middle of a journal.
        std::fs::write(&snap, &snapshot1).expect("writes");
        let mut twice = journal2.clone();
        twice.extend_from_slice(&journal2);
        std::fs::write(&path, &twice).expect("writes");
        let err = open_error(&path);
        assert!(err.message().contains("not the first line"), "{err}");

        // The files that belong together open.
        std::fs::write(&path, &journal2).expect("writes");
        assert_eq!(reopen(&path).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Step 3 of a compaction, with a write between the steps: what the
    /// snapshot does not hold moves into the new journal.
    #[test]
    fn writes_made_while_the_snapshot_is_saved_are_carried_over() {
        let (dir, path) = temp_journal("compact-carry");
        let live = reopen(&path);
        populate(&live);
        let journal = live.journal.as_ref().expect("a journal");
        // Steps 1 and 2, as `compact` does them.
        let (covered, body, hash, entries) = {
            let state = live.read();
            let j = journal.lock().expect("the journal lock");
            let covered = Covered {
                generation: j.generation,
                journal_len: j.len,
            };
            drop(j);
            let (body, hash, entries) = state.snapshot_body().expect("serialises");
            (covered, body, hash, entries)
        };
        write_snapshot(&path, &SnapshotHeader::new(covered, entries, hash), &body).expect("writes");
        // Writes arrive before the journal is swapped.
        more_writes(&live);
        let carried = journal
            .lock()
            .expect("the journal lock")
            .restart(covered.journal_len)
            .expect("restarts");
        assert_eq!(carried, 3);
        assert_eq!(live.journal_ops(), 3);
        assert_eq!(journal_generation(&live), 1);
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("reads")
                .lines()
                .count(),
            // The generation line, its note, and the three carried lines.
            5
        );
        // The swapped journal takes further writes.
        live.put(Entity::new("ENG-7", "issue").with_title("Coupons"), opts(9))
            .expect("creates");
        assert_same_state(&live, &reopen(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_in_memory_registry_does_not_compact() {
        let reg = Registry::new(Engine::default());
        populate(&reg);
        let before = dump(&reg);
        assert_eq!(reg.compact().expect("a no-op"), None);
        assert_eq!(reg.journal_ops(), 0);
        assert_eq!(reg.compact_after_ops(), None);
        assert!(reg.take_compaction_error().is_none());
        assert_eq!(dump(&reg), before);
    }

    #[test]
    fn the_registry_compacts_itself_after_the_configured_operations() {
        let (dir, path) = temp_journal("compact-auto");
        let options = RegistryOptions {
            compact_after_ops: Some(3),
            ..RegistryOptions::default()
        };
        let live = Registry::open_with(Engine::default(), &path, options).expect("opens");
        assert_eq!(live.compact_after_ops(), Some(3));
        live.put(parent(), opts(1)).expect("creates");
        live.put(kid(), opts(1)).expect("creates");
        assert_eq!(live.journal_ops(), 2);
        assert!(!snapshot_path(&path).exists());
        live.put(blocked(), opts(1)).expect("creates");
        assert!(
            snapshot_path(&path).exists(),
            "the third operation compacts"
        );
        assert_eq!(live.journal_ops(), 0);
        assert_eq!(journal_generation(&live), 1);
        // Every kind of write counts: an update, a re-pin, a delete, an import.
        live.put(parent().with_description("- [ ] Card payments"), opts(2))
            .expect("updates");
        live.acknowledge("ENG-2", Some(t(3))).expect("re-pins");
        assert_eq!(live.journal_ops(), 2);
        live.delete("ENG-3", false, None, Some(t(4)))
            .expect("deletes");
        assert_eq!(journal_generation(&live), 2);
        live.import_with(
            ["A-1", "A-2", "A-3", "A-4"]
                .map(|id| ImportItem::upsert(Entity::new(id, "issue").with_title("Imported"))),
            &import_opts(5),
        )
        .expect("imports");
        assert_eq!(journal_generation(&live), 3, "once, after the batch");
        assert_eq!(live.journal_ops(), 0);
        assert!(live.take_compaction_error().is_none());
        // Dry runs and skipped writes are not operations.
        live.put(
            parent().with_title("What if"),
            PutOptions {
                dry_run: true,
                ..opts(6)
            },
        )
        .expect("assesses");
        assert_eq!(live.journal_ops(), 0);
        assert_same_state(&live, &reopen(&path));

        // What is replayed on open counts toward the threshold.
        live.put(Entity::new("B-1", "issue"), opts(7))
            .expect("creates");
        live.put(Entity::new("B-2", "issue"), opts(7))
            .expect("creates");
        drop(live);
        let live = Registry::open_with(Engine::default(), &path, options).expect("opens");
        assert_eq!(live.journal_ops(), 2);
        live.put(Entity::new("B-3", "issue"), opts(7))
            .expect("creates");
        assert_eq!(journal_generation(&live), 4);
        assert_same_state(&live, &reopen(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_automatic_compaction_does_not_fail_the_write() {
        let (dir, path) = temp_journal("compact-auto-fails");
        // A directory where the snapshot belongs: the rename cannot succeed.
        std::fs::create_dir_all(snapshot_path(&path).join("in-the-way")).expect("creates");
        let options = RegistryOptions {
            compact_after_ops: Some(2),
            ..RegistryOptions::default()
        };
        let live = Registry::open_with(Engine::default(), &path, options);
        // Opening reads the snapshot, and a directory is not one.
        assert_eq!(live.expect_err("not a file").kind(), ErrorKind::Storage);
        std::fs::remove_dir_all(snapshot_path(&path)).expect("removes");

        let live = Registry::open_with(Engine::default(), &path, options).expect("opens");
        live.put(parent(), opts(1)).expect("creates");
        std::fs::create_dir_all(snapshot_path(&path).join("in-the-way")).expect("creates");
        live.put(kid(), opts(1)).expect("the write is stored");
        let err = live.take_compaction_error().expect("the compaction failed");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(live.take_compaction_error().is_none(), "taken once");
        assert_eq!(live.journal_ops(), 2);
        assert!(!sibling(&path, ".snapshot.tmp").exists());
        // The next write does not try again straight away…
        live.put(blocked(), opts(1)).expect("creates");
        assert!(live.take_compaction_error().is_none());
        // …but two operations later it does, and an explicit call reports it.
        live.put(Entity::new("ENG-5", "issue"), opts(1))
            .expect("creates");
        assert!(live.take_compaction_error().is_some());
        assert_eq!(
            live.compact().expect_err("fails").kind(),
            ErrorKind::Storage
        );
        // Nothing was lost, and it works again once the way is clear.
        std::fs::remove_dir_all(snapshot_path(&path)).expect("removes");
        assert_same_state(&live, &reopen(&path));
        live.compact().expect("compacts");
        assert_same_state(&live, &reopen(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_keeps_what_the_history_limit_kept() {
        let (dir, path) = temp_journal("compact-history");
        let options = RegistryOptions {
            history_limit: Some(2),
            ..RegistryOptions::default()
        };
        let live = Registry::open_with(Engine::default(), &path, options).expect("opens");
        assert_eq!(live.history_limit(), Some(2));
        for (day, estimate) in [(1, 1.0), (2, 2.0), (3, 3.0), (4, 5.0)] {
            live.put(parent().with_estimate(estimate), opts(day))
                .expect("puts");
        }
        let history = live.history("ENG-1", 0).expect("history");
        assert_eq!(history.len(), 4);
        assert_eq!(history.iter().filter(|h| h.snapshot.is_some()).count(), 2);
        live.compact().expect("compacts");

        let limited = Registry::open_with(Engine::default(), &path, options).expect("opens");
        assert_same_state(&live, &limited);
        // Without the limit, the trimmed bodies are still gone: the snapshot
        // never held them. The version metadata is complete.
        let unlimited = reopen(&path);
        assert_eq!(unlimited.history_limit(), None);
        assert_eq!(unlimited.history("ENG-1", 0).expect("history"), history);
        assert_eq!(
            unlimited
                .get("ENG-1", Some(history[0].version))
                .expect_err("trimmed")
                .kind(),
            ErrorKind::NotFound
        );
        // A tighter limit applies while the snapshot loads.
        let tighter = Registry::open_with(
            Engine::default(),
            &path,
            RegistryOptions {
                history_limit: Some(1),
                ..RegistryOptions::default()
            },
        )
        .expect("opens");
        let kept = tighter.history("ENG-1", 0).expect("history");
        assert_eq!(kept.iter().filter(|h| h.snapshot.is_some()).count(), 1);
        assert!(kept.last().expect("a version").snapshot.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writes_and_compactions_lose_nothing() {
        let (dir, path) = temp_journal("compact-concurrent");
        let live = Registry::open_with(
            Engine::default(),
            &path,
            RegistryOptions {
                compact_after_ops: Some(5),
                ..RegistryOptions::default()
            },
        )
        .expect("opens");
        live.put(parent(), opts(1)).expect("creates");
        std::thread::scope(|s| {
            for n in 0..4 {
                let reg = &live;
                s.spawn(move || {
                    for v in 0..8 {
                        reg.put(
                            Entity::new(format!("K-{n}"), "sub_issue")
                                .with_parent("ENG-1")
                                .with_title(format!("Child {n}"))
                                .with_estimate(f64::from(1_u32 << v)),
                            opts(2),
                        )
                        .expect("puts");
                        // A read between writes and compactions.
                        assert!(reg.get(&format!("K-{n}"), None).is_ok());
                    }
                });
            }
            let reg = &live;
            s.spawn(move || {
                for _ in 0..6 {
                    reg.compact().expect("compacts");
                }
            });
        });
        assert!(live.take_compaction_error().is_none());
        for n in 0..4 {
            assert_eq!(
                live.history(&format!("K-{n}"), 0).expect("history").len(),
                8
            );
        }
        assert!(journal_generation(&live) >= 6);
        assert_same_state(&live, &reopen(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_checksum_is_fnv_1a_64() {
        let hash = |bytes: &[u8]| {
            let mut h = Fnv1a::new();
            h.update(bytes);
            h.0
        };
        // The published test vectors.
        assert_eq!(hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash(b"foobar"), 0x8594_4171_f739_67e8);
        let mut split = Fnv1a::new();
        split.update(b"foo");
        split.update(b"bar");
        assert_eq!(split.0, hash(b"foobar"));
    }

    /// A parent `P` with `n` sub-issues; every text is its own.
    fn family(n: usize) -> Vec<Entity> {
        let mut out = vec![
            Entity::new("P", "issue")
                .with_title("Account security")
                .with_description("- [ ] Password login\n- [ ] Password reset by email")
                .with_status(StatusCategory::Started, "In Progress"),
        ];
        out.extend((0..n).map(|i| {
            Entity::new(format!("C{i:04}"), "sub_issue")
                .with_parent("P")
                .with_title(format!("Password reset step {i}"))
                .with_description(format!("Send reset email number {i} with a link."))
                .with_status(StatusCategory::Started, "In Progress")
        }));
        out
    }

    /// Two texts for `P` that differ, so that every put is a change.
    fn rewrite(oauth: bool) -> Entity {
        let text = if oauth {
            "- [ ] Google OAuth login only\nPassword reset is not needed."
        } else {
            "- [ ] Password login\n- [ ] Password reset by text message"
        };
        family(0).remove(0).with_description(text)
    }

    /// The texts this thread analyses while `f` runs.
    fn analysed_by<R>(f: impl FnOnce() -> R) -> (R, u64) {
        let before = crate::text::analysed::texts();
        let out = f();
        (out, crate::text::analysed::texts() - before)
    }

    /// Loads a family of `n`, puts two rewrites of `P`, and returns the
    /// texts the first and the second put analysed.
    fn texts_of_two_puts(n: usize, cache: bool, imported: bool) -> (u64, u64) {
        let reg = Registry::new(Engine::default()).with_analysis_cache(cache);
        if imported {
            reg.import(family(n).into_iter().map(ImportItem::upsert))
                .expect("imports");
        } else {
            for e in family(n) {
                reg.put(e, opts(1)).expect("stores");
            }
        }
        let (first, first_texts) = analysed_by(|| reg.put(rewrite(true), opts(2)).expect("puts"));
        let (second, second_texts) =
            analysed_by(|| reg.put(rewrite(false), opts(3)).expect("puts"));
        for outcome in [first, second] {
            assert!(outcome.stored && !outcome.assessment.impacts.is_empty());
        }
        (first_texts, second_texts)
    }

    #[test]
    fn a_second_put_on_a_stored_graph_analyses_the_same_few_texts_at_any_size() {
        for imported in [false, true] {
            let (small_first, small) = texts_of_two_puts(10, true, imported);
            let (large_first, large) = texts_of_two_puts(300, true, imported);
            assert_eq!(small, large, "imported {imported}");
            assert!(small <= 12, "{small} texts for one changed entity");
            if imported {
                // Nothing was analysed at import: the first put pays for it.
                assert!(large_first >= small_first + 2 * 290);
            }

            // Without the cache every put analyses the whole context again.
            let (_, small_off) = texts_of_two_puts(10, false, imported);
            let (_, large_off) = texts_of_two_puts(300, false, imported);
            assert!(large_off >= small_off + 2 * 290, "imported {imported}");
            assert!(small_off > small);
        }
    }

    #[test]
    fn an_import_analyses_no_text_of_a_new_entity() {
        let reg = Registry::new(Engine::default());
        let (_, texts) = analysed_by(|| {
            reg.import(family(50).into_iter().map(ImportItem::upsert))
                .expect("imports")
        });
        assert_eq!(texts, 0);
        let state = reg.read();
        assert!(state.records.values().all(|r| !r.stats.is_filled()));
    }

    #[test]
    fn a_text_change_drops_the_kept_analysis_and_other_changes_keep_it() {
        let reg = Registry::new(Engine::default());
        for e in family(3) {
            reg.put(e, opts(1)).expect("stores");
        }
        reg.put(rewrite(true), opts(2)).expect("puts");
        let cell = |id: &str| Arc::clone(&reg.read().records[id].stats);
        let (parent, child) = (cell("P"), cell("C0001"));
        assert!(parent.is_filled() && child.is_filled());

        // An estimate is no text: the analysis stays.
        let stored = reg.get("C0001", None).expect("stored");
        reg.put(stored.clone().with_estimate(5.0), opts(3))
            .expect("puts");
        assert!(Arc::ptr_eq(&child, &cell("C0001")));

        // A new description: the entity gets the analysis of its new text,
        // and the old one is not touched.
        let (_, texts) = analysed_by(|| {
            reg.put(
                stored.with_description("Send a text message with a code."),
                opts(4),
            )
            .expect("puts")
        });
        assert!(texts > 0);
        let renewed = cell("C0001");
        assert!(!Arc::ptr_eq(&child, &renewed));
        assert!(renewed.is_filled());
        assert!(
            Arc::ptr_eq(&parent, &cell("P")),
            "the parent did not change"
        );

        // A dry run stores nothing, so the stored analysis stays.
        reg.put(
            rewrite(false),
            PutOptions {
                dry_run: true,
                ..opts(5)
            },
        )
        .expect("assesses");
        assert!(Arc::ptr_eq(&parent, &cell("P")));

        // A deletion and a new creation start from an empty cell. The
        // tombstone keeps no analysis, also not the one its own assessment
        // made (nothing reads the text of a deleted entity again).
        let filled = cell("C0002");
        assert!(filled.is_filled());
        reg.delete("C0002", false, None, Some(t(6)))
            .expect("deletes");
        assert!(!Arc::ptr_eq(&filled, &cell("C0002")));
        assert!(!cell("C0002").is_filled(), "the tombstone dropped it");
        reg.import([ImportItem::upsert(
            Entity::new("X", "issue").with_title("Never assessed"),
        )])
        .expect("imports");
        let empty = cell("X");
        assert!(!empty.is_filled());
        reg.delete("X", false, None, Some(t(6))).expect("deletes");
        assert!(empty.is_filled(), "the deletion analysed the text");
        assert!(!cell("X").is_filled(), "and the tombstone does not keep it");
        reg.put(family(3).remove(3), opts(7))
            .expect("creates again");
        assert!(reg.get("C0002", None).is_ok());
    }

    #[test]
    fn the_kept_analysis_is_built_again_after_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "assessment-analysis-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("registry.journal");
        let expected = {
            let reg = Registry::open(Engine::default(), &path, false).expect("opens");
            for e in family(5) {
                reg.put(e, opts(1)).expect("stores");
            }
            reg.put(rewrite(true), opts(2)).expect("puts");
            reg.put(
                rewrite(false),
                PutOptions {
                    dry_run: true,
                    ..opts(3)
                },
            )
            .expect("assesses")
        };
        let reg = Registry::open(Engine::default(), &path, false).expect("opens again");
        assert!(reg.read().records.values().all(|r| !r.stats.is_filled()));
        let dry = PutOptions {
            dry_run: true,
            ..opts(3)
        };
        let again = reg.put(rewrite(false), dry).expect("assesses");
        assert_eq!(again, expected);
        assert!(reg.read().records["C0001"].stats.is_filled());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
