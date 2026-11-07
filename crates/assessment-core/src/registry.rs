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
//!
//! **History.** Every version keeps its metadata ([`VersionRecord`]) for
//! good. Snapshot bodies are kept for every version by default; bound the
//! memory they take with [`Registry::with_history_limit`] (or
//! [`Registry::open_with_history_limit`], which applies the limit while
//! replaying). The journal itself is not compacted.
//!
//! **Concurrency.** A put or delete is assessed under a shared lock, so reads
//! and dry runs proceed meanwhile; it then commits under the exclusive lock
//! if the entity has not changed in between, or is assessed again under that
//! lock if it has. Neighbours that change meanwhile do not cause a retry: the
//! assessment, and the versions auto-pinned for the entity, are those of the
//! neighbourhood it was assessed against (so a pin can only be older than
//! the neighbour, which [`Registry::stale`] then reports).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::assessment::{Assessment, BasisStatus, ChangeAssessment, ChangeType, ImpactLevel};
use crate::engine::Engine;
use crate::error::{Error, ErrorKind, Result};
use crate::model::{
    ActivityEvent, AssessOptions, AssessmentRequest, BasisPin, ChangeSet, Entity, EntityId,
    EntityKind, HistoryPoint, RelationKind,
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
        }
    }
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
    pub assessment: Assessment,
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
}

impl State {
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

