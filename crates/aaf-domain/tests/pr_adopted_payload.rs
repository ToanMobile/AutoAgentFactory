//! The `PrAdopted` payload: its wire shape is what `aaf board adopt` writes
//! and the adopt lane reads.

use aaf_domain::event::PrAdoptedPayload;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn json(head: &str) -> String {
    format!(
        r#"{{"pr_number":7,"url":"https://github.com/o/r/pull/7","head_sha":"{head}",
        "head_ref":"feat/x","base_ref":"main","author_login":"owner"}}"#
    )
}

#[test]
fn a_payload_without_declared_executor_reads_and_round_trips() {
    let payload: PrAdoptedPayload = serde_json::from_str(&json(HEAD)).unwrap();
    assert_eq!(payload.pr_number, 7);
    assert_eq!(payload.declared_executor, None);
    assert!(payload.head_is_valid());
    assert_eq!(payload.pinned(), HEAD);
    let again: PrAdoptedPayload =
        serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
    assert_eq!(again, payload);
}

#[test]
fn a_head_that_is_not_forty_hex_is_not_valid() {
    for head in ["", "abc", &"g".repeat(40), &"a".repeat(41)] {
        let payload: PrAdoptedPayload = serde_json::from_str(&json(head)).unwrap();
        assert!(!payload.head_is_valid(), "{head:?}");
    }
}

#[test]
fn a_payload_missing_a_field_does_not_read() {
    assert!(serde_json::from_str::<PrAdoptedPayload>(r#"{"pr_number":7}"#).is_err());
}

#[test]
fn the_pinned_head_is_lowercase() {
    let payload: PrAdoptedPayload = serde_json::from_str(&json(&HEAD.to_uppercase())).unwrap();
    assert_eq!(payload.pinned(), HEAD);
}
