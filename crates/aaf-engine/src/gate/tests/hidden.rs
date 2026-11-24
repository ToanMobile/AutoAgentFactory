//! What the gate's commands read from disk must be what the receipt signs.
//! Git offers three ways to keep a file on disk out of `git add -A`, and
//! with it out of the fingerprint and of the head lock's diff: an index flag
//! (skip-worktree, assume-unchanged), `info/exclude`, and an excludes file
//! named by the configuration. The agent can write all three from inside
//! its worktree. Real git, temp repositories.
//!
//! An index flag blocks everywhere. The ignored files depend on who runs
//! the gate ([`HiddenPolicy`]): a person's own checkout gets a warning in
//! the receipt, an agent's work tree gets them deleted.

use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{GateMode, GateStatus, Receipt};

use super::super::{GateRun, HiddenPolicy, run_gate_policy};
use super::support::{FixedClock, Repo, passing_cfg};
use crate::throttle::tests::support::Rig;

const NEUTERED: &str = "fn checks() {\n    assert!(true);\n}\n";
const PLANTED: &str = "collect_ignore = ['tests']\n";

fn exclude(repo: &Repo, pattern: &str) {
    let path = repo.path(".git/info/exclude");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(pattern);
    text.push('\n');
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn run(repo: &Repo, cfg: &RepoConfig, policy: HiddenPolicy) -> GateRun {
    let rig = Rig::calm();
    let throttle = rig.throttle();
    run_gate_policy(
        repo.data_dir(),
        repo.root(),
        cfg,
        GateMode::Scoped,
        &FixedClock(1_000),
        &throttle,
        policy,
    )
    .unwrap()
}

/// The gate a person runs in their own checkout.
fn gate(repo: &Repo) -> Receipt {
    repo.gate(&passing_cfg(), GateMode::Scoped)
}

/// The gate the factory runs on an agent's work tree.
fn strict(repo: &Repo, cfg: &RepoConfig) -> Receipt {
    run(repo, cfg, HiddenPolicy::Remove).receipt
}

fn cfg_with_test(test_scoped: &str) -> RepoConfig {
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = test_scoped.to_string();
    cfg
}

fn reason_naming<'a>(receipt: &'a Receipt, file: &str) -> Option<&'a String> {
    receipt.reasons.iter().find(|r| r.contains(file))
}

/// The security review's scenario in a person's checkout: the ignored half
/// is their environment and is named; the flagged half blocks.
#[test]
fn default_gate_warns_about_a_hidden_conftest_and_blocks_on_a_skip_worktree_test() {
    let repo = Repo::new();
    exclude(&repo, "conftest.py");
    repo.write("conftest.py", PLANTED);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    let receipt = gate(&repo);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(!receipt.commands.is_empty());
    let warning = reason_naming(&receipt, "conftest.py").expect("names the hidden file");
    assert!(
        warning.starts_with("warning: work tree: 1 file(s)"),
        "{warning}"
    );
    assert!(warning.contains("info/exclude"), "{warning}");
    assert!(repo.path("conftest.py").exists());

    repo.git(&["update-index", "--skip-worktree", "tests/a_test.rs"]);
    repo.write("tests/a_test.rs", NEUTERED);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    let receipt = gate(&repo);
    assert_eq!(receipt.status, GateStatus::Blocked, "{:?}", receipt.reasons);
    let flagged = reason_naming(&receipt, "tests/a_test.rs").expect("names the test file");
    assert!(flagged.contains("skip-worktree"), "{flagged}");
    // Nothing of the repository ran on a test nobody signed.
    assert!(receipt.commands.is_empty());
}

#[test]
fn assume_unchanged_file_on_disk_blocks_the_gate() {
    let repo = Repo::new();
    repo.git(&["update-index", "--assume-unchanged", "tests/a_test.rs"]);
    repo.write("tests/a_test.rs", NEUTERED);

    for policy in [HiddenPolicy::Warn, HiddenPolicy::Remove] {
        let receipt = run(&repo, &passing_cfg(), policy).receipt;
        assert_eq!(receipt.status, GateStatus::Blocked);
        let reason = reason_naming(&receipt, "tests/a_test.rs").expect("names the file");
        assert!(reason.contains("assume-unchanged"), "{reason}");
        assert!(repo.path("tests/a_test.rs").exists());
    }
}

#[test]
fn clear_receipt_is_reused_beside_ignored_files_and_not_beside_a_flagged_file() {
    let repo = Repo::new();
    let cfg = passing_cfg();
    assert!(!run(&repo, &cfg, HiddenPolicy::Warn).reused);

    // The operator's own environment: the fingerprint and the verdict stand.
    exclude(&repo, "conftest.py");
    repo.write("conftest.py", PLANTED);
    let again = run(&repo, &cfg, HiddenPolicy::Warn);
    assert!(again.reused);
    assert_eq!(again.receipt.status, GateStatus::Clear);

    // Same fingerprint, another test: the old verdict says nothing about it.
    repo.git(&["update-index", "--skip-worktree", "tests/a_test.rs"]);
    repo.write("tests/a_test.rs", NEUTERED);
    let flagged = run(&repo, &cfg, HiddenPolicy::Warn);
    assert!(!flagged.reused);
    assert_eq!(flagged.receipt.status, GateStatus::Blocked);
}

