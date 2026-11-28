//! A laboratory for the adopt lane: a real repository with a bare origin, a
//! PR branch pinned as `aaf/<id>` in an adopt worktree, a `gh` that answers for
//! one PR, an `Adopting` task with its `PrAdopted` event, and two scripted
//! reviewers of different providers wired into a `StageRouter`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};
use tempfile::TempDir;

use crate::cross_audit::dual_verdict::build_dual_checklist;
use crate::factory::adopt_lane::{self, AdoptEnv};
use crate::factory::stage_router::{StageRouter, StageTarget};
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult};

use crate::factory::tests::fakes::{FakeRunner, spec};
use crate::factory::tests::git_real::git;
use crate::factory::tests::world::World;
use crate::factory::{Deps, RealGit, resolve_base};
use crate::git::cmd::git_raw;
use crate::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};

pub(super) const ID: &str = "adopt-1";
pub(super) const PR: u64 = 7;

/// A `gh` for one PR. It records what is done to it.
pub(super) struct LaneGh {
    pub pr: Mutex<PrInfo>,
    pub checks: Mutex<Vec<PrCheck>>,
    pub comments: Mutex<Vec<String>>,
    pub merges: AtomicUsize,
    pub panic_on_view: Mutex<bool>,
    pub views: AtomicUsize,
    /// `(n, f)`: `f` runs on the n-th `view_pr` call (1-based), before it answers.
    #[allow(clippy::type_complexity)]
    pub on_view: Mutex<Vec<(usize, Box<dyn Fn() + Send>)>>,
    pub merge_error: Mutex<Option<String>>,
    /// Runs inside `merge_pr`, right before it answers.
    #[allow(clippy::type_complexity)]
    pub on_merge: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl LaneGh {
    fn new(head: &str, branch: &str) -> Self {
        let pr = PrInfo {
            number: PR,
            state: "OPEN".to_string(),
            head_ref_oid: head.to_string(),
            head_ref_name: branch.to_string(),
            base_ref_name: "main".to_string(),
            mergeable: "MERGEABLE".to_string(),
            title: format!("PR #{PR}"),
            url: format!("https://github.com/org/repo/pull/{PR}"),
            ..Default::default()
        };
        Self {
            pr: Mutex::new(pr),
            checks: Mutex::new(Vec::new()),
            comments: Mutex::new(Vec::new()),
            merges: AtomicUsize::new(0),
            panic_on_view: Mutex::new(false),
            views: AtomicUsize::new(0),
            on_view: Mutex::new(Vec::new()),
            merge_error: Mutex::new(None),
            on_merge: Mutex::new(None),
        }
    }

    pub fn merges(&self) -> usize {
        self.merges.load(Ordering::SeqCst)
    }

    pub fn comments(&self) -> Vec<String> {
        self.comments.lock().unwrap().clone()
    }

    pub fn set_state(&self, state: &str) {
        self.pr.lock().unwrap().state = state.to_string();
    }

    pub fn set_checks(&self, bucket: &str) {
        *self.checks.lock().unwrap() = vec![PrCheck {
            name: "ci".to_string(),
            state: bucket.to_uppercase(),
            bucket: bucket.to_string(),
        }];
    }
}

impl GhClient for LaneGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        let nth = self.views.fetch_add(1, Ordering::SeqCst) + 1;
        for (at, hook) in self.on_view.lock().unwrap().iter() {
            if *at == nth {
                hook();
            }
        }
        assert!(!*self.panic_on_view.lock().unwrap(), "gh view panicked");
        Ok(self.pr.lock().unwrap().clone())
    }

    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(self.checks.lock().unwrap().clone())
    }

    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        if let Some(err) = self.merge_error.lock().unwrap().clone() {
            return Err(GitError::Invalid(err));
        }
        if let Some(hook) = self.on_merge.lock().unwrap().as_ref() {
            hook();
        }
        self.merges.fetch_add(1, Ordering::SeqCst);
        self.pr.lock().unwrap().state = "MERGED".to_string();
        Ok(())
    }

    fn comment_pr(&self, _: &Path, _: u64, body: &str) -> Result<(), GitError> {
        self.comments.lock().unwrap().push(body.to_string());
        Ok(())
    }
}

