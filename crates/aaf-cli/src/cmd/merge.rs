//! `aaf merge <PR> --expect-head <sha40>`: locked Pull Request merge gate.
//!
//! Exit 0 for `MERGED`, `DRY_RUN_CLEAR`, or `ALREADY_MERGED`; exit 2 for
//! policy blocks (head mismatch, conflict, CI failure, unapproved head-lock,
//! gate blocked); exit 1 for operational/git/gh errors.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::GateMode;
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::gate::GIT_TIMEOUT;
use aaf_engine::git::{GhClient, MergeStrategy, SystemGh, main_work_tree, worktree};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr_with_cwd};
use aaf_engine::throttle::Progress;
use clap::ValueEnum;
use serde_json::{Value, json};

use super::board::store::{open_db, refuse_inside_agent_run};
use super::run::report::attach_single_handover_report;
use crate::cmd::gate::progress_line;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Scoped,
    Full,
    Release,
}

impl From<ModeArg> for GateMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Scoped => GateMode::Scoped,
            ModeArg::Full => GateMode::Full,
            ModeArg::Release => GateMode::Release,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyArg {
    Squash,
    Merge,
    Rebase,
}

impl From<StrategyArg> for MergeStrategy {
    fn from(s: StrategyArg) -> Self {
        match s {
            StrategyArg::Squash => MergeStrategy::Squash,
            StrategyArg::Merge => MergeStrategy::Merge,
            StrategyArg::Rebase => MergeStrategy::Rebase,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// Pull request number to verify and merge (omit when using `--task`).
    #[arg(required_unless_present = "task", conflicts_with = "task")]
    pub pr_number: Option<u64>,
    /// Exact 40-character commit SHA expected at the head of the PR (auto-resolved with `--task`).
    #[arg(long, required_unless_present = "task")]
    pub expect_head: Option<String>,
    /// Resolve PR number and head SHA from a board task id, and mark it Done on merge.
    #[arg(long, conflicts_with = "pr_number")]
    pub task: Option<String>,
    /// Gate mode to run on the merged tree (default: scoped).
    #[arg(long, value_enum, default_value_t = ModeArg::Scoped)]
    mode: ModeArg,
    /// GitHub merge strategy (default: squash).
    #[arg(long, value_enum, default_value_t = StrategyArg::Squash)]
    strategy: StrategyArg,
    /// Git remote name (default: origin).
    #[arg(long, default_value = "origin")]
    remote: String,
    /// Optional commit subject passed to `gh pr merge --subject`.
    #[arg(long)]
    subject: Option<String>,
    /// Optional commit body file passed to `gh pr merge --body-file`.
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Seconds to wait for pending GitHub PR checks before failing closed.
    #[arg(long, default_value_t = 0)]
    ci_wait: u64,
    /// Seconds to wait for the per-repo merge lock.
    #[arg(long, default_value_t = 1800)]
    lock_timeout: u64,
    /// Verify the merged tree under the lock without merging on GitHub.
    #[arg(long)]
    dry_run: bool,
}

fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    let trimmed = url.trim().split(['?', '#']).next()?.trim_end_matches('/');
    let mut parts = trimmed.rsplit('/');
    let num = parts.next()?.parse::<u64>().ok()?;
    let seg = parts.next()?;
    (num > 0 && matches!(seg, "pull" | "pulls")).then_some(num)
}

fn find_pr_number_in_events(events: &[FactoryEvent]) -> Option<u64> {
    for ev in events {
        let Ok(payload) = serde_json::from_str::<Value>(&ev.payload_json) else {
            continue;
        };
        if ev.kind == EventKind::PrOpened
            && let Some(url) = payload["url"].as_str()
            && let Some(num) = parse_pr_number_from_url(url)
        {
            return Some(num);
        }
        if ev.kind == EventKind::TaskStateChanged
            && let Some(why) = payload["why"].as_str()
            && let Some(rest) = why.strip_prefix("pull request ")
            && let Some(num) = parse_pr_number_from_url(rest)
        {
            return Some(num);
        }
    }
    None
}

fn lookup_pr_via_gh_branch(repo_root: &Path, branch: &str) -> Result<(u64, String), CmdError> {
    let gh_bin = aaf_fp::program("gh").map_err(|e| CmdError::new(format!("merge: {e}")))?;
    let out = Command::new(gh_bin)
        .args(["pr", "view", branch, "--json", "number,headRefOid"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CmdError::new(format!("merge: gh pr view {branch}: {e}")))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(CmdError::new(format!(
            "merge: no open PR found for branch {branch}: {detail}"
        )));
    }
    let val: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| CmdError::new(format!("merge: unparsable gh pr view JSON: {e}")))?;
    let num = val["number"]
        .as_u64()
        .filter(|&n| n > 0)
        .ok_or_else(|| CmdError::new(format!("merge: gh pr view {branch} missing number")))?;
    let head = val["headRefOid"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok((num, head))
}

fn resolve_task_merge_target(
    args: &MergeArgs,
    ctx: &Ctx,
    task_id: &str,
) -> Result<(PathBuf, u64, String, TaskState), CmdError> {
    let task_id = task_id.trim();
    worktree::validate_task_id(task_id)
        .map_err(|e| CmdError::new(format!("merge: invalid task id: {e}")))?;
    let db = open_db(ctx)?;
    let task = db
        .get_task(task_id)
        .map_err(|e| CmdError::new(format!("merge: task {task_id}: {e}")))?;
    let repo_root = if !task.repo_root.is_empty() && Path::new(&task.repo_root).is_dir() {
        main_work_tree(Path::new(&task.repo_root))
    } else {
        main_work_tree(&ctx.repo_root()?)
    };
    let events = db
        .events_for_task(&task.id, 100)
        .map_err(|e| CmdError::new(format!("merge: events for {task_id}: {e}")))?;
    let branch = format!("{}{task_id}", worktree::BRANCH_PREFIX);
    let mut gh_head: Option<String> = None;
    let pr_number = match find_pr_number_in_events(&events) {
        Some(n) => n,
        None => {
            let (n, h) = lookup_pr_via_gh_branch(&repo_root, &branch)?;
            if !h.is_empty() {
                gh_head = Some(h);
            }
            n
        }
    };
    let expect_head = if let Some(h) = &args.expect_head {
        h.clone()
    } else if let Ok(Some((_, h))) =
        worktree::existing_head(&repo_root, &ctx.worktrees_dir, task_id)
    {
        h
    } else {
        let remote_ref = format!("refs/remotes/{}/{branch}", args.remote);
        if let Ok(h) = aaf_fp::git(
            &repo_root,
            &["rev-parse", "--verify", "--quiet", &remote_ref],
            &[],
            GIT_TIMEOUT,
        ) && !h.is_empty()
        {
            h
        } else if let Some(h) = gh_head {
            h
        } else {
            SystemGh
                .view_pr(&repo_root, pr_number)
                .map_err(|e| {
                    CmdError::new(format!("merge: resolve head for PR #{pr_number}: {e}"))
                })?
                .head_ref_oid
        }
    };
    Ok((repo_root, pr_number, expect_head, task.state))
}

fn mark_task_done(
    ctx: &Ctx,
    task_id: &str,
    prev_state: TaskState,
    pr_number: u64,
    head_sha: &str,
) -> Result<bool, CmdError> {
    if !prev_state.can_transition_to(TaskState::Done) {
        return Ok(false);
    }
    let db = open_db(ctx)?;
    let now = SystemClock.now();
    let changed = db
        .compare_and_set_state(task_id, prev_state, TaskState::Done, None, now)
        .map_err(|e| CmdError::new(format!("merge: mark {task_id} done: {e}")))?;
    if !changed {
        return Ok(false);
    }
    let state_payload = json!({
        "from": prev_state.as_str(),
        "to": TaskState::Done.as_str(),
        "why": format!("merged PR #{pr_number}"),
        "by": "human",
        "pr_number": pr_number,
        "head_sha": head_sha,
    });
    let _ = db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, state_payload.to_string(), now)
            .for_task(task_id),
    );
    let merge_payload = json!({
        "merged": true,
        "kind": "pr_merge",
        "by": "human",
        "pr_number": pr_number,
        "head_sha": head_sha,
    });
    let _ = db.emit(
        &FactoryEvent::new(EventKind::MergeResult, merge_payload.to_string(), now)
            .for_task(task_id),
    );
    Ok(true)
}

