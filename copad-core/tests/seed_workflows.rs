//! Verifies that the workflow seed YAMLs at `examples/workflows/`
//! all parse under `WorkflowRegistry::load_from_dir` — Phase 22.2
//! Step 7. If a seed gains a load-bearing field that the schema
//! doesn't carry, this test fails.

use copad_core::workflow::WorkflowRegistry;
use std::path::PathBuf;

#[test]
fn all_seed_yamls_load() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples/workflows");
    let reg = WorkflowRegistry::load_from_dir(&dir);
    let ids: Vec<&str> = reg.list().iter().map(|s| s.id.as_str()).collect();
    let expected = [
        "catchup",
        "cross-review",
        "debug",
        "handoff",
        "mentor",
        "ship",
        "verify",
    ];
    for e in expected {
        assert!(ids.contains(&e), "missing seed: {e} (loaded: {ids:?})");
    }
    assert_eq!(
        reg.list().len(),
        expected.len(),
        "extra or fewer seeds than expected: {ids:?}"
    );
}

#[test]
fn ship_carries_timeout_and_fresh_session() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples/workflows");
    let reg = WorkflowRegistry::load_from_dir(&dir);
    let ship = reg.get("ship").expect("ship spec missing");
    assert_eq!(ship.timeout_secs, Some(1800));
    assert!(ship.fresh_session);
    assert!(ship.require_project);
}

#[test]
fn verify_carries_pattern_and_max_length() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples/workflows");
    let reg = WorkflowRegistry::load_from_dir(&dir);
    let verify = reg.get("verify").expect("verify spec missing");
    let url = verify
        .form_fields
        .iter()
        .find(|f| f.name == "url")
        .expect("verify.url field missing");
    assert_eq!(url.pattern.as_deref(), Some("^https?://.+"));
    assert_eq!(url.max_length, Some(500));
    assert!(url.required);
}
