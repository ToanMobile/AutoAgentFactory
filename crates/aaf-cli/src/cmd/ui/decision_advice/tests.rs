#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

use super::classify::{
    DecisionKind, classify_item, compute_input_hash, merge_and_harden_items,
    try_deterministic_fast_path,
};
use super::runner::extract_first_json_object;
use super::store::{
    ADVICE_SCHEMA_VERSION, AdviceItem, AdviceOption, PENDING_STALE_SECS, StoredAdvice, advice_path,
    read_valid_advice, write_advice_atomic,
};

fn sample_task(id: &str) -> Task {
    let mut t = Task::new_root(
        id,
        "/tmp/repo",
        "Test task",
        ImmutableSpec {
            description: "Desc".into(),
            acceptance: vec!["AC-1".into()],
        },
        Priority::P2,
        100,
    );
    t.state = TaskState::Blocked;
    t
}

#[test]
fn classifier_handles_real_db_misfires_and_strict_human_blockers() {
    assert_eq!(
        classify_item(
            "BLOCKER: what is C06? Please paste its text (symptom, file, expected behaviour)."
        ),
        DecisionKind::NeedsInput
    );
    assert_eq!(
        classify_item("Cần chủ repo cấp quyền đọc Supabase để kiểm tra telemetry (#451)"),
        DecisionKind::NeedsAccess
    );
    assert_eq!(
        classify_item("Cần chủ repo ra quyết định, cung cấp secret và nghe nhãn gộp (#452)"),
        DecisionKind::NeedsAccess
    );
    assert_eq!(
        classify_item("Cần người lái & xe thật (IHU629G): một buổi đo gộp toàn bộ thẻ QA (#450)"),
        DecisionKind::NeedsDevice
    );
    // Must NOT misclassify sandbox/pre-commit questions as device/access or scope
    assert_eq!(
        classify_item(
            "Lượt chạy kết thúc không commit (ví dụ keychain không đọc được trong sandbox) thì aaf đặt thẻ trạng thái gì?"
        ),
        DecisionKind::NeedsAccess
    );
    assert_eq!(
        classify_item(
            "A10 cần thêm API ở core/common, nằm ngoài scope paths, mang cả API vào PR này hay hoãn A10?"
        ),
        DecisionKind::ScopeChoice
    );
    assert_eq!(
        classify_item(
            "V04 (#567) được ghi là đã có trên main. Lúc thực thi cần grep marker trên base để xác nhận."
        ),
        DecisionKind::VerifyOnBase
    );
}

#[test]
fn deterministic_fast_path_fires_for_access_and_device_only() {
    let items = vec![
        "Cần chủ repo cấp quyền đọc Supabase (#451)".to_string(),
        "Cần người lái & xe thật (IHU629G) đo VHAL".to_string(),
    ];
    let fast = try_deterministic_fast_path(&items).expect("should use zero-token fast path");
    assert_eq!(fast.len(), 2);
    assert!(fast[0].requires_human_input);
    assert!(fast[1].requires_human_input);
    assert!(
        fast.iter()
            .all(|i| i.options.iter().all(|o| !o.recommended))
    );

    let mixed = vec![
        "A10 mang vào PR này hay hoãn A10?".to_string(),
        "Cần cấp quyền Supabase".to_string(),
    ];
    assert!(try_deterministic_fast_path(&mixed).is_none());
}

#[test]
fn monotonic_hardening_blocks_model_downgrade_and_unjustified_ac_shortcuts() {
    let raw = vec![
        "Cần chủ repo cấp quyền đọc Supabase (#451)".to_string(),
        "Chủ repo có đồng ý PR này chỉ tích AC-4/5/6 và để thẻ mở?".to_string(),
    ];
    let hostile_model = vec![
        AdviceItem {
            question: "Supabase".into(),
            kind: "scope_choice".into(),
            requires_human_input: false,
            hint: "Just proceed without access".into(),
            options: vec![AdviceOption {
                num: "1".into(),
                title: "Proceed now".into(),
                desc: "".into(),
                recommended: true,
            }],
        },
        AdviceItem {
            question: "AC shortcut".into(),
            kind: "scope_choice".into(),
            requires_human_input: false,
            hint: "Faster to merge today".into(),
            options: vec![
                AdviceOption {
                    num: "1".into(),
                    title: "Chỉ tích AC-4/5/6 và bỏ qua phần còn lại".into(),
                    desc: "".into(),
                    recommended: true,
                },
                AdviceOption {
                    num: "2".into(),
                    title: "Làm đầy đủ tất cả AC".into(),
                    desc: "".into(),
                    recommended: false,
                },
            ],
        },
    ];

    let merged = merge_and_harden_items(&raw, hostile_model);
    assert_eq!(merged[0].kind, "needs_access");
    assert!(merged[0].requires_human_input);
    assert!(merged[0].options.iter().all(|o| !o.recommended));

    // Shortcut without file evidence must lose its recommended flag and require explicit input.
    assert!(merged[1].requires_human_input);
    assert!(merged[1].options.iter().all(|o| !o.recommended));
}

#[test]
fn sidecar_store_prevents_path_traversal_and_enforces_hash_and_stale_pending() {
    let dir = tempfile::tempdir().unwrap();
    let evil_id = "../../etc/passwd";
    let p = advice_path(dir.path(), evil_id);
    assert_eq!(p.parent().unwrap(), dir.path().join("decision_advice"));

    let task = sample_task("t-safe1");
    let hash = compute_input_hash(&task, "blocked", &["q1".into()]);
    let rec = StoredAdvice {
        schema_version: ADVICE_SCHEMA_VERSION,
        task_id: "t-safe1".into(),
        input_hash: hash.clone(),
        status: "pending".into(),
        model: String::new(),
        created_at: 1000,
        error: None,
        items: Vec::new(),
    };
    write_advice_atomic(dir.path(), &rec).unwrap();

    // Wrong hash => cache miss
    assert!(read_valid_advice(dir.path(), "t-safe1", "other-hash", 1010).is_none());
    // Fresh pending => pending
    let fresh = read_valid_advice(dir.path(), "t-safe1", &hash, 1020).unwrap();
    assert_eq!(fresh.status, "pending");
    // Stale pending => converted to timeout error
    let stale =
        read_valid_advice(dir.path(), "t-safe1", &hash, 1000 + PENDING_STALE_SECS + 1).unwrap();
    assert_eq!(stale.status, "error");
    assert!(stale.error.unwrap().contains("timed out"));
}

#[test]
fn extract_first_json_object_handles_fences_and_braces_in_strings() {
    let raw = "Here is JSON:\n```json\n{\"items\":[{\"question\":\"x {y}\",\"hint\":\"ok\"}]}\n```\ntrailing";
    let json = extract_first_json_object(raw).unwrap();
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(v["items"][0]["question"], "x {y}");
}
