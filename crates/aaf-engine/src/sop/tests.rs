#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicI64, Ordering};

use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_domain::sop::{
    SopCheckpoint, SopEventKind, SopRunStatus, SopSpec, SopStage, SopValidationError, StageKind,
    StageStatus,
};

use super::builtin::{builtin_sops, find_builtin, render_toml};
use super::runner::{
    SopError, StageDriver, StageOutcome, approve_human_gate, checkpoint_path, list_checkpoints,
    load_checkpoint, ready_stages, run_sop_persisted, save_checkpoint, send_back, step_sop,
};
use super::{init_sop_file, list_all_sops, load_repo_sops, render_ascii_dag, resolve_sop};

struct FakeClock(AtomicI64);

impl FakeClock {
    fn new(start: i64) -> Self {
        Self(AtomicI64::new(start))
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

struct ScriptedDriver {
    outcomes: HashMap<String, Vec<StageOutcome>>,
    executed: Vec<String>,
}

impl ScriptedDriver {
    fn new() -> Self {
        Self {
            outcomes: HashMap::new(),
            executed: Vec::new(),
        }
    }

    fn on(mut self, stage_id: &str, outcome: StageOutcome) -> Self {
        self.outcomes
            .entry(stage_id.to_string())
            .or_default()
            .push(outcome);
        self
    }
}

impl StageDriver for ScriptedDriver {
    fn run_stage(&mut self, stage: &SopStage, _cp: &SopCheckpoint) -> StageOutcome {
        self.executed.push(stage.id.clone());
        if let Some(queue) = self.outcomes.get_mut(&stage.id)
            && !queue.is_empty()
        {
            return queue.remove(0);
        }
        match &stage.kind {
            StageKind::HumanGate { prompt } => StageOutcome::WaitHuman {
                prompt: prompt.clone(),
            },
            _ => StageOutcome::Passed {
                summary: format!("{} ok", stage.id),
                artifacts: stage.artifacts.clone(),
            },
        }
    }
}

fn make_spec(name: &str, max_reworks: u32, stages: Vec<SopStage>) -> SopSpec {
    SopSpec {
        name: name.to_string(),
        version: 1,
        description: String::new(),
        profile: "auto".to_string(),
        max_reworks,
        stages,
    }
}

fn script(id: &str, deps: &[&str], on_fail: Option<&str>) -> SopStage {
    SopStage {
        id: id.to_string(),
        title: format!("Stage {id}"),
        kind: StageKind::Script {
            command: format!("echo {id}"),
            heavy: false,
            timeout_secs: 60,
        },
        depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
        on_fail_goto: on_fail.map(str::to_string),
        artifacts: vec![format!("{id}.out")],
    }
}

fn human(id: &str, deps: &[&str]) -> SopStage {
    SopStage {
        id: id.to_string(),
        title: format!("Human {id}"),
        kind: StageKind::HumanGate {
            prompt: "Please approve".to_string(),
        },
        depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
        on_fail_goto: None,
        artifacts: Vec::new(),
    }
}

#[test]
fn all_five_builtin_sops_validate_and_round_trip_toml() {
    let sops = builtin_sops();
    let names: Vec<&str> = sops.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "backend",
            "frontend",
            "mobile-android",
            "unity-game",
            "bugfix"
        ]
    );
    for spec in &sops {
        let order = spec.validate().expect("builtin SOP must validate");
        assert_eq!(order.len(), spec.stages.len());
        let toml_text = render_toml(spec).expect("render TOML");
        let parsed: SopSpec = toml::from_str(&toml_text).expect("parse TOML back");
        assert_eq!(&parsed, spec);
        assert_eq!(parsed.validate().unwrap(), order);
        assert!(render_ascii_dag(spec).unwrap().contains(&spec.name));
    }
    let unity = find_builtin("unity-game").unwrap();
    assert!(
        unity
            .stages
            .iter()
            .any(|s| matches!(s.kind, StageKind::Script { heavy: true, .. }))
    );
    let bugfix = find_builtin("bugfix").unwrap();
    assert!(
        bugfix
            .stages
            .iter()
            .any(|s| s.id == "gate_scoped" && s.on_fail_goto.as_deref() == Some("fix"))
    );
    assert!(find_builtin("nonexistent").is_none());
}

