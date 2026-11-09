//! Golden outputs: the full JSON answer for fixed requests, so a change meant
//! to preserve behaviour can prove it byte for byte, and a change meant to
//! alter it shows exactly what moved.
//!
//! ```text
//! cargo test -p assessment-core --test golden                    # compare
//! GOLDEN_BLESS=1 cargo test -p assessment-core --test golden     # rewrite
//! ```
//!
//! Review the diff of `tests/golden/` before committing a re-bless, and say in
//! the CHANGELOG what moved and why. `engine_version` is blanked so a version
//! bump alone does not rewrite every file.

mod support;

use std::path::PathBuf;

use assessment_core::prelude::*;
use serde::Serialize;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn blessing() -> bool {
    std::env::var_os("GOLDEN_BLESS").is_some()
}

fn render(value: &impl Serialize) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("the value serializes");
    s.push('\n');
    s
}

fn without_version(mut a: Assessment) -> Assessment {
    a.engine_version = String::new();
    a
}

/// Compares (or, when blessing, writes) one golden file; returns a
/// description of the first difference, if any.
fn check(name: &str, actual: &str) -> Option<String> {
    let path = golden_dir().join(format!("{name}.json"));
    if blessing() {
        std::fs::create_dir_all(golden_dir()).expect("the golden directory can be created");
        std::fs::write(&path, actual).expect("the golden file can be written");
        return None;
    }
    let Ok(expected) = std::fs::read_to_string(&path) else {
        return Some(format!(
            "{name}: no golden file at {} (run with GOLDEN_BLESS=1 to create it)",
            path.display()
        ));
    };
    if expected == actual {
        return None;
    }
    let line = expected
        .lines()
        .zip(actual.lines())
        .position(|(e, a)| e != a)
        .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
    let show = |s: &str| {
        s.lines()
            .skip(line.saturating_sub(2))
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    };
    Some(format!(
        "{name}: differs from line {}\n--- expected\n{}\n--- actual\n{}",
        line + 1,
        show(&expected),
        show(actual)
    ))
}

fn assert_all(results: Vec<Option<String>>) {
    let failures: Vec<String> = results.into_iter().flatten().collect();
    assert!(
        failures.is_empty(),
        "{} golden file(s) differ:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

fn assess(req: &AssessmentRequest) -> Assessment {
    without_version(Engine::default().assess(req).expect("the request assesses"))
}

#[test]
fn text_cases_match_their_golden_files() {
    let results = support::TEXT_CASES
        .iter()
        .map(|(name, before, after)| {
            let a = assess(&support::text_case(before, after));
            check(&format!("text_{name}"), &render(&a))
        })
        .collect();
    assert_all(results);
}

#[test]
fn synthetic_graphs_match_their_golden_files() {
    let graphs: Vec<(&str, AssessmentRequest)> = vec![
        ("graph_siblings", support::siblings(200, 120, 25)),
        ("graph_cousins", support::cousins(40, 3, 120, 25)),
        ("graph_fan_out", support::fan_out(60, 120, 25)),
        ("graph_deep_chain", support::deep_chain(12, 120, 25)),
        ("graph_unrelated", support::unrelated(100, 120, 25)),
        ("graph_blocking_chain", support::blocking_chain(8, 120, 25)),
    ];
    let results = graphs
        .iter()
        .map(|(name, req)| check(name, &render(&assess(req))))
        .collect();
    assert_all(results);
}

#[test]
fn the_shipped_example_matches_its_golden_file() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/requests/parent_rewrite.core.json"
    );
    let json = std::fs::read_to_string(path).expect("the example exists");
    let req: AssessmentRequest = serde_json::from_str(&json).expect("the example parses");
    assert_all(vec![check(
        "example_parent_rewrite",
        &render(&assess(&req)),
    )]);
}

/// The registry path: a project tree stored put by put, then the parent's
/// rewrite and a sub-issue's re-estimate assessed against the stored graph.
#[test]
fn registry_puts_match_their_golden_files() {
    let registry = Registry::new(Engine::default());
    let opts = || PutOptions {
        now: Some(support::now()),
        ..PutOptions::default()
    };
    let req = support::cousins(12, 3, 120, 25);
    for e in support::load_order(&req) {
        registry.put(e, opts()).expect("the entity stores");
    }
    let outcome = |entity: Entity| {
        let mut o = registry.put(entity, opts()).expect("the change stores");
        o.assessment = without_version(o.assessment);
        render(&o)
    };
    let rewrite = req.changes[0]
        .after
        .clone()
        .expect("the change has an after");
    let rewritten = outcome(rewrite);
    let mut sub = registry.get("X-2", None).expect("X-2 is stored");
    sub.fields.insert(
        assessment_core::model::field::ESTIMATE.to_owned(),
        FieldValue::Number(8.0),
    );
    let reestimated = outcome(sub);
    let stale = render(&registry.stale(Some("X")).expect("stale lists"));
    assert_all(vec![
        check("registry_parent_rewrite", &rewritten),
        check("registry_sub_reestimate", &reestimated),
        check("registry_stale", &stale),
    ]);
}
