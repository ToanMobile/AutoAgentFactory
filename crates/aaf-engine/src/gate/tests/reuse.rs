//! Receipt reuse (docs/PLAN.md C.11): a signed `Clear` receipt for the same
//! tree and configuration, from a mode at least as strict, is returned as is
//! and no command runs. Every other case runs the gate again.

use std::fs;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, Receipt};
use tempfile::TempDir;

use super::support::{FixedClock, Repo};
use crate::throttle::tests::support::Rig;

/// Every command appends its name to a file outside the repo, so the tree
/// stays the same and each run is visible.
fn counting_cfg(log: &Path) -> RepoConfig {
    let step = |name: &str| format!("echo {name} >> '{}'", log.display());
    let mut cfg = RepoConfig::default();
    cfg.commands.lint = step("lint");
    cfg.commands.build = step("build");
    cfg.commands.test_scoped = step("scoped");
    cfg.commands.test_full = step("full");
    cfg
}

struct Case {
    repo: Repo,
    out: TempDir,
    cfg: RepoConfig,
}

impl Case {
    fn new() -> Case {
        let out = TempDir::new().unwrap();
        let cfg = counting_cfg(&out.path().join("log"));
        Case {
            repo: Repo::new(),
            out,
            cfg,
        }
    }

    fn gate_at(&self, mode: GateMode, now: i64) -> Receipt {
        self.repo
            .gate_on(&Rig::calm(), &self.cfg, mode, &FixedClock(now))
    }

