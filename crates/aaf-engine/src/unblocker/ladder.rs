//! Tiers T2..T5: decide the next unblock action and record it.
//!
//! The ladder never executes an action. Which tiers were already tried for a
//! failure is read back from marker lines in the task's `context_notes`, so
//! the decision survives a factory restart without extra storage.

mod repo_freeze;

use serde::Serialize;

use aaf_domain::blocked::{BlockedReason, UnblockAction, UnblockTier};
use aaf_domain::budget::{BudgetCheck, RootBudget, can_spawn_subtask, scaled_token_limit};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    BudgetStore, Clock, EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN,
    META_FACTORY_UNFROZEN_AT, PortError, PortResult, TaskStore,
};
use aaf_domain::task::Task;

use super::classifier::{
    FailureSignal, budget_detail, classify, extract_gate_diagnostics, failure_digest,
    failure_error_count, one_line, truncate_chars,
};
use super::proposer::{SubtaskProposer, validate_plans};

pub use repo_freeze::{
    DEFAULT_REPO_FREEZE_THRESHOLD, MAX_REPO_PROBE_COUNT, REPO_FREEZE_COOLDOWN_SECS, is_repo_frozen,
    record_repo_root_result, repo_consecutive_fails_key, repo_frozen_key, repo_frozen_until_key,
    repo_probe_count_key, try_begin_repo_probe, unfreeze_repo,
};

const NOTE_MAX_CHARS: usize = 300;
const MAX_T3_ATTEMPTS_ON_PROGRESS: usize = 2;

/// What a task can do after its account ran into a usage limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaRoute {
    /// Another account may take the task now.
    Switch,
    /// No other account may: the task waits for a cooldown to end.
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockDecision {
    pub tier: UnblockTier,
    pub reason: BlockedReason,
    pub action: UnblockAction,
}

pub struct Ladder<'a> {
    pub tasks: &'a dyn TaskStore,
    pub budgets: &'a dyn BudgetStore,
    pub kv: &'a dyn KvStore,
    pub events: &'a dyn EventSink,
    pub clock: &'a dyn Clock,
    pub cfg: &'a UnblockerConfig,
}

#[derive(Serialize)]
struct StepPayload<'a> {
    tier: &'static str,
    reason: &'a BlockedReason,
    action: &'a UnblockAction,
    digest: &'a str,
    cycle: u32,
}

#[derive(Serialize)]
struct FreezePayload<'a> {
    root_id: &'a str,
    consecutive_fails: u32,
    threshold: u32,
}

enum WalkOutcome {
    Decision(UnblockTier, BlockedReason, UnblockAction),
    /// Proposer aborted due to infra/quota; never charge a cycle or emit T5Escalate.
    ProposerTransient(UnblockTier, BlockedReason, UnblockAction),
}

fn to_json<T: Serialize>(value: &T) -> PortResult<String> {
    serde_json::to_string(value).map_err(|e| PortError::Invalid(format!("event payload: {e}")))
}

fn marker(tier: UnblockTier, digest: &str) -> String {
    format!("unblock[{}] digest={digest}", tier.as_str())
}

pub(crate) fn tried(task: &Task, tier: UnblockTier, digest: &str) -> bool {
    let marker = marker(tier, digest);
    // Human rework or unblock resets the unblock tier ladder: scan only notes
    // appended after the most recent `rework:` or `unblocked:` note.
    let start = task
        .context_notes
        .iter()
        .rposition(|n| n.starts_with("rework:") || n.starts_with("unblocked:"))
        .map_or(0, |idx| idx + 1);
    task.context_notes[start..]
        .iter()
        .any(|n| n.starts_with(&marker))
}

