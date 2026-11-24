#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use aaf_domain::gate::{GoldenScore, PostGateAuditRecord};
use aaf_domain::pipeline::{IssueSeverity, ReviewIssue};
use aaf_fp::{
    AUDIT_PARTIAL_DOMAIN, FpError, PartialAuditKey, PartialAuditRecord, Signed,
    clear_audits_for_repo, load_or_create_key, partial_audit_path, read_partial_audit, sha256_hex,
    write_audit_verdict, write_partial_audit,
};
use tempfile::tempdir;

fn sample_record(repo_root: &Path) -> PartialAuditRecord {
    PartialAuditRecord {
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: "git-tree:1111111111111111".to_string(),
        base_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        config_hash: "cfg-hash-1".to_string(),
        plan_contract_hash: "plan-hash-1".to_string(),
        reviewer_id: "rev-primary".to_string(),
        executor: "antigravity".to_string(),
        canonical_provider: "gemini".to_string(),
        model: "model-a".to_string(),
        effort: "high".to_string(),
        verdict: "PASS".to_string(),
        p0: 0,
        p1: 0,
        p2: 1,
        mechanical_score: 100,
        llm_average: Some(97),
        effective_score: 97,
        scores: vec![GoldenScore {
            id: "G1".to_string(),
            score: 97,
            deduction_reason: String::new(),
        }],
        issues: vec![ReviewIssue {
            id: "ISS-1".to_string(),
            severity: IssueSeverity::P2,
            category: "STYLE".to_string(),
            location: "src/lib.rs:10".to_string(),
            description: "minor nit".to_string(),
            proposed_fix: "simplify".to_string(),
        }],
        created_at: 1_700_000_000,
    }
}

fn key_for<'a>(repo_root: &'a Path, rec: &'a PartialAuditRecord) -> PartialAuditKey<'a> {
    PartialAuditKey {
        repo_root,
        tree_fp: &rec.tree_fp,
        base_sha: &rec.base_sha,
        config_hash: &rec.config_hash,
        plan_contract_hash: &rec.plan_contract_hash,
        reviewer_id: &rec.reviewer_id,
        executor: &rec.executor,
        model: &rec.model,
        effort: &rec.effort,
    }
}

#[test]
fn partial_audit_path_uses_full_sha256_and_prevents_traversal() {
    let data = tempdir().unwrap();
    let repo = tempdir().unwrap();

    let path = partial_audit_path(
        data.path(),
        repo.path(),
        "git-tree:abc",
        "base123",
        "cfg456",
        "plan789",
        "../../receipts/latest",
    );
    let expected_hash =
        sha256_hex(b"git-tree:abc\0base123\0cfg456\0plan789\0../../receipts/latest");
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        format!("{expected_hash}.partial.json")
    );
    assert!(path.starts_with(data.path().join("audits")));
}

#[test]
fn partial_audit_round_trip_and_missing_returns_none() {
    let data = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let key = load_or_create_key(data.path()).unwrap();
    let rec = sample_record(repo.path());
    let expected = key_for(repo.path(), &rec);

    // Missing file returns Ok(None).
    assert_eq!(
        read_partial_audit(data.path(), &key, &expected).unwrap(),
        None
    );

    let written = write_partial_audit(data.path(), &key, repo.path(), &rec).unwrap();
    assert!(written.exists());

    let loaded = read_partial_audit(data.path(), &key, &expected)
        .unwrap()
        .expect("partial audit should exist");
    assert_eq!(loaded, rec);
    assert_eq!(AUDIT_PARTIAL_DOMAIN, "audit_partial\0");
}

#[test]
fn partial_audit_rejects_tampering_and_wrong_key() {
    let data = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let key = load_or_create_key(data.path()).unwrap();
    let rec = sample_record(repo.path());
    let expected = key_for(repo.path(), &rec);

    let path = write_partial_audit(data.path(), &key, repo.path(), &rec).unwrap();

    // Wrong key fails closed.
    let other_key = vec![0x5au8; 32];
    assert!(matches!(
        read_partial_audit(data.path(), &other_key, &expected),
        Err(FpError::BadSignature)
    ));

    // Tampered payload fails closed.
    let mut signed: Signed = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    signed.payload = signed.payload.replace("\"PASS\"", "\"FAIL\"");
    fs::write(&path, serde_json::to_vec(&signed).unwrap()).unwrap();
    assert!(matches!(
        read_partial_audit(data.path(), &key, &expected),
        Err(FpError::BadSignature)
    ));
}

