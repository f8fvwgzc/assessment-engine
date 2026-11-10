//! Bulk import through the public API: loading a graph with
//! `Registry::import` leaves the registry in the state that putting the same
//! entities one at a time does, and the journal reproduces it.

mod support;

use assessment_core::prelude::*;
use assessment_core::registry::{
    DeleteOptions, ImportItem, ImportOptions, ImportSummary, PutOptions, Registry, VersionRecord,
};

fn put_opts() -> PutOptions {
    PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    }
}

fn import_opts() -> ImportOptions {
    ImportOptions {
        now: Some(support::now()),
        ..ImportOptions::default()
    }
}

/// The live entity and the history of every id. The scores of a creation
/// are left out: an import stores new entities without assessing them.
fn stored(registry: &Registry, ids: &[String]) -> Vec<(Entity, Vec<VersionRecord>)> {
    ids.iter()
        .map(|id| {
            let mut history = registry.history(id, 0).expect("the history is kept");
            for h in &mut history {
                if h.change_type == ChangeType::Created {
                    h.raw_score = 0.0;
                    h.level = ImpactLevel::None;
                    h.changed_fields.clear();
                }
            }
            (registry.get(id, None).expect("the entity is live"), history)
        })
        .collect()
}

fn journal(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("assessment-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("journal.jsonl");
    (dir, path)
}

/// Imports `entities`, puts them one by one into a second registry, and
/// checks that both, and the reopened journal, hold the same thing.
fn import_matches_puts(name: &str, entities: Vec<Entity>) {
    let ids: Vec<String> = entities.iter().map(|e| e.id.clone()).collect();
    let put = Registry::new(Engine::default());
    for e in entities.clone() {
        put.put(e, put_opts()).expect("the entity stores");
    }

    let (dir, path) = journal(name);
    let imported = Registry::open(Engine::default(), &path, false).expect("opens");
    let summary = imported
        .import_with(
            entities.iter().cloned().map(ImportItem::upsert),
            &import_opts(),
        )
        .expect("imports");
    assert_eq!(
        summary,
        ImportSummary {
            created: ids.len() as u64,
            ..ImportSummary::default()
        }
    );
    assert_eq!(imported.len(), put.len());
    assert_eq!(stored(&imported, &ids), stored(&put, &ids), "{name}");
    assert_eq!(
        imported.stale(None).expect("stale lists"),
        put.stale(None).expect("stale lists")
    );

    // Importing the same batch again changes nothing.
    let again = imported
        .import_with(entities.into_iter().map(ImportItem::upsert), &import_opts())
        .expect("imports");
    assert_eq!(
        again,
        ImportSummary {
            unchanged: ids.len() as u64,
            ..ImportSummary::default()
        }
    );
    assert_eq!(stored(&imported, &ids), stored(&put, &ids), "{name}");

    drop(imported);
    let reopened = Registry::open(Engine::default(), &path, false).expect("replays");
    assert_eq!(stored(&reopened, &ids), stored(&put, &ids), "{name}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn importing_a_graph_matches_putting_its_entities_one_by_one() {
    for (name, request) in [
        ("cousins", support::cousins(12, 3, 120, 25)),
        ("deep-chain", support::deep_chain(12, 120, 25)),
        ("blocking-chain", support::blocking_chain(20, 120, 25)),
        ("fan-out", support::fan_out(40, 120, 25)),
    ] {
        let ordered = support::load_order(&request);
        import_matches_puts(name, ordered.clone());
        // Children before parents: every pin is back-filled.
        let reversed: Vec<Entity> = ordered.into_iter().rev().collect();
        import_matches_puts(&format!("{name}-reversed"), reversed);
    }
}

/// After an import, a change is assessed exactly as after the same puts.
#[test]
fn a_change_after_an_import_is_assessed_like_after_puts() {
    let request = support::cousins(12, 3, 120, 25);
    let entities = support::load_order(&request);
    let put = Registry::new(Engine::default());
    for e in entities.clone() {
        put.put(e, put_opts()).expect("the entity stores");
    }
    let imported = Registry::new(Engine::default());
    imported
        .import_with(entities.into_iter().map(ImportItem::upsert), &import_opts())
        .expect("imports");

    let rewrite = request.changes[0]
        .after
        .clone()
        .expect("the change has an after");
    let expected = put.put(rewrite.clone(), put_opts()).expect("updates");
    let summary = imported
        .import_with([ImportItem::upsert(rewrite.clone())], &import_opts())
        .expect("imports");
    assert_eq!(summary.assessed, 1);
    assert_eq!(
        imported.get(&rewrite.id, None).expect("live"),
        expected.entity
    );
    assert_eq!(
        imported.history(&rewrite.id, 1).expect("history"),
        put.history(&rewrite.id, 1).expect("history")
    );
    assert_eq!(
        imported.stale(None).expect("stale lists"),
        put.stale(None).expect("stale lists")
    );
}

fn delete_at(seq: u64) -> DeleteOptions {
    DeleteOptions {
        now: Some(support::now()),
        source_seq: Some(seq),
        ..DeleteOptions::default()
    }
}

fn put_at(seq: u64) -> PutOptions {
    PutOptions {
        source_seq: Some(seq),
        ..put_opts()
    }
}

/// The source created an item at sequence 3 and deleted it at 5, and the
/// deletion arrives first. The late put must not bring the item back.
#[test]
fn a_delete_of_an_item_that_is_not_stored_remembers_its_sequence() {
    let (dir, path) = journal("delete-missing-seq");
    let reg = Registry::open(Engine::default(), &path, false).expect("opens");
    let late = support::item("ENG-1", "issue", None, 1, 40);

    let err = reg
        .delete_with("ENG-1", delete_at(5))
        .expect_err("nothing is stored to delete");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    let check = |reg: &Registry, when: &str| {
        let o = reg.put(late.clone(), put_at(3)).expect("the put answers");
        assert!(o.skipped_stale, "{when}");
        assert!(!o.stored, "{when}");
        assert_eq!(reg.len(), 0, "{when}");
    };
    check(&reg, "live");
    drop(reg);
    let reg = Registry::open(Engine::default(), &path, false).expect("replays");
    check(&reg, "after a reopen");
    reg.compact().expect("compacts");
    check(&reg, "after a compaction");
    drop(reg);
    let reg = Registry::open(Engine::default(), &path, false).expect("loads the snapshot");
    check(&reg, "after a reopen from the snapshot");

    // A newer put is still applied.
    let o = reg.put(late.clone(), put_at(6)).expect("stores");
    assert!(o.stored && o.created);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second deletion with a higher sequence, of an item that is already
/// deleted, raises the remembered sequence.
#[test]
fn a_delete_of_a_deleted_item_raises_its_sequence() {
    let reg = Registry::new(Engine::default());
    let item = support::item("ENG-1", "issue", None, 1, 40);
    reg.put(item.clone(), put_at(1)).expect("stores");
    assert!(
        reg.delete_with("ENG-1", delete_at(2))
            .expect("deletes")
            .deleted
    );
    let err = reg
        .delete_with("ENG-1", delete_at(9))
        .expect_err("already deleted");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    let o = reg.put(item, put_at(7)).expect("the put answers");
    assert!(o.skipped_stale);
    assert_eq!(reg.len(), 0);
}

/// A what-if deletion stores nothing, also no sequence.
#[test]
fn a_dry_run_delete_of_an_item_that_is_not_stored_remembers_nothing() {
    let reg = Registry::new(Engine::default());
    let err = reg
        .delete_with(
            "ENG-1",
            DeleteOptions {
                dry_run: true,
                ..delete_at(5)
            },
        )
        .expect_err("nothing is stored to delete");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    let o = reg
        .put(support::item("ENG-1", "issue", None, 1, 40), put_at(3))
        .expect("stores");
    assert!(o.stored && !o.skipped_stale);
}

/// The items of an import may come from an iterator that reads the registry
/// (or a file, or a channel). No registry lock is held while it runs.
#[test]
fn an_import_reads_its_items_without_holding_the_registry_lock() {
    let reg = std::sync::Arc::new(Registry::new(Engine::default()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = {
        let reg = std::sync::Arc::clone(&reg);
        std::thread::spawn(move || {
            let seen = std::cell::Cell::new(0usize);
            let items = (0..3000u64).map(|i| {
                // Deadlocks if the import holds its lock here.
                seen.set(seen.get().max(reg.len()));
                ImportItem::upsert(support::item(&format!("E-{i}"), "issue", None, i, 20))
            });
            let summary = reg.import_with(items, &import_opts());
            let _ = done_tx.send((summary, seen.get()));
        })
    };
    let (summary, seen) = done_rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the import returns: its iterator may read the registry");
    worker.join().expect("the import thread ends");
    assert_eq!(summary.expect("imports").created, 3000);
    assert!(seen > 0, "items were applied while later ones were read");
    assert_eq!(reg.len(), 3000);
}

/// The registry lock is free between two runs of an import: while the
/// import waits for the first item of its second run, another thread reads
/// what the first run stored and commits a write of its own.
#[test]
fn readers_and_writers_get_in_between_two_runs_of_an_import() {
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// The import takes its items in runs of this many.
    const RUN: u64 = 1024;
    const WAIT: Duration = Duration::from_secs(20);

    let reg = Arc::new(Registry::new(Engine::default()));
    let (paused_tx, paused_rx) = channel::<()>();
    let (resume_tx, resume_rx) = channel::<()>();
    let (done_tx, done_rx) = channel();
    let importer = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || {
            let items = (0..2500u64).map(move |i| {
                if i == RUN {
                    // The first run is applied, the second not yet started.
                    paused_tx.send(()).expect("the test listens");
                    resume_rx.recv_timeout(WAIT).expect("the test resumes");
                }
                ImportItem::upsert(support::item(&format!("E-{i}"), "issue", None, i, 20))
            });
            let _ = done_tx.send(reg.import_with(items, &import_opts()));
        })
    };
    paused_rx
        .recv_timeout(WAIT)
        .expect("the import reaches its second run");

    // Each call runs on its own thread, so a held lock fails the test
    // instead of hanging it.
    let (tx, rx) = channel();
    let other = {
        let reg = Arc::clone(&reg);
        std::thread::spawn(move || {
            let _ = tx.send(reg.len());
            let _ = tx.send(usize::from(reg.get("E-1023", None).is_ok()));
            let put = reg.put(support::item("OTHER", "issue", None, 1, 20), put_opts());
            let _ = tx.send(usize::from(put.is_ok_and(|o| o.stored)));
        })
    };
    let len = rx.recv_timeout(WAIT).expect("a read gets the lock");
    assert_eq!(len, RUN as usize, "exactly the first run is stored");
    let got = rx.recv_timeout(WAIT).expect("a read gets the lock");
    assert_eq!(got, 1, "the last item of the first run is there");
    let stored = rx.recv_timeout(WAIT).expect("a write gets the lock");
    assert_eq!(stored, 1, "a put commits between two runs");
    other.join().expect("the other thread ends");

    resume_tx.send(()).expect("the import waits");
    let summary = done_rx
        .recv_timeout(WAIT)
        .expect("the import ends")
        .expect("imports");
    importer.join().expect("the import thread ends");
    assert_eq!(summary.created, 2500);
    assert_eq!(reg.len(), 2501);
}

/// An iterator that must not be asked again once it returned `None` (a
/// stand-in for one that would then panic, block, or yield more items).
struct Once<I> {
    items: I,
    ended: bool,
}

impl<I: Iterator<Item = ImportItem>> Iterator for Once<I> {
    type Item = ImportItem;

    fn next(&mut self) -> Option<ImportItem> {
        assert!(!self.ended, "next() was called after it returned None");
        let item = self.items.next();
        self.ended = item.is_none();
        item
    }
}

/// An import ends at the first `None` of its iterator, like a `for` loop.
#[test]
fn an_import_does_not_ask_its_iterator_again_after_the_end() {
    for n in [0u64, 5, 1024, 1030] {
        let reg = Registry::new(Engine::default());
        let items = Once {
            items: (0..n).map(|i| {
                ImportItem::upsert(support::item(&format!("E-{i}"), "issue", None, i, 20))
            }),
            ended: false,
        };
        let summary = reg.import_with(items, &import_opts()).expect("imports");
        assert_eq!(summary.created, n);
    }
}
