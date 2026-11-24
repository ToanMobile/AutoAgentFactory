use std::time::Duration;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{
    ChangeKind, CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, LockedChange,
    Severity, StoreComplianceReport, StoreFinding, StoreStatus,
};

use super::super::{
    Evidence, GIT_TIMEOUT, GateError, REASON_TREE_CHANGED, base, config_hash, decide, head_lock,
    run_gate_in,
};
use super::support::{FixedClock, Repo, fake_secret, passing_cfg};

fn current_fp(repo: &Repo) -> String {
    aaf_fp::tree_fp(repo.root(), Duration::from_secs(30)).unwrap()
}

fn secret_line() -> String {
    format!("const K: &str = \"{}\";\n", fake_secret())
}

#[test]
fn clear_path_writes_a_verifiable_receipt_for_the_current_tree() {
    let repo = Repo::new();
    repo.write("src/more.rs", "pub fn two() -> u32 {\n    2\n}\n");
    repo.commit("feature work");
    repo.write("notes.txt", "uncommitted and untracked\n");
    let cfg = passing_cfg();

    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(receipt.reasons.is_empty());
    assert_eq!(receipt.mode, GateMode::Scoped);
    assert_eq!(receipt.created_at, 1_000);
    assert_eq!(receipt.config_hash, config_hash(&cfg));
    assert_eq!(receipt.head_sha, Some(repo.git(&["rev-parse", "HEAD"])));
    assert_eq!(receipt.base_sha, Some(repo.git(&["rev-parse", "main"])));
    assert_eq!(receipt.head_lock.status, LockStatus::Clear);
    assert_eq!(receipt.store_compliance.status, StoreStatus::NotApplicable);
    assert!(receipt.static_findings.is_empty());
    let names: Vec<&str> = receipt.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["lint", "test_scoped"]);
    assert!(receipt.commands.iter().all(CommandResult::passed));

    assert_eq!(receipt.tree_fp, current_fp(&repo));
    let key = aaf_fp::load_key(repo.data_dir()).unwrap();
    let stored = aaf_fp::read_receipt(repo.data_dir(), &key, repo.root()).unwrap();
    assert_eq!(stored, receipt);
    // A different key must not verify what the gate signed.
    assert!(aaf_fp::read_receipt(repo.data_dir(), &[9u8; 32], repo.root()).is_err());
}

#[test]
fn full_mode_runs_lint_build_and_the_full_tests() {
    let repo = Repo::new();
    let mut cfg = passing_cfg();
    cfg.commands.build = "true".to_string();
    cfg.commands.test_full = "echo full-suite".to_string();
    let receipt = repo.gate(&cfg, GateMode::Full);
    assert_eq!(receipt.status, GateStatus::Clear);
    let names: Vec<&str> = receipt.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["lint", "build", "test_full"]);
    assert!(receipt.commands[2].log_tail.contains("full-suite"));
}

#[test]
fn critical_static_finding_blocks_and_skips_the_commands() {
    let repo = Repo::new();
    repo.append("src/lib.rs", &secret_line());
    repo.commit("oops");
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "touch commands-ran".to_string();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.static_findings.len(), 1);
    assert_eq!(receipt.static_findings[0].rule, "SEC-001");
    assert_eq!(receipt.static_findings[0].file, "src/lib.rs");
    assert_eq!(receipt.static_findings[0].line, Some(4));
    assert!(receipt.commands.is_empty());
    assert!(!repo.path("commands-ran").exists());
    let starts = |prefix: &str| receipt.reasons.iter().any(|r| r.starts_with(prefix));
    assert!(starts("static checks"));
    assert!(starts("commands skipped"));
}

#[test]
fn warnings_alone_do_not_block() {
    let repo = Repo::new();
    repo.append("src/lib.rs", &["pub fn f() { db", "g!(1); }\n"].concat());
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear);
    assert_eq!(receipt.static_findings.len(), 1);
    assert_eq!(receipt.static_findings[0].severity, Severity::Warn);
}

#[test]
fn only_added_lines_are_scanned() {
    let repo = Repo::new();
    // The secret is already on the base branch; this change does not add it.
    repo.git(&["checkout", "-q", "main"]);
    repo.write("src/legacy.rs", &secret_line());
    repo.commit("legacy");
    repo.git(&["checkout", "-q", "-b", "next"]);
    repo.append("src/legacy.rs", "pub fn fine() {}\n");
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert!(receipt.static_findings.is_empty());
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
}