    /// Commands run so far, in order.
    fn ran(&self) -> Vec<String> {
        fs::read_to_string(self.out.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn receipt_file(&self) -> std::path::PathBuf {
        let root = fs::canonicalize(self.repo.root()).unwrap();
        aaf_fp::receipt_path(self.repo.data_dir(), &root)
    }
}

#[test]
fn same_tree_config_and_mode_reuses_the_receipt_and_runs_nothing() {
    let case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    assert_eq!(first.status, GateStatus::Clear, "{:?}", first.reasons);
    assert_eq!(case.ran(), vec!["lint", "scoped"]);

    let again = case.gate_at(GateMode::Scoped, 2_000);
    // Returned as is: even the timestamp is the stored one.
    assert_eq!(again, first);
    assert_eq!(again.created_at, 1_000);
    assert_eq!(case.ran(), vec!["lint", "scoped"]);
}

#[test]
fn a_stricter_stored_mode_covers_a_weaker_request() {
    let case = Case::new();
    let release = case.gate_at(GateMode::Release, 1_000);
    assert_eq!(release.status, GateStatus::Clear, "{:?}", release.reasons);
    let ran = case.ran();
    assert_eq!(ran, vec!["lint", "build", "full"]);

    for mode in [GateMode::Scoped, GateMode::Full, GateMode::Release] {
        let reused = case.gate_at(mode, 2_000);
        assert_eq!(reused, release, "{mode:?}");
        assert_eq!(reused.mode, GateMode::Release);
    }
    assert_eq!(case.ran(), ran);
}

#[test]
fn a_weaker_stored_mode_does_not_cover_a_stricter_request() {
    let case = Case::new();
    case.gate_at(GateMode::Scoped, 1_000);
    let full = case.gate_at(GateMode::Full, 2_000);
    assert_eq!(full.mode, GateMode::Full);
    assert_eq!(full.created_at, 2_000);
    assert_eq!(case.ran(), vec!["lint", "scoped", "lint", "build", "full"]);

    // Full is still weaker than Release.
    let release = case.gate_at(GateMode::Release, 3_000);
    assert_eq!(release.mode, GateMode::Release);
    assert_eq!(case.ran().len(), 8);
}

#[test]
fn a_changed_tree_runs_the_gate_again() {
    let case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    case.repo
        .write("src/more.rs", "pub fn two() -> u32 {\n    2\n}\n");
    let second = case.gate_at(GateMode::Scoped, 2_000);
    assert_ne!(second.tree_fp, first.tree_fp);
    assert_eq!(second.created_at, 2_000);
    assert_eq!(case.ran(), vec!["lint", "scoped", "lint", "scoped"]);
}

#[test]
fn a_changed_gate_config_runs_the_gate_again() {
    let mut case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    case.cfg.gate.command_timeout_secs += 1;
    let second = case.gate_at(GateMode::Scoped, 2_000);
    assert_ne!(second.config_hash, first.config_hash);
    assert_eq!(second.tree_fp, first.tree_fp);
    assert_eq!(case.ran(), vec!["lint", "scoped", "lint", "scoped"]);
}

#[test]
fn blocked_and_unverified_receipts_are_never_reused() {
    let mut case = Case::new();
    let log = case.out.path().join("log");
    case.cfg.commands.test_scoped = format!("echo scoped >> '{}'; exit 1", log.display());
    for now in [1_000, 2_000] {
        let blocked = case.gate_at(GateMode::Scoped, now);
        assert_eq!(blocked.status, GateStatus::Blocked);
        assert_eq!(blocked.created_at, now);
    }
    assert_eq!(case.ran(), vec!["lint", "scoped", "lint", "scoped"]);

    case.cfg.commands.test_scoped = String::new();
    for now in [3_000, 4_000] {
        let unverified = case.gate_at(GateMode::Scoped, now);
        assert_eq!(unverified.status, GateStatus::Unverified);
        assert_eq!(unverified.created_at, now);
    }
    assert_eq!(case.ran().len(), 6);
}

#[test]
fn a_tampered_receipt_is_not_reused() {
    let case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    assert_eq!(first.status, GateStatus::Clear);
    let path = case.receipt_file();
    let genuine = fs::read_to_string(&path).unwrap();

    // The payload is edited (a later timestamp) under the old signature.
    let edited = genuine.replace("created_at\\\":1000", "created_at\\\":1001");
    assert_ne!(edited, genuine);
    fs::write(&path, edited).unwrap();
    let rerun = case.gate_at(GateMode::Scoped, 2_000);
    assert_eq!(rerun.created_at, 2_000);
    assert_eq!(case.ran().len(), 4);

    // A receipt signed with somebody else's key.
    aaf_fp::write_receipt(case.repo.data_dir(), &[7u8; 32], &rerun).unwrap();
    let rerun = case.gate_at(GateMode::Scoped, 3_000);
    assert_eq!(rerun.created_at, 3_000);
    assert_eq!(case.ran().len(), 6);

    // The genuine file put back is reused again: the misses above were
    // caused by the signature and by nothing else.
    fs::write(&path, &genuine).unwrap();
    assert_eq!(case.gate_at(GateMode::Scoped, 4_000), first);
    assert_eq!(case.ran().len(), 6);
}

#[test]
fn a_moved_base_runs_the_gate_again_and_the_head_lock_blocks() {
    let case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    assert_eq!(first.status, GateStatus::Clear, "{:?}", first.reasons);

    // A locked file lands on main. The feature branch is checked out again
    // with exactly the files it had.
    case.repo.git(&["checkout", "-q", "main"]);
    case.repo.write("tests/must_keep.rs", "fn kept() {}\n");
    case.repo.commit("add a locked test");
    case.repo.git(&["checkout", "-q", "feature"]);

    // The fork point (the base the gate compares against) has not moved, so
    // the receipt still proves this tree.
    assert_eq!(case.gate_at(GateMode::Scoped, 2_000), first);
    assert_eq!(case.ran(), vec!["lint", "scoped"]);

    // The branch now sits on the new main with its files untouched: the
    // tree is the same, but against this base it deletes the locked test.
    case.repo.git(&["reset", "-q", "--soft", "main"]);
    let second = case.gate_at(GateMode::Scoped, 3_000);
    assert_eq!(second.tree_fp, first.tree_fp);
    assert_ne!(second.base_sha, first.base_sha);
    assert_eq!(second.created_at, 3_000);
    assert_eq!(second.head_lock.status, LockStatus::Blocked);
    assert_eq!(second.status, GateStatus::Blocked);
}

#[test]
fn a_receipt_signed_for_another_repo_root_is_not_reused() {
    let case = Case::new();
    let first = case.gate_at(GateMode::Scoped, 1_000);
    assert_eq!(first.status, GateStatus::Clear, "{:?}", first.reasons);

    // Genuinely signed, but it says it was made somewhere else (a receipt
    // file copied between repos, or two roots sharing one receipt path).
    let mut elsewhere = first.clone();
    elsewhere.repo_root = "/somewhere/else".to_string();
    let key = aaf_fp::load_key(case.repo.data_dir()).unwrap();
    let payload = serde_json::to_string(&elsewhere).unwrap();
    let signed = aaf_fp::sign_tagged(&key, aaf_fp::RECEIPT_DOMAIN, payload).unwrap();
    fs::write(
        case.receipt_file(),
        serde_json::to_vec_pretty(&signed).unwrap(),
    )
    .unwrap();

    let rerun = case.gate_at(GateMode::Scoped, 2_000);
    assert_eq!(rerun.created_at, 2_000);
    assert_eq!(rerun.repo_root, first.repo_root);
    assert_eq!(case.ran().len(), 4);
}

#[test]
fn a_blocked_receipt_edited_to_clear_does_not_pass() {
    let mut case = Case::new();
    let log = case.out.path().join("log");
    case.cfg.commands.test_scoped = format!("echo scoped >> '{}'; exit 1", log.display());
    let blocked = case.gate_at(GateMode::Scoped, 1_000);
    assert_eq!(blocked.status, GateStatus::Blocked);

    let path = case.receipt_file();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("BLOCKED"));
    fs::write(&path, text.replace("BLOCKED", "CLEAR")).unwrap();
    let again = case.gate_at(GateMode::Scoped, 2_000);
    assert_eq!(again.status, GateStatus::Blocked);
    assert_eq!(again.created_at, 2_000);
}

/// A release receipt from one worktree is not written into a sibling
/// worktree whose tree holds a file hidden only by `info/exclude`: a release
/// gate there would have blocked on it, and the materialized receipt would
/// unlock a store submission of that tree (audit a13 F2-1).
#[test]
fn a_common_release_receipt_is_not_materialized_beside_hidden_files() {
    let case = Case::new();
    let release = case.gate_at(GateMode::Release, 1_000);
    assert_eq!(release.status, GateStatus::Clear, "{:?}", release.reasons);
    let ran = case.ran().len();

    let sibling_dir = TempDir::new().unwrap();
    let sibling = sibling_dir.path().join("wt");
    case.repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        sibling.to_str().unwrap(),
        "HEAD",
    ]);
    let sibling = fs::canonicalize(&sibling).unwrap();
    fs::write(sibling.join("x"), "planted\n").unwrap();
    let common = case
        .repo
        .git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let exclude = Path::new(&common).join("info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
    rules.push_str("\nx\n");
    fs::write(&exclude, rules).unwrap();

    let rig = Rig::calm();
    let run = super::super::run_gate_policy(
        case.repo.data_dir(),
        &sibling,
        &case.cfg,
        GateMode::Scoped,
        &FixedClock(2_000),
        &rig.throttle(),
        super::super::HiddenPolicy::Warn,
    )
    .unwrap();
    assert!(
        !run.reused,
        "a release receipt stood in beside a hidden file"
    );
    assert_eq!(run.receipt.mode, GateMode::Scoped);
    assert!(case.ran().len() > ran, "the scoped gate must run");
    let local = aaf_fp::read_receipt(
        case.repo.data_dir(),
        &aaf_fp::load_key(case.repo.data_dir()).unwrap(),
        &sibling,
    )
    .unwrap();
    assert_ne!(local.mode, GateMode::Release, "{:?}", local.reasons);
}