/// A scripted reviewer that can also do something right before it answers.
#[derive(Default)]
pub(super) struct Reviewer {
    pub fake: FakeRunner,
    pub before: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl AgentRunner for Reviewer {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        if let Some(hook) = self.before.lock().unwrap().as_ref() {
            hook();
        }
        self.fake.run(request)
    }
}

/// What differs between the base configs of the tests.
pub(super) struct Cfg {
    pub auditor: &'static str,
    pub test: String,
    /// Appended to `[cross_audit]`.
    pub cross: String,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            auditor: "opencode",
            test: "echo ran >> '{LOG}'".to_string(),
            cross: String::new(),
        }
    }
}

/// `Cargo.toml` is a Hard lock in the rust profile. Padded so an edit at the
/// bottom (the PR) and one at the top (the base) merge without a conflict.
pub(super) fn padded_cargo(top: &str, bottom: &str) -> String {
    let pad: String = (0..20).map(|i| format!("# pad {i}\n")).collect();
    format!("{top}[package]\nname = \"adopt\"\nversion = \"0.1.0\"\n{pad}{bottom}")
}

/// What the PR branch changes.
#[derive(Clone, Copy)]
pub(super) enum Pr {
    /// `src/lib.rs` only: no locked file.
    Lib,
    /// `Cargo.toml` (a Hard lock) and `src/lib.rs`.
    Locked,
    /// `.github/workflows/ci.yml`: only a person may approve.
    Workflow,
}

pub(super) struct Lab {
    pub world: World,
    pub gh: Arc<LaneGh>,
    /// The reviewers' scripts: `[codex, opencode]`.
    pub runners: [Arc<Reviewer>; 2],
    pub router: StageRouter,
    pub head: String,
    pub work: PathBuf,
    pub data: PathBuf,
    pub worktrees: PathBuf,
    git: Arc<RealGit>,
    _bare: TempDir,
    _dirs: TempDir,
}

const CONFIG: &str = "[project]\nname = \"adopt\"\nprofile = \"rust\"\n\n[run]\nexecutor = \"codex\"\n\n\
    [commands]\ntest_scoped = \"{TEST}\"\ntest_full = \"true\"\n\n\
    [gate]\nbase_ref = \"origin/main\"\n\n[cross_audit]\nexecutor = \"{AUDITOR}\"\n\
    require_different_provider = false\n{CROSS}\n";

impl Lab {
    pub fn new(kind: Pr) -> Lab {
        Lab::with(kind, Cfg::default())
    }

    pub fn with(kind: Pr, cfg: Cfg) -> Lab {
        Lab::build(kind, cfg, true)
    }

    /// The same repository, remote and PR branch, but nothing adopted yet: no
    /// `aaf/<id>` ref, no adopt worktree, no task. The end-to-end tests make
    /// those with the code `aaf board adopt` runs.
    pub fn unadopted(kind: Pr) -> Lab {
        Lab::build(kind, Cfg::default(), false)
    }

