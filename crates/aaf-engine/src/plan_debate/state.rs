//! Epoch-aware Plan Debate replay, mechanical critique verification,
//! UPHELD issue carry-forward, and convergence/stagnation transitions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::ChangeKind;
use aaf_domain::pipeline::{
    IssueSeverity, PlanContract, PlanCritique, PlanCritiqueVerdict, ResolvedStatus, ReviewIssue,
};
use aaf_domain::task::{Priority, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::MAX_CONSECUTIVE_TURN_FAILS;
use super::schema::{canonical_plan_hash, normalize_repo_path};
use crate::gate::changes::Change;
use crate::gate::docs_fast;

/// Active role executing one Plan Debate turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebateRole {
    Author,
    Challenger,
}

/// Next deterministic step computed from a replayed [`DebateSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebateNext {
    RunAuthor {
        round: u32,
    },
    RunChallenger {
        round: u32,
    },
    Converged {
        plan: Box<PlanContract>,
        plan_hash: String,
    },
    Stalled {
        reason: String,
    },
}

/// Replayed state of the current Plan Debate epoch for one task.
#[derive(Debug, Clone, Default)]
pub struct DebateSnapshot {
    pub round: u32,
    pub latest_plan: Option<PlanContract>,
    pub latest_plan_hash: Option<String>,
    pub latest_critique: Option<PlanCritique>,
    pub open_issues: Vec<ReviewIssue>,
    pub round_issue_sets: Vec<(BTreeSet<(String, String)>, usize)>,
    pub consecutive_fails: u32,
    pub converged: Option<(PlanContract, String)>,
    pub stalled_reason: Option<String>,
    pub pending_challenger: bool,
}

/// Result of mechanically checking a Challenger's [`PlanCritique`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CritiqueEvaluation {
    pub converged: bool,
    pub stalled: bool,
    pub stall_reason: Option<String>,
    pub open_issues: Vec<ReviewIssue>,
    pub issue_set: BTreeSet<(String, String)>,
    pub dropped_locations: Vec<String>,
    pub failed_checks: Vec<String>,
}

/// Returns `true` when `event` is a human unblock transition (`Blocked -> Ready`)
/// that starts a fresh Plan Debate epoch (R3-CONC-3).
///
/// Single source of truth shared by Plan Debate replay, unblocker ladder, and CLI
/// plan lookup: excludes `why == "plan_approved"` and `plan_approved == true` so
/// human plan approval never hides its companion `PlanConverged` event.
pub fn is_human_unblock_event(event: &FactoryEvent) -> bool {
    if event.kind != EventKind::TaskStateChanged {
        return false;
    }
    let Ok(p) = serde_json::from_str::<Value>(&event.payload_json) else {
        return false;
    };
    if p["to"].as_str() != Some("ready")
        || p["why"].as_str() == Some("plan_approved")
        || p["plan_approved"].as_bool() == Some(true)
    {
        return false;
    }
    if p["from"].as_str().is_some_and(|from| from != "blocked") {
        return false;
    }
    let why_is_unblock = p["why"].as_str().is_some_and(|w| {
        w == "unblocked"
            || w.ends_with(" unblock")
            || w.contains(" unblock ")
            || w.ends_with(" unblock-all")
    });
    let blocked_by_human = (p["from"].as_str() == Some("blocked") || p.get("was").is_some())
        && p["by"].as_str() == Some("human");
    why_is_unblock || blocked_by_human
}

