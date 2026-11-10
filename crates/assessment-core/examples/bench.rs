//! Timings of the engine's hot paths on deterministic synthetic graphs.
//!
//! ```text
//! cargo run --release -p assessment-core --example bench             # full table (~minutes)
//! cargo run --release -p assessment-core --example bench -- --quick  # up to ~1k entities
//! cargo run --release -p assessment-core --example bench -- --reach  # text.idf_scope = reach
//! ```
//!
//! Each row builds one request (see `tests/support/mod.rs`), times parsing its
//! JSON once, and times `Engine::assess` (median and minimum of several runs).
//! The registry rows store a project tree put by put and then time a put that
//! rewrites the changed issue, with and without the analysis cache. The
//! import rows time `Registry::import` of new entities, the first put after
//! it (which analyses the imported texts) and a put after that. Record the
//! numbers in `docs/PERFORMANCE.md`.

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use assessment_core::prelude::*;
use assessment_core::registry::ImportItem;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

/// Median and minimum of `runs` timings of `f`.
fn time(runs: usize, mut f: impl FnMut()) -> (Duration, Duration) {
    let mut samples: Vec<Duration> = (0..runs.max(1))
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    samples.sort();
    (samples[samples.len() / 2], samples[0])
}

struct Row {
    name: String,
    entities: usize,
    json_mb: f64,
    parse_ms: f64,
    median_ms: f64,
    min_ms: f64,
}

/// The engine under test: the default policy, or with `text.idf_scope = reach`.
fn engine() -> Engine {
    if std::env::args().any(|a| a == "--reach") {
        Engine::default()
            .with_policy_patch(&serde_json::json!({ "text": { "idf_scope": "reach" } }))
            .expect("a valid policy patch")
    } else {
        Engine::default()
    }
}

fn assess_row(name: &str, req: &AssessmentRequest, runs: usize) -> Row {
    let engine = engine();
    let json = serde_json::to_string(req).expect("the request serializes");
    let (parse, _) = time(1, || {
        let parsed: AssessmentRequest = serde_json::from_str(&json).expect("the request parses");
        black_box(parsed);
    });
    // Warm up once (allocator, page faults) before timing.
    black_box(engine.assess(req).expect("the request assesses"));
    let (median, min) = time(runs, || {
        black_box(engine.assess(req).expect("the request assesses"));
    });
    Row {
        name: name.to_owned(),
        entities: req.context.len() + req.changes.len(),
        json_mb: json.len() as f64 / 1_000_000.0,
        parse_ms: ms(parse),
        median_ms: ms(median),
        min_ms: ms(min),
    }
}

/// Loads a registry from `req` put by put (timed once), then times a put that
/// rewrites the changed issue, alternating between its two texts.
fn registry_row(name: String, entities: usize, median: Duration, min: Duration) -> Row {
    Row {
        name,
        entities,
        json_mb: 0.0,
        parse_ms: 0.0,
        median_ms: ms(median),
        min_ms: ms(min),
    }
}

/// Imports the graph of `siblings x{n}` as new entities, then puts rewrites
/// of the changed issue: the first one analyses the imported texts.
fn import_rows(label: &str, n: usize, runs: usize) -> Vec<Row> {
    let req = support::siblings(n, 268, 100);
    let order = support::load_order(&req);
    let entities = order.len();
    let registry = Registry::new(engine());
    let items: Vec<ImportItem> = order.into_iter().map(ImportItem::upsert).collect();
    let (import, _) = time(1, || {
        black_box(registry.import(items.clone()).expect("the graph imports"));
    });
    let opts = || PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    };
    let before = req.changes[0].before.clone().expect("a before");
    let after = req.changes[0].after.clone().expect("an after");
    let (first, _) = time(1, || {
        black_box(
            registry
                .put(after.clone(), opts())
                .expect("the change stores"),
        );
    });
    let mut flip = true;
    let (median, min) = time(runs, || {
        flip = !flip;
        let next = if flip { after.clone() } else { before.clone() };
        black_box(registry.put(next, opts()).expect("the change stores"));
    });
    vec![
        registry_row(
            format!("registry: import {label} new entities"),
            entities,
            import,
            import,
        ),
        registry_row(
            format!("registry: first put after the {label} import"),
            entities,
            first,
            first,
        ),
        registry_row(
            format!("registry: put after the {label} import"),
            entities,
            median,
            min,
        ),
    ]
}

