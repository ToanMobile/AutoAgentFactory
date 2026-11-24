//! The production git and gate adapters against real repositories in a temp
//! dir (real `git`, real gate commands, no agent).

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateStatus, LockStatus};
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Priority, Task, TaskState};
use tempfile::TempDir;

use super::fakes::{REPO, spec};
use super::world::World;
use crate::factory::adapters::{RealGate, SystemClock};
use crate::factory::batch::{Member, Verdict, gate_batch};
use crate::factory::git_real::open_pr_noted;
use crate::factory::ports::{GateRunner, GitOps};
use crate::factory::scheduler::{GateStats, judge_round};
use crate::factory::worker::GateJob;
use crate::factory::{BaseRef, RealGit, resolve_base};
use crate::git::{PrOutcome, PrRequest, TaskWorktree, pr_body};
use crate::process::{CommandOutput, CommandRunner};

pub(super) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub(super) struct Fixture {
    pub tmp: TempDir,
    pub repo: PathBuf,
    pub ops: RealGit,
}

impl Fixture {
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("tests")).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        // Repo-local, so the developer's identity, signing and hooks cannot
        // change what the tests observe.
        for (key, value) in [
            ("user.name", "aaf test"),
            ("user.email", "aaf@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&repo, &["config", key, value]);
        }
        fs::write(repo.join("app.txt"), "one\n").unwrap();
        fs::write(repo.join("tests/app_test.rs"), "// checks\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "init"]);
        let ops = RealGit::new(
            repo.clone(),
            tmp.path().join("worktrees"),
            tmp.path().join("data/locks"),
            resolve_base(&repo, "auto").unwrap(),
        );
        Fixture { tmp, repo, ops }
    }

    /// A task worktree with one committed change.
    pub fn task(&self, id: &str, file: &str, text: &str) -> TaskWorktree {
        let tree = self.ops.task_worktree(id).unwrap();
        fs::write(tree.path.join(file), text).unwrap();
        self.ops.commit_all(&tree, &format!("aaf: {id}")).unwrap();
        tree
    }

    pub fn gate(&self, test_command: &str) -> RealGate {
        let mut cfg = RepoConfig::default();
        cfg.commands.test_scoped = test_command.to_string();
        RealGate {
            data_dir: self.tmp.path().join("data"),
            cfg,
            clock: Arc::new(SystemClock),
            // The test commands must not wait for the CPU load of the
            // machine that runs the tests.
            machine: MachineConfig {
                max_load_percent: 0,
                ..MachineConfig::default()
            },
            progress: Box::new(|_, _| {}),
            worktrees_dir: self.tmp.path().join("worktrees"),
        }
    }

    /// What an agent can do in its sandbox: hide a file from the tree
    /// fingerprint through the repository's own exclude file (shared by
    /// every worktree) and plant it in `tree`.
    fn plant_hidden(&self, tree: &Path, name: &str) {
        fs::write(self.repo.join(".git/info/exclude"), format!("{name}\n")).unwrap();
        fs::write(tree.join(name), "raise SystemExit(0)\n").unwrap();
    }
}

#[test]
fn a_file_hidden_from_the_fingerprint_does_not_reach_the_gate_of_a_task_worktree() {
    let fx = Fixture::new();
    // Red whenever the planted file is there while the commands run.
    let gate = fx.gate("test ! -f conftest.py");
    let tree = fx.task("t1", "other.txt", "x\n");
    fx.plant_hidden(&tree.path, "conftest.py");

    let receipt = gate.run(&tree.path).unwrap();

    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    assert!(!tree.path.join("conftest.py").exists());
    // What the task committed is untouched.
    assert!(tree.path.join("other.txt").is_file());
}

#[test]
fn the_factorys_gate_deletes_nothing_outside_its_own_worktrees() {
    let fx = Fixture::new();
    let gate = fx.gate("true");
    // The operator's own checkout, with a file only they know about.
    fx.plant_hidden(&fx.repo, "notes.local");

    // Whatever the verdict is: the file is the operator's and stays.
    let _ = gate.run(&fx.repo);

    assert!(fx.repo.join("notes.local").is_file());
}

#[test]
fn the_base_is_the_local_trunk_or_the_remote_one_when_there_is_a_remote() {
    let fx = Fixture::new();
    let local = BaseRef {
        remote: None,
        branch: "main".to_string(),
    };
    assert_eq!(*fx.ops.base(), local);
    assert_eq!(local.start_point(), "main");

    let bare = fx.tmp.path().join("origin.git");
    git(fx.tmp.path(), &["init", "--quiet", "--bare", "origin.git"]);
    git(
        &fx.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&fx.repo, &["push", "--quiet", "origin", "main"]);
    let remote = resolve_base(&fx.repo, "auto").unwrap();
    assert_eq!(remote.remote.as_deref(), Some("origin"));
    assert_eq!(remote.start_point(), "origin/main");

    assert_eq!(resolve_base(&fx.repo, "main").unwrap(), local);
    assert!(resolve_base(&fx.repo, "no-such-branch").is_err());
    assert!(resolve_base(&fx.repo, "--upload-pack=x").is_err());
}

