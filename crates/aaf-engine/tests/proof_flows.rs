#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proof_flows_helpers;

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{GlobalConfig, GlobalProofConfig, ProofConfig, ProofFlow};
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::proof::{
    AssertionResult, FlowResult, PROOF_SCHEMA_VERSION, PROOF_SCHEMA_VERSION_V2, ProofRecord,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::git::MergeStrategy;
use aaf_engine::handover::build_handover_from_parts;
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};
use aaf_engine::proof::{
    HOST_PROOF_FLOWS_DEVICE, HOST_PROOF_FLOWS_TOOL, ProofInput, load_base_flows, record_proof,
    run_flows_and_record_proof_with_env, run_single_flow_with_timeout, show_proof, verify_proof,
};
use proof_flows_helpers::{
    FakeGh, FixedClock, base_gate_cfg, calm_machine, git, init_base_repo, new_ws, pass_script,
    run_gate_opts, valid_ui_png,
};

#[test]
fn flow_is_read_from_base_not_worktree() {
    let (_tmp, root) = new_ws();
    let (repo, data, png) = (root.join("repo"), root.join("data"), root.join("good.png"));
    fs::write(&png, valid_ui_png()).unwrap();
    let script_a = pass_script(&png, "base_ok", "from base");
    let base_toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"flow_a\"\nscript = \"scripts/flow_a.sh\"\n";
    let base_sha = init_base_repo(&repo, base_toml, &[("scripts/flow_a.sh", &script_a)]);

    git(&repo, &["checkout", "-q", "-b", "feat/work"]);
    fs::write(repo.join("src/app.txt"), "v2\n").unwrap();
    fs::write(
        repo.join("scripts/flow_a.sh"),
        "printf '{\"assertions\":[{\"name\":\"tampered\",\"passed\":false}]}' > \"$AAF_PROOF_ASSERTIONS\"\nexit 1\n",
    )
    .unwrap();

    let mut cfg = base_gate_cfg();
    cfg.proof.flows = vec![ProofFlow {
        name: "flow_b".into(),
        script: "scripts/flow_b.sh".into(),
        ..ProofFlow::default()
    }];
    let run = run_gate_opts(&data, &repo, &cfg, Some(&[]));
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    let rec = show_proof(&repo, &data, Some(&run.receipt.tree_fp)).unwrap();
    assert_eq!(rec.schema_version, PROOF_SCHEMA_VERSION_V2);
    assert_eq!(rec.base_sha.as_deref(), Some(base_sha.as_str()));
    assert_eq!(rec.source_tool, HOST_PROOF_FLOWS_TOOL);
    assert_eq!(rec.device_serial, HOST_PROOF_FLOWS_DEVICE);
    assert_eq!(rec.flows.len(), 1);
    assert_eq!(rec.flows[0].name, "flow_a");
    assert!(rec.flows[0].passed);
}