fn registry_rows(name: &str, req: &AssessmentRequest, runs: usize) -> Vec<Row> {
    let mut rows = stored_rows(name, req, runs, true);
    rows.extend(stored_rows(name, req, runs, false).into_iter().skip(1));
    rows
}

fn stored_rows(name: &str, req: &AssessmentRequest, runs: usize, cache: bool) -> Vec<Row> {
    let registry = Registry::new(engine()).with_analysis_cache(cache);
    let opts = || PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    };
    let order = support::load_order(req);
    let entities = order.len();
    let (load, _) = time(1, || {
        for e in &order {
            registry.put(e.clone(), opts()).expect("the entity stores");
        }
    });
    let before = req.changes[0].before.clone().expect("a before");
    let after = req.changes[0].after.clone().expect("an after");
    let mut flip = false;
    let (median, min) = time(runs, || {
        flip = !flip;
        let next = if flip { after.clone() } else { before.clone() };
        black_box(registry.put(next, opts()).expect("the change stores"));
    });
    let put = if cache {
        "put on the stored graph"
    } else {
        "put on the stored graph, analysis cache off"
    };
    vec![
        registry_row(
            format!("{name}: bulk load (put by put)"),
            entities,
            load,
            load,
        ),
        registry_row(format!("{name}: {put}"), entities, median, min),
    ]
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    let scope = if std::env::args().any(|a| a == "--reach") {
        "reach"
    } else {
        "context"
    };
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut rows = Vec::new();

    let sizes: &[usize] = if quick {
        &[100, 1_000]
    } else {
        &[100, 1_000, 5_000, 10_000]
    };
    for &n in sizes {
        let runs = if n <= 1_000 { 7 } else { 3 };
        rows.push(assess_row(
            &format!("siblings x{n} (268 chars)"),
            &support::siblings(n, 268, 100),
            runs,
        ));
    }
    let big = if quick { 1_000 } else { 10_000 };
    let runs = if quick { 7 } else { 3 };
    rows.push(assess_row(
        "siblings x1000 (2000 chars)",
        &support::siblings(1_000, 2_000, 100),
        runs,
    ));
    rows.push(assess_row(
        &format!("cousins {}x3 (268 chars)", big / 4),
        &support::cousins(big / 4, 3, 268, 100),
        runs,
    ));
    rows.push(assess_row(
        &format!("fan-out tree x{big}"),
        &support::fan_out(big, 268, 100),
        runs,
    ));
    rows.push(assess_row(
        &format!("deep chain x{big}"),
        &support::deep_chain(big, 268, 100),
        runs,
    ));
    rows.push(assess_row(
        &format!("unrelated project x{big}"),
        &support::unrelated(big, 268, 100),
        runs,
    ));
    rows.push(assess_row(
        "blocking chain x200",
        &support::blocking_chain(200, 268, 100),
        7,
    ));
    let stored = if quick { 75 } else { 250 };
    rows.extend(registry_rows(
        &format!("registry cousins {stored}x3"),
        &support::cousins(stored, 3, 268, 100),
        7,
    ));
    rows.extend(import_rows("1k", 1_000, 7));
    if !quick {
        rows.extend(import_rows("10k", 10_000, 3));
    }

    println!(
        "assessment-core bench ({profile} build, idf_scope {scope}{})",
        if quick { ", quick" } else { "" }
    );
    println!();
    println!("| scenario | entities | JSON MB | parse ms | assess median ms | assess min ms |");
    println!("|---|---:|---:|---:|---:|---:|");
    for r in rows {
        println!(
            "| {} | {} | {:.2} | {:.1} | {:.1} | {:.1} |",
            r.name, r.entities, r.json_mb, r.parse_ms, r.median_ms, r.min_ms
        );
    }
}
