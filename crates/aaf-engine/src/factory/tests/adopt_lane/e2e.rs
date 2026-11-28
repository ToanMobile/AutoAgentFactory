//! `aaf board adopt` and the lane end to end: the task under test is made by
//! the command's own code (intake, `pin_head`, `Db::insert_adopting`, with the
//! task and events built the way `cmd/board/adopt.rs` builds them), never by a
//! direct insert, and then the lane (and the `Auto` scheduler) runs it to
//! `Done` on real git.
//!
//! Hermetic as to `AAF_WORKTREE_DIR`: nothing the lane or the pin reads takes
//! the worktree directory from the environment. The command takes it from
//! `ctx.worktrees_dir` and the factory from `RealGit::new`; both are handed
//! `lab.worktrees` explicitly here, so a host value cannot reach the test.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::event::{EventKind, FactoryEvent, PrAdoptedPayload};
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

use super::support::{ID, Lab, PR, Pr};
use crate::db::AdoptInsert;
use crate::detector::github_issues::frame_untrusted;
use crate::factory::tests::git_real::git;
use crate::factory::{Hooks, Mode, run_with_router_and_stop};
use crate::git::base_ref::resolve_base;
use crate::git::gh_client::PrAuthor;
use crate::git::{TaskWorktree, main_work_tree};
use crate::pr_ops::adopt_intake::{self, Intake};
use crate::pr_ops::adopt_pin::pin_head;

const LOGIN: &str = "octodev";
const NOW: i64 = 1;

/// The task `cmd/board/adopt.rs::adopting_task` makes (title, spec framing and
/// fingerprint included).
fn adopting_task(root: &str, intake: &Intake) -> Task {
    let pr = &intake.pr;
    let spec = ImmutableSpec {
        description: format!(
            "Adopted pull request #{} ({}), pinned at {}.\n\n{}",
            pr.number,
            pr.url,
            intake.head_sha,
            frame_untrusted(&format!("{}\n\n{}", pr.title, pr.body))
        ),
        acceptance: vec![
            "the head passes the gate, two reviewers of different providers pass it, \
             and it merges at the pinned head"
                .to_string(),
        ],
    };
    let mut task = Task::new_root(
        ID,
        root,
        format!("adopt PR #{}: {}", pr.number, pr.title),
        spec,
        Priority::P1,
        NOW,
    );
    task.state = TaskState::Adopting;
    task.fingerprint = Some(adopt_intake::fingerprint(pr));
    task
}

/// The two events `cmd/board/adopt.rs::events_for` writes.
fn adopting_events(task: &Task, intake: &Intake) -> Vec<FactoryEvent> {
    let pr = &intake.pr;
    let created = serde_json::json!({
        "title": task.title,
        "blocked_by": Vec::<String>::new(),
        "scope_paths": Vec::<String>::new(),
        "source": "board adopt",
        "pr_number": pr.number,
    });
    let adopted = PrAdoptedPayload {
        pr_number: pr.number,
        url: pr.url.clone(),
        head_sha: intake.head_sha.clone(),
        head_ref: pr.head_ref_name.clone(),
        base_ref: pr.base_ref_name.clone(),
        author_login: pr
            .author
            .as_ref()
            .map(|a| a.login.trim().to_string())
            .unwrap_or_default(),
        declared_executor: None,
    };
    vec![
        FactoryEvent::new(EventKind::TaskCreated, created.to_string(), NOW).for_task(&task.id),
        FactoryEvent::new(
            EventKind::PrAdopted,
            serde_json::to_string(&adopted).unwrap(),
            NOW,
        )
        .for_task(&task.id),
    ]
}

/// What `aaf board adopt <PR>` leaves behind when started in `cwd`.
struct Adopted {
    tree: TaskWorktree,
    /// The repository root the command stored in `tasks.repo_root`.
    root: PathBuf,
}

/// `cmd/board/adopt.rs::run` with the fake `gh`: the same root resolution, the
/// same intake, the same pin and the same transaction, in the same order.
fn adopt_from(lab: &Lab, cwd: &Path) -> Adopted {
    aaf_fp::repo_root(cwd).expect("cwd is inside a repository");
    let root = fs::canonicalize(main_work_tree(cwd)).unwrap();
    let base = resolve_base(&root, "origin/main").unwrap();
    let allowlist = vec![LOGIN.to_string()];
    let intake = adopt_intake::intake(&*lab.gh, &root, PR, &base, &allowlist)
        .expect("intake infrastructure")
        .unwrap_or_else(|refusal| panic!("refused: {refusal:?}"));

    let repo = root.to_string_lossy().into_owned();
    let task = adopting_task(&repo, &intake);
    let events = adopting_events(&task, &intake);
    let tree = pin_head(&root, &lab.worktrees, ID, &intake.head_sha).expect("pin");
    let inserted = lab.world.db.insert_adopting(&task, &events).unwrap();
    assert_eq!(inserted, AdoptInsert::Inserted);
    Adopted { tree, root }
}