    fn apply(&mut self, op: JournalOp) {
        let keep = self.history_limit;
        match op {
            JournalOp::Put { entity, record } => {
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
                        };
                        rec.retain_snapshots(keep);
                        self.records.insert(rec.current.id.clone(), rec);
                    }
                }
            }
            JournalOp::Delete { id, record } => {
                if let Some(rec) = self.records.get_mut(&id) {
                    self.links.unlink(&rec.current);
                    rec.deleted = true;
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
    /// `exclude`: every entity at most once, sorted by id.
    fn context(&self, seeds: &[&str], exclude: &str, depth: u32) -> Vec<Entity> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        for s in seeds {
            if seen.insert((*s).to_owned()) {
                queue.push_back(((*s).to_owned(), 0));
            }
        }
        let mut out = Vec::new();
        while let Some((id, d)) = queue.pop_front() {
            let mut next: Vec<String> = Vec::new();
            if let Some(e) = self.live(&id) {
                if id != exclude {
                    out.push(e.clone());
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
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
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
    },
    Delete {
        id: EntityId,
        record: VersionRecord,
    },
}

#[derive(Debug)]
struct Journal {
    file: File,
    path: PathBuf,
    fsync: bool,
    /// The length of the file up to the last acknowledged write.
    len: u64,
    /// A write failed and could not be undone: the file no longer matches memory.
    failed: bool,
}

impl Journal {
    /// Opens the journal at `path` (creating it if missing), streams it into
    /// `state`, and repairs its tail so that the next append starts a line:
    /// an unreadable last line (a torn write: cut JSON, a cut UTF-8
    /// character) is truncated, and a last record without its newline gets one.
    fn open(path: PathBuf, fsync: bool, state: &mut State) -> Result<Self> {
        let io = |what: &str, e: std::io::Error| {
            Error::with_source(
                ErrorKind::Storage,
                format!("the journal `{}` could not be {what}", path.display()),
                e,
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
                match serde_json::from_slice::<JournalOp>(body) {
                    Ok(op) => {
                        state.apply(op);
                        missing_newline = line.last() != Some(&b'\n');
                    }
                    Err(e) => unreadable = Some((number, start, e)),
                }
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
        let len = file.metadata().map_err(|e| io("read", e))?.len();
        Ok(Self {
            file,
            path,
            fsync,
            len,
            failed: false,
        })
    }

    /// Appends `ops`, all or nothing: a write that fails part-way is
    /// truncated away again, so the file never holds what memory does not.
    fn append(&mut self, ops: &[JournalOp]) -> Result<()> {
        if self.failed {
            return Err(Error::new(
                ErrorKind::Storage,
                format!(
                    "the journal `{}` could not be restored after a failed write; reopen the registry",
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
        match self.write(&bytes) {
            Ok(()) => {
                self.len += bytes.len() as u64;
                Ok(())
            }
            Err(e) => {
                let undone = self.file.set_len(self.len).and_then(|()| {
                    if self.fsync {
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

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.file.flush()?;
        if self.fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

/// A thread-safe, versioned entity registry.
#[derive(Debug)]
pub struct Registry {
    engine: Engine,
    state: RwLock<State>,
    journal: Option<Mutex<Journal>>,
}

impl Registry {
    /// An in-memory registry.
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            state: RwLock::new(State::default()),
            journal: None,
        }
    }

    /// A registry backed by the journal at `path` (created if missing,
    /// replayed if present). With `fsync`, every write is synced to disk.
    ///
    /// A torn last line (a crash mid-write) is dropped and truncated, and a
    /// last record that lost its newline gets it back, before anything new
    /// is appended; corruption anywhere else fails with [`ErrorKind::Storage`].
    pub fn open(engine: Engine, path: impl AsRef<Path>, fsync: bool) -> Result<Self> {
        Self::open_inner(engine, path.as_ref(), fsync, None)
    }

    /// Like [`Registry::open`], with [`Registry::with_history_limit`]
    /// applied while replaying, so the full history is never held in memory.
    pub fn open_with_history_limit(
        engine: Engine,
        path: impl AsRef<Path>,
        fsync: bool,
        snapshots: usize,
    ) -> Result<Self> {
        Self::open_inner(engine, path.as_ref(), fsync, Some(snapshots.max(1)))
    }

    fn open_inner(engine: Engine, path: &Path, fsync: bool, keep: Option<usize>) -> Result<Self> {
        let mut state = State {
            history_limit: keep,
            ..State::default()
        };
        let journal = Journal::open(path.to_path_buf(), fsync, &mut state)?;
        Ok(Self {
            engine,
            state: RwLock::new(state),
            journal: Some(Mutex::new(journal)),
        })
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

    /// The number of newest versions per entity whose snapshots are kept
    /// (`None` = all).
    pub fn history_limit(&self) -> Option<usize> {
        self.read().history_limit
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

    fn persist(&self, ops: &[JournalOp]) -> Result<()> {
        match &self.journal {
            Some(j) => j.lock().unwrap_or_else(PoisonError::into_inner).append(ops),
            None => Ok(()),
        }
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
    pub fn put(&self, entity: Entity, opts: PutOptions) -> Result<PutOutcome> {
        if entity.id.trim().is_empty() {
            return Err(Error::invalid("an entity has an empty id"));
        }
        let now = opts.now.unwrap_or_else(Utc::now);
        if opts.dry_run {
            let state = self.read();
            let p = self.prepare_put(&state, entity, &opts, now)?;
            return Ok(p.into_outcome(false));
        }
        let assessed = self.assess_put(entity.clone(), &opts, now)?;
        self.commit_put(entity, &opts, now, assessed)
    }

    /// The expensive half of a put, under the shared lock: the assessment,
    /// and the entity's revision it was made against.
    fn assess_put(
        &self,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
    ) -> Result<(Prepared, Option<u64>)> {
        let state = self.read();
        let revision = state.revision(&entity.id);
        Ok((self.prepare_put(&state, entity, opts, now)?, revision))
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
    ) -> Result<PutOutcome> {
        let mut state = self.write();
        let p = if state.revision(&entity.id) == revision {
            assessed
        } else {
            self.prepare_put(&state, entity, opts, now)?
        };
        if p.noop {
            return Ok(p.into_outcome(false));
        }
        let mut ops = vec![JournalOp::Put {
            entity: p.entity.clone(),
            record: p.record.clone(),
        }];
        if p.created && opts.auto_pin {
            ops.extend(state.backfill_pins(&p.entity));
        }
        self.persist(&ops)?;
        for op in ops {
            state.apply(op);
        }
        Ok(p.into_outcome(true))
    }

    fn prepare_put(
        &self,
        state: &State,
        entity: Entity,
        opts: &PutOptions,
        now: DateTime<Utc>,
    ) -> Result<Prepared> {
        let existing = state.records.get(&entity.id);
        let live = existing.filter(|r| !r.deleted);
        check_expected(&entity.id, live, opts.expected_version)?;

        let mut after = entity;
        normalize(&mut after);
        let explicit_basis = !after.basis.is_empty();
        let created = live.is_none();
        let previous_version = live.and_then(|r| r.current.version);
        match live {
            Some(rec) => {
                if !explicit_basis {
                    after.basis = rec.current.basis.clone();
                    if opts.auto_pin {
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
                if opts.auto_pin && !explicit_basis {
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

        let mut seeds: BTreeSet<&str> = BTreeSet::from([after.id.as_str()]);
        seeds.extend(after.parent_id.as_deref());
        seeds.extend(live.and_then(|r| r.current.parent_id.as_deref()));
        seeds.extend(after.relations.iter().map(|r| r.target.as_str()));
        let seeds: Vec<&str> = seeds.into_iter().collect();
        let context = state.context(&seeds, &after.id, self.depth());
        let change = match live {
            Some(rec) => ChangeSet::update(rec.current.clone(), after.clone()),
            None => ChangeSet::create(after.clone()),
        }
        .with_events(opts.events.clone());
        let request = AssessmentRequest {
            changes: vec![change],
            context,
            history: state.history_points(&after.id),
            now: Some(now),
            options: opts.assess.clone(),
        };
        let assessment = self.engine.assess(&request)?;
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
        });
        let noop = !created
            && bump == VersionBump::None
            && live.is_some_and(|r| same_content(&r.current, &after));
        if noop && let Some(rec) = live {
            after = rec.current.clone();
        }
        Ok(Prepared {
            entity: after,
            created,
            bump,
            assessment,
            record,
            noop,
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
        let now = now.unwrap_or_else(Utc::now);
        if dry_run {
            let state = self.read();
            return self
                .prepare_delete(&state, id, expected_version, now)
                .map(|(a, _)| a);
        }
        // Assess under the shared lock; commit under the exclusive one, or
        // assess again under it if the entity changed meanwhile.
        let (assessed, revision) = {
            let state = self.read();
            (
                self.prepare_delete(&state, id, expected_version, now)?,
                state.revision(id),
            )
        };
        let mut state = self.write();
        let (assessment, op) = if state.revision(id) == revision {
            assessed
        } else {
            self.prepare_delete(&state, id, expected_version, now)?
        };
        self.persist(std::slice::from_ref(&op))?;
        state.apply(op);
        Ok(assessment)
    }

    fn prepare_delete(
        &self,
        state: &State,
        id: &str,
        expected_version: Option<SemVer>,
        now: DateTime<Utc>,
    ) -> Result<(Assessment, JournalOp)> {
        let rec = state
            .records
            .get(id)
            .filter(|r| !r.deleted)
            .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
        check_expected(id, Some(rec), expected_version)?;
        let before = rec.current.clone();
        let mut seeds: BTreeSet<&str> = BTreeSet::from([id]);
        seeds.extend(before.parent_id.as_deref());
        let seeds: Vec<&str> = seeds.into_iter().collect();
        let context = state.context(&seeds, id, self.depth());
        let assessment = self.engine.assess(&AssessmentRequest {
            changes: vec![ChangeSet::delete(before.clone())],
            context,
            history: state.history_points(id),
            now: Some(now),
            options: AssessOptions::default(),
        })?;
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
            },
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
        let state = self.read();
        let current = state
            .live(id)
            .ok_or_else(|| Error::not_found(format!("entity `{id}` does not exist")))?;
        let mut seeds: Vec<&str> = vec![id];
        if let Some(p) = &current.parent_id {
            seeds.push(p);
        }
        let mut context = state.context(&seeds, "", self.depth());
        if !context.iter().any(|e| e.id == id) {
            context.push(current.clone());
        }
        self.engine.assess(&AssessmentRequest {
            changes: vec![ChangeSet::activity(id, events)],
            context,
            history: state.history_points(id),
            now,
            options: AssessOptions::default(),
        })
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
    pub fn compare(
        &self,
        id: &str,
        from: SemVer,
        to: SemVer,
        now: Option<DateTime<Utc>>,
    ) -> Result<Assessment> {
        let before = self.get(id, Some(from))?;
        let after = self.get(id, Some(to))?;
        let state = self.read();
        let mut seeds: Vec<&str> = vec![id];
        if let Some(p) = &after.parent_id {
            seeds.push(p);
        }
        let context = state.context(&seeds, id, self.depth());
        self.engine.assess(&AssessmentRequest {
            changes: vec![ChangeSet::update(before, after)],
            context,
            history: Vec::new(),
            now,
            options: AssessOptions::default(),
        })
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
        };
        self.persist(std::slice::from_ref(&op))?;
        state.apply(op);
        Ok(updated)
    }
}

struct Prepared {
    entity: Entity,
    created: bool,
    bump: VersionBump,
    assessment: Assessment,
    record: Option<VersionRecord>,
    noop: bool,
}

impl Prepared {
    fn into_outcome(self, stored: bool) -> PutOutcome {
        PutOutcome {
            entity: self.entity,
            created: self.created,
            stored,
            bump: self.bump,
            assessment: self.assessment,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StatusCategory;
    use chrono::TimeZone;

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
        let ctx = state.context(&["ENG-2", "ENG-1", "ENG-1", "ENG-2"], "", 3);
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
        let reg = Registry {
            engine: Engine::default(),
            state: RwLock::new(State::default()),
            journal: Some(Mutex::new(Journal {
                file,
                path: path.clone(),
                fsync: false,
                len: 0,
                failed: false,
            })),
        };
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

    /// Finding 34: the assessment runs under the shared lock; a commit that
    /// finds the entity changed meanwhile assesses again instead of
    /// overwriting the newer version.
    #[test]
    fn a_put_assessed_against_an_older_state_is_assessed_again() {
        let reg = Registry::new(Engine::default());
        reg.put(parent(), opts(1)).expect("creates");

        // No interference: the optimistic assessment is committed as is.
        let typo = parent().with_title("Checkuot");
        let o = opts(2);
        let assessed = reg.assess_put(typo.clone(), &o, t(2)).expect("assesses");
        let expected = assessed.0.assessment.clone();
        let out = reg.commit_put(typo, &o, t(2), assessed).expect("commits");
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
        let out = reg.commit_put(mine, &o, t(3), assessed).expect("commits");
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
            }),
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
}