#[test]
fn untracked_secret_file_is_found_and_ignored_files_are_not() {
    let repo = Repo::new();
    repo.write(".gitignore", "private.cfg\n");
    repo.write("private.cfg", &format!("KEY={}\n", fake_secret()));
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);

    let creds = format!("line one\nKEY={}\n", fake_secret());
    repo.write("deploy/creds.txt", &creds);
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.static_findings.len(), 1);
    assert_eq!(receipt.static_findings[0].file, "deploy/creds.txt");
    assert_eq!(receipt.static_findings[0].line, Some(2));
}

#[test]
fn untracked_binary_file_is_not_scanned() {
    let repo = Repo::new();
    let mut bytes = vec![0u8, 1, 2, 0];
    bytes.extend_from_slice(fake_secret().as_bytes());
    std::fs::write(repo.path("blob.bin"), bytes).unwrap();
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert!(receipt.static_findings.is_empty());
}

#[test]
fn inline_allow_is_counted_in_the_receipt() {
    let repo = Repo::new();
    let line = [
        "const K: &str = \"",
        &fake_secret(),
        "\"; // aaf:al",
        "low(SEC-001) documented sample key\n",
    ]
    .concat();
    repo.append("src/lib.rs", &line);
    let cfg = passing_cfg();
    let blocked = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(blocked.status, GateStatus::Blocked);
    assert_eq!(blocked.head_lock.status, LockStatus::Blocked);
    assert_eq!(blocked.suppressions, 1);
    assert_eq!(
        blocked
            .static_findings
            .iter()
            .map(|f| f.rule.as_str())
            .collect::<Vec<_>>(),
        vec!["ALLOW-001"]
    );

    let clock = FixedClock(6);
    head_lock::approve(repo.data_dir(), repo.root(), &cfg, "rev", false, &clock).unwrap();
    let clear = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(clear.status, GateStatus::Clear, "{:?}", clear.reasons);
    assert_eq!(clear.head_lock.status, LockStatus::Approved);
    assert_eq!(clear.suppressions, 1);
}

#[test]
fn head_lock_blocks_until_approved() {
    let repo = Repo::new();
    repo.append("tests/a_test.rs", "// changed assertion\n");
    let cfg = passing_cfg();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    assert!(receipt.commands.is_empty());
    assert!(receipt.reasons.iter().any(|r| r.starts_with("head lock")));

    let clock = FixedClock(5);
    head_lock::approve(repo.data_dir(), repo.root(), &cfg, "rev", false, &clock).unwrap();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert_eq!(receipt.head_lock.status, LockStatus::Approved);

    repo.append("tests/a_test.rs", "// edited after approval\n");
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
}

/// In a repository that has none, a task adding Claude Code's instructions,
/// hooks or servers is as unaudited as one editing them. Every other new
/// file, a new test included, stays exempt.
#[test]
fn an_added_agent_config_file_is_locked_and_other_added_files_are_not() {
    let cfg = passing_cfg();
    let repo = Repo::new();
    repo.write("tests/b_test.rs", "fn more() {}\n");
    repo.write("src/two.rs", "pub fn two() {}\n");
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert_eq!(receipt.head_lock.status, LockStatus::Clear);

    for path in [
        ".claude/settings.json",
        "CLAUDE.md",
        ".mcp.json",
        "packages/app/CLAUDE.md",
    ] {
        let repo = Repo::new();
        repo.write(path, "{}\n");
        let receipt = repo.gate(&cfg, GateMode::Scoped);
        assert_eq!(receipt.status, GateStatus::Blocked, "{path}");
        assert_eq!(
            receipt.head_lock.locked_changes,
            [LockedChange {
                path: path.to_string(),
                change: ChangeKind::Added,
                tier: aaf_domain::gate::LockTier::Hard,
            }],
            "{path}"
        );
        assert!(receipt.commands.is_empty(), "{path}");
    }

    // Committed on the branch rather than left untracked: the same, and an
    // approval of exactly that content unlocks it.
    let repo = Repo::new();
    repo.write("CLAUDE.md", "be brief\n");
    repo.commit("instructions");
    assert_eq!(
        repo.gate(&cfg, GateMode::Scoped).status,
        GateStatus::Blocked
    );
    let clock = FixedClock(5);
    head_lock::approve(repo.data_dir(), repo.root(), &cfg, "rev", false, &clock).unwrap();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert_eq!(receipt.head_lock.status, LockStatus::Approved);

    // A repository whose own list leaves them out decides for itself, as it
    // does for an edit (`aaf doctor` says so).
    let mut own_list = cfg.clone();
    own_list.gate.head_lock_globs = vec!["**/tests/**".to_string()];
    let receipt = repo.gate(&own_list, GateMode::Scoped);
    assert_eq!(receipt.head_lock.status, LockStatus::Clear);
}

