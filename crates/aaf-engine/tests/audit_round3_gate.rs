#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round 3 gate, FP, merge cache, and iOS fixes.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{GateConfig, GlobalConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{
    ChangeKind, GateMode, GateStatus, LockStatus, LockTier, LockedChange, StoreStatus,
};
use aaf_domain::ports::Clock;
use aaf_engine::gate::changes::{AddedLines, Change};
use aaf_engine::gate::{
    HiddenPolicy, changes, docs_fast, head_lock, run_gate_policy, static_checks,
};
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};
use aaf_engine::store_compliance;
use tempfile::TempDir;

#[path = "throttle_support.rs"]
mod throttle_support;
use throttle_support::calm_throttle;

const TIMEOUT: Duration = Duration::from_secs(30);

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn scan_one(path: &str, line: &str) -> Vec<String> {
    static_checks::scan(&[AddedLines {
        path: path.to_string(),
        lines: vec![(1, line.to_string())],
    }])
    .unwrap()
    .findings
    .into_iter()
    .map(|f| f.rule)
    .collect()
}

#[test]
fn head_lock_blocks_agent_config_dir_symlink_add_modify_delete_rename() {
    let (dir, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("docs/evil")).unwrap();
    fs::create_dir_all(root.join("docs/evil2")).unwrap();
    fs::write(root.join("docs/evil/settings.json"), "{}\n").unwrap();
    fs::write(root.join("docs/evil2/settings.json"), "{\"v\":2}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    let cfg = RepoConfig::default();
    let expect_blocked = |base: &str, kind: ChangeKind| {
        let rep = head_lock::evaluate(data.path(), root, &cfg, base);
        assert_eq!(rep.status, LockStatus::Blocked);
        assert_eq!(
            rep.locked_changes,
            vec![LockedChange {
                path: ".claude".to_string(),
                change: kind,
                tier: LockTier::Hard,
            }]
        );
    };

    // 1. Add: `ln -s docs/evil .claude` (git mode 120000, path ".claude").
    symlink("docs/evil", root.join(".claude")).unwrap();
    expect_blocked("main", ChangeKind::Added);

    git(root, &["add", ".claude"]);
    git(root, &["commit", "-q", "-m", "add claude link"]);
    let base_link = git(root, &["rev-parse", "HEAD"]);

    // 2. Modify symlink target.
    fs::remove_file(root.join(".claude")).unwrap();
    symlink("docs/evil2", root.join(".claude")).unwrap();
    expect_blocked(&base_link, ChangeKind::Modified);

    // 3. Delete symlink.
    fs::remove_file(root.join(".claude")).unwrap();
    expect_blocked(&base_link, ChangeKind::Deleted);

    // 4. Rename out of `.claude`.
    symlink("docs/evil", root.join(".claude")).unwrap();
    git(root, &["mv", ".claude", "docs/moved"]);
    expect_blocked(&base_link, ChangeKind::Renamed);

    // 5. Rename into `.claude`.
    git(root, &["commit", "-q", "-m", "moved"]);
    let base_moved = git(root, &["rev-parse", "HEAD"]);
    git(root, &["mv", "docs/moved", ".claude"]);
    expect_blocked(&base_moved, ChangeKind::Renamed);
}

#[test]
fn static_checks_closes_star_block_tail_and_non_dash_language_bypass() {
    let err = concat!("catch (e) {", "}");
    let log = concat!("console.", "log(1);");
    let dbg = concat!("db", "g!(x)");

    assert_eq!(
        scan_one("src/a.js", &format!(" * doc */ {err}")),
        vec!["ERR-001"]
    );
    assert_eq!(
        scan_one("src/a.js", &format!(" *\tdoc */ {log}")),
        vec!["LOG-001"]
    );
    assert!(scan_one("src/a.js", &format!(" * {err}")).is_empty());
    assert!(scan_one("src/a.js", &format!(" * {err} */ // ok")).is_empty());

    assert_eq!(
        scan_one("src/a.js", &format!("-- i; {err}")),
        vec!["ERR-001"]
    );
    assert_eq!(scan_one("src/a.rs", &format!("-- {dbg}")), vec!["LOG-001"]);
    assert!(scan_one("db/init.sql", &format!("-- {err}")).is_empty());
    assert!(scan_one("scripts/a.lua", &format!("-- {err}")).is_empty());
    assert!(scan_one("src/Main.hs", &format!("-- {err}")).is_empty());
}

fn assert_unquote_scans_err001(tricky: &str, msg: &str) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("init.txt"), "ok\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let err = concat!("try {} catch (e) {", "}");
    let full = root.join(tricky);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, format!("{err}\n")).unwrap();
    git(root, &["add", tricky]);

    let added =
        changes::diff_added_tracked(root, "HEAD", &static_checks::skip_path, TIMEOUT).unwrap();
    assert_eq!(added.len(), 1, "{msg}");
    assert_eq!(added[0].path, tricky);
    let report = static_checks::scan(&added).unwrap();
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.rule == "ERR-001" && f.file == tricky),
        "{:#?}",
        report.findings
    );
}

