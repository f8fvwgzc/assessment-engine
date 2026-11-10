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

// ---------------------------------------------------------------------------
// The judgment examples.
// ---------------------------------------------------------------------------

const PARAPHRASE_FINGERPRINT: &str =
    "a5092b8fdb1115a48435979471ef040eb3f920811cb6bfdca108c7393e2d56b5";

fn read(name: &str) -> String {
    let path = format!(
        "{}/../../examples/requests/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn request(name: &str) -> AssessmentRequest {
    serde_json::from_str(&read(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn description_of(entity: &Option<Entity>) -> String {
    match entity
        .as_ref()
        .and_then(|e| e.fields.get(field::DESCRIPTION))
    {
        Some(FieldValue::Text(text)) => text.clone(),
        other => panic!("no description: {other:?}"),
    }
}

#[test]
fn paraphrase_example_asks_for_a_judgment() {
    let request = request("paraphrase.core.json");
    let change = &request.changes[0];
    let (before, after) = (
        description_of(&change.before),
        description_of(&change.after),
    );
    assert_eq!(
        assessment_core::text::fingerprint(&before, &after),
        PARAPHRASE_FINGERPRINT
    );

    let a = Engine::default().assess(&request).expect("assesses");
    assert_eq!(a.changes[0].bump, VersionBump::Major);
    assert_eq!(a.changes[0].next_version, Some(SemVer::new(2, 0, 0)));
    assert_eq!(a.judgment_requests.len(), 1);
    let asked = &a.judgment_requests[0];
    assert_eq!(asked.field, field::DESCRIPTION);
    assert_eq!(asked.fingerprint, PARAPHRASE_FINGERPRINT);
    assert_eq!(asked.reasons, [JudgmentReason::ParaphraseSuspected]);
    // The example sets `judgment_texts`, so the two texts come back.
    assert_eq!(asked.before.as_deref(), Some(before.as_str()));
    assert_eq!(asked.after.as_deref(), Some(after.as_str()));
    let judgment = a.changes[0].fields[0].judgment.as_ref().expect("flagged");
    assert_eq!(judgment.status, JudgmentStatus::Requested);
}

#[test]
fn paraphrase_judged_example_carries_the_right_fingerprint() {
    let plain = request("paraphrase.core.json");
    let judged = request("paraphrase_judged.core.json");
    // The two examples differ only in the judgment.
    let mut without = judged.clone();
    without.changes[0].judgments.clear();
    assert_eq!(without, plain);

    let change = &judged.changes[0];
    assert_eq!(change.judgments.len(), 1);
    assert_eq!(
        change.judgments[0].fingerprint,
        assessment_core::text::fingerprint(
            &description_of(&change.before),
            &description_of(&change.after)
        )
    );

    let a = Engine::default().assess(&judged).expect("assesses");
    assert_eq!(a.changes[0].bump, VersionBump::Patch);
    assert_eq!(a.changes[0].next_version, Some(SemVer::new(1, 0, 1)));
    let judgment = a.changes[0].fields[0].judgment.as_ref().expect("judged");
    assert_eq!(judgment.status, JudgmentStatus::Applied);
    assert_eq!(judgment.relation, Some(TextRelation::Equivalent));
    assert!(a.judgment_requests.is_empty());
    assert!(a.unmatched_judgments.is_empty());
    let child = a
        .impacts
        .iter()
        .find(|i| i.entity_id == "ENG-22")
        .expect("the child is affected");
    assert_eq!(child.basis_status, BasisStatus::Compatible);
}

/// The proto3-JSON twins hold the same texts and the same fingerprint as
/// the core examples. (No proto decoder here: they are read as plain JSON.)
#[test]
fn proto_json_paraphrase_examples_match_the_core_ones() {
    let core = request("paraphrase.core.json");
    let (before, after) = (
        description_of(&core.changes[0].before),
        description_of(&core.changes[0].after),
    );
    for name in ["paraphrase.json", "paraphrase_judged.json"] {
        let value: serde_json::Value =
            serde_json::from_str(&read(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let change = &value["changes"][0];
        let text = |side: &str| change[side]["fields"]["description"]["text"].as_str();
        assert_eq!(text("before"), Some(before.as_str()), "{name}");
        assert_eq!(text("after"), Some(after.as_str()), "{name}");
        assert_eq!(value["options"]["judgmentTexts"], true, "{name}");
    }
    let judged: serde_json::Value =
        serde_json::from_str(&read("paraphrase_judged.json")).expect("json");
    let judgment = &judged["changes"][0]["judgments"][0];
    assert_eq!(judgment["fingerprint"], PARAPHRASE_FINGERPRINT);
    assert_eq!(judgment["relation"], "TEXT_RELATION_EQUIVALENT");
    assert_eq!(judgment["field"], "description");
    let plain: serde_json::Value = serde_json::from_str(&read("paraphrase.json")).expect("json");
    assert!(plain["changes"][0].get("judgments").is_none());
}