#[test]
fn failing_assertion_blocks_proof_with_scrubbed_reason() {
    let (_tmp, root) = new_ws();
    let (repo, data) = (root.join("repo"), root.join("data"));
    let secret_val = "s3cr3t-flow-token-99887766";
    let script = r#"printf '{"assertions":[{"name":"auth_check","passed":false,"detail":"failed with %s"}]}' "$FOO_TOKEN" > "$AAF_PROOF_ASSERTIONS""#;
    let base_toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"auth_flow\"\nscript = \"scripts/auth.sh\"\nsecrets = [\"FOO_TOKEN\"]\n";
    let base_sha = init_base_repo(&repo, base_toml, &[("scripts/auth.sh", script)]);
    let fp = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    let flows = load_base_flows(&repo, &base_sha).unwrap();
    let allow = vec!["FOO_TOKEN".to_string()];
    let env_map: HashMap<String, String> = [
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("FOO_TOKEN".into(), secret_val.into()),
    ]
    .into_iter()
    .collect();
    let pcfg = ProofConfig {
        required: true,
        ..ProofConfig::default()
    };
    let err = run_flows_and_record_proof_with_env(
        &repo,
        &data,
        &base_sha,
        &pcfg,
        &flows,
        Some(&allow),
        &fp,
        &FixedClock(1000),
        &|k| env_map.get(k).cloned(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("proof flow auth_flow failed") && err.contains("auth_check"),
        "{err}"
    );
    assert!(!err.contains(secret_val), "secret leaked: {err}");

    let manifest_val = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script2 = r#"printf '{"assertions":[{"name":"gate_check","passed":false,"detail":"bad %s"}]}' "$CARGO_MANIFEST_DIR" > "$AAF_PROOF_ASSERTIONS""#;
    let (repo2, data2) = (root.join("repo2"), root.join("data2"));
    let toml2 = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"gate_flow\"\nscript = \"scripts/g.sh\"\nsecrets = [\"CARGO_MANIFEST_DIR\"]\n";
    init_base_repo(&repo2, toml2, &[("scripts/g.sh", script2)]);
    let run = run_gate_opts(
        &data2,
        &repo2,
        &base_gate_cfg(),
        Some(&["CARGO_MANIFEST_DIR".into()]),
    );
    assert_eq!(run.receipt.status, GateStatus::Blocked);
    let joined = run.receipt.reasons.join(" | ");
    assert!(
        joined.contains("proof flow gate_flow failed") && !joined.contains(&manifest_val),
        "{joined}"
    );
}

#[test]
fn secret_not_in_tier1_allowlist_and_dangerous_or_empty_secret_refused() {
    let (_tmp, root) = new_ws();
    let (repo, data, marker) = (
        root.join("repo"),
        root.join("data"),
        root.join("ran.marker"),
    );
    let script = format!("touch '{}'\n", marker.display());
    let base_toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"sec_flow\"\nscript = \"scripts/sec.sh\"\nsecrets = [\"NEEDED_TOKEN\"]\n";
    let base_sha = init_base_repo(&repo, base_toml, &[("scripts/sec.sh", &script)]);

    let run = run_gate_opts(&data, &repo, &base_gate_cfg(), Some(&[]));
    assert_eq!(run.receipt.status, GateStatus::Blocked);
    assert!(
        run.receipt
            .reasons
            .join(" | ")
            .contains("not allowlisted in the global config")
    );
    assert!(!marker.exists());

    let run_ld = run_gate_opts(
        &data,
        &repo,
        &base_gate_cfg(),
        Some(&["LD_PRELOAD".into(), "NEEDED_TOKEN".into()]),
    );
    assert_eq!(run_ld.receipt.status, GateStatus::Blocked);
    assert!(!marker.exists());

    let fp = aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    let flows = load_base_flows(&repo, &base_sha).unwrap();
    let allow = vec!["NEEDED_TOKEN".to_string()];
    let env_map: HashMap<String, String> = [
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("NEEDED_TOKEN".into(), String::new()),
    ]
    .into_iter()
    .collect();
    let pcfg = ProofConfig {
        required: true,
        ..ProofConfig::default()
    };
    let err = run_flows_and_record_proof_with_env(
        &repo,
        &data,
        &base_sha,
        &pcfg,
        &flows,
        Some(&allow),
        &fp,
        &FixedClock(1000),
        &|k| env_map.get(k).cloned(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("NEEDED_TOKEN"), "{err}");
    assert!(!marker.exists());
}

#[test]
fn allowlisted_secret_reaches_flow_and_other_env_does_not() {
    let (_tmp, root) = new_ws();
    let (repo, data, png) = (root.join("repo"), root.join("data"), root.join("ok.png"));
    fs::write(&png, valid_ui_png()).unwrap();
    let script = format!(
        "if [ \"${{CARGO_PKG_NAME:-}}\" = \"aaf-engine\" ] && [ -z \"${{CARGO_MANIFEST_DIR:-}}\" ]; then\n  cp '{}' \"$AAF_PROOF_OUT\"\n  printf '%s' '{{\"assertions\":[{{\"name\":\"env_isolated\",\"passed\":true}}]}}' > \"$AAF_PROOF_ASSERTIONS\"\nelse\n  printf '%s' '{{\"assertions\":[{{\"name\":\"env_isolated\",\"passed\":false,\"detail\":\"env leak\"}}]}}' > \"$AAF_PROOF_ASSERTIONS\"\nfi\n",
        png.display()
    );
    let base_toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"env_flow\"\nscript = \"scripts/env.sh\"\nsecrets = [\"CARGO_PKG_NAME\"]\n";
    init_base_repo(&repo, base_toml, &[("scripts/env.sh", &script)]);
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    let run = run_gate_opts(
        &data,
        &repo,
        &base_gate_cfg(),
        Some(&["CARGO_PKG_NAME".into()]),
    );
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
}

#[test]
fn flow_cannot_read_receipt_key() {
    let (_tmp, root) = new_ws();
    let (repo, data, png, leak) = (
        root.join("repo"),
        root.join("data"),
        root.join("ok.png"),
        root.join("leak.key"),
    );
    fs::create_dir_all(&data).unwrap();
    let _ = aaf_fp::load_or_create_key(&data).unwrap();
    fs::write(&png, valid_ui_png()).unwrap();
    let script = format!(
        "cp '{}/receipt.key' '{}' 2>/dev/null || true\n{}",
        data.display(),
        leak.display(),
        pass_script(&png, "confined", "")
    );
    let base_toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"confine_flow\"\nscript = \"scripts/confine.sh\"\n";
    init_base_repo(&repo, base_toml, &[("scripts/confine.sh", &script)]);
    let run = run_gate_opts(&data, &repo, &base_gate_cfg(), Some(&[]));
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    assert!(!leak.exists(), "proof flow leaked receipt.key");
}

#[test]
fn missing_or_empty_assertions_fail() {
    for (label, body) in [
        ("missing", "true\n"),
        ("malformed", "printf '{bad' > \"$AAF_PROOF_ASSERTIONS\"\n"),
        (
            "empty",
            "printf '{\"assertions\":[]}' > \"$AAF_PROOF_ASSERTIONS\"\n",
        ),
    ] {
        let (_tmp, root) = new_ws();
        let (repo, data) = (root.join("repo"), root.join("data"));
        let toml = format!(
            "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"{label}\"\nscript = \"scripts/a.sh\"\n"
        );
        init_base_repo(&repo, &toml, &[("scripts/a.sh", body)]);
        let run = run_gate_opts(&data, &repo, &base_gate_cfg(), Some(&[]));
        assert_eq!(run.receipt.status, GateStatus::Blocked, "case {label}");
        assert!(
            run.receipt
                .reasons
                .join(" | ")
                .contains("no assertions reported"),
            "case {label}"
        );
    }
}

#[test]
fn record_v2_has_base_sha_and_flows_and_v1_records_still_verify() {
    let (_tmp, root) = new_ws();
    let (repo, data, png) = (root.join("repo"), root.join("data"), root.join("ok.png"));
    fs::write(&png, valid_ui_png()).unwrap();
    let script = pass_script(&png, "render", "ok");
    let toml = "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"ui_check\"\nscript = \"scripts/ui.sh\"\n";
    let base_sha = init_base_repo(
        &repo,
        toml,
        &[
            ("scripts/ui.sh", &script),
            (".adb-allowlist", "emulator-5554\n"),
        ],
    );

    let cfg = base_gate_cfg();
    let run = run_gate_opts(&data, &repo, &cfg, Some(&[]));
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    let v2 = verify_proof(&repo, &data, &cfg.proof, Some(&run.receipt.tree_fp)).unwrap();
    assert_eq!(v2.schema_version, PROOF_SCHEMA_VERSION_V2);
    assert_eq!(v2.base_sha.as_deref(), Some(base_sha.as_str()));
    assert_eq!(v2.flows[0].assertions[0].name, "render");
    assert!(run_gate_opts(&data, &repo, &cfg, Some(&[])).reused);

    let mut bad_v2 = v2;
    bad_v2.flows.clear();
    aaf_fp::write_proof(&data, &bad_v2).unwrap();
    assert!(verify_proof(&repo, &data, &cfg.proof, Some(&run.receipt.tree_fp)).is_err());

    let data_v1 = root.join("data_v1");
    let saved_v1 = record_proof(
        &repo,
        &data_v1,
        &cfg.proof,
        &ProofInput {
            image_path: png,
            source_tool: "mobile-mcp:mobile_save_screenshot".into(),
            device_serial: "emulator-5554".into(),
            task_id: None,
            now: 500,
        },
    )
    .unwrap();
    assert_eq!(saved_v1.record.schema_version, PROOF_SCHEMA_VERSION);
    assert_eq!(
        verify_proof(&repo, &data_v1, &cfg.proof, None)
            .unwrap()
            .schema_version,
        PROOF_SCHEMA_VERSION
    );
}

#[test]
fn script_symlink_or_parent_escape_in_base_is_rejected() {
    let (_t1, r1) = new_ws();
    let repo1 = r1.join("repo");
    let sha1 = init_base_repo(
        &repo1,
        "[proof]\nrequired = true\n[[proof.flows]]\nname = \"esc\"\nscript = \"../escape.sh\"\n",
        &[],
    );
    assert!(load_base_flows(&repo1, &sha1).is_err());

    #[cfg(unix)]
    {
        let (_t2, r2) = new_ws();
        let (repo2, data2) = (r2.join("repo"), r2.join("data"));
        fs::create_dir_all(repo2.join(".autofactory")).unwrap();
        fs::create_dir_all(repo2.join("scripts")).unwrap();
        git(&repo2, &["init", "-q", "-b", "main"]);
        fs::write(
            repo2.join(".autofactory/config.toml"),
            "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"sym\"\nscript = \"scripts/link.sh\"\n",
        ).unwrap();
        std::os::unix::fs::symlink("../outside.sh", repo2.join("scripts/link.sh")).unwrap();
        git(&repo2, &["add", "-A"]);
        git(&repo2, &["commit", "-q", "-m", "symlink script"]);
        let run = run_gate_opts(&data2, &repo2, &base_gate_cfg(), Some(&[]));
        assert_eq!(run.receipt.status, GateStatus::Blocked);
        assert!(
            run.receipt
                .reasons
                .join(" | ")
                .contains("regular file blob")
        );
    }
}

#[test]
fn timeout_is_enforced_with_short_duration() {
    let (_tmp, root) = new_ws();
    let (repo, data) = (root.join("repo"), root.join("data"));
    fs::create_dir_all(&data).unwrap();
    let base_sha = init_base_repo(
        &repo,
        "[proof]\nrequired = true\n[[proof.flows]]\nname = \"slow\"\nscript = \"scripts/slow.sh\"\n",
        &[("scripts/slow.sh", "sleep 10\n")],
    );
    let flows = load_base_flows(&repo, &base_sha).unwrap();
    let err = run_single_flow_with_timeout(
        &repo,
        &data,
        &base_sha,
        &ProofConfig::default(),
        &flows[0],
        &[],
        Duration::from_secs(1),
        &|k| std::env::var(k).ok(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("timed out"), "{err}");
}

#[test]
fn pr_merge_blocks_when_proof_needs_human_and_handover_marks_missing() {
    let (_tmp, root) = new_ws();
    let (bare, work, data, png) = (
        root.join("bare.git"),
        root.join("work"),
        root.join("data"),
        root.join("ok.png"),
    );
    fs::write(&png, valid_ui_png()).unwrap();
    git(
        &root,
        &["init", "--bare", "-q", "-b", "main", bare.to_str().unwrap()],
    );
    let script = pass_script(&png, "visual", "");
    let toml = "[project]\nname = \"demo\"\nprofile = \"rust\"\n[commands]\ntest_scoped = \"true\"\n[gate]\nbase_ref = \"origin/main\"\n[proof]\nrequired = true\n[[proof.flows]]\nname = \"human_visual\"\nscript = \"scripts/visual.sh\"\nneeds_human = true\n";
    init_base_repo(&work, toml, &[("scripts/visual.sh", &script)]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    git(&work, &["checkout", "-q", "-b", "feat/ui"]);
    fs::write(work.join("src/app.txt"), "v2\n").unwrap();
    git(&work, &["add", "src/app.txt"]);
    git(&work, &["commit", "-q", "-m", "ui change"]);
    let head_sha = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["push", "-q", "-u", "origin", "feat/ui"]);
    git(&work, &["checkout", "-q", "main"]);

    let gh = FakeGh {
        head: head_sha.clone(),
        merges: AtomicUsize::new(0),
    };
    let req = PrMergeRequest {
        repo_root: work,
        data_dir: data,
        pr_number: 7,
        expect_head: head_sha,
        remote: "origin".into(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: 0,
        require_checks: false,
        lock_timeout: Duration::from_secs(5),
        dry_run: false,
        machine: calm_machine(),
        global_config: Some(GlobalConfig {
            proof: GlobalProofConfig::default(),
            ..GlobalConfig::default()
        }),
    };
    let PrMergeOutcome::Blocked { code, reason, .. } =
        merge_pr(&req, &gh, &FixedClock(1000), &|_| {}).unwrap()
    else {
        panic!("expected Blocked");
    };
    assert_eq!(code, "proof_needs_human");
    assert!(
        reason.contains("proof flow human_visual needs human review"),
        "{reason}"
    );
    assert_eq!(gh.merges.load(Ordering::SeqCst), 0);

    let mut task = Task::new_root(
        "t1",
        "/repo",
        "ui task",
        ImmutableSpec {
            description: "d".into(),
            acceptance: vec!["a".into()],
        },
        Priority::P2,
        1,
    );
    task.state = TaskState::Review;
    let proof = ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION_V2,
        repo_root: "/repo".into(),
        tree_fp: "git-tree:abc".into(),
        image_path: "/tmp/ok.png".into(),
        image_sha256: "a".repeat(64),
        width: 1080,
        height: 1920,
        byte_len: 9000,
        source_tool: HOST_PROOF_FLOWS_TOOL.into(),
        device_serial: HOST_PROOF_FLOWS_DEVICE.into(),
        task_id: None,
        recorded_at: 1000,
        base_sha: Some("deadbeef".into()),
        flows: vec![FlowResult {
            name: "human_visual".into(),
            passed: true,
            needs_human: true,
            assertions: vec![AssertionResult {
                name: "visual".into(),
                passed: true,
                detail: String::new(),
            }],
            duration_ms: 12,
        }],
    };
    let h = build_handover_from_parts(
        &task,
        &[],
        None,
        None,
        Some(&proof),
        true,
        false,
        95,
        90,
        false,
    );
    assert!(
        h.missing
            .iter()
            .any(|m| m.contains("proof flow human_visual needs human review"))
    );
}

#[test]
fn flow_without_image_falls_back_to_capture_cmd_into_single_v2_record() {
    let (_tmp, root) = new_ws();
    let (repo, data, png) = (
        root.join("repo"),
        root.join("data"),
        root.join("fallback.png"),
    );
    fs::write(&png, valid_ui_png()).unwrap();
    let script = "printf '%s' '{\"assertions\":[{\"name\":\"api_ok\",\"passed\":true}]}' > \"$AAF_PROOF_ASSERTIONS\"\n";
    let capture_cmd = format!("cp '{}' \"$AAF_PROOF_OUT\"", png.display());
    let toml = format!(
        "[commands]\ntest_scoped = \"true\"\n[proof]\nrequired = true\ncapture_cmd = {capture_cmd:?}\n[[proof.flows]]\nname = \"api\"\nscript = \"scripts/api.sh\"\n"
    );
    let base_sha = init_base_repo(&repo, &toml, &[("scripts/api.sh", script)]);
    let cfg = base_gate_cfg();
    let run = run_gate_opts(&data, &repo, &cfg, Some(&[]));
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    let rec = verify_proof(&repo, &data, &cfg.proof, Some(&run.receipt.tree_fp)).unwrap();
    assert_eq!(rec.schema_version, PROOF_SCHEMA_VERSION_V2);
    assert_eq!(rec.base_sha.as_deref(), Some(base_sha.as_str()));
    assert_eq!(rec.flows[0].name, "api");
}