/// A lab whose remote serves the PR head at `refs/pull/<PR>/head` and whose PR
/// is written by an allowed login: what the intake needs from GitHub.
fn lab_with_open_pr(kind: Pr) -> Lab {
    let lab = Lab::unadopted(kind);
    git(
        &lab.work,
        &[
            "push",
            "-q",
            "origin",
            &format!("{}:refs/pull/{PR}/head", lab.head),
        ],
    );
    lab.gh.pr.lock().unwrap().author = Some(PrAuthor {
        login: LOGIN.to_string(),
    });
    assert!(
        !lab.ref_exists(&format!("refs/heads/aaf/{ID}")) && !lab.adopt_wt().exists(),
        "nothing is pinned before the command runs"
    );
    lab
}

/// A second checkout of the same repository, as a chat session works in one.
fn linked_worktree(lab: &Lab) -> PathBuf {
    let linked = lab.work.parent().unwrap().join("chat-session");
    git(
        &lab.work,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "chat-side",
            linked.to_str().unwrap(),
        ],
    );
    linked
}

/// The main checkout reached through a symlink (what a non-canonical cwd is).
fn aliased(lab: &Lab) -> PathBuf {
    let alias = lab.work.parent().unwrap().join("alias");
    std::os::unix::fs::symlink(&lab.work, &alias).unwrap();
    alias
}

/// What the lane and the scheduler rely on, checked before either runs.
fn assert_created(lab: &Lab, adopted: &Adopted) {
    // The pin made the worktree where the lane looks for it.
    assert_eq!(adopted.tree.path, lab.adopt_wt());
    assert!(adopted.tree.path.is_dir());
    assert_eq!(adopted.tree.branch, format!("aaf/{ID}"));
    let deps = lab.deps();
    let found = deps
        .git
        .existing_worktree_head(ID)
        .unwrap()
        .expect("the lane's git finds the worktree the pin created");
    assert_eq!(found.0.path, adopted.tree.path);
    assert_eq!(found.1, lab.head);
    assert_eq!(git(&adopted.tree.path, &["rev-parse", "HEAD"]), lab.head);
    assert_eq!(
        git(&lab.work, &["rev-parse", &format!("aaf/{ID}")]),
        lab.head
    );

    // The task is what the scheduler lists: the main work tree, canonicalized,
    // byte for byte the `repo_root` of the deps `aaf run` builds.
    let canonical = fs::canonicalize(&lab.work).unwrap();
    assert_eq!(adopted.root, canonical);
    // `aaf run --auto` started in the main checkout computes its root with the
    // recipe of `cmd/board/store.rs::repo_root`; the task must carry that string.
    let run_root = fs::canonicalize(aaf_fp::repo_root(&lab.work).unwrap()).unwrap();
    assert_eq!(lab.task().repo_root, run_root.to_string_lossy());
    let task = lab.task();
    assert_eq!(task.state, TaskState::Adopting);
    assert_eq!(task.repo_root, canonical.to_string_lossy());
    assert_eq!(task.repo_root, deps.settings.repo_root);
    let listed = lab
        .world
        .db
        .list_tasks(Some(&deps.settings.repo_root), Some(TaskState::Adopting))
        .unwrap();
    assert_eq!(
        listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        [ID],
        "list_tasks(repo, Adopting) finds the task the command inserted"
    );
    assert!(
        task.fingerprint
            .as_deref()
            .is_some_and(|f| f.starts_with("gh-pr:"))
    );

    // The pin the lane reads back is the one the command recorded.
    let events = lab.events(EventKind::PrAdopted);
    assert_eq!(events.len(), 1);
    let pin: PrAdoptedPayload = serde_json::from_str(&events[0].payload_json).unwrap();
    assert!(pin.head_is_valid());
    assert_eq!(pin.pinned(), lab.head);
    assert_eq!(pin.pr_number, PR);
    assert_eq!(pin.author_login, LOGIN);
    assert_eq!(lab.events(EventKind::TaskCreated).len(), 1);
}

/// Done, and everything the lane promises to leave: the merge once, two
/// reviewers, `aaf/<id>` and the adopt worktree gone, the chat's branch kept.
fn assert_done_and_cleaned(lab: &Lab, summary: &str) {
    assert_eq!(
        lab.state(),
        TaskState::Done,
        "{summary} {:?}",
        lab.abort_code()
    );
    assert_eq!(lab.gh.merges(), 1);
    assert_eq!(lab.reviewer_runs(), 2);
    assert!(lab.events(EventKind::AdoptAborted).is_empty());
    let merged = lab.events(EventKind::MergeResult);
    assert_eq!(merged.len(), 1);
    let value: serde_json::Value = serde_json::from_str(&merged[0].payload_json).unwrap();
    assert_eq!(value["merged"], true);
    assert_eq!(value["head_sha"], lab.head.as_str());
    assert!(!lab.ref_exists(&format!("refs/heads/aaf/{ID}")));
    assert!(!lab.adopt_wt().exists());
    assert!(
        lab.ref_exists("refs/heads/feat/pr"),
        "the chat's branch stays"
    );
}