/// Replays debate events (supplied newest-first) for the current epoch.
///
/// Stops scanning immediately upon hitting [`EventKind::ReworkRequested`] or
/// [`is_human_unblock_event`] so `steer + unblock` starts Round 1 cleanly
/// (R3-CONC-3).
pub fn replay_debate_state(events_newest_first: &[FactoryEvent]) -> DebateSnapshot {
    let epoch: Vec<&FactoryEvent> = events_newest_first
        .iter()
        .take_while(|ev| ev.kind != EventKind::ReworkRequested && !is_human_unblock_event(ev))
        .collect();
    let mut snap = DebateSnapshot::default();
    for ev in epoch.into_iter().rev() {
        match ev.kind {
            EventKind::PlanConverged => apply_plan_converged(&mut snap, &ev.payload_json),
            EventKind::PlanDebateTurn => apply_plan_debate_turn(&mut snap, &ev.payload_json),
            _ => {}
        }
    }
    snap
}

fn parse_plan_and_hash(payload: &Value) -> Option<(PlanContract, String)> {
    let plan: PlanContract = serde_json::from_value(payload.get("plan_contract")?.clone()).ok()?;
    let hash = payload["plan_contract_hash"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| canonical_plan_hash(&plan));
    Some((plan, hash))
}

fn apply_plan_converged(snap: &mut DebateSnapshot, payload_json: &str) {
    let Ok(payload) = serde_json::from_str::<Value>(payload_json) else {
        return;
    };
    let Some((plan, hash)) = parse_plan_and_hash(&payload) else {
        return;
    };
    snap.latest_plan = Some(plan.clone());
    snap.latest_plan_hash = Some(hash.clone());
    snap.converged = Some((plan, hash));
    snap.stalled_reason = None;
    snap.pending_challenger = false;
    snap.consecutive_fails = 0;
}

fn apply_plan_debate_turn(snap: &mut DebateSnapshot, payload_json: &str) {
    let Ok(payload) = serde_json::from_str::<Value>(payload_json) else {
        return;
    };
    if !payload["ok"].as_bool().unwrap_or(false) {
        snap.consecutive_fails = snap.consecutive_fails.saturating_add(1);
        return;
    }
    let Some(role) = payload["role"].as_str() else {
        return;
    };
    let round = payload["round"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok());
    if matches!(role, "author" | "challenger") {
        snap.consecutive_fails = 0;
        if let Some(r) = round {
            snap.round = r;
        }
    }
    match role {
        "author" => {
            if let Some((plan, hash)) = parse_plan_and_hash(&payload) {
                snap.latest_plan = Some(plan);
                snap.latest_plan_hash = Some(hash);
                snap.pending_challenger = true;
                snap.stalled_reason = None;
            }
        }
        "challenger" => {
            if let Ok(critique) =
                serde_json::from_value::<PlanCritique>(payload["critique"].clone())
            {
                snap.latest_critique = Some(critique);
            }
            if let Ok(issues) =
                serde_json::from_value::<Vec<ReviewIssue>>(payload["open_issues"].clone())
            {
                snap.open_issues = issues;
            }
            let issue_set =
                serde_json::from_value::<Vec<(String, String)>>(payload["issue_set"].clone())
                    .map(|pairs| pairs.into_iter().collect::<BTreeSet<_>>())
                    .unwrap_or_else(|_| compute_issue_set(&snap.open_issues));
            let open_count = payload["open_count"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(snap.open_issues.len());
            snap.round_issue_sets.push((issue_set, open_count));
            snap.pending_challenger = false;
            snap.stalled_reason = if payload["stalled"].as_bool() == Some(true) {
                Some(
                    payload["stall_reason"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("round {} stalled without convergence", snap.round)
                        }),
                )
            } else {
                None
            };
        }
        _ => {}
    }
}

fn strip_line_suffix(location: &str) -> &str {
    let is_digits = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    let mut path = location.trim();
    for _ in 0..2 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        let is_spec = is_digits(tail)
            || tail
                .split_once('-')
                .is_some_and(|(a, b)| is_digits(a) && is_digits(b));
        if is_spec && !head.is_empty() {
            path = head;
        } else {
            break;
        }
    }
    path
}

fn canonical_issue_location(location: &str) -> String {
    normalize_repo_path(strip_line_suffix(location)).unwrap_or_else(|_| location.trim().to_string())
}

