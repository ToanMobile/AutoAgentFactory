//! Round 10 autonomy fixes on real git: a commit the repository's hook
//! refuses goes back to the agent, a commit that cannot be signed waits,
//! and hooks the agent wrote inside its own worktree never run.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult};
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

/// Plan v3 V2-SEC-03: headless `commit_all` bypasses developer pre-commit hooks
/// (`-c core.hooksPath=/dev/null --no-verify`) so `aaf gate` is the single gate
/// and unattended tasks reach Review cleanly in one round.
#[test]
fn a_pre_commit_hook_is_bypassed_so_unattended_task_reaches_review() {
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
    let (world, deps, writer) = real_world(&fx, vec!["BAD\n"]);

    let first = once(&deps);

    assert_eq!(first.review, vec!["t1"], "{first:?}");
    assert_eq!(world.state("t1"), TaskState::Review);
    assert_eq!(writer.prompts.lock().unwrap().len(), 1);
    assert_eq!(git(&fx.repo, &["show", "aaf/t1:feature.txt"]), "BAD");
}

/// Plan v3 V2-SEC-03: `commit_all` passes `-c commit.gpgsign=false`, so a broken
/// or interactive GPG pinentry setup never blocks unattended tasks.
#[test]
fn a_broken_gpg_signing_config_is_bypassed_so_task_reaches_review() {
    let fx = Fixture::new();
    let gpg = fx.tmp.path().join("gpg-fails.sh");
    script(
        &gpg,
        "#!/bin/sh\necho 'gpg: signing failed: No pinentry' >&2\nexit 2\n",
    );
    git(&fx.repo, &["config", "commit.gpgsign", "true"]);
    git(&fx.repo, &["config", "gpg.program", gpg.to_str().unwrap()]);
    let (world, deps, writer) = real_world(&fx, vec!["x\n"]);

    let summary = once(&deps);

    assert_eq!(summary.review, vec!["t1"], "{summary:?}");
    assert_eq!(world.state("t1"), TaskState::Review);
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert_eq!(writer.runs.load(Ordering::SeqCst), 1, "one agent run only");
}

#[test]
fn commit_all_bypasses_failing_gpg_program() {
    let fx = Fixture::new();
    let gpg = fx.tmp.path().join("gpg-fails.sh");
    script(&gpg, "#!/bin/sh\nexit 2\n");
    git(&fx.repo, &["config", "commit.gpgsign", "true"]);
    git(&fx.repo, &["config", "gpg.program", gpg.to_str().unwrap()]);
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
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

/// Plan v3 V2-SEC-03: even base-committed in-tree hooks are bypassed during
/// headless factory commits (`-c core.hooksPath=/dev/null --no-verify`).
#[test]
fn an_unchanged_in_tree_hook_of_the_base_is_bypassed_at_commit() {
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

    assert!(!marker.exists(), "headless commit must bypass repo hooks");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
}