#[test]
fn failed_command_blocks() {
    let repo = Repo::new();
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "echo boom; exit 3".to_string();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.commands.len(), 2);
    assert_eq!(receipt.commands[1].exit_code, Some(3));
    assert!(receipt.commands[1].log_tail.contains("boom"));
    assert_eq!(receipt.reasons, vec!["command test_scoped failed (exit 3)"]);
}

#[test]
fn command_timeout_blocks() {
    let repo = Repo::new();
    let mut cfg = passing_cfg();
    cfg.gate.command_timeout_secs = 1;
    cfg.commands.test_scoped = "sleep 300".to_string();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert!(receipt.commands[1].timed_out);
    assert_eq!(receipt.reasons, vec!["command test_scoped timed out"]);
}

#[test]
fn empty_test_command_is_unverified() {
    let repo = Repo::new();
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = String::new();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Unverified);
    assert_eq!(
        receipt.reasons,
        vec!["no test command is configured for mode scoped"]
    );
    // The scoped command does not stand in for the full one.
    let receipt = repo.gate(&passing_cfg(), GateMode::Release);
    assert_eq!(receipt.status, GateStatus::Unverified);
    assert_eq!(receipt.mode, GateMode::Release);
}

#[test]
fn command_that_edits_a_tracked_file_is_unverified() {
    let repo = Repo::new();
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "echo changed >> README.md".to_string();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Unverified);
    assert_eq!(receipt.reasons, vec![REASON_TREE_CHANGED]);
    // The receipt is bound to the tree as it is now.
    assert_eq!(receipt.tree_fp, current_fp(&repo));
}

#[test]
fn command_writing_only_ignored_files_stays_clear() {
    let repo = Repo::new();
    repo.write(".gitignore", "out/\n");
    repo.commit("ignore build output");
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "mkdir -p out && echo artefact > out/log.txt".to_string();
    let receipt = repo.gate(&cfg, GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
}

#[test]
fn blocked_wins_over_unverified() {
    let repo = Repo::new();
    repo.write("src/k.rs", &secret_line());
    let receipt = repo.gate(&RepoConfig::default(), GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Blocked);
    let starts = |prefix: &str| receipt.reasons.iter().any(|r| r.starts_with(prefix));
    assert!(starts("no test command"));
}

fn evidence<'a>(
    head_lock: &'a HeadLockReport,
    store: &'a StoreComplianceReport,
    commands: Option<&'a [CommandResult]>,
) -> Evidence<'a> {
    Evidence {
        hidden: &[],
        critical_findings: 0,
        head_lock,
        store,
        proof_block: None,
        commands,
        test_command_missing: false,
        tree_changed: false,
        mode: GateMode::Release,
    }
}

/// The store scanner is a stub in this lane, so its blocking path is proven
/// on the decision function with a hand-built report.
#[test]
fn store_block_decides_blocked_and_other_store_states_do_not() {
    let lock = HeadLockReport {
        status: LockStatus::Clear,
        lock_fp: String::new(),
        locked_changes: Vec::new(),
        approval: None,
    };
    let finding = StoreFinding {
        rule: "AND-001".to_string(),
        severity: Severity::Critical,
        platform: "android".to_string(),
        guideline: "target API level".to_string(),
        file: "app/build.gradle".to_string(),
        line: None,
        message: "too low".to_string(),
        fix: "raise it".to_string(),
    };
    let blocked = StoreComplianceReport::from_findings(vec![finding], Vec::new());
    assert_eq!(blocked.status, StoreStatus::Blocked);
    let (status, reasons) = decide(&evidence(&lock, &blocked, None));
    assert_eq!(status, GateStatus::Blocked);
    assert!(reasons[0].starts_with("store compliance"));

    for store in [
        StoreComplianceReport::from_findings(Vec::new(), Vec::new()),
        StoreComplianceReport::skipped(),
        StoreComplianceReport::not_applicable(),
    ] {
        let (status, reasons) = decide(&evidence(&lock, &store, Some(&[])));
        assert_eq!(status, GateStatus::Clear);
        assert!(reasons.is_empty());
    }
}