#[test]
fn validation_rejects_cycles_missing_deps_bad_reworks_and_empty_fields() {
    assert_eq!(
        make_spec(
            "cyclic",
            0,
            vec![script("a", &["b"], None), script("b", &["a"], None)]
        )
        .validate(),
        Err(SopValidationError::DependencyCycle)
    );
    assert_eq!(
        make_spec("self", 0, vec![script("a", &["a"], None)]).validate(),
        Err(SopValidationError::SelfDependency {
            stage: "a".to_string()
        })
    );
    assert_eq!(
        make_spec("missing", 0, vec![script("a", &["ghost"], None)]).validate(),
        Err(SopValidationError::UnknownDependency {
            stage: "a".to_string(),
            dependency: "ghost".to_string()
        })
    );
    assert_eq!(
        make_spec("bad-goto", 2, vec![script("a", &[], Some("missing"))]).validate(),
        Err(SopValidationError::UnknownReworkTarget {
            stage: "a".to_string(),
            target: "missing".to_string()
        })
    );
    assert_eq!(
        make_spec(
            "zero-rework",
            0,
            vec![script("a", &[], None), script("b", &["a"], Some("a"))]
        )
        .validate(),
        Err(SopValidationError::InvalidMaxReworks { got: 0, max: 10 })
    );
    assert_eq!(
        make_spec("too-many", 11, vec![script("a", &[], None)]).validate(),
        Err(SopValidationError::MaxReworksTooHigh { got: 11, max: 10 })
    );
    assert!(matches!(
        make_spec("bad-id", 0, vec![script("Bad/Id", &[], None)]).validate(),
        Err(SopValidationError::InvalidStageId(_))
    ));
    let mut empty_cmd = script("run", &[], None);
    empty_cmd.kind = StageKind::Script {
        command: "  ".to_string(),
        heavy: false,
        timeout_secs: 10,
    };
    assert!(matches!(
        make_spec("empty-cmd", 0, vec![empty_cmd]).validate(),
        Err(SopValidationError::EmptyScriptCommand { .. })
    ));
    let empty_role = SopStage {
        id: "plan".to_string(),
        title: "Plan".to_string(),
        kind: StageKind::Agent {
            role: String::new(),
            executor: None,
            model: None,
            read_only: true,
            prompt: "hi".to_string(),
        },
        depends_on: Vec::new(),
        on_fail_goto: None,
        artifacts: Vec::new(),
    };
    assert!(matches!(
        make_spec("empty-role", 0, vec![empty_role]).validate(),
        Err(SopValidationError::EmptyAgentRole { .. })
    ));
    let unknown_toml = "name = \"x\"\nbogus = true\n[[stages]]\nid = \"g\"\ntitle = \"G\"\n[stages.kind]\ntype = \"gate\"\nmode = \"scoped\"\n";
    assert!(toml::from_str::<SopSpec>(unknown_toml).is_err());
}

