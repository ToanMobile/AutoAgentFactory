#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for board-stability fixes audited by 3 agents + Claude Opus 5.5 + Grok 4.7:
//! - A1: macOS Seatbelt guarded directory `access(W_OK)` + `~/.robolectric-download-lock` + security invariants
//! - A4: `AgyRunner` `antigravity-cli` runtime subdirs vs closed credentials/config
//! - A5: `replay_debate_state` clears sticky `snap.converged` on subsequent unconverged turns
//! - A10: `.sh` wrapper scripts wrapping `./gradlew` classify as `HeavyTool::Gradle` (not `Unmeasured`)

use std::fs;
use std::path::Path;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::pipeline::PlanContract;
use aaf_engine::executors::agy::{AGY_RUNTIME_WRITABLE_SUBDIRS, GEMINI_CONFIG_CLOSED};
use aaf_engine::plan_debate::schema::canonical_plan_hash;
use aaf_engine::plan_debate::state::replay_debate_state;
use aaf_engine::throttle::heavy::{HeavyTool, heavy_tool_argv};
use serde_json::json;

fn sample_plan(summary: &str) -> PlanContract {
    serde_json::from_value(json!({
        "summary": summary,
        "tier_assessment": "M",
        "tier_reasons": ["regression test"],
        "files_to_change": [{
            "path": "src/lib.rs",
            "responsibility": "fix bug",
            "risk": "low"
        }],
        "invariants_and_edge_cases": ["no regressions"],
        "red_green_tests": [{
            "ac_ref": "AC-1",
            "test_path": "tests/t.rs",
            "red_condition": "fails before fix",
            "green_condition": "passes after fix"
        }],
        "dispositions": [],
        "open_questions": []
    }))
    .unwrap()
}

#[test]
fn a5_replay_debate_state_clears_sticky_converged_on_later_non_converged_turns() {
    let plan = sample_plan("cycle-1 plan");
    let hash = canonical_plan_hash(&plan);
    let converged_ev = FactoryEvent::new(
        EventKind::PlanConverged,
        json!({ "plan_contract": plan, "plan_contract_hash": hash }).to_string(),
        100,
    );
    let revised_author = FactoryEvent::new(
        EventKind::PlanDebateTurn,
        json!({
            "ok": true,
            "role": "author",
            "round": 1,
            "plan_contract": sample_plan("cycle-2 draft"),
        })
        .to_string(),
        200,
    );
    let failed_challenger = FactoryEvent::new(
        EventKind::PlanDebateTurn,
        json!({
            "ok": true,
            "role": "challenger",
            "round": 1,
            "converged": false,
            "open_count": 1,
            "open_issues": [],
        })
        .to_string(),
        300,
    );

    // Supply newest-first as `replay_debate_state` expects.
    let snap_after_author = replay_debate_state(&[revised_author.clone(), converged_ev.clone()]);
    assert!(
        snap_after_author.converged.is_none(),
        "a newer author turn must clear sticky converged state"
    );
    assert!(snap_after_author.pending_challenger);

    let snap_after_challenger =
        replay_debate_state(&[failed_challenger, revised_author, converged_ev]);
    assert!(
        snap_after_challenger.converged.is_none(),
        "a newer non-converged challenger turn must clear sticky converged state"
    );
}

#[test]
fn a4_agy_sandbox_opens_antigravity_cli_runtime_while_closing_credentials_and_config() {
    assert!(
        !GEMINI_CONFIG_CLOSED.contains(&"antigravity-cli"),
        "whole antigravity-cli dir must not be closed; close sensitive subpaths instead"
    );
    for required_closed in [
        "antigravity-cli/antigravity-oauth-token",
        "antigravity-cli/bin",
        "antigravity-cli/mcp",
        "antigravity-cli/settings.json",
    ] {
        assert!(
            GEMINI_CONFIG_CLOSED.contains(&required_closed),
            "missing closed path {required_closed}"
        );
    }
    for required_writable in [
        "antigravity-cli/log",
        "antigravity-cli/presence",
        "antigravity-cli/annotations",
        "antigravity-cli/cache",
    ] {
        assert!(
            AGY_RUNTIME_WRITABLE_SUBDIRS.contains(&required_writable),
            "missing runtime writable dir {required_writable}"
        );
    }
}

#[test]
fn a10_wrapper_shell_script_wrapping_gradlew_classifies_as_gradle() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("run.sh");
    fs::write(
        &script,
        "#!/bin/bash\nset -e\ncd /tmp/repo && ./gradlew --no-daemon :app:testDebugUnitTest\n",
    )
    .unwrap();
    let script_str = script.to_string_lossy().into_owned();
    assert_eq!(heavy_tool_argv(&[script_str.as_str()]), Some(HeavyTool::Gradle));
    assert_eq!(
        heavy_tool_argv(&["bash", script_str.as_str()]),
        Some(HeavyTool::Gradle)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a1_seatbelt_guarded_dir_allows_w_ok_while_blocking_rename_and_missing_symlink() {
    use aaf_domain::ports::SandboxPolicy;
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
    use std::process::Command;

    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), "gitdir: /tmp/nowhere\n").unwrap();
    let missing_closed = wt.join("local.properties");
    let robolock = tmp.path().join(".robolectric-download-lock");
    fs::write(&robolock, "").unwrap();

    let policy = SandboxPolicy {
        enabled: true,
        read_only: false,
        writable_paths: Vec::new(),
        read_denied_paths: Vec::new(),
    };
    let paths = SandboxPaths {
        worktree: Some(wt.to_string_lossy().into_owned()),
        write_denied: vec![missing_closed.to_string_lossy().into_owned()],
        writable_data_files: vec![robolock.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let prof = seatbelt_profile(&policy, &paths).unwrap();
    let wt_canon = fs::canonicalize(&wt).unwrap();
    let robolock_canon = fs::canonicalize(&robolock).unwrap();
    let py = format!(
        r#"
import os, fcntl, sys
wt = {wt:?}
missing = os.path.join(wt, "local.properties")
robolock = {rl:?}
assert os.access(wt, os.W_OK), "expected W_OK on guarded worktree dir"
open(os.path.join(wt, "child.txt"), "w").write("ok")
for name, fn in [
    ("rename_wt", lambda: os.rename(wt, wt + "_moved")),
    ("chmod_wt", lambda: os.chmod(wt, 0o755)),
    ("symlink_missing", lambda: os.symlink("/tmp", missing)),
    ("create_missing", lambda: open(missing, "w").write("evil")),
    ("rename_dot_git", lambda: os.rename(os.path.join(wt, ".git"), os.path.join(wt, ".git2"))),
]:
    try:
        fn()
        sys.exit(f"FAIL: {{name}} should be denied")
    except OSError:
        pass
fd = os.open(robolock, os.O_RDWR | os.O_CREAT, 0o644)
fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
os.close(fd)
print("ALL_OK")
"#,
        wt = wt_canon.to_string_lossy(),
        rl = robolock_canon.to_string_lossy(),
    );
    let out = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &prof, "python3", "-c", &py])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sandbox probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("ALL_OK"));
}