fn parse_note_errors(note: &str) -> Option<u32> {
    let rest = note.split("errors=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn t3_progress_state(task: &Task, digest: &str) -> (usize, Option<u32>) {
    let prefix = marker(UnblockTier::T3FreshContext, digest);
    let start = task
        .context_notes
        .iter()
        .rposition(|n| n.starts_with("rework:") || n.starts_with("unblocked:"))
        .map_or(0, |idx| idx + 1);
    let (mut attempts, mut last_errors) = (0usize, None);
    for note in &task.context_notes[start..] {
        if note.starts_with(&prefix) {
            attempts += 1;
            last_errors = parse_note_errors(note);
        }
    }
    (attempts, last_errors)
}

fn reason_text(reason: &BlockedReason) -> &str {
    match reason {
        BlockedReason::MergeConflict { detail }
        | BlockedReason::FlakyTest { detail }
        | BlockedReason::Stuck { detail }
        | BlockedReason::BudgetExceeded { detail } => detail,
        BlockedReason::GateFailed { summary } => summary,
        BlockedReason::NeedsHuman { why } => why,
        BlockedReason::Infra { detail } | BlockedReason::AuditRejected { detail } => detail,
        BlockedReason::Dependency { task_id } => task_id,
        BlockedReason::Quota { .. } => "",
    }
}

impl<'a> Ladder<'a> {
    pub fn new(
        tasks: &'a dyn TaskStore,
        budgets: &'a dyn BudgetStore,
        kv: &'a dyn KvStore,
        events: &'a dyn EventSink,
        clock: &'a dyn Clock,
        cfg: &'a UnblockerConfig,
    ) -> Ladder<'a> {
        Ladder {
            tasks,
            budgets,
            kv,
            events,
            clock,
            cfg,
        }
    }

    /// Charges one unblock cycle, then returns the cheapest action not yet
    /// tried for this failure. Every decision is emitted as an `UnblockStep`.
    pub fn next_action(
        &self,
        task_id: &str,
        signal: &FailureSignal,
        proposer: Option<&dyn SubtaskProposer>,
    ) -> Result<UnblockDecision, PortError> {
        self.next_action_routed(task_id, signal, proposer, None)
    }

    /// [`Ladder::next_action`] told what a usage limit leaves this task (`quota`).
    pub fn next_action_routed(
        &self,
        task_id: &str,
        signal: &FailureSignal,
        proposer: Option<&dyn SubtaskProposer>,
        quota: Option<QuotaRoute>,
    ) -> Result<UnblockDecision, PortError> {
        let quota = quota.unwrap_or(if signal.same_failure_count <= 1 {
            QuotaRoute::Switch
        } else {
            QuotaRoute::Wait
        });
        let now = self.clock.now();
        let task = self.tasks.get_task(task_id)?;
        let digest = failure_digest(signal);
        let classified = classify(signal);
        let gate_diag = extract_gate_diagnostics(signal);
        let error_count = failure_error_count(signal);

        let (cycle, tier, reason, action) = if matches!(
            classified,
            BlockedReason::Infra { .. } | BlockedReason::Quota { .. }
        ) {
            // Upstream infra/quota must not burn task unblock cycles or trigger FreezeRoot.
            let (t, r, a) = match self.walk(
                &task,
                classified,
                &digest,
                gate_diag.as_deref(),
                error_count,
                quota,
                proposer,
                now,
            )? {
                WalkOutcome::Decision(t, r, a) | WalkOutcome::ProposerTransient(t, r, a) => {
                    (t, r, a)
                }
            };
            (task.unblock_cycles, t, r, a)
        } else {
            let mut projected = self.ensure_budget(&task.root_id)?;
            projected.add_usage(0, 0, 1);
            let check = match projected.check() {
                BudgetCheck::Ok => signal.budget,
                other => other,
            };
            if let Some(detail) = budget_detail(check) {
                let cycle = self.tasks.increment_unblock_cycles(task_id, now)?;
                let budget = self.charge_cycle(&task.root_id)?;
                let (t, r, a) = self.freeze_root(&task, &budget, detail, now)?;
                (cycle, t, r, a)
            } else {
                match self.walk(
                    &task,
                    classified,
                    &digest,
                    gate_diag.as_deref(),
                    error_count,
                    quota,
                    proposer,
                    now,
                )? {
                    WalkOutcome::ProposerTransient(tier, reason, action) => {
                        return Ok(UnblockDecision {
                            tier,
                            reason,
                            action,
                        });
                    }
                    WalkOutcome::Decision(t, r, a) => {
                        let cycle = self.tasks.increment_unblock_cycles(task_id, now)?;
                        self.charge_cycle(&task.root_id)?;
                        (cycle, t, r, a)
                    }
                }
            }
        };

        let payload = to_json(&StepPayload {
            tier: tier.as_str(),
            reason: &reason,
            action: &action,
            digest: &digest,
            cycle,
        })?;
        self.events
            .emit(&FactoryEvent::new(EventKind::UnblockStep, payload, now).for_task(task_id))?;
        Ok(UnblockDecision {
            tier,
            reason,
            action,
        })
    }

    fn scaled_budget(&self, root_id: &str) -> RootBudget {
        let active = self.tasks.children_of(root_id).map_or(0, |kids| {
            kids.iter()
                .filter(|t| t.state != aaf_domain::task::TaskState::Cancelled)
                .count()
        });
        let mut b = RootBudget::new(root_id, self.cfg);
        b.token_limit = scaled_token_limit(self.cfg.max_token_budget, active);
        b
    }

    fn ensure_budget(&self, root_id: &str) -> PortResult<RootBudget> {
        self.budgets
            .ensure_budget_if_missing(&self.scaled_budget(root_id))
    }

    fn charge_cycle(&self, root_id: &str) -> PortResult<RootBudget> {
        match self.budgets.add_usage(root_id, 0, 0, 1) {
            // A root that never ran has no budget row yet.
            Err(PortError::NotFound(_)) => {
                self.budgets
                    .ensure_budget_if_missing(&self.scaled_budget(root_id))?;
                self.budgets.add_usage(root_id, 0, 0, 1)
            }
            other => other,
        }
    }

    fn freeze_root(
        &self,
        task: &Task,
        budget: &RootBudget,
        detail: &str,
        now: i64,
    ) -> PortResult<(UnblockTier, BlockedReason, UnblockAction)> {
        let why = format!(
            "{detail} (tokens {}/{}, wall {}s/{}s, cycles {}/{})",
            budget.tokens_used,
            budget.token_limit,
            budget.wall_secs_used,
            budget.wall_secs_limit,
            budget.cycles_used,
            budget.cycles_limit
        );
        if !budget.frozen {
            self.budgets.set_frozen(&task.root_id, true)?;
            let payload = to_json(&serde_json::json!({ "root_id": task.root_id, "why": why }))?;
            self.events.emit(
                &FactoryEvent::new(EventKind::BudgetFrozen, payload, now).for_task(&task.root_id),
            )?;
        }
        Ok((
            UnblockTier::T5Escalate,
            BlockedReason::BudgetExceeded {
                detail: why.clone(),
            },
            UnblockAction::FreezeRoot { why },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        task: &Task,
        reason: BlockedReason,
        digest: &str,
        gate_diag: Option<&str>,
        error_count: Option<u32>,
        quota: QuotaRoute,
        proposer: Option<&dyn SubtaskProposer>,
        now: i64,
    ) -> PortResult<WalkOutcome> {
        let t2 = UnblockTier::T2Mechanical;
        let mechanical = match &reason {
            // Waiting is the only fix for these two, so they never climb;
            // the cycle budget bounds the wait.
            BlockedReason::Quota { reset_at } => Some(match quota {
                QuotaRoute::Switch => UnblockAction::SwitchAccount,
                QuotaRoute::Wait => UnblockAction::WaitForAccount { until: *reset_at },
            }),
            BlockedReason::Dependency { task_id } => Some(UnblockAction::WaitForDependency {
                task_id: task_id.clone(),
            }),
            // One mechanical attempt per failure, then the agent gets it.
            BlockedReason::MergeConflict { .. } if !tried(task, t2, digest) => {
                self.note(task, t2, digest, "rebase and re-gate", now)?;
                Some(UnblockAction::RebaseAndRegate)
            }
            BlockedReason::FlakyTest { .. } if !tried(task, t2, digest) => {
                self.note(task, t2, digest, "rerun tests", now)?;
                Some(UnblockAction::RerunTests)
            }
            BlockedReason::Infra { .. } if !tried(task, t2, digest) => {
                self.note(task, t2, digest, "retry after infrastructure failure", now)?;
                Some(UnblockAction::RetryInfra {
                    detail: reason_text(&reason).to_string(),
                })
            }
            _ => None,
        };
        if let Some(action) = mechanical {
            return Ok(WalkOutcome::Decision(t2, reason, action));
        }
        if matches!(
            reason,
            BlockedReason::NeedsHuman { .. }
                | BlockedReason::BudgetExceeded { .. }
                | BlockedReason::Infra { .. }
                | BlockedReason::AuditRejected { .. }
        ) {
            let why = reason_text(&reason).to_string();
            let (t, r, a) = self.escalate(reason, why);
            return Ok(WalkOutcome::Decision(t, r, a));
        }

        let t3 = UnblockTier::T3FreshContext;
        let skip_t3_for_oversized_diff = matches!(
            &reason,
            BlockedReason::Stuck { detail } if detail.starts_with("diff_too_large:")
        );
        let (t3_attempts, prev_errors) = t3_progress_state(task, digest);
        let t3_making_progress = t3_attempts > 0
            && t3_attempts < MAX_T3_ATTEMPTS_ON_PROGRESS
            && prev_errors.is_some_and(|prev| error_count.is_some_and(|cur| cur < prev));
        if !skip_t3_for_oversized_diff && (t3_attempts == 0 || t3_making_progress) {
            let err_tag = error_count.map_or_else(String::new, |n| format!("errors={n} "));
            let text = format!(
                "{err_tag}previous attempt failed ({}): {}",
                reason.kind(),
                reason_text(&reason)
            );
            self.note(task, t3, digest, &text, now)?;
            if let Some(diag) = gate_diag {
                let diag_note = format!(
                    "gate_diagnostic: <untrusted-data id=\"gate-diagnostic\">{diag}</untrusted-data>"
                );
                self.tasks.append_context_note(&task.id, &diag_note, now)?;
            }
            let action = UnblockAction::RetryFreshContext {
                digest: digest.to_string(),
            };
            return Ok(WalkOutcome::Decision(t3, reason, action));
        }

        let t4 = UnblockTier::T4Decompose;
        if tried(task, t4, digest) {
            let (t, r, a) = self.escalate(reason, "decomposition did not fix this failure".into());
            return Ok(WalkOutcome::Decision(t, r, a));
        }
        if !can_spawn_subtask(task.depth, self.cfg.max_subtask_depth) {
            let (t, r, a) = self.escalate(reason, "subtask depth limit reached".into());
            return Ok(WalkOutcome::Decision(t, r, a));
        }
        let Some(proposer) = proposer else {
            let (t, r, a) = self.escalate(reason, "no subtask proposer available".into());
            return Ok(WalkOutcome::Decision(t, r, a));
        };
        let plans = match proposer.propose(task, &reason, digest) {
            // Proposers are untrusted too: re-validate whatever they return.
            Ok(plans) => validate_plans(plans),
            Err(PortError::Io(msg)) if msg == "account_busy" => {
                return Ok(WalkOutcome::ProposerTransient(
                    t2,
                    BlockedReason::Quota { reset_at: None },
                    UnblockAction::WaitForAccount { until: None },
                ));
            }
            Err(PortError::Io(msg)) if msg.starts_with("infra:") => {
                let raw = msg.trim_start_matches("infra:").trim();
                let detail = if raw.is_empty() {
                    "subtask proposer infrastructure failure".to_string()
                } else {
                    truncate_chars(&one_line(raw), 200)
                };
                let infra = BlockedReason::Infra {
                    detail: detail.clone(),
                };
                return Ok(WalkOutcome::ProposerTransient(
                    t2,
                    infra,
                    UnblockAction::RetryInfra { detail },
                ));
            }
            Err(PortError::Io(msg)) if msg.starts_with("limit_hit:") => {
                let action = match quota {
                    QuotaRoute::Switch => UnblockAction::SwitchAccount,
                    QuotaRoute::Wait => UnblockAction::WaitForAccount { until: None },
                };
                return Ok(WalkOutcome::ProposerTransient(
                    t2,
                    BlockedReason::Quota { reset_at: None },
                    action,
                ));
            }
            Err(e) => {
                let (t, r, a) = self.escalate(reason, format!("subtask proposer failed: {e}"));
                return Ok(WalkOutcome::Decision(t, r, a));
            }
        };
        if plans.is_empty() {
            let (t, r, a) = self.escalate(reason, "no valid subtask plan was proposed".into());
            return Ok(WalkOutcome::Decision(t, r, a));
        }
        let text = format!("split into {} subtasks", plans.len());
        self.note(task, t4, digest, &text, now)?;
        Ok(WalkOutcome::Decision(
            t4,
            reason,
            UnblockAction::SpawnSubtasks { plans },
        ))
    }

    fn escalate(
        &self,
        reason: BlockedReason,
        why: String,
    ) -> (UnblockTier, BlockedReason, UnblockAction) {
        let why = format!("{why}; last failure: {}", reason.kind());
        (
            UnblockTier::T5Escalate,
            reason,
            UnblockAction::EscalateHuman { why },
        )
    }

    /// Only `context_notes` is ever written; the spec stays as created.
    fn note(
        &self,
        task: &Task,
        tier: UnblockTier,
        digest: &str,
        text: &str,
        now: i64,
    ) -> PortResult<()> {
        let note = format!("{}: {}", marker(tier, digest), one_line(text));
        self.tasks
            .append_context_note(&task.id, &truncate_chars(&note, NOTE_MAX_CHARS), now)
    }

    /// Tracks consecutive failed roots; returns whether the factory is frozen.
    pub fn record_root_result(&self, root_id: &str, success: bool) -> Result<bool, PortError> {
        if success {
            self.kv.set_meta(META_CONSECUTIVE_ROOT_FAILS, "0")?;
            return self.is_frozen();
        }
        let fails = self.consecutive_fails()?.saturating_add(1);
        self.kv
            .set_meta(META_CONSECUTIVE_ROOT_FAILS, &fails.to_string())?;
        // A threshold of 0 would freeze before anything failed; treat it as 1.
        let threshold = self.cfg.global_freeze_consecutive_fails.max(1);
        if fails >= threshold && !self.is_frozen()? {
            self.kv.set_meta(META_FACTORY_FROZEN, "1")?;
            let payload = to_json(&FreezePayload {
                root_id,
                consecutive_fails: fails,
                threshold,
            })?;
            self.events.emit(
                &FactoryEvent::new(EventKind::FactoryFrozen, payload, self.clock.now())
                    .for_task(root_id),
            )?;
        }
        self.is_frozen()
    }

    fn consecutive_fails(&self) -> PortResult<u32> {
        match self.kv.get_meta(META_CONSECUTIVE_ROOT_FAILS)? {
            None => Ok(0),
            // A corrupt counter must not silently reset the freeze guard.
            Some(raw) => raw.trim().parse().map_err(|_| {
                PortError::Invalid(format!(
                    "{META_CONSECUTIVE_ROOT_FAILS} is not a number: {raw}"
                ))
            }),
        }
    }

    pub fn is_frozen(&self) -> Result<bool, PortError> {
        Ok(self.kv.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1"))
    }

    pub fn unfreeze(&self) -> Result<(), PortError> {
        let now = self.clock.now();
        self.kv.set_meta(META_FACTORY_FROZEN, "0")?;
        self.kv.set_meta(META_CONSECUTIVE_ROOT_FAILS, "0")?;
        self.kv
            .set_meta(META_FACTORY_UNFROZEN_AT, &now.to_string())?;
        self.events
            .emit(&FactoryEvent::new(EventKind::FactoryUnfrozen, "{}", now))?;
        Ok(())
    }

    pub fn is_repo_frozen(&self, repo_root: &str) -> Result<bool, PortError> {
        is_repo_frozen(self.kv, repo_root, self.clock.now())
    }

    pub fn record_repo_root_result(
        &self,
        repo_root: &str,
        root_id: &str,
        success: bool,
    ) -> Result<bool, PortError> {
        let now = self.clock.now();
        let was_frozen = is_repo_frozen(self.kv, repo_root, now)?;
        let threshold = self.cfg.global_freeze_consecutive_fails.max(1);
        let frozen = repo_freeze::record_repo_root_result_with_threshold(
            self.kv, repo_root, root_id, success, threshold, now,
        )?;
        if frozen && !was_frozen {
            let fails =
                repo_freeze::parse_u32_meta(self.kv, &repo_consecutive_fails_key(repo_root))?;
            let payload = to_json(&FreezePayload {
                root_id,
                consecutive_fails: fails,
                threshold,
            })?;
            self.events.emit(
                &FactoryEvent::new(EventKind::FactoryFrozen, payload, now).for_task(root_id),
            )?;
        }
        Ok(frozen)
    }

    pub fn unfreeze_repo(&self, repo_root: &str) -> Result<(), PortError> {
        unfreeze_repo(self.kv, repo_root)?;
        self.events.emit(&FactoryEvent::new(
            EventKind::FactoryUnfrozen,
            "{}",
            self.clock.now(),
        ))?;
        Ok(())
    }
}