fn release_cfg(test_full: &str) -> RepoConfig {
    let mut cfg = passing_cfg();
    cfg.commands.test_full = test_full.to_string();
    cfg
}

/// A release receipt unlocks a store submission of the tree on disk, so it
/// must not be signed beside a file nobody fingerprinted: what the scoped
/// and full gates only name, the release gate refuses.
#[test]
fn release_gate_blocks_on_a_file_ignored_only_outside_the_tree() {
    let repo = Repo::new();
    exclude(&repo, "conftest.py");
    repo.write("conftest.py", PLANTED);
    let cfg = release_cfg("touch ../ran");

    for mode in [GateMode::Scoped, GateMode::Full] {
        let receipt = repo.gate(&cfg, mode);
        assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
        let warning = reason_naming(&receipt, "conftest.py").expect("names the file");
        assert!(warning.starts_with("warning: work tree: "), "{warning}");
    }

    // The full receipt above is `Clear` for this very tree and is not
    // handed back for a release.
    let release = repo.gate(&cfg, GateMode::Release);
    assert_eq!(release.status, GateStatus::Blocked, "{:?}", release.reasons);
    let reason = reason_naming(&release, "conftest.py").expect("names the file");
    assert!(reason.starts_with("work tree: 1 file(s)"), "{reason}");
    assert!(reason.contains("info/exclude"), "{reason}");
    assert!(release.commands.is_empty());
    // A person's checkout: named, never deleted.
    assert!(repo.path("conftest.py").exists());

    // Without the file the same tree is released.
    fs::remove_file(repo.path("conftest.py")).unwrap();
    let release = repo.gate(&cfg, GateMode::Release);
    assert_eq!(release.status, GateStatus::Clear, "{:?}", release.reasons);
    assert!(!release.commands.is_empty());
}

#[test]
fn release_gate_blocks_on_a_hidden_file_its_own_commands_left() {
    let repo = Repo::new();
    let leaves_a_cache = "mkdir -p .cache && echo '*' > .cache/.gitignore && echo v > .cache/v";

    let full = repo.gate(&release_cfg(leaves_a_cache), GateMode::Full);
    assert_eq!(full.status, GateStatus::Clear, "{:?}", full.reasons);
    fs::remove_dir_all(repo.path(".cache")).unwrap();

    let release = repo.gate(&release_cfg(leaves_a_cache), GateMode::Release);
    assert_eq!(release.status, GateStatus::Blocked, "{:?}", release.reasons);
    assert!(reason_naming(&release, ".cache/").is_some());
    // The commands did run: the file is theirs.
    assert!(!release.commands.is_empty());
    assert!(repo.path(".cache/v").exists());
}

/// What an honest checkout looks like passes without a word.
#[test]
fn files_ignored_by_a_tracked_gitignore_and_unused_exclude_patterns_pass() {
    let repo = Repo::new();
    repo.write(".gitignore", "target/\n*.log\n");
    repo.write("sub/.gitignore", "cache/\n");
    repo.commit("ignore rules");
    repo.write("target/debug/out.bin", "built\n");
    repo.write("run.log", "log\n");
    repo.write("sub/cache/blob", "cached\n");
    // A pattern of the developer's own that hides nothing on disk.
    exclude(&repo, "scratch/");
    // Not committed yet, and in the fingerprint all the same.
    repo.write("docs/.gitignore", "*.tmp\n");
    repo.write("docs/draft.tmp", "draft\n");

    let receipt = gate(&repo);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(receipt.reasons.is_empty(), "{:?}", receipt.reasons);
}

#[test]
fn sparse_checkout_passes() {
    let repo = Repo::new();
    repo.git(&["sparse-checkout", "set", "src"]);
    assert!(!repo.path("tests").exists());
    assert!(repo.git(&["ls-files", "-v"]).contains("S tests/a_test.rs"));

    for policy in [HiddenPolicy::Warn, HiddenPolicy::Remove] {
        let receipt = run(&repo, &passing_cfg(), policy).receipt;
        assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    }
}

#[test]
fn strict_gate_removes_planted_files_before_the_command_runs() {
    let repo = Repo::new();
    exclude(&repo, "conftest.py");
    repo.write("conftest.py", PLANTED);
    // An untracked ignore file that hides itself and its neighbours.
    repo.write("tools/.gitignore", "*\n");
    repo.write("tools/conftest.py", PLANTED);
    // An excludes file the repository's own configuration names.
    let outside = tempfile::tempdir().unwrap();
    let list = outside.path().join("ignore");
    fs::write(&list, "sitecustomize.py\n").unwrap();
    repo.git(&["config", "core.excludesFile", list.to_str().unwrap()]);
    repo.write("sitecustomize.py", "import os\n");
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    // The command fails if any of them is still there when it runs.
    let cfg =
        cfg_with_test("test ! -e conftest.py && test ! -e tools && test ! -e sitecustomize.py");
    let receipt = strict(&repo, &cfg);

    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    let note = reason_naming(&receipt, "conftest.py").expect("lists what it removed");
    assert!(
        note.contains("removed 3 ignored file(s) before the commands ran"),
        "{note}"
    );
    assert!(
        note.contains("tools/") && note.contains("sitecustomize.py"),
        "{note}"
    );
    assert!(!repo.path("conftest.py").exists());
    assert!(!repo.path("tools").exists());
    // Tracked files are nobody's to remove.
    assert!(repo.path("tests/a_test.rs").exists());
}

