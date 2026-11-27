use super::*;

#[test]
fn unblock_with_decision_note_steers_task_and_hydrates_plan_contract() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let hash = "178c54de9f369332dd5fef3e666ce1dafb414f416a765302c2657fbbf8459aa3";
    let mut task = aaf_domain::task::Task::new_root(
        "t-dec1",
        "/repo/a",
        "Zone 0.3 decisions",
        aaf_domain::task::ImmutableSpec {
            description: "Decide 8 items".into(),
            acceptance: vec!["User decides".into()],
        },
        aaf_domain::task::Priority::P2,
        100,
    );
    task.state = aaf_domain::task::TaskState::Blocked;
    task.blocked_reason = Some(aaf_domain::blocked::BlockedReason::NeedsHuman {
        why: "Need owner product decisions".into(),
    });
    task.context_notes.push(format!(
        "plan_approved: Summary [plan_contract_hash={hash}]"
    ));
    db.insert_task(&task).unwrap();
    db.emit(
        &aaf_domain::event::FactoryEvent::new(
            aaf_domain::event::EventKind::PlanConverged,
            serde_json::json!({
                "summary": "Summary",
                "plan_contract_hash": hash,
                "plan_contract": {
                    "summary": "Summary",
                    "files_to_change": [],
                    "invariants_and_edge_cases": [],
                    "red_green_tests": [],
                    "open_questions": [
                        "FB-A06: (1) Fix (Recommended): follow 0.4. (2) Close: keep old."
                    ],
                    "tier_assessment": "L",
                    "tier_reasons": []
                }
            })
            .to_string(),
            101,
        )
        .for_task("t-dec1"),
    )
    .unwrap();

    // Oversize unblock note must be refused BEFORE mutating state.
    let too_big = "x".repeat(2001);
    let err = actions::do_unblock(&db, "t-dec1", &too_big).unwrap_err();
    assert!(err.contains("exceeds"), "{err}");
    assert_eq!(
        db.get_task("t-dec1").unwrap().state,
        aaf_domain::task::TaskState::Blocked
    );

    // Valid unblock with decision note moves to Ready and records steer note.
    let res = actions::do_unblock(&db, "t-dec1", "FB-A06: (1) Fix").unwrap();
    assert_eq!(res["state"], "ready");
    assert_eq!(res["note"], "steer: FB-A06: (1) Fix");
    let updated = db.get_task("t-dec1").unwrap();
    assert_eq!(updated.state, aaf_domain::task::TaskState::Ready);
    assert!(
        updated
            .context_notes
            .iter()
            .any(|n| n == "steer: FB-A06: (1) Fix")
    );

    // Even after human unblock started a new epoch, plan_detail_json hydrates open_questions by hash.
    let detail = extras::plan_detail_json(&db, &updated);
    assert_eq!(detail["available"], true);
    assert_eq!(detail["hash"], hash);
    assert_eq!(
        detail["contract"]["open_questions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