fn compute_issue_set(open_issues: &[ReviewIssue]) -> BTreeSet<(String, String)> {
    open_issues
        .iter()
        .filter(|i| matches!(i.severity, IssueSeverity::P0 | IssueSeverity::P1))
        .map(|i| {
            (
                i.category.trim().to_ascii_lowercase(),
                canonical_issue_location(&i.location),
            )
        })
        .collect()
}

fn is_deterministic_docs_only_plan(task: &Task, cfg: &RepoConfig, plan: &PlanContract) -> bool {
    if task.priority == Priority::P0 || plan.files_to_change.is_empty() {
        return false;
    }
    let Some(changes): Option<Vec<Change>> = plan
        .files_to_change
        .iter()
        .map(|f| {
            normalize_repo_path(&f.path).ok().map(|path| Change {
                kind: ChangeKind::Modified,
                path,
                old_path: None,
            })
        })
        .collect()
    else {
        return false;
    };
    matches!(
        docs_fast::is_docs_only(
            &changes,
            &cfg.gate.head_lock_globs,
            &cfg.gate.docs_fast_path_exclude_globs,
        ),
        Ok(true)
    )
}

fn check_required_checklist(task: &Task, critique: &PlanCritique, failed: &mut Vec<String>) {
    let required = (1..=task.spec.acceptance.len())
        .map(|i| format!("PLAN-AC-{i}"))
        .chain(["PLAN-SCOPE", "PLAN-SEC", "PLAN-TEST", "PLAN-ROLLBACK"].map(str::to_string));
    for req in required {
        let mut found = false;
        let mut all_pass = true;
        for item in &critique.checklist {
            if item.id.trim().eq_ignore_ascii_case(&req) {
                found = true;
                all_pass &= item.pass;
            }
        }
        if !found {
            failed.push(format!("missing required checklist item {req}"));
        } else if !all_pass {
            failed.push(format!("checklist item {req} failed"));
        }
    }
}

fn check_plan_structure(
    task: &Task,
    cfg: &RepoConfig,
    plan: &PlanContract,
    failed: &mut Vec<String>,
) {
    if plan.files_to_change.is_empty() {
        failed.push("plan.files_to_change must not be empty".to_string());
    }
    if !task.spec.acceptance.is_empty() {
        for idx in 1..=task.spec.acceptance.len() {
            let expected = format!("AC-{idx}");
            if !plan
                .red_green_tests
                .iter()
                .any(|t| t.ac_ref.trim().eq_ignore_ascii_case(&expected))
            {
                failed.push(format!("missing red_green_tests coverage for {expected}"));
            }
        }
    } else if plan.red_green_tests.is_empty() && !is_deterministic_docs_only_plan(task, cfg, plan) {
        failed.push(
            "plan.red_green_tests requires at least 1 test for non-docs or P0 task".to_string(),
        );
    }
}

