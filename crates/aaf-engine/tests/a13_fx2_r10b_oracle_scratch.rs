#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Round-10b (fixer 2): the red->green oracle's scratch worktree lives under
//! the data dir, never `$TMPDIR` (R10-E10-2): today the warm slot
//! `<warm-slots>/oracle/<common_id>/slot-0`, wiped after every run and
//! reused, never accumulating worktrees; `aaf gc` still sweeps legacy
//! `<data_dir>/oracle-scratch/<common_id>-<pid>-<seq>` dirs an interrupted
//! gate left behind. A block-mode compile-red oracle run blocks with
//! guidance for the agent (O12-R10-7).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::{GateMode, GateStatus, Receipt};
use aaf_domain::ports::Clock;
use aaf_engine::config::{ConfigInputs, ConfigOverlay, REPO_CONFIG_REL, load};
use aaf_engine::gate::{run_gate_throttled, stats_repo_id};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::warm_slots_root;
use aaf_engine::throttle::{
    LoadProbe, Pressure, PriorityTools, SAMPLE_EVERY, SystemGroup, ThreadSleeper, Throttle,
};
use tempfile::TempDir;

struct Calm;

impl LoadProbe for Calm {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }
    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }
    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
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
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(test_cmd: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    let cmd_toml = toml::Value::String(test_cmd.to_string()).to_string();
    let cfg = format!(
        "[commands]\ntest_scoped = {cmd_toml}\ntest_full = {cmd_toml}\n\n[gate]\nbase_ref = \"main\"\nred_green_oracle = \"block\"\n"
    );
    fs::write(root.join(REPO_CONFIG_REL), cfg).unwrap();
    fs::write(root.join("src/app.sh"), "#!/bin/sh\necho 0\n").unwrap();
    fs::write(root.join("tests/test_smoke.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    git_out(root, &["add", "-A"]);
    git_out(root, &["commit", "-q", "-m", "base"]);
    git_out(root, &["checkout", "-q", "-b", "feat/work"]);
    dir
}

fn run_gate(repo: &Path, data: &Path) -> Receipt {
    let no_env = |_: &str| None;
    let effective = load(&ConfigInputs {
        global_config: None,
        repo_root: repo.to_path_buf(),
        env: &no_env,
        cli: ConfigOverlay::default(),
    })
    .unwrap();
    let machine = MachineConfig::default();
    let throttle = Throttle {
        machine: &machine,
        probe: &Calm,
        sleeper: &ThreadSleeper,
        control: &SystemGroup,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::detect(),
        jitter_seed: 1,
        sample_every: SAMPLE_EVERY,
    };
    run_gate_throttled(
        data,
        repo,
        &effective.repo,
        GateMode::Scoped,
        &FixedClock(2000),
        &throttle,
    )
    .unwrap_or_else(|err| panic!("run_gate returned Err (no receipt): {err}"))
}

fn worktree_count(repo: &Path) -> usize {
    git_out(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

fn reaped_child_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn age(dir: &Path, by: Duration) {
    let file = fs::File::open(dir).unwrap();
    file.set_modified(SystemTime::now() - by).unwrap();
}

#[test]
fn oracle_scratch_is_a_wiped_warm_slot_under_the_data_dir_and_never_accumulates() {
    let data = TempDir::new().unwrap();
    let data_root = fs::canonicalize(data.path()).unwrap();
    let out = TempDir::new().unwrap();
    let marker = fs::canonicalize(out.path()).unwrap().join("cwds.txt");
    let cmd = format!(
        "pwd -P >> '{}'; for f in tests/test_*.sh; do [ -f \"$f\" ] || continue; sh \"$f\" || exit 1; done",
        marker.display()
    );
    let repo = init_repo(&cmd);
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(
        repo.path().join("tests/test_app.sh"),
        "#!/bin/sh\nif grep -q 'echo 0' src/app.sh; then echo 'FAIL: app'; exit 1; fi\necho ok\n",
    )
    .unwrap();

    let id = stats_repo_id(repo.path());
    let slot_parent = warm_slots_root(&data_root).join("oracle").join(&id);
    assert!(slot_parent.starts_with(&data_root), "{slot_parent:?}");
    for round in 1..=2 {
        let r = run_gate(repo.path(), &data_root);
        assert!(
            r.reasons.iter().any(|m| m.contains("red_green=proven")),
            "round {round}: {:?}",
            r.reasons
        );
        let cwds = fs::read_to_string(&marker).unwrap();
        let oracle_cwd = cwds
            .lines()
            .rev()
            .find(|l| Path::new(l).starts_with(&slot_parent))
            .unwrap_or_else(|| panic!("oracle did not run under {slot_parent:?}: {cwds}"));
        // The slot stays for reuse, wiped: the reverted production file and
        // overlaid tests are gone again.
        let left = git_out(
            Path::new(oracle_cwd),
            &["status", "--porcelain", "--ignored"],
        );
        assert!(left.is_empty(), "round {round}: slot not wiped: {left}");
        // One reusable slot at most: worktrees never accumulate per run.
        assert_eq!(worktree_count(repo.path()), 2, "round {round}");
    }
}

#[test]
fn gc_sweeps_dead_owner_oracle_scratch_and_keeps_live_and_young_ones() {
    let repo = init_repo("echo ok");
    let tmp = TempDir::new().unwrap();
    let data = fs::canonicalize(tmp.path()).unwrap().join("data");
    let worktrees = tmp.path().join("worktrees");
    let base = data.join("oracle-scratch");
    fs::create_dir_all(&base).unwrap();
    let id = stats_repo_id(repo.path());
    let dead = base.join(format!("{id}-{}-0", reaped_child_pid()));
    let live = base.join(format!("{id}-{}-1", std::process::id()));
    let young = base.join(format!("{id}-{}-2", reaped_child_pid()));
    // A dead owner whose dir is no worktree at all (crashed mid-add).
    let bare_dir = base.join(format!("{id}-{}-3", reaped_child_pid()));
    for wt in [&dead, &live, &young] {
        let p = wt.to_str().unwrap();
        git_out(
            repo.path(),
            &["worktree", "add", "-q", "--detach", p, "HEAD"],
        );
    }
    fs::create_dir_all(&bare_dir).unwrap();
    for dir in [&dead, &live, &bare_dir] {
        age(dir, Duration::from_secs(3 * 3600));
    }
    assert_eq!(worktree_count(repo.path()), 4);

    let dry = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(dead.is_dir(), "dry run removed a scratch");
    let shown = |p: &Path| dry.removed_files.iter().any(|f| Path::new(f) == p);
    assert!(shown(&dead) && shown(&bare_dir), "{:?}", dry.removed_files);

    let report = run_gc(&data, &worktrees, &GcOptions::default()).unwrap();
    assert!(!dead.exists(), "dead-owner oracle scratch survived gc");
    assert!(
        !bare_dir.exists(),
        "dead-owner non-worktree scratch survived"
    );
    assert!(live.is_dir(), "a live owner's oracle scratch was swept");
    assert!(young.is_dir(), "a young oracle scratch was swept");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let listed = git_out(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains(dead.to_str().unwrap()),
        "registration left: {listed}"
    );
    assert_eq!(worktree_count(repo.path()), 3, "{listed}");

    for wt in [&live, &young] {
        let p = wt.to_str().unwrap();
        git_out(repo.path(), &["worktree", "remove", "--force", p]);
    }
    assert_eq!(worktree_count(repo.path()), 1);
}

#[test]
fn gc_never_follows_a_forged_gitdir_into_another_repository() {
    let repo = init_repo("echo ok");
    let other = init_repo("echo ok");
    let tmp = TempDir::new().unwrap();
    let data = fs::canonicalize(tmp.path()).unwrap().join("data");
    let base = data.join("oracle-scratch");
    fs::create_dir_all(&base).unwrap();
    let victim = fs::canonicalize(other.path()).unwrap().join("wt-victim");
    git_out(
        other.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            victim.to_str().unwrap(),
            "HEAD",
        ],
    );
    let id = stats_repo_id(repo.path());
    let forged = base.join(format!("{id}-{}-0", reaped_child_pid()));
    fs::create_dir_all(&forged).unwrap();
    // `.git` of the forged scratch names the victim's admin dir; that admin
    // dir points back at the victim, not at the scratch.
    let admin = fs::read_to_string(victim.join(".git")).unwrap();
    fs::write(forged.join(".git"), admin).unwrap();
    age(&forged, Duration::from_secs(3 * 3600));

    run_gc(&data, &tmp.path().join("worktrees"), &GcOptions::default()).unwrap();
    assert!(!forged.exists(), "dead forged scratch dir should still go");
    assert!(victim.is_dir(), "gc removed another repository's worktree");
    assert_eq!(worktree_count(other.path()), 2);
}

#[test]
fn block_mode_compile_red_blocks_with_guidance_for_the_agent() {
    let cmd = "if grep -q 'echo 0' src/app.sh; then echo 'error: could not compile `app`'; exit 101; fi; echo ok";
    let repo = init_repo(cmd);
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(repo.path().join("tests/test_app.sh"), "echo ok\n").unwrap();

    let r = run_gate(repo.path(), data.path());
    assert_eq!(r.status, GateStatus::Blocked, "{:?}", r.reasons);
    assert!(
        r.reasons
            .iter()
            .any(|m| m.contains("compile_red") && m.contains("compiles against the base")),
        "{:?}",
        r.reasons
    );
}

/// The oracle's warm slot checks out the agent's HEAD: with a repo-relative
/// `core.hooksPath` its `post-checkout` / `reference-transaction` hooks are
/// files of that commit, and they must never run with the gate's trust.
#[test]
fn oracle_warm_slot_never_runs_agent_controlled_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let data = TempDir::new().unwrap();
    let data_root = fs::canonicalize(data.path()).unwrap();
    let marker = data_root.join("hook-ran");
    let repo = init_repo(
        "for f in tests/test_*.sh; do [ -f \"$f\" ] || continue; sh \"$f\" || exit 1; done",
    );
    let hooks = repo.path().join(".githooks");
    fs::create_dir_all(&hooks).unwrap();
    for hook in ["post-checkout", "reference-transaction"] {
        let p = hooks.join(hook);
        fs::write(
            &p,
            format!("#!/bin/sh\necho {hook} >> '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Hooks in the base itself (a hooks edit on the branch is head-locked and
    // would block before the oracle runs).
    git_out(repo.path(), &["add", "-A"]);
    git_out(repo.path(), &["commit", "-q", "-m", "hooks"]);
    git_out(repo.path(), &["branch", "-f", "main", "HEAD"]);
    fs::write(repo.path().join("src/app.sh"), "#!/bin/sh\necho 42\n").unwrap();
    fs::write(
        repo.path().join("tests/test_app.sh"),
        "#!/bin/sh\nif grep -q 'echo 0' src/app.sh; then echo 'FAIL: app'; exit 1; fi\necho ok\n",
    )
    .unwrap();
    git_out(repo.path(), &["config", "core.hooksPath", ".githooks"]);
    // Only what the gate does from here counts.
    let _ = fs::remove_file(&marker);

    for round in 1..=2 {
        let r = run_gate(repo.path(), &data_root);
        assert!(
            r.reasons.iter().any(|m| m.contains("red_green=proven")),
            "round {round}: {:?}",
            r.reasons
        );
        let ran = fs::read_to_string(&marker).unwrap_or_default();
        assert!(ran.is_empty(), "round {round}: agent hook ran: {ran}");
    }
}
