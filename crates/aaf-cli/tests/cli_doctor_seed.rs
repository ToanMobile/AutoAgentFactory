#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 6 tests: `aaf doctor` CoW volume diagnostics (P0.3) and seed dry-run (S7).

#[path = "cli_support.rs"]
mod support;

use std::path::Path;

use serde_json::{Value, json};
use support::{Sandbox, write};

fn check<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

#[test]
fn doctor_seed_checks_not_applicable_outside_git_repo() {
    let sb = Sandbox::without_git();
    let (code, out) = sb.json(&["doctor"]);
    assert_eq!(code, 0, "{out}");

    for name in ["worktree_cow_volume", "seed_cache_dry_run"] {
        let c = check(&out, name);
        assert_eq!(c["applicable"], json!(false), "{name}: {c}");
        assert_eq!(c["required"], json!(false), "{name}: {c}");
        assert!(
            c["detail"].as_str().unwrap().starts_with("not applicable"),
            "{name}: {c}"
        );
    }
}

#[test]
fn doctor_seed_dry_run_reports_would_seed_and_respects_opt_out_and_readonly() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);
    write(&sb.repo.join(".gitignore"), "Pods/\n");
    write(&sb.repo.join("Podfile"), "platform :ios, '15.0'\n");
    write(&sb.repo.join("Podfile.lock"), "PODS:\n  - Demo (1.0)\n");
    sb.commit_all("add pods markers");
    write(
        &sb.repo.join("Pods/Manifest.lock"),
        "PODS:\n  - Demo (1.0)\n",
    );
    write(&sb.repo.join("Pods/Demo/Demo.swift"), "let x = 1\n");

    let missing_wt = sb.repo.parent().unwrap().join("nonexistent-wt-root");
    let out = sb
        .command(&["doctor", "--json"])
        .env("AAF_WORKTREE_DIR", &missing_wt)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val = support::one_object(&out);

    // Doctor must stay strictly read-only and never create missing_wt.
    assert!(
        !missing_wt.exists(),
        "doctor must not create AAF_WORKTREE_DIR"
    );

    let dry = check(&val, "seed_cache_dry_run");
    let detail = dry["detail"].as_str().unwrap();
    assert!(
        detail.contains("Pods") && (detail.contains("would seed") || detail.contains("skip")),
        "expected dry-run seed summary for Pods, got: {detail}"
    );
    #[cfg(target_os = "macos")]
    assert!(
        detail.contains("would seed Pods"),
        "on macOS APFS same-volume, dry-run must say would seed Pods: {detail}"
    );

    // With AAF_SEED_CACHE=0, dry-run reports disabled.
    let out_off = sb
        .command(&["doctor", "--json"])
        .env("AAF_WORKTREE_DIR", &missing_wt)
        .env("AAF_SEED_CACHE", "0")
        .output()
        .unwrap();
    let val_off = support::one_object(&out_off);
    let dry_off = check(&val_off, "seed_cache_dry_run");
    assert!(
        dry_off["detail"]
            .as_str()
            .unwrap()
            .contains("AAF_SEED_CACHE=0"),
        "{dry_off}"
    );
}

#[test]
fn doctor_seed_warns_when_worktree_dir_inside_repo_or_unsafe_data_dir() {
    let sb = Sandbox::new();
    assert_eq!(sb.json(&["init"]).0, 0);

    // 1. Worktrees dir inside repo -> warn.
    let inside_repo = sb.repo.join(".nested-wt");
    let out = sb
        .command(&["doctor", "--json"])
        .env("AAF_WORKTREE_DIR", &inside_repo)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let val = support::one_object(&out);
    let c = check(&val, "worktree_cow_volume");
    assert_eq!(c["ok"], json!(false), "{c}");
    assert_eq!(c["required"], json!(false), "{c}");
    assert!(!inside_repo.exists());

    // 2. Worktrees dir inside data_dir (not `<data>/worktrees`) -> warn.
    let bad_data_wt = sb.data.join("other-dir");
    let out2 = sb
        .command(&["doctor", "--json"])
        .env("AAF_WORKTREE_DIR", &bad_data_wt)
        .output()
        .unwrap();
    let val2 = support::one_object(&out2);
    let c2 = check(&val2, "worktree_cow_volume");
    assert_eq!(c2["ok"], json!(false), "{c2}");
    assert!(!bad_data_wt.exists());

    // 3. `<data>/worktrees` is allowed (not warned for data-dir confinement).
    let ok_data_wt = sb.data.join("worktrees");
    let out3 = sb
        .command(&["doctor", "--json"])
        .env("AAF_WORKTREE_DIR", &ok_data_wt)
        .output()
        .unwrap();
    let val3 = support::one_object(&out3);
    let c3 = check(&val3, "worktree_cow_volume");
    assert_eq!(c3["ok"], json!(true), "{c3}");
    assert!(!ok_data_wt.exists());

    // 4. Cross-volume check when repo and another existing volume differ (e.g. macOS /Volumes/Data vs /tmp).
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let (Ok(d1), Ok(d2)) = (
        aaf_engine::git::hydration::device::device_id(&sb.repo),
        aaf_engine::git::hydration::device::device_id(manifest_dir),
    ) && d1 != d2
    {
        let cross_missing = manifest_dir.join("target/__nonexistent_doctor_wt_probe");
        let out_x = sb
            .command(&["doctor", "--json"])
            .env("AAF_WORKTREE_DIR", &cross_missing)
            .output()
            .unwrap();
        let val_x = support::one_object(&out_x);
        let cx = check(&val_x, "worktree_cow_volume");
        assert_eq!(cx["ok"], json!(false), "cross-volume must warn: {cx}");
        assert!(!cross_missing.exists());
    }
}