#[test]
fn strict_gate_removes_a_cache_the_command_left_and_the_next_gate_is_clear() {
    let repo = Repo::new();
    // What pytest does on every run: a cache directory that ignores itself.
    let cfg = cfg_with_test("mkdir -p .cache && echo '*' > .cache/.gitignore && echo v > .cache/v");

    let first = strict(&repo, &cfg);
    assert_eq!(first.status, GateStatus::Clear, "{:?}", first.reasons);
    let note = reason_naming(&first, ".cache/").expect("lists what it removed");
    assert!(note.contains("that the commands left behind"), "{note}");
    assert!(!repo.path(".cache").exists());

    let second = strict(&repo, &cfg);
    assert_eq!(second.status, GateStatus::Clear, "{:?}", second.reasons);
}

#[test]
fn strict_gate_keeps_what_a_tracked_gitignore_covers() {
    let repo = Repo::new();
    repo.write(".gitignore", "build/\nlocal.properties\n");
    repo.commit("ignore rules");
    repo.write("build/out.bin", "an hour of compiling\n");
    repo.write("local.properties", "sdk.dir=/somewhere\n");
    exclude(&repo, "conftest.py");
    repo.write("conftest.py", PLANTED);

    let receipt = strict(&repo, &passing_cfg());

    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(repo.path("build/out.bin").exists());
    assert!(repo.path("local.properties").exists());
    assert!(!repo.path("conftest.py").exists());
    assert!(reason_naming(&receipt, "build").is_none());
}

#[test]
fn strict_gate_removes_a_planted_link_and_leaves_its_target() {
    let repo = Repo::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("kept.txt"), "not the agent's\n").unwrap();
    exclude(&repo, "vendor");
    std::os::unix::fs::symlink(outside.path(), repo.path("vendor")).unwrap();

    let receipt = strict(&repo, &passing_cfg());

    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(fs::symlink_metadata(repo.path("vendor")).is_err());
    assert!(outside.path().join("kept.txt").exists());
}

#[test]
fn strict_gate_blocks_and_names_the_path_it_could_not_remove() {
    use std::os::unix::fs::PermissionsExt;
    let repo = Repo::new();
    exclude(&repo, "locked/planted.py");
    repo.write("locked/planted.py", PLANTED);
    let locked = repo.path("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let receipt = strict(&repo, &cfg_with_test("touch ../ran"));
    // So the temp directory can be cleaned up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(receipt.status, GateStatus::Blocked, "{:?}", receipt.reasons);
    let reason = reason_naming(&receipt, "locked/planted.py").expect("names the path");
    assert!(reason.contains("could not be removed"), "{reason}");
    assert!(receipt.commands.is_empty());
}

/// Git never lists a path below a link, so this cannot happen through the
/// gate as long as git behaves; the deletion does not rely on that.
#[test]
fn deletion_refuses_a_path_that_leaves_the_work_tree() {
    use super::super::hidden::delete;
    let repo = Repo::new();
    let root = fs::canonicalize(repo.root()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("kept.txt"), "not the agent's\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("way-out")).unwrap();

    for entry in ["way-out/kept.txt", "../kept.txt", "/etc/hosts", "", "."] {
        assert!(delete(&root, entry).is_err(), "{entry:?}");
    }
    assert!(outside.path().join("kept.txt").exists());
    // The link itself is an entry of the work tree.
    assert!(delete(&root, "way-out").is_ok());
    assert!(outside.path().join("kept.txt").exists());
}

/// The inspection runs beside the git layout and base lookups (audit a13
/// O11-3); when every one of them fails, the error is still the
/// inspection's, as it was when they ran one after another.
#[test]
fn outside_a_repository_the_inspection_error_comes_first() {
    let outside = tempfile::TempDir::new().unwrap();
    let data = tempfile::TempDir::new().unwrap();
    let root = fs::canonicalize(outside.path()).unwrap();
    let expected = super::super::hidden::inspect(
        &root,
        HiddenPolicy::Warn,
        super::super::hidden::When::BeforeCommands,
        false,
    )
    .unwrap_err()
    .to_string();
    let rig = Rig::calm();
    let err = run_gate_policy(
        data.path(),
        &root,
        &passing_cfg(),
        GateMode::Scoped,
        &FixedClock(1_000),
        &rig.throttle(),
        HiddenPolicy::Warn,
    )
    .unwrap_err();
    assert_eq!(err.to_string(), expected);
}