/// Mechanically verifies a Challenger's [`PlanCritique`] against all 6 Plan
/// Debate checks (§D.2.3 & R3-PROTO-4), checking convergence BEFORE stagnation
/// and round cap.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_critique(
    task: &Task,
    repo_root: &Path,
    cfg: &RepoConfig,
    round: u32,
    plan: &PlanContract,
    critique: &PlanCritique,
    prior_open_issues: &[ReviewIssue],
    prior_round_sets: &[(BTreeSet<(String, String)>, usize)],
) -> CritiqueEvaluation {
    let (mut failed_checks, mut dropped_locations) = (Vec::new(), Vec::new());
    check_required_checklist(task, critique, &mut failed_checks);

    let planned: BTreeSet<String> = plan
        .files_to_change
        .iter()
        .filter_map(|f| normalize_repo_path(&f.path).ok())
        .collect();
    let (mut open_issues, mut seen_ids) = (Vec::new(), BTreeSet::new());

    for issue in &critique.issues {
        let valid = normalize_repo_path(strip_line_suffix(&issue.location))
            .is_ok_and(|norm| repo_root.join(&norm).exists() || planned.contains(&norm));
        if !valid {
            dropped_locations.push(issue.location.clone());
        } else if matches!(issue.severity, IssueSeverity::P0 | IssueSeverity::P1)
            && (issue.id.is_empty() || seen_ids.insert(issue.id.clone()))
        {
            open_issues.push(issue.clone());
        }
    }

    let mut resolved: BTreeMap<&str, ResolvedStatus> = BTreeMap::new();
    for res in &critique.resolved_from_previous {
        resolved
            .entry(res.issue_id.trim())
            .and_modify(|cur| {
                if res.status == ResolvedStatus::Upheld {
                    *cur = ResolvedStatus::Upheld;
                }
            })
            .or_insert(res.status);
    }
    for prior in prior_open_issues {
        let blocking = matches!(prior.severity, IssueSeverity::P0 | IssueSeverity::P1);
        let cleared = matches!(
            resolved.get(prior.id.trim()),
            Some(ResolvedStatus::VerifiedFixed | ResolvedStatus::Withdrawn)
        );
        if blocking && !cleared && (prior.id.is_empty() || seen_ids.insert(prior.id.clone())) {
            open_issues.push(prior.clone());
        }
    }

    check_plan_structure(task, cfg, plan, &mut failed_checks);
    let issue_set = compute_issue_set(&open_issues);
    let open_count = open_issues.len();

    // Check convergence FIRST before stagnation or round cap (§D.2.4).
    let converged = failed_checks.is_empty()
        && open_issues.is_empty()
        && critique.verdict == PlanCritiqueVerdict::Converged;

    let stall_reason = if converged {
        None
    } else if !issue_set.is_empty()
        && prior_round_sets
            .iter()
            .any(|(prev_set, prev_count)| *prev_set == issue_set && open_count >= *prev_count)
    {
        Some(format!(
            "round {round}: repeated open issue set without reduction ({open_count} open P0/P1)"
        ))
    } else if let [.., (_, p2), (_, p1)] = prior_round_sets
        && open_count >= *p1
        && *p1 >= *p2
    {
        Some(format!(
            "round {round}: non-decreasing open issue count across 3 rounds ({p2} -> {p1} -> {open_count})"
        ))
    } else if round >= cfg.pipeline.max_plan_rounds {
        Some(format!(
            "round {round} reached max_plan_rounds ({}) without convergence",
            cfg.pipeline.max_plan_rounds
        ))
    } else {
        None
    };

    CritiqueEvaluation {
        converged,
        stalled: stall_reason.is_some(),
        stall_reason,
        open_issues,
        issue_set,
        dropped_locations,
        failed_checks,
    }
}

/// Computes the next deterministic action from a replayed [`DebateSnapshot`].
pub fn next_step(snapshot: &DebateSnapshot, max_rounds: u32) -> DebateNext {
    if let Some((plan, hash)) = &snapshot.converged {
        return DebateNext::Converged {
            plan: Box::new(plan.clone()),
            plan_hash: hash.clone(),
        };
    }
    if let Some(reason) = &snapshot.stalled_reason {
        return DebateNext::Stalled {
            reason: reason.clone(),
        };
    }
    if snapshot.consecutive_fails >= MAX_CONSECUTIVE_TURN_FAILS {
        return DebateNext::Stalled {
            reason: format!(
                "{} consecutive plan debate turn failures",
                snapshot.consecutive_fails
            ),
        };
    }
    if snapshot.pending_challenger && snapshot.latest_plan.is_some() {
        return DebateNext::RunChallenger {
            round: snapshot.round,
        };
    }
    let next_round = snapshot.round.saturating_add(1);
    if next_round > max_rounds {
        return DebateNext::Stalled {
            reason: format!("reached max_plan_rounds ({max_rounds}) without convergence"),
        };
    }
    DebateNext::RunAuthor { round: next_round }
}
