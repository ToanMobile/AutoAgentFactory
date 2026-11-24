//! Round 10 autonomy fixes on real git: a commit the repository's hook
//! refuses goes back to the agent, a commit that cannot be signed waits,
//! and hooks the agent wrote inside its own worktree never run.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::ok_outcome;
use super::git_real::{Fixture, git};
use super::world::World;
use crate::factory::ports::GitOps;
use crate::factory::{Deps, RealGit, resolve_base};

/// Writes `texts[n]` to `feature.txt` on its n-th agent run and keeps every
/// prompt it was given.
struct Writer {
    texts: Vec<&'static str>,
    runs: AtomicUsize,
    prompts: Mutex<Vec<String>>,
}

impl AgentRunner for Writer {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.prompt.contains("Split it into at") {
            return Ok(AgentOutcome {
                final_text: "[]".into(),
                ..ok_outcome()
            });
        }
        self.prompts.lock().unwrap().push(req.prompt.clone());
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some(text) = self.texts.get(n) {
            fs::write(Path::new(&req.cwd).join("feature.txt"), text).unwrap();
        }
        Ok(ok_outcome())
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn real_world(fx: &Fixture, texts: Vec<&'static str>) -> (World, Deps, Arc<Writer>) {
    let world = World::with_accounts(&[]);
    world.add("t1");
    let mut deps = world.deps();
    deps.git = Arc::new(RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    ));
    deps.gate = Arc::new(fx.gate("true"));
    let writer = Arc::new(Writer {
        texts,
        runs: AtomicUsize::new(0),
        prompts: Mutex::new(Vec::new()),
    });
    deps.runner = writer.clone();
    (world, deps, writer)
}

fn once(deps: &Deps) -> crate::factory::RunSummary {
    let hooks = crate::factory::Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    crate::factory::run(deps, crate::factory::Mode::Once, &hooks).unwrap()
}

/// O12-R10-3: a pre-commit hook that refuses the agent's change is feedback
/// for the agent, not a task for a person: the hook's words go into the next
/// run's notes, and the fixed change lands.
#[test]
fn a_pre_commit_hook_refusal_goes_back_to_the_agent_and_the_fix_reaches_review() {
    let fx = Fixture::new();
    let hooks = fx.tmp.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    script(
        &hooks.join("pre-commit"),
        "#!/bin/sh\nif git diff --cached | grep -q BAD; then\n  \
         echo 'lint: feature.txt contains BAD, write good' >&2\n  exit 1\nfi\n",
    );
    git(
        &fx.repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let (world, deps, writer) = real_world(&fx, vec!["BAD\n", "good\n"]);

    let first = once(&deps);

    let task = world.task("t1");
    assert_eq!(
        task.state,
        TaskState::Ready,
        "{first:?} {:?}",
        task.blocked_reason
    );
    assert!(first.blocked.is_empty(), "{first:?}");
    let note = task
        .context_notes
        .iter()
        .find(|n| n.contains("write good"))
        .unwrap_or_else(|| panic!("no hook note: {:?}", task.context_notes));
    assert!(note.contains("<untrusted-data"), "{note}");

    let second = once(&deps);

    assert_eq!(second.review, vec!["t1"], "{second:?}");
    assert_eq!(world.state("t1"), TaskState::Review);
    let prompts = writer.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("write good"), "{}", prompts[1]);
    let landed = git(&fx.repo, &["show", "aaf/t1:feature.txt"]);
    assert_eq!(landed, "good");
}

/// O12-R10-3 (e): a commit that cannot be signed waits (twice for the same
/// failure) before a person is asked; it never hangs or blocks at once.
#[test]
fn a_commit_that_cannot_be_signed_waits_twice_then_asks_a_person() {
    let fx = Fixture::new();
    let gpg = fx.tmp.path().join("gpg-fails.sh");
    script(
        &gpg,
        "#!/bin/sh\necho 'gpg: signing failed: No pinentry' >&2\nexit 2\n",
    );
    git(&fx.repo, &["config", "commit.gpgsign", "true"]);
    git(&fx.repo, &["config", "gpg.program", gpg.to_str().unwrap()]);
    let (world, deps, writer) = real_world(&fx, vec!["x\n"]);

    for round in 0..2 {
        let summary = once(&deps);
        let task = world.task("t1");
        assert_eq!(task.state, TaskState::Ready, "round {round}: {summary:?}");
        assert!(summary.blocked.is_empty(), "round {round}: {summary:?}");
    }
    let third = once(&deps);

    assert_eq!(third.blocked, vec!["t1"], "{third:?}");
    let task = world.task("t1");
    assert!(
        matches!(task.blocked_reason, Some(BlockedReason::NeedsHuman { .. })),
        "{:?}",
        task.blocked_reason
    );
    assert_eq!(writer.runs.load(Ordering::SeqCst), 1, "one agent run only");
}

#[test]
fn a_signing_failure_is_reported_as_a_deferred_commit() {
    let fx = Fixture::new();
    let gpg = fx.tmp.path().join("gpg-fails.sh");
    script(&gpg, "#!/bin/sh\nexit 2\n");
    git(&fx.repo, &["config", "commit.gpgsign", "true"]);
    git(&fx.repo, &["config", "gpg.program", gpg.to_str().unwrap()]);
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    let error = fx.ops.commit_all(&tree, "aaf: t1").unwrap_err();

    assert!(
        matches!(&error, PortError::Io(m) if m.starts_with("commit deferred")),
        "{error}"
    );
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "0");
}

/// LEAD C: hooks that live inside the task's own worktree and that the task
/// changed are the agent's own code; a commit does not run them.
#[test]
fn an_agent_written_in_tree_hook_does_not_run_at_commit() {
    let fx = Fixture::new();
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    let tree = fx.ops.task_worktree("t1").unwrap();
    let marker = fx.tmp.path().join("agent-hook-ran");
    fs::create_dir_all(tree.path.join(".githooks")).unwrap();
    script(
        &tree.path.join(".githooks/pre-commit"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert!(!marker.exists(), "the agent's own hook ran unsandboxed");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
}

/// The same in-tree hooks, committed on the base and untouched by the task,
/// are the repository's: they run.
#[test]
fn an_unchanged_in_tree_hook_of_the_base_still_runs() {
    let fx = Fixture::new();
    let marker = fx.tmp.path().join("repo-hook-ran");
    fs::create_dir_all(fx.repo.join(".githooks")).unwrap();
    script(
        &fx.repo.join(".githooks/pre-commit"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "hooks"]);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert!(marker.exists(), "the repository's own hook did not run");
}