#[test]
fn a_task_worktree_is_created_once_and_then_reused_with_its_work() {
    let fx = Fixture::new();
    let tree = fx.ops.task_worktree("t1").unwrap();
    assert_eq!(tree.branch, "aaf/t1");
    assert!(tree.path.ends_with("t1"));
    assert!(!fx.ops.has_changes(&tree).unwrap());

    // Uncommitted work is committed for the agent; a clean tree is left alone.
    fs::write(tree.path.join("new.txt"), "hello\n").unwrap();
    fx.ops.commit_all(&tree, "aaf: t1").unwrap();
    let head = git(&tree.path, &["rev-parse", "HEAD"]);
    fx.ops.commit_all(&tree, "aaf: t1 again").unwrap();
    assert_eq!(git(&tree.path, &["rev-parse", "HEAD"]), head);

    assert!(fx.ops.has_changes(&tree).unwrap());
    assert!(fx.ops.diff(&tree).unwrap().contains("+hello"));
    assert_eq!(fx.ops.task_worktree("t1").unwrap(), tree);
    assert!(
        tree.path.join("new.txt").is_file(),
        "the rerun sees the earlier work"
    );
    assert!(!fx.ops.is_merged("t1").unwrap());
    // Unmerged work is never cleaned away.
    assert!(fx.ops.cleanup("t1").is_err());
    assert!(tree.path.join("new.txt").is_file());

    git(&fx.repo, &["merge", "--quiet", "aaf/t1"]);
    assert!(fx.ops.is_merged("t1").unwrap());
    assert!(!fx.ops.is_merged("never-existed").unwrap());
    fx.ops.cleanup("t1").unwrap();
    assert!(!tree.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t1"]), "");
    fx.ops.cleanup("t1").unwrap();
}

#[test]
fn integration_merges_clean_branches_and_leaves_the_conflicting_one_out() {
    let fx = Fixture::new();
    fx.task("t1", "app.txt", "one\nfrom t1\n");
    fx.task("t2", "other.txt", "from t2\n");
    fx.task("t3", "app.txt", "one\nfrom t3\n");
    let ids: Vec<String> = ["t1", "t2", "t3"].map(String::from).to_vec();

    let integration = fx.ops.integrate(&ids).unwrap();

    assert_eq!(integration.merged, vec!["t1", "t2"]);
    assert_eq!(integration.conflicted, vec!["t3"]);
    let tree = &integration.tree.path;
    assert_eq!(
        fs::read_to_string(tree.join("app.txt")).unwrap(),
        "one\nfrom t1\n"
    );
    assert!(tree.join("other.txt").is_file());
    assert_eq!(
        git(tree, &["status", "--porcelain"]),
        "",
        "no half-done merge"
    );
    // Built on the base: the task branches themselves are untouched.
    assert_eq!(git(&fx.repo, &["rev-list", "--count", "main..aaf/t1"]), "1");

    fx.ops.discard(&integration).unwrap();
    assert!(!tree.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/batch-*"]), "");
}

#[test]
fn rebase_reports_the_conflicting_files_and_leaves_the_worktree_usable() {
    let fx = Fixture::new();
    let clean = fx.task("t1", "other.txt", "from t1\n");
    let clash = fx.task("t2", "app.txt", "one\nfrom t2\n");
    fs::write(fx.repo.join("app.txt"), "one\nfrom main\n").unwrap();
    git(&fx.repo, &["commit", "--quiet", "-am", "main moves"]);
    let tip = git(&fx.repo, &["rev-parse", "main"]);
    assert_eq!(fx.ops.base_sha().unwrap(), tip);

    assert_eq!(fx.ops.rebase(&clean).unwrap(), Vec::<String>::new());
    assert_eq!(git(&clean.path, &["merge-base", "HEAD", "main"]), tip);

    assert_eq!(fx.ops.rebase(&clash).unwrap(), vec!["app.txt"]);
    assert_eq!(
        git(&clash.path, &["status", "--porcelain"]),
        "",
        "rebase aborted"
    );
    assert_eq!(
        git(&clash.path, &["symbolic-ref", "--short", "HEAD"]),
        "aaf/t2"
    );

    // Edits an earlier run left uncommitted: git refuses the rebase, and
    // the error says so instead of "no rebase in progress". Once they are
    // committed (what a rerun does first) the rebase goes through.
    fs::write(clean.path.join("other.txt"), "half done\n").unwrap();
    let refused = fx.ops.rebase(&clean).unwrap_err().to_string();
    assert!(refused.contains("unstaged changes"), "{refused}");
    fx.ops.commit_all(&clean, "aaf: earlier attempt").unwrap();
    assert_eq!(fx.ops.rebase(&clean).unwrap(), Vec::<String>::new());
}

#[test]
fn two_real_task_branches_are_gated_with_one_real_gate_run() {
    let fx = Fixture::new();
    let gate = fx.gate("test -f app.txt && test -f other.txt && test -f third.txt");
    let members: Vec<Member> = [("t1", "other.txt"), ("t2", "third.txt")]
        .iter()
        .map(|(id, file)| Member {
            task_id: (*id).to_string(),
            worktree: fx.task(id, file, "x\n"),
        })
        .collect();

    let outcome = gate_batch(&fx.ops, &gate, &members);

    // The test command needs both tasks' files: it can only pass in the
    // integration tree, never in a single task's worktree.
    assert_eq!(outcome.gate_runs, 1);
    for judged in &outcome.verdicts {
        let Verdict::Clear(receipt) = &judged.verdict else {
            panic!("{}: {:?}", judged.task_id, judged.verdict);
        };
        assert!(judged.batch_group.is_some());
        assert_eq!(receipt.status, GateStatus::Clear);
        assert!(
            receipt.repo_root.contains("batch-"),
            "{}",
            receipt.repo_root
        );
    }
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/batch-*"]), "");
}

#[test]
fn a_red_real_batch_is_bisected_down_to_the_task_that_breaks_the_gate() {
    let fx = Fixture::new();
    let gate = fx.gate("test ! -f broken.txt");
    let files = ["a.txt", "b.txt", "broken.txt", "d.txt"];
    let members: Vec<Member> = files
        .iter()
        .enumerate()
        .map(|(n, file)| {
            let task_id = format!("t{}", n + 1);
            Member {
                worktree: fx.task(&task_id, file, "x\n"),
                task_id,
            }
        })
        .collect();

    let outcome = gate_batch(&fx.ops, &gate, &members);

    // 1 + 2 * log2(4)
    assert!(outcome.gate_runs <= 5, "{} runs", outcome.gate_runs);
    for judged in &outcome.verdicts {
        let red = matches!(judged.verdict, Verdict::Red(_));
        assert_eq!(
            red,
            judged.task_id == "t3",
            "{}: {:?}",
            judged.task_id,
            judged.verdict
        );
    }
}

/// One gate round with `--auto-merge` on the real git and gate adapters:
/// task t<n> adds `files[n - 1]`. Returns the number of gate runs.
fn real_auto_merge_round(fx: &Fixture, test_command: &str, files: &[&str]) -> u32 {
    let mut world = World::new();
    world.auto_merge = true;
    let mut deps = world.deps();
    deps.git = Arc::new(RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    ));
    deps.gate = Arc::new(fx.gate(test_command));
    let jobs: Vec<GateJob> = files
        .iter()
        .enumerate()
        .map(|(n, file)| {
            let id = format!("t{}", n + 1);
            let mut task = Task::new_root(&id, REPO, &id, spec(), Priority::P2, 1);
            task.state = TaskState::Gating;
            world.db.insert_task(&task).unwrap();
            GateJob {
                worktree: fx.task(&id, file, "x\n"),
                task_id: id,
                audited: false,
                cached_receipt: None,
            }
        })
        .collect();
    let stats = GateStats::default();
    judge_round(&deps, jobs, &stats, &|_, resolution| {
        if let Err(error) = resolution {
            panic!("{error}");
        }
    });
    let warnings = stats.warnings.lock().unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    stats.runs.load(std::sync::atomic::Ordering::Relaxed)
}

fn on_main(fx: &Fixture) -> Vec<String> {
    let mut files: Vec<String> = git(&fx.repo, &["ls-tree", "--name-only", "main"])
        .lines()
        .filter(|name| name.ends_with(".txt") && *name != "app.txt")
        .map(str::to_string)
        .collect();
    files.sort();
    files
}

#[test]
fn a_green_real_batch_lands_through_the_real_merge_queue_with_one_gate_run() {
    let fx = Fixture::new();

    let runs = real_auto_merge_round(&fx, "test -f app.txt", &["one.txt", "two.txt", "three.txt"]);

    assert_eq!(runs, 1);
    assert_eq!(on_main(&fx), ["one.txt", "three.txt", "two.txt"]);
}

#[test]
fn a_real_sub_batch_is_gated_again_before_it_lands_on_the_other_sub_batch() {
    let fx = Fixture::new();
    // t3 is red alone; t1 and t4 are red only in the same tree.
    let command = "test ! -f broken.txt && ! { test -f one.txt && test -f four.txt; }";
    let files = ["one.txt", "two.txt", "broken.txt", "four.txt", "five.txt"];

    let runs = real_auto_merge_round(&fx, command, &files);

    // [t1, t2] and [t4, t5] were gated green apart. t4 must not land on t1
    // without a gate run of its own, and that run is red.
    assert_eq!(on_main(&fx), ["five.txt", "one.txt", "two.txt"]);
    // 5 runs of the bisection, then t4 and t5 on the base that holds t1.
    assert_eq!(runs, 7);
}

/// `gh` that opens every pull request and keeps the bodies it was given.
#[derive(Default)]
struct GhStub {
    bodies: RefCell<Vec<String>>,
}

impl CommandRunner for GhStub {
    fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> io::Result<CommandOutput> {
        assert_eq!(program, "gh");
        if let Some(at) = args.iter().rposition(|arg| *arg == "--body") {
            self.bodies.borrow_mut().push(args[at + 1].to_string());
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: "https://example.invalid/pull/1\n".to_string(),
            stderr: String::new(),
        })
    }
}

