//! `aaf status`: read-only cost, token, task/run, account and throttle overview.

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::Path;
use std::process::{Command, Stdio};

use aaf_domain::gate::LockStatus;
use aaf_domain::ports::Clock;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::gate::{GIT_TIMEOUT, base, head_lock};
use aaf_engine::telemetry::{
    CostRates, RunCounts, StatusQuery, TaskCounts, TelemetrySummary, collect_status_from_db,
    inspect_throttle,
};
use serde_json::json;

use super::board::store::{err, open_db_read_only, repo_root};
use super::run::detach::{RunMeta, run_lock_path, run_meta_path};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RunnerLiveness {
    alive: bool,
    pid: Option<u32>,
    log_path: Option<String>,
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid_s = pid.to_string();
    for bin in ["/bin/kill", "/usr/bin/kill", "kill"] {
        if let Ok(status) = Command::new(bin)
            .args(["-0", &pid_s])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            return status.success();
        }
    }
    false
}

#[cfg(not(unix))]
fn pid_alive(pid: u32) -> bool {
    pid != 0 && pid == std::process::id()
}

/// Strictly read-only runner liveness check from `<data_dir>/locks/<repo_id>.run.{meta.json,lock}`.
fn inspect_runner(data_dir: &Path, repo: Option<&str>) -> RunnerLiveness {
    let Some(repo_str) = repo else {
        return RunnerLiveness::default();
    };
    let repo_path = Path::new(repo_str);
    let meta_path = run_meta_path(data_dir, repo_path);
    let meta: Option<RunMeta> = fs::read_to_string(&meta_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let lock_path = run_lock_path(data_dir, repo_path);
    let lock_held = if lock_path.is_file() {
        match File::open(&lock_path) {
            Ok(file) => match file.try_lock_shared() {
                Ok(()) => {
                    let _ = file.unlock();
                    Some(false)
                }
                Err(TryLockError::WouldBlock) => Some(true),
                Err(TryLockError::Error(_)) => None,
            },
            Err(_) => None,
        }
    } else {
        None
    };
    let alive = match lock_held {
        Some(held) => held,
        None => meta.as_ref().is_some_and(|m| pid_alive(m.pid)),
    };
    RunnerLiveness {
        alive,
        pid: meta.as_ref().map(|m| m.pid),
        log_path: meta.and_then(|m| m.log_path),
    }
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Summarize across every repository instead of scoping to the current repo.
    #[arg(long, conflicts_with = "approval")]
    pub all: bool,
    /// Inspect read-only head-lock approval state for the current repo/worktree.
    #[arg(long)]
    pub approval: bool,
}

fn run_approval_status(ctx: &Ctx) -> CmdResult {
    let root = ctx.repo_root()?;
    let cfg = ctx.load_config(&root)?;
    let resolved = base::resolve(&root, &cfg.repo.gate.base_ref, GIT_TIMEOUT)
        .map_err(|e| CmdError::new(format!("status --approval: {e}")))?;
    let mut eval_cfg = cfg.repo.clone();
    eval_cfg.cross_audit.require_different_provider = false;
    let mut report = head_lock::evaluate(&ctx.data_dir, &root, &eval_cfg, &resolved.diff_target);
    if report.status == LockStatus::Blocked && report.lock_fp.is_empty() {
        return Err(CmdError::new(
            "status --approval: failed to evaluate head-lock (check git status and gate.head_lock_globs)",
        ));
    }

    if report
        .approval
        .as_ref()
        .is_some_and(|a| !a.passes_independent(&report.lock_fp))
    {
        let mut ind_cfg = cfg.repo.clone();
        ind_cfg.cross_audit.require_different_provider = true;
        let ind_report = head_lock::evaluate(&ctx.data_dir, &root, &ind_cfg, &resolved.diff_target);
        if ind_report.status == LockStatus::Approved {
            report = ind_report;
        }
    }

    let independent_approved = report
        .approval
        .as_ref()
        .is_some_and(|a| a.passes_independent(&report.lock_fp));
    let approved = if cfg.repo.cross_audit.require_different_provider {
        independent_approved
    } else {
        report.status == LockStatus::Approved
    };
    let effective_status = match report.status {
        LockStatus::Clear => LockStatus::Clear,
        LockStatus::Approved if approved => LockStatus::Approved,
        LockStatus::Approved | LockStatus::Blocked => LockStatus::Blocked,
    };
    let status_word = match effective_status {
        LockStatus::Clear => "clear",
        LockStatus::Approved => "approved",
        LockStatus::Blocked => "missing",
    };
    let approval_rec = report.approval.as_ref();
    let reviewer = approval_rec.map(|a| a.reviewer.as_str());
    let same_account = approval_rec.map(|a| a.same_account);
    let locked_files: Vec<String> = report
        .locked_changes
        .iter()
        .map(|c| c.path.clone())
        .collect();

    let mut lines = vec![
        format!("scope: {}", root.display()),
        format!("base: {}", resolved.diff_target),
    ];
    match effective_status {
        LockStatus::Clear => {
            lines.push("approval: clear (no locked files changed)".to_string());
        }
        LockStatus::Approved => {
            lines.push(format!(
                "approval: approved (independent={}, reviewer={}, lock_fp={}, locked_files={})",
                independent_approved,
                reviewer.unwrap_or("unknown"),
                report.lock_fp,
                locked_files.len()
            ));
        }
        LockStatus::Blocked => {
            lines.push(format!(
                "approval: missing (lock_fp={}, locked_files={})",
                report.lock_fp,
                locked_files.len()
            ));
        }
    }
    for file in &locked_files {
        lines.push(format!("  locked: {file}"));
    }

    let approval_json = json!({
        "status": status_word,
        "base": resolved.diff_target,
        "lock_fp": report.lock_fp,
        "locked_files": locked_files,
        "approved": approved,
        "independent_approved": independent_approved,
        "reviewer": reviewer,
        "same_account": same_account,
    });

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("scope", json!(root.to_string_lossy().into_owned()))
        .field("approval", approval_json))
}

