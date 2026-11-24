//! Throttled heavy script runner and output outcome helpers for `aaf sop`.

use std::path::Path;
use std::time::Duration;

use aaf_domain::sop::SopStage;
use aaf_engine::sop::runner::StageOutcome;
use aaf_engine::throttle::{HeavyJob, Progress, SystemProbe, Throttle};

use super::super::gate::progress_line;
use crate::ctx::{Ctx, SystemClock};
use crate::output::notice;

pub(super) fn script_outcome(
    exit_code: Option<i32>,
    timed_out: bool,
    secs: u64,
    detail: &str,
    artifacts: &[String],
) -> StageOutcome {
    if timed_out {
        return StageOutcome::Failed {
            reason: format!("script timed out after {secs}s: {}", tail_line(detail)),
        };
    }
    if exit_code == Some(0) {
        return StageOutcome::Passed {
            summary: format!("script passed (exit 0): {}", tail_line(detail)),
            artifacts: artifacts.to_vec(),
        };
    }
    let code = exit_code.map_or_else(|| "signal".to_string(), |c| c.to_string());
    StageOutcome::Failed {
        reason: format!("script exit {code}: {}", tail_line(detail)),
    }
}

pub(super) fn run_heavy_script(
    ctx: &Ctx,
    repo_root: &Path,
    stage: &SopStage,
    command: &str,
    secs: u64,
) -> StageOutcome {
    notice(
        "sop",
        &format!("running heavy script stage '{}': {command}", stage.id),
    );
    let cfg = match ctx.load_config(repo_root) {
        Ok(c) => c,
        Err(e) => {
            return StageOutcome::Failed {
                reason: format!("load config: {}", e.message),
            };
        }
    };
    let repo_id = aaf_engine::gate::stats_repo_id(repo_root);
    let label = format!("sop:{}", stage.id);
    let progress = |event: &Progress<'_>| notice("sop", &progress_line(event));
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&cfg.global.machine, &probe, &progress);
    let job = HeavyJob {
        repo_id: &repo_id,
        label: &label,
        command,
        cwd: repo_root,
        timeout: Duration::from_secs(secs),
        log_tail_bytes: 64 * 1024,
    };
    match throttle.run_heavy(&ctx.data_dir, &SystemClock, &job) {
        Ok(run) => {
            let out = String::from_utf8_lossy(&run.output);
            script_outcome(run.exit_code, run.timed_out, secs, &out, &stage.artifacts)
        }
        Err(e) => StageOutcome::Failed {
            reason: format!("heavy script execution error: {e}"),
        },
    }
}

fn tail_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map_or_else(String::new, |line| line.trim().chars().take(160).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::*;
    use std::process::{Command, Stdio};

    fn init_repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let ok = Command::new("git")
            .args(["-C", dir.to_str().unwrap(), "init", "-q", "-b", "main"])
            .stdin(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn sop_resume_and_approve_reject_mismatched_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = init_repo(&tmp.path().join("repo-a"));
        let repo_b = init_repo(&tmp.path().join("repo-b"));
        let data_dir = tmp.path().join("data");
        let spec = resolve_sop(None, "bugfix").unwrap().spec;
        let cp = SopCheckpoint::new("sop-cross-repo".into(), &spec, path_str(&repo_a), 100);
        save_checkpoint(&data_dir, &cp).unwrap();

        let mut ctx = Ctx {
            cwd: repo_b,
            home: tmp.path().join("home"),
            data_dir,
            config_dir: tmp.path().join("config"),
            worktrees_dir: tmp.path().join("worktrees"),
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        let err = cmd_run("bugfix", Some("sop-cross-repo"), false, &ctx).unwrap_err();
        assert!(err.message.contains("belongs to repo"), "{}", err.message);
        let a_err = cmd_approve("sop-cross-repo", "alice", &ctx).unwrap_err();
        assert!(
            a_err.message.contains("belongs to repo"),
            "{}",
            a_err.message
        );
        ctx.cwd = tmp.path().to_path_buf();
        let out_err = cmd_approve("sop-cross-repo", "alice", &ctx).unwrap_err();
        assert!(
            !out_err.message.contains("belongs to repo"),
            "{}",
            out_err.message
        );
    }

    #[test]
    fn sop_send_back_rejects_cross_repo_and_allows_same_or_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = init_repo(&tmp.path().join("repo-a"));
        let repo_b = init_repo(&tmp.path().join("repo-b"));
        let data_dir = tmp.path().join("data");
        let spec = resolve_sop(None, "bugfix").unwrap().spec;
        let target_stage = spec.stages[0].id.clone();
        let cp = SopCheckpoint::new("sop-sendback-repo".into(), &spec, path_str(&repo_a), 100);
        save_checkpoint(&data_dir, &cp).unwrap();

        let mut ctx = Ctx {
            cwd: repo_b,
            home: tmp.path().join("home"),
            data_dir: data_dir.clone(),
            config_dir: tmp.path().join("config"),
            worktrees_dir: tmp.path().join("worktrees"),
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };

        let err = cmd_send_back("sop-sendback-repo", &target_stage, "retry", &ctx).unwrap_err();
        assert!(err.message.contains("belongs to repo"), "{}", err.message);
        assert_eq!(
            load_checkpoint(&data_dir, "sop-sendback-repo")
                .unwrap()
                .rework_count,
            0
        );

        ctx.cwd = repo_a;
        let same = cmd_send_back("sop-sendback-repo", &target_stage, "retry same", &ctx).unwrap();
        assert_eq!(same.exit, crate::output::Exit::Ok);
        assert_eq!(
            load_checkpoint(&data_dir, "sop-sendback-repo")
                .unwrap()
                .rework_count,
            1
        );

        ctx.cwd = tmp.path().to_path_buf();
        let out = cmd_send_back("sop-sendback-repo", &target_stage, "retry outside", &ctx).unwrap();
        assert_eq!(out.exit, crate::output::Exit::Ok);
        assert_eq!(
            load_checkpoint(&data_dir, "sop-sendback-repo")
                .unwrap()
                .rework_count,
            2
        );
    }
}