#[test]
fn changes_unquote_keeps_trailing_escaped_quote_so_lockfile_skip_is_not_abused() {
    assert_unquote_scans_err001("Cargo.lock\"", "must not be mis-unquoted as Cargo.lock");
}

#[test]
fn changes_unquote_decodes_git_c_escapes_so_backspace_build_is_not_skipped() {
    assert_unquote_scans_err001(
        "\x08uild/leak.js",
        "must not decode \\b to 'b' and skip build/",
    );
}

#[test]
fn docs_fast_blocks_go_embed_directives_and_keeps_unrelated_docs_fast() {
    let globs = GateConfig::default().head_lock_globs;
    let go_embed = concat!("//go:", "embed");
    let rows = [
        (
            format!("{go_embed} docs/guide.md\n"),
            "docs/guide.md",
            false,
        ),
        (format!("{go_embed} docs\n"), "docs/guide.md", false),
        (format!("{go_embed} all:docs\n"), "docs/guide.md", false),
        (format!("{go_embed} sub/docs\n"), "sub/docs/guide.md", false),
        (format!("{go_embed} \"docs\"\n"), "docs/guide.md", false),
        (format!("{go_embed} *.md\n"), "docs/guide.md", false),
        (format!("{go_embed} *\n"), "docs/guide.md", false),
        (format!("{go_embed} all:*\n"), "docs/guide.md", false),
        (format!("{go_embed} web\n"), "docs/guide.md", true),
        (format!("{go_embed} mydocs\n"), "docs/guide.md", true),
    ];
    for (body, doc, expected_fast) in rows {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(Path::new(doc).parent().unwrap())).unwrap();
        fs::write(root.join(doc), "# Doc\n").unwrap();
        fs::write(
            root.join("embed.go"),
            format!("package main\n{body}var fs string\n"),
        )
        .unwrap();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);

        let change = Change {
            kind: ChangeKind::Modified,
            path: doc.to_string(),
            old_path: None,
        };
        let fast = docs_fast::is_docs_only_in_repo(root, &[change], &globs, &[], TIMEOUT).unwrap();
        assert_eq!(fast, expected_fast, "body={body:?}, doc={doc}");
    }
}