/// Without a hidden file the sibling still reuses the common receipt.
#[test]
fn a_common_release_receipt_still_covers_a_clean_sibling_worktree() {
    let case = Case::new();
    let release = case.gate_at(GateMode::Release, 1_000);
    assert_eq!(release.status, GateStatus::Clear, "{:?}", release.reasons);
    let ran = case.ran().len();
    let sibling_dir = TempDir::new().unwrap();
    let sibling = sibling_dir.path().join("wt");
    case.repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        sibling.to_str().unwrap(),
        "HEAD",
    ]);
    let rig = Rig::calm();
    let run = super::super::run_gate_policy(
        case.repo.data_dir(),
        &sibling,
        &case.cfg,
        GateMode::Scoped,
        &FixedClock(2_000),
        &rig.throttle(),
        super::super::HiddenPolicy::Warn,
    )
    .unwrap();
    assert!(run.reused, "{:?}", run.receipt.reasons);
    assert_eq!(run.receipt.mode, GateMode::Release);
    assert_eq!(case.ran().len(), ran);
}

/// One git call answers what `aaf slot` asked with two (audit a13 O11-7):
/// the same root and the same shared statistics id, in the main checkout,
/// in a sibling worktree, in a subdirectory, and nothing outside a repo.
#[test]
fn repo_root_and_stats_id_match_the_two_separate_calls() {
    let case = Case::new();
    let sibling_dir = TempDir::new().unwrap();
    let sibling = sibling_dir.path().join("wt");
    case.repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        sibling.to_str().unwrap(),
        "HEAD",
    ]);
    for dir in [
        case.repo.root().to_path_buf(),
        sibling.clone(),
        sibling.join("src"),
    ] {
        let root = aaf_fp::repo_root(&dir).unwrap();
        let expected = (root.clone(), super::super::stats_repo_id(&root));
        assert_eq!(
            super::super::repo_root_and_stats_id(&dir),
            Some(expected),
            "{dir:?}"
        );
    }
    let main_id = super::super::stats_repo_id(case.repo.root());
    assert_eq!(
        super::super::repo_root_and_stats_id(&sibling).map(|(_, id)| id),
        Some(main_id)
    );
    let outside = TempDir::new().unwrap();
    assert_eq!(super::super::repo_root_and_stats_id(outside.path()), None);
}