#[test]
fn partial_audit_misses_on_path_key_change_and_rejects_binding_mismatches() {
    let data = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let key = load_or_create_key(data.path()).unwrap();
    let rec = sample_record(repo.path());
    let src_path = write_partial_audit(data.path(), &key, repo.path(), &rec).unwrap();

    // Changing base_sha or plan_contract_hash resolves to a different path -> cache miss (None).
    let mut diff_base = key_for(repo.path(), &rec);
    diff_base.base_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    assert_eq!(
        read_partial_audit(data.path(), &key, &diff_base).unwrap(),
        None
    );

    let mut diff_plan = key_for(repo.path(), &rec);
    diff_plan.plan_contract_hash = "plan-hash-2";
    assert_eq!(
        read_partial_audit(data.path(), &key, &diff_plan).unwrap(),
        None
    );

    // Copying the validly signed file into another binding slot fails closed with BadSignature.
    let target_base_path = partial_audit_path(
        data.path(),
        repo.path(),
        diff_base.tree_fp,
        diff_base.base_sha,
        diff_base.config_hash,
        diff_base.plan_contract_hash,
        diff_base.reviewer_id,
    );
    fs::copy(&src_path, &target_base_path).unwrap();
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_base),
        Err(FpError::BadSignature)
    ));

    let target_plan_path = partial_audit_path(
        data.path(),
        repo.path(),
        diff_plan.tree_fp,
        diff_plan.base_sha,
        diff_plan.config_hash,
        diff_plan.plan_contract_hash,
        diff_plan.reviewer_id,
    );
    fs::copy(&src_path, &target_plan_path).unwrap();
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_plan),
        Err(FpError::BadSignature)
    ));

    // Copying into a different reviewer_id slot fails closed.
    let mut diff_rev = key_for(repo.path(), &rec);
    diff_rev.reviewer_id = "rev-other";
    let target_rev_path = partial_audit_path(
        data.path(),
        repo.path(),
        diff_rev.tree_fp,
        diff_rev.base_sha,
        diff_rev.config_hash,
        diff_rev.plan_contract_hash,
        diff_rev.reviewer_id,
    );
    fs::copy(&src_path, &target_rev_path).unwrap();
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_rev),
        Err(FpError::BadSignature)
    ));

    // Same file path, but executor / model / effort changed -> BadSignature.
    let mut diff_exec = key_for(repo.path(), &rec);
    diff_exec.executor = "claude";
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_exec),
        Err(FpError::BadSignature)
    ));

    let mut diff_model = key_for(repo.path(), &rec);
    diff_model.model = "model-b";
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_model),
        Err(FpError::BadSignature)
    ));

    let mut diff_effort = key_for(repo.path(), &rec);
    diff_effort.effort = "low";
    assert!(matches!(
        read_partial_audit(data.path(), &key, &diff_effort),
        Err(FpError::BadSignature)
    ));
}

#[test]
fn partial_audit_rejects_cross_repo_copy() {
    let data = tempdir().unwrap();
    let repo_a = tempdir().unwrap();
    let repo_b = tempdir().unwrap();
    let key = load_or_create_key(data.path()).unwrap();

    let rec_a = sample_record(repo_a.path());
    let path_a = write_partial_audit(data.path(), &key, repo_a.path(), &rec_a).unwrap();

    let path_b = partial_audit_path(
        data.path(),
        repo_b.path(),
        &rec_a.tree_fp,
        &rec_a.base_sha,
        &rec_a.config_hash,
        &rec_a.plan_contract_hash,
        &rec_a.reviewer_id,
    );
    fs::create_dir_all(path_b.parent().unwrap()).unwrap();
    fs::copy(&path_a, &path_b).unwrap();

    let key_b = key_for(repo_b.path(), &rec_a);
    assert!(matches!(
        read_partial_audit(data.path(), &key, &key_b),
        Err(FpError::BadSignature)
    ));
}

#[test]
fn clear_audits_for_repo_removes_final_and_partial_without_touching_other_repos() {
    let data = tempdir().unwrap();
    let repo_a = tempdir().unwrap();
    let repo_b = tempdir().unwrap();
    let key = load_or_create_key(data.path()).unwrap();

    // Missing dir returns 0.
    assert_eq!(
        clear_audits_for_repo(data.path(), repo_a.path()).unwrap(),
        0
    );

    let rec_a = sample_record(repo_a.path());
    let partial_a = write_partial_audit(data.path(), &key, repo_a.path(), &rec_a).unwrap();
    let final_a = write_audit_verdict(
        data.path(),
        &key,
        repo_a.path(),
        &PostGateAuditRecord {
            repo_root: repo_a.path().to_string_lossy().into_owned(),
            tree_fp: rec_a.tree_fp.clone(),
            config_hash: rec_a.config_hash.clone(),
            verdict: "PASS".to_string(),
            reviewer: "dual".to_string(),
            same_account: false,
            mechanical_score: 100,
            llm_average: Some(97),
            effective_score: 97,
            scores: vec![],
            dual_consensus: true,
            adjudicated: false,
            reviewers: vec![],
            created_at: 1_700_000_000,
        },
    )
    .unwrap();

    let rec_b = sample_record(repo_b.path());
    let partial_b = write_partial_audit(data.path(), &key, repo_b.path(), &rec_b).unwrap();

    let removed = clear_audits_for_repo(data.path(), repo_a.path()).unwrap();
    assert_eq!(removed, 2);
    assert!(!partial_a.exists());
    assert!(!final_a.exists());
    assert!(!partial_a.parent().unwrap().exists());
    assert!(partial_b.exists());
}
