#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round 3 gate, FP, merge cache, and iOS fixes.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{GateConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{ChangeKind, GateMode, GateStatus, LockStatus, LockedChange, StoreStatus};
use aaf_domain::ports::Clock;
use aaf_engine::gate::changes::{AddedLines, Change};
use aaf_engine::gate::{
    HiddenPolicy, changes, docs_fast, head_lock, run_gate_policy, static_checks,
};
use aaf_engine::git::{
    GhClient, GitError, MergeStrategy, PrCheck, PrInfo, PrMergeOutcome, PrMergeRequest, merge_pr,
};
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

#[test]
fn changes_unquote_keeps_trailing_escaped_quote_so_lockfile_skip_is_not_abused() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("init.txt"), "ok\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let err = concat!("try {} catch (e) {", "}");
    let tricky = "Cargo.lock\"";
    fs::write(root.join(tricky), format!("{err}\n")).unwrap();
    git(root, &["add", tricky]);

    let added =
        changes::diff_added_tracked(root, "HEAD", &static_checks::skip_path, TIMEOUT).unwrap();
    assert_eq!(added.len(), 1, "must not be mis-unquoted as Cargo.lock");
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
fn changes_unquote_decodes_git_c_escapes_so_backspace_build_is_not_skipped() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("init.txt"), "ok\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    let err = concat!("try {} catch (e) {", "}");
    let tricky_dir = "\x08uild";
    let tricky = "\x08uild/leak.js";
    fs::create_dir_all(root.join(tricky_dir)).unwrap();
    fs::write(root.join(tricky), format!("{err}\n")).unwrap();
    git(root, &["add", tricky]);

    let added =
        changes::diff_added_tracked(root, "HEAD", &static_checks::skip_path, TIMEOUT).unwrap();
    assert_eq!(added.len(), 1, "must not decode \\b to 'b' and skip build/");
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

    for setting in [
        format!("INFOPLIST_FILE = \"{}\";", outside_plist.display()),
        "INFOPLIST_FILE = \"..\\\\outside\\\\Info.plist\";".to_string(),
    ] {
        let repo = TempDir::new().unwrap();
        fs::create_dir_all(repo.path().join("App.xcodeproj")).unwrap();
        fs::write(
            repo.path().join("App.xcodeproj/project.pbxproj"),
            format!("SDKROOT = iphoneos;\n{setting}\n"),
        )
        .unwrap();
        assert_ios001_unresolved(repo.path(), &setting);
    }

    // Directory symlink escape: `App -> outside`.
    let dir_escape = TempDir::new().unwrap();
    fs::create_dir_all(dir_escape.path().join("App.xcodeproj")).unwrap();
    symlink(outside.path(), dir_escape.path().join("App")).unwrap();
    fs::write(
        dir_escape.path().join("App.xcodeproj/project.pbxproj"),
        "SDKROOT = iphoneos;\nINFOPLIST_FILE = App/Info.plist;\n",
    )
    .unwrap();
    assert_ios001_unresolved(dir_escape.path(), "dir_symlink_escape");

    // File symlink escape: `App/Info.plist -> outside/Info.plist`.
    let file_escape = TempDir::new().unwrap();
    fs::create_dir_all(file_escape.path().join("App.xcodeproj")).unwrap();
    fs::create_dir_all(file_escape.path().join("App")).unwrap();
    symlink(&outside_plist, file_escape.path().join("App/Info.plist")).unwrap();
    fs::write(
        file_escape.path().join("App.xcodeproj/project.pbxproj"),
        "SDKROOT = iphoneos;\nINFOPLIST_FILE = App/Info.plist;\n",
    )
    .unwrap();
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
        lock_timeout: Duration::from_secs(10),
        dry_run,
        machine: machine.clone(),
    };

    // 1. Worktree gate with base_ref = "auto" populates common receipt -> merge hits cache!
    git(
        work.path(),
        &["checkout", "-b", "feat/cache-hit", "origin/main"],
    );
    fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-m", "feat"]);
    let head1 = git(work.path(), &["rev-parse", "HEAD"]);
    git(work.path(), &["push", "-u", "origin", "feat/cache-hit"]);
    let g1 = run_gate_policy(
        data.path(),
        work.path(),
        &repo_cfg,
        GateMode::Scoped,
        &FixedClock(10),
        &throttle,
        HiddenPolicy::Remove,
    )
    .unwrap();
    assert_eq!(g1.receipt.status, GateStatus::Clear);
    assert_eq!(runs(), 1);
    git(work.path(), &["checkout", "main"]);

    let gh1 = mock_pr(101, "feat/cache-hit", &head1);
    let out1 = merge_pr(&make_req(101, &head1, true), &gh1, &FixedClock(11), &noop).unwrap();
    assert!(
        matches!(out1, PrMergeOutcome::DryRunClear { .. }),
        "{out1:?}"
    );
    assert_eq!(
        runs(),
        1,
        "merge must reuse common receipt without re-running tests"
    );
    let scratch_dir = data.path().join("merge-scratch");
    assert!(
        !scratch_dir.exists(),
        "OPT 11.2 fast path must not create merge-scratch"
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
    git(
        work.path(),
        &["checkout", "-b", "feat/tier1-hit", "origin/main"],
    );
    fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 3 }\n").unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-m", "feat tier1"]);
    let head2 = git(work.path(), &["rev-parse", "HEAD"]);
    git(work.path(), &["push", "-u", "origin", "feat/tier1-hit"]);
    let mut tier1_cfg = repo_cfg.clone();
    tier1_cfg.cross_audit.require_different_provider = true;
    let g2 = run_gate_policy(
        data.path(),
        work.path(),
        &tier1_cfg,
        GateMode::Scoped,
        &FixedClock(20),
        &throttle,
        HiddenPolicy::Remove,
    )
    .unwrap();
    assert_eq!(g2.receipt.status, GateStatus::Clear);
    assert_eq!(runs(), 2);
    git(work.path(), &["checkout", "main"]);

    let gh2 = mock_pr(102, "feat/tier1-hit", &head2);
    let out2 = merge_pr(&make_req(102, &head2, true), &gh2, &FixedClock(21), &noop).unwrap();
    assert!(
        matches!(out2, PrMergeOutcome::DryRunClear { .. }),
        "{out2:?}"
    );
    assert_eq!(runs(), 2, "merge must hit alt_config_hash receipt too");
    assert!(
        !scratch_dir.exists(),
        "alt_config_hash hit must avoid merge-scratch"
    );

    // 3. Cached Clear receipt with same_account=true approval MUST miss & block merge!
    git(
        work.path(),
        &["checkout", "-b", "feat/same-acct", "origin/main"],
    );
    fs::write(
        work.path().join("tests/basic_test.rs"),
        "#[test] fn t() { assert_eq!(2, 2); }\n",
    )
    .unwrap();
    git(work.path(), &["add", "-A"]);
    git(work.path(), &["commit", "-m", "edit test"]);
    let head3 = git(work.path(), &["rev-parse", "HEAD"]);
    git(work.path(), &["push", "-u", "origin", "feat/same-acct"]);
    head_lock::approve(
        data.path(),
        work.path(),
        &repo_cfg,
        "self",
        true,
        &FixedClock(30),
    )
    .unwrap();
    let g3 = run_gate_policy(
        data.path(),
        work.path(),
        &repo_cfg,
        GateMode::Scoped,
        &FixedClock(31),
        &throttle,
        HiddenPolicy::Remove,
    )
    .unwrap();
    assert_eq!(g3.receipt.status, GateStatus::Clear);
    assert_eq!(g3.receipt.head_lock.status, LockStatus::Approved);
    git(work.path(), &["checkout", "main"]);

    let gh3 = mock_pr(103, "feat/same-acct", &head3);
    let out3 = merge_pr(&make_req(103, &head3, false), &gh3, &FixedClock(32), &noop).unwrap();
    match out3 {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("expected Blocked(cross_approval_required), got {other:?}"),
    }
    assert_eq!(gh3.merged.load(Ordering::SeqCst), 0);
}

/// The real machine probe with the CPU-load wait disabled, so these tests do
/// not stall (up to `heavy_wait_secs`) when the host is busy.
fn load_free_machine() -> MachineConfig {
    MachineConfig {
        max_load_percent: 0,
        ..MachineConfig::default()
    }
}