#[test]
fn parallel_branches_human_gate_pause_crash_resume_and_approve() {
    let spec = make_spec(
        "parallel-release",
        2,
        vec![
            script("plan", &[], None),
            script("fe", &["plan"], None),
            script("be", &["plan"], None),
            human("signoff", &["fe", "be"]),
            SopStage {
                id: "gate".to_string(),
                title: "Final Gate".to_string(),
                kind: StageKind::Gate {
                    mode: GateMode::Full,
                },
                depends_on: vec!["signoff".to_string()],
                on_fail_goto: None,
                artifacts: vec!["receipt.json".to_string()],
            },
        ],
    );

    let tmp = tempfile::tempdir().unwrap();
    let clock = FakeClock::new(1000);
    let mut cp = SopCheckpoint::new(
        "run-parallel-1".to_string(),
        &spec,
        "/repo".to_string(),
        clock.now(),
    );

    // Parallel readiness: once `plan` is Passed, both `fe` and `be` are simultaneously ready.
    let mut probe_cp = cp.clone();
    probe_cp.stage_states.get_mut("plan").unwrap().status = StageStatus::Passed;
    let parallel_ready: Vec<&str> = ready_stages(&spec, &probe_cp)
        .unwrap()
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(parallel_ready, vec!["fe", "be"]);

    // Run until human gate pauses, persisting checkpoint after every transition.
    let mut driver1 = ScriptedDriver::new();
    let status = run_sop_persisted(&spec, &mut cp, tmp.path(), &mut driver1, &clock).unwrap();
    assert_eq!(status, SopRunStatus::WaitingHuman);
    assert_eq!(driver1.executed, vec!["plan", "fe", "be", "signoff"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(tmp.path().join("sops"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(checkpoint_path(tmp.path(), "run-parallel-1").unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    // Crash resume: reload checkpoint; stepping before approval runs nothing.
    let mut reloaded = load_checkpoint(tmp.path(), "run-parallel-1").unwrap();
    assert_eq!(reloaded.status, SopRunStatus::WaitingHuman);
    let mut noop = ScriptedDriver::new();
    assert_eq!(
        step_sop(&spec, &mut reloaded, &mut noop, &clock).unwrap(),
        SopRunStatus::WaitingHuman
    );
    assert!(noop.executed.is_empty());

    // Approve human gate (empty reviewer denied).
    assert!(approve_human_gate(&mut reloaded, "  ", &clock).is_err());
    assert_eq!(
        approve_human_gate(&mut reloaded, "alice", &clock).unwrap(),
        "signoff"
    );
    save_checkpoint(tmp.path(), &reloaded).unwrap();

    // Resume after approval: skips already-passed stages and executes only `gate`.
    let mut reloaded2 = load_checkpoint(tmp.path(), "run-parallel-1").unwrap();
    let mut driver2 = ScriptedDriver::new();
    let done = run_sop_persisted(&spec, &mut reloaded2, tmp.path(), &mut driver2, &clock).unwrap();
    assert_eq!(done, SopRunStatus::Completed);
    assert_eq!(driver2.executed, vec!["gate"]);
    assert!(
        reloaded2
            .stage_states
            .values()
            .all(|r| r.status == StageStatus::Passed)
    );
    assert!(approve_human_gate(&mut reloaded2, "bob", &clock).is_err());
    assert_eq!(list_checkpoints(tmp.path()).unwrap().len(), 1);
}

#[test]
fn automatic_rework_resets_downstream_and_stops_when_max_reworks_exhausted() {
    let spec = make_spec(
        "rework-flow",
        2,
        vec![
            script("plan", &[], None),
            script("implement", &["plan"], None),
            script("gate", &["implement"], None),
            script("review", &["gate"], Some("implement")),
            script("publish", &["review"], None),
        ],
    );
    let clock = FakeClock::new(2000);

    // One failure reworks from `implement` (skipping `plan`), then completes.
    let mut cp = SopCheckpoint::new("rw-ok".to_string(), &spec, "/repo".to_string(), clock.now());
    let mut driver = ScriptedDriver::new().on(
        "review",
        StageOutcome::Failed {
            reason: "missing negative test".to_string(),
        },
    );
    assert_eq!(
        step_sop(&spec, &mut cp, &mut driver, &clock).unwrap(),
        SopRunStatus::Completed
    );
    assert_eq!(cp.rework_count, 1);
    assert_eq!(
        driver.executed,
        vec![
            "plan",
            "implement",
            "gate",
            "review",
            "implement",
            "gate",
            "review",
            "publish"
        ]
    );
    assert!(cp.history.iter().any(|e| e.kind == SopEventKind::Rework));

    // Three failures with max_reworks=2 stops cleanly with Failed.
    let mut cp2 = SopCheckpoint::new(
        "rw-exhaust".to_string(),
        &spec,
        "/repo".to_string(),
        clock.now(),
    );
    let mut failing = ScriptedDriver::new()
        .on("review", StageOutcome::Failed { reason: "1".into() })
        .on("review", StageOutcome::Failed { reason: "2".into() })
        .on("review", StageOutcome::Failed { reason: "3".into() });
    assert_eq!(
        step_sop(&spec, &mut cp2, &mut failing, &clock).unwrap(),
        SopRunStatus::Failed
    );
    assert_eq!(cp2.rework_count, 2);
    assert_eq!(cp2.stage_states["review"].status, StageStatus::Failed);
    assert_eq!(cp2.stage_states["publish"].status, StageStatus::Pending);
}

#[test]
fn manual_send_back_from_waiting_human_resets_downstream_and_resumes() {
    let spec = make_spec(
        "sendback-flow",
        2,
        vec![
            script("plan", &[], None),
            script("implement", &["plan"], None),
            script("verify", &["implement"], None),
            human("signoff", &["verify"]),
        ],
    );
    let clock = FakeClock::new(3000);
    let mut cp = SopCheckpoint::new("sb-1".to_string(), &spec, "/repo".to_string(), clock.now());

    let mut d1 = ScriptedDriver::new();
    assert_eq!(
        step_sop(&spec, &mut cp, &mut d1, &clock).unwrap(),
        SopRunStatus::WaitingHuman
    );
    assert!(matches!(
        send_back(&spec, &mut cp, "nope", "bad", &clock),
        Err(SopError::UnknownStage(_))
    ));
    assert!(matches!(
        send_back(&spec, &mut cp, "implement", "  ", &clock),
        Err(SopError::InvalidInput(_))
    ));

    send_back(&spec, &mut cp, "implement", "needs refactor", &clock).unwrap();
    assert_eq!(cp.rework_count, 1);
    assert_eq!(cp.status, SopRunStatus::Running);
    assert_eq!(cp.stage_states["plan"].status, StageStatus::Passed);
    assert_eq!(cp.stage_states["implement"].status, StageStatus::Pending);
    assert_eq!(cp.stage_states["verify"].status, StageStatus::Pending);
    assert_eq!(cp.stage_states["signoff"].status, StageStatus::Pending);

    let mut d2 = ScriptedDriver::new();
    assert_eq!(
        step_sop(&spec, &mut cp, &mut d2, &clock).unwrap(),
        SopRunStatus::WaitingHuman
    );
    assert_eq!(d2.executed, vec!["implement", "verify", "signoff"]);

    approve_human_gate(&mut cp, "lead", &clock).unwrap();
    let mut d3 = ScriptedDriver::new();
    assert_eq!(
        step_sop(&spec, &mut cp, &mut d3, &clock).unwrap(),
        SopRunStatus::Completed
    );
    assert!(d3.executed.is_empty());
}

#[test]
fn checkpoint_rejects_path_traversal_run_ids_and_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = find_builtin("bugfix").unwrap();

    for bad_id in ["", "../escape", "a/b", "a\\b", ".hidden", "has space"] {
        assert!(matches!(
            checkpoint_path(tmp.path(), bad_id),
            Err(SopError::InvalidRunId(_))
        ));
        assert!(matches!(
            load_checkpoint(tmp.path(), bad_id),
            Err(SopError::InvalidRunId(_))
        ));
        let bad_cp = SopCheckpoint::new(bad_id.to_string(), &spec, "/repo".to_string(), 1);
        assert!(matches!(
            save_checkpoint(tmp.path(), &bad_cp),
            Err(SopError::InvalidRunId(_))
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tmp.path().join("outside.json");
        fs::write(&outside, "{}").unwrap();
        let sops_dir = tmp.path().join("sops");
        fs::create_dir_all(&sops_dir).unwrap();
        symlink(&outside, sops_dir.join("sym-run.json")).unwrap();

        let cp = SopCheckpoint::new("sym-run".to_string(), &spec, "/repo".to_string(), 1);
        assert!(matches!(
            save_checkpoint(tmp.path(), &cp),
            Err(SopError::InvalidPath(_))
        ));
        assert!(matches!(
            load_checkpoint(tmp.path(), "sym-run"),
            Err(SopError::InvalidPath(_))
        ));
        assert!(matches!(
            list_checkpoints(tmp.path()),
            Err(SopError::InvalidPath(_))
        ));
    }
}

#[test]
fn init_discovery_and_override_enforce_no_overwrite_and_no_symlinks() {
    let repo = tempfile::tempdir().unwrap();
    let path = init_sop_file(repo.path(), "backend", None, false).unwrap();
    assert!(path.is_file());
    assert!(matches!(
        init_sop_file(repo.path(), "backend", None, false),
        Err(SopError::AlreadyExists(_))
    ));
    assert!(init_sop_file(repo.path(), "backend", None, true).is_ok());
    assert!(matches!(
        init_sop_file(repo.path(), "backend", Some("../evil"), false),
        Err(SopError::InvalidInput(_))
    ));

    assert_eq!(load_repo_sops(repo.path()).unwrap().len(), 1);
    assert_eq!(
        resolve_sop(Some(repo.path()), "backend").unwrap().source,
        super::SopSource::Repo(path)
    );
    assert_eq!(list_all_sops(Some(repo.path())).unwrap().len(), 5);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let target = repo.path().join("escape.toml");
        fs::write(&target, "").unwrap();
        symlink(&target, repo.path().join(".autofactory/sops/custom.toml")).unwrap();
        assert!(matches!(
            init_sop_file(repo.path(), "bugfix", Some("custom"), true),
            Err(SopError::InvalidPath(_))
        ));
    }
}
