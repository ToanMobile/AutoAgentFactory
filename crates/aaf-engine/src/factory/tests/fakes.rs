//! In-memory fakes of every port the loop drives. `world.rs` wires them up.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortError, PortResult,
};
use aaf_domain::task::ImmutableSpec;

use crate::cross_audit::build_checklist;
use crate::factory::ports::{AccountPool, GateRunner, Grant, Notifier};
use crate::webhook::WebhookEvent;

pub(super) use super::fakes_git::*;

pub const REPO: &str = "/fake/repo";

pub struct FakeClock(pub AtomicI64);

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Make the parser accept empty input.".to_string(),
        acceptance: vec!["empty input parses to an empty list".to_string()],
    }
}

// ---- gate -----------------------------------------------------------------

/// Makes the first gate run announce itself and wait until released.
pub struct Latch {
    pub entered: Sender<()>,
    pub release: Receiver<()>,
}

#[derive(Default)]
pub struct GateState {
    pub red: HashSet<String>,
    /// Red only when both tasks are in the gated tree.
    pub red_pairs: Vec<(String, String)>,
    pub unverified: bool,
    /// Task id -> lock fingerprint of its locked changes.
    pub locked: HashMap<String, String>,
    /// Task ids in the tree of each gate run, in order.
    pub runs: Vec<Vec<String>>,
    pub latch: Option<Latch>,
    /// Told the task ids of every gate run as it starts.
    pub ran: Option<Sender<Vec<String>>>,
}

pub struct FakeGate {
    pub git: Arc<FakeGit>,
    pub data_dir: PathBuf,
    pub state: Mutex<GateState>,
}

pub fn receipt(tree: &Path, status: GateStatus, reasons: Vec<String>) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: tree.to_string_lossy().into_owned(),
        tree_fp: format!("git-tree:{}", task_of(tree)),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status,
        reasons,
        config_hash: String::new(),
        commands: Vec::new(),
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 0,
    }
}

impl FakeGate {
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().runs.clone()
    }

    fn approved(&self, tree: &Path, lock_fp: &str) -> bool {
        aaf_fp::approval_path(&self.data_dir, tree, lock_fp).is_file()
    }
}

impl GateRunner for FakeGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let ids = self.git.ids(tree);
        let latch = {
            let mut state = self.state.lock().unwrap();
            state.runs.push(ids.clone());
            if let Some(ran) = &state.ran {
                let _ = ran.send(ids.clone());
            }
            state.latch.take()
        };
        if let Some(latch) = latch {
            // A test that already failed dropped its ends: go on, so the
            // run can finish and the failure is reported instead of hanging.
            let _ = latch.entered.send(());
            let _ = latch.release.recv();
        }
        let state = self.state.lock().unwrap();
        // A rebased worktree also holds what landed before it; `runs` keeps
        // naming only the branches the tree was built from.
        let below = {
            let git = self.git.0.lock().unwrap();
            git.rebased_on.get(tree).cloned().unwrap_or_default()
        };
        let has = |id: &String| ids.contains(id) || below.contains(id);
        if state.unverified {
            let reason = "no test command is configured for mode scoped".to_string();
            return Ok(receipt(tree, GateStatus::Unverified, vec![reason]));
        }
        let mut reasons: Vec<String> = ids
            .iter()
            .filter(|id| state.red.contains(*id))
            .map(|id| format!("command test_scoped failed for {id} (exit 1)"))
            .collect();
        for (a, b) in &state.red_pairs {
            if has(a) && has(b) {
                reasons.push(format!("{a} and {b} break each other"));
            }
        }
        for id in &ids {
            if let Some(fp) = state.locked.get(id)
                && !self.approved(tree, fp)
            {
                reasons.push("head lock: 1 locked file(s) changed".to_string());
            }
        }
        let status = if reasons.is_empty() {
            GateStatus::Clear
        } else {
            GateStatus::Blocked
        };
        Ok(receipt(tree, status, reasons))
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let ids = self.git.ids(tree);
        let state = self.state.lock().unwrap();
        let fp = ids.iter().find_map(|id| state.locked.get(id));
        let (status, lock_fp) = match fp {
            None => (LockStatus::Clear, String::new()),
            Some(fp) if self.approved(tree, fp) => (LockStatus::Approved, fp.clone()),
            Some(fp) => (LockStatus::Blocked, fp.clone()),
        };
        Ok(HeadLockReport {
            status,
            lock_fp,
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

// ---- agent runner ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunKind {
    Agent,
    Audit,
    Proposer,
}

pub fn ok_outcome() -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: "done".to_string(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

pub fn failed_outcome(tail: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        log_tail: tail.to_string(),
        ..ok_outcome()
    }
}

pub fn limit_outcome(reset_at: Option<i64>) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        limit_hit: Some(LimitHit {
            reset_at,
            raw: "usage limit reached".to_string(),
        }),
        ..ok_outcome()
    }
}