#[test]
fn config_hash_tracks_gate_and_commands_only() {
    let cfg = RepoConfig::default();
    let hash = config_hash(&cfg);
    assert_eq!(hash.len(), 64);
    assert_eq!(hash, config_hash(&RepoConfig::default()));

    let mut other = cfg.clone();
    other.commands.test_full = "make test".to_string();
    assert_ne!(config_hash(&other), hash);
    let mut other = cfg.clone();
    other.commands.repro = "make repro".to_string();
    assert_ne!(config_hash(&other), hash);
    let mut other = cfg.clone();
    other.gate.head_lock_globs.push("**/extra/**".to_string());
    assert_ne!(config_hash(&other), hash);
    let mut other = cfg.clone();
    other.run.max_parallel = 9;
    other.project.name = "x".to_string();
    assert_eq!(config_hash(&other), hash);
}

#[test]
fn base_is_the_merge_base_with_the_trunk() {
    let repo = Repo::new();
    let fork = repo.git(&["rev-parse", "main"]);
    repo.write("src/f.rs", "pub fn f() {}\n");
    repo.commit("on feature");
    // The trunk moves on after the fork.
    repo.git(&["checkout", "-q", "main"]);
    repo.write("src/trunk.rs", "pub fn t() {}\n");
    repo.commit("on main");
    repo.git(&["checkout", "-q", "feature"]);

    let auto = base::resolve(repo.root(), "auto", GIT_TIMEOUT).unwrap();
    assert_eq!(auto.sha.as_deref(), Some(fork.as_str()));
    let explicit = base::resolve(repo.root(), "main", GIT_TIMEOUT).unwrap();
    assert_eq!(explicit, auto);

    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.base_sha, Some(fork));
    assert_eq!(receipt.status, GateStatus::Clear);
}

#[test]
fn base_falls_back_to_head_and_rejects_a_bad_explicit_ref() {
    let repo = Repo::new();
    repo.git(&["branch", "-m", "main", "trunk"]);
    let head = repo.git(&["rev-parse", "HEAD"]);
    let auto = base::resolve(repo.root(), "auto", GIT_TIMEOUT).unwrap();
    assert_eq!(auto.sha, Some(head));

    for bad in ["no-such-ref", "--all"] {
        let err = base::resolve(repo.root(), bad, GIT_TIMEOUT).unwrap_err();
        assert!(matches!(err, GateError::Config(_)), "{bad}");
    }
    let mut cfg = passing_cfg();
    cfg.gate.base_ref = "no-such-ref".to_string();
    let clock = FixedClock(1);
    let result = run_gate_in(repo.data_dir(), repo.root(), &cfg, GateMode::Scoped, &clock);
    assert!(matches!(result, Err(GateError::Config(_))));
}

/// A git that could not answer is an error, never "no commit yet" (the
/// whole tree would be the change set, and the head lock exempts added
/// files) nor "no such trunk" (committed work would leave the scope).
#[test]
fn a_git_that_cannot_answer_is_an_error_not_a_missing_commit() {
    let outside = tempfile::TempDir::new().unwrap();
    let result = base::resolve(outside.path(), "auto", GIT_TIMEOUT);
    assert!(matches!(result, Err(GateError::Fp(_))), "{result:?}");
}

#[test]
fn repo_without_a_commit_scans_everything() {
    let repo = Repo::empty();
    repo.write("staged.txt", &format!("A={}\n", fake_secret()));
    repo.git(&["add", "staged.txt"]);
    repo.write("loose.txt", &format!("B={}\n", fake_secret()));
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.head_sha, None);
    assert_eq!(receipt.base_sha, None);
    assert_eq!(receipt.status, GateStatus::Blocked);
    let mut files: Vec<&str> = receipt
        .static_findings
        .iter()
        .map(|f| f.file.as_str())
        .collect();
    files.sort_unstable();
    assert_eq!(files, vec!["loose.txt", "staged.txt"]);
}

#[test]
fn large_change_set_does_not_stall_git() {
    let repo = Repo::new();
    // Well past a 64 KiB pipe buffer in both the diff and the file list.
    let big = "let value = compute(1, 2, 3);\n".repeat(6_000);
    repo.write("src/big.rs", &big);
    repo.git(&["add", "src/big.rs"]);
    for index in 0..1500 {
        let name = format!("data/some-long-directory-name/file-number-{index:04}.txt");
        repo.write(&name, "x\n");
    }
    let receipt = repo.gate(&passing_cfg(), GateMode::Scoped);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
}
