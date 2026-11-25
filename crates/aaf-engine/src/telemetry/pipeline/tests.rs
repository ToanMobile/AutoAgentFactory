#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use EventKind as E;
use aaf_domain::task::{ImmutableSpec, Priority};

fn mk_task(st: TaskState, blk: Option<BlockedReason>) -> Task {
    let sp = ImmutableSpec {
        description: "".into(),
        acceptance: vec![],
    };
    let mut t = Task::new_root("t-1", "/repo", "Pipe", sp, Priority::P1, 100);
    (t.state, t.blocked_reason) = (st, blk);
    t
}

fn mk_run(id: &str, s: i64, e: i64) -> RunRecord {
    let j = format!(
        r#"{{"id":"{id}","task_id":"t-1","account_id":"a1","executor":"codex","worktree_path":"/wt","branch":"b","status":"succeeded","exit_code":0,"tokens_in":1,"tokens_out":1,"started_at":{s},"ended_at":{e}}}"#
    );
    serde_json::from_str(&j).unwrap()
}

fn at<'a>(p: &'a TaskPipeline, id: &str) -> &'a PipelineNode {
    p.nodes.iter().find(|n| n.id == id).unwrap()
}

#[test]
fn pipeline_tracks_rework_loop_and_completion() {
    let rs = [mk_run("r-1", 110, 120), mk_run("r-2", 140, 155)];
    let es = [
        (
            E::PlanConverged,
            r#"{"summary":"ok","executor":"claude"}"#,
            105,
        ),
        (
            E::GateResult,
            r#"{"status":"failed","duration_ms":3000}"#,
            125,
        ),
        (E::ReworkRequested, r#"{"reason":"fix"}"#, 130),
        (
            E::GateResult,
            r#"{"status":"clear","duration_ms":2500}"#,
            160,
        ),
        (E::PrOpened, r#"{"pr_url":"https://x/7"}"#, 165),
        (E::MergeResult, r#"{"ok":true,"merged_head":"abc"}"#, 170),
    ]
    .map(|(k, b, ts)| FactoryEvent::new(k, b, ts).for_task("t-1"));
    let p = build_task_pipeline(&mk_task(TaskState::Done, None), &rs, &es);
    let (i, g) = (at(&p, "implement"), at(&p, "gate"));
    assert_eq!(p.nodes.len(), 8);
    assert_eq!(at(&p, "plan").status, "passed");
    assert_eq!((i.status.as_str(), i.attempts), ("passed", 2));
    assert_eq!(i.duration_ms, Some(25_000));
    assert_eq!(i.executor.as_deref(), Some("codex"));
    assert_eq!(i.account_id.as_deref(), Some("a1"));
    assert_eq!((g.status.as_str(), g.attempts), ("passed", 2));
    assert_eq!(g.duration_ms, Some(5500));
    assert_eq!(at(&p, "review").status, "passed");
    assert_eq!(at(&p, "done").status, "passed");
    let rw = p.edges.iter().find(|x| x.kind == "rework").unwrap();
    assert_eq!((rw.from.as_str(), rw.to.as_str()), ("gate", "implement"));
    assert_eq!(rw.count, 1);
}

#[test]
fn rework_rerun_deduplicates_adjacent_rework_and_state_change() {
    let rs = [mk_run("r-1", 110, 120), mk_run("r-2", 140, 155)];
    let mut e1 = FactoryEvent::new(E::GateResult, r#"{"status":"clear"}"#, 125).for_task("t-1");
    e1.id = Some(1);
    let mut e2 = FactoryEvent::new(
        E::ReworkRequested,
        r#"{"reason":"tweak","to":"ready","rerun":true}"#,
        130,
    )
    .for_task("t-1");
    e2.id = Some(2);
    let mut e3 = FactoryEvent::new(
        E::TaskStateChanged,
        r#"{"from":"review","to":"ready","why":"aaf ui rework"}"#,
        130,
    )
    .for_task("t-1");
    e3.id = Some(3);
    let p = build_task_pipeline(&mk_task(TaskState::Running, None), &rs, &[e1, e2, e3]);
    let rws: Vec<_> = p.edges.iter().filter(|x| x.kind == "rework").collect();
    assert_eq!(rws.len(), 1);
    assert_eq!(
        (rws[0].from.as_str(), rws[0].to.as_str()),
        ("review", "implement")
    );
    assert_eq!(rws[0].count, 1, "must not double-count rework --rerun");
}

#[test]
fn reentering_gating_after_prior_cycle_keeps_gate_running() {
    let rs = [mk_run("r-1", 110, 120), mk_run("r-2", 140, 155)];
    let es = [
        (E::GateResult, r#"{"status":"clear"}"#, 125),
        (E::ProofRecorded, r#"{"ok":true,"summary":"ok"}"#, 126),
        (
            E::CrossAudit,
            r#"{"verdict":"pass","executor":"codex"}"#,
            127,
        ),
        (E::ReworkRequested, r#"{"reason":"fix"}"#, 130),
        (
            E::TaskStateChanged,
            r#"{"from":"review","to":"ready","why":"aaf ui rework"}"#,
            130,
        ),
        (
            E::TaskStateChanged,
            r#"{"from":"running","to":"gating","why":"worker done"}"#,
            156,
        ),
    ]
    .map(|(k, b, ts)| FactoryEvent::new(k, b, ts).for_task("t-1"));
    let p = build_task_pipeline(&mk_task(TaskState::Gating, None), &rs, &es);
    assert_eq!(
        at(&p, "gate").status,
        "running",
        "Gate node must stay running when re-entering Gating after prior Proof/Audit"
    );
}

#[test]
fn blocked_task_waiting_human_sets_waiting_human_status() {
    let blk = Some(BlockedReason::NeedsHuman {
        why: "approve".into(),
    });
    let es = [FactoryEvent::new(E::GateResult, r#"{"status":"clear"}"#, 125).for_task("t-1")];
    let p = build_task_pipeline(
        &mk_task(TaskState::Blocked, blk),
        &[mk_run("r-1", 110, 120)],
        &es,
    );
    assert_eq!(at(&p, "gate").status, "passed");
    assert_eq!(at(&p, "review").status, "waiting_human");
    assert_eq!(at(&p, "review").detail.as_deref(), Some("approve"));
    assert_eq!(at(&p, "done").status, "pending");
}

#[test]
fn list_sop_runs_reads_checkpoints_respects_limit_and_skips_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list_sop_runs(dir.path(), 10).is_empty());
    let sops = dir.path().join("sops");
    fs::create_dir_all(&sops).unwrap();
    let cp = |id: &str, nm: &str, st: &str, ts: i64| {
        format!(
            r#"{{"run_id":"{id}","sop_name":"{nm}","repo_root":"/r","status":"{st}","rework_count":0,"stage_states":{{"plan":{{"status":"passed","attempts":1,"summary":"ok","artifacts":["a.md"]}}}},"history":[],"created_at":10,"updated_at":{ts}}}"#
        )
    };
    let put = |f: &str, b: String| fs::write(sops.join(f), b).unwrap();
    put("old.json", cp("run-old", "rel", "completed", 200));
    put("mid.json", cp("run-mid", "qa", "running", 350));
    put("new.json", cp("run-new", "dep", "waiting_human", 500));
    put("bad.json", "{bad".into());
    put(".dot.json", "{}".into());
    #[cfg(unix)]
    {
        let ext = dir.path().join("ext.json");
        fs::write(&ext, cp("sym", "s", "running", 999)).unwrap();
        std::os::unix::fs::symlink(&ext, sops.join("sym.json")).unwrap();
    }
    let runs = list_sop_runs(dir.path(), 10);
    assert_eq!((runs.len(), runs[0].run_id.as_str()), (3, "run-new"));
    assert_eq!(runs[0].status, "waiting_human");
    assert_eq!(runs[0].stages[0].artifacts, vec!["a.md"]);
    assert_eq!(runs[1].run_id, "run-mid");
    assert_eq!(runs[2].run_id, "run-old");
    let limited = list_sop_runs(dir.path(), 2);
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].run_id, "run-new");
    assert_eq!(limited[1].run_id, "run-mid");
    assert!(list_sop_runs(dir.path(), 0).is_empty());
}