/// In a normal repository and in a sibling worktree the combined call alone
/// answers: one git call, the two-call fallback never asked (audit a13
/// O11-7). Outside a repository the fallback is what says `None`.
#[test]
fn repo_root_and_stats_id_takes_the_single_call_path() {
    use std::cell::Cell;
    let case = Case::new();
    let sibling_dir = TempDir::new().unwrap();
    let sibling = sibling_dir.path().join("wt");
    case.repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        sibling.to_str().unwrap(),
        "HEAD",
    ]);
    let outside = TempDir::new().unwrap();
    for (dir, in_repo) in [
        (case.repo.root().to_path_buf(), true),
        (sibling.join("src"), true),
        (outside.path().to_path_buf(), false),
    ] {
        let (calls, fallbacks) = (Cell::new(0), Cell::new(0));
        let got = super::super::reuse::repo_root_and_stats_id_by(
            |args| {
                calls.set(calls.get() + 1);
                aaf_fp::git(&dir, args, &[], std::time::Duration::from_secs(30)).ok()
            },
            || {
                fallbacks.set(fallbacks.get() + 1);
                None
            },
        );
        assert_eq!(
            (calls.get(), fallbacks.get()),
            (1, usize::from(!in_repo)),
            "{dir:?}"
        );
        assert_eq!(got, super::super::repo_root_and_stats_id(&dir), "{dir:?}");
    }
}