    fn build(kind: Pr, cfg: Cfg, adopt: bool) -> Lab {
        let (bare, dirs) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        // Canonical, as the root `aaf run` and `aaf board adopt` work with
        // (`/var` is a symlink on macOS).
        let base_dir = fs::canonicalize(dirs.path()).unwrap();
        let work = base_dir.join("work");
        let (data, worktrees) = (base_dir.join("data"), base_dir.join("worktrees"));
        fs::create_dir_all(&data).unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);
        fs::create_dir_all(work.join(".autofactory")).unwrap();
        fs::create_dir_all(work.join("src")).unwrap();
        git(&work, &["init", "-b", "main"]);
        for (key, value) in [
            ("user.name", "aaf test"),
            ("user.email", "aaf@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&work, &["config", key, value]);
        }
        git(
            &work,
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        let log = base_dir.join("log");
        let config = CONFIG
            .replace("{TEST}", &cfg.test)
            .replace("{LOG}", &log.to_string_lossy())
            .replace("{AUDITOR}", cfg.auditor)
            .replace("{CROSS}", &cfg.cross);
        fs::write(work.join(".autofactory/config.toml"), config).unwrap();
        fs::write(work.join("Cargo.toml"), padded_cargo("", "")).unwrap();
        fs::write(work.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "init"]);
        git(&work, &["push", "-u", "origin", "main"]);
        let branch = "feat/pr";
        git(&work, &["checkout", "-b", branch, "origin/main"]);
        match kind {
            Pr::Lib => fs::write(work.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap(),
            Pr::Locked => {
                fs::write(work.join("Cargo.toml"), padded_cargo("", "# pr line\n")).unwrap();
                fs::write(work.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
            }
            Pr::Workflow => {
                fs::create_dir_all(work.join(".github/workflows")).unwrap();
                fs::write(work.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
            }
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "pr"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "-u", "origin", branch]);
        git(&work, &["checkout", "main"]);

        if adopt {
            // What `aaf board adopt` leaves: the pin as a ref, and its worktree.
            git(
                &work,
                &["update-ref", &format!("refs/heads/aaf/{ID}"), &head],
            );
            let wt = crate::git::worktree::worktree_path(&work, &worktrees, ID).unwrap();
            fs::create_dir_all(wt.parent().unwrap()).unwrap();
            git(
                &work,
                &[
                    "worktree",
                    "add",
                    "-q",
                    wt.to_str().unwrap(),
                    &format!("aaf/{ID}"),
                ],
            );
        }

        let world = World::new();
        let gh = Arc::new(LaneGh::new(&head, branch));
        let runners: [Arc<Reviewer>; 2] = [Arc::default(), Arc::default()];
        let router = router(&world, &runners, &gh, &work, &data);
        let real = Arc::new(RealGit::new(
            work.clone(),
            worktrees.clone(),
            data.join("locks"),
            resolve_base(&work, "auto").unwrap(),
        ));
        let lab = Lab {
            world,
            gh,
            runners,
            router,
            head,
            work,
            data,
            worktrees,
            git: real,
            _bare: bare,
            _dirs: dirs,
        };
        for runner in &lab.runners {
            lab.script_pass(&runner.fake);
        }
        if adopt {
            lab.add_adopting();
        }
        lab
    }

    /// The deps of the lane: the world's fakes, with this repository, data
    /// directory and real git.
    pub fn deps(&self) -> Deps {
        let mut deps = self.world.deps();
        deps.settings.repo_root = self.work.to_string_lossy().into_owned();
        deps.settings.data_dir = self.data.clone();
        deps.git = self.git.clone();
        deps
    }

    pub fn adopt_wt(&self) -> PathBuf {
        crate::git::worktree::worktree_path(&self.work, &self.worktrees, ID).unwrap()
    }

    fn add_adopting(&self) {
        let root = self.work.to_string_lossy().into_owned();
        let mut task = Task::new_root(ID, &root, "adopt PR", spec(), Priority::P2, 1);
        task.state = TaskState::Adopting;
        self.world.db.insert_task(&task).unwrap();
        let payload = serde_json::json!({
            "pr_number": PR,
            "url": format!("https://github.com/org/repo/pull/{PR}"),
            "head_sha": self.head,
            "head_ref": "feat/pr",
            "base_ref": "main",
            "author_login": "owner",
            "declared_executor": null,
        });
        let event = FactoryEvent::new(EventKind::PrAdopted, payload.to_string(), 1).for_task(ID);
        self.world.db.emit(&event).unwrap();
    }

    /// A reviewer that finds nothing.
    pub fn script_pass(&self, runner: &FakeRunner) {
        let answers: Vec<_> = build_dual_checklist(&spec(), false, false)
            .iter()
            .map(|i| serde_json::json!({"id": i.id, "answer": "no", "evidence": ""}))
            .collect();
        runner.0.lock().unwrap().audit_text =
            Some(serde_json::json!({"answers": answers, "issues": []}).to_string());
    }

    /// A reviewer that blocks the PR on a real line of its diff.
    pub fn script_fail(&self, runner: &FakeRunner) {
        let answers: Vec<_> = build_dual_checklist(&spec(), false, false)
            .iter()
            .map(|i| serde_json::json!({"id": i.id, "answer": "no", "evidence": ""}))
            .collect();
        let issue = serde_json::json!({
            "id": "i1", "severity": "P1", "category": "bug", "location": "src/lib.rs:1",
            "description": "the answer is wrong", "proposed_fix": "restore it",
        });
        runner.0.lock().unwrap().audit_text =
            Some(serde_json::json!({"answers": answers, "issues": [issue]}).to_string());
    }

    /// One lane turn.
    pub fn turn(&self) -> adopt_lane::AdoptDone {
        adopt_lane::run_job(&self.deps(), &self.router, ID)
    }

    pub fn task(&self) -> Task {
        self.world.task(ID)
    }

    pub fn state(&self) -> TaskState {
        self.world.state(ID)
    }

    pub fn events(&self, kind: EventKind) -> Vec<FactoryEvent> {
        self.world
            .db
            .events_for_task_by_kinds(ID, &[kind], 200)
            .unwrap()
    }

    /// The `code` of the `AdoptAborted` event, if the task was cancelled.
    pub fn abort_code(&self) -> Option<String> {
        let event = self.events(EventKind::AdoptAborted).into_iter().next()?;
        let value: serde_json::Value = serde_json::from_str(&event.payload_json).ok()?;
        value.get("code")?.as_str().map(str::to_string)
    }

    /// How many reviewer prompts the two scripts have been asked.
    pub fn reviewer_runs(&self) -> usize {
        self.runners
            .iter()
            .map(|r| r.fake.0.lock().unwrap().requests.len())
            .sum()
    }

    /// Moves the base: a commit on `origin/main` that edits `edit`.
    pub fn advance_base(&self, edit: impl FnOnce(&Path)) {
        git(&self.work, &["checkout", "main"]);
        edit(&self.work);
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-m", "base moves"]);
        git(&self.work, &["push", "origin", "main"]);
    }

    pub fn clock_to(&self, now: i64) {
        self.world
            .clock
            .0
            .store(now, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a signed approval of any lock fingerprint is on disk.
    pub fn approvals_on_disk(&self) -> usize {
        fn count(dir: &Path) -> usize {
            fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|e| {
                        let path = e.path();
                        if path.is_dir() {
                            count(&path)
                        } else {
                            usize::from(path.to_string_lossy().contains("approval"))
                        }
                    })
                    .sum()
            })
        }
        count(&self.data)
    }

    pub fn ref_exists(&self, name: &str) -> bool {
        git_raw(&self.work, &["show-ref", "--verify", "--quiet", name])
            .map(|o| o.success())
            .unwrap_or(false)
    }
}

/// The router of the lane: the two adopt reviewers (`codex` runs, `opencode`
/// audits) and the adopt environment with the fake `gh`.
fn router(
    world: &World,
    runners: &[Arc<Reviewer>; 2],
    gh: &Arc<LaneGh>,
    work: &Path,
    data: &Path,
) -> StageRouter {
    let target = |exec: &str, runner: &Arc<Reviewer>| {
        let mut t = StageTarget::new(exec, "", "", "", runner.clone(), world.pool.clone());
        t.hook_required = false;
        (exec.to_string(), t)
    };
    let template = AdoptEnv::request_template(
        work.to_path_buf(),
        data.to_path_buf(),
        "origin",
        None,
        false,
    );
    let default = StageTarget::new(
        "codex",
        "",
        "",
        "",
        world.runner.clone(),
        world.pool.clone(),
    );
    StageRouter::new(default)
        .with_adopt_reviewers(vec![
            target("codex", &runners[0]),
            target("opencode", &runners[1]),
        ])
        .with_adopt_env(AdoptEnv::new(gh.clone(), template))
}

impl Lab {
    /// How many times the base config's test command ran (the gate's commands).
    pub fn commands_ran(&self) -> usize {
        fs::read_to_string(self._dirs.path().join("log"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