#[test]
fn the_note_of_a_batch_receipt_stands_above_the_receipt_summary_of_the_pr_body() {
    let fx = Fixture::new();
    let bare = fx.tmp.path().join("origin.git");
    git(fx.tmp.path(), &["init", "--quiet", "--bare", "origin.git"]);
    git(
        &fx.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    let worktree = fx.task("t1", "other.txt", "x\n");
    let request = PrRequest {
        repo_root: fx.repo.clone(),
        receipt: fx.gate("true").run(&worktree.path).unwrap(),
        worktree,
        base: "main".to_string(),
        remote: "origin".to_string(),
        // Untrusted text that looks like the flag the note is attached to.
        title: "--body".to_string(),
    };
    let gh = GhStub::default();

    let noted = open_pr_noted(&request, &gh, Some("## Batch gate receipt\n\nmembers"));
    let plain = open_pr_noted(&request, &gh, None);

    let opened = PrOutcome::Opened {
        url: "https://example.invalid/pull/1".to_string(),
    };
    assert_eq!((noted.unwrap(), plain.unwrap()), (opened.clone(), opened));
    let summary = pr_body(&request.receipt);
    assert!(summary.starts_with("## Gate receipt"), "{summary}");
    assert_eq!(
        *gh.bodies.borrow(),
        [
            format!("## Batch gate receipt\n\nmembers\n\n{summary}"),
            summary
        ]
    );
}

#[test]
fn the_head_lock_precheck_sees_a_changed_test_file_but_not_a_new_one() {
    let fx = Fixture::new();
    let gate = fx.gate("true");
    let added = fx.task("t1", "tests/new_test.rs", "// new\n");
    let edited = fx.task("t2", "tests/app_test.rs", "// weakened\n");

    assert_eq!(
        gate.head_lock(&added.path).unwrap().status,
        LockStatus::Clear
    );
    let report = gate.head_lock(&edited.path).unwrap();
    assert_eq!(report.status, LockStatus::Blocked);
    assert!(!report.lock_fp.is_empty());
    assert_eq!(gate.run(&edited.path).unwrap().status, GateStatus::Blocked);
}

#[test]
fn workers_and_the_gate_thread_can_use_the_repository_at_the_same_time() {
    let fx = Fixture::new();
    let seed: Vec<String> = ["s1", "s2"].map(String::from).to_vec();
    fx.task("s1", "s1.txt", "x\n");
    fx.task("s2", "s2.txt", "x\n");

    // What `aaf run --max-parallel N` does: workers create task worktrees
    // while the gate thread builds and drops integration trees. Plain git
    // fails here now and then ("failed to read .git/worktrees/<other>/
    // commondir"): `worktree add` reads the entries of worktrees that
    // another `worktree add` is still writing.
    let start = std::sync::Barrier::new(25);
    std::thread::scope(|scope| {
        for n in 0..24 {
            let (fx, start) = (&fx, &start);
            scope.spawn(move || {
                start.wait();
                let tree = fx.ops.task_worktree(&format!("p{n}")).unwrap();
                assert!(tree.path.is_dir());
            });
        }
        start.wait();
        for _ in 0..3 {
            let integration = fx.ops.integrate(&seed).unwrap();
            assert_eq!(integration.merged, seed);
            fx.ops.discard(&integration).unwrap();
        }
    });
}