#[test]
fn ios_store_compliance_rejects_absolute_and_symlink_escaping_infoplist_file() {
    let outside = TempDir::new().unwrap();
    let bad_plist = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
        <plist version=\"1.0\"><dict><key>NSCameraUsageDescription</key>\
        <string></string></dict></plist>\n";
    let outside_plist = outside.path().join("Info.plist");
    fs::write(&outside_plist, bad_plist).unwrap();

    let assert_ios001_unresolved = |repo: &Path, label: &str| {
        let report = store_compliance::scan(repo, &RepoConfig::default(), true);
        assert_eq!(report.status, StoreStatus::Clear, "{label}: {report:#?}");
        assert_eq!(report.critical, 0, "{label}");
        assert!(
            report.unresolved.iter().any(|u| u.starts_with("IOS-001")),
            "{label}: {:#?}",
            report.unresolved
        );
    };

    let init_pbx = |repo: &Path, setting: &str| {
        fs::create_dir_all(repo.join("App.xcodeproj")).unwrap();
        fs::write(
            repo.join("App.xcodeproj/project.pbxproj"),
            format!("SDKROOT = iphoneos;\n{setting}\n"),
        )
        .unwrap();
    };

    for setting in [
        format!("INFOPLIST_FILE = \"{}\";", outside_plist.display()),
        "INFOPLIST_FILE = \"..\\\\outside\\\\Info.plist\";".to_string(),
    ] {
        let repo = TempDir::new().unwrap();
        init_pbx(repo.path(), &setting);
        assert_ios001_unresolved(repo.path(), &setting);
    }

    // Directory symlink escape: `App -> outside`.
    let dir_escape = TempDir::new().unwrap();
    init_pbx(dir_escape.path(), "INFOPLIST_FILE = App/Info.plist;");
    symlink(outside.path(), dir_escape.path().join("App")).unwrap();
    assert_ios001_unresolved(dir_escape.path(), "dir_symlink_escape");

    // File symlink escape: `App/Info.plist -> outside/Info.plist`.
    let file_escape = TempDir::new().unwrap();
    init_pbx(file_escape.path(), "INFOPLIST_FILE = App/Info.plist;");
    fs::create_dir_all(file_escape.path().join("App")).unwrap();
    symlink(&outside_plist, file_escape.path().join("App/Info.plist")).unwrap();
    assert_ios001_unresolved(file_escape.path(), "file_symlink_escape");
}

struct MockGh {
    pr: PrInfo,
    merged: AtomicUsize,
}

