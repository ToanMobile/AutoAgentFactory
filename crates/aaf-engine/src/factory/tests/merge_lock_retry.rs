//! R10-L4-2: `aaf merge` holds the repository's merge lock across a whole
//! scratch gate. A factory merge that times out waiting for that lock is
//! retried later, not blocked for a human. Real git, real gate, real lock.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, EventSink, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::ok_outcome;
use super::git_real::{Fixture, git};
use super::world::World;
use crate::factory::{Hooks, Mode, RealGit, resolve_base, run};

/// Writes a feature file in the task's worktree.
struct Writer;
impl AgentRunner for Writer {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.prompt.contains("Split it into at") {
            return Ok(AgentOutcome {
                final_text: "[]".into(),
                ..ok_outcome()
            });
        }
        fs::write(Path::new(&req.cwd).join("feature.txt"), "x\n").unwrap();
        Ok(ok_outcome())
    }
}

/// Long enough that only a hung test reaches it.
const HANG: Duration = Duration::from_secs(100);

#[test]
fn a_merge_lock_held_by_another_process_is_waited_out_not_blocked() {
    let fx = Fixture::new();
    let mut world = World::with_accounts(&[]);
    world.auto_merge = true;
    world.add("t1");
    let mut deps = world.deps();
    let locks = fx.tmp.path().join("data/locks");
    deps.git = Arc::new(
        RealGit::new(
            fx.repo.clone(),
            fx.tmp.path().join("worktrees"),
            locks.clone(),
            resolve_base(&fx.repo, "auto").unwrap(),
        )
        .with_merge_lock_timeout(Duration::from_millis(200)),
    );
    deps.gate = Arc::new(fx.gate("test -f feature.txt"));
    deps.runner = Arc::new(Writer);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    // Another process (`aaf merge`) holds the merge lock. It lets go only
    // once the factory judged the task a second time, i.e. after a merge
    // that timed out on the lock was retried.
    fs::create_dir_all(&locks).unwrap();
    let path = locks.join(format!("{}.merge.lock", aaf_fp::repo_id(&fx.repo)));
    let holder = File::create(&path).unwrap();
    holder.lock().unwrap();
    let done = AtomicBool::new(false);
    let summary = std::thread::scope(|scope| {
        let releaser = scope.spawn(|| {
            let deadline = Instant::now() + HANG;
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let gated = world.db.events_for_task("t1", 200).unwrap();
                let gated = gated.iter().filter(|e| e.kind == EventKind::GateResult);
                if gated.count() >= 2 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            holder.unlock().unwrap();
        });
        let summary = run(&deps, Mode::Once, &hooks);
        done.store(true, Ordering::SeqCst);
        releaser.join().unwrap();
        summary
    })
    .unwrap();

    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert!(summary.errors.is_empty(), "{summary:?}");
    assert_eq!(world.state("t1"), TaskState::Done, "{summary:?}");
    let landed = git(&fx.repo, &["ls-tree", "--name-only", "main"]);
    assert!(landed.lines().any(|f| f == "feature.txt"), "{landed}");
}