pub fn run(args: &MergeArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf merge") {
        return Ok(refused);
    }
    let (repo_root, pr_number, expect_head, task_ctx) = if let Some(task_id) = args.task.as_deref()
    {
        let (root, pr, head, state) = resolve_task_merge_target(args, ctx, task_id)?;
        (root, pr, head, Some((task_id.trim().to_string(), state)))
    } else {
        let pr = args
            .pr_number
            .ok_or_else(|| CmdError::new("merge: PR number or --task is required"))?;
        let head = args
            .expect_head
            .clone()
            .ok_or_else(|| CmdError::new("merge: --expect-head or --task is required"))?;
        (main_work_tree(&ctx.repo_root()?), pr, head, None)
    };
    let cfg = ctx.load_config(&repo_root)?;
    let req = PrMergeRequest {
        repo_root,
        data_dir: ctx.data_dir.clone(),
        pr_number,
        expect_head,
        remote: args.remote.clone(),
        mode: args.mode.into(),
        strategy: args.strategy.into(),
        subject: args.subject.clone(),
        body_file: args.body_file.clone(),
        ci_wait_secs: args.ci_wait,
        require_checks: false,
        lock_timeout: Duration::from_secs(args.lock_timeout),
        dry_run: args.dry_run,
        machine: cfg.global.machine.clone(),
        global_config: Some(cfg.global.clone()),
    };
    let progress = |event: &Progress<'_>| notice("merge", &progress_line(event));
    aaf_engine::git::ensure_plain_reflogs(&req.repo_root)
        .map_err(|e| CmdError::new(format!("merge: {e}")))?;
    let (outcome, cleanup) = merge_pr_with_cwd(
        &req,
        Some(ctx.cwd.as_path()),
        &SystemGh,
        &SystemClock,
        &progress,
    )
    .map_err(|e| CmdError::new(format!("merge: {e}")))?;

    match outcome {
        PrMergeOutcome::Merged {
            pr_number,
            head_sha,
            base_sha,
            receipt,
            local_sync_note,
        } => {
            let mut marked_done = cleanup.as_ref().is_some_and(|c| c.task_marked_done);
            if let Some((ref task_id, prev_state)) = task_ctx
                && !marked_done
            {
                marked_done = mark_task_done(ctx, task_id, prev_state, pr_number, &head_sha)?;
            }
            let report_task_id = (!args.dry_run)
                .then(|| {
                    task_ctx
                        .as_ref()
                        .map(|(id, _)| id.clone())
                        .or_else(|| cleanup.as_ref().and_then(|c| c.task_id.clone()))
                })
                .flatten();
            let text = format!(
                "MERGED PR #{pr_number}\n  head: {head_sha}\n  base: {base_sha}\n  tree: {}\n  local: {local_sync_note}",
                receipt.tree_fp
            );
            let mut out = CmdOutput::ok("MERGED", text)
                .field("pr_number", json!(pr_number))
                .field("head_sha", json!(head_sha))
                .field("base_sha", json!(base_sha))
                .field("local_sync_note", json!(local_sync_note))
                .field("receipt", json!(receipt));
            if let Some(report) = cleanup {
                out = out.field("cleanup", json!(report));
            }
            if let Some((task_id, _)) = task_ctx {
                out = out
                    .field("task_id", json!(task_id))
                    .field("task_marked_done", json!(marked_done));
            }
            Ok(attach_single_handover_report(
                out,
                ctx,
                report_task_id.as_deref(),
                &cfg.repo,
            ))
        }
        PrMergeOutcome::DryRunClear {
            pr_number,
            head_sha,
            base_sha,
            receipt,
        } => {
            let text = format!(
                "DRY_RUN_CLEAR PR #{pr_number}\n  head: {head_sha}\n  base: {base_sha}\n  tree: {}",
                receipt.tree_fp
            );
            Ok(CmdOutput::ok("DRY_RUN_CLEAR", text)
                .field("pr_number", json!(pr_number))
                .field("head_sha", json!(head_sha))
                .field("base_sha", json!(base_sha))
                .field("dry_run", json!(true))
                .field("receipt", json!(receipt)))
        }
        PrMergeOutcome::AlreadyMerged { pr_number, url } => {
            let report_task_id = (!args.dry_run && cleanup.is_some())
                .then(|| {
                    cleanup
                        .as_ref()
                        .and_then(|c| c.task_id.clone())
                        .or_else(|| task_ctx.as_ref().map(|(id, _)| id.clone()))
                })
                .flatten();
            let mut text = format!("ALREADY_MERGED PR #{pr_number} ({url})");
            if let Some(summary) = cleanup.as_ref().and_then(|c| c.summary_note()) {
                text.push_str(&format!("\n  cleanup: {summary}"));
            }
            let mut out = CmdOutput::ok("ALREADY_MERGED", text)
                .field("pr_number", json!(pr_number))
                .field("url", json!(url));
            if let Some(report) = cleanup {
                out = out.field("cleanup", json!(report));
            }
            Ok(attach_single_handover_report(
                out,
                ctx,
                report_task_id.as_deref(),
                &cfg.repo,
            ))
        }
        PrMergeOutcome::Blocked {
            code,
            reason,
            receipt,
        } => {
            let text = format!("BLOCKED ({code}): {reason}");
            Ok(CmdOutput::policy("BLOCKED", text)
                .field("code", json!(code))
                .field("reason", json!(reason))
                .field("receipt", json!(receipt)))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::output::Exit;

    fn sample_args(dry_run: bool) -> MergeArgs {
        MergeArgs {
            pr_number: Some(42),
            expect_head: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            task: None,
            mode: ModeArg::Scoped,
            strategy: StrategyArg::Squash,
            remote: "origin".to_string(),
            subject: None,
            body_file: None,
            ci_wait: 0,
            lock_timeout: 60,
            dry_run,
        }
    }

    #[test]
    fn parse_pr_number_extracts_pull_request_id_from_github_urls() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/org/repo/pull/567"),
            Some(567)
        );
        assert_eq!(
            parse_pr_number_from_url("https://github.com/org/repo/pull/42/?w=1#top"),
            Some(42)
        );
        assert_eq!(
            parse_pr_number_from_url("https://github.com/org/repo/issues/12"),
            None
        );
        assert_eq!(parse_pr_number_from_url("not-a-url"), None);
    }

    #[test]
    fn resolves_pr_number_and_head_from_task_events_and_marks_done() {
        use aaf_domain::task::{ImmutableSpec, Priority, Task};
        use aaf_engine::db::Db;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let run_git = |dir: &Path, args: &[&str]| {
            let st = Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@e.invalid"])
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        run_git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "hi\n").unwrap();
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);

        let worktrees = root.join("worktrees");
        let wt = worktree::create(&repo, &worktrees, "t-merge", "main").unwrap();
        fs::write(wt.path.join("a.txt"), "hi2\n").unwrap();
        run_git(&wt.path, &["add", "-A"]);
        run_git(&wt.path, &["commit", "-q", "-m", "feat"]);
        let head = aaf_fp::git(&wt.path, &["rev-parse", "HEAD"], &[], GIT_TIMEOUT).unwrap();

        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let db = Db::open(&data_dir.join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-merge",
            repo.to_string_lossy().into_owned(),
            "Merge task",
            ImmutableSpec {
                description: "desc".into(),
                acceptance: vec![],
            },
            Priority::P2,
            10,
        );
        task.state = TaskState::Review;
        db.insert_task(&task).unwrap();
        db.emit(
            &FactoryEvent::new(
                EventKind::PrOpened,
                json!({ "url": "https://github.com/org/repo/pull/88" }).to_string(),
                11,
            )
            .for_task("t-merge"),
        )
        .unwrap();

        let ctx = Ctx {
            cwd: repo.clone(),
            home: root.join("home"),
            data_dir,
            config_dir: root.join("config"),
            worktrees_dir: worktrees,
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: true,
            aaf_env: Vec::new(),
        };
        let mut args = sample_args(false);
        args.pr_number = None;
        args.expect_head = None;
        args.task = Some("t-merge".to_string());

        let (resolved_root, pr, sha, state) =
            resolve_task_merge_target(&args, &ctx, "t-merge").unwrap();
        assert_eq!(resolved_root, repo);
        assert_eq!(pr, 88);
        assert_eq!(sha, head);
        assert_eq!(state, TaskState::Review);

        assert!(mark_task_done(&ctx, "t-merge", state, pr, &sha).unwrap());
        assert_eq!(db.get_task("t-merge").unwrap().state, TaskState::Done);
    }

    #[test]
    fn refuses_inside_agent_run_even_on_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            cwd: tmp.path().to_path_buf(),
            home: tmp.path().join("home"),
            data_dir: tmp.path().join("data"),
            config_dir: tmp.path().join("config"),
            worktrees_dir: tmp.path().join("worktrees"),
            path_var: std::ffi::OsString::new(),
            run_id: Some("run-42".to_string()),
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        for dry_run in [false, true] {
            let out = run(&sample_args(dry_run), &ctx).unwrap();
            assert_eq!(out.status, "refused", "dry_run={dry_run}");
            assert_eq!(out.exit, Exit::Policy, "dry_run={dry_run}");
            assert!(out.text.contains("aaf merge"), "text={}", out.text);
            assert!(out.text.contains("run-42"), "text={}", out.text);
        }
    }
}
