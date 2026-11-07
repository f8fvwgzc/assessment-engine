//! The JSON request examples shipped in `examples/requests` stay valid.

use assessment_core::prelude::*;

fn example() -> AssessmentRequest {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/requests/parent_rewrite.core.json"
    );
    let json = std::fs::read_to_string(path).expect("the example exists");
    serde_json::from_str(&json).expect("the example is a valid request")
}

#[test]
fn parent_rewrite_example_parses_and_assesses() {
    let a = Engine::default()
        .assess(&example())
        .expect("the example assesses");
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    assert!(a.level >= ImpactLevel::Medium);
    assert!(a.score >= a.severity);
    if a.recommendation >= Recommendation::PartialReplan {
        assert_eq!(a.replan_root.as_deref(), Some("ENG-12"), "{}", a.summary);
    }
    // The children pinned 1.2.0; this change is what made the pins incompatible.
    for i in a.impacts.iter().filter(|i| !i.changed) {
        if i.basis_status == BasisStatus::Incompatible {
            assert!(i.basis_introduced, "{}", i.entity_id);
        }
    }
    assert!(a.changes.iter().all(|c| !c.propagation_truncated));
    let round = serde_json::to_string(&a).expect("serializes");
    let back: Assessment = serde_json::from_str(&round).expect("deserializes");
    assert_eq!(back, a);
}

#[test]
fn assessments_without_the_newer_fields_still_deserialize() {
    let a = Engine::default()
        .assess(&example())
        .expect("the example assesses");
    let mut value = serde_json::to_value(&a).expect("serializes");
    for impact in value["impacts"].as_array_mut().expect("impacts") {
        impact
            .as_object_mut()
            .expect("an object")
            .remove("basis_introduced");
    }
    for change in value["changes"].as_array_mut().expect("changes") {
        change
            .as_object_mut()
            .expect("an object")
            .remove("propagation_truncated");
    }
    let older: Assessment = serde_json::from_value(value).expect("deserializes");
    assert!(older.impacts.iter().all(|i| !i.basis_introduced));
    assert_eq!(older.score, a.score);
}