impl GhClient for MockGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
    }
    fn pr_checks(&self, _repo_root: &Path, _pr_number: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![])
    }
    fn merge_pr(
        &self,
        _repo_root: &Path,
        _pr_number: u64,
        _expect_head: &str,
        _strategy: MergeStrategy,
        _subject: Option<&str>,
        _body_file: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn mock_pr(number: u64, branch: &str, head: &str) -> MockGh {
    MockGh {
        pr: PrInfo {
            number,
            state: "OPEN".to_string(),
            head_ref_oid: head.to_string(),
            head_ref_name: branch.to_string(),
            base_ref_name: "main".to_string(),
            mergeable: "MERGEABLE".to_string(),
            is_draft: false,
            title: format!("PR #{number}"),
            url: format!("https://example.invalid/pr/{number}"),
            ..Default::default()
        },
        merged: AtomicUsize::new(0),
    }
}

#[test]
fn pr_merge_hits_common_receipt_cache_with_auto_base_and_rejects_same_account() {
    let (bare, work, data) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    let counter_dir = TempDir::new().unwrap();
    let counter_file = counter_dir.path().join("runs.log");

    git(bare.path(), &["init", "--bare", "-b", "main"]);
    git(work.path(), &["init", "-b", "main"]);
    git(
        work.path(),
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    for d in [".autofactory", "src", "tests"] {
        fs::create_dir_all(work.path().join(d)).unwrap();
    }
    let cfg_toml = format!(
        "[commands]\ntest_scoped = \"sh -c 'echo run >> {}'\"\n\
         [gate]\nbase_ref = \"auto\"\n\
         [cross_audit]\nrequire_different_provider = false\n",
        counter_file.display()
    );
    fs::write(work.path().join(".autofactory/config.toml"), &cfg_toml).unwrap();
    fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    fs::write(
        work.path().join("tests/basic_test.rs"),
        "#[test] fn t() { assert_eq!(1, 1); }\n",
    )
    .unwrap();
    git(work.path(), &["add", "."]);
    git(work.path(), &["commit", "-m", "init"]);
    git(work.path(), &["push", "-u", "origin", "main"]);

    let repo_cfg: RepoConfig = toml::from_str(&cfg_toml).unwrap();
    let machine = load_free_machine();
    let throttle = calm_throttle(&machine);
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let runs = || {
        fs::read_to_string(&counter_file)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    let make_req = |pr_number: u64, expect_head: &str, dry_run: bool| PrMergeRequest {
        repo_root: work.path().to_path_buf(),
        data_dir: data.path().to_path_buf(),
        pr_number,
        expect_head: expect_head.to_string(),
        remote: "origin".to_string(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: 0,
        require_checks: false,
        lock_timeout: Duration::from_secs(10),
        dry_run,
        machine: machine.clone(),
        global_config: None,
    };

    let push_branch = |branch: &str, file: &str, body: &str| {
        git(work.path(), &["checkout", "-b", branch, "origin/main"]);
        fs::write(work.path().join(file), body).unwrap();
        git(work.path(), &["add", "-A"]);
        git(work.path(), &["commit", "-m", branch]);
        let head = git(work.path(), &["rev-parse", "HEAD"]);
        git(work.path(), &["push", "-u", "origin", branch]);
        head
    };
    let gate_clear = |cfg: &RepoConfig, ts: i64| {
        let g = run_gate_policy(
            data.path(),
            work.path(),
            cfg,
            GateMode::Scoped,
            &FixedClock(ts),
            &throttle,
            HiddenPolicy::Remove,
        )
        .unwrap();
        assert_eq!(g.receipt.status, GateStatus::Clear);
        g
    };
    let scratch_dir = data.path().join("merge-scratch");
    let assert_dry_run_hit =
        |pr: u64, branch: &str, head: &str, ts: i64, expected_runs: usize, msg: &str| {
            let gh = mock_pr(pr, branch, head);
            let out = merge_pr(&make_req(pr, head, true), &gh, &FixedClock(ts), &noop).unwrap();
            assert!(matches!(out, PrMergeOutcome::DryRunClear { .. }), "{out:?}");
            assert_eq!(runs(), expected_runs, "{msg}");
            assert!(!scratch_dir.exists(), "{msg} must not create merge-scratch");
        };

    // 1. Worktree gate with base_ref = "auto" populates common receipt -> merge hits cache!
    let head1 = push_branch("feat/cache-hit", "src/lib.rs", "pub fn v() -> i32 { 2 }\n");
    let _ = gate_clear(&repo_cfg, 10);
    assert_eq!(runs(), 1);
    git(work.path(), &["checkout", "main"]);
    assert_dry_run_hit(
        101,
        "feat/cache-hit",
        &head1,
        11,
        1,
        "OPT 11.2 fast path: merge must reuse common receipt without re-running tests",
    );

    // 1b. Empty PR (head == base) also blocks immediately with zero scratch worktree.
    let base_head = git(work.path(), &["rev-parse", "main"]);
    let gh_empty = mock_pr(100, "main", &base_head);
    let out_empty = merge_pr(
        &make_req(100, &base_head, true),
        &gh_empty,
        &FixedClock(12),
        &noop,
    )
    .unwrap();
    assert!(matches!(out_empty, PrMergeOutcome::Blocked { ref code, .. } if code == "empty_pr"));
    assert!(
        !scratch_dir.exists(),
        "empty_pr must not create merge-scratch"
    );

    // 2. Tier-1 require_different_provider = true receipt ALSO hits merge cache via alt_config_hash!
    let head2 = push_branch("feat/tier1-hit", "src/lib.rs", "pub fn v() -> i32 { 3 }\n");
    let mut tier1_cfg = repo_cfg.clone();
    tier1_cfg.cross_audit.require_different_provider = true;
    let _ = gate_clear(&tier1_cfg, 20);
    assert_eq!(runs(), 2);
    git(work.path(), &["checkout", "main"]);
    assert_dry_run_hit(
        102,
        "feat/tier1-hit",
        &head2,
        21,
        2,
        "merge must hit alt_config_hash receipt too",
    );

    // 3. Cached Clear receipt with same_account=true approval:
    //    - Under require_different_provider=true -> MUST miss & block merge!
    //    - Under require_different_provider=false (solo dev, Tier-2 test) -> hits cache & merges!
    let head3 = push_branch(
        "feat/same-acct",
        "tests/basic_test.rs",
        "#[test] fn t() { assert_eq!(2, 2); }\n",
    );
    head_lock::approve(
        data.path(),
        work.path(),
        &repo_cfg,
        "self",
        true,
        &FixedClock(30),
    )
    .unwrap();
    let g3 = gate_clear(&repo_cfg, 31);
    assert_eq!(g3.receipt.head_lock.status, LockStatus::Approved);
    git(work.path(), &["checkout", "main"]);

    let gh3 = mock_pr(103, "feat/same-acct", &head3);
    let mut strict_req = make_req(103, &head3, false);
    let mut global_strict = GlobalConfig::default();
    global_strict.cross_audit.require_different_provider = Some(true);
    strict_req.global_config = Some(global_strict);
    let out3_strict = merge_pr(&strict_req, &gh3, &FixedClock(32), &noop).unwrap();
    match out3_strict {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("expected Blocked(cross_approval_required), got {other:?}"),
    }
    assert_eq!(gh3.merged.load(Ordering::SeqCst), 0);

    let out3_solo = merge_pr(&make_req(103, &head3, false), &gh3, &FixedClock(33), &noop).unwrap();
    assert!(
        matches!(out3_solo, PrMergeOutcome::Merged { pr_number: 103, .. }),
        "{out3_solo:?}"
    );
    assert_eq!(gh3.merged.load(Ordering::SeqCst), 1);
}

/// The real machine probe with the CPU-load wait disabled, so these tests do
/// not stall (up to `heavy_wait_secs`) when the host is busy.
fn load_free_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..MachineConfig::default()
    }
}

#[test]
fn probe_git_layout_and_tree_fp_with_layout_match_primary_and_worktree() {
    let dir = TempDir::new().unwrap();
    let primary = dir.path();
    git(primary, &["init", "-q", "-b", "main"]);
    fs::write(primary.join("a.txt"), "hello\n").unwrap();
    git(primary, &["add", "-A"]);
    git(primary, &["commit", "-q", "-m", "init"]);

    let canon_primary = fs::canonicalize(primary).unwrap();
    let layout = aaf_fp::probe_git_layout(&canon_primary, TIMEOUT).unwrap();
    assert_eq!(layout.git_dir, canon_primary.join(".git"));
    assert_eq!(layout.common_dir, canon_primary.join(".git"));
    assert_eq!(layout.index, canon_primary.join(".git/index"));
    assert_eq!(layout.objects, canon_primary.join(".git/objects"));
    assert_eq!(layout.common_repo_root(), canon_primary.as_path());
    assert_eq!(
        aaf_fp::tree_fp_with_layout(&canon_primary, &layout, TIMEOUT).unwrap(),
        aaf_fp::tree_fp(&canon_primary, TIMEOUT).unwrap()
    );

    let wt_parent = TempDir::new().unwrap();
    let wt = wt_parent.path().join("linked-wt");
    git(
        primary,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );
    let canon_wt = fs::canonicalize(&wt).unwrap();
    let wt_layout = aaf_fp::probe_git_layout(&canon_wt, TIMEOUT).unwrap();
    assert_ne!(wt_layout.git_dir, wt_layout.common_dir);
    assert_eq!(wt_layout.common_dir, canon_primary.join(".git"));
    assert_eq!(wt_layout.objects, canon_primary.join(".git/objects"));
    assert_eq!(wt_layout.common_repo_root(), canon_primary.as_path());
    assert_eq!(
        aaf_fp::tree_fp_with_layout(&canon_wt, &wt_layout, TIMEOUT).unwrap(),
        aaf_fp::tree_fp(&canon_wt, TIMEOUT).unwrap()
    );
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", wt.to_str().unwrap()])
        .current_dir(primary)
        .output();
}