/// One `Auto` round of the scheduler, as `tests/adopt_lane/sched.rs` runs it.
fn auto_round(lab: &Lab) -> String {
    let db = lab.world.db.clone();
    let seen = AtomicUsize::new(0);
    let idle = move || {
        let adopting = TaskStore::get_task(&db, ID).is_ok_and(|t| t.state == TaskState::Adopting);
        adopting && seen.fetch_add(1, Ordering::SeqCst) < 50
    };
    let hooks = Hooks {
        idle: &idle,
        reap: &|_| Ok(()),
    };
    let summary = run_with_router_and_stop(
        &lab.deps(),
        None,
        Some(&lab.router),
        Mode::Auto,
        &hooks,
        &|| false,
    )
    .unwrap();
    assert!(
        summary.blocked.is_empty() && summary.halted.is_none(),
        "{summary:?}"
    );
    format!("{summary:?}")
}

#[test]
fn an_adopted_pr_without_a_locked_file_goes_to_done_through_the_lane() {
    let lab = lab_with_open_pr(Pr::Lib);
    // Started from the main checkout, spelled through a symlink.
    let adopted = adopt_from(&lab, &aliased(&lab));
    assert_created(&lab, &adopted);

    let done = lab.turn();
    assert_eq!(done.error, None);
    assert_done_and_cleaned(&lab, "lane turn");
    assert_eq!(lab.approvals_on_disk(), 0, "no lock, nothing signed");
}

#[test]
fn an_adopted_pr_with_a_hard_locked_file_goes_to_done_through_the_lane() {
    let lab = lab_with_open_pr(Pr::Locked);
    // Started from a linked worktree: the root must still be the main checkout.
    let linked = linked_worktree(&lab);
    assert_ne!(
        fs::canonicalize(aaf_fp::repo_root(&linked).unwrap()).unwrap(),
        fs::canonicalize(&lab.work).unwrap(),
        "the linked worktree's own top level is not the main checkout"
    );
    let adopted = adopt_from(&lab, &linked);
    assert_created(&lab, &adopted);

    let done = lab.turn();
    assert_eq!(done.error, None);
    assert_done_and_cleaned(&lab, "lane turn");
    assert!(lab.approvals_on_disk() > 0, "the Hard lock was signed");
    assert!(linked.is_dir(), "the chat's own checkout is untouched");
}

#[test]
fn an_auto_round_runs_an_adopted_pr_without_a_lock_to_done() {
    let lab = lab_with_open_pr(Pr::Lib);
    let adopted = adopt_from(&lab, &linked_worktree(&lab));
    assert_created(&lab, &adopted);

    let summary = auto_round(&lab);
    assert_done_and_cleaned(&lab, &summary);
    assert_eq!(lab.approvals_on_disk(), 0);
}

#[test]
fn an_auto_round_runs_an_adopted_pr_with_a_hard_lock_to_done() {
    let lab = lab_with_open_pr(Pr::Locked);
    let adopted = adopt_from(&lab, &aliased(&lab));
    assert_created(&lab, &adopted);

    let summary = auto_round(&lab);
    assert_done_and_cleaned(&lab, &summary);
    assert!(lab.approvals_on_disk() > 0);
}

#[test]
fn a_second_adoption_of_the_same_pr_is_refused_and_leaves_the_first_pin_alone() {
    let lab = lab_with_open_pr(Pr::Lib);
    let first = adopt_from(&lab, &lab.work);
    assert_created(&lab, &first);

    // A second task for the same PR: the pin of its own id works, the insert
    // finds the first one and the command unpins its own.
    let canonical = first.root.to_string_lossy().into_owned();
    let base = resolve_base(&first.root, "origin/main").unwrap();
    let allowlist = vec![LOGIN.to_string()];
    let intake = adopt_intake::intake(&*lab.gh, &first.root, PR, &base, &allowlist)
        .unwrap()
        .unwrap();
    let mut task = adopting_task(&canonical, &intake);
    task.id = "adopt-2".to_string();
    let tree = pin_head(&first.root, &lab.worktrees, &task.id, &intake.head_sha).unwrap();
    let outcome = lab
        .world
        .db
        .insert_adopting(&task, &adopting_events(&task, &intake))
        .unwrap();
    assert_eq!(
        outcome,
        AdoptInsert::AlreadyAdopting {
            task_id: ID.to_string()
        }
    );
    crate::pr_ops::adopt_pin::unpin_head(&first.root, &tree, &intake.head_sha);
    assert!(!tree.path.exists());
    assert!(!lab.ref_exists("refs/heads/aaf/adopt-2"));

    // The first adoption still runs to Done.
    assert_created(&lab, &first);
    lab.turn();
    assert_done_and_cleaned(&lab, "after a refused duplicate");
}