/// The auditor's answer; ids in `yes` are answered "yes" with concrete evidence.
pub fn audit_answer(yes: &[&str]) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(&spec(), false)
        .iter()
        .map(|item| {
            let hit = yes.contains(&item.id.as_str());
            serde_json::json!({
                "id": item.id,
                "answer": if hit { "yes" } else { "no" },
                "evidence": if hit { "src/lib.rs:2" } else { "" },
            })
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

#[derive(Default)]
pub struct RunnerState {
    /// Scripted agent outcomes per task; without one the run succeeds.
    pub outcomes: HashMap<String, VecDeque<AgentOutcome>>,
    pub audit_text: Option<String>,
    /// The whole outcome of the next audit run, taking `audit_text`'s place.
    pub audit_outcome: Option<AgentOutcome>,
    pub proposer_text: Option<String>,
    pub log: Vec<(RunKind, String, Option<String>)>,
    pub requests: Vec<AgentRequest>,
    /// Told the task id whenever an agent run starts.
    pub started: Option<Sender<String>>,
    /// The agent run of these tasks waits until told to go on.
    pub hold: HashMap<String, Receiver<()>>,
    /// The executor itself fails for these tasks.
    pub broken: HashSet<String>,
}

#[derive(Default)]
pub struct FakeRunner(pub Mutex<RunnerState>);

impl FakeRunner {
    pub fn script(&self, task_id: &str, outcomes: Vec<AgentOutcome>) {
        let mut state = self.0.lock().unwrap();
        state.outcomes.insert(task_id.to_string(), outcomes.into());
    }

    pub fn count(&self, kind: &RunKind) -> usize {
        let state = self.0.lock().unwrap();
        state.log.iter().filter(|(k, _, _)| k == kind).count()
    }
}

impl AgentRunner for FakeRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let task_id = task_of(Path::new(&request.cwd));
        let kind = if request
            .prompt
            .contains(crate::cross_audit::prompt::SPEC_BEGIN)
        {
            RunKind::Audit
        } else if request.prompt.contains("Split it into at") {
            RunKind::Proposer
        } else {
            RunKind::Agent
        };
        let mut state = self.0.lock().unwrap();
        let account = request.account.as_ref().map(|a| a.id.clone());
        state.log.push((kind.clone(), task_id.clone(), account));
        state.requests.push(request.clone());
        let text = |text: &Option<String>, default: String| AgentOutcome {
            final_text: text.clone().unwrap_or(default),
            ..ok_outcome()
        };
        let outcome = match kind {
            RunKind::Audit => match state.audit_outcome.take() {
                Some(outcome) => outcome,
                None => text(&state.audit_text, audit_answer(&[])),
            },
            RunKind::Proposer => text(&state.proposer_text, "[]".to_string()),
            RunKind::Agent => {
                if state.broken.contains(&task_id) {
                    return Err(PortError::Io("executor crashed".to_string()));
                }
                if let Some(started) = &state.started {
                    let _ = started.send(task_id.clone());
                }
                state
                    .outcomes
                    .get_mut(&task_id)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(ok_outcome)
            }
        };
        let hold = match kind {
            RunKind::Agent => state.hold.remove(&task_id),
            _ => None,
        };
        // Not under the lock: other runs go on while this one is held.
        drop(state);
        if let Some(hold) = hold {
            // Dropped sender (failed test): stop holding.
            let _ = hold.recv();
        }
        Ok(outcome)
    }
}

// ---- accounts and notifications -------------------------------------------

#[derive(Default)]
pub struct PoolState {
    pub leased: HashSet<String>,
    pub cooldown: HashMap<String, i64>,
}

/// A rotating pool: the first free account, whoever asks (what
/// `accounts.selection = "least_used"` does; it binds nothing and cannot
/// tell the unblocker what a limit leaves). The modes themselves are tested
/// on the real pool in `selection.rs`.
pub struct FakePool {
    pub accounts: Vec<AccountProfile>,
    pub clock: Arc<FakeClock>,
    pub state: Arc<Mutex<PoolState>>,
}

struct FakeLease {
    id: String,
    state: Arc<Mutex<PoolState>>,
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        self.state.lock().unwrap().leased.remove(&self.id);
    }
}

pub fn account(name: &str) -> AccountProfile {
    AccountProfile {
        id: name.to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: format!("/fake/home/.claude-{name}"),
        name: name.to_string(),
    }
}

impl AccountPool for FakePool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        if self.accounts.is_empty() {
            return Ok(Some(Grant::new(None, None)));
        }
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap();
        for account in &self.accounts {
            let cooling = state.cooldown.get(&account.id).is_some_and(|u| *u > now);
            if cooling || state.leased.contains(&account.id) {
                continue;
            }
            state.leased.insert(account.id.clone());
            let lease = FakeLease {
                id: account.id.clone(),
                state: Arc::clone(&self.state),
            };
            return Ok(Some(Grant::new(
                Some(account.clone()),
                Some(Box::new(lease)),
            )));
        }
        Ok(None)
    }
    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()> {
        let until = hit.reset_at.unwrap_or(self.clock.now() + 18_000);
        let mut state = self.state.lock().unwrap();
        state.cooldown.insert(account_id.to_string(), until);
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.accounts.clone())
    }
}

#[derive(Default)]
pub struct FakeNotifier(pub Mutex<Vec<WebhookEvent>>);

impl Notifier for FakeNotifier {
    fn notify(&self, event: &WebhookEvent) -> Result<bool, String> {
        self.0.lock().unwrap().push(event.clone());
        Ok(true)
    }
}