pub fn run(args: &StatusArgs, ctx: &Ctx) -> CmdResult {
    if args.approval {
        return run_approval_status(ctx);
    }
    let db = open_db_read_only(ctx)?;
    let repo = if args.all {
        None
    } else {
        repo_root(ctx)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    let scope = if args.all {
        "all".to_string()
    } else if let Some(root) = &repo {
        root.clone()
    } else {
        "all (outside git repo)".to_string()
    };

    let summary = match db {
        Some(db) => collect_status_from_db(
            &db,
            &StatusQuery {
                repo_root: repo.as_deref(),
                data_dir: Some(&ctx.data_dir),
                now: SystemClock.now(),
                rates: CostRates::default(),
            },
        )
        .map_err(|e| CmdError::new(e.to_string()))?,
        None => TelemetrySummary {
            frozen: false,
            consecutive_root_fails: 0,
            repo_root: repo.clone(),
            counts: TaskCounts::default(),
            blocked_reasons: BTreeMap::new(),
            runs: RunCounts::default(),
            tokens_in: 0,
            tokens_out: 0,
            estimated_cost_usd: 0.0,
            accounts: Vec::new(),
            throttle: inspect_throttle(&ctx.data_dir),
            autonomy: Default::default(),
        },
    };

    let blocked_text = if summary.blocked_reasons.is_empty() {
        "none".to_string()
    } else {
        summary
            .blocked_reasons
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let lines = [
        format!("scope: {scope}"),
        format!(
            "tasks: {} total (backlog={}, ready={}, running={}, gating={}, review={}, blocked={}, done={}, cancelled={})",
            summary.counts.total,
            summary.counts.backlog,
            summary.counts.ready,
            summary.counts.running,
            summary.counts.gating,
            summary.counts.review,
            summary.counts.blocked,
            summary.counts.done,
            summary.counts.cancelled
        ),
        format!("blocked_reasons: {blocked_text}"),
        format!(
            "runs: {} total (running={}, succeeded={}, failed={}, timed_out={}, limit_hit={}, killed={})",
            summary.runs.total,
            summary.runs.running,
            summary.runs.succeeded,
            summary.runs.failed,
            summary.runs.timed_out,
            summary.runs.limit_hit,
            summary.runs.killed
        ),
        format!(
            "tokens: {} in + {} out (~${:.4})",
            summary.tokens_in, summary.tokens_out, summary.estimated_cost_usd
        ),
        format!(
            "factory: frozen={}, consecutive_root_fails={}, accounts={}, active_slots={} (lock_files={}), draining={}",
            summary.frozen,
            summary.consecutive_root_fails,
            summary.accounts.len(),
            summary.throttle.throttle_slots,
            summary.throttle.slot_lock_files,
            summary.throttle.drain_lock_present
        ),
    ];

    let throttle_json = json!({
        "throttle_dir_exists": summary.throttle.throttle_dir_exists,
        "drain_lock_present": summary.throttle.drain_lock_present,
        "draining": summary.throttle.drain_lock_present,
        "throttle_slots": summary.throttle.throttle_slots,
        "active_slots": summary.throttle.throttle_slots,
        "slot_lock_files": summary.throttle.slot_lock_files,
        "guest_record_files": summary.throttle.guest_record_files,
    });

    let runner = inspect_runner(&ctx.data_dir, repo.as_deref());
    let runner_json = json!({
        "alive": runner.alive,
        "pid": runner.pid,
        "log_path": runner.log_path,
    });

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("scope", json!(scope))
        .field("frozen", json!(summary.frozen))
        .field(
            "consecutive_root_fails",
            json!(summary.consecutive_root_fails),
        )
        .field("repo_root", json!(summary.repo_root))
        .field(
            "tasks",
            serde_json::to_value(&summary.counts).map_err(|e| err("tasks", e))?,
        )
        .field(
            "blocked_reasons",
            serde_json::to_value(&summary.blocked_reasons)
                .map_err(|e| err("blocked_reasons", e))?,
        )
        .field(
            "runs",
            serde_json::to_value(&summary.runs).map_err(|e| err("runs", e))?,
        )
        .field("tokens_in", json!(summary.tokens_in))
        .field("tokens_out", json!(summary.tokens_out))
        .field("estimated_cost_usd", json!(summary.estimated_cost_usd))
        .field(
            "accounts",
            serde_json::to_value(&summary.accounts).map_err(|e| err("accounts", e))?,
        )
        .field("throttle", throttle_json)
        .field("runner", runner_json))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::StatusArgs;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: StatusArgs,
    }

    #[test]
    fn all_and_approval_flags_conflict() {
        let parsed = Wrapper::try_parse_from(["status", "--all", "--approval"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn approval_status_prefers_common_root_independent_approval() {
        use std::fs;
        use std::path::Path;
        use std::process::{Command, Stdio};

        use aaf_domain::gate::Approval;

        fn git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let base_dir = fs::canonicalize(tmp.path()).unwrap();
        let main_repo = base_dir.join("main");
        let wt_repo = base_dir.join("wt");
        let data_dir = base_dir.join("data");
        fs::create_dir_all(&main_repo).unwrap();
        git(&main_repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(main_repo.join(".autofactory")).unwrap();
        fs::write(
            main_repo.join(".autofactory/config.toml"),
            "[gate]\nbase_ref = \"main\"\nhead_lock_globs = [\"Cargo.toml\"]\n\
             [cross_audit]\nrequire_different_provider = true\n",
        )
        .unwrap();
        fs::write(main_repo.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        git(&main_repo, &["add", "-A"]);
        git(&main_repo, &["commit", "-q", "-m", "init"]);
        git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                wt_repo.to_str().unwrap(),
            ],
        );
        let wt_canon = fs::canonicalize(&wt_repo).unwrap();
        fs::write(wt_canon.join("Cargo.toml"), "[package]\nname = \"y\"\n").unwrap();

        let ctx = super::Ctx {
            cwd: wt_canon.clone(),
            home: base_dir.join("home"),
            data_dir: data_dir.clone(),
            config_dir: base_dir.join("config"),
            worktrees_dir: base_dir.join("worktrees"),
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        let cfg = ctx.load_config(&wt_canon).unwrap();
        let resolved =
            super::base::resolve(&wt_canon, &cfg.repo.gate.base_ref, super::GIT_TIMEOUT).unwrap();
        let eval =
            super::head_lock::evaluate(&data_dir, &wt_canon, &cfg.repo, &resolved.diff_target);
        assert!(!eval.lock_fp.is_empty());

        let key = aaf_fp::load_or_create_key(&data_dir).unwrap();
        let same_acct = Approval {
            lock_fp: eval.lock_fp.clone(),
            reviewer: "same-acct".into(),
            verdict: "PASS".into(),
            same_account: true,
            created_at: 100,
        };
        let independent = Approval {
            lock_fp: eval.lock_fp.clone(),
            reviewer: "ind-reviewer".into(),
            verdict: "PASS".into(),
            same_account: false,
            created_at: 101,
        };
        aaf_fp::write_approval(&data_dir, &key, &wt_canon, &same_acct).unwrap();
        aaf_fp::write_approval(&data_dir, &key, &main_repo, &independent).unwrap();

        let out = super::run(
            &StatusArgs {
                all: false,
                approval: true,
            },
            &ctx,
        )
        .unwrap();
        let approval = &out.fields["approval"];
        assert_eq!(approval["status"], serde_json::json!("approved"));
        assert_eq!(approval["approved"], serde_json::json!(true));
        assert_eq!(approval["independent_approved"], serde_json::json!(true));
        assert_eq!(approval["same_account"], serde_json::json!(false));
        assert_eq!(approval["reviewer"], serde_json::json!("ind-reviewer"));
    }
}
